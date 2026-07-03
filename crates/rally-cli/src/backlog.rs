// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Backlog store — per-room claimable backlog backed by ledger facts.
//!
//! Items are stored as `FactKind::BacklogItem` facts. The item's fields are
//! encoded in `summary` / `scope` / `evidence` using the additive-marker
//! pattern already established by `build_id:` in presence facts and
//! `read_seq:` in read-checkpoint facts:
//!
//! - `subject` : the intent (free-form description)
//! - `scope`   : `owns:<path>` entries (one per owned path) + `backlog-item` sentinel
//! - `summary` : `id:<id>` prefix, then the rest is free text
//! - `evidence`: `dep:<dep-id>` entries (one per dependency item id) and
//!   `expected_by:<time-or-checkpoint>` for live status planning
//! - `target`  : assigned owner expected to keep status current
//! - `status`  : "open" | "planned" | "in_progress" | "blocked" | "done"
//!
//! No schema bump is required because all encoding lives in existing fields.

use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{RallyError, Result};
use crate::store::{Fact, FactKind, RoomStore};
use crate::{FACT_SCHEMA, new_id, now_string};

// ─── Domain types ────────────────────────────────────────────────────────────

/// A single backlog item as presented to callers.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct BacklogItem {
    pub(crate) id: String,
    pub(crate) intent: String,
    pub(crate) owns: Vec<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) status: String,
    pub(crate) target: Option<String>,
    pub(crate) expected_by: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) created_at: String,
    /// `event_id` of the underlying ledger fact — callers use it to resolve.
    pub(crate) event_id: String,
    pub(crate) seq: i64,
}

// ─── Encoding helpers ─────────────────────────────────────────────────────────

/// Extract the `id:` from the `summary` field of a backlog fact.
fn extract_id(fact: &Fact) -> Option<String> {
    fact.summary
        .as_deref()
        .and_then(|s| s.strip_prefix("id:"))
        .and_then(|rest| rest.split('\n').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract intent text — returns the fact's `subject` field directly.
fn extract_intent(fact: &Fact) -> String {
    fact.subject.clone()
}

/// Extract `owns:<path>` entries from scope.
fn extract_owns(fact: &Fact) -> Vec<String> {
    fact.scope
        .iter()
        .filter_map(|s| s.strip_prefix("owns:"))
        .map(str::to_string)
        .collect()
}

/// Extract `dep:<id>` entries from evidence.
fn extract_depends_on(fact: &Fact) -> Vec<String> {
    fact.evidence
        .iter()
        .filter_map(|s| s.strip_prefix("dep:"))
        .map(str::to_string)
        .collect()
}

fn extract_expected_by(fact: &Fact) -> Option<String> {
    fact.evidence
        .iter()
        .filter_map(|s| s.strip_prefix("expected_by:"))
        .next_back()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn extract_status(fact: &Fact) -> String {
    fact.status.clone().unwrap_or_else(|| "open".to_string())
}

/// Decode a ledger `Fact` into a `BacklogItem`. Returns `None` for non-backlog facts.
pub(crate) fn fact_to_backlog_item(fact: &Fact) -> Option<BacklogItem> {
    if fact.kind != "backlog-item" {
        return None;
    }
    if !fact.scope.iter().any(|s| s == "backlog-item") {
        return None;
    }
    let id = extract_id(fact)?;
    Some(BacklogItem {
        id,
        intent: extract_intent(fact),
        owns: extract_owns(fact),
        depends_on: extract_depends_on(fact),
        status: extract_status(fact),
        target: fact.target.clone(),
        expected_by: extract_expected_by(fact),
        tool: fact.tool.clone(),
        created_at: fact.created_at.clone(),
        event_id: fact.event_id.clone(),
        seq: fact.seq,
    })
}

// ─── Public API ──────────────────────────────────────────────────────────────

fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        return Err(RallyError::Usage("--id must not be empty".to_string()));
    }
    if id.contains(':') || id.contains('\n') {
        return Err(RallyError::Usage(
            "--id must not contain ':' or newlines".to_string(),
        ));
    }
    Ok(())
}

/// The closed set of valid backlog statuses (matches the module-doc contract and
/// `rally next`'s actionable-status filter). Kept as the single source of truth
/// so an unrecognized `--status` (e.g. `wip`) fails loud instead of being stored
/// and silently dropping off the plan/status obligation radar.
const VALID_STATUSES: [&str; 5] = ["open", "planned", "in_progress", "blocked", "done"];

fn validate_status(status: &str) -> Result<()> {
    if VALID_STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(RallyError::Usage(format!(
            "--status must be one of {}; got {status:?}",
            VALID_STATUSES.join("|")
        )))
    }
}

