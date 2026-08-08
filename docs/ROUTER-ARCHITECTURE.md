<!-- markdownlint-disable MD013 -->
<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Rally Router Architecture

> **Status:** proposed architecture and incremental build contract.
> **Source baseline:** `main` at `8625be8` plus the working tree observed on 2026-08-08.
> **Position:** Rally remains the canonical, host-neutral coordination plane. External protocols
> are codecs at the edge. The router is deterministic infrastructure, not an agent, scheduler,
> judge, or source of truth.
> **Dependency map:** [`ROUTER-DEPENDENCY-MAP.md`](ROUTER-DEPENDENCY-MAP.md) maps current source,
> reusable components, proposed changes, and the regression blast radius.

## Decision

Add a restartable Rally delivery router as a separate client of the canonical Rally store. The
router owns delivery after the sender has recorded a canonical message. It resolves a stable Rally
recipient to live endpoints, chooses one route, records every attempt, and waits for evidence.

This is a **delivery router**. Current source also calls `RoomStore::route` a router, but that code
only chooses direct store access versus `rallyd` socket access. It never selects an agent endpoint
or transport. Use `DeliveryRouter`/`rally-routerd` in code and documentation to prevent the two
meanings from collapsing.

Keep three responsibilities separate:

| Component | Owns | Does not own |
| --- | --- | --- |
| `rallyd` | Per-repo ledger/cache serialization, projections, canonical coordination facts, and later atomic delivery-state facts | Network delivery, PTYs, route choice, LLM judgment |
| Rally router | Endpoint registry, deterministic route planning, attempt leases, delivery attempts, retry/fallback, monitoring | Canonical state, terminal processes, work assignment, correctness judgment |
| `ptyd` | Terminal/process lifecycle, Rally-id-to-pane binding, stdin, capture, process liveness, low-level PTY framing | Rally ledger, cross-adapter route choice, target ACK semantics |

The product presents one service surface even when these are separate processes:

```text
rally service start
rally service status
rally doctor
```

`rally service start` should start the per-repo store and router. It should attach to or lazily
start `ptyd` only when a selected endpoint requires terminal ownership.

## Current behavior

Current Rally has two daemon-related paths, but neither is the proposed router.

### `rallyd`

`rallyd` is an optional per-repo single-writer store daemon. It owns the warm SQLite projection and
serializes `RoomStore` operations over `.rally/rallyd.sock`. Its current charter explicitly excludes
external commands, network calls, scheduling, and agent execution. When `rallyd` is absent, each
CLI opens the store directly under the ownership lock.

### `ptyd`

`ptyd` is an external terminal and process runtime. Rally talks to it through line-delimited
JSON-RPC. It can create and register agent panes, send input, capture output, report live pane IDs,
and stop processes. Rally keeps no direct crate dependency on `ptyd`; it mirrors the wire contract.

### Delivery today

```text
sender
  -> short-lived rally CLI process
  -> append Directive / coordination fact
  -> resolve a ManagedSession in that same CLI process
  -> call ptyd agent.send OR inject through tmux/cmux
  -> optional sender-observed transport receipt
  -> wait for a target-authored Rally fact when --require-ack is used
```

The sender process owns route selection and transport execution. If it exits, no resident process
started or supervised by Rally continues delivery. A separate `rally-termd`/`terminal-rally-point`
binary in the sibling `ptyd` project can consume Rally's existing `FileInbox`, but Rally does not
start it, monitor it, or use it to choose among transports. The installed `rally-termd` binary was
not running during the 2026-08-08 assessment. `rallyd` does not consume pending messages or select
transports. `ptyd` can deliver to a pane it owns, but it cannot choose among Claude Channels, Codex
app-server, OpenCode, A2A, and other Rally endpoints.

## Target behavior

```text
any sender or native ingress
  -> append canonical Rally envelope
  -> durable recipient inbox                       correctness boundary
  -> router leases pending delivery
  -> router resolves exact Rally recipient
  -> router selects one compatible live endpoint
      -> Claude Channel / native messaging
      -> Codex app-server
      -> OpenCode server
      -> A2A endpoint
      -> ptyd
      -> tmux/cmux
      -> no endpoint: remain queued
  -> adapter records transport evidence
  -> receiver obtains canonical envelope
  -> receiver writes ACK / progress / blocker / completion to Rally
```

The inbox is the correctness path. Live transport reduces latency. A router or adapter failure may
delay delivery; it must not lose the message.

Rally already has the first version of this durable inbox in
`rally_protocol::ledger::FileInbox`, with typed `Directive` and `Receipt` records. The initial
router should consume and extend that contract instead of creating a second queue.

### Local and remote payload rules

