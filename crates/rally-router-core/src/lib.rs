// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Deterministic delivery planning without transport or storage side effects.
//!
//! The planner consumes a canonical delivery envelope, an exact Rally target,
//! endpoint registry snapshots, route policy, and prior attempts. It returns an
//! explainable plan; it never writes the ledger, opens a socket, or sends text.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use rally_protocol::delivery::{
    AdapterKind, DELIVERY_ATTEMPT_SCHEMA_V1, DeliveryAttemptOutcome, DeliveryAttemptV1,
    DeliveryContractError, DeliveryEnvelopeV1, ENDPOINT_SCHEMA_V1, EndpointDescriptorV1,
    EndpointHealth, stable_attempt_id,
};
use serde::{Deserialize, Serialize};

/// Wire schema for deterministic route-plan observations.
pub const ROUTE_PLAN_SCHEMA_V1: &str = "rally.route-plan.v1";

/// Exact recipient the caller wants the planner to resolve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    /// Repository/room boundary. Resolution never crosses it.
    pub repository_id: String,
    /// Stable Rally agent identity, not a terminal or provider address.
    pub agent_id: String,
    /// Optional exact endpoint selected by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_endpoint_id: Option<String>,
}

/// Route policy supplied to the pure planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePolicy {
    /// Ordered adapter preference. An omitted adapter is ineligible.
    pub adapter_order: Vec<AdapterKind>,
    /// Whether a degraded endpoint may be used.
    #[serde(default)]
    pub allow_degraded: bool,
    /// Whether an endpoint without a trustworthy health observation may be used.
    #[serde(default)]
    pub allow_unknown_health: bool,
    /// Whether eligible endpoints must return target-authored positive ACKs.
    #[serde(default)]
    pub require_positive_ack: bool,
}

impl Default for RoutePolicy {
    fn default() -> Self {
        Self {
            adapter_order: [
                "claude-channel",
                "codex-app-server",
                "opencode-server",
                "a2a",
                "ptyd",
                "tmux",
                "cmux",
            ]
            .into_iter()
            .map(AdapterKind::from)
            .collect(),
            allow_degraded: false,
            allow_unknown_health: false,
            require_positive_ack: false,
        }
    }
}

/// Final route decision. Non-selected outcomes never imply a send occurred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RouteDecisionV1 {
    /// One endpoint is eligible and preferred.
    Selected {
        /// Stable endpoint address selected for the future adapter call.
        endpoint_id: String,
        /// Adapter family selected for the future adapter call.
        adapter: AdapterKind,
    },
    /// No safe endpoint can be selected yet.
    Queued {
        /// Typed reason the message must remain pending.
        reason: RouteHoldReason,
    },
    /// A prior send may have succeeded, so another non-idempotent send is unsafe.
    HeldOutcomeUnknown {
        /// Attempt whose outcome is not safe to repeat.
        attempt_id: String,
        /// Endpoint used by that attempt.
        endpoint_id: String,
        /// Coarse attempt outcome that triggered the hold.
        state: DeliveryAttemptOutcome,
    },
}

/// Why planning stopped before selecting an endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum RouteHoldReason {
    /// The caller supplied a non-finite planning timestamp.
    InvalidPlannerTime,
    /// The canonical envelope failed current contract validation.
    InvalidEnvelope(Vec<DeliveryContractError>),
    /// The requested repository does not match the canonical envelope.
    RepositoryMismatch,
    /// The requested Rally agent does not match the directive recipient.
    RecipientMismatch,
    /// A matching attempt carried inconsistent event or dedupe identity.
    InvalidAttemptIdentity(String),
    /// Attempt numbers, uniqueness, or state progression are inconsistent.
    InvalidAttemptHistory(String),
    /// An existing planned attempt must finish or be reconciled first.
    ExistingAttemptPending(String),
    /// A coarse attempt status cannot substitute for target-authored evidence.
    TargetEvidenceRequired(String),
    /// No endpoint survived exact identity and capability filtering.
    NoEligibleEndpoint,
}