// These helpers mirror CLI/user-facing backlog fields. Keeping the call shape
// explicit is clearer than hiding optional fields behind a builder at call sites.
#[allow(clippy::too_many_arguments)]
fn build_backlog_fact(
    tool: &str,
    id: &str,
    intent: &str,
    owns: &[String],
    depends_on: &[String],
    status: &str,
    target: Option<&str>,
    expected_by: Option<&str>,
) -> Fact {
    let mut scope = vec!["backlog-item".to_string()];
    for path in owns {
        scope.push(format!("owns:{path}"));
    }
    scope.sort();
    scope.dedup();

    let mut evidence: Vec<String> = depends_on.iter().map(|dep| format!("dep:{dep}")).collect();
    if let Some(expected_by) = expected_by.map(str::trim).filter(|value| !value.is_empty()) {
        evidence.push(format!("expected_by:{expected_by}"));
    }
    evidence.sort();
    evidence.dedup();

    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("backlog"),
        seq: 0,
        thread_id: format!("backlog-{}", id.chars().take(32).collect::<String>()),
        kind: FactKind::BacklogItem,
        tool: Some(tool.to_string()),
        role: None,
        subject: intent.to_string(),
        scope,
        created_at: now_string(),
        summary: Some(format!("id:{id}")),
        evidence,
        target: target.map(str::to_string),
        ref_id: None,
        status: Some(status.to_string()),
        severity: None,
        uri: None,
        session: None,
    }
}

/// Add a backlog item to the room ledger. Returns the stored `Fact`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_backlog_item(
    room: &RoomStore,
    tool: &str,
    id: &str,
    intent: &str,
    owns: &[String],
    depends_on: &[String],
    status: Option<&str>,
    target: Option<&str>,
    expected_by: Option<&str>,
) -> Result<Fact> {
    validate_id(id)?;
    if let Some(status) = status {
        validate_status(status)?;
    }
    let status = status.unwrap_or("open");
    let fact = build_backlog_fact(
        tool,
        id,
        intent,
        owns,
        depends_on,
        status,
        target,
        expected_by,
    );
    room.append_fact_verified(&fact)
}

/// Update an existing backlog item by appending a same-id fact. Omitted fields
/// inherit from the latest item so update facts remain self-contained.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_backlog_item(
    room: &RoomStore,
    tool: &str,
    id: &str,
    intent: Option<&str>,
    owns: Option<&[String]>,
    depends_on: Option<&[String]>,
    status: Option<&str>,
    target: Option<&str>,
    expected_by: Option<&str>,
) -> Result<Fact> {
    validate_id(id)?;
    if let Some(status) = status {
        validate_status(status)?;
    }
    let existing = list_backlog_items(room)?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| RallyError::Usage(format!("no backlog item with id '{id}'")))?;

    let intent = intent.unwrap_or(&existing.intent);
    let owns = owns.unwrap_or(&existing.owns);
    let depends_on = depends_on.unwrap_or(&existing.depends_on);
    let status = status.unwrap_or(&existing.status);
    let inherited_target = existing.target.as_deref();
    let target = target.or(inherited_target);
    let inherited_expected_by = existing.expected_by.as_deref();
    let expected_by = expected_by.or(inherited_expected_by);

    let fact = build_backlog_fact(
        tool,
        id,
        intent,
        owns,
        depends_on,
        status,
        target,
        expected_by,
    );
    room.append_fact_verified(&fact)
}

