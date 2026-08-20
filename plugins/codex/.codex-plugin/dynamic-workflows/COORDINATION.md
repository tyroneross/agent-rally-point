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

## Squads (agent teams = terminals)

A **squad** is one terminal. **N terminals = N squads** (1:1). A squad may be a *single agent* (e.g. one
GPT-5.5 or one Opus session) **or** a *multi-agent team* — for example a Rally Flow Tier-1 fan-out, a
LangGraph DAG, or a plugin-orchestrated subagent group like build-loop. Either way it is **one squad**.
The unit rally
coordinates is the **squad**, not the individual sub-agent — a squad's internal agents are its own
Tier-1 business, **hidden behind the squad id** (per the in-memory-subagents-stay-host-side boundary
above). So: 10 terminals → 10 squad ids in the room, regardless of how many agents run inside each.

- **Distinct, self-declaring squad id, enforced at entry.** Every squad enters under **one** stable,
  distinct id in the form **`host-llm-role-number`** (e.g. `codex-gpt-5.5-builder-01`,
  `claude-opus-4.8-lead-01`). The **model lives in the id**, so the lead never *infers* a peer's model
  — it reads it. Rally MUST reject/flag a duplicate id so two squads never collide (the bare-`codex` /
  bare-`claude_code` collisions this session are the failure mode). No squad uses a bare host id or two
  ids. *(Legacy `<host>:<squad>` ids — `claude_code:lead`, `codex:dynwf-coordinator` — migrate to the
  new form **on next `enter`**, never by force-rename: renaming a live squad orphans its in-flight
  claims/handoffs.)*
- **Model tier per squad.** Each squad's **tier** (**frontier | executing | fast**, per `MODEL-TIERS.md`)
  is the stable routing abstraction; the **specific model** is the mutable `llm` token *in the id*. A
  model upgrade (`gpt-5.5`→`gpt-5.6`) = **re-enter under the new id + release old claims** (a different
  model is a different capability). Because the model is in the id, B11 need only enforce id-uniqueness +
  tier + lead-assert — not separate model-registration.
- Rally-side enforcement (enter rejects duplicate squad id + records tier) is backlog **B11**; until it
  lands the **lead enforces the convention** and maintains the squad roster on the board.

## Frontier-agent rules (lightweight)

1. **First agent on the room is lead.** The first frontier agent to enter assumes the lead role:
   de-confliction, the board (`docs/ORCHESTRATION.md`), and boundary/ripple rulings. Lead is
   *advisory coordination, not control* — no agent gates an irreversible action on a lead claim.
   The lead **routes**, and may delegate not only execution but **integration/synthesis** (e.g.,
   merging multi-agent outputs into one artifact) — its irreducible role is **core decisions +
   final acceptance**, not doing mechanical work by hand. Delegate, then verify.
   - **Title lifecycle** (L-1 landed; `rally lead` surface per [`../docs/SPEC-lead-agent.md`](../docs/SPEC-lead-agent.md)).
     Lead auto-assign is **frontier-only** (`rally enter --tier frontier`); a declared `executing`/`fast`
     first-enter leaves the seat open until a frontier agent joins. The lead may **hand off** the title
     to another frontier agent or **relinquish** it; a **user-designated** lead supersedes a first-join lead.
   - **Decide from goals + app knowledge; when uncertain, resolve from the mission.** The lead makes
     tradeoffs and decisions from the work's goal and its knowledge of the app. When information is
     insufficient, it resolves from the **ultimate intent / outcome of the work** — the queryable
     `rally mission` north-star + per-agent autonomy envelopes. *Resolve from the mission; don't stall.*
     (Charter: rally **records + exposes** the lead and the mission; it never **enforces** — lead
     authority is doctrine the agents honor, not a CLI gate.)
2. **Proactive engagement — a waiting agent finds work in this order, never idles silently:**
   1. **Backlog** — claim an `open` item whose dependencies are met.
   2. **No-regrets move** — if the backlog is empty or blocked, do work valuable under *any* likely
      outcome (read-only audit, tests, docs) on files no one owns.
   3. **Check with lead** — if neither applies, ask: `rally say handoff --target <lead> --subject "next lane?"`.
