// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-room snapshot fetching: spawns the `rally` binary against a room's
//! working directory, reads the newest ledger log file for a recent-events
//! feed, and derives a `health` verdict a troubleshooting operator can act
//! on without reading raw JSON.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::registry;

const OUTER_TIMEOUT: Duration = Duration::from_secs(25);
const STALE_LOG_THRESHOLD: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentCounts {
    pub working: usize,
    pub idle: usize,
    pub blocked: usize,
    pub done: usize,
    pub stale: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentRow {
    pub tool: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_after: Option<String>,
    pub last_seen_ts: String,
    pub last_seen_seq: i64,
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct HandoffRow {
    pub subject: String,
    pub from_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub kind: String,
    pub tool: String,
    pub subject: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomSnapshot {
    pub id: String,
    pub path: String,
    pub name: String,
    pub agents: Vec<AgentRow>,
    pub agent_counts: AgentCounts,
    pub handoffs: Vec<HandoffRow>,
    pub events: Vec<EventRow>,
    pub claims: usize,
    pub max_seq: i64,
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_reason: Option<String>,
    pub fetched_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomSummary {
    pub id: String,
    pub path: String,
    pub name: String,
    pub agent_counts: AgentCounts,
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_reason: Option<String>,
}

impl From<&RoomSnapshot> for RoomSummary {
    fn from(s: &RoomSnapshot) -> Self {
        RoomSummary {
            id: s.id.clone(),
            path: s.path.clone(),
            name: s.name.clone(),
            agent_counts: s.agent_counts.clone(),
            health: s.health.clone(),
            health_reason: s.health_reason.clone(),
        }
    }
}

struct Inner {
    agents: Vec<AgentRow>,
    handoffs: Vec<HandoffRow>,
    events: Vec<EventRow>,
    claims: usize,
    max_seq: i64,
    health: &'static str,
    health_reason: Option<String>,
}

/// Fetch a full snapshot for one room, bounded by a 25s outer timeout.
pub async fn fetch_snapshot(room_path: &Path, rally_bin: &str) -> RoomSnapshot {
    let path_str = room_path.to_string_lossy().to_string();
    let id = registry::short_id(&path_str);
    let name = room_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path_str.clone());

    let inner = match tokio::time::timeout(OUTER_TIMEOUT, fetch_inner(room_path, rally_bin)).await {
        Ok(inner) => inner,
        Err(_) => Inner {
            agents: vec![],
            handoffs: vec![],
            events: vec![],
            claims: 0,
            max_seq: 0,
            health: "error",
            health_reason: Some(format!(
                "timed out after {}s fetching room snapshot",
                OUTER_TIMEOUT.as_secs()
            )),
        },
    };

    let agent_counts = counts_from_agents(&inner.agents);

    RoomSnapshot {
        id,
        path: path_str,
        name,
        agents: inner.agents,
        agent_counts,
        handoffs: inner.handoffs,
        events: inner.events,
        claims: inner.claims,
        max_seq: inner.max_seq,
        health: inner.health.to_string(),
        health_reason: inner.health_reason,
        fetched_at: chrono::Utc::now().to_rfc3339(),
    }
}

async fn fetch_inner(room_path: &Path, rally_bin: &str) -> Inner {
    if !room_path.join(".rally").is_dir() {
        return Inner {
            agents: vec![],
            handoffs: vec![],
            events: vec![],
            claims: 0,
            max_seq: 0,
            health: "error",
            health_reason: Some("no .rally directory found at this path".to_string()),
        };
    }

    let status_res = run_rally(
        room_path,
        rally_bin,
        &["status", "read", "--json", "--timeout-ms", "20000"],
    )
    .await;
    let room_res = run_rally(
        room_path,
        rally_bin,
        &["room", "--json", "--timeout-ms", "20000"],
    )
    .await;
    let events = read_last_events(room_path).await;

    // Spawn/parse failures take priority over everything else — we have no
    // real data to reason about health from.
    let mut errors = Vec::new();
    if let Err(e) = &status_res {
        errors.push(e.clone());
    }
    if let Err(e) = &room_res {
        errors.push(e.clone());
    }
    if !errors.is_empty() {
        return Inner {
            agents: vec![],
            handoffs: vec![],
            events,
            claims: 0,
            max_seq: 0,
            health: "error",
            health_reason: Some(errors.join("; ")),
        };
    }

    let status_value = status_res.expect("checked Ok above");
    let room_value = room_res.expect("checked Ok above");

    let agents = parse_agents(&status_value);
    let (handoffs, claims, max_seq) = parse_room(&room_value);

    if is_stub_response(&status_value) || is_stub_response(&room_value) {
        return Inner {
            agents,
            handoffs,
            events,
            claims,
            max_seq,
            health: "degraded",
            health_reason: Some("watchdog stub response — writes may be dropping".to_string()),
        };
    }

    let has_live_agent = agents.iter().any(|a| !a.stale);
    let health = if log_is_stale(room_path) && !has_live_agent {
        "stale"
    } else {
        "ok"
    };

    Inner {
        agents,
        handoffs,
        events,
        claims,
        max_seq,
        health,
        health_reason: None,
    }
}

/// Spawn `rally_bin <args>` with `cwd = room_path` and parse stdout as JSON.
async fn run_rally(room_path: &Path, rally_bin: &str, args: &[&str]) -> Result<Value, String> {
    let output = tokio::process::Command::new(rally_bin)
        .args(args)
        .current_dir(room_path)
        // Reap the child if the outer per-room timeout drops this future —
        // otherwise an abandoned rally process lingers until its own watchdog.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("failed to spawn `{rally_bin}`: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Value>(&stdout).map_err(|e| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let joined_args = args.join(" ");
        format!(
            "failed to parse JSON from `{rally_bin} {joined_args}`: {e}; stderr={}",
            stderr.trim()
        )
    })
}

/// Detect the degraded "watchdog stub" shape: `{"ok":true,"product":"rally"}`
/// with no `data` key at all (a healthy response always carries `data`).
fn is_stub_response(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    obj.len() <= 2
        && obj.get("ok").and_then(Value::as_bool) == Some(true)
        && obj.get("product").and_then(Value::as_str) == Some("rally")
        && !obj.contains_key("data")
}

fn parse_agents(value: &Value) -> Vec<AgentRow> {
    let Some(states) = value
        .pointer("/data/status_read/states")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    states
        .iter()
        .map(|s| AgentRow {
            tool: str_field(s, "tool").unwrap_or_else(|| "unknown".to_string()),
            state: str_field(s, "state").unwrap_or_else(|| "unknown".to_string()),
            file: str_field(s, "file"),
            intent: str_field(s, "intent"),
            blocked_ref: str_field(s, "ref"),
            committed_sha: str_field(s, "committed_sha"),
            worktree_branch: str_field(s, "worktree_branch"),
            wake_after: str_field(s, "wake_after"),
            last_seen_ts: str_field(s, "last_seen_ts").unwrap_or_default(),
            last_seen_seq: s.get("last_seen_seq").and_then(Value::as_i64).unwrap_or(0),
            stale: s.get("stale").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

fn parse_room(value: &Value) -> (Vec<HandoffRow>, usize, i64) {
    let Some(room) = value.pointer("/data/room") else {
        return (Vec::new(), 0, 0);
    };
    let max_seq = room.get("max_seq").and_then(Value::as_i64).unwrap_or(0);
    let claims = room
        .get("active_claims")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let handoffs = room
        .get("open_handoffs")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|h| HandoffRow {
                    subject: str_field(h, "subject").unwrap_or_default(),
                    from_session_id: str_field(h, "from_session_id").unwrap_or_default(),
                    target: str_field(h, "target"),
                    created_at: str_field(h, "created_at").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    (handoffs, claims, max_seq)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn counts_from_agents(agents: &[AgentRow]) -> AgentCounts {
    let mut counts = AgentCounts::default();
    for agent in agents {
        if agent.stale {
            counts.stale += 1;
            continue;
        }
        match agent.state.as_str() {
            "working" => counts.working += 1,
            "blocked" => counts.blocked += 1,
            "done" => counts.done += 1,
            // "idle" and any forward-compat "unknown" state both bucket as
            // idle for the summary count — neither is actionable the way
            // working/blocked/done are.
            _ => counts.idle += 1,
        }
    }
    counts
}

/// Newest `.rally/log/*.jsonl` file by mtime, if any.
fn newest_log_file(room_path: &Path) -> Option<PathBuf> {
    let log_dir = room_path.join(".rally").join("log");
    let entries = std::fs::read_dir(&log_dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, t)| modified > *t) {
            best = Some((path, modified));
        }
    }
    best.map(|(path, _)| path)
}

/// True if the newest log file is missing or older than 30 minutes.
fn log_is_stale(room_path: &Path) -> bool {
    let Some(log_path) = newest_log_file(room_path) else {
        return true;
    };
    let Ok(metadata) = std::fs::metadata(&log_path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified
        .elapsed()
        .map(|age| age > STALE_LOG_THRESHOLD)
        .unwrap_or(true)
}

/// Last 40 lines of the newest log file, reverse-chronological, parsed
/// defensively (each line skipped if it isn't valid JSON at all).
async fn read_last_events(room_path: &Path) -> Vec<EventRow> {
    let Some(log_path) = newest_log_file(room_path) else {
        return Vec::new();
    };
    let Ok(content) = tokio::fs::read_to_string(&log_path).await else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(40);
    let mut events: Vec<EventRow> = lines[start..]
        .iter()
        .filter_map(|line| parse_event_line(line))
        .collect();
    events.reverse();
    events
}

/// Dig seq/kind/tool/subject/created_at out of a ledger line regardless of
/// whether they live at the top level or nested under `payload` — the
/// ledger schema has drifted across rally versions and both shapes exist in
/// the wild.
fn parse_event_line(raw: &str) -> Option<EventRow> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let seq = v
        .get("seq")
        .and_then(Value::as_i64)
        .or_else(|| v.pointer("/payload/seq").and_then(Value::as_i64))
        .unwrap_or(0);
    let kind = v
        .get("event_type")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/payload/kind").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_string();
    let tool = v
        .pointer("/payload/tool")
        .and_then(Value::as_str)
        .or_else(|| v.get("tool").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let subject = v
        .pointer("/payload/subject")
        .and_then(Value::as_str)
        .or_else(|| v.get("subject").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let created_at = v
        .get("occurred_at")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/payload/created_at").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    Some(EventRow {
        seq,
        kind,
        tool,
        subject,
        created_at,
    })
}
