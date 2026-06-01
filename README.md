# Agent Rally Point

> Rally is a repo-local coordination room for coding agents working in the
> same checkout: durable facts, current room state, next-action guidance, and
> write-boundary checks without a server.

The only shipped coordination command is `rally`.

## Start Here

New agent or human dropping in? Read [`RALLY.md`](RALLY.md). It is the short
operating guide for the Rally loop.

The full product boundary is [`docs/RALLY_ARCHITECTURE.md`](docs/RALLY_ARCHITECTURE.md).

Checkout and active-ledger migration rules live in
[`docs/CANONICAL-CHECKOUT-MIGRATION.md`](docs/CANONICAL-CHECKOUT-MIGRATION.md).

## Status

Rally owns the primary product path:

- `rally` is the primary CLI.
- `.rally/facts.db` is the durable fact store, including managed sessions.
- Linked git worktrees share one room through the repo's git common dir.
- Room state is derived from the fact store on demand.
- `enter`, `next`, `say`, `room`, `check`, `run`, `sessions`, `inject`,
  `attach`, `capture`, `stop`, `locate`, `recent`, `lead`, `backlog`, and
  `mission` are the load-bearing commands.
- Managed sessions own live delivery into tmux, Herdr, and cmux panes.

Network transport remains out of scope. Files, Git, rsync, shared folders, A2A,
or a future service can move facts; Rally defines what the bytes mean.

## Install

Install the primary Rally CLI from the checkout:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally enter --tool codex --json
rally next --tool codex --json
```

## Rally Command Surface

The primary loop is intentionally small:

```bash
rally enter --tool codex --json
rally next --tool codex --json
rally check before-write --tool codex --path crates/foo.rs --strict --json
rally say artifact --tool codex --subject "implementation complete" --uri crates/foo.rs --evidence "cargo test" --json
rally room --json
```

The autonomous act-on-next contract is:

```text
enter -> next -> if actionable, claim/check -> execute -> verify
      -> say artifact/handoff/resolve/release -> next
```

`rally next` returns `actionable`, `requires_human`, `stop_reason`,
`suggested_claims`, `suggested_commands`, and `completion` so an agent harness
can act without turning Rally into a scheduler or coding agent.

Useful fact writes:

```bash
rally say claim --tool codex --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say release --tool codex --ref <claim-id> --subject "done" --json
rally say blocker --tool codex --subject "need merge decision" --severity high --json
rally say resolve --tool codex --ref <blocker-id> --subject "resolved" --json
rally say decision --tool codex --subject "Rally is primary" --status binding --json
rally say handoff --tool codex --target claude_code --subject "review docs" --summary "Rally is now primary" --json
```

Managed sessions need no setup step. `rally run --backend <tmux|herdr|cmux>`
starts the addressable pane/workspace, and `rally inject` delivers work to it.
Use `--herdr-socket <path>` to target a private herdr-compatible daemon socket
such as Easy Terminal's `~/Library/Application Support/EasyTerminal/herdr.sock`;
the backend command receives both `PTYD_SOCKET_PATH` and `HERDR_SOCKET_PATH`.
When a private socket is supplied without `--herdr-bin`, Rally resolves a
`ptyd` client (PATH first, then common Easy Terminal bundled/dev locations) so
`rally run codex --backend herdr --herdr-socket <sock>` follows Easy Terminal's
live `ptyd agent start ... -- codex` path.
Rally assigns readable per-agent ids from the active room: `rally run claude`
becomes `claude-01` with tool `claude_code:01`; a named reviewer becomes
`reviewer-01` with tool `claude_code:reviewer-01`.

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

- Rally is the primary product.
- The product model is room, fact, enter, next, say, check.
- **One repo = one rally point.** Coordination lives at
  `<repo_root>/.rally/`, segmented per-repo, never co-mingled.
- **`.rally/log/<engagement>.jsonl` is canonical** — append-only,
  committed, `merge=union`. The legacy `.rally/ledger.jsonl` remains a
  replayable migration input.
- `.rally/facts.db` is a derived sqlite cache rebuilt by replaying the
  ledger when missing or behind.
- `~/.agent-rally-point/rooms/v1/index.json` is a global discovery hint
  (pointers-only, no canonical data). Disable with
  `RALLY_NO_GLOBAL_INDEX=1`.
- JSON contracts are designed for agents first.
- Managed session delivery is how Rally becomes part of normal agent
  behavior.

See [docs/RALLY_ARCHITECTURE.md](docs/RALLY_ARCHITECTURE.md) for the full
per-repo segmentation contract.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
