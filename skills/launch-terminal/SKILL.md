---
name: launch-terminal
description: Use when asked to launch a fresh Claude (or Codex) terminal session — optionally with Remote Control visible on the user's phone — as a rally-managed session that is named, injectable, and verifiable. Triggers on "launch a fresh terminal", "spin up a new Claude session", "start a session I can see on my phone", "launch a reviewer session". Not for coordinating with an already-running session (use agent-rally-point) and not for multi-agent fan-out design (use rally-workflows).
user-invocable: false
---

# Launch Terminal — a fresh, named, remotely visible agent session

Launch through rally so the session is MANAGED: named, injectable, liveness-tracked,
and cleanly stoppable. A bare `tmux new-session claude` gives none of that.

## Two different "remote controls" — say which one you mean

- **Rally remote delivery** (agent-facing): the managed lifecycle itself.
  `rally run` makes the session addressable for `rally inject` / `capture` /
  `attach` / `stop`. No flag needed — being managed IS the enablement.
- **Claude Remote Control** (human-facing): the claude.ai / mobile-app bridge.
  Separate feature; see the `/rc` verification step below.

## Launch

```bash
rally run claude --name <name> --backend auto --json     # worktree-isolated (default)
rally run claude --name <name> --backend auto --shared   # share the live checkout (no worktree)
```

`--backend auto` uses rally's ptyd remote-delivery daemon when available and
falls back to `tmux`. Pass `--backend tmux` explicitly only when you need a
pane a human will attach to by name.

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
   inspect the receipt honestly. A successful transport write can report
   `delivery_reason: "sent_unverified"`, `reached_target: false`, and
   `queued: true`; wait for a target-authored ACK before treating it as read.
   Only a confirmed target acknowledgement makes the session usable by peers.

## Hand back to the user

Report: the name (as it appears on their phone), the claude.ai session URL,
the terminal attach command (`tmux attach -t rally-claude-<name>`), and whether
the session runs in a worktree or the shared checkout.

## Operate

```bash
rally inject <name> --text "..." --json   # inspect delivery_reason and target ACK
rally capture <name> --json               # read session output via the backend
rally attach <name> --json                # attach to the runtime surface where supported
rally stop <name> --json                  # stop cleanly (releases claims, cleans worktree)
```

For an ALREADY-open terminal, `rally adopt` registers it as managed — but only
after positively identifying its tmux/cmux target; adopting a guessed pane
delivers keystrokes into the wrong window.

## Teardown

Prefer `rally stop <name>` per session. `rally sessions --reap --apply` sweeps
dead sessions in bulk; a worktree-isolated session's worktree is cleaned at
stop. Never `kill -9` the pane while it holds unreleased claims — stop it
properly so claims release.
