<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Spec — Coordination Mandate ("flexibility within the rules")

User directive (2026-05-31): **require coordination** without scripting agents or
blocking work. Principle: *"Acknowledge once, then trusted; skip it, and your work
is conflicted-out and unmergeable — but never stopped."*

The forcing function is on **understanding (ack)**; the teeth are on **landing (merge)**.
Never on the keystroke. This evolves rally from "never enforces" → "enforces a
minimal rule-set at the merge boundary" while preserving never-block-at-work.

## Layers (build chunks)

- **C1 · Ack gate at enter.** `rally enter` surfaces the context an agent must ingest
  — rules, guardrails, leadership+plan, mission — and the agent records an explicit
  `rally ack`. Until acked, the squad projects `acknowledged: false`. One-time,
  lightweight. *Verify:* `cargo test -p rally-cli ack`.
- **C2 · Liveness conflict-out.** A checkpoint (`rally check liveness` + room
  projection) flips a non-coordinating or unacknowledged squad to `verified: false`
  (status `conflicted`), **releases its open claims** (paths freed), and records a
  durable `risk` alert for the lead + user. Advisory mode may scan the room;
  enforcement requires one exact `--tool <target>` and a separate explicit
  `--actor <release-author>`, so selecting one stale squad cannot release another.
  **Never blocks editing.** *Verify:*
  `cargo test -p rally-cli conflict`.
- **C3 · Merge gate (the teeth).** A real `rally check ci` evaluates the coordination
  predicate against `git diff`: committer has **presence + ack**, and **every changed
  file is claimed** by it (or has a recorded cross-claim handoff). Exit non-zero on
  violation → wired as a required branch-protection check. *Verify:*
  `cargo test -p rally-cli check_ci`.

## Fact model (additive — no schema break)

- **Ack:** `FactKind::Decision`, `subject = "coordination:ack"`, `tool = <agent>`,
  `evidence = ["acked-at-seq:<max_seq>"]`. Projection: a squad is `acknowledged`
  iff it has a `coordination:ack` fact (v1; re-ack-on-context-change is a later
  refinement keyed on mission seq).
- **Conflict-out:** reuse `FactKind::Risk` (`severity=warn`, `scope=["conflicted:<tool>"]`)
  as the alert + a `release` per freed claim. Projection adds `verified: bool` to squads.
- **Merge predicate:** computed live from `git diff --name-only <base>..HEAD` ∩ the
  ledger's claims/presence/ack for the committing tool. No new persisted fact.

## Charter guardrails

- **Never blocks work.** No layer denies a keystroke or a local write. C2 releases
  claims + alerts; C3 fails a *merge check*, not a commit.
- **Records + exposes.** Ack/conflict are facts; status is projection. The lead and
  CI consume status; rally never forces an agent.
- **Bootstrap-safe.** Empty room / first agent / no prior claims must pass (nothing
  to violate yet).

## Explicitly DEFERRED (user, 2026-05-31)

- **Branch-isolation of non-compliant agents** — steering a conflicted agent onto a
  non-canonical branch. Deferred: risk of firing by accident and disrupting work.
  Not built, not wired, not fired. Revisit only on explicit go.
