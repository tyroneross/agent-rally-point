#!/bin/sh
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# check-git-identity.sh — reject commits whose author or committer identity
# is a known test-fixture / hostname-fallback / agent shape, or is simply
# unknown (not on the allowlist).
#
# WHY THIS EXISTS: 64 commits in this repo's history landed authored
# `Rally Test <rally@example.test>` because a test fixture wrote a
# repo-LOCAL `user.email` override into the REAL `.git/config`, silently
# beating the correct global identity. History is not rewritten for those —
# this script stops the NEXT one, at two chokepoints:
#   - `.githooks/pre-commit` calls `--pending`  (before the commit exists)
#   - `.githooks/pre-push`   calls `--commits`  (backstop for commits made
#     outside pre-commit: other clones, `--no-verify`, IDE commits, other
#     agents)
#
# WHAT THIS DELIBERATELY DOES NOT DO: it reads ONLY the author and
# committer identity fields (`%an`/`%ae`/`%cn`/`%ce` from `git log`, or
# `git var GIT_AUTHOR_IDENT`/`GIT_COMMITTER_IDENT` for a not-yet-made
# commit). It NEVER inspects the commit message body or trailers. That
# matters because CONTRIBUTING.md in this repo REQUIRES a
# `Co-Authored-By: ... <noreply@anthropic.com>` (or `<noreply@openai.com>`)
# trailer on AI-assisted commits — that is a legitimate, required convention
# and must keep working. A commit whose AUTHOR/COMMITTER are allowlisted
# humans but whose BODY happens to contain `noreply@anthropic.com` in a
# trailer passes this check every time, by construction, because the body is
# never read.
#
# Usage:
#   check-git-identity.sh --pending
#       Check the identity a commit made RIGHT NOW would carry, before it
#       exists. Source: `git var GIT_AUTHOR_IDENT` / `GIT_COMMITTER_IDENT`.
#       Used by .githooks/pre-commit.
#
#   check-git-identity.sh --commits <rev-list-args...>
#       Check every commit <rev-list-args...> resolves to (passed straight
#       to `git log`, e.g. `HEAD --not --remotes=origin`). Used by
#       .githooks/pre-push, scoped to commits not already on the remote —
#       see the pin/scope comments in .githooks/pre-push for why that
#       scoping is load-bearing and must not be widened here.
#
# Exit 0 = every identity checked is acceptable. Exit 1 = at least one was
# rejected, the allowlist could not be resolved, or the arguments/git calls
# were malformed. PRINTS NOTHING ON SUCCESS: the operator already types
# RALLY_PREPUSH_ACK_VACUOUS_PIN=1 to push in this repo; this script does not
# add a second required env var or any noise on the correct-identity path.
#
# Decision logic, in this order, applied independently to the author field
# and the committer field:
#   1. Allowlist hit (exact, case-insensitive match on the EMAIL) -> accept.
#   2. Suspect pattern hit (see SUSPECT reasons below) -> reject, naming
#      which pattern matched.
#   3. Neither -> reject as unknown, with a message that says how to
#      allowlist it. Deny-by-default is the right posture: an address that
#      is neither known-good nor a known-bad shape is exactly the case that
#      produced the historical defect.
#
# Allowlist resolution order (see also config/git-identity-allowlist.txt):
#   1. $RALLY_IDENTITY_ALLOWLIST if set (absolute path).
#   2. <repo-root>/config/git-identity-allowlist.txt.
#   3. Neither resolves -> FAIL CLOSED. A gate that silently passes because
#      its config vanished is worse than no gate.
#
# Pure POSIX sh (matches the house style of scripts/prepush-ref-updates.sh
# and .githooks/pre-push): `set -eu`, no bashisms, no `local` (function-local
# state is carried in uniquely prefixed variable names instead, since POSIX
# sh does not define `local`), no external deps beyond git/grep/sed/cut.
set -eu

