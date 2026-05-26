<!-- build-loop@tyroneross:canary:agent-rally-point -->
<!-- canary-end -->
# Agent Rally Point

> **Local-first coordination point for coding agents working in the same repo: presence, handoffs, verifier gates, conflicts, and closeout without a server.**

The target user-facing CLI is **`rally`**, implemented by the Rust core. The
older Python package remains in the tree only as legacy cutover material while
the greenfield Rust surface takes over.

Two or more AI agent CLIs (Claude Code, Codex, peer Claude sessions, CI verifiers) coordinating on a shared git repo with a human in the loop. No HTTP service. No broker. Just files + git for repo identity.

## Status

**Greenfield Rust rewrite in progress** (2026-05-26). Rust owns the target
coordination kernel, command envelopes, trust model, sync packet flow, and
agent-start preflight UX. Legacy behavior is not the acceptance oracle for new
work.

**v0.3.0 — canonical substrate** (2026-05-24). Adds: canonical channel layout at `~/.agent-rally-point/apps/<repo_id>/`, three-mode policy (canonical/migration/legacy-only), versioned discover envelope (`protocol_version: "1.0"`), repo_id normalization (worktree-stable + clone-stable), legacy → canonical migration tool with 4-condition cutover verifier, long-running presence-watcher with parent-liveness check.

