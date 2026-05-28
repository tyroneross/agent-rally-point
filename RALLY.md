# Rally - the 60-second guide

Rally is the primary Agent Rally Point path. It gives coding agents a shared
repo-local room: what is owned, blocked, handed off, decided, produced, and what
to do next.

## The Load-Bearing Commands

```bash
rally enter --tool <you> --json
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
rally inject <session|name|tool> --handoff <event-id> --json
rally capture <session|name|tool> --json
```

Agents can still call `rally check before-write` explicitly before shared
edits. Rally no longer installs host hooks or prompt injection glue.

## Useful Fact Writes

```bash
rally say claim --tool <you> --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say release --tool <you> --ref <claim-id> --subject "done" --json
rally say blocker --tool <you> --subject "need decision" --severity high --json
rally say resolve --tool <you> --ref <blocker-id> --subject "resolved" --json
rally say decision --tool <you> --subject "Rally is primary" --status binding --json
rally say risk --tool <you> --subject "managed session unavailable" --severity medium --json
```

## Where State Lives

```text
.rally/facts.db  canonical fact store
.rally/cursors.json per-tool read cursors
```

Room state is derived from the fact store on demand. The fact store is the
source of truth, including managed session lifecycle facts.

## Install

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally enter --tool <you> --json
```
