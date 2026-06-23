// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for ADAPTIVE, MULTI-SIGNAL session liveness.
//!
//! Replaces the fixed `IDLE_THRESHOLD_SECS` / `TAKEOVER_STALE_SECS` cutoffs for
//! the squad/presence projection with a window that ADAPTS to each session's
//! planned heartbeat cadence, and decides liveness from FOUR independent
//! signals. A session is LIVE if ANY signal is fresh within its adaptive window.
//!
//! Two fail-directions, each defaulting to the SAFE side:
//! * **Squad projection** is FAIL-OPEN — a missing/unparseable signal yields
//!   [`Liveness::Unknown`], which the caller MUST treat as visible/alive. Hiding
//!   a still-alive peer would cause the exact write-collision this system
//!   prevents.
//! * **Reaper removal** is FAIL-CLOSED — [`Liveness::Unknown`] is NEVER reaped.
//!   Removal is destructive; we refuse on any signal we cannot trust.
//!
//! Time is INJECTED (callers pass signal ages + `now`) so the math is pure and
//! deterministically testable — the established rally-cli convention. The
//! constants are pinned here and MUST match the Python mirror
//! (`scripts/rally_point/liveness.py` in build-loop); parity is double-pinned by
//! the byte-identical golden fixture `liveness_vectors.json`.

/// Default planned heartbeat cadence (seconds) for a session that has NOT
/// declared one. 5 minutes — an undeclared session is assumed to beat often, so
/// it goes stale on the conservative-soonest schedule.
pub(crate) const DEFAULT_CADENCE_SECS: i64 = 300;

/// Number of missed beats before a session is stale. Six beats of the declared
/// cadence: a 5-min cadence → 30-min base window; a 5-hour cadence → 30-h base.
pub(crate) const MISS_MULTIPLIER: i64 = 6;

/// Extra grace (seconds) added on top of the missed-beats window to absorb one
/// beat of clock skew / scheduling jitter. 1 minute.
pub(crate) const GRACE_SECS: i64 = 60;

/// Liveness verdict for a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Liveness {
    /// At least one signal is fresh within the adaptive window.
    Live,
    /// Every PARSEABLE signal is stale (and at least one was parseable).
    Stale,
    /// No fresh signal AND at least one signal is absent/unparseable — we cannot
    /// prove the session is dead. Squad projection: treat as VISIBLE (fail-open).
    /// Reaper removal: treat as NOT-reapable (fail-closed).
    Unknown,
}

/// The four liveness signals, each an OPTIONAL age in seconds since it was last
/// fresh. `None` = the signal was never observed / could not be parsed (absent).
/// A `Some(age)` with `age <= window` is FRESH; `Some(age)` with `age > window`
/// is STALE for that one signal.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LivenessSignals {
    /// (a) Heartbeat / presence `last_seen` age.
    pub(crate) heartbeat_age: Option<i64>,
    /// (b) Most-recent direct inject/ack (Directive/Receipt) to/from the session.
    pub(crate) inject_age: Option<i64>,
    /// (c) Forward code progress — age since the session's worktree branch HEAD
    /// last moved.
    pub(crate) code_progress_age: Option<i64>,
    /// (d) Declared active work — age of the session's newest live claim /
    /// mission / handoff.
    pub(crate) plan_age: Option<i64>,
}

impl LivenessSignals {
    fn as_array(&self) -> [Option<i64>; 4] {
        [
            self.heartbeat_age,
            self.inject_age,
            self.code_progress_age,
            self.plan_age,
        ]
    }
}

/// The adaptive staleness window (seconds) for a session whose planned beat is
/// `planned_interval_secs`. A non-positive interval falls back to the default
/// cadence (never a zero/negative window).
///
/// `window = clamp(interval) * miss_multiplier + grace`.
pub(crate) fn adaptive_window_secs(
    planned_interval_secs: i64,
    default_cadence_secs: i64,
    miss_multiplier: i64,
    grace_secs: i64,
) -> i64 {
    let interval = if planned_interval_secs > 0 {
        planned_interval_secs
    } else {
        // Defend the default too: a misconfigured non-positive default falls
        // back to the pinned constant rather than producing a <=0 window.
        if default_cadence_secs > 0 {
            default_cadence_secs
        } else {
            DEFAULT_CADENCE_SECS
        }
    };
    let mult = miss_multiplier.max(1);
    let grace = grace_secs.max(0);
    interval * mult + grace
}

