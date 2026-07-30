# Agent Rally Point

**Run many coding agents in one repo without them clobbering each other — no server, no scheduler.**

Rally is a repo-local coordination room. Agents — Claude Code, Codex, Gemini, Cursor — enter the room, claim the files they're about to touch, hand work off with delivery receipts, and read shared room state, all through one CLI (`rally`) backed by an append-only fact log in `.rally/`. There is no daemon to run and no service to reach: the coordination lives in your checkout.

## Why it exists

When two agents work the same checkout in parallel, they overwrite each other's uncommitted edits, redo each other's work, and lose track of who decided what. The usual fixes are a human babysitting a queue or a heavyweight orchestration server. Rally is neither — it is a durable, git-friendly record of *who is doing what, right now* that any agent reads and writes in one command, and that **advises, never blocks**.

## What you get

- **Deconfliction before edits.** `rally check before-write` warns when a file you're about to touch is claimed by another live agent — surfaced automatically through a PreToolUse hook.
- **Handoffs that are actually received.** A handoff waits for target-authored evidence (the receiver posts a receipt / artifact / resolve). Text landing in a pane is not proof; an unacknowledged handoff returns `ack_state: "timeout"` and a fallback plan.
- **Room state on demand.** `rally room`, `rally risks`, and `rally next` project current claims, decisions, risks, and the recommended next action from the fact log — no server round-trip.
- **Host-neutral by design.** One room coordinates Claude Code, Codex, Gemini, and Cursor; each agent passes its own `--tool` id.

## Install

**As a Claude Code plugin (recommended):**

```bash
claude plugin marketplace add tyroneross/agent-rally-point
claude plugin install agent-rally-point@agent-rally-point
```

Hooks (SessionStart coordination + PreToolUse write-boundary checks) and skills (`agent-rally-point`, `rally-workflows`, `mini-loop`) activate on install. The `rally` CLI is **auto-provisioned on first session** — downloaded from GitHub Releases (SHA256-verified) or built from source as a fallback. Nothing else to run.

**From source (manual / development):**

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
```

**Verify either path:**

```bash
rally whoami --tool codex --json   # host runtime, room, lead, mission, ack status
```

## Keep Claude Code and Codex on one canonical release

Rally derives every host-facing manifest, hook setting, skill frontmatter, and
packaged Codex artifact from `config/host-integrations.json` plus the CLI version
in `crates/rally-cli/Cargo.toml`. Generated files carry the same release identity
and content digest, including the identity inside Codex's installed
`.codex-plugin` cache root, and the release gate rejects drift.

```bash
python3 scripts/generate_host_surfaces.py --check
python3 scripts/sync_host_integrations.py --json          # read-only diagnosis
python3 scripts/sync_host_integrations.py --apply --json  # reconcile installed hosts
```

The reconciler requires exactly one enabled provider per host:
`agent-rally-point@agent-rally-point`. It removes stale duplicate providers,
updates from the canonical marketplace, and reports when Claude Code or Codex
must restart to load the new content. It does not mutate anything unless
`--apply` is passed.

## The loop

What an agent does each turn — small on purpose:

```text
whoami → enter → ack → next → (if actionable) claim → check before-write → edit
       → verify → say artifact|handoff|resolve|release → next
```

`rally next` returns `actionable`, `requires_human`, `stop_reason`, `suggested_claims`, `suggested_commands`, and `completion` — enough for a harness to act on its own without turning Rally into a scheduler or a coding agent. JSON contracts are designed for agents first; every command takes `--json`.

```bash
rally enter --tool codex --json
rally ack   --tool codex
rally next  --tool codex --json
rally check before-write --tool codex --path crates/rally-cli/src/main.rs --strict --json
rally say claim    --tool codex --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say artifact --tool codex --subject "implementation complete" --uri crates/rally-cli/src/main.rs --evidence "cargo test" --json
rally say handoff  --tool codex --target claude_code --subject "review docs" --summary "Rally is now primary" --json
rally say resolve  --tool codex --ref <blocker-id> --subject "resolved" --json
rally room --json
```

Resolve handoff targets from live room state (`rally whoami`, `rally lead show`, `rally room --json`) — never from examples or old logs.

## Room signal (v0.1.6)

`rally room` shows only **human coordination risks**. System-generated telemetry — `unmanaged-agent`, `duplicate-active-squad-id`, `binary-drift`, `external-intake` — projects into a separate, subject-deduped `system_health` bucket (count surfaced as `system_health=N`), so the risk view stays trustworthy. Read one kind at a time without hand-parsing JSON:

```bash
rally risks --json        # human coordination risks only
rally decisions --json
rally artifacts --json
rally claims --json
```

## Managed sessions

Managed sessions make Rally part of normal agent behavior — live delivery into real panes, no setup step:

```bash
rally run --backend <auto|tmux|cmux|ptyd>   # start an addressable pane; auto = ptyd if live, else tmux
rally run claude                            # convenience: becomes claude-01, tool claude_code:01
rally inject <session|name|tool> --handoff <event-id> --json
```

Rally assigns readable per-agent ids from the room: `rally run claude` becomes `claude-01` with tool `claude_code:01`.

## Start here

- **[`RALLY.md`](RALLY.md)** — the 60-second operating guide for the loop. Read this first.
- **[`docs/RALLY_ARCHITECTURE.md`](docs/RALLY_ARCHITECTURE.md)** — the full per-repo segmentation contract and product boundary.
- **[`docs/COMMAND-SEMANTICS.md`](docs/COMMAND-SEMANTICS.md)** — read/write behavior per command.
- **[`docs/PROTOCOL-NORTH-STAR.md`](docs/PROTOCOL-NORTH-STAR.md)** — the long-term coordination protocol model.
- **[`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md)** — automatic Claude Code / Codex hook wiring.

## How it works

- **One repo = one rally point.** Coordination lives at `<repo_root>/.rally/`, segmented per-repo, never co-mingled. Linked git worktrees share one room through the git common dir.
- **`.rally/log/<engagement>.jsonl` is canonical** — append-only, committed, `merge=union`. `.rally/facts.db` is a derived sqlite cache, rebuilt by replaying the log when missing or behind.
- **Room state is derived on demand** from the fact log; there is no live server state to lose.
- **Network transport is out of scope.** Files, Git, rsync, shared folders, or a future service can move the facts; Rally defines what the bytes *mean*.

## Verification

Rust is the acceptance path (also enforced by the pre-push gate):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
git diff --check
```

The workspace declares `rust-version = "1.85"`; primary code must compile on Rust 1.85.

## License

Apache-2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
