<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Auto-Coordination Hooks

Make Rally presence and mutation deconfliction **automatic** for coding
agents (Claude Code, Codex, Cursor) without serializing parallel reads. Other agents can
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

> **This is a trust decision, and it should be a deliberate one.** Trusting the repo
> auto-loads code that runs on your host at session start and before every edit. That
> is the point — instructing agents to run the commands themselves produced inconsistent
> compliance ([`DESIGN-TRADEOFFS.md`](DESIGN-TRADEOFFS.md) §1) — but it is not free.
>
> What the hooks do and do not do, and every off switch:
> [`security/TRUST-MODEL.md`](security/TRUST-MODEL.md).
>
> They **do not** download, build, `chmod +x`, or install anything. Provisioning was
> removed from the hook path entirely after the issue #52 audit (RC-013). Installing the
> `rally` binary is an explicit step you run: `scripts/install-rally.sh`, or
> `cargo install --path crates/rally-cli`.
>
> Peer-authored ledger prose reaching your context is sanitized, length-capped, and quoted
> as untrusted data (RC-016). It is still unsigned — treat it as data, never instructions.

**Opt-in only — user-wide install across every repo on one machine** (edits your
global `~/.claude/settings.json`; per-machine, not portable):

```bash
scripts/install_rally_hooks.sh --global [--repoint-codex]    # install user-wide
scripts/install_rally_hooks.sh --uninstall [--repoint-codex] # revert
```

Running the installer **without `--global` does nothing** but print this guidance —
the portable project config is already committed.

## Canonical generated host contract

`config/host-integrations.json` is the host-neutral contract. It defines plugin
identity, provider IDs, host-specific descriptions and keywords, hook cadence,
event matchers, native-tool effect classes, timeouts, and skill-frontmatter overlays. The CLI version remains
canonical in `crates/rally-cli/Cargo.toml`; the generator reads it rather than
duplicating it in the contract.

```bash
# Rewrite all derived Claude, Codex, Cursor, marketplace, skill, and artifact files.
python3 scripts/generate_host_surfaces.py

# Read-only drift gate used by release parity.
python3 scripts/generate_host_surfaces.py --check

# Inspect or reconcile the installed Claude Code and Codex providers.
python3 scripts/sync_host_integrations.py --json
python3 scripts/sync_host_integrations.py --apply --json
```

`rally-release.json` records the canonical provider, repository, release
version, and deterministic digest of the generated surfaces. The same identity
ships inside the Codex `.codex-plugin` payload so the installed cache can be
attested at its real `local/.codex-plugin` root. The host reconciler compares
the full identity, not version alone; it reports `current`, `restart_required`,
`uninstalled`, `stale`, `duplicate_provider`, or `unknown`. Diagnosis is
read-only and exits non-zero on drift. Apply mode refuses to mutate a host whose
installed state could not be read, reduces each host to
`agent-rally-point@agent-rally-point`, updates from the canonical marketplace,
stops on the first failed plugin-manager command, then requires a host restart.

This provides persistent source sync without silently changing a running agent:
canonical edits regenerate all host surfaces, CI rejects stale generated files,
and explicit reconciliation updates local host caches at a safe restart
boundary. `scripts/check-release-parity.sh` also runs the generator/reconciler,
hook, global-installer, and first-session provisioner suites in CI, pre-push,
and release jobs.

> **Other repos that adopt Rally:** treat the project wiring as a bundle. Copy
> `hooks/rally-coordination-hook.sh` (executable) into the target, then merge the
> needed generated hooks from `.claude/settings.json`, `.codex/hooks.json`, and/or
> `.cursor/hooks.json` into the target's corresponding host file—do not overwrite
> existing host settings wholesale. Each command resolves the target project root
> and requires a `rally` CLI on `PATH`. The universal zero-bundle mechanism (a
> `rally hook` binary subcommand so a one-line committed config calls `rally hook …`
> with no script path) is tracked as a backlog item.
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
> **Other hosts:** Gemini, Qwen, Gemma, Aider, IDE plugins, and custom CLIs do not get
> automatic hooks from this file today. Give them the any-agent bootstrap prompt,
> or wrap/adopt them into a managed backend before relying on direct injection.

## What the hook does

