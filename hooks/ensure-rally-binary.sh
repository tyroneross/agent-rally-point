#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# ensure-rally-binary.sh — provisioning engine for the rally CLI.
#
# NOT A HOOK. Nothing in hooks/ or in any generated host surface may call this
# file. It is invoked by scripts/install-rally.sh, which a human runs on
# purpose, and by nothing else.
#
# CHARTER (ARP-001, fail-closed): trusting or opening a repo must never install
# software. This script therefore refuses to run unless the caller sets
# RALLY_EXPLICIT_INSTALL=1, which only the explicit installer does. A future
# accidental re-wiring into a lifecycle hook fails closed: the guard fires, the
# script exits 3 without touching the network, the compiler, or $HOME.
#
# Usage: RALLY_EXPLICIT_INSTALL=1 ensure-rally-binary.sh [plugin_root]
#   plugin_root resolved: $1 → $CLAUDE_PLUGIN_ROOT → dirname($0)/..
#
# Provision order (first success wins): already reachable and working →
# download from GitHub Releases (SHA256- AND attestation-verified) → cargo
# build from local source → unavailable.
#
# Provisioning runs in the FOREGROUND. A human is waiting on the result, so the
# installer reports what happened instead of detaching a worker. The pid/flock
# pair still guards against two concurrent installs racing on the same target.
#
# SECURITY (download path), both checks mandatory, both BEFORE chmod/exec:
#   1. SHA256 against the release's published <asset>.sha256. Defends transit
#      and CDN corruption and partial downloads. It does NOT defend a
#      compromised GitHub account or release — whoever can swap the binary can
#      swap its checksum, which is the same authority.
#   2. `gh attestation verify` against the sigstore build-provenance attestation
#      the release workflow publishes (.github/workflows/release.yml). This is
#      the independent authority, and it DOES defend substitution. It used to be
#      an out-of-band human step; ARP-001 makes it client-side and mandatory.
# A mismatch, a missing checksum, a missing `gh`, or a failed attestation all
# reject the download. There is no unverified fallback: cargo build from the
# checked-out source is the alternative, and the caller chooses it explicitly.
#
# REMOVED (ARP-001): the shipped-prebuilt path, which copied
# <plugin>/bin/<triple>/rally into $HOME/.local/bin and ran it. A plugin package
# carries no attestation, so that path was unverifiable by construction.
#
# State file: ${XDG_CACHE_HOME:-$HOME/.cache}/rally/provision.json
#   {ts, method, result("ok"|"unavailable"), binary, hint}
# Exit codes: 0 provisioning attempted and recorded · 3 refused (not an
# explicit install).

set -euo pipefail

# ---------------------------------------------------------------------------
# ARP-001 GUARD — first executable statement, before any path, network, or
# state work. Provisioning happens only when a human asked for it.
# ---------------------------------------------------------------------------
if [ "${RALLY_EXPLICIT_INSTALL:-}" != "1" ]; then
  printf '%s\n' \
    "ensure-rally-binary: refusing to provision. This script installs software and" \
    "is not reachable from a lifecycle hook by design (ARP-001). Run the explicit" \
    "installer instead: scripts/install-rally.sh" >&2
  exit 3
fi

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
      # Report signal-death as failure (128+N), not the misleading exit 0 that
      # ($? >> 8) yields when the low byte holds a signal — so a binary that
      # SIGSEGVs during `version` is rejected, not stamped healthy.
      exit( ($? & 127) ? (128 + ($? & 127)) : ($? >> 8) );
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

_file_mtime_epoch() {
  local f="$1" value=""
  if value="$(stat -f %m "$f" 2>/dev/null)"; then
    case "$value" in ''|*[!0-9]*) value="" ;; esac
  else
    value=""
  fi
  if [ -z "$value" ]; then
    if value="$(stat -c %Y "$f" 2>/dev/null)"; then
      case "$value" in ''|*[!0-9]*) value="" ;; esac
    else
      value=""
    fi
  fi
  printf '%s\n' "${value:-0}"
}

