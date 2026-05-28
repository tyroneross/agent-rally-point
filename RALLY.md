# RALLY 2 - the 60-second guide

Rally 2 is the primary Agent Rally Point path. It gives coding agents a shared
repo-local room: what is owned, blocked, handed off, decided, produced, and what
to do next.

## The Load-Bearing Commands

```bash
rally2 enter --tool <you> --json
rally2 next --tool <you> --json
rally2 check before-write --tool <you> --path <path> --strict --json
rally2 say artifact --tool <you> --subject "<what changed>" --uri <path> --evidence "<verification>" --json
rally2 room --json
```

That is the core loop. `enter` shows the room, `next` gives a concrete action
contract, `check` protects shared boundaries, `say` records durable facts, and
`room` inspects the current projection.

## Identify Yourself

Pick a stable `tool` id and use it across sessions. Peers address you by this
id.

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
enter repo/session
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
rally2 say handoff --tool claude_code \
  --target codex \
  --subject "implement the auth refactor from docs/plans/auth-v2.md" \
  --summary "tests in tests/auth_test.rs should still pass" \
  --json

# Codex:
rally2 enter --tool codex --json
rally2 next --tool codex --json
# ... claims/checks, does the work, verifies ...
rally2 say artifact --tool codex \
  --subject "auth refactor implemented" \
  --uri docs/plans/auth-v2.md \
  --evidence "cargo test" \
  --json
rally2 next --tool codex --json
```

## How Agents Wire This In

Install Rally 2 adapter glue for the host:

```bash
rally2 install codex --dry-run --json
rally2 install codex --json
rally2 install claude_code --json
rally2 install pi --json
rally2 install all --json
```

Write-boundary adapters should call `rally2 check before-write` before shared
edits. Managed sessions should use `rally2 run` and `rally2 inject` to deliver
work into tmux, Herdr, or cmux panes.

## Useful Fact Writes

```bash
rally2 say claim --tool <you> --subject "edit parser" --path crates/rally2-cli/src/main.rs --json
rally2 say release --tool <you> --ref <claim-id> --subject "done" --json
rally2 say blocker --tool <you> --subject "need decision" --severity high --json
rally2 say resolve --tool <you> --ref <blocker-id> --subject "resolved" --json
rally2 say decision --tool <you> --subject "Rally 2 is primary" --status binding --json
rally2 say risk --tool <you> --subject "adapter not installed everywhere" --severity medium --json
```

## Where State Lives

```text
.rally2/facts.db  canonical fact store
.rally2/room.db   derived SQLite room projection
```

The room database is disposable derived state. The fact store is the source of
truth.

## Install

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally2-cli
rally2 enter --tool <you> --json
```
