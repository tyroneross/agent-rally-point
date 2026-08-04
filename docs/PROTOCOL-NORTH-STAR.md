<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Rally Agent Coordination Protocol - North Star

## Governing Thought

Rally should be an agent coordination protocol for large fleets of local and cloud
coding agents. The ledger is the durable proof layer inside that protocol, not
the whole product.

The protocol exists so thousands of agents, subagents, terminals, managed panes,
cloud workers, and LLM tools can safely answer:

- Who is alive and reachable?
- Who is working on what?
- Who owns which resource scope?
- What was handed off, acknowledged, accepted, blocked, resolved, or superseded?
- What evidence proves work is real?
- What conflicts with what?
- What should an idle agent do next without a human acting as clipboard or referee?

This serves the Rally north star: coordinate thousands of AI coding agents across
many terminals on one codebase, trustworthily, losslessly, and without a human
referee.

## Design Position

Rally should remain a facilitator, not an executor.

It records, projects, routes, warns, and advises. It does not decide whether code
is correct, execute work on behalf of agents, grant exclusive locks, or become a
hidden scheduler. Host agents and their harnesses execute the work. Rally keeps
their shared coordination state reliable enough that they can work in parallel
without clobbering each other.

The right model is a protocol with four main planes:

| Plane | Purpose | Persistence |
|---|---|---|
| Presence registry | Live sessions, endpoints, heartbeats, capabilities, liveness | Mutable TTL state |
| Coordination ledger | Claims, handoffs, ACKs, blockers, resolves, decisions, artifacts | Append-only durable facts |
| Active claim index | Transactional authority for current resource ownership | Mutable, rebuildable from ledger |
| Artifact index | Commits, branches, builds, screenshots, reports, logs | Durable references, heavy data out of band |

This split is the central scale decision. High-frequency liveness is not durable
ledger traffic. Durable events are reserved for coordination-significant facts.

The active claim index is the write-path authority for current claims. Claim
acquisition must check this index transactionally before an agent begins work.
The append-only ledger remains the durable audit source, and reconciliation must
be able to rebuild or repair the active claim index from ledger facts.

## Identity Model

Rally needs session identity, not just agent identity. Tool labels such as
`claude_code` and `codex` are useful, but they do not prove which terminal,
pane, process, or cloud worker acted.

Use layered identity:

| Identity | Meaning | Required On Events? |
|---|---|---|
| `workspace_id` | A local workspace, org, or machine-level coordination namespace | Usually derived |
| `room_id` / `repo_id` | The repo-local room. Defaults to canonical `repo_root` | Yes |
| `endpoint_id` | Stable-ish addressable place: tmux pane, terminal, process, cloud job | Registry-level |
| `session_id` | Current live lease occupying an endpoint | Yes as `from_session_id` |
| `principal_id` | Human, service, or agent identity behind the session | Required for privileged actions |
| `actor_id` | Logical persona or subagent inside a session | Optional unless multiple actors share one session |
| `run_id` | One task execution span | Required for task work |
| `event_id` | One durable event | Yes |
| `ref_event_id` | Exact event being acknowledged, resolved, superseded, or answered | Required for replies |

### Endpoint And Session

`endpoint_id` should be derived from the best available runtime source:

| Runtime | Endpoint Inputs |
|---|---|
| Terminal.app/iTerm | `TERM_SESSION_ID`, tty, host |
| tmux | server socket, session id, window id, pane id |
| Local process | host, pid, process start time, cwd |
| Managed pane | Rally managed session id plus backend target |
| Cloud worker | provider, job id, container id, instance id |

`session_id` is a fresh live lease on top of that endpoint. If the endpoint
restarts, gets re-used, or changes actor, Rally should issue a new `session_id`
while preserving the endpoint lineage.

Each session registry row should include:

```json
{
  "session_id": "sess_...",
  "endpoint_id": "endpoint_...",
  "tool_type": "claude_code",
  "actor_id": "persona-audit",
  "legible_name": "Claude persona-audit in tmux pane 2",
  "pid": 12345,
  "host": "workstation.local",
  "cwd": "/repo",
  "branch": "main",
  "capabilities": ["swift", "rust", "ui-audit"],
  "last_seen": "2026-06-06T12:00:00Z",
  "expires_at": "2026-06-06T12:02:00Z"
}
```