/// Decide liveness from the four signals against the adaptive `window`.
///
/// Rules (the EXACT contract the golden fixture asserts):
/// * any signal `Some(age)` with `age <= window` → [`Liveness::Live`].
/// * else if EVERY signal is `Some(_)` (all parseable) → [`Liveness::Stale`].
/// * else (no fresh signal AND at least one `None`) → [`Liveness::Unknown`].
pub(crate) fn is_live(signals: &LivenessSignals, window: i64) -> Liveness {
    let arr = signals.as_array();
    let any_fresh = arr
        .iter()
        .any(|s| matches!(s, Some(age) if *age <= window));
    if any_fresh {
        return Liveness::Live;
    }
    // No fresh signal. If every signal is present (parseable), it's provably
    // stale. If any signal is absent, we cannot prove death → Unknown.
    if arr.iter().all(Option::is_some) {
        Liveness::Stale
    } else {
        Liveness::Unknown
    }
}

/// Convenience: compute the window from a planned interval and decide liveness
/// in one call, using the pinned default constants. Callers that have resolved
/// tunables from config use [`adaptive_window_secs`] + [`is_live`] directly.
/// Used by the orphan-tmux reaper path (which has no `CoordinationConfig` in
/// hand) and by external integration tests.
#[allow(dead_code)]
pub(crate) fn is_live_default(
    signals: &LivenessSignals,
    planned_interval_secs: i64,
) -> Liveness {
    let window = adaptive_window_secs(
        planned_interval_secs,
        DEFAULT_CADENCE_SECS,
        MISS_MULTIPLIER,
        GRACE_SECS,
    );
    is_live(signals, window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn opt(v: &Value) -> Option<i64> {
        if v.is_null() {
            None
        } else {
            Some(v.as_i64().expect("signal age must be i64 or null"))
        }
    }

    /// Parity guard: the SHARED golden vectors (byte-identical file in build-loop
    /// at `scripts/rally_point/liveness_vectors.json`) must produce the same
    /// window math and the same `is_live` verdict here as in the Python mirror.
    #[test]
    fn matches_shared_golden_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/liveness_vectors.json"
        );
        let raw = std::fs::read_to_string(path).expect("read liveness vectors");
        let v: Value = serde_json::from_str(&raw).expect("parse liveness vectors");

        let default_cadence = v["default_cadence_secs"].as_i64().unwrap();
        let mult = v["miss_multiplier"].as_i64().unwrap();
        let grace = v["grace_secs"].as_i64().unwrap();
        assert_eq!(default_cadence, DEFAULT_CADENCE_SECS, "fixture cadence drift");
        assert_eq!(mult, MISS_MULTIPLIER, "fixture multiplier drift");
        assert_eq!(grace, GRACE_SECS, "fixture grace drift");

        for case in v["window_cases"].as_array().unwrap() {
            let interval = case["planned_interval_secs"].as_i64().unwrap();
            let expected = case["expected_window_secs"].as_i64().unwrap();
            let got = adaptive_window_secs(interval, default_cadence, mult, grace);
            assert_eq!(
                got, expected,
                "window case {}: interval={interval} got {got} expected {expected}",
                case["name"]
            );
        }

        for case in v["liveness_cases"].as_array().unwrap() {
            let interval = case["planned_interval_secs"].as_i64().unwrap();
            let sig = &case["signals"];
            let signals = LivenessSignals {
                heartbeat_age: opt(&sig["heartbeat_age"]),
                inject_age: opt(&sig["inject_age"]),
                code_progress_age: opt(&sig["code_progress_age"]),
                plan_age: opt(&sig["plan_age"]),
            };
            let window = adaptive_window_secs(interval, default_cadence, mult, grace);
            let got = is_live(&signals, window);
            let expected = match case["expected"].as_str().unwrap() {
                "live" => Liveness::Live,
                "stale" => Liveness::Stale,
                "unknown" => Liveness::Unknown,
                other => panic!("bad expected verdict {other}"),
            };
            assert_eq!(
                got, expected,
                "liveness case {}: got {got:?} expected {expected:?}",
                case["name"]
            );
        }
    }

    #[test]
    fn five_min_cadence_window_is_thirty_one_minutes() {
        // 6 missed 5-min beats + 1 min grace = 31 min.
        assert_eq!(
            adaptive_window_secs(300, DEFAULT_CADENCE_SECS, MISS_MULTIPLIER, GRACE_SECS),
            31 * 60
        );
    }

    #[test]
    fn five_hour_cadence_window_is_thirty_hours_plus_grace() {
        let w = adaptive_window_secs(18000, DEFAULT_CADENCE_SECS, MISS_MULTIPLIER, GRACE_SECS);
        assert_eq!(w, 18000 * 6 + 60);
        assert!(w > 30 * 3600, "5-hour cadence window must exceed 30h");
    }

    #[test]
    fn nonpositive_interval_falls_back_to_default() {
        let def = adaptive_window_secs(0, DEFAULT_CADENCE_SECS, MISS_MULTIPLIER, GRACE_SECS);
        assert_eq!(
            def,
            adaptive_window_secs(
                DEFAULT_CADENCE_SECS,
                DEFAULT_CADENCE_SECS,
                MISS_MULTIPLIER,
                GRACE_SECS
            )
        );
        // Negative interval AND negative default both clamp to the pinned const.
        let both_bad = adaptive_window_secs(-5, -5, MISS_MULTIPLIER, GRACE_SECS);
        assert_eq!(
            both_bad,
            DEFAULT_CADENCE_SECS * MISS_MULTIPLIER + GRACE_SECS
        );
    }

    #[test]
    fn multiplier_and_grace_are_clamped() {
        // multiplier < 1 clamps to 1; negative grace clamps to 0.
        assert_eq!(adaptive_window_secs(100, 300, 0, -10), 100);
        assert_eq!(adaptive_window_secs(100, 300, -3, 50), 150);
    }

    #[test]
    fn each_signal_independently_keeps_alive() {
        let window = 1860;
        for build in [
            |a| LivenessSignals {
                heartbeat_age: Some(a),
                ..Default::default()
            },
            |a| LivenessSignals {
                inject_age: Some(a),
                ..Default::default()
            },
            |a| LivenessSignals {
                code_progress_age: Some(a),
                ..Default::default()
            },
            |a| LivenessSignals {
                plan_age: Some(a),
                ..Default::default()
            },
        ] {
            assert_eq!(is_live(&build(10), window), Liveness::Live);
            // Same single signal, stale, with the other three absent → Unknown
            // (fail-open), NOT Stale — we never observed the other three.
            assert_eq!(is_live(&build(window + 1), window), Liveness::Unknown);
        }
    }

    #[test]
    fn all_present_and_stale_is_stale() {
        let window = 1860;
        let s = LivenessSignals {
            heartbeat_age: Some(window + 1),
            inject_age: Some(window + 1),
            code_progress_age: Some(window + 1),
            plan_age: Some(window + 1),
        };
        assert_eq!(is_live(&s, window), Liveness::Stale);
    }

    #[test]
    fn all_absent_is_unknown_failopen() {
        assert_eq!(
            is_live(&LivenessSignals::default(), 1860),
            Liveness::Unknown
        );
    }

    #[test]
    fn boundary_at_window_is_fresh() {
        // age == window is FRESH (<=); window+1 is stale for that signal.
        let window = 1860;
        let at = LivenessSignals {
            heartbeat_age: Some(window),
            ..Default::default()
        };
        assert_eq!(is_live(&at, window), Liveness::Live);
    }
}
