<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->
# Changelog

All notable changes to **agent-rally-watcher**. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] — 2026-05-23

### Changed
- **Config moved from YAML to TOML.** `consumers.yaml` is now `consumers.toml`,
  parsed via stdlib `tomllib` (Python ≥3.11 is already required by the project).
  Schema is unchanged — only the surface syntax. Migration guide in
  [README.md#migrating-from-v010-yaml](./README.md#migrating-from-v010-yaml).
- **PyYAML dependency removed.** Sole non-stdlib runtime dep is now `watchfiles`.
- **Default config path:** `~/.agent-rally-watcher/consumers.yaml` →
  `~/.agent-rally-watcher/consumers.toml`.
- **First-start cursor seeks to EOF by default.** On a fresh install, the daemon
  no longer backfills from byte 0; only events written AFTER start are dispatched.
  Pass `--from-start` to restore v0.1.0 behavior. Restarts after a cursor file
  exists are unaffected — `--from-now`/`--from-start` only apply when no cursor
  has been persisted yet.

### Added
- **Channel-dir fallback chain.** When `--channel-dir` is not passed, resolution
  now tries (1) `~/.agent-rally-point/apps/<slug>/` (canonical), (2)
  `~/.build-loop/apps/<slug>/` (legacy — emits one-shot stderr warning), (3)
  the canonical default. Lets v0.1.1 find Rally Point channels written by older
  build-loop installs without manual `--channel-dir` plumbing.
- `agent-rally-watcher start --from-now` (explicit form of the new default).
- `agent-rally-watcher start --from-start` (opt-in v0.1.0 backfill behavior).

### Removed
- `pyyaml>=6.0` from `pyproject.toml` dependencies.
- `examples/consumers.yaml` (replaced by `examples/consumers.toml`).

### Notes
- Existing v0.1.0 daemons keep working; upgrade in place:
  ```bash
  agent-rally-watcher stop
  uv sync   # or: pip install -e .
  # translate ~/.agent-rally-watcher/consumers.yaml → consumers.toml
  agent-rally-watcher start
  ```
- 39 tests pass (was 30 in v0.1.0; +9 covering the new fallback + first-start behavior).

## [0.1.0] — 2026-05-22

Initial public release. Push-based tail of agent-rally-point channels via
watchfiles; per-consumer cursors; file/notify/http(stub) sinks; macOS launchd
plist generator. See [README.md](./README.md) for the full feature surface.
