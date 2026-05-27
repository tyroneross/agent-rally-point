# Review of `rally next` (commit `88ee57a`)

Reviewer: claude_code · Target: pi · Status: ship + sharpen

## Verdict

Solid spike. The five candidate sources from the design memo are all wired, scoring is monotonic enough to be useful, schema is versioned, and there's a test that proves the ordering invariant. Genuinely ships the missing piece — there's now a recommendation engine, not just a bulletin board.

The depth items from the memo are explicitly deferred — right call for a 1-day spike. Concerns below are about what to harden next, not blockers.

## What landed

- `rally next --tool <you> [--limit N] --json` returning top action + up to 3 alternatives
- Candidate sources: `pick_up_handoff` (100), `unblock_peer` (90/70), `progress_owned_task` (80), `claim_task` (55+bonus), `review_artifact`/`consume_artifact` (35+bonus)
- Role-aware bonus (reviewer→artifact +25, builder→task +15, architect→task +10)
- Schema `agent-rally.command.next.v1` committed
- Idle fallback with explicit reasoning
- Tests: scoring ordering (verify.rs:389) + schema contract (golden_contracts.rs:417)

## What's strong

1. **Pure derivation.** No daemon. `next_recommendation(projection, tool, limit)` is one pure function. (query_commands.rs:1018)
2. **All five candidate sources from the memo present.**
3. **Ordering invariant pinned by test.** Handoff beats owned task; owned task surfaces as alternative.
4. **`source_event_ids[]` per candidate.** Auditable.
5. **Idle path returns reasoning, not silence.** "no actionable work" with a reason.
6. **Schema versioned + golden-contract tested from day one.**

## Deferred (per memo, fine for v1)

- Critical-path / dependency analysis (downstream_unblock_count)
- Freshness scoring (artifact age weighting)
- Idle-peer factor (peer_currently_busy)
- North-star alignment (`.build-loop/intent.md`)
- Dissent recording (`rally next --rejected`)
- `--explain` mode
- Configurable weights (`judgment.toml`)

## Concrete concerns (small, sharpen before adding more candidates)

1. **Magic scores inline.** `100/90/80/70/55/35` + bonuses `25/15/10`, no constants, no rationale. Promote to `const HANDOFF_SCORE: f64 = 100.0;` etc with a band comment (100=obligation, 80-90=strong pull, ≤55=opportunistic). Becomes a tuning table later. (query_commands.rs:1035, 1044, 1064, 1080, 1097)

2. **`role_match_bonus` uses substring match on role.** `role.contains("review")` would match "preview-engineer". Should be exact match against a known set, or token-split. Fragile as roles proliferate. (query_commands.rs:1158-1164)

3. **Artifact candidates skip "has follow-up?"** Every recent artifact scores 35+bonus regardless of whether it's been consumed/reviewed. Design memo #4 said *"Artifacts with subscriptions matching me, no follow-up"*. Current code skips both:
   - Doesn't verify subscriber path overlap
   - Doesn't check for downstream causal events
   
   Result: a reviewed artifact stays in the pool forever; same recommendation fires until something newer outranks it. (query_commands.rs:1087-1102)

4. **Reasoning is fixed per source type.** Same string for every handoff candidate, same string for every blocker, etc. Doesn't tell the agent *why this one* beat the alternatives. `factors{}` carries the score breakdown but `reasoning[]` is the human-readable surface and it's currently uninformative.

5. **No de-duplication across sources.** Task T owned-by-me + has an artifact → can appear twice with different `action_kind`s. `alternatives[]` may repeat target_event_id. Worth a dedup pass keeping highest-scoring action per target.

6. **`limit.max(3)` only caps artifacts.** Other candidate sources enumerate uncapped. O(N) on channel size. Bound the loops or document the volume assumption.

7. **`capabilities.clone()` is unnecessary** — only read. Pass by reference. (query_commands.rs:1026)

8. **`role_match_bonus` called twice per `claim_task` candidate** — once in score, once in factors map. Bind once. (query_commands.rs:1075-1081)

## Test gaps

Single ordering test is great but thin. Add:

- **Blocker > owned task** when both exist
- **Role flips action_kind** — same artifact, role=reviewer → `review_artifact`; role=builder → `consume_artifact`
- **Idle case** — empty channel returns `{action_kind: "idle", reasoning: [...]}`
- **Unowned task role bonus** — builder gets +15 on unowned task

Each ~10 LOC against the existing `RallyWorkspace` harness.

## Skill / docs alignment

`rally next` actually exists now — `RALLY.md` and SKILL.md should advertise it as the "decide what now" command (replacing the misaligned `rally judge --phase idle` framing from the earlier commit). Worth verifying agents are taught to call `rally next` at idle boundaries instead of just `rally inbox`.

## Suggested next steps (ordered by leverage)

1. **De-dup candidates across sources** (#5) — small, prevents weird repeats in `alternatives[]`.
2. **Artifact follow-up filter** (#3) — meaningful; without it recommendations stale fast.
3. **Score constants + rationale comment** (#1) — sets up future tuning.
4. **Test coverage gaps** above — pin the ordering invariants you want.
5. **`--explain` mode** — derive from existing `factors{}` and `reasoning[]`; mostly formatting.

After that, the deferred memo items (freshness, critical-path, peer-load, dissent) become incremental adds against a known-good scaffold.

## Note on pi's current WIP

Pi has uncommitted changes adding `AdapterInstall.backup_paths` + `--dry-run` to `setup install` — compile-broken at the moment (`backup_paths` field added to struct but not populated at construction sites). Not part of this review; mid-flight on a follow-up. WIP left alone.
