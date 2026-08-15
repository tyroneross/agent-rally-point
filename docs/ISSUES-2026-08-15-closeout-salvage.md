<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Closeout of the stale 2026-08 worktrees and branches

Stamp: `closeout-20260815T233859Z`.

Filed before deletion, per the standing rule that a dropped branch's surviving
ideas get an entry rather than a tombstone. Every branch and every dirty
worktree named here is preserved at
`refs/archive/closeout-20260815T233859Z/*` (14 refs: 12 `branch-*`, 2
`dirty-worktree-*`) and in
`archive/bundles/pre-closeout-20260815T233859Z.bundle` (`git bundle verify`:
okay, complete history, 26 MiB).

**These artifacts are local-only by policy and are never pushed.** `archive/` is
de-tracked at `.gitignore:128` (ARP-R-06), the repository declares no
`remote.origin.push` refspec, and `git ls-remote --refs origin 'refs/archive/*'`
returns 0 — checked before and after this closeout. `refs/archive/*` is outside
the default push refspec, so no ordinary `git push` can carry it; only
`--mirror`/`--all` would, and neither was run.

Recover any entry with:

```sh
git branch <name> refs/archive/closeout-20260815T233859Z/branch-<name-with-dashes>
```

Nothing below requires recovering a ref to act on, with the single exception of
**S6**, which names work that exists nowhere else.

## Verification standard used

Each branch was re-checked against `main` immediately before archiving, not
taken from the prior read-only assessment: `git merge-base --is-ancestor <b>
main` for ancestry and `git cherry main <b>` for patch-equivalence. Both dirty
worktrees had their uncommitted delta diffed against their own `HEAD` and every
added line tested against `main` before any ref was written.

---

## Group A — contained in `main` (ancestor, zero unique patches)

Eight branches whose tips `git merge-base --is-ancestor` places on `main` and
for which `git cherry main <b>` reports zero unique patches. Deleting the branch
name removes a label, not a commit; the commits remain reachable from `main`.

| Branch | Tip | Archived as |
|---|---|---|
| `bl/merge-prep-997022` | `a1ead94` | `branch-bl-merge-prep-997022` |
| `bl/r1-r5-authority` | `3905283` | `branch-bl-r1-r5-authority` |
| `bl/rc-071a` | `2d05ca0` | `branch-bl-rc-071a` |
| `bl/router-r0-r1-019fdfe3` | `8d21f2c` | `branch-bl-router-r0-r1-019fdfe3` |
| `bl/run-997022` | `dd35535` | `branch-bl-run-997022` |
| `codex/s10-o33c-composite` | `030cfbb` | `branch-codex-s10-o33c-composite` |
| `oc/cd32d567c308ad187e32e67164285d02` | `3d27f28` | `branch-oc-cd32d567c308ad187e32e67164285d02` |

`bl/run-997022-o33a` (`aae3b8f`, archived as `branch-bl-run-997022-o33a`) is the
one member of this group that is **not** an ancestor of `main`. Its two commits
— `abce783` "Fix native read deconfliction" and `aae3b8f` "fix(workflows):
separate read activity from ownership" — are patch-equivalent to commits already
on `main`, which `git cherry main bl/run-997022-o33a` reports as `-` for both.
The distinction matters only for how it was proved, not for what survives.

Nothing was ported from Group A. There was nothing to port.

---

## Group B — superseded, one unique commit each

### S4 — `bl/run-788004`: the native before-write hook, landed then deliberately reverted

