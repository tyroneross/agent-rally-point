// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Event envelope — causal, idempotency, and authorization context for durable
//! coordination events.
//!
//! ## Why
//! Per [`docs/PROTOCOL-NORTH-STAR.md`](../../../docs/PROTOCOL-NORTH-STAR.md),
//! every durable event should be "small, typed, idempotent, and replayable".
//! This module adds the causal/auth fields the north star requires
//! (`causation_id`, `correlation_id`, `idempotency_key`, `work_id`, `run_id`,
//! `attempt_id`, `claim_id`, `handoff_id`, `from_session_id`, `principal_id`,
//! `actor_id`, `auth_context`) **without** widening the legacy `Fact` shape in a
//! breaking way: every field is optional and `#[serde(default)]`, so an old
//! ledger row missing all of them still deserializes (replay-safe).
//!
//! Two pure capabilities sit on top of the envelope:
//!
//! 1. [`ProtocolEventKind::validate`] — event-kind validation: which ids are
//!    mandatory for a given kind (claim events need `claim_id`, replies need
//!    `ref_event_id` + `causation_id`, etc). Gated by [`CompatMode`] so the
//!    `from_session_id` requirement only bites once the compatibility gate is
//!    explicitly enabled (north star Builder Implication #1/#6).
//! 2. [`Deduper`] — idempotency: a duplicate `event_id`/`idempotency_key` does
//!    not create a duplicate durable fact.
//!
//! ## Charter alignment
//! Pure: no ledger writes, no clock, no spawning. `validate` returns decisions;
//! the caller (the `say` write-path, in the later integration task) decides
//! whether to reject, warn, or record. Rally facilitates; hosts execute.
//!
//! ## Staged delivery
//! `#![allow(dead_code)]`: Phase-4 module of a staged build; its surface is
//! folded into `store::Fact` + the `say` write-path during `integration-wiring`,
//! at which point the allow is removed.
#![allow(dead_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Coordination authority roles carried in an envelope's auth context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Observer,
    Agent,
    LeadAgent,
    Maintainer,
    Owner,
    System,
}

/// Authorization context carried on a privileged event.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub(crate) struct AuthContext {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// The causal + identity + auth bundle attached to a durable event. Every field
/// is optional and `#[serde(default)]` so legacy rows replay; event-kind
/// validation (not the type) decides which are mandatory for a given kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub(crate) struct EventEnvelope {
    /// Writer-supplied retry key; equal keys collapse to one durable fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The event that directly caused this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    /// The larger flow / user request this event belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// The exact prior event being acked/accepted/rejected/resolved/superseded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<String>,
    /// The live session lease that authored this write (see `session_identity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_context: Option<AuthContext>,
}

/// A required-id slot an event kind may demand.
// Variants intentionally share the `Id` suffix — they ARE id slots; the
// suffix is protocol vocabulary, not noise.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequiredId {
    FromSessionId,
    RefEventId,
    CausationId,
    ClaimId,
    HandoffId,
    WorkId,
    RunId,
    AttemptId,
}

/// Whether the `from_session_id` compatibility gate is enabled. Lenient is the
/// default for existing rooms; Strict is opt-in once every writer is upgraded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompatMode {
    /// Do not require `from_session_id` (back-compatible with pre-session rows).
    Lenient,
    /// Require `from_session_id` on every durable LLM-authored event.
    Strict,
}

/// The north-star durable event vocabulary (Optimal Durable Event Set).
#[derive(Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtocolEventKind {
    SessionRegistered,
    SessionClosed,
    SessionRevoked,
    WorkCreated,
    WorkCheckpoint,
    WorkBlocked,
    WorkResolved,
    WorkFailed,
    WorkCancelled,
    WorkAbandoned,
    WorkSuperseded,
    ClaimAcquired,
    ClaimReleased,
    ClaimExpired,
    ClaimTransferred,
    HandoffRequested,
    HandoffAcked,
    HandoffAccepted,
    HandoffRejected,
    ArtifactPublished,
    ValidationResult,
    DecisionRecorded,
    ConflictDetected,
    ConflictResolved,
    OperationIntent,
    OperationResult,
}

/// A validation failure: which required id was missing for which kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnvelopeError {
    pub kind: ProtocolEventKind,
    pub missing: RequiredId,
}

