# Judgment Layer — Design Memo

Status: implemented as the v1 boundary judgment + recommendation layer.
`rally next` ranks candidate work; `rally judge` and `rally hook <phase>` provide
the closed loop. Deeper weight tuning remains future work.

## Problem

Rally today is a passive bulletin board. Typed events, identity, hash-chained log, sync-able. But no entity reasons over the facts. `rally context` / `rally packet` return heuristic toys ("no pending handoffs → proceed_solo") not judgment.

The product goal is N agents behaving like a coordinated team without a human routing work. That decomposes into:

1. **State awareness** — every agent knows what's happened. ✅ done.
2. **Signal of contribution** — agents post what they did. ✅ done.
3. **Judgment about what to do next** — given (1), what's the highest-leverage thing *this specific agent* should do right now? ❌ **missing.**

Earlier design work on routing engines, auto-handoffs, and a `rally done` verb was compensating for the missing judgment layer. If judgment exists, much of the explicit-signaling problem evaporates — the next agent's recommendation engine notices "this artifact is stale, the producer is idle, you're the right next step" instead of waiting for an explicit handoff event.

## What "judgment" means concretely

Each agent's `/rally` should answer one question: **"What is the most useful thing I can do right now?"**

Output is a single ranked recommendation with reasoning + source events:

> "Pick up review of claude's auth refactor (artifact `art_abc`). Ready 12 min, no reviewer claimed it, blocks tasks T3 and T7, you're the only declared reviewer. Diff: 47 lines across 3 files."

Not `proceed_solo` / `join_active`. Specific, scored, justified.

## What this dissolves

- **`rally done` verb debate**: agents don't need to fire explicit completion if the next agent's judgment infers readiness from the absence of follow-up activity.
- **Routing engine**: routing falls out of the recommendation. If pi's `/rally` says "review claude's artifact", routing happened.
- **"Speculative" event kinds**: lesson, decision, artifact, subscription, profile are not speculative — they're under-utilized *inputs to the judgment engine*. The kinds were correct; the consumer was missing.

## Minimum viable shape

New command:

```bash
rally next --tool <you> --json
```

Returns: one ranked recommendation. Fields:

- `action_kind` (e.g., `review_artifact`, `pick_up_task`, `unblock_peer`, `progress_north_star`)
- `target_event_id`
- `subject` (one-line natural language)
- `reasoning` (which factors fired)
- `source_event_ids[]`
- `score` (float, for debugging)
- `factors{}` (per-factor scores)
- `alternatives[]` (top 2-3 not chosen, for transparency)

Agent's `/rally` calls this in addition to (or instead of) raw inbox dump.

## Scoring function (strawman)

Pure function over the log + agent's declared state.

```
score(candidate_action, log, agent_state) =
    w_leverage   * downstream_unblock_count(candidate)
  + w_freshness  * inverse_age_of_blocked_artifact(candidate)
  + w_match      * role_capability_match(candidate, agent_state)
  + w_load       * (1 - peer_currently_busy_for_candidate(candidate))
  + w_intent     * alignment_with_north_star(candidate)
  - w_dup        * has_other_peer_already_claimed(candidate)
  - w_stale      * age_beyond_relevance_threshold(candidate)
```

Tuned over real session traces. Defaults reasonable; configurable via `~/.agent-rally-point/judgment.toml`.

## Inputs the function needs

From the log:
- Open vs closed tasks (with `depends_on` chain → critical-path computation)
- Artifacts and their `ref_task` linkage
- Subscriptions (`--path`, `--event-kind`) per peer
- Profile declarations (`--role`, `--capability`)
- Recent presence events (peer active vs idle)
- Open handoffs and their `to` field
- Open claims (file-level conflicts)
- Open blockers
- Decisions (binding vs proposed)
- Lessons (de-prioritize approaches marked as anti-pattern)

From local agent state:
- This agent's `tool` id
- This agent's declared role/capabilities (from latest profile event)
- This agent's open obligations (handoffs addressed to it, claims it owns)

From environment:
- `north_star.intent` / `goal` (from `.build-loop/intent.md` if present)
- Time-of-call (for age computations)

