// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use rally_protocol::delivery::{
    AdapterCapabilities, AdapterKind, AuthContext, CAPABILITY_IDEMPOTENT_DELIVERY,
    CAPABILITY_POSITIVE_ACK, CompatMode, Deduper, DeliveryAttemptOutcome, DeliveryAttemptV1,
    DeliveryContractError, DeliveryEnvelopeV1, DeliveryEvidenceV1, DeliveryReceiptV1,
    DeliveryState, EndpointDescriptorV1, EnvelopeError, EventEnvelope, EvidenceExpectation,
    EvidenceSource, EvidenceValidationError, ProtocolEventKind, RequiredId, Role,
    stable_delivery_dedupe_key,
};
use rally_protocol::{DeliveryStatus, Directive, DirectiveKind, InterruptType, Receipt};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FrozenDirectiveKindV1 {
    Deliver,
    Read,
    Stop,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FrozenInterruptTypeV1 {
    Addition,
    Revision,
    Retraction,
}

#[derive(Debug, Deserialize, PartialEq)]
struct FrozenDirectiveV1 {
    seq: u64,
    to: String,
    from: String,
    kind: FrozenDirectiveKindV1,
    #[serde(rename = "type")]
    itype: FrozenInterruptTypeV1,
    text: Option<String>,
    #[serde(default)]
    urgent: bool,
    ts: f64,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FrozenDeliveryStatusV1 {
    Pending,
    Delivered,
    Seen,
    Acted,
    Failed,
}

#[derive(Debug, Deserialize, PartialEq)]
struct FrozenReceiptV1 {
    ref_seq: u64,
    to: String,
    status: FrozenDeliveryStatusV1,
    by: String,
    evidence: Option<String>,
    error: Option<String>,
    ts: f64,
}

fn directive() -> Directive {
    Directive {
        seq: 7,
        to: "codex:worker-01".into(),
        from: "claude_code:lead-01".into(),
        message: Default::default(),
        kind: DirectiveKind::Deliver,
        itype: InterruptType::Addition,
        text: Some("Implement the parser".into()),
        urgent: false,
        ts: 1_786_300_000.25,
    }
}

fn current_envelope() -> DeliveryEnvelopeV1 {
    let mut envelope = DeliveryEnvelopeV1::new(
        directive(),
        "event-01",
        "repo-agent-rally-point",
        "sha256:abc123",
    );
    envelope.context.correlation_id = Some("flow-01".into());
    envelope.context.from_session_id = Some("sess:codex-01".into());
    envelope.required_fields = BTreeSet::from([
        "event_id".into(),
        "idempotency_key".into(),
        "correlation_id".into(),
        "extensions.provider".into(),
    ]);
    envelope.required_capabilities = BTreeSet::from(["positive_ack".into()]);
    envelope.extensions.insert(
        "provider".into(),
        json!({"name": "codex-app-server", "opaque": [1, 2, 3]}),
    );
    envelope
}

fn receipt() -> Receipt {
    Receipt {
        ref_seq: 7,
        to: "codex:worker-01".into(),
        status: DeliveryStatus::Delivered,
        by: "ptyd".into(),
        evidence: Some("bytes-written".into()),
        error: None,
        ts: 1_786_300_001.0,
    }
}

fn expected_evidence() -> EvidenceExpectation {
    EvidenceExpectation {
        event_id: "event-01".into(),
        target_agent_id: "codex:worker-01".into(),
        target_session_id: Some("sess:codex-01".into()),
        correlation_id: Some("flow-01".into()),
        handoff_id: Some("handoff-01".into()),
    }
}

fn evidence(source: EvidenceSource, state: DeliveryState) -> DeliveryEvidenceV1 {
    DeliveryEvidenceV1 {
        delivery_evidence_schema: "rally.delivery-evidence.v1".into(),
        event_id: "event-01".into(),
        attempt_id: Some("attempt-01".into()),
        state,
        source,
        author_agent_id: Some("codex:worker-01".into()),
        author_session_id: Some("sess:codex-01".into()),
        correlation_id: Some("flow-01".into()),
        handoff_id: Some("handoff-01".into()),
        extensions: BTreeMap::new(),
    }
}

#[test]
fn frozen_v1_reader_accepts_new_writer_flat_json() {
    let envelope = current_envelope();
    let value = serde_json::to_value(&envelope).unwrap();
    assert!(
        value.get("directive").is_none(),
        "legacy fields must stay flat"
    );
    assert_eq!(value["to"], "codex:worker-01");
    assert_eq!(value["event_id"], "event-01");

    let legacy: FrozenDirectiveV1 = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.seq, 7);
    assert_eq!(legacy.to, "codex:worker-01");
    assert_eq!(legacy.kind, FrozenDirectiveKindV1::Deliver);
    assert_eq!(legacy.itype, FrozenInterruptTypeV1::Addition);
}

#[test]
fn new_reader_accepts_golden_v1_directive() {
    let golden = r#"{
        "seq": 4,
        "to": "claude_code:01",
        "from": "codex:lead",
        "kind": "deliver",
        "type": "revision",
        "text": "Use the new schema",
        "urgent": false,
        "ts": 1786300000.5
    }"#;
    let envelope: DeliveryEnvelopeV1 = serde_json::from_str(golden).unwrap();
    assert_eq!(envelope.directive.seq, 4);
    assert_eq!(envelope.directive.itype, InterruptType::Revision);
    assert!(envelope.event_id.is_none());
    let errors = envelope.validate_current().unwrap_err();
    assert!(
        errors.contains(&DeliveryContractError::MissingRequiredField(
            "event_id".into()
        ))
    );
}

