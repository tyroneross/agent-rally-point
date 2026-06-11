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
# 5. Step 3: download prebuilt from GitHub Releases
# ---------------------------------------------------------------------------
_try_download() {
  if ! command -v curl >/dev/null 2>&1; then return 1; fi
  if [ -z "$HOST_TRIPLE" ]; then return 1; fi

  # Resolve latest tag via the GitHub Releases API (no auth needed for public repo)
  local api_url="https://api.github.com/repos/tyroneross/agent-rally-point/releases/latest"
  local release_json
  release_json="$(curl -fsSL --max-time 10 "$api_url" 2>/dev/null)" || return 1

  # Extract tag_name (portable; no jq)
  local tag
  tag="$(printf '%s' "$release_json" | grep -o '"tag_name":"[^"]*"' | head -1 | cut -d'"' -f4 || true)"
  [ -z "$tag" ] && return 1

  local asset_name="rally-${HOST_TRIPLE}"
  local dl_url="https://github.com/tyroneross/agent-rally-point/releases/download/${tag}/${asset_name}"
  local tmp_path
  tmp_path="$(mktemp 2>/dev/null || echo "/tmp/rally-dl-$$")"

  # Try plain binary first
  if curl -fsSL --max-time 60 -o "$tmp_path" "$dl_url" 2>/dev/null; then
    chmod +x "$tmp_path" 2>/dev/null || true
    if "$tmp_path" version >/dev/null 2>&1; then
      mkdir -p "$LOCAL_BIN" 2>/dev/null || true
      mv "$tmp_path" "$LOCAL_RALLY" 2>/dev/null || { rm -f "$tmp_path"; return 1; }
      _write_state "downloaded" "ok" "$LOCAL_RALLY" ""
      return 0
    fi
    rm -f "$tmp_path" 2>/dev/null || true
  fi

  # Try .tar.gz asset
  local tgz_url="${dl_url}.tar.gz"
  local tmp_tgz
  tmp_tgz="$(mktemp 2>/dev/null || echo "/tmp/rally-dl-$$.tar.gz")"
  local tmp_extract
  tmp_extract="$(mktemp -d 2>/dev/null || echo "/tmp/rally-extract-$$")"
  if curl -fsSL --max-time 60 -o "$tmp_tgz" "$tgz_url" 2>/dev/null; then
    if tar -xzf "$tmp_tgz" -C "$tmp_extract" 2>/dev/null; then
      local extracted_bin
      extracted_bin="$(find "$tmp_extract" -name "rally" -type f | head -1 || true)"
      if [ -n "$extracted_bin" ] && [ -f "$extracted_bin" ]; then
        chmod +x "$extracted_bin" 2>/dev/null || true
        if "$extracted_bin" version >/dev/null 2>&1; then
          mkdir -p "$LOCAL_BIN" 2>/dev/null || true
          mv "$extracted_bin" "$LOCAL_RALLY" 2>/dev/null || { rm -rf "$tmp_tgz" "$tmp_extract"; return 1; }
          rm -f "$tmp_tgz" 2>/dev/null || true
          rm -rf "$tmp_extract" 2>/dev/null || true
          _write_state "downloaded" "ok" "$LOCAL_RALLY" ""
          return 0
        fi
      fi
    fi
  fi
  rm -f "$tmp_tgz" 2>/dev/null || true
  rm -rf "$tmp_extract" 2>/dev/null || true
  return 1
}

if _try_download; then
  exit 0
fi

# ---------------------------------------------------------------------------
# 6. Step 4: cargo install from source (BACKGROUNDED — never blocks the hook)
# ---------------------------------------------------------------------------
_try_cargo_build() {
  if ! command -v cargo >/dev/null 2>&1; then return 1; fi
  local source_dir="$PLUGIN_ROOT/crates/rally-cli"
  if [ ! -d "$source_dir" ]; then return 1; fi

  local lock_dir="$CACHE_DIR/.build.lock"
  # Acquire lock via mkdir (atomic on POSIX). If it exists but the PID inside
  # is no longer running, we remove the stale lock and proceed.
  if ! mkdir "$lock_dir" 2>/dev/null; then
    local lock_pid_file="$lock_dir/pid"
    local stale=1
    if [ -f "$lock_pid_file" ]; then
      local lock_pid
      lock_pid="$(cat "$lock_pid_file" 2>/dev/null || echo 0)"
      if [ -n "$lock_pid" ] && kill -0 "$lock_pid" 2>/dev/null; then
        stale=0  # build is still running
      fi
    fi
    if [ "$stale" = "1" ]; then
      rm -rf "$lock_dir" 2>/dev/null || true
      mkdir "$lock_dir" 2>/dev/null || return 1
    else
      # Build already in progress from a concurrent session
      return 0
    fi
  fi

  mkdir -p "$LOCAL_BIN" 2>/dev/null || true
  _write_state "source" "building" "" "Building rally from source; binary will be at $LOCAL_RALLY once done."

  # Background the build; write PID into lock so stale detection works.
  # NOTE: `set -e` is inherited by this subshell, so a bare failing
  # `cargo install` would abort it BEFORE we record "unavailable" and release
  # the lock — leaving stale "building" state. Guard the build in an `if` so
  # both the success and failure states are always recorded. cargo is the
  # most-traveled provision path until release binaries exist.
  (
    if cargo install --path "$source_dir" --root "$HOME/.local" --quiet >/dev/null 2>&1 \
       && _binary_works "$LOCAL_RALLY"; then
      _write_state "source" "ok" "$LOCAL_RALLY" ""
    else
      _write_state "source" "unavailable" "" "cargo install from source failed"
    fi
    rm -rf "$lock_dir" 2>/dev/null || true
  ) &

  local bg_pid=$!
  printf '%s\n' "$bg_pid" > "$lock_dir/pid" 2>/dev/null || true
  # Disown so it survives the hook process exiting
  disown "$bg_pid" 2>/dev/null || true
  return 0
}

if _try_cargo_build; then
  exit 0
fi

# ---------------------------------------------------------------------------
# 7. Step 5: nothing worked
# ---------------------------------------------------------------------------
_path_hint=""
if [ -d "$HOME/.local/bin" ]; then
  case ":${PATH}:" in
    *":$HOME/.local/bin:"*) ;;
    *) _path_hint="Add \$HOME/.local/bin to PATH to make rally available after provisioning." ;;
  esac
fi
_write_state "none" "unavailable" "" "$_path_hint"
exit 0
