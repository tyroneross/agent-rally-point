#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# RC-044 reproduction harness: N-way concurrent mutation against one room,
# repeated for TRIALS rounds. Models the real-world profile that produced the
# `facts.db.corrupt.*` debris — many agents appending to and reading an
# established room at once — rather than first-run `enter` alone (which mostly
# exercises lead-seat arbitration and refuses by design).
#
# Counts, per round:
#   * hard command failures (non-zero exit that is NOT an expected refusal)
#   * `facts.db.corrupt.*` quarantine files produced
#   * `PRAGMA integrity_check` verdict on the surviving facts.db
#
# RC-044's standing rule: a flaky gate certifies failures, so a fix is only
# credible against N-consecutive clean rounds. Run before AND after any candidate
# fix and compare the corruption rate.
#
# Usage: repro_facts_db_corruption.sh [TRIALS] [WAYS] [OPS_PER_WORKER]
set -uo pipefail

TRIALS="${1:-10}"
WAYS="${2:-6}"
OPS="${3:-12}"
SEED_FACTS="${SEED_FACTS:-40}"
RALLY_BIN="${RALLY_BIN:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/rally}"

if [[ ! -x "$RALLY_BIN" ]]; then
  echo "rally binary not found/executable at $RALLY_BIN" >&2
  echo "build it with: cargo build --release -p rally-cli" >&2
  exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rc044-repro.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

total_fail=0
total_quarantine=0
total_bad_integrity=0
rounds_dirty=0
daemon_rounds=0

printf 'RC-044 repro: %s trials x %s-way concurrent mutation (%s ops/worker)\n' \
  "$TRIALS" "$WAYS" "$OPS"
printf 'binary: %s\n' "$RALLY_BIN"
printf 'arm   : %s\n\n' \
  "$([[ "${RC044_DAEMON:-0}" == "1" ]] && echo 'rallyd single-writer (RC044_DAEMON=1)' || echo 'direct store (default)')"

for ((trial = 1; trial <= TRIALS; trial++)); do
  room="$WORK/room-$trial"
  home="$WORK/home-$trial"
  mkdir -p "$room" "$home"
  git -C "$room" init -q 2>/dev/null || true

  export HOME="$home"
  export RALLY_HOOKS=off

  # --- establish the room + a history deep enough that a rebuild is real work
  (cd "$room" && "$RALLY_BIN" enter --tool claude_code:01 --json) >/dev/null 2>&1
  for ((s = 1; s <= SEED_FACTS; s++)); do
    (cd "$room" && "$RALLY_BIN" say artifact --tool claude_code:01 \
      --subject "seed fact $s" --json) >/dev/null 2>&1
  done

  # --- optional arm: route every worker through the single-writer daemon.
  # RC-044's open question is whether serializing store access closes the fault.
  # `rally daemon start` returns only after the socket is bound and a ping
  # round-trips, so the storm below cannot race the daemon's cold start.
  # A treatment arm that silently fell back to the direct store would look like
  # a clean result and mean nothing, so liveness is ASSERTED, not assumed.
  daemon_up=0
  if [[ "${RC044_DAEMON:-0}" == "1" ]]; then
    (cd "$room" && "$RALLY_BIN" daemon start --json) >/dev/null 2>&1
    if (cd "$room" && "$RALLY_BIN" daemon status --json) 2>/dev/null \
      | grep -q '"live": *true'; then
      daemon_up=1
      daemon_rounds=$((daemon_rounds + 1))
    else
      printf 'trial %3d: daemon NOT LIVE — this round is not a daemon arm\n' "$trial"
    fi
  fi

  # --- N-way concurrent mixed read/write storm
  pids=()
  for ((w = 1; w <= WAYS; w++)); do
    (
      cd "$room" || exit 1
      tool="claude_code:$(printf '%02d' "$w")"
      for ((o = 1; o <= OPS; o++)); do
        "$RALLY_BIN" say artifact --tool "$tool" \
          --subject "w$w op$o storm" --json >>"$room/.w$w.out" 2>>"$room/.w$w.err"
        "$RALLY_BIN" room --json >>"$room/.w$w.out" 2>>"$room/.w$w.err"
      done
    ) &
    pids+=("$!")
  done
  for pid in "${pids[@]}"; do wait "$pid"; done

  # Stop before measuring: the daemon holds the store open, and quarantine +
  # integrity_check must read a settled database, not a live one.
  if ((daemon_up == 1)); then
    (cd "$room" && "$RALLY_BIN" daemon stop --json) >/dev/null 2>&1
  fi

  # Hard failures only: expected coordination refusals (lead seat, claim
  # conflict, ack gating) are the product working, not the defect under test.
  fails=$(grep -h '"ok":false' "$room"/.w*.err 2>/dev/null \
    | grep -cv 'lead transfer refused\|already holds\|claim conflict\|not acknowledged' \
    || true)
  fails=${fails:-0}

  quarantines=$(find "$room/.rally" -maxdepth 1 -name 'facts.db.corrupt.*' \
    ! -name '*-db-wal' ! -name '*-db-shm' 2>/dev/null | wc -l | tr -d ' ')

  integrity="absent"
  if [[ -f "$room/.rally/facts.db" ]]; then
    integrity=$(sqlite3 "$room/.rally/facts.db" 'PRAGMA integrity_check;' 2>&1 | head -1)
  fi

  bad_integrity=0
  [[ "$integrity" != "ok" && "$integrity" != "absent" ]] && bad_integrity=1

  total_fail=$((total_fail + fails))
  total_quarantine=$((total_quarantine + quarantines))
  total_bad_integrity=$((total_bad_integrity + bad_integrity))

  if ((fails > 0 || quarantines > 0 || bad_integrity > 0)); then
    rounds_dirty=$((rounds_dirty + 1))
    printf 'trial %3d: DIRTY  hard_fails=%d quarantines=%d integrity=%s\n' \
      "$trial" "$fails" "$quarantines" "$integrity"
    if ((rounds_dirty == 1)); then
      cp -R "$room/.rally" "$WORK/first-dirty-rally" 2>/dev/null || true
      grep -h '"ok":false' "$room"/.w*.err 2>/dev/null \
        | grep -v 'lead transfer refused\|already holds\|claim conflict\|not acknowledged' \
        | sort -u | head -3 | cut -c1-220 | sed 's/^/            /'
    fi
  else
    printf 'trial %3d: clean\n' "$trial"
  fi
done

printf '\n== summary ==\n'
printf 'trials              : %d (%d-way, %d ops each)\n' "$TRIALS" "$WAYS" "$OPS"
printf 'dirty rounds        : %d / %d\n' "$rounds_dirty" "$TRIALS"
printf 'hard failures       : %d\n' "$total_fail"
printf 'quarantine files    : %d\n' "$total_quarantine"
printf 'bad integrity_check : %d\n' "$total_bad_integrity"
if [[ "${RC044_DAEMON:-0}" == "1" ]]; then
  printf 'daemon-live rounds  : %d / %d  (arm validity)\n' "$daemon_rounds" "$TRIALS"
fi

if ((rounds_dirty > 0)); then
  printf '\nfirst dirty round preserved at: %s\n' "$WORK/first-dirty-rally"
  trap - EXIT
  printf '(work dir NOT cleaned: %s)\n' "$WORK"
  exit 1
fi
exit 0