#[test]
fn opaque_extensions_and_unknown_capabilities_survive_round_trip() {
    let envelope = current_envelope();
    let encoded = serde_json::to_string(&envelope).unwrap();
    let decoded: DeliveryEnvelopeV1 = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.extensions, envelope.extensions);
    assert_eq!(
        decoded.required_capabilities,
        envelope.required_capabilities
    );
    assert_eq!(decoded.content_digest, envelope.content_digest);
    assert!(decoded.validate_current().is_ok());

    let mut capabilities = AdapterCapabilities::default();
    capabilities.features.insert("future-capability-x".into());
    capabilities
        .extensions
        .insert("provider".into(), json!({"unknown": true}));
    let encoded = serde_json::to_string(&capabilities).unwrap();
    let decoded: AdapterCapabilities = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, capabilities);

    let adapter: AdapterKind = serde_json::from_str("\"future-native-y\"").unwrap();
    assert_eq!(adapter.as_str(), "future-native-y");
}

#[test]
fn new_typed_records_require_schemas_session_and_expiry() {
    let mut evidence_value = serde_json::to_value(evidence(
        EvidenceSource::VerifiedTarget,
        DeliveryState::TargetAcknowledged,
    ))
    .unwrap();
    evidence_value
        .as_object_mut()
        .unwrap()
        .remove("delivery_evidence_schema");
    assert!(serde_json::from_value::<DeliveryEvidenceV1>(evidence_value).is_err());

    let endpoint = json!({
        "endpoint_schema": "rally.endpoint.v1",
        "repository_id": "repo-agent-rally-point",
        "agent_id": "codex:worker-01",
        "session_id": "sess:codex-01",
        "endpoint_id": "codex-native-01",
        "adapter": "codex-app-server",
        "health": "ready",
        "observed_at": 1_786_300_000.0,
        "expires_at": 1_786_300_060.0
    });
    for required in ["endpoint_schema", "session_id", "expires_at"] {
        let mut missing = endpoint.clone();
        missing.as_object_mut().unwrap().remove(required);
        assert!(
            serde_json::from_value::<EndpointDescriptorV1>(missing).is_err(),
            "endpoint accepted missing required field {required}"
        );
    }

    let attempt = DeliveryAttemptV1::new(
        "event-01",
        "dedupe-01",
        1,
        "codex-native-01",
        DeliveryAttemptOutcome::Planned,
        false,
    );
    let mut attempt_value = serde_json::to_value(attempt).unwrap();
    attempt_value
        .as_object_mut()
        .unwrap()
        .remove("delivery_attempt_schema");
    assert!(serde_json::from_value::<DeliveryAttemptV1>(attempt_value).is_err());
}

