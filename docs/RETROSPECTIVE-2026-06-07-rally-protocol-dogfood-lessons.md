<!--
SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Retrospective - Rally protocol dogfood, handoff proof, and memory discipline

_Date: 2026-06-06 local / 2026-06-07 UTC_
_Repo: `agent-rally-point`_
_Intent: Capture reusable lessons from the terminal-chat/Rally dogfood sequence so future agents can prevent the same coordination, validation, and memory failures._
_Source evidence: terminal chat, Rally facts, git state, current repo docs, build-loop memory conventions._

## Current Answer

The session produced working protocol improvements, but the main durable lesson is operational:
**Rally facts, git state, terminal panes, and build-loop memory are different truth surfaces and must be reconciled before claiming work is done.**

The failure pattern was not a single code bug. It was a cluster:

- A targeted handoff projection bug let non-target commentary make a target handoff disappear.
- A validated source fix was released before it was committed, leaving the main checkout dirty.
- Managed injection could report "delivered" while the target had not ACKed or acted.
- A stale host hook still produced `stop` hook failures (`exit 127`) outside the source-current binary path.
- Build-loop memory was not used early enough in the implementation loop, so known Rally lessons were re-derived.

The closeout state improved after user correction:

- `de6c20b` landed `fix(rally): require target-authored handoff close`.
- `6e18262` landed `fix(rally): require verified handoff inject ack`.
- `6a6584b` landed `docs(rally): clarify session liveness proposal`.
- Rally room later showed `open_handoffs=0`, `active_blockers=0`; only this retrospective doc claim was active while writing.
- The repo-local `target/debug/rally` still reported build `0.1.0+de6c20b` while source `HEAD` was `6a6584b`; source-current behavior must be rebuilt or called out before relying on it for new source changes.

## Timeline

1. **Protocol work started with explicit north-star constraints.**
   The project goal was session identity, transactional claim authority, structured scopes, claim leases, causal/auth event envelope, and targeted Claude/Codex handoff proof. The governing split was clear: Rally facilitates; hosts execute.

2. **Session identity and event envelope work landed through Claude-owned lanes.**
   The Rally facts record session identity in `whoami`, durable `from_session_id`, event envelope advisory validation, and closeout docs/tests.

3. **Codex claim authority and injection reliability work landed on main.**
   The room evidence records claim authority and injection reliability merges, later followed by a product fix for verified handoff inject ACK.

4. **A targeted handoff projection bug was found after live room state disagreed with the intended protocol.**
   Non-target session-era `artifact`/`resolve`/`receipt` references could close a targeted handoff. The fix made targeted handoff closure target-authored, rejected wrong-tool targeted resolves before append, and preserved legacy no-session replay behavior.

5. **Validation artifact was mistaken for completion until the user pointed out live git state.**
   The fix passed `fmt`, `clippy`, `cargo test`, and `git diff --check`, and the claim was released. But `store.rs` and `user_journey.rs` remained modified on main and `HEAD` had not advanced. The user forced the correct binary choice: commit the fix or explicitly hand off dirty files.

6. **The fix was committed and Rally-resolved.**
   Commit `de6c20b` landed, source files were clean, and Rally fact `fact_10fa0` was resolved with commit evidence.

7. **Lane 2 was later resolved, and closeout moved to operator-only decisions.**
   Rally artifacts later recorded `open_handoffs=0`, final validation green, and no source dirt beyond local Rally/archive/proposal artifacts.

8. **A host-side stop hook remained stale.**
   The installed Codex hook wrapper still used a stale path/surface and could fail with `exit 127`. This was correctly tracked as host-hook hygiene, not the same source lane as handoff projection.

## Lessons Learned

### L1 - Validation evidence is not a landing event

Passing validation proves the candidate change works in the current checkout. It does not prove the change is landed, shareable, or safe for another agent to build on.

Required closeout sequence for a mutating lane:

1. Run the declared validation.
2. Check `git status --short`.
3. Commit the validated files or explicitly hand them off.
4. Re-check `git status --short`.
5. Post Rally artifact with commit hash or handoff fact.
6. Only then release the claim.

The user correction on the dirty `store.rs` and `user_journey.rs` state was the right intervention. Future agents should not need that intervention.

### L2 - A Rally claim release is not proof the file is clean

Claim release is coordination state. Git cleanliness is source state. They must both be true before a lane is done.

Failure mode:

- Agent validates.
- Agent releases claim.
- Another agent sees no active claim and assumes the file is available.
- The source checkout still contains uncommitted changes.

