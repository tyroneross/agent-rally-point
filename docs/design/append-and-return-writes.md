# Append-and-return writes: why Rally commands take seconds, and what to change

Status: design proposal, no code changed. Branch `design/append-and-return-writes-1005`, based on `main@e718d2f`.
Date: 2026-10-08. Author: Claude (storage/concurrency assessment requested by the owner).

## 1. Answer first

Every Rally command we measured burns its time on CPU work that grows with history, not on waiting for the SQLite lock. On a copy of Easy Terminal's room (45 MB of segments, 61.5k events), one uncontended `rally say` takes **~5.3 s** with no peers. About 89% of that time goes to work it repeats several times per command: loading or counting every row of `facts.db`, reparsing every canonical segment (at least 5 passes), rebuilding the room snapshot, and refreshing the indexes. SQLite lock waits never show up: 0 of 2,521 profiler samples. The default 3 s watchdog therefore kills a lone `say` on this room before its own append commits.

Concurrency makes it worse in a simple way. Direct mode allows **one process at a time** to own the room (`direct.owner.lock` EX, held for the life of the process), so N concurrent commands run one after another and latency grows linearly: about 8 s at N=2, 18 s at N=4, and failures at N=8 when the 30 s ownership wait runs out. The error "pool timed out waiting for an open connection" comes from **budget starvation**, not real contention. The SQLite busy and pool-acquire timeouts are fractions of the watchdog time that is *left*. A command that spent its budget queued behind a peer opens its pool with an acquire timeout of a few milliseconds and fails, even when nobody else holds the database.

So the owner's three options (a queue, parallel spools, sharding) all point at the wrong cost, unless the per-command O(history) work goes first. **Recommendation: option (b), append-and-return, delivered in stages, with Stage 0 being the removal of O(history) work from the write path.** Stage 0 alone should bring `say` from seconds to tens of milliseconds [INFERRED, estimated from the phase breakdown]. Stages 1–2 shrink the critical section to "allocate seq, write one line, fsync" (~5–10 ms) and move projection off the caller's path.

## 2. Measurement

### 2.1 Method

- **Subject.** A copy of `/Users/tyroneross/dev/git-folder/easy-terminal/.rally` taken with `cp -Rp` into a scratch repo (worktrees, `.hook-seen`, `dogfood` and quarantined `facts.db.corrupt*` excluded). The live store was never opened. At copy time: 48 segments, 45 MB, 61,542 events. The active segment (`team-chat-send-1005.jsonl`) holds 19 MB. `facts.db` is 49 MB. Line sizes: p50 739 B, p99 1,549 B, max 15,634 B.
- **Binary.** `rally 0.2.8+e718d2f` (current main), release build with line-table debuginfo so `sample(1)` resolves symbols. Cross-checked against the shipped `rally-core 0.2.8+533a46d` from Easy Terminal.app: same shape (`claims` 489 ms, `say` hits the 3 s watchdog).
- **Environment.** `HOME` pointed at a scratch dir, `RALLY_DAEMON_AUTOSTART=0`, `RALLY_HOOKS=off`, `RALLY_NO_AUTO_REAP=1`, session env unset. This is **direct (daemonless) mode**, the mode Easy Terminal's repo was in at measurement time (no `rallyd` alive). `--timeout-ms 60000` lifts the watchdog so the true cost is visible. Runs without it are noted separately.
- **Harness.** `bench.py` launches N processes at once and reports per-process wall-clock time (median / p90 / max). Single-writer numbers are medians of 3–5 runs; concurrent numbers are medians over all processes across 1–2 rounds.
- **Phase attribution.** No tracing flag exists on this path (`RUST_LOG` and timing env vars are not wired), so phases come from `sample <pid>` at 1 ms intervals on the `rally-command` thread. Each sample is attributed to the innermost matching frame.
- **Host caveat.** The host was heavily loaded by other agents (load average ~28–31 on 16 cores). Absolute numbers carry noise of roughly ±30%. Ratios and growth shape are stable across reruns.

### 2.2 Wall-clock by command and concurrency (median ms; 60 s budget)

