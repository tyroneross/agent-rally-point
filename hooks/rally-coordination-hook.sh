#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Rally Coordination Hook — canonical, version-controlled, host-neutral.
#
# CHARTER (never-block, advisory-only): rally records/exposes coordination
# signals; it NEVER denies or blocks a write by default. See `rally mission`.
# A strict-mode escape hatch exists (RALLY_HOOK_STRICT=1) for orchestration
# paths that want hard gates; off by default.
#
# This file is the SINGLE SOURCE OF TRUTH for both Claude Code and Codex (and
# Gemini) rally hooks. The desync incident documented in
# docs/assessment-2026-05-31-codex-hook-desync.md ("Recurrence risk — wrapper
# is not version-controlled") is the reason this file lives in-repo. Any
# host-side wrapper (e.g. ~/.codex/rally-hook.sh) should be a thin shim that
# `exec`s this script; see scripts/install_rally_hooks.sh.
#
# Usage (called by a coding host's hook system):
#   rally-coordination-hook.sh <phase> <tool>
#     phase ∈ { start | before-write | after-write | idle }
#     tool  ∈ free-form id (e.g. claude_code, codex, gemini, claude_code:01)
#   STDIN: the host's hook input envelope (JSON). Optional.
#
# Behavior:
#   - Self-gate: if no .rally/ is found walking up from cwd, exit 0 with no
#     output. Safe to install globally; only acts in rally repos.
#   - Fail-open: any rally CLI error / timeout / missing binary → exit 0.
#   - Advisory only (default): emits `additionalContext` / `systemMessage`,
#     never `permissionDecision: "deny"` / `decision: "block"`.
#   - Strict mode (opt-in, RALLY_HOOK_STRICT=1): high-severity coordination
#     signals (allow=false or severity=stop) emit a deny/block decision.
#     Off-charter; documented as an escape hatch.
#
# Env:
#   RALLY_HOOK_TIMEOUT_MS  — wall-clock budget for each rally call (default 5000).
#   RALLY_BIN              — rally binary path (default ./target/debug/rally or $PATH).
#   RALLY_SESSION_ID       — override session id (default <tool>-<epoch>).
#   RALLY_HOOK_STRICT      — "1" to enable deny/block on high-severity signals.
#
# Exit code: 0 always (fail-open). Output goes on stdout per host hook contract.

set -euo pipefail

# --- Self-gate: walk up from $PWD to find .rally/; exit 0 if absent --------
find_rally_root() {
  local dir
  dir="$(pwd -P 2>/dev/null || pwd)"
  while [ -n "$dir" ] && [ "$dir" != "/" ]; do
    if [ -d "$dir/.rally" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
    dir="$(dirname "$dir")"
  done
  return 1
}

if ! find_rally_root >/dev/null 2>&1; then
  # Not a rally repo — silent no-op so this script is safe to install globally.
  exit 0
fi

# --- Defense-in-depth wall-clock guard ------------------------------------
# The `rally` binary self-bounds via an internal watchdog (default 3s), but a
# hung `rally` (or a wedged child it spawns) must NEVER outlive a short budget
# even if the binary is old/missing. macOS ships no `timeout(1)` by default,
# so we detect a real timeout command and otherwise fall back to a portable
# perl-alarm shim, with a pure-bash background-kill shim as last resort.
_rally_budget_ms="${RALLY_HOOK_TIMEOUT_MS:-5000}"
_rally_budget_s=$(( (_rally_budget_ms + 999) / 1000 ))
[ "$_rally_budget_s" -lt 1 ] && _rally_budget_s=1

if [ -z "${RALLY_BIN:-}" ]; then
  if [ -x "./target/debug/rally" ]; then
    RALLY_BIN="./target/debug/rally"
  else
    RALLY_BIN="rally"
  fi
fi

# If the binary truly is missing, fail-open immediately rather than emit shell
# errors. We test once up front so the watchdog branches stay clean.
#
# Plugin-install advisory (SessionStart only, never for PreToolUse): when this
# hook fires inside a rally repo (.rally/ present, gate above) but `rally` is
# not on PATH, emit a one-time concise install hint. This handles the
# plugin-only install case where skills/hooks shipped via marketplace but the
# Rust CLI was not built. Stays charter-compliant: advisory `additionalContext`
# only, exit 0, never blocks. Suppressed on every other phase so PreToolUse
# does not spam the agent on every edit.
if ! command -v "$RALLY_BIN" >/dev/null 2>&1 && [ ! -x "$RALLY_BIN" ]; then
  if [ "${1:-idle}" = "start" ]; then
    rally_root="$(find_rally_root 2>/dev/null || true)"
    msg="Agent Rally Point: rally CLI not found on PATH (looked for: $RALLY_BIN). This repo uses .rally/ but the backend binary is missing. To enable coordination: \`cd ${rally_root:-<rally-repo>} && cargo install --path crates/rally-cli\` (installs to ~/.local/bin/rally). Until then, this plugin's hooks no-op and skills will report rally errors."
    if command -v node >/dev/null 2>&1; then
      printf '%s' "$msg" | node -e '
let m=""; process.stdin.on("data",c=>m+=c); process.stdin.on("end",()=>{
  process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:m}}));
});'
    fi
  fi
  exit 0
