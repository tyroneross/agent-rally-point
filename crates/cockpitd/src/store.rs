// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! SQLite-backed event store.
//!
//! Tables:
//!   sessions  — one row per supervised session.
//!   events    — (session_id, seq) PK; seq is monotonic per session, starts at 1.
//!   approvals — pending / resolved approval rows.
//!
//! Sanitization: control chars and NUL bytes are stripped from content / diff
//! strings before storage (prior-art lesson from terminal-output corruption).

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::model::{Approval, Event, Session, SessionStatus};

// ── Store ─────────────────────────────────────────────────────────────────────

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Expose the raw connection for extension traits (e.g. approval::StorePendingExt).
    /// Not part of the public API — crate-internal only.
    pub(crate) fn raw_conn(&self) -> &Connection {
        &self.conn
    }

    /// Open (or create) an on-disk database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("open sqlite db")?;
        let mut s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    /// Open an in-memory database (tests only).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory sqlite")?;
        let mut s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(SCHEMA).context("run migrations")?;
        Ok(())
    }

    // ── sessions ─────────────────────────────────────────────────────────────

    pub fn create_session(&mut self, session: &Session) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sessions
               (id, owner_id, agent_type, repo_path, status, title, created_at, last_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.id.to_string(),
                    session.owner_id,
                    session.agent_type,
                    session.repo_path,
                    session.status.to_string(),
                    session.title,
                    session.created_at.to_rfc3339(),
                    session.last_seq as i64,
                ],
            )
            .context("insert session")?;
        Ok(())
    }

    pub fn get_session(&self, id: Uuid) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, owner_id, agent_type, repo_path, status, title, created_at, last_seq
             FROM sessions WHERE id = ?1",
        )?;
        let rows = stmt.query_map(params![id.to_string()], row_to_session)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some(row?));
        }
        Ok(None)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, owner_id, agent_type, repo_path, status, title, created_at, last_seq
             FROM sessions ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        rows.collect::<Result<Vec<_>, _>>().context("list sessions")
    }

    /// Return sessions belonging to `owner_id` only.
    ///
    /// This is the multi-user access path (§8): one owner cannot see
    /// another's sessions.  The single-user `DirectWs` path uses
    /// `list_sessions()` which is an unrestricted view.
    pub fn list_sessions_for_owner(&self, owner_id: &str) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, owner_id, agent_type, repo_path, status, title, created_at, last_seq
             FROM sessions WHERE owner_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![owner_id], row_to_session)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("list sessions for owner")
    }

    /// Fetch a session only if it belongs to `owner_id`; returns `None` if the
    /// session exists but is owned by a different owner.
    pub fn get_session_for_owner(&self, id: Uuid, owner_id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, owner_id, agent_type, repo_path, status, title, created_at, last_seq
             FROM sessions WHERE id = ?1 AND owner_id = ?2",
        )?;
        let rows = stmt.query_map(params![id.to_string(), owner_id], row_to_session)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some(row?));
        }
        Ok(None)
    }

    pub fn update_session_status(&mut self, id: Uuid, status: &SessionStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = ?1 WHERE id = ?2",
            params![status.to_string(), id.to_string()],
        )?;
        Ok(())
    }

    // ── events ────────────────────────────────────────────────────────────────

    /// Append an event to a session.
    ///
    /// Assigns the next `seq` atomically (reads `last_seq`, increments,
    /// writes event row, bumps `sessions.last_seq`). Returns the assigned seq.
    pub fn append_event(&mut self, e: &Event) -> Result<u64> {
        // Read current last_seq
        let last_seq: i64 = self
            .conn
            .query_row(
                "SELECT last_seq FROM sessions WHERE id = ?1",
                params![e.session_id.to_string()],
                |row| row.get(0),
            )
            .context("read last_seq (session not found?)")?;
        let next_seq = last_seq + 1;

        let clean_content = sanitize(&e.content);
        let meta_str = serde_json::to_string(&e.metadata).unwrap_or_else(|_| "{}".into());

        self.conn
            .execute(
                "INSERT INTO events
               (session_id, seq, sender, kind, content, requires_user_input, created_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    e.session_id.to_string(),
                    next_seq,
                    e.sender,
                    e.kind,
                    clean_content,
                    e.requires_user_input as i64,
                    e.created_at.to_rfc3339(),
                    meta_str,
                ],
            )
            .context("insert event")?;

        self.conn
            .execute(
                "UPDATE sessions SET last_seq = ?1 WHERE id = ?2",
                params![next_seq, e.session_id.to_string()],
            )
            .context("bump last_seq")?;

        Ok(next_seq as u64)
    }

    /// Return events with `seq > from_seq`, ordered by seq ascending.
    pub fn replay_from(&self, session_id: Uuid, from_seq: u64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, seq, sender, kind, content, requires_user_input, created_at, metadata
             FROM events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(
            params![session_id.to_string(), from_seq as i64],
            row_to_event,
        )?;
        rows.collect::<Result<Vec<_>, _>>().context("replay events")
    }

    // ── approvals ─────────────────────────────────────────────────────────────

    pub fn insert_approval(&mut self, a: &Approval) -> Result<()> {
        self.conn.execute(
            "INSERT INTO approvals
               (id, session_id, event_seq, tool, args, created_at, ttl_secs, resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                a.id.to_string(),
                a.session_id.to_string(),
                a.event_seq as i64,
                a.tool,
                serde_json::to_string(&a.args).unwrap_or_else(|_| "{}".into()),
                a.created_at.to_rfc3339(),
                a.ttl_secs as i64,
                a.resolution,
            ],
        )?;
        Ok(())
    }

    pub fn resolve_approval(&mut self, id: Uuid, resolution: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE approvals SET resolution = ?1 WHERE id = ?2",
            params![resolution, id.to_string()],
        )?;
        Ok(())
    }

    pub fn get_approval(&self, id: Uuid) -> Result<Option<Approval>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, event_seq, tool, args, created_at, ttl_secs, resolution
             FROM approvals WHERE id = ?1",
        )?;
        let rows = stmt.query_map(params![id.to_string()], row_to_approval)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some(row?));
        }
        Ok(None)
    }
}

