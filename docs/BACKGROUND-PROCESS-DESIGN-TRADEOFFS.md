<!-- markdownlint-disable MD013 -->
<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Background Process, Daemon, and Communication Design Guide

> **Status:** source-grounded architecture and tradeoff guide.
> **Inspected:** 2026-08-08.
> **Local source baselines:** Agent Rally Point `6dcb024`; Agent Harness `16689b2`;
> AI Assistant `a5f55aa`; Easy Terminal `99e4c78`.
> **Installed host surfaces:** Codex CLI `0.147.0`, Claude Code `2.1.226`, OpenCode `1.4.3`;
> Gemini CLI was not installed on the inspected machine.
> **Evidence labels:** “current” means source-confirmed at the pinned revision. “Proposed” means
> documented product direction without a current production path.

## Executive decision

Choose the process **role and authority before choosing a language or calling it a daemon**.
Python, Rust, Swift, JavaScript, or a shell can implement a long-running service. The architectural
difference comes from what keeps the process alive, what triggers it, which state and resources it
owns, what it may do, and how failures are recovered.

For the current products, the clean boundary is:

```text
host hook or user command
  -> Rally canonical record and durable recipient intent
  -> deterministic route planner / worker
  -> native agent server, A2A, ptyd, or terminal fallback
  -> target-authored ACK, progress, completion, or blocker

reasoning remains in the invoked agent or LLM
coordination truth remains in Rally
process and PTY ownership remains in ptyd
host-boundary enforcement remains in AI Assistant hooks
```

Keep `rallyd`, the proposed router, and `ptyd` logically separate even if one user-facing
`rally service` command supervises them. They hold different authority and have different failure
domains. Do not create a single process that simultaneously owns the canonical ledger, network
credentials, provider adapters, PTYs, and model judgment.

## 1. Classification model

“Script,” “binary,” “daemon,” “server,” and “agent” answer different questions. Classify every
background component along six independent axes.

| Axis | Question | Examples |
| --- | --- | --- |
| Implementation | What is the program made from? | Python script, shell script, Rust binary, Swift application |
| Runtime role | What does the running process do? | Command, hook, worker, watcher, server, supervisor, agent loop |
| Lifecycle | How long does one invocation live? | One-shot, scheduled, app-owned, session-owned, persistent service |
| Trigger | What causes an action? | User request, stdin, socket, HTTP, file event, ledger append, timer, child exit |
| Authority | What may it access or change? | Files, ledger, network, credentials, child processes, terminal input, model tools |
| Decision logic | How does it choose an action? | Fixed procedure, deterministic policy, rules, model judgment, human approval |

### Runtime roles

| Role | Definition | Usually stays alive? | Example |
| --- | --- | ---: | --- |
| Command | Performs one requested operation and exits | No | `rally room`, `harness run` process wrapper |
| Hook | Host invokes a short command at a lifecycle boundary | No | AI Assistant `PreToolUse` hook |
| Scheduled job | Service manager starts one bounded pass | No | AI Assistant daily self-improvement |
| Worker | Consumes a bounded item from a queue or parent | Maybe | One delivery attempt |
| Watcher | Waits for a file, ledger, or timer change | Yes while watching | `rally watch` |
| Server | Waits for requests on an endpoint | Yes | `rallyd`, `harness serve`, OpenCode server |
| Supervisor | Owns or monitors child-process lifecycle | Yes | Easy Terminal `DaemonController`, `cockpitd` |
| Daemon/service | Operationally managed long-running background process | Yes | `ptyd`, managed `rallyd` |
| Agent loop | Uses model judgment to choose several steps toward a supplied goal | Until goal/limit | `harness run`, Claude/Codex session |

### Logic and autonomy levels

| Level | Logic | Can act after startup without another human command? | Can invent the next step? |
| ---: | --- | ---: | ---: |
| 0 | Passive request handler | Only after a request | No |
| 1 | Event reaction | Yes, after a known event | No |
| 2 | Timer maintenance | Yes, on schedule or TTL | No |
| 3 | Process supervision | Yes, on health/process changes | No |
| 4 | Deterministic policy | Yes, when rules select an action | No model judgment |
| 5 | Goal-directed agent | Yes, within a supplied goal and tools | Yes |
| 6 | Human-governed agent | Yes, with approval gates on bounded actions | Yes, subject to approval |

A daemon normally sits at Levels 0–4. It has logic, but its “brain” is a state machine or policy
function. An LLM is optional and should be explicit. Putting a model call inside a daemon changes
the component from deterministic infrastructure into an agentic decision-maker and expands its
testing, authorization, cost, latency, privacy, and failure requirements.

## 2. Where a background process can live

| Placement | Lifetime owner | Reach | Strong fit | Main tradeoff |
| --- | --- | --- | --- | --- |
| Function or thread inside an app | App process | One app instance | UI polling, local caches | Dies and shares failures with app |
| Embedded child process | Parent app/session | Parent and child | Model server or bounded worker | Parent must reap and classify ownership |
| Per-repository service | Rally/service manager | One repository | Canonical coordination, repo router | Many repos can mean many services |
| Per-user laptop service | `launchd`/systemd user manager | All user apps | PTY runtime, local adapter broker | Wider authority and identity namespace |
| Machine-wide system daemon | System service manager | All users/apps | Privileged hardware/network services | Root boundary and multi-user isolation burden |
| Local container or VM | Container/VM manager | Declared mounts and network | Isolation, reproducible services | IPC, mounts, startup, and debugging friction |
| LAN server | Remote service manager | Authorized machines | Team-shared routing or model inference | Network auth, discovery, TLS, availability |
| Cloud service | Orchestrator/platform | Organizations/regions | Cross-device coordination and elastic work | Cost, privacy, tenant isolation, outage dependency |
| Cluster worker | Queue/orchestrator | Distributed jobs | High throughput, resilient queues | Highest deployment and consistency complexity |

Placement is not capability. A Python service can run inside one app, for one user, on a laptop,
inside a container, or in a cloud cluster. Placement changes discovery, trust, latency, failure
scope, and who operates it.

### Placement guidance for the current stack

- `rallyd`: per repository, same user, private Unix socket.
- Proposed `rally-routerd`: per repository initially; a future user-wide broker may supervise
  several repo workers but must preserve repository isolation.
- `ptyd`: per user/runtime socket because it owns live processes and terminals across app clients.
- Easy Terminal snapshot/recovery tasks: inside the GUI application.
- `cockpitd`: per user or app deployment; separate from the Rally repository store.
- `harness serve`: localhost development server unless authentication and remote policy are added.
- Model inference: embedded child for bounded ownership, or explicit external local/cloud service.
- Hooks: inside the host lifecycle; never add a daemon when the host already supplies the event.
- A2A endpoints: local, LAN, or cloud depending on discovery and authentication requirements.