**Source:** `bl/run-788004` (`7fc4879`, "feat(hooks): add native before-write
transaction").
**Preserved at:** `refs/archive/closeout-20260815T233859Z/branch-bl-run-788004`.

**Why the branch is closed.** The same change landed on
`bl/step4-native-hook-attempt` as `f23b22a` — an identical subject line — and was
then reverted on `main` by `f57056d`, "Revert 'feat(hooks): add native
before-write transaction'" (`f57056d` is an ancestor of `main`). The branch tip
is therefore not a missing feature but a second copy of an attempt `main` has
already tried and backed out.

**This is the one closure that is not final.** The revert closed the *attempt*,
not the *intent*. The operator has separately decided to keep
`bl/step4-native-hook-attempt` alive as the donor for a native before-write hook
(option A) — see the operator decisions section below. `bl/run-788004` is closed
because it is the redundant copy, not because the idea is dead. Anyone resuming
the work should start from the kept donor branch and read `f57056d` first for
why the first attempt was withdrawn.

### S5 — `wip/hook-shebang-stash-recovery`: a snapshot its own author marked not-for-merge

**Source:** `wip/hook-shebang-stash-recovery` (`91bb046`, "recovery(stash):
contents of the 2026-07 'fix/hook-shebang-and-flake-instrumentation' stash").
**Preserved at:**
`refs/archive/closeout-20260815T233859Z/branch-wip-hook-shebang-stash-recovery`.

**Why the branch is closed.** Its commit message ends "Snapshot only; not for
merge." It is the residue of a `git stash branch` operation on 2026-08-14 that
was aiming at different work; the doctor/ledger-health edits that operation
intended to preserve had already been committed to `main` by their owner as
`c004d67` ("fix(doctor): make rally doctor work ON a broken store, and never
delete"), which is an ancestor of `main`. The unique commit's content is a
`.rally/log` JSONL corpus plus a vendored `Cargo.lock` — recovery debris, not
source.

**Correction to the incoming assessment, recorded because it changes the proof
and not the outcome.** The two code fixes carried in this branch's history —
`13c7137` "fix(tests): quiesce sqlite pool before corruption-injection surgery
(closes #48)" and `9759fb1` "fix(tests): invoke hook via its shebang (dash breaks
pipefail)" — were described as "already on main". They are **not ancestors of
`main`**. Their *content* is on `main` under different shas: the shebang
rationale comment appears verbatim at
`crates/rally-cli/tests/hook_wrapper_contract.rs:91-92`, and `quiesce` appears
twice in `main`'s `crates/rally-cli/src/store.rs`. Both commits are also
ancestors of `91bb046`, so the archive ref preserves them by reachability. No
content is lost either way, but the branch was closed on content-equivalence,
not on ancestry.

### S3 (already documented) — `feat/fact-retraction`

**Source:** `feat/fact-retraction` (`4c4ea01`, "feat(retract): withdraw a posted
fact without rewriting the ledger").
**Preserved at:**
`refs/archive/closeout-20260815T233859Z/branch-feat-fact-retraction`.

Superseded by the stabilized `crates/rally-cli/src/retraction.rs` on `main`. The
surviving open question — that all 13 retraction tests exercise the wrapper and
none covers the direct `snapshot_from_facts_with_policy_at` caller that the
snapshot cache uses — is already written up as **§S3 of
`docs/ISSUES-2026-08-13-integration-salvage.md`** and is not restated here. That
gap remains open.

### `donor/o13-retraction-and-attribution`: a byte-identical second copy of S3

**Source:** `donor/o13-retraction-and-attribution` (`2123cdc`,
"recovery(snapshot): canonical dirty tree + untracked work at 20260811T024244Z").
**Preserved at:**
`refs/archive/closeout-20260815T233859Z/branch-donor-o13-retraction-and-attribution`.

**Why the branch is closed.** Its `crates/rally-cli/src/retraction.rs` is the
same blob as `feat/fact-retraction`'s — both resolve to `43716dc` — so it carries
no retraction work that S3 does not. `main` has since stabilized that file
(`crates/rally-cli/src/retraction.rs`, blob `26ced64`). This is a duplicate
donor, closed for the same reason as S3 and adding nothing to it.

---

## Group C — dirty worktrees

### `cd32d567c308ad187e32e67164285d02` — RC-044 scrub, fully landed

**Snapshot:** `refs/archive/closeout-20260815T233859Z/dirty-worktree-cd32d567c308ad187e32e67164285d02`
(`5990f71`), committed over the worktree's own `HEAD` `3d27f28` using a temporary
index; the worktree's real index was never written.

**Port check — nothing to port, and here is the evidence.** The uncommitted delta
was four paths. Every one is already on `main`:

- `crates/rally-cli/src/store.rs` — the `.corrupt.` → `.<quarantined>.` scrub is
  on `main` at `store.rs:8483`, and its test
  `quarantine_filename_in_an_error_is_not_a_corruption_report` at
  `store.rs:11915`.
- `scripts/repro_facts_db_corruption.sh` — tracked on `main` and **byte-identical**
  to the worktree's untracked copy (`diff -q`: no difference).
- `CHANGELOG.md` and `docs/ROOT-CAUSE-REGISTER.md` — every non-blank added line
  was matched against `main`'s copy of the same file with a fixed-string
  whole-line search; zero lines were missing.

The snapshot exists for provenance, not because it holds anything `main` lacks.

### `s10-o33c-composite` — composite landed; one feature did not

**Snapshot:** `refs/archive/closeout-20260815T233859Z/dirty-worktree-s10-o33c-composite`
(`927cdd3`), committed over `030cfbb` by the same temporary-index method.

**The composite itself landed.** `cdfcf86` — "merge: land s10-o33c composite
(read/ownership separation) onto main" — is an ancestor of `main`, and spot
checks confirm the landing: `crates/rally-cli/src/decay.rs` and
`crates/rally-cli/tests/write_authority_daemon_parity.rs` both exist on `main`.

**The snapshot is bit-identical to one already on disk.** This new ref's tree is
the same object as `refs/archive/s10-o33c/dirty-worktree-20260813T0545Z`'s tree
(`b069f45`), so the worktree has not moved since 2026-08-13 and this closeout
preserves nothing that was not already preserved. Duplicated anyway so the
`closeout-20260815T233859Z` namespace is self-contained.

### S6 — the O33-C `read-context` command exists only in this snapshot

This is the one item in the closeout that names work `main` does not have in any
form.

Two untracked files in the worktree implement the `rally read-context` command:

- `crates/rally-cli/src/read_context.rs` (1004 lines)
- `crates/rally-cli/tests/s10_read_awareness.rs` (207 lines)

Neither path exists on `main`. `git grep -i 'read.context' main -- crates/`
returns nothing at all — no implementation, no alternative spelling, no
equivalent under another name. What `main` has is the *plan*:
`docs/plans/2026-08-05-observed-liveness-and-durable-renewal-audit-amendment.md`
describes O33-C at lines 597, 606, 726, 747 and 775 as a "new read-context
command/schema/journey", to be built after S9/S10. The implementation was
written and never landed.

**Port status: pending, file busy — deliberately not applied.** Two independent
blockers, either sufficient on its own:

1. The module does not compile against `main`. It imports
   `crate::store::ReadObservation` and `crate::store::ReadWriterObservation`, and
   its caller needs `store::read_context_observation`. None of the three exists
   on `main`. Landing it means editing `crates/rally-cli/src/store.rs`, which is
   **currently dirty on `main` under another agent's active edit** — the brief's
   own do-not-touch list. Editing it would collide with live work.
2. The snapshot dates from 2026-08-10 and targets a `store.rs` that has since
   moved by thousands of lines. "Absent from `main`" is proven; "still correct
   against today's `main`" is **not**, and cannot be without a compile the busy
   file blocks. Applying it blind would be manufacturing a port.

**The patch is staged for whoever picks this up:**
`archive/ports/closeout-20260815T233859Z-s10-read-context.patch` (1223 lines,
gitignored, local-only). It contains the two files as clean additions. The wiring
it also needs, which is **not** in the patch because those hunks are entangled
with the rest of the composite, is visible in the snapshot at:

- `crates/rally-cli/src/lib.rs:138` — `mod read_context;`
- `crates/rally-cli/src/lib.rs:1399` — `CliCommand::ReadContext(args) => command_read_context(args)`
- `crates/rally-cli/src/lib.rs:4382` — `fn command_read_context(...)`
- `crates/rally-cli/src/cli.rs:935` — `read_context_parser()`

**Ask:** decide explicitly whether O33-C `read-context` is still wanted. If yes,
resume from the snapshot ref once `store.rs` is quiet, and re-verify against
current `main` rather than trusting the 2026-08-10 base. If no, say so in the
plan doc so the next sweep does not re-discover it.

---

## Operator decisions recorded

- **`bl/step4-native-hook-attempt` is KEPT**, not archived and not deleted. It is
  the donor for the native before-write hook build (option A). S4 above closes
  the redundant `bl/run-788004` copy only.
- **The two active `oc/` worktrees are KEPT**:
  `.build-loop/worktrees/5d74464d3d3cb9c3bab512d1788fd651` and
  `.build-loop/worktrees/beabaa45daf66e741d10b36f431617e4`, with their branches
  `oc/5d74464d3d3cb9c3bab512d1788fd651` and
  `oc/beabaa45daf66e741d10b36f431617e4`. Both were touched by live runs during
  this closeout. `beabaa45` is dirty and was left dirty and untouched.
- **`main` was not checked out, reset, or stashed.** Its working tree carried
  another agent's in-flight edits throughout. The only change this closeout makes
  to `main` is this file, staged by explicit path.
