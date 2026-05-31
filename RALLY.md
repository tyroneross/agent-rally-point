# Rally - the 60-second guide

Rally is the primary Agent Rally Point path. It gives coding agents a shared
repo-local room: what is owned, blocked, handed off, decided, produced, and what
to do next.

> **First time in a repo?** Run `rally init` once. It writes
> `.rally/manifest.json` (machine-readable self-description) and injects a
> fenced rally pointer into the repo's `CLAUDE.md` and `AGENTS.md` so any
> agent landing there sees how to enter and where the deeper docs live.
> Idempotent — re-running `rally init` refreshes the pointer block between
> stable markers without duplicating anything.

## The Load-Bearing Commands

```bash
rally enter --tool <you> [--tier frontier|executing|fast] --json   # --tier: first frontier agent auto-leads
rally next --tool <you> --json
rally check before-write --tool <you> --path <path> --strict --json
rally say artifact --tool <you> --subject "<what changed>" --uri <path> --evidence "<verification>" --json
rally room --json
```

That is the core loop. `enter` shows the room, `next` gives a concrete action
contract, `check` protects shared boundaries, `say` records durable facts, and
`room` inspects the current projection.

## Identify Yourself

Pick a stable `tool` id and use it across sessions. Peers address you by this
id.

For managed sessions, `rally run` appends readable ids from the active room.
The first default Claude session is `claude-01` with tool `claude_code:01`; a
named reviewer is `reviewer-01` with tool `claude_code:reviewer-01`.

| Host          | tool id       |
|---------------|---------------|
| Claude Code   | `claude_code` |
| Codex CLI     | `codex`       |
| Pi            | `pi`          |
| Cursor        | `cursor`      |
| Gemini CLI    | `gemini`      |
| CI/automation | `ci`          |

## The Agent Loop

```text
self-locate (rally whoami)   # host runtime, room, lead, mission, ambiguity — run FIRST
  -> enter repo/session + ack
  -> run next
     -> if actionable, claim/check
        -> execute and verify
           -> say artifact/handoff/resolve/release
              -> run next again
```

Stop and ask when `next.requires_human` is true, when `next.actionable` is
false, when a boundary check blocks, or when the work hits a real blocker.

## Concrete Example

Claude has finished planning a refactor and wants Codex to implement:

```bash
# Claude:
rally say handoff --tool claude_code \
  --target codex \
  --subject "implement the auth refactor from docs/plans/auth-v2.md" \
  --summary "tests in tests/auth_test.rs should still pass" \
  --json

# Codex:
rally enter --tool codex --json
rally next --tool codex --json
# ... claims/checks, does the work, verifies ...
rally say artifact --tool codex \
  --subject "auth refactor implemented" \
  --uri docs/plans/auth-v2.md \
  --evidence "cargo test" \
  --json
rally next --tool codex --json
```

## How Agents Wire This In

Managed sessions are the reliable delivery path:

```bash
rally run claude --backend tmux --json
rally inject <session|name|tool> --handoff <event-id> --json  # e.g. claude-01
rally capture <session|name|tool> --json
```

Agents can still call `rally check before-write` explicitly before shared
edits. Rally no longer installs host hooks or prompt injection glue.

## Discovery & Session Management

Beyond the core loop, Rally ships discovery and session-lifecycle commands:

```bash
rally sessions --json                          # list managed sessions in the room
rally attach <session|name|tool> --json        # attach to an existing managed session
rally capture <session|name|tool> --json       # capture a managed session's current output
rally stop <session|name|tool> --json          # stop a managed session
rally locate <event-id> [--include-legacy] --json   # find which channel an event lives in
rally recent [--all] [--limit N] [--include-legacy] --json   # recent activity across channels
```

`sessions`, `attach`, `capture`, and `stop` operate on managed sessions started
by `rally run`. `locate` and `recent` answer "where is this?" / "what just
happened?" across the channels Rally knows about; `--include-legacy` also scans
the retiring `~/.agent-rally-point/apps/` JSONL channels (the pre-`.rally/`
per-repo store, kept readable during migration).

## Useful Fact Writes

```bash
rally say claim --tool <you> --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say release --tool <you> --ref <claim-id> --subject "done" --json
rally say blocker --tool <you> --subject "need decision" --severity high --json
rally say resolve --tool <you> --ref <blocker-id> --subject "resolved" --json
rally say decision --tool <you> --subject "Rally is primary" --status binding --json
rally say risk --tool <you> --subject "managed session unavailable" --severity medium --json
```

## Lead & Backlog

```bash
# Lead-agent title — rally records/exposes only, never enforces (see COORDINATION.md):
rally lead show --json                                          # current lead, tier, how-assigned
rally lead handoff --tool <lead> --to <frontier-tool> --json    # transfer the title
rally lead assign  --tool <you> --to <tool> [--user-designated] --json   # set lead (user-designated supersedes first-join)
rally lead relinquish --tool <lead> --json                      # drop the title (reopens the seat)
# Lead auto-assigns to the first FRONTIER agent to enter (rally enter --tier frontier).

# Claimable backlog:
rally backlog add  --tool <you> --id <id> --intent "<what>" [--owns <path>] [--depends-on <id>] --json
rally backlog list --json                                       # OPEN items only
rally backlog done --tool <you> --id <id> --json                # close an item (drops out of list)
```

## Where State Lives

```text
.rally/log/<engagement>.jsonl   canonical, append-only per-engagement facts (R5; committed)
.rally/ledger.jsonl             legacy monolith — migrated into log/ on first open (R1)
.rally/archive/                 rotated old segments, still replayable (R7)
.rally/manifest.json            self-describing pointers (R4; committed)
.rally/facts.db                 derived sqlite cache — rebuilt by replay (gitignored)
.rally/cursors.json             per-tool read cursors
```

Linked git worktrees resolve this room from the shared git common dir, so the
main checkout and its worktrees coordinate through one `.rally/` store. The
**ledger files under `.rally/log/`** are the source of truth — append-only,
committed, durable across clone/machine. `facts.db` is a pure cache that
`rally` rebuilds from the ledger on first open. Managed session lifecycle
facts ride the same ledger.

## Install

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally enter --tool <you> --json
```
