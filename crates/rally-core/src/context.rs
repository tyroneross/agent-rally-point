// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::query::{
    ActiveBlocker, ActiveClaim, ClaimConflict, PendingHandoff, RecentChange, TraceProjection,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextBrief {
    pub tool: String,
    pub routing: ContextRouting,
    pub top_priority: Option<ContextItem>,
    pub recommended_next_action: ContextRecommendation,
    pub needs_attention: Vec<ContextItem>,
    pub collision_risk: Vec<ClaimConflict>,
    pub active_claims: Vec<ActiveClaim>,
    pub active_blockers: Vec<ActiveBlocker>,
    pub relevant_changes: Vec<RecentChange>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextRouting {
    pub action: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub kind: String,
    pub event_id: String,
    pub subject: String,
    pub reason: String,
    pub source_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextRecommendation {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub confidence: f64,
    pub reason: String,
    pub source_event_ids: Vec<String>,
}

pub fn build_context_brief(
    projection: &TraceProjection,
    tool: &str,
    recent_limit: usize,
) -> ContextBrief {
    let pending_handoffs = projection.pending_handoffs(Some(tool));
    let own_blockers = projection.active_blockers(Some(tool));
    let active_claims = projection.active_claims(Some(tool));
    let collision_risk = projection
        .claim_conflicts()
        .into_iter()
        .filter(|conflict| conflict.owners.iter().any(|owner| owner == tool))
        .collect::<Vec<_>>();

    let mut needs_attention = Vec::new();
    needs_attention.extend(pending_handoffs.iter().map(handoff_item));
    needs_attention.extend(own_blockers.iter().map(blocker_item));
    needs_attention.extend(collision_risk.iter().map(conflict_item));

    let top_priority = needs_attention.first().cloned();
    let recommended_next_action = recommend_next_action(
        &pending_handoffs,
        &own_blockers,
        &collision_risk,
        &active_claims,
    );
    let routing = routing_for(&recommended_next_action);

    ContextBrief {
        tool: tool.to_string(),
        routing,
        top_priority,
        recommended_next_action,
        needs_attention,
        collision_risk,
        active_claims,
        active_blockers: own_blockers,
        relevant_changes: projection.recent_changes(recent_limit),
    }
}

fn handoff_item(item: &PendingHandoff) -> ContextItem {
    ContextItem {
        kind: "handoff".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        reason: "assigned to this tool and requires acknowledgement".to_string(),
        source_event_ids: vec![item.event_id.clone()],
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    }
}

fn blocker_item(item: &ActiveBlocker) -> ContextItem {
    ContextItem {
        kind: "blocker".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        reason: "this tool has an unresolved blocker".to_string(),
        source_event_ids: vec![item.event_id.clone()],
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    }
}

fn conflict_item(item: &ClaimConflict) -> ContextItem {
    ContextItem {
        kind: "claim_conflict".to_string(),
        event_id: item.claim_ids.first().cloned().unwrap_or_default(),
        subject: item.resource.clone(),
        reason: "active ownership claims overlap across tools".to_string(),
        source_event_ids: item.claim_ids.clone(),
        origin: None,
        trust_status: None,
    }
}

fn recommend_next_action(
    pending_handoffs: &[PendingHandoff],
    own_blockers: &[ActiveBlocker],
    collision_risk: &[ClaimConflict],
    active_claims: &[ActiveClaim],
) -> ContextRecommendation {
    if let Some(item) = pending_handoffs.first() {
        return ContextRecommendation {
            action: "ack_handoff".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.95,
            reason: "a required handoff is assigned to this tool".to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    if let Some(item) = own_blockers.first() {
        return ContextRecommendation {
            action: "resolve_blocker".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.85,
            reason: "this tool is blocked until the blocker is resolved or updated".to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    if let Some(item) = collision_risk.first() {
        return ContextRecommendation {
            action: "resolve_claim_conflict".to_string(),
            target: Some(item.resource.clone()),
            confidence: 0.8,
            reason: "this tool has an active claim that overlaps another owner".to_string(),
            source_event_ids: item.claim_ids.clone(),
        };
    }
    if let Some(item) = active_claims.first() {
        return ContextRecommendation {
            action: "continue_claim".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.65,
            reason: "this tool has active claimed work and no higher-priority coordination risk"
                .to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    ContextRecommendation {
        action: "proceed_solo".to_string(),
        target: None,
        confidence: 0.55,
        reason: "no pending handoffs, blockers, or claim conflicts for this tool".to_string(),
        source_event_ids: Vec::new(),
    }
}

fn routing_for(recommendation: &ContextRecommendation) -> ContextRouting {
    let action = match recommendation.action.as_str() {
        "ack_handoff" | "resolve_blocker" | "resolve_claim_conflict" => "join_active",
        "continue_claim" => "continue_active",
        _ => "proceed_solo",
    };
    ContextRouting {
        action: action.to_string(),
        reason: recommendation.reason.clone(),
    }
}
