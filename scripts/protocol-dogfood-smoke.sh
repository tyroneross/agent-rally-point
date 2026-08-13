#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Phase 5 handoff-lifecycle / targeted-ACK dogfood smoke test.
#
# Proves the north-star reliability invariants (docs/PROTOCOL-NORTH-STAR.md
# "Reliability Semantics") against the CURRENT rally CLI, using only commands
# that exist today:
#
#   1. delivered != acked  — a targeted handoff is actionable for its target
#      but is NOT auto-acknowledged on delivery.
#   2. ACK cites the exact event — the target's ACK/resolve carries the precise
#      ref_event_id of the handoff and is authored by the target session
#      (from_session_id), not "some Claude".
#   3. targeting              — the handoff surfaces for the intended target and
#      not for an unrelated session ("which Claude received it?").
#   4. acked transition       — after the ACK the handoff leaves the target's
#      actionable queue (delivered -> acked).
#
# Runs in a disposable temp room (its own git repo), so it never touches the
# live ledger. Deterministic; no clock/random.
#
# Usage:  RALLY=/path/to/rally scripts/protocol-dogfood-smoke.sh
# Default RALLY: the repo's debug build (target/debug/rally).

set -euo pipefail

# --- resolve the rally binary (repo-local debug build by default) ------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1090,SC1091
source "$SCRIPT_DIR/disposable-repo-guard.sh"
RALLY="${RALLY:-$REPO_ROOT/target/debug/rally}"
if [ ! -x "$RALLY" ]; then
  echo "FATAL: rally binary not found/executable at: $RALLY" >&2
  echo "       build it (cargo build -p rally-cli) or set RALLY=/path/to/rally" >&2
  exit 2
fi
if [[ "$RALLY" == */* ]]; then
  RALLY="$(cd "$(dirname "$RALLY")" && pwd -P)/$(basename "$RALLY")"
else
  RALLY="$(command -v "$RALLY")"
fi

# --- isolated room -----------------------------------------------------------
SCRATCH_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rally-dogfood.XXXXXX")"
WORK="$SCRATCH_ROOT/repo"
mkdir -p "$WORK"
cleanup() { rm -rf "$SCRATCH_ROOT"; }
trap cleanup EXIT
cd "$WORK"
git init -q
git commit -q --allow-empty -m "dogfood room init"
rally_assert_disposable_repo "$WORK" "$SCRATCH_ROOT" "$REPO_ROOT"

# Two distinct host sessions + one bystander, mirroring the Claude/Codex split.
A="codex:dogfood-author"     # poses the handoff (Codex implementer)
B="claude_code:dogfood-tgt"  # the intended target (Claude reviewer)
C="claude_code:dogfood-other" # a different Claude — must NOT receive B's handoff

# Extract a dotted path from a rally --json payload on stdin.
val() { python3 -c "import sys,json
d=json.load(sys.stdin)
try:
    print(eval(\"d$1\"))
except Exception:
    print('')"; }

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1" >&2; exit 1; }

# --- enter the three sessions ------------------------------------------------
"$RALLY" enter --tool "$A" --json >/dev/null
"$RALLY" enter --tool "$B" --json >/dev/null
"$RALLY" enter --tool "$C" --json >/dev/null

# --- A posts a targeted handoff to B ----------------------------------------
HID="$("$RALLY" say handoff --tool "$A" --target "$B" \
  --subject "review claim-authority lane" --json | val "['data']['say']['fact']['event_id']")"
[ -n "$HID" ] || fail "handoff post returned no event_id"
pass "handoff posted $A -> $B (event_id=$HID)"

# --- (1) delivered: B's actionable next surfaces the handoff -----------------
NEXT_B="$("$RALLY" next --tool "$B" --json | val "['data']['next'].get('fact',{}) or {}")"
NEXT_B_ID="$("$RALLY" next --tool "$B" --json | val "['data']['next'].get('fact',{}).get('event_id','') if d['data']['next'].get('fact') else ''")"
[ "$NEXT_B_ID" = "$HID" ] || fail "delivered: B's 'next' should surface $HID, got '$NEXT_B_ID'"
pass "delivered: B's 'rally next' surfaces the handoff (actionable, not yet acked)"

# --- (3) targeting: C must NOT be handed B's handoff ------------------------
NEXT_C_ID="$("$RALLY" next --tool "$C" --json | val "['data']['next'].get('fact',{}).get('event_id','') if d['data']['next'].get('fact') else ''")"
[ "$NEXT_C_ID" != "$HID" ] || fail "targeting leak: bystander $C was handed B's handoff $HID"
pass "targeting: $C does NOT receive B's handoff ('which Claude received it')"

# --- (2) B ACKs/resolves with the EXACT ref + its own from_session_id --------
ACK_JSON="$("$RALLY" say resolve --tool "$B" --ref "$HID" \
  --subject "ACK+resolve: reviewed" \
  --evidence "from_session_id=$B" --evidence "ref_event_id=$HID" --json)"
ACK_REF="$(printf '%s' "$ACK_JSON" | val "['data']['say']['fact']['ref']")"
ACK_TOOL="$(printf '%s' "$ACK_JSON" | val "['data']['say']['fact']['tool']")"
ACK_EVID="$(printf '%s' "$ACK_JSON" | val "['data']['say']['fact']['evidence']")"
[ "$ACK_REF" = "$HID" ] || fail "ACK must cite exact ref_event_id $HID, cited '$ACK_REF'"
[ "$ACK_TOOL" = "$B" ] || fail "ACK must be authored by target $B, authored by '$ACK_TOOL'"
case "$ACK_EVID" in
  *"from_session_id=$B"*) : ;;
  *) fail "ACK evidence must carry from_session_id=$B, got: $ACK_EVID" ;;
esac
pass "ack-proof: B's ACK cites exact ref_event_id=$HID, authored by $B, with from_session_id"

# --- (4) acked transition: handoff leaves B's actionable queue --------------
NEXT_B2_ID="$("$RALLY" next --tool "$B" --json | val "['data']['next'].get('fact',{}).get('event_id','') if d['data']['next'].get('fact') else ''")"
[ "$NEXT_B2_ID" != "$HID" ] || fail "acked!=resolved: $HID still actionable for B after ACK"
pass "acked: after B's ACK the handoff is no longer in B's actionable queue (delivered->acked)"

echo
echo "ALL PASS — handoff lifecycle proven: delivered != acked; ACK cites exact"
echo "ref_event_id + from_session_id; targeting holds; acked transition observed."
