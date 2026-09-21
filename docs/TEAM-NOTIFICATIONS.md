<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Team notifications — roles + performance visibility

Minimal event schema so a user can see who is doing what, in which role, and
how each agent is performing. Grounded in existing surfaces found by the
coordination research: per-repo `.rally/log/*.jsonl` (canonical), the
`agent-rally.fact.v1` fact envelope (kinds/severity/scope), per-tool file sinks
(`consumers.toml` precedent), `monitor.py` deltas (`last_seq` + surfaced keys),
and bookmark trails (`trails/files.md`, `LATEST.md`, `cost-ledger.jsonl`).

## Event (`team.event.v1`)

```json
{
  "event": "team.event.v1",
  "seq": 42,
  "ts": "2026-09-21T00:00:00Z",
  "agent": "claude_code:01",
  "role": "lead|worker|reviewer",
  "task": "ALPHA",
  "status": "started|recalled|posted|completed|blocked",
  "context_version": "TEAM-CTX-1",
  "ack_seq": 41,
  "detail": "recalled 3/3 lines verbatim"
}
```

Field notes:

- `role` is read from live state (`rally room --json` / `rally lead show`);
  research found `role:null` in stored sessions, so roles are derived at read
  time, not persisted as identity.
- `ack_seq` ties the event to the acked context; a `context_version` bump
  without re-ack marks later events stale.
- Performance = counts over this stream per agent: tasks completed vs blocked,
  recall pass rate (R1–R3), heartbeat gaps >15 min (a coordination bug per
  `AGENTS.md`).

## Delivery (no new service)

1. Emit via `rally say artifact --tool <you> --subject "TEAM <status>: <task> — <detail>"`,
   which lands in `.rally/log/` alongside other facts.
2. Optional per-tool file sink (consumers.toml pattern) for dashboards:
   `team-events.<tool>.jsonl`, one `team.event.v1` object per line.
3. Room projection (`rally room`) may surface `last_seq` + per-agent last
   event, mirroring the `monitor.py` delta pattern.

## Roadmap hooks (unresolved, kept open)

- Precedence of `~/.rally` vs `~/dev/.rally` vs per-repo `.rally` not verified
  from code — until then, per-repo log is the source of truth.
- Lead/worker/reviewer vocabulary is new (inspected code only showed
  orchestrator/implementer/judge) — adopt it in charter + events first, then in
  CLI projections.
