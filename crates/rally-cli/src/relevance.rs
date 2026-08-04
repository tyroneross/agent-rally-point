// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for ROOM COMPOSITION relevance.
//!
//! The room is composed by filling a byte budget with the most relevant items,
//! not by cutting each bucket at a fixed count. Relevance is COMPOSED from
//! signals Rally already computes, so this module adds a ranking, not a second
//! policy:
//!
//! * **recency** — [`crate::decay::recency_weight`], the same exponential
//!   half-life that drives archive-floor partitioning.
//! * **author liveness** — [`crate::liveness::is_live`], the same adaptive
//!   four-signal window that drives squad visibility.
//! * **addressed-to-me** — the caller is the item's `target`, is named in a
//!   `to:<tool>` evidence stamp, or appears in the item's scope.
//! * **path overlap** — the fraction of the caller's declared working set that
//!   the item's scope touches.
//!
//! # The fail-open invariant
//!
//! The model is MULTIPLICATIVE with recency as the spine, and **every factor is
//! 1.0 when its signal is absent**. An item whose consumer-relative signals
//! cannot be computed therefore scores exactly its recency weight — it is never
//! demoted for a signal nobody could measure. Only a *provably* stale author
//! demotes an item, mirroring the squad rule where only a provably-`Stale`
//! squad is dropped and `Unknown` stays visible.
//!
//! [`relevance`] enforces this with a floor: the returned score is never below
//! the recency weight it was given. Boosts may raise an item; absent signals
//! may not lower it.
//!
//! Time is INJECTED (callers pass ages) so the math is pure and deterministically
//! testable — the established rally-cli convention shared with `decay` and
//! `liveness`. Tunables live in `hooks_config` under `coordination.relevance`
//! and resolve default → user → repo → env like every other coordination knob.

use crate::liveness::Liveness;

/// Multiplier applied to an item whose AUTHOR is provably stale (all four
/// liveness signals present and past the adaptive window). `Live` and `Unknown`
/// authors are never demoted. 0.5 halves the item's effective age-weight, which
/// at the default 48 h half-life is worth exactly one half-life of extra age.
pub(crate) const DEFAULT_STALE_AUTHOR_FACTOR: f64 = 0.5;

/// Boost applied when an item is addressed to the caller. 1.0 doubles the score
/// (`1.0 + boost`), so an item addressed to you outranks an equally-fresh item
/// addressed to somebody else.
pub(crate) const DEFAULT_ADDRESSED_BOOST: f64 = 1.0;

/// Boost applied at FULL overlap between the item's scope and the caller's
/// declared working set, scaled linearly by the overlap fraction. 1.0 doubles
/// the score for a total match.
pub(crate) const DEFAULT_PATH_OVERLAP_BOOST: f64 = 1.0;

/// Fraction of a consumer's context that the room may occupy. Multiplied by
/// [`DEFAULT_CONSUMER_CONTEXT_BYTES`] to yield the byte ceiling. Expressed as a
/// fraction so the bound scales with the consumer rather than pinning a count.
pub(crate) const DEFAULT_ROOM_BUDGET_FRACTION: f64 = 0.05;

/// Assumed consumer context size in bytes when the caller declares none.
/// 4 MB ≈ 1 M tokens — the frontier-model context this room is read into.
/// With the default fraction this yields a 200 KB ceiling.
pub(crate) const DEFAULT_CONSUMER_CONTEXT_BYTES: i64 = 4_000_000;

/// Relevance tunables, resolved from `coordination.relevance` in the same
/// config chain as every other coordination knob.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RelevanceWeights {
    /// Multiplier for a provably-stale author. Must be in `(0.0, 1.0]`.
    pub(crate) stale_author_factor: f64,
    /// Additive boost when the item is addressed to the caller. `>= 0.0`.
    pub(crate) addressed_boost: f64,
    /// Additive boost at full path overlap. `>= 0.0`.
    pub(crate) path_overlap_boost: f64,
}

