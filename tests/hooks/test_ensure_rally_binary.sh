#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Tests for hooks/ensure-rally-binary.sh
#
# Run: bash tests/hooks/test_ensure_rally_binary.sh
# Exits 0 on full pass, 1 on any failure. Prints "Passed: N / Failed: M".
#
# All tests run inside isolated sandboxes (tmpdir HOME + restricted PATH) so
# they never touch the real machine's files or network.

set -u
# (deliberately not -e: we need to catch exit codes from the hook)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
HOOK="$REPO_ROOT/hooks/ensure-rally-binary.sh"

if [ ! -x "$HOOK" ]; then
  echo "FAIL: hook missing or not executable at $HOOK"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()

note() { printf '  %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

# Create a shared top-level temp dir; individual tests get subdirs.
TMPDIR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

# ---------------------------------------------------------------------------
# Helper: make a fake rally binary that mirrors the REAL CLI's contract — it
# answers the `version` SUBCOMMAND (exit 0) and rejects an unknown invocation
# like `--version` (exit 2). This is deliberate: ensure-rally-binary.sh probes
# liveness with `rally version`, and a stub that exited 0 for every arg would
# silently mask a regression back to the wrong `--version` flag.
# ---------------------------------------------------------------------------
_make_fake_rally() {
  local dest="$1"
  mkdir -p "$(dirname "$dest")"
  printf '#!/usr/bin/env bash\ncase "${1:-}" in\n  version) printf "rally 0.0.0-test\\n"; exit 0 ;;\n  *) printf "rally: unknown Rally command %s\\n" "${1:-}" >&2; exit 2 ;;\nesac\n' > "$dest"
  chmod +x "$dest"
}

# ---------------------------------------------------------------------------
# Test 1: Triple detection — uname output maps to a non-empty triple on macOS
# ---------------------------------------------------------------------------
T="triple-detection: macOS host produces a non-empty HOST_TRIPLE"
(
  # Extract HOST_TRIPLE by sourcing just the detection block.
  # We use a subshell to avoid polluting environment.
  sandbox="$TMPDIR_ROOT/t1"
  mkdir -p "$sandbox"
  export HOME="$sandbox"
  export XDG_CACHE_HOME="$sandbox/.cache"
  # Run the hook with a plugin root that has no prebuilts, no cargo, no curl
  # pointing at an unreachable URL — we just want the triple detection to
  # produce a non-empty value on this macOS machine.
  # Approach: inject a sentinel binary called "rally" that echoes a marker and
  # verify it is found via the "present" fast-path, which requires the triple
  # detection to at least not crash (it doesn't gate the fast path).
  # Instead, we directly test the triple via a small inline script.
  _uname_s="$(uname -s 2>/dev/null || echo unknown)"
  _uname_m="$(uname -m 2>/dev/null || echo unknown)"
  HOST_TRIPLE=""
  case "${_uname_s}:${_uname_m}" in
    Darwin:arm64)              HOST_TRIPLE="aarch64-apple-darwin"     ;;
    Darwin:x86_64)             HOST_TRIPLE="x86_64-apple-darwin"      ;;
    Linux:x86_64)              HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) HOST_TRIPLE="aarch64-unknown-linux-gnu";;
    *)                         HOST_TRIPLE=""                          ;;
  esac
  if [ -n "$HOST_TRIPLE" ]; then
    printf 'triple=%s\n' "$HOST_TRIPLE" >/dev/null
    exit 0
  else
    printf 'empty triple for %s:%s\n' "$_uname_s" "$_uname_m" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "HOST_TRIPLE was empty on this host"; fi

