<!--
SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Command Semantics

This table is the agent-facing contract for whether a command changes durable
coordination state, local caches, or managed runtime state. It exists so audit
and review agents can choose commands deliberately instead of assuming every
JSON command is read-only.

Definitions:

- **Ledger write:** appends or rotates canonical `.rally/log/**` or legacy
  replay state.
- **Cache write:** may create, rebuild, or update derived local files such as
  `.rally/facts.db`, `.rally/cursors.json`, or room discovery hints.
- **Runtime write:** starts, injects into, adopts, attaches to, captures from,
  stops, or otherwise touches a managed terminal/backend surface.
- **Audit-safe:** safe for review use when small derived cache writes are
  acceptable. It does not mean "no filesystem writes."

## Primary Loop

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally whoami` | no | yes | no | yes | Self-locates repo, host runtime, lead, mission, and build id; may open/rebuild the room cache. |
| `rally enter` | yes | yes | no | no | Records presence, lead context, build-id drift, duplicate tool risks, and read cursor advancement. |
| `rally ack` | yes | yes | no | no | Records that the tool ingested current rules, guardrails, lead, and mission. |
| `rally next` | yes | yes | no | no | Projects actionable work and records wake/read state for the calling tool. |
| `rally next --audit` | no | no | no | no | Projects the same actionable work without presence, wake, or read-checkpoint facts; derived caches may still rebuild. |
| `rally room` | no | yes | no | yes | Projects current room state from the ledger; use for ownership/blocker inspection. |
| `rally check before-write` | no | yes | no | yes | Evaluates claim/decision/risk state; hooks may pair it with a separate claim write. |
| `rally say <kind>` | yes | yes | no | no | Appends durable coordination facts: claim, release, blocker, resolve, decision, artifact, handoff, risk, lesson, standby, wake, backlog-item, mission. |

## Managed Sessions

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally run` | yes | yes | yes | no | Starts Claude/Codex/OpenCode/Gemini in a managed worktree or shared checkout. Backends: `auto`, `tmux`, `cmux`, `ptyd`. |
| `rally sessions` | no | yes | optional | yes | Lists managed sessions; `--reap` tombstones stale sessions and is not audit-safe. |
| `rally inject` | yes | yes | yes | no | Writes a directive and may deliver through ptyd/tmux/cmux; `--handoff` waits for target-authored evidence. |
| `rally attach` | no | yes | yes | no | Attaches to a managed runtime surface when supported by that backend. |
| `rally capture` | no | yes | yes | no | Reads managed session output through the backend. Treat as runtime-touching even though it does not mutate the ledger. |
| `rally stop` | yes | yes | yes | no | Stops/tombstones a managed session. |
| `rally adopt` | yes | yes | yes | no | Registers an already-running tmux/cmux target as managed. |

## Inspection And Maintenance

| Command | Ledger write | Cache write | Runtime write | Audit-safe | Notes |
|---|---:|---:|---:|---:|---|
| `rally recent` | no | yes | no | yes | Reads recent room facts; `--all` remains scoped by global-index settings. |
| `rally locate` | no | yes | no | yes | Locates an event id in known room segments. |
| `rally status --global` | no | yes | no | yes | Workspace-scoped overview of indexed rooms; does not write facts. |
| `rally hooks status` | no | no | no | yes | Shows effective hook policy after session, repo, user, and default resolution. |
| `rally hooks on/off` | no | no | no | no | Writes `.rally/config.json` or `~/.config/rally/config.json`; hook runtime reads it before room work. |
| `rally hooks prompt` | no | no | no | no | Writes startup prompt mode (`once`, `always`, `off`) for repo or user scope. |
| `rally board` | no | yes | no | yes | Projects in-flight claims and backlog from facts. |
| `rally dag` | no | yes | no | yes | Read-only causation view for a run id. |
| `rally wake-due` | no | yes | no | yes | Read-only standby projection; emits suggested commands, never executes them. |
| `rally mission` | no | yes | no | yes | GET is read-oriented; `--set`, `--may`, and `--must-check` append mission facts. |
| `rally backlog list` | no | yes | no | yes | Listing is read-oriented; `add` and `done` append facts. |
| `rally check-ci` | no | yes | no | yes | Read-only CI health gate; strict mode changes exit code, not ledger state. |
| `rally doctor` | no | yes | no | yes | Dry inspection by default; `--apply` rewrites the discovery index and is not audit-safe. |
| `rally retrospective` | no | yes | no | yes | Writes the requested retrospective output file, not ledger facts. |
| `rally rotate` | yes | yes | no | no | Moves old segments into archive unless `--dry-run` is used. |
| `rally migrate-legacy` | yes | yes | no | no | Replays legacy room data into repo-local ledger segments. |
| `rally init` | yes | yes | no | no | Creates/refreshes manifest and doc pointer blocks. |
| `rally version` | no | no | no | yes | Pure process metadata. |
| `rally watch` | no | yes | optional | conditional | `--once`/projection-only use is audit-friendly; `--on-activity` executes an external command. |
| `rally route-findings` | yes | yes | no | no | Converts verified findings into risks or handoffs. |
| `rally worktree-gc` | optional | yes | yes | no | Dry-run is inspection; apply removes worktrees/branches after its safety checks. |

## Simplification Direction

Do not simplify Rally by deleting established commands first. Agents already
depend on the command names and JSON envelopes.

Simplify in this order:

1. Keep the command names stable and group them in docs by user intent.
2. Move each command implementation out of `crates/rally-cli/src/lib.rs` into a
   command module with one owner and focused tests.
3. Add a true no-write audit mode after the command semantics are executable
   enough to enforce, not just documented.
4. Only then consider aliases or deprecations, with compatibility warnings and
   envelope-contract tests.