impl Default for RelevanceWeights {
    fn default() -> Self {
        Self {
            stale_author_factor: DEFAULT_STALE_AUTHOR_FACTOR,
            addressed_boost: DEFAULT_ADDRESSED_BOOST,
            path_overlap_boost: DEFAULT_PATH_OVERLAP_BOOST,
        }
    }
}

/// What the room knows about the caller. Every field is optional; a caller that
/// declares nothing gets a purely recency-and-liveness ranking, which is the
/// neutral case, not a degraded one.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConsumerContext {
    /// The caller's tool id (`rally room --tool <id>`).
    pub(crate) tool: Option<String>,
    /// The caller's declared working set (`rally room --path <p>`), normalised
    /// to bare paths with any `file:` / `dir:` prefix stripped.
    pub(crate) paths: Vec<String>,
}

impl ConsumerContext {
    /// A caller that declared nothing. All consumer-relative factors evaluate
    /// to the neutral 1.0, so the ranking falls back to recency and author
    /// staleness alone.
    pub(crate) fn neutral() -> Self {
        Self::default()
    }
}

/// The consumer-relative and author-relative signals for ONE item.
///
/// Every field encodes "absent" explicitly so [`relevance`] can honour the
/// fail-open invariant rather than inferring absence from a zero.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RelevanceSignals {
    /// The item author's liveness verdict. `None` = the author could not be
    /// resolved (no `tool`, or no squad entry) → neutral.
    pub(crate) author_liveness: Option<Liveness>,
    /// True when the item names the caller as recipient. False also covers
    /// "no caller declared" — both are neutral, neither demotes.
    pub(crate) addressed_to_caller: bool,
    /// Fraction in `[0, 1]` of the caller's declared paths that this item's
    /// scope touches. `None` = the caller declared no paths → neutral.
    pub(crate) path_overlap: Option<f64>,
}

/// Compose an item's relevance from its recency weight and its signals.
///
/// `recency` is [`crate::decay::recency_weight`] for the item — already in
/// `(0, 1]`, and already 1.0 for an unparseable timestamp (decay must never
/// invent staleness from a bad stamp).
///
/// Guarantees, each asserted by a test:
/// 1. **Fail-open floor** — the result is never below `recency`. An absent or
///    unreadable signal cannot push an item down the ranking.
/// 2. **Only provable staleness demotes** — `Liveness::Unknown` and
///    `Liveness::Live` are neutral; only `Liveness::Stale` applies
///    `stale_author_factor`, and even then the floor in (1) does not apply to
///    it, because a provably-stale author IS a measured signal.
/// 3. **Monotone in recency** — with identical signals, a fresher item always
///    outranks an older one.
///
/// Guarantee (1) is deliberately scoped to *absent* signals. The one factor
/// that may lower a score is the one backed by a positive measurement.
pub(crate) fn relevance(recency: f64, signals: &RelevanceSignals, w: &RelevanceWeights) -> f64 {
    let base = if recency.is_finite() && recency > 0.0 {
        recency
    } else {
        // A non-finite or non-positive weight is a broken measurement, not a
        // verdict of irrelevance. Treat it as fresh.
        1.0
    };

    // Author liveness: the ONLY factor that may reduce a score, and only on a
    // provable Stale verdict. Absent / Unknown / Live are all neutral.
    let liveness_factor = match signals.author_liveness {
        Some(Liveness::Stale) => clamp_unit(w.stale_author_factor, DEFAULT_STALE_AUTHOR_FACTOR),
        _ => 1.0,
    };

    // Consumer-relative boosts: additive on top of 1.0, never below it.
    let addressed_factor = if signals.addressed_to_caller {
        1.0 + w.addressed_boost.max(0.0)
    } else {
        1.0
    };
    let overlap_factor = match signals.path_overlap {
        Some(frac) if frac.is_finite() && frac > 0.0 => {
            1.0 + w.path_overlap_boost.max(0.0) * frac.clamp(0.0, 1.0)
        }
        _ => 1.0,
    };

    base * liveness_factor * addressed_factor * overlap_factor
}

