<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Plan: rallyd — per-repo single-writer daemon for `.rally/` facts.db (BACKLOG S-P3)

> **Governing headline:** rallyd dissolves issue #50 structurally — exactly one process opens facts.db WHILE a daemon serves — by putting a kernel-enforced SH/EX flock handover in front of `RoomStore::open` (the single chokepoint all 214 call sites already flow through), routing every store operation of a live daemon over a line-delimited JSON-RPC Unix socket, and failing open to today's byte-identical direct path when no daemon exists (fail-open mode keeps N direct SH openers and today's flake, by design).

<!-- checklist
Item 1 — Auth guard: N/A: no HTTP routes. Local-socket equivalent: OS-enforced 0600 perms on `.rally/rallyd.sock` (see ADR-04 / F-auth). getpeereid rejected as v1 scope (same-user threat model already covered by perms).
Item 2 — External APIs: N/A: no new external API calls. Only internal reuse (factstr-sqlite 0.5.2, already a workspace dep — see Research Context).
Item 3 — Rate-limit criterion: N/A: no paid API calls.
Item 4 — Discoverability: CLI feature, no GUI. Surfaces: `rally daemon start|stop|status` verbs under `rally --help`; `rally daemon status` reports pid/socket/uptime; docs commit C6 updates RALLY.md + .rally/manifest.json pointers.
Item 5 — Server/client boundary: N/A: no Next.js/server-component boundary. Rust boundary: wire types in `rally-protocol` (shared crate), daemon core in rally-cli module, thin bin in crates/rallyd (ADR-03).
Item 6 — Concurrency: per write path — (1) facts.db writes: single daemon dispatcher thread owning ONE RoomStore with ONE warm factstr pool (`warm_fact_store` handle; the hot per-op re-open sites reuse it via `fact_store_handle()` — R1), requests totally ordered via mpsc; (2) direct fail-open writes: existing blocking `flock(LOCK_EX)` on `.rally/mutation.lock` (store.rs:674), unchanged; (3) daemon-vs-direct exclusion: NEW `.rally/rallyd.owner.lock` — daemon holds flock LOCK_EX for its lifetime, every direct open holds LOCK_SH for process lifetime (ADR-01); (4) run-id reservation: existing client-side CAS loop (lib.rs:4324) over two wire ops, unchanged semantics.
Item 7 — Observability: daemon lifecycle + per-request outcome logged: foreground mode → stderr; `rally daemon start` (background) → `.rally/rallyd.log`. Events: startup {pid, socket, repo_root}, shutdown {reason}, request {method, engagement, outcome, duration_ms}, refresh {segments_replayed}. `rally daemon status` is the operator surface.
Item 8 — Input validation: every wire request is serde-deserialized into the closed `StoreRequest` enum at the socket boundary; unknown method / malformed params → structured error response, never a panic; oversized line (> 8 MiB) → error + connection close. Daemon also verifies repo_root match on ping (guards stale/corrupt .addr pointer).
Item 9 — Stable ID traceability: U-01 → F-01..F-05 → D-01/D-02 → T-01..T-09 / A-01..A-05. Example chain: U-01 → F-03 (handover) → D-02 (owner-lock contract) → T-03/T-04. Every P0 row in the Spec Object carries test IDs.
Item 10 — JSON spec object: present — see "## Spec Object (JSON)".
Item 11 — Blocking-and-novel question gate: zero open questions. All forks resolved with evidence; remaining unknowns are labelled [ASSUMED:] in body (segment-refresh cost, factstr Fact/RoomSnapshot serde derive — each with in-plan verification step at Chunk A).
Item 12 — Low-reversibility ADRs: ADR-01 (SH/EX ownership-lock handover + corridor policy), ADR-02 (wire protocol home + shape), ADR-03 (crate structure: core-in-rally-cli, thin rallyd bin), ADR-04 (auth = 0600 perms), ADR-05 (std-only daemon, no tokio in rally-cli). All in "## ADRs".
Item 13 — Analytical lens: Pugh — option selection across the four design forks (each with named rejected alternatives); DSM — chunk dependency ordering (A freezes interface → B∥C → D).
Item 14 — Handoff document: docs/plans/2026-07-11-rallyd-S-P3-PLAN.handoff.md (sibling file, written).
Item 15 — Synthesis dimensions: N/A: no UI surface.
Item 16 — Risk reason: Chunk A `runtime protocol`; Chunk B `runtime protocol`; Chunk C `persistence contract`; Chunk D none (tests/scripts only).
Item 17 — UI input/output contract: N/A: no UI surface.
Item 18 — Dispatch tier per work item: A=opus (interface-freezing judgment), B=sonnet, C=sonnet (escalate→opus on 2 failures or lock-choreography ambiguity per standing org), D=sonnet. Justifications inline per chunk.
Item 19 — Env-var manifest: N/A: no new external service. New env vars introduced: none (discovery is file-based via `.rally/rallyd.sock.addr`; deliberately no socket-override env in v1 — per-repo discovery gives tests isolation for free).
Item 20 — Capability gap map: present — see "## Capability Gap Map".
Item 21 — Single-shot build guardrails: present — see "## Single-Shot Build Guardrails".
Item 22 — Read-before-edit map: present — see "## Read-Before-Edit Map".
-->

```yaml
plan_id: 2026-07-11-rallyd-S-P3
backlog_item: S-P3
modifies_api: true
scope_auditor_status: pending   # scope-auditor gate required before Phase 3 dispatch (wire types + new bin + new CLI verbs)
verified_by: plan-critic + scope-auditor (pending — downstream; this plan does not self-certify)
replan_of: 2026-07-11-rallyd-S-P3 (rev 1) — closes 11 plan-critic findings + 2 scope-auditor gaps (R1–R10, evidence-confirmed 2026-07-11)
```

## Goal

Build `rallyd`, a per-repo single-writer daemon that owns `.rally/` facts.db (one warm RoomStore = ONE warm facts.db pool held via the `warm_fact_store` handle — R1 — plus the in-memory projection) served over a per-repo Unix socket, and turn rally-cli commands into thin clients with a FAIL-OPEN no-daemon fallback. This removes multi-process facts.db access WHILE a daemon serves — the measured root cause of issue #50 (SQLITE_IOERR_SHORT_READ 17–33% at open+query under 8-way bootstrap contention; one SQLITE_CORRUPT on append; factstr-sqlite 0.5.2 never closes its sqlx pool on Drop, and its background close/checkpoint races other processes). Fail-open (no-daemon) mode deliberately keeps today's N-direct-opener behavior, flake included (intent.md byte-identity contract). Retry/lock choreography inside the multi-process design is exhausted (4 falsified patch variants on the #50 ledger — none re-attempted here); the fix is structural: remove multi-process access whenever a daemon serves. Charter-pure: rallyd records/serves/derives only — never decides/gates/schedules/spawns/executes; no LLM anywhere.

**Falsifiable goal statement:** with rallyd serving, 30-round docker hammers of both #50 acceptance tests go 0/30, while the full existing suite stays green with no daemon running (byte-identical no-daemon behavior). Either observation failing falsifies the plan.

## Locked Decisions

Analytical lens: Pugh — option selection across the four forks; DSM — chunk dependency ordering.

| # | Decision | Value | Reversibility | ADR |
|---|----------|-------|---------------|-----|
| L1 | Handover primitive | SH/EX flock on `.rally/rallyd.owner.lock`: daemon=LOCK_EX for lifetime, direct clients=LOCK_SH for process lifetime | LOW (on-disk coordination contract other processes rely on) | ADR-01 |
| L2 | Routing seam | Inside `RoomStore::open` — RoomStore becomes a two-variant dispatcher (`Direct`/`Routed`); all 214 call sites unchanged | MED | ADR-01 |
| L3 | Wire protocol | Line-delimited JSON-RPC (`{"id","method","params"}\n`, one-line reply) mirroring daemon_client.rs:639 `round_trip`; typed `StoreRequest`/`StoreResponse` in `rally-protocol` | LOW (runtime protocol) | ADR-02 |
| L4 | Crate structure | Daemon core = module in rally-cli (`rallyd_core.rs`, full `pub(crate)` store access); `crates/rallyd` = thin bin calling `rally_cli::rallyd_core::serve(ServeConfig)` (signature frozen in Chunk A — R5); `rally daemon serve` = same entry (what test fixtures spawn via `CARGO_BIN_EXE_rally`) | MED | ADR-03 |
| L5 | Daemon runtime | std-only: `UnixListener` + per-connection threads + mpsc to ONE dispatcher thread owning the RoomStore. No tokio in rally-cli | HIGH (internal) | ADR-05 |
| L6 | Lifecycle | Explicit only: `rallyd` bin + `rally daemon start|stop|status|serve`. NO auto-spawn in v1 (acceptance gate does not need it — fixtures start the daemon explicitly). Optional `--idle-exit-secs N`, default off | HIGH | ADR-03 |
| L7 | Socket + discovery | Daemon binds `.rally/rallyd.sock` (0600); if abs path > 103 bytes (macOS sun_path 104 incl. NUL) binds `$TMPDIR/rallyd-<sha256(repo_root)[..12]>.sock`. Daemon ALWAYS writes actual path to `.rally/rallyd.sock.addr` (0600); clients discover via `.addr` only — one mechanism, no branching. Socket bind + `.addr`/pid write happen BEFORE the daemon's store open/reconcile (normative startup order, Chunk B — R3); `rally daemon start` returns only after `.addr` exists AND a ping round-trips | HIGH | ADR-02 |
| L8 | Auth | 0600 socket perms, OS-enforced, same-user. No getpeereid, no cockpitd authz/crypto | HIGH | ADR-04 |
| L9 | Engagement scoping | Every wire request carries the CLIENT's resolved engagement label; the dispatcher applies it per request via A's frozen `set_engagement_scope` facade setter (single-threaded dispatcher makes the rebind safe — R4). Daemon's own process-global env is never consulted per request | LOW (runtime protocol) | ADR-02 |
| L10 | Scale honesty | Correctness at N≤16 is the win. Flock-thundering ceiling at N=1000 may remain; architecture must not preclude fixing it (single dispatcher can later split reads), but do NOT gold-plate for 1000 | — | — |
| L11 | Warm-pool reuse (daemon mode) | `DirectRoomStore` gains `warm_fact_store: Option<SqliteStore>` + a `fact_store_handle()` accessor; the daemon installs ONE pool at startup; the hot interior open sites (store.rs :907/:1340/:1471 + snapshot read :1498) reuse it; direct-CLI mode = `None` ⇒ per-op opens, byte-identical to today (R1) | MED (internal facade contract B and C build against) | — |
| L12 | Unreachable-corridor policy | SH refused ⇒ bounded-block connect (`DAEMON_TIMEOUT`=3s per attempt, extendable) retried up to a 30s corridor bound, THEN fail loud naming `rally daemon status`/`stop`; a Routed op on a dead socket fails fast with a retryable error and NEVER opens facts.db directly mid-command (R3/R6) | MED | ADR-01 |

