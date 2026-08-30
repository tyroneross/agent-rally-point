---
name: launch-terminal
description: Use when asked to launch a fresh Claude (or Codex) terminal session — optionally with Remote Control visible on the user's phone — as a rally-managed session that is named, injectable, and verifiable. Triggers on "launch a fresh terminal", "spin up a new Claude session", "start a session I can see on my phone", "launch a reviewer session". Not for coordinating with an already-running session (use agent-rally-point) and not for multi-agent fan-out design (use rally-workflows).
---

# Launch Terminal — a fresh, named, remotely visible agent session

Launch through rally so the session is MANAGED: named, injectable, liveness-tracked,
and cleanly stoppable. A bare `tmux new-session claude` gives none of that.

## Launch

```bash
rally run claude --name <name> --backend tmux --json     # worktree-isolated (default)
rally run claude --name <name> --backend tmux --shared   # share the live checkout (no worktree)
```

- `<name>` is what the user sees everywhere — their phone, `rally sessions`,
  the tmux target (`rally-claude-<name>`). Pick a human-meaningful name.
- Default provisions a per-agent git worktree; use `--shared` for a
  general-purpose terminal that should sit in the real checkout.
- `codex` / `opencode` / `gemini` work as the agent argument too.
- The session inherits the user's global config (settings, plugins, MCP).
  Rally passes no extra flags — per-session launch-arg passthrough does not
  exist yet.

## Verify before claiming success (each step, in order)

1. **Alive + injectable**: `rally sessions --json` → your name with
   `liveness: live`, `injectable: true`.
2. **Session actually started**: `tmux capture-pane -t rally-claude-<name> -p | tail`
   — look for the Claude Code banner and an idle prompt. A trust/theme dialog
   parked here blocks everything downstream; answer it via
   `tmux send-keys` before proceeding.
3. **Remote Control, if the user wants it on their phone**: do NOT assume the
   global `remoteControlAtStartup` took effect in the pane — observed 2026-08-29
   that a rally-launched pane needed an explicit nudge. Run:
   `tmux send-keys -t rally-claude-<name> "/rc" Enter`, wait ~5s, capture the
   pane. Success shows "This session is available in the Claude mobile app and
   at https://claude.ai/code/session_…" — report that URL to the user, then
   dismiss the dialog with one more `Enter`.
4. **Channel test**: `rally inject <name> --text "<short hello>" --json` and
   confirm `delivered: true`. Only now is the session usable by peers.

## Hand back to the user

Report: the name (as it appears on their phone), the claude.ai session URL,
the terminal attach command (`tmux attach -t rally-claude-<name>`), and whether
the session runs in a worktree or the shared checkout.

## Teardown

`rally sessions --reap --apply` sweeps dead sessions; a worktree-isolated
session's worktree is cleaned at stop. Never `kill -9` the pane while it holds
unreleased claims — stop it properly so claims release.
