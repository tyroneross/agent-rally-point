#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Tests for hooks/rally-coordination-hook.sh
#
# Run: tests/hooks/test_rally_coordination_hook.sh
# Exits 0 on full pass, 1 on first failure (prints details).

set -u
# (deliberately not -e: we want to assert exit codes from the hook)

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
HOOK="$REPO_ROOT/hooks/rally-coordination-hook.sh"

if [ ! -x "$HOOK" ]; then
  echo "FAIL: hook missing or not executable at $HOOK"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()

note() { printf '  %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

# ----------------------------------------------------------------------
# Test 1: self-gate — no .rally/ → exit 0, no output
# ----------------------------------------------------------------------
T="self-gate: no .rally/ → exit 0 + empty stdout"
# Some machines can have a `.rally/` marker in the default mktemp parent
# (for example under /private/tmp), which makes every child look like a Rally
# repo when the hook walks upward. Use /var/tmp by default because these tests
# need a parent that is not already coordinated.
scratch_parent="${RALLY_TEST_TMPDIR:-/var/tmp}"
tmpdir="$(mktemp -d "${scratch_parent%/}/rally-hook-test.XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT
(
  cd "$tmpdir"
  out=$("$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" = "0" ] && [ -z "$out" ]; then
    exit 0
  else
    printf 'rc=%s out=[%s]' "$rc" "$out" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "see stderr"; fi

# ----------------------------------------------------------------------
# Test 2: fail-open — RALLY_BIN points at a non-existent binary → exit 0
# ----------------------------------------------------------------------
T="fail-open: missing rally binary → exit 0"
(
  cd "$REPO_ROOT"   # has .rally/, so self-gate passes
  RALLY_BIN="/no/such/rally/binary/exists" \
    "$HOOK" before-write claude_code </dev/null >/dev/null 2>&1
  rc=$?
  [ "$rc" = "0" ]
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 2b: session opt-out — RALLY_HOOKS=off exits before rally work
# ----------------------------------------------------------------------
T="session opt-out: RALLY_HOOKS=off → exit 0 + empty stdout"
(
  cd "$REPO_ROOT"
  out=$(RALLY_HOOKS=off RALLY_BIN="/no/such/rally/binary/exists" "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" = "0" ] && [ -z "$out" ]; then
    exit 0
  else
    printf 'rc=%s out=[%s]' "$rc" "$out" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 2c: identity — hooks must not reuse a bare host id as the routed tool.
# ----------------------------------------------------------------------
T="identity: bare host tool is scoped by session id before enter/next"
identity_bin="$tmpdir/rally_identity"
identity_calls="$tmpdir/rally_identity.calls"
cat > "$identity_bin" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${CALLS:?}"
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"off"}}}'
elif [ "$1" = "room" ]; then
  printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}'
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$identity_bin"
(
  repo="$tmpdir/identity-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  CALLS="$identity_calls" RALLY_SESSION_ID="Term A/One" RALLY_BIN="$identity_bin" "$HOOK" start claude_code </dev/null >/dev/null 2>&1
  if ! grep -q -- 'enter --tool claude_code:term-a-one --session-id Term A/One' "$identity_calls"; then
    printf 'enter did not use session-scoped tool id:\n%s\n' "$(cat "$identity_calls" 2>/dev/null)" >&2
    exit 1
  fi
  if ! grep -q -- 'next --tool claude_code:term-a-one' "$identity_calls"; then
    printf 'next did not use same session-scoped tool id:\n%s\n' "$(cat "$identity_calls" 2>/dev/null)" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "session id must affect routed tool, not only --session-id"; fi

T="identity: RALLY_AGENT_ID supplies routed ids across host families"
agent_id_calls="$tmpdir/rally_identity_agent_id.calls"
(
  repo="$tmpdir/identity-agent-id-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  : > "$agent_id_calls"
  for host in codex claude_code; do
    CALLS="$agent_id_calls" RALLY_SESSION_ID="Terminal 99" RALLY_AGENT_ID="Agent 42" RALLY_BIN="$identity_bin" "$HOOK" start "$host" </dev/null >/dev/null 2>&1
    if ! grep -q -- "enter --tool ${host}:agent-42 --session-id Terminal 99" "$agent_id_calls"; then
      printf 'enter did not use host+RALLY_AGENT_ID as routed id for %s:\n%s\n' "$host" "$(cat "$agent_id_calls" 2>/dev/null)" >&2
      exit 1
    fi
    if ! grep -q -- "next --tool ${host}:agent-42" "$agent_id_calls"; then
      printf 'next did not use host+RALLY_AGENT_ID as routed id for %s:\n%s\n' "$host" "$(cat "$agent_id_calls" 2>/dev/null)" >&2
      exit 1
    fi
  done
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "agent id must route handoffs/claims/presence across hosts"; fi

T="status heartbeat: start publishes idle with next check-in"
status_start_calls="$tmpdir/rally_status_start.calls"
(
  repo="$tmpdir/status-start-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  CALLS="$status_start_calls" RALLY_SESSION_ID="Terminal 99" RALLY_AGENT_ID="Agent 42" RALLY_CHECKIN_SECS=600 RALLY_BIN="$identity_bin" "$HOOK" start codex </dev/null >/dev/null 2>&1
  if ! grep -q -- 'status post --tool codex:agent-42 --state idle --wake-after' "$status_start_calls"; then
    printf 'start did not publish idle status with wake-after:\n%s\n' "$(cat "$status_start_calls" 2>/dev/null)" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "agents must publish state + next check-in"; fi

T="status heartbeat: no node still publishes idle and exits 0"
status_no_node_calls="$tmpdir/rally_status_no_node.calls"
(
  repo="$tmpdir/status-no-node-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  CALLS="$status_no_node_calls" PATH="/usr/bin:/bin" RALLY_SESSION_ID="Terminal 99" RALLY_AGENT_ID="Agent 42" RALLY_BIN="$identity_bin" "$HOOK" start codex </dev/null >/dev/null 2>&1
  rc=$?
  if [ "$rc" != "0" ]; then
    printf 'hook exited nonzero without node: rc=%s\n' "$rc" >&2
    exit 1
  fi
  if ! grep -q -- 'status post --tool codex:agent-42 --state idle --json' "$status_no_node_calls"; then
    printf 'no-node start did not publish idle status without wake-after:\n%s\n' "$(cat "$status_no_node_calls" 2>/dev/null)" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "status helper must stay fail-open without node"; fi

T="status heartbeat: before-write publishes working file and intent"
status_work_calls="$tmpdir/rally_status_work.calls"
(
  repo="$tmpdir/status-work-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  CALLS="$status_work_calls" RALLY_SESSION_ID="Terminal 99" RALLY_AGENT_ID="Agent 42" RALLY_BIN="$identity_bin" "$HOOK" before-write codex <<<'{"tool_input":{"file_path":"src/lib.rs"}}' >/dev/null 2>&1
  if ! grep -q -- 'status post --tool codex:agent-42 --state working --file src/lib.rs --intent editing src/lib.rs' "$status_work_calls"; then
    printf 'before-write did not publish working status:\n%s\n' "$(cat "$status_work_calls" 2>/dev/null)" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "agents must publish what they are working on"; fi

T="identity: explicit full tool id is preserved"
explicit_calls="$tmpdir/rally_identity_explicit.calls"
(
  repo="$tmpdir/identity-explicit-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  CALLS="$explicit_calls" RALLY_SESSION_ID="Term B/Two" RALLY_BIN="$identity_bin" "$HOOK" start claude_code:observer </dev/null >/dev/null 2>&1
  if grep -q -- 'claude_code:observer:term-b-two' "$explicit_calls"; then
    printf 'explicit tool id was double-suffixed:\n%s\n' "$(cat "$explicit_calls" 2>/dev/null)" >&2
    exit 1
  fi
  grep -q -- 'enter --tool claude_code:observer --session-id Term B/Two' "$explicit_calls"
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "explicit tool ids must not be rewritten"; fi

# ----------------------------------------------------------------------
# Test 2c: config opt-out — hooks status disabled stops before enter/room
# ----------------------------------------------------------------------
T="config opt-out: hooks status disabled → no room writes"
disabled_bin="$tmpdir/rally_disabled"
disabled_marker="$tmpdir/disabled-unexpected"
cat > "$disabled_bin" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":false,"prompt":"off"}}}'
  exit 0
fi
printf 'unexpected:%s\n' "$*" >> "${MARKER:?}"
exit 0
EOF
chmod +x "$disabled_bin"
(
  repo="$tmpdir/disabled-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  out=$(MARKER="$disabled_marker" RALLY_BIN="$disabled_bin" "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" = "0" ] && [ -z "$out" ] && [ ! -e "$disabled_marker" ]; then
    exit 0
  else
    printf 'rc=%s out=[%s] marker=[%s]' "$rc" "$out" "$(cat "$disabled_marker" 2>/dev/null)" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 2d: quiet room still surfaces the Rally active prompt on start
# ----------------------------------------------------------------------
T="SessionStart prompt: quiet rally repo still tells user Rally is active"
prompt_bin="$tmpdir/rally_prompt"
cat > "$prompt_bin" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"once"}}}'
elif [ "$1" = "enter" ]; then
  :
elif [ "$1" = "room" ]; then
  printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}'
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$prompt_bin"
(
  repo="$tmpdir/prompt-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  out=$(RALLY_BIN="$prompt_bin" "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" = "0" ] && printf '%s' "$out" | grep -q "Agent Rally Point is active in this repo"; then
    exit 0
  else
    printf 'rc=%s out=[%s]' "$rc" "$out" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 2e: noisy room projection is trimmed to actionable current state
# ----------------------------------------------------------------------
T="SessionStart prompt omits stale peers, expired claims, and non-actionable waits"
noise_bin="$tmpdir/rally_noise"
cat > "$noise_bin" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"once"}}}'
elif [ "$1" = "enter" ]; then
  :
elif [ "$1" = "room" ]; then
  cat <<'JSON'
{"data":{"room":{"squads":[{"tool":"claude_code","status":"active","last_seen_ts":"2999-01-01T00:00:00Z"},{"tool":"stale-peer","status":"idle","last_seen_ts":"2000-01-01T00:00:00Z"}],"active_claims":[{"tool":"claude_code","scope":["file:active.rs"],"evidence":["lease_expires_at:2999-01-01T00:00:00Z"]},{"tool":"claude_code","scope":["file:expired.rs"],"evidence":["lease_expires_at:2000-01-01T00:00:00Z"]},{"tool":"stale-peer","scope":["file:idle.rs"],"evidence":["lease_expires_at:2999-01-01T00:00:00Z"]}],"open_handoffs":[{"tool":"stale-peer","target":"codex","created_at":"2000-01-01T00:00:00Z"}]}}}
JSON
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false,"action":"wait"}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$noise_bin"
(
  repo="$tmpdir/noise-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  out=$(RALLY_BIN="$noise_bin" "$HOOK" start codex </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  printf '%s' "$out" | grep -q "Active peers: claude_code" || { printf 'missing active peer: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "file:active.rs" || { printf 'missing active claim: %s\n' "$out" >&2; exit 1; }
  for bad_text in "stale-peer" "file:expired.rs" "file:idle.rs" "Suggested next: wait"; do
    if printf '%s' "$out" | grep -q "$bad_text"; then
      printf 'stale/non-actionable text leaked (%s): %s\n' "$bad_text" "$out" >&2
      exit 1
    fi
  done
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "startup prompt must stay concise and current"; fi

# ----------------------------------------------------------------------
# Test 2f: agent status is surfaced from the typed status projection.
# ----------------------------------------------------------------------
T="SessionStart prompt includes agent status, work, and next check-in"
status_prompt_bin="$tmpdir/rally_status_prompt"
cat > "$status_prompt_bin" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"once"}}}'
elif [ "$1" = "enter" ]; then
  :
