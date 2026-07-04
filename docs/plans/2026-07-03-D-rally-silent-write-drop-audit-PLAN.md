# PLAN-D · Rally silent-write-drop audit + durable fix (codex-executable)

- **Status:** READY for codex to execute — **v2 (hardened by plan-critic)**
- **Author:** claude_code:667ec76c (RCA + plan)
- **Owner (execution):** codex
- **Produces work for:** codex (implementation lane) + claude_code (verification lane)
- **Baseline:** origin/main @ 11b0f07 (CI-green; clippy/audit/deny gates now active)
- **Related memory:** `reference_rally_stale_binary_write_drop` (same symptom class, different root cause)
- **v2 changelog:** corrected the false "append is O(1)" premise (Fix B); added the commit-signal/idempotency hazard (Fix A→duplicate facts); reframed contention around the unbounded flock; added swallowed-Result and torn-write audit items; de-conflicted the test-authorship lane split; made the concurrency acceptance falsifiable; added the hook-wrapper contract gate.

---

## 1. Verified root cause (do NOT re-derive — start from here)

**Symptom:** `rally say` / `rally room` return the stub `{"ok":true,"product":"rally"}`; writes are silently lost (fact never appended, caller sees success); room reads come back empty.

**Root cause (two layers):**

1. **Architectural defect.** Every command runs under a **3000ms fail-open watchdog**:
   - `crates/rally-cli/src/lib.rs:133` — `const DEFAULT_WATCHDOG_TIMEOUT_MS: u64 = 3000;`
   - `crates/rally-cli/src/lib.rs:403` — `resolve_watchdog_posture()` returns `FailOpen` for **every mutating `say` command**; only `check before-write` fails closed, and only behind opt-in `RALLY_BEFORE_WRITE_FAILCLOSED`.
   - `crates/rally-cli/src/lib.rs:464` — `emit_timeout_fail_open()` prints `{"ok":true,"product":"rally"}` and returns, **abandoning in-flight work** on a detached worker thread.
   - `crates/rally-cli/src/lib.rs:1366` — `command_say()` calls `RoomStore::open()` (expensive reconcile/projection) then appends.

2. **Trigger (transient).** Single-invocation latency intermittently spikes >3000ms under **filesystem/DB lock contention** when multiple agents coordinate. Evidence: identical command completes in 8.0s cold / 0.02–0.06s warm under `--timeout-ms 60000` (budget overrun, not hang); same write dropped then landed 0.06s apart while a peer session advanced the seq counter.

**⚠️ Critical correction from plan-critic (read before designing Fix B):** the expensive work is **not** confined to `RoomStore::open()`. `append_fact` (`store.rs:881`) itself takes the mutation flock (886), runs `reconcile_segments_and_db` (887) — the same replay that makes a cold open 8s — and reopens the fact store (888). So **the append is NOT O(1)**; simply moving the append before `command_say`'s `open()` does not remove it from the abandonable window. The real cost center is the flock-guarded reconcile, reachable from both `open()` and `append_fact`.

**Durable rule this must enforce:** for any append-to-canonical-then-rebuild-cache store, *a write succeeded only if it persists through a rebuild* — never trust exit code / `ok:true`. Conversely (new), *a caller must never be told a write FAILED when it actually committed* — that induces duplicate facts on retry.

---

## 2. Objective

Codex executes this plan to (a) **find every instance of the silent-write-drop / fail-open-on-mutation / torn-write defect class**, and (b) land the durable fix. Output is a findings table split into a codex implementation lane and a claude verification lane.

---

## 3. Phase 1 — Reproduce deterministically (codex owns; test file: `tests/watchdog_write_durability.rs`)

Seam already exists: `RALLY_TEST_BLOCK_MS` (`lib.rs:527`, debug-only) sleeps inside `run_inner_with` to force a watchdog timeout.

