// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//! Append-only retraction — withdraw a posted fact without mutating the ledger.
//!
//! The ledger is append-only by construction, so a wrong fact posted via
//! `rally say` had no remedy: it kept re-surfacing in `rally room` and in
//! every agent's session-start context forever. A retraction is an APPENDED
//! fact naming the target's `event_id`; read-time projections drop the
//! withdrawn fact and keep the retraction, so peers see the correction
//! instead of the withdrawn claim. Nothing on disk is ever rewritten.
//!
//! Wire shape (cross-store on purpose — matches build-loop's resolver in
//! `scripts/rally_point/retraction.py`, which cannot rely on a first-class
//! kind because unknown kinds remap to `artifact` in older binaries):
//!
//! ```text
//! kind    : artifact
//! subject : "retract: <target-event-id>"           (anchored; single token)
//! ref     : <target-event-id>
//! summary : "<reason> [retracts=<id>[ superseded_by=<id>]]"
//! ```
//!
//! Detection reads the same three carriers in order: the anchored subject,
//! the `ref` field on a `retract:`-subject fact, and the `retracts=<id>`
//! summary token. A fact merely *discussing* retraction ("how do we retract:
//! a design note") never matches — the subject target must be one bare token.
//!
//! `superseded_by` is additive: a retraction may simply withdraw a fact, or
//! withdraw it AND point at the corrected fact that replaces it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::store::Fact;

/// Leading token of a retraction subject. The space matters: `subject_for`
/// always writes it, and detection tolerates arbitrary whitespace after the
/// colon but requires the prefix at the very start of the subject.
const SUBJECT_PREFIX: &str = "retract:";

/// Canonical retraction subject for `target`.
pub(crate) fn subject_for(target: &str) -> String {
    format!("retract: {target}")
}

/// Free-text summary that survives any store round-trip (the machine marker
/// block is the last-resort detection carrier).
pub(crate) fn summary_for(target: &str, reason: &str, superseded_by: Option<&str>) -> String {
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "retracted"
    } else {
        reason
    };
    match superseded_by {
        Some(by) => format!("{reason} [retracts={target} superseded_by={by}]"),
        None => format!("{reason} [retracts={target}]"),
    }
}

/// The subject's target token, if the subject is an anchored retraction
/// subject (`retract: <one-bare-token>` and nothing else).
fn subject_target(subject: &str) -> Option<&str> {
    let rest = subject.trim().strip_prefix(SUBJECT_PREFIX)?.trim();
    (!rest.is_empty() && !rest.contains(char::is_whitespace)).then_some(rest)
}

/// Extract the token following `marker=` in `text`, honoring a word boundary
/// before the marker and terminating at the first character outside the event
/// id charset (`[A-Za-z0-9_-]`) — so `[retracts=fact_a]` and `(retracts=fact_a)`
/// both yield the bare id regardless of which wrapper a writer used.
fn token_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(marker) {
        let start = search_from + rel;
        let boundary_ok = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_alphanumeric() && c != '_');
        let rest = &text[start + marker.len()..];
        if boundary_ok {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .unwrap_or(rest.len());
            let token = &rest[..end];
            if !token.is_empty() {
                return Some(token);
            }
        }
        search_from = start + marker.len();
    }
    None
}

/// The event id this fact retracts, or `None` if it is not a retraction.
///
/// Carriers, in order: anchored subject; `ref` on a `retract:`-subject fact;
/// `retracts=<id>` summary token.
pub(crate) fn target_of(fact: &Fact) -> Option<String> {
    if let Some(target) = subject_target(&fact.subject) {
        return Some(target.to_string());
    }
    // A subject that starts with the prefix but carries a multi-word target is
    // still honored when `ref` names the target — the subject shape drifted
    // but the intent is explicit and machine-readable.
    if fact.subject.trim().starts_with(SUBJECT_PREFIX)
        && let Some(ref_id) = fact.ref_id.as_deref()
        && !ref_id.is_empty()
    {
        return Some(ref_id.to_string());
    }
    fact.summary
        .as_deref()
        .and_then(|s| token_after(s, "retracts="))
        .map(str::to_string)
}

/// True when `fact` is a retraction record (regardless of its wire kind).
pub(crate) fn is_retraction(fact: &Fact) -> bool {
    target_of(fact).is_some()
}

/// The replacement event id a retraction points at, or `None`.
pub(crate) fn superseded_by_of(fact: &Fact) -> Option<String> {
    fact.summary
        .as_deref()
        .and_then(|s| token_after(s, "superseded_by="))
        .map(str::to_string)
}

/// What a surfaced correction needs to explain the withdrawal without
/// re-reading the ledger.
#[derive(Clone, Debug)]
pub(crate) struct RetractionInfo {
    pub(crate) event_id: String,
    pub(crate) superseded_by: Option<String>,
}

