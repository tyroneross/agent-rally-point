<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Orchestration board — agent-rally-point multi-agent work

> **Lead orchestrator:** `claude_code:lead` (this session — scoped id; bare `claude_code` = the PR46/L4 terminal). **Codex-side coordinator:** `codex:dynwf-coordinator` (delegated, acknowledges lead).
> **Coordination substrate:** the `rally` CLI (this repo). **Source of truth for status:** this file + `rally room`.
> **Protocol:** every lane `rally say claim` + `rally check before-write` before editing its owned paths; commit at each
> stop point; the lead syncs this board as commits/facts land. Updated as a binding `decision` in the room.

This board is **dogfoodable**: it is the human-readable projection of the rally facts. A fresh agent (Claude or Codex)
reads this + `rally room --json` and knows what is owned, in flight, landed, and next — without the user relaying it.

## Lanes

| Lane | Owner (rally tool) | Owns | Status | Stop-point commits | Next checkpoint |
|------|--------------------|------|--------|--------------------|-----------------|
| **L1 · dynamic-workflows module** | `claude_code:lead` | `dynamic-workflows/**` | ✅ landed | `6d76780`, `85a089a`, `d52a184` | discovery wiring (deferred) |
| **L2 · managed-session scale hardening** | `codex` / `codex:dynwf-coordinator` | `crates/rally-cli/src/{cli,backends,lib}.rs` (managed-session paths) | ✅ landed locally | `f852960`, `63f7a66`, `f6bab07` | keep installed CLI aligned before new dogfood |
| **L3 · bpaf run/inject/capture/stop --help panic** | `codex:dynamic-scale-01` | `crates/rally-cli/src/cli.rs` (parsers) | ✅ fixed (lead-verified) | `9332915`, `b056855` (local; push pending) | — `rally run --help` exits 0 ✓ |
| **L3b · room projection: clear resolved risks** | `codex:dynamic-scale-01` | `crates/rally-cli/src/store.rs` (projection) | ✅ fixed locally | room-state clarity commit | resolved risks drop from `current_risks` |
| **L4 · PR46 port** (contract-claims + receipts + CI gate) | `claude_code` (PR46 terminal — bare id) | `crates/rally-cli/src/{next,store,check}.rs`, `docs/schemas/*`, `RALLY.md` | ⏳ pending | — | `--produces` + receipts on `main` |
| **L5 · observation seam** (Plan B: DAG / wake-due / heartbeat) | unassigned | TBD | ⏸ deferred | — | gated on L4 lineage landing |

## Dogfood linkages
- **L2 → L1**: managed-session auto-numbering + parallel-launch id reservation is the substrate L1's **Tier-2 cross-host**
  fan-out (`rally run` / `rally inject`) rides on. L1's skills document that path; L2 makes it scale.
- **L3 → L1**: the bpaf `--help` panic affects exactly the `rally run`/`inject` surface L1's skills reference. L1 already
  corrected its documented syntax (`d52a184`); L3 fixes the CLI itself. **Pattern flagged** (positional-not-rightmost) so
  L4 can audit its own new parsers (`check ci`, `--produces`/`--depends`) for the same defect.
- **L4 → L5**: the seam's DAG + wake-due lean on PR46's `produces`/`depends` lineage + receipt lifecycle. A before B.

## Stop-point / commit policy
1. Each lane commits at a **verifiable stop point** (its F-criteria green), not mid-edit.
2. The lead syncs this board + re-commits it whenever a lane lands a stop-point commit or posts a room fact that changes
   status. Commit subject prefix: `docs(orchestration): sync board — <what changed>`.
3. Branch hygiene: lanes collapse onto `main`; no leftover branches/worktrees. One folder, one branch.
4. Verifiable artifacts only: each landed lane posts `rally say artifact … --evidence <verification>`.

## Live status log (newest first)
- **2026-05-29 (sync 3)** — **L3b CLOSED**: Codex fixed room projection so `resolve --ref <risk>` removes the risk from
  `current_risks`. Validation: `cargo test --all` 23/23 user journeys, `cargo clippy --all-targets -D warnings`, fmt,
  and diff-check clean in the room-state worktree.
- **2026-05-29 (sync 2)** — Lead identity scoped to `claude_code:lead` (room decision); bare `claude_code` reserved for L4/PR46 terminal.
  **L3 CLOSED**: codex:dynamic-scale-01 fixed the bpaf panic (`9332915`,`b056855`); lead-verified `rally run --help` exits 0. Local `main` b056855 ahead of origin 237d067 (push pending). `codex:dynwf-coordinator` acknowledged lead (seq 26). Dogfood loop confirmed: risk seq13 → claim seq18 → fix → artifact seq21.
- **2026-05-29** — Lead established (`claude_code`); board created. L1 landed + tests 7/7 after L2 merges (no regression).
  L3 claimed by `codex:dynamic-scale-01`; bpaf `--help` still panics. L4 not yet on `main`. Risk seq 13 (bpaf) → claim seq 18.

> To update: edit the table + append a dated line here, then `rally say` the status change and commit with the
> `docs(orchestration): sync board` prefix. Keep this file and `rally room` in agreement.
