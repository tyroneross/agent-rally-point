<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Attuned Coordination

Rally's ambitious product direction is a local intelligence layer that helps
independent coding agents anticipate what each other need.

The goal is not to become an agent runtime. The goal is to make every agent
that already works in a repo better coordinated, better informed, and less
likely to duplicate, collide, or lose context.

## Thesis

Most agent systems coordinate through one of three shapes:

- an orchestrator that routes work to subagents
- a chat or handoff stream between agents
- a memory store that agents can query

Rally's stronger lane is different:

> A repo-native coordination substrate that derives the next useful context for
> each coding agent from durable work facts, trust, ownership, dependencies,
> and prior lessons.

Agents should not have to scrape a log and guess what matters. They should be
able to ask Rally:

```bash
rally context --tool codex --json
```

and receive a compact, ranked briefing:

- what changed since this agent last looked
- who needs something from this agent
- which claims, blockers, and decisions affect its current work
- which other agents may collide with it
- which imported facts are trusted enough to act on
- which prior lessons or conventions are relevant
- what next action is recommended, with sources

That is "attunement": Rally adapts the coordination view to the current agent,
task, repo, files, trust policy, and recent trace.

## What Rally Learns From The Field

External agent systems are converging on useful primitives:

| Source | Relevant lesson for Rally |
|---|---|
| A2A | Tasks, artifacts, agent cards, async updates, and secure remote interoperability matter. Rally should bridge task/artifact identity without requiring a server. |
| OpenAI Agents SDK | Handoffs, sessions, tools, and traces are core agent primitives. Rally should persist cross-process handoff/trace facts. |
| Anthropic multi-agent sessions | Parallel subagents work because each has isolated context and returns condensed findings. Rally should help agents exchange the right summary, not full hidden context. |
| LangGraph | Context engineering and explicit state determine whether handoffs work. Rally should make filtered handoff context a first-class output. |
| AutoGen | Selector/group-chat systems need turn state, candidate selection, and termination conditions. Rally should record state and evidence, not run the conversation. |
| Blackboard multi-agent research | Broadcast/shared workspaces let agents volunteer when they have relevant capability. Rally should support role-aware discovery and subscriptions. |
| Reflexion | Agents improve by storing verbal lessons from failures and feedback. Rally should capture verified lessons as durable, source-linked memory. |
| Voyager | Capability compounds through an executable/retrievable skill library. Rally should distinguish reusable procedures from one-off notes. |
| MemGPT/A-Mem/collaborative memory | Memory needs hierarchy, provenance, access control, and relevance ranking. Rally memory must be auditable and trust-aware, not a blob of recalled text. |

Sources include:

- https://a2a-protocol.org/latest/specification/
- https://developers.openai.com/api/docs/guides/agents/orchestration
- https://platform.claude.com/docs/en/managed-agents/multi-agent
- https://docs.langchain.com/oss/python/langchain/multi-agent/handoffs
- https://microsoft.github.io/autogen/dev/user-guide/agentchat-user-guide/selector-group-chat.html
- https://arxiv.org/abs/2303.11366
- https://openreview.net/forum?id=nfx5IutEed
- https://arxiv.org/abs/2310.08560

## Product Primitive

The core primitive is a projection, not a prompt.

```text
trace events
  -> TraceProjection
  -> AgentProfile
  -> ContextBrief
  -> JSON renderer / MCP resource / UI adapter
```

`TraceProjection` is the shared read model for durable state. Higher-level
intelligence should build on that projection rather than reparsing the log in
separate commands.

The first target output is `ContextBrief`:

```text
ContextBrief
  agent: tool/session identity
  routing: proceed/join/blocked/waiting
  needs_attention: ranked handoffs, blockers, conflicts
  relevant_changes: recent trusted changes with source ids
  collision_risk: overlapping claims and touched paths
  suggested_next_action: machine-readable action + reason
  memory: relevant decisions, conventions, lessons, procedures
  provenance: event ids, trust status, timestamps
```

The briefing must stay bounded, explainable, and source-linked. If an agent
cannot trace a recommendation back to specific Rally facts, Rally should lower
confidence or omit the recommendation.

## New Event Families

Rally already has handoffs, acknowledgements, claims, blockers, releases,
feedback, trust, and sync. Attuned coordination adds structured facts that make
anticipation possible:

| Event family | Purpose |
|---|---|
| `agent-profile` | Capabilities, current task, active branch, tools, preferences, availability, and trust identity. |
| `task` | Objective, owner, lifecycle, dependencies, expected artifacts, verification criteria. |
| `artifact` | Reports, patches, screenshots, test outputs, review notes, exported context packets. |
| `decision` | Binding or tentative project decisions, with scope, authority, and supersession. |
| `lesson` | Verified failure/success reflection that can improve later work. |
| `procedure` | Reusable repo-local skill or recipe, possibly generated by agents and validated by humans or CI. |
| `subscription` | What an agent wants surfaced: paths, tasks, event kinds, peers, or topics. |
| `recommendation` | Optional derived/signed advisory from a trusted analyzer, never canonical truth by itself. |

These should remain events. Mutable convenience views are projections.

## Authority Model

Anticipation is only useful if it is safe.

- Rally may surface untrusted facts, but must label them.
- Rally may recommend actions, but recommendations are advisory unless policy
  grants the recommender authority.
- Bridge commands that inject context into agents, editors, shells, or files
  must declare a minimum trust threshold.
- Lessons and procedures require provenance, supersession, and confidence.
- Conflicting claims or decisions should be explicit arbitration problems, not
  hidden ranking choices.

The core rule:

> Durable facts drive projections. Trusted policy controls automation.

## Build Sequence

1. **Projection foundation.** Parse once, derive shared query state once, and
   make preflight/diagnose/query use the same read model.
2. **`rally context`.** Emit a bounded context brief from the projection:
   pending work, active claims, blockers, conflicts, recent changes, trust
   labels, and recommended next action.
3. **Agent profiles.** Let agents declare capabilities, current task, branch,
   and subscriptions.
4. **Task/artifact events.** Add explicit task lifecycle and artifact metadata
   without becoming a scheduler.
5. **Decision and lesson memory.** Store source-linked decisions, conventions,
   failure reflections, and reusable procedures with supersession.
6. **Attunement ranking.** Rank relevance by agent profile, touched paths,
   thread/dependency links, trust status, recency, and unresolved state.
7. **Adapters.** Expose the context brief through MCP, A2A bridge metadata,
   editor surfaces, and optional UI streams.

PR #34 starts at step 1. It makes one projection the core query substrate so
future intelligence can compound instead of becoming another parallel
interpretation of the trace.
