<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Plan — observed liveness, durable renewal, and the remaining budget defects

## Governing thought

Rally cannot tell a working agent from a wedged one, because all four liveness signals are
self-reported (`liveness.rs:51-68`). Renewal does not reach the reaper, so live agents lose
claims (`store.rs:1286` writes a sidecar `reaper.rs:285` never reads); nothing can prove an
agent is dead, so dead agents hold claims forever (63 claims lease-expired since 2026-08-03,
per `rally doctor --reap-stale` dry-run). Both errors have the same fix shape: **make the
evidence observed rather than asserted**, then let the reaper act on it.

The user-visible outcome is one sentence: *an agent stops losing its files mid-task, and a
crashed agent stops holding files hostage.*

## Measured baseline (2026-08-05, HEAD 542c884)

| Fact | Value | Source |
|---|---:|---|
| Active claims in this room | 63 | `rally room --json` |
| Of those, lease-expired since 2026-08-03 | 63 | `rally doctor --reap-stale` dry-run |
| `branch_head_sha:` stamps in today's segment | 52 | `grep -c` on the live segment |
| `worktree_path:` stamps anywhere | 0 | `grep` returns empty |
| Auto-reap default | off (`DEFAULT_AUTO_REAP_INTERVAL_SECS = 0`) | `reaper.rs:143` |
| Reason it is off | it closed a live agent's claim | `reaper.rs:115-142` |

## Root cause

**Every liveness signal is a claim the agent makes about itself.**

`LivenessSignals` (`liveness.rs:51-68`) carries four ages — heartbeat, inject/ack,
code-progress, declared-work. All four are stamped by the agent onto its own beat. That
detects an agent that *stopped*. It cannot detect an agent that is hung, looping, or
confused while still beating, because such an agent keeps stamping.

Two consequences, opposite in direction:

| Error | Mechanism | Symptom today |
|---|---|---|
| False positive — live agent reaped | `renew_claim_lease` (`store.rs:1286`, `:2221`) writes only `claim-index.json`; the reaper rebuilds its index from facts (`reaper.rs:285`) and never reads that sidecar | auto-reap disabled by default; RC-051's stated precondition is wrong |
| False negative — dead agent's claim held | no observer can prove absence; only the agent's own silence accrues | 63 claims expired for 2 days, unreclaimable |

## Depends-on (reads-from)

Every data path the new code reads, and whether the contract is verified.

| Contract | Read by | Status | Evidence |
|---|---|---|---|
| `LivenessSignals` four-slot shape | S2 (adds a fifth) | verified | `liveness.rs:51-68` read directly |
| `branch_head_sha:` evidence stamp | S2 (adds `worktree_path:` beside it) | verified | writer at `lib.rs:1803`; 52 stamps in today's segment |
| `code_progress_age_per_tool` derivation | S2, S3 | verified | `store.rs:3101-3120` |
| `claim-index.json` sidecar | S1 (replaces as the renewal path) | verified | `store.rs:1286`, `:2221`; reaper does not read it (`reaper.rs:285`) |
| `claim_reclaim_eligible` | S1, S3 | verified | `store.rs:552-587` |
| `DEFAULT_AUTO_REAP_INTERVAL_SECS` + rationale | S3 | verified | `reaper.rs:115-143` |
| Compose reserve calculation | S4 | verified | `store.rs:3432-3478` |
| Envelope assembly after compose | S4 | verified | `lib.rs:2922-2938` |
| `system_health` dedup key | S5 | verified | `store.rs:2981-3024` |
| Hook renderer squad/claim filters | S6 | verified | `hooks/rally-coordination-hook.sh:749-806` |
| `event_envelope::authorize` callers | S7 | verified | zero production call sites; `event_envelope.rs:292-312` |
| Worktree path per live session | S2 | **unverified — this is the gap S2 closes** | `grep worktree_path:` on the ledger returns empty; recorded only in the `done` summary (`lib.rs:3942`) |

## Approach lenses — Path A vs Path B

**Path A** — make renewal durable (D3 alone). Fixes the false positive. Auto-reap stays off,
because reaping still cannot distinguish quiet-but-alive from gone.

**Path B** — durable renewal *plus* an external observer. Fixes both directions, which is
what makes auto-reap safe to enable by default. The typed contract extends by one evidence
stamp (`worktree_path:`) alongside the `branch_head_sha:` stamp that already exists, and the
observer is pure git plumbing with no new dependency.

