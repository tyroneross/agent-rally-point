<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# North Star — Agent Rally Point

> Human-facing mirror of the durable product vision. The machine-read source of truth (loaded by
> build-loop Phase 1) is `build-loop-memory/projects/agent-rally-point/constitution.md`. Edit both
> together, and only on a deliberate vision shift. Last reviewed: 2026-06-04.

## The one line

**Coordinate thousands of AI coding agents across many terminals on one codebase — trustworthily,
losslessly, and without a human referee.**

## What it is

A per-repo, no-server "rally point." Agents from any host (Claude, Codex, …) announce presence, claim
work, hand off, and read-back through a shared per-repo `.rally/` channel — so a fleet works the same
code without clobbering each other and without a human deconflicting.

## Charter — facilitator, never executor

Rally **records and advises; it never gates, grants, schedules, spawns, retries, or executes.** It
derives state (presence, claims, handoffs, the DAG, wake-due) from facts and exposes it; the host runs
the work. A feature that gates or executes work is off-charter.

## Invariants (non-negotiable)

1. **Zero data loss** — the append-only JSONL ledger (`.rally/log/` + archive) is canonical.
2. **Derived caches are disposable** — `facts.db` and `.rally/.reconcile-cache.json` rebuild from the
   ledger; corruption is a non-event, not data loss.
3. **One owner per path, one store per repo** — collisions WARN + record an audit fact; work is never
   blocked, mistakes stay fixable.
4. **Trustworthy results** — room-stamped, fail-loud, read-back-verified, liveness-aware.
5. **Host-neutral** — one substrate for every coding host; the host's LLM interprets.

## Scale

Durable and correct at **thousands of agents and many terminals**. Public claims
must follow measured behavior in the current implementation and its tests.

## Non-goals

Not an orchestrator/scheduler/executor (that's the host) · not a server/cloud service (per-repo,
local-first) · not a human-in-the-loop referee (removing that referee is the point).

## Ecosystem

**Easy Terminal** is the host that launches, hosts, and observes the fleet Rally coordinates underneath.
