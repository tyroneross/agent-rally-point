<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Agent Rally Point — Architecture

> **Role: protocol-of-record for agent-to-agent coordination via append-only JSONL stream.**
> Substrate only. Daemons live in [`agent-rally-watcher`](https://github.com/tyroneross/agent-rally-watcher). Consumers (build-loop, codex, claude_code, custom tools) post events into and read events out of channels owned by this package.

## Three-layer model

```
┌─────────────────────────────────────────────────────────────────────┐
│  Layer 3 — CONSUMERS (build-loop, codex, claude_code, custom tools) │
│    post() events · checkpoint_read() deltas · inbox/<tool>.jsonl    │
└────────────────────────────▲─────────────────────▲──────────────────┘
                             │                     │
              read filtered  │     write events    │
                             │                     │
┌────────────────────────────┴──────────┐ ┌────────┴──────────────────┐
│  Layer 2 — DAEMON (agent-rally-watcher)│ │  Layer 1.5 — DISCOVERY    │
│    kqueue/inotify tail of channel ·    │ │    manifest.toml resolver │
│    consumers.toml filter rules ·       │ │    `agent-rally discover` │
│    per-consumer cursor · sinks         │ │    `discover()` API       │
└────────────────────────────▲───────────┘ └────────▲──────────────────┘
                             │                      │
            tails             │       publishes      │
                             │                      │
┌────────────────────────────┴──────────────────────┴──────────────────┐
│  Layer 1 — SUBSTRATE (this repo, agent-rally-point)                  │
│    channel layout · changes.jsonl append-only · revision counter ·   │
│    presence/heartbeat · checkpoint_read · post · lifecycle reapers   │
└──────────────────────────────────────────────────────────────────────┘
```

**Layer 1 (this repo)** is the only writer of the canonical channel files. It defines the record schema, owns atomicity (O_APPEND single-write + tmp+rename), and exposes both a Python API and a CLI.

**Layer 2 ([agent-rally-watcher](https://github.com/tyroneross/agent-rally-watcher))** is a long-running daemon that reads the channel via kqueue/inotify and pushes filtered events to per-consumer sinks. It never writes the channel itself; it only consumes and dispatches.

**Layer 3 (consumers)** is any tool that participates — build-loop's orchestrator, the codex CLI, claude_code sessions, custom integrations. Consumers call `post(...)` to publish and either (a) call `checkpoint_read(...)` directly for cheap inline polling or (b) subscribe via the watcher daemon for push.

## Data flow

```
producer ─[1]─▶ append_change(changes.jsonl)
producer ─[2]─▶ bump_revision(revision)        ← atomic increment under fcntl lock

       ─[3]─▶  watcher reads kqueue/inotify edge on changes.jsonl mtime
              filter by consumers.toml rules per consumer
       ─[4]─▶  sink: file append (per-tool inbox), notify, HTTP

       ─[3']─▶ consumer polls checkpoint_read(cwd, session_id)
              compares current revision to session cursor
              returns delta + reactions ({reinstall, re-baseline, soft-claim})
              advances ONLY this session's cursor
```

The `post()` helper (`agent_rally_point.post`) is the canonical writer: it bumps the revision *before* appending the record so that any reader who sees the new revision can always find its corresponding line. Callers that use `append_change` directly without bumping leave the channel in a silent-no-op state (peers never notice). See [`docs/SCHEMA.md`](docs/SCHEMA.md) for the record format.

## Channel layout

One channel per canonical repo, rooted at `~/.agent-rally-point/apps/<slug>/`. Slug is derived from `git rev-parse --git-common-dir` so a clone, the main checkout, and every `git worktree` of the same repo all resolve to the same channel (see `agent_rally_point/channel_paths.py`).

| Path                            | Purpose                                      | Writer            | Reader               |
|---------------------------------|----------------------------------------------|-------------------|----------------------|
| `changes.jsonl`                 | Append-only event stream                     | any consumer      | any consumer, watcher|
| `revision`                      | Monotonic counter (cheap "did anything change") | any consumer   | any consumer         |
| `revision.lock`                 | fcntl lock for revision bumps                | revision module   | n/a                  |
| `sessions/<session-id>.json`    | Per-session presence + read cursor           | the owning session| reaper, checkpoint   |
| `inbox/<tool>.jsonl`            | Per-tool addressed messages (push target)    | watcher           | the target tool      |
| `inbox/all.jsonl`               | Broadcast inbox (all tools)                  | watcher           | every tool           |
| `rally/current.json`            | Optional rally pointer (active topic)        | rally subcommand  | any consumer         |
| `watchers/<pid>.json`           | Daemon registration (one per watcher)        | watcher           | watcher              |
| `arch/digest.json`              | Optional architecture digest (opt-in)        | architecture-scout| checkpoint reactions |

Legacy path: channels also live at `~/.build-loop/apps/<slug>/` for sessions that predate the standalone package. The discovery layer transparently falls back to the legacy location when the canonical path is absent. See [`docs/DISCOVERY.md`](docs/DISCOVERY.md).

## Record schema

Every line in `changes.jsonl` is one self-contained JSON record. Schema and known kinds (`commit`, `dep-change`, `phase`, `arch-scan-complete`, `feedback`, `handoff`) are documented in [`docs/SCHEMA.md`](docs/SCHEMA.md). Unknown kinds are kept-and-warned (D7), never dropped.

## Discovery

Sibling tools (build-loop, agent-rally-watcher, codex) discover the channel layout and active state without hardcoding paths via the discovery layer documented in [`docs/DISCOVERY.md`](docs/DISCOVERY.md):

- Manifest at `~/.agent-rally-point/manifest.toml` (global, auto-generated on first install).
- Optional `.agent-rally.toml` at repo root (per-repo overlay, opt-in).
- `agent-rally-discover` CLI (or `python3 -m agent_rally_point.discover`) returns the resolved layout as JSON.
- `from agent_rally_point.discover import discover` Python API returns the same dict.
- Resolution order: repo-level overlay → global manifest → legacy `~/.build-loop/apps/<slug>/` fallback.

## Cross-references

- **Watcher daemon**: [`agent-rally-watcher/ARCHITECTURE.md`](https://github.com/tyroneross/agent-rally-watcher/blob/main/ARCHITECTURE.md)
- **Build-loop's usage**: [`build-loop/skills/build-loop/references/coordination.md`](https://github.com/tyroneross/build-loop/blob/main/skills/build-loop/references/coordination.md)
- **Record schema**: [`docs/SCHEMA.md`](docs/SCHEMA.md)
- **Discovery protocol**: [`docs/DISCOVERY.md`](docs/DISCOVERY.md)

## Design invariants

| Invariant | Why |
|-----------|-----|
| Readers never lock | A slow/dead reader must never block a producer. |
| O_APPEND single-write up to PIPE_BUF is atomic | No torn lines across concurrent writers; matches POSIX guarantee. |
| The log is immutable | No rewrite/delete/truncate API — only `append_change` and `read_changes_since`. Audit replay is always possible. |
| Soft-claim is awareness, not a lock | Peer-file-overlap warns; never blocks. Consumers decide whether to mutate. |
| Worktree-independent slug | Same channel from main checkout, clone, and every git worktree of the same canonical repo. |
| Fire-and-forget writes | A coordination failure must never crash a host action. |
