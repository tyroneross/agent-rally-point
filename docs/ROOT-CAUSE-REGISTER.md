<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Root-cause register

**Standing rule (operator-set 2026-07-30): every issue that surfaces gets an entry here.**
No exceptions for "small", "already fixed", or "just a flake". An issue with no entry is an
issue nobody is counting, and recurrence is the signal this register exists to catch.

## Why this file exists

Three separate defects surfaced in a single release session on 2026-07-30, and each had the
same shape: **an operation reported success while its effect was silently lost.** They were
being handled as unrelated one-offs. They are not obviously unrelated, and nothing in the repo
was accumulating them in one place where the pattern could be seen.

The second reason: two prior commits closed `RC-005` as root-caused. It came back a third time.
A root cause that does not survive contact with recurrence was a hypothesis, not a cause.

## Lifecycle — an entry is not done when the symptom stops

| State | Meaning |
|-------|---------|
| `observed` | Symptom recorded with evidence. No mechanism established yet. |
| `mechanism` | The causal chain is known and cited to file:line. Not yet fixed. |
| `fixed` | Change landed. **Not** a terminal state. |
| `controlled` | An adversarial run proved the control fires — a test that fails before the fix and passes after, with the test itself validated by mutation. |
| `recurred` | Came back after `fixed` or `controlled`. The prior root cause was wrong; say why in the entry. |

**Only `controlled` closes an entry.** Per the operator's standing note: *a verification protocol
that has never been exercised is a hypothesis.* `fixed` means the code changed. `controlled`
means someone tried to break it and failed.

## Register

### RC-001 — `rally inject` returns `ok: true` without delivering
- **State:** `mechanism` — **root cause: Rally has no owner for the receive side of a message.**
- **Mechanism (`docs/assessment-2026-07-30-delivery-architecture.md`):** every "delivery" is a
  best-effort act by the sender's short-lived CLI process — keystrokes into a guessed pane, or an
  append to an inbox that nothing on this machine reads — aimed at identities minted ad hoc per
  process. There is no resident receiver, no receiver-authored acknowledgement, and no registry of
  who can be reached how.
- **Verified independently by me** (ledger replay + `ps`, run after the report to confirm):
  **706 of 740 wake facts are `pending`** — 95%, and they never leave that state. **143 distinct
  tool ids** in one room. **No `rallyd`, `rally-termd`, or `ptyd` process is running at all**, so the
  inbox has no reader. 15 handoffs sit `pending`; 165 carry no status.
- **The human is the delivery daemon.** Four injects tonight (seq 6403, 6469, +2) returned
  `ok: true, status: pending` and reached nobody; every message that actually landed was either
  hand-carried by the operator or pulled from the ledger by the peer on its next turn.
- **Sender-side honesty was improved ~6 times across ≥25 commits since May; delivery never was.**
  Each round made the failure more *legible* (`status: pending`, injectability warnings) without
  making a message arrive. Legibility is not delivery.
- **Suspected contribution from RC-005:** duplicate session identity can route an inject to the
  wrong holder. ⚠️ unconfirmed.
- **Superseded prior state:** observed
- **First seen:** 2026-07-30 (long-standing; see `docs/INJECT-RCA-2026-07-09.md`)
- **Evidence:** Urgent CI-hold warning to `codex:019fb517-…` returned `ok: true` with
  `status: "pending"`, seq 6403, "pending via ledger". Never reached the running Codex session.
  Target was `presence_only_unmanaged`, so no pane to write to.
- **Observed 3× in one session:** (a) the operator's hash-handshake warning, (b) Codex's
  continuity handoff seq 6393 to a Claude session it could not reach, (c) the CI-hold inject above.
  In all three the human hand-carried the message between two agents on the same machine.
- **Owner:** independent delivery-architecture investigation → `docs/assessment-2026-07-30-delivery-architecture.md`
- **Note:** the exit code describes *ledger enqueue*, not *delivery*. Success of the wrong verb.

### RC-002 — audit boundary pinned to a relative diff hash
- **State:** `fixed` (record), `observed` (procedure)
- **Evidence:** `git diff HEAD | shasum -a 256` was agreed as the audit boundary. It failed on
  first use: after commit it returns `e3b0c442…b855` (SHA-256 of the empty string) while content
  is unchanged, so a mechanical re-verify rejects a valid PASS. Worse, `git diff HEAD` omits
  untracked files — at audit time `config/`, `generate_host_surfaces.py`,
  `sync_host_integrations.py`, `rally-release.json`, and `tests/scripts/` were all untracked and
  therefore never covered by hash `203cb7a3…`.