impl ProtocolEventKind {
    /// Is this a *reply* event — one that answers a specific prior event?
    /// Replies must carry `ref_event_id` + `causation_id`.
    pub(crate) fn is_reply(self) -> bool {
        matches!(
            self,
            ProtocolEventKind::HandoffAcked
                | ProtocolEventKind::HandoffAccepted
                | ProtocolEventKind::HandoffRejected
                | ProtocolEventKind::WorkResolved
                | ProtocolEventKind::WorkSuperseded
                | ProtocolEventKind::ConflictResolved
        )
    }

    /// Brainstem-authored events (session lifecycle) are exempt from the
    /// `from_session_id` requirement — they *establish* sessions.
    fn is_brainstem(self) -> bool {
        matches!(
            self,
            ProtocolEventKind::SessionRegistered
                | ProtocolEventKind::SessionClosed
                | ProtocolEventKind::SessionRevoked
        )
    }

    /// Kind-specific mandatory ids (excluding the compat-gated `from_session_id`).
    fn intrinsic_required(self) -> &'static [RequiredId] {
        use ProtocolEventKind::*;
        use RequiredId::*;
        match self {
            ClaimAcquired | ClaimReleased | ClaimExpired => &[ClaimId],
            ClaimTransferred => &[ClaimId],
            HandoffRequested => &[HandoffId],
            HandoffAcked | HandoffAccepted | HandoffRejected => {
                &[HandoffId, RefEventId, CausationId]
            }
            WorkResolved | WorkSuperseded => &[WorkId, RefEventId, CausationId],
            WorkFailed => &[WorkId, AttemptId],
            WorkCheckpoint | WorkBlocked | WorkCancelled | WorkAbandoned => &[WorkId],
            ConflictResolved => &[RefEventId, CausationId],
            _ => &[],
        }
    }

    /// Validate an envelope against this kind's required ids under `mode`.
    /// Returns every missing id (not just the first) so callers can report all.
    pub(crate) fn validate(
        self,
        env: &EventEnvelope,
        mode: CompatMode,
    ) -> Result<(), Vec<EnvelopeError>> {
        let present = |r: RequiredId| -> bool {
            match r {
                RequiredId::FromSessionId => env.from_session_id.is_some(),
                RequiredId::RefEventId => env.ref_event_id.is_some(),
                RequiredId::CausationId => env.causation_id.is_some(),
                RequiredId::ClaimId => env.claim_id.is_some(),
                RequiredId::HandoffId => env.handoff_id.is_some(),
                RequiredId::WorkId => env.work_id.is_some(),
                RequiredId::RunId => env.run_id.is_some(),
                RequiredId::AttemptId => env.attempt_id.is_some(),
            }
        };
        let mut errors = Vec::new();
        for &r in self.intrinsic_required() {
            if !present(r) {
                errors.push(EnvelopeError {
                    kind: self,
                    missing: r,
                });
            }
        }
        if mode == CompatMode::Strict && !self.is_brainstem() && !present(RequiredId::FromSessionId)
        {
            errors.push(EnvelopeError {
                kind: self,
                missing: RequiredId::FromSessionId,
            });
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Idempotency guard: collapses duplicate writer retries to one durable fact.
/// Keyed on `idempotency_key` when present, else `event_id`.
#[derive(Default)]
pub(crate) struct Deduper {
    seen: BTreeSet<String>,
}

impl Deduper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a write keyed on `idempotency_key` (preferred) or `event_id`.
    /// Returns `true` if this is the first sighting (the caller should append),
    /// `false` if it is a duplicate retry (the caller should skip).
    pub(crate) fn observe(&mut self, env: &EventEnvelope, event_id: &str) -> bool {
        let key = env
            .idempotency_key
            .clone()
            .unwrap_or_else(|| event_id.to_string());
        self.seen.insert(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EventEnvelope {
        EventEnvelope::default()
    }

    #[test]
    fn legacy_row_without_envelope_fields_deserializes() {
        // A pre-protocol fact body carries none of the new fields.
        let legacy = r#"{"subject":"old fact","extra_unknown":"ignored"}"#;
        let parsed: EventEnvelope = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            parsed,
            EventEnvelope::default(),
            "all envelope fields default to None"
        );
        assert!(parsed.from_session_id.is_none());
    }

    #[test]
    fn envelope_serializes_only_present_fields() {
        let e = EventEnvelope {
            run_id: Some("r1".into()),
            ..env()
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("run_id"));
        assert!(
            !obj.contains_key("claim_id"),
            "absent fields are skipped, not null"
        );
        assert!(!obj.contains_key("auth_context"));
    }

    #[test]
    fn claim_events_require_claim_id() {
        let e = env();
        assert!(
            ProtocolEventKind::ClaimAcquired
                .validate(&e, CompatMode::Lenient)
                .is_err()
        );
        let ok = EventEnvelope {
            claim_id: Some("claim_1".into()),
            ..env()
        };
        assert!(
            ProtocolEventKind::ClaimAcquired
                .validate(&ok, CompatMode::Lenient)
                .is_ok()
        );
    }

    #[test]
    fn handoff_request_requires_handoff_id() {
        let e = env();
        let err = ProtocolEventKind::HandoffRequested
            .validate(&e, CompatMode::Lenient)
            .unwrap_err();
        assert!(err.iter().any(|x| x.missing == RequiredId::HandoffId));
    }

    #[test]
    fn ack_and_resolve_require_ref_event_id_and_causation_id() {
        // This is the "delivered != acked, ACK cites the exact ref" invariant.
        let bare = EventEnvelope {
            handoff_id: Some("h1".into()),
            ..env()
        };
        let errs = ProtocolEventKind::HandoffAcked
            .validate(&bare, CompatMode::Lenient)
            .unwrap_err();
        let missing: Vec<_> = errs.iter().map(|e| e.missing).collect();
        assert!(missing.contains(&RequiredId::RefEventId));
        assert!(missing.contains(&RequiredId::CausationId));

        let full = EventEnvelope {
            handoff_id: Some("h1".into()),
            ref_event_id: Some("evt_orig".into()),
            causation_id: Some("evt_orig".into()),
            ..env()
        };
        assert!(
            ProtocolEventKind::HandoffAcked
                .validate(&full, CompatMode::Lenient)
                .is_ok()
        );
        assert!(ProtocolEventKind::HandoffAcked.is_reply());
    }

    #[test]
    fn strict_mode_requires_from_session_id_but_lenient_does_not() {
        let e = EventEnvelope {
            claim_id: Some("c1".into()),
            ..env()
        };
        assert!(
            ProtocolEventKind::ClaimAcquired
                .validate(&e, CompatMode::Lenient)
                .is_ok(),
            "lenient tolerates missing from_session_id (back-compat)"
        );
        let errs = ProtocolEventKind::ClaimAcquired
            .validate(&e, CompatMode::Strict)
            .unwrap_err();
        assert!(errs.iter().any(|x| x.missing == RequiredId::FromSessionId));
        let with = EventEnvelope {
            from_session_id: Some("sess:proc:h:1#L".into()),
            ..e
        };
        assert!(
            ProtocolEventKind::ClaimAcquired
                .validate(&with, CompatMode::Strict)
                .is_ok()
        );
    }

    #[test]
    fn brainstem_session_events_exempt_from_from_session_id_even_in_strict() {
        // SessionRegistered ESTABLISHES the session; it can't cite one.
        assert!(
            ProtocolEventKind::SessionRegistered
                .validate(&env(), CompatMode::Strict)
                .is_ok()
        );
    }

    #[test]
    fn duplicate_idempotency_key_collapses_to_one() {
        let mut d = Deduper::new();
        let e = EventEnvelope {
            idempotency_key: Some("retry-42".into()),
            ..env()
        };
        assert!(d.observe(&e, "evt_a"), "first sighting appends");
        // Same key, different event_id (a retry) → duplicate, skip.
        assert!(
            !d.observe(&e, "evt_b"),
            "duplicate idempotency_key is suppressed"
        );
        // No key → falls back to event_id; distinct events are distinct.
        let n = env();
        assert!(d.observe(&n, "evt_c"));
        assert!(!d.observe(&n, "evt_c"), "same event_id is a duplicate");
        assert!(d.observe(&n, "evt_d"));
    }

    #[test]
    fn validate_reports_all_missing_ids_not_just_first() {
        let errs = ProtocolEventKind::WorkResolved
            .validate(&env(), CompatMode::Lenient)
            .unwrap_err();
        let missing: BTreeSet<_> = errs.iter().map(|e| format!("{:?}", e.missing)).collect();
        // WorkResolved needs work_id + ref_event_id + causation_id.
        assert_eq!(missing.len(), 3);
    }

    #[test]
    fn auth_context_round_trips_through_json() {
        let ctx = AuthContext {
            role: Role::Maintainer,
            policy_version: Some("policy_7".into()),
            capabilities: vec!["push".into()],
        };
        let s = serde_json::to_string(&ctx).unwrap();
        let back: AuthContext = serde_json::from_str(&s).unwrap();
        assert_eq!(ctx, back);
        assert!(s.contains("maintainer"));
    }
}
