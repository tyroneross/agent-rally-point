<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Any-Agent Onboarding Contract

This is the minimal runtime contract for any LLM, coding agent, terminal
assistant, CI worker, or editor-hosted model that joins a repo using Rally.

Rally coordination is host-agnostic. First-class launch/injection support is
not. A tool can participate in the room as soon as it can run the `rally` CLI;
managed stdin injection only works when the session is launched or adopted by a
Rally-aware runtime.

## Contract

Every host must establish one lease in the long-lived parent process before it
starts short-lived Rally commands. Reuse an existing `RALLY_SESSION_ID`; mint a
new one only for a genuinely new agent session:

```bash
eval "$(rally session ensure \
  --tool <stable-tool-id> \
  --adapter <host-family> \
  --json | jq -r '.data.session.shell_export')"
```

An adapter adds `--native-hook`, `--strict`, `--lifecycle-close`, or
`--live-delivery` only after it has wired that behavior. The response reports
each guarantee as `enforced`, `advisory`, or `unmanaged`. In particular, Codex
write blocking remains `advisory` because its current hook output cannot deny a
tool call; visibility and atomic claim acquisition remain separate guarantees.
When a second fresh lease appears, `session ensure` starts the per-repo
single-writer daemon idempotently and reports the result under `daemon`.
Set `RALLY_DAEMON_AUTOSTART=0` in the parent only when the host deliberately
owns daemon lifecycle itself.

Any agent can inspect the bounded live control plane without expanding room
history:

```bash
rally session current --json
rally session history --limit 20 --json
```

`current` caps rows at 128 and reports `total`, `emitted`, and `omitted` plus
fresh/stale/unknown counts. Its `window_secs` is derived from the effective
coordination cadence, miss multiplier, and grace settings. `history` is an
explicit, separately bounded view.

Every agent then does these steps before editing:

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

At real process/session exit, an integrated adapter must run:

```bash
rally session close --tool <stable-tool-id> --json
```

`session close` releases only claims whose authoring `tool` and
`from_session_id` both match the closing lease, across every engagement. It
also requires the one-time `RALLY_SESSION_CLOSE_TOKEN` exported by `session
ensure`; Rally stores only its hash while the lease is active. After close,
Rally rejects every later fact carrying that exact tool/session identity; a
new parent lease is required for more work. Never bind close
to a per-turn `Stop` callback: one conversation turn ending is not the agent
session ending. The token prevents a sibling process that only knows the lease
id from closing it; it does not protect against a same-UID process that can read
the parent environment or write `.rally/` directly.

`enter`, `next`, and `inbox` retrieve obligations with a bounded target-scoped
read. Totals remain exact even when rows are omitted, and `stale_wait_secs`
only annotates/de-prioritizes old work; it never closes receiver-owned work.

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
rally run codex --name <lane> --task "<bounded assignment>" --json
rally run opencode --name <lane> --json
rally run gemini --name <lane> --json
```

Bounded Codex work should use `--task`: Rally passes the prompt to the long-lived
child over stdin, captures the final response, and closes the managed session
automatically. It removes a clean worktree or reports the recovery path for a
dirty one. Private result files under `.rally/task-results/` remain until the
operator archives or deletes them; Rally applies no automatic retention window.
The invoking shell may retain the `--task` command in its history.
Use plain `rally run codex` only for a deliberately persistent session that
will receive later injections, then stop it explicitly.

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

3. For a deliberately persistent session, inject only after it exists:

   ```bash
   rally inject <session|name|tool> --handoff <event-id> --json
   ```

The durable Rally fact is the contract. Injection is only delivery. If injection
fails or the agent does not acknowledge receipt, continue from the Rally fact
and use capture/attach/manual paste as a fallback.

### Direct messages versus directives

`rally inject` defaults to `--intent directive`. A directive may control the
recipient and therefore still requires one of Rally's control bases: the sender
holds the lead seat, sender and target are the same logical tool, the target
opened a handoff to the sender, or the room is leaderless during bootstrap.
Always pass `--tool <sender>` so Rally can enforce that gate. For compatibility,
an omitted sender identity is still delivered, but it receives no authority:
the recipient sees `sender=(none stated)`, `control-attempt=yes`, and
`authority=unverified`. Treat that message as an unauthenticated directive, not
as lead or target-consent proof.

Any identified sender may deliver non-controlling context directly; the frame
states whether Rally observed it as lead, participant, or unjoined:

```bash
rally inject <session|name|tool> \
  --tool <sender> \
  --intent inform|request|propose \
  --responsibility investigator|planner|implementer|verifier|reviewer|integrator|operator \
  --text "<message>" \
  --json
