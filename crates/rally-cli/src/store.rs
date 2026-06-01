use factstr::{EventQuery as FactQuery, EventStore, EventStoreError, NewEvent};
use factstr_sqlite::SqliteStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Filename of the **legacy** monolithic ledger (R1).
///
/// Prior to R5, every event in the room landed in this one append-only file
/// at `.rally/ledger.jsonl`. R5 supersedes the monolith with per-engagement
/// segments at `.rally/log/<engagement>.jsonl`. On first open in R5, the
/// monolith is partitioned into segments and moved to
/// `.rally/archive/ledger-pre-segment.jsonl`. The file name is kept exported
/// because rooms cloned at the R1 layer still carry it, and the replay path
/// transparently unions segments + legacy monolith + archive.
pub(crate) const LEDGER_FILENAME: &str = "ledger.jsonl";

/// Directory holding per-engagement segment files (R5). Each segment is a
/// `<engagement-or-utc-date>.jsonl` append-only file with the same LedgerLine
/// shape as the legacy monolith. All segment files together form the
/// canonical record; replaying them in seq order rebuilds `facts.db`.
pub(crate) const LOG_DIRNAME: &str = "log";

/// Index file inside the log dir. Maps each segment to `{first_seq, last_seq,
/// count, engagement, span: {first_ts, last_ts}}`. Refreshed on append and on
/// open. Read by R6 (`rally retrospective`) and R7 (rotation).
pub(crate) const LOG_INDEX_FILENAME: &str = "index.json";

/// Directory holding rotated/migrated segments (R5 migration, R7 rotation).
/// Same line format as live segments; replay walks here too.
pub(crate) const ARCHIVE_DIRNAME: &str = "archive";

/// Filename used by the R5 migration to preserve the R1 monolith verbatim.
pub(crate) const ARCHIVED_MONOLITH_FILENAME: &str = "ledger-pre-segment.jsonl";

/// Env var that pins the active engagement label for this process. Set by
/// host wrappers, direnv, CI runners, etc.
pub(crate) const ENGAGEMENT_ENV_VAR: &str = "RALLY_ENGAGEMENT";

/// On-disk file holding the persisted active engagement label, written by
/// `rally enter --engagement <name>` so subsequent calls without the env var
/// or flag inherit the label. Plain text, one line, no trailing newline
/// required.
pub(crate) const ACTIVE_ENGAGEMENT_FILENAME: &str = "active-engagement";

/// Cross-process guard for critical sections that must keep `facts.db` and the
/// canonical JSONL segments in lock-step.
const ROOM_MUTATION_LOCK_FILENAME: &str = "mutation.lock";

#[cfg(unix)]
mod unix_lock {
    pub(crate) const LOCK_EX: i32 = 2;
    pub(crate) const LOCK_UN: i32 = 8;

    unsafe extern "C" {
        pub(crate) fn flock(fd: i32, operation: i32) -> i32;
    }
}

use crate::backends::ManagedSession;
use crate::cli::RoomArgs;
use crate::discovery::refresh_room_index;
use crate::error::{RallyError, Result};
use crate::{FACT_SCHEMA, normalize_paths, now_string, path_matches_scope, repo_root, short_id};

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FactKind {
    Claim,
    Release,
    Blocker,
    Resolve,
    Decision,
    Artifact,
    Handoff,
    Risk,
    Lesson,
    Session,
    Wake,
    /// Agent presence heartbeat — emitted once per `rally enter` call.
    Presence,
    /// R10 read-checkpoint — durable record that a tool deliberately read up to
    /// a given sequence number. Appended by `rally next --tool X` (and optionally
    /// `rally room --tool X`) only when the tool's read position has ADVANCED
    /// since its last recorded checkpoint (coalesced — no-op polls write nothing).
    ///
    /// `summary` encodes the read sequence number as `"read_seq:<N>"` (same
    /// pattern as `build_id:<BUILD_ID>` in presence facts — no schema bump).
    ///
    /// EXCLUDED from claimable-work surfaces: not surfaced in `active_claims`,
    /// `next` candidates, `open_handoffs`, or any backlog bucket.
    Read,
    /// Backlog item — encodes `{id, intent, owns[], depends_on[], status}` in
    /// existing fields (summary/scope/evidence) using the additive-marker pattern.
    /// Never surfaced in active_claims / open_handoffs / next candidates.
    BacklogItem,
    /// B13: handoff receipt — durable record that a handoff was acted on by the
    /// recipient.  `ref_id` points to the originating handoff `event_id`.
    /// Subject prefix: `"receipt:"`.  Closes the referenced handoff from
    /// `open_handoffs` (same projection logic as `resolve`).
    Receipt,
    /// B1 (pi-dynamic seam): agent declares it is going dormant and requests a
    /// future wake signal.  Encoded fields (additive marker pattern, no struct
    /// field changes):
    ///   - `summary`: `"reason:<r>"` + whitespace-separated `"wake_after:<iso>"`.
    ///   - `scope`: optional `"run:<id>"`, `"step:<id>"`, `"parent-step:<id>"`
    ///     lineage markers so a causation DAG can be reconstructed.
    ///   - `tool`: the sleeping tool (the one requesting the wake).
    ///   - `status`: `"pending"` until woken.
    ///
    /// RALLY RECORDS ONLY. The actual model wake is performed by the external
    /// runner (rally watch / LaunchAgent / cron). Rally never calls exec/spawn.
    Standby,
    /// Room north-star (mission) or per-agent autonomy envelope. Additive-marker
    /// pattern — no Fact struct fields change; specifics encoded in existing fields:
    ///   - Mission fact:   `scope = ["mission"]`, `subject = <north-star text>`.
    ///   - Envelope fact:  `scope = ["envelope", "agent:<name>"]`,
    ///     `subject = "autonomy envelope for <name>"`,
    ///     `summary = "may:<...>"`,
    ///     `evidence = ["must_check:<...>"]`.
    ///
    /// RALLY RECORDS AND EXPOSES ONLY. Never checks, gates, or grants anything.
    /// Setting again supersedes: latest-by-seq wins on read.
    Mission,
    #[serde(other)]
    #[default]
    Unknown,
}

impl FactKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "claim" => Some(Self::Claim),
            "release" => Some(Self::Release),
            "blocker" => Some(Self::Blocker),
            "resolve" => Some(Self::Resolve),
            "decision" => Some(Self::Decision),
            "artifact" => Some(Self::Artifact),
            "handoff" => Some(Self::Handoff),
            "risk" => Some(Self::Risk),
            "lesson" => Some(Self::Lesson),
            "session" => Some(Self::Session),
            "wake" => Some(Self::Wake),
            "presence" => Some(Self::Presence),
            "read" => Some(Self::Read),
            "backlog-item" => Some(Self::BacklogItem),
            "receipt" => Some(Self::Receipt),
            "standby" => Some(Self::Standby),
            "mission" => Some(Self::Mission),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Claim => "claim",
            Self::Release => "release",
            Self::Blocker => "blocker",
            Self::Resolve => "resolve",
            Self::Decision => "decision",
            Self::Artifact => "artifact",
            Self::Handoff => "handoff",
            Self::Risk => "risk",
            Self::Lesson => "lesson",
            Self::Session => "session",
            Self::Wake => "wake",
            Self::Presence => "presence",
            Self::Read => "read",
            Self::BacklogItem => "backlog-item",
            Self::Receipt => "receipt",
            Self::Standby => "standby",
            Self::Mission => "mission",
            Self::Unknown => "unknown",
        }
    }
}

impl PartialEq<&str> for FactKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(crate) struct Fact {
    #[serde(default = "fact_schema")]
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) event_id: String,
    #[serde(default)]
    pub(crate) seq: i64,
    #[serde(default)]
    pub(crate) thread_id: String,
    #[serde(default)]
    pub(crate) kind: FactKind,
    #[serde(default)]
    pub(crate) tool: Option<String>,
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) subject: String,
    #[serde(default)]
    pub(crate) scope: Vec<String>,
    #[serde(default)]
    pub(crate) created_at: String,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) evidence: Vec<String>,
    #[serde(default)]
    pub(crate) target: Option<String>,
    #[serde(default, rename = "ref")]
    pub(crate) ref_id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) severity: Option<String>,
    #[serde(default)]
    pub(crate) uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<ManagedSession>,
}

impl Fact {
    fn from_value(value: Value, seq: i64) -> Result<Self> {
        let mut fact: Self =
            serde_json::from_value(value).map_err(RallyError::json("parse fact payload"))?;
        if fact.seq == 0 {
            fact.seq = seq;
        }
        Ok(fact)
    }
}

fn fact_schema() -> String {
    FACT_SCHEMA.to_string()
}

/// A tool that has entered the room, derived from presence + authored facts.
///
/// `status` is "active" if `last_seen_ts` is within the last 15 minutes,
/// "idle" otherwise.  The 15-minute threshold is intentionally generous so
/// agents that are doing long computes don't flicker out of the squad view.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct Squad {
    pub(crate) tool: String,
    pub(crate) last_seen_seq: i64,
    pub(crate) last_seen_ts: String,
    /// "active" or "idle".  Active = last_seen_ts within 15 minutes of now.
    pub(crate) status: String,
    /// Coordination-mandate (C1): has this squad recorded a `coordination:ack`
    /// fact? Acknowledged squads have ingested the rules/guardrails/lead/mission.
    pub(crate) acknowledged: bool,
}

/// Seconds of inactivity after which a squad member is marked "idle".
const IDLE_THRESHOLD_SECS: i64 = 15 * 60;

/// Coordination-mandate (C1): tools that have recorded a `coordination:ack`
/// decision. A squad is "acknowledged" iff it appears here.
pub(crate) fn acknowledged_tools(facts: &[Fact]) -> std::collections::BTreeSet<String> {
    facts
        .iter()
        .filter(|f| f.kind == "decision" && f.subject == "coordination:ack")
        .filter_map(|f| f.tool.clone())
        .collect()
}

/// R10: per-tool read receipt projected from `FactKind::Read` checkpoints.
///
/// `last_read_seq` is the highest sequence number the tool has durably
/// recorded as read. `behind_by` is `max_seq - last_read_seq` (0 = caught up).
/// `status` is "caught_up" when `behind_by == 0`, else "behind".
///
/// Surfaced only under `rally room --readers`; omitted from the default room
/// output to avoid bloat.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct ReadReceipt {
    pub(crate) tool: String,
    pub(crate) last_read_seq: i64,
    pub(crate) behind_by: i64,
    /// "caught_up" | "behind"
    pub(crate) status: String,
}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct RoomSnapshot {
    pub(crate) max_seq: i64,
    /// R10: highest seq of a substantive (non-read-checkpoint) fact.
    /// Used internally by `command_next` to record the read position WITHOUT
    /// inflating it with the read-checkpoint's own seq (anti-loop).
    /// Not serialized to JSON — internal field only.
    #[serde(skip)]
    pub(crate) content_max_seq: i64,
    /// `created_at` of the highest-seq fact; `None` when the room is empty.
    /// Populated by `snapshot_from_facts` so `status_global` avoids a second
    /// `store.facts()` call. Not serialized to the public room JSON.
    #[serde(skip)]
    pub(crate) last_activity_ts: Option<String>,
    pub(crate) active_claims: Vec<Fact>,
    pub(crate) active_blockers: Vec<Fact>,
    pub(crate) open_handoffs: Vec<Fact>,
    pub(crate) current_decisions: Vec<Fact>,
    pub(crate) current_risks: Vec<Fact>,
    pub(crate) recent_artifacts: Vec<Fact>,
    pub(crate) unconsumed_artifacts: Vec<Fact>,
    pub(crate) stale_facts: Vec<Fact>,
    /// Distinct tools that have entered or authored facts in this room.
    pub(crate) squads: Vec<Squad>,
    /// Tool asserting the `role:lead` decision, if any.
    pub(crate) lead: Option<String>,
    /// R10: per-tool read receipts projected from `FactKind::Read` checkpoints.
    /// Populated only when `include_readers` is requested (see
    /// `RoomStore::snapshot_with_readers`); empty in the default snapshot.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) readers: Vec<ReadReceipt>,
    /// Current room north-star text, projected from the latest `FactKind::Mission`
    /// fact whose scope contains `"mission"`. `None` when no mission has been set.
    /// Omitted from JSON when unset so existing B16-style round-trip tests are
    /// unaffected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mission: Option<String>,
}

impl RoomSnapshot {
    pub(crate) fn filtered(self, query: &RoomQuery) -> Self {
        if query.is_empty() {
            return self;
        }
        Self {
            max_seq: self.max_seq,
            content_max_seq: self.content_max_seq,
            last_activity_ts: self.last_activity_ts,
            active_claims: filter_facts(self.active_claims, query),
            active_blockers: filter_facts(self.active_blockers, query),
            open_handoffs: filter_facts(self.open_handoffs, query),
            current_decisions: filter_facts(self.current_decisions, query),
            current_risks: filter_facts(self.current_risks, query),
            recent_artifacts: filter_facts(self.recent_artifacts, query),
            unconsumed_artifacts: filter_facts(self.unconsumed_artifacts, query),
            stale_facts: filter_facts(self.stale_facts, query),
            // squads, lead, readers, and mission are room-level aggregates; not filtered by path/tool query.
            squads: self.squads,
            lead: self.lead,
            readers: self.readers,
            mission: self.mission,
        }
    }
}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct RoomQuery {
    pub(crate) tool: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) paths: Vec<String>,
    #[serde(rename = "event")]
    pub(crate) event_id: Option<String>,
    #[serde(rename = "thread")]
    pub(crate) thread_id: Option<String>,
    pub(crate) since: Option<i64>,
    /// R10: when true, `command_room` projects per-tool read receipts.
    /// Not serialized into the query output (internal routing only).
    #[serde(skip)]
    pub(crate) readers: bool,
}

impl RoomQuery {
    pub(crate) fn from(args: RoomArgs) -> Self {
        Self {
            tool: args.tool,
            role: args.role,
            paths: normalize_paths(args.paths),
            event_id: args.event_id,
            thread_id: args.thread_id,
            since: args.since,
            readers: args.readers,
        }
    }

    fn is_empty(&self) -> bool {
        self.tool.is_none()
            && self.role.is_none()
            && self.paths.is_empty()
            && self.event_id.is_none()
            && self.thread_id.is_none()
            && self.since.is_none()
    }

