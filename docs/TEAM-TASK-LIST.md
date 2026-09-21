<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Team task list — higher-echelon coordination

Companion to the [team task board](TEAM-TASKS.md). The board states the work
and the owning session; this note states how sessions stay coordinated with
higher echelon (lead → user).

## Where the live list lives (single source of truth)

Do not maintain a second table here. The board is:

- `config/team-task-board.json` — operator-visible list (intent, session,
  resources, status);
- `python3 scripts/team_task_board.py --board config/team-task-board.json`
  prints it; `--json` emits the same rows;
- `rally backlog add --id <id> --intent "…" --target <session>` records the
  ledger assignment; `team.event.v1` carries status pings.

A task with no `session` is unassigned. A newly launched pane is not a company
until `room_ready` — until then `standby`, never an owner.

## Intent down, status up

1. **Receive.** The owning session takes intent from the lead (board row +
   mission), acks it (`rally ack`), and quotes back `context_version` before
   starting (R1/R2). Method is the session's latitude; outcome is its
   accountability — it may task plugins, skills, MCP, and subagents freely
   within the charter.
2. **Report.** Status flows up as `team.event.v1`
   (`started → recalled → posted → completed|blocked`) plus heartbeats (~10 min;
   silence over ~15 min is a coordination bug per `AGENTS.md`).
3. **Escalate.** Blockers go to the lead, which re-plans (reassign, split, or
   descope). The user (highest echelon) reads the rollup in `rally room` + the
   event stream — never by chasing individual sessions.

## Lifecycle

`open → assigned → acked → in-progress → completed|blocked`. Transitions are
`team.event.v1` facts (see [TEAM-NOTIFICATIONS](TEAM-NOTIFICATIONS.md)); the
board is re-cut from the event stream whenever intent changes. Ownership stays
runtime data (`rally room --json` / `lead show`); a session working from a
stale `context_version` is conflicted-out at merge, never blocked at the
keystroke.
