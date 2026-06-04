<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Auto-Coordination Hooks

Make rally presence + before-write deconfliction **automatic** for coding
agents (Claude Code, Codex, Gemini) without per-repo setup. Closes backlog
**B19-(a)** ("Claude PreToolUse hook — land separately") and the recurrence
risk documented in
[`assessment-2026-05-31-codex-hook-desync.md`](assessment-2026-05-31-codex-hook-desync.md).

## What the hook does

| Event | Action |
|------|--------|
| `SessionStart` | Calls `rally enter` so the agent registers presence on turn 1. Surfaces a short context line from `rally room` / `rally next` (active peers, claimed paths, suggested next) so a fresh agent is rally-aware without a manual nudge. |
| `PreToolUse(Edit\|Write\|MultiEdit)` | Extracts the target file path from the tool input envelope, calls `rally check before-write --path <p>`, and (when the path is unclaimed and the check allows) auto-claims it. On a conflict, surfaces an `additionalContext` warning to the agent. `rally check` already records the durable audit fact. |

**Charter — advisory-only (default).** Coordination is recorded + exposed,
never enforced. The hook NEVER emits `permissionDecision: "deny"` or
`decision: "block"` by default. Collisions warn; the agent decides.

**Strict mode (opt-in escape hatch).** `RALLY_HOOK_STRICT=1` enables hard
deny/block on high-severity collisions. Off by default. Documented as an
explicit deviation from the never-block charter for orchestration paths that
want hard gates.

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

- Permanent: `scripts/install_rally_hooks.sh --uninstall`.
- One session: `unset` the hook entry from `~/.claude/settings.json` for that
  session, or override `RALLY_BIN` to point at `/bin/true` (which exits 0
  immediately and produces no envelope — the translator emits `{}`, a valid
  empty hook output).

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