fi

if command -v timeout >/dev/null 2>&1; then
  rally_timeout() { timeout -k 1 "${_rally_budget_s}s" "$RALLY_BIN" "$@"; }
elif command -v gtimeout >/dev/null 2>&1; then
  rally_timeout() { gtimeout -k 1 "${_rally_budget_s}s" "$RALLY_BIN" "$@"; }
elif command -v perl >/dev/null 2>&1; then
  # IMPORTANT: this shim fork+setsid's the child so we can kill its WHOLE
  # process group on timeout. The earlier `exec`-based shim leaked grandchildren
  # (e.g. a `sleep 60` inside the hung binary) which kept the captured stdout
  # FD open, making `$(...)` hang for the grandchild's lifetime even though
  # the watchdog had already "expired."
  rally_timeout() {
    perl -e '
      use POSIX qw(setsid);
      my $t = shift;
      my $pid = fork();
      die "fork failed" unless defined $pid;
      if ($pid == 0) {
        setsid();           # new process group; we kill it on timeout
        exec @ARGV or exit 127;
      }
      $SIG{ALRM} = sub {
        kill "-KILL", $pid;
        waitpid($pid, 0);
        exit 124;
      };
      alarm $t;
      waitpid($pid, 0);
      exit($? >> 8);
    ' "$_rally_budget_s" "$RALLY_BIN" "$@";
  }
else
  # Pure-bash shim: run rally in its own process group, kill the group on
  # overrun. Same FD-leak concern as the perl shim above motivates `setsid`
  # if available; without it we kill what we can.
  rally_timeout() {
    if command -v setsid >/dev/null 2>&1; then
      setsid "$RALLY_BIN" "$@" &
    else
      "$RALLY_BIN" "$@" &
    fi
    local pid=$!
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
      if [ "$waited" -ge "$_rally_budget_s" ]; then
        kill -KILL "-$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
        return 124
      fi
      sleep 1; waited=$((waited + 1))
    done
    wait "$pid"
  }
fi
# --------------------------------------------------------------------------

# Optional node detection — used only for parsing host hook envelopes and
# rally JSON output. If node is missing, we still emit basic output (we just
# can't extract file_path from PreToolUse input).
have_node=0
if command -v node >/dev/null 2>&1; then have_node=1; fi

phase="${1:-idle}"
tool="${2:-claude_code}"

# Read stdin envelope if present; do not block if empty.
input=""
if [ ! -t 0 ]; then
  input="$(cat || true)"
fi

# Extract file_path + session_id from the host's hook input envelope.
path=""
session=""
if [ "$have_node" = "1" ] && [ -n "$input" ]; then
  meta="$({ printf '%s' "$input" | node -e '
