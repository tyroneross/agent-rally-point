<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Spec — Lead Agent (rally coordination)

User directive (2026-05-31): rally formalizes a **lead agent** that orchestrates and
coordinates autonomous multi-agent work. Rally **records + exposes** the lead and the
mission; it **never enforces** behavior (charter: facilitator, not executor).

## Model

1. **Assignment.** Lead is the **first frontier-tier agent to enter** an empty room
   (frontier = Opus / GPT-Pro class per `dynamic-workflows/MODEL-TIERS.md`). Non-frontier
   agents (executing/fast) entering first do **not** auto-lead — the seat stays open
   (`lead: null`) until a frontier agent joins, OR a **user-designated** lead is assigned.
2. **Clear title.** `rally enter` / `rally room` surface `lead` + its tier and whether it
   is `user_designated`.
3. **Handoff.** The current lead may hand the title to another **frontier** agent
   (`rally lead handoff --to <tool>`). Records a `decision` fact `role:lead` with
   `ref` to the prior lead.
4. **Relinquish / user override.** The lead may relinquish (`rally lead relinquish`),
   reopening the seat. A **user-designated** lead (`rally lead assign --to <tool>
   --user-designated`) supersedes a first-join lead; the incumbent relinquishes.
   `user_designated` leads are not auto-displaced by later first-joins.
5. **Responsibility (doctrine, not enforced).** The lead:
   - orchestrates + coordinates the autonomous work (lanes, claims, merges, deconflict);
   - **makes tradeoffs and decisions from the goal + knowledge of the app**;
   - when information is insufficient, falls back to the **ultimate intent / outcome of
     the work** — the queryable `rally mission` north-star + per-agent autonomy envelopes.
     "Resolve from the mission; don't stall."

## Surfaces (rally CLI)

| Command | Effect |
|---|---|
| `rally enter --tool <t> [--tier frontier\|executing\|fast]` | declares tier; first frontier → lead |
| `rally room --json` | `lead`, `lead_tier`, `lead_user_designated` |
| `rally lead show` | current lead + tier + how-assigned |
| `rally lead handoff --to <frontier-tool>` | transfer title (frontier-gated) |
| `rally lead relinquish` | drop title, reopen seat |
| `rally lead assign --to <tool> --user-designated` | user-set lead, supersedes first-join |

## Fact model

Reuses `FactKind::Decision` with `subject: "role:lead"` (already present). Adds:
- `evidence: ["tier:frontier", "assigned:first-join|handoff|user-designated"]`
- `ref`: prior lead's event_id on handoff/relinquish.
No schema change — projection (`RoomSnapshot::lead`) reads the latest live `role:lead`
decision, plus a `role:lead-relinquished` to reopen the seat.

## Charter guardrail

Rally records who the lead is and what the mission is; it does **not** force any agent to
obey the lead or block a non-lead's writes. Lead authority is doctrine surfaced via the
ledger + mission, enforced socially by the agents, not by the CLI.

## Build chunks

- **L-1** tier-at-enter + frontier-only auto-lead + projection (`lead_tier`, `user_designated`). *Verify:* `cargo test -p rally-cli lead::`.
- **L-2** `rally lead show|handoff|relinquish|assign` command group. *Verify:* round-trip tests.
- **L-3** mission-fallback doctrine in `COORDINATION.md` + surface lead responsibilities on `enter`/`mission`.
