<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Any-Agent Onboarding Contract

This is the minimal runtime contract for any LLM, coding agent, terminal
assistant, CI worker, or editor-hosted model that joins a repo using Rally.

Rally coordination is host-agnostic. First-class launch/injection support is
not. A tool can participate in the room as soon as it can run the `rally` CLI;
managed stdin injection only works when the session is launched or adopted by a
Rally-aware runtime.

## Contract

Every agent must do these steps before editing:

```bash
rally whoami --tool <stable-tool-id> --json
rally enter --tool <stable-tool-id> --json
rally ack --tool <stable-tool-id>
rally next --tool <stable-tool-id> --json
rally room --json
```

Then, before touching a shared file:

```bash
# Save the claim response's event_id as <claim-id>.
rally say claim --tool <stable-tool-id> --subject "<work lane>" --path <path> --json
rally check before-write --tool <stable-tool-id> --path <path> --strict --json
```

If the strict check returns exit 4, do not edit. Coordinate with the holder or
release `<claim-id>` before switching work; do not automatically release a
claim for an unrelated command failure.

When work finishes:

```bash
rally say artifact --tool <stable-tool-id> \
  --subject "<what changed>" \
  --evidence "<commands or checks run>" \
  --json

rally say release --tool <stable-tool-id> --ref <claim-id> --subject "done" --json
rally next --tool <stable-tool-id> --json
```

Use `rally say blocker`, `rally say handoff`, `rally say decision`, and
`rally say resolve` for durable coordination. Do not rely on chat scrollback as
the source of truth.

## Parallel / fan-out work

When a job splits into independent pieces — review from N perspectives, the same
change across many files, one task per route or dimension — do not improvise the
split. Route it through the **rally-workflows** skill
(`skills/rally-workflows/SKILL.md`): author a JSON *workstream descriptor* (one
task each with `id` / `intent` / `owns` / `validation` / `output`), lint it with
`node dynamic-workflows/core/workstream-lint.mjs <descriptor>.json` to prove the
write boundaries are disjoint (exit 0 = safe), then generate a ready-to-paste
prompt per task with `node dynamic-workflows/core/packet.mjs <descriptor>.json
--run <run_id>`. `references/decomposition.md` is the host-neutral procedure for
turning a vague goal into that descriptor. Rally records and lints the fan-out;
your host's own spawn mechanism runs the agents.

Resolve how many to run at once with `resolveFanout()` from
`dynamic-workflows/core/fanout.mjs` rather than picking a number — it returns the
width (default 10, hard ceiling 12) alongside the `limiting_factors` that produced
it. Pass your own resource ceiling as `hostCap`; Rally never spawns, so it has no
model, token, or CPU picture of its own.

## Tool Ids

Use a stable id that names the runtime and, when needed, the lane:

| Runtime | Recommended id |
|---|---|
| Claude Code | `claude_code` or `claude_code:<lane>` |
| Codex CLI | `codex` or `codex:<lane>` |
| Gemini CLI | `gemini` or `gemini:<lane>` |
| OpenCode | `opencode` or `opencode:<lane>` |
| Cursor agent | `cursor` or `cursor:<lane>` |
| Qwen-based CLI | `qwen` or `qwen:<lane>` |
| Gemma-based CLI | `gemma` or `gemma:<lane>` |
| Aider | `aider` or `aider:<lane>` |
| CI/automation | `ci` or `ci:<job>` |

The exact string is less important than consistency. Peers target that id in
handoffs, claims, and resolution facts.

## Managed vs Manual Sessions

There are two integration levels.

### Managed session

A managed session is started or adopted by Rally-aware infrastructure. It has a
session row in `rally sessions --json`, can be captured, and can usually receive
direct stdin injection.

Current first-class launch targets:

```bash
rally run claude --name <lane> --json
rally run codex --name <lane> --json
rally run opencode --name <lane> --json
rally run gemini --name <lane> --json
```

For managed targets, the injection contract is:

1. Post a durable targeted fact first:

   ```bash
   rally say handoff --tool <sender> \
     --target <target-tool-id> \
     --subject "<action needed>" \
     --summary "<short context>" \
     --json
   ```

2. Confirm the target is managed and live:

   ```bash
   rally sessions --json
   ```

3. Inject only after the session exists:

   ```bash
   rally inject <session|name|tool> --handoff <event-id> --json
   ```

The durable Rally fact is the contract. Injection is only delivery. If injection
fails or the agent does not acknowledge receipt, continue from the Rally fact
and use capture/attach/manual paste as a fallback.