Earlier: v0.2.x added the discovery layer + manifest. v0.1.0 (2026-05-20) extracted channel primitives from [build-loop](https://github.com/tyroneross/build-loop) v0.12.8.

## What it does

- **Presence**: `rally preflight --start-ping` writes ephemeral liveness under
  `rally/presence/`; peers see each other without making liveness part of the
  durable event log.
- **Channel**: append-only event log (`changes.jsonl`) with strict store-entry
  validation, local sequence numbers, event hashes, and previous-entry hashes.
- **Typed writes**: `handoff`, `ack`, `claim`, `blocker`, and lifecycle commands
  write through Rust event builders.
- **Read projections**: `inbox`, `claims`, `blockers`, `conflicts`, `diagnose`,
  `score`, `thread`, `replay`, and `report` derive state from one Rust query
  engine.
- **Trust and sync**: `identity`, signed writes, `verify`, and `sync
  export/import` move portable signed events between local channels.
- **Rust command surface**: `rally` owns the target command contracts for
  verify, typed writes, read projections, signed import/export, and preflight.

## Install

Install the Rust CLI from the checkout:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally preflight --channel-dir /tmp/rally-smoke --tool codex --json
```

During the greenfield rewrite, commands take an explicit `--channel-dir`:

```bash
rally preflight --channel-dir ~/.agent-rally-point/apps/<repo_id> --tool codex --start-ping --json
```

Repo/channel discovery is the next Rust cutover target; until that lands,
callers pass the channel directory directly.

## Greenfield Verification

Rust is the acceptance path:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```

Do not use legacy compatibility gates for Rust-core work. The older package
surface is cutover material and should not define greenfield behavior.

## Session-start integration

Every AI coding agent should run **one line** at session start. The preflight
reads the channel, surfaces pending ACKs + live peers, and returns a routing
decision (`join_active` vs `proceed_solo`).

```bash
rally preflight --channel-dir <dir> --tool <host> --start-ping --json
```

Replace `<host>` with one of `claude_code`, `codex`, `cursor`, `gemini`, `ci`.
`--start-ping` writes a presence record so peers see this session immediately.

### Claude Code (plugin / `.claude-plugin/hooks/session-start.sh`)

```bash
#!/usr/bin/env bash
# .claude-plugin/hooks/session-start.sh
rally preflight --channel-dir "$AGENT_RALLY_CHANNEL_DIR" --tool claude_code --start-ping --json
```

The hook runs once per session; the JSON or human-readable summary becomes context the model reads immediately.

### Codex (project `AGENTS.md`)

Add a single line near the top of `AGENTS.md`:

```markdown
## Session start
Run `rally preflight --channel-dir <dir> --tool codex --start-ping --json` and read the output before any tool call.
```

### Cursor / Gemini

Same shape — run the executable at session start with the appropriate `--tool` value. Any host that loads a session-bootstrap file (custom-instructions / system-prompt / pre-session script) can call the CLI directly.

### Sample output (human mode)

```json
{
  "ok": true,
  "command": "preflight",
  "schema": "agent-rally.command.preflight.v1",
  "coordination_status": "idle",
  "routing": {
    "action": "proceed_solo",
    "reason": "no pending acknowledgements or active peers"
  },
  "pending_acks_for_me": [],
  "active_peers": []
}
```

### Behavior

- **Idle**: no peers, no pending ACKs → `routing.action: proceed_solo`. The session can run normally; the preflight writes its own presence so peers will see it on their next check.
- **Coordinated**: pending ACK or live peer detected → `routing.action: join_active`. The session should handle the inbox or coordinate with peers before parallel work.
- **Degraded**: substrate unreachable → exit code 1 with JSON envelope still emitted (just with `channel_dir: null`). Host LLM proceeds without coordination but isn't misled.

## How this differs from A2A / MCP / LangGraph / CrewAI / Temporal

Most agent coordination today is **service-mediated** (A2A's HTTP+JSON-RPC), **in-process** (LangGraph state machines, CrewAI crews), or **broker-backed** (Temporal). Agent Rally Point is **filesystem-mediated** — for the case where two-or-more agent CLIs share a dev box (or shared FS) and need durable handoff signals without infrastructure.

Use Agent Rally Point when:
- You have 2+ agent CLIs (Claude Code, Codex, peer Claude) on the same repo
- You want zero-setup coordination (no service, no broker)
- You want the discipline patterns (gating verdicts, MECE handoffs, release-surface verification) as shipped tooling

Use A2A / MCP / LangGraph / CrewAI / Temporal when:
- Cross-vendor over the network → A2A
- Agent-to-tool wiring → MCP
- Single-runtime state machines → LangGraph
- Role-based agent teams in one Python process → CrewAI
- Long-running production workflows with retries → Temporal

These are complements, not competitors.

## Constitution

The binding rules of coordination (operating rule, MECE packets, post() canonical, release-surface verification, closeout hygiene) ship as in-package docs in v0.3. For now, see the extraction-source research packet at [`tyroneross/dev/research/topics/multi-peer-coordination-standalone-extraction/`](https://github.com/tyroneross) (private) or the build-loop predecessor at [`build-loop/references/coordination-rules.md`](https://github.com/tyroneross/build-loop/blob/main/references/coordination-rules.md).

## Trademark disclaimer

**Agent Rally Point is not affiliated with Rally Point IT, LLC** or any other entity holding marks containing "Rally Point." Agent Rally Point is an open-source developer tool for AI agent coordination; Rally Point IT, LLC is an IT services / infrastructure consultancy. The composite name uses the military-origin "rally point" metaphor (a known place where independent units regroup with status).

## License

Apache-2.0. See [LICENSE](./LICENSE).

## Provenance

Channel primitives copied verbatim from [build-loop](https://github.com/tyroneross/build-loop) `scripts/app_pulse/` at v0.12.8 (commit `10224c2`). Build-loop will become a downstream consumer of `agent-rally-point` once v1.0 ships; channel root migrates from `~/.build-loop/apps/` to `~/.agent-rally-point/apps/` at that cutover.

## License & Attribution

This project is licensed under the [Apache License 2.0](LICENSE).

- [`LICENSE`](LICENSE) — full license text.
- [`NOTICE`](NOTICE) — attribution notices that, per Apache 2.0 §4(d), must travel with any redistribution of this work.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution conventions: per-file SPDX headers (REUSE 3.3), AI co-author trailer, signed commits, conventional commits.

Per-file `SPDX-FileCopyrightText` and `SPDX-License-Identifier` headers are required on shipped source files. Files that cannot carry inline comments (JSON, generated assets) are annotated in [`REUSE.toml`](REUSE.toml). Validate compliance locally with `uvx reuse lint`.