| Command | N=1 | N=2 | N=4 | N=8 | Notes |
|---|---:|---:|---:|---:|---|
| process start (`--version`) | 17 | – | – | – | store-free floor |
| `claims` (read) | 489 | – | – | – | cold first open: 810 |
| `room` (read) | 739 | 1,575 | 1,926 | 3,307 | reads also serialize on direct ownership |
| `next` | 5,991 | 9,334 | 14,671 | 27,143 (5/16 fail) | appends presence |
| `check before-write` | 4,250 | 4,709 | 7,537 | 13,446 (max 24.2 s) | appends presence |
| `say artifact` | 5,272 | 8,073 | 18,361 (max 30.6 s) | 22,583 (3/16 fail) | 2 appends: presence + artifact |
| `enter` | 5,545 | ~13,700 (n=2 procs) | 14,893 | 29,438 (3/8 fail) | appends presence; 1 round |

With the default **3 s** watchdog, `say artifact` and `say blocker` failed **5/5 single-writer runs** ("mutating command exceeded 3000ms wall-clock budget before its primary durable append committed"). One of five `before-write` runs failed the same way. The N=8 failures are the 30 s ownership corridor expiring ("blocked on direct.owner.lock … no daemon has published a socket").

### 2.3 Phase breakdown of one `say artifact` (single writer, `sample` on `rally-command`)

Full uncapped run: 6.15 s wall, 2,521 samples (~2.44 ms/sample), attributed to the innermost matching frame (`cat_sample.py`).

| Phase | Share | ≈ ms | What runs |
|---|---:|---:|---|
| `facts.db` full load / count | 29.9% | ~1,840 | `facts_from_store` returns **every** event (before + after each projection append, `facts_under_lock`, `snapshot_cache_capture`); `read_db_event_stats` counts all rows twice for the reconcile sidecar. Worker threads spend only 74 samples in `sqlite3_step`; the rest is row/JSON decode |
| Canonical full reparse | 28.3% | ~1,740 | `canonical_segment_entries` over **all** 45 MB, ~145 samples (~350 ms) per pass, ≥5 passes: `resolve_canonical_event_id` (idempotency), `segment_seq_stats` (seq allocation + `refresh_reconcile_cache_after_append`), `facts_from_segments` (presence/claim checks) |
| Snapshot rebuild | 17.3% | ~1,060 | `snapshot_from_facts_with_policy_at` over every fact (`RoomStore::snapshot` in `command_say`, `snapshot_cache_capture`) |
| Log / claim index refresh | 13.4% | ~820 | `refresh_log_index`, `refresh_index` after each append |
| Other (parse, render, output) | 5.3% | ~330 | |
| Append write + fsync + readback | 3.0% | ~185 | `append_canonical_line_and_readback`. The fsync itself is ~4 ms (`F_FULLFSYNC` measured at 4.3 ms median); the rest is the readback |
| Active-tail inspection | 2.3% | ~145 | `inspect_active_segment_tail` reads the 19 MB active segment |
| `facts.db` open / pool | 0.2% | ~15 | |
| Reconcile on open | 0.2% | ~10 | fast path held (sidecar fingerprint matched) |
| **SQLite busy / lock / ownership wait** | **0.0%** | **0** | no samples in `flock`, `acquire_*`, SQLite busy handler, or sleep |

The command makes **two** full appends: the implicit presence fact (`command_say → ensure_presence → append_presence_tiered → append_fact`, 453 samples), then the artifact (1,692 samples, including its projection). In a separate run capped at the default 3 s watchdog, the profile window covered only the presence append and the user's artifact had not started when the watchdog fired. Every presence-posting command (`say`, `next`, `enter`, `before-write`) pays at least one O(history) append.

### 2.4 Where the time goes under concurrency

For N concurrent writers the per-process work stays the same (~5 s); the difference is the **queue**. `acquire_direct_ownership_or_route_bounded` polls `direct.owner.lock` every 10 ms, and the winner keeps it until the process exits (`install_direct_owner_once`). So process k waits roughly (k−1) × 5 s. Medians of ~N/2 × 5 s and maxima of ~N × 5 s match the table above. With the default 3 s watchdog the queue is fatal at N=2.

One earlier `before-write` N=8 round returned 23 ms per process. I could not reproduce it: the rerun measured 13.4 s median, and 8 concurrent fresh calls run by hand each committed a presence append. Treat the 23 ms round as an unexplained artifact ❓ and use the rerun.