1. Red test: set a **low watchdog budget** (`--timeout-ms 50`) + `RALLY_TEST_BLOCK_MS` above it, run `rally say handoff …`, assert the fact **persisted to the ledger segment** (seq incremented AND survives a `RoomStore` reopen/replay). Today this FAILS.
2. Mirror assertion: a timed-out write must NOT return a success envelope.
3. **Falsifiability guard (plan-critic §6):** the test must PIN the overrun condition (low budget + block), not rely on ambient latency — warm writes are 0.02–0.06s and would pass on today's broken binary otherwise.

## 4. Phase 2 — Audit the defect class (codex) — the "find issues" step → `docs/plans/2026-07-03-D-findings.md`

For each, record file:line, whether it can silently drop/duplicate/tear a mutation, and proposed owner.

- **A. Watchdog-posture matrix.** Every subcommand in the `CliCommand` match (`lib.rs:548+`): is it a mutation? What posture does `resolve_watchdog_posture` give it? Any `FailOpen` mutation that emits `ok:true` on timeout is a finding. Cover explicitly: `say`, **`enter`** (appends presence/lead/risk/checkpoint, `lib.rs:1260–1290` — also a liveness verb, see §5 Fix A), `ack`, `status post`, `backlog`, `lead`, session-lifecycle (`run`/`sessions`, `lib.rs:3647–3930, 5329`), receipt append (`lib.rs:4754`).
- **B. Swallowed append Results (silent drop, no watchdog needed).** `let _ = room.append_fact(...)` discards the error and proceeds to a success envelope. Known sites: `lib.rs:1657, 1677, 3850, 3930, 4754`. Grep `rg -n 'let _ = .*append_fact' crates/rally-cli/src`. Each is a finding.
- **C. Open-before-append / abandonable-append ordering.** Anywhere a durable append sits after (or inside) the expensive flock+reconcile within the watchdog window. Check `append_fact`, `append_fact_verified`, `maybe_append_read_checkpoint` (`lib.rs:1290`), `append_state_transition_verified`.
- **D. Contention — the unbounded flock is the PRIMARY queue point, not busy_timeout.** `acquire_room_mutation_lock` (`store.rs:653`) is `flock(LOCK_EX)` with **no timeout**, held by `open_at_with_engagement` (`store.rs:763`, across migrate+reconcile+db-open) and `append_fact` (`886`). One cold 8s open convoys every concurrent writer past the 3000ms watchdog regardless of SQLite settings. Audit: (i) flock hold-time under cold reconcile; (ii) flock-vs-watchdog interaction; (iii) **verify whether a 5s SQLite busy_timeout already exists** (comment at `store.rs:5694`) before "adding" one — and note 5s > 3s watchdog makes it moot on its own; (iv) confirm the external `SqliteStore`/factstr PRAGMA surface; (v) existing 16-attempt db-locked retry loops (`store.rs:1010, 2213`).
- **E. Torn / partial state from watchdog-killed process exit.** Append order is SQLite first (`store.rs:1014`), canonical segment line second (`1026`). Exit between them → derived cache ahead of canon (phantom fact until reconcile); exit mid-`append_segment_line` can tear a JSONL line — `store.rs:1002`'s own error says a bad segment write "bricks replay for every reader." Same class: abandonment mid-`migrate_monolith_to_segments` / `reconcile_segments_and_db` inside `open()` (idempotence asserted in comments `store.rs:743`, not verified against a mid-flight kill). Audit both.
- **F. R9 stale-binary write-drop guard** (backlogged in the memory note): does the binary stamp a storage-model version into the ledger so replay warns if a write arrived via an incompatible path? Still missing → finding.

## 5. Phase 3 — Durable fix (codex implements; claude verifies). Resolve the two hazards below BEFORE coding.

**Hazard 1 (Fix B premise):** because `append_fact` itself reconciles under the flock (§1 correction), Fix B is not "reorder append before open." The durable append must be made **cheap and bounded** — e.g. append the segment line + SQLite row **without** a full `reconcile_segments_and_db` on the write path (defer reconcile to the read/projection path), so the commit cannot overrun the watchdog. If that decoupling is infeasible, Fix B must be explicitly scoped as warm-path-only and the flock/reconcile bounding (Fix D) becomes the primary fix.

