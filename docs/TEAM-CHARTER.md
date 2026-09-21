<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Team Charter — goals, roles, shared memory

Default team contract for multi-agent work in this repo. Complements the
coordination mandate ([SPEC-coordination-mandate](SPEC-coordination-mandate.md))
and the lead seat ([SPEC-lead-agent](SPEC-lead-agent.md)): rally records the
contract, agents follow it as doctrine, merge gates enforce it.

## Team goal (single north-star)

Reliable agent-to-agent communication: every member works from the same
mission, the same ordered task list, and the same context version.

## Roles

| Role | Seat | Duty |
|---|---|---|
| Lead | per `rally lead show` (frontier or user-designated) | authors mission + ordered task list, resolves tradeoffs from the mission |
| Worker | any entered agent | claims a lane, does the work, posts status |
| Reviewer | any entered agent not authoring the lane | checks recall + merge predicate before landing |

Role identity is runtime data (`rally room --json`, `rally lead show`), never a
constant copied from old logs.

## Shared-memory protocol

1. Lead writes the mission (`rally mission`) and the ordered task list (this
   charter's `context_version`, e.g. `TEAM-CTX-1`).
2. Each member runs `rally enter` + `rally ack` and quotes back
   `context_version` + task order before starting (see
   [TEAM-RECALL-RETESTS](TEAM-RECALL-RETESTS.md) R1).
3. Context change = new `context_version` + re-ack. Work citing a stale version
   is conflicted-out at merge (claims released, risk recorded), never blocked
   at the keystroke.

## Personas + build-loop injection

- Personas (e.g. `~/.persona-lab` rosters) supply **role voice**, not facts: a
  worker may write as reviewer-persona, but task order and mission come only
  from the lead's acked context.
- Build-loop memory (`.build-loop/`) supplies **prior-art context**: proposals
  and retros linked by id. When cited, the citing agent records the proposal id
  + `context_version` so staleness is detectable.

## User-selectable modes (`config/team-modes.json`)

| Mode | Recall gate | Status cadence | Use when |
|---|---|---|---|
| `standard` (default) | R1 verbatim recall before start | ~10 min heartbeats | normal parallel work |
| `strict` | R1 + R3 mutation recall before start and before merge | ~5 min heartbeats | merge-heavy or flaky-recall squads |
| `lightweight` | checksum line only, no verbatim quote | final status only | solo or 2-agent spikes |

Select with `TEAM_MODE=strict rally enter` (or per-command `--team-mode` where
supported); `rally room --json` surfaces the active mode. Default is
`standard`; no selection needed.

## Team task list (sessions as companies)

High-level work is the board in [TEAM-TASKS](TEAM-TASKS.md) /
`config/team-task-board.json`: one row per task, one **session** as the
company that owns it. That session is the GM — it may use any plugin, skill,
or subagent under the charter. Every company **reports to higher echelon**
(operator HHQ and/or the lead session); it does not invent a parallel mission.
Durable assignment is still
`rally backlog add --id <id> --intent "…" --target <session>`.