## 3. Core design-decision register

Every daemon, watcher, worker, server, or scheduled job needs an explicit answer to these decisions.

| ID | Decision | Main options | Tradeoff and required evidence |
| --- | --- | --- | --- |
| D01 | Runtime role | Command, hook, job, watcher, worker, server, supervisor, agent | Choose the least persistent role that meets the requirement |
| D02 | Lifecycle owner | User, parent app, `launchd`, systemd, container, cloud orchestrator | The owner must start, stop, restart, log, and upgrade it |
| D03 | Scope | Request, session, repo, user, machine, organization | Wider scope improves reuse but expands authority and isolation needs |
| D04 | Activation | Manual, app start, boot/login, socket activation, file/timer event | Prefer on-demand activation when readiness latency permits |
| D05 | Managed versus adopted | Started here, reused existing, external | Stop only resources the component started and proved it owns |
| D06 | Single-instance rule | PID, file lock, socket ownership, distributed lease | The proof must prevent two consumers from double-acting |
| D07 | Readiness | Socket exists, protocol ping, dependency probe, warm model | A PID or file alone does not prove requests can succeed |
| D08 | Liveness | Process probe, protocol ping, heartbeat, observed work | Liveness does not prove correctness or progress |
| D09 | Restart | Never, on failure, bounded backoff, always, manual | Avoid restart storms; preserve terminal failure visibility |
| D10 | Shutdown | Signal, RPC, parent cancellation, lease expiry | Define drain deadline, state flush, and child reaping |
| D11 | Trigger | Direct request, queue, file event, timer, child exit, model output | Trigger must be discoverable and replay behavior explicit |
| D12 | Transport | In-process, stdin/stdout, Unix socket, TCP, HTTP, WebSocket, SSE, files | Pick from locality, streaming, security, and client compatibility |
| D13 | Framing | Newline JSON, length prefix, HTTP body, event frame, raw bytes | Bound every frame and define partial-write recovery |
| D14 | Schema | Closed typed enum, JSON Schema, OpenAPI, protobuf, opaque text | Strong types reduce ambiguity; extensions preserve forward compatibility |
| D15 | Versioning | Exact version, negotiated range, additive fields, capability probe | Reject unsafe incompatibility and record fallback reason |
| D16 | Directionality | Request/reply, notification, duplex stream, polling | One-way notifications cannot prove acceptance |
| D17 | Streaming | None, chunked, SSE, WebSocket, binary side channel | Define reconnection, cursor, ordering, and backpressure |
| D18 | Backpressure | Bounded queue, drop/coalesce, block producer, spill to disk | Make loss or delay observable; never silently grow memory |
| D19 | Correlation | Request ID, session ID, task ID, `(recipient, sequence)` | Keep one stable correlation key across every adapter |
| D20 | Idempotency | None, request key, endpoint dedup, compare-and-set | Required for safe retry after an ambiguous failure |
| D21 | Delivery semantics | Queued, sent, seen, accepted, acted, completed, verified | Do not promote transport success into semantic completion |
| D22 | Ordering | None, per connection, per recipient, per repo, total | Broader ordering reduces concurrency and raises coordination cost |
| D23 | Source of truth | Memory, JSONL, database, provider transcript, external queue | Name one canonical truth; all other views are projections or evidence |
| D24 | Recovery cursor | Sequence, checkpoint, lease, replay token, task state | Specify first activation, restart, and missing-state behavior |
| D25 | Crash window | Before action, after action, after receipt, after checkpoint | Classify unknown outcomes and test a kill at every transition |
| D26 | Identity | OS user, process, session, agent, repo, organization | Display names are not stable authenticated identities |
| D27 | Authentication | Same UID, token, mTLS, OAuth, signed assertion | Match mechanism to local versus remote threat boundary |
| D28 | Authorization | Role, capability, target scope, operation, file/repo scope | Authenticate first, then authorize the exact action |
| D29 | Secret handling | Environment, file, keychain, brokered token | Define redaction, rotation need, and child inheritance |
| D30 | Network exposure | No network, loopback, LAN, internet | Default to the narrowest bind; remote requires transport security |
| D31 | Route selection | Hard pin, preference order, health/capability policy, model choice | Infrastructure routing should be deterministic and explainable |
| D32 | Reasoning | None, rules, classifier, LLM, human | A model expands nondeterminism; isolate it from canonical storage |
| D33 | Approval | None, advisory, blocking, expiring lease | Approval must gate execution, not merely observe output afterward |
| D34 | Observability | Logs, metrics, traces, receipts, audit ledger | Record trigger, decision, target, attempt, result, and uncertainty |
| D35 | Resource bounds | Time, memory, payload, queue, child count, concurrency | Bound every attacker- or agent-controlled dimension |
| D36 | Upgrade | Restart, rolling, side-by-side protocol window | Test old/new readers and writers before changing shared wire types |
| D37 | Fallback | Fail closed, queue, alternate adapter, terminal injection | Fallback must not duplicate a possibly successful attempt |
| D38 | Test strategy | Unit, contract fixture, fault injection, process test, matrix | Tests must include restart, ambiguity, version, and permission boundaries |

## 4. Process-form tradeoffs

| Form | Startup/latency | State | Failure isolation | Operational burden | Best use |
| --- | --- | --- | --- | --- | --- |
| In-process function/thread | Lowest | Shares app memory | Lowest isolation | Low until app grows complex | UI refresh, pure planning, local caches |
| One-shot script/binary | Startup each time | Reloads durable state | Strong fresh-process isolation | Low | Hooks, migrations, bounded delivery attempt |
| Scheduled one-shot job | Scheduler latency | Durable files/database | Strong | Low/medium | Cleanup, analysis, ingestion, backfill |
| Long-running watcher | Low after startup | Cursor plus optional memory | Medium | Medium | Ledger/file activity detection |
| Persistent local daemon | Low | Warm shared state | Strong process boundary | Medium/high | Shared socket, PTY ownership, router worker |
| Remote service | Network latency | Remote durable state | Strong host boundary | High | Team/organization access, elastic work |
| Agent loop | Model latency/cost | Session plus tools | Depends on sandbox | High semantic risk | Work requiring judgment rather than fixed policy |

### Python versus a compiled daemon

Both can expose the same sockets, HTTP APIs, file watchers, timers, child-process controls, and
agent loops. For these products, language is a secondary choice.

