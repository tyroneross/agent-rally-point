#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# test_sanitizer_block_parity.sh — the ARP-004 sanitizer is duplicated in two
# node renderers inside rally-coordination-hook.sh. This asserts the copies stay
# byte-identical, and that there are exactly two of them.
#
# WHY THIS EXISTS. The sanitizer (ident/prose/line + the trust preamble) is the
# control that keeps peer-authored ledger prose out of a high-trust model
# channel. It is written twice because each renderer is a separate `node -e`
# heredoc with no shared module to import.
#
# The only thing holding the copies together was a comment:
# "KEEP THIS BLOCK BYTE-IDENTICAL". That is not a control. During the ARP-004
# work a mutation check PASSED SPURIOUSLY because the harness reverted only ONE
# of the two blocks — a partial revert of a duplicated defence looks exactly
# like a passing test. If the two copies can drift, one renderer can lose the
# sanitizer while every existing test keeps reporting green, because each test
# exercises one path at a time.
#
# Marker-based extraction, not line numbers: line numbers move on every edit to
# the file, and a test that needs updating whenever anything nearby changes gets
# updated carelessly.

# No `set -u`: this must run on stock macOS /bin/bash 3.2, where an empty array
# expansion trips unbound-variable. An earlier draft used `mapfile` (bash 4+),
# which failed on 3.2 AND still exited 0 — a vacuously passing test, the exact
# failure this file exists to prevent. The trailing `[ "$fail" -eq 0 ]` is the
# exit guard, so a harness error cannot report success.
set -o pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
HOOK="$REPO_ROOT/hooks/rally-coordination-hook.sh"

BEGIN_MARK='// ---- UNTRUSTED-DATA BOUNDARY (ARP-004) ---'
END_MARK='// ---- end UNTRUSTED-DATA BOUNDARY ---'

pass=0
fail=0
ok()  { printf 'ok   %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf 'FAIL %s\n     %s\n' "$1" "${2:-}" >&2; fail=$((fail + 1)); }

# ---------------------------------------------------------------------------
T="the hook file exists and is readable"
if [ -r "$HOOK" ]; then ok "$T"; else bad "$T" "missing: $HOOK"; fi

# Collect the 1-based line numbers of every begin/end marker.
# bash 3.2 compatible (no mapfile / readarray).
BEGINS=()
ENDS=()
while IFS= read -r n; do [ -n "$n" ] && BEGINS+=("$n"); done < <(grep -n -F "$BEGIN_MARK" "$HOOK" | cut -d: -f1)
while IFS= read -r n; do [ -n "$n" ] && ENDS+=("$n");   done < <(grep -n -F "$END_MARK"   "$HOOK" | cut -d: -f1)

n_begin=${#BEGINS[@]}
n_end=${#ENDS[@]}

# ---------------------------------------------------------------------------
T="exactly two sanitizer blocks are present"
if [ "$n_begin" -eq 2 ] && [ "$n_end" -eq 2 ]; then
  ok "$T"
else
  bad "$T" "found $n_begin begin and $n_end end markers — expected 2 and 2.
     If a renderer was ADDED, it must carry the sanitizer and this count must rise deliberately.
     If one was REMOVED, confirm its renderer no longer emits peer-authored prose at all."
fi

# ---------------------------------------------------------------------------
T="the two sanitizer blocks are byte-identical"
if [ "$n_begin" -eq 2 ] && [ "$n_end" -eq 2 ]; then
  hash_a="$(sed -n "${BEGINS[0]},${ENDS[0]}p" "$HOOK" | shasum -a 256 | cut -d' ' -f1)"
  hash_b="$(sed -n "${BEGINS[1]},${ENDS[1]}p" "$HOOK" | shasum -a 256 | cut -d' ' -f1)"
  if [ "$hash_a" = "$hash_b" ]; then
    ok "$T ($hash_a)"
  else
    bad "$T" "block at line ${BEGINS[0]} = $hash_a
     block at line ${BEGINS[1]} = $hash_b
     The ARP-004 sanitizer has drifted between the two renderers. One model-facing
     path is now sanitized differently from the other. Diff them:
       diff <(sed -n '${BEGINS[0]},${ENDS[0]}p' hooks/rally-coordination-hook.sh) \\
            <(sed -n '${BEGINS[1]},${ENDS[1]}p' hooks/rally-coordination-hook.sh)"
  fi
else
  bad "$T" "skipped — marker count wrong (see above)"
fi

# ---------------------------------------------------------------------------
# Guard the contents, not just the equality. Two identical blocks that both lost
# the sanitizer would pass the hash check.
T="each block still defines ident(), prose(), and line()"
if [ "$n_begin" -eq 2 ]; then
  missing=""
  for i in 0 1; do
    body="$(sed -n "${BEGINS[$i]},${ENDS[$i]}p" "$HOOK")"
    # Match the open-paren too. A bare "function prose" substring also matches
    # "function proseX", so a rename in BOTH blocks kept the hashes equal AND
    # satisfied this check — a mutation that survived the first draft of this
    # test. The sanitizer would have been gone with every assertion green.
    for fn in "function ident(" "function prose(" "function line("; do
      printf '%s' "$body" | grep -qF "$fn" || missing="$missing block$((i + 1)):${fn};"
    done
  done
  if [ -z "$missing" ]; then ok "$T"; else bad "$T" "missing: $missing"; fi
else
  bad "$T" "skipped — marker count wrong"
fi

# ---------------------------------------------------------------------------
T="the trust preamble is defined and is not chosen by sniffing untrusted content"
# SEC-004: the preamble must be applied from provenance, never by testing the
# assembled message for the preamble's own marker — a peer could then suppress it.
if grep -qF 'UNTRUSTED LEDGER DATA FOLLOWS' "$HOOK"; then
  if grep -qE 'includes\("UNTRUSTED LEDGER DATA FOLLOWS"\)' "$HOOK"; then
    bad "$T" "the preamble decision reads the untrusted content for its own marker (SEC-004 regression)"
  else
    ok "$T"
  fi
else
  bad "$T" "UNTRUSTED LEDGER DATA FOLLOWS preamble not found"
fi

printf '\nPassed: %s / Failed: %s\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
