<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Agent Coordination Landscape

Research pass date: 2026-05-26.

This document benchmarks Rally against the broader agent coordination space.
The goal is ambition with discipline: Rally should become excellent at the
coordination substrate layer, not drift into being an agent runtime, IDE
protocol, workflow engine, tool protocol, or hosted broker.

## Executive Takeaway

The market is clustering around five layers:

1. **Tool/context access**: MCP.
2. **Agent-client/editor transport**: ACP and AG-UI.
3. **Agent-to-agent network tasks**: A2A.
4. **Agent runtime/orchestration**: OpenAI Agents SDK, LangGraph, CrewAI,
   AutoGen.
5. **Durability/observability substrates**: Temporal, OpenTelemetry,
   CloudEvents, local-first CRDT/event logs.

Rally should not compete head-on with any one layer. Its strongest lane is:

> A local-first, inspectable coordination ledger for coding agents, with
> signed portable events, deterministic derived state, and bridges to the
> surrounding protocols.

That gives Rally a sharper job than "multi-agent framework": it is the durable
coordination truth for independent agents that may be running in different
CLIs, worktrees, editors, shells, or machines.

The ambitious version is one step stronger:

> Rally should become a local intelligence layer that anticipates what each
> coding agent needs next from durable coordination facts, trust, ownership,
> task state, artifacts, and source-linked lessons.

This is not generic memory or orchestration. It is attuned, repo-native
situational awareness. See
[`ATTUNED_COORDINATION.md`](ATTUNED_COORDINATION.md).

## Landscape

| System | What it owns | What Rally should learn | What Rally should not copy |
|---|---|---|---|
| A2A | Network protocol for agent discovery, messages, tasks, artifacts, streaming, and push notifications. | Model remote handoffs as task-like lifecycle state and support bridges to A2A task/context IDs. | Do not require HTTP, servers, or live connections for local coordination. |
| MCP | Standard way for AI apps to access tools, data, prompts, and workflows. | Expose Rally state through MCP so agents can inspect inbox/diagnose/trust via existing clients. | Do not redefine tool execution or resource access. |
| ACP | Agent-client protocol for connecting coding agents to editors. | Treat ACP agents/editors as consumers and producers of Rally events. | Do not become an editor UI or terminal streaming protocol. |
| AG-UI | Event protocol between agent backends and user-facing frontends. | Future UI surfaces can render Rally events through AG-UI-style event streams. | Do not tie the core to frontend sessions. |
| OpenAI Agents SDK | Code-first orchestration with agents, handoffs, guardrails, sessions, tracing, tools, and sandbox execution. | Borrow the primitive set: handoff, guardrail, session, trace. Rally can be the cross-process coordination record below SDK runs. | Do not implement an agent loop or SDK-specific runtime. |
| LangGraph | Stateful graph/workflow runtime with persistence, streaming, interrupts, durable execution, memory, and orchestrator-worker patterns. | Adopt explicit derived state, replay, thread IDs, and interrupt/resume vocabulary where useful. | Do not become graph orchestration. Rally records what happened across agents; it does not schedule graph nodes. |
| CrewAI | Crews for autonomous collaboration; Flows for stateful event-driven control. | Separate "control flow" from "agent team" concepts; Rally should record both without owning either. | Do not become a role/task framework. |
| AutoGen | Conversable agents and group-chat-style multi-agent workflows. | Conversation is one coordination shape; keep message/handoff history inspectable. | Do not make chat the only coordination primitive. Coding agents coordinate through files, claims, blockers, commits, and reviews too. |
| Temporal | Durable execution, retries, workflow state, signals, timers, long-running reliability. | Learn from its durability semantics: replay, resume, explicit state, no lost progress. | Do not require a service or task queue. Rally is a ledger, not an execution engine. |
| OpenTelemetry | Cross-system traces, metrics, logs, GenAI semantic conventions. | Export or map Rally events into traces/spans for observability. | Do not make observability backends the source of truth. |
| CloudEvents | Common event envelope and formats across systems. | Continue CloudEvents-aligned envelopes for interoperability. | Do not overfit to cloud event buses when Rally's primary substrate is local JSONL. |
| Local-first CRDT/event systems | Offline-first sync and deterministic merge. | Use event identity and merge rules; consider CRDTs only for future shared mutable projections. | Do not add CRDT complexity for append-only coordination facts unless a real mutable state problem appears. |
| NATS JetStream / streaming brokers | Persistent streams, replay, consumers, replication. | Terminology and guarantees are useful for optional broker bridges. | Do not require a broker for Rally's core value. |
| Matrix | Federated event graph and eventual consistency. | Event graph thinking is relevant for remote causation and partial order. | Do not become a global federation service. |

