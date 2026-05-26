# Agent Rally Point

> Local-first coordination for coding agents working in the same repo: durable
> events, handoffs, claims, blockers, trust, and sync without a server.

Agent Rally Point is now a Rust product. The user-facing command is `rally`.
The older runtime package has been removed from the product architecture.

## Status

The greenfield Rust rewrite owns the product surface:

- `rally` is the installed CLI.
- `changes.jsonl` is the durable, append-only coordination log.
- `rally-core` owns typed events, store entries, queries, diagnosis, and
  preflight projection.
- `rally-trust` owns identity, signatures, verification, and trust policy.
- `rally-protocol` owns canonical JSON and portable event helpers.
- `rally-cli` is the current command parser/renderer and is being thinned.

Network transport is intentionally out of scope. Files, Git, rsync, shared
folders, A2A, or a future service can move sync packets; Rally defines what the
bytes mean.

## Install

Install the Rust CLI from the checkout:

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally preflight --tool codex --json
```

Run `rally` from inside a repo. The CLI derives the local coordination channel
under `~/.agent-rally-point/apps/<repo_id>` from the Git origin when available,
then the Git root, then the current directory as a final fallback.

```bash
rally preflight --tool codex --start-ping --json
```

## Command Surface

Core coordination commands:

```bash
rally preflight --tool codex --start-ping --json
rally handoff --to pi --from-tool codex --subject "review sync"
rally ack --tool pi <handoff-id> --summary "done"
rally claim --tool codex --path crates/rally-core/src/query.rs --subject "query cleanup"
rally blocker --tool codex --subject "need decision"
```

Read projections:

```bash
rally inbox --tool codex --json
rally claims --json
rally blockers --json
rally conflicts --json
rally diagnose --json
rally score --json
rally thread <event-id> --json
rally replay --json
rally report --json
```

Trust and sync:

```bash
rally identity init --tool codex --json
rally handoff --identity-dir <identity-dir> --sign --to pi --subject "signed handoff"
rally verify --json --trust-policy <trust.toml> <changes.jsonl>
rally sync export --json > packet.json
rally sync import --trust-policy <trust.toml> packet.json --json
```

## Session Start

Every coding agent should start with preflight:

```bash
rally preflight --tool <host> --start-ping --json
```

Use stable tool IDs such as `codex`, `claude_code`, `pi`, `cursor`, `gemini`,
or `ci`.

Preflight returns `routing.action`:

- `proceed_solo` when no active peers or pending handoffs need attention.
- `join_active` when the agent has pending work or another peer is active in
  the channel.

## Verification

Rust is the acceptance path:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```

Do not use legacy compatibility gates for Rust-core work.

## Architecture

Read [`docs/RUST_GREENFIELD_ARCHITECTURE.md`](docs/RUST_GREENFIELD_ARCHITECTURE.md)
for the target architecture. The short version:

- One Rust binary named `rally`.
- One durable source of truth: `changes.jsonl`.
- Store metadata is local replica state.
- Portable events are the sync/signature unit.
- CLI output is machine-readable JSON first.
- Bridges to ACP, A2A, MCP, Herdr, and UI surfaces live outside the core.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
