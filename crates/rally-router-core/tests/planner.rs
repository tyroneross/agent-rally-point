// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use rally_protocol::delivery::{
    AdapterCapabilities, AdapterKind, DELIVERY_SCHEMA_V1, DeliveryAttemptOutcome,
    DeliveryAttemptV1, DeliveryEnvelopeV1, ENDPOINT_SCHEMA_V1, EndpointDescriptorV1,
    EndpointHealth, stable_delivery_dedupe_key,
};
use rally_protocol::{Directive, DirectiveKind, InterruptType};
use rally_router_core::{
    CandidateDecision, CurrentRoute, DeliveryPlanner, DeliveryTarget, RouteDecisionV1,
    RouteHoldReason, RoutePolicy, RouteRejectionReason, ShadowComparisonResult,
    ShadowDifferenceReason, compare_shadow,
};

const NOW: f64 = 1_900_000_000.0;

fn envelope() -> DeliveryEnvelopeV1 {
    DeliveryEnvelopeV1::new(
        Directive {
            seq: 1,
            from: "claude:lead".into(),
            to: "codex:worker".into(),
            message: Default::default(),
            kind: DirectiveKind::Deliver,
            itype: InterruptType::Addition,
            text: Some("Build the pure planner".into()),
            urgent: false,
            ts: NOW - 1.0,
        },
        "event-1",
        "repo-rally",
        "sha256:content",
    )
}

fn target() -> DeliveryTarget {
    DeliveryTarget {
        repository_id: "repo-rally".into(),
        agent_id: "codex:worker".into(),
        session_id: "session-target".into(),
        pinned_endpoint_id: None,
    }
}

fn endpoint(id: &str, adapter: &str) -> EndpointDescriptorV1 {
    EndpointDescriptorV1 {
        endpoint_schema: ENDPOINT_SCHEMA_V1.into(),
        repository_id: "repo-rally".into(),
        agent_id: "codex:worker".into(),
        session_id: "session-target".into(),
        endpoint_id: id.into(),
        adapter: AdapterKind::from(adapter),
        capabilities: AdapterCapabilities {
            preserves_fields: BTreeSet::new(),
            features: BTreeSet::new(),
            supports_positive_ack: false,
            supports_idempotent_delivery: false,
            extensions: BTreeMap::new(),
        },
        health: EndpointHealth::Ready,
        ambiguous: false,
        observed_at: NOW - 1.0,
        expires_at: NOW + 60.0,
        accepted_schemas: BTreeSet::from([DELIVERY_SCHEMA_V1.into()]),
        principal_id: None,
        extensions: BTreeMap::new(),
    }
}

fn selected(plan: &rally_router_core::RoutePlanV1) -> (&str, &str) {
    match &plan.decision {
        RouteDecisionV1::Selected {
            endpoint_id,
            adapter,
        } => (endpoint_id, adapter.as_str()),
        other => panic!("expected selected plan, got {other:?}"),
    }
}

fn rejection<'a>(
    plan: &'a rally_router_core::RoutePlanV1,
    endpoint_id: &str,
) -> &'a RouteRejectionReason {
    let evaluation = plan
        .candidates
        .iter()
        .find(|candidate| candidate.endpoint_id == endpoint_id)
        .expect("candidate exists");
    match &evaluation.decision {
        CandidateDecision::Rejected(reason) => reason,
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn resolves_exact_repository_and_recipient_before_preference() {
    let mut wrong_repo = endpoint("native-wrong-repo", "codex-app-server");
    wrong_repo.repository_id = "another-repo".into();
    let mut wrong_agent = endpoint("native-wrong-agent", "codex-app-server");
    wrong_agent.agent_id = "codex:other".into();
    let fallback = endpoint("ptyd-exact", "ptyd");

    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[wrong_repo, wrong_agent, fallback],
        &RoutePolicy::default(),
        &[],
    );

    assert_eq!(selected(&plan), ("ptyd-exact", "ptyd"));
    assert_eq!(
        rejection(&plan, "native-wrong-repo"),
        &RouteRejectionReason::RepositoryMismatch
    );
    assert_eq!(
        rejection(&plan, "native-wrong-agent"),
        &RouteRejectionReason::RecipientMismatch
    );
}