3. **De-conflict before writing.** `rally say claim` + `rally check before-write` on **canonical
   paths** (`crates/.../<file>`); one owner per file. A change whose signature ripples into another
   agent's file is a **cross-lane ripple → route it to the lead**, don't reach across.
   **One canonical clone on `main`** (the default): all squads — including the lead — work the single
   repo clone on `main`. Reach for a dedicated **ephemeral** `git worktree` ONLY for genuinely
   branch-isolated work, and **collapse it back to `main` immediately at close** — do not leave standing
   per-agent worktrees/branches (the `arp-lead`/`arp-claude2` split caused a wrong-branch commit and a
   ledger-commit gap; retired 2026-05-30). Never switch the shared checkout's branch under peers (it
   lands their in-flight commits on your branch). Keep the canonical `.rally/` ledger in this one clone
   so coordination travels with commits.
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
| 4 | **authority** | `read-only \| edit \| commit \| merge \| push` — **commit + merge are delegated to frontier agents**; the lead **audits post-hoc** (source-verify + test-evidence) and reverts only on a failed audit, rather than pre-gating. Each merge posts a durable Rally artifact with evidence. |
| 5 | **base** | branch + worktree (default: your **own worktree off `main`**; canonical paths) |
| 6 | **mode** | `fix` vs `review-only` (resolve the issue, or only report risks) |
| 7 | **model/tier + fan-out** | host-relative tier (cheapest sufficient, your own family) + parallel cap |
| 8 | **execution** | structured plan→execute→review→verify loop, not raw edits — **host-relative**: use whichever loop your host provides. The portable, rally-required minimum is rally `check before-write` + tests + `git diff --check` + commit. Plugin orchestrators (build-loop on Claude, ChatGPT optimizers on Codex, host-native Workflow tools, LangGraph) are valid *richer* substitutes; none is required. |
| 9 | **validation + evidence** | the verify command + the evidence to post |
| 10 | **completion** | closing action: `rally say artifact --evidence …` → `resolve` / handoff-back |
| 11 | **stop** | `blocker \| requires-human \| core-decision \| budget` |

Example (`--summary`): `ref:B9 fix · P1 · owns:crates/rally-cli/src/lib.rs no-touch:next.rs,store.rs ·
authority:edit+commit · base:own-worktree off main · mode:fix · tier:executing fan-out:resolved(6/10,host) ·
exec:verify-loop · validate:cargo test → evidence in artifact · done:rally say artifact then resolve ·
stop:requires-human|core-decision`. (The `exec:` token names *whichever* plan→execute→review→verify
loop the squad uses — e.g. `exec:verify-loop` for the rally-minimum, `exec:build-loop` if that plugin
is the chosen substrate. Rally does not care which.)

**Execution substrate — route through a structured plan→execute→review→verify loop, not raw edits**
(field 8). Rally's portable, required minimum *per lane* is: rally `check before-write` → run tests →
`git diff --check` → commit → post evidence. Anything that does at least that satisfies the contract.
Plugin orchestrators are richer substitutes — build-loop (`/build-loop:run`) on Claude, ChatGPT
optimizers on Codex, host-native Workflow tools, LangGraph DAGs — and squads should use the richest
loop their runtime offers, but none is required and none is privileged. The host-relative principle
(rule 5) still holds: never *assume* a peer has your tooling; check, then use whatever satisfies the
minimum. Rally coordinates *across* agents; the verify-loop drives *within* each lane. (Lead
board/doc syncs are coordination artifacts, not code changes.)

## Joining checklist (drop-in)

```bash
rally whoami  --tool <your-stable-id> --json   # SELF-LOCATE FIRST: host runtime, room, lead, mission, ack — if host_runtime.ambiguous, STOP & resolve
rally enter   --tool <your-stable-id> --json   # who's here, what changed, what's owned
rally ack     --tool <your-stable-id>          # confirm you ingested rules/guardrails/lead/mission (coordination-mandate)
rally mission --json                           # the room's north-star + your autonomy envelope — read before acting
rally next    --tool <your-stable-id> --json   # a concrete next-action contract (or wait/ask)
# read docs/ORCHESTRATION.md → roster, lanes, backlog. If no lead exists, you are lead.
# then: backlog → no-regrets → check-with-lead. Claim + check before any write.
```

Every `--json` result follows one envelope: `{ ok, command, product, schema, data }`, and the
command's result is always at `data[<command>]` (e.g. `data.room`, `data.next`, `data["wake-due"]`).
Parse by that rule — don't guess per-command nesting. Full map: [`../docs/JSON_ENVELOPE.md`](../docs/JSON_ENVELOPE.md).

Keep it light. The rules above are the whole contract — everything else is each agent's own judgment.
