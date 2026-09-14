// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Handoff DELIVERY — the send-time liveness decision and the undelivered
//! projection. Two halves of one property: **a handoff either reaches a live
//! session or says loudly that it cannot.**
//!
//! # The defect this closes
//!
//! Codex has no inbound hook, so ledger delivery is pull-only: a handoff is
//! "delivered" when the target runs `rally next`/`rally inbox` and finds it. A
//! target that has already exited never pulls, and nothing anywhere said so.
//! Measured on the `ross-labs-astro` room (2026-09-13, 15,139 events):
//! **8 of 27 targeted handoffs were never picked up** — including
//! `fact_2b7b_18d5033b70b1b380` (`codex:sol-v6` → `codex:motion-review`,
//! 22:53:00Z), whose target had posted its last read-checkpoint at 22:20:04Z
//! and never wrote again. It sat unread for over an hour until a human noticed.
//!
//! `stale_target_warning` (lib.rs) already WARNED on that send. Warning was not
//! enough, for two reasons this module fixes:
//!
//! 1. **The warning was the only effect.** The fact went to exactly one inbox —
//!    the dead one. [`delivery_plan`] additionally routes a copy to the base
//!    tool (`codex` for `codex:*`), which is the inbox a *fresh* Codex session
//!    actually polls, and emits a `wake` fact so the target's own wake queue
//!    records the attempt.
//! 2. **Nothing said so afterwards.** A warning is a line on one terminal at one
//!    moment. [`project_handoffs`] makes the same condition queryable at any
//!    later time, over the whole ledger, regardless of lease expiry.
//!
//! # Fail direction: toward delivery
//!
//! Both halves fail toward OVER-delivery. An unprovable target (no squad row,
//! unparseable timestamp) is treated as NOT live, so it gets the fallback copy.
//! The cost of a wrong "not live" is one extra inbox item a peer acks and drops;
//! the cost of a wrong "live" is the silent hour this module exists to prevent.
//! The original fact is ALWAYS committed to the requested target first — this
//! module never gates, redirects, or rewrites a send.

use std::collections::BTreeSet;

use crate::retraction;
use crate::store::{FRESHNESS_FRESH, Fact, FactKind, RoomSnapshot, is_system_authored};

/// Kinds that prove a target CONSUMED its inbox rather than merely existing.
///
/// `presence` is deliberately absent. A presence row is written by
/// `ensure_presence` on any rally call and by the coordination hook on the
/// target's behalf, so it proves a process touched the room — not that anyone
/// read the handoff. On the measured ledger, counting presence as delivery
/// hides `fact_ad0d_18d3f4e4e8c9b3f0` (seq 6243), whose target posted 1,659
/// later events and still never acknowledged the request addressed to it.
const CONSUMPTION_KINDS: &[FactKind] = &[
    FactKind::Read,
    FactKind::Resolve,
    FactKind::Receipt,
    FactKind::Decision,
    FactKind::Handoff,
    FactKind::Artifact,
];

/// The base-tool inbox for a suffixed session id: `codex:motion-review` →
/// `codex`, `claude_code:1380685e-…` → `claude_code`.
///
/// `None` when the id carries no suffix (it IS the base inbox — there is
/// nowhere further to fall back to) or when either side would be empty.
/// Splits on the FIRST `:` because session ids nest (`codex:01a0-sim:retry`);
/// the family inbox is the outermost segment.
pub(crate) fn base_tool(target: &str) -> Option<&str> {
    let (base, suffix) = target.split_once(':')?;
    if base.is_empty() || suffix.is_empty() {
        return None;
    }
    Some(base)
}

/// Seconds between two RFC3339 stamps, or `None` if either fails to parse.
/// Never negative — a future stamp reads as age 0 rather than as freshness.
fn age_between(earlier: &str, now: &str) -> Option<i64> {
    let earlier = chrono::DateTime::parse_from_rfc3339(earlier).ok()?;
    let now = chrono::DateTime::parse_from_rfc3339(now).ok()?;
    Some((now - earlier).num_seconds().max(0))
}

// =============================================================================
// Send-time liveness
// =============================================================================

