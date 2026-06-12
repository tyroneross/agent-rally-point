#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# ensure-rally-binary.sh — Auto-provision the rally CLI on first SessionStart.
#
# CHARTER (fail-open, never-block): this script ALWAYS exits 0 and never blocks
# the hook. ALL network + compiler work (download, cargo build) runs in ONE
# backgrounded, locked worker; local liveness probes are time-bounded. Even a
# hung on-disk binary cannot stall the hook. Idempotency + handoff via a state
# file in XDG_CACHE_HOME and a single pid lock.
#
# Usage: ensure-rally-binary.sh [plugin_root]
#   plugin_root resolved: $1 → $CLAUDE_PLUGIN_ROOT → dirname($0)/..
#
# Provision order (first success wins): reachable on PATH/known-loc → shipped
# prebuilt in plugin → download from GitHub Releases (checksum-verified) →
# cargo build (backgrounded) → unavailable.
#
# SECURITY (download path): a downloaded binary is SHA256-verified against the
# release's published <asset>.sha256 BEFORE it is made executable. The download
# path is FAIL-CLOSED: a mismatch OR an unverifiable download (no checksum, no
# sum tool) is rejected and never executed — cargo (verified source) is the
# fallback. Scope: the same-repo .sha256 defends transit/CDN corruption +
# partial downloads. It does NOT defend GitHub-account compromise (an attacker
# who can swap the binary can swap its checksum). The release also publishes a
# sigstore build-provenance attestation (release.yml) which DOES defend
# substitution, but it is verified OUT OF BAND by a human/CI via
# `gh attestation verify rally-<triple> --repo tyroneross/agent-rally-point` —
# NOT client-side here, because a fail-open hook cannot reliably distinguish a
# real tamper from a network/auth error without turning every offline start
# into a hard failure.
#
# State file: ${XDG_CACHE_HOME:-$HOME/.cache}/rally/provision.json
#   {ts, method, result("ok"|"building"|"unavailable"), binary, hint}
# Exit code: 0 always.

set -euo pipefail

# A usable HOME (or XDG_CACHE_HOME) is required for every install target and the
# state/lock dir. Without one there is nothing safe to do — exit 0 (charter)
# before any $HOME expansion can trip `set -u`.
if [ -z "${HOME:-}" ] && [ -z "${XDG_CACHE_HOME:-}" ]; then
  exit 0
fi
: "${HOME:=$XDG_CACHE_HOME}"   # last-resort so $HOME/.local stays well-formed

# ---------------------------------------------------------------------------
# 0. Resolve plugin root
# ---------------------------------------------------------------------------
if [ -n "${1:-}" ]; then
  PLUGIN_ROOT="$1"
elif [ -n "${CLAUDE_PLUGIN_ROOT:-}" ]; then
  PLUGIN_ROOT="$CLAUDE_PLUGIN_ROOT"
else
  _script_dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd -P || echo .)"   # guard: never abort under set -e
  PLUGIN_ROOT="$(dirname "$_script_dir")"
fi

# ---------------------------------------------------------------------------
# 1. Host triple
# ---------------------------------------------------------------------------
_uname_s="$(uname -s 2>/dev/null || echo unknown)"
_uname_m="$(uname -m 2>/dev/null || echo unknown)"
HOST_TRIPLE=""
case "${_uname_s}:${_uname_m}" in
  Darwin:arm64)               HOST_TRIPLE="aarch64-apple-darwin"     ;;
  Darwin:x86_64)              HOST_TRIPLE="x86_64-apple-darwin"      ;;
  Linux:x86_64)               HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64)  HOST_TRIPLE="aarch64-unknown-linux-gnu";;
  *)                          HOST_TRIPLE=""                          ;;
esac

# ---------------------------------------------------------------------------
# 2. Paths + helpers
# ---------------------------------------------------------------------------
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/rally"
STATE_FILE="$CACHE_DIR/provision.json"
LOCK_FILE="$CACHE_DIR/.provision.lock"
LOCAL_BIN="$HOME/.local/bin"
LOCAL_RALLY="$LOCAL_BIN/rally"
GH_REPO="tyroneross/agent-rally-point"

_write_state() {
  local method="$1" result="$2" binary="${3:-}" hint="${4:-}" ts
  ts="$(date +%s 2>/dev/null || echo 0)"
  mkdir -p "$CACHE_DIR" 2>/dev/null || true
  printf '{"ts":%s,"method":"%s","result":"%s","binary":"%s","hint":"%s"}\n' \
    "$ts" "$method" "$result" "$binary" "$hint" > "$STATE_FILE" 2>/dev/null || true
}