| Phase (median ms per process, `say artifact`) | N=1 | N=2 | N=4 | N=8 |
|---|---:|---:|---:|---:|
| Process start | 17 | 17 | 17 | 17 |
| Ownership queue (`direct.owner.lock`) | 0 | ~2,800 | ~13,100 | ~17,300 (30 s cap → 3/16 fail) |
| `facts.db` full load / count | ~1,600 | ~1,600 | ~1,600 | ~1,600 |
| Canonical full reparse (≥5 passes) | ~1,500 | ~1,500 | ~1,500 | ~1,500 |
| Snapshot rebuild | ~900 | ~900 | ~900 | ~900 |
| Log / claim index refresh | ~700 | ~700 | ~700 | ~700 |
| Segment append + fsync + readback (fsync ≈ 4–8) | ~160 | ~160 | ~160 | ~160 |
| Tail inspect + facts.db open + reconcile + other | ~400 | ~400 | ~400 | ~400 |
| SQLite lock wait | 0 | 0 | 0 | 0 |
| **Total (measured)** | **5,272** | **8,073** | **18,361** | **22,583** |

Work rows are §2.3 shares scaled to the 5,272 ms N=1 median. Queue rows are derived as total − N=1 work. Rows at N>1 assume per-process work is unchanged, which is consistent with zero lock-wait samples [INFERRED].

### 2.5 Verdict on the hypothesis

**Refuted: the SQLite projection lock is not the dominant cost.** The dominant costs are:

1. **O(history) recomputation inside every append**: ≥5 full canonical reparses, several full `facts.db` loads/counts, 2 snapshot rebuilds and the index refreshes per `say`, with each command appending at least one presence fact.
2. **Process-lifetime exclusive ownership** in direct mode. It turns (1) into a serial queue across all processes, readers included.

SQLite shows up only as the **victim**. `retry_budget::budgets_for` derives `busy_timeout` (remaining / divisor) and the sqlx pool `acquire_timeout` (a fraction of that) from the watchdog time remaining. A process that spent ~2.9 s in the queue opens `facts.db` with an acquire timeout of a few milliseconds. The pool times out on its own connection setup, and the code reports this as "database stayed contended" [INFERRED from code; consistent with the reported error text and zero lock-wait samples].

Amplifier [INFERRED]: Easy Terminal's `RallyRefreshScheduler` watches `facts.db`, `snapshot.cache.json` and `claim-index.json` and runs `rally room --json` on each change. Every write therefore produces at least one ~0.7 s reader per open window, and each reader also queues on direct ownership.

## 3. Options

All latency estimates assume the Easy Terminal room (45 MB / 61.5k events) and are [INFERRED] from §2's phase costs.

### (a) Single-writer queue (rallyd, or Easy Terminal's ez-ptyd as optional owner)

One long-lived process owns `facts.db` and the segments. Clients send requests over a socket and fall back to direct mode when no owner answers. `rallyd` already exists (`rally daemon serve`, warm pool, `store_client` routing) and the router already prefers it.

- **Pros.** Pays process start, pool open and reconcile once. A resident owner can keep in-memory indexes (max seq, event-id set, live snapshot), which makes O(1) appends easy. One writer means no cross-process seq races. Easy Terminal could supervise it (ez-ptyd already supervises long-lived panes).
- **Cons.** As written today the daemon runs the **same** `append_fact_under_lock` code, so it serializes the same ~5 s of O(history) work: a queue of identical slow jobs [INFERRED; daemon mode not measured here]. "Daemonless" is a product contract (RALLY_ARCHITECTURE.md: "there is no daemon"), so the direct fallback must stay correct and fast anyway, which means two write paths to keep equivalent. A daemon adds lifecycle failure modes this repo has already paid for (socket takeover, wedged daemon, `rallyd.owner.lock` SH→EX handover). Making ez-ptyd the owner couples Rally's correctness to another product's release cadence.
- **Expected latency.** With today's append path, unchanged per-op (~5 s) and still queued. With Stage 0 indexes held in memory: ~2–10 ms per append plus one socket round trip.

### (b) Append-and-return (recommended)

Writers take a **short** lock, allocate the next seq from a cheap high-water mark, append one line with `O_APPEND`, fsync, and return. No SQLite on the caller's path. A projector catches `facts.db` up lazily: whichever process wins a non-blocking `projector.lock` applies the backlog. Readers never wait for it. They serve the projected view tagged `as_of_seq`, and reads that must be current fold in the segment tail beyond `as_of_seq`.

- **Pros.** The critical section becomes O(1): stat the tails, write, fsync, update the high-water mark (~5–10 ms). This removes both the queue and the budget starvation that produces the misleading SQLite error. It stays daemonless and keeps the segment as the commit point, which already matches the canonical-first design (`append_canonical_line_and_readback` commits before projection, and `projection_complete:false` exists today). A future daemon can host the projector unchanged.
- **Cons.** It needs explicit freshness classes on every read, a real cost: each read call site must be classified, and claim checks need a tail fold. Projection lag becomes visible: Easy Terminal's UI may show a fact a few hundred ms late. It adds sidecar state (seq high-water mark, event-id index) that must be validated against the canonical tail on every use, or it becomes a second source of truth.
- **Expected latency.** `say` ~15–40 ms end to end (process start 17 ms + append ~5–10 ms + output). Before-write current-read: projected claim index + tail fold of ≤ a few hundred lines, ~5–20 ms. Projector: amortized, off the caller's path.

