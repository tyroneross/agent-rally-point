---
name: dynamic-workflows
description: Use when the user explicitly asks Codex to run a dynamic workflow, fan out work across agents, use parallel subagents/delegation, or coordinate several agents on a workstream through Agent Rally Point.
---

# Dynamic Workflows — Codex skill

## 1. Framing

Rally facilitates. Codex executes. Rally records claims, checks write
boundaries, routes handoffs, and exposes room state. It never spawns agents,
schedules work, or runs code. Codex fans out with its own native delegation
tools (subagents, parallel tasks). Do not port or replicate any Pi/Lambda
runtime here.

## 2. Permission gate

Only spawn subagents or delegate tasks when the user **explicitly** asks for
parallel/delegation work. Otherwise execute locally — but still author a
descriptor and follow the rally loop. The loop is always active; fan-out is
conditional.

## 3. Decompose + lint

Author a workstream descriptor per `../../PROTOCOL.md` (§1):

```json
{
  "workstream": "<objective>",
  "description": "<context for any agent dropping in>",
  "tasks": [
    {
      "id": "t1",
      "intent": "<what this achieves>",
      "owns": ["path/to/scope"],
      "validation": "<deterministic command>",
      "output": "<expected artifact shape>",
      "tier": "host-native"
    }
  ]
}
```

Lint rules enforced (exit 0 required before fan-out):
- Structural completeness: every task has `id`, `intent`, `owns`, `validation`, `output`.
- MECE boundaries: no two write-tasks own overlapping paths.
- Determinism: `validation` and `commands[]` must not contain `Date.now()`, `Math.random()`, or `new Date()`.
- Dependency integrity: `depends_on` ids must resolve; no cycles.

```bash
node dynamic-workflows/core/workstream-lint.mjs my.workstream.json
# exit 0 → valid   exit 1 → violations   exit 2 → parse error
```

Do not fan out until exit 0.

## 4. Tier 1 — host-native fan-out

Use Codex native delegation. Each delegated subagent receives exactly one task
packet (id, intent, owns, validation, output). Write sets must be disjoint —
enforce this via the linted `owns` fields, not by convention.

Subagent prompt discipline: the final action is **one** structured result
returning changed files + validation output. No prose after the result block.

## 5. Tier 2 — cross-host

When a task's `tier` is `cross-host` or the work must reach another terminal:

```bash
rally run <host> --json
rally inject <session|name|tool> --handoff <event-id> --json
```

## 6. Per-task rally loop

Run this loop for every task (substitute your task values):

```bash
# Enter the room
rally enter --tool codex --json

# Claim the work
rally say claim --tool codex --subject "<task.intent>" --path <task.owns...> --json

# Guard: check for conflicts before touching anything
rally check before-write --tool codex --path <task.owns...> --strict --json
# Blocking finding → stop; resolve or pick a non-overlapping task

# Do the work (native or via rally run/inject)

# Verify: run task.validation (must be deterministic)

# Record the artifact
rally say artifact --tool codex --subject "<task.output>" --uri <path> --evidence "<validation result>" --json

# Release the claim
rally say release --tool codex --ref <claim-id> --subject "done" --json

# Advance
rally next --tool codex --json
```

## 7. Aggregate + integrate

After all tasks complete, read the full room:

```bash
rally room --json
```

Confirm every task posted an artifact with evidence before declaring the
workstream done. Integrate and re-validate subagent output yourself. Never
auto-trust a peer's result — inspect evidence fields and source event ids.

## 8. Stop conditions

Hand back to the user when:
- `rally next` returns `requires_human: true`.
- `rally check before-write` blocks and the conflict cannot be resolved.
- A task hits a real blocker: `rally say blocker --tool codex --subject "<reason>" --severity high --json`.

---

Reference: `../../PROTOCOL.md`
