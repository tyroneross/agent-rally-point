#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# ARP-R-05 adversarial control (docs/ROOT-CAUSE-REGISTER.md).
#
# An independent security re-assessment found .githooks/pre-push reporting
# success on a path that reviewed nothing. Three defects, three controls here.
# Each control performs the hostile or wrong action and asserts the REFUSAL —
# a test that only exercises the happy path grades nothing.
#
#   D1 — the DEFAULT pin's vacuity check only warned. SEC-005b refused a vacuous
#   ENV pin but let a vacuous DEFAULT pin continue, on the reasoning that
#   refusing would block every push of the pin branch. Pushing `main` with the
#   pin at `main` is this repo's ORDINARY path: the warning fired on nearly
#   every push, each pinned script was diffed against its own bytes, and the
#   gate executed the pushed tree's code anyway. Control: default pin resolving
#   to a pushed SHA must exit non-zero and must not run the gate.
#
#   D2 — the affirmative "gate scripts pinned to <ref> @ <sha>" line printed
#   immediately after `git show` copied the pinned bytes out, BEFORE
#   resolve_gate_script had diffed anything and before the vacuity check ran.
#   Control: an order assertion on captured stderr — no affirmative pin
#   statement may appear before the comparison output it describes.
#
#   D3 — hooks/ensure-rally-binary.sh (curl, chmod +x, cargo install) is
#   EXECUTED by tests/hooks/test_no_autoprovision.sh and
#   tests/hooks/test_ensure_rally_binary.sh, which the pinned
#   check-release-parity.sh globs and runs out of the pushed worktree. RC-034
#   pinned those test FILES; the engine they invoke stayed outside the pin
#   because the pin hardcoded a `scripts/` prefix. Control: a push that leaves
#   every pinned script byte-identical and edits ONLY
#   hooks/ensure-rally-binary.sh must be refused, and the edited copy must not
#   execute.
#
# Fixture shape follows tests/hooks/test_prepush_pinned_gate.sh: a throwaway
# `git init` repo with trivial stubs for the quality, release-parity, and
# identity gates, driving the REAL .githooks/pre-push. The parity stub here
# executes hooks/ensure-rally-binary.sh the way the real host tests do, so
# "did the unreviewed engine run" is a marker-file question and not an
# inspection.
#
# Named test_prepush_* on purpose: check-release-parity.sh skips that prefix to
# avoid recursing into itself, so this suite runs in CI instead
# (.github/workflows/ci.yml, "Pre-push hook suites").
#
# Run: tests/hooks/test_prepush_pin.sh
# Exits 0 on full pass, 1 on any failure. Prints "Passed: N / Failed: M".

set -u
# (deliberately not -e: we assert exit codes throughout)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
PARSER="$REPO_ROOT/scripts/prepush-ref-updates.sh"
PREPUSH_HOOK="$REPO_ROOT/.githooks/pre-push"

if [ ! -x "$PARSER" ]; then
  echo "FAIL: parser missing or not executable at $PARSER"
  exit 1
fi
if [ ! -x "$PREPUSH_HOOK" ]; then
  echo "FAIL: pre-push hook missing or not executable at $PREPUSH_HOOK"
  exit 1
fi

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
FIXTURE="$(mktemp -d "${scratch_parent%/}/rally-prepush-pin-arpr05.XXXXXX")"
GATE_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-gate.XXXXXX")"
ENGINE_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-engine.XXXXXX")"
PWNED_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-pwned.XXXXXX")"
rm -f "$ENGINE_MARKER" "$PWNED_MARKER"   # existence IS the assertion below
cleanup_fixture() {
  rm -rf "$FIXTURE" 2>/dev/null || true
  rm -f "$GATE_MARKER" "$ENGINE_MARKER" "$PWNED_MARKER" 2>/dev/null || true
}
trap cleanup_fixture EXIT

git -C "$FIXTURE" init -q -b main
git -C "$FIXTURE" config user.email "prepush-pin-arpr05@example.com"
git -C "$FIXTURE" config user.name "Prepush Pin ARP-R-05"

mkdir -p "$FIXTURE/scripts" "$FIXTURE/hooks" "$FIXTURE/.githooks"
cp "$PARSER" "$FIXTURE/scripts/prepush-ref-updates.sh"
cp "$PREPUSH_HOOK" "$FIXTURE/.githooks/pre-push"
chmod +x "$FIXTURE/scripts/prepush-ref-updates.sh" "$FIXTURE/.githooks/pre-push"