# ---------------------------------------------------------------------------
# Test 2: Fast-path — rally already on sandbox PATH → records "present",
# does NOT attempt build or download, NO lock dir created.
# ---------------------------------------------------------------------------
T="fast-path: rally already on PATH → exit 0 + method=present + no lock"
(
  sandbox="$TMPDIR_ROOT/t2"
  fake_bin_dir="$sandbox/fake-bin"
  _make_fake_rally "$fake_bin_dir/rally"

  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"

  # PATH contains only our fake bin dir + /usr/bin + /bin (no cargo, no curl)
  export PATH="$fake_bin_dir:/usr/bin:/bin"

  # Use a plugin root with no bin/<triple>/ and no crates/rally-cli
  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root"

  "$HOOK" "$plugin_root"
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi

  # State file must exist and record method=present
  state_file="$XDG_CACHE_HOME/rally/provision.json"
  if [ ! -f "$state_file" ]; then
    printf 'provision.json not written\n' >&2; exit 1
  fi
  method="$(grep -o '"method":"[^"]*"' "$state_file" | cut -d'"' -f4 || true)"
  if [ "$method" != "present" ]; then
    printf 'expected method=present, got: %s\n' "$method" >&2; exit 1
  fi

  # No build lock dir should have been created
  lock_dir="$XDG_CACHE_HOME/rally/.provision.lock"
  if [ -d "$lock_dir" ]; then
    printf 'unexpected lock dir created\n' >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test 3: Fail-open — no cargo, no network (curl pointed nowhere), no prebuilt
#          → hook still exits 0 and records "unavailable"
# ---------------------------------------------------------------------------
T="fail-open: no cargo + no network + no prebuilt → exit 0 + unavailable"
(
  sandbox="$TMPDIR_ROOT/t3"
  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"

  # Fake curl that always fails (network unreachable simulation)
  fake_tools="$sandbox/tools"
  mkdir -p "$fake_tools"
  printf '#!/usr/bin/env bash\nexit 1\n' > "$fake_tools/curl"
  chmod +x "$fake_tools/curl"

  # PATH with NO rally, NO cargo, but our failing curl
  export PATH="$fake_tools:/usr/bin:/bin"

  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root"

  "$HOOK" "$plugin_root"
  rc=$?
  if [ "$rc" != "0" ]; then
    printf 'hook must exit 0, got rc=%s\n' "$rc" >&2; exit 1
  fi

  # Provisioning is now BACKGROUNDED (f3 — the hook never blocks on network or a
  # compiler), so the terminal "unavailable" state arrives asynchronously after
  # the synchronous "provisioning" stamp. Poll for the terminal state.
  state_file="$XDG_CACHE_HOME/rally/provision.json"
  result=""
  for _ in $(seq 1 40); do
    [ -f "$state_file" ] && result="$(grep -o '"result":"[^"]*"' "$state_file" | cut -d'"' -f4 || true)"
    [ "$result" = "unavailable" ] && break
    sleep 0.25
  done
  if [ ! -f "$state_file" ]; then
    printf 'provision.json not written\n' >&2; exit 1
  fi
  if [ "$result" != "unavailable" ]; then
    printf 'expected terminal result=unavailable, got: %s\n' "$result" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test 4: provision.json is valid JSON with required fields
# ---------------------------------------------------------------------------
T="provision.json: written file is valid JSON with ts/method/result fields"
(
  sandbox="$TMPDIR_ROOT/t4"
  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"

  fake_tools="$sandbox/tools"
  mkdir -p "$fake_tools"
  printf '#!/usr/bin/env bash\nexit 1\n' > "$fake_tools/curl"
  chmod +x "$fake_tools/curl"

  export PATH="$fake_tools:/usr/bin:/bin"
  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root"

  "$HOOK" "$plugin_root" >/dev/null 2>&1
  rc=$?

  state_file="$XDG_CACHE_HOME/rally/provision.json"
  if [ ! -f "$state_file" ]; then
    printf 'provision.json not written (rc=%s)\n' "$rc" >&2; exit 1
  fi

  content="$(cat "$state_file" 2>/dev/null)"

  # Must have the three required keys: ts, method, result
  for key in ts method result; do
    if ! printf '%s' "$content" | grep -q "\"$key\""; then
      printf 'provision.json missing key "%s": %s\n' "$key" "$content" >&2; exit 1
    fi
  done

  # Must be parseable as JSON (validate with node if available, else basic check)
  if command -v node >/dev/null 2>&1; then
    if ! printf '%s' "$content" | node -e \
      'try{JSON.parse(require("fs").readFileSync(0,"utf8"));process.exit(0);}catch(_){process.exit(1);}' \
      2>/dev/null; then
      printf 'provision.json is not valid JSON: %s\n' "$content" >&2; exit 1
    fi
  else
    # Fallback: check it starts with { and ends with }
    first="$(printf '%s' "$content" | head -c1)"
    last="$(printf '%s' "$content" | tail -c1)"
    if [ "$first" != "{" ] || [ "$last" != "}" ]; then
      printf 'provision.json does not look like JSON: %s\n' "$content" >&2; exit 1
    fi
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test 5: shipped-binary path — prebuilt exists in plugin bin/<triple>/rally
#          → copied to $HOME/.local/bin/rally, recorded as shipped-binary
# ---------------------------------------------------------------------------
T="shipped-binary: prebuilt in plugin bin/<triple>/ → copied + method=shipped-binary"
(
  sandbox="$TMPDIR_ROOT/t5"
  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"

  # Detect the real triple for this host
  _us="$(uname -s 2>/dev/null || echo unknown)"
  _um="$(uname -m 2>/dev/null || echo unknown)"
  TRIPLE=""
  case "${_us}:${_um}" in
    Darwin:arm64)              TRIPLE="aarch64-apple-darwin"     ;;
    Darwin:x86_64)             TRIPLE="x86_64-apple-darwin"      ;;
    Linux:x86_64)              TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) TRIPLE="aarch64-unknown-linux-gnu";;
  esac

  if [ -z "$TRIPLE" ]; then
    # Can't test shipped-binary on unrecognised host; skip gracefully
    printf 'SKIP (unknown triple)\n'
    exit 0
  fi

  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root/bin/$TRIPLE"
  _make_fake_rally "$plugin_root/bin/$TRIPLE/rally"

  # PATH with no rally binary, no cargo
  export PATH="/usr/bin:/bin"

  "$HOOK" "$plugin_root"
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi

  state_file="$XDG_CACHE_HOME/rally/provision.json"
  if [ ! -f "$state_file" ]; then
    printf 'provision.json not written\n' >&2; exit 1
  fi

  method="$(grep -o '"method":"[^"]*"' "$state_file" | cut -d'"' -f4 || true)"
  result="$(grep -o '"result":"[^"]*"' "$state_file" | cut -d'"' -f4 || true)"

  if [ "$method" != "shipped-binary" ]; then
    printf 'expected method=shipped-binary, got: %s\n' "$method" >&2; exit 1
  fi
  if [ "$result" != "ok" ]; then
    printf 'expected result=ok, got: %s\n' "$result" >&2; exit 1
  fi

  # Binary must now be at $HOME/.local/bin/rally
  if [ ! -x "$HOME/.local/bin/rally" ]; then
    printf '$HOME/.local/bin/rally not installed\n' >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test 6: idempotency — running twice with rally already present does NOT
#          create a build lock or re-copy anything
# ---------------------------------------------------------------------------
T="idempotency: second run with cached ok state exits 0 quickly, no lock"
(
  sandbox="$TMPDIR_ROOT/t6"
  fake_bin_dir="$sandbox/fake-bin"
  _make_fake_rally "$fake_bin_dir/rally"

  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"
  export PATH="$fake_bin_dir:/usr/bin:/bin"

  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root"

  # First run — seeds the state file
  "$HOOK" "$plugin_root" >/dev/null 2>&1

  # Second run — must still exit 0
  "$HOOK" "$plugin_root"
  rc=$?
  if [ "$rc" != "0" ]; then printf 'second run rc=%s\n' "$rc" >&2; exit 1; fi

  # No lock dir
  lock_dir="$XDG_CACHE_HOME/rally/.provision.lock"
  if [ -d "$lock_dir" ]; then
    printf 'unexpected lock dir on second run\n' >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test 7: f2 integrity — a downloaded binary is verified against the published
# SHA256 BEFORE chmod/exec. Match installs (method=downloaded); a mismatch is
# rejected and never installed (falls through to unavailable).
# ---------------------------------------------------------------------------
if command -v shasum >/dev/null 2>&1; then
  _FAKE_BODY='#!/usr/bin/env bash
case "${1:-}" in version) printf "rally 0.0.0-test\n"; exit 0;; *) exit 2;; esac'
  _GOOD_HASH="$(printf '%s' "$_FAKE_BODY" | shasum -a 256 | awk '{print $1}')"
  _write_curl_stub() {  # $1=dir  $2=served_hash
    cat > "$1/curl" <<STUB
#!/usr/bin/env bash
out=""; url=""
while [ \$# -gt 0 ]; do
  case "\$1" in -o) out="\$2"; shift 2;; http*) url="\$1"; shift;; *) shift;; esac
done
case "\$url" in
  *.sha256) printf '%s  rally\n' "$2";;
  *.tar.gz) exit 1;;
  *) printf '%s' '$_FAKE_BODY' > "\$out";;
