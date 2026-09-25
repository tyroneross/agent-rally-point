<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Rally Flow

Rally Flow validates multi-agent workstream plans and reports which tasks are ready to run. It reads Agent Rally Point records to distinguish completed, claimed, active and pending work. Your agent harness launches workers and performs the work.

The npm package name is **`@tyroneross/rally-flow`**. The unscoped `dynamic-workflows` package on npm belongs to a different project. This repository directory keeps its existing name.

Requires **Node.js 22.22.0 or newer**. The module has no third-party runtime dependencies. Install the separate [Rally CLI](https://github.com/tyroneross/agent-rally-point#install-with-a-coding-agent-recommended) when you need live room snapshots; linting a descriptor needs only Node.

**The linter is not a security boundary.** It checks structure, determinism, MECE write boundaries,
dependency integrity, and the charset of anything rendered into a command. It does not read your
code, sandbox anything, or judge intent. A clean lint does not mean a descriptor is safe to run —
a descriptor from an author you do not trust is untrusted input. `PROTOCOL.md §1b` states the limits
in full.

---

## Contents

| Path | Purpose |
|---|---|
| `PROTOCOL.md` | Canonical spec: descriptor format, lint rules, spawn tiers, the agent loop, and **durable fan-out & resume** |
| `COORDINATION.md` | Frontier-agent coordination doctrine — two modes, the rules (first-agent-is-lead, proactive engagement, instruction contract), rally-facilitates-not-coordinates |
| `MODEL-TIERS.md` | Host-neutral model-tier taxonomy (frontier/executing/fast) + the empirical A/B verdict |
| `core/workstream-lint.mjs` | Zero-dependency linter — structure, determinism, MECE boundaries, dependency integrity, and command-charset limits (exits 0/1/2). Also holds `VALIDATION_RECIPES`, the local registry of named commands a descriptor may ask for by name |
| `core/packet.mjs` | Renders a ready-to-paste prompt packet per task. Shell-quotes every value; only the rally loop and a named recipe reach a ```` ```bash ```` block |
| `core/workstream-status.mjs` | **Resume helper** — derives done/claimed/read-active/pending + the `to_dispatch` set from a `rally room` snapshot without treating nonexclusive reads as claims |
| `core/route.mjs` | **Deterministic routing** (ported host-neutral from pi): `parallel`/`pipeline`/`budget` + onError/abort failure-visibility |
| `core/limiter.mjs` | Bounded-concurrency helper hosts can use to cap their own Tier-1 fan-out |
| `core/fanout.mjs` | **Fan-out resolver** — `resolveFanout()` returns the width *and* the constraint that bound it (default 10, hard ceiling 12). Feed its `effective_max` to `createLimiter` |
| [`../skills/rally-workflows/SKILL.md`](https://github.com/tyroneross/agent-rally-point/blob/main/skills/rally-workflows/SKILL.md) | Host-neutral Rally Flow skill (moved out of this module) mapping a workstream onto rally primitives; references `PROTOCOL.md` |
| `examples/*.workstream.json` | One valid + two invalid descriptors (linter demos) |
| `tests/*.test.mjs` | Tests across lint / packet / status / route / limiter (Node built-in runner) — run `npm test`. `tests/injection.test.mjs` is the adversarial suite: every test is an attack on the linter or the renderer |
| `package.json` | Module manifest; exports `lint`, `status`, `route`, `limiter` and `fanout` |
| `LICENSE` | Apache-2.0 license for this module |
| `NOTICE` | MIT attribution for the portions lifted from pi-dynamic-workflows |

---

## Try the source checkout

Run these commands from `dynamic-workflows/` in the Rally repository:

```bash
# Exit 0 and a "valid" result mean the descriptor passes lint.
node core/workstream-lint.mjs examples/audit-repo.workstream.json

# Source tests, including installation of the packed artifact in a temporary consumer.
npm test

# Read the room and derive the next dispatch set.
rally room --json > room.json
node core/workstream-status.mjs my.workstream.json room.json --tool-prefix agent
```

The status command prints JSON with `to_dispatch`. It exits **0** when the workstream is complete, **3** when work remains, and **2** for invalid arguments or unreadable input. Exit 3 is an expected work-in-progress result.

## Install a local package

Registry publication is pending. To try this checkout's package, create a tarball from `dynamic-workflows/`:

```bash
npm pack
```

Install the resulting `tyroneross-rally-flow-0.1.0.tgz` into a separate project:

```bash
npm install /absolute/path/to/tyroneross-rally-flow-0.1.0.tgz
npx --no-install workstream-lint node_modules/@tyroneross/rally-flow/examples/audit-repo.workstream.json
```

For JavaScript consumers, use a named subpath:

```js
import { lintWorkstream } from "@tyroneross/rally-flow/lint";
import { workstreamStatus } from "@tyroneross/rally-flow/status";
```

The package includes the core tools, examples, protocol documents, LICENSE and NOTICE. The host skill and source tests remain in the Git repository. Installing the package does not install Rally, register host hooks, start services or launch agents.

Maintainers should run `npm publish --dry-run` from the source directory before a release. The `prepublishOnly` lifecycle runs `npm test`, including the packed-consumer check. Publishing a tarball directly or disabling scripts bypasses that lifecycle; this is a release check, not a security boundary.

Use the same tool prefix passed to `packet.mjs` (default `agent`). Until the combined O33-A+B+C
activation, read-active resume is a documented exact active-squad/task-tool heuristic; it is not a
run-scoped liveness proof and does not create ownership.

`owns: "read-only"` prohibits intentional changes to task/domain resources, not the generated Rally
coordination records or ordinary transient tool state created by verification. Neither exception
creates task ownership or counts as task output.

The source tools run directly without `npm install`; the npm package installs only this module.

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
would blur the boundary this module is built around: it emits text, a host decides what to run.
See `PROTOCOL.md §5`.

---

## Cargo note

This is a Node.js module living at the repo root alongside a Rust workspace. The Rust workspace
uses explicit `[workspace] members = [...]`, so `cargo build` ignores this directory. Do **not**
add a `Cargo.toml` here.
