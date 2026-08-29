# Rally Architecture

Rally is a repo-local coordination layer for parallel goal-driven agents.
It is not a conductor, task runner, dashboard, or coding agent. Its job is to
keep shared room state correct, fresh, and visible before agents act.

Rally is the Agent Rally Point product path. The repository ships one
coordination CLI: `rally`.

Product sentence:

```text
Rally makes a repo feel like a room that every agent enters before acting.
```

The user should stop being the clipboard between agents. A fresh Claude, Codex,
Pi, CI job, or human shell should be able to enter a busy repo and understand
what changed, what is owned, what is blocked, what decisions bind the work, and
what it must not collide with.

## Core Thesis

Agents can already write clean code and run long goals. The missing layer is
shared situational awareness across tabs, tools, sessions, and time.

Rally should own:

- Durable coordination facts.
- Current room state projected from those facts.
- Agent entry state.
- Boundary checks before shared work collides.
- Managed-session delivery into addressable tmux, cmux, and ptyd panes.

Rally should not own:

- Agent reasoning.
- Goal execution.
- Work scheduling.
- Workflow proof or run validity.
- Visual graph UI.
- A conductor role.

## Architecture

```text
.rally/log/<engagement>.jsonl  canonical append-only event segments (committed; one file per engagement)
.rally/ledger.jsonl            R1 legacy single-log — still replayed on read for pre-segmentation repos
.rally/manifest.json           self-describing pointers to docs + log (committed)
.rally/RETROSPECTIVE.md        human-readable digest grouped by engagement (committed)
.rally/archive/**              rotated old segments, still replayable (committed)
.rally/facts.db                derived sqlite cache (gitignored; rebuildable)
.rally/cursors.json            per-tool read cursors (gitignored; a write-through cache — the ledger is authoritative)
enter/next/room/check          product APIs derived from the segments
managed sessions               tmux, cmux, and ptyd launch/inject/capture/stop
```

The per-repo **`.rally/log/` segments are the source of truth** (R5 segmentation).
The R1 single `.rally/ledger.jsonl` remains a valid canonical log and is still
replayed on read for repos that predate segmentation. Each segment is
append-only, one event per line (`{seq, occurred_at, event_type, payload}`),
committed, and carries `merge=union` (see `.gitattributes`) so concurrent
appends from sibling worktrees merge without conflict.

**Commit cadence.** The committed segments may lag the on-disk segments during
active work — that is fine: the segments are a live working-tree artifact, and
`merge=union` makes a lagging commit safe to catch up at any time. Commit them
on whatever cadence the engagement uses (per chunk, per closeout, or batched);
there is no daemon and no required cadence.

`facts.db` is a *derived* sqlite cache (factstr-sqlite) that `RoomStore` rebuilds
by replaying the segments whenever the cache is missing or behind. `cursors.json`
is a write-through read-cursor cache, **never** the source of truth — a tool's
read position is derived from ledger `Read` checkpoints (R10), with `cursors.json`
consulted only as a fast-path fallback. A clone or fresh machine reconstructs room
state from the committed segments alone — no external service, no migration step.

Room snapshots and managed session lists are derived from the event log on
demand, not stored in a second projection DB.

## Per-Repo Segmentation

**One repo = one rally point.** Rally's coordination data lives at
`<repo_root>/.rally/`, keyed by `repo_root` in `discovery.rs::repo_root()`.
Data never co-mingles across repos: claims, blockers, decisions, artifacts,
and managed sessions in repo A are invisible to a `rally` invocation in
repo B.

The home-dir directory `~/.agent-rally-point/rooms/v1/index.json` is an
**opt-in, pointers-only** discovery hint — it lists `(repo_root,
workspace_root, facts_db_path, last_seen_seq)` so `rally locate --all`,
`rally recent --all`, and `rally status --global` can answer "what other rooms
exist in this workspace?" without a network call. It holds **zero canonical
fact data**; the per-repo `.rally/log/` segments do. Deleting the global index
loses cross-repo visibility but not a single fact.

