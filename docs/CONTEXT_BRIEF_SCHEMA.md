<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Context Brief Contract

## Session Start Contract

Known agent harnesses start from a tool-named command:

```bash
rally pi
rally claude
rally codex
```

Custom tools use the generic form:

```bash
rally start <tool>
```

`start` defaults to JSON because it is agent-facing. Use `--human` for text.
It does not launch the harness process; it starts/refreshes Rally coordination
for that tool in the current repo. The envelope is:

```json
{
  "ok": true,
  "command": "start",
  "schema": "agent-rally.command.start.v1",
  "tool": "pi",
  "session_id": "...",
  "started_process": false,
  "preflight": {},
  "context": { "brief": {} },
  "packet": {},
  "checkpoint": {},
  "cursor": {
    "before": 12,
    "after": 42,
    "max_seq": 42,
    "unseen_count": 30,
    "advanced": true
  },
  "warnings": [],
  "next_commands": {
    "watch": "rally watch --tool pi --session-id ... --since-cursor"
  }
}
```

By default `start` advances the per-session cursor to the current tail after
returning the full context and packet. This makes the returned `watch` command
stream future changes instead of replaying everything the startup brief already
summarized. Pass `--peek` to inspect startup state without advancing the cursor.

`rally context --tool <agent> --json` is the first attuned coordination surface.
It is designed for agents, not prose readers.

The output envelope follows the standard query command shape:

```json
{
  "ok": true,
  "command": "context",
  "schema": "agent-rally.command.context.v1",
  "channel": "/Users/me/.agent-rally-point/apps/repo",
  "data": {
    "brief": {}
  }
}
```

The `brief` object is a bounded, source-linked view over `TraceProjection`.
Current fields:

| Field | Meaning |
|---|---|
| `tool` | Agent/tool id the brief is ranked for. |
| `profile` | Latest `profile` event for the tool, when present, including optional `role`. |
| `subscription` | Latest `subscription` event for the tool, when present. |
| `routing` | Agent-start action and reason. |
| `top_priority` | Highest-priority attention item, if any. |
| `attuned_items` | Scored, explainable relevance ranking for this tool across attention items, claims, artifacts, decisions, lessons, and recent changes. |
| `recommended_next_action` | Machine-readable action, target, confidence, minimum automation trust, reason, and source ids. |
| `needs_attention` | Ranked attention items such as handoffs, tasks, blockers, and claim conflicts. |
| `collision_risk` | Claim conflicts involving this tool. |
| `active_tasks` | Open tasks assigned to this tool. |
| `active_claims` | Active claims owned by this tool. |
| `active_blockers` | Unresolved blockers raised by this tool. |
| `artifacts` | Recent structured artifacts. |
| `decisions` | Recent structured decisions. |
| `lessons` | Recent source-linked lessons. |
| `relevant_changes` | Recent trace changes with origin/trust labels. |

`needs_attention` items include:

```json
{
  "kind": "handoff",
  "priority": 100,
  "event_id": "evt_...",
  "subject": "review sync",
  "reason": "assigned to this tool and requires acknowledgement",
  "source_event_ids": ["evt_..."],
  "paths": ["crates/rally-core/src/sync.rs"],
  "origin": "import:sync",
  "trust_status": "trusted"
}
```

Current priority bands:

| Priority | Kind |
|---:|---|
| 100 | required handoff assigned to this tool |
| 90 | active task assigned to this tool |
| 80 | unresolved blocker raised by this tool |
| 70 | claim conflict involving this tool |

`attuned_items` explain why each item is relevant:

```json
{
  "kind": "artifact",
  "event_id": "evt_...",
  "subject": "context ranking notes",
  "score": 125,
  "factors": [
    "artifact:notes",
    "current_task:evt_task",
    "subscribed_task:evt_task",
    "profile_watch:crates/rally-core",
    "subscribed_path:crates/rally-core/src/context.rs",
    "subscribed_kind:artifact",
    "trusted"
  ],
  "source_event_ids": ["evt_..."],
  "paths": ["crates/rally-core/src/context.rs"],
  "linked_task_ids": ["evt_task"],
  "origin": "remote:peer-a",
  "trust_status": "trusted"
}
```

Attunement scoring is deterministic and source-linked. It combines:

| Signal | Examples |
|---|---|
| Unresolved state | Required handoffs, active tasks, blockers, claim conflicts. |
| Agent profile | `current_task` and watched paths. |
| Specialization | Profile `role` or role-like capabilities such as `review`, `architecture`, `qa`, or `implementation`. |
| Subscriptions | Event kinds, paths, threads, and task ids. |
| Active ownership | Paths already claimed by this tool. |
| Trust | Trusted imports rank up; untrusted/invalid facts rank down. |
| Recency | Fresh recent changes get a small boost. |

Task lifecycle is projected from the latest task event for the same
`owner_tool` and `subject`. A later `status: done`, `completed`, or `cancelled`
event removes that task from `active_tasks` and from unresolved attention.

