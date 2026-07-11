<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Handoff: rallyd S-P3 — implementer pointers

Plan of record: `docs/plans/2026-07-11-rallyd-S-P3-PLAN.md`. Read your chunk's rows in the plan's Read-Before-Edit Map BEFORE editing. Dispatch order: A serial → B ∥ C → D. B and C build only against the contract A froze; any contract gap is a stop-the-line event back to a serial A-amendment — do not patch the contract unilaterally.

## Chunk A (opus) — foundation, freezes the interface
- Read ADR-02, ADR-03, ADR-05 and satisfy T-01 (build) + T-02 (behavior-identical).
- Rename `RoomStore::open/open_at/open_at_with_engagement/open_existing_at` (store.rs:742/:766/:778/:812) to `open_direct*`; introduce the `RoomStore { Direct, Routed }` enum; router hard-wired to Direct in this chunk; Routed arms return a "routing not yet enabled" error.
- Enumerate ALL ~28 `pub(crate)` RoomStore instance methods (store.rs:742–1753) × all 214 call sites; classify routed vs local; commit the table as doc comments on the wire enum. Completeness = grep-diff empty. The daemon core (B) is itself a consumer — check the facade against it too.
- Wire types → `crates/rally-protocol/src/store_wire.rs`: closed `StoreRequest`/`StoreResponse`, per-request `engagement`, error mapping to existing `RallyError` kinds, `store.ping → {repo_root, pid, wire_version: 1}`. VERIFY every routed variant's serde derives compile before ACKing the freeze (plan Risk row 1).
- Owner-lock helpers beside `acquire_room_mutation_lock` (store.rs:674): same hand-declared `extern "C"` flock (:69), add `LOCK_SH`/`LOCK_NB`; `acquire_owner_shared_nb` + `acquire_owner_exclusive_blocking`; non-unix no-op mirror of :696.
- `crates/rallyd` thin bin + workspace member + `rallyd_core::serve()` stub: `cargo build -p rallyd` must pass from this commit on.
- Gate before fan-out: full suite outcomes identical to main; B and C ACK the frozen contract.

## Chunk B (sonnet) — daemon core
- Read ADR-01 (startup ordering is NORMATIVE), ADR-05, and daemon_client.rs:639–700 for the exact wire framing clients expect. Satisfy T-01, T-05.
- Startup order: EX flock (blocking, log if waiting) → `open_direct_at` (the ONE store) → resolve socket per L7 (>103 bytes ⇒ `$TMPDIR/rallyd-<sha256(root)[..12]>.sock`) → unlink stale → bind → chmod 0600 → write `.addr` + pid (0600) → serve.
- std-only: accept loop (nonblocking + 100ms shutdown poll), per-conn reader threads, mpsc → ONE dispatcher thread owning the RoomStore. Apply each request's engagement scoped per request (never the daemon's env). mtime-checked `refresh_log_index` before reads; micro-benchmark the check ([ASSUMED] row).
- SIGTERM/SIGINT: drain → drop store → unlink socket/.addr/pid → exit (EX released by kernel). `--idle-exit-secs N` optional, default off.
- Falsifiers to self-test locally: engagement X lands in segment X; second rallyd blocks/exits loudly; perms 0600; malformed line ⇒ structured error, no panic.
- Never touch: store.rs, lib.rs, store_client.rs (C owns them); factstr internals (Guardrail G6); no spawn/schedule/LLM (G5 grep must stay clean).

## Chunk C (sonnet) — thin-client routing
- Read ADR-01 choreography and satisfy T-02, T-04, T-08. Router order is NORMATIVE: `.addr` → ping (verify repo_root) → live ⇒ Routed (NEVER take SH, NEVER open facts.db — reads included) → not live ⇒ SH try-NB ⇒ acquired (store process-global, hold to exit — G7) ⇒ `open_direct*` unchanged ⇒ refused ⇒ re-probe 3×250ms ⇒ else FAIL LOUD naming `rally daemon status`/`stop`.
- `store_client.rs`: mirror `round_trip` from daemon_client.rs:639 (line-delimited, 3s timeout). Do NOT reuse `rally_owned_socket()` (:63) — that is ptyd's socket, a different daemon.
- Fill the Routed dispatch arms in store.rs against A's frozen wire enum. `RoutedRoomStore` must contain no fact_store field (G3).
- `rally daemon start|stop|status|serve` verbs in lib.rs; `serve` calls `rallyd_core::serve()` — test fixtures depend on this exact path (`CARGO_BIN_EXE_rally daemon serve`).
- The CAS loop at lib.rs:4324 stays byte-identical — its two legs (store.rs:1464/:1329) route transparently.
- Guardrails G1, G2, G8 are yours: no edits inside `open_direct*` bodies; chokepoint grep clean; wire errors map to existing RallyError variants with exit-code parity.
- Never touch: rallyd_core.rs, crates/rallyd/** (B owns them).

## Chunk D (sonnet) — tests + acceptance hammer
- Implement T-03/T-04/T-05/T-06/T-08 in `tests/rallyd_handover.rs` (specs in plan §Chunk D). T-03 includes the lsof no-facts.db-fd assertion and the git-status-clean teardown (G9).
- Fixture wiring ONLY in watchdog_concurrency.rs:185 and user_journey.rs:2023 — spawn `CARGO_BIN_EXE_rally daemon serve` against the temp repo, block on successful ping, register kill+cleanup; assertions untouched. Deep `$TMPDIR` paths exercise the L7 fallback for free.
- `scripts/hammer-rallyd.sh`: docker rust:1.95, 30 rounds each test, per-round daemon logs captured — reuse the issue-#50 harness pattern; NO new verdict source.
- Done = F4: both hammers 0/30 + `scripts/run-quality-gate.sh` exit 0 + pre-push green. Unit-green with a red hammer is NOT done.