/// Mark an existing backlog item `done` by appending a same-id status fact
/// (append-only; `list_backlog_items` takes the latest fact per id). Errors if
/// no item with that id exists, or it is already done.
pub(crate) fn mark_backlog_done(room: &RoomStore, tool: &str, id: &str) -> Result<Fact> {
    let existing = list_backlog_items(room)?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| RallyError::Usage(format!("no backlog item with id '{id}'")))?;
    if existing.status == "done" {
        return Err(RallyError::Usage(format!(
            "backlog item '{id}' is already done"
        )));
    }
    let fact = build_backlog_fact(
        tool,
        id,
        &existing.intent,
        &existing.owns,
        &existing.depends_on,
        "done",
        existing.target.as_deref(),
        existing.expected_by.as_deref(),
    );
    room.append_fact_verified(&fact)
}

/// Return all backlog items for this room, ordered by seq ascending.
pub(crate) fn list_backlog_items(room: &RoomStore) -> Result<Vec<BacklogItem>> {
    let facts = room.facts()?;
    // The latest status fact per id wins (later seq overrides).
    // We also need to handle status-update facts: a fact with the same `id:`
    // but a different status is a status update. We collect the highest-seq
    // fact per id.
    let mut by_id: std::collections::BTreeMap<String, Fact> = std::collections::BTreeMap::new();
    for fact in &facts {
        if fact.kind != "backlog-item" {
            continue;
        }
        if !fact.scope.iter().any(|s| s == "backlog-item") {
            continue;
        }
        let Some(id) = extract_id(fact) else { continue };
        let entry = by_id.entry(id).or_insert_with(|| fact.clone());
        if fact.seq > entry.seq {
            *entry = fact.clone();
        }
    }
    let mut items: Vec<BacklogItem> = by_id.values().filter_map(fact_to_backlog_item).collect();
    items.sort_by_key(|i| i.seq);
    Ok(items)
}