The human-legible name matters. Operators need to know which visible terminal or
cloud worker a session represents. The stable machine id handles routing; the
legible name handles human trust.

## Brainstem Responsibility

Heartbeats should not be authored by LLMs.

Liveness and transport receipts should be handled by a lightweight brainstem:
host hooks, a local daemon, managed-session backend, cloud sidecar, or connector.
This keeps token use low and keeps the LLM focused on semantic work.

Brainstem-owned state:

- Session registration and renewal.
- `last_seen` heartbeat updates.
- Active, idle, stale, closed, and revoked session transitions.
- Managed-session transport delivery.
- Inbox materialization by `to_session_id`.
- Optional read markers for debugging.

LLM-owned events:

- Work creation, blocker, resolve, checkpoint, failure, cancellation, and supersede.
- Claim acquisition, release, transfer, and explicit abandon requests.
- Handoff ACK, accept, and reject.
- Artifact publication.
- Validation result.
- Decision record.
- Risky operation intent and result.

This split keeps Rally reliable at high scale while avoiding durable telemetry
noise.

## Event Model

Every durable event should be small, typed, idempotent, and replayable.

Required durable event envelope:

```json
{
  "event_id": "evt_...",
  "idempotency_key": "optional-writer-retry-key",
  "kind": "claim.acquired",
  "room_id": "repo:/Users/example/project",
  "from_session_id": "sess_...",
  "created_at": "2026-06-06T12:00:00Z",
  "causation_id": "evt_that_directly_caused_this",
  "correlation_id": "larger-flow-or-user-request",
  "work_id": "work_...",
  "run_id": "run_...",
  "attempt_id": "attempt_...",
  "claim_id": "claim_...",
  "handoff_id": "handoff_...",
  "resource_scope": {
    "type": "file",
    "repo_id": "project",
    "path": "Sources/Workbench/AgentInspector.swift",
    "access": "exclusive"
  },
  "auth_context": {
    "role": "agent",
    "policy_version": "policy_..."
  },
  "subject": "Claim inspector correction pass",
  "summary": "Short human-readable detail",
  "evidence": []
}
```

Targeted messages add:

```json
{
  "to_session_id": "sess_..."
}
```

Replies add:

```json
{
  "ref_event_id": "evt_original",
  "causation_id": "evt_original"
}
```

Events should be designed for agent consumption first. `subject` is short.
`summary` is bounded prose. `evidence` contains command summaries, commit ids,
paths, or artifact ids. Large logs, screenshots, and build outputs live outside
the event body and are referenced by URI or artifact id.

Not every causal field is required on every event. The envelope should support
them consistently, while event kind validation decides which ids are mandatory.
For example, claim events require `claim_id`, handoff events require
`handoff_id`, and retryable work requires `attempt_id`.

## Optimal Durable Event Set

The target set is not minimal. It is the best event vocabulary that preserves
reliability, deconfliction, auditability, and performance.

