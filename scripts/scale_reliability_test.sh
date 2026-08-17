#!/usr/bin/env bash
# Small-team reliability gate. It models persistent local agent sessions in one
# repo after the optional daemon is ready. Every reported success must exist
# exactly once in the canonical ledger; shared-path claim conflicts are the
# only accepted operation failures.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
# shellcheck disable=SC1090,SC1091
source "$SCRIPT_DIR/disposable-repo-guard.sh"

MODE="both"
SCALES="2,4,6"
MAX_WALL_S="0"
SELF_TEST=0
INTERNAL_MUTANT="${RALLY_SCALE_MUTANT:-}"

usage() {
  echo "usage: RALLY_BIN=/path/to/rally $0 [--mode direct|daemon|both] [--scales 2,4,6] [--max-wall-s seconds] [--self-test]" >&2
}

while (($#)); do
  case "$1" in
    --mode) MODE="${2:-}"; shift 2 ;;
    --scales) SCALES="${2:-}"; shift 2 ;;
    --max-wall-s) MAX_WALL_S="${2:-}"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 64 ;;
  esac
done

case "$MODE" in direct|daemon|both) ;; *) usage; exit 64 ;; esac
if ! [[ "$MAX_WALL_S" =~ ^([0-9]+([.][0-9]+)?|[.][0-9]+)$ ]]; then
  echo "GATE_FAIL max wall: --max-wall-s must be a non-negative number" >&2
  exit 64
fi
RALLY="${RALLY_BIN:-}"
if [[ -z "$RALLY" || ! -x "$RALLY" ]]; then
  echo "GATE_FAIL binary: RALLY_BIN must name an executable rally binary" >&2
  exit 64