## Candidate-action generation

Before scoring, enumerate plausible actions. Sources:

1. **Pending handoffs addressed to me** → `pick_up_handoff`
2. **Open tasks I own** → `progress_owned_task`
3. **Open tasks unowned, matching my role/capabilities** → `claim_task`
4. **Artifacts with subscriptions matching me, no follow-up** → `review_artifact` / `consume_artifact`
5. **Blockers I can unblock** → `unblock_peer`
6. **Stale artifacts of idle peers** → `pull_orphan`
7. **North-star steps with no progress** → `progress_north_star`

Each candidate gets scored. Top one wins. Ties broken by recency.

## Failure mode

`rally next` returns no candidate → response is `{"action": "idle", "reason": "no actionable work"}` plus a structured list of *why nothing scored above threshold*. This is itself useful: "you're declared as a builder but no unowned build tasks exist, and no peer has produced an artifact subscribed to your watched paths."

`doctor` surfaces this as "agent X has no actionable work for >M minutes" — a real signal vs silence.

## Why this is the right move next

- It makes already-existing event kinds load-bearing instead of decorative.
- It dissolves the routing-engine debate; routing is a property of recommendations, not a separate system.
- It's a pure derivation — no daemon, no process, deterministic from the log.
- It's incrementally testable: a synthetic log + agent state → assert a specific recommendation. Trivial test surface.
- It gives the system a real measurable property: "did the recommended action match what a competent operator would have picked?" Tunable over time.

## Open questions before implementation

1. **Recommendation scope** — is it always one action, or top-N with the agent choosing? Lean: one. Multiple recommendations push judgment back onto the agent. Top-N as `alternatives[]` for transparency only.

2. **Weight tuning** — start with hand-picked weights; later, learn from "agent ignored the recommendation, did X instead" patterns. Don't build the learning loop yet, just record the dissent (`rally next --rejected` event when an agent picks something else).

3. **Idle peer detection** — presence events with > N minute heartbeat gap = idle. What's N? Probably 5 min. Configurable.

4. **Intent file ingestion** — `.build-loop/intent.md` exists as a convention. Parse it? Or require structured `north_star` events posted via `rally`? Lean: parse if present, supplemented by `decision` events.

5. **What about the receiver-pull problem** — does the judgment layer make `rally claim` from receiver-side first-class? An agent picks up via `rally next` → top recommendation is `claim_task` → agent fires `rally claim`. The pull *is* the routing.

6. **Versioning and stability** — `rally next` output schema needs to be stable for agents. Version it from day one (`schema: agent-rally.next.v1`).

## Implemented v1

- Candidate sources: pending handoffs, active blockers, owned tasks, unowned
  tasks, and recent artifacts.
- Scoring: deterministic fixed weights with role/capability bonuses.
- Output: one top recommendation plus top alternatives, source event ids,
  factors, score, and reasoning.
- Boundary enforcement: `rally judge` and `rally hook` stop unsafe work and can
  auto-claim before writes.

## Concrete next steps (suggested)

1. Specify the candidate-action enumerator: small function per kind, returns candidates from the log.
2. Specify the scoring weights file format and defaults.
3. Specify the `rally next` JSON schema (versioned).
4. Build one candidate-source end-to-end (start with `pending_handoff` since that's the easiest, already in preflight) so the scaffolding lands runnable.
5. Add candidates incrementally: `claim_task` next, then `review_artifact`, then the rest.
6. Wire `/rally` skills to call `rally next` and surface the recommendation as the primary brief.

Spike-before-plan: build candidate #1 + scoring + `rally next --json` as a 1-day spike before designing further. The shape of the scoring will become obvious once one end-to-end recommendation flows.

## Sister discussions

- `RALLY.md` — the 4-command loop docs. Will need updating once `rally next` lands.
- `docs/ATTUNED_COORDINATION.md` — earlier attempt at this; revisit and align.
- `docs/CONTEXT_BRIEF_SCHEMA.md` — `rally next` output should evolve this schema, not parallel it.