/// `{target_event_id → latest retraction}` over `facts`. A target retracted
/// more than once keeps the LAST retraction by seq — a later correction
/// supersedes an earlier one, matching the append-only reading of the log.
pub(crate) fn index(facts: &[Fact]) -> BTreeMap<String, RetractionInfo> {
    let mut out: BTreeMap<String, (i64, RetractionInfo)> = BTreeMap::new();
    for fact in facts {
        let Some(target) = target_of(fact) else {
            continue;
        };
        let info = RetractionInfo {
            event_id: fact.event_id.clone(),
            superseded_by: superseded_by_of(fact),
        };
        match out.get(&target) {
            Some((seq, _)) if *seq > fact.seq => {}
            _ => {
                out.insert(target, (fact.seq, info));
            }
        }
    }
    out.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

/// Event ids withdrawn by a retraction present in `facts`. Resolution is
/// batch-scoped: a retraction only neutralizes a target present in the same
/// batch — a fact a peer already consumed cannot be un-read.
pub(crate) fn retracted_ids(facts: &[Fact]) -> BTreeSet<String> {
    facts.iter().filter_map(target_of).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FactKind;

    fn fact(event_id: &str, subject: &str, summary: Option<&str>, ref_id: Option<&str>) -> Fact {
        Fact {
            event_id: event_id.to_string(),
            kind: FactKind::Artifact,
            subject: subject.to_string(),
            summary: summary.map(str::to_string),
            ref_id: ref_id.map(str::to_string),
            ..Fact::default()
        }
    }

    #[test]
    fn subject_carrier_detects_anchored_target() {
        let f = fact("r1", "retract: fact_abc_123", None, None);
        assert_eq!(target_of(&f).as_deref(), Some("fact_abc_123"));
        assert!(is_retraction(&f));
    }

    #[test]
    fn prose_mentioning_retract_is_not_a_retraction() {
        // Multi-word "target" → not a retraction subject.
        let f = fact("a1", "retract: a design note", None, None);
        assert_eq!(target_of(&f), None);
        // Prefix not at the start → not a retraction subject.
        let f = fact("a2", "how do we retract: fact_abc", None, None);
        assert_eq!(target_of(&f), None);
    }

    #[test]
    fn ref_carrier_rescues_multiword_retract_subject() {
        let f = fact("r2", "retract: the flaky claim", None, Some("fact_abc"));
        assert_eq!(target_of(&f).as_deref(), Some("fact_abc"));
    }

    #[test]
    fn summary_token_carrier_detects_target_and_superseded_by() {
        let f = fact(
            "r3",
            "correction posted",
            Some("wrong port number [retracts=fact_a superseded_by=fact_b]"),
            None,
        );
        assert_eq!(target_of(&f).as_deref(), Some("fact_a"));
        assert_eq!(superseded_by_of(&f).as_deref(), Some("fact_b"));
    }

    #[test]
    fn summary_token_requires_word_boundary() {
        let f = fact("a3", "x", Some("contretracts=fact_a"), None);
        assert_eq!(target_of(&f), None);
        let f = fact("a4", "x", Some("(retracts=fact_a)"), None);
        assert_eq!(target_of(&f).as_deref(), Some("fact_a"));
    }

    #[test]
    fn summary_for_round_trips_through_token_detection() {
        let s = summary_for("fact_a", "wrong port", Some("fact_b"));
        let f = fact("r4", "unrelated subject", Some(&s), None);
        assert_eq!(target_of(&f).as_deref(), Some("fact_a"));
        assert_eq!(superseded_by_of(&f).as_deref(), Some("fact_b"));
        let s = summary_for("fact_a", "  ", None);
        assert!(s.starts_with("retracted "));
    }

    #[test]
    fn index_keeps_last_retraction_by_seq() {
        let mut r1 = fact("r1", "retract: fact_a", None, None);
        r1.seq = 5;
        let mut r2 = fact(
            "r2",
            "retract: fact_a",
            Some("better reason [retracts=fact_a superseded_by=fact_c]"),
            None,
        );
        r2.seq = 9;
        // Out-of-order input still resolves to the higher-seq retraction.
        let idx = index(&[r2.clone(), r1]);
        let info = idx.get("fact_a").expect("indexed");
        assert_eq!(info.event_id, "r2");
        assert_eq!(info.superseded_by.as_deref(), Some("fact_c"));
    }

    #[test]
    fn retracted_ids_collects_all_targets() {
        let facts = vec![
            fact("r1", "retract: fact_a", None, None),
            fact("f2", "plain artifact", None, None),
            fact("r2", "x", Some("[retracts=fact_b]"), None),
        ];
        let ids = retracted_ids(&facts);
        assert!(ids.contains("fact_a") && ids.contains("fact_b"));
        assert_eq!(ids.len(), 2);
    }
}
