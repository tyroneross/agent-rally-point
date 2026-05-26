<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rust Greenfield Architecture

This is the target architecture for Rally if Rust is the product, not a port of
the current Python CLI. The goal is to be ambitious about the coordination
substrate while staying in Rally's lane: local-first, file-backed, agent-first
coordination. Rally is not an agent runtime, scheduler, broker, chat service, or
workflow engine.

This proposal should be read alongside
[`AGENT_COORDINATION_LANDSCAPE.md`](AGENT_COORDINATION_LANDSCAPE.md), which
benchmarks Rally against A2A, MCP, ACP, OpenAI Agents SDK, LangGraph, CrewAI,
AutoGen, Temporal, OpenTelemetry, CloudEvents, and local-first sync systems.

## Problem

Independent coding agents need one durable coordination substrate that works
locally, can sync safely across machines, and gives agents stable JSON state
without requiring a daemon or central service.

## Greenfield Sketch

If Rally started today, it would be:

- A single Rust binary named `rally`.
- A small Rust library stack with one event model, one store, one query engine,
  one diagnosis engine, one trust layer, and one sync/import boundary.
- `changes.jsonl` as the public, durable, append-only protocol.
- Optional local projections for speed, treated as disposable caches.
- Signed portable events as the unit of remote trust.
- Local sequence/revision as replica metadata, never portable identity.
- JSON output as the primary product surface; human text as a rendering layer.
- Adapters for Herdr, ACP, A2A, CI, and editor surfaces outside the core.

Estimated target: roughly 5k-7k Rust LOC for the core product surface before
adapters, plus tests and docs. The current repository is roughly 14k LOC across
Python, Rust, and docs. The target is smaller because the rewrite does not carry
forward duplicate interpretations of the trace.

## Quality Bar

Greenfield means Rally optimizes for the best architecture, not the safest path
from the current one. The bar is:

- **Clean:** one typed event model, one append path, one query engine, one
  command envelope. `serde_json::Value` belongs at I/O boundaries and tests, not
  in core domain logic.
- **Secure:** unsigned or untrusted remote events are visible facts, never
  automation authority. Trust checks happen before any bridge can affect an
  agent, editor, shell, or file.
- **Resilient:** append is crash-safe, concurrent writers coordinate through a
  lock, corrupt records are isolated with diagnostics, and all derived state is
  rebuildable from the log.
- **Efficient:** reads stream by default, projections allocate only their output,
  indexes are optional caches, and hot commands avoid reparsing unrelated
  history once a verified checkpoint exists.
- **Agent-first:** every command has stable JSON output, predictable exit codes,
  and enough machine-readable context for another agent to decide what to do
  next without scraping prose.

The implementation should choose simple data structures until measurement says
otherwise, but it should not choose ambiguous ownership, permissive trust, or
duplicate interpretation paths for convenience.

## Non-Goals

Rally should not become:

- A long-running orchestrator that owns agent lifecycles.
- A remote procedure call protocol between agents.
- A general workflow engine with retries, scheduling, and task queues.
- A database server.
- A replacement for Git, A2A, ACP, MCP, Herdr, LangGraph, CrewAI, or Temporal.
- A product that requires network access to coordinate agents on one machine.

The lane is narrower and stronger: Rally records coordination truth and derives
agent-usable state from it.

## Rally Owns, Bridges, Refuses

| Category | Scope |
|---|---|
| Owns | Durable coordination facts, append-only event storage, derived inbox/claims/blockers/diagnosis state, local trust policy, signed event import/export. |
| Bridges | A2A task/context IDs, MCP tools/resources, ACP-connected coding agents, AG-UI frontends, OpenTelemetry traces, CloudEvents event buses. |
| Refuses | Agent runtime loops, model/tool orchestration, editor transport, hosted workflow execution, broker semantics, global federation service. |

## Load-Bearing Invariant

Rejected:

> Python owns the product surface; Rust is an emerging protocol/trust helper.

That forces each new remote-ready capability to cross language and ownership
boundaries. Signing, trust, diagnosis, preflight, and eventual sync risk being
implemented as related but separate interpretations of the same JSONL trace.

Proposed:

> Rust owns the coordination kernel; every product surface is a view or adapter
> over that kernel.

This collapses the architecture. Instead of porting Python modules to Rust, we
build the kernel Rally should always have had and delete the older surfaces that
do not fit it.

## Core Model

Rally is an event-sourced coordination kernel.

```text
agent action
  -> Event
  -> Store append
  -> Query projection
  -> Diagnosis / inbox / replay / sync / trust output
```

The core types should stay few and boring:

| Type | Role |
|---|---|
| `Event` | Portable, typed, signable coordination fact. |
| `StoreEntry` | Local wrapper around an event: local sequence, origin, received time, hashes. |
| `Store` | Locked append, streaming read, checkpoint, and merge over JSONL. |
| `Query` | Pure derived state: inbox, thread, claims, blockers, timeline. |
| `Diagnosis` | Deterministic findings over derived state and trust classification. |
| `Identity` | Local key material and signer identity. |
| `TrustPolicy` | Local rules mapping keys to tools, event kinds, and capabilities. |
| `SyncEnvelope` | Import/export packet for remote replicas. |

Everything else is either an adapter or a rendering concern.

## Storage Primitive

`changes.jsonl` remains the canonical public primitive.

It is the right durable protocol because it is append-only, inspectable, easy to
copy, easy for agents to reason about, friendly to Git and shell tools, and
usable without a daemon. Greenfield Rust should not replace this with SQLite as
truth.

The stronger storage split is:

```text
changes.jsonl       canonical source of truth
rally.lock          advisory writer lock
rally.index.sqlite  optional local projection cache
checkpoints/        optional verified projection checkpoints
quarantine/         rejected or corrupt input packets
```

Indexes and snapshots must be rebuildable from `changes.jsonl`. They are never
authoritative.

Append rules:

- A writer takes `rally.lock`, reads the current tail metadata, assigns the next
  `local_seq`, writes exactly one newline-delimited `StoreEntry`, flushes, and
  releases the lock.
- Each `StoreEntry` carries `event_hash` and `prev_entry_hash` so corruption,
  accidental rewrites, and forked local histories are diagnosable.
- Readers stream records through a validating iterator. A corrupt line stops
  authoritative projection at that point and emits a diagnostic instead of
  silently skipping history.
- Optional indexes store offsets and projection checkpoints only after the log
  prefix has been hash-verified.

## Event Shape

The target event line separates portable truth from local replica metadata:

```json
{
  "local_seq": 12,
  "received_at": "2026-05-26T18:06:20.000Z",
  "origin": "local",
  "event_hash": "sha256:...",
  "prev_entry_hash": "sha256:...",
  "event": {
    "specversion": "1.0",
    "id": "evt_345ea9b74be3461b9473e0cf80a79d40",
    "source": "urn:agent-rally-point:tool:codex",
    "subject": "agent-rally-point",
    "time": "2026-05-26T18:06:19.660Z",
    "kind": "handoff",
    "type": "agent-rally.handoff.created.v1",
    "tool": "codex",
    "model": "gpt-5",
    "run_id": "codex-123",
    "app_slug": "agent-rally-point",
    "thread_id": "thr_6d9d66a7e94844faacaa41f2fc1bafa5",
    "causation_id": null,
    "correlation_id": "thr_6d9d66a7e94844faacaa41f2fc1bafa5",
    "datacontenttype": "application/json",
    "dataschema": "urn:agent-rally-point:schema:handoff.created.v1",
    "payload": {
      "from_tool": "codex",
      "to_tool": "pi",
      "subject": "review trust policy verifier",
      "requires_ack": true
    },
    "signature": {
      "version": "rally-signature-v1",
      "algorithm": "ed25519",
      "key_id": "key_codex_local",
      "signed_at": "2026-05-26T18:06:19.700Z",
      "canonicalization": "rally-json-v1",
      "signature": "base64..."
    }
  }
}
```

