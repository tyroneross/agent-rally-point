// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Approval state machine.
//!
//! Wraps the SQLite-backed `Store` with pending-TTL logic and a `sweep()`
//! that auto-denies expired approvals. Uses an injected `Clock` so TTL is
//! fully testable without wall-clock dependency.
//!
//! ## State transitions:
//!   pending (resolution = NULL)
//!     → allow / deny   via `resolve(id, decision)`
//!     → auto_denied    via `sweep(now)` when created_at + ttl_secs < now
//!     → aborted        via `abort(id)` (if session killed before approval resolved)
//!
//! ## Integration with Supervisor (described here, wired in later):
//!   When an adapter emits an `approval_request` Event, the Supervisor calls
//!   `ApprovalManager::register_pending`. A client `approve` command calls
//!   `ApprovalManager::resolve`, which also sends the decision back to the
//!   live session via `SessionCommand::Approve`.

use anyhow::{Result, bail};
use chrono::Duration;
use uuid::Uuid;

use crate::clock::Clock;
use crate::model::Approval;
use crate::store::Store;

// ── ApprovalManager ───────────────────────────────────────────────────────────

/// Thin state-machine layer over the Store for approval lifecycle.
pub struct ApprovalManager<C: Clock> {
    store: Store,
    clock: C,
}

impl<C: Clock> ApprovalManager<C> {
    pub fn new(store: Store, clock: C) -> Self {
        Self { store, clock }
    }

    /// Register a new pending approval.
    ///
    /// Persists the `Approval` row with `resolution = None`.
    pub fn register_pending(&mut self, approval: &Approval) -> Result<()> {
        if approval.resolution.is_some() {
            bail!("register_pending called with a pre-resolved approval");
        }
        self.store.insert_approval(approval)?;
        Ok(())
    }

    /// Resolve a pending approval with the given decision ("allow" or "deny").
    ///
    /// - Returns `Ok(())` on success.
    /// - Returns `Err` if `id` not found.
    /// - Is a no-op (returns `Ok`) if already resolved (idempotent — prevents
    ///   double-resolve from a retry).
    pub fn resolve(&mut self, id: Uuid, decision: &str) -> Result<()> {
        let existing = self
            .store
            .get_approval(id)?
            .ok_or_else(|| anyhow::anyhow!("approval {id} not found"))?;

        if existing.resolution.is_some() {
            // Already resolved — treat as no-op.
            tracing::debug!(
                "approval {id} already resolved as {:?}, ignoring re-resolve",
                existing.resolution
            );
            return Ok(());
        }

        self.store.resolve_approval(id, decision)?;
        Ok(())
    }

    /// Mark an approval as aborted (e.g. session killed before it was resolved).
    ///
    /// No-op if already resolved.
    pub fn abort(&mut self, id: Uuid) -> Result<()> {
        let existing = self
            .store
            .get_approval(id)?
            .ok_or_else(|| anyhow::anyhow!("approval {id} not found"))?;

        if existing.resolution.is_some() {
            return Ok(());
        }

        self.store.resolve_approval(id, "aborted")?;
        Ok(())
    }

