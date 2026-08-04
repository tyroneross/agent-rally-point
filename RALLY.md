# Rally - the 60-second guide

Rally is the primary Agent Rally Point path. It gives coding agents a shared
repo-local room: what is owned, blocked, handed off, decided, produced, and what
to do next.

> **First time in a repo?** Run `rally init` once. It writes
> `.rally/manifest.json` (machine-readable self-description) and injects a
> fenced rally pointer into the repo's `CLAUDE.md` and `AGENTS.md` so any
> agent landing there sees how to enter and where the deeper docs live.
> Idempotent — re-running `rally init` refreshes the pointer block between
> stable markers without duplicating anything.
>
> Generic or non-first-class agents should start with
> [`docs/ANY-AGENT-ONBOARDING.md`](docs/ANY-AGENT-ONBOARDING.md). Rally room
> participation only requires the CLI; direct injection requires a managed
> session.

## The Load-Bearing Commands

```bash
rally whoami --tool <you> --json
rally enter --tool <you> [--tier frontier|executing|fast] --json   # --tier: first frontier agent auto-leads
rally ack --tool <you>
rally next --tool <you> --json
rally check before-write --tool <you> --path <path> --strict --json
rally say artifact --tool <you> --subject "<what changed>" --uri <path> --evidence "<verification>" --json
rally room --json
```

That is the core loop. `whoami` identifies the active host, room, lead, and
mission before any action, `enter` shows the room, `ack` confirms the startup
rules were ingested, `next` gives a concrete action contract, `check` protects
shared boundaries, `say` records durable facts, and `room` inspects the current
projection.

## Identify Yourself

Pick a stable `tool` id and use it across sessions. Peers address you by this
id.

For managed sessions, `rally run` appends readable ids from the active room.
The first default Claude session is `claude-01` with tool `claude_code:01`; a
named reviewer is `reviewer-01` with tool `claude_code:reviewer-01`.

| Host          | tool id       |
|---------------|---------------|
| Claude Code   | `claude_code` |
| Codex CLI     | `codex`       |
| Pi            | `pi`          |
| Cursor        | `cursor`      |
| Gemini CLI    | `gemini`      |
| OpenCode      | `opencode`    |
| Qwen CLI      | `qwen`        |
| Gemma CLI     | `gemma`       |
| Aider         | `aider`       |
| CI/automation | `ci`          |

## Resolve Targets From Live State

Treat every lead, reviewer, worker, and session id as runtime state. The current
target comes from the mission, `rally whoami`, `rally lead show`, `rally next`,
an explicit handoff target, or `rally room --json`. Do not reuse ids from
examples, old logs, another repo, or a previous engagement.

When a specific agent must act, write a targeted handoff first; that is the
durable action request:

```bash
rally say handoff --tool <you> \
  --target <target-tool> \
  --subject "<action needed>" \
  --json
```

Direct injection is only a wake/delivery path for managed sessions. Before
injecting, verify the target exists in `rally sessions --json`. If it is not a
managed session, do not guess a terminal pane. Keep the targeted handoff as the
source of truth and report that direct injection is unavailable unless the exact
running surface can be positively adopted.

## The Agent Loop

```text
self-locate (rally whoami)   # host runtime, room, lead, mission, ambiguity — run FIRST
  -> enter repo/session + ack
  -> run next
     -> if actionable, claim/check
        -> execute and verify
           -> say artifact/handoff/resolve/release
              -> run next again
```

Stop and ask when `next.requires_human` is true, when `next.actionable` is
false, when a boundary check blocks, or when the work hits a real blocker.

## Concrete Example

Claude has finished planning a refactor and wants Codex to implement:

