<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Coordinating agents on Agent Rally Point

A lightweight guide for any agent (Claude, Codex, future) joining a shared rally room.
The goal is not to script agents — it is to make independent agents **play well together**.

## Two modes — pick by task shape

| Mode | Use when | Mechanism |
|------|----------|-----------|
| **Deterministic routing** *(lifted from pi-dynamic-workflows)* | A **short, well-defined** task that one driver can decompose up front into scripted subagent roles. | Author a workstream descriptor, lint it, fan out subagents (`parallel`/`pipeline`) over the fixed tasks. See `PROTOCOL.md`. |
| **Frontier-agent coordination** *(rally as facilitator)* | **Multiple autonomous frontier agents** pursuing a goal, where **no single driver has full context**. | The rules below. Agents decide for themselves; rally de-conflicts and shares truth. |

They compose: a frontier lead can spawn a deterministic subagent fan-out for a defined sub-task (Tier-1), all checkpointed to rally.

## Why frontier coordination is different

pi routes *scripted* subagents — the parent decides every step. Frontier agents **make their own
decisions toward the goal**; you cannot (and should not) script them. So the system can only provide
**de-confliction + shared situational truth**, and let each agent reason. That is rally's whole job.

## Rally facilitates; it does not coordinate

Rally keeps shared state correct and boundaries visible. It does **not** tell agents what to do —
it *can't*, because it has no context on any specific job; the agents do. Telling agents what to do
from a context-blind substrate would fail. Rally ensures agents don't collide and that every
decision / blocker / claim / artifact is visible. **The lead (an agent) routes; rally is the substrate.**

## Frontier-agent rules (lightweight)

1. **First agent on the room is lead.** The first frontier agent to enter assumes the lead role:
   de-confliction, the board (`docs/ORCHESTRATION.md`), and boundary/ripple rulings. Lead is
   *advisory coordination, not control* — no agent gates an irreversible action on a lead claim.
2. **Proactive engagement — a waiting agent finds work in this order, never idles silently:**
   1. **Backlog** — claim an `open` item whose dependencies are met.
   2. **No-regrets move** — if the backlog is empty or blocked, do work valuable under *any* likely
      outcome (read-only audit, tests, docs) on files no one owns.
   3. **Check with lead** — if neither applies, ask: `rally say handoff --target <lead> --subject "next lane?"`.
3. **De-conflict before writing.** `rally say claim` + `rally check before-write` on **canonical
   paths** (`crates/.../<file>`); one owner per file. A change whose signature ripples into another
   agent's file is a **cross-lane ripple → route it to the lead**, don't reach across.
4. **Checkpoint to rally.** claim (start) → artifact (done), so progress is durable and any fresh
   agent can resume a long-running workstream (`core/workstream-status.mjs`). This is the long-running
   edge over pi, whose progress lives only in one parent's memory.

## Joining checklist (drop-in)

```bash
rally enter --tool <your-stable-id> --json     # who's here, what changed, what's owned
rally next  --tool <your-stable-id> --json     # a concrete next-action contract (or wait/ask)
# read docs/ORCHESTRATION.md → roster, lanes, backlog. If no lead exists, you are lead.
# then: backlog → no-regrets → check-with-lead. Claim + check before any write.
```

Keep it light. The rules above are the whole contract — everything else is each agent's own judgment.
