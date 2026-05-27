// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Rally graph projection — SQLite-backed derived view over `changes.jsonl`.
//!
//! `changes.jsonl` is canonical. This module maintains a derived graph
//! projection so queries like "subgraph around node N", "causal chain back
//! from event E", and "unconsumed artifacts" become recursive-CTE SQL
//! instead of bespoke event-walking code.
//!
//! Discipline:
//!   * Events mutate; the graph projects; queries read the projection.
//!     One-way data flow. The graph never writes events back to the log.
//!   * Each projection tracks its own offset against the log. Other
//!     projections (CheckpointStatus, TraceProjection) are independent.
//!   * Lazy-on-read catch-up: callers invoke `Graph::catch_up(records)`
//!     before serving a query; sub-second at our scale, no daemon needed.
//!   * Schema bumps trigger an in-place rebuild with a status flag.
//!     Readers see `{status: "rebuilding"}` rather than partial data.
//!
//! Schema:
//!   * `nodes(id, kind, subject, payload_json, created_at, source_event_id, source_seq)`
//!   * `edges(source_id, target_id, kind, payload_json, source_event_id, source_seq)`
//!   * `graph_meta(key, value)` — `schema_version`, `last_applied_seq`,
//!     `status`, `last_built_at`.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Re-export so callers don't pull in rusqlite directly.
pub use rusqlite::Connection as GraphConnection;

/// Bumped when the projection schema or builder semantics change. Triggers
/// a full rebuild on next `catch_up`. v2 added the `producer_tool` column
/// on `nodes` and changed `unconsumed_artifacts` to "no later non-producer
/// activity" (vs the v1 "no reviewed_by edge", which missed the real
/// review flow where acks live on handoffs not artifacts).
pub const SCHEMA_VERSION: i64 = 2;

const GRAPH_FILENAME: &str = "rally.graph.db";

/// Subdirectory under the channel where the projection cache lives.
const GRAPH_SUBDIR: &str = "rally";

/// Projection status — surfaced on every query response so callers can see
/// staleness without a separate roundtrip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphStatus {
    Ok,
    Rebuilding,
    Stale,
    Empty,
}

impl GraphStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rebuilding => "rebuilding",
            Self::Stale => "stale",
            Self::Empty => "empty",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GraphMeta {
    pub schema_version: i64,
    pub last_applied_seq: i64,
    pub status: GraphStatus,
    pub last_built_at: Option<String>,
}

/// Resolve the graph DB path under `<channel_dir>/rally/rally.graph.db`.
pub fn graph_path(channel_dir: &Path) -> PathBuf {
    channel_dir.join(GRAPH_SUBDIR).join(GRAPH_FILENAME)
}

/// Open or create the graph DB. Idempotent — running `init` on an existing
/// DB is a no-op aside from `last_built_at` getting refreshed.
pub fn init(channel_dir: &Path, now_rfc3339: &str) -> Result<Connection, rusqlite::Error> {
    let path = graph_path(channel_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!("create_dir_all({}): {err}", parent.display())),
            )
        })?;
    }
    let conn = Connection::open(&path)?;
    // If a prior schema version's tables exist, drop them before applying
    // the current schema. CREATE TABLE IF NOT EXISTS won't add new columns
    // to an existing table, so a version bump that adds columns requires
    // a clean recreate. Catch_up will replay events into the fresh tables.
    drop_stale_tables_if_schema_changed(&conn)?;
    apply_schema(&conn)?;
    seed_meta(&conn, now_rfc3339)?;
    Ok(conn)
}

fn drop_stale_tables_if_schema_changed(conn: &Connection) -> Result<(), rusqlite::Error> {
    // graph_meta may not exist yet on a fresh DB; query defensively.
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='graph_meta'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !table_exists {
        return Ok(());
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM graph_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let stored_version: i64 = stored.and_then(|v| v.parse().ok()).unwrap_or(0);
    if stored_version != SCHEMA_VERSION {
        // Newer version exists in code → wipe and rebuild. Same version → leave alone.
        conn.execute("DROP TABLE IF EXISTS edges", [])?;
        conn.execute("DROP TABLE IF EXISTS nodes", [])?;
        conn.execute("DROP TABLE IF EXISTS graph_meta", [])?;
    }
    Ok(())
}

fn apply_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS nodes (
            id              TEXT PRIMARY KEY,
            kind            TEXT NOT NULL,
            subject         TEXT,
            payload_json    TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            source_event_id TEXT,
            source_seq      INTEGER,
            producer_tool   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_kind     ON nodes(kind);
        CREATE INDEX IF NOT EXISTS idx_nodes_subject  ON nodes(subject);
        CREATE INDEX IF NOT EXISTS idx_nodes_producer ON nodes(producer_tool);

        CREATE TABLE IF NOT EXISTS edges (
            source_id       TEXT NOT NULL,
            target_id       TEXT NOT NULL,
            kind            TEXT NOT NULL,
            payload_json    TEXT,
            source_event_id TEXT NOT NULL,
            source_seq      INTEGER NOT NULL,
            PRIMARY KEY (source_id, target_id, kind, source_event_id)
        );
        CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id, kind);

        CREATE TABLE IF NOT EXISTS graph_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )
}

