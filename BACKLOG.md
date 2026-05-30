<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Backlog — agent-rally-point

Consolidated, findable view of open work. Merges the `## Backlog` board in
[`docs/ORCHESTRATION.md`](docs/ORCHESTRATION.md) (B-items, with owns/deps/validation) and the
forward-action candidates in
[`docs/assessment-2026-05-30-session-rallyflow.md`](docs/assessment-2026-05-30-session-rallyflow.md)
(R-items). The auto-generated fact history is [`.rally/RETROSPECTIVE.md`](.rally/RETROSPECTIVE.md)
(`rally retrospective`). For full per-item context (file owns, suggested validation, dependency
notes) see the ORCHESTRATION board — this file is the at-a-glance roadmap, that file is the
authoritative board.

**Through-line of the open work:** make every rally CLI result *trustworthy* (room-stamped,
fail-loud, read-back-verified) and *liveness-aware*, then enforce the two guarantees the facilitator
model rests on — one-owner-per-path and one-store-per-repo — so agents coordinate without a human
referee.

## Open — ranked

| Rank | ID | Item | Why it matters | Depends on |
|------|----|------|----------------|-----------|
| 1 | **R9-readback** | **Post-mutation readback** — after any mutating command (`say`/`claim`/`release`/`handoff`/`resolve`/`enter`), re-read the canonical ledger and assert the new `event_id` landed before reporting success; return the resolved `{room, seq}`, fail loud otherwise. | Kills the whole silent-corruption class (release no-op · stale-binary write-drop · "landed"-on-exit-0 · wrong-room write) at **one** cause-agnostic anchor. Falsifiable acceptance block already written (ORCHESTRATION §R9-readback). | none |
| 2 | **R9** | **Stale-binary write-drop guard** — rally embeds a build-id/version; `rally enter` warns/fails when the invoked binary disagrees with the repo's expected line. | Two binaries with a ~10h mtime gap are still on PATH; a stale one silently drops writes. | none |
| 3 | **R10** | **Deterministic read+write path** — bring read-state onto the same append-only ledger as writes (read checkpoints as ledger facts; cursor becomes a derived cache; `rally room --readers` surfaces receipts). | Today writes are deterministic (ledger) but the read cursor is a gitignored last-writer-wins side-file surfaced by no command. | R9-readback |
| 4 | **B10** | **Canonical-path matching in `claim`/`check`** (rank-1 linchpin) — `check before-write` detects cross-path-form collisions (`src/lib.rs` vs `crates/rally-cli/src/lib.rs`). | The gate that let the lib.rs collision through this session; 4 other capabilities depend on one-owner-per-path actually holding. | none (L4-scoped landed) |
| 5 | **B11** | **Squad identity enforcement** — `rally enter` rejects a duplicate squad id, records tier, auto-asserts first squad as lead; `rally room` exposes `squads[]` + `room.lead`; `rally next` renders the 11-field contract. | The id/lead guarantees are still manually enforced. Model now lives in the id (`host-llm-role-number`). | none (L4-scoped landed) |
| 6 | **B12** | **`rally board` projection + reader/liveness** — auto-derive the board from facts; tag idle-checkpoint noise (`--artifacts signal`); stamp a `picked_up` fact when a target's `rally next` first surfaces a handoff; project each tool's last-seen seq vs max_seq; derive stale claims from age/heartbeat (record + project — no daemon, no auto-release). | Removes hand-reading the chain and "is this peer alive?" probing. Fold: implement or remove the always-empty `RoomSnapshot::stale_facts`. | none (L4-scoped landed) |
| 7 | **B13** | **PR46 feature surface** (deferred from L4) — `--produces`/`--depends` predictive contract-claims, transitive handoff **receipts**, `check ci` gate; `inject --require-ack` → session-bound `seen`/`ack` fact (two-stage: seen → resolved). | Only the L4 audit-folds reached main; the predictive/receipt feature never landed. Unblocks B2/L5 (observation seam). Fold: wire orphaned `docs/schemas/agent-rally.session-backend.v1.json`. | none |
| 8 | **B16** | **Write→read round-trip contract gate** — `cargo test`: every `say <kind>` reads back identically via `room`/`recent`/`next`/`enter`; `enter --json` room id non-null; identity survives ledger replay. | Regression guard for the reader-desync-from-canonical-shape class (the CloudEvents-vs-flat break). | none |
| 9 | **B17** | **Legacy global-index retirement** (one-store north-star) — make `RALLY_NO_GLOBAL_INDEX=1` the default; demote `--include-legacy` to a one-shot `rally migrate-legacy`; remove the read path. Build-loop-side legacy writers retired separately (tracked there). | The `~/.agent-rally-point/...changes.jsonl` second store drives cross-repo contamination + trust drift; coexistence contradicts one-repo-one-rally-point. | B16 (round-trip proves parity first) |
| 10 | **B18** | **Repo-scope write guard + external-intake quarantine** — every mutating command/URI resolves `repo_root`, `repo_id`, `ledger_scope`; external project assessments are rejected or routed to a neutral intake surface, never promoted into this repo's backlog/ledger. | A different repo's assessment briefly contaminated this ledger this session. Repo scope is a truth boundary, not just a path prefix. | B16 |

