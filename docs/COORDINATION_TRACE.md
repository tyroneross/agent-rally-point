<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Rally Coordination Trace

Rally's coordination trace is an append-only `changes.jsonl` log of facts that
help independent coding agents coordinate in one repo.

The trace is:

- local-first
- file-backed
- replayable
- inspectable
- syncable through portable signed events
- independent of any agent runtime, editor protocol, or network service

## Trace Model

Each line is a Rust `StoreEntry`:

```text
StoreEntry {
  local metadata,
  event_hash,
  prev_entry_hash,
  event: PortableEvent
}
```

Local metadata is useful for one replica. The portable event is the unit of
identity, signing, verification, import, export, and derived state.

## Core Event Kinds

| Kind | Meaning |
|---|---|
| `handoff` | One tool asks another tool to take or review work. |
| `ack` | A handoff response: `done`, `rejected`, or `needs-info`. |
| `feedback` | A verifier or peer posts a verdict or commentary. |
| `claim` | A tool claims ownership of a file/resource. |
| `claim-release` | A tool releases a prior claim. |
| `blocker` | A tool records blocked work. |
| `blocker-resolved` | A blocker is resolved. |

## Derived State

Rally derives agent-facing state from the log:

- `inbox`: pending handoffs for a tool
- `claims`: active ownership claims
- `blockers`: unresolved blockers
- `conflicts`: overlapping active claims
- `thread`: related events by id, thread, causation, and correlation
- `replay` / `report`: chronological trace views
- `diagnose` / `score`: deterministic coordination findings
- `verify`: per-event signature and trust status
- `sync import`: duplicate, conflict, invalid, and trust counts

Derived state is not authoritative. It is rebuilt from the log.

## Trust

Unsigned or untrusted events remain visible facts, but they are not automation
authority. Commands that bridge event content into an agent, editor, shell, or
file must declare and enforce their minimum trust state.

Trust states are defined in `rally-trust`:

| State | Meaning |
|---|---|
| `trusted` | Signature is valid and allowed by local policy. |
| `valid-untrusted` | Signature is valid but not authorized by policy. |
| `unsigned` | No signature. Visible, not remote-authoritative. |
| `unknown-key` | Signature uses an unknown key. |
| `invalid` | Signature exists but verification failed. |
| `unsupported` | Algorithm or canonicalization is unsupported. |

## Sync

Sync packets carry portable events only:

```bash
rally sync export --channel-dir <dir> --json > packet.json
rally sync import --channel-dir <dir> --trust-policy <trust.toml> packet.json --json
```

Import appends accepted events with local origin metadata, preserves signatures,
deduplicates by event id and canonical bytes, reports same-id conflicts, and
classifies trust using local policy.

## Protocol Positioning

Rally is below orchestration:

- ACP, A2A, MCP, AG-UI, OpenTelemetry, and CloudEvents can bridge to Rally.
- Rally does not own agent lifecycles, model loops, task queues, or network
  transport.
- Rally's job is durable coordination truth and agent-usable derived state.
