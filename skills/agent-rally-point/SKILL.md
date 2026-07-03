---
name: agent-rally-point
description: Use when working in a repository that uses Rally/Agent Rally Point for cross-agent coordination, especially at session start, before editing files, when deciding what to do next, handing work to another agent, recording facts/artifacts/decisions, resolving blockers, or coordinating with other coding agents through the `rally` CLI.
---

# Agent Rally Point

Use `rally` as the live source of coordination truth.

## Session Start

From inside the repo, enter with a Rally id that identifies the working
agent/session, not just the host family. The host family is metadata
(`codex`, `claude_code`, `cursor`, etc.); the agent id is a unique string or
number for this terminal/session/worker. Every concurrently working agent gets
its own id. `--session-id` is not a substitute because Rally routes handoffs,
claims, cursors, and presence by `--tool`.

```bash
HOST="${RALLY_HOST_FAMILY:-codex}"  # use claude_code, cursor, gemini, etc. as needed
if [ -z "${RALLY_AGENT_ID:-}" ]; then
  RALLY_AGENT_ID="$(uuidgen 2>/dev/null || printf 'agent-%s' "$$")"
  RALLY_AGENT_ID="$(printf '%s' "$RALLY_AGENT_ID" | tr '[:upper:]' '[:lower:]')"
  export RALLY_AGENT_ID
fi
TOOL="${RALLY_TOOL_ID:-$HOST:$RALLY_AGENT_ID}"
rally enter --tool "$TOOL" --json
rally next --tool "$TOOL" --json
```

If `rally enter` warns `duplicate-active-squad-id`, stop using that tool id in
this terminal and re-enter with a distinct `RALLY_AGENT_ID` / `--tool`. If you
spawn another agent or worker that will post Rally facts, give it a separate
agent id; do not let it reuse the parent terminal's id.

## Live Agent Status

Every working agent must keep its status current so peers can tell who is
working, what they are working on, whether they are idle/blocked/done, and when
they will check in again. Automatic hooks post status for start, before-write,
idle, and stop phases; manual agents should use the same host-neutral commands.

```bash
rally status post --tool "$TOOL" --state working --file <path> --intent "<one-line>"
rally status post --tool "$TOOL" --state idle --wake-after <iso-8601>
rally status post --tool "$TOOL" --state blocked --blocked-ref <event-id>
rally status post --tool "$TOOL" --state done
rally status read --json
```

Treat `rally status read --json` as the current roster before coordinating or
joining shared work. Ignore `stale:true` entries for live ownership decisions
unless you are explicitly cleaning up abandoned work.

Read `next` before broad repo exploration:

- `actionable`: whether the recommendation can become work.
- `requires_human`: whether to stop and ask.
- `stop_reason`: why autonomous action should stop.
- `suggested_claims`: claim commands to reserve work.
- `suggested_commands`: checks and completion fact templates.
- `completion`: what durable fact is expected after work.

If `actionable` is false, do not invent work from Rally state. If
`requires_human` is true, ask the user.

## Core Workflow

1. **Claim before shared edits** when `next` recommends work or when the file is
   likely to overlap with another agent:

```bash
rally say claim --tool "$TOOL" --subject "edit shared file" --path <path> --json
```

2. **Check before writing**:

```bash
rally check before-write --tool "$TOOL" --path <path> --strict --json
```

If the check returns blocking findings, stop and resolve them before editing.

3. **Record meaningful outputs**:

```bash
rally say artifact --tool "$TOOL" --subject "implemented change" --uri <path> --evidence "<verification>" --json
```

4. **Record coordination facts**:

```bash
rally say handoff --tool "$TOOL" --target <other-tool> --subject "review this" --summary "<context>" --json
rally say blocker --tool "$TOOL" --subject "need decision" --severity high --json
rally say resolve --tool "$TOOL" --ref <blocker-id> --subject "resolved" --json
rally say decision --tool "$TOOL" --subject "binding decision" --status binding --json
rally say release --tool "$TOOL" --ref <claim-id> --subject "done" --json
```

5. **Loop back**:

```bash
rally next --tool "$TOOL" --json
```

Continue only while the next action is actionable, safe, and inside the user's
scope.

## Managed Sessions

Use managed sessions for reliable live delivery into visible panes:

```bash
rally run claude --backend tmux --json
rally inject <session|name|tool> --handoff <event-id> --json
rally capture <session|name|tool> --json
```

Rally does not keep agents awake by itself. Treat `rally next --tool "$TOOL"
--json` as the wake-intent check and `rally inject ... --handoff <event-id>` as
the focused delivery path for managed sessions. Host adapters decide whether to
use native wake, prompt injection, pane notification, resume-only context, or CI
policy.

Watchers must stay narrow: they may detect a transition and notify or inject
through the host's native mechanism, but they must not edit files, resolve
blockers, publish facts on behalf of an agent, or behave like hidden
schedulers.

For Herdr-managed panes, submit injected text with Herdr's `Enter` key, not
tmux-style `C-m`. Full-length payloads can collapse behind `[Pasted Content]`;
submit those with two Enters, where the first expands and the second submits.
Short inline nudges need one Enter. After installing Herdr's Claude/Codex
integrations, restart the agent session before treating Herdr `agent_status` as
authoritative. Even post-restart, use a Rally channel post as the strongest
confirmation that the woken agent acted on the handoff.

When delegating work from inside herdr, keep the user's main tab clean. Start
new helper agents in the workspace's `agents` tab whenever one exists. Discover
the tab with `herdr tab list`, then start the agent with `herdr agent start
... --tab <agents-tab-id> --no-focus -- ...` or use a Rally backend option that
targets that tab when available. Only place helper agents in the active tab when
the user explicitly asks for that.

Agents should call `rally check before-write` explicitly before shared edits.
Rally does not install host hooks or prompt injection glue.

## Judgment Rule

Rally recommends and constrains work; it does not replace judgment.

- If a fact affects files, shells, editors, credentials, or another agent,
  inspect source event ids and evidence before acting.

## Multi-Agent Fan-Out (Rally Flow)

To coordinate *several* agents on one objective — fan out parallel subagents, run a dynamic
workflow, or split a workstream across hosts — use **Rally Flow** (the `dynamic-workflows/` module).
It adds a lint-checked workstream descriptor (MECE write boundaries + determinism) on top of the
rally loop above. One host-neutral entry point — the same skill for every host; you supply your own
`--tool` value at runtime:

- [`rally-workflows`](../rally-workflows/SKILL.md) — workstream descriptor + lint + the two fan-out tiers + the per-task rally loop.
- Canonical wire spec it references: [`dynamic-workflows/PROTOCOL.md`](../../dynamic-workflows/PROTOCOL.md).

## Finish Work Cleanly

Before ending a session:

```bash
rally room --json
rally next --tool "$TOOL" --json
rally say release --tool "$TOOL" --ref <claim-id> --subject "done" --json
```

If something remains for another agent, leave a `handoff` with enough context
and source-linked artifacts.