#[test]
fn reserved_capabilities_use_typed_truth_and_expose_conflicts() {
    let mut capabilities = AdapterCapabilities {
        supports_positive_ack: true,
        ..Default::default()
    };
    assert!(capabilities.supports(CAPABILITY_POSITIVE_ACK));
    assert!(!capabilities.supports(CAPABILITY_IDEMPOTENT_DELIVERY));
    assert_eq!(capabilities.conflicting_reserved_capability(), None);

    capabilities
        .features
        .insert(CAPABILITY_IDEMPOTENT_DELIVERY.into());
    assert!(!capabilities.supports(CAPABILITY_IDEMPOTENT_DELIVERY));
    assert_eq!(
        capabilities.conflicting_reserved_capability(),
        Some(CAPABILITY_IDEMPOTENT_DELIVERY)
    );
}

#[test]
fn required_field_validation_rejects_missing_and_unknown_semantics() {
    let mut missing = current_envelope();
    missing.context.correlation_id = None;
    let errors = missing.validate_current().unwrap_err();
    assert!(
        errors.contains(&DeliveryContractError::MissingRequiredField(
            "correlation_id".into()
        ))
    );

    let mut unknown = current_envelope();
    unknown.required_fields.insert("provider_magic".into());
    let errors = unknown.validate_current().unwrap_err();
    assert!(
        errors.contains(&DeliveryContractError::UnsupportedRequiredField(
            "provider_magic".into()
        ))
    );

    for field in ["from", "to"] {
        let mut invalid = current_envelope();
        if field == "from" {
            invalid.directive.from = "  ".into();
        } else {
            invalid.directive.to = "  ".into();
        }
        assert!(
            invalid
                .validate_current()
                .unwrap_err()
                .contains(&DeliveryContractError::MissingRequiredField(field.into()))
        );
    }
}

#[test]
fn logical_dedupe_key_is_stable_while_attempt_ids_change() {
    let envelope = current_envelope();
    let dedupe = envelope.delivery_dedupe_key().unwrap().to_string();
    assert_eq!(
        dedupe,
        stable_delivery_dedupe_key("event-01", "repo-agent-rally-point", "codex:worker-01")
    );
    let first = DeliveryAttemptV1::new(
        "event-01",
        dedupe.clone(),
        1,
        "endpoint-a",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    let second = DeliveryAttemptV1::new(
        "event-01",
        dedupe.clone(),
        2,
        "endpoint-b",
        DeliveryAttemptOutcome::Planned,
        false,
    );
    let same_number_other_endpoint = DeliveryAttemptV1::new(
        "event-01",
        dedupe.clone(),
        1,
        "endpoint-b|with:delimiters",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    assert_eq!(first.delivery_dedupe_key, second.delivery_dedupe_key);
    assert_ne!(first.attempt_id, second.attempt_id);
    assert_ne!(first.attempt_id, same_number_other_endpoint.attempt_id);
    assert!(first.permits_same_endpoint_retry());
    assert!(!second.permits_same_endpoint_retry());
}

#[test]
fn dedupe_key_mismatch_is_rejected() {
    let mut envelope = current_envelope();
    envelope.context.idempotency_key = Some("attempt-specific-key".into());
    assert!(matches!(
        envelope.validate_current().unwrap_err().as_slice(),
        [DeliveryContractError::DedupeKeyMismatch { .. }]
    ));
}

#[test]
fn frozen_v1_receipt_reader_accepts_typed_receipt() {
    let mut wrapped = DeliveryReceiptV1::from_legacy(receipt());
    wrapped.delivery_evidence = Some(evidence(
        EvidenceSource::Transport,
        DeliveryState::TransportSent,
    ));
    let value = serde_json::to_value(wrapped).unwrap();
    assert!(
        value.get("receipt").is_none(),
        "legacy fields must stay flat"
    );
    let legacy: FrozenReceiptV1 = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.ref_seq, 7);
    assert_eq!(legacy.status, FrozenDeliveryStatusV1::Delivered);
    assert_eq!(legacy.by, "ptyd");
}

#[test]
fn new_receipt_reader_accepts_golden_v1_receipt_without_promotion() {
    let golden = r#"{
        "ref_seq": 9,
        "to": "codex:01",
        "status": "acted",
        "by": "codex:01",
        "evidence": "self asserted",
        "ts": 1786300001.0
    }"#;
    let wrapped: DeliveryReceiptV1 = serde_json::from_str(golden).unwrap();
    assert_eq!(wrapped.receipt.status, DeliveryStatus::Acted);
    assert!(wrapped.delivery_evidence.is_none());
}