fi
if [[ "$RALLY" == */* ]]; then
  RALLY="$(cd "$(dirname "$RALLY")" && pwd -P)/$(basename "$RALLY")"
else
  RALLY="$(command -v "$RALLY")"
fi

run_case() {
  local mode="$1" n="$2" tmp repo results daemon_pid=""
  tmp="$(mktemp -d)" || return 1
  repo="$tmp/repo"
  results="$tmp/results"
  mkdir -p "$repo/src" "$results" "$tmp/home"
  git -C "$repo" init -q

  capture() {
    local index="$1" op="$2"
    shift 2
    env -u GITHUB_ACTIONS -u GITHUB_RUN_ID "$RALLY" "$@" >"$results/$index.$op.json" 2>"$results/$index.$op.err"
    printf '%s\n' "$?" >"$results/$index.$op.rc"
  }

  if [[ "$mode" == "daemon" ]]; then
    (
      cd "$repo" || exit 1
      rally_assert_disposable_repo "$repo" "$tmp" "$SOURCE_ROOT" || exit 70
      HOME="$tmp/home" RALLY_SESSION_ID=seed env -u GITHUB_ACTIONS -u GITHUB_RUN_ID "$RALLY" enter --tool lead:seed --json --timeout-ms 60000
    ) >"$results/seed.json" 2>"$results/seed.err"
    if [[ $? -ne 0 ]]; then
      echo "GATE_FAIL mode=$mode scale=$n daemon seed failed" >&2
      return 1
    fi
    (
      cd "$repo" || exit 1
      rally_assert_disposable_repo "$repo" "$tmp" "$SOURCE_ROOT" || exit 70
      HOME="$tmp/home" env -u GITHUB_ACTIONS -u GITHUB_RUN_ID "$RALLY" daemon serve --idle-exit-secs 180
    ) >"$results/daemon.out" 2>"$results/daemon.err" &
    daemon_pid=$!
    local ready=0
    for _ in {1..300}; do
      if (
        cd "$repo" || exit 1
        HOME="$tmp/home" env -u GITHUB_ACTIONS -u GITHUB_RUN_ID "$RALLY" daemon status --json
      ) >"$results/status.json" 2>"$results/status.err" &&
        python3 - "$results/status.json" <<'PY'
import json,sys
try:
    data=json.load(open(sys.argv[1]))
    raise SystemExit(0 if data.get("ok") and data.get("data",{}).get("daemon",{}).get("live") is True else 1)
except Exception:
    raise SystemExit(1)
PY
      then
        ready=1
        break
      fi
      sleep 0.05
    done
    if [[ $ready -ne 1 ]]; then
      echo "GATE_FAIL mode=$mode scale=$n daemon did not become ready" >&2
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
      return 1
    fi
  fi

  agent() {
    local i="$1" tool path tier
    tool="codex:scale-$i"
    if ((i % 3 == 2)); then tier="fast"; else tier="executing"; fi
    if ((i % 2 == 0)); then path="src/contended.rs"; else path="src/unique-$i.rs"; fi
    (
      cd "$repo" || exit 1
      rally_assert_disposable_repo "$repo" "$tmp" "$SOURCE_ROOT" || exit 70
      export HOME="$tmp/home"
      export RALLY_SESSION_ID="scale-$i"
      capture "$i" enter enter --tool "$tool" --json --timeout-ms 60000
      capture "$i" next next --tool "$tool" --json --timeout-ms 60000
      capture "$i" check check before-write --tool "$tool" --path "$path" --strict --json --timeout-ms 60000
      capture "$i" claim say claim --tool "$tool" --path "$path" --subject "scale-$mode-$n-claim-$i" --json --timeout-ms 60000
      capture "$i" artifact say artifact --tool "$tool" --subject "scale-$mode-$n-artifact-$i" --uri "file:$path" --evidence "scale gate $i" --json --timeout-ms 60000
      capture "$i" tierfit check tier-fit --tool "$tool" --role implementer --proposed-tier "$tier" --json --timeout-ms 60000
      capture "$i" handoff say handoff --tool "$tool" --target "peer-$i" --subject "scale-$mode-$n-handoff-$i" --json --timeout-ms 60000
    )
  }

  local pids=() start end
  start="$(python3 -c 'import time; print(time.monotonic())')"
  for ((i=0; i<n; i++)); do agent "$i" & pids+=("$!"); done
  for pid in "${pids[@]}"; do wait "$pid" || true; done
  end="$(python3 -c 'import time; print(time.monotonic())')"

  if [[ "$INTERNAL_MUTANT" == "op-failure" ]]; then
    printf '%s\n' '{"ok":false,"error":{"code":"forced-mutant"}}' >"$results/0.artifact.json"
    printf '%s\n' '9' >"$results/0.artifact.rc"
  elif [[ "$INTERNAL_MUTANT" == "silent-loss" ]]; then
    python3 - "$repo" "scale-$mode-$n-artifact-0" <<'PY'
import glob,json,os,sys
repo,subject=sys.argv[1],sys.argv[2]
for path in glob.glob(os.path.join(repo,".rally","log","*.jsonl")):
    lines=open(path).readlines()
    kept=[]
    removed=False
    for line in lines:
        row=json.loads(line)
        if not removed and row.get("payload",{}).get("subject")==subject:
            removed=True
            continue
        kept.append(line)
    if removed:
        open(path,"w").writelines(kept)
        break
PY
  fi

  python3 - "$mode" "$n" "$repo" "$results" "$start" "$end" "$INTERNAL_MUTANT" "$MAX_WALL_S" <<'PY'
import glob,json,os,sqlite3,sys
mode,n,repo,res,start,end,mutant,max_wall_s=sys.argv[1],int(sys.argv[2]),sys.argv[3],sys.argv[4],float(sys.argv[5]),float(sys.argv[6]),sys.argv[7],float(sys.argv[8])
failures=[]
docs={}
def load(i,op):
    key=(i,op)
    try:
        raw=open(f"{res}/{i}.{op}.json").read()
        if not raw.strip(): raw=open(f"{res}/{i}.{op}.err").read()
        docs[key]=json.loads(raw)
    except Exception as exc:
        failures.append(f"parse {i}.{op}: {exc}")
        docs[key]={}
    try: rc=int(open(f"{res}/{i}.{op}.rc").read().strip())
    except Exception as exc:
        failures.append(f"missing rc {i}.{op}: {exc}"); rc=999
    return docs[key],rc

for i in range(n):
    for op in ("enter","next","check","claim","artifact","tierfit","handoff"):
        doc,rc=load(i,op)
        if op in ("enter","next","artifact","tierfit","handoff"):
            if rc != 0 or doc.get("ok") is not True:
                failures.append(f"unexpected op failure {i}.{op}: rc={rc} ok={doc.get('ok')}")
        elif op == "check":
            allow=doc.get("data",{}).get("check",{}).get("allow")
            if not isinstance(allow,bool):
                failures.append(f"check outcome missing {i}: rc={rc}")
        elif i % 2:
            if rc != 0 or doc.get("ok") is not True:
                failures.append(f"unique claim failed {i}: rc={rc} ok={doc.get('ok')}")

session_ids=[]
for i in range(n):
    def fact_session(op, kind):
        outcomes=docs[(i,op)].get("data",{}).get("append_outcomes",[])
        matches=[row.get("fact",{}).get("from_session_id") for row in outcomes
                 if row.get("committed") and row.get("fact",{}).get("kind")==kind]
        return matches[0] if len(matches)==1 else None
    entered=fact_session("enter","presence")
    artifact=fact_session("artifact","artifact")
    if not entered or not entered.startswith("sess:managed:"):
        failures.append(f"agent {i} did not use managed local identity: {entered}")
    elif artifact != entered:
        failures.append(f"agent {i} identity changed between turns: enter={entered} artifact={artifact}")
    session_ids.append(entered)
if len(set(session_ids)) != n:
    failures.append(f"agents did not receive {n} distinct sessions: {session_ids}")

shared_success=[]
for i in range(0,n,2):
    doc,rc=docs[(i,"claim")],int(open(f"{res}/{i}.claim.rc").read())
    if rc == 0 and doc.get("ok") is True:
        shared_success.append(i)
    else:
        rendered=json.dumps(doc).lower()
        if rc == 0 or doc.get("ok") is not False or not any(token in rendered for token in ("claim conflict","overlap")):
            failures.append(f"unexpected shared claim failure {i}: rc={rc} body={rendered[:160]}")
if len(shared_success) != 1:
    failures.append(f"expected exactly one shared claim success, got {shared_success}")

facts=[]
for path in glob.glob(os.path.join(repo,".rally","log","*.jsonl")):
    for lineno,line in enumerate(open(path),1):
        try: facts.append(json.loads(line)["payload"])
        except Exception as exc: failures.append(f"ledger parse {path}:{lineno}: {exc}")
def count(kind,subject): return sum(f.get("kind")==kind and f.get("subject")==subject for f in facts)
for i in range(n):
    for kind in ("artifact","handoff"):
        subject=f"scale-{mode}-{n}-{kind}-{i}"
        if count(kind,subject) != 1:
            failures.append(f"command/ledger mismatch {kind} {i}: count={count(kind,subject)}")
    if i % 2 or i in shared_success:
        subject=f"scale-{mode}-{n}-claim-{i}"
        if count("claim",subject) != 1:
            failures.append(f"command/ledger mismatch claim {i}: count={count('claim',subject)}")
quarantine=glob.glob(os.path.join(repo,".rally","facts.db.corrupt.*"))
if quarantine: failures.append(f"quarantine files present: {quarantine}")
db_path=os.path.join(repo,".rally","facts.db")
try:
    check=sqlite3.connect(db_path).execute("PRAGMA integrity_check").fetchone()[0]
    if check != "ok": failures.append(f"integrity failure: {check}")
except Exception as exc: failures.append(f"integrity check failed: {exc}")

wall_s=end-start
if max_wall_s and wall_s > max_wall_s:
    failures.append(f"wall time {wall_s:.2f}s exceeded {max_wall_s:.2f}s")
summary={"mode":mode,"scale":n,"wall_s":round(wall_s,2),"max_wall_s":max_wall_s or None,"shared_claim_winner":shared_success,"failures":failures}
print(json.dumps(summary,sort_keys=True))
raise SystemExit(1 if failures else 0)
PY
  local gate_rc=$?

  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  return "$gate_rc"
}

run_self_test() {
  local failures=0 rc threshold_output
  set +e
  RALLY_SCALE_MUTANT=op-failure RALLY_BIN="$RALLY" "$0" --mode direct --scales 2 >/dev/null 2>&1
  rc=$?; [[ $rc -ne 0 ]] || { echo "SELF_TEST_FAIL forced operation failure passed" >&2; failures=$((failures+1)); }
  RALLY_SCALE_MUTANT=silent-loss RALLY_BIN="$RALLY" "$0" --mode direct --scales 2 >/dev/null 2>&1
  rc=$?; [[ $rc -ne 0 ]] || { echo "SELF_TEST_FAIL forced silent loss passed" >&2; failures=$((failures+1)); }
  threshold_output=$(RALLY_BIN="$RALLY" "$0" --mode direct --scales 2 --max-wall-s 0.000001 2>&1)
  rc=$?
  if [[ $rc -eq 0 || "$threshold_output" != *"wall time "*" exceeded "* ]]; then
    echo "SELF_TEST_FAIL impossible wall threshold did not fail for the expected reason" >&2
    failures=$((failures+1))
  fi
  RALLY_BIN="/definitely/missing/rally" "$0" --mode direct --scales 2 >/dev/null 2>&1
  rc=$?; [[ $rc -ne 0 ]] || { echo "SELF_TEST_FAIL missing binary passed" >&2; failures=$((failures+1)); }
  RALLY_BIN="/usr/bin/true" "$0" --mode direct --scales 2 >/dev/null 2>&1
  rc=$?; [[ $rc -ne 0 ]] || { echo "SELF_TEST_FAIL wrong binary passed" >&2; failures=$((failures+1)); }
  set -e
  if [[ $failures -eq 0 ]]; then
    echo '{"self_test":"pass","mutants_rejected":["operation-failure","silent-loss","wall-threshold","missing-binary","wrong-binary"]}'
    return 0
  fi
  return 1
}

if [[ $SELF_TEST -eq 1 ]]; then
  run_self_test
  exit $?
fi

IFS=',' read -r -a scale_values <<<"$SCALES"
overall=0
for scale in "${scale_values[@]}"; do
  if [[ ! "$scale" =~ ^[1-9][0-9]*$ ]]; then
    echo "GATE_FAIL invalid scale: $scale" >&2
    overall=1
    continue
  fi
  if [[ "$MODE" == "direct" || "$MODE" == "both" ]]; then
    run_case direct "$scale" || overall=1
  fi
  if [[ "$MODE" == "daemon" || "$MODE" == "both" ]]; then
    run_case daemon "$scale" || overall=1
  fi
done
exit "$overall"