    fn matches(&self, fact: &Fact) -> bool {
        if let Some(tool) = &self.tool {
            let tool_matches = fact.tool.as_deref() == Some(tool.as_str())
                || fact.target.as_deref() == Some(tool.as_str());
            if !tool_matches {
                return false;
            }
        }
        if let Some(role) = &self.role {
            if fact.role.as_deref() != Some(role.as_str()) {
                return false;
            }
        }
        if !self.paths.is_empty()
            && !self.paths.iter().any(|path| {
                fact.scope
                    .iter()
                    .any(|scope| path_matches_scope(scope, path))
            })
        {
            return false;
        }
        if let Some(event_id) = &self.event_id {
            let related = fact.event_id == *event_id || fact.ref_id.as_deref() == Some(event_id);
            if !related {
                return false;
            }
        }
        if let Some(thread_id) = &self.thread_id {
            if fact.thread_id != *thread_id {
                return false;
            }
        }
        if let Some(since) = self.since {
            if fact.seq <= since {
                return false;
            }
        }
        true
    }
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct RoomSummary {
    pub(crate) max_seq: i64,
    pub(crate) active_claims: usize,
    pub(crate) active_blockers: usize,
    pub(crate) open_handoffs: usize,
    pub(crate) current_decisions: usize,
    pub(crate) current_risks: usize,
    pub(crate) recent_artifacts: usize,
    pub(crate) unconsumed_artifacts: usize,
    pub(crate) stale_facts: usize,
}

impl From<&RoomSnapshot> for RoomSummary {
    fn from(snapshot: &RoomSnapshot) -> Self {
        Self {
            max_seq: snapshot.max_seq,
            active_claims: snapshot.active_claims.len(),
            active_blockers: snapshot.active_blockers.len(),
            open_handoffs: snapshot.open_handoffs.len(),
            current_decisions: snapshot.current_decisions.len(),
            current_risks: snapshot.current_risks.len(),
            recent_artifacts: snapshot.recent_artifacts.len(),
            unconsumed_artifacts: snapshot.unconsumed_artifacts.len(),
            stale_facts: snapshot.stale_facts.len(),
        }
    }
}

pub(crate) struct RoomStore {
    fact_store: SqliteStore,
    cursor_path: PathBuf,
    repo_root: PathBuf,
    facts_db_path: PathBuf,
    /// Per-engagement segment directory (R5). All segment files together form
    /// the canonical append-only record.
    log_dir: PathBuf,
    /// Rotated/migrated segments (R5 migration on first open; R7 rotation).
    /// Replay walks here too, after live segments.
    archive_dir: PathBuf,
    /// Engagement label stamped into every segment append. Resolved once at
    /// open via [`resolve_active_engagement`] (env var → on-disk file → UTC
    /// date). Empty string is never produced.
    active_engagement: String,
}

#[cfg(unix)]
struct RoomMutationLock {
    file: fs::File,
}

#[cfg(not(unix))]
struct RoomMutationLock;

#[cfg(unix)]
fn acquire_room_mutation_lock(room_dir: &Path) -> Result<RoomMutationLock> {
    fs::create_dir_all(room_dir)
        .map_err(RallyError::io(format!("create {}", room_dir.display())))?;
    let path = room_dir.join(ROOM_MUTATION_LOCK_FILENAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(RallyError::io(format!("open {}", path.display())))?;
    let rc = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_EX) };
    if rc != 0 {
        return Err(RallyError::Io {
            context: format!("lock {}", path.display()),
            source: io::Error::last_os_error(),
        });
    }
    Ok(RoomMutationLock { file })
}

#[cfg(not(unix))]
fn acquire_room_mutation_lock(_room_dir: &Path) -> Result<RoomMutationLock> {
    Ok(RoomMutationLock)
}

#[cfg(unix)]
impl Drop for RoomMutationLock {
    fn drop(&mut self) {
        let _ = unsafe { unix_lock::flock(self.file.as_raw_fd(), unix_lock::LOCK_UN) };
    }
}

/// One line of a segment file.
///
/// Compact on purpose: one event, its assigned `seq` (factstr's monotonic
/// `sequence_number`), an `occurred_at` ISO-8601 timestamp, the factstr
/// `event_type`, and the full payload (the serialised `Fact`). Replaying these
/// lines in order through `factstr` rebuilds `facts.db` verbatim because
/// factstr assigns seqs deterministically in append order.
///
/// `engagement` is the per-row engagement tag (R5). Older lines migrated from
/// the R1 monolith may carry the UTC date that the row was first observed (no
/// tag was recorded pre-R5). `serde(default)` keeps the format
/// forward-compatible — readers that don't know the field treat it as absent.
#[derive(Debug, Deserialize, Serialize)]
struct LedgerLine {
    seq: i64,
    occurred_at: String,
    event_type: String,
    payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engagement: Option<String>,
}

/// One entry of `.rally/log/index.json`.
#[derive(Debug, Deserialize, Serialize)]
struct SegmentIndexEntry {
    segment: String,    // filename only (e.g. "2026-05-29.jsonl")
    engagement: String, // segment key
    first_seq: i64,
    last_seq: i64,
    count: i64,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

impl RoomStore {
    pub(crate) fn open() -> Result<Self> {
        Self::open_at(repo_root()?)
    }

    /// Open the per-repo room, applying the **canonical segments / derived
    /// db** contract (R5; supersedes R1's single-monolith contract):
    ///
    /// 1. If a legacy `.rally/ledger.jsonl` monolith exists, partition its
    ///    lines into per-engagement segments under `.rally/log/` (key = each
    ///    line's engagement tag if present, else the UTC date from its
    ///    `occurred_at`), then **move** the monolith to
    ///    `.rally/archive/ledger-pre-segment.jsonl`. Every event survives;
    ///    the monolith is preserved verbatim in the archive.
    /// 2. If the union of live segments + archived segments contains more
    ///    events than the current `facts.db`, the db is rebuilt by replaying
    ///    every segment in seq order. The db is a pure cache — never
    ///    canonical.
    /// 3. If no segment / monolith / archive exists but `facts.db` already
    ///    has events, seed a single segment from the db so no history is
    ///    lost on first upgrade.
    /// 4. Otherwise segments and db are already in sync and we proceed.
    ///
    /// Replay, migration, and seed are all idempotent — running them twice
    /// on the same inputs yields identical state.
    pub(crate) fn open_at(root: PathBuf) -> Result<Self> {
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).map_err(RallyError::io("create .rally"))?;
        let _guard = acquire_room_mutation_lock(&dir)?;
        let _ = fs::remove_file(dir.join("room.db"));
        let fact_store_path = dir.join("facts.db");
        let log_dir = dir.join(LOG_DIRNAME);
        let archive_dir = dir.join(ARCHIVE_DIRNAME);
        let legacy_ledger_path = dir.join(LEDGER_FILENAME);

        // R1 → R5 migration (idempotent, see [`migrate_monolith_to_segments`]).
        migrate_monolith_to_segments(&legacy_ledger_path, &log_dir, &archive_dir)?;

        reconcile_segments_and_db(&log_dir, &archive_dir, &fact_store_path)?;

        let fact_store = open_fact_store(&fact_store_path)?;
        let active_engagement = resolve_active_engagement(&dir);
        let store = Self {
            fact_store,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            log_dir,
            archive_dir,
            active_engagement,
        };
        let _ = store.refresh_log_index();
        let _ = store.refresh_index(0);
        Ok(store)
    }

    pub(crate) fn open_existing_at(root: PathBuf) -> Result<Option<Self>> {
        let dir = root.join(".rally");
        let fact_store_path = dir.join("facts.db");
        let log_dir = dir.join(LOG_DIRNAME);
        let archive_dir = dir.join(ARCHIVE_DIRNAME);
        let legacy_ledger_path = dir.join(LEDGER_FILENAME);
        // Existence is determined by ANY canonical input: derived db, live
        // segments, archived segments, or the legacy R1 monolith. A clone
        // carrying only segments OR only the monolith is still a real room.
        let has_segments = read_segment_files(&log_dir).is_ok_and(|v| !v.is_empty());
        let has_archive = read_segment_files(&archive_dir).is_ok_and(|v| !v.is_empty());
        if !fact_store_path.exists()
            && !legacy_ledger_path.exists()
            && !has_segments
            && !has_archive
        {
            return Ok(None);
        }
        let _guard = acquire_room_mutation_lock(&dir)?;
        migrate_monolith_to_segments(&legacy_ledger_path, &log_dir, &archive_dir)?;
        reconcile_segments_and_db(&log_dir, &archive_dir, &fact_store_path)?;
        let fact_store = open_fact_store(&fact_store_path)?;
        let active_engagement = resolve_active_engagement(&dir);
        let store = Self {
            fact_store,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            log_dir,
            archive_dir,
            active_engagement,
        };
        let _ = store.refresh_log_index();
        Ok(Some(store))
    }

    /// Override the active engagement for this RoomStore instance. Used by
    /// `rally enter --engagement <name>` and tests. Persisting to disk so
    /// future opens inherit the label is a separate step ([`persist_active_engagement`]).
    #[cfg(test)]
    pub(crate) fn set_active_engagement_for_test(&mut self, engagement: &str) {
        self.active_engagement = engagement.to_string();
    }

    /// The engagement label currently being stamped on appends.
    pub(crate) fn active_engagement(&self) -> &str {
        &self.active_engagement
    }

