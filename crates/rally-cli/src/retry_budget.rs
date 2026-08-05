// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Retry budgets derived from the watchdog deadline, not from an attempt count.
//!
//! # The defect this module exists to make unrepresentable
//!
//! Every `rally` invocation runs under a wall-clock watchdog
//! ([`crate::DEFAULT_WATCHDOG_TIMEOUT_MS`], 3000ms). Four budgets governed a
//! single mutation and nothing coupled them:
//!
//! | budget | value | where (pinned at `4cc36ab`, pre-fix) |
//! |--------|-------|--------------------------------------|
//! | SQLite `busy_timeout` — blocks INSIDE one call | **5000ms** | `factstr-sqlite`'s `open_pool` |
//! | `open_fact_store` retry, `20ms * attempt` × 16 | 2720ms | `store.rs::open_fact_store` |
//! | append retry, `15ms * attempt` × 16 | 2040ms | `store.rs::RoomStore::append_fact` |
//! | the deadline all three had to fit inside | **3000ms** | `lib.rs::DEFAULT_WATCHDOG_TIMEOUT_MS` |
//!
//! The busy timeout is the one that fired. It blocks inside a single SQLite
//! call, so it swallowed the lock error for 5s while the watchdog killed the
//! process at 3s — and **the two retry loops never executed one iteration.**
//! They were not too slow, they were unreachable. Their own combined 4760ms was
//! a second violation waiting behind the first.
//!
//! Measured 2026-08-05, debug build, empty scratch room, no peers, no daemon,
//! hooks off, against a genuine `BEGIN EXCLUSIVE` holder: `rally say claim`
//! exited 4 at 3.040s. After this change the same command returns rc=1 at
//! 1.415s naming the exhausted budget. Uncontended is unchanged: 0.028s on both
//! sides, medians of 10 interleaved runs on a quiesced host.
//!
//! # Why a deadline rather than smaller constants
//!
//! Lowering the constants would fix that arithmetic and leave the class open:
//! they would still be independent, so the next edit to any one of them — or to
//! the watchdog — silently re-opens the gap. Nothing about `attempts < 16` tells
//! a reader it is coupled to a timeout defined in another file.
//!
//! # Why ONE function returns BOTH budgets
//!
//! [`budgets_for`] returns the blocking budget and the retry budget together
//! because they are not independent: a retry loop can only stop STARTING
//! attempts at its deadline, so an attempt begun one millisecond inside it
//! still blocks a full `busy_timeout` PAST it. Sizing the two separately
//! reintroduces the original defect one level down — which it did in the first
//! draft of this module, where a quarter-of-remaining blocking budget and a
//! half-of-remaining retry budget composed to exactly 100% of the watchdog with
//! zero headroom. They are derived in one place now, and
//! [`tests::composed_worst_case_stays_under_every_watchdog`] asserts the sum
//! rather than the parts.
//!
//! # The composed worst case
//!
//! One mutation opens the pool and then appends on it, so it absorbs the
//! blocking budget TWICE — the pool fixes `busy_timeout` at open and the append
//! blocks against that same value:
//!
//! ```text
//! open loop budget        R/3
//!   + one busy overshoot  R/8    (last attempt starts just inside the deadline)
//! append loop budget      (remainder)/3
//!   + one busy overshoot  R/8
//! ```
//!
//! At the 3000ms default: 1000 + 375 + 541 + 375 = 2291ms worst case, leaving
//! **709ms** for the durable append and its fsync — roughly twelve times the
//! ~60ms an uncontended write measures.

use std::time::{Duration, Instant};

/// Fraction of the remaining budget one retry loop may spend, as
/// `NUMER/DENOM`. Integer math so the invariant tests are exact rather than
/// float-approximate.
///
/// A third, not the half first drafted: the loop budget is only one of the four
/// terms in the composed worst case above, and a half left no headroom once the
/// two blocking overshoots were counted.
pub(crate) const RETRY_BUDGET_NUMER: u32 = 1;
/// Denominator for [`RETRY_BUDGET_NUMER`].
pub(crate) const RETRY_BUDGET_DENOM: u32 = 3;

/// Divisor for the per-call SQLite busy timeout, as a share of the remaining
/// budget. An eighth, because it is absorbed twice per mutation.
const BUSY_TIMEOUT_DIVISOR: u32 = 8;