#[test]
fn target_mismatch_queues_without_evaluating_endpoints() {
    let mut wrong_target = target();
    wrong_target.agent_id = "codex:other".into();
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &wrong_target,
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::RecipientMismatch
        }
    );
    assert!(plan.candidates.is_empty());
}

#[test]
fn target_session_is_required_before_evaluating_endpoints() {
    let mut encoded = serde_json::to_value(target()).unwrap();
    encoded.as_object_mut().unwrap().remove("session_id");
    assert!(serde_json::from_value::<DeliveryTarget>(encoded).is_err());

    let mut missing_session = target();
    missing_session.session_id = "  ".into();
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &missing_session,
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::InvalidTargetSession
        }
    );
    assert!(plan.candidates.is_empty());
}

#[test]
fn missing_or_wrong_endpoint_session_fails_closed() {
    let mut missing = endpoint("missing-session", "codex-app-server");
    missing.session_id.clear();
    let mut wrong = endpoint("wrong-session", "ptyd");
    wrong.session_id = "session-other".into();
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[missing, wrong],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::NoEligibleEndpoint
        }
    );
    for endpoint_id in ["missing-session", "wrong-session"] {
        assert_eq!(
            rejection(&plan, endpoint_id),
            &RouteRejectionReason::SessionMismatch
        );
    }
}

#[test]
fn pinned_endpoint_wins_without_reordering_policy() {
    let mut pinned = target();
    pinned.pinned_endpoint_id = Some("ptyd".into());
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &pinned,
        &[
            endpoint("native", "codex-app-server"),
            endpoint("ptyd", "ptyd"),
        ],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(selected(&plan), ("ptyd", "ptyd"));
    assert_eq!(
        rejection(&plan, "native"),
        &RouteRejectionReason::PinnedEndpointMismatch
    );
}

#[test]
fn ambiguous_expired_and_duplicate_endpoints_fail_closed() {
    let mut ambiguous = endpoint("ambiguous", "codex-app-server");
    ambiguous.ambiguous = true;
    let mut expired = endpoint("expired", "codex-app-server");
    expired.expires_at = NOW;
    let duplicate_a = endpoint("duplicate", "codex-app-server");
    let duplicate_b = endpoint("duplicate", "ptyd");
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[ambiguous, expired, duplicate_a, duplicate_b],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::NoEligibleEndpoint
        }
    );
    assert_eq!(
        rejection(&plan, "ambiguous"),
        &RouteRejectionReason::AmbiguousEndpoint
    );
    assert_eq!(rejection(&plan, "expired"), &RouteRejectionReason::Expired);
    assert!(
        plan.candidates
            .iter()
            .filter(|candidate| candidate.endpoint_id == "duplicate")
            .all(|candidate| {
                candidate.decision
                    == CandidateDecision::Rejected(RouteRejectionReason::DuplicateEndpointId)
            })
    );
}

#[test]
fn invalid_planner_and_endpoint_times_fail_closed() {
    let invalid_now = DeliveryPlanner.plan(
        f64::NAN,
        &envelope(),
        &target(),
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        invalid_now.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::InvalidPlannerTime
        }
    );

    let mut empty_id = endpoint("placeholder", "codex-app-server");
    empty_id.endpoint_id = " ".into();
    let mut invalid_observation = endpoint("nan-observation", "codex-app-server");
    invalid_observation.observed_at = f64::NAN;
    let mut invalid_expiry = endpoint("nan-expiry", "codex-app-server");
    invalid_expiry.expires_at = f64::INFINITY;
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[empty_id, invalid_observation, invalid_expiry],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        rejection(&plan, " "),
        &RouteRejectionReason::EmptyEndpointId
    );
    assert_eq!(
        rejection(&plan, "nan-observation"),
        &RouteRejectionReason::InvalidObservationTime
    );
    assert_eq!(
        rejection(&plan, "nan-expiry"),
        &RouteRejectionReason::InvalidObservationTime
    );
}

