// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::query::{
    ActiveBlocker, ActiveClaim, ActiveTask, AgentProfile, AgentSubscription, ClaimConflict,
    CoordinationArtifact, CoordinationDecision, CoordinationLesson, PendingHandoff, RecentChange,
    TraceProjection,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextBrief {
    pub tool: String,
    pub profile: Option<AgentProfile>,
    pub subscription: Option<AgentSubscription>,
    pub routing: ContextRouting,
    pub top_priority: Option<ContextItem>,
    pub attuned_items: Vec<AttunedItem>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub linked_task_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_status: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttunedItem {
    pub kind: String,
    pub event_id: String,
    pub subject: String,
    pub score: i64,
    pub factors: Vec<String>,
    pub source_event_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub linked_task_ids: Vec<String>,
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
    let artifacts = projection.artifacts(10);
    let decisions = projection.decisions(10);
    let lessons = projection.lessons(10);
    let relevant_changes = projection.recent_changes(recent_limit);
    let attuned_items = build_attuned_items(AttunementInput {
        profile: profile.as_ref(),
        subscription: subscription.as_ref(),
        needs_attention: &needs_attention,
        active_claims: &active_claims,
        artifacts: &artifacts,
        decisions: &decisions,
        lessons: &lessons,
        relevant_changes: &relevant_changes,
        limit: recent_limit,
    });
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
        attuned_items,
        recommended_next_action,
        needs_attention,
        collision_risk,
        active_tasks,
        active_claims,
        active_blockers: own_blockers,
        artifacts,
        decisions,
        lessons,
        relevant_changes,
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
        paths: item.files.clone(),
        linked_task_ids: Vec::new(),
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
        paths: Vec::new(),
        linked_task_ids: item.depends_on.clone(),
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
        paths: item
            .resource
            .as_deref()
            .and_then(normalize_path)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: Vec::new(),
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
        paths: normalize_path(&item.resource)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: Vec::new(),
        origin: None,
        trust_status: None,
    }
}

struct AttunementInput<'a> {
    profile: Option<&'a AgentProfile>,
    subscription: Option<&'a AgentSubscription>,
    needs_attention: &'a [ContextItem],
    active_claims: &'a [ActiveClaim],
    artifacts: &'a [CoordinationArtifact],
    decisions: &'a [CoordinationDecision],
    lessons: &'a [CoordinationLesson],
    relevant_changes: &'a [RecentChange],
    limit: usize,
}

#[derive(Clone, Debug)]
struct AttunementPolicy<'a> {
    current_task: Option<&'a str>,
    profile_watch: Vec<&'a str>,
    subscribed_paths: Vec<&'a str>,
    subscribed_kinds: Vec<&'a str>,
    subscribed_threads: Vec<&'a str>,
    subscribed_tasks: Vec<&'a str>,
    active_claim_paths: Vec<String>,
}

fn build_attuned_items(input: AttunementInput<'_>) -> Vec<AttunedItem> {
    let policy = AttunementPolicy::new(input.profile, input.subscription, input.active_claims);
    let mut items = Vec::new();

    items.extend(
        input
            .needs_attention
            .iter()
            .map(|item| attune_attention_item(item, &policy)),
    );
    items.extend(
        input
            .active_claims
            .iter()
            .map(|item| attune_claim(item, &policy)),
    );
    items.extend(
        input
            .artifacts
            .iter()
            .map(|item| attune_artifact(item, &policy)),
    );
    items.extend(
        input
            .decisions
            .iter()
            .map(|item| attune_decision(item, &policy)),
    );
    items.extend(
        input
            .lessons
            .iter()
            .map(|item| attune_lesson(item, &policy)),
    );
    items.extend(
        input
            .relevant_changes
            .iter()
            .map(|item| attune_recent_change(item, &policy)),
    );

    items.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(left.kind.cmp(&right.kind))
            .then(left.event_id.cmp(&right.event_id))
    });
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.event_id.clone()));
    items.truncate(input.limit.max(1));
    items
}

impl<'a> AttunementPolicy<'a> {
    fn new(
        profile: Option<&'a AgentProfile>,
        subscription: Option<&'a AgentSubscription>,
        active_claims: &'a [ActiveClaim],
    ) -> Self {
        Self {
            current_task: profile.and_then(|profile| profile.current_task.as_deref()),
            profile_watch: profile
                .map(|profile| profile.watch.iter().map(String::as_str).collect())
                .unwrap_or_default(),
            subscribed_paths: subscription
                .map(|subscription| subscription.paths.iter().map(String::as_str).collect())
                .unwrap_or_default(),
            subscribed_kinds: subscription
                .map(|subscription| {
                    subscription
                        .event_kinds
                        .iter()
                        .map(String::as_str)
                        .collect()
                })
                .unwrap_or_default(),
            subscribed_threads: subscription
                .map(|subscription| subscription.threads.iter().map(String::as_str).collect())
                .unwrap_or_default(),
            subscribed_tasks: subscription
                .map(|subscription| subscription.tasks.iter().map(String::as_str).collect())
                .unwrap_or_default(),
            active_claim_paths: active_claims
                .iter()
                .filter_map(|claim| normalize_path(&claim.resource).map(str::to_string))
                .collect(),
        }
    }
}

