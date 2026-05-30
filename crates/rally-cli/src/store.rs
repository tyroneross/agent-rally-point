use chrono;
use factstr::{EventQuery as FactQuery, EventStore, EventStoreError, NewEvent};
use factstr_sqlite::SqliteStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
}

/// Seconds of inactivity after which a squad member is marked "idle".
const IDLE_THRESHOLD_SECS: i64 = 15 * 60;

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct RoomSnapshot {
    pub(crate) max_seq: i64,
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
}

impl RoomSnapshot {
    pub(crate) fn filtered(self, query: &RoomQuery) -> Self {
        if query.is_empty() {
            return self;
        }
        Self {
            max_seq: self.max_seq,
            active_claims: filter_facts(self.active_claims, query),
            active_blockers: filter_facts(self.active_blockers, query),
            open_handoffs: filter_facts(self.open_handoffs, query),
            current_decisions: filter_facts(self.current_decisions, query),
            current_risks: filter_facts(self.current_risks, query),
            recent_artifacts: filter_facts(self.recent_artifacts, query),
            unconsumed_artifacts: filter_facts(self.unconsumed_artifacts, query),
            stale_facts: filter_facts(self.stale_facts, query),
            // squads and lead are room-level aggregates; not filtered by path/tool query.
            squads: self.squads,
            lead: self.lead,
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
    #[allow(dead_code)] // public for future R6 retrospective consumers
    pub(crate) fn active_engagement(&self) -> &str {
        &self.active_engagement
    }

    /// Path of the segment file the next append will land in.
    pub(crate) fn active_segment_path(&self) -> PathBuf {
        self.log_dir
            .join(format!("{}.jsonl", self.active_engagement))
    }

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        let mut fact = fact.clone();
        let event_type = fact.kind.as_str().to_string();
        let payload = serde_json::to_value(&fact).map_err(RallyError::json("render fact"))?;
        let result = self
            .fact_store
            .append(vec![NewEvent::new(event_type.clone(), payload.clone())])
            .map_err(|err| RallyError::Message(format!("append fact: {err}")))?;
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

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<Option<Fact>> {
        let mut fact = fact.clone();
        let payload =
            serde_json::to_value(&fact).map_err(RallyError::json("render session fact"))?;
        let result = self.fact_store.append_if(
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
        let query = self
            .fact_store
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
        let max_seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);
        let resolved = facts
            .iter()
            .filter(|f| f.kind == "resolve" || f.kind == "release")
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
        for fact in &facts {
            if let Some(tool) = &fact.tool {
                if tool == "rally" {
                    continue;
                }
                let entry = tool_last
                    .entry(tool.clone())
                    .or_insert((0, String::new()));
                if fact.seq > entry.0 {
                    *entry = (fact.seq, fact.created_at.clone());
                }
            }
        }
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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
                Squad {
                    tool,
                    last_seen_seq: seq,
                    last_seen_ts: ts,
                    status,
                }
            })
            .collect::<Vec<_>>();

        // Lead is the tool from the most-recent decision with subject "role:lead".
        let lead = facts
            .iter()
            .filter(|f| f.kind == "decision" && f.subject == "role:lead")
            .max_by_key(|f| f.seq)
            .and_then(|f| f.tool.clone());

        Ok(RoomSnapshot {
            max_seq,
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
        })
    }

    pub(crate) fn cursor_for(&self, tool: &str) -> Result<i64> {
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
}

fn filter_facts(facts: Vec<Fact>, query: &RoomQuery) -> Vec<Fact> {
    facts
        .into_iter()
        .filter(|fact| query.matches(fact))
        .collect()
}

fn open_fact_store(path: &Path) -> Result<SqliteStore> {
    let mut attempts = 0;
    loop {
        match SqliteStore::open(path) {
            Ok(store) => return Ok(store),
            Err(err)
                if attempts < 8 && (is_bootstrap_metadata_race(&err) || is_db_locked(&err)) =>
            {
                attempts += 1;
                thread::sleep(Duration::from_millis(25 * attempts));
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

    // Dedup by seq (keep first occurrence); hard-error if two seqs disagree
    // on payload.
    let mut deduped: Vec<LedgerLine> = Vec::with_capacity(all_entries.len());
    for entry in all_entries {
        if let Some(prev) = deduped.last()
            && prev.seq == entry.seq
        {
            if prev.payload != entry.payload || prev.event_type != entry.event_type {
                return Err(RallyError::Message(format!(
                    "segment replay conflict at seq {}: two distinct events recorded with the same sequence number",
                    entry.seq
                )));
            }
            continue;
        }
        deduped.push(entry);
    }

    let store = open_fact_store(facts_db_path)?;
    for entry in &deduped {
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
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(segment_path)
        .map_err(RallyError::io(format!("open {}", segment_path.display())))?;
    writeln!(file, "{line}")
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
        let rendered =
            serde_json::to_string_pretty(&json!({"segments": entries, "updated_at": now_string()}))
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
}
