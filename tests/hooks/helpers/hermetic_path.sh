#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# hermetic_path.sh — build a sandbox PATH where a named tool is provably absent.
#
# WHY THIS EXISTS (RC-025). Several hook tests assert behaviour that only occurs
# when some tool is missing — "no gh -> download refused", "an UNVERIFIABLE
# download falls back to cargo". They expressed that as
# PATH="$sandbox/tools:/usr/bin:/bin" and simply did not write a stub for the
# tool, treating "I did not provide it" as "it is not there".
#
# That is an assertion about the machine, not about the code. It held on a Mac,
# where `gh` lives in /opt/homebrew/bin (outside that PATH), and was false on the
# GitHub Actions Linux runner, which ships `gh` at /usr/bin/gh. The result was a
# test that failed only in CI — and, worse, a sibling that PASSED in CI through a
# different branch than the one it names.
#
# Absence has to be established inside the sandbox. This mirrors the system tool
# directories as symlinks, minus the named tool, and then PROVES the tool cannot
# be resolved before the caller proceeds.
#
# Usage:
#   . "$(dirname "$0")/helpers/hermetic_path.sh"
#   write_path_without "$sb/nogh" gh || exit 1
#   PATH="$sb/stub:$sb/nogh" ...
#
# Returns 0 when the mirror is built and the tool is unresolvable, 1 otherwise.
# A failure is a HARNESS error and should fail the test loudly — never fall back
# to the unmirrored PATH, because that silently restores the original defect.

# shellcheck shell=bash

write_path_without() {  # $1=mirror_dir  $2=tool_to_omit
  local mirror="${1:?mirror dir required}" omit="${2:?tool to omit required}"
  local d f base

  mkdir -p "$mirror" || return 1

  for d in /usr/bin /bin /usr/local/bin; do
    [ -d "$d" ] || continue
    for f in "$d"/*; do
      [ -e "$f" ] || continue                 # unmatched glob, or a broken link
      base="$(basename "$f")"
      [ "$base" = "$omit" ] && continue
      [ -e "$mirror/$base" ] && continue      # earliest dir wins, mirroring PATH order
      ln -s "$f" "$mirror/$base" 2>/dev/null || true
    done
  done

  # The premise, established rather than assumed. If a host resolves the tool
  # some other way, the test must stop here instead of quietly exercising a
  # different code path and reporting green.
  if PATH="$mirror" command -v "$omit" >/dev/null 2>&1; then
    printf 'test harness: %s is still resolvable in the mirrored PATH at %s\n' \
      "$omit" "$mirror" >&2
    return 1
  fi

  # A mirror with nothing in it would make every test fail for the wrong reason.
  if [ -z "$(ls -A "$mirror" 2>/dev/null)" ]; then
    printf 'test harness: mirrored PATH at %s is empty\n' "$mirror" >&2
    return 1
  fi

  return 0
}
