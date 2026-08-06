<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Proposal — Opinionated cross-agent coordinator on a two-plane `rallyd`

Status: **PROPOSAL — awaiting operator sign-off on the charter amendment (§1).**
Date: 2026-07-10. Author: assistant (interactive session). Research-backed (§5).

> This is a deliberate vision shift. Per `NORTH_STAR.md`, the charter is edited
> "only on a deliberate vision shift," and both `NORTH_STAR.md` and
> `build-loop-memory/.../constitution.md` change together. Neither is touched by
> this doc — this is the proposal that gates that edit.

---

## 0. The ask

Make Rally *persistent and opinionated*: a manager that detects stalled agents,
challenges them, and reconnects/reassigns work — instead of the current passive,
pull-only, advisory substrate.

> **v2 (2026-07-10, post plan-critic).** Adversarial review flipped the
> placement decision and caught two research over-claims. Changes from v1:
> (a) the coordinator is now a **separate process / thin rallyd client**, NOT a
> plane inside rallyd — in-daemon placement imported LLM creds, network egress,
> and delivery side-effects into the single-writer process (violating
> local-first) and gave only asserted, not real, isolation; (b) **advise-only
> needs no charter amendment** and can pilot *before* rallyd exists — the charter
> constrains what *Rally* does, not what an agent *using* Rally does; only
> act-mode (auto-handoff by a non-human) needs the amendment; (c) research claims
> re-scoped to what MAST/Anthropic actually support (§5).

## 1. Charter amendment (gates ACT-mode only)

The current charter (`NORTH_STAR.md`, `constitution.md §Charter`):

> Rally **records and advises; it never gates, grants, schedules, spawns,
> retries, or executes.** … Non-goals: not an orchestrator/scheduler/executor …
> not a human-in-the-loop referee (removing that referee is the point).

**Diagnosis:** the "no agency" rule rested on an unstated assumption — that a
manager would be *context-blind*. A **cross-agent** coordinator breaks that
assumption: it sees every agent, claim, stall, and handoff — context no single
agent has. Independent confirmation: the failure-mode literature (§5) shows
multi-agent breakdowns are fundamentally a **context-sharing** problem, which is
exactly what a globally-scoped coordinator addresses.

**But context ≠ correctness.** A context-rich coordinator can still misjudge
(read a slow-but-correct agent as stalled). So we keep the charter's
*reversibility* instinct while dropping its *no-agency* instinct. Proposed edit:

> **The substrate (`.rally/` ledger + `rally-cli`) never decides or executes.**
> The ecosystem MAY include an opt-in **coordinator** that decides, but it acts
> ONLY through the substrate's reversible, WARN-not-block mechanisms
> (`inject`, `handoff`), under a labeled `coordinator:*` identity, with every
> decision recorded as an attributed fact and overridable by any agent or human.

What this preserves: zero data loss, disposable derived caches, one-owner-per-path
(WARN not block), attribution, host-neutrality of the *substrate*. What it
changes: the *ecosystem* may contain one opinionated actor, clearly labeled,
in a **separate process** from the ledger writer.

Bound the hole explicitly (per critique): the amendment authorizes **at most one
first-party coordinator per repo, running as a distinct process, acting only via
`inject`/`handoff`** — not "the ecosystem MAY include" any number of third-party
deciders.

## 2. Architecture — coordinator is a rallyd *client*, not a plane inside it

**v1 said "one daemon, two planes." That was wrong.** Putting the decider inside
the single-writer daemon imports into the canonical-state writer: (a) LLM API
credentials + network egress (breaks the local-first / no-server non-goal),
(b) `inject` delivery side-effects (tmux/session), (c) shared address space, so a
coordinator panic/OOM takes the writer down. "Quarantined" is false when you
share a process with the only writer of canonical state.

Correct shape: the coordinator is a **separate process** — a host-side agent or a
standalone `rally-coordinator` — that **subscribes to rallyd's socket as a thin
client** and acts through the existing `rally inject` / `rally handoff` verbs.

