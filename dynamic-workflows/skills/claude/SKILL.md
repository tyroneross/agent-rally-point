---
name: dynamic-workflows
description: Use when the user asks to fan out work across multiple agents, run a dynamic workflow, use parallel subagents, or coordinate several agents on a workstream through Agent Rally Point. Defines the workstream/task-packet protocol and the rally coordination loop.
---

# Rally Flow — Claude skill

Rally facilitates; Claude executes. Rally records claims, checks write boundaries, routes handoffs,
and exposes room state — it never spawns or retries agents. This skill turns a goal into a
*workstream descriptor*, lints it to prove boundary safety, then fans out agents that coordinate
through `rally`. Claude Code is the execution host.

**The full host protocol — decompose+lint, the two fan-out tiers, the per-task rally loop, aggregate,
and stop conditions — lives in [`../SHARED.md`](../SHARED.md). Read it.** This file states only the
Claude-specific bits below.

## Claude-specific bits

- **`--tool` value:** `claude_code` everywhere `<TOOL>` appears in SHARED.md.
- **Fan-out tool:** Claude's `Agent`/`Task` tool for Tier 1; managed `rally run claude …` for Tier 2.
- **Permission gate:** none — Claude may fan out by default when the work is parallelizable and
  the descriptor lints clean. (Codex gates fan-out on an explicit user request; Claude does not.)
- **Blocker severity:** include `--severity` when the blocker warrants it; it is optional for Claude.

Reference: [`../SHARED.md`](../SHARED.md) · canonical spec [`../../PROTOCOL.md`](../../PROTOCOL.md)
