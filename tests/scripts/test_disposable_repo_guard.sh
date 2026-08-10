#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
GUARD="$SOURCE_ROOT/scripts/disposable-repo-guard.sh"

if [ ! -f "$GUARD" ]; then
  echo "RED: disposable repository guard is missing" >&2
  exit 1
fi

# The path is derived from this checkout at runtime.
# shellcheck disable=SC1090,SC1091
source "$GUARD"

SCRATCH_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rally-disposable-guard.XXXXXX")"
trap 'rm -rf "$SCRATCH_ROOT"' EXIT
FIXTURE_PARENT="$SCRATCH_ROOT/fixtures"
FIXTURE_REPO="$FIXTURE_PARENT/repo"
OUTSIDE_REPO="$SCRATCH_ROOT/outside"
NON_GIT="$FIXTURE_PARENT/non-git"
LINKED_SOURCE="$SCRATCH_ROOT/linked-source"
LINKED_WORKTREE="$FIXTURE_PARENT/linked-worktree"
mkdir -p "$FIXTURE_REPO" "$OUTSIDE_REPO" "$NON_GIT" "$LINKED_SOURCE"
git -C "$FIXTURE_REPO" init -q
git -C "$OUTSIDE_REPO" init -q
git -C "$LINKED_SOURCE" init -q
git -C "$LINKED_SOURCE" config user.email guard@example.invalid
git -C "$LINKED_SOURCE" config user.name "Disposable Guard Test"
git -C "$LINKED_SOURCE" commit -q --allow-empty -m init
git -C "$LINKED_SOURCE" worktree add -q -b guard-linked "$LINKED_WORKTREE"

passes=0
failures=0

expect_pass() {
  local label="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    passes=$((passes + 1))
  else
    echo "FAIL: expected pass: $label" >&2
    failures=$((failures + 1))
  fi
}

expect_fail() {
  local label="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    echo "FAIL: expected refusal: $label" >&2
    failures=$((failures + 1))
  else
    passes=$((passes + 1))
  fi
}

pass_exact_fixture() {
  cd "$FIXTURE_REPO"
  rally_assert_disposable_repo "$FIXTURE_REPO" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

fail_wrong_cwd() {
  cd "$SOURCE_ROOT"
  rally_assert_disposable_repo "$FIXTURE_REPO" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

fail_source_as_fixture() {
  cd "$SOURCE_ROOT"
  rally_assert_disposable_repo "$SOURCE_ROOT" "$(dirname "$SOURCE_ROOT")" "$SOURCE_ROOT"
}

fail_outside_scratch() {
  cd "$OUTSIDE_REPO"
  rally_assert_disposable_repo "$OUTSIDE_REPO" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

fail_nested_cwd() {
  mkdir -p "$FIXTURE_REPO/nested"
  cd "$FIXTURE_REPO/nested"
  rally_assert_disposable_repo "$FIXTURE_REPO" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

fail_non_git() {
  cd "$NON_GIT"
  rally_assert_disposable_repo "$NON_GIT" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

fail_linked_worktree() {
  cd "$LINKED_WORKTREE"
  rally_assert_disposable_repo "$LINKED_WORKTREE" "$FIXTURE_PARENT" "$SOURCE_ROOT"
}

expect_pass "exact disposable repository" pass_exact_fixture
expect_fail "caller forgot to cd into fixture" fail_wrong_cwd
expect_fail "source repository selected as fixture" fail_source_as_fixture
expect_fail "fixture escapes declared scratch parent" fail_outside_scratch
expect_fail "nested cwd is not the repository root" fail_nested_cwd
expect_fail "non-git directory" fail_non_git
expect_fail "linked worktree resolves to an external common repository" fail_linked_worktree

printf 'disposable-repo-guard: passed=%s failed=%s\n' "$passes" "$failures"
[ "$failures" -eq 0 ]
