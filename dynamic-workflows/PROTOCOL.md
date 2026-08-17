<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Flow — coordination protocol

*Rally Flow is agent-rally-point's take on dynamic workflows.*

A **workstream** is a coordination plan for several agents working the same repo. It is
**not** an execution engine. Rally (the `rally` CLI) facilitates — it records facts, checks
write boundaries, routes handoffs, and exposes room state. **Each host runs its own agents.**
This module gives you the descriptor format, a linter that checks a plan's structure before
you fan out, and host-facing skills that map the plan onto the rally primitives.

> **The linter is not a security boundary.** It checks structure, determinism, MECE write
> boundaries, dependency integrity, and the charset of the identifiers and paths that get
> rendered into commands. It does not read your code, sandbox anything, or tell a helpful
> descriptor from a hostile one. A clean lint does **not** mean a descriptor is safe to run.
> Treat a descriptor from an author you do not trust the way you would treat a pull request
> from a stranger. See §1b.

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
      "id": "string",            // required, unique, /^[A-Za-z0-9._-]+$/
      "intent": "string",        // required — what this task achieves
      "owns": ["path", ...] | "read-only",  // required — MECE write scope, or read-only
      "validation": "string",    // required — PROSE describing how to verify. Not run.
      "validation_recipe": "name",           // optional — a recipe from the local registry
      "output": "string",        // required — the expected result shape / artifact
      "tier": "host-native" | "cross-host",  // optional, default host-native
      "depends_on": ["taskId", ...],         // optional — must resolve, no cycles
      "commands": ["string", ...]            // optional — additional declared commands (lint-scanned)
    }
  ]
}
```

### `validation` is a description; `validation_recipe` is a command

`validation` is free prose written by whoever wrote the descriptor. Nothing checks it and
nothing runs it. `core/packet.mjs` renders it in a ```` ```text ```` block, never a
```` ```bash ```` block, and tells the receiving agent to work out the command itself and
run it under its own host's approval policy. A descriptor cannot hand an agent command text
to paste into a shell.

When you want a real command in the packet, name a **recipe**:

```jsonc
"validation": "the rally-cli store tests pass",
"validation_recipe": "cargo-test"
```

The recipe's argv lives in `VALIDATION_RECIPES` in `core/workstream-lint.mjs` — local source,
reviewed like any other code. The descriptor supplies only the name. Current recipes:
`cargo-clippy`, `cargo-test`, `go-test`, `node-test`, `none`, `npm-test`, `pytest`,
`shellcheck`. An unknown name is a lint error. Adding one is a code change and a review.

### Lineage (run / step / parent-step)

A fan-out batch shares one **`run_id`** (any stable string, minted at batch start — it is *not* a
descriptor field). Each task's `id` is its **step**, each `depends_on` entry a **parent-step**. Agents
stamp these as scope markers on every fact they emit (`rally say … --run <run_id> --step <task.id>
--parent-step <dep>`), which lets the orchestrator reconstruct the whole fan-out via
`rally dag --run <run_id>` and resume dormant agents via `rally wake-due` (an idle agent emits
`rally say standby --reason idle --wake-after +30m`; the runner fires the resume). The host skill
([`../skills/rally-workflows/SKILL.md`](../skills/rally-workflows/SKILL.md) §4, §7) carries the exact
call shapes; the event vocabulary and encoding are in
[`../docs/ORCHESTRATOR_SEAM.md`](../docs/ORCHESTRATOR_SEAM.md). Markers are optional and additive —
omitting them costs only observability, never correctness.

### Lint rules (enforced)

1. **Structural completeness** — every task declares `id`, `intent`, `owns`, `validation`, `output`.
2. **MECE boundaries** — no two write-tasks may `own` overlapping paths (prefix-aware). This is what
   lets agents fan out without colliding. Use `"read-only"` for review/analysis tasks.
3. **Determinism** — declared commands (`validation`, `commands[]`) must not contain `Date.now()`,
   `Math.random()`, or `new Date()`; a shared plan must be reproducible. (Rule lifted from
   pi-dynamic-workflows, MIT — see `NOTICE`.)
4. **Dependency integrity** — `depends_on` ids must resolve; no cycles.
5. **Charset limits on anything rendered into a command:**
   - `id` — `/^[A-Za-z0-9._-]+$/`.
   - `owns` paths — only `[A-Za-z0-9._/-]`, plus an optional trailing `*` or `**` on the last
     segment. Repo-relative only: no leading `/`, no `..` segment, no `//`, no trailing `/`.
     Everything else is rejected, including `;` `|` `&` `>` `<` `(` `)` `$` backtick, quotes,
     backslash, whitespace, and control characters.
   - `intent`, `output` — no `"`, `$`, backtick, newline, or control character.
   - `validation` — no triple-backtick fence, no control characters.
   - `validation_recipe` — must name a recipe in the local registry.

Run it before any fan-out:

```bash
node core/workstream-lint.mjs my.workstream.json   # exit 0 valid · 1 violations · 2 parse error
```

