<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Local Gate + Workspace Issues — 2026-07-03

Context: retired the ubuntu GitHub Actions `rally-gate.yml` in favour of a local
`.githooks/pre-push` (commit `4b0cfeb`) — a local-first tool should gate on the
mac it runs on, not a rented Linux cloud. Two issues surfaced while validating
the swap.

## Issue 1 — Shared multi-agent checkout conflicts with a working-tree pre-push gate

**Problem.** Multiple agents (claude, codex) work concurrently in ONE shared
checkout. `cargo fmt/clippy/test` operate on the working tree — including other
agents' uncommitted / untracked WIP. So a whole-workspace pre-push gate couples
*every* agent's push to the least-complete WIP in the tree: one agent's broken
or unformatted in-progress file blocks an unrelated agent's push.

**Evidence (2026-07-03).**
- `.githooks/pre-push` correctly blocked a push on an untracked, unformatted
  `watchdog_concurrency.rs` (another agent's WIP) — unrelated to the pushed diff.
- Then blocked again on Issue 2 (`rally-ui` missing `main.rs`), forcing
  `RALLY_SKIP_PREPUSH=1` to land an unrelated change.

**Partial fix already shipped (`4b0cfeb`).** The **fmt** check is scoped to the
`.rs` files in the pushed range (`@{upstream}..HEAD`), so unrelated formatting
drift no longer blocks. `clippy`/`test` still compile the whole workspace and
cannot be file-scoped (a missing workspace member breaks `cargo` globally).

**Options (pick one):**
1. **Ephemeral-worktree HEAD gate (recommended).** Run the gates in a throwaway
   `git worktree add … HEAD` of exactly what's being pushed — immune to any
   working-tree WIP. Enforced + robust; heavier per push (worktree + build,
   mitigated by a shared/`sccache` target dir).
2. **On-demand `scripts/preflight.sh`.** Same gates, run on demand; no
   enforcement, no coupling. Relies on discipline.
3. **Keep as-is** and use `RALLY_SKIP_PREPUSH=1` when others' WIP blocks — simple,
   but the escape becomes routine and the gate erodes.

**Do NOT** `git stash` in the hook to hide WIP: unsafe in a shared checkout where
peers are actively editing those files.

## Issue 2 — Incomplete workspace member breaks the whole build — RESOLVED

**Status: RESOLVED** by `8e4ed94 feat(rally-ui): localhost multi-room agent
dashboard` — the crate was completed (`main.rs` + `server.rs`/`registry.rs`/
`room_source.rs` added) and `cargo build --workspace` is green again. Recorded
here as a lesson: an incomplete workspace member (Cargo.toml without its
`main.rs`) sitting in the shared tree breaks every agent's build/push until
completed. Prevention: don't add a crate to the workspace `members` until it has
a compilable entry point, or park it behind a feature/separate dir.

**Original problem.** An untracked WIP crate `crates/rally-ui/` had a `Cargo.toml` that
declares a `rally-ui` binary at `src/main.rs`, but `main.rs` does not exist:

```
error: can't find bin `rally-ui` at path `crates/rally-ui/src/main.rs`
error: could not compile due to 2 previous target resolution errors
```

Because it is a workspace member, this breaks `cargo build/clippy/test` for the
**entire workspace** — every local build/test/push in this checkout fails until
it is resolved. (Untracked, so not on any branch; it's live in the shared tree.)

**Fix (owner = whichever agent started `rally-ui`):** either add a minimal
`src/main.rs`, or remove/park the `Cargo.toml` / drop it from the workspace
`members` until the crate is ready. Incomplete workspace members should not be
left in the shared tree.

## Related
- `docs/CARGO-QUALITY-GATE-RECOMMENDATIONS-2026-07-03.md` — the hygiene-gate ladder now enforced locally.
- `.githooks/pre-push`, `rust-toolchain.toml` (pinned 1.95.0), `release.yml` (macos-14 binary, kept).