/// Clamp a configured stale-author factor into `(0.0, 1.0]`, falling back to
/// `fallback` when the value is unusable. A factor above 1.0 would let a
/// provably-dead author's items OUTRANK a live author's — the config surface
/// must not be able to invert the signal.
fn clamp_unit(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 && value <= 1.0 {
        value
    } else {
        fallback
    }
}

/// The fraction of `caller_paths` that at least one entry of `item_scope`
/// touches. Returns `None` when the caller declared no paths (neutral).
///
/// Matching is prefix-based in both directions so a `dir:` claim covers a file
/// beneath it and a file path matches a directory the caller declared. Scope
/// entries carry a `file:` / `dir:` / `repo:` style prefix which is stripped
/// before comparison; entries without a prefix compare as-is.
pub(crate) fn path_overlap(caller_paths: &[String], item_scope: &[String]) -> Option<f64> {
    if caller_paths.is_empty() {
        return None;
    }
    if item_scope.is_empty() {
        return Some(0.0);
    }
    let scopes: Vec<&str> = item_scope.iter().map(|s| strip_scope_prefix(s)).collect();
    let matched = caller_paths
        .iter()
        .filter(|p| {
            let p = strip_scope_prefix(p);
            !p.is_empty() && scopes.iter().any(|s| paths_touch(p, s))
        })
        .count();
    Some(matched as f64 / caller_paths.len() as f64)
}

/// Strip a `<type>:` scope prefix (`file:`, `dir:`, `repo:`, `task:`, …) so a
/// raw path and a scoped claim compare on the same footing. A value with no
/// recognised prefix is returned unchanged, so a path containing a colon (rare
/// but legal) is not mangled: only a prefix made of ASCII lowercase letters
/// followed by `:` is treated as a type tag.
fn strip_scope_prefix(value: &str) -> &str {
    match value.split_once(':') {
        Some((tag, rest))
            if !tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            rest
        }
        _ => value,
    }
}

/// True when two paths refer to the same file, or one contains the other.
/// Comparison is on `/`-delimited segment boundaries so `src/ab` does not match
/// `src/a`.
fn paths_touch(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let contains = |outer: &str, inner: &str| {
        inner.len() > outer.len()
            && inner.starts_with(outer)
            && inner.as_bytes().get(outer.len()) == Some(&b'/')
    };
    contains(a, b) || contains(b, a)
}

