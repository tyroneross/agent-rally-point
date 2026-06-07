<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Rally Protocol — Durable Event Envelope

Schema reference for the causal / idempotency / authorization envelope added by
[`crates/rally-cli/src/event_envelope.rs`](../../crates/rally-cli/src/event_envelope.rs).
Design source: [`docs/PROTOCOL-NORTH-STAR.md`](../PROTOCOL-NORTH-STAR.md).

Rally facilitates; hosts execute. Validation and authorization here are
**advisory decisions** the `say` write-path consumes — they reject/warn/record,
they never edit files or schedule work.

## Compatibility contract

Every envelope field is **optional** and `#[serde(default)]`. A legacy ledger row
that carries none of them deserializes to an all-`None` envelope, so **old logs
replay unchanged**. Event-kind validation — not the type — decides which ids are
mandatory. New durable writes SHOULD populate the envelope; the stricter
`from_session_id` requirement is gated (see [Compat mode](#compat-mode)).

## Envelope fields

| Field | Meaning |
|---|---|
| `idempotency_key` | Writer retry key; equal keys collapse to one durable fact. |
| `causation_id` | The event that directly caused this one. |
| `correlation_id` | The larger flow / user request this belongs to. |
| `ref_event_id` | Exact prior event being acked/accepted/rejected/resolved/superseded. |
| `work_id` | The task/run object. |
| `run_id` | One task-execution span (fan-out lineage). |
| `attempt_id` | One retry attempt of retryable work. |
| `claim_id` | The claim a claim-event acts on. |
| `handoff_id` | The handoff a handoff-event acts on. |
| `from_session_id` | The live session lease that authored the write (`session_identity`). |
| `principal_id` | Human/service/agent identity behind the session. |
| `actor_id` | Persona/subagent inside the session. |
| `auth_context` | `{ role, policy_version?, capabilities[] }` for privileged events. |

## Event kinds and required ids

Per-kind mandatory ids (`ProtocolEventKind::validate`). `ref_event_id` +
`causation_id` together mark a **reply** (the "delivered ≠ acked" invariant: an
ACK must cite the exact event it answers).

| Kind | Required ids |
|---|---|
| `claim.acquired` / `claim.released` / `claim.expired` / `claim.transferred` | `claim_id` |
| `handoff.requested` | `handoff_id` |
| `handoff.acked` / `handoff.accepted` / `handoff.rejected` | `handoff_id`, `ref_event_id`, `causation_id` |
| `work.resolved` / `work.superseded` | `work_id`, `ref_event_id`, `causation_id` |
| `work.failed` | `work_id`, `attempt_id` |
| `work.checkpoint` / `work.blocked` / `work.cancelled` / `work.abandoned` | `work_id` |
| `conflict.resolved` | `ref_event_id`, `causation_id` |
| `session.registered` / `session.closed` / `session.revoked` | — (brainstem; establishes sessions) |
| all others (`artifact.published`, `validation.result`, `decision.recorded`, …) | — |

`validate` returns **every** missing id, not just the first.

## Compat mode

| Mode | `from_session_id` |
|---|---|
| `Lenient` (default) | Not required — back-compatible with pre-session rows. |
| `Strict` (opt-in) | Required on every durable LLM-authored event. Brainstem `session.*` events are exempt — they establish the session and cannot cite one. |

Enable `Strict` only once every writer in a room emits `from_session_id`.

## Authorization (advisory)

Roles, lowest trust first: `observer < agent < lead_agent < maintainer < owner < system`.
A missing `auth_context` is treated as `observer`.

| Privileged action | Minimum role |
|---|---|
| `ReleaseOthersClaim` | `lead_agent` |
| `TransferClaim` | `lead_agent` |
| `CancelWork` | `lead_agent` |
| `SupersedeOthersWork` | `lead_agent` |
| `PublishValidation` | `agent` |
| `RiskyOperation` (merge/push/deploy/prune) | `agent` |

`authorize()` returns a boolean decision. Local trusted rooms surface a denial as
a warning; hardened rooms turn it into a hard reject ("warnings over hard locks").

## Idempotency

`Deduper.observe(envelope, event_id)` keys on `idempotency_key` when present, else
`event_id`. First sighting → append; duplicate retry → skip. Prevents a
double-delivered write from creating two durable facts.

## Integration note

This module is delivered isolated (branch `claude/session-identity`). In
`integration-wiring` its fields fold into `store::Fact`, `validate`/`authorize`
are called at the `say` write-path, and `Deduper` backs append-dedup. The staged
`#![allow(dead_code)]` is removed then.