fn attune_attention_item(item: &ContextItem, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: item.kind.clone(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: item.priority,
        factors: vec![format!("unresolved:{}", item.kind)],
        source_event_ids: item.source_event_ids.clone(),
        paths: item.paths.clone(),
        linked_task_ids: item.linked_task_ids.clone(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, None);
    candidate
}

fn attune_claim(item: &ActiveClaim, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: "claim".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: 60,
        factors: vec!["owned_claim".to_string()],
        source_event_ids: vec![item.event_id.clone()],
        paths: normalize_path(&item.resource)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: Vec::new(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, item.thread_id.as_deref());
    candidate
}

fn attune_artifact(item: &CoordinationArtifact, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: "artifact".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: 35,
        factors: vec![format!("artifact:{}", item.artifact_kind)],
        source_event_ids: vec![item.event_id.clone()],
        paths: item
            .uri
            .as_deref()
            .and_then(normalize_path)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: item.ref_task_id.iter().cloned().collect(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, None);
    candidate
}

fn attune_decision(item: &CoordinationDecision, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: "decision".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: 30,
        factors: vec![format!("decision:{}", item.status)],
        source_event_ids: vec![item.event_id.clone()],
        paths: item
            .scope
            .as_deref()
            .and_then(normalize_path)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: Vec::new(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, None);
    candidate
}

fn attune_lesson(item: &CoordinationLesson, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: "lesson".to_string(),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: 25
            + item
                .confidence
                .map(|value| (value * 10.0) as i64)
                .unwrap_or(0),
        factors: item
            .lesson_kind
            .as_ref()
            .map(|kind| vec![format!("lesson:{kind}")])
            .unwrap_or_else(|| vec!["lesson".to_string()]),
        source_event_ids: std::iter::once(item.event_id.clone())
            .chain(item.source_event_ids.iter().cloned())
            .collect(),
        paths: item
            .scope
            .as_deref()
            .and_then(normalize_path)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        linked_task_ids: Vec::new(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, None);
    candidate
}

fn attune_recent_change(item: &RecentChange, policy: &AttunementPolicy<'_>) -> AttunedItem {
    let mut candidate = AttunedItem {
        kind: format!("recent_{}", item.kind),
        event_id: item.event_id.clone(),
        subject: item.subject.clone(),
        score: 15,
        factors: vec!["recent_change".to_string()],
        source_event_ids: vec![item.event_id.clone()],
        paths: paths_from_texts([item.subject.as_str()]),
        linked_task_ids: Vec::new(),
        origin: item.origin.clone(),
        trust_status: item.trust_status.clone(),
    };
    apply_policy(&mut candidate, policy, item.thread_id.as_deref());
    if item.age_seconds.is_some_and(|age| age <= 900) {
        add_factor(&mut candidate, 8, "fresh");
    }
    candidate
}

fn apply_policy(
    candidate: &mut AttunedItem,
    policy: &AttunementPolicy<'_>,
    thread_id: Option<&str>,
) {
    if let Some(task) = policy.current_task {
        if candidate.event_id == task
            || candidate
                .source_event_ids
                .iter()
                .any(|source| source == task)
            || candidate
                .linked_task_ids
                .iter()
                .any(|source| source == task)
        {
            add_factor(candidate, 35, format!("current_task:{task}"));
        }
    }

    for task in &policy.subscribed_tasks {
        if candidate.event_id == *task
            || candidate
                .source_event_ids
                .iter()
                .any(|source| source == task)
            || candidate
                .linked_task_ids
                .iter()
                .any(|source| source == task)
        {
            add_factor(candidate, 30, format!("subscribed_task:{task}"));
        }
    }

    if let Some(thread_id) = thread_id {
        for thread in &policy.subscribed_threads {
            if thread_id == *thread {
                add_factor(candidate, 25, format!("subscribed_thread:{thread}"));
            }
        }
    }

    let kind = candidate
        .kind
        .strip_prefix("recent_")
        .unwrap_or(&candidate.kind);
    if policy.subscribed_kinds.contains(&kind) {
        add_factor(candidate, 20, format!("subscribed_kind:{kind}"));
    }

    apply_path_matches(candidate, &policy.profile_watch, "profile_watch", 25);
    apply_path_matches(candidate, &policy.subscribed_paths, "subscribed_path", 25);
    let claim_paths = policy
        .active_claim_paths
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    apply_path_matches(candidate, &claim_paths, "active_claim_path", 15);
    apply_trust(candidate);
}

fn apply_path_matches(candidate: &mut AttunedItem, watched: &[&str], label: &str, score: i64) {
    for path in watched {
        let Some(path) = normalize_path(path) else {
            continue;
        };
        if candidate
            .paths
            .iter()
            .any(|candidate_path| paths_overlap(candidate_path, path))
        {
            add_factor(candidate, score, format!("{label}:{path}"));
        }
    }
}

fn apply_trust(candidate: &mut AttunedItem) {
    match candidate.trust_status.as_deref() {
        Some("trusted") => add_factor(candidate, 10, "trusted"),
        Some("invalid") | Some("conflict") => add_factor(candidate, -60, "trust_risk"),
        Some("untrusted") => add_factor(candidate, -20, "untrusted"),
        Some("unsigned") => add_factor(candidate, -5, "unsigned"),
        _ if candidate
            .origin
            .as_deref()
            .is_none_or(|origin| origin == "local") =>
        {
            add_factor(candidate, 8, "local");
        }
        _ => {}
    }
}

fn add_factor(candidate: &mut AttunedItem, score: i64, factor: impl Into<String>) {
    let factor = factor.into();
    if !candidate.factors.iter().any(|value| value == &factor) {
        candidate.factors.push(factor);
        candidate.score += score;
    }
}

fn paths_from_texts<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    values
        .into_iter()
        .filter_map(normalize_path)
        .map(str::to_string)
        .collect()
}

fn normalize_path(value: &str) -> Option<&str> {
    let value = value.trim();
    let value = value.strip_prefix("file:").unwrap_or(value);
    let value = value.trim_matches('/');
    (!value.is_empty()
        && (value.contains('/')
            || value.starts_with("docs")
            || value.starts_with("crates")
            || value.ends_with(".rs")
            || value.ends_with(".md")
            || value.ends_with(".json")))
    .then_some(value)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
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
