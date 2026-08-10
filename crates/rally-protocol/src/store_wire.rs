// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Typed wire contract for the per-repo `rallyd` store daemon (BACKLOG S-P3,
//! ADR-02). This is the FROZEN coupling surface between the routed store client
//! (`rally-cli::store_client`, Chunk C) and the daemon dispatcher
//! (`rally-cli::rallyd_core`, Chunk B). Both build against the types here; any
//! change is a stop-the-line Chunk-A amendment, never patched unilaterally.
//!
//! ## Framing (mirrors `daemon_client.rs::round_trip`, L3)
//!
//! Line-delimited JSON: the client writes one [`StoreRequest`] serialised to a
//! single line (`\n`-terminated) and reads one [`StoreResponse`] line back.
//! One request → one reply over a dedicated connection, so no `id` correlation
//! field is needed (the ptyd `{"id","method","params"}` envelope is the framing
//! *inspiration*; here the whole typed [`StoreRequest`] IS the line).
//!
//! ## Why the payloads are `serde_json::Value` (KEY CONTRACT DECISION)
//!
//! The store payload types — `Fact`, `RoomSnapshot`, `ReadReceipt`,
//! `ActiveClaimRecord` — live in `rally-cli` as `pub(crate)` types, and
//! `rally-cli` *depends on* `rally-protocol`. Referencing them here would be a
//! circular crate dependency. ADR-02 froze the wire home in `rally-protocol`
//! (the zero-extra-dep serde crate), so the resolution is: the ENVELOPE
//! ([`StoreRequest`]/[`StoreResponse`] and the closed [`StoreOp`]/[`StoreOk`]
//! enums) is fully typed and closed here; the leaf payloads travel as
//! [`serde_json::Value`] and are strictly (de)serialised into the concrete
//! `pub(crate)` types AT the `rally-cli` boundary (`Fact` etc. all derive
//! `Serialize + Deserialize`, verified against the segment-replay and
//! snapshot-cache paths). Closed-enum validation therefore holds at both the
//! envelope (unknown `kind` → deserialise error) and the leaf (concrete
//! `Deserialize`) layers. Zero new deps: only `serde` + `serde_json`, already
//! `rally-protocol`'s charter.
//!
//! ## Engagement scoping (L9 / R4)
//!
//! Every store op carries the CLIENT's already-resolved [`StoreRequest::engagement`]
//! label. The single-threaded daemon dispatcher applies it per request via
//! `DirectRoomStore::set_engagement_scope` before dispatching, and NEVER
//! consults its own process env. `None` = resolve the room default
//! (active-engagement file → UTC date), no env read.
//!
//! ## Transport-error mapping (R7 / G8) — documented contract, enforced in rally-cli
//!
//! Wire *store* errors ([`StoreError`]) map onto the existing `RallyError`
//! variants with exit-code parity (`kind` selects the variant; `code` is the
//! redundant exit code). TRANSPORT-layer failures — connect/read timeout,
//! connection reset, an over-long line (> [`MAX_LINE_BYTES`]), or a
//! `wire_version` / `repo_root` mismatch — have NO direct-path equivalent and
//! MUST map to `RallyError::Command` (exit 1) with remedy text naming
//! `rally daemon status` / `rally daemon stop`. These classes are therefore
//! excluded from the fail-open goldens (T-04) and covered by dedicated unit
//! assertions instead. The concrete `RallyError` conversion lives in `rally-cli`
//! (it cannot live here — `RallyError` is `rally-cli`-private); this module
//! freezes the SHAPE and the mapping contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire protocol version. Bumped only on a breaking envelope change. The ping
/// reply carries this; a client seeing a version it does not speak treats the
/// daemon as not-live and lets the ownership lock decide (ADR-02 rollback note).
pub const WIRE_VERSION: u32 = 3;

