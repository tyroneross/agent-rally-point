// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Agent-state coordination model — typed, liveness-aware projection over the
//! append-only ledger.
//!
//! ## Charter alignment
//! This module is a **pure projection** over `FactKind::Presence` facts. It
//! never writes, never spawns, never schedules. Consistent with the rally
//! charter: facilitator, not executor.
//!
//! ## Vocabulary
//! Each agent has one current [`AgentState`]:
//!
//! - [`AgentState::Idle`] — last heartbeat carried `state=idle`. May carry an
//!   optional `wake_after` timestamp (mirrors the `Standby` marker grammar).
//! - [`AgentState::Working`] — last heartbeat carried `state=working`, plus
//!   `file=<path>` and `intent=<one-line>`.
//! - [`AgentState::Blocked`] — last heartbeat carried `state=blocked`, plus
//!   `ref=<event-id>` naming the blocking fact.
//! - [`AgentState::Done`] — last heartbeat carried `state=done`, plus
//!   `committed_sha=<hash>` and `worktree_branch=<branch>`. This is the
//!   **done producer seam**: Codex, Claude Code, or any other Rally participant
//!   detects or reports that a commit landed on a managed worktree branch and
//!   posts the `done` heartbeat. The projection consumes the fact unchanged —
//!   no git-side reasoning here.
//!
//! ## Marker grammar
//! Markers live in the presence fact's `subject` and (for `done` only) the
//! `summary`. The grammar is `key=value` pairs separated by ` | ` (space-pipe-
//! space). This is the **existing additive-marker convention** already in
//! `Standby` (`reason:`, `wake_after:`), `Mission` (`role:lead`), and
//! `Presence` (`build_id:`). Reusing it avoids a schema bump while staying
//! forward-compat: a future PR can promote `state` to a typed Fact field and
//! [`project_agent_states`] will read both shapes.
//!
//! ## Liveness
//! An agent whose latest presence fact is older than [`IDLE_THRESHOLD_SECS`]
//! (15 minutes, mirrors `store::IDLE_THRESHOLD_SECS`) is marked `stale: true`.
//! The board surface uses this to (a) suppress stale agents from "current
//! ownership" views and (b) list their claims as `auto_releasable_claims[]`
//! for an operator/lead to action.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;

use crate::store::{Fact, FactKind};

/// Liveness threshold (seconds). Mirrors `store::IDLE_THRESHOLD_SECS` — keep in
/// sync; both define "stale" for projection purposes. 15 minutes is generous
/// enough that an agent doing a long compute does not flicker out.
pub(crate) const IDLE_THRESHOLD_SECS: i64 = 15 * 60;

/// Typed agent-state vocabulary.
///
/// Serialised as a flat tagged enum: `{"state": "working", "file": "...", "intent": "..."}`
/// for forward-compat with host consumers that already index on `state`.
#[derive(Clone, Debug, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AgentState {
    /// Agent is alive but not actively working. May carry an `wake_after`
    /// timestamp (mirrors `Standby` marker grammar).
    Idle {
        #[serde(skip_serializing_if = "Option::is_none")]
        wake_after: Option<String>,
    },
    /// Agent is actively working on `file` with the stated `intent`.
    Working { file: String, intent: String },
    /// Agent is blocked on `ref` (an event_id of a blocker/handoff/etc).
    Blocked { #[serde(rename = "ref")] ref_id: String },
    /// Agent finished a commit on a managed worktree branch. Authored by any
    /// Rally participant's committing lane and consumed here unchanged.
    Done {
        committed_sha: String,
        worktree_branch: String,
    },
    /// Marker grammar present but state not parseable. Surfaced as-is so an
    /// older binary writer that uses an unknown state value doesn't disappear
    /// from the board entirely.
    Unknown { raw: String },
}

