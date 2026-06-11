<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Auto-Coordination Hooks

Make rally presence + before-write deconfliction **automatic** for coding
agents (Claude Code, Codex, Gemini) without per-repo setup. Other agents can
still participate manually through the same Rally contract; see
[`ANY-AGENT-ONBOARDING.md`](ANY-AGENT-ONBOARDING.md). Closes backlog
**B19-(a)** ("Claude PreToolUse hook — land separately") and the recurrence
risk documented in
[`assessment-2026-05-31-codex-hook-desync.md`](assessment-2026-05-31-codex-hook-desync.md).

## Install — portable, ships in the repo (no global config)

The wiring **ships with this repo** so it works on **any user machine** without
touching `~/.claude` or `~/.codex`:

- `.claude/settings.json` — Claude Code hooks via `${CLAUDE_PROJECT_DIR}` (resolves
  to the repo root on any machine).
- `.codex/hooks.json` — Codex hooks via the git top-level (portable path).
- `.cursor/hooks.json` — Cursor hooks (schema v1) via the git top-level. Cursor's
  contract only injects an agent-visible message on `preToolUse` (`agent_message`);
  `sessionStart`/`stop` register presence / surface next-actions with no visible
  output. Advisory by default; `RALLY_HOOK_STRICT=1` turns a `preToolUse` collision
  into `permission: deny`.

Just open the repo in Claude Code / Codex / Cursor and **trust it on first prompt**. The
hook self-gates on `.rally/` presence (no-op elsewhere) and fail-opens (never
blocks an edit). This is the default and recommended path — nothing to run.
`SessionStart` shows a concise Rally-active prompt with the current off switch
so users know the repo is coordinated before work starts.

**Opt-in only — user-wide install across every repo on one machine** (edits your
global `~/.claude/settings.json`; per-machine, not portable):

```bash
scripts/install_rally_hooks.sh --global [--repoint-codex]    # install user-wide
scripts/install_rally_hooks.sh --uninstall [--repoint-codex] # revert
```

Running the installer **without `--global` does nothing** but print this guidance —
the portable project config is already committed.

> **Other repos that adopt rally:** copy `.claude/settings.json` + `.codex/hooks.json`
> into that repo, pointing the command at a `rally`-resolvable hook. The universal
> zero-bundle mechanism (a `rally hook` binary subcommand so a one-line committed
> config calls `rally hook …` with no script path) is tracked as a backlog item.
>
> **Cursor:** delivered as the project hook `.cursor/hooks.json` (Cursor has no
> plugin marketplace, so it is not a plugin install like Claude Code / Codex).
> Cursor's hook schema cannot inject context at `sessionStart`, so the
> session-start awareness/offer does not surface there; the safety-critical
> `preToolUse` before-write deconfliction does (`agent_message`). The
> `preToolUse` input envelope shape is matched against Cursor's tool input but is
> not yet validated against a live Cursor session — treat path-specific claims as
> best-effort until confirmed.
>
> **Other hosts:** Qwen, Gemma, Aider, IDE plugins, and custom CLIs do not get
> automatic hooks from this file today. Give them the any-agent bootstrap prompt,
> or wrap/adopt them into a managed backend before relying on direct injection.

## What the hook does

| Event | Action |
|------|--------|
| `SessionStart` | Resolves `rally hooks status`, calls `rally enter` when hooks are enabled, and surfaces a short context line from `rally room` / `rally next` (active peers, claimed paths, suggested next). Even in a quiet room, the default prompt tells the user Rally is active and shows the session/repo off commands. |
| `UserPromptSubmit` | Per-turn presence refresh (hook phase `idle`). Re-surfaces active peers / open claims / suggested next from `rally next` at the start of each turn, so Claude stays rally-aware during long read/explore phases instead of only re-checking at the moment it edits. Advisory `additionalContext`; emits `{}` when the room is quiet. This is the key parity fix — Codex already touches rally continuously; Claude previously only did so right before a write. |
| `PreToolUse(Edit\|Write\|MultiEdit)` | Extracts the target file path from the tool input envelope, calls `rally check before-write --path <p>`, and (when the path is unclaimed and the check allows) auto-claims it. On a conflict, surfaces an `additionalContext` warning to the agent. `rally check` already records the durable audit fact. |
| `Stop` | At turn end (hook phase `after-write`), runs `rally next` and surfaces any pending coordination obligation as an advisory `systemMessage`. Parity with Codex's `Stop` hook; never blocks turn completion (strict mode is the only path that can emit `decision: block`). |


