<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Dynamic Workflows — coordination protocol

A **workstream** is a coordination plan for several agents working the same repo. It is
**not** an execution engine. Rally (the `rally` CLI) facilitates — it records facts, checks
write boundaries, routes handoffs, and exposes room state. **Each host runs its own agents.**
This module gives you the descriptor format, a linter that proves a plan is safe to fan out,
and host-facing skills that map the plan onto the rally primitives.

> **The boundary, stated once:** in-memory subagents are a *host* strategy (Claude `Agent`/`Task`,
> Codex delegation, Pi child agents). They are Tier-1 fan-out, hidden behind the host. Rally never
> spawns, resumes, retries, or schedules work. If something would make Rally *execute*, it belongs
> in the host or an external runner, not here.

> **Two coordination modes.** This doc is **deterministic routing** — one driver decomposes a
> short, defined task into scripted subagent roles. For **multiple autonomous frontier agents**
> sharing a room (no single driver with full context), see **`COORDINATION.md`**: first-agent-is-lead,
> proactive engagement (backlog → no-regrets → check-lead), and rally-as-facilitator-not-coordinator.

## 1. The workstream descriptor

A JSON document. `core/workstream-lint.mjs` is the source of truth for what is valid; this section
documents the same contract.

```jsonc
{
  "workstream": "string",        // required, non-empty — the objective
  "description": "string",       // required, non-empty — context for any agent dropping in
  "thread": "string",            // optional — the rally thread_id this workstream coordinates under
  "tasks": [                     // required, non-empty
    {
      "id": "string",            // required, unique across tasks
      "intent": "string",        // required — what this task achieves
      "owns": ["path", ...] | "read-only",  // required — MECE write scope, or read-only
      "validation": "string",    // required — the command that verifies the task (deterministic)
      "output": "string",        // required — the expected result shape / artifact
      "tier": "host-native" | "cross-host",  // optional, default host-native
      "depends_on": ["taskId", ...],         // optional — must resolve, no cycles
      "commands": ["string", ...]            // optional — additional declared commands (lint-scanned)
    }
  ]
}
```

### Lint rules (enforced)

1. **Structural completeness** — every task declares `id`, `intent`, `owns`, `validation`, `output`.
2. **MECE boundaries** — no two write-tasks may `own` overlapping paths (prefix-aware). This is the
   guarantee that lets agents fan out without colliding. Use `"read-only"` for review/analysis tasks.
3. **Determinism** — declared commands (`validation`, `commands[]`) must not contain `Date.now()`,
   `Math.random()`, or `new Date()`; a shared plan must be reproducible. (Rule lifted from
   pi-dynamic-workflows, MIT — see `NOTICE`.)
4. **Dependency integrity** — `depends_on` ids must resolve; no cycles.

Run it before any fan-out:

```bash
node core/workstream-lint.mjs my.workstream.json   # exit 0 valid · 1 violations · 2 parse error
```

## 2. Spawn tiers

| Tier | When | Mechanism | Rally's role |
|------|------|-----------|--------------|
| **host-native** (default) | Single host, agents share one machine/session | Host's own fan-out — Claude `Agent`/`Task`, Codex delegation, Pi children | Records claims/handoffs/artifacts; checks boundaries |
| **cross-host** | Work spans hosts/terminals/machines | `rally run` a managed session, `rally inject` the task packet | Same — plus carries the packet across the host boundary |

Tier is a hint on the task; the host decides how to actually run. Rally behaves identically either way.

## 3. The agent loop (per task)

```text
rally enter --tool <you>
  → claim:   rally say claim --tool <you> --subject "<task.intent>" --path <owns...>
  → guard:   rally check before-write --tool <you> --path <owns...> --strict
             (blocking finding → stop, resolve, or pick a non-overlapping task)
  → do the work (host-native or via rally run/inject)
  → verify:  run task.validation  (must be deterministic)
  → record:  rally say artifact --tool <you> --subject "<task.output>" --uri <path> --evidence "<validation result>"
  → release: rally say release --tool <you> --ref <claim-id> --subject "done"
  → rally next --tool <you>
```

**Aggregate**: the coordinating agent reads `rally room` and confirms every task posted an artifact
with evidence before declaring the workstream done. It never auto-trusts a peer's result.

**Stop** (hand back to the user) when: `rally next.requires_human` is true, a `check before-write`
blocks and can't be resolved, or a task hits a real blocker (`rally say blocker`).

## 3b. Durable fan-out & resume (the long-running edge over pi-dynamic-workflows)

pi-dynamic-workflows fans out cleanly (`agent()`/`parallel()`/`pipeline()`) but keeps all progress
in one parent process's memory (`RuntimeState`). If that process dies — or the work spans sessions,
hosts, or hours — everything is lost; there is no resume. This module keeps the same fan-out shape
but **checkpoints every task to Rally**, so progress is durable and any fresh agent can resume.

**Checkpoint convention** — a task is *started* by a claim and *done* by an artifact naming the id:

```bash
rally say claim    --tool <you> --subject "<task.id>" --path <owns...>
rally say artifact --tool <you> --subject "<task.id>: <result>" --uri <path> --evidence "<validation>"
```

**Resume** (after a crash, a new session, or on a different host) — re-derive the remaining work from
Rally instead of memory:

```bash
rally room --json > room.json
node core/workstream-status.mjs my.workstream.json room.json
# → per-task done|claimed|pending + `to_dispatch` (pending tasks whose deps are done)
# exit 0 = complete · exit 3 = work remains
```

Re-dispatch ONLY the `to_dispatch` set; tasks with a done-artifact are skipped. A resumable host loop:

```bash
while ! node core/workstream-status.mjs ws.json <(rally room --json); do
  : # spawn host-native agents for each id in to_dispatch (Tier 1), or rally run/inject (Tier 2)
done
```

This is the piece pi structurally cannot have: **state lives in Rally, not a parent's RAM**, so a
multi-hour / multi-session / multi-host workstream survives a crash and resumes exactly where it
stopped. Bounded concurrency (`core/limiter.mjs`, lifted from pi) still caps in-flight fan-out.

## 4. Scaffolding scales to harness strength

- **Strong harness** (Claude Code, Codex): stay thin. The skill points at this protocol, the host
  fans out natively, the descriptor + lint keep boundaries honest.
- **Weaker harness** (no native subagents): lean on Rally for more — `rally next` for suggested
  commands, `rally run`/`inject` for cross-host delegation, the descriptor as the explicit plan the
  host follows step by step.

Either way Rally stays a facilitator. The host owns execution.

## 5. What this module is NOT

No `vm` script executor, no in-memory subagent runtime, no `agent()/parallel()/pipeline()` engine,
no scheduler, no `rally` execution subcommand. Those are the parts of pi-dynamic-workflows we
deliberately dropped. See `README.md` and `NOTICE` for what was lifted (the determinism rule, the
descriptor-validation discipline, a bounded-concurrency helper) and why.
