---
name: agent-rally-point
description: Use when working in a repository that uses Rally/Agent Rally Point for cross-agent coordination, especially at session start, before editing files, when deciding what to do next, handing work to another agent, recording tasks/artifacts/decisions/lessons, acknowledging handoffs, resolving blockers, or coordinating with other coding agents through the `rally` CLI.
---

# Agent Rally Point

Rally is a local-first coordination substrate for coding agents. Use the
`rally` CLI as the source of live coordination truth; do not rely on a large
planning doc as the day-to-day control surface.

## Session Start

From inside the repo, identify your stable tool id (`codex`, `claude`, `pi`,
`cursor`, `gemini`, `ci`, etc.) and run the tool-named startup command when
available:

```bash
rally pi
rally claude
rally codex
```

For custom tools, use:

```bash
rally start <tool>
```

`rally <tool>` / `rally start <tool>` defaults to JSON. It writes presence,
returns preflight, context, packet, checkpoint, cursor state, warnings, and the
next watch command. It does not launch the tool process.

Run `rally doctor --tool <tool> --json` when startup warnings are present or
before making broad edits. Run `rally setup --json` when onboarding a new repo or
checking whether cmux/Herdr/agent harnesses are visible. If anonymous claims or
handoffs appear, treat that as a harness setup problem.

`rally setup enforcement strict` makes new anonymous writes fail. Use it once all
harnesses pass a stable `--tool`. `rally setup install cmux` and `rally setup
install herdr` install edge wrappers/config hooks; they do not move coordination
state out of Rally.

If `rally start` is unavailable, fall back to:

```bash
rally context --tool <tool> --json
rally packet --tool <tool> --json
```

Use the returned start `context.brief` or context `data.brief` to decide what to
do next:

- `recommended_next_action`: the preferred next action.
- `attuned_items`: scored, source-linked facts ranked for your tool.
- `top_priority`: the highest-priority source item.
- `needs_attention`: ranked handoffs, tasks, blockers, and conflicts.
- `active_tasks`, `active_claims`, `active_blockers`: current work state.
- `decisions`, `lessons`: durable project knowledge.
- `minimum_trust_for_automation`: trust threshold before acting automatically.

Use `rally packet --tool <tool> --json` when you need a compact work brief for a
specialized agent. It is derived from context, read-only, and role-shaped by the
tool profile: reviewer, builder, architect, QA, or general.

Use `rally adapter contract --json` before wiring a new client integration.
Use `rally cmux packet --tool <tool> --json` or `rally herdr packet --tool
<tool> --json` for side-effect-free adapter payloads. Adapters must honor trust
fields and `ready_to_inject: false` unless the operator explicitly overrides.

If `rally context` is unavailable, fall back to:

```bash
rally preflight --tool <tool> --start-ping --json
rally inbox --tool <tool> --json
rally diagnose --tool <tool> --json
```

## Core Workflow

1. **Create or refresh your profile** when missing, stale, or your task changed:

```bash
rally profile --tool <tool> --role builder --capability rust --capability implementation --watch crates/rally-core --json
```

Use `--role reviewer`, `--role architect`, `--role builder`, or `--role qa`
when the agent is intentionally specialized. If no role is declared, Rally can
still infer lightweight specialization from capabilities such as `review`,
`architecture`, `qa`, or `implementation`.

2. **Declare subscriptions** for paths or event kinds you want surfaced:

```bash
rally subscribe --tool <tool> --path crates/rally-core --event-kind task --event-kind decision --json
```

3. **Record active work** as a task:

```bash
rally task --tool <tool> --subject "finish context ranking" --status active --verification "cargo test" --json
```

4. **Claim files/resources before editing**:

```bash
rally claim --tool <tool> --path crates/rally-core/src/context.rs --subject "context ranking" --json
```

5. **Record outputs as artifacts**:

```bash
rally artifact --tool <tool> --subject "context contract" --artifact-kind schema --uri docs/CONTEXT_BRIEF_SCHEMA.md --json
```

6. **Record durable project truth as decisions**:

```bash
rally decision --tool <tool> --subject "agents use rally context for next action" --status binding --scope agent-start --json
```

7. **Record reusable learning as lessons** when a failure, convention, or
pattern should compound across sessions:

```bash
rally lesson --tool <tool> --subject "avoid giant planning docs as control surfaces" --lesson-kind coordination --confidence 0.9 --json
```

8. **Handoff or acknowledge work**:

```bash
rally handoff --to <other-tool> --from-tool <tool> --subject "review context brief" --json
rally ack --tool <tool> <handoff-id> --summary "done" --json
rally needs-info --tool <tool> <handoff-id> --reason "need branch name" --json
rally reject --tool <tool> <handoff-id> --reason "out of scope" --json
```

9. **Block and unblock explicitly**:

```bash
rally blocker --tool <tool> --subject "need decision" --reason "which PR is next?" --json
rally unblock --tool <tool> <blocker-id> --resolution "decision recorded" --json
```

## Acting On Context

Treat `rally context` as the live brief:

- Read `attuned_items` before broad repo exploration. Prefer items with factors
  matching your role, current task, watched paths, subscriptions, trusted
  origin, or active claims.
- If `recommended_next_action.action` is `ack_handoff`, inspect the source
  event and respond with `ack`, `needs-info`, or `reject`.
- If it is `work_task`, work the referenced task and record artifacts.
- If it is `resolve_blocker`, update or resolve the blocker before continuing.
- If it is `resolve_claim_conflict`, coordinate before editing overlapping
  files.
- If it is `continue_claim`, continue the claimed work and release it when done.
- If it is `proceed_solo`, proceed, but still claim files before editing.

For role-specific startup, prefer the packet after reading context:

- Reviewer packets emphasize review targets, artifacts, decisions, test
  evidence, and trust risks.
- Builder packets emphasize active tasks, claims, blockers, decisions, and
  files to touch.
- Architect packets emphasize decisions, lessons, artifacts, and open tradeoffs.
- QA packets emphasize verification artifacts, test commands, failure lessons,
  and risk areas.

Never treat recommendations as magic. They are derived from source events.
Check `source_event_ids`, `origin`, and `trust_status` when the action affects
files, tools, shells, editors, or another agent.

## Trust Rule

`minimum_trust_for_automation` is policy, not decoration.

- It is okay to read and display untrusted facts.
- Do not automatically act on imported/remote facts unless their trust status
  satisfies the recommendation threshold.
- When in doubt, ask the user or record a blocker rather than silently acting.

## Finish Work Cleanly

Before ending a session:

```bash
rally context --tool <tool> --json
rally checkpoint status --json
rally release --tool <tool> <claim-id> --reason "done" --json
rally task --tool <tool> --subject "<task>" --status done --json
```

If something remains for another agent, leave a handoff with enough context and
source-linked artifacts.
