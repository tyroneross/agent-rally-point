# Rally 2.0 Architecture

Rally 2.0 is a repo-local coordination layer for parallel goal-driven agents.
It is not a conductor, task runner, dashboard, or coding agent. Its job is to
keep shared room state correct, fresh, and visible before agents act.

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
shared situational awareness across tabs, tools, sessions, remotes, and time.

Rally should own:

- Durable coordination facts.
- Current room state projected from those facts.
- Agent entry state.
- Boundary checks before shared work collides.
- Adapter installation that makes agents remember to consult Rally.

Rally should not own:

- Agent reasoning.
- Goal execution.
- Work scheduling.
- Workflow proof or run validity.
- Visual graph UI.
- A conductor role.

## Architecture

```text
.rally2/facts.jsonl  canonical append-only typed fact log
.rally2/room.db      live SQLite room projection/index
enter/room/check     product APIs backed by the projection
HANDOFF.md           optional plain-text snapshot export
adapters             Codex, Claude, Pi, Herdr, cmux, CI integration
```

`facts.jsonl` is the source of truth. `room.db` is a derived cache and never
writes back to the log. The live surface is a SQLite room projection with
relationship indexes, not a graph product.
`HANDOFF.md` is an optional export for humans, agents without Rally access, and
end-of-session snapshots.

## Command Surface

The greenfield surface should be small:

```bash
rally2 enter --tool codex    # agent entry state + changed attention
rally2 say <kind> ...        # append a typed coordination fact
rally2 room --json           # inspect current projected room state
rally2 check before-write    # boundary check before shared work changes
```

Everything else is debug, admin, or adapter plumbing. `HANDOFF.md` export and
adapter setup matter, but they should not be part of the core product loop.

## Agent Product Loop

An agent should not need to remember Rally from documentation. The integration
must put Rally into the agent's normal operating loop.

Required loop:

```text
enter repo/session -> receive room state
before shared change -> run boundary check
after meaningful work -> say the durable fact
when another agent acts -> it enters with those facts visible
```

The product succeeds when agents know Rally through repeated interaction:

- At startup, the adapter injects `rally2 enter` into visible model state.
- During work, write boundaries call `rally2 check` before changes land.
- At completion, the adapter prompts for `rally2 say artifact`, `handoff`,
  `decision`, `risk`, or `blocker` when appropriate.
- On resume or loop boundary, the adapter calls `rally2 enter` again so the
  agent sees what changed while it was focused elsewhere.
- When the agent asks for the broader picture, `rally2 room` is the source of
  truth.

Manual CLI use should be possible, but the primary product path is adapter-led.
If agents must remember to run Rally by habit, the product is not finished.

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
- `origin`
- `trust_status`

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
- Which remote/imported facts are trusted enough to automate against?

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
- trusted/imported fact summary

The room must be queryable by tool, role, path, event, thread, and since cursor.
Agents should normally consume `enter` and `room`, not a rendered markdown file.

## HANDOFF.md Export

`HANDOFF.md` is a portable snapshot. It should be generated on demand from the
room state and safe to overwrite.

Recommended sections:

```md
# Rally Handoff

## Do Not Touch
## Active Work
## Open Handoffs
## Blockers
## Decisions
## Risks
## Recent Artifacts
## Evidence
## Next Attention Points
```

The file should be concise enough to paste into an agent prompt. It is useful
for humans, tools without adapter support, end-of-session summaries, and remote
handoff bundles.

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
- `attention`
- `ignore`

`attention` is the since-last-enter refresh bucket. It is for newly relevant
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
- `after-artifact`: encourage evidence and handoff routing.
- `before-complete`: ensure active claims are released or explained.

Strict blocking should only apply to local or trusted facts unless configured
otherwise.

## Adapters

Setup is the infrastructure that teaches each agent surface how to use Rally.
It is not a separate product surface.

Adapters make each product call Rally at the right moments:

- startup/resume: inject `enter`
- idle/loop boundary: call `enter` again
- before write: call `check before-write`

Completion should not run on every finished model turn by default. It is too
noisy. Agents can still run `rally2 check before-complete` explicitly, and a
surface may add a completion prompt later only when there is an actionable
condition such as an active owned claim or blocker.

