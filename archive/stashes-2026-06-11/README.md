<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Archived stashes — 2026-06-11

Six `git stash` entries (all from prior sessions, on branches now deleted) were
archived here and then dropped. Each was assessed against `main` and found to
hold no unique, unmerged, still-relevant work. This directory is the reversible
record: full-fidelity `stashes.bundle` (thin — recover with `main` present) plus
human-readable per-stash `.patch` diffs.

## Recovery

- One stash, as a patch: `git apply archive/stashes-2026-06-11/stash-<N>-<slug>.patch`
- Full fidelity from the bundle (exact commit, incl. any untracked files):
  ```bash
  git fetch archive/stashes-2026-06-11/stashes.bundle <sha>
  git stash apply <sha>        # or: git checkout <sha>
  ```

## Inventory + verdict

| Stash | SHA | What it was | Why removed |
|---|---|---|---|
| {0} | `2e3b067` | orphaned "RUN B" inject-fix-01 working tree (daemon-first inject routing, 247 lines) | Feature **landed in main** — `try_register_session_with_daemon`, `delivery_path`, `ledger_only`, `daemon_client.rs`, and `docs/PLAN-daemon-first-inject-routing.md` all present. Orphaned duplicate. |
| {1} | `37b0d5a` | 2-line `session_identity` fragment | Self-labeled "superseded by feat/protocol-integration". `mod session_identity;` already in main; only a `#[allow(clippy::wrong_self_convention)]` differs (main compiles clean without it). |
| {2} | `302a449` | `.rally/log/index.json` (1 line) | Runtime ledger index; pre-canonical-migration backup; migration long complete. |
| {3} | `29a35d4` | `.rally/log/2026-05-31.jsonl` ledger | Runtime coordination log, ~2 weeks stale, pre-migration backup. |
| {4} | `98473e8` | identical to {3} | Exact duplicate ledger snapshot. |
| {5} | `60ec90b` | "rally-global-discovery" refactor (850 lines, 15 files) | Feature **already in main** (`discovery.rs`, `rally locate` command). Targets a **rejected architecture** (`rally-core` crate + `args/dispatch/query_commands`) — all 15 target files are gone from main, so it cannot apply and the direction was abandoned. |

Branches the stashes referenced — `fix/b19-codex-hook-repoint`, `codex/rally-global-discovery` — are deleted (local + remote).