| Event | Action |
|------|--------|
| `SessionStart` | Resolves `rally hooks status`, calls `rally enter` when hooks are enabled, posts `state=idle` with a next check-in, and surfaces a short context line from `rally room` / `rally next` / `rally status read` (active peers, claimed paths, suggested next, agent status). Even in a quiet room, the default prompt tells the user Rally is active and shows the session/repo off commands. |
| `UserPromptSubmit` | Per-turn presence refresh (hook phase `idle`). Posts `state=idle`, then re-surfaces actionable `rally next` work plus peer status from `rally status read` when another live agent is working/idle/blocked/done. Advisory `additionalContext`; emits `{}` when the room is quiet or unchanged. This is the cadence parity fix for Claude and Codex. |
| `PreToolUse` — named pure read | Returns exactly `{}` before the wrapper's repo walk or Rally binary resolution. It posts no status, performs no check, and creates no claim. The generated Codex launcher still runs `git rev-parse` to locate the wrapper; O33-D measures that installed envelope separately. Read tool names come from `hooks.native_effects.pure_read`; generator tests require byte-for-byte parity with the wrapper registry. |
| `PreToolUse` — opaque shell | Returns exactly `{}` with zero Rally calls because command text is not a trustworthy effect or path declaration. A shell command that will mutate files must use the explicit agent loop: claim the exact target and run `rally check before-write --strict` before the mutation. |
| `PreToolUse` — native transaction | When the installed binary reports `before-write` in `rally hook capabilities`, the wrapper execs `rally hook before-write` and that one process does the whole transaction: envelope parse, O33-A classification, hooks-status, working status, per-target check, unowned filter, auto-claim, dedupe and host-envelope render, under a single self-enforced deadline. The rows below describe the behaviour, which is identical on both paths; a binary without the subcommand falls back to the shell/Node pipeline they originally described. `RALLY_NATIVE_HOOK=off` forces the fallback; `RALLY_HOOK_TRACE=1` emits one stderr line of per-stage timings. The capability verdict is cached per binary in `.rally/.hook-seen/native-probe.*.seen`, keyed on the binary's size and fractional mtime. |
| `PreToolUse` — named mutation | Extracts and canonicalizes every declared target against the validated event `cwd` and physical Rally root. It posts `state=working` once, checks every target before any claim, reads room ownership once, and then creates one aggregate repeated-path claim for the targets not already covered by that agent's claim. If any target denies, times out, errors, or escapes the root, the whole automatic claim is skipped; no prefix is claimed. Advisory `allow: true` warnings remain visible and do not turn into a denial. |
| `PreToolUse` — unknown or malformed | Inside an enabled Rally repo, fails open with exact `{}`, one bounded rate-limited stderr diagnostic, and zero Rally status/check/claim calls. Outside a Rally repo or when hooks are disabled, it is silent. It never treats an arbitrary `path` field as write ownership. |
| `Stop` | At turn end (hook phase `after-write`), posts `state=idle` with a next check-in, runs `rally next`, and surfaces any pending coordination obligation or peer status change as an advisory `systemMessage`. Parity with Codex's `Stop` hook; never blocks turn completion (strict mode is the only path that can emit `decision: block`). |

