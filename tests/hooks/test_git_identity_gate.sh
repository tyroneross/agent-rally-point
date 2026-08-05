#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Adversarial control for scripts/check-git-identity.sh (docs/ROOT-CAUSE-REGISTER.md
# defect class: 64 commits landed authored `Rally Test <rally@example.test>`
# because a test fixture wrote a repo-LOCAL `user.email` override into the
# REAL `.git/config`, silently beating the correct global identity).
#
# A gate that only asserts its own happy path certifies nothing. Every case
# below proves a REJECTION (or, for the trailer carve-out and the three
# allowlisted addresses, proves a legitimate identity still passes silently).
# The load-bearing case is the fabricated-identity rejection: it asserts the
# offending address, the correct address, and the exact remediation command
# all appear in stderr, not just a non-zero exit code.
#
# Fixtures are throwaway `git init` repos under a mktemp dir. Per-invocation
# identity is set with GIT_AUTHOR_*/GIT_COMMITTER_* env vars or `git -c
# user.name=... -c user.email=...` — NEVER `git config user.email` against
# the fixture. Reproducing the repo-local-config defect in the test for the
# gate that exists to catch it would certify nothing.
#
# Run: tests/hooks/test_git_identity_gate.sh
# Exits 0 on full pass, 1 on any failure. Prints "Passed: N / Failed: M".

set -u
# (deliberately not -e: we assert exit codes throughout)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
CHECK="$REPO_ROOT/scripts/check-git-identity.sh"
ALLOWLIST="$REPO_ROOT/config/git-identity-allowlist.txt"

if [ ! -x "$CHECK" ]; then
  echo "FAIL: scripts/check-git-identity.sh missing or not executable at $CHECK"
  exit 1
fi
if [ ! -f "$ALLOWLIST" ]; then
  echo "FAIL: config/git-identity-allowlist.txt missing at $ALLOWLIST"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()

