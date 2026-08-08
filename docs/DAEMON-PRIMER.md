<!-- markdownlint-disable MD013 -->
<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Daemon Primer: Rally Point, Agent Harness, AI Assistant, and Easy Terminal

> **Status:** source-grounded primer for the current local implementations.
> **Inspected on:** 2026-08-08.
> **Pinned revisions:** Agent Rally Point `e332f4f`; RossLabs Agent Harness `16689b2`;
> RossLabs AI Assistant `a5f55aa`; Easy Terminal `99e4c78`.
> Each repository was inspected with its existing unrelated working-tree changes preserved.

## Bottom line

A daemon is a program that stays alive to handle later work. It is not automatically intelligent,
autonomous, or authorized to do anything it wants.

Every daemon must be started once by a person, an application, or an operating-system service such
as `launchd`. After it starts, it can act without another manual command **only when its code has a
trigger and an allowed action for that trigger**. Common triggers are a socket request, a file
change, a timer, a process exit, or a new ledger record.

The current systems use three distinct forms of automation:

1. **Reactive infrastructure** waits for a defined event and performs a deterministic action.
2. **Supervisors** start, monitor, stop, or restart other processes.
3. **Agent loops** receive a goal once, then let a model choose steps until completion or a limit.

Most current background components are reactive infrastructure or supervisors. They do not invent
new goals. `rally watch --on-activity` can launch an agent after Rally activity, but the watcher only
detects and launches; the launched agent supplies the reasoning.

## Core terms

| Term | Plain meaning | Does it stay alive? | Can it act without another human command? |
| --- | --- | ---: | --- |
| Foreground process | Runs in the terminal that started it; the prompt waits | While the command is running | Yes, if its code reacts to events or timers |
| Background process | Runs while the starting terminal or app continues doing something else | Usually | Yes, under the same programmed limits |
| Daemon or service | Long-running process intended to receive later work | Yes | Yes, after a configured trigger |
| Hook | Short command invoked by a host at a lifecycle event | No; normally exits in seconds | It acts only when the host invokes the event |
| Scheduled job | One-shot command launched by `launchd`, cron, or systemd on a schedule or file event | Usually no | The scheduler invokes it automatically |
| Worker | Process or thread that performs a bounded unit of work | Maybe | Only from its queue, event, or parent request |
| Supervisor | Process that owns or monitors child processes | Yes | It may restart, stop, or signal children by policy |
| Agent loop | Goal-directed process that lets a model choose multiple steps | Until the run ends | Yes, but only inside the initial goal and tool permissions |

“Foreground” and “background” describe **where a process runs and who waits for it**. They do not
describe its authority. `ptyd server` in a terminal is still a daemon even though it is running in
the foreground. `et server &` places the same server in the shell's background.

## What “running in the background” means

When a process runs in the background:

- The operating system continues scheduling it even though no terminal is waiting for its result.
- It waits most of the time, consuming little CPU until a socket, file, timer, or process event fires.
- It retains the user identity, environment, working directory, file permissions, and credentials it
  received at launch. Background execution grants no additional access.
- Its output normally goes to a log file, pipe, or application console instead of the active terminal.
- It survives closing a window only if the parent app keeps running, the process detached safely, or a
  service manager owns it.
- It survives logout or reboot only when an operating-system service is configured to start it again.
- A crash is restarted only when a parent supervisor, `launchd`, or systemd has an explicit restart
  policy. “Background” alone does not provide recovery.

The useful operational contract is therefore:

```text
starter -> process starts -> readiness is proven -> process waits
                                            |
                     socket / file / timer / child exit / ledger event
                                            |
                              validate trigger and authority
                                            |
                                  perform bounded action
                                            |
                              record result, receipt, or log
                                            |
                                  return to waiting state
```

## Action and autonomy ladder

| Level | What causes action | Example | Independent judgment? |
| --- | --- | --- | ---: |
| 0. Passive server | An explicit request | `rallyd` handles one store request | No |
| 1. Event-reactive worker | A known event | `rally-termd` sees a new directive | No |
| 2. Timer-driven maintainer | A clock or inactivity threshold | `ptyd` sweeps expired delivery state | No |
| 3. Supervisor | Child health or a client launch request | Easy Terminal restarts a dead `ptyd` | No |
| 4. Policy dispatcher | State plus deterministic route policy | Proposed `rally-routerd` chooses an adapter | No model judgment required |
| 5. Goal-directed agent | A goal plus observations and tools | `harness run` or an agent launched by `rally watch` | Yes, within its goal and permissions |