```text
~/.agent-rally-point/rooms/v1/index.json   workspace discovery hint (pointers; opt-in via RALLY_GLOBAL_INDEX=1)
~/dev/repo-a/.rally/log/<engagement>.jsonl repo A — canonical fact segments
~/dev/repo-a/.rally/facts.db               repo A — derived cache
~/dev/repo-b/.rally/log/<engagement>.jsonl repo B — canonical fact segments (isolated)
~/dev/repo-b/.rally/facts.db               repo B — derived cache (isolated)
```

### The global index is opt-in (`RALLY_GLOBAL_INDEX=1`)

**As of B17 the global index is OFF by default** (one-store north-star — the
home-dir store was a cross-repo contamination and trust-drift surface). By
default rally never touches the user's home directory. To enable cross-repo
discovery, set `RALLY_GLOBAL_INDEX=1` (any non-empty value) on the `rally`
process. `RALLY_NO_GLOBAL_INDEX=1` still **force-disables unconditionally** even
when opted in (back-compat, and for privacy-isolated / multi-tenant / sandboxed /
CI scenarios). With the index disabled (the default):

- No write to `~/.agent-rally-point/rooms/v1/index.json`.
- No read of the same file.
- `rally locate --all` / `rally recent --all` collapse to "this repo only".

Per-repo coordination is **unaffected** — `.rally/log/` segments and
`.rally/facts.db` work exactly as before. Only the cross-repo "what other
rooms exist?" surface is gated.

`rally status --global` is workspace-scoped even when the pointer index contains
rooms from elsewhere on the machine. The workspace root is `RALLY_WORKSPACE_ROOT`
when set; otherwise it is the current repo root's parent directory. Rooms outside
that boundary are hidden from the status rollup. This lets a Terminal Rally
Point viewer show all Rally activity inside one local workspace without exposing
unrelated workspaces.

```bash
# Default: no env var needed — the global index is already off (B17).
rally recent --all --json                          # silent on other repos (this repo only)

# Opt in to cross-repo discovery:
RALLY_GLOBAL_INDEX=1 rally recent --all --json      # lists other rooms on this machine
RALLY_GLOBAL_INDEX=1 rally locate --all --json
RALLY_GLOBAL_INDEX=1 rally status --global --json   # lists rooms in this workspace

# Force-off even when opted in (privacy-isolated / multi-tenant / CI):
RALLY_NO_GLOBAL_INDEX=1 rally enter --tool codex --json
```

There is no CLI flag — env-var-only is the minimum surface that does the
job, and it composes cleanly with `direnv`, container env files, and CI
secret panels. A future release may add `--global-index` if the env-var
form proves insufficient.

## Command Surface

The greenfield surface should be small:

```bash
rally whoami --tool codex   # self-locate host, room, lead, mission, ack state
rally enter --tool codex    # agent entry state + changed attention
rally ack --tool codex      # confirm startup rules/guardrails/lead/mission
rally next --tool codex     # ranked next action when idle or waiting
rally say <kind> ...        # append a typed coordination fact
rally room --json           # inspect current projected room state
rally check before-write    # boundary check before shared work changes
```

Everything else is debug or admin plumbing.

## Agent Product Loop

An agent should not need to remember Rally from documentation. The managed
session path must put Rally into the agent's normal operating loop.

Required loop:

```text
rally run -> managed mux session starts the agent
self-locate -> identify host, room, lead, mission, and ack state
enter repo/session -> receive room state
ack -> confirm startup rules/guardrails/lead/mission
explicit idle/wait command -> receive next useful action
before shared change -> run boundary check
after meaningful work -> say the durable fact
when another agent acts -> rally inject routes the obligation to a managed session
```

The product succeeds when agents know Rally through repeated interaction:

- At startup, `rally run` starts the agent in a managed mux session.
- During work, write boundaries call `rally check` before changes land.
- On prompt, idle, resume, or loop boundaries, Rally stays silent. It must
  not inject full room or `next` state into ordinary prompts just to keep Rally
  in context.
