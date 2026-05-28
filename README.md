# Agent Rally Point

> Rally 2 is a repo-local coordination room for coding agents working in the
> same checkout: durable facts, current room state, next-action guidance, and
> write-boundary checks without a server.

The only shipped coordination command is `rally2`.

## Start Here

New agent or human dropping in? Read [`RALLY.md`](RALLY.md). It is the short
operating guide for the Rally 2 loop.

The full product boundary is [`docs/RALLY_2_ARCHITECTURE.md`](docs/RALLY_2_ARCHITECTURE.md).

## Status

Rally 2 owns the primary product path:

- `rally2` is the primary CLI.
- `.rally2/facts.db` is the durable fact store.
- `.rally2/room.db` is the rebuildable SQLite room projection.
- `enter`, `next`, `say`, `room`, `check`, `install`, `run`, `sessions`,
  `inject`, `attach`, `capture`, and `stop` are the load-bearing commands.
- Adapter setup installs write-boundary guards where available; managed
  sessions own live delivery into tmux, Herdr, and cmux panes.

Network transport remains out of scope. Files, Git, rsync, shared folders, A2A,
or a future service can move facts; Rally defines what the bytes mean.

## Install

Install the primary Rally 2 CLI from the checkout:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally2-cli
rally2 enter --tool codex --json
rally2 next --tool codex --json
```

## Rally 2 Command Surface

The primary loop is intentionally small:

```bash
rally2 enter --tool codex --json
rally2 next --tool codex --json
rally2 check before-write --tool codex --path crates/foo.rs --strict --json
rally2 say artifact --tool codex --subject "implementation complete" --uri crates/foo.rs --evidence "cargo test" --json
rally2 room --json
```

The autonomous act-on-next contract is:

```text
enter -> next -> if actionable, claim/check -> execute -> verify
      -> say artifact/handoff/resolve/release -> next
```

`rally2 next` returns `actionable`, `requires_human`, `stop_reason`,
`suggested_claims`, `suggested_commands`, and `completion` so an agent harness
can act without turning Rally into a scheduler or coding agent.

Useful fact writes:

```bash
rally2 say claim --tool codex --subject "edit parser" --path crates/rally2-cli/src/main.rs --json
rally2 say release --tool codex --ref <claim-id> --subject "done" --json
rally2 say blocker --tool codex --subject "need merge decision" --severity high --json
rally2 say resolve --tool codex --ref <blocker-id> --subject "resolved" --json
rally2 say decision --tool codex --subject "Rally 2 is primary" --status binding --json
rally2 say handoff --tool codex --target claude_code --subject "review docs" --summary "Rally 2 is now primary" --json
```

Adapter setup:

```bash
rally2 install codex --dry-run --json
rally2 install codex --json
rally2 install all --json
rally2 install codex --uninstall --json
```

Rally 2 installers write only Rally 2-owned hook scripts, snippets, extensions,
and config entries.

## Verification

Rust is the acceptance path:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
git diff --check
```

The workspace declares `rust-version = "1.85"`, so primary code must compile on
Rust 1.85 even when newer stable compilers accept newer syntax.

## Architecture

The short version:

- Rally 2 is the primary product.
- The product model is room, fact, enter, next, say, check.
- The room projection is SQLite-backed derived state.
- JSON contracts are designed for agents first.
- Adapter integration is how Rally becomes part of normal agent behavior.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
