// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for time-based coordination policy.
//!
//! Two policies share this module so the half-life constant and the
//! size→timeout mapping are defined exactly once:
//!
//! * **Recency decay** — every coordination message gets a continuously
//!   computed weight from its age. `weight(age) = 0.5 ^ (age_hours / half_life)`
//!   (exponential half-life). Listings deprioritize by weight; a message whose
//!   weight falls below the archive floor is moved to the archive store.
//! * **Lead / ownership auto-reclaim** — the reclaim timeout scales with the
//!   size of the claimed work (a single-file claim reclaims sooner than a
//!   multi-file / coarse claim).
//!
//! Time is INJECTED (callers pass `age_secs` / `now`) so the math is pure and
//! deterministically testable — the established rally-cli convention (there is
//! no `Clock` trait here; cockpitd's is intentionally not ported).
//!
//! The boundary operators are pinned here and MUST match the Python mirror
//! (`scripts/rally_point/decay.py` in build-loop):
//! * `is_archivable` uses STRICT `<` — a message exactly at the floor is kept.
//! * `age_secs` is integer seconds (floored) before the float power, so
//!   truncation behaves identically across languages.

use crate::resource_scope::{ResourceScope, ResourceType};

/// Default exponential half-life for recency decay (hours).
/// Reference weights at half-life 48h: 0h≈1.00, 12h≈0.84, 2d≈0.50, 4d≈0.25,
/// 7d≈0.09, 14d≈0.007.
pub(crate) const DEFAULT_HALF_LIFE_HOURS: f64 = 48.0;

/// Default archive floor — a message whose weight drops below this is archived.
/// 0.05 ≈ 14 days at the default half-life.
pub(crate) const DEFAULT_ARCHIVE_FLOOR: f64 = 0.05;

/// Default reclaim timeout for a SMALL (single-file) claim (minutes).
pub(crate) const DEFAULT_RECLAIM_SMALL_MINUTES: i64 = 30;

/// Default reclaim timeout for a LARGE (multi-file / coarse) claim (minutes).
/// Equal to the historical `TAKEOVER_STALE_SECS` (2h) so coarse claims keep
/// their existing grace window — no behavior change for that class.
pub(crate) const DEFAULT_RECLAIM_LARGE_MINUTES: i64 = 120;

/// The size class of a claim, used to scale the auto-reclaim timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkSize {
    /// A single-file claim — reclaims sooner (default 30m).
    Small,
    /// A multi-file claim, or any directory/repo/workspace/task-scoped claim —
    /// reclaims later (default 2h).
    Large,
}

/// Recency weight from an age, using an exponential half-life.
///
/// `weight = 0.5 ^ (age_hours / half_life_hours)` where `age_hours` is derived
/// from integer `age_secs` (floored seconds, matching the Python mirror). A
/// negative `age_secs` (clock skew) is clamped to 0 → weight 1.0. A non-positive
/// `half_life_secs` is treated as the default to avoid a divide-by-zero / NaN.
pub(crate) fn recency_weight(age_secs: i64, half_life_secs: i64) -> f64 {
    let age = age_secs.max(0) as f64;
    let half_life = if half_life_secs > 0 {
        half_life_secs as f64
    } else {
        DEFAULT_HALF_LIFE_HOURS * 3600.0
    };
    0.5_f64.powf(age / half_life)
}

/// True when a message's weight has fallen below the archive floor.
/// STRICT less-than: a message exactly at the floor is NOT archived. This
/// operator is pinned and must match the Python mirror.
pub(crate) fn is_archivable(weight: f64, floor: f64) -> bool {
    weight < floor
}

/// The reclaim timeout (seconds) for a claim of the given size.
pub(crate) fn reclaim_timeout_secs(size: WorkSize, small_minutes: i64, large_minutes: i64) -> i64 {
    match size {
        WorkSize::Small => small_minutes.max(0) * 60,
        WorkSize::Large => large_minutes.max(0) * 60,
    }
}

