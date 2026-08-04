#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Adversarial suite for BLOCKER 2: when node is absent from PATH, every render
# path in hooks/rally-coordination-hook.sh used to `exit 0` with zero bytes on
# BOTH stdout and stderr. `rally enter` / `status post` / `check before-write`
# still ran, so the ledger and the room looked healthy, while the PreToolUse
# deconfliction warning — this tool's headline feature — silently never
# reached the agent. That is the RC-027 shape: "absent" and "healthy" are
# indistinguishable from the consumer's side.
#
# METHOD. Build a hermetic PATH (helpers/hermetic_path.sh, RC-025) that
# provably has no `node` resolvable, put a minimal working `rally` stub first
# on that PATH, and run the hook's start / before-write phases against a
# throwaway `.rally/`-bearing repo. Assert the hook now says why on stderr
# (naming node), does so once per session (implemented behaviour: a
# `.rally/.hook-seen` marker shared across phases, same directory the JSON
# renderer's own anti-spam logic already owns), never regresses when node IS
# present, and always exits 0 (fail-open, unconditionally).
#
# Run: bash tests/hooks/test_node_absence_advisory.sh
# Exits 0 on full pass, 1 on any failure.

set -u
# (deliberately not -e: we assert on exit codes and stderr content)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
. "$REPO_ROOT/tests/hooks/helpers/hermetic_path.sh"
HOOK="$REPO_ROOT/hooks/rally-coordination-hook.sh"

if [ ! -x "$HOOK" ]; then
  echo "FAIL: hook missing or not executable at $HOOK"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()
