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
//! - `evidence`: `dep:<dep-id>` entries (one per dependency item id)
//! - `status`  : "open" | "assigned" | "done"
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
        event_id: fact.event_id.clone(),
        seq: fact.seq,
    })
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Add a backlog item to the room ledger. Returns the stored `Fact`.
pub(crate) fn add_backlog_item(
    room: &RoomStore,
    tool: &str,
    id: &str,
    intent: &str,
    owns: &[String],
    depends_on: &[String],
) -> Result<Fact> {
    // Validate id: must not be empty and must not contain ':' or '\n'
    if id.trim().is_empty() {
        return Err(RallyError::Usage("--id must not be empty".to_string()));
    }
    if id.contains(':') || id.contains('\n') {
        return Err(RallyError::Usage(
            "--id must not contain ':' or newlines".to_string(),
        ));
    }

    let mut scope = vec!["backlog-item".to_string()];
    for path in owns {
        scope.push(format!("owns:{path}"));
    }
    scope.sort();
    scope.dedup();

    let mut evidence: Vec<String> = depends_on.iter().map(|dep| format!("dep:{dep}")).collect();
    evidence.sort();

    let fact = Fact {
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
        target: None,
        ref_id: None,
        status: Some("open".to_string()),
        severity: None,
        uri: None,
        session: None,
    };
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
    let mut scope = vec!["backlog-item".to_string()];
    for path in &existing.owns {
        scope.push(format!("owns:{path}"));
    }
    scope.sort();
    scope.dedup();
    let fact = Fact {
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("backlog"),
        seq: 0,
        thread_id: format!("backlog-{}", id.chars().take(32).collect::<String>()),
        kind: FactKind::BacklogItem,
        tool: Some(tool.to_string()),
        role: None,
        subject: existing.intent.clone(),
        scope,
        created_at: now_string(),
        summary: Some(format!("id:{id}")),
        evidence: Vec::new(),
        target: None,
        ref_id: None,
        status: Some("done".to_string()),
        severity: None,
        uri: None,
        session: None,
    };
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
        add_backlog_item(&room, "t", "X-1", "do the thing", &[], &[]).unwrap();
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_room() -> (RoomStore, std::path::PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-backlog-test-{nanos}"));
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
        assert_eq!(item.status, "open");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn backlog_list_open_excludes_done_items() {
        let (room, root) = test_room();

        add_backlog_item(&room, "tool-a", "dep-1", "dep task", &[], &[]).unwrap();
        add_backlog_item(
            &room,
            "tool-a",
            "task-2",
            "dependant task",
            &[],
            &["dep-1".to_string()],
        )
        .unwrap();

        // Mark dep-1 done by adding another backlog-item fact with same id + done status
        let done_fact = Fact {
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
    fn backlog_satisfied_ids_includes_done() {
        let (room, root) = test_room();

        add_backlog_item(&room, "tool-a", "finished", "done task", &[], &[]).unwrap();
        let done_fact = Fact {
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