#[test]
fn schema_fields_capabilities_and_ack_policy_are_enforced() {
    let mut message = envelope();
    message.required_fields.insert("correlation_id".into());
    message.context.correlation_id = Some("flow-1".into());
    message.required_capabilities.insert("interrupt".into());

    let mut wrong_schema = endpoint("schema", "codex-app-server");
    wrong_schema.accepted_schemas.clear();
    let missing_field = endpoint("field", "codex-app-server");
    let mut missing_capability = endpoint("capability", "codex-app-server");
    missing_capability
        .capabilities
        .preserves_fields
        .insert("correlation_id".into());
    let mut missing_ack = endpoint("ack", "codex-app-server");
    missing_ack
        .capabilities
        .preserves_fields
        .insert("correlation_id".into());
    missing_ack.capabilities.features.insert("interrupt".into());

    let policy = RoutePolicy {
        require_positive_ack: true,
        ..RoutePolicy::default()
    };
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[wrong_schema, missing_field, missing_capability, missing_ack],
        &policy,
        &[],
    );

    assert_eq!(
        rejection(&plan, "schema"),
        &RouteRejectionReason::UnsupportedDeliverySchema(DELIVERY_SCHEMA_V1.into())
    );
    assert_eq!(
        rejection(&plan, "field"),
        &RouteRejectionReason::MissingRequiredField("correlation_id".into())
    );
    assert_eq!(
        rejection(&plan, "capability"),
        &RouteRejectionReason::MissingCapability("interrupt".into())
    );
    assert_eq!(
        rejection(&plan, "ack"),
        &RouteRejectionReason::PositiveAckRequired
    );
}

#[test]
fn reserved_capabilities_use_typed_fields_and_reject_conflicts() {
    for (capability, positive_ack) in [("positive_ack", true), ("idempotent_delivery", false)] {
        let mut message = envelope();
        message.required_capabilities.insert(capability.into());

        let missing = endpoint("missing", "codex-app-server");
        let mut typed = endpoint("typed", "codex-app-server");
        if positive_ack {
            typed.capabilities.supports_positive_ack = true;
        } else {
            typed.capabilities.supports_idempotent_delivery = true;
        }
        let mut conflict = endpoint("conflict", "codex-app-server");
        conflict.capabilities.features.insert(capability.into());

        let plan = DeliveryPlanner.plan(
            NOW,
            &message,
            &target(),
            &[missing, typed, conflict],
            &RoutePolicy::default(),
            &[],
        );
        assert_eq!(selected(&plan), ("typed", "codex-app-server"));
        assert_eq!(
            rejection(&plan, "missing"),
            &RouteRejectionReason::MissingCapability(capability.into())
        );
        assert_eq!(
            rejection(&plan, "conflict"),
            &RouteRejectionReason::ConflictingCapability(capability.into())
        );
    }
}

#[test]
fn default_order_prefers_native_then_a2a_then_ptyd_then_mux() {
    let endpoints = [
        endpoint("cmux", "cmux"),
        endpoint("tmux", "tmux"),
        endpoint("ptyd", "ptyd"),
        endpoint("a2a", "a2a"),
        endpoint("opencode", "opencode-server"),
        endpoint("codex", "codex-app-server"),
    ];
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &endpoints,
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(selected(&plan), ("codex", "codex-app-server"));
}

#[test]
fn positive_ack_breaks_ties_deterministically() {
    let no_ack = endpoint("a-no-ack", "codex-app-server");
    let mut ack = endpoint("z-ack", "codex-app-server");
    ack.capabilities.supports_positive_ack = true;
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[no_ack, ack],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(selected(&plan), ("z-ack", "codex-app-server"));
}

#[test]
fn planner_output_is_invariant_to_endpoint_and_attempt_order() {
    let endpoints = [
        endpoint("ptyd", "ptyd"),
        endpoint("native", "codex-app-server"),
        endpoint("a2a", "a2a"),
    ];
    let mut reversed = endpoints.clone();
    reversed.reverse();

    let message = envelope();
    let attempts = [
        DeliveryAttemptV1::new(
            "event-1",
            message.delivery_dedupe_key().unwrap(),
            1,
            "native",
            DeliveryAttemptOutcome::FailedBeforeSend,
            false,
        ),
        DeliveryAttemptV1::new(
            "event-1",
            message.delivery_dedupe_key().unwrap(),
            2,
            "a2a",
            DeliveryAttemptOutcome::FailedBeforeSend,
            false,
        ),
    ];
    let mut reversed_attempts = attempts.clone();
    reversed_attempts.reverse();

    let plan_a = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &endpoints,
        &RoutePolicy::default(),
        &attempts,
    );
    let plan_b = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &reversed,
        &RoutePolicy::default(),
        &reversed_attempts,
    );
    assert_eq!(plan_a, plan_b);
    assert_eq!(
        serde_json::to_value(plan_a).unwrap(),
        serde_json::to_value(plan_b).unwrap()
    );
}