| Event | Author | Durable? | Purpose |
|---|---|---:|---|
| `session.registered` | Brainstem | Yes | Proves a session joined with identity, endpoint, cwd, tool, and capabilities. |
| `session.closed` | Brainstem | Yes | Clean closeout; retires a session lease. |
| `session.revoked` | Brainstem or operator | Yes | Security or control event. |
| `presence.heartbeat` | Brainstem | No | Mutable TTL update only. |
| `capability.updated` | Brainstem | No by default | Mutable session capability registry update; durable only if policy-relevant. |
| `work.created` | Agent or lead | Yes | Creates a task/run others can reference. |
| `work.checkpoint` | Agent | Yes, rate-limited | Meaningful phase transition or durable progress. |
| `work.blocked` | Agent | Yes | Prevents silent stalls and duplicated work. |
| `work.resolved` | Agent | Yes | Completion receipt tied to evidence. |
| `work.failed` | Agent or validator | Yes | Work was attempted and reached a failure state. |
| `work.cancelled` | Human, lead, or policy agent | Yes | Work should stop by external decision. |
| `work.abandoned` | Agent or lead | Yes | Marks a claim/run unreliable or intentionally dropped. |
| `work.superseded` | Agent or lead | Yes | Prevents stale branches/artifacts from being merged later. |
| `claim.acquired` | Agent through claim authority | Yes | Reserves a resource scope before work. |
| `claim.released` | Agent or authorized lead | Yes | Frees a resource scope. |
| `claim.expired` | Brainstem or claim authority | Yes | Lease expired and the claim left active ownership. |
| `claim.transferred` | Authorized owner or lead | Yes | Moves claim ownership to another session. |
| `handoff.requested` | Agent | Yes | Explicit ask from one session or actor to another. |
| `handoff.delivered` | Brainstem | No by default | Transport/index proof; durable only in strict audit mode. |
| `handoff.acked` | LLM | Yes | Target confirms it saw the handoff. Folds normal `read` into ACK. |
| `handoff.accepted` | LLM | Yes | Target accepts responsibility. Distinct from ACK. |
| `handoff.rejected` | LLM | Yes | Target refuses, is wrong target, or cannot act. |
| `artifact.published` | Agent | Yes | Branch, commit, patch, report, screenshot, build output, or bundle exists. |
| `validation.result` | Agent or validator | Yes | Test/build/audit result tied to an artifact or resolve. |
| `decision.recorded` | Agent or lead | Yes | Coordination, product, or technical decision. |
| `conflict.detected` | Rally projection or agent | Yes | Resource scopes collide or histories disagree. |
| `conflict.resolved` | Agent or lead | Yes | Records winning branch, owner, or resolution decision. |
| `operation.intent` | Agent | Yes for risky ops | Merge, push, deploy, delete, prune, migration. |
| `operation.result` | Agent or brainstem | Yes | Records success, failure, denial, rollback, or cancellation with evidence. |

### Why `read` Is Not First-Class

For normal LLM workflow, `read` should be folded into `handoff.acked`.
Transport systems may keep `read_at` in an inbox index for debugging, but a
durable `read` event has poor signal unless the actor explicitly acknowledges
or accepts the work.

The important distinctions are:

```text
delivered != acked
acked != accepted
accepted != resolved
resolved != validated
validated != pushed/deployed
```

These distinctions prevent the common failure mode where a message reached a
pane, but the intended agent never processed it.

## State Machines

Rally should project three separate state machines from facts and registry data.
Do not collapse them.

### Session State

| State | Source | Meaning |
|---|---|---|
| `registered` | Durable event | Session joined the room. |
| `active` | Registry | Session heartbeat is fresh and currently doing work. |
| `idle` | Registry | Session heartbeat is fresh but no current work is active. |
| `stale` | Registry projection | Heartbeat TTL expired. |
| `closed` | Durable event | Session exited cleanly. |
| `revoked` | Durable event | Session identity should no longer be trusted. |

### Message State

| State | Source | Meaning |
|---|---|---|
| `posted` | Ledger | Handoff exists. |
| `delivered` | Brainstem event or delivery index | Transport reached target session or endpoint. |
| `acked` | LLM event | Target actor confirms it saw the request. |
| `accepted` | LLM event | Target actor owns the requested work. |
| `rejected` | LLM event | Target actor declines or cannot act. |
| `resolved` | LLM event | Target actor completed the request. |
| `failed` | Brainstem or agent event | Delivery or action failed. |
| `superseded` | Agent or lead event | A later event replaces the request. |

### Work State

| State | Source | Meaning |
|---|---|---|
| `created` | Ledger | Work item exists. |
| `in_progress` | Derived | Work has an active owner, accepted handoff, or active claim. |
| `blocked` | Ledger | Progress requires external input or environment change. |
| `needs_review` | Ledger or validation policy | Artifact exists and awaits review. |
| `complete` | Ledger | Resolve exists with evidence. |
| `failed` | Ledger | Work was attempted and failed. |
| `cancelled` | Ledger | Human/system decided the work should stop. |
| `abandoned` | Ledger | Owner dropped or timed out the work. |
| `superseded` | Ledger | Different work replaces it. |

The same word should not mean different things across these machines. A stale
session does not automatically mean work is abandoned. It means the claim lease
may expire or become eligible for authorized release.