**Hazard 2 (Fix A → duplicate facts):** the watchdog is a detached worker + main-thread timeout; the main thread cannot tell whether the append committed when it emits the envelope. If Fix A returns "failed" after a commit that actually landed, a retrying caller double-appends. **Required:** a commit-signal channel — the worker sets a "committed" flag *after* the durable append and *before* projection; the main thread on timeout emits **fail-closed only if not-yet-committed**, and emits a committed-but-projection-slow success (with a note) otherwise. Plus append idempotency (dedupe by fact id / content hash on replay — note the existing L2 duplicate-seq quarantine, `store.rs`).

Land in order, each with a test:
- **Fix B:** bounded, watchdog-safe durable append per Hazard 1.
- **Fix A:** fail-CLOSED on uncommitted mutation timeout via the Hazard-2 commit signal; reads stay fail-open. Generalize `emit_timeout_fail_closed_before_write` (`lib.rs:483`) → `emit_timeout_fail_closed_mutation`. **Classify `enter` explicitly** (mutation vs liveness verb that must never hard-fail — pick and document).
- **Fix D:** bound the flock (try-lock with timeout, or lock-hold minimization by moving reconcile off the write path) + set/verify SQLite `busy_timeout`.
- **Fix E (optional, R9):** storage-model version stamp + replay warning.

## 6. Acceptance / verification gates (claude lane) — all must be falsifiable

- Phase 1 red test → green after Fix B+A.
- **Concurrency test (claude, separate file `tests/watchdog_concurrency.rs`):** N parallel `rally say` under a real `RoomStore`, **with the watchdog budget pinned below the induced work time** (low `--timeout-ms` + `RALLY_TEST_BLOCK_MS`, or measured contention > budget — NOT ambient latency), asserting **zero dropped facts AND zero duplicated facts**. (An unpinned test passes on today's broken binary — plan-critic §6.)
- **Hook-wrapper contract gate:** Fix A introduces a non-ok / non-zero envelope for mutating verbs invoked from write-hooks. Add a test/assertion that the codex/claude/gemini wrapper parsers route the new envelope sanely (no hard error surfaced into every hook). See `lib.rs:459–468`.
- All existing gates: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo audit`, `cargo deny check` (CI-enforced by 11b0f07).
- Behavioral probe (durable rule): post a fact → force a replay (`rally room`) → row SURVIVES. `ok:true` alone is NOT acceptance.
- CI green on the ubuntu `rally-gate` run for each fix commit.

## 7. Coordination (MECE, Rally) — de-conflicted test ownership

- **codex lane (implementation + its own tests):** `crates/rally-cli/src/lib.rs` (posture + `emit_*` + `command_say` ordering + `enter` classification), `crates/rally-cli/src/store.rs` (flock/reconcile/append-path + busy_timeout), and **`tests/watchdog_write_durability.rs`** (Phase 1) + any unit tests co-located with the fix. Deliverable doc: `docs/plans/2026-07-03-D-findings.md`. Claim these paths before editing.
- **claude lane (independent verification only):** authors **`tests/watchdog_concurrency.rs`** + the hook-wrapper contract check (distinct files — no overlap with codex's test file), reviews codex's diffs, watches CI on the ubuntu runner per push, triages the findings table (triage output appended to `docs/plans/2026-07-03-D-findings.md` under a `## Triage` heading). Claude does NOT edit codex's claimed source files.
- Post findings + status to Rally; **verify every post LANDS** (seq increments and survives replay). Use `--timeout-ms 30000` on writes while contention is active — this plan's own subject matter.

## 8. Non-goals

- No change to fail-OPEN posture for read-only/advisory commands (must never hang the host tool).
- No rewrite of the R5 segmented-ledger model. Write-durability + timeout-posture + contention fix only, not a storage redesign.

---
**Addendum 2026-07-03 (auditor f3):** §6's "CI green on the ubuntu rally-gate run" is superseded — 4b0cfeb retired that workflow in favour of the local `.githooks/pre-push` gate (fmt scoped to pushed .rs, clippy -D warnings, workspace tests, audit/deny on dep change, pinned 1.95.0). Verification target is now the pre-push gate passing on the pushing machine. release.yml (macos-14 artifact) unchanged.
