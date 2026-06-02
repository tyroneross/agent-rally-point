# SPEC — `rally run` provisions a worktree per launched agent (Phase 1b)

## Context / why
Companion to build-loop's run-entry isolation (Phase 1a, already landed on the build-loop repo). This is the rally-cli substrate side: when `rally run` launches an agent session, every agent should land in its **own linked git worktree** (its own HEAD/branch) rather than all sharing the canonical checkout. This is the structural fix for the recurring shared-checkout hazard (commits on the wrong branch, HEAD switched under a peer) — now a user-confirmed RULE: worktree-per-agent is mandatory.

## Verified foundation (do not re-question)
- Rally resolves its coordination room via the shared **git common-dir** — `crates/rally-cli/src/lib.rs:6285` (`git_common_dir()` reads `<gitdir>/commondir`), and the test `crates/rally-cli/tests/user_journey.rs:1725` (`linked_git_worktree_uses_common_room`) asserts linked worktrees share ONE room. So per-agent linked worktrees keep a single `.rally/` coordination room while isolating each agent's HEAD. This is the load-bearing property that makes this clean.
- Today `rally run` launches every agent with `ManagedSession.cwd = <repo root>` (`crates/rally-cli/src/backends.rs:56` field; `command_run` in `lib.rs` ~1903 sets it). `worktree_guard.rs` only DETECTS+warns the shared-branch hazard (advisory, line 16) — never provisions or blocks.

## Goal
**`rally run` provisions a dedicated linked git worktree per launched agent and launches the agent there.** Agents share one coordination room (via common-dir) but never share a working tree / HEAD. Default on; explicit opt-out for the rare shared case.

## Approach (refine against real code in Assess/Plan)
1. **Provision before launch.** In `command_run` (before `backend_runner.start()`), create a linked worktree for the agent on a per-agent branch off the run base (default the current branch or `main`). Reuse a small helper; set `ManagedSession.cwd` to the worktree path so the backend (tmux/herdr/cmux/ptyd) launches the agent there. Fail-closed: if worktree creation fails, surface a clear error rather than silently launching in the shared checkout.
2. **Worktree location.** Prefer `.rally/worktrees/<agent-id>/` for consistency with the `.<toolname>/` storage convention — BUT only if `.rally/worktrees/` can be gitignored without disturbing the tracked `.rally/log/` ledger. If that's awkward, use an out-of-tree base `~/.agent-rally-point/worktrees/<repo-slug>/<agent-id>/` (rally already owns `~/.agent-rally-point/` for the global room index). Decide based on what keeps the room-resolution + ledger intact.
3. **Branch per agent.** `agent/<tool>-<session-short>` (or caller-provided), off the base. Recorded on `ManagedSession` (add a `worktree_path` / `branch` field) and durably in a fact for audit/cleanup.
4. **Cleanup.** On `rally stop` / session close, remove the agent's worktree (and its branch if fully merged / empty); bundle-before-remove only if it carries unmerged commits. Add a reaper-equivalent or extend existing session teardown.
5. **Opt-out.** `--shared` (or `--no-worktree`) flag on `rally run` for the deliberate shared-checkout case; default = isolated.

Prefer reusing/extending `worktree_guard.rs` + existing session teardown over new modules. Minimal deps. Keep the advisory hazard detector as a backstop for any `--shared` use.

## Constraints (HARD)
- Work ONLY in `/Users/tyroneross/dev/git-folder/agent-rally-point-wt-runiso` (branch `feat/rally-run-worktree-isolation`, off clean `main` @ 3024aa4). NEVER touch the canonical `agent-rally-point` checkout or the `agent-rally-point-cockpit` worktree.
- Preserve the single-agent / solo case (transparent: the agent just runs in a worktree). Don't break the existing room-sharing test — extend it to assert per-agent worktrees still share one room.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` all green. Add tests for: per-agent worktree provisioning, common-room sharing across two agents' worktrees, cleanup on stop, `--shared` opt-out.
- Do NOT push; do NOT merge to main. Commit on the feature branch.

## Verification
- Two `rally run` launches → two distinct worktrees on distinct branches, both resolving to the SAME `.rally/` room (assert via the room projection / common-dir).
- A commit by agent A lands on A's branch; B's checkout/HEAD is unaffected (the exact hazard, now structurally impossible).
- `rally stop` removes the worktree; no leak (`git worktree list` clean).
- `--shared` reproduces today's behavior (one checkout) for the opt-out path.