    /// Path of the segment file the next append will land in.
    pub(crate) fn active_segment_path(&self) -> PathBuf {
        self.log_dir
            .join(format!("{}.jsonl", self.active_engagement))
    }

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        let fact_store = open_fact_store(&self.facts_db_path)?;
        let mut fact = fact.clone();
        let event_type = fact.kind.as_str().to_string();
        let payload = serde_json::to_value(&fact).map_err(RallyError::json("render fact"))?;
        // The room lock serializes Rally writers; keep a short retry for
        // transient SQLite lock errors from readers or older Rally binaries.
        let result = {
            let jitter = (std::process::id() % 17) as u64;
            let mut attempts = 0;
            loop {
                match fact_store.append(vec![NewEvent::new(event_type.clone(), payload.clone())]) {
                    Ok(r) => break r,
                    Err(err) if attempts < 16 && is_db_locked(&err) => {
                        attempts += 1;
                        thread::sleep(Duration::from_millis(15 * attempts + jitter));
                    }
                    Err(err) => return Err(RallyError::Message(format!("append fact: {err}"))),
                }
            }
        };
        fact.seq = i64::try_from(result.last_sequence_number)
            .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
        append_segment_line(
            &self.active_segment_path(),
            &LedgerLine {
                seq: fact.seq,
                occurred_at: now_string(),
                event_type,
                payload,
                engagement: Some(self.active_engagement.clone()),
            },
        )?;
        // Both index refreshes are best-effort caches; swallow failures so a
        // racing parallel writer never poisons the append path. Replay
        // rebuilds them on next open from segments.
        let _ = self.refresh_log_index();
        let _ = self.refresh_index(fact.seq);
        Ok(fact)
    }

    // -------------------------------------------------------------------------
    // R9-readback: canonical-ledger verification after every mutation
    // -------------------------------------------------------------------------

    /// The engagement label (room id) currently being stamped on appends.
    /// Exposed for R9 readback output in command results.
    pub(crate) fn room_id(&self) -> &str {
        &self.active_engagement
    }

    /// Append `fact` and immediately re-read the CANONICAL SEGMENTS (not
    /// `facts.db`) to assert the returned `event_id` is actually present.
    ///
    /// This catches the silent-corruption class: stale-binary write-drop,
    /// no-op release, wrong-room write. `facts.db` is a DERIVED cache and is
    /// deliberately NOT consulted here — reading it would false-pass a scenario
    /// where the segment write silently dropped but the db write succeeded.
    ///
    /// Returns the verified `Fact` (with `seq` populated) on success.
    /// Returns `Err` with a clear message if the `event_id` is absent from
    /// the canonical segment record after write.
    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<Fact> {
        let appended = self.append_fact(fact)?;
        let event_id = &appended.event_id;

        // Re-read the canonical segments (live + archive) and scan every line
        // for the exact event_id we just appended.
        let live_segments = read_segment_files(&self.log_dir)?;
        let archive_segments = read_segment_files(&self.archive_dir)?;

        let found = segment_event_id_present(
            live_segments.iter().chain(archive_segments.iter()),
            event_id,
        )?;

        if !found {
            return Err(RallyError::Message(format!(
                "readback failed: {event_id} not found in canonical ledger after append"
            )));
        }

        Ok(appended)
    }

    /// For `release` and `resolve` facts: enforce that `--ref` names a live
    /// target, write via `append_fact_verified`, then re-`snapshot()` to confirm
    /// the state transition actually took effect.
    ///
    /// * `release` requires the referenced `event_id` to have been an active
    ///   claim (no longer in `active_claims` after the write).
    /// * `resolve` requires the referenced `event_id` to have been an active
    ///   blocker/risk/handoff/claim (no longer un-resolved after the write).
    ///
    /// Returns the verified `Fact` on success, or a loud error with the reason.
    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<Fact> {
        let ref_id = fact.ref_id.as_deref().ok_or_else(|| {
            RallyError::Usage(format!(
                "{} requires --ref <event-id> targeting a live fact; none provided",
                fact.kind.as_str()
            ))
        })?;

        // Assert the target is live BEFORE writing.
        let snapshot_before = self.snapshot()?;
        match fact.kind {
            FactKind::Release => {
                // A release must reference an active claim (or resolve any fact by
                // event_id).  We check the broader "is this event_id currently
                // un-released" by looking at active_claims.
                let is_live = snapshot_before
                    .active_claims
                    .iter()
                    .any(|c| c.event_id == ref_id);
                if !is_live {
                    return Err(RallyError::Usage(format!(
                        "release failed: ref {ref_id} is not an active claim (already released, never existed, or invalid); nothing to release"
                    )));
                }
            }
            FactKind::Resolve => {
                // Resolve must reference a live blocker, risk, handoff, claim,
                // or an unconsumed artifact.  Artifacts are consumed by resolve
                // (via the `consumed_refs` projection) which drops them from
                // `unconsumed_artifacts`.
                let is_live = snapshot_before
                    .active_blockers
                    .iter()
                    .any(|f| f.event_id == ref_id)
                    || snapshot_before
                        .active_claims
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_before
                        .open_handoffs
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_before
                        .current_risks
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_before
                        .unconsumed_artifacts
                        .iter()
                        .any(|f| f.event_id == ref_id);
                if !is_live {
                    return Err(RallyError::Usage(format!(
                        "resolve failed: ref {ref_id} is not a live blocker, claim, handoff, risk, or unconsumed artifact (already resolved, never existed, or invalid); nothing to resolve"
                    )));
                }
            }
            _ => {}
        }

        // Write + canonical readback.
        let appended = self.append_fact_verified(fact)?;

        // Assert the projected status flipped.
        let snapshot_after = self.snapshot()?;
        match fact.kind {
            FactKind::Release => {
                let still_active = snapshot_after
                    .active_claims
                    .iter()
                    .any(|c| c.event_id == ref_id);
                if still_active {
                    return Err(RallyError::Message(format!(
                        "release readback failed: {ref_id} is still in active_claims after release — the release fact was recorded but the projection did not flip; this is a corruption signal"
                    )));
                }
            }
            FactKind::Resolve => {
                let still_active = snapshot_after
                    .active_blockers
                    .iter()
                    .any(|f| f.event_id == ref_id)
                    || snapshot_after
                        .active_claims
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_after
                        .open_handoffs
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_after
                        .current_risks
                        .iter()
                        .any(|f| f.event_id == ref_id)
                    || snapshot_after
                        .unconsumed_artifacts
                        .iter()
                        .any(|f| f.event_id == ref_id);
                if still_active {
                    return Err(RallyError::Message(format!(
                        "resolve readback failed: {ref_id} is still active after resolve — the resolve fact was recorded but the projection did not flip; this is a corruption signal"
                    )));
                }
            }
            _ => {}
        }

        Ok(appended)
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<Option<Fact>> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        let fact_store = open_fact_store(&self.facts_db_path)?;
        let mut fact = fact.clone();
        let payload =
            serde_json::to_value(&fact).map_err(RallyError::json("render session fact"))?;
        let result = fact_store.append_if(
            vec![NewEvent::new("session", payload.clone())],
            &FactQuery::for_event_types(["session"]),
            expected_context_version,
        );
        match result {
            Ok(result) => {
                fact.seq = i64::try_from(result.last_sequence_number).map_err(|err| {
                    RallyError::Message(format!("sequence number overflow: {err}"))
                })?;
                append_segment_line(
                    &self.active_segment_path(),
                    &LedgerLine {
                        seq: fact.seq,
                        occurred_at: now_string(),
                        event_type: "session".to_string(),
                        payload,
                        engagement: Some(self.active_engagement.clone()),
                    },
                )?;
                let _ = self.refresh_log_index();
                let _ = self.refresh_index(fact.seq);
                Ok(Some(fact))
            }
            Err(EventStoreError::ConditionalAppendConflict { .. }) => Ok(None),
            Err(err) => Err(RallyError::Message(format!("append session fact: {err}"))),
        }
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        let query = self
            .fact_store
            .query(&FactQuery::all())
            .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
        query
            .event_records
            .into_iter()
            .map(|record| {
                let seq = i64::try_from(record.sequence_number).map_err(|err| {
                    RallyError::Message(format!("sequence number overflow: {err}"))
                })?;
                Fact::from_value(record.payload, seq)
            })
            .collect()
    }

    pub(crate) fn session_facts_with_context_version(&self) -> Result<(Vec<Fact>, Option<u64>)> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        let fact_store = open_fact_store(&self.facts_db_path)?;
        let query = fact_store
            .query(&FactQuery::for_event_types(["session"]))
            .map_err(|err| RallyError::Message(format!("query session facts: {err}")))?;
        let context_version = query
            .event_records
            .last()
            .map(|record| record.sequence_number);
        let facts = query
            .event_records
            .into_iter()
            .map(|record| {
                let seq = i64::try_from(record.sequence_number).map_err(|err| {
                    RallyError::Message(format!("sequence number overflow: {err}"))
                })?;
                Fact::from_value(record.payload, seq)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((facts, context_version))
    }

    pub(crate) fn snapshot(&self) -> Result<RoomSnapshot> {
        let facts = self.facts()?;
        Ok(snapshot_from_facts(&facts))
    }

    /// Return the current read cursor for `tool`.
    ///
    /// R10 ledger-first: if the ledger contains a `FactKind::Read` checkpoint
    /// for this tool, that value is the source of truth (durable, survives
    /// `cursors.json` deletion). Falls back to `cursors.json` only when no
    /// ledger checkpoint exists, preserving backwards compatibility.
    pub(crate) fn cursor_for(&self, tool: &str) -> Result<i64> {
        let ledger_seq = self.last_checkpoint_seq(tool)?;
        if ledger_seq > 0 {
            return Ok(ledger_seq);
        }
        Ok(self.read_cursors()?.get(tool).copied().unwrap_or(0))
    }

    pub(crate) fn set_cursor(&self, tool: &str, seq: i64) -> Result<()> {
        let mut cursors = self.read_cursors()?;
        cursors.insert(tool.to_string(), seq);
        if let Some(parent) = self.cursor_path.parent() {
            fs::create_dir_all(parent)
                .map_err(RallyError::io(format!("create {}", parent.display())))?;
        }
        let content = serde_json::to_string_pretty(&json!({
            "updated_at": now_string(),
            "cursors": cursors
        }))
        .map_err(RallyError::json("render cursors"))?;
        let temp_path = self
            .cursor_path
            .with_extension(format!("json.tmp-{}", short_id()));
        fs::write(&temp_path, content)
            .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
        fs::rename(&temp_path, &self.cursor_path).map_err(|err| {
            let _ = fs::remove_file(&temp_path);
            RallyError::Io {
                context: format!(
                    "replace {} with {}",
                    self.cursor_path.display(),
                    temp_path.display()
                ),
                source: err,
            }
        })
    }

    fn read_cursors(&self) -> Result<BTreeMap<String, i64>> {
        if !self.cursor_path.exists() {
            return Ok(BTreeMap::new());
        }
        let text = fs::read_to_string(&self.cursor_path).map_err(RallyError::io(format!(
            "read {}",
            self.cursor_path.display()
        )))?;
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Ok(BTreeMap::new());
        };
        let Some(cursors) = value.get("cursors").and_then(Value::as_object) else {
            return Ok(BTreeMap::new());
        };
        Ok(cursors
            .iter()
            .filter_map(|(tool, seq)| seq.as_i64().map(|seq| (tool.clone(), seq)))
            .collect())
    }

    fn refresh_index(&self, last_seen_seq: i64) -> Result<()> {
        refresh_room_index(&self.repo_root, &self.facts_db_path, last_seen_seq)
    }

    // -------------------------------------------------------------------------
    // R10: read-checkpoint ledger facts
    // -------------------------------------------------------------------------

    /// Return the highest `read_seq` recorded in `FactKind::Read` checkpoint
    /// facts for `tool`, or 0 if none exist.
    ///
    /// The read-seq is encoded in the fact's `summary` field as `"read_seq:<N>"`.
    pub(crate) fn last_checkpoint_seq(&self, tool: &str) -> Result<i64> {
        let query = self
            .fact_store
            .query(&FactQuery::for_event_types(["read"]))
            .map_err(|err| RallyError::Message(format!("query read checkpoints: {err}")))?;
        let max = query
            .event_records
            .into_iter()
            .filter_map(|record| {
                let seq = i64::try_from(record.sequence_number).ok()?;
                let fact = Fact::from_value(record.payload, seq).ok()?;
                if fact.tool.as_deref() != Some(tool) {
                    return None;
                }
                fact.summary
                    .as_deref()
                    .and_then(|s| s.strip_prefix("read_seq:"))
                    .and_then(|n| n.parse::<i64>().ok())
            })
            .max()
            .unwrap_or(0);
        Ok(max)
    }

    /// Append a `FactKind::Read` checkpoint for `tool` recording that it has
    /// read up to `read_seq`, BUT ONLY IF `read_seq` is strictly greater than
    /// the tool's last recorded checkpoint (coalescing guard — no-op polls must
    /// not inflate the ledger).
    ///
    /// Returns `Ok(Some(fact))` when a checkpoint was written, `Ok(None)` when
    /// the read position did not advance beyond the last checkpoint.
    ///
    /// Uses `append_fact` (not `append_fact_verified`) — a dropped checkpoint is
    /// low-stakes metadata and must NOT trigger a second readback (which itself
    /// would be another append and could loop). R9-readback is reserved for
    /// load-bearing state transitions.
    pub(crate) fn maybe_append_read_checkpoint(
        &self,
        tool: &str,
        read_seq: i64,
    ) -> Result<Option<Fact>> {
        let last_checkpoint = self.last_checkpoint_seq(tool)?;
        if read_seq <= last_checkpoint {
            // No advancement — coalesce.
            return Ok(None);
        }
        let fact = Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: crate::new_id("read"),
            seq: 0,
            thread_id: format!(
                "read-{}",
                tool.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect::<String>()
            ),
            kind: FactKind::Read,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("read-checkpoint: {tool} at seq {read_seq}"),
            scope: Vec::new(),
            created_at: crate::now_string(),
            summary: Some(format!("read_seq:{read_seq}")),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let appended = self.append_fact(&fact)?;
        Ok(Some(appended))
    }

    /// Project per-tool read receipts from `FactKind::Read` checkpoint facts,
    /// merged with `cursors.json` as the fast-path fallback.
    ///
    /// For each tool that has either a read-checkpoint fact OR an entry in
    /// `cursors.json`, emit a `ReadReceipt` with `last_read_seq`, `behind_by`,
    /// and `status`. Read-checkpoint facts take precedence over `cursors.json`
    /// when both exist for the same tool (the ledger is the durable record;
    /// `cursors.json` is the fast-path cache).
    ///
    /// Prefer `snapshot_with_readers` when you also need the full snapshot —
    /// that path loads facts once. This method is the standalone entry point
    /// used by tests and any future caller that only needs receipts.
    #[allow(dead_code)] // used in tests; kept as standalone entry point for future callers
    pub(crate) fn project_read_receipts(&self, max_seq: i64) -> Result<Vec<ReadReceipt>> {
        let facts = self.facts()?;
        self.project_read_receipts_from_facts(&facts, max_seq)
    }

    /// Same as `project_read_receipts` but operates on an already-loaded facts
    /// slice. Used by `snapshot_with_readers` to avoid a second DB round-trip.
    fn project_read_receipts_from_facts(
        &self,
        facts: &[Fact],
        max_seq: i64,
    ) -> Result<Vec<ReadReceipt>> {
        // Collect highest read_seq per tool from checkpoint facts.
        let mut ledger_reads: BTreeMap<String, i64> = BTreeMap::new();
        for fact in facts {
            if fact.kind != "read" {
                continue;
            }
            let Some(tool) = fact.tool.as_deref() else {
                continue;
            };
            let Some(seq) = fact
                .summary
                .as_deref()
                .and_then(|s| s.strip_prefix("read_seq:"))
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            let entry = ledger_reads.entry(tool.to_string()).or_insert(0);
            if seq > *entry {
                *entry = seq;
            }
        }

        // Merge with cursors.json (fast-path cache); ledger takes precedence.
        let cursors = self.read_cursors().unwrap_or_default();
        let mut combined: BTreeMap<String, i64> = cursors;
        for (tool, seq) in ledger_reads {
            let entry = combined.entry(tool).or_insert(0);
            if seq > *entry {
                *entry = seq;
            }
        }

        // Build receipts.
        let receipts = combined
            .into_iter()
            .map(|(tool, last_read_seq)| {
                let behind_by = (max_seq - last_read_seq).max(0);
                let status = if behind_by == 0 {
                    "caught_up".to_string()
                } else {
                    "behind".to_string()
                };
                ReadReceipt {
                    tool,
                    last_read_seq,
                    behind_by,
                    status,
                }
            })
            .collect();
        Ok(receipts)
    }

    /// Variant of `snapshot()` that additionally populates `readers` by
    /// projecting `FactKind::Read` checkpoints. Only called when `--readers`
    /// is passed to `rally room`; the default snapshot leaves `readers` empty
    /// to avoid the extra projection cost on every room query.
    ///
    /// Loads facts ONCE and passes the same slice to both `snapshot_from_facts`
    /// and `project_read_receipts_from_facts` — one DB round-trip instead of two.
    pub(crate) fn snapshot_with_readers(&self) -> Result<RoomSnapshot> {
        let facts = self.facts()?;
        let mut snapshot = snapshot_from_facts(&facts);
        snapshot.readers = self.project_read_receipts_from_facts(&facts, snapshot.max_seq)?;
        Ok(snapshot)
    }
}

fn filter_facts(facts: Vec<Fact>, query: &RoomQuery) -> Vec<Fact> {
    facts
        .into_iter()
        .filter(|fact| query.matches(fact))
        .collect()
}