Preventive rule: every release after edits must cite either a commit hash or an explicit handoff event for the dirty files.

### L3 - Delivered is not ACKed, submitted, accepted, or resolved

The dogfood repeatedly exposed this distinction:

- `rally inject` can write text or a doorbell into a pane.
- The target terminal can display staged text without submitting it.
- A transport status of `delivered` is not target-authored protocol evidence.
- A real ACK/resolve must cite the exact handoff/ref and originate from the target session/tool.

The fixed product direction is correct: inject success may report delivery, but it must expose `ack_state` separately and treat timeout as "not received/verified" for action purposes.

### L4 - Targeted handoffs need target-authored closure

A targeted handoff should leave the target's actionable queue only when one of these happens:

- The target session/tool posts an ACK/resolve/receipt/artifact/blocker/decision that references the handoff.
- A legacy no-session closer is replayed under the compatibility rule.
- A supersede/cancel path explicitly closes it under an auditable authority rule.

Non-target commentary that merely references the handoff is context, not closure.

### L5 - Compatibility belongs on the replay boundary, not as a loophole for new writes

Old ledgers without `from_session_id` must still replay. That does not mean new session-era writes can keep legacy semantics.

The useful split:

- Old closer event without session identity: preserve previous projection behavior.
- New closer event with session identity: enforce target-authored closure for targeted handoffs.
- Wrong-tool targeted resolve: reject before append.

This pattern should be reused for future envelope migrations.

### L6 - Source-current and behavior-current can diverge

This session had multiple binary truth surfaces:

- PATH `rally` was known stale.
- Repo-local `target/debug/rally` was required for current Rally behavior.
- Later, source `HEAD` advanced to `6a6584b` while `target/debug/rally whoami` still reported build `0.1.0+de6c20b`.

Rule: for source-current behavior, either rebuild the local binary after source movement or explicitly say which build the binary represents. Do not infer current source behavior from an older built binary.

### L7 - Hook failures and source bugs can be adjacent but not identical

The `stop` hook issue (`hook exited with code 127`) belongs to host-hook hygiene unless proven otherwise. It can coexist with Rally source defects, but it should not be fixed by blindly changing source code.

Efficient diagnosis:

1. Identify which hook actually ran.
2. Reproduce under the hook's minimal PATH.
3. Check whether it calls PATH `rally`, repo-local `target/debug/rally`, or a stale command surface.
4. Classify as host wrapper/config, plugin cache, or Rally source.
5. Only then edit the owning layer.

### L8 - Rally is source of truth for coordination, not for repository cleanliness

Rally knows claims, handoffs, ACKs, blockers, artifacts, decisions, risks, sessions, and room projections. Git knows source state. tmux knows whether text is actually staged in a pane. Build-loop memory knows reusable lessons.

None replaces the others:

- Rally artifact without git commit: not landed.
- Git commit without Rally artifact: landed but not coordinated.
- Inject delivery without target-authored Rally ACK: not received for protocol purposes.
- Terminal diagnosis without memory lookup: likely to re-derive known incidents.

### L9 - Build-loop memory should have been consulted earlier

I did not use build-loop memory enough at the start of the implementation/dogfood loop. I used Rally live state, repo docs, and current terminal evidence, but the reusable memory layer came in only after the user requested this retrospective.

Why that happened:

1. The task was framed as Rally protocol work, and I treated Rally as the only durable coordination source.
2. I incorrectly let "Rally is source of truth" expand from coordination truth into process-memory truth.
3. The active workflow did not have a mandatory memory-read gate before debugging known Rally failure classes.
4. The live room had rich facts, so it felt sufficient even though it lacked cross-session lessons.

The correction:

- Use Rally for current coordination truth.
- Use build-loop memory for reusable prior lessons.
- Use git for source landing truth.
- Use terminal/backend inspection for transport truth.

### L10 - Build-loop memory writes need a promotion gate

Not every terminal fact deserves memory. A memory entry should be written when a lesson is reusable, evidence-backed, and likely to prevent repeated cost.

This session qualifies because it produced repeatable rules:

- Commit or hand off after validation.
- Never release an edited-file claim without commit/handoff evidence.
- Treat delivered as separate from ACKed.
- Use target-authored closure for targeted handoffs.
- Rebuild or disclose binary/source drift.
- Read build-loop memory before debugging a repeated Rally failure class.

## New Efficiencies

### E1 - Use Rally event ids as the handoff chain

Rally event ids made it possible to reason about this session without trusting memory or prose summaries:

