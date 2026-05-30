---
name: rally-workflows
description: Use when fanning out work across multiple agents, running a dynamic workflow, coordinating parallel subagents, or splitting a workstream across hosts, terminals, or machines through Agent Rally Point. Defines the workstream descriptor + task-packet protocol and the per-task rally coordination loop. Host-neutral — works for any coding agent.
---

<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Flow — host-neutral skill

Rally facilitates; the host executes. Rally records claims, checks write boundaries, routes
handoffs, and exposes room state — it never spawns, schedules, or retries agents. This skill turns a
goal into a *workstream descriptor*, lints it to prove boundary safety, then fans out agents that
coordinate through `rally`. The same protocol runs on any coding host; the host supplies the few
runtime values below. Canonical wire spec: [`../../dynamic-workflows/PROTOCOL.md`](../../dynamic-workflows/PROTOCOL.md).

## Host adapter (resolve these for your runtime)

This skill names no specific agent. Before running, resolve three host values:

| Knob | How to resolve |
|------|----------------|
| `<TOOL>` | Your host's rally tool id — the value you pass to `rally enter --tool …`. Use one stable id per terminal/role, e.g. `<host>:<role>:<n>`. The model belongs in the id, not in a separate registration. |
| Fan-out authorization | Tier-1 in-process fan-out is allowed when the work is parallelizable **and** the descriptor lints clean — **unless your host requires explicit user authorization before spawning subagents/delegating**. If it does, gate Tier-1 on that explicit request; otherwise fan out by default. The rally loop is always active either way; only fan-out is conditional. |
| Flag conventions | If your host parses structured output, pass `--json` on every `rally` command. Add `--severity` on `rally say blocker` when your host distinguishes severities. |

Do not hard-code another agent's identity in a descriptor or command — each host supplies its own
`<TOOL>` at runtime. Substitute `<TOOL>` wherever it appears below.

## 1 · Decompose — author the descriptor

Write a JSON workstream descriptor per `../../dynamic-workflows/PROTOCOL.md` (§1). Required
top-level fields: `workstream` (objective), `description` (drop-in context), `tasks` (non-empty).
Each task needs `id` (unique), `intent`, `owns`, `validation`, `output`.

- `owns` — either `"read-only"` or a non-empty array of path strings. Paths across write-tasks must
  be **disjoint** (no prefix overlap). That MECE guarantee is what lets agents run in parallel.
- `validation` — a deterministic shell command (no `Date.now()`, `Math.random()`, `new Date()`).
- `output` — the expected artifact shape (what a reviewer would confirm).
- `depends_on` — optional; ids must resolve; no cycles permitted.

**Pick one `run_id` per workstream** (any stable string — e.g. the descriptor filename or a UUID
minted at batch start). It is the lineage handle: every fact a fanned-out agent emits carries
`--run <run_id>`, each task's `id` becomes its `--step`, and each `depends_on` entry becomes a
`--parent-step`. That is what lets the orchestrator reconstruct the whole fan-out with
`rally dag --run <run_id>` (see §7). The `run_id` is not a descriptor field — it is generated/chosen
at fan-out time and threaded through the loop below.

**Lint before any fan-out.** The linter enforces all four rules (structure, MECE, determinism,
dependency integrity):

```bash
node dynamic-workflows/core/workstream-lint.mjs my.workstream.json
# exit 0 → valid · 1 → violations · 2 → parse error
```

Do not dispatch agents until the linter exits 0.

## 2 · Tier 1 (default) — host-native fan-out

Use the host's native subagent/delegation tool. Hard cap: **≤4 parallel**. One agent per task
packet. Prompt each subagent with its `owns`, `validation`, and `output` only — minimal context.

**Subagent prompt discipline:** the subagent's final action is ONE structured result:

```
{ "task": "<id>", "changed_files": [...], "validation_result": "<verbatim output>" }
```

No prose after that block. The orchestrating agent collects results; it does not trust them
without re-running validation for any shared-impact change.

## 3 · Tier 2 — cross-host fan-out

When work spans hosts, terminals, or machines, use managed rally sessions instead of in-process
subagents:

```bash
# AGENT is a positional (claude | codex | …); pick a backend you have installed.
rally run <agent> --name <session> --backend tmux --tool <TOOL> --json   # start a managed session
# Deliver work as a handoff FACT, then inject its event id (inject takes --handoff/--text, not a file):
rally say handoff --tool <TOOL> --target <session> --subject "<task.intent>" --json   # → returns <event-id>
rally inject <session> --handoff <event-id> --require-ack --json
```

