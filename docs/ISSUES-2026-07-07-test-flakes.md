<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Test-Suite Flakiness under `cargo test --workspace` — 2026-07-07

**Status: shared-state flakes FIXED; resource-contention flakes remain.**

## Resolution (2026-07-07)

The two **deterministic shared-state** root causes are fixed and validated (0
failures across 15 `--workspace` runs each):

- **Signature A cascade** — the `status_post_*` tests did process-global
  `set_current_dir` with a restore skipped on panic + a non-poison-tolerant
  lock, so one failure left a deleted CWD + poisoned lock and dragged the whole
  cluster (incl. `busy_but_quiet`) down. **Fix:** a panic-safe RAII `CwdEnvGuard`
  (poison-tolerant lock + `Drop`-restores-CWD). Also unified `hooks_config`'s
  private `env_lock()` into the crate-wide `PROCESS_ENV_LOCK` and added a
  `ConfigEnvGuard` that removes coordination env vars on panic (real
  `set_var`-unsoundness hardening).
- **`busy_but_quiet` boundary bug** — the captured panic
  (`lib.rs "takeover must be refused"`) traced to a **boundary value**, not an
  env/CWD race: the test stamped a *single-file* claim at **exactly 30 min**,
  which equals `DEFAULT_RECLAIM_SMALL_MINUTES`. `command_release_by_path` checks
  `age > reclaim_timeout`; with second-truncated timestamps + slow execution
  under load, `age` lands at 1801 vs 1800 ~7% of runs → reclaimable → takeover
  succeeds. **Fix:** stamp at **22 min** (past 15m idle, safely under the 30m
  single-file reclaim bar) → deterministic.

**Remaining (resource contention, NOT shared-state):**
`parallel_say_invocations_never_drop_or_duplicate_facts`,
`rally_run_reserves_numbered_ids_under_parallel_launch`, `envelope_owners_dirty`.
These spawn many concurrent subprocesses/threads; under full-suite CPU
saturation a subprocess is starved and times out. They live in **separate test
binaries** (separate processes), so a Rust mutex cannot coordinate them — the
real fix is structural: **cargo-nextest** (per-test resource groups + better
scheduling) or a reduced global `--test-threads`. Observed post-fix rate ~1/15,
down from ~3/10 with 5-test clusters.

## Original report (pre-fix)

**Was: CONFIRMED on `main` (09a4482).** The `v0.1.6` `PROCESS_ENV_LOCK` fix
reduced but did NOT eliminate it.

## Evidence

`cargo test --workspace` (the exact command `.githooks/pre-push` runs) looped
**10×** on `main` @ 09a4482: **3 of 10 runs failed (~30%)**, across **three
distinct flake signatures**. The failures come in **streaks**, not uniformly —
runs 1, 3, 4 failed; runs 5–10 were all green — so a small sample badly
over- or under-estimates the rate (an earlier 4-run sample read ~75%). (A 4th
signature, `rally_run_reserves_numbered_ids_under_parallel_launch`, blocked the
v0.1.6 tag push on a prior day.) Each run is non-deterministic — same binary,
same commit, different result.

| Signature | Tests | Seen |
|---|---|---|
| **A — process-global CWD race (highest freq)** | `busy_but_quiet_owner_is_warnable_but_not_takeover_eligible` + `status_post_done_autofills_git_metadata_for_codex_and_claude_code` + `status_post_done_explicit_marker_overrides_git_for_missing_pair_only` + `status_post_done_explicit_metadata_does_not_require_git_autofill` + `status_post_then_status_read_roundtrip` — fail **together** | runs 1, 3 |
| **B — owners/git-state** | `envelope_owners_dirty` (json_envelope_contract) | run 4 |
| **C — CPU over-subscription** | `rally_run_reserves_numbered_ids_under_parallel_launch` (user_journey) | v0.1.6 tag-push gate |

## Root cause

Cargo runs tests multi-threaded within each test binary. Several tests mutate
**process-global** state without full coordination:

- **Signature A (CWD-on-panic cascade).** The `status_post_*` tests
  (`crates/rally-cli/src/lib.rs:10690–10846`) do
  `std::env::set_current_dir(&root)` … work … `set_current_dir(prev_cwd)`. They
  hold `PROCESS_ENV_LOCK`, so they serialize against each other and against
  `busy_but_quiet` (which now also holds it after v0.1.6). BUT: if any one of
  them fails its assertion **between** the two `set_current_dir` calls, it (a)
  leaves the process CWD pointing at a temp dir that gets deleted, and (b)
  **poisons** `PROCESS_ENV_LOCK`. The other lock-holders use
  `.unwrap_or_else(|p| p.into_inner())` (poison-tolerant) and then run with a
  dangling CWD → the whole cluster fails together. So the cluster is one root
  failure cascading, not five independent races.
- **`PROCESS_ENV_LOCK` scope gap.** It is `pub(crate)` — reachable ONLY from
  unit tests. Integration-test binaries (`tests/*.rs`) run as separate processes
  and cannot acquire it. `tests/worktree_gc.rs:725` does its own
  `set_current_dir` with no cross-binary coordination (harmless across
  processes, but the pattern is unguarded within its own binary).
- **Signature C (resource).** `rally_run_reserves_numbered_ids_under_parallel_launch`
  spawns `available_parallelism() * 4` (8–24) `rally run` subprocesses; its own
  comment warns of "spurious watchdog timeouts" under over-subscription. When it
  runs alongside the rest of the CPU-saturating suite, a subprocess times out and
  `output.status.success()` fails. Correctness (distinct-id reservation) is fine;
  the failure is a starved subprocess.

## Impact

- **Releases require gate retries.** v0.1.6 needed multiple push/tag attempts
  because each `git push` re-runs `cargo test --workspace` and hits a different
  flake. This is operational drag and erodes trust in the gate.
- Any CI that runs the full suite has a ~30%-per-run chance of a spurious red
  (streaky — several clean runs can precede a cluster failure).

## Fix directions (for the dedicated fix, not done here)

1. **Kill process-CWD mutation in tests.** Replace `std::env::set_current_dir`
   with explicit `Command::current_dir(...)` per invocation, or pass the working
   dir into the code under test. This removes the entire Signature A/B class.
2. **If CWD mutation must stay:** guard it so a panic cannot leave a dangling CWD
   — a RAII restorer (`Drop` restores `prev_cwd`) so an assertion failure still
   resets CWD; and treat the lock as covering CWD, not just env.
3. **Signature C:** gate the parallel-launch test behind a global heavy-test
   semaphore, drop N, raise the under-test `rally run` watchdog, or serialize it
   against the whole suite.
4. **Structural:** adopt `cargo nextest` (process-per-test isolation) — it
   eliminates the shared-process CWD/env class entirely and is the highest-leverage
   single change.

## Note on the v0.1.6 partial fix

`busy_but_quiet` / `liveness_enforce` holding `PROCESS_ENV_LOCK` (commit in
v0.1.6) was correct-in-direction and made them stable under `cargo test --lib`
(validated 5×). It is **insufficient** under `--workspace` because the poisoning
cascade and the integration-binary races are outside that lock's reach. The
partial fix was validated against the wrong (lighter) command — a reminder to
validate flake fixes with the exact gate command.