/// Hard cap on a single request/response line. A longer line is a framing
/// error (or an abuse) and maps to the transport-error class (R7): the daemon
/// rejects it with a structured error and closes the connection.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// One request line on the wire. The whole struct serialises to a single
/// `\n`-terminated JSON line.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoreRequest {
    /// Protocol version the client speaks. The daemon rejects a mismatch as a
    /// transport error (R7) rather than attempting a best-effort dispatch.
    pub wire_version: u32,
    /// The client's already-resolved engagement label (L9). `None` = room
    /// default; the daemon never reads its own env to fill this.
    #[serde(default)]
    pub engagement: Option<String>,
    /// The store operation to perform.
    pub op: StoreOp,
}

impl StoreRequest {
    /// Construct a request stamped with the current [`WIRE_VERSION`].
    pub fn new(engagement: Option<String>, op: StoreOp) -> Self {
        Self {
            wire_version: WIRE_VERSION,
            engagement,
            op,
        }
    }
}

/// The closed set of ROUTED store operations — exactly one variant per
/// `RoomStore` `pub(crate)` method classified `routed` (touches `facts.db`).
/// LOCAL methods (pure accessors + `cursors.json` file ops) are intentionally
/// ABSENT: the routed client answers them from its own local state without a
/// round-trip (see the classification table on `RoomStore` in
/// `rally-cli/src/store.rs`).
///
/// Internally tagged on `"kind"`: an unknown kind fails deserialisation, which
/// is the closed-enum guarantee.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Each variant is documented; the payload fields (`fact`, `tool`, `read_seq`,
// …) are self-describing, so field-level docs are allowed here.
#[allow(missing_docs)]
pub enum StoreOp {
    /// `RoomStore::append_fact(fact)` → `Fact`.
    AppendFact { fact: Value },
    /// `RoomStore::append_fact_verified(fact)` → `Fact`.
    AppendFactVerified { fact: Value },
    /// `RoomStore::append_state_transition_verified(fact)` → `Fact`.
    AppendStateTransitionVerified { fact: Value },
    /// `RoomStore::append_session_fact_if_context(fact, expected)` →
    /// `Option<Fact>` (`None` = conditional-append conflict, NOT an error).
    AppendSessionFactIfContext {
        fact: Value,
        expected_context_version: Option<u64>,
    },
    /// `RoomStore::facts()` → `Vec<Fact>`.
    Facts,
    /// `RoomStore::rebuild_claim_index()` → `()`.
    RebuildClaimIndex,
    /// `RoomStore::renew_claim_lease(claim_id, lease_expires_at, caller, expected)` →
    /// `Option<ActiveClaimRecord>`.
    RenewClaimLease {
        claim_id: String,
        lease_expires_at: String,
        /// Tool asserted by the process requesting renewal. Optional at the
        /// serde boundary so a missing field is decoded and refused by the
        /// authority check rather than synthesized from the claim.
        #[serde(default)]
        caller_tool: Option<String>,
        /// Protocol session asserted by the caller. `None` is valid only for a
        /// legacy claim whose owner session is also absent.
        #[serde(default)]
        caller_session_id: Option<String>,
        /// Session observed on the claim by the caller before dispatch. The
        /// daemon verifies it still matches instead of deriving authority from
        /// `claim_id`.
        #[serde(default)]
        expected_owner_session_id: Option<String>,
    },
    /// `RoomStore::expire_claim_leases_at(now)` → `Vec<Fact>`.
    /// `now` is RFC3339 (chrono is NOT a `rally-protocol` dep; the boundary
    /// parses it back into `chrono::DateTime<Utc>`).
    ExpireClaimLeasesAt { now_rfc3339: String },
    /// `RoomStore::session_facts_with_context_version()` →
    /// `(Vec<Fact>, Option<u64>)`.
    SessionFactsWithContextVersion,
    /// `RoomStore::snapshot()` (== `snapshot_with_archived(false)`) and
    /// `RoomStore::snapshot_with_archived(include_archived)` → `RoomSnapshot`.
    SnapshotWithArchived { include_archived: bool },
    /// `RoomStore::snapshot_with_readers_archived(include_archived)` →
    /// `RoomSnapshot` (with reader receipts projected).
    SnapshotWithReadersArchived { include_archived: bool },
    /// `RoomStore::last_checkpoint_seq(tool)` → `i64`.
    LastCheckpointSeq { tool: String },
    /// `RoomStore::maybe_append_read_checkpoint(tool, read_seq)` →
    /// `Option<Fact>`.
    MaybeAppendReadCheckpoint { tool: String, read_seq: i64 },
    /// `RoomStore::project_read_receipts(max_seq)` → `Vec<ReadReceipt>`.
    ProjectReadReceipts { max_seq: i64 },
    /// Liveness + identity probe (NOT a `RoomStore` method). Reply is
    /// [`StoreOk::Pong`]; the client verifies `repo_root` matches its own and
    /// `wire_version` is one it speaks before routing (L7, ADR-02).
    Ping,
}