Rally behavior is identical either way — it stays a facilitator regardless of tier.

## 4 · Per-task rally loop

Each agent executes this loop for its assigned task. The `--run`/`--step`/`--parent-step` markers
are what make the fan-out **observable** — drop them and `rally dag` reconstructs nothing:

```bash
rally enter --tool <TOOL>
rally say claim --tool <TOOL> --subject "<task.intent>" --path <owns...> \
  --run <run_id> --step <task.id> --parent-step <dep>   # --parent-step once per depends_on entry; omit if none
rally check before-write --tool <TOOL> --path <owns...> --strict
# blocking finding → stop; resolve or pick a non-overlapping task

# do the work
<run task.validation>

rally say artifact --tool <TOOL> --subject "<task.output>" --uri <path> --evidence "<validation result>" \
  --run <run_id> --step <task.id>
rally say release --tool <TOOL> --ref <claim-id> --subject "done"
rally next --tool <TOOL>
```

A `check before-write --strict` that returns a blocking finding **stops the agent** — it does not
proceed; it either resolves the conflict or moves to a non-overlapping task.

**Idle → standby (dormant, not stopped).** If the agent must *wait* — an unfinished `depends_on`
dependency, or an external signal that hasn't arrived — it goes **dormant** instead of handing back
or busy-waiting. This is distinct from the §6 hard-stop `blocker` (a real failure): standby is a
recoverable pause that stays observable and resumable.

```bash
rally say standby --tool <TOOL> --reason idle --wake-after +30m --run <run_id> --step <task.id>
# … later, when the dependency lands or the signal arrives, on resume:
rally say wake --tool <TOOL> --ref-standby <standby-event-id>
# then continue the loop (re-check, do the work, artifact, release)
```

A standby surfaces the agent in `rally wake-due` once `--wake-after` passes; the external runner
(`rally watch`, §7) is what fires the resume. Rally signals; it never wakes the agent itself.

**Quality wrapper (recommended):** run each task's *do the work* step through
[`mini-loop`](../mini-loop/SKILL.md) — a zero-dependency assess → plan → execute → mini-judge loop
that checks the result against the task's own `validation` and `output` contract before it posts an
artifact. It catches a wrong-but-plausible result at the task instead of at integration.

## 5 · Aggregate

```bash
rally room --tool <TOOL> --json
```

Confirm every task posted an artifact **with evidence** before declaring the workstream done.
Re-run `task.validation` for any change with shared impact. Never auto-trust a subagent result.

## 6 · Stop conditions

**Hard stop** (hand back to the user) when any of:

- `rally next` returns `requires_human: true`
- `rally check before-write --strict` blocks and cannot be resolved
- A task hits a real blocker → `rally say blocker --tool <TOOL> --subject "<reason>"`

A hard stop is a *failure* the user must resolve. Do not use it for *waiting* — a recoverable wait
(unmet dependency, pending external signal) is a `standby` (§4), not a `blocker`.

## 7 · Observe & resume

The orchestrator watches a running fan-out and resumes idle agents through three read-only views.
The hard rule holds throughout: **Rally observes and signals; the external runner fires the work.**

```bash
# Progress: every step as landed | in-flight | stalled, edges from parent-step/ref lineage.
rally dag --run <run_id> --json

# Resumable agents: standby facts whose --wake-after has passed (trust-gated to room squads).
# Each entry carries a `suggested_command` STRING — Rally never runs it.
rally wake-due --json

# The runner that actually fires: polls wake-due / new activity and invokes the suggestion.
rally watch --on-activity 'rally next --tool <TOOL> --json'
```

Split of responsibility, never blurred:
- `rally dag` / `rally wake-due` — **Rally**: read-only projections over the ledger. They report and
  suggest; they start nothing.
- `rally watch` — the **runner**: the only piece that executes a `suggested_command`. Substitute a
  LaunchAgent / cron / Build Loop here if you prefer; Rally's contract is unchanged.

Full event vocabulary, lineage encoding, and the runner contract:
[`../../docs/ORCHESTRATOR_SEAM.md`](../../docs/ORCHESTRATOR_SEAM.md).

Reference: canonical spec [`../../dynamic-workflows/PROTOCOL.md`](../../dynamic-workflows/PROTOCOL.md) ·
coordination doctrine [`../../dynamic-workflows/COORDINATION.md`](../../dynamic-workflows/COORDINATION.md) ·
model tiers [`../../dynamic-workflows/MODEL-TIERS.md`](../../dynamic-workflows/MODEL-TIERS.md)