**Path B chosen.** The named future capability it unlocks is auto-reap on by default — the
thing that actually drains the 63 stuck claims without an operator running a command. Path A
delivers a fix nobody turns on.

## Segments

Segments are MECE by file ownership. `store.rs` is the contention hotspot — a peer clobbered
uncommitted `store.rs` edits earlier in this cycle — so every segment that touches it runs in
its own worktree and commits path-scoped.

### S1 — Durable renewal (D3 / RC-053) · owner: lifecycle lane

**Owns:** `crates/rally-cli/src/store.rs` (`renew_claim_lease` only),
`crates/rally-cli/src/claim_authority.rs`, `crates/rally-cli/src/reaper.rs`

Renewal becomes a durable fact the projection honors, not a sidecar write. Correct RC-051's
precondition, which states that adding a renewal caller is sufficient — it is not, because the
reaper never reads what renewal writes.

**Acceptance:** a claim renewed after its original lease survives a reap pass. Reverting the
durable-write hunk must fail that test.

### S2 — Observed liveness probe · owner: observer lane · depends on S1

**Owns:** new `crates/rally-cli/src/observed_liveness.rs`,
`crates/rally-cli/src/lib.rs` (the beat-stamping function that already writes
`branch_head_sha:`, ~`:1786-1803`)

Add the `worktree_path:` stamp so an observer knows where to look. Add a probe that reads,
per session, without agent cooperation: worktree `HEAD`, `status --porcelain` dirty count,
newest tracked-file mtime, and whether the recorded pid is alive. Feed the result as a
**fifth signal** — observed, not reported — into `LivenessSignals`.

Fail-open is preserved: an unreadable worktree yields `None`, which cannot demote.

**Acceptance:** a session whose worktree HEAD has not moved and whose process is gone is
graded stale *even while its heartbeat is fresh*. This is the case no current signal catches;
the test must fail on today's code.

### S3 — Reaper acts on observed evidence · owner: lifecycle lane · depends on S1, S2

**Owns:** `crates/rally-cli/src/reaper.rs`, `crates/rally-cli/src/hooks_config.rs`

Reap requires observed corroboration, not self-report alone. Re-evaluate whether
`DEFAULT_AUTO_REAP_INTERVAL_SECS` can leave zero — with a recommendation and evidence, not a
flip. Whether it ships on is an operator decision; whether it is *safe* to ship on is this
segment's finding.

**Acceptance:** replay the incident that disabled auto-reap — a live agent, quiet, mid-work —
and assert its claim survives. Then a crashed agent's claim is reaped in the same run.

### S4 — Budget is not a response ceiling (D4 / RC-054 + D12) · owner: composition lane

**Owns:** `crates/rally-cli/src/store.rs` (compose path only),
`crates/rally-cli/src/lib.rs` (envelope assembly, ~`:2922`)

The reserve omits fixed snapshot fields, totals, readers, mission, composition metadata, and
assigned handoffs; the envelope then adds readers, mission, and `agent_injectability` *after*
composition; `emitted_bytes` omits the composition block because size is computed before
assignment; `over_budget` derives from the initial reserve only, so a response can exceed
budget while reporting it did not. D12 folds in: `--include-archived` is complete only when no
explicit `--budget-bytes` is supplied — document it or override it.

**Acceptance:** on a ledger at 10× current size, the emitted response is under the ceiling and
`over_budget` is true whenever it is not. Adjacent move: exactly one item per bucket, where
today no omission entry is created at all.

### S5 — `system_health` unbounded growth (D5) · owner: composition lane · after S4

**Owns:** `crates/rally-cli/src/store.rs` (`system_health` dedup only)

Dedup by prefix class rather than complete subject, so an `external-intake:`-style prefix
cannot mint unbounded distinct subjects inside a never-cut bucket.

**Acceptance:** 1,000 synthetic subjects sharing a prefix collapse to the prefix's bound.

### S6 — Rust/JS fail-direction parity (D11) · owner: hook lane

**Owns:** `hooks/rally-coordination-hook.sh` (renderer only),
`crates/rally-cli/tests/` (new parity test)