- **Corrected primitive:** pin to an absolute Git **tree** hash. Before commit:
  `git add -A -N && git write-tree`. After commit: `git rev-parse HEAD^{tree}`. Covers tracked
  and untracked alike and survives the commit unchanged.
- **Fixed in:** `d396dea` corrected the `docs/MERGES.md` audit record to tree
  `f5a38d02eb3a1e2ca39a2b5ecea6f8bba4427ddd`.
- **Still open:** the *procedure* is unwritten. `grep -rn "shasum|git diff HEAD"` across `docs/`,
  root `*.md`, and `skills/` returns nothing — the handshake lived only in room conversation,
  which is why nobody could run it as written. Not `controlled` until the release procedure
  cites the tree-hash primitive and one audit exercises it.

### RC-003 — no CLI version handshake between coordinating agents
- **State:** `observed` (deferred out of v0.1.7)
- **Evidence:** during the v0.1.7 release audit, the auditing Claude session's binary was
  `build_id: 0.1.6+744bf06` (via `rally whoami --tool claude_code --json`) while the release
  under audit was 0.1.7. Two agents coordinated through one ledger on different binaries and
  **nothing flagged it** — including during the release that deferred this very gap.
- **Why it matters:** every ledger read/write is interpreted by a binary whose schema
  assumptions are unverified against the peer's.

### RC-004 — agent identity is guessed, not resolved
- **State:** `observed` (deferred out of v0.1.7 as "pane-derived identity")
- **Evidence:** Codex's continuity handoff seq 6393 targeted `claude_code:ppid-39874`, and the
  room offered `claude_code:0995a4e4-…` as the alternative. `rally whoami` resolved the live
  session to a third identity: endpoint
  `term:tyrones-macbook-pro-2.local:685e5ca6-235c-4a68-a006-1554f17b1fda`. The handoff was
  addressed to a session that was not the one running.
- **Related:** PID- and PPID-derived IDs do not survive process churn; tool IDs are self-asserted.

### RC-005 — WAL unlink destroys committed facts; two agents get the same identity
- **State:** ✅ `controlled` as of `4b28c0e` — first entry in this register to earn it.
- **Fix:** `4b28c0e fix(store): close SQLite pools before releasing WAL lock` — vendors a
  `factstr-sqlite` 0.5.2 delta that closes sqlx pools **synchronously**, and closes room-owned pools
  **under the mutation lock**, so SQLite's final WAL checkpoint/unlink completes inside the lock
  window instead of after it.
- **Adversarial evidence (the reason this is `controlled` and not merely `fixed`):**
  - Pre-fix on `36a2789`: **20/40 in-suite failures**, measured twice — once with 58 accumulated
    `$TMPDIR` workspaces, once in a fresh isolated `TMPDIR`. Identical result, so the accumulation
    confound is ruled out.
  - Post-fix on `10a4b05`: **40/40 pass, 0 failures.**
  - Independent reverse mutation (RCA): factstr patched to close synchronously → 0/72 over 1,728
    launches; unpatched → duplicates reproduce.
- **The test was strengthened, not weakened — checked explicitly**, because "weaken the test until it
  passes" is precisely how the three prior fixes looked green. `git diff 280840d..HEAD` on
  `user_journey.rs` shows `n` unchanged and a NEW direct assertion that every child's returned
  `session_id` is unique, plus `assert_eq!(returned_session_ids.len(), n)`. The test now asserts the
  real invariant (uniqueness) rather than the shadow (count), and the 20/40 baseline was measured
  against this stronger test.
- **Superseded prior state:** `mechanism` — real substrate defect, data loss, release-blocking
- **Mechanism (RCA: `docs/rca-2026-07-30-parallel-launch-lost-session.md`):** `factstr-sqlite` 0.5.2
  `impl Drop for SqliteStore` (`sqlite_store.rs:430`) shuts down its delivery thread but never calls
  `pool.close()`. sqlx closes the sqlite handles on background threads *after* the rally op returned
  and released `.rally/mutation.lock`. sqlite's last-connection-close protocol checkpoints and
  **unlinks `facts.db-wal`**. Direct mode opens a fresh store per op (`warm_fact_store` is `None` for
  CLI processes), so a 16–24 process cold-start stampede races that unlink against another process's
  open/append window. A 2 ms poller filmed a 12,392-byte WAL holding a committed reservation unlinked
  ~80 ms after its op returned, with `facts.db` main never growing past its 36,864-byte schema size —
  every committed row lived only in the WAL. The next reader sees `version=None count=0` on the same
  inode after 13 commits, so its `append_if` CAS validates against the gutted view and **succeeds**.
  Two processes exit 0 holding `claude-01`.
- **The count assert was the shadow. The broken invariant is uniqueness**, not persistence. The
  "reservation is CAS-atomic" comment in the test is false in direct mode.
