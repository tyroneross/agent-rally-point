<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Wake + Integration Plan — shared by Claude & Codex (dogfoods agent-rally-point)

**Read this first, in a fresh terminal, as either Claude Code or Codex.** This single file is the entry point: it gives you the full context, your owned piece, and how to coordinate with your peer **through agent-rally-point itself** (this build dogfoods the tool we're building).

Repo/worktree: `/Users/tyroneross/dev/git-folder/agent-rally-point-integration` (branch `integration`, off `origin/main` = PR45).

---

## 0. Context (no prior session needed)

PR45 ("fact-backed managed sessions") is merged to `origin/main` — single crate `rally-cli`. Local kernel-line work (rally-core/trust/protocol) was assessed and **dropped**; three items survived and are in progress on `integration`:
- ✅ `e2a41d3` toolchain pin (PR45 builds locally on rustc 1.95; `cargo test --workspace` green).
- `9b623f6` discovery re-port **design** (not yet code).
- `85550a5` standby/wake **contract** (not yet code).
- `05adfc4` + routing fix: `scripts/rally_wake.py` (cross-agent wake) + `docs/WAKE_TEST_PROTOCOL.md`.

The wake mechanism was researched — read it before touching submit code:
`~/dev/research/projects/agent-rally-point/cross-agent-terminal-wake-inject-herdr-tmux-2026-05-28.md`
Key result: **doorbell + mailbox**; short nudge + per-backend submit (herdr=`Enter`, ×2 if collapsed; tmux=`C-m`); **confirm via Rally channel post, never TUI scraping.**

## 1. Goal

Land the 3 surviving items and make cross-agent wake production-ready, with Claude + Codex working in parallel and coordinating via Rally.

## 2. Pieces (MECE — claim yours before editing)

| # | Piece | Owner | Files (owned) | Done = |
|---|---|---|---|---|
| **P1** | Finalize + **live-test** `rally_wake.py` per-backend routing (tmux + herdr) against real agent TUIs; channel-confirm path | **claude_code** | `scripts/rally_wake.py`, `docs/WAKE_TEST_PROTOCOL.md` | both backends submit + a channel-confirmed wake, committed |
| **P2** | Turn the **standby/wake contract** doc into code: `rally inject`/`next` honor doorbell+mailbox + emit a channel post on wake | **codex** | `crates/rally-cli/src/{next,backends}.rs`, `docs/schemas/*session-backend*` | `cargo test -p rally-cli` green + contract behavior demonstrated |
| **P3** | Implement the **discovery re-port** design against `.rally/` RoomStore (incl. legacy `~/.agent-rally-point/apps` visibility) | **codex** | new `crates/rally-cli/src/discovery*.rs` | `rally` surfaces channels/recent from `.rally/`; tests pass |
| **P4** | Decide **herdr-integration-restart vs channel-confirm** as default wake confirmation; record | **joint** | this file §5 | a recorded decision both agents ratify |

Out of scope: pushing `origin/main`; deleting worktrees; re-introducing rally-core/trust/protocol.

## 3. Coordinate via agent-rally-point (the dogfood)

`S=/Users/tyroneross/.claude/plugins/cache/rosslabs-ai-toolkit/build-loop/0.12.16/scripts`
(Codex: same path works; or use the host-neutral `agent_rally.py`.)

1. **Join** the channel + announce your piece:
   `python3 $S/coordination_rally.py --workdir . --session-id <you>-$(date +%s) --tool <claude_code|codex> --to peer --message "joined, claiming P<n>" --owns "<your files>" --json`
2. **Check** before each step boundary:
   `python3 $S/coordination_status.py --workdir . --session-id <you> --json`
3. **Wake your peer** when you have a handoff — short doorbell, payload in the channel:
   `python3 scripts/rally_wake.py --tool <peer> "Unread in Rally — run coordination_status + read this plan §<n>" --require-idle --confirm-channel ~/.agent-rally-point/apps/repo_196422842096be12/changes.jsonl`
   (herdr submit handled per-backend automatically; the channel post is the confirm.)
4. **Post verdicts/handoffs** to the channel (revision bump) — that bump is also how your peer's `rally_wake --confirm-channel` knows you acted.

Rules: verdicts gate (don't advance a piece past `verification-pending` until the peer posts PASS or resolved VARIANCE); every write-handoff names owns / does-not-own / interface / checkpoint; one owner per file.

## 4. Acceptance per piece
- **P1:** a tmux wake and a herdr wake each submit (status/output proof), and one wake is **channel-confirmed**; `rally_wake.py` committed.
- **P2:** `rally inject`/`next` emit a wake-intent fact + the woken agent's action shows as a channel post; `cargo test -p rally-cli` green.
- **P3:** `rally` lists channels/recent from `.rally/`; legacy apps dir handled (import | legacy-only | warn); tests pass.
- **P4:** decision recorded in §5 and ratified by both.

## 5. Decisions log (P4 + any mid-flight)
<!-- append: date — decision — ratified-by -->
- 2026-05-28 — Default wake confirmation should be Rally channel-confirm; Herdr v4 status is useful only after target session restart and should remain a secondary liveness/delivery signal. Token echo remains valid for direct reply tests; tmux/no-integration paths use channel-confirm. — ratified-by: codex; pending: claude_code
- 2026-05-28 — Coordination for this integration run uses the original Rally channel for `/Users/tyroneross/dev/git-folder/agent-rally-point` (`repo_196422842096be12`) even though the integration worktree resolves to a separate room; use explicit `--workdir /Users/tyroneross/dev/git-folder/agent-rally-point` for cross-agent status until the plan is updated or rooms are bridged. — ratified-by: codex; pending: claude_code

## 6. References
- Research: `~/dev/research/projects/agent-rally-point/cross-agent-terminal-wake-inject-herdr-tmux-2026-05-28.md`
- Wake test protocol: `docs/WAKE_TEST_PROTOCOL.md`
- Live coord file (verdict log): `.build-loop/coordination/rally-diff-integration-assessment-2026-05-28.md`
- Standby/wake contract: commit `85550a5`; discovery design: commit `9b623f6`