Rust keeps Live *and* Unknown squads; the renderer treats only `status === "active"` as
present, so the prompt can omit a claim the write-check still enforces. Rust ignores
`lease_expires_at` when projecting active claims; the renderer hides parseably-expired ones.
Pick one direction per case and make both sides agree.

**Acceptance:** a claim visible to `check before-write` is visible in the rendered prompt, and
the converse. This is the defect an agent actually feels — blocked by a claim it was never
shown.

### S7 — Delete `event_envelope::authorize` (D13) · owner: any · isolated

**Owns:** `crates/rally-cli/src/event_envelope.rs`

Computed authorization with no call site, documented as advisory. Third instance of the
register's "computed verdict, no consumer" pattern. Remove it rather than preserve it; if it
should live, it needs a caller and that is a different plan.

**Acceptance:** deletion compiles and no test regresses. No user-visible change — stated
plainly rather than dressed as a benefit.

## Parallelism

Three lanes run concurrently in separate worktrees. Within a lane, order is strict.

| Lane | Segments | Blocked by |
|---|---|---|
| lifecycle | S1 → S3 | S3 waits on S2 |
| observer | S2 | S1 (needs the renewal fact shape) |
| composition | S4 → S5 | — |
| hook | S6 | — |
| isolated | S7 | — |

S7 can land at any time. S4/S5 and S6 never touch the lifecycle files.

## Outcomes a user can observe

| Segment | Before | After | How to check |
|---|---|---|---|
| S1+S3 | a long-running agent loses its claim mid-task | renewed claims survive; crashed agents' claims are reclaimed | run an agent past its lease; the claim holds |
| S2 | a wedged agent reads as healthy | wedged agent grades stale on observed evidence | kill an agent mid-work; its claim frees without operator action |
| S4 | `rally room --json` can exceed its ceiling silently | response stays under the ceiling; truncation is reported with true totals | `rally room --json \| wc -c` on a large ledger |
| S5 | room size creeps upward over months | flat | 1,000-subject synthetic |
| S6 | an agent is blocked by a claim it was never shown | prompt and enforcement agree | conflicting claim, compare both views |
| S7 | — | — | none; dead-code removal |

## Validation

**Every segment closes only on a proven adversarial control** — a test that fails when the fix
is reverted. This cycle produced three controls that passed against broken code: a parity test
comparing two identical wrong answers, a retry control whose hold was shorter than the timeout
it was meant to exercise, and a SIGPIPE test that redirected stdout so no pipe ever broke. So
each control also answers: *what does this not cover?*

**Adjacent-move testing is mandatory.** Two "proven" controls this cycle were proven against
the move the fix anticipated rather than the one next to it. For each segment, ask what
authority or invariant the control trusts, and whether an actor can simply assert it.

**Performance measurement runs on a quiesced host.** Twelve orphaned processes at 81% CPU
pushed load to 48 during this cycle and invalidated a benchmark set that was reported as
findings. Follow the precedent set by the reaper-scale test: assert against the product's own
default watchdog rather than a stopwatch, so the criterion is the contract, not the machine.

**Architecture review uses the register's pattern catalog, not NavGator** — two consecutive
full scans returned 402 of 412 edges pointing at components that do not exist. The catalog has
three named shapes and has caught real instances of each: a computed verdict with no call site
(S7 is one, S1 is another), a fix scoped to the instance rather than the class, and two
projections of one fact disagreeing (S6 is exactly this). Classify each segment before writing
the fix.

**An independent reviewer grades the result.** Every self-graded verdict that mattered this
cycle needed correction.

## Risks

| Risk | Mitigation |
|---|---|
| `store.rs` clobbering between lanes | worktree isolation, path-scoped commits, MECE ownership above |
| S2's probe cost on many worktrees | git plumbing + stat only; budget it and measure, no LLM in the path |
| Observed signal misgrades a legitimately idle agent | fail-open — unreadable or absent evidence yields `None` and cannot demote |
| Auto-reap re-enabled prematurely | S3 delivers a recommendation with evidence; the flip is an operator decision |
| Renewal fact inflates the ledger | renewal is a fact per beat, not per write; measure segment growth before landing |

## Out of scope

RC-063 authority model, the history-scrub decision, and tagging v0.2.0 — all operator
decisions with completed decision packets. D8 remains deliberately uncontrolled per its
existing entry.