### Claim State

| State | Source | Meaning |
|---|---|---|
| `requested` | Claim authority | Agent is asking to reserve a scope. |
| `acquired` | Transaction plus ledger | Scope is actively reserved. |
| `renewed` | Mutable claim index | Lease was extended without ledger spam. |
| `released` | Ledger | Owner or authorized lead freed the claim. |
| `expired` | Ledger | Lease expired and claim left active ownership. |
| `transferred` | Ledger | Ownership moved to another session. |
| `conflicted` | Claim authority | Requested claim conflicts with active ownership. |

## Resource Scopes And Deconfliction

Resource scopes are load-bearing. Without them, Rally can record status but
cannot reliably deconflict work.

Display strings are acceptable for logs and CLI output, but the claim authority
must use structured canonical scopes.

Canonical scope example:

```json
{
  "type": "file",
  "workspace_id": "ws_local",
  "repo_id": "easy-terminal",
  "path": "Sources/Workbench/AgentInspector.swift",
  "access": "exclusive"
}
```

Common scope types:

```text
workspace:<workspace-id>
repo:<repo-id>
file:<path>
dir:<path>
branch:<branch-name>
commit:<sha>
port:<number>
process:<name-or-pid>
service:<name>
task:<task-id>
run:<run-id>
cross-repo:<repo-a>+<repo-b>
```

Access modes:

| Mode | Meaning |
|---|---|
| `exclusive` | Agent intends to mutate; conflicts with other exclusive claims. |
| `shared_read` | Agent is inspecting or validating; generally non-blocking. |
| `advisory` | Soft reservation; warning only. |
| `namespace` | Broad claim over a directory, repo, service, or task subtree. |

Claims should be as narrow as possible and as broad as necessary. A broad claim
such as `repo:easy-terminal` should be rare and time-bounded. Most claims should
target files, directories, branches, ports, processes, or task ids.

**Containment is decided by identifier, not by type (RC-037).** A namespace root
(`workspace:`, `repo:`) contains a finer scope only when its identifier answers
the question: the explicit wildcard `*`, or a path the finer scope sits beneath.
An opaque root such as `workspace:zzz` says nothing about whether `src/lib.rs`
lives inside it, so it contains nothing but itself. Treating an unknowable
containment relation as a conflict is what let one coarse claim reject every
other claim in the room, permanently.

**Room-wide breadth is explicit and authority-gated.** `workspace:*` and `repo:*`
mean "everything in this room", and only the lead may hold one. Anyone else is
refused at append time with the reason and the alternative. The gate gives the
lead a real capability and denies an unauthorized agent a room-wide lock; it does
not authenticate the lead seat itself — see
[`docs/security/TRUST-MODEL.md`](security/TRUST-MODEL.md).

Claim acquisition should be deterministic:

1. Canonicalize requested scope.
2. Check active claim index transactionally.
3. Apply the conflict policy.
4. Acquire, reject, queue, warn, or request handoff.
5. Append durable `claim.acquired` or conflict event for coordination-significant
   outcomes.

Minimum invariant:

> At most one active `exclusive` claim may exist for the same canonical resource
> scope or a conflicting parent/child scope.

Conflict examples:

| Existing claim | New claim | Result |
|---|---|---|
| `file:a.swift`, `exclusive` | `file:a.swift`, `exclusive` | Reject or queue. |
| `dir:Sources`, `namespace` + `exclusive` | `file:Sources/a.swift`, `exclusive` | Conflict. |
| `file:a.swift`, `shared_read` | `file:a.swift`, `exclusive` | Allow or warn, policy-dependent. |
| `repo:easy-terminal`, `advisory` | `file:a.swift`, `exclusive` | Allow with warning. |
| `workspace:zzz`, `namespace` | `file:a.swift`, `exclusive` | Allow — an opaque root does not contain a path it never names. |
| `workspace:*`, `namespace` (lead) | `file:a.swift`, `exclusive` | Conflict — the wildcard is deliberately room-wide. |
| `workspace:*` requested by a non-lead | — | Reject at append: only the lead may hold a room-wide claim. |

Conflict policy should be explicit:

```json
{
  "on_conflict": "reject | queue | allow_with_warning | request_handoff"
}
```

