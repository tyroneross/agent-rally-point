<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# dynamic-workflows

A **portable, self-contained module** — workstream protocol + a zero-dependency descriptor linter
(guardrail) + host-facing skills (Claude, Codex) — that lets any agent coordinate a multi-agent
workstream through the `rally` CLI. **Rally facilitates; hosts execute.**

Drop this directory into any repo. No install required; no runtime dependencies.

---

## Contents

| Path | Purpose |
|---|---|
| `PROTOCOL.md` | Canonical spec: descriptor format, lint rules, spawn tiers, and agent loop |
| `core/workstream-lint.mjs` | Zero-dependency linter — validates a descriptor before fan-out (exits 0/1/2) |
| `core/limiter.mjs` | Bounded-concurrency helper hosts can use to cap their own Tier-1 fan-out |
| `skills/claude/SKILL.md` | Skill that maps a workstream onto rally primitives for Claude Code |
| `skills/codex/SKILL.md` | Same, for Codex |
| `examples/audit-repo.workstream.json` | Valid example: repo-audit workstream with three tasks |
| `examples/bad-missing-fields.workstream.json` | Invalid example: missing required fields (linter demo) |
| `examples/bad-nondeterministic.workstream.json` | Invalid example: non-deterministic validation command |
| `tests/workstream-lint.test.mjs` | 7 unit tests for the linter (Node built-in test runner) |
| `package.json` | Module manifest; no runtime dependencies |
| `NOTICE` | MIT attribution for the portions lifted from pi-dynamic-workflows |

---

## Quickstart

```bash
# Validate a descriptor (exit 0 = valid)
node core/workstream-lint.mjs examples/audit-repo.workstream.json

# Run the test suite (7 tests, no install needed)
npm test
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
