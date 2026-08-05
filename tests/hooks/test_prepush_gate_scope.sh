#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# RC-034 adversarial control (docs/ROOT-CAUSE-REGISTER.md).
#
# ARP-006 pinned three gate scripts by NAME. RC-034 found the pin protects the
# dispatcher and not the code the dispatcher runs, in two exploitable shapes:
#
#   Gap 1 — the pinned scripts/check-release-parity.sh globs
#   tests/hooks/test_*.sh FROM THE PUSHED WORKTREE and executes each. A push
#   that ADDS tests/hooks/test_zz_probe.sh modifies none of the three pinned
#   names, so .githooks/pre-push prints "gate scripts pinned to main @ <sha>"
#   and the pinned dispatcher then runs the attacker's file. The only
#   assertion over that file set was a non-zero COUNT, which fires on too few
#   and never on unexpected.
#
#   Gap 2 — the SEC-005 vacuity check is a SHA-identity test, so it only
#   catches an env-supplied pin aimed at a commit IN the push. Aim
#   RALLY_PREPUSH_GATE_PIN_REF at any other attacker-controlled ref carrying
#   the same gate scripts as the pushed branch and the pin resolves, the SHAs
#   differ, `diff -q` reports identical, and the hook affirms a healthy pin
#   having reviewed nothing. Content-identity was read as evidence of review.
#
# Two layers, mirroring tests/hooks/test_prepush_changed_files.sh:
#   1. Drives the REAL scripts/check-release-parity.sh against a throwaway
#      `git init` fixture built to pass every OTHER parity check, so the exit
#      code is a real signal and not swallowed by unrelated fixture failures.
#      Each host test writes a marker file; a marker's ABSENCE is the proof
#      that the file never executed.
#   2. Drives the REAL .githooks/pre-push against a second fixture with
#      trivial stubs for the quality, release-parity, and identity gates (not
#      the real policy/build gates, which would obscure this plumbing oracle).
#
# Named test_prepush_* on purpose: check-release-parity.sh skips that prefix
# to avoid recursing into itself (see its host-test loop), so this suite runs
# in CI instead (.github/workflows/ci.yml, "Pre-push hook suites").
#
# Run: tests/hooks/test_prepush_gate_scope.sh
# Exits 0 on full pass, 1 on any failure. Prints "Passed: N / Failed: M".

set -u
# (deliberately not -e: we assert exit codes throughout)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
PARITY="$REPO_ROOT/scripts/check-release-parity.sh"
PARSER="$REPO_ROOT/scripts/prepush-ref-updates.sh"
PREPUSH_HOOK="$REPO_ROOT/.githooks/pre-push"

for required in "$PARITY" "$PARSER" "$PREPUSH_HOOK"; do
  if [ ! -f "$required" ]; then
    echo "FAIL: missing $required"
    exit 1
  fi
done

PASS=0
FAIL=0
FAILS=()