/// One projected entry per agent.
///
/// `stale=true` means `last_seen_ts` is older than [`IDLE_THRESHOLD_SECS`] vs
/// the projection's `now_ts`. Stale entries are still emitted so a host can
/// reason about "who used to be here"; consumers should hide stale entries
/// from "current owners" views.
#[derive(Clone, Debug, JsonSchema, PartialEq, Eq, Serialize)]
pub(crate) struct AgentStateEntry {
    pub(crate) tool: String,
    #[serde(flatten)]
    pub(crate) state: AgentState,
    pub(crate) last_seen_seq: i64,
    pub(crate) last_seen_ts: String,
    pub(crate) stale: bool,
}

// ---------------------------------------------------------------------------
// Marker parsing
// ---------------------------------------------------------------------------

/// Parse a marker-bearing string into a key→value map.
///
/// Format: `key1=v1 | key2=v2 | key3=v3` — exactly the existing presence
/// convention (e.g. `state=working | file=… | intent=…`). Tolerant:
///
/// - Whitespace around `|` and `=` is trimmed.
/// - A segment with no `=` is recorded with empty value (lets the projection
///   distinguish "marker absent" from "marker explicitly empty").
/// - Duplicate keys: last value wins (matches host consumer reading-order
///   intuition).
///
/// Returns a `BTreeMap<String,String>` (stable iteration order for tests).
pub(crate) fn parse_marker_string(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for segment in s.split('|') {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some((k, v)) = seg.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            // segment without `=` — keep as a flag (empty value).
            out.insert(seg.to_string(), String::new());
        }
    }
    out
}

/// Project a single presence fact into an [`AgentState`].
///
/// Reads markers from `fact.subject` first, then falls back to `fact.summary`
/// (so e.g. a `done` fact may carry `committed_sha` / `worktree_branch` in
/// either place). Returns `None` when the fact carries no `state=` marker at
/// all (e.g. the legacy `agent presence: <tool>` shape ensure_presence_tiered
/// writes); the projection layer treats those as `Idle{wake_after: None}`.
pub(crate) fn project_presence_to_state(fact: &Fact) -> Option<AgentState> {
    let mut markers = parse_marker_string(&fact.subject);
    if let Some(summary) = fact.summary.as_deref() {
        for (k, v) in parse_marker_string(summary) {
            // subject wins; summary fills gaps.
            markers.entry(k).or_insert(v);
        }
    }
    let state = markers.get("state").cloned()?;
    Some(match state.as_str() {
        "idle" => AgentState::Idle {
            wake_after: markers.get("wake_after").cloned(),
        },
        "working" => AgentState::Working {
            file: markers.get("file").cloned().unwrap_or_default(),
            intent: markers.get("intent").cloned().unwrap_or_default(),
        },
        "blocked" => AgentState::Blocked {
            ref_id: markers.get("ref").cloned().unwrap_or_default(),
        },
        "done" => AgentState::Done {
            committed_sha: markers.get("committed_sha").cloned().unwrap_or_default(),
            worktree_branch: markers.get("worktree_branch").cloned().unwrap_or_default(),
        },
        other => AgentState::Unknown {
            raw: other.to_string(),
        },
    })
}

// ---------------------------------------------------------------------------
// Liveness-aware projection
// ---------------------------------------------------------------------------

