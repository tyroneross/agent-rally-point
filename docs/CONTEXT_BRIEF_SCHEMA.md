<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Context Brief Contract

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