Rust writes this wrapper shape. Protocol helpers may also operate on bare
portable event objects for signing, verification, and import/export packet
construction, but `changes.jsonl` is not required to preserve any older flat
record shape.

`event_hash` is computed from canonical event bytes without the signature
envelope. `prev_entry_hash` is computed over the previous complete store-entry
line. Signatures prove event authorship; hashes prove local log integrity.

## Trust Model

Trust is not a later feature. It is part of whether an event may influence
automation.

| State | Automation meaning |
|---|---|
| `trusted` | Eligible for normal agent automation. |
| `valid-untrusted` | Authenticated but not authorized for this tool/kind. |
| `unsigned` | Visible legacy/local information; not remote-authoritative. |
| `unknown-key` | Visible but requires operator or policy update. |
| `invalid` | Diagnostic finding; never automation-authoritative. |
| `unsupported` | Visible but not trusted by this verifier. |

Rally does not delete or hide untrusted events. It classifies them and lets
commands decide what authority is required.

Examples:

- `rally replay` can show all events with trust badges.
- `rally inbox` can include untrusted handoffs but mark them.
- `rally herdr inject` should require trusted input or an explicit override.
- `rally sync import` should preserve every event but report trust counts.

Security invariants:

- Canonicalization is versioned and deterministic.
- Signing keys are local secrets; public trust policy is explicit and auditable.
- Remote import never overwrites local log history. It appends classified facts
  or quarantines malformed packets.
- Bridge adapters must declare the minimum trust state they require before they
  can act on an event.
- File paths in payloads are data until a command validates them against the
  active repository root and command authority.

## Command Contract

JSON is primary. Text is a renderer over the same structs.

Every command should support `--json` with a stable envelope:

```json
{
  "ok": true,
  "command": "diagnose",
  "schema": "agent-rally.command.diagnose.v1",
  "channel": "/Users/me/.agent-rally-point/apps/repo-12345678",
  "data": {}
}
```

Errors use the same shape on stderr:

```json
{
  "ok": false,
  "command": "diagnose",
  "error": "invalid --since value",
  "exit_code": 2
}
```

The first-class commands should be:

| Command | Purpose |
|---|---|
| `rally preflight` | Session-start routing, peers, pending ACKs, context locations. |
| `rally post` | Low-level event append for automation. |
| `rally handoff` / `ack` / `reject` / `needs-info` | Handoff lifecycle. |
| `rally claim` / `release` / `claims` | Ownership state. |
| `rally blocker` / `unblock` / `blockers` | Blocker state. |
| `rally inbox` | Agent-specific pending work. |
| `rally thread` | Related event expansion. |
| `rally replay` / `report` | Timeline and summaries. |
| `rally diagnose` | Deterministic coordination findings. |
| `rally verify` | Signature/trust verification. |
| `rally identity` / `trust` | Local keys and trust policy. |
| `rally sync export` / `sync import` | Remote-safe event movement. |

Adapter commands, such as Herdr injection, should live behind feature gates or
adapter crates once the core is stable.

## Crate Layout

Target workspace:

```text
crates/
  rally-core/
    event.rs       typed event model and payload enums
    store.rs       locked JSONL append, streaming read, checkpoints
    query.rs       inbox, threads, claims, blockers, report/replay
    diagnose.rs    deterministic findings and score
    preflight.rs   session-start projection over store + presence
    presence.rs    ephemeral TTL state
    repo.rs        repo identity and channel resolution
  rally-trust/
    identity.rs    key generation and local key storage
    policy.rs      trust.toml loading and policy evaluation
    verify.rs      canonicalization and signature verification
    sign.rs        sign-on-write support
  rally-sync/
    export.rs      signed event packets
    import.rs      merge, origin, duplicate/conflict reporting
  rally-cli/
    main.rs        argument parsing and rendering only
```

