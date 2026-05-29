---
name: dynamic-workflows
description: Use when the user asks to fan out work across multiple agents, run a dynamic workflow, use parallel subagents, or coordinate several agents on a workstream through Agent Rally Point. Defines the workstream/task-packet protocol and the rally coordination loop.
---

Rally facilitates; Claude executes. Rally records claims, checks write boundaries, routes handoffs,
and exposes room state — it never spawns or retries agents. This skill turns a goal into a
*workstream descriptor*, lints it to prove boundary safety, then fans out agents that coordinate
through `rally`. Claude Code is the execution host; the descriptor + lint keep the plan honest.

## 1 · Decompose — author the descriptor

Write a JSON workstream descriptor per PROTOCOL.md. Required fields at the top level:
`workstream` (objective), `description` (drop-in context), `tasks` (non-empty).
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

Use Claude's `Agent`/`Task` tool. Hard cap: **≤4 parallel**. One agent per task packet.
Prompt each subagent with its `owns`, `validation`, and `output` only — minimal context.

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
rally run claude --name <session> --backend tmux --tool claude_code --json   # start a managed session
# Deliver work as a handoff FACT, then inject its event id (inject takes --handoff/--text, not a file):
rally say handoff --tool claude_code --target <session> --subject "<task.intent>" --json   # → returns <event-id>
rally inject <session> --handoff <event-id> --require-ack --json
```

Rally behavior is identical either way — it stays a facilitator regardless of tier.

## 4 · Per-task rally loop (§3 of PROTOCOL)

Each agent executes this loop for its assigned task:

```bash
rally enter --tool claude_code
rally say claim --tool claude_code --subject "<task.intent>" --path <owns...>
rally check before-write --tool claude_code --path <owns...> --strict
# blocking finding → stop; resolve or pick a non-overlapping task

# do the work
<run task.validation>

rally say artifact --tool claude_code --subject "<task.output>" --uri <path> --evidence "<validation result>"
rally say release --tool claude_code --ref <claim-id> --subject "done"
rally next --tool claude_code --json
```

A `check before-write --strict` that returns a blocking finding **stops the agent** — it does not
proceed; it either resolves the conflict or moves to a non-overlapping task.

## 5 · Aggregate

```bash
rally room --tool claude_code --json
```

Confirm every task posted an artifact **with evidence** before declaring the workstream done.
Re-run `task.validation` for any change with shared impact. Never auto-trust a subagent result.

## 6 · Stop conditions

Hand back to the user when any of:

- `rally next` returns `requires_human: true`
- `rally check before-write --strict` blocks and cannot be resolved
- A task hits a real blocker → `rally say blocker --tool claude_code --subject "<reason>"`

---

Reference: [../../PROTOCOL.md](../../PROTOCOL.md)