## 1b. What the linter does not do

Rule 5 exists so that a rendered command cannot be broken out of — an `owns` path cannot append
a second command to a `--path` argument. It is not a judgement about intent. The linter:

- does not read the code a task will touch, or the diff it produces;
- does not sandbox, contain, or restrict anything at run time;
- does not verify that `validation` describes a real or honest check;
- cannot distinguish a useful task from a hostile one.

`core/packet.mjs` carries the same posture into the packets it renders. Only two kinds of command
text reach a ```` ```bash ```` block: the rally loop the generator writes itself, and the argv of a
named recipe from the local registry. Every value interpolated into those commands is POSIX
shell-quoted (`shellQuote`), so a value stays one argument whatever it contains — including a value
that never went through the linter. Descriptor prose renders as prose.

Running the work a descriptor describes is the host's call, under the host's own approval policy.
Rally facilitates; it does not vouch.

## 2. Spawn tiers

| Tier | When | Mechanism | Rally's role |
|------|------|-----------|--------------|
| **host-native** (default) | Single host, agents share one machine/session | Host's own fan-out — Claude `Agent`/`Task`, Codex delegation, Pi children | Records write claims or read activity, handoffs, and artifacts; checks write boundaries |
| **cross-host** | Work spans hosts/terminals/machines | `rally run` a managed session, `rally inject` the task packet | Same — plus carries the packet across the host boundary |

Tier is a hint on the task; the host decides how to actually run. Rally behaves identically either way.

## 3. The agent loop (per task)

Every task enters, does the work, verifies, posts `<task.id>: <task.output>` as an artifact, and
calls `rally next`. Its coordination lifecycle depends on `owns`:

- A write task posts one path-scoped `claim`, runs strict `before-write` once per owned path, and
  releases the claim after its artifact.
- An `owns: "read-only"` task posts one run/step-scoped `presence` with
  `summary=activity:read-only` and `status=working`. It posts no claim, runs no before-write check,
  and posts no release. It must not intentionally change task/domain resources; its only permitted
  writes are the generated Rally coordination records and ordinary transient tool state created
  during verification. Its terminal artifact references the activity event id.

```text
write:     enter → claim → before-write → work → verify → artifact → release → next
read-only: enter → presence(activity:read-only) → work → verify → artifact(ref=activity) → next
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

**Checkpoint convention** — a write task starts with a claim, a read-only task starts with
nonexclusive activity, and both finish with an artifact whose subject names the task id:

```bash
rally say claim    --tool <you> --subject "<task.id>" --path <owns...>
rally say presence --tool <you> --subject "<task.id>: <intent>" --summary activity:read-only \
  --status working --run <run_id> --step <task.id>
rally say artifact --tool <you> --subject "<task.id>: <result>" --uri <path> --evidence "<validation>" \
  --run <run_id> --step <task.id>
```

**Resume** (after a crash, a new session, or on a different host) — re-derive the remaining work from
Rally instead of memory:

```bash
rally room --json > room.json
node core/workstream-status.mjs my.workstream.json room.json --tool-prefix agent
# → per-task done|claimed|active|pending + `to_dispatch` (pending tasks whose deps are done)
# exit 0 = complete · exit 3 = work remains
```

`claimed` remains exclusive write ownership. `active` is nonexclusive read-only work. Before
O33-C's run-scoped `active_activities` projection exists, `active` uses only an exact fresh squad
tool match, `<tool-prefix>:<task.id>`; a differently prefixed, idle, or substring-matching tool does
not hold the task. Pass the same prefix used by `packet.mjs` (both default to `agent`). This is a
transitional resume heuristic, not proof that a particular run is live, and the A+B branch stays
inactive until O33-C replaces it in the combined activation gate.

Re-dispatch ONLY the `to_dispatch` set; tasks with a done-artifact are skipped. A resumable host loop:

```bash
while ! node core/workstream-status.mjs ws.json <(rally room --json) --tool-prefix agent; do
  : # spawn host-native agents for each id in to_dispatch (Tier 1), or rally run/inject (Tier 2)
done
```

This is the piece pi structurally cannot have: **state lives in Rally, not a parent's RAM**, so a
multi-hour / multi-session / multi-host workstream survives a crash and resumes exactly where it
stopped. Bounded concurrency (`core/limiter.mjs`, lifted from pi) still caps in-flight fan-out;
`core/fanout.mjs` supplies the width and names the constraint that set it (default 10, hard
ceiling 12).

**Lineage from the .mjs path.** `core/route.mjs` owns concurrency/ordering only — it does **not**
shell out to `rally`; the host supplies each task body as a thunk. So the lineage markers
(`--run`/`--step`, §1) are emitted **inside that host thunk** — the same `rally say claim/presence/artifact`
calls the SKILL §4 loop documents — not by `route.mjs`. Stamping them there is what makes a
`route.mjs`-driven fan-out visible to `rally dag --run <run_id>`. The thunk *is* the integration
point; `route.mjs` needs no change.

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
