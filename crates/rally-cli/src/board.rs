// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Board — read-only single-repo projection from ledger facts.
//!
//! Emits three views:
//! - **Lanes**: claim-no-artifact=in-flight, artifact-no-resolve=landed-unverified, resolve=closed
//! - **Backlog**: open/assigned/done + dep-blocked items
//! - **Delta**: chronological recent fact log (last N facts)
//!
//! Never writes to ORCHESTRATION.md or any file. Pure read + projection.

use schemars::JsonSchema;
use serde::Serialize;

use crate::agent_state::{AgentStateEntry, project_agent_states, stale_tools};
use crate::backlog::{BacklogItem, list_backlog_items, satisfied_ids};
use crate::now_string;
use crate::store::{Fact, RoomStore};

// ─── Lane projection ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneStatus {
    InFlight,
    LandedUnverified,
    Closed,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LaneItem {
    pub(crate) status: LaneStatus,
    pub(crate) owner: Option<String>,
    pub(crate) subject: String,
    pub(crate) event_id: String,
    pub(crate) seq: i64,
    pub(crate) scope: Vec<String>,
}

/// Project claim/artifact/resolve facts into lanes.
fn project_lanes(facts: &[Fact]) -> Vec<LaneItem> {
    use std::collections::BTreeMap;

    // Which event_ids have been resolved or released?
    let resolved_ids: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "resolve" || f.kind == "release")
        .filter_map(|f| f.ref_id.clone())
        .collect();

    // Which claim scopes have been released?
    let released_scopes: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "release")
        .flat_map(|f| f.scope.clone())
        .collect();

    // Which handoff event_ids are consumed by an artifact --ref?
    let artifact_consumed: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "artifact")
        .filter_map(|f| f.ref_id.clone())
        .collect();

    // Collect artifacts per claim scope (claim event_id → Vec<artifact event_id>)
    let mut artifacts_per_claim: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for fact in facts.iter().filter(|f| f.kind == "artifact") {
        if let Some(ref_id) = &fact.ref_id {
            artifacts_per_claim
                .entry(ref_id.clone())
                .or_default()
                .push(fact.event_id.clone());
        }
    }

    let mut items = Vec::new();
    for fact in facts.iter().filter(|f| f.kind == "claim") {
        // Skip external-intake facts
        if fact.scope.iter().any(|s| s == "external-intake") {
            continue;
        }
        let is_released = resolved_ids.contains(&fact.event_id)
            || fact.scope.iter().any(|s| released_scopes.contains(s));

        if is_released {
            items.push(LaneItem {
                status: LaneStatus::Closed,
                owner: fact.tool.clone(),
                subject: fact.subject.clone(),
                event_id: fact.event_id.clone(),
                seq: fact.seq,
                scope: fact.scope.clone(),
            });
            continue;
        }

        // Has an artifact been recorded for this claim?
        let has_artifact = artifacts_per_claim.contains_key(&fact.event_id);

        // Is the artifact consumed/resolved?
        let artifacts = artifacts_per_claim
            .get(&fact.event_id)
            .cloned()
            .unwrap_or_default();
        let artifact_resolved = artifacts
            .iter()
            .any(|a_id| resolved_ids.contains(a_id) || artifact_consumed.contains(a_id));

        let status = if has_artifact && artifact_resolved {
            LaneStatus::Closed
        } else if has_artifact {
            LaneStatus::LandedUnverified
        } else {
            LaneStatus::InFlight
        };

        items.push(LaneItem {
            status,
            owner: fact.tool.clone(),
            subject: fact.subject.clone(),
            event_id: fact.event_id.clone(),
            seq: fact.seq,
            scope: fact.scope.clone(),
        });
    }

    // Sort by seq ascending for stable output
    items.sort_by_key(|i| i.seq);
    items
}

// ─── Backlog view ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct BacklogView {
    pub(crate) open: Vec<BacklogItem>,
    pub(crate) dep_blocked: Vec<BacklogItem>,
    pub(crate) done: Vec<BacklogItem>,
}

fn project_backlog_view(
    items: &[BacklogItem],
    active_claim_scopes: &std::collections::BTreeSet<String>,
) -> BacklogView {
    let all_ids: std::collections::BTreeSet<String> = items.iter().map(|i| i.id.clone()).collect();
    let done_ids = satisfied_ids(items);

    let mut open = Vec::new();
    let mut dep_blocked = Vec::new();
    let mut done = Vec::new();

    for item in items {
        if done_ids.contains(&item.id) {
            done.push(item.clone());
            continue;
        }
        // Dep-blocked: any dependency not yet in done_ids
        let blocked = item
            .depends_on
            .iter()
            .any(|dep| all_ids.contains(dep) && !done_ids.contains(dep));
        // Also blocked if any owns path is actively claimed by another tool
        let owns_claimed = item.owns.iter().any(|path| {
            active_claim_scopes.contains(path)
                || active_claim_scopes.contains(&format!("file:{path}"))
        });
        if blocked || owns_claimed {
            dep_blocked.push(item.clone());
        } else {
            open.push(item.clone());
        }
    }

    BacklogView {
        open,
        dep_blocked,
        done,
    }
}