usage() {
  echo "usage: check-git-identity.sh --pending" >&2
  echo "       check-git-identity.sh --commits <rev-list-args...>" >&2
  exit 1
}

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "check-git-identity: not inside a git repository" >&2
  exit 1
}

# --- allowlist resolution --------------------------------------------------
if [ -n "${RALLY_IDENTITY_ALLOWLIST:-}" ]; then
  allowlist_path="$RALLY_IDENTITY_ALLOWLIST"
  allowlist_source="\$RALLY_IDENTITY_ALLOWLIST"
else
  allowlist_path="$repo_root/config/git-identity-allowlist.txt"
  allowlist_source="repo default (config/git-identity-allowlist.txt)"
fi

if [ ! -f "$allowlist_path" ]; then
  echo "check-git-identity: FAIL CLOSED — allowlist file not found at '$allowlist_path' (source: $allowlist_source)." >&2
  echo "check-git-identity: a gate that silently passes because its config vanished is worse than no gate. Restore config/git-identity-allowlist.txt, or point \$RALLY_IDENTITY_ALLOWLIST at a valid file." >&2
  exit 1
fi

# is_allowlisted EMAIL -> 0 if EMAIL (case-insensitively) matches a
# non-comment, non-blank line of $allowlist_path exactly, 1 otherwise.
is_allowlisted() {
  ial_email_lc=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
  while IFS= read -r ial_line; do
    case "$ial_line" in
      ''|'#'*) continue ;;
    esac
    ial_entry_lc=$(printf '%s' "$ial_line" | tr '[:upper:]' '[:lower:]' | sed -e 's/[[:space:]]*$//' -e 's/^[[:space:]]*//')
    [ "$ial_entry_lc" = "$ial_email_lc" ] && return 0
  done < "$allowlist_path"
  return 1
}

# first_allowlist_entry -> the first non-comment, non-blank line of
# $allowlist_path, trimmed. Used as a last-resort "correct address" when the
# global git config itself is not sane (unset or not allowlisted).
first_allowlist_entry() {
  grep -v '^[[:space:]]*#' "$allowlist_path" | grep -v '^[[:space:]]*$' | head -n1 | sed -e 's/[[:space:]]*$//' -e 's/^[[:space:]]*//'
}

# --- suspect patterns -------------------------------------------------------
# suspect_reason EMAIL NAME -> prints a human-readable reason and returns 0
# if EMAIL/NAME match a known-bad shape; returns 1 (prints nothing) if
# neither matches. Only called after an allowlist miss, so a
# users.noreply.github.com address that IS on the allowlist never reaches
# here (allowlist hit already accepted it and stopped).
suspect_reason() {
  sr_email_lc=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
  sr_name="$2"
  case "$sr_email_lc" in
    *@example.*)
      printf '%s' "RFC 2606 reserved test domain (*@example.*) — test-fixture identity"
      return 0
      ;;
    *@*.invalid)
      printf '%s' "RFC 2606 reserved test domain (*@*.invalid) — test-fixture identity"
      return 0
      ;;
    *@*.local)
      printf '%s' "git hostname fallback (*@*.local), e.g. jason@MacBook-Air-110.local"
      return 0
      ;;
    *@localhost)
      printf '%s' "hostname fallback (*@localhost)"
      return 0
      ;;
    noreply@anthropic.com)
      printf '%s' "agent identity used as AUTHOR/COMMITTER (noreply@anthropic.com) — this address is legitimate ONLY in a Co-Authored-By trailer in the commit body, which this check never reads"
      return 0
      ;;
    noreply@openai.com)
      printf '%s' "agent identity used as AUTHOR/COMMITTER (noreply@openai.com) — this address is legitimate ONLY in a Co-Authored-By trailer in the commit body, which this check never reads"
      return 0
      ;;
    *@users.noreply.github.com)
      printf '%s' "GitHub noreply address (*@users.noreply.github.com) not on the allowlist — someone else's GitHub noreply"
      return 0
      ;;
    *) : ;;
  esac
  case "$sr_email_lc" in
    *@*) : ;;
    *)
      printf '%s' "email has no @ — malformed identity"
      return 0
      ;;
  esac
  if [ -z "$sr_name" ]; then
    printf '%s' "empty name field — malformed identity"
    return 0
  fi
  return 1
}

