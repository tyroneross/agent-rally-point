<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Assessment — Rally Flow simplification + e2e architecture/orphan + API/instruction-surface scan

**Source:** Rally Flow dogfood `wor9rkkhp` (4-agent read-only fan-out, ~56 min, 2026-05-29). Read-only — no fixes applied by the assessment. 18 findings: **12 integrity** (broken/orphaned/dangling/dead-code) + **6 simplification**; 6 high-priority (all medium-severity).

Two dominant patterns: **(1)** docs/plans citing files/paths never ported from the prior pi-dynamic-workflows repo (one citation sweep clears five rows); **(2)** fields/branches/skills spec'd for pending B-tasks but never wired (fold each into its owning task, don't delete blind).

> **Taxonomy crosswalk (kind vs lane).** The **12 integrity / 6 simplification** split is by finding *kind*. The **B14 / B15** clusters below are by *lane/ownership* (top-level docs vs the `dynamic-workflows/` module) — so they do **not** map 1:1 to the kind split. Crosswalk: **12 integrity** = 8 in B14 (all integrity-kind) + 2 inside B15 (the `extractRoom` dead-code + the shipped-but-unlinked skills orphan) + 2 folded to crate tasks (`stale_facts`→B12 dead-code, `session-backend` schema→B13 orphan). **6 simplification** = the remaining 6 B15 rows (limiter-test dedup, route re-export, determinism loop, SKILL.md dedup, `norm()` docs, README count). 8 + 8 + 2 = 18. *(Raised by `codex:dynwf-coordinator` seq391 — the "B15 = simplifications" label was lane-shorthand; B15 is 6 simplify + 2 integrity.)*

## Lead-owned cluster → B14 (doc-citation sweep, INTEGRITY)

| Sev | Where | Issue |
|-----|-------|-------|
| M | `examples/manifest.toml` | `discover_module="agent_rally_point.discover"` + `cli_entry="agent-rally"` reference a nonexistent Python module/CLI; only the `rally` binary exists. No runtime reads it. → set `cli_entry=rally`, stub/remove `discover_module`, mark aspirational fields. |
| M | `RALLY.md` | 5 implemented subcommands absent: `sessions`, `attach`, `stop`, `locate`, `recent` (fully wired in `cli.rs`). → add a "Discovery & session management" section. |
| M | `docs/WAKE_COORDINATION_PLAN.md:77` | Cites `.build-loop/coordination/rally-diff-integration-assessment-2026-05-28.md` — does not exist. → remove or mark external. |
| M | `docs/pi-dynamic-assessment-handoff.md:65-80` | Cites 3 missing files (`RUST_GREENFIELD_ARCHITECTURE.md`, `COORDINATION_TRACE.md`, `.bookmark/bookmark.context.md`) — from the prior repo. → header note "cited files are from the original pi repo, not present here" or delete the line-number citations. |
| L | `README.md:24-25` | Load-bearing commands list omits `locate` and `recent`. → append. |
| L | `docs/PLAN-take-best-pr46.md:103-104` | Bare schema shorthand (`fact/next/room/check.v1.json`) ≠ actual `agent-rally.command.*.v1.json` / `agent-rally.fact.v1.json` naming. → correct the paths. |
| L | `docs/PLAN-pi-dynamic-seam.md:69` | B6 row references `scripts/rally_heartbeat.*` as if it exists; only `rally_wake.py` present. → mark row future/not-started. |
| L | `scripts/rally_wake.py` | Orphan operational tool — no inbound refs, no CI, no docs. → document + smoke-test, or deprecate if superseded by `rally inject`. |

## Lead-owned cluster → B15 (dynamic-workflows simplifications, SIMPLIFY)

| Sev | Where | Issue / fix |
|-----|-------|-------------|
| M | `tests/workstream-lint.test.mjs:59-71` vs `tests/route.test.mjs:181-210` | `createLimiter` tested twice (same contract). → delete the block from the lint test; add a one-line re-export-identity assert in route.test.mjs. |
| M | `skills/claude/SKILL.md` vs `skills/codex/SKILL.md` | Substantially duplicated protocol; already drifting (`--severity high`, `--json` variance). → one shared source (SHARED.md or PROTOCOL.md sections); keep only host-different bits (permission gate, tool name, `--tool` value, flag variance) per file. |
| L | `core/route.mjs:13` + `package.json` | `createLimiter` reachable via two public paths (`./route` re-export + `./limiter`). → drop the re-export from route.mjs; repoint route.test.mjs to `./limiter`. |
| L | `core/workstream-lint.mjs:105-109` | Single-element `for (const field of ["validation"])` placeholder loop. → inline `if (...)` matching the `task.commands[]` style. |
| L | `core/workstream-lint.mjs:46-55` vs `core/workstream-status.mjs:47-50` | `norm()` divergence (lint normalizes glob `/*`; status strips `file:`) undocumented — semantics intentionally differ (prefix-overlap vs exact-match). → add a one-line comment to each documenting what it does/doesn't normalize. Do NOT force-merge. |
| L | `core/workstream-status.mjs:31-34` | `extractRoom` `raw.data.room` branch untested; unknown if `rally room --json` emits that shape. → add a test for it or remove the branch. |
| L | `README.md` | Hardcoded "35 tests" will drift. → replace with "run `npm test`". |
| L | `dynamic-workflows/skills/{claude,codex}/SKILL.md` | Shipped in `package.json` `files[]` but nothing links/imports them; B1 (skills→host discovery) created no cross-refs. → verify B1 wiring + add cross-refs, or mark `status: draft`. |

## Folded into existing crate-lane tasks (Codex lane — NOT lead-executed)

- **→ B12** (`store.rs:472`): `RoomSnapshot::stale_facts` serialized to output but always `Vec::new()` → consumers always see `0`. Implement stale-fact detection as part of the board projection, or remove the field.
- **→ B13** (`docs/schemas/agent-rally.session-backend.v1.json`): zero inbound refs — spec'd for the B13 backend-contract, never wired. Wire via B13 or move to `docs/schemas/deferred/`.

## Recommended sequence (from synthesis)

1. One doc-citation sweep PR (B14) — clears the five stale-reference rows + missing-CLI docs at once.
2. Fold the three B-task stubs (session-backend→B13, stale_facts→B12, skills-wiring→B1 verify) into their owning tasks.
3. The simplification cluster (B15), quickest high-value win first: the duplicate `createLimiter` test removal.