When the receiver can read the same Rally room, the adapter should send an event ID, content digest,
and concise wake instruction. The receiver pulls the canonical envelope from Rally. This prevents a
transport-specific rendering from becoming a second source of truth.

When the receiver cannot access the Rally room, such as a remote A2A endpoint, the adapter sends the
complete canonical coordination envelope plus its digest. The bridge writes the remote response and
evidence back into Rally.

The canonical envelope covers coordination semantics, not every provider's entire conversation
protocol. Provider fields that Rally does not interpret remain in a versioned opaque extension or a
content-addressed artifact. A codec must refuse a route when it cannot preserve a required field; it
must never silently discard one.

## Router contract

The router is a small deterministic Rust worker. It has no model, prompt, autonomous planning, or
work-assignment policy. It can be rebuilt entirely from Rally facts after a crash.

### Inputs

- Pending canonical delivery envelopes.
- Stable Rally recipient identity.
- Endpoint registrations and capability descriptors.
- Observed endpoint health and expiry.
- Delivery policy resolved from default, user, repository, and message scope.
- Prior attempts, receipts, ACKs, and completion facts.

### Outputs

- A durable route plan naming the selected endpoint and reason.
- A leased delivery attempt with an idempotency key.
- A transport result that does not overstate its evidence.
- A retry, fallback, or queued decision.
- Health facts for stale endpoints, adapter degradation, and the router itself.

### Endpoint registration

Each addressable session advertises a runtime descriptor above the codec layer:

```text
agent_id             stable Rally identity
principal_id         optional verified human/service identity
session_id           one execution session
repo_id              exact Rally room scope
host_id              machine/runtime boundary
endpoint_id          adapter-specific address, never the Rally identity
adapter              claude-channel | codex-app-server | opencode | a2a | ptyd | tmux | cmux
capabilities         deliver, interrupt, capture, stop, positive_ack, completion_events
last_verified_at     last observation by the adapter or host integration
inactive_after       adaptive expiry, not proof of abandonment
build_id             Rally/adapter protocol compatibility
auth_scope           local same-user or later authenticated scope
```

Endpoint IDs remain runtime data. They must not be copied into durable facts as the identity of the
agent. Ambiguous resolution fails closed; the router never guesses a pane, process, or session.

### Route policy

The router applies one policy in one place:

1. Resolve one exact Rally recipient in the repository scope.
2. Remove expired, incompatible, unauthorized, and unverified endpoints.
3. Filter endpoints that cannot preserve the envelope's required capabilities.
4. Prefer a structured native endpoint over terminal injection.
5. Prefer an endpoint capable of positive ACK over one that only proves bytes were sent.
6. Lease the envelope before attempting delivery.
7. Use the envelope event ID plus attempt number as the idempotency key.
8. Record the route, reason, attempt, timing, and exact evidence returned.
9. Retry or select the next allowed endpoint after a typed failure or deadline.
10. Leave the envelope queued when no safe route exists.

Default preference:

```text
structured native endpoint
  > A2A endpoint
  > ptyd-owned process
  > explicitly bound tmux/cmux pane
  > queued inbox / receiver pull
```

Repository or message policy may constrain this order, but an adapter cannot override it.
`ptyd` may choose paste framing versus raw PTY input after the router selects `ptyd`; it must not
choose whether Rally uses `ptyd` instead of a native adapter.

### Evidence state machine

```text
recorded -> leased -> route_selected -> transport_sent -> target_acked -> working -> completed
               |             |                |               |            |
               +---------- retry / fallback / queued / failed with typed evidence ----------+
```

These states are not interchangeable:

| State | What it proves |
| --- | --- |
| `recorded` | Rally durably accepted the envelope. |
| `transport_sent` | The adapter performed its send contract; for a PTY this may mean bytes written. |
| `target_acked` | The target identity wrote a correlated Rally ACK or equivalent verified callback. |
| `working` | The target reported active work or an observable native state was recorded as observation. |
| `completed` | The target reported completion with the required evidence or result. |

Only target-authored or authenticated target callbacks advance `target_acked` and `completed`.
A sender or transport may never manufacture those states.

### Native sends outside Rally

Claude and Codex native communication surfaces are endpoints, not transparent network taps. Rally
cannot guarantee that a direct provider-native message is recorded unless the host exposes a
reliable interception event.

The enforceable path is therefore:

```text
agent calls rally.send
  -> Rally records the envelope
  -> router invokes the best native adapter
```

`rally.send` names the proposed protocol/API operation. The current CLI entry points are
`rally inject` and attributed coordination facts such as `rally say handoff`; the public command
name should be finalized during Phase R0 rather than implied to exist today.