    /// Auto-deny all pending approvals whose TTL has expired relative to `now`.
    ///
    /// Returns the count of approvals auto-denied.
    pub fn sweep(&mut self) -> Result<usize> {
        let now = self.clock.now();
        let pendings = self.store.list_pending_approvals()?;

        let mut count = 0;
        for approval in &pendings {
            let deadline = approval.created_at + Duration::seconds(approval.ttl_secs as i64);
            if now >= deadline {
                self.store.resolve_approval(approval.id, "auto_denied")?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Retrieve a single approval by ID.
    pub fn get(&self, id: Uuid) -> Result<Option<Approval>> {
        self.store.get_approval(id)
    }
}

// ── Store extension: list_pending_approvals ───────────────────────────────────

// We need `list_pending_approvals` on the Store. Rather than modifying store.rs
// mid-chunk, we extend it here via a trait. The trait is private (pub(crate)).

pub(crate) trait StorePendingExt {
    fn list_pending_approvals(&self) -> Result<Vec<Approval>>;
}

impl StorePendingExt for Store {
    fn list_pending_approvals(&self) -> Result<Vec<Approval>> {
        use crate::model::Approval;
        use rusqlite::params;

        let conn = self.raw_conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, event_seq, tool, args, created_at, ttl_secs, resolution
             FROM approvals WHERE resolution IS NULL",
        )?;

        let rows = stmt.query_map(params![], |row| {
            let id_str: String = row.get(0)?;
            let session_id_str: String = row.get(1)?;
            let event_seq: i64 = row.get(2)?;
            let created_str: String = row.get(5)?;
            let ttl: i64 = row.get(6)?;
            let args_str: String = row.get(4)?;

            Ok(Approval {
                id: uuid::Uuid::parse_str(&id_str).unwrap_or_else(|_| uuid::Uuid::nil()),
                session_id: uuid::Uuid::parse_str(&session_id_str)
                    .unwrap_or_else(|_| uuid::Uuid::nil()),
                event_seq: event_seq as u64,
                tool: row.get(3)?,
                args: serde_json::from_str(&args_str)
                    .unwrap_or(serde_json::Value::Object(Default::default())),
                created_at: created_str
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap_or_else(|_| chrono::Utc::now()),
                ttl_secs: ttl as u64,
                resolution: row.get(7)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("list_pending_approvals: {e}"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::model::{Approval, Session, SessionStatus};
    use crate::store::Store;
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn open_manager() -> (ApprovalManager<FakeClock>, FakeClock) {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::at_epoch();
        let mgr = ApprovalManager::new(store, clock.clone());
        (mgr, clock)
    }

    fn seed_session(mgr: &mut ApprovalManager<FakeClock>) -> Uuid {
        let sid = Uuid::new_v4();
        let session = Session {
            id: sid,
            owner_id: "local".into(),
            agent_type: "claude".into(),
            repo_path: "/tmp".into(),
            status: SessionStatus::Active,
            title: None,
            created_at: Utc::now(),
            last_seq: 0,
        };
        mgr.store.create_session(&session).unwrap();
        sid
    }

    fn make_approval(
        session_id: Uuid,
        ttl_secs: u64,
        created_at: chrono::DateTime<Utc>,
    ) -> Approval {
        Approval {
            id: Uuid::new_v4(),
            session_id,
            event_seq: 1,
            tool: "bash".into(),
            args: serde_json::json!({ "cmd": "ls" }),
            created_at,
            ttl_secs,
            resolution: None,
        }
    }

    // ── B3-1: register → resolve(allow) sets resolution ───────────────────────

    #[test]
    fn register_and_resolve_allow() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);
        let approval = make_approval(sid, 60, clock.now());

        mgr.register_pending(&approval).unwrap();

        let fetched = mgr.get(approval.id).unwrap().unwrap();
        assert!(fetched.resolution.is_none());

        mgr.resolve(approval.id, "allow").unwrap();

        let resolved = mgr.get(approval.id).unwrap().unwrap();
        assert_eq!(resolved.resolution.as_deref(), Some("allow"));
    }

    // ── B3-2: register → resolve(deny) sets resolution ────────────────────────

    #[test]
    fn register_and_resolve_deny() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);
        let approval = make_approval(sid, 60, clock.now());

        mgr.register_pending(&approval).unwrap();
        mgr.resolve(approval.id, "deny").unwrap();

        let resolved = mgr.get(approval.id).unwrap().unwrap();
        assert_eq!(resolved.resolution.as_deref(), Some("deny"));
    }

    // ── B3-3: TTL expiry → sweep auto-denies ──────────────────────────────────

    #[test]
    fn ttl_expiry_sweep_auto_denies() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);

        // created_at = epoch; ttl = 10s
        let approval = make_approval(sid, 10, clock.now()); // epoch
        mgr.register_pending(&approval).unwrap();

        // Advance clock by 11 seconds (past TTL).
        clock.advance(Duration::seconds(11));

        let denied_count = mgr.sweep().unwrap();
        assert_eq!(denied_count, 1);

        let fetched = mgr.get(approval.id).unwrap().unwrap();
        assert_eq!(fetched.resolution.as_deref(), Some("auto_denied"));
    }

    // ── B3-4: sweep does NOT auto-deny before TTL ─────────────────────────────

    #[test]
    fn sweep_does_not_deny_before_ttl() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);

        let approval = make_approval(sid, 10, clock.now());
        mgr.register_pending(&approval).unwrap();

        // Advance by only 9 seconds — TTL not yet expired.
        clock.advance(Duration::seconds(9));

        let denied_count = mgr.sweep().unwrap();
        assert_eq!(denied_count, 0);

        let fetched = mgr.get(approval.id).unwrap().unwrap();
        assert!(fetched.resolution.is_none(), "should still be pending");
    }

    // ── B3-5: resolving already-resolved is a no-op ───────────────────────────

    #[test]
    fn double_resolve_is_noop() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);
        let approval = make_approval(sid, 60, clock.now());

        mgr.register_pending(&approval).unwrap();
        mgr.resolve(approval.id, "allow").unwrap();
        // Second resolve with different decision — should be ignored.
        mgr.resolve(approval.id, "deny").unwrap(); // no error, no change

        let fetched = mgr.get(approval.id).unwrap().unwrap();
        assert_eq!(
            fetched.resolution.as_deref(),
            Some("allow"),
            "first resolution must win"
        );
    }

    // ── B3-6: sweep skips already-resolved rows ───────────────────────────────

    #[test]
    fn sweep_skips_resolved_rows() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);

        let a1 = make_approval(sid, 10, clock.now());
        let a2 = make_approval(sid, 10, clock.now());

        mgr.register_pending(&a1).unwrap();
        mgr.register_pending(&a2).unwrap();

        // Resolve a1 manually.
        mgr.resolve(a1.id, "allow").unwrap();

        // Advance past TTL.
        clock.advance(Duration::seconds(11));

        let denied_count = mgr.sweep().unwrap();
        // Only a2 should be auto_denied; a1 is already resolved.
        assert_eq!(denied_count, 1);

        let fetched_a1 = mgr.get(a1.id).unwrap().unwrap();
        assert_eq!(fetched_a1.resolution.as_deref(), Some("allow"));

        let fetched_a2 = mgr.get(a2.id).unwrap().unwrap();
        assert_eq!(fetched_a2.resolution.as_deref(), Some("auto_denied"));
    }

    // ── B3-7: abort sets resolution to "aborted" ──────────────────────────────

    #[test]
    fn abort_sets_aborted_resolution() {
        let (mut mgr, clock) = open_manager();
        let sid = seed_session(&mut mgr);
        let approval = make_approval(sid, 60, clock.now());

        mgr.register_pending(&approval).unwrap();
        mgr.abort(approval.id).unwrap();

        let fetched = mgr.get(approval.id).unwrap().unwrap();
        assert_eq!(fetched.resolution.as_deref(), Some("aborted"));
    }

    // ── B3-8: resolve unknown id returns error ────────────────────────────────

    #[test]
    fn resolve_unknown_id_returns_error() {
        let (mut mgr, _clock) = open_manager();
        let unknown = Uuid::new_v4();
        let result = mgr.resolve(unknown, "allow");
        assert!(result.is_err(), "resolving unknown id should error");
    }
}