A Level 0–4 process can take actions “on its own” after startup, but it follows predetermined logic.
A Level 5 agent can choose the next step. That distinction matters more than whether the process is
called a daemon.

## Cross-product map

| Product | Component | Actual type | Starts it | Later trigger | Actions | Key limit |
| --- | --- | --- | --- | --- | --- | --- |
| Rally Point | `rallyd` | Per-repository store daemon | `rally daemon start`, `rally daemon serve`, or `rallyd` | Unix-socket store request | Serializes canonical Rally reads and writes | Cannot route messages, spawn agents, or use providers |
| Rally Point | `rally watch` | Long-running watcher command | Operator, launchd, or systemd | Per-repo ledger sequence changes | Emits activity and optionally runs one configured shell command | Detects activity; it does not understand the work |
| Rally Point | `cockpitd` | App server and agent supervisor | Operator or LaunchAgent | Authenticated WebSocket command and approval timer | Launches sessions, streams events, expires approvals | Separate SQLite authority; not the Rally message router |
| Rally Point / Easy Terminal | `ptyd` | PTY and child-process daemon | Easy Terminal, `et server`, `ptyd server`, or Rally's managed-session path | Socket request; internal maintenance timer | Starts/stops panes and agents, reads output, writes input, maintains runtime state | Does not read Rally's canonical ledger or decide coordination routes |
| Rally Point / Easy Terminal | `rally-termd` / `terminal-rally-point` | Ledger-to-PTY reactive daemon | Operator or external service manager | New `FileInbox` directive | Authorizes and executes PTY delivery, then writes receipt/high-water | Requires explicit ledger/state/agent configuration; no provider routing |
| Rally Point | Python `agent-rally-watcher` | Legacy companion daemon | Its CLI or launchd | Legacy `changes.jsonl` file event | Filters and copies records to sinks | Reference-only: watches the retired global store, not current canonical `.rally/log` |
| Agent Harness | `harness run` | One-shot agent loop | User, script, or another service | Model output inside the active run | Chooses tools and iterates toward the supplied goal | Ends at completion, error, loop guard, or turn cap; not a daemon |
| Agent Harness | `harness serve` | Local foreground HTTP server | `harness serve` | HTTP request | Shows models/sessions, toggles model state, spawns `harness run` | No built-in daemonize, restart, or service installation |
| Agent Harness | Managed `llama-server` | Transient inference child or reused external server | A routed harness run | Model request | Serves constrained local inference | Harness kills only the instance it spawned when the run guard drops |
| Agent Harness | Ollama | External model daemon | Ollama app/service | HTTP model request | Loads models and serves inference | Harness uses it; Harness does not own the daemon lifecycle |
| Agent Harness | Vault ingest job | `launchd` file-triggered one-shot worker | `launchd` after explicit installation/loading | Drop-zone path change | Runs serialized `harness vault ingest` for new files | The script exits after each scan; `launchd`, not the script, keeps watch |
| Agent Harness | Vault backfill job | `launchd` scheduled one-shot worker | `launchd` after explicit installation/loading | Daily 03:30 schedule | Applies deterministic managed wikilink blocks | Not resident; exits after the pass |
| AI Assistant | Routing and safety hooks | Host-invoked one-shot hooks | Claude/Codex hook lifecycle | Session, prompt, tool, or stop event | Classifies, logs, injects guidance, or blocks a verified conflict | No host event means no process and no action |
| AI Assistant | Self-improvement job | Daily `launchd` one-shot analysis | `launchd` after explicit install/load | Daily 09:00 schedule | Writes private, proposal-only routing reports | Never edits routes, policy, prompts, adapters, or memory |
| Easy Terminal | SwiftUI app monitors | In-process tasks, not daemons | Easy Terminal app launch | One-second poll, stream event, or suspected daemon death | Updates UI state and asks `DaemonController` to recover `ptyd` | Stop when the app process exits |

## Agent Rally Point

### `rallyd`: canonical-state serializer

- **What:** A thin per-repository server around `RoomStore`.
- **Where:** `crates/rallyd/src/main.rs` and `crates/rally-cli/src/rallyd_core.rs`.
- **Who starts it:** `rally daemon start` detaches it; `rally daemon serve` or `rallyd
  --foreground` runs it in the foreground.
- **When it acts:** A same-user client connects to its private Unix socket and sends one bounded
  `StoreRequest`.
- **How it acts:** One store-owning dispatcher applies the request to the canonical per-repo JSONL and
  SQLite projection, then returns one typed response.
- **What it can access:** The selected repository's Rally directory and its own PID, socket, and log.
- **What it cannot do:** It cannot inspect provider sessions, choose Claude versus Codex, call a
  network API, send terminal input, start an agent, or decide what work should happen.