fn seed_meta(conn: &Connection, now_rfc3339: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO graph_meta(key, value) VALUES (?1, ?2)",
        params!["schema_version", SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO graph_meta(key, value) VALUES (?1, ?2)",
        params!["last_applied_seq", "0"],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO graph_meta(key, value) VALUES (?1, ?2)",
        params!["status", GraphStatus::Empty.as_str()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO graph_meta(key, value) VALUES (?1, ?2)",
        params!["last_built_at", now_rfc3339],
    )?;
    Ok(())
}

pub fn read_meta(conn: &Connection) -> Result<GraphMeta, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT key, value FROM graph_meta")?;
    let mut schema_version = SCHEMA_VERSION;
    let mut last_applied_seq = 0i64;
    let mut status = GraphStatus::Empty;
    let mut last_built_at = None;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (key, value) = row?;
        match key.as_str() {
            "schema_version" => schema_version = value.parse().unwrap_or(SCHEMA_VERSION),
            "last_applied_seq" => last_applied_seq = value.parse().unwrap_or(0),
            "status" => {
                status = match value.as_str() {
                    "ok" => GraphStatus::Ok,
                    "rebuilding" => GraphStatus::Rebuilding,
                    "stale" => GraphStatus::Stale,
                    _ => GraphStatus::Empty,
                }
            }
            "last_built_at" => last_built_at = Some(value),
            _ => {}
        }
    }
    Ok(GraphMeta {
        schema_version,
        last_applied_seq,
        status,
        last_built_at,
    })
}

fn write_meta(tx: &Transaction, key: &str, value: &str) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT INTO graph_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Replay every record from scratch. Use when the schema version bumps or
/// the projection is suspected divergent. Sets `status=rebuilding` for the
/// duration so concurrent readers see honest staleness.
pub fn rebuild(
    conn: &mut Connection,
    records: &[Value],
    now_rfc3339: &str,
) -> Result<GraphMeta, rusqlite::Error> {
    let tx = conn.transaction()?;
    write_meta(&tx, "status", GraphStatus::Rebuilding.as_str())?;
    tx.execute("DELETE FROM edges", [])?;
    tx.execute("DELETE FROM nodes", [])?;
    write_meta(&tx, "last_applied_seq", "0")?;
    tx.commit()?;

    let mut high = 0i64;
    {
        let tx = conn.transaction()?;
        for record in records {
            let seq = record
                .get("local_seq")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if seq > high {
                high = seq;
            }
            apply_record(&tx, record, seq)?;
        }
        write_meta(&tx, "last_applied_seq", &high.to_string())?;
        write_meta(&tx, "schema_version", &SCHEMA_VERSION.to_string())?;
        write_meta(&tx, "status", GraphStatus::Ok.as_str())?;
        write_meta(&tx, "last_built_at", now_rfc3339)?;
        tx.commit()?;
    }
    read_meta(conn)
}

/// Incremental tail: apply records with `local_seq > last_applied_seq`.
/// Returns updated meta. If schema version disagrees, calls `rebuild`.
pub fn catch_up(
    conn: &mut Connection,
    records: &[Value],
    now_rfc3339: &str,
) -> Result<GraphMeta, rusqlite::Error> {
    let meta = read_meta(conn)?;
    if meta.schema_version != SCHEMA_VERSION {
        return rebuild(conn, records, now_rfc3339);
    }
    let cursor = meta.last_applied_seq;
    let mut high = cursor;
    let tx = conn.transaction()?;
    for record in records {
        let seq = record
            .get("local_seq")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if seq <= cursor {
            continue;
        }
        if seq > high {
            high = seq;
        }
        apply_record(&tx, record, seq)?;
    }
    write_meta(&tx, "last_applied_seq", &high.to_string())?;
    if high > 0 {
        write_meta(&tx, "status", GraphStatus::Ok.as_str())?;
        write_meta(&tx, "last_built_at", now_rfc3339)?;
    }
    tx.commit()?;
    read_meta(conn)
}