- **Independently re-verified at source** (I read these myself, not taken from the RCA):
  `sqlite_store.rs:430` Drop has no `pool.close()`; `store.rs:3388` `fingerprint_db` hashes only the
  main db file; `store.rs:1738` `refresh_reconcile_cache_after_append` advances counts `+1` from the
  prior sidecar instead of recounting.
- **Measured rates:** pre-fix in-suite 8/25 · standalone hammer ~5/102 workspaces ·
  **committed `36a2789` still 1/72 with the exact CI signature** · factstr patched to close the pool
  synchronously inside the flock **0/72** (1,728 launches). Two-direction mutation check.
- **Production blast radius:** direct mode is how every hook-driven `rally` invocation runs. Any
  concurrent burst can silently lose **any** WAL-resident fact — `say`, `inject`, claims, handoffs —
  not just sessions. JSONL segments retain the data; the derived `facts.db` diverges and is never
  healed (see RC-010). Duplicate session identity means inject/handoff **misdelivery**.
- **Suspected parent of RC-001.** If two agents hold one `session_id`, an inject can be routed to the
  wrong holder. ⚠️ not yet confirmed — this is the first thing the delivery investigation should test.
- **Third "fix" that did not fix it:** `36a2789` (reservation flock + readback) joins `a8bec15` and
  `39970a8` as a probability reducer. Three consecutive root-cause claims on this defect were wrong.
- **Prior symptom record (superseded, kept for the recurrence trail):** third occurrence
- **Evidence:** GitHub CI run 30591797638 on `origin/main` 280840d, quality gate exit 100.
  `rally-cli::user_journey::rally_run_reserves_numbered_ids_under_parallel_launch`,
  `user_journey.rs:2189`, `assert_eq!(sessions.len(), n)` → left 15, right 16. All 16 child
  `rally run` processes exited 0 (the `output.status.success()` loop above passed); only 15
  sessions were readable afterward. The uniqueness asserts sit after line 2189 and never ran,
  so this is a lost row, **not** an ID collision.
- **Not caused by the release:** `git log 744bf06..HEAD -- crates/rally-cli/tests/user_journey.rs`
  is empty. Neither 880e144 nor 280840d touched this test.
- **Prior "root causes" that did not hold:**
  - `a8bec15` — "scale rally_run parallel-launch test to host (CI over-subscription fix)".
    Lowered concurrency. Reduces probability; does not remove a race.
  - `39970a8` — "kill user_journey parallel-launch flake at root (B-test-flake)".
    Claimed root fix. Recurred.
- **Owner:** independent RCA → `docs/rca-2026-07-30-parallel-launch-lost-session.md`
- **Open question that decides severity:** can `rally run` exit 0 while its session registration
  is lost in production, or is this confined to the test harness? If the former, RC-005 and
  RC-001 are the same defect class — a write that acks without landing.

### RC-006 — local pre-push gate passes while CI fails on the same commit
- **State:** `observed`
- **Evidence:** the isolated-worktree pre-push gate on 280840d passed (fmt, clippy `-D warnings`,
  serialized cargo tests, host parity). GitHub CI on the identical commit failed. A gate that is
  green locally and red remotely on the same tree is not a gate — it is a coin flip whose
  outcome depends on core count, since `n` scales with `available_parallelism()`.
- **Consequence:** "full pre-push gate passed" was reported as release-ready evidence, and it
  was not sufficient to predict CI.

### RC-007 — no positive-ACK guarantee on handoffs
- **State:** `observed` (deferred out of v0.1.7)
- **Partial mechanism exists:** `rally inject --handoff` documents "waits for target-authored
  Rally ACK by default; no ACK means assume not received and follow fallback_plan". Unverified
  whether it fires in practice, and `--text` injects (the common path) have no such guarantee.
- **Not `controlled`:** no adversarial run has exercised the timeout path.

### RC-008 — unconsumed coordination state accumulates without bound
- **State:** `observed`
- **Evidence:** `rally room` reports 42 open handoffs and 1234 stale facts against
  `system_health=82`. Open handoffs date back to 2026-07-03.
- **Why it belongs here:** if handoffs are routinely never consumed, "open handoff" carries no
  signal, and a genuinely urgent one is indistinguishable from four weeks of residue.

### RC-009 — process fragmentation
- **State:** `observed` (deferred out of v0.1.7)
- **Evidence:** stale prepush worktree at `3a17fe8` still registered in `git worktree list`
  under `/private/var/folders/.../rally-prepush-wt.6oysNY/`. Multiple rally processes, daemons,
  and PID-keyed sessions with no single reaper of record.