ok()  { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

# A parent that is definitely NOT inside a rally repo itself and is stable
# across runs (the hook walks upward looking for .rally/).
scratch_parent="${RALLY_TEST_TMPDIR:-/var/tmp}"
TMPDIR_ROOT="$(mktemp -d "${scratch_parent%/}/rally-nonode.XXXXXX")"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

# --- Hermetic "no node" PATH (RC-025: absence must be PROVEN, not assumed) --
NONODE_MIRROR="$TMPDIR_ROOT/nonode"
write_path_without "$NONODE_MIRROR" node || {
  echo "FAIL: harness could not build a PATH provably without node"
  exit 1
}

# --- Minimal "present and working" rally stub -------------------------------
# have_node=0 means the hook never parses any of this binary's stdout (every
# JSON-consuming block is itself node-gated), so the stub only needs to exit
# 0 for every subcommand the no-node phases call: enter, status post, check
# before-write. A single unconditional `exit 0` covers all of them and is
# consistent with how the rest of this suite fakes `rally` (see
# test_rally_coordination_hook.sh's identity_bin / disabled_bin / stub_bin).
STUB_DIR="$TMPDIR_ROOT/stub"
mkdir -p "$STUB_DIR"
cat > "$STUB_DIR/rally" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$STUB_DIR/rally"

NO_NODE_PATH="$STUB_DIR:$NONODE_MIRROR"

new_repo() {  # $1 = name -> prints repo path with .rally/ present
  local d="$TMPDIR_ROOT/$1"
  mkdir -p "$d/.rally"
  printf '%s' "$d"
}

# ----------------------------------------------------------------------
# Test 1: SessionStart, node absent, rally present -> advisory naming node
# on stderr, hook still exits 0.
# ----------------------------------------------------------------------
T="node-absent SessionStart: advisory names node on stderr, exits 0"
(
  repo="$(new_repo session-start-repo)"
  cd "$repo" || exit 1
  err="$TMPDIR_ROOT/t1.stderr"
  out=$(PATH="$NO_NODE_PATH" RALLY_SESSION_ID="t1-session" \
    "$HOOK" start claude_code </dev/null 2>"$err")
  rc=$?
  if [ "$rc" != "0" ]; then
    printf 'rc=%s out=[%s] err=[%s]\n' "$rc" "$out" "$(cat "$err")" >&2
    exit 1
  fi
  if ! grep -qi "node" "$err"; then
    printf 'stderr does not name node: [%s]\n' "$(cat "$err")" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "SessionStart must say node is missing, not go silent"; fi

# ----------------------------------------------------------------------
# Test 2: before-write, node absent, rally present, FRESH session (no prior
# SessionStart in this session) -> the PreToolUse/before-write path also
# surfaces the degradation on its own, same as SessionStart above.
# ----------------------------------------------------------------------
T="node-absent before-write (first phase in session): advisory names node, exits 0"
(
  repo="$(new_repo before-write-first-repo)"
  cd "$repo" || exit 1
  err="$TMPDIR_ROOT/t2.stderr"
  out=$(PATH="$NO_NODE_PATH" RALLY_SESSION_ID="t2-session" \
    "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"src/lib.rs"}}' 2>"$err")
  rc=$?
  if [ "$rc" != "0" ]; then
    printf 'rc=%s out=[%s] err=[%s]\n' "$rc" "$out" "$(cat "$err")" >&2
    exit 1
  fi
  if ! grep -qi "node" "$err"; then
    printf 'stderr does not name node: [%s]\n' "$(cat "$err")" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "before-write must not rely on SessionStart having already fired"; fi

# ----------------------------------------------------------------------
# Test 3: IMPLEMENTED BEHAVIOUR IS ONCE-PER-SESSION ACROSS PHASES. When
# SessionStart already surfaced the advisory in a session, a later
# before-write in the SAME session must be silent on stderr (the shared
# `.rally/.hook-seen` marker suppresses the repeat) — anti-spam, not a
# second identical warning on every tool call. Still exits 0.
# ----------------------------------------------------------------------
T="node-absent once-per-session: before-write after start in same session is silent (implemented: marker suppression)"
(
  repo="$(new_repo once-per-session-repo)"
  cd "$repo" || exit 1
  session="t3-session"
  # First call (start) seeds the marker and must itself advise.
  err_start="$TMPDIR_ROOT/t3.start.stderr"
  PATH="$NO_NODE_PATH" RALLY_SESSION_ID="$session" \
    "$HOOK" start claude_code </dev/null 2>"$err_start" >/dev/null
  rc_start=$?
  if [ "$rc_start" != "0" ] || ! grep -qi "node" "$err_start"; then
    printf 'setup call (start) did not behave as Test 1: rc=%s err=[%s]\n' "$rc_start" "$(cat "$err_start")" >&2
    exit 1
  fi
  # Second call (before-write), same session -> must be silent.
  err_write="$TMPDIR_ROOT/t3.write.stderr"
  PATH="$NO_NODE_PATH" RALLY_SESSION_ID="$session" \
    "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"src/lib.rs"}}' 2>"$err_write" >/dev/null
  rc_write=$?
  if [ "$rc_write" != "0" ]; then
    printf 'before-write after start did not exit 0: rc=%s err=[%s]\n' "$rc_write" "$(cat "$err_write")" >&2
    exit 1
  fi
  if [ -s "$err_write" ]; then
    printf 'repeat advisory was NOT suppressed within the same session: [%s]\n' "$(cat "$err_write")" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "same-session repeats must not spam stderr on every tool call"; fi

# ----------------------------------------------------------------------
# Test 4: node PRESENT -> no false positive. The advisory must not appear
# when node resolves normally, on either phase.
# ----------------------------------------------------------------------
T="node-present: no node-missing advisory on start or before-write (no false positive)"
if command -v node >/dev/null 2>&1; then
  (
    repo="$(new_repo node-present-repo)"
    cd "$repo" || exit 1
    err_start="$TMPDIR_ROOT/t4.start.stderr"
    err_write="$TMPDIR_ROOT/t4.write.stderr"
    # Inherit the real PATH (node present) but keep the stub rally in front
    # so the working directory's real rally, if any, is never invoked.
    PATH="$STUB_DIR:$PATH" RALLY_SESSION_ID="t4-session" \
      "$HOOK" start claude_code </dev/null 2>"$err_start" >/dev/null
    rc_start=$?
    PATH="$STUB_DIR:$PATH" RALLY_SESSION_ID="t4-session" \
      "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"src/lib.rs"}}' 2>"$err_write" >/dev/null
    rc_write=$?
    if [ "$rc_start" != "0" ] || [ "$rc_write" != "0" ]; then
      printf 'rc_start=%s rc_write=%s\n' "$rc_start" "$rc_write" >&2
      exit 1
    fi
    if grep -qi "node is not on PATH" "$err_start" "$err_write"; then
      printf 'false positive: advisory fired with node present: start=[%s] write=[%s]\n' \
        "$(cat "$err_start")" "$(cat "$err_write")" >&2
      exit 1
    fi
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "advisory must be conditioned on node's actual absence"; fi
else
  ok "$T (skipped — node unavailable in test env, so 'node present' cannot be exercised)"
fi

# Summary
# ----------------------------------------------------------------------
echo ""
echo "Passed: $PASS"
echo "Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  for f in "${FAILS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