note() { printf '  %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

ZERO="0000000000000000000000000000000000000000"

# Mirrors tests/hooks/test_prepush_changed_files.sh's tmp-parent choice: some
# machines have a `.rally/` marker walkable from the default mktemp parent,
# which can confuse anything that walks up looking for repo markers.
scratch_parent="${RALLY_TEST_TMPDIR:-/var/tmp}"

PARITY_FIXTURE="$(mktemp -d "${scratch_parent%/}/rally-gate-scope-parity.XXXXXX")"
HOOK_FIXTURE="$(mktemp -d "${scratch_parent%/}/rally-gate-scope-hook.XXXXXX")"
MARKER_DIR="$(mktemp -d "${scratch_parent%/}/rally-gate-scope-markers.XXXXXX")"
ALPHA_MARKER="$MARKER_DIR/alpha.ran"
PROBE_MARKER="$MARKER_DIR/probe.ran"
MALICIOUS_MARKER="$MARKER_DIR/pwned.ran"
PIN_ECHO="$MARKER_DIR/pin-seen.txt"

cleanup() {
  rm -rf "$PARITY_FIXTURE" "$HOOK_FIXTURE" "$MARKER_DIR" 2>/dev/null || true
}
trap cleanup EXIT

clear_markers() { rm -f "$ALPHA_MARKER" "$PROBE_MARKER" "$MALICIOUS_MARKER" "$PIN_ECHO" 2>/dev/null || true; }
exists() { [ -e "$1" ] && echo yes || echo no; }

# ===========================================================================
# Layer 1 fixture — a minimal repo that scripts/check-release-parity.sh
# passes CLEANLY (exit 0) when nothing is wrong. Every check in that script
# is satisfied with a stub so the only variable left is the host-test loop;
# that is what makes exit-code assertions below mean something.
# ===========================================================================
F="$PARITY_FIXTURE"
git -C "$F" init -q -b main
git -C "$F" config user.email "gate-scope-test@example.com"
git -C "$F" config user.name "Gate Scope Test"

mkdir -p "$F/crates/rally-cli" "$F/scripts" "$F/tests/hooks" "$F/tests/scripts" \
         "$F/.claude-plugin" "$F/.codex-plugin" "$F/plugins/codex/.codex-plugin" \
         "$F/.agents/plugins"

cat > "$F/crates/rally-cli/Cargo.toml" <<'TOML'
[package]
name = "rally-cli"
version = "9.9.9"
TOML

printf '{"name": "rally"}\n'                        > "$F/.claude-plugin/plugin.json"
printf '{"name": "rally"}\n'                        > "$F/.codex-plugin/plugin.json"
printf '{"name": "rally"}\n'                        > "$F/plugins/codex/.codex-plugin/plugin.json"
printf '{"version": "9.9.9"}\n'                     > "$F/.agents/plugins/marketplace.json"
printf '{"metadata": {"version": "9.9.9"}}\n'       > "$F/.claude-plugin/marketplace.json"

# Stub generator: --check is clean by construction here.
cat > "$F/scripts/generate_host_surfaces.py" <<'PY'
import sys
sys.exit(0)
PY

# Stub builder honouring RALLY_CODEX_DEST exactly as the real one does (SEC-003),
# so the artifact-freshness diff compares a real regeneration.
cat > "$F/scripts/build-codex-artifact.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
dest="${RALLY_CODEX_DEST:-plugins/codex/.codex-plugin}"
rm -rf "$dest"
mkdir -p "$dest"
cp -RL .codex-plugin/. "$dest"/
SH
chmod +x "$F/scripts/build-codex-artifact.sh"

for m in test_generate_host_surfaces test_sync_host_integrations; do
  cat > "$F/tests/scripts/$m.py" <<'PY'
import unittest


class Stub(unittest.TestCase):
    def test_stub(self):
        self.assertTrue(True)
PY
done

# The baseline host test: present at the pin, so it must keep running.
cat > "$F/tests/hooks/test_alpha.sh" <<SH
#!/bin/sh
touch "$ALPHA_MARKER"
exit 0
SH
chmod +x "$F/tests/hooks/test_alpha.sh"

cp "$PARITY" "$F/scripts/check-release-parity.sh"
chmod +x "$F/scripts/check-release-parity.sh"

git -C "$F" add -A
git -C "$F" commit -q -m "baseline: this commit is the pin"
SHA_PIN="$(git -C "$F" rev-parse HEAD)"

run_parity() {
  ( cd "$F" && env -u RALLY_PREPUSH_PIN_COMMIT -u RALLY_PREPUSH_ACK_UNPINNED_HOST_TEST \
      "$@" bash scripts/check-release-parity.sh ) 2>&1
}

# --- positive control ------------------------------------------------------
# Without this passing, every "REFUSED" below could be an artifact of a
# fixture that never passes anything.
T="parity fixture positive control: clean tree at the pin exits 0 and runs the host test"
clear_markers
out=$(run_parity RALLY_PREPUSH_PIN_COMMIT="$SHA_PIN")
rc=$?
if [ "$rc" = "0" ] && [ -e "$ALPHA_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc alpha_marker=$(exists "$ALPHA_MARKER")"; note "$out"
fi

# ===========================================================================
# Case (a) — RC-034 Gap 1. The push ADDS a host test that did not exist at
# the pin. The gate must refuse it AND must not execute it.
# ===========================================================================
git -C "$F" checkout -q -b feature
cat > "$F/tests/hooks/test_zz_probe.sh" <<SH
#!/bin/sh
touch "$PROBE_MARKER"
exit 0
SH
chmod +x "$F/tests/hooks/test_zz_probe.sh"
git -C "$F" add -A
git -C "$F" commit -q -m "attack: add a host test the pin never saw"

T="RC-034(a): a host test absent at the pin is REFUSED"
clear_markers
out=$(run_parity RALLY_PREPUSH_PIN_COMMIT="$SHA_PIN")
rc=$?
if [ "$rc" != "0" ] \
   && printf '%s' "$out" | grep -q "REFUSED" \
   && printf '%s' "$out" | grep -qF "tests/hooks/test_zz_probe.sh"; then
  ok "$T"
else
  bad "$T" "rc=$rc — expected non-zero plus a refusal naming the file"; note "$out"
fi

T="RC-034(a): the unpinned host test's body did NOT execute"
if [ ! -e "$PROBE_MARKER" ]; then
  ok "$T"
else
  bad "$T" "$PROBE_MARKER exists — the pushed tree's new host test EXECUTED"
fi

T="RC-034(a): the pinned host test still ran (the refusal is targeted, not a blanket skip)"
if [ -e "$ALPHA_MARKER" ]; then
  ok "$T"
else
  bad "$T" "alpha marker absent — refusing one file must not disable the suite"; note "$out"
fi

T="RC-034(a): the refusal names the override so the operator is not stuck"
if printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_UNPINNED_HOST_TEST"; then
  ok "$T"
else
  bad "$T" "no override named in the refusal"; note "$out"
fi

# ===========================================================================
# Case (b) — the ack is a real escape hatch, not a no-op. The operator who
# reviewed the file gets to run it.
# ===========================================================================
T="RC-034(b): RALLY_PREPUSH_ACK_UNPINNED_HOST_TEST=1 executes the unpinned host test"
clear_markers
out=$(run_parity RALLY_PREPUSH_PIN_COMMIT="$SHA_PIN" RALLY_PREPUSH_ACK_UNPINNED_HOST_TEST=1)
rc=$?
if [ "$rc" = "0" ] && [ -e "$PROBE_MARKER" ] && [ -e "$ALPHA_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc probe=$(exists "$PROBE_MARKER") alpha=$(exists "$ALPHA_MARKER")"; note "$out"
fi

T="RC-034(b): the ack path says out loud that it is executing pushed-tree code"
if printf '%s' "$out" | grep -q "EXECUTING IT ANYWAY"; then
  ok "$T"
else
  bad "$T" "a silent ack is the bug this closes"; note "$out"
fi

# ===========================================================================
# Case (c) — no pin supplied. check-release-parity.sh also runs in CI and in
# the release workflow, where no pre-push pin exists. Behavior there must be
# byte-for-byte what it was before RC-034.
# ===========================================================================
T="RC-034(c): with RALLY_PREPUSH_PIN_COMMIT unset, every host test runs and the gate passes"
clear_markers
out=$(run_parity)
rc=$?
if [ "$rc" = "0" ] && [ -e "$PROBE_MARKER" ] && [ -e "$ALPHA_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc probe=$(exists "$PROBE_MARKER") alpha=$(exists "$ALPHA_MARKER")"; note "$out"
fi

T="RC-034(c): with no pin, the gate says nothing about pinning"
if ! printf '%s' "$out" | grep -q "host tests pinned to"; then
  ok "$T"
else
  bad "$T" "unpinned callers must not see pin machinery"; note "$out"
fi

# The pre-existing vacuity check — an empty glob means the suites moved or the
# gate is running from the wrong directory — must survive, in both modes.
mkdir -p "$MARKER_DIR/stash"
mv "$F/tests/hooks/test_alpha.sh" "$F/tests/hooks/test_zz_probe.sh" "$MARKER_DIR/stash/"

T="RC-034(c): 'NO HOST TESTS FOUND' still fires with no pin"
clear_markers
out=$(run_parity)
rc=$?
if [ "$rc" != "0" ] && printf '%s' "$out" | grep -q "NO HOST TESTS FOUND"; then
  ok "$T"
else
  bad "$T" "rc=$rc — the vacuity check must not have been traded away"; note "$out"
fi

T="RC-034(c): 'NO HOST TESTS FOUND' still fires with a pin"
clear_markers
out=$(run_parity RALLY_PREPUSH_PIN_COMMIT="$SHA_PIN")
rc=$?
if [ "$rc" != "0" ] && printf '%s' "$out" | grep -q "NO HOST TESTS FOUND"; then
  ok "$T"
else
  bad "$T" "rc=$rc"; note "$out"
fi

mv "$MARKER_DIR/stash/test_alpha.sh" "$MARKER_DIR/stash/test_zz_probe.sh" "$F/tests/hooks/"
rm -f "$F/tests/hooks/test_zz_probe.sh"

# ===========================================================================
# Case (a2) — the same bypass without adding a file: EDIT a host test that
# does exist at the pin. Membership alone would miss this, so the check
# compares content too.
# ===========================================================================
T="RC-034(a2): a host test MODIFIED relative to the pin is REFUSED and does not run"
clear_markers
cat > "$F/tests/hooks/test_alpha.sh" <<SH
#!/bin/sh
touch "$PROBE_MARKER"
touch "$ALPHA_MARKER"
exit 0
SH
chmod +x "$F/tests/hooks/test_alpha.sh"
out=$(run_parity RALLY_PREPUSH_PIN_COMMIT="$SHA_PIN")
rc=$?
if [ "$rc" != "0" ] && [ ! -e "$PROBE_MARKER" ] \
   && printf '%s' "$out" | grep -q "DIFFERS from the copy at the pin"; then
  ok "$T"
else
  bad "$T" "rc=$rc probe=$(exists "$PROBE_MARKER")"; note "$out"
fi

# ===========================================================================
# Layer 2 fixture — the REAL .githooks/pre-push, stub gate scripts.
# Covers the plumbing (does the hook actually hand the pin down?) and
# RC-034 Gap 2 (an env-supplied pin aimed anywhere at all).
# ===========================================================================
H="$HOOK_FIXTURE"
git -C "$H" init -q -b main
git -C "$H" config user.email "gate-scope-test@example.com"
git -C "$H" config user.name "Gate Scope Test"

mkdir -p "$H/scripts" "$H/.githooks"
cp "$PARSER" "$H/scripts/prepush-ref-updates.sh"
cp "$PREPUSH_HOOK" "$H/.githooks/pre-push"
chmod +x "$H/scripts/prepush-ref-updates.sh" "$H/.githooks/pre-push"

# Identity policy is outside this suite's pin-plumbing scope. The real hook
# still has to resolve and invoke its mandatory dependency before either
# downstream gate can expose the pin value, so provide a neutral executable
# stub instead of letting a missing fixture file short-circuit every case.
cat > "$H/scripts/check-git-identity.sh" <<'STUB'
#!/bin/sh
exit 0
STUB

cat > "$H/scripts/run-quality-gate.sh" <<'STUB'
#!/bin/sh
exit 0
STUB
# The parity stub records what the hook handed it — this is the direct probe
# for "did the pin actually reach the dispatcher it is supposed to constrain".
cat > "$H/scripts/check-release-parity.sh" <<STUB
#!/bin/sh
printf '%s\n' "\${RALLY_PREPUSH_PIN_COMMIT:-<unset>}" >> "$PIN_ECHO"
exit 0
STUB
chmod +x "$H/scripts/check-git-identity.sh" "$H/scripts/run-quality-gate.sh" "$H/scripts/check-release-parity.sh"

echo "base" > "$H/README.md"
git -C "$H" add -A
git -C "$H" commit -q -m "base: trusted stubs (this is 'main', the default pin)"
SHA_MAIN="$(git -C "$H" rev-parse HEAD)"

run_hook() {
  hook_stdin="$1"; shift
  ( cd "$H" && printf '%s\n' "$hook_stdin" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_ACK_GATE_CHANGE \
            -u RALLY_PREPUSH_ACK_VACUOUS_PIN -u RALLY_PREPUSH_ACK_ENV_PIN \
            -u RALLY_PREPUSH_GATE_PIN_REF -u RALLY_PREPUSH_PIN_COMMIT \
        "$@" sh .githooks/pre-push origin fake-remote-url ) 2>&1
}

# --- plumbing: the hook must hand its resolved pin to the gate scripts -----
git -C "$H" checkout -q -b work
echo "work" > "$H/NOTES.md"
git -C "$H" add -A
git -C "$H" commit -q -m "work: an ordinary push, gate scripts untouched"
SHA_WORK="$(git -C "$H" rev-parse HEAD)"
git -C "$H" checkout -q main

T="RC-034 plumbing: the hook exports its resolved pin commit to the gate scripts"
clear_markers
out=$(run_hook "refs/heads/work $SHA_WORK refs/heads/work $ZERO")
rc=$?
seen="$(cat "$PIN_ECHO" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$seen" = "$SHA_MAIN" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc parity saw [$seen], expected $SHA_MAIN"; note "$out"
fi

# The environment is not trusted, so an inherited value must not survive: a
# caller who names a commit that already contains their test file would
# otherwise re-open Gap 1 through the fix for Gap 1.
T="RC-034 plumbing: an inherited RALLY_PREPUSH_PIN_COMMIT is overwritten, not honoured"
clear_markers
out=$( ( cd "$H" && printf '%s\n' "refs/heads/work $SHA_WORK refs/heads/work $ZERO" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_GATE_PIN_REF \
            RALLY_PREPUSH_PIN_COMMIT=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1 )
rc=$?
seen="$(cat "$PIN_ECHO" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$seen" = "$SHA_MAIN" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc parity saw [$seen], expected $SHA_MAIN"; note "$out"
fi

# ===========================================================================
# Case (d) — RC-034 Gap 2. The attack the SEC-005 vacuity check cannot see:
# branch `evil` carries the SAME gate scripts as the pushed branch but is a
# DIFFERENT commit. The pin resolves, the SHAs differ so vacuity passes, and
# `diff -q` reports pinned == pushed. The pin reviewed nothing.
# ===========================================================================
git -C "$H" checkout -q -b feature main
cat > "$H/scripts/run-quality-gate.sh" <<STUB
#!/bin/sh
touch "$MALICIOUS_MARKER"
exit 0
STUB
chmod +x "$H/scripts/run-quality-gate.sh"
git -C "$H" add -A
git -C "$H" commit -q -m "malicious: gate script writes a marker"
SHA_FEATURE="$(git -C "$H" rev-parse HEAD)"

# Same bytes in scripts/, different commit — that is the whole trick.
git -C "$H" checkout -q -b evil feature
echo "decoy" > "$H/DECOY.md"
git -C "$H" add -A
git -C "$H" commit -q -m "evil: identical gate scripts, different commit"
SHA_EVIL="$(git -C "$H" rev-parse HEAD)"
git -C "$H" checkout -q main

if [ "$SHA_EVIL" = "$SHA_FEATURE" ]; then
  echo "FIXTURE BUG: evil and feature must be distinct commits" >&2
  exit 1
fi

T="RC-034(d): an env-supplied pin on a ref NOT in the push is REFUSED"
clear_markers
out=$(run_hook "refs/heads/feature $SHA_FEATURE refs/heads/feature $ZERO" \
        RALLY_PREPUSH_GATE_PIN_REF=evil)
rc=$?
if [ "$rc" != "0" ] \
   && printf '%s' "$out" | grep -q "REFUSED" \
   && printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_ENV_PIN"; then
  ok "$T"
else
  bad "$T" "rc=$rc — a resolving env pin must require an operator ack"; note "$out"
fi

T="RC-034(d): the gate script the attacker-chosen pin vouched for did NOT run"
if [ ! -e "$MALICIOUS_MARKER" ]; then
  ok "$T"
else
  bad "$T" "$MALICIOUS_MARKER exists — content-identity was accepted as review"
fi

T="RC-034(d): the vacuity check alone would have passed this push"
# Proof the case is genuinely outside SEC-005's reach: no vacuity language.
if ! printf '%s' "$out" | grep -q "SAME COMMIT AS THE PIN"; then
  ok "$T"
else
  bad "$T" "the fixture drifted into the vacuous case; it no longer tests Gap 2"; note "$out"
fi

T="RC-034(d): RALLY_PREPUSH_ACK_ENV_PIN=1 lets the operator proceed"
clear_markers
out=$(run_hook "refs/heads/feature $SHA_FEATURE refs/heads/feature $ZERO" \
        RALLY_PREPUSH_GATE_PIN_REF=evil RALLY_PREPUSH_ACK_ENV_PIN=1)
rc=$?
if [ "$rc" = "0" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc — the ack must be a real escape hatch"; note "$out"
fi

# The default pin is not env-supplied, so nothing above may touch it.
T="RC-034(d): the DEFAULT 'main' pin still needs no ack"
clear_markers
out=$(run_hook "refs/heads/work $SHA_WORK refs/heads/work $ZERO")
rc=$?
if [ "$rc" = "0" ] && ! printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_ENV_PIN"; then
  ok "$T"
else
  bad "$T" "rc=$rc — the default path must be unchanged"; note "$out"
fi

# An env pin that does not resolve keeps the bootstrap fallback (loud warning,
# pushed-tree scripts, no ack). Locked down here so a later change to the
# RC-034b refusal cannot silently swallow the brand-new-repo path.
T="RC-034(d): an UNRESOLVABLE env pin still takes the bootstrap fallback"
clear_markers
out=$(run_hook "refs/heads/work $SHA_WORK refs/heads/work $ZERO" \
        RALLY_PREPUSH_GATE_PIN_REF=does-not-exist-anywhere)
rc=$?
seen="$(cat "$PIN_ECHO" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$seen" = "<unset>" ] \
   && printf '%s' "$out" | grep -qi "WARNING.*does-not-exist-anywhere"; then
  ok "$T"
else
  bad "$T" "rc=$rc parity saw [$seen], expected <unset>"; note "$out"
fi

# ===========================================================================
# Summary
# ===========================================================================
echo ""
echo "Passed: $PASS"
echo "Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  for f in "${FAILS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
