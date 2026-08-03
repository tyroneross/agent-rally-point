<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Efficiency assessment — 2026-08-03

Measured on a 16-core macOS host against an isolated copy of this repo's real
5.3 MB ledger. Medians of 5–7 runs. Where a number is an estimate rather than a
measurement, it says so.

Companion architecture findings: RC-026 (charter contradiction) and RC-027 (the
watcher tails a dead channel) in [`ROOT-CAUSE-REGISTER.md`](ROOT-CAUSE-REGISTER.md).

## What an agent pays per turn

This is the headline, because every agent in every session pays it.

| Turn shape | Cost |
|---|---|
| 0 edits | 0.92 s (UserPromptSubmit 440 ms + Stop 475 ms) |
| 3 edits | 2.3 s |
| 10 edits | 5.6 s |
| Session start, once | 0.95 s |

The `start` phase spawns **6 `rally` processes and 4 `node` processes** —
951 ms median. Attribution: `enter` 239 ms · `status post` 136 ms · `room`
104 ms · `next --audit` 97 ms · `status read` 75 ms · 4× node 100 ms ·
`hooks status` 11 ms.

**The cost is per-invocation, not per-byte.** `rally hooks status` (11 ms) and
`rally check before-write` (13 ms) never open the ledger. Every *projection*
command pays 75–240 ms because it scans the whole log. So the win is fewer
invocations, not smaller output.

## Findings

| ID | Area | Sev | Evidence | Fix | Effort |
|---|---|---|---|---|---|
| P1 | Hook latency | HIGH | `start` = 6 rally + 4 node = 951 ms | Collapse `room`+`next`+`status read` into one call | M |
| P2 | Ledger payload | HIGH | `rally room --json` returns **1,956,274 bytes**; `stale_facts` is **1,292,805 of them (66%)** and the hook greps `stale_facts` **zero** times | `rally room --lean` | M |
| P3 | Ledger scan | HIGH | Projection is O(whole ledger): empty→5.3 MB moves `room` 16→104 ms. Curve fits **15.2 µs/KB + 15 ms**. `--since` cuts payload 16× and latency **not at all** — the filter runs after the scan | Push the filter into SQL; index on seq | L |
| P4 | Ledger growth | MED | **51 open handoffs, 1374 stale facts, 154 squads, 67 active claims** (RC-008 recorded 42/1234 — up 21%/11%) | Retention policy | M |
| P5 | Test gate | HIGH | `cargo-nextest` **not installed**, so the gate runs 843 tests fully serialized. Measured penalty on `user_journey`: **4.4 s → 10.9 s (2.5×)** | `cargo install cargo-nextest --locked` | S |
| P6 | Test gate | HIGH | **No `CARGO_TARGET_DIR` anywhere** in `.githooks/`, `run-quality-gate.sh`, or `.cargo/config.toml`. Every push compiles the workspace from scratch in a fresh worktree | Shared target dir for the gate | S |
| P7 | Test runtime | HIGH | `hook_wrapper_contract`: **1 test, 36.9 s** — the slowest in the repo. `RALLY_TEST_BLOCK_MS=3600` × 10 hook calls ≈ 36 s, matching exactly. Its runtime is a direct function of P1's subprocess count | Falls out of P1; or drop the block | S |
| P8 | Test runtime | MED | `rallyd_handover`: 8 tests, 30.1 s — polling deadlines of 25/30/20 s. With P7 these two suites are **78%** of the integration set | Injectable clock | M |
| P10 | Disk | MED | `target/` = **28 GB** (debug 21 G, release 3.8 G, `target/assessment-main` 2.6 G); up to 13 stale copies of each test binary | `cargo clean` | S |
| P11 | Disk | MED | 3 × `.rally.bak-*` = **468 MB**; 35 × `.rally/facts.db.corrupt.*` = **84 MB** (85% of `.rally/`). All untracked and gitignored — zero clone impact | Operator decision (see below) | S |
| P12 | Disk | MED | **3 stale pre-push worktrees = 2.1 GB** | ✅ **Removed this run** | S |
| P13 | Clone weight | MED | **18 tracked git bundles** under `archive/bundles` (94 MB working tree, ~13 MB packed) + 5 near-identical app icons (7.2 MB packed) ≈ **31% of the 65 MB `.git`**. Every clone pays it | Move bundles to release assets | M |
| P14 | Duplication | HIGH | Two sanitizer blocks byte-identical with **nothing enforcing it** | ✅ **Fixed this run** — `tests/hooks/test_sanitizer_block_parity.sh`, mutation-validated twice | S |
| P15 | Module size | MED | **`crates/rally-cli/src/lib.rs` = 13,568 lines** — 61% larger than `store.rs` (8,427). The register flags `store.rs`; `lib.rs` is bigger and was unflagged | Split `lib.rs` first | L |
| P16 | DX | MED | `rally --version` fails with *"unknown Rally command --version"*. `rally version` works. The error names neither the right form nor `--help` | Accept `--version`/`-V`; suggest `--help` | S |
| P19 | Hook waste | MED | `before-write` spawns **6 node processes**; three parse the *same* stdin envelope — one emits `{path,session}`, two more each re-parse it for one field. ~50 ms of pure startup wasted | One node call emits both | S |
| P20 | Hook | LOW | Claude registers `Stop` → `after-write` (475 ms). The cheap `stop` branch (205 ms) is **unreachable** in that registration | Route or delete | S |
| P21 | Hook tail | LOW | One `idle` run took **10,088 ms** against a 440 ms median — consistent with two `rally` calls hitting the 5 s timeout budget. **Did not reproduce in 20 follow-ups.** Under Claude's 5 s SessionStart budget this is a hook kill, not just slowness | Investigate lock contention | M |
| P22 | Dead code | LOW | `cargo-machete` and `cargo-udeps` are **both absent**. A grep heuristic produced false positives and was discarded. Unused-dep status is **unknown, not clean** | Install a real tool first | S |