# check_identity NAME EMAIL -> 0 accept, 1 reject. On reject, sets
# $IDENT_REASON to a human-readable reason (allowlist miss + suspect pattern,
# or "unknown"). Single-threaded/synchronous script, no recursion — safe as
# a global.
IDENT_REASON=""
check_identity() {
  ci_name="$1"
  ci_email="$2"
  if is_allowlisted "$ci_email"; then
    return 0
  fi
  if ci_reason=$(suspect_reason "$ci_email" "$ci_name"); then
    IDENT_REASON="$ci_reason"
    return 1
  fi
  IDENT_REASON="not on the allowlist (unknown address) — neither known-good nor a known-bad shape. Add it to config/git-identity-allowlist.txt (or the file \$RALLY_IDENTITY_ALLOWLIST points at) if it is legitimate."
  return 1
}

# correct_address -> the address to suggest as "the correct one": the live
# global user.email if it is itself allowlisted (sane), else the first
# allowlist entry. Deliberately reads --global specifically, not the
# effective (local-overriding) config: in the exact defect this gate exists
# to catch, the effective config IS the bad value, so suggesting it back as
# "correct" would be circular.
correct_address() {
  ca_global=$(git config --global --get user.email 2>/dev/null || true)
  if [ -n "$ca_global" ] && is_allowlisted "$ca_global"; then
    printf '%s' "$ca_global"
    return 0
  fi
  first_allowlist_entry
}

# --- --pending ---------------------------------------------------------
pending_mode() {
  pm_author_ident=$(git var GIT_AUTHOR_IDENT)
  pm_committer_ident=$(git var GIT_COMMITTER_IDENT)

  pm_author_name=$(printf '%s' "$pm_author_ident" | sed -n 's/ <[^>]*>.*$//p')
  pm_author_email=$(printf '%s' "$pm_author_ident" | sed -n 's/^.*<\([^>]*\)>.*$/\1/p')
  pm_committer_name=$(printf '%s' "$pm_committer_ident" | sed -n 's/ <[^>]*>.*$//p')
  pm_committer_email=$(printf '%s' "$pm_committer_ident" | sed -n 's/^.*<\([^>]*\)>.*$/\1/p')

  pm_status=0

  if ! check_identity "$pm_author_name" "$pm_author_email"; then
    echo "check-git-identity: REJECTED author identity — email='$pm_author_email' name='$pm_author_name': $IDENT_REASON" >&2
    pm_status=1
  fi

  if ! check_identity "$pm_committer_name" "$pm_committer_email"; then
    echo "check-git-identity: REJECTED committer identity — email='$pm_committer_email' name='$pm_committer_name': $IDENT_REASON" >&2
    pm_status=1
  fi

  if [ "$pm_status" != "0" ]; then
    pm_local_email=$(git config --local --get user.email 2>/dev/null || true)
    pm_local_name=$(git config --local --get user.name 2>/dev/null || true)
    pm_global_email=$(git config --global --get user.email 2>/dev/null || true)
    pm_correct=$(correct_address)

    echo "check-git-identity: the correct address for this repo is: $pm_correct" >&2

    if [ -n "$pm_local_email" ] && [ "$pm_local_email" != "$pm_global_email" ]; then
      echo "check-git-identity: ROOT CAUSE — $repo_root/.git/config sets a repo-LOCAL user.email ('$pm_local_email') that differs from, and silently beats, the global user.email ('${pm_global_email:-<unset>}'). This repo-local override is the exact mechanism that produced this repo's historical defect (64 commits authored as 'Rally Test <rally@example.test>')." >&2
    fi

    echo "check-git-identity: fix — remove the repo-local override so the commit picks up the correct global identity:" >&2
    echo "  git config --local --unset user.email" >&2
    echo "  git config --local --unset user.name" >&2

    if [ -n "$pm_local_email" ] || [ -n "$pm_local_name" ]; then
      echo "check-git-identity: current local override in $repo_root/.git/config: user.email='${pm_local_email:-<unset>}' user.name='${pm_local_name:-<unset>}'" >&2
    fi
  fi

  return "$pm_status"
}