/// Per-candidate planner result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvaluation {
    /// Candidate endpoint identity.
    pub endpoint_id: String,
    /// Candidate adapter family.
    pub adapter: AdapterKind,
    /// Why the candidate was selected, retained, or rejected.
    pub decision: CandidateDecision,
}

/// Candidate-level planner decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
pub enum CandidateDecision {
    /// This candidate is the selected route.
    Selected,
    /// This candidate was eligible but ranked after the selected route.
    EligibleNotSelected,
    /// This candidate is not safe or compatible.
    Rejected(RouteRejectionReason),
}

/// Typed reason one endpoint was not eligible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum RouteRejectionReason {
    /// Endpoint descriptor uses an unsupported schema.
    UnsupportedEndpointSchema(String),
    /// Endpoint identity is empty.
    EmptyEndpointId,
    /// Endpoint observation or expiry time is not finite.
    InvalidObservationTime,
    /// Endpoint belongs to another repository/room.
    RepositoryMismatch,
    /// Endpoint belongs to another Rally agent.
    RecipientMismatch,
    /// Caller pinned a different endpoint.
    PinnedEndpointMismatch,
    /// Registry returned the same endpoint ID more than once.
    DuplicateEndpointId,
    /// Runtime signals could not uniquely bind this endpoint.
    AmbiguousEndpoint,
    /// Endpoint observation is expired.
    Expired,
    /// Endpoint is observed unavailable.
    Unavailable,
    /// Policy does not permit degraded endpoints.
    DegradedNotAllowed,
    /// Policy does not permit endpoints with unknown health.
    UnknownHealthNotAllowed,
    /// Endpoint cannot decode this delivery schema.
    UnsupportedDeliverySchema(String),
    /// Adapter does not preserve a required canonical field.
    MissingRequiredField(String),
    /// Adapter does not implement a required semantic capability.
    MissingCapability(String),
    /// A reserved generic feature contradicts its authoritative typed field.
    ConflictingCapability(String),
    /// Policy requires positive target ACK support.
    PositiveAckRequired,
    /// Adapter is absent from the ordered policy.
    AdapterNotAllowed,
    /// A prior safe-to-fallback attempt already used this endpoint.
    PriorAttemptUsed,
    /// A post-send retry requires the next route to preserve target dedupe.
    CrossRouteIdempotencyRequired,
}

/// Versioned, explainable output of the pure planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePlanV1 {
    /// Route plan schema.
    pub route_plan_schema: String,
    /// Canonical event when present, empty only for an invalid legacy envelope.
    pub event_id: String,
    /// Stable logical-delivery key when present.
    pub delivery_dedupe_key: String,
    /// Exact requested target.
    pub target: DeliveryTarget,
    /// Final deterministic decision.
    pub decision: RouteDecisionV1,
    /// Every endpoint, sorted by endpoint ID then adapter, with its reason.
    pub candidates: Vec<CandidateEvaluation>,
}

impl RoutePlanV1 {
    fn held(
        envelope: &DeliveryEnvelopeV1,
        target: DeliveryTarget,
        decision: RouteDecisionV1,
    ) -> Self {
        Self {
            route_plan_schema: ROUTE_PLAN_SCHEMA_V1.to_string(),
            event_id: envelope.event_id.clone().unwrap_or_default(),
            delivery_dedupe_key: envelope
                .delivery_dedupe_key()
                .unwrap_or_default()
                .to_string(),
            target,
            decision,
            candidates: Vec::new(),
        }
    }
}

/// Pure, stateless delivery planner.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryPlanner;