The ranking is intentionally bounded rather than a scheduler. It does not
execute work or hide lower-scoring facts; it gives agents a compact, explainable
brief they can act on:

> Every recommendation must cite source events.

Recommendations also carry `minimum_trust_for_automation`. Agents may display
lower-trust facts, but bridge adapters must satisfy that threshold before they
act on the recommendation automatically.

Recommendations also include a `trust` object so agents do not have to infer
automation safety from prose:

```json
{
  "action": "ack_handoff",
  "target": "evt_...",
  "minimum_trust_for_automation": "trusted",
  "trust": {
    "required": "trusted",
    "automation_allowed": false,
    "source_statuses": [
      {
        "event_id": "evt_...",
        "origin": "local",
        "trust_status": "local"
      }
    ]
  }
}
```

## Work Packet Contract

`rally packet --tool <agent> --json` returns a bounded, role-shaped work brief
derived from the same `ContextBrief`. It is read-only and does not assign work.

Envelope:

```json
{
  "ok": true,
  "command": "packet",
  "schema": "agent-rally.command.packet.v1",
  "channel": "/Users/me/.agent-rally-point/apps/repo",
  "data": {
    "packet": {}
  }
}
```

Common packet fields:

| Field | Meaning |
|---|---|
| `tool` | Agent/tool id the packet is shaped for. |
| `role` | Canonical role: `reviewer`, `builder`, `architect`, `qa`, or `general`. |
| `packet_kind` | Role-oriented kind: `review`, `build`, `architecture`, `verification`, or `general`. |
| `recommended_next_action` | Same action contract as `context`, including trust assessment. |
| `trust_summary` | Counts for trusted/local/unsigned/untrusted/invalid/unknown focus items plus recommendation automation safety. |
| `source_event_ids` | Source ids used to assemble the packet. |
| `focus` | Top bounded `attuned_items`. |
| `files` | Deduplicated file/path hints from focus, attention items, and active claims. |
| `test_commands` | Verification commands from active tasks. |
| `trust_risks` | Focus items with unsigned, untrusted, invalid, conflict, or unknown trust. |

Role-specific fields are omitted when empty:

| Role | Fields |
|---|---|
| reviewer | `review_targets`, `artifacts`, `decisions`, `lessons`, `risk_areas` |
| builder | `build_targets`, `active_tasks`, `active_claims`, `active_blockers`, `collision_risk`, `decisions` |
| architect | `architecture_targets`, `decisions`, `lessons`, `artifacts`, `open_tradeoffs` |
| qa | `verification_targets`, `artifacts`, `lessons`, `test_commands`, `risk_areas`, `collision_risk` |
| general | `focus`, `recommended_next_action`, `trust_summary`, `source_event_ids` |

The packet exists so specialized agents can start from one compact JSON object
instead of re-filtering the full context brief. It is still derived state over
the trace, not a scheduler or workflow framework.

Packets are intentionally smaller than full context. They preserve full
`AttunedItem` metadata for the bounded `focus` set and role-specific target
lists, but they are a curated projection. Agents that need every recent fact or
all attention categories should read `rally context` first.

## Herdr Injection Gate

`rally herdr inject --json <handoff-id>` is a safety gate for future Herdr
adapters. It surfaces the handoff trust state and refuses unsigned/untrusted
input unless explicitly overridden with `--force`. The command emits gate data;
actual editor/terminal injection remains an adapter concern.

Adapters that consume this output must honor `ready_to_inject: false` unless the
operator supplies an explicit override equivalent to `--force`.

## Adapter Packet Contract

Adapters consume Rally JSON; they do not reinterpret `changes.jsonl` directly.
The shared contract is available through:

```bash
rally adapter contract --json
```

Current side-effect-free adapter packet exports:

```bash
rally cmux packet --tool codex-reviewer --json
rally herdr packet --tool codex-reviewer --json
```

Both commands wrap `rally packet` output with adapter-specific metadata. The
cmux envelope includes a `work_item` suitable for workspace/feed surfaces plus
suggested cmux commands. The Herdr envelope includes a prompt payload and
`ready_to_inject` derived from `recommended_next_action.trust.automation_allowed`.

Adapter rules:

- Read `data.packet.recommended_next_action.trust` before acting.
- Treat `ready_to_inject: false` as a hard stop unless the operator explicitly
  overrides.
- Preserve `source_event_ids` when displaying or forwarding packet contents.
- Keep adapter side effects outside Rally core; these commands are JSON exports.

## Setup and Doctor Contracts

`rally setup --json` discovers known harnesses and returns their availability
plus canonical startup commands. `rally setup enforcement <off|warn|strict>`
records how strongly anonymous coordination should be treated. `rally setup
install <cmux|herdr>` writes adapter notes under the channel directory and
installs edge hooks in the harness config directory:

- cmux: `~/.config/cmux/rally-agent-wrapper.sh` plus a `rally-agent` command in
  `~/.config/cmux/cmux.json`.
