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

## Mixed-version rooms: unknown event kinds

A room may run several rally versions at once, so the two sides of a version gap
each carry an obligation.

**Readers skip what they cannot name.** A canonical row whose kind this build
does not know is SKIPPED with a warning on stderr, never graded corruption and
never a write-blocker. The row stays on disk byte-for-byte for the binaries that
can read it, and it reaches no work surface — `FactKind::Unknown` appears in no
claim, handoff, blocker, or `next` bucket. The row's `tool` still reaches the
agent roster, because the roster reads envelope metadata every
`agent-rally.fact.v1` row carries rather than interpreting the kind; that peer is
the binary an operator has to upgrade.

Tolerance stops exactly there. Structural damage stays fail-loud: unparseable
JSON, a non-positive or broken seq, an unsupported `schema`, an empty
`event_id`, and any envelope/payload kind disagreement other than the
forward-compatible case — including an unknown `event_type` whose payload `kind`
does not match it, which no binary would ever write.

**Writers do not outrun the room.** Each kind carries a schema-floor generation,
and the room records the floor every participating binary is known to handle in
`.rally/schema-floor.json` (`agent-rally.schema-floor.v1`). A room with no
recorded floor, or with an unreadable one, reads as generation 1. Appending a
kind above the room's floor is refused at the write boundary with two ways
forward: dual-write the meaning onto a kind at or below the floor, carrying the
new semantics in a `protocol:` evidence marker older readers ignore; or upgrade
every binary in the room and then raise the floor with
`rally doctor --schema-floor --apply`. A room floor ABOVE the running binary
never blocks the kinds that binary does know — one upgraded peer must not lock
everyone else out.

Reader tolerance alone is not sufficient, which is why both halves exist: it only
protects binaries built after it shipped, and the binaries already installed
elsewhere are exactly the ones a new kind breaks.

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

## Fact v1 integration bridge

Referenced handoffs use the typed `ProtocolEventKind` and `EventEnvelope`
validators before append. Until the next Fact schema revision adds dedicated
envelope fields, the append-only Fact v1 row serializes that contract as one
Rally-owned marker per field in `evidence`. The required discriminator is
`protocol:bridge_version=fact-v1`; the controlled state marker is
`protocol:event_kind=handoff.requested|handoff.acked|handoff.accepted|handoff.rejected`.
The write boundary rejects missing, duplicate, unknown, or caller-supplied
`protocol:*` markers. Legacy rows replay without retroactive validation.
