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
  local fixture_abs scratch_abs source_abs current_abs
  local git_root git_root_abs git_common git_common_abs fixture_git_abs

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

  # Rally deliberately resolves a linked worktree through its common git
  # directory to the canonical repository room. A fixture whose `.git` is a
  # worktree pointer could therefore pass the cwd/root checks above and still
  # append to the source room. Disposable mutation fixtures must own their git
  # directory outright, and their common dir must be exactly that local `.git`.
  if [ ! -d "$fixture_abs/.git" ]; then
    echo "disposable-repo-guard: linked or external git directory is forbidden: $fixture_abs/.git" >&2
    return 70
  fi
  fixture_git_abs="$(cd "$fixture_abs/.git" 2>/dev/null && pwd -P)" || return 70
  git_common="$(git rev-parse --git-common-dir 2>/dev/null)" || return 70
  git_common_abs="$(cd "$git_common" 2>/dev/null && pwd -P)" || {
    echo "disposable-repo-guard: git common directory is unavailable: $git_common" >&2
    return 70
  }
  if [ "$git_common_abs" != "$fixture_git_abs" ]; then
    echo "disposable-repo-guard: external git common directory is forbidden: common=$git_common_abs fixture=$fixture_git_abs" >&2
    return 70
  fi
}