## Source Notes

### A2A

A2A is the closest neighboring protocol for remote agent-to-agent work. Its
core unit is a `Task`, with status, artifacts, and optional message history. It
supports polling, streaming task updates, and push notifications. The spec also
warns that message streams are not reliable storage for critical information;
agents may persist important messages in task history, but clients should not
assume that without negotiation.

Rally implication: A2A is a transport/task protocol. Rally can bridge to it, but
Rally's value is the durable local/remote coordination record that survives
missed streams, tool restarts, and offline work.

Source: https://a2a-protocol.org/latest/specification/

### MCP

MCP is an open standard for connecting AI applications to external systems:
data sources, tools, and workflows. Its own docs frame it as a standardized
connection layer for clients and servers, with broad ecosystem support across
AI assistants and developer tools.

Rally implication: Rally should expose coordination state through MCP; it should
not become MCP. "List pending handoffs" and "read diagnosis" are good MCP
tools/resources. "Run arbitrary tool against user data" is not Rally's lane.

Source: https://modelcontextprotocol.io/docs/getting-started/intro

### ACP

ACP is an open standard for connecting coding agents to editing environments.
Zed describes it as bringing external agents into IDE surfaces while preserving
local/privacy-friendly execution.

Rally implication: ACP is an editor/agent transport. Rally can be the substrate
that lets multiple ACP-connected agents coordinate across one repo.

Source: https://zed.dev/acp

### OpenAI Agents SDK

OpenAI positions the Agents SDK for code-first orchestration when an
application needs agents, tools, handoffs, guardrails, tracing, or sandbox
execution. Its core primitives include Agent, Handoff, Guardrail, and Session.

Rally implication: those are good product primitives, but Rally should sit
under or beside SDKs as the cross-process coordination log. A Rally handoff is
not just an in-memory ownership transfer; it is an auditable fact other tools
can discover.

Sources:

- https://developers.openai.com/api/docs/libraries#use-the-agents-sdk
- https://developers.openai.com/tracks/building-agents#foundations-of-the-agents-sdk

### LangGraph

LangGraph distinguishes workflows with predetermined paths from agents that
choose process/tool usage dynamically. It offers persistence, streaming,
debugging, deployment, durable execution, and state checkpointing organized by
threads. It also documents orchestrator-worker patterns for dynamically
creating workers and aggregating outputs.

Rally implication: Rally should steal the seriousness about threads,
checkpoints, replay, and resume semantics, while refusing to become a graph
runtime. Rally's equivalent of "state" is derived from an event log, not owned
by node execution.

Sources:

- https://docs.langchain.com/oss/python/langgraph/workflows-agents
- https://docs.langchain.com/oss/python/langgraph/persistence
- https://docs.langchain.com/oss/python/langgraph/durable-execution

### CrewAI

CrewAI separates Flows from Crews: Flows manage state, events, and control
logic; Crews are teams of agents delegated complex work. Their docs recommend
starting production apps with a Flow and invoking Crews where autonomy is
needed.

Rally implication: this validates a split between deterministic coordination
state and autonomous agent work. Rally should own the former and record the
latter.

Source: https://docs.crewai.com/en/introduction

### AutoGen

AutoGen popularized multi-agent conversation as a programming model:
customizable conversable agents that integrate LLMs, tools, humans, and
automated chat.

Rally implication: conversation is important, but coding-agent coordination is
broader than conversation. Claims, blockers, review verdicts, commits,
dependencies, and trust state deserve first-class events.