// ─── Delta ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DeltaItem {
    pub(crate) seq: i64,
    pub(crate) kind: String,
    pub(crate) tool: Option<String>,
    pub(crate) subject: String,
    pub(crate) event_id: String,
    pub(crate) created_at: String,
}

fn project_delta(facts: &[Fact], limit: usize) -> Vec<DeltaItem> {
    facts
        .iter()
        // Exclude internal plumbing facts from the delta surface
        .filter(|f| !matches!(f.kind.as_str(), "read" | "wake" | "presence"))
        .rev()
        .take(limit)
        .map(|f| DeltaItem {
            seq: f.seq,
            kind: f.kind.as_str().to_string(),
            tool: f.tool.clone(),
            subject: f.subject.clone(),
            event_id: f.event_id.clone(),
            created_at: f.created_at.clone(),
        })
        .collect()
}

// ─── Board output ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct BoardOutput {
    pub(crate) lanes: Vec<LaneItem>,
    pub(crate) backlog: BacklogView,
    pub(crate) delta: Vec<DeltaItem>,
    pub(crate) max_seq: i64,
    /// Liveness-aware agent-state projection — one entry per tool, derived from
    /// the latest presence fact per tool. See `agent_state::project_agent_states`.
    /// Additive field: a host that ignores it sees identical legacy shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) agent_states: Vec<AgentStateEntry>,
    /// Claims whose owner is currently stale (last seen older than the liveness
    /// threshold). Surfaced separately from `lanes` so a host can render
    /// "auto-releasable" without losing the original claim's status. Rally
    /// records + surfaces; the operator/lead triggers any actual release —
    /// consistent with the charter (facilitator, not executor).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) auto_releasable_claims: Vec<LaneItem>,
}

