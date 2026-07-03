# PLAN-D · Rally silent-write-drop audit + durable fix (codex-executable)

- **Status:** READY for codex to execute
- **Author:** claude_code:667ec76c (RCA + plan)
- **Owner (execution):** codex
- **Produces work for:** codex (implementation lane) + claude_code (verification lane)
- **Baseline:** origin/main @ 11b0f07 (CI-green; clippy/audit/deny gates now active)
- **Related memory:** `reference_rally_stale_binary_write_drop` (same symptom class, different root cause)

---

## 1. Verified root cause (do NOT re-derive — start from here)

**Symptom:** `rally say` / `rally room` return the stub `{"ok":true,"product":"rally"}`; writes are silently lost (fact never appended, but caller sees success); room reads come back empty.

**Root cause (two layers, both evidence-backed):**

1. **Architectural defect (the real bug).** Every command runs under a **3000ms fail-open watchdog**:
   - `crates/rally-cli/src/lib.rs:133` — `const DEFAULT_WATCHDOG_TIMEOUT_MS: u64 = 3000;`
   - `crates/rally-cli/src/lib.rs:403` — `resolve_watchdog_posture()` returns `FailOpen` for **every mutating `say` command**; only `check before-write` can fail closed, and only behind opt-in `RALLY_BEFORE_WRITE_FAILCLOSED`.
   - `crates/rally-cli/src/lib.rs:464` — `emit_timeout_fail_open()` prints exactly `{"ok":true,"product":"rally"}` and returns, **abandoning in-flight work**.
   - `crates/rally-cli/src/lib.rs:1366` — `command_say()` calls `RoomStore::open()` (the expensive ledger reconcile/projection) **before** it appends the fact. So the durable append is gated behind the slow op the watchdog kills → **timed-out write = false success + data loss.**

2. **Trigger (transient/environmental).** A single invocation's latency intermittently spikes >3000ms under **SQLite WAL writer-lock contention** when multiple agents coordinate against the same `.rally/facts.db`. Evidence: identical command completes in 8.0s cold / 0.02–0.06s warm under `--timeout-ms 60000` (not a hang, a budget overrun); same write dropped then succeeded 0.06s seconds apart while a second live session advanced the seq counter.

**Durable rule this must enforce:** for any append-to-canonical-then-rebuild-cache store, *a write succeeded only if it persists through a rebuild* — never trust the exit code / `ok:true`.

---

## 2. Objective

Codex executes this plan to (a) **find every instance of the silent-write-drop / fail-open-on-mutation defect class**, not just the one path above, and (b) land the durable fix. Output is a findings list split into a codex implementation lane and a claude verification lane.

---

## 3. Phase 1 — Reproduce deterministically (codex)

There is already a test seam: `RALLY_TEST_BLOCK_MS` (`lib.rs:527`, debug-only) sleeps inside `run_inner_with` to force a watchdog timeout.

1. Write a failing test that: sets a low watchdog budget + `RALLY_TEST_BLOCK_MS` above it, runs `rally say handoff …`, then asserts the fact **persisted to the ledger segment** (seq incremented AND survives a `RoomStore` reopen/replay). Today this test FAILS (write dropped, `ok:true` returned).
2. Add the mirror assertion: the command's exit code / envelope must NOT report success when the append did not land.

Acceptance: one red test in `crates/rally-cli/tests/` that encodes "timed-out write must not report success and must not silently vanish."

## 4. Phase 2 — Audit the defect class (codex) — THE "find issues" step

Enumerate and record findings for each. For every finding, capture: file:line, whether it can silently drop a mutation, and proposed owner.