- Explicit wake or handoff delivery is different: a managed backend may inject a
  focused Rally obligation when a peer addresses a live session.
- If an explicit `rally next` call shows the agent is waiting on a peer, the
  agent can still use alternate work such as reviewing unconsumed artifacts
  before settling for a wait state.
- When the agent asks for the broader picture, `rally room` is the source of
  truth.

Manual CLI use should be possible, but the primary product path is managed
session delivery. If agents must remember to run Rally by habit, the product is
not finished.

## Act-On-Next Contract

`rally next` is an execution contract for an agent build loop, not a daemon.
Rally recommends and constrains work; the agent or harness still executes,
verifies, and decides when to continue.

The `next` payload must make the loop explicit:

- `actionable`: whether the agent may treat the recommendation as a task
  candidate.
- `requires_human`: whether the agent should stop and ask before acting.
- `stop_reason`: why there is no autonomous action, such as waiting on a peer.
- `suggested_claims`: scoped claim commands for work the agent should reserve
  before editing or reviewing files.
- `suggested_commands`: command templates for checks and completion facts.
- `completion`: the durable fact kind expected after work, whether evidence is
  required, whether claims should be released, and whether the agent should run
  `rally next` again.

The intended autonomous build loop is:

```text
whoami -> enter -> ack -> next -> if actionable, claim/check -> execute
       -> verify -> say artifact/handoff/resolve/release -> next
```

The loop stops when `actionable` is false, `requires_human` is true, the agent
hits a blocker, or the harness/user budget expires. This keeps autonomy in the
agent harness while Rally remains the room and coordination substrate.

## Standby And Wake Contract

Rally does not keep model agents awake. Standby behavior belongs to the agent
host, managed-session backend, connector, or external runner. Rally's job is to
make wake intent cheap to detect and unambiguous to deliver.

The contract:

- Rally persists facts and projects room state.
- `rally next --tool <tool> --json` is the canonical "should this agent act?"
  check when no direct handoff id is already known.
- `rally inject <session|name|tool> --handoff <event-id> --json` is the
  canonical delivery path for a focused obligation into a managed session.
- Lightweight watchers may poll `next`, query `room`, or watch fact-store
  changes, but they must only emit transition signals. They must not edit code,
  resolve blockers, publish facts on behalf of an agent, or act as hidden
  schedulers.
- Connectors decide how to deliver the signal: native wake, managed-session
  injection, pane notification, resume-only context, or automation
  failure/warning.
- Durable always-on monitoring belongs in host-native packaging such as launchd,
  systemd, CI, cmux, or ptyd. Ad hoc background shells are for short active
  sessions only.

This keeps Rally optimized for coordination instead of process supervision. If
a connector can wake an idle agent safely, it should do so through the
connector's native primitive. If it cannot, it should make the latest Rally
state visible at the next prompt, resume, or loop boundary.

## Typed Facts

Rally facts are speech acts that affect future work. They should remain small
and explicit.

Required fact kinds:

- `claim`: a tool owns a path/resource for a bounded period.
- `release`: a claim is no longer active.
- `blocker`: work cannot proceed without a named decision/input/artifact.
- `resolve`: a blocker is resolved.
- `decision`: future work must conform within a scope.
- `artifact`: work produced something reviewable, with evidence.
- `handoff`: another participant should act with supplied context.
- `risk`: a known uncertainty or likely failure mode.
- `lesson`: reusable memory candidate, not binding truth.

Each fact should carry:

- `event_id`
- `thread_id`
- `created_at`
- `tool`
- `role`
- `kind`
- `subject`
- `scope`
- `summary`
- `evidence`
- `target` when another tool/human should act
- `ref` when the fact relates to another event, artifact, issue, or external id
- `status` for lifecycle-bearing facts such as claims, blockers, and handoffs
- `severity` for risks, blockers, warnings, and check findings
- `uri` for produced artifacts or external evidence

## Room Projection

Use SQLite projection tables and relationship indexes for queryability, not as a
product metaphor.

The room projection answers:

