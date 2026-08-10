#!/usr/bin/env bash

# Refuse to run a mutating Rally fixture unless the caller is standing at the
# exact root of a git repository beneath the declared disposable scratch root.
# The source repository is an explicit forbidden target even if a caller gives
# it a permissive scratch parent.
rally_assert_disposable_repo() {
  if [ "$#" -ne 3 ]; then
    echo "disposable-repo-guard: expected <fixture-repo> <scratch-root> <source-root>" >&2
    return 64
  fi

  local fixture_repo="$1"
  local scratch_root="$2"
  local source_root="$3"
  local fixture_abs scratch_abs source_abs current_abs git_root git_root_abs

  fixture_abs="$(cd "$fixture_repo" 2>/dev/null && pwd -P)" || {
    echo "disposable-repo-guard: fixture repository is unavailable: $fixture_repo" >&2
    return 70
  }
  scratch_abs="$(cd "$scratch_root" 2>/dev/null && pwd -P)" || {
    echo "disposable-repo-guard: scratch root is unavailable: $scratch_root" >&2
    return 70
  }
  source_abs="$(cd "$source_root" 2>/dev/null && pwd -P)" || {
    echo "disposable-repo-guard: source root is unavailable: $source_root" >&2
    return 70
  }
  current_abs="$(pwd -P)"

  if [ "$current_abs" != "$fixture_abs" ]; then
    echo "disposable-repo-guard: cwd mismatch: current=$current_abs expected=$fixture_abs" >&2
    return 70
  fi
  if [ "$fixture_abs" = "$source_abs" ]; then
    echo "disposable-repo-guard: refusing the source repository: $source_abs" >&2
    return 70
  fi
  case "$fixture_abs/" in
    "$scratch_abs/"*) ;;
    *)
      echo "disposable-repo-guard: fixture escapes scratch root: fixture=$fixture_abs scratch=$scratch_abs" >&2
      return 70
      ;;
  esac

  git_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "disposable-repo-guard: cwd is not a git repository root: $current_abs" >&2
    return 70
  }
  git_root_abs="$(cd "$git_root" 2>/dev/null && pwd -P)" || return 70
  if [ "$git_root_abs" != "$fixture_abs" ]; then
    echo "disposable-repo-guard: git root mismatch: git=$git_root_abs fixture=$fixture_abs" >&2
    return 70
  fi
}