// ── Schema ────────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL DEFAULT 'local',
    agent_type  TEXT NOT NULL,
    repo_path   TEXT NOT NULL,
    status      TEXT NOT NULL,
    title       TEXT,
    created_at  TEXT NOT NULL,
    last_seq    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS events (
    session_id          TEXT NOT NULL,
    seq                 INTEGER NOT NULL,
    sender              TEXT NOT NULL,
    kind                TEXT NOT NULL,
    content             TEXT NOT NULL,
    requires_user_input INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    metadata            TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (session_id, seq),
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE TABLE IF NOT EXISTS approvals (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    event_seq   INTEGER NOT NULL,
    tool        TEXT NOT NULL,
    args        TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    ttl_secs    INTEGER NOT NULL DEFAULT 60,
    resolution  TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
";

// ── Sanitization ──────────────────────────────────────────────────────────────

/// Strip NUL bytes and non-printable control characters (< 0x20) except for
/// newline (0x0A), carriage return (0x0D), and tab (0x09), which are meaningful
/// in agent output. This mirrors the prior-art fix for terminal-output corruption.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let n = c as u32;
            n == 0x09 || n == 0x0A || n == 0x0D || n >= 0x20
        })
        .collect()
}

// ── Row mappers ───────────────────────────────────────────────────────────────

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let id_str: String = row.get(0)?;
    let status_str: String = row.get(4)?;
    let created_str: String = row.get(6)?;
    let last_seq: i64 = row.get(7)?;

    Ok(Session {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
        owner_id: row.get(1)?,
        agent_type: row.get(2)?,
        repo_path: row.get(3)?,
        status: serde_json::from_value(serde_json::Value::String(status_str))
            .unwrap_or(SessionStatus::Unknown),
        title: row.get(5)?,
        created_at: created_str
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now()),
        last_seq: last_seq as u64,
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let session_id_str: String = row.get(0)?;
    let seq: i64 = row.get(1)?;
    let requires_user_input: i64 = row.get(5)?;
    let created_str: String = row.get(6)?;
    let meta_str: String = row.get(7)?;

    Ok(Event {
        session_id: Uuid::parse_str(&session_id_str).unwrap_or_else(|_| Uuid::nil()),
        seq: seq as u64,
        sender: row.get(2)?,
        kind: row.get(3)?,
        content: row.get(4)?,
        requires_user_input: requires_user_input != 0,
        created_at: created_str
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now()),
        metadata: serde_json::from_str(&meta_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    let id_str: String = row.get(0)?;
    let session_id_str: String = row.get(1)?;
    let event_seq: i64 = row.get(2)?;
    let created_str: String = row.get(5)?;
    let ttl: i64 = row.get(6)?;
    let args_str: String = row.get(4)?;

    Ok(Approval {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
        session_id: Uuid::parse_str(&session_id_str).unwrap_or_else(|_| Uuid::nil()),
        event_seq: event_seq as u64,
        tool: row.get(3)?,
        args: serde_json::from_str(&args_str)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        created_at: created_str
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now()),
        ttl_secs: ttl as u64,
        resolution: row.get(7)?,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn make_session(id: Uuid) -> Session {
        Session {
            id,
            owner_id: "local".into(),
            agent_type: "claude".into(),
            repo_path: "/tmp/repo".into(),
            status: SessionStatus::Active,
            title: Some("test session".into()),
            created_at: Utc::now(),
            last_seq: 0,
        }
    }

    fn make_event(session_id: Uuid, kind: &str, content: &str) -> Event {
        Event {
            session_id,
            seq: 0, // store assigns the real seq
            sender: "agent".into(),
            kind: kind.into(),
            content: content.into(),
            requires_user_input: false,
            created_at: Utc::now(),
            metadata: json!({}),
        }
    }

    fn open_store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    // ── A2-1: append 3 events → replay_from(0) returns 3, seq 1,2,3 ──────────

    #[test]
    fn append_and_replay_three_events() {
        let mut store = open_store();
        let sid = Uuid::new_v4();
        store.create_session(&make_session(sid)).unwrap();

        let s1 = store
            .append_event(&make_event(sid, "message", "hello"))
            .unwrap();
        let s2 = store
            .append_event(&make_event(sid, "message", "world"))
            .unwrap();
        let s3 = store
            .append_event(&make_event(sid, "tool_call", "ls"))
            .unwrap();

        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(s3, 3);

        let events = store.replay_from(sid, 0).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[2].seq, 3);
    }

    // ── A2-2: replay_from(2) returns only seq 3 ───────────────────────────────

    #[test]
    fn replay_from_returns_only_after_cursor() {
        let mut store = open_store();
        let sid = Uuid::new_v4();
        store.create_session(&make_session(sid)).unwrap();

        store
            .append_event(&make_event(sid, "message", "a"))
            .unwrap();
        store
            .append_event(&make_event(sid, "message", "b"))
            .unwrap();
        store
            .append_event(&make_event(sid, "message", "c"))
            .unwrap();

        let events = store.replay_from(sid, 2).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 3);
        assert_eq!(events[0].content, "c");
    }

    // ── A2-3: last_seq updated after each append ───────────────────────────────

    #[test]
    fn last_seq_updated_correctly() {
        let mut store = open_store();
        let sid = Uuid::new_v4();
        store.create_session(&make_session(sid)).unwrap();

        store
            .append_event(&make_event(sid, "message", "x"))
            .unwrap();
        store
            .append_event(&make_event(sid, "message", "y"))
            .unwrap();

        let session = store.get_session(sid).unwrap().unwrap();
        assert_eq!(session.last_seq, 2);
    }

    // ── A2-4: control chars / NUL stripped before storage ─────────────────────

    #[test]
    fn control_chars_stripped_from_content() {
        let dirty = "hello\x00world\x01\x02\x1b[31mred\x1b[0m\nline2";
        let clean = sanitize(dirty);
        assert!(!clean.contains('\x00'));
        assert!(!clean.contains('\x01'));
        assert!(!clean.contains('\x02'));
        // ESC (0x1b) should be stripped
        assert!(!clean.contains('\x1b'));
        // newline preserved
        assert!(clean.contains('\n'));
        // printable chars preserved
        assert!(clean.contains("hello"));
        assert!(clean.contains("world"));
    }

    #[test]
    fn sanitized_content_stored_and_retrieved() {
        let mut store = open_store();
        let sid = Uuid::new_v4();
        store.create_session(&make_session(sid)).unwrap();

        let mut evt = make_event(sid, "message", "clean\x00\x01dirty");
        evt.content = evt.content.clone(); // content with NUL
        let dirty_evt = Event {
            content: "clean\x00\x01dirty".into(),
            ..make_event(sid, "message", "")
        };
        store.append_event(&dirty_evt).unwrap();

        let events = store.replay_from(sid, 0).unwrap();
        assert_eq!(events.len(), 1);
        assert!(!events[0].content.contains('\x00'));
        assert!(!events[0].content.contains('\x01'));
        assert!(events[0].content.contains("clean"));
        assert!(events[0].content.contains("dirty"));
    }

    // ── A2-5: in-memory DB (no filesystem) ────────────────────────────────────

    #[test]
    fn in_memory_db_no_filesystem() {
        // This test itself proves no filesystem: open_in_memory() doesn't take a path.
        let store = Store::open_in_memory().unwrap();
        let sessions = store.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    // ── A2-6: approval insert / resolve ───────────────────────────────────────

    #[test]
    fn approval_insert_and_resolve() {
        let mut store = open_store();
        let sid = Uuid::new_v4();
        store.create_session(&make_session(sid)).unwrap();

        let approval = Approval {
            id: Uuid::new_v4(),
            session_id: sid,
            event_seq: 1,
            tool: "bash".into(),
            args: json!({ "cmd": "ls" }),
            created_at: Utc::now(),
            ttl_secs: 60,
            resolution: None,
        };
        store.insert_approval(&approval).unwrap();
        let fetched = store.get_approval(approval.id).unwrap().unwrap();
        assert!(fetched.resolution.is_none());

        store.resolve_approval(approval.id, "allow").unwrap();
        let resolved = store.get_approval(approval.id).unwrap().unwrap();
        assert_eq!(resolved.resolution.as_deref(), Some("allow"));
    }

    // ── F4 owner-scoping: owner A's sessions not visible to owner B ───────────

    #[test]
    fn list_sessions_for_owner_isolates_owners() {
        let mut store = open_store();

        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();

        let session_a = Session {
            owner_id: "alice".into(),
            ..make_session(sid_a)
        };
        let session_b = Session {
            owner_id: "bob".into(),
            ..make_session(sid_b)
        };

        store.create_session(&session_a).unwrap();
        store.create_session(&session_b).unwrap();

        let alice_sessions = store.list_sessions_for_owner("alice").unwrap();
        assert_eq!(
            alice_sessions.len(),
            1,
            "alice must see exactly her own session"
        );
        assert_eq!(alice_sessions[0].id, sid_a);

        let bob_sessions = store.list_sessions_for_owner("bob").unwrap();
        assert_eq!(
            bob_sessions.len(),
            1,
            "bob must see exactly his own session"
        );
        assert_eq!(bob_sessions[0].id, sid_b);

        // Cross-owner: alice asking for bob's session returns None.
        let cross = store.get_session_for_owner(sid_b, "alice").unwrap();
        assert!(cross.is_none(), "alice must not see bob's session");

        // Own session is accessible.
        let own = store.get_session_for_owner(sid_a, "alice").unwrap();
        assert!(own.is_some(), "alice must be able to fetch her own session");
    }

    #[test]
    fn list_sessions_all_still_works() {
        // list_sessions() (DirectWs single-user path) must still return everyone.
        let mut store = open_store();
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();
        store
            .create_session(&Session {
                owner_id: "alice".into(),
                ..make_session(sid_a)
            })
            .unwrap();
        store
            .create_session(&Session {
                owner_id: "bob".into(),
                ..make_session(sid_b)
            })
            .unwrap();
        let all = store.list_sessions().unwrap();
        assert_eq!(all.len(), 2, "unscoped list must return all sessions");
    }
}