# --- --commits ---------------------------------------------------------
commits_mode() {
  if [ "$#" -eq 0 ]; then
    echo "check-git-identity: --commits requires at least one rev-list argument" >&2
    return 1
  fi

  # 0x1F (unit separator) as the field delimiter: author/committer name and
  # email can legitimately contain spaces, commas, etc., so a "safe" ASCII
  # delimiter like comma or tab is not actually safe. cut(1) takes it as a
  # literal -d argument, not a shell IFS split, so no bashism is needed.
  cm_us=$(printf '\37')

  cm_log_file=$(mktemp "${TMPDIR:-/tmp}/rally-git-identity-log.XXXXXX")
  trap 'rm -f "$cm_log_file"' EXIT

  if ! git log --format="%H${cm_us}%an${cm_us}%ae${cm_us}%cn${cm_us}%ce" --no-show-signature "$@" > "$cm_log_file" 2>/dev/null; then
    echo "check-git-identity: 'git log --format=... --no-show-signature $*' failed — cannot resolve the given rev-list arguments" >&2
    return 1
  fi

  cm_status=0
  cm_correct=""

  while IFS= read -r cm_line || [ -n "$cm_line" ]; do
    [ -n "$cm_line" ] || continue
    cm_full=$(printf '%s' "$cm_line" | cut -d "$cm_us" -f1)
    cm_an=$(printf '%s' "$cm_line" | cut -d "$cm_us" -f2)
    cm_ae=$(printf '%s' "$cm_line" | cut -d "$cm_us" -f3)
    cm_cn=$(printf '%s' "$cm_line" | cut -d "$cm_us" -f4)
    cm_ce=$(printf '%s' "$cm_line" | cut -d "$cm_us" -f5)
    cm_short=$(printf '%s' "$cm_full" | cut -c1-12)

    cm_bad=0
    if ! check_identity "$cm_an" "$cm_ae"; then
      echo "check-git-identity: REJECTED commit $cm_short ($cm_full) — author identity email='$cm_ae' name='$cm_an': $IDENT_REASON" >&2
      cm_bad=1
    fi
    if ! check_identity "$cm_cn" "$cm_ce"; then
      echo "check-git-identity: REJECTED commit $cm_short ($cm_full) — committer identity email='$cm_ce' name='$cm_cn': $IDENT_REASON" >&2
      cm_bad=1
    fi

    if [ "$cm_bad" != "0" ]; then
      cm_status=1
      if [ -z "$cm_correct" ]; then
        cm_correct=$(correct_address)
      fi
      echo "check-git-identity: commit $cm_short is ALREADY WRITTEN. History is NOT rewritten for it — do not filter-branch, filter-repo, or rebase -i across pushed history." >&2
      echo "check-git-identity: if $cm_short is NOT YET PUSHED, fix the identity and re-create the commit before pushing. The correct address is: $cm_correct" >&2
      echo "check-git-identity:   git config --local --unset user.email   # if a repo-local override is beating your global identity" >&2
      echo "check-git-identity:   git config --local --unset user.name" >&2
      echo "check-git-identity:   git commit --amend --reset-author      # ONLY if $cm_short is the TIP commit of your unpushed work" >&2
    fi
  done < "$cm_log_file"

  return "$cm_status"
}

# --- dispatch ---------------------------------------------------------
case "${1:-}" in
  --pending)
    shift
    pending_mode "$@"
    ;;
  --commits)
    shift
    commits_mode "$@"
    ;;
  *)
    usage
    ;;
esac