The setup command is intentionally narrow:

```bash
rally2 install codex --dry-run --json
rally2 install all --json
rally2 install codex --uninstall --json
```

It writes only Rally 2-owned hook scripts, extensions, snippets, and hook config
entries. It does not silently delete older Rally wiring from other products;
this project has one user right now, so legacy cleanup is a manual local
operation instead of product behavior. The installer may report legacy hooks so
the operator knows what to remove.

Required first-class adapters:

- Codex
- Claude Code
- Pi
- Herdr
- cmux
- CI

Adapter status should report surfaces, not abstract health:

```json
{
  "adapter": "codex",
  "installed": true,
  "surfaces": {
    "startup_enter": true,
    "loop_enter": true,
    "before_write_check": true,
    "completion_prompt": false
  }
}
```

Each adapter must define:

- How Rally room state becomes model-visible.
- Which boundary events can call `check`.
- How an agent is prompted to publish facts.
- Where tool identity, role, watched paths, and cursors are stored.
- Which failures are blocking, warning-only, or invisible to the agent.

The adapter layer is the main reason Rally can be useful. Without it, Rally is
just another command an agent may forget.

Adapter expectations by surface:

- Codex: inject `enter` at startup/resume, refresh at goal loop boundaries when
  possible, call `check before-write`, and prompt for `say` on completion.
- Claude Code: inject `enter` through project/user instructions or hooks, call
  `check before-write`, and prompt for durable facts when tasks complete.
- Pi: inject `enter` into the active Pi message/context surface, refresh at
  session boundaries, and support completion prompts for `say`.
- Herdr: expose room context to panes, track pane/tool identity, and make it
  easy for an operator agent to read panes, route work, and inject `enter`.
- cmux: expose Rally entry state to sessions without pretending cmux
  owns the coordination state.
- CI: read room/check state, fail or warn on unresolved trusted blockers and
  unsafe claims according to policy, and publish evidence facts when useful.

## Remote Model

Remote support is event exchange, not live shared state.

```text
local facts -> export -> import elsewhere -> project room locally
```

Imported facts retain `origin` and `trust_status`. Checks only automate against
facts whose trust status satisfies local policy.

## Product Constraints

Build Rally 2.0 as a clean product. The implementation may use proven local
techniques, but the user-facing model should stand on its own.

Required product qualities:

- Few nouns: room, fact, enter, say, check.
- Commands that are obvious without reading old project history.
- SQLite room projection with relationship indexes as the default live surface.
- `HANDOFF.md` as an explicit export, not constantly updated state.
- JSON contracts designed for agents first.
- Internal projection/indexing hidden behind product commands.
- Adapter setup tied to concrete surfaces, not broad health checks.
- Tests written around user journeys and stable command contracts.

Forbidden product drift:

- conductor behavior
- workflow engine behavior
- dashboard-first design
- visual graph product
- proof/run-validity framing
- task recommendation as a substitute for agent reasoning
- broad setup/doctor surfaces not tied to adapter installation

## Build Plan

Phase 1: Product contract.

- Freeze this document as the Rally 2.0 product boundary.
- Define the command contracts and event schemas.
- Define JSON output for `enter`, `room`, and `check`.
- Define optional `HANDOFF.md` export sections and ordering.

Phase 2: Product build.

- Implement `enter`, `say`, `room`, and `check`.
- Use append-only events as canonical state.
- Use an internal SQLite projection for queries.
- Add golden JSON tests for each product command.

Phase 3: Adapter loop.

- Install Codex, Claude Code, and Pi startup/refresh adapters.
- Add Herdr and cmux workspace/session demos.
- Add CI read-only room checks and optional handoff export checks.

Phase 4: Dogfood.

- Run Claude + Codex in Herdr against the same repo.
- Verify a fresh agent becomes useful in under 10 seconds.
- Verify the user no longer manually summarizes between agents.
- Cut anything that does not support that journey.

## Acceptance Test

Rally 2.0 is working when:

```text
A fresh agent enters a busy repo, receives room state, avoids claimed work,
uses current decisions, sees relevant handoffs, publishes an artifact with
evidence, and updates shared room state without the user acting as the memory bus.
```
