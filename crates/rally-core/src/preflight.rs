// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use crate::query::{ActiveBlocker, ActiveClaim, ClaimConflict, RecentChange};
use crate::store::ChannelStore;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

const PRESENCE_DIR: &str = "rally/presence";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightOptions {
    pub tool: String,
    pub model: Option<String>,
    pub session_id: String,
    pub start_ping: bool,
    pub stale_after_seconds: i64,
    pub recent_limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreflightEnvelope {
    pub ok: bool,
    pub command: String,
    pub schema: String,
    pub channel: String,
    pub tool: String,
    pub session_id: String,
    pub coordination_status: String,
    pub routing: PreflightRouting,
    pub pending_acks_for_me: Vec<crate::query::PendingHandoff>,
    pub active_peers: Vec<ActivePeer>,
    pub active_claims: Vec<ActiveClaim>,
    pub active_blockers: Vec<ActiveBlocker>,
    pub claim_conflicts: Vec<ClaimConflict>,
    pub recent_changes: Vec<RecentChange>,
    pub presence_written: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreflightRouting {
    pub action: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivePeer {
    pub tool: String,
    pub model: Option<String>,
    pub session_id: String,
    pub observed_at: String,
    pub age_seconds: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PresenceRecord {
    schema: String,
    tool: String,
    model: Option<String>,
    session_id: String,
    observed_at: String,
}

pub fn run_preflight(
    store: &ChannelStore,
    options: &PreflightOptions,
) -> Result<PreflightEnvelope, CoreError> {
    let presence_written = if options.start_ping {
        Some(write_presence(store.channel_dir(), options)?)
    } else {
        None
    };

    let now = DateTime::<Utc>::from(SystemTime::now());
    let now_epoch = now.timestamp() as f64;
    let records = store.load_records()?;
    let active_peers = active_peers(
        store.channel_dir(),
        &options.tool,
        &options.session_id,
        options.stale_after_seconds,
        now,
    )?;

    // Graph is the source of truth. Surfacing graph errors here lets
    // callers detect a corrupt projection rather than serve stale data.
    let now_rfc = now.to_rfc3339();
    let mut conn = crate::graph::init(store.channel_dir(), &now_rfc).map_err(|err| {
        crate::CoreError::Io(std::io::Error::other(format!("open graph: {err}")))
    })?;
    crate::graph::catch_up(&mut conn, &records, &now_rfc).map_err(|err| {
        crate::CoreError::Io(std::io::Error::other(format!("graph catch_up: {err}")))
    })?;
    let pending_acks_for_me = crate::graph::pending_handoffs_typed(&conn, Some(&options.tool), now_epoch)
        .map_err(|err| crate::CoreError::Io(std::io::Error::other(format!("handoffs: {err}"))))?;
    let active_claims = crate::graph::active_claims_typed(&conn, None, now_epoch)
        .map_err(|err| crate::CoreError::Io(std::io::Error::other(format!("claims: {err}"))))?;
    let active_blockers = crate::graph::active_blockers_typed(&conn, None, now_epoch)
        .map_err(|err| crate::CoreError::Io(std::io::Error::other(format!("blockers: {err}"))))?;
    let claim_conflicts = crate::graph::claim_conflicts_typed(&conn)
        .map_err(|err| crate::CoreError::Io(std::io::Error::other(format!("conflicts: {err}"))))?;
    let recent_changes = crate::graph::recent_changes_typed(&conn, options.recent_limit as u32, now_epoch)
        .map_err(|err| crate::CoreError::Io(std::io::Error::other(format!("recent: {err}"))))?;

    let routing = if !pending_acks_for_me.is_empty() {
        PreflightRouting {
            action: "join_active".to_string(),
            reason: "pending acknowledgements require this tool".to_string(),
        }
    } else if !active_peers.is_empty() {
        PreflightRouting {
            action: "join_active".to_string(),
            reason: "active peers are present in this channel".to_string(),
        }
    } else {
        PreflightRouting {
            action: "proceed_solo".to_string(),
            reason: "no pending acknowledgements or active peers".to_string(),
        }
    };
    let coordination_status = if routing.action == "join_active" {
        "active"
    } else {
        "idle"
    };

    Ok(PreflightEnvelope {
        ok: true,
        command: "preflight".to_string(),
        schema: "agent-rally.command.preflight.v1".to_string(),
        channel: store.channel_dir().display().to_string(),
        tool: options.tool.clone(),
        session_id: options.session_id.clone(),
        coordination_status: coordination_status.to_string(),
        routing,
        pending_acks_for_me,
        active_peers,
        active_claims,
        active_blockers,
        claim_conflicts,
        recent_changes,
        presence_written,
    })
}

fn write_presence(channel_dir: &Path, options: &PreflightOptions) -> Result<String, CoreError> {
    let dir = channel_dir.join(PRESENCE_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}-{}.json",
        safe_component(&options.tool),
        safe_component(&options.session_id)
    ));
    let tmp_path = path.with_extension("json.tmp");
    let record = PresenceRecord {
        schema: "agent-rally.presence.v1".to_string(),
        tool: options.tool.clone(),
        model: options.model.clone(),
        session_id: options.session_id.clone(),
        observed_at: DateTime::<Utc>::from(SystemTime::now())
            .to_rfc3339_opts(SecondsFormat::Millis, true),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp_path)?;
    serde_json::to_writer(&mut file, &record)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    fs::rename(&tmp_path, &path)?;
    Ok(path.display().to_string())
}

fn active_peers(
    channel_dir: &Path,
    own_tool: &str,
    own_session_id: &str,
    stale_after_seconds: i64,
    now: DateTime<Utc>,
) -> Result<Vec<ActivePeer>, CoreError> {
    let dir = channel_dir.join(PRESENCE_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_reader::<_, PresenceRecord>(file) else {
            continue;
        };
        if record.tool == own_tool && record.session_id == own_session_id {
            continue;
        }
        let Ok(observed_at) = DateTime::parse_from_rfc3339(&record.observed_at) else {
            continue;
        };
        let age_seconds = now
            .signed_duration_since(observed_at.with_timezone(&Utc))
            .num_seconds()
            .max(0);
        if age_seconds > stale_after_seconds {
            continue;
        }
        out.push(ActivePeer {
            tool: record.tool,
            model: record.model,
            session_id: record.session_id,
            observed_at: record.observed_at,
            age_seconds,
        });
    }
    out.sort_by(|left, right| {
        left.tool
            .cmp(&right.tool)
            .then(left.session_id.cmp(&right.session_id))
    });
    Ok(out)
}

fn safe_component(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe.to_string()
    }
}