/// Pure projection of a `RoomSnapshot` from an already-loaded facts slice.
///
/// This is the body formerly inlined in `RoomStore::snapshot`. Extracted so
/// that both `snapshot()` and `snapshot_with_readers()` can call it without
/// loading facts twice (fix #2 — one DB round-trip instead of two).
fn snapshot_from_facts(facts: &[Fact]) -> RoomSnapshot {
    let max_seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);
    // R10: `content_max_seq` is the highest seq of a non-read-checkpoint
    // fact. Used by command_next to derive the read position to record
    // WITHOUT including the read-checkpoint's own seq (which would inflate
    // the position on every poll and create a feedback loop).
    let content_max_seq = facts
        .iter()
        .filter(|f| f.kind != "read")
        .map(|f| f.seq)
        .max()
        .unwrap_or(0);
    // `last_activity_ts`: created_at of the highest-seq fact.  Computed here
    // (from the same slice) so status_global avoids a redundant store.facts() call.
    let last_activity_ts = facts
        .iter()
        .max_by_key(|f| f.seq)
        .map(|f| f.created_at.clone());
    // B13: receipts close handoffs (same projection as resolve).
    let resolved = facts
        .iter()
        .filter(|f| f.kind == "resolve" || f.kind == "release" || f.kind == "receipt")
        .filter_map(|f| f.ref_id.clone())
        .collect::<BTreeSet<_>>();
    let released_scopes = facts
        .iter()
        .filter(|f| f.kind == "release")
        .flat_map(|f| f.scope.clone())
        .collect::<BTreeSet<_>>();
    let active_claims = facts
        .iter()
        .filter(|f| f.kind == "claim")
        .filter(|f| !resolved.contains(&f.event_id))
        .filter(|f| !f.scope.iter().any(|scope| released_scopes.contains(scope)))
        // B18: exclude external-intake facts from repo-local backlog.
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .cloned()
        .collect::<Vec<_>>();
    let active_blockers = facts
        .iter()
        .filter(|f| f.kind == "blocker")
        .filter(|f| !resolved.contains(&f.event_id))
        .cloned()
        .collect::<Vec<_>>();
    let artifact_consumed_handoffs = facts
        .iter()
        .filter(|f| f.kind == "artifact")
        .filter_map(|f| f.ref_id.clone())
        .collect::<BTreeSet<_>>();
    let open_handoffs = facts
        .iter()
        .filter(|f| f.kind == "handoff")
        .filter(|f| !resolved.contains(&f.event_id))
        .filter(|f| !artifact_consumed_handoffs.contains(&f.event_id))
        // B18: exclude external-intake facts from repo-local backlog.
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .cloned()
        .collect::<Vec<_>>();
    let current_decisions = facts
        .iter()
        .filter(|f| f.kind == "decision")
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    let current_risks = facts
        .iter()
        .filter(|f| f.kind == "risk")
        .filter(|f| !resolved.contains(&f.event_id))
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    let recent_artifacts = facts
        .iter()
        .filter(|f| f.kind == "artifact")
        // B18: exclude external-intake facts from repo-local backlog.
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    let consumed_refs = facts
        .iter()
        .filter(|f| f.kind == "handoff" || f.kind == "resolve")
        .filter_map(|f| f.ref_id.clone())
        .collect::<BTreeSet<_>>();
    let unconsumed_artifacts = recent_artifacts
        .iter()
        .filter(|f| !consumed_refs.contains(&f.event_id))
        .cloned()
        .collect::<Vec<_>>();

    // --- Presence projection ---
    // Collect the highest-seq fact per tool (any kind counts; presence is
    // the primary signal but a claim or artifact also proves presence).
    // "rally" is the reserved system author (used by wake_fact); it is not
    // a participating agent and must not appear in squads[].
    let mut tool_last: BTreeMap<String, (i64, String)> = BTreeMap::new();
    for fact in facts {
        if let Some(tool) = &fact.tool {
            if tool == "rally" {
                continue;
            }
            let entry = tool_last.entry(tool.clone()).or_insert((0, String::new()));
            if fact.seq > entry.0 {
                *entry = (fact.seq, fact.created_at.clone());
            }
        }
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let acked = acknowledged_tools(facts);
    let squads = tool_last
        .into_iter()
        .map(|(tool, (seq, ts))| {
            // Parse ISO-8601 ts to epoch secs for idle check; fall back to
            // treating the tool as active if parsing fails.
            let seen_secs = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|dt| dt.timestamp())
                .unwrap_or(now_secs);
            let status = if now_secs - seen_secs <= IDLE_THRESHOLD_SECS {
                "active".to_string()
            } else {
                "idle".to_string()
            };
            let acknowledged = acked.contains(&tool);
            Squad {
                tool,
                last_seen_seq: seq,
                last_seen_ts: ts,
                status,
                acknowledged,
            }
        })
        .collect::<Vec<_>>();

    // Lead is the tool from the most-recent decision with subject "role:lead".
    // Lead = the tool of the latest `role:lead` decision, UNLESS the latest
    // lead-family decision is a `role:lead:relinquished` (seat reopened → None).
    let lead = facts
        .iter()
        .filter(|f| {
            f.kind == "decision"
                && (f.subject == "role:lead" || f.subject == "role:lead:relinquished")
        })
        .max_by_key(|f| f.seq)
        .filter(|f| f.subject == "role:lead")
        .and_then(|f| f.tool.clone());

    // Mission: latest-by-seq Mission fact whose scope contains "mission".
    // "mission" scope distinguishes north-star facts from envelope facts.
    let mission = facts
        .iter()
        .filter(|f| f.kind == "mission" && f.scope.iter().any(|s| s == "mission"))
        .max_by_key(|f| f.seq)
        .map(|f| f.subject.clone());

    RoomSnapshot {
        max_seq,
        content_max_seq,
        last_activity_ts,
        active_claims,
        active_blockers,
        open_handoffs,
        current_decisions,
        current_risks,
        recent_artifacts,
        unconsumed_artifacts,
        stale_facts: Vec::new(),
        squads,
        lead,
        readers: Vec::new(),
        mission,
    }
}

fn open_fact_store(path: &Path) -> Result<SqliteStore> {
    // Per-process jitter de-synchronizes concurrent retriers (the thundering-herd
    // cure); budget raised for write-burst tolerance (B-write-burst-scale).
    let jitter = (std::process::id() % 17) as u64;
    let mut attempts = 0;
    loop {
        match SqliteStore::open(path) {
            Ok(store) => return Ok(store),
            Err(err)
                if attempts < 16 && (is_bootstrap_metadata_race(&err) || is_db_locked(&err)) =>
            {
                attempts += 1;
                thread::sleep(Duration::from_millis(20 * attempts + jitter));
            }
            Err(err) => return Err(RallyError::Message(format!("open fact store: {err}"))),
        }
    }
}

fn is_bootstrap_metadata_race(err: &impl std::fmt::Display) -> bool {
    err.to_string()
        .contains("UNIQUE constraint failed: store_metadata.key")
}

fn is_db_locked(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string();
    msg.contains("database is locked") || msg.contains("code: 5")
}

// =============================================================================
// R5: per-engagement segment ledger
// =============================================================================
//
// The "ledger" is now a set of files: `.rally/log/<engagement>.jsonl` for live
// segments plus `.rally/archive/<engagement>.jsonl` for rotated/migrated ones.
// Each line has the same `LedgerLine` shape as the R1 monolith. Replaying every
// line in **seq order** rebuilds `facts.db`. The replay is concat-and-sort —
// segment file names don't have to match append order, only the per-line seqs.

/// Resolve the engagement label used to stamp new appends.
///
/// Priority:
/// 1. `RALLY_ENGAGEMENT` env var (non-empty after trim, sanitised).
/// 2. `.rally/active-engagement` file (one line, sanitised).
/// 3. UTC date `YYYY-MM-DD` from the current clock.
///
/// Sanitisation strips path separators and trims whitespace so a label can
/// never escape the log dir. The fallback never fails — if the clock returns
/// something exotic, `"unknown-engagement"` is used.
fn resolve_active_engagement(rally_dir: &Path) -> String {
    if let Ok(value) = env::var(ENGAGEMENT_ENV_VAR) {
        let cleaned = sanitise_engagement(&value);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    let active_path = rally_dir.join(ACTIVE_ENGAGEMENT_FILENAME);
    if let Ok(text) = fs::read_to_string(&active_path) {
        let cleaned = sanitise_engagement(text.trim());
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    utc_date_label()
}

/// Public wrapper so `command_watch` in lib.rs can resolve the engagement
/// label without opening a full RoomStore (cheap, no db access needed).
pub(crate) fn resolve_active_engagement_pub(rally_dir: &Path) -> String {
    resolve_active_engagement(rally_dir)
}

/// Persist an engagement label so subsequent rally invocations inherit it.
/// Used by `rally enter --engagement <name>`. Idempotent — writing the same
/// label is a no-op.
pub(crate) fn persist_active_engagement(rally_dir: &Path, engagement: &str) -> Result<()> {
    let cleaned = sanitise_engagement(engagement);
    if cleaned.is_empty() {
        return Err(RallyError::Usage(format!(
            "engagement label {engagement:?} is empty after sanitising"
        )));
    }
    fs::create_dir_all(rally_dir)
        .map_err(RallyError::io(format!("create {}", rally_dir.display())))?;
    let target = rally_dir.join(ACTIVE_ENGAGEMENT_FILENAME);
    if let Ok(existing) = fs::read_to_string(&target)
        && existing.trim() == cleaned
    {
        return Ok(());
    }
    let temp_path = target.with_extension(format!("tmp-{}", short_id()));
    fs::write(&temp_path, format!("{cleaned}\n"))
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    fs::rename(&temp_path, &target).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        RallyError::Io {
            context: format!("replace {} with {}", target.display(), temp_path.display()),
            source: err,
        }
    })
}

/// Strip path separators + leading/trailing whitespace so an engagement label
/// can't escape the log directory or trip on shell quoting.
fn sanitise_engagement(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .collect()
}

/// UTC date `YYYY-MM-DD` from `chrono::Utc::now()`.
fn utc_date_label() -> String {
    // chrono::Utc is already a dep (lib.rs uses it for `now_string`); avoid
    // pulling another crate.
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Reconcile the canonical segment set with the derived sqlite cache.
///
/// Called on every `RoomStore::open_at` / `open_existing_at`. The contract is:
///
/// * Segments ahead of db (incl. db absent) → rebuild db by replaying segments.
/// * Segments absent but db has events → seed one segment from db (first-run
///   upgrade from a pre-R1 db that never had a ledger).
/// * Both empty, or in sync → no-op.
///
/// Idempotent: running twice yields the same state.
fn reconcile_segments_and_db(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
) -> Result<()> {
    let segments = read_segment_files(log_dir)?;
    // Replay walks live segments + rotated archive segments, but NOT the R5
    // migration monolith: post-migration its events already live verbatim in
    // the live segments, so counting/replaying it double-counts every event
    // (see [`replay_archive_segments`]).
    let archived = replay_archive_segments(archive_dir)?;

    // The canonical record is the *set of distinct seqs* across replay sources.
    // The cache is fresh iff it holds exactly that many events — replay
    // reassigns factstr seqs 1..N, so `db_event_count == distinct_replay_seqs`
    // exactly when in sync. Comparing distinct-seq COUNT (not max seq, not raw
    // line count) is what makes this correct under (a) seq gaps from rotation
    // and (b) the same seq appearing in two files (archive + live).
    let canonical_count = distinct_segment_seqs(&segments, &archived)?;
    let db_count = read_db_event_count(facts_db_path)?;

    if canonical_count == 0 && db_count == 0 {
        return Ok(());
    }

    if canonical_count == 0 && db_count > 0 {
        // No segments yet but the db has events: first-run upgrade from a
        // pre-segment install. Seed a segment so the canonical record exists.
        seed_segment_from_db(log_dir, facts_db_path)?;
        return Ok(());
    }

    if canonical_count != db_count {
        // Segment set and cache disagree on event count → cache is stale (or
        // absent). Rebuild it from the canonical segments. Replay is a pure
        // function of the deduped segment set, so this is idempotent.
        rebuild_db_from_segments(&segments, &archived, facts_db_path)?;
        return Ok(());
    }

    // canonical_count == db_count > 0 → cache is fresh; leave it untouched.
    Ok(())
}

/// Sorted segment file paths in a directory. Empty / missing dir → empty Vec.
fn read_segment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).map_err(RallyError::io(format!("read_dir {}", dir.display())))? {
        let entry = entry.map_err(RallyError::io(format!("readdir entry {}", dir.display())))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.contains(".tmp-")
        {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

/// R9-readback: scan segment files for the presence of a specific `event_id`
/// in any `LedgerLine.payload.event_id` field.  Returns `true` if found.
///
/// Reads each line of each segment file; parses as `LedgerLine`; deserializes
/// `payload` as a minimal struct that exposes `event_id`.  Uses the segment
/// *files* as the authoritative source — never `facts.db`.
fn segment_event_id_present<'a>(
    paths: impl Iterator<Item = &'a PathBuf>,
    event_id: &str,
) -> Result<bool> {
    for path in paths {
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<LedgerLine>(&line) else {
                continue;
            };
            // The payload is a serialized Fact.  Extract event_id without a
            // full Fact deserialization to keep this path allocation-light.
            if entry.payload.get("event_id").and_then(|v| v.as_str()) == Some(event_id) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Raw count of non-empty lines across the given segment files. Test-only:
/// production reconcile compares *distinct* seqs (see [`distinct_segment_seqs`]),
/// but tests assert on physical line counts to verify on-disk layout.
#[cfg(test)]
fn count_segment_events(paths: &[PathBuf]) -> Result<i64> {
    let mut total = 0i64;
    for path in paths {
        let file =
            fs::File::open(path).map_err(RallyError::io(format!("read {}", path.display())))?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(RallyError::io(format!("read {}", path.display())))?;
            if !line.trim().is_empty() {
                total += 1;
            }
        }
    }
    Ok(total)
}

/// Archive segments eligible for **replay**, i.e. every rotated
/// `<engagement>.jsonl` segment but NOT the R5 migration monolith
/// `ledger-pre-segment.jsonl`. The monolith's events are already present
/// verbatim in the live segments after migration; replaying it would
/// double-count every event (inflating the reconcile trigger) without adding
/// any history. Rotated segments keep their original `<engagement>.jsonl`
/// name (see `rotate.rs`), so a filename-constant match cleanly separates the
/// two — only the constant-named monolith is excluded.
fn replay_archive_segments(archive_dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(read_segment_files(archive_dir)?
        .into_iter()
        .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some(ARCHIVED_MONOLITH_FILENAME))
        .collect())
}

/// Number of *distinct* sequence numbers across the replay sources. This is
/// the canonical event count: the same seq appearing in two files (e.g. a
/// rotated segment and a stray copy) counts once, and gaps in the seq range
/// don't inflate it. Used to decide whether the derived cache is in sync.
fn distinct_segment_seqs(live: &[PathBuf], archived: &[PathBuf]) -> Result<i64> {
    let mut seqs: BTreeSet<i64> = BTreeSet::new();
    for path in live.iter().chain(archived.iter()) {
        let file =
            fs::File::open(path).map_err(RallyError::io(format!("read {}", path.display())))?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(RallyError::io(format!("read {}", path.display())))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: LedgerLine = serde_json::from_str(&line)
                .map_err(RallyError::json(format!("parse {}", path.display())))?;
            seqs.insert(entry.seq);
        }
    }
    i64::try_from(seqs.len())
        .map_err(|err| RallyError::Message(format!("distinct seq count overflow: {err}")))
}

/// Number of events currently held by the derived sqlite cache. Compared
/// against [`distinct_segment_seqs`] to detect a stale/absent cache. Returns
/// 0 when the db file does not exist.
fn read_db_event_count(facts_db_path: &Path) -> Result<i64> {
    if !facts_db_path.exists() {
        return Ok(0);
    }
    let store = open_fact_store(facts_db_path)?;
    // TODO(perf): O(N) full load — replace with count() when factstr exposes one.
    let query = store
        .query(&FactQuery::all())
        .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
    i64::try_from(query.event_records.len())
        .map_err(|err| RallyError::Message(format!("event count overflow: {err}")))
}

