#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
source "$repo_root/scripts/repro_facts_db_corruption.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rc044-harness-test.XXXXXX")
cleanup() {
  rm -rf "$tmp"
}
trap cleanup EXIT

rally_dir="$tmp/.rally"
mkdir -p "$rally_dir/archive/swept"

# Retention keeps eight live groups and archives older groups by stamp. The
# canary must count all ten events, not only the live max-depth-one set.
for stamp in {3..10}; do
  : >"$rally_dir/facts.db.corrupt.$stamp"
done
for stamp in 1 2; do
  mkdir -p "$rally_dir/archive/swept/$stamp"
  : >"$rally_dir/archive/swept/$stamp/facts.db.corrupt.$stamp"
done

observed=$(count_quarantine_groups "$rally_dir")
if [[ "$observed" != "10" ]]; then
  printf 'not ok  archived quarantine groups counted: expected 10, got %s\n' "$observed" >&2
  exit 1
fi
printf 'ok   archived quarantine groups count toward the canary\n'

if counter_tracks_quarantines 9 "$observed"; then
  echo 'not ok  an under-counted corruption counter passed the canary' >&2
  exit 1
fi
printf 'ok   counter canary rejects a count below archived-plus-live groups\n'

counter_tracks_quarantines 10 "$observed"
printf 'ok   counter canary accepts an exact count\n'

# Exercise the independent 32 MiB retention path as a sparse fixture: one
# 20 MiB group remains live and one is archived, so a counter of one must not
# pass against the two recorded corruption events.
byte_rally_dir="$tmp/byte-cap/.rally"
mkdir -p "$byte_rally_dir/archive/swept/1"
dd if=/dev/zero of="$byte_rally_dir/facts.db.corrupt.2" bs=1 count=0 seek=20971520 \
  2>/dev/null
dd if=/dev/zero of="$byte_rally_dir/archive/swept/1/facts.db.corrupt.1" \
  bs=1 count=0 seek=20971520 2>/dev/null
byte_observed=$(count_quarantine_groups "$byte_rally_dir")
if [[ "$byte_observed" != "2" ]] || counter_tracks_quarantines 1 "$byte_observed"; then
  printf 'not ok  byte-cap archive did not expose the under-counted counter\n' >&2
  exit 1
fi
printf 'ok   byte-cap archived groups count toward the counter canary\n'

if daemon_arm_is_valid 0 1; then
  echo 'not ok  unavailable daemon treatment arm passed' >&2
  exit 1
fi
printf 'ok   daemon treatment arm rejects missing live rounds\n'

daemon_arm_is_valid 1 1
printf 'ok   daemon treatment arm accepts complete live coverage\n'

fake_rally="$tmp/fake-rally"
cat >"$fake_rally" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == "daemon" && "${2:-}" == "status" ]]; then
  printf '{"ok":true,"data":{"live":false}}\n'
else
  printf '{"ok":true}\n'
fi
SH
chmod +x "$fake_rally"

set +e
daemon_output=$(SEED_FACTS=0 RC044_DAEMON=1 RALLY_BIN="$fake_rally" \
  "$repo_root/scripts/repro_facts_db_corruption.sh" 1 1 1 2>&1)
daemon_status=$?
set -e
if [[ "$daemon_status" -eq 0 ]] \
  || [[ "$daemon_output" != *"DAEMON ARM INVALID: only 0 / 1 rounds used a live daemon."* ]]; then
  printf 'not ok  unavailable daemon did not fail the treatment run\n%s\n' "$daemon_output" >&2
  exit 1
fi
printf 'ok   unavailable daemon fails the full treatment run\n'