**Why PreToolUse stays edit-scoped (deliberate).** Codex's `.codex/hooks.json` wires PreToolUse with *no matcher*, so it fires `before-write` on every tool call — consistent, but it spawns the hook + watchdog on reads/bash/etc. that have no file path and no-op. Claude keeps the `Edit|Write|MultiEdit|NotebookEdit` matcher for `before-write` and instead gets continuous awareness from the cheaper `UserPromptSubmit` (idle) refresh + `Stop` (after-write). Same coordination footprint as Codex, without per-tool overhead.

**Charter — advisory-only (default).** Coordination is recorded + exposed,
never enforced. The hook NEVER emits `permissionDecision: "deny"` or
`decision: "block"` by default. Collisions warn; the agent decides.

**Strict mode (opt-in escape hatch).** `RALLY_HOOK_STRICT=1` enables hard
deny/block on high-severity collisions. Off by default. Documented as an
explicit deviation from the never-block charter for orchestration paths that
want hard gates.

## Hook Policy And Opt-Out

Hooks are default-on for repos with `.rally/`, then resolved in this order:

1. Session env: `RALLY_HOOKS=off|on` and `RALLY_HOOK_PROMPT=once|always|off`.
2. Repo config: `.rally/config.json`.
3. User config: `~/.config/rally/config.json`.
4. Built-in default: hooks enabled, prompt once.

Commands:

```bash
rally hooks status --json
rally hooks off --scope repo
rally hooks on --scope repo
rally hooks off --scope user
rally hooks prompt --once --scope repo
rally hooks prompt --always --scope repo
rally hooks prompt --off --scope repo
```

One-session opt-out:

```bash
RALLY_HOOKS=off
```

## What ships

| Path | Role |
|------|------|
| `hooks/rally-coordination-hook.sh` | Single source of truth. Host-neutral; argv-dispatched by `<phase> <tool>`. Self-gates on missing `.rally/` (silent no-op). Defense-in-depth wall-clock watchdog with process-group-kill on overrun so a hung `rally` can never stall a host session. |
| `scripts/install_rally_hooks.sh` | Idempotent installer that wires the hook into `~/.claude/settings.json`. Supports `--uninstall`, `--dry-run`, `--repoint-codex`. Pure shell (no Rust changes); resolves the repo path to an absolute string at install time. |
| `tests/hooks/test_rally_coordination_hook.sh` | Self-gate, fail-open (missing + hung binary), advisory-only invariant, strict-mode, warn-never-denies. |
| `tests/hooks/test_install_rally_hooks.sh` | Install-from-empty, idempotency, preserves unrelated hooks, uninstall round-trip, `--dry-run`, codex repoint round-trip. Uses scratch HOME — never touches the user's real settings. |

## Install / uninstall

```bash
# Install for Claude Code (writes ~/.claude/settings.json)
scripts/install_rally_hooks.sh

# Also repoint ~/.codex/rally-hook.sh at the in-repo versioned script (opt-in)
# This is the durable fix for "loose-file desync" — closes the recurrence risk
# called out in docs/assessment-2026-05-31-codex-hook-desync.md.
scripts/install_rally_hooks.sh --repoint-codex

# Show what would change without writing
scripts/install_rally_hooks.sh --dry-run

# Remove (leaves unrelated hooks alone)
scripts/install_rally_hooks.sh --uninstall

# Remove both Claude entries AND the codex shim (restoring its .bak if present)
scripts/install_rally_hooks.sh --uninstall --repoint-codex

# Quiet
scripts/install_rally_hooks.sh --quiet
```

The installer is idempotent: re-running with no changes prints `no change`.

