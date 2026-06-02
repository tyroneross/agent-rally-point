// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Injectable Clock trait.
//!
//! Production code uses `SystemClock`; tests use `FakeClock` to control time
//! without wall-clock dependency.

use chrono::{DateTime, Duration, Utc};
use std::sync::{Arc, Mutex};

/// Abstraction over the current time, allowing tests to control the clock.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

// ── SystemClock ───────────────────────────────────────────────────────────────

/// Production clock: delegates to `Utc::now()`.
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

// ── FakeClock ─────────────────────────────────────────────────────────────────

/// Test clock: holds a fixed instant that can be set and advanced.
///
/// Wrap in `Arc` to share across threads / tasks.
#[derive(Debug, Clone)]
pub struct FakeClock {
    inner: Arc<Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    /// Create a FakeClock frozen at the given instant.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(at)),
        }
    }

    /// Create a FakeClock frozen at the Unix epoch (convenient for tests).
    pub fn at_epoch() -> Self {
        Self::new(DateTime::from_timestamp(0, 0).expect("epoch is valid"))
    }

    /// Advance the clock by the given duration.
    pub fn advance(&self, d: Duration) {
        let mut t = self.inner.lock().unwrap();
        *t = *t + d;
    }

    /// Set the clock to an absolute instant.
    pub fn set(&self, at: DateTime<Utc>) {
        let mut t = self.inner.lock().unwrap();
        *t = at;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.inner.lock().unwrap()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn system_clock_returns_nonnull() {
        let c = SystemClock;
        let t = c.now();
        // Sanity: year is after 2020
        assert!(t.timestamp() > 1_577_836_800);
    }

    #[test]
    fn fake_clock_is_frozen_at_creation() {
        let epoch = DateTime::from_timestamp(0, 0).unwrap();
        let c = FakeClock::new(epoch);
        assert_eq!(c.now(), epoch);
        assert_eq!(c.now(), epoch); // stable
    }

    #[test]
    fn fake_clock_advance_works() {
        let c = FakeClock::at_epoch();
        c.advance(Duration::seconds(60));
        assert_eq!(c.now().timestamp(), 60);
        c.advance(Duration::seconds(60));
        assert_eq!(c.now().timestamp(), 120);
    }

    #[test]
    fn fake_clock_set_works() {
        let c = FakeClock::at_epoch();
        let target = DateTime::from_timestamp(9999, 0).unwrap();
        c.set(target);
        assert_eq!(c.now(), target);
    }

    #[test]
    fn fake_clock_clone_shares_state() {
        let c1 = FakeClock::at_epoch();
        let c2 = c1.clone();
        c1.advance(Duration::seconds(100));
        // Both should see the same time because they share the Arc<Mutex<_>>
        assert_eq!(c1.now(), c2.now());
    }
}
