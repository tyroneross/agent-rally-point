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

Each agent executes this loop for its assigned task:

```bash
rally enter --tool <TOOL>
rally say claim --tool <TOOL> --subject "<task.intent>" --path <owns...>
rally check before-write --tool <TOOL> --path <owns...> --strict
# blocking finding → stop; resolve or pick a non-overlapping task

# do the work
<run task.validation>

rally say artifact --tool <TOOL> --subject "<task.output>" --uri <path> --evidence "<validation result>"
rally say release --tool <TOOL> --ref <claim-id> --subject "done"
rally next --tool <TOOL>
```

A `check before-write --strict` that returns a blocking finding **stops the agent** — it does not
proceed; it either resolves the conflict or moves to a non-overlapping task.

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

Hand back to the user when any of:

- `rally next` returns `requires_human: true`
- `rally check before-write --strict` blocks and cannot be resolved
- A task hits a real blocker → `rally say blocker --tool <TOOL> --subject "<reason>"`

Reference: canonical spec [`../../dynamic-workflows/PROTOCOL.md`](../../dynamic-workflows/PROTOCOL.md) ·
coordination doctrine [`../../dynamic-workflows/COORDINATION.md`](../../dynamic-workflows/COORDINATION.md) ·
model tiers [`../../dynamic-workflows/MODEL-TIERS.md`](../../dynamic-workflows/MODEL-TIERS.md)