Host integrations may mirror direct native sends when a trustworthy hook exists. Strict mode may
later warn or block unrecorded native messaging. Until that is proven per host, direct native sends
remain explicitly off-ledger rather than being claimed as captured.

## Capability delta

The router update enables behavior current Rally cannot provide:

| Capability | Current Rally | With router |
| --- | --- | --- |
| Delivery after sender exits | Partial external mechanism; `rally-termd` can consume `FileInbox` when separately deployed, but Rally does not own or supervise it | Yes; Rally starts, monitors, and resumes the router from pending envelopes |
| Central cross-provider route choice | No; fixed managed-session backend in sender | Yes; one policy across native, A2A, PTY, and mux endpoints |
| Live endpoint registry | Partial managed-session and host-runtime records | Typed multi-endpoint capabilities, health, version, and expiry |
| Retry and fallback | Limited synchronous fallback inside one command | Durable attempts, deadlines, idempotent retry, ordered fallback |
| Explain why a route was chosen | Limited delivery-path label | Durable route plan and rejection reasons for every candidate |
| Recover after restart | Ledger retains facts, but no worker resumes transport | Router replays pending state and resumes safely |
| Separate send from ACK | Partly implemented and command-specific | One enforced state machine across every adapter |
| Native Claude/Codex/OpenCode/A2A delivery | Not implemented in Rally | First-class adapters behind one contract |
| Monitor adapter degradation | Backend probe at selected commands | Continuous or event-driven endpoint and router health |
| Receive native ingress into Rally | Not provided as a common path | Adapter can record inbound envelope before routing it onward |
| Prevent duplicate multi-path delivery | No cross-adapter lease | One attempt lease and idempotency key per envelope/target |
| Offline correctness | Ledger-only facts require voluntary agent polling | Durable inbox plus mandatory host-boundary pull; routing is acceleration |

The router does not make agent work correct, authorize arbitrary execution, or prove a message was
understood. It makes delivery ownership, route evidence, and failures explicit.

## Alternatives considered

| Configuration | Benefit | Loss | Decision |
| --- | --- | --- | --- |
| One monolithic `rallyd` containing store, router, codecs, and PTYs | Fewest processes and sockets | Canonical store shares privileges and crash domain with unstable/networked transports | Reject |
| Keep sender-side routing and improve labels | Smallest change | Sender exit still abandons delivery; no receive owner | Transitional only |
| Put router logic inside `rallyd` | One fewer process | Violates the current pure-store boundary and couples hooks/store availability to adapters | Prototype only, behind a module seam |
| Pure `rallyd` plus separate router plus optional `ptyd` | Clear state, routing, and runtime boundaries | One more supervised internal process | Recommended |
| One process per codec | Strong isolation and independent upgrades | Excessive installation and observability burden for current users | Later, for risky/remote codecs |
| Remove `ptyd` and use only native APIs | Simpler structured fleet | Loses generic CLI lifecycle, capture, and fallback | Supported configuration, not universal default |

`cockpitd` is not the router. It is a separate app-facing session supervisor with its own store,
network surface, and approval model. Reuse contract/code lessons where appropriate; do not merge the
processes or state.

## Incremental build and dogfood plan

Each phase keeps the current path available and produces measurable evidence before the next cutover.

### Phase R0: Freeze the contract

- Extend and version the existing `Directive`/`Receipt` contract; do not add a second delivery queue.
- Move or merge the staged `EventEnvelope` identity, correlation, and idempotency fields into the
  shared protocol contract.
- Version the endpoint descriptor, route plan, and delivery attempt.
- Specify required-field preservation and opaque extensions.
- Add adapter conformance fixtures for `ptyd`, Claude, Codex, and A2A vocabularies.
- Add state-transition tests that reject sender-authored ACK/completion.

Gate: schemas and round-trip fixtures pass; no runtime behavior changes.

### Phase R1: Extract a pure route planner in shadow mode

- Move target and backend selection behind one pure `DeliveryRouter` interface.
- Keep the existing CLI transport path active.
- Run the new planner in shadow mode and record when its route differs from current behavior.
- Rename or clearly scope `daemon_client` as the `ptyd` client to remove daemon ambiguity.

Gate: existing delivery tests remain green; shadow selection matches or explains every difference.

### Phase R2: Add durable route leases and attempt history

- Reuse the existing `FileInbox` as the pending queue.
- Record route plan, lease, attempt, result, and next deadline in additive Rally facts or a
  versioned delivery-state side log.
- Let the sender CLI drain the outbox inline at first; no new resident process yet.
- Add crash tests between every state transition.

Gate: killing the sender at any transition loses no envelope and produces no duplicate target ACK.

### Phase R3: Start the resident router