/// Why a handoff did or did not earn a fallback copy. Recorded verbatim in the
/// `delivery` JSON block so a reader never has to re-derive the decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FallbackReason {
    /// Target's presence is inside its window — normal pull delivery works.
    TargetLive,
    /// `--target-policy exact`, or a `--ref`-bound reply. Fan-out is forbidden.
    PolicyForbids,
    /// Target is already a base inbox; there is no broader one.
    NoBaseInbox,
    /// Target is not live and a base inbox exists — copy sent.
    Delivered,
}

impl FallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TargetLive => "target_live",
            Self::PolicyForbids => "policy_forbids_fallback",
            Self::NoBaseInbox => "no_base_inbox",
            Self::Delivered => "fallback_delivered",
        }
    }
}

/// The send-time delivery decision for one handoff. Pure over the snapshot so
/// the policy is testable without a store — the established rally-cli
/// convention for liveness math.
#[derive(Clone, Debug)]
pub(crate) struct DeliveryPlan {
    /// Target's presence is inside its adaptive window.
    pub(crate) target_live: bool,
    /// Target's last presence stamp, `None` when it has never been seen.
    pub(crate) last_seen: Option<String>,
    /// Age of that stamp in seconds.
    pub(crate) last_seen_age_secs: Option<i64>,
    /// Base inbox the copy went to, `None` when no copy was sent.
    pub(crate) fallback_inbox: Option<String>,
    pub(crate) reason: FallbackReason,
}

/// Decide where a handoff to `target` must go.
///
/// `liveness_window_secs` pins a FLAT window when the operator has configured
/// one (`coordination.handoff_liveness_window_secs` /
/// `RALLY_HANDOFF_LIVENESS_WINDOW_SECS`). Left `None`, liveness comes from the
/// squad row's own adaptive window — the same measurement `rally room` and
/// `stale_target_warning` already publish, so one send and one room read can
/// never disagree about whether a peer is alive.
///
/// `fallback_forbidden` is the caller's policy verdict, not ours: `--ref`-bound
/// replies are bound to exactly one receiver by protocol, and
/// `--target-policy exact` is the operator saying the same thing explicitly.
pub(crate) fn delivery_plan(
    snapshot: &RoomSnapshot,
    target: &str,
    liveness_window_secs: Option<i64>,
    fallback_forbidden: bool,
) -> DeliveryPlan {
    let squad = snapshot.squad_for(target);
    let last_seen = squad.map(|sq| sq.last_seen_ts.clone());
    let last_seen_age_secs = squad.and_then(|sq| sq.age_secs);

    // A squad row that is missing entirely is the DEAD case, not the unknown
    // one: the projection drops sessions it has proven stale, so "no row" and
    // "long gone" are the same observation. Treating it as live would skip the
    // fallback for exactly the targets that need it most.
    let target_live = match (squad, liveness_window_secs) {
        (None, _) => false,
        (Some(sq), Some(window)) => sq.age_secs.is_some_and(|age| age <= window),
        (Some(sq), None) => sq.freshness == FRESHNESS_FRESH,
    };

    if target_live {
        return DeliveryPlan {
            target_live,
            last_seen,
            last_seen_age_secs,
            fallback_inbox: None,
            reason: FallbackReason::TargetLive,
        };
    }
    if fallback_forbidden {
        return DeliveryPlan {
            target_live,
            last_seen,
            last_seen_age_secs,
            fallback_inbox: None,
            reason: FallbackReason::PolicyForbids,
        };
    }
    match base_tool(target) {
        Some(base) => DeliveryPlan {
            target_live,
            last_seen,
            last_seen_age_secs,
            fallback_inbox: Some(base.to_string()),
            reason: FallbackReason::Delivered,
        },
        None => DeliveryPlan {
            target_live,
            last_seen,
            last_seen_age_secs,
            fallback_inbox: None,
            reason: FallbackReason::NoBaseInbox,
        },
    }
}

// =============================================================================
// The undelivered projection
// =============================================================================