- Which claims affect this path?
- Which decisions bind this path?
- What changed since this tool last checked?
- What artifacts are unreviewed or unconsumed?
- Which blockers are active?
- What is related to this event/thread/task?

The product should expose `enter`, `room`, and `check`, not require users or
agents to think in storage or query-planning terms. A richer graph should not be
introduced unless the product needs queries that the current projection cannot
answer cleanly.

## Room

`room` is the primary product surface. It exposes projected coordination state
from the SQLite room projection in stable JSON.

Required room sections:

- active claims
- active blockers
- open handoffs
- current decisions
- current risks
- recent artifacts
- unconsumed artifacts
- stale facts

The room must be queryable by tool, role, path, event, thread, and since cursor.
Agents should normally consume `enter` and `room`, not a rendered markdown file.

## Portable State

Portable state lives in Rally facts and typed command output. Agents should use
`enter`, `next`, and `room --json` instead of a generated markdown snapshot.

The file should be concise enough to paste into an agent prompt. It is useful
for humans and end-of-session summaries.

## Enter

`enter` is the main agent interaction. It combines role-shaped room state and
since-last-check attention refresh.

It answers:

```text
What should this tool know before acting?
What changed since this tool last entered?
```

It should be shaped by tool identity, role/profile, watched paths, current
claims, subscriptions, focus event, and since cursor.

Output should group facts as:

- `do`
- `do_not`
- `know`
- `verify`
- `respond_to`
- `ignore`

`attention` is a separate sibling, emitted once with total/emitted/omitted
counts; every enter list is capped at 128 rows. It is the since-last-enter
refresh bucket for newly relevant
facts and conflicts that deserve immediate model awareness, not a replacement
for the durable `do`, `do_not`, `know`, `verify`, and `respond_to` buckets.

Good attention candidates:

- handoff assigned to this tool
- claim conflict on a watched path
- decision affecting watched files
- blocker this tool can resolve
- artifact from another tool relevant to this role
- stale claim owned by this tool
- new risk or evidence contradicting this tool's active work

Bad attention candidates:

- self-authored artifacts with no new peer activity
- generic recent changes
- broad recommendations like "do this task next"

## Check

`check` enforces boundaries, not workflow.

Required checks:

- `before-write`: warn/block if a path is claimed or constrained.
- `before-complete`: ensure active claims are released or explained.

Strict blocking should only apply to local or trusted facts unless configured
otherwise.

## Managed Sessions

Reliable live delivery is mux-backed, not daemon-backed and not raw terminal
automation. Rally starts and records a normal visible agent session, then
injects Rally obligations into that session through a small hard-coded backend.
The agent confirms delivery by writing Rally facts; screen scraping is only
debug evidence.

First-class commands:

```bash
rally run claude --name reviewer --backend tmux --json
rally sessions --json
rally inject reviewer --handoff <event-id> --require-ack --json
rally capture reviewer --lines 120 --json
rally stop reviewer --json
```

Backend contract:

```text
start agent session
record session id, name, tool id, cwd, backend target
clear pending input
paste prompt
submit
optionally wait for a resolve/ack fact
```

Hard-coded backends, in order:

- tmux: first implementation and baseline for macOS/Linux/WSL.
- cmux: visible surface/workspace backend with native `new-workspace`, `send`,
  `read-screen`, `select-workspace`, and `close-workspace` commands.