`rally-cli` should stay thin. If command behavior is hard to test without
shelling out to the binary, it belongs in `rally-core`.

Initial implementation status:

- `rally-protocol` owns portable event canonicalization and merge by event id.
- `rally-trust` owns signature verification and trust policy loading.
- `rally-core` now owns channel loading and deterministic query/diagnose
  projections over portable events and store entries.
- `rally-core::store` now has locked append, strict store-entry reads,
  `event_hash`, and `prev_entry_hash` validation for the greenfield log shape.
- `rally-core` has the first typed `EventRecord`/`EventKind` boundary so query
  logic can stop growing around raw JSON kind strings.
- `rally-cli verify --json` now reads through `rally-core` and emits the command
  envelope shape for success and failure.
- `rally-cli` is still a prototype verifier surface and should become a
  renderer over `rally-core` outputs rather than owning behavior.

## Remote Agent Readiness

Remote support means import/export of coordination facts, not a Rally-hosted
network.

Minimum viable remote flow:

1. Agent A exports a set of signed events.
2. Agent B imports them into a local channel.
3. The store deduplicates by event id.
4. Conflicting same-id/different-bytes records are preserved as conflicts.
5. Trust is evaluated against local policy.
6. Derived state includes origin and trust classification.
7. Automation only acts on events whose trust state meets the command's policy.

Import must be idempotent and bounded-memory: validate packet structure, verify
event hashes/signatures, sort only the packet being imported if needed, append
accepted entries, and report rejected entries by reason.

Network transport is out of scope. Files, Git, rsync, a shared folder, A2A, or a
future service can move the bytes. Rally defines what the bytes mean.

## Protocol Interop

Rally should be easy to bridge because its kernel is narrower than the
surrounding protocols:

| Protocol/runtime | Rally mapping |
|---|---|
| A2A | `handoff`/`ack`/`needs-info` map to task lifecycle state; `thread_id` maps to A2A context; attachments/results map to artifacts. |
| MCP | Expose `inbox`, `diagnose`, `verify`, `thread`, and `trust` as tools/resources for any MCP-capable agent client. |
| ACP | Treat editor-connected coding agents as producers/consumers; ACP sessions can post Rally presence, handoffs, claims, and verdicts. |
| AG-UI | UI event streams can render Rally events and derived state without changing the core. |
| OpenAI Agents SDK | SDK handoffs/sessions/traces can emit Rally events for cross-process coordination. Rally does not replace the SDK agent loop. |
| LangGraph/CrewAI/AutoGen | Orchestrated workflows can publish Rally events at handoff, claim, blocker, and verdict boundaries. Rally stays below orchestration. |
| Temporal | Temporal workflows can emit Rally events; Rally does not provide durable workflow execution. |
| OpenTelemetry | Rally event IDs, thread IDs, and causation IDs can become trace links or span attributes. |
| CloudEvents | Rally events stay CloudEvents-aligned for event bus export/import. |

## What Gets Deleted

These are target deletions during the Rust cutover:

| Current surface | Why it goes | Replacement |
|---|---|---|
| Python dict-based event construction | Not part of the target architecture. | `rally-core::EventBuilder` or typed payload constructors. |
| Python trace query helpers | Duplicate the Rust query engine. | `rally-core::Query`. |
| Python scorer/diagnose logic | Duplicate deterministic findings. | `rally-core::diagnose`. |
| Python preflight logic | Duplicates store/query/presence interpretation. | `rally preflight`. |
| Python CLI command implementations | Text/JSON rendering should use Rust structs. | `rally-cli`. |
| Rust `rally-rs` prototype name | Transitional name leaks implementation status. | Binary named `rally`. |
| Docs that say signing/sync are future-only | Trust becomes active architecture. | Signed/trusted event docs tied to code. |