| Consideration | Python | Rust |
| --- | --- | --- |
| Iteration and adapter prototypes | Faster | More ceremony |
| Packaging | Runtime and dependencies unless bundled | Single native binary is practical |
| Cold start and idle memory | Usually higher | Usually lower |
| I/O concurrency | Adequate with threads/async | Strong native concurrency |
| CPU-bound work | Multiprocessing/native extension often needed | Native parallel execution |
| Process/PTY/file-descriptor safety | Possible, discipline-dependent | Stronger compile-time ownership guarantees |
| Schema experimentation | Very fast | Stronger typed contracts |
| Best current fit | Provider adapter prototype, scheduled job, migration | `ptyd`, `rallyd`, durable router core |

Measure the actual path. A resident Python service can outperform a repeatedly spawned Rust command
when the repeated path pays shell, interpreter, dependency, and initialization costs. Conversely, a
small Rust binary can make a hook fast enough that a daemon is unnecessary.

## 5. Communication-style tradeoffs

| Style | Data shape | Strengths | Friction and failure mode | Current examples |
| --- | --- | --- | --- | --- |
| Function call | Typed memory objects | Fastest, compiler-visible | Same process and failure domain | Pure delivery planner |
| stdin/stdout | Text, NDJSON, JSON-RPC | Simple child ownership, broad language support | Stdout contamination breaks framing; parent exit ends channel | MCP stdio, AI hooks |
| Unix socket | Bytes, NDJSON, custom RPC | Low-latency local IPC, filesystem permissions, peer UID | Unix-only; socket discovery/versioning required | `rallyd`, `ptyd`, Codex daemon proxy |
| TCP socket | Bytes or framed protocol | Cross-host capable | TLS/auth/framing/reconnect must be designed | Model servers |
| HTTP request/reply | JSON, multipart, protobuf | Tooling, proxies, auth ecosystem | Request overhead and limited server push | Harness/OpenCode/A2A bindings |
| WebSocket | Duplex framed messages | Interactive commands and events | Connection ownership, replay, backpressure, reconnect | `cockpitd`, optional Codex app-server |
| SSE | Server-to-client event stream | Browser-friendly streaming and retry IDs | Primarily one direction; POST path still needed | Harness query, OpenCode events, MCP HTTP |
| JSONL file queue | One record per line | Durable, inspectable, language-neutral, offline | Local/shared filesystem only; locking, polling, compaction | Rally facts, directives, receipts |
| Database queue | Typed rows and transactions | Queries, leases, atomic transitions | Schema/migration/locking coupling | Cockpit SQLite; possible router state |
| PTY keystrokes | Raw bytes/text | Universal fallback for terminal agents | Lossy, ambiguous, non-idempotent, hard to authenticate | `ptyd`, tmux/cmux injection |
| Native provider API | Provider schema and stream | Richest provider semantics | Version drift and vendor coupling | Claude/Codex/OpenCode adapters |
| MCP | JSON-RPC tools/resources/prompts over stdio or HTTP | Broad tool-client compatibility | Tool protocol, not durable peer coordination or wake guarantee | `ptyd-mcp`, host MCP servers |
| A2A | AgentCard, Message, Task, Part, Artifact over JSON-RPC/gRPC/REST | Peer-agent discovery and async task semantics | Remote auth, version/capability negotiation, Rally field mapping | Proposed Rally adapter |

JSON is only an encoding. Two JSON protocols remain incompatible when their method names, identity,
state machines, error rules, framing, versioning, or delivery meanings differ.

## 6. Current component inventory

| Product | Component | Role/lifecycle | Trigger | Inputs | Outputs | Logic | Principal friction |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Rally | `rally` CLI | One-shot command | User/hook/script | argv, env, repo files | JSON/text, ledger mutation, transport attempt | Deterministic command logic | Ends before later delivery unless another consumer exists |
| Rally | `rallyd` | Per-repo daemon/server | Unix-socket request | `StoreRequest` NDJSON | `StoreResponse` NDJSON, canonical state | Level 0 deterministic dispatcher | Store-only protocol; no provider/process authority |
| Rally | `rally watch` | Foreground or managed watcher | Ledger sequence change | Index/cursor and options | Activity record; optional command | Level 1 detection | Detects activity, not semantic work |
| Rally | `FileInbox` | Durable substrate, not a process | Writer append/reader poll | `Directive` JSONL | `Receipt` JSONL and reads | Sequence/lock rules | Same-filesystem and same-UID trust assumptions |
| Rally | `cockpitd` | WebSocket server/supervisor | Client frame, child event, approval TTL | Tagged JSON `ClientCommand` | Tagged JSON `ServerEvent` | Level 0–3 | Separate identity and SQLite authority from Rally |
| Rally/Easy Terminal | `terminal-rally-point` | Inbox watcher/worker daemon | File event plus recovery poll | Rally directives and CLI policy | Embedded PTY actions, receipts, high-water | Level 1 policy worker | Embeds a separate session; does not call live `ptyd` socket |
| Rally | Legacy Python watcher | Companion file watcher | Legacy global JSONL append | Old `changes.jsonl` | Filtered sink writes | Level 1 | Watches retired global store, not current `.rally/log` |
| Rally | Proposed `rally-routerd` | Per-repo deterministic worker | Pending directive, retry timer, endpoint change | Envelope, registry, policy, attempt state | Adapter calls, attempts, receipts | Level 4 | Not implemented; cutover and idempotency are critical |
| Easy Terminal | `ptyd` | Per-user/runtime PTY daemon | Socket RPC, timer, signal | Custom JSON request; raw bytes; launch config | Typed JSON result/error; binary streams; child processes | Level 0–3 | Custom protocol and privileged process authority |
| Easy Terminal | `ptyd-mcp` | Host-owned stdio MCP server | MCP tool call | MCP JSON-RPC | MCP tool result after `ptyd` RPC | Level 0 translator | Copies a subset; not a durable delivery or raw streaming protocol |
| Easy Terminal | `DaemonController` | App-internal supervisor | App start, retry, confirmed death | Binary/socket paths and probes | Spawn/adopt/recover/stop; UI state | Level 3 | Exists only while Easy Terminal runs |
| Easy Terminal | `DaemonSnapshotStore` | App-internal polling task | One-second timer/manual refresh | `workspace.snapshot` | Swift observable projection | Level 2 | App-only, polling latency, no cross-app API |
| Harness | `harness run` | One-shot goal-directed agent | CLI/HTTP parent | Goal, model, session, tools | Event/session JSONL, stdout, tool effects | Level 5 | Autonomous but not persistent coordination |
| Harness | `harness serve` | Foreground localhost HTTP server | HTTP request | JSON and URL params | JSON, HTML, SSE, spawned run | Level 0–3 | No built-in daemon install/restart/auth contract |
| Harness | Managed `llama-server` | Session-owned child server | Provider route | Model config, GGUF, HTTP health | Inference HTTP | Deterministic ownership/health logic | Private Ollama disk layout assumption; high warm-resource cost |
| Harness | External Ollama | Independently managed model daemon | HTTP request | Ollama JSON/model request | Token/model response | Model inference | Harness uses but does not own lifecycle |
| Harness | Vault ingest job | `launchd` file-triggered one-shot | WatchPaths | Drop-zone files and config | Vault writes and logs | Deterministic job plus invoked model tools | macOS-specific activation and filesystem coupling |
| Harness | Vault backfill job | `launchd` scheduled one-shot | Daily timer | Vault files/config | Deterministic managed link edits and logs | Deterministic job | Scheduled latency and macOS service-manager coupling |
| AI Assistant | Host hooks | Host-invoked one-shot commands | Session/prompt/tool/stop events | Host JSON stdin, env, Rally state | Host JSON stdout, warnings/blocks, telemetry | Deterministic routing/policy | Host schema differences; cannot wake without an event |
| AI Assistant | Self-improvement job | Daily `launchd` one-shot | 09:00 timer | Privacy-reduced telemetry | Proposal files and logs | Deterministic analysis plus bounded inference | Proposal-only; not live enforcement |
| Codex | App server | Foreground or managed local server | stdio/Unix/WebSocket request | Versioned app-server protocol | Responses, notifications, generated schemas | Server plus agent-session control | Experimental surface and version negotiation |
| Claude Code | Background agent supervisor | Claude-owned background sessions | `--background`, agent UI/actions | Claude session commands/state | Session status and agent messages | Provider-native supervision plus agent reasoning | Provider/account namespace; Rally cannot assume interception |
| OpenCode | Headless server | Local HTTP service | HTTP request/SSE subscription | OpenAPI 3.1 HTTP JSON | JSON and global SSE events | Server plus agent-session control | Network discovery/auth and OpenCode-specific session model |
| OpenCode | ACP server | Host/client protocol server | ACP client | ACP protocol | Agent events/results | Agent-client bridge | Different semantic boundary from Rally coordination |
| External | A2A agent | Local or remote agent server | A2A operation | AgentCard/Message/Task/Part/Artifact | Task/message/artifact state | Agent reasoning behind protocol | Remote trust and Rally-extension mapping |