let data=""; process.stdin.on("data", c => data += c); process.stdin.on("end", () => {
  try {
    const value = JSON.parse(data || "{}");
    const toolInput = value.tool_input || value.toolInput || value.input || value;
    const path = toolInput.file_path || toolInput.filePath || toolInput.path || toolInput.notebook_path || "";
    const session = value.session_id || value.sessionId || "";
    process.stdout.write(JSON.stringify({path, session}));
  } catch (_) { process.stdout.write("{}"); }
});
' ; } 2>/dev/null)"
  path="$({ printf '%s' "$meta" | node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(0,"utf8")||"{}"); process.stdout.write(v.path||""); } catch (_) {}' ; } 2>/dev/null)"
  session="$({ printf '%s' "$meta" | node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(0,"utf8")||"{}"); process.stdout.write(v.session||""); } catch (_) {}' ; } 2>/dev/null)"
fi
if [ -z "$session" ]; then session="${RALLY_SESSION_ID:-${tool}-$(date +%s)}"; fi

# Dispatch on phase.
rally_output=""
if [ "$phase" = "start" ]; then
  # Register presence (auto-enter), then surface room awareness so a NEW agent
  # automatically knows there is an active room + who owns what, and deconflicts
  # before editing. Stays quiet (no nag) when the agent is solo.
  rally_timeout enter --tool "$tool" --session-id "$session" --json >/dev/null 2>&1 || true
  if [ "$have_node" = "1" ]; then
    room_json="$(rally_timeout room --json 2>/dev/null || true)"
    next_json="$(rally_timeout next --tool "$tool" --json 2>/dev/null || true)"
    rally_output="$({ printf '%s' "$room_json" | RALLY_NEXT_JSON="$next_json" RALLY_SELF_TOOL="$tool" node -e '
const fs = require("fs");
const tool = process.env.RALLY_SELF_TOOL || "";
let room = {}, nxt = {};
try { room = JSON.parse(fs.readFileSync(0, "utf8") || "{}"); } catch (_) {}
try { nxt = JSON.parse(process.env.RALLY_NEXT_JSON || "{}"); } catch (_) {}
const R = room?.data?.room || {};
const squads = Array.isArray(R.squads) ? R.squads : [];
const peers = [...new Set(squads.map(s => s && s.tool).filter(t => t && t !== tool && t !== "rally"))];
const claims = (Array.isArray(R.active_claims) ? R.active_claims : [])
  .filter(c => c && c.tool !== tool)
  .map(c => `${(c.scope || []).join(",") || "?"} (by ${c.tool})`);
const handoffs = Array.isArray(R.open_handoffs) ? R.open_handoffs.length : 0;
const nextAction = nxt?.data?.next?.action;
if (peers.length === 0 && claims.length === 0 && handoffs === 0) { process.stdout.write("{}"); process.exit(0); }
let msg = "Active rally room here. ";
if (peers.length) msg += `Peers: ${peers.join(", ")}. `;
if (claims.length) msg += `Open claims: ${claims.slice(0, 8).join("; ")}. `;
if (handoffs) msg += `${handoffs} open handoff(s). `;
if (nextAction) msg += `Suggested next: ${nextAction}. `;
msg += "Before editing, check `rally room` / `rally next` and deconflict — do not edit a path another agent has claimed (rally auto-checks before each write).";
process.stdout.write(JSON.stringify({ agent_visible: { present: true, severity: "warn", message: msg } }));
' ; } 2>/dev/null)"
  fi
elif [ "$phase" = "before-write" ]; then
  if [ -n "$path" ]; then
    rally_output="$(rally_timeout check before-write --tool "$tool" --path "$path" --json 2>/dev/null || true)"
  else
    rally_output="$(rally_timeout check before-write --tool "$tool" --json 2>/dev/null || true)"
  fi

  # Auto-claim if the check allowed it and the path isn't already claimed by us.
  if [ "$have_node" = "1" ] && [ -n "$path" ]; then
    should_claim="$({ printf '%s' "$rally_output" | node -e '