/// Ceiling on a single retry sleep.
///
/// Without it a large remaining budget produces one long sleep that overshoots
/// the moment the contender actually releases: the lock frees at 40ms and the
/// retrier sleeps until 400ms. Capping the step keeps re-probe latency bounded
/// however much budget is available.
const MAX_RETRY_SLEEP: Duration = Duration::from_millis(50);

/// Floor on a single retry sleep, so a nearly-exhausted budget cannot spin.
const MIN_RETRY_SLEEP: Duration = Duration::from_millis(5);

/// How many full blocking waits an UNARMED retry loop must be able to survive.
///
/// With no watchdog there is no deadline to derive from, but a loop still has to
/// terminate. Sizing it as a multiple of the blocking budget is the only
/// framing that keeps the two coherent: a retry budget shorter than one
/// blocking call yields a loop that can never retry, which is what the first
/// draft did to `daemon serve` — one attempt where the pre-fix code had sixteen.
const UNARMED_RETRY_BLOCKS: u32 = 4;

/// The two coupled budgets for one command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Budgets {
    /// How long SQLite may block INSIDE a single call before returning
    /// `SQLITE_BUSY`. Fixed for the life of the pool at open, and therefore
    /// absorbed by the append as well.
    pub(crate) busy_timeout: Duration,
    /// How long ONE retry loop may keep starting new attempts.
    pub(crate) retry: Duration,
}

/// Budgets for a command with `remaining` left on its watchdog.
///
/// # `None` means no deadline exists, NOT "assume the default"
///
/// `rally daemon serve` deliberately runs with no watchdog — it blocks for the
/// daemon's entire lifetime by design, so `run_with_watchdog` routes it to
/// `run_inline` before a budget is ever sized, and the standalone `rallyd`
/// binary calls `serve` with no watchdog at all. Deriving a SHORT budget from a
/// watchdog that is not armed would invent a deadline the caller never asked
/// for and newly break the long-running case: the same defect as the one being
/// fixed, pointed the other way.
///
/// So the unarmed case keeps upstream's 5s blocking budget, byte-identical to
/// behaviour before this change, and gives the retry loop room for
/// [`UNARMED_RETRY_BLOCKS`] of them. A budget is only ever tightened against a
/// deadline that actually exists.
pub(crate) fn budgets_for(remaining: Option<Duration>) -> Budgets {
    match remaining {
        Some(remaining) => Budgets {
            // No positive floor: once less than 10ms remains, a 10ms minimum
            // can itself overrun the watchdog. Zero is a valid SQLite busy
            // timeout and means "return SQLITE_BUSY immediately".
            busy_timeout: (remaining / BUSY_TIMEOUT_DIVISOR)
                .min(factstr_sqlite::DEFAULT_BUSY_TIMEOUT),
            retry: spend_fraction(remaining),
        },
        None => {
            let busy = factstr_sqlite::DEFAULT_BUSY_TIMEOUT;
            Budgets {
                busy_timeout: busy,
                retry: busy.saturating_mul(UNARMED_RETRY_BLOCKS),
            }
        }
    }
}

/// A retry budget: a deadline plus the sleep schedule that respects it.
///
/// Construct once at loop entry with [`RetryBudget::new`], then call
/// [`RetryBudget::next_backoff`] per failed attempt. `None` means the budget is
/// spent and the caller must surface the underlying error rather than retry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryBudget {
    deadline: Instant,
    attempt: u32,
    jitter_ms: u64,
}

impl RetryBudget {
    /// Build a budget that stops starting attempts once `budget` elapses.
    ///
    /// `jitter_ms` de-synchronizes concurrent retriers; it perturbs each sleep
    /// but can never push the loop past the deadline, because the deadline —
    /// not the accumulated sleep — is what terminates the loop.
    pub(crate) fn new(budget: Duration, jitter_ms: u64) -> Self {
        Self {
            deadline: Instant::now() + budget,
            attempt: 0,
            jitter_ms,
        }
    }

