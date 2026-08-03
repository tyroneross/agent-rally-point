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
#
# ARP-001: the script under test refuses to do anything unless
# RALLY_EXPLICIT_INSTALL=1 is set, which only scripts/install-rally.sh sets.
# Every test that exercises provisioning therefore sets it. The guard itself is
# covered here (refusal case) and in tests/hooks/test_no_autoprovision.sh (the
# hook path cannot reach it at all).

set -u
# (deliberately not -e: we need to catch exit codes from the hook)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
HOOK="$REPO_ROOT/hooks/ensure-rally-binary.sh"

# Stand in for the explicit installer. Exported so every sandboxed subshell
# below inherits it; `env -i` call sites pass it by hand.
export RALLY_EXPLICIT_INSTALL=1

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
trap 'pkill -f "$TMPDIR_ROOT" 2>/dev/null || true; rm -rf "$TMPDIR_ROOT" 2>/dev/null || { sleep 1; pkill -f "$TMPDIR_ROOT" 2>/dev/null || true; rm -rf "$TMPDIR_ROOT" 2>/dev/null || true; }' EXIT

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
# Test 5 (ARP-001): the shipped-prebuilt path is GONE. A binary sitting in the
# plugin at bin/<triple>/rally carries no checksum and no attestation, so it
# must never be copied into $HOME and run. Nothing is installed; the run falls
# through to the (unreachable, here) download/cargo paths and records
# unavailable.
# ---------------------------------------------------------------------------
T="ARP-001: a shipped plugin prebuilt is NOT copied or executed"
(
  sandbox="$TMPDIR_ROOT/t5"
  export HOME="$sandbox/home"
  export XDG_CACHE_HOME="$sandbox/home/.cache"
  mkdir -p "$HOME"

  _us="$(uname -s 2>/dev/null || echo unknown)"
  _um="$(uname -m 2>/dev/null || echo unknown)"
  TRIPLE=""
  case "${_us}:${_um}" in
    Darwin:arm64)              TRIPLE="aarch64-apple-darwin"     ;;
    Darwin:x86_64)             TRIPLE="x86_64-apple-darwin"      ;;
    Linux:x86_64)              TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) TRIPLE="aarch64-unknown-linux-gnu";;
  esac
  if [ -z "$TRIPLE" ]; then printf 'SKIP (unknown triple)\n'; exit 0; fi

  plugin_root="$sandbox/plugin"
  mkdir -p "$plugin_root/bin/$TRIPLE"
  # A "prebuilt" that records the fact it was executed. If the removed path came
  # back, this marker appears.
  marker="$sandbox/shipped-was-executed"
  printf '#!/usr/bin/env bash\nprintf x > "%s"\ncase "${1:-}" in version) exit 0;; *) exit 2;; esac\n' "$marker" \
    > "$plugin_root/bin/$TRIPLE/rally"
  chmod +x "$plugin_root/bin/$TRIPLE/rally"

  # No rally, no cargo, no curl on PATH.
  export PATH="/usr/bin:/bin"

  "$HOOK" "$plugin_root"
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi

  if [ -f "$marker" ]; then
    printf 'shipped plugin binary was EXECUTED\n' >&2; exit 1
  fi
  if [ -e "$HOME/.local/bin/rally" ]; then
    printf 'shipped plugin binary was INSTALLED into $HOME\n' >&2; exit 1
  fi
  state_file="$XDG_CACHE_HOME/rally/provision.json"
  method="$(grep -o '"method":"[^"]*"' "$state_file" 2>/dev/null | cut -d'"' -f4 || true)"
  if [ "$method" = "shipped-binary" ]; then
    printf 'shipped-binary provisioning path is still live\n' >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "unverifiable plugin prebuilts must never be installed"; fi

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
[ -n "\${CURL_LOG:-}" ] && printf '%s\n' "\$url" >> "\$CURL_LOG"
exit 0
STUB
    chmod +x "$1/curl"
  }
  # ARP-001: the download path now requires `gh attestation verify` to pass as
  # well. This stub stands in for a working gh; $2 is its exit code.
  _write_gh_stub() {  # $1=dir  $2=exit_code
    cat > "$1/gh" <<STUB
#!/usr/bin/env bash
[ -n "\${GH_LOG:-}" ] && printf '%s\n' "\$*" >> "\$GH_LOG"
exit $2
STUB
    chmod +x "$1/gh"
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

  T="f2 checksum match + attestation verified -> installed (method=downloaded)"
  (
    sb="$TMPDIR_ROOT/ck-ok"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/.claude-plugin"
    printf '{"name":"x"}\n' > "$sb/plugin/.claude-plugin/plugin.json"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 0
    CURL_LOG="$sb/curl.log" GH_LOG="$sb/gh.log" HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "downloaded" ] || { printf 'expected downloaded, got: %s\n' "$m" >&2; exit 1; }
    [ -x "$sb/home/.local/bin/rally" ] || { printf 'binary not installed\n' >&2; exit 1; }
    grep -q '/v0.0.0-test/' "$sb/curl.log" || { printf 'release identity did not pin v0.0.0-test\n' >&2; exit 1; }
    grep -q 'attestation verify' "$sb/gh.log" || { printf 'attestation was never checked: %s\n' "$(cat "$sb/gh.log" 2>/dev/null)" >&2; exit 1; }
    grep -q 'tyroneross/agent-rally-point' "$sb/gh.log" || { printf 'attestation not scoped to the repo\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

  T="f2 checksum MISMATCH -> rejected (tampered binary never installed)"
  (
    sb="$TMPDIR_ROOT/ck-bad"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/.claude-plugin"
    printf '{"name":"x","version":"0.0.0-test"}\n' > "$sb/plugin/.claude-plugin/plugin.json"
    _write_curl_stub "$sb/tools" "deadbeef00000000000000000000000000000000000000000000000000000000"
    _write_gh_stub "$sb/tools" 0
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -x "$sb/home/.local/bin/rally" ] || { printf 'TAMPERED binary was installed!\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

  # ARP-001: the checksum and the binary come from the same GitHub release, so a
  # good checksum proves nothing about a compromised release. Provenance is the
  # independent authority and it is mandatory.
  T="ARP-001 attestation FAILS -> rejected even with a matching checksum"
  (
    sb="$TMPDIR_ROOT/attest-bad"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/.claude-plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 1
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -x "$sb/home/.local/bin/rally" ] || { printf 'UNATTESTED binary was installed!\n' >&2; exit 1; }
    grep -q 'attestation-failed' "$sb/home/.cache/rally/download-rejections.log" 2>/dev/null \
      || { printf 'rejection was not recorded durably\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "a failed attestation must reject the download"; fi

  # No gh means provenance CANNOT be checked. Fail closed; do not fall through
  # to an unverified install.
  T="ARP-001 no gh -> download refused, never silently unverified"
  (
    sb="$TMPDIR_ROOT/attest-none"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"   # good checksum, but no gh on PATH
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -x "$sb/home/.local/bin/rally" ] || { printf 'UNVERIFIED binary was installed!\n' >&2; exit 1; }
    h="$(grep -o '"hint":"[^"]*"' "$sb/home/.cache/rally/provision.json" | cut -d'"' -f4 || true)"
    case "$h" in *gh*) ;; *) printf 'hint does not name the missing tool: %s\n' "$h" >&2; exit 1;; esac
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "missing gh must fail closed"; fi

  # -------------------------------------------------------------------------
  # SEC-002: the release tag is committed content. rally-release.json is a file
  # a second contributor can edit, and its `version` used to be interpolated
  # straight into the download URL. curl normalizes RFC 3986 dot segments, so
  # "0.1.7/../../../../attacker/evil/releases/download/v9" resolves to an
  # attacker-controlled path — and the .sha256 is fetched from that same path,
  # so checksum verification passes trivially.
  # -------------------------------------------------------------------------
  T="SEC-002 a traversal in the release tag is rejected before any URL is built"
  (
    sb="$TMPDIR_ROOT/tag-traversal"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.1.7/../../../../attacker/evil/releases/download/v9"}\n' \
      > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 0
    CURL_LOG="$sb/curl.log" HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
      PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'a traversal tag still installed a binary\n' >&2; exit 1; }
    [ ! -s "$sb/curl.log" ] \
      || { printf 'curl was reached with a malformed tag: %s\n' "$(cat "$sb/curl.log")" >&2; exit 1; }
    grep -q 'malformed-release-tag' "$sb/home/.cache/rally/download-rejections.log" 2>/dev/null \
      || { printf 'the malformed tag was not recorded durably\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "a tag must be validated before it reaches a URL"; fi

  T="SEC-002 a well-formed tag still resolves and downloads (positive control)"
  (
    sb="$TMPDIR_ROOT/tag-ok"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"1.2.3-rc.1"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 0
    CURL_LOG="$sb/curl.log" HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
      PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "downloaded" ] || { printf 'a valid prerelease tag was rejected, got: %s\n' "$m" >&2; exit 1; }
    grep -q '/v1.2.3-rc.1/' "$sb/curl.log" || { printf 'tag not used: %s\n' "$(cat "$sb/curl.log")" >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the tag validator must not reject legitimate versions"; fi

  # -------------------------------------------------------------------------
  # SEC-003: `gh attestation verify --repo X` only asserts that SOME workflow in
  # repo X signed the artifact. Any workflow holding `attestations: write`
  # satisfies that, so a contributor who can push a branch carrying such a
  # workflow can sign an arbitrary binary and pass the gate.
  #
  # This stub models exactly that attestation: it is valid for the repo, so
  # --repo alone passes, and it was NOT minted by release.yml, so pinning the
  # signer workflow must reject it.
  # -------------------------------------------------------------------------
  _write_gh_stub_foreign_workflow() {  # $1=dir
    cat > "$1/gh" <<'STUB'
#!/usr/bin/env bash
[ -n "${GH_LOG:-}" ] && printf '%s\n' "$*" >> "$GH_LOG"
pinned=0
for a in "$@"; do
  [ "$a" = "--signer-workflow" ] && pinned=1
done
# Signed by some other workflow in the same repo: --repo alone is satisfied,
# a signer-workflow pin is not.
[ "$pinned" = "1" ] && exit 1
exit 0
STUB
    chmod +x "$1/gh"
  }

  T="SEC-003 an attestation minted by another workflow in the repo is REJECTED"
  (
    sb="$TMPDIR_ROOT/attest-foreign"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub_foreign_workflow "$sb/tools"
    GH_LOG="$sb/gh.log" HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
      PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] || { printf 'expected download-rejected, got: %s\n' "$m" >&2; exit 1; }
    [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'a foreign-workflow attestation installed a binary\n' >&2; exit 1; }
    grep -q -- '--signer-workflow tyroneross/agent-rally-point/.github/workflows/release.yml' "$sb/gh.log" \
      || { printf 'the signer workflow was never pinned: %s\n' "$(cat "$sb/gh.log" 2>/dev/null)" >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "attestation must be pinned to the signing workflow"; fi

  # -------------------------------------------------------------------------
  # SEC-012: a failed provenance check is active-substitution evidence, and it
  # is the one signal that must never be downgraded to a silent fallback. It
  # used to record `download-rejected`, return 1, fall through to cargo, and
  # have the cargo success overwrite the hint with an empty string — after
  # which the installer printed a bare "Installed".
  # -------------------------------------------------------------------------
  _write_working_cargo() {  # $1=dir  $2=log path
    cat > "$1/cargo" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "$2"
mkdir -p "\$HOME/.local/bin"
printf '#!/usr/bin/env bash\ncase "\${1:-}" in\n  version) exit 0 ;;\n  *) exit 2 ;;\nesac\n' > "\$HOME/.local/bin/rally"
chmod +x "\$HOME/.local/bin/rally"
exit 0
STUB
    chmod +x "$1/cargo"
  }

  T="SEC-012 a FAILED attestation is terminal — cargo is never reached"
  (
    sb="$TMPDIR_ROOT/tamper-terminal"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/crates/rally-cli"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 1                 # attestation rejects the file
    _write_working_cargo "$sb/tools" "$sb/cargo.log"
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
      "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "download-rejected" ] \
      || { printf 'tamper evidence was overwritten, method is now: %s\n' "$m" >&2; exit 1; }
    [ ! -e "$sb/cargo.log" ] \
      || { printf 'cargo ran after tamper evidence: %s\n' "$(cat "$sb/cargo.log")" >&2; exit 1; }
    [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'something was installed anyway\n' >&2; exit 1; }
    h="$(grep -o '"hint":"[^"]*"' "$sb/home/.cache/rally/provision.json" | cut -d'"' -f4 || true)"
    case "$h" in *"attestation FAILED"*) ;; *) printf 'the hint no longer names the failure: %s\n' "$h" >&2; exit 1;; esac
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "tamper evidence must not be downgraded to a fallback"; fi

  T="SEC-012 an UNVERIFIABLE download still falls back to cargo, carrying the reason"
  (
    sb="$TMPDIR_ROOT/unverifiable-carry"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/crates/rally-cli"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"   # good checksum, but no gh on PATH
    _write_working_cargo "$sb/tools" "$sb/cargo.log"
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
      "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" = "source" ] || { printf 'expected the cargo fallback, got: %s\n' "$m" >&2; exit 1; }
    [ -e "$sb/cargo.log" ] || { printf 'cargo never ran\n' >&2; exit 1; }
    h="$(grep -o '"hint":"[^"]*"' "$sb/home/.cache/rally/provision.json" | cut -d'"' -f4 || true)"
    case "$h" in
      *gh*) ;;
      *) printf 'the cargo-success record blanked the download rejection reason: [%s]\n' "$h" >&2; exit 1 ;;
    esac
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the rejection reason must survive the cargo fallback"; fi

  # -------------------------------------------------------------------------
  # SEC-015: no predictable temp path, and re-verify what actually landed.
  # -------------------------------------------------------------------------
  T="SEC-015 a broken mktemp fails closed — no predictable /tmp fallback"
  (
    sb="$TMPDIR_ROOT/mktemp-broken"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 0
    printf '#!/usr/bin/env bash\nexit 1\n' > "$sb/tools/mktemp"; chmod +x "$sb/tools/mktemp"
    before="$(ls -d /tmp/rally-dl-* 2>/dev/null | wc -l | tr -d ' ')"
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
      "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ "$m" != "downloaded" ] || { printf 'downloaded through a predictable temp path\n' >&2; exit 1; }
    [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'installed despite a broken mktemp\n' >&2; exit 1; }
    after="$(ls -d /tmp/rally-dl-* 2>/dev/null | wc -l | tr -d ' ')"
    [ "$before" = "$after" ] || { printf 'a predictable /tmp/rally-dl-* path was created\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "mktemp must be mandatory, not best-effort"; fi

  T="SEC-015 the download staging dir lives under the cache and is 0700"
  (
    sb="$TMPDIR_ROOT/dl-perms"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _write_curl_stub "$sb/tools" "$_GOOD_HASH"
    _write_gh_stub "$sb/tools" 0
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
      "$HOOK" "$sb/plugin" >/dev/null 2>&1
    dl="$sb/home/.cache/rally/dl"
    [ -d "$dl" ] || { printf 'no staging dir under the cache: downloads still land elsewhere\n' >&2; exit 1; }
    mode="$(ls -ld "$dl" | awk '{print $1}')"
    case "$mode" in
      drwx------*) ;;
      *) printf 'staging dir is %s, expected drwx------\n' "$mode" >&2; exit 1 ;;
    esac
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "staging must be private and out of shared /tmp"; fi

  # The verify -> chmod -> exec -> mv window, driven for real: the downloaded
  # file is executed once as a liveness probe, and this one replaces ITSELF
  # during that probe (via a rename, so the running shell keeps its own inode).
  # Whatever lands in ~/.local/bin is therefore NOT what was verified.
  T="SEC-015 a binary that swaps itself after verification is caught after the move"
  (
    sb="$TMPDIR_ROOT/post-mv-swap"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
    printf '{"schema":"agent-rally.release-identity.v1","version":"0.0.0-test"}\n' > "$sb/plugin/rally-release.json"
    _SWAP_BODY='#!/usr/bin/env bash
case "${1:-}" in
  version)
    printf "%s\n" "#!/usr/bin/env bash" "exit 0" > "$0.swap"
    mv "$0.swap" "$0"
    exit 0
    ;;
  *) exit 2 ;;
esac'
    _SWAP_HASH="$(printf '%s' "$_SWAP_BODY" | shasum -a 256 | awk '{print $1}')"
    cat > "$sb/tools/curl" <<STUB
#!/usr/bin/env bash
out=""; url=""
while [ \$# -gt 0 ]; do
  case "\$1" in -o) out="\$2"; shift 2;; http*) url="\$1"; shift;; *) shift;; esac
done
case "\$url" in
  *.sha256) printf '%s  rally\n' "$_SWAP_HASH";;
  *) printf '%s' '$_SWAP_BODY' > "\$out";;
esac
exit 0
STUB
    chmod +x "$sb/tools/curl"
    _write_gh_stub "$sb/tools" 0
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
      "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(_poll_method "$sb/home/.cache/rally/provision.json")"
    [ ! -e "$sb/home/.local/bin/rally" ] \
      || { printf 'a swapped binary survived in %s\n' "$sb/home/.local/bin/rally" >&2; exit 1; }
    [ "$m" = "download-rejected" ] || { printf 'the swap was not recorded, method=%s\n' "$m" >&2; exit 1; }
    grep -q 'post-move-sha256-mismatch' "$sb/home/.cache/rally/download-rejections.log" 2>/dev/null \
      || { printf 'the swap was not recorded durably\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the installed bytes must be the verified bytes"; fi
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
  # Exercise Linux behavior deterministically on every host: GNU stat may emit
  # partial filesystem output before rejecting BSD flags, and flock may exist
  # even when the competing worker used the portable pid lock.
  printf '#!/usr/bin/env bash\nif [ "${1:-}" = "-f" ]; then printf "gnu-stat-partial-output\\n"; exit 1; fi\nif [ "${1:-}" = "-c" ]; then /bin/date +%%s; exit 0; fi\nexit 1\n' > "$sb/tools/stat"; chmod +x "$sb/tools/stat"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$sb/tools/flock"; chmod +x "$sb/tools/flock"
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
# ARP-001 GUARD: without RALLY_EXPLICIT_INSTALL=1 the script refuses outright.
# It exits 3, touches no network, no compiler, and no $HOME. This is the
# fail-closed backstop for a future accidental re-wiring into a lifecycle hook.
# ---------------------------------------------------------------------------
T="ARP-001 guard: no RALLY_EXPLICIT_INSTALL -> exit 3, nothing provisioned"
(
  sb="$TMPDIR_ROOT/guard"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin/crates/rally-cli"
  # Recorders: if any of these fire, provisioning happened.
  for t in curl cargo chmod gh; do
    printf '#!/usr/bin/env bash\nprintf x > "%s/called.%s"\nexit 0\n' "$sb" "$t" > "$sb/tools/$t"
    chmod +x "$sb/tools/$t"
  done
  env -i HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" \
    /bin/bash "$HOOK" "$sb/plugin" >/dev/null 2>"$sb/err"
  rc=$?
  [ "$rc" = 3 ] || { printf 'expected exit 3, got %s\n' "$rc" >&2; exit 1; }
  for t in curl cargo chmod gh; do
    [ ! -f "$sb/called.$t" ] || { printf '%s was invoked by a refused run\n' "$t" >&2; exit 1; }
  done
  [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'binary installed by a refused run\n' >&2; exit 1; }
  [ ! -e "$sb/home/.cache/rally/provision.json" ] || { printf 'state written by a refused run\n' >&2; exit 1; }
  grep -q "install-rally.sh" "$sb/err" || { printf 'refusal does not name the installer: %s\n' "$(cat "$sb/err")" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "hook-context invocation must fail closed"; fi

# ---------------------------------------------------------------------------
# ARP-001: provisioning is now FOREGROUND. The old f1/f3 tests asserted the
# opposite (detached worker, lock handed to a background pid) because a
# lifecycle hook could not be allowed to block. No hook calls this any more, and
# a human installer must report its own outcome, so the run is synchronous and
# the lock is released before return.
# ---------------------------------------------------------------------------
T="ARP-001 foreground: run is synchronous and leaves no live lock behind"
(
  sb="$TMPDIR_ROOT/fg"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
  printf '#!/usr/bin/env bash\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"
  HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
  rc=$?
  [ "$rc" = 0 ] || { printf 'rc=%s\n' "$rc" >&2; exit 1; }
  # Terminal state is already on disk at return time — no polling needed.
  r="$(grep -o '"result":"[^"]*"' "$sb/home/.cache/rally/provision.json" 2>/dev/null | cut -d'"' -f4 || true)"
  [ "$r" = "unavailable" ] || { printf 'expected a terminal result at return, got: %s\n' "$r" >&2; exit 1; }
  [ ! -f "$sb/home/.cache/rally/.provision.lock" ] || { printf 'lock survived the run\n' >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "explicit install must be synchronous"; fi


# ---------------------------------------------------------------------------
# Test: f2 liveness — a binary that crashes by SIGNAL on `version` must be
# REJECTED, never stamped 'present' (the timer must not map signal-death to 0).
# ---------------------------------------------------------------------------
T="f2 liveness: SIGSEGV-on-version binary is rejected, not stamped present"
if [ -x /bin/bash ]; then
  (
    sb="$TMPDIR_ROOT/sigcrash"; mkdir -p "$sb/home/.local/bin" "$sb/tools" "$sb/plugin"
    printf '#!/bin/bash\ncase "${1:-}" in version) kill -SEGV $$;; *) exit 2;; esac\n' > "$sb/home/.local/bin/rally"; chmod +x "$sb/home/.local/bin/rally"
    printf '#!/bin/bash\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"   # no real network
    env -i RALLY_EXPLICIT_INSTALL=1 HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" /bin/bash "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(grep -o '"method":"[^"]*"' "$sb/home/.cache/rally/provision.json" 2>/dev/null | cut -d'"' -f4 || true)"
    [ "$m" != "present" ] || { printf 'crashing binary stamped present\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "signal-death must fail liveness"; fi
else
  ok "$T (skipped — /bin/bash unavailable)"
fi

# ---------------------------------------------------------------------------
# Test: f2b liveness — a crashing rally ON PATH (the `command -v` fast-path) must
# also be rejected, not stamped present (the signal gate must cover PATH too).
# ---------------------------------------------------------------------------
T="f2b liveness: SIGSEGV rally on PATH is rejected (command -v fast-path)"
if [ -x /bin/bash ]; then
  (
    sb="$TMPDIR_ROOT/sigcrash-path"; mkdir -p "$sb/home" "$sb/bin" "$sb/plugin"
    printf '#!/bin/bash\ncase "${1:-}" in version) kill -SEGV $$;; *) exit 2;; esac\n' > "$sb/bin/rally"; chmod +x "$sb/bin/rally"
    printf '#!/bin/bash\nexit 1\n' > "$sb/bin/curl"; chmod +x "$sb/bin/curl"
    env -i RALLY_EXPLICIT_INSTALL=1 HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/bin:/usr/bin:/bin" /bin/bash "$HOOK" "$sb/plugin" >/dev/null 2>&1
    m="$(grep -o '"method":"[^"]*"' "$sb/home/.cache/rally/provision.json" 2>/dev/null | cut -d'"' -f4 || true)"
    [ "$m" != "present" ] || { printf 'crashing PATH binary stamped present\n' >&2; exit 1; }
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "PATH fast-path must also gate on liveness"; fi
else
  ok "$T (skipped — /bin/bash unavailable)"
fi

# ---------------------------------------------------------------------------
# Test: flock path — when flock(1) is present (Linux), provisioning goes through
# the flock branch. Real flock semantics (worker holds the lock, OS auto-release)
# are exercised on Linux/CI; here a stub drives the branch logic on macOS.
# ---------------------------------------------------------------------------
T="flock acquire: with flock present + acquirable, the hook provisions"
(
  sb="$TMPDIR_ROOT/flock-ok"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
  printf '#!/bin/bash\nexit 0\n' > "$sb/tools/flock"; chmod +x "$sb/tools/flock"   # acquires
  printf '#!/bin/bash\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"     # no network
  env -i RALLY_EXPLICIT_INSTALL=1 HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
  rc=$?; [ "$rc" = 0 ] || { printf 'rc=%s\n' "$rc" >&2; exit 1; }
  [ -f "$sb/home/.cache/rally/provision.json" ] || { printf 'flock path did not provision\n' >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "flock acquire branch"; fi

T="flock busy: with flock held by a peer, the hook backs off (no provisioning)"
(
  sb="$TMPDIR_ROOT/flock-busy"; mkdir -p "$sb/home" "$sb/tools" "$sb/plugin"
  printf '#!/bin/bash\nexit 1\n' > "$sb/tools/flock"; chmod +x "$sb/tools/flock"   # busy (can't acquire)
  printf '#!/bin/bash\nexit 1\n' > "$sb/tools/curl"; chmod +x "$sb/tools/curl"
  env -i RALLY_EXPLICIT_INSTALL=1 HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" PATH="$sb/tools:/usr/bin:/bin" "$HOOK" "$sb/plugin" >/dev/null 2>&1
  rc=$?; [ "$rc" = 0 ] || { printf 'rc=%s\n' "$rc" >&2; exit 1; }
  [ ! -f "$sb/home/.cache/rally/provision.json" ] || { printf 'backed-off run still provisioned\n' >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "flock busy backoff"; fi

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
