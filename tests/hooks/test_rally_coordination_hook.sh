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
tmpdir="$(mktemp -d)"
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
