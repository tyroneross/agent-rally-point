// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Render layer for `RoomSnapshot::open_obligations` — the pull-based inbox.
//!
//! # Why this module is named `obligations` and not `inbox`
//!
//! `rally_protocol::Inbox` and `rally_protocol::FileInbox` already own the
//! `Inbox` identifier in this workspace (imported in `lib.rs`), and they refer to
//! the ABANDONED Plan F directive ledger — 38 directives queued, 0 ever consumed.
//! Two unrelated "inbox" concepts in one crate is how the next reader ends up
//! wiring the dead one. The user-facing CLI verb stays `rally inbox`; only the
//! Rust identifier differs.
//!
//! # What lives here vs. in `store.rs`
//!
//! The open/closed PREDICATE lives in `store::project_open_obligations`, where it
//! runs once over the already-loaded fact slice and every reader inherits it.
//! This module is pure presentation over that bucket: filter to one tool, sort,
//! cap, and attach the command that clears each row.

use schemars::JsonSchema;
use serde::Serialize;

use crate::next::DEFAULT_STALE_WAIT_SECS;
use crate::shell_quote;
use crate::store::{Fact, FactKind, RoomSnapshot};

/// One open obligation addressed to the calling tool.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct InboxItem {
    pub(crate) event_id: String,
    pub(crate) kind: FactKind,
    pub(crate) subject: String,
    /// The tool that addressed this obligation to the caller. Empty when the
    /// authoring fact carried no `tool` (legacy rows).
    pub(crate) from: String,
    /// Seconds since `created_at`. `0` when the timestamp is unparseable —
    /// see [`fact_age_secs`] for why that direction is the safe one.
    pub(crate) age_secs: i64,
    /// ADVISORY ONLY. `true` means "older than the stale window", which is worth
    /// telling the agent about. It MUST NOT filter: age deciding an obligation is
    /// finished is the exact defect this whole feature closes.
    pub(crate) stale: bool,
    /// The one command that clears this row.
    pub(crate) ack_command: String,
}

/// Inbox projection for one tool.
///
/// `count`, `handoffs`, `artifacts`, and `oldest_age_secs` are EXACT over every
/// matching obligation. Only `items` is capped, so a truncated render can never
/// understate how much is owed.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct InboxResult {
    pub(crate) count: usize,
    pub(crate) handoffs: usize,
    pub(crate) artifacts: usize,
    /// Age of the OLDEST open obligation, or `0` when the inbox is empty.
    pub(crate) oldest_age_secs: i64,
    pub(crate) items: Vec<InboxItem>,
}

/// Project the open obligations addressed to `tool`, oldest first.
///
/// Pure: no store read, no filesystem access, and the only clock read is the
/// `age_secs` arithmetic below. `snapshot.open_obligations` is already sorted
/// oldest-first by `seq` in `store::project_open_obligations`; the order is
/// re-established here anyway so this function does not silently depend on a
/// caller's sort.
pub(crate) fn build_inbox(snapshot: &RoomSnapshot, tool: &str, limit: usize) -> InboxResult {
    let mut mine = snapshot
        .open_obligations
        .iter()
        .filter(|fact| fact.target.as_deref() == Some(tool))
        .collect::<Vec<_>>();
    mine.sort_by_key(|fact| fact.seq);

    let handoffs = mine
        .iter()
        .filter(|fact| fact.kind == FactKind::Handoff)
        .count();
    let artifacts = mine
        .iter()
        .filter(|fact| fact.kind == FactKind::Artifact)
        .count();
    let oldest_age_secs = mine
        .iter()
        .map(|fact| fact_age_secs(fact))
        .max()
        .unwrap_or(0);

    let items = mine
        .iter()
        .take(limit)
        .map(|fact| {
            let age_secs = fact_age_secs(fact);
            InboxItem {
                event_id: fact.event_id.clone(),
                kind: fact.kind.clone(),
                subject: fact.subject.clone(),
                from: fact.tool.clone().unwrap_or_default(),
                age_secs,
                stale: age_secs > DEFAULT_STALE_WAIT_SECS,
                ack_command: ack_command(tool, &fact.event_id),
            }
        })
        .collect::<Vec<_>>();

    InboxResult {
        count: mine.len(),
        handoffs,
        artifacts,
        oldest_age_secs,
        items,
    }
}

/// The receiver-authored ack that closes an obligation.
///
/// A `receipt` is the narrowest of the three closing kinds (`resolve`, `receipt`,
/// `artifact`) and needs no evidence argument, so it is the one suggested here.
pub(crate) fn ack_command(tool: &str, event_id: &str) -> String {
    format!(
        "rally say receipt --tool {} --ref {} --subject \"acked\" --json",
        shell_quote(tool),
        shell_quote(event_id)
    )
}

