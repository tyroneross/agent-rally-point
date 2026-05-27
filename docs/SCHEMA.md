<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Event Schema

`changes.jsonl` is Rally's durable coordination log. Each line is a strict JSON
store entry with local replica metadata wrapped around one portable event.

## Store Entry

Rust writes this shape:

```json
{
  "local_seq": 12,
  "received_at": "2026-05-26T18:06:20.000Z",
  "origin": "local",
  "event_hash": "sha256:...",
  "prev_entry_hash": "sha256:...",
  "event": {}
}
```

Fields:

| Field | Meaning |
|---|---|
| `local_seq` | Monotonic sequence assigned by this local channel. |
| `received_at` | Local receive/write time in RFC3339 UTC. |
| `origin` | Local origin label, such as `local` or `import:sync`. |
| `event_hash` | Hash of canonical event bytes. |
| `prev_entry_hash` | Hash chain pointer to the previous complete store-entry line. |
| `event` | Portable signed/syncable Rally event. |

Store metadata is never portable event identity. Sync export emits only
`event` objects.

## Portable Event

Portable events are CloudEvents-aligned JSON objects:

```json
{
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
    "to_tool": "pi",
    "from_tool": "codex",
    "subject": "review sync",
    "requires_ack": true
  }
}
```

Required identity fields:

| Field | Meaning |
|---|---|
| `id` | Stable event identity, preserved across export/import. |
| `kind` | Coarse event kind used by the Rust query engine. |
| `type` | Versioned semantic event type. |
| `tool` / `model` / `run_id` | Producer identity. |
| `thread_id` | Related-event grouping. |
| `payload` | Event-specific data. |

## Known Kinds

| Kind | Type |
|---|---|
| `handoff` | `agent-rally.handoff.created.v1` |
| `ack` | `agent-rally.handoff.acknowledged.v1` |
| `feedback` | `agent-rally.feedback.posted.v1` |
| `claim` | `agent-rally.claim.created.v1` |
| `claim-release` | `agent-rally.claim.released.v1` |
| `blocker` | `agent-rally.blocker.raised.v1` |
| `blocker-resolved` | `agent-rally.blocker.resolved.v1` |
| `profile` | `agent-rally.profile.updated.v1` |
| `subscription` | `agent-rally.subscription.updated.v1` |
| `task` | `agent-rally.task.updated.v1` |
| `artifact` | `agent-rally.artifact.recorded.v1` |
| `decision` | `agent-rally.decision.recorded.v1` |
| `lesson` | `agent-rally.lesson.recorded.v1` |

Unknown kinds may be displayed, but core automation should only rely on kinds
with typed Rust payload support.

Attuned context commands use the newer kinds as structured source facts:

- `profile`: capabilities, watched paths, current task, branch, availability.
- `subscription`: paths, event kinds, threads, and tasks an agent wants surfaced.
- `task`: objective, owner, lifecycle status, dependencies, artifacts,
  verification.
- `artifact`: structured output or reference attached to a task or trace.
- `decision`: source-linked project truth with status, scope, and supersession.
- `lesson`: reusable failure/success reflection with source event ids and
  confidence.

## Signatures

Signed events include a top-level `signature` object inside the portable event:

```json
{
  "signature": {
    "version": "rally-signature-v1",
    "algorithm": "ed25519",
    "key_id": "key_codex_local",
    "signed_at": "2026-05-26T18:06:19.700Z",
    "canonicalization": "rally-json-v1",
    "signature": "base64..."
  }
}
```

Canonical event bytes exclude `signature` and store metadata. See
[`SIGNED_EVENTS.md`](SIGNED_EVENTS.md).

## Non-Goal Guard

Records carry coordination structure only. They should not carry telemetry such
as call frequency, token usage, model latency, invocation counts, or hit counts.

Rally is a coordination substrate, not an observability ledger.
