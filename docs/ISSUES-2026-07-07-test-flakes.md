<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Test-Suite Flakiness under `cargo test --workspace` — 2026-07-07

**Status: CONFIRMED, still an issue on `main` (09a4482).** The `v0.1.6`
`PROCESS_ENV_LOCK` fix reduced but did NOT eliminate it.

## Evidence

`cargo test --workspace` (the exact command `.githooks/pre-push` runs) looped on
`main` @ 09a4482: **3 of 4 runs failed (~75%)**, across **three distinct flake
signatures**. (A 4th, `rally_run_reserves_numbered_ids_under_parallel_launch`,
blocked the v0.1.6 tag push on a prior day.) Each run is non-deterministic —
same binary, same commit, different result.

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
- Any CI that runs the full suite has a ~75%-per-run chance of a spurious red.

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