/// Classify a claim's work size from its resource scopes + raw scope count.
///
/// A claim is SMALL only when it is a single `file:` scope and nothing coarser.
/// Any directory/repo/workspace/task/cross-repo scope, or more than one scope,
/// classifies LARGE. This maps onto the only existing claim metadata
/// (`ResourceType` breadth + `raw_scope.len()`); see `claim_authority.rs`.
pub(crate) fn classify_work_size(
    resource_scopes: &[ResourceScope],
    raw_scope_len: usize,
) -> WorkSize {
    // More than one declared scope → coarse by breadth.
    if raw_scope_len > 1 {
        return WorkSize::Large;
    }
    // Any non-file resource type → coarse.
    let any_coarse = resource_scopes
        .iter()
        .any(|rs| !matches!(rs.resource_type, ResourceType::File));
    // A bare claim with no parsed resource scope (e.g. a task name that did not
    // parse to a file) is conservatively LARGE — we do not aggressively reclaim
    // something we cannot prove is a single file.
    if resource_scopes.is_empty() || any_coarse {
        return WorkSize::Large;
    }
    // Exactly one resource scope, and it is a File.
    if resource_scopes.len() == 1 {
        WorkSize::Small
    } else {
        WorkSize::Large
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_scope::AccessMode;

    const HL_SECS: i64 = (DEFAULT_HALF_LIFE_HOURS as i64) * 3600; // 48h

    /// Parity guard: the SHARED golden vectors (identical file in build-loop at
    /// `scripts/rally_point/decay_vectors.json`) must produce the same weights
    /// here as in the Python mirror. A divergence fails one of the two suites.
    #[test]
    fn weights_match_shared_golden_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/decay_vectors.json"
        );
        let raw = std::fs::read_to_string(path).expect("read golden vectors");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse golden vectors");
        let hl = v["half_life_secs"].as_i64().unwrap();
        for row in v["weights"].as_array().unwrap() {
            let age = row["age_secs"].as_i64().unwrap();
            let expected = row["expected"].as_f64().unwrap();
            let got = recency_weight(age, hl);
            assert!(
                (got - expected).abs() < 1e-4,
                "age {age}s: got {got}, expected {expected}"
            );
        }
        let floor = v["archive_floor"].as_f64().unwrap();
        for row in v["archive_floor_cases"].as_array().unwrap() {
            let weight = row["weight"].as_f64().unwrap();
            let want = row["archivable"].as_bool().unwrap();
            assert_eq!(is_archivable(weight, floor), want, "weight {weight}");
        }
    }

    fn scope(rt: ResourceType, id: &str) -> ResourceScope {
        ResourceScope {
            resource_type: rt,
            identifier: id.to_string(),
            access: AccessMode::Exclusive,
        }
    }

    // ---- recency_weight at the locked reference ages (half-life 48h) ----
    #[test]
    fn weight_at_reference_ages() {
        let cases = [
            (0_i64, 1.0_f64),
            (12 * 3600, 0.8409),
            (2 * 24 * 3600, 0.5000),
            (4 * 24 * 3600, 0.2500),
            (7 * 24 * 3600, 0.0884),
            (14 * 24 * 3600, 0.00781),
        ];
        for (age, expected) in cases {
            let w = recency_weight(age, HL_SECS);
            assert!(
                (w - expected).abs() < 1e-4,
                "age {age}s: got {w}, expected {expected}"
            );
        }
    }

    #[test]
    fn weight_monotonic_decreasing() {
        // A fresher message must always weigh more than an older one.
        let fresh = recency_weight(0, HL_SECS);
        let day3 = recency_weight(3 * 24 * 3600, HL_SECS);
        let day7 = recency_weight(7 * 24 * 3600, HL_SECS);
        assert!(fresh > day3 && day3 > day7);
    }

    #[test]
    fn weight_clamps_negative_age() {
        // Clock skew (negative age) must not produce >1 or NaN.
        assert_eq!(recency_weight(-100, HL_SECS), 1.0);
    }

    #[test]
    fn weight_handles_nonpositive_half_life() {
        // Falls back to default half-life rather than dividing by zero.
        let w = recency_weight(2 * 24 * 3600, 0);
        assert!((w - 0.5).abs() < 1e-4);
    }

    // ---- archive-floor boundary (STRICT <) ----
    #[test]
    fn archive_floor_boundary_strict() {
        let floor = DEFAULT_ARCHIVE_FLOOR;
        assert!(
            !is_archivable(floor + 0.0001, floor),
            "just above floor: keep"
        );
        assert!(
            !is_archivable(floor, floor),
            "exactly at floor: keep (strict <)"
        );
        assert!(
            is_archivable(floor - 0.0001, floor),
            "just below floor: archive"
        );
    }

    #[test]
    fn fourteen_days_is_archivable() {
        // The locked ~14d horizon must fall below the 0.05 floor.
        let w = recency_weight(14 * 24 * 3600, HL_SECS);
        assert!(is_archivable(w, DEFAULT_ARCHIVE_FLOOR));
    }

    // ---- reclaim timeout: small vs large, just-under / just-over ----
    #[test]
    fn reclaim_timeout_small_and_large() {
        let small = reclaim_timeout_secs(
            WorkSize::Small,
            DEFAULT_RECLAIM_SMALL_MINUTES,
            DEFAULT_RECLAIM_LARGE_MINUTES,
        );
        let large = reclaim_timeout_secs(
            WorkSize::Large,
            DEFAULT_RECLAIM_SMALL_MINUTES,
            DEFAULT_RECLAIM_LARGE_MINUTES,
        );
        assert_eq!(small, 30 * 60);
        assert_eq!(large, 120 * 60);
        // A single-file claim silent for 31m is reclaimable; 29m is not.
        assert!(31 * 60 > small);
        assert!(29 * 60 < small);
        // The same boundary semantics for large at 2h.
        assert!(121 * 60 > large);
        assert!(119 * 60 < large);
    }

    // ---- work-size classification ----
    #[test]
    fn classify_single_file_is_small() {
        let scopes = vec![scope(ResourceType::File, "src/a.rs")];
        assert_eq!(classify_work_size(&scopes, 1), WorkSize::Small);
    }

    #[test]
    fn classify_multi_file_is_large() {
        let scopes = vec![
            scope(ResourceType::File, "src/a.rs"),
            scope(ResourceType::File, "src/b.rs"),
        ];
        assert_eq!(classify_work_size(&scopes, 2), WorkSize::Large);
    }

    #[test]
    fn classify_dir_repo_workspace_task_are_large() {
        for rt in [
            ResourceType::Dir,
            ResourceType::Repo,
            ResourceType::Workspace,
            ResourceType::Task,
            ResourceType::CrossRepo,
        ] {
            let scopes = vec![scope(rt.clone(), "x")];
            assert_eq!(
                classify_work_size(&scopes, 1),
                WorkSize::Large,
                "{rt:?} must classify Large"
            );
        }
    }

    #[test]
    fn classify_empty_scope_is_large_conservative() {
        // Cannot prove single-file → do not aggressively reclaim.
        assert_eq!(classify_work_size(&[], 1), WorkSize::Large);
        assert_eq!(classify_work_size(&[], 0), WorkSize::Large);
    }
}