elif [ "$1" = "status" ] && [ "$2" = "post" ]; then
  printf '%s\n' '{}'
elif [ "$1" = "status" ] && [ "$2" = "read" ]; then
  cat <<'JSON'
{"data":{"status_read":{"states":[{"tool":"codex:observer","state":"idle","wake_after":"2999-01-01T00:10:00Z","last_seen_seq":1,"last_seen_ts":"2999-01-01T00:00:00Z","stale":false},{"tool":"claude_code:lead","state":"working","file":"crates/rally-cli","intent":"engine dispatch","last_seen_seq":2,"last_seen_ts":"2999-01-01T00:00:00Z","stale":false},{"tool":"gemini:qa","state":"idle","wake_after":"2999-01-01T00:05:00Z","last_seen_seq":3,"last_seen_ts":"2999-01-01T00:00:00Z","stale":false},{"tool":"codex:blocked","state":"blocked","ref":"fact_blocker","last_seen_seq":4,"last_seen_ts":"2999-01-01T00:00:00Z","stale":false},{"tool":"stale-peer","state":"working","file":"old.rs","intent":"old work","last_seen_seq":5,"last_seen_ts":"2000-01-01T00:00:00Z","stale":true}]}}}
JSON
elif [ "$1" = "room" ]; then
  printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}'
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$status_prompt_bin"
(
  repo="$tmpdir/status-prompt-repo"
  mkdir -p "$repo/.rally"
  cd "$repo"
  out=$(RALLY_BIN="$status_prompt_bin" RALLY_SESSION_ID="Status Terminal" RALLY_AGENT_ID="observer" "$HOOK" start codex </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  printf '%s' "$out" | grep -q "Agent status:" || { printf 'missing status header: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "claude_code:lead: working on crates/rally-cli («engine dispatch»)" || { printf 'missing working peer: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "gemini:qa: idle, next check-in 2999-01-01T00:05:00Z" || { printf 'missing idle wake-after: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "codex:blocked: blocked on fact_blocker" || { printf 'missing blocked ref: %s\n' "$out" >&2; exit 1; }
  if printf '%s' "$out" | grep -q "stale-peer"; then
    printf 'stale status leaked: %s\n' "$out" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "start must tell agents who is working, blocked, idle, and due next"; fi

T="UserPromptSubmit prompt includes peer status changes"
(
  repo="$tmpdir/status-idle-repo"
  mkdir -p "$repo/.rally/.hook-seen"
  cd "$repo"
  rm -f .rally/.hook-seen/status-peer-session.*.seen 2>/dev/null
  out=$(RALLY_BIN="$status_prompt_bin" RALLY_SESSION_ID="status-peer-session" RALLY_AGENT_ID="observer" "$HOOK" idle codex </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  printf '%s' "$out" | grep -q "UserPromptSubmit" || { printf 'missing UserPromptSubmit envelope: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "Agent status:" || { printf 'missing status header: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "claude_code:lead: working on crates/rally-cli («engine dispatch»)" || { printf 'missing working peer: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q "gemini:qa: idle, next check-in 2999-01-01T00:05:00Z" || { printf 'missing peer next check-in: %s\n' "$out" >&2; exit 1; }
  if printf '%s' "$out" | grep -q "codex:observer: idle"; then
    printf 'per-turn prompt should omit self-only status noise: %s\n' "$out" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "per-turn awareness must include peer status"; fi

# ----------------------------------------------------------------------
# Test 3: fail-open — rally binary that hangs → killed by watchdog,
# overall exit still 0
# ----------------------------------------------------------------------
T="fail-open: hung rally binary → watchdog kills it, hook exits 0"
hang_bin="$tmpdir/rally_hang"
cat > "$hang_bin" <<'EOF'
#!/usr/bin/env bash
sleep 60
EOF
chmod +x "$hang_bin"
(
  cd "$REPO_ROOT"
  # Tight budget — 1s — so the test completes quickly.
  start_ts=$(date +%s)
  RALLY_HOOK_TIMEOUT_MS=1000 RALLY_BIN="$hang_bin" \
    "$HOOK" before-write claude_code </dev/null >/dev/null 2>&1
  rc=$?
  end_ts=$(date +%s)
  elapsed=$(( end_ts - start_ts ))
  # Must finish within ~6s (1s budget x 3 calls + slack); rc must be 0.
  if [ "$rc" = "0" ] && [ "$elapsed" -lt 10 ]; then
    exit 0
  else
    printf 'rc=%s elapsed=%ss\n' "$rc" "$elapsed" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "watchdog did not bound runtime"; fi

# ----------------------------------------------------------------------
# Test 4: SessionStart inside rally repo → exit 0, valid JSON envelope or {}
# ----------------------------------------------------------------------
T="SessionStart in rally repo → exit 0 + JSON output"
(
  cd "$REPO_ROOT"
  out=$("$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  # Output is either {} or a JSON object with hookSpecificOutput / systemMessage.
  if [ "$rc" = "0" ] && printf '%s' "$out" | node -e 'try { JSON.parse(require("fs").readFileSync(0,"utf8")); process.exit(0); } catch (_) { process.exit(1); }' 2>/dev/null; then
    exit 0
  else
    printf 'rc=%s out=[%s]\n' "$rc" "$out" >&2
    exit 1
  fi
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 5: advisory-only invariant — even with a stub rally that emits a
# `stop`-severity envelope, default mode must NOT emit deny/block.
# ----------------------------------------------------------------------
T="advisory-only default: stop-severity → allow+systemMessage, not deny"
stub_bin="$tmpdir/rally_stub"
cat > "$stub_bin" <<'EOF'
#!/usr/bin/env bash
# Emit a synthetic "stop" severity check envelope.
cat <<JSON
{"data":{"check":{"allow":false,"agent_visible":{"present":true,"severity":"stop","message":"path already claimed by peer"}}}}
JSON
EOF
chmod +x "$stub_bin"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  # Default = no permissionDecision and no decision:block
  if printf '%s' "$out" | grep -q '"permissionDecision":"deny"'; then
    printf 'unexpected deny in default mode: %s\n' "$out" >&2; exit 1
  fi
  if printf '%s' "$out" | grep -q '"decision":"block"'; then
    printf 'unexpected block in default mode: %s\n' "$out" >&2; exit 1
  fi
  # Must surface a high-severity advisory
  if ! printf '%s' "$out" | grep -q "HIGH-SEVERITY"; then
    printf 'missing high-severity marker in advisory: %s\n' "$out" >&2; exit 1
  fi
  # Verified PreToolUse advisory contract: permissionDecision "allow"
  # (non-blocking) + systemMessage (guaranteed-surfaced warn).
  if ! printf '%s' "$out" | grep -q '"permissionDecision":"allow"'; then
    printf 'missing permissionDecision:allow: %s\n' "$out" >&2; exit 1
  fi
  if ! printf '%s' "$out" | grep -q "systemMessage"; then
    printf 'missing systemMessage envelope: %s\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "default mode must stay advisory"; fi

# ----------------------------------------------------------------------
# Test 6: strict mode — same stub, RALLY_HOOK_STRICT=1 → deny emitted
# ----------------------------------------------------------------------
T="strict mode: RALLY_HOOK_STRICT=1 + stop-severity → permissionDecision:deny"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" RALLY_HOOK_STRICT=1 "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if ! printf '%s' "$out" | grep -q '"permissionDecision":"deny"'; then
    printf 'expected deny in strict mode: %s\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 6b: Cursor host schema — before-write conflict must render Cursor's
# {permission, agent_message} contract (advisory allow), never Claude's
# permissionDecision/systemMessage keys. Strict mode → permission:deny.
# ----------------------------------------------------------------------
T="cursor schema: before-write conflict → permission:allow + agent_message (advisory)"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" "$HOOK" before-write cursor <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  printf '%s' "$out" | grep -q '"permission":"allow"' || { printf 'missing permission:allow: %s\n' "$out" >&2; exit 1; }
  printf '%s' "$out" | grep -q '"agent_message"' || { printf 'missing agent_message: %s\n' "$out" >&2; exit 1; }
  if printf '%s' "$out" | grep -qE '"permissionDecision"|"systemMessage"'; then
    printf 'cursor output leaked Claude-only keys: %s\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

T="cursor schema: strict mode → permission:deny"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" RALLY_HOOK_STRICT=1 "$HOOK" before-write cursor <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  printf '%s' "$out" | grep -q '"permission":"deny"' || { printf 'expected permission:deny: %s\n' "$out" >&2; exit 1; }
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 6c: Codex host schema — Codex rejects Claude's permissionDecision
# field on PreToolUse. A before-write conflict must fail open and surface a
# visible systemMessage instead, in default and strict modes.
# ----------------------------------------------------------------------
T="codex schema: before-write conflict → systemMessage, no permissionDecision"
(
  cd "$REPO_ROOT"
  out_default=$(RALLY_BIN="$stub_bin" "$HOOK" before-write codex <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  out_strict=$(RALLY_BIN="$stub_bin" RALLY_HOOK_STRICT=1 "$HOOK" before-write codex <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  for out in "$out_default" "$out_strict"; do
    if ! printf '%s' "$out" | grep -q "systemMessage"; then
      printf 'codex missing systemMessage: %s\n' "$out" >&2; exit 1
    fi
    if printf '%s' "$out" | grep -qE '"permissionDecision"|"permission":"deny"|"decision":"block"'; then
      printf 'codex output leaked unsupported/blocking keys: %s\n' "$out" >&2; exit 1
    fi
  done
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 7: low-severity warn → additionalContext (no deny) in both modes
# ----------------------------------------------------------------------
T="low-severity warn: never deny (even strict)"
warn_bin="$tmpdir/rally_warn"
cat > "$warn_bin" <<'EOF'
#!/usr/bin/env bash
cat <<JSON
{"data":{"check":{"allow":true,"agent_visible":{"present":true,"severity":"warn","message":"fyi: similar path was touched yesterday"}}}}
JSON
EOF
chmod +x "$warn_bin"
(
  cd "$REPO_ROOT"
  out_default=$(RALLY_BIN="$warn_bin" "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  out_strict=$(RALLY_BIN="$warn_bin" RALLY_HOOK_STRICT=1 "$HOOK" before-write claude_code <<<'{"tool_input":{"file_path":"foo.txt"}}' 2>/dev/null)
  for out in "$out_default" "$out_strict"; do
    if printf '%s' "$out" | grep -q '"permissionDecision":"deny"'; then
      printf 'warn must never deny: %s\n' "$out" >&2; exit 1
    fi
    if ! printf '%s' "$out" | grep -q "systemMessage"; then
      printf 'warn missing systemMessage: %s\n' "$out" >&2; exit 1
    fi
  done
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# ----------------------------------------------------------------------
# Test 11: anti-spam dedup — idle surfaces once, then silent until changed
# ----------------------------------------------------------------------
T="anti-spam: idle repeats are silent ({}), a changed message surfaces again"
# Stub emits an actionable `next` envelope; message text is controlled by $SUBJ.
dedup_bin="$tmpdir/rally_dedup"
cat > "$dedup_bin" <<'EOF'
#!/usr/bin/env bash
cat <<JSON
{"data":{"next":{"actionable":true,"action":"continue_or_release_claim","reason":"${SUBJ:-first message}"}}}
JSON
EOF
chmod +x "$dedup_bin"
SID="test-dedup-$$"
(
  cd "$REPO_ROOT"
  rm -f ".rally/.hook-seen/${SID}."*".seen" 2>/dev/null
  # 1st call: state is new -> must surface (non-empty, has additionalContext)
  o1=$(RALLY_BIN="$dedup_bin" RALLY_SESSION_ID="$SID" SUBJ="alpha" "$HOOK" idle claude_code </dev/null 2>/dev/null)
  # 2nd call: identical state -> must be silent ({})
  o2=$(RALLY_BIN="$dedup_bin" RALLY_SESSION_ID="$SID" SUBJ="alpha" "$HOOK" idle claude_code </dev/null 2>/dev/null)
  # 3rd call: changed message -> must surface again
  o3=$(RALLY_BIN="$dedup_bin" RALLY_SESSION_ID="$SID" SUBJ="beta" "$HOOK" idle claude_code </dev/null 2>/dev/null)
  rm -f ".rally/.hook-seen/${SID}."*".seen" 2>/dev/null
  if ! printf '%s' "$o1" | grep -q "additionalContext"; then printf '1st call should surface: [%s]
' "$o1" >&2; exit 1; fi
  if [ "$o2" != "{}" ]; then printf '2nd identical call should be silent, got: [%s]
' "$o2" >&2; exit 1; fi
  if ! printf '%s' "$o3" | grep -q "additionalContext"; then printf '3rd changed call should surface: [%s]
' "$o3" >&2; exit 1; fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "dedup must suppress repeats, surface changes"; fi

# ----------------------------------------------------------------------
# Test 11b: installed-plugin + project hook registration must execute one
# logical event once, including Rally side effects (not only message output).
# ----------------------------------------------------------------------
T="duplicate registration: identical event envelope runs Rally side effects once"
registration_bin="$tmpdir/rally_registration"
registration_calls="$tmpdir/rally_registration.calls"
cat > "$registration_bin" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${CALLS:?}"
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"off"}}}'
elif [ "$1" = "room" ]; then
  printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}'
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$registration_bin"
(
  repo="$tmpdir/duplicate-registration-repo"
  mkdir -p "$repo/.rally"
  git -C "$repo" init -q
  cd "$repo"
  envelope='{"session_id":"duplicate-registration-session","hook_event_name":"SessionStart"}'
  CALLS="$registration_calls" RALLY_HOOK_SOURCE=plugin RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  first_count="$(wc -l < "$registration_calls" | tr -d ' ')"
  CALLS="$registration_calls" RALLY_HOOK_SOURCE=project RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  second_count="$(wc -l < "$registration_calls" | tr -d ' ')"
  if [ "$first_count" -eq 0 ] || [ "$second_count" != "$first_count" ]; then
    printf 'duplicate event added Rally calls: first=%s second=%s\n%s\n' \
      "$first_count" "$second_count" "$(cat "$registration_calls" 2>/dev/null)" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "plugin+project registration must be one logical event"; fi

# ----------------------------------------------------------------------
# Test 11c: identical events from the same registration source are distinct
# invocations and must never be suppressed, including strict-mode denies.
# ----------------------------------------------------------------------
T="dedupe never suppresses repeated strict events from the same source"
(
  repo="$tmpdir/same-source-dedupe-repo"
  mkdir -p "$repo/.rally"
  git -C "$repo" init -q
  cd "$repo"
  envelope='{"session_id":"same-source-session","hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"claimed.txt"}}'
  first="$(RALLY_HOOK_SOURCE=project RALLY_HOOK_STRICT=1 RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$stub_bin" \
    "$HOOK" before-write claude_code <<<"$envelope" 2>/dev/null)"
  second="$(RALLY_HOOK_SOURCE=project RALLY_HOOK_STRICT=1 RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$stub_bin" \
    "$HOOK" before-write claude_code <<<"$envelope" 2>/dev/null)"
  if ! printf '%s' "$first" | grep -q '"permissionDecision":"deny"'; then
    printf 'first strict event was not denied: [%s]\n' "$first" >&2
    exit 1
  fi
  if ! printf '%s' "$second" | grep -q '"permissionDecision":"deny"'; then
    printf 'second same-source strict event was suppressed: [%s]\n' "$second" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "same-source events must remain fail-safe"; fi

# ----------------------------------------------------------------------
# Test 11d: source-count dedupe stays correct when two same-envelope logical
# events arrive grouped by registration source (plugin, plugin, project, project).
# ----------------------------------------------------------------------
T="dedupe source counts handle grouped registration arrival order"
(
  repo="$tmpdir/grouped-source-dedupe-repo"
  calls="$tmpdir/grouped-source-dedupe.calls"
  mkdir -p "$repo/.rally"
  git -C "$repo" init -q
  cd "$repo"
  envelope='{"session_id":"grouped-source-session","hook_event_name":"SessionStart"}'
  CALLS="$calls" RALLY_HOOK_SOURCE=plugin RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  one="$(wc -l < "$calls" | tr -d ' ')"
  CALLS="$calls" RALLY_HOOK_SOURCE=plugin RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  two="$(wc -l < "$calls" | tr -d ' ')"
  CALLS="$calls" RALLY_HOOK_SOURCE=project RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  after_project_one="$(wc -l < "$calls" | tr -d ' ')"
  CALLS="$calls" RALLY_HOOK_SOURCE=project RALLY_HOOK_DEDUPE_DIR="$repo/dedupe" RALLY_BIN="$registration_bin" \
    "$HOOK" start claude_code <<<"$envelope" >/dev/null 2>&1
  after_project_two="$(wc -l < "$calls" | tr -d ' ')"
  if [ "$one" -eq 0 ] || [ "$two" -le "$one" ] || \
    [ "$after_project_one" != "$two" ] || [ "$after_project_two" != "$two" ]; then
    printf 'grouped source counts wrong: one=%s two=%s project1=%s project2=%s\n' \
      "$one" "$two" "$after_project_one" "$after_project_two" >&2
    exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "two logical events must run exactly twice"; fi

# ----------------------------------------------------------------------
# Test 8: idle phase (UserPromptSubmit / per-turn refresh) — advisory only
# ----------------------------------------------------------------------
T="idle phase: exit 0 + valid JSON + never deny/block (default)"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" "$HOOK" idle claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if ! printf '%s' "$out" | node -e 'try { JSON.parse(require("fs").readFileSync(0,"utf8")||"{}"); process.exit(0);} catch(_){process.exit(1);} ' 2>/dev/null; then
    printf 'idle: invalid JSON: %s\n' "$out" >&2; exit 1
  fi
  if printf '%s' "$out" | grep -q '"permissionDecision":"deny"'; then
    printf 'idle must never deny: %s\n' "$out" >&2; exit 1
  fi
  if printf '%s' "$out" | grep -q '"decision":"block"'; then
    printf 'idle must never block: %s\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "idle must stay advisory"; fi

# ----------------------------------------------------------------------
# Test 9: after-write phase (Stop) — advisory only, never decision:block
# ----------------------------------------------------------------------
T="after-write phase: exit 0 + valid JSON + never block (default)"
(
  cd "$REPO_ROOT"
  out=$(RALLY_BIN="$stub_bin" "$HOOK" after-write claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if ! printf '%s' "$out" | node -e 'try { JSON.parse(require("fs").readFileSync(0,"utf8")||"{}"); process.exit(0);} catch(_){process.exit(1);} ' 2>/dev/null; then
    printf 'after-write: invalid JSON: %s\n' "$out" >&2; exit 1
  fi
  if printf '%s' "$out" | grep -q '"decision":"block"'; then
    printf 'after-write must never block: %s\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "Stop must stay advisory"; fi

# ----------------------------------------------------------------------
# Test 10: idle + after-write self-gate — no .rally/ → exit 0, empty stdout
# ----------------------------------------------------------------------
T="self-gate: idle + after-write outside .rally/ → exit 0 + empty"
(
  cd "$tmpdir"
  for ph in idle after-write; do
    o=$("$HOOK" "$ph" claude_code </dev/null 2>/dev/null); r=$?
    if [ "$r" != "0" ] || [ -n "$o" ]; then printf 'phase=%s rc=%s out=[%s]\n' "$ph" "$r" "$o" >&2; exit 1; fi
  done
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T"; fi

# ----------------------------------------------------------------------
# Test 12: git repo + no .rally/ + start → offer emitted once; second start
#          is silent (sentinel suppresses).
# ----------------------------------------------------------------------
T="no-.rally git repo: start emits offer once, second start is silent"
(
  # Create a real git repo with no .rally/.
  offer_repo="$tmpdir/offer-repo"
  mkdir -p "$offer_repo"
  cd "$offer_repo"
  git init -q
  git commit --allow-empty -q -m "init" 2>/dev/null
  # Remove any pre-existing sentinel from a prior run.
  _gc="$(git rev-parse --git-common-dir 2>/dev/null)"
  rm -f "${_gc}/rally-offer-shown" 2>/dev/null

  out1=$("$HOOK" start claude_code </dev/null 2>/dev/null)
  rc1=$?
  # First call: must contain additionalContext with "rally init".
  if [ "$rc1" != "0" ]; then printf 'rc1=%s\n' "$rc1" >&2; exit 1; fi
  if ! printf '%s' "$out1" | grep -q "rally init"; then
    printf 'first start missing offer: [%s]\n' "$out1" >&2; exit 1
  fi
  if ! printf '%s' "$out1" | grep -q "additionalContext"; then
    printf 'first start missing additionalContext: [%s]\n' "$out1" >&2; exit 1
  fi

  out2=$("$HOOK" start claude_code </dev/null 2>/dev/null)
  rc2=$?
  # Second call: sentinel exists → silent (empty or {}).
  if [ "$rc2" != "0" ]; then printf 'rc2=%s\n' "$rc2" >&2; exit 1; fi
  if printf '%s' "$out2" | grep -q "rally init"; then
    printf 'second start should be silent, got: [%s]\n' "$out2" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "offer must appear once then be suppressed by sentinel"; fi

# ----------------------------------------------------------------------
# Test 13: NON-git dir + no .rally/ + start → silent exit 0, no offer.
# ----------------------------------------------------------------------
T="non-git dir: start phase → silent exit 0, no offer"
(
  non_git="$tmpdir/non-git-dir"
  mkdir -p "$non_git"
  cd "$non_git"
  out=$("$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if [ -n "$out" ]; then
    printf 'expected empty output in non-git dir, got: [%s]\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "non-git dir must stay silent"; fi

# ----------------------------------------------------------------------
# Test 14: RALLY_HOOKS=off + git repo + no .rally/ + start → no offer.
# ----------------------------------------------------------------------
T="RALLY_HOOKS=off + no-.rally git repo: no offer on start"
(
  optout_repo="$tmpdir/optout-repo"
  mkdir -p "$optout_repo"
  cd "$optout_repo"
  git init -q
  git commit --allow-empty -q -m "init" 2>/dev/null

  out=$(RALLY_HOOKS=off "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if printf '%s' "$out" | grep -q "rally init"; then
    printf 'RALLY_HOOKS=off should suppress offer, got: [%s]\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "RALLY_HOOKS=off must suppress offer"; fi

# ----------------------------------------------------------------------
# Test 15: Regression — .rally/ PRESENT → existing start behavior preserved.
#          The test mirrors Test 2d (quiet room, prompt="once") and asserts
#          the room-awareness message is still emitted correctly.
# ----------------------------------------------------------------------
T="regression: .rally/ present → start still emits room-awareness (no regressions)"
reg_bin="$tmpdir/rally_regression"
cat > "$reg_bin" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = "hooks" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"data":{"hooks":{"enabled":true,"prompt":"once"}}}'
elif [ "$1" = "enter" ]; then
  :
elif [ "$1" = "room" ]; then
  printf '%s\n' '{"data":{"room":{"squads":[],"active_claims":[],"open_handoffs":[]}}}'
elif [ "$1" = "next" ]; then
  printf '%s\n' '{"data":{"next":{"actionable":false}}}'
else
  printf '%s\n' '{}'
fi
EOF
chmod +x "$reg_bin"
(
  reg_repo="$tmpdir/reg-repo"
  mkdir -p "$reg_repo/.rally"
  cd "$reg_repo"
  out=$(RALLY_BIN="$reg_bin" "$HOOK" start claude_code </dev/null 2>/dev/null)
  rc=$?
  if [ "$rc" != "0" ]; then printf 'rc=%s\n' "$rc" >&2; exit 1; fi
  if ! printf '%s' "$out" | grep -q "Agent Rally Point is active in this repo"; then
    printf 'regression: missing room-awareness message: [%s]\n' "$out" >&2; exit 1
  fi
  exit 0
)
if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "existing .rally/ start path must be unchanged"; fi

# ----------------------------------------------------------------------
# Test 16: provision→consume integration — a binary at ~/.local/bin/rally
# (where ensure-rally-binary.sh installs it) must be resolved even when
# ~/.local/bin is NOT on the hook PATH. Regression guard for the dormant-
# provision defect (independent-auditor f1): without the explicit fallback,
# a freshly auto-provisioned binary is invisible and the hook no-ops forever.
# ----------------------------------------------------------------------
T="provisioned ~/.local/bin/rally resolved off-PATH (provision->consume integration)"
if command -v node >/dev/null 2>&1; then
  node_dir="$(dirname "$(command -v node)")"
  (
    sbhome="$tmpdir/t16home"; mkdir -p "$sbhome/.local/bin"
    printf '#!/usr/bin/env bash\nprintf "{}"\nexit 0\n' > "$sbhome/.local/bin/rally"
    chmod +x "$sbhome/.local/bin/rally"
    repo="$tmpdir/t16repo"; mkdir -p "$repo/.rally"
    cd "$repo"
    # PATH has node but NOT rally and NOT ~/.local/bin
    out=$(HOME="$sbhome" XDG_CACHE_HOME="$sbhome/.cache" PATH="$node_dir:/usr/bin:/bin" \
      "$HOOK" start claude_code </dev/null 2>/dev/null)
    if printf '%s' "$out" | grep -q "not found on PATH"; then
      printf 'binary not resolved — got not-found advisory: %s\n' "$out" >&2; exit 1
    fi
    if ! printf '%s' "$out" | grep -q "active in this repo"; then
      printf 'expected room-awareness (binary resolved+used): %s\n' "$out" >&2; exit 1
    fi
    exit 0
  )
  if [ "$?" = "0" ]; then ok "$T"; else bad "$T" "~/.local/bin/rally must be resolvable off-PATH"; fi
else
  ok "$T (skipped — node unavailable in test env)"
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