## Exact JSON the installer writes

Merged into `~/.claude/settings.json` (preserves any existing entries):

```jsonc
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "<repo>/hooks/rally-coordination-hook.sh start claude_code"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [
          {
            "type": "command",
            "command": "<repo>/hooks/rally-coordination-hook.sh before-write claude_code"
          }
        ]
      }
    ]
  }
}
```

`<repo>` is resolved to the absolute checkout path at install time (Claude Code
does not reliably expand `~` in command strings).

When `--repoint-codex` is passed, `~/.codex/rally-hook.sh` is replaced (after
backing up to `.bak`) with a thin shim:

```sh
#!/usr/bin/env bash
# Auto-installed by <repo>/scripts/install_rally_hooks.sh
# Delegates to the version-controlled hook so it cannot desync from the CLI.
exec "<repo>/hooks/rally-coordination-hook.sh" "$@"
```

## Self-gating

The hook walks up from `$PWD` looking for a `.rally/` directory. If none is
found, the hook exits 0 with no output. Side effect: the same hook installation
is safe in every repo on the machine. Only repos with active rally rooms get
coordination signal.

## Fail-open

Any rally CLI error, timeout, or missing binary causes the hook to exit 0
silently. The hook MUST NEVER stall a host session. Specifically:

- **Missing binary** — detected up front (`command -v RALLY_BIN`) → exit 0.
- **Hung binary** — bounded by a wall-clock watchdog (default 5s per call;
  `RALLY_HOOK_TIMEOUT_MS` ms env override). The watchdog kills the entire
  process group on overrun (fixed a leak in the earlier `exec`-based shim
  where grandchildren kept the captured stdout FD open and stalled `$(...)`).
- **Malformed rally output** — JSON parse failure → empty envelope.

## Strict mode

```bash
export RALLY_HOOK_STRICT=1
```

When set, the hook may emit `permissionDecision: "deny"` (PreToolUse) or
`decision: "block"` (Stop) on **high-severity** signals only (`severity==stop`
or `allow==false`). Low-severity warnings always remain advisory.

This contradicts the never-block charter (`rally mission` — *"records and
exposes only; never enforces"*) and is documented as a deliberate escape hatch
for orchestration paths where the operator wants hard gates. Use sparingly.

## Disabling per session

- Repo: `rally hooks off --scope repo`.
- User: `rally hooks off --scope user`.
- One session: set `RALLY_HOOKS=off`.
- Legacy global uninstall: `scripts/install_rally_hooks.sh --uninstall`.

## Why a hook (vs. lazy auto-enter)?

The lazy-auto-enter direction (every `rally check before-write` call auto-
registers presence with no bespoke hook) remains the long-term goal — see
`assessment-2026-05-31-codex-hook-desync.md` § "Lazy auto-enter (no hook)".
Until that lands as the agent's default reflex, the hook closes the gap for
**Claude Code today**: agents do not reliably self-invoke skill or CLI
patterns mid-task (memory: `feedback_subagent_skill_reactivity`), so the
host's PreToolUse mechanism is the deterministic surface.

## Recurrence risk closure

`docs/assessment-2026-05-31-codex-hook-desync.md` flagged that the loose
`~/.codex/rally-hook.sh` desynced from the CLI when `0d5024b` removed the
`hook` subcommand. That file lived outside the repo, so it could not be
caught by tests. **This document's hook lives in-repo**, is exercised by
`cargo test --workspace` adjacent tests in `tests/hooks/`, and is the single
source of truth that `~/.codex/rally-hook.sh` delegates to when
`--repoint-codex` is used. The desync can no longer happen silently.

## Cross-refs

- [`assessment-2026-05-31-codex-hook-desync.md`](assessment-2026-05-31-codex-hook-desync.md) — recurrence-risk source.
- [`../NORTH_STAR.md`](../NORTH_STAR.md) — durable vision + never-block charter.
- [`../RALLY.md`](../RALLY.md) — 60-second guide.
- [`../BACKLOG.md`](../BACKLOG.md) — B19-(a) close-out anchor.