## 7. Detailed input/output contracts

### 7.1 `rallyd`

| Dimension | Current decision |
| --- | --- |
| Discovery | Socket path read from `.rally/rallyd.sock.addr` |
| Transport | Private same-user Unix socket |
| Framing | One newline-terminated JSON request and one JSON response per connection |
| Request | `StoreRequest { wire_version, engagement, op }` |
| Response | `StoreResponse::Ok(StoreOk)` or `Err(StoreError)` |
| Version | Exact `WIRE_VERSION = 2`; `Ping` returns repo root, PID, and version |
| Payload | Closed operation enums; leaf Rally types cross as `serde_json::Value` and revalidate at CLI boundary |
| Limits | One line capped at 8 MiB |
| State | Canonical room JSONL plus SQLite projection; socket, PID, and log runtime files |
| Concurrency | Reader connections feed one store-owning dispatcher |
| Authority | Repository Rally state only; no external process, PTY, provider, or network authority |
| Compatibility | JSON is easy cross-language; exact enum/version/repo checks require a generated or maintained client |

Friction is low for Rally clients, medium for another language, and intentionally high for provider
delivery because the protocol refuses operations outside the store charter.

### 7.2 Rally facts, directives, and receipts

| Surface | Shape | Identity/correlation | Durability meaning |
| --- | --- | --- | --- |
| Canonical facts | JSON objects in engagement JSONL | Event ID, sequence, tool/session, ref/scope | Coordination source of truth |
| Directives | `Directive { from, to, seq, kind, text, ... }` JSONL | `(to, seq)` plus optional higher-level correlation | Durable intent for one recipient |
| Receipts | `Receipt` JSONL with delivery status/evidence | Recipient sequence/correlation | Consumer-reported delivery outcome |
| SQLite | Rebuildable projection | Fact sequence/event IDs | Query performance, not independent truth |

FileInbox provides per-recipient sequence allocation, private permissions, bounded frames, advisory
locking, sync-on-append, and partial-tail tolerance. It does not provide remote transport,
cryptographic sender authentication, or exactly-once PTY effects.

### 7.3 `ptyd`

| Dimension | Current decision |
| --- | --- |
| Discovery | `PTYD_SOCKET_PATH` or CLI/app-specific default socket |
| Transport | Unix socket; optional feature-gated remote transport exists outside Rally's current path |
| Request | `{ "id": string, "method": string, "params": object }` plus newline |
| Success | `{ "id", "result": { "type": tag, ... } }` |
| Failure | `{ "id", "error": { "code", "message" } }` |
| Streams | JSON subscription ACK, then `[u32 big-endian length][payload bytes]`; zero length marks EOF/discontinuity |
| Important methods | Workspace/tab/pane lifecycle, raw/snapshot subscriptions, agent register/send/read/wait/stop, delivery ACK/get, events, capabilities, config, server stop |
| Identity | Stable identity-to-pane binding; exact identity precedes display-name resolution; ambiguity refuses |
| Delivery evidence | Transport ladder such as sent/seen/acted plus distinct semantic delivery state |
| State | Session/workspace/tab/pane tree, scrollback, records, identity bindings, event ledgers |
| Authority | Spawns and signals child processes; reads output; injects terminal input |
| Security | Private socket directory/file and same-UID peer verification |

The request envelope resembles RPC but is **not JSON-RPC 2.0**: it has no `jsonrpc: "2.0"`, uses a
custom result tag, and changes framing after a subscription. A JSON-RPC or HTTP client therefore
needs an adapter even though every one-shot payload is JSON.

### 7.4 `ptyd-mcp`

`ptyd-mcp` is a separate stdio MCP server and a client of the running `ptyd`. The MCP host launches
it and owns its stdin/stdout lifecycle. It exposes orchestration tools such as pane listing, recent
output reading, pane text injection, and agent status. Each tool performs a `ptyd` socket operation.

