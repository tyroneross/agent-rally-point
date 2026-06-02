<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Spec — `rally watch`: rally-native autonomy watcher (host-neutral)

**Goal:** formalize the emergent "idle agent picks up coordination work" behavior into a first-class,
installable rally feature that works for **Claude, Codex, and any other agent** — not a build-loop
Python poller watching the soon-retired legacy global index.

> **Reference implementation:** the legacy Python push-based watcher lives at
> [`tools/agent-rally-watcher/`](../tools/agent-rally-watcher/) (vendored from
> the standalone [agent-rally-watcher](https://github.com/tyroneross/agent-rally-watcher),
> v0.1.1). When this spec ships and reaches feature parity, that subtree is
> retired. See [`tools/agent-rally-watcher/MIGRATION.md`](../tools/agent-rally-watcher/MIGRATION.md).

**Language: Rust**, as a new `rally watch` subcommand. Rationale: it ships in the single `rally`
binary every agent already installs (host-neutral by construction); zero new deps (rally-cli is
std-only — a poll-thread on `index.json`, no async/notify needed); and it reads the **per-repo
`.rally/log`**, so it survives B17 (legacy-global-index retirement) which breaks the current Python
pollers. The host-specific *engagement* is kept OUT of Rust — pluggable via a command — so the core
stays neutral.

## Detect / engage split

```
                 ┌──────────── rally watch (Rust, host-neutral) ────────────┐
  .rally/log ──► │ poll index.json max_seq (adaptive 5s→300s, reset on Δ)   │
                 │ on new revision: emit JSONL event  +  run --on-activity   │
                 └──────────────────────────┬───────────────────────────────┘
                                            │ (pluggable host adapter)
                          ┌─────────────────┼──────────────────┐
                       Claude wake      codex exec …       any agent cmd
```

## CLI

```
rally watch [--tool <id>] [--interval <secs=5>] [--max-interval <secs=300>]
            [--on-activity <command>] [--once] [--duration-hours <h>] [--json]
```

- **Detect:** poll `.rally/log/index.json` `max_seq` (cheap — no full snapshot). Interval starts at
  `--interval`, multiplies toward `--max-interval` while idle, resets to `--interval` on any new
  revision. `--once` polls a single time and exits (for cron/launchd-driven cadence). `--duration-hours`
  bounds a long-running loop (default: unbounded; KeepAlive handles restart).
- **Emit:** on a new revision, write one JSONL line to stdout:
  `{event:"activity", from_seq, to_seq, new_kinds:[...], tool_last, room, ts}`. Always-on, so any
  consumer (a host loop, a log) can react. On no change: optional heartbeat under `--json`.
- **Engage (pluggable):** if `--on-activity <command>` is set, run it on new activity with context in
  the environment: `RALLY_ROOM`, `RALLY_FROM_SEQ`, `RALLY_TO_SEQ`, `RALLY_TOOL`, `RALLY_REPO`. The
  command IS the host adapter — `codex exec "…"`, a Claude wake, or any agent's entrypoint. The watcher
  never hard-codes an agent. One in-flight engage at a time (don't stack).
- **Host-neutral identity:** `--tool` only labels the watcher's own heartbeat facts (optional); the
  watcher does NOT impersonate the engaged agent.

## Persistence (install)

`rally watch --print-launchd [--engage <cmd>]` emits a ready launchd plist (macOS) to stdout; the
Linux sibling `--print-systemd` emits a unit. The user installs it (`launchctl load …`). The unit
runs `rally watch` in the repo with `RunAtLoad` + `KeepAlive` so the behavior survives restarts.
(Keep OS-file *writing* out of the binary — emit text, user installs — minimal + auditable.)

## B17 interaction (critical)

The watcher reads **per-repo `.rally/log`**, never `~/.agent-rally-point/apps/.../changes.jsonl`. So
B17 (legacy-index retirement) and this watcher are aligned by construction — retiring the legacy
index does NOT break autonomy (unlike the current build-loop Python poller, which watches the legacy
index and would break). Document this as the B17/autonomy co-design.

## Acceptance

- `rally watch --once --json` in a repo with a room prints an `activity` event when `max_seq`
  advanced since a stored cursor, nothing when unchanged; exits 0.
- `rally watch --on-activity 'echo HIT >> /tmp/x'` (short loop) runs the command exactly once per new
  revision, with `RALLY_TO_SEQ` set; verified by posting a fact in another shell.
- Adaptive backoff: interval grows while idle, resets to `--interval` on activity (assert via log).
- Reads per-repo `.rally/log` only (no legacy-global-index read) — `rg` proves it.
- `--print-launchd` emits a valid plist referencing `rally watch` + the repo cwd.
- Cross-agent: the same binary + a different `--on-activity` command serves Codex (`codex exec`),
  Claude, or any agent — no agent name in the watcher core.

## Engage adapters (build-loop-free)

The watcher core is host-neutral; the `--on-activity` command is the only per-agent piece, and none
of these need build-loop:

**Codex** (autonomous, `approval_policy="never"`):
```bash
rally watch --print-launchd --on-activity 'codex exec "You are codex:auto on this rally-point repo. Run: rally next --tool codex:auto --json. If it returns claimable, deps-met work: rally say claim it, do it, then rally say artifact + release. Coordinate via rally; never block."' \
  > ~/Library/LaunchAgents/com.agent-rally-point.watch.codex.plist
launchctl load ~/Library/LaunchAgents/com.agent-rally-point.watch.codex.plist
```

**Claude** (headless): swap the engage for `claude -p "<same self-contained autonomy prompt>"`.

**Any other agent:** point `--on-activity` at its own non-interactive entrypoint. The watcher passes
`RALLY_ROOM`/`RALLY_FROM_SEQ`/`RALLY_TO_SEQ`/`RALLY_TOOL`/`RALLY_REPO` in the env for the prompt to use.

This fully replaces the emergent `.build-loop` Python poller path: autonomy now lives in the `rally`
binary + a one-line adapter, with **zero build-loop dependency** and per-repo (B17-safe) reads.