- **A. Watchdog-posture matrix.** For every subcommand in `parse_cli` / the `CliCommand` match (`lib.rs:548+`), classify: is it a mutation (appends a fact / writes state)? What posture does `resolve_watchdog_posture` give it? Any mutation with `FailOpen` that emits `ok:true` on timeout is a finding. (`say`, `enter`, `ack`, `status post`, `backlog`, `lead`, presence auto-registration, read-checkpoint appends — check each.)
- **B. Open-before-append ordering.** Anywhere a durable append is sequenced *after* an expensive `RoomStore::open()` / projection within the same watchdog window (like `command_say`), the append is abandonable. Grep for `append_fact`, `append_fact_verified`, `maybe_append_read_checkpoint`, `append_state_transition_verified` and check each call's position relative to the expensive open/projection.
- **C. Fail-open envelopes that claim success.** Grep every site that prints `{"ok":true …}` or an empty/neutral envelope on an error/timeout path. Any on a mutation path is a false-success finding.
- **D. SQLite contention.** Is `busy_timeout` set on the connection? Is WAL checkpointing tuned? Under concurrent writers, does `open()` block indefinitely or return promptly? (This is the trigger — mitigating it shrinks the window.)
- **E. R9 stale-binary write-drop guard** (backlogged in the memory note): does the binary stamp a storage-model version into the ledger so replay can warn if a write arrived via an incompatible path? Still missing → finding.
- **F. Presence/wake/read-checkpoint auto-writes.** `ensure_presence` (`lib.rs:1368`) and `maybe_append_read_checkpoint` (`lib.rs:1290`) append facts as side effects of *read* commands. Do these silently drop under the same watchdog? If a read's side-effect write drops, coordination liveness rots.

Deliverable: `docs/plans/2026-07-03-D-findings.md` — a table of findings with severity + proposed lane.

## 5. Phase 3 — Durable fix (codex implements; claude verifies)

Land in this order (each with a test from Phase 1's harness):

- **Fix B (core):** In `command_say` (and every mutation found in 4B), commit the ledger append **before / independent of** the expensive projection, so the durable write is never abandonable by the projection watchdog. The append is O(1); only the room-render response should sit under the fail-open watchdog.
- **Fix A:** Mutating commands **fail CLOSED** on watchdog timeout — emit a non-ok envelope + non-zero exit, never `ok:true` for an uncommitted write. Generalize `emit_timeout_fail_closed_before_write` (`lib.rs:483`) to a `emit_timeout_fail_closed_mutation` and route all mutations through it. Reads stay fail-open.
- **Fix D:** Set SQLite `busy_timeout` (e.g. 5s) on the `RoomStore` connection and confirm concurrent writers queue instead of stalling `open()`. Optionally tune WAL auto-checkpoint.
- **Fix E (optional, R9):** storage-model version stamp + replay warning.

## 6. Acceptance / verification gates (claude lane)

- Phase 1 test goes green after Fix B+A; add a concurrency test (N parallel `rally say` under a real `RoomStore`) asserting **zero** dropped facts and **zero** false-success envelopes.
- All existing gates pass: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo audit`, `cargo deny check` (these are now CI-enforced by 11b0f07).
- Behavioral probe (the durable rule): post a fact → force a replay (`rally room`) → confirm the row SURVIVES. `ok:true` alone is NOT acceptance.
- CI green on the ubuntu `rally-gate` run for the fix commit(s).

## 7. Coordination (MECE, Rally)

- **codex lane (implementation):** `crates/rally-cli/src/lib.rs` (watchdog posture + emit_* + command_say ordering), `store.rs` (busy_timeout / append ordering), new tests. Claim these paths before editing.
- **claude lane (verification):** Phase 1/6 test authoring + review, concurrency test, CI watch, findings triage. Claude does NOT edit codex's claimed source files; verifies each fix on the ubuntu runner per push.
- Post findings + status to Rally; **verify every post LANDS** (seq increments and survives replay) — this plan's own subject matter. Use `--timeout-ms 30000` on writes while multi-agent contention is active.

## 8. Non-goals

- No change to the fail-OPEN posture for read-only/advisory commands (correct as-is; must never hang the host tool).
- No rewrite of the R5 segmented-ledger model. This is a write-durability + timeout-posture fix, not a storage redesign.