const fs = require("fs");
try {
  const parsed = JSON.parse(fs.readFileSync(0, "utf8") || "{}");
  process.stdout.write(parsed?.data?.check?.allow === true ? "yes" : "no");
} catch (_) {
  process.stdout.write("no");
}
' ; } 2>/dev/null)"

    if [ "$should_claim" = "yes" ]; then
      room_output="$(rally_timeout room --json 2>/dev/null || true)"
      already_claimed="$({ printf '%s' "$room_output" | node -e '
const fs = require("fs");
const tool = process.argv[1] || "";
const path = process.argv[2] || "";
function clean(value) {
  let out = String(value || "");
  if (out.startsWith("file:")) out = out.slice(5);
  if (out.startsWith("./")) out = out.slice(2);
  const cwd = process.cwd();
  if (out.startsWith(cwd + "/")) out = out.slice(cwd.length + 1);
  return out.replace(/\/+/g, "/").replace(/\/$/, "");
}
function matches(scope, candidate) {
  const s = clean(scope);
  const p = clean(candidate);
  return Boolean(s) && (s === p || p.startsWith(s + "/"));
}
try {
  const parsed = JSON.parse(fs.readFileSync(0, "utf8") || "{}");
  const claims = parsed?.data?.room?.active_claims || [];
  const found = claims.some((fact) =>
    fact?.tool === tool && Array.isArray(fact?.scope) && fact.scope.some((scope) => matches(scope, path))
  );
  process.stdout.write(found ? "yes" : "no");
} catch (_) {
  process.stdout.write("no");
}
' "$tool" "$path" ; } 2>/dev/null)"
      if [ "$already_claimed" != "yes" ]; then
        rally_timeout say claim --tool "$tool" --path "$path" --subject "auto-claim $path" --json >/dev/null 2>&1 || true
      fi
    fi
  fi
else
  rally_output="$(rally_timeout next --tool "$tool" --json 2>/dev/null || true)"
fi

# Render the host-specific output envelope from rally's JSON output.
# Without node we can't parse rally JSON — emit nothing (silent no-op).
if [ "$have_node" != "1" ]; then exit 0; fi

# RALLY_HOOK_STRICT=1 → translator may emit deny/block on high-severity signals.
# Default (any other value): force advisory-only.
strict="${RALLY_HOOK_STRICT:-0}"

rally_root="$(find_rally_root 2>/dev/null || pwd)"
printf '%s' "$rally_output" | RALLY_HOOK_STRICT="$strict" RALLY_HOOK_ROOT="$rally_root" RALLY_HOOK_SESSION="$session" node -e '
const fs = require("fs");
const raw = fs.readFileSync(0, "utf8");
const phase = process.argv[1] || "idle";
const tool = process.argv[2] || "claude_code";
const strict = process.env.RALLY_HOOK_STRICT === "1";

function nativeEvent(tool, phase) {
  if (tool === "gemini" || tool.startsWith("gemini")) {
    return {start:"SessionStart", idle:"BeforeAgent", "before-write":"BeforeTool", "after-write":"AfterAgent"}[phase] || "BeforeAgent";
  }
  // Claude Code + Codex use the same event names for our purposes.
  return {start:"SessionStart", idle:"UserPromptSubmit", "before-write":"PreToolUse", "after-write":"Stop"}[phase] || "UserPromptSubmit";
}
function output(value) { process.stdout.write(JSON.stringify(value)); }

let parsed = {};
try { parsed = JSON.parse(raw || "{}"); } catch (_) { output({}); process.exit(0); }

const hook = parsed?.data?.hook || {};
const judgment = hook?.judgment || parsed?.data?.judgment || {};
const check = parsed?.data?.check || {};
const next = parsed?.data?.next || {};
let visible = hook?.agent_visible || judgment?.agent_visible || check?.agent_visible || parsed?.agent_visible || next?.agent_visible || {};

