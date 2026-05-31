#!/usr/bin/env bash
# Rally-workflows scale + reliability harness v2.
# Warm room (seeded first), tier-declared fleet, per-command outcome capture.
# Distinguishes graceful degradation (ok:false, fail-loud) from silent loss
# (ok:true but fact absent from ledger = corruption). Isolated room (temp HOME).
set -uo pipefail
RALLY="${RALLY_BIN:?set RALLY_BIN}"

# Returns the ok field of a rally --json call, and records the event_id if present.
call_ok() { python3 -c 'import sys,json
try: d=json.load(sys.stdin)
except: print("PARSE"); sys.exit()
print("T" if d.get("ok") else "F")'; }

run_scale() {
  local N="$1"
  local TMP; TMP="$(mktemp -d)"; export HOME="$TMP"
  local REPO="$TMP/repo"; mkdir -p "$REPO/src"; ( cd "$REPO" && git init -q ); cd "$REPO"
  local R="$TMP/res"; mkdir -p "$R"
  # WARM the room first (real scenario: agents join an existing room, not race genesis)
  "$RALLY" enter --tool lead:seed --session-id seed --json >/dev/null 2>&1
  local SHARED="src/contended.rs"
  local t0; t0=$(python3 -c 'import time;print(time.time())')

  agent() {
    local i="$1" tool tier
    case $(( i % 3 )) in
      0) tool="codex:$i";              tier="executing" ;;
      1) tool="claude_code:sonnet-$i"; tier="executing" ;;
      2) tool="claude_code:haiku-$i";  tier="fast" ;;
    esac
    local path; if (( i % 2 == 0 )); then path="$SHARED"; else path="src/uniq_$i.rs"; fi
    local e c cl ar tf hf
    e=$("$RALLY"  enter --tool "$tool" --session-id "$tool-s" --json 2>/dev/null | call_ok)
    "$RALLY" next --tool "$tool" --json >/dev/null 2>&1
    c=$("$RALLY"  check before-write --tool "$tool" --path "$path" --strict --json 2>/dev/null \
        | python3 -c 'import sys,json
try: print("allow" if json.load(sys.stdin)["data"]["check"]["allow"] else "deny")
except: print("ERR")')
    cl=$("$RALLY" say claim    --tool "$tool" --path "$path" --subject "claim $tool" --json 2>/dev/null | call_ok)
    ar=$("$RALLY" say artifact --tool "$tool" --subject "art $i" --uri "f:$path" --evidence "work $i" --json 2>/dev/null | call_ok)
    tf=$("$RALLY" check tier-fit --tool "$tool" --role implementer --proposed-tier "$tier" --json 2>/dev/null | call_ok)
    hf=$("$RALLY" say handoff  --tool "$tool" --target "peer$(( (i+1)%N ))" --subject "h $i" --json 2>/dev/null | call_ok)
    echo "$tool|$e|$c|$cl|$ar|$tf|$hf" > "$R/a_$i"
  }

  local pids=()
  for ((i=0;i<N;i++)); do agent "$i" & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p"; done
  local t1; t1=$(python3 -c 'import time;print(time.time())')
  local ledger; ledger="$(ls "$REPO"/.rally/log/*.jsonl 2>/dev/null | head -1)"

  python3 - "$N" "$R" "$ledger" "$t0" "$t1" <<'PY'
import sys,json,glob,os
N=int(sys.argv[1]); R=sys.argv[2]; ledger=sys.argv[3]; t0=float(sys.argv[4]); t1=float(sys.argv[5])
rows=[open(f).read().strip().split("|") for f in glob.glob(f"{R}/a_*")]
# row = tool,enter,check,claim,artifact,tierfit,handoff
tools=[r[0] for r in rows]
def okc(idx): return sum(1 for r in rows if r[idx]=="T")
enter_ok,claim_ok,art_ok,tf_ok,hand_ok = okc(1),okc(3),okc(4),okc(5),okc(6)
allow=sum(1 for r in rows if r[2]=="allow"); deny=sum(1 for r in rows if r[2]=="deny"); cerr=sum(1 for r in rows if r[2]=="ERR")
# ledger truth
facts=[]
if ledger and os.path.exists(ledger):
    for line in open(ledger):
        try: facts.append(json.loads(line)["payload"])
        except: pass
def K(k,tool=None):
    return [f for f in facts if f.get("kind")==k and (tool is None or f.get("tool")==tool)]
claim_tools_ok=set(r[0] for r in rows if r[3]=="T")
claim_in_ledger=set(f.get("tool") for f in K("claim"))
# SILENT LOSS = said ok:true but no claim fact in ledger for that tool
silent_loss=sorted(claim_tools_ok - claim_in_ledger)
out={
 "scale":N, "wall_s":round(t1-t0,2),
 "enter_ok":f"{enter_ok}/{N}", "claim_ok":f"{claim_ok}/{N}", "artifact_ok":f"{art_ok}/{N}",
 "tierfit_ok":f"{tf_ok}/{N}", "handoff_ok":f"{hand_ok}/{N}",
 "dup_ids": len(tools)-len(set(tools)),
 "check_allow":allow, "check_deny":deny, "check_err":cerr,
 "ledger_claims":len(K("claim")), "ledger_artifacts":len(K("artifact")), "ledger_handoffs":len(K("handoff")),
 "SILENT_LOSS": len(silent_loss),     # ok:true but absent from ledger — must be 0
}
print(json.dumps(out))
PY
  cd /; rm -rf "$TMP"
}
echo "=== rally-workflows scale + reliability v2 (warm room) ==="
for N in 4 8 16 32; do run_scale "$N"; done
