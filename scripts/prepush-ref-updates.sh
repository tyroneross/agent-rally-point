#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# prepush-ref-updates.sh — pure stdin -> stdout filter for git's pre-push hook.
#
# Git's pre-push hook receives one ref-update tuple per line on stdin:
#   <local_ref> SP <local_sha> SP <remote_ref> SP <remote_sha>
# (see `git help githooks`, PRE-PUSH). This script emits the de-duplicated
# set of non-deletion `local_sha` values that need to be quality-gated, one
# per line, on stdout.
#
# A deletion push has local_sha == the all-zero SHA (nothing to validate —
# the ref is going away, not an object being pushed). Everything else
# (normal pushes, new-branch pushes where remote_sha is all-zero but
# local_sha is real, force-updates, multiple refs in one push) is emitted.
# Duplicate local_sha values (e.g. two refs pointing at the same commit) are
# de-duplicated to one line.
#
# Pure: reads stdin, writes stdout. No git calls, no side effects — safe to
# call from `.githooks/pre-push` or drive directly from a test harness.
# Output line order is not significant; treat it as a set.
set -euo pipefail

zero_sha="0000000000000000000000000000000000000000"

awk -v zero="$zero_sha" '
  NF < 2 { next }                       # malformed/blank line — ignore, no side effects
  $2 == zero { next }                   # deletion — nothing to validate
  $2 !~ /^[0-9a-f]{7,64}$/ { next }     # SEC-006: defense-in-depth — only emit
                                         # something that actually looks like a
                                         # git object name. git itself is the
                                         # only realistic caller of this script
                                         # via .githooks/pre-push, but this keeps
                                         # a non-git caller (or a future refactor
                                         # that feeds it untrusted input) from
                                         # smuggling an arbitrary string through
                                         # to the `git worktree add --detach`
                                         # call downstream in the hook.
  !seen[$2]++ { print $2 }
'
