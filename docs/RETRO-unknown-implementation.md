<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Retro: "I couldn't find the pi-dynamic implementation"

> **Type:** systems analysis (not discipline/effort). **Date:** 2026-05-30.
> **Verification:** ✅ confirmed against artifacts — `docs/PLAN-pi-dynamic-seam.md` (the spec the
> agent saw and dismissed), `docs/ORCHESTRATION.md:20-22,52-53,88,99` (B13 → unblocks B2/L5;
> B13 landed sync 31), and `git log` commit `b3a6f33 feat(seam): pi-dynamic observation seam …
> rally dag, wake-due` (the implementation that existed).

---

## What happened

User: *"Also added the implementation of pi-dynamic-workflows into rally point."* The agent searched
the working tree, `git log --all`, sibling repos, and `dynamic-workflows/`. It **found** the
planning docs (`PLAN-pi-dynamic-seam.md`, `CONTEXT-take-best-and-pi-dynamic.md`,
`pi-dynamic-assessment-handoff.md`) — then **dismissed** them as "just pre-existing planning docs,
not an implementation," concluded **not-found**, and asked the user. The user redirected: *"check
build loop memory and documents and adjacent repos."* The answer was inside the docs already
surfaced: `PLAN-pi-dynamic-seam.md` **was** the spec, its only blocker **B13** had just landed
(ORCHESTRATION sync 31), and the seam commit `b3a6f33` existed. The agent had the evidence in hand
and walked past it.

## Four-question systems diagnostic

- **Handoffs.** The repo's own protocol stamps state across three coupled surfaces — `PLAN-*.md`
  (spec) → `ORCHESTRATION.md` (live lane/backlog status, e.g. "B13 → unblocks B2/L5") → `git log`
  (landed commits). The agent read one surface (loose docs) and never joined it to the status ledger
  that would have said "this is now buildable/built." The handoff exists; the agent didn't traverse
  it.
- **Freedom.** The agent had full freedom to read ORCHESTRATION.md and build-loop memory. It
  self-limited the search to a too-narrow definition of the target ("implementation = committed
  source files") and stopped early. Over-constrained interpretation, not over-constrained tooling.
- **Simplicity.** A found PLAN/spec is the *simplest possible* positive evidence that a feature
  exists or is imminent — yet it was treated as a negative. The cheap signal was discarded in favor
  of an expensive, narrower one (matching file names).
- **Communications.** "Implementation" was read as a strict type (code), when in this repo's
  vocabulary it routinely means **an unblocked, planned-and-now-buildable seam**. No disambiguation
  step ("does the user mean code, or the spec that just went green?") before concluding not-found.

## The system gap (not discipline)

A fresh agent runs the same failure: it equates "implementation" with committed source, so a
**found PLAN/spec is filed as a negative**, and it never consults the repo's own status ledger
(`ORCHESTRATION.md`) or build-loop memory before declaring not-found — even though those are the two
places that resolve "is this built / unblocked?" The gap is a missing **lookup-completion rule**, not
a lapse in care.

## Durable fix (falsifiable rule)

**When a user references a feature/implementation you cannot find as committed code, you may NOT
conclude "not found" until you have (a) READ — not dismissed — every PLAN/spec/handoff doc your own
search surfaced in the repo, (b) joined them to the repo's status ledger (`docs/ORCHESTRATION.md` or
equivalent) and recent `git log` to check whether the spec's blockers cleared / it landed, and (c)
checked build-loop memory.** A found spec whose dependency just cleared **is** the implementation
under this repo's vocabulary; treat "implementation" as possibly meaning *an unblocked spec*, not
strictly source. Only after all three return empty may you report not-found — and then report *what
you checked*, not just the conclusion.

*Falsifiable:* if a future "can't find X" episode resolves to a doc/ledger entry the agent had
already surfaced, this rule was skipped.
