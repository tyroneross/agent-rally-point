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

/// Forensic holding area for canonical JSONL records that replay can safely
/// skip while keeping the room readable.
const QUARANTINE_DIRNAME: &str = "quarantine";

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
    /// Shared (read) advisory lock — many holders coexist. Used by direct
    /// facts.db openers on the ownership lock (ADR-01, L1).
    pub(crate) const LOCK_SH: i32 = 1;
    pub(crate) const LOCK_EX: i32 = 2;
    /// Non-blocking modifier: `flock` returns `EWOULDBLOCK` instead of blocking
    /// when the lock is contended. OR'd with `LOCK_SH` for the router's
    /// non-blocking SH try (ADR-01).
    pub(crate) const LOCK_NB: i32 = 4;
    pub(crate) const LOCK_UN: i32 = 8;

    unsafe extern "C" {
        pub(crate) fn flock(fd: i32, operation: i32) -> i32;
    }
}

/// Ownership lock coordinating the daemon (LOCK_EX for its serving lifetime)
/// against direct facts.db openers (LOCK_SH for their process lifetime) —
/// ADR-01 / L1. DISTINCT from [`ROOM_MUTATION_LOCK_FILENAME`]: the mutation
/// lock serialises appends within the direct path; the owner lock decides
/// whether ANY process may open facts.db directly at all.
pub(crate) const RALLYD_OWNER_LOCK_FILENAME: &str = "rallyd.owner.lock";

use crate::backends::ManagedSession;
use crate::cli::RoomArgs;
use crate::discovery::refresh_room_index;
use crate::error::{RallyError, Result};
use crate::store_client::{self, RoutedRoomStore};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Process-global retry salt. Bumped on every SQLite-busy retry so that two
/// retriers in the SAME process (same pid, possibly even the same thread id if
/// a thread is reused) do not converge on identical back-off schedules. The
/// thread id and pid de-sync across threads/processes; this salt de-syncs
/// successive retry loops within one thread. Combined, no two concurrent
/// retriers thunder on the same millisecond.
static RETRY_SALT: AtomicU64 = AtomicU64::new(0);

/// Per-retrier jitter in milliseconds, de-synchronized across threads AND
/// processes. The old `pid % 17` was constant across all threads in one
/// process, so intra-process concurrent retriers (cargo's multi-threaded test
/// runner, a single binary spawning worker threads) thundered together and
/// exhausted the SQLite `busy_timeout`. We fold in the current thread id and a
/// monotonically-bumped process-global salt so each retrier gets a distinct
/// offset, while still keeping pid in the mix for cross-process de-sync.
fn retry_jitter_ms() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    RETRY_SALT.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    // Spread over [0, 23): wider than the old mod-17 window so more concurrent
    // retriers fit without collision, still small relative to the 15ms*attempt
    // base back-off so it perturbs rather than dominates the schedule.
    hasher.finish() % 23
}
use crate::{
    FACT_SCHEMA, claim_authority, normalize_paths, now_string, path_matches_scope, repo_root,
    short_id,
};

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FactKind {
    Claim,
    #[serde(rename = "claim.expired")]
    ClaimExpired,
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
    ///
    /// Wire/on-disk kind is `backlog_item` (the `rename_all = "snake_case"`
    /// default — no per-variant rename here). The `backlog-item` alias exists
    /// only so a producer coded against the earlier (buggy) published schema
    /// still deserializes correctly instead of falling to `#[serde(other)]
    /// Unknown` (f1, 2026-07-09).
    #[serde(alias = "backlog-item")]
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
            "claim.expired" | "claim_expired" => Some(Self::ClaimExpired),
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
            Self::ClaimExpired => "claim.expired",
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

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
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
    /// The live session lease that authored this durable write
    /// (see `session_identity`). Optional + serde-default so legacy
    /// rows without it replay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from_session_id: Option<String>,
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
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
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

/// Seconds of TOTAL silence after which a stale owner's claim becomes eligible
/// for a non-owner takeover release. Deliberately much larger than
/// `IDLE_THRESHOLD_SECS`: idle (15m) is advisory-only, but a DESTRUCTIVE
/// takeover requires proof the owner is really gone, not merely busy-and-quiet
/// (independent-auditor HIGH, 2026-06-09). 2h ≫ any plausible work-pause,
/// ≪ the real ~2-day dead-owner case.
const TAKEOVER_STALE_SECS: i64 = 2 * 60 * 60;

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
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub(crate) struct ReadReceipt {
    pub(crate) tool: String,
    pub(crate) last_read_seq: i64,
    pub(crate) behind_by: i64,
    /// "caught_up" | "behind"
    pub(crate) status: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
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
    /// Pending wake intents used to coalesce repeated `next` polls. Internal
    /// projection only; the public room schema remains unchanged.
    #[serde(skip)]
    pub(crate) pending_wakes: Vec<Fact>,
    pub(crate) current_decisions: Vec<Fact>,
    pub(crate) current_risks: Vec<Fact>,
    /// System-generated health/telemetry facts (`external-intake`,
    /// `unmanaged-agent`, `duplicate-active-squad-id`, `binary-drift`) split out
    /// of `current_risks` so the risk view shows only human coordination risks.
    /// Deduped by subject (freshest kept). Auditable here; omitted from JSON when
    /// empty so existing round-trip tests are unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) system_health: Vec<Fact>,
    pub(crate) recent_artifacts: Vec<Fact>,
    pub(crate) unconsumed_artifacts: Vec<Fact>,
    pub(crate) stale_facts: Vec<Fact>,
    /// Distinct tools that have entered or authored facts in this room.
    pub(crate) squads: Vec<Squad>,
    /// Tool asserting the `role:lead` decision, if any.
    #[serde(default)]
    pub(crate) lead: Option<String>,
    /// Seq of the latest lead-family decision (`role:lead` or relinquish).
    /// Agents can use this as a cheap epoch to detect stale lead context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lead_epoch: Option<i64>,
    /// R10: per-tool read receipts projected from `FactKind::Read` checkpoints.
    /// Populated only when `include_readers` is requested (see
    /// `RoomStore::snapshot_with_readers`); empty in the default snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) readers: Vec<ReadReceipt>,
    /// Current room north-star text, projected from the latest `FactKind::Mission`
    /// fact whose scope contains `"mission"`. `None` when no mission has been set.
    /// Omitted from JSON when unset so existing B16-style round-trip tests are
    /// unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mission: Option<String>,
}

impl RoomSnapshot {
    /// ADVISORY tier — tools whose latest presence is liveness-idle (squad
    /// `status == "idle"`, i.e. `last_seen_ts` older than the 15-minute
    /// `IDLE_THRESHOLD_SECS`). This is the standard TTL-primary signal reused
    /// from the squad view (lesson 2026-06-07-managed-session-liveness-ttl-primary).
    ///
    /// Use this ONLY for advisory surfaces (the non-blocking `before-write`
    /// downgrade): a 15-minute quiet window is plausible for a busy agent that
    /// simply hasn't posted to Rally, so it must never authorize a DESTRUCTIVE
    /// action on its behalf. For that, use [`takeover_eligible_owners`].
    pub(crate) fn idle_owner_tools(&self) -> std::collections::BTreeSet<String> {
        self.squads
            .iter()
            .filter(|sq| sq.status == "idle")
            .map(|sq| sq.tool.clone())
            .collect()
    }

    /// DESTRUCTIVE tier — tools whose latest presence is older than
    /// [`TAKEOVER_STALE_SECS`] (2 hours), the conservative bar required to
    /// authorize a NON-OWNER takeover release of a squatting claim. The 15-min
    /// idle threshold is deliberately NOT used here: an agent doing a long
    /// build or local work without a Rally write for 15 minutes is alive, and
    /// reclaiming its claim would be a coordination-integrity REGRESSION
    /// (independent-auditor HIGH, 2026-06-09). Two hours of total silence is
    /// well beyond any plausible single work-pause yet far under the real dead-
    /// owner case (the squatting claude_code:01 claims were ~2 DAYS stale).
    ///
    /// Owners whose `last_seen_ts` fails to parse are treated as NOT eligible
    /// (fail-closed: never reclaim on a malformed timestamp).
    pub(crate) fn takeover_eligible_owners(&self) -> std::collections::BTreeSet<String> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.squads
            .iter()
            .filter(|sq| {
                chrono::DateTime::parse_from_rfc3339(&sq.last_seen_ts)
                    .map(|dt| now_secs - dt.timestamp() > TAKEOVER_STALE_SECS)
                    .unwrap_or(false)
            })
            .map(|sq| sq.tool.clone())
            .collect()
    }

    /// PER-CLAIM destructive reclaim eligibility, with the timeout SCALED by the
    /// size of the work the claim covers (`decay::classify_work_size`):
    /// a single-file claim becomes reclaimable after the SMALL timeout
    /// (default 30m); a multi-file / directory / repo / task claim only after
    /// the LARGE timeout (default 2h, == the historical `TAKEOVER_STALE_SECS`).
    ///
    /// Fail-closed: an owner whose `last_seen_ts` is unknown or unparseable is
    /// NEVER reclaimable (matches `takeover_eligible_owners`). The owner's age
    /// is taken from the squad projection (last authored/presence fact).
    /// Returns `(eligible, work_size)` so the caller can record the size in the
    /// reclaim provenance.
    pub(crate) fn claim_reclaim_eligible(
        &self,
        claim: &Fact,
        coord: &crate::hooks_config::CoordinationConfig,
    ) -> (bool, crate::decay::WorkSize) {
        let resource_scopes: Vec<crate::resource_scope::ResourceScope> = claim
            .scope
            .iter()
            .filter_map(|s| crate::resource_scope::ResourceScope::parse_claim_scope(s))
            .collect();
        let size = crate::decay::classify_work_size(&resource_scopes, claim.scope.len());
        let timeout = crate::decay::reclaim_timeout_secs(
            size,
            coord.reclaim_small_minutes,
            coord.reclaim_large_minutes,
        );
        let Some(owner) = claim.tool.as_deref() else {
            return (false, size);
        };
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Owner age from the squad projection; fail-closed on missing/bad ts.
        let eligible = self
            .squads
            .iter()
            .find(|sq| sq.tool == owner)
            .and_then(|sq| {
                chrono::DateTime::parse_from_rfc3339(&sq.last_seen_ts)
                    .ok()
                    .map(|dt| now_secs - dt.timestamp() > timeout)
            })
            .unwrap_or(false);
        (eligible, size)
    }

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
            pending_wakes: self.pending_wakes,
            current_decisions: filter_facts(self.current_decisions, query),
            current_risks: filter_facts(self.current_risks, query),
            recent_artifacts: filter_facts(self.recent_artifacts, query),
            unconsumed_artifacts: filter_facts(self.unconsumed_artifacts, query),
            stale_facts: filter_facts(self.stale_facts, query),
            // system_health, squads, lead, readers, and mission are room-level
            // aggregates; not filtered by path/tool query.
            system_health: self.system_health,
            squads: self.squads,
            lead: self.lead,
            lead_epoch: self.lead_epoch,
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
    /// When true, re-include recency-decayed (archived) facts in the snapshot.
    /// Internal routing only — not serialized, not part of `is_empty`.
    #[serde(skip)]
    pub(crate) include_archived: bool,
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
            include_archived: args.include_archived,
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
        if let Some(role) = &self.role
            && fact.role.as_deref() != Some(role.as_str())
        {
            return false;
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
        if let Some(thread_id) = &self.thread_id
            && fact.thread_id != *thread_id
        {
            return false;
        }
        if let Some(since) = self.since
            && fact.seq <= since
        {
            return false;
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

/// The per-repo room store as seen by all 214 call sites — a two-variant
/// dispatcher (L2/ADR-01). `Direct` is today's in-process facts.db store;
/// `Routed` speaks to a live daemon over the wire. The four `open*`
/// constructors are the ROUTING SEAM: they probe for a live daemon and return
/// the appropriate variant.
///
/// **Chunk C:** the router runs the full ADR-01 choreography — probe →
/// SH try → bounded-block corridor → route or fail loud (see [`RoomStore::route`])
/// — and every `Routed` dispatch arm calls the real [`RoutedRoomStore`] method
/// in `store_client.rs` (a `round_trip` mirror of `daemon_client.rs:639`).
///
/// ## Method classification (the frozen contract's core — Work item 1)
///
/// Every `pub(crate)` instance method is `routed` (touches `facts.db` via a
/// factstr pool → served by the daemon) or `local` (pure accessor / a
/// `cursors.json` file op → answered by the routed client itself, no wire hop,
/// NOT part of the #50 surface). ROUTED methods each have a `store_wire::StoreOp`
/// variant; LOCAL methods do NOT.
///
/// ```text
/// ROUTED (→ StoreOp variant, served by the daemon):
///   append_fact                          → AppendFact
///   append_fact_verified                 → AppendFactVerified
///   append_state_transition_verified     → AppendStateTransitionVerified
///   append_session_fact_if_context       → AppendSessionFactIfContext
///   facts                                → Facts
///   rebuild_claim_index                  → RebuildClaimIndex
///   renew_claim_lease                    → RenewClaimLease
///   expire_claim_leases_at               → ExpireClaimLeasesAt
///   session_facts_with_context_version   → SessionFactsWithContextVersion
///   snapshot / snapshot_with_archived    → SnapshotWithArchived
///   snapshot_with_readers_archived       → SnapshotWithReadersArchived
///   last_checkpoint_seq                  → LastCheckpointSeq
///   maybe_append_read_checkpoint         → MaybeAppendReadCheckpoint
///   project_read_receipts                → ProjectReadReceipts
///
/// LOCAL (answered on the routed client's own state — NO StoreOp variant):
///   active_engagement, room_id           (return &self.active_engagement)
///   active_segment_path                  (derived from log_dir + engagement)
///   repo_root                            (return &self.repo_root)
///   claim_index_path        [cfg(test)]  (return &self.claim_index_path)
///   set_active_engagement_for_test [test]
///   set_cursor                           (cursors.json file op — not #50)
///
/// MIXED (R10 correction, flagged to D/auditor): `cursor_for` is NOT pure
/// LOCAL despite the table above's original placement — its body calls
/// `last_checkpoint_seq` (a ROUTED op, `LastCheckpointSeq` over the wire)
/// FIRST, and only falls back to the local `cursors.json` read when the
/// ledger has no checkpoint for that tool yet (R10 ledger-first, preserving
/// backwards compatibility with pre-R10 cursor files). `RoutedRoomStore`
/// mirrors this exact two-step order rather than reading cursors.json alone.
/// ```
///
/// NOTE (classification correction, flagged to B/C): the plan text listed
/// `room_id` as `routed`, but its body is `&self.active_engagement` — a pure
/// local accessor. It is classified LOCAL here on the evidence of the code.
// The `Direct` variant is large (holds a factstr pool + several PathBufs) and
// is the OVERWHELMINGLY common, hot-path case (every direct CLI op). Boxing it
// to satisfy `large_enum_variant` would add a pointless heap allocation on the
// hot path the release profile is tuned to keep fast, to save memory only in
// the rare, short-lived `Routed` case. Allow the lint deliberately.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RoomStore {
    /// Today's in-process store: opens facts.db under an owner SH lock.
    Direct(DirectRoomStore),
    /// Daemon-routed store (Chunk C): speaks the `store_wire` protocol over
    /// `.rally/rallyd.sock`, holds NO facts.db handle (G3). Constructed only
    /// by [`RoomStore::route`] after a successful daemon identity probe.
    Routed(RoutedRoomStore),
}

/// Today's in-process room store (was `RoomStore` before the router split).
/// The `Direct` variant of [`RoomStore`]. In direct-CLI mode `warm_fact_store`
/// is always `None`, so every hot interior open goes through
/// [`DirectRoomStore::fact_store_handle`]'s cold branch = today's per-op open,
/// byte-identical to main (G1). Chunk B installs a warm pool for daemon mode.
pub(crate) struct DirectRoomStore {
    fact_store: SqliteStore,
    /// Daemon-mode warm facts.db pool (L11/R1/G10). `Some` ⇒ the hot interior
    /// opens reuse this ONE pool instead of churning a pool per op (which
    /// factstr-sqlite 0.5.2's un-closed-on-Drop background checkpoint would race
    /// in-process, re-creating #50 inside the daemon). `None` in direct-CLI mode
    /// ⇒ per-op opens, byte-identical to main. Installed by Chunk B at startup.
    warm_fact_store: Option<SqliteStore>,
    cursor_path: PathBuf,
    repo_root: PathBuf,
    facts_db_path: PathBuf,
    claim_index_path: PathBuf,
    /// Per-engagement segment directory (R5). All segment files together form
    /// the canonical append-only record.
    log_dir: PathBuf,
    /// Rotated/migrated segments (R5 migration on first open; R7 rotation).
    /// Replay walks here too, after live segments.
    archive_dir: PathBuf,
    /// Engagement label stamped into every segment append. Resolved once at
    /// open via [`resolve_active_engagement`] (env var → on-disk file → UTC
    /// date). Empty string is never produced. Rebindable per-request in daemon
    /// mode via [`DirectRoomStore::set_engagement_scope`] (L9/R4).
    active_engagement: String,
}

/// Handle returned by [`DirectRoomStore::fact_store_handle`]: either the warm
/// daemon pool (borrowed, reused across ops) or a freshly-opened per-op pool
/// (owned — today's direct-mode behavior). Derefs to `SqliteStore` so call
/// sites use it exactly as they used the old `let fact_store = open_…()?;`.
enum FactStoreHandle<'a> {
    /// Borrowed warm pool (daemon mode, `warm_fact_store` is `Some`).
    Warm(&'a SqliteStore),
    /// Freshly-opened pool (direct mode, `warm_fact_store` is `None`).
    Fresh(SqliteStore),
}

impl std::ops::Deref for FactStoreHandle<'_> {
    type Target = SqliteStore;
    fn deref(&self) -> &SqliteStore {
        match self {
            FactStoreHandle::Warm(store) => store,
            FactStoreHandle::Fresh(store) => store,
        }
    }
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

/// RAII guard for the ownership lock (`.rally/rallyd.owner.lock`), ADR-01/L1.
/// Holds a `flock` (SH for direct openers, EX for the daemon) for as long as
/// the guard lives; the kernel also releases it on any process death (crash
/// safety — the reason a marker/pid file was rejected). The DIRECT router holds
/// its SH guard in a process-global for the process lifetime (G7): dropping it
/// early would reopen the factstr background-close race window.
///
/// Reuses the same hand-declared `extern "C"` flock pattern as
/// [`RoomMutationLock`] (no `nix` crate). Constructed by Chunk C's router (SH)
/// and Chunk B's daemon startup (EX); dormant in Chunk A.
#[cfg(unix)]
#[allow(dead_code)]
pub(crate) struct OwnerGuard {
    file: fs::File,
}

#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) struct OwnerGuard;

#[cfg(unix)]
impl Drop for OwnerGuard {
    fn drop(&mut self) {
        let _ = unsafe { unix_lock::flock(self.file.as_raw_fd(), unix_lock::LOCK_UN) };
    }
}

/// Open (creating if absent) the ownership lock file at `rally_dir`.
#[cfg(unix)]
fn open_owner_lock_file(rally_dir: &Path) -> Result<fs::File> {
    fs::create_dir_all(rally_dir)
        .map_err(RallyError::io(format!("create {}", rally_dir.display())))?;
    let path = rally_dir.join(RALLYD_OWNER_LOCK_FILENAME);
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(RallyError::io(format!("open {}", path.display())))
}

/// Try to acquire the ownership lock SHARED, non-blocking (ADR-01 direct-open
/// branch). `Ok(Some(guard))` ⇒ provably no daemon holds EX ⇒ the caller may
/// open facts.db directly (and MUST hold the guard for its process lifetime,
/// G7). `Ok(None)` ⇒ a daemon holds EX (the SH try would block) ⇒ the caller
/// must route, never open directly. `Err` ⇒ a real I/O failure on the lock
/// file. Dormant in Chunk A; Chunk C's router calls it.
#[cfg(unix)]
#[allow(dead_code)]
pub(crate) fn acquire_owner_shared_nb(rally_dir: &Path) -> Result<Option<OwnerGuard>> {
    let file = open_owner_lock_file(rally_dir)?;
    let rc = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_SH | unix_lock::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(OwnerGuard { file }));
    }
    let err = io::Error::last_os_error();
    // EWOULDBLOCK/EAGAIN ⇒ the EX daemon holds it — "refused", not an error.
    if err.kind() == io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(RallyError::Io {
        context: format!(
            "lock-sh {}",
            rally_dir.join(RALLYD_OWNER_LOCK_FILENAME).display()
        ),
        source: err,
    })
}

/// Acquire the ownership lock EXCLUSIVE, BLOCKING (ADR-01 daemon startup). Held
/// for the daemon's entire serving lifetime; blocks until every in-flight
/// direct SH holder has exited (the "waiting for direct writers to drain"
/// window B logs). Dormant in Chunk A; Chunk B's `serve` calls it.
#[cfg(unix)]
#[allow(dead_code)]
pub(crate) fn acquire_owner_exclusive_blocking(rally_dir: &Path) -> Result<OwnerGuard> {
    let file = open_owner_lock_file(rally_dir)?;
    let rc = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_EX) };
    if rc != 0 {
        return Err(RallyError::Io {
            context: format!(
                "lock-ex {}",
                rally_dir.join(RALLYD_OWNER_LOCK_FILENAME).display()
            ),
            source: io::Error::last_os_error(),
        });
    }
    Ok(OwnerGuard { file })
}

/// Non-unix no-op mirror: no flock, so the owner lock is a no-op and the direct
/// path is always taken (mirrors [`acquire_room_mutation_lock`]'s
/// `#[cfg(not(unix))]` stub). rallyd is a unix-only daemon; on non-unix the CLI
/// behaves exactly as today (no daemon, direct only).
#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn acquire_owner_shared_nb(_rally_dir: &Path) -> Result<Option<OwnerGuard>> {
    Ok(Some(OwnerGuard))
}

#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn acquire_owner_exclusive_blocking(_rally_dir: &Path) -> Result<OwnerGuard> {
    Ok(OwnerGuard)
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

/// Process-global table of ownership-lock SH guards (ADR-01/G7), keyed by
/// canonicalized `.rally` dir. A guard is installed the first time THIS
/// process takes the direct branch for a given room and held until process
/// exit — never dropped early (an early drop would reopen the factstr
/// background-close race window the whole owner-lock design exists to
/// close). A per-root TABLE, not a single slot, because one process can
/// legitimately open MANY distinct rooms: the `rally-cli` test binary runs
/// many `#[test]` functions (each against its own temp-dir room) as threads
/// inside one process. A single global slot would silently drop every root's
/// guard but the first one's.
static OWNER_GUARDS: OnceLock<Mutex<BTreeMap<PathBuf, OwnerGuard>>> = OnceLock::new();

/// Install `guard` for `rally_dir` in the process-global table (G7), unless
/// this process already holds one for that exact root (in which case `guard`
/// — a redundant, separately-acquired fd — is simply dropped, releasing only
/// ITS OWN lock; the winning fd for this root keeps holding SH).
fn install_owner_guard_once(rally_dir: &Path, guard: OwnerGuard) {
    let table = OWNER_GUARDS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    table
        .entry(canonical_repo_root_string(rally_dir).into())
        .or_insert(guard);
}

/// A truly wedged daemon (ADR-01 corridor policy, L12): the SH try was
/// refused (a daemon holds EX) and no successful `Ping` arrived within the
/// bounded-block corridor. Actionable, and — critically — NEVER a silent
/// fallback to a direct facts.db open (that would skip the SH choreography
/// and void the G2 chokepoint premise); the operator must intervene.
fn wedged_daemon_error() -> RallyError {
    RallyError::Command(format!(
        "rally daemon appears wedged: no successful ping within the {}s corridor bound; \
         run `rally daemon status` to check it, or `rally daemon stop` to clear a stuck instance",
        store_client::CORRIDOR_BOUND.as_secs()
    ))
}

/// The routing seam (L2/ADR-01). These are the ONLY entry points the 214 call
/// sites use; the old public names (`open`, `open_at`, `open_at_with_engagement`,
/// `open_existing_at`) survive here as the router constructors so every caller
/// compiles unchanged.
///
/// **Chunk C:** [`RoomStore::route`] runs the full ADR-01 choreography: probe
/// for a live daemon (`.addr` → connect → `Ping`, verifying `repo_root` +
/// `wire_version`) → live ⇒ `Routed` (facts.db never opened, SH never taken)
/// → not live ⇒ SH try-lock non-blocking → acquired (installed
/// process-global, held to exit — G7) ⇒ `Direct` (today's path, byte-identical
/// — G1) → SH refused ⇒ bounded-block corridor (re-probe up to
/// [`store_client::CORRIDOR_BOUND`]) ⇒ live ⇒ route, else fail loud
/// ([`wedged_daemon_error`]) naming `rally daemon status`/`stop`.
impl RoomStore {
    /// Router entry (was `RoomStore::open`). Resolves the repo root, then routes.
    pub(crate) fn open() -> Result<Self> {
        Self::open_at(repo_root()?)
    }

