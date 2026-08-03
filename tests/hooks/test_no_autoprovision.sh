#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Adversarial suite for ARP-001: trusting or opening the repo must not run
# provisioning code on the host.
#
# The audit found that SessionStart executed hooks/rally-coordination-hook.sh,
# which invoked hooks/ensure-rally-binary.sh on both branches (.rally absent and
# .rally present). That provisioner downloaded a release binary, marked it
# executable, ran it, copied and ran a shipped plugin binary, fell back to
# cargo, and wrote into ~/.local/bin — all before the user ran any project code.
#
# METHOD. Every dangerous verb is stubbed with a recorder that writes a marker
# file when called: curl, cargo, chmod, gh, cp, mv, install. The hook then runs
# its start phase in a sandboxed HOME with that stub directory first on PATH.
# The test passes only if NO marker exists and nothing landed in
# $HOME/.local/bin/rally.
#
# Run: bash tests/hooks/test_no_autoprovision.sh
# Exits 0 on full pass, 1 on any failure.

set -u
# (deliberately not -e: we assert on exit codes)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
. "$REPO_ROOT/tests/hooks/helpers/hermetic_path.sh"
HOOK="$REPO_ROOT/hooks/rally-coordination-hook.sh"
ENGINE="$REPO_ROOT/hooks/ensure-rally-binary.sh"

if [ ! -x "$HOOK" ]; then
  echo "FAIL: hook missing or not executable at $HOOK"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()
ok()  { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

# A parent that is definitely NOT inside a rally repo (the hook walks upward
# looking for .rally/, and /private/tmp can carry one on this host).
scratch_parent="${RALLY_TEST_TMPDIR:-/var/tmp}"
TMPDIR_ROOT="$(mktemp -d "${scratch_parent%/}/rally-noprov.XXXXXX")"
trap 'rm -rf "$TMPDIR_ROOT" 2>/dev/null || true' EXIT

NODE_DIR=""
if command -v node >/dev/null 2>&1; then NODE_DIR="$(dirname "$(command -v node)")"; fi

DANGEROUS="curl cargo chmod gh cp mv install wget"

# Build a stub dir where every dangerous verb records that it was called.
# `cp`/`mv`/`install` also refuse to do the copy, so a regression cannot both
# fire and succeed.
_make_recorders() {  # $1 = sandbox dir
  local sb="$1" t
  mkdir -p "$sb/stub"
  for t in $DANGEROUS; do
    printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> "%s/called.%s"\nexit 0\n' "$sb" "$t" > "$sb/stub/$t"
    chmod +x "$sb/stub/$t"
  done
}

_assert_no_provisioning() {  # $1 = sandbox dir ; echoes reason on failure
  local sb="$1" t
  for t in $DANGEROUS; do
    if [ -f "$sb/called.$t" ]; then
      printf '%s was invoked by the hook: %s' "$t" "$(cat "$sb/called.$t" 2>/dev/null | head -3)"
      return 1
    fi
  done
  if [ -e "$sb/home/.local/bin/rally" ]; then
    printf 'the hook wrote %s' "$sb/home/.local/bin/rally"
    return 1
  fi
  if [ -e "$sb/home/.cache/rally/provision.json" ]; then
    printf 'the hook wrote a provisioning state file'
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# 1. .rally PRESENT, rally binary ABSENT — the branch that used to call
#    ensure-rally-binary.sh at rally-coordination-hook.sh:467-470.
# ---------------------------------------------------------------------------
T="ARP-001: start in a .rally repo with no rally binary provisions nothing"
(
  sb="$TMPDIR_ROOT/present"; mkdir -p "$sb/home" "$sb/repo/.rally"
  _make_recorders "$sb"
  cd "$sb/repo" || exit 1
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
        "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  [ "$rc" = "0" ] || { printf 'hook must exit 0, got %s\n' "$rc" >&2; exit 1; }
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  # And it must still be useful: the advisory has to name the explicit installer.
  if [ -n "$NODE_DIR" ]; then
    printf '%s' "$out" | grep -q "install-rally.sh" \
      || { printf 'advisory does not name the installer: %s\n' "$out" >&2; exit 1; }
    printf '%s' "$out" | grep -q "Hooks never install it" \
      || { printf 'advisory does not state that hooks never install: %s\n' "$out" >&2; exit 1; }
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the .rally-present start branch must not provision"; fi

# ---------------------------------------------------------------------------
# 2. .rally ABSENT, inside a git work tree — the branch that used to call
#    ensure-rally-binary.sh at rally-coordination-hook.sh:71-77.
# ---------------------------------------------------------------------------
T="ARP-001: start in a non-rally git repo provisions nothing"
(
  sb="$TMPDIR_ROOT/absent"; mkdir -p "$sb/home" "$sb/repo"
  cd "$sb/repo" || exit 1
  git init -q . >/dev/null 2>&1 || { printf 'SKIP (git unavailable)\n'; exit 0; }
  _make_recorders "$sb"
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
        "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  [ "$rc" = "0" ] || { printf 'hook must exit 0, got %s\n' "$rc" >&2; exit 1; }
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  # rally is absent here, so the one-time offer must point at the installer
  # rather than assume the CLI is already there.
  if [ -n "$NODE_DIR" ] && [ -n "$out" ]; then
    printf '%s' "$out" | grep -q "install-rally.sh" \
      || { printf 'no-rally offer does not name the installer: %s\n' "$out" >&2; exit 1; }
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the no-.rally start branch must not provision"; fi

# ---------------------------------------------------------------------------
# 3. .rally PRESENT with a working rally on PATH — the full start path runs, and
#    still provisions nothing. Guards against a "refresh the binary" regression.
# ---------------------------------------------------------------------------
T="ARP-001: a full start with a working rally still provisions nothing"
(
  sb="$TMPDIR_ROOT/working"; mkdir -p "$sb/home" "$sb/repo/.rally"
  _make_recorders "$sb"
  cat > "$sb/stub/rally" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  "hooks status") printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"once"}}}' ;;
  *) case "$1" in
       room) printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}' ;;
       next) printf '%s\n' '{"data":{"next":{"actionable":false}}}' ;;
       *)    printf '%s\n' '{}' ;;
     esac ;;
