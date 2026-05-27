<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Dogfood Report

Date: 2026-05-27
Branch: `codex/rally-role-specialization`
PR: https://github.com/tyroneross/agent-rally-point/pull/40

## Sessions

- Pi used `rally pi --session-id pi-doctor-setup` to start from Rally.
- A second agent/panel used Rally to claim and release
  `feature:rally-post-cursors-watch`, but emitted `owner_tool: unknown` because
  that harness did not identify itself.
- Pi observed that anonymous claim via `rally claims` and waited for release
  before editing overlapping command files.

## What Worked

- Claims prevented blind overlap between panels.
- Handoffs surfaced through `rally start`/`context` and were acknowledged.
- `rally post`, cursor-scoped inbox reads, and `rally watch` now provide a live
  coordination loop for agents.
- Golden contracts caught startup/adapter/checkpoint JSON drift.
- `rally cmux packet` and `rally herdr packet` provide side-effect-free adapter
  payloads.
- `rally doctor --tool pi --json` correctly flagged the pending handoff,
  anonymous handoff source, and missing profile.
- `rally setup --json` discovered local harness binaries and wrote Herdr adapter
  notes with `rally setup install herdr --json`.

## Product Gaps Found

- Anonymous writes are rejected after `rally setup enforcement strict`; existing
  anonymous events still surface in `rally doctor`.
- Harness install now writes cmux/Herdr wrapper hooks and updates their local
  config files, but this still needs real multi-harness dogfood before calling
  it fully polished.
- JSON schemas are intentionally minimal and should be expanded as contracts
  stabilize.

## Current Agent Loop

```bash
rally pi
rally watch --tool pi --session-id <session> --since-cursor
rally doctor --tool pi --json
rally setup --json
rally judge --tool pi --phase idle --json
rally hook before-write --tool pi --path <path> --auto-claim --json
```

`rally judge` correctly stopped Pi while a required handoff from `claude_code`
was pending. `rally hook before-write --auto-claim` was then adjusted so it does
not create claims when a stop reason already exists.

## Verification

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
git diff --check
```