/// Project the latest agent-state per tool from the room's facts.
///
/// Inputs: the full facts list (chronological — caller already loaded via
/// `room.facts()`) and `now_ts` (RFC3339 string — caller may pass
/// `crate::now_string()`). The `now_ts` parameter is explicit so tests can
/// pin the liveness clock.
///
/// Algorithm:
/// 1. Filter to `FactKind::Presence` facts only — keep the projection narrow.
/// 2. Group by `tool` (exclude `"rally"` — reserved system author).
/// 3. Keep the highest-`seq` presence per tool.
/// 4. Project to `AgentState` via [`project_presence_to_state`]; missing
///    marker → `Idle{wake_after: None}`.
/// 5. Compute `stale = (now_ts - last_seen_ts) > IDLE_THRESHOLD_SECS`. If
///    either timestamp is unparseable, treat as not stale (conservative —
///    matches existing squad-projection behaviour in store.rs).
///
/// Returns entries sorted by `tool` for stable host consumption.
pub(crate) fn project_agent_states(facts: &[Fact], now_ts: &str) -> Vec<AgentStateEntry> {
    let mut latest: BTreeMap<String, &Fact> = BTreeMap::new();
    for fact in facts.iter().filter(|f| f.kind == FactKind::Presence) {
        let Some(tool) = fact.tool.as_deref() else {
            continue;
        };
        if tool == "rally" {
            continue;
        }
        let entry = latest.entry(tool.to_string()).or_insert(fact);
        if fact.seq > entry.seq {
            *entry = fact;
        }
    }

    let now_secs = chrono::DateTime::parse_from_rfc3339(now_ts)
        .map(|dt| dt.timestamp())
        .ok();

    latest
        .into_iter()
        .map(|(tool, fact)| {
            let state = project_presence_to_state(fact)
                .unwrap_or(AgentState::Idle { wake_after: None });
            let seen_secs = chrono::DateTime::parse_from_rfc3339(&fact.created_at)
                .map(|dt| dt.timestamp())
                .ok();
            let stale = match (now_secs, seen_secs) {
                (Some(n), Some(s)) => (n - s) > IDLE_THRESHOLD_SECS,
                _ => false,
            };
            AgentStateEntry {
                tool,
                state,
                last_seen_seq: fact.seq,
                last_seen_ts: fact.created_at.clone(),
                stale,
            }
        })
        .collect()
}