/// One response line on the wire: either the op succeeded ([`StoreOk`]) or it
/// failed ([`StoreError`]). Externally tagged so a reply is unambiguously one
/// or the other.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreResponse {
    /// The op succeeded; payload mirrors the op's `Result::Ok`.
    Ok(StoreOk),
    /// The op failed; carries the wire error for boundary reconstruction.
    Err(StoreError),
}

/// The success payload, mirroring each [`StoreOp`]'s `Result::Ok` type. Leaf
/// payloads are `Value` for the same crate-layering reason as [`StoreOp`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Variants documented; payload fields are self-describing (see [`StoreOp`]).
#[allow(missing_docs)]
pub enum StoreOk {
    /// `Fact`.
    AppendFact { fact: Value },
    /// `Fact`.
    AppendFactVerified { fact: Value },
    /// `Fact`.
    AppendStateTransitionVerified { fact: Value },
    /// `Option<Fact>` (`None` = conditional-append conflict).
    AppendSessionFactIfContext { fact: Option<Value> },
    /// `Vec<Fact>`.
    Facts { facts: Vec<Value> },
    /// `()`.
    RebuildClaimIndex,
    /// `Option<ActiveClaimRecord>`.
    RenewClaimLease { record: Option<Value> },
    /// `Vec<Fact>`.
    ExpireClaimLeasesAt { facts: Vec<Value> },
    /// `(Vec<Fact>, Option<u64>)`.
    SessionFactsWithContextVersion {
        facts: Vec<Value>,
        context_version: Option<u64>,
    },
    /// `RoomSnapshot`.
    Snapshot { snapshot: Value },
    /// `RoomSnapshot` (readers projected).
    SnapshotWithReaders { snapshot: Value },
    /// `i64`.
    LastCheckpointSeq { seq: i64 },
    /// `Option<Fact>`.
    MaybeAppendReadCheckpoint { fact: Option<Value> },
    /// `Vec<ReadReceipt>`.
    ProjectReadReceipts { receipts: Vec<Value> },
    /// Ping reply: the daemon's identity for the client's pre-route check.
    Pong {
        repo_root: String,
        pid: u32,
        wire_version: u32,
    },
}

/// A structured store error, mirroring a `RallyError` across the wire with
/// exit-code parity (G8). The boundary in `rally-cli` reconstructs the concrete
/// `RallyError` from `kind`; `code` is the redundant exit code for assertions.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoreError {
    /// Redundant with `kind`'s exit code; carried for cheap assertion.
    pub code: u8,
    /// Which `RallyError` variant to reconstruct at the boundary.
    pub kind: StoreErrorKind,
    /// Rendered error message (the `Display` text of the original error).
    pub message: String,
}

/// Closed tag selecting which `RallyError` variant the boundary reconstructs.
/// `Io`/`Json` lose their `source` over the wire and reconstruct as
/// `RallyError::Command` (same exit code 1); `Transport` is the R7 class with
/// no direct-path equivalent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreErrorKind {
    /// → `RallyError::Usage` (exit 2).
    Usage,
    /// → `RallyError::NotFound` (exit 3).
    NotFound,
    /// → `RallyError::Command` (exit 1).
    Command,
    /// → `RallyError::Message` (exit 1).
    Message,
    /// `RallyError::Io`/`Json` collapsed for the wire → reconstruct as
    /// `RallyError::Command` (exit 1, source dropped).
    Internal,
    /// R7 transport class (timeout / reset / oversized line / version or
    /// repo_root mismatch). No direct-path equivalent → reconstruct as
    /// `RallyError::Command` (exit 1) with remedy text.
    Transport,
}

