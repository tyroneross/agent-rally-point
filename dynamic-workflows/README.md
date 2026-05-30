<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Rally Flow

> **Rally Flow** — Agent Rally Point's take on dynamic workflows. The host-side workstream protocol
> + guardrail + skills + durable resume that sits on rally. *Rally facilitates; hosts execute.*
> (Implemented in this `dynamic-workflows/` directory.)

A **portable, self-contained module** — workstream protocol + a zero-dependency descriptor linter
(guardrail) + host-facing skills (Claude, Codex) — that lets any agent coordinate a multi-agent
workstream through the `rally` CLI. **Rally facilitates; hosts execute.**

Drop this directory into any repo. No install required; no runtime dependencies.

---

## Contents

| Path | Purpose |
|---|---|
| `PROTOCOL.md` | Canonical spec: descriptor format, lint rules, spawn tiers, the agent loop, and **durable fan-out & resume** |
| `COORDINATION.md` | Frontier-agent coordination doctrine — two modes, the rules (first-agent-is-lead, proactive engagement, instruction contract), rally-facilitates-not-coordinates |
| `MODEL-TIERS.md` | Host-neutral model-tier taxonomy (frontier/executing/fast) + the empirical A/B verdict |
| `core/workstream-lint.mjs` | Zero-dependency linter — validates a descriptor before fan-out (exits 0/1/2) |
| `core/workstream-status.mjs` | **Resume helper** — derives done/claimed/pending + the `to_dispatch` set from a `rally room` snapshot (the durable counterpart to pi's in-memory progress) |
| `core/route.mjs` | **Deterministic routing** (ported host-neutral from pi): `parallel`/`pipeline`/`budget` + onError/abort failure-visibility |
| `core/limiter.mjs` | Bounded-concurrency helper hosts can use to cap their own Tier-1 fan-out |
| [`../skills/rally-workflows/SKILL.md`](../skills/rally-workflows/SKILL.md) | Host-neutral Rally Flow skill (moved out of this module) mapping a workstream onto rally primitives; references `PROTOCOL.md` |
| `examples/*.workstream.json` | One valid + two invalid descriptors (linter demos) |
| `tests/*.test.mjs` | Tests across lint / status / route / limiter (Node built-in runner) — run `npm test` |
| `package.json` | Module manifest; no runtime dependencies (exports: lint/status/route/limiter) |
| `NOTICE` | MIT attribution for the portions lifted from pi-dynamic-workflows |

---

## Quickstart

```bash
# Validate a descriptor (exit 0 = valid)
node core/workstream-lint.mjs examples/audit-repo.workstream.json

# Run the test suite (no install needed)
npm test

# Resume a long-running workstream — what's left to dispatch?
rally room --json > room.json && node core/workstream-status.mjs my.workstream.json room.json
```

No `npm install` required — `"dependencies": {}`.

---

## Lifted vs dropped

This module adapted three pieces from
[pi-dynamic-workflows](https://github.com/Michaelliv/pi-dynamic-workflows) (MIT,
via [tyroneross/pi-dynamic-workflows-fork](https://github.com/tyroneross/pi-dynamic-workflows-fork)):

**Lifted:**
- The `DETERMINISM_BLOCKLIST` regex — rejects `Date.now()`, `Math.random()`, `new Date()` in
  declared commands so a shared plan is reproducible across agents.
- The literal-descriptor validation discipline (`evaluateLiteral`/`validateMeta` pattern) — every
  required field is validated as a literal JSON value, never executed.
- The `createLimiter` bounded-concurrency helper (`core/limiter.mjs`) — lets a host cap its own
  Tier-1 fan-out without pulling in a concurrency library.

**Dropped:**
- The `node:vm` script executor.
- The in-memory subagent runtime.
- The `agent()`/`parallel()`/`pipeline()` execution primitives.
- Pi SDK and TUI plumbing.

**Why:** Rally is a coordination facilitator, never an executor. Keeping execution machinery here
would blur the boundary that makes the module safe to embed in any host. See `PROTOCOL.md §5`.

---

## Cargo note

This is a Node.js module living at the repo root alongside a Rust workspace. The Rust workspace
uses explicit `[workspace] members = [...]`, so `cargo build` ignores this directory. Do **not**
add a `Cargo.toml` here.