/// The set of tools currently considered stale by [`project_agent_states`].
/// Helper for board projections — keeps stale-owner logic in one place.
pub(crate) fn stale_tools(states: &[AgentStateEntry]) -> std::collections::BTreeSet<String> {
    states
        .iter()
        .filter(|s| s.stale)
        .map(|s| s.tool.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FACT_SCHEMA;
    use crate::store::{Fact, FactKind};

    fn presence(
        tool: &str,
        seq: i64,
        subject: &str,
        summary: Option<&str>,
        created_at: &str,
    ) -> Fact {
        Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: format!("ev-{tool}-{seq}"),
            seq,
            thread_id: format!("t-{tool}-{seq}"),
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            role: None,
            subject: subject.to_string(),
            scope: Vec::new(),
            created_at: created_at.to_string(),
            summary: summary.map(|s| s.to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    // ── parse_marker_string ──────────────────────────────────────────────────

    #[test]
    fn parse_marker_string_handles_canonical_presence_subject() {
        let m = parse_marker_string(
            "state=working | file=crates/rally-cli | intent=agent-state model",
        );
        assert_eq!(m.get("state").map(String::as_str), Some("working"));
        assert_eq!(m.get("file").map(String::as_str), Some("crates/rally-cli"));
        assert_eq!(m.get("intent").map(String::as_str), Some("agent-state model"));
    }

    #[test]
    fn parse_marker_string_tolerates_whitespace_variation() {
        let m = parse_marker_string("  state =  idle  |wake_after=  2026-06-04T22:30:00Z");
        assert_eq!(m.get("state").map(String::as_str), Some("idle"));
        assert_eq!(
            m.get("wake_after").map(String::as_str),
            Some("2026-06-04T22:30:00Z")
        );
    }

    #[test]
    fn parse_marker_string_records_flag_segment_without_eq() {
        let m = parse_marker_string("state=blocked | needs-review | ref=abc123");
        assert_eq!(m.get("state").map(String::as_str), Some("blocked"));
        assert_eq!(m.get("needs-review").map(String::as_str), Some(""));
        assert_eq!(m.get("ref").map(String::as_str), Some("abc123"));
    }

    #[test]
    fn parse_marker_string_dedups_with_last_value_wins() {
        let m = parse_marker_string("state=idle | state=working | file=x.rs");
        assert_eq!(m.get("state").map(String::as_str), Some("working"));
        assert_eq!(m.get("file").map(String::as_str), Some("x.rs"));
    }

    #[test]
    fn parse_marker_string_empty_returns_empty_map() {
        assert!(parse_marker_string("").is_empty());
        assert!(parse_marker_string("   |   |  ").is_empty());
    }

    // ── project_presence_to_state ────────────────────────────────────────────

    #[test]
    fn legacy_presence_subject_with_no_state_marker_returns_none() {
        let f = presence("claude_code", 1, "agent presence: claude_code", None, "2026-06-04T22:00:00Z");
        assert_eq!(project_presence_to_state(&f), None);
    }

    #[test]
    fn working_state_extracts_file_and_intent() {
        let f = presence(
            "claude_code",
            10,
            "state=working | file=crates/rally-cli | intent=agent-state",
            None,
            "2026-06-04T22:00:00Z",
        );
        match project_presence_to_state(&f).unwrap() {
            AgentState::Working { file, intent } => {
                assert_eq!(file, "crates/rally-cli");
                assert_eq!(intent, "agent-state");
            }
            other => panic!("expected Working, got {other:?}"),
        }
    }

    #[test]
    fn idle_state_extracts_optional_wake_after() {
        let f1 = presence("a", 1, "state=idle", None, "2026-06-04T22:00:00Z");
        match project_presence_to_state(&f1).unwrap() {
            AgentState::Idle { wake_after } => assert_eq!(wake_after, None),
            other => panic!("expected Idle, got {other:?}"),
        }
        let f2 = presence("a", 2, "state=idle | wake_after=2026-06-04T23:00:00Z", None, "2026-06-04T22:00:00Z");
        match project_presence_to_state(&f2).unwrap() {
            AgentState::Idle { wake_after } => {
                assert_eq!(wake_after.as_deref(), Some("2026-06-04T23:00:00Z"));
            }
            other => panic!("expected Idle, got {other:?}"),
        }
    }

    #[test]
    fn blocked_state_extracts_ref() {
        let f = presence(
            "a",
            1,
            "state=blocked | ref=fact_1234_abcd",
            None,
            "2026-06-04T22:00:00Z",
        );
        match project_presence_to_state(&f).unwrap() {
            AgentState::Blocked { ref_id } => assert_eq!(ref_id, "fact_1234_abcd"),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// Producer seam test — a synthetic `done` fact authored by Codex (one
    /// committed→mergeable producer) is consumed unchanged by the projection.
    /// Locks the wire vocabulary: `state=done | committed_sha=<h> | worktree_branch=<b>`.
    #[test]
    fn project_agent_states_recognises_done_from_synthetic_codex_fact() {
        let codex_done = presence(
            "codex",
            42,
            "state=done | committed_sha=abc123def456 | worktree_branch=feat/committed-mergeable",
            Some("Codex committed-to-mergeable lane: detected commit landed"),
            "2026-06-04T22:30:00Z",
        );
        match project_presence_to_state(&codex_done).unwrap() {
            AgentState::Done {
                committed_sha,
                worktree_branch,
            } => {
                assert_eq!(committed_sha, "abc123def456");
                assert_eq!(worktree_branch, "feat/committed-mergeable");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        // and through project_agent_states with a fresh now_ts
        let states = project_agent_states(&[codex_done], "2026-06-04T22:31:00Z");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].tool, "codex");
        assert!(!states[0].stale, "1-minute lag should not be stale");
    }

    #[test]
    fn done_marker_in_summary_is_picked_up_when_subject_only_has_state() {
        let f = presence(
            "codex",
            10,
            "state=done",
            Some("committed_sha=deadbeef | worktree_branch=feat/x"),
            "2026-06-04T22:30:00Z",
        );
        match project_presence_to_state(&f).unwrap() {
            AgentState::Done {
                committed_sha,
                worktree_branch,
            } => {
                assert_eq!(committed_sha, "deadbeef");
                assert_eq!(worktree_branch, "feat/x");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn unknown_state_falls_back_to_unknown_variant() {
        let f = presence(
            "a",
            1,
            "state=napping | reason=tea-break",
            None,
            "2026-06-04T22:00:00Z",
        );
        match project_presence_to_state(&f).unwrap() {
            AgentState::Unknown { raw } => assert_eq!(raw, "napping"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // ── project_agent_states ─────────────────────────────────────────────────

    #[test]
    fn project_agent_states_keeps_only_highest_seq_per_tool() {
        let facts = vec![
            presence("alpha", 1, "state=idle", None, "2026-06-04T22:00:00Z"),
            presence(
                "alpha",
                5,
                "state=working | file=x.rs | intent=fix",
                None,
                "2026-06-04T22:05:00Z",
            ),
            presence("beta", 2, "state=working | file=y.rs | intent=tidy", None, "2026-06-04T22:01:00Z"),
        ];
        let states = project_agent_states(&facts, "2026-06-04T22:06:00Z");
        assert_eq!(states.len(), 2);
        let alpha = states.iter().find(|s| s.tool == "alpha").unwrap();
        assert!(matches!(alpha.state, AgentState::Working { .. }));
        assert_eq!(alpha.last_seen_seq, 5);
    }

    #[test]
    fn project_agent_states_excludes_rally_system_author() {
        let facts = vec![
            presence("rally", 1, "state=working", None, "2026-06-04T22:00:00Z"),
            presence("real-agent", 2, "state=working | file=x | intent=y", None, "2026-06-04T22:01:00Z"),
        ];
        let states = project_agent_states(&facts, "2026-06-04T22:02:00Z");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].tool, "real-agent");
    }

    #[test]
    fn project_agent_states_ignores_non_presence_kinds() {
        let facts = vec![Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: "ev-c-1".into(),
            seq: 1,
            thread_id: "t".into(),
            kind: FactKind::Claim,
            tool: Some("a".into()),
            role: None,
            subject: "state=working | file=x | intent=y".into(),
            scope: vec!["file:x".into()],
            created_at: "2026-06-04T22:00:00Z".into(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }];
        // A claim that LOOKS like a presence subject must not be projected.
        let states = project_agent_states(&facts, "2026-06-04T22:01:00Z");
        assert!(states.is_empty());
    }

    #[test]
    fn stale_detected_when_last_seen_older_than_threshold() {
        let facts = vec![
            presence(
                "fresh",
                1,
                "state=working | file=x.rs | intent=fix",
                None,
                "2026-06-04T22:00:00Z",
            ),
            presence("stale", 2, "state=idle", None, "2026-06-02T10:00:00Z"),
        ];
        let states = project_agent_states(&facts, "2026-06-04T22:10:00Z");
        let fresh = states.iter().find(|s| s.tool == "fresh").unwrap();
        let stale = states.iter().find(|s| s.tool == "stale").unwrap();
        assert!(!fresh.stale, "10-minute lag is within the 15-minute threshold");
        assert!(stale.stale, "2-day lag must trip the threshold");
        let st = stale_tools(&states);
        assert!(st.contains("stale"));
        assert!(!st.contains("fresh"));
    }

    #[test]
    fn unparseable_timestamps_default_to_not_stale() {
        let facts = vec![presence("a", 1, "state=idle", None, "garbage-ts")];
        let states = project_agent_states(&facts, "also-garbage");
        assert_eq!(states.len(), 1);
        assert!(
            !states[0].stale,
            "unparseable ts must NOT silently mark active agents stale"
        );
    }

    #[test]
    fn legacy_presence_without_state_marker_becomes_idle_default() {
        let f = presence("legacy", 1, "agent presence: legacy", Some("build_id:abc"), "2026-06-04T22:00:00Z");
        let states = project_agent_states(&[f], "2026-06-04T22:01:00Z");
        assert_eq!(states.len(), 1);
        match &states[0].state {
            AgentState::Idle { wake_after } => assert_eq!(wake_after, &None),
            other => panic!("expected Idle, got {other:?}"),
        }
    }
}