    /// Sleep duration for the next attempt, or `None` when the budget is spent.
    ///
    /// The returned duration is clamped to what actually remains, so the caller
    /// cannot sleep past the deadline even on the final attempt.
    pub(crate) fn next_backoff(&mut self) -> Option<Duration> {
        let now = Instant::now();
        if now >= self.deadline {
            return None;
        }
        let remaining = self.deadline.saturating_duration_since(now);
        self.attempt = self.attempt.saturating_add(1);
        // Linear ramp, matching the pre-existing schedule's shape so behaviour
        // under light contention is unchanged; the deadline, not the ramp, is
        // what bounds the loop.
        let step = Duration::from_millis(
            u64::from(self.attempt)
                .saturating_mul(10)
                .saturating_add(self.jitter_ms),
        );
        Some(step.clamp(MIN_RETRY_SLEEP, MAX_RETRY_SLEEP).min(remaining))
    }

    /// Attempts made so far. Reported in errors so an operator can tell a
    /// budget that was spent retrying from one that was never contended.
    pub(crate) fn attempts(&self) -> u32 {
        self.attempt
    }

    /// True once the deadline has passed. Test-only: production code learns the
    /// same fact from `next_backoff` returning `None`.
    #[cfg(test)]
    pub(crate) fn is_exhausted(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

/// `remaining * NUMER / DENOM`, saturating.
///
/// Millisecond precision is deliberate: the budget is compared against a
/// millisecond watchdog, and nanosecond math would overflow `u128` conversions
/// for the absurd-but-representable `Duration::MAX`.
pub(crate) fn spend_fraction(remaining: Duration) -> Duration {
    let ms = remaining.as_millis();
    let spend = ms.saturating_mul(u128::from(RETRY_BUDGET_NUMER)) / u128::from(RETRY_BUDGET_DENOM);
    Duration::from_millis(u64::try_from(spend).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every watchdog setting rally can be configured with: the clamped floor,
    /// the default, the `daemon start` and `inject` ceilings, and the range
    /// between.
    fn every_watchdog_setting() -> Vec<u64> {
        let mut budgets: Vec<u64> = vec![
            100, 250, 400, 500, 1000, 3000, 5000, 30_000, 45_000, 60_000, 605_000,
        ];
        budgets.extend((1..=60).map(|n| n * 1000));
        budgets
    }

    /// Worst-case wall clock one mutation can spend before its append lands:
    /// both retry loops, plus the blocking overshoot each can absorb.
    fn composed_worst_case(watchdog: Duration) -> Duration {
        let b = budgets_for(Some(watchdog));
        let open = b.retry;
        // A loop stops STARTING attempts at its deadline; an attempt begun just
        // inside it still blocks a full busy_timeout past it.
        let after_open = watchdog.saturating_sub(open + b.busy_timeout);
        let append = spend_fraction(after_open);
        open + b.busy_timeout + append + b.busy_timeout
    }

    /// THE CLASS-CLOSING INVARIANT.
    ///
    /// Not "the current constants happen to fit" — that is the instance. This
    /// asserts that for EVERY watchdog rally can be configured with, the total
    /// a mutation can spend before its append stays under the deadline with
    /// real headroom left, so the budgets cannot drift apart the way they did.
    ///
    /// It asserts the SUM. An earlier version of this file checked the blocking
    /// terms and the retry terms as two separate inequalities, each of which
    /// passed while their sum reached exactly 100% of the watchdog — the same
    /// "each part is fine, the whole is not" shape as the original defect.
    #[test]
    fn composed_worst_case_stays_under_every_watchdog() {
        for watchdog_ms in every_watchdog_setting() {
            let watchdog = Duration::from_millis(watchdog_ms);
            let worst = composed_worst_case(watchdog);

            assert!(
                worst < watchdog,
                "composed worst case {worst:?} reaches watchdog {watchdog:?} \
                 — the 2026-08-05 defect was 4760ms of retry plus a 5000ms \
                 blocking budget inside 3000ms",
            );

            // Headroom is not slack: the durable append + fsync run AFTER the
            // last retry and must fit. An uncontended write measures ~60ms.
            let headroom = watchdog - worst;
            assert!(
                headroom >= watchdog / 8,
                "headroom {headroom:?} of {watchdog:?} is too thin for the \
                 append + fsync that follow the last retry",
            );
        }
    }

    /// The blocking budget is bounded in ABSOLUTE terms, not just as a
    /// fraction. Without a ceiling, `inject`'s 605s watchdog yields a 151s
    /// blocking call — one blind wait with no retry-loop iteration, which is
    /// the original defect's shape at a larger scale.
    #[test]
    fn the_blocking_budget_is_bounded_in_absolute_terms() {
        for watchdog_ms in every_watchdog_setting() {
            let b = budgets_for(Some(Duration::from_millis(watchdog_ms)));
            assert!(
                b.busy_timeout <= factstr_sqlite::DEFAULT_BUSY_TIMEOUT,
                "a {watchdog_ms}ms watchdog produced a {:?} blocking budget; a \
                 long deadline must buy more RETRIES, not one longer blind wait",
                b.busy_timeout,
            );
            assert!(b.busy_timeout <= Duration::from_millis(watchdog_ms));
        }
        // The two settings that motivated the ceiling.
        assert_eq!(
            budgets_for(Some(Duration::from_millis(605_000))).busy_timeout,
            Duration::from_secs(5),
        );
        assert_eq!(
            budgets_for(Some(Duration::from_millis(45_000))).busy_timeout,
            Duration::from_secs(5),
        );
    }

    /// The final milliseconds of a command are still part of the contract.
    /// A former 10ms floor could exceed the watchdog remainder and turn a
    /// clean budget error into a watchdog kill.
    #[test]
    fn sub_ten_millisecond_remainders_never_create_a_longer_block() {
        for remaining_ms in 0..10 {
            let remaining = Duration::from_millis(remaining_ms);
            let b = budgets_for(Some(remaining));
            assert!(
                b.busy_timeout <= remaining,
                "{remaining:?} remaining produced {:?} of blocking",
                b.busy_timeout,
            );
            assert_eq!(b.busy_timeout, remaining / BUSY_TIMEOUT_DIVISOR);
        }
    }

    /// A retry loop must be able to RETRY: its budget has to outlast at least
    /// one full blocking wait, or the loop gets exactly one attempt and the
    /// retry logic is decorative.
    ///
    /// This is the invariant the first draft violated on the unarmed path,
    /// where a 5s blocking budget met a 1500ms retry budget.
    #[test]
    fn a_retry_budget_always_outlasts_at_least_one_blocking_wait() {
        for watchdog_ms in every_watchdog_setting() {
            let b = budgets_for(Some(Duration::from_millis(watchdog_ms)));
            assert!(
                b.retry > b.busy_timeout,
                "{watchdog_ms}ms watchdog: retry budget {:?} does not outlast \
                 one {:?} blocking wait, so the loop can never retry",
                b.retry,
                b.busy_timeout,
            );
        }
        let unarmed = budgets_for(None);
        assert!(
            unarmed.retry > unarmed.busy_timeout,
            "unarmed: retry {:?} must outlast one {:?} blocking wait — \
             `daemon serve` otherwise gets one attempt where the pre-fix code \
             had sixteen",
            unarmed.retry,
            unarmed.busy_timeout,
        );
        assert_eq!(unarmed.retry, Duration::from_secs(20));
    }

    /// The unarmed case is a DIFFERENT question from the armed one, and
    /// answering it with a derived short deadline breaks `daemon serve`, which
    /// runs inline with no watchdog on purpose.
    #[test]
    fn an_unarmed_watchdog_does_not_invent_a_deadline() {
        let unarmed = budgets_for(None);
        assert_eq!(
            unarmed.busy_timeout,
            factstr_sqlite::DEFAULT_BUSY_TIMEOUT,
            "no watchdog armed must leave the blocking budget at upstream's \
             default — inventing a short one newly breaks `daemon serve`",
        );
        assert_eq!(unarmed.busy_timeout, Duration::from_secs(5));

        // Armed: derived, and an eighth of what is left.
        assert_eq!(
            budgets_for(Some(Duration::from_millis(3000))).busy_timeout,
            Duration::from_millis(375),
        );

        let mut budget = RetryBudget::new(unarmed.retry, 0);
        assert!(
            !budget.is_exhausted(),
            "an unarmed budget must permit retries"
        );
        for _ in 0..32 {
            if let Some(d) = budget.next_backoff() {
                assert!(d <= MAX_RETRY_SLEEP, "step {d:?} exceeds the cap");
            }
        }
    }

    /// The specific arithmetic that failed in the field, stated as a test.
    /// Fails if anyone restores a schedule that can outlast the default
    /// watchdog — including by restoring the old constants.
    #[test]
    fn default_watchdog_leaves_headroom_the_old_constants_did_not() {
        let watchdog = Duration::from_millis(crate::DEFAULT_WATCHDOG_TIMEOUT_MS);
        let b = budgets_for(Some(watchdog));

        // The regime that shipped before this module.
        let old_open: u64 = (1..=16).map(|a| 20 * a).sum();
        let old_append: u64 = (1..=16).map(|a| 15 * a).sum();
        assert_eq!(old_open, 2720, "documented measurement drifted");
        assert_eq!(old_append, 2040, "documented measurement drifted");
        assert!(
            Duration::from_millis(old_open + old_append) > watchdog,
            "the old schedule is what this module replaces; if it no longer \
             exceeds the watchdog the doc comment is stale",
        );
        // And the blocking budget alone used to exceed the whole watchdog.
        assert!(factstr_sqlite::DEFAULT_BUSY_TIMEOUT > watchdog);

        assert_eq!(b.busy_timeout, Duration::from_millis(375));
        assert_eq!(b.retry, Duration::from_millis(1000));
        assert_eq!(composed_worst_case(watchdog), Duration::from_millis(2291));
    }

    /// A budget hands out sleeps until the deadline, then stops. This is what
    /// makes the loop terminate on time rather than on an attempt count.
    #[test]
    fn budget_stops_handing_out_sleeps_once_spent() {
        let mut budget = RetryBudget::new(Duration::from_millis(50), 0);
        let mut slept = Duration::ZERO;
        let mut handed = 0;
        while let Some(d) = budget.next_backoff() {
            slept += d;
            handed += 1;
            std::thread::sleep(d);
            assert!(handed < 1000, "budget must terminate, not spin forever");
        }
        assert!(budget.is_exhausted());
        assert!(
            slept <= Duration::from_millis(50),
            "slept {slept:?} beyond the 50ms it was given",
        );
        assert!(handed > 0, "a live budget must allow at least one retry");
    }

    /// Boundary: a budget with nothing left never retries.
    #[test]
    fn exhausted_budget_refuses_the_first_attempt() {
        let mut budget = RetryBudget::new(Duration::ZERO, 0);
        assert!(budget.next_backoff().is_none());
        assert_eq!(budget.attempts(), 0);
    }

    /// No single sleep may overshoot the deadline, even when the ramp or the
    /// jitter would exceed what is left.
    #[test]
    fn a_single_sleep_never_overshoots_the_deadline() {
        let mut budget = RetryBudget::new(Duration::from_millis(12), 22);
        let mut total = Duration::ZERO;
        while let Some(d) = budget.next_backoff() {
            total += d;
            std::thread::sleep(d);
        }
        // A jitter of 22ms alone would blow a 12ms budget if the clamp to
        // `remaining` were missing.
        assert!(total <= Duration::from_millis(12), "total {total:?}");
    }

    /// Jitter perturbs the schedule without extending the budget — the property
    /// the old loop lacked, where `16 * jitter` added 352ms of unaccounted time
    /// on top of an already-oversized schedule.
    #[test]
    fn jitter_cannot_extend_the_budget() {
        for jitter in [0, 1, 11, 22, 1000] {
            let mut budget = RetryBudget::new(Duration::from_millis(50), jitter);
            let mut total = Duration::ZERO;
            while let Some(d) = budget.next_backoff() {
                total += d;
                std::thread::sleep(d);
            }
            assert!(
                total <= Duration::from_millis(50),
                "jitter {jitter} pushed total to {total:?}, past the 50ms budget",
            );
        }
    }

    #[test]
    fn spend_fraction_is_saturating_at_the_extremes() {
        assert_eq!(spend_fraction(Duration::ZERO), Duration::ZERO);
        assert_eq!(
            spend_fraction(Duration::from_millis(3000)),
            Duration::from_millis(1000)
        );
        // Must not panic or wrap.
        let _ = spend_fraction(Duration::MAX);
        let _ = budgets_for(Some(Duration::MAX));
    }
}