`queue` must not turn Rally into a scheduler. It records the pending claim
request; the host or agent harness decides when to retry.

Conflicts include:

- Same exact resource claimed by multiple active sessions.
- Parent/child scope overlap, such as `dir:Sources/Workbench` and
  `file:Sources/Workbench/AgentInspector.swift`.
- Branch or worktree collisions.
- Port or process collisions.
- Cross-repo task overlap where one run owns linked repos.

Rally should enforce claim acquisition policy at the claim authority boundary.
After that, it should warn and record; it should not execute work, edit files, or
act as a hidden scheduler. Hosts may choose to treat rejected claims as hard
blocks before editing.

## Claim Leases

Every active claim should have a lease.

```json
{
  "kind": "claim.acquired",
  "claim_id": "claim_123",
  "lease_expires_at": "2026-06-06T20:15:00Z"
}
```

Lease renewal updates the mutable active claim index only. It does not append a
durable event. If the session disappears or lease renewal stops, the claim
authority emits `claim.expired` once the lease is observed expired.

| State Change | Storage |
|---|---|
| Claim acquired | Durable event plus active claim index |
| Lease renewed | Mutable active claim index |
| Claim expired | Durable system event |
| Claim released | Durable event |

This prevents dead sessions from holding resources indefinitely without turning
heartbeat renewal into ledger spam.

## Authorization

Session identity, actor identity, and authority are separate.

| Identity | Meaning |
|---|---|
| `principal_id` | Human, service account, or agent identity. |
| `session_id` | Live runtime lease. |
| `actor_id` | Persona or subagent speaking inside the session. |
| `tool_type` | Host family such as `codex`, `claude_code`, `cursor`, or `ci`. |
| `auth_context` | Role, policy version, and optional capability grants. |

Authorization rules should cover:

- Who can acquire a claim.
- Who can release or transfer someone else's claim.
- Who can supersede or cancel work.
- Who can publish validation results.
- Who can perform risky operations.
- Whether a room requires signed events or content-addressed artifacts.

Minimum roles:

```text
owner
maintainer
lead_agent
agent
observer
system
```

Local trusted rooms can begin with advisory authorization checks. Multi-user,
untrusted, or federated rooms should require signed session registration and
policy-checked writes.

## Rooms, Threads, And Cross-Repo Work

The default room identity should be the canonical repo root. Do not include dates
in room identity. Dates belong in engagement segments, run labels, and event
timestamps.

For scale, use hierarchical addressing:

```text
workspace_id
  -> repo_id / room_id
    -> run_id
      -> thread_id / ref_event_id
        -> resource_scope
```

One repo remains one Rally point by default. Cross-repo work should create a
coordination room or run that references multiple repo rooms rather than
co-mingling their canonical facts. The global index should remain a pointer and
discovery surface, not a second source of truth.

## Performance Policy

The protocol should optimize for high reliability with bounded write volume.

| Data | Storage Policy |
|---|---|
| Heartbeat and current liveness | Mutable TTL registry |
| Current capabilities | Mutable registry |
| Inbox by target session | Materialized index |
| Resource ownership | Transactional active claim index, rebuildable from ledger |
| Coordination facts | Append-only ledger |
| Heavy evidence | Artifact references |

Default cadence:

| Activity | Cadence |
|---|---|
| Active heartbeat | 30 to 60 seconds, brainstem-owned |
| Idle heartbeat | 120 to 300 seconds, brainstem-owned |
| Stale threshold | 2 to 3 missed active heartbeats, configurable |
| ACK direct handoff | As soon as the LLM processes it |
| Accept/reject | Before beginning or declining work |
| Checkpoint | Phase boundary or every 5 to 10 minutes for long work |
| Blocker | Immediately after a real blocker is established |
| Artifact | Once per meaningful output |
| Resolve | Once per completed request, with evidence |

Hard guardrails:

- Do not append heartbeats to the durable ledger.
- Do not append lease renewals to the durable ledger.
- Do not post command-by-command progress.
- Keep normal event payloads small, generally 1 to 4 KB.
- Store logs, screenshots, and long command output as artifacts.
- Deduplicate retries using `event_id` and `idempotency_key`.
- Partition and subscribe by room, run, target session, and resource scope.

