#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# ensure-rally-binary.sh — Auto-provision the rally CLI on first SessionStart.
#
# CHARTER (fail-open, never-block): this script ALWAYS exits 0. Slow work
# (cargo build) is backgrounded with a lockfile so concurrent sessions do not
# double-build. Idempotency via a state file in XDG_CACHE_HOME.
#
# Usage:
#   ensure-rally-binary.sh [plugin_root]
#
#   plugin_root — optional. Resolved in order:
#     1. $1 (arg)
#     2. $CLAUDE_PLUGIN_ROOT
#     3. dirname($0)/..   (script lives at plugin_root/hooks/ensure-rally-binary.sh)
#
# Resolution order (stops at first success):
#   1. rally already reachable (command -v / known paths)        → "present"
#   2. Prebuilt binary shipped inside plugin at bin/<triple>/rally → "shipped-binary"
#   3. Download prebuilt from GitHub Releases                     → "downloaded"
#   4. cargo install from source (backgrounded)                   → "building"
#   5. Nothing worked                                             → "unavailable"
#
# State file: ${XDG_CACHE_HOME:-$HOME/.cache}/rally/provision.json
#   Fields: ts (epoch), method, result ("ok"|"building"|"unavailable"), binary, hint
#
# Exit code: 0 always.

set -euo pipefail

# ---------------------------------------------------------------------------
# 0. Resolve plugin root
# ---------------------------------------------------------------------------
if [ -n "${1:-}" ]; then
  PLUGIN_ROOT="$1"
elif [ -n "${CLAUDE_PLUGIN_ROOT:-}" ]; then
  PLUGIN_ROOT="$CLAUDE_PLUGIN_ROOT"
else
  # Script lives at $plugin_root/hooks/ensure-rally-binary.sh
  _script_dir="$(cd "$(dirname "$0")" && pwd -P)"
  PLUGIN_ROOT="$(dirname "$_script_dir")"
fi

# ---------------------------------------------------------------------------
# 1. Detect host triple
# ---------------------------------------------------------------------------
_uname_s="$(uname -s 2>/dev/null || echo unknown)"
_uname_m="$(uname -m 2>/dev/null || echo unknown)"

HOST_TRIPLE=""
case "${_uname_s}:${_uname_m}" in
  Darwin:arm64)                  HOST_TRIPLE="aarch64-apple-darwin"    ;;
  Darwin:x86_64)                 HOST_TRIPLE="x86_64-apple-darwin"     ;;
  Linux:x86_64)                  HOST_TRIPLE="x86_64-unknown-linux-gnu";;
  Linux:aarch64|Linux:arm64)     HOST_TRIPLE="aarch64-unknown-linux-gnu";;
  *)                             HOST_TRIPLE=""                         ;;
esac

# ---------------------------------------------------------------------------
# 2. Cache / state file helpers
# ---------------------------------------------------------------------------
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/rally"
STATE_FILE="$CACHE_DIR/provision.json"
LOCAL_BIN="$HOME/.local/bin"
LOCAL_RALLY="$LOCAL_BIN/rally"