```
   ┌──────── rallyd (S-P3, charter-PURE, no opinion) ────────┐
   │  single writer → append-only ledger                      │  ← reliability
   │  warm SQLite + in-memory projection                      │    substrate;
   │  serves presence/claims/DAG/stall over socket  ──push──┐ │    fixes WAL/flock
   └────────────────────────▲──────────────────────────────┼─┘
        agents ─CLI(thin)────┘                               │ subscribe
                                                             ▼
   ┌──────── rally-coordinator (SEPARATE process, opt-in) ───┐
   │  subscribes to projection · holds NO canonical state     │
   │  VERIFIES stall vs read-back facts before acting         │ ← MAST refinement
   │  DECIDES (deterministic rules) · LLM only for copy        │
   │  acts ONLY via `rally inject` / `rally handoff`          │
   │  identity coordinator:* · every act an attributed fact   │
   │  crash/OOM/bad-LLM cannot touch rallyd (separate process)│
   └──────────────────────────────────────────────────────────┘
```

Process isolation is now **real**, not asserted: separate address space, separate
lifecycle, separate crash domain. Secrets + network live in the coordinator, not
the writer. Local-first preserved (the coordinator can be gated off entirely).

### Reliability properties (deduped per critique)

Four independent properties + one shared-risk they do NOT cover:

1. **No canonical state in the coordinator** → restart rebuilds from ledger
   (actor-model "let it crash"; Anthropic durable-execution).
2. **Separate process** → coordinator crash/OOM/hung-LLM cannot stall the writer
   or any agent's `before-write` hook. Fleet degrades to today's advisory mode.
3. **Bounded, reversible effects** → acts only via `inject`/`handoff`
   (WARN-not-block, attributed). See §override for the reversal primitive.
4. **Deterministic decision; LLM only for challenge copy** (python-brainstem +
   LLM-deltas).
5. **SHARED RISK — the warm projection is a single point of failure for
   *correctness*.** If presence *semantics* are stale, both rallyd's served view
   and the coordinator's deterministic `stall > N` fire on phantom staleness.
   S-P3 fixes WAL/flock *flake*, NOT presence-semantics staleness — a distinct
   heuristic/cadence bug (see §3). Mitigation: the coordinator's
   verify-stall-before-act cross-checks the projection against read-back ledger
   facts; and presence-semantics is a named act-mode prerequisite, not assumed
   discharged.

### The reversal primitive (§override)

"Reversible" must name a mechanism. A superseding ledger fact does not un-run a
wrong `handoff` (the reassigned work already moved). Define: an `inject` carrying
a coordinator decision is **advisory-first** — the target agent (or a human) can
`rally reject <decision-id>` before it takes a claim, and a `handoff` issued by
`coordinator:*` is **two-stage** (proposed → confirmed) so a mis-targeted
reassignment is caught at the proposal stage, not after work has moved.

### Rejected placements

- **`rally-cli`** — LLM/opinion in the per-commit hook path. Reliability killer.
- **Inside `rallyd`** — the v1 mistake; imports secrets/network/side-effects into
  the single writer (above).
- **Extend `cockpitd`** — cockpitd is iOS session observability, per-Mac; different
  concern. Share the factored crates (`crypto/authz/transport`), not the process.
- **Coordinator as pure host-side agent (no `rally-coordinator` binary)** — viable
  and needs zero new daemon; weaker only in that it can't guarantee single-instance
  or survive host-session restarts. **Recommended for the advise-only pilot
  (§4.1); promote to a managed process for act-mode.**

## 3. Prerequisites — split by mode (per critique)

The "communication failures" motivating this are documented substrate bugs, but
they gate **different modes**, and they are **two distinct classes**:

- **Delivery/consistency flake** — WAL/lock flake under concurrent writers
  (`BACKLOG:70`), stale-binary write-drop, `inject ok ≠ delivered`. Fixed by
  rallyd S-P3 (single writer) + verified delivery.
- **Presence *semantics* staleness** — a heuristic/cadence bug in *what counts as
  present/idle*, NOT a write-path flake. **S-P3 does not fix this.** It must be
  characterized separately before any act-mode `stall` threshold is trustworthy.

Gate rule: **act-mode** (auto-`handoff`) requires *both* classes fixed +
verify-before-act. **Advise-only** requires *neither* — a wrong advisory nudge is
the same WARN-noise trust class as today's advisory substrate, so it is safe to
pilot immediately and is the only way to *measure* the false-stall rate that
decides whether act-mode is ever safe.

## 4. Sequence (parallelized — advise-pilot no longer blocked on rallyd)

