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
//! Detection reads TWO carriers, in order: the anchored subject, and the `ref`
//! field on a `retract:`-subject fact. A fact merely *discussing* retraction
//! ("how do we retract: a design note") never matches — the subject target must
//! be one bare token.
//!
//! # Why the `retracts=` summary token is emitted but NOT read (R2)
//!
//! It used to be a third detection carrier, and that made every gate that has
//! to reason about retraction cover three spellings of one act. This module's
//! defect class — a correct rule guarding one spelling while the ledger accepts
//! another (RC-029, ARP-R-01, ARP-R-02, RC-071) — gets worse with every extra
//! spelling, and the third one earned nothing: build-loop's resolver
//! (`scripts/rally_point/retraction.py`) writes the anchored subject
//! REDUNDANTLY alongside the token and checks the subject FIRST, so carrier 1
//! already catches every build-loop record. No sync path writes the token into
//! the native store without also writing the subject.
//!
//! The token stays in [`summary_for`] on purpose, for two reasons that are
//! about the OTHER store, not this one. build-loop's `superseded_by_of` reads
//! `superseded_by=` out of the same bracket block, and its `_clean_reason`
//! strips exactly `[retracts=...]` off the end of a reason before surfacing it
//! — so emitting `[superseded_by=x]` without the leading `retracts=` would
//! leave the wire marker in build-loop's human-facing prose. Emission is a
//! carrier for a peer reader; detection is a spelling this codebase's gates
//! must cover. They are allowed to differ, and here they should.
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

/// Free-text summary carrying the machine marker block.
///
/// The `retracts=` half is written for build-loop's resolver and for
/// [`superseded_by_of`]'s bracket parse; it is NOT a detection carrier here.
/// See the module header for why emission and detection differ.
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
/// id charset (`[A-Za-z0-9_-]`) — so `superseded_by=fact_b]` and
/// `superseded_by=fact_b)` both yield the bare id regardless of which wrapper a
/// writer used. The word boundary is what keeps `contrasuperseded_by=x` from
/// matching.
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
/// TWO carriers, in order: the anchored subject, then `ref` on a
/// `retract:`-subject fact. Both require the `retract:` subject prefix, so the
/// whole predicate short-circuits on one string comparison for the ordinary
/// fact — which matters because [`crate::write_authority::needs_authority_check`]
/// calls this on every artifact append. The `retracts=` summary token is
/// deliberately NOT read; see the module header.
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
    None
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

    /// R2. The `retracts=` summary token is NOT a detection carrier. A fact
    /// whose ONLY retraction-shaped signal is that token withdraws nothing, so
    /// every gate reasoning about retraction has two spellings to cover rather
    /// than three. Emission is unchanged — this asserts the read side only.
    #[test]
    fn summary_token_alone_is_not_a_retraction() {
        let f = fact(
            "r3",
            "correction posted",
            Some("wrong port number [retracts=fact_a superseded_by=fact_b]"),
            None,
        );
        assert_eq!(target_of(&f), None);
        assert!(!is_retraction(&f));
        // The bracket block is still PARSED for `superseded_by` — it is read
        // off a real retraction, it just cannot mint one.
        assert_eq!(superseded_by_of(&f).as_deref(), Some("fact_b"));
    }

    #[test]
    fn superseded_by_token_requires_word_boundary() {
        let f = fact("a3", "x", Some("contrasuperseded_by=fact_b"), None);
        assert_eq!(superseded_by_of(&f), None);
        let f = fact("a4", "x", Some("(superseded_by=fact_b)"), None);
        assert_eq!(superseded_by_of(&f).as_deref(), Some("fact_b"));
    }

    /// What `rally retract` actually writes: the anchored subject carries the
    /// target, the summary carries `superseded_by`. Both halves of a real
    /// emitted record, read back.
    #[test]
    fn emitted_record_round_trips_subject_target_and_superseded_by() {
        let s = summary_for("fact_a", "wrong port", Some("fact_b"));
        let f = fact("r4", &subject_for("fact_a"), Some(&s), Some("fact_a"));
        assert_eq!(target_of(&f).as_deref(), Some("fact_a"));
        assert_eq!(superseded_by_of(&f).as_deref(), Some("fact_b"));
        let s = summary_for("fact_a", "  ", None);
        assert!(s.starts_with("retracted "));
    }

    /// The `retracts=` token stays on the wire even though nothing here reads
    /// it: build-loop's resolver parses `superseded_by` out of the same bracket
    /// block, and its `_clean_reason` strips exactly `[retracts=...]` off a
    /// surfaced reason. Dropping the emission would leave the wire marker in
    /// that peer's human-facing prose. Pinned so a future cleanup of the
    /// now-unread token has to read this first.
    #[test]
    fn summary_emission_keeps_the_cross_store_marker_block() {
        assert_eq!(
            summary_for("fact_a", "wrong port", Some("fact_b")),
            "wrong port [retracts=fact_a superseded_by=fact_b]"
        );
        assert_eq!(
            summary_for("fact_a", "wrong port", None),
            "wrong port [retracts=fact_a]"
        );
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
            fact("r2", "retract: the flaky one", None, Some("fact_b")),
            // R2: token-only, so it withdraws nothing.
            fact("f3", "x", Some("[retracts=fact_c]"), None),
        ];
        let ids = retracted_ids(&facts);
        assert!(ids.contains("fact_a") && ids.contains("fact_b"));
        assert!(!ids.contains("fact_c"));
        assert_eq!(ids.len(), 2);
    }
}