Source: https://microsoft.github.io/autogen/0.2/docs/Use-Cases/agent_chat/

### Temporal

Temporal owns durable execution: persistent workflow state, retries, signals,
timers, and recovery after crashes. It explicitly targets agents and long
running workflows.

Rally implication: do not rebuild Temporal. But do borrow the discipline:
explicit state, replayability, no lost progress, and clear failure recovery.
Rally achieves a much smaller version through an append-only log and derived
state.

Source: https://temporal.io/

### OpenTelemetry

OpenTelemetry has GenAI semantic conventions in development, including model
spans, agent/framework spans, metrics, events, exceptions, and MCP-related
semantics.

Rally implication: Rally should eventually export coordination events as trace
data or include trace/span IDs in events. But observability is derived from the
coordination log, not vice versa.

Source: https://opentelemetry.io/docs/specs/semconv/gen-ai/

### CloudEvents

CloudEvents standardizes event data across services and platforms and includes
multiple protocol/event format bindings. It is CNCF graduated and has SDKs
across languages, including Rust and Python.

Rally implication: stay CloudEvents-aligned. It makes Rally events easier to
bridge into external event systems without letting cloud event buses define the
core.

Source: https://github.com/cloudevents/spec

### Local-first Sync, CRDTs, and Event Streams

Automerge and Yjs show the mature local-first/CRDT direction: concurrent
changes can merge without a central server. Matrix shows a federated event graph
model. NATS JetStream shows persistent streams, replay, consumers, and
replication from the broker world.

Rally implication: append-only event facts probably do not need CRDTs. The hard
parts are identity, causation, duplicate/conflict handling, trust, and derived
state. CRDTs become relevant only if Rally later adds mutable collaborative
documents or shared live state beyond the event log.

Sources:

- https://automerge.org/
- https://docs.yjs.dev/
- https://spec.matrix.org/
- https://docs.nats.io/nats-concepts/jetstream

## Strategic Positioning

Rally should describe itself as:

> A local-first coordination ledger for coding agents.

More expansive:

> Rally records what independent coding agents are doing, what they need from
> each other, what work is claimed or blocked, and which events are trusted
> enough to drive automation.

Not:

- "An agent framework."
- "An agent runtime."
- "A task queue."
- "A decentralized chat protocol."
- "A replacement for MCP/A2A/ACP."

## Product Implications

The greenfield architecture should be revised around these ideas:

1. **Interop first-class.** Define mappings to A2A task/context IDs, ACP
   sessions, MCP resources/tools, OpenTelemetry trace/span IDs, and
   CloudEvents fields.
2. **Trust before remote automation.** Remote imports are visible immediately
   but automation-authority depends on signature and policy.
3. **Derived state, not mutable state.** Inbox, claims, blockers, diagnosis,
   and trust summaries are projections over the log.
4. **Attuned context over raw logs.** Agents should receive a bounded,
   source-linked context brief that is ranked for their current tool, task,
   files, trust policy, and unresolved obligations.
5. **JSON contracts over prose.** Every command that agents consume needs a
   stable schema.
6. **Adapters at the edge.** Herdr, ACP, MCP, A2A, AG-UI, and OTel are adapters,
   not the kernel.
7. **No daemon requirement.** Push/streaming can be added, but the core must
   work through file append and replay.
8. **Local-first sync semantics.** Import/export packets should be transport
   neutral. Files, Git, shared folders, A2A, or a future service can carry them.

## Architecture Doc Changes Needed

The Rust greenfield architecture should add:

- A dedicated "Protocol Interop" section.
- A "Rally owns / Rally bridges / Rally refuses" boundary table.
- A2A mapping: Rally `handoff` and `ack` to A2A `Task`, `TaskStatus`, and
  artifacts.
- MCP mapping: Rally inbox/diagnose/trust as tools/resources.
- ACP mapping: editor-connected coding agents as Rally producers/consumers.
- OTel mapping: Rally event IDs as trace links or span attributes.
- Explicit statement that Rally is below orchestration frameworks, not a
  competitor to them.