impl DeliveryPlanner {
    /// Select a safe endpoint or return a typed queued/held outcome.
    ///
    /// `now` is supplied by the caller so identical inputs always yield the
    /// same plan. The planner performs no clock, filesystem, network, or ledger
    /// access.
    pub fn plan(
        &self,
        now: f64,
        envelope: &DeliveryEnvelopeV1,
        target: &DeliveryTarget,
        endpoints: &[EndpointDescriptorV1],
        policy: &RoutePolicy,
        prior_attempts: &[DeliveryAttemptV1],
    ) -> RoutePlanV1 {
        if !now.is_finite() {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::InvalidPlannerTime,
                },
            );
        }
        if let Err(errors) = envelope.validate_current() {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::InvalidEnvelope(errors),
                },
            );
        }
        if envelope.repository_id.as_deref() != Some(target.repository_id.as_str()) {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::RepositoryMismatch,
                },
            );
        }
        if envelope.directive.to != target.agent_id {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::RecipientMismatch,
                },
            );
        }

        let event_id = envelope.event_id.as_deref().expect("validated event_id");
        let dedupe_key = envelope
            .delivery_dedupe_key()
            .expect("validated idempotency_key");
        let mut attempts = prior_attempts.iter().collect::<Vec<_>>();
        attempts.sort_by(|left, right| {
            left.attempt_number
                .cmp(&right.attempt_number)
                .then_with(|| left.attempt_id.cmp(&right.attempt_id))
        });

        for attempt in &attempts {
            let event_matches = attempt.event_id == event_id;
            let dedupe_matches = attempt.delivery_dedupe_key == dedupe_key;
            let invalid_matching_attempt = event_matches
                && (attempt.delivery_attempt_schema != DELIVERY_ATTEMPT_SCHEMA_V1
                    || attempt.attempt_number == 0
                    || attempt.attempt_id
                        != stable_attempt_id(&attempt.delivery_dedupe_key, attempt.attempt_number)
                    || attempt.endpoint_id.trim().is_empty());
            if event_matches != dedupe_matches || invalid_matching_attempt {
                return RoutePlanV1::held(
                    envelope,
                    target.clone(),
                    RouteDecisionV1::Queued {
                        reason: RouteHoldReason::InvalidAttemptIdentity(attempt.attempt_id.clone()),
                    },
                );
            }
        }

        let matching_attempts = attempts
            .into_iter()
            .filter(|attempt| {
                attempt.event_id == event_id && attempt.delivery_dedupe_key == dedupe_key
            })
            .collect::<Vec<_>>();

        let mut attempt_numbers = BTreeSet::new();
        let mut attempt_ids = BTreeSet::new();
        let mut required_retry_endpoint: Option<String> = None;
        for (index, attempt) in matching_attempts.iter().enumerate() {
            if !attempt_numbers.insert(attempt.attempt_number)
                || !attempt_ids.insert(attempt.attempt_id.as_str())
                || usize::try_from(attempt.attempt_number).ok() != Some(index + 1)
            {
                return RoutePlanV1::held(
                    envelope,
                    target.clone(),
                    RouteDecisionV1::Queued {
                        reason: RouteHoldReason::InvalidAttemptHistory(attempt.attempt_id.clone()),
                    },
                );
            }

            if required_retry_endpoint
                .as_deref()
                .is_some_and(|required| required != attempt.endpoint_id)
            {
                return RoutePlanV1::held(
                    envelope,
                    target.clone(),
                    RouteDecisionV1::Queued {
                        reason: RouteHoldReason::InvalidAttemptHistory(attempt.attempt_id.clone()),
                    },
                );
            }

            let has_later_attempt = index + 1 < matching_attempts.len();
            let terminal_before_later = matches!(
                attempt.outcome,
                DeliveryAttemptOutcome::Planned
                    | DeliveryAttemptOutcome::TargetAcknowledged
                    | DeliveryAttemptOutcome::Completed
            ) || (matches!(
                attempt.outcome,
                DeliveryAttemptOutcome::TransportSent
                    | DeliveryAttemptOutcome::OutcomeUnknown
                    | DeliveryAttemptOutcome::FailedAfterSend
            ) && !attempt.idempotent_delivery);
            if terminal_before_later && has_later_attempt {
                return RoutePlanV1::held(
                    envelope,
                    target.clone(),
                    RouteDecisionV1::Queued {
                        reason: RouteHoldReason::InvalidAttemptHistory(attempt.attempt_id.clone()),
                    },
                );
            }

            if matches!(
                attempt.outcome,
                DeliveryAttemptOutcome::TransportSent
                    | DeliveryAttemptOutcome::OutcomeUnknown
                    | DeliveryAttemptOutcome::FailedAfterSend
            ) && attempt.idempotent_delivery
            {
                required_retry_endpoint = Some(attempt.endpoint_id.clone());
            }
        }

        if let Some(attempt) = matching_attempts.iter().rev().find(|attempt| {
            matches!(
                attempt.outcome,
                DeliveryAttemptOutcome::TargetAcknowledged | DeliveryAttemptOutcome::Completed
            )
        }) {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::TargetEvidenceRequired(attempt.attempt_id.clone()),
                },
            );
        }

        if let Some(attempt) = matching_attempts
            .iter()
            .rev()
            .find(|attempt| attempt.outcome == DeliveryAttemptOutcome::Planned)
        {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::Queued {
                    reason: RouteHoldReason::ExistingAttemptPending(attempt.attempt_id.clone()),
                },
            );
        }

        if let Some(attempt) = matching_attempts.iter().rev().find(|attempt| {
            matches!(
                attempt.outcome,
                DeliveryAttemptOutcome::TransportSent
                    | DeliveryAttemptOutcome::OutcomeUnknown
                    | DeliveryAttemptOutcome::FailedAfterSend
            ) && !attempt.idempotent_delivery
        }) {
            return RoutePlanV1::held(
                envelope,
                target.clone(),
                RouteDecisionV1::HeldOutcomeUnknown {
                    attempt_id: attempt.attempt_id.clone(),
                    endpoint_id: attempt.endpoint_id.clone(),
                    state: attempt.outcome,
                },
            );
        }

        let used_endpoints = matching_attempts
            .iter()
            .filter(|attempt| {
                attempt.outcome == DeliveryAttemptOutcome::FailedBeforeSend
                    && required_retry_endpoint.as_deref() != Some(attempt.endpoint_id.as_str())
            })
            .map(|attempt| attempt.endpoint_id.as_str())
            .collect::<BTreeSet<_>>();

        let duplicate_ids = duplicate_endpoint_ids(endpoints);
        let adapter_positions = policy
            .adapter_order
            .iter()
            .enumerate()
            .map(|(index, adapter)| (adapter.as_str(), index))
            .collect::<BTreeMap<_, _>>();

        let mut ordered = endpoints.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            left.endpoint_id
                .cmp(&right.endpoint_id)
                .then_with(|| left.adapter.cmp(&right.adapter))
        });

        let mut evaluations = Vec::with_capacity(ordered.len());
        let mut eligible = Vec::new();
        for endpoint in ordered {
            let rejection = reject_candidate(
                now,
                envelope,
                target,
                endpoint,
                policy,
                &adapter_positions,
                &duplicate_ids,
                &used_endpoints,
                required_retry_endpoint.as_deref(),
            );
            if let Some(reason) = rejection {
                evaluations.push(CandidateEvaluation {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    adapter: endpoint.adapter.clone(),
                    decision: CandidateDecision::Rejected(reason),
                });
            } else {
                let health_rank = match endpoint.health {
                    EndpointHealth::Ready => 0_u8,
                    EndpointHealth::Degraded => 1,
                    EndpointHealth::Unknown => 2,
                    EndpointHealth::Unavailable => 3,
                };
                let adapter_rank = *adapter_positions
                    .get(endpoint.adapter.as_str())
                    .expect("eligible adapter has a policy position");
                let ack_rank = u8::from(!endpoint.capabilities.supports_positive_ack);
                eligible.push((
                    (
                        health_rank,
                        adapter_rank,
                        ack_rank,
                        endpoint.endpoint_id.as_str(),
                    ),
                    endpoint,
                ));
                evaluations.push(CandidateEvaluation {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    adapter: endpoint.adapter.clone(),
                    decision: CandidateDecision::EligibleNotSelected,
                });
            }
        }

        eligible.sort_by(|left, right| left.0.cmp(&right.0));
        let decision = if let Some((_, selected)) = eligible.first() {
            if let Some(evaluation) = evaluations
                .iter_mut()
                .find(|evaluation| evaluation.endpoint_id == selected.endpoint_id)
            {
                evaluation.decision = CandidateDecision::Selected;
            }
            RouteDecisionV1::Selected {
                endpoint_id: selected.endpoint_id.clone(),
                adapter: selected.adapter.clone(),
            }
        } else {
            RouteDecisionV1::Queued {
                reason: RouteHoldReason::NoEligibleEndpoint,
            }
        };

        RoutePlanV1 {
            route_plan_schema: ROUTE_PLAN_SCHEMA_V1.to_string(),
            event_id: event_id.to_string(),
            delivery_dedupe_key: dedupe_key.to_string(),
            target: target.clone(),
            decision,
            candidates: evaluations,
        }
    }
}