```bash
# Claude:
rally say handoff --tool claude_code \
  --target codex \
  --subject "implement the auth refactor from docs/plans/auth-v2.md" \
  --summary "tests in tests/auth_test.rs should still pass" \
  --json

# Codex:
rally whoami --tool codex --json
rally enter --tool codex --json
rally ack --tool codex
rally next --tool codex --json
# ... claims/checks, does the work, verifies ...
rally say artifact --tool codex \
  --subject "auth refactor implemented" \
  --uri docs/plans/auth-v2.md \
  --evidence "cargo test" \
  --json
rally next --tool codex --json
```

## How Agents Wire This In

Managed sessions are the reliable direct-delivery path:

```bash
rally run claude --backend auto --json
rally inject <session|name|tool> --handoff <event-id> --json  # e.g. claude-01
rally capture <session|name|tool> --json
```

Run backends are `auto`, `tmux`, `cmux`, and `ptyd`. `auto` selects the
rally-owned ptyd path only when that daemon socket is live, otherwise tmux.

First-class `rally run` launch targets are currently Claude, Codex, OpenCode,
and Gemini. Other agents - Cursor, Qwen, Gemma, Aider, IDE plugins, CI workers -
can still participate by running the core `whoami` / `enter` / `ack` / `next`
loop with a stable tool id.

`rally inject` addresses managed sessions only. If `rally sessions --json` does
not list the target, use `rally say handoff --target <target-tool>` and either
adopt/relaunch the running surface or report that direct delivery is unavailable.
For the generic bootstrap contract, see
[`docs/ANY-AGENT-ONBOARDING.md`](docs/ANY-AGENT-ONBOARDING.md).

Agents can still call `rally check before-write` explicitly before shared
edits.

**This repo ships committed host hook registrations, and opening the repo in a
host that trusts it auto-loads them.** `.claude/settings.json`,
`.codex/hooks.json`, `.cursor/hooks.json`, and `hooks/hooks.json` wire
`SessionStart` presence and `PreToolUse` deconfliction with no setup step. That
is deliberate — instructing agents to run the commands produced inconsistent
compliance, and coordination that works most of the time is close to useless.
It is also a real trust decision, so read what the hooks do and how to disable
them before relying on it:
[`docs/security/TRUST-MODEL.md`](docs/security/TRUST-MODEL.md) and
[`docs/AUTO-COORDINATION-HOOKS.md`](docs/AUTO-COORDINATION-HOOKS.md).

The hooks are advisory and fail open — they never block an edit and they exit 0
when Rally is broken. They self-gate on missing `.rally/`, so they are a no-op
in unrelated repos. They do not download, build, or install anything; the
`rally` binary is installed by an explicit step you run
(`scripts/install-rally.sh` or `cargo install --path crates/rally-cli`).

Off: `RALLY_HOOKS=off` (session), `rally hooks off --scope repo` (repo),
`rally hooks status` (check).

Why hooks won over a hookless CLI, why agents self-manage instead of being
managed, and why delivery is push-preferred with a pull floor:
[`docs/DESIGN-TRADEOFFS.md`](docs/DESIGN-TRADEOFFS.md).

## Discovery & Session Management

Beyond the core loop, Rally ships discovery and session-lifecycle commands:

```bash
rally sessions --json                          # list managed sessions in the room
rally attach <session|name|tool> --json        # attach to an existing managed session
rally capture <session|name|tool> --json       # capture a managed session's current output
rally stop <session|name|tool> --json          # stop a managed session
rally locate <event-id> --json                                  # find which channel an event lives in
rally recent [--all] [--limit N] [--include-archived] --json    # recent activity across channels
```

`sessions`, `attach`, `capture`, and `stop` operate on managed sessions started
by `rally run`. `locate` and `recent` answer "where is this?" / "what just
happened?" across the rooms-based channels Rally knows about. The legacy
`~/.agent-rally-point/apps/` JSONL store and its `--include-legacy` flag have
been retired — facts written there no longer surface in `locate` or `recent`.
`--include-archived` on `recent` re-includes recency-decayed facts that would
otherwise drop out of the default listing.

## Useful Fact Writes