# Run a command under a wall-clock cap. macOS ships no timeout(1); fall back to
# gtimeout, then a portable bash background-kill shim. Robust under set -e
# (every failing command is `|| ...`-guarded).
_timed() {
  local secs="$1"; shift
  local rc=0
  if command -v timeout >/dev/null 2>&1; then timeout -k 1 "${secs}s" "$@" || rc=$?; return "$rc"; fi
  if command -v gtimeout >/dev/null 2>&1; then gtimeout -k 1 "${secs}s" "$@" || rc=$?; return "$rc"; fi
  # Process-group-aware perl shim (same pattern as rally-coordination-hook.sh):
  # fork+setsid the child into its own group, alarm, and KILL the WHOLE group on
  # timeout so no grandchild (e.g. a watchdog sleep) leaks. Returns 124 on
  # timeout, the child's code otherwise.
  if command -v perl >/dev/null 2>&1; then
    perl -e '
      use POSIX qw(setsid);
      my $t = shift;
      my $pid = fork();
      die "fork" unless defined $pid;
      if ($pid == 0) { setsid(); exec @ARGV or exit 127; }
      $SIG{ALRM} = sub { kill "-KILL", $pid; waitpid($pid, 0); exit 124; };
      alarm $t;
      waitpid($pid, 0);
      exit($? >> 8);
    ' "$secs" "$@"
    return $?
  fi
  # Last-resort bash shim: run the child in its own session (setsid if present)
  # and kill its whole group, so the watchdog's sleep is never orphaned.
  if command -v setsid >/dev/null 2>&1; then setsid "$@" & else "$@" & fi
  local p=$!
  local waited=0
  while kill -0 "$p" 2>/dev/null; do
    if [ "$waited" -ge "$secs" ]; then
      kill -KILL "-$p" 2>/dev/null || kill -KILL "$p" 2>/dev/null || true
      wait "$p" 2>/dev/null || true
      return 124
    fi
    sleep 1; waited=$((waited + 1))
  done
  wait "$p" 2>/dev/null || rc=$?
  return "$rc"
}

# Liveness probe is TIME-BOUNDED so a hung on-disk binary can never stall the
# hook (the hook's whole point is to not block).
_binary_works() {
  local b="$1"
  [ -x "$b" ] && _timed 3 "$b" version >/dev/null 2>&1
}

_file_age_secs() {
  local f="$1" mt now
  mt="$(stat -f %m "$f" 2>/dev/null || stat -c %Y "$f" 2>/dev/null || echo 0)"
  now="$(date +%s 2>/dev/null || echo 0)"
  echo $(( now - mt ))
}

# True iff the lock exists AND names a live pid (a provision is in progress).
_lock_pid_alive() {
  [ -f "$LOCK_FILE" ] || return 1
  local lp; lp="$(cat "$LOCK_FILE" 2>/dev/null || echo '')"
  [ -n "$lp" ] && kill -0 "$lp" 2>/dev/null
}

# ---------------------------------------------------------------------------
# 3. Fast synchronous exits (cheap; no network, no compiler)
# ---------------------------------------------------------------------------
_check_existing() {
  if command -v rally >/dev/null 2>&1; then
    _write_state "present" "ok" "$(command -v rally)" ""; return 0
  fi
  if _binary_works "$LOCAL_RALLY"; then
    _write_state "present" "ok" "$LOCAL_RALLY" ""; return 0
  fi
  return 1
}

if [ -f "$STATE_FILE" ]; then
  _cached_result="$(grep -o '"result":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
  _cached_bin="$(grep -o '"binary":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
  _cached_ts="$(grep -o '"ts":[0-9]*' "$STATE_FILE" 2>/dev/null | cut -d: -f2 || echo 0)"
  case "${_cached_ts:-}" in ''|*[!0-9]*) _cached_ts=0 ;; esac   # corrupt-state guard
  _now_ts="$(date +%s 2>/dev/null || echo 0)"
  case "${_now_ts:-}" in ''|*[!0-9]*) _now_ts=0 ;; esac
  _age=$(( _now_ts - _cached_ts ))

  if [ "$_cached_result" = "ok" ] && [ "$_age" -lt 86400 ]; then
    if [ -n "$_cached_bin" ] && _binary_works "$_cached_bin"; then exit 0; fi
    if command -v rally >/dev/null 2>&1; then exit 0; fi
    # binary moved — fall through to re-provision
  fi

  # A provision is only "in progress" if its worker is actually alive — tie the
  # building/provisioning short-circuit to lock-pid liveness, not age, so a
  # crashed worker does not wedge provisioning for 30 minutes.
  if { [ "$_cached_result" = "building" ] || [ "$_cached_result" = "provisioning" ]; } && _lock_pid_alive; then
    exit 0
  fi