/// Apply one store-entry record (wrapping an event) to the graph. The event
/// payload sits at `record["event"]`. Unknown event kinds are no-ops —
/// they live in the log but don't add nodes/edges.
fn apply_record(tx: &Transaction, record: &Value, seq: i64) -> Result<(), rusqlite::Error> {
    let event = record.get("event").unwrap_or(record);
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if event_id.is_empty() {
        return Ok(());
    }
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let tool = event.get("tool").and_then(Value::as_str).unwrap_or("unknown");
    let time = event
        .get("time")
        .and_then(Value::as_str)
        .unwrap_or("1970-01-01T00:00:00Z");
    let subject_field = event.get("subject").and_then(Value::as_str).map(str::to_string);

    match kind {
        "task" => {
            upsert_node(tx, &event_id, "task", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            if let Some(owner) = payload.get("owner_tool").and_then(Value::as_str) {
                upsert_peer(tx, owner, time, &event_id, seq)?;
                upsert_edge(tx, &event_id, owner, "owned_by", None, &event_id, seq)?;
            } else {
                upsert_edge(tx, &event_id, tool, "owned_by", None, &event_id, seq)?;
            }
            for dep in payload
                .get("depends_on")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if let Some(dep_id) = dep.as_str() {
                    upsert_edge(tx, &event_id, dep_id, "depends_on", None, &event_id, seq)?;
                }
            }
            for art in payload
                .get("artifacts")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if let Some(art_id) = art.as_str() {
                    upsert_edge(tx, art_id, &event_id, "produced_by", None, &event_id, seq)?;
                }
            }
        }
        "artifact" => {
            upsert_node(tx, &event_id, "artifact", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            upsert_edge(tx, &event_id, tool, "produced_by", None, &event_id, seq)?;
            if let Some(task_id) = payload.get("ref_task_id").and_then(Value::as_str) {
                upsert_edge(tx, &event_id, task_id, "references", None, &event_id, seq)?;
            }
        }
        "decision" => {
            upsert_node(tx, &event_id, "decision", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            upsert_edge(tx, &event_id, tool, "produced_by", None, &event_id, seq)?;
            for sup in payload
                .get("supersedes")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if let Some(sup_id) = sup.as_str() {
                    upsert_edge(tx, &event_id, sup_id, "supersedes", None, &event_id, seq)?;
                }
            }
        }
        "lesson" => {
            upsert_node(tx, &event_id, "lesson", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            upsert_edge(tx, &event_id, tool, "produced_by", None, &event_id, seq)?;
            for src in payload
                .get("source_event_ids")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if let Some(src_id) = src.as_str() {
                    upsert_edge(tx, &event_id, src_id, "derived_from", None, &event_id, seq)?;
                }
            }
        }
        "profile" => {
            // Profile events refresh the Peer node's metadata. The node id is the tool name,
            // not the event id — peer identity is stable across many profile events.
            let profile_tool = payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or(tool);
            upsert_peer(tx, profile_tool, time, &event_id, seq)?;
        }
        "handoff" => {
            // Project handoffs as nodes so "any later non-producer activity"
            // can see them. The producer is the from_tool (the sender), which
            // is the same as the event's `tool` envelope field.
            upsert_node(tx, &event_id, "handoff", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            if let Some(to) = payload.get("to_tool").and_then(Value::as_str) {
                upsert_peer(tx, to, time, &event_id, seq)?;
                upsert_edge(tx, &event_id, to, "addressed_to", None, &event_id, seq)?;
            }
        }
        "ack" => {
            // Project the ack as a node so it counts as later non-producer
            // activity on prior artifacts/handoffs from the original sender.
            upsert_node(tx, &event_id, "ack", subject_field.as_deref(), &payload, time, &event_id, seq, Some(tool))?;
            upsert_peer(tx, tool, time, &event_id, seq)?;
            let verdict = payload.get("verdict").and_then(Value::as_str).unwrap_or("");
            if matches!(verdict, "done" | "passed" | "approved") {
                if let Some(handoff_id) = payload.get("ref_handoff_id").and_then(Value::as_str) {
                    upsert_edge(tx, handoff_id, tool, "reviewed_by", None, &event_id, seq)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn upsert_node(
    tx: &Transaction,
    id: &str,
    kind: &str,
    subject: Option<&str>,
    payload: &Value,
    time: &str,
    source_event_id: &str,
    seq: i64,
    producer_tool: Option<&str>,
) -> Result<(), rusqlite::Error> {
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    tx.execute(
        "INSERT INTO nodes(id, kind, subject, payload_json, created_at, source_event_id, source_seq, producer_tool)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
            kind            = excluded.kind,
            subject         = COALESCE(excluded.subject, nodes.subject),
            payload_json    = excluded.payload_json,
            source_event_id = excluded.source_event_id,
            source_seq      = excluded.source_seq,
            producer_tool   = COALESCE(excluded.producer_tool, nodes.producer_tool)
         WHERE excluded.source_seq >= nodes.source_seq",
        params![id, kind, subject, payload_str, time, source_event_id, seq, producer_tool],
    )?;
    Ok(())
}

fn upsert_peer(
    tx: &Transaction,
    tool: &str,
    time: &str,
    source_event_id: &str,
    seq: i64,
) -> Result<(), rusqlite::Error> {
    if tool.is_empty() || tool == "unknown" {
        return Ok(());
    }
    // A peer "produces" itself; setting producer_tool=tool keeps the column
    // non-null and makes "later activity by different peer" queries simpler.
    tx.execute(
        "INSERT INTO nodes(id, kind, subject, payload_json, created_at, source_event_id, source_seq, producer_tool)
         VALUES (?1, 'peer', ?1, '{}', ?2, ?3, ?4, ?1)
         ON CONFLICT(id) DO UPDATE SET
            source_event_id = excluded.source_event_id,
            source_seq      = excluded.source_seq
         WHERE excluded.source_seq >= nodes.source_seq",
        params![tool, time, source_event_id, seq],
    )?;
    Ok(())
}

fn upsert_edge(
    tx: &Transaction,
    source_id: &str,
    target_id: &str,
    kind: &str,
    payload: Option<&Value>,
    source_event_id: &str,
    seq: i64,
) -> Result<(), rusqlite::Error> {
    let payload_str = payload.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()));
    tx.execute(
        "INSERT OR IGNORE INTO edges(source_id, target_id, kind, payload_json, source_event_id, source_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![source_id, target_id, kind, payload_str, source_event_id, seq],
    )?;
    Ok(())
}

// ───────────────────────── Query helpers ─────────────────────────

/// Return a node by id, if present.
pub fn get_node(conn: &Connection, id: &str) -> Result<Option<Value>, rusqlite::Error> {
    conn.query_row(
        "SELECT id, kind, subject, payload_json, created_at, source_event_id, source_seq
         FROM nodes WHERE id = ?1",
        params![id],
        |row| {
            let payload: String = row.get(3)?;
            let payload_val: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            Ok(json!({
                "id":              row.get::<_, String>(0)?,
                "kind":            row.get::<_, String>(1)?,
                "subject":         row.get::<_, Option<String>>(2)?,
                "payload":         payload_val,
                "created_at":      row.get::<_, String>(4)?,
                "source_event_id": row.get::<_, Option<String>>(5)?,
                "source_seq":      row.get::<_, Option<i64>>(6)?,
            }))
        },
    )
    .optional()
}

/// Subgraph around a node, up to `depth` edges out (both directions).
/// Returns `{root, nodes[], edges[]}`. Suitable for "give the agent the
/// relevant context for this work" without dumping the entire log.
pub fn subgraph_around(
    conn: &Connection,
    root_id: &str,
    depth: u32,
) -> Result<Value, rusqlite::Error> {
    let depth = depth.max(1).min(10) as i64;
    let mut stmt = conn.prepare(
        r#"
        WITH RECURSIVE walk(node_id, distance) AS (
            SELECT ?1 AS node_id, 0 AS distance
            UNION
            SELECT e.target_id, walk.distance + 1
              FROM edges e
              JOIN walk ON walk.node_id = e.source_id
             WHERE walk.distance < ?2
            UNION
            SELECT e.source_id, walk.distance + 1
              FROM edges e
              JOIN walk ON walk.node_id = e.target_id
             WHERE walk.distance < ?2
        )
        SELECT n.id, n.kind, n.subject, n.payload_json, n.created_at,
               n.source_event_id, n.source_seq,
               MIN(walk.distance) AS distance
          FROM walk
          JOIN nodes n ON n.id = walk.node_id
         GROUP BY n.id
         ORDER BY distance, n.created_at DESC
        "#,
    )?;
    let nodes: Vec<Value> = stmt
        .query_map(params![root_id, depth], |row| {
            let payload: String = row.get(3)?;
            let payload_val: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            Ok(json!({
                "id":              row.get::<_, String>(0)?,
                "kind":            row.get::<_, String>(1)?,
                "subject":         row.get::<_, Option<String>>(2)?,
                "payload":         payload_val,
                "created_at":      row.get::<_, String>(4)?,
                "source_event_id": row.get::<_, Option<String>>(5)?,
                "source_seq":      row.get::<_, Option<i64>>(6)?,
                "distance":        row.get::<_, i64>(7)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();

    let node_ids: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let edges = edges_among(conn, &node_ids)?;

    Ok(json!({
        "root":  root_id,
        "depth": depth,
        "nodes": nodes,
        "edges": edges,
    }))
}

/// Edges where both endpoints are in `node_ids`.
fn edges_among(conn: &Connection, node_ids: &[String]) -> Result<Vec<Value>, rusqlite::Error> {
    if node_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Anonymous `?` placeholders bind positionally — 2N placeholders, 2N
    // values via params_from_iter. Reusing `?1..?N` here would conflate
    // the two IN clauses.
    let placeholders = std::iter::repeat("?")
        .take(node_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT source_id, target_id, kind, payload_json, source_event_id, source_seq
           FROM edges
          WHERE source_id IN ({placeholders}) AND target_id IN ({placeholders})
          ORDER BY source_seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&str> = node_ids
        .iter()
        .map(String::as_str)
        .chain(node_ids.iter().map(String::as_str))
        .collect();
    let edges: Vec<Value> = stmt
        .query_map(rusqlite::params_from_iter(bound.iter()), |row| {
            let payload: Option<String> = row.get(3)?;
            let payload_val = payload
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or(Value::Null);
            Ok(json!({
                "source_id":       row.get::<_, String>(0)?,
                "target_id":       row.get::<_, String>(1)?,
                "kind":            row.get::<_, String>(2)?,
                "payload":         payload_val,
                "source_event_id": row.get::<_, String>(4)?,
                "source_seq":      row.get::<_, i64>(5)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(edges)
}

/// Causal chain backwards from `id`: nodes reachable by walking
/// `derived_from`, `supersedes`, `depends_on`, and `references` edges from
/// the target back through ancestors. Useful for "why does this exist?"
pub fn causal_chain(
    conn: &Connection,
    target_id: &str,
    max_depth: u32,
) -> Result<Vec<Value>, rusqlite::Error> {
    let depth = max_depth.max(1).min(20) as i64;
    let mut stmt = conn.prepare(
        r#"
        WITH RECURSIVE chain(node_id, distance) AS (
            SELECT ?1 AS node_id, 0 AS distance
            UNION
            SELECT e.target_id, chain.distance + 1
              FROM edges e
              JOIN chain ON chain.node_id = e.source_id
             WHERE chain.distance < ?2
               AND e.kind IN ('derived_from', 'supersedes', 'depends_on', 'references')
        )
        SELECT n.id, n.kind, n.subject, n.created_at, MIN(chain.distance) AS distance
          FROM chain
          JOIN nodes n ON n.id = chain.node_id
         WHERE chain.distance > 0
         GROUP BY n.id
         ORDER BY distance ASC
        "#,
    )?;
    let chain: Vec<Value> = stmt
        .query_map(params![target_id, depth], |row| {
            Ok(json!({
                "id":         row.get::<_, String>(0)?,
                "kind":       row.get::<_, String>(1)?,
                "subject":    row.get::<_, Option<String>>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "distance":   row.get::<_, i64>(4)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(chain)
}

/// Transitive dependencies of a task: BFS along `depends_on` edges.
pub fn transitive_deps(
    conn: &Connection,
    task_id: &str,
    max_depth: u32,
) -> Result<Vec<Value>, rusqlite::Error> {
    let depth = max_depth.max(1).min(20) as i64;
    let mut stmt = conn.prepare(
        r#"
        WITH RECURSIVE deps(node_id, distance) AS (
            SELECT ?1 AS node_id, 0 AS distance
            UNION
            SELECT e.target_id, deps.distance + 1
              FROM edges e
              JOIN deps ON deps.node_id = e.source_id
             WHERE deps.distance < ?2 AND e.kind = 'depends_on'
        )
        SELECT n.id, n.kind, n.subject, MIN(deps.distance) AS distance
          FROM deps
          JOIN nodes n ON n.id = deps.node_id
         WHERE deps.distance > 0
         GROUP BY n.id
         ORDER BY distance ASC
        "#,
    )?;
    let deps: Vec<Value> = stmt
        .query_map(params![task_id, depth], |row| {
            Ok(json!({
                "id":       row.get::<_, String>(0)?,
                "kind":     row.get::<_, String>(1)?,
                "subject":  row.get::<_, Option<String>>(2)?,
                "distance": row.get::<_, i64>(3)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(deps)
}

/// Artifacts with no later activity by a different peer — the practical
/// "this still needs follow-up" filter.
///
/// An artifact A is consumed iff there exists any node N where:
///   * N.source_seq > A.source_seq, AND
///   * N.producer_tool != A.producer_tool AND N.producer_tool IS NOT NULL,
///     AND
///   * N is not itself a peer (peer upserts run on every event and would
///     mark every artifact consumed instantly otherwise).
///
/// This is broader than "ack with verdict=done" but matches real-team
/// behavior: when a different peer has acted after an artifact, that peer
/// has effectively engaged with the work; recommendations should move on.
pub fn unconsumed_artifacts(conn: &Connection, limit: u32) -> Result<Vec<Value>, rusqlite::Error> {
    let lim = limit.max(1).min(200) as i64;
    let mut stmt = conn.prepare(
        r#"
        SELECT n.id, n.subject, n.created_at, n.source_event_id, n.producer_tool
          FROM nodes n
         WHERE n.kind = 'artifact'
           AND n.producer_tool IS NOT NULL
           AND NOT EXISTS (
             SELECT 1
               FROM nodes other
              WHERE other.kind != 'peer'
                AND other.source_seq > n.source_seq
                AND other.producer_tool IS NOT NULL
                AND other.producer_tool != n.producer_tool
           )
         ORDER BY n.source_seq DESC
         LIMIT ?1
        "#,
    )?;
    let rows: Vec<Value> = stmt
        .query_map(params![lim], |row| {
            Ok(json!({
                "id":              row.get::<_, String>(0)?,
                "subject":         row.get::<_, Option<String>>(1)?,
                "created_at":      row.get::<_, String>(2)?,
                "source_event_id": row.get::<_, Option<String>>(3)?,
                "producer_tool":   row.get::<_, Option<String>>(4)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Helper exposed for CLI surfaces — returns counts by node kind + edge kind.
pub fn stats(conn: &Connection) -> Result<Value, rusqlite::Error> {
    let mut node_counts = serde_json::Map::new();
    let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY kind")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (kind, count) = row?;
        node_counts.insert(kind, json!(count));
    }
    let mut edge_counts = serde_json::Map::new();
    let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM edges GROUP BY kind")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (kind, count) = row?;
        edge_counts.insert(kind, json!(count));
    }
    Ok(json!({ "nodes": node_counts, "edges": edge_counts }))
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "rally-graph-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn task_event(id: &str, subject: &str, depends_on: &[&str]) -> Value {
        json!({
            "local_seq": 1,
            "event": {
                "id": id,
                "kind": "task",
                "tool": "claude_code",
                "time": "2026-05-27T00:00:00.000Z",
                "subject": subject,
                "payload": {
                    "subject": subject,
                    "status": "active",
                    "depends_on": depends_on,
                }
            }
        })
    }

    fn artifact_event(id: &str, subject: &str, ref_task: Option<&str>) -> Value {
        json!({
            "local_seq": 2,
            "event": {
                "id": id,
                "kind": "artifact",
                "tool": "pi",
                "time": "2026-05-27T00:01:00.000Z",
                "subject": subject,
                "payload": {
                    "subject": subject,
                    "artifact_kind": "code",
                    "ref_task_id": ref_task,
                }
            }
        })
    }

    fn decision_event(id: &str, subject: &str, supersedes: &[&str], seq: i64) -> Value {
        json!({
            "local_seq": seq,
            "event": {
                "id": id,
                "kind": "decision",
                "tool": "pi",
                "time": "2026-05-27T00:02:00.000Z",
                "subject": subject,
                "payload": {
                    "subject": subject,
                    "status": "binding",
                    "supersedes": supersedes,
                }
            }
        })
    }

    #[test]
    fn init_creates_schema_and_seeds_meta() {
        // intent: rally graph init must be idempotent and leave the DB in a queryable state.
        let dir = temp_dir("init");
        let conn = init(&dir, "2026-05-27T00:00:00Z").unwrap();
        let meta = read_meta(&conn).unwrap();
        assert_eq!(meta.schema_version, SCHEMA_VERSION);
        assert_eq!(meta.last_applied_seq, 0);
        assert_eq!(meta.status, GraphStatus::Empty);

        // re-init is a no-op.
        let conn2 = init(&dir, "2026-05-27T01:00:00Z").unwrap();
        let meta2 = read_meta(&conn2).unwrap();
        assert_eq!(meta2.schema_version, SCHEMA_VERSION);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn catch_up_projects_task_events_with_dependencies() {
        // intent: a task event with depends_on must produce both the Task node and the depends_on edge.
        let dir = temp_dir("task-deps");
        let mut conn = init(&dir, "now").unwrap();
        let records = vec![
            task_event("evt_a", "build the thing", &[]),
            task_event("evt_b", "ship the thing", &["evt_a"]),
        ];
        let meta = catch_up(&mut conn, &records, "now").unwrap();
        assert_eq!(meta.status, GraphStatus::Ok);
        assert!(meta.last_applied_seq >= 1);

        let node = get_node(&conn, "evt_a").unwrap().unwrap();
        assert_eq!(node["kind"], "task");
        assert_eq!(node["subject"], "build the thing");

        let deps = transitive_deps(&conn, "evt_b", 5).unwrap();
        assert!(deps.iter().any(|d| d["id"] == "evt_a"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unconsumed_artifacts_drops_after_later_non_producer_activity() {
        // intent: once a peer other than the producer has posted any event
        // with a greater source_seq, the producer's earlier artifacts must
        // stop surfacing as unconsumed. This is the bug behind stale
        // "rally next" recommendations that kept resurfacing reviewed work.
        let dir = temp_dir("unconsumed");
        let mut conn = init(&dir, "now").unwrap();

        // pi posts two artifacts. With no other peer activity, both are unconsumed.
        let mut records = vec![
            artifact_event("art_one", "first cut", None),
            artifact_event("art_two", "second cut", None),
        ];
        // Bump the seqs so they're distinct.
        records[0]["local_seq"] = json!(1);
        records[1]["local_seq"] = json!(2);
        catch_up(&mut conn, &records, "now").unwrap();
        let baseline: Vec<String> = unconsumed_artifacts(&conn, 10)
            .unwrap()
            .into_iter()
            .filter_map(|n| n["id"].as_str().map(str::to_string))
            .collect();
        assert!(baseline.contains(&"art_one".to_string()));
        assert!(baseline.contains(&"art_two".to_string()));

        // claude_code posts a follow-up handoff. Both prior artifacts (by pi)
        // are now consumed — a different peer has engaged.
        let followup = json!({
            "local_seq": 3,
            "event": {
                "id": "evt_followup",
                "kind": "handoff",
                "tool": "claude_code",
                "time": "2026-05-27T00:03:00.000Z",
                "subject": "follow-up review",
                "payload": {
                    "subject": "follow-up review",
                    "to_tool": "pi"
                }
            }
        });
        catch_up(&mut conn, &[followup], "now").unwrap();
        let after: Vec<String> = unconsumed_artifacts(&conn, 10)
            .unwrap()
            .into_iter()
            .filter_map(|n| n["id"].as_str().map(str::to_string))
            .collect();
        assert!(
            !after.contains(&"art_one".to_string()),
            "art_one should have dropped from unconsumed after claude_code activity"
        );
        assert!(
            !after.contains(&"art_two".to_string()),
            "art_two should have dropped from unconsumed after claude_code activity"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn causal_chain_walks_supersedes_and_derived_from() {
        // intent: "why does decision D3 exist?" must reach D1 via the supersedes edge chain.
        let dir = temp_dir("causal");
        let mut conn = init(&dir, "now").unwrap();
        let records = vec![
            decision_event("d1", "use X", &[], 1),
            decision_event("d2", "use Y instead", &["d1"], 2),
            decision_event("d3", "actually use Z", &["d2"], 3),
        ];
        catch_up(&mut conn, &records, "now").unwrap();
        let chain = causal_chain(&conn, "d3", 5).unwrap();
        let ids: Vec<&str> = chain.iter().filter_map(|n| n["id"].as_str()).collect();
        assert!(ids.contains(&"d2"));
        assert!(ids.contains(&"d1"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn catch_up_is_incremental_skipping_already_applied_seqs() {
        // intent: re-running catch_up on the same records must not duplicate nodes or advance the cursor backwards.
        let dir = temp_dir("incremental");
        let mut conn = init(&dir, "now").unwrap();
        let records = vec![task_event("evt_x", "do the work", &[])];
        let meta1 = catch_up(&mut conn, &records, "now").unwrap();
        let meta2 = catch_up(&mut conn, &records, "now").unwrap();
        assert_eq!(meta1.last_applied_seq, meta2.last_applied_seq);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes WHERE id = 'evt_x'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn subgraph_returns_neighborhood_with_edges() {
        // intent: subgraph_around must return both the root and its 1-hop
        // neighbors plus the edges connecting them, with no parameter-binding
        // bugs in the multi-IN-clause query.
        let dir = temp_dir("subgraph");
        let mut conn = init(&dir, "now").unwrap();
        let records = vec![
            task_event("evt_root", "root task", &[]),
            task_event("evt_dep", "dependency", &["evt_root"]),
            artifact_event("evt_art", "artifact for root", Some("evt_root")),
        ];
        catch_up(&mut conn, &records, "now").unwrap();
        let sub = subgraph_around(&conn, "evt_root", 1).unwrap();
        let nodes = sub["nodes"].as_array().unwrap();
        let edges = sub["edges"].as_array().unwrap();
        assert!(nodes.iter().any(|n| n["id"] == "evt_root"));
        // 1-hop neighbors of evt_root: evt_dep (via depends_on) and evt_art (via references).
        let ids: Vec<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
        assert!(ids.contains(&"evt_dep"));
        assert!(ids.contains(&"evt_art"));
        // At least one edge must appear; the prior bug returned zero with a binding error.
        assert!(!edges.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rebuild_clears_and_replays_when_schema_changes() {
        // intent: schema version bump must trigger a full rebuild, not silently drift.
        let dir = temp_dir("rebuild");
        let mut conn = init(&dir, "now").unwrap();
        let records = vec![task_event("evt_q", "old work", &[])];
        catch_up(&mut conn, &records, "now").unwrap();

        // Simulate a schema bump by writing an old version into meta.
        conn.execute(
            "UPDATE graph_meta SET value = '0' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();

        let new_records = vec![task_event("evt_r", "new work", &[])];
        let meta = catch_up(&mut conn, &new_records, "now").unwrap();
        assert_eq!(meta.schema_version, SCHEMA_VERSION);
        // The old node must be gone — rebuild from new_records only.
        assert!(get_node(&conn, "evt_q").unwrap().is_none());
        assert!(get_node(&conn, "evt_r").unwrap().is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn init_drops_stale_tables_when_schema_version_changed_on_disk() {
        // intent: opening a DB whose schema_version differs from the
        // compiled SCHEMA_VERSION must wipe the prior tables so columns
        // added in the new version don't fail at apply_schema time. The
        // alternative is silent SQL errors like "no such column".
        let dir = temp_dir("schema-drift");
        {
            let conn = init(&dir, "now").unwrap();
            // Force an old schema version onto the existing DB. The next
            // open() must notice and rebuild.
            conn.execute(
                "UPDATE graph_meta SET value = '0' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        // Open again — must succeed even though the underlying nodes table
        // was created without the new columns. The drop+recreate path is
        // what makes this work.
        let conn = init(&dir, "now").unwrap();
        let meta = read_meta(&conn).unwrap();
        assert_eq!(meta.schema_version, SCHEMA_VERSION);
        // Inserting a task event into the fresh schema must succeed.
        let records = vec![task_event("evt_after_migration", "fresh", &[])];
        let mut conn = conn;
        catch_up(&mut conn, &records, "now").unwrap();
        assert!(get_node(&conn, "evt_after_migration").unwrap().is_some());
        std::fs::remove_dir_all(dir).ok();
    }
}