**Why the Codex matcher remains unset.** Claude's documented matcher keeps its
hook edit-scoped. OpenAI's Codex 0.144.3 source proves that `apply_patch` sends
patch text as `tool_input.command`, and the wrapper replays that exact shape;
`tool_input.patch` remains a named legacy-adapter carrier. That source does not
prove which matcher names and combinations an installed Codex host accepts, so
the generator deliberately emits no Codex matcher. The wrapper classifies the
native envelope before its repo walk or any Rally subprocess. Known reads still
pay for the host launcher plus one shell/JSON parse, but no Rally process or
ledger write. Narrowing the host matcher remains an O33-D optimization after a
live captured matcher test; it is not assumed from another host's vocabulary.
[Codex 0.144.3 source evidence](https://github.com/openai/codex/blob/rust-v0.144.3/codex-rs/core/src/tools/handlers/apply_patch.rs#L479-L483).
The generated Codex file uses only its accepted top-level `description` and
`hooks` keys. Claude and Codex handler timeouts are whole seconds, so the
generator converts the canonical 5,000/10,000 millisecond values to 5/10 before
rendering either host surface.

**Mutation targets are all-or-none.** `apply_patch` reads only `Add File`,
`Update File`, `Delete File`, and `Move to/from` directives, and those directive
paths must be relative to the validated event `cwd`. Other named mutation
envelopes such as Claude `Write`/`Edit` may carry an absolute `file_path`; the
wrapper accepts it only when physical containment resolves it inside the Rally
root. One identity-whitespace, empty, malformed, root-equal, outside-root, or
symlink-escaping target rejects the entire automatic route before Rally runs.
For a new path whose parent directories do not exist yet, containment starts at
the nearest physical existing ancestor and appends the unresolved suffix. An
unresolved suffix containing `..` rejects atomically rather than guessing
through a path that does not yet exist.
Only an absent target alias is optional: a present null/blank move destination
invalidates the whole mutation. A present `tool_input`, `toolInput`, or `input`
carrier must be an object and never falls back to an outer-envelope `path` when
malformed.
Native Windows drive/backslash paths are not proven on the supported
macOS/Linux wrapper and fail open as an explicit `UNKNOWN` platform case.
Rally path, file, intent, and claim-subject option values use attached
`--name=value` arguments, so a valid root filename such as `--evil` cannot be
reparsed as a CLI option.

The current wrapper accepts at most 16 mutation targets. A 17-target envelope
is rejected atomically with a diagnostic and zero Rally calls; it is never
silently truncated. This is a documented degraded-mode ceiling, not evidence
that 16 is an optimal product limit. For a larger mutation, the agent must
strict-check and claim every exact target explicitly before running it; a
future batch CLI primitive can remove the ceiling. The configured worst-case
Rally budget is 400 ms for hook settings, 400 ms for one working status,
at most 4,000 ms across all path checks, 400 ms for one room read, and 1,000 ms
for one claim: at most 6,200 ms, leaving 3,800 ms of orchestration margin under
the generated 10-second host timeout. At 16 targets each check receives 250 ms.
The outer watchdog sends immediate `KILL` at each millisecond deadline; it does
not add a per-call TERM grace period. If neither a millisecond-capable
`timeout`/`gtimeout` nor high-resolution Perl guard is available, a classified
mutation degrades before any Rally subprocess and creates no automatic claim.
These numbers prove a configured bound, not that a real host completes within
it; O33-D's quiesced installed-surface benchmark decides whether they should
change.

## Read versus write operation policy

Reads do not own a resource. They can run concurrently, including when another
agent is editing the same file. The reader needs writer context and a recheck,
not a lock.

| Operation | Claim or lock | Context and validation contract |
|---|---|---|
| Pure read | None | Use the latest turn-level active-writer context. If the path has an active writer, treat the bytes as provisional and do not wait merely for ownership. |
| Read for a decision, audit finding, or final conclusion | None | Capture the file digest plus Rally source sequence/active-claim reference. Re-run the scoped path read immediately before the conclusion; reread and recompute if either token changed. The automated source-token projection lands after the engagement/session work in S9/S10; until then this is an explicit agent obligation. |
| Read before write | No claim for the read; exact path claim and strict check before mutation | Revalidate after acquiring the write claim when the earlier read was provisional or its token changed. |
| Mutation | One aggregate exclusive claim after one before-write check for every target | Never substitute opaque shell text or a generic `path` field for declared targets. A multi-file patch checks all paths even when an earlier path conflicts; any denial or check failure creates zero claims. |
| Destructive or administrative mutation | Exact targets, explicit authority, recovery evidence, then the mutation checks above | Read-only context never authorizes deletion, migration, rotation, or room-wide cleanup. |

Turn-level context remains important even though per-read hooks bypass Rally:
`SessionStart` and `UserPromptSubmit` surface live writers, files, and intents.
The follow-on reader-context segment will bind that context to a stable
engagement/run, add a source token, and require final revalidation without
making the reader wait for the writer to finish.

**Activation hold:** O33-A may be committed only on its isolated branch. It must
not be merged, cherry-picked, or checked out into central integration, local
main, an installed plugin, a pushed ref, or any user-active worktree until
O33-B and O33-C are ready and the combined A+B+C gate passes. Build O33-B on top
of A in isolation; integrate the combined chain only after post-O26 O33-C is
complete. This hold is necessary because the project Codex and Claude hook
files are already active for new sessions, while turn-level writer context can
be stale or omit a relevant path. O33-C supplies the path-scoped active-writer
context, source token, and final revalidation that make the read bypass safe
for consequential work.

**Duplicate registration is safe.** Claude Code can load both the installed
plugin hooks and this repo's project hooks. Identical event envelopes share a
short-lived, locked source-count record in the git-common directory. The largest
per-source count is the number of logical events, so plugin/project/global hook
ordering cannot change the result. A repeated event from the same source always
runs; deduplication can never suppress a real retry or a strict-mode deny.

**Identity model.** The hook argv names the host family (`codex`,
`claude_code`, `cursor`, etc.). The routed Rally id must identify the working
agent/session, so bare host ids are expanded to `<host-family>:<agent-id>`.
Set `RALLY_AGENT_ID` to a unique string or number for this terminal/session or
worker; otherwise the hook derives a stable id from host session metadata.
Every concurrently working agent that posts Rally facts needs its own id.
`--session-id` is recorded as metadata only and does not route
handoffs/claims/presence.

**Status model.** The hook uses Rally's typed agent-state surface rather than a
host-specific side channel. Each working agent posts `rally status post` as it
moves between `idle`, `working`, `blocked`, and `done`; peers read the roster
with `rally status read --json`. Startup prompts include the live roster, and
per-turn prompts surface peer status changes so agents know who is working,
what file/intent they have, whether they are blocked or done, and when idle
agents expect to check in again.

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
| `config/host-integrations.json` | Canonical host-neutral plugin, provider, hook, and skill-frontmatter contract. |
| `scripts/generate_host_surfaces.py` | Deterministically generates host manifests/settings, skill frontmatter, release identity, and the dereferenced Codex artifact. `--check` is the drift gate. |
| `scripts/sync_host_integrations.py` | Read-only installed-host doctor by default; `--apply` removes duplicate providers, updates canonical caches, and reports restart requirements. |
| `hooks/rally-coordination-hook.sh` | Single source of truth. Host-neutral; argv-dispatched by `<phase> <tool>`. Self-gates on missing `.rally/` (silent no-op). Defense-in-depth wall-clock watchdog with process-group-kill on overrun so a hung `rally` can never stall a host session. |
| `scripts/install_rally_hooks.sh` | Idempotent installer that derives the global Claude hook entries from the generated project template, then rewrites only path and source scope using Python 3 or jq. Supports `--uninstall`, `--dry-run`, `--repoint-codex`. |
| `tests/hooks/test_rally_coordination_hook.sh` | Self-gate, fail-open (missing + hung binary), advisory-only invariant, strict-mode, warn-never-denies. |
| `tests/hooks/test_install_rally_hooks.sh` | Project hook cadence regression, install-from-empty, idempotency, preserves unrelated hooks, uninstall round-trip, `--dry-run`, codex repoint round-trip. Uses scratch HOME for global installer cases — never touches the user's real settings. |

## Install / uninstall

```bash
# Install for Claude Code user-wide (writes ~/.claude/settings.json)
scripts/install_rally_hooks.sh --global

# Also repoint ~/.codex/rally-hook.sh at the in-repo versioned script (opt-in)
# This is the durable fix for "loose-file desync" — closes the recurrence risk
# called out in docs/assessment-2026-05-31-codex-hook-desync.md.
scripts/install_rally_hooks.sh --global --repoint-codex

# Show what would change without writing
scripts/install_rally_hooks.sh --global --dry-run

# Remove (leaves unrelated hooks alone)
scripts/install_rally_hooks.sh --uninstall

# Remove both Claude entries AND the codex shim (restoring its .bak if present)
scripts/install_rally_hooks.sh --uninstall --repoint-codex

# Quiet
scripts/install_rally_hooks.sh --quiet
```

The installer is idempotent: re-running with no changes prints `no change`.

## Generated settings the installer writes

`.claude/settings.json` is the exact template. The installer preserves unrelated
entries, replaces the project hook path with the absolute checkout path, and
changes `RALLY_HOOK_SOURCE=project` to `RALLY_HOOK_SOURCE=global`. Matchers,
timeouts, cadence, and all other fields remain byte-for-byte derived from the
generated template. Claude Code does not reliably expand `~` in command strings.

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

When set, the hook may emit `permissionDecision: "deny"` (Claude PreToolUse) or
`decision: "block"` (Stop) on **high-severity** signals only (`severity==stop`
or `allow==false`). Codex PreToolUse remains fail-open with `systemMessage`
because Codex rejects the Claude `permissionDecision` field. Low-severity
warnings always remain advisory.

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
Until that lands as the agent's default reflex, host hooks close the gap for
**Claude Code and Codex today**: agents do not reliably self-invoke skill or CLI
patterns mid-task (memory: `feedback_subagent_skill_reactivity`), so the host
hook mechanism is the deterministic surface.

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
