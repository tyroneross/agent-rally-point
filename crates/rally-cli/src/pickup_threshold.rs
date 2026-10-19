// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The open-queue revalidation AGE threshold: a room's own pickup p95 shrunk
//! toward the cross-room prior.
//!
//! Narrow interface on purpose: [`blended_p95`] takes the room's measured p95
//! and sample size (computed from the ledger by `open_queue::room_pickup_p95`)
//! and returns a [`PickupThreshold`]. It reads no ledger, clock or filesystem,
//! so the policy can be swapped or moved to another crate without touching the
//! projection that feeds it.

/// The cross-room prior: open-queue time-to-close p95 over 518 closed items in
/// 37 rooms on 2026-10-18 (backlog item AGEN-RALLY-NEXT-m590zk409zjqpzxdevzb3,
/// `handoff_lifetimes.py`). Every room's threshold is shrunk toward it; a room
/// with no pickup history uses it outright. It is only a revalidation trigger,
/// so a wrong value costs one extra read, never a lost item.
pub(crate) const FALLBACK_PICKUP_P95_SECS: i64 = 208 * 3600;

/// Prior strength `k` in the room weight `w = n / (n + k)`: the room's own
/// p95 and the cross-room prior count equally at `n = k` picked-up items.
///
/// Fitted to the 2026-10-18 data as a credibility ratio in LOG space (pickup
/// times are heavy-tailed, so they blend multiplicatively):
/// `k = within-room sampling variance / between-room variance`.
/// * Buhlmann on per-room mean log time-to-close, 26 rooms / 518 items:
///   sigma^2 = 7.34, tau^2 = 2.00, so k = 3.7.
/// * Specific to the p95, six rooms with n >= 20: a bootstrap gives
///   n * Var(log p95) of median 7.6, and the between-room variance of log p95
///   net of sampling noise is 1.42, so k = 5.4.
///
/// 5 sits between the two, leaning to the p95 estimate because the threshold
/// is a tail quantile, which is noisier than a mean. At the old cutoff
/// (n = 20) the room carries 80% of the weight.
pub(crate) const PICKUP_PRIOR_STRENGTH: f64 = 5.0;

/// Floor for a room p95 before taking its log: a room whose items close
/// instantly would otherwise drive the blend toward zero.
const MIN_ROOM_P95_SECS: i64 = 60;

/// The revalidation age threshold actually applied, and how it was formed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PickupThreshold {
    pub(crate) secs: i64,
    /// `"cross_room"` when the room has no pickup history, else `"room_blend"`.
    pub(crate) source: &'static str,
    /// Weight on the room's own p95, `n / (n + k)`; 0 for `"cross_room"`.
    pub(crate) room_weight: f64,
    pub(crate) room_sample: usize,
}

/// Blend a room's p95 with the cross-room prior: a weighted geometric mean,
/// `exp(w * ln(room) + (1 - w) * ln(cross))` with `w = n / (n + k)`.
///
/// Continuous and monotone in `n` for a fixed room p95: equal to the prior at
/// `n = 0` and converging to the room's value as `n` grows, with no cutoff.
pub(crate) fn blended_p95(room_p95_secs: Option<i64>, sample: usize) -> PickupThreshold {
    match room_p95_secs {
        Some(room) if sample > 0 => {
            let n = sample as f64;
            let w = n / (n + PICKUP_PRIOR_STRENGTH);
            let room = (room.max(MIN_ROOM_P95_SECS)) as f64;
            let cross = FALLBACK_PICKUP_P95_SECS as f64;
            let secs = (w * room.ln() + (1.0 - w) * cross.ln()).exp().round() as i64;
            PickupThreshold {
                secs,
                source: "room_blend",
                room_weight: w,
                room_sample: sample,
            }
        }
        _ => PickupThreshold {
            secs: FALLBACK_PICKUP_P95_SECS,
            source: "cross_room",
            room_weight: 0.0,
            room_sample: sample,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    #[test]
    fn blend_equals_the_cross_room_prior_at_n_zero() {
        let t = blended_p95(None, 0);
        assert_eq!(
            (t.secs, t.source, t.room_weight),
            (FALLBACK_PICKUP_P95_SECS, "cross_room", 0.0)
        );
        // A room p95 with no sample behind it carries no weight either.
        assert_eq!(blended_p95(Some(HOUR), 0).secs, FALLBACK_PICKUP_P95_SECS);
    }

    #[test]
    fn blend_is_monotone_in_n_and_converges_to_the_room() {
        for room in [19 * HOUR, 650 * HOUR] {
            let mut last = FALLBACK_PICKUP_P95_SECS;
            for n in 1..=2_000 {
                let t = blended_p95(Some(room), n);
                assert_eq!(t.source, "room_blend");
                if room < FALLBACK_PICKUP_P95_SECS {
                    assert!(t.secs <= last, "n={n}: {} > {last}", t.secs);
                } else {
                    assert!(t.secs >= last, "n={n}: {} < {last}", t.secs);
                }
                last = t.secs;
            }
            let far = blended_p95(Some(room), 2_000).secs as f64;
            assert!(
                (far / room as f64 - 1.0).abs() < 0.02,
                "n=2000 is within 2% of the room"
            );
        }
    }

    #[test]
    fn blend_has_no_jump_at_the_old_cutoff() {
        let room = 19 * HOUR;
        let (a, b) = (blended_p95(Some(room), 19), blended_p95(Some(room), 20));
        assert!(b.secs <= a.secs);
        // The step 19 -> 20 is the same size as its neighbours: no cutoff.
        let step = |n| blended_p95(Some(room), n).secs - blended_p95(Some(room), n + 1).secs;
        assert!(
            (step(19) as f64) < 1.2 * step(18) as f64,
            "{} vs {}",
            step(19),
            step(18)
        );
        assert!(
            (a.secs - b.secs) as f64 / (a.secs as f64) < 0.05,
            "less than 5% per item"
        );
    }

    #[test]
    fn a_fast_room_lands_well_below_the_cross_room_value_at_n_twenty() {
        // atomize-ai on 2026-10-18: 20 picked-up items, p95 about 19 h.
        let t = blended_p95(Some(19 * HOUR), 20);
        assert!((t.room_weight - 0.8).abs() < 1e-9);
        assert!(t.secs < 40 * HOUR, "{}h", t.secs / HOUR);
        assert!(t.secs > 19 * HOUR, "still shrunk toward the prior");
    }
}