fn duplicate_endpoint_ids(endpoints: &[EndpointDescriptorV1]) -> BTreeSet<&str> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for endpoint in endpoints {
        if !seen.insert(endpoint.endpoint_id.as_str()) {
            duplicates.insert(endpoint.endpoint_id.as_str());
        }
    }
    duplicates
}

#[allow(clippy::too_many_arguments)]
fn reject_candidate(
    now: f64,
    envelope: &DeliveryEnvelopeV1,
    target: &DeliveryTarget,
    endpoint: &EndpointDescriptorV1,
    policy: &RoutePolicy,
    adapter_positions: &BTreeMap<&str, usize>,
    duplicate_ids: &BTreeSet<&str>,
    used_endpoints: &BTreeSet<&str>,
    required_retry_endpoint: Option<&str>,
) -> Option<RouteRejectionReason> {
    if endpoint.endpoint_schema != ENDPOINT_SCHEMA_V1 {
        return Some(RouteRejectionReason::UnsupportedEndpointSchema(
            endpoint.endpoint_schema.clone(),
        ));
    }
    if endpoint.endpoint_id.trim().is_empty() {
        return Some(RouteRejectionReason::EmptyEndpointId);
    }
    if !endpoint.observed_at.is_finite()
        || endpoint
            .expires_at
            .is_some_and(|expires_at| !expires_at.is_finite())
    {
        return Some(RouteRejectionReason::InvalidObservationTime);
    }
    if endpoint.repository_id != target.repository_id {
        return Some(RouteRejectionReason::RepositoryMismatch);
    }
    if endpoint.agent_id != target.agent_id {
        return Some(RouteRejectionReason::RecipientMismatch);
    }
    if target
        .pinned_endpoint_id
        .as_deref()
        .is_some_and(|pinned| pinned != endpoint.endpoint_id)
    {
        return Some(RouteRejectionReason::PinnedEndpointMismatch);
    }
    if duplicate_ids.contains(endpoint.endpoint_id.as_str()) {
        return Some(RouteRejectionReason::DuplicateEndpointId);
    }
    if endpoint.ambiguous {
        return Some(RouteRejectionReason::AmbiguousEndpoint);
    }
    if endpoint
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
    {
        return Some(RouteRejectionReason::Expired);
    }
    match endpoint.health {
        EndpointHealth::Unavailable => return Some(RouteRejectionReason::Unavailable),
        EndpointHealth::Degraded if !policy.allow_degraded => {
            return Some(RouteRejectionReason::DegradedNotAllowed);
        }
        EndpointHealth::Unknown if !policy.allow_unknown_health => {
            return Some(RouteRejectionReason::UnknownHealthNotAllowed);
        }
        EndpointHealth::Ready | EndpointHealth::Degraded | EndpointHealth::Unknown => {}
    }
    if !endpoint
        .accepted_schemas
        .contains(&envelope.delivery_schema)
    {
        return Some(RouteRejectionReason::UnsupportedDeliverySchema(
            envelope.delivery_schema.clone(),
        ));
    }
    if let Some(field) = envelope
        .required_fields
        .iter()
        .find(|field| !endpoint.capabilities.preserves_fields.contains(*field))
    {
        return Some(RouteRejectionReason::MissingRequiredField(field.clone()));
    }
    if let Some(capability) = endpoint.capabilities.conflicting_reserved_capability() {
        return Some(RouteRejectionReason::ConflictingCapability(
            capability.to_string(),
        ));
    }
    if let Some(capability) = envelope
        .required_capabilities
        .iter()
        .find(|capability| !endpoint.capabilities.supports(capability))
    {
        return Some(RouteRejectionReason::MissingCapability(capability.clone()));
    }
    if policy.require_positive_ack && !endpoint.capabilities.supports_positive_ack {
        return Some(RouteRejectionReason::PositiveAckRequired);
    }
    if !adapter_positions.contains_key(endpoint.adapter.as_str()) {
        return Some(RouteRejectionReason::AdapterNotAllowed);
    }
    if let Some(required_endpoint) = required_retry_endpoint
        && (endpoint.endpoint_id != required_endpoint
            || !endpoint.capabilities.supports_idempotent_delivery)
    {
        return Some(RouteRejectionReason::CrossRouteIdempotencyRequired);
    }
    if used_endpoints.contains(endpoint.endpoint_id.as_str()) {
        return Some(RouteRejectionReason::PriorAttemptUsed);
    }
    None
}

