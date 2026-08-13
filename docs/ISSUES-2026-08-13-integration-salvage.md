<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Salvage from branches dropped during the 2026-08-13 integration

Filed before deletion, per the standing rule that a dropped branch's surviving
ideas get an entry rather than a tombstone. Every branch named here is preserved
at `refs/archive/integration-20260813T073628Z/branch-*` and in
`archive/bundles/pre-integration-20260813T073628Z.bundle`; nothing below requires
recovering a ref to act on.

---

## S1 — Auto-reap should size its budget from the caller's watchdog, not a flat constant

**Source:** `oc/a598bb4be22c9041ea4a86c694a4fc35` (`6eadf9a`), dropped as superseded.
**Preserved at:** `refs/archive/integration-20260813T073628Z/branch-oc-a598bb4be22c9041ea4a86c694a4fc35`
and `refs/archive/build-loop-997022/pre-stabilization-20260810T082000Z/branch-auto-reap-donor-only`.

**Why the branch was dropped.** The stabilization work landed a stronger fix for
the same hazard. `reaper.rs` on `main` now takes a non-blocking kernel advisory
lock on `.auto-reap-in-flight` (`reaper.rs:252`) rather than the dropped branch's
reuse of the room mutation lock, and caps a pass at `AUTO_REAP_MAX_FACTS = 200`
(`reaper.rs:393`). The dropped branch would additionally have flipped
`DEFAULT_AUTO_REAP_INTERVAL_SECS` from `0` to `900`, against `main`'s documented
decision (`reaper.rs:248`, and RC-051) to keep auto-reap opt-in.

**What did not survive, and should.** The dropped branch carried a budget model
that `main` has no equivalent of. `main`'s reaper budgets against
`DEFAULT_REAP_APPLY_BUDGET_MS`, a flat constant chosen for the human-invoked
`doctor` pass; it knows nothing about the deadline the *calling* command is
judged on. That mismatch is what produced the 8/8 `exit 4` the first time
auto-reap shipped: the reap spent the caller's whole remaining watchdog and
`enter` missed its own presence append.

Three pieces are worth lifting:

1. **`auto_reap_budget()`** — divide `crate::watchdog_remaining()` (the same
   deadline the watchdog will enforce) rather than applying a flat constant, so
   raising `--timeout-ms` raises the reap budget with it.
2. **`AUTO_REAP_WATCHDOG_RESERVE_MS = 1_500`** — the slice the errand may not
   touch, reserved for the caller's own durable append. Return "skip this pass"
   whenever less than the reserve remains, so a slow or contended command spends
   its last milliseconds on the write the caller actually asked for.
3. **`AUTO_REAP_CATCHUP_SECS = 60` plus a backdated marker** — an incomplete pass
   backdates `.rally/.last-auto-reap` so the drain resumes in a minute instead of
   waiting a full interval. Convergence comes from the interval, not the per-pass
   cap; raising `AUTO_REAP_MAX_FACTS` changes nothing, because the wall-clock
   budget binds first on any ledger large enough to have a backlog.

**Not recommended:** the `DEFAULT_AUTO_REAP_INTERVAL_SECS = 0 → 900` flip. That
is an operator decision, `main` has already decided it, and nothing here reopens
it. Items 1–3 are independent of it and improve the human-invoked `doctor` path
too.

---

## S2 — RC-070 (memo-isolation flake) has no entry in the register on `main`

**Source:** the same dropped branch, which filed RC-070 in its own
`docs/ROOT-CAUSE-REGISTER.md`. That entry is not present on `main`, so dropping
the branch would have taken the only written record of a flake that is live here.

**Reproduced during this integration, on an unmodified `main` (`ea27ded`):**
`store::ledger_tests::segment_fold_memo_explicit_invalidation_handles_same_length_rewrite`
failed once in the first full `cargo test --all` run, panicking at
`store.rs:7709` (`"adversarial fingerprint collision must hit the cached fold"`).
It then passed 3/3 in isolation and 2/2 on full-suite reruns — the isolation-passes
signature. Pre-existing, and not attributable to anything landed in this
integration.

**Mechanism (hypothesis, not yet pinned to a competing writer):**
`SEGMENT_FOLD_MEMO` is a process global. The test calls
`invalidate_segment_fold_memo()` and then requires the memo to hold *its* fold.
`PROCESS_ENV_LOCK` does not protect that — it serializes env-var mutation, not
memo ownership — so any concurrently-running test whose read path populates or
clears the memo can slot in between the invalidate and the assertion.
`--test-threads=1` is the cheap discriminator; identifying the competing test is
the first step.

**Why it matters beyond the flake:** the memo is a correctness cache on the
canonical ledger read. A test that cannot state when it owns that cache is weak
evidence for the invalidation property it was written to grade.

**Ask:** restore an RC-070 entry to `docs/ROOT-CAUSE-REGISTER.md` on `main`,
carrying the evidence above.

---

## S3 — Retraction resolution now lives in the projection core; the cache path has no direct test

**Source:** this integration, not a dropped branch. Filed here so it is not lost.

`feat/fact-retraction` authored its retraction filter at the top of
`snapshot_from_facts_with_policy`. Stabilization has since reduced that function
to a thin wrapper over `snapshot_from_facts_with_policy_at`, and the
snapshot-cache capture path calls the core directly (`store.rs:4578`). The filter
was therefore placed in the **core**, not the wrapper — filtering in the wrapper
alone would let a cached snapshot keep showing facts that a freshly-computed one
had already dropped.

The 13 retraction tests all exercise the wrapper. **No test covers the direct
`_at` caller**, so the placement is correct by construction but not by evidence.
A test that captures a snapshot into the cache, retracts the target, and asserts
the cached read also drops it would close that gap cheaply.
