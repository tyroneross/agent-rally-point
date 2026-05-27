# Adapter Visibility Research

Status: research spike complete  
Date: 2026-05-27

## Question

Rally already records obligations in the channel. The failure mode is delivery: a
pending handoff can exist while the addressed agent never sees it in model
context. This spike asks, for each supported surface:

> If Rally emits an obligation message at startup, idle, or before-write, can the
> adapter make the model see it and act on it?

## Finding

Rally should keep the core pull-based, but every adapter needs an
**agent-visible obligation path**. The portable contract should be:

```json
{
  "agent_visible": {
    "present": true,
    "severity": "stop|warn|info",
    "message": "Rally: pending handoff from claude_code: review ...",
    "required_action": "ack_handoff",
    "source_event_ids": ["evt_..."]
  }
}
```

Adapters translate that contract into their native model-visible mechanism.
Do not rely on cursor-scoped inbox reads for obligations; pending handoffs,
blockers, and claim conflicts must be derived from full projected state every
boundary check.

## Surface matrix

| Surface | Startup visible to model | Mid-turn / idle visible to model | Before-write enforcement | Best Rally adapter path | Confidence |
| --- | --- | --- | --- | --- | --- |
| Pi | Yes | Yes | Yes | Pi extension: `before_agent_start` returns persistent `message`; `pi.sendMessage(..., {triggerTurn:true})` can steer/wake; `tool_call` can block writes. | High from local Pi docs |
| Claude Code | Yes | Yes | Yes | Hooks: `SessionStart` / `UserPromptSubmit` / `Stop` / `PreToolUse`. Use `hookSpecificOutput.additionalContext` for model-visible context; use `decision:"block"` on `Stop` to continue with a reason; use `permissionDecision:"deny"` on `PreToolUse` to block writes. | High from Claude docs |
| Codex CLI | Yes | Yes | Partial | Hooks: `SessionStart` and `UserPromptSubmit` add developer context; `Stop` can continue with a reason; `PreToolUse` can return `additionalContext` or deny supported tools. Coverage is incomplete for some shell/tool paths. | Medium-high from Codex docs |
| Gemini CLI | Yes | Yes | Yes | Hooks: `SessionStart` / `BeforeAgent` inject `additionalContext`; `AfterAgent` can retry with `reason`; `BeforeTool` can deny writes with reason sent to agent. | High from Gemini docs |
| cmux | No, not directly by itself | No, not directly by itself | No | cmux is an orchestration/terminal surface. Use it to launch agents with a Rally startup packet, send text to panes, set sidebar status, and notify. It does not itself guarantee model-visible context unless it injects text into the agent process/pane. | Medium from cmux docs |
| Herdr | No, not directly by itself | No, not directly by itself | No | Herdr is an orchestration/terminal surface. Use `agent start` wrappers for startup packet injection, `agent send`/`pane run` for explicit text injection, and `pane.report_agent` for UI state. It does not replace native per-agent hooks. | Medium from Herdr docs |

## Per-surface notes

### Pi

Local Pi docs explicitly support model-visible insertion:

- `before_agent_start` can inject a persistent message stored in the session and
  sent to the LLM.
- `context` can modify messages before each LLM call.
- `pi.sendMessage` can inject custom messages; with `triggerTurn: true`, an idle
  agent can be woken.
- `tool_call` can block writes.

The current Rally Pi extension only runs `rally hook idle` and `rally hook
before-write`; it does not return a `before_agent_start` message or call
`pi.sendMessage`, so obligations may be logged but not model-visible.

Recommended Pi change:

1. On `before_agent_start`, run `rally next --tool pi --json` or `rally hook idle
   --tool pi --json`.
2. If `agent_visible.present`, return:
   ```ts
   return { message: { customType: "rally", content: msg, display: true } };
   ```
3. On async watch/sentinel later, use `pi.sendMessage({ customType:"rally",
   content: msg, display:true }, { triggerTurn:true, deliverAs:"followUp" })`.
4. Keep `tool_call` as the before-write enforcement path.

### Claude Code

Claude Code hooks have several model-visible paths:

- `SessionStart`: stdout or `hookSpecificOutput.additionalContext` is added to
  Claude context before the first prompt.
- `UserPromptSubmit`: stdout or `additionalContext` is added alongside the user
  prompt.
- `PreToolUse`: `permissionDecision:"deny"` reason is shown to Claude; also
  supports `additionalContext`.
- `PostToolUse` / `PostToolBatch`: support `additionalContext`.
- `Stop`: `decision:"block"` with `reason` tells Claude to continue working.

Recommended Claude change:

- Startup and user-prompt hooks return `additionalContext` when Rally has an
  obligation.
- Stop hook returns `decision:"block"` with the Rally message when unresolved
  obligations remain.
- PreToolUse hook denies writes only for hard blockers/conflicts; otherwise use
  `additionalContext`.