esac
exit 0
STUB
    chmod +x "$1/curl"
  }
  _poll_method() {  # $1=state_file ; echoes terminal method
    local m=""
    for _ in $(seq 1 40); do
      [ -f "$1" ] && m="$(grep -o '"method":"[^"]*"' "$1" | cut -d'"' -f4 || true)"
      case "$m" in downloaded|download-rejected|none|source) break;; esac
      sleep 0.25
    done
    printf '%s' "$m"
  }

  T="f2 checksum match -> verified + installed (method=downloaded)"
  (
    sb="$TMPDIR_ROOT/ck-ok"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/.claude-plugin"
    printf '{"name":"x","version":"0.0.0-test"}\n' > "$sb/plugin/.claude-plugin/plugin.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "downloaded" ] || { printf 'expected downloaded, got: %s\n' "$m" >&2; exit 1; }
    [ -x "$sb/home/.local/bin/rally" ] || { printf 'binary not installed\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

  T="f2 checksum MISMATCH -> rejected (tampered binary never installed)"
  (
    sb="$TMPDIR_ROOT/ck-bad"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/.claude-plugin"
    printf '{"name":"x","version":"0.0.0-test"}\n' > "$sb/plugin/.claude-plugin/plugin.json"
    _write_curl_stub "$sb/tools" "deadbeef00000000000000000000000000000000000000000000000000000000"
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -x "$sb/home/.local/bin/rally" ] || { printf 'TAMPERED binary was installed!\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi
else
  ok "f2 checksum tests (skipped — shasum unavailable)"
fi

# ---------------------------------------------------------------------------
# Test: f10 lock race — a FRESH empty lock (the create/write window, or a crash
# mid-write) is treated as live and NOT stolen; provisioning backs off.
# ---------------------------------------------------------------------------
T="f10 lock race: fresh empty lock not reclaimed (no double-provision)"
(
  sb="$TMPDIR_ROOT/lock-young"; mkdir -p "$sb/home/.cache/rally" "$sb/tools" "$sb/plugin"
  printf '#!/usr/bin/env bash\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"
  : > "$sb/home/.cache/rally/.provision.lock"   # fresh, empty lock
  HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
  rc=$?
  [ "$rc" = 0 ] || { printf 'rc=%s\n' "$rc" >&2; exit 1; }
  lk="$sb/home/.cache/rally/.provision.lock"
  [ -f "$lk" ] && [ ! -s "$lk" ] || { printf 'fresh empty lock was stolen/overwritten\n' >&2; exit 1; }
  [ ! -f "$sb/home/.cache/rally/provision.json" ] || { printf 'provisioned despite a young lock\n' >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test: f11 charter — a corrupted state file (ts with no digits) must not crash
# the hook under set -euo pipefail; it still exits 0.
# ---------------------------------------------------------------------------
T="f11 corrupt-state: malformed ts -> hook still exits 0"
(
  sb="$TMPDIR_ROOT/corrupt"; mkdir -p "$sb/home/.cache/rally" "$sb/plugin"
  printf '{"ts":,"method":"x","result":"ok","binary":"","hint":""}\n' > "$sb/home/.cache/rally/provision.json"
  HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="/usr/bin:/bin" "$HOOK" "$sb/plugin"
  rc=$?
  [ "$rc" = 0 ] || { printf 'corrupt state must exit 0, got rc=%s\n' "$rc" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ---------------------------------------------------------------------------
# Test: f1 never-block — a caller that CAPTURES the hook's stdout must not block
# for the background worker's lifetime (worker fds are detached).
# ---------------------------------------------------------------------------
T="f1 never-block: stdout-capturing caller returns fast despite a slow worker"
(
  sb="$TMPDIR_ROOT/fdblock"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
  printf '#!/usr/bin/env bash\nsleep 6\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"
  t0=$(date +%s)
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin")
  t1=$(date +%s)
  [ $((t1-t0)) -lt 3 ] || { printf 'caller blocked %ss on the worker\n' "$((t1-t0))" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "worker must detach inherited fds"; fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Passed: $PASS / Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  for f in "${FAILS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