Compatibility is high for MCP hosts and low for continuous terminal rendering. The adapter drains a
bounded recent-output window and returns it as a tool result; it deliberately does not relay the raw
binary render stream through MCP. It also does not turn MCP tool success into a Rally target ACK.

### 7.5 `terminal-rally-point` / `rally-termd`

| Input | Output | Recovery behavior | Friction |
| --- | --- | --- | --- |
| `--ledger`, `--state`, agents and role lists | PID lock and heartbeat | One process per state root | Explicit configuration; no room discovery |
| New per-agent `Directive` records | Policy-authorized PTY action | File event plus polling recovery | Local file boundary only |
| Sender/controller/injector policy | Success/failure `Receipt` | Receipt before high-water advance | Claimed file identity is not cryptographic authentication |
| Per-agent high-water file | Restart cursor | Fresh state baselines to current max | Fresh state skips existing backlog |

The production process embeds its own `ptyd::Session`; it does not send to the independently running
`ptyd server`. That makes source-code reuse high but runtime compatibility low with Easy Terminal's
already-registered panes. A future Rally `PtydAdapter` should call the live server socket.

### 7.6 `cockpitd`

| Dimension | Current decision |
| --- | --- |
| Transport | Authenticated WebSocket |
| Request | `ClientCommand` tagged by `t`, including hello, list/open/launch/close session, prompt, steer, approval, audit query |
| Output | `ServerEvent` tagged by `t`, including session lists/snapshots/status, normalized events, approval request, errors |
| State | Separate session/event/approval and audit SQLite stores |
| Authority | Launch and supervise configured agent CLI; manage app-facing approvals |
| Compatibility | Strong for Cockpit clients; weak direct fit with Rally because IDs, state, approvals, and replay authority differ |

Reuse the provider event-codec lessons. Do not make the proposed Rally router depend on Cockpit's
database or session supervisor.

### 7.7 `harness serve` and `harness run`

`harness serve` binds loopback HTTP and uses one thread per request. It accepts browser HTTP/JSON,
reads session JSONL, and converts a spawned `harness run` line-oriented stdout stream into SSE.

| Interface | Input | Output |
| --- | --- | --- |
| `GET /api/models` | None | JSON routes and models |
| `GET /api/session?session=...` | Path-safe session ID | Filtered conversational JSON |
| `POST /api/toggle` | `{ id, enabled }` | `{ "ok": true }` and registry mutation |
| `POST /api/query` | `{ task, model, session }` | SSE `data:` frames ending `[DONE]` |
| `harness run` | Goal, model, session, tool/sandbox configuration | stdout plus durable event/session JSONL and tool effects |

HTTP/SSE is broadly compatible, but the current server has no Rally envelope, endpoint registry,
target ACK, or service-install/restart contract. An adapter must add correlation and publish results
back to Rally rather than treating SSE EOF as task verification.

### 7.8 AI Assistant hooks and scheduled analysis

Hooks consume host-provided JSON on stdin and return host-specific JSON on stdout. Their protocol is
short-lived and synchronous: a host lifecycle event starts the process, waits for a decision, and
then the process exits. This is ideal for enter/check/claim/guard behavior because the host already
provides the enforcement point.

The daily self-improvement LaunchAgent reads privacy-reduced telemetry and writes proposals. It is a
scheduled one-shot job. It cannot enforce a prompt while idle and deliberately cannot modify active
policy, routes, adapters, or memory.

### 7.9 Model services

| Model placement | Input/output | Ownership | Tradeoff |
| --- | --- | --- | --- |
| In-process model | Tensors/tokens in memory | App | Lowest IPC, largest app memory/failure coupling |
| Managed `llama-server` child | Local HTTP JSON/token stream | Harness run that started it | Warm inference with bounded child ownership |
| External Ollama/local server | Provider HTTP API | External service/app | Shared models; separate discovery and lifecycle |
| LAN inference server | Authenticated HTTP/gRPC | Team/server operator | Shared accelerator; network latency and privacy |
| Cloud LLM API | HTTPS provider schema/stream | Provider | Elastic and capable; cost, egress, credentials, outage dependency |

The daemon does not become the model. It may own a model process, call a model endpoint, or forward a
request. The presence of an LLM only changes decision logic when the component lets model output
select subsequent actions.

## 8. Compatibility and communication-friction matrix

### Friction scale

| Level | Meaning |
| ---: | --- |
| 0 | Same protocol and semantics; direct call |
| 1 | Mechanical framing or generated-client adaptation; no intended semantic loss |
| 2 | Identity/state mapping required; common semantics mostly preserved |
| 3 | Material semantic gap or delivery evidence loss; explicit policy required |
| 4 | No current direct path or unsafe without a new bridge/auth boundary |

| From → to | Friction | What translates | What can be lost or confused |
| --- | ---: | --- | --- |
| Rally CLI → `rallyd` | 0 | Native store wire | Nothing inside supported `StoreOp`; wrong version/repo rejects |
| Rally fact → FileInbox directive | 1 | Canonical coordination event to recipient intent | Facts not required for delivery should remain references, not repeated prose |
| Rally router → `ptyd` | 2 | Agent/session identity, text, request ID, receipt ladder | Claims/dependencies are not PTY payloads; PTY action is not semantic ACK |
| Rally router → tmux/cmux | 3 | Text to framed keystrokes | Structured identity, idempotency, exact acceptance, rich content |
| FileInbox → `rally-termd` | 1 | Native directive/receipt types | Exactly-once effect is impossible for ordinary keystrokes |
| `rally-termd` → live Easy Terminal `ptyd` | 4 current | Would require socket client adapter | Current process embeds a separate session and cannot see live registry |
| MCP host → `ptyd-mcp` → `ptyd` | 1–2 | MCP tool schema to custom socket RPC | Continuous binary stream and Rally delivery semantics |
| Rally → MCP server | 2–3 | Rally request to tool invocation | MCP is tool/client protocol; no automatic target wake or peer task history |
| Rally → A2A agent | 2 | Identity/capability to AgentCard; intent to Message/Task; output to Artifact/status | Rally claims, repo scope, receipts, and verification need extensions/references |
| MCP ↔ A2A | 3 | Tool call versus peer-agent message/task | Different role model, lifecycle, discovery, and task ownership |
| Rally → `cockpitd` | 3 | Rally identity/session to Cockpit UUID and command | Competing stores, approval meaning, provider/account versus repo scope |
| Rally → Harness run | 2–3 | Intent to goal/session/model; events back to Rally | No native ACK/correlation until adapter adds it |
| Rally → Harness serve | 2–3 | Envelope to HTTP query/SSE | SSE completion is not independent verification |
| Rally → Codex app-server | 2 | Envelope to versioned app-server request/session | Experimental schema drift; Rally ACK must remain separate |
| Rally → Claude background/native session | 2–3 | Rally target/message to Claude session/channel | Provider namespace and interception availability; native send is not Rally completion |
| Rally → OpenCode server | 2 | Endpoint/session/message to OpenAPI HTTP; events from SSE | Auth/discovery and OpenCode-specific task/session states |
| Rally → local/cloud LLM API | 3 | Prompt/tool context to provider schema | This invokes a model; it does not address an already-running agent identity |
| AI hook → Rally CLI/store | 1 | Host event to check/claim/fact | Hook timeout budget and host schema differences |
| Rally → AI hook | 4 asynchronous | No active process to receive it | Hooks cannot wake themselves between host events |