pub(crate) fn build_board(room: &RoomStore) -> crate::error::Result<BoardOutput> {
    let facts = room.facts()?;
    let max_seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);

    // Collect active claim scopes for backlog dep-block check
    let resolved_ids: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "resolve" || f.kind == "release")
        .filter_map(|f| f.ref_id.clone())
        .collect();
    let released_scopes: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "release")
        .flat_map(|f| f.scope.clone())
        .collect();
    let active_claim_scopes: std::collections::BTreeSet<String> = facts
        .iter()
        .filter(|f| f.kind == "claim")
        .filter(|f| !resolved_ids.contains(&f.event_id))
        .filter(|f| !f.scope.iter().any(|s| released_scopes.contains(s)))
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .flat_map(|f| f.scope.clone())
        .collect();

    let lanes = project_lanes(&facts);
    let backlog_items = list_backlog_items(room)?;
    let backlog = project_backlog_view(&backlog_items, &active_claim_scopes);
    let delta = project_delta(&facts, 20);

    // Liveness-aware agent-state projection. `now_string()` is the same clock
    // every projection in the crate reads; passing it explicitly keeps the
    // projector pure + testable.
    let now_ts = now_string();
    let agent_states = project_agent_states(&facts, &now_ts);
    let stale = stale_tools(&agent_states);

    // A claim is "auto-releasable" iff (a) it is currently in-flight (not
    // closed or landed) AND (b) its owner is in the stale set. We project from
    // the lanes vec the work to derive the same status taxonomy.
    let auto_releasable_claims: Vec<LaneItem> = lanes
        .iter()
        .filter(|l| matches!(l.status, LaneStatus::InFlight))
        .filter(|l| {
            l.owner
                .as_deref()
                .map(|o| stale.contains(o))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    Ok(BoardOutput {
        lanes,
        backlog,
        delta,
        max_seq,
        agent_states,
        auto_releasable_claims,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::add_backlog_item;
    use crate::store::{Fact, FactKind, RoomStore};
    use crate::{FACT_SCHEMA, new_id, now_string};

    fn test_room() -> (RoomStore, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rally-board-test-{id}-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = RoomStore::open_at(root.clone()).unwrap();
        (room, root)
    }

    fn append_fact(room: &RoomStore, kind: FactKind, subject: &str, tool: &str) -> Fact {
        let fact = Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("test"),
            seq: 0,
            thread_id: new_id("thread"),
            kind,
            tool: Some(tool.to_string()),
            role: None,
            subject: subject.to_string(),
            scope: vec!["file:src/lib.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: vec!["test-evidence".to_string()],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    #[test]
    fn board_projects_in_flight_claim() {
        let (room, root) = test_room();
        append_fact(&room, FactKind::Claim, "active work", "tool-a");
        let board = build_board(&room).unwrap();
        assert_eq!(board.lanes.len(), 1);
        assert!(matches!(board.lanes[0].status, LaneStatus::InFlight));
        assert_eq!(board.lanes[0].owner.as_deref(), Some("tool-a"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn board_projects_landed_unverified_when_artifact_present() {
        let (room, root) = test_room();
        let claim = append_fact(&room, FactKind::Claim, "claim with artifact", "tool-b");

        let artifact = Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("artifact"),
            seq: 0,
            thread_id: new_id("thread"),
            kind: FactKind::Artifact,
            tool: Some("tool-b".to_string()),
            role: None,
            subject: "artifact for claim".to_string(),
            scope: vec!["file:src/lib.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: vec!["cargo test".to_string()],
            target: None,
            ref_id: Some(claim.event_id.clone()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&artifact).unwrap();

        let board = build_board(&room).unwrap();
        assert_eq!(board.lanes.len(), 1);
        assert!(matches!(
            board.lanes[0].status,
            LaneStatus::LandedUnverified
        ));
        std::fs::remove_dir_all(root).ok();
    }

    /// Verifies the liveness-aware projection: when a tool claimed a path 2 days
    /// ago and has not posted anything since (stale > 15m threshold), the claim
    /// must (a) STILL appear in `lanes` as in-flight (Rally records the truth)
    /// AND (b) surface separately in `auto_releasable_claims[]` so a host can
    /// render the auto-release affordance. Models the codex:consolidation-01
    /// case the lesson fact (seq 1603) flagged. */
    #[test]
    fn board_surfaces_stale_owner_claim_as_auto_releasable() {
        use crate::store::FactKind;
        let (room, root) = test_room();

        // Make a stale claim by hand-writing the created_at (the helper uses
        // now_string()). Override via direct append.
        let claim = Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("ev-stale"),
            seq: 0,
            thread_id: new_id("t"),
            kind: FactKind::Claim,
            tool: Some("stale-tool".to_string()),
            role: None,
            subject: "ancient claim that nobody released".to_string(),
            scope: vec!["file:src/lib.rs".to_string()],
            // 2 days ago — well past the 15-minute liveness threshold.
            created_at: "2026-06-02T10:00:00Z".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&claim).unwrap();

        // Add a presence fact for stale-tool that is also old — this is what
        // makes the tool stale in the projection.
        let stale_presence = Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("ev-pres"),
            seq: 0,
            thread_id: new_id("t"),
            kind: FactKind::Presence,
            tool: Some("stale-tool".to_string()),
            role: None,
            subject: "state=working | file=src/lib.rs | intent=long-gone".to_string(),
            scope: Vec::new(),
            created_at: "2026-06-02T10:00:00Z".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&stale_presence).unwrap();

        let board = build_board(&room).unwrap();

        // Stale claim is still in lanes (Rally records the truth — never lies).
        assert_eq!(board.lanes.len(), 1);
        assert!(matches!(board.lanes[0].status, LaneStatus::InFlight));

        // And surfaced in auto-releasable.
        assert_eq!(
            board.auto_releasable_claims.len(),
            1,
            "stale-owner claim must surface in auto_releasable_claims; got board={:?}",
            board.auto_releasable_claims
        );
        assert_eq!(
            board.auto_releasable_claims[0].owner.as_deref(),
            Some("stale-tool")
        );

        // And agent_states[stale-tool].stale == true.
        let stale_state = board
            .agent_states
            .iter()
            .find(|s| s.tool == "stale-tool")
            .expect("stale-tool must appear in agent_states");
        assert!(stale_state.stale, "stale-tool must be marked stale");

        std::fs::remove_dir_all(root).ok();
    }

    /// A fresh agent's claim must NOT surface in auto_releasable_claims.
    #[test]
    fn board_does_not_mark_fresh_owner_claim_as_auto_releasable() {
        let (room, root) = test_room();
        append_fact(&room, FactKind::Claim, "fresh work", "fresh-tool");
        // Add a fresh presence too, so projection has a state to work with.
        append_fact(
            &room,
            FactKind::Presence,
            "state=working | file=x | intent=fresh",
            "fresh-tool",
        );

        let board = build_board(&room).unwrap();
        assert_eq!(board.lanes.len(), 1);
        assert!(
            board.auto_releasable_claims.is_empty(),
            "fresh agent's claim must not be flagged auto-releasable"
        );
        let fresh = board
            .agent_states
            .iter()
            .find(|s| s.tool == "fresh-tool")
            .expect("fresh-tool must appear");
        assert!(!fresh.stale);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn board_backlog_includes_open_and_dep_blocked() {
        let (room, root) = test_room();

        add_backlog_item(&room, "tool-a", "dep-task", "prerequisite", &[], &[]).unwrap();
        add_backlog_item(
            &room,
            "tool-a",
            "main-task",
            "depends on dep",
            &[],
            &["dep-task".to_string()],
        )
        .unwrap();

        let board = build_board(&room).unwrap();
        // dep-task is open (no deps); main-task is dep-blocked
        assert_eq!(board.backlog.open.len(), 1);
        assert_eq!(board.backlog.open[0].id, "dep-task");
        assert_eq!(board.backlog.dep_blocked.len(), 1);
        assert_eq!(board.backlog.dep_blocked[0].id, "main-task");
        std::fs::remove_dir_all(root).ok();
    }
}