- **How it stops:** Signal, `rally daemon stop`, or optional inactivity expiry.

`rallyd` does nothing merely because a new message exists. A caller must issue a store operation.

### `rally watch`: detector with an optional action hook

- **What:** A host-neutral long-running command that polls `.rally/log/index.json` with adaptive
  backoff.
- **Who starts it:** A person, a generated LaunchAgent/systemd service, or another supervisor.
- **When it acts:** The maximum Rally sequence increases.
- **How it acts:** It emits one activity record and, when configured, runs exactly one
  `--on-activity` shell command with Rally context in environment variables. The command runs to
  completion before another one starts.
- **What it cannot do:** It does not parse the new facts into a work decision, claim work, or prove
  that the launched agent acted. Those behaviors must live in the configured adapter/agent.
- **Important edge:** Current `--once` mode updates its cursor and exits, but does not run the
  `--on-activity` command. Continuous mode performs the command launch.

This is deterministic autonomy: Rally activity can cause a configured action without a person typing
another command, but the watcher does not invent the action.

### `cockpitd`: remote app server and session supervisor

- **What:** A loopback WebSocket service for Agent Cockpit with its own SQLite session, event,
  approval, and audit records.
- **Who starts it:** `cockpitd serve` or its `RunAtLoad`/`KeepAlive` LaunchAgent.
- **When it acts:** A token-authenticated client sends a command, an agent emits output, or an
  approval TTL expires.
- **How it acts:** It can launch and supervise a configured CLI agent, normalize events, replay
  history, and auto-deny expired approval rows.
- **Current runtime boundary:** The main `serve` entrypoint wires the Claude adapter. Codex adapter
  code exists, but the production entrypoint does not select it.
- **Security boundary:** The current shared token is not per-client identity, and its documented
  approval layer observes output rather than containing the child process. Use an OS sandbox for
  real execution containment.
- **What it is not:** It is not the canonical Rally ledger, the delivery router, or `ptyd`.

### `rally-termd`: durable inbox consumer

`rally-termd` is built in the Easy Terminal `ptyd` workspace and exposed as
`terminal-rally-point`. It remains alive after explicit startup, watches configured per-agent
`FileInbox` files, validates directives, performs PTY actions through an embedded `ptyd::Session`,
writes receipts and high-water state, and emits heartbeats.

It does **not** connect to a separate `ptyd server`. It owns an embedded PTY runtime. It also does not
auto-discover every Rally repository or agent; startup requires explicit `--ledger`, `--state`, and
one or more `--agent` values.

### Legacy Python watcher

`tools/agent-rally-watcher/` demonstrates useful push, filter, cursor, and sink behavior. It is not
the current product path. It tails `~/.agent-rally-point/apps/<slug>/changes.jsonl`, while current
Rally uses per-repository `.rally/log`. The repository's consolidation assessment marks this tool as
reference-only until native parity. Use `rally watch` for current per-repository activity detection.

### Proposed `rally-routerd`

`rally-routerd` is an architecture proposal, not a current binary. Its intended job is to consume
pending Rally envelopes, resolve an authenticated endpoint, choose a native or fallback adapter,
lease the attempt, record transport evidence, and resume after restart. It should remain a
deterministic dispatcher. An optional judge/auditor agent would be a separate Level 5 consumer, not
hidden inside the router daemon.

## RossLabs Agent Harness

### The normal agent loop is not a daemon

`harness run <goal>` is autonomous after invocation: the model can choose tools and the Rust engine
can execute multiple turns until it returns a final response or hits a safety/loop/turn boundary.
The process then exits. This is a goal-directed worker, not an always-running service.

That distinction lets the Harness be highly autonomous without leaving a permanent process on the
machine.

### `harness serve`

`harness serve` binds `127.0.0.1`, waits for HTTP requests, and starts one thread per request. Its
query endpoint spawns `harness run` and streams output to the browser. It remains in the foreground
until interrupted. Running it with shell backgrounding makes it a background server, but the code
does not install, restart, or detach itself.

### Model servers

- **Ollama** is a separate, already-running model daemon. The Harness sends it HTTP requests and can
  ask it to release a loaded model, but it does not own the Ollama service lifecycle.
- **`llama-server`** can be managed per Harness run. The Harness first reuses a healthy server if one
  already exists. Otherwise it resolves the GGUF model, spawns the server, waits for health, and
  keeps a process guard. Dropping the guard kills only the child the Harness started; it never kills
  a server it merely reused.

### Vault LaunchAgents