- `fact_1079d`: final validation of handoff projection fix.
- `fact_10fa0`: user/supervisor correction that validation had not landed.
- `fact_1201e`: supervisor accepted the committed fix.
- `fact_15322`: verified handoff inject ACK product fix.
- `fact_13329`: supervisor closeout state.

Future retrospectives should start from event ids, then check git and source.

### E2 - Treat "status" questions as live-state questions

When the user asks "status" or "how will you know when that happened", the answer should be based on fresh checks:

- `git rev-parse --short HEAD`
- `git status --short`
- `target/debug/rally room --json`
- `target/debug/rally next --tool <target> --json`
- relevant file diffs or absence of diffs

Do not answer from a prior summary unless explicitly saying it may be stale.

### E3 - Separate claim authority from claim narrative

A claim fact says who intends to own a resource. Enforcement depends on the active claim index and before-write checks. Closeout depends on source state plus Rally artifact evidence.

This distinction avoids both false confidence and overcentralization. Rally facilitates the guardrail; agents still do the verification.

### E4 - Use short mailbox/doorbell patterns for injection

Long prompt injection into a TUI is fragile. The more reliable direction is:

- Durable handoff in Rally.
- Short injected doorbell containing the exact event id.
- Target runs `rally next` / `rally ack` / `rally resolve`.
- Initiator waits for target-authored evidence.

This minimizes paste-buffer and composer-state risk.

### E5 - Keep host-hook workstream separate from source workstream

The stale Codex stop hook is a host integration problem. It should have its own claim, audit, minimal-PATH repro, and rollout approval. It should not be folded silently into Rally source commits.

## Issues With Causal Trees

### I1 - Validated fix was released while source files remained dirty

**Observed:** `store.rs` and `user_journey.rs` were modified after validation and claim release.

**Why chain:**

1. Why was the user correction needed? The final artifact emphasized validation, not landing.
2. Why did validation substitute for landing? The local lane lacked an enforced "commit or handoff" closeout gate.
3. Why did release happen first? Rally release did not require git cleanliness or a commit/handoff reference.
4. Why was this dangerous? Other agents could see no claim and edit over dirty local changes.
5. Missing system control: release-after-edit should require commit hash or explicit handoff evidence.

### I2 - Delivered injection was initially too easy to mistake for target receipt

**Observed:** A wake could be recorded as delivered while the Codex pane still held unsubmitted text and no target ACK/resolve existed.

**Why chain:**

1. Why was "delivered" misleading? It described transport action, not agent action.
2. Why was target action unproven? The target had not authored any Rally event referencing the handoff.
3. Why did this persist? Managed injection and handoff lifecycle projection were conflated.
4. Why did pane inspection matter? It showed the TUI composer state was the real blocker.
5. Missing system control: inject result must expose delivery and ACK as separate states, with timeout fallback.

### I3 - Stop hook `127` remained outside the source closeout

**Observed:** The user still saw `Stop hook (failed) error: hook exited with code 127`.

**Why chain:**

1. Why did it remain? The source lane fixed Rally behavior but did not update installed host hook wrappers.
2. Why not update the hook? Fleet-deploy/install changes require explicit approval and are outside source-current code.
3. Why is it easy to confuse? Both failures surface during Rally/Codex coordination.
4. Why is direct source editing risky? It can mask a PATH/wrapper problem with unrelated source churn.
5. Missing system control: host-hook audit and source fix need separate workstream labels and closeout criteria.

### I4 - Build-loop memory was consulted late

**Observed:** Prior Rally lessons existed, but this terminal sequence relied primarily on live Rally facts and repo inspection until the user requested retrospective capture.

**Why chain:**

1. Why was memory late? The workflow was framed as Rally dogfood, not build-loop.
2. Why did that matter? Build-loop memory stores reusable lessons that Rally room state does not summarize as doctrine.
3. Why was the boundary misread? "Rally is source of truth" was over-applied beyond coordination.
4. Why did the agent not self-trigger memory? No explicit turn-start memory gate was applied for this project.
5. Missing system control: any non-trivial repo task should run a memory quick pass when a project has build-loop-memory entries.

## What Should Be Enforced