## Scope

IN: new `crates/rallyd` bin crate; `StoreRequest`/`StoreResponse` wire types in `rally-protocol`; ownership-lock helpers + `RoomStore` router enum + warm-pool facade + engagement scope setter in `store.rs`; daemon core module (`rallyd_core.rs`) in rally-cli; routed store client (`store_client.rs`); `rally daemon start|stop|status|serve` verbs; handover-invariant + fail-open + daemon-serving tests; docker hammer script; docs (RALLY.md, `.rally/manifest.json` pointer).

### Out of scope

- The opinionated coordinator (docs/plans/2026-07-10-opinionated-coordinator-PROPOSAL.md §2) — a SEPARATE, later, opt-in client process.
- Any decide/gate/schedule/spawn/execute feature; any LLM anywhere in rallyd.
- Auto-spawn of the daemon (deferred; design note in ADR-03 keeps it un-precluded).
- Re-attempting any of the 4 falsified factstr patch variants from the issue #50 ledger.
- New concurrency verdict sources beyond the existing docker hammer harness pattern.
- Extracting/reusing cockpitd transport/authz/crypto (see "Modularity decision" below).
- Fixing the N=1000 flock-thundering ceiling.
- The known no-daemon flake: in fail-open mode today's #50 flake persists BY DESIGN (intent.md: no-daemon behavior byte-identical, including its flake).
- External non-daemon mutation of `.rally/log/` segments while the daemon serves — UNSUPPORTED (see Chunk B staleness model, R8): segments are gitignored (`.gitignore` default-denies `.rally/*`) and the daemon is the sole writer while serving (holds EX; all clients route), so there is no external append to race in the supported model; a manual mid-serve edit is operator error.

**Modularity decision (rebuts the naive "reuse cockpitd" hint):** cockpitd's transport/ is a TCP WebSocket (tokio TcpListener on 127.0.0.1) not a Unix socket; its authz.rs is a tool-allowlist, not peer-credential; its crypto.rs is dryoc, unneeded for local same-user 0600 sockets. Extracting them would import tokio+axum+dryoc into the `rally` binary path (which runs on every commit hook — Cargo.toml deliberately tunes for small/fast startup) to reuse nothing that fits. rallyd instead mirrors the ~40-line wire helper from daemon_client.rs:639 — the one piece that IS the right shape. cockpitd modules stay untouched.

## Research Context

This build is internal-reuse-driven; the only external research is a dependency-currency check. factstr AND factstr-sqlite latest = **0.5.2, confirmed via crates.io registry check 2026-07-11** (0.5.2 published 2026-05-11); no pool-Drop-fix successor exists, so the structural fix stands. The workspace already pins `factstr = "0.5.2"` / `factstr-sqlite = "0.5.2"` (Cargo.toml:23-24). The defect being dissolved is measured, not hypothesized: SQLITE_IOERR_SHORT_READ (522) at open+query 17–33% under 8-way bootstrap contention; one SQLITE_CORRUPT (11) on append. All reuse anchors verified against the working tree on 2026-07-11:

| Anchor | Location (verified ✅) | Reused for |
|---|---|---|
| Wire helper `round_trip(socket, method, params, timeout)` | daemon_client.rs:639; `DAEMON_TIMEOUT=3s` :44; fail-open posture; `rally_owned_socket()` :63 (ptyd's — NOT reused; rallyd gets its own per-repo socket) | Mirror the helper shape for `store_client.rs`. ptyd is a DIFFERENT daemon; zero socket collision (`~/.local/share/rally/ptyd.sock` vs `.rally/rallyd.sock`) |
| `acquire_room_mutation_lock(room_dir)` — raw `extern "C"` flock, LOCK_EX blocking, `.rally/mutation.lock`, release on Drop | store.rs:674 (Drop :701; hand-declared flock :69 — no `nix` crate) | Pattern for the NEW ownership-lock helpers (same extern, LOCK_SH\|LOCK_NB added); mutation.lock itself stays unchanged on the direct append path |
| Store seam: `RoomStore::open()` :742 → `open_at` :766 → `open_at_with_engagement` :778 → `open_fact_store_lenient` :794. Writes `append_fact` :901 / `append_fact_verified` :1150; reads `snapshot` :1498; `open_fact_store` :2333 | store.rs | The #50 race is N processes reaching :794 at bootstrap. Router intercepts at :742/:766 — before the CONSTRUCTOR's facts.db open; the interior per-op opens (next row) inherit the caller's held owner lock, SH or EX |
| Interior `open_fact_store{,_lenient}` call sites — 8 total, incl. per-op re-opens INSIDE methods: `append_fact` :907, `append_fact_verified` :1340, `session_facts_with_context_version` :1471, the snapshot read path :1498; plus cold recovery/reconcile free fns :1792 / :3135 / :3321 / :3359 | store.rs, grep 2026-07-11 ✅ | Why L11/R1 exists: a daemon holding "one warm RoomStore" but leaving these per-op would churn one factstr pool PER REQUEST — and 0.5.2's un-closed-pool-on-Drop background checkpoint would race the NEXT request's open IN-PROCESS, re-creating #50 inside the daemon. Hot sites reuse `fact_store_handle()`; the cold recovery/reconcile free fns stay per-op (rare paths) |
| Run-id reservation: `command_run` lib.rs:3731 → `reserve_numbered_session` lib.rs:4324 — CAS loop over `session_facts_with_context_version()` store.rs:1464 + `append_session_fact_if_context` store.rs:1329 | lib.rs / store.rs | Both legs become wire ops; the CAS retry loop stays client-side (charter-pure: daemon never computes identity). Multi-op shape is why the mid-command dead-socket policy (R6, T-09) is pinned in A |
| Acceptance tests | watchdog_concurrency.rs:185; user_journey.rs:2023 (both spawn raw `CARGO_BIN_EXE_rally` subprocesses) | Subprocesses discover the fixture-started daemon via the temp repo's `.rally/rallyd.sock.addr` — no test-body changes to the assertion logic |
| Serde evidence for wire payloads | `Fact` round-trips through JSONL segments (store.rs replay); `RoomSnapshot` round-trips through the snapshot cache (`write_snapshot_cache` :3744 / `try_load_cached_snapshot` :3729) | ✅ both wire payload types already Serialize+Deserialize in production paths |
| RoomStore call-site count: 214 across 10 files; instance-method surface ~28 `pub(crate)` fns | grep, 2026-07-11 | Justifies L2 (route inside `open()`, not at call sites) and sizes Chunk A's enumeration |

## The Four Forks — decisions, rationale, rejected alternatives

### F-lifecycle → explicit lifecycle only (L6)

**Decision:** ship `rallyd` bin + `rally daemon start|stop|status|serve`. `start` daemonizes (fork/spawn `rally daemon serve` detached, log → `.rally/rallyd.log`, pid → `.rally/rallyd.pid`) and returns only after `.addr` is present AND a ping round-trips (R3 — a `start` return means clients can route); `stop` = SIGTERM to pid then verify EX lock released; `status` = ping + pid + socket path. NO auto-spawn.
**Why:** the acceptance gate does NOT require auto-spawn — Chunk D fixtures start rallyd explicitly against the temp repo before spawning CLI subprocesses (answering the fork's posed question). Auto-spawn adds a spawn path to the CLI hot path and a charter-adjacent smell for zero F-criterion coverage. Minimal effective action.
**Rejected:** (a) unconditional auto-spawn — charter risk (client silently spawning a process) + complexity, and would change no-daemon behavior (violates F2); (b) opt-in env-gated auto-spawn with single-spawn flock — sound design (a second flock where one spawner wins), kept as the documented follow-up so v1 doesn't preclude it, but cut because no v1 criterion needs it.

### F-socket → `.rally/rallyd.sock` + always-written `.addr` pointer (L7)

**Decision:** bind `.rally/rallyd.sock` perms 0600; if the absolute path exceeds 103 bytes (macOS `sun_path` is 104 incl. NUL), bind `$TMPDIR/rallyd-<sha256(canonical_repo_root)[..12]>.sock` instead. The daemon ALWAYS writes the actual bound path into `.rally/rallyd.sock.addr` (0600) — bind + `.addr`/pid write happen BEFORE the (possibly slow) store open/reconcile, per the normative startup order in Chunk B (R3) — and removes socket+addr+pid on graceful shutdown; on startup (only after EX acquired — proof no other daemon lives) it unlinks any stale socket file before binding. Clients discover exclusively via `.addr`. Health = connect + `store.ping` RPC; ping reply carries `{repo_root, pid, wire_version}` and the client verifies repo_root matches its own — guarding a stale or corrupt pointer. Stale socket/addr → not live → the ownership lock decides (fail-open to direct if SH acquirable).
**Why always-write `.addr`:** one discovery mechanism instead of two ("try sock, then addr") removes a client-side branch and makes the macOS-tmpdir fallback exercised on the SAME code path — the deep `$TMPDIR` test dirs will hit the fallback in CI, so it's tested for free.
**Rejected:** (a) client tries `.rally/rallyd.sock` first, `.addr` only on overlength — two discovery paths, the fallback one under-tested; (b) `$XDG_RUNTIME_DIR`/home-dir socket keyed by repo hash as primary — loses the "everything about a room lives in `.rally/`" property and complicates `rally doctor`; (c) an env-var override (`RALLY_RALLYD_SOCKET`) — unnecessary since discovery is per-repo (tests get isolation from temp roots), and every env knob is a new way to point two repos at one daemon. No collision with ptyd (different path family entirely).

### F-handover → SH/EX ownership flock, kernel-enforced (L1) — **the correctness heart, changed vs the recommendation**

**Decision:** new lock file `.rally/rallyd.owner.lock`, distinct from `mutation.lock`.

- **Daemon:** on startup acquires `flock(LOCK_EX)` **blocking** → binds socket + writes `.addr`/pid (cheap, immediate — R3) → THEN opens its direct RoomStore (reconcile may take seconds on a real room; connections accepted meanwhile block on their in-flight request until the dispatcher is ready) → serves. Holds EX for its entire serving lifetime; kernel releases on any death.
- **Direct client (fail-open path):** `RoomStore::open` first probes the socket (ping). If live → return `Routed` (the process NEVER opens facts.db, reads included). If not live → `flock(LOCK_SH | LOCK_NB)` (shared, non-blocking). Success ⇒ provably no daemon (EX excludes SH) ⇒ open today's direct store — which internally still uses `mutation.lock` for append serialization, unchanged. The SH guard is held for the **remainder of the process** (stored in a process-global; the kernel releases it at process exit).
- **SH try fails** ⇒ a daemon holds EX but its socket didn't answer yet ⇒ enter the **bounded-block corridor** (L12; ADR-01 corridor policy): CONNECT with a bounded-block timeout — `DAEMON_TIMEOUT`=3s per attempt, extendable — retried up to a generous **30s corridor bound**. This covers a cold `rally daemon start` whose reconcile takes seconds: concurrent commit-hook CLIs block on their in-flight request instead of failing. Only past the 30s bound (a truly wedged daemon) does the client **fail LOUD** with an actionable error naming `rally daemon status` / `rally daemon stop`. Never fall back to a direct write/read here — that corridor is exactly where dual-access would resurrect #50.

**Why SH for clients (the material change vs the brief's recommendation of a client-side exclusive try-lock):** with an exclusive-only ownership lock, two concurrent NO-daemon writers break: writer B's try-EX fails while writer A holds it, and B — finding no daemon to route to — would error. That regresses today's no-daemon concurrency and fails F2 outright (watchdog_concurrency spawns N concurrent direct writers with no daemon). LOCK_SH is shared among any number of direct processes (today's behavior preserved; `mutation.lock` still serializes their appends) while conflicting with the daemon's LOCK_EX in both directions. The invariant becomes a kernel theorem, not choreography:

> facts.db is only ever opened by (a) the daemon, which holds EX, or (b) direct processes, each holding SH. flock guarantees ¬(SH ∧ EX). Therefore no process opens facts.db while the daemon serves, and the daemon cannot begin serving until every in-flight direct process has exited. ∎ — *modulo the chokepoint premise:* every facts.db open happens inside a process that already holds an owner lock (SH or EX) — the only two store-opening entry points are the router's SH-guarded direct branch and the daemon's EX-guarded startup, and the interior per-op opens (store.rs :907/:1340/:1471 + cold recovery fns) inherit the caller's held lock (Guardrail G2's grep proves this).

**Why process-lifetime SH (not per-write):** the #50 defect is factstr-sqlite's *background* close/checkpoint racing after pool Drop. A guard released at end-of-write could let the daemon acquire EX while the client's detached close is still in flight. Holding SH until process exit means the kernel releases the lock only when the whole process — including any background close threads — is dead. This closes the residual window completely.
**Startup ordering + no lock inversion:** owner lock is always acquired BEFORE any store open; `mutation.lock` is only ever taken while the owner lock (SH or EX) is already held; the owner lock is never requested while holding `mutation.lock`. A pathological unbroken stream of overlapping SH holders can starve the daemon's blocking EX acquire (flock has no fairness guarantee) — accepted at N≤16 and surfaced by `rally daemon start` logging "waiting for direct writers to drain".
**Rejected:** (a) the brief's client-side exclusive try-lock — breaks no-daemon concurrency (above); (b) pid/marker file protocols — racy (check-then-act) and survive crashes as lies, exactly what flock's kernel release avoids; (c) reusing `mutation.lock` itself for ownership — it is acquired/released per append AND briefly inside `open_at_with_engagement` (store.rs:784); overloading it would either serialize all opens against the daemon forever or deadlock the daemon's own store open (daemon would hold ownership-EX on the same file its `open_at` re-acquires); (d) gating only writes, letting reads open facts.db directly while the daemon runs — reader pool Drops trigger the same factstr background close/checkpoint race; #50's short-reads were at open+query. Reads route too; (e) the prior draft's 3×250ms fail-loud corridor — falsified by the R3 evidence: cold-start reconcile takes seconds on a real room, so a 750ms corridor fails every concurrent commit-hook CLI on every cold `rally daemon start`; replaced by the bounded-block corridor (L12).

### F-auth → 0600 perms only (L8)

**Decision:** socket file (and `.addr`/pid/log) created 0600; `.rally/` itself is user-owned. OS-enforced same-user access is the entire v1 auth model.
**Rejected:** (a) `getpeereid()`/`SO_PEERCRED` defense-in-depth — viable via the same hand-declared `extern "C"` pattern as store.rs's flock, but adds platform variance (macOS getpeereid vs Linux SO_PEERCRED) for zero added coverage in the same-user local threat model; documented as the first thing to add if the socket ever leaves `.rally/`-perms territory; (b) cockpitd authz/crypto (wrong transport, wrong threat model — see Modularity decision).

## Dependency graph

```
A (foundation — serial, FREEZES: wire enum + socket/.addr contract + owner-lock protocol + facade signatures
   incl. warm-pool accessor, engagement scope setter, and the FINAL serve(ServeConfig) signature)
├──► B (daemon core)        — parallel with C after A lands
├──► C (thin-client routing) — parallel with B after A lands
└──► D (tests + hammer)      — after B AND C merge
```

Freeze discipline (memory lesson: freeze-complete-interface-before-parallel-fanout): B and C share ONLY the contract frozen by A. Any contract change discovered mid-B/C is a stop-the-line event routed back to a serial A-amendment commit, never patched unilaterally by B or C. A's enumeration must also be checked against *internal* consumers (memory lesson: frozen-iface-strands-internal-consumers) — the daemon core (B) is itself a consumer of the facade.

**Dispatch batching:**

```yaml
dispatch:
  - batch: 1
    chunks: [A]
    parallel_skipped_reason: "A is the serial interface-freeze; nothing may fan out until the wire+lock+facade contract is frozen and ACKed"
  - batch: 2
    parallel_batch: [B, C]   # B owns rallyd_core.rs + crates/rallyd/**; C owns store_client.rs + store.rs(Routed arms) + cli.rs + lib.rs(dispatch arm) — disjoint file sets, share only A's frozen contract
  - batch: 3
    chunks: [D]
    parallel_skipped_reason: "D validates the merged B+C surface (handover tests + daemon-serving hammers); depends on both"
```

Dispatch decision (plain-text record): batch 2 is a `parallel_batch` of [B, C] — disjoint owned-file sets, sharing only A's frozen contract. Batches 1 (A) and 3 (D) carry `parallel_skipped_reason` above (A = serial interface freeze; D = depends on both B and C).

## Depends-on (reads-from)

Every data path / contract the new code reads. Status verified = confirmed against the working tree 2026-07-11; unverified = a NEW contract this build creates (its correctness is a build output, gated by the named test).

| Read dependency | Kind | Status | Anchor / gated by |
|---|---|---|---|
| `.rally/log/<engagement>.jsonl` segments | canonical ledger (append-only) | verified ✅ | existing replay path store.rs; daemon appends segment-then-db exactly as today (D-01) |
| `.rally/facts.db` (factstr-sqlite pool) | derived, disposable cache | verified ✅ | `open_fact_store` store.rs:2333; only the daemon opens it while serving |
| `.rally/mutation.lock` (blocking LOCK_EX flock) | existing write-serialization | verified ✅ | `acquire_room_mutation_lock` store.rs:674 — UNCHANGED on the direct append path |
| daemon_client wire framing `round_trip` | wire shape to mirror | verified ✅ | daemon_client.rs:639 (ptyd's socket NOT reused) |
| RoomStore `pub(crate)` method surface (~28 fns, 214 call sites) | facade being wrapped | verified ✅ | grep 2026-07-11; classification table is Chunk A's falsifier |
| Interior `open_fact_store{,_lenient}` per-op re-open sites (:907/:1340/:1471/:1498 + cold :1792/:3135/:3321/:3359) | hot-path pool churn being fixed | verified ✅ | grep 2026-07-11; drives L11/R1 warm-pool facade |
| `Fact`, `RoomSnapshot` serde round-trip | wire payloads | verified ✅ | segment replay + snapshot cache (store.rs:3729/3744) |
| `.rally/rallyd.owner.lock` (SH/EX) | NEW handover contract | unverified (build output) | gated by T-03; ADR-01 |
| `.rally/rallyd.sock` + `.sock.addr` + `.pid` | NEW discovery contract | unverified (build output) | gated by T-03/T-04; L7 |
| per-request `engagement` label over wire + `set_engagement_scope` facade setter | NEW scoping contract | unverified (build output) | gated by T-05; L9/R4 |
| `warm_fact_store` handle + `fact_store_handle()` accessor | NEW warm-pool facade (daemon-mode pool reuse) | unverified (build output) | gated by B micro-test + T-07 hammers; L11/R1 |

## Activation Map

Every NEW component that is dormant until a trigger fires — each MUST have a test that exercises the trigger (memory lesson: build-loop ships dormant features; a capability with no activation path is not shipped). `verified-live: pending` until Chunk D lands the named test; then flipped to `yes`.

- `RoomStore::Routed` dispatch arms (store_client.rs) — trigger: `.rally/rallyd.sock.addr` present AND `store.ping` verifies repo_root (daemon live) — verified-live: pending (T-03 routed path holds no facts.db fd; T-05; T-07 hammers)
- `rallyd_core::serve(ServeConfig)` accept loop + dispatcher — trigger: `rally daemon serve`/`start` or `crates/rallyd` bin exec — verified-live: pending (T-01 `cargo build -p rallyd` + smoke; D fixtures spawn it)
- Owner-lock SH fail-open branch in the router — trigger: no live daemon AND `flock(LOCK_SH|LOCK_NB)` acquirable — verified-live: pending (T-04 no-daemon byte-identical)
- Bounded-block corridor + fail-loud exit — trigger: SH refused AND no successful ping within the 30s corridor bound (3s bounded-block per attempt) — verified-live: pending (T-08 wedged daemon → loud error, never direct write)
- `fact_store_handle()` warm arm — trigger: daemon startup installs `warm_fact_store` (daemon mode only; direct CLIs keep `None` ⇒ cold branch, today's per-op open) — verified-live: pending (B cold-branch-never-fires micro-test; T-07 hammers)
- Dead-socket fail-fast (mid-command daemon death) — trigger: a Routed op's socket write/read fails after routing began — verified-live: pending (T-09 retryable error, no direct facts.db open)
- macOS `sun_path` fallback (`$TMPDIR/rallyd-<hash>.sock`) — trigger: absolute `.rally/rallyd.sock` path > 103 bytes — verified-live: pending (B unit test both branches; D fixtures use deep `$TMPDIR` roots)
- Daemon crash → kernel EX release → client fail-open — trigger: daemon SIGKILL/panic while clients live — verified-live: pending (T-06 SIGKILL → next client fails open; db rebuilds)
- `--idle-exit-secs N` — trigger: flag set AND N idle seconds elapsed — verified-live: pending (B unit test with a short idle window)

## Chunks

### Chunk A — foundation: wire types, owner-lock helpers, RoomStore router shell + warm-pool/engagement facade, crate skeleton (SERIAL)

```yaml
chunk: A
dispatch_tier: opus   # interface-freezing judgment: classifying ~28 store methods route/local and freezing a wire contract that two parallel chunks build against — a wrong call here ripples into both
risk_reason: runtime protocol
modifies_api: true    # rally-protocol gains pub wire types; rally-cli gains the FINAL pub serve(ServeConfig) facade; workspace gains a member
owned_files:
  - Cargo.toml                                   # workspace members += crates/rallyd
  - crates/rally-protocol/src/store_wire.rs      # NEW: StoreRequest/StoreResponse + wire_version + transport-error mapping contract
  - crates/rally-protocol/src/lib.rs             # pub mod store_wire
  - crates/rally-cli/src/store.rs                # rename open()→router / open_direct_at(); RoomStore enum shell; owner-lock helpers; warm-pool facade (warm_fact_store + fact_store_handle()); set_engagement_scope; method classification
  - crates/rally-cli/src/rallyd_core.rs          # NEW: ServeConfig struct + FINAL pub fn serve(config: ServeConfig) signature (FROZEN here — R5); body is a STUB returning an unimplemented error — B fills the body only
  - crates/rally-cli/src/lib.rs                  # SCOPE-AUDIT GAP-1: ONE line `pub mod rallyd_core;` in the mod block (lib.rs:54-86) — without it `cargo build -p rallyd` (A's own checkpoint) cannot pass. MECE-safe: A is serial batch 1; C (the other lib.rs owner) edits lib.rs strictly after A in batch 2; B never touches lib.rs.
  - crates/rallyd/Cargo.toml                     # NEW thin bin
  - crates/rallyd/src/main.rs                    # NEW: parses args into A's ServeConfig, calls rally_cli::rallyd_core::serve()
falsifier: >
  Any RoomStore instance method reachable from lib.rs whose params/returns cannot serde
  round-trip (forcing a wire redesign after B/C fan out), OR any post-A behavior diff:
  the full existing suite must be green with the router hard-wired to Direct
  (`cargo test -p rally-cli` identical outcomes vs main).
```

Work items:

1. **Enumerate + classify the store surface** (the frozen contract's core): table of every `pub(crate) fn` in `impl RoomStore` (~28, listed store.rs:742–1753) × every lib.rs/module call site → classification `routed` (touches `self.fact_store`, i.e. facts.db: appends :901/:1150/:1197/:1329, `facts()` :1387, snapshots :1498/:1504/:1753, `session_facts_with_context_version` :1464, claim-lease ops :1398/:1405/:1421, read-checkpoint/receipt ops :1590/:1626/:1679, `room_id` :1135) vs `local` (pure accessors `repo_root` :1494, `active_engagement` :858, `active_segment_path` :863, `claim_index_path` :1416; cursors.json file ops :1520/:1528 — file-based, not #50 surface, unchanged). The classification table is committed into the chunk as doc comments on the wire enum. Completeness check: grep-diff of call sites vs table = empty.
2. **Wire types** in `rally-protocol/src/store_wire.rs`: closed enums `StoreRequest`/`StoreResponse`, one variant per routed method, every request carrying `engagement: Option<String>` (L9) and the reply mirroring the method's `Result` (errors as `{code, kind, message}` mapping onto existing `RallyError` variants — exit-code parity is part of the contract). Plus `store.ping` → `{repo_root, pid, wire_version: 1}`. **Transport-error mapping (R7):** all transport-layer failures — connect/read timeout, connection reset, oversized line, `wire_version` or `repo_root` mismatch — map to `RallyError::Command` (exit 1) with a message naming `rally daemon status`/`stop`; these classes have no direct-path equivalent, so they are excluded from T-04 goldens and get dedicated unit assertions (see G8). Zero new deps (serde only — matches rally-protocol's charter).
3. **Owner-lock helpers** in store.rs beside the mutation lock (same hand-declared `extern "C"` flock, adding `LOCK_SH`/`LOCK_NB` consts): `acquire_owner_shared_nb(rally_dir) -> Result<Option<OwnerGuard>>`, `acquire_owner_exclusive_blocking(rally_dir) -> Result<OwnerGuard>`; non-unix no-op mirrors of the existing cfg pattern (:696).
4. **RoomStore router shell**: today's `open()`/`open_at`/`open_at_with_engagement`/`open_existing_at` bodies become `open_direct*`; `RoomStore` becomes `enum RoomStore { Direct(DirectRoomStore), Routed(RoutedRoomStore) }` with all methods dispatching; in Chunk A the router ALWAYS returns Direct and `Routed` arms return a "daemon routing not yet enabled" error (C replaces both). Field-order note carried in code: the Direct variant stores no owner guard — the guard is process-global (L1 rationale). **Mid-command dead-socket policy (R6), pinned in this frozen contract:** a Routed op on a dead socket FAILS FAST with a retryable error (exit 1, message "daemon stopped mid-request; retry"); it must NOT silently open facts.db directly mid-command — that skips the SH choreography and breaks the G2 premise. The whole command is re-run by the caller, which re-enters the router (no daemon → direct; new daemon → route). Matters because multi-op commands exist (`reserve_numbered_session` CAS loop, lib.rs:4324). Gated by T-09.
5. **Warm-pool facade (R1/L11)**: `DirectRoomStore` gains `warm_fact_store: Option<SqliteStore>` (or the equivalent factstr-sqlite pool handle type — A confirms the exact type when it verifies serde derives) plus a `fact_store_handle()` accessor: returns the warm handle when present, else opens fresh (today's behavior, byte-identical — G1). The hot interior per-op open sites — `append_fact` :907, `append_fact_verified` :1340, `session_facts_with_context_version` :1471, the snapshot read path :1498 — switch to the accessor. Direct-CLI constructors always set `None` ⇒ per-op opens exactly as on main. The cold recovery/reconcile free fns (:1792/:3135/:3321/:3359) stay per-op (rare paths). B installs the warm handle at daemon startup.
6. **Per-request engagement scope setter (R4)**: `active_engagement` is fixed at construction today (store.rs:769/:796); a warm one-store daemon must serve per-request engagements (L9), so A freezes `DirectRoomStore::set_engagement_scope(&mut self, engagement)` on the facade — rebinds `active_engagement` (and its derived active-segment path) for subsequent ops. Safe because the daemon dispatcher is single-threaded; direct-CLI mode never calls it (constructor-fixed, byte-identical). B's dispatcher applies each `StoreRequest`'s engagement through it before dispatching the op. Gated by T-05.
7. **Crate skeleton**: `crates/rallyd` thin bin + workspace member + `rallyd_core::serve(ServeConfig)` — the `ServeConfig` struct (repo_root, idle_exit_secs, foreground) and the `serve` signature are FINAL as of A (R5); the stub compiles and returns an unimplemented error so `cargo build -p rallyd` passes from A onward (F1's build check bites early). B implements the body only; a mid-window signature change would break C's lib.rs call site at merge, so signature changes are stop-the-line A-amendments.

Integration checkpoint (gates the fan-out): `cargo build -p rallyd && cargo test -p rally-cli` green, outcomes identical to main; wire enum ↔ classification table ↔ grep of call sites mutually consistent; B and C leads ack the frozen contract (positive handoff ACK).

### Chunk B — rallyd daemon core (PARALLEL after A)

```yaml
chunk: B
dispatch_tier: sonnet   # implements against a frozen contract with the lock choreography fully specified; escalate→opus on 2 failures or any contract ambiguity
risk_reason: runtime protocol
modifies_api: true      # rallyd bin arg surface only (flags parse into A's FROZEN ServeConfig; the serve() signature itself was finalized in A — R5)
owned_files:
  - crates/rally-cli/src/rallyd_core.rs   # fills A's stub BODY (signature frozen) — accept loop, dispatcher, lifecycle
  - crates/rallyd/src/main.rs             # real arg surface parsing into A's ServeConfig (--idle-exit-secs; --foreground is implicit)
  - crates/rallyd/Cargo.toml
falsifier: >
  Any of: an append routed with engagement X landing in a segment other than X;
  a second concurrently-launched rallyd serving (it must block on EX or exit loudly);
  socket/.addr/pid files with perms other than 0600; a malformed request line
  crashing the daemon instead of yielding a structured error; a second facts.db
  pool opened while serving (fact_store_handle() cold branch firing in daemon mode).
```

Startup sequence (exact order is normative — R3): acquire owner EX (blocking; log "waiting for direct writers to drain" if not immediate) → resolve socket path per L7, unlink stale socket, bind `UnixListener`, chmod 0600, write `.addr` + pid file (0600) — cheap and immediate, so corridor clients have a socket to connect to during a cold start → `open_direct_at(repo_root)` opening ONE factstr pool and installing it as the store's `warm_fact_store` handle (R1; F1's "one connection + one projection" IS this warm store + warm pool) — reconcile may take seconds on a real room; the accept loop accepts connections during this window but requests dispatch only once the store is ready (accept early, dispatch after ready); if the store open FAILS, every queued and incoming request receives a structured error response, never a hang → start dispatcher → serve. The detaching `rally daemon start` parent returns only after `.addr` exists AND a ping round-trips (a completed ping implies the dispatcher is live).

Serving: accept loop thread (nonblocking accept + shutdown-flag poll, ~100ms); per-connection reader threads parse one line → `StoreRequest`; all requests funneled `(req, reply_oneshot)` over mpsc into ONE dispatcher thread owning the RoomStore — a total order over every store op (single-writer by construction; N≤16 needs nothing faster; later read-parallelism can split the dispatcher without touching the wire — L10). Dispatcher applies each request's engagement via A's `set_engagement_scope` before the op (L9/R4), and refreshes the segment index (`refresh_log_index`) before serving reads when any segment's **byte length OR mtime** changed (R8 — strictly stronger than mtime alone for an append-only ledger: same-second appends can't be masked by a length check). **Staleness model (R8):** while the daemon serves, it is the sole `.rally/log/` writer — it holds EX, every client routes, and segments are gitignored (`.gitignore` default-denies `.rally/*`), so no external append exists to race in the supported model; a manual mid-serve edit of segments is operator error, documented UNSUPPORTED (out of scope). The len+mtime gate is belt-and-braces within the supported model, keeping the zero-data-loss invariant intact for supported paths; replays are idempotent [ASSUMED: per-request len+mtime stat cost is negligible at N≤16 — verify in B with a micro-benchmark, else relax to time-bucketed refresh].

Lifecycle: SIGTERM/SIGINT → flag → drain in-flight requests → drop store → unlink socket/.addr/pid → release EX (kernel also covers crash). Optional `--idle-exit-secs N` (default off) exits after N idle seconds — test-hygiene against orphaned daemons. Logging per checklist Item 7. Charter purity is enforceable by grep: no `spawn`, no `Command::new` (except none), no scheduling, no LLM/client SDK anywhere under rallyd_core/crates/rallyd — F1's grep check.

Integration checkpoint: B-local smoke — start daemon on a temp room, ping over the socket, one append + snapshot round-trip via a raw socket write in a unit test; both concurrent-daemon and engagement-scoping falsifier tests pass locally. **Warm-pool proof (R1):** an in-process back-to-back-append micro-test asserting the daemon serves consecutive appends without opening a second pool — instrument `fact_store_handle()`'s cold branch (test-only counter or debug assertion) and assert it never fires while serving. The end-to-end proof is D's daemon-serving hammer rounds going 0/30 (T-07).

### Chunk C — thin-client routing in rally-cli (PARALLEL after A)

```yaml
chunk: C
dispatch_tier: sonnet   # mechanical dispatch arms + a router whose choreography ADR-01 specifies move-by-move; escalate→opus on 2 failures or cross-file surprise
risk_reason: persistence contract   # changes HOW facts.db may be opened — the ownership contract around the persistent store
modifies_api: true      # new `rally daemon start|stop|status|serve` verbs
owned_files:
  - crates/rally-cli/src/store_client.rs   # NEW: RoutedRoomStore — round_trip mirror (of daemon_client.rs:639, NOT reusing ptyd's socket), .addr discovery, ping/repo_root verify, dead-socket fail-fast (R6)
  - crates/rally-cli/src/store.rs           # router logic in open*(); Routed dispatch arms (replacing A's error stubs)
  - crates/rally-cli/src/cli.rs             # SCOPE-AUDIT GAP-2: `rally daemon` ARG PARSING lives HERE, not lib.rs — add `CliCommand::Daemon` variant (:9), `DaemonSubcommand` parser in the construct block (:761-959), and a `daemon` entry in `const COMMANDS` allowlist (:668, gated at :727). MECE-safe: no other chunk touches cli.rs.
  - crates/rally-cli/src/lib.rs             # `rally daemon` DISPATCH match arm only (the CliCommand::Daemon => … handler that routes to rallyd_core::serve / start / stop / status). NOTE: arg parsing is in cli.rs (above); lib.rs holds only the dispatch match + the `mod store_client;` line.
falsifier: >
  Either: (a) a Routed process opening facts.db by ANY path — checked by code review
  (RoutedRoomStore contains no fact_store field, no open_fact_store call) plus a
  runtime lsof assertion in D; or (b) no-daemon behavior diverging from main —
  the full existing suite run with no daemon must be green with outcomes identical to main.
```

Router in `RoomStore::open*` (exact choreography from ADR-01): read `.addr` → probe (connect + `store.ping`, verify repo_root) → live ⇒ `Routed` (SH never taken, facts.db never opened, reads AND writes) → not live ⇒ SH try-lock non-blocking → acquired (stored process-global, held to exit) ⇒ `open_direct*` (today's path, byte-identical) → SH refused ⇒ **bounded-block corridor (L12)**: connect+ping with `DAEMON_TIMEOUT`=3s per attempt, retried up to the 30s corridor bound (covers a cold daemon start whose reconcile takes seconds) ⇒ live ⇒ route, else FAIL LOUD naming `rally daemon status`/`stop` (a truly wedged daemon). **Mid-command daemon death (R6):** a Routed op that hits a closed socket fails fast with A's retryable error ("daemon stopped mid-request; retry", exit 1) — the router NEVER falls back to a direct facts.db open mid-command (that would skip the SH choreography and void the G2 premise); the re-run re-enters the router fresh. The CAS reservation loop (lib.rs:4324) is untouched — its two store legs route transparently. `rally daemon serve` invokes `rallyd_core::serve(ServeConfig)` (this is what fixtures spawn via `CARGO_BIN_EXE_rally`); `start` detaches a `serve` child with log redirection and returns only after `.addr` is present and a ping round-trips (R3); `stop` SIGTERMs the pid file's pid and confirms EX release (non-blocking EX probe succeeds) before returning; `status` reports ping result + pid + socket path.

Integration checkpoint: C-local — with no daemon, full suite green (byte-identical, F2); with a hand-started daemon on a scratch room, `rally say` + `rally room` round-trip and `lsof -p <cli pid>` shows no facts.db handle.

### Chunk D — tests, invariants, acceptance hammer (after B+C)

```yaml
chunk: D
dispatch_tier: sonnet
modifies_api: false
owned_files:
  - crates/rally-cli/tests/rallyd_handover.rs        # NEW: T-03 T-04 T-05 T-06 T-08 T-09
  - crates/rally-cli/tests/watchdog_concurrency.rs   # daemon-serving fixture wiring only (assertions untouched)
  - crates/rally-cli/tests/user_journey.rs           # daemon-serving fixture wiring only (assertions untouched)
  - crates/rally-cli/tests/json_envelope_contract.rs # envelope_daemon_status case (scope-audit advisory — daemon status --json honors the envelope)
  - scripts/hammer-rallyd.sh                          # NEW: 30-round docker hammer (rust:1.95, issue-#50 harness pattern — no new verdict source)
falsifier: >
  Docker hammer > 0/30 failures on either acceptance test with the daemon serving,
  OR the handover test constructing any interleaving where a client holds SH while
  the daemon holds EX (kernel should make this impossible — the test attempts to force it).
```

Tests: **T-03** handover invariant — daemon up ⇒ direct SH refused ⇒ client routes (and its process holds no facts.db fd, `lsof` assertion); daemon `stop` ⇒ SH acquirable ⇒ direct path; daemon `start` while a long-lived direct SH holder lives ⇒ EX blocks until that process exits. **T-04** fail-open — no daemon: behavior identical to main (suite subset + golden outputs); stale socket/.addr: treated as no-daemon. **T-05** engagement scoping through the daemon (exercises A's `set_engagement_scope` per request). **T-06** daemon crash (SIGKILL) ⇒ kernel releases EX ⇒ next client fails open cleanly. **T-08** wedged-daemon corridor — SH refused AND no successful ping within the (test-shortened) corridor bound ⇒ fail-loud error names the remedy, never a direct write. **T-09** mid-command daemon death (R6) — kill the daemon between two routed ops of one command (e.g. between the CAS legs); the second op fails fast with the retryable "daemon stopped mid-request; retry" error (exit 1) and `lsof` shows the process never opened facts.db. Daemon-serving fixtures: each hammer test's setup spawns `CARGO_BIN_EXE_rally daemon serve` against the temp repo (deep `$TMPDIR` paths exercise the L7 fallback), blocks on a successful ping (which completes only once the dispatcher is ready — R3), registers kill+cleanup; subprocess CLIs discover via `.addr` naturally (cwd = temp repo). Hammer script runs both tests 30 rounds in docker rust:1.95 and captures logs — the F4 verdict artifact.

Integration checkpoint (= release gate): F4 — both hammers 0/30 with daemon serving + `scripts/run-quality-gate.sh` exit 0 + pre-push green. Unit tests green with a failing hammer is NOT done.

## Six-Commit Table

| # | Commit subject | Files owned | Depends on |
|---|----------------|-------------|------------|
| 1 | feat(protocol): store wire types + owner-lock helpers + RoomStore router shell + warm-pool/engagement facade + rallyd crate skeleton [Chunk A] | Cargo.toml, rally-protocol/src/{store_wire.rs,lib.rs}, rally-cli/src/store.rs, rally-cli/src/rallyd_core.rs (ServeConfig + frozen serve() signature, stub body), **rally-cli/src/lib.rs (`pub mod rallyd_core;` line — GAP-1)**, crates/rallyd/** | — |
| 2 | feat(rallyd): daemon core — EX-owned single-writer serve loop, warm pool, socket lifecycle, engagement scoping [Chunk B] | rally-cli/src/rallyd_core.rs, crates/rallyd/** | C1 |
| 3 | feat(cli): routed store client + open() router + bounded-block corridor + `rally daemon` verbs [Chunk C] | rally-cli/src/store_client.rs, rally-cli/src/store.rs (Routed arms + router), **rally-cli/src/cli.rs (CliCommand::Daemon + parser + COMMANDS — GAP-2)**, rally-cli/src/lib.rs (dispatch arm + `mod store_client;`) | C1 |
| 4 | test(rallyd): handover invariant, fail-open, crash-release, engagement, mid-command-death tests [Chunk D1] | rally-cli/tests/rallyd_handover.rs | C2+C3 |
| 5 | test(rallyd): daemon-serving fixtures for #50 hammers + docker hammer script [Chunk D2] | tests/watchdog_concurrency.rs, tests/user_journey.rs, scripts/hammer-rallyd.sh | C4 |
| 6 | docs(rallyd): RALLY.md daemon section + manifest pointer + SCALE-ROADMAP P3 status | RALLY.md, .rally/manifest.json, docs/SCALE-ROADMAP.md | C5 |

MECE note: store.rs is edited by C1 (A) then C3 (C) — sequential, never parallel. lib.rs is edited by C1 (A: one `pub mod rallyd_core;` line) then C3 (C: dispatch arm + `mod store_client;`) — sequential, never parallel (A is serial batch 1; C runs in batch 2 strictly after A). During the batch-2 parallel window B and C overlap on ZERO files (B: rallyd_core.rs + crates/rallyd/**; C: store_client.rs + store.rs + cli.rs + lib.rs). cli.rs is owned solely by C.

**Envelope-contract advisory (scope-auditor, non-blocking):** `tests/json_envelope_contract.rs`, the `--help` sweep in `user_journey.rs:562`, and `cli_guardrails.rs:118` are hand-enumerated per-command lists that a new `daemon` verb SILENTLY SKIPS (they can't see `pub(crate) COMMANDS`). Since checklist Item 4 requires `rally daemon status --json` to be a discoverable, contract-honoring surface, Chunk C's `daemon status` handler MUST emit the standard JSON envelope, and Chunk D adds an `envelope_daemon_status` case to `tests/json_envelope_contract.rs` (that file is in Chunk D owned_files).

## Capability Gap Map

| Capability/Workflow | Current source of truth | Target behavior | Gap | Build action | Owned files/contracts | Validation |
|---|---|---|---|---|---|---|
| Concurrent facts.db access | store.rs:742 `open()` → :794 pool-per-process; measured 17–33% short-read + 1 corrupt under 8-way (issue #50) | Exactly one process (rallyd) opens facts.db while serving — with ONE warm pool inside it (R1) | N processes each open a factstr-sqlite pool at bootstrap; naive daemon would still churn a pool per request via the interior per-op re-opens | SH/EX owner lock + router before the constructor's `open_fact_store`; `warm_fact_store` handle for the hot interior sites | `.rally/rallyd.owner.lock` contract; store.rs router + `fact_store_handle()` | T-03; B warm-pool micro-test; F4 hammers 0/30 |
| CLI store ops | 214 `RoomStore::open` call sites, all direct | Same call sites, transparently routed when daemon live; byte-identical direct when not | No routing layer exists | RoomStore enum dispatcher + RoutedRoomStore | store.rs, store_client.rs, rally-protocol wire | T-04; existing suite green no-daemon |
| Run-id reservation under parallel launch | lib.rs:4324 CAS over :1464/:1329 (flaky under contention per #50) | Same CAS, both legs served by the daemon's total order; a mid-command daemon death fails fast + retryable (R6) | Legs hit per-process pools; no dead-socket policy existed | Wire ops for both legs; loop unchanged; A's dead-socket contract | rally-protocol store_wire | user_journey.rs:2023 hammer 0/30; T-09 |
| Daemon lifecycle operations | none (ptyd exists but is a different daemon for terminal sessions) | `rally daemon start\|stop\|status\|serve` + rallyd bin; `start` returns only after `.addr` + ping (R3) | No daemon exists | Chunks B + C verbs | crates/rallyd, cli.rs, lib.rs | T-03/T-06; `cargo build -p rallyd` |
| Fleet doc surface | RALLY.md/manifest without daemon mention | Operator can discover/start/diagnose the daemon | Docs gap | Commit 6 | RALLY.md, manifest.json | doc review in independent-auditor pass |

## Single-Shot Build Guardrails

| Guardrail | Prevents | Evidence/test |
|---|---|---|
| G1: No-daemon path byte-identical — router's Direct branch calls the UNMODIFIED `open_direct*` bodies; `warm_fact_store` is `None` in direct mode so `fact_store_handle()`'s cold branch reproduces today's per-op opens exactly; zero behavior edits inside them | Silent regression of today's semantics (F2) | Full suite green with no daemon, outcomes diffed vs main (T-04) |
| G2: Chokepoint integrity — every facts.db open happens inside a process that ALREADY holds an owner lock (SH or EX): the only two store-opening entry points are (a) the router's SH-guarded direct branch and (b) the daemon's EX-guarded startup; the interior per-op opens (store.rs :907/:1340/:1471/:1498 + cold recovery fns) inherit the caller's held lock | A store-opening entry point that bypasses the owner lock (dissolves the F3 proof) | Grep that PASSES by construction: `grep -rln "open_fact_store" crates/` hits only `store.rs` + `rallyd_core.rs`; `grep -n "open_direct" crates/rally-cli/src/` shows exactly the two guarded entry points — reviewed at C3 + auditor |
| G3: RoutedRoomStore holds NO fact_store and can't name one | Routed processes touching facts.db | Code review + `lsof` assertion in T-03 |
| G4: Owner lock acquired before any store open; mutation.lock only while owner held; never the reverse order | Lock inversion deadlock | Code review of the two acquire sites (rallyd_core startup; router direct branch) |
| G5: Charter purity — no decide/gate/schedule/spawn/execute, no LLM in rallyd_core/crates/rallyd | Charter violation (F1/F5) | `grep -rn "Command::new\|spawn\|anthropic\|openai" crates/rallyd crates/rally-cli/src/rallyd_core.rs` clean (fixture spawns live in tests, not daemon code) |
| G6: Do not re-attempt the 4 falsified factstr patch variants; do not touch factstr-sqlite internals | Re-litigating exhausted fixes | Cargo.toml pins unchanged; no factstr patch/fork in diff |
| G7: SH guard is process-global and never dropped early | Reopening the background-close race window | Code review: guard stored in `OnceLock`/static, no scoped drop |
| G8: Wire errors map onto existing RallyError variants with exit-code parity; transport-ONLY failure classes (timeout, reset, oversized line, wire_version/repo_root mismatch — R7) map to `RallyError::Command` (exit 1) with remedy text | Routed commands changing observable CLI behavior; transport failures with undefined exit codes | T-04 golden comparisons include error cases that HAVE a main equivalent; transport-only classes have no main equivalent so they are excluded from T-04 goldens and covered by dedicated unit assertions instead |
| G9: New `.rally` runtime files (sock/addr/pid/log/owner.lock) are untracked, mirroring mutation.lock's existing treatment | Committing runtime state | `git status` clean after daemon run (checked in T-03 teardown) |
| G10: ONE warm pool while serving — daemon mode installs `warm_fact_store`; `fact_store_handle()`'s open-fresh branch must never fire during serving | In-process pool churn re-creating the #50 pool-Drop race inside the daemon (R1) | B cold-branch-never-fires micro-test + T-07 hammers 0/30 |

## Read-Before-Edit Map

| Chunk/Work item | Read first | Why it matters | Edit after |
|---|---|---|---|
| A: router shell + lock helpers + warm-pool facade | store.rs:660–905 (mutation lock + open family + append incl. the :907 interior open), :1329–1520 (CAS legs + :1340/:1471 interior opens, snapshot :1498, cursors); `grep -n "RoomStore::open" crates/rally-cli/src/` (all 214); `grep -n "open_fact_store" crates/rally-cli/src/` (all 8 sites) | The rename must preserve every caller; the classification table must be exhaustive or the frozen contract strands a consumer; the warm-pool accessor must cover every HOT interior open or the daemon churns pools | store.rs |
| A: wire types | rally-protocol/src/lib.rs (Directive/Receipt conventions); store.rs:3729–3780 (RoomSnapshot serde evidence) | Wire enum must match crate conventions; payload serde must be verified not assumed | rally-protocol/src/store_wire.rs |
| B: serve loop | daemon_client.rs:1–100 + :639–700 (wire framing, timeout, fail-open posture); store.rs:766–810 (what open_direct does, incl. its internal mutation-lock use); rallyd_core.rs stub (ServeConfig — frozen, do not change) | Mirror the exact framing the client expects; understand the store's own locking before wrapping it; the serve signature is A's, not B's | rallyd_core.rs, crates/rallyd/** |
| C: router + client | ADR-01 choreography + corridor policy (this plan); daemon_client.rs:639 round_trip; lib.rs:1542 (command_say), :2221 (command_room), :3731–3830 (command_run) as representative call paths | The router must be provably before any facts.db open on every path; the corridor + dead-socket policies are frozen contract, not C's judgment | store_client.rs, store.rs, lib.rs |
| D: fixtures + hammer | watchdog_concurrency.rs:150–260; user_journey.rs:1980–2100; issue #50 hammer harness pattern (scripts/ + issue ledger) | Fixtures must not alter assertions; hammer must reuse the existing verdict pattern (no new verdict source) | tests/**, scripts/hammer-rallyd.sh |

## F-Criteria (functional — from .build-loop/goal.md, each with named falsifier)

| ID | Criterion | Pass condition | Named falsifier | Grader |
|----|-----------|---------------|-----------------|--------|
| F1 | Charter-pure single-writer daemon exists | `cargo build -p rallyd` exits 0; the daemon reuses ONE warm pool across the hot append/query/snapshot ops (`fact_store_handle()` warm arm — B micro-test proves the cold branch never fires while serving); direct mode retains per-op opens, byte-identical; one dispatcher thread; G5 grep clean | A second pool opened while serving (cold-branch micro-test firing, or any daemon-serving hammer failure); any spawn/schedule/LLM path in the grep | build + B micro-test + independent-auditor |
| F2 | Thin-client routing + fail-open | Routed when daemon live (reads and writes); full existing suite green with NO daemon, outcomes identical to main | Any no-daemon test outcome diff vs main; any routed op observed opening facts.db | T-04 + CI suite |
| F3 | No dual-writer window | T-03 passes; ADR-01 kernel argument holds under G2 chokepoint grep | A demonstrated interleaving with a direct facts.db open concurrent with a serving daemon | T-03 + code review of acquire/serve/release ordering |
| F4 | ACCEPTANCE GATE | BOTH #50 hammers 0/30 in docker rust:1.95 with rallyd serving + `run-quality-gate.sh` exit 0 + pre-push green | Any hammer failure in 30 rounds; unit-green-hammer-red is NOT done | captured hammer logs + gate exit code |
| F5 | Charter + invariants preserved | JSONL canonical (daemon appends segments then db, same as today); facts.db still disposable/rebuildable (T-06 crash + rebuild); WARN-not-block preserved; host-neutral; no new verdict source | Any daemon-only data unrecoverable from the ledger; any block-instead-of-WARN | independent-auditor scope=build |

## Q-Criteria (quality)

| Criterion | Pass condition | Grader |
|-----------|---------------|--------|
| Build | `cargo build --workspace` (incl. `-p rallyd`) exits 0 on 1.95.0 | CI |
| Tests | `cargo test -p rally-cli` green, no daemon required | CI |
| Quality gate | `scripts/run-quality-gate.sh` exit 0 | CI |
| Wire hygiene | `StoreRequest`/`StoreResponse` closed enums, `deny_unknown_fields`-equivalent strictness, `wire_version: 1` in ping | code review |
| No new heavy deps | rally-cli gains zero deps; rally-protocol stays serde-only; crates/rallyd deps = rally-cli only | Cargo.lock diff review |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| A store method's types don't serde round-trip, discovered after fan-out | Low (Fact + RoomSnapshot verified ✅; others are scalars/paths) | A verifies EVERY routed variant compiles with serde derives BEFORE the freeze ACK; this is A's falsifier, caught serially |
| Daemon churns a factstr pool per request via the interior per-op re-opens (:907/:1340/:1471/:1498), re-creating #50 in-process | High without R1 (8 open sites measured); Low with the warm-pool facade | L11/R1: daemon installs ONE warm pool; hot sites reuse `fact_store_handle()`; B cold-branch micro-test + T-07 hammers prove it (G10) |
| Cold `rally daemon start` reconcile (seconds on a real room) starves concurrent commit-hook CLIs | Low after R3 (was the corridor's failure mode) | Normative startup order binds socket + writes `.addr` BEFORE the store open; corridor clients bounded-block (3s/attempt, 30s bound) instead of fast-failing; `start` returns only after ping |
| Warm daemon serves stale in-memory state if `.rally/log/` mutates outside the daemon | Low (supported model: daemon is sole segment writer while serving — EX held, all clients route; segments gitignored so no git-driven mutation; manual mid-serve edits are operator error, documented UNSUPPORTED — R8) | B: byte-length+mtime staleness gate before reads (strictly stronger than mtime alone for append-only segments); replay idempotence (store.rs:764) makes the worst case a redundant replay; [ASSUMED] stat cost negligible at N≤16, micro-benchmarked in B |
| Error/exit-code parity drift between routed and direct paths | Medium | Wire errors map to existing RallyError variants (A contract); transport-only classes pinned to `RallyError::Command` exit 1 (R7); T-04 goldens include error cases (G8) |
| Daemon EX starvation under continuous SH stream | Low at N≤16 | Accepted + logged ("waiting for direct writers"); documented ceiling (L10) — does not preclude a later fair-queue |
| Stale `.addr` after crash pointing at a reused tmp path | Low | Ping verifies repo_root + liveness; SH lock is the authority, probe is only a router hint |
| Hammer flakes due to fixture races (daemon not ready) | Low after R3 | `.addr` is written before reconcile, and the fixture blocks on a successful ping — which completes only once the dispatcher is ready; hammer logs capture per-round daemon logs for diagnosis |
| macOS sun_path fallback path itself over-long or $TMPDIR unset | Low | Hash keeps fallback name short + fixed; `$TMPDIR` empty → `/tmp`; unit test in B for both branches |
| factstr background close outliving a direct client's SH release | Eliminated by design | Process-lifetime SH: kernel releases only at process death, after any background threads are gone (ADR-01, G7) |

## ADRs

### ADR-01 — SH/EX ownership flock handover (`.rally/rallyd.owner.lock`)
**Decision (lock primitive):** daemon holds LOCK_EX for lifetime; every direct open holds LOCK_SH for process lifetime; router probes socket first, SH-try second, bounded-block corridor third. **Alternatives:** client-side exclusive try-lock (breaks no-daemon concurrency — two direct writers can't coexist); pid files (racy, lie after crashes); reuse mutation.lock (per-append acquire/release semantics + self-deadlock at daemon's own open); gate writes only (reader pool Drops re-open the #50 race). **Rollback:** delete the owner-lock acquisition from the router's direct branch and the daemon refuses to start — the system degrades to exactly today's multi-process behavior; the lock file is inert data. **Tradeoff:** flock unfairness can starve daemon startup under continuous direct traffic (accepted, logged, N≤16).

**Corridor policy (distinct from the lock primitive — R3/R6, L12):** SH refused ⇒ the client CONNECTS with a bounded-block timeout (`DAEMON_TIMEOUT`=3s per attempt, extendable) and retries the connect+ping corridor up to a generous **30s corridor bound** before failing loud with remedy text (`rally daemon status`/`stop`). Rationale: the normative startup order binds the socket + writes `.addr` immediately, but the store open/reconcile behind it can take seconds on a real room — a fast fail-loud corridor (the prior draft's 3×250ms/750ms budget) was falsified because it would fail every concurrent commit-hook CLI on every cold `rally daemon start`; the bounded-block corridor lets those clients block on their in-flight request until the dispatcher is ready. Failing loud only past 30s means a truly wedged daemon, which IS operator-actionable. **Mid-flight daemon death (R6):** a Routed op on a dead socket fails fast with a retryable error ("daemon stopped mid-request; retry", exit 1) and NEVER opens facts.db directly mid-command — a silent direct fallback would skip the SH choreography and void the G2 premise; the caller re-runs the command, which re-enters the router (no daemon → direct; new daemon → route). Gated by T-08/T-09.

### ADR-02 — Wire protocol home + shape
**Decision:** typed `StoreRequest`/`StoreResponse` in `rally-protocol` (existing zero-dep serde crate whose charter is exactly "the shared rally↔daemon coupling surface"); line-delimited JSON-RPC framing mirroring daemon_client.rs:639; per-request engagement; `wire_version: 1` in ping; `.addr` file as sole discovery; transport-only failures map to `RallyError::Command` exit 1 (R7). **Alternatives:** new `rallyd-wire` crate (crate sprawl, no benefit); types in rally-cli (circular dep — rallyd needs them too... resolved anyway by ADR-03, but protocol types belong in the protocol crate); untyped serde_json::Value dispatch (loses closed-enum validation). **Rollback:** wire_version field allows a v2 without breaking v1 clients (unknown version → client falls back as not-live → lock decides).

### ADR-03 — Crate structure: daemon core inside rally-cli, thin `crates/rallyd` bin
**Decision:** serve loop lives in `rally-cli/src/rallyd_core.rs` (full `pub(crate)` access to RoomStore — no store internals go pub); `crates/rallyd` is a ~30-line bin calling `rally_cli::rallyd_core::serve(ServeConfig)` — the `ServeConfig` struct and `serve` signature are FROZEN in Chunk A (R5) so B's body work and C's call site never race on the signature during the parallel window; `rally daemon serve` exposes the same entry (this is what fixtures spawn via `CARGO_BIN_EXE_rally`, sidestepping cross-crate CARGO_BIN_EXE limits). **Alternatives:** extract a `rally-store` crate (correct long-term, but a large mechanical refactor of 214 call sites' crate paths mid-correctness-fix — churn risk dominates); daemon only as a subcommand with no rallyd crate (fails F1's explicit crate requirement); pub-facade re-exporting store internals to a standalone rallyd (widens the public surface for no consumer but ourselves). **Rollback:** rallyd_core is a leaf module; deleting it + the thin crate restores main. Auto-spawn note (not precluded): a future opt-in auto-spawn = `rally` taking a single-spawn flock (`.rally/rallyd.spawn.lock`, EX|NB — one process wins and spawns `rally daemon serve`), guarded behind an explicit env opt-in; charter-acceptable as host-side store infra.

### ADR-04 — Auth = 0600 socket perms
**Decision:** OS-enforced same-user via 0600 on socket/addr/pid/log. **Alternatives:** getpeereid/SO_PEERCRED (platform variance, zero added coverage same-user; first addition if threat model widens); cockpitd authz/crypto (wrong transport + threat model). **Rollback:** additive — peer-cred check can be inserted at accept() later without wire changes.

### ADR-05 — std-only daemon (no tokio in rally-cli)
**Decision:** std `UnixListener`, per-conn threads, mpsc single dispatcher. **Alternatives:** tokio UnixListener (the brief's sketch — but pulls tokio into rally-cli, inflating the `rally` binary that runs on every commit hook, per Cargo.toml's own release-profile rationale; async buys nothing at N≤16 with a totally-ordered dispatcher); shared-nothing thread-per-conn each owning a pool (recreates multi-pool #50 in-process — the same defect R1's warm pool prevents on the dispatcher path). **Rollback:** the dispatcher boundary (mpsc of `StoreRequest`) is runtime-agnostic; a tokio front-end can replace the accept loop later without touching dispatch or wire.

## Approach lenses

- **Clean-sheet:** a long-lived daemon owns the ONLY live sqlx pool (one warm pool, reused across ops — not one per request); clients speak typed JSON-RPC over a per-repo Unix socket; direct mode exists only as a fail-open fallback; single dispatcher = free total order.
- **Current-constraints:** land inside the existing workspace (edition 2024, toolchain 1.95.0); reuse the daemon_client wire shape, the hand-declared flock pattern, the existing hammer harness and quality gate; 214 call sites force the router INTO `RoomStore::open` rather than a parallel client API; 8 interior `open_fact_store` sites force the warm-pool facade rather than a naive "hold one RoomStore" reading.
- **Bridge/backcast:** ship rallyd strictly additively behind the SH/EX handover — no daemon ⇒ kernel-provably today's path, byte-identical (its known flake included); daemon present ⇒ #50 structurally impossible. Every increment keeps the suite green; the acceptance hammer flips from red-ish (17–33%) to 0/30 only at commit 5, and that flip is the whole point.

## Spec Object (JSON)

```json
{
  "needs": [
    {"id": "U-01", "priority": "P0", "statement": "Agent fleet + operator run many concurrent rally CLIs on one repo without facts.db corruption or dropped/duplicated facts (dissolve issue #50 structurally)", "features": ["F-01", "F-02", "F-03", "F-04", "F-05"], "tests": ["T-01", "T-02", "T-03", "T-04", "T-07"]}
  ],
  "features": [
    {"id": "F-01", "priority": "P0", "title": "rallyd charter-pure single-writer daemon (crates/rallyd + rallyd_core) with ONE warm facts.db pool while serving", "chunk": "B", "adrs": ["A-03", "A-05"], "data": ["D-01"], "tests": ["T-01", "T-05"]},
    {"id": "F-02", "priority": "P0", "title": "Thin-client routing with fail-open no-daemon fallback, reads and writes; bounded-block corridor + dead-socket fail-fast", "chunk": "C", "adrs": ["A-01", "A-02"], "data": ["D-01", "D-02"], "tests": ["T-02", "T-04", "T-08", "T-09"]},
    {"id": "F-03", "priority": "P0", "title": "SH/EX flock handover — no dual-writer window while a daemon serves", "chunk": "A+C", "adrs": ["A-01"], "data": ["D-02"], "tests": ["T-03", "T-06"]},
    {"id": "F-04", "priority": "P0", "title": "Acceptance gate: both #50 hammers 0/30 with daemon serving", "chunk": "D", "adrs": [], "data": [], "tests": ["T-07"]},
    {"id": "F-05", "priority": "P0", "title": "Charter + invariants preserved (JSONL canonical, db disposable, WARN-not-block, host-neutral)", "chunk": "all", "adrs": ["A-01", "A-03"], "data": ["D-01"], "tests": ["T-05", "T-06"]}
  ],
  "data": [
    {"id": "D-01", "contract": "facts.db remains a disposable derived cache of the canonical .rally/log/ JSONL ledger; daemon appends segment-then-db exactly as the direct path does today, through ONE warm pool while serving", "tests": ["T-06"]},
    {"id": "D-02", "contract": ".rally/rallyd.owner.lock — daemon holds flock LOCK_EX for lifetime; every direct facts.db opener holds LOCK_SH for process lifetime; .rally/rallyd.sock.addr is the sole discovery pointer, written before the daemon's store open", "tests": ["T-03", "T-04"]}
  ],
  "tests": [
    {"id": "T-01", "check": "cargo build -p rallyd exits 0; code review + B micro-test: one warm pool (fact_store_handle cold branch never fires while serving), one dispatcher; charter grep clean", "grader": "CI + auditor"},
    {"id": "T-02", "check": "full existing suite green with no daemon, outcomes identical to main", "grader": "CI"},
    {"id": "T-03", "check": "handover invariant: daemon EX blocks direct SH (and vice versa); routed client holds no facts.db fd (lsof)", "grader": "tests/rallyd_handover.rs"},
    {"id": "T-04", "check": "fail-open: no daemon / stale socket / stale addr all yield today's direct behavior; goldens include error cases with a main equivalent (transport-only classes unit-asserted separately)", "grader": "tests/rallyd_handover.rs"},
    {"id": "T-05", "check": "engagement scoping: append routed with RALLY_ENGAGEMENT=X lands in segment X (dispatcher applies set_engagement_scope per request)", "grader": "tests/rallyd_handover.rs"},
    {"id": "T-06", "check": "SIGKILL the daemon: kernel releases EX; next client fails open; facts.db rebuilds from ledger (disposability)", "grader": "tests/rallyd_handover.rs"},
    {"id": "T-07", "check": "docker rust:1.95, 30 rounds each: watchdog_concurrency.rs:185 and user_journey.rs:2023 with daemon-serving fixtures — 0/30 failures each; quality gate exit 0", "grader": "scripts/hammer-rallyd.sh logs"},
    {"id": "T-08", "check": "wedged-daemon corridor: SH refused AND no successful ping within the corridor bound fails LOUD with remedy text, never writes directly", "grader": "tests/rallyd_handover.rs"},
    {"id": "T-09", "check": "mid-command daemon death: kill daemon between two routed ops of one command; second op fails fast with retryable 'daemon stopped mid-request; retry' (exit 1); lsof shows the process never opened facts.db", "grader": "tests/rallyd_handover.rs"}
  ],
  "adrs": [
    {"id": "A-01", "title": "SH/EX ownership flock handover + bounded-block corridor policy"},
    {"id": "A-02", "title": "Wire protocol home + shape (rally-protocol, line-delimited JSON-RPC, .addr discovery, transport-error mapping)"},
    {"id": "A-03", "title": "Crate structure: core in rally-cli, thin rallyd bin, serve(ServeConfig) frozen in A; explicit lifecycle, no auto-spawn"},
    {"id": "A-04", "title": "Auth = 0600 socket perms"},
    {"id": "A-05", "title": "std-only daemon runtime"}
  ]
}
```

## Open Questions

None. All forks are resolved with evidence above; the two residual unknowns are labelled assumptions with in-chunk verification (segment-refresh len+mtime stat cost → B micro-benchmark; serde round-trip of every routed variant → A compile-time verification before the freeze ACK) — neither passes the blocking-and-novel test as a question.

## Out of Scope

Mirror of §Scope: opinionated coordinator; any decide/gate/schedule/spawn/execute or LLM feature; auto-spawn; the 4 falsified factstr variants; new concurrency verdict sources; cockpitd module extraction; N=1000 thundering ceiling; the no-daemon flake (persists by design in fail-open mode); external non-daemon mutation of `.rally/log/` while the daemon serves (unsupported operator error — R8).

---

*Certainty markers: reuse anchors, serde evidence, call-site counts, interior open-site lines, crates.io currency (0.5.2, 2026-07-11) — ✅ verified by code read/grep/registry check 2026-07-11. Segment-refresh stat cost, per-variant serde derives — ⚠️ [ASSUMED] with named in-chunk verification. Plan verified downstream by plan-critic (blocking at these stakes) + scope-auditor (`modifies_api: true`); this document does not self-certify. Re-plan provenance: rev 2 closes plan-critic F1–F11 + scope-auditor GAP-1/GAP-2 via R1–R10; the falsified 3×250ms corridor draft is preserved in ADR-01/F-handover as failure evidence.*