fi

if _check_existing; then exit 0; fi

# ---------------------------------------------------------------------------
# 4. Shipped prebuilt copy (local, fast — stays synchronous)
# ---------------------------------------------------------------------------
if [ -n "$HOST_TRIPLE" ]; then
  _shipped="$PLUGIN_ROOT/bin/$HOST_TRIPLE/rally"
  if [ -f "$_shipped" ]; then
    mkdir -p "$LOCAL_BIN" 2>/dev/null || true
    if cp "$_shipped" "$LOCAL_RALLY" 2>/dev/null && chmod +x "$LOCAL_RALLY" 2>/dev/null && _binary_works "$LOCAL_RALLY"; then
      _write_state "shipped-binary" "ok" "$LOCAL_RALLY" ""
      exit 0
    fi
    rm -f "$LOCAL_RALLY" 2>/dev/null || true
  fi
fi

# ---------------------------------------------------------------------------
# 5. Heavy provisioning — download (checksum-verified) or cargo, BACKGROUNDED.
# ---------------------------------------------------------------------------

# Tag from the installed plugin version (no API call; pins to the matching
# binary), API fallback only when no manifest version is readable.
_resolve_tag() {
  local manifest="" m ver
  for m in "$PLUGIN_ROOT/.claude-plugin/plugin.json" "$PLUGIN_ROOT/.codex-plugin/plugin.json"; do
    [ -f "$m" ] && { manifest="$m"; break; }
  done
  if [ -n "$manifest" ]; then
    ver="$(grep -o '"version"[[:space:]]*:[[:space:]]*"[^"]*"' "$manifest" 2>/dev/null | head -1 | sed 's/.*"\([^"]*\)"[[:space:]]*$/\1/')"
    [ -n "$ver" ] && { printf 'v%s' "$ver"; return 0; }
  fi
  command -v curl >/dev/null 2>&1 || return 1
  local j
  j="$(curl -fsSL --max-time 8 "https://api.github.com/repos/$GH_REPO/releases/latest" 2>/dev/null || true)"
  printf '%s' "$j" | grep -o '"tag_name":"[^"]*"' | head -1 | cut -d'"' -f4
}

# Verify a downloaded FILE against <asset>.sha256.
#   0 verified · 1 mismatch · 2 unverifiable (no checksum published, or no sum tool)
_verify_sha256() {
  local file="$1" tag="$2" asset="$3"
  command -v curl >/dev/null 2>&1 || return 2
  local sumtool=""
  if command -v shasum >/dev/null 2>&1; then sumtool="shasum -a 256"
  elif command -v sha256sum >/dev/null 2>&1; then sumtool="sha256sum"
  else return 2; fi
  local want
  want="$(curl -fsSL --max-time 8 "https://github.com/$GH_REPO/releases/download/$tag/$asset.sha256" 2>/dev/null | awk '{print $1}' | head -1)"
  [ -z "$want" ] && return 2
  local got
  got="$($sumtool "$file" 2>/dev/null | awk '{print $1}')"
  [ -n "$got" ] && [ "$got" = "$want" ] && return 0
  return 1
}

_do_download() {
  command -v curl >/dev/null 2>&1 || return 1
  [ -n "$HOST_TRIPLE" ] || return 1
  local tag; tag="$(_resolve_tag)"; [ -n "$tag" ] || return 1
  local asset="rally-${HOST_TRIPLE}"
  local url="https://github.com/$GH_REPO/releases/download/${tag}/${asset}"
  local tmp; tmp="$(mktemp 2>/dev/null || echo "/tmp/rally-dl-$$")"
  if ! curl -fsSL --max-time 120 -o "$tmp" "$url" 2>/dev/null; then
    rm -f "$tmp" 2>/dev/null || true; return 1
  fi
  # FAIL-CLOSED: verify BEFORE chmod/exec. Reject mismatch AND unverifiable.
  _verify_sha256 "$tmp" "$tag" "$asset"
  local vrc=$?
  if [ "$vrc" != "0" ]; then
    local why
    if [ "$vrc" = "1" ]; then
      why="sha256 MISMATCH for $asset@$tag — download rejected (possible tamper/corruption)"
      # A mismatch is potential tamper evidence; record it durably so it survives
      # a later cargo-success overwrite of the single-record state file.
      printf '%s sha256-mismatch %s@%s\n' "$(date +%s 2>/dev/null || echo 0)" "$asset" "$tag" \
        >> "$CACHE_DIR/download-rejections.log" 2>/dev/null || true
    else
      why="no verifiable sha256 for $asset@$tag — download rejected (fail-closed)"
    fi
    rm -f "$tmp" 2>/dev/null || true
    _write_state "download-rejected" "unavailable" "" "$why"
    return 1
  fi
  chmod +x "$tmp" 2>/dev/null || true
  if ! _timed 5 "$tmp" version >/dev/null 2>&1; then rm -f "$tmp" 2>/dev/null || true; return 1; fi
  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  mv "$tmp" "$LOCAL_RALLY" 2>/dev/null || { rm -f "$tmp" 2>/dev/null || true; return 1; }
  _write_state "downloaded" "ok" "$LOCAL_RALLY" ""
  return 0
}

