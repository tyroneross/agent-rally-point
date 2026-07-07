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

## New observations — 2026-07-06 (hook/CLI defect report, agent-builder-studio)

Filed from a live `claude_code:1ad7c71b` multi-agent session (Fable peer + codex agents). Full
report: [`docs/ISSUES-2026-07-06-hooks.md`](docs/ISSUES-2026-07-06-hooks.md). Six issues, one
correctness-class: **P1** — bare host-family `--tool` (`claude_code` vs `claude_code:<uuid>`) is
accepted silently and mints `unmanaged-agent`/`duplicate-active-squad-id` risk facts, violating the
operator-set unique-id model. **P2** — `external-intake` + stale codex-peer risk facts flood
`current_risks` (~90% noise in the observed room; 1 real risk of 10). **P3** — no per-kind read
verbs (`rally risks|decisions|artifacts` all "unknown command"), `whoami` returns all-null fields,
`sessions --json` double-nests. Through-line match: directly serves the "make every rally CLI result
*trustworthy* and *liveness-aware*" goal — a room whose risk view is 90% telemetry is not
trustworthy coordination truth.

## New observations — 2026-07-04 (cross-repo defect report, rosslabs-agent-harness)

Filed from live coordination failures during the harness P-A.1/Phase-8 sessions (Claude session
59009f33 + a Codex peer). Rally-room risk fact posted: `fact_b833_18bf4bac327b1518` (seq 3991).

**Root cause established by direct diagnosis: the consumer repo's `.rally/facts.db` is corrupt.**
`PRAGMA integrity_check` on `rosslabs-agent-harness/.rally/facts.db` (1.9 MB) returns btree
`error code 11` plus hundreds of double-referenced pages ("2nd reference to page N"). Total store
size there: **697 MB** (log segments only 1.7 MB / 2,745 lines across 8 segments — the bulk is
elsewhere in the store dir).

Symptoms as experienced by the two agents (same binary `0.1.5+5e90f0b`):

