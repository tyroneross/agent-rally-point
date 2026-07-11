#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# hammer-rallyd.sh — the F4 acceptance hammer (BACKLOG S-P3, Chunk D, T-07).
#
# Runs BOTH #50 acceptance tests in DAEMON-SERVING mode (RALLY_TEST_RALLYD=1),
# 30 rounds each (configurable), inside a `rust:1.95` container, and reports a
# per-test PASS/FAIL summary. This mirrors the issue-#50 hammer pattern (repeated
# runs to surface a probabilistic race) — no NEW verdict source — but with rallyd
# serving, so the acceptance criterion is 0/N failures: the #50 bootstrap race is
# structurally dissolved when exactly one process (the daemon) owns facts.db.
#
#   Tests hammered:
#     * watchdog_concurrency::parallel_say_invocations_never_drop_or_duplicate_facts
#     * user_journey::rally_run_reserves_numbered_ids_under_parallel_launch
#
# Usage:
#     scripts/hammer-rallyd.sh [ROUNDS]      # default ROUNDS=30
#     scripts/hammer-rallyd.sh 3             # short smoke before the full 30
#
# Session-ops traps honored:
#   * Named cargo-registry + target cache volumes so rebuilds are fast.
#   * Container runs DETACHED (`docker run -d`); results are read via
#     `docker logs` (piping to a client-killed process would lose them).
#   * The exact `docker run` invocation is printed before launch.
#   * `orb start` guard if docker isn't responding.

set -uo pipefail

ROUNDS="${1:-30}"
IMAGE="rust:1.95"
CARGO_CACHE_VOL="rallyd-hammer-cargo-cache"
TARGET_CACHE_VOL="rallyd-hammer-target-cache"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ── orb / docker guard ───────────────────────────────────────────────────────
if ! docker info >/dev/null 2>&1; then
  echo "docker not responding; attempting 'orb start'..."
  orb start >/dev/null 2>&1 || true
  for _ in $(seq 1 30); do
    docker info >/dev/null 2>&1 && break
    sleep 1
  done
  if ! docker info >/dev/null 2>&1; then
    echo "ERROR: docker still not available after 'orb start'; aborting." >&2
    exit 1
  fi
fi

# Ensure the cache volumes exist (idempotent).
docker volume create "$CARGO_CACHE_VOL"  >/dev/null
docker volume create "$TARGET_CACHE_VOL" >/dev/null

# ── in-container hammer script ───────────────────────────────────────────────
# Builds the two test binaries once (cached across rounds via the target
# volume), then runs each test ROUNDS times with RALLY_TEST_RALLYD=1, counting
# failures and tailing the log of any failing round.
read -r -d '' CONTAINER_SCRIPT <<'INNER'
set -uo pipefail
cd /work
export CARGO_TERM_COLOR=never
ROUNDS="__ROUNDS__"

echo "=== rallyd F4 hammer: building test binaries (rust: $(rustc --version)) ==="
cargo test -p rally-cli --test watchdog_concurrency --test user_journey --no-run 2>&1 | tail -6

run_rounds() {
  test_file="$1"; test_name="$2"; rounds="$3"
  fails=0
  for i in $(seq 1 "$rounds"); do
    logf="/tmp/${test_file}_round_${i}.log"
    if RALLY_TEST_RALLYD=1 cargo test -p rally-cli --test "$test_file" \
         "$test_name" -- --exact --nocapture >"$logf" 2>&1; then
      echo "[$test_file] round $i/$rounds PASS"
    else
      fails=$((fails + 1))
      echo "[$test_file] round $i/$rounds FAIL"
      echo "----- failing round $i log tail -----"
      tail -50 "$logf"
      echo "----- end round $i log -----"
    fi
  done
  if [ "$fails" -eq 0 ]; then
    echo "SUMMARY $test_file: PASS 0/$rounds"
  else
    echo "SUMMARY $test_file: FAIL $fails/$rounds"
  fi
  return "$fails"
}

run_rounds watchdog_concurrency \
  parallel_say_invocations_never_drop_or_duplicate_facts "$ROUNDS"
WATCHDOG_FAILS=$?

run_rounds user_journey \
  rally_run_reserves_numbered_ids_under_parallel_launch "$ROUNDS"
RUNID_FAILS=$?

echo "=== HAMMER COMPLETE: watchdog_fails=${WATCHDOG_FAILS} run_id_fails=${RUNID_FAILS} ==="
if [ "$WATCHDOG_FAILS" -eq 0 ] && [ "$RUNID_FAILS" -eq 0 ]; then
  echo "F4 VERDICT: PASS (both tests 0/${ROUNDS})"
  exit 0
else
  echo "F4 VERDICT: FAIL"
  exit 1
fi
INNER
CONTAINER_SCRIPT="${CONTAINER_SCRIPT//__ROUNDS__/$ROUNDS}"

# ── launch ───────────────────────────────────────────────────────────────────
DOCKER_RUN_ARGS=(
  run -d
  -v "${REPO_ROOT}:/work"
  -v "${CARGO_CACHE_VOL}:/usr/local/cargo/registry"
  -v "${TARGET_CACHE_VOL}:/work/target"
  -w /work
  "$IMAGE"
  bash -c "$CONTAINER_SCRIPT"
)

echo "=== rallyd hammer: ${ROUNDS} rounds/test, image ${IMAGE} ==="
echo "exact invocation:"
echo "  docker run -d \\"
echo "    -v ${REPO_ROOT}:/work \\"
echo "    -v ${CARGO_CACHE_VOL}:/usr/local/cargo/registry \\"
echo "    -v ${TARGET_CACHE_VOL}:/work/target \\"
echo "    -w /work ${IMAGE} bash -c '<hammer script, ROUNDS=${ROUNDS}>'"
echo

CID="$(docker "${DOCKER_RUN_ARGS[@]}")"
if [ -z "$CID" ]; then
  echo "ERROR: failed to start hammer container." >&2
  exit 1
fi
echo "hammer container: $CID"
echo "streaming logs (container keeps running even if this client is killed;"
echo "re-attach with:  docker logs -f $CID )"
echo

# Stream logs live; the container is detached so log loss on client death is
# impossible — the authoritative record is `docker logs $CID`.
docker logs -f "$CID"

# Reap the container's exit code as the script's verdict.
EXIT_CODE="$(docker wait "$CID" 2>/dev/null || echo 1)"
docker rm "$CID" >/dev/null 2>&1 || true
echo
echo "=== hammer container exit code: ${EXIT_CODE} ==="
exit "$EXIT_CODE"