_do_cargo() {
  command -v cargo >/dev/null 2>&1 || return 1
  [ -d "$PLUGIN_ROOT/crates/rally-cli" ] || return 1
  if cargo install --path "$PLUGIN_ROOT/crates/rally-cli" --root "$HOME/.local" --quiet >/dev/null 2>&1 \
     && _binary_works "$LOCAL_RALLY"; then
    _write_state "source" "ok" "$LOCAL_RALLY" ""
    return 0
  fi
  return 1
}

# Nothing to do if neither path can possibly work — record synchronously.
if ! command -v curl >/dev/null 2>&1 && ! command -v cargo >/dev/null 2>&1; then
  _path_hint=""
  if [ -d "$HOME/.local/bin" ]; then
    case ":${PATH}:" in
      *":$HOME/.local/bin:"*) ;;
      *) _path_hint="Add \$HOME/.local/bin to PATH to make rally available after provisioning." ;;
    esac
  fi
  _write_state "none" "unavailable" "" "$_path_hint"
  exit 0
fi

# Acquire a single pid lock for the worker. The lock FILE is created atomically
# (noclobber → O_EXCL) and immediately holds a live pid, so there is no pidless
# window a concurrent session could reclaim. A young EMPTY lock (the sub-syscall
# create/write gap, or a crash mid-write) is treated as live (mtime grace) so it
# is never stolen; an old empty or dead-pid lock is reclaimed.
#   returns 0 = acquired (we own it, our pid is in the file), 1 = held/busy
# Atomic create (noclobber → O_EXCL) + verify-after-write: re-read and confirm
# WE own it, so a contender that raced our rm+recreate (deleting our just-created
# live lock) makes us back off rather than double-provision.
_lock_try_create() {
  ( set -C; printf '%s\n' "$$" > "$LOCK_FILE" ) 2>/dev/null || return 1
  [ "$(cat "$LOCK_FILE" 2>/dev/null || echo)" = "$$" ] || return 1
  return 0
}
_acquire_lock() {
  mkdir -p "$CACHE_DIR" 2>/dev/null || true
  _lock_try_create && return 0
  local lp; lp="$(cat "$LOCK_FILE" 2>/dev/null || echo '')"
  if [ -n "$lp" ] && kill -0 "$lp" 2>/dev/null; then return 1; fi
  if [ -z "$lp" ] && [ "$(_file_age_secs "$LOCK_FILE")" -lt 10 ]; then return 1; fi
  rm -f "$LOCK_FILE" 2>/dev/null || true
  _lock_try_create && return 0
  return 1
}

_provision_bg() {
  _acquire_lock || return 0   # another session is provisioning
  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  _write_state "provisioning" "building" "" "Provisioning rally in background; binary will be at $LOCAL_RALLY when ready."
  # Worker: (a) takes ownership of the lock with its OWN pid as its first action
  # so the lock always names a live pid across the handoff; (b) runs with all
  # inherited fds detached (`>/dev/null 2>&1 </dev/null`) so a caller that
  # captures the hook's stdout never blocks on the worker's lifetime.
  (
    printf '%s\n' "${BASHPID:-$$}" > "$LOCK_FILE" 2>/dev/null || true
    if _do_download; then :
    elif _do_cargo; then :
    else
      case "$(grep -o '"method":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)" in
        download-rejected) : ;;
        *) _write_state "none" "unavailable" "" "no verifiable prebuilt download and no cargo build available" ;;
      esac
    fi
    rm -f "$LOCK_FILE" 2>/dev/null || true
  ) >/dev/null 2>&1 </dev/null &
  local bg=$!
  disown "$bg" 2>/dev/null || true
  # Bounded wait until the worker owns the lock (its pid replaced ours) or has
  # finished (lock gone) — so we never return while the lock still names our
  # about-to-exit pid. ~1ms in practice; capped ~1s.
  local i=0 c
  while [ "$i" -lt 100 ]; do
    c="$(cat "$LOCK_FILE" 2>/dev/null || echo MISSING)"
    [ "$c" != "$$" ] && break
    i=$((i + 1)); sleep 0.01 2>/dev/null || true
  done
}

_provision_bg
exit 0