- Add a small router worker as a `rallyd` client.
- Start in observe-only mode, then allow it to lease and deliver one adapter.
- Make the CLI append-only once the resident worker is healthy.
- Preserve receiver pull when the router is absent.

Gate: kill `rally-routerd` during delivery; the envelope remains available, restart resumes once,
and the room reports router degradation.

### Phase R4: Dogfood `ptyd`, then native adapters

1. Use the existing real `ptyd` integration as the first router adapter.
2. Disable the autonomous `rally-termd` consumer for router-owned rooms before enabling router
   delivery, so two consumers cannot deliver the same directive.
3. Add Codex app-server and Claude Channel adapters against generated/current schemas.
4. Add mixed Claude-to-Codex and Codex-to-Claude journeys.
5. Add OpenCode and A2A after the first two native adapters pass the same conformance suite.

Gate: the same Rally envelope completes through each adapter without changing ACK semantics.

### Phase R5: Cut over the public messaging surface

- Make `rally send`/`inject` append first and delegate live delivery to the router.
- Make host integrations pull at turn boundaries.
- Remove duplicate sender-side transport only after activation is proven for Claude and Codex.
- Keep an explicit emergency/direct diagnostic command, not an automatic competing sender.

Gate: no dual-send race; no message loss with router absent; provider-native direct sends are either
captured by a verified host integration or reported as outside Rally.

## Potential regressions and controls

| Regression | Cause | Required control |
| --- | --- | --- |
| Higher send latency | Durable append plus router hop | Measure p50/p95; inline drain until resident router is faster; keep local sockets |
| Router becomes a single point of failure | Correctness depends on push | Inbox and host pull remain authoritative; router failure changes latency only |
| Duplicate delivery | CLI and router both send, or retry races | Exclusive attempt lease, event-id idempotency, explicit cutover flag, mutation tests |
| Out-of-order messages | Multiple routes or concurrent retries | Per-recipient/thread sequence and ordered lease policy |
| Wrong session receives work | Stale or guessed endpoint | Exact Rally identity, fresh observed registration, ambiguity refusal, endpoint echo check |
| Payload or metadata loss | Codec cannot represent a required field | Round-trip conformance, digest verification, opaque extensions, fail instead of downgrade |
| False ACK or completion | Sender/transport overstates evidence | Actor-bound state transitions; separate transport receipt from target ACK |
| Arbitrary ledger text becomes terminal input | Router turns untrusted facts into keystrokes | Authorization before lease, pointer-first wake, sanitization, allowlisted operations, audit fact |
| Credentials or private content leave the machine | Native/A2A route selected unexpectedly | Route policy by scope, explicit remote capability, redaction classification, no remote default |
| Store availability falls with an adapter | Router runs inside `rallyd` | Separate process and address space; `rallyd` stays pure |
| Provider preview change breaks delivery | Claude or other native contract changes | Versioned codec, capability handshake, conformance fixtures, automatic fallback |
| More background-process burden | Separate store, router, and optional PTY runtime | One user-facing service command, lazy `ptyd`, unified doctor/status, idle exit |
| Message loops | Native ingress is re-emitted as a new outbound message | Stable event ID, origin chain, hop limit, dedupe before append/delivery |
| Old clients disagree with new state | Mixed Rally versions | Additive versioned facts, build/protocol registration, warn before incompatible delivery |
| Direct native messages bypass Rally | Provider send surface is not interceptable | Rally-owned send tool, verified mirroring hooks where possible, strict-mode warning/block later |
| Terminal fallback behavior regresses | `ptyd` authority is narrowed | Preserve current real-ptyd tests; compare shadow/current route; cut over adapter by adapter |

## Non-regression invariants

The router does not ship until all of these hold:

1. The canonical Rally envelope is written before any live delivery.
2. A router failure cannot delete or make an envelope unreadable.
3. No adapter can write target-authored ACK or completion as the sender.
4. No message is delivered twice during CLI-to-router cutover.
5. No endpoint is selected by guessed process, pane, or provider identity.
6. A codec that cannot preserve required semantics fails visibly.
7. `rallyd` remains usable when every adapter and `ptyd` are absent.
8. A structured-only user never needs to install or run `ptyd`.
9. A single-user install retains a one-command, zero-configuration path.
10. Existing ledger replay and current direct/routed store parity remain green.

## Split triggers

Start with one router process containing trusted first-party adapter modules. Split a codec into a
separate sidecar only when it accepts non-loopback traffic, stores third-party credentials, loads
untrusted plugins, has a materially different upgrade cadence, or requires independent scaling.

Keep an optional coordinator or judge separate from the router. A coordinator may observe delivery
and work state and may post attributed recommendations. It must not own route leases, canonical
state, or transport credentials. Automated reassignment remains a separate product decision.
