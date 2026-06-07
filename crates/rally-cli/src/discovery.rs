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
    /// Set on collapsed/summary warnings that represent multiple occurrences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) count: Option<usize>,
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
    pub(crate) limit: i64,
    pub(crate) rows: Vec<RecentRow>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct MigrateLegacyData {
    /// Directory names (app slugs) matched and examined.
    pub(crate) slugs_found: Vec<String>,
    /// Total records read across all matched legacy channels.
    pub(crate) facts_read: usize,
    /// Records successfully replayed into the repo ledger.
    pub(crate) facts_migrated: usize,
    /// Records skipped because their event_id already exists in the ledger.
    pub(crate) facts_skipped_existing: usize,
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
pub(crate) struct RoomIndex {
    #[serde(default = "room_index_schema")]
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) rooms: Vec<KnownRoom>,
}

pub(crate) fn room_index_schema() -> String {
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

pub(crate) fn locate(event_id: &str) -> Result<LocateData> {
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
            "global index disabled (opt in with RALLY_GLOBAL_INDEX=1); showing this repo only",
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

    let mut missing_count: usize = 0;
    for room_entry in &index.rooms {
        let Some(store) = try_open_indexed_room(room_entry)? else {
            missing_count += 1;
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

        // last_activity_ts is populated by snapshot_from_facts from the same
        // facts slice — no second store.facts() call needed (fix #4).
        let last_activity_ts = snapshot.last_activity_ts.clone();

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

    if missing_count > 0 {
        warnings.push(warning_count(
            "rooms_missing",
            format!(
                "{missing_count} room{} in the registry no longer exist",
                if missing_count == 1 { "" } else { "s" }
            ),
            missing_count,
        ));
    }

    Ok(GlobalStatusData { repos, warnings })
}

pub(crate) fn recent(all: bool, limit: i64) -> Result<RecentData> {
    let current = repo_root()?;
    let mut warnings = Vec::new();
    let mut rows = Vec::new();
    let rooms = if all {
        known_rooms_with_current(&current, &mut warnings)?
    } else {
        vec![known_room_for_current(&current)?]
    };
    let mut missing_count: usize = 0;
    for room in rooms {
        let (room_rows, was_missing) = recent_in_room(&room, &mut warnings)?;
        rows.extend(room_rows);
        if was_missing {
            missing_count += 1;
        }
    }
    if missing_count > 0 {
        warnings.push(warning_count(
            "rooms_missing",
            format!(
                "{missing_count} room{} in the registry no longer exist",
                if missing_count == 1 { "" } else { "s" }
            ),
            missing_count,
        ));
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
) -> Result<(Vec<RecentRow>, bool)> {
    let Some(store) = try_open_indexed_room(room)? else {
        return Ok((Vec::new(), true));
    };
    let _ = warnings; // retained for future per-room warnings that are not room_missing
    let rows = store
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
        .collect();
    Ok((rows, false))
}

/// Opens an indexed room without touching the warnings vec.
/// Returns `Ok(None)` when the facts db is absent (room is stale/deleted).
fn try_open_indexed_room(room: &KnownRoom) -> Result<Option<RoomStore>> {
    if !room.facts_db.exists() {
        return Ok(None);
    }
    RoomStore::open_existing_at(room.repo_root.clone())
}

/// Opens an indexed room, pushing a per-item `room_missing` warning when the
/// facts db is absent. Only use this in non-looping contexts (e.g. `locate`)
/// where at most one stale entry will be encountered per call.
fn open_indexed_room(
    room: &KnownRoom,
    warnings: &mut Vec<DiscoveryWarning>,
) -> Result<Option<RoomStore>> {
    match try_open_indexed_room(room)? {
        Some(store) => Ok(Some(store)),
        None => {
            warnings.push(warning(
                "room_missing",
                format!(
                    "indexed room is missing facts db: {}",
                    room.facts_db.display()
                ),
                Some(room.facts_db.clone()),
            ));
            Ok(None)
        }
    }
}

pub(crate) fn read_room_index_at(path: &Path) -> Result<RoomIndex> {
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

pub(crate) fn write_room_index_at(path: &Path, index: &RoomIndex) -> Result<()> {
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
/// Global index is **opt-in** (default: off / one-store mode).
///
/// Returns `Some(path)` only when ALL of the following hold:
///   1. `RALLY_GLOBAL_INDEX` is set to a non-empty value (`1`, `true`, etc.)
///   2. `RALLY_NO_GLOBAL_INDEX` is NOT set (legacy kill-switch takes priority)
///
/// With no env vars set the default is `None` — per-repo isolation, no
/// cross-repo index reads or writes.  Set `RALLY_GLOBAL_INDEX=1` to opt in.
/// Setting `RALLY_NO_GLOBAL_INDEX` always overrides regardless of
/// `RALLY_GLOBAL_INDEX` (back-compat: existing opt-out scripts still work).
pub(crate) fn room_index_path() -> Option<PathBuf> {
    // Kill-switch wins unconditionally (back-compat).
    if env::var_os("RALLY_NO_GLOBAL_INDEX")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return None;
    }
    // Must explicitly opt in.
    if !env::var_os("RALLY_GLOBAL_INDEX")
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

/// Migrate surviving rally facts from the legacy `~/.agent-rally-point/apps/<slug>/changes.jsonl`
/// store into the current repo's `.rally/log` ledger.
///
/// Slug matching: any directory under `apps/` whose name equals the current repo's
/// basename, or starts with `{basename}-` followed by 8+ hex characters (the
/// suffix that the old coordinator appended as a path-hash disambiguator).
///
/// Only records that (a) have a current-format rally fact schema
/// (`agent-rally.fact.v1`) AND (b) whose `event_id` does not yet exist in the
/// ledger are replayed. All other records are counted in `facts_read` but
/// contribute to neither `facts_migrated` nor `facts_skipped_existing`.
///
/// The legacy files are NOT deleted — this is a replay-only, non-destructive
/// migrator. Running it twice is safe: the second run produces
/// `facts_migrated == 0` because all event_ids are already present.
pub(crate) fn migrate_legacy(room: &RoomStore, repo_basename: &str) -> Result<MigrateLegacyData> {
    migrate_legacy_from(room, repo_basename, &legacy_apps_root())
}

fn migrate_legacy_from(
    room: &RoomStore,
    repo_basename: &str,
    apps_root: &Path,
) -> Result<MigrateLegacyData> {
    let apps_root = apps_root.to_path_buf();
    let mut slugs_found: Vec<String> = Vec::new();
    let mut facts_read: usize = 0;
    let mut facts_migrated: usize = 0;
    let mut facts_skipped_existing: usize = 0;
    let mut warnings: Vec<DiscoveryWarning> = Vec::new();

    if !apps_root.exists() {
        return Ok(MigrateLegacyData {
            slugs_found,
            facts_read,
            facts_migrated,
            facts_skipped_existing,
            warnings,
        });
    }

    // Collect the set of event_ids already present in the repo ledger for
    // idempotency checks.
    let existing_ids: std::collections::BTreeSet<String> = room
        .facts()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.event_id)
        .collect();

    // Enumerate candidate slug directories.
    let entries = match fs::read_dir(&apps_root) {
        Ok(e) => e,
        Err(err) => {
            warnings.push(warning(
                "legacy_apps_unreadable",
                format!("failed to read legacy apps dir: {err}"),
                Some(apps_root),
            ));
            return Ok(MigrateLegacyData {
                slugs_found,
                facts_read,
                facts_migrated,
                facts_skipped_existing,
                warnings,
            });
        }
    };

    for entry in entries.flatten() {
        let slug = entry.file_name().to_string_lossy().into_owned();
        if !slug_matches_repo(&slug, repo_basename) {
            continue;
        }
        let channel = entry.path().join("changes.jsonl");
        if !channel.exists() {
            continue;
        }
        slugs_found.push(slug.clone());

        let text = match fs::read_to_string(&channel) {
            Ok(t) => t,
            Err(err) => {
                warnings.push(warning(
                    "legacy_channel_unreadable",
                    format!("failed to read legacy channel {slug}: {err}"),
                    Some(channel),
                ));
                continue;
            }
        };

        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // Parse as a serde_json::Value first to check schema.
            let record: Value = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(err) => {
                    warnings.push(warning(
                        "legacy_channel_malformed",
                        format!("failed to parse {slug} line {}: {err}", idx + 1),
                        Some(channel.clone()),
                    ));
                    continue;
                }
            };

            // Only migrate current-format rally facts.
            let schema = record.get("schema").and_then(Value::as_str).unwrap_or("");
            if schema != crate::FACT_SCHEMA {
                continue;
            }

            facts_read += 1;

            // Deserialize as a Fact.
            let fact: Fact = match serde_json::from_value(record) {
                Ok(f) => f,
                Err(err) => {
                    warnings.push(warning(
                        "legacy_fact_malformed",
                        format!(
                            "failed to deserialize {slug} line {} as Fact: {err}",
                            idx + 1
                        ),
                        Some(channel.clone()),
                    ));
                    continue;
                }
            };

            if existing_ids.contains(&fact.event_id) {
                facts_skipped_existing += 1;
                continue;
            }

            // Replay into the repo ledger via the normal append path.
            // seq is reset to 0 so the store assigns the next local seq.
            let replay = Fact { seq: 0, ..fact };
            match room.append_fact(&replay) {
                Ok(_) => facts_migrated += 1,
                Err(err) => {
                    warnings.push(warning(
                        "legacy_fact_replay_failed",
                        format!("failed to replay fact from {slug}: {err}"),
                        Some(channel.clone()),
                    ));
                }
            }
        }
    }

    // Sort slugs for deterministic output.
    slugs_found.sort();

    Ok(MigrateLegacyData {
        slugs_found,
        facts_read,
        facts_migrated,
        facts_skipped_existing,
        warnings,
    })
}

/// Returns true when the given `slug` directory name matches the repo `basename`.
///
/// Match rules (both cover known slug derivation patterns):
/// - Exact match: `slug == basename`
/// - Hash-suffix match: `slug` starts with `{basename}-` and the remainder is
///   8+ lowercase hex characters (the old coordinator's path-hash suffix).
pub(crate) fn slug_matches_repo(slug: &str, basename: &str) -> bool {
    if slug == basename {
        return true;
    }
    if let Some(suffix) = slug.strip_prefix(&format!("{basename}-")) {
        // Require ≥ 8 hex chars (path hash suffix pattern).
        if suffix.len() >= 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
    }
    false
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
        count: None,
    }
}

fn warning_count(
    code: impl Into<String>,
    message: impl Into<String>,
    count: usize,
) -> DiscoveryWarning {
    DiscoveryWarning {
        code: code.into(),
        message: message.into(),
        path: None,
        count: Some(count),
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

    /// Multiple stale registry entries must produce exactly one `rooms_missing`
    /// warning (with `count` set) — not one warning per missing room.
    #[test]
    fn open_indexed_room_collapsed_warning_for_missing_rooms() {
        // All three rooms point to paths that don't exist.
        let rooms = vec![
            make_known_room("/nonexistent/repo-a"),
            make_known_room("/nonexistent/repo-b"),
            make_known_room("/nonexistent/repo-c"),
        ];

        let mut missing_count: usize = 0;
        let mut warnings: Vec<DiscoveryWarning> = Vec::new();
        for room in &rooms {
            if try_open_indexed_room(room).unwrap().is_none() {
                missing_count += 1;
            }
        }
        if missing_count > 0 {
            warnings.push(warning_count(
                "rooms_missing",
                format!(
                    "{missing_count} room{} in the registry no longer exist",
                    if missing_count == 1 { "" } else { "s" }
                ),
                missing_count,
            ));
        }

        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one collapsed warning, got {}: {warnings:?}",
            warnings.len()
        );
        assert_eq!(warnings[0].code, "rooms_missing");
        assert_eq!(warnings[0].count, Some(3));
    }

    // =========================================================================
    // B17 — slug_matches_repo unit tests
    // =========================================================================

    /// Exact slug name matches the repo basename.
    #[test]
    fn slug_matches_exact_basename() {
        assert!(slug_matches_repo("agent-rally-point", "agent-rally-point"));
    }

    /// Slug with 8-char hex suffix matches.
    #[test]
    fn slug_matches_with_8char_hex_suffix() {
        assert!(slug_matches_repo(
            "agent-rally-point-2b14b480",
            "agent-rally-point"
        ));
    }

    /// Slug with longer hex suffix (16 chars) matches.
    #[test]
    fn slug_matches_with_longer_hex_suffix() {
        assert!(slug_matches_repo("my-repo-abcdef0123456789", "my-repo"));
    }

    /// Slug with only 7 hex chars after the dash does NOT match (too short).
    #[test]
    fn slug_does_not_match_short_hex_suffix() {
        assert!(!slug_matches_repo(
            "agent-rally-point-2b14b48",
            "agent-rally-point"
        ));
    }

    /// Slug with non-hex suffix does NOT match.
    #[test]
    fn slug_does_not_match_non_hex_suffix() {
        assert!(!slug_matches_repo(
            "agent-rally-point-worker",
            "agent-rally-point"
        ));
    }

    /// Completely unrelated slug does NOT match.
    #[test]
    fn slug_does_not_match_unrelated() {
        assert!(!slug_matches_repo("atomize-ai", "agent-rally-point"));
    }

    // =========================================================================
    // B17 — migrate_legacy idempotency tests
    // =========================================================================

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rally-migrate-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a minimal rally fact JSON line for the legacy store.
    fn make_rally_fact_line(event_id: &str, subject: &str) -> String {
        serde_json::json!({
            "schema": crate::FACT_SCHEMA,
            "event_id": event_id,
            "seq": 1,
            "thread_id": "thr_test",
            "kind": "decision",
            "tool": "test-tool",
            "role": null,
            "subject": subject,
            "scope": [],
            "created_at": "2026-05-29T00:00:00Z",
            "summary": null,
            "evidence": [],
            "target": null,
            "ref_id": null,
            "status": null,
            "severity": null,
            "uri": null,
            "session": null,
        })
        .to_string()
    }

    /// B17.migrate: seeding a legacy channel with rally facts and running
    /// migrate_legacy replays them into the repo ledger. A second run
    /// migrates 0 facts (idempotent).
    #[test]
    fn migrate_legacy_replays_facts_and_is_idempotent() {
        let root = tmp_dir("migrate-idempotent");
        let home = tmp_dir("migrate-idempotent-home");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // Build the legacy channel under a temp home — pass apps_root directly
        // to avoid global HOME mutation (which would race with parallel tests).
        let apps_root = home.join(".agent-rally-point/apps");
        let apps_dir = apps_root.join("my-repo");
        fs::create_dir_all(&apps_dir).unwrap();
        let channel = apps_dir.join("changes.jsonl");
        fs::write(
            &channel,
            format!(
                "{}\n{}\n",
                make_rally_fact_line("evt_b17_001", "b17 migrate test decision 1"),
                make_rally_fact_line("evt_b17_002", "b17 migrate test decision 2"),
            ),
        )
        .unwrap();

        let room = RoomStore::open_at(root.clone()).unwrap();

        // First run: should migrate both facts.
        let result = migrate_legacy_from(&room, "my-repo", &apps_root).unwrap();
        assert_eq!(result.slugs_found, vec!["my-repo".to_string()]);
        assert_eq!(result.facts_read, 2, "two rally facts in the channel");
        assert_eq!(result.facts_migrated, 2, "both replayed on first run");
        assert_eq!(result.facts_skipped_existing, 0);

        // Verify facts are in the ledger.
        let facts = room.facts().unwrap();
        let event_ids: Vec<&str> = facts.iter().map(|f| f.event_id.as_str()).collect();
        assert!(
            event_ids.contains(&"evt_b17_001"),
            "evt_b17_001 must be in ledger"
        );
        assert!(
            event_ids.contains(&"evt_b17_002"),
            "evt_b17_002 must be in ledger"
        );

        // Second run: both already exist → migrated == 0.
        let result2 = migrate_legacy_from(&room, "my-repo", &apps_root).unwrap();
        assert_eq!(result2.facts_migrated, 0, "second run must migrate nothing");
        assert_eq!(
            result2.facts_skipped_existing, 2,
            "second run must skip both as existing"
        );

        // Legacy file is untouched (not deleted).
        assert!(
            channel.exists(),
            "migrate_legacy must not delete the legacy file"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&home).ok();
    }

    /// B17.migrate: non-rally records (build-loop phase events without rally schema)
    /// are counted in facts_read=0 — only records with schema==agent-rally.fact.v1
    /// are counted as facts_read.
    #[test]
    fn migrate_legacy_skips_non_rally_records() {
        let root = tmp_dir("migrate-non-rally");
        let home = tmp_dir("migrate-non-rally-home");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let apps_root = home.join(".agent-rally-point/apps");
        let apps_dir = apps_root.join("my-repo");
        fs::create_dir_all(&apps_dir).unwrap();
        let channel = apps_dir.join("changes.jsonl");
        // One build-loop phase record (no rally schema) + one rally fact.
        fs::write(
            &channel,
            format!(
                "{}\n{}\n",
                r#"{"ts":1780000000.0,"kind":"phase","tool":"claude_code","app_slug":"my-repo","payload":{"phase":"rally-start"}}"#,
                make_rally_fact_line("evt_b17_003", "b17 non-rally skip test"),
            ),
        )
        .unwrap();

        let room = RoomStore::open_at(root.clone()).unwrap();

        let result = migrate_legacy_from(&room, "my-repo", &apps_root).unwrap();
        assert_eq!(
            result.facts_read, 1,
            "only the rally-schema record is counted as facts_read"
        );
        assert_eq!(result.facts_migrated, 1, "the rally fact is migrated");

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&home).ok();
    }

    /// B17.migrate: recent and locate do NOT return legacy-only facts (flag retired).
    /// The repo ledger is the only source; legacy facts are invisible until migrated.
    #[test]
    fn recent_and_locate_do_not_read_legacy() {
        let root = tmp_dir("recent-no-legacy");
        let home = tmp_dir("recent-no-legacy-home");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // Seed a legacy channel with a rally fact — should NOT appear in repo ledger.
        let apps_root = home.join(".agent-rally-point/apps");
        let apps_dir = apps_root.join("my-repo");
        fs::create_dir_all(&apps_dir).unwrap();
        let channel = apps_dir.join("changes.jsonl");
        fs::write(
            &channel,
            format!(
                "{}\n",
                make_rally_fact_line("evt_b17_legacy_only", "legacy only fact")
            ),
        )
        .unwrap();

        // Write one fact in the real repo ledger.
        let room = RoomStore::open_at(root.clone()).unwrap();
        let live_fact = crate::store::Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: "evt_b17_live".to_string(),
            seq: 0,
            thread_id: "thr_b17".to_string(),
            kind: crate::store::FactKind::Decision,
            tool: Some("b17-tool".to_string()),
            role: None,
            subject: "live fact".to_string(),
            scope: Vec::new(),
            created_at: "2026-05-29T00:00:00Z".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&live_fact).unwrap();
        drop(room);

        // Verify via the room facts API: the legacy-only fact is NOT in the ledger
        // (discover::recent/locate no longer reads the legacy store).
        let reader = RoomStore::open_at(root.clone()).unwrap();
        let facts = reader.facts().unwrap();
        assert!(
            facts.iter().any(|f| f.event_id == "evt_b17_live"),
            "live fact must be in ledger"
        );
        assert!(
            !facts.iter().any(|f| f.event_id == "evt_b17_legacy_only"),
            "legacy-only fact must NOT be in ledger before migrate"
        );

        // After migrate_legacy_from, the legacy fact IS in the ledger.
        let room2 = RoomStore::open_at(root.clone()).unwrap();
        migrate_legacy_from(&room2, "my-repo", &apps_root).unwrap();
        let facts_after = room2.facts().unwrap();
        assert!(
            facts_after
                .iter()
                .any(|f| f.event_id == "evt_b17_legacy_only"),
            "after migration, legacy fact must appear in ledger"
        );

        // Legacy file still exists (non-destructive).
        assert!(
            channel.exists(),
            "legacy file must not be deleted by migrator"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&home).ok();
    }

    /// B17: room_index_path() must default to None (global index off by default).
    ///
    /// Three cases:
    ///   1. No env vars → None (one-store default)
    ///   2. RALLY_GLOBAL_INDEX=1 → Some(path)
    ///   3. RALLY_GLOBAL_INDEX=1 + RALLY_NO_GLOBAL_INDEX=1 → None (kill-switch wins)
    ///
    /// Uses a process-level mutex so env mutations don't race other tests that
    /// also read these env vars.
    #[test]
    fn b17_room_index_path_defaults_off_opt_in_and_killswitch() {
        // Serialize against all other env-touching tests in this binary via the
        // crate-wide lock defined in lib.rs.
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // SAFETY: single-threaded via mutex; no other threads read these vars
        // while the lock is held. Required by Rust's unsafe contract on set/remove_var.

        // --- Case 1: no env vars → None ---
        unsafe {
            std::env::remove_var("RALLY_GLOBAL_INDEX");
            std::env::remove_var("RALLY_NO_GLOBAL_INDEX");
        }
        assert!(
            room_index_path().is_none(),
            "default (no env vars) must return None — global index is off by default"
        );

        // --- Case 2: opt-in → Some ---
        unsafe {
            std::env::set_var("RALLY_GLOBAL_INDEX", "1");
            std::env::remove_var("RALLY_NO_GLOBAL_INDEX");
        }
        let path = room_index_path();
        assert!(
            path.is_some(),
            "RALLY_GLOBAL_INDEX=1 must return Some(path)"
        );
        let p = path.unwrap();
        assert!(
            p.ends_with(".agent-rally-point/rooms/v1/index.json"),
            "opt-in path must point at the global index: got {p:?}"
        );

        // --- Case 3: both set — kill-switch wins → None ---
        unsafe {
            std::env::set_var("RALLY_GLOBAL_INDEX", "1");
            std::env::set_var("RALLY_NO_GLOBAL_INDEX", "1");
        }
        assert!(
            room_index_path().is_none(),
            "RALLY_NO_GLOBAL_INDEX must override RALLY_GLOBAL_INDEX (kill-switch wins)"
        );

        // Restore clean state.
        unsafe {
            std::env::remove_var("RALLY_GLOBAL_INDEX");
            std::env::remove_var("RALLY_NO_GLOBAL_INDEX");
        }
    }
}