/// Route selected by the current authoritative sender for shadow comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentRoute {
    /// Adapter used by the current path.
    pub adapter: AdapterKind,
    /// Exact endpoint when the legacy path can expose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
}

/// Comparison between the pure plan and the still-authoritative current route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowComparison {
    /// Whether the planner and current route agree.
    pub result: ShadowComparisonResult,
}

/// Typed shadow-mode comparison result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "reasons", rename_all = "snake_case")]
pub enum ShadowComparisonResult {
    /// Adapter and endpoint agree.
    Match,
    /// One or more route dimensions differ or cannot be compared.
    Different(Vec<ShadowDifferenceReason>),
}

/// Why shadow planning differs from the current authoritative route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ShadowDifferenceReason {
    /// Planner intentionally did not select a route.
    PlannerDidNotSelect,
    /// Current routing cannot expose the exact endpoint it used.
    CurrentEndpointUnbound,
    /// Adapter families differ.
    AdapterMismatch {
        /// Adapter selected by the pure planner.
        planned: AdapterKind,
        /// Adapter reported by the current path.
        current: AdapterKind,
    },
    /// Exact endpoint identities differ.
    EndpointMismatch {
        /// Endpoint selected by the pure planner.
        planned: String,
        /// Endpoint reported by the current path.
        current: String,
    },
}

/// Compare a plan with the current route without sending or recording anything.
pub fn compare_shadow(plan: &RoutePlanV1, current: &CurrentRoute) -> ShadowComparison {
    let RouteDecisionV1::Selected {
        endpoint_id,
        adapter,
    } = &plan.decision
    else {
        return ShadowComparison {
            result: ShadowComparisonResult::Different(vec![
                ShadowDifferenceReason::PlannerDidNotSelect,
            ]),
        };
    };

    let mut reasons = Vec::new();
    if adapter != &current.adapter {
        reasons.push(ShadowDifferenceReason::AdapterMismatch {
            planned: adapter.clone(),
            current: current.adapter.clone(),
        });
    }
    match &current.endpoint_id {
        Some(current_endpoint) if current_endpoint != endpoint_id => {
            reasons.push(ShadowDifferenceReason::EndpointMismatch {
                planned: endpoint_id.clone(),
                current: current_endpoint.clone(),
            });
        }
        Some(_) => {}
        None => reasons.push(ShadowDifferenceReason::CurrentEndpointUnbound),
    }
    ShadowComparison {
        result: if reasons.is_empty() {
            ShadowComparisonResult::Match
        } else {
            ShadowComparisonResult::Different(reasons)
        },
    }
}