note() { printf '  %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

# The three addresses config/git-identity-allowlist.txt actually carries.
# Kept in sync manually — if the shipped file changes, update these too.
ALLOW_1="46267523+tyroneross@users.noreply.github.com"
ALLOW_2="tyrone.ross@gmail.com"
ALLOW_3="technique@gmail.com"

# --- fixture root: must be under ${TMPDIR:-/tmp}, resolved (macOS aliases
# /tmp -> /private/tmp), asserted BEFORE the first git call. -------------
TMP_BASE="${TMPDIR:-/tmp}"
mkdir -p "$TMP_BASE" 2>/dev/null || true
TMP_BASE_REAL="$(cd "$TMP_BASE" 2>/dev/null && pwd -P)"
if [ -z "$TMP_BASE_REAL" ]; then
  echo "FAIL: cannot resolve TMPDIR/base tmp dir '$TMP_BASE'"
  exit 1
fi
FIXTURE="$(mktemp -d "${TMP_BASE%/}/rally-git-identity.XXXXXX")"
FIXTURE_REAL="$(cd "$FIXTURE" && pwd -P)"
case "$FIXTURE_REAL" in
  "$TMP_BASE_REAL"/*) : ;;
  *)
    echo "FAIL: fixture root '$FIXTURE_REAL' is not under '$TMP_BASE_REAL' — refusing to run git commands here"
    rm -rf "$FIXTURE" 2>/dev/null || true
    exit 1
    ;;
esac

cleanup() { rm -rf "$FIXTURE" 2>/dev/null || true; }
trap cleanup EXIT

git -C "$FIXTURE" init -q -b main

# Give the fixture its own copy of the REAL shipped allowlist at the same
# repo-relative path (config/git-identity-allowlist.txt), so tests that do
# NOT override $RALLY_IDENTITY_ALLOWLIST exercise the script's default
# resolution order against the actual file this repo ships, not a stand-in.
mkdir -p "$FIXTURE/config"
cp "$ALLOWLIST" "$FIXTURE/config/git-identity-allowlist.txt"

# mk_commit AUTHOR_NAME AUTHOR_EMAIL COMMITTER_NAME COMMITTER_EMAIL MSG
# Creates one commit with the given identities (via env vars, never via
# `git config`) and echoes its full SHA.
COMMIT_SEQ=0
mk_commit() {
  local an="$1" ae="$2" cn="$3" ce="$4" msg="$5"
  COMMIT_SEQ=$((COMMIT_SEQ + 1))
  printf 'seq=%s\n' "$COMMIT_SEQ" >> "$FIXTURE/f.txt"
  git -C "$FIXTURE" add -A >/dev/null
  GIT_AUTHOR_NAME="$an" GIT_AUTHOR_EMAIL="$ae" \
    GIT_COMMITTER_NAME="$cn" GIT_COMMITTER_EMAIL="$ce" \
    git -C "$FIXTURE" commit -q -m "$msg"
  git -C "$FIXTURE" rev-parse HEAD
}

# run_pending AUTHOR_NAME AUTHOR_EMAIL COMMITTER_NAME COMMITTER_EMAIL
# [EXTRA_ENV...]
# Invokes `check-git-identity.sh --pending` from inside the fixture with the
# given identity, WITHOUT ever writing it to git config. Captures combined
# stdout+stderr and the exit code (via globals OUT/RC) so callers can assert
# both the silence-on-success contract and the message contents on reject.
OUT=""
RC=0
run_pending() {
  local an="$1" ae="$2" cn="$3" ce="$4"
  shift 4
  OUT=$( (cd "$FIXTURE" && env -u RALLY_IDENTITY_ALLOWLIST \
            GIT_AUTHOR_NAME="$an" GIT_AUTHOR_EMAIL="$ae" \
            GIT_COMMITTER_NAME="$cn" GIT_COMMITTER_EMAIL="$ce" \
            "$@" "$CHECK" --pending) 2>&1 )
  RC=$?
}

# run_commits REV_LIST_ARG...
# Invokes `check-git-identity.sh --commits <args...>` from inside the
# fixture. Captures combined stdout+stderr and the exit code into OUT/RC.
run_commits() {
  OUT=$( (cd "$FIXTURE" && env -u RALLY_IDENTITY_ALLOWLIST "$CHECK" --commits "$@") 2>&1 )
  RC=$?
}

# ===========================================================================
# 1. --pending accepts each of the three allowlisted addresses (silent, exit 0)
# ===========================================================================
for addr in "$ALLOW_1" "$ALLOW_2" "$ALLOW_3"; do
  T="pending: accepts allowlisted address ($addr)"
  run_pending "Allowlisted Human" "$addr" "Allowlisted Human" "$addr"
  if [ "$RC" = "0" ] && [ -z "$OUT" ]; then
    ok "$T"
  else
    bad "$T" "rc=$RC out=[$OUT]"
  fi
done

# ===========================================================================
# 2. LOAD-BEARING: --pending REJECTS a fabricated identity. Must name the
# offending address, the correct address, and a `git config --local --unset`
# remediation command.
# ===========================================================================
T="pending: REJECTS fabricated identity rally@example.test (author) — names offending address, correct address, and unset command"
run_pending "Rally Test" "rally@example.test" "Allowlisted Human" "$ALLOW_1"
if [ "$RC" != "0" ] \
   && printf '%s' "$OUT" | grep -q "rally@example.test" \
   && printf '%s' "$OUT" | grep -q "git config --local --unset" \
   && printf '%s' "$OUT" | grep -qF -e "$ALLOW_1" -e "$ALLOW_2" -e "$ALLOW_3"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 3. --pending REJECTS a hostname-fallback (.local) address and names the
# .local pattern.
# ===========================================================================
T="pending: REJECTS jason@MacBook-Air-110.local (hostname fallback), names .local"
run_pending "jason" "jason@MacBook-Air-110.local" "Allowlisted Human" "$ALLOW_1"
if [ "$RC" != "0" ] \
   && printf '%s' "$OUT" | grep -q "jason@MacBook-Air-110.local" \
   && printf '%s' "$OUT" | grep -q '\.local'; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 4. --pending REJECTS noreply@anthropic.com as AUTHOR.
# ===========================================================================
T="pending: REJECTS noreply@anthropic.com as AUTHOR"
run_pending "Claude" "noreply@anthropic.com" "Allowlisted Human" "$ALLOW_1"
if [ "$RC" != "0" ] \
   && printf '%s' "$OUT" | grep -q "noreply@anthropic.com" \
   && printf '%s' "$OUT" | grep -qi "author"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 5. TRAILER CARVE-OUT: a commit whose author/committer are allowlisted but
# whose MESSAGE BODY contains `Co-Authored-By: ... <noreply@anthropic.com>`
# must PASS --commits. This is CONTRIBUTING.md's required AI-attribution
# convention; if this fails, the gate breaks it.
# ===========================================================================
T="commits: trailer carve-out — allowlisted author/committer + Co-Authored-By noreply@anthropic.com in body PASSES"
trailer_msg=$(printf 'feat: trailer carve-out fixture\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>\n')
sha_trailer=$(mk_commit "Allowlisted Human" "$ALLOW_1" "Allowlisted Human" "$ALLOW_1" "$trailer_msg")
run_commits "$sha_trailer"
if [ "$RC" = "0" ] && [ -z "$OUT" ]; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 6. --commits REJECTS a range containing one bad-identity commit among good
# ones, and names the offending SHA.
# ===========================================================================
c1=$(mk_commit "Allowlisted Human" "$ALLOW_1" "Allowlisted Human" "$ALLOW_1" "good commit 1")
c2=$(mk_commit "Rally Test" "rally@example.test" "Rally Test" "rally@example.test" "bad commit")
c3=$(mk_commit "Allowlisted Human" "$ALLOW_2" "Allowlisted Human" "$ALLOW_2" "good commit 2")
c2_short=$(printf '%s' "$c2" | cut -c1-12)

T="commits: REJECTS a range with one bad commit among good ones, names the offending SHA"
run_commits "$c1" "$c2" "$c3"
if [ "$RC" != "0" ] \
   && printf '%s' "$OUT" | grep -q "$c2_short" \
   && printf '%s' "$OUT" | grep -qi "REJECTED commit"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT] c2_short=$c2_short"
fi

# ===========================================================================
# 7. Committer-only contamination: author allowlisted, committer
# rally@example.test -> REJECT. (A check that reads only the author misses
# half the surface.)
# ===========================================================================
T="pending: committer-only contamination (allowlisted author, bad committer) REJECTS"
run_pending "Allowlisted Human" "$ALLOW_1" "Rally Test" "rally@example.test"
if [ "$RC" != "0" ] \
   && printf '%s' "$OUT" | grep -qi "committer identity" \
   && printf '%s' "$OUT" | grep -q "rally@example.test"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 8. MUTATION CHECK: point the script at an allowlist file containing ONLY a
# decoy address. A normally-allowlisted address must now be REJECTED — this
# proves the allowlist is actually consulted, not that the check happens to
# pass for some unrelated reason. Decoy domain matches none of the suspect
# patterns (not example./*.invalid/*.local/localhost), so a rejection here
# can only come from the allowlist miss.
# ===========================================================================
DECOY_ALLOWLIST="$FIXTURE/decoy-allowlist.txt"
printf '# decoy-only allowlist for the mutation check\ndecoy@decoy-corp-not-real.test\n' > "$DECOY_ALLOWLIST"

T="mutation: decoy-only allowlist rejects a normally-allowlisted address ($ALLOW_2)"
OUT=$( (cd "$FIXTURE" && \
          GIT_AUTHOR_NAME="Allowlisted Human" GIT_AUTHOR_EMAIL="$ALLOW_2" \
          GIT_COMMITTER_NAME="Allowlisted Human" GIT_COMMITTER_EMAIL="$ALLOW_2" \
          RALLY_IDENTITY_ALLOWLIST="$DECOY_ALLOWLIST" \
          "$CHECK" --pending) 2>&1 )
RC=$?
if [ "$RC" != "0" ] && printf '%s' "$OUT" | grep -q "$ALLOW_2"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
fi

# ===========================================================================
# 9. Missing allowlist file (RALLY_IDENTITY_ALLOWLIST pointed at a
# nonexistent path) -> exit non-zero, fail closed.
# ===========================================================================
T="fail-closed: RALLY_IDENTITY_ALLOWLIST pointed at a nonexistent path exits non-zero"
MISSING_ALLOWLIST="$FIXTURE/does-not-exist-allowlist.txt"
OUT=$( (cd "$FIXTURE" && \
          GIT_AUTHOR_NAME="Allowlisted Human" GIT_AUTHOR_EMAIL="$ALLOW_1" \
          GIT_COMMITTER_NAME="Allowlisted Human" GIT_COMMITTER_EMAIL="$ALLOW_1" \
          RALLY_IDENTITY_ALLOWLIST="$MISSING_ALLOWLIST" \
          "$CHECK" --pending) 2>&1 )
RC=$?
if [ "$RC" != "0" ] && printf '%s' "$OUT" | grep -qi "FAIL CLOSED"; then
  ok "$T"
else
  bad "$T" "rc=$RC out=[$OUT]"
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