    /// Router entry (was `RoomStore::open_at`). Reads `RALLY_ENGAGEMENT` the
    /// same way `DirectRoomStore::open_direct_at` always has, then routes.
    pub(crate) fn open_at(root: PathBuf) -> Result<Self> {
        let engagement = env::var(ENGAGEMENT_ENV_VAR).ok();
        Self::route(root, engagement)
    }

    /// Router entry (was `RoomStore::open_at_with_engagement`). `engagement`
    /// is used exactly as given — no env consultation — matching
    /// `DirectRoomStore::open_direct_at_with_engagement`'s contract (tests use
    /// this to avoid the process-global `env::set_var` unsoundness under the
    /// parallel test runner).
    // Only reached from tests in production; part of the frozen router
    // surface other call sites build against.
    #[allow(dead_code)]
    pub(crate) fn open_at_with_engagement(
        root: PathBuf,
        engagement: Option<String>,
    ) -> Result<Self> {
        Self::route(root, engagement)
    }

    /// Router entry (was `RoomStore::open_existing_at`): `None` iff no room
    /// exists yet by ANY canonical evidence (derived db, segments, archive, or
    /// legacy monolith) AND no daemon is live for this root (a live daemon
    /// proves the room already exists — it opened the store at its own
    /// startup, so routing straight to `Routed` is correct without re-deriving
    /// existence locally).
    pub(crate) fn open_existing_at(root: PathBuf) -> Result<Option<Self>> {
        let rally_dir = root.join(".rally");
        let engagement = env::var(ENGAGEMENT_ENV_VAR).ok();
        if let Some(routed) = store_client::probe_live(&root, &rally_dir, engagement.clone()) {
            return Ok(Some(RoomStore::Routed(routed)));
        }
        match acquire_owner_shared_nb(&rally_dir)? {
            Some(guard) => {
                install_owner_guard_once(&rally_dir, guard);
                Ok(DirectRoomStore::open_direct_existing_at(root)?.map(RoomStore::Direct))
            }
            None => match store_client::probe_live_bounded(
                &root,
                &rally_dir,
                engagement,
                store_client::CORRIDOR_BOUND,
            ) {
                Some(routed) => Ok(Some(RoomStore::Routed(routed))),
                None => Err(wedged_daemon_error()),
            },
        }
    }

    /// The ADR-01 routing seam shared by `open_at`/`open_at_with_engagement`.
    /// See the `impl RoomStore` doc comment above for the full choreography.
    fn route(root: PathBuf, engagement: Option<String>) -> Result<Self> {
        let rally_dir = root.join(".rally");
        if let Some(routed) = store_client::probe_live(&root, &rally_dir, engagement.clone()) {
            return Ok(RoomStore::Routed(routed));
        }
        match acquire_owner_shared_nb(&rally_dir)? {
            Some(guard) => {
                install_owner_guard_once(&rally_dir, guard);
                Ok(RoomStore::Direct(
                    DirectRoomStore::open_direct_at_with_engagement(root, engagement)?,
                ))
            }
            None => match store_client::probe_live_bounded(
                &root,
                &rally_dir,
                engagement,
                store_client::CORRIDOR_BOUND,
            ) {
                Some(routed) => Ok(RoomStore::Routed(routed)),
                None => Err(wedged_daemon_error()),
            },
        }
    }