#[test]
fn failed_attempt_advances_to_next_endpoint() {
    let message = envelope();
    let attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[
            endpoint("native", "codex-app-server"),
            endpoint("ptyd", "ptyd"),
        ],
        &RoutePolicy::default(),
        &[attempt],
    );
    assert_eq!(selected(&plan), ("ptyd", "ptyd"));
    assert_eq!(
        rejection(&plan, "native"),
        &RouteRejectionReason::PriorAttemptUsed
    );
}

#[test]
fn forged_attempt_identity_cannot_suppress_or_redirect_delivery() {
    let message = envelope();
    let mut attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native",
        DeliveryAttemptOutcome::Completed,
        true,
    );
    attempt.attempt_id = "forged-completion".into();
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        &[attempt],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::InvalidAttemptIdentity("forged-completion".into())
        }
    );
}

#[test]
fn endpoint_tampering_invalidates_attempt_identity() {
    let message = envelope();
    let mut attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native-original",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    attempt.endpoint_id = "native-tampered".into();
    let attempt_id = attempt.attempt_id.clone();
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[endpoint("native-tampered", "codex-app-server")],
        &RoutePolicy::default(),
        &[attempt],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::InvalidAttemptIdentity(attempt_id)
        }
    );
}

#[test]
fn duplicate_or_illegal_attempt_history_fails_closed_in_any_order() {
    let message = envelope();
    let first = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native-a",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    let duplicate_number = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native-b",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );
    let mut conflicting_outcome = first.clone();
    conflicting_outcome.outcome = DeliveryAttemptOutcome::Completed;
    let second_after_terminal = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        2,
        "native-a",
        DeliveryAttemptOutcome::FailedBeforeSend,
        false,
    );

    let cases = [
        vec![first.clone(), duplicate_number],
        vec![first.clone(), conflicting_outcome],
        vec![
            DeliveryAttemptV1 {
                outcome: DeliveryAttemptOutcome::Planned,
                ..first.clone()
            },
            second_after_terminal,
        ],
    ];
    for history in cases {
        for candidate_history in [history.clone(), history.into_iter().rev().collect()] {
            let plan = DeliveryPlanner.plan(
                NOW,
                &message,
                &target(),
                &[endpoint("native", "codex-app-server")],
                &RoutePolicy::default(),
                &candidate_history,
            );
            assert!(matches!(
                plan.decision,
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::InvalidAttemptHistory(_)
                }
            ));
        }
    }
}

#[test]
fn unknown_post_send_outcome_holds_non_idempotent_delivery() {
    let message = envelope();
    let attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "ptyd",
        DeliveryAttemptOutcome::OutcomeUnknown,
        false,
    );
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        std::slice::from_ref(&attempt),
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::HeldOutcomeUnknown {
            attempt_id: attempt.attempt_id,
            endpoint_id: "ptyd".into(),
            state: DeliveryAttemptOutcome::OutcomeUnknown,
        }
    );
}

#[test]
fn idempotent_post_send_outcome_retries_only_the_same_endpoint() {
    let message = envelope();
    let attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native-a",
        DeliveryAttemptOutcome::OutcomeUnknown,
        true,
    );
    let mut same_endpoint = endpoint("native-a", "codex-app-server");
    same_endpoint.capabilities.supports_idempotent_delivery = true;
    let mut different_endpoint = endpoint("native-b", "codex-app-server");
    different_endpoint.capabilities.supports_idempotent_delivery = true;
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[same_endpoint, different_endpoint],
        &RoutePolicy::default(),
        &[attempt],
    );
    assert_eq!(selected(&plan), ("native-a", "codex-app-server"));
    assert_eq!(
        rejection(&plan, "native-b"),
        &RouteRejectionReason::CrossRouteIdempotencyRequired
    );
}