## No Migration Contract

The greenfield target has no old-log carry-forward requirement and no Python
behavior gate. Existing code can inform vocabulary and product lessons, but it
is not the oracle for command behavior or storage shape.

The durable contract is at the agent boundary: commands expose stable JSON
schemas and clear exit codes. Human text exact wording is never a contract.

## What Survives

| Existing asset | Why it earns its weight |
|---|---|
| `changes.jsonl` | Correct public protocol primitive for inspectable event sourcing. |
| Coordination trace docs | Strong product semantics and event vocabulary. |
| Rust canonicalization and trust spike | Seed for the actual kernel. |
| Herdr bridge concept | Valuable adapter, but not core architecture. |

## Build Strategy

1. **Land the Rust event/trust seed.** PRs for protocol, merge, verification,
   trust policy, and JSON verifier establish the kernel direction.
2. **Create the typed kernel.** `rally-core` owns event builders, store-entry
   validation, query structs, diagnosis structs, and command data models.
3. **Implement the safe store.** Locked append, streaming read, hash-chain
   validation, quarantine, and rebuildable checkpoints come before broad CLI
   coverage.
4. **Implement read projections.** `report`, `replay`, `thread`, `inbox`,
   `claims`, `blockers`, `conflicts`, `diagnose`, and `verify` run over the
   same query engine and target store shape.
5. **Implement writes.** `handoff`, `ack`, `claim`, `blocker`, and low-level
   `post` write the target store-entry event shape.
6. **Add identity and sign-on-write.** Local key generation, `trust add`, and
   signing policies make remote events authoritative.
7. **Add sync import/export.** Keep transport out of scope; focus on event
   packets, merge, origin, conflict, and trust reporting.
8. **Make Rust the installed surface.** The installed `rally` command is the
   Rust binary.
9. **Delete older command implementations.** Keep only adapters that match the
   new architecture.

## Verification Strategy

The greenfield rewrite must be proven by behavior, not by preserving structure.

- Rust unit tests for event canonicalization, read/write, query, diagnosis,
  signing, trust, and merge conflict behavior.
- Golden JSON command tests for every agent-facing command.
- Property-style tests for append/read round trips and duplicate event merges.
- Crash tests for interrupted writes, corrupt tails, and concurrent appenders.
- Snapshot/index rebuild tests that prove cached state is disposable.
- Security tests for trust policy denial, path normalization, malformed packet
  quarantine, and untrusted bridge refusal.
- A small set of end-to-end shell tests using the installed `rally` binary.

## Risks

- **Cutover risk.** Some local scripts may still invoke older entry points.
  Caller search must happen before deletion.
- **Schema churn risk.** Store-entry lines are the target shape. Tests must make
  the new JSON contract explicit so agents know what to emit and consume.
- **Hash-chain complexity.** Integrity metadata must stay simple enough that
  agents can still inspect the log and operators can repair local corruption.
- **Ambition creep.** Sync can pull Rally toward being a service. The boundary
  stays at import/export packets and local merge semantics.
- **Trust UX risk.** If trust is too strict too early, local coordination gets
  noisy. Commands should classify first, then tighten authority by command.

## Success Criteria

Rally's Rust rewrite is successful when:

- `rally --json` is the stable agent discovery surface.
- `rally preflight --json` works without Python.
- `rally diagnose --json` includes coordination and trust findings from one
  Rust query engine.
- A signed event exported from one channel can be imported into another,
  deduplicated, classified, and shown in inbox/thread/replay output.
- A corrupt or partially written log produces a precise diagnostic without
  corrupting derived state.
- Hot read commands can use verified checkpoints and avoid full-log reparsing.
- No normal command path calls Python.
- The codebase is smaller because old interpretations were deleted, not because
  behavior was hidden behind wrappers.
