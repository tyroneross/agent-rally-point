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

## Delivered — coordination program (2026-05-30)

Shipped this session (see `docs/PROGRAM-rally-coordination-spec.md`). Triaged into tiers; **Tier 1
done**.

- **Presence substrate (B11/B12/B16 — largely delivered):** `enter` emits a `presence` fact; `room`
  projects `squads[]` (active/idle) + `lead`; first-enter asserts lead; `enter --json` returns a
  non-null room id; round-trip test. **Remaining for B11:** reject a *duplicate* squad id at enter.
- **Lazy auto-enter:** any tool-scoped command registers presence (no hook).
- **`rally status --global`** cross-repo rollup (read-only) + **resilient room-index reader**
  (recovers a torn index; also repaired `recent --all`). Partial **B12** (board projection).
- **mini-loop** skill (per-task quality loop for Rally Flow).
- **Inject channel-of-record (asks):** `inject --tool <sender>` records a content fact
  `{sender → recipient: message}` before delivery, so coordination is durable even if live tmux
  delivery fails; verify via ledger, not TUI scrape.
- **Tier-1 fixes:** `room_missing` warning collapsed to one summary; system author `rally` excluded
  from `squads[]`; **multiple managed sessions per caller tool** (a lead can spawn N subagents).

**Tier 2 — DONE:** B10 canonical-path matching (exact/dir-prefix STOP + suffix WARN); B16 round-trip
gate (all 9 fact kinds reload-verified); **B11** duplicate-id = warn-not-block + durable `risk` audit
fact (never stops work, fully traceable — the auditability principle); **B12** delivered by
`status --global` (board) + `squads[]` active/idle (liveness). **Tier 3 — DONE (2026-05-30):** B17
one-store retirement (global index default-off, commit `3b2c292`) · B18 repo-scope guard (via
quarantine, the charter-aligned approach — not hard-reject) · B13 PR46 surface · automation ranks 4–9
· R10 ledger-cursor · `doctor`. **Still open:** B11-race (rapid back-to-back `run` drops a session);
stale-registry prune (B-index-monolith — ~340 dead entries, warning-collapsed not pruned); + the other
housekeeping rows below.

**Design principle now embodied (from user):** coordination is *never blocked* — collisions and
duplicate ids **warn + record a durable audit fact** (inject channel-of-record, B10 ambiguous-path
WARN, B11 duplicate-id risk fact) so work continues and any mistake is traceable + fixable after.

## Open — ranked

> **Reconciliation (2026-05-30 #2 — validated against real code + call-graph scan):** most rows below
> shipped this session. **DONE (verified):** R9-readback (`append_fact_verified` across commands), R9
> stale-binary (`BUILD_ID`/drift in `command_enter`), **R10** (ledger-derived enter-cursor —
> `cursor_for` now ledger-first, `cursors.json` demoted to write-through cache; commit `3b2c292`),
> B10/B11/B12/B16 (commits `1944ae4`/`c99b835`/`d1c9eeb`), **B13** (PR46 surface: `--produces`/`--depends`/
> `require-ack` + `check ci`), **B17** (legacy global index now default-**OFF**, opt-in `RALLY_GLOBAL_INDEX=1`,
> `RALLY_NO_GLOBAL_INDEX` still wins; `--include-legacy` never existed; `migrate-legacy` ships the one-shot
> import; commit `3b2c292`), the **B2/L5 observation seam** (`dag`/`wake-due`/`standby`/`wake`, tested e2e),
> and automation ranks **4–9** + `doctor`.
>
> **B18 is DONE via quarantine** — repo_root-anchored ledger + `external-intake` scope tag + durable `Risk`
> audit fact + projection filtering + tests b18b/d/e/f/g. The backlog's **"hard-reject" framing is REJECTED**
> as contrary to the never-block charter (*coordination is never blocked — warn + record a durable audit
> fact*); quarantine-and-filter IS the charter-aligned approach. Optional micro-hardening only:
> `command_route_findings`/`command_backlog` don't `classify_scope` on write (their facts are repo-local or
> already-safe risk facts — low value).
>
> **Housekeeping — all closed (2026-05-30):** B-arch-doc (`e46c083`), B-ledger-cadence (`e46c083`),
> B-whoami + B11-race (`fcb81ca`), B-index-monolith (verified non-issue). **Automation rank-11 — DONE**
> (`8faa9de`): shipped as `rally mission` (queryable room north-star + per-agent autonomy envelopes,
> surfaced on enter/room; charter-pure — rally records/exposes, never enforces). **The ranked + automation
> backlogs are now fully closed; nothing open remains below.**
>
> **Code-scan pass (2026-05-30, `fcb81ca`):** a sonnet+haiku rally-workflow scan (7 scanners, rally-lineage
> coordinated — see `rally dag --run scan-20260530`) surfaced 20 findings (0 false positives); all fixed —
> 1 correctness bug (inject_content_fact durability/R9-readback), 6 efficiency (redundant full-ledger scans),
> simplicity/dedup/deadcode (net **−3 lines** across the batch incl. a new command). cargo test 207→210.