_file_age_secs() {
  local f="$1" mt now
  mt="$(_file_mtime_epoch "$f")"
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
  # `rally` on PATH counts as present ONLY if it actually runs — a binary that
  # crashes (signal) or hangs on `version` must NOT be stamped healthy.
  local p
  if p="$(command -v rally 2>/dev/null)" && _binary_works "$p"; then
    _write_state "present" "ok" "$p" ""; return 0
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
    # A rally on PATH satisfies the cached-ok fast-exit only if it still runs.
    _path_rally="$(command -v rally 2>/dev/null || true)"
    if [ -n "$_path_rally" ] && _binary_works "$_path_rally"; then exit 0; fi
    # binary moved/broke — fall through to re-provision
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
# 4. Provisioning — download (SHA256 + attestation verified) or cargo.
#
# The shipped-prebuilt path that used to live here is gone (ARP-001): copying
# and running <plugin>/bin/<triple>/rally trusted whatever a plugin package
# happened to contain, with no checksum and no attestation to check it against.
# ---------------------------------------------------------------------------

# Pin the binary to the generated plugin release identity. Git-source plugin
# manifests intentionally omit `version`, so reading them made this branch dead
# and silently fell through to GitHub "latest". Old plugin generations retain
# the manifest fallback; the API is last-resort compatibility only.
_resolve_tag() {
  local identity="$PLUGIN_ROOT/rally-release.json" manifest="" m ver
  if [ -f "$identity" ]; then
    ver="$(grep -o '"version"[[:space:]]*:[[:space:]]*"[^"]*"' "$identity" 2>/dev/null | head -1 | sed 's/.*"\([^"]*\)"[[:space:]]*$/\1/')"
    [ -n "$ver" ] && { printf 'v%s' "$ver"; return 0; }
  fi
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

# Verify a downloaded FILE against the release's sigstore build-provenance
# attestation. This is the INDEPENDENT authority: unlike the .sha256, it is not
# something a compromised release can rewrite, because the signature chains to
# the workflow identity recorded in the public transparency log.
#   0 verified · 1 attestation rejected the file · 2 cannot verify (no `gh`)
# There is no "assume fine" branch. Callers treat 1 and 2 the same way.
_verify_attestation() {
  local file="$1"
  command -v gh >/dev/null 2>&1 || return 2
  _timed 60 gh attestation verify "$file" --repo "$GH_REPO" >/dev/null 2>&1 || return 1
  return 0
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
  # Second gate, also BEFORE chmod/exec: client-side provenance (ARP-001).
  # The checksum and the binary come from the same GitHub authority, so the
  # checksum alone cannot catch a compromised release. This can.
  _verify_attestation "$tmp"
  local arc=$?
  if [ "$arc" != "0" ]; then
    local awhy
    if [ "$arc" = "1" ]; then
      awhy="build-provenance attestation FAILED for $asset@$tag — download rejected (possible substitution). Verify by hand: gh attestation verify <file> --repo $GH_REPO"
      printf '%s attestation-failed %s@%s\n' "$(date +%s 2>/dev/null || echo 0)" "$asset" "$tag" \
        >> "$CACHE_DIR/download-rejections.log" 2>/dev/null || true
    else
      awhy="cannot verify build provenance: the GitHub CLI (gh) is not installed, so the attestation for $asset@$tag cannot be checked. Download rejected (fail-closed). Install gh and re-run, or build from source with cargo install --path crates/rally-cli."
    fi
    rm -f "$tmp" 2>/dev/null || true
    _write_state "download-rejected" "unavailable" "" "$awhy"
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

# Preferred lock: a held flock gives TRUE mutual exclusion with zero TOCTOU (it
# closes the portable path's narrow reclaim race entirely). We hold the locked
# fd 9 for the duration of the provision — auto-released by the OS on exit OR
# crash, so it can never wedge. flock(1) is Linux-standard and absent on stock
# macOS; there `_flock_acquire` returns 2 and `_provision` uses the portable
# path. A FIXED fd (9) keeps this parseable on bash 3.2 (which lacks
# `{var}`-fd syntax); the whole branch parses-but-never-runs where flock is gone.
#   returns: 0 = acquired (we hold fd 9), 1 = busy, 2 = unavailable
_flock_acquire() {
  command -v flock >/dev/null 2>&1 || return 2
  mkdir -p "$CACHE_DIR" 2>/dev/null || true
  exec 9>"${LOCK_FILE}.flock" 2>/dev/null || return 2
  if flock -n 9 2>/dev/null; then return 0; fi
  exec 9>&- 2>/dev/null || true
  return 1
}
_flock_release() { exec 9>&- 2>/dev/null || true; }

# FOREGROUND (ARP-001). The old version detached a background worker because a
# lifecycle hook must never block. No hook calls this any more: a human ran the
# installer and is waiting on the answer, so the work happens inline and the
# caller reads the outcome straight out of the state file.
_provision() {
  mkdir -p "$CACHE_DIR" 2>/dev/null || true
  local via_flock=0 frc=0
  _flock_acquire || frc=$?                        # capture rc without tripping set -e
  if [ "$frc" = 1 ]; then return 0; fi           # another install holds the flock
  if [ "$frc" = 0 ]; then
    # Also acquire the portable pid lock while holding flock. A peer may have
    # started without flock on PATH; ignoring its live/young pid lock would let
    # the two locking strategies provision concurrently.
    if ! _acquire_lock; then
      _flock_release
      return 0
    fi
    via_flock=1
  else
    _acquire_lock || return 0                     # no flock → portable lock (another install has it)
  fi
  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  if _do_download; then :
  elif _do_cargo; then :
  else
    case "$(grep -o '"method":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)" in
      download-rejected) : ;;
      *) _write_state "none" "unavailable" "" "no verifiable release download and no cargo build available" ;;
    esac
  fi
  rm -f "$LOCK_FILE" 2>/dev/null || true
  [ "$via_flock" = 1 ] && _flock_release
  return 0
}

_provision
exit 0