- ptyd: visible pane/workspace backend with `pane send-keys`, `pane send-text`,
  `agent send`, and pane lifecycle commands. (Replaces the legacy Herdr backend
  removed in Plan F; Easy Terminal's app daemon socket is `ptyd.sock`.)

No dynamic backend/plugin system is needed until this contract stabilizes and a
third-party runtime needs to implement it. Unmanaged existing panes remain
best-effort; managed sessions are the reliable path.

## Managed Sessions

Managed sessions are the delivery infrastructure. Rally starts or addresses a
visible agent pane/workspace, injects a focused handoff prompt, captures output,
and records completion through Rally facts.

Required managed backends:

- tmux
- cmux
- ptyd
- CI

Backend status should report addressability, not abstract health:

```json
{
  "backend": "tmux",
  "available": true,
  "surfaces": {
    "run": true,
    "inject": true,
    "capture": true,
    "stop": true,
    "wake_signal": "inject"
  },
  "standby": {
    "wake_intent": "rally next --tool <tool> --json",
    "delivery": "managed_session_injection",
    "watcher_role": "transition_signal_only",
    "rally_owns_daemon": false
  }
}
```

Each backend must define:

- How managed sessions deliver actionable work.
- How standby signals are handled: native wake, injected message, pane/status
  notification, resume-only visibility, or automation policy.
- How an agent prompt is cleared before injection.
- How Rally captures recent output.
- Where tool identity, role, watched paths, and cursors are stored.
- Which failures are blocking, warning-only, or invisible to the operator.

Backend expectations by surface:

- ptyd: implement the managed-session backend, track pane/tool identity, and
  make it easy for an operator agent to read panes and route work. ptyd may
  wake a standby pane with focused `agent send` or pane injection when the host
  exposes that primitive. (Replaces the legacy Herdr backend removed in Plan F.)
- cmux: implement the managed-session backend without pretending cmux owns the
  coordination state. cmux may notify or inject into sessions, but the room
  state remains Rally-owned.
- tmux: provide the default local backend with no setup step.
- CI: read room/check state, fail or warn on unresolved blockers and unsafe
  claims according to policy, and publish evidence facts when useful.

The machine-readable backend contract is documented in
`docs/schemas/agent-rally.session-backend.v1.json`.

## Remote Model

Remote support is operational, not a Rally-level import/export protocol. SSH to
the machine that owns the repo, then run Rally there:

```text
ssh machine
cd repo
rally run claude --backend tmux
```

The room remains repo-local. If Rally later grows federation or A2A remote-room
support, provenance/trust should return as an explicit module rather than
speculative fields on every fact.

## Product Constraints

Build Rally as a clean product. The implementation may use proven local
techniques, but the user-facing model should stand on its own.

Required product qualities:

- Few nouns: room, fact, enter, next, say, check.
- Commands that are obvious without reading old project history.
- Factstr-backed SQLite event storage as the default live surface.
- JSON contracts designed for agents first, backed by typed Rust structs and
  schemars generation checks.
- Internal projection/indexing hidden behind product commands.
- Managed-session backends tied to concrete surfaces, not broad health checks.
- Tests written around user journeys and stable command contracts.

Forbidden product drift:

- conductor behavior
- workflow engine behavior
- dashboard-first design
- visual graph product
- proof/run-validity framing
- task recommendation as a substitute for agent reasoning
- broad setup surfaces that write host config

## Build Plan

Phase 1: Product contract.

- Freeze this document as the Rally product boundary.
- Define typed command contracts and event schemas.
- Define JSON output for `enter`, `room`, and `check`.

Phase 2: Product build.

- Implement `enter`, `say`, `room`, and `check`.
- Use `.rally/log/<engagement>.jsonl` as canonical state.
- Use `facts.db` as the internal SQLite projection for queries.
- Add journey tests for stable command contracts.

Phase 3: Managed-session loop. (historical — Herdr was removed in Plan F; ptyd is the current visible-pane backend.)

- Implement tmux-backed `run`, `sessions`, and `inject`.
- Add Herdr and cmux managed-session backends.
- Add CI read-only room checks and optional handoff export checks.

Phase 4: Dogfood. (historical — see Phase 3 note.)

- Run Claude + Codex in Herdr against the same repo.
- Verify a fresh agent becomes useful in under 10 seconds.
- Verify the user no longer manually summarizes between agents.
- Cut anything that does not support that journey.

## Acceptance Test

Rally is working when:

```text
A fresh agent enters a busy repo, receives room state, avoids claimed work,
uses current decisions, sees relevant handoffs, publishes an artifact with
evidence, and updates shared room state without the user acting as the memory bus.
```
