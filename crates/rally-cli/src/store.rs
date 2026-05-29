use factstr::{EventQuery as FactQuery, EventStore, EventStoreError, NewEvent};
use factstr_sqlite::SqliteStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Filename of the canonical, append-only, per-repo ledger.
///
/// This is the *durable* record of every fact that lands in the room. It is
/// committed to the repository (un-gitignored in `.gitignore`, with
/// `merge=union` in `.gitattributes`) so a clone or a fresh machine can
/// reconstruct the room state without any external service or cache.
///
/// `facts.db` is a derived sqlite cache built by replaying this ledger; if the
/// db is missing or behind the ledger, it is rebuilt on `open_at`. The ledger
/// itself is never deleted by rally.
pub(crate) const LEDGER_FILENAME: &str = "ledger.jsonl";

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
    ledger_path: PathBuf,
}

/// One line of `ledger.jsonl`.
///
/// Compact on purpose: one event, its assigned `seq` (factstr's monotonic
/// `sequence_number`), an `occurred_at` ISO-8601 timestamp, the factstr
/// `event_type`, and the full payload (the serialised `Fact`). Replaying these
/// lines in order through `factstr` rebuilds `facts.db` verbatim because
/// factstr assigns seqs deterministically in append order.
#[derive(Debug, Deserialize, Serialize)]
struct LedgerLine {
    seq: i64,
    occurred_at: String,
    event_type: String,
    payload: Value,
}

impl RoomStore {
    pub(crate) fn open() -> Result<Self> {
        Self::open_at(repo_root()?)
    }

    /// Open the per-repo room, applying the **canonical ledger / derived db**
    /// contract:
    ///
    /// 1. If `ledger.jsonl` exists and contains more events than the current
    ///    `facts.db`, the db is rebuilt by replaying the ledger. The db is a
    ///    pure cache — never canonical.
    /// 2. If `ledger.jsonl` is absent but `facts.db` already has events, seed
    ///    `ledger.jsonl` from the db so no history is lost on first upgrade.
    /// 3. Otherwise the ledger and db are already in sync and we proceed.
    ///
    /// Both replay and seed are idempotent — running them twice on the same
    /// inputs yields identical state.
    pub(crate) fn open_at(root: PathBuf) -> Result<Self> {
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).map_err(RallyError::io("create .rally"))?;
        let _ = fs::remove_file(dir.join("room.db"));
        let fact_store_path = dir.join("facts.db");
        let ledger_path = dir.join(LEDGER_FILENAME);

        reconcile_ledger_and_db(&ledger_path, &fact_store_path)?;

        let fact_store = open_fact_store(&fact_store_path)?;
        let store = Self {
            fact_store,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            ledger_path,
        };
        let _ = store.refresh_index(0);
        Ok(store)
    }

    pub(crate) fn open_existing_at(root: PathBuf) -> Result<Option<Self>> {
        let dir = root.join(".rally");
        let fact_store_path = dir.join("facts.db");
        let ledger_path = dir.join(LEDGER_FILENAME);
        // Existence is determined by EITHER the canonical ledger OR the
        // derived db — a clone that only carries `ledger.jsonl` is still a
        // real room and must open transparently.
        if !fact_store_path.exists() && !ledger_path.exists() {
            return Ok(None);
        }
        reconcile_ledger_and_db(&ledger_path, &fact_store_path)?;
        let fact_store = open_fact_store(&fact_store_path)?;
        Ok(Some(Self {
            fact_store,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            ledger_path,
        }))
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
        append_ledger_line(
            &self.ledger_path,
            &LedgerLine {
                seq: fact.seq,
                occurred_at: now_string(),
                event_type,
                payload,
            },
        )?;
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
                append_ledger_line(
                    &self.ledger_path,
                    &LedgerLine {
                        seq: fact.seq,
                        occurred_at: now_string(),
                        event_type: "session".to_string(),
                        payload,
                    },
                )?;
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
            Err(err) if attempts < 5 && is_bootstrap_metadata_race(&err) => {
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

/// Reconcile the canonical ledger with the derived sqlite cache.
///
/// Called on every `RoomStore::open_at` / `open_existing_at`. The contract is:
///
/// * Ledger ahead of db (incl. db absent) → rebuild db by replaying ledger.
/// * Ledger absent but db has events → seed ledger from db.
/// * Both empty, or in sync → no-op.
///
/// Idempotent: running twice yields the same state.
fn reconcile_ledger_and_db(ledger_path: &Path, facts_db_path: &Path) -> Result<()> {
    let ledger_count = count_ledger_events(ledger_path)?;
    let db_max_seq = read_db_max_seq(facts_db_path)?;

    if ledger_count == 0 && db_max_seq == 0 {
        return Ok(()); // fresh room or both empty
    }

    if ledger_count > db_max_seq {
        // Canonical ledger is ahead of derived cache. Rebuild the cache.
        rebuild_db_from_ledger(ledger_path, facts_db_path)?;
        return Ok(());
    }

    if ledger_count == 0 && db_max_seq > 0 {
        // First-run upgrade: db pre-dates the ledger feature. Seed the
        // canonical record from the existing cache so we don't lose history.
        seed_ledger_from_db(ledger_path, facts_db_path)?;
    }
    // ledger_count <= db_max_seq && ledger_count > 0 → cache is fresh or
    // ahead; nothing to do. (Cache "ahead" can only happen if someone wrote
    // straight to the db, which we never do.)
    Ok(())
}

fn count_ledger_events(ledger_path: &Path) -> Result<i64> {
    if !ledger_path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(ledger_path)
        .map_err(RallyError::io(format!("read {}", ledger_path.display())))?;
    let mut count = 0i64;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(RallyError::io(format!("read {}", ledger_path.display())))?;
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

fn read_db_max_seq(facts_db_path: &Path) -> Result<i64> {
    if !facts_db_path.exists() {
        return Ok(0);
    }
    // Briefly open the cache to ask for its high-water mark, then drop the
    // handle so the rebuild path (if it fires) can replace the file.
    let store = open_fact_store(facts_db_path)?;
    let query = store
        .query(&FactQuery::all())
        .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
    let max = query
        .event_records
        .last()
        .map(|record| record.sequence_number)
        .unwrap_or(0);
    i64::try_from(max)
        .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))
}

fn rebuild_db_from_ledger(ledger_path: &Path, facts_db_path: &Path) -> Result<()> {
    // Drop the cache file (and its sqlite sidecars) so factstr assigns
    // sequence_numbers 1,2,3,… in the exact order they appear in the ledger.
    // Replay-equivalence depends on this. Idempotent: replaying the same
    // ledger twice yields the same db.
    let _ = fs::remove_file(facts_db_path);
    let _ = fs::remove_file(facts_db_path.with_extension("db-shm"));
    let _ = fs::remove_file(facts_db_path.with_extension("db-wal"));

    let file = fs::File::open(ledger_path)
        .map_err(RallyError::io(format!("read {}", ledger_path.display())))?;
    let store = open_fact_store(facts_db_path)?;
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(RallyError::io(format!("read {}", ledger_path.display())))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: LedgerLine = serde_json::from_str(&line).map_err(RallyError::json(format!(
            "parse {} line {}",
            ledger_path.display(),
            idx + 1
        )))?;
        let result = store
            .append(vec![NewEvent::new(entry.event_type, entry.payload)])
            .map_err(|err| RallyError::Message(format!("replay ledger: {err}")))?;
        let assigned = i64::try_from(result.last_sequence_number)
            .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
        if assigned != entry.seq {
            return Err(RallyError::Message(format!(
                "ledger replay seq mismatch at line {}: expected {} got {}",
                idx + 1,
                entry.seq,
                assigned
            )));
        }
    }
    Ok(())
}