if ((!visible || !visible.present) && next?.actionable) {
  const subject = next?.fact?.subject || next?.reason || "see rally next";
  visible = {
    present: true,
    severity: next?.requires_human ? "stop" : "warn",
    message: `Rally has actionable coordination work: ${next.action}. Subject: ${subject}.`
  };
}

if (!visible.present) { output({}); process.exit(0); }

const event = nativeEvent(tool, phase);
const rawMessage = visible.message || "Rally has a pending coordination obligation.";
const severity = visible.severity || "warn";
const allow = hook.allow ?? judgment.allow ?? check.allow ?? true;
const highSeverity = severity === "stop" || allow === false;

// CHARTER (never-block, default): coordination is recorded + exposed, never
// enforced. Default `stop=false` so we always emit `additionalContext` /
// `systemMessage`. STRICT MODE (RALLY_HOOK_STRICT=1) is the documented
// escape hatch: it lets the high-severity branch emit deny/block. Even in
// strict mode, every emission is also surfaced as a visible message so the
// human/agent sees why.
const stop = strict && highSeverity;
const message = highSeverity
  ? (stop
      ? `⚠️ HIGH-SEVERITY coordination signal (STRICT MODE — BLOCKING): ${rawMessage}`
      : `⚠️ HIGH-SEVERITY coordination signal (advisory — not blocking; rally never enforces): ${rawMessage}`)
  : rawMessage;

// Anti-spam: surface-on-change, not on-poll. On the per-turn phases
// (idle -> UserPromptSubmit, after-write -> Stop) suppress an identical
// surface already shown this session — emit {} (a valid empty hook result)
// so smooth turns stay quiet and only a CHANGED room nudges again. Not
// applied to `start` (fires once/session) or `before-write` (edit-scoped +
// conflict-specific — repetition there is intentional).
if (phase === "idle" || phase === "after-write") {
  try {
    const root = process.env.RALLY_HOOK_ROOT || process.cwd();
    const sess = (process.env.RALLY_HOOK_SESSION || "anon").replace(/[^A-Za-z0-9_.:-]/g, "_");
    const dir = root + "/.rally/.hook-seen";
    const file = dir + "/" + sess + "." + phase + ".seen";
    const key = event + "|" + severity + "|" + rawMessage;
    let h = 5381; for (let i = 0; i < key.length; i++) { h = ((h * 33) ^ key.charCodeAt(i)) >>> 0; }
    const sig = String(h);
    let prev = "";
    try { prev = fs.readFileSync(file, "utf8"); } catch (_) {}
    if (prev === sig) { output({}); process.exit(0); }
    try { fs.mkdirSync(dir, { recursive: true }); fs.writeFileSync(file, sig); } catch (_) {}
  } catch (_) { /* dedup is best-effort; never block surfacing on an FS error */ }
}

if (tool === "gemini" || tool.startsWith("gemini")) {
  if (event === "SessionStart" || event === "BeforeAgent") {
    output({hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "BeforeTool") {
    output(stop ? {decision: "deny", reason: message} : {hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "AfterAgent") {
    output(stop ? {decision: "deny", reason: message} : {systemMessage: message});
  } else {
    output({systemMessage: message});
  }
} else {
  if (event === "SessionStart" || event === "UserPromptSubmit") {
    output({hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "PreToolUse") {
    // Advisory (default): permissionDecision "allow" keeps the edit unblocked
    // while systemMessage GUARANTEES the deconflict warning surfaces to the
    // agent (additionalContext is not reliably injected on PreToolUse). Strict
    // mode is the only path that emits "deny". Verified against the official
    // Claude Code hooks contract (code.claude.com/docs/en/hooks, 2026-06).
    output(stop
      ? {hookSpecificOutput: {hookEventName: event, permissionDecision: "deny", permissionDecisionReason: message}}
      : {hookSpecificOutput: {hookEventName: event, permissionDecision: "allow", permissionDecisionReason: message}, systemMessage: message});
  } else if (event === "Stop") {
    output(stop ? {decision: "block", reason: message} : {systemMessage: message});
  } else {
    output({systemMessage: message});
  }
}
' "$phase" "$tool"

exit 0
