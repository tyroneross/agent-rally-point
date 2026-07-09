#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/prepush-ref-updates.sh + .githooks/pre-push.
#
# Run: tests/hooks/test_prepush_changed_files.sh
# Exits 0 on full pass, 1 on any failure. Prints "Passed: N / Failed: M".
#
# Two layers:
#   1. Parser-only tests drive scripts/prepush-ref-updates.sh directly with
#      crafted stdin — fast, no git repo needed, covers the SHA-selection
#      logic exhaustively (dedup, deletions, force-updates, multi-ref).
#   2. One end-to-end layer drives the REAL .githooks/pre-push against a
#      throwaway `git init` fixture repo, with trivial stubs for
#      scripts/run-quality-gate.sh AND scripts/check-release-parity.sh (f2 —
#      NOT the real cargo gate or the real parity check, which would make
#      this test suite as slow as a full build) so we can assert, against a
#      real git worktree, that the SHA actually checked out and validated is
#      the pushed sha, independent of the fixture repo's current HEAD, that
#      duplicates across refs run the gate once, that check-release-parity.sh
#      is invoked for every validated SHA (f2), and that a gate OR parity
#      failure blocks the push and still tears down its worktree.

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

# Deterministic well-formed 40-hex pseudo-SHAs for parser-only tests (these
# never touch git, so the values need not be real objects — just 40 hex
# chars, which is all scripts/prepush-ref-updates.sh's contract requires).
sha_for() { printf '%s' "$1" | shasum | awk '{print $1}'; }

SHA_A="$(sha_for aaa)"
SHA_B="$(sha_for bbb)"
SHA_C="$(sha_for ccc)"

# ===========================================================================
# Layer 1: parser-only tests (scripts/prepush-ref-updates.sh)
# ===========================================================================

