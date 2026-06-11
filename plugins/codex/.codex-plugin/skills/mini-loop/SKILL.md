---
name: mini-loop
description: Ultra-light per-task quality loop for Rally Flow / dynamic-workflows fan-out. Wrap each fanned-out task in assess → plan → execute → mini-judge to raise accuracy, with zero tools or scripts. Use when running a workstream task and you want it to actually meet its validation + output contract, not just "be done".
---

<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# mini-loop — the smallest loop that improves a fanned-out task

A stripped-down build loop for **one** dynamic-workflows task. It keeps only the core
assess → plan → execute → judge spine and throws away everything else — no IBR, NavGator,
mockup-gallery, ui-design, architecture/debug scripts, no sub-agents, no extra files. It is
text the host LLM runs in its own context; it adds no dependency and no tool call.

Use it per task inside a Rally Flow fan-out (the per-task loop in `rally-workflows`). The win:
each task is checked against **its own** `validation` and `output` contract before it posts an
artifact, so a wrong-but-plausible result gets caught at the task, not at integration.

## The loop

Run these four steps for the single task you were handed. The task packet gives you `intent`,
`owns`, `validation`, `output`.

1. **Assess** — restate, in one or two lines each: what `intent` actually asks; which paths you
   `owns` (and therefore must NOT step outside); what `validation` command/predicate will judge it;
   what shape `output` must take. If any of these is unclear, stop and ask — do not guess scope.

2. **Plan** — write 1–3 concrete steps. If it needs more than 3, the task is too big; flag it for
   re-decomposition rather than ballooning. Name the files you will touch (must be inside `owns`).

3. **Execute** — do the steps. Stay inside `owns`. Make the smallest change that satisfies `intent`.
   Don't add scope, options, or abstraction the task didn't ask for.

4. **Mini-judge** — judge your own result against the contract, honestly:

   | Check | Pass requires |
   |-------|---------------|
   | Validation | Running the task's `validation` actually succeeds (run it — don't assume). |
   | Output shape | The result matches the declared `output` shape a reviewer would confirm. |
   | Scope | Only `owns` paths changed; no collateral edits. |
   | Intent | The change serves `intent` — not a near-miss that passes validation by accident. |
   | Honesty | No mock/placeholder data, no unverified claim, no "should work". |

   Verdict: **pass** (all true) or **flag** (any false, with the one-line reason).

## On flag

One retry: fix the specific failed check, re-run the mini-judge. Still flagged → **surface it**
(post a `rally say blocker` or return the flag to the orchestrator). Do not loop more than once and
do not paper over a failing `validation`. A second flag means the task or the plan is wrong, which
is information the orchestrator needs — not something to grind on.

## What this is NOT

- Not build-loop. No Phase 1–6 machinery, no critic/optimize/fact-check/learn sub-steps, no agents.
- Not a script. There is nothing to install or run except the task's own `validation`.
- Not a place to add scope. If you find yourself wanting more steps, files, or options than the task
  named, that is a re-decomposition signal for the orchestrator, not work for this loop.

Reference: the Rally Flow per-task loop in [`../rally-workflows/SKILL.md`](../rally-workflows/SKILL.md) §4.