/// One row of `rally handoffs`.
#[derive(Clone, Debug, schemars::JsonSchema, serde::Serialize)]
pub(crate) struct HandoffRow {
    pub(crate) event_id: String,
    pub(crate) seq: i64,
    pub(crate) from: Option<String>,
    pub(crate) target: String,
    pub(crate) subject: String,
    /// The handoff's own `created_at`. Read this, NOT the ledger's
    /// `occurred_at` column: a `migrate-legacy` replay re-stamps `occurred_at`
    /// with ingest time, so on a migrated room every event appears to have
    /// happened in the same few seconds
    /// (backlog `AGEN-RALLY-m2eb8e3491kt21zmt51de`).
    pub(crate) created_at: String,
    pub(crate) age_secs: Option<i64>,
    /// `true` when no consumption signal follows this handoff's sequence.
    pub(crate) undelivered: bool,
    pub(crate) target_last_seen: Option<String>,
    pub(crate) target_last_seen_age_secs: Option<i64>,
    /// `false` when the target never posted a single fact in this room — it was
    /// addressed but never existed here.
    pub(crate) target_ever_seen: bool,
    /// Base inbox that also holds a copy, when the send fell back.
    pub(crate) fallback_inbox: Option<String>,
}

/// Project every non-retracted, targeted handoff in the ledger.
///
/// Deliberately reads `facts` whole rather than `RoomSnapshot`. The snapshot's
/// `open_handoffs` is ranked by lease freshness, so a handoff that expired out
/// of view is invisible there precisely when it has been pending longest — on
/// the measured room, `open_handoffs` was EMPTY while eight handoffs sat
/// unconsumed. Lease expiry describes a claim's grip on a file; it says nothing
/// about whether a message was read.
///
/// `now` is injected so age math is pure and deterministically testable.
pub(crate) fn project_handoffs(
    facts: &[Fact],
    now: &str,
    undelivered_only: bool,
) -> Vec<HandoffRow> {
    let retracted = retraction::retracted_ids(facts);

    // One pass to index consumption, so the scan stays O(n) rather than
    // O(handoffs × facts). Rooms reach six figures of events.
    let mut consumed_by: Vec<(&str, i64)> = Vec::new();
    let mut answered: BTreeSet<&str> = BTreeSet::new();
    let mut last_presence: std::collections::BTreeMap<&str, &str> =
        std::collections::BTreeMap::new();
    let mut ever_seen: BTreeSet<&str> = BTreeSet::new();
    for fact in facts {
        if let Some(tool) = fact.tool.as_deref() {
            ever_seen.insert(tool);
            if fact.kind == FactKind::Presence {
                last_presence.insert(tool, fact.created_at.as_str());
            }
            if CONSUMPTION_KINDS.contains(&fact.kind) {
                consumed_by.push((tool, fact.seq));
            }
        }
        // An explicit reply discharges the handoff whoever wrote it — a lead
        // answering on a worker's behalf still means the request was handled.
        //
        // EXCEPT Rally's own bookkeeping. The wake fact `deliver_handoff_fallback`
        // appends cites the handoff it could not deliver; counting that as a
        // reply marks a handoff ANSWERED at the exact moment it was proven
        // undeliverable, hiding every fallback case from this view. Measured on
        // the first end-to-end run: 3 of 3 fallback sends read as delivered.
        if let Some(ref_id) = fact.ref_id.as_deref()
            && fact.kind != FactKind::Wake
            && !is_system_authored(fact)
        {
            answered.insert(ref_id);
        }
    }

    let mut rows: Vec<HandoffRow> = Vec::new();
    for fact in facts {
        if fact.kind != FactKind::Handoff {
            continue;
        }
        let Some(target) = fact.target.as_deref() else {
            continue;
        };
        // A broadcast has no addressee whose silence could be measured.
        if target == "all" || target.is_empty() {
            continue;
        }
        if retracted.contains(&fact.event_id) {
            continue;
        }
        let undelivered = !answered.contains(fact.event_id.as_str())
            && !consumed_by
                .iter()
                .any(|(tool, seq)| *tool == target && *seq > fact.seq);
        if undelivered_only && !undelivered {
            continue;
        }
        let target_last_seen = last_presence.get(target).map(|ts| (*ts).to_string());
        rows.push(HandoffRow {
            event_id: fact.event_id.clone(),
            seq: fact.seq,
            from: fact.tool.clone(),
            target: target.to_string(),
            subject: fact.subject.clone(),
            created_at: fact.created_at.clone(),
            age_secs: age_between(&fact.created_at, now),
            undelivered,
            target_last_seen_age_secs: target_last_seen
                .as_deref()
                .and_then(|seen| age_between(seen, now)),
            target_last_seen,
            target_ever_seen: ever_seen.contains(target),
            fallback_inbox: fallback_copy_inbox(facts, &fact.event_id),
        });
    }
    // Oldest first: the longest-unanswered handoff is the one most likely to
    // have been forgotten, so it leads the list rather than trailing it.
    rows.sort_by_key(|row| row.seq);
    rows
}