#[test]
fn sender_and_transport_cannot_promote_ack_or_completion() {
    for source in [EvidenceSource::Sender, EvidenceSource::Transport] {
        for state in [DeliveryState::TargetAcknowledged, DeliveryState::Completed] {
            let err = evidence(source, state)
                .validate_against(&expected_evidence())
                .unwrap_err();
            assert_eq!(
                err,
                EvidenceValidationError::UnauthorizedPromotion { state, source }
            );
        }
    }
}

#[test]
fn evidence_with_an_unknown_schema_is_rejected_before_semantic_promotion() {
    let mut value = evidence(EvidenceSource::VerifiedTarget, DeliveryState::Completed);
    value.delivery_evidence_schema = "rally.delivery-evidence.v999".into();
    assert_eq!(
        value.validate_against(&expected_evidence()).unwrap_err(),
        EvidenceValidationError::UnsupportedSchema("rally.delivery-evidence.v999".into())
    );
}

#[test]
fn asserted_and_verified_target_evidence_remain_distinct() {
    let asserted = evidence(
        EvidenceSource::AssertedTarget,
        DeliveryState::TargetAcknowledged,
    );
    assert!(asserted.validate_against(&expected_evidence()).is_ok());
    assert_eq!(asserted.source, EvidenceSource::AssertedTarget);

    let verified = evidence(EvidenceSource::VerifiedTarget, DeliveryState::Completed);
    assert!(verified.validate_against(&expected_evidence()).is_ok());
    assert_eq!(verified.source, EvidenceSource::VerifiedTarget);
}

#[test]
fn verified_target_requires_an_exact_verified_session_expectation() {
    let mut expected = expected_evidence();
    expected.target_session_id = None;
    let verified = evidence(EvidenceSource::VerifiedTarget, DeliveryState::Completed);
    assert_eq!(
        verified.validate_against(&expected),
        Err(EvidenceValidationError::SessionMismatch)
    );
}

#[test]
fn verified_target_identity_binding_applies_to_every_delivery_state() {
    for state in [
        DeliveryState::Recorded,
        DeliveryState::RouteSelected,
        DeliveryState::TransportSent,
        DeliveryState::TargetAcknowledged,
        DeliveryState::Working,
        DeliveryState::Completed,
        DeliveryState::Failed,
        DeliveryState::OutcomeUnknown,
    ] {
        let mut wrong_agent = evidence(EvidenceSource::VerifiedTarget, state);
        wrong_agent.author_agent_id = Some("codex:other".into());
        assert_eq!(
            wrong_agent.validate_against(&expected_evidence()),
            Err(EvidenceValidationError::TargetMismatch)
        );

        let mut wrong_session = evidence(EvidenceSource::VerifiedTarget, state);
        wrong_session.author_session_id = Some("sess:other".into());
        assert_eq!(
            wrong_session.validate_against(&expected_evidence()),
            Err(EvidenceValidationError::SessionMismatch)
        );
    }
}

#[test]
fn target_session_and_correlations_must_match() {
    let cases = [
        (
            {
                let mut value = evidence(
                    EvidenceSource::AssertedTarget,
                    DeliveryState::TargetAcknowledged,
                );
                value.event_id = "wrong".into();
                value
            },
            EvidenceValidationError::EventMismatch,
        ),
        (
            {
                let mut value = evidence(
                    EvidenceSource::AssertedTarget,
                    DeliveryState::TargetAcknowledged,
                );
                value.author_agent_id = Some("wrong".into());
                value
            },
            EvidenceValidationError::TargetMismatch,
        ),
        (
            {
                let mut value = evidence(
                    EvidenceSource::AssertedTarget,
                    DeliveryState::TargetAcknowledged,
                );
                value.author_session_id = Some("wrong".into());
                value
            },
            EvidenceValidationError::SessionMismatch,
        ),
        (
            {
                let mut value = evidence(
                    EvidenceSource::AssertedTarget,
                    DeliveryState::TargetAcknowledged,
                );
                value.correlation_id = Some("wrong".into());
                value
            },
            EvidenceValidationError::CorrelationMismatch,
        ),
        (
            {
                let mut value = evidence(
                    EvidenceSource::AssertedTarget,
                    DeliveryState::TargetAcknowledged,
                );
                value.handoff_id = Some("wrong".into());
                value
            },
            EvidenceValidationError::HandoffMismatch,
        ),
    ];
    for (value, expected) in cases {
        assert_eq!(value.validate_against(&expected_evidence()), Err(expected));
    }
}

