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
| `profile` | Latest `profile` event for the tool, when present. |
| `subscription` | Latest `subscription` event for the tool, when present. |
| `routing` | Agent-start action and reason. |
| `top_priority` | Highest-priority attention item, if any. |
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

The ranking is intentionally simple and deterministic. Later versions can add
profile-fit scoring, path subscriptions, dependency links, task/artifact
relationships, and trust weighting without changing the core principle:

> Every recommendation must cite source events.

Recommendations also carry `minimum_trust_for_automation`. Agents may display
lower-trust facts, but bridge adapters must satisfy that threshold before they
act on the recommendation automatically.

## Write Commands

The context brief improves when agents write structured coordination facts:

```bash
rally profile --tool codex --capability rust --capability review --watch crates/rally-core --json
rally subscribe --tool codex --path crates/rally-core --event-kind task --json
rally task --tool codex --subject "finish context ranking" --status active --verification "cargo test" --json
rally artifact --tool codex --subject "context schema" --artifact-kind schema --uri docs/context.schema.json --json
rally decision --tool codex --subject "agents use rally context for next action" --status binding --json
rally lesson --tool codex --subject "avoid giant planning docs as control surfaces" --lesson-kind coordination --json
```

These facts remain append-only events. The brief is derived state and can be
rebuilt from `changes.jsonl`.
