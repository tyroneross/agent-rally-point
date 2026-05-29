---
name: dynamic-workflows
description: Use when the user explicitly asks Codex to run a dynamic workflow, fan out work across agents, use parallel subagents/delegation, or coordinate several agents on a workstream through Agent Rally Point.
---

# Rally Flow — Codex skill

Rally facilitates. Codex executes. Rally records claims, checks write boundaries, routes handoffs,
and exposes room state. It never spawns agents, schedules work, or runs code. Codex fans out with
its own native delegation tools. Do not port or replicate any Pi/Lambda runtime here.

**The full host protocol — decompose+lint, the two fan-out tiers, the per-task rally loop, aggregate,
and stop conditions — lives in [`../SHARED.md`](../SHARED.md). Read it.** This file states only the
Codex-specific bits below.

## Codex-specific bits

- **`--tool` value:** `codex` everywhere `<TOOL>` appears in SHARED.md.
- **Fan-out tool:** Codex native delegation (subagents/parallel tasks) for Tier 1; managed
  `rally run codex …` for Tier 2.
- **Permission gate (Codex-only):** spawn subagents or delegate tasks ONLY when the user
  **explicitly** asks for parallel/delegation work. Otherwise execute locally — but still author a
  descriptor and follow the rally loop. The loop is always active; fan-out is conditional.
- **Flag convention:** pass `--json` on every `rally` command (Codex parses structured output).
- **Blocker severity:** include `--severity high` on `rally say blocker` (Codex convention).

Reference: [`../SHARED.md`](../SHARED.md) · canonical spec [`../../PROTOCOL.md`](../../PROTOCOL.md)
