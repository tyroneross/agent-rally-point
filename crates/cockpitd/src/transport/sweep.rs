// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! TTL auto-deny background sweep (H2).
//!
//! ## What it does
//! Periodically scans the supervisor's store for pending approvals whose
//! `created_at + ttl_secs` is in the past.  For each expired approval it:
//!  1. Resolves the row to `auto_denied` in the store.
//!  2. Wakes the parked gate (`Notify`) so the waiting `run_pump` task
//!     unblocks, reads the resolution, and emits a `tool_blocked` event.
//!
//! ## Deadlock safety
//! `sweep_once` acquires the supervisor lock, collects expired IDs, resolves
//! them, then *releases* the lock before touching the gates map (a separate
//! `std::sync::Mutex`).  No lock is held across an `.await`.
//!
//! ## Testability
//! `sweep_once` is `pub(crate)` so tests can call it directly without waiting
//! for a real interval.  The background loop reads `COCKPIT_SWEEP_INTERVAL_MS`
//! so integration tests can set it to a very small value (e.g. 10 ms).

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::transport::SupervisorBox;

// ── sweep_once ────────────────────────────────────────────────────────────────

/// Perform one sweep pass: auto-deny all expired pending approvals and wake
/// their parked gates.
///
/// Returns the count of approvals auto-denied in this pass.
///
/// `now` is the reference instant for TTL comparison; passing it as a parameter
/// makes the function deterministically testable without wall-clock dependency.
///
/// ## Locking discipline
/// Acquires `supervisor` lock → resolves expired rows → releases lock → then
/// iterates cloned gate handles.  No lock is held while calling `notify_one`.
pub async fn sweep_once(
    supervisor: &Arc<Mutex<SupervisorBox>>,
    gates: &Arc<std::sync::Mutex<HashMap<Uuid, Arc<Notify>>>>,
    now: chrono::DateTime<Utc>,
) -> usize {
    // ── 1. Collect expired approval IDs under the supervisor lock ─────────────
    let expired_ids: Vec<Uuid> = {
        let sup = supervisor.lock().await;
        match sup.0.list_pending_approvals() {
            Ok(pendings) => pendings
                .into_iter()
                .filter(|a| {
                    let deadline = a.created_at
                        + chrono::Duration::seconds(a.ttl_secs as i64);
                    now >= deadline
                })
                .map(|a| a.id)
                .collect(),
            Err(e) => {
                warn!("sweep: list_pending_approvals failed: {e}");
                return 0;
            }
        }
    };

    if expired_ids.is_empty() {
        return 0;
    }

    // ── 2. Resolve each expired approval in the store ─────────────────────────
    let mut resolved = Vec::with_capacity(expired_ids.len());
    {
        let mut sup = supervisor.lock().await;
        for id in &expired_ids {
            match sup.0.resolve_approval(*id, "auto_denied") {
                Ok(()) => resolved.push(*id),
                Err(e) => warn!("sweep: resolve_approval({id}) failed: {e}"),
            }
        }
    }
    // Lock released here.

    // ── 3. Wake parked gates (no lock held) ───────────────────────────────────
    let gate_handles: Vec<Arc<Notify>> = {
        let g = gates.lock().unwrap();
        resolved
            .iter()
            .filter_map(|id| g.get(id).cloned())
            .collect()
    };

    for notify in &gate_handles {
        notify.notify_one();
    }

    let count = resolved.len();
    if count > 0 {
        debug!("sweep: auto-denied {count} expired approval(s)");
    }
    count
}

// ── spawn_sweep_task ──────────────────────────────────────────────────────────

/// Spawn a background tokio task that calls `sweep_once` on a fixed interval.
///
/// The interval defaults to 5 s but can be overridden by setting
/// `COCKPIT_SWEEP_INTERVAL_MS` in the environment (useful in integration tests
/// to keep them fast without real wall-clock waits).
///
/// The returned `JoinHandle` is detached (fire-and-forget); it terminates when
/// the process exits.
pub fn spawn_sweep_task(
    supervisor: Arc<Mutex<SupervisorBox>>,
    gates: Arc<std::sync::Mutex<HashMap<Uuid, Arc<Notify>>>>,
) -> tokio::task::JoinHandle<()> {
    let interval_ms: u64 = std::env::var("COCKPIT_SWEEP_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        // First tick fires immediately — skip it so we don't sweep at time-zero.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            sweep_once(&supervisor, &gates, Utc::now()).await;
        }
    })
}