/// Rebuild the derived sqlite cache by replaying every segment line in seq
/// order (live segments first, then archive — the union — sorted by seq).
/// Dedup by `sequence_number` (re-running migration twice can otherwise
/// duplicate). Hard error on conflict (two different payloads at the same
/// seq is corruption, not noise).
///
/// Replay is a **pure function of the deduped event set**: each surviving
/// line is appended in seq order and factstr assigns fresh monotonic seqs
/// 1..N. We do NOT assert the reassigned seq equals the stored seq — after
/// rotation or any historical gap the stored seqs are not contiguous from 1,
/// and contiguity is not required for the cache to faithfully reflect the
/// canonical record. Ordering (sort-by stored seq) is what we preserve.
fn rebuild_db_from_segments(
    live: &[PathBuf],
    archived: &[PathBuf],
    facts_db_path: &Path,
) -> Result<()> {
    let _ = fs::remove_file(facts_db_path);
    let _ = fs::remove_file(facts_db_path.with_extension("db-shm"));
    let _ = fs::remove_file(facts_db_path.with_extension("db-wal"));

    let mut all_entries: Vec<LedgerLine> = Vec::new();
    for path in live.iter().chain(archived.iter()) {
        let file =
            fs::File::open(path).map_err(RallyError::io(format!("read {}", path.display())))?;
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(RallyError::io(format!("read {}", path.display())))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: LedgerLine = serde_json::from_str(&line).map_err(RallyError::json(
                format!("parse {} line {}", path.display(), idx + 1),
            ))?;
            all_entries.push(entry);
        }
    }
    all_entries.sort_by_key(|e| e.seq);

    // Dedup by seq in-place (keep first occurrence); hard-error if two seqs
    // disagree on payload.  Operates on the same Vec to avoid a second allocation.
    let mut write = 0usize;
    for read in 0..all_entries.len() {
        if write > 0 && all_entries[write - 1].seq == all_entries[read].seq {
            if all_entries[write - 1].payload != all_entries[read].payload
                || all_entries[write - 1].event_type != all_entries[read].event_type
            {
                return Err(RallyError::Message(format!(
                    "segment replay conflict at seq {}: two distinct events recorded with the same sequence number",
                    all_entries[read].seq
                )));
            }
            // duplicate — skip
        } else {
            if read != write {
                all_entries.swap(read, write);
            }
            write += 1;
        }
    }
    all_entries.truncate(write);

    let store = open_fact_store(facts_db_path)?;
    for entry in &all_entries {
        store
            .append(vec![NewEvent::new(
                entry.event_type.clone(),
                entry.payload.clone(),
            )])
            .map_err(|err| RallyError::Message(format!("replay segments: {err}")))?;
    }
    Ok(())
}

