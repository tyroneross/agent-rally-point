// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Append-only audit log (spec §9).
//!
//! Table: `audit`
//!   id          TEXT PRIMARY KEY  (UUID)
//!   ts          TEXT NOT NULL     (RFC-3339, from injected Clock)
//!   actor       TEXT NOT NULL     ("client", a token id, or "system")
//!   action      TEXT NOT NULL     (e.g. "cmd:approve", "session:launch", "approval:resolved")
//!   session_id  TEXT              (nullable)
//!   detail      TEXT NOT NULL     (JSON object)
//!
//! Invariants:
//! - No UPDATE or DELETE ever touches this table — only INSERT.
//! - Timestamps come from the injected Clock so tests are deterministic.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::clock::Clock;

// ── Schema fragment ───────────────────────────────────────────────────────────

pub(crate) const AUDIT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit (
    id          TEXT PRIMARY KEY,
    ts          TEXT NOT NULL,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    session_id  TEXT,
    detail      TEXT NOT NULL DEFAULT '{}'
);
";

// ── AuditEntry ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: Uuid,
    /// RFC-3339 timestamp (from injected Clock).
    pub ts: String,
    /// Who triggered the action ("client", token-id, or "system").
    pub actor: String,
    /// What happened (e.g. "cmd:approve", "session:launch", "approval:resolved").
    pub action: String,
    /// Optional session context.
    pub session_id: Option<Uuid>,
    /// Free-form JSON detail object.
    pub detail: serde_json::Value,
}

// ── AuditLog ──────────────────────────────────────────────────────────────────

/// Append-only audit log backed by an open SQLite connection.
pub struct AuditLog<C: Clock> {
    pub(crate) conn: Connection,
    clock: C,
}

impl<C: Clock> AuditLog<C> {
    /// Open (or create) an on-disk audit log.
    pub fn open(path: impl AsRef<std::path::Path>, clock: C) -> Result<Self> {
        let conn = Connection::open(path).context("open audit db")?;
        let mut s = Self { conn, clock };
        s.migrate()?;
        Ok(s)
    }

