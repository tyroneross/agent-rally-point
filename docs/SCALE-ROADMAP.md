<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Rally store — scale roadmap (measured)

North star: durable coordination for **thousands of agents across many terminals**, zero data loss.

Architecture: append-only JSONL segments (`.rally/log/<engagement>.jsonl`) are canonical;
`facts.db` (SQLite) + `.rally/.reconcile-cache.json` are **derived, disposable** caches;
a flock `mutation.lock` serializes writers.

## Shipped (2026-06-04, commits `5c68dac`..`79f3dbc`)

| Area | Before | After | Evidence |
|---|---|---|---|
| **Corruption** | malformed `facts.db` bricked the room; history lost (the 2026-06-01 incident) | quarantine + rebuild from canonical JSONL; torn trailing line + mid-page `SQLITE_CORRUPT` handled | end-to-end: real `rally room` recovers a corrupted store, 0 history loss, idempotent |
| **Reconcile** | O(N) full segment scan + full SQLite load on **every** open/append (under the flock → O(N²) burst) | **O(1)** happy path via fingerprint sidecar; full scan only on drift/corruption | unit: 150µs flat at n=200 and n=4000 (was ~12µs/fact) |
| **R9 readback** | scanned **all** segments after every verified write | active-segment tail-first; full scan only on miss | silent-drop detection unchanged (tested) |
| **Concurrency** | constant `pid%17` open jitter → intra-process thundering herd | thread-aware FNV jitter | parallel test flake 25% → 0% |
| **Integrity** | — | `SILENT_LOSS=0` at N≤128 in the scale harness | `scripts/scale_reliability_test.sh` |

## Measured remaining bottlenecks (do NOT guess — these are benchmarked)

1. **`snapshot()` / `read_db_event_count` is O(N)** — loads *every* fact via `store.query(FactQuery::all())`
   to build the projection (active claims, open handoffs, …). This now **dominates** command latency:
   `say` 16.6→32.6ms, `room` 5.4→9.6ms over 251→2001 facts (≈linear). Reconcile being O(1) does not
   flatten this — the projection does.
2. **Flock serialization** — every writer holds an exclusive lock across reconcile+SQLite+fsync+index.
   Scale harness wall-clock is ∝ N: 8→0.78s, 32→3.8s, 64→7.6s, 128→14.5s (≈0.11s/agent ⇒ ~110s at N=1000).
3. **Archive is replayed** — `replay_archive_segments` walks rotated segments too, so rotation alone does
   **not** shrink the reconcile/rebuild set. Bounded growth needs compaction, not just rotation.

## Recommended phases (sequenced by leverage × safety)

- **P1 — Projection → indexed SQL** (the user-visible speed win). Replace `read_db_event_count` full load
  with `SELECT COUNT(*)`; push the snapshot projections (latest-per-thread claim, unresolved handoffs,
  active blockers) into indexed queries (`idx_events_type`, `idx_events_occurred_at`) instead of folding
  all rows in Rust. Correctness-sensitive (drives coordination decisions) → own build-loop, TDD + audit.
- **P2 — Rotation + compaction.** Auto size-trigger rotation of the active segment, AND checkpoint rotated
  archive into `facts.db` so it is no longer re-replayed — bounds the hot set as total history grows.
- **P3 — `rallyd` single-writer daemon** ✅ SHIPPED (2026-07-11). A per-repo Unix-socket daemon owns one
  warm SQLite pool + in-memory projection; `rally` commands become thin clients (`rally daemon
  start|stop|status|serve`) that route over the socket when a daemon is live and **fail open** to today's
  direct path when not (no-daemon behavior byte-identical). A kernel SH/EX flock handover on
  `.rally/rallyd.owner.lock` guarantees exactly one writer (the daemon holds `LOCK_EX` for its lifetime;
  direct clients hold `LOCK_SH` for their process lifetime), so multi-process `facts.db` access — the
  measured root cause of the concurrency flake (issue #50: `SQLITE_IOERR_SHORT_READ` 17–33% + one
  `SQLITE_CORRUPT` under 8-way bootstrap contention) — is **structurally dissolved** whenever a daemon
  serves. Design forks resolved: explicit lifecycle (no auto-spawn), per-repo `.rally/rallyd.sock` (0600,
  `$TMPDIR` fallback past the `sun_path` limit) discovered via a `.addr` pointer, and same-user 0600
  socket auth. Acceptance: 30-round docker hammers (rust:1.95) of both #50 tests go **0/30 with the
  daemon serving**. The N=1000 flock-thundering ceiling on the *no-daemon* path remains (a daemon-served
  fleet raises it); the architecture does not preclude splitting the single dispatcher for read
  parallelism later.

Benchmarks: `scripts/scale_reliability_test.sh` (concurrency + `SILENT_LOSS`); latency-vs-ledger curve
seeds with `rally say artifact` (a persisting kind — `say note` is non-persisting and must not be used
to seed). Pre-register pass criteria: `SILENT_LOSS=0` at every N; command latency flat vs ledger size.