/// The byte ceiling for a room response.
///
/// `fraction * context_bytes`, rounded down. A non-positive fraction or context
/// size DISABLES the ceiling (returns `None`) — the bound is opt-outable, so it
/// can never become a blind cut nobody chose.
pub(crate) fn budget_bytes(fraction: f64, context_bytes: i64) -> Option<usize> {
    if !fraction.is_finite() || fraction <= 0.0 || context_bytes <= 0 {
        return None;
    }
    let raw = fraction * context_bytes as f64;
    if !raw.is_finite() || raw < 1.0 {
        return None;
    }
    Some(raw as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w() -> RelevanceWeights {
        RelevanceWeights::default()
    }

    // ---- the fail-open invariant ----

    #[test]
    fn absent_signals_never_demote() {
        // Every consumer-relative signal absent → score is exactly the recency
        // weight. This is the invariant the whole budget-fill rests on: an item
        // whose relevance cannot be computed is ranked on age alone, not sunk.
        let bare = RelevanceSignals::default();
        for recency in [1.0, 0.5, 0.05, 0.0001] {
            let score = relevance(recency, &bare, &w());
            assert!(
                (score - recency).abs() < 1e-12,
                "recency {recency} scored {score}, expected exactly the recency"
            );
        }
    }

    #[test]
    fn unknown_author_liveness_is_neutral_not_stale() {
        // Mirrors the squad rule: Unknown means "cannot prove dead", and cannot
        // prove dead must never cost an item its place.
        let unknown = RelevanceSignals {
            author_liveness: Some(Liveness::Unknown),
            ..Default::default()
        };
        let live = RelevanceSignals {
            author_liveness: Some(Liveness::Live),
            ..Default::default()
        };
        let absent = RelevanceSignals::default();
        assert_eq!(relevance(0.4, &unknown, &w()), 0.4);
        assert_eq!(relevance(0.4, &live, &w()), 0.4);
        assert_eq!(relevance(0.4, &absent, &w()), 0.4);
    }

    #[test]
    fn only_provable_stale_demotes() {
        let stale = RelevanceSignals {
            author_liveness: Some(Liveness::Stale),
            ..Default::default()
        };
        assert!(relevance(0.4, &stale, &w()) < 0.4);
        assert!((relevance(0.4, &stale, &w()) - 0.2).abs() < 1e-12);
    }

    #[test]
    fn score_never_below_recency_for_absent_signals() {
        // Exhaustive over the absent/neutral combinations.
        for liveness in [None, Some(Liveness::Live), Some(Liveness::Unknown)] {
            for overlap in [None, Some(0.0)] {
                let s = RelevanceSignals {
                    author_liveness: liveness,
                    addressed_to_caller: false,
                    path_overlap: overlap,
                };
                assert!(
                    relevance(0.3, &s, &w()) >= 0.3,
                    "liveness={liveness:?} overlap={overlap:?} demoted an item on absent signals"
                );
            }
        }
    }

    // ---- boosts ----

    #[test]
    fn addressed_to_caller_outranks_equally_fresh_peer_item() {
        let mine = RelevanceSignals {
            addressed_to_caller: true,
            ..Default::default()
        };
        let theirs = RelevanceSignals::default();
        assert!(relevance(0.5, &mine, &w()) > relevance(0.5, &theirs, &w()));
    }

    #[test]
    fn addressed_beats_a_full_half_life_of_freshness() {
        // A message addressed to me from two days ago must outrank an unrelated
        // message from now — otherwise the boost cannot do its job.
        let mine = RelevanceSignals {
            addressed_to_caller: true,
            ..Default::default()
        };
        let fresh_other = RelevanceSignals::default();
        assert!(relevance(0.5, &mine, &w()) > relevance(0.99, &fresh_other, &w()));
    }

    #[test]
    fn path_overlap_scales_linearly() {
        let half = RelevanceSignals {
            path_overlap: Some(0.5),
            ..Default::default()
        };
        let full = RelevanceSignals {
            path_overlap: Some(1.0),
            ..Default::default()
        };
        assert!((relevance(1.0, &half, &w()) - 1.5).abs() < 1e-12);
        assert!((relevance(1.0, &full, &w()) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn monotone_in_recency_with_identical_signals() {
        let s = RelevanceSignals {
            addressed_to_caller: true,
            path_overlap: Some(0.7),
            author_liveness: Some(Liveness::Stale),
        };
        assert!(relevance(0.9, &s, &w()) > relevance(0.4, &s, &w()));
    }

    // ---- config hardening ----

    #[test]
    fn stale_factor_above_one_cannot_invert_the_signal() {
        // A misconfigured factor must not let a provably-dead author's items
        // outrank a live author's.
        let bad = RelevanceWeights {
            stale_author_factor: 4.0,
            ..RelevanceWeights::default()
        };
        let stale = RelevanceSignals {
            author_liveness: Some(Liveness::Stale),
            ..Default::default()
        };
        let live = RelevanceSignals {
            author_liveness: Some(Liveness::Live),
            ..Default::default()
        };
        assert!(relevance(0.5, &stale, &bad) <= relevance(0.5, &live, &bad));
    }

    #[test]
    fn negative_boosts_are_clamped_to_neutral() {
        let bad = RelevanceWeights {
            stale_author_factor: DEFAULT_STALE_AUTHOR_FACTOR,
            addressed_boost: -5.0,
            path_overlap_boost: -5.0,
        };
        let s = RelevanceSignals {
            addressed_to_caller: true,
            path_overlap: Some(1.0),
            author_liveness: None,
        };
        assert!(
            relevance(0.5, &s, &bad) >= 0.5,
            "a negative boost must clamp to neutral, never demote"
        );
    }

    #[test]
    fn broken_recency_is_treated_as_fresh() {
        let s = RelevanceSignals::default();
        assert_eq!(relevance(f64::NAN, &s, &w()), 1.0);
        assert_eq!(relevance(0.0, &s, &w()), 1.0);
        assert_eq!(relevance(-1.0, &s, &w()), 1.0);
    }

    // ---- path overlap ----

    #[test]
    fn overlap_is_none_when_caller_declares_no_paths() {
        assert!(path_overlap(&[], &["file:src/a.rs".into()]).is_none());
    }

    #[test]
    fn overlap_strips_scope_prefixes_on_both_sides() {
        let caller = vec!["src/store.rs".to_string()];
        let scope = vec!["file:src/store.rs".to_string()];
        assert_eq!(path_overlap(&caller, &scope), Some(1.0));
    }

    #[test]
    fn overlap_matches_a_directory_claim_covering_the_file() {
        let caller = vec!["src/nested/a.rs".to_string()];
        let scope = vec!["dir:src/nested".to_string()];
        assert_eq!(path_overlap(&caller, &scope), Some(1.0));
    }

    #[test]
    fn overlap_does_not_match_a_sibling_prefix() {
        // `src/a` must not match `src/ab` — segment-boundary comparison.
        let caller = vec!["src/ab.rs".to_string()];
        let scope = vec!["file:src/a.rs".to_string()];
        assert_eq!(path_overlap(&caller, &scope), Some(0.0));
    }

    #[test]
    fn overlap_is_a_fraction_of_the_callers_working_set() {
        let caller = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
            "src/d.rs".to_string(),
        ];
        let scope = vec!["file:src/a.rs".to_string(), "file:src/c.rs".to_string()];
        assert_eq!(path_overlap(&caller, &scope), Some(0.5));
    }

    #[test]
    fn overlap_is_zero_not_none_for_a_scopeless_item() {
        // The caller declared paths, so the signal IS measurable; the answer is
        // "no overlap", which is neutral (factor 1.0), not a demotion.
        let caller = vec!["src/a.rs".to_string()];
        assert_eq!(path_overlap(&caller, &[]), Some(0.0));
        let s = RelevanceSignals {
            path_overlap: Some(0.0),
            ..Default::default()
        };
        assert_eq!(relevance(0.6, &s, &w()), 0.6);
    }

    // ---- budget ----

    #[test]
    fn budget_is_a_fraction_of_the_consumer_context() {
        assert_eq!(budget_bytes(0.05, 4_000_000), Some(200_000));
        assert_eq!(budget_bytes(0.01, 1_000_000), Some(10_000));
    }

    #[test]
    fn zero_fraction_disables_the_ceiling() {
        assert_eq!(budget_bytes(0.0, 4_000_000), None);
        assert_eq!(budget_bytes(0.05, 0), None);
        assert_eq!(budget_bytes(-1.0, 4_000_000), None);
        assert_eq!(budget_bytes(f64::NAN, 4_000_000), None);
    }

    #[test]
    fn default_budget_is_two_hundred_kilobytes() {
        assert_eq!(
            budget_bytes(DEFAULT_ROOM_BUDGET_FRACTION, DEFAULT_CONSUMER_CONTEXT_BYTES),
            Some(200_000)
        );
    }

    #[test]
    fn neutral_context_declares_nothing() {
        let c = ConsumerContext::neutral();
        assert!(c.tool.is_none() && c.paths.is_empty());
    }
}