The Vault jobs demonstrate two kinds of background automation without a resident Harness daemon:

- `vault-ingest-watch` uses macOS `WatchPaths`. `launchd` watches the directory and launches the
  script after a change. The script takes a lock, finds unprocessed files, invokes Harness ingest,
  records success/rejection/failure, and exits.
- `vault-backfill-links` launches daily at 03:30, runs a deterministic apply pass, logs, and exits.

Both jobs can mutate the vault without a person present **after** the operator explicitly installs
and loads their LaunchAgents. The repository plists alone do not activate them.

### “Autonomous eval loop” status

The current eval-loop plan deliberately adds no hook, watcher, daemon, cron job, or host event. Its
matrix runner begins only when an operator invokes it. The name describes unattended execution after
launch, not an always-on service.

## RossLabs AI Assistant

### Routing hooks are not daemons

The AI Assistant is primarily host-reactive:

| Host event | Short-lived action |
| --- | --- |
| `SessionStart` | Build and return routing policy, clock, and judgment context |
| `UserPromptSubmit` | Classify the prompt, persist route metadata, and optionally inject a lean directive |
| `PreToolUse` | Redirect a wrong skill, block a proven Rally conflict, or warn about unclassified work |
| `Stop` | Emit a closeout reminder only when a rule requires it |

The host launches the hook command, provides JSON on stdin, waits for JSON output, and the hook exits.
No prompt or tool event means no AI Assistant process is running. The hooks can automatically block or
annotate an action because the host invokes them at the enforcement point; they cannot wake themselves.

### Scheduled self-improvement is proposal-only

`scripts/self_improve.py --install-launchd` writes a per-user LaunchAgent scheduled for 09:00. When
loaded, `launchd` runs the script once at that time. The script reads privacy-reduced routing
telemetry and writes owner-only proposal files. It does not edit the registry, routing rules, prompts,
policy, adapters, `BRAIN.md`, or memory. It is scheduled automation, not an autonomous policy editor.

### What does not exist today

The AI Assistant repository contains design notes for a persistent Rally arbiter, but no current
AI Assistant-owned always-on arbiter is implemented. Current coordination checks are hook-reactive.
Likewise, optional Ollama-assisted reranking uses an already-warm local service; it does not make the
router itself a daemon.

## Easy Terminal

### The app and the daemon are separate processes

Easy Terminal is a long-running GUI application. It is not a daemon, even though it can remain active
with no visible window. On app launch, `DaemonController` starts or adopts a private `ptyd server`,
waits for socket and health readiness, publishes the private socket to clients, and restores state.
On app exit, it asks the server to stop, then reaps or signals it if necessary.

The app private instance and the CLI instance are intentionally separate:

| Instance | Socket/state owner | Starts it | Visible through |
| --- | --- | --- | --- |
| App-private `ptyd` | Easy Terminal application-specific private runtime | Easy Terminal | Easy Terminal UI and its private client socket |
| CLI `ptyd` | User CLI state, normally `~/.config/ptyd/ptyd.sock` | `et server` or `ptyd server` | `et pane`, `et agent`, `et workspace`, raw `ptyd` commands |

An `et pane list` against the CLI socket does not list the app-private panes.

### What `ptyd` does automatically

After startup, `ptyd` can act through two mechanisms:

1. **Socket requests:** clients ask it to create panes, launch agents, register stable identities,
   write PTY input, read or subscribe to output, update status, or stop the server.
2. **Internal maintenance:** a timer thread prunes retained delivery state, maintains persisted
   scrollback, and can move idle panes to dormant state when the idle TTL is enabled.

The second mechanism is action without a new client command, but it is fixed housekeeping policy.
It is not agent reasoning.

`ptyd` has meaningful authority: it can spawn child commands, read their terminal output, inject
input, and stop them. The Unix socket is therefore a privilege boundary. Current code restricts the
socket directory and file to the owner and verifies that connecting Unix peers have the daemon's UID.

### In-app monitors are background tasks, not daemons

While Easy Terminal remains open, one app-level snapshot task polls `ptyd` once per second. UI
consumers read that shared snapshot. A liveness watchdog can report suspected daemon death;
`DaemonController` confirms it with a process and socket probe before reaping and restarting the
daemon. These tasks disappear when the Easy Terminal process exits.

### `terminal-rally-point`

Easy Terminal builds and bundles the `terminal-rally-point` binary and exposes it through
`et rally-point`. Bundling is not activation. Nothing in the inspected app-start path automatically
starts it. An operator or service manager must launch it with its explicit Rally ledger, state, and
agent roster; only then does it remain alive and react to later directives.

## Who owns which decision