# Write provision.json (best-effort; never fatal)
_write_state() {
  local method="$1" result="$2" binary="${3:-}" hint="${4:-}"
  local ts
  ts="$(date +%s 2>/dev/null || echo 0)"
  mkdir -p "$CACHE_DIR" 2>/dev/null || true
  printf '{"ts":%s,"method":"%s","result":"%s","binary":"%s","hint":"%s"}\n' \
    "$ts" "$method" "$result" "$binary" "$hint" > "$STATE_FILE" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 3. Fast-exit if a recent successful provision is cached or binary is live
# ---------------------------------------------------------------------------
_binary_works() {
  local b="$1"
  [ -x "$b" ] && "$b" version >/dev/null 2>&1
}

_check_existing() {
  # Already on PATH?
  if command -v rally >/dev/null 2>&1; then
    local found
    found="$(command -v rally)"
    _write_state "present" "ok" "$found" ""
    return 0
  fi
  # Well-known install location
  if _binary_works "$LOCAL_RALLY"; then
    _write_state "present" "ok" "$LOCAL_RALLY" ""
    return 0
  fi
  # Shipped prebuilt in plugin (for present-detection before copying)
  if [ -n "$HOST_TRIPLE" ] && _binary_works "$PLUGIN_ROOT/bin/$HOST_TRIPLE/rally"; then
    # Not yet installed, do not record "present" — fall through to step 2
    return 1
  fi
  return 1
}

# Check cached state first (avoids re-probing every session)
if [ -f "$STATE_FILE" ]; then
  # Read result and ts from cached state (portable; no jq required)
  _cached_result="$(grep -o '"result":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
  _cached_bin="$(grep -o '"binary":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
  _cached_ts="$(grep -o '"ts":[0-9]*' "$STATE_FILE" 2>/dev/null | cut -d: -f2 || echo 0)"
  _now_ts="$(date +%s 2>/dev/null || echo 0)"
  _age=$(( _now_ts - _cached_ts ))

  if [ "$_cached_result" = "ok" ] && [ "$_age" -lt 86400 ]; then
    # Validate the recorded binary is still there
    if [ -n "$_cached_bin" ] && _binary_works "$_cached_bin"; then
      exit 0
    fi
    if command -v rally >/dev/null 2>&1; then
      exit 0
    fi
    # Binary moved — fall through to re-provision
  fi

  if [ "$_cached_result" = "building" ] && [ "$_age" -lt 1800 ]; then
    # A build is in progress (backgrounded in a previous session within 30 min).
    exit 0
  fi
fi

# Quick PATH / known-location check before heavier work
if _check_existing; then
  exit 0
fi

# ---------------------------------------------------------------------------
# 4. Step 2: copy shipped prebuilt from plugin bin/<triple>/rally
# ---------------------------------------------------------------------------
if [ -n "$HOST_TRIPLE" ]; then
  _shipped="$PLUGIN_ROOT/bin/$HOST_TRIPLE/rally"
  if [ -f "$_shipped" ]; then
    mkdir -p "$LOCAL_BIN" 2>/dev/null || true
    if cp "$_shipped" "$LOCAL_RALLY" 2>/dev/null && chmod +x "$LOCAL_RALLY" 2>/dev/null; then
      if _binary_works "$LOCAL_RALLY"; then
        _write_state "shipped-binary" "ok" "$LOCAL_RALLY" ""
        exit 0
      fi
    fi
    # Copy succeeded but binary didn't verify — clean up and fall through
    rm -f "$LOCAL_RALLY" 2>/dev/null || true
  fi
fi

# ---------------------------------------------------------------------------
# 5. Heavy provisioning (download or cargo build) — BACKGROUNDED under one lock.
#
# The fast synchronous checks above already handled "binary present" and the
# local shipped-prebuilt copy. If we reach here we genuinely need the network or
# a compiler, so it MUST run off the hook's critical path: a SessionStart hook
# has a small budget (Cursor's is ~5s) and a cargo build is minutes — running
# either synchronously gets the hook killed every session and never caches the
# result. We kick the work into the background and let the binary be ready for
# the next session.
# ---------------------------------------------------------------------------
GH_REPO="tyroneross/agent-rally-point"

# Resolve the release tag from the installed plugin version (no API call; this
# also pins the download to the binary that MATCHES the installed plugin),
# falling back to releases/latest only when no manifest version is readable.
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

# f2 — integrity: verify a downloaded file against the release's published
# <asset>.sha256 BEFORE making it executable/installing.
#   return 0 = verified · 1 = MISMATCH (reject, never exec) · 2 = no checksum
#   published for this release (caller falls back to the execute-check, the
#   prior behavior). NOTE: a same-repo .sha256 defends transit/CDN corruption
#   and partial downloads; it does NOT defend GitHub-account compromise (an
#   attacker who can swap the binary can swap the checksum). Build-provenance
#   attestation (release.yml `attest-build-provenance`, verifiable out-of-band
#   via `gh attestation verify`) is the anti-tamper layer for that threat.
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

# Install a downloaded candidate: integrity-verify (f2) BEFORE exec, then a
# liveness check, then atomically place it.
_install_candidate() {
  local cand="$1" tag="$2" asset="$3"
  _verify_sha256 "$cand" "$tag" "$asset"
  local vrc=$?
  [ "$vrc" = "1" ] && return 1   # checksum MISMATCH → reject, do not chmod/exec
  chmod +x "$cand" 2>/dev/null || true
  "$cand" version >/dev/null 2>&1 || return 1
  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  mv "$cand" "$LOCAL_RALLY" 2>/dev/null || return 1
  if [ "$vrc" = "2" ]; then
    _write_state "downloaded-unverified" "ok" "$LOCAL_RALLY" "no SHA256 published for $tag; relied on execute-check"
  else
    _write_state "downloaded" "ok" "$LOCAL_RALLY" ""
  fi
  return 0
}

_do_download() {
  command -v curl >/dev/null 2>&1 || return 1
  [ -n "$HOST_TRIPLE" ] || return 1
  local tag; tag="$(_resolve_tag)"; [ -n "$tag" ] || return 1
  local asset="rally-${HOST_TRIPLE}"
  local base="https://github.com/$GH_REPO/releases/download/${tag}/${asset}"
  local tmp; tmp="$(mktemp 2>/dev/null || echo "/tmp/rally-dl-$$")"
  if curl -fsSL --max-time 120 -o "$tmp" "$base" 2>/dev/null; then
    _install_candidate "$tmp" "$tag" "$asset" && return 0
  fi
  rm -f "$tmp" 2>/dev/null || true
  # .tar.gz fallback (checksum keyed to the tarball asset)
  local tgz ex b
  tgz="$(mktemp 2>/dev/null || echo "/tmp/rally-tgz-$$")"
  ex="$(mktemp -d 2>/dev/null || echo "/tmp/rally-ex-$$")"
  if curl -fsSL --max-time 120 -o "$tgz" "${base}.tar.gz" 2>/dev/null && tar -xzf "$tgz" -C "$ex" 2>/dev/null; then
    b="$(find "$ex" -name rally -type f 2>/dev/null | head -1)"
    if [ -n "$b" ] && _install_candidate "$b" "$tag" "${asset}.tar.gz"; then
      rm -f "$tgz" 2>/dev/null || true; rm -rf "$ex" 2>/dev/null || true; return 0
    fi
  fi
  rm -f "$tgz" 2>/dev/null || true; rm -rf "$ex" 2>/dev/null || true
  return 1
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

# Nothing to do if neither path can work — record unavailable synchronously
# (cheap) rather than backgrounding a guaranteed no-op.
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

# f3 + f4 — one lock for all heavy provisioning; the live pid is written into
# the lock BEFORE the background worker is spawned, so there is never a window
# where the lock dir exists without a live pid. That window is exactly what let
# a concurrent session misclassify a fresh live lock as stale and delete it
# (double-build) — the marketed parallel-agents scenario.
_provision_bg() {
  mkdir -p "$CACHE_DIR" 2>/dev/null || true   # the lock mkdir below needs the parent dir
  local lock="$CACHE_DIR/.provision.lock"
  if ! mkdir "$lock" 2>/dev/null; then
    local lp
    lp="$(cat "$lock/pid" 2>/dev/null || echo '')"
    if [ -n "$lp" ] && kill -0 "$lp" 2>/dev/null; then
      return 0   # provisioning already in progress in another session
    fi
    rm -rf "$lock" 2>/dev/null || true
    mkdir "$lock" 2>/dev/null || return 0
  fi
  printf '%s' "$$" > "$lock/pid" 2>/dev/null || true   # placeholder live pid (f4)
  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  _write_state "provisioning" "building" "" "Provisioning rally in background; binary will be at $LOCAL_RALLY when ready."
  (
    if _do_download || _do_cargo; then :; else
      _write_state "none" "unavailable" "" "no verifiable prebuilt download and no cargo build available"
    fi
    rm -rf "$lock" 2>/dev/null || true
  ) &
  local bg=$!
  printf '%s' "$bg" > "$lock/pid" 2>/dev/null || true  # real worker pid (f4)
  disown "$bg" 2>/dev/null || true
}

_provision_bg
exit 0