fn seed_ledger_from_db(ledger_path: &Path, facts_db_path: &Path) -> Result<()> {
    let store = open_fact_store(facts_db_path)?;
    let query = store
        .query(&FactQuery::all())
        .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
    if let Some(parent) = ledger_path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(ledger_path)
        .map_err(RallyError::io(format!("create {}", ledger_path.display())))?;
    for record in query.event_records {
        let seq = i64::try_from(record.sequence_number)
            .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
        let entry = LedgerLine {
            seq,
            occurred_at: record.occurred_at.to_string(),
            event_type: record.event_type,
            payload: record.payload,
        };
        let line = serde_json::to_string(&entry).map_err(RallyError::json("render ledger line"))?;
        writeln!(file, "{line}")
            .map_err(RallyError::io(format!("write {}", ledger_path.display())))?;
    }
    file.sync_all()
        .map_err(RallyError::io(format!("fsync {}", ledger_path.display())))?;
    Ok(())
}

fn append_ledger_line(ledger_path: &Path, entry: &LedgerLine) -> Result<()> {
    if let Some(parent) = ledger_path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let line = serde_json::to_string(entry).map_err(RallyError::json("render ledger line"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path)
        .map_err(RallyError::io(format!("open {}", ledger_path.display())))?;
    // One write call → atomic per POSIX for small lines on a local FS.
    // `merge=union` in `.gitattributes` handles concurrent worktree appends.
    writeln!(file, "{line}").map_err(RallyError::io(format!("write {}", ledger_path.display())))?;
    file.sync_data()
        .map_err(RallyError::io(format!("fsync {}", ledger_path.display())))?;
    Ok(())
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

    /// The headline guarantee: `ledger.jsonl` is canonical and `facts.db` is a
    /// pure derived cache. Delete the cache, reopen, and the room must
    /// reconstruct identically — same seqs, same payloads, same snapshot.
    #[test]
    fn round_trip_db_rebuilds_from_ledger() {
        let root = unique_root("ledger-roundtrip");
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

        // Delete the derived cache. Ledger remains.
        let facts_db = root.join(".rally/facts.db");
        let ledger = root.join(".rally/ledger.jsonl");
        assert!(ledger.exists(), "ledger.jsonl must persist as canonical");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));
        assert!(!facts_db.exists(), "cache deleted for replay test");

        // Reopen → reconcile replays ledger into a fresh cache.
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

    /// First-run upgrade: a pre-existing room with a db but no ledger seeds
    /// the canonical ledger from the cache so no history is lost.
    #[test]
    fn seed_ledger_from_existing_db() {
        let root = unique_root("ledger-bootstrap");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim a"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided b"))
            .unwrap();
        drop(store);

        // Simulate "upgraded from a pre-ledger version of rally": delete
        // the ledger but keep the db.
        let ledger = root.join(".rally/ledger.jsonl");
        fs::remove_file(&ledger).unwrap();
        assert!(!ledger.exists());
        assert!(root.join(".rally/facts.db").exists());

        // Reopen → reconcile seeds the ledger from the db.
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert!(ledger.exists(), "ledger must be seeded from db");

        let lines: Vec<String> = fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);

        // Now delete the db and confirm the seeded ledger round-trips.
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
}