```

The receiving turn begins with one Rally-authored `RALLY MESSAGE FRAME`. Run
`rally help frame` anywhere, including outside a Rally repo, to decode it. A
non-controlling turn then includes a separate Rally-authored `RALLY RECEIVER
RULE` before the sender-authored payload. The rule tells the recipient that it
decides whether to use, accept, refuse, or ignore the message and that the
message does not replace its goal. `--urgent` is invalid for non-controlling
intent.

#### Canonical message-frame glossary

This table defines the runtime frame contract. Keep its field labels aligned
with `rally help frame` and the renderer.

| Field | Source and assurance | Receiver behavior | Unknown or default |
|---|---|---|---|
| `sender` | `--tool` claim; unverified | Identifies the claimed author; never grants authority. | `(none stated)` |
| `intent` | Sender declaration; `directive` when omitted | `inform`, `request`, and `propose` are receiver-decided; `directive` tries to control. | Unknown fails closed as controlling. |
| `control-attempt` | Derived from `intent` | `no` means the payload cannot replace the recipient's goal; `yes` means evaluate `authority` before obeying. | `yes` for unknown intent. |
| `sender-type` | Inferred from the claimed sender id | Context only; never grants authority. | `unknown` |
| `room-position` | Room snapshot observed for the claimed sender and lead decision | Reports `lead`, `participant`, `unjoined`, or `unknown`; status alone is not command authority. | `unknown` when room cannot be read. |
| `responsibility` | Sender-provided category, or `unspecified` when omitted; unverified and unscoped | Describes duty only; grants neither work scope nor authority. | `unspecified` or `unknown` |
| `authority` | Derived by Rally's send gate for the claimed sender | Records why a control attempt was allowed. `not-required` applies only to non-controlling intent. | `unverified` is compatibility evidence, not proof. |
| `guide` | Rally literal | Points to the stable decoder. | `rally help frame` |

`control-attempt=yes` describes message intent, not authorization. A receiver
must use the separate `authority` basis. `responsibility` is an unverified category,
not a task/claim scope and not an authority grant. Full caller-session and typed
message metadata remain in durable directive and JSON output even though the
compact visible frame omits caller-session.

Do not store `peer` as a role; new `enter` and `say` writes reject it. Peer is a relationship derived from the viewer.
Rally stores independent axes: room seat, work-responsibility category, message
intent, actor kind, and authority basis.

### Manual or generic session

A manual session is any CLI or editor agent that can run commands but is not
listed by `rally sessions --json`. Examples: Cursor agent, Qwen CLI, Gemma CLI,
Aider, an IDE plugin, or a model running in another terminal.

Manual sessions still participate in Rally. Establish their lease in the
parent shell, then enter the room:

```bash
eval "$(rally session ensure --tool qwen:reviewer --adapter qwen --json | jq -r '.data.session.shell_export')"
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
  eval "$(rally session ensure --tool <your-stable-tool-id> --adapter <host-family> --json | jq -r '.data.session.shell_export')"
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
run rally next again. On real agent-session exit, run rally session close. If
blocked, post rally say blocker with the concrete blocking condition.
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

- `rally onboarding --tool <id> --json|--text` <!-- conformance:planned -->
  to emit this contract as a machine-readable bootstrap packet.
- `rally runtime-contract --json` <!-- conformance:planned -->
  to expose the coordination/injection contract to host apps.
- `rally run custom --tool <id> --cmd <command>` or equivalent adoption support
  for Cursor/Qwen/Gemma/Aider-style CLIs.
- Easy Terminal "Agent Bootstrap" UI for copying/injecting the generic prompt
  into any non-first-class agent runtime.

Until those exist, the portable contract is this document plus the core Rally
commands above.