| Decision | Current owner |
| --- | --- |
| Is this Rally fact valid and durably ordered? | Rally store logic, optionally serialized by `rallyd` |
| Did repository activity occur? | `rally watch` |
| Which terminal process owns this agent identity? | `ptyd` identity/pane registry |
| May this inbox directive become a PTY action? | `rally-termd` policy |
| Which provider or endpoint should receive a Rally message? | No single current owner; proposed `rally-routerd` |
| What should the agent do next to satisfy a goal? | The invoked agent loop, not the daemon |
| Should a recurring routing pattern change policy? | Human review of AI Assistant proposals |
| Should a dead Easy Terminal runtime restart? | Easy Terminal `DaemonController` after liveness proof |

## Practical operating questions

### Does a daemon need to be invoked?

Yes, once. The invoker may be invisible to the user because an app, `launchd`, systemd, or a parent
supervisor performed the invocation. After startup, the daemon waits for its configured triggers.

### Can it take actions on its own?

Yes, if “on its own” means “without another human command after startup.” It still needs a trigger,
code path, permissions, and policy. It cannot exceed the access of its operating-system user or the
capabilities implemented in its protocol.

### Can it invent work?

Not unless it contains or launches a goal-directed agent/policy loop. Current `rallyd` and `ptyd` do
not. `rally watch` may launch such a loop, but the configured command and launched agent own the goal.

### How do I know what started it?

Use the process parent, service registry, PID/state files, socket path, and logs together. A parent PID
of `1` usually means the process detached or was adopted by the system service manager; it does not
prove which product originally launched it.

### How should every daemon be operated?

Every production daemon should expose:

- an explicit `start`, `status`, `stop`, and health/readiness contract;
- one discoverable socket or endpoint and one documented state root;
- the exact triggers it handles and actions it may perform;
- least-privilege filesystem, process, network, and credential access;
- idempotency keys or cursors for replay-safe work;
- durable receipts that distinguish queued, transported, accepted, completed, and verified;
- bounded retries, backoff, and a visible terminal failure state;
- logs that identify the trigger, policy decision, target, action, and outcome;
- a single-instance or lease rule when duplicate workers would double-act.

## Implication for Rally Point's direction

Rally should expose one user-facing service lifecycle while preserving separate internal authority:

```text
rally service
  ├── rallyd          canonical state and ordering
  ├── rally-routerd   deterministic delivery selection and attempt recovery (proposed)
  └── ptyd            optional PTY/process endpoint, only when needed
```

This keeps the common experience simple without turning one process into a universal privileged
monolith. `rallyd` should not gain process or network authority. `ptyd` should not gain canonical
coordination authority. The proposed router should not become an unbounded reasoning agent. A future
auditor or judge can observe Rally facts and propose or trigger bounded interventions through the same
recorded protocol.

## Source map

### Agent Rally Point (`e332f4f`)

- `crates/rallyd/src/main.rs`
- `crates/rally-cli/src/rallyd_core.rs`
- `crates/rally-cli/src/lib.rs` (`command_watch`)
- `crates/cockpitd/src/main.rs`
- `crates/cockpitd/src/transport/ws.rs`
- `docs/DAEMON-AND-TRANSPORT-ARCHITECTURE.md`
- `docs/CONSOLIDATION-ASSESSMENT-2026-06-02.md`
- `tools/agent-rally-watcher/`

### RossLabs Agent Harness (`16689b2`)

- `crates/cli/src/main.rs`
- `crates/cli/src/serve.rs`
- `crates/engine/src/lib.rs`
- `crates/provider/src/llama_server_process.rs`
- `tools/launchd/README.md`
- `tools/launchd/*.plist`
- `tools/scripts/vault_ingest_watch.sh`
- `tools/scripts/vault_backfill_daily.sh`
- `docs/plans/harness-autonomous-eval-loop.md`

### RossLabs AI Assistant (`a5f55aa`)

- `hooks/hooks.json`
- `hooks/route-guard.sh`
- `hooks/session-start.sh`
- `scripts/route_guard.py`
- `scripts/self_improve.py`
- `docs/hooks.md`
- `docs/design/2026-07-09-rally-arbiter-daemon-notes.md`

### Easy Terminal (`99e4c78`)

- `README.md`
- `Sources/EasyTerminalApp.swift`
- `Sources/Ptyd/DaemonController.swift`
- `Sources/Ptyd/DaemonSnapshotStore.swift`
- `daemon/ptyd/src/main.rs`
- `daemon/ptyd/src/termd.rs`
- `daemon/ptyd/src/terminal_rally_point_main.rs`