```bash
rally say claim --tool <you> --subject "edit parser" --path crates/rally-cli/src/main.rs --json
rally say release --tool <you> --ref <claim-id> --subject "done" --json
rally say blocker --tool <you> --subject "need decision" --severity high --json
rally say resolve --tool <you> --ref <blocker-id> --subject "resolved" --json
rally say decision --tool <you> --subject "Rally is primary" --status binding --json
rally say risk --tool <you> --subject "managed session unavailable" --severity medium --json
```

## Lead & Backlog

```bash
# Lead-agent title — rally records/exposes only, never enforces (see COORDINATION.md):
rally lead show --json                                          # current lead, tier, how-assigned
rally lead handoff --tool <lead> --to <frontier-tool> --json    # transfer the title
rally lead assign  --tool <you> --to <tool> [--user-designated] --json   # set lead (user-designated supersedes first-join)
rally lead relinquish --tool <lead> --json                      # drop the title (reopens the seat)
# Lead auto-assigns to the first FRONTIER agent to enter (rally enter --tier frontier).

# Claimable backlog + plan/status bus:
rally backlog add --tool <you> --id <id> --intent "<what>" [--target <owner>] [--status planned] [--expected-by "<when>"] [--owns <path>] [--depends-on <id>] --json
rally backlog update --tool <you> --id <id> [--status in_progress|blocked] [--expected-by "<next checkpoint>"] --json
rally backlog list --json                                       # OPEN items only
rally backlog done --tool <you> --id <id> --json                # close an item (drops out of list)
```

When a backlog item has `--target <tool>` and status `open`, `planned`, or
`blocked`, `rally next --tool <tool> --json` returns `update_plan_status` until
that tool posts a status update. This is the lightweight Rally bus for plan,
owner, ETA, and status; do not rely on chat paste as the only coordination
surface.

## Where State Lives

```text
.rally/log/<engagement>.jsonl   canonical, append-only per-engagement facts (R5; committed)
.rally/ledger.jsonl             legacy monolith — migrated into log/ on first open (R1)
.rally/archive/                 rotated old segments, still replayable (R7)
.rally/manifest.json            self-describing pointers (R4; committed)
.rally/facts.db                 derived sqlite cache — rebuilt by replay (gitignored; owned by rallyd when the daemon runs)
.rally/cursors.json             per-tool read cursors
.rally/rallyd.sock              rallyd's Unix socket when the daemon runs (gitignored)
.rally/rallyd.owner.lock        kernel file lock guarding the daemon/direct-writer handover (gitignored)
```

Linked git worktrees resolve this room from the shared git common dir, so the
main checkout and its worktrees coordinate through one `.rally/` store. The
**ledger files under `.rally/log/`** are the source of truth — append-only,
committed, durable across clone/machine. `facts.db` is a pure cache that
`rally` rebuilds from the ledger on first open. Managed session lifecycle
facts ride the same ledger.

## What `rally room` Gives You

`rally room --json` returns what is relevant to you now, not everything the room
has ever held. Relevance is computed from signals Rally already tracks, so the
answer changes with the situation and with who is asking.

**Four signals compose the ranking.** How recently a fact was written (the same
exponential decay that drives archiving). Whether its author is still beating
inside their own adaptive liveness window. Whether the fact is addressed to you.
And how much its scope overlaps the working set you declared with `--path`. Pass
`--tool` and `--path` and you get a different, better room than a bare call —
that is the point.

**A missing signal never costs an item its place.** Every factor is neutral when
it cannot be measured, so an item whose relevance is unknowable is ranked on age
alone rather than sunk. Only a positive measurement — an author provably past
their window — moves anything down.

**Some things are never cut.** Active claims, blockers, peers, system health, and
any handoff addressed to you ship whole regardless of size. Dropping one of those
risks the write collision this tool exists to prevent, so their size is
controlled by expiry, not by cutting. Everything else is ranked and fitted to a
byte budget, and every non-empty bucket always contributes at least its top item.

