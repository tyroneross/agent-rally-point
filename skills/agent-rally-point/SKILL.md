---
name: agent-rally-point
description: Use when working in a repository that uses Rally/Agent Rally Point for cross-agent coordination, especially at session start, before editing files, when deciding what to do next, handing work to another agent, recording facts/artifacts/decisions, resolving blockers, or coordinating with other coding agents through the `rally` CLI.
---

# Agent Rally Point

Use `rally` as the live source of coordination truth.

## Session Start

From inside the repo, identify your stable tool id (`codex`, `claude_code`,
`pi`, `cursor`, `gemini`, `ci`, etc.) and enter the room:

```bash
rally enter --tool <tool> --json
rally next --tool <tool> --json
```

Read `next` before broad repo exploration:

- `actionable`: whether the recommendation can become work.
- `requires_human`: whether to stop and ask.
- `stop_reason`: why autonomous action should stop.
- `suggested_claims`: claim commands to reserve work.
- `suggested_commands`: checks and completion fact templates.
- `completion`: what durable fact is expected after work.

If `actionable` is false, do not invent work from Rally state. If
`requires_human` is true, ask the user.

## Core Workflow

1. **Claim before shared edits** when `next` recommends work or when the file is
   likely to overlap with another agent:

```bash
rally say claim --tool <tool> --subject "edit shared file" --path <path> --json
```

2. **Check before writing**:

```bash
rally check before-write --tool <tool> --path <path> --strict --json
```

If the check returns blocking findings, stop and resolve them before editing.

3. **Record meaningful outputs**:

```bash
rally say artifact --tool <tool> --subject "implemented change" --uri <path> --evidence "<verification>" --json
```

4. **Record coordination facts**:

```bash
rally say handoff --tool <tool> --target <other-tool> --subject "review this" --summary "<context>" --json
rally say blocker --tool <tool> --subject "need decision" --severity high --json
rally say resolve --tool <tool> --ref <blocker-id> --subject "resolved" --json
rally say decision --tool <tool> --subject "binding decision" --status binding --json
rally say release --tool <tool> --ref <claim-id> --subject "done" --json
```

5. **Loop back**:

```bash
rally next --tool <tool> --json
```

Continue only while the next action is actionable, safe, and inside the user's
scope.

## Managed Sessions

Use managed sessions for reliable live delivery into visible panes:

```bash
rally run claude --backend tmux --json
rally inject <session|name|tool> --handoff <event-id> --json
rally capture <session|name|tool> --json
```

When delegating work from inside herdr, keep the user's main tab clean. Start
new helper agents in the workspace's `agents` tab whenever one exists. Discover
the tab with `herdr tab list`, then start the agent with `herdr agent start
... --tab <agents-tab-id> --no-focus -- ...` or use a Rally backend option that
targets that tab when available. Only place helper agents in the active tab when
the user explicitly asks for that.

Agents should call `rally check before-write` explicitly before shared edits.
Rally does not install host hooks or prompt injection glue.

## Judgment Rule

Rally recommends and constrains work; it does not replace judgment.

- If a fact affects files, shells, editors, credentials, or another agent,
  inspect source event ids and evidence before acting.

## Finish Work Cleanly

Before ending a session:

```bash
rally room --json
rally next --tool <tool> --json
rally say release --tool <tool> --ref <claim-id> --subject "done" --json
```

If something remains for another agent, leave a `handoff` with enough context
and source-linked artifacts.