1. **Release-after-edit gate:** a release fact for edited files must cite a commit hash or a handoff event.
2. **Final answer gate:** before reporting done, run `git status --short` and state whether changes are committed, uncommitted, or handed off.
3. **Targeted handoff gate:** only target-authored session-era replies close targeted handoffs.
4. **Injection gate:** delivered transport is never ACK. Require target-authored evidence with exact `ref_event_id`/handoff id.
5. **Binary drift gate:** if using a built local CLI, record its build id and compare to source `HEAD` when source changed.
6. **Memory gate:** before debugging repeated Rally/coordination failures, query build-loop memory for the repo and relevant global lessons.
7. **Hook workstream gate:** host hook changes require explicit owner, minimal-PATH repro, and separate approval from Rally source changes.

## Reusable Checklists

### Status / "How will you know?" checklist

Use this whenever the user asks how to know another agent has completed a step:

1. Check Rally for target-authored event:
   `target/debug/rally room --json` and `target/debug/rally next --tool <target> --json`.
2. Check git:
   `git rev-parse --short HEAD`, `git status --short`, and `git show --stat --oneline -1`.
3. Check file cleanliness for named files:
   `git diff --name-only -- <paths>`.
4. If injection was involved, require ACK/resolve/artifact authored by the target, not just wake delivery.
5. If a binary behavior claim is involved, compare CLI build id to source `HEAD` or rebuild.

### Mutating lane closeout checklist

1. Claim exact files before editing.
2. Run `rally check before-write` for exact files.
3. Edit only claimed files.
4. Run focused validation, then full required validation if the blast radius is shared.
5. Run `git diff --check`.
6. Commit or hand off dirty files.
7. Post Rally artifact with validation and commit/handoff.
8. Release claim only after the artifact exists.

### Build-loop memory checklist

1. At task start, search project memory for the repo slug and failure keywords.
2. During debugging, search memory before forming a new root-cause tree for repeated classes.
3. At closeout, ask whether the run produced a reusable lesson, gotcha, pattern, or incident.
4. If yes and authorized, write through `build-loop/scripts/memory_writer.py` so provenance and indexes update.
5. Keep Rally facts as evidence, not as the only long-term lesson store.

## Build-Loop Memory Assessment

I had not been using build-loop memory sufficiently for this implementation sequence. The reason is not that memory was unavailable; it is that I relied too heavily on the live Rally room and current repo state.

The correct operating model is:

| Surface | Use for | Not enough for |
|---|---|---|
| Rally ledger | Current coordination, claims, handoffs, ACKs, artifacts | Cross-session doctrine and recurring lessons |
| Git | Landed source truth | Whether a peer received a handoff |
| tmux/backend | Transport and visible pane state | Protocol ACK or work acceptance |
| build-loop memory | Reusable lessons, incidents, efficiencies | Live claim authority |
| Codex memory | User preference and cross-thread recall | Repo-local source truth |

Improvement that should stick:

- Start every non-trivial `agent-rally-point` task with a memory quick pass for `agent-rally-point`, `Rally`, `handoff`, `inject`, `claim`, `hook`, and `build-loop memory`.
- Record lessons promptly when the lesson is reusable and evidence-backed.
- Do not wait for the user to request a retrospective after the same failure class appears twice.
- When memory was not used, explicitly state why and add a process fix in the retrospective.

## Prompt Repeats And Enforce Candidates

User prompts in this terminal sequence repeatedly asked for:

- Status and live state.
- The `stop hook` issue.
- Continuation after a stalled/dirty state.
- Summary of work done and next step.
- How to know when another Codex terminal has actually completed the missing step.
- A comprehensive retrospective and memory capture.

Enforce candidates:

- "Status" must mean live git + Rally + relevant transport checks.
- "Done" must mean committed or explicitly handed off, not merely validated.
- "Continue" after a coordination issue must first reconcile Rally state and git state.
- Retrospective capture should happen before the user has to ask when a run produced reusable failure patterns.

## Open Follow-Ups

1. **Host stop hook:** audit and update stale `~/.codex/rally-hook.sh` or related Codex hook wrapper only with explicit approval.
2. **Binary/source drift:** rebuild repo-local `target/debug/rally` after source `HEAD=6a6584b` if future behavior checks need source-current code.
3. **Memory indexing:** store this retrospective in build-loop memory through the canonical writer so future recall sees it.
4. **Release gate productization:** consider adding Rally guidance or checks that warn when a release follows an edited-file claim without commit/handoff evidence.

## Retrieval Tags

`agent-rally-point`, `rally-protocol`, `handoff-projection`, `target-authored-ack`, `delivered-not-acked`, `claim-release-dirty-files`, `de6c20b`, `6e18262`, `6a6584b`, `stop-hook-127`, `stale-rally-binary`, `target-debug-rally`, `build-loop-memory-gap`, `retrospective`