#[test]
fn cross_endpoint_retry_holds_without_a_verified_dedupe_domain() {
    let message = envelope();
    let attempt = DeliveryAttemptV1::new(
        "event-1",
        message.delivery_dedupe_key().unwrap(),
        1,
        "native-a",
        DeliveryAttemptOutcome::OutcomeUnknown,
        true,
    );
    let mut unavailable_same = endpoint("native-a", "codex-app-server");
    unavailable_same.health = EndpointHealth::Unavailable;
    let mut different = endpoint("native-b", "codex-app-server");
    different.capabilities.supports_idempotent_delivery = true;
    let plan = DeliveryPlanner.plan(
        NOW,
        &message,
        &target(),
        &[unavailable_same, different],
        &RoutePolicy::default(),
        &[attempt],
    );
    assert_eq!(
        plan.decision,
        RouteDecisionV1::Queued {
            reason: RouteHoldReason::NoEligibleEndpoint
        }
    );
    assert_eq!(
        rejection(&plan, "native-b"),
        &RouteRejectionReason::CrossRouteIdempotencyRequired
    );
}

#[test]
fn coarse_target_status_requires_separate_verified_evidence() {
    let message = envelope();
    for outcome in [
        DeliveryAttemptOutcome::TargetAcknowledged,
        DeliveryAttemptOutcome::Completed,
    ] {
        let attempt = DeliveryAttemptV1::new(
            "event-1",
            message.delivery_dedupe_key().unwrap(),
            1,
            "native",
            outcome,
            true,
        );
        let plan = DeliveryPlanner.plan(
            NOW,
            &message,
            &target(),
            &[endpoint("ptyd", "ptyd")],
            &RoutePolicy::default(),
            std::slice::from_ref(&attempt),
        );
        assert_eq!(
            plan.decision,
            RouteDecisionV1::Queued {
                reason: RouteHoldReason::TargetEvidenceRequired(attempt.attempt_id),
            }
        );
    }
}

#[test]
fn shadow_comparison_reports_match_and_each_difference() {
    let plan = DeliveryPlanner.plan(
        NOW,
        &envelope(),
        &target(),
        &[endpoint("native", "codex-app-server")],
        &RoutePolicy::default(),
        &[],
    );
    assert_eq!(
        compare_shadow(
            &plan,
            &CurrentRoute {
                adapter: AdapterKind::from("codex-app-server"),
                endpoint_id: Some("native".into()),
            }
        )
        .result,
        ShadowComparisonResult::Match
    );
    assert_eq!(
        compare_shadow(
            &plan,
            &CurrentRoute {
                adapter: AdapterKind::from("ptyd"),
                endpoint_id: None,
            }
        )
        .result,
        ShadowComparisonResult::Different(vec![
            ShadowDifferenceReason::AdapterMismatch {
                planned: AdapterKind::from("codex-app-server"),
                current: AdapterKind::from("ptyd"),
            },
            ShadowDifferenceReason::CurrentEndpointUnbound,
        ])
    );
}

#[test]
fn dogfood_common_agent_paths_use_one_planner_and_fallback_chain() {
    let senders = ["claude:lead", "codex:lead", "cursor:lead", "gemini:lead"];
    let receivers = [
        ("claude:worker", "claude-channel"),
        ("codex:worker", "codex-app-server"),
        ("cursor:worker", "a2a"),
        ("gemini:worker", "a2a"),
    ];
    for sender in senders {
        for (receiver, native_adapter) in receivers {
            let mut message = envelope();
            message.directive.from = sender.into();
            message.directive.to = receiver.into();
            message.context.idempotency_key = Some(stable_delivery_dedupe_key(
                "event-1",
                "repo-rally",
                receiver,
            ));
            let route_target = DeliveryTarget {
                repository_id: "repo-rally".into(),
                agent_id: receiver.into(),
                session_id: "session-target".into(),
                pinned_endpoint_id: None,
            };
            let mut terminal = endpoint("terminal-fallback", "ptyd");
            terminal.agent_id = receiver.into();
            let mut structured = endpoint("native-or-a2a", native_adapter);
            structured.agent_id = receiver.into();
            let plan = DeliveryPlanner.plan(
                NOW,
                &message,
                &route_target,
                &[terminal, structured],
                &RoutePolicy::default(),
                &[],
            );
            assert_eq!(selected(&plan), ("native-or-a2a", native_adapter));
            assert_eq!(plan.event_id, "event-1");
            assert_eq!(
                plan.delivery_dedupe_key,
                message.delivery_dedupe_key().unwrap()
            );
        }
    }
}