# This suite grades pin trust and execution order, not identity policy. Keep
# the real hook dependency in the fixture while making it a neutral pass-through
# so it cannot short-circuit the D1-D3 assertions.
cat > "$FIXTURE/scripts/check-git-identity.sh" <<'STUB'
#!/bin/sh
exit 0
STUB

# Honest stub gate — records the SHA it ran against. This is the TRUSTED copy
# committed on `main` and pinned.
cat > "$FIXTURE/scripts/run-quality-gate.sh" <<'STUB'
#!/bin/sh
set -e
if [ -n "${RALLY_TEST_MARKER:-}" ]; then
  git rev-parse HEAD >> "$RALLY_TEST_MARKER"
fi
exit 0
STUB

# Stands in for the real check-release-parity.sh, which globs
# tests/hooks/test_*.sh out of the pushed worktree and runs each. Two of those
# suites execute hooks/ensure-rally-binary.sh — that transitive reach is the
# whole of D3, so the stub reproduces it literally.
cat > "$FIXTURE/scripts/check-release-parity.sh" <<'STUB'
#!/bin/sh
set -e
if [ -x ./hooks/ensure-rally-binary.sh ]; then
  ./hooks/ensure-rally-binary.sh
fi
exit 0
STUB

# The provisioning engine. Trusted copy: records that it ran, nothing else.
cat > "$FIXTURE/hooks/ensure-rally-binary.sh" <<STUB
#!/bin/sh
touch "$ENGINE_MARKER"
exit 0
STUB
chmod +x "$FIXTURE/scripts/check-git-identity.sh" \
         "$FIXTURE/scripts/run-quality-gate.sh" \
         "$FIXTURE/scripts/check-release-parity.sh" \
         "$FIXTURE/hooks/ensure-rally-binary.sh"

echo "base" > "$FIXTURE/README.md"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "base: trusted stubs + parser + hook (this is 'main', the default pin)"
SHA_MAIN="$(git -C "$FIXTURE" rev-parse HEAD)"