### RC-010 — reconcile cannot detect the loss it exists to heal
- **State:** ✅ `controlled` as of `4b28c0e`, with a noted coverage gap (below).
- **Fix:** the sidecar now fingerprints **`facts.db` and its WAL** (`fingerprint_wal`, `store.rs:3429`),
  and `refresh_reconcile_cache_after_append` **remeasures** post-append counts instead of advancing
  `+1` from a possibly-stale sidecar.
- **Control validated by mutation** (run by me in a throwaway detached worktree so Codex's `main` was
  never touched; worktree removed, `main` verified clean):
  - `store::ledger_tests::step3_wal_state_change_invalidates_an_otherwise_matching_sidecar` passes on
    `10a4b05`.
  - Mutating the three *reconcile-write* sites (`store.rs:3532/3653/3683`) to `wal_fingerprint: None`
    → **test still passes**. The mutant survived.
  - Additionally mutating the *append-refresh* site (`store.rs:1793`) → **test FAILS**.
  - So the test is genuinely anchored on the append-refresh path — which is the exact site that
    caused RC-010 — and is a valid control for the defect as diagnosed.
- ⚠️ **Coverage gap:** the three reconcile-write sites are not covered; their WAL fingerprint could be
  silently dropped in a future edit without any test noticing. Worth a follow-up assertion.
- **Superseded prior state:** `mechanism`
- **Evidence (verified at source):** `reconcile_segments_and_db` (`store.rs:3526`) fast-paths on the
  sidecar when fingerprints match. `fingerprint_db` (`store.rs:3388`) hashes **only the main db file**
  — in WAL mode the main file's bytes do not change on append, so WAL loss is invisible to it.
  `refresh_reconcile_cache_after_append` (`store.rs:1738`) advances `canonical_count`/`db_count`
  **by one from the previous sidecar** rather than recounting, so the first post-wipe writer inherits
  the pre-wipe sidecar (13/13), writes 14/14 while the db truly holds 1 event, and every later op
  fast-paths on that self-consistent lie.
- **Independent of RC-005.** Even after RC-005 is fixed, this safety net is blind to any WAL-resident
  divergence and will keep certifying a truncated db as reconciled. It also makes RC-005 permanent
  rather than transient, which is what turned a race into data loss.
- **The docstring above `refresh_reconcile_cache_after_append` asserts "No data loss is possible."**
  That is true of the JSONL segments and false of the derived db. A comment that states the
  safety property it does not actually provide is how this survived review.

### RC-011 — `--tmux-bin /usr/bin/true` makes the test's own premise untestable
- **State:** `observed`
- **Evidence:** the parallel-launch test launches with `--tmux-bin /usr/bin/true`, so the "pane"
  exits immediately. Any liveness/reaping behavior downstream is therefore exercised against a
  process that is already dead at assert time.
- **Why it belongs here:** it was a leading candidate mechanism for RC-005 and was ruled out, but it
  means the test cannot distinguish "session never registered" from "session registered then reaped."
  A test whose fixture cannot separate two hypotheses will keep producing ambiguous root causes —
  which is consistent with three wrong ones in a row.

### RC-012 — `main` CI has been red for 19 days and the push policy never noticed
- **State:** `observed`
- **Evidence:** `gh run list --branch main --limit 20` → **the most recent successful run on `main`
  was 2026-07-11T18:27:30Z**. Ten failures since. Four consecutive failures precede tonight's.
- **Why it matters:** the room mission states the push policy as *"push to origin main and
  `~/.local/bin/rally` reinstall are AUTHORIZED after green CI with a recorded run close."*
  There has been no green CI on `main` to authorize against for nineteen days. Either the policy
  is being satisfied by something other than CI, or it is being bypassed without anyone noticing —
  and RC-006 (local gate green, CI red) is how that goes unobserved from inside a session.
- **Consequence:** "CI is red" carried no signal tonight, because CI is *always* red. A permanently
  failing gate is indistinguishable from a newly failing one, so RC-005's CI failure had to be
  caught by reading the log rather than by the gate doing its job.

## Working hypothesis across entries

RC-001, RC-005, and RC-007 share one shape: **an operation returns success for a step that is
not the step the caller cares about.** Enqueue-succeeded reported as delivered. Process-exited-0
reported as registered. Inject-accepted reported as received. If the delivery-architecture
investigation and the RC-005 RCA converge on that, these are not nine issues — they are one
architectural miss (no end-to-end acknowledgement, only local step acks) wearing nine faces,
and the four gaps deferred out of v0.1.7 should be sequenced as one workstream rather than four.

⚠️ Unverified. Two independent investigations are running and neither has reported. This
hypothesis is recorded so it can be **disproved**, not adopted.