## Open — lower priority / housekeeping

| ID | Item | Notes |
|----|------|-------|
| **B-arch-doc** | Update [`docs/RALLY_ARCHITECTURE.md`](docs/RALLY_ARCHITECTURE.md) — still R1-era prose ("ledger.jsonl is the source of truth"); contradicts the R5/R8 segmented design. | MED |
| **B-whoami** | `rally whoami` — report tool id / clone / worktree / expected binary in one call (reduce two-clone identity confusion). | MED |
| **B11-race** | Harden parallel-launch id-reservation race — `rally_run_reserves_numbered_ids_under_parallel_launch` flakes in isolation; retry-on-collision or `#[ignore]` + tracking note. | MED |
| **B-index-monolith** | Filter the migration monolith from `refresh_log_index` — committed `index.json` double-counts 489 phantom events (canonical replay already excludes it; the advisory index doesn't). | LOW |
| **B-ledger-cadence** | Commit-ledger cadence policy — committed history lags on-disk; commit on a cadence (merge=union is conflict-free) or document `.rally/log/` as a live working-tree artifact. | LOW |
| **B2 / L5** | **Observation seam** — orchestrators emit `handoff/artifact/decision/standby/wake`; rally derives a DAG + wake-due; **never executes**. | Blocked on B13. |

## Automation proposals — facilitator self-coordination

From [`docs/RALLY-AUTOMATION-PROPOSALS.md`](docs/RALLY-AUTOMATION-PROPOSALS.md) (11 ranked; every one charter-safe — rally records/flags, never decides or executes). Ranks 1/2/3/10 are tracked above as **B10/B11/B12**; ranks 4–9 + 11 below are the rest. Most gate on **B10** (canonical paths) and on FP-adjudication *before* any routing.

| Rank | Capability | Status | Note |
|------|-----------|--------|------|
| 4 | `rally route-findings` — match each `{file,severity,evidence}` to the owning claim via canonical paths → typed handoff; unowned → `risk` fact | open | needs B10; never auto-route unverified findings |
| 5 | `rally board` — read-only board projection from facts (lanes + backlog + live-status delta); emits a draft, never writes `ORCHESTRATION.md` | open (also in B12) | High impact |
| 6 | Artifact source-grounding + verify gate — content-hash snapshot at `say artifact`; byte-identical to claim-open → `grounded:false` + risk; parse `--evidence` into a `verification_contract` checked by `rally verify artifact` | open (overlaps R9-readback) | verify-before-trust at the artifact layer |
| 7 | `rally next --backlog` — proactive self-routing: parse backlog, resolve deps vs landed artifacts, tier-affinity rank → `suggested_backlog_item` | open | needs B10; lead sets `safe_to_self_route` |
| 8 | Cross-lane ripple detector — grep changed `pub` signatures at artifact/check, post non-blocking `ripple-alert` + handoffs to affected owners | open | needs B10; notification only |
| 9 | `rally check tier-fit` — derive task class, compare to room MODEL-TIERS calibration, flag `tier_mismatch` (never blocks/selects) | open | host-relative tiers |
| 11 | Presence/liveness + queryable `rally goal`/intent + per-agent autonomy-envelope fact | open (liveness in B12; goal/intent + envelope net-new) | from Claude #2 cross-host input |

Shipped from this family: `rally locate` / `rally recent --all` (the [discovery re-port](docs/DISCOVERY_RE_PORT_DESIGN.md) design — done; the legacy-visibility tie-in is tracked as **B17**). Also pending: `rally doctor --canonical-paths` (rank-1 retro-scan helper).

## Done (archive)

B1, B3, B4, B5, B6, B7, B8, B9, B14, B15 are landed and verified — see the status column + commit
refs in [`docs/ORCHESTRATION.md`](docs/ORCHESTRATION.md). Highlights: the HIGH audit fixes
(NUL-byte panics, boundary-gate bypasses, discovery index-clobber), the doc-citation sweep (B14),
and the dynamic-workflows simplifications incl. the SKILL.md dedup (B15).
