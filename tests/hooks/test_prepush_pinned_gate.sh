#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# ARP-006 adversarial control (GitHub issue #52, third-party security audit).
#
# .githooks/pre-push builds a detached worktree at the pushed SHA and used to
# execute scripts/run-quality-gate.sh + scripts/check-release-parity.sh
# straight out of that worktree. A pushed branch that edited either script
# would therefore execute its own (possibly malicious) version, unreviewed.
#
# The fix pins both gate scripts to ${RALLY_PREPUSH_GATE_PIN_REF:-main} and
# only falls through to the pushed tree's copy when the operator explicitly
# sets RALLY_PREPUSH_ACK_GATE_CHANGE=1 after reviewing the printed diff, or
# when no pin ref resolves at all (bootstrap).
#
# This file is the DIRECT adversarial control for ARP-006: it pushes a SHA
# whose scripts/run-quality-gate.sh has been modified to write a marker
# file, and asserts the marker is NOT created — i.e. the modified gate from
# the pushed tree was never executed.
#
# Run: tests/hooks/test_prepush_pinned_gate.sh
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
FIXTURE="$(mktemp -d "${scratch_parent%/}/rally-prepush-pin-e2e.XXXXXX")"
GATE_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-gate-marker.XXXXXX")"
MALICIOUS_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-pwned-marker.XXXXXX")"
rm -f "$MALICIOUS_MARKER"  # existence-of-file IS the assertion below
cleanup_e2e() {
  rm -rf "$FIXTURE" 2>/dev/null || true
  rm -f "$GATE_MARKER" "$MALICIOUS_MARKER" 2>/dev/null || true
}
trap cleanup_e2e EXIT

git -C "$FIXTURE" init -q -b main
git -C "$FIXTURE" config user.email "prepush-pin-test@example.com"
git -C "$FIXTURE" config user.name "Prepush Pin Test"

mkdir -p "$FIXTURE/scripts" "$FIXTURE/.githooks"
cp "$PARSER" "$FIXTURE/scripts/prepush-ref-updates.sh"
chmod +x "$FIXTURE/scripts/prepush-ref-updates.sh"
cp "$PREPUSH_HOOK" "$FIXTURE/.githooks/pre-push"
chmod +x "$FIXTURE/.githooks/pre-push"

# Trivial honest stub gate — records the SHA it ran against, like
# test_prepush_changed_files.sh's stub. This is the TRUSTED version that
# will be committed on `main` and pinned.
cat > "$FIXTURE/scripts/run-quality-gate.sh" <<'STUB'
#!/bin/sh
set -e
if [ -n "${RALLY_TEST_MARKER:-}" ]; then
  git rev-parse HEAD >> "$RALLY_TEST_MARKER"
fi
exit "${RALLY_TEST_GATE_EXIT:-0}"
STUB
chmod +x "$FIXTURE/scripts/run-quality-gate.sh"

cat > "$FIXTURE/scripts/check-release-parity.sh" <<'STUB'
#!/bin/sh
exit 0
STUB
chmod +x "$FIXTURE/scripts/check-release-parity.sh"

echo "base" > "$FIXTURE/README.md"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "base: trusted stub gate + parser + hook (this is 'main')"
SHA_MAIN="$(git -C "$FIXTURE" rev-parse HEAD)"

