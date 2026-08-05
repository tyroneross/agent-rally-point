# Agent Rally Point

**Run several coding agents in one repo without them overwriting each other. No server, no scheduler.**

Rally is a coordination room that lives in your checkout. Agents — Claude Code, Codex, Gemini, Cursor — enter the room, claim the files they are about to touch, hand work off with receipts, and read shared state through one CLI (`rally`) backed by an append-only log in `.rally/`. Nothing to host, nothing to reach over the network.

Apache-2.0. CLI version 0.1.7.

## The problem it solves

Two agents editing one checkout overwrite each other's uncommitted work, redo the same task, and lose the record of who decided what. The usual answers are a human babysitting a queue or an orchestration server. Rally is a third option: a durable, git-friendly record of who is doing what right now, that any agent reads and writes in one command.

Rally **advises by default; three opt-in switches make it block.** In the default posture a failing hook still lets your edit through — PreToolUse returns `permissionDecision: "allow"` with a warning, and every hook exits 0 even when Rally is broken. Setting `RALLY_HOOK_STRICT=1` turns a high-severity collision into a hard deny, `rally check before-write --strict` exits 4 on a stop finding, and `RALLY_BEFORE_WRITE_FAILCLOSED=1` makes that same check exit 4 when it times out. Each is off unless you turn it on. Full list and blast radius: [`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md) and [`docs/security/TRUST-MODEL.md`](docs/security/TRUST-MODEL.md).

## What you get

- **Deconfliction before an edit.** `rally check before-write` warns when another live agent has claimed the file you are about to touch. A PreToolUse hook fires it automatically.
- **Handoffs with proof of receipt.** A handoff waits for the receiver to write its own acknowledgement. Text landing in a terminal pane is not proof; an unacknowledged handoff returns `ack_state: "timeout"` and a fallback plan.
- **Room state on demand.** `rally room`, `rally risks`, and `rally next` project current claims, decisions, risks, and a recommended next action from the log.
- **One room, any host.** Claude Code, Codex, Gemini, and Cursor share a room; each agent passes its own `--tool` id.

## Prerequisites

| Tool | Required for | Without it |
|------|---------------|------------|
| `git` | Cloning the repo; `.rally/log/` is committed, append-only content. | No repo, no ledger. |
| `tmux` | The default backend for `rally run` / `rally inject` (`--backend auto` falls back to tmux when no `ptyd` socket is live — see `crates/rally-cli/src/backends.rs`). | Managed sessions cannot launch or receive injected messages, unless you configure `cmux` or a live `ptyd` daemon instead. |
| `node` | Rendering the committed hooks' JSON output — SessionStart's room summary, PreToolUse's deconfliction warning (`hooks/rally-coordination-hook.sh`). | Hooks still run their Rally side effects (enter, status, claims) but print no warning text; a one-line stderr notice names the gap once per session, and hooks still exit 0. |
| `python3` | `scripts/generate_host_surfaces.py --check` and `scripts/sync_host_integrations.py`, the host-surface drift checks under "Keeping hosts on one release" below. | Those two scripts don't run; the CLI and hooks are unaffected. |
| `gh` (GitHub CLI) | `scripts/install-rally.sh` only — it requires `gh attestation verify` (build-provenance check) before making the downloaded binary executable. | The installer refuses to install rather than fall back to an unverified download. Not needed for `cargo install --path crates/rally-cli`. |
| `jq` | ⚠️ Uncertain / optional — appears only in dev-facing scripts (`scripts/coordination-smoke.sh`, `scripts/check-release-parity.sh`, `scripts/install_rally_hooks.sh`), not in the documented install or `rally` CLI paths. | Those specific scripts fail if run directly; nothing else is affected. |

## Install

**As a Claude Code plugin:**

```bash
claude plugin marketplace add tyroneross/agent-rally-point
claude plugin install agent-rally-point@agent-rally-point
```

That installs the hooks and three skills (`agent-rally-point`, `rally-workflows`, `mini-loop`). Install the CLI yourself, as a separate deliberate step:

```bash
scripts/install-rally.sh          # --dry-run prints the plan and writes nothing
```

The installer verifies a SHA256 **and** a build-provenance attestation (`gh attestation verify`, pinned to the release workflow) before it makes the downloaded file executable. If either check cannot complete — no `gh`, no network, no published checksum — it refuses and prints why. It never falls back to an unverified download.

**From source:**

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
```

Building from source needs Rust 1.89 or newer — the MSRV pinned in `Cargo.toml`'s `rust-version`. For a reproducible build (matching `rustfmt`/`clippy` output), use the exact toolchain `rust-toolchain.toml` pins (currently 1.95.0); `rustup` picks it up automatically inside this checkout.

Hooks also need `node` on PATH to render their output — SessionStart's room summary and PreToolUse's deconfliction warning are both built by parsing `rally`'s JSON in a small Node script. Without `node`, the hooks still run their Rally side effects (enter, status, claims), but no warning text is produced; they print a one-line notice naming the gap once per session (on stderr) and still exit 0.

**Check either path:**

```bash
rally whoami --tool codex --json    # host runtime, room, lead, mission, ack status
```

## Opening this repo runs its hooks

This repo commits hook registrations for four hosts: `.claude/settings.json`, `.codex/hooks.json`, `.cursor/hooks.json`, and `hooks/hooks.json`. **Opening the repo in one of those hosts and trusting it loads those hooks.** That is the intended design and the reason coordination needs no setup step. It is also a trust decision, so here is what runs.

| Event | What the hook does |
|-------|--------------------|
| SessionStart | Registers presence, reads room state, prints a sanitized summary. Names the install command when `rally` is missing. |
| PreToolUse (edits) | Checks whether another live agent claimed the path. Advisory. |
| UserPromptSubmit | Refreshes idle status. |
| Stop | Records that the write finished. |

The hooks **do not download, build, `chmod`, or install anything**. Provisioning was removed from every lifecycle hook after an external security audit (finding ARP-001). They self-gate on a missing `.rally/`, so they no-op in unrelated repos, and they exit 0 even when Rally is broken.

**Turning them off:**

| Scope | Command |
|-------|---------|
| This session | `RALLY_HOOKS=off` |
| This repo | `rally hooks off --scope repo` |
| Check current state | `rally hooks status` |

What the hooks do and what Rally does not defend: [`docs/security/TRUST-MODEL.md`](docs/security/TRUST-MODEL.md).

## The loop

What an agent does each turn:

```text
whoami → enter → ack → next → (if actionable) claim → check before-write → edit
       → verify → say artifact|handoff|resolve|release → next
```

```bash
rally enter --tool codex --json
rally ack   --tool codex
rally next  --tool codex --json
rally check before-write --tool codex --path crates/rally-cli/src/main.rs --strict --json
rally say claim    --tool codex --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say artifact --tool codex --subject "parser hardened" --uri crates/rally-cli/src/main.rs --evidence "cargo test" --json
rally say handoff  --tool codex --target claude_code --subject "review docs" --json
rally say resolve  --tool codex --ref <blocker-id> --subject "resolved" --json
rally room --json
```

The `--strict` on `check before-write` above is one of the three blocking switches: it exits 4 when a stop finding is present, so a harness that reads the exit code aborts the write. Drop `--strict` to get the warning without the non-zero exit.

`rally next` returns `actionable`, `requires_human`, `stop_reason`, `suggested_claims`, `suggested_commands`, and `completion` — enough for a harness to act on its own without turning Rally into a scheduler. Every command takes `--json`.

Resolve handoff targets from live room state (`rally whoami`, `rally lead show`, `rally room --json`), never from examples or old logs.

## Room signal

`rally room` shows **human coordination risks only**. System telemetry — `unmanaged-agent`, `duplicate-active-squad-id`, `binary-drift`, `external-intake` — projects into a separate subject-deduped `system_health` bucket (surfaced as `system_health=N`), so the risk view stays worth reading. Read one kind at a time instead of hand-parsing JSON:

```bash
rally risks --json        # human coordination risks only
rally decisions --json
rally artifacts --json
rally claims --json
```

## Managed sessions

```bash
rally run claude                                  # becomes claude-01, tool claude_code:01
rally run claude --backend <auto|tmux|cmux|ptyd>  # auto = ptyd if live, else tmux
rally inject <session|name|tool> --handoff <event-id> --json
```

**`rally inject` returns `ok: true` when a message is enqueued, which is not the same as delivered.** The receive side has no resident owner yet (RC-001 in the register). Treat the target's own ACK as proof, not the inject's exit code.

## How it works

- **One repo, one rally point.** Coordination lives at `<repo_root>/.rally/`, never co-mingled across repos. Linked git worktrees share one room through the git common dir.
- **`.rally/log/<engagement>.jsonl` is canonical** — append-only, committed, `merge=union`. `.rally/facts.db` is a derived SQLite cache, rebuilt by replaying the log when it is missing or behind.
- **Room state is derived on demand**, so no live server state can be lost.
- **Network transport is out of scope.** Files, Git, rsync, or a shared folder move the facts; Rally defines what the bytes mean.

## Design tradeoffs

Three decisions shape everything above, and each cost something. [`docs/DESIGN-TRADEOFFS.md`](docs/DESIGN-TRADEOFFS.md) records what was tried, what broke, and what was chosen:

- **Hooks beat a hookless CLI.** Instructing agents to run the commands produced inconsistent compliance that failed silently, because a missed check looks identical to a repo where nobody else is working. Hooks made compliance near-universal and made the repo more intrusive.
- **Agents self-manage; a manager agent was rejected.** A manager would turn the substrate into a scheduler and a single point of failure. Rally fixed the observability that made silence ambiguous instead — mandated check-ins, worktree isolation for no-shows, lease expiry on claims.
- **Push where available, pull as the floor.** Direct pane delivery arrives now and lets two agents argue a design question in real time. The ledger is what the protocol guarantees.

## Security and maturity

Rally assumes **one operator, on one machine, running agents you started yourself.** Every agent runs as your UID, so Rally coordinates them and cannot sandbox them — a coordination layer cannot be a privilege boundary between processes that all hold your privileges.

If a second contributor can land commits in your repo, read the trust model first. `.rally/log/*.jsonl` is committed content that replays on clone, and facts carry no signature, so review those diffs the way you review code — they steer agents.

An independent audit (issue #52) produced seven findings in August 2026: three Critical, one High, two Medium, one Low. Six are closed with tests that fail when the fix is reverted. One — Cockpit's approval gate, which observes tool calls but does not stop them — has a documented fail-safe and an open redesign. Per-finding disposition: [`docs/security/AUDIT-2026-08-02-issue-52-triage.md`](docs/security/AUDIT-2026-08-02-issue-52-triage.md). Known open defects live in [`docs/ROOT-CAUSE-REGISTER.md`](docs/ROOT-CAUSE-REGISTER.md), where an entry closes only once an adversarial test proves the control fires.

**Maturity, stated plainly:** Rally runs daily on a small number of fresh macOS installs driven by one operator. It is not proven on Linux beyond CI, on hosts other than the four wired here, or with more than one human. Expect edge cases outside that envelope.

## Start here

- [`RALLY.md`](RALLY.md) — the 60-second operating guide. Read this first.
- [`docs/RALLY_ARCHITECTURE.md`](docs/RALLY_ARCHITECTURE.md) — per-repo segmentation contract and product boundary.
- [`docs/COMMAND-SEMANTICS.md`](docs/COMMAND-SEMANTICS.md) — read/write behavior per command.
- [`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md) — how the host hook wiring works.
- [`dynamic-workflows/PROTOCOL.md`](dynamic-workflows/PROTOCOL.md) — the workstream descriptor for fanning several agents out on one objective.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — development setup and the verification bar.

## Keeping hosts on one release

Rally generates every host manifest, hook setting, skill frontmatter, and packaged Codex artifact from `config/host-integrations.json` plus the CLI version in `crates/rally-cli/Cargo.toml`. Generated files carry the same release identity and content digest, and the release gate rejects drift.

```bash
python3 scripts/generate_host_surfaces.py --check
python3 scripts/sync_host_integrations.py --json          # read-only diagnosis
python3 scripts/sync_host_integrations.py --apply --json  # reconcile installed hosts
```

The reconciler requires exactly one enabled provider per host. It removes stale duplicates, updates from the canonical marketplace, and reports when Claude Code or Codex must restart to load new content. It changes nothing without `--apply`.

## Verification

Rust is the acceptance path, and the pre-push gate runs it:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
git diff --check
```

Primary code must compile on Rust 1.89 (the MSRV in `Cargo.toml`). These verification commands themselves run under the exact toolchain `rust-toolchain.toml` pins (1.95.0) — `cargo fmt --check` needs a matching `rustfmt` build or its diff is meaningless.

## License

Apache-2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