- **Claude:** every mutating command (`enter`, `say`) failed closed with
  `watchdog-timeout-uncommitted-mutation` (>3000ms before durable append committed) — ~6 attempts
  across a full session, zero landed; reads (`room`, `next`) intermittently failed open ("hook
  exceeded 3000ms"). No corruption message ever surfaced.
- **Codex:** `sessions`/`next`/`room`/`enter`/`say`/`check before-write` all failed with
  `database disk image is malformed`; `whoami` worked.
- **Differential:** the same binary in THIS repo's healthy store posts fine (the risk fact above,
  ledger-verified). And the plain-file inbox path
  (`.rally/inbox/<tool>.jsonl` appends via build-loop's `inbox.py`) kept working against the
  corrupt store the whole time — it was the only reliable channel.

Backlog candidates from this (fit the "trustworthy, fail-loud" through-line):

- **R-obs1 — fail-loud corruption surfacing:** the mutation watchdog masked corruption as a generic
  timeout for one agent while the other saw the real SQLite error. On any store error path, run/report
  `integrity_check` and name corruption explicitly; a timeout wrapper should never be the only
  diagnostic.
- **R-obs2 — corrupt-store quarantine + rebuild:** detect malformed `facts.db`, quarantine it
  (rename aside), and rebuild projections from the append-only jsonl ledger (which stayed healthy).
  The ledger-as-source-of-truth design makes this recoverable; today nothing attempts it.
- **R-obs3 — concurrent-writer hardening:** the corrupt store served two live agents plus
  high-frequency hook invocations from bench/build runs (see `chore(rally): ledger segment churn
  from build/gate runs`, `5e90f0b`). Suspect WAL/locking under concurrent writers; needs a repro
  harness. Related artifacts in-store: `facts.db-wal`, `facts.db-shm`, `mutation.lock`.
- **R-obs4 — store bloat:** 697 MB `.rally/` in a consumer repo (harness). Inventory what grows
  (worktrees? snapshot caches?) and add retention/GC.
- **R-obs5 — hook auto-init scope:** harness `run` child workspaces (temp/practice dirs) each grew
  their own `.rally/` store via hooks (e.g. `harness-practice/docsynth-app/.rally`, 8 files incl. a
  fresh `facts.db`). Hooks probably shouldn't initialize stores in throwaway workdirs.

**Version deltas at time of observation (2026-07-04 ~19:00 PT):**

| Surface | Version | Note |
|---|---|---|
| Binary Claude ran (`~/.local/bin/rally`) | `0.1.5+5e90f0b` | matches repo commit `5e90f0b` |
| Binary Codex ran (`rally whoami`) | `0.1.5+5e90f0b` | same binary — no agent-to-agent delta |
| This repo local `main` | `adb661f` (1 ahead of binary) | delta commit is rally-ui display only (`fix(rally-ui): prefix event timestamps with day-age`) — **no write-path code between binary and main**, so the failures are not the stale-binary class |
| CC plugin cache (`agent-rally-point`) | `0.1.3` | plugin/hook surface trails the 0.1.5 CLI; see `RALLY-VERSION-MISMATCH-ASSESSMENT-2026-07-01.md` for the class |

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

## Delivered — store durability for scale (2026-06-04)

Foundation for the **thousands-of-agents / many-terminals** north star. Commits `5c68dac`..`32d21be`
on `origin/main`. Measured roadmap: [`docs/SCALE-ROADMAP.md`](docs/SCALE-ROADMAP.md).

- **Corruption resilience — DONE.** Malformed/missing `facts.db` → quarantine + rebuild from the
  canonical JSONL ledger, zero history loss (header/mid-page/extended SQLite codes + torn trailing
  line). Resolves the 2026-06-01 easy-terminal `facts.db.corrupt` incident (was a cross-reference in
  build-loop issue `bl-coord-store-fragmentation`).
- **O(1) happy-path reconcile — DONE.** Fingerprint sidecar (`.rally/.reconcile-cache.json`,
  deterministic FNV-1a) short-circuits the O(N) scan; authoritative scan+rebuild on any drift.
  Measured flat (150µs at n=200 and n=4000).
- **Concurrency + determinism — DONE.** Active-segment-first R9 readback; thread-aware open jitter
  (replaces `pid%17`); parallel-test flake 25% → 0%. `SILENT_LOSS=0` at N≤128.

## Open — ranked — scale (P1–P3, measured; see SCALE-ROADMAP.md)

| ID | Item | Status |
|----|------|--------|
| **S-P1** | **Projection → indexed SQL.** `snapshot()`/`read_db_event_count` still load *all* facts → now the dominant command-latency cost (`say` 16→33ms over 251→2001 facts). Push counts to `COUNT(*)` and projections to indexed queries. Correctness-sensitive (coordination decisions) → own build-loop, TDD + audit. | open — highest leverage |
| **S-P2** | **Rotation + compaction.** Auto size-trigger rotation AND checkpoint archive into `facts.db` so it is no longer re-replayed (`replay_archive_segments`) — bounds the hot set as history grows. | open |
| **S-P3** | **`rallyd` single-writer daemon.** Warm SQLite + in-memory projection over a Unix socket; CLIs become thin clients. Removes per-process cold opens + flock thundering (wall ∝ N ≈ 110s at N=1000). Architectural — design forks (lifecycle/socket/fallback/auth) need a decision first. | open — N=1000+ ceiling |

## Delivered — automatic coordination hooks (2026-06-04, B19-(a))

Rally presence + before-write deconfliction now fire automatically (no human nudge).
**Portable by design** — the wiring ships in the repo (`.claude/settings.json` via
`${CLAUDE_PROJECT_DIR}`, `.codex/hooks.json` via git-toplevel), so it works on any
user machine with NO global `~/.claude`/`~/.codex` change. Advisory/non-blocking by
default (verified Claude + Codex hook contracts); `RALLY_HOOK_STRICT=1` opt-in to
block; `--global` installer is opt-in only. SessionStart surfaces active peers +
open claims + deconflict guidance; PreToolUse warns on a peer-claimed path. See
[`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md).

## Open — coordination portability

| ID | Item | Status |
|----|------|--------|
| **B19-(a)-universal** | **`rally hook` binary subcommand** so ANY rally repo needs only a one-line committed `.claude/settings.json` / `.codex/hooks.json` that calls `rally hook <phase> <host>` (rally on PATH — no bundled script, no absolute paths). Universal zero-config mechanism for repos that don't vendor `hooks/rally-coordination-hook.sh`. The old `rally hook` was removed (desync); re-add as a proper binary subcommand emitting the verified host envelope. | open |

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
| **0 — CORE LANDED** | **B19** | **Codex hook desync — automatic coordination is a live no-op.** **✅ LANDED on origin/main (merge `40d641d`, 2026-05-31):** `~/.codex/rally-hook.sh` repointed off the removed `rally hook`/`start` (now lazy-auto-enter — `rally enter` + `rally check before-write`, envelope parser → `data.check.*`) + the wall-clock watchdog (`29e263f`, permanent no-hang net) + B19 before-write test coverage. Auto-merged conflict-free with `763be1c` worktree_guard; 275 tests pass. **REMAINING (deliberately separate per assessment):** (a) Claude PreToolUse hook — **✅ LANDED on `feat/auto-coordination-hooks` (2026-06-04):** version-controlled `hooks/rally-coordination-hook.sh` (host-neutral, self-gating, watchdog now uses fork+setsid+process-group-kill — closes a real bug in the previous `exec`-based perl shim where grandchildren of a hung binary kept the captured stdout FD open and stalled `$(...)`) + `scripts/install_rally_hooks.sh` (idempotent, `--uninstall`, `--repoint-codex`, `--dry-run`) + 15 tests (7 hook + 8 installer) + [`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md). Closes the loose-file desync recurrence risk (`--repoint-codex` repoints `~/.codex/rally-hook.sh` at the in-repo versioned file with `.bak` backup). The hook is advisory-only by default (`RALLY_HOOK_STRICT=1` escape hatch). Self-gates on missing `.rally/` → safe to install globally. (b) `rally migrate-legacy` + retire Python writers (B17 follow-through); (c) adopt `rally run/inject/capture/stop` lifecycle in ET. Full report: [`docs/assessment-2026-05-31-codex-hook-desync.md`](docs/assessment-2026-05-31-codex-hook-desync.md). | The field no-op (codex coordination inert) is **fixed**. Remaining items are enhancements, not the live break. | B17 (migrate-legacy parity) |
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
| **B-test-flake** | ~~`user_journey` parallel-launch flake~~ **DONE** (`39970a8`) — root cause: `append_fact` had no lock-retry + `open_fact_store` retried in lockstep (thundering herd). Fix: per-process jittered SQLITE_BUSY retry on both write+open paths (`store.rs`) + poison-tolerant module `Mutex` serializing the 5 heavy `rally run` tests (`user_journey.rs`). Verified 5/5 clean; full suite 275 passed. | shipped |

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