# run_prepush STDIN_TUPLES [extra env assignments...]
# Every ack is unset explicitly: an inherited one from the developer's shell
# would silently turn a refusal assertion into a pass.
run_prepush() {
  rp_stdin="$1"
  shift
  ( cd "$FIXTURE" && printf '%s\n' "$rp_stdin" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_ACK_GATE_CHANGE \
            -u RALLY_PREPUSH_ACK_VACUOUS_PIN -u RALLY_PREPUSH_ACK_ENV_PIN \
            -u RALLY_PREPUSH_GATE_PIN_REF -u RALLY_PREPUSH_PIN_COMMIT \
        RALLY_TEST_MARKER="$GATE_MARKER" \
        "$@" \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1
}

clear_markers() {
  : > "$GATE_MARKER"
  rm -f "$ENGINE_MARKER" "$PWNED_MARKER"
}

# ===========================================================================
# D1 — a vacuous DEFAULT pin is REFUSED, not warned about.
#
# RALLY_PREPUSH_GATE_PIN_REF unset -> pin is `main`. Push `main` itself and the
# pin resolves to a commit in the push: every pinned script is diffed against
# its own bytes. This is the ORDINARY path for this repo, which is exactly why
# it cannot be a warning — a check that passes on every normal push certifies
# nothing while the gate goes on to execute this push's code.
#
# Fails against the pre-fix hook, which exits 0 here.
# ===========================================================================
T="D1: default pin resolving to a pushed SHA is REFUSED without the ack"
clear_markers
out=$(run_prepush "refs/heads/main $SHA_MAIN refs/heads/main $ZERO")
rc=$?
if [ "$rc" != "0" ] && printf '%s' "$out" | grep -q "REFUSED"; then
  ok "$T"
else
  bad "$T" "rc=$rc — a vacuous default pin must refuse, not warn"; note "$out"
fi

T="D1: the gate did not run under a vacuous default pin"
recorded="$(cat "$GATE_MARKER" 2>/dev/null)"
if [ -z "$recorded" ] && [ ! -e "$ENGINE_MARKER" ]; then
  ok "$T"
else
  ok_gate="$([ -z "$recorded" ] && echo none || echo "$recorded")"
  bad "$T" "gate ran anyway: quality-gate=[$ok_gate] engine=$([ -e "$ENGINE_MARKER" ] && echo ran || echo absent)"; note "$out"
fi

T="D1: the refusal names the ack and the pin/pushed relationship"
if printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_VACUOUS_PIN=1 git push" \
   && printf '%s' "$out" | grep -q "SAME COMMIT AS THE PIN" \
   && printf '%s' "$out" | grep -qF "$SHA_MAIN"; then
  ok "$T"
else
  bad "$T" "operator cannot see what is wrong or how to proceed deliberately"; note "$out"
fi

# The refusal must not be reachable by accident, and it must not be silent
# about which surface is unreviewed.
T="D1: the refusal does not print an affirmative 'gate scripts pinned to' claim"
if ! printf '%s' "$out" | grep -q "gate scripts pinned to"; then
  ok "$T"
else
  bad "$T" "an affirmative pin claim survives on a run that compared nothing"; note "$out"
fi

# ===========================================================================
# D1 (escape hatch) — the ack is a real override, not decoration. If this
# passes while the refusal above also passes, the control is a decision and not
# a wall.
# ===========================================================================
T="D1: RALLY_PREPUSH_ACK_VACUOUS_PIN=1 lets the pin branch be pushed"
clear_markers
out=$(run_prepush "refs/heads/main $SHA_MAIN refs/heads/main $ZERO" \
        RALLY_PREPUSH_ACK_VACUOUS_PIN=1)
rc=$?
recorded="$(cat "$GATE_MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_MAIN" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded] — the ack must actually proceed"; note "$out"
fi

T="D1: the acked run says out loud that it is running unpinned"
if printf '%s' "$out" | grep -q "effectively UNPINNED" \
   && printf '%s' "$out" | grep -q "NOT reviewed against an earlier baseline"; then
  ok "$T"
else
  bad "$T" "the acked path must not read as a healthy pin"; note "$out"
fi

# ===========================================================================
# D3 — hooks/ensure-rally-binary.sh is pinned too.
#
# The push edits ONLY that file. Every pinned script (run-quality-gate.sh,
# check-release-parity.sh, prepush-ref-updates.sh) is byte-identical to `main`,
# so the name-keyed pin saw nothing to report and the pinned parity dispatcher
# then executed the edited engine out of the pushed worktree.
#
# Pushed from a BRANCH, so the D1 vacuity refusal is not what produces the
# non-zero exit here — the pin stays at `main` and is genuinely independent.
# ===========================================================================
git -C "$FIXTURE" checkout -q -b engine-attack main
cat > "$FIXTURE/hooks/ensure-rally-binary.sh" <<STUB
#!/bin/sh
touch "$PWNED_MARKER"
touch "$ENGINE_MARKER"
exit 0
STUB
chmod +x "$FIXTURE/hooks/ensure-rally-binary.sh"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "engine-attack: only hooks/ensure-rally-binary.sh changes"
SHA_ENGINE="$(git -C "$FIXTURE" rev-parse HEAD)"
git -C "$FIXTURE" checkout -q main

# Fixture invariant: if any pinned script also changed, this stops testing D3
# and starts re-testing ARP-006.
changed="$(git -C "$FIXTURE" diff --name-only "$SHA_MAIN" "$SHA_ENGINE")"
if [ "$changed" != "hooks/ensure-rally-binary.sh" ]; then
  echo "FIXTURE BUG: expected only hooks/ensure-rally-binary.sh to differ, got: $changed" >&2
  exit 1
fi

T="D3: a push editing ONLY hooks/ensure-rally-binary.sh is REFUSED"
clear_markers
out=$(run_prepush "refs/heads/engine-attack $SHA_ENGINE refs/heads/engine-attack $ZERO")
rc=$?
if [ "$rc" != "0" ] && printf '%s' "$out" | grep -q "REFUSED"; then
  ok "$T"
else
  bad "$T" "rc=$rc — an unpinned engine reached execution"; note "$out"
fi

T="D3: the edited engine did NOT execute"
if [ ! -e "$PWNED_MARKER" ]; then
  ok "$T"
else
  bad "$T" "$PWNED_MARKER exists — the pushed hooks/ensure-rally-binary.sh ran unreviewed"
fi

T="D3: the refusal names hooks/ensure-rally-binary.sh and the override"
if printf '%s' "$out" | grep -qF "hooks/ensure-rally-binary.sh" \
   && printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_GATE_CHANGE=1 git push"; then
  ok "$T"
else
  bad "$T" "refusal does not identify the file or how to proceed"; note "$out"
fi

T="D3: the refusal prints the actual diff of the engine"
if printf '%s' "$out" | grep -qF "$PWNED_MARKER"; then
  ok "$T"
else
  bad "$T" "operator cannot review what changed"; note "$out"
fi

T="D3: RALLY_PREPUSH_ACK_GATE_CHANGE=1 accepts the pushed engine deliberately"
clear_markers
out=$(run_prepush "refs/heads/engine-attack $SHA_ENGINE refs/heads/engine-attack $ZERO" \
        RALLY_PREPUSH_ACK_GATE_CHANGE=1)
rc=$?
if [ "$rc" = "0" ] && [ -e "$PWNED_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc pwned_marker=$([ -e "$PWNED_MARKER" ] && echo present || echo absent) — the ack must be a real escape hatch"; note "$out"
fi

# ===========================================================================
# D2 — ordering. The affirmative pin statement must come AFTER the comparisons
# it describes. Uses a clean branch push (nothing differs from the pin) so the
# run reaches the end and emits every line.
# ===========================================================================
git -C "$FIXTURE" checkout -q -b clean-work main
echo "work" > "$FIXTURE/NOTES.md"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "clean-work: ordinary push, no gate script touched"
SHA_CLEAN="$(git -C "$FIXTURE" rev-parse HEAD)"
git -C "$FIXTURE" checkout -q main

clear_markers
ORDER_OUT="$(mktemp "${scratch_parent%/}/rally-prepush-pin-order.XXXXXX")"
run_prepush "refs/heads/clean-work $SHA_CLEAN refs/heads/clean-work $ZERO" > "$ORDER_OUT"
rc=$?

T="D2: the clean-push baseline run succeeds (ordering asserted on a real run)"
if [ "$rc" = "0" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc"; note "$(cat "$ORDER_OUT")"
fi

# line_of PATTERN -> first matching line number, or empty
line_of() { grep -n -- "$1" "$ORDER_OUT" 2>/dev/null | head -1 | cut -d: -f1; }

L_AFFIRM="$(line_of 'pin comparison complete')"
L_COMPARE="$(line_of 'running release parity')"
L_VACUITY="$(line_of 'pushing ')"

T="D2: an affirmative pin statement is emitted at all"
if [ -n "$L_AFFIRM" ]; then
  ok "$T"
else
  bad "$T" "no post-comparison pin summary found"; note "$(cat "$ORDER_OUT")"
fi

T="D2: the affirmative pin statement comes AFTER the comparison output"
if [ -n "$L_AFFIRM" ] && [ -n "$L_COMPARE" ] && [ -n "$L_VACUITY" ] \
   && [ "$L_AFFIRM" -gt "$L_COMPARE" ] && [ "$L_AFFIRM" -gt "$L_VACUITY" ]; then
  ok "$T"
else
  bad "$T" "affirm=@${L_AFFIRM:-none} compare=@${L_COMPARE:-none} vacuity=@${L_VACUITY:-none}"; note "$(cat "$ORDER_OUT")"
fi

T="D2: no affirmative 'gate scripts pinned to' line precedes any comparison"
if ! grep -q 'gate scripts pinned to' "$ORDER_OUT"; then
  ok "$T"
else
  bad "$T" "the pre-comparison affirmative line is back at line $(line_of 'gate scripts pinned to')"; note "$(cat "$ORDER_OUT")"
fi

T="D2: the summary states WHAT was compared and which copy ran"
if grep -q 'scripts/run-quality-gate.sh.*identical to pin' "$ORDER_OUT" \
   && grep -q 'scripts/check-release-parity.sh.*identical to pin' "$ORDER_OUT" \
   && grep -q 'scripts/prepush-ref-updates.sh.*identical to pin' "$ORDER_OUT" \
   && grep -q 'hooks/ensure-rally-binary.sh.*compared only' "$ORDER_OUT"; then
  ok "$T"
else
  bad "$T" "summary does not enumerate the compared paths and their verdicts"; note "$(cat "$ORDER_OUT")"
fi

T="D2: the closing line does not claim a blanket all-clear"
if ! grep -q 'all gates green' "$ORDER_OUT" \
   && grep -q 'gated 1 pushed SHA' "$ORDER_OUT" \
   && grep -q 'NOT covered by the pin' "$ORDER_OUT"; then
  ok "$T"
else
  bad "$T" "the closing line overstates what was verified"; note "$(cat "$ORDER_OUT")"
fi
rm -f "$ORDER_OUT"

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
