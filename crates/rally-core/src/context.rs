// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::query::{
    ActiveBlocker, ActiveClaim, ActiveTask, AgentProfile, AgentSubscription, ClaimConflict,
    CoordinationArtifact, CoordinationDecision, CoordinationLesson, PendingHandoff, RecentChange,
    TraceProjection,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextBrief {
    pub tool: String,
    pub profile: Option<AgentProfile>,
    pub subscription: Option<AgentSubscription>,
    pub routing: ContextRouting,
    pub top_priority: Option<ContextItem>,
    pub recommended_next_action: ContextRecommendation,
    pub needs_attention: Vec<ContextItem>,
    pub collision_risk: Vec<ClaimConflict>,
    pub active_tasks: Vec<ActiveTask>,
    pub active_claims: Vec<ActiveClaim>,
    pub active_blockers: Vec<ActiveBlocker>,
    pub artifacts: Vec<CoordinationArtifact>,
    pub decisions: Vec<CoordinationDecision>,
    pub lessons: Vec<CoordinationLesson>,
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
    pub priority: i64,
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
    pub minimum_trust_for_automation: String,
    pub reason: String,
    pub source_event_ids: Vec<String>,
}

pub fn build_context_brief(
    projection: &TraceProjection,
    tool: &str,
    recent_limit: usize,
) -> ContextBrief {
    let profile = projection.profile(tool);
    let subscription = projection.subscription(tool);
    let pending_handoffs = projection.pending_handoffs(Some(tool));
    let own_blockers = projection.active_blockers(Some(tool));
    let active_tasks = projection.active_tasks(Some(tool));
    let active_claims = projection.active_claims(Some(tool));
    let collision_risk = projection
        .claim_conflicts()
        .into_iter()
        .filter(|conflict| conflict.owners.iter().any(|owner| owner == tool))
        .collect::<Vec<_>>();

    let mut needs_attention = Vec::new();
    needs_attention.extend(pending_handoffs.iter().map(handoff_item));
    needs_attention.extend(active_tasks.iter().map(task_item));
    needs_attention.extend(own_blockers.iter().map(blocker_item));
    needs_attention.extend(collision_risk.iter().map(conflict_item));
    needs_attention.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then(left.kind.cmp(&right.kind))
            .then(left.event_id.cmp(&right.event_id))
    });

    let top_priority = needs_attention.first().cloned();
    let recommended_next_action = recommend_next_action(
        &pending_handoffs,
        &active_tasks,
        &own_blockers,
        &collision_risk,
        &active_claims,
    );
    let routing = routing_for(&recommended_next_action);

    ContextBrief {
        tool: tool.to_string(),
        profile,
        subscription,
        routing,
        top_priority,
        recommended_next_action,
        needs_attention,
        collision_risk,
        active_tasks,
        active_claims,
        active_blockers: own_blockers,
        artifacts: projection.artifacts(10),
        decisions: projection.decisions(10),
        lessons: projection.lessons(10),
        relevant_changes: projection.recent_changes(recent_limit),
    }
}

fn handoff_item(item: &PendingHandoff) -> ContextItem {
    ContextItem {
        kind: "handoff".to_string(),
        priority: 100,
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        reason: "assigned to this tool and requires acknowledgement".to_string(),
        source_event_ids: vec![item.event_id.clone()],
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    }
}

fn task_item(item: &ActiveTask) -> ContextItem {
    ContextItem {
        kind: "task".to_string(),
        priority: 90,
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        reason: "active task is assigned to this tool".to_string(),
        source_event_ids: vec![item.event_id.clone()],
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    }
}

fn blocker_item(item: &ActiveBlocker) -> ContextItem {
    ContextItem {
        kind: "blocker".to_string(),
        priority: 80,
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
        priority: 70,
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
    active_tasks: &[ActiveTask],
    own_blockers: &[ActiveBlocker],
    collision_risk: &[ClaimConflict],
    active_claims: &[ActiveClaim],
) -> ContextRecommendation {
    if let Some(item) = pending_handoffs.first() {
        return ContextRecommendation {
            action: "ack_handoff".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.95,
            minimum_trust_for_automation: "trusted".to_string(),
            reason: "a required handoff is assigned to this tool".to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    if let Some(item) = active_tasks.first() {
        return ContextRecommendation {
            action: "work_task".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.88,
            minimum_trust_for_automation: "trusted".to_string(),
            reason: "an active task is assigned to this tool".to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    if let Some(item) = own_blockers.first() {
        return ContextRecommendation {
            action: "resolve_blocker".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.85,
            minimum_trust_for_automation: "local-or-trusted".to_string(),
            reason: "this tool is blocked until the blocker is resolved or updated".to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    if let Some(item) = collision_risk.first() {
        return ContextRecommendation {
            action: "resolve_claim_conflict".to_string(),
            target: Some(item.resource.clone()),
            confidence: 0.8,
            minimum_trust_for_automation: "local-or-trusted".to_string(),
            reason: "this tool has an active claim that overlaps another owner".to_string(),
            source_event_ids: item.claim_ids.clone(),
        };
    }
    if let Some(item) = active_claims.first() {
        return ContextRecommendation {
            action: "continue_claim".to_string(),
            target: Some(item.event_id.clone()),
            confidence: 0.65,
            minimum_trust_for_automation: "local-or-trusted".to_string(),
            reason: "this tool has active claimed work and no higher-priority coordination risk"
                .to_string(),
            source_event_ids: vec![item.event_id.clone()],
        };
    }
    ContextRecommendation {
        action: "proceed_solo".to_string(),
        target: None,
        confidence: 0.55,
        minimum_trust_for_automation: "none".to_string(),
        reason: "no pending handoffs, blockers, or claim conflicts for this tool".to_string(),
        source_event_ids: Vec::new(),
    }
}

fn routing_for(recommendation: &ContextRecommendation) -> ContextRouting {
    let action = match recommendation.action.as_str() {
        "ack_handoff" | "work_task" | "resolve_blocker" | "resolve_claim_conflict" => "join_active",
        "continue_claim" => "continue_active",
        _ => "proceed_solo",
    };
    ContextRouting {
        action: action.to_string(),
        reason: recommendation.reason.clone(),
    }
}
