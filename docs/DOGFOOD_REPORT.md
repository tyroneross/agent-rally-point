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

- Anonymous writes are still possible unless setup/doctor enforcement is used.
- Harness install is still file/snippet based; cmux/Herdr do not yet mutate their
  native config automatically.
- JSON schemas are intentionally minimal and should be expanded as contracts
  stabilize.

## Current Agent Loop

```bash
rally pi
rally watch --tool pi --session-id <session> --since-cursor
rally doctor --tool pi --json
rally setup --json
```

## Verification

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
git diff --check
```