# run_prepush STDIN_TUPLES [extra env assignments...]
run_prepush() {
  stdin_data="$1"
  shift
  ( cd "$FIXTURE" && printf '%s\n' "$stdin_data" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_ACK_GATE_CHANGE -u RALLY_PREPUSH_ACK_VACUOUS_PIN \
        RALLY_TEST_MARKER="$GATE_MARKER" RALLY_TEST_GATE_EXIT=0 \
        "$@" \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1
}

# ---------------------------------------------------------------------------
# Positive control: a clean push (pushed tree's gate == pinned main's gate)
# still runs the gate and succeeds.
# ---------------------------------------------------------------------------
T="positive control: clean push (gate unmodified) runs via pin and succeeds"
: > "$GATE_MARKER"
out=$(run_prepush "refs/heads/main $SHA_MAIN refs/heads/main $ZERO")
rc=$?
recorded="$(cat "$GATE_MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_MAIN" ] && printf '%s' "$out" | grep -q "gate scripts pinned to main"; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded]"; note "$out"
fi

# ---------------------------------------------------------------------------
# THE adversarial control (ARP-006): a "malicious branch" commit modifies
# scripts/run-quality-gate.sh to write MALICIOUS_MARKER, in addition to the
# honest behavior. Push that SHA. Assert:
#   (1) the push is REFUSED (non-zero exit) — no explicit ack was given
#   (2) MALICIOUS_MARKER was NOT created — the modified gate never executed
#
# Committed on a SEPARATE branch ("feature"), NOT main — main must stay at
# SHA_MAIN (the trusted baseline the hook pins to) so this test actually
# exercises "pushed tree differs from pinned main", not "pin moved too".
# ---------------------------------------------------------------------------
git -C "$FIXTURE" checkout -q -b feature
cat > "$FIXTURE/scripts/run-quality-gate.sh" <<STUB
#!/bin/sh
set -e
touch "$MALICIOUS_MARKER"
if [ -n "\${RALLY_TEST_MARKER:-}" ]; then
  git rev-parse HEAD >> "\$RALLY_TEST_MARKER"
fi
exit "\${RALLY_TEST_GATE_EXIT:-0}"
STUB
chmod +x "$FIXTURE/scripts/run-quality-gate.sh"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "malicious: gate script now writes a marker file"
SHA_MALICIOUS="$(git -C "$FIXTURE" rev-parse HEAD)"
git -C "$FIXTURE" checkout -q main
main_head_after_branch="$(git -C "$FIXTURE" rev-parse main)"
if [ "$main_head_after_branch" != "$SHA_MAIN" ]; then
  echo "FIXTURE BUG: main moved to $main_head_after_branch, expected to stay at $SHA_MAIN" >&2
  exit 1
fi

T="ARP-006: pushed SHA with a modified gate script is REFUSED by default"
: > "$GATE_MARKER"
rm -f "$MALICIOUS_MARKER"
out=$(run_prepush "refs/heads/feature $SHA_MALICIOUS refs/heads/feature $ZERO")
rc=$?
if [ "$rc" != "0" ]; then
  ok "$T"
else
  bad "$T" "expected non-zero rc, got rc=$rc"; note "$out"
fi

T="ARP-006: the modified (pushed-tree) gate script's marker file was NOT created"
if [ ! -e "$MALICIOUS_MARKER" ]; then
  ok "$T"
else
  bad "$T" "$MALICIOUS_MARKER exists — the pushed tree's modified gate script EXECUTED"
fi

T="ARP-006: refusal output names the differing script and how to override"
if printf '%s' "$out" | grep -q "REFUSED" && printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_GATE_CHANGE"; then
  ok "$T"
else
  bad "$T" "refusal message missing expected markers"; note "$out"
fi

T="ARP-006: refusal output includes the actual diff of the changed script"
if printf '%s' "$out" | grep -qF "$MALICIOUS_MARKER"; then
  ok "$T"
else
  bad "$T" "diff output not surfaced to the operator"; note "$out"
fi

# ---------------------------------------------------------------------------
# Explicit-ack path: with RALLY_PREPUSH_ACK_GATE_CHANGE=1, the operator has
# reviewed the diff and accepts running the pushed tree's version. This is
# the audit's allowed fallback for "the push IS the change to the gate" —
# verify it is a real escape hatch (marker DOES get created) and NOT a no-op.
# ---------------------------------------------------------------------------
T="ack path: RALLY_PREPUSH_ACK_GATE_CHANGE=1 runs the pushed (modified) gate script"
: > "$GATE_MARKER"
rm -f "$MALICIOUS_MARKER"
out=$( ( cd "$FIXTURE" && printf '%s\n' "refs/heads/feature $SHA_MALICIOUS refs/heads/feature $ZERO" \
      | env -u RALLY_SKIP_PREPUSH RALLY_PREPUSH_ACK_GATE_CHANGE=1 \
        RALLY_TEST_MARKER="$GATE_MARKER" RALLY_TEST_GATE_EXIT=0 \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1 )
rc=$?
if [ "$rc" = "0" ] && [ -e "$MALICIOUS_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc marker_exists=$([ -e "$MALICIOUS_MARKER" ] && echo yes || echo no)"; note "$out"
fi
rm -f "$MALICIOUS_MARKER"

# ---------------------------------------------------------------------------
# Bootstrap fallback: no pin ref resolves at all -> falls back to the pushed
# tree's own copy (with a loud warning), rather than refusing every push on
# a brand-new repo that has no trusted ref yet.
# ---------------------------------------------------------------------------
T="bootstrap fallback: unresolvable pin ref falls back to pushed-tree gate with a warning"
: > "$GATE_MARKER"
out=$( ( cd "$FIXTURE" && printf '%s\n' "refs/heads/main $SHA_MAIN refs/heads/main $ZERO" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_ACK_GATE_CHANGE \
        RALLY_PREPUSH_GATE_PIN_REF="does-not-exist-anywhere" \
        RALLY_TEST_MARKER="$GATE_MARKER" RALLY_TEST_GATE_EXIT=0 \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1 )
rc=$?
recorded="$(cat "$GATE_MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_MAIN" ] && printf '%s' "$out" | grep -qi "WARNING.*does-not-exist-anywhere"; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded]"; note "$out"
fi

# ===========================================================================
# SEC-005 — the pin must not be able to silently pin to the thing it is
# supposed to be reviewing.
#
# RALLY_PREPUSH_GATE_PIN_REF is read from the environment, and the environment
# is attacker-reachable: a committed .envrc, a Makefile, an npm script, or any
# process running as this UID can set it. Point it at HEAD (or at the branch
# being pushed) and the pinned copy is byte-identical to the pushed copy —
# `diff -q` says identical, the pushed branch's own gate script runs, and the
# hook used to print an affirmative "gate scripts pinned to HEAD @ <sha>" with
# no warning. ARP-006 became a no-op that reported success.
# ===========================================================================

T="SEC-005: an env-supplied pin that resolves to the pushed commit is REFUSED"
: > "$GATE_MARKER"
rm -f "$MALICIOUS_MARKER"
out=$(run_prepush "refs/heads/feature $SHA_MALICIOUS refs/heads/feature $ZERO" \
        RALLY_PREPUSH_GATE_PIN_REF=feature)
rc=$?
if [ "$rc" != "0" ] && printf '%s' "$out" | grep -q "REFUSED"; then
  ok "$T"
else
  bad "$T" "expected non-zero rc + REFUSED, got rc=$rc"; note "$out"
fi

T="SEC-005: the malicious gate did NOT run under a self-pinned ref"
if [ ! -e "$MALICIOUS_MARKER" ]; then
  ok "$T"
else
  bad "$T" "$MALICIOUS_MARKER exists — a self-pin let the pushed gate script execute"
fi

T="SEC-005: refusal prints the pin commit and each pushed SHA side by side"
if printf '%s' "$out" | grep -q "SAME COMMIT AS THE PIN" \
   && printf '%s' "$out" | grep -qF "$SHA_MALICIOUS" \
   && printf '%s' "$out" | grep -q "RALLY_PREPUSH_ACK_VACUOUS_PIN"; then
  ok "$T"
else
  bad "$T" "operator cannot see the pin-vs-pushed relationship"; note "$out"
fi

# The literal attack from the finding: RALLY_PREPUSH_GATE_PIN_REF=HEAD while the
# working tree sits on the malicious branch.
T="SEC-005: RALLY_PREPUSH_GATE_PIN_REF=HEAD is refused, not silently accepted"
git -C "$FIXTURE" checkout -q feature
: > "$GATE_MARKER"
rm -f "$MALICIOUS_MARKER"
out=$(run_prepush "refs/heads/feature $SHA_MALICIOUS refs/heads/feature $ZERO" \
        RALLY_PREPUSH_GATE_PIN_REF=HEAD)
rc=$?
if [ "$rc" != "0" ] && [ ! -e "$MALICIOUS_MARKER" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc marker_exists=$([ -e "$MALICIOUS_MARKER" ] && echo yes || echo no)"; note "$out"
fi
git -C "$FIXTURE" checkout -q main
rm -f "$MALICIOUS_MARKER"

T="SEC-005: RALLY_PREPUSH_ACK_VACUOUS_PIN=1 runs, and says it is running unpinned"
: > "$GATE_MARKER"
rm -f "$MALICIOUS_MARKER"
out=$( ( cd "$FIXTURE" && printf '%s\n' "refs/heads/feature $SHA_MALICIOUS refs/heads/feature $ZERO" \
      | env -u RALLY_SKIP_PREPUSH -u RALLY_PREPUSH_ACK_GATE_CHANGE \
        RALLY_PREPUSH_GATE_PIN_REF=feature RALLY_PREPUSH_ACK_VACUOUS_PIN=1 \
        RALLY_TEST_MARKER="$GATE_MARKER" RALLY_TEST_GATE_EXIT=0 \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1 )
rc=$?
if [ "$rc" = "0" ] && [ -e "$MALICIOUS_MARKER" ] && printf '%s' "$out" | grep -q "effectively UNPINNED"; then
  ok "$T"
else
  bad "$T" "rc=$rc marker_exists=$([ -e "$MALICIOUS_MARKER" ] && echo yes || echo no)"; note "$out"
fi
rm -f "$MALICIOUS_MARKER"

# Pushing `main` with the DEFAULT pin is the one vacuous case that cannot be
# avoided — the pin branch IS the push. It stays allowed, but it must say so
# instead of printing a healthy-looking "pinned to main @ <sha>".
T="SEC-005: default pin on the pin branch itself warns instead of affirming"
: > "$GATE_MARKER"
out=$(run_prepush "refs/heads/main $SHA_MAIN refs/heads/main $ZERO")
rc=$?
if [ "$rc" = "0" ] \
   && printf '%s' "$out" | grep -q "SAME COMMIT AS THE PIN" \
   && printf '%s' "$out" | grep -q "NOT reviewed against an earlier baseline"; then
  ok "$T"
else
  bad "$T" "rc=$rc — a vacuous default pin must be loud"; note "$out"
fi

# ---------------------------------------------------------------------------
# SEC-005 (second half): scripts/prepush-ref-updates.sh runs first, from the
# working tree, and decides which SHAs get gated at all. It was not in
# GATE_SCRIPT_NAMES, so it was a third gate script with no pin.
# ---------------------------------------------------------------------------
T="SEC-005: a modified scripts/prepush-ref-updates.sh is pinned too, and refused"
git -C "$FIXTURE" checkout -q -b parser-attack main
PARSER_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-pin-parser-marker.XXXXXX")"
rm -f "$PARSER_MARKER"
cat > "$FIXTURE/scripts/prepush-ref-updates.sh" <<STUB
#!/bin/sh
touch "$PARSER_MARKER"
awk '{ if (\$2 != "" && \$2 !~ /^0+\$/) print \$2 }' | sort -u
STUB
chmod +x "$FIXTURE/scripts/prepush-ref-updates.sh"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "parser-attack: ref-update parser writes a marker"
SHA_PARSER="$(git -C "$FIXTURE" rev-parse HEAD)"
: > "$GATE_MARKER"
out=$(run_prepush "refs/heads/parser-attack $SHA_PARSER refs/heads/parser-attack $ZERO")
rc=$?
if [ "$rc" != "0" ] && [ ! -e "$PARSER_MARKER" ] \
   && printf '%s' "$out" | grep -q "prepush-ref-updates.sh"; then
  ok "$T"
else
  bad "$T" "rc=$rc parser_marker=$([ -e "$PARSER_MARKER" ] && echo created || echo absent)"; note "$out"
fi
rm -f "$PARSER_MARKER"
git -C "$FIXTURE" checkout -q main

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