## Reliability Semantics

Rally should be explicit about what is proven.

| Claim | Proof Required |
|---|---|
| Message exists | `handoff.requested` event is in the ledger. |
| Message reached a terminal | Delivery index names exact `to_session_id` or endpoint; durable only in strict audit mode. |
| Actor saw the message | `handoff.acked` from that actor/session. |
| Actor owns the work | `handoff.accepted` or `claim.acquired` with matching scope. |
| Work is done | `work.resolved` references the original request and evidence. |
| Work is verified | `validation.result` references the artifact or resolve. |
| Operation outcome is known | `operation.result` references merge, push, deploy, rollback, or failure evidence. |

This is the standard that would have prevented the "which Claude received it?"
confusion. Transport to a tmux pane is not semantic acknowledgement by the
interactive Claude. A copied chat transcript is not identity proof. A Rally ACK
from a specific `from_session_id` against a specific `ref_event_id` is proof.

## Rationale

### Why Protocol, Not Just Ledger

The ledger records durable facts, but coordination also needs live reachability,
routing, conflict projections, indexes, and artifact references. Calling Rally
only a ledger hides the parts required to make many agents work together
seamlessly.

### Why Registry Plus Ledger

Durable append-only facts are excellent for audit and replay. They are poor for
high-frequency liveness. A 60-second heartbeat from 10,000 sessions produces
14.4 million records per day. That harms performance and buries useful facts.

Mutable TTL presence lets Rally scale while preserving durable proof for
coordination-significant events.

### Why Transactional Claim Authority

Ledger replay can prove what happened. It is not enough to prevent two live
agents from acquiring the same exclusive resource at the same time. Claim
acquisition needs a transactional active index so Rally can answer before work
begins.

The ledger still remains canonical for durable audit and rebuild. The active
claim index is authoritative for current ownership only because it can be
reconstructed and reconciled from append-only claim events.

### Why Session Identity

Tool identity is not enough. `claude_code` can mean an interactive terminal, a
managed tmux pane, a persona-audit worker, or a cloud Claude worker. Session
identity lets Rally target and verify the exact runtime that acted.

### Why Actor Identity Is Optional

Most sessions have one primary actor. Requiring actor identity everywhere adds
noise. But sessions that host multiple personas or subagents need `actor_id` to
distinguish who accepted or resolved work.

### Why Read Folds Into ACK

Read receipts are useful for transport debugging but weak as coordination facts.
Agents need to know whether the actor acknowledged or accepted the request, not
whether some UI surface displayed it.

### Why Rally Does Not Enforce Locks

Hard locks can strand work when sessions die, hosts crash, or agents make wrong
claims. Rally should reject or mark conflicting claim acquisition according to
policy, then surface conflicts and stale ownership loudly. Hosts, leads, or
policies decide whether a rejected or conflicted claim stops editing. This keeps
Rally on-charter as a facilitator while still giving agents a reliable
write-path coordination primitive.

## Tradeoffs

| Choice | Benefit | Cost |
|---|---|---|
| Mutable presence registry | Scales liveness without ledger bloat | Registry is not a complete audit trail |
| Append-only coordination ledger | Replayable, mergeable, auditable facts | Needs projections and compaction for speed |
| Transactional active claim index | Prevents low-latency resource collisions | Adds reconciliation and policy complexity |
| Claim leases | Dead sessions cannot hold resources forever | Requires clock/TTL policy and expiry handling |
| Claim/work split | Models multi-resource work cleanly | More event kinds and ids |
| Structured scopes | Deterministic conflict checks | More schema validation than display strings |
| Session identity separate from tool id | Solves "which Claude?" and managed delivery | More identity plumbing |
| Endpoint identity separate from session id | Handles reconnects and terminal reuse | Requires runtime-specific derivation |
| Actor id optional | Keeps common path simple | Multi-actor sessions must opt into precision |
| Read folded into ACK | Avoids low-signal event noise | Debugging may need non-durable read indexes |
| Warnings over hard locks | Avoids deadlock and stranded claims | Hosts may still collide if they ignore warnings |
| Crypto deferred | Fast local implementation | Not sufficient for untrusted or federated networks |
| Cross-room HLC deferred | Simpler local ordering | Federated cross-repo ordering remains a known gap |