#[test]
fn post_send_unknown_allows_only_same_endpoint_retry_with_target_dedupe() {
    let unsafe_attempt = DeliveryAttemptV1::new(
        "event-01",
        "dedupe",
        1,
        "ptyd-01",
        DeliveryAttemptOutcome::OutcomeUnknown,
        false,
    );
    let safe_attempt = DeliveryAttemptV1 {
        idempotent_delivery: true,
        ..unsafe_attempt.clone()
    };
    assert!(!unsafe_attempt.permits_same_endpoint_retry());
    assert!(safe_attempt.permits_same_endpoint_retry());
}

#[test]
fn moved_event_validation_preserves_cli_semantics() {
    let incomplete = EventEnvelope {
        handoff_id: Some("handoff-01".into()),
        ..Default::default()
    };
    let errors = ProtocolEventKind::HandoffAcked
        .validate(&incomplete, CompatMode::Lenient)
        .unwrap_err();
    assert_eq!(
        errors,
        vec![
            EnvelopeError {
                kind: ProtocolEventKind::HandoffAcked,
                missing: RequiredId::RefEventId,
            },
            EnvelopeError {
                kind: ProtocolEventKind::HandoffAcked,
                missing: RequiredId::CausationId,
            },
        ]
    );
    assert!(ProtocolEventKind::HandoffAcked.is_reply());
    assert!(ProtocolEventKind::WorkResolved.is_reply());
    assert!(!ProtocolEventKind::HandoffRequested.is_reply());

    let strict = ProtocolEventKind::DecisionRecorded
        .validate(&EventEnvelope::default(), CompatMode::Strict)
        .unwrap_err();
    assert_eq!(strict[0].missing, RequiredId::FromSessionId);
}

#[test]
fn moved_event_validation_preserves_v1_option_presence_semantics() {
    let envelope = EventEnvelope {
        claim_id: Some(String::new()),
        ..EventEnvelope::default()
    };
    assert!(
        ProtocolEventKind::ClaimAcquired
            .validate(&envelope, CompatMode::Lenient)
            .is_ok(),
        "the live v1 CLI validator treated Some(empty) as present"
    );
}

#[test]
fn moved_event_context_preserves_legacy_defaults_and_sparse_json() {
    let parsed: EventEnvelope =
        serde_json::from_str(r#"{"subject":"old fact","extra_unknown":"ignored"}"#).unwrap();
    assert_eq!(parsed, EventEnvelope::default());

    let sparse = EventEnvelope {
        run_id: Some("run-1".into()),
        ..EventEnvelope::default()
    };
    let encoded = serde_json::to_value(sparse).unwrap();
    assert_eq!(encoded["run_id"], "run-1");
    assert!(encoded.get("claim_id").is_none());
    assert!(encoded.get("auth_context").is_none());
}

#[test]
fn moved_event_context_preserves_deduplication_and_auth_round_trip() {
    let mut deduper = Deduper::new();
    let keyed = EventEnvelope {
        idempotency_key: Some("retry-42".into()),
        ..EventEnvelope::default()
    };
    assert!(deduper.observe(&keyed, "event-a"));
    assert!(!deduper.observe(&keyed, "event-b"));
    assert!(deduper.observe(&EventEnvelope::default(), "event-c"));
    assert!(!deduper.observe(&EventEnvelope::default(), "event-c"));

    let auth = AuthContext {
        role: Role::Maintainer,
        policy_version: Some("policy-7".into()),
        capabilities: vec!["push".into()],
    };
    let encoded = serde_json::to_string(&auth).unwrap();
    assert_eq!(serde_json::from_str::<AuthContext>(&encoded).unwrap(), auth);
}

#[test]
fn moved_event_validation_preserves_brainstem_and_all_error_semantics() {
    assert!(
        ProtocolEventKind::SessionRegistered
            .validate(&EventEnvelope::default(), CompatMode::Strict)
            .is_ok()
    );
    let errors = ProtocolEventKind::WorkResolved
        .validate(&EventEnvelope::default(), CompatMode::Lenient)
        .unwrap_err();
    assert_eq!(
        errors
            .iter()
            .map(|error| format!("{:?}", error.missing))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "WorkId".to_string(),
            "RefEventId".to_string(),
            "CausationId".to_string(),
        ])
    );
}
