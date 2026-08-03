# RCA — parallel-launch "lost session" (CI run 30591797638, 15/16)

Independent root-cause investigation, 2026-07-30. Repo `agent-rally-point`, defect commit `280840d` (origin/main), test `rally_run_reserves_numbered_ids_under_parallel_launch` (crates/rally-cli/tests/user_journey.rs:2189 at 280840d).

## 1. Headline

SQLite's last-connection-close WAL housekeeping, running on a background thread AFTER the room mutation flock is released (factstr-sqlite 0.5.2 never closes its pool in `Drop`), unlinks a live `facts.db-wal` containing committed-but-never-checkpointed session facts; the next `rally run` then reads an empty/rewound store, its "CAS" validates against that stale view and succeeds, and two processes exit 0 holding the SAME numbered identity — which the session projection (a `BTreeMap` keyed by `session_id`) collapses to one row.

The test's count assert (16 exits-0, 15 rows) is the shadow of the real broken invariant: **uniqueness**, not persistence. The "reservation is CAS-atomic" comment in the test is false in direct (no-daemon) mode.

## 2. Verdict

**Real substrate defect, not a test artifact.** `rally run` returns exit 0 while its committed session fact is silently destroyed, and a second `rally run` is handed the same identity.

Deciding evidence (all reproduced locally, primary data preserved under the session scratchpad `r2-*`/`r3-*`/`r5-*` workspaces):

- Ledger forensics of a failing run: 24 JSONL segment lines, all children exit 0, names `claude-01/02/03` each reserved TWICE, zero `stopped` facts, zero duplicate ledger seqs. Projection (`active_session_facts_from_facts`, crates/rally-cli/src/lib.rs:6581 — `BTreeMap` insert by `session_id`) collapses the pairs → count short by the number of duplicate pairs. CI's 15/16 = one duplicate pair.
- Instrumented trace (temporary local tracing, since reverted) caught the wipe live: after 13 committed reservations (store context versions 1..13), the next reader got `READ version=None count=0` on the **same facts.db inode**, and factstr's internal sequence numbers restarted at 1. The 13 events survive in the JSONL segments but are gone from SQLite.
- A 2 ms filesystem poller filmed the mechanism: a `facts.db-wal` (12,392 bytes, holding a committed reservation) was **unlinked ~80 ms after the owning op returned**, with no rally op in flight; `facts.db` main stayed schema-sized (36,864 B) throughout — every committed row lived only in the WAL. Neither `reconcile` rebuild nor `quarantine_corrupt_db` fired (both were trace-instrumented; zero hits).
- Mutation check (both directions):
  - **Fix-mutation**: patching factstr-sqlite locally so `Drop` closes the pool synchronously (making sqlite's close-time checkpoint+unlink happen inside the caller's flock window) → **0/72 workspaces with duplicates** (1,728 launches).
  - Unpatched (HEAD, including the new reservation lock + readback) → duplicates still occur (see §3 rates).