impl StoreError {
    /// Build a [`StoreError`] with the exit `code` implied by `kind`.
    pub fn new(kind: StoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            code: kind.exit_code(),
            kind,
            message: message.into(),
        }
    }

    /// A transport-class error (R7) with the standard remedy text.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(StoreErrorKind::Transport, message)
    }
}

impl StoreErrorKind {
    /// The CLI exit code this kind maps to at the `rally-cli` boundary — the
    /// exit-code-parity contract (G8).
    pub fn exit_code(self) -> u8 {
        match self {
            StoreErrorKind::Usage => 2,
            StoreErrorKind::NotFound => 3,
            StoreErrorKind::Command
            | StoreErrorKind::Message
            | StoreErrorKind::Internal
            | StoreErrorKind::Transport => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_a_single_line() {
        let req = StoreRequest::new(
            Some("alpha".to_string()),
            StoreOp::MaybeAppendReadCheckpoint {
                tool: "claude_code:01".to_string(),
                read_seq: 42,
            },
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains('\n'));
        let back: StoreRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back.wire_version, WIRE_VERSION);
        assert_eq!(back.engagement.as_deref(), Some("alpha"));
        matches!(
            back.op,
            StoreOp::MaybeAppendReadCheckpoint { read_seq: 42, .. }
        );
    }

    #[test]
    fn unknown_op_kind_is_rejected() {
        let bad = r#"{"wire_version":1,"engagement":null,"op":{"kind":"not_a_real_op"}}"#;
        assert!(serde_json::from_str::<StoreRequest>(bad).is_err());
    }

    #[test]
    fn unknown_envelope_field_is_rejected() {
        let bad = r#"{"wire_version":1,"engagement":null,"op":{"kind":"facts"},"extra":1}"#;
        assert!(serde_json::from_str::<StoreRequest>(bad).is_err());
    }

    #[test]
    fn value_payload_carries_an_opaque_fact() {
        // Mirrors the rally-cli boundary: a Fact serialises to a Value here and
        // deserialises back losslessly on the other side.
        let fact = serde_json::json!({"event_id": "fact_x", "seq": 7});
        let ok = StoreOk::AppendFact { fact: fact.clone() };
        let line = serde_json::to_string(&StoreResponse::Ok(ok)).unwrap();
        let back: StoreResponse = serde_json::from_str(&line).unwrap();
        match back {
            StoreResponse::Ok(StoreOk::AppendFact { fact: f }) => assert_eq!(f, fact),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn ping_reply_carries_identity() {
        let ok = StoreOk::Pong {
            repo_root: "/repo".to_string(),
            pid: 1234,
            wire_version: WIRE_VERSION,
        };
        let line = serde_json::to_string(&StoreResponse::Ok(ok)).unwrap();
        let back: StoreResponse = serde_json::from_str(&line).unwrap();
        match back {
            StoreResponse::Ok(StoreOk::Pong {
                pid, wire_version, ..
            }) => {
                assert_eq!(pid, 1234);
                assert_eq!(wire_version, WIRE_VERSION);
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn error_exit_codes_match_the_parity_contract() {
        assert_eq!(StoreErrorKind::Usage.exit_code(), 2);
        assert_eq!(StoreErrorKind::NotFound.exit_code(), 3);
        assert_eq!(StoreErrorKind::Command.exit_code(), 1);
        assert_eq!(StoreErrorKind::Message.exit_code(), 1);
        assert_eq!(StoreErrorKind::Internal.exit_code(), 1);
        assert_eq!(StoreErrorKind::Transport.exit_code(), 1);
        assert_eq!(StoreError::transport("gone").code, 1);
    }
}