## 9. Canonical envelope and translation rules

Rally should preserve one provider-neutral envelope and permit adapter-owned extensions.

```json
{
  "envelope_version": 1,
  "message_id": "msg_...",
  "correlation_id": "work_...",
  "idempotency_key": "delivery_...",
  "sender": {
    "principal": "user-or-service",
    "agent": "codex:...",
    "session": "sess:..."
  },
  "recipient": {
    "agent": "claude_code:...",
    "session": "optional-exact-session"
  },
  "context": {
    "repository": "repo-id",
    "engagement": "engagement-id",
    "scope": ["file:..."],
    "intent_ref": "fact-id"
  },
  "content": {
    "parts": [
      { "type": "text", "text": "..." },
      { "type": "artifact_ref", "uri": "...", "digest": "..." }
    ]
  },
  "delivery": {
    "deadline": "...",
    "required_receipt": "accepted",
    "fallback_policy": "safe-alternate-or-queue"
  },
  "extensions": {
    "provider-name": {}
  }
}
```

Translation rules:

1. Record the canonical message before any transport attempt.
2. Keep claims, dependencies, decisions, and history as Rally references unless the target protocol
   has an equivalent typed field.
3. Preserve unrecognized provider data under a namespaced opaque extension when safe.
4. Reject a route when a required field cannot survive translation.
5. Never map provider “sent” to Rally “accepted,” “completed,” or “verified.”
6. Keep one correlation and idempotency key across retries and alternate transports.
7. After an ambiguous timeout, do not use a non-idempotent fallback until policy resolves whether
   duplicate execution is acceptable.
8. Record adapter choice, capability probe, version, payload digest, timing, and returned evidence.

## 10. Communication paths

### Claude → Claude

```text
Claude sender hook/native ingress
  -> Rally canonical message + recipient directive
  -> router chooses exact Claude endpoint when capability and session match
  -> Claude-native delivery
  -> provider transport evidence
  -> target Claude writes Rally ACK/progress/completion
```

Without a verified inbound interception surface, Claude-native messages sent outside Rally cannot be
guaranteed in Rally history. Rally can make its own send path canonical; it cannot claim control of a
provider path it cannot observe.

### Codex → Codex

The inspected Codex `0.147.0` app-server accepts `stdio://`, Unix-socket, and WebSocket listeners;
its managed daemon has start/restart/stop/version controls and a stdio-to-socket proxy. Rally should
use generated JSON Schema fixtures and an exact capability/version handshake. The app-server is a
session transport and execution surface, not Rally's coordination ledger.

### Claude ↔ Codex

Both directions should use the same Rally envelope and attempt state. Only the final adapter differs.
This avoids a direct Claude-to-Codex bridge with a separate queue, identity namespace, retry policy,
and receipt vocabulary.

### Rally → OpenCode

The inspected OpenCode `1.4.3` server binds HTTP, publishes OpenAPI 3.1 at `/doc`, supplies health and
SSE events, supports Basic authentication by configuration, and can advertise through mDNS. A Rally
adapter should pin an explicit endpoint and credentials; mDNS discovery alone is not authenticated
identity.

### Rally → A2A

A2A is the strongest fit for remote peer agents because its model includes discovery, messages,
tasks, parts, artifacts, async work, and multiple bindings. Rally remains the durable coordination
hub: map A2A task/message state into correlated Rally evidence and keep Rally-specific claim and
repo semantics in references or declared extensions.

### Rally → MCP

MCP is the strongest fit when Rally or `ptyd` is exposed as tools to an agent host. MCP stdio gives
simple parent-owned local lifecycle; Streamable HTTP supports independent multi-client servers and
optional server-to-client notifications. MCP does not by itself guarantee that an idle agent is
awake, that a peer accepted work, or that cross-agent history is canonical. The official
2026-07-28 specification release candidate also moves the protocol layer toward stateless requests,
so a Rally adapter must negotiate the active version instead of assuming one session model.

## 11. Recommended architecture and incremental path

```text
one user-facing lifecycle: rally service

  canonical state plane
    rallyd
      inputs: typed store operations
      outputs: ordered facts and projections
      logic: deterministic serialization

  delivery plane
    rally-routerd
      inputs: pending envelopes, endpoint registry, policy, attempts
      outputs: route plan, adapter call, durable evidence
      logic: deterministic policy and retry state machine

  execution endpoints
    Claude native | Codex app-server | OpenCode | A2A | ptyd | tmux/cmux
      inputs: adapter-specific request
      outputs: transport evidence and provider events

  process plane
    ptyd
      inputs: process/PTY RPC
      outputs: live processes, terminal streams, transport receipts
      logic: deterministic runtime and supervision

  judgment plane
    Claude | Codex | Harness | local/cloud LLM
      inputs: goal, observations, tools
      outputs: chosen actions and work results
      logic: model judgment within policy

  enforcement plane
    AI Assistant hooks
      inputs: host lifecycle events
      outputs: allow, warn, block, or coordination mutation
      logic: fast deterministic rules; optional bounded classifier
```

### Smallest capability-first sequence

1. Build a pure `DeliveryPlanner` and adapter contract. Run it in shadow mode.
2. Add `rally route --once` using the existing FileInbox. Test deterministic selection and receipts.
3. Wrap existing `ptyd` and tmux/cmux paths as adapters. Keep the current CLI authoritative until
   equivalence is proven.
4. Make the inline planner authoritative for new sends. Preserve ledger-only operation.
5. Add a resident router worker for post-sender delivery, leases, retries, and restart recovery.
6. Add Codex and Claude native adapters behind capability probes and automatic safe fallback.
7. Add OpenCode and A2A after the first four Claude/Codex combinations pass one contract suite.
8. Expose one `rally service start|status|stop|doctor`; keep internal processes separate.