**Nothing is ever dropped silently.** `totals` carries true counts for every
bucket on every response, so "1,390 archived facts" and "no archived facts" can
never look the same from your side. If anything was omitted, a `composition`
block names the bucket, the counts, the omitted event ids, and the command that
returns the full view. If the never-cut buckets alone exceed the budget, the room
ships over budget and says so rather than cutting state you need.

### Getting the full view

```bash
rally room --include-archived --json   # everything, no budget applied
rally room --tool <you> --path src/store.rs --json   # ranked for your lane
rally room --budget-bytes 0 --json     # disable the ceiling for this call
rally locate <event-id> --json         # one item named in composition.omitted_ids
```

`--include-archived` is the drill-in, so the budget does not apply to it. An
escape hatch that is itself truncated is not an escape hatch.

### Tuning it

Every threshold is config, resolved default → user → repo → env, under
`coordination` in `.rally/config.json`:

| Key | Default | What it does |
|---|---|---|
| `room_budget_fraction` | `0.05` | Share of the consumer's context the room may occupy. `0` disables the ceiling. |
| `consumer_context_bytes` | `4000000` | Assumed consumer context. With the default fraction this is a 200 KB ceiling. |
| `half_life_hours` | `48` | Recency decay half-life. |
| `archive_floor_weight` | `0.05` | Below this weight a fact is archived out of the active buckets. |
| `relevance.stale_author_factor` | `0.5` | Multiplier for a provably-stale author. Clamped to `(0, 1]`. |
| `relevance.addressed_boost` | `1.0` | Boost when an item is addressed to you. |
| `relevance.path_overlap_boost` | `1.0` | Boost at full overlap with your declared paths. |
| `stale_wait_secs` | `86400` | When an unanswered handoff stops counting as an active obligation. |
| `rotate_threshold_days` | `30` | Age at which a log segment is eligible for `rally rotate`. |

Each has a `RALLY_`-prefixed env override (`RALLY_ROOM_BUDGET_FRACTION`,
`RALLY_HALF_LIFE_HOURS`, and so on) that beats repo config.

## Single-Writer Daemon (rallyd, optional)

`rallyd` is a per-repo daemon that owns `.rally/facts.db` so exactly one process
ever touches it. When it runs, every `rally` command routes its store reads and
writes over a Unix socket (`.rally/rallyd.sock`) instead of opening the SQLite
cache directly. This removes the multi-process contention that made many
concurrent CLIs flaky under load — the cache is opened once, by one writer.

It is **opt-in and fail-open.** With no daemon running, every command behaves
exactly as before (each process opens the cache directly). Start it when a repo
has many concurrent agents; skip it otherwise.

```bash
rally daemon start    # spawn the daemon (detached; returns once it is serving)
rally daemon status   # is it live? pid, socket, wire version
rally daemon stop     # graceful shutdown (SIGTERM); the cache stays intact
rally daemon serve    # run in the foreground (what `start` launches)
```

The daemon records, serves, and derives only — it never decides, schedules, or
runs work (charter: facilitator, never executor). The JSONL ledger under
`.rally/log/` stays canonical; `facts.db` stays a disposable cache the daemon
rebuilds from the ledger. A client never writes the cache directly while the
daemon holds it, and the daemon never starts while a direct writer is mid-write —
a kernel file lock (`.rally/rallyd.owner.lock`) enforces the handover, so there
is never more than one writer.

## Install

```bash
git clone https://github.com/tyroneross/agent-rally-point.git
cd agent-rally-point
cargo install --path crates/rally-cli
rally whoami --tool <you> --json
rally enter --tool <you> --json
rally ack --tool <you>
```

The committed hooks also need `node` on PATH to render output (they parse `rally`'s JSON in Node). Without it the hooks still run their Rally calls silently but produce no advisory text; a one-line stderr notice names the gap once per session and the hooks stay fail-open (exit 0).