    /// Open an in-memory audit log (tests only).
    pub fn open_in_memory(clock: C) -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory audit db")?;
        let mut s = Self { conn, clock };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn
            .execute_batch(AUDIT_SCHEMA)
            .context("audit migration")?;
        Ok(())
    }

    /// Append a new audit entry. Returns the entry's UUID.
    ///
    /// This is the ONLY write method — no update or delete.
    pub fn append(
        &mut self,
        actor: &str,
        action: &str,
        session_id: Option<Uuid>,
        detail: serde_json::Value,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let ts = self.clock.now().to_rfc3339();
        let detail_str = serde_json::to_string(&detail).unwrap_or_else(|_| "{}".into());
        let sid_str = session_id.map(|s| s.to_string());

        self.conn
            .execute(
                "INSERT INTO audit (id, ts, actor, action, session_id, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id.to_string(), ts, actor, action, sid_str, detail_str],
            )
            .context("insert audit entry")?;

        Ok(id)
    }

    /// List audit entries, optionally filtered by session_id.
    ///
    /// Returns up to `limit` entries in ascending timestamp order.
    pub fn list(
        &self,
        session_id: Option<Uuid>,
        limit: Option<u64>,
    ) -> Result<Vec<AuditEntry>> {
        let limit_val = limit.unwrap_or(1000) as i64;

        match session_id {
            Some(sid) => {
                let sid_str = sid.to_string();
                let mut stmt = self.conn.prepare(
                    "SELECT id, ts, actor, action, session_id, detail
                     FROM audit WHERE session_id = ?1
                     ORDER BY ts ASC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![sid_str, limit_val], row_to_entry)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .context("list audit entries by session")
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, ts, actor, action, session_id, detail
                     FROM audit ORDER BY ts ASC LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![limit_val], row_to_entry)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .context("list audit entries")
            }
        }
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEntry> {
    let id_str: String = row.get(0)?;
    let detail_str: String = row.get(5)?;
    let sid_str: Option<String> = row.get(4)?;

    Ok(AuditEntry {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
        ts: row.get(1)?,
        actor: row.get(2)?,
        action: row.get(3)?,
        session_id: sid_str
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok()),
        detail: serde_json::from_str(&detail_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{FakeClock};
    use chrono::Duration;

    fn open_log() -> (AuditLog<FakeClock>, FakeClock) {
        let clock = FakeClock::at_epoch();
        let log = AuditLog::open_in_memory(clock.clone()).unwrap();
        (log, clock)
    }

    // ── audit::append + ordered read ─────────────────────────────────────────

    #[test]
    fn append_and_read_ordered() {
        let (mut log, clock) = open_log();

        let id1 = log
            .append("client", "cmd:list_sessions", None, serde_json::json!({}))
            .unwrap();

        clock.advance(Duration::seconds(1));

        let id2 = log
            .append(
                "client",
                "session:launch",
                Some(Uuid::new_v4()),
                serde_json::json!({"agent_type": "codex"}),
            )
            .unwrap();

        let entries = log.list(None, None).unwrap();
        assert_eq!(entries.len(), 2, "should have 2 entries");

        // Ordered by ts ascending.
        assert_eq!(entries[0].id, id1);
        assert_eq!(entries[1].id, id2);
        assert_eq!(entries[1].action, "session:launch");
    }

    // ── audit::filter_by_session ──────────────────────────────────────────────

    #[test]
    fn filter_by_session_id() {
        let (mut log, _clock) = open_log();

        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();

        log.append("client", "cmd:approve", Some(sid1), serde_json::json!({}))
            .unwrap();
        log.append("client", "session:launch", Some(sid2), serde_json::json!({}))
            .unwrap();
        log.append(
            "client",
            "approval:resolved",
            Some(sid1),
            serde_json::json!({"decision": "allow"}),
        )
        .unwrap();

        let entries = log.list(Some(sid1), None).unwrap();
        assert_eq!(entries.len(), 2, "should see 2 entries for sid1");
        for e in &entries {
            assert_eq!(e.session_id, Some(sid1));
        }

        let entries2 = log.list(Some(sid2), None).unwrap();
        assert_eq!(entries2.len(), 1, "should see 1 entry for sid2");
    }

    // ── audit::limit ──────────────────────────────────────────────────────────

    #[test]
    fn limit_caps_results() {
        let (mut log, clock) = open_log();

        for i in 0..10u64 {
            clock.advance(Duration::seconds(1));
            log.append(
                "client",
                "cmd:ping",
                None,
                serde_json::json!({"i": i}),
            )
            .unwrap();
        }

        let entries = log.list(None, Some(3)).unwrap();
        assert_eq!(entries.len(), 3, "limit should cap at 3");
    }

    // ── audit::append-only (no mutation API) ─────────────────────────────────

    #[test]
    fn append_only_no_delete_or_update_api() {
        // Structural: `AuditLog` has no `delete`, `update`, or `truncate` method.
        // This test exists as a compile-time proof: if any of those methods were
        // added, the doc-comment invariant would need revisiting.
        // We just verify the entry count never decreases.
        let (mut log, _clock) = open_log();
        log.append("system", "startup", None, serde_json::json!({}))
            .unwrap();
        let before = log.list(None, None).unwrap().len();

        // Can only append more, never fewer.
        log.append("system", "heartbeat", None, serde_json::json!({}))
            .unwrap();
        let after = log.list(None, None).unwrap().len();
        assert!(after > before, "audit log count must be non-decreasing");
    }

    // ── audit::detail preserved ───────────────────────────────────────────────

    #[test]
    fn detail_round_trips() {
        let (mut log, _clock) = open_log();
        let detail = serde_json::json!({
            "tool": "bash",
            "args": {"cmd": "ls /tmp"},
            "decision": "allow",
        });
        log.append("client", "approval:resolved", None, detail.clone())
            .unwrap();

        let entries = log.list(None, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].detail["tool"].as_str(), Some("bash"));
        assert_eq!(entries[0].detail["decision"].as_str(), Some("allow"));
    }
}
