#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Localhost end-to-end proof (chunk E1).
#
# Two layers, both offline (mock agent, no credit, no device):
#   1. The authoritative in-process E2E suite (tests/e2e.rs): launch -> open ->
#      seq-replay -> reconnect(from_seq), auth, ping/pong.
#   2. A cross-process socket smoke: a real `cockpitd serve` on a TCP port +
#      `cockpit-cli` (the phone stand-in) doing hello + list over the wire.
#
# The iOS *app* ↔ daemon UI-level E2E (simulator driving the live daemon) is a
# DEFERRED manual step — see docs/plans/DEFERRED.md — because a reliable XCUITest
# against a live socket needs hand-tuning the app's connect config; cockpit-cli is
# the verified phone stand-in here.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO="${CARGO:-/opt/homebrew/bin/cargo}"
PORT="${PORT:-18890}"
TOKEN="e2e-$$"
DB="$(mktemp -t cockpit_e2e_XXXX.db)"
cd "$ROOT"

echo "== layer 1: in-process E2E suite =="
"$CARGO" test -p cockpitd --test e2e

echo "== layer 2: cross-process socket smoke =="
"$CARGO" build -q -p cockpitd -p cockpit-cli
export COCKPIT_TOKEN="$TOKEN" COCKPIT_ADDR="127.0.0.1:$PORT" COCKPIT_DB="$DB"
./target/debug/cockpitd serve >/tmp/cockpit_e2e_daemon.log 2>&1 &
DPID=$!
cleanup() { kill "$DPID" 2>/dev/null || true; rm -f "$DB"; }
trap cleanup EXIT
sleep 1.5

CLI=(./target/debug/cockpit-cli --addr "ws://127.0.0.1:$PORT" --token "$TOKEN")
out="$("${CLI[@]}" list)"
echo "  list -> $out"
[[ "$out" == *"no sessions"* ]] || { echo "FAIL: expected empty session list"; exit 1; }

# bad token must be rejected
if ./target/debug/cockpit-cli --addr "ws://127.0.0.1:$PORT" --token WRONG list 2>/dev/null; then
  echo "FAIL: bad token was accepted"; exit 1
fi
echo "  bad-token -> rejected (ok)"

echo "ALL E2E LAYERS PASSED"
