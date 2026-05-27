# Agent Rally Point

> Rally 2 is a repo-local coordination room for coding agents working in the
> same checkout: durable facts, current room state, next-action guidance, and
> write-boundary checks without a server.

Rally's product direction is Rally 2. The primary user-facing command is now
`rally2`. The legacy `rally` CLI remains available for compatibility,
migration, and older adapter workflows, but new product behavior should target
Rally 2 unless it is explicitly a migration bridge.

Rally 2 builds on Jason's rewrite and act-on-next contract work in PRs #42/#43.
This follow-up promotes that work as the default path and hardens compatibility
around it.

## Start Here

New agent or human dropping in? Read [`RALLY.md`](RALLY.md). It is the short
operating guide for the Rally 2 loop.

The full product boundary is [`docs/RALLY_2_ARCHITECTURE.md`](docs/RALLY_2_ARCHITECTURE.md).

## Status

Rally 2 owns the primary product path:

- `rally2` is the primary CLI.
- `.rally2/facts.jsonl` is the durable append-only coordination log.
- `.rally2/room.db` is the derived SQLite room projection.
- `enter`, `next`, `say`, `room`, `check`, and `install` are the load-bearing
  commands.
- Adapter setup injects `enter` and `next` into Codex, Claude Code, Pi, Herdr,
  cmux, and CI surfaces where available.

Legacy Rally remains in the workspace as a deprecated compatibility surface:

- `rally` still supports the existing `changes.jsonl` channel, sync, trust,
  diagnosis, packet, and older setup workflows.
- Existing hooks and users do not need an immediate hard cutover.
- Do not add new primary agent-loop behavior to `rally` unless it helps migrate
  users or keep old channels readable.

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

Install the deprecated legacy CLI only when an older integration still needs it:

```bash
cargo install --path crates/rally-cli
rally preflight --tool codex --json
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
and config entries. They may report older Rally hooks but do not silently delete
them.

## Legacy Compatibility

Use legacy `rally` only for existing channels or older adapter surfaces:

```bash
rally preflight --tool codex --start-ping --json
rally context --tool codex --json
rally packet --tool codex --json
rally sync export --json > packet.json
rally sync import --trust-policy <trust.toml> packet.json --json
rally verify --json --trust-policy <trust.toml> <changes.jsonl>
```

The legacy CLI is still tested, but it is no longer the design center. Treat it
as a compatibility layer while Rally 2 reaches parity on the workflows that
matter.

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
- Legacy `rally` remains available for compatibility, not future primary design.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
