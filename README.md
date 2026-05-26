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
rally preflight --channel-dir /tmp/rally-smoke --tool codex --json
```

During the current cutover, commands take an explicit `--channel-dir`:

```bash
rally preflight --channel-dir ~/.agent-rally-point/apps/<repo_id> --tool codex --start-ping --json
```

Rust repo/channel discovery is the next product cutover target.

## Command Surface

Core coordination commands:

```bash
rally preflight --channel-dir <dir> --tool codex --start-ping --json
rally handoff --channel-dir <dir> --to pi --from-tool codex --subject "review sync"
rally ack --channel-dir <dir> --tool pi <handoff-id> --summary "done"
rally claim --channel-dir <dir> --tool codex --path crates/rally-core/src/query.rs --subject "query cleanup"
rally blocker --channel-dir <dir> --tool codex --subject "need decision"
```

Read projections:

```bash
rally inbox --channel-dir <dir> --tool codex --json
rally claims --channel-dir <dir> --json
rally blockers --channel-dir <dir> --json
rally conflicts --channel-dir <dir> --json
rally diagnose --channel-dir <dir> --json
rally score --channel-dir <dir> --json
rally thread --channel-dir <dir> <event-id> --json
rally replay --channel-dir <dir> --json
rally report --channel-dir <dir> --json
```

Trust and sync:

```bash
rally identity init --tool codex --json
rally handoff --channel-dir <dir> --identity-dir <identity-dir> --sign --to pi --subject "signed handoff"
rally verify --json --trust-policy <trust.toml> <changes.jsonl>
rally sync export --channel-dir <dir> --json > packet.json
rally sync import --channel-dir <dir> --trust-policy <trust.toml> packet.json --json
```

## Session Start

Every coding agent should start with preflight:

```bash
rally preflight --channel-dir <dir> --tool <host> --start-ping --json
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
