<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Agent Rally Point North Star

**Make parallel coding agents accountable to the same record of work, even when they run in different terminals or harnesses.**

An agent joining a busy repository should be able to answer who owns the work, what needs its response, and what evidence supports a result. The person directing the work should be able to inspect those answers without relaying messages between terminals.

## What Rally owns

- **A shared repository room.** Agents share context, communicate, and review in parallel. Linked Git worktrees share a room while keeping edits isolated. The local append-only ledger retains coordination history, and derived indexes can be rebuilt from it.
- **Narrow ownership boundaries.** Each piece of work has one accountable owner. Exclusive claims protect specific resources when an effect needs serialization. Rally refuses conflicting exclusive claims. Edit checks are advisory by default, with strict enforcement where the host or caller supports it.
- **Handoffs that can be followed to a result.** Queuing, transport delivery, receiver acknowledgement, acceptance of ownership, completion, and verification each require their own evidence. A missed delivery remains visible as pending work the recipient can pick up from the room.
- **Host-neutral coordination and explicit runtime controls.** The CLI works independently of a model provider. Host hooks can register sessions and check edits. Rally can launch or adopt managed sessions, route prompts, and serve the local store through an optional coordinator.

The lead or operator sets the goal, decides who does what, and owns convergence, integration, and publication. Agent harnesses perform the work. Rally Flow can validate a plan and report which tasks are ready. Launch, routing, and admission controls act on explicit requests and configured policy.

## Where the product should grow

A busy room should surface the work and decisions relevant to the current agent while keeping the complete record available for revalidation. Coordination should survive restarts and handoffs, and operators should be able to establish what happened from the record when a route or agent fails.

Broader host support and larger rooms are design targets. Claims about a given host, delivery route, or fleet size depend on evidence from that setup. The current trust model covers trusted local agents under one operator. Easy Terminal is one host for managed sessions and delivery. Other hosts can use the same room protocol.

[Current product and setup](README.md) · [Architecture](docs/RALLY_ARCHITECTURE.md) · [Protocol design](docs/PROTOCOL-NORTH-STAR.md) · [Trust model](docs/security/TRUST-MODEL.md)
