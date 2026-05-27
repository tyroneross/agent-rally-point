# Agent Rally Point

> Local-first coordination for coding agents working in the same repo: durable
> events, handoffs, claims, blockers, trust, and sync without a server.

Rally's ambitious direction is attuned coordination: a repo-native intelligence
layer that helps agents anticipate what each other need, using durable facts,
trust, ownership, blockers, artifacts, decisions, and source-linked lessons.

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
rally pi
rally start codex
rally preflight --tool codex --start-ping --json
rally profile --tool codex --role builder --capability rust --capability implementation --watch crates/rally-core --json
rally task --tool codex --subject "finish context ranking" --status active --verification "cargo test" --json
rally artifact --tool codex --subject "context schema" --artifact-kind schema --uri docs/context.schema.json --json
rally decision --tool codex --subject "agents use rally context for next action" --status binding --json
rally lesson --tool codex --subject "avoid giant planning docs as control surfaces" --lesson-kind coordination --json
rally subscribe --tool codex --path crates/rally-core --event-kind task --event-kind decision --json
rally handoff --to pi --from-tool codex --subject "review sync"
rally ack --tool pi <handoff-id> --summary "done"
rally claim --tool codex --path crates/rally-core/src/query.rs --subject "query cleanup"
rally blocker --tool codex --subject "need decision"
```

Read projections:

```bash
rally inbox --tool codex --json
rally context --tool codex --json
rally packet --tool codex-reviewer --json
rally adapter contract --json
rally cmux packet --tool codex-reviewer --json
rally herdr packet --tool codex-reviewer --json
rally checkpoint rebuild --json
rally checkpoint status --json
rally claims --json
rally blockers --json
rally conflicts --json
rally diagnose --json
rally score --json
rally thread <event-id> --json
rally replay --json
rally report --json
```

`rally context` is the agent-facing intelligence layer. It returns the
recommended next action plus `attuned_items`: scored, source-linked facts ranked
for that specific tool from active work, declared role, watched paths,
subscriptions, task links, trust labels, and recent changes.

`rally <tool>` is the canonical session-start surface for known harnesses such
as `pi`, `claude`, `codex`, `gemini`, and `cursor`. It defaults to JSON, writes
presence, returns preflight/context/packet/checkpoint/cursor state, and gives
the next watch command. `rally start <tool>` is the generic equivalent.

`rally packet` is the bounded work-brief surface for specialized agents. It is
read-only, derived from the same context projection, and shapes the JSON for the
requesting profile role: reviewer, builder, architect, QA, or general.

Adapter commands are edge exports over the same packet contract. `rally cmux
packet` and `rally herdr packet` emit side-effect-free adapter envelopes;
`rally adapter contract` documents the trust fields adapters must honor.
`rally checkpoint rebuild/status` manages the disposable hot-read checkpoint
used by query commands.

Trust and sync:

```bash
rally identity init --tool codex --json
rally handoff --identity-dir <identity-dir> --sign --to pi --subject "signed handoff"
rally verify --json --trust-policy <trust.toml> <changes.jsonl>
rally sync export --json > packet.json
rally sync import --trust-policy <trust.toml> packet.json --json
rally herdr inject --json <handoff-id>
```

Imported events retain their import `origin` and trust classification in the
local store. Agent-facing JSON projections such as `inbox`, `claims`,
`blockers`, `preflight`, and recent changes expose `origin` and `trust_status`
when present, so automation can distinguish local facts from imported or
untrusted facts.

## Session Start

Every coding agent should start with preflight:

```bash
rally preflight --tool <host> --start-ping --json
```

Use stable tool IDs such as `codex`, `claude_code`, `pi`, `cursor`, `gemini`,
or `ci`.

Agents that support skills should also install/use the bundled Rally skill at
[`skills/agent-rally-point/SKILL.md`](skills/agent-rally-point/SKILL.md). The
canonical cross-agent copy lives at `~/.agent-skills/agent-rally-point/SKILL.md`
and can be linked into Claude, Codex, Pi, and other agent clients with:

```bash
~/.agents/bin/sync-agent-skills
```

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
and [`docs/ATTUNED_COORDINATION.md`](docs/ATTUNED_COORDINATION.md) for the
target architecture. The short version:

- One Rust binary named `rally`.
- One durable source of truth: `changes.jsonl`.
- Store metadata is local replica state.
- Portable events are the sync/signature unit.
- CLI output is machine-readable JSON first.
- Agent integration is through the shared Rally skill and CLI JSON first;
  bridges to ACP, A2A, MCP, Herdr, and UI surfaces stay at the edge.
- `rally context --tool <agent> --json` is the first attuned briefing surface.
- `rally packet --tool <agent> --json` is the role-shaped work brief.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