/// The base inbox holding a fallback copy of `event_id`, if one was sent.
/// Reads the marker [`FALLBACK_OF_MARKER`] that `command_say` writes.
fn fallback_copy_inbox(facts: &[Fact], event_id: &str) -> Option<String> {
    facts
        .iter()
        .find(|fact| {
            fact.kind == FactKind::Handoff
                && fact
                    .evidence
                    .iter()
                    .any(|marker| marker == &format!("{FALLBACK_OF_MARKER}{event_id}"))
        })
        .and_then(|fact| fact.target.clone())
}

/// Evidence marker linking a fallback copy back to the handoff it duplicates.
/// A plain `key:value` string in the existing `evidence` array — no schema
/// bump, and older binaries replay it untouched.
pub(crate) const FALLBACK_OF_MARKER: &str = "delivery:fallback_of=";

/// Evidence marker recording WHY the copy was sent.
pub(crate) const FALLBACK_REASON_MARKER: &str = "delivery:fallback_reason=";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Squad;

    fn squad(tool: &str, freshness: &str, age: Option<i64>) -> Squad {
        Squad {
            tool: tool.to_string(),
            last_seen_seq: 1,
            last_seen_ts: "2026-09-13T22:20:01Z".to_string(),
            status: "idle".to_string(),
            acknowledged: false,
            age_secs: age,
            window_secs: 1860,
            freshness: freshness.to_string(),
        }
    }

    fn snapshot_with(squads: Vec<Squad>) -> RoomSnapshot {
        RoomSnapshot {
            squads,
            ..RoomSnapshot::default()
        }
    }

    fn handoff(seq: i64, event_id: &str, from: &str, target: Option<&str>) -> Fact {
        Fact {
            event_id: event_id.to_string(),
            seq,
            kind: FactKind::Handoff,
            tool: Some(from.to_string()),
            target: target.map(str::to_string),
            subject: format!("handoff {seq}"),
            created_at: "2026-09-13T22:53:00Z".to_string(),
            ..Fact::default()
        }
    }

    fn act(seq: i64, kind: FactKind, tool: &str) -> Fact {
        Fact {
            event_id: format!("act_{seq}"),
            seq,
            kind,
            tool: Some(tool.to_string()),
            created_at: "2026-09-13T23:00:00Z".to_string(),
            ..Fact::default()
        }
    }

    // ---- base_tool -----------------------------------------------------

    #[test]
    fn base_tool_strips_the_session_suffix() {
        assert_eq!(base_tool("codex:motion-review"), Some("codex"));
        assert_eq!(base_tool("claude_code:1380685e-2378"), Some("claude_code"));
    }

    #[test]
    fn base_tool_splits_on_the_first_colon_for_nested_ids() {
        assert_eq!(base_tool("codex:01a0931a-sim:retry"), Some("codex"));
    }

    #[test]
    fn base_tool_is_none_when_the_target_is_already_a_base_inbox() {
        assert_eq!(base_tool("codex"), None);
        assert_eq!(base_tool("claude_code"), None);
    }

    #[test]
    fn base_tool_refuses_a_malformed_id_rather_than_inventing_an_inbox() {
        assert_eq!(base_tool(":orphan"), None);
        assert_eq!(base_tool("codex:"), None);
        assert_eq!(base_tool(""), None);
    }

    // ---- delivery_plan -------------------------------------------------

    #[test]
    fn a_fresh_target_gets_no_fallback_copy() {
        let snap = snapshot_with(vec![squad("codex:live", FRESHNESS_FRESH, Some(30))]);
        let plan = delivery_plan(&snap, "codex:live", None, false);
        assert!(plan.target_live);
        assert_eq!(plan.fallback_inbox, None);
        assert_eq!(plan.reason, FallbackReason::TargetLive);
    }

    #[test]
    fn a_stale_target_falls_back_to_its_base_inbox() {
        let snap = snapshot_with(vec![squad("codex:motion-review", "stale", Some(1_980))]);
        let plan = delivery_plan(&snap, "codex:motion-review", None, false);
        assert!(!plan.target_live);
        assert_eq!(plan.fallback_inbox.as_deref(), Some("codex"));
        assert_eq!(plan.reason, FallbackReason::Delivered);
        assert_eq!(plan.last_seen_age_secs, Some(1_980));
    }

    /// The measured defect: the dead target had NO squad row at all, because
    /// the projection drops sessions it has proven stale. Reading a missing row
    /// as "live" would skip the fallback for exactly this case.
    #[test]
    fn a_target_with_no_squad_row_is_treated_as_dead_not_as_unknown() {
        let snap = snapshot_with(vec![squad("codex:someone-else", FRESHNESS_FRESH, Some(5))]);
        let plan = delivery_plan(&snap, "codex:motion-review", None, false);
        assert!(!plan.target_live);
        assert_eq!(plan.fallback_inbox.as_deref(), Some("codex"));
        assert_eq!(plan.last_seen, None);
    }

    #[test]
    fn an_explicit_policy_suppresses_the_fallback_copy() {
        let snap = snapshot_with(vec![squad("codex:motion-review", "stale", Some(9_000))]);
        let plan = delivery_plan(&snap, "codex:motion-review", None, true);
        assert!(!plan.target_live);
        assert_eq!(plan.fallback_inbox, None);
        assert_eq!(plan.reason, FallbackReason::PolicyForbids);
    }

    #[test]
    fn a_dead_base_target_has_nowhere_to_fall_back_to_and_says_so() {
        let snap = snapshot_with(vec![squad("codex", "stale", Some(9_000))]);
        let plan = delivery_plan(&snap, "codex", None, false);
        assert_eq!(plan.reason, FallbackReason::NoBaseInbox);
        // Dead end: not live AND nowhere to copy to. Nothing more is possible
        // automatically, so the warning is the whole remedy.
        assert!(!plan.target_live && plan.fallback_inbox.is_none());
    }

    #[test]
    fn a_configured_flat_window_overrides_the_adaptive_verdict() {
        // The squad row says STALE by its own adaptive window; a generous
        // operator-pinned window must win, and vice versa.
        let snap = snapshot_with(vec![squad("codex:slow", "stale", Some(1_800))]);
        assert!(delivery_plan(&snap, "codex:slow", Some(3_600), false).target_live);
        assert!(!delivery_plan(&snap, "codex:slow", Some(900), false).target_live);
    }

    #[test]
    fn a_squad_row_with_an_unparseable_age_is_not_live_under_a_pinned_window() {
        let snap = snapshot_with(vec![squad("codex:ghost", "unknown", None)]);
        let plan = delivery_plan(&snap, "codex:ghost", Some(3_600), false);
        assert!(!plan.target_live);
        assert_eq!(plan.fallback_inbox.as_deref(), Some("codex"));
    }

    // ---- project_handoffs ----------------------------------------------

    const NOW: &str = "2026-09-14T00:53:00Z";

    #[test]
    fn a_handoff_the_target_never_consumed_is_undelivered() {
        let facts = vec![
            handoff(10, "fact_dead", "codex:sol-v6", Some("codex:motion-review")),
            // The target's own activity is all BEFORE the handoff.
            act(5, FactKind::Read, "codex:motion-review"),
        ];
        let rows = project_handoffs(&facts, NOW, true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, "fact_dead");
        assert!(rows[0].undelivered);
        assert_eq!(rows[0].age_secs, Some(7_200));
    }

    #[test]
    fn a_handoff_the_target_read_afterwards_is_delivered() {
        let facts = vec![
            handoff(10, "fact_ok", "codex:sol-v6", Some("codex:motion-review")),
            act(11, FactKind::Read, "codex:motion-review"),
        ];
        assert!(project_handoffs(&facts, NOW, true).is_empty());
        let all = project_handoffs(&facts, NOW, false);
        assert_eq!(all.len(), 1);
        assert!(!all[0].undelivered);
    }

    /// Presence proves a process touched the room, not that anyone read the
    /// inbox — `ensure_presence` writes it on every rally call, and the
    /// coordination hook writes it on the agent's behalf.
    #[test]
    fn presence_after_a_handoff_does_not_count_as_consumption() {
        let facts = vec![
            handoff(10, "fact_dead", "codex:sol-v6", Some("codex:motion-review")),
            act(11, FactKind::Presence, "codex:motion-review"),
            act(12, FactKind::Risk, "codex:motion-review"),
        ];
        let rows = project_handoffs(&facts, NOW, true);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].target_ever_seen);
    }

    #[test]
    fn a_reply_citing_the_handoff_discharges_it_whoever_wrote_it() {
        let mut reply = act(11, FactKind::Resolve, "some-lead");
        reply.ref_id = Some("fact_ok".to_string());
        let facts = vec![
            handoff(10, "fact_ok", "codex:sol-v6", Some("codex:motion-review")),
            reply,
        ];
        assert!(project_handoffs(&facts, NOW, true).is_empty());
    }

    /// Rally's own undeliverable-wake cites the handoff it failed to deliver.
    /// Reading that as a reply would mark the handoff answered at the exact
    /// moment it was proven undeliverable.
    #[test]
    fn rallys_own_wake_does_not_discharge_the_handoff_it_reports_on() {
        let mut wake = act(11, FactKind::Wake, "codex:sol-v6");
        wake.ref_id = Some("fact_dead".to_string());
        let mut system_note = act(12, FactKind::Artifact, "rally");
        system_note.ref_id = Some("fact_dead".to_string());
        system_note.role = Some("system".to_string());
        let facts = vec![
            handoff(10, "fact_dead", "codex:sol-v6", Some("codex:motion-review")),
            wake,
            system_note,
        ];
        let rows = project_handoffs(&facts, NOW, true);
        assert_eq!(rows.len(), 1, "a system wake must not discharge a handoff");
        assert!(rows[0].undelivered);
    }

    #[test]
    fn a_broadcast_handoff_is_not_an_undelivered_candidate() {
        let facts = vec![
            handoff(10, "fact_all", "codex:sol-v6", Some("all")),
            handoff(11, "fact_none", "codex:sol-v6", None),
        ];
        assert!(project_handoffs(&facts, NOW, false).is_empty());
    }

    #[test]
    fn a_retracted_handoff_is_dropped() {
        let facts = vec![
            handoff(
                10,
                "fact_wrong",
                "codex:sol-v6",
                Some("codex:motion-review"),
            ),
            Fact {
                event_id: "fact_retract".to_string(),
                seq: 11,
                kind: FactKind::Artifact,
                tool: Some("codex:sol-v6".to_string()),
                subject: retraction::subject_for("fact_wrong"),
                ref_id: Some("fact_wrong".to_string()),
                created_at: "2026-09-13T23:00:00Z".to_string(),
                ..Fact::default()
            },
        ];
        assert!(project_handoffs(&facts, NOW, false).is_empty());
    }

    #[test]
    fn a_target_that_never_posted_anything_reports_target_ever_seen_false() {
        let facts = vec![handoff(
            10,
            "fact_ghost",
            "codex:sol-v6",
            Some("claude_code:never-existed"),
        )];
        let rows = project_handoffs(&facts, NOW, true);
        assert!(!rows[0].target_ever_seen);
        assert_eq!(rows[0].target_last_seen, None);
    }

    #[test]
    fn a_fallback_copy_is_reported_against_the_handoff_it_duplicates() {
        let mut copy = handoff(11, "fact_copy", "codex:sol-v6", Some("codex"));
        copy.evidence = vec![format!("{FALLBACK_OF_MARKER}fact_dead")];
        let facts = vec![
            handoff(10, "fact_dead", "codex:sol-v6", Some("codex:motion-review")),
            copy,
        ];
        let rows = project_handoffs(&facts, NOW, true);
        let original = rows.iter().find(|r| r.event_id == "fact_dead").unwrap();
        assert_eq!(original.fallback_inbox.as_deref(), Some("codex"));
    }

    #[test]
    fn rows_are_ordered_oldest_first() {
        let facts = vec![
            handoff(30, "c", "codex:sol-v6", Some("codex:a")),
            handoff(10, "a", "codex:sol-v6", Some("codex:b")),
            handoff(20, "b", "codex:sol-v6", Some("codex:c")),
        ];
        let ids: Vec<String> = project_handoffs(&facts, NOW, true)
            .into_iter()
            .map(|r| r.event_id)
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_unparseable_timestamp_yields_no_age_rather_than_a_wrong_one() {
        let mut h = handoff(
            10,
            "fact_bad_ts",
            "codex:sol-v6",
            Some("codex:motion-review"),
        );
        h.created_at = "not-a-timestamp".to_string();
        let facts = vec![h];
        let rows = project_handoffs(&facts, NOW, true);
        assert_eq!(rows[0].age_secs, None);
        assert!(rows[0].undelivered);
    }
}
