<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Agent Rally Watcher — Architecture

> **Role: long-running daemon that demultiplexes a Rally Point channel stream into per-consumer sinks.**
> Reads only. Never writes the channel. Companion to [`agent-rally-point`](https://github.com/tyroneross/agent-rally-point), which owns the substrate.

## Position in the three-layer model

Rally Watcher is **Layer 2** of the agent-rally architecture. Cross-reference [`agent-rally-point/ARCHITECTURE.md`](https://github.com/tyroneross/agent-rally-point/blob/main/ARCHITECTURE.md) for the canonical model.

```
Layer 3:  CONSUMERS  (build-loop, codex, claude_code, custom)
              ▲                          ▲
              │ push (per-consumer sink) │ pull (checkpoint_read)
              │                          │
Layer 2:  ┌───┴────────────────┐         │
          │  AGENT RALLY       │         │
          │  WATCHER (this)    │─────────┤
          │  kqueue/inotify    │         │
          │  consumers.toml    │         │
          │  per-consumer cur  │         │
          └────────▲───────────┘         │
                   │ tail                │
                   │                     │
Layer 1:  AGENT RALLY POINT (substrate, append-only changes.jsonl)
```

The watcher is a **convenience over polling**: every consumer could call `checkpoint_read` directly on a timer. The watcher exists so that low-latency tools (a Claude Code session that wants to react to a peer's commit within ~1s) don't pay polling cost and don't have to implement filesystem watching themselves. Tools that don't need sub-second latency can skip the watcher and poll directly — both paths read the same underlying log.

## Data flow

```
agent-rally-point posts a record
   │
   ▼
~/.agent-rally-point/apps/<slug>/changes.jsonl    ← Layer 1 owns the write
   │  (kqueue on macOS / inotify on Linux fires on mtime change)
   ▼
watcher.watch_changes_jsonl()
   │
   ▼
filter.apply_consumer_rules(record, consumers.toml)
   │  (kinds, senders, payload-field matches)
   ▼
   ┌────────────────────┬─────────────────────┬────────────────────┐
   ▼                    ▼                     ▼                    ▼
dispatch.append_file   dispatch.notify    dispatch.http_post   (future sinks)
   │                    │                     │
   ▼                    ▼                     ▼
inbox/<tool>.jsonl   macOS osascript      HTTP POST (v0.2)
inbox/all.jsonl
```

The watcher never bumps the revision, never writes `changes.jsonl`, never modifies `sessions/`. Its only writes are:

- Per-consumer cursors at `~/.agent-rally-watcher/consumers/<tool>.cursor` (one integer per consumer; resumable mid-stream after daemon restart).
- Per-consumer sinks (`inbox/<tool>.jsonl`, `inbox/all.jsonl` — these live under the rally channel but the watcher is the sole writer per filename).
- Its own log at `~/.agent-rally-watcher/logs/daemon.log` (size-bounded rotation).
- Its own PID file at `~/.agent-rally-watcher/daemon.pid`.

## Consumer configuration

Path: `~/.agent-rally-watcher/consumers.toml`. TOML; parsed via stdlib `tomllib`. Each entry maps a stable tool-id (`claude_code`, `codex`, `cursor`, `terminal`, ...) to filter rules + sink configuration.

```toml
schema_version = "1.0"

[[consumers]]
tool_id = "claude_code"
# Only forward these record kinds (omit to forward all)
kinds = ["phase", "feedback", "handoff"]
# Only forward records sent BY these tools (omit to forward all)
senders = ["codex", "cursor"]
# Sink shape
sink = "file"
sink_path = "~/.agent-rally-point/apps/build-loop/inbox/claude_code.jsonl"

[[consumers]]
tool_id = "codex"
kinds = ["commit", "phase"]
sink = "notify"   # macOS osascript
notify_title = "Rally Point"
```

Filter semantics:

- `kinds` — record matches if `record.kind in kinds`. Absent → match-all.
- `senders` — record matches if `record.tool in senders`. Absent → match-all.
- `payload_match` (advanced) — exact-match key=value tests against `record.payload`. Future v0.2.

A record may match multiple consumers; each match dispatches independently. A record matching zero consumers is dropped (the watcher does not maintain a "default" inbox unless explicitly configured).

## Sink types

| Sink     | Status   | Behavior |
|----------|----------|----------|
| `file`   | v0.1     | Append-to-file (JSONL). The default sink target lives under the rally channel's `inbox/` so other tools can consume it via standard rally-point primitives. |
| `notify` | v0.1     | macOS `osascript -e 'display notification ...'`. Title from `notify_title`. |
| `http`   | v0.2     | HTTP POST to a configured URL. Useful for hosted Claude/Codex endpoints. Stubbed in v0.1. |
| `stdout` | planned  | Append a line to stdout of a registered subscriber process. Future. |

The sink choice is per-consumer; one consumer config produces exactly one sink write per matched record.

## Channel discovery

As of v0.1.1+, the watcher resolves its target channel via a three-step fallback (see `cli.py::DEFAULT_RALLY_POINT_APPS_ROOT` and `LEGACY_BUILD_LOOP_APPS_ROOT`):

1. **Canonical** — `~/.agent-rally-point/apps/<slug>/` if it exists.
2. **Legacy** — `~/.build-loop/apps/<slug>/` if it exists (rally-point shipped inside build-loop before becoming standalone). Logs a one-shot warning so the operator knows to migrate.
3. **Default canonical** — `~/.agent-rally-point/apps/<slug>/`. Creates the canonical layout on first write.

`--channel-dir PATH` overrides the entire resolution chain. `--cwd PATH` overrides the cwd used to derive the slug (handy when launching the daemon outside the repo it should watch).

In a future minor, the watcher will additionally consult [`agent-rally-point`'s discovery layer](https://github.com/tyroneross/agent-rally-point/blob/main/docs/DISCOVERY.md) (`discover()`) for the same resolution — currently the watcher mirrors `app_slug` directly rather than depending on `agent-rally-point` at runtime; this is a deliberate decoupling to keep the watcher's dependency graph minimal.

## Lifecycle

```
agent-rally-watcher start [--channel-dir PATH] [--cwd PATH] [--from-start]
agent-rally-watcher stop
agent-rally-watcher status
agent-rally-watcher reload
agent-rally-watcher install-launchd
```

| Command | Behavior |
|---------|----------|
| `start` | Fork daemon, write PID file, attach kqueue/inotify watch, begin tail. Default cursor: file-end (only NEW events dispatch). `--from-start` backfills from cursor or `~/.agent-rally-watcher/consumers/<tool>.cursor` per consumer. The `--from-now` flag (v0.1.1+) explicitly resets every consumer cursor to the current file-end on start. |
| `stop`  | Read PID file, SIGTERM, wait for clean exit (SIGKILL fallback at 5s). |
| `status` | Print PID + uptime + last-dispatched record from log. Exit 0 if running, 1 otherwise. |
| `reload` | Re-read `consumers.toml` without restarting. Useful for adding a new consumer without dropping in-flight events. |
| `install-launchd` | Generate a macOS `LaunchAgent` plist at `~/Library/LaunchAgents/com.tyroneross.agent-rally-watcher.plist`. Linux/systemd planned. |

## Log rotation

Log path: `~/.agent-rally-watcher/logs/daemon.log`. Rotated in-process when size exceeds the configured threshold (default ~1 MB). Rotated files become `daemon.log.<N>`; the daemon keeps the last few and prunes older ones. No external `logrotate` dependency.

## Cross-references

- **Substrate (channel format, presence, revision, post API)**: [`agent-rally-point/ARCHITECTURE.md`](https://github.com/tyroneross/agent-rally-point/blob/main/ARCHITECTURE.md) and [`docs/SCHEMA.md`](https://github.com/tyroneross/agent-rally-point/blob/main/docs/SCHEMA.md).
- **Discovery layer (manifest, CLI)**: [`agent-rally-point/docs/DISCOVERY.md`](https://github.com/tyroneross/agent-rally-point/blob/main/docs/DISCOVERY.md).
- **Build-loop's consumption pattern (post events on chunk-close, inbox pickup on Phase 1)**: [`build-loop/skills/build-loop/references/coordination.md`](https://github.com/tyroneross/build-loop/blob/main/skills/build-loop/references/coordination.md).

## Design invariants

| Invariant | Why |
|-----------|-----|
| Read-only against the channel | The watcher must never bump revision or append `changes.jsonl`. Layer 1 is the sole writer of those. |
| Cursor persistence | A daemon restart must resume mid-stream without duplicates or gaps. Per-consumer cursors at `~/.agent-rally-watcher/consumers/<tool>.cursor`. |
| Zero non-stdlib deps beyond `watchfiles` | Keep the install footprint tiny; config is TOML via stdlib `tomllib` (v0.1.1+). |
| Filter is matched once per record per consumer | A record cannot match the same consumer twice. Dispatch is idempotent over restarts via cursor. |
| Crash safety | A panicked dispatch must not crash the daemon. Each sink dispatch is wrapped; errors are logged and skipped. |