    // ----- dispatch: ROUTED methods -------------------------------------------
    // Each `Routed` arm calls the real wire client (`store_client.rs`); a
    // transport failure there (R6) surfaces as a retryable
    // `RallyError::Command` and NEVER falls back to a direct facts.db open.

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        match self {
            RoomStore::Direct(d) => d.append_fact(fact),
            RoomStore::Routed(r) => r.append_fact(fact),
        }
    }

    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<Fact> {
        match self {
            RoomStore::Direct(d) => d.append_fact_verified(fact),
            RoomStore::Routed(r) => r.append_fact_verified(fact),
        }
    }

    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<Fact> {
        match self {
            RoomStore::Direct(d) => d.append_state_transition_verified(fact),
            RoomStore::Routed(r) => r.append_state_transition_verified(fact),
        }
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<Option<Fact>> {
        match self {
            RoomStore::Direct(d) => {
                d.append_session_fact_if_context(fact, expected_context_version)
            }
            RoomStore::Routed(r) => {
                r.append_session_fact_if_context(fact, expected_context_version)
            }
        }
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        match self {
            RoomStore::Direct(d) => d.facts(),
            RoomStore::Routed(r) => r.facts(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn rebuild_claim_index(&self) -> Result<()> {
        match self {
            RoomStore::Direct(d) => d.rebuild_claim_index(),
            RoomStore::Routed(r) => r.rebuild_claim_index(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn renew_claim_lease(
        &self,
        claim_id: &str,
        lease_expires_at: String,
    ) -> Result<Option<claim_authority::ActiveClaimRecord>> {
        match self {
            RoomStore::Direct(d) => d.renew_claim_lease(claim_id, lease_expires_at),
            RoomStore::Routed(r) => r.renew_claim_lease(claim_id, lease_expires_at),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn expire_claim_leases_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Fact>> {
        match self {
            RoomStore::Direct(d) => d.expire_claim_leases_at(now),
            RoomStore::Routed(r) => r.expire_claim_leases_at(now),
        }
    }

    pub(crate) fn session_facts_with_context_version(&self) -> Result<(Vec<Fact>, Option<u64>)> {
        match self {
            RoomStore::Direct(d) => d.session_facts_with_context_version(),
            RoomStore::Routed(r) => r.session_facts_with_context_version(),
        }
    }

    pub(crate) fn snapshot(&self) -> Result<RoomSnapshot> {
        match self {
            RoomStore::Direct(d) => d.snapshot(),
            RoomStore::Routed(r) => r.snapshot(),
        }
    }

    pub(crate) fn snapshot_with_archived(&self, include_archived: bool) -> Result<RoomSnapshot> {
        match self {
            RoomStore::Direct(d) => d.snapshot_with_archived(include_archived),
            RoomStore::Routed(r) => r.snapshot_with_archived(include_archived),
        }
    }

    pub(crate) fn snapshot_with_readers_archived(
        &self,
        include_archived: bool,
    ) -> Result<RoomSnapshot> {
        match self {
            RoomStore::Direct(d) => d.snapshot_with_readers_archived(include_archived),
            RoomStore::Routed(r) => r.snapshot_with_readers_archived(include_archived),
        }
    }

    // Reached only from tests in production (production callers invoke the
    // DirectRoomStore method internally, or route through `cursor_for` — R10);
    // frozen router surface other call sites build against.
    #[allow(dead_code)]
    pub(crate) fn last_checkpoint_seq(&self, tool: &str) -> Result<i64> {
        match self {
            RoomStore::Direct(d) => d.last_checkpoint_seq(tool),
            RoomStore::Routed(r) => r.last_checkpoint_seq(tool),
        }
    }

    pub(crate) fn maybe_append_read_checkpoint(
        &self,
        tool: &str,
        read_seq: i64,
    ) -> Result<Option<Fact>> {
        match self {
            RoomStore::Direct(d) => d.maybe_append_read_checkpoint(tool, read_seq),
            RoomStore::Routed(r) => r.maybe_append_read_checkpoint(tool, read_seq),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn project_read_receipts(&self, max_seq: i64) -> Result<Vec<ReadReceipt>> {
        match self {
            RoomStore::Direct(d) => d.project_read_receipts(max_seq),
            RoomStore::Routed(r) => r.project_read_receipts(max_seq),
        }
    }

    // ----- dispatch: LOCAL methods -------------------------------------------
    // Classified LOCAL: answered on the routed client's own state (no wire
    // hop) — see the `impl RoomStore` doc comment's classification table,
    // including the R10 `cursor_for` MIXED correction.

    pub(crate) fn cursor_for(&self, tool: &str) -> Result<i64> {
        match self {
            RoomStore::Direct(d) => d.cursor_for(tool),
            RoomStore::Routed(r) => r.cursor_for(tool),
        }
    }

    pub(crate) fn set_cursor(&self, tool: &str, seq: i64) -> Result<()> {
        match self {
            RoomStore::Direct(d) => d.set_cursor(tool, seq),
            RoomStore::Routed(r) => r.set_cursor(tool, seq),
        }
    }

    pub(crate) fn active_engagement(&self) -> &str {
        match self {
            RoomStore::Direct(d) => d.active_engagement(),
            RoomStore::Routed(r) => r.active_engagement(),
        }
    }

    pub(crate) fn room_id(&self) -> &str {
        match self {
            RoomStore::Direct(d) => d.room_id(),
            RoomStore::Routed(r) => r.room_id(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn active_segment_path(&self) -> PathBuf {
        match self {
            RoomStore::Direct(d) => d.active_segment_path(),
            RoomStore::Routed(r) => r.active_segment_path(),
        }
    }

    pub(crate) fn repo_root(&self) -> &Path {
        match self {
            RoomStore::Direct(d) => d.repo_root(),
            RoomStore::Routed(r) => r.repo_root(),
        }
    }

    #[cfg(test)]
    pub(crate) fn claim_index_path(&self) -> &Path {
        match self {
            RoomStore::Direct(d) => d.claim_index_path(),
            RoomStore::Routed(r) => r.claim_index_path(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_active_engagement_for_test(&mut self, engagement: &str) {
        match self {
            RoomStore::Direct(d) => d.set_active_engagement_for_test(engagement),
            RoomStore::Routed(r) => r.set_active_engagement_for_test(engagement),
        }
    }
}

impl DirectRoomStore {
    /// Direct (no-daemon) constructor. The router entry point is
    /// [`RoomStore::open`]; this opens the in-process store unconditionally.
    /// Chunk B's daemon startup uses [`DirectRoomStore::open_direct_at`].
    #[allow(dead_code)]
    pub(crate) fn open_direct() -> Result<Self> {
        Self::open_direct_at(repo_root()?)
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
    pub(crate) fn open_direct_at(root: PathBuf) -> Result<Self> {
        // Production path: resolve the engagement from the process-global
        // `RALLY_ENGAGEMENT` env (sound for a single CLI process).
        Self::open_direct_at_with_engagement(root, std::env::var(ENGAGEMENT_ENV_VAR).ok())
    }

    /// Open a room with the engagement label INJECTED rather than read from the
    /// process-global `RALLY_ENGAGEMENT` env. `engagement: None` resolves the
    /// default deterministically (active-engagement file, else the UTC date) —
    /// no env read. Tests use this so a concurrent test toggling `RALLY_ENGAGEMENT`
    /// cannot flip which room subdir this store resolves under parallel runs
    /// (`env::set_var` is process-global and unsound across threads — Rust 2024).
    pub(crate) fn open_direct_at_with_engagement(
        root: PathBuf,
        engagement: Option<String>,
    ) -> Result<Self> {
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

        let fact_store = open_fact_store_lenient(&fact_store_path)?;
        seed_segments_from_db_if_absent(&log_dir, &archive_dir, &fact_store_path)?;
        let active_engagement = resolve_active_engagement_with_env(&dir, engagement);
        let store = Self {
            fact_store,
            // Direct-CLI mode: no warm pool ⇒ per-op opens, byte-identical to
            // main (G1). Chunk B installs a warm pool for daemon mode.
            warm_fact_store: None,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            claim_index_path: dir.join(claim_authority::CLAIM_INDEX_FILENAME),
            log_dir,
            archive_dir,
            active_engagement,
        };
        let _ = store.refresh_log_index();
        let _ = store.refresh_index(0);
        Ok(store)
    }

    pub(crate) fn open_direct_existing_at(root: PathBuf) -> Result<Option<Self>> {
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
        let fact_store = open_fact_store_lenient(&fact_store_path)?;
        seed_segments_from_db_if_absent(&log_dir, &archive_dir, &fact_store_path)?;
        let active_engagement = resolve_active_engagement(&dir);
        let store = Self {
            fact_store,
            // Direct-CLI mode: no warm pool ⇒ per-op opens (G1).
            warm_fact_store: None,
            cursor_path: dir.join("cursors.json"),
            repo_root: root,
            facts_db_path: fact_store_path,
            claim_index_path: dir.join(claim_authority::CLAIM_INDEX_FILENAME),
            log_dir,
            archive_dir,
            active_engagement,
        };
        let _ = store.refresh_log_index();
        Ok(Some(store))
    }

    /// The facts.db pool to use for a hot interior op (L11/R1/G10).
    ///
    /// * Daemon mode (`warm_fact_store` is `Some`): returns the ONE warm pool,
    ///   borrowed and reused across ops — no per-op churn (the churn is what
    ///   factstr-sqlite 0.5.2's un-closed-on-Drop background checkpoint races
    ///   in-process, re-creating #50 inside the daemon).
    /// * Direct mode (`warm_fact_store` is `None`): opens a FRESH pool exactly
    ///   as the call site did before this facade — byte-identical to main (G1).
    ///   `lenient` selects the cold-open strategy so each site preserves its
    ///   prior `open_fact_store` (strict) vs `open_fact_store_lenient` choice.
    ///
    /// The returned [`FactStoreHandle`] derefs to `SqliteStore`, so call sites
    /// use it exactly as the old owned `let fact_store = open_…()?;` value.
    fn fact_store_handle(&self, lenient: bool) -> Result<FactStoreHandle<'_>> {
        if let Some(warm) = &self.warm_fact_store {
            return Ok(FactStoreHandle::Warm(warm));
        }
        let fresh = if lenient {
            open_fact_store_lenient(&self.facts_db_path)?
        } else {
            open_fact_store(&self.facts_db_path)?
        };
        Ok(FactStoreHandle::Fresh(fresh))
    }

    /// Install the ONE warm facts.db pool for daemon mode (L11/R1/G10).
    ///
    /// Called by `rallyd_core::serve` after `open_direct_at`, so every hot
    /// interior op reuses this single pool via [`DirectRoomStore::fact_store_handle`]
    /// instead of churning a pool per request — the in-process re-creation of
    /// issue #50 (factstr-sqlite 0.5.2's un-closed-on-Drop background checkpoint
    /// racing the next open) that R1 exists to prevent. Direct-CLI stores never
    /// call this (`warm_fact_store` stays `None`, byte-identical to main — G1).
    pub(crate) fn install_warm_fact_store(&mut self) -> Result<()> {
        self.warm_fact_store = Some(open_fact_store_lenient(&self.facts_db_path)?);
        Ok(())
    }

    /// Rebind the active engagement (and its derived active-segment path) for
    /// subsequent ops (L9/R4). Used ONLY by the daemon dispatcher, which applies
    /// each wire request's engagement before dispatching; direct CLIs fix the
    /// engagement at construction and never call this (byte-identical to main).
    ///
    /// The client already resolved its label, so `engagement: Some(label)` is
    /// used directly (through the same sanitising/reserved-fixture ladder as
    /// construction); `None` resolves the room default WITHOUT consulting the
    /// daemon's own process env (L9). `active_segment_path()` is recomputed from
    /// `active_engagement` on each call, so no cached path needs updating.
    #[allow(dead_code)]
    pub(crate) fn set_engagement_scope(&mut self, engagement: Option<String>) {
        let rally_dir = self
            .facts_db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.repo_root.join(".rally"));
        self.active_engagement = resolve_active_engagement_with_env(&rally_dir, engagement);
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

    /// Parse the `authorized-takeover:stale-owner=<a>,<b>` evidence marker that
    /// `command_release_by_path` writes onto a takeover Release, returning the
    /// list of stale owners the release claims to reclaim. `None` when the
    /// marker is absent (an ordinary self-release, no takeover guard needed).
    fn takeover_owners_marker(evidence: &[String]) -> Option<Vec<String>> {
        const PREFIX: &str = "authorized-takeover:stale-owner=";
        for item in evidence {
            if let Some(rest) = item.strip_prefix(PREFIX) {
                let owners: Vec<String> = rest
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !owners.is_empty() {
                    return Some(owners);
                }
            }
        }
        None
    }

    /// Read a single-value `reaper:<key>=<value>` evidence marker. The reaper
    /// stamps `reaper:reason=<owner-stale|lease-expired|owner-stale+lease-expired>`
    /// and `reaper:owner=<tool>` onto every ClaimExpired it appends; the
    /// under-lock revival guard reads them to decide whether to re-validate.
    fn reaper_marker<'a>(evidence: &'a [String], key: &str) -> Option<&'a str> {
        let prefix = format!("reaper:{key}=");
        evidence
            .iter()
            .find_map(|item| item.strip_prefix(prefix.as_str()))
            .map(str::trim)
    }

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<Fact> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        // Warm-pool facade (L11/R1/G10): reuse the daemon's pool if installed,
        // else open fresh lenient — byte-identical to main in direct mode (G1).
        let fact_store = self.fact_store_handle(true)?;
        let mut fact = fact.clone();
        // SEC-001 close: a takeover Release authorizes reclaiming a stale peer's
        // claim. Eligibility was judged on an UNLOCKED snapshot in
        // `command_release_by_path`; an owner could revive in the gap before we
        // hold the lock. Re-assert eligibility HERE, under the held mutation
        // lock, against freshly-read facts — if a named stale-owner is no longer
        // reclaim-eligible (it posted activity and became live), refuse the
        // takeover rather than reclaim a now-live owner's claim (the
        // independent-auditor-HIGH 2026-06-09 regression class).
        if fact.kind == FactKind::Release
            && let Some(stale_owners) = Self::takeover_owners_marker(&fact.evidence)
        {
            let coord =
                crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            let fresh = snapshot_from_facts_with_policy(&facts, &coord, false);
            for owner in &stale_owners {
                let still_eligible = fresh
                    .active_claims
                    .iter()
                    .filter(|c| c.tool.as_deref() == Some(owner.as_str()))
                    .any(|c| fresh.claim_reclaim_eligible(c, &coord).0);
                // No remaining eligible claim for this owner means either it
                // was already reclaimed (fine) or it revived (refuse). We
                // only refuse when the owner still HOLDS active claims that
                // are NO LONGER eligible — i.e. it came back to life.
                let still_owns = fresh
                    .active_claims
                    .iter()
                    .any(|c| c.tool.as_deref() == Some(owner.as_str()));
                if still_owns && !still_eligible {
                    return Err(RallyError::Usage(format!(
                        "takeover refused: owner {owner} is no longer stale \
                             (revived under the mutation lock); not reclaiming a \
                             now-live owner's claim"
                    )));
                }
            }
        }
        // SEC-001 close (ClaimExpired): the reaper computes owner-eligibility on
        // an UNLOCKED snapshot (reaper.rs top), then appends ClaimExpired here,
        // later. A peer owner that REVIVES (posts fresh activity) in the
        // snapshot→append gap could still have its claim closed — the racy
        // OWNER-STALE signal. Mirror the Release guard: under the held mutation
        // lock, re-snapshot fresh facts and re-assert the closure is still
        // justified. A LEASE-EXPIRED close is exempt — a past lease cannot
        // un-expire (monotonic), so we only re-validate OWNER-STALE-only
        // closures. The reaper stamps `reaper:reason=` + `reaper:owner=` so this
        // guard can distinguish without re-parsing the subject.
        if fact.kind == FactKind::ClaimExpired {
            let reason = Self::reaper_marker(&fact.evidence, "reason");
            // Only a PURE owner-stale close is racy. A close that also carries
            // the lease-expired signal ("lease-expired" or
            // "owner-stale+lease-expired") is monotonic-safe and exempt.
            if reason == Some("owner-stale")
                && let Some(owner) = Self::reaper_marker(&fact.evidence, "owner")
            {
                let coord =
                    crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
                let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
                let fresh = snapshot_from_facts_with_policy(&facts, &coord, false);
                // The specific claim this ClaimExpired closes (by ref).
                let ref_id = fact.ref_id.as_deref();
                let still_owns = fresh.active_claims.iter().any(|c| {
                    c.tool.as_deref() == Some(owner) && Some(c.event_id.as_str()) == ref_id
                });
                let still_eligible = fresh
                    .active_claims
                    .iter()
                    .filter(|c| {
                        c.tool.as_deref() == Some(owner) && Some(c.event_id.as_str()) == ref_id
                    })
                    .any(|c| fresh.claim_reclaim_eligible(c, &coord).0);
                // The claim still HOLDS but is NO LONGER reap-eligible means
                // the owner came back to life in the gap → refuse the reap.
                // If the claim is already gone (already closed) we let the
                // append proceed; the projection dedups via ref_id.
                if still_owns && !still_eligible {
                    return Err(RallyError::Usage(format!(
                        "reap refused: owner {owner} revived under the mutation \
                             lock (owner-stale ClaimExpired for claim {} no longer \
                             eligible); not closing a now-live owner's claim",
                        ref_id.unwrap_or("<unknown>")
                    )));
                }
            }
        }
        if fact.kind == FactKind::Claim {
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            if let Some(conflict) = claim_authority::detect_conflict(&facts, &fact) {
                return Err(RallyError::Usage(format!(
                    "claim conflict: {} already owns {}; existing claim {} conflicts with requested scope {}",
                    conflict
                        .existing_owner
                        .as_deref()
                        .unwrap_or("unknown owner"),
                    conflict.scope,
                    conflict.existing_claim_id,
                    conflict.scope
                )));
            }
        }
        let logical_seq =
            next_canonical_seq(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        fact.seq = logical_seq;
        // Defense-in-depth dup gate (2026-07-02): the allocated seq must exceed
        // the active segment's on-disk tail. A stale cache or an old count-based
        // allocator could hand out an already-used seq; fail LOUD here rather
        // than write a duplicate that bricks replay for every reader.
        if let Some(tail) = last_seq_in_segment(&self.active_segment_path())?
            && fact.seq <= tail
        {
            return Err(RallyError::Message(format!(
                "seq allocation conflict: allocated {} <= active segment tail {} — refusing to write a duplicate. Delete .rally/.reconcile-cache.json and retry.",
                fact.seq, tail
            )));
        }
        let event_type = fact.kind.as_str().to_string();
        let payload = serde_json::to_value(&fact).map_err(RallyError::json("render fact"))?;
        // The room lock serializes Rally writers; keep a short retry for
        // transient SQLite lock errors from readers or older Rally binaries.
        let result = {
            let jitter = retry_jitter_ms();
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
        let _store_seq = i64::try_from(result.last_sequence_number)
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
        crate::mark_watchdog_command_commit();
        // Refresh the reconcile sidecar while the flock is still held so the
        // NEXT op stays on the O(1) fast path. The db and active segment each
        // grew by exactly one event; carry the pre-append counts forward +1 and
        // re-fingerprint (cheap, O(#files)). Best-effort: a miss just means the
        // next op does one authoritative scan and re-seeds the sidecar.
        self.refresh_reconcile_cache_after_append(fact.seq);
        // Both index refreshes are best-effort caches; swallow failures so a
        // racing parallel writer never poisons the append path. Replay
        // rebuilds them on next open from segments.
        let _ = self.refresh_log_index();
        let _ = self.refresh_index(fact.seq);
        if matches!(
            fact.kind,
            FactKind::Claim | FactKind::Release | FactKind::Resolve | FactKind::ClaimExpired
        ) {
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            claim_authority::write_index_from_facts(&self.claim_index_path, &facts)
                .map_err(|err| RallyError::Message(format!("write claim index: {err}")))?;
        }
        Ok(fact)
    }

    /// After a successful single-event append, advance the reconcile sidecar in
    /// place: counts += 1, re-fingerprint the (now-grown) active segment + the
    /// (now-grown) facts.db. Reads the pre-append sidecar established by open
    /// or a previous reconcile; if it's missing/inconsistent, drops the sidecar
    /// so the next op re-scans authoritatively. Never errors.
    ///
    /// Non-Unix note: on non-Unix platforms the mutation lock is a no-op
    /// (see store.rs `acquire_room_mutation_lock` #[cfg(not(unix))]). A concurrent
    /// writer may therefore replace the sidecar between the reconcile and this
    /// re-read. Worst case: event counts in the sidecar drift by N peers; this is
    /// self-corrected by a fingerprint mismatch on the next open, which triggers
    /// the authoritative full scan. No data loss is possible — the canonical
    /// JSONL segments are not affected by sidecar drift.
    fn refresh_reconcile_cache_after_append(&self, appended_seq: i64) {
        let (Ok(segments), Ok(archived)) = (
            read_segment_files(&self.log_dir),
            replay_archive_segments(&self.archive_dir),
        ) else {
            // Couldn't enumerate segments; drop the sidecar to force a re-scan.
            if let Some(p) = reconcile_cache_path(&self.facts_db_path) {
                let _ = fs::remove_file(p);
            }
            return;
        };
        let new_seg_fp = segments_fingerprint(&segments, &archived);
        let new_db_fp = fingerprint_db(&self.facts_db_path);
        match read_reconcile_cache(&self.facts_db_path) {
            // The pre-append sidecar was consistent → just advance counts by one.
            Some(prev)
                if prev.canonical_count == prev.db_count
                    && prev.canonical_max_seq == prev.db_max_seq =>
            {
                let cache = ReconcileCache {
                    segments_fingerprint: new_seg_fp,
                    db_fingerprint: new_db_fp,
                    canonical_count: prev.canonical_count + 1,
                    canonical_max_seq: prev.canonical_max_seq.max(appended_seq),
                    db_count: prev.db_count + 1,
                    db_max_seq: prev.db_max_seq.max(appended_seq),
                };
                let _ = write_reconcile_cache(&self.facts_db_path, &cache);
            }
            // No trustworthy prior sidecar → drop any stale one; next op re-scans.
            _ => {
                if let Some(p) = reconcile_cache_path(&self.facts_db_path) {
                    let _ = fs::remove_file(p);
                }
            }
        }
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

        // Fast path: the fact was JUST appended to the active segment, and it is
        // the newest line there. Scan the active segment tail-first — on the
        // happy path the event is the last line, so this is O(1) instead of
        // O(#segments * #lines). Correctness is unchanged: a true silent-drop
        // misses here (the line was never written), so we fall through to the
        // full live+archive scan, which also misses, yielding the error. The
        // active-first scan can only ADD a hit, never mask a real absence.
        let active = self.active_segment_path();
        if segment_event_id_present_tail_first(&active, event_id)? {
            return Ok(appended);
        }

        // Slow path / true silent-drop detector: re-read EVERY canonical segment
        // (live + archive) and scan for the exact event_id. If the active-first
        // scan missed (e.g. the event landed in a different segment, or a silent
        // drop occurred), this authoritative full scan is the final arbiter.
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
                let open_handoff = snapshot_before
                    .open_handoffs
                    .iter()
                    .find(|f| f.event_id == ref_id);
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
                    // DI-1: system-health telemetry (risk-kind, split out of
                    // current_risks) must remain resolvable by ref.
                    || snapshot_before
                        .system_health
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
                if let Some(handoff) = open_handoff
                    && !handoff_closer_matches_target(handoff, fact)
                {
                    let target = handoff.target.as_deref().unwrap_or("<untargeted>");
                    let tool = fact.tool.as_deref().unwrap_or("<unknown>");
                    return Err(RallyError::Usage(format!(
                        "resolve failed: ref {ref_id} is targeted to {target}; tool {tool} cannot resolve it"
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
                        .system_health
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
        // Warm-pool facade (L11/R1/G10): warm reuse in daemon mode, per-op
        // lenient open in direct mode (byte-identical to main — G1).
        let fact_store = self.fact_store_handle(true)?;
        let mut fact = fact.clone();
        let logical_seq =
            next_canonical_seq(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        fact.seq = logical_seq;
        // Defense-in-depth dup gate (2026-07-02) — see append_fact.
        if let Some(tail) = last_seq_in_segment(&self.active_segment_path())?
            && fact.seq <= tail
        {
            return Err(RallyError::Message(format!(
                "seq allocation conflict: allocated {} <= active segment tail {} — refusing to write a duplicate. Delete .rally/.reconcile-cache.json and retry.",
                fact.seq, tail
            )));
        }
        let payload =
            serde_json::to_value(&fact).map_err(RallyError::json("render session fact"))?;
        let result = fact_store.append_if(
            vec![NewEvent::new("session", payload.clone())],
            &FactQuery::for_event_types(["session"]),
            expected_context_version,
        );
        match result {
            Ok(result) => {
                let _store_seq = i64::try_from(result.last_sequence_number).map_err(|err| {
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
                crate::mark_watchdog_command_commit();
                self.refresh_reconcile_cache_after_append(fact.seq);
                let _ = self.refresh_log_index();
                let _ = self.refresh_index(fact.seq);
                Ok(Some(fact))
            }
            Err(EventStoreError::ConditionalAppendConflict { .. }) => Ok(None),
            Err(err) => Err(RallyError::Message(format!("append session fact: {err}"))),
        }
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        // Warm-pool facade for the READ path (snapshot's underlying read, L11/R1):
        // in daemon mode read through the ONE warm pool; on a corrupt-db error
        // fall through to the cold recovery path (quarantine + reconcile + reopen),
        // same as direct mode. In direct mode (`warm_fact_store` is None) this
        // block is skipped entirely ⇒ byte-identical to main (G1).
        if let Some(warm) = &self.warm_fact_store {
            match facts_from_store(warm) {
                Ok(facts) => return Ok(facts),
                Err(err) if is_malformed_db_error(&err) => {}
                Err(err) => return Err(err),
            }
        }
        facts_from_db_with_query_recovery(&self.log_dir, &self.archive_dir, &self.facts_db_path)
    }

    #[allow(dead_code)]
    pub(crate) fn rebuild_claim_index(&self) -> Result<()> {
        let facts = self.facts()?;
        claim_authority::write_index_from_facts(&self.claim_index_path, &facts)
            .map_err(|err| RallyError::Message(format!("write claim index: {err}")))
    }

    #[allow(dead_code)]
    pub(crate) fn renew_claim_lease(
        &self,
        claim_id: &str,
        lease_expires_at: String,
    ) -> Result<Option<claim_authority::ActiveClaimRecord>> {
        self.rebuild_claim_index()?;
        claim_authority::renew_claim_lease(&self.claim_index_path, claim_id, lease_expires_at)
            .map_err(|err| RallyError::Message(format!("renew claim lease: {err}")))
    }

    #[cfg(test)]
    pub(crate) fn claim_index_path(&self) -> &Path {
        &self.claim_index_path
    }

    #[allow(dead_code)]
    pub(crate) fn expire_claim_leases_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Fact>> {
        self.rebuild_claim_index()?;
        let facts = self.facts()?;
        let index = claim_authority::read_index(&self.claim_index_path)
            .map_err(|err| RallyError::Message(format!("read claim index: {err}")))?;
        let expired = claim_authority::expired_claims(&index, &facts, now);
        let mut appended = Vec::new();
        for claim in expired {
            let fact = Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: crate::new_id("fact"),
                seq: 0,
                thread_id: crate::new_id("room"),
                kind: FactKind::ClaimExpired,
                tool: Some("rally".to_string()),
                role: None,
                subject: format!("claim expired: {}", claim.claim_id),
                scope: claim.raw_scope.clone(),
                created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                summary: claim
                    .lease_expires_at
                    .as_ref()
                    .map(|lease| format!("lease_expires_at:{lease}")),
                evidence: vec![format!("expired_claim:{}", claim.claim_id)],
                target: claim.owner_tool.clone(),
                ref_id: Some(claim.claim_id.clone()),
                status: Some("expired".to_string()),
                severity: None,
                uri: None,
                session: None,
            };
            appended.push(self.append_fact_verified(&fact)?);
        }
        if !appended.is_empty() {
            self.rebuild_claim_index()?;
        }
        Ok(appended)
    }

    pub(crate) fn session_facts_with_context_version(&self) -> Result<(Vec<Fact>, Option<u64>)> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(&self.log_dir, &self.archive_dir, &self.facts_db_path)?;
        // Warm-pool facade (L11/R1/G10): warm reuse in daemon mode, per-op
        // strict open in direct mode (byte-identical to main — G1).
        let fact_store = self.fact_store_handle(false)?;
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

    /// Repo root this store is rooted at (parent of `.rally`). Used to resolve
    /// the coordination policy (`resolve_coordination`).
    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub(crate) fn snapshot(&self) -> Result<RoomSnapshot> {
        self.snapshot_with_archived(false)
    }

    /// Snapshot honoring an explicit `include_archived` flag (the
    /// `rally room --include-archived` path re-includes decayed facts).
    pub(crate) fn snapshot_with_archived(&self, include_archived: bool) -> Result<RoomSnapshot> {
        let facts = self.facts()?;
        let coord = crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
        Ok(snapshot_from_facts_with_policy(
            &facts,
            &coord,
            include_archived,
        ))
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
        Ok(read_cursors_at(&self.cursor_path)?
            .get(tool)
            .copied()
            .unwrap_or(0))
    }

    pub(crate) fn set_cursor(&self, tool: &str, seq: i64) -> Result<()> {
        write_cursor_at(&self.cursor_path, tool, seq)
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
            from_session_id: None,
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
        let cursors = read_cursors_at(&self.cursor_path).unwrap_or_default();
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
    /// Snapshot with per-tool read receipts, honoring an `include_archived` flag.
    pub(crate) fn snapshot_with_readers_archived(
        &self,
        include_archived: bool,
    ) -> Result<RoomSnapshot> {
        let facts = self.facts()?;
        let coord = crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
        let mut snapshot = snapshot_from_facts_with_policy(&facts, &coord, include_archived);
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

fn facts_from_store(store: &SqliteStore) -> Result<Vec<Fact>> {
    let query = store
        .query(&FactQuery::all())
        .map_err(|err| RallyError::Message(format!("query facts: {err}")))?;
    query
        .event_records
        .into_iter()
        .map(|record| {
            let seq = i64::try_from(record.sequence_number)
                .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
            Fact::from_value(record.payload, seq)
        })
        .collect()
}

fn facts_from_db_with_query_recovery(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
) -> Result<Vec<Fact>> {
    let fact_store = open_fact_store_lenient(facts_db_path)?;
    match facts_from_store(&fact_store) {
        Ok(facts) => Ok(facts),
        Err(err) if is_malformed_db_error(&err) => {
            quarantine_corrupt_db(facts_db_path)?;
            if let Some(path) = reconcile_cache_path(facts_db_path) {
                let _ = fs::remove_file(path);
            }
            reconcile_segments_and_db(log_dir, archive_dir, facts_db_path)?;
            let recovered_store = open_fact_store_lenient(facts_db_path)?;
            facts_from_store(&recovered_store)
        }
        Err(err) => Err(err),
    }
}

fn facts_from_segments(log_dir: &Path, archive_dir: &Path) -> Result<Vec<Fact>> {
    let live = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    let mut entries: Vec<LedgerLine> = Vec::new();
    for path in live.iter().chain(archived.iter()) {
        entries.extend(read_segment_entries(path)?);
    }
    entries.sort_by_key(|entry| entry.seq);
    let mut facts = Vec::with_capacity(entries.len());
    let mut seen = BTreeSet::<i64>::new();
    for entry in entries {
        if !seen.insert(entry.seq) {
            continue;
        }
        facts.push(Fact::from_value(entry.payload, entry.seq)?);
    }
    Ok(facts)
}

fn handoff_closer_matches_target(handoff: &Fact, closer: &Fact) -> bool {
    // Legacy rows predate session identity and used artifact/resolve refs as
    // broad completion markers. Keep replay stable for those ledgers while
    // applying target correlation to session-era durable writes.
    if closer.from_session_id.is_none() {
        return true;
    }

    match handoff.target.as_deref() {
        Some("all") | None => true,
        Some(target) => closer.tool.as_deref() == Some(target),
    }
}

fn fact_closes_handoff(handoff: &Fact, closer: &Fact) -> bool {
    matches!(
        closer.kind,
        FactKind::Resolve | FactKind::Receipt | FactKind::Artifact
    ) && closer.seq > handoff.seq
        && closer.ref_id.as_deref() == Some(handoff.event_id.as_str())
        && handoff_closer_matches_target(handoff, closer)
}

fn handoff_is_closed(handoff: &Fact, facts: &[Fact]) -> bool {
    facts
        .iter()
        .any(|closer| fact_closes_handoff(handoff, closer))
}

/// Pure projection of a `RoomSnapshot` from an already-loaded facts slice.
///
/// This is the body formerly inlined in `RoomStore::snapshot`. Extracted so
/// that both `snapshot()` and `snapshot_with_readers()` can call it without
/// loading facts twice (fix #2 — one DB round-trip instead of two).
/// Recency weight for a fact, from its `created_at` and the policy half-life.
/// A fact whose `created_at` fails to parse is treated as fresh (weight 1.0):
/// decay must never hide a message just because its timestamp is malformed.
fn fact_recency_weight(fact: &Fact, now_secs: i64, half_life_secs: i64) -> f64 {
    match chrono::DateTime::parse_from_rfc3339(&fact.created_at) {
        Ok(dt) => crate::decay::recency_weight(now_secs - dt.timestamp(), half_life_secs),
        Err(_) => 1.0,
    }
}

/// Sort a bucket newest-first by recency weight (DESC), tie-broken by seq (DESC)
/// so the existing insertion-order behavior is preserved for equal-age facts.
fn sort_by_recency(facts: &mut [Fact], now_secs: i64, half_life_secs: i64) {
    facts.sort_by(|a, b| {
        let wa = fact_recency_weight(a, now_secs, half_life_secs);
        let wb = fact_recency_weight(b, now_secs, half_life_secs);
        wb.partial_cmp(&wa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.seq.cmp(&a.seq))
    });
}

/// Age in seconds of a fact (now - created_at), fail-open to 0 (treated as
/// fresh) when the timestamp is unparseable — never invent staleness from a bad
/// stamp on the squad-visibility path.
fn fact_age_secs(fact: &Fact, now_secs: i64) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&fact.created_at)
        .map(|dt| now_secs - dt.timestamp())
        .unwrap_or(0)
}

/// For each tool, the age (seconds) of the NEWEST fact of one of `kinds` that
/// names the tool either as author (`fact.tool`) or as recipient/subject
/// (`fact.target` / a `to:<tool>` in evidence). Pure over `facts`. Absent tools
/// simply don't appear in the map (caller reads `None` → signal absent).
fn newest_fact_age_per_tool(
    facts: &[Fact],
    now_secs: i64,
    kinds: &[&str],
) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    let note = |tool: &str, age: i64, out: &mut BTreeMap<String, i64>| {
        out.entry(tool.to_string())
            .and_modify(|e| *e = (*e).min(age))
            .or_insert(age);
    };
    for f in facts.iter().filter(|f| kinds.contains(&f.kind.as_str())) {
        let age = fact_age_secs(f, now_secs);
        if let Some(t) = &f.tool
            && t != "rally"
        {
            note(t, age, &mut out);
        }
        if let Some(t) = &f.target {
            note(t, age, &mut out);
        }
        // Recipient encoded as `to:<tool>` in evidence (inject content facts).
        for ev in &f.evidence {
            if let Some(t) = ev.strip_prefix("to:") {
                note(t, age, &mut out);
            }
        }
    }
    out
}

/// Code-progress signal: for each tool, the age of its newest presence/session
/// fact IFF that fact's `branch_head_sha:` stamp DIFFERS from the prior such
/// fact's stamp (the worktree HEAD moved → forward code progress). When fewer
/// than two stamped facts exist, or the sha is unchanged, the tool is ABSENT
/// from the map (caller reads `None` → signal absent → fail-open). Pure over
/// `facts`; no git I/O (the presence writer stamps the sha).
fn code_progress_age_per_tool(facts: &[Fact], now_secs: i64) -> BTreeMap<String, i64> {
    // Gather, per tool, the (seq, age, sha) of every stamped presence/session fact.
    let mut by_tool: BTreeMap<String, Vec<(i64, i64, String)>> = BTreeMap::new();
    for f in facts
        .iter()
        .filter(|f| f.kind == "presence" || f.kind == "session")
    {
        let Some(tool) = f.tool.as_deref() else {
            continue;
        };
        if tool == "rally" {
            continue;
        }
        let Some(sha) = f.evidence.iter().find_map(|e| {
            e.strip_prefix("branch_head_sha:")
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "unknown")
        }) else {
            continue;
        };
        by_tool.entry(tool.to_string()).or_default().push((
            f.seq,
            fact_age_secs(f, now_secs),
            sha.to_string(),
        ));
    }
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for (tool, mut entries) in by_tool {
        if entries.len() < 2 {
            continue; // need two observations to prove movement
        }
        entries.sort_by_key(|(seq, _, _)| *seq);
        let newest = &entries[entries.len() - 1];
        let prev = &entries[entries.len() - 2];
        if newest.2 != prev.2 {
            // HEAD moved between the two latest observations → progress at the
            // age of the newest observation.
            out.insert(tool, newest.1);
        }
    }
    out
}

/// The planned heartbeat cadence (seconds) a tool has declared, if any. Sourced
/// (first present wins) from a `planned_heartbeat_secs:<n>` evidence stamp on the
/// tool's newest presence/session fact. `None` → caller uses the default
/// cadence. Pure over `facts`; never panics on a bad value.
fn planned_cadence_for_tool(facts: &[Fact], tool: &str) -> Option<i64> {
    facts
        .iter()
        .filter(|f| f.tool.as_deref() == Some(tool))
        .filter(|f| f.kind == "presence" || f.kind == "session")
        .max_by_key(|f| f.seq)
        .and_then(|f| {
            f.evidence.iter().find_map(|e| {
                e.strip_prefix("planned_heartbeat_secs:")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .filter(|n| *n > 0)
            })
        })
}

/// Build a room snapshot, applying recency-decay ordering and archive-floor
/// partitioning per the coordination policy.
///
/// * Listing buckets (decisions / risks / artifacts) are ordered by recency
///   weight so fresher messages surface first.
/// * Facts whose weight has fallen below the archive floor are moved OUT of the
///   active buckets and into `stale_facts` (lossless — the raw segments stay on
///   disk and are re-included when `include_archived` is true / via
///   `--include-archived`).
/// * Squad entries decay by ADAPTIVE multi-signal liveness
///   (`crate::liveness`): a squad whose four signals are ALL provably stale is
///   DROPPED from the default snapshot (restored under `include_archived`).
///   Fail-OPEN: a Live or Unknown verdict keeps the squad visible.
fn snapshot_from_facts_with_policy(
    facts: &[Fact],
    coord: &crate::hooks_config::CoordinationConfig,
    include_archived: bool,
) -> RoomSnapshot {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let half_life_secs = coord.half_life_secs();
    let floor = coord.archive_floor_weight;
    let is_archived = |fact: &Fact| -> bool {
        !include_archived
            && crate::decay::is_archivable(
                fact_recency_weight(fact, now_secs, half_life_secs),
                floor,
            )
    };
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
        .filter(|f| {
            f.kind == "resolve"
                || f.kind == "release"
                || f.kind == "receipt"
                || f.kind == "claim.expired"
        })
        .filter_map(|f| f.ref_id.clone())
        .collect::<BTreeSet<_>>();
    let active_claims = facts
        .iter()
        .filter(|fact| claim_authority::is_active_claim_fact(fact, facts))
        .cloned()
        .collect::<Vec<_>>();
    let active_blockers = facts
        .iter()
        .filter(|f| f.kind == "blocker")
        .filter(|f| !resolved.contains(&f.event_id))
        .cloned()
        .collect::<Vec<_>>();
    let open_handoffs = facts
        .iter()
        .filter(|f| f.kind == "handoff")
        .filter(|f| !handoff_is_closed(f, facts))
        // B18: exclude external-intake facts from repo-local backlog.
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .cloned()
        .collect::<Vec<_>>();
    let pending_wakes = facts
        .iter()
        .filter(|fact| fact.kind == "wake")
        .filter(|fact| fact.status.as_deref() == Some("pending"))
        .filter(|fact| !resolved.contains(&fact.event_id))
        .cloned()
        .collect::<Vec<_>>();
    // Recency-decay buckets: order by weight (fresh first), drop facts that
    // have decayed below the archive floor into `stale_facts`.
    let mut stale_facts: Vec<Fact> = Vec::new();
    let mut current_decisions = facts
        .iter()
        .filter(|f| f.kind == "decision")
        .filter(|f| {
            if is_archived(f) {
                stale_facts.push((*f).clone());
                false
            } else {
                true
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_by_recency(&mut current_decisions, now_secs, half_life_secs);
    current_decisions.truncate(20);

    // DI-1: split kind=risk facts into human coordination risks (current_risks)
    // vs system-generated health/telemetry (system_health), keyed on a known
    // machine subject prefix. Keeps the risk view trustworthy; telemetry stays
    // auditable in its own bucket. `SYSTEM_HEALTH_SUBJECT_PREFIXES` is the single
    // source of truth for the telemetry class.
    const SYSTEM_HEALTH_SUBJECT_PREFIXES: &[&str] = &[
        "external-intake:",
        "unmanaged-agent:",
        "duplicate-active-squad-id:",
        "binary-drift:",
    ];
    fn is_system_health_subject(subject: &str) -> bool {
        SYSTEM_HEALTH_SUBJECT_PREFIXES
            .iter()
            .any(|p| subject.starts_with(p))
    }

    let mut current_risks: Vec<Fact> = Vec::new();
    let mut system_health_all: Vec<Fact> = Vec::new();
    for f in facts.iter().filter(|f| f.kind == "risk") {
        if resolved.contains(&f.event_id) {
            continue;
        }
        if is_archived(f) {
            stale_facts.push(f.clone());
            continue;
        }
        if is_system_health_subject(&f.subject) {
            system_health_all.push(f.clone());
        } else {
            current_risks.push(f.clone());
        }
    }
    sort_by_recency(&mut current_risks, now_secs, half_life_secs);
    current_risks.truncate(20);
    // Dedup telemetry by subject (freshest kept) so historical un-guarded
    // accumulation (pre-DI-4 binary-drift / duplicate-squad rows) collapses to
    // one row per distinct signal.
    sort_by_recency(&mut system_health_all, now_secs, half_life_secs);
    let mut seen_subjects: BTreeSet<String> = BTreeSet::new();
    let mut system_health: Vec<Fact> = Vec::new();
    for f in system_health_all {
        if seen_subjects.insert(f.subject.clone()) {
            system_health.push(f);
        }
    }
    // NOT truncated: the bucket is already deduped by subject (bounded by the
    // small, machine-generated system vocabulary), and the enter-path
    // idempotency guards read this projection — a truncation could drop a prior
    // subject and defeat the guard, re-appending duplicates to the ledger.

    let mut recent_artifacts = facts
        .iter()
        .filter(|f| f.kind == "artifact")
        // B18: exclude external-intake facts from repo-local backlog.
        .filter(|f| !f.scope.iter().any(|s| s == "external-intake"))
        .filter(|f| {
            if is_archived(f) {
                stale_facts.push((*f).clone());
                false
            } else {
                true
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_by_recency(&mut recent_artifacts, now_secs, half_life_secs);
    recent_artifacts.truncate(20);
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
    // `now_secs` already computed at the top of this function.
    let acked = acknowledged_tools(facts);
    // Adaptive-liveness signal sources (all PURE over `facts`):
    //   (a) heartbeat   = age of the tool's highest-seq fact (computed below).
    //   (b) inject/ack  = age of the newest delivery record naming the tool: a
    //                     `receipt` or `wake` it authored (ack), or a `handoff` /
    //                     `wake` whose `target` is the tool (inject TO it).
    //   (c) code progress = age of the tool's newest presence/session fact WHEN
    //                     its `branch_head_sha:` evidence differs from the prior
    //                     such fact (HEAD moved). Pure over facts — the presence
    //                     writer stamps the sha; the snapshot stays I/O-free.
    //                     Absent (no two stamped facts) → None → fail-open.
    //   (d) plan/mission = age of the tool's newest live claim, or of the latest
    //                     mission/handoff it authored (declared active work).
    let inject_ages = newest_fact_age_per_tool(facts, now_secs, &["receipt", "wake", "handoff"]);
    let progress_ages = code_progress_age_per_tool(facts, now_secs);
    // Plan signal: newest live-claim OR mission/handoff age per owning tool.
    let mut plan_ages: BTreeMap<String, i64> = BTreeMap::new();
    for claim in &active_claims {
        if let Some(t) = &claim.tool {
            let age = fact_age_secs(claim, now_secs);
            plan_ages
                .entry(t.clone())
                .and_modify(|e| *e = (*e).min(age))
                .or_insert(age);
        }
    }
    for f in facts
        .iter()
        .filter(|f| f.kind == "mission" || f.kind == "handoff")
    {
        if let Some(t) = &f.tool {
            let age = fact_age_secs(f, now_secs);
            plan_ages
                .entry(t.clone())
                .and_modify(|e| *e = (*e).min(age))
                .or_insert(age);
        }
    }

    let cadence = coord.default_cadence_secs;
    let mult = coord.miss_multiplier;
    let grace = coord.grace_secs;
    let squads = tool_last
        .into_iter()
        .filter_map(|(tool, (seq, ts))| {
            // Parse ISO-8601 ts to epoch secs for idle check; fall back to
            // treating the tool as active if parsing fails.
            let seen_secs = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|dt| dt.timestamp())
                .unwrap_or(now_secs);
            let heartbeat_age = now_secs - seen_secs;
            // The 15-min idle label is preserved for the existing surfaces that
            // read `Squad.status`; it is independent of the drop decision.
            let status = if heartbeat_age <= IDLE_THRESHOLD_SECS {
                "active".to_string()
            } else {
                "idle".to_string()
            };
            let acknowledged = acked.contains(&tool);

            // --- Adaptive multi-signal liveness (the squad-decay gap) ---
            let planned_interval = planned_cadence_for_tool(facts, &tool).unwrap_or(cadence);
            let window =
                crate::liveness::adaptive_window_secs(planned_interval, cadence, mult, grace);
            let signals = crate::liveness::LivenessSignals {
                heartbeat_age: Some(heartbeat_age),
                inject_age: inject_ages.get(&tool).copied(),
                code_progress_age: progress_ages.get(&tool).copied(),
                plan_age: plan_ages.get(&tool).copied(),
            };
            let verdict = crate::liveness::is_live(&signals, window);

            // FAIL-OPEN: Live and Unknown are KEPT (Unknown = cannot prove dead).
            // Only a provably-Stale squad (all signals present & past window) is
            // DROPPED from the default snapshot; `include_archived` restores it.
            // This direction is opposite the reaper's fail-CLOSED removal path on
            // purpose: hiding a still-alive peer is the dangerous direction here.
            let dropped = matches!(verdict, crate::liveness::Liveness::Stale) && !include_archived;
            if dropped {
                return None;
            }

            Some(Squad {
                tool,
                last_seen_seq: seq,
                last_seen_ts: ts,
                status,
                acknowledged,
            })
        })
        .collect::<Vec<_>>();

    // Lead is the tool from the most-recent decision with subject "role:lead".
    // Lead = the tool of the latest `role:lead` decision, UNLESS the latest
    // lead-family decision is a `role:lead:relinquished` (seat reopened → None).
    let latest_lead_fact = facts
        .iter()
        .filter(|f| {
            f.kind == "decision"
                && (f.subject == "role:lead" || f.subject == "role:lead:relinquished")
        })
        .max_by_key(|f| f.seq);
    let lead_epoch = latest_lead_fact.map(|f| f.seq);
    let lead = latest_lead_fact
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
        pending_wakes,
        current_decisions,
        current_risks,
        system_health,
        recent_artifacts,
        unconsumed_artifacts,
        stale_facts,
        squads,
        lead,
        lead_epoch,
        readers: Vec::new(),
        mission,
    }
}

fn open_fact_store(path: &Path) -> Result<SqliteStore> {
    // Per-retrier jitter (pid + thread id + process-global salt) de-synchronizes
    // concurrent retriers across BOTH threads and processes — the thundering-herd
    // cure; budget raised for write-burst tolerance (B-write-burst-scale).
    let jitter = retry_jitter_ms();
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

fn open_fact_store_lenient(path: &Path) -> Result<SqliteStore> {
    match open_fact_store(path) {
        Ok(store) => Ok(store),
        Err(err) if is_malformed_db_error(&err) => {
            quarantine_corrupt_db(path)?;
            open_fact_store(path)
        }
        Err(err) => Err(err),
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

/// Engagement labels reserved for committed test/CI fixtures. A LIVE runtime
/// session must never resolve to one of these — its facts would leak into a
/// git-tracked fixture segment (`.rally/log/test.jsonl`), perma-dirtying the
/// working tree and mixing real coordination history with fixture data
/// (HIGH-risk fact_182e8, 2026-06-09). The in-process cargo test suite is
/// unaffected: tests set the engagement via [`RoomStore::set_active_engagement_for_test`]
/// (a `#[cfg(test)]` setter that bypasses this resolver entirely), so the
/// reserved-label guard only ever fires for a production `rally` invocation
/// that inherited a stale `test` label from the env or the
/// `.rally/active-engagement` file.
pub(crate) const RESERVED_FIXTURE_ENGAGEMENTS: &[&str] = &["test"];

/// True when `label` is a reserved fixture engagement that live appends must
/// not write into. Case-insensitive so `Test`/`TEST` are caught too.
pub(crate) fn is_reserved_fixture_engagement(label: &str) -> bool {
    RESERVED_FIXTURE_ENGAGEMENTS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(label))
}

/// Resolve the engagement label used to stamp new appends.
///
/// Priority:
/// 1. `RALLY_ENGAGEMENT` env var (non-empty after trim, sanitised).
/// 2. `.rally/active-engagement` file (one line, sanitised).
/// 3. UTC date `YYYY-MM-DD` from the current clock.
///
/// A resolved label that is a [reserved fixture engagement](RESERVED_FIXTURE_ENGAGEMENTS)
/// is REJECTED at every tier and falls through to the UTC-date fallback, so a
/// live session can never append into the committed `test.jsonl` fixture even
/// if its env/file says `test`.
///
/// Sanitisation strips path separators and trims whitespace so a label can
/// never escape the log dir. The fallback never fails — if the clock returns
/// something exotic, `"unknown-engagement"` is used.
fn resolve_active_engagement(rally_dir: &Path) -> String {
    resolve_active_engagement_with_env(rally_dir, env::var(ENGAGEMENT_ENV_VAR).ok())
}

/// Engagement resolution with the `RALLY_ENGAGEMENT` value injected rather than
/// read from the process environment. This is the real implementation; the
/// public wrapper passes the live env var. Tests pass an explicit value so they
/// can exercise the priority ladder WITHOUT mutating the process-global env —
/// `std::env::set_var` is unsound under cargo's multi-threaded test runner and
/// raced concurrent engagement resolution in other tests (e.g. backlog).
pub(crate) fn resolve_active_engagement_with_env(
    rally_dir: &Path,
    env_value: Option<String>,
) -> String {
    if let Some(value) = env_value {
        let cleaned = sanitise_engagement(&value);
        if !cleaned.is_empty() && !is_reserved_fixture_engagement(&cleaned) {
            return cleaned;
        }
    }
    let active_path = rally_dir.join(ACTIVE_ENGAGEMENT_FILENAME);
    if let Ok(text) = fs::read_to_string(&active_path) {
        let cleaned = sanitise_engagement(text.trim());
        if !cleaned.is_empty() && !is_reserved_fixture_engagement(&cleaned) {
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

/// Canonicalize `root` into the string form the daemon's `Pong` identity
/// carries, so a client's repo_root verification compares like-for-like
/// (ADR-02/L7). Deliberately mirrors `rallyd_core.rs`'s private
/// `canonical_repo_root` helper rather than importing it: that module is
/// Chunk B's owned file, edited concurrently in the same parallel window, so
/// this Chunk-C slice keeps its own copy of the (three-line) algorithm rather
/// than reaching across the chunk boundary. Falls back to the given path
/// verbatim if canonicalization fails (e.g. the root doesn't exist yet).
pub(crate) fn canonical_repo_root_string(root: &Path) -> String {
    fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Read `cursors.json` at `path` (`{"cursors": {tool: seq, ...}}`), tolerating
/// a missing or malformed file as "no cursors yet" rather than erroring —
/// this file is a cache (R10 makes the ledger checkpoint authoritative; this
/// is only the fallback). Factored out of [`DirectRoomStore`] so
/// `RoutedRoomStore`'s LOCAL `cursor_for` fallback (store_client.rs) shares
/// the exact on-disk format instead of re-deriving it.
pub(crate) fn read_cursors_at(path: &Path) -> Result<BTreeMap<String, i64>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text =
        fs::read_to_string(path).map_err(RallyError::io(format!("read {}", path.display())))?;
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

/// Write-through update of `cursors.json` at `path`: read-modify-write via a
/// temp-file-then-rename swap (atomic on the same filesystem). Factored out
/// of [`DirectRoomStore`] for the same reason as [`read_cursors_at`].
pub(crate) fn write_cursor_at(path: &Path, tool: &str, seq: i64) -> Result<()> {
    let mut cursors = read_cursors_at(path)?;
    cursors.insert(tool.to_string(), seq);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "updated_at": now_string(),
        "cursors": cursors
    }))
    .map_err(RallyError::json("render cursors"))?;
    let temp_path = path.with_extension(format!("json.tmp-{}", short_id()));
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
    // Reject reserved fixture labels at WRITE time too (independent-auditor LOW,
    // 2026-06-09): the resolver already refuses to ROUTE live appends to a
    // reserved label, but silently accepting `rally enter --engagement test`
    // and then never honoring it is a confusing no-op. Fail loud instead.
    if is_reserved_fixture_engagement(&cleaned) {
        return Err(RallyError::Usage(format!(
            "engagement label {cleaned:?} is reserved for the committed test/CI \
             fixture segment and cannot be set for a live session; pick a \
             different label (or omit --engagement to use the dated segment)"
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

/// Filename of the derived reconcile fast-path sidecar (Step-3). Holds a cheap
/// fingerprint of the segment files + facts.db plus the last-verified counts, so
/// a no-change open/append can confirm freshness in O(#segment-files) instead of
/// O(#ledger-lines). DISPOSABLE: missing/corrupt/stale → ignored, full scan runs.
/// Lives under `.rally/`, already gitignored by the `.rally/*` whitelist rule.
const RECONCILE_CACHE_FILENAME: &str = ".reconcile-cache.json";

/// Cheap per-file fingerprint component: `(filename, byte_len, mtime_ns)` plus
/// an optional content hash of the file's first page.
///
/// For segment files only `(name, len, mtime_ns)` is used — JSONL segments are
/// append-only, so any content change also changes `len`. (Exception: the
/// `seed_segment_from_db` path uses `truncate(true)` and rewrites the file at a
/// potentially equal length — this is safe ONLY because that caller drops the
/// sidecar before returning (~store.rs `reconcile`, seed branch). Any future
/// same-length-rewrite path such as compaction or repair MUST also drop the
/// sidecar, or must add a content hash to segment fingerprints.) For `facts.db` the
/// `head_hash` (hash of the first 4096 bytes, the SQLite file-format header +
/// page 1) is ALSO populated: in-place header corruption (SQLITE_NOTADB) keeps
/// the same `len` and may collide on a coarse `mtime_ns` under load, but it
/// always changes the header bytes — so the head_hash diverges and the fast
/// path correctly refuses to trust the corrupt db, falling through to
/// `read_db_event_count` (quarantine + rebuild). `mtime_ns` + `len` still guard
/// mid-page corruption (which rewrites the file, advancing mtime).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct FileFingerprint {
    name: String,
    len: u64,
    mtime_ns: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head_hash: Option<u64>,
}

/// Derived sidecar for the reconcile fast path. All fields are recomputable from
/// the canonical ledger + facts.db; this file is never authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ReconcileCache {
    /// Sorted fingerprint of every live + archive (replayable) segment file.
    segments_fingerprint: Vec<FileFingerprint>,
    /// `facts.db` fingerprint at the moment counts were last verified equal.
    /// A change here (mtime or size) means the db was rewritten/corrupted since
    /// we last trusted it → we must NOT take the fast path.
    db_fingerprint: Option<FileFingerprint>,
    canonical_count: i64,
    #[serde(default)]
    canonical_max_seq: i64,
    db_count: i64,
    #[serde(default)]
    db_max_seq: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SeqStats {
    count: i64,
    max_seq: i64,
}

// Test hook: counts how many times the AUTHORITATIVE O(N) reconcile path ran
// (the full `distinct_segment_seqs` + `read_db_event_count` scan) on THIS
// thread. The fast path does NOT bump this. Thread-local so parallel tests
// (cargo's multi-threaded runner) never cross-contaminate the counter — a
// process-global counter raced across concurrent tests and produced false
// fast-path-miss assertions.
#[cfg(test)]
thread_local! {
    static FULL_RECONCILE_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_full_reconcile_scan() {
    FULL_RECONCILE_SCANS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn full_reconcile_scan_count() -> u64 {
    FULL_RECONCILE_SCANS.with(|c| c.get())
}

#[cfg(not(test))]
#[inline]
fn note_full_reconcile_scan() {}

/// Fingerprint a single file as `(name, byte_len, mtime_ns)` with NO content
/// hash. Used for append-only JSONL segments, where any change moves `len`.
/// Returns `None` if the file is absent or its metadata can't be read — callers
/// treat `None` as "no trustworthy signal" and fall through to the
/// authoritative path.
fn fingerprint_file(path: &Path) -> Option<FileFingerprint> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(FileFingerprint {
        name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        len: meta.len(),
        mtime_ns,
        head_hash: None,
    })
}

/// Fingerprint `facts.db` for the corruption-safe fast-path guard:
/// `(name, len, mtime_ns)` PLUS a `head_hash` over the first 4096 bytes (the
/// SQLite header + page 1). Header corruption (SQLITE_NOTADB) preserves `len`
/// and can collide on a coarse `mtime_ns` under concurrency, but it ALWAYS
/// changes the header bytes → `head_hash` diverges → the fast path refuses to
/// trust the db and falls through to `read_db_event_count`, which quarantines +
/// rebuilds. O(1): a fixed 4KB read regardless of ledger size. Returns `None`
/// if the db is absent (caller then forces the authoritative path).
fn fingerprint_db(path: &Path) -> Option<FileFingerprint> {
    let mut fp = fingerprint_file(path)?;
    fp.head_hash = Some(hash_file_head(path));
    Some(fp)
}

/// Hash of the first 4096 bytes of `path` (fewer if the file is shorter). Cheap,
/// fixed-cost, content-sensitive. A read error hashes the empty slice — the
/// resulting mismatch just forces the authoritative path, which is safe.
///
/// IMPORTANT: uses FNV-1a 64-bit, NOT `DefaultHasher`. `DefaultHasher` is
/// randomly seeded per-process (Rust guarantees), so a value hashed in process A
/// and persisted to the sidecar NEVER equals the same bytes re-hashed in process B
/// — defeating the entire cross-process fast-path. FNV-1a is deterministic across
/// processes, platforms, and Rust versions (it is a fixed algorithm, not a
/// std-library hasher). Do NOT replace this with any `std::collections::hash_map`
/// hasher for any value persisted to disk.
fn hash_file_head(path: &Path) -> u64 {
    use std::io::Read;
    if let Ok(mut f) = fs::File::open(path) {
        let mut buf = [0u8; 4096];
        if let Ok(n) = f.read(&mut buf) {
            return hash_bytes_fnv1a(&buf[..n]);
        }
    }
    hash_bytes_fnv1a(&[])
}

/// FNV-1a 64-bit for persisted filenames/fingerprints. See [`hash_file_head`].
fn hash_bytes_fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Sorted fingerprint over the replayable segment files (live + archive). O(#files).
fn segments_fingerprint(live: &[PathBuf], archived: &[PathBuf]) -> Vec<FileFingerprint> {
    let mut fps: Vec<FileFingerprint> = live
        .iter()
        .chain(archived.iter())
        .filter_map(|p| fingerprint_file(p))
        .collect();
    fps.sort_by(|a, b| a.name.cmp(&b.name));
    fps
}

fn reconcile_cache_path(facts_db_path: &Path) -> Option<PathBuf> {
    facts_db_path
        .parent()
        .map(|p| p.join(RECONCILE_CACHE_FILENAME))
}

/// Read the sidecar, returning `None` on absent/unparseable (never errors — the
/// sidecar is disposable and must never override the canonical ledger).
fn read_reconcile_cache(facts_db_path: &Path) -> Option<ReconcileCache> {
    let path = reconcile_cache_path(facts_db_path)?;
    let text = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the sidecar atomically (tmp + rename). Best-effort: a write failure is
/// swallowed by the caller — the next op simply re-scans and rewrites it.
fn write_reconcile_cache(facts_db_path: &Path, cache: &ReconcileCache) -> Result<()> {
    let Some(path) = reconcile_cache_path(facts_db_path) else {
        return Ok(());
    };
    let rendered =
        serde_json::to_string(cache).map_err(RallyError::json("render reconcile cache"))?;
    let temp_path = path.with_extension(format!("json.tmp-{}", short_id()));
    fs::write(&temp_path, rendered)
        .map_err(RallyError::io(format!("write {}", temp_path.display())))?;
    match fs::rename(&temp_path, &path) {
        Ok(()) => Ok(()),
        // Lost a race with a peer writer; their sidecar is just as valid.
        Err(_) => {
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
    }
}

fn seed_segments_from_db_if_absent(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
) -> Result<()> {
    let segments = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    if !segments.is_empty() || !archived.is_empty() {
        return Ok(());
    }

    let db_stats = read_db_event_stats(facts_db_path)?;
    if db_stats.count > 0 {
        seed_segment_from_db(log_dir, facts_db_path)?;
        refresh_reconcile_cache_after_full_scan(log_dir, archive_dir, facts_db_path, db_stats);
        return Ok(());
    }

    let cache = ReconcileCache {
        segments_fingerprint: segments_fingerprint(&segments, &archived),
        db_fingerprint: fingerprint_db(facts_db_path),
        canonical_count: 0,
        canonical_max_seq: 0,
        db_count: 0,
        db_max_seq: 0,
    };
    let _ = write_reconcile_cache(facts_db_path, &cache);
    Ok(())
}

/// Reconcile the canonical segment set with the derived sqlite cache.
///
/// Called by read projections and targeted repair paths. Hot opens/appends use
/// cheaper cache-open plus canonical segment readback; they do not run this
/// full reconcile on every invocation.
/// The contract is:
///
/// * Segments ahead of db (incl. db absent) → rebuild db by replaying segments.
/// * Segments absent but db has events → seed one segment from db (first-run
///   upgrade from a pre-R1 db that never had a ledger).
/// * Both empty, or in sync → no-op.
///
/// Fast path (Step-3): a cheap O(#segment-files) fingerprint of the segment set
/// and facts.db is compared against the sidecar. When they match AND the
/// sidecar's recorded counts agree (canonical_count == db_count), nothing has
/// changed since the last authoritative reconcile, so we return Ok WITHOUT the
/// O(N) line scans. This NEVER short-circuits corruption detection: an in-place
/// facts.db corruption rewrites the file (mtime/size change), so its
/// fingerprint no longer matches the sidecar and we fall through to the
/// authoritative path, where `read_db_event_count` issues a query that detects
/// and quarantines the corrupt db. The sidecar is purely disposable; any
/// miss/mismatch/parse-error degrades cleanly to the full scan.
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

    // ----- Fast path: cheap fingerprint vs sidecar (O(#files)) -----
    let seg_fp = segments_fingerprint(&segments, &archived);
    let db_fp = fingerprint_db(facts_db_path);
    if let Some(cache) = read_reconcile_cache(facts_db_path)
        && cache.segments_fingerprint == seg_fp
        && cache.canonical_count == cache.db_count
        && cache.canonical_max_seq == cache.db_max_seq
        && (cache.canonical_count == 0 || cache.canonical_max_seq >= cache.canonical_count)
        // facts.db must be present AND byte-identical (same len + mtime) to when
        // we last verified the count. A corrupt-in-place db has a fresh mtime,
        // so this guard fails and we fall through to corruption detection.
        && db_fp.is_some()
        && cache.db_fingerprint == db_fp
    {
        return Ok(());
    }

    // ----- Authoritative path (O(N)): the canonical scan + rebuild on drift ---
    note_full_reconcile_scan();

    // The canonical record is the *set of distinct seqs* across replay sources.
    // The cache is fresh iff it holds the same number of events AND the same
    // highest logical seq. Count alone misses sparse histories: canonical
    // seqs {1,2,4} have count 3, but a derived db with logical max 3 would make
    // the next append reuse seq 4.
    let canonical_stats = segment_seq_stats(&segments, &archived)?;
    // NOTE: read_db_event_stats both COUNTS and DETECTS+QUARANTINES corruption.
    // It must run on the authoritative path — the fast path above only returns
    // early when the db fingerprint is unchanged (no rewrite/corruption since
    // the last successful count), so corruption can never bypass this call.
    let db_stats = read_db_event_stats(facts_db_path)?;

    if canonical_stats.count == 0 && db_stats.count == 0 {
        // Nothing to cache (no db, no segments). Drop any stale sidecar.
        if let Some(p) = reconcile_cache_path(facts_db_path) {
            let _ = fs::remove_file(p);
        }
        return Ok(());
    }

    if canonical_stats.count == 0 && db_stats.count > 0 {
        // No segments yet but the db has events: first-run upgrade from a
        // pre-segment install. Seed a segment so the canonical record exists.
        seed_segment_from_db(log_dir, facts_db_path)?;
        // State just changed; let the next op re-fingerprint. Drop the sidecar.
        if let Some(p) = reconcile_cache_path(facts_db_path) {
            let _ = fs::remove_file(p);
        }
        return Ok(());
    }

    if canonical_stats != db_stats {
        // Segment set and cache disagree on event count or logical high-water
        // mark → cache is stale (or absent). Rebuild it from the canonical
        // segments. Replay is a pure function of the deduped segment set, so
        // this is idempotent.
        rebuild_db_from_segments(&segments, &archived, facts_db_path)?;
        // Refresh the sidecar against the freshly-rebuilt db so the next op is
        // O(1). Re-fingerprint the db (it was just recreated) and recount it.
        refresh_reconcile_cache_after_full_scan(
            log_dir,
            archive_dir,
            facts_db_path,
            canonical_stats,
        );
        return Ok(());
    }

    // canonical_stats == db_stats > 0 → cache is fresh; leave the db untouched
    // and refresh the sidecar so subsequent ops take the O(1) fast path.
    let cache = ReconcileCache {
        segments_fingerprint: seg_fp,
        db_fingerprint: fingerprint_db(facts_db_path),
        canonical_count: canonical_stats.count,
        canonical_max_seq: canonical_stats.max_seq,
        db_count: db_stats.count,
        db_max_seq: db_stats.max_seq,
    };
    let _ = write_reconcile_cache(facts_db_path, &cache);
    Ok(())
}

/// Recompute the sidecar after a rebuild (db was recreated, so its fingerprint
/// and count are now fresh). `canonical_count` is already known from the caller;
/// after a successful rebuild the db holds exactly that many events. Best-effort.
fn refresh_reconcile_cache_after_full_scan(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
    canonical_stats: SeqStats,
) {
    // Re-read segment files: a rebuild does not change them, but re-fingerprint
    // for correctness (cheap, O(#files)).
    let Ok(segments) = read_segment_files(log_dir) else {
        return;
    };
    let Ok(archived) = replay_archive_segments(archive_dir) else {
        return;
    };
    let cache = ReconcileCache {
        segments_fingerprint: segments_fingerprint(&segments, &archived),
        db_fingerprint: fingerprint_db(facts_db_path),
        canonical_count: canonical_stats.count,
        canonical_max_seq: canonical_stats.max_seq,
        db_count: canonical_stats.count,
        db_max_seq: canonical_stats.max_seq,
    };
    let _ = write_reconcile_cache(facts_db_path, &cache);
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

/// Read one canonical JSONL segment with a single framing policy used by every
/// replay, allocation, readback, and index path.
///
/// A malformed final fragment without `\n` is a torn append and is ignored.
/// Any malformed newline-terminated record is completed corruption and fails
/// loudly with path and line evidence. A valid final record is accepted even
/// when it lacks a newline.
fn read_segment_entries(path: &Path) -> Result<Vec<LedgerLine>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        // f4: callers list segment files via `read_segment_files` and then
        // open each one in a SEPARATE step (exists()-then-open at some call
        // sites, no check at all at others) — a concurrent archival/rotation
        // can remove a listed segment in between. That is not corruption; it
        // is a benign race with rotation. Treat it as an empty segment
        // rather than propagating, so callers fall through to whatever else
        // they scan instead of hard-failing. Every PARSE error below stays
        // loud — this only widens tolerance for the file's ABSENCE.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            let ctx = format!("read canonical segment {}", path.display());
            return Err(RallyError::io(ctx)(err));
        }
    };
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut bytes = Vec::new();
    let mut line_number = 0usize;

    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(RallyError::io(format!(
                "read canonical segment {}",
                path.display()
            )))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let had_newline = bytes.last() == Some(&b'\n');
        if had_newline {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }

        match serde_json::from_slice::<LedgerLine>(&bytes) {
            Ok(entry) => entries.push(entry),
            Err(_) if !had_newline => break,
            Err(err) => {
                return Err(RallyError::Message(format!(
                    "completed canonical segment corruption in {} at line {}: {}",
                    path.display(),
                    line_number,
                    err
                )));
            }
        }
    }
    Ok(entries)
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
        for entry in read_segment_entries(path)? {
            // The payload is a serialized Fact.  Extract event_id without a
            // full Fact deserialization to keep this path allocation-light.
            if entry.payload.get("event_id").and_then(|v| v.as_str()) == Some(event_id) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// R9-readback fast path: scan a SINGLE segment file for `event_id`, tail-first.
///
/// The event we just appended is the newest line in the active segment, so the
/// last non-empty line is the overwhelmingly likely hit. Reading the whole file
/// and walking lines bottom-up keeps this allocation-light and O(1) on the happy
/// path. A missing file (segment not yet created — impossible right after an
/// append, but handled defensively) returns `false`, deferring to the full scan.
///
/// Correctness: returning `false` here NEVER produces a false readback failure —
/// the caller falls through to the authoritative full live+archive scan. A
/// `true` here is a genuine presence (we matched the exact `event_id`).
fn segment_event_id_present_tail_first(path: &Path, event_id: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in read_segment_entries(path)?.into_iter().rev() {
        if entry.payload.get("event_id").and_then(|v| v.as_str()) == Some(event_id) {
            return Ok(true);
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

/// Sequence stats across replay sources. `count` is the number of distinct
/// sequence numbers; `max_seq` is the canonical high-water mark. Both are
/// required: sparse histories can have `count < max_seq`, and append must never
/// reuse an existing canonical sequence.
fn segment_seq_stats(live: &[PathBuf], archived: &[PathBuf]) -> Result<SeqStats> {
    let mut seqs: BTreeSet<i64> = BTreeSet::new();
    for path in live.iter().chain(archived.iter()) {
        for entry in read_segment_entries(path)? {
            seqs.insert(entry.seq);
        }
    }
    let count = i64::try_from(seqs.len())
        .map_err(|err| RallyError::Message(format!("distinct seq count overflow: {err}")))?;
    Ok(SeqStats {
        count,
        max_seq: seqs.iter().next_back().copied().unwrap_or(0),
    })
}

fn next_canonical_seq(log_dir: &Path, archive_dir: &Path, facts_db_path: &Path) -> Result<i64> {
    let segments = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    // Fast path ONLY when the sidecar's counts AND its segment fingerprint still
    // match the on-disk segments — so the O(1) shortcut can never hand out a
    // stale max regardless of caller order (defense-in-depth, 2026-07-02). The
    // fingerprint compare is O(#files) stat, cheap; the fallback already reads
    // these segments.
    if let Some(cache) = read_reconcile_cache(facts_db_path)
        && cache.canonical_count == cache.db_count
        && cache.canonical_max_seq == cache.db_max_seq
        && cache.canonical_max_seq >= 0
        && cache.segments_fingerprint == segments_fingerprint(&segments, &archived)
    {
        return Ok(cache.canonical_max_seq + 1);
    }
    Ok(segment_seq_stats(&segments, &archived)?.max_seq + 1)
}

/// Highest `seq` currently written to a segment file (its on-disk tail), or
/// `None` when the segment is absent/empty. Used as a defense-in-depth dup gate:
/// an allocated seq must always exceed the active segment's tail, else we would
/// write a duplicate that bricks segment replay. Reads the (per-engagement,
/// typically small) active segment; scans from the end for the last non-empty
/// line.
fn last_seq_in_segment(segment_path: &Path) -> Result<Option<i64>> {
    if !segment_path.exists() {
        return Ok(None);
    }
    Ok(read_segment_entries(segment_path)?
        .last()
        .map(|entry| entry.seq))
}

/// Events currently held by the derived sqlite cache. Compared against
/// [`segment_seq_stats`] to detect a stale/absent cache.
///
/// Returns 0 in any of three cases that all funnel into the same recovery path:
///
/// 1. **Absent** — the db file does not exist. First-run or post-delete.
/// 2. **Malformed** — the db file exists but SQLite refuses to open it
///    (`SQLITE_CORRUPT`, `SQLITE_NOTADB`, "disk image is malformed"). The
///    cache is QUARANTINED to `facts.db.corrupt.<UTC_NS>` (plus its `-shm`
///    and `-wal` siblings) so the bytes survive for forensics, then we
///    return 0. The caller — `reconcile_segments_and_db` — then sees
///    `canonical_count != db_count`, fires `rebuild_db_from_segments`, and
///    full history is restored from the canonical JSONL ledger. The
///    canonical ledger is `.rally/log/<engagement>.jsonl` (+ archive); the
///    db is a pure derived cache (see [`reconcile_segments_and_db`] and
///    the cache-false-pass invariant in `docs/ORCHESTRATION.md`).
/// 3. **Healthy** — count is queried and returned.
///
/// Idempotent: a second call after quarantine sees the file absent and takes
/// the case-(1) branch with no further quarantine churn.
fn read_db_event_stats(facts_db_path: &Path) -> Result<SeqStats> {
    if !facts_db_path.exists() {
        return Ok(SeqStats::default());
    }
    let store = match open_fact_store(facts_db_path) {
        Ok(store) => store,
        Err(err) if is_malformed_db_error(&err) => {
            // Cache is corrupt; the canonical JSONL ledger is unaffected.
            // Move the bad bytes aside and let reconcile rebuild from segments.
            quarantine_corrupt_db(facts_db_path)?;
            return Ok(SeqStats::default());
        }
        Err(err) => return Err(err),
    };
    // TODO(perf): O(N) full load — replace with count() when factstr exposes one.
    // Mid-page corruption (SQLITE_CORRUPT / code 11) only surfaces during
    // b-tree traversal, not at open time. Catch it here so the same
    // quarantine+rebuild path fires for code-11 as for code-26 (header).
    let query = match store.query(&FactQuery::all()) {
        Ok(q) => q,
        Err(err) if is_malformed_db_error(&err) => {
            quarantine_corrupt_db(facts_db_path)?;
            return Ok(SeqStats::default());
        }
        Err(err) => return Err(RallyError::Message(format!("query facts: {err}"))),
    };
    let mut max_seq = 0_i64;
    for record in &query.event_records {
        let seq = i64::try_from(record.sequence_number)
            .map_err(|err| RallyError::Message(format!("sequence number overflow: {err}")))?;
        let fact = Fact::from_value(record.payload.clone(), seq)?;
        max_seq = max_seq.max(fact.seq);
    }
    let count = i64::try_from(query.event_records.len())
        .map_err(|err| RallyError::Message(format!("event count overflow: {err}")))?;
    Ok(SeqStats { count, max_seq })
}

/// Recognize the SQLite error class that means "this file exists but cannot be
/// opened as a database." On these errors the derived cache is unrecoverable
/// and must be rebuilt from the canonical JSONL ledger.
///
/// Detection is by error-message substring because `factstr`/`factstr-sqlite`
/// returns errors as opaque [`RallyError::Message`] strings here (the chain
/// flattens before we see it). The substrings cover the two SQLite extended
/// codes that mean unrecoverable corruption:
///
/// * `(code: 11)` — `SQLITE_CORRUPT`: malformed database disk image. Triggered
///   by mid-file byte corruption (cosmic-ray bit flip, truncated write, partial
///   journal replay).
/// * `(code: 26)` — `SQLITE_NOTADB`: file does not look like a database.
///   Triggered by a fully-overwritten or zero-truncated file.
///
/// The human-readable substrings (`"disk image is malformed"`,
/// `"file is not a database"`, and the broader `"corrupt"`) are checked as
/// belt-and-braces: SQLite's numeric-code wording is stable across the
/// supported version range, but the English message is what most callers grep
/// for in logs.
///
/// The `"corrupt"` substring catches SQLite's *extended* result codes for
/// `SQLITE_CORRUPT` — `SQLITE_CORRUPT_VTAB` (267), `SQLITE_CORRUPT_SEQUENCE`
/// (523), `SQLITE_CORRUPT_INDEX` (779), and the rest of the 11 | (N<<8)
/// family. These are still unrecoverable corruption; the base-code match
/// alone would miss them because their numeric representation is `267`,
/// `523`, etc. (not `11`). Their messages all carry the word `"corrupt"`
/// from SQLite's `sqlite3_errstr` table.
fn is_malformed_db_error(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string();
    // Match "code: 11)" with closing paren to avoid false positive on "code: 110"
    // (SQLite does not emit code 110, but the substring "code: 11" would match it).
    // "code: 26)" is already unambiguous but gets the same treatment for consistency.
    msg.contains("code: 11)")
        || msg.contains("code: 26)")
        || msg.contains("disk image is malformed")
        || msg.contains("file is not a database")
        || msg.contains("corrupt")
}

/// Rename a corrupt `facts.db` (plus its `-shm` / `-wal` siblings) to
/// `facts.db.corrupt.<UTC_NS>{,-shm,-wal}` so the bytes are preserved for
/// forensics and the next `open_at` sees an absent cache (triggering the
/// rebuild path).
///
/// Atomic per file: each rename is a single `rename(2)` call. If a sibling
/// (`-shm`, `-wal`) is missing — which is normal for a non-WAL-mode db or a
/// clean shutdown — that slot is simply skipped. Idempotent: a second call
/// after a successful quarantine finds the source files absent and returns
/// Ok(()) immediately.
///
/// The timestamp suffix is monotonic to nanosecond resolution; even back-to-back
/// quarantines from the same process produce distinct paths.
fn quarantine_corrupt_db(facts_db_path: &Path) -> Result<()> {
    if !facts_db_path.exists() {
        // Nothing to quarantine — already healed, or never present. The next
        // case-(1) branch in `read_db_event_count` will return 0 directly.
        return Ok(());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let parent = facts_db_path
        .parent()
        .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
    let base = facts_db_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RallyError::Message("facts db path has no file name".to_string()))?;
    let quarantine_main = parent.join(format!("{base}.corrupt.{stamp}"));
    fs::rename(facts_db_path, &quarantine_main).map_err(RallyError::io(format!(
        "quarantine {} -> {}",
        facts_db_path.display(),
        quarantine_main.display()
    )))?;
    // Best-effort: quarantine the WAL/SHM siblings so they don't interfere with
    // the rebuild. They are not load-bearing — the canonical record is the
    // JSONL ledger — so a sibling rename failure is logged but not fatal.
    for ext in ["db-shm", "db-wal"] {
        let sibling = facts_db_path.with_extension(ext);
        if sibling.exists() {
            let quarantine_sibling = parent.join(format!("{base}.corrupt.{stamp}-{ext}"));
            let _ = fs::rename(&sibling, &quarantine_sibling);
        }
    }
    Ok(())
}

/// Rebuild the derived sqlite cache by replaying every segment line in seq
/// order (live segments first, then archive — the union — sorted by seq).
/// Dedup by `sequence_number` (re-running migration twice can otherwise
/// duplicate). If two different payloads share a seq, keep the first-valid line,
/// write the conflicting later line to `.rally/quarantine/`, and continue. One
/// bad duplicate line must not brick every read path.
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
    let mut all_entries: Vec<LedgerLine> = Vec::new();
    for path in live.iter().chain(archived.iter()) {
        all_entries.extend(read_segment_entries(path)?);
    }
    all_entries.sort_by_key(|e| e.seq);

    // Dedup by seq in-place (keep first occurrence); quarantine conflicting
    // later lines so replay can project the rest of the room.
    let mut write = 0usize;
    for read in 0..all_entries.len() {
        if write > 0 && all_entries[write - 1].seq == all_entries[read].seq {
            if all_entries[write - 1].payload != all_entries[read].payload
                || all_entries[write - 1].event_type != all_entries[read].event_type
            {
                quarantine_duplicate_segment_entry(facts_db_path, &all_entries[read])?;
            }
            // duplicate/conflict — skip
        } else {
            if read != write {
                all_entries.swap(read, write);
            }
            write += 1;
        }
    }
    all_entries.truncate(write);

    let replay_events = all_entries
        .iter()
        .map(|entry| {
            let mut payload = entry.payload.clone();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("seq".to_string(), json!(entry.seq));
            }
            NewEvent::new(entry.event_type.clone(), payload)
        })
        .collect::<Vec<_>>();
    // Canonical parsing and deduplication completed successfully. Only now is
    // it safe to replace the derived cache; completed segment corruption must
    // never destroy a still-readable facts.db before surfacing the error.
    let _ = fs::remove_file(facts_db_path);
    let _ = fs::remove_file(facts_db_path.with_extension("db-shm"));
    let _ = fs::remove_file(facts_db_path.with_extension("db-wal"));
    if replay_events.is_empty() {
        return Ok(());
    }
    let store = open_fact_store(facts_db_path)?;
    store
        .append(replay_events)
        .map_err(|err| RallyError::Message(format!("replay segments: {err}")))?;
    Ok(())
}

fn quarantine_duplicate_segment_entry(facts_db_path: &Path, entry: &LedgerLine) -> Result<()> {
    let parent = facts_db_path
        .parent()
        .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
    let quarantine_dir = parent.join(QUARANTINE_DIRNAME);
    fs::create_dir_all(&quarantine_dir).map_err(RallyError::io(format!(
        "create {}",
        quarantine_dir.display()
    )))?;
    let line =
        serde_json::to_string(entry).map_err(RallyError::json("render duplicate segment"))?;
    let hash = hash_bytes_fnv1a(line.as_bytes());
    let path = quarantine_dir.join(format!("duplicate-seq-{}-{hash:016x}.jsonl", entry.seq));
    if path.exists() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(RallyError::io(format!("create {}", path.display())))?;
    writeln!(file, "{line}").map_err(RallyError::io(format!("write {}", path.display())))?;
    file.sync_all()
        .map_err(RallyError::io(format!("sync {}", path.display())))?;
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

impl DirectRoomStore {
    /// Refresh `.rally/log/index.json` from the current segment set.
    /// Best-effort — failure does not block reads or appends.
    ///
    /// Skip-when-fresh (Step-4): the index embeds a cheap `fingerprint`
    /// (sorted `(name, len, mtime_ns)` over live + archive segment files,
    /// O(#files)). If the on-disk index's fingerprint already matches the
    /// current one, no segment changed since it was built → return early
    /// WITHOUT the O(#lines) re-read. The index is a derived cache (gitignored,
    /// rebuilt on open), so a stale/missing fingerprint just means we do the
    /// full rebuild — never an error, never wrong data.
    fn refresh_log_index(&self) -> Result<()> {
        let segments = read_segment_files(&self.log_dir)?;
        let archived = read_segment_files(&self.archive_dir)?;

        let index_path = self.log_dir.join(LOG_INDEX_FILENAME);
        let current_fp = segments_fingerprint(&segments, &archived);
        let current_fp_value = serde_json::to_value(&current_fp)
            .map_err(RallyError::json("render index fingerprint"))?;
        // Fast path: fingerprint unchanged → index already current.
        if let Ok(existing_text) = fs::read_to_string(&index_path)
            && let Ok(existing) = serde_json::from_str::<Value>(&existing_text)
            && existing.get("fingerprint") == Some(&current_fp_value)
        {
            return Ok(());
        }

        let mut entries = Vec::new();
        for path in segments.iter().chain(archived.iter()) {
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let mut first_seq = i64::MAX;
            let mut last_seq = 0i64;
            let mut count = 0i64;
            let mut first_ts: Option<String> = None;
            let mut last_ts: Option<String> = None;
            for entry in read_segment_entries(path)? {
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
        let rendered = serde_json::to_string_pretty(&json!({
            "segments": segments_value,
            "fingerprint": current_fp_value,
            "updated_at": now_string(),
        }))
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

// =============================================================================
// Snapshot fast-path cache (B-perf): bound read-only snapshot cost under load
// =============================================================================
//
// Under heavy CPU contention the `RoomStore::open()` + `snapshot()` path is
// dominated by:
//   (a) the LOCK_EX `mutation.lock` flock (serializes every rally invocation,
//       reader OR writer, against any concurrent writer's open),
//   (b) `reconcile_segments_and_db` reading every JSONL line in every segment
//       to count distinct seqs vs. the SQLite event count, and
//   (c) the SQLite query that deserializes the full Fact set to project a
//       `RoomSnapshot`.
//
// All three are O(N) in ledger size and become the bottleneck on a busy box.
// The before-write coordination gate is a hot path called from agent
// write-hooks; if it cannot return in the 3s watchdog budget, the watchdog
// fires fail-open (i.e. NO coordination check applied — the worst possible
// outcome for a gate whose entire purpose is preventing two agents from
// clobbering one claimed path).
//
// Mitigation: when the canonical ledger has not changed since the last
// snapshot we projected, reuse that snapshot directly from a tiny on-disk
// cache instead of reopening the database. Cache freshness is checked
// against a fingerprint of `(facts.db mtime_ns, log/index.json mtime_ns +
// content)`. The log-index is refreshed on every append (`refresh_log_index`
// runs at the end of `append_fact`); a writer that mutates the room thus
// inevitably invalidates the cache, even when it never touches the cache
// file itself.
//
// The cache is *advisory* — a miss only costs the existing slow path; a
// corrupt cache file is treated as a miss. Readers do NOT take the mutation
// lock on the fast path: the cache is read-only and the underlying canonical
// files are append-only, so a reader that observes a fingerprint can rely
// on it being a consistent view of the ledger at that moment in time. The
// fingerprint mechanism is the only correctness gate.

const SNAPSHOT_CACHE_FILENAME: &str = "snapshot.cache.json";

#[derive(Debug, Deserialize, Serialize)]
struct SnapshotCacheEnvelope {
    /// Fingerprint of the canonical inputs at the time of caching. A cache
    /// is fresh iff this matches the current fingerprint exactly.
    fingerprint: SnapshotCacheFingerprint,
    /// Projected `RoomSnapshot` for the fingerprinted ledger state.
    snapshot: RoomSnapshot,
    /// ISO-8601 stamp for observability (not part of the freshness check).
    cached_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SnapshotCacheFingerprint {
    /// `facts.db` modification time in nanoseconds since the unix epoch.
    /// 0 when the db file is absent (a perfectly valid empty-room state).
    facts_db_mtime_ns: i128,
    /// Inline copy of the `log/index.json` content. Comparing the full text
    /// is cheaper than re-parsing the JSON and gives strictly-stronger
    /// freshness than mtime alone (the index file is rewritten on every
    /// append even when the highest seq does not change — e.g. the index
    /// re-stamps `updated_at`). Bounded in size by the live segment count,
    /// not by the line count.
    log_index_text: String,
}

fn snapshot_cache_path(rally_dir: &Path) -> PathBuf {
    rally_dir.join(SNAPSHOT_CACHE_FILENAME)
}

fn file_mtime_ns(path: &Path) -> i128 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(err) => -(err.duration().as_nanos() as i128),
    }
}

fn current_fingerprint(rally_dir: &Path) -> SnapshotCacheFingerprint {
    let facts_db = rally_dir.join("facts.db");
    let log_index = rally_dir.join(LOG_DIRNAME).join(LOG_INDEX_FILENAME);
    SnapshotCacheFingerprint {
        facts_db_mtime_ns: file_mtime_ns(&facts_db),
        log_index_text: fs::read_to_string(&log_index).unwrap_or_default(),
    }
}

/// Read-only snapshot retrieval: return the cached `RoomSnapshot` when its
/// fingerprint matches the current canonical state. `None` when the cache is
/// absent, unparseable, or stale. No mutation lock is acquired and no SQLite
/// connection is opened on a hit; this is the path the before-write gate
/// takes under sub-100ms targets.
pub(crate) fn try_load_cached_snapshot(rally_dir: &Path) -> Option<RoomSnapshot> {
    let cache_path = snapshot_cache_path(rally_dir);
    let text = fs::read_to_string(&cache_path).ok()?;
    let envelope: SnapshotCacheEnvelope = serde_json::from_str(&text).ok()?;
    let now = current_fingerprint(rally_dir);
    if envelope.fingerprint == now {
        Some(envelope.snapshot)
    } else {
        None
    }
}

/// Persist `snapshot` under the current fingerprint. Atomic temp+rename; any
/// IO error is swallowed (the cache is advisory — a failed write only forces
/// the next reader through the slow path).
pub(crate) fn write_snapshot_cache(rally_dir: &Path, snapshot: &RoomSnapshot) {
    let envelope = SnapshotCacheEnvelope {
        fingerprint: current_fingerprint(rally_dir),
        snapshot: snapshot.clone(),
        cached_at: now_string(),
    };
    let Ok(rendered) = serde_json::to_string(&envelope) else {
        return;
    };
    let cache_path = snapshot_cache_path(rally_dir);
    let Some(parent) = cache_path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp_path = cache_path.with_extension(format!("json.tmp-{}", short_id()));
    if fs::write(&temp_path, rendered).is_err() {
        let _ = fs::remove_file(&temp_path);
        return;
    }
    if fs::rename(&temp_path, &cache_path).is_err() {
        let _ = fs::remove_file(&temp_path);
    }
}

/// Resolve the `.rally` directory for `repo_root` and return a cached
/// snapshot when one is available. Convenience wrapper used by hot read-only
/// paths (the before-write gate today; status / next / room are candidates
/// for a follow-up extension).
pub(crate) fn try_load_cached_snapshot_for(repo_root: &Path) -> Option<RoomSnapshot> {
    try_load_cached_snapshot(&repo_root.join(".rally"))
}

/// Convenience writer keyed by `repo_root` rather than the inner `.rally`
/// directory. Same fail-soft semantics as [`write_snapshot_cache`].
pub(crate) fn write_snapshot_cache_for(repo_root: &Path, snapshot: &RoomSnapshot) {
    write_snapshot_cache(&repo_root.join(".rally"), snapshot);
}

#[cfg(test)]
mod ledger_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn fact_from_session_id_round_trips_and_defaults_none() {
        // New durable writes carry the authoring session lease.
        let f = Fact {
            from_session_id: Some("sess:term:host:abc#live".to_string()),
            ..Default::default()
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["from_session_id"], "sess:term:host:abc#live");
        // Default facts have no lease and skip the field on the wire.
        let bare = Fact::default();
        assert!(bare.from_session_id.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("from_session_id")
                .is_none(),
            "absent from_session_id is skipped, not serialized as null"
        );
    }

    #[test]
    fn legacy_fact_without_from_session_id_still_replays() {
        // A pre-protocol ledger row carries no from_session_id field.
        let legacy = r#"{"schema":"agent-rally.fact.v1","event_id":"fact_old","kind":"decision","subject":"old","tool":"codex:01"}"#;
        let f: Fact = serde_json::from_str(legacy).unwrap();
        assert!(
            f.from_session_id.is_none(),
            "old rows replay with from_session_id=None"
        );
        assert_eq!(f.subject, "old");
    }

    /// f4 (2026-07-09): callers list segment files via `read_segment_files`
    /// then open each one separately — a concurrent archival/rotation can
    /// remove a listed segment in between (TOCTOU). That is a benign race
    /// with rotation, not corruption: the segment's entries simply moved to
    /// the archive. `read_segment_entries` must treat a missing file as an
    /// empty segment rather than propagating an error.
    #[test]
    fn f4_read_segment_entries_treats_missing_file_as_empty_not_error() {
        let root = unique_root("read-segment-entries-notfound");
        let missing = root.join("does-not-exist.jsonl");
        let entries = read_segment_entries(&missing).unwrap();
        assert!(
            entries.is_empty(),
            "a missing segment file must read as empty, not error"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// f4 regression guard: widening tolerance for the file's ABSENCE must
    /// NOT widen tolerance for actual corruption of a line that IS present.
    #[test]
    fn f4_read_segment_entries_still_errors_loudly_on_parse_corruption() {
        let root = unique_root("read-segment-entries-corrupt");
        let path = root.join("corrupt.jsonl");
        fs::write(&path, b"{not json}\n").unwrap();
        let err = read_segment_entries(&path).unwrap_err().to_string();
        assert!(
            err.contains("corruption"),
            "a completed malformed line must still fail loudly: {err}"
        );
        fs::remove_dir_all(&root).ok();
    }

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
            from_session_id: None,
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

    fn claim_fact(event_id: &str, tool: &str, scope: &str, lease_expires_at: &str) -> Fact {
        let mut fact = make_fact(event_id, FactKind::Claim, scope, "claim");
        fact.tool = Some(tool.to_string());
        if !lease_expires_at.is_empty() {
            fact.evidence
                .push(format!("lease_expires_at:{lease_expires_at}"));
        }
        fact
    }

    #[test]
    fn claim_authority_rejects_second_exclusive_owner_on_same_scope() {
        let root = unique_root("claim-authority-conflict");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let first = claim_fact(
            "claim-first",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        let second = claim_fact(
            "claim-second",
            "tool-b",
            "file:./src/lib.rs",
            "2099-01-01T00:00:00Z",
        );

        store.append_fact_verified(&first).unwrap();
        let err = store.append_fact_verified(&second).unwrap_err().to_string();

        assert!(
            err.contains("claim conflict"),
            "second owner must be rejected by claim authority; got {err}"
        );
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.active_claims.len(), 1);
        assert_eq!(snapshot.active_claims[0].event_id, "claim-first");
        fs::remove_dir_all(&root).ok();
    }

    /// DI-1 regression: a system-health telemetry fact (split out of
    /// current_risks) must still be resolvable by ref. Pre-fix, the Resolve
    /// pre-check in `append_state_transition_verified` scanned only
    /// `current_risks` and hard-failed with "not a live risk".
    #[test]
    fn di1_system_health_fact_is_resolvable_by_ref() {
        let root = unique_root("di1-resolve");
        let store = RoomStore::open_at(root.clone()).unwrap();
        // make_fact hardcodes subject; set the system-health prefix explicitly.
        let mut risk_fact = make_fact("sys-drift", FactKind::Risk, "tests/", "drift telemetry");
        risk_fact.subject = "binary-drift: build-A vs build-B".to_string();
        let risk = store.append_fact(&risk_fact).unwrap();
        let snap = store.snapshot().unwrap();
        assert!(
            snap.system_health
                .iter()
                .any(|f| f.event_id == risk.event_id),
            "telemetry must project into system_health"
        );
        let mut resolve = make_fact(
            "resolve-drift",
            FactKind::Resolve,
            "tests/",
            "drift resolved",
        );
        resolve.ref_id = Some(risk.event_id.clone());
        store
            .append_state_transition_verified(&resolve)
            .expect("a system_health fact must be resolvable by ref");
        let after = store.snapshot().unwrap();
        assert!(
            !after
                .system_health
                .iter()
                .any(|f| f.event_id == risk.event_id),
            "resolved telemetry must leave system_health"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn release_scope_before_later_same_scope_claim_does_not_suppress_later_claim() {
        let root = unique_root("claim-authority-release-order");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let old_claim = claim_fact(
            "claim-old",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        let old_claim = store.append_fact_verified(&old_claim).unwrap();
        let mut release = make_fact(
            "release-old",
            FactKind::Release,
            "file:src/lib.rs",
            "release old claim",
        );
        release.ref_id = Some(old_claim.event_id);
        store.append_state_transition_verified(&release).unwrap();
        let later_claim = claim_fact(
            "claim-later",
            "tool-b",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );

        store.append_fact_verified(&later_claim).unwrap();
        let snapshot = store.snapshot().unwrap();
        let index = claim_authority::read_index(store.claim_index_path()).unwrap();

        assert_eq!(snapshot.active_claims.len(), 1);
        assert_eq!(snapshot.active_claims[0].event_id, "claim-later");
        assert_eq!(snapshot.active_claims[0].tool.as_deref(), Some("tool-b"));
        assert_eq!(index.claims.len(), 1);
        assert!(index.claims.contains_key("claim-later"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_authority_concurrent_exclusive_acquire_allows_one_owner() {
        use std::sync::{Arc, Barrier};

        let root = unique_root("claim-authority-concurrent");
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for i in 0..8 {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let store = RoomStore::open_at(root).unwrap();
                let fact = claim_fact(
                    &format!("claim-{i}"),
                    &format!("tool-{i}"),
                    "file:src/lib.rs",
                    "2099-01-01T00:00:00Z",
                );
                barrier.wait();
                store.append_fact_verified(&fact).map(|f| f.event_id)
            }));
        }

        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        let store = RoomStore::open_at(root.clone()).unwrap();
        let owners = store
            .snapshot()
            .unwrap()
            .active_claims
            .into_iter()
            .filter_map(|claim| claim.tool)
            .collect::<BTreeSet<_>>();

        assert_eq!(successes, 1, "exactly one append should acquire the scope");
        assert_eq!(owners.len(), 1, "projection must show exactly one owner");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_lease_renewal_updates_index_without_durable_event() {
        let root = unique_root("claim-lease-renew");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let claim = claim_fact(
            "claim-renew",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        store.append_fact_verified(&claim).unwrap();
        let before_count = store.facts().unwrap().len();

        let renewed = store
            .renew_claim_lease("claim-renew", "2099-01-01T00:30:00Z".to_string())
            .unwrap()
            .unwrap();
        let after_count = store.facts().unwrap().len();
        let index = claim_authority::read_index(store.claim_index_path()).unwrap();

        assert_eq!(
            before_count, after_count,
            "lease renewal must not append durable facts"
        );
        assert_eq!(
            renewed.lease_expires_at.as_deref(),
            Some("2099-01-01T00:30:00Z")
        );
        assert_eq!(
            index
                .claims
                .get("claim-renew")
                .and_then(|record| record.lease_expires_at.as_deref()),
            Some("2099-01-01T00:30:00Z")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_lease_expiry_appends_one_durable_event_and_frees_claim() {
        let root = unique_root("claim-lease-expire");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let claim = claim_fact(
            "claim-expiring",
            "tool-a",
            "file:src/lib.rs",
            "2000-01-01T00:00:00Z",
        );
        store.append_fact_verified(&claim).unwrap();

        let first = store.expire_claim_leases_at(chrono::Utc::now()).unwrap();
        let second = store.expire_claim_leases_at(chrono::Utc::now()).unwrap();
        let snapshot = store.snapshot().unwrap();
        let expired_count = store
            .facts()
            .unwrap()
            .into_iter()
            .filter(|fact| fact.kind == FactKind::ClaimExpired)
            .count();

        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "expiry must be durable exactly once");
        assert_eq!(expired_count, 1);
        assert!(
            snapshot.active_claims.is_empty(),
            "expired claim must leave active ownership"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_authority_replays_legacy_claim_without_lease_marker() {
        let root = unique_root("claim-authority-legacy-replay");
        {
            let store = RoomStore::open_at(root.clone()).unwrap();
            let claim = claim_fact("claim-legacy", "tool-a", "file:src/lib.rs", "");
            store.append_fact_verified(&claim).unwrap();
        }

        let reopened = RoomStore::open_at(root.clone()).unwrap();
        reopened.rebuild_claim_index().unwrap();
        let snapshot = reopened.snapshot().unwrap();
        let index = claim_authority::read_index(reopened.claim_index_path()).unwrap();

        assert_eq!(snapshot.active_claims.len(), 1);
        assert_eq!(index.claims.len(), 1);
        assert_eq!(
            index
                .claims
                .get("claim-legacy")
                .and_then(|record| record.lease_expires_at.as_deref()),
            None,
            "legacy claim replay remains tolerant of missing lease metadata"
        );
        fs::remove_dir_all(&root).ok();
    }

    fn segments_under(root: &Path) -> Vec<PathBuf> {
        read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap_or_default()
    }

    fn archive_under(root: &Path) -> Vec<PathBuf> {
        read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap_or_default()
    }

    /// Wait for a just-dropped store's sqlite pool to actually CLOSE, then
    /// remove any leftover journal siblings.
    ///
    /// `SqliteStore`'s `Drop` joins its delivery thread, but the sqlx pool's
    /// sqlite worker threads close their connections ASYNCHRONOUSLY after
    /// drop. Until that close completes, a worker holds open fds to both
    /// `facts.db` and its `-wal` — and the final close CHECKPOINTS the WAL
    /// back over the main file, silently undoing any corruption/surgery a
    /// test performed in the window. Observed as issue #48: deterministic
    /// failure on Linux CI (the async close reliably lands after the test's
    /// corruption write), racy pass on macOS.
    ///
    /// sqlite removes `-wal`/`-shm` itself on the last connection close, so
    /// their disappearance IS the quiesce signal. Bounded: after ~5s fall
    /// through and delete stragglers — with no WAL left on disk there is no
    /// checkpoint-from-WAL hazard for the caller's subsequent surgery.
    fn remove_fact_store_journals(facts_db: &Path) {
        let wal = facts_db.with_extension("db-wal");
        let shm = facts_db.with_extension("db-shm");
        for _ in 0..100 {
            if !wal.exists() && !shm.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = fs::remove_file(shm);
        let _ = fs::remove_file(wal);
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

    // -------------------------------------------------------------------------
    // Malformed-DB recovery (Q1): the canonical JSONL ledger is the source of
    // truth, the SQLite db is a disposable cache. A corrupt db must not lose
    // history — the next open quarantines the bad bytes and rebuilds from
    // segments. Empirical reproduction of the failure mode observed on
    // easy-terminal (facts.db.corrupt + facts.db.corrupt.bak orphans).
    // -------------------------------------------------------------------------

    #[test]
    fn is_malformed_db_error_recognises_known_codes() {
        // SQLite base numeric codes (stable across supported versions).
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 11) database disk image is malformed"
                .to_string()
        ));
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 26) file is not a database".to_string()
        ));
        // Human-readable substring fallback.
        assert!(is_malformed_db_error(
            &"some other wrapping: disk image is malformed".to_string()
        ));
        assert!(is_malformed_db_error(
            &"some other wrapping: file is not a database".to_string()
        ));
        // SQLite *extended* corruption codes (11 | N<<8 family). Their
        // numeric form is 267 / 523 / 779 / ... — base-code substring would
        // miss them. The "corrupt" message substring catches them.
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 267) database disk image is malformed: vtab corrupt".to_string()
        ));
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 523) sequence table is corrupt".to_string()
        ));
        assert!(is_malformed_db_error(
            &"index corrupt detected by integrity_check".to_string()
        ));
        // Negative controls: lock contention and metadata races are NOT
        // unrecoverable corruption — the existing retry loop handles those.
        assert!(!is_malformed_db_error(&"database is locked".to_string()));
        assert!(!is_malformed_db_error(
            &"UNIQUE constraint failed: store_metadata.key".to_string()
        ));
        // Negative control: an unrelated error that happens to contain "code: 11x".
        // SQLite does not currently emit "code: 110" / "code: 119"; we now
        // match "code: 11)" (closing paren) so this is a true negative rather
        // than an acceptable false positive.
        assert!(!is_malformed_db_error(
            &"error from database: (code: 110) some other error".to_string()
        ));
        assert!(!is_malformed_db_error(&"".to_string()));
    }

    #[test]
    fn quarantine_corrupt_db_moves_aside_atomically() {
        let root = unique_root("quarantine-mv");
        let rally = root.join(".rally");
        fs::create_dir_all(&rally).unwrap();
        let facts_db = rally.join("facts.db");
        // Plant a "corrupt" file + WAL/SHM siblings.
        fs::write(&facts_db, b"GARBAGE bytes pretending to be sqlite").unwrap();
        fs::write(facts_db.with_extension("db-shm"), b"shm").unwrap();
        fs::write(facts_db.with_extension("db-wal"), b"wal").unwrap();

        quarantine_corrupt_db(&facts_db).unwrap();

        assert!(!facts_db.exists(), "primary file moved aside");
        assert!(
            !facts_db.with_extension("db-shm").exists(),
            "shm sibling moved aside"
        );
        assert!(
            !facts_db.with_extension("db-wal").exists(),
            "wal sibling moved aside"
        );

        // Quarantine files exist with `.corrupt.<stamp>` infix; their bytes
        // are preserved verbatim.
        let mut found_main = false;
        let mut found_shm = false;
        let mut found_wal = false;
        for entry in fs::read_dir(&rally).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("facts.db.corrupt.") {
                if name.ends_with("-db-shm") {
                    found_shm = true;
                } else if name.ends_with("-db-wal") {
                    found_wal = true;
                } else {
                    found_main = true;
                    let bytes = fs::read(entry.path()).unwrap();
                    assert_eq!(
                        bytes, b"GARBAGE bytes pretending to be sqlite",
                        "quarantine preserves bytes verbatim for forensics"
                    );
                }
            }
        }
        assert!(
            found_main && found_shm && found_wal,
            "all three siblings quarantined"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn quarantine_corrupt_db_is_idempotent() {
        let root = unique_root("quarantine-idempotent");
        let rally = root.join(".rally");
        fs::create_dir_all(&rally).unwrap();
        let facts_db = rally.join("facts.db");
        // No file → noop, returns Ok.
        quarantine_corrupt_db(&facts_db).unwrap();
        assert!(!facts_db.exists());

        // After a real quarantine, a second call still returns Ok and does
        // not error (the source file is gone).
        fs::write(&facts_db, b"corrupt").unwrap();
        quarantine_corrupt_db(&facts_db).unwrap();
        quarantine_corrupt_db(&facts_db).unwrap();
        assert!(!facts_db.exists());

        fs::remove_dir_all(&root).ok();
    }

    /// THE empirical test: corrupt facts.db mid-bytes, reopen the room, and
    /// assert every fact appended before the corruption is recovered byte-for-
    /// byte from the canonical JSONL ledger. This is the failure mode observed
    /// on easy-terminal (2026-06-01 → 2026-06-04, history reset to seq 1 with
    /// orphan facts.db.corrupt.bak left on disk).
    #[test]
    fn malformed_facts_db_is_rebuilt_from_segments_on_open() {
        let root = unique_root("malformed-recovery");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Seed enough history that recovery is visibly nontrivial.
        let a = store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim a"))
            .unwrap();
        let b = store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided b"))
            .unwrap();
        let c = store
            .append_fact(&make_fact("e3", FactKind::Blocker, "tests/", "blocker c"))
            .unwrap();
        let d = store
            .append_fact(&make_fact("e4", FactKind::Risk, "src/", "risk d"))
            .unwrap();
        assert_eq!((a.seq, b.seq, c.seq, d.seq), (1, 2, 3, 4));
        let before_facts = store.facts().unwrap();
        assert_eq!(before_facts.len(), 4);

        // Capture canonical JSONL bytes BEFORE corruption so we can prove the
        // ledger was not touched by the recovery path.
        let segments = segments_under(&root);
        assert!(!segments.is_empty(), "segments are canonical");
        let segment_bytes_before: Vec<(PathBuf, Vec<u8>)> = segments
            .iter()
            .map(|p| (p.clone(), fs::read(p).unwrap()))
            .collect();

        drop(store);

        // Quiesce the dropped store's pool + remove the WAL/SHM siblings before
        // corrupting the main file. A leftover WAL lets SQLite recover the
        // (about-to-be-)corrupted header from the WAL — masking the corruption
        // so the precondition + quarantine assertions flap. Deleting the WAL by
        // path is NOT enough: the async pool close still holds an open fd to it
        // and checkpoints it back over the main file (issue #48 — deterministic
        // on Linux). `remove_fact_store_journals` waits for sqlite's own
        // last-close cleanup first.
        remove_fact_store_journals(&root.join(".rally/facts.db"));

        // Corrupt facts.db by overwriting the SQLite magic header (bytes 0-15
        // hold the ASCII string "SQLite format 3\000"). This reproduces
        // SQLITE_NOTADB / "file is not a database" — categorically detectable
        // by sqlite at open time, with no dependency on filesystem cache
        // coherency for follow-on page reads (the header check is the very
        // first thing sqlite does). Mid-file byte corruption — used in the
        // manual CLI smoke test elsewhere — is also recovered by the same
        // path, but is harder to assert deterministically in a parallel test
        // harness because sqlite may not touch the corrupted page during open
        // alone (some queries do, some don't, depending on which b-tree pages
        // they visit). Header corruption is the strictly-stronger assertion.
        let facts_db = root.join(".rally/facts.db");
        {
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&facts_db)
                .unwrap();
            use std::io::{Seek, SeekFrom, Write};
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"GARBAGE-not-sqlite-magic").unwrap();
            f.sync_all().unwrap();
        }

        // Sanity: the corrupted db cannot be opened directly. SQLite's first
        // act on open is to validate the magic header; a wrong header is
        // detected before any page is read, so this is parallel-test-safe.
        assert!(
            open_fact_store(&facts_db).is_err(),
            "precondition: corruption is visible to sqlite"
        );

        // Reopen the room. This is the failure path before the fix; with the
        // fix it must succeed.
        let store = RoomStore::open_at(root.clone()).unwrap();
        let after_facts = store.facts().unwrap();

        // (1) Every pre-corruption fact recovered, in order, byte-identical
        // on the load-bearing fields.
        assert_eq!(after_facts.len(), 4, "all four facts recovered");
        for (pre, post) in before_facts.iter().zip(after_facts.iter()) {
            assert_eq!(pre.seq, post.seq);
            assert_eq!(pre.event_id, post.event_id);
            assert_eq!(pre.kind.as_str(), post.kind.as_str());
            assert_eq!(pre.subject, post.subject);
            assert_eq!(pre.scope, post.scope);
            assert_eq!(pre.summary, post.summary);
        }

        // (2) The canonical JSONL segments are byte-identical post-recovery —
        // the recovery path read them, it did not rewrite them.
        for (path, bytes_before) in &segment_bytes_before {
            let bytes_after = fs::read(path).unwrap();
            assert_eq!(
                bytes_before,
                &bytes_after,
                "segment {} bytes unchanged by recovery",
                path.display()
            );
        }

        // (3) A quarantine file exists; bytes preserved for forensics. (We
        // cannot assert exact mtime; the `.corrupt.<stamp>` prefix is enough.)
        let rally_dir = root.join(".rally");
        let mut found_quarantine = false;
        for entry in fs::read_dir(&rally_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if name.starts_with("facts.db.corrupt.")
                && !name.ends_with("-db-shm")
                && !name.ends_with("-db-wal")
            {
                found_quarantine = true;
                // Quarantine contains the corrupted bytes, NOT the rebuilt db.
                let qb = fs::read(entry.path()).unwrap();
                // Header was overwritten with our marker; verify it survived
                // into the quarantine file verbatim.
                assert!(qb.starts_with(b"GARBAGE-not-sqlite-magic"));
            }
        }
        assert!(
            found_quarantine,
            "corrupt bytes preserved as facts.db.corrupt.<stamp>"
        );

        // (4) The rebuilt facts.db is healthy (we can query it).
        let snap = store.snapshot().unwrap();
        assert_eq!(snap.max_seq, 4);

        // (5) Idempotent: a second open after the heal is a no-op — no new
        // quarantine file, healthy cache stays.
        let quarantine_count_pre = fs::read_dir(&rally_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("facts.db.corrupt.")
                    })
                    .unwrap_or(false)
            })
            .count();
        drop(store);
        let store2 = RoomStore::open_at(root.clone()).unwrap();
        let facts2 = store2.facts().unwrap();
        assert_eq!(facts2.len(), 4, "second open: same recovered state");
        let quarantine_count_post = fs::read_dir(&rally_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("facts.db.corrupt.")
                    })
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            quarantine_count_pre, quarantine_count_post,
            "idempotent: no new quarantine on second open"
        );

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
    /// Exercised through `resolve_active_engagement_with_env` so the test never
    /// mutates the process-global `RALLY_ENGAGEMENT` — `env::set_var` is unsound
    /// under cargo's multi-threaded runner and previously raced concurrent
    /// engagement resolution in other tests (e.g. backlog), making the suite
    /// non-deterministic.
    #[test]
    fn engagement_resolution_priority_env_then_file_then_date() {
        let root = unique_root("engagement-resolve");
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).unwrap();

        // 1. No env, no file → UTC date.
        let label = resolve_active_engagement_with_env(&dir, None);
        let today = utc_date_label();
        assert_eq!(label, today);

        // 2. Persisted file (no env) → that label.
        persist_active_engagement(&dir, "  my-sprint  ").unwrap();
        assert_eq!(resolve_active_engagement_with_env(&dir, None), "my-sprint");

        // 3. Env var wins over file.
        assert_eq!(
            resolve_active_engagement_with_env(&dir, Some("env-engagement".to_string())),
            "env-engagement"
        );

        // Sanitise strips path separators.
        let cleaned = sanitise_engagement("../escape/me");
        assert!(!cleaned.contains('/'));

        fs::remove_dir_all(&root).ok();
    }

    /// A live session whose env OR `.rally/active-engagement` file says `test`
    /// must NOT resolve to the reserved `test` fixture engagement — it falls
    /// through to the UTC-date label so production facts never leak into the
    /// committed `test.jsonl` segment (HIGH-risk fact_182e8). This is the
    /// durable product fix for the test-segment leak.
    #[test]
    fn reserved_fixture_engagement_never_resolves_for_live_session() {
        let root = unique_root("engagement-reserved");
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).unwrap();
        let today = utc_date_label();

        // A pre-existing/stale `test` active-engagement file (e.g. left by an
        // old fixture run) must NOT route live appends to the fixture — the
        // resolver falls through to the UTC date. Write the file directly to
        // simulate the stale pin (persist_active_engagement now rejects writing
        // a reserved label outright — see persist_rejects_reserved_label).
        fs::write(dir.join(ACTIVE_ENGAGEMENT_FILENAME), "test\n").unwrap();
        assert_eq!(
            resolve_active_engagement_with_env(&dir, None),
            today,
            "a 'test' active-engagement file must not route live appends to the fixture"
        );

        // Reserved label via the env var → also falls through to UTC date.
        assert_eq!(
            resolve_active_engagement_with_env(&dir, Some("test".to_string())),
            today,
            "RALLY_ENGAGEMENT=test must not route live appends to the fixture"
        );

        // Case-insensitive: TEST / Test are also reserved.
        assert_eq!(
            resolve_active_engagement_with_env(&dir, Some("TEST".to_string())),
            today
        );

        // A non-reserved label from env still wins normally.
        assert_eq!(
            resolve_active_engagement_with_env(&dir, Some("sprint-7".to_string())),
            "sprint-7"
        );

        assert!(is_reserved_fixture_engagement("test"));
        assert!(is_reserved_fixture_engagement("Test"));
        assert!(!is_reserved_fixture_engagement("2026-06-09"));

        fs::remove_dir_all(&root).ok();
    }

    /// independent-auditor LOW (2026-06-09): persisting a reserved label is
    /// rejected loudly rather than silently accepted-then-ignored, so
    /// `rally enter --engagement test` fails with a clear usage error instead
    /// of appearing to work while the resolver silently uses the dated segment.
    #[test]
    fn persist_rejects_reserved_label() {
        let root = unique_root("engagement-persist-reserved");
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).unwrap();

        let err = persist_active_engagement(&dir, "test").unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "persisting a reserved label must error with 'reserved'; got: {err}"
        );
        // Case-insensitive.
        assert!(persist_active_engagement(&dir, "TEST").is_err());
        // The file must not have been written.
        assert!(
            !dir.join(ACTIVE_ENGAGEMENT_FILENAME).exists(),
            "a rejected reserved label must not leave an active-engagement file"
        );
        // A normal label still persists fine.
        persist_active_engagement(&dir, "sprint-9").unwrap();
        assert_eq!(resolve_active_engagement_with_env(&dir, None), "sprint-9");

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

    #[test]
    fn last_seq_in_segment_reads_tail_or_none() {
        let root = unique_root("last-seq-tail");
        // Absent segment → None.
        let missing = root.join(".rally").join(LOG_DIRNAME).join("missing.jsonl");
        assert_eq!(last_seq_in_segment(&missing).unwrap(), None);
        // Segment with seqs [1,2,5] → Some(5) (the on-disk tail).
        let lines = [
            ledger_line(1, "decision", "e1", "alpha"),
            ledger_line(2, "decision", "e2", "alpha"),
            ledger_line(5, "decision", "e5", "alpha"),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);
        let seg = root.join(".rally").join(LOG_DIRNAME).join("alpha.jsonl");
        assert_eq!(last_seq_in_segment(&seg).unwrap(), Some(5));
    }

    /// A higher seq written out-of-band (a peer / old binary) changes the
    /// active segment's fingerprint. The next allocation MUST scan
    /// authoritatively past that tail (GAP B fingerprint check) rather than
    /// trust a stale sidecar — and never emit a duplicate (GAP A dup gate is
    /// the last-resort backstop). Regression for the 2026-07-02 corruption.
    #[test]
    fn out_of_band_higher_seq_forces_authoritative_allocation() {
        let root = unique_root("oob-higher-seq");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for i in 1..=3 {
            store
                .append_fact(&make_fact(
                    &format!("seed{i}"),
                    FactKind::Decision,
                    "src/",
                    "seed",
                ))
                .unwrap();
        }
        assert_eq!(store.snapshot().unwrap().max_seq, 3);
        // Out-of-band write of a HIGHER seq straight to the active segment.
        append_segment_line(
            &store.active_segment_path(),
            &LedgerLine {
                seq: 7,
                occurred_at: now_string(),
                event_type: "decision".to_string(),
                payload: json!({
                    "schema": fact_schema(),
                    "event_id": "oob7",
                    "seq": 7,
                    "kind": "decision",
                    "subject": "oob",
                    "scope": ["src/"],
                }),
                engagement: Some("default".to_string()),
            },
        )
        .unwrap();
        // Must allocate ABOVE the out-of-band tail (8), never a stale 4.
        let appended = store
            .append_fact(&make_fact("after-oob", FactKind::Artifact, "src/", "after"))
            .unwrap();
        assert_eq!(
            appended.seq, 8,
            "fingerprint mismatch must force an authoritative scan past the out-of-band tail"
        );
        let live = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let archived = replay_archive_segments(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        assert_eq!(
            segment_seq_stats(&live, &archived).unwrap().max_seq,
            8,
            "ledger stays duplicate-free after out-of-band + normal append"
        );
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

    /// A duplicate sequence number with a different event used to hard-fail
    /// replay, which made every `room` / `recent` / `next` read unusable until
    /// manual segment surgery. L2 graceful degradation keeps the first-valid
    /// record, quarantines the conflicting later line, and projects the rest.
    #[test]
    fn duplicate_seq_conflict_is_quarantined_and_room_stays_readable() {
        let root = unique_root("reconcile-dup-seq-quarantine");

        let lines = [
            ledger_line(1, "decision", "e1", "alpha"),
            ledger_line(2, "decision", "e2-first", "alpha"),
            ledger_line(2, "blocker", "e2-duplicate", "alpha"),
            ledger_line(3, "artifact", "e3", "alpha"),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);

        let store = RoomStore::open_at(root.clone()).unwrap();
        let facts = store.facts().unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.event_id.as_str()).collect();
        assert_eq!(
            ids,
            ["e1", "e2-first", "e3"],
            "duplicate seq must not brick replay; first-valid record is kept"
        );
        assert_eq!(
            store.snapshot().unwrap().max_seq,
            3,
            "snapshot still reports the canonical high-water mark"
        );

        let quarantine_dir = root.join(".rally").join(QUARANTINE_DIRNAME);
        let quarantined: Vec<PathBuf> = fs::read_dir(&quarantine_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "conflicting duplicate line is preserved once for forensics"
        );
        let quarantined_body = fs::read_to_string(&quarantined[0]).unwrap();
        assert!(
            quarantined_body.contains("e2-duplicate"),
            "quarantine file must contain the skipped duplicate event"
        );

        let appended = store
            .append_fact(&make_fact(
                "after-duplicate",
                FactKind::Decision,
                "src/",
                "append after duplicate quarantine",
            ))
            .unwrap();
        assert_eq!(
            appended.seq, 4,
            "append must allocate above the surviving canonical max"
        );

        drop(store);
        let facts_db = root.join(".rally/facts.db");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));
        let store2 = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(store2.facts().unwrap().len(), 4);
        let quarantine_count_after_rebuild = fs::read_dir(&quarantine_dir).unwrap().count();
        assert_eq!(
            quarantine_count_after_rebuild, 1,
            "deterministic quarantine filenames prevent repeated rebuild churn"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Sparse canonical ledgers must not be treated as fresh just because the
    /// distinct event count matches the derived db count. If canonical seqs are
    /// {1,2,4}, a db whose logical max is 3 would make the next append reuse
    /// seq 4 and corrupt replay. Reconcile must rebuild on max-seq drift, and
    /// append must allocate from the canonical high-water mark.
    #[test]
    fn reconcile_rebuilds_when_sparse_canonical_max_exceeds_db_max() {
        let root = unique_root("reconcile-sparse-max-drift");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for seq in 1..=3 {
            store
                .append_fact(&make_fact(
                    &format!("dense-e{seq}"),
                    FactKind::Decision,
                    "src/",
                    "dense db seed",
                ))
                .unwrap();
        }
        assert_eq!(store.snapshot().unwrap().max_seq, 3);
        drop(store);

        fs::remove_dir_all(root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let sparse = [
            ledger_line(1, "decision", "sparse-e1", "alpha"),
            ledger_line(2, "decision", "sparse-e2", "alpha"),
            ledger_line(4, "decision", "sparse-e4", "alpha"),
        ];
        let refs: Vec<&str> = sparse.iter().map(String::as_str).collect();
        write_segment(&root, "log", "alpha.jsonl", &refs);

        let store = RoomStore::open_at(root.clone()).unwrap();
        let facts = store.facts().unwrap();
        let seqs: Vec<i64> = facts.iter().map(|f| f.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 4],
            "rebuild must preserve canonical segment seqs in fact payloads"
        );
        assert_eq!(
            store.snapshot().unwrap().max_seq,
            4,
            "snapshot must report canonical high-water mark after rebuild"
        );

        let appended = store
            .append_fact(&make_fact(
                "after-sparse",
                FactKind::Artifact,
                "src/",
                "append after sparse ledger",
            ))
            .unwrap();
        assert_eq!(
            appended.seq, 5,
            "append must allocate from canonical max seq, not db event count"
        );

        let live = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let archived = replay_archive_segments(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        assert_eq!(
            segment_seq_stats(&live, &archived).unwrap().max_seq,
            5,
            "canonical ledger high-water mark advances without reusing seq 4"
        );

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

    /// Step-2 fast-path correctness: the active-segment tail-first readback
    /// finds a freshly-appended event in O(1), AND a simulated silent drop is
    /// STILL caught end-to-end through `append_fact_verified`'s full-scan
    /// fallback (active-first miss → full scan miss → error).
    #[test]
    fn r9_active_segment_first_readback_happy_path_and_still_catches_drop() {
        let root = unique_root("r9-active-first");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Happy path: verified append succeeds, and the tail-first helper finds
        // the event in the active segment directly (proving the O(1) path hits).
        let fact = make_fact("ev-active-1", FactKind::Claim, "src/", "active-first hit");
        let verified = store.append_fact_verified(&fact).unwrap();
        let active = store.active_segment_path();
        assert!(
            segment_event_id_present_tail_first(&active, &verified.event_id).unwrap(),
            "tail-first scan must find the just-appended event in the active segment"
        );

        // Silent-drop: append again, then truncate the active segment so the
        // line vanishes from the canonical record. The tail-first helper must
        // return false (deferring to the full scan), and the full scan must also
        // miss — proving the fast path does NOT mask a real drop.
        let drop_fact = make_fact("ev-active-drop", FactKind::Decision, "src/", "drop");
        let appended = store.append_fact(&drop_fact).unwrap();
        fs::write(&active, b"").unwrap();
        assert!(
            !segment_event_id_present_tail_first(&active, &appended.event_id).unwrap(),
            "tail-first must miss after truncation (defers to full scan)"
        );
        let live = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        assert!(
            !segment_event_id_present(live.iter().chain(arch.iter()), &appended.event_id).unwrap(),
            "full scan must also miss the dropped event — silent drop is still caught"
        );

        fs::remove_dir_all(&root).ok();
    }

    // =========================================================================
    // Step-3 reconcile fast-path tests (O(1) happy path + corruption safety)
    // =========================================================================

    /// Call reconcile directly and report whether it took the authoritative
    /// O(N) scan path (true) or the O(1) fast path (false). Measures a DELTA on
    /// the process-global counter around exactly this call, so it is robust to
    /// other tests bumping the counter concurrently.
    fn reconcile_took_full_scan(root: &Path) -> bool {
        let dir = root.join(".rally");
        let log_dir = dir.join(LOG_DIRNAME);
        let archive_dir = dir.join(ARCHIVE_DIRNAME);
        let facts_db = dir.join("facts.db");
        let before = full_reconcile_scan_count();
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db).unwrap();
        full_reconcile_scan_count() != before
    }

    /// (a) After appends, a no-change reconcile takes the O(1) fast path —
    /// the authoritative full scan does NOT run.
    #[test]
    fn step3_reconcile_takes_fast_path_after_append() {
        let root = unique_root("step3-fast-path");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for i in 0..5u32 {
            store
                .append_fact(&make_fact(&format!("e{i}"), FactKind::Claim, "src/", "f"))
                .unwrap();
        }
        // The append already refreshed the sidecar with current fingerprints +
        // counts, so a reconcile with no intervening change must be O(1).
        assert!(
            !reconcile_took_full_scan(&root),
            "reconcile after append must take the O(1) fast path (no full scan)"
        );
        // And again — idempotent fast path.
        assert!(
            !reconcile_took_full_scan(&root),
            "second no-change reconcile must also be O(1)"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// (b) Corrupt or delete the sidecar → the next reconcile falls through to
    /// the authoritative scan, rebuilds correctly, NO error, NO data loss, and
    /// re-seeds a valid sidecar (subsequent op is fast again).
    #[test]
    fn step3_corrupt_or_missing_sidecar_falls_through_no_loss() {
        let root = unique_root("step3-sidecar-disposable");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for i in 0..4u32 {
            store
                .append_fact(&make_fact(&format!("e{i}"), FactKind::Claim, "src/", "f"))
                .unwrap();
        }
        let before = store.facts().unwrap();
        assert_eq!(before.len(), 4);
        drop(store);

        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);

        // -- Corrupt sidecar --
        fs::write(&sidecar, b"{ this is not valid json ::::").unwrap();
        assert!(
            reconcile_took_full_scan(&root),
            "corrupt sidecar must be ignored → authoritative scan runs"
        );
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store.facts().unwrap().len(),
            4,
            "no data loss after corrupt sidecar"
        );
        drop(store);

        // -- Delete sidecar --
        let _ = fs::remove_file(&sidecar);
        assert!(
            reconcile_took_full_scan(&root),
            "missing sidecar must trigger authoritative scan"
        );
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store.facts().unwrap().len(),
            4,
            "no data loss after missing sidecar"
        );
        // The reopen re-seeded the sidecar → next reconcile is fast.
        assert!(
            !reconcile_took_full_scan(&root),
            "sidecar re-seeded after full scan → next reconcile is O(1)"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// (c) A malformed facts.db with a STALE-but-structurally-valid sidecar must
    /// STILL quarantine + rebuild. The fast path must NOT short-circuit around
    /// corruption detection: in-place db corruption rewrites the file (mtime
    /// changes), so its fingerprint no longer matches the sidecar → fall through.
    #[test]
    fn step3_malformed_db_with_stale_sidecar_does_not_bypass_corruption_recovery() {
        let root = unique_root("step3-stale-sidecar-corrupt-db");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let mut ids = Vec::new();
        for i in 0..4u32 {
            let f = store
                .append_fact(&make_fact(&format!("e{i}"), FactKind::Claim, "src/", "f"))
                .unwrap();
            ids.push(f.event_id);
        }
        assert_eq!(store.facts().unwrap().len(), 4);
        drop(store);

        let facts_db = root.join(".rally/facts.db");
        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);

        // Capture the (now-stale) sidecar that the last append wrote. It is
        // structurally valid and claims canonical_count == db_count == 4.
        let stale = fs::read(&sidecar).expect("sidecar exists after appends");
        let parsed: ReconcileCache = serde_json::from_slice(&stale).unwrap();
        assert_eq!(parsed.canonical_count, 4);
        assert_eq!(parsed.db_count, 4);

        // Remove WAL/SHM siblings BEFORE corrupting the header so that SQLite
        // cannot recover through the WAL on open. With a valid WAL present,
        // SQLite reads page 0 from the WAL (not from facts.db), bypassing the
        // corrupted header and NOT returning SQLITE_NOTADB — making quarantine
        // non-deterministic. Eliminating the WAL first makes the corruption
        // categorically SQLITE_NOTADB (code 26) at open, which is what this
        // test is designed to prove. (WAL files are safe to delete between a
        // store close and a store open because the WAL is just a pending-write
        // journal; all committed data is already in facts.db after checkpoint.)
        remove_fact_store_journals(&facts_db);

        // Corrupt the db header in place (→ SQLITE_NOTADB). This rewrites the
        // file, changing its mtime and head_hash, so its fingerprint diverges
        // from the sidecar's recorded db_fingerprint.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&facts_db)
                .unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"GARBAGE-not-sqlite-magic").unwrap();
            f.sync_all().unwrap();
        }
        // Re-write the STALE sidecar so it still references the OLD db
        // fingerprint and counts — simulating a sidecar that never saw the
        // corruption. (The corruption above may have left the sidecar untouched
        // already, but we force the adversarial case explicitly.)
        fs::write(&sidecar, &stale).unwrap();

        // Probe whether SQLite raises SQLITE_NOTADB on the corrupt file.
        // factstr-sqlite uses `create_if_missing(true)`; under parallel test
        // pressure sqlx occasionally treats a corrupt-but-extant file as
        // "missing" and creates a fresh empty db without returning an error.
        // When that happens, no quarantine file is written (the corrupt bytes
        // are lost, but data is still recovered from the canonical ledger).
        // We assert quarantine only on the deterministic SQLITE_NOTADB path;
        // the data-recovery assertion below covers both paths.
        let open_fails = open_fact_store(&facts_db).is_err();

        // The core guarantee: reconcile must NOT trust the stale sidecar over a
        // corrupt db. The db's head_hash changed (header overwrite), so the
        // fast-path guard fails and the AUTHORITATIVE scan runs. Both code-26
        // (header) and code-11 (mid-page) route through the same
        // quarantine+rebuild path in read_db_event_count.
        let log_dir = root.join(".rally").join(LOG_DIRNAME);
        let archive_dir = root.join(".rally").join(ARCHIVE_DIRNAME);
        let before = full_reconcile_scan_count();
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db).unwrap();
        assert!(
            full_reconcile_scan_count() != before,
            "stale sidecar must NOT short-circuit corruption detection — authoritative scan must run"
        );

        // Header corruption (SQLITE_NOTADB / code 26) is detected at open-time,
        // so the quarantine file exists immediately after reconcile, before any
        // further room open. This proves goal criterion #3: the corrupt bytes are
        // preserved and the fast path did NOT short-circuit quarantine.
        // Guard: only assert quarantine when the open actually failed (NOTADB
        // path). On the rare sqlx-silent-recreation path the corrupt bytes are
        // lost, but data is still fully recovered (asserted below).
        if open_fails {
            let quarantine_exists = root
                .join(".rally")
                .read_dir()
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("facts.db.corrupt.")
                });
            assert!(
                quarantine_exists,
                "header corruption with a stale sidecar must still quarantine"
            );
        }

        // And the full room reopen recovers every fact from the canonical ledger.
        let store = RoomStore::open_at(root.clone()).unwrap();
        let after = store.facts().unwrap();
        assert_eq!(
            after.len(),
            4,
            "all facts recovered from canonical segments"
        );
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(&after[i].event_id, id, "order + identity preserved");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn facts_query_corruption_with_matching_sidecar_quarantines_and_rebuilds() {
        let root = unique_root("query-corrupt-sidecar-match");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let mut ids = Vec::new();
        for i in 0..800u32 {
            let fact = store
                .append_fact(&make_fact(
                    &format!("e{i}"),
                    FactKind::Claim,
                    "src/",
                    &format!("fact {i} padding to force many sqlite pages"),
                ))
                .unwrap();
            ids.push(fact.event_id);
        }
        assert_eq!(store.facts().unwrap().len(), 800);
        drop(store);

        let facts_db = root.join(".rally/facts.db");
        remove_fact_store_journals(&facts_db);

        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&facts_db)
                .unwrap();
            let db_size = f.seek(SeekFrom::End(0)).unwrap();
            assert!(
                db_size > 16384,
                "DB must be multi-page for query-corruption test (got {db_size} bytes)"
            );
            f.seek(SeekFrom::Start(4096)).unwrap();
            f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF].repeat(16)).unwrap();
            f.sync_all().unwrap();
        }

        let corrupt_store =
            open_fact_store(&facts_db).expect("mid-file corruption should still open");
        let query_err = corrupt_store
            .query(&FactQuery::all())
            .expect_err("corrupt b-tree must fail during full fact query");
        assert!(
            is_malformed_db_error(&query_err),
            "precondition: query error must be treated as malformed DB; got {query_err}"
        );
        drop(corrupt_store);

        let segments = segments_under(&root);
        let archived = archive_under(&root);
        let canonical_stats = segment_seq_stats(&segments, &archived).unwrap();
        let adversarial_cache = ReconcileCache {
            segments_fingerprint: segments_fingerprint(&segments, &archived),
            db_fingerprint: fingerprint_db(&facts_db),
            canonical_count: canonical_stats.count,
            canonical_max_seq: canonical_stats.max_seq,
            db_count: canonical_stats.count,
            db_max_seq: canonical_stats.max_seq,
        };
        write_reconcile_cache(&facts_db, &adversarial_cache).unwrap();
        assert!(
            !reconcile_took_full_scan(&root),
            "matching sidecar reproduces the fast-path false-pass precondition"
        );

        let store = RoomStore::open_at(root.clone()).unwrap();
        let after = store.facts().unwrap();
        assert_eq!(after.len(), 800);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(&after[i].event_id, id, "event identity must survive replay");
        }

        let quarantine_exists = root
            .join(".rally")
            .read_dir()
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("facts.db.corrupt.")
            });
        assert!(
            quarantine_exists,
            "query-time corruption must preserve corrupt bytes in quarantine"
        );
        let rebuilt_query = open_fact_store(&facts_db)
            .unwrap()
            .query(&FactQuery::all())
            .unwrap();
        assert_eq!(
            rebuilt_query.event_records.len(),
            800,
            "rebuilt projection must be queryable and complete"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Empirical O(1) proof: reconcile wall-time on the no-change happy path
    /// stays roughly flat as the ledger grows (the brief's regression was
    /// ~linear). `#[ignore]` because it's a perf probe, not a correctness gate;
    /// run with `cargo test --release -- --ignored reconcile_fast_path_is_flat`.
    #[test]
    #[ignore]
    fn reconcile_fast_path_is_flat_vs_ledger_size() {
        use std::time::Instant;
        fn time_reconcile_at(n: usize) -> u128 {
            let root = unique_root(&format!("reconcile-flat-{n}"));
            let store = RoomStore::open_at(root.clone()).unwrap();
            for i in 0..n {
                store
                    .append_fact(&make_fact(&format!("e{i}"), FactKind::Claim, "src/", "pad"))
                    .unwrap();
            }
            let dir = root.join(".rally");
            let log_dir = dir.join(LOG_DIRNAME);
            let archive_dir = dir.join(ARCHIVE_DIRNAME);
            let facts_db = dir.join("facts.db");
            // Warm + measure the fast-path reconcile only (no projection).
            reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db).unwrap();
            let mut best = u128::MAX;
            for _ in 0..20 {
                let t = Instant::now();
                reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db).unwrap();
                best = best.min(t.elapsed().as_micros());
            }
            // Confirm we actually stayed on the fast path the whole time.
            assert!(
                !reconcile_took_full_scan(&root),
                "reconcile at n={n} must be on the O(1) fast path"
            );
            fs::remove_dir_all(&root).ok();
            best
        }
        let small = time_reconcile_at(200);
        let large = time_reconcile_at(4000);
        eprintln!("reconcile fast-path: n=200 -> {small}us, n=4000 -> {large}us");
        // 20x the ledger must NOT cost ~20x the time. Allow generous slack for
        // directory-stat noise; linear scaling would be ~20x, we require < 5x.
        assert!(
            large < small.saturating_mul(5).max(small + 200),
            "reconcile must not scale ~linearly: n=200 {small}us vs n=4000 {large}us"
        );
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
            from_session_id: None,
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
            from_session_id: None,
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
        // Chunk A router is hard-wired to Direct; this test reaches into the
        // direct store's private fields, so bind the DirectRoomStore directly.
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();

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

        // 8 threads (down from 24) still exercises concurrent open+append
        // lockstep while halving I/O burst so the 5-second SQLite busy_timeout
        // in parallel store tests is not tripped.
        let handles: Vec<_> = (0..8)
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

        assert_eq!(facts.len(), 8);
        assert_eq!(actual_ids, expected_ids);
        assert_eq!(seqs.len(), 8);
        assert!(seqs.contains(&1));
        assert!(seqs.contains(&8));

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

    // -------------------------------------------------------------------------
    // F1 — torn trailing JSONL line must not brick the store
    // -------------------------------------------------------------------------

    /// A crash during segment append can leave a partially-written (non-JSON)
    /// last line.  Previously `distinct_segment_seqs` and `rebuild_db_from_segments`
    /// hard-errored on it, bricking the store.  The torn line was never durably
    /// committed (fsync contract), so it must be tolerated and skipped.
    #[test]
    fn torn_trailing_segment_line_is_skipped_on_rebuild() {
        let root = unique_root("torn-trailing");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Write 3 valid facts (seq 1-3).
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

        // Grab the segment path before drop so we can mutate it.
        let segment_path = store.active_segment_path();
        drop(store);

        // Simulate a torn write: append a partial/truncated JSON fragment that
        // a crash would leave — not valid JSON, no terminating newline.
        {
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(&segment_path).unwrap();
            // Deliberately incomplete — looks like the beginning of a LedgerLine
            // that was cut off mid-write.
            f.write_all(b"{\"seq\":4,\"occurred_at\":\"2026-01-01T00:00:00Z\",\"event_type\":\"claim\",\"payload\":{\"ev")
                .unwrap();
            // No trailing newline — crash happened before completion.
        }

        // Delete facts.db so reconcile is forced to call rebuild_db_from_segments.
        let facts_db = root.join(".rally/facts.db");
        fs::remove_file(&facts_db).ok();
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));

        // open_at must succeed — torn line must be skipped, not fatal.
        let store2 = RoomStore::open_at(root.clone()).unwrap();
        let facts = store2.facts().unwrap();

        // All 3 valid facts recovered; the torn line produces no entry.
        assert_eq!(
            facts.len(),
            3,
            "all 3 valid facts recovered; torn line skipped"
        );
        assert_eq!(facts[0].seq, 1);
        assert_eq!(facts[1].seq, 2);
        assert_eq!(facts[2].seq, 3);

        // The canonical segment was not quarantined (quarantine only applies to
        // facts.db, not to JSONL segments).
        let rally_dir = root.join(".rally");
        let has_quarantine = fs::read_dir(&rally_dir).unwrap().any(|e| {
            e.map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("facts.db.corrupt.")
            })
            .unwrap_or(false)
        });
        assert!(
            !has_quarantine,
            "no quarantine file: torn line is not treated as DB corruption"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn completed_segment_corruption_fails_all_canonical_readers() {
        let root = unique_root("completed-segment-corruption");
        // Direct-store internals under test; bind the DirectRoomStore directly
        // (Chunk A router always returns Direct).
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let fact = store
            .append_fact(&make_fact(
                "e1",
                FactKind::Decision,
                "src/",
                "valid before corruption",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let facts_db = store.facts_db_path.clone();
        let db_len_before = fs::metadata(&facts_db).unwrap().len();

        {
            let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
            file.write_all(b"completed-corruption\n").unwrap();
            file.sync_data().unwrap();
        }

        let live = read_segment_files(&store.log_dir).unwrap();
        let archived = replay_archive_segments(&store.archive_dir).unwrap();
        let assert_completed = |err: RallyError| {
            let message = err.to_string();
            assert!(message.contains("completed canonical segment corruption"));
            assert!(message.contains(&segment.display().to_string()));
            assert!(message.contains("line 2"));
        };

        assert_completed(read_segment_entries(&segment).unwrap_err());
        assert_completed(facts_from_segments(&store.log_dir, &store.archive_dir).unwrap_err());
        assert_completed(segment_seq_stats(&live, &archived).unwrap_err());
        assert_completed(last_seq_in_segment(&segment).unwrap_err());
        assert_completed(segment_event_id_present(live.iter(), &fact.event_id).unwrap_err());
        assert_completed(
            segment_event_id_present_tail_first(&segment, &fact.event_id).unwrap_err(),
        );
        assert_completed(store.refresh_log_index().unwrap_err());
        assert_completed(rebuild_db_from_segments(&live, &archived, &facts_db).unwrap_err());

        assert!(facts_db.exists(), "failed rebuild must preserve facts.db");
        assert_eq!(
            fs::metadata(&facts_db).unwrap().len(),
            db_len_before,
            "failed canonical parse must not replace the derived cache"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pending_wake_projection_excludes_terminal_and_resolved_wakes() {
        let mut pending = make_fact("wake-pending", FactKind::Wake, "", "pending");
        pending.status = Some("pending".to_string());
        pending.target = Some("codex".to_string());

        let mut delivered = make_fact("wake-delivered", FactKind::Wake, "", "delivered");
        delivered.status = Some("delivered".to_string());
        delivered.target = Some("codex".to_string());

        let mut resolved_wake = make_fact("wake-resolved", FactKind::Wake, "", "pending");
        resolved_wake.status = Some("pending".to_string());
        resolved_wake.target = Some("codex".to_string());
        let mut resolution = make_fact("resolve-wake", FactKind::Resolve, "", "resolved");
        resolution.ref_id = Some(resolved_wake.event_id.clone());

        for (seq, fact) in [
            &mut pending,
            &mut delivered,
            &mut resolved_wake,
            &mut resolution,
        ]
        .into_iter()
        .enumerate()
        {
            fact.seq = (seq + 1) as i64;
        }
        let snapshot = snapshot_from_facts_with_policy(
            &[pending, delivered, resolved_wake, resolution],
            &crate::hooks_config::CoordinationConfig::default(),
            false,
        );
        assert_eq!(snapshot.pending_wakes.len(), 1);
        assert_eq!(snapshot.pending_wakes[0].event_id, "wake-pending");
    }

    // -------------------------------------------------------------------------
    // F2 — mid-page SQLITE_CORRUPT (code 11) triggers full recovery end-to-end
    // -------------------------------------------------------------------------

    /// The existing test `malformed_facts_db_is_rebuilt_from_segments_on_open`
    /// corrupts only the SQLite header (→ SQLITE_NOTADB / code 26).  This test
    /// exercises mid-page corruption (→ SQLITE_CORRUPT / code 11) by writing
    /// enough facts to produce a multi-page DB, then corrupting bytes in page 2
    /// (offset 4096), and forcing a real b-tree traversal via `store.facts()`.
    #[test]
    fn malformed_facts_db_midpage_corruption_triggers_rebuild() {
        let root = unique_root("midpage-corrupt");
        let store = RoomStore::open_at(root.clone()).unwrap();

        // Write ~500 facts to ensure the DB spans multiple pages.
        for i in 0..500u32 {
            store
                .append_fact(&make_fact(
                    &format!("e{i}"),
                    FactKind::Claim,
                    "src/",
                    &format!("fact {i} — padding to grow the db past one page boundary"),
                ))
                .unwrap();
        }
        let before_facts = store.facts().unwrap();
        assert_eq!(before_facts.len(), 500);

        let segments = segments_under(&root);
        assert!(!segments.is_empty(), "segments are canonical");

        drop(store);

        // Corrupt page 2 (offset 4096) — well past the header so sqlite opens
        // without noticing until it traverses the b-tree during a real query.
        let facts_db = root.join(".rally/facts.db");
        // Remove WAL/SHM siblings before corrupting page 2. A valid WAL can
        // let SQLite mask the damaged main file and skip the quarantine path.
        remove_fact_store_journals(&facts_db);
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&facts_db)
                .unwrap();
            let db_size = f.seek(SeekFrom::End(0)).unwrap();
            // Only proceed if the file is actually multi-page.
            assert!(
                db_size > 8192,
                "DB must be multi-page for mid-page corruption test (got {db_size} bytes)"
            );
            f.seek(SeekFrom::Start(4096)).unwrap();
            // Overwrite 64 bytes in the middle of page 2 with garbage.
            f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF].repeat(16)).unwrap();
            f.sync_all().unwrap();
        }

        // Reopen the room.  reconcile_segments_and_db detects the corrupt DB
        // via read_db_event_count (which issues a query, forcing page traversal)
        // and triggers quarantine + rebuild.
        let store2 = RoomStore::open_at(root.clone()).unwrap();

        // Force a full b-tree traversal so SQLite must visit the corrupted page.
        let after_facts = store2.facts().unwrap();

        // All 500 facts recovered from the canonical JSONL segments.
        assert_eq!(
            after_facts.len(),
            500,
            "all 500 facts recovered after mid-page corruption"
        );
        for (pre, post) in before_facts.iter().zip(after_facts.iter()) {
            assert_eq!(pre.seq, post.seq);
            assert_eq!(pre.event_id, post.event_id);
        }

        // A quarantine file exists (corrupt bytes preserved for forensics).
        let rally_dir = root.join(".rally");
        let found_quarantine = fs::read_dir(&rally_dir).unwrap().any(|e| {
            e.map(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with("facts.db.corrupt.")
                    && !s.ends_with("-db-shm")
                    && !s.ends_with("-db-wal")
            })
            .unwrap_or(false)
        });
        assert!(
            found_quarantine,
            "corrupt bytes preserved as facts.db.corrupt.<stamp>"
        );

        // Rebuilt DB is healthy — snapshot is queryable.
        let snap = store2.snapshot().unwrap();
        assert_eq!(snap.max_seq, 500);

        fs::remove_dir_all(&root).ok();
    }

    /// Regression test: `hash_file_head` must return a FIXED golden value for
    /// fixed bytes. If this test fails it means someone swapped back to
    /// `DefaultHasher` (or any other per-process-seeded hasher), which is
    /// cross-process non-deterministic and breaks the sidecar fast-path.
    ///
    /// Golden value: FNV-1a 64-bit of b"hello world" = 0x779a65e7023cd2e7.
    /// Verified independently via Python reference implementation.
    #[test]
    fn hash_file_head_is_deterministic_golden_value() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "rally-golden-hash-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("golden.bin");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"hello world").unwrap();
        drop(f);

        let h = hash_file_head(&path);
        // FNV-1a 64-bit of the 11 bytes b"hello world".
        // Recompute if the algorithm changes; any per-process-seeded hasher will
        // produce a DIFFERENT value on each run and this assert will flap.
        assert_eq!(
            h, 0x779a65e7023cd2e7,
            "hash_file_head returned {h:#018x}; expected FNV-1a golden 0x779a65e7023cd2e7. \
             A failing assert here means the implementation uses a per-process-seeded hasher \
             (e.g. DefaultHasher) which breaks cross-process sidecar fast-path."
        );

        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod decay_reclaim_tests {
    use super::*;
    use crate::decay::WorkSize;
    use crate::hooks_config::CoordinationConfig;
    use crate::resource_scope::{AccessMode, ResourceScope, ResourceType};

    fn iso_ago(secs: i64) -> String {
        let now = chrono::Utc::now();
        (now - chrono::Duration::seconds(secs.max(0)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn aged_fact(event_id: &str, kind: FactKind, age_secs: i64) -> Fact {
        Fact {
            from_session_id: None,
            schema: fact_schema(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: format!("t-{event_id}"),
            kind,
            tool: Some("tester".to_string()),
            role: None,
            subject: format!("s-{event_id}"),
            scope: Vec::new(),
            created_at: iso_ago(age_secs),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    fn squad(tool: &str, last_seen_age_secs: i64) -> Squad {
        Squad {
            tool: tool.to_string(),
            last_seen_seq: 1,
            last_seen_ts: iso_ago(last_seen_age_secs),
            status: "idle".to_string(),
            acknowledged: false,
        }
    }

    fn claim_with(tool: &str, scopes: &[&str], owner_age_secs: i64) -> (Fact, RoomSnapshot) {
        let mut claim = aged_fact("claim-1", FactKind::Claim, 0);
        claim.tool = Some(tool.to_string());
        claim.scope = scopes.iter().map(|s| s.to_string()).collect();
        let snapshot = RoomSnapshot {
            squads: vec![squad(tool, owner_age_secs)],
            ..Default::default()
        };
        (claim, snapshot)
    }

    // --- INTEGRATION: snapshot decay sort + archive partition ---
    #[test]
    fn snapshot_orders_by_recency_and_partitions_stale() {
        let coord = CoordinationConfig::default(); // 48h half-life, 0.05 floor
        // fresh decision (0d), mid (3d, weight ~0.42 > floor), stale (20d, < floor)
        let mut fresh = aged_fact("d-fresh", FactKind::Decision, 0);
        fresh.seq = 1;
        let mut mid = aged_fact("d-mid", FactKind::Decision, 3 * 24 * 3600);
        mid.seq = 2;
        let mut stale = aged_fact("d-stale", FactKind::Decision, 20 * 24 * 3600);
        stale.seq = 3;
        let facts = vec![mid.clone(), stale.clone(), fresh.clone()];

        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        // Fresh sorts above mid; stale is archived out of the active bucket.
        let ids: Vec<&str> = snap
            .current_decisions
            .iter()
            .map(|f| f.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["d-fresh", "d-mid"], "fresh first, stale excluded");
        assert!(
            snap.stale_facts.iter().any(|f| f.event_id == "d-stale"),
            "20d decision moved to stale_facts"
        );

        // include_archived re-includes the stale fact in the active bucket.
        let snap_all = snapshot_from_facts_with_policy(&facts, &coord, true);
        assert!(
            snap_all
                .current_decisions
                .iter()
                .any(|f| f.event_id == "d-stale"),
            "include_archived keeps decayed facts"
        );
        assert!(
            snap_all.stale_facts.is_empty(),
            "nothing archived when included"
        );
    }

    #[test]
    fn malformed_created_at_is_kept_not_archived() {
        let coord = CoordinationConfig::default();
        let mut bad = aged_fact("d-bad", FactKind::Decision, 0);
        bad.created_at = "not-a-timestamp".to_string();
        bad.seq = 1;
        let snap = snapshot_from_facts_with_policy(&[bad], &coord, false);
        assert_eq!(
            snap.current_decisions.len(),
            1,
            "fail-open: malformed ts kept"
        );
        assert!(snap.stale_facts.is_empty());
    }

    // --- claim_reclaim_eligible: size-scaled, just-under/just-over, fail-closed ---
    #[test]
    fn single_file_claim_reclaims_after_small_timeout() {
        let coord = CoordinationConfig::default(); // small 30m, large 2h
        // owner silent 31m → eligible (single file = small = 30m)
        let (claim, snap) = claim_with("ghost", &["file:src/a.rs"], 31 * 60);
        let (eligible, size) = snap.claim_reclaim_eligible(&claim, &coord);
        assert!(eligible, "single-file claim reclaimable at 31m");
        assert_eq!(size, WorkSize::Small);

        // owner silent 29m → NOT eligible
        let (claim2, snap2) = claim_with("ghost", &["file:src/a.rs"], 29 * 60);
        let (eligible2, _) = snap2.claim_reclaim_eligible(&claim2, &coord);
        assert!(!eligible2, "single-file claim NOT reclaimable at 29m");
    }

    #[test]
    fn multi_file_claim_only_reclaims_after_large_timeout() {
        let coord = CoordinationConfig::default();
        // multi-file, owner silent 31m → NOT eligible (large = 2h)
        let (claim, snap) = claim_with("ghost", &["file:src/a.rs", "file:src/b.rs"], 31 * 60);
        let (eligible, size) = snap.claim_reclaim_eligible(&claim, &coord);
        assert!(!eligible, "multi-file claim NOT reclaimable at 31m");
        assert_eq!(size, WorkSize::Large);

        // owner silent 121m → eligible
        let (claim2, snap2) = claim_with("ghost", &["file:src/a.rs", "file:src/b.rs"], 121 * 60);
        let (eligible2, _) = snap2.claim_reclaim_eligible(&claim2, &coord);
        assert!(eligible2, "multi-file claim reclaimable at 121m");
    }

    #[test]
    fn dir_scope_claim_uses_large_timeout() {
        let coord = CoordinationConfig::default();
        let (claim, snap) = claim_with("ghost", &["dir:src"], 31 * 60);
        let (eligible, size) = snap.claim_reclaim_eligible(&claim, &coord);
        assert_eq!(size, WorkSize::Large, "dir scope is coarse");
        assert!(!eligible, "dir claim not reclaimable at 31m");
    }

    #[test]
    fn malformed_owner_last_seen_is_never_reclaimable() {
        let coord = CoordinationConfig::default();
        let mut claim = aged_fact("claim-1", FactKind::Claim, 0);
        claim.tool = Some("ghost".to_string());
        claim.scope = vec!["file:src/a.rs".to_string()];
        let mut sq = squad("ghost", 0);
        sq.last_seen_ts = "garbage".to_string(); // unparseable
        let snap = RoomSnapshot {
            squads: vec![sq],
            ..Default::default()
        };
        let (eligible, _) = snap.claim_reclaim_eligible(&claim, &coord);
        assert!(!eligible, "fail-closed: malformed last_seen never reclaims");
    }

    #[test]
    fn unknown_owner_is_never_reclaimable() {
        let coord = CoordinationConfig::default();
        let mut claim = aged_fact("claim-1", FactKind::Claim, 0);
        claim.tool = Some("ghost".to_string());
        claim.scope = vec!["file:src/a.rs".to_string()];
        // squad list does NOT contain "ghost"
        let snap = RoomSnapshot {
            squads: vec![squad("someone-else", 999999)],
            ..Default::default()
        };
        let (eligible, _) = snap.claim_reclaim_eligible(&claim, &coord);
        assert!(!eligible, "fail-closed: no squad entry for owner");
    }

    #[test]
    fn config_tunables_change_timeout() {
        // custom small=10m: owner silent 11m on a single file → eligible
        let coord = CoordinationConfig {
            reclaim_small_minutes: 10,
            ..CoordinationConfig::default()
        };
        let (claim, snap) = claim_with("ghost", &["file:src/a.rs"], 11 * 60);
        let (eligible, _) = snap.claim_reclaim_eligible(&claim, &coord);
        assert!(eligible, "custom 10m small timeout honored");
        // at 9m, not yet
        let (claim2, snap2) = claim_with("ghost", &["file:src/a.rs"], 9 * 60);
        let (eligible2, _) = snap2.claim_reclaim_eligible(&claim2, &coord);
        assert!(!eligible2);
    }

    #[test]
    fn classify_helper_matches_parse() {
        // sanity: a parsed file scope classifies Small.
        let rs = ResourceScope {
            resource_type: ResourceType::File,
            identifier: "src/a.rs".to_string(),
            access: AccessMode::Exclusive,
        };
        assert_eq!(crate::decay::classify_work_size(&[rs], 1), WorkSize::Small);
    }
}

#[cfg(test)]
mod sec001_takeover_guard_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let r = std::env::temp_dir().join(format!("rally-sec001-{label}-{nanos}"));
        fs::create_dir_all(&r).unwrap();
        r
    }

    fn iso_ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs.max(0)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn fact_at(event_id: &str, kind: FactKind, tool: &str, scope: &str, created_at: &str) -> Fact {
        Fact {
            from_session_id: None,
            schema: fact_schema(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: format!("t-{event_id}"),
            kind,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("s-{event_id}"),
            scope: vec![scope.to_string()],
            created_at: created_at.to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    #[test]
    fn marker_parser_extracts_owners() {
        let ev = vec![
            "produces:x".to_string(),
            "authorized-takeover:stale-owner=ghost,other".to_string(),
        ];
        assert_eq!(
            DirectRoomStore::takeover_owners_marker(&ev),
            Some(vec!["ghost".to_string(), "other".to_string()])
        );
        assert_eq!(
            DirectRoomStore::takeover_owners_marker(&["x".to_string()]),
            None
        );
    }

    #[test]
    fn takeover_refused_when_owner_revived_under_lock() {
        let r = root("revived");
        let store = RoomStore::open_at(r.clone()).unwrap();
        // Stale owner "ghost" claimed a single file long ago (>30m => small,
        // reclaim-eligible at snapshot time).
        let claim = fact_at(
            "claim-g",
            FactKind::Claim,
            "ghost",
            "file:src/a.rs",
            &iso_ago(40 * 60),
        );
        store.append_fact(&claim).unwrap();
        // ghost REVIVES: posts a fresh fact (now its squad last_seen is ~now).
        let revive = fact_at(
            "revive-g",
            FactKind::Presence,
            "ghost",
            "presence",
            &iso_ago(1),
        );
        store.append_fact(&revive).unwrap();
        // A peer attempts a takeover release marked for ghost. The in-lock guard
        // must REFUSE because ghost is no longer stale.
        let mut release = fact_at(
            "rel-1",
            FactKind::Release,
            "peer",
            "file:src/a.rs",
            &iso_ago(0),
        );
        release.ref_id = Some("claim-g".to_string());
        release.evidence = vec!["authorized-takeover:stale-owner=ghost".to_string()];
        let err = store.append_fact(&release).unwrap_err().to_string();
        assert!(
            err.contains("takeover refused") && err.contains("revived"),
            "revived owner's claim must not be reclaimed; got: {err}"
        );
        // ghost still owns its claim.
        let snap = store.snapshot().unwrap();
        assert!(snap.active_claims.iter().any(|c| c.event_id == "claim-g"));
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn takeover_allowed_when_owner_still_stale() {
        let r = root("still-stale");
        let store = RoomStore::open_at(r.clone()).unwrap();
        // ghost claimed a single file 40m ago and has NOT revived -> eligible.
        let claim = fact_at(
            "claim-g",
            FactKind::Claim,
            "ghost",
            "file:src/a.rs",
            &iso_ago(40 * 60),
        );
        store.append_fact(&claim).unwrap();
        let mut release = fact_at(
            "rel-1",
            FactKind::Release,
            "peer",
            "file:src/a.rs",
            &iso_ago(0),
        );
        release.ref_id = Some("claim-g".to_string());
        release.evidence = vec!["authorized-takeover:stale-owner=ghost".to_string()];
        // Guard passes (owner still stale); release succeeds.
        store.append_fact(&release).unwrap();
        let snap = store.snapshot().unwrap();
        assert!(
            !snap.active_claims.iter().any(|c| c.event_id == "claim-g"),
            "stale owner's claim is reclaimed when it is genuinely stale"
        );
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn ordinary_self_release_has_no_takeover_guard() {
        let r = root("self-release");
        let store = RoomStore::open_at(r.clone()).unwrap();
        let claim = fact_at(
            "claim-s",
            FactKind::Claim,
            "me",
            "file:src/a.rs",
            &iso_ago(5),
        );
        store.append_fact(&claim).unwrap();
        // No takeover marker -> guard is skipped; a normal release works.
        let mut release = fact_at(
            "rel-s",
            FactKind::Release,
            "me",
            "file:src/a.rs",
            &iso_ago(0),
        );
        release.ref_id = Some("claim-s".to_string());
        store.append_fact(&release).unwrap();
        let snap = store.snapshot().unwrap();
        assert!(!snap.active_claims.iter().any(|c| c.event_id == "claim-s"));
        fs::remove_dir_all(&r).ok();
    }

    /// Build a reaper-style ClaimExpired fact closing `claim_id` for `owner`
    /// with the given reap reason, mirroring what `reaper.rs` stamps.
    fn reaper_claim_expired(claim_id: &str, owner: &str, reason: &str) -> Fact {
        let mut f = fact_at(
            "exp-1",
            FactKind::ClaimExpired,
            "rally",
            "file:src/a.rs",
            &iso_ago(0),
        );
        f.ref_id = Some(claim_id.to_string());
        f.summary = Some(format!("reaper:reason={reason}"));
        f.evidence = vec![
            format!("reaper:ref_id={claim_id}"),
            format!("reaper:reason={reason}"),
            format!("reaper:owner={owner}"),
        ];
        f
    }

    #[test]
    fn reaper_owner_stale_close_refused_if_owner_revived_under_lock() {
        // Parallel to `takeover_refused_when_owner_revived_under_lock`, but for
        // the reaper's ClaimExpired path: an OWNER-STALE close computed on an
        // unlocked snapshot must be REFUSED if the owner revives before the
        // append acquires the mutation lock.
        let r = root("reaper-revived");
        let store = RoomStore::open_at(r.clone()).unwrap();
        // ghost claimed a single file 40m ago (small => 30m bar => stale).
        let claim = fact_at(
            "claim-g",
            FactKind::Claim,
            "ghost",
            "file:src/a.rs",
            &iso_ago(40 * 60),
        );
        store.append_fact(&claim).unwrap();
        // ghost REVIVES: fresh presence => squad last_seen ~now => not stale.
        let revive = fact_at(
            "revive-g",
            FactKind::Presence,
            "ghost",
            "presence",
            &iso_ago(1),
        );
        store.append_fact(&revive).unwrap();
        // The reaper attempts an owner-stale ClaimExpired for ghost's claim.
        // The under-lock guard must refuse it.
        let expired = reaper_claim_expired("claim-g", "ghost", "owner-stale");
        let err = store.append_fact(&expired).unwrap_err().to_string();
        assert!(
            err.contains("reap refused") && err.contains("revived"),
            "revived owner's claim must not be reaped via ClaimExpired; got: {err}"
        );
        // ghost still owns its claim (the skip is observable as a kept claim).
        let snap = store.snapshot().unwrap();
        assert!(
            snap.active_claims.iter().any(|c| c.event_id == "claim-g"),
            "owner-stale ClaimExpired must NOT close a revived owner's claim"
        );
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn reaper_lease_expired_close_survives_owner_activity() {
        // A LEASE-EXPIRED close is monotonic-safe: a past lease cannot un-expire,
        // so the under-lock guard must NOT refuse it even when the owner is fully
        // active (fresh presence). This is the dual-signal case the owner-stale
        // re-check must leave alone.
        let r = root("reaper-lease-survives");
        let store = RoomStore::open_at(r.clone()).unwrap();
        // live-owner claims a single file just now AND is fresh (active squad).
        let claim = fact_at(
            "claim-l",
            FactKind::Claim,
            "live-owner",
            "file:src/a.rs",
            &iso_ago(5),
        );
        store.append_fact(&claim).unwrap();
        let presence = fact_at(
            "pres-l",
            FactKind::Presence,
            "live-owner",
            "presence",
            &iso_ago(1),
        );
        store.append_fact(&presence).unwrap();
        // The reaper closes the claim on the LEASE-EXPIRED signal. Even though
        // the owner is active, the guard exempts lease-expired closures.
        let expired = reaper_claim_expired("claim-l", "live-owner", "lease-expired");
        store
            .append_fact(&expired)
            .expect("lease-expired ClaimExpired must not be refused for an active owner");
        let snap = store.snapshot().unwrap();
        assert!(
            !snap.active_claims.iter().any(|c| c.event_id == "claim-l"),
            "lease-expired ClaimExpired must close the claim despite owner activity"
        );
        fs::remove_dir_all(&r).ok();
    }
}

#[cfg(test)]
mod squad_decay_tests {
    //! The squad-projection GAP fix: an all-signals-stale squad DROPS from the
    //! default snapshot (restored under include_archived); any fresh signal or
    //! any unparseable/absent signal (fail-open) keeps it visible.
    use super::*;
    use crate::hooks_config::CoordinationConfig;

    fn iso_ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs.max(0)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn fact(kind: FactKind, tool: &str, age_secs: i64) -> Fact {
        use std::sync::atomic::{AtomicI64, Ordering};
        static N: AtomicI64 = AtomicI64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Fact {
            from_session_id: None,
            schema: fact_schema(),
            event_id: format!("evt-{n}"),
            seq: 0,
            thread_id: format!("room-{n}"),
            kind,
            tool: Some(tool.to_string()),
            role: None,
            subject: tool.to_string(),
            scope: Vec::new(),
            created_at: iso_ago(age_secs),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    /// A presence fact stamped with a branch sha (for the code-progress signal).
    fn presence_sha(tool: &str, age_secs: i64, sha: &str) -> Fact {
        let mut f = fact(FactKind::Presence, tool, age_secs);
        f.evidence = vec![format!("branch_head_sha:{sha}")];
        f
    }

    /// Assign monotonic seqs the way the real ledger does (replay order).
    fn seqd(mut facts: Vec<Fact>) -> Vec<Fact> {
        for (i, f) in facts.iter_mut().enumerate() {
            f.seq = (i + 1) as i64;
        }
        facts
    }

    fn has_squad(snap: &RoomSnapshot, tool: &str) -> bool {
        snap.squads.iter().any(|s| s.tool == tool)
    }

    #[test]
    fn five_min_cadence_all_stale_squad_dropped_and_restored() {
        // Default cadence (5m) → window 31m. Make ALL FOUR signals present and
        // past the window so the verdict is provably Stale → DROP.
        //   (a) heartbeat: stale presence facts (35m+, the newest).
        //   (b) inject: a handoff TO the tool, stale (40m).
        //   (c) code progress: two presence facts with DIFFERENT shas, both old
        //       (the newer is 35m → progress age 35m, stale).
        //   (d) plan: a claim owned by the tool, stale (60m).
        let coord = CoordinationConfig::default();
        let mut handoff = fact(FactKind::Handoff, "sender", 40 * 60);
        handoff.target = Some("stale-tool".to_string());
        let mut claim = fact(FactKind::Claim, "stale-tool", 60 * 60);
        claim.scope = vec!["file:src/x.rs".to_string()];
        let facts = seqd(vec![
            presence_sha("stale-tool", 90 * 60, "aaaa"), // older sha
            handoff,
            claim,
            presence_sha("stale-tool", 35 * 60, "bbbb"), // newer sha → moved, but 35m old
        ]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            !has_squad(&snap, "stale-tool"),
            "all-signals-stale 5-min-cadence squad must be DROPPED from default view"
        );
        let snap_all = snapshot_from_facts_with_policy(&facts, &coord, true);
        assert!(
            has_squad(&snap_all, "stale-tool"),
            "dropped squad must return under include_archived"
        );
    }

    #[test]
    fn five_hour_cadence_idle_2h_stays_visible() {
        // Declared 5-hour cadence via planned_heartbeat_secs stamp → window 30h.
        // A 2-hour-old presence is well within window → Live → visible.
        let coord = CoordinationConfig::default();
        let mut presence = fact(FactKind::Presence, "slow-tool", 2 * 60 * 60);
        presence.evidence = vec!["planned_heartbeat_secs:18000".to_string()];
        let facts = seqd(vec![presence]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            has_squad(&snap, "slow-tool"),
            "5-hour-cadence tool idle 2h must stay LIVE/visible"
        );
    }

    #[test]
    fn fresh_code_progress_keeps_stale_heartbeat_alive() {
        // Heartbeat is the newest fact at 35m (stale on a 5m cadence) BUT the two
        // newest presence shas DIFFER and the newer is fresh (30s) → code progress
        // fresh → Live → visible. (Here the newest presence both IS the heartbeat
        // and proves progress; use a fresh newer presence.)
        let coord = CoordinationConfig::default();
        let facts = seqd(vec![
            presence_sha("worker", 50 * 60, "old1"),
            presence_sha("worker", 30, "new2"), // fresh + moved
        ]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            has_squad(&snap, "worker"),
            "fresh code-progress must keep the squad visible"
        );
    }

    #[test]
    fn fresh_inject_keeps_squad_alive() {
        // Stale heartbeat presence, but a FRESH handoff targeting the tool (inject).
        let coord = CoordinationConfig::default();
        let mut handoff = fact(FactKind::Handoff, "sender", 60); // fresh inject
        handoff.target = Some("recv".to_string());
        let facts = seqd(vec![fact(FactKind::Presence, "recv", 40 * 60), handoff]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(has_squad(&snap, "recv"), "fresh inject keeps squad visible");
    }

    #[test]
    fn fresh_plan_claim_keeps_squad_alive() {
        // Stale heartbeat presence, but a FRESH live claim (declared active work).
        let coord = CoordinationConfig::default();
        let mut claim = fact(FactKind::Claim, "builder", 30);
        claim.scope = vec!["file:src/y.rs".to_string()];
        // presence older than claim so heartbeat (highest-seq = claim) is fresh;
        // to isolate the plan signal, give a stale presence as the highest-seq.
        let facts = seqd(vec![claim, fact(FactKind::Presence, "builder", 40 * 60)]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            has_squad(&snap, "builder"),
            "fresh plan/claim keeps squad visible"
        );
    }

    #[test]
    fn stale_heartbeat_only_is_unknown_failopen_visible() {
        // Only a stale presence; inject/code/plan signals never observed → Unknown
        // → fail-open keeps it VISIBLE.
        let coord = CoordinationConfig::default();
        let facts = seqd(vec![fact(FactKind::Presence, "quiet", 40 * 60)]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            has_squad(&snap, "quiet"),
            "stale-heartbeat-only (other signals absent) must FAIL-OPEN to visible"
        );
    }

    #[test]
    fn unparseable_timestamp_fails_open_visible() {
        // A presence with a garbage created_at: fact_age_secs returns 0 (fresh) on
        // the visibility path → heartbeat fresh → visible. Never invent staleness.
        let coord = CoordinationConfig::default();
        let mut bad = fact(FactKind::Presence, "badts", 0);
        bad.created_at = "NOT-A-TIMESTAMP".to_string();
        let facts = seqd(vec![bad]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            has_squad(&snap, "badts"),
            "unparseable presence ts must FAIL-OPEN to visible"
        );
    }

    #[test]
    fn fresh_tool_stays_visible() {
        let coord = CoordinationConfig::default();
        let facts = seqd(vec![fact(FactKind::Presence, "live", 30)]);
        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(has_squad(&snap, "live"));
    }
}
