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
    pub trust: RecommendationTrust,
    pub reason: String,
    pub source_event_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecommendationTrust {
    pub required: String,
    pub automation_allowed: bool,
    pub source_statuses: Vec<TrustSourceStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrustSourceStatus {
    pub event_id: String,
    pub origin: String,
    pub trust_status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkPacket {
    pub tool: String,
    pub role: String,
    pub packet_kind: String,
    pub profile: Option<AgentProfile>,
    pub recommended_next_action: ContextRecommendation,
    pub trust_summary: PacketTrustSummary,
    pub source_event_ids: Vec<String>,
    pub focus: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub review_targets: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub build_targets: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub architecture_targets: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub verification_targets: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub active_tasks: Vec<ActiveTask>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub active_claims: Vec<ActiveClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub active_blockers: Vec<ActiveBlocker>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub collision_risk: Vec<ClaimConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub artifacts: Vec<CoordinationArtifact>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub decisions: Vec<CoordinationDecision>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub lessons: Vec<CoordinationLesson>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trust_risks: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub risk_areas: Vec<AttunedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub open_tradeoffs: Vec<ContextItem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PacketTrustSummary {
    pub minimum_trust_for_automation: String,
    pub recommendation_automation_allowed: bool,
    pub trusted: usize,
    pub local: usize,
    pub unsigned: usize,
    pub untrusted: usize,
    pub invalid: usize,
    pub unknown: usize,
}

/// All inputs `build_context_brief` needs, decoupled from how they were
/// gathered. The legacy gather path (records → TraceProjection → method
/// calls) lives in `ContextInputs::from_projection`. A graph-backed path
/// will be added in a follow-up commit as `ContextInputs::from_graph`.
///
/// This struct exists so the brief assembly logic stays in one place and
/// callers pick the source: in-memory records or persisted graph.
#[derive(Clone, Debug)]
pub struct ContextInputs {
    pub tool: String,
    pub recent_limit: usize,
    pub profile: Option<AgentProfile>,
    pub subscription: Option<AgentSubscription>,
    pub pending_handoffs: Vec<PendingHandoff>,
    pub own_blockers: Vec<ActiveBlocker>,
    pub active_tasks: Vec<ActiveTask>,
    pub active_claims: Vec<ActiveClaim>,
    pub claim_conflicts: Vec<ClaimConflict>,
    pub artifacts: Vec<CoordinationArtifact>,
    pub decisions: Vec<CoordinationDecision>,
    pub lessons: Vec<CoordinationLesson>,
    pub recent_changes: Vec<RecentChange>,
}

impl ContextInputs {
    /// Gather inputs from a TraceProjection — the legacy in-memory path.
    /// Identical to what `build_context_brief` previously did inline.
    pub fn from_projection(projection: &TraceProjection, tool: &str, recent_limit: usize) -> Self {
        Self {
            tool: tool.to_string(),
            recent_limit,
            profile: projection.profile(tool),
            subscription: projection.subscription(tool),
            pending_handoffs: projection.pending_handoffs(Some(tool)),
            own_blockers: projection.active_blockers(Some(tool)),
            active_tasks: projection.active_tasks(Some(tool)),
            active_claims: projection.active_claims(Some(tool)),
            claim_conflicts: projection.claim_conflicts(),
            artifacts: projection.artifacts(10),
            decisions: projection.decisions(10),
            lessons: projection.lessons(10),
            recent_changes: projection.recent_changes(recent_limit),
        }
    }

    /// Gather inputs from the SQLite graph projection — the migration
    /// target. Same field shapes as `from_projection`, sourced from the
    /// persistent index instead of an in-memory record scan.
    ///
    /// Caller must ensure the graph is caught up before calling (e.g.,
    /// via `graph::catch_up`). This function performs no mutation.
    pub fn from_graph(
        conn: &crate::graph::GraphConnection,
        tool: &str,
        recent_limit: usize,
        now_epoch: f64,
    ) -> Result<Self, rusqlite::Error> {
        Ok(Self {
            tool: tool.to_string(),
            recent_limit,
            profile: crate::graph::latest_profile_typed(conn, tool)?,
            subscription: crate::graph::latest_subscription_typed(conn, tool)?,
            pending_handoffs: crate::graph::pending_handoffs_typed(conn, Some(tool), now_epoch)?,
            own_blockers: crate::graph::active_blockers_typed(conn, Some(tool), now_epoch)?,
            active_tasks: crate::graph::active_tasks_typed(conn, Some(tool))?,
            active_claims: crate::graph::active_claims_typed(conn, Some(tool), now_epoch)?,
            claim_conflicts: crate::graph::claim_conflicts_typed(conn)?,
            artifacts: crate::graph::recent_artifacts_typed(conn, 10)?,
            decisions: crate::graph::recent_decisions_typed(conn, 10)?,
            lessons: crate::graph::recent_lessons_typed(conn, 10)?,
            recent_changes: crate::graph::recent_changes_typed(conn, recent_limit as u32, now_epoch)?,
        })
    }
}

/// Legacy entrypoint preserved for callers passing `&TraceProjection`.
/// Internally just constructs `ContextInputs::from_projection` and calls
/// `build_context_brief_from_inputs`. New callers should prefer
/// `ContextInputs::from_graph` (added in a follow-up) and pass the
/// inputs directly.
pub fn build_context_brief(
    projection: &TraceProjection,
    tool: &str,
    recent_limit: usize,
) -> ContextBrief {
    let inputs = ContextInputs::from_projection(projection, tool, recent_limit);
    build_context_brief_from_inputs(&inputs)
}

/// Assemble a ContextBrief from already-gathered inputs. Pure transformer
/// — no I/O, no projection construction. The data-source split lives in
/// `ContextInputs::from_*` constructors.
pub fn build_context_brief_from_inputs(inputs: &ContextInputs) -> ContextBrief {
    let tool = inputs.tool.as_str();
    let recent_limit = inputs.recent_limit;
    let profile = inputs.profile.clone();
    let subscription = inputs.subscription.clone();
    let pending_handoffs = inputs.pending_handoffs.clone();
    let own_blockers = inputs.own_blockers.clone();
    let active_tasks = inputs.active_tasks.clone();
    let active_claims = inputs.active_claims.clone();
    let collision_risk = inputs
        .claim_conflicts
        .iter()
        .filter(|conflict| conflict.owners.iter().any(|owner| owner == tool))
        .cloned()
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
    let artifacts = inputs.artifacts.clone();
    let decisions = inputs.decisions.clone();
    let lessons = inputs.lessons.clone();
    let relevant_changes = inputs.recent_changes.clone();
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
        profile.as_ref(),
        &pending_handoffs,
        &active_tasks,
        &own_blockers,
        &collision_risk,
        &active_claims,
        &attuned_items,
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

pub fn build_work_packet(brief: &ContextBrief, limit: usize) -> WorkPacket {
    let limit = limit.max(1);
    let role = role_for_brief(brief).unwrap_or("general");
    let packet_kind = match role {
        "reviewer" => "review",
        "builder" => "build",
        "architect" => "architecture",
        "qa" => "verification",
        _ => "general",
    };
    let focus = brief
        .attuned_items
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let trust_risks = focus
        .iter()
        .filter(|item| item_has_trust_risk(item))
        .cloned()
        .collect::<Vec<_>>();
    let mut packet = WorkPacket {
        tool: brief.tool.clone(),
        role: role.to_string(),
        packet_kind: packet_kind.to_string(),
        profile: brief.profile.clone(),
        recommended_next_action: brief.recommended_next_action.clone(),
        trust_summary: packet_trust_summary(brief, &focus),
        source_event_ids: packet_source_event_ids(brief, &focus),
        focus: focus.clone(),
        review_targets: Vec::new(),
        build_targets: Vec::new(),
        architecture_targets: Vec::new(),
        verification_targets: Vec::new(),
        active_tasks: Vec::new(),
        active_claims: Vec::new(),
        active_blockers: Vec::new(),
        collision_risk: Vec::new(),
        artifacts: Vec::new(),
        decisions: Vec::new(),
        lessons: Vec::new(),
        files: packet_files(brief, &focus),
        test_commands: packet_test_commands(brief),
        trust_risks,
        risk_areas: Vec::new(),
        open_tradeoffs: Vec::new(),
    };

    match role {
        "reviewer" => {
            packet.review_targets = filter_focus(&focus, review_target_kind);
            packet.artifacts = brief.artifacts.clone();
            packet.decisions = brief.decisions.clone();
            packet.lessons = brief.lessons.clone();
            packet.risk_areas = packet.trust_risks.clone();
        }
        "builder" => {
            packet.build_targets = filter_focus(&focus, build_target_kind);
            packet.active_tasks = brief.active_tasks.clone();
            packet.active_claims = brief.active_claims.clone();
            packet.active_blockers = brief.active_blockers.clone();
            packet.collision_risk = brief.collision_risk.clone();
            packet.decisions = brief.decisions.clone();
        }
        "architect" => {
            packet.architecture_targets = filter_focus(&focus, architecture_target_kind);
            packet.decisions = brief.decisions.clone();
            packet.lessons = brief.lessons.clone();
            packet.artifacts = brief.artifacts.clone();
            packet.open_tradeoffs = brief
                .needs_attention
                .iter()
                .filter(|item| matches!(item.kind.as_str(), "blocker" | "claim_conflict"))
                .cloned()
                .collect();
        }
        "qa" => {
            packet.verification_targets = filter_focus(&focus, verification_target_kind);
            packet.artifacts = brief.artifacts.clone();
            packet.lessons = brief.lessons.clone();
            packet.active_blockers = brief.active_blockers.clone();
            packet.collision_risk = brief.collision_risk.clone();
            packet.risk_areas = focus
                .iter()
                .filter(|item| item_has_trust_risk(item) || item.kind.contains("blocker"))
                .cloned()
                .collect();
        }
        _ => {}
    }

    packet
}

fn role_for_brief(brief: &ContextBrief) -> Option<&'static str> {
    brief.profile.as_ref().and_then(profile_specialization)
}

fn filter_focus(items: &[AttunedItem], predicate: fn(&str) -> bool) -> Vec<AttunedItem> {
    items
        .iter()
        .filter(|item| predicate(item.kind.as_str()))
        .cloned()
        .collect()
}

fn review_target_kind(kind: &str) -> bool {
    matches!(kind, "artifact" | "decision" | "lesson" | "handoff")
        || kind.starts_with("recent_artifact")
        || kind.starts_with("recent_decision")
}

fn build_target_kind(kind: &str) -> bool {
    matches!(kind, "task" | "claim" | "blocker" | "claim_conflict")
        || kind.starts_with("recent_task")
        || kind.starts_with("recent_claim")
}

fn architecture_target_kind(kind: &str) -> bool {
    matches!(kind, "decision" | "lesson" | "artifact") || kind.starts_with("recent_decision")
}

fn verification_target_kind(kind: &str) -> bool {
    matches!(kind, "artifact" | "lesson") || kind.starts_with("recent_artifact")
}

fn packet_files(brief: &ContextBrief, focus: &[AttunedItem]) -> Vec<String> {
    let mut files = BTreeSet::new();
    for path in focus.iter().flat_map(|item| item.paths.iter()) {
        files.insert(path.clone());
    }
    for path in brief
        .needs_attention
        .iter()
        .flat_map(|item| item.paths.iter())
    {
        files.insert(path.clone());
    }
    for path in brief
        .active_claims
        .iter()
        .filter_map(|claim| normalize_path(&claim.resource).map(str::to_string))
    {
        files.insert(path);
    }
    files.into_iter().collect()
}

fn packet_test_commands(brief: &ContextBrief) -> Vec<String> {
    let mut commands = BTreeSet::new();
    for command in brief
        .active_tasks
        .iter()
        .filter_map(|task| task.verification.as_ref())
    {
        commands.insert(command.clone());
    }
    commands.into_iter().collect()
}

fn packet_source_event_ids(brief: &ContextBrief, focus: &[AttunedItem]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    ids.extend(
        brief
            .recommended_next_action
            .source_event_ids
            .iter()
            .cloned(),
    );
    for item in focus {
        ids.extend(item.source_event_ids.iter().cloned());
    }
    ids.into_iter().collect()
}

fn packet_trust_summary(brief: &ContextBrief, focus: &[AttunedItem]) -> PacketTrustSummary {
    let mut summary = PacketTrustSummary {
        minimum_trust_for_automation: brief
            .recommended_next_action
            .minimum_trust_for_automation
            .clone(),
        recommendation_automation_allowed: brief.recommended_next_action.trust.automation_allowed,
        trusted: 0,
        local: 0,
        unsigned: 0,
        untrusted: 0,
        invalid: 0,
        unknown: 0,
    };
    for item in focus {
        match classified_trust(item.origin.as_deref(), item.trust_status.as_deref()).as_str() {
            "trusted" => summary.trusted += 1,
            "local" => summary.local += 1,
            "unsigned" => summary.unsigned += 1,
            "untrusted" => summary.untrusted += 1,
            "invalid" | "conflict" => summary.invalid += 1,
            _ => summary.unknown += 1,
        }
    }
    summary
}

fn item_has_trust_risk(item: &AttunedItem) -> bool {
    matches!(
        classified_trust(item.origin.as_deref(), item.trust_status.as_deref()).as_str(),
        "unsigned" | "untrusted" | "invalid" | "conflict" | "unknown"
    )
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
    role: Option<&'a str>,
    capabilities: Vec<&'a str>,
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
            role: profile.and_then(|profile| profile.role.as_deref()),
            capabilities: profile
                .map(|profile| profile.capabilities.iter().map(String::as_str).collect())
                .unwrap_or_default(),
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
    apply_specialization(candidate, policy);
    apply_trust(candidate);
}

fn apply_specialization(candidate: &mut AttunedItem, policy: &AttunementPolicy<'_>) {
    match specialization(policy) {
        Some("reviewer") => match candidate.kind.as_str() {
            "artifact" => add_factor(candidate, 40, "role:reviewer"),
            "decision" => add_factor(candidate, 30, "role:reviewer"),
            "lesson" => add_factor(candidate, 20, "role:reviewer"),
            "handoff" => add_factor(candidate, 15, "role:reviewer"),
            value
                if value.starts_with("recent_artifact") || value.starts_with("recent_decision") =>
            {
                add_factor(candidate, 20, "role:reviewer");
            }
            _ => {}
        },
        Some("architect") => match candidate.kind.as_str() {
            "decision" => add_factor(candidate, 45, "role:architect"),
            "lesson" => add_factor(candidate, 25, "role:architect"),
            "artifact" => add_factor(candidate, 15, "role:architect"),
            value if value.starts_with("recent_decision") => {
                add_factor(candidate, 25, "role:architect");
            }
            _ => {}
        },
        Some("builder") => match candidate.kind.as_str() {
            "task" | "claim" => add_factor(candidate, 25, "role:builder"),
            value if value.starts_with("recent_task") || value.starts_with("recent_claim") => {
                add_factor(candidate, 15, "role:builder");
            }
            _ => {}
        },
        Some("qa") => match candidate.kind.as_str() {
            "artifact" | "lesson" => add_factor(candidate, 30, "role:qa"),
            value if value.starts_with("recent_artifact") => {
                add_factor(candidate, 20, "role:qa");
            }
            _ => {}
        },
        _ => {}
    }
}

fn specialization(policy: &AttunementPolicy<'_>) -> Option<&'static str> {
    if let Some(role) = policy.role {
        return canonical_role(role);
    }
    capability_specialization(policy.capabilities.iter().copied())
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
    profile: Option<&AgentProfile>,
    pending_handoffs: &[PendingHandoff],
    active_tasks: &[ActiveTask],
    own_blockers: &[ActiveBlocker],
    collision_risk: &[ClaimConflict],
    active_claims: &[ActiveClaim],
    attuned_items: &[AttunedItem],
) -> ContextRecommendation {
    if let Some(item) = pending_handoffs.first() {
        return context_recommendation(
            "ack_handoff",
            Some(item.event_id.clone()),
            0.95,
            "trusted",
            "a required handoff is assigned to this tool".to_string(),
            vec![trust_ref(
                &item.event_id,
                item.origin.as_deref(),
                item.trust_status.as_deref(),
            )],
        );
    }
    if let Some(item) = active_tasks.first() {
        return context_recommendation(
            "work_task",
            Some(item.event_id.clone()),
            0.88,
            "trusted",
            "an active task is assigned to this tool".to_string(),
            vec![trust_ref(
                &item.event_id,
                item.origin.as_deref(),
                item.trust_status.as_deref(),
            )],
        );
    }
    if let Some(item) = own_blockers.first() {
        return context_recommendation(
            "resolve_blocker",
            Some(item.event_id.clone()),
            0.85,
            "local-or-trusted",
            "this tool is blocked until the blocker is resolved or updated".to_string(),
            vec![trust_ref(
                &item.event_id,
                item.origin.as_deref(),
                item.trust_status.as_deref(),
            )],
        );
    }
    if let Some(item) = collision_risk.first() {
        return context_recommendation(
            "resolve_claim_conflict",
            Some(item.resource.clone()),
            0.8,
            "local-or-trusted",
            "this tool has an active claim that overlaps another owner".to_string(),
            item.claim_ids
                .iter()
                .map(|event_id| trust_ref(event_id, None, None))
                .collect(),
        );
    }
    if let Some(item) = active_claims.first() {
        return context_recommendation(
            "continue_claim",
            Some(item.event_id.clone()),
            0.65,
            "local-or-trusted",
            "this tool has active claimed work and no higher-priority coordination risk"
                .to_string(),
            vec![trust_ref(
                &item.event_id,
                item.origin.as_deref(),
                item.trust_status.as_deref(),
            )],
        );
    }
    if let Some(recommendation) = recommend_specialized_action(profile, attuned_items) {
        return recommendation;
    }
    context_recommendation(
        "proceed_solo",
        None,
        0.55,
        "none",
        "no pending handoffs, blockers, or claim conflicts for this tool".to_string(),
        Vec::new(),
    )
}

fn recommend_specialized_action(
    profile: Option<&AgentProfile>,
    attuned_items: &[AttunedItem],
) -> Option<ContextRecommendation> {
    let role = profile.and_then(profile_specialization)?;
    let item = attuned_items.first()?;
    let action = match (role, item.kind.as_str()) {
        ("reviewer", "artifact") => "review_artifact",
        ("reviewer", "decision") => "review_decision",
        ("reviewer", "lesson") => "review_lesson",
        ("architect", "decision") => "review_decision",
        ("architect", "lesson") => "review_lesson",
        ("qa", "artifact") => "verify_artifact",
        _ => return None,
    };
    Some(context_recommendation(
        action,
        Some(item.event_id.clone()),
        0.72,
        "local-or-trusted",
        format!("{role} profile is best matched to {}", item.kind),
        item.source_event_ids
            .iter()
            .map(|event_id| {
                trust_ref(
                    event_id,
                    item.origin.as_deref(),
                    item.trust_status.as_deref(),
                )
            })
            .collect(),
    ))
}

#[derive(Clone, Debug)]
struct RecommendationTrustRef {
    event_id: String,
    origin: Option<String>,
    trust_status: Option<String>,
}

fn trust_ref(
    event_id: &str,
    origin: Option<&str>,
    trust_status: Option<&str>,
) -> RecommendationTrustRef {
    RecommendationTrustRef {
        event_id: event_id.to_string(),
        origin: origin.map(str::to_string),
        trust_status: trust_status.map(str::to_string),
    }
}

fn context_recommendation(
    action: &str,
    target: Option<String>,
    confidence: f64,
    minimum_trust_for_automation: &str,
    reason: String,
    trust_refs: Vec<RecommendationTrustRef>,
) -> ContextRecommendation {
    let source_event_ids = trust_refs
        .iter()
        .map(|item| item.event_id.clone())
        .collect::<Vec<_>>();
    let source_statuses = trust_refs
        .iter()
        .map(|item| TrustSourceStatus {
            event_id: item.event_id.clone(),
            origin: item.origin.clone().unwrap_or_else(|| "local".to_string()),
            trust_status: classified_trust(item.origin.as_deref(), item.trust_status.as_deref()),
        })
        .collect::<Vec<_>>();
    let trust = RecommendationTrust {
        required: minimum_trust_for_automation.to_string(),
        automation_allowed: automation_allowed(minimum_trust_for_automation, &source_statuses),
        source_statuses,
    };
    ContextRecommendation {
        action: action.to_string(),
        target,
        confidence,
        minimum_trust_for_automation: minimum_trust_for_automation.to_string(),
        trust,
        reason,
        source_event_ids,
    }
}

fn automation_allowed(required: &str, statuses: &[TrustSourceStatus]) -> bool {
    match required {
        "none" => true,
        "trusted" => {
            !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|status| status.trust_status == "trusted")
        }
        "local-or-trusted" => statuses.iter().all(|status| {
            status.trust_status == "trusted"
                || (status.trust_status == "local" && status.origin == "local")
        }),
        _ => false,
    }
}

fn classified_trust(origin: Option<&str>, trust_status: Option<&str>) -> String {
    if let Some(status) = trust_status {
        return status.to_string();
    }
    if origin.is_none_or(|origin| origin == "local") {
        "local".to_string()
    } else {
        "unknown".to_string()
    }
}

fn profile_specialization(profile: &AgentProfile) -> Option<&'static str> {
    if let Some(role) = profile.role.as_deref() {
        return canonical_role(role);
    }
    capability_specialization(profile.capabilities.iter().map(String::as_str))
}

fn canonical_role(role: &str) -> Option<&'static str> {
    match role {
        "review" | "reviewer" => Some("reviewer"),
        "architecture" | "architect" => Some("architect"),
        "qa" | "test" | "testing" => Some("qa"),
        "build" | "builder" | "implementation" => Some("builder"),
        _ => None,
    }
}

fn capability_specialization<'a>(
    capabilities: impl IntoIterator<Item = &'a str>,
) -> Option<&'static str> {
    for capability in capabilities {
        if matches!(capability, "review" | "reviewer" | "security") {
            return Some("reviewer");
        }
        if matches!(capability, "architecture" | "architect" | "design") {
            return Some("architect");
        }
        if matches!(capability, "qa" | "test" | "testing") {
            return Some("qa");
        }
        if matches!(capability, "build" | "builder" | "implementation") {
            return Some("builder");
        }
    }
    None
}

fn routing_for(recommendation: &ContextRecommendation) -> ContextRouting {
    let action = match recommendation.action.as_str() {
        "ack_handoff"
        | "work_task"
        | "resolve_blocker"
        | "resolve_claim_conflict"
        | "review_artifact"
        | "review_decision"
        | "review_lesson"
        | "verify_artifact" => "join_active",
        "continue_claim" => "continue_active",
        _ => "proceed_solo",
    };
    ContextRouting {
        action: action.to_string(),
        reason: recommendation.reason.clone(),
    }
}