/// Seed a single segment file from the existing db when no segment exists
/// yet. Used as a forward-compat path: a pre-R1 install that only had
/// `facts.db` still ends up with a canonical segment record.
fn seed_segment_from_db(log_dir: &Path, facts_db_path: &Path) -> Result<()> {
    let store = open_fact_store(facts_db_path)?;
    let query = store
        .query(&FactQuery::all())
        .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
    fs::create_dir_all(log_dir).map_err(RallyError::io(format!("create {}", log_dir.display())))?;
    let seed_label = utc_date_label();
    let target = log_dir.join(format!("{seed_label}.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&target)
        .map_err(RallyError::io(format!("create {}", target.display())))?;
    for record in query.event_records {
        let seq = i64::try_from(record.sequence_number)
            .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
        let entry = LedgerLine {
            seq,
            occurred_at: record.occurred_at.to_string(),
            event_type: record.event_type,
            payload: record.payload,
            engagement: Some(seed_label.clone()),
        };
        let line =
            serde_json::to_string(&entry).map_err(RallyError::json("render segment line"))?;
        writeln!(file, "{line}").map_err(RallyError::io(format!("write {}", target.display())))?;
    }
    file.sync_all()
        .map_err(RallyError::io(format!("fsync {}", target.display())))?;
    Ok(())
}

/// Append a single line to a segment file. Path/payload format identical to
/// the R1 monolith; only the *location* moved.
fn append_segment_line(segment_path: &Path, entry: &LedgerLine) -> Result<()> {
    if let Some(parent) = segment_path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let line = serde_json::to_string(entry).map_err(RallyError::json("render segment line"))?;
    // Append `line\n` as a single write(2) call so that O_APPEND atomicity
    // prevents interleaving with concurrent writers. writeln!(file, "{line}")
    // expands to write_fmt which issues two separate write() calls (content
    // then '\n'), allowing another process's bytes to land between them and
    // corrupt the JSONL record. write_all issues a single syscall.
    let record = format!("{line}\n");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(segment_path)
        .map_err(RallyError::io(format!("open {}", segment_path.display())))?;
    file.write_all(record.as_bytes())
        .map_err(RallyError::io(format!("write {}", segment_path.display())))?;
    file.sync_data()
        .map_err(RallyError::io(format!("fsync {}", segment_path.display())))?;
    Ok(())
}

/// One-time partition of the R1 `.rally/ledger.jsonl` monolith into
/// per-engagement segments under `.rally/log/`, then **move** the monolith to
/// `.rally/archive/ledger-pre-segment.jsonl`. Idempotent — running twice on
/// already-migrated state is a no-op.
///
/// Partition key for each row: persisted `engagement` field if present (R5
/// rows in a mixed monolith), else the UTC date from `occurred_at`. Rows
/// with an unparseable `occurred_at` are filed under `"undated"`.
///
/// Every row of the monolith is preserved verbatim — also retained in the
/// archive copy as a belt-and-braces guarantee.
fn migrate_monolith_to_segments(
    legacy_ledger_path: &Path,
    log_dir: &Path,
    archive_dir: &Path,
) -> Result<()> {
    if !legacy_ledger_path.exists() {
        return Ok(());
    }
    let archived_target = archive_dir.join(ARCHIVED_MONOLITH_FILENAME);
    if archived_target.exists() {
        // Migration already happened (rerun, or someone left both files
        // somehow). Either way: ensure live segments contain the events.
        // If the archive exists and the monolith still exists, the previous
        // run died after writing segments but before moving the monolith —
        // we can safely delete the monolith.
        let _ = fs::remove_file(legacy_ledger_path);
        return Ok(());
    }

    fs::create_dir_all(log_dir).map_err(RallyError::io(format!("create {}", log_dir.display())))?;
    fs::create_dir_all(archive_dir)
        .map_err(RallyError::io(format!("create {}", archive_dir.display())))?;

    // Partition pass.
    let file = fs::File::open(legacy_ledger_path).map_err(RallyError::io(format!(
        "read {}",
        legacy_ledger_path.display()
    )))?;
    let mut by_engagement: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(RallyError::io(format!(
            "read {}",
            legacy_ledger_path.display()
        )))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: LedgerLine = serde_json::from_str(&line).map_err(RallyError::json(format!(
            "parse {} line {}",
            legacy_ledger_path.display(),
            idx + 1
        )))?;
        let key = entry.engagement.clone().unwrap_or_else(|| {
            // Default key = UTC date from occurred_at, else "undated".
            extract_date_prefix(&entry.occurred_at).unwrap_or_else(|| "undated".to_string())
        });
        by_engagement.entry(key).or_default().push(line);
    }

    // Atomic write per partition: write to tmp file, rename into place. If a
    // segment for the same engagement already exists (rerun under partial
    // failure), append rather than truncate.
    for (engagement, lines) in &by_engagement {
        let segment_path = log_dir.join(format!("{engagement}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&segment_path)
            .map_err(RallyError::io(format!("open {}", segment_path.display())))?;
        for line in lines {
            writeln!(file, "{line}")
                .map_err(RallyError::io(format!("write {}", segment_path.display())))?;
        }
        file.sync_data()
            .map_err(RallyError::io(format!("fsync {}", segment_path.display())))?;
    }

    // Move the monolith into the archive verbatim.
    fs::rename(legacy_ledger_path, &archived_target).map_err(RallyError::io(format!(
        "move {} -> {}",
        legacy_ledger_path.display(),
        archived_target.display()
    )))?;
    Ok(())
}

/// Pull a `YYYY-MM-DD` prefix off a RFC3339 timestamp, or None if the input
/// doesn't look like one.
fn extract_date_prefix(occurred_at: &str) -> Option<String> {
    let head = occurred_at.get(..10)?;
    let bytes = head.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let ok = bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !ok {
        return None;
    }
    Some(head.to_string())
}

impl RoomStore {
    /// Refresh `.rally/log/index.json` from the current segment set.
    /// Best-effort — failure does not block reads or appends.
    fn refresh_log_index(&self) -> Result<()> {
        let segments = read_segment_files(&self.log_dir)?;
        let archived = read_segment_files(&self.archive_dir)?;
        let mut entries = Vec::new();
        for path in segments.iter().chain(archived.iter()) {
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let file =
                fs::File::open(path).map_err(RallyError::io(format!("read {}", path.display())))?;
            let mut first_seq = i64::MAX;
            let mut last_seq = 0i64;
            let mut count = 0i64;
            let mut first_ts: Option<String> = None;
            let mut last_ts: Option<String> = None;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(RallyError::io(format!("read {}", path.display())))?;
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(entry) = serde_json::from_str::<LedgerLine>(&line) else {
                    continue;
                };
                count += 1;
                if entry.seq < first_seq {
                    first_seq = entry.seq;
                    first_ts = Some(entry.occurred_at.clone());
                }
                if entry.seq > last_seq {
                    last_seq = entry.seq;
                    last_ts = Some(entry.occurred_at);
                }
            }
            if count == 0 {
                continue;
            }
            entries.push(SegmentIndexEntry {
                segment: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string(),
                engagement: label,
                first_seq,
                last_seq,
                count,
                first_ts,
                last_ts,
            });
        }

        let index_path = self.log_dir.join(LOG_INDEX_FILENAME);
        fs::create_dir_all(&self.log_dir)
            .map_err(RallyError::io(format!("create {}", self.log_dir.display())))?;
        let segments_value =
            serde_json::to_value(&entries).map_err(RallyError::json("render log index"))?;
        if let Ok(existing_text) = fs::read_to_string(&index_path)
            && let Ok(existing) = serde_json::from_str::<Value>(&existing_text)
            && existing.get("segments") == Some(&segments_value)
        {
            return Ok(());
        }
        let rendered = serde_json::to_string_pretty(
            &json!({"segments": segments_value, "updated_at": now_string()}),
        )
        .map_err(RallyError::json("render log index"))?;
        let rendered = format!("{rendered}\n");
        let temp_path = index_path.with_extension(format!("json.tmp-{}", short_id()));
        fs::write(&temp_path, rendered)
            .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
        match fs::rename(&temp_path, &index_path) {
            Ok(()) => Ok(()),
            // Parallel-writer race: another append refreshed the index
            // between our write and rename, removing our temp file. The
            // peer's index is current; ours is just stale. Treat as no-op.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&temp_path);
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(&temp_path);
                Err(RallyError::Io {
                    context: format!(
                        "replace {} with {}",
                        index_path.display(),
                        temp_path.display()
                    ),
                    source: err,
                })
            }
        }
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-{label}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_fact(event_id: &str, kind: FactKind, scope: &str, summary: &str) -> Fact {
        Fact {
            schema: fact_schema(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: format!("t-{event_id}"),
            kind,
            tool: Some("test".to_string()),
            role: Some("test-role".to_string()),
            subject: format!("subject-{event_id}"),
            scope: vec![scope.to_string()],
            created_at: now_string(),
            summary: Some(summary.to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    fn segments_under(root: &Path) -> Vec<PathBuf> {
        read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap_or_default()
    }

    fn archive_under(root: &Path) -> Vec<PathBuf> {
        read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap_or_default()
    }

    /// R1-era guarantee, ported to R5: the segments under `.rally/log/` are
    /// canonical and `facts.db` is a pure derived cache. Delete the cache,
    /// reopen, and the room must reconstruct identically — same seqs, same
    /// payloads, same snapshot.
    #[test]
    fn round_trip_db_rebuilds_from_segments() {
        let root = unique_root("segments-roundtrip");
        let store = RoomStore::open_at(root.clone()).unwrap();

        let a = store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim a"))
            .unwrap();
        let b = store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided b"))
            .unwrap();
        let c = store
            .append_fact(&make_fact("e3", FactKind::Blocker, "tests/", "blocker c"))
            .unwrap();
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));

        let before_facts = store.facts().unwrap();
        let before_snapshot = store.snapshot().unwrap();
        drop(store);

        // Delete the derived cache. Segments remain.
        let facts_db = root.join(".rally/facts.db");
        let live_segments = segments_under(&root);
        assert!(
            !live_segments.is_empty(),
            "segments must persist as canonical"
        );
        // Sum of live-segment lines = 3 events.
        assert_eq!(count_segment_events(&live_segments).unwrap(), 3);
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));
        assert!(!facts_db.exists(), "cache deleted for replay test");

        // Reopen → reconcile replays segments into a fresh cache.
        let store = RoomStore::open_at(root.clone()).unwrap();
        let after_facts = store.facts().unwrap();
        let after_snapshot = store.snapshot().unwrap();

        assert_eq!(before_facts.len(), after_facts.len());
        for (b, a) in before_facts.iter().zip(after_facts.iter()) {
            assert_eq!(b.seq, a.seq);
            assert_eq!(b.event_id, a.event_id);
            assert_eq!(b.kind.as_str(), a.kind.as_str());
            assert_eq!(b.subject, a.subject);
            assert_eq!(b.scope, a.scope);
        }
        assert_eq!(before_snapshot.max_seq, after_snapshot.max_seq);
        assert_eq!(
            before_snapshot.active_claims.len(),
            after_snapshot.active_claims.len()
        );

        // Idempotency: a second replay (delete cache again, reopen) yields
        // identical state.
        drop(store);
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));
        let store = RoomStore::open_at(root.clone()).unwrap();
        let after2 = store.facts().unwrap();
        assert_eq!(after_facts.len(), after2.len());
        for (x, y) in after_facts.iter().zip(after2.iter()) {
            assert_eq!(x.seq, y.seq);
            assert_eq!(x.event_id, y.event_id);
        }

        fs::remove_dir_all(&root).ok();
    }

    /// First-run upgrade: a pre-existing room with a db but no segments
    /// seeds a segment from the cache so no history is lost.
    #[test]
    fn seed_segment_from_existing_db() {
        let root = unique_root("segments-bootstrap");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim a"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided b"))
            .unwrap();
        drop(store);

        // Simulate "upgraded from a pre-segment version of rally": delete
        // every segment but keep the db. Also remove the index so first-open
        // can't accidentally short-circuit.
        let log_dir = root.join(".rally/log");
        if log_dir.exists() {
            for entry in fs::read_dir(&log_dir).unwrap() {
                let _ = fs::remove_file(entry.unwrap().path());
            }
        }
        assert!(segments_under(&root).is_empty());
        assert!(root.join(".rally/facts.db").exists());

        // Reopen → reconcile seeds a segment from the db.
        let store = RoomStore::open_at(root.clone()).unwrap();
        let segs = segments_under(&root);
        assert_eq!(segs.len(), 1, "exactly one seeded segment");
        assert_eq!(count_segment_events(&segs).unwrap(), 2);

        // Now delete the db and confirm the seeded segment round-trips.
        drop(store);
        let facts_db = root.join(".rally/facts.db");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));

        let store = RoomStore::open_at(root.clone()).unwrap();
        let facts = store.facts().unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].event_id, "e1");
        assert_eq!(facts[1].event_id, "e2");

        fs::remove_dir_all(&root).ok();
    }

    /// R5 round-trip: seed events under TWO different engagement labels, then
    /// blow away the cache and confirm the room reconstructs identically,
    /// from per-engagement segments.
    #[test]
    fn round_trip_two_engagements_reconstruct_from_segments() {
        let root = unique_root("segments-two-engagements");
        let mut store = RoomStore::open_at(root.clone()).unwrap();

        store.set_active_engagement_for_test("alpha");
        let a = store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "alpha claim"))
            .unwrap();
        let b = store
            .append_fact(&make_fact(
                "e2",
                FactKind::Decision,
                "src/",
                "alpha decided",
            ))
            .unwrap();

        store.set_active_engagement_for_test("beta");
        let c = store
            .append_fact(&make_fact(
                "e3",
                FactKind::Blocker,
                "tests/",
                "beta blocker",
            ))
            .unwrap();
        let d = store
            .append_fact(&make_fact(
                "e4",
                FactKind::Resolve,
                "tests/",
                "beta resolved",
            ))
            .unwrap();
        assert_eq!((a.seq, b.seq, c.seq, d.seq), (1, 2, 3, 4));

        let before_facts = store.facts().unwrap();
        drop(store);

        // Two distinct segment files exist.
        let segs = segments_under(&root);
        let names: Vec<String> = segs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"alpha.jsonl".to_string()), "got: {names:?}");
        assert!(names.contains(&"beta.jsonl".to_string()), "got: {names:?}");
        assert_eq!(count_segment_events(&segs).unwrap(), 4);

        // Delete the cache, reopen, reconstruct.
        let facts_db = root.join(".rally/facts.db");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));

        let store = RoomStore::open_at(root.clone()).unwrap();
        let after_facts = store.facts().unwrap();
        assert_eq!(before_facts.len(), after_facts.len());
        for (b, a) in before_facts.iter().zip(after_facts.iter()) {
            assert_eq!(b.seq, a.seq);
            assert_eq!(b.event_id, a.event_id);
            assert_eq!(b.kind.as_str(), a.kind.as_str());
        }

        // Index file written and parseable.
        let index_path = root.join(".rally/log").join(LOG_INDEX_FILENAME);
        assert!(index_path.exists());
        let index_val: Value =
            serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
        assert!(index_val["segments"].is_array());
        assert_eq!(index_val["segments"].as_array().unwrap().len(), 2);

        let index_before_noop_open = fs::read_to_string(&index_path).unwrap();
        drop(store);
        let _store = RoomStore::open_at(root.clone()).unwrap();
        let index_after_noop_open = fs::read_to_string(&index_path).unwrap();
        assert_eq!(
            index_after_noop_open, index_before_noop_open,
            "opening an unchanged room must not dirty the derived segment index"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R1 → R5 migration: a pre-existing monolith on disk gets partitioned
    /// into segments + the monolith moves to archive. Every event survives.
    #[test]
    fn migrates_r1_monolith_into_segments_preserving_all_events() {
        let root = unique_root("segments-migrate");
        // Phase 1: seed the room as if R1 had written every event into the
        // monolith (no segments dir).
        let store = RoomStore::open_at(root.clone()).unwrap();
        for n in 0..10 {
            store
                .append_fact(&make_fact(
                    &format!("e{n}"),
                    FactKind::Decision,
                    "src/",
                    "monolith seed",
                ))
                .unwrap();
        }
        drop(store);

        // Simulate the on-disk state of an R1 install: move every line back
        // into a synthetic `.rally/ledger.jsonl` and remove the segments.
        let log_dir = root.join(".rally/log");
        let monolith_path = root.join(".rally/ledger.jsonl");
        let mut all_lines = Vec::new();
        if log_dir.exists() {
            for entry in fs::read_dir(&log_dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    for line in fs::read_to_string(&path).unwrap().lines() {
                        if !line.trim().is_empty() {
                            all_lines.push(line.to_string());
                        }
                    }
                    fs::remove_file(&path).ok();
                }
            }
        }
        fs::write(&monolith_path, all_lines.join("\n") + "\n").unwrap();
        assert_eq!(all_lines.len(), 10);
        // Also delete the cache so reopen has to migrate + replay.
        let facts_db = root.join(".rally/facts.db");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));

        // Phase 2: reopen. Migration should partition + archive.
        let store = RoomStore::open_at(root.clone()).unwrap();
        let after_facts = store.facts().unwrap();
        assert_eq!(after_facts.len(), 10, "all 10 events preserved");

        // Live segments exist (at least one).
        let segs = segments_under(&root);
        assert!(!segs.is_empty());
        assert_eq!(count_segment_events(&segs).unwrap(), 10);

        // Archive contains the monolith verbatim.
        let archive = archive_under(&root);
        assert_eq!(archive.len(), 1);
        let archived_name = archive[0].file_name().unwrap().to_string_lossy();
        assert_eq!(archived_name, ARCHIVED_MONOLITH_FILENAME);
        assert_eq!(count_segment_events(&archive).unwrap(), 10);

        // Monolith file gone from `.rally/`.
        assert!(!monolith_path.exists());

        // Phase 3: re-run migration (reopen). Idempotent — no duplication.
        drop(store);
        let _ = RoomStore::open_at(root.clone()).unwrap();
        let segs2 = segments_under(&root);
        assert_eq!(
            count_segment_events(&segs2).unwrap(),
            10,
            "no event duplicated on second open"
        );
        let archive2 = archive_under(&root);
        assert_eq!(archive2.len(), 1);

        fs::remove_dir_all(&root).ok();
    }

    /// Engagement resolution priority: env var > persisted file > UTC date.
    #[test]
    fn engagement_resolution_priority_env_then_file_then_date() {
        let root = unique_root("engagement-resolve");
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).unwrap();

        // 1. No env, no file → UTC date.
        // Unset the env var if it's set in the test environment.
        // (cargo test isolates env vars per process; the var may leak from
        // outer shells if someone set it. Defensive remove.)
        // SAFETY: env mutation is safe in single-threaded test execution.
        unsafe {
            env::remove_var(ENGAGEMENT_ENV_VAR);
        }
        let label = resolve_active_engagement(&dir);
        let today = utc_date_label();
        assert_eq!(label, today);

        // 2. Persisted file → that label.
        persist_active_engagement(&dir, "  my-sprint  ").unwrap();
        assert_eq!(resolve_active_engagement(&dir), "my-sprint");

        // 3. Env var wins over file.
        // SAFETY: env mutation, single-threaded test.
        unsafe {
            env::set_var(ENGAGEMENT_ENV_VAR, "env-engagement");
        }
        assert_eq!(resolve_active_engagement(&dir), "env-engagement");
        // SAFETY: env mutation, single-threaded test.
        unsafe {
            env::remove_var(ENGAGEMENT_ENV_VAR);
        }

        // Sanitise strips path separators.
        let cleaned = sanitise_engagement("../escape/me");
        assert!(!cleaned.contains('/'));

        fs::remove_dir_all(&root).ok();
    }

    /// Write a raw segment file (lines already JSON) under `.rally/<dir>/`.
    fn write_segment(root: &Path, dir: &str, filename: &str, lines: &[&str]) {
        let seg_dir = root.join(".rally").join(dir);
        fs::create_dir_all(&seg_dir).unwrap();
        let body = format!("{}\n", lines.join("\n"));
        fs::write(seg_dir.join(filename), body).unwrap();
    }

    /// Render one segment line for `event_id` at `seq`/`kind`/`engagement`.
    fn ledger_line(seq: i64, kind: &str, event_id: &str, engagement: &str) -> String {
        let entry = LedgerLine {
            seq,
            occurred_at: format!("2026-05-01T00:00:{:02}Z", seq.min(59)),
            event_type: kind.to_string(),
            payload: json!({
                "schema": fact_schema(),
                "event_id": event_id,
                "seq": seq,
                "kind": kind,
                "subject": format!("subject-{event_id}"),
                "scope": ["src/"],
            }),
            engagement: Some(engagement.to_string()),
        };
        serde_json::to_string(&entry).unwrap()
    }

    /// Inode of `.rally/facts.db`. A destructive rebuild deletes + recreates
    /// the file, so the inode changes; a no-op reconcile leaves it stable.
    /// This is the canary for "was the cache rebuilt" WITHOUT perturbing the
    /// event count (planting a sentinel row would itself desync the count and
    /// force the very rebuild we're testing against).
    fn db_inode(root: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(root.join(".rally/facts.db")).unwrap().ino()
    }

    /// TEST A — A healthy cache must NOT be destroyed on open merely because
    /// the raw segment-line count exceeds the db's max sequence number. Count
    /// and max-seq are only comparable when seqs are contiguous from 1; a
    /// double-counted archive (same seqs in two files) makes line-count >
    /// max-seq with a perfectly fresh cache. RED against the
    /// `total_count > db_max_seq` trigger; GREEN once the trigger compares
    /// distinct-seq count to db event count.
    #[test]
    fn healthy_cache_not_rebuilt_when_count_exceeds_max_seq() {
        let root = unique_root("reconcile-no-false-rebuild");

        // Live segment + an archived monolith copy carrying the SAME seqs.
        // Raw line count across files = 6, but distinct seqs = {1,2,3} → the
        // rebuilt db's max_seq = 3. 6 > 3 must NOT mean "segments ahead".
        let lines: Vec<String> = (1..=3)
            .map(|s| ledger_line(s, "decision", &format!("e{s}"), "alpha"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);
        write_segment(&root, "archive", ARCHIVED_MONOLITH_FILENAME, &refs);

        // First open builds the cache from the (deduped) segment set.
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store.facts().unwrap().len(),
            3,
            "deduped to 3 distinct seqs"
        );
        assert_eq!(store.snapshot().unwrap().max_seq, 3);
        drop(store);
        let before = db_inode(&root);

        // Reopen. A correct reconcile sees the cache is fresh and does NOT
        // rebuild it → the db file is the same inode.
        let store = RoomStore::open_existing_at(root.clone()).unwrap().unwrap();
        assert_eq!(store.facts().unwrap().len(), 3);
        drop(store);
        assert_eq!(
            db_inode(&root),
            before,
            "healthy cache was destroyed: count > max_seq false-triggered a rebuild"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// TEST B — The R5 archived monolith `ledger-pre-segment.jsonl` is excluded
    /// from replay sources. Post-migration its events already live in the live
    /// segments; counting + replaying it double-counts every event, which
    /// inflates the reconcile trigger (false rebuild on every open). The
    /// distinctly-named file must be skipped. RED against today's "archive
    /// walked wholesale"; GREEN once the constant-named monolith is filtered.
    #[test]
    fn archived_monolith_excluded_from_replay_no_double_count() {
        let root = unique_root("reconcile-monolith-excluded");

        let lines: Vec<String> = (1..=4)
            .map(|s| ledger_line(s, "claim", &format!("e{s}"), "alpha"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);
        // Verbatim monolith copy in archive — same seqs as the live segment.
        write_segment(&root, "archive", ARCHIVED_MONOLITH_FILENAME, &refs);

        let store = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store.facts().unwrap().len(),
            4,
            "monolith not double-counted"
        );
        drop(store);
        let before = db_inode(&root);

        // Reopen: fresh cache, monolith excluded → no rebuild.
        let store = RoomStore::open_existing_at(root.clone()).unwrap().unwrap();
        assert_eq!(store.facts().unwrap().len(), 4);
        drop(store);
        assert_eq!(
            db_inode(&root),
            before,
            "archived monolith double-counted: triggered a rebuild of a fresh cache"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// TEST D — Replay tolerates a non-contiguous seq set. After rotation +
    /// dedup the surviving event set may start above 1 or contain gaps; replay
    /// is a pure function of the deduped events, not an assertion that the
    /// freshly-assigned seq equals the stored seq. RED against the strict
    /// `assigned != entry.seq` check; GREEN once that assertion is dropped.
    #[test]
    fn replay_tolerates_non_contiguous_seqs() {
        let root = unique_root("reconcile-noncontiguous");

        // Seqs {2, 5, 9} — gaps everywhere, none starting at 1. factstr will
        // reassign 1,2,3 on replay; the old strict check fired here.
        let lines = [
            ledger_line(2, "decision", "e2", "alpha"),
            ledger_line(5, "decision", "e5", "alpha"),
            ledger_line(9, "blocker", "e9", "alpha"),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);

        let store = RoomStore::open_at(root.clone()).unwrap();
        let facts = store.facts().unwrap();
        assert_eq!(facts.len(), 3, "all 3 non-contiguous events replayed");
        let ids: Vec<&str> = facts.iter().map(|f| f.event_id.as_str()).collect();
        assert_eq!(ids, ["e2", "e5", "e9"], "order preserved by stored seq");

        fs::remove_dir_all(&root).ok();
    }

    /// TEST C — R7 rotated segments (kept under their original
    /// `<engagement>.jsonl` name, NOT the monolith constant) must still be
    /// replayed from the archive. Guards against the exclusion being too broad
    /// (filtering all archive files instead of just the monolith).
    #[test]
    fn rotated_engagement_segment_in_archive_still_replays() {
        let root = unique_root("reconcile-rotated-replays");

        // Old engagement rotated to archive under its own name.
        let archived = [
            ledger_line(1, "decision", "old1", "2024-old"),
            ledger_line(2, "decision", "old2", "2024-old"),
        ];
        let arch_refs: Vec<&str> = archived.iter().map(String::as_str).collect();
        write_segment(&root, "archive", "2024-old.jsonl", &arch_refs);

        // Recent engagement still live.
        let live = [ledger_line(3, "claim", "new3", "beta")];
        let live_refs: Vec<&str> = live.iter().map(String::as_str).collect();
        write_segment(&root, "log", "beta.jsonl", &live_refs);

        let store = RoomStore::open_at(root.clone()).unwrap();
        let ids: Vec<String> = store
            .facts()
            .unwrap()
            .iter()
            .map(|f| f.event_id.clone())
            .collect();
        assert_eq!(
            ids,
            vec!["old1", "old2", "new3"],
            "rotated archive segment + live segment both replay"
        );

        fs::remove_dir_all(&root).ok();
    }

    // =========================================================================
    // R9-readback tests
    // =========================================================================

    /// R9-case-6 (green baseline): a genuine successful mutation → readback
    /// passes and the returned fact carries {room, seq}.
    #[test]
    fn r9_case6_successful_mutation_readback_passes_with_room_and_seq() {
        let root = unique_root("r9-case6-green");
        let store = RoomStore::open_at(root.clone()).unwrap();

        let fact = make_fact("ev-r9-6", FactKind::Claim, "src/", "r9 green baseline");
        let verified = store.append_fact_verified(&fact).unwrap();

        assert!(verified.seq > 0, "seq must be > 0 after verified append");
        assert_eq!(verified.event_id, "ev-r9-6", "event_id must be preserved");
        // room_id is available from the store.
        let room = store.room_id();
        assert!(!room.is_empty(), "room_id must be non-empty");

        fs::remove_dir_all(&root).ok();
    }

    /// R9-case-1 (stale-binary drop): a fact that lands only in `facts.db` but
    /// NOT a segment → `append_fact_verified`'s readback MUST fail.
    ///
    /// Simulation: call `append_fact` to write both db + segment, then truncate
    /// the segment file (removing the line), then call the segment-readback path
    /// directly. This proves the readback reads SEGMENTS, not the db.
    #[test]
    fn r9_case1_segment_drop_readback_fails() {
        let root = unique_root("r9-case1-drop");
        let store = RoomStore::open_at(root.clone()).unwrap();

        let fact = make_fact("ev-r9-1", FactKind::Decision, "src/", "segment drop test");
        // Write normally — both db and segment get the line.
        let appended = store.append_fact(&fact).unwrap();
        let event_id = &appended.event_id;

        // Simulate segment drop: truncate the active segment file so the line
        // is absent from the canonical record (db still has it).
        let seg_path = store.active_segment_path();
        assert!(seg_path.exists(), "segment file must exist after append");
        // Truncate: remove all content from the segment.
        fs::write(&seg_path, b"").unwrap();

        // Now run the segment-only readback logic.  It must not find the event.
        let live_segs = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch_segs = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let found =
            segment_event_id_present(live_segs.iter().chain(arch_segs.iter()), event_id).unwrap();
        assert!(
            !found,
            "readback must NOT find event_id in segments after segment truncation (drop simulation)"
        );

        // Confirm db still has it — proving the readback correctly targets segments.
        let db_facts = store.facts().unwrap();
        let in_db = db_facts.iter().any(|f| f.event_id == *event_id);
        assert!(
            in_db,
            "fact must still exist in facts.db (cache) after segment truncation — proving split state"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R9-case-4 (cache-false-pass guard): prove that a readback reading
    /// `facts.db` instead of segments WOULD false-pass the stale-binary drop
    /// case — i.e., after segment truncation `facts.db` still contains the fact,
    /// confirming our readback's segment-only approach is necessary.
    ///
    /// This test is the companion to case-1: it explicitly asserts that the db
    /// contains the event_id even though the segment does not, proving that
    /// ANY readback path that checked the db would false-pass.
    #[test]
    fn r9_case4_db_false_passes_where_segment_readback_correctly_fails() {
        let root = unique_root("r9-case4-db-false-pass");
        let store = RoomStore::open_at(root.clone()).unwrap();

        let fact = make_fact("ev-r9-4", FactKind::Claim, "src/", "db false-pass guard");
        let appended = store.append_fact(&fact).unwrap();
        let event_id = &appended.event_id;

        // Drop the segment (truncate), leaving the db intact.
        let seg_path = store.active_segment_path();
        fs::write(&seg_path, b"").unwrap();

        // Assert 1: segment-based readback returns false (correct).
        let live_segs = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch_segs = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let seg_found =
            segment_event_id_present(live_segs.iter().chain(arch_segs.iter()), event_id).unwrap();
        assert!(
            !seg_found,
            "segment readback must return false after truncation (correct)"
        );

        // Assert 2: db-based readback returns true (false-pass territory).
        let db_facts = store.facts().unwrap();
        let db_found = db_facts.iter().any(|f| f.event_id == *event_id);
        assert!(
            db_found,
            "db readback returns true even with the segment gone — this is the false-pass that our segment-only readback avoids"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R9-case-2 (no-op release): `release` without a valid `--ref` that names
    /// a live active claim → MUST fail loud via `append_state_transition_verified`.
    #[test]
    fn r9_case2_noop_release_fails_loud_without_valid_ref() {
        let root = unique_root("r9-case2-noop-release");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Write a claim first.
        let claim = make_fact("ev-claim-r9", FactKind::Claim, "src/", "claim to release");
        store.append_fact(&claim).unwrap();

        // Case A: release with no ref_id at all → must fail.
        let release_no_ref = Fact {
            schema: fact_schema(),
            event_id: "ev-release-no-ref".to_string(),
            seq: 0,
            thread_id: "t-r".to_string(),
            kind: FactKind::Release,
            tool: Some("test".to_string()),
            role: None,
            subject: "release no ref".to_string(),
            scope: vec!["src/".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None, // no ref
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let err_no_ref = store
            .append_state_transition_verified(&release_no_ref)
            .unwrap_err();
        let msg_no_ref = err_no_ref.to_string();
        assert!(
            msg_no_ref.contains("requires --ref"),
            "error for missing ref must mention --ref; got: {msg_no_ref}"
        );

        // Case B: release with a bogus ref that is not a live claim → must fail.
        let release_bogus = Fact {
            schema: fact_schema(),
            event_id: "ev-release-bogus".to_string(),
            seq: 0,
            thread_id: "t-rb".to_string(),
            kind: FactKind::Release,
            tool: Some("test".to_string()),
            role: None,
            subject: "release bogus ref".to_string(),
            scope: vec!["src/".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: Some("nonexistent-event-id".to_string()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let err_bogus = store
            .append_state_transition_verified(&release_bogus)
            .unwrap_err();
        let msg_bogus = err_bogus.to_string();
        assert!(
            msg_bogus.contains("not an active claim") || msg_bogus.contains("release failed"),
            "error for bogus ref must indicate the target is not a live claim; got: {msg_bogus}"
        );

        // Verify neither release fact landed in the canonical segments.
        let segs = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        for bad_id in ["ev-release-no-ref", "ev-release-bogus"] {
            let found = segment_event_id_present(segs.iter().chain(arch.iter()), bad_id).unwrap();
            assert!(
                !found,
                "failed release fact {bad_id} must NOT appear in canonical segments"
            );
        }

        fs::remove_dir_all(&root).ok();
    }

    /// R9-case-3 (wrong-room write): a readback expecting the event_id in room A
    /// when it landed in room B MUST fail.
    ///
    /// Simulation: write a fact to store-B, then run the segment-readback against
    /// store-A's log dir — the event_id is absent from A's segments.
    #[test]
    fn r9_case3_wrong_room_event_absent_in_other_room_segments() {
        let root_a = unique_root("r9-case3-room-a");
        let root_b = unique_root("r9-case3-room-b");
        let _store_a = RoomStore::open_at(root_a.clone()).unwrap();
        let store_b = RoomStore::open_at(root_b.clone()).unwrap();

        // Write a fact to room B.
        let fact = make_fact("ev-room-b", FactKind::Artifact, "src/", "wrong room test");
        let appended_b = store_b.append_fact(&fact).unwrap();
        let event_id = &appended_b.event_id;

        // Readback against room A's segments — must return false (wrong room).
        let segs_a = read_segment_files(&root_a.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch_a = read_segment_files(&root_a.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let found_in_a =
            segment_event_id_present(segs_a.iter().chain(arch_a.iter()), event_id).unwrap();
        assert!(
            !found_in_a,
            "event written to room B must NOT be found in room A's canonical segments"
        );

        // Confirm it IS in room B's segments (for sanity).
        let segs_b = read_segment_files(&root_b.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch_b = read_segment_files(&root_b.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let found_in_b =
            segment_event_id_present(segs_b.iter().chain(arch_b.iter()), event_id).unwrap();
        assert!(
            found_in_b,
            "event written to room B must be found in room B's canonical segments"
        );

        fs::remove_dir_all(&root_a).ok();
        fs::remove_dir_all(&root_b).ok();
    }

    /// R9-case-5 (concurrency): a peer append between write and readback MUST NOT
    /// false-pass — assert the EXACT event_id is found, not merely that seq advanced.
    ///
    /// Simulation: write fact-A, then simulate a concurrent peer write (manually
    /// insert a segment line for fact-B with a higher seq), then run readback for
    /// fact-A's event_id — must return true (exact match, not max-seq advancement).
    /// Then verify fact-B's (different) event_id is also present — but searching
    /// for a nonexistent id still returns false.
    #[test]
    fn r9_case5_concurrent_peer_append_does_not_false_pass_exact_event_id() {
        let root = unique_root("r9-case5-concurrency");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Write fact-A (our mutation).
        let fact_a = make_fact("ev-r9-5a", FactKind::Claim, "src/", "our fact");
        let appended_a = store.append_fact(&fact_a).unwrap();

        // Simulate a concurrent peer append: manually write a segment line for
        // a peer fact at a higher seq.  This is what a concurrent writer would do.
        let peer_seq = appended_a.seq + 100; // jump to simulate concurrent write
        let peer_event_id = "ev-r9-5b-peer";
        let peer_line = LedgerLine {
            seq: peer_seq,
            occurred_at: now_string(),
            event_type: "claim".to_string(),
            payload: serde_json::json!({
                "schema": fact_schema(),
                "event_id": peer_event_id,
                "seq": peer_seq,
                "kind": "claim",
                "subject": "peer concurrent fact",
                "scope": ["src/"],
            }),
            engagement: Some(store.active_engagement.clone()),
        };
        let seg_path = store.active_segment_path();
        let peer_line_str = serde_json::to_string(&peer_line).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .unwrap();
        writeln!(file, "{peer_line_str}").unwrap();
        drop(file);

        // Now run the segment readback for fact-A's exact event_id.
        // It must find fact-A (not merely see that max_seq advanced to peer_seq).
        let segs = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();

        let found_a =
            segment_event_id_present(segs.iter().chain(arch.iter()), &appended_a.event_id).unwrap();
        assert!(
            found_a,
            "exact event_id for fact-A must be found even with a concurrent peer append present"
        );

        // Also verify the peer event is present.
        let segs2 = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch2 = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let found_peer =
            segment_event_id_present(segs2.iter().chain(arch2.iter()), peer_event_id).unwrap();
        assert!(found_peer, "peer event_id must also be findable");

        // Key concurrency assertion: searching for a NONEXISTENT event_id must
        // still return false even though seq advanced (disproves max-seq advancement
        // as a false-pass proxy).
        let segs3 = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch3 = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        let found_ghost =
            segment_event_id_present(segs3.iter().chain(arch3.iter()), "ev-does-not-exist")
                .unwrap();
        assert!(
            !found_ghost,
            "a nonexistent event_id must NOT be found even though seq advanced (exact-match, not seq-advance check)"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// TEST E — Parallel reads (concurrent `open_existing_at`) must not destroy
    /// the cache or race each other into an error. With the false-rebuild
    /// trigger fixed, a reader never rebuilds a fresh db, so N concurrent
    /// readers all see the same 5 facts and the db file is never recreated.
    #[test]
    fn parallel_reads_do_not_destroy_cache() {
        use std::sync::Arc;

        let root = unique_root("reconcile-parallel-read");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for n in 1..=5 {
            store
                .append_fact(&make_fact(
                    &format!("e{n}"),
                    FactKind::Decision,
                    "src/",
                    "x",
                ))
                .unwrap();
        }
        drop(store);
        let before = db_inode(&root);

        let root = Arc::new(root);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                thread::spawn(move || {
                    let store = RoomStore::open_existing_at((*root).clone())
                        .unwrap()
                        .unwrap();
                    store.facts().unwrap().len()
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 5, "reader saw a destroyed/racing cache");
        }
        assert_eq!(db_inode(&root), before, "parallel reads rebuilt the cache");

        fs::remove_dir_all(&*root).ok();
    }

    #[test]
    fn parallel_opens_and_appends_keep_db_and_segments_in_lockstep() {
        use std::sync::Arc;

        let root = Arc::new(unique_root("parallel-open-append-lockstep"));
        let store = RoomStore::open_at((*root).clone()).unwrap();
        drop(store);

        let handles: Vec<_> = (0..24)
            .map(|n| {
                let root = Arc::clone(&root);
                thread::spawn(move || {
                    let store = RoomStore::open_at((*root).clone()).unwrap();
                    let event_id = format!("parallel-event-{n}");
                    store
                        .append_fact_verified(&make_fact(
                            &event_id,
                            FactKind::Decision,
                            "src/",
                            "parallel append",
                        ))
                        .unwrap();
                    event_id
                })
            })
            .collect();

        let expected_ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<BTreeSet<_>>();

        let reader = RoomStore::open_at((*root).clone()).unwrap();
        let facts = reader.facts().unwrap();
        let actual_ids = facts
            .iter()
            .map(|fact| fact.event_id.clone())
            .collect::<BTreeSet<_>>();
        let seqs = facts.iter().map(|fact| fact.seq).collect::<BTreeSet<_>>();

        assert_eq!(facts.len(), 24);
        assert_eq!(actual_ids, expected_ids);
        assert_eq!(seqs.len(), 24);
        assert!(seqs.contains(&1));
        assert!(seqs.contains(&24));

        fs::remove_dir_all(&*root).ok();
    }

    // =========================================================================
    // R10 read-checkpoint tests
    // =========================================================================

    /// R10-a: After a tool records a read-checkpoint, a `FactKind::Read` fact
    /// exists in the ledger with the correct `read_seq`, and
    /// `project_read_receipts` surfaces it with the right `last_read_seq` and
    /// `behind_by`.
    #[test]
    fn r10_a_read_checkpoint_lands_in_ledger_and_projects_correctly() {
        let root = unique_root("r10-a-read-checkpoint");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Post two substantive facts.
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim one"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided"))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let content_max = snapshot.content_max_seq;
        assert_eq!(content_max, 2, "content_max_seq after 2 substantive facts");

        // Record a read-checkpoint for "tool-a" at content_max.
        let cp = store
            .maybe_append_read_checkpoint("tool-a", content_max)
            .unwrap();
        assert!(
            cp.is_some(),
            "checkpoint must be written when read position advances"
        );

        // The checkpoint fact must be in the ledger.
        let facts = store.facts().unwrap();
        let read_facts: Vec<&Fact> = facts
            .iter()
            .filter(|f| f.kind == "read" && f.tool.as_deref() == Some("tool-a"))
            .collect();
        assert_eq!(
            read_facts.len(),
            1,
            "exactly one read-checkpoint fact for tool-a"
        );
        let cp_fact = read_facts[0];
        let expected_summary = format!("read_seq:{content_max}");
        assert_eq!(
            cp_fact.summary.as_deref(),
            Some(expected_summary.as_str()),
            "summary encodes read_seq"
        );

        // project_read_receipts: tool-a is caught up (behind_by = 0) since no
        // substantive facts have landed after the checkpoint.
        // snapshot.max_seq includes the checkpoint itself, but behind_by is
        // relative to the total ledger tip (max_seq).
        let total_max = store.snapshot().unwrap().max_seq;
        let receipts = store.project_read_receipts(total_max).unwrap();
        let tool_a = receipts
            .iter()
            .find(|r| r.tool == "tool-a")
            .expect("tool-a in receipts");
        assert_eq!(
            tool_a.last_read_seq, content_max,
            "last_read_seq = content_max"
        );
        // behind_by = total_max - last_read_seq; since tool-a read at content_max
        // and there's 1 more fact (the checkpoint itself), behind_by = 1.
        // This is intentional: the checkpoint is also a ledger fact.
        assert!(
            tool_a.behind_by <= 1,
            "tool-a is at most 1 behind (checkpoint fact itself)"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R10-b: BLOAT GUARD — calling `maybe_append_read_checkpoint` twice with
    /// the same `read_seq` (no new substantive activity between calls) writes
    /// only ONE checkpoint — the second call is a no-op.
    #[test]
    fn r10_b_no_bloat_repeated_checkpoint_at_same_seq_is_noop() {
        let root = unique_root("r10-b-no-bloat");
        let store = RoomStore::open_at(root.clone()).unwrap();

        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim"))
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let content_max = snapshot.content_max_seq;

        // First checkpoint — must write.
        let cp1 = store
            .maybe_append_read_checkpoint("tool-a", content_max)
            .unwrap();
        assert!(cp1.is_some(), "first checkpoint must write");

        // Second checkpoint at the same position — must be a no-op.
        let cp2 = store
            .maybe_append_read_checkpoint("tool-a", content_max)
            .unwrap();
        assert!(
            cp2.is_none(),
            "second checkpoint at same seq must be a no-op (coalesced)"
        );

        // Only ONE read-checkpoint fact in the ledger for tool-a.
        let facts = store.facts().unwrap();
        let read_count = facts
            .iter()
            .filter(|f| f.kind == "read" && f.tool.as_deref() == Some("tool-a"))
            .count();
        assert_eq!(
            read_count, 1,
            "BLOAT GUARD: exactly one read-checkpoint fact for tool-a after two no-advance polls"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R10-b extension: after posting a NEW substantive fact, a further
    /// checkpoint IS written (position genuinely advanced).
    #[test]
    fn r10_b_new_activity_allows_second_checkpoint() {
        let root = unique_root("r10-b-new-activity");
        let store = RoomStore::open_at(root.clone()).unwrap();

        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "first claim"))
            .unwrap();
        let snap1 = store.snapshot().unwrap();
        let c1 = snap1.content_max_seq;

        // First checkpoint.
        let cp1 = store.maybe_append_read_checkpoint("tool-a", c1).unwrap();
        assert!(cp1.is_some());

        // Post a new substantive fact.
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "new decision"))
            .unwrap();
        let snap2 = store.snapshot().unwrap();
        let c2 = snap2.content_max_seq;
        assert!(
            c2 > c1,
            "content_max_seq must advance after new substantive fact"
        );

        // Second checkpoint at the new position — must write.
        let cp2 = store.maybe_append_read_checkpoint("tool-a", c2).unwrap();
        assert!(cp2.is_some(), "checkpoint after new activity must write");

        // Two read-checkpoint facts now.
        let facts = store.facts().unwrap();
        let read_count = facts
            .iter()
            .filter(|f| f.kind == "read" && f.tool.as_deref() == Some("tool-a"))
            .count();
        assert_eq!(
            read_count, 2,
            "two read-checkpoints after two distinct advances"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R10-c: `FactKind::Read` facts do NOT appear in `active_claims`,
    /// `open_handoffs`, `active_blockers`, or `current_risks` — they are
    /// invisible to claimable-work projection.
    #[test]
    fn r10_c_read_checkpoint_facts_excluded_from_claimable_work() {
        let root = unique_root("r10-c-excluded-from-work");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Post some substantive facts, then record a checkpoint.
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "real claim"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Blocker, "src/", "real blocker"))
            .unwrap();
        let snap = store.snapshot().unwrap();
        store
            .maybe_append_read_checkpoint("tool-a", snap.content_max_seq)
            .unwrap();

        let snapshot = store.snapshot().unwrap();

        // active_claims contains only the claim fact, not the read-checkpoint.
        assert!(
            snapshot.active_claims.iter().all(|f| f.kind != "read"),
            "active_claims must not contain read-checkpoint facts"
        );
        // active_blockers contains only the blocker.
        assert!(
            snapshot.active_blockers.iter().all(|f| f.kind != "read"),
            "active_blockers must not contain read-checkpoint facts"
        );
        // open_handoffs is empty (we posted none).
        assert!(snapshot.open_handoffs.is_empty());
        // current_risks is empty.
        assert!(snapshot.current_risks.is_empty());

        // The ledger DOES contain the read-checkpoint fact.
        let all_facts = store.facts().unwrap();
        let read_count = all_facts.iter().filter(|f| f.kind == "read").count();
        assert_eq!(read_count, 1, "read-checkpoint fact is in the ledger");

        fs::remove_dir_all(&root).ok();
    }

    /// R10-d: Two distinct tools both record checkpoints; `project_read_receipts`
    /// reports both with correct `behind_by` values.
    #[test]
    fn r10_d_two_tools_both_appear_in_read_receipts() {
        let root = unique_root("r10-d-two-tools");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Post 3 substantive facts.
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim one"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided"))
            .unwrap();
        store
            .append_fact(&make_fact("e3", FactKind::Blocker, "src/", "blocker"))
            .unwrap();

        let snap1 = store.snapshot().unwrap();
        let after_3_substantive = snap1.content_max_seq;
        // content_max_seq = 3 (3 substantive facts, no checkpoints yet)
        assert_eq!(after_3_substantive, 3);

        // tool-a reads all 3 facts.
        store
            .maybe_append_read_checkpoint("tool-a", after_3_substantive)
            .unwrap();
        // Ledger now: seqs 1,2,3 (facts) + 4 (tool-a checkpoint).

        // Post one more substantive fact (gets next seq after tool-a's checkpoint).
        store
            .append_fact(&make_fact("e4", FactKind::Artifact, "src/", "artifact"))
            .unwrap();

        let snap2 = store.snapshot().unwrap();
        let after_4_substantive = snap2.content_max_seq;
        // content_max_seq = seq of e4 (the checkpoint at seq 4 is excluded).
        assert!(
            after_4_substantive > after_3_substantive,
            "content_max_seq advances with e4"
        );

        // tool-b reads only up to after_3 (missed the new artifact).
        store
            .maybe_append_read_checkpoint("tool-b", after_3_substantive)
            .unwrap();

        // Project read receipts.
        let total_max = store.snapshot().unwrap().max_seq;
        let receipts = store.project_read_receipts(total_max).unwrap();

        let a = receipts
            .iter()
            .find(|r| r.tool == "tool-a")
            .expect("tool-a in receipts");
        let b = receipts
            .iter()
            .find(|r| r.tool == "tool-b")
            .expect("tool-b in receipts");

        // Both tools checkpointed at after_3_substantive.
        assert_eq!(
            a.last_read_seq, after_3_substantive,
            "tool-a last_read_seq = after_3_substantive"
        );
        assert_eq!(
            b.last_read_seq, after_3_substantive,
            "tool-b last_read_seq = after_3_substantive"
        );

        // Both are behind the ledger head (e4 + checkpoints landed after their read).
        assert_eq!(
            a.behind_by, b.behind_by,
            "both tools are equally behind (same checkpoint position)"
        );
        assert!(
            a.behind_by > 0,
            "both tools are behind (e4 and its checkpoints landed after their read)"
        );

        // Status: both "behind".
        assert_eq!(a.status, "behind", "tool-a status = behind");
        assert_eq!(b.status, "behind", "tool-b status = behind");

        // tool-a with higher read (caught up after e4) would show caught_up —
        // simulate by checking tool-a after it reads e4.
        let read_seq_e4 = after_4_substantive;
        store
            .maybe_append_read_checkpoint("tool-a", read_seq_e4)
            .unwrap();
        let receipts2 = store
            .project_read_receipts(store.snapshot().unwrap().max_seq)
            .unwrap();
        let a2 = receipts2
            .iter()
            .find(|r| r.tool == "tool-a")
            .expect("tool-a in receipts2");
        // tool-a now has higher last_read_seq; tool-b is still at after_3.
        assert_eq!(
            a2.last_read_seq, read_seq_e4,
            "tool-a advanced to e4 read_seq"
        );
        let b2 = receipts2
            .iter()
            .find(|r| r.tool == "tool-b")
            .expect("tool-b in receipts2");
        assert_eq!(b2.last_read_seq, after_3_substantive, "tool-b unchanged");
        // tool-b is further behind than tool-a.
        assert!(
            b2.behind_by > a2.behind_by,
            "tool-b is further behind than tool-a"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R10-anti-loop: calling `maybe_append_read_checkpoint` repeatedly with
    /// `content_max_seq` (which EXCLUDES read-checkpoint seqs) must never create
    /// more than one checkpoint per substantive advancement — no feedback loop.
    #[test]
    fn r10_anti_loop_content_max_seq_prevents_self_inflation() {
        let root = unique_root("r10-anti-loop");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Post one substantive fact.
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "lone claim"))
            .unwrap();

        // Simulate 5 polls with no new substantive activity.
        for _ in 0..5 {
            let snap = store.snapshot().unwrap();
            // Use content_max_seq (excludes read checkpoints) — mimics command_next.
            let _ = store.maybe_append_read_checkpoint("tool-a", snap.content_max_seq);
        }

        // Only ONE read-checkpoint fact must exist (first poll wrote it; subsequent
        // polls saw content_max_seq unchanged and were coalesced).
        let facts = store.facts().unwrap();
        let read_count = facts.iter().filter(|f| f.kind == "read").count();
        assert_eq!(
            read_count, 1,
            "5 no-advance polls with content_max_seq must produce only 1 read-checkpoint (anti-loop guard)"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// R10-cursor-ledger-primary: cursor_for() must return the ledger-derived
    /// position even when cursors.json is absent, and must not drift on
    /// repeated checkpoints (enter → append → re-enter stability).
    ///
    /// Simulates the pattern `command_enter` uses:
    ///   1. set_cursor + maybe_append_read_checkpoint (enter)
    ///   2. append substantive facts (peer activity)
    ///   3. delete cursors.json (simulate lost side-file)
    ///   4. assert cursor_for still returns ledger value
    ///   5. advance checkpoint (second enter) — assert stable, not inflating
    #[test]
    fn r10_cursor_for_is_ledger_derived_survives_cursors_json_deletion() {
        let root = unique_root("r10-cursor-ledger-primary");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Step 1: simulate first enter — write both the side-file cache and a ledger checkpoint.
        let snap0 = store.snapshot().unwrap();
        let cursor_after_enter1 = snap0.max_seq; // 0 at start
        store.set_cursor("tool-a", cursor_after_enter1).unwrap();
        // content_max_seq is 0 here; maybe_append_read_checkpoint coalesces at 0 (no-op is ok).
        // Post a substantive fact first so content_max > 0 before the checkpoint.
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "first claim"))
            .unwrap();
        let snap1 = store.snapshot().unwrap();
        let content_max1 = snap1.content_max_seq;
        assert_eq!(
            content_max1, 1,
            "one substantive fact → content_max_seq == 1"
        );

        // Record a real ledger checkpoint for tool-a.
        let cp = store
            .maybe_append_read_checkpoint("tool-a", content_max1)
            .unwrap();
        assert!(cp.is_some(), "first checkpoint must be written");

        // Step 2: append more substantive facts (peer activity after the enter).
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decision"))
            .unwrap();
        store
            .append_fact(&make_fact("e3", FactKind::Risk, "src/", "risk"))
            .unwrap();

        // Step 3: delete cursors.json to prove ledger is the source of truth.
        let cursor_path = root.join(".rally").join("cursors.json");
        if cursor_path.exists() {
            fs::remove_file(&cursor_path).expect("delete cursors.json for test");
        }
        assert!(
            !cursor_path.exists(),
            "cursors.json must be gone before testing cursor_for"
        );

        // Step 4: cursor_for must still return content_max1 from the ledger checkpoint.
        let recovered = store.cursor_for("tool-a").unwrap();
        assert_eq!(
            recovered, content_max1,
            "cursor_for must return ledger checkpoint value even with cursors.json deleted"
        );

        // Step 5: simulate second enter — advance checkpoint to current content_max.
        let snap2 = store.snapshot().unwrap();
        let content_max2 = snap2.content_max_seq;
        // e1 (seq=1) + read-checkpoint (seq=2, excluded from content_max) +
        // e2 (seq=3) + e3 (seq=4) → content_max_seq = 4 (highest non-read seq).
        assert_eq!(
            content_max2, 4,
            "three substantive facts (e1/e2/e3) with one intervening read-checkpoint → content_max_seq == 4"
        );

        let cp2 = store
            .maybe_append_read_checkpoint("tool-a", content_max2)
            .unwrap();
        assert!(
            cp2.is_some(),
            "second checkpoint must advance (content advanced from 1 to 3)"
        );

        // cursor_for must now return the new higher value — no inflation, stable.
        let after_re_enter = store.cursor_for("tool-a").unwrap();
        assert_eq!(
            after_re_enter, content_max2,
            "cursor_for after re-enter must equal advanced checkpoint, not inflate further"
        );

        // Calling cursor_for a third time must return the same value (idempotent).
        let idempotent = store.cursor_for("tool-a").unwrap();
        assert_eq!(
            idempotent, after_re_enter,
            "cursor_for must be idempotent — no side effects on repeated reads"
        );

        fs::remove_dir_all(&root).ok();
    }
}