**Per-writer spool variant (owner's "parallel temp logs merged later").** Each writer appends to `.rally/spool/<pid>-<nonce>.jsonl` with no shared lock, and a merger assigns global seqs later. I recommend **against** it for the primary path: a writer cannot return a global `seq` at commit, so ordering between writers is only defined after merge. Every reader would have to union N spools to be current, which is the O(N) fan-in we are trying to remove. Idempotent retry would need to search spools plus the log, and the `merge=union` git story gets a second file family. The shared short flock costs ~5 ms per append (≈100–200 appends/s per room), far above observed demand, so the spool's lock-freedom buys nothing we need.

### (c) Shard the derived store per engagement or room

- **Pros.** Each shard's history is smaller. The active engagement here is 19 MB of 45 MB, so work would drop by roughly 2.4× [INFERRED]. Shards also stop contending with one another.
- **Cons.** It doesn't remove the O(history) shape; the cost returns as soon as an engagement grows. Seq is one global sequence across segments (`segment_seq_stats` takes max over all segments), and claims, presence, leads and obligations are repo-wide, so before-write still has to fan in across shards. It multiplies `facts.db` files, reconcile sidecars and corruption-quarantine paths. Ownership would still be per-room unless the lock is also sharded, which reopens cross-shard atomicity for claim checks.
- **Expected latency.** ~2 s per `say` on the active engagement, still queued. Not sufficient alone.

### Comparison

| | Removes O(history)? | Removes queue? | Daemonless? | New invariants | Est. `say` p50 |
|---|---|---|---|---|---|
| today | no | no | yes | – | 5.3 s (N=1), 18 s (N=4) |
| (a) queue, current code | no | moves it | no (fallback needed) | daemon lifecycle | ~5 s + queue |
| (a) queue + in-memory indexes | yes (in daemon) | yes | no | + fallback parity | ~10 ms + RTT |
| (b) append-and-return | yes | yes (5–10 ms critical section) | yes | seq HWM, freshness classes | ~15–40 ms |
| (c) shard | partially (~2.4×) | no | yes | cross-shard fan-in | ~2 s + queue |

## 4. Recommendation and staged plan

Adopt (b). Ship it in stages, each independently useful and each gated by a measured regression test on a synthetic 60k-event room.

### Stage 0: stop recomputing history inside an append (no format change)

Goal: cut a single-writer `say` from ~5 s to <150 ms without changing the commit protocol.

1. **Seq allocation from tails, not full reparse.** Seq = 1 + max over segments of the last valid line (O(#segments) tail reads, ~48 files). Keep a `log/.seq-hwm` sidecar `{max_seq, segment_fingerprint}` as a hint that is valid only when the fingerprint matches. Fall back to the full scan only on mismatch or on a tail that fails validation.
2. **Idempotency from an index, not a scan.** Fresh event ids (`fact_<pid>_<nanos>`) need no lookup. Look up caller-supplied retry ids in an event-id index: SQLite unique index on `json_extract(payload,'$.event_id')`, or a sidecar. A miss in an up-to-date index is authoritative; the full scan runs only when the index is behind.
3. **Projection by point query.** Replace the two `facts_from_store` full loads around the projection append with `SELECT … WHERE event_id = ?`. Replace `read_db_event_stats` (an O(N) load, flagged `TODO(perf)` in `store.rs`) with `SELECT count(*), max(seq)`.
4. **Incremental snapshot and indexes.** `snapshot.cache.json` already exists. Advance it by applying facts with `seq > cache.as_of_seq` instead of rebuilding from all facts. Do the same for `claim-index.json`, `refresh_log_index` and `refresh_index` (13% of the profile), and take one snapshot per command instead of two.
5. **Readback bounded to the written bytes.** Read back from the pre-write file length, not the whole active segment, and inspect the tail only in its last few KB.
6. **Debounce presence.** Skip the implicit presence append when this tool session posted presence within its heartbeat window. This removes one of the two appends in every `say`.
7. **Scope direct ownership to the command's store work, not the process lifetime**, or at least release it before output rendering. It can be done in Stage 0 or Stage 1 and must keep the daemon SH→EX handover semantics.

Expected: ~89% of measured time removed, so `say` p50 drops to roughly 50–150 ms on this room, and N=4 drops to <0.5 s [INFERRED].

### Stage 1: append-and-return commit path

- Critical section under `mutation.lock` (flock, already exists): read the seq HWM and validate it against the active tail; write the line with one `write(2)` on an `O_APPEND` fd; `F_FULLFSYNC`; fsync the directory on segment creation; update the HWM. Return `{event_id, seq, projection: "pending"}`.
- Check-then-append facts (claim, release/takeover, claim.expired) evaluate their precondition **inside** the same critical section against "projected claim index + segment tail beyond its `as_of_seq`". This keeps today's SEC-001 re-check-under-lock guarantee at O(tail) cost.
- No `facts.db` open on the write path. The watchdog's OutcomeUnknown window shrinks to the write+fsync.

### Stage 2: lazy projector and freshness contract

- **Projector.** After returning, a writer tries `projector.lock` with `LOCK_NB`. Readers try the same when they notice lag above a threshold. On success it applies `(db_hwm, log_hwm]` in seq order in one SQLite transaction, then advances the snapshot, claim index and reconcile sidecar. On failure it leaves the work to whoever holds the lock and never waits.
- **Freshness classes** (each read call site declares one):
  - `stale-ok`: `room`, `recent`, `claims` listing, Easy Terminal's UI refresh. Serve projected state with `as_of_seq` and `lag` in the JSON envelope.
  - `current`: `check before-write`, `next` obligations, `inbox`, claim/release preconditions. Projected state plus an in-memory fold of segment lines with `seq > as_of_seq`. If the tail exceeds K lines (e.g. 2,000), try the projector first, then fold.
- Easy Terminal refreshes on `log/.seq-hwm` changes instead of `facts.db`, which removes the self-trigger amplification.

### Stage 3 (optional): resident owner

Once Stages 0–2 hold, `rallyd` (or ez-ptyd as supervisor) can host the projector and keep the HWM and indexes in memory. The direct path stays correct and fast, so the daemon becomes a pure optimization rather than a correctness dependency.

## 5. Correctness invariants

| # | Invariant | How it holds |
|---|---|---|
| I1 | Seqs are unique and strictly increasing in commit order within a clone | Allocated only under `mutation.lock`. HWM = max(sidecar if fingerprint-valid, last valid line of every segment). Tails are authoritative, so a stale or missing sidecar cannot cause reuse |
| I2 | Commit point = durable line in a segment | `F_FULLFSYNC` of the segment (and directory on create) before returning. Projection never precedes the canonical line (unchanged from O26) |
| I3 | Torn writes never become facts | Keep `read_segment_entries` framing: a malformed final fragment without `\n` is ignored, and `inspect_active_segment_tail` + `apply_active_tail_repair` truncate it under the lock before the next append |
| I4 | Crash between fsync and HWM update is harmless | The next allocator reads the tail, sees the higher seq, and repairs the sidecar |
| I5 | Idempotent retry | Same `event_id` + identical normalized value returns the existing row. A different value is an identity conflict. The index is checked against the log tail before a miss is treated as authoritative |
| I6 | Projection is idempotent and ordered | The projector applies strictly by seq range under `projector.lock`. A re-run after a crash skips seqs ≤ `db_hwm`. factstr's own `sequence_number` is mapped back to canonical seq via payload (as replay already does) |
| I7 | Current reads see every committed fact | Tail fold beyond `as_of_seq`. Check-then-append runs inside the mutation lock, so no fact can commit between the check and the append |
| I8 | Cross-worktree `merge=union` | Unchanged: duplicate-seq handling across merged branches stays in the replay path (O29). Stage 0 must not let the HWM hint mask a conflicting duplicate. A full scan still runs on fingerprint change |

**`O_APPEND` atomicity on macOS/APFS.** POSIX guarantees non-interleaving only for pipe writes ≤ `PIPE_BUF` (512 B on macOS). For regular files, `O_APPEND` makes the seek-to-end and the write atomic with respect to the offset, and XNU holds the vnode lock for one `write(2)` on a local filesystem, so concurrent single-call appends do not interleave in practice [INFERRED from XNU behaviour, not a documented guarantee]. An empirical check on this host (8 processes × 300 lines, `O_APPEND`, lines of 400 B / 4 KB / 16 KB / 64 KB, and 8 × 20 lines of 1 MB) produced **0 interleaved or torn lines**. Rally's max observed line is 15.6 KB, far above `PIPE_BUF`. A `write(2)` can still return short (signal, `ENOSPC`, quota), and Rust's `write_all` then issues a second syscall that another appender can interleave with. **Design rule: keep the `mutation.lock` around the append (it orders seqs anyway), and keep the torn-tail repair.** Do not rely on `O_APPEND` alone for multi-writer safety, and never on network filesystems.

## 6. Migration

- All new state is derived and rebuildable: `log/.seq-hwm`, the event-id index, `as_of_seq` in the snapshot and claim-index sidecars, and `projector.lock`. If any is absent, one full scan under the lock rebuilds it, the same pattern as `.reconcile-cache.json`.
- **Mixed versions.** Old binaries append without updating the HWM or index. New binaries tolerate that because tails are authoritative (I1) and index misses are validated against the tail (I5). Old binaries reading a lagging `facts.db` hit today's reconcile fingerprint mismatch and rebuild, which is slow but correct. Ship Stage 0 first: it changes no on-disk contract.
- Stage 1 changes the envelope (`projection: "pending"`). Callers that read `projection_complete` (Easy Terminal, hooks) must treat pending as success. Gate the flip behind a config flag for one release.
- Easy Terminal: switch the refresh trigger to `log/.seq-hwm`, and display `as_of_seq`/lag rather than assuming read-your-writes.

## 7. Tests that prove it

1. **Perf regression (gate).** Generate a synthetic room of 60k events / 45 MB with the same line-size distribution. Assert single `say` p50 < 150 ms (Stage 0) and < 50 ms (Stage 1), and N=8 concurrent `say` max < 1 s. Count O(history) calls per append with test probes like the existing `note_full_reconcile_scan`, and assert **zero** `canonical_segment_entries` and `facts_from_store` full loads on the steady-state append path.
2. **Seq stress.** 16 processes × 500 appends with random sizes up to 64 KB. Assert seqs are exactly `{base+1 … base+8000}`, every event id appears once, no torn lines, and `facts.db` equals a replay after the projector drains.
3. **Crash injection.** Reuse the O26 fault points and add `AfterFsyncBeforeHwm`, `ProjectorMidBatch` and `AfterHwmBeforeReturn`. Kill at each; assert I1–I6 on restart and that a retry with the same event id returns the original.
4. **Freshness.** With the projector disabled, process A claims `path`; process B's immediate `before-write` on `path` must see the conflict. A `stale-ok` read must report `lag ≥ 1`.
5. **Budget starvation.** Hold `mutation.lock` for 2.9 s, then run a mutation with the 3 s watchdog. Assert a typed busy/not-started error, never "database stayed contended".
6. **Mixed version.** Interleave appends from `0.2.8` and the new binary on one room; assert no seq reuse and a converged projection.
7. **`O_APPEND` torture** (macOS CI). Turn the §5 experiment into a test so a platform change shows up as a failure.

## 8. Open risks

- **Classifying freshness at ~200 call sites.** A mis-classed `current` read silently weakens a coordination guarantee. Mitigation: default to `current` and opt in to `stale-ok` explicitly.
- **Presence debouncing changes liveness semantics.** The reaper and liveness use presence age, so the debounce window must stay below the liveness window.
- **factstr seq mapping.** factstr assigns its own `sequence_number`. Projecting batches out of a single process must keep the canonical-seq mapping that replay relies on (`replay_tolerates_non_contiguous_seqs`).
- **Daemon mode was not measured.** Whether `rallyd` today shows the same per-op cost is [INFERRED] from shared code, not measured.
- **Measurement noise.** Loaded host (load ~30), n=1–5 per cell. The one anomalous N=8 `before-write` cell is unexplained.
- **The per-writer spool variant was rejected on reasoning, not measurement.** If future demand exceeds ~100 appends/s per room, revisit it.

## Appendix: reproduction

Scratch harness (not committed): `bench.py` (N-way concurrent launcher), `cat_sample.py` (phase attribution from `sample` output), `oappend.py` (interleave test). Steps: `cp -Rp <repo>/.rally` into a fresh `git init` dir (skip `worktrees/`), set `HOME` to scratch, `RALLY_DAEMON_AUTOSTART=0 RALLY_HOOKS=off RALLY_NO_AUTO_REAP=1`, then run `rally --timeout-ms 60000 say artifact --tool bench:<i> --subject … --json` N-way, and `sample <pid> 3 1` on one process.