## Deferred Capabilities And Triggers

These are known gaps, not omissions.

| Capability | Defer Until |
|---|---|
| Session key signing | Multi-user, untrusted, or network-federated Rally rooms. |
| Challenge-response registration | Agents connect over untrusted transport or remote service. |
| Hybrid logical clocks | Cross-machine ordering matters beyond per-room sequence. |
| Federated transport | Multiple machines need real-time coordination without shared files. |
| Policy enforcement locks | Hosts demand hard prevention instead of warning-only deconfliction. |
| Server-backed indexes | Repo-local files cannot handle target fleet size or remote topology. |
| Strict durable transport audit | Delivery/read receipts become compliance evidence, not debugging state. |

The local-first design should name these triggers but not pay their complexity
before they are necessary.

## Builder Implications

A build plan should implement this in layers:

1. Add or strengthen session registry identity: `endpoint_id`, `session_id`,
   `legible_name`, TTL, capabilities, cwd, branch, pid, and host.
2. Add structured resource scopes, access modes, and canonicalization.
3. Split `work.*` and `claim.*` events, with `work_id`, `claim_id`, and
   `idempotency_key`.
4. Add transactional active claim acquisition with deterministic conflict policy.
5. Add claim leases: mutable renewal, durable release, durable expiry.
6. Require `from_session_id` on new durable writes.
7. Add targeted `to_session_id` for direct handoffs and managed injection.
8. Require `ref_event_id` plus `causation_id` on ACK, accept, reject, resolve,
   supersede, and conflict-resolution events.
9. Move high-frequency heartbeat and transport delivery to brainstem-owned
   registry/index updates.
10. Project session, message, claim, and work states separately.
11. Replace risky-operation completion with `operation.result`.
12. Add authorization checks for privileged events.
13. Rate-limit checkpoints and keep event payloads bounded.
14. Keep crypto and federated ordering behind explicit triggers.

Use Rally Flow for implementation fan-out:

- `skills/rally-workflows/SKILL.md` decomposes the workstream, lints MECE
  ownership, stamps `run`/`step` lineage, and exposes `rally dag`.
- `skills/mini-loop/SKILL.md` gives each spawned worker a lightweight
  assess-plan-execute-judge loop without loading full build-loop in every worker.
- Cross-host dogfood still uses managed sessions and targeted handoffs when the
  test needs independent Claude/Codex terminals.

Acceptance tests should prove:

- Two Claude sessions can be distinguished and targeted.
- A delivered handoff is not treated as ACKed.
- ACK requires exact `from_session_id` and `ref_event_id`.
- Claims conflict by overlapping structured resource scopes.
- Exclusive claim acquisition is transactional under concurrent writers.
- Lease renewal does not append durable facts.
- Claim expiration emits one durable event and frees active ownership.
- Stale sessions surface auto-releasable claims without automatically releasing.
- Heartbeat updates do not append durable ledger rows.
- Replayed ledger plus registry snapshot reconstructs current room state.
- Duplicate writer retries do not duplicate durable facts.
- Authorization prevents an ordinary observer from releasing another session's
  claim or publishing privileged operation results.

## North-Star Fit

This approach advances the product north star because it makes Rally:

- Trustworthy: claims are tied to session identity and exact evidence.
- Lossless: durable coordination facts remain append-only and replayable.
- Scalable: liveness and high-volume state avoid ledger bloat.
- Collision-resistant: exclusive claims are acquired transactionally before work
  starts.
- Host-neutral: Claude, Codex, Cursor, CI, local shells, and cloud workers share
  one protocol vocabulary.
- Deconflicting: resource scopes and indexes expose collisions before work is
  merged or pushed.
- Human-light: agents can hand off, ACK, accept, block, resolve, and supersede
  without the user acting as clipboard.
- Local-first: repo-local rooms remain canonical, with cross-repo discovery as a
  pointer layer rather than a second truth store.

The protocol should make a busy repo feel like a room where every agent knows
who is present, what is owned, what changed, what is waiting, and what proof
exists.
