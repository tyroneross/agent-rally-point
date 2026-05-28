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

## Contract-Aware Claims (predictive collisions)

A claim can declare the contracts it `produces` (changes) and `depends`
on, optionally pinned to a version with `@<hash>`. When one agent's active
claim changes a contract another agent's active claim depends on, Rally
surfaces it as a `stale_base` finding in `next` and in `enter` attention —
*before* the change reaches a merge.

```bash
# Producer: declares the contract it changes
rally say claim --tool codex --subject "soft-delete users" \
  --produces scope.active_users@b1c2 --json

# Consumer: pins the version it built against
rally say claim --tool claude_code --subject "user search" \
  --depends scope.active_users@a3f9 --json

# Consumer is warned its base is shifting (confirmed: pins differ)
rally next --tool claude_code --json   # -> data.next.stale_bases[]
```

A *confirmed* stale base (both sides pinned, hashes differ) sets
`data.next.coordination_required = true` and each finding carries a
`suggested_command` (raise a blocker referencing the producer) so the agent
can reconcile before building further. This is advisory: it never forces
`requires_human`. An unpinned overlap stays informational
(`confirmed: false`, gate not tripped).

## Receipts (handoff lifecycle)

`rally room` derives a **receipt** for every handoff by walking its `ref`
chain — no extra commands, no new facts. Each receipt reports a `state`:

```text
delivered -> acted -> completed     (claim refs it -> artifact refs it)
              \-> blocked            (an open blocker refs it)
              \-> acknowledged       (a resolve refs it, no artifact)
```

```bash
rally room --json   # -> data.room.receipts[]
# { "handoff_event_id": "...", "state": "completed",
#   "evidence": ["cargo test --all"], "self_reported": true, "chain": [ ... ] }
```

`self_reported` is always `true`: Rally *renders* the chain, it does not run
it. The `evidence` is the producer's own claim, and `state` reflects which
facts were written — not whether the work is actually correct.

## CI gate

`rally check ci --strict` reads the whole room and **fails the build**
(exit 4) on unreconciled coordination state:

- `confirmed-stale-base` — a peer changes a contract another agent pinned a
  different version of (stop)
- `active-blocker` — work is blocked (stop)
- `open-handoff` — delegated work is still in flight (warn)

```yaml
- run: cargo run -p rally-cli -- check ci --tool ci --strict --json
```

Agents are nudged to declare contracts two ways, so coverage doesn't depend
on memory: the injected handoff prompt tells them to, and every
`rally check before-write` returns a `declare-contracts` reminder.

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
