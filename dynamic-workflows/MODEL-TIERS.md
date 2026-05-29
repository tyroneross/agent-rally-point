<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Model tiers (host-neutral)

Three capability/cost tiers. Each host maps its **own** model family into them — the tier is the
stable abstraction; the model names are the mutable mapping (verify current names before relying).
Coordination rule: **pick the cheapest tier sufficient for the task**, chosen by each agent in its
own family (COORDINATION.md rule 5 — never dictate a Claude tier to a GPT host or vice-versa).

## The tiers

| Tier | Use for | Claude *(runtime: 4.x)* | OpenAI / Codex *(GPT-5.4, 2026-03)* |
|------|---------|--------------------------|--------------------------------------|
| **Frontier** | Lead / orchestration, planning & spec, architecture, novel/ambiguous reasoning, adversarial judgment, cross-lane rulings | **Opus** (4.8) | **GPT-5.4 Pro / Thinking** |
| **Executing** | Defined implementation, code edits, bounded multi-step work, structured extraction, the workhorse | **Sonnet** (4.6) | **GPT-5.4 Mini** |
| **Fast** | Mechanical / classification / ranking, high-volume cheap checks, idle-poll & heartbeat, large fan-out of clearly-defined sub-tasks | **Haiku** (4.5) | **GPT-5.4 Nano** |

*Names as of 2026-05; Claude tiers from the runtime, OpenAI tiers per OpenAI's GPT-5.4 release
(o-series reasoning folded into the "Thinking" tier, Feb 2026). Other hosts (Gemini, etc.) map their
own families into the same three tiers. **Verify current names — they change.**

## Task → tier (defaults)

- **Frontier**: steering the run, writing a plan/spec, architectural decisions, judging another agent's
  output, resolving cross-lane/boundary conflicts, anything ambiguous where a wrong call is expensive.
- **Executing**: applying a written plan, mechanical-but-multi-step refactors, single-file edits,
  schema/structured extraction, the bulk of implementation. *(This session's audit-fix fan-out was here.)*
- **Fast**: residual-scan / lint, classification, idle heartbeats, big read-only fan-outs of *clearly
  defined* checks where each unit is simple and volume is the point.

The boundary that matters most: **defined task → don't pay frontier prices.** A clearly-scoped
assessment or implementation runs at Executing (or Fast for trivial high-volume units); Frontier is
for steering and judgment, not throughput.

## Empirical calibration (A/B)

We A/B the tier boundary instead of guessing. Run `ab-sonnet-vs-opus-assessment` (workflow): the
*same* code assessment by an Executing arm vs a Frontier arm over identical files, then a delta
synthesis (`premium_justified: yes|no|marginal`). Result feeds whether "code assessment" sits at
Executing (expected) or warrants Frontier. Re-run per task class; record the verdict here.

> Status: A/B `wjh0kr38l` in flight — verdict + recommendation to be appended on completion.
