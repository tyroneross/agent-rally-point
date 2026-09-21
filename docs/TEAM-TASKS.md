<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Team task list — sessions as companies

High-level board: **what the team must do**, and **which session owns it**.
Complements the charter ([TEAM-CHARTER](TEAM-CHARTER.md)) and Rally backlog
(`rally backlog add --target <session>`).

## Model (company / MAGTF)

A **session** (Claude Code, Codex, Cursor, Muse Spark, …) is one company:

| MAGTF / company | Here |
|---|---|
| Commander's intent | Team charter + `rally mission` + `context_version` |
| Company commander | The long-lived session (GM / senior exec). It tasks its own plugins, skills, MCP, and subagents |
| Organic resources | Host plugins, skills, Rally, build-loop, persona-lab, Muse/Codex/Claude subagents |
| Constraints | Team mode, R1–R3 recall, claim-before-edit, merge gate |
| Assigned mission | One or more board tasks (`id` + `intent`) |
| Higher echelon | Operator (HHQ) and/or the lead session. Every company **must** report here |

A company does not write its own war. It executes inside higher's intent and
**must maintain coordination with higher echelon**:

1. **Intent down** — charter, `rally mission`, and `context_version` come from
   HHQ / the lead. A company may not replace the north-star.
2. **Status up** — every status change (`started` / `blocked` / `completed`)
   is posted to higher: `rally say artifact`, `team.event.v1`, and this board.
3. **Escalate silence** — `blocked` or a heartbeat gap >15 min is a report to
   higher, not a private stall.
4. **No bypass** — `reports_to` is required on every company. It is HHQ or the
   lead session. Self-report and missing `higher_echelon` fail the board lint.

Do **not** assign a task to a raw model name. Assign it to a **session tool id**
(`claude_code:…`, `codex:…`, `cursor:…`, `muse_spark:…`). That session may
fan out internally. The board still shows one owner.

## Durable vs display

| Surface | Role |
|---|---|
| `config/team-task-board.json` | Operator-visible board (intent, session, resources, status) |
| `rally backlog add --id <id> --intent "…" --target <session>` | Ledger assignment; `rally next` nags the target |
| `team.event.v1` ([TEAM-NOTIFICATIONS](TEAM-NOTIFICATIONS.md)) | Status pings (`started` / `completed` / `blocked`) |

`python3 scripts/team_task_board.py --board config/team-task-board.json` prints
the table. `--json` emits the same rows. A task with no `session` is unassigned.

## New-session admission

A newly launched pane is not a company until it is `room_ready` (see Muse /
new-agent gate). Until then it may appear as `standby`, not as a task owner.
