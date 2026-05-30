use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};
use crate::store::{Fact, RoomStore};
use crate::{now_string, repo_root};

const ROOM_INDEX_SCHEMA: &str = "agent-rally.room-index.v1";

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(crate) struct KnownRoom {
    #[serde(default = "room_index_schema")]
    pub(crate) schema: String,
    pub(crate) repo_root: PathBuf,
    pub(crate) display_name: String,
    pub(crate) facts_db: PathBuf,
    pub(crate) last_seen_seq: i64,
    pub(crate) last_seen_at: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DiscoveryWarning {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) path: Option<PathBuf>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LocatedRecord {
    pub(crate) source: String,
    pub(crate) repo_root: Option<PathBuf>,
    pub(crate) display_name: Option<String>,
    pub(crate) facts_db: Option<PathBuf>,
    pub(crate) legacy_channel: Option<PathBuf>,
    pub(crate) local_seq: Option<i64>,
    pub(crate) fact: Option<Fact>,
    pub(crate) record: Option<Value>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LocateData {
    pub(crate) event_id: String,
    pub(crate) located: Option<LocatedRecord>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RecentRow {
    pub(crate) source: String,
    pub(crate) repo_root: Option<PathBuf>,
    pub(crate) display_name: Option<String>,
    pub(crate) facts_db: Option<PathBuf>,
    pub(crate) legacy_channel: Option<PathBuf>,
    pub(crate) local_seq: Option<i64>,
    pub(crate) seq: Option<i64>,
    pub(crate) created_at: Option<String>,
    pub(crate) fact: Option<Fact>,
    pub(crate) record: Option<Value>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RecentData {
    pub(crate) all: bool,
    pub(crate) include_legacy: bool,
    pub(crate) limit: i64,
    pub(crate) rows: Vec<RecentRow>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// Per-repo entry in the global status rollup.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RepoStatus {
    /// Repo root path.
    pub(crate) repo: PathBuf,
    /// Room display name (basename of repo root).
    pub(crate) room: String,
    /// Tool holding `role:lead`, if any.
    pub(crate) lead: Option<String>,
    /// Count of active (unreleased, unresolved) claims.
    pub(crate) open_claims: usize,
    /// Timestamp of the highest-seq fact in this room, or null if empty.
    pub(crate) last_activity_ts: Option<String>,
    /// Tools whose most-recent fact is within the active threshold.
    pub(crate) alive_agents: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct GlobalStatusData {
    pub(crate) repos: Vec<RepoStatus>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

#[derive(Default, Deserialize, Serialize)]
struct RoomIndex {
    #[serde(default = "room_index_schema")]
    schema: String,
    #[serde(default)]
    rooms: Vec<KnownRoom>,
}

fn room_index_schema() -> String {
    ROOM_INDEX_SCHEMA.to_string()
}

pub(crate) fn refresh_room_index(
    repo_root: &Path,
    facts_db: &Path,
    last_seen_seq: i64,
) -> Result<()> {
    let Some(path) = room_index_path() else {
        return Ok(());
    };
    let mut index = read_room_index_at(&path)?;
    let repo_root = absolute_path(repo_root);
    let facts_db = absolute_path(facts_db);
    let display_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| repo_root.display().to_string());
    let now = now_string();
    let mut updated = false;
    for room in &mut index.rooms {
        if room.repo_root == repo_root {
            room.display_name = display_name.clone();
            room.facts_db = facts_db.clone();
            room.last_seen_seq = room.last_seen_seq.max(last_seen_seq);
            room.last_seen_at = now.clone();
            updated = true;
            break;
        }
    }
    if !updated {
        index.rooms.push(KnownRoom {
            schema: ROOM_INDEX_SCHEMA.to_string(),
            repo_root,
            display_name,
            facts_db,
            last_seen_seq,
            last_seen_at: now,
        });
    }
    index
        .rooms
        .sort_by(|left, right| left.repo_root.cmp(&right.repo_root));
    write_room_index_at(&path, &index)
}

pub(crate) fn locate(event_id: &str, include_legacy: bool) -> Result<LocateData> {
    let current = repo_root()?;
    let mut warnings = Vec::new();
    let rooms = known_rooms_with_current(&current, &mut warnings)?;
    for room in rooms {
        if let Some(record) = locate_in_room(&room, event_id, &mut warnings)? {
            return Ok(LocateData {
                event_id: event_id.to_string(),
                located: Some(record),
                warnings,
            });
        }
    }
    if include_legacy {
        if let Some(record) = locate_in_legacy(event_id, &mut warnings)? {
            return Ok(LocateData {
                event_id: event_id.to_string(),
                located: Some(record),
                warnings,
            });
        }
    } else {
        warn_if_legacy_exists(&mut warnings);
    }
    Ok(LocateData {
        event_id: event_id.to_string(),
        located: None,
        warnings,
    })
}

/// Derive a high-level rollup across all known repo rooms.
///
/// Walks the global room index at `~/.agent-rally-point/rooms/v1/index.json`
/// and, for each registered room, opens its ledger read-only and projects a
/// `RepoStatus`. Never appends any fact to any ledger.
pub(crate) fn status_global() -> Result<GlobalStatusData> {
    let mut warnings = Vec::new();
    let mut repos = Vec::new();

    let Some(index_path) = room_index_path() else {
        warnings.push(warning(
            "global_index_disabled",
            "RALLY_NO_GLOBAL_INDEX is set; cross-repo status unavailable",
            None,
        ));
        return Ok(GlobalStatusData { repos, warnings });
    };

    let index = match read_room_index_at(&index_path) {
        Ok(idx) => idx,
        Err(err) => {
            warnings.push(warning(
                "room_index_unreadable",
                format!("failed to read room index: {err}"),
                Some(index_path),
            ));
            return Ok(GlobalStatusData { repos, warnings });
        }
    };

    for room_entry in &index.rooms {
        let Some(store) = open_indexed_room(room_entry, &mut warnings)? else {
            continue;
        };
        let snapshot = match store.snapshot() {
            Ok(s) => s,
            Err(err) => {
                warnings.push(warning(
                    "room_snapshot_failed",
                    format!(
                        "failed to snapshot {}: {err}",
                        room_entry.repo_root.display()
                    ),
                    Some(room_entry.facts_db.clone()),
                ));
                continue;
            }
        };

        // last_activity_ts = created_at of the highest-seq fact.
        let last_activity_ts = {
            let facts = match store.facts() {
                Ok(f) => f,
                Err(_) => Vec::new(),
            };
            facts
                .into_iter()
                .max_by_key(|f| f.seq)
                .map(|f| f.created_at)
        };

        let alive_agents = snapshot
            .squads
            .iter()
            .filter(|s| s.status == "active")
            .map(|s| s.tool.clone())
            .collect::<Vec<_>>();

        repos.push(RepoStatus {
            repo: room_entry.repo_root.clone(),
            room: room_entry.display_name.clone(),
            lead: snapshot.lead.clone(),
            open_claims: snapshot.active_claims.len(),
            last_activity_ts,
            alive_agents,
        });
    }

    Ok(GlobalStatusData { repos, warnings })
}

pub(crate) fn recent(all: bool, include_legacy: bool, limit: i64) -> Result<RecentData> {
    let current = repo_root()?;
    let mut warnings = Vec::new();
    let mut rows = Vec::new();
    let rooms = if all {
        known_rooms_with_current(&current, &mut warnings)?
    } else {
        vec![known_room_for_current(&current)?]
    };
    for room in rooms {
        rows.extend(recent_in_room(&room, &mut warnings)?);
    }
    if include_legacy {
        rows.extend(recent_legacy(&mut warnings)?);
    } else {
        warn_if_legacy_exists(&mut warnings);
    }
    rows.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.seq.cmp(&left.seq))
            .then_with(|| right.local_seq.cmp(&left.local_seq))
    });
    rows.truncate(limit.max(0) as usize);
    Ok(RecentData {
        all,
        include_legacy,
        limit,
        rows,
        warnings,
    })
}

fn known_rooms_with_current(
    current: &Path,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Vec<KnownRoom>> {
    let current_room = known_room_for_current(current)?;
    let mut rooms = BTreeMap::new();
    rooms.insert(current_room.repo_root.clone(), current_room);
    if let Some(path) = room_index_path() {
        match read_room_index_at(&path) {
            Ok(index) => {
                for room in index.rooms {
                    rooms.entry(room.repo_root.clone()).or_insert(room);
                }
            }
            Err(err) => warnings.push(warning(
                "room_index_unreadable",
                format!("failed to read room index: {err}"),
                Some(path),
            )),
        }
    }
    Ok(rooms.into_values().collect())
}

fn known_room_for_current(current: &Path) -> Result<KnownRoom> {
    let _ = RoomStore::open_at(current.to_path_buf())?;
    let facts_db = current.join(".rally/facts.db");
    Ok(KnownRoom {
        schema: ROOM_INDEX_SCHEMA.to_string(),
        repo_root: absolute_path(current),
        display_name: current
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| current.display().to_string()),
        facts_db: absolute_path(&facts_db),
        last_seen_seq: 0,
        last_seen_at: now_string(),
    })
}

fn locate_in_room(
    room: &KnownRoom,
    event_id: &str,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Option<LocatedRecord>> {
    let Some(store) = open_indexed_room(room, warnings)? else {
        return Ok(None);
    };
    for fact in store.facts()? {
        if fact.event_id == event_id {
            return Ok(Some(LocatedRecord {
                source: "room".to_string(),
                repo_root: Some(room.repo_root.clone()),
                display_name: Some(room.display_name.clone()),
                facts_db: Some(room.facts_db.clone()),
                legacy_channel: None,
                local_seq: None,
                fact: Some(fact),
                record: None,
            }));
        }
    }
    Ok(None)
}

fn recent_in_room(
    room: &KnownRoom,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Vec<RecentRow>> {
    let Some(store) = open_indexed_room(room, warnings)? else {
        return Ok(Vec::new());
    };
    Ok(store
        .facts()?
        .into_iter()
        .map(|fact| RecentRow {
            source: "room".to_string(),
            repo_root: Some(room.repo_root.clone()),
            display_name: Some(room.display_name.clone()),
            facts_db: Some(room.facts_db.clone()),
            legacy_channel: None,
            local_seq: None,
            seq: Some(fact.seq),
            created_at: Some(fact.created_at.clone()),
            fact: Some(fact),
            record: None,
        })
        .collect())
}

fn open_indexed_room(
    room: &KnownRoom,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Option<RoomStore>> {
    if !room.facts_db.exists() {
        warnings.push(warning(
            "room_missing",
            format!(
                "indexed room is missing facts db: {}",
                room.facts_db.display()
            ),
            Some(room.facts_db.clone()),
        ));
        return Ok(None);
    }
    RoomStore::open_existing_at(room.repo_root.clone())
}

fn locate_in_legacy(
    event_id: &str,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Option<LocatedRecord>> {
    for (path, local_seq, record) in legacy_records(warnings)? {
        if legacy_event_id(&record).as_deref() == Some(event_id) {
            return Ok(Some(LocatedRecord {
                source: "legacy_channel".to_string(),
                repo_root: None,
                display_name: None,
                facts_db: None,
                legacy_channel: Some(path),
                local_seq,
                fact: None,
                record: Some(record),
            }));
        }
    }
    Ok(None)
}

fn recent_legacy(warnings: &mut Vec<DiscoveryWarning>) -> Result<Vec<RecentRow>> {
    Ok(legacy_records(warnings)?
        .into_iter()
        .map(|(path, local_seq, record)| RecentRow {
            source: "legacy_channel".to_string(),
            repo_root: None,
            display_name: None,
            facts_db: None,
            legacy_channel: Some(path),
            local_seq,
            seq: None,
            created_at: legacy_created_at(&record),
            fact: None,
            record: Some(record),
        })
        .collect())
}

fn legacy_records(
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Vec<(PathBuf, Option<i64>, Value)>> {
    let mut records = Vec::new();
    let root = legacy_apps_root();
    if !root.exists() {
        return Ok(records);
    }
    let entries =
        fs::read_dir(&root).map_err(RallyError::io(format!("read {}", root.display())))?;
    for entry in entries {
        let entry = entry.map_err(RallyError::io(format!("read {}", root.display())))?;
        let path = entry.path().join("changes.jsonl");
        if !path.exists() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                warnings.push(warning(
                    "legacy_channel_unreadable",
                    format!("failed to read legacy channel: {err}"),
                    Some(path),
                ));
                continue;
            }
        };
        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(record) => {
                    let local_seq = record
                        .get("local_seq")
                        .and_then(Value::as_i64)
                        .or(Some(idx as i64 + 1));
                    records.push((path.clone(), local_seq, record));
                }
                Err(err) => warnings.push(warning(
                    "legacy_channel_malformed",
                    format!("failed to parse legacy channel line {}: {err}", idx + 1),
                    Some(path.clone()),
                )),
            }
        }
    }
    Ok(records)
}

fn warn_if_legacy_exists(warnings: &mut Vec<DiscoveryWarning>) {
    let root = legacy_apps_root();
    if root.exists() {
        warnings.push(warning(
            "legacy_hidden",
            "legacy ~/.agent-rally-point/apps channels exist; pass --include-legacy to read them",
            Some(root),
        ));
    }
}

fn legacy_event_id(record: &Value) -> Option<String> {
    record
        .pointer("/event/id")
        .or_else(|| record.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn legacy_created_at(record: &Value) -> Option<String> {
    record
        .pointer("/event/time")
        .or_else(|| record.get("received_at"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn read_room_index_at(path: &Path) -> Result<RoomIndex> {
    if !path.exists() {
        return Ok(RoomIndex::default());
    }
    let text =
        fs::read_to_string(path).map_err(RallyError::io(format!("read {}", path.display())))?;
    if text.trim().is_empty() {
        return Ok(RoomIndex::default());
    }
    // Fast path: clean, single-document file.
    if let Ok(index) = serde_json::from_str::<RoomIndex>(&text) {
        return Ok(index);
    }
    // Resilient path: file may be torn (valid JSON followed by trailing garbage).
    // A streaming deserializer reads only the first well-formed value and stops,
    // so it succeeds even when trailing bytes would break a strict parse.
    if let Some(Ok(index)) = serde_json::Deserializer::from_str(&text)
        .into_iter::<RoomIndex>()
        .next()
    {
        return Ok(index);
    }
    // Legacy fallback: bare Vec<KnownRoom> (streaming, tolerates trailing data too).
    if let Some(Ok(rooms)) = serde_json::Deserializer::from_str(&text)
        .into_iter::<Vec<KnownRoom>>()
        .next()
    {
        return Ok(RoomIndex {
            schema: ROOM_INDEX_SCHEMA.to_string(),
            rooms,
        });
    }
    // Return the original strict-parse error (we already know it fails).
    Err(RallyError::json(format!("parse {}", path.display()))(
        serde_json::from_str::<serde_json::Value>(&text).unwrap_err(),
    ))
}

fn write_room_index_at(path: &Path, index: &RoomIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let content =
        serde_json::to_string_pretty(index).map_err(RallyError::json("render room index"))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, content)
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    fs::rename(&temp_path, path).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        RallyError::Io {
            context: format!("replace {} with {}", path.display(), temp_path.display()),
            source: err,
        }
    })
}

/// Resolve the path to the global discovery index, or `None` when global
/// discovery is disabled.
///
/// The index at `~/.agent-rally-point/rooms/v1/index.json` is a
/// **pointers-only** file — it tells `rally locate --all` and
/// `rally recent --all` which `.rally/facts.db` files exist on this
/// machine. It never holds canonical fact data; that lives per-repo under
/// `<repo_root>/.rally/ledger.jsonl` and is rebuilt into the cache on
/// demand. Disabling the index therefore *does not* affect coordination
/// within any single repo — only the cross-repo "what other rooms do I
/// know about?" surface.
///
/// Setting `RALLY_NO_GLOBAL_INDEX=1` (any non-empty value) returns `None`,
/// which both `refresh_room_index` and `known_rooms_with_current` already
/// treat as the fully-isolated mode: no writes to the index, no reads of
/// the index, and `--all` collapses to "this repo only".
fn room_index_path() -> Option<PathBuf> {
    if env::var_os("RALLY_NO_GLOBAL_INDEX")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return None;
    }
    Some(home_dir()?.join(".agent-rally-point/rooms/v1/index.json"))
}

fn legacy_apps_root() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-rally-point/apps")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn warning(
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<PathBuf>,
) -> DiscoveryWarning {
    DiscoveryWarning {
        code: code.into(),
        message: message.into(),
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rally-discovery-{label}-{nanos}.json"))
    }

    fn make_known_room(repo: &str) -> KnownRoom {
        KnownRoom {
            schema: ROOM_INDEX_SCHEMA.to_string(),
            repo_root: PathBuf::from(repo),
            display_name: repo.to_string(),
            facts_db: PathBuf::from(format!("{repo}/.rally/facts.db")),
            last_seen_seq: 1,
            last_seen_at: "2026-05-29T00:00:00Z".to_string(),
        }
    }

    /// A clean (non-torn) index still parses correctly.
    #[test]
    fn read_room_index_clean_round_trips() {
        let path = tmp_path("clean");
        let room = make_known_room("/home/user/my-repo");
        let index = RoomIndex {
            schema: ROOM_INDEX_SCHEMA.to_string(),
            rooms: vec![room],
        };
        fs::write(&path, serde_json::to_string_pretty(&index).unwrap()).unwrap();

        let result = read_room_index_at(&path).unwrap();
        assert_eq!(result.rooms.len(), 1);
        assert_eq!(
            result.rooms[0].repo_root,
            PathBuf::from("/home/user/my-repo")
        );
        let _ = fs::remove_file(&path);
    }

    /// A torn file (valid JSON + trailing garbage) returns the rooms from the
    /// first valid document instead of erroring.
    #[test]
    fn read_room_index_torn_file_recovers_rooms() {
        let path = tmp_path("torn");
        let room = make_known_room("/home/user/agent-rally-point");
        let index = RoomIndex {
            schema: ROOM_INDEX_SCHEMA.to_string(),
            rooms: vec![room],
        };
        // Simulate a torn/concatenated write: valid document + leftover bytes.
        let clean = serde_json::to_string_pretty(&index).unwrap();
        let torn = format!("{clean}\n  ]\n}}5:11Z\"\n}}");
        fs::write(&path, &torn).unwrap();

        let result = read_room_index_at(&path).unwrap();
        assert_eq!(
            result.rooms.len(),
            1,
            "torn file must recover the 1 room, not error or return empty"
        );
        assert_eq!(
            result.rooms[0].repo_root,
            PathBuf::from("/home/user/agent-rally-point")
        );
        let _ = fs::remove_file(&path);
    }
}