/// Age of `fact` in seconds, FAILING OPEN at 0 on a malformed or missing
/// timestamp.
///
/// 0 reads as "fresh / unknown", which keeps the row in the inbox and merely
/// unannotated. The opposite convention — treating an unparseable timestamp as
/// very old — would let a corrupt `created_at` mark an item `stale`, and any
/// future consumer that filters on `stale` would then hide it. Nothing about a
/// timestamp may become a reason not to show an unanswered obligation.
fn fact_age_secs(fact: &Fact) -> i64 {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&fact.created_at) else {
        return 0;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    (now.as_secs() as i64 - created.timestamp()).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obligation(event_id: &str, kind: FactKind, target: &str, created_at: &str) -> Fact {
        Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 1,
            thread_id: "t-inbox".to_string(),
            kind,
            tool: Some("claude_code".to_string()),
            subject: format!("subject-{event_id}"),
            created_at: created_at.to_string(),
            target: Some(target.to_string()),
            ..Fact::default()
        }
    }

    /// The inbox is scoped to ONE tool: an obligation addressed to a peer is not
    /// this agent's to answer, so it must not inflate this agent's count.
    #[test]
    fn inbox_holds_only_obligations_addressed_to_the_caller() {
        let snapshot = RoomSnapshot {
            open_obligations: vec![
                obligation("mine", FactKind::Handoff, "codex", "2000-01-01T00:00:00Z"),
                obligation(
                    "theirs",
                    FactKind::Handoff,
                    "claude_code:01",
                    "2000-01-01T00:00:00Z",
                ),
            ],
            ..RoomSnapshot::default()
        };

        let inbox = build_inbox(&snapshot, "codex", 5);
        assert_eq!(inbox.count, 1);
        assert_eq!(inbox.handoffs, 1);
        assert_eq!(inbox.artifacts, 0);
        assert_eq!(inbox.items[0].event_id, "mine");
        assert!(inbox.items[0].ack_command.contains("--ref mine"));
        assert!(inbox.items[0].ack_command.contains("--tool codex"));
    }

    /// `items` is capped; the counts are not. A render that showed "2 items" for
    /// a 40-item backlog would be the false-empty failure in miniature.
    #[test]
    fn item_cap_never_understates_the_count() {
        let mut open_obligations = Vec::new();
        for index in 0..7 {
            let mut fact = obligation(
                &format!("ob-{index}"),
                FactKind::Handoff,
                "codex",
                "2000-01-01T00:00:00Z",
            );
            fact.seq = index as i64;
            open_obligations.push(fact);
        }
        let snapshot = RoomSnapshot {
            open_obligations,
            ..RoomSnapshot::default()
        };

        let inbox = build_inbox(&snapshot, "codex", 2);
        assert_eq!(inbox.count, 7, "count covers every obligation");
        assert_eq!(inbox.items.len(), 2, "items are capped");
        assert_eq!(
            inbox
                .items
                .iter()
                .map(|item| item.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ob-0", "ob-1"],
            "oldest first"
        );
    }

    /// A malformed `created_at` must not hide a row, and must not be reported as
    /// stale. Fail open at age 0.
    #[test]
    fn malformed_timestamp_stays_visible_and_is_not_marked_stale() {
        let snapshot = RoomSnapshot {
            open_obligations: vec![obligation(
                "bad-time",
                FactKind::Artifact,
                "codex",
                "not-a-timestamp",
            )],
            ..RoomSnapshot::default()
        };

        let inbox = build_inbox(&snapshot, "codex", 5);
        assert_eq!(inbox.count, 1);
        assert_eq!(inbox.artifacts, 1);
        assert_eq!(inbox.items[0].age_secs, 0);
        assert!(!inbox.items[0].stale);
    }

    /// `stale` annotates; it never filters. A 30-day-old obligation is both
    /// present AND flagged.
    #[test]
    fn stale_is_advisory_and_the_item_still_ships() {
        let snapshot = RoomSnapshot {
            open_obligations: vec![obligation(
                "ancient",
                FactKind::Handoff,
                "codex",
                "2000-01-01T00:00:00Z",
            )],
            ..RoomSnapshot::default()
        };

        let inbox = build_inbox(&snapshot, "codex", 5);
        assert_eq!(inbox.count, 1);
        assert!(inbox.items[0].stale, "a very old item is annotated stale");
        assert_eq!(inbox.oldest_age_secs, inbox.items[0].age_secs);
    }
}