### Manual or generic session

A manual session is any CLI or editor agent that can run commands but is not
listed by `rally sessions --json`. Examples: Cursor agent, Qwen CLI, Gemma CLI,
Aider, an IDE plugin, or a model running in another terminal.

Manual sessions still fully participate in Rally:

```bash
rally whoami --tool qwen:reviewer --json
rally enter --tool qwen:reviewer --json
rally ack --tool qwen:reviewer
rally next --tool qwen:reviewer --json
```

They are not direct-injectable until a wrapper, Easy Terminal pane, tmux
backend, or future custom backend registers them as a managed session. For
manual sessions, use targeted `handoff` facts and paste the bootstrap prompt
below into the agent's chat/input surface.

## Injectable Or Heartbeat Requirement

Every active agent must be observable through one of two paths:

- **Injectable:** a managed or adopted session appears in `rally sessions --json`
  and can receive `rally inject`, `rally capture`, and `rally stop`.
- **Heartbeat:** a manual or generic session that is not injectable must keep a
  Rally cadence. Run `rally next --tool <stable-tool-id> --json` before work,
  before edits, before the final response, and after any long gap. For idle
  monitoring, run `rally watch --tool <stable-tool-id> --once --json` from a
  scheduler or `rally watch --tool <stable-tool-id> --interval 5
  --max-interval 300 --json` as an attached watcher.

Not communicating is a coordination failure. If a session cannot be injected
and cannot maintain a heartbeat, post a `blocker` or stand down instead of
continuing invisible work.

Use the backlog as the plan/status bus when work has an owner or timeline:

```bash
rally backlog add --tool <you> --id <id> --intent "<work>" --target <owner-tool> --status planned --expected-by "<when>" --json
rally backlog update --tool <owner-tool> --id <id> --status in_progress --expected-by "<next checkpoint>" --json
```

`rally next --tool <owner-tool> --json` surfaces targeted `open`, `planned`,
or `blocked` backlog items as `update_plan_status` obligations until the owner
updates them. A plan that is only in chat is not durable coordination.

## Bootstrap Prompt

Use this when starting a generic agent that does not automatically read
`AGENTS.md`, `CLAUDE.md`, or the Rally docs:

```text
You are joining this repo as a Rally participant.

Before editing, run:
  rally whoami --tool <your-stable-tool-id> --json
  rally enter --tool <your-stable-tool-id> --json
  rally ack --tool <your-stable-tool-id>
  rally next --tool <your-stable-tool-id> --json
  rally room --json

Treat Rally as the coordination source of truth. Use live ids from whoami,
next, room, and explicit handoffs. Do not copy tool/session ids from examples.

Before writing a file, run:
  rally check before-write --tool <your-stable-tool-id> --path <path> --strict --json
  rally say claim --tool <your-stable-tool-id> --subject "<work lane>" --path <path> --json

After work, verify it, post an artifact with evidence, release your claim, and
run rally next again. If blocked, post rally say blocker with the concrete
blocking condition.
```

## Easy Terminal Runtime Contract

Easy Terminal can host multiple agent CLIs through ptyd. In that environment,
the app owns panes and stdin delivery; Rally owns durable coordination truth.

The mapping should stay simple:

| Layer | Responsibility |
|---|---|
| ptyd / terminal host | pane identity, stdin delivery, output capture, process liveness |
| Rally ledger | handoffs, claims, decisions, blockers, artifacts, ack/receipt facts |
| Agent CLI | reads bootstrap/context, runs Rally commands, performs work, reports evidence |
| Easy Terminal UI | shows panes, status, inputs/outputs, and coordination projection |

Direct input into a terminal pane must not be treated as proof that an agent
understood the assignment. The reliable proof is a Rally fact from the receiving
agent: `ack`, `resolve`, `artifact`, `blocker`, or another targeted response.

## What Is Not Automatic Yet

These are product backlog items, not assumptions agents should rely on today:

- `rally onboarding --tool <id> --json|--text` to emit this contract as a
  machine-readable bootstrap packet.
- `rally runtime-contract --json` to expose the coordination/injection contract
  to host apps.
- `rally run custom --tool <id> --cmd <command>` or equivalent adoption support
  for Cursor/Qwen/Gemma/Aider-style CLIs.
- Easy Terminal "Agent Bootstrap" UI for copying/injecting the generic prompt
  into any non-first-class agent runtime.

Until those exist, the portable contract is this document plus the core Rally
commands above.