## 12. Performance and reliability measurement

Do not decide “script versus daemon” by intuition. Measure these paths separately:

| Metric | Why |
| --- | --- |
| Cold start p50/p95 | Cost of one-shot scripts and binary/runtime startup |
| Warm request p50/p95/p99 | Steady-state daemon latency |
| Idle RSS/file descriptors | Permanent laptop cost |
| Requests/deliveries per second | Concurrency and serialization limit |
| Queue age and retry count | Whether reliability creates unacceptable delay |
| Restart recovery time | User-visible outage after crash/reboot |
| Duplicate and unknown outcome count | Delivery correctness, not only availability |
| CPU wakeups while idle | Battery cost of polling watchers |
| Payload/frame rejection count | Compatibility and abuse signal |
| Adapter fallback rate | Native-path health and schema drift |
| End-to-end ACK/completion latency | Actual coordination outcome |

Compare at least:

- direct one-shot command;
- one-shot command through shell/runtime wrappers;
- warm Unix-socket daemon request;
- local HTTP server;
- persistent WebSocket/SSE event path;
- file watcher at idle and under burst;
- native adapter and PTY fallback;
- local model and cloud model calls.

## 13. Verification matrix

| Test family | Required cases |
| --- | --- |
| Lifecycle | Start, duplicate start, adopt, ready, status, graceful stop, hard kill, restart, reboot/login activation |
| Identity | Exact ID, alias ambiguity, stale endpoint, wrong repo, wrong user, revoked registration |
| Compatibility | Old reader/new writer, new reader/old writer, unknown optional field, breaking version, capability downgrade |
| Delivery | Queue, send, seen, accept, progress, complete, verify; never skip semantic rungs |
| Crash injection | Kill before action, after action, before receipt, after receipt, before cursor, after cursor |
| Idempotency | Same key concurrent, retry after timeout, fallback after unknown result, duplicate consumer cutover |
| Backpressure | Slow reader, full queue, burst writes, dropped/coalesced stream, bounded memory |
| Security | Socket permissions, peer UID, token rejection, Origin/CORS, path traversal, payload limits, secret redaction |
| Routing | Exact pin, preferred healthy route, unavailable native route, safe fallback, no compatible route |
| Cross-agent | Claude→Claude, Claude→Codex, Codex→Claude, Codex→Codex, OpenCode, A2A, offline pull |
| Semantics | Provider send cannot forge target ACK; task completion cannot forge independent verification |
| Observability | Every attempt has trigger, policy, endpoint, version, digest, timing, outcome, and correlation |

## 14. Common architecture mistakes

1. Calling every automated process a daemon.
2. Assuming shell backgrounding provides restart, readiness, or log management.
3. Assuming JSON encoding makes two protocols compatible.
4. Treating a PID or socket file as proof of readiness.
5. Letting two consumers execute the same inbox during migration.
6. Retrying non-idempotent PTY input after an unknown outcome.
7. Treating transport delivery as agent acceptance or work completion.
8. Giving the canonical store process provider credentials and PTY authority.
9. Adding an LLM where a deterministic state machine is sufficient.
10. Using display names as authenticated agent identities.
11. Letting a parent kill a healthy external service it merely reused.
12. Adding a permanent daemon when a host hook or scheduled one-shot job already supplies the trigger.
13. Polling independently from every UI consumer instead of publishing one shared snapshot.
14. Using an unauthenticated discovery mechanism as authorization.
15. Hiding permanent failures behind infinite restart loops.

## 15. Academic research implications

The research supports the recommended logical boundaries, but it challenges one implementation
assumption: a logical router does **not** need to begin as a mandatory standalone daemon. The
strongest incremental design is a pure routing and attempt-state core that can run inline, as a
one-shot worker, or inside a resident process.

| Research finding | Evidence status | Rally implication |
| --- | --- | --- |
| Recovery semantics can be separated from execution | Peer-reviewed: ExoFlow, OSDI 2023 | Keep canonical state and recovery independent from provider execution |
| Orchestration can run as a library over a strong durable substrate | Peer-reviewed counter-evidence: Unum, NSDI 2023 | Build `DeliveryPlanner` and `route --once` before requiring `rally-routerd` |
| Agent-specific interfaces materially affect coding results | Peer-reviewed: SWE-agent, NeurIPS 2024 | Optimize the host-facing Rally interface, not only the transport |
| Concurrent coding agents need dependency-aware delegation, isolation, structured communication, integration, and tests | COLM 2026 accepted: CAID v2 | Combine claims and intent with worktrees/branches when work is sufficiently independent |
| MCP is lighter for constrained coordination; A2A carries richer task lifecycle at higher complexity | July 2026 preprint experience report | Use both as adapters; keep task truth in Rally |
| No one protocol is likely to maximize versatility, efficiency, and portability | June 2026 preprint taxonomy | Preserve a layered adapter architecture rather than one universal wire path |
| Current protocols transport but do not enforce full governance semantics | June 2026 preprint gap analysis | Keep decisions, dissent, escalation, and replay above transport protocols |
| Tool results and protocol composition are security boundaries | Peer-reviewed AgentDojo and AgentFuzz; AgentRFC preprint | Treat payloads as untrusted data; test the composed route, not only each adapter |
| Durable agent state can resume against changed prompts, tools, models, or policies | August 2026 preprint | Record an execution manifest and revalidate bindings on long-lived resumes |
| Multi-agent debate and judging have conditional gains and can degrade outcomes | 2025 systematic preprint | Use executable checks first and model auditors selectively; never place a judge in the router |
| Local model placement trades latency/privacy against memory, energy, and quality | Peer-reviewed CLONE, USENIX ATC 2025 | Keep placement configurable and benchmark local, hybrid, and cloud paths |

### Research-adjusted architecture decision

Use **one logical coordination and routing system with three possible process hosts**:

1. The CLI executes the planner inline for immediate, unambiguous delivery.
2. A one-shot worker drains bounded pending work when a hook or scheduler already supplies the
   trigger.
3. A resident router worker handles retries, wakeups, warm native connections, and work that must
   continue after the sender exits.

All three hosts must call the same pure planner and attempt state machine. This structure retains a
single semantic implementation without forcing a permanent process where the workload does not
need one.

### Research-adjusted verification

Add these cases to the matrix in Section 13:

- **Host equivalence:** the same envelope produces the same route plan and attempt transition when
  run inline, one-shot, or resident.
- **Semantic drift:** resume after an adapter, capability, policy, prompt, tool, or model version
  changes; require explicit revalidation or an observable warning.