/// Return the set of item ids that are considered "satisfied" (done or resolved).
pub(crate) fn satisfied_ids(items: &[BacklogItem]) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .filter(|i| i.status == "done")
        .map(|i| i.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlog_done_marks_item_done() {
        let (room, _root) = test_room();
        add_backlog_item(
            &room,
            "t",
            "X-1",
            "do the thing",
            &[],
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(list_backlog_items(&room).unwrap()[0].status, "open");
        mark_backlog_done(&room, "t", "X-1").unwrap();
        let item = list_backlog_items(&room)
            .unwrap()
            .into_iter()
            .find(|i| i.id == "X-1")
            .unwrap();
        assert_eq!(item.status, "done", "latest fact per id must be done");
        assert!(
            mark_backlog_done(&room, "t", "missing").is_err(),
            "unknown id errors"
        );
        assert!(
            mark_backlog_done(&room, "t", "X-1").is_err(),
            "already-done errors"
        );
    }

    use crate::store::RoomStore;

    fn test_room() -> (RoomStore, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rally-backlog-test-{id}-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = RoomStore::open_at(root.clone()).unwrap();
        (room, root)
    }

    #[test]
    fn backlog_add_stores_and_list_returns_item() {
        let (room, root) = test_room();

        let fact = add_backlog_item(
            &room,
            "claude_code:01",
            "task-1",
            "implement the widget",
            &["crates/widget/src/lib.rs".to_string()],
            &[],
            Some("planned"),
            Some("codex"),
            Some("2026-07-02T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(fact.kind, FactKind::BacklogItem);

        let items = list_backlog_items(&room).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "task-1");
        assert_eq!(item.intent, "implement the widget");
        assert_eq!(item.owns, vec!["crates/widget/src/lib.rs"]);
        assert!(item.depends_on.is_empty());
        assert_eq!(item.status, "planned");
        assert_eq!(item.target.as_deref(), Some("codex"));
        assert_eq!(item.expected_by.as_deref(), Some("2026-07-02T12:00:00Z"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_add_rejects_invalid_status() {
        let (room, root) = test_room();
        let err = add_backlog_item(
            &room,
            "t",
            "task-1",
            "do it",
            &[],
            &[],
            Some("wip"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, RallyError::Usage(msg) if msg.contains("--status must be one of")),
            "invalid --status must fail loud, not be stored"
        );
        assert!(
            list_backlog_items(&room).unwrap().is_empty(),
            "no fact is appended when status is invalid"
        );
        // Every valid status is accepted.
        for status in VALID_STATUSES {
            add_backlog_item(
                &room,
                "t",
                &format!("ok-{status}"),
                "do it",
                &[],
                &[],
                Some(status),
                None,
                None,
            )
            .unwrap_or_else(|_| panic!("valid status {status:?} must be accepted"));
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_update_rejects_invalid_status() {
        let (room, root) = test_room();
        add_backlog_item(&room, "t", "task-1", "do it", &[], &[], None, None, None).unwrap();
        let err = update_backlog_item(
            &room,
            "t",
            "task-1",
            None,
            None,
            None,
            Some("wip"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, RallyError::Usage(msg) if msg.contains("--status must be one of")),
            "invalid --status on update must fail loud"
        );
        // The item keeps its prior status; no bad fact was appended.
        let item = list_backlog_items(&room)
            .unwrap()
            .into_iter()
            .find(|i| i.id == "task-1")
            .unwrap();
        assert_eq!(item.status, "open");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_list_open_excludes_done_items() {
        let (room, root) = test_room();

        add_backlog_item(
            &room,
            "tool-a",
            "dep-1",
            "dep task",
            &[],
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        add_backlog_item(
            &room,
            "tool-a",
            "task-2",
            "dependant task",
            &[],
            &["dep-1".to_string()],
            None,
            None,
            None,
        )
        .unwrap();

        // Mark dep-1 done by adding another backlog-item fact with same id + done status
        let done_fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("backlog"),
            seq: 0,
            thread_id: "backlog-dep-1".to_string(),
            kind: FactKind::BacklogItem,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "dep task".to_string(),
            scope: vec!["backlog-item".to_string()],
            created_at: now_string(),
            summary: Some("id:dep-1".to_string()),
            evidence: vec![],
            target: None,
            ref_id: None,
            status: Some("done".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&done_fact).unwrap();

        // open = items where status != "done"
        let all = list_backlog_items(&room).unwrap();
        let open: Vec<_> = all.iter().filter(|i| i.status != "done").collect();
        // dep-1 is done so only task-2 should be open
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "task-2");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_update_preserves_omitted_plan_fields() {
        let (room, root) = test_room();

        add_backlog_item(
            &room,
            "claude_code",
            "plan-1",
            "publish a live plan",
            &["docs/ORCHESTRATION.md".to_string()],
            &["dep-1".to_string()],
            Some("planned"),
            Some("codex"),
            Some("noon"),
        )
        .unwrap();
        update_backlog_item(
            &room,
            "codex",
            "plan-1",
            None,
            None,
            None,
            Some("in_progress"),
            None,
            Some("next checkpoint"),
        )
        .unwrap();

        let item = list_backlog_items(&room)
            .unwrap()
            .into_iter()
            .find(|item| item.id == "plan-1")
            .unwrap();
        assert_eq!(item.intent, "publish a live plan");
        assert_eq!(item.owns, vec!["docs/ORCHESTRATION.md"]);
        assert_eq!(item.depends_on, vec!["dep-1"]);
        assert_eq!(item.target.as_deref(), Some("codex"));
        assert_eq!(item.status, "in_progress");
        assert_eq!(item.expected_by.as_deref(), Some("next checkpoint"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_satisfied_ids_includes_done() {
        let (room, root) = test_room();

        add_backlog_item(
            &room,
            "tool-a",
            "finished",
            "done task",
            &[],
            &[],
            None,
            None,
            None,
        )
        .unwrap();
        let done_fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("backlog"),
            seq: 0,
            thread_id: "backlog-finished".to_string(),
            kind: FactKind::BacklogItem,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "done task".to_string(),
            scope: vec!["backlog-item".to_string()],
            created_at: now_string(),
            summary: Some("id:finished".to_string()),
            evidence: vec![],
            target: None,
            ref_id: None,
            status: Some("done".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&done_fact).unwrap();

        let all = list_backlog_items(&room).unwrap();
        let sat = satisfied_ids(&all);
        assert!(sat.contains("finished"));

        std::fs::remove_dir_all(root).ok();
    }
}
