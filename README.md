# Agent Rally Point

> Rally is a repo-local coordination room for coding agents working in the
> same checkout: durable facts, current room state, next-action guidance, and
> write-boundary checks without a server.

The only shipped coordination command is `rally`.

## Install as a Claude Code plugin

```bash
claude plugin marketplace add tyroneross/agent-rally-point
claude plugin install agent-rally-point@agent-rally-point
```

Hooks (SessionStart auto-coordination + PreToolUse write-boundary checks) and skills (`agent-rally-point`, `rally-workflows`, `mini-loop`) activate automatically on install. The `rally` CLI binary is auto-provisioned on first session.

## Start Here

New agent or human dropping in? Read [`RALLY.md`](RALLY.md). It is the short
operating guide for the Rally loop.

The full product boundary is [`docs/RALLY_ARCHITECTURE.md`](docs/RALLY_ARCHITECTURE.md).

The long-term coordination protocol model is
[`docs/PROTOCOL-NORTH-STAR.md`](docs/PROTOCOL-NORTH-STAR.md).

Command read/write behavior is summarized in
[`docs/COMMAND-SEMANTICS.md`](docs/COMMAND-SEMANTICS.md).

The build-loop implementation and Claude/Codex dogfood plan is
[`docs/PLAN-protocol-claim-authority-dogfood.md`](docs/PLAN-protocol-claim-authority-dogfood.md).

Checkout and active-ledger migration rules live in
[`docs/CANONICAL-CHECKOUT-MIGRATION.md`](docs/CANONICAL-CHECKOUT-MIGRATION.md).

Automatic Claude Code / Codex hook wiring (`SessionStart` + `PreToolUse`) is
documented in [`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md).

## Status

Rally owns the primary product path:

- `rally` is the primary CLI.
- `.rally/log/<engagement>.jsonl` is the durable fact store, including managed
  sessions.
- `.rally/facts.db` is a derived sqlite cache rebuilt from the log.
- Linked git worktrees share one room through the repo's git common dir.
- Room state is derived from the fact store on demand.
- `enter`, `next`, `say`, `room`, `check`, `run`, `sessions`, `inject`,
  `attach`, `capture`, `stop`, `locate`, `recent`, `lead`, `backlog`, and
  `mission` are the load-bearing commands.
- Managed sessions own live delivery into tmux, cmux, and ptyd panes.

Network transport remains out of scope. Files, Git, rsync, shared folders, A2A,
or a future service can move facts; Rally defines what the bytes mean.

## Install

Install the primary Rally CLI from the checkout:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally whoami --tool codex --json
rally enter --tool codex --json
rally ack --tool codex
rally next --tool codex --json
```

## Rally Command Surface

The primary loop is intentionally small:

```bash
rally whoami --tool codex --json
rally enter --tool codex --json
rally ack --tool codex
rally next --tool codex --json
rally check before-write --tool codex --path crates/foo.rs --strict --json
rally say artifact --tool codex --subject "implementation complete" --uri crates/foo.rs --evidence "cargo test" --json
rally room --json
```

The autonomous act-on-next contract is:

```text
whoami -> enter -> ack -> next -> if actionable, claim/check -> execute
       -> verify -> say artifact/handoff/resolve/release -> next
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

Resolve handoff targets from live Rally state, not from examples or old logs.
Use `rally whoami`, `rally lead show`, `rally next`, `rally room --json`, and
explicit handoff targets to identify the current recipient. A targeted handoff
is the durable action request; `rally inject` is only a wake/delivery path for a
target already listed by `rally sessions --json`.

For `--handoff`, `rally inject` waits for target-authored Rally evidence by
default. Text reaching a pane is not proof that the receiving agent read or
acted on it. If the target does not post `resolve`, `receipt`, `artifact`,
`blocker`, or `decision` for the expected handoff before the timeout, the
inject result returns `ack_state: "timeout"`, `verified_received: false`, and a
`fallback_plan`. Treat that as not received: check Rally state and assigned
file movement, retry once with a short doorbell, then move the work to a
separate worktree, hand it to another live agent, or escalate.

Managed sessions need no setup step. `rally run --backend <auto|tmux|cmux|ptyd>`
starts the addressable pane/workspace, and `rally inject` delivers work to it.
The default `auto` path uses a rally-owned ptyd daemon when its socket is live,
otherwise it falls back to tmux.
The legacy `herdr` backend (and its `--herdr-bin` / `--herdr-socket` flags)
were removed in Plan F: rally writes to the `.rally` ledger and the
`rally-termd` daemon subscribes, replacing the previous "rally calls the
daemon" path.
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
- `~/.agent-rally-point/rooms/v1/index.json` is an opt-in global discovery
  hint (pointers-only, no canonical data). It is off by default; enable with
  `RALLY_GLOBAL_INDEX=1`. `RALLY_NO_GLOBAL_INDEX=1` force-disables it even
  when opted in.
- JSON contracts are designed for agents first.
- Managed session delivery is how Rally becomes part of normal agent
  behavior.

See [docs/RALLY_ARCHITECTURE.md](docs/RALLY_ARCHITECTURE.md) for the full
per-repo segmentation contract.

## Vendored tools

- [`tools/agent-rally-watcher/`](tools/agent-rally-watcher/) — vendored snapshot of
  the standalone [agent-rally-watcher](https://github.com/tyroneross/agent-rally-watcher)
  Python daemon (v0.1.1, Apache-2.0), kept as a legacy reference for the
  push-based watcher/dispatch surface. Superseded by the planned native
  [`rally watch`](docs/SPEC-rally-watch-autonomy.md) subcommand. See
  [`tools/agent-rally-watcher/MIGRATION.md`](tools/agent-rally-watcher/MIGRATION.md)
  for the path-lineage table and the relationship to
  [`docs/CANONICAL-CHECKOUT-MIGRATION.md`](docs/CANONICAL-CHECKOUT-MIGRATION.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
