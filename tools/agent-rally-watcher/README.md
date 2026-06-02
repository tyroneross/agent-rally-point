<!-- build-loop@tyroneross:canary:agent-rally-watcher -->
<!-- canary-end -->
# Agent Rally Watcher

> **Daemon companion to [agent-rally-point](https://github.com/tyroneross/agent-rally-point): push-based watcher for Rally Point streams, with per-consumer filtering and dispatch.**

Rally Point is the substrate (event posting, presence, channels). Rally Watcher is the long-running listener that tails the channel's `changes.jsonl` and pushes filtered events to the tools that need them (Claude Code sessions, Codex CLI, terminals, hosted endpoints). Same architectural pattern as build-loop ↔ build-loop-monitor — substrate vs daemon, separated by concern.

## Status

**v0.1.1 — alpha** (2026-05-23). Push-based via [`watchfiles`](https://watchfiles.helpmanual.io/) (kqueue on macOS, inotify on Linux). Per-consumer cursor persistence. stdout + macOS notification dispatch. HTTP POST dispatch stubbed for v0.2. **Zero non-stdlib deps beyond `watchfiles`** — config is TOML (`tomllib`).

## What it does

- **Push-based tail** of `~/.agent-rally-point/apps/<slug>/changes.jsonl` (sub-second on kqueue/inotify; no polling).
- **Per-consumer filtering**: `consumers.toml` maps tool-id (`claude_code`, `codex`, ...) to filter rules (kinds, senders, payload-field matches).
- **Cursor persistence**: each consumer has a cursor at `~/.agent-rally-watcher/consumers/<tool>.cursor` so a daemon restart resumes mid-stream without dupes or gaps.
- **Dispatch sinks (v0.1)**: append-to-file (stdout-pipe target), macOS `osascript` notify. HTTP POST stubbed.
- **Daemon lifecycle**: `agent-rally-watcher start | stop | status | reload`. PID file + size-bounded log rotation at `~/.agent-rally-watcher/logs/daemon.log`.
- **macOS launchd**: plist generator (`agent-rally-watcher install-launchd`) for boot-time autostart. Linux/systemd planned.

## Install

```bash
uv pip install agent-rally-watcher   # or: pip install agent-rally-watcher
```

PyPI publication pending. Install from source until then:

```bash
git clone https://github.com/tyroneross/agent-rally-watcher.git
cd agent-rally-watcher
uv pip install -e .
```

## Quickstart

```bash
# 1. Author a consumers config
cp examples/consumers.toml ~/.agent-rally-watcher/consumers.toml
$EDITOR ~/.agent-rally-watcher/consumers.toml

# 2. Start the daemon (current repo's channel — derived from `git rev-parse --git-common-dir`)
#    Default: cursor at file-end → only NEW events dispatch. Pass --from-start to backfill.
agent-rally-watcher start

# 3. Check status / tail logs
agent-rally-watcher status
tail -F ~/.agent-rally-watcher/logs/daemon.log

# 4. Stop the daemon
agent-rally-watcher stop
```

The daemon discovers the current repo's Rally Point channel via the same `app_slug()` logic agent-rally-point uses — worktree-independent, same channel across all clones of the canonical repo.

## consumers.toml

```toml
# Each [consumers.<id>] table = one consumer (id is also the cursor name).
# Filter rules AND-combine; missing fields match everything.
# Sinks: file (append JSONL), notify (macOS osascript), http (POST, v0.2).

[consumers.claude_code.filter]
kinds = ["feedback", "handoff", "dep-change"]
tools_not = ["claude_code"]      # don't echo back to self

[consumers.claude_code.sink]
type = "file"
path = "~/.agent-rally-watcher/streams/claude_code.jsonl"


[consumers.codex.filter]
kinds = ["feedback", "handoff"]

[consumers.codex.sink]
type = "file"
path = "~/.agent-rally-watcher/streams/codex.jsonl"


[consumers.urgent_notify.filter]
kinds = ["feedback"]
payload_match = { verdict = "BLOCKED" }

[consumers.urgent_notify.sink]
type = "notify"
title = "Rally Watcher"
```

### Migrating from v0.1.0 YAML

v0.1.0 used `consumers.yaml`; v0.1.1 uses `consumers.toml` (stdlib `tomllib`, no
PyYAML). The schema is the same — only the surface syntax changed:

- Each `consumers.<id>:` block becomes a `[consumers.<id>]` table.
- `filter:` and `sink:` become `[consumers.<id>.filter]` and `[consumers.<id>.sink]` sub-tables.
- `[feedback, handoff]` YAML lists become `["feedback", "handoff"]` TOML arrays.
- `payload_match:` mappings become inline tables: `payload_match = { verdict = "BLOCKED" }`.

The default config path moved from `~/.agent-rally-watcher/consumers.yaml` to
`~/.agent-rally-watcher/consumers.toml`. Rename the file and translate the syntax
(or `cp examples/consumers.toml ~/.agent-rally-watcher/consumers.toml` and re-edit).

### First-start backfill

On a fresh install, the daemon defaults to seeking the current cursor to the END of
`changes.jsonl` on first start (`--from-now`, the default). Only events written AFTER
start are dispatched. To replay everything from byte 0, pass `--from-start`:

```bash
agent-rally-watcher start --from-start
```

Restarts after that point honor the persisted cursor regardless of which flag is
passed — the flag only affects the FIRST start when no cursor exists yet.

## Architecture

> **Full architecture spec: [`ARCHITECTURE.md`](ARCHITECTURE.md)** (three-layer model, data flow, sink types, lifecycle, design invariants).

### How it fits

```
┌─────────────────────────────────────────────────────────────────────┐
│  Layer 3 — CONSUMERS (build-loop, codex, claude_code, custom tools) │
│    post() events · checkpoint_read() deltas · inbox/<tool>.jsonl    │
└────────────────────────────▲─────────────────────▲──────────────────┘
                             │                     │
              read filtered  │     write events    │
                             │                     │
┌────────────────────────────┴──────────┐          │
│  Layer 2 — DAEMON (THIS REPO)         │          │
│    kqueue/inotify tail of channel ·   │          │
│    consumers.toml filter rules ·      │          │
│    per-consumer cursor · sinks        │          │
└────────────────────────────▲──────────┘          │
                             │ tails                │ publishes
                             │                     │
┌────────────────────────────┴─────────────────────┴──────────────────┐
│  Layer 1 — SUBSTRATE (agent-rally-point)                            │
│    channel layout · changes.jsonl append-only · revision counter ·  │
│    presence/heartbeat · checkpoint_read · post · lifecycle reapers  │
└─────────────────────────────────────────────────────────────────────┘
```

The watcher is **Layer 2**: a long-running daemon that pushes filtered events to per-consumer sinks so that low-latency tools don't have to poll. Tools that don't need sub-second latency can skip the watcher entirely and call `checkpoint_read` from `agent-rally-point` directly on a timer — both paths read the same underlying log.

### Data flow

```
agent-rally-point          (substrate)
   writes ──► ~/.agent-rally-point/apps/<slug>/changes.jsonl
                         │
                         │  watchfiles (kqueue / inotify push)
                         ▼
agent-rally-watcher       (daemon)
   filter.match(record, consumer.rules)
   cursor.advance(consumer_id, byte_offset)
   dispatch.send(record, consumer.sink)
                         │
                         ▼
   ┌─────────────┬───────┴────────┬──────────────┐
   ▼             ▼                ▼              ▼
 file sink   notify sink      http POST       (custom)
 (stream)    (osascript)      (v0.2 stub)
```

### Adding a new consumer

1. Append a `[consumers.<id>]` block to `~/.agent-rally-watcher/consumers.toml` with filter rules + sink config (see [consumers.toml example above](#consumerstoml)).
2. `agent-rally-watcher reload` — re-reads the config without restarting the daemon. The new consumer's cursor initializes to `--from-now` (file-end); pass `--from-start` on the next `start` if you want to backfill from byte 0.
3. Verify routing: `tail -F ~/.agent-rally-watcher/streams/<id>.jsonl` after the next channel event.

### Cross-references

- **Substrate (channel format, record schema, presence API)**: [`agent-rally-point/ARCHITECTURE.md`](https://github.com/tyroneross/agent-rally-point/blob/main/ARCHITECTURE.md), [`docs/SCHEMA.md`](https://github.com/tyroneross/agent-rally-point/blob/main/docs/SCHEMA.md).
- **Discovery (manifest, CLI)**: [`agent-rally-point/docs/DISCOVERY.md`](https://github.com/tyroneross/agent-rally-point/blob/main/docs/DISCOVERY.md).
- **Build-loop's consumption pattern**: [`build-loop/skills/build-loop/references/coordination.md`](https://github.com/tyroneross/build-loop/blob/main/skills/build-loop/references/coordination.md).

## How this differs from `tail -F | grep`

- **Per-consumer cursors** — restart-safe, no duplicate delivery, no missed events.
- **Schema-aware filters** — match on `kind`, `tool`, or any `payload.<field>` without writing jq.
- **Structured dispatch** — notify, file, HTTP all from one config; no shell-pipeline plumbing.
- **Daemon lifecycle** — PID file, log rotation, launchd integration.

For one-shot tailing, `tail -F .../changes.jsonl | jq` is fine. For coordinating across multiple long-running tools, use the watcher.

## Provenance

Companion to [agent-rally-point](https://github.com/tyroneross/agent-rally-point); architectural sibling of [build-loop-monitor](https://github.com/tyroneross/build-loop). Tail engine: [`watchfiles`](https://github.com/samuelcolvin/watchfiles) (the only third-party dep).

## License & Attribution

Apache-2.0. See [LICENSE](./LICENSE).

- [`LICENSE`](LICENSE) — full license text.
- [`NOTICE`](NOTICE) — attribution notices that, per Apache 2.0 §4(d), must travel with any redistribution of this work.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution conventions: per-file SPDX headers (REUSE 3.3), AI co-author trailer, signed commits, conventional commits.

Per-file `SPDX-FileCopyrightText` and `SPDX-License-Identifier` headers are required on shipped source files. Files that cannot carry inline comments (JSON, generated assets) are annotated in [`REUSE.toml`](REUSE.toml). Validate compliance locally with `uvx reuse lint`.
