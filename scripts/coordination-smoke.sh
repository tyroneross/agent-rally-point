#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# coordination-smoke.sh — end-to-end multi-AGENT coordination smoke against the
# real rally binary. Drives THREE distinct tool ids through one room and asserts
# the coordination layer behaves: cross-agent presence, file-ownership claims,
# before-write conflict blocking, and the handoff post→resolve lifecycle.
#
# rally tracks agents by tool-id string, so this single-process smoke exercises
# the exact mechanics regardless of vendor. The live CROSS-VENDOR run (Claude +
# a second Claude + Codex, separate processes) was verified by hand on
# 2026-06-12: 3 agents present, claims fileA/B/C to the right owners, both the
# second Claude and Codex blocked from claude_code:01's fileA (allow=false,
# severity=stop), handoff Claude→Codex posted and resolved (open_handoffs 0).
#
# Self-gating: SKIPs (exit 0) when no rally binary is found. Run:
#   RALLY_BIN=./target/debug/rally bash scripts/coordination-smoke.sh
set -euo pipefail

RALLY="${RALLY_BIN:-}"
if [ -z "$RALLY" ]; then
  if [ -x "./target/debug/rally" ]; then RALLY="./target/debug/rally"
  elif command -v rally >/dev/null 2>&1; then RALLY="rally"
  else echo "SKIP: no rally binary (set RALLY_BIN or build crates/rally-cli)"; exit 0; fi
fi

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

ROOM="$(mktemp -d)/coord-smoke"; mkdir -p "$ROOM"
trap 'rm -rf "$(dirname "$ROOM")"' EXIT
cd "$ROOM"
git init -q; git config user.email t@t.co; git config user.name t; git commit -q --allow-empty -m init
"$RALLY" init >/dev/null 2>&1 || true

# jq-free extractors over rally --json output.
_allow()   { python3 -c "import sys,json;print(json.load(sys.stdin)['data']['check'].get('allow'))"; }
_squads()  { python3 -c "import sys,json;print(','.join(sorted(s['tool'] for s in json.load(sys.stdin)['data']['room'].get('squads',[]))))"; }
_claims()  { python3 -c "import sys,json;print(';'.join(sorted(c['tool']+'='+c['scope'][0] for c in json.load(sys.stdin)['data']['room'].get('active_claims',[]))))"; }
_handoffs(){ python3 -c "import sys,json;print(len(json.load(sys.stdin)['data']['room'].get('open_handoffs',[])))"; }

# Agent A: enter + claim fileA
"$RALLY" enter --tool agent_a --json >/dev/null 2>&1
"$RALLY" say claim --tool agent_a --path "$ROOM/fileA.txt" --subject "A owns fileA" --json >/dev/null 2>&1

# Agent B: enter, see A, get blocked on A's file, allowed on its own, claim fileB, hand off to C
"$RALLY" enter --tool agent_b --json >/dev/null 2>&1
sq="$("$RALLY" room --json 2>/dev/null | _squads)"
[ "$sq" = "agent_a,agent_b" ] && ok "presence: B sees A" || bad "presence: B sees A" "got [$sq]"
a="$("$RALLY" check before-write --tool agent_b --path "$ROOM/fileA.txt" --json 2>/dev/null | _allow)"
[ "$a" = "False" ] && ok "deconflict: B blocked from A's fileA (allow=false)" || bad "deconflict: B blocked from A's fileA" "allow=$a"
"$RALLY" say claim --tool agent_b --path "$ROOM/fileB.txt" --subject "B owns fileB" --json >/dev/null 2>&1
a="$("$RALLY" check before-write --tool agent_b --path "$ROOM/fileB.txt" --json 2>/dev/null | _allow)"
[ "$a" = "True" ] && ok "own-file: B allowed on its own fileB (allow=true)" || bad "own-file: B allowed on fileB" "allow=$a"
"$RALLY" say handoff --tool agent_b --to agent_c --subject "agent_c please take fileC.txt" --json >/dev/null 2>&1
h="$("$RALLY" room --json 2>/dev/null | _handoffs)"
[ "$h" = "1" ] && ok "handoff: B→C posted (open_handoffs=1)" || bad "handoff posted" "open_handoffs=$h"

# Agent C: enter, see A+B, blocked on A's file, claim fileC, resolve the handoff
"$RALLY" enter --tool agent_c --json >/dev/null 2>&1
a="$("$RALLY" check before-write --tool agent_c --path "$ROOM/fileA.txt" --json 2>/dev/null | _allow)"
[ "$a" = "False" ] && ok "deconflict: C blocked from A's fileA (allow=false)" || bad "deconflict: C blocked from fileA" "allow=$a"
"$RALLY" say claim --tool agent_c --path "$ROOM/fileC.txt" --subject "C owns fileC" --json >/dev/null 2>&1
ref="$("$RALLY" room --json 2>/dev/null | python3 -c "import sys,json;hs=json.load(sys.stdin)['data']['room'].get('open_handoffs',[]);print(hs[0].get('event_id') or hs[0].get('id') or '' if hs else '')")"
[ -n "$ref" ] && "$RALLY" say resolve --tool agent_c --ref "$ref" --subject "C took fileC" --json >/dev/null 2>&1 || true

# Final assertions
final="$("$RALLY" room --json 2>/dev/null)"
sq="$(printf '%s' "$final" | _squads)"
cl="$(printf '%s' "$final" | _claims)"
h="$(printf '%s' "$final" | _handoffs)"
[ "$sq" = "agent_a,agent_b,agent_c" ] && ok "final: all 3 agents present" || bad "final presence" "got [$sq]"
[ "$cl" = "agent_a=file:fileA.txt;agent_b=file:fileB.txt;agent_c=file:fileC.txt" ] && ok "final: 3 claims to correct owners" || bad "final claims" "got [$cl]"
[ "$h" = "0" ] && ok "final: handoff resolved (open_handoffs=0)" || bad "handoff resolved" "open_handoffs=$h"

echo ""; echo "Passed: $PASS  Failed: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