T="parser: normal single-ref push emits that local_sha"
out=$(printf 'refs/heads/main %s refs/heads/main %s\n' "$SHA_A" "$SHA_B" | "$PARSER")
[ "$out" = "$SHA_A" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: deletion tuple (local_sha=zero) emits nothing"
out=$(printf 'refs/heads/gone %s refs/heads/gone %s\n' "$ZERO" "$SHA_B" | "$PARSER")
[ -z "$out" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: new-branch push (remote_sha=zero, local_sha real) still emits"
out=$(printf 'refs/heads/new %s refs/heads/new %s\n' "$SHA_A" "$ZERO" | "$PARSER")
[ "$out" = "$SHA_A" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: force-update (non-ff remote_sha) still emits local_sha"
out=$(printf 'refs/heads/main %s refs/heads/main %s\n' "$SHA_A" "$SHA_C" | "$PARSER")
[ "$out" = "$SHA_A" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: duplicate SHAs across two refs validate once"
out=$(printf 'refs/heads/a %s refs/heads/a %s\nrefs/heads/b %s refs/heads/b %s\n' "$SHA_A" "$ZERO" "$SHA_A" "$ZERO" | "$PARSER")
n=$(printf '%s\n' "$out" | grep -c "^$SHA_A\$")
[ "$n" = "1" ] && [ "$(printf '%s\n' "$out" | grep -vc '^$')" = "1" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: multi-ref push emits each distinct non-deletion SHA"
out=$(printf 'refs/heads/a %s refs/heads/a %s\nrefs/heads/b %s refs/heads/b %s\nrefs/heads/gone %s refs/heads/gone %s\n' \
  "$SHA_A" "$ZERO" "$SHA_B" "$ZERO" "$ZERO" "$SHA_C" | "$PARSER")
has_a=$(printf '%s\n' "$out" | grep -c "^$SHA_A\$")
has_b=$(printf '%s\n' "$out" | grep -c "^$SHA_B\$")
total=$(printf '%s\n' "$out" | grep -vc '^$')
[ "$has_a" = "1" ] && [ "$has_b" = "1" ] && [ "$total" = "2" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser: no stdin / empty input emits nothing, exit 0"
out=$(printf '' | "$PARSER"); rc=$?
[ "$rc" = "0" ] && [ -z "$out" ] && ok "$T" || bad "$T" "rc=$rc out=[$out]"

T="parser (SEC-006): non-hex \$2 is dropped, valid SHAs on other lines still emit"
out=$(printf 'refs/heads/a not-a-sha refs/heads/a %s\nrefs/heads/b %s refs/heads/b %s\n' \
  "$ZERO" "$SHA_A" "$ZERO" | "$PARSER")
[ "$out" = "$SHA_A" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser (SEC-006): uppercase-hex \$2 is dropped (git never emits uppercase SHAs)"
out=$(printf 'refs/heads/a %s refs/heads/a %s\n' "$(printf '%s' "$SHA_A" | tr 'a-f' 'A-F')" "$ZERO" | "$PARSER")
[ -z "$out" ] && ok "$T" || bad "$T" "out=[$out]"

T="parser (SEC-006): too-short \$2 (< 7 hex chars) is dropped"
out=$(printf 'refs/heads/a abc123 refs/heads/a %s\n' "$ZERO" | "$PARSER")
[ -z "$out" ] && ok "$T" || bad "$T" "out=[$out]"

# ===========================================================================
# Layer 2: end-to-end — real .githooks/pre-push against a fixture git repo,
# with a trivial stubbed scripts/run-quality-gate.sh.
# ===========================================================================

# Some machines have a `.rally/` marker walkable from the default mktemp
# parent (e.g. under /private/tmp), which can confuse anything that walks up
# looking for repo markers. Use /var/tmp by default, mirroring
# tests/hooks/test_rally_coordination_hook.sh.
scratch_parent="${RALLY_TEST_TMPDIR:-/var/tmp}"
FIXTURE="$(mktemp -d "${scratch_parent%/}/rally-prepush-e2e.XXXXXX")"
MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-e2e-marker.XXXXXX")"
PARITY_MARKER="$(mktemp "${scratch_parent%/}/rally-prepush-e2e-parity-marker.XXXXXX")"
cleanup_e2e() {
  rm -rf "$FIXTURE" 2>/dev/null || true
  rm -f "$MARKER" 2>/dev/null || true
  rm -f "$PARITY_MARKER" 2>/dev/null || true
}
trap cleanup_e2e EXIT

git -C "$FIXTURE" init -q -b main
git -C "$FIXTURE" config user.email "prepush-test@example.com"
git -C "$FIXTURE" config user.name "Prepush Test"

mkdir -p "$FIXTURE/scripts" "$FIXTURE/.githooks"
cp "$PARSER" "$FIXTURE/scripts/prepush-ref-updates.sh"
chmod +x "$FIXTURE/scripts/prepush-ref-updates.sh"
cp "$PREPUSH_HOOK" "$FIXTURE/.githooks/pre-push"
chmod +x "$FIXTURE/.githooks/pre-push"

# Trivial stub gate — NOT the real cargo gate (that would make this suite as
# slow as a full build). Records the SHA it was run against (via
# `git rev-parse HEAD` inside whatever worktree it's executing in) to
# $RALLY_TEST_MARKER, then exits $RALLY_TEST_GATE_EXIT (default 0), so the
# test can assert exactly which SHA(s) .githooks/pre-push actually validated.
cat > "$FIXTURE/scripts/run-quality-gate.sh" <<'STUB'
#!/bin/sh
set -e
if [ -n "${RALLY_TEST_MARKER:-}" ]; then
  git rev-parse HEAD >> "$RALLY_TEST_MARKER"
fi
exit "${RALLY_TEST_GATE_EXIT:-0}"
STUB
chmod +x "$FIXTURE/scripts/run-quality-gate.sh"

# f2: trivial stub release-parity check — same marker/exit-code pattern as
# the gate stub above, so we can assert .githooks/pre-push actually invokes
# scripts/check-release-parity.sh for every validated SHA (and that a
# parity failure blocks the push exactly like a gate failure does).
cat > "$FIXTURE/scripts/check-release-parity.sh" <<'STUB'
#!/bin/sh
set -e
if [ -n "${RALLY_TEST_PARITY_MARKER:-}" ]; then
  git rev-parse HEAD >> "$RALLY_TEST_PARITY_MARKER"
fi
exit "${RALLY_TEST_PARITY_EXIT:-0}"
STUB
chmod +x "$FIXTURE/scripts/check-release-parity.sh"

echo "base" > "$FIXTURE/README.md"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "base: stub gate + parser + hook"
SHA_BASE="$(git -C "$FIXTURE" rev-parse HEAD)"

echo "second" >> "$FIXTURE/README.md"
git -C "$FIXTURE" add -A
git -C "$FIXTURE" commit -q -m "second commit"
SHA_HEAD="$(git -C "$FIXTURE" rev-parse HEAD)"

# run_prepush STDIN_TUPLES [GATE_EXIT=0] [PARITY_EXIT=0]
# Invokes the fixture's .githooks/pre-push exactly as git would (stdin =
# ref-update tuples, argv = remote name + url), with RALLY_SKIP_PREPUSH
# explicitly unset so no ambient env can short-circuit the test.
run_prepush() {
  local stdin_data="$1"
  local gate_exit="${2:-0}"
  local parity_exit="${3:-0}"
  ( cd "$FIXTURE" && printf '%s\n' "$stdin_data" \
      | env -u RALLY_SKIP_PREPUSH RALLY_TEST_MARKER="$MARKER" RALLY_TEST_GATE_EXIT="$gate_exit" \
        RALLY_TEST_PARITY_MARKER="$PARITY_MARKER" RALLY_TEST_PARITY_EXIT="$parity_exit" \
        sh .githooks/pre-push origin fake-remote-url ) 2>&1
}

worktree_count() {
  git -C "$FIXTURE" worktree list --porcelain 2>/dev/null | grep -c '^worktree '
}

# ---------------------------------------------------------------------------
# (a) a normal HEAD push validates
# ---------------------------------------------------------------------------
T="e2e: normal push of current HEAD validates that SHA"
: > "$MARKER"
out=$(run_prepush "refs/heads/main $SHA_HEAD refs/heads/main $ZERO")
rc=$?
recorded="$(cat "$MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_HEAD" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded]"; note "$out"
fi

# ---------------------------------------------------------------------------
# (b) a non-HEAD SHA is the one validated (not HEAD) — proves the hook gates
# the OBJECT BEING PUSHED, not whatever the fixture repo's current checkout
# happens to be (fixture HEAD is SHA_HEAD throughout this whole test).
# ---------------------------------------------------------------------------
T="e2e: non-HEAD local_sha is validated, not the repo's current HEAD"
: > "$MARKER"
out=$(run_prepush "refs/heads/main $SHA_BASE refs/heads/main $ZERO")
rc=$?
recorded="$(cat "$MARKER" 2>/dev/null)"
fixture_head_after="$(git -C "$FIXTURE" rev-parse HEAD)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_BASE" ] && [ "$recorded" != "$SHA_HEAD" ] && [ "$fixture_head_after" = "$SHA_HEAD" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded] fixture_head_after=$fixture_head_after (expect $SHA_HEAD unchanged)"; note "$out"
fi

# ---------------------------------------------------------------------------
# (c) a deletion tuple validates nothing / exits 0
# ---------------------------------------------------------------------------
T="e2e: deletion tuple validates nothing and exits 0"
: > "$MARKER"
out=$(run_prepush "refs/heads/gone $ZERO refs/heads/gone $SHA_HEAD")
rc=$?
recorded="$(cat "$MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ -z "$recorded" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded]"; note "$out"
fi

# ---------------------------------------------------------------------------
# (d) a force-update still validates (non-fast-forward local/remote pair)
# ---------------------------------------------------------------------------
T="e2e: force-update (non-ff local/remote pair) still validates local_sha"
: > "$MARKER"
out=$(run_prepush "refs/heads/main $SHA_BASE refs/heads/main $SHA_HEAD")
rc=$?
recorded="$(cat "$MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$recorded" = "$SHA_BASE" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc recorded=[$recorded]"; note "$out"
fi

# ---------------------------------------------------------------------------
# (e) duplicate SHAs across refs validate once
# ---------------------------------------------------------------------------
T="e2e: duplicate SHA across two refs runs the gate exactly once"
: > "$MARKER"
tuples="refs/heads/a $SHA_HEAD refs/heads/a $ZERO
refs/heads/b $SHA_HEAD refs/heads/b $ZERO"
out=$(run_prepush "$tuples")
rc=$?
count=$(grep -c "^$SHA_HEAD\$" "$MARKER" 2>/dev/null || echo 0)
if [ "$rc" = "0" ] && [ "$count" = "1" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc count=$count"; note "$out"
fi

# ---------------------------------------------------------------------------
# (f) a multi-ref push validates each distinct non-deletion SHA
# ---------------------------------------------------------------------------
T="e2e: multi-ref push validates each distinct non-deletion SHA"
: > "$MARKER"
tuples="refs/heads/a $SHA_BASE refs/heads/a $ZERO
refs/heads/b $SHA_HEAD refs/heads/b $ZERO"
out=$(run_prepush "$tuples")
rc=$?
has_base=$(grep -c "^$SHA_BASE\$" "$MARKER" 2>/dev/null || echo 0)
has_head=$(grep -c "^$SHA_HEAD\$" "$MARKER" 2>/dev/null || echo 0)
if [ "$rc" = "0" ] && [ "$has_base" = "1" ] && [ "$has_head" = "1" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc has_base=$has_base has_head=$has_head"; note "$out"
fi

# ---------------------------------------------------------------------------
# Bonus: a gate failure blocks the push AND the worktree is still cleaned up
# (the trap runs on every exit path, not just success).
# ---------------------------------------------------------------------------
T="e2e: gate failure blocks the push (non-zero exit)"
: > "$MARKER"
out=$(run_prepush "refs/heads/main $SHA_HEAD refs/heads/main $ZERO" 7)
rc=$?
[ "$rc" != "0" ] && ok "$T" || bad "$T" "expected non-zero rc, got $rc"; note "$out" > /dev/null

T="e2e: no stray worktrees remain after a gate failure"
wc_after="$(worktree_count)"
[ "$wc_after" = "1" ] && ok "$T" || bad "$T" "worktree_count=$wc_after (expected 1, just the fixture's own)"

T="e2e: no stray worktrees remain after a successful run"
: > "$MARKER"
run_prepush "refs/heads/main $SHA_HEAD refs/heads/main $ZERO" >/dev/null 2>&1
wc_after2="$(worktree_count)"
[ "$wc_after2" = "1" ] && ok "$T" || bad "$T" "worktree_count=$wc_after2 (expected 1)"

# ---------------------------------------------------------------------------
# f2 (2026-07-09): .githooks/pre-push must also invoke
# scripts/check-release-parity.sh for every validated SHA, inside the same
# detached worktree the quality gate ran in — mirrors the gate-stub pattern
# above via scripts/check-release-parity.sh's own marker/exit stub.
# ---------------------------------------------------------------------------
T="e2e: check-release-parity.sh is invoked for the pushed SHA"
: > "$MARKER"
: > "$PARITY_MARKER"
out=$(run_prepush "refs/heads/main $SHA_HEAD refs/heads/main $ZERO")
rc=$?
parity_recorded="$(cat "$PARITY_MARKER" 2>/dev/null)"
if [ "$rc" = "0" ] && [ "$parity_recorded" = "$SHA_HEAD" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc parity_recorded=[$parity_recorded]"; note "$out"
fi

T="e2e: parity runs once per distinct SHA, in the same worktree as the gate"
: > "$MARKER"
: > "$PARITY_MARKER"
tuples="refs/heads/a $SHA_BASE refs/heads/a $ZERO
refs/heads/b $SHA_HEAD refs/heads/b $ZERO"
out=$(run_prepush "$tuples")
rc=$?
gate_has_base=$(grep -c "^$SHA_BASE\$" "$MARKER" 2>/dev/null || echo 0)
gate_has_head=$(grep -c "^$SHA_HEAD\$" "$MARKER" 2>/dev/null || echo 0)
parity_has_base=$(grep -c "^$SHA_BASE\$" "$PARITY_MARKER" 2>/dev/null || echo 0)
parity_has_head=$(grep -c "^$SHA_HEAD\$" "$PARITY_MARKER" 2>/dev/null || echo 0)
if [ "$rc" = "0" ] && [ "$gate_has_base" = "1" ] && [ "$gate_has_head" = "1" ] \
   && [ "$parity_has_base" = "1" ] && [ "$parity_has_head" = "1" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc gate: base=$gate_has_base head=$gate_has_head; parity: base=$parity_has_base head=$parity_has_head"
  note "$out"
fi

T="e2e: parity failure blocks the push (non-zero exit), independent of gate exit"
: > "$MARKER"
: > "$PARITY_MARKER"
out=$(run_prepush "refs/heads/main $SHA_HEAD refs/heads/main $ZERO" 0 7)
rc=$?
gate_ran="$(cat "$MARKER" 2>/dev/null)"
[ "$rc" != "0" ] && [ "$gate_ran" = "$SHA_HEAD" ] \
  && ok "$T" \
  || bad "$T" "expected non-zero rc with gate having already run; got rc=$rc gate_ran=[$gate_ran]"
note "$out" > /dev/null

T="e2e: no stray worktrees remain after a parity failure"
wc_after3="$(worktree_count)"
[ "$wc_after3" = "1" ] && ok "$T" || bad "$T" "worktree_count=$wc_after3 (expected 1, just the fixture's own)"

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
