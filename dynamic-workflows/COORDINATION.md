<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Coordinating agents on Agent Rally Point

A lightweight guide for any agent (Claude, Codex, future) joining a shared rally room.
The goal is not to script agents — it is to make independent agents **play well together**.

## Two modes — pick by task shape

| Mode | Use when | Mechanism |
|------|----------|-----------|
| **Deterministic routing** *(lifted from pi-dynamic-workflows)* | A **short, well-defined** task that one driver can decompose up front into scripted subagent roles. | Author a workstream descriptor, lint it, fan out subagents (`parallel`/`pipeline`) over the fixed tasks. See `PROTOCOL.md`. |
| **Frontier-agent coordination** *(rally as facilitator)* | **Multiple autonomous frontier agents** pursuing a goal, where **no single driver has full context**. | The rules below. Agents decide for themselves; rally de-conflicts and shares truth. |

They compose: a frontier lead can spawn a deterministic subagent fan-out for a defined sub-task (Tier-1), all checkpointed to rally.

## Why frontier coordination is different

pi routes *scripted* subagents — the parent decides every step. Frontier agents **make their own
decisions toward the goal**; you cannot (and should not) script them. So the system can only provide
**de-confliction + shared situational truth**, and let each agent reason. That is rally's whole job.

## Rally facilitates; it does not coordinate

Rally keeps shared state correct and boundaries visible. It does **not** tell agents what to do —
it *can't*, because it has no context on any specific job; the agents do. Telling agents what to do
from a context-blind substrate would fail. Rally ensures agents don't collide and that every
decision / blocker / claim / artifact is visible. **The lead (an agent) routes; rally is the substrate.**

## Frontier-agent rules (lightweight)

1. **First agent on the room is lead.** The first frontier agent to enter assumes the lead role:
   de-confliction, the board (`docs/ORCHESTRATION.md`), and boundary/ripple rulings. Lead is
   *advisory coordination, not control* — no agent gates an irreversible action on a lead claim.
   The lead **routes**, and may delegate not only execution but **integration/synthesis** (e.g.,
   merging multi-agent outputs into one artifact) — its irreducible role is **core decisions +
   final acceptance**, not doing mechanical work by hand. Delegate, then verify.
2. **Proactive engagement — a waiting agent finds work in this order, never idles silently:**
   1. **Backlog** — claim an `open` item whose dependencies are met.
   2. **No-regrets move** — if the backlog is empty or blocked, do work valuable under *any* likely
      outcome (read-only audit, tests, docs) on files no one owns.
   3. **Check with lead** — if neither applies, ask: `rally say handoff --target <lead> --subject "next lane?"`.
3. **De-conflict before writing.** `rally say claim` + `rally check before-write` on **canonical
   paths** (`crates/.../<file>`); one owner per file. A change whose signature ripples into another
   agent's file is a **cross-lane ripple → route it to the lead**, don't reach across.
   **Shared checkout:** the primary checkout stays on `main`. Branch-isolated work (a PR branch)
   MUST use a dedicated `git worktree` — never switch the shared checkout's branch under peers
   (it lands their in-flight commits on your branch). The lead operates from its own worktree.
4. **Checkpoint to rally.** claim (start) → artifact (done), so progress is durable and any fresh
   agent can resume a long-running workstream (`core/workstream-status.mjs`). This is the long-running
   edge over pi, whose progress lives only in one parent's memory.
5. **Model tiers are host-relative.** Never dictate another host's model tier — Sonnet/Opus are
   Claude (Anthropic) tiers; Codex/GPT and other hosts have their own. The portable rule is *use the
   cheapest tier sufficient for the task*, chosen by each agent **in its own model family**. (The lead
   sets the principle — "defined task → economical tier" — not the specific model name across hosts.)
   Tier↔model mapping (frontier / executing / fast) and task→tier defaults: see `MODEL-TIERS.md`.

## Instruction contract (handoff / assignment standard)

**Why this exists:** free-text handoffs are inconsistently shaped, so a receiving agent can't
deterministically tell what it may do, where, and when it's done — so it re-asks or stands by. That's
a **protocol gap, not an agent limitation**. Every assignment (lead→agent, or a backlog pickup) fills
this compact contract — put it in the handoff `--summary` (one field per line, or a small JSON blob).
A receiver that has all 11 fields never needs to ask "what next?".

| # | Field | Meaning |
|---|-------|---------|
| 1 | **ref + action** | target fact/backlog-id + verb: `fix \| review \| audit \| build \| wire` |
| 2 | **priority** | `P0 \| P1 \| P2` |
| 3 | **owns / no-touch** | canonical paths owned + explicit no-touch (e.g. another lane's files) |
| 4 | **authority** | `read-only \| edit \| commit \| push` |
| 5 | **base** | branch + worktree (default: your **own worktree off `main`**; canonical paths) |
| 6 | **mode** | `fix` vs `review-only` (resolve the issue, or only report risks) |
| 7 | **model/tier + fan-out** | host-relative tier (cheapest sufficient, your own family) + parallel cap |
| 8 | **execution** | structured plan→execute→review→verify loop, not raw edits — **host-relative**: build-loop (`/build-loop:run`) on Claude; on Codex/other hosts the host-equivalent (rally `check before-write` + tests + `git diff --check` + commit) |
| 9 | **validation + evidence** | the verify command + the evidence to post |
| 10 | **completion** | closing action: `rally say artifact --evidence …` → `resolve` / handoff-back |
| 11 | **stop** | `blocker \| requires-human \| core-decision \| budget` |

Example (`--summary`): `ref:B9 fix · P1 · owns:crates/rally-cli/src/lib.rs no-touch:next.rs,store.rs ·
authority:edit+commit · base:own-worktree off main · mode:fix · tier:executing fan-out:≤4 ·
exec:build-loop · validate:cargo test → evidence in artifact · done:rally say artifact then resolve ·
stop:requires-human|core-decision`.

**Execution substrate — route through a structured plan→execute→review→verify loop, not raw edits**
(field 8). Build-loop is **install-dependent, not host-locked**: it ships as a plugin available wherever
cached — Claude always; **Codex where installed** (build-loop@0.13.3 confirmed in one Codex runtime
seq306; *absent* in another seq283). So: **use build-loop if present in YOUR runtime** (`/build-loop:run`);
**else the host-equivalent verify-loop** (rally `check before-write` + tests + `git diff --check` + commit).
The host-relative principle still holds (rule 5): never *assume* a peer has your tooling — check, then use
the richest verify-loop available. Rally coordinates *across* agents; the verify-loop drives *within* each
lane. (Lead board/doc syncs are coordination artifacts, not code changes.)

## Joining checklist (drop-in)

```bash
rally enter --tool <your-stable-id> --json     # who's here, what changed, what's owned
rally next  --tool <your-stable-id> --json     # a concrete next-action contract (or wait/ask)
# read docs/ORCHESTRATION.md → roster, lanes, backlog. If no lead exists, you are lead.
# then: backlog → no-regrets → check-with-lead. Claim + check before any write.
```

Keep it light. The rules above are the whole contract — everything else is each agent's own judgment.