esac
EOF
  chmod +x "$sb/stub/rally" 2>/dev/null
  # chmod is stubbed above; set the bit with the real one so the stub rally runs.
  /bin/chmod +x "$sb/stub/rally"
  rm -f "$sb/called.chmod"
  cd "$sb/repo" || exit 1
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
        "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  [ "$rc" = "0" ] || { printf 'hook must exit 0, got %s\n' "$rc" >&2; exit 1; }
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "a healthy start path must not provision either"; fi

# ---------------------------------------------------------------------------
# 4. Every non-start phase, both with and without .rally. PreToolUse fires on
#    every edit; a provisioning call there would be worse than on start.
# ---------------------------------------------------------------------------
T="ARP-001: before-write / after-write / idle provision nothing"
(
  sb="$TMPDIR_ROOT/phases"; mkdir -p "$sb/home" "$sb/repo/.rally" "$sb/bare"
  _make_recorders "$sb"
  for phase in before-write after-write idle; do
    for dir in "$sb/repo" "$sb/bare"; do
      ( cd "$dir" && HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
        "$HOOK" "$phase" claude_code </dev/null >/dev/null 2>&1 )
      rc=$?
      [ "$rc" = "0" ] || { printf '%s in %s exited %s\n' "$phase" "$dir" "$rc" >&2; exit 1; }
    done
  done
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "no phase may provision"; fi

# ---------------------------------------------------------------------------
# 5. The provisioner itself fails closed in a hook context. Option (b) was
#    chosen: hooks/ensure-rally-binary.sh survives as the provisioning engine
#    but refuses unless RALLY_EXPLICIT_INSTALL=1, which only the explicit
#    installer sets. An accidental future re-wiring therefore fails closed
#    rather than silently provisioning again.
# ---------------------------------------------------------------------------
T="ARP-001: ensure-rally-binary.sh invoked from a hook context does not provision"
(
  sb="$TMPDIR_ROOT/engine"; mkdir -p "$sb/home" "$sb/plugin/crates/rally-cli"
  _make_recorders "$sb"
  # Exactly how the hook used to call it: no RALLY_EXPLICIT_INSTALL in scope.
  env -i HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
    PATH="$sb/stub:/usr/bin:/bin" /bin/bash "$ENGINE" "$sb/plugin" >/dev/null 2>"$sb/err"
  rc=$?
  [ "$rc" != "0" ] || { printf 'a hook-context invocation must not succeed silently\n' >&2; exit 1; }
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  grep -q "install-rally.sh" "$sb/err" \
    || { printf 'refusal does not name the installer: %s\n' "$(cat "$sb/err" 2>/dev/null)" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the provisioner must fail closed outside an explicit install"; fi

# ---------------------------------------------------------------------------
# 6. Static guard: no hook script and no generated host surface may name the
#    provisioner at all. This is what actually keeps the wiring gone — tests 1-4
#    only prove the current code path, this proves nobody re-added the call.
# ---------------------------------------------------------------------------
T="ARP-001: no hook surface references the provisioner"
(
  surfaces="hooks/rally-coordination-hook.sh hooks/hooks.json .claude/settings.json .codex/hooks.json .cursor/hooks.json config/host-integrations.json"
  bad_files=""
  for f in $surfaces; do
    [ -f "$REPO_ROOT/$f" ] || continue
    if grep -q "ensure-rally-binary" "$REPO_ROOT/$f"; then
      bad_files="$bad_files $f"
    fi
  done
  if [ -n "$bad_files" ]; then
    printf 'provisioner referenced from:%s\n' "$bad_files" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "a hook surface names the provisioner again"; fi

# ---------------------------------------------------------------------------
# 7. The explicit installer is the documented replacement and is safe to
#    inspect: --dry-run writes nothing and prints where it would write.
# ---------------------------------------------------------------------------
T="ARP-001: scripts/install-rally.sh --dry-run writes nothing"
(
  installer="$REPO_ROOT/scripts/install-rally.sh"
  [ -x "$installer" ] || { printf 'installer missing at %s\n' "$installer" >&2; exit 1; }
  sb="$TMPDIR_ROOT/dryrun"; mkdir -p "$sb/home"
  _make_recorders "$sb"
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:/usr/bin:/bin" "$installer" --dry-run 2>&1)
  rc=$?
  [ "$rc" = "0" ] || { printf 'dry run exited %s: %s\n' "$rc" "$out" >&2; exit 1; }
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  printf '%s' "$out" | grep -q "nothing was downloaded, built, or written" \
    || { printf 'dry run does not say it wrote nothing: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "attestation verify" \
    || { printf 'plan does not name the provenance check: %s\n' "$out" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "the explicit installer must print a plan and write nothing"; fi

# ---------------------------------------------------------------------------
# 8. The installer refuses the download path when gh is missing, instead of
#    degrading to an unverified download.
# ---------------------------------------------------------------------------
T="ARP-001: installer refuses the download path without gh"
(
  installer="$REPO_ROOT/scripts/install-rally.sh"
  sb="$TMPDIR_ROOT/nogh"; mkdir -p "$sb/home" "$sb/stub"
  # curl + shasum present, gh absent.
  printf '#!/usr/bin/env bash\nexit 0\n' > "$sb/stub/curl";   /bin/chmod +x "$sb/stub/curl"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$sb/stub/shasum"; /bin/chmod +x "$sb/stub/shasum"
  # RC-025: gh must be provably absent. CI runners ship it at /usr/bin/gh, so
  # "no stub written" is not "not installed" — assert the premise.
  write_path_without "$sb/nogh" gh || exit 1
  out=$(HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
        PATH="$sb/stub:$sb/nogh" "$installer" --yes 2>&1)
  rc=$?
  [ "$rc" != "0" ] || { printf 'installer must refuse without gh, got rc=0: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "Refusing the download path" \
    || { printf 'refusal reason not printed: %s\n' "$out" >&2; exit 1; }
  [ ! -e "$sb/home/.local/bin/rally" ] || { printf 'installed anyway\n' >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "missing gh must be a hard stop, not a downgrade"; fi

# ---------------------------------------------------------------------------
# 9 (SEC-001). The hook must never execute a binary that resolves inside the
#    repo it is scanning. The old code preferred ./target/debug/rally, so a
#    hostile repo could ship a committed .rally/log/*.jsonl (committed by
#    design — the .rally self-gate is NOT a mitigation) plus a committed
#    executable at that path, and opening the repo ran it.
#
#    METHOD: plant an executable at ./target/debug/rally that writes a marker
#    file. Run every phase. The marker must not appear.
# ---------------------------------------------------------------------------
T="SEC-001: an executable planted at ./target/debug/rally is never executed"
(
  sb="$TMPDIR_ROOT/repo-relative"; mkdir -p "$sb/home" "$sb/repo/.rally/log" "$sb/repo/target/debug"
  _make_recorders "$sb"
  marker="$sb/repo-binary-executed"
  printf '#!/usr/bin/env bash\nprintf x > "%s"\nprintf "%%s" "{}"\nexit 0\n' "$marker" \
    > "$sb/repo/target/debug/rally"
  /bin/chmod +x "$sb/repo/target/debug/rally"
  # A rally repo commits its ledger, so .rally/ presence is attacker-reachable.
  printf '%s\n' '{}' > "$sb/repo/.rally/log/planted.jsonl"
  cd "$sb/repo" || exit 1
  for phase in start before-write after-write idle; do
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
      PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
      "$HOOK" "$phase" claude_code </dev/null >/dev/null 2>/dev/null
    rc=$?
    [ "$rc" = "0" ] || { printf '%s exited %s\n' "$phase" "$rc" >&2; exit 1; }
    [ ! -e "$marker" ] || { printf 'the %s phase EXECUTED the repo-relative binary\n' "$phase" >&2; exit 1; }
  done
  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "no hook phase may execute a binary under the scanned repo"; fi

# ---------------------------------------------------------------------------
# 10 (SEC-001). RALLY_BIN survives as a dev override, but the repo must not be
#    able to aim it at itself. A relative path, an absolute path inside the
#    scanned repo, and a symlink that resolves into the repo are all refused.
#    The last case in this block is the POSITIVE control: an absolute path
#    outside the repo still runs, so the check discriminates instead of just
#    disabling the override.
# ---------------------------------------------------------------------------
T="SEC-001: RALLY_BIN inside the scanned repo is refused; outside still works"
(
  sb="$TMPDIR_ROOT/rally-bin-scope"; mkdir -p "$sb/home" "$sb/repo/.rally" "$sb/repo/tools" "$sb/outside"
  _make_recorders "$sb"
  inside_marker="$sb/inside-executed"
  outside_marker="$sb/outside-executed"
  printf '#!/usr/bin/env bash\nprintf x > "%s"\nprintf "%%s" "{}"\nexit 0\n' "$inside_marker" \
    > "$sb/repo/tools/rally"
  printf '#!/usr/bin/env bash\nprintf x > "%s"\nprintf "%%s" "{}"\nexit 0\n' "$outside_marker" \
    > "$sb/outside/rally"
  /bin/chmod +x "$sb/repo/tools/rally" "$sb/outside/rally"
  ln -s "$sb/repo/tools/rally" "$sb/laundered-rally"
  cd "$sb/repo" || exit 1

  _run_with_bin() {  # $1 = RALLY_BIN value ; echoes stderr
    HOME="$sb/home" XDG_CACHE_HOME="$sb/home/.cache" \
      PATH="$sb/stub:${NODE_DIR:+$NODE_DIR:}/usr/bin:/bin" \
      RALLY_BIN="$1" "$HOOK" start claude_code </dev/null 2>&1 >/dev/null
  }

  # (a) absolute, inside the repo
  err="$(_run_with_bin "$sb/repo/tools/rally")"
  [ ! -e "$inside_marker" ] || { printf 'an in-repo absolute RALLY_BIN was executed\n' >&2; exit 1; }
  printf '%s' "$err" | grep -q "refusing RALLY_BIN" \
    || { printf 'no refusal printed for an in-repo RALLY_BIN: %s\n' "$err" >&2; exit 1; }

  # (b) relative
  err="$(_run_with_bin "tools/rally")"
  [ ! -e "$inside_marker" ] || { printf 'a relative RALLY_BIN was executed\n' >&2; exit 1; }
  printf '%s' "$err" | grep -q "refusing RALLY_BIN" \
    || { printf 'no refusal printed for a relative RALLY_BIN: %s\n' "$err" >&2; exit 1; }

  # (c) symlink outside the repo that resolves back into it
  err="$(_run_with_bin "$sb/laundered-rally")"
  [ ! -e "$inside_marker" ] || { printf 'a symlink into the repo laundered RALLY_BIN\n' >&2; exit 1; }
  printf '%s' "$err" | grep -q "refusing RALLY_BIN" \
    || { printf 'no refusal printed for a laundering symlink: %s\n' "$err" >&2; exit 1; }

  # (d) POSITIVE CONTROL: absolute, outside the repo — must still run.
  _run_with_bin "$sb/outside/rally" >/dev/null
  [ -e "$outside_marker" ] \
    || { printf 'an absolute RALLY_BIN outside the repo was refused too — the check is a blanket disable\n' >&2; exit 1; }

  reason="$(_assert_no_provisioning "$sb")" || { printf '%s\n' "$reason" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "RALLY_BIN must be absolute and outside the scanned repo"; fi

echo ""
echo "Passed: $PASS / Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  for f in "${FAILS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
