// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The open-queue revalidation AGE threshold: per repo, from that repo's own
//! pickup history only.
//!
//! A room is one repo. Operations Center work and release batches are per
//! repo, so no other repo's history may move this repo's threshold (owner
//! decision, 2026-10-19). There is no cross-room prior and no global fallback:
//! a repo without enough history simply has no age signal, and the other
//! revalidation signals (scope changed, later done fact, linked plan done)
//! still apply.
//!
//! Narrow interface on purpose: [`room_threshold`] takes the room's measured
//! nearest-rank p95 and sample size (computed from the ledger by
//! `open_queue::room_pickup_p95`) and returns a [`PickupThreshold`]. It reads no
//! ledger, clock or filesystem, so the policy can move to another crate without
//! touching the projection that feeds it.
//!
//! Reproduce the numbers below with `scripts/research/room_pickup_threshold.py`; the
//! output is in `docs/research/2026-10-19-room-pickup-threshold.md`.

/// Fewest picked-up open-queue items before a repo's age signal turns on.
///
/// Below 20 items the nearest-rank p95 is the slowest pickup seen. A bootstrap
/// over the repos with >= 20 pickups (2026-10-19 data) measured how that
/// estimate compares with the repo's full-history p95:
/// * n = 5: median 0.69x the true p95, and below it 68% of the time. The label
///   would fire on items that are still inside normal pickup time.
/// * n = 8: median 0.98x, below it 54% of the time. Roughly unbiased.
/// * n = 10 and n = 14: median 1.00x, but fewer repos qualify (11 and 7 of 22,
///   against 13 at n = 8).
///
/// 8 is the smallest n where the estimate is centred on the true p95. In the
/// measured data the same 13 repos qualify at 5 and at 8, so it costs no
/// coverage.
pub(crate) const MIN_ROOM_SAMPLE: usize = 8;

/// Floor for the threshold: 1 h.
///
/// Without a floor, a repo whose pickups are same-second acks would get a
/// threshold of minutes or zero (`room p95 = 0`), and almost every open-queue
/// item there would be labelled "revalidate", so the label would stop carrying
/// meaning. The fastest measured repo with >= 8 pickups has a p95 of 19 h, so
/// a 1 h floor sits more than 10x below every real repo and never binds on
/// one. It only stops degenerate histories.
pub(crate) const MIN_THRESHOLD_SECS: i64 = 3600;

/// The revalidation age threshold for one repo, and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PickupThreshold {
    /// `None` means the age signal is OFF for this repo.
    pub(crate) secs: Option<i64>,
    /// `"room"`, or `"insufficient_history"` below [`MIN_ROOM_SAMPLE`].
    pub(crate) source: &'static str,
    pub(crate) room_sample: usize,
}

/// The repo's own p95, floored, once it has [`MIN_ROOM_SAMPLE`] pickups.
/// Otherwise no age threshold.
pub(crate) fn room_threshold(room_p95_secs: Option<i64>, sample: usize) -> PickupThreshold {
    match room_p95_secs {
        Some(room) if sample >= MIN_ROOM_SAMPLE => PickupThreshold {
            secs: Some(room.max(MIN_THRESHOLD_SECS)),
            source: "room",
            room_sample: sample,
        },
        _ => PickupThreshold {
            secs: None,
            source: "insufficient_history",
            room_sample: sample,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    /// N = 8 is a measured choice (docs/research/2026-10-19-room-pickup-threshold.md):
    /// at n = 5 the nearest-rank p95 reads 0.69x the true value. Change it only with new data.
    #[test]
    fn minimum_room_sample_is_the_measured_eight() {
        assert_eq!(MIN_ROOM_SAMPLE, 8);
    }

    #[test]
    fn below_the_minimum_sample_there_is_no_age_signal() {
        for n in 0..MIN_ROOM_SAMPLE {
            let t = room_threshold(Some(5 * HOUR), n);
            assert_eq!((t.secs, t.source), (None, "insufficient_history"), "n={n}");
        }
        assert_eq!(room_threshold(None, 50).secs, None);
    }

    /// At or above the minimum the threshold is EXACTLY the room's own p95, so
    /// any cross-room or prior term reintroduced here fails this test.
    #[test]
    fn at_the_minimum_sample_the_threshold_is_the_rooms_own_p95() {
        for (room, n) in [
            (19 * HOUR, MIN_ROOM_SAMPLE),
            (650 * HOUR, 35),
            (2 * HOUR, 1_000),
        ] {
            let t = room_threshold(Some(room), n);
            assert_eq!((t.secs, t.source, t.room_sample), (Some(room), "room", n));
        }
    }

    #[test]
    fn the_floor_holds_at_extreme_inputs() {
        for room in [0, 1, 60, 3599] {
            for n in [MIN_ROOM_SAMPLE, 100, 10_000] {
                assert_eq!(room_threshold(Some(room), n).secs, Some(MIN_THRESHOLD_SECS));
            }
        }
        assert_eq!(room_threshold(Some(3601), 20).secs, Some(3601));
    }
}
