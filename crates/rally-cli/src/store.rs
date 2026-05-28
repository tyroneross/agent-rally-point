use factstr::{EventQuery as FactQuery, EventStore, NewEvent};
use factstr_sqlite::SqliteStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use crate::backends::ManagedSession;
use crate::cli::RoomArgs;
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) produces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) depends: Vec<String>,
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

/// One step in a handoff's life, kept for transparency in a receipt.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ReceiptStep {
    pub(crate) event_id: String,
    pub(crate) kind: FactKind,
    pub(crate) tool: Option<String>,
    pub(crate) seq: i64,
}

/// The derived lifecycle of a delegated unit of work (a handoff), read straight
/// from the ref chain. Rally *renders* the chain; it does not verify the work.
/// `self_reported` is always true: `evidence` is the producer's own claim, and
/// `state` reflects which facts were written, not whether they are accurate.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct Receipt {
    pub(crate) handoff_event_id: String,
    pub(crate) subject: String,
    pub(crate) from_tool: Option<String>,
    pub(crate) to_target: Option<String>,
    pub(crate) created_at: String,
    pub(crate) seq: i64,
    /// delivered | acted | acknowledged | blocked | completed
    pub(crate) state: &'static str,
    pub(crate) evidence: Vec<String>,
    pub(crate) self_reported: bool,
    pub(crate) chain: Vec<ReceiptStep>,
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
    pub(crate) receipts: Vec<Receipt>,
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
            receipts: filter_receipts(self.receipts, query),
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
    pub(crate) receipts: usize,
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
            receipts: snapshot.receipts.len(),
        }
    }
}

pub(crate) struct RoomStore {
    fact_store: SqliteStore,
    cursor_path: PathBuf,
}

impl RoomStore {
    pub(crate) fn open() -> Result<Self> {
        let root = repo_root()?;
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).map_err(RallyError::io("create .rally"))?;
        let _ = fs::remove_file(dir.join("room.db"));
        let fact_store_path = dir.join("facts.db");
        let fact_store = SqliteStore::open(&fact_store_path)
            .map_err(|err| RallyError::Message(format!("open fact store: {err}")))?;
        Ok(Self {
            fact_store,
            cursor_path: dir.join("cursors.json"),
        })
    }

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        let mut fact = fact.clone();
        let event_type = fact.kind.as_str().to_string();
        let payload = serde_json::to_value(&fact).map_err(RallyError::json("render fact"))?;
        let result = self
            .fact_store
            .append(vec![NewEvent::new(event_type, payload)])
            .map_err(|err| RallyError::Message(format!("append fact: {err}")))?;
        fact.seq = result.last_sequence_number as i64;
        Ok(fact)
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        let query = self
            .fact_store
            .query(&FactQuery::all())
            .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
        query
            .event_records
            .into_iter()
            .map(|record| Fact::from_value(record.payload, record.sequence_number as i64))
            .collect()
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
        let receipts = build_receipts(&facts, &resolved);
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
            receipts,
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
}

fn filter_facts(facts: Vec<Fact>, query: &RoomQuery) -> Vec<Fact> {
    facts
        .into_iter()
        .filter(|fact| query.matches(fact))
        .collect()
}

/// Walk each handoff's ref chain into a lifecycle receipt. `resolved` is the set
/// of event ids that a resolve/release fact points at (the acknowledged/closed
/// set). State precedence: completed (an artifact landed) > blocked (an open
/// blocker refs it) > acknowledged (a resolve refs it) > acted (a claim refs it)
/// > delivered. This only reads what facts exist; it does not verify them.
fn build_receipts(facts: &[Fact], resolved: &BTreeSet<String>) -> Vec<Receipt> {
    facts
        .iter()
        .filter(|fact| fact.kind == "handoff")
        .map(|handoff| {
            let chain_facts = facts
                .iter()
                .filter(|fact| fact.ref_id.as_deref() == Some(handoff.event_id.as_str()))
                .collect::<Vec<_>>();
            let artifact = chain_facts.iter().find(|fact| fact.kind == "artifact");
            let open_blocker = chain_facts
                .iter()
                .any(|fact| fact.kind == "blocker" && !resolved.contains(&fact.event_id));
            let acted = chain_facts.iter().any(|fact| fact.kind == "claim");
            let acknowledged = resolved.contains(&handoff.event_id);
            let state = if artifact.is_some() {
                "completed"
            } else if open_blocker {
                "blocked"
            } else if acknowledged {
                "acknowledged"
            } else if acted {
                "acted"
            } else {
                "delivered"
            };
            let evidence = artifact
                .map(|fact| fact.evidence.clone())
                .unwrap_or_default();
            let mut chain = chain_facts
                .iter()
                .map(|fact| ReceiptStep {
                    event_id: fact.event_id.clone(),
                    kind: fact.kind.clone(),
                    tool: fact.tool.clone(),
                    seq: fact.seq,
                })
                .collect::<Vec<_>>();
            chain.sort_by_key(|step| step.seq);
            Receipt {
                handoff_event_id: handoff.event_id.clone(),
                subject: handoff.subject.clone(),
                from_tool: handoff.tool.clone(),
                to_target: handoff.target.clone(),
                created_at: handoff.created_at.clone(),
                seq: handoff.seq,
                state,
                evidence,
                self_reported: true,
                chain,
            }
        })
        .collect()
}

fn filter_receipts(receipts: Vec<Receipt>, query: &RoomQuery) -> Vec<Receipt> {
    receipts
        .into_iter()
        .filter(|receipt| {
            query.tool.as_deref().is_none_or(|tool| {
                receipt.from_tool.as_deref() == Some(tool)
                    || receipt.to_target.as_deref() == Some(tool)
            })
        })
        .collect()
}