### Codex CLI

Codex has a similar hook model but narrower current behavior:

- `SessionStart`: plain stdout or `hookSpecificOutput.additionalContext` becomes
  extra developer context.
- `UserPromptSubmit`: plain stdout or `additionalContext` becomes extra
  developer context.
- `PreToolUse`: can deny Bash/apply_patch/MCP tool calls and can inject
  `additionalContext`, but it does not intercept every possible tool path.
- `PostToolUse`: can inject context and/or replace feedback.
- `Stop`: `decision:"block"` continues the turn with the reason as a new prompt.

Recommended Codex change:

- Use `SessionStart` + `UserPromptSubmit` for agent-visible obligations.
- Use `Stop` to prevent going idle with unresolved handoffs.
- Use `PreToolUse` for best-effort write gates, but keep Rally core aware this is
  not complete enforcement.
- Avoid unsupported fields in `PreToolUse` (`continue`, `stopReason`,
  `suppressOutput`) because current Codex treats them as hook failures.

### Gemini CLI

Gemini hooks provide direct model-visible context paths:

- `SessionStart`: `hookSpecificOutput.additionalContext` is injected as the
  first turn in interactive mode or prepended to the prompt in non-interactive
  mode.
- `BeforeAgent`: `additionalContext` is appended to the prompt for the current
  turn.
- `BeforeTool`: `decision:"deny"` sends the reason to the agent as a tool error.
- `AfterTool`: `additionalContext` appends to the tool result.
- `AfterAgent`: `decision:"deny"` sends `reason` to the agent as a retry prompt.

Recommended Gemini change:

- Use `BeforeAgent` as the primary per-turn visibility hook.
- Use `AfterAgent` to keep working if pending obligations remain.
- Use `BeforeTool` to block unsafe writes.

### cmux

cmux gives process/pane/notification controls, not a universal agent-context
API. It can:

- send text/keys to a terminal surface;
- create desktop/feed notifications;
- set sidebar status, progress, and logs;
- run notification hooks.

That makes cmux useful for operator visibility and startup wrappers, but not a
substitute for native agent hooks. If cmux injects text into a live terminal, it
is essentially typing into the agent UI; that may be correct for a deliberate
watch daemon, but should not be the default silent behavior.

Recommended cmux change:

- On launch, prepend/submit a Rally startup packet through the agent command or
  pane input.
- For live events, notify/sidebar by default; only send text into an agent pane
  when configured with an explicit `inject_agent_input` option.

### Herdr

Herdr similarly exposes orchestration primitives:

- `agent start` launches named agent targets;
- `agent send` writes literal text to a resolved agent terminal;
- `pane.report_agent` reports state/status to UI;
- event subscriptions can drive a watch loop.

Herdr can deliver visible text by sending input to an agent pane, but that is a
terminal injection policy decision, not a native model-context guarantee.

Recommended Herdr change:

- Startup wrapper should run `rally <tool>` and inject/prepend the packet before
  or as the first prompt.
- Watcher may use `agent send` only when configured and when Rally trust says
  automation is allowed.
- Always report `blocked`/`working` state via `pane.report_agent` for operator
  visibility.

## Contract implications for Rally

Add a single helper that turns `rally next` / `rally hook` decisions into a short
agent-visible brief:

```text
Rally obligation: pending handoff from claude_code
Subject: review of rally next (88ee57a): ship + sharpen
Required action: inspect evt_d22... and respond with ack / needs-info / reject.
```

Recommended JSON placement:

- `rally next`: `data.next.agent_visible`
- `rally hook`: `data.hook.agent_visible`
- `rally start`: `agent_visible` at top level plus `context.brief...`

Minimum fields:

```json
{
  "present": true,
  "severity": "stop",
  "message": "...short text...",
  "required_action": "ack_handoff",
  "source_event_ids": ["evt_..."],
  "automation_allowed": false
}
```

Rules:

1. Full projected obligations, not cursor-only inbox, decide `present`.
2. Cursor-scoped reads remain for “what changed since I last checked”.
3. Adapter output must use model-visible fields where available.
4. Terminal/pane injection requires explicit opt-in and trust approval.
5. Blocking hooks should be used only for stop-worthy obligations; otherwise
   inject context and let the model continue.

## Next implementation slice

1. Implement the Rally `agent_visible` helper and add fields to `next`, `hook`,
   and `start` JSON schemas/goldens.
2. Update Pi extension to return a `before_agent_start` message when
   `agent_visible.present` and keep `tool_call` blocking for writes.
3. Update Claude/Codex/Gemini hook scripts to translate `agent_visible` into
   each native `additionalContext` / continuation / deny shape.
4. Update cmux/Herdr adapters to distinguish operator visibility from explicit
   agent input injection.
5. Add a small fixture test per script shape: given a synthetic Rally obligation,
   the adapter emits the expected native hook JSON.