Healthy, no action: **cockpitd** (bounded channels, 5 s sweep, 50 ms coalesce, terminal cleanup verified) and the **watcher's** runtime (1 Hz idle wake; the new quarantine path adds zero happy-path cost). The watcher's problem is RC-027, not its efficiency.

## Landed this run

- **P14** — the sanitizer-parity test, mutation-validated twice. The second mutation caught a hole in the test itself: `grep "function prose"` also matched `function proseX`, so a rename in *both* blocks kept hashes equal and every assertion green. Now matches `function prose(`.
- **P12** — three stale pre-push worktrees removed, 2.1 GB reclaimed, including the July `3a17fe8` that RC-009 already recorded.
- **P4** — current numbers recorded into RC-008.

## Queued, with effort

**Do first — highest impact per unit of effort:**

1. **P5 + P6 (S, S)** — one `cargo install cargo-nextest --locked` plus one shared `CARGO_TARGET_DIR`. These are almost certainly the whole ">10 minute push" complaint. P6 must re-verify that the SEC-005 gate-script pinning still holds with a shared target dir.
2. **P16 + P19 (S, S)** — the first error a new user hits, and 50 ms off every edit.
3. **P1 + P2 (M, M)** — collapse the start-phase calls and trim the payload. Shrinks P7's 36.9 s test as a side effect. **Land P14 before touching the hook** (done), and re-run the ARP-004 sanitization tests after, since these touch the renderer path.
4. **P10 (S)** — `cargo clean` reclaims ~28 GB. Costs one full rebuild.
5. **P3, P8, P13, P15 (L, M, M, L)** — real work, own builds.

**Operator decision, not an agent's:** P11. The `.rally.bak-*` directories are **backups of a coordination ledger**, not reproducible build output. They are untracked and cost nothing to clone. Deleting 468 MB of someone's ledger history is not a call an agent should make unilaterally, so they stay until you say otherwise. Same for the 35 quarantined corrupt DBs, which are diagnostic evidence for RC-005/RC-010.

## Pre-public hygiene note

`git grep` over tracked files finds **`/Users/tyroneross` absolute paths in 22 files**, concentrated in `.rally/RETROSPECTIVE.md` and committed ledger segments. Committed coordination history is canonical by design, so this is not a bug — but it does publish local machine paths to anyone who clones. Worth a decision before any wider distribution, alongside P13's tracked bundles, which `references/pre-public-hygiene.md` flags as leaking pre-sanitization history wholesale.