**4.1 — Advise-only pilot (NOW, no rallyd, no charter change).**
Coordinator as a host-side agent / CLI poller of the existing ledger. Detects
stalls, posts a *recommendation* via `inject`, records outcome. **Primary goal:
measure the false-stall rate.** This is the promotion evidence §6 requires and
needs none of the infrastructure below.

**4.2 — In parallel: reliability substrate.**
(a) Land `rallyd` S-P3 (warm single-writer projection; fixes flock/WAL).
(b) Characterize + fix presence-semantics staleness.
(c) Verify `inject` delivery (close `inject ok ≠ delivered`).

**4.3 — Charter-safe advisory primitive.**
First-class `stall` derivation + push subscription on rallyd's projection. Still
advisory; no charter change. (plan-critic: the strongest stand-alone idea here.)

**4.4 — [GATE: false-stall rate acceptable AND 4.2 done AND charter §1 signed]
Promote to act-mode.**
Managed `rally-coordinator` process, deterministic policy, verify-stall-before-act,
two-stage `handoff` + `rally reject` override (§override), `coordinator:*` identity,
LLM only for challenge copy.

## 5. Research grounding (2025–2026)

| # | Claim | Evidence | Tier | Note |
|---|-------|----------|------|------|
| 1 | Multi-agent breakdowns are dominated by coordination + context-sharing problems; MAST's inter-agent-misalignment ≈ 37% of failures | MAST — Cemri et al., *Why Do Multi-Agent LLM Systems Fail?*, NeurIPS 2025, arXiv 2503.13657 | T1 | **[INFERRED] mapping:** MAST's 37% comes from *conversational* MAS (ChatDev/MetaGPT-class) — ignored input, info-withholding, derailment — NOT process-liveness *stalls* in a repo fleet. MAST does not test a supervisor as the fix. Correct takeaway: **any coordination change must be paired with verification**, which §4.4 does. Do not read 37% as "stalled agents." |
| 2 | A globally-scoped coordinator has context to judge coordination decisions that no single agent has | Anthropic, *How we built our multi-agent research system*; LangGraph supervisor-vs-swarm literature | T2 | The Anthropic **90.2%** figure is dropped — it measured an orchestrator-*executor* spawning parallel search subagents (≈80% of variance = token/breadth), which this proposal does **not** build, and is not evidence a supervisor judges *stalls* better. [INFERRED] that global context helps stall decisions. |
| 3 | **No silver bullet** — orchestration+verification interventions gave only +14%; a coordinator fixes coordination, NOT specification (42%) or verification (21%). Must ship *with* verification | MAST intervention studies (same paper); ChatDev +15.6% from a verification step | T1 | Honest ceiling; §4.4 verify-before-act is the direct application. |
| 4 | Reliability = supervision tree ("let it crash", restart-from-clean, sibling isolation) + durable execution (checkpoint, resume, persist plan to memory) | Erlang/OTP supervision (established); Anthropic durable-execution ("agents are stateful and errors compound") | T1/T2 | Directly supports separate-process + rebuild-from-ledger (§2). |
| 5 | Production trajectory: start centralized, add decentralized as protocols mature; Rally today = blackboard → adding a coordinator = the blackboard+supervisor hybrid | Enterprise orchestration write-ups (see session sources) | T3 | Directional only; not load-bearing. |

**Calibration for the operator (unchanged, and the point of the whole design):**
this addresses agents that are *stuck, waiting, or duplicating*. It will NOT fix
agents given ambiguous tasks or skipping verification. The research does not prove
a supervisor judges stalls accurately — that is exactly what the §4.1 advise-only
pilot must *measure* before act-mode. Pair with Rally's existing read-back +
source-grounding verification to reach MAST's full prescription.

Session sources (T1→T3): arXiv 2503.13657 (MAST) · anthropic.com/engineering/
multi-agent-research-system · Erlang/OTP supervision-tree literature ·
LangGraph supervisor-vs-swarm comparisons · enterprise orchestration surveys.

## 6. Open decisions

- **Autonomy default** — operator chose: opinionated (act), given cross-agent
  context justifies agency. Recommend shipping `--advise-only` as the *default*
  and promoting to act after the stall-verification path is proven in-repo.
- **`stall > N min` threshold** — derive from `liveness.rs` check-in cadence;
  start conservative (2× expected check-in interval).
- **Coordinator model tier** — Fable (judgment layer) for the decision policy per
  the standing model org; deterministic rules first, LLM only for challenge copy.
