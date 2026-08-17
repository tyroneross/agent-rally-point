#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Build the current Rally CLI and run product-level acceptance against that
# exact executable. The default release mode also exercises maintainer-only
# pre-push integration suites; --product-only omits those duplicate internals.
set -euo pipefail

product_only=0
if [ "${1:-}" = "--product-only" ]; then
  product_only=1
  shift
fi
if [ "$#" -ne 0 ]; then
  echo "usage: $0 [--product-only]" >&2
  exit 64
fi

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

if [ ! -f dynamic-workflows/package.json ]; then
  echo "release-auxiliary-gate: dynamic-workflows/package.json is missing" >&2
  exit 1
fi
if ! command -v npm >/dev/null 2>&1; then
  echo "release-auxiliary-gate: npm is required for packaged workflow tests" >&2
  exit 1
fi

cargo_messages=$(mktemp "${TMPDIR:-/tmp}/rally-release-auxiliary-cargo.XXXXXX")
cleanup() {
  rm -f "$cargo_messages"
}
trap cleanup EXIT

echo "release-auxiliary-gate: building current Rally release binary" >&2
set +e
cargo build --release -p rally-cli --bin rally \
  --message-format=json-render-diagnostics >"$cargo_messages"
cargo_status=$?
set -e
if [ "$cargo_status" -ne 0 ]; then
  cat "$cargo_messages" >&2
  exit "$cargo_status"
fi

if ! rally_binary=$(python3 - "$cargo_messages" <<'PY'
import json
import os
import sys

executable = None
with open(sys.argv[1], encoding="utf-8") as messages:
    for line in messages:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = message.get("target", {})
        if (
            message.get("reason") == "compiler-artifact"
            and target.get("name") == "rally"
            and "bin" in target.get("kind", [])
            and message.get("executable")
        ):
            executable = message["executable"]

if executable is None:
    raise SystemExit(1)
print(os.path.abspath(executable))
PY
); then
  echo "release-auxiliary-gate: Cargo did not report the built rally executable" >&2
  exit 1
fi

if [ ! -x "$rally_binary" ]; then
  echo "release-auxiliary-gate: Cargo-reported rally executable is missing or not executable: $rally_binary" >&2
  exit 1
fi

echo "release-auxiliary-gate: packaged workflow tests using $rally_binary" >&2
set +e
npm_output=$(RALLY_PACKET_EMPIRICAL_BIN="$rally_binary" npm --prefix dynamic-workflows test 2>&1)
npm_status=$?
set -e
printf '%s\n' "$npm_output"
if [ "$npm_status" -ne 0 ]; then
  echo "release-auxiliary-gate: packaged workflow tests failed with exit $npm_status" >&2
  exit "$npm_status"
fi

executed_tests=$(printf '%s\n' "$npm_output" | awk '/^# tests [0-9][0-9]*$/ { print $3 }')
if ! [[ "$executed_tests" =~ ^[1-9][0-9]*$ ]]; then
  echo "release-auxiliary-gate: packaged workflow suite reported no unambiguous positive executed-test count" >&2
  exit 1
fi

skipped_tests=$(printf '%s\n' "$npm_output" | awk '/^# skipped [0-9][0-9]*$/ { print $3 }')
if ! [[ "$skipped_tests" =~ ^0$ ]]; then
  echo "release-auxiliary-gate: packaged workflow suite must report exactly zero skipped tests" >&2
  exit 1
fi

echo "release-auxiliary-gate: 2/4/6-agent local-repo acceptance" >&2
RALLY_BIN="$rally_binary" ./scripts/scale_reliability_test.sh \
  --mode both --scales 2,4,6 --max-wall-s 20

if [ "$product_only" -eq 1 ]; then
  echo "release-auxiliary-gate: product acceptance green" >&2
  exit 0
fi

echo "release-auxiliary-gate: pre-push hook suites" >&2
prepush_suites=0
prepush_status=0
for suite in tests/hooks/test_prepush_*.sh; do
  [ -f "$suite" ] || continue
  prepush_suites=$((prepush_suites + 1))
  echo "release-auxiliary-gate: $suite" >&2
  set +e
  "$suite"
  suite_status=$?
  set -e
  if [ "$suite_status" -ne 0 ]; then
    echo "release-auxiliary-gate: $suite failed with exit $suite_status" >&2
    if [ "$prepush_status" -eq 0 ]; then
      prepush_status=$suite_status
    fi
  fi
done

if [ "$prepush_suites" -eq 0 ]; then
  echo "release-auxiliary-gate: no tests/hooks/test_prepush_*.sh found; refusing to pass vacuously" >&2
  exit 1
fi
if [ "$prepush_status" -ne 0 ]; then
  exit "$prepush_status"
fi

echo "release-auxiliary-gate: all auxiliary gates green" >&2