- Herdr: `~/.config/herdr/integrations/rally-agent-start.sh` plus a marked
  `[integrations.rally]` block in `~/.config/herdr/config.toml`.

Tests may override those locations with `RALLY_CMUX_CONFIG_DIR` and
`RALLY_HERDR_CONFIG_DIR`.

`rally setup install <pi|claude|codex|gemini>` uses each tool's native hook
surface rather than PATH shadowing:

- Pi: `~/.pi/agent/extensions/rally-judgment.ts` via Pi's extension API.
- Claude: `~/.claude/settings.json` hook entries plus
  `~/.claude/hooks/rally-hook.sh`.
- Codex: `~/.codex/hooks.json`, `~/.codex/config.toml` hook enablement, and
  `~/.codex/rally-hook.sh`.
- Gemini: `~/.gemini/settings.json` hook entries plus `~/.gemini/rally-hook.sh`.

`rally setup uninstall <tool>` removes generated Rally hooks or marked adapter
config blocks.

Safety rules for setup mutation:

- `--dry-run` returns planned files/config without writing.
- Existing files are copied to `<file>.rally.bak` before mutation/removal.
- `rally setup verify [tool] --json` checks expected files and markers.
- Install/uninstall are intended to be idempotent: generated hook entries are not
  duplicated on repeat installs.

`rally doctor --tool <tool> --json` combines deterministic diagnosis,
checkpoint status, setup enforcement, active anonymous claims/tasks/handoffs,
and profile checks. Its status is `pass`, `warn`, or `fail`. Under `strict`,
anonymous active coordination findings are P1, and write commands reject new
anonymous `tool`, `from_tool`, or `owner_tool` values.

Formal schema files for agent-facing contracts live in `docs/schemas/`:

- `agent-rally.command.start.v1.json`
- `agent-rally.command.packet.v1.json`
- `agent-rally.command.next.v1.json`
- `agent-rally.command.doctor.v1.json`
- `agent-rally.command.setup.v1.json`
- `agent-rally.command.judge.v1.json`
- `agent-rally.command.hook.v1.json`

## Judgment and Hook Contracts

`rally next --tool <tool> --json` is the ranked next-action surface. It returns
one top recommendation plus alternatives, source events, score, and factor
breakdown. It is deterministic derived state over the log: pending handoffs,
blockers, owned tasks, unowned tasks, and recent artifacts are candidate sources.

`rally judge --tool <tool> --phase <phase> --json` is the pure judgment surface.
It answers whether the agent should continue, pause, acknowledge a handoff, or
refresh context. Phases are conventions (`start`, `before-write`, `after-write`,
`before-commit`, `idle`) and are intentionally shared across all integrations.

`rally hook <phase> --tool <tool> --json` is the adapter-facing boundary hook.
It wraps the same judgment envelope and may perform safe boundary side effects.
The first side effect is `hook before-write --path <path> --auto-claim`, which
creates a claim only when no stop reasons and no competing claim exist. Hooks are
boundary-based, not token-by-token.

`rally ci gate --tool ci --json` is the merge gate. It fails non-zero when active
blockers, pending required handoffs, claim conflicts, or invalid checkpoints are
present.

## Checkpoint Contract

Hot query commands use a disposable checkpoint at `rally.checkpoint.json` when
it matches the current `changes.jsonl` tail metadata. The checkpoint is a cache,
not source of truth. It can be rebuilt or inspected with:

```bash
rally checkpoint rebuild --json
rally checkpoint status --json
```

If the checkpoint or tail cache is missing, stale, or mismatched, Rally falls
back to strict log replay and rebuilds the checkpoint. Cached and uncached reads
must produce the same command JSON except for expected timing fields.

## Golden Contract Tests

Agent-facing JSON contracts are covered by CLI golden tests under
`crates/rally-cli/tests/golden/`. The test harness normalizes dynamic ids,
timestamps, hashes, signatures, channel paths, and age counters before comparing
fixtures. Update fixtures deliberately with:

```bash
RALLY_UPDATE_GOLDENS=1 cargo test -p rally-cli --test golden_contracts
```

Then run the same test without `RALLY_UPDATE_GOLDENS` to prove the checked-in
fixtures are stable.

## Write Commands

The context brief improves when agents write structured coordination facts:

```bash
rally profile --tool codex --capability rust --capability review --watch crates/rally-core --json
rally profile --tool codex-reviewer --role reviewer --capability rust --capability review --json
rally subscribe --tool codex --path crates/rally-core --event-kind task --json
rally task --tool codex --subject "finish context ranking" --status active --verification "cargo test" --json
rally artifact --tool codex --subject "context schema" --artifact-kind schema --uri docs/context.schema.json --json
rally decision --tool codex --subject "agents use rally context for next action" --status binding --json
rally lesson --tool codex --subject "avoid giant planning docs as control surfaces" --lesson-kind coordination --json
```

These facts remain append-only events. The brief is derived state and can be
rebuilt from `changes.jsonl`.