| Rank | ID | Item | Why it matters | Depends on |
|------|----|------|----------------|-----------|
| **0 (new, top)** | **B19** | **Codex hook desync — automatic coordination is a live no-op** — the installed `~/.codex/rally-hook.sh` still calls the `rally hook`/`start` subcommands removed in `0d5024b`, so every codex before-write resolves to `unknown command` and records nothing; `--fail-open` hides it. Retire/re-point the external wrapper to the live **lazy-auto-enter** model (`rally check before-write` / `rally enter`), add a Claude integration, run `migrate-legacy`, adopt `run/inject/capture/stop`. The wall-clock watchdog (commit `29e263f`, branch `fix/rally-hook-walltime-timeout`) is the permanent no-hang net. Full report: [`docs/assessment-2026-05-31-codex-hook-desync.md`](docs/assessment-2026-05-31-codex-hook-desync.md). | Codex coordination is **currently inert in the field** — the no-op partly explains "2/19 agents tracked". Concrete instance of B17's "build-loop-side legacy writers retired separately". Live multi-agent ET orchestration depends on it. | B17 (migrate-legacy parity) |
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
| **B-arch-doc** | ~~Update RALLY_ARCHITECTURE.md~~ **DONE** (`e46c083`) — data-model rewritten R1→R5 segmented; global-index section corrected to B17 default-off. | shipped |
| **B-whoami** | ~~`rally whoami`~~ **DONE** (`fcb81ca`) — reports tool / repo_root / repo_id / worktree / build_id / cwd in one call. | shipped |
| **B11-race** | ~~Parallel-launch id-reservation race~~ **DONE** (`fcb81ca`) — durable fix: session-id backoff (#16) breaks the thundering herd; CAS already prevents duplicate ids, the flake was retry-budget exhaustion. | shipped |
| **B-index-monolith** | ~~Filter the migration monolith from `refresh_log_index`~~ **VERIFIED NON-ISSUE** (scan 2026-05-30) — index entries over-count the archived monolith, but `max(seq)` consumption is idempotent and DB rebuild dedups independently; no correctness impact. | closed |
| **B-ledger-cadence** | ~~Commit-ledger cadence policy~~ **DONE** (`e46c083`) — documented in RALLY_ARCHITECTURE.md: segments are a live working-tree artifact, `merge=union` makes lagging commits safe; no required cadence. | shipped |
| **B2 / L5** | ~~Observation seam~~ **DONE** — `dag.rs` + `dag`/`wake-due`/`say standby`/`say wake` + `--run/--step/--parent-step` lineage; rally derives the DAG + wake-due, never executes. Tested e2e 2026-05-30; wired into `skills/rally-workflows/SKILL.md` §7. | shipped |

## Automation proposals — facilitator self-coordination

From [`docs/RALLY-AUTOMATION-PROPOSALS.md`](docs/RALLY-AUTOMATION-PROPOSALS.md) (11 ranked; every one charter-safe — rally records/flags, never decides or executes). Ranks 1/2/3/10 are tracked above as **B10/B11/B12**; ranks 4–9 + 11 below are the rest. Most gate on **B10** (canonical paths) and on FP-adjudication *before* any routing.

| Rank | Capability | Status | Note |
|------|-----------|--------|------|
| 4 | `rally route-findings` — match each `{file,severity,evidence}` to the owning claim via canonical paths → typed handoff; unowned → `risk` fact | **DONE** (`route_findings.rs` + subcommand + tests) | shipped |
| 5 | `rally board` — read-only board projection from facts (lanes + backlog + live-status delta); emits a draft, never writes `ORCHESTRATION.md` | **DONE** (`board.rs` + subcommand; B12) | shipped |
| 6 | Artifact source-grounding + verify gate — content-hash snapshot at `say artifact`; byte-identical to claim-open → `grounded:false` + risk; parse `--evidence` into a `verification_contract` | **DONE** (`source_grounding.rs`) | shipped |
| 7 | `rally next --backlog` — proactive self-routing: parse backlog, resolve deps vs landed artifacts, tier-affinity rank → `suggested_backlog_item` | **DONE** (`next_returns_suggested_backlog_item` test) | shipped |
| 8 | Cross-lane ripple detector — grep changed `pub` signatures at artifact/check, post non-blocking `ripple-alert` + handoffs to affected owners | **DONE** (`ripple.rs`) | shipped |
| 9 | `rally check tier-fit` — derive task class, compare to room MODEL-TIERS calibration, flag `tier_mismatch` (never blocks/selects) | **DONE** (`tier_fit.rs`) | shipped |
| 11 | Presence/liveness + queryable goal/intent + per-agent autonomy-envelope fact | **DONE** (`8faa9de`) — shipped as `rally mission` (set/get north-star + envelopes, surfaced on enter/room); liveness was already B12 | shipped |

Shipped from this family: `rally locate` / `rally recent --all` (the [discovery re-port](docs/DISCOVERY_RE_PORT_DESIGN.md) design — done; the legacy-visibility tie-in is tracked as **B17**). Also pending: `rally doctor --canonical-paths` (rank-1 retro-scan helper).

## Done (archive)

B1, B3, B4, B5, B6, B7, B8, B9, B14, B15 are landed and verified — see the status column + commit
refs in [`docs/ORCHESTRATION.md`](docs/ORCHESTRATION.md). Highlights: the HIGH audit fixes
(NUL-byte panics, boundary-gate bypasses, discovery index-clobber), the doc-citation sweep (B14),
and the dynamic-workflows simplifications incl. the SKILL.md dedup (B15).
