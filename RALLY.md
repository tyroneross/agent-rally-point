# RALLY — the 60-second guide

Rally is a local-first message bus so coding agents working in the same
repo can see each other and hand off work. This file is the minimum
surface you need; everything else in `docs/` is reference.

## The load-bearing commands

```bash
rally <you>                                                       # session start
rally judge --tool <you> --phase idle --json                      # decide what now
rally hook before-write --tool <you> --path <path> --auto-claim   # write gate
rally inbox --tool <you> --since-cursor --session-id <id> --json  # mid-session check
rally handoff --to <peer> --subject "<what you're handing off>"   # give work away
rally ack --tool <you> <handoff-id> --summary "<what you did>"    # close work
```

That's the load-bearing surface. Startup gives context, judgment tells
you whether to continue, hooks protect boundaries, and handoff/ack close
obligations.

## Identify yourself

Pick a stable `tool` id and use it across sessions. Peers address you
by this id.

| Host          | tool id      |
|---------------|--------------|
| Claude Code   | `claude_code`|
| Codex CLI     | `codex`      |
| Pi            | `pi`         |
| Cursor        | `cursor`     |
| Gemini CLI    | `gemini`     |
| CI/automation | `ci`         |

## The loop

```
session start
  └─ rally preflight                  → peers? pending handoffs?
       ├─ action: proceed_solo         → do your work
       └─ action: join_active          → handle pending ACKs first

mid-session, at meaningful boundaries
  └─ rally inbox --since-cursor       → what's new since last check?

when you're done with a slice another agent should pick up
  └─ rally handoff --to <peer> ...    → posts a handoff event

when preflight/inbox shows a handoff addressed to you
  └─ do the work, then
     rally ack <handoff-id> ...       → closes the obligation
```

Start with `rally <tool>`, judge at boundaries, use hooks before writes
and commits, handoff to delegate, ack to close. That's the whole protocol.

## Concrete example

Claude has finished planning a refactor and wants Codex to implement:

```bash
# Claude:
rally handoff --to codex \
  --subject "implement the auth refactor from docs/plans/auth-v2.md" \
  --notes "tests in tests/auth_test.rs should still pass"

# Codex (next session):
rally preflight --tool codex --start-ping --json
# → sees pending_acks_for_me: [{id: evt_..., subject: "implement the auth refactor..."}]
# ... does the work ...
rally ack --tool codex evt_... --summary "done, all auth tests green at abc1234"
```

## How agents wire this in

One command per host. Run it, then launch the agent:

```bash
rally pi      && pi          # Pi
rally claude  && claude      # Claude Code
rally codex   && codex       # Codex CLI
rally start <tool>           # Cursor, Gemini, anything else
```

`rally <tool>` writes presence, runs preflight, and emits the JSON
brief (peers, pending handoffs, recent changes, recommended next
action) the agent reads on its first turn. The agent picks up the
loop from there: `judge` when deciding, `hook before-write` before
editing, `inbox --since-cursor` at boundaries, `handoff` to delegate,
`ack` to close.

If you use herdr, `rally setup install herdr` chains these together
automatically — open a `rally pi` pane and you're done.

The bundled skill at `skills/agent-rally-point/SKILL.md` (linked
into Claude/Codex/Pi via `~/.agents/bin/sync-agent-skills`) is what
teaches each agent the loop above.

## What to ignore unless you have a reason

The `rally` CLI has ~25 other commands (`claim`, `blocker`, `task`,
`artifact`, `decision`, `lesson`, `subscribe`, `profile`, `diagnose`,
`score`, `report`, `replay`, `verify`, `sync export/import`,
adapter packets, etc.). They're real, but they're not the loop.
Reach for them when you have a specific need:

- **`claim` / `release`** — when two agents might touch the same file
  concurrently and you want a soft lock visible in `rally conflicts`.
- **`blocker` / `unblock`** — when you're stuck on something only
  another agent can resolve.
- **`watch`** — when a daemon needs to block on new events
  (notify/inotify-driven; not for interactive agents).
- **`post`** — escape hatch for event kinds we haven't typed yet.
- **`sync export` / `sync import`** — cross-machine via files / git /
  rsync / shared folder. No network transport built in by design.
- **`identity init` / `--sign` / `verify`** — only if you actually need
  cross-machine trust. Single-machine multi-agent doesn't.

Everything else is reference; reach for `docs/SCHEMA.md` and
`docs/RUST_GREENFIELD_ARCHITECTURE.md` when you need them.

## Where state lives

```
~/.agent-rally-point/apps/<repo_id>/
├── changes.jsonl         ← source of truth, append-only
├── rally/cursors/        ← per-(tool, session) read cursors
└── rally/...             ← derived projections, checkpoints
```

`<repo_id>` is derived from `git remote get-url origin` when present,
else the git root, else `cwd`. Worktrees and clones of the same repo
share the channel.

## Install

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally preflight --tool <you> --json
```