**Production blast radius**: this is not confined to session reservations or to tests. In direct mode (no rallyd — the default for every hook-driven `rally` invocation from Claude Code/Codex sessions), ANY burst of concurrent rally CLI processes can lose ANY facts whose only durable SQLite home is the WAL: `say`, `inject`, claims, handoffs. The canonical JSONL segments retain the data (segment append is fsync'd under the flock), but the derived `facts.db` silently diverges and — because of the fingerprint blindness in §4d — is **never healed** by reconcile. Consequences seen in the data: duplicate session identities handed to two agents (inject/handoff misdelivery), and projections (sessions, room, next) computed over a silently truncated fact set. Frequency scales with process concurrency; a single interactive user is unlikely to hit it, a multi-agent swarm hammering one room is the worst case.

## 3. Reproduction

Environment: macOS (APFS), M-series 16 cores, binary built from the working tree. `n = available_parallelism*4` clamped to 24 locally (CI: 16). All commands run from repo root.

1. In-suite loop (tree = 280840d + the then-uncommitted 36a2789 changes):

```
for i in $(seq 1 25); do cargo test -q -p rally-cli --test user_journey \
  rally_run_reserves_numbered_ids_under_parallel_launch -- --exact; done
```

→ **8/25 failed**. Failing workspaces persist in `$TMPDIR/rally-run-parallel-numbered-ids-*` (panic skips `cleanup()`); forensics above came from these.

2. Standalone hammer (no cargo-test load): script spawns 24 parallel `rally run claude --json --backend tmux --tmux-bin /usr/bin/true` with `RALLY_NO_WORKTREE=1` into a fresh workspace, then inspects the segment ledger. Six workspaces in parallel per wave to add load.
   - Pre-36a2789 tree: ~**5/102 workspaces** produced silent duplicates (all children exit 0); separate runs produced loud child failures (`database disk image is malformed`, exit 1 — a visible sibling failure mode, not this defect).
   - **Committed HEAD 36a2789** (reservation flock + readback): **1/72 workspaces** still produced `fails=0, dup=claude-01` — the exact CI signature. The fix reduces probability; the race is intact.
   - factstr-sqlite patched for synchronous close: **0/72**.

CI vs local: same mechanism, different dice. The failure needs (a) many concurrent cold-start processes, (b) a background pool-teardown landing in another process's open/append window. CI's 2-core runner at n=16 stretches the cold-start burst (more overlap per core); one observed hit at 280840d. Not a regression — `3be788e` (May 29) already logged this test as "one unrelated pre-existing flake."

## 4. Mechanism

File:line cites at HEAD `36a2789` unless noted.

a. **Reservation path**: `reserve_numbered_session` (crates/rally-cli/src/lib.rs:5139) reads session facts + context version (`session_facts_with_context_version`, crates/rally-cli/src/store.rs:2133), picks the next free number, then conditionally appends via `append_session_fact_if_context` (store.rs:1976). The conditional append is factstr-sqlite's `append_if` (`~/.cargo/.../factstr-sqlite-0.5.2/src/sqlite_store.rs:510`): `BEGIN IMMEDIATE`, re-read last matching sequence number, compare, insert. **Within one database view this CAS is sound.**

b. **The teardown escape**: direct mode opens a fresh store per op (`fact_store_handle`, store.rs:1445 — `warm_fact_store` is `None` for CLI processes). `SqliteStore::open` (factstr sqlite_store.rs:68) creates an sqlx pool (`max_connections(4)`, WAL, `synchronous=Full` — connection.rs:9); `SqliteStore::Drop` (sqlite_store.rs:430) shuts down its delivery thread but **never calls `pool.close()`**. sqlx closes the underlying sqlite handles later, on background worker threads — at an unbounded time after the rally op returned and released `.rally/mutation.lock`. sqlite's close protocol on the last-closing connection checkpoints and **unlinks `-wal`/`-shm`**. Under a 16–24 process stampede of short-lived pools, that unlink races other processes' open/append windows. The filmed outcome: a WAL holding committed frames is unlinked with `facts.db` main never having grown past the schema — the frames are destroyed. (The repo already knew the in-process variant: the "factstr-sqlite 0.5.2's un-closed-on-Drop background checkpoint racing the next open (issue #50)" note on `install_warm_fact_store`, store.rs:1461ff, is exactly this bug class; the daemon got a warm pool, the direct path did not.)

c. **From lost WAL to duplicate identity**: the next `rally run` opens the db and sees only the main file (schema, zero events, or a rewound prefix): trace shows `READ version=None count=0` after 13 commits. Its identity computation restarts at `claude-01`; its `append_if` expectation (`None`, or a low version) now MATCHES the gutted store, so the "CAS" **succeeds**. Both processes exit 0. Segment seq allocation (`next_canonical_seq` + segment-tail dup gate, store.rs:1991–2001) stays correct because it reads the JSONL files — so the ledger shows 24 unique seqs with duplicate `session.name`/`session_id` payloads, which the projection collapses.

d. **Why the safety net never heals it**: `reconcile_segments_and_db`'s fast path (store.rs:3526) trusts the sidecar cache when fingerprints match. `fingerprint_db` (store.rs:3388) hashes **only the main db file** — in WAL mode the main file's bytes never change on append, so WAL loss is invisible to it. Worse, `refresh_reconcile_cache_after_append` (store.rs:1738) advances `canonical_count`/`db_count` **by one from the previous sidecar** rather than recounting: the first post-wipe writer inherits the pre-wipe sidecar (13/13), writes 14/14 while the db truly holds 1 event, and every later op fast-paths on that self-consistent lie. The 13 lost rows are never re-derived from the (complete) segments.

e. **Ruled out** (each was a candidate in the brief):
   1. *Lost write via last-writer-wins interleaving* — ruled out as the primary story: duplicates reproduce even with the whole read→allocate→append sequence under the new cross-process `session-reservation.lock` (lib.rs:5148). The stale view comes from the storage layer, not from interleaving.
   2. *Read-side visibility at the final `sessions` read* — partially IN: the final read does see fewer rows, but because the db is durably gutted (and §4d prevents healing), not because of an in-flight uncommitted write.
   3. *Reaping/eviction* — ruled out: zero `stopped`/tombstone facts in any failing ledger; `sessions --json` without `--reap` never mutates; `Liveness::Unknown` is never reaped (liveness.rs:16).
   4. *Daemon divergence* — N/A: neither ci.yml nor scripts/run-quality-gate.sh sets `RALLY_TEST_RALLYD`; CI ran direct mode. (Daemon mode's single warm pool would dodge this bug — consistent with the F4 hammer passing at full N.)
   5. *Error swallowed into exit 0* — a real adjacent hazard (`run_with_watchdog`'s `ClosedMutation` posture exits 0 on timeout after any marked commit, lib.rs:~500) but NOT this defect: that path leaves the row present. 36a2789's `response["command"]=="run"` assert now guards it.
   6. *Test-harness fault* — ruled out: fresh per-test temp HOME/cwd; workspaces show no cross-test contamination. (Hygiene note: `serialize_rally_run`'s process-local mutex serializes nothing under cargo-nextest's process-per-test model — it protects only the multi-test-in-one-process case.)

## 5. Why the two prior fixes failed

- **`39970a8`** "kill user_journey parallel-launch flake at root": added SQLITE_BUSY retry jitter in `open_fact_store` and an in-process test mutex. Both address **visible lock-contention errors**, not the silent WAL unlink; neither touches pool teardown. Its own commit message concedes the un-fixed remainder ("Does NOT fully fix the simultaneous-burst scale ceiling (… seq-replay-conflict under cold cold-opens)"). Verification was pass-rate (53/53, 275 pass) — probability evidence on a probabilistic bug. **False "at root" claim.**
- **`a8bec15`** "scale rally_run parallel-launch test to host": reduced N on small runners and asserted "reservation is CAS-atomic — uniqueness holds at any N" — the exact invariant this RCA falsifies. Diagnosed the CI failure as an over-subscription **timeout** flake on the strength of one green rerun. Lowering concurrency only shrinks the race window. **False root cause.**
- (Context) **`15531fd`** fixed a real but different daemon accept-loop bug; CI runs no daemon. **`3be788e`** fixed a real but different destructive-rebuild trigger; rebuilds are not involved here (trace-verified zero rebuild/quarantine events in failing runs).
- **`36a2789`** (landed mid-investigation) — cross-process reservation flock + post-append readback + strengthened test asserts. The lock cannot help: the destroying unlink runs in a background close **after** any lock is released. The readback races the same close (observed unlink ~80 ms after op return; readback runs within ~ms). Empirically still 1/72 silent-duplicate workspaces. Valuable hardening (duplicate-session_id assert makes the failure loud and diagnosable), but if merged as "the fix" it becomes the third probability-reducer labeled a root cause.

## 6. Why it escaped controls

Separate from the defect:

- **Same gate, different dice.** Local pre-push and CI run the identical script; the defect fires per-run with p ≈ 1–5% (load-dependent, higher on slow/oversubscribed hosts). Every green local gate and green rerun was sampling, not proof. All three "recurrences" are the same untreated race crossing the threshold again.
- **The count assert misdirects.** Uniqueness collapse presents as `sessions.len() == 15`, which reads as "a write was lost," sending each investigation toward persistence/timeouts instead of identity. The uniqueness asserts sit after the count assert and never run. (36a2789's per-child session_id set closes this.)
- **The system hides its own evidence.** The reconcile cache's increment-only refresh plus a main-file-only db fingerprint (§4d) makes the post-wipe store look internally consistent, so post-mortem inspection of `.rally` shows a plausible state; and in some runs a later authoritative reconcile heals the db from segments, erasing the discrepancy — but not the already-issued duplicate identity.
- **Prior fixes were verified by hammer pass-rates** (e.g. "PASS 0/5", "30-round hammer"), which cannot distinguish "race removed" from "race made rarer." No prior fix was validated by a mutation-style check (make it worse / make it deterministic).

## 7. Proposed fix

Smallest durable change (two layers, both required):

1. **Close the store synchronously, inside the flock.** Make `SqliteStore` teardown block until the sqlx pool is fully closed (upstream factstr-sqlite fix: `Drop` spawns a thread, `runtime.block_on(pool.close())`, join — the same pattern its `open` already uses; or a rally-side explicit `close()` called by `FactStoreHandle::Fresh`'s drop path before the mutation-lock guard releases). Verified experimentally: this exact patch took silent duplicates from ~5/102 and 1/72 to **0/72** hammer workspaces. This also shrinks the loud `malformed-db` sibling flake (the split-brain `-shm`/`-wal` churn shares the same teardown escape) but does not fully remove it — that residue stays visible (exit 1), and is the lane already being worked in the `flaky-malformed-db-precondition` worktree.
2. **Make WAL state visible to the reconcile safety net.** `fingerprint_db` must cover `facts.db-wal` (len+mtime+head-hash like the main file), and `refresh_reconcile_cache_after_append` must not increment counts blindly — recount the db (cheap `SELECT count(*)`/`max`) or at minimum invalidate the sidecar when the wal fingerprint moved unexpectedly. This converts any future silent db divergence into an authoritative re-scan that heals from the canonical segments.

**Freeze note**: both layers touch the frozen v0.1.7 tree (`crates/rally-cli/src/store.rs`, `Cargo.toml`/factstr version). I am NOT proposing edits to the frozen snapshot `880e1442` / tree `f5a38d02`; this must land as a new release commit (and, for layer 1, ideally a factstr-sqlite 0.5.3 upstream bump).

**Adversarial test** (a control, not a demo): keep 36a2789's per-child duplicate-session_id assert (it converts the silent failure to a loud one), and add a deterministic interleaving test rather than relying on the hammer's dice: an env-gated test seam (debug-assertions only, like the existing `RALLY_TEST_BLOCK_AFTER_COMMIT_MS`) that delays store teardown by T ms — spawn A (reserve, delayed teardown), then B (reserve) during A's teardown window, then assert two distinct identities and 2 rows. Validity check by mutation, already performed at the mechanism level for the hammer variant: with the synchronous-close patch the duplicate signature is 0/72; with it reverted the same harness reproduces duplicates (5/102, 1/72, 8/25 in-suite) — i.e., the test fails before the fix and passes after, and the failure it detects is the defect, not noise. The deterministic variant should be red on 36a2789 and green on the layered fix before it is trusted as the gate.

## 8. Confidence

- ✅ Duplicate-identity collapse (not reaping, not a missing segment write) is what makes 16 exits-0 show N−k rows — verified by ledger forensics of preserved failing workspaces (dup names, zero stopped facts, complete segment set) and projection code read.
- ✅ Committed facts are destroyed by `-wal` unlink after op completion, main db never checkpointed — verified by instrumented trace (count rewind on same inode, factstr seq restart) + 2 ms file poller (unlink of a 12 KB wal at the wipe instant) + post-mortem SQLite dump (db holds only post-wipe events; segments hold all).
- ✅ Teardown runs outside the flock and is causal — verified by code read (factstr `Drop` closes nothing; sqlx background close) and by the fix-mutation: synchronous close in `Drop` → 0/72 duplicates.
- ✅ 36a2789 does not close the defect — verified by hammering the committed HEAD build: 1/72 silent-duplicate workspaces with the lock + readback active.
- ✅ CI ran direct (no-daemon) mode — verified by reading ci.yml + scripts/run-quality-gate.sh (no `RALLY_TEST_RALLYD`).
- ⚠️ The exact sqlite-internal interleaving of the unlink (which connection's close deletes a wal at same time as which reader's open, incl. the possible `-shm` split-brain where a fresh opener creates new sidecar files while a holder maps unlinked ones) is inferred from filmed file-state transitions and SQLite's documented close behavior, not from syscall-level attribution (no dtrace). The causal claim does not depend on this detail — the mutation check pins teardown as the trigger.
- ⚠️ CI failure-rate estimate (p ≈ 1–5%/run) is extrapolated from local rates on different hardware; the CI hit itself is a single sample.
- ❓ Whether the residual loud `malformed-db` failures (exit 1) share 100% of their mechanism with this defect — plausible (same teardown escape), but that lane belongs to the flaky-malformed-db-precondition worktree and was not root-caused here.

---
*Method note: reproduction and forensics used temporary local instrumentation (trace lines in store.rs, a cargo `[patch.crates-io]` factstr-sqlite fork in the session scratchpad). All reverted; `git status` shows no investigator changes. No Rally ledger writes were made.*