- **Composition safety:** property-test every Rally/native/A2A/MCP round trip, including unknown
  extensions, capability downgrade, identity scope, and error translation.
- **Adversarial payload:** place instruction-like content inside messages, tool output, artifacts,
  and receipts; deterministic infrastructure must continue treating it as data.
- **Judge ablation:** compare no judge, deterministic verifier, same-model judge, and independent
  model judge against executable ground truth, cost, latency, and false-confidence rates.

The full 15-source evidence package, corroboration assessment, counter-evidence, and research gaps
are stored in the central research library as
`agent-systems.background-process-agent-runtime-architecture`.

## 16. Authoritative references

### Local source

- Agent Rally Point: `crates/rallyd/src/main.rs`, `crates/rally-cli/src/rallyd_core.rs`,
  `crates/rally-protocol/src/store_wire.rs`, `crates/rally-protocol/src/ledger.rs`,
  `crates/cockpitd/src/protocol.rs`, `docs/DAEMON-AND-TRANSPORT-ARCHITECTURE.md`,
  `docs/ROUTER-ARCHITECTURE.md`, and `docs/ROUTER-DEPENDENCY-MAP.md`.
- Easy Terminal: `daemon/ptyd/src/main.rs`, `protocol.rs`, `termd.rs`,
  `terminal_rally_point_main.rs`, `src/bin/ptyd-mcp.rs`, `Sources/Ptyd/DaemonController.swift`, and
  `Sources/Ptyd/DaemonSnapshotStore.swift`.
- Agent Harness: `crates/cli/src/serve.rs`, `crates/engine/src/lib.rs`,
  `crates/provider/src/llama_server_process.rs`, and `tools/launchd/`.
- AI Assistant: `hooks/hooks.json`, `scripts/route_guard.py`, `scripts/self_improve.py`, and
  `docs/hooks.md`.

### External primary specifications

- Apple, [Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html): on-demand jobs, user agents, socket ownership, and shutdown behavior.
- systemd, [systemd-socket-activate](https://www.freedesktop.org/software/systemd/man/systemd-socket-activate.html): socket activation and file-descriptor handoff.
- JSON-RPC Working Group, [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification): request, response, notification, correlation, and transport independence.
- Model Context Protocol, [Transports, 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports): stdio and Streamable HTTP requirements and security boundaries.
- Model Context Protocol, [2026-07-28 specification release candidate](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/): proposed stateless request semantics and migration context; this is a release candidate, not the pinned released transport contract above.
- A2A Project, [Agent2Agent Protocol Specification](https://a2a-protocol.org/latest/specification): latest released `1.0.0` at inspection, with AgentCard, Message, Task, Part, Artifact, discovery, and JSON-RPC/gRPC/REST bindings; its protobuf model is normative.
- OpenCode, [Server documentation](https://dev.opencode.ai/docs/server/): OpenAPI HTTP server, health, SSE events, authentication, and discovery.
- Anthropic, [Claude Code CLI reference](https://docs.anthropic.com/en/docs/claude-code/cli-usage): process/session invocation surface. Installed `2.1.226` help was also inspected because the public page may lag the local background-agent surface.

### Academic and research sources

- Zhuang et al., [ExoFlow: A Universal Workflow System for Exactly-Once DAGs](https://www.usenix.org/conference/osdi23/presentation/zhuang), OSDI 2023.
- Liu et al., [Doing More with Less: Orchestrating Serverless Applications without an Orchestrator](https://www.usenix.org/conference/nsdi23/presentation/liu-david), NSDI 2023.
- Yang et al., [SWE-agent: Agent-Computer Interfaces Enable Automated Software Engineering](https://proceedings.neurips.cc/paper_files/paper/2024/hash/5a7c947568c1b1328ccc5230172e1e7c-Abstract-Conference.html), NeurIPS 2024.
- Debenedetti et al., [AgentDojo](https://proceedings.neurips.cc/paper_files/paper/2024/hash/97091a5177d8dc64b1da8bf3e1f6fb54-Abstract-Datasets_and_Benchmarks_Track.html), NeurIPS 2024.
- Liu et al., [Make Agent Defeat Agent](https://www.usenix.org/conference/usenixsecurity25/presentation/liu-fengyu), USENIX Security 2025.
- Tian et al., [CLONE: Collaborative Learning on the Edge](https://www.usenix.org/conference/atc25/presentation/tian), USENIX ATC 2025.
- Mei et al., [AIOS: LLM Agent Operating System](https://openreview.net/pdf?id=L4HHkCDz2x), COLM 2025.
- Geng and Neubig, [Effective Strategies for Asynchronous Software Engineering Agents](https://arxiv.org/abs/2603.21489), COLM 2026 accepted, v2 revised 2026-07-08.
- Predoaia et al., [A Comparative Study of MCP and A2A](https://arxiv.org/abs/2607.23884), preprint, 2026-07-26.
- Sander et al., [A Technical Taxonomy of LLM Agent Communication Protocols](https://arxiv.org/abs/2606.19135), preprint, 2026-06-17.
- Zhang et al., [Governance Gaps in Agent Interoperability Protocols](https://arxiv.org/abs/2606.31498), preprint, 2026-06-30.
- Zheng and Zhang, [AgentRFC](https://arxiv.org/abs/2603.23801), preprint, 2026-03-25.
- Mozafari, [BEGIN AI TRANSACTION](https://arxiv.org/abs/2608.05412), preprint, 2026-08-05.
- [Revisiting Multi-Agent Debate](https://arxiv.org/abs/2505.22960), preprint, 2025-05-29.
- OpenID Foundation, [Identity Management for Agentic AI](https://arxiv.org/abs/2510.25819), standards whitepaper, 2025-10-29.

## 17. Decision checklist

Before approving a new background component, require answers to all of these:

- What user outcome requires the process?
- Why can an existing hook, scheduler, service, or one-shot worker not perform it?
- What is the smallest runtime role?
- Who starts, owns, updates, and stops it?
- Where does it live and which users/repos/organizations can it see?
- What exact event triggers an action?
- What input and output schema crosses the boundary?
- Which data is canonical, projected, cached, or merely transport evidence?
- How are identity, authorization, correlation, ordering, and idempotency established?
- What happens at every crash boundary?
- What is the terminal failure state?
- What does the component do deterministically?
- Does it call a model, and if so, why is model judgment required?
- Which actions require human approval?
- Which compatibility versions and fallbacks are supported?
- What latency, memory, battery, throughput, and recovery targets must tests prove?
- How will a user inspect health and understand why an action occurred?

If those answers are unclear, the process boundary is not ready to implement.
