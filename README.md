# Agent Rally Point

**Coordinate coding agents working in the same Git repository.** Rally gives agents in separate terminals and harnesses a shared record of who owns each change, what needs a response, and what evidence is available for review.

For example, Claude Code can claim a parser change while Codex reviews the tests. Both read the same room. The reviewer can acknowledge an exact handoff and post findings against it. The lead can see which work is open and inspect the result before accepting or integrating it.

## What Rally does

- **Share the current state of work.** Agents join a repository room with a distinct session identity and see its lead, assignments, claims, decisions, blockers, and results. Git worktrees share the room through their common Git directory.
- **Prevent conflicting claims.** Agents can share context and read in parallel. Rally refuses overlapping exclusive claims in the same scope. Configured edit checks advise by default and can block on supported paths when strict enforcement is enabled. Managed launches use isolated worktrees by default.
- **Track handoffs through review.** Rally records the request, any delivery attempt, the recipient’s acknowledgement or acceptance, and evidence of completion as distinct steps. A managed route can send a live prompt when the target and backend support it. Agents without a live route can pick up work through the CLI.
- **Launch and coordinate on demand.** An explicit `rally run` starts a supported agent harness. [Rally Flow](dynamic-workflows/README.md) validates workstream plans and derives which tasks are ready. The lead or host chooses what to dispatch, evaluates the results, and owns integration.

See the [latest GitHub release](https://github.com/tyroneross/agent-rally-point/releases/latest) for downloadable builds and the [changelog](CHANGELOG.md) for changes by version.

## Install with a coding agent (recommended)

Paste this into a coding agent that can run shell commands:

```text
Set up Agent Rally Point for this Git repository using
https://github.com/tyroneross/agent-rally-point and its README.
Confirm the target repo and active host. Use the latest published tag of the
Rally checkout. Preview the CLI install with scripts/install-rally.sh --dry-run,
then use the verified release installer.
Run rally init only in the target repo. Preserve existing agent instructions
and host settings. Use the manual CLI loop unless I request host hooks.
Report the binary path and version, the files changed, and the results of
rally doctor --json, rally hooks status, and rally whoami --json
with a unique --tool ID.
```

### Install manually

Choose a tag from [GitHub Releases](https://github.com/tyroneross/agent-rally-point/releases).
The installer uses the version declared by your checkout. Replace `v0.2.10`
below if you chose a different tag.

```bash
git clone --branch v0.2.10 https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
./scripts/install-rally.sh --dry-run
./scripts/install-rally.sh
rally --version
```

The release installer uses GitHub CLI (`gh`) to check build provenance and verifies the asset’s SHA256 before making it executable. Release binaries cover macOS and GNU/Linux on ARM64 and x86-64. To build the checked-out source, use `cargo install --locked --path crates/rally-cli`. The CLI declares Rust 1.89 as its minimum and the checkout pins the contributor toolchain. See [Upgrading Rally](docs/UPGRADING.md) before replacing an existing CLI or upgrading a room with a running coordinator.

In the Git repository your agents will share, run:

```bash
rally init
rally doctor --json
```

`rally init` creates repo-local Rally files and adds a fenced onboarding pointer to `AGENTS.md` and `CLAUDE.md`. Review the resulting diff. Hook activation is a separate host setup step.

### Verify a first result

Run this in a fresh disposable Git repo with the CLI on your PATH. The two pilot identities simulate a writer and a reviewer. No model or terminal backend is needed.

```bash
git init
rally init
export RALLY_SESSION_ID="pilot-writer-$(date +%s)-$$"
rally say claim --tool pilot:writer --path README.md --subject "Edit the README" --json

RALLY_SESSION_ID="pilot-reviewer-$(date +%s)-$$" \
  rally check before-write --tool pilot:reviewer --path README.md --strict --json
```

The claim command returns the exclusive claim’s `data.say.fact.event_id`. The strict check exits **4** and reports `data.check.allow: false`. Keep that writer session ID to release the claim via `rally say release --tool pilot:writer --ref <claim-event-id>`. A host hook needs its own activation and edit check. [Manual coordination guide](RALLY.md) · [Hook verification](docs/AUTO-COORDINATION-HOOKS.md)

## Host integrations

Any coding agent that can run shell commands can join a room through `rally`. Automatic hooks and direct prompt delivery depend on the host and runtime.

| Host | Available integration |
|---|---|
| Claude Code | Plugin with skills and hooks, or separate hook configuration |
| Codex | Plugin with skills. Automatic hooks use project configuration and the Rally hook script. |
| Muse | Skills and Claude-compatible hooks in the dedicated bundle |
| Cursor | Skills and hooks in the dedicated bundle, plus project hook configuration |
| Antigravity | Skills in the dedicated bundle |
| Gemini, OpenCode, and other CLI-capable hosts | Manual coordination loop. Managed launch is available for built-in targets and custom CLI commands. |

See [host plugin surfaces](docs/HOST-PLUGIN-SURFACES.md) for packaging, activation checks, and host-specific limits. Read the [trust model](docs/security/TRUST-MODEL.md) before enabling hooks.

## Managed sessions and handoffs

With the target harness installed and signed in, inspect a launch before starting it:

```bash
rally run codex --name test-review --task "Review the parser tests and report gaps with file references." --dry-run --json
rally run codex --name test-review --task "Review the parser tests and report gaps with file references." --json
rally sessions --json
```

Bounded Codex tasks write their final response to `.rally/task-results/`. Rally closes the session and removes its clean worktree or reports the retained recovery path. Interactive sessions can receive later handoffs. `rally run claude` is one interactive entry point. Its `--backend <auto|tmux|cmux|ptyd|ptyd-strict>` option controls the terminal runtime. Use [setup and route tests](docs/SETUP-AND-ROUTING.md) to check whether a specific recipient can receive a live prompt. Use [handoff receipts](docs/HANDOFFS-AND-LAUNCHING-AGENTS.md) to verify what a recipient actually acknowledged.

## Where state lives

Rally is local-first. The append-only ledger under `.rally/log/` is the canonical coordination record. `.rally/facts.db` is a rebuildable index. Commands can work directly against the local store. An optional per-repo coordinator serves the same state through a Unix socket. This project tracks its Rally manifest and keeps live ledger history local. Review the trust implications before opting to commit any room history.

Rally’s trust boundary is one operator coordinating trusted local processes. Ledger authorship is self-asserted, and same-user agents share operating-system privileges. Release binaries and CI cover macOS and Linux. Live integration evidence varies by host and route. [Architecture](docs/RALLY_ARCHITECTURE.md) · [Trust model](docs/security/TRUST-MODEL.md)

## More documentation

- [Rally guide](RALLY.md) for the everyday session loop and command examples
- [Any-agent onboarding](docs/ANY-AGENT-ONBOARDING.md) for harness and adapter authors
- [Rally Flow protocol](dynamic-workflows/PROTOCOL.md) for multi-agent workstreams
- [North Star](NORTH_STAR.md) for the product direction and operating boundaries
- [Contributing](CONTRIBUTING.md) for development and verification

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
