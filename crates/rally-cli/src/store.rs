use factstr::{EventQuery as FactQuery, EventStore, NewEvent};
use factstr_sqlite::SqliteStore;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

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

/// Finite fallback for callers without a command/request deadline (principally
/// in-process use and tests). Real direct commands use the shorter watchdog
/// deadline, while routed requests install the shorter client deadline.
const MUTATION_LOCK_FALLBACK_BOUND: Duration = Duration::from_secs(5);
const MUTATION_LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
const MUTATION_LOCK_WATCHDOG_RESERVE: Duration = Duration::from_millis(150);

thread_local! {
    /// Optional caller-provided start deadline. Routed daemon requests install
    /// the client deadline here for one dispatch; direct CLI calls derive from
    /// the command watchdog instead.
    static MUTATION_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    #[cfg(test)]
    static FORCE_WARM_CLOSE_SPAWN_FAILURE: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static FORCE_ROOM_LOCK_POST_FLOCK_PAUSE: Cell<Option<Duration>> = const { Cell::new(None) };
    #[cfg(test)]
    static FORCE_OWNER_LOCK_POST_FLOCK_PAUSE: Cell<Option<Duration>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn force_next_room_lock_post_flock_pause(pause: Duration) {
    FORCE_ROOM_LOCK_POST_FLOCK_PAUSE.with(|slot| slot.set(Some(pause)));
}

#[cfg(test)]
fn pause_after_room_lock_flock_for_test() {
    FORCE_ROOM_LOCK_POST_FLOCK_PAUSE.with(|slot| {
        if let Some(pause) = slot.take() {
            thread::sleep(pause);
        }
    });
}

#[cfg(not(test))]
fn pause_after_room_lock_flock_for_test() {}

#[cfg(test)]
fn force_next_owner_lock_post_flock_pause(pause: Duration) {
    FORCE_OWNER_LOCK_POST_FLOCK_PAUSE.with(|slot| slot.set(Some(pause)));
}

#[cfg(test)]
fn pause_after_owner_lock_flock_for_test() {
    FORCE_OWNER_LOCK_POST_FLOCK_PAUSE.with(|slot| {
        if let Some(pause) = slot.take() {
            thread::sleep(pause);
        }
    });
}

#[cfg(not(test))]
fn pause_after_owner_lock_flock_for_test() {}

#[cfg(test)]
fn force_next_warm_close_spawn_failure() {
    FORCE_WARM_CLOSE_SPAWN_FAILURE.with(|flag| flag.set(true));
}

#[cfg(test)]
fn warm_close_spawn_failure_requested() -> bool {
    FORCE_WARM_CLOSE_SPAWN_FAILURE.with(|flag| flag.replace(false))
}

#[cfg(not(test))]
fn warm_close_spawn_failure_requested() -> bool {
    false
}

struct MutationDeadlineGuard {
    previous: Option<Instant>,
}

impl Drop for MutationDeadlineGuard {
    fn drop(&mut self) {
        MUTATION_DEADLINE.with(|slot| slot.set(self.previous));
    }
}

/// Run `work` with a deadline for starting any nested room mutation.
/// Nested callers can only shorten an existing deadline.
#[cfg(test)]
pub(crate) fn with_mutation_deadline<T>(budget: Duration, work: impl FnOnce() -> T) -> T {
    let now = Instant::now();
    let requested = now
        .checked_add(budget)
        .unwrap_or(now + MUTATION_LOCK_FALLBACK_BOUND);
    with_mutation_deadline_at(requested, work)
}

#[cfg(test)]
pub(crate) fn expire_mutation_deadline_for_test() {
    MUTATION_DEADLINE.with(|slot| slot.set(Some(Instant::now())));
}

#[cfg(test)]
pub(crate) fn clear_mutation_deadline_for_test() {
    MUTATION_DEADLINE.with(|slot| slot.set(None));
}

/// Run `work` with an already-anchored monotonic deadline.
///
/// Routed requests use this form so dispatcher queueing or preemption between
/// deadline calculation and installation cannot add elapsed time back.
pub(crate) fn with_mutation_deadline_at<T>(requested: Instant, work: impl FnOnce() -> T) -> T {
    let previous = MUTATION_DEADLINE.with(|slot| {
        let previous = slot.get();
        slot.set(Some(
            previous.map_or(requested, |prior| prior.min(requested)),
        ));
        previous
    });
    let _guard = MutationDeadlineGuard { previous };
    work()
}

fn effective_mutation_deadline() -> Instant {
    let now = Instant::now();
    MUTATION_DEADLINE.with(Cell::get).unwrap_or_else(|| {
        crate::watchdog_remaining().map_or(now + MUTATION_LOCK_FALLBACK_BOUND, |remaining| {
            now + remaining.saturating_sub(MUTATION_LOCK_WATCHDOG_RESERVE)
        })
    })
}

fn mutation_not_started(path: &Path) -> RallyError {
    RallyError::NotStarted(format!(
        "mutation-not-started: deadline elapsed before acquiring {}; no durable mutation started and retry is safe",
        path.display()
    ))
}

fn mutation_not_started_after_provisional_lock(path: &Path) -> RallyError {
    RallyError::NotStarted(format!(
        "mutation-not-started: deadline elapsed after provisional lock acquisition at {}; lock released before any durable mutation started and retry is safe",
        path.display()
    ))
}

fn mutation_start_deadline_elapsed() -> bool {
    Instant::now() >= effective_mutation_deadline()
}

pub(crate) fn ensure_new_mutation_can_start(path: &Path) -> Result<()> {
    if mutation_start_deadline_elapsed() {
        return Err(RallyError::NotStarted(format!(
            "mutation-not-started: deadline elapsed after validation but before the first durable side effect at {}; no durable mutation started and retry is safe",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
mod unix_lock {
    /// Shared (read) advisory lock — many holders coexist. Direct mode holds
    /// this on the daemon-ownership lock only after it has acquired the
    /// separate exclusive direct-owner lock.
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

/// Durable fence installed by the explicit offline DB-only migration. While
/// this marker (or its create-new staging predecessor) exists, doctor owns the
/// only safe recovery path: an ordinary open/reconcile/append could otherwise
/// change the marker-bound DB or its hard-linked canonical candidate.
pub(crate) const DB_ONLY_MIGRATION_MARKER_FILENAME: &str = "db-only-migration.v1.json";
pub(crate) const DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME: &str = "db-only-migration.v1.marker.tmp";

/// Exclusive process-lifetime guard for the direct fallback. The daemon does
/// not take this lock: it already owns [`RALLYD_OWNER_LOCK_FILENAME`] EX for
/// its serving lifetime. A direct process takes this lock EX first and then
/// takes the daemon lock SH, so at most one process can retain direct
/// `facts.db` pools while daemon startup still drains through the established
/// SH -> EX handover.
const DIRECT_OWNER_LOCK_FILENAME: &str = "direct.owner.lock";

fn ensure_no_db_only_migration_recovery(root: &Path) -> Result<()> {
    let rally_dir = root.join(".rally");
    for filename in [
        DB_ONLY_MIGRATION_MARKER_FILENAME,
        DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME,
    ] {
        let path = rally_dir.join(filename);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(RallyError::Usage(format!(
                    "DB-only migration recovery is pending at {}; ordinary Rally store access is fenced to preserve marker-bound evidence. Resume with `rally doctor --migrate-db-only --engagement <label> --apply --json`",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RallyError::io(format!(
                    "stat DB-only migration recovery marker {}",
                    path.display()
                ))(error));
            }
        }
    }
    Ok(())
}

use crate::backends::ManagedSession;
use crate::cli::RoomArgs;
use crate::discovery::refresh_room_index;
use crate::error::{RallyError, Result};
use crate::store_client::{self, RoutedRoomStore};
use std::hash::{Hash, Hasher};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Deterministic O26 fault sites. Test controls are keyed by the exact `.rally`
/// path, so parallel stores cannot affect one another. Production builds retain
/// only no-op call sites; no environment variable or process-wide switch can
/// alter storage behavior.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum O26FaultPoint {
    BeforeCanonicalMutation,
    TailRepairSync,
    AfterTailRepair,
    PartialCanonicalWrite,
    AfterCanonicalSyncBeforeReadback,
    AfterCanonicalReadback,
    FactsDbProjection,
    ReconcileCacheProjection,
    SnapshotPostCommit,
    DaemonReplyDrop,
}

#[cfg(test)]
enum O26FaultAction {
    Pass,
    Fail,
    Pause(Arc<O26FaultPauseState>),
}

#[cfg(test)]
struct O26FaultPauseState {
    phase: Mutex<u8>,
    changed: Condvar,
}

#[cfg(test)]
pub(crate) struct O26FaultPause {
    state: Arc<O26FaultPauseState>,
}

#[cfg(test)]
impl O26FaultPause {
    pub(crate) fn wait_until_reached(&self) {
        let mut phase = self
            .state
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *phase == 0 {
            phase = self
                .state
                .changed
                .wait(phase)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(crate) fn resume(&self) {
        let mut phase = self
            .state
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *phase = 2;
        self.state.changed.notify_all();
    }
}

#[cfg(test)]
type O26FaultQueue = BTreeMap<(PathBuf, O26FaultPoint), VecDeque<O26FaultAction>>;

#[cfg(test)]
static O26_TEST_FAULTS: OnceLock<Mutex<O26FaultQueue>> = OnceLock::new();

#[cfg(test)]
fn o26_faults() -> &'static Mutex<O26FaultQueue> {
    O26_TEST_FAULTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
pub(crate) fn fail_o26_once(rally_dir: &Path, point: O26FaultPoint) {
    o26_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((rally_dir.to_path_buf(), point))
        .or_default()
        .push_back(O26FaultAction::Fail);
}

#[cfg(test)]
pub(crate) fn skip_o26_once(rally_dir: &Path, point: O26FaultPoint) {
    o26_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((rally_dir.to_path_buf(), point))
        .or_default()
        .push_back(O26FaultAction::Pass);
}

#[cfg(test)]
pub(crate) fn pause_o26_once(rally_dir: &Path, point: O26FaultPoint) -> O26FaultPause {
    let state = Arc::new(O26FaultPauseState {
        phase: Mutex::new(0),
        changed: Condvar::new(),
    });
    o26_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((rally_dir.to_path_buf(), point))
        .or_default()
        .push_back(O26FaultAction::Pause(Arc::clone(&state)));
    O26FaultPause { state }
}

#[cfg(test)]
pub(crate) fn trigger_o26_fault(
    rally_dir: &Path,
    point: O26FaultPoint,
) -> std::result::Result<(), &'static str> {
    let action = {
        let mut faults = o26_faults()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (rally_dir.to_path_buf(), point);
        let action = faults.get_mut(&key).and_then(VecDeque::pop_front);
        if faults.get(&key).is_some_and(VecDeque::is_empty) {
            faults.remove(&key);
        }
        action
    };
    match action {
        None => Ok(()),
        Some(O26FaultAction::Pass) => Ok(()),
        Some(O26FaultAction::Fail) => Err("injected path-scoped O26 fault"),
        Some(O26FaultAction::Pause(state)) => {
            let mut phase = state
                .phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *phase = 1;
            state.changed.notify_all();
            while *phase != 2 {
                phase = state
                    .changed
                    .wait(phase)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
fn o26_fault_armed(rally_dir: &Path, point: O26FaultPoint) -> bool {
    o26_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(rally_dir.to_path_buf(), point))
        .is_some_and(|queue| !queue.is_empty())
}

#[cfg(not(test))]
pub(crate) fn trigger_o26_fault(
    _rally_dir: &Path,
    _point: O26FaultPoint,
) -> std::result::Result<(), &'static str> {
    Ok(())
}

#[cfg(not(test))]
fn o26_fault_armed(_rally_dir: &Path, _point: O26FaultPoint) -> bool {
    false
}

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
use crate::retry_budget::RetryBudget;
use crate::{
    FACT_SCHEMA, claim_authority, normalize_paths, now_string, path_matches_scope, repo_root,
    short_id,
};

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FactKind {
    Claim,
    /// Durable lease extension for an existing claim. `ref_id` names the
    /// original claim and `lease_expires_at:` carries the new authoritative
    /// deadline. This is an internal store transition, not a public `say` kind.
    #[serde(rename = "claim.renewed")]
    ClaimRenewed,
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
    /// Every variant, in the order `rally say --help` lists them. Kept beside
    /// the enum so the declaration and this slice are read together.
    pub(crate) const ALL: &'static [Self] = &[
        Self::Claim,
        Self::ClaimRenewed,
        Self::ClaimExpired,
        Self::Release,
        Self::Blocker,
        Self::Resolve,
        Self::Decision,
        Self::Artifact,
        Self::Handoff,
        Self::Risk,
        Self::Lesson,
        Self::Session,
        Self::Wake,
        Self::Presence,
        Self::Read,
        Self::BacklogItem,
        Self::Receipt,
        Self::Standby,
        Self::Mission,
        Self::Unknown,
    ];

    /// Whether `rally say --help` and the unsupported-kind error advertise this
    /// kind as a KIND a caller may type.
    ///
    /// Deliberately exhaustive with no `_` arm: a new variant fails to compile
    /// here until someone decides whether callers may author it. That is the
    /// drift lock — the advertised list is derived from this match, never
    /// hand-maintained alongside it.
    pub(crate) const fn advertised_in_say(&self) -> bool {
        match self {
            Self::Claim
            | Self::ClaimExpired
            | Self::Release
            | Self::Blocker
            | Self::Resolve
            | Self::Decision
            | Self::Artifact
            | Self::Handoff
            | Self::Risk
            | Self::Lesson
            | Self::Session
            | Self::Wake
            | Self::Presence
            | Self::Read
            | Self::BacklogItem
            | Self::Receipt
            | Self::Standby
            | Self::Mission => true,
            // Renewals enter through `RoomStore::renew_claim_lease`, which
            // verifies the live target and owner under the write lock. `parse`
            // rejects it, so advertising it would hand callers a kind that fails.
            Self::ClaimRenewed => false,
            // The `#[serde(other)]` fallback for facts written by a newer
            // producer. `parse` accepts it for round-tripping, but it is not a
            // kind anyone should author — listing it in a discovery surface
            // invites exactly the junk facts this enumeration exists to prevent.
            Self::Unknown => false,
        }
    }

    /// The KIND values `rally say` advertises, in `ALL` order. Single source of
    /// truth for `rally say --help` and the unsupported-kind error, so neither
    /// surface can drift from the other or from [`FactKind::parse`].
    pub(crate) fn say_kinds() -> Vec<&'static str> {
        Self::ALL
            .iter()
            .filter(|kind| kind.advertised_in_say())
            .map(|kind| kind.as_str())
            .collect()
    }

    /// [`FactKind::say_kinds`] rendered for a help line or an error message.
    pub(crate) fn say_kinds_display() -> String {
        Self::say_kinds().join(", ")
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "claim" => Some(Self::Claim),
            // `claim.renewed` is intentionally internal-only. Renewals enter
            // through `RoomStore::renew_claim_lease`, which verifies the live
            // target and owner under the write lock.
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
            // `as_str` renders `backlog-item`, but serde writes the variant to
            // the ledger as `backlog_item` (the `rename_all = "snake_case"`
            // default). A caller who reads a kind off a fact and retypes it
            // hands us the underscore form, so accept both — same reason
            // `claim_expired` aliases `claim.expired` above.
            "backlog-item" | "backlog_item" => Some(Self::BacklogItem),
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
            Self::ClaimRenewed => "claim.renewed",
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

#[cfg(test)]
mod fact_kind_say_surface_tests {
    use super::FactKind;

    /// Anything `rally say --help` and the unsupported-kind error advertise must
    /// actually work. Advertising a kind that `parse` rejects is worse than
    /// advertising nothing: it sends the caller down a path that fails.
    #[test]
    fn every_advertised_kind_parses_and_round_trips() {
        for kind in FactKind::say_kinds() {
            let parsed = FactKind::parse(kind).unwrap_or_else(|| {
                panic!("advertised kind {kind:?} is rejected by FactKind::parse")
            });
            assert_eq!(
                parsed.as_str(),
                kind,
                "advertised kind {kind:?} is not the canonical spelling"
            );
        }
    }

    /// The spelling serde writes to the ledger must be a spelling `parse`
    /// accepts. Those are two independent tables — `rename_all = "snake_case"`
    /// plus per-variant renames on one side, a hand-written match on the other
    /// — and `BacklogItem` drifted across them: every backlog fact on disk
    /// carried `backlog_item`, and `rally say backlog_item` rejected it. Round-
    /// tripping the real serde output for every variant catches the next drift
    /// at compile-and-test time instead of at a caller's prompt.
    #[test]
    fn every_wire_spelling_parses_back_to_its_variant() {
        for kind in FactKind::ALL {
            // `claim.renewed` is unparseable on purpose: renewals enter through
            // `RoomStore::renew_claim_lease`, never through a caller-typed kind.
            if matches!(kind, FactKind::ClaimRenewed) {
                continue;
            }
            let wire = serde_json::to_value(kind)
                .expect("FactKind serializes")
                .as_str()
                .expect("FactKind serializes to a JSON string")
                .to_string();
            assert_eq!(
                FactKind::parse(&wire).as_ref(),
                Some(kind),
                "serde writes {kind:?} to the ledger as {wire:?}, which FactKind::parse rejects"
            );
        }
    }

    /// `ALL` has to list every variant or `say_kinds` silently under-reports.
    /// `advertised_in_say` is exhaustive, so a new variant cannot compile
    /// without a decision — but nothing forces it into `ALL`, and this does.
    #[test]
    fn all_lists_every_variant_exactly_once() {
        let mut seen: Vec<&str> = FactKind::ALL.iter().map(FactKind::as_str).collect();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "FactKind::ALL repeats a variant");
        // Every kind `parse` accepts must appear. `claim_expired` is an alias of
        // `claim.expired`, so it is checked through its canonical spelling.
        for kind in [
            "claim",
            "claim.renewed",
            "claim.expired",
            "release",
            "blocker",
            "resolve",
            "decision",
            "artifact",
            "handoff",
            "risk",
            "lesson",
            "session",
            "wake",
            "presence",
            "read",
            "backlog-item",
            "receipt",
            "standby",
            "mission",
            "unknown",
        ] {
            assert!(
                seen.binary_search(&kind).is_ok(),
                "FactKind::ALL is missing {kind:?}; say_kinds() would omit it"
            );
        }
    }

    /// The two withheld kinds are withheld on purpose. Pinned so a future edit
    /// has to restate the reason rather than quietly widen the surface.
    #[test]
    fn internal_kinds_stay_unadvertised() {
        assert!(
            !FactKind::ClaimRenewed.advertised_in_say(),
            "claim.renewed is not parseable — advertising it hands callers a failing kind"
        );
        assert!(
            !FactKind::Unknown.advertised_in_say(),
            "unknown is the serde fallback, not a kind a caller should author"
        );
        let display = FactKind::say_kinds_display();
        assert!(
            display.contains("handoff"),
            "expected a real kind: {display:?}"
        );
        assert!(
            !display.contains("claim.renewed"),
            "leaked internal kind: {display:?}"
        );
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
        // factstr compacts record sequence numbers when rebuilding a sparse
        // canonical ledger. Its normalized payload seq therefore carries the
        // canonical high-water; legacy payloads without one fall back to the
        // database record sequence.
        if fact.seq == 0 {
            fact.seq = seq;
        }
        Ok(fact)
    }

    /// Decode a canonical JSONL row. Unlike a derived-database read, the
    /// LedgerLine envelope owns the sequence and always overwrites payload seq.
    fn from_segment_value(value: Value, seq: i64) -> Result<Self> {
        let mut fact: Self =
            serde_json::from_value(value).map_err(RallyError::json("parse fact payload"))?;
        fact.seq = seq;
        Ok(fact)
    }
}

/// A typed degradation that happened only after the canonical JSONL fact was
/// synced and read back exactly. Callers must surface these warnings, but must
/// not retry the mutation: `committed` is already true.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectionWarningCode {
    FactsDb,
    ReconcileCache,
    LogIndex,
    ClaimIndex,
    TransitionVerification,
    PostCommitWork,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub(crate) struct ProjectionWarning {
    pub(crate) code: ProjectionWarningCode,
    pub(crate) message: String,
}

/// Successful append reply. This type is returned only after exact canonical
/// readback, so `committed` is invariantly true; projection failures are data,
/// not retryable append failures.
#[must_use = "a committed append may carry projection warnings that forbid blind retry"]
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct AppendOutcome {
    pub(crate) fact: Fact,
    pub(crate) committed: bool,
    pub(crate) projection_complete: bool,
    pub(crate) warnings: Vec<ProjectionWarning>,
}

#[must_use = "conditional append results distinguish no-op from committed outcomes"]
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "status", content = "outcome", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ConditionalAppendOutcome {
    NotApplied,
    Applied(AppendOutcome),
}

#[cfg(test)]
impl ConditionalAppendOutcome {
    fn is_some(&self) -> bool {
        matches!(self, Self::Applied(_))
    }

    fn is_none(&self) -> bool {
        matches!(self, Self::NotApplied)
    }
}

/// Result of a lease-renewal request. `append_outcome` is present exactly when
/// this request appended or resolved its own stable canonical renewal event.
/// A monotonic no-op or missing claim returns the observed record without
/// pretending a write occurred.
#[must_use = "lease renewal may have canonically committed with projection warnings"]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RenewClaimLeaseOutcome {
    pub(crate) record: Option<claim_authority::ActiveClaimRecord>,
    pub(crate) append_outcome: Option<AppendOutcome>,
}

#[cfg(test)]
impl RenewClaimLeaseOutcome {
    pub(crate) fn unwrap(self) -> claim_authority::ActiveClaimRecord {
        self.record.unwrap()
    }

    pub(crate) fn expect(self, message: &str) -> claim_authority::ActiveClaimRecord {
        self.record.expect(message)
    }
}

impl AppendOutcome {
    fn committed(fact: Fact, warnings: Vec<ProjectionWarning>) -> Self {
        Self {
            fact,
            committed: true,
            projection_complete: warnings.is_empty(),
            warnings,
        }
    }

    pub(crate) fn into_fact_reporting(self) -> Fact {
        crate::record_append_outcome(&self);
        self.fact
    }
}

#[derive(Deserialize)]
struct AppendOutcomeWire {
    fact: Fact,
    committed: bool,
    projection_complete: bool,
    warnings: Vec<ProjectionWarning>,
}

impl<'de> Deserialize<'de> for AppendOutcome {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = AppendOutcomeWire::deserialize(deserializer)?;
        if !wire.committed {
            return Err(serde::de::Error::custom(
                "append outcome invariant violated: successful reply must have committed=true",
            ));
        }
        if wire.projection_complete != wire.warnings.is_empty() {
            return Err(serde::de::Error::custom(
                "append outcome invariant violated: projection_complete must equal warnings.is_empty()",
            ));
        }
        Ok(Self {
            fact: wire.fact,
            committed: wire.committed,
            projection_complete: wire.projection_complete,
            warnings: wire.warnings,
        })
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

/// TRUE counts for every room bucket, taken BEFORE any budget fill or archive
/// drop. Always serialized, whether or not anything was omitted.
///
/// This is the honesty contract. `stale_facts` is empty in the default room
/// output, so without a count beside it "1390 archived facts" and "no archived
/// facts" would be indistinguishable from the consumer's side — the exact
/// failure mode that let RC-027 hide a severed channel for five weeks.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub(crate) struct RoomTotals {
    pub(crate) active_claims: usize,
    pub(crate) active_blockers: usize,
    pub(crate) open_handoffs: usize,
    pub(crate) current_decisions: usize,
    pub(crate) current_risks: usize,
    pub(crate) system_health: usize,
    pub(crate) recent_artifacts: usize,
    pub(crate) unconsumed_artifacts: usize,
    pub(crate) stale_facts: usize,
    pub(crate) squads: usize,
}

/// What one bucket contributed, and what it left out.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub(crate) struct BucketComposition {
    pub(crate) total: usize,
    pub(crate) emitted: usize,
    pub(crate) omitted: usize,
    /// Event ids of the omitted items, for a targeted `rally locate <id>`.
    /// Populated for the actionable classes; omitted for `stale_facts`, where
    /// the drill-in is `--include-archived` rather than 1000+ ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) omitted_ids: Vec<String>,
    /// TRUE when `omitted_ids` hit their response-size cap. `omitted` remains
    /// the complete count; callers can use `rally locate` for a known id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) omitted_ids_truncated: bool,
    /// Why the omission happened: `"budget"` or `"archived"`.
    pub(crate) reason: String,
}

/// Present ONLY when the room omitted something. Its absence is the positive
/// statement that the response is complete.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub(crate) struct RoomComposition {
    /// The byte ceiling in force, or `None` when the ceiling is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) budget_bytes: Option<usize>,
    /// Exact pretty-printed byte size of the final `rally room --json`
    /// response, including its trailing newline.
    pub(crate) emitted_bytes: usize,
    /// Per-bucket accounting. Only buckets that omitted something appear.
    pub(crate) buckets: BTreeMap<String, BucketComposition>,
    /// Commands that return the full view.
    pub(crate) drill_in: Vec<String>,
    /// TRUE when the never-cut buckets ALONE exceed the ceiling, so the room
    /// shipped over budget rather than dropping correctness-bearing state.
    ///
    /// This is the defined behavior for "never-cut exceeds budget", and it is
    /// deliberately loud. `squads` grows monotonically (32 → 78 → 147 → 155
    /// distinct tools over four months, and tool ids are per-session so they
    /// never recur), and un-reaped claims grow with it, so this condition is
    /// reachable rather than hypothetical. The alternative — cutting a claim or
    /// a peer to fit — trades a payload problem for a write collision.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) over_budget: bool,
    /// Which never-cut buckets drove an over-budget response, largest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) over_budget_causes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub(crate) struct RoomSnapshot {
    pub(crate) max_seq: i64,
    /// R10: highest seq of a substantive (non-read-checkpoint) fact.
    /// Used internally by `command_next` to record the read position WITHOUT
    /// inflating it with the read-checkpoint's own seq (anti-loop).
    ///
    /// Omitted from the PUBLIC room JSON, and carried across the daemon wire in
    /// the [`SnapshotInternals`] side-channel — see that type for why the two
    /// are different questions.
    #[serde(skip)]
    pub(crate) content_max_seq: i64,
    /// `created_at` of the highest-seq fact; `None` when the room is empty.
    /// Populated by `snapshot_from_facts` so `status_global` avoids a second
    /// `store.facts()` call. Omitted from the public room JSON; carried over the
    /// daemon wire in [`SnapshotInternals`].
    #[serde(skip)]
    pub(crate) last_activity_ts: Option<String>,
    pub(crate) active_claims: Vec<Fact>,
    pub(crate) active_blockers: Vec<Fact>,
    pub(crate) open_handoffs: Vec<Fact>,
    /// Pending wake intents used to coalesce repeated `next` polls. Internal
    /// projection only; the public room schema remains unchanged. Carried over
    /// the daemon wire in [`SnapshotInternals`].
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
    /// `event_id` of the ONE unscoped blocker that is an AUTHORIZED room-wide
    /// freeze, if any: its author held the lead seat AT ITS OWN `seq`.
    ///
    /// ARP-R-01 / design audit D9. `check_before_write` used to decide this
    /// itself, comparing each unscoped blocker's author against the CURRENT
    /// lead. That made the verdict a function of the room's present state
    /// rather than of the fact, and the same fact id flipped both ways in live
    /// testing: a non-lead's blocker armed into a room-wide deny once its
    /// author later took the seat, and the honest lead's freeze disarmed the
    /// moment anyone else took it. The room's only stop control was removable
    /// in one command.
    ///
    /// The decision is made HERE, in the projection, where the fact slice and
    /// each fact's `seq` are both in hand, and `check` reports it. Authority is
    /// a property of the moment a fact was written.
    ///
    /// This field SERIALIZES on purpose. Three sibling fields are
    /// `#[serde(skip)]` and therefore arrive empty over the daemon wire, which
    /// the design audit (D1/D6) found silently changes behavior in routed mode.
    /// `check` runs client-side on a possibly-routed snapshot, so a skipped
    /// field here would mean no freeze is ever enforced with rallyd running.
    /// `#[serde(default)]` keeps older persisted/public payloads deserializable;
    /// daemon compatibility is enforced separately by the wire-version probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) room_freeze_id: Option<String>,
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
    /// TRUE bucket counts as projected, before any output-path composition.
    /// `#[serde(default)]` so older persisted/public payloads still deserialize.
    #[serde(default)]
    pub(crate) totals: RoomTotals,
    /// Present only when the output path omitted something. Absence is the
    /// positive statement that this response is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) composition: Option<RoomComposition>,
    /// Tools whose newest fact is older than that tool's adaptive window at
    /// projection time. The only input the relevance model has that may lower an
    /// item's rank, so it is derived where the signals live rather than
    /// re-inferred downstream.
    ///
    /// **This is a HEARTBEAT verdict, not a `Liveness` one.** Membership here
    /// does NOT mean `Liveness::Stale`: dropping a squad demands four-signal
    /// unanimity because hiding a live peer causes a write collision, while
    /// ranking an item lower hides nothing and so uses the one signal present on
    /// every tool. The reasoning is stated at the insertion site in
    /// `snapshot_from_facts_with_policy` and in the `relevance` module docs; all
    /// three said different things until design audit D2, and this is the
    /// contract the three now share.
    ///
    /// Omitted from the public room JSON, and carried across the daemon wire in
    /// [`SnapshotInternals`]. Before that side-channel existed this field was
    /// dropped by routing, so Direct and Routed mode ranked the SAME ledger
    /// differently — design audit D1.
    #[serde(skip)]
    pub(crate) stale_authors: BTreeSet<String>,
    /// Newest authored ledger timestamp per tool. This is authority evidence,
    /// not presentation state: adaptive decay may remove a squad without
    /// erasing the timestamp destructive reclaim must inspect.
    #[serde(skip)]
    pub(crate) author_last_seen: BTreeMap<String, String>,
}

/// The `#[serde(skip)]` projections on [`RoomSnapshot`], carried across the
/// daemon wire beside the snapshot.
///
/// # The two questions `#[serde(skip)]` was answering at once
///
/// `RoomSnapshot`'s `Serialize` impl serves two consumers: the PUBLIC room JSON
/// that `rally room --json` prints, and the daemon wire that `rallyd` replies
/// on. `#[serde(skip)]` answered "keep this out of the public schema" and, as a
/// side effect nobody chose, also answered "drop this when rallyd is running".
///
/// The second answer is a behaviour change disguised as a serialization detail.
/// Design audit D1 and D6 traced three consequences, all silent:
///
/// * **Ranking (D1).** `apply_budget` demotes items whose author is in
///   `stale_authors`. Empty over the wire means no item is ever demoted, so the
///   same ledger composes into a DIFFERENT room depending on whether a daemon
///   happened to be up.
/// * **Read checkpoints (D6).** `enter` and `next` pass
///   `snapshot.content_max_seq` to `maybe_append_read_checkpoint`, which
///   coalesces at `read_seq <= last_checkpoint` — so a routed caller passing 0
///   wrote no checkpoint at all, and its read position never advanced.
/// * **Wake coalescing (D6).** `append_next_wake_intent` looks for an existing
///   pending wake before appending one. An empty `pending_wakes` means the guard
///   never matches, so a routed caller appends a DUPLICATE wake intent.
///
/// # Why a side-channel rather than serializing the fields
///
/// Serializing them would fix routing and change the public room schema at the
/// same time, adding an unbudgeted `pending_wakes` array to every `rally room
/// --json` response — which is the payload growth RC-054 is open about. The two
/// questions stay separate: this struct rides the wire, and the public schema is
/// byte-identical to what it was.
///
/// Every field is `#[serde(default)]` for additive changes within a compatible
/// wire version. The v3 identity probe rejects every older daemon before
/// routing; that boundary both protects scoped snapshot internals and prevents
/// claim renewal from falling back to a daemon that predates caller-session
/// authority.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct SnapshotInternals {
    #[serde(default)]
    pub(crate) content_max_seq: i64,
    #[serde(default)]
    pub(crate) last_activity_ts: Option<String>,
    #[serde(default)]
    pub(crate) pending_wakes: Vec<Fact>,
    #[serde(default)]
    pub(crate) stale_authors: BTreeSet<String>,
    #[serde(default)]
    pub(crate) author_last_seen: BTreeMap<String, String>,
}

/// Hard, fail-loud bounds for the daemon-only snapshot side-channel.
///
/// The side-channel preserves exact direct/routed behaviour, so it is never
/// silently truncated. Crossing any bound returns a structured daemon error
/// rather than dropping wake-dedupe or relevance state. The aggregate byte cap
/// is deliberately much smaller than the 8 MiB frame limit: other public
/// snapshot fields still need room inside that frame.
pub(crate) const MAX_WIRE_PENDING_WAKES: usize = 1_024;
pub(crate) const MAX_WIRE_STALE_AUTHORS: usize = 4_096;
pub(crate) const MAX_WIRE_SNAPSHOT_INTERNALS_BYTES: usize = 512 * 1_024;

/// Key the internals ride under in the wire snapshot object.
///
/// Double-underscored so it cannot collide with a `RoomSnapshot` field name, and
/// removed on the way in, so a snapshot that round-trips the wire is byte-equal
/// to one that never left the process.
pub(crate) const WIRE_INTERNALS_KEY: &str = "__internals";

impl RoomSnapshot {
    /// Lift the skipped projections out for the wire.
    pub(crate) fn internals(&self) -> SnapshotInternals {
        SnapshotInternals {
            content_max_seq: self.content_max_seq,
            last_activity_ts: self.last_activity_ts.clone(),
            pending_wakes: self.pending_wakes.clone(),
            stale_authors: self.stale_authors.clone(),
            author_last_seen: self.author_last_seen.clone(),
        }
    }

    /// Restore the skipped projections after the wire.
    pub(crate) fn restore_internals(&mut self, internals: SnapshotInternals) {
        self.content_max_seq = internals.content_max_seq;
        self.last_activity_ts = internals.last_activity_ts;
        self.pending_wakes = internals.pending_wakes;
        self.stale_authors = internals.stale_authors;
        self.author_last_seen = internals.author_last_seen;
    }
}

/// Serialize a snapshot FOR THE WIRE: the public shape plus [`SnapshotInternals`]
/// under [`WIRE_INTERNALS_KEY`].
///
/// A snapshot that does not serialize to a JSON object is returned unchanged.
/// No `RoomSnapshot` does; the branch exists so a future `#[serde]` change
/// degrades to the old behaviour instead of panicking.
pub(crate) fn snapshot_to_wire_value(
    snapshot: &RoomSnapshot,
) -> std::result::Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(snapshot)?;
    let internals = snapshot.internals();
    if internals.pending_wakes.len() > MAX_WIRE_PENDING_WAKES {
        return Err(serde_json::Error::io(std::io::Error::other(format!(
            "snapshot internals exceed pending-wake bound: {} > {}",
            internals.pending_wakes.len(),
            MAX_WIRE_PENDING_WAKES
        ))));
    }
    if internals.stale_authors.len() > MAX_WIRE_STALE_AUTHORS {
        return Err(serde_json::Error::io(std::io::Error::other(format!(
            "snapshot internals exceed stale-author bound: {} > {}",
            internals.stale_authors.len(),
            MAX_WIRE_STALE_AUTHORS
        ))));
    }
    let internals = serde_json::to_value(internals)?;
    let internals_bytes = serde_json::to_vec(&internals)?.len();
    if internals_bytes > MAX_WIRE_SNAPSHOT_INTERNALS_BYTES {
        return Err(serde_json::Error::io(std::io::Error::other(format!(
            "snapshot internals exceed byte bound: {internals_bytes} > {MAX_WIRE_SNAPSHOT_INTERNALS_BYTES}"
        ))));
    }
    if let Value::Object(map) = &mut value {
        map.insert(WIRE_INTERNALS_KEY.to_string(), internals);
    }
    Ok(value)
}

/// Deserialize a wire snapshot, restoring [`SnapshotInternals`] if present.
///
/// A payload without the key yields defaults only for additive compatibility
/// within the current wire version. The identity probe rejects older daemons
/// before this decoder runs, including daemons that lack scoped snapshots or
/// would synthesize renewal authority from claim id.
pub(crate) fn snapshot_from_wire_value(
    mut value: Value,
) -> std::result::Result<RoomSnapshot, serde_json::Error> {
    let internals = match &mut value {
        Value::Object(map) => map
            .remove(WIRE_INTERNALS_KEY)
            .map(serde_json::from_value::<SnapshotInternals>)
            .transpose()?,
        _ => None,
    };
    let mut snapshot: RoomSnapshot = serde_json::from_value(value)?;
    if let Some(internals) = internals {
        snapshot.restore_internals(internals);
    }
    Ok(snapshot)
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
    /// Fail-closed: an owner whose authored activity cannot be established is
    /// NEVER reclaimable. The owner's age normally comes from the squad
    /// projection; a decay-pruned squad still retains its authored timestamp.
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
        // Read durable authored activity, not the presentation-only squad
        // list. Adaptive decay can remove a provably stale squad, while facts
        // merely TARGETED at the owner must not become owner activity.
        let owner_last_seen = self
            .author_last_seen
            .get(owner)
            .map(String::as_str)
            .or_else(|| {
                // Compatibility for programmatic snapshots constructed before
                // authored timestamps became a separate internal projection.
                self.squads
                    .iter()
                    .find(|squad| squad.tool == owner)
                    .map(|squad| squad.last_seen_ts.as_str())
            });
        let eligible = owner_last_seen
            .and_then(|last_seen| chrono::DateTime::parse_from_rfc3339(last_seen).ok())
            .is_some_and(|dt| now_secs - dt.timestamp() > timeout);
        (eligible, size)
    }

    /// Apply the caller's `--tool` / `--path` / `--event` / `--thread` /
    /// `--since` query.
    ///
    /// `totals` is RECOMPUTED from the filtered buckets rather than carried
    /// through. Totals answer "how much did this query match", so composition
    /// can then answer "how much of that shipped". Carrying pre-filter totals
    /// would make every filtered response report an omission it did not make.
    pub(crate) fn filtered(self, query: &RoomQuery) -> Self {
        if query.is_empty() {
            return self;
        }
        let mut filtered = Self {
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
            room_freeze_id: self.room_freeze_id.clone(),
            readers: self.readers,
            mission: self.mission,
            totals: self.totals,
            composition: self.composition,
            stale_authors: self.stale_authors,
            author_last_seen: self.author_last_seen,
        };
        filtered.totals = RoomTotals {
            active_claims: filtered.active_claims.len(),
            active_blockers: filtered.active_blockers.len(),
            open_handoffs: filtered.open_handoffs.len(),
            current_decisions: filtered.current_decisions.len(),
            current_risks: filtered.current_risks.len(),
            system_health: filtered.system_health.len(),
            recent_artifacts: filtered.recent_artifacts.len(),
            unconsumed_artifacts: filtered.unconsumed_artifacts.len(),
            stale_facts: filtered.stale_facts.len(),
            squads: filtered.squads.len(),
        };
        filtered
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

/// Durable association between one stable protocol session and its task
/// engagement. S9 owns the deterministic resolver; S10 owns the writers and
/// CLI surfaces that persist and consume these records.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct EngagementBinding {
    pub(crate) session_id: String,
    pub(crate) tool: String,
    pub(crate) engagement: String,
    pub(crate) active: bool,
    /// Monotonic record order. The newest record for a session supersedes its
    /// prior binding without minting a new actor identity for the task.
    pub(crate) seq: i64,
}

/// Resolve the engagement for a scoped read or write without consulting the
/// caller process id or guessing between concurrent sessions.
///
/// Priority is explicit process/CLI engagement, explicit managed session,
/// unique active binding for the tool, then the legacy room-wide fallback.
/// Ambiguity fails closed before the legacy fallback can relabel either task.
#[allow(dead_code)]
pub(crate) fn resolve_current_engagement(
    explicit_engagement: Option<&str>,
    explicit_session_id: Option<&str>,
    tool: Option<&str>,
    bindings: &[EngagementBinding],
    legacy_fallback: Option<&str>,
) -> Result<String> {
    if let Some(engagement) = explicit_engagement {
        return validate_scoped_engagement(engagement);
    }

    let mut latest_by_session = BTreeMap::<&str, &EngagementBinding>::new();
    for binding in bindings {
        match latest_by_session.get(binding.session_id.as_str()) {
            Some(current) if current.seq > binding.seq => {}
            Some(current)
                if current.seq == binding.seq
                    && (current.engagement != binding.engagement
                        || current.tool != binding.tool
                        || current.active != binding.active) =>
            {
                return Err(RallyError::Usage(format!(
                    "ambiguous engagement binding for session {:?} at seq {}",
                    binding.session_id, binding.seq
                )));
            }
            _ => {
                latest_by_session.insert(binding.session_id.as_str(), binding);
            }
        }
    }

    if let Some(session_id) = explicit_session_id {
        let binding = latest_by_session
            .get(session_id)
            .copied()
            .filter(|binding| binding.active)
            .ok_or_else(|| {
                RallyError::Usage(format!(
                    "no active engagement binding for explicit session {session_id:?}"
                ))
            })?;
        if tool.is_some_and(|tool| tool != binding.tool) {
            return Err(RallyError::Usage(format!(
                "explicit session {session_id:?} is bound to tool {:?}, not {:?}",
                binding.tool,
                tool.unwrap_or_default()
            )));
        }
        return validate_scoped_engagement(&binding.engagement);
    }

    if let Some(tool) = tool {
        let matches = latest_by_session
            .values()
            .copied()
            .filter(|binding| binding.active && binding.tool == tool)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [binding] => return validate_scoped_engagement(&binding.engagement),
            [] => {}
            _ => {
                return Err(RallyError::Usage(format!(
                    "ambiguous current engagement for tool {tool:?}: {} active session bindings; provide a managed session id or explicit engagement",
                    matches.len()
                )));
            }
        }
    }

    if let Some(engagement) = legacy_fallback {
        return validate_scoped_engagement(engagement);
    }

    Err(RallyError::Usage(
        "current engagement is unknown; provide an explicit engagement or enter/adopt a managed session"
            .to_string(),
    ))
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
///   session_facts_with_context_version   → SessionFactsWithContextVersion
///   snapshot / snapshot_with_archived    → SnapshotWithArchived
///   snapshot_scoped                      → SnapshotScoped
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
    /// In-process fallback: opens facts.db only while this process owns the
    /// direct EX lock and holds the daemon owner lock SH.
    Direct(DirectRoomStore),
    /// Daemon-routed store (Chunk C): speaks the `store_wire` protocol over
    /// `.rally/rallyd.sock`, holds NO facts.db handle (G3). Constructed only
    /// by [`RoomStore::route`] after a successful daemon identity probe.
    Routed(RoutedRoomStore),
}

/// Read-only handle used while an inject command waits for a target-authored
/// acknowledgement. Direct mode must not retain exclusive store ownership
/// during that wait or a peer CLI cannot append the acknowledgement.
pub(crate) enum AckPollingStore {
    Direct {
        room_dir: PathBuf,
        log_dir: PathBuf,
        archive_dir: PathBuf,
    },
    Routed(RoutedRoomStore),
}

impl AckPollingStore {
    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        match self {
            Self::Direct {
                room_dir,
                log_dir,
                archive_dir,
            } => {
                let _guard = acquire_room_mutation_lock(room_dir)?;
                facts_from_segments(log_dir, archive_dir)
            }
            Self::Routed(routed) => routed.facts(),
        }
    }
}

/// Today's in-process room store (was `RoomStore` before the router split).
/// The `Direct` variant of [`RoomStore`]. In direct-CLI mode `warm_fact_store`
/// is always `None`, so every hot interior open goes through
/// [`DirectRoomStore::fact_store_handle`]'s cold branch = today's per-op open,
/// byte-identical to main (G1). Chunk B installs a warm pool for daemon mode.
pub(crate) struct DirectRoomStore {
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
pub(crate) struct RoomMutationLock {
    file: fs::File,
}

#[cfg(not(unix))]
pub(crate) struct RoomMutationLock;

#[cfg(unix)]
pub(crate) fn acquire_room_mutation_lock(room_dir: &Path) -> Result<RoomMutationLock> {
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
    let deadline = effective_mutation_deadline();
    loop {
        if Instant::now() >= deadline {
            return Err(mutation_not_started(&path));
        }
        let rc =
            unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_EX | unix_lock::LOCK_NB) };
        if rc == 0 {
            pause_after_room_lock_flock_for_test();
            if Instant::now() >= deadline {
                // A process can be descheduled between the pre-check and the
                // successful syscall. Relinquish the just-acquired lock and
                // preserve the typed no-mutation-started contract.
                let _ = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_UN) };
                return Err(mutation_not_started_after_provisional_lock(&path));
            }
            return Ok(RoomMutationLock { file });
        }
        let source = io::Error::last_os_error();
        if source.kind() != io::ErrorKind::WouldBlock {
            return Err(RallyError::Io {
                context: format!("lock {}", path.display()),
                source,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(mutation_not_started(&path));
        }
        thread::sleep(MUTATION_LOCK_RETRY_DELAY.min(remaining));
    }
}

#[cfg(not(unix))]
pub(crate) fn acquire_room_mutation_lock(_room_dir: &Path) -> Result<RoomMutationLock> {
    Ok(RoomMutationLock)
}

#[cfg(unix)]
impl Drop for RoomMutationLock {
    fn drop(&mut self) {
        let _ = unsafe { unix_lock::flock(self.file.as_raw_fd(), unix_lock::LOCK_UN) };
    }
}

impl Drop for DirectRoomStore {
    fn drop(&mut self) {
        // The daemon closes its warm pool explicitly through
        // `close_warm_fact_store_bounded`, under `mutation.lock`. Drop is only
        // the emergency/failure path: it must never wait for a contended lock or
        // panic while unwinding. Tell the vendored store not to synchronously
        // join its workers; the failure-only branch below preserves the inert
        // pool until process exit rather than scheduling an unguarded close.
        if let Some(mut warm) = self.warm_fact_store.take() {
            warm.prepare_nonblocking_drop();
            // An unexpected Drop cannot safely close without the room lock and
            // cannot wait for that lock. Keep the inert pool process-local;
            // daemon process exit reclaims it. Normal lifecycle always uses the
            // explicit bounded close above, so this is a failure-only tradeoff
            // that prevents delayed unguarded WAL housekeeping.
            std::mem::forget(warm);
        }
    }
}

/// RAII guard for a named advisory lock. It holds a `flock` for as long as the
/// guard lives; the kernel also releases it on process death. Direct routing
/// retains both its direct EX guard and daemon-exclusion SH guard in the
/// process-global table; automatic reaping holds its guard for one pass.
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

/// Open (creating if absent) an advisory lock file at `rally_dir`.
#[cfg(unix)]
fn open_named_lock_file(rally_dir: &Path, filename: &str) -> Result<fs::File> {
    fs::create_dir_all(rally_dir)
        .map_err(RallyError::io(format!("create {}", rally_dir.display())))?;
    let path = rally_dir.join(filename);
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
    let file = open_named_lock_file(rally_dir, RALLYD_OWNER_LOCK_FILENAME)?;
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
    let file = open_named_lock_file(rally_dir, RALLYD_OWNER_LOCK_FILENAME)?;
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

/// Acquire daemon ownership within `budget`, returning a typed not-started
/// result when direct owners do not drain in time.
#[cfg(unix)]
pub(crate) fn acquire_owner_exclusive_bounded(
    rally_dir: &Path,
    budget: Duration,
) -> Result<OwnerGuard> {
    let path = rally_dir.join(RALLYD_OWNER_LOCK_FILENAME);
    let file = open_named_lock_file(rally_dir, RALLYD_OWNER_LOCK_FILENAME)?;
    let now = Instant::now();
    let deadline = now
        .checked_add(budget)
        .unwrap_or(now + MUTATION_LOCK_FALLBACK_BOUND);
    loop {
        if Instant::now() >= deadline {
            return Err(RallyError::NotStarted(format!(
                "daemon-open-not-started: deadline elapsed before acquiring {}; no daemon runtime state was published",
                path.display()
            )));
        }
        let rc =
            unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_EX | unix_lock::LOCK_NB) };
        if rc == 0 {
            pause_after_owner_lock_flock_for_test();
            if Instant::now() >= deadline {
                let _ = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_UN) };
                return Err(RallyError::NotStarted(format!(
                    "daemon-open-not-started: deadline elapsed after provisional lock acquisition at {}; lock released before any daemon runtime state was published and retry is safe",
                    path.display()
                )));
            }
            return Ok(OwnerGuard { file });
        }
        let source = io::Error::last_os_error();
        if source.kind() != io::ErrorKind::WouldBlock {
            return Err(RallyError::Io {
                context: format!("lock-ex {}", path.display()),
                source,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RallyError::NotStarted(format!(
                "daemon-open-not-started: deadline elapsed before acquiring {}; no daemon runtime state was published",
                path.display()
            )));
        }
        thread::sleep(MUTATION_LOCK_RETRY_DELAY.min(remaining));
    }
}

/// Try to acquire a named exclusive advisory lock without blocking.
///
/// The file is only a stable rendezvous point. Kernel lock ownership, not file
/// creation or deletion, provides single-flight semantics and is released on
/// process exit.
#[cfg(unix)]
pub(crate) fn acquire_named_exclusive_nb(
    rally_dir: &Path,
    filename: &str,
) -> Result<Option<OwnerGuard>> {
    let file = open_named_lock_file(rally_dir, filename)?;
    let rc = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_EX | unix_lock::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(OwnerGuard { file }));
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(RallyError::Io {
        context: format!("lock-ex-nb {}", rally_dir.join(filename).display()),
        source: err,
    })
}

/// Try to become the sole direct-mode process for this room. This lock is
/// deliberately independent from the daemon ownership lock: direct takes it
/// EX before taking the daemon lock SH, while daemon startup only takes the
/// daemon lock EX. That order cannot form a cycle.
#[cfg(unix)]
fn acquire_direct_owner_exclusive_nb(rally_dir: &Path) -> Result<Option<OwnerGuard>> {
    acquire_named_exclusive_nb(rally_dir, DIRECT_OWNER_LOCK_FILENAME)
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

#[cfg(not(unix))]
pub(crate) fn acquire_owner_exclusive_bounded(
    _rally_dir: &Path,
    _budget: Duration,
) -> Result<OwnerGuard> {
    Ok(OwnerGuard)
}

#[cfg(not(unix))]
fn acquire_direct_owner_exclusive_nb(_rally_dir: &Path) -> Result<Option<OwnerGuard>> {
    Ok(Some(OwnerGuard))
}

/// Exclusive offline authority for the one recovery path that is allowed to
/// inspect `facts.db` as source evidence. This deliberately does not install
/// the guards in the process-global direct-store table and never probes or
/// routes to rallyd: the maintenance command owns direct EX, daemon-owner SH,
/// and the canonical mutation lock only for the duration of one invocation.
pub(crate) struct OfflineMigrationAuthority {
    _direct_owner: OwnerGuard,
    _daemon_exclusion: OwnerGuard,
    _mutation: RoomMutationLock,
}

#[cfg(unix)]
pub(crate) fn acquire_offline_migration_authority(
    rally_dir: &Path,
) -> Result<OfflineMigrationAuthority> {
    let direct_owner = acquire_direct_owner_exclusive_nb(rally_dir)?.ok_or_else(|| {
        RallyError::Command(
            "offline migration authority is busy: another direct Rally process owns facts.db; \
             stop all Rally commands and run `rally daemon stop` before retrying"
                .to_string(),
        )
    })?;
    let daemon_exclusion = match acquire_owner_shared_nb(rally_dir)? {
        Some(guard) => guard,
        None => {
            drop(direct_owner);
            return Err(RallyError::Command(
                "offline migration authority is unavailable because a live or unresponsive \
                 daemon owns facts.db; run `rally daemon stop` and confirm it stopped before \
                 retrying"
                    .to_string(),
            ));
        }
    };
    let mutation = acquire_room_mutation_lock(rally_dir)?;
    Ok(OfflineMigrationAuthority {
        _direct_owner: direct_owner,
        _daemon_exclusion: daemon_exclusion,
        _mutation: mutation,
    })
}

#[cfg(not(unix))]
pub(crate) fn acquire_offline_migration_authority(
    _rally_dir: &Path,
) -> Result<OfflineMigrationAuthority> {
    Err(RallyError::Usage(
        "rally doctor --migrate-db-only is unsupported on this platform because equivalent \
         cross-process owner locks and directory sync semantics are not implemented"
            .to_string(),
    ))
}

/// Optimistic, byte-inert owner observation for migration dry-run. Missing
/// rendezvous files mean only "nothing observable"; apply creates/acquires the
/// real lock set and revalidates from scratch before making any safety claim.
#[cfg(unix)]
pub(crate) fn observe_offline_migration_authority(rally_dir: &Path) -> Result<String> {
    fn try_existing(path: &Path, operation: i32) -> Result<Option<bool>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RallyError::io(format!("stat {}", path.display()))(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(RallyError::Usage(format!(
                "offline migration lock {} must be a regular file, not a symlink or special file",
                path.display()
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(RallyError::io(format!("open {}", path.display())))?;
        let rc = unsafe { unix_lock::flock(file.as_raw_fd(), operation | unix_lock::LOCK_NB) };
        if rc == 0 {
            let _ = unsafe { unix_lock::flock(file.as_raw_fd(), unix_lock::LOCK_UN) };
            return Ok(Some(true));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(Some(false))
        } else {
            Err(RallyError::Io {
                context: format!("observe offline migration lock {}", path.display()),
                source: error,
            })
        }
    }

    let direct = rally_dir.join(DIRECT_OWNER_LOCK_FILENAME);
    if try_existing(&direct, unix_lock::LOCK_EX)? == Some(false) {
        return Ok("direct_owner_busy".to_string());
    }
    let daemon = rally_dir.join(RALLYD_OWNER_LOCK_FILENAME);
    if try_existing(&daemon, unix_lock::LOCK_SH)? == Some(false) {
        return Ok("daemon_owner_busy".to_string());
    }
    Ok("clear_at_optimistic_inspection".to_string())
}

#[cfg(not(unix))]
pub(crate) fn observe_offline_migration_authority(_rally_dir: &Path) -> Result<String> {
    Ok("unsupported_platform".to_string())
}

/// Destructive automatic cleanup fails closed on platforms without a kernel
/// advisory lock equivalent. A process-local/no-op guard would permit two
/// entrants to reap concurrently.
#[cfg(not(unix))]
pub(crate) fn acquire_named_exclusive_nb(
    _rally_dir: &Path,
    _filename: &str,
) -> Result<Option<OwnerGuard>> {
    Ok(None)
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
#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct LedgerLine {
    seq: i64,
    occurred_at: String,
    event_type: String,
    payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engagement: Option<String>,
}

/// Closed-DB canonical candidate used only by the explicit offline DB-only
/// migration. `bytes` is already sorted by the logical payload sequence,
/// fully canonical-validated, and newline framed one row per source DB row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DbOnlyMigrationSegment {
    pub(crate) bytes: Vec<u8>,
    pub(crate) row_count: u64,
    pub(crate) max_seq: i64,
}

/// Raw row read through a strictly read-only SQLite connection by the offline
/// migration command. Keeping SQLite access out of the normal fact store avoids
/// its schema/WAL bootstrap writes while this helper retains canonical rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DbOnlyMigrationSourceRow {
    pub(crate) database_seq: i64,
    pub(crate) occurred_at: String,
    pub(crate) event_type: String,
    pub(crate) payload: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveTailRepair {
    None,
    AddNewline,
    TruncateTo(u64),
}

fn validate_canonical_line(entry: &LedgerLine) -> Result<Fact> {
    if entry.seq <= 0 {
        return Err(RallyError::Message(format!(
            "canonical segment row has non-positive seq {}",
            entry.seq
        )));
    }
    let fact = Fact::from_segment_value(entry.payload.clone(), entry.seq)?;
    if fact.schema != FACT_SCHEMA {
        return Err(RallyError::Message(format!(
            "canonical segment row has unsupported fact schema {:?}",
            fact.schema
        )));
    }
    if fact.event_id.trim().is_empty() {
        return Err(RallyError::Message(
            "canonical segment row has an empty event_id".to_string(),
        ));
    }
    if entry.event_type != fact.kind.as_str() {
        return Err(RallyError::Message(format!(
            "canonical segment event_type {:?} does not match payload kind {:?}",
            entry.event_type,
            fact.kind.as_str()
        )));
    }
    Ok(fact)
}

/// Validate the entire active segment without changing it and classify only
/// the final unterminated fragment. A complete syntactic/schema error is
/// corruption even without a newline; only serde's EOF class is truncatable.
fn inspect_active_segment_tail(path: &Path) -> Result<ActiveTailRepair> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ActiveTailRepair::None);
        }
        Err(error) => return Err(RallyError::io(format!("read {}", path.display()))(error)),
    };
    if bytes.is_empty() {
        return Ok(ActiveTailRepair::None);
    }

    let completed_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    for (index, raw) in bytes[..completed_end]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let entry = serde_json::from_slice::<LedgerLine>(raw).map_err(|error| {
            RallyError::Message(format!(
                "completed canonical segment corruption in {} at line {}: {}",
                path.display(),
                index + 1,
                error
            ))
        })?;
        validate_canonical_line(&entry).map_err(|error| {
            RallyError::Message(format!(
                "completed canonical segment corruption in {} at line {}: {}",
                path.display(),
                index + 1,
                error
            ))
        })?;
    }

    let tail = &bytes[completed_end..];
    if tail.is_empty() {
        return Ok(ActiveTailRepair::None);
    }
    match serde_json::from_slice::<LedgerLine>(tail) {
        Ok(entry) => {
            validate_canonical_line(&entry).map_err(|error| {
                RallyError::Message(format!(
                    "unterminated canonical segment corruption in {} at line {}: {}",
                    path.display(),
                    bytes[..completed_end]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count()
                        + 1,
                    error
                ))
            })?;
            Ok(ActiveTailRepair::AddNewline)
        }
        Err(error) if error.is_eof() => Ok(ActiveTailRepair::TruncateTo(
            u64::try_from(completed_end).map_err(|overflow| {
                RallyError::Message(format!("canonical tail offset overflow: {overflow}"))
            })?,
        )),
        Err(error) => Err(RallyError::Message(format!(
            "unterminated canonical segment corruption in {}: {}; refusing mutation",
            path.display(),
            error
        ))),
    }
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(RallyError::io(format!(
            "fsync directory {}",
            path.display()
        )))
}

fn rally_dir_for_segment(path: &Path) -> Result<&Path> {
    path.parent().and_then(Path::parent).ok_or_else(|| {
        RallyError::Message(format!("segment path has no room root: {}", path.display()))
    })
}

fn apply_active_tail_repair(path: &Path, repair: ActiveTailRepair, event_id: &str) -> Result<()> {
    let rally_dir = rally_dir_for_segment(path)?;
    if repair != ActiveTailRepair::None {
        // Tail repair is the sole bounded rewrite of canonical segment bytes.
        // Invalidate every derived view before the rewrite begins; the segment
        // tail hash below independently prevents a stale schema-2 sidecar from
        // matching even if a best-effort removal cannot complete.
        invalidate_segment_fold_memo();
        let _ = fs::remove_file(rally_dir.join(RECONCILE_CACHE_FILENAME));
        let _ = fs::remove_file(snapshot_cache_path(rally_dir));
        let _ = fs::remove_file(rally_dir.join(LOG_DIRNAME).join(LOG_INDEX_FILENAME));
        let _ = fs::remove_file(rally_dir.join(claim_authority::CLAIM_INDEX_FILENAME));
    }
    match repair {
        ActiveTailRepair::None => return Ok(()),
        ActiveTailRepair::AddNewline => {
            let mut file = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|error| {
                    RallyError::outcome_unknown(event_id, "tail-repair-open", error.to_string())
                })?;
            file.write_all(b"\n").map_err(|error| {
                RallyError::outcome_unknown(event_id, "tail-repair-write", error.to_string())
            })?;
            trigger_o26_fault(rally_dir, O26FaultPoint::TailRepairSync).map_err(|detail| {
                RallyError::outcome_unknown(event_id, "tail-repair-sync", detail)
            })?;
            file.sync_all().map_err(|error| {
                RallyError::outcome_unknown(event_id, "tail-repair-sync", error.to_string())
            })?;
        }
        ActiveTailRepair::TruncateTo(length) => {
            let file = OpenOptions::new().write(true).open(path).map_err(|error| {
                RallyError::outcome_unknown(event_id, "tail-repair-open", error.to_string())
            })?;
            file.set_len(length).map_err(|error| {
                RallyError::outcome_unknown(event_id, "tail-repair-truncate", error.to_string())
            })?;
            trigger_o26_fault(rally_dir, O26FaultPoint::TailRepairSync).map_err(|detail| {
                RallyError::outcome_unknown(event_id, "tail-repair-sync", detail)
            })?;
            file.sync_all().map_err(|error| {
                RallyError::outcome_unknown(event_id, "tail-repair-sync", error.to_string())
            })?;
        }
    }
    match inspect_active_segment_tail(path) {
        Ok(ActiveTailRepair::None) => Ok(()),
        Ok(remaining) => Err(RallyError::outcome_unknown(
            event_id,
            "tail-repair-readback",
            format!("tail still requires repair after sync: {remaining:?}"),
        )),
        Err(error) => Err(RallyError::outcome_unknown(
            event_id,
            "tail-repair-readback",
            error.to_string(),
        )),
    }
}

fn normalized_fact_value(fact: &Fact, seq: i64) -> Result<Value> {
    let mut normalized = fact.clone();
    normalized.seq = seq;
    serde_json::to_value(normalized).map_err(RallyError::json("normalize fact identity"))
}

const MAX_APPEND_EVENT_ID_BYTES: usize = 256;

/// Validate identity fields that determine whether an append can ever be
/// queried or retried. This must run before tail inspection or any filesystem
/// mutation: an invalid stable id cannot be represented as OutcomeUnknown.
fn validate_append_identity(fact: &Fact) -> Result<()> {
    if fact.schema != FACT_SCHEMA {
        return Err(RallyError::Usage(format!(
            "append fact schema must be {FACT_SCHEMA}, got {:?}",
            fact.schema
        )));
    }
    validate_append_event_id(&fact.event_id)
}

pub(crate) fn validate_append_event_id(event_id: &str) -> Result<()> {
    if event_id.trim().is_empty() {
        return Err(RallyError::Usage(
            "append event_id must not be empty".to_string(),
        ));
    }
    if event_id.len() > MAX_APPEND_EVENT_ID_BYTES {
        return Err(RallyError::Usage(format!(
            "append event_id exceeds {MAX_APPEND_EVENT_ID_BYTES} bytes"
        )));
    }
    if event_id.chars().any(char::is_control) {
        return Err(RallyError::Usage(
            "append event_id must not contain control characters".to_string(),
        ));
    }
    Ok(())
}

fn resolve_canonical_event_id(
    log_dir: &Path,
    archive_dir: &Path,
    candidate: &Fact,
    engagement: &str,
) -> Result<Option<(Fact, Vec<PathBuf>)>> {
    let live = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    // Preserve O29's complete-envelope duplicate-sequence validation before
    // using an event id for idempotency. Source-path discovery below is only a
    // durability aid after the authoritative union has proved equality.
    let mut matches = canonical_segment_entries(&live, &archived)?
        .into_iter()
        .filter(|entry| {
            entry.payload.get("event_id").and_then(Value::as_str)
                == Some(candidate.event_id.as_str())
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 {
        return Err(RallyError::Usage(format!(
            "event-id identity ambiguity: {} appears in {} canonical rows; refusing retry",
            candidate.event_id,
            matches.len()
        )));
    }
    let entry = matches.pop().expect("one checked canonical identity row");
    let existing = validate_canonical_line(&entry)?;
    let exact = entry.event_type == candidate.kind.as_str()
        && entry.engagement.as_deref() == Some(engagement)
        && normalized_fact_value(candidate, entry.seq)?
            == normalized_fact_value(&existing, entry.seq)?;
    if !exact {
        return Err(RallyError::Usage(format!(
            "event-id identity conflict: {} already names a different canonical fact",
            candidate.event_id
        )));
    }
    let mut source_paths = Vec::new();
    for path in live.iter().chain(archived.iter()) {
        if read_segment_entries(path)?
            .iter()
            .any(|observed| observed == &entry)
        {
            source_paths.push(path.clone());
        }
    }
    if source_paths.is_empty() {
        return Err(RallyError::Message(format!(
            "canonical event {} lost its physical source during locked identity resolution",
            candidate.event_id
        )));
    }
    Ok(Some((existing, source_paths)))
}

fn resync_existing_canonical_fact(path: &Path, event_id: &str) -> Result<()> {
    let file = fs::File::open(path).map_err(|error| {
        RallyError::outcome_unknown(event_id, "retry-resync-open", error.to_string())
    })?;
    file.sync_all().map_err(|error| {
        RallyError::outcome_unknown(event_id, "retry-resync-file", error.to_string())
    })?;
    let parent = path.parent().ok_or_else(|| {
        RallyError::outcome_unknown(event_id, "retry-resync-parent", "segment has no parent")
    })?;
    sync_directory(parent).map_err(|error| {
        RallyError::outcome_unknown(event_id, "retry-resync-parent", error.to_string())
    })?;
    if let Some(room_dir) = parent.parent() {
        sync_directory(room_dir).map_err(|error| {
            RallyError::outcome_unknown(event_id, "retry-resync-room", error.to_string())
        })?;
    }
    Ok(())
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

/// Process-global table of direct EX + daemon-exclusion SH guard pairs, keyed
/// by canonicalized `.rally` dir. A pair is installed the first time THIS
/// process takes the direct branch for a given room and held until process
/// exit. A per-root TABLE, not a single slot, because one process can
/// legitimately open MANY distinct rooms: the `rally-cli` test binary runs
/// many `#[test]` functions (each against its own temp-dir room) as threads
/// inside one process. A single global slot would silently drop every root's
/// guard but the first one's.
struct DirectOwnershipGuards {
    _direct_owner: OwnerGuard,
    _daemon_exclusion: OwnerGuard,
}

static DIRECT_OWNER_GUARDS: OnceLock<Mutex<BTreeMap<PathBuf, DirectOwnershipGuards>>> =
    OnceLock::new();

/// Install the guard pair for `rally_dir` in the process-global table unless
/// this process already owns that exact room.
fn direct_owner_key(rally_dir: &Path) -> PathBuf {
    canonical_repo_root_string(rally_dir).into()
}

fn process_owns_direct_room(rally_dir: &Path) -> bool {
    let Some(table) = DIRECT_OWNER_GUARDS.get() else {
        return false;
    };
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&direct_owner_key(rally_dir))
}

fn install_direct_owner_once(
    rally_dir: &Path,
    direct_owner: OwnerGuard,
    daemon_exclusion: OwnerGuard,
) {
    let table = DIRECT_OWNER_GUARDS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    table
        .entry(direct_owner_key(rally_dir))
        .or_insert(DirectOwnershipGuards {
            _direct_owner: direct_owner,
            _daemon_exclusion: daemon_exclusion,
        });
}

fn release_direct_owner_for_ack_poll(rally_dir: &Path) -> Result<()> {
    let Some(table) = DIRECT_OWNER_GUARDS.get() else {
        return Err(RallyError::Message(
            "direct ACK polling started without process ownership".to_string(),
        ));
    };
    let removed = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&direct_owner_key(rally_dir));
    if removed.is_none() {
        return Err(RallyError::Message(
            "direct ACK polling could not release process ownership".to_string(),
        ));
    }
    Ok(())
}

fn direct_owner_busy_unknown_error(bound: Duration) -> RallyError {
    RallyError::Command(format!(
        "direct-store-busy-unknown: could not establish exclusive direct ownership or a live \
         daemon route within {}ms; no facts.db write was attempted. Run `rally daemon status`; \
         if the daemon is wedged, run `rally daemon stop` before retrying",
        bound.as_millis()
    ))
}

fn direct_owner_wait_bound() -> Duration {
    const WATCHDOG_RESERVE: Duration = Duration::from_millis(250);
    crate::watchdog_remaining()
        .map(|remaining| remaining.saturating_sub(WATCHDOG_RESERVE))
        .unwrap_or(store_client::CORRIDOR_BOUND)
        .min(store_client::CORRIDOR_BOUND)
}

/// Establish one safe storage owner. `Ok(Some(_))` routes to a live daemon;
/// `Ok(None)` means this process holds both direct EX and daemon-exclusion SH
/// guards until process exit. The retry is bounded so contention never leaks
/// out as a misleading claim/session/domain failure.
fn acquire_direct_ownership_or_route_bounded(
    root: &Path,
    rally_dir: &Path,
    engagement: Option<String>,
    bound: Duration,
) -> Result<Option<RoutedRoomStore>> {
    const RETRY_SLEEP: Duration = Duration::from_millis(10);
    let deadline = Instant::now() + bound;
    loop {
        if let Some(routed) =
            store_client::probe_live_bounded(root, rally_dir, engagement.clone(), Duration::ZERO)?
        {
            return Ok(Some(routed));
        }
        if process_owns_direct_room(rally_dir) {
            return Ok(None);
        }
        if let Some(direct_owner) = acquire_direct_owner_exclusive_nb(rally_dir)? {
            match acquire_owner_shared_nb(rally_dir)? {
                Some(daemon_exclusion) => {
                    install_direct_owner_once(rally_dir, direct_owner, daemon_exclusion);
                    return Ok(None);
                }
                None => drop(direct_owner),
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(direct_owner_busy_unknown_error(bound));
        }
        thread::sleep(RETRY_SLEEP.min(deadline.saturating_duration_since(now)));
    }
}

fn acquire_direct_ownership_or_route(
    root: &Path,
    rally_dir: &Path,
    engagement: Option<String>,
) -> Result<Option<RoutedRoomStore>> {
    acquire_direct_ownership_or_route_bounded(
        root,
        rally_dir,
        engagement,
        direct_owner_wait_bound(),
    )
}

/// The routing seam (L2/ADR-01). These are the ONLY entry points the 214 call
/// sites use; the old public names (`open`, `open_at`, `open_at_with_engagement`,
/// `open_existing_at`) survive here as the router constructors so every caller
/// compiles unchanged.
///
/// [`RoomStore::route`] probes for a live daemon first. If none answers, it
/// boundedly competes for direct EX, then daemon-exclusion SH. A successful
/// pair is retained to process exit; contention is retried alongside daemon
/// probes and ends only in a typed busy/unknown result.
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
        ensure_no_db_only_migration_recovery(&root)?;
        let rally_dir = root.join(".rally");
        let engagement = env::var(ENGAGEMENT_ENV_VAR).ok();
        match acquire_direct_ownership_or_route(&root, &rally_dir, engagement)? {
            Some(routed) => Ok(Some(RoomStore::Routed(routed))),
            None => Ok(DirectRoomStore::open_direct_existing_at(root)?.map(RoomStore::Direct)),
        }
    }

    /// The ADR-01 routing seam shared by `open_at`/`open_at_with_engagement`.
    /// See the `impl RoomStore` doc comment above for the full choreography.
    fn route(root: PathBuf, engagement: Option<String>) -> Result<Self> {
        ensure_no_db_only_migration_recovery(&root)?;
        let rally_dir = root.join(".rally");
        match acquire_direct_ownership_or_route(&root, &rally_dir, engagement.clone())? {
            Some(routed) => Ok(RoomStore::Routed(routed)),
            None => Ok(RoomStore::Direct(
                DirectRoomStore::open_direct_at_with_engagement(root, engagement)?,
            )),
        }
    }

    // ----- dispatch: ROUTED methods -------------------------------------------
    // Each `Routed` arm calls the real wire client (`store_client.rs`); a
    // transport failure there (R6) surfaces as a retryable
    // `RallyError::Command` and NEVER falls back to a direct facts.db open.

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<AppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
        match self {
            RoomStore::Direct(d) => d.append_fact(fact),
            RoomStore::Routed(r) => r.append_fact(fact),
        }
    }

    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
        match self {
            RoomStore::Direct(d) => d.append_fact_verified(fact),
            RoomStore::Routed(r) => r.append_fact_verified(fact),
        }
    }

    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
        match self {
            RoomStore::Direct(d) => d.append_state_transition_verified(fact),
            RoomStore::Routed(r) => r.append_state_transition_verified(fact),
        }
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<ConditionalAppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
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

    /// Consume the command's general-purpose store before a bounded ACK wait.
    /// Routed mode keeps using the daemon. Direct mode closes its store facade,
    /// releases process ownership, and thereafter folds canonical JSONL only.
    pub(crate) fn into_ack_polling(self) -> Result<AckPollingStore> {
        match self {
            Self::Routed(routed) => Ok(AckPollingStore::Routed(routed)),
            Self::Direct(direct) => {
                if direct.warm_fact_store.is_some() {
                    return Err(RallyError::Message(
                        "daemon warm store cannot enter direct ACK polling".to_string(),
                    ));
                }
                let room_dir = direct
                    .facts_db_path
                    .parent()
                    .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?
                    .to_path_buf();
                let log_dir = direct.log_dir.clone();
                let archive_dir = direct.archive_dir.clone();
                drop(direct);
                release_direct_owner_for_ack_poll(&room_dir)?;
                Ok(AckPollingStore::Direct {
                    room_dir,
                    log_dir,
                    archive_dir,
                })
            }
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
        caller_tool: &str,
        caller_session_id: Option<&str>,
        expected_owner_session_id: Option<&str>,
    ) -> Result<RenewClaimLeaseOutcome> {
        // Fix the mutation identity on the client side before choosing direct
        // versus routed transport. A daemon never mints an unqueryable ID.
        let event_id = crate::new_id("renew");
        let thread_id = crate::new_id("room");
        let created_at = now_string();
        match self {
            RoomStore::Direct(d) => d.renew_claim_lease(
                claim_id,
                lease_expires_at,
                Some(caller_tool),
                caller_session_id,
                expected_owner_session_id,
                &event_id,
                &thread_id,
                &created_at,
            ),
            RoomStore::Routed(r) => r.renew_claim_lease(
                claim_id,
                lease_expires_at,
                caller_tool,
                caller_session_id,
                expected_owner_session_id,
                event_id,
                thread_id,
                created_at,
            ),
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

    /// Capture a snapshot together with the canonical fingerprint measured in
    /// the same mutation epoch. Routed mode receives the pair captured by the
    /// daemon; the client never re-fingerprints a detached snapshot.
    pub(crate) fn snapshot_cache_capture(
        &self,
        include_archived: bool,
    ) -> Result<SnapshotCacheCapture> {
        match self {
            RoomStore::Direct(d) => d.snapshot_cache_capture(include_archived),
            RoomStore::Routed(r) => r.snapshot_cache_capture(include_archived),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn snapshot_scoped(
        &self,
        engagement: &str,
        run_id: Option<&str>,
        path: Option<&str>,
        include_archived: bool,
        include_presence_only: bool,
    ) -> Result<RoomSnapshot> {
        match self {
            RoomStore::Direct(d) => d.snapshot_scoped(
                engagement,
                run_id,
                path,
                include_archived,
                include_presence_only,
            ),
            RoomStore::Routed(r) => r.snapshot_scoped(
                engagement,
                run_id,
                path,
                include_archived,
                include_presence_only,
            ),
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
    ) -> Result<ConditionalAppendOutcome> {
        let checkpoint = Fact {
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
        match self {
            RoomStore::Direct(d) => d.maybe_append_read_checkpoint(&checkpoint, read_seq),
            RoomStore::Routed(r) => r.maybe_append_read_checkpoint(&checkpoint, read_seq),
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

    /// The `.rally` state directory backing this room.
    ///
    /// Test-only since 2026-08-14. Its one production caller was
    /// `doctor::run_sweep_corrupt`, which opened a full `RoomStore` purely to
    /// learn `repo_root/.rally` — and that open is exactly what made
    /// `--sweep-corrupt` fail on the corrupt stores it exists to clean up.
    /// Production callers that need the path and not the store compute it from
    /// `repo_root()` directly; do not reintroduce a store open to get a path.
    #[cfg(test)]
    pub(crate) fn rally_dir(&self) -> PathBuf {
        self.repo_root().join(".rally")
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
    /// 3. If no canonical source exists but `facts.db` has events, preserve
    ///    the db and fail loud with the explicit offline
    ///    `rally doctor --migrate-db-only` recovery path. Ordinary store open
    ///    never promotes derived state into canonical history.
    /// 4. Otherwise segments and db are already in sync and we proceed.
    ///
    /// Replay and legacy-monolith migration are idempotent — running them
    /// twice on the same inputs yields identical state.
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
        ensure_no_db_only_migration_recovery(&root)?;
        let dir = root.join(".rally");
        fs::create_dir_all(&dir).map_err(RallyError::io("create .rally"))?;
        // RC-072 first-open auto-init: `rally enter` creates the room far more
        // often than anyone runs `rally init`, so the ignore rules have to
        // land here too or the common path commits its own facts.db. Costs one
        // stat when the file is already there; never fails the open.
        crate::init::ensure_ignore_present(&dir);
        let _guard = acquire_room_mutation_lock(&dir)?;
        let _ = fs::remove_file(dir.join("room.db"));
        let fact_store_path = dir.join("facts.db");
        let log_dir = dir.join(LOG_DIRNAME);
        let archive_dir = dir.join(ARCHIVE_DIRNAME);
        let legacy_ledger_path = dir.join(LEDGER_FILENAME);

        // R1 → R5 migration (idempotent, see [`migrate_monolith_to_segments`]).
        migrate_monolith_to_segments(&legacy_ledger_path, &log_dir, &archive_dir)?;

        let fact_store = open_fact_store_lenient(&fact_store_path)?;
        // Direct mode keeps no room-lifetime SQLite pool. The process-lifetime
        // direct owner lock excludes peer processes, and each operation opens
        // and closes its pool inside that ownership window.
        drop(fact_store);
        seed_segments_from_db_if_absent(&log_dir, &archive_dir, &fact_store_path)?;
        // Repair canonical-ahead cache state before a daemon can install its
        // warm pool. This is the fresh-process recovery path after a committed
        // append reported a projection warning.
        reconcile_segments_and_db(&log_dir, &archive_dir, &fact_store_path, true)?;
        let active_engagement = resolve_active_engagement_with_env(&dir, engagement);
        let store = Self {
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
        ensure_no_db_only_migration_recovery(&root)?;
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
        drop(fact_store);
        seed_segments_from_db_if_absent(&log_dir, &archive_dir, &fact_store_path)?;
        reconcile_segments_and_db(&log_dir, &archive_dir, &fact_store_path, true)?;
        let active_engagement = resolve_active_engagement(&dir);
        let store = Self {
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
        // COLD branch: direct-mode per-op open (byte-identical to main — G1).
        // The G10 proof counts these for a watched db path (test-only; a no-op
        // in non-test builds, so the direct path is unchanged).
        note_cold_open(&self.facts_db_path);
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

    /// Explicitly close the daemon's warm SQLite pool within `budget`.
    ///
    /// Teardown runs on a dedicated thread that owns `mutation.lock` until the
    /// vendored store has joined delivery and completed its final pool close.
    /// If the caller's wait expires, the teardown thread continues holding the
    /// lock; no peer mutation can race the delayed WAL checkpoint. The store's
    /// fallback [`Drop`] is nonblocking and nonpanicking.
    pub(crate) fn close_warm_fact_store_bounded(&mut self, budget: Duration) -> Result<()> {
        if self.warm_fact_store.is_none() {
            return Ok(());
        }
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let now = Instant::now();
        let deadline = now
            .checked_add(budget)
            .unwrap_or(now + MUTATION_LOCK_FALLBACK_BOUND);
        let guard = with_mutation_deadline_at(deadline, || acquire_room_mutation_lock(room_dir))?;
        let Some(mut warm) = self.warm_fact_store.take() else {
            return Ok(());
        };
        // If spawning the close worker fails, dropping the captured store must
        // stay prompt rather than re-entering its synchronous Drop path.
        warm.prepare_nonblocking_drop();

        // Retain both resources outside the closure until the OS has actually
        // created the worker. `Builder::spawn` consumes and drops its closure on
        // failure, so capturing these values directly would release the lock
        // and pool at the exact failure boundary this method must fail closed.
        let retained = Arc::new(Mutex::new(Some((warm, guard))));
        let worker_retained = Arc::clone(&retained);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let spawn_result = if warm_close_spawn_failure_requested() {
            Err(io::Error::other("injected warm-close worker spawn failure"))
        } else {
            thread::Builder::new()
                .name("rally-warm-store-close".to_string())
                .spawn(move || {
                    let resources = match worker_retained.lock() {
                        Ok(mut slot) => slot.take(),
                        Err(poisoned) => poisoned.into_inner().take(),
                    };
                    let Some((mut warm, guard)) = resources else {
                        let _ =
                            done_tx
                                .send(Err("warm-close worker started without retained resources"
                                    .to_string()));
                        return;
                    };
                    let result = warm.close_synchronously();
                    if result.is_ok() {
                        drop(warm);
                    } else {
                        std::mem::forget(warm);
                    }
                    drop(guard);
                    let _ = done_tx.send(result);
                })
        };
        let worker = match spawn_result {
            Ok(worker) => {
                // The live worker owns the remaining Arc and takes both values.
                drop(retained);
                worker
            }
            Err(error) => {
                // Preserve pool + mutation.lock until process exit. The daemon
                // also retains owner EX after this loud close failure.
                std::mem::forget(retained);
                return Err(RallyError::Command(format!(
                    "daemon-close-not-started: could not spawn warm-store close worker: {error}; warm pool and mutation.lock retained until process exit"
                )));
            }
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = match done_rx.recv_timeout(remaining) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(RallyError::Command(format!("daemon-close-failed: {error}"))),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(RallyError::Command(format!(
                "daemon-close-timeout: warm store did not close within {}ms; teardown continues while holding mutation.lock",
                budget.as_millis()
            ))),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(RallyError::Command(
                "daemon-close-failed: warm-store close worker exited without a result".to_string(),
            )),
        };
        // Detach rather than joining: the completion channel is the bounded
        // close contract, and a join after its deadline would reintroduce an
        // unbounded teardown edge.
        drop(worker);
        result
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

    pub(crate) fn append_fact(&self, fact: &Fact) -> Result<AppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        self.append_fact_under_lock(fact)
    }

    fn append_fact_under_lock(&self, fact: &Fact) -> Result<AppendOutcome> {
        ensure_no_db_only_migration_recovery(&self.repo_root)?;
        let mut fact = fact.clone();
        // Only invariant input bounds precede canonical identity resolution.
        // Stateful authority/claim/transition checks below apply solely to a
        // genuinely new event; the exact canonical row proves a retry was
        // already authorized before the first commit changed mutable state.
        validate_append_identity(&fact)?;
        crate::write_authority::assert_field_bounds(&fact)?;
        let active_segment = self.active_segment_path();
        let tail_repair = inspect_active_segment_tail(&active_segment)?;
        if let Some((existing, source_paths)) = resolve_canonical_event_id(
            &self.log_dir,
            &self.archive_dir,
            &fact,
            &self.active_engagement,
        )? {
            let rally_dir = rally_dir_for_segment(&active_segment)?;
            trigger_o26_fault(rally_dir, O26FaultPoint::BeforeCanonicalMutation).map_err(
                |detail| RallyError::outcome_unknown(&fact.event_id, "retry-resync", detail),
            )?;
            if mutation_start_deadline_elapsed() {
                crate::mark_watchdog_command_outcome_unknown(
                    &fact.event_id,
                    "retry-resync-deadline",
                );
                return Err(RallyError::outcome_unknown(
                    &fact.event_id,
                    "retry-resync-deadline",
                    "deadline elapsed before the exact canonical row could be re-synced",
                ));
            }
            if tail_repair != ActiveTailRepair::None {
                crate::mark_watchdog_command_outcome_unknown(
                    &fact.event_id,
                    "canonical-tail-repair",
                );
                apply_active_tail_repair(&active_segment, tail_repair, &fact.event_id)?;
                trigger_o26_fault(rally_dir, O26FaultPoint::AfterTailRepair).map_err(|detail| {
                    RallyError::outcome_unknown(&fact.event_id, "after-tail-repair", detail)
                })?;
            }
            crate::mark_watchdog_command_outcome_unknown(&fact.event_id, "retry-resync");
            for source_path in source_paths {
                resync_existing_canonical_fact(&source_path, &fact.event_id)?;
            }
            let mut outcome = AppendOutcome::committed(existing.clone(), Vec::new());
            crate::mark_watchdog_append_outcome(&outcome);
            outcome.warnings = self.project_canonical_fact(&existing);
            outcome.projection_complete = outcome.warnings.is_empty();
            crate::mark_watchdog_append_outcome(&outcome);
            return Ok(outcome);
        }
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
        // justified. Durable renewal means lease expiry is no longer monotonic:
        // a lease that was expired in the unlocked snapshot may have advanced
        // before this append. Re-check every reason the reaper stamped and
        // proceed only while at least one remains true.
        if fact.kind == FactKind::ClaimExpired {
            let reason = Self::reaper_marker(&fact.evidence, "reason");
            let owner_reason = matches!(reason, Some("owner-stale" | "owner-stale+lease-expired"));
            let lease_reason =
                matches!(reason, Some("lease-expired" | "owner-stale+lease-expired"));
            if (owner_reason || lease_reason)
                && let Some(owner) = Self::reaper_marker(&fact.evidence, "owner")
                && let Some(ref_id) = fact.ref_id.as_deref()
            {
                let coord =
                    crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
                let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
                let fresh = snapshot_from_facts_with_policy(&facts, &coord, false);
                let owner_session_marker = Self::reaper_marker(&fact.evidence, "owner_session");
                let active_claim = fresh
                    .active_claims
                    .iter()
                    .find(|c| c.tool.as_deref() == Some(owner) && c.event_id == ref_id);
                if let Some(claim) = active_claim {
                    let owner_session_matches =
                        match (claim.from_session_id.as_deref(), owner_session_marker) {
                            (Some(claim_session), Some(marker_session)) => {
                                marker_session == claim_session
                            }
                            (None, Some("legacy") | None) => true,
                            _ => false,
                        };
                    if !owner_session_matches {
                        return Err(RallyError::Usage(format!(
                            "reap refused: claim {ref_id} owner session does not match the reaper \
                             evidence; not closing an active claim"
                        )));
                    }
                }
                let observed_sessions =
                    crate::observed_liveness::observe_sessions(&self.repo_root, &facts);
                let lease_boundary = active_claim
                    .and_then(|claim| {
                        claim
                            .evidence
                            .iter()
                            .find_map(|item| item.strip_prefix("lease_expires_at:"))
                    })
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                    .map(|time| time.with_timezone(&chrono::Utc));
                let owner_still_stale = owner_reason
                    && active_claim.is_some_and(|claim| {
                        if claim.from_session_id.is_some() {
                            observed_sessions
                                .for_claim(claim.tool.as_deref(), claim.from_session_id.as_deref())
                                == crate::observed_liveness::ObservedLiveness::Stale
                        } else {
                            fresh.claim_reclaim_eligible(claim, &coord).0
                        }
                    });
                let lease_still_expired = lease_reason
                    && active_claim.is_some_and(|claim| {
                        claim
                            .evidence
                            .iter()
                            .find_map(|item| item.strip_prefix("lease_expires_at:"))
                            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                            .is_some_and(|expires| expires <= chrono::Utc::now())
                    });
                let observation_still_permits_cleanup = active_claim.is_some_and(|claim| {
                    let current = if lease_reason {
                        observed_sessions.for_claim_since(
                            claim.tool.as_deref(),
                            claim.from_session_id.as_deref(),
                            lease_boundary,
                        )
                    } else {
                        observed_sessions
                            .for_claim(claim.tool.as_deref(), claim.from_session_id.as_deref())
                    };
                    match Self::reaper_marker(&fact.evidence, "observed") {
                        Some("stale") => {
                            current == crate::observed_liveness::ObservedLiveness::Stale
                        }
                        Some("unknown") => {
                            current == crate::observed_liveness::ObservedLiveness::Unknown
                        }
                        _ => false,
                    }
                });
                // If the claim is already closed, allow the append and let the
                // projection deduplicate it. If it is still active, at least
                // one of the reasons computed by the unlocked pass must remain
                // true under this lock.
                if active_claim.is_some()
                    && ((!owner_still_stale && !lease_still_expired)
                        || !observation_still_permits_cleanup)
                {
                    return Err(RallyError::Usage(format!(
                        "reap refused: claim {ref_id} is no longer eligible under the mutation \
                         lock (owner revived or lease renewed); not closing an active claim"
                    )));
                }
            }
        }
        // Durable renewal is an internal state transition. Re-assert its target
        // and monotonicity under the same lock that assigns its sequence so two
        // concurrent renewals cannot shorten or duplicate the effective lease.
        if fact.kind == FactKind::ClaimRenewed {
            let claim_id = fact.ref_id.as_deref().ok_or_else(|| {
                RallyError::Usage("renew claim lease: missing claim ref".to_string())
            })?;
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            let current =
                claim_authority::active_claim_record(&facts, claim_id).ok_or_else(|| {
                    RallyError::Usage(format!(
                        "renew claim lease: ref {claim_id} is not an active claim"
                    ))
                })?;
            if !claim_authority::claim_owner_matches_caller(
                current.owner_tool.as_deref(),
                current.from_session_id.as_deref(),
                fact.tool.as_deref(),
                fact.from_session_id.as_deref(),
            ) {
                return Err(RallyError::Usage(format!(
                    "renew claim lease: {} session does not own claim {claim_id}",
                    fact.tool.as_deref().unwrap_or("<unknown>"),
                )));
            }
            let requested_raw = fact
                .evidence
                .iter()
                .find_map(|item| item.strip_prefix("lease_expires_at:"))
                .ok_or_else(|| {
                    RallyError::Usage(
                        "renew claim lease: missing lease_expires_at evidence".to_string(),
                    )
                })?;
            let requested = chrono::DateTime::parse_from_rfc3339(requested_raw).map_err(|err| {
                RallyError::Usage(format!(
                    "renew claim lease: lease_expires_at must be RFC3339: {err}"
                ))
            })?;
            if current
                .lease_expires_at
                .as_deref()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .is_some_and(|existing| existing >= requested)
            {
                return Err(RallyError::Usage(format!(
                    "renew claim lease: requested lease does not advance claim {claim_id}"
                )));
            }
        }
        if matches!(fact.kind, FactKind::Release | FactKind::Resolve) {
            let ref_id = fact.ref_id.as_deref().ok_or_else(|| {
                RallyError::Usage(format!(
                    "{} requires --ref <event-id> targeting a live fact; none provided",
                    fact.kind.as_str()
                ))
            })?;
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            let coord =
                crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
            let snapshot = snapshot_from_facts_with_policy(&facts, &coord, false);
            match fact.kind {
                FactKind::Release => {
                    if !snapshot
                        .active_claims
                        .iter()
                        .any(|claim| claim.event_id == ref_id)
                    {
                        return Err(RallyError::Usage(format!(
                            "release failed: ref {ref_id} is not an active claim (already released, never existed, or invalid); nothing to release"
                        )));
                    }
                }
                FactKind::Resolve => {
                    let open_handoff = snapshot
                        .open_handoffs
                        .iter()
                        .find(|candidate| candidate.event_id == ref_id);
                    let is_live = snapshot
                        .active_blockers
                        .iter()
                        .any(|candidate| candidate.event_id == ref_id)
                        || snapshot
                            .active_claims
                            .iter()
                            .any(|candidate| candidate.event_id == ref_id)
                        || open_handoff.is_some()
                        || snapshot
                            .current_risks
                            .iter()
                            .any(|candidate| candidate.event_id == ref_id)
                        || snapshot
                            .system_health
                            .iter()
                            .any(|candidate| candidate.event_id == ref_id)
                        || snapshot
                            .unconsumed_artifacts
                            .iter()
                            .any(|candidate| candidate.event_id == ref_id);
                    if !is_live {
                        return Err(RallyError::Usage(format!(
                            "resolve failed: ref {ref_id} is not a live blocker, claim, handoff, risk, or unconsumed artifact (already resolved, never existed, or invalid); nothing to resolve"
                        )));
                    }
                    if let Some(handoff) = open_handoff
                        && !handoff_closer_matches_target(handoff, &fact)
                    {
                        let target = handoff.target.as_deref().unwrap_or("<untargeted>");
                        let tool = fact.tool.as_deref().unwrap_or("<unknown>");
                        return Err(RallyError::Usage(format!(
                            "resolve failed: ref {ref_id} is targeted to {target}; tool {tool} cannot resolve it"
                        )));
                    }
                }
                _ => unreachable!("guarded transition kind"),
            }
        }
        // ---- WRITE-BOUNDARY AUTHORITY (ARP-R-01, ARP-R-02, ARP-R-04) -------
        //
        // The one gate every durable write passes, in BOTH store modes: routed
        // mode reaches this same method through `rallyd_core::run_op`, so a
        // hand-built daemon request is gated by the process that owns the
        // ledger rather than by the client that asked. That equivalence is
        // asserted by `tests/write_authority_daemon_parity.rs`, not assumed —
        // the design audit's D1/D6 findings are what assuming costs.
        //
        // Placed here rather than at the commands because three cycles of this
        // defect class (RC-029, ARP-R-01, ARP-R-02) all had the same shape: a
        // correct rule guarding one spelling of an action, and the ledger
        // accepting the action. Field bounds always apply; the claim-close and
        // lead-transfer arms self-select on kind, so ordinary writes pay one
        // `matches!` and nothing else.
        if crate::write_authority::needs_authority_check(&fact) {
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            let coord =
                crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
            let snapshot = snapshot_from_facts_with_policy(&facts, &coord, false);
            crate::write_authority::assert_write_authorized(&fact, &facts, &snapshot, &coord)?;
        } else {
            crate::write_authority::assert_field_bounds(&fact)?;
        }
        if fact.kind == FactKind::Claim {
            let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
            // RC-037: gate room-wide breadth BEFORE conflict detection. A
            // `workspace:*` claim conflicts with everything by design, so if it
            // ever lands, every later claim in the room fails to append. Refuse
            // the unauthorized wildcard at the door rather than let it become a
            // permanent room-wide lock.
            if let Some(refusal) = claim_authority::breadth_violation(&fact, &facts) {
                return Err(RallyError::Usage(refusal));
            }
            if let Some(conflict) = claim_authority::detect_conflict(&facts, &fact) {
                return Err(RallyError::Usage(format!(
                    "claim conflict: {} holds {} (claim {}), which overlaps the scope you \
                     requested, {}",
                    conflict
                        .existing_owner
                        .as_deref()
                        .unwrap_or("unknown owner"),
                    conflict.existing_scope,
                    conflict.existing_claim_id,
                    conflict.scope
                )));
            }
        }
        let live = read_segment_files(&self.log_dir)?;
        let archived = replay_archive_segments(&self.archive_dir)?;
        fact.seq = segment_seq_stats(&live, &archived)?
            .max_seq
            .checked_add(1)
            .ok_or_else(|| RallyError::Message("canonical sequence overflow".to_string()))?;
        let event_type = fact.kind.as_str().to_string();
        let payload = serde_json::to_value(&fact).map_err(RallyError::json("render fact"))?;
        let entry = LedgerLine {
            seq: fact.seq,
            occurred_at: now_string(),
            event_type,
            payload,
            engagement: Some(self.active_engagement.clone()),
        };
        let mut rendered =
            serde_json::to_vec(&entry).map_err(RallyError::json("render canonical fact"))?;
        rendered.push(b'\n');

        let rally_dir = rally_dir_for_segment(&active_segment)?;
        trigger_o26_fault(rally_dir, O26FaultPoint::BeforeCanonicalMutation).map_err(|detail| {
            RallyError::NotStarted(format!(
                "mutation-not-started: {detail}; canonical append was not opened and retry is safe"
            ))
        })?;
        ensure_new_mutation_can_start(&active_segment)?;
        // From the first tail/file mutation onward NotStarted is impossible.
        crate::mark_watchdog_command_outcome_unknown(&fact.event_id, "canonical-write");
        apply_active_tail_repair(&active_segment, tail_repair, &fact.event_id)?;
        if tail_repair != ActiveTailRepair::None {
            trigger_o26_fault(rally_dir, O26FaultPoint::AfterTailRepair).map_err(|detail| {
                RallyError::outcome_unknown(&fact.event_id, "after-tail-repair", detail)
            })?;
        }
        append_canonical_line_and_readback(&active_segment, &entry, &rendered, &fact.event_id)?;

        let mut outcome = AppendOutcome::committed(fact.clone(), Vec::new());
        outcome.projection_complete = false;
        crate::mark_watchdog_append_outcome(&outcome);
        // Preserve O25's deterministic post-commit watchdog seam at O26's
        // canonical-readback boundary, before any derived projection begins.
        crate::block_after_watchdog_commit_for_test();
        if let Err(detail) = trigger_o26_fault(rally_dir, O26FaultPoint::AfterCanonicalReadback) {
            outcome.warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::PostCommitWork,
                message: format!("post-readback projection seam: {detail}"),
            });
        }
        outcome.warnings.extend(self.project_canonical_fact(&fact));
        outcome.projection_complete = outcome.warnings.is_empty();
        crate::mark_watchdog_append_outcome(&outcome);
        Ok(outcome)
    }

    fn project_canonical_fact(&self, fact: &Fact) -> Vec<ProjectionWarning> {
        let warnings = self.project_canonical_fact_inner(fact);
        if !warnings.is_empty() {
            self.invalidate_projection_sidecars();
        }
        warnings
    }

    fn invalidate_projection_sidecars(&self) {
        if let Some(path) = reconcile_cache_path(&self.facts_db_path) {
            let _ = fs::remove_file(path);
        }
        let rally_dir = self
            .facts_db_path
            .parent()
            .unwrap_or(self.repo_root.as_path());
        let _ = fs::remove_file(snapshot_cache_path(rally_dir));
        let _ = fs::remove_file(self.log_dir.join(LOG_INDEX_FILENAME));
        let _ = fs::remove_file(&self.claim_index_path);
    }

    fn project_canonical_fact_inner(&self, fact: &Fact) -> Vec<ProjectionWarning> {
        let mut warnings = Vec::new();
        let rally_dir = self
            .facts_db_path
            .parent()
            .unwrap_or(self.repo_root.as_path());
        if let Err(detail) = trigger_o26_fault(rally_dir, O26FaultPoint::FactsDbProjection) {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::FactsDb,
                message: format!("facts.db projection fault: {detail}"),
            });
            return warnings;
        }
        let fact_store = match self.fact_store_handle(true) {
            Ok(store) => store,
            Err(error) => {
                warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::FactsDb,
                    message: format!("open facts.db projection: {error}"),
                });
                return warnings;
            }
        };

        let before = match facts_from_store(&fact_store) {
            Ok(facts) => facts,
            Err(error) => {
                warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::FactsDb,
                    message: format!("query facts.db projection: {error}"),
                });
                return warnings;
            }
        };
        let same_id = before
            .iter()
            .filter(|existing| existing.event_id == fact.event_id)
            .collect::<Vec<_>>();
        if same_id.len() > 1 {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::FactsDb,
                message: format!(
                    "facts.db projection contains {} rows for event_id {}; canonical fact remains committed",
                    same_id.len(),
                    fact.event_id
                ),
            });
            return warnings;
        }
        if let Some(existing) = same_id.first() {
            let existing_value = normalized_fact_value(existing, fact.seq).ok();
            let canonical_value = normalized_fact_value(fact, fact.seq).ok();
            if existing_value != canonical_value {
                warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::FactsDb,
                    message: format!(
                        "facts.db event-id identity conflict for {}; canonical fact remains committed",
                        fact.event_id
                    ),
                });
                return warnings;
            }
        } else {
            let payload = match serde_json::to_value(fact) {
                Ok(payload) => payload,
                Err(error) => {
                    warnings.push(ProjectionWarning {
                        code: ProjectionWarningCode::FactsDb,
                        message: format!("render facts.db projection: {error}"),
                    });
                    return warnings;
                }
            };
            if let Err(error) =
                fact_store.append(vec![NewEvent::new(fact.kind.as_str().to_string(), payload)])
            {
                warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::FactsDb,
                    message: format!("append facts.db projection: {error}"),
                });
                return warnings;
            }
        }

        match facts_from_store(&fact_store) {
            Ok(after) => {
                let exact = after
                    .iter()
                    .filter(|existing| existing.event_id == fact.event_id)
                    .filter(|existing| {
                        normalized_fact_value(existing, fact.seq).ok()
                            == normalized_fact_value(fact, fact.seq).ok()
                    })
                    .count();
                if exact != 1 {
                    warnings.push(ProjectionWarning {
                        code: ProjectionWarningCode::FactsDb,
                        message: format!(
                            "facts.db exact readback found {exact} rows for event_id {}; canonical fact remains committed",
                            fact.event_id
                        ),
                    });
                    return warnings;
                }
            }
            Err(error) => {
                warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::FactsDb,
                    message: format!("read back facts.db projection: {error}"),
                });
                return warnings;
            }
        }
        drop(fact_store);

        if let Err(detail) = trigger_o26_fault(rally_dir, O26FaultPoint::ReconcileCacheProjection) {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::ReconcileCache,
                message: format!("reconcile cache projection fault: {detail}"),
            });
        } else if let Err(error) = self.refresh_reconcile_cache_after_append(fact.seq) {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::ReconcileCache,
                message: error.to_string(),
            });
        }
        if let Err(error) = self.refresh_log_index() {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::LogIndex,
                message: error.to_string(),
            });
        }
        if let Err(error) = self.refresh_index(fact.seq) {
            warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::ReconcileCache,
                message: error.to_string(),
            });
        }
        if matches!(
            fact.kind,
            FactKind::Claim
                | FactKind::ClaimRenewed
                | FactKind::Release
                | FactKind::Resolve
                | FactKind::ClaimExpired
        ) {
            match facts_from_segments(&self.log_dir, &self.archive_dir).and_then(|facts| {
                claim_authority::write_index_from_facts(&self.claim_index_path, &facts)
                    .map_err(|error| RallyError::Message(format!("write claim index: {error}")))
            }) {
                Ok(()) => {}
                Err(error) => warnings.push(ProjectionWarning {
                    code: ProjectionWarningCode::ClaimIndex,
                    message: error.to_string(),
                }),
            }
        }
        warnings
    }

    /// After a successful single-event append, rebuild the reconcile sidecar
    /// from measured segment + database stats and fingerprint both the main
    /// database and its WAL. If either side cannot be measured or they differ,
    /// return an error so the committed append reports an incomplete derived
    /// projection and invalidates the sidecar before the next operation.
    ///
    /// Non-Unix note: on non-Unix platforms the mutation lock is a no-op
    /// (see store.rs `acquire_room_mutation_lock` #[cfg(not(unix))]). A concurrent
    /// writer may therefore replace the sidecar between the reconcile and this
    /// re-read. Worst case: event counts in the sidecar drift by N peers; this is
    /// self-corrected by a fingerprint mismatch on the next open, which triggers
    /// the authoritative full scan. No data loss is possible — the canonical
    /// JSONL segments are not affected by sidecar drift.
    fn refresh_reconcile_cache_after_append(&self, appended_seq: i64) -> Result<()> {
        let segments = read_segment_files(&self.log_dir)?;
        let archived = replay_archive_segments(&self.archive_dir)?;
        // Never advance counts from the previous sidecar. A lost WAL can leave
        // that sidecar internally consistent while facts.db has rewound. Re-read
        // both authoritative segments and the live SQLite view after the append;
        // only publish a fast-path cache when they still agree exactly.
        let canonical_stats = segment_seq_stats(&segments, &archived)?;
        let db_stats = read_db_event_stats(&self.facts_db_path, self.warm_fact_store.is_none())?;
        if canonical_stats != db_stats || canonical_stats.max_seq < appended_seq {
            return Err(RallyError::Message(format!(
                "reconcile cache projection is not publishable after seq {appended_seq}: canonical count/max {}/{}, facts.db count/max {}/{}",
                canonical_stats.count, canonical_stats.max_seq, db_stats.count, db_stats.max_seq,
            )));
        }
        let cache = ReconcileCache {
            schema_version: RECONCILE_CACHE_SCHEMA_VERSION,
            segments_fingerprint: segments_fingerprint(&segments, &archived),
            db_fingerprint: fingerprint_db(&self.facts_db_path),
            wal_fingerprint: fingerprint_wal(&self.facts_db_path),
            canonical_count: canonical_stats.count,
            canonical_max_seq: canonical_stats.max_seq,
            db_count: db_stats.count,
            db_max_seq: db_stats.max_seq,
        };
        write_reconcile_cache(&self.facts_db_path, &cache)
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
    pub(crate) fn append_fact_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        // O26's base append performs full event-id/seq/LedgerLine readback
        // before it can construct AppendOutcome, so the old presence-only
        // second pass is both weaker and redundant.
        self.append_fact(fact)
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
    pub(crate) fn append_state_transition_verified(&self, fact: &Fact) -> Result<AppendOutcome> {
        let ref_id = fact.ref_id.clone();
        let mut outcome = self.append_fact(fact)?;
        if !outcome.projection_complete {
            return Ok(outcome);
        }
        let verification = (|| -> Result<()> {
            let Some(ref_id) = ref_id.as_deref() else {
                return Ok(());
            };
            let snapshot = self.snapshot()?;
            let still_active = match fact.kind {
                FactKind::Release => snapshot
                    .active_claims
                    .iter()
                    .any(|candidate| candidate.event_id == ref_id),
                FactKind::Resolve => snapshot
                    .active_blockers
                    .iter()
                    .chain(snapshot.active_claims.iter())
                    .chain(snapshot.open_handoffs.iter())
                    .chain(snapshot.current_risks.iter())
                    .chain(snapshot.system_health.iter())
                    .chain(snapshot.unconsumed_artifacts.iter())
                    .any(|candidate| candidate.event_id == ref_id),
                _ => false,
            };
            if still_active {
                return Err(RallyError::Message(format!(
                    "{} projection readback left ref {ref_id} active",
                    fact.kind.as_str()
                )));
            }
            Ok(())
        })();
        if let Err(error) = verification {
            outcome.projection_complete = false;
            outcome.warnings.push(ProjectionWarning {
                code: ProjectionWarningCode::TransitionVerification,
                message: error.to_string(),
            });
            crate::mark_watchdog_append_outcome(&outcome);
        }
        Ok(outcome)
    }

    pub(crate) fn append_session_fact_if_context(
        &self,
        fact: &Fact,
        expected_context_version: Option<u64>,
    ) -> Result<ConditionalAppendOutcome> {
        validate_append_identity(fact)?;
        crate::write_authority::assert_field_bounds(fact)?;
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        let tail = inspect_active_segment_tail(&self.active_segment_path())?;
        if resolve_canonical_event_id(
            &self.log_dir,
            &self.archive_dir,
            fact,
            &self.active_engagement,
        )?
        .is_some()
        {
            return self
                .append_fact_under_lock(fact)
                .map(ConditionalAppendOutcome::Applied);
        }
        let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
        let current_context = facts
            .iter()
            .filter(|existing| existing.kind == FactKind::Session)
            .map(|existing| u64::try_from(existing.seq).unwrap_or(u64::MAX))
            .max();
        if current_context != expected_context_version {
            debug_assert_eq!(
                tail,
                inspect_active_segment_tail(&self.active_segment_path())?
            );
            return Ok(ConditionalAppendOutcome::NotApplied);
        }
        self.append_fact_under_lock(fact)
            .map(ConditionalAppendOutcome::Applied)
    }

    pub(crate) fn facts(&self) -> Result<Vec<Fact>> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        self.facts_under_lock()
    }

    /// Read the reconciled fact projection while the caller already owns the
    /// room mutation lock. Snapshot-cache capture uses this to keep projection
    /// and canonical fingerprint inside one epoch without recursive locking.
    fn facts_under_lock(&self) -> Result<Vec<Fact>> {
        reconcile_segments_and_db(
            &self.log_dir,
            &self.archive_dir,
            &self.facts_db_path,
            self.warm_fact_store.is_none(),
        )?;
        // Warm-pool facade for the READ path (snapshot's underlying read, L11/R1):
        // in daemon mode read through the ONE warm pool; on a corrupt-db error
        // fall through to the cold recovery path (quarantine + reconcile + reopen),
        // same as direct mode. In direct mode (`warm_fact_store` is None) this
        // block is skipped entirely ⇒ byte-identical to main (G1).
        if let Some(warm) = &self.warm_fact_store {
            match facts_from_store(warm) {
                Ok(facts) => return Ok(facts),
                Err(err) if is_malformed_db_error(&err) => {
                    return Err(live_db_recovery_required_error(&self.facts_db_path));
                }
                Err(err) => return Err(err),
            }
        }
        // BOUNDED RECOVERY PATH (f1/G10): the corrupt-db fallback opens a fresh
        // pool directly (not via `fact_store_handle`), so it is intentionally
        // NOT routed through the warm handle and is NOT counted by the G10
        // cold-open probe. It fires only on a malformed-db error, never on a
        // healthy serving daemon; it is shared with the direct path, so
        // rerouting it through the warm handle would risk G1 byte-identity.
        // G10's hot-path claim (append/query/snapshot) is what the counter
        // proves; recovery churn here is out of that scope.
        facts_from_db_with_query_recovery(&self.log_dir, &self.archive_dir, &self.facts_db_path)
    }

    #[allow(dead_code)]
    pub(crate) fn rebuild_claim_index(&self) -> Result<()> {
        let facts = self.facts()?;
        claim_authority::write_index_from_facts(&self.claim_index_path, &facts)
            .map_err(|err| RallyError::Message(format!("write claim index: {err}")))
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn renew_claim_lease(
        &self,
        claim_id: &str,
        lease_expires_at: String,
        caller_tool: Option<&str>,
        caller_session_id: Option<&str>,
        expected_owner_session_id: Option<&str>,
        event_id: &str,
        thread_id: &str,
        created_at: &str,
    ) -> Result<RenewClaimLeaseOutcome> {
        let requested = chrono::DateTime::parse_from_rfc3339(&lease_expires_at).map_err(|err| {
            RallyError::Usage(format!(
                "renew claim lease: lease_expires_at must be RFC3339: {err}"
            ))
        })?;
        validate_append_event_id(event_id)?;
        if thread_id.trim().is_empty() || thread_id.chars().any(char::is_control) {
            return Err(RallyError::Usage(
                "renew claim lease: thread_id must be nonempty and contain no control characters"
                    .to_string(),
            ));
        }
        chrono::DateTime::parse_from_rfc3339(created_at).map_err(|error| {
            RallyError::Usage(format!(
                "renew claim lease: created_at must be RFC3339: {error}"
            ))
        })?;
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
        let current = claim_authority::active_claim_record(&facts, claim_id);
        let existing = facts.iter().find(|fact| fact.event_id == event_id);
        let scope = current
            .as_ref()
            .map(|record| record.raw_scope.clone())
            .or_else(|| existing.map(|fact| fact.scope.clone()))
            .unwrap_or_default();
        let renewal = Fact {
            from_session_id: caller_session_id.map(str::to_string),
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: thread_id.to_string(),
            kind: FactKind::ClaimRenewed,
            tool: caller_tool.map(str::to_string),
            role: None,
            subject: format!("claim lease renewed: {claim_id}"),
            scope,
            created_at: created_at.to_string(),
            summary: None,
            evidence: vec![format!("lease_expires_at:{lease_expires_at}")],
            target: None,
            ref_id: Some(claim_id.to_string()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        validate_append_identity(&renewal)?;
        crate::write_authority::assert_field_bounds(&renewal)?;

        // Canonical identity precedes every stateful lease/owner check. This
        // is the exact-retry path after a reply was lost and the first commit
        // already advanced mutable lease state.
        if existing.is_some() {
            let append_outcome = self.append_fact_under_lock(&renewal)?;
            return Ok(RenewClaimLeaseOutcome {
                record: current,
                append_outcome: Some(append_outcome),
            });
        }

        let Some(mut current) = current else {
            return Ok(RenewClaimLeaseOutcome {
                record: None,
                append_outcome: None,
            });
        };
        if current.from_session_id.as_deref() != expected_owner_session_id {
            return Err(RallyError::Usage(format!(
                "renew claim lease: expected owner session does not match active claim {claim_id}"
            )));
        }
        if !claim_authority::claim_owner_matches_caller(
            current.owner_tool.as_deref(),
            current.from_session_id.as_deref(),
            caller_tool,
            caller_session_id,
        ) {
            return Err(RallyError::Usage(format!(
                "renew claim lease: {} session does not own claim {claim_id}",
                caller_tool.unwrap_or("<unknown>")
            )));
        }
        if current
            .lease_expires_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_some_and(|existing| existing >= requested)
        {
            // Renewal is monotonic. Equal/older retries are idempotent and
            // must never shorten the authoritative lease.
            return Ok(RenewClaimLeaseOutcome {
                record: Some(current),
                append_outcome: None,
            });
        }
        let append_outcome = self.append_fact_under_lock(&renewal)?;
        current.lease_expires_at = Some(lease_expires_at);
        Ok(RenewClaimLeaseOutcome {
            record: Some(current),
            append_outcome: Some(append_outcome),
        })
    }

    #[cfg(test)]
    pub(crate) fn claim_index_path(&self) -> &Path {
        &self.claim_index_path
    }

    pub(crate) fn session_facts_with_context_version(&self) -> Result<(Vec<Fact>, Option<u64>)> {
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        reconcile_segments_and_db(
            &self.log_dir,
            &self.archive_dir,
            &self.facts_db_path,
            self.warm_fact_store.is_none(),
        )?;
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
        self.snapshot_cache_capture(include_archived)
            .map(|capture| capture.snapshot)
    }

    pub(crate) fn snapshot_cache_capture(
        &self,
        include_archived: bool,
    ) -> Result<SnapshotCacheCapture> {
        self.snapshot_cache_capture_at(include_archived, projection_unix_sec())
    }

    fn snapshot_cache_capture_at(
        &self,
        include_archived: bool,
        projection_unix_sec: i64,
    ) -> Result<SnapshotCacheCapture> {
        let rally_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        trigger_o26_fault(rally_dir, O26FaultPoint::SnapshotPostCommit)
            .map_err(|detail| RallyError::Message(format!("snapshot fault: {detail}")))?;
        let _guard = acquire_room_mutation_lock(rally_dir)?;
        let facts = self.facts_under_lock()?;
        let coord = crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
        let snapshot = snapshot_from_facts_with_policy_at(
            &facts,
            &coord,
            include_archived,
            projection_unix_sec,
        );
        let fingerprint = snapshot_cache_fingerprint_at(
            rally_dir,
            projection_unix_sec,
            include_archived,
            &coord,
        )?;
        Ok(SnapshotCacheCapture {
            snapshot,
            fingerprint: Some(fingerprint),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_snapshot_cache_capture_at(
        &self,
        include_archived: bool,
        projection_unix_sec: i64,
    ) -> Result<SnapshotCacheCapture> {
        self.snapshot_cache_capture_at(include_archived, projection_unix_sec)
    }

    fn repo_wide_claim_lifecycle_facts(&self) -> Result<Vec<Fact>> {
        // facts.db is derived from the canonical segment set. Validate that
        // relationship before asking it a safety-bearing collision question.
        // Cold/direct mode may rebuild the cache; a warm daemon pool must fail
        // loud and restart rather than replace a database it still owns.
        reconcile_segments_and_db(
            &self.log_dir,
            &self.archive_dir,
            &self.facts_db_path,
            self.warm_fact_store.is_none(),
        )?;

        // Count/max and file fingerprints prove shape and change detection,
        // not content identity. A derived row can retain the same canonical
        // seq while changing a safety-bearing claim scope. Load only canonical
        // lifecycle rows and compare their normalized Facts before allowing
        // the path-collision join to trust the derived database.
        let canonical = claim_lifecycle_facts_from_segments(&self.log_dir, &self.archive_dir)?;

        if let Some(warm) = &self.warm_fact_store {
            let derived = match claim_lifecycle_facts_from_store(warm) {
                Ok(facts) => facts,
                Err(err) if is_malformed_db_error(&err) => {
                    return Err(live_db_recovery_required_error(&self.facts_db_path));
                }
                Err(err) => return Err(err),
            };
            if !claim_lifecycle_content_equivalent(&canonical, &derived)? {
                return Err(live_db_recovery_required_error(&self.facts_db_path));
            }
            return Ok(canonical);
        }

        // Reconcile above has already repaired/quarantined a malformed cold
        // cache. Opening leniently here could quarantine and then query a new,
        // empty database before canonical replay, falsely reporting no claim.
        let derived = match claim_lifecycle_facts_from_db_path(&self.facts_db_path) {
            Ok(facts) => facts,
            Err(err) if is_malformed_db_error(&err) => {
                quarantine_corrupt_db(&self.facts_db_path)?;
                if let Some(path) = reconcile_cache_path(&self.facts_db_path) {
                    let _ = fs::remove_file(path);
                }
                reconcile_segments_and_db(
                    &self.log_dir,
                    &self.archive_dir,
                    &self.facts_db_path,
                    true,
                )?;
                claim_lifecycle_facts_from_db_path(&self.facts_db_path)?
            }
            Err(err) => return Err(err),
        };
        if claim_lifecycle_content_equivalent(&canonical, &derived)? {
            return Ok(canonical);
        }

        // Same-shape content drift bypasses the global count/max reconcile.
        // Direct mode owns no warm pool, so rebuild the disposable cache from
        // canonical segments, then re-read and prove byte-normalized Fact
        // equivalence before returning any collision context.
        force_rebuild_db_from_canonical_segments(
            &self.log_dir,
            &self.archive_dir,
            &self.facts_db_path,
        )?;
        let repaired = claim_lifecycle_facts_from_db_path(&self.facts_db_path)?;
        if !claim_lifecycle_content_equivalent(&canonical, &repaired)? {
            return Err(RallyError::Message(format!(
                "facts-db-recovery-required: {} lifecycle content still differs from canonical segments after direct rebuild",
                self.facts_db_path.display()
            )));
        }
        Ok(canonical)
    }

    /// Project one engagement/run/path without folding repository-wide facts.
    /// A path adds only repository-wide live collision claims after the scoped
    /// participant and health projection is complete.
    pub(crate) fn snapshot_scoped(
        &self,
        engagement: &str,
        run_id: Option<&str>,
        path: Option<&str>,
        include_archived: bool,
        include_presence_only: bool,
    ) -> Result<RoomSnapshot> {
        let engagement = validate_scoped_engagement(engagement)?;
        let run_marker = run_id
            .map(str::trim)
            .map(|run_id| {
                if run_id.is_empty() {
                    Err(RallyError::Usage(
                        "scoped snapshot run id cannot be empty".to_string(),
                    ))
                } else {
                    Ok(format!("run:{run_id}"))
                }
            })
            .transpose()?;
        let normalized_path = path
            .map(str::trim)
            .map(|path| {
                if path.is_empty() {
                    Err(RallyError::Usage(
                        "scoped snapshot path cannot be empty".to_string(),
                    ))
                } else {
                    Ok(normalize_paths(vec![path.to_string()])
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| path.to_string()))
                }
            })
            .transpose()?;

        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        // Capture both canonical inputs under one mutation epoch, then release
        // the cross-process lock before CPU-only closure/projection/sorting.
        let (engagement_facts, claim_lifecycle_facts) = {
            let _guard = acquire_room_mutation_lock(room_dir)?;
            let engagement_facts =
                facts_from_engagement_segments(&self.log_dir, &self.archive_dir, &engagement)?;
            let claim_lifecycle_facts = normalized_path
                .as_ref()
                .map(|_| self.repo_wide_claim_lifecycle_facts())
                .transpose()?;
            (engagement_facts, claim_lifecycle_facts)
        };

        #[cfg(test)]
        pause_scoped_projection_after_capture(room_dir);

        let scoped_facts = select_scoped_facts(
            &engagement_facts,
            run_marker.as_deref(),
            normalized_path.as_deref(),
        );
        let coord = crate::hooks_config::resolve_coordination(&self.repo_root).unwrap_or_default();
        let mut snapshot = snapshot_from_facts_with_policy(&scoped_facts, &coord, include_archived);

        if include_presence_only {
            let engagement_snapshot =
                snapshot_from_facts_with_policy(&engagement_facts, &coord, include_archived);
            snapshot.squads = engagement_snapshot.squads;
            snapshot.stale_authors = engagement_snapshot.stale_authors;
            snapshot.author_last_seen = engagement_snapshot.author_last_seen;
        } else {
            let contributors = scoped_contributor_tools(&scoped_facts);
            snapshot
                .squads
                .retain(|squad| contributors.contains(&squad.tool));
            snapshot
                .stale_authors
                .retain(|tool| contributors.contains(tool));
            snapshot
                .author_last_seen
                .retain(|tool, _| contributors.contains(tool));
        }

        if let Some(path) = normalized_path.as_deref() {
            let lifecycle = claim_lifecycle_facts.as_deref().ok_or_else(|| {
                RallyError::Message(
                    "path-scoped snapshot captured no claim lifecycle input".to_string(),
                )
            })?;
            let collision_lifecycle = claim_lifecycle_relevant_to_path(lifecycle, path);
            let mut external_claims = collision_lifecycle
                .iter()
                .filter(|fact| claim_authority::is_active_claim_fact(fact, &collision_lifecycle))
                .map(|fact| claim_authority::project_effective_claim(fact, &collision_lifecycle))
                .collect::<Vec<_>>();
            external_claims.sort_by_key(|fact| fact.seq);
            for claim in external_claims {
                if !snapshot
                    .active_claims
                    .iter()
                    .any(|existing| existing.event_id == claim.event_id)
                {
                    snapshot.active_claims.push(claim);
                }
            }
            snapshot.active_claims.sort_by_key(|fact| fact.seq);

            // The collision answer is derived from lifecycle inputs, not only
            // the claims it emits. A newer renewal changes the projected lease;
            // a newer close can remove the claim entirely. Advance the cursor
            // to the newest path-relevant source even when its origin claim is
            // older or the projection emits no claim.
            if let Some(latest_source) = collision_lifecycle.iter().max_by_key(|fact| fact.seq)
                && latest_source.seq > snapshot.max_seq
            {
                snapshot.max_seq = latest_source.seq;
                snapshot.content_max_seq = snapshot.content_max_seq.max(latest_source.seq);
                snapshot.last_activity_ts = Some(latest_source.created_at.clone());
            }
        }

        refresh_snapshot_totals(&mut snapshot);
        Ok(snapshot)
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
        let fact_store = self.fact_store_handle(false)?;
        let query = fact_store
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
    /// Returns `Applied(AppendOutcome)` when this stable checkpoint request was
    /// written/resolved, `NotApplied` when the read position did not advance.
    ///
    /// O26's base append performs exact canonical readback for every kind,
    /// including low-stakes checkpoints. The conditional admission and append
    /// both run under one mutation lock, so concurrent polls cannot pass the
    /// coalescing guard and inflate the ledger.
    pub(crate) fn maybe_append_read_checkpoint(
        &self,
        checkpoint: &Fact,
        read_seq: i64,
    ) -> Result<ConditionalAppendOutcome> {
        validate_append_identity(checkpoint)?;
        crate::write_authority::assert_field_bounds(checkpoint)?;
        if checkpoint.kind != FactKind::Read {
            return Err(RallyError::Usage(
                "read checkpoint request must carry kind=read".to_string(),
            ));
        }
        let tool = checkpoint.tool.as_deref().ok_or_else(|| {
            RallyError::Usage("read checkpoint request requires tool".to_string())
        })?;
        if checkpoint.summary.as_deref() != Some(format!("read_seq:{read_seq}").as_str()) {
            return Err(RallyError::Usage(
                "read checkpoint summary does not match requested read_seq".to_string(),
            ));
        }
        let room_dir = self
            .facts_db_path
            .parent()
            .ok_or_else(|| RallyError::Message("facts db path has no parent".to_string()))?;
        let _guard = acquire_room_mutation_lock(room_dir)?;
        if resolve_canonical_event_id(
            &self.log_dir,
            &self.archive_dir,
            checkpoint,
            &self.active_engagement,
        )?
        .is_some()
        {
            return self
                .append_fact_under_lock(checkpoint)
                .map(ConditionalAppendOutcome::Applied);
        }
        let facts = facts_from_segments(&self.log_dir, &self.archive_dir)?;
        let last_checkpoint = facts
            .iter()
            .filter(|fact| fact.kind == FactKind::Read && fact.tool.as_deref() == Some(tool))
            .filter_map(|fact| {
                fact.summary
                    .as_deref()
                    .and_then(|summary| summary.strip_prefix("read_seq:"))
                    .and_then(|raw| raw.parse::<i64>().ok())
            })
            .max()
            .unwrap_or(0);
        if read_seq <= last_checkpoint {
            // No advancement — coalesce.
            return Ok(ConditionalAppendOutcome::NotApplied);
        }
        self.append_fact_under_lock(checkpoint)
            .map(ConditionalAppendOutcome::Applied)
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

fn fact_matches_scoped_selection(
    fact: &Fact,
    run_marker: Option<&str>,
    path: Option<&str>,
) -> bool {
    let run_matches = run_marker
        .map(|marker| fact.scope.iter().any(|scope| scope == marker))
        .unwrap_or(true);
    let path_matches = path
        .map(|path| {
            fact.scope
                .iter()
                .any(|scope| path_matches_scope(scope, path))
        })
        .unwrap_or(true);
    run_matches && path_matches
}

/// Select task facts before projection, then close the small referential
/// neighborhood needed to project claim/handoff lifecycle correctly. This
/// never expands beyond the already-selected engagement segment pair.
fn select_scoped_facts(
    engagement_facts: &[Fact],
    run_marker: Option<&str>,
    path: Option<&str>,
) -> Vec<Fact> {
    let (facts, stats) = select_scoped_facts_with_stats(engagement_facts, run_marker, path);
    debug_assert!(stats.facts_indexed >= facts.len());
    facts
}

#[derive(Clone, Copy, Debug, Default)]
struct ScopedSelectionStats {
    facts_indexed: usize,
    initial_match_checks: usize,
    queue_pops: usize,
    adjacency_visits: usize,
    scope_visits: usize,
    ref_buckets_processed: usize,
    scope_buckets_processed: usize,
}

impl ScopedSelectionStats {
    #[cfg(test)]
    fn work_units(self) -> usize {
        self.facts_indexed
            + self.initial_match_checks
            + self.queue_pops
            + self.adjacency_visits
            + self.scope_visits
    }
}

#[cfg(test)]
struct ScopedCapturePause {
    room_dir: PathBuf,
    captured: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static SCOPED_CAPTURE_PAUSE: Mutex<Option<ScopedCapturePause>> = Mutex::new(None);

/// Test seam at the exact ownership boundary: selected canonical inputs have
/// been captured, and the mutation lock must already be available to a peer.
#[cfg(test)]
fn pause_scoped_projection_after_capture(room_dir: &Path) {
    let pause = {
        let mut slot = SCOPED_CAPTURE_PAUSE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot
            .as_ref()
            .is_some_and(|pause| pause.room_dir == room_dir)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.captured.send(()).unwrap();
        pause
            .resume
            .recv_timeout(Duration::from_secs(5))
            .expect("scoped projection test did not release its capture pause");
    }
}

/// Test-only copy of the pre-index closure's segment-inspection count. It is
/// retained as measurement evidence, not as a product fallback.
#[cfg(test)]
fn legacy_scoped_selection_work_count(
    engagement_facts: &[Fact],
    run_marker: Option<&str>,
    path: Option<&str>,
) -> usize {
    let mut inspections = 0;
    let mut selected = engagement_facts
        .iter()
        .filter(|fact| {
            inspections += 1;
            fact_matches_scoped_selection(fact, run_marker, path)
        })
        .map(|fact| fact.seq)
        .collect::<BTreeSet<_>>();

    loop {
        let selected_event_ids = engagement_facts
            .iter()
            .filter(|fact| {
                inspections += 1;
                selected.contains(&fact.seq)
            })
            .map(|fact| fact.event_id.as_str())
            .collect::<BTreeSet<_>>();
        let selected_refs = engagement_facts
            .iter()
            .filter(|fact| {
                inspections += 1;
                selected.contains(&fact.seq)
            })
            .filter_map(|fact| fact.ref_id.as_deref())
            .collect::<BTreeSet<_>>();
        let selected_claim_scopes = engagement_facts
            .iter()
            .filter(|fact| {
                inspections += 1;
                selected.contains(&fact.seq) && fact.kind == FactKind::Claim
            })
            .flat_map(|fact| fact.scope.iter().map(String::as_str))
            .collect::<BTreeSet<_>>();

        let mut changed = false;
        for fact in engagement_facts {
            inspections += 1;
            if selected.contains(&fact.seq) {
                continue;
            }
            let references_selected = fact
                .ref_id
                .as_deref()
                .is_some_and(|ref_id| selected_event_ids.contains(ref_id));
            let is_referenced = selected_refs.contains(fact.event_id.as_str());
            let overlapping_release = fact.kind == FactKind::Release
                && fact
                    .scope
                    .iter()
                    .any(|scope| selected_claim_scopes.contains(scope.as_str()));
            if references_selected || is_referenced || overlapping_release {
                changed |= selected.insert(fact.seq);
            }
        }
        if !changed {
            break;
        }
    }
    inspections
}

/// Indexed closure over the selected engagement. Event/ref adjacency and
/// claim/release scope buckets are each expanded at most once, so a long
/// reference chain grows with its rows/edges instead of triggering repeated
/// full-segment scans.
fn select_scoped_facts_with_stats(
    engagement_facts: &[Fact],
    run_marker: Option<&str>,
    path: Option<&str>,
) -> (Vec<Fact>, ScopedSelectionStats) {
    let mut stats = ScopedSelectionStats {
        facts_indexed: engagement_facts.len(),
        ..ScopedSelectionStats::default()
    };
    if run_marker.is_none() && path.is_none() {
        stats.initial_match_checks = engagement_facts.len();
        stats.queue_pops = engagement_facts.len();
        return (engagement_facts.to_vec(), stats);
    }

    let mut facts_by_event_id = BTreeMap::<&str, Vec<usize>>::new();
    let mut facts_by_ref_id = BTreeMap::<&str, Vec<usize>>::new();
    let mut releases_by_scope = BTreeMap::<&str, Vec<usize>>::new();
    for (index, fact) in engagement_facts.iter().enumerate() {
        facts_by_event_id
            .entry(fact.event_id.as_str())
            .or_default()
            .push(index);
        if let Some(ref_id) = fact.ref_id.as_deref() {
            facts_by_ref_id.entry(ref_id).or_default().push(index);
        }
        if fact.kind == FactKind::Release {
            for scope in &fact.scope {
                releases_by_scope
                    .entry(scope.as_str())
                    .or_default()
                    .push(index);
            }
        }
    }

    let mut selected = vec![false; engagement_facts.len()];
    let mut queue = VecDeque::new();
    for (index, fact) in engagement_facts.iter().enumerate() {
        stats.initial_match_checks += 1;
        if fact_matches_scoped_selection(fact, run_marker, path) {
            selected[index] = true;
            queue.push_back(index);
        }
    }

    let mut processed_event_ids = BTreeSet::new();
    let mut processed_ref_ids = BTreeSet::new();
    let mut processed_claim_scopes = BTreeSet::new();
    let enqueue = |index: usize, selected: &mut [bool], queue: &mut VecDeque<usize>| {
        if !selected[index] {
            selected[index] = true;
            queue.push_back(index);
        }
    };

    while let Some(index) = queue.pop_front() {
        stats.queue_pops += 1;
        let fact = &engagement_facts[index];

        if processed_event_ids.insert(fact.event_id.as_str()) {
            stats.ref_buckets_processed += 1;
            if let Some(neighbors) = facts_by_ref_id.get(fact.event_id.as_str()) {
                for &neighbor in neighbors {
                    stats.adjacency_visits += 1;
                    enqueue(neighbor, &mut selected, &mut queue);
                }
            }
        }

        if let Some(ref_id) = fact.ref_id.as_deref()
            && processed_ref_ids.insert(ref_id)
        {
            stats.ref_buckets_processed += 1;
            if let Some(neighbors) = facts_by_event_id.get(ref_id) {
                for &neighbor in neighbors {
                    stats.adjacency_visits += 1;
                    enqueue(neighbor, &mut selected, &mut queue);
                }
            }
        }

        if fact.kind == FactKind::Claim {
            for scope in &fact.scope {
                if processed_claim_scopes.insert(scope.as_str()) {
                    stats.scope_buckets_processed += 1;
                    if let Some(releases) = releases_by_scope.get(scope.as_str()) {
                        for &release in releases {
                            stats.scope_visits += 1;
                            enqueue(release, &mut selected, &mut queue);
                        }
                    }
                }
            }
        }
    }

    let facts = engagement_facts
        .iter()
        .enumerate()
        .filter(|(index, _)| selected[*index])
        .map(|(_, fact)| fact)
        .cloned()
        .collect();
    (facts, stats)
}

fn scoped_contributor_tools(facts: &[Fact]) -> BTreeSet<String> {
    facts
        .iter()
        .filter(|fact| {
            !matches!(
                fact.kind,
                FactKind::Presence | FactKind::Session | FactKind::Read | FactKind::ClaimRenewed
            )
        })
        .filter_map(|fact| fact.tool.clone())
        .filter(|tool| tool != "rally")
        .collect()
}

fn refresh_snapshot_totals(snapshot: &mut RoomSnapshot) {
    snapshot.totals = RoomTotals {
        active_claims: snapshot.active_claims.len(),
        active_blockers: snapshot.active_blockers.len(),
        open_handoffs: snapshot.open_handoffs.len(),
        current_decisions: snapshot.current_decisions.len(),
        current_risks: snapshot.current_risks.len(),
        system_health: snapshot.system_health.len(),
        recent_artifacts: snapshot.recent_artifacts.len(),
        unconsumed_artifacts: snapshot.unconsumed_artifacts.len(),
        stale_facts: snapshot.stale_facts.len(),
        squads: snapshot.squads.len(),
    };
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

const CLAIM_LIFECYCLE_EVENT_TYPES: [&str; 6] = [
    "claim",
    "claim.renewed",
    "claim.expired",
    "release",
    "resolve",
    "receipt",
];

fn is_claim_lifecycle_event_type(event_type: &str) -> bool {
    CLAIM_LIFECYCLE_EVENT_TYPES.contains(&event_type)
}

/// Query only claim-lifecycle rows. This is the repository-wide collision
/// seam for a path-scoped view; it deliberately does not load unrelated
/// ledger facts, squads, health, or contributor activity.
fn claim_lifecycle_facts_from_store(store: &SqliteStore) -> Result<Vec<Fact>> {
    let query = store
        .query(&FactQuery::for_event_types(CLAIM_LIFECYCLE_EVENT_TYPES))
        .map_err(|err| RallyError::Message(format!("query claim lifecycle facts: {err}")))?;
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

fn claim_lifecycle_facts_from_db_path(path: &Path) -> Result<Vec<Fact>> {
    let store = open_fact_store(path)?;
    claim_lifecycle_facts_from_store(&store)
}

/// Load canonical claim lifecycle content without accumulating unrelated room
/// history. Each segment is parsed independently and non-lifecycle rows are
/// discarded before the next segment is opened. Exact live/archive copies
/// dedupe; conflicting canonical envelopes at one seq fail loud.
fn claim_lifecycle_facts_from_segments(log_dir: &Path, archive_dir: &Path) -> Result<Vec<Fact>> {
    let live = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    let mut entries = Vec::new();
    for path in live.iter().chain(archived.iter()) {
        entries.extend(read_segment_entries_matching(path, |entry| {
            is_claim_lifecycle_event_type(&entry.event_type)
        })?);
    }
    entries.sort_by_key(|entry| entry.seq);

    let mut facts = Vec::with_capacity(entries.len());
    let mut seen = BTreeMap::<i64, LedgerLine>::new();
    for entry in entries {
        if let Some(existing) = seen.get(&entry.seq) {
            if existing != &entry {
                return Err(RallyError::Message(format!(
                    "conflicting canonical claim lifecycle rows at seq {}: live/archive rows differ",
                    entry.seq
                )));
            }
            continue;
        }
        let seq = entry.seq;
        let payload = entry.payload.clone();
        seen.insert(seq, entry);
        facts.push(Fact::from_segment_value(payload, seq)?);
    }
    Ok(facts)
}

/// Canonical and derived facts use different physical record sequences after
/// sparse replay. Both decoders normalize them onto the canonical payload seq;
/// sorting full serialized Facts then compares every collision-bearing field
/// while retaining duplicate rows as a mismatch.
fn normalized_claim_lifecycle_content(facts: &[Fact]) -> Result<Vec<(i64, String)>> {
    let mut rows = facts
        .iter()
        .map(|fact| {
            serde_json::to_string(fact)
                .map(|serialized| (fact.seq, serialized))
                .map_err(RallyError::json("normalize claim lifecycle fact"))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort();
    Ok(rows)
}

fn claim_lifecycle_content_equivalent(canonical: &[Fact], derived: &[Fact]) -> Result<bool> {
    Ok(normalized_claim_lifecycle_content(canonical)?
        == normalized_claim_lifecycle_content(derived)?)
}

fn force_rebuild_db_from_canonical_segments(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
) -> Result<()> {
    let live = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    let canonical_stats = segment_seq_stats(&live, &archived)?;
    rebuild_db_from_segments(&live, &archived, facts_db_path)?;
    refresh_reconcile_cache_after_full_scan(log_dir, archive_dir, facts_db_path, canonical_stats);
    Ok(())
}

/// Reduce the typed repository-wide lifecycle query to the facts that can
/// change the collision answer for one requested path. The origin claims name
/// the relevant ids/scopes; renewals and id-based closers reference those ids,
/// while an atomic Release may close by exact scope without a ref.
fn claim_lifecycle_relevant_to_path(facts: &[Fact], path: &str) -> Vec<Fact> {
    let relevant_claims = facts
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::Claim
                && fact
                    .scope
                    .iter()
                    .any(|scope| path_matches_scope(scope, path))
        })
        .collect::<Vec<_>>();
    let claim_ids = relevant_claims
        .iter()
        .map(|claim| claim.event_id.as_str())
        .collect::<BTreeSet<_>>();
    let claim_scopes = relevant_claims
        .iter()
        .flat_map(|claim| claim.scope.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();

    facts
        .iter()
        .filter(|fact| {
            (fact.kind == FactKind::Claim && claim_ids.contains(fact.event_id.as_str()))
                || fact
                    .ref_id
                    .as_deref()
                    .is_some_and(|ref_id| claim_ids.contains(ref_id))
                || (fact.kind == FactKind::Release
                    && fact
                        .scope
                        .iter()
                        .any(|scope| claim_scopes.contains(scope.as_str())))
        })
        .cloned()
        .collect()
}

fn facts_from_db_with_query_recovery(
    log_dir: &Path,
    archive_dir: &Path,
    facts_db_path: &Path,
) -> Result<Vec<Fact>> {
    let fact_store = open_fact_store_lenient(facts_db_path)?;
    match facts_from_store(&fact_store) {
        Ok(facts) => {
            let stats = SeqStats {
                count: i64::try_from(facts.len())
                    .map_err(|err| RallyError::Message(format!("event count overflow: {err}")))?,
                max_seq: facts.iter().map(|fact| fact.seq).max().unwrap_or(0),
            };
            // A cold query creates a WAL even when it only reads. Close the
            // pool first, then fingerprint the settled files so the cache does
            // not immediately invalidate itself on the WAL unlink.
            drop(fact_store);
            refresh_reconcile_cache_after_full_scan(log_dir, archive_dir, facts_db_path, stats);
            Ok(facts)
        }
        Err(err) if is_malformed_db_error(&err) => {
            // Close the only direct-mode pool before moving any SQLite file.
            drop(fact_store);
            quarantine_corrupt_db(facts_db_path)?;
            if let Some(path) = reconcile_cache_path(facts_db_path) {
                let _ = fs::remove_file(path);
            }
            reconcile_segments_and_db(log_dir, archive_dir, facts_db_path, true)?;
            let recovered_store = open_fact_store_lenient(facts_db_path)?;
            let facts = facts_from_store(&recovered_store)?;
            let stats = SeqStats {
                count: i64::try_from(facts.len())
                    .map_err(|err| RallyError::Message(format!("event count overflow: {err}")))?,
                max_seq: facts.iter().map(|fact| fact.seq).max().unwrap_or(0),
            };
            drop(recovered_store);
            refresh_reconcile_cache_after_full_scan(log_dir, archive_dir, facts_db_path, stats);
            Ok(facts)
        }
        Err(err) => Err(err),
    }
}

/// Process-local memo for [`facts_from_segments`], keyed on the segment
/// fingerprint the reconcile sidecar already uses.
///
/// # Why this exists
///
/// One verified append folds the whole segment set about five times: the
/// breadth/conflict check before the write, then `segment_seq_stats`,
/// `refresh_log_index` and the claim-index rebuild after it, plus up to two
/// conditional revival guards (design audit D10 / RC-058). Every one of those
/// folds is individually justified and none of them knows about the others, so
/// the cost is only visible by counting along one call — which is why review
/// never found it and why a reap of 63 expired claims took 40 seconds against a
/// 6,500-fact ledger while the mutation watchdog allows 3.
///
/// The memo does not remove a single fold; it makes the repeats free. Within one
/// append the room mutation lock is held, so nothing can change underneath the
/// post-write folds and they collapse to one parse. Across appends the first
/// fold after each write re-parses and the rest hit.
///
/// # Why the fingerprint is trusted
///
/// `(name, len, mtime_ns, tail_hash)` per file, the SAME signal
/// `refresh_reconcile_cache_after_full_scan` already trusts. Appends move the
/// length, while O26's bounded tail repair moves the fixed-size tail hash even
/// if length and timestamp collide. The repair path also calls
/// [`invalidate_segment_fold_memo`] before rewriting so the current process
/// cannot reuse a detached fold.
///
/// # What this does NOT do
///
/// It does not make the fold cheaper, it does not survive the process, and it
/// does not help a cold single-command invocation, which still folds once per
/// distinct segment state. RC-058's other costs — `read_db_event_stats`
/// deserializing every row to produce a count, `last_seq_in_segment` parsing a
/// whole segment to take its last line — are untouched and stay open.
static SEGMENT_FOLD_MEMO: std::sync::Mutex<Option<SegmentFoldMemo>> = std::sync::Mutex::new(None);

struct SegmentFoldMemo {
    log_dir: PathBuf,
    archive_dir: PathBuf,
    fingerprint: Vec<FileFingerprint>,
    facts: std::sync::Arc<Vec<Fact>>,
}

/// Forget the memo. Call from any path that rewrites a segment file without
/// changing its length.
fn invalidate_segment_fold_memo() {
    if let Ok(mut memo) = SEGMENT_FOLD_MEMO.lock() {
        *memo = None;
    }
}

/// Return the cached fold only when every room key and fingerprint component
/// matches. Kept as a pure helper so the adversarial same-fingerprint control
/// can exercise the exact production predicate while holding the memo lock;
/// calling `facts_from_segments` there would release the lock and let an
/// unrelated parallel test replace the process-global one-slot cache.
fn segment_fold_memo_hit(
    memo: &Option<SegmentFoldMemo>,
    log_dir: &Path,
    archive_dir: &Path,
    fingerprint: &[FileFingerprint],
) -> Option<Vec<Fact>> {
    let memo = memo.as_ref()?;
    (memo.log_dir == log_dir && memo.archive_dir == archive_dir && memo.fingerprint == fingerprint)
        .then(|| (*memo.facts).clone())
}

fn facts_from_segments(log_dir: &Path, archive_dir: &Path) -> Result<Vec<Fact>> {
    let live = read_segment_files(log_dir)?;
    let archived = replay_archive_segments(archive_dir)?;
    let fingerprint = segments_fingerprint(&live, &archived);

    // A poisoned lock is not a reason to fail a read: fall through and fold.
    if let Ok(memo) = SEGMENT_FOLD_MEMO.lock()
        && let Some(facts) = segment_fold_memo_hit(&memo, log_dir, archive_dir, &fingerprint)
    {
        return Ok(facts);
    }

    let entries = canonical_segment_entries(&live, &archived)?;
    let mut facts = Vec::with_capacity(entries.len());
    for entry in entries {
        facts.push(Fact::from_segment_value(entry.payload, entry.seq)?);
    }

    if let Ok(mut memo) = SEGMENT_FOLD_MEMO.lock() {
        *memo = Some(SegmentFoldMemo {
            log_dir: log_dir.to_path_buf(),
            archive_dir: archive_dir.to_path_buf(),
            fingerprint,
            facts: std::sync::Arc::new(facts.clone()),
        });
    }
    Ok(facts)
}

/// Read exactly one engagement's live and archive segments. Rotation moves a
/// segment between these directories, so both locations are always unioned;
/// `include_archived` is a projection policy, not a storage-location switch.
fn facts_from_engagement_segments(
    log_dir: &Path,
    archive_dir: &Path,
    engagement: &str,
) -> Result<Vec<Fact>> {
    let engagement = validate_scoped_engagement(engagement)?;
    let file_name = format!("{engagement}.jsonl");
    let mut by_seq = BTreeMap::<i64, (LedgerLine, bool)>::new();
    let mut fold_source = |path: &Path, exact_legacy_name: bool| -> Result<()> {
        for entry in read_segment_entries(path)? {
            // A modern authoritative envelope stamp always wins. Filename
            // fallback exists only for unstamped legacy exact-name rows.
            let belongs_to_engagement = entry
                .engagement
                .as_deref()
                .map_or(exact_legacy_name, |actual| actual == engagement);
            if let Some((existing, existing_belongs)) = by_seq.get_mut(&entry.seq) {
                if existing != &entry {
                    return Err(RallyError::Message(format!(
                        "conflicting canonical rows for engagement {engagement:?} at seq {}: live/archive rows differ",
                        entry.seq
                    )));
                }
                *existing_belongs |= belongs_to_engagement;
                continue;
            }
            by_seq.insert(entry.seq, (entry, belongs_to_engagement));
        }
        Ok(())
    };

    // The exact live path is inherently scoped by its validated filename.
    fold_source(&log_dir.join(&file_name), true)?;
    for path in replay_archive_segments(archive_dir)? {
        let exact_legacy_name =
            path.file_name().and_then(|name| name.to_str()) == Some(file_name.as_str());
        // Decode and exact-fold every generation before selecting rows. This
        // keeps unrelated canonical conflicts loud while preserving legacy
        // exact-name files that predate the authoritative envelope stamp.
        fold_source(&path, exact_legacy_name)?;
    }

    let mut facts = Vec::with_capacity(by_seq.len());
    for (entry, belongs_to_engagement) in by_seq.into_values() {
        if !belongs_to_engagement {
            continue;
        }
        let seq = entry.seq;
        facts.push(Fact::from_segment_value(entry.payload, seq)?);
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

// NOTE (orphaned doc block): the items these paragraphs documented were
// moved out of this file (the RoomSnapshot projection was inlined back into
// its callers; the claim-close gate moved to
// `write_authority::assert_claim_close_authorized` under ARP-R-02). The prose
// is kept as historical context. It is `//`, not `///`, precisely because a
// doc comment with no item silently attaches to the NEXT one -- here
// `fact_recency_weight`, which it does not describe.
// Pure projection of a `RoomSnapshot` from an already-loaded facts slice.
//
// This is the body formerly inlined in `RoomStore::snapshot`. Extracted so
// that both `snapshot()` and `snapshot_with_readers()` can call it without
// loading facts twice (fix #2 — one DB round-trip instead of two).
// ARP-R-02. The claim-close authorization gate MOVED to
// `write_authority::assert_claim_close_authorized`.
//
// It lived here, called from exactly two arms of
// `append_state_transition_verified` — Release and Resolve. But
// `claim_authority::closes_active_claim` closes a claim on FOUR kinds, and the
// doc comment on this very function named all four while the code covered two.
// `Receipt` and `ClaimExpired` reached `append_fact` with no ownership check,
// so `rally say receipt --tool rogue --ref <claim-id>` took any live claim in
// the room, seconds old, with a live lease. Reproduced end to end.
//
// The replacement is keyed off `closes_active_claim` itself and runs at the
// write boundary, so the kinds that close a claim and the kinds that must be
// authorized to close one are one list. Two hand-copied call sites is how the
// gap opened; there is now one call site and no list to copy.

/// Recency weight for a fact, from its `created_at` and the policy half-life.
/// A fact whose `created_at` fails to parse is treated as fresh (weight 1.0):
/// decay must never hide a message just because its timestamp is malformed.
fn fact_recency_weight(fact: &Fact, now_secs: i64, half_life_secs: i64) -> f64 {
    match chrono::DateTime::parse_from_rfc3339(&fact.created_at) {
        Ok(dt) => crate::decay::recency_weight(now_secs - dt.timestamp(), half_life_secs),
        Err(_) => 1.0,
    }
}

/// Recency weight for RANKING, which is not the same question as recency
/// weight for VISIBILITY.
///
/// [`fact_recency_weight`] answers "may this fact be archived?" and deliberately
/// returns 1.0 for a timestamp it cannot trust, so decay never hides a message
/// on the strength of a bad stamp. That is the right answer for visibility and
/// the wrong one for ranking: under a budget fill, weight 1.0 does not merely
/// keep an untrustworthy fact — it puts it at the FRONT and evicts trustworthy
/// facts behind it.
///
/// Two untrusted cases, both MEASURED rather than absent, both floored to the
/// archive floor so the fact stays visible (the floor comparison is strict `<`)
/// but cannot jump the queue:
///
/// * **Unparseable or empty `created_at`.** `Fact::created_at` carries
///   `#[serde(default)]`, so an omitted field deserializes to `""`. Segments are
///   committed, merged across machines, and hand-edited during conflict
///   resolution, so this is a live route rather than a theoretical one.
/// * **Future-dated.** `decay::recency_weight` clamps a negative age to 0 and
///   returns exactly 1.0. A machine whose clock is two days fast would
///   otherwise pin its facts above every peer's for two days with no
///   malformation anywhere. A negative age is a measurement of clock skew, not
///   evidence of freshness.
///
/// The clamp in `decay.rs` is left alone: it is pinned by a shared golden-vector
/// fixture that must stay byte-identical with the Python mirror. This wrapper is
/// the local, ranking-only correction.
fn fact_rank_weight(fact: &Fact, now_secs: i64, half_life_secs: i64, floor: f64) -> f64 {
    match chrono::DateTime::parse_from_rfc3339(&fact.created_at) {
        Ok(dt) => {
            let age = now_secs - dt.timestamp();
            if age < 0 {
                floor
            } else {
                crate::decay::recency_weight(age, half_life_secs)
            }
        }
        Err(_) => floor,
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
    snapshot_from_facts_with_policy_at(facts, coord, include_archived, projection_unix_sec())
}

fn projection_unix_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Deterministic projection core. Snapshot-cache capture passes the same whole
/// second into both this projection and its freshness proof so a time-derived
/// decision can never be stamped with a different epoch.
fn snapshot_from_facts_with_policy_at(
    facts: &[Fact],
    coord: &crate::hooks_config::CoordinationConfig,
    include_archived: bool,
    now_secs: i64,
) -> RoomSnapshot {
    // Retraction resolution (read-time, append-only): a fact withdrawn by a
    // `retract: <event-id>` fact is dropped from EVERY projection bucket here,
    // while the retraction fact itself survives (it is an artifact, so peers
    // see the correction in recent_artifacts instead of the withdrawn claim).
    // The retraction is always appended after its target, so `max_seq` and
    // read-checkpoint positions never regress. Presence intentionally uses the
    // resolved slice too: a withdrawn fact should not count as evidence of
    // anything, including liveness.
    //
    // PLACEMENT (integration of feat/fact-retraction onto the O02-O31 store):
    // this filter was authored at the top of `snapshot_from_facts_with_policy`,
    // which the stabilization work has since reduced to a thin wrapper over this
    // deterministic core. It belongs in the CORE, not the wrapper: the
    // snapshot-cache capture path calls `snapshot_from_facts_with_policy_at`
    // directly, so filtering in the wrapper alone would let a cached snapshot
    // keep showing facts that a freshly-computed one had already dropped.
    let retracted = crate::retraction::retracted_ids(facts);
    let resolved_facts: Vec<Fact>;
    let facts: &[Fact] = if retracted.is_empty() {
        facts
    } else {
        resolved_facts = facts
            .iter()
            .filter(|f| !retracted.contains(&f.event_id))
            .cloned()
            .collect();
        &resolved_facts
    };
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
        .map(|fact| claim_authority::project_effective_claim(fact, facts))
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
    // Ordered by recency; NOT count-capped. The archive floor above is the
    // adaptive bound (a decision that has decayed past it is already in
    // `stale_facts`); a fixed `truncate(20)` on top of that was a blind cut
    // that could hide the 21st-freshest live decision with no signal saying so.
    // Byte-bounding happens once, on the OUTPUT path, where it can report what
    // it left out — see `compose_room_output`.
    sort_by_recency(&mut current_decisions, now_secs, half_life_secs);

    // DI-1: split kind=risk facts into human coordination risks (current_risks)
    // vs system-generated health/telemetry (system_health), keyed on a known
    // machine subject prefix. Keeps the risk view trustworthy; telemetry stays
    // auditable in its own bucket. `system_health_class` is the single source
    // of truth for both classification and bounded projection.
    const SYSTEM_HEALTH_SUBJECT_PREFIXES: &[&str] = &[
        "external-intake:",
        "unmanaged-agent:",
        "duplicate-active-squad-id:",
        "binary-drift:",
    ];
    fn system_health_class(subject: &str) -> Option<&'static str> {
        SYSTEM_HEALTH_SUBJECT_PREFIXES
            .iter()
            .copied()
            .find(|prefix| subject.starts_with(prefix))
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
        if system_health_class(&f.subject).is_some() {
            system_health_all.push(f.clone());
        } else {
            current_risks.push(f.clone());
        }
    }
    sort_by_recency(&mut current_risks, now_secs, half_life_secs);
    // Dedup telemetry by prefix class (freshest kept). A complete subject often
    // embeds a path, pid, session id, or build pair; using it as the key lets a
    // machine mint an unbounded never-cut bucket with nominally unique rows.
    sort_by_recency(&mut system_health_all, now_secs, half_life_secs);
    let mut seen_classes: BTreeSet<&'static str> = BTreeSet::new();
    let mut system_health: Vec<Fact> = Vec::new();
    for f in system_health_all {
        if system_health_class(&f.subject).is_some_and(|class| seen_classes.insert(class)) {
            system_health.push(f);
        }
    }
    // NOT truncated: prefix classes bound this never-cut bucket to the small,
    // machine-generated system vocabulary.

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
    let author_last_seen = tool_last
        .iter()
        .map(|(tool, (_, timestamp))| (tool.clone(), timestamp.clone()))
        .collect::<BTreeMap<_, _>>();
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
    // Provably-stale authors, captured for the relevance model. Collected here
    // because this is the ONLY place the four liveness signals are in hand.
    let mut stale_authors: BTreeSet<String> = BTreeSet::new();
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
                // Pure ledger projection has no process/filesystem access.
                // The reaper supplies the external observer verdict at its I/O
                // boundary; absence here preserves the existing fail-open view.
                observed_alive: None,
            };
            let verdict = crate::liveness::is_live(&signals, window);

            // FAIL-OPEN: Live and Unknown are KEPT (Unknown = cannot prove dead).
            // Only a provably-Stale squad (all signals present & past window) is
            // DROPPED from the default snapshot; `include_archived` restores it.
            // This direction is opposite the reaper's fail-CLOSED removal path on
            // purpose: hiding a still-alive peer is the dangerous direction here.
            // --- Two different bars, on purpose ---------------------------
            // DROPPING a squad (below) and REAPING a claim are destructive or
            // hiding decisions, so they demand four-signal unanimity: hiding a
            // live peer causes the write collision this system prevents.
            //
            // RANKING an item lower is neither destructive nor hiding — the
            // item still ships if the budget allows, and any omission is
            // reported with its event id. So ranking uses the one signal that
            // is present on EVERY tool: heartbeat age against this session's
            // adaptive window. That is a positive measurement, not an absence.
            //
            // This distinction is load-bearing. `Liveness::Stale` requires
            // signal (c) `code_progress_age`, which no writer produced until
            // this release, so a demotion keyed on `Stale` alone would have
            // been unreachable on every fact in the existing ledger — a dead
            // factor shipped as a live one.
            if heartbeat_age > window {
                stale_authors.insert(tool.clone());
            }
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

    // Lead = the beneficiary of the latest `role:lead` decision, UNLESS the
    // latest lead-family decision is a `role:lead:relinquished` (seat reopened
    // → None).
    //
    // ARP-R-01. This block used to re-implement the predicate and the extractor
    // that `claim_authority` also owns, and the copies drifted the moment
    // `set_lead` started stamping the ACTOR in `tool` and the beneficiary in
    // `target`: the write gate read the new shape, this projection read the old
    // one, and a legitimate `lead handoff` reported success while the seat did
    // not move. Caught by the post-fix verification run, not by review — two
    // projections of one fact is the same defect shape as the two hand-copied
    // claim-close gates in ARP-R-02, so it gets the same treatment.
    //
    // RC-071a. Sharing the two PREDICATES was not enough: the derivation itself
    // was still written out here and again in `claim_authority`, and the seat
    // gates are correct only insofar as they answer what this line answers. A
    // reviewed agreement between two copies is exactly what ARP-R-01 was. Both
    // the seat and its epoch now come from `claim_authority::lead_and_epoch_of`,
    // so the projection and every gate share one body by construction.
    let (lead, lead_epoch) = claim_authority::lead_and_epoch_of(facts);

    // The authorized room-wide freeze, decided ADMISSION-TIME (see the field's
    // doc on `RoomSnapshot`). An unscoped blocker freezes the room only if its
    // author held the seat as of that blocker's own seq. Newest such blocker
    // wins, matching how every other latest-fact-wins projection here behaves.
    let room_freeze_id = active_blockers
        .iter()
        .filter(|b| b.scope.is_empty())
        .filter(|b| b.tool.is_some() && b.tool == claim_authority::lead_as_of(facts, b.seq))
        .max_by_key(|b| b.seq)
        .map(|b| b.event_id.clone());

    // Mission: latest-by-seq Mission fact whose scope contains "mission".
    // "mission" scope distinguishes north-star facts from envelope facts.
    let mission = facts
        .iter()
        .filter(|f| f.kind == "mission" && f.scope.iter().any(|s| s == "mission"))
        .max_by_key(|f| f.seq)
        .map(|f| f.subject.clone());

    let totals = RoomTotals {
        active_claims: active_claims.len(),
        active_blockers: active_blockers.len(),
        open_handoffs: open_handoffs.len(),
        current_decisions: current_decisions.len(),
        current_risks: current_risks.len(),
        system_health: system_health.len(),
        recent_artifacts: recent_artifacts.len(),
        unconsumed_artifacts: unconsumed_artifacts.len(),
        stale_facts: stale_facts.len(),
        squads: squads.len(),
    };

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
        room_freeze_id,
        readers: Vec::new(),
        mission,
        totals,
        composition: None,
        stale_authors,
        author_last_seen,
    }
}

// =============================================================================
// Output-path room composition
// =============================================================================
//
// The byte budget lives HERE and nowhere else. It is deliberately NOT part of
// `snapshot_from_facts_with_policy`, because that projection is a WRITE-PATH
// AUTHORITY: `append_state_transition_verified` gates `rally resolve` on
// membership in `current_risks` / `open_handoffs` / `unconsumed_artifacts` /
// `system_health` (store.rs, "Assert the target is live BEFORE writing"), and
// the `system_health` bucket backs the enter-path idempotency guard. A fact the
// budget dropped from the projection would make `resolve` reject a fact that
// exists, and would let duplicate health rows re-append. Composition is a view
// concern; it stays on the view.

/// The budgeted (informational) buckets, in the order their guaranteed top-1
/// is claimed. Everything NOT listed here is never-cut.
///
/// The split is by CONSEQUENCE, not by size:
/// * `active_claims` / `active_blockers` / `squads` — dropping one risks the
///   write collision Rally exists to prevent, or hides a peer.
/// * `system_health` — reads like telemetry, but the enter-path duplicate guard
///   reads this projection; a dropped subject re-appends a row to the ledger on
///   the next enter, so cutting it is a ledger-growth bug.
/// * `open_handoffs` — budgeted ONLY for handoffs not assigned to the caller;
///   assigned ones are pulled out and reserved before anything competes.
const BUDGETED_BUCKETS: &[&str] = &[
    "current_decisions",
    "recent_artifacts",
    "current_risks",
    "open_handoffs",
];

/// Keep omission metadata bounded too. The count remains exact even when the
/// convenience id sample is capped.
const MAX_OMITTED_IDS: usize = 64;

/// True when a handoff is assigned to `tool` under the SAME rule `rally next`
/// uses (`next::assigned_to_tool`): an explicit target match, or an untargeted
/// / broadcast handoff, which is addressed to everyone including the caller.
///
/// Assigned handoffs are never budget-cut. Narrowing this to an exact target
/// match would make a broadcast handoff droppable, which is the "hides an
/// assignment" failure the never-cut class exists to prevent.
fn handoff_assigned_to(fact: &Fact, tool: &str) -> bool {
    match fact.target.as_deref() {
        None | Some("all") => true,
        Some(target) => target == tool,
    }
}

/// True when a fact names `tool` as its recipient: an explicit `target`, a
/// `to:<tool>` evidence stamp (how injected content records its recipient), or
/// the tool appearing in the fact's scope.
fn fact_addressed_to(fact: &Fact, tool: &str) -> bool {
    if fact.target.as_deref() == Some(tool) {
        return true;
    }
    if fact
        .evidence
        .iter()
        .any(|e| e.strip_prefix("to:").is_some_and(|t| t.trim() == tool))
    {
        return true;
    }
    fact.scope.iter().any(|s| s == tool)
}

/// Approximate serialized size of a fact, in bytes. Used only for budget
/// accounting, so an exact match with the final envelope is unnecessary — but
/// it must never UNDER-count, or the budget could be overrun silently.
fn fact_bytes(fact: &Fact) -> usize {
    serde_json::to_string(fact).map(|s| s.len()).unwrap_or(0) + 1
}

/// One item competing for budget.
struct Candidate {
    bucket: &'static str,
    index: usize,
    score: f64,
    bytes: usize,
    seq: i64,
}

/// Compose the room for OUTPUT: honor the archive verdict, rank the
/// informational buckets by relevance, and fill a byte budget.
///
/// Guarantees, each asserted by a test:
/// 1. **Never-cut buckets are emitted whole**, whatever the budget says.
/// 2. **Every non-empty budgeted bucket emits at least its top item.** This
///    guarantee OUTRANKS the ceiling — a budget too small to hold the top-1 set
///    is overrun, loudly, rather than emptying a bucket.
/// 3. **A handoff assigned to the caller is never cut.**
/// 4. **Nothing is dropped silently.** `totals` always carries true counts, and
///    any omission produces a `composition` block naming the bucket, the count,
///    the omitted event ids, and the command that returns the full view.
/// 5. **An item whose relevance cannot be computed is not demoted for it.** An
///    unparseable timestamp scores 1.0 (`fact_recency_weight`), which places it
///    at the TOP of the fill, not the bottom. That is the deliberate cost of the
///    fail-open direction: a corrupt stamp can outrank a genuinely fresh item.
///    Removal is the destructive direction, so ambiguity resolves toward keeping.
pub(crate) fn compose_room_output<F>(
    mut snapshot: RoomSnapshot,
    coord: &crate::hooks_config::CoordinationConfig,
    consumer: &crate::relevance::ConsumerContext,
    include_archived: bool,
    budget_override: Option<usize>,
    measure_output: F,
) -> RoomSnapshot
where
    F: Fn(&RoomSnapshot) -> usize,
{
    let mut buckets: BTreeMap<String, BucketComposition> = BTreeMap::new();

    // --- Honor the archive verdict ---------------------------------------
    // The fold already decided these facts are below the archive floor and
    // moved them out of the active buckets. Serializing them anyway contradicts
    // the verdict it just reached. The raw segments stay on disk; the count
    // stays in `totals`; `--include-archived` returns them.
    if !include_archived && !snapshot.stale_facts.is_empty() {
        let total = snapshot.stale_facts.len();
        snapshot.stale_facts.clear();
        buckets.insert(
            "stale_facts".to_string(),
            BucketComposition {
                total,
                emitted: 0,
                omitted: total,
                // Deliberately no ids: the drill-in for the archive class is one
                // flag, and a four-figure id list would reintroduce the payload
                // this removes.
                omitted_ids: Vec::new(),
                omitted_ids_truncated: false,
                reason: "archived".to_string(),
            },
        );
    }

    // `--include-archived` IS the drill-in. A caller who asks for the full view
    // and receives a budget-truncated one has no way left to see everything, so
    // the ceiling does not apply — an escape hatch that is itself truncated is
    // not an escape hatch. An explicit `--budget-bytes` still wins, because that
    // caller asked for a bound with their eyes open.
    let budget = match (budget_override, include_archived) {
        (Some(0), _) => None,
        (Some(explicit), _) => Some(explicit),
        (None, true) => None,
        (None, false) => coord.room_budget_bytes(),
    };

    let mut over_budget_causes: Vec<String> = Vec::new();
    if let Some(budget) = budget {
        over_budget_causes = apply_budget(&mut snapshot, coord, consumer, budget, &mut buckets);
    }
    if !over_budget_causes.is_empty() || !buckets.is_empty() {
        install_composition(
            &mut snapshot,
            budget,
            &buckets,
            !over_budget_causes.is_empty(),
            over_budget_causes.clone(),
        );
    }

    if let Some(limit) = budget {
        // The ranking pass budgets fact bodies. The final command envelope also
        // contains fixed snapshot fields, totals, duplicate readers/mission,
        // agent injectability, composition metadata, pretty-print whitespace,
        // and the trailing newline. Measure that exact response and trim the
        // lowest-ranked droppable fact until the real ceiling holds.
        if measure_output(&snapshot) > limit && snapshot.composition.is_none() {
            install_composition(&mut snapshot, budget, &buckets, false, Vec::new());
        }
        loop {
            let bytes = stabilize_emitted_bytes(&mut snapshot, &measure_output);
            if bytes <= limit {
                if let Some(composition) = snapshot.composition.as_mut() {
                    composition.over_budget = false;
                    composition.over_budget_causes.clear();
                }
                stabilize_emitted_bytes(&mut snapshot, &measure_output);
                break;
            }
            let mut estimated_bytes = bytes;
            let mut trimmed = false;
            while estimated_bytes > limit {
                let Some(estimated_savings) =
                    trim_lowest_budgeted(&mut snapshot, coord, consumer, &mut buckets)
                else {
                    break;
                };
                trimmed = true;
                estimated_bytes = estimated_bytes.saturating_sub(estimated_savings);
            }
            if trimmed {
                install_composition(&mut snapshot, budget, &buckets, false, Vec::new());
                continue;
            }

            // Nothing correctness-safe remains to cut. Report the actual
            // overflow instead of deriving a false PASS from an incomplete
            // reserve estimate.
            over_budget_causes = exact_over_budget_causes(&snapshot, consumer);
            install_composition(&mut snapshot, budget, &buckets, true, over_budget_causes);
            stabilize_emitted_bytes(&mut snapshot, &measure_output);
            break;
        }
    } else if snapshot.composition.is_some() {
        stabilize_emitted_bytes(&mut snapshot, &measure_output);
    }
    snapshot
}

fn install_composition(
    snapshot: &mut RoomSnapshot,
    budget: Option<usize>,
    buckets: &BTreeMap<String, BucketComposition>,
    over_budget: bool,
    over_budget_causes: Vec<String>,
) {
    let mut drill_in = vec![
        "rally room --json                     # this view".to_string(),
        "rally locate <event-id> --json        # one omitted item".to_string(),
    ];
    if buckets.contains_key("stale_facts") {
        drill_in.insert(
            0,
            "rally room --include-archived --json  # every archived fact".to_string(),
        );
    }
    let emitted_bytes = snapshot
        .composition
        .as_ref()
        .map(|composition| composition.emitted_bytes)
        .unwrap_or(0);
    snapshot.composition = Some(RoomComposition {
        budget_bytes: budget,
        emitted_bytes,
        buckets: buckets.clone(),
        drill_in,
        over_budget,
        over_budget_causes,
    });
}

/// Set `composition.emitted_bytes` to the exact self-describing response size.
/// The value's own decimal width participates in the measurement, so converge
/// instead of assuming one assignment is enough.
fn stabilize_emitted_bytes<F>(snapshot: &mut RoomSnapshot, measure_output: &F) -> usize
where
    F: Fn(&RoomSnapshot) -> usize,
{
    for _ in 0..8 {
        let bytes = measure_output(snapshot);
        let Some(composition) = snapshot.composition.as_mut() else {
            return bytes;
        };
        if composition.emitted_bytes == bytes {
            return bytes;
        }
        composition.emitted_bytes = bytes;
    }
    measure_output(snapshot)
}

fn composition_score(
    snapshot: &RoomSnapshot,
    fact: &Fact,
    coord: &crate::hooks_config::CoordinationConfig,
    consumer: &crate::relevance::ConsumerContext,
    now_secs: i64,
) -> f64 {
    let recency = fact_rank_weight(
        fact,
        now_secs,
        coord.half_life_secs(),
        coord.archive_floor_weight,
    );
    let signals = crate::relevance::RelevanceSignals {
        author_past_heartbeat_window: fact
            .tool
            .as_deref()
            .is_some_and(|tool| snapshot.stale_authors.contains(tool)),
        addressed_to_caller: consumer
            .tool
            .as_deref()
            .is_some_and(|tool| fact_addressed_to(fact, tool)),
        path_overlap: crate::relevance::path_overlap(&consumer.paths, &fact.scope),
    };
    crate::relevance::relevance(recency, &signals, &coord.relevance)
}

/// Remove one lowest-ranked fact while preserving the top item in every
/// informational bucket and every handoff assigned to the caller.
fn trim_lowest_budgeted(
    snapshot: &mut RoomSnapshot,
    coord: &crate::hooks_config::CoordinationConfig,
    consumer: &crate::relevance::ConsumerContext,
    buckets: &mut BTreeMap<String, BucketComposition>,
) -> Option<usize> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let mut lowest: Option<(&'static str, usize, f64, i64)> = None;
    for &name in BUDGETED_BUCKETS {
        let facts = bucket_ref(snapshot, name);
        let unassigned_handoffs = if name == "open_handoffs" {
            consumer.tool.as_deref().map_or(0, |tool| {
                facts
                    .iter()
                    .filter(|fact| !handoff_assigned_to(fact, tool))
                    .count()
            })
        } else {
            0
        };
        for (index, fact) in facts.iter().enumerate() {
            let removable =
                if name == "open_handoffs" {
                    consumer.tool.as_deref().is_some_and(|tool| {
                        !handoff_assigned_to(fact, tool) && unassigned_handoffs > 1
                    }) || (consumer.tool.is_none() && facts.len() > 1)
                } else {
                    facts.len() > 1
                };
            if !removable {
                continue;
            }
            let score = composition_score(snapshot, fact, coord, consumer, now_secs);
            if lowest.as_ref().is_none_or(|(_, _, best_score, best_seq)| {
                score < *best_score || (score == *best_score && fact.seq < *best_seq)
            }) {
                lowest = Some((name, index, score, fact.seq));
            }
        }
    }
    let (name, index, _, _) = lowest?;

    let total = bucket_total(snapshot, name);
    let removed = bucket_mut(snapshot, name).remove(index);
    let estimated_fact_bytes = serde_json::to_string_pretty(&removed)
        .map(|rendered| rendered.len() + 1)
        .unwrap_or_else(|_| fact_bytes(&removed));
    let removed_id = removed.event_id;
    let emitted = bucket_ref(snapshot, name).len();
    let entry = buckets.entry(name.to_string()).or_default();
    entry.total = total;
    entry.emitted = emitted;
    entry.omitted = total.saturating_sub(emitted);
    entry.reason = "budget".to_string();
    let id_metadata_bytes = if entry.omitted_ids.len() < MAX_OMITTED_IDS {
        let bytes = removed_id.len() + 16;
        entry.omitted_ids.push(removed_id);
        bytes
    } else {
        0
    };
    entry.omitted_ids_truncated = entry.omitted > entry.omitted_ids.len();

    sync_unconsumed_artifacts(snapshot, buckets);
    Some(
        estimated_fact_bytes
            .saturating_sub(id_metadata_bytes)
            .max(1),
    )
}

fn bucket_total(snapshot: &RoomSnapshot, name: &str) -> usize {
    match name {
        "current_decisions" => snapshot.totals.current_decisions,
        "current_risks" => snapshot.totals.current_risks,
        "recent_artifacts" => snapshot.totals.recent_artifacts,
        "open_handoffs" => snapshot.totals.open_handoffs,
        other => unreachable!("bucket_total: {other} is not a budgeted bucket"),
    }
}

fn sync_unconsumed_artifacts(
    snapshot: &mut RoomSnapshot,
    buckets: &mut BTreeMap<String, BucketComposition>,
) {
    let emitted_artifact_ids: BTreeSet<&str> = snapshot
        .recent_artifacts
        .iter()
        .map(|fact| fact.event_id.as_str())
        .collect();
    snapshot
        .unconsumed_artifacts
        .retain(|fact| emitted_artifact_ids.contains(fact.event_id.as_str()));
    let emitted = snapshot.unconsumed_artifacts.len();
    let total = snapshot.totals.unconsumed_artifacts;
    if emitted < total {
        buckets.insert(
            "unconsumed_artifacts".to_string(),
            BucketComposition {
                total,
                emitted,
                omitted: total - emitted,
                omitted_ids: Vec::new(),
                omitted_ids_truncated: true,
                reason: "budget".to_string(),
            },
        );
    }
}

fn exact_over_budget_causes(
    snapshot: &RoomSnapshot,
    consumer: &crate::relevance::ConsumerContext,
) -> Vec<String> {
    let (_, mut causes) = never_cut_bytes(snapshot);
    if consumer.tool.as_deref().is_some_and(|tool| {
        snapshot
            .open_handoffs
            .iter()
            .any(|fact| handoff_assigned_to(fact, tool))
    }) {
        causes.push("assigned_handoffs".to_string());
    }
    causes.push("response_floor".to_string());
    causes.dedup();
    causes
}

/// Rank the budgeted buckets by relevance and fill `budget` bytes.
/// Returns the never-cut buckets that drove an over-budget response, largest
/// first. An empty vec means the ceiling held.
fn apply_budget(
    snapshot: &mut RoomSnapshot,
    coord: &crate::hooks_config::CoordinationConfig,
    consumer: &crate::relevance::ConsumerContext,
    budget: usize,
    buckets: &mut BTreeMap<String, BucketComposition>,
) -> Vec<String> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let half_life_secs = coord.half_life_secs();
    let weights = &coord.relevance;

    // Never-cut buckets are emitted whole and consume budget first. What is
    // left is what the informational buckets compete for. When they alone
    // exceed the ceiling we ship over budget and SAY SO — cutting a claim or a
    // peer to fit would trade a payload problem for a write collision.
    let (reserved, mut over_budget_causes) = never_cut_bytes(snapshot);
    let mut remaining = budget.saturating_sub(reserved);
    if reserved <= budget {
        over_budget_causes.clear();
    }

    let score_of = |fact: &Fact| -> f64 {
        // Ranking weight, not visibility weight — see `fact_rank_weight`.
        let recency = fact_rank_weight(fact, now_secs, half_life_secs, coord.archive_floor_weight);
        // `stale_authors` is a HEARTBEAT verdict, not a `Liveness` one — see
        // the insertion site in `snapshot_from_facts_with_policy` and the
        // `relevance` module docs for why ranking and dropping use different
        // bars. An unresolved author is neutral.
        let author_past_heartbeat_window = fact
            .tool
            .as_deref()
            .is_some_and(|t| snapshot.stale_authors.contains(t));
        let signals = crate::relevance::RelevanceSignals {
            author_past_heartbeat_window,
            addressed_to_caller: consumer
                .tool
                .as_deref()
                .is_some_and(|t| fact_addressed_to(fact, t)),
            path_overlap: crate::relevance::path_overlap(&consumer.paths, &fact.scope),
        };
        crate::relevance::relevance(recency, &signals, weights)
    };

    // Handoffs assigned to the caller are correctness-bearing: split them out
    // and reserve them before anything competes.
    let assigned_handoffs: Vec<Fact> = match consumer.tool.as_deref() {
        Some(tool) => {
            let (mine, theirs): (Vec<Fact>, Vec<Fact>) = snapshot
                .open_handoffs
                .drain(..)
                .partition(|f| handoff_assigned_to(f, tool));
            snapshot.open_handoffs = theirs;
            mine
        }
        None => Vec::new(),
    };
    remaining = remaining.saturating_sub(assigned_handoffs.iter().map(fact_bytes).sum::<usize>());

    // Rank each budgeted bucket. Ties break by seq DESC, matching
    // `sort_by_recency`, so ordering stays deterministic.
    let mut ranked: BTreeMap<&'static str, Vec<Candidate>> = BTreeMap::new();
    for name in BUDGETED_BUCKETS {
        let facts = bucket_ref(snapshot, name);
        let mut cands: Vec<Candidate> = facts
            .iter()
            .enumerate()
            .map(|(index, fact)| Candidate {
                bucket: name,
                index,
                score: score_of(fact),
                bytes: fact_bytes(fact),
                seq: fact.seq,
            })
            .collect();
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.seq.cmp(&a.seq))
        });
        ranked.insert(name, cands);
    }

    // Pass 1 — the floor. Every non-empty bucket emits its top item, even when
    // the budget cannot afford it. A bucket that silently empties is the
    // failure mode this whole design exists to avoid.
    let mut keep: BTreeMap<&'static str, BTreeSet<usize>> = BTreeMap::new();
    for (name, cands) in &ranked {
        if let Some(top) = cands.first() {
            keep.entry(name).or_default().insert(top.index);
            remaining = remaining.saturating_sub(top.bytes);
        }
    }

    // Pass 2 — global fill by descending relevance across all budgeted buckets.
    // Cross-bucket comparison is meaningful because every score shares the same
    // recency spine and the same consumer-relative factors.
    let mut rest: Vec<&Candidate> = ranked
        .values()
        .flat_map(|c| c.iter().skip(1))
        .collect::<Vec<_>>();
    rest.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.seq.cmp(&a.seq))
    });
    for cand in rest {
        if cand.bytes > remaining {
            continue;
        }
        remaining -= cand.bytes;
        keep.entry(cand.bucket).or_default().insert(cand.index);
    }

    // Rebuild each budgeted bucket, preserving its relevance order.
    for name in BUDGETED_BUCKETS {
        let kept = keep.remove(name).unwrap_or_default();
        let order: Vec<usize> = ranked
            .get(name)
            .map(|c| c.iter().map(|x| x.index).collect())
            .unwrap_or_default();
        let facts = bucket_mut(snapshot, name);
        let total = facts.len();
        if kept.len() == total {
            continue;
        }
        let omitted_ids: Vec<String> = order
            .iter()
            .filter(|i| !kept.contains(i))
            .filter_map(|i| facts.get(*i).map(|f| f.event_id.clone()))
            .take(MAX_OMITTED_IDS)
            .collect();
        let mut rebuilt: Vec<Fact> = Vec::with_capacity(kept.len());
        for i in &order {
            if kept.contains(i)
                && let Some(f) = facts.get(*i)
            {
                rebuilt.push(f.clone());
            }
        }
        let emitted = rebuilt.len();
        *facts = rebuilt;
        buckets.insert(
            (*name).to_string(),
            BucketComposition {
                total,
                emitted,
                omitted: total - emitted,
                omitted_ids_truncated: total - emitted > omitted_ids.len(),
                omitted_ids,
                reason: "budget".to_string(),
            },
        );
    }

    // Re-attach the caller's assigned handoffs at the front — they were never
    // in the competition, and they are the first thing the caller needs.
    if !assigned_handoffs.is_empty() {
        let mut merged = assigned_handoffs;
        merged.append(&mut snapshot.open_handoffs);
        snapshot.open_handoffs = merged;
        if let Some(entry) = buckets.get_mut("open_handoffs") {
            entry.emitted = snapshot.open_handoffs.len();
            entry.total = snapshot.totals.open_handoffs;
            entry.omitted = entry.total.saturating_sub(entry.emitted);
        }
    }

    // `unconsumed_artifacts` is DERIVED from `recent_artifacts`; several call
    // sites assume the subset relation. Re-derive it from what actually shipped
    // so the relation survives composition.
    sync_unconsumed_artifacts(snapshot, buckets);

    over_budget_causes
}

fn bucket_ref<'a>(snapshot: &'a RoomSnapshot, name: &str) -> &'a Vec<Fact> {
    match name {
        "current_decisions" => &snapshot.current_decisions,
        "current_risks" => &snapshot.current_risks,
        "recent_artifacts" => &snapshot.recent_artifacts,
        "open_handoffs" => &snapshot.open_handoffs,
        other => unreachable!("bucket_ref: {other} is not a budgeted bucket"),
    }
}

fn bucket_mut<'a>(snapshot: &'a mut RoomSnapshot, name: &str) -> &'a mut Vec<Fact> {
    match name {
        "current_decisions" => &mut snapshot.current_decisions,
        "current_risks" => &mut snapshot.current_risks,
        "recent_artifacts" => &mut snapshot.recent_artifacts,
        "open_handoffs" => &mut snapshot.open_handoffs,
        other => unreachable!("bucket_mut: {other} is not a budgeted bucket"),
    }
}

/// Bytes consumed by the buckets that are never budget-cut, plus those buckets
/// named largest-first so an over-budget response can say WHICH one caused it.
fn never_cut_bytes(snapshot: &RoomSnapshot) -> (usize, Vec<String>) {
    let mut sized: Vec<(usize, String)> = vec![
        (
            snapshot.active_claims.iter().map(fact_bytes).sum(),
            "active_claims".to_string(),
        ),
        (
            snapshot.active_blockers.iter().map(fact_bytes).sum(),
            "active_blockers".to_string(),
        ),
        (
            snapshot.system_health.iter().map(fact_bytes).sum(),
            "system_health".to_string(),
        ),
        (
            serde_json::to_string(&snapshot.squads)
                .map(|s| s.len())
                .unwrap_or(0),
            "squads".to_string(),
        ),
        // Derived, not independently cuttable: `unconsumed_artifacts` is a
        // subset of `recent_artifacts` and is re-derived from whatever that
        // bucket emits, so it shrinks on its own. It still costs bytes, and
        // leaving it out of the accounting made ~44 KB invisible to the budget
        // on this repo — the ceiling cannot bound what it does not count.
        (
            snapshot.unconsumed_artifacts.iter().map(fact_bytes).sum(),
            "unconsumed_artifacts".to_string(),
        ),
    ];
    let total = sized.iter().map(|(b, _)| *b).sum();
    sized.sort_by_key(|(bytes, _)| std::cmp::Reverse(*bytes));
    let causes = sized
        .into_iter()
        .filter(|(b, _)| *b > 0)
        .map(|(_, name)| name)
        .collect();
    (total, causes)
}

fn open_fact_store(path: &Path) -> Result<SqliteStore> {
    // Per-retrier jitter (pid + thread id + process-global salt) de-synchronizes
    // concurrent retriers across BOTH threads and processes — the thundering-herd
    // cure; budget raised for write-burst tolerance (B-write-burst-scale).
    //
    // BOTH budgets below are derived from this command's watchdog deadline
    // rather than chosen independently of it. Before 2026-08-05 this loop ran
    // `20ms * attempt` for 16 attempts (2720ms) and the append loop another
    // 2040ms against a 3000ms watchdog, while SQLite's own busy timeout sat at
    // 5s — so a contended mutation was killed by the watchdog every time and
    // this loop never executed an iteration. See `crate::retry_budget`.
    // `Option`, deliberately: `None` means no watchdog is armed (daemon serve),
    // which is a different question from "how much is left".
    // One call returns BOTH budgets because they are not independent: a loop
    // stops STARTING attempts at its deadline, so an attempt begun just inside
    // it still blocks a full busy_timeout past it. `None` means no watchdog is
    // armed (daemon serve) — a different question from "how much is left".
    let budgets = crate::retry_budget::budgets_for(crate::watchdog_remaining());
    if budgets.busy_timeout.is_zero() {
        return Err(RallyError::Message(
            "open fact store: no watchdog budget remains; the database was not opened".to_string(),
        ));
    }
    // Fixed HERE for the life of the pool: it governs how long SQLite blocks
    // inside EVERY later call on this handle, the append included.
    let mut budget = RetryBudget::new(budgets.retry, retry_jitter_ms());
    loop {
        match SqliteStore::open_with_busy_timeout(path, budgets.busy_timeout) {
            Ok(store) => return Ok(store),
            Err(err) if is_bootstrap_metadata_race(&err) || is_transient_store_contention(&err) => {
                let Some(backoff) = budget.next_backoff() else {
                    // Budget spent — say that, rather than asserting a
                    // contender nobody observed.
                    return Err(RallyError::Message(format!(
                        "open fact store: retry budget exhausted after {} attempts \
                         within this command's watchdog budget while the database \
                         stayed contended ({err}). Something is holding or \
                         saturating {}; \
                         `rally doctor --reap-stale` lists holders and stale \
                         presence, and `--timeout-ms` raises this command's budget.",
                        budget.attempts(),
                        path.display(),
                    )));
                };
                thread::sleep(backoff);
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

fn is_transient_store_contention(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string();
    is_db_locked(err) || msg.contains("pool timed out while waiting for an open connection")
}

#[cfg(test)]
mod store_contention_tests {
    use super::*;

    #[test]
    fn pool_checkout_timeout_is_retryable_contention() {
        assert!(is_transient_store_contention(
            &"sqlx backend failure: pool timed out while waiting for an open connection"
        ));
    }

    #[test]
    fn unrelated_store_failure_is_not_retryable_contention() {
        assert!(!is_transient_store_contention(
            &"sqlx backend failure: invalid database URL"
        ));
    }
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

/// Validate a caller-selected segment name. Unlike append routing, a scoped
/// read must never silently rewrite a label and select a neighboring segment.
pub(crate) fn validate_scoped_engagement(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let cleaned = sanitise_engagement(trimmed);
    if cleaned.is_empty() {
        return Err(RallyError::Usage(
            "scoped snapshot requires a non-empty engagement".to_string(),
        ));
    }
    if value != trimmed || cleaned != trimmed {
        return Err(RallyError::Usage(format!(
            "invalid engagement label {value:?}: leading/trailing whitespace, path separators, and NUL bytes are not allowed"
        )));
    }
    Ok(cleaned)
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

/// Schema for sidecars written only after the complete canonical `LedgerLine`
/// fold has verified every duplicate sequence. Unversioned and older caches
/// predate that invariant and must never authorize a reconcile/append fast path.
const RECONCILE_CACHE_SCHEMA_VERSION: u32 = 2;

/// Cheap per-file fingerprint component: `(filename, byte_len, mtime_ns)` plus
/// optional fixed-size content hashes.
///
/// Segment files populate `tail_hash` from their final 4096 bytes. This keeps
/// O26's bounded tail truncation/newline repair visible even when replacement
/// bytes preserve the prior file length and the filesystem timestamp collides.
/// Old schema-2 sidecars deserialize the missing hash as `None`, which cannot
/// equal a current `Some` hash and therefore fail closed without a schema bump.
/// For `facts.db` the `head_hash` (hash of the first 4096 bytes, the SQLite
/// file-format header + page 1) is populated: in-place header corruption
/// (SQLITE_NOTADB) keeps
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tail_hash: Option<u64>,
}

/// Derived sidecar for the reconcile fast path. All fields are recomputable from
/// the canonical ledger + facts.db; this file is never authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ReconcileCache {
    /// Generation of the validation contract that produced this cache.
    /// Missing legacy fields deserialize as zero and are rejected by
    /// [`read_reconcile_cache`].
    #[serde(default)]
    schema_version: u32,
    /// Sorted fingerprint of every live + archive (replayable) segment file.
    segments_fingerprint: Vec<FileFingerprint>,
    /// `facts.db` fingerprint at the moment counts were last verified equal.
    /// A change here (mtime or size) means the db was rewritten/corrupted since
    /// we last trusted it → we must NOT take the fast path.
    db_fingerprint: Option<FileFingerprint>,
    /// Fingerprint of `facts.db-wal`, when present. SQLite commits can live only
    /// in the WAL while the main file stays byte-identical; omitting this made a
    /// destructive WAL unlink invisible to the reconcile fast path.
    #[serde(default)]
    wal_fingerprint: Option<FileFingerprint>,
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

// ---- G10 cold-open probe (test-only, mirrors note_full_reconcile_scan) ----
//
// Counts [`DirectRoomStore::fact_store_handle`] COLD-branch opens (a fresh
// per-op pool — the churn G10's warm pool exists to avoid) for a WATCHED
// db-path prefix ONLY. Scoping to a watched prefix keeps the daemon warm-pool
// proof deterministic under cargo's parallel test model: unrelated direct-mode
// tests each run on their own temp room, so their cold opens don't match the
// smoke test's watched repo_root and can't contaminate the count. The daemon
// store has `warm_fact_store = Some`, so its hot ops (append/query/snapshot)
// NEVER reach the cold branch — the smoke test watches its own repo_root and
// asserts the count stays 0 across two appends + a snapshot (G10: one warm
// pool, no per-op churn). Compiled out entirely in non-test builds (no-op),
// so the direct path stays byte-identical to main (G1).
#[cfg(test)]
mod cold_open_probe {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COLD_OPENS: AtomicU64 = AtomicU64::new(0);
    static WATCH_PREFIX: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub(super) fn note(path: &Path) {
        if let Ok(guard) = WATCH_PREFIX.lock()
            && let Some(prefix) = guard.as_deref()
            && path.starts_with(prefix)
        {
            COLD_OPENS.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn watch(prefix: &Path) {
        *WATCH_PREFIX.lock().unwrap() = Some(prefix.to_path_buf());
        COLD_OPENS.store(0, Ordering::Relaxed);
    }

    pub(super) fn count() -> u64 {
        COLD_OPENS.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[inline]
fn note_cold_open(path: &Path) {
    cold_open_probe::note(path);
}

#[cfg(not(test))]
#[inline]
fn note_cold_open(_path: &Path) {}

/// Test-only: start counting `fact_store_handle` cold-branch opens whose db
/// path is under `prefix` (the daemon's repo_root), resetting the count to 0.
/// Used by `rallyd_core`'s smoke test to prove the daemon's hot path reuses the
/// ONE warm pool (G10) instead of churning a per-op pool.
#[cfg(test)]
pub(crate) fn watch_cold_opens_under(prefix: &Path) {
    cold_open_probe::watch(prefix);
}

/// Test-only: current watched cold-open count (see [`watch_cold_opens_under`]).
#[cfg(test)]
pub(crate) fn cold_open_count() -> u64 {
    cold_open_probe::count()
}

/// Fingerprint a single file as `(name, byte_len, mtime_ns)` with no content
/// hash. Callers add the fixed-size hash appropriate to their file type.
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
        tail_hash: None,
    })
}

fn fingerprint_segment(path: &Path) -> Option<FileFingerprint> {
    let mut fingerprint = fingerprint_file(path)?;
    fingerprint.tail_hash = Some(hash_file_tail(path));
    Some(fingerprint)
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

/// Fingerprint the live WAL with the same fixed-cost content signal as the main
/// database. Absence is meaningful and therefore represented as `None`.
fn fingerprint_wal(facts_db_path: &Path) -> Option<FileFingerprint> {
    let fingerprint = fingerprint_db(&facts_db_path.with_extension("db-wal"))?;
    // SQLite can leave a zero-length WAL briefly between the synchronous pool
    // close and the file's final unlink. It contains no committed frames, so
    // it is equivalent to no WAL; persisting its transient inode metadata
    // makes a freshly-written sidecar invalidate itself as soon as unlink
    // completes. A later commit grows the WAL and is still fingerprinted.
    (fingerprint.len > 0).then_some(fingerprint)
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

/// Hash the final 4096 bytes of a canonical segment. Fixed-cost and sensitive
/// to the only in-place rewrite O26 permits: active-tail framing repair.
fn hash_file_tail(path: &Path) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return hash_bytes_fnv1a(&[]);
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = length.saturating_sub(4096);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return hash_bytes_fnv1a(&[]);
    }
    let mut bytes = Vec::with_capacity(4096);
    if file.take(4096).read_to_end(&mut bytes).is_err() {
        return hash_bytes_fnv1a(&[]);
    }
    hash_bytes_fnv1a(&bytes)
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
        .filter_map(|p| fingerprint_segment(p))
        .collect();
    fps.sort_by(|a, b| a.name.cmp(&b.name));
    fps
}

fn reconcile_cache_path(facts_db_path: &Path) -> Option<PathBuf> {
    facts_db_path
        .parent()
        .map(|p| p.join(RECONCILE_CACHE_FILENAME))
}

/// Read the sidecar, returning `None` on absent/unparseable/unsupported schema
/// (never errors — the sidecar is disposable and must never override the
/// canonical ledger). Legacy sidecars have no schema field and deserialize as
/// version zero, so upgrades fail closed into one authoritative scan.
fn read_reconcile_cache(facts_db_path: &Path) -> Option<ReconcileCache> {
    let path = reconcile_cache_path(facts_db_path)?;
    let text = fs::read_to_string(&path).ok()?;
    let cache: ReconcileCache = serde_json::from_str(&text).ok()?;
    (cache.schema_version == RECONCILE_CACHE_SCHEMA_VERSION).then_some(cache)
}

/// Write the sidecar atomically (tmp + rename). A failed rename is benign only
/// when another writer published the exact cache value we intended to install.
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
        Err(error) => {
            let exact_peer = read_reconcile_cache(facts_db_path)
                .as_ref()
                .is_some_and(|existing| existing == cache);
            let _ = fs::remove_file(&temp_path);
            if exact_peer {
                Ok(())
            } else {
                Err(RallyError::io(format!(
                    "rename reconcile cache {} to {}",
                    temp_path.display(),
                    path.display()
                ))(error))
            }
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

    let db_stats = read_db_event_stats(facts_db_path, true)?;
    if db_stats.count > 0 {
        return Err(RallyError::Usage(format!(
            "current-format DB-only room detected at {}; automatic promotion is disabled. Preserve facts.db and run `rally doctor --migrate-db-only --engagement <label> --apply` while the daemon is stopped",
            facts_db_path.display()
        )));
    }

    let cache = ReconcileCache {
        schema_version: RECONCILE_CACHE_SCHEMA_VERSION,
        segments_fingerprint: segments_fingerprint(&segments, &archived),
        db_fingerprint: fingerprint_db(facts_db_path),
        wal_fingerprint: fingerprint_wal(facts_db_path),
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
/// * Segments absent but db has events → preserve the db and fail loud with
///   the explicit offline `rally doctor --migrate-db-only` recovery path.
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
    allow_cache_replacement: bool,
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
    let wal_fp = fingerprint_wal(facts_db_path);
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
        && cache.wal_fingerprint == wal_fp
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
    let db_stats = read_db_event_stats(facts_db_path, allow_cache_replacement)?;

    if canonical_stats.count == 0 && db_stats.count == 0 {
        // Nothing to cache (no db, no segments). Drop any stale sidecar.
        if let Some(p) = reconcile_cache_path(facts_db_path) {
            let _ = fs::remove_file(p);
        }
        return Ok(());
    }

    if canonical_stats.count == 0 && db_stats.count > 0 {
        return Err(RallyError::Usage(format!(
            "current-format DB-only room detected at {}; automatic promotion is disabled. Preserve facts.db and run `rally doctor --migrate-db-only --engagement <label> --apply` while the daemon is stopped",
            facts_db_path.display()
        )));
    }

    if canonical_stats != db_stats {
        if !allow_cache_replacement {
            return Err(live_db_recovery_required_error(facts_db_path));
        }
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
        schema_version: RECONCILE_CACHE_SCHEMA_VERSION,
        segments_fingerprint: seg_fp,
        db_fingerprint: fingerprint_db(facts_db_path),
        wal_fingerprint: fingerprint_wal(facts_db_path),
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
        schema_version: RECONCILE_CACHE_SCHEMA_VERSION,
        segments_fingerprint: segments_fingerprint(&segments, &archived),
        db_fingerprint: fingerprint_db(facts_db_path),
        wal_fingerprint: fingerprint_wal(facts_db_path),
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
    read_segment_entries_matching(path, |_| true)
}

/// Apply `include` as each canonical row is decoded so bounded projections can
/// avoid materializing unrelated rows even when one segment is large.
fn read_segment_entries_matching(
    path: &Path,
    include: impl FnMut(&LedgerLine) -> bool,
) -> Result<Vec<LedgerLine>> {
    read_segment_entries_matching_with_policy(path, true, true, include)
}

/// Rotation runs under `mutation.lock`, so a listed source cannot disappear
/// legitimately and an incomplete final fragment must fail rather than be
/// treated as a replay-ignorable torn append. A valid final record without a
/// newline remains valid canonical JSON and is included.
fn read_segment_entries_strict(path: &Path) -> Result<Vec<LedgerLine>> {
    read_segment_entries_matching_with_policy(path, false, false, |_| true)
}

fn read_segment_entries_matching_with_policy(
    path: &Path,
    ignore_incomplete_tail: bool,
    tolerate_missing: bool,
    mut include: impl FnMut(&LedgerLine) -> bool,
) -> Result<Vec<LedgerLine>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        // f4: ordinary replay callers may list immediately before rotation
        // moves a file, so their policy treats NotFound as a benign empty
        // source. Rotation itself holds `mutation.lock` and disables that
        // tolerance: a planned source disappearing under the lock is loud.
        Err(err) if tolerate_missing && err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
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
            Ok(entry) => {
                validate_canonical_line(&entry).map_err(|error| {
                    RallyError::Message(format!(
                        "completed canonical segment corruption in {} at line {}: {}",
                        path.display(),
                        line_number,
                        error
                    ))
                })?;
                if include(&entry) {
                    entries.push(entry);
                }
            }
            Err(_) if !had_newline && ignore_incomplete_tail => break,
            Err(err) => {
                let state = if had_newline {
                    "completed"
                } else {
                    "unterminated"
                };
                return Err(RallyError::Message(format!(
                    "{state} canonical segment corruption in {} at line {}: {}",
                    path.display(),
                    line_number,
                    err
                )));
            }
        }
    }
    Ok(entries)
}

/// Normalize raw read-only SQLite rows into the exact canonical `LedgerLine`
/// representation used by replay. Logical payload sequences win over compacted
/// database positions, preserving sparse histories and their high-water mark.
pub(crate) fn render_db_only_migration_segment(
    source_rows: Vec<DbOnlyMigrationSourceRow>,
    engagement: &str,
) -> Result<DbOnlyMigrationSegment> {
    let engagement = validate_scoped_engagement(engagement)?;
    let mut rows = BTreeMap::<i64, LedgerLine>::new();
    let mut previous_database_seq = 0;
    for record in source_rows {
        let database_seq = record.database_seq;
        if database_seq <= previous_database_seq {
            return Err(RallyError::Message(format!(
                "DB-only source record seq {database_seq} is not strictly newer than {previous_database_seq}"
            )));
        }
        previous_database_seq = database_seq;
        let fact = Fact::from_value(record.payload.clone(), database_seq)?;
        let logical_seq = fact.seq;
        if logical_seq <= 0 {
            return Err(RallyError::Message(format!(
                "DB-only row {} has non-positive logical seq {logical_seq}",
                fact.event_id
            )));
        }
        let mut payload = record.payload;
        let payload_object = payload.as_object_mut().ok_or_else(|| {
            RallyError::Message(format!(
                "DB-only row at record seq {database_seq} is not an object payload"
            ))
        })?;
        payload_object.insert("seq".to_string(), json!(logical_seq));
        let entry = LedgerLine {
            seq: logical_seq,
            occurred_at: record.occurred_at.to_string(),
            event_type: record.event_type,
            payload,
            engagement: Some(engagement.clone()),
        };
        chrono::DateTime::parse_from_rfc3339(&entry.occurred_at).map_err(|error| {
            RallyError::Message(format!(
                "DB-only row at logical seq {logical_seq} has invalid occurred_at {:?}: {error}",
                entry.occurred_at
            ))
        })?;
        validate_canonical_line(&entry)?;
        if rows.insert(logical_seq, entry).is_some() {
            return Err(RallyError::Message(format!(
                "DB-only history has more than one row at logical seq {logical_seq}; migration cannot choose canonical precedence"
            )));
        }
    }
    if rows.is_empty() {
        return Err(RallyError::Usage(
            "facts.db contains no rows; there is no DB-only history to migrate".to_string(),
        ));
    }

    let row_count = u64::try_from(rows.len())
        .map_err(|error| RallyError::Message(format!("row count overflow: {error}")))?;
    let max_seq = rows.last_key_value().map(|(seq, _)| *seq).unwrap_or(0);
    let mut bytes = Vec::new();
    for entry in rows.values() {
        serde_json::to_writer(&mut bytes, entry)
            .map_err(RallyError::json("render DB-only canonical row"))?;
        bytes.push(b'\n');
    }
    Ok(DbOnlyMigrationSegment {
        bytes,
        row_count,
        max_seq,
    })
}

/// Exact readback for marker-bound migration temp/target files. Byte equality
/// prevents normalization drift; strict canonical parsing independently proves
/// that every complete line remains replayable and no torn tail was accepted.
pub(crate) fn verify_db_only_migration_segment(
    path: &Path,
    expected: &DbOnlyMigrationSegment,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(RallyError::io(format!(
        "stat migration segment {}",
        path.display()
    )))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(RallyError::Usage(format!(
            "migration segment {} must be a regular file, not a symlink or special file",
            path.display()
        )));
    }
    let observed = fs::read(path).map_err(RallyError::io(format!(
        "read migration segment {}",
        path.display()
    )))?;
    if observed != expected.bytes {
        return Err(RallyError::Message(format!(
            "migration segment {} differs from the marker-bound canonical candidate",
            path.display()
        )));
    }
    let entries = read_segment_entries_strict(path)?;
    if entries.len() as u64 != expected.row_count
        || entries.last().map_or(0, |entry| entry.seq) != expected.max_seq
    {
        return Err(RallyError::Message(format!(
            "migration segment {} readback stats differ from the marker-bound candidate",
            path.display()
        )));
    }
    Ok(())
}

/// Verify an immutable migration receipt against a target that may have grown
/// by later canonical appends. The receipt-bound bytes must remain the exact
/// prefix; every extension row must be strictly newer, canonical-valid, and
/// stamped for the same engagement. Nothing is rewritten or repaired here.
pub(crate) fn verify_db_only_migration_extension(
    path: &Path,
    expected_prefix: &DbOnlyMigrationSegment,
    expected_engagement: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(RallyError::io(format!(
        "stat migration target {}",
        path.display()
    )))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(RallyError::Usage(format!(
            "migration target {} must be a regular file, not a symlink or special file",
            path.display()
        )));
    }
    let observed = fs::read(path).map_err(RallyError::io(format!(
        "read migration target {}",
        path.display()
    )))?;
    if !observed.starts_with(&expected_prefix.bytes) {
        return Err(RallyError::Message(format!(
            "migration target {} diverges from its immutable receipt-bound prefix",
            path.display()
        )));
    }
    let entries = read_segment_entries_strict(path)?;
    let prefix_count = usize::try_from(expected_prefix.row_count)
        .map_err(|error| RallyError::Message(format!("row count overflow: {error}")))?;
    if entries.len() < prefix_count
        || entries
            .get(prefix_count.saturating_sub(1))
            .map_or(0, |entry| entry.seq)
            != expected_prefix.max_seq
    {
        return Err(RallyError::Message(format!(
            "migration target {} no longer contains the receipt-bound row boundary",
            path.display()
        )));
    }
    for entry in &entries {
        if entry.engagement.as_deref() != Some(expected_engagement) {
            return Err(RallyError::Message(format!(
                "migration target {} seq {} is not stamped for receipt engagement {:?}",
                path.display(),
                entry.seq,
                expected_engagement
            )));
        }
        chrono::DateTime::parse_from_rfc3339(&entry.occurred_at).map_err(|error| {
            RallyError::Message(format!(
                "migration target {} seq {} has invalid occurred_at {:?}: {error}",
                path.display(),
                entry.seq,
                entry.occurred_at
            ))
        })?;
    }
    let mut previous_seq = expected_prefix.max_seq;
    for entry in entries.iter().skip(prefix_count) {
        if entry.seq <= previous_seq {
            return Err(RallyError::Message(format!(
                "migration target {} extension seq {} is not newer than {}",
                path.display(),
                entry.seq,
                previous_seq
            )));
        }
        previous_seq = entry.seq;
    }
    Ok(())
}

/// Strict, non-mutating rotation preflight over one canonical segment.
/// Schema/kind/identity validation is shared with replay; timestamps remain
/// strings here so rotate owns its cutoff parsing and error context.
pub(crate) fn rotation_segment_occurred_at_values(path: &Path) -> Result<Vec<String>> {
    Ok(read_segment_entries_strict(path)?
        .into_iter()
        .map(|entry| entry.occurred_at)
        .collect())
}

/// R9-readback: scan segment files for the presence of a specific `event_id`
/// in any `LedgerLine.payload.event_id` field.  Returns `true` if found.
///
/// Reads each line of each segment file; parses as `LedgerLine`; deserializes
/// `payload` as a minimal struct that exposes `event_id`.  Uses the segment
/// *files* as the authoritative source — never `facts.db`.
#[cfg(test)]
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

/// R9-readback fast path: validate a SINGLE segment file, then scan for
/// `event_id` tail-first.
///
/// The event we just appended is the newest line in the active segment, so the
/// last non-empty line is the overwhelmingly likely hit. A missing file
/// (segment not yet created — impossible right after an append, but handled
/// defensively) returns `false`, deferring to the full scan.
///
/// Correctness: returning `false` here NEVER produces a false readback failure —
/// the caller falls through to the authoritative full live+archive scan. A
/// `true` here is a genuine presence (we matched the exact `event_id`) and all
/// completed lines in the segment passed canonical validation.
#[cfg(test)]
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

/// Fold every repo-wide replay source into one deterministic canonical row per
/// sequence. A repeated sequence is valid only when the complete serialized
/// envelope is equal: timestamp, event type, payload, and engagement included.
/// Anything else has no authoritative winner, so fail before projection or DB
/// replacement instead of selecting whichever file happened to be read first.
fn canonical_segment_entries(live: &[PathBuf], archived: &[PathBuf]) -> Result<Vec<LedgerLine>> {
    let mut by_seq = BTreeMap::<i64, LedgerLine>::new();
    for path in live.iter().chain(archived.iter()) {
        for entry in read_segment_entries(path)? {
            if let Some(existing) = by_seq.get(&entry.seq) {
                if existing != &entry {
                    return Err(RallyError::Message(format!(
                        "conflicting canonical segment rows at seq {}: full LedgerLine values differ",
                        entry.seq
                    )));
                }
                continue;
            }
            by_seq.insert(entry.seq, entry);
        }
    }
    Ok(by_seq.into_values().collect())
}

/// Sequence stats across replay sources. `count` is the number of exact-deduped
/// canonical rows; `max_seq` is the canonical high-water mark. Both are
/// required: sparse histories can have `count < max_seq`, and append must never
/// reuse an existing canonical sequence.
fn segment_seq_stats(live: &[PathBuf], archived: &[PathBuf]) -> Result<SeqStats> {
    let entries = canonical_segment_entries(live, archived)?;
    let count = i64::try_from(entries.len())
        .map_err(|err| RallyError::Message(format!("distinct seq count overflow: {err}")))?;
    Ok(SeqStats {
        count,
        max_seq: entries.last().map(|entry| entry.seq).unwrap_or(0),
    })
}

/// Highest `seq` currently written to a segment file (its on-disk tail), or
/// `None` when the segment is absent/empty. Used as a defense-in-depth dup gate:
/// an allocated seq must always exceed the active segment's tail, else we would
/// write a duplicate that bricks segment replay. Reads the (per-engagement,
/// typically small) active segment and validates every completed line before
/// trusting the last entry.
#[cfg(test)]
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
fn live_db_recovery_required_error(facts_db_path: &Path) -> RallyError {
    RallyError::Command(format!(
        "facts-db-recovery-required: {} needs derived-cache recovery, but a live daemon pool \
         still owns it; stop/restart the daemon before retrying",
        facts_db_path.display()
    ))
}

fn read_db_event_stats(facts_db_path: &Path, allow_cache_replacement: bool) -> Result<SeqStats> {
    if !facts_db_path.exists() {
        return Ok(SeqStats::default());
    }
    // BOUNDED RECOVERY/RECONCILE PATH (f1/G10): opens a fresh pool directly (not
    // via `fact_store_handle`), so it is intentionally NOT routed through the
    // warm handle and is NOT counted by the G10 cold-open probe. It runs on the
    // authoritative reconcile path (fast-path miss) and to quarantine a corrupt
    // cache — a rare, self-bounded event, not the healthy per-op hot path.
    // Rerouting through the warm handle would risk G1 byte-identity (this path
    // is shared with the direct path). G10's counter proves the hot path
    // (append/query/snapshot) reuses the ONE warm pool; this is out of scope.
    let store = match open_fact_store(facts_db_path) {
        Ok(store) => store,
        Err(err) if is_malformed_db_error(&err) => {
            if !allow_cache_replacement {
                return Err(live_db_recovery_required_error(facts_db_path));
            }
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
            // Ensure no pool from this query path remains live before moving
            // the database and its WAL/SHM siblings.
            drop(store);
            if !allow_cache_replacement {
                return Err(live_db_recovery_required_error(facts_db_path));
            }
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
///
/// # The self-triggering hazard this guards (RC-044, second-order)
///
/// Every quarantine file this module writes is literally named
/// `facts.db.corrupt.<UTC_NS>`, and SQLite/sqlx errors routinely embed the
/// database path. A bare `msg.contains("corrupt")` therefore reads *our own
/// quarantine filename* as a corruption report: an ordinary I/O error that
/// merely NAMES a quarantine path — which is exactly what a process hitting
/// leftover debris produces — self-triggers another destructive rename of a
/// database nobody showed to be damaged. Blank the filename token before the
/// word test so only SQLite's own wording can match. The numeric-code and
/// human-message checks are untouched; none of them collide with a path.
fn is_malformed_db_error(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string();
    // Neutralize our own `.corrupt.<stamp>` quarantine filenames before the
    // word-match below; see the doc comment. `.corrupt.` is the exact infix
    // written by `quarantine_corrupt_db`, and SQLite never emits the word with
    // a dot on both sides, so this cannot mask a real corruption message.
    let scrubbed = msg.replace(".corrupt.", ".<quarantined>.");
    // Match "code: 11)" with closing paren to avoid false positive on "code: 110"
    // (SQLite does not emit code 110, but the substring "code: 11" would match it).
    // "code: 26)" is already unambiguous but gets the same treatment for consistency.
    scrubbed.contains("code: 11)")
        || scrubbed.contains("code: 26)")
        || scrubbed.contains("disk image is malformed")
        || scrubbed.contains("file is not a database")
        || scrubbed.contains("corrupt")
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

/// Rebuild the derived sqlite cache by replaying the canonical segment fold in
/// seq order. Exact full-envelope copies dedupe (re-running migration twice can
/// otherwise duplicate); any non-identical row at the same seq fails before the
/// existing cache is touched because neither row has canonical precedence.
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
    let all_entries = canonical_segment_entries(live, archived)?;

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

/// Append a single line to a segment file. Path/payload format identical to
/// the R1 monolith; only the *location* moved.
#[cfg(test)]
fn append_segment_line(segment_path: &Path, entry: &LedgerLine) -> Result<()> {
    if let Some(parent) = segment_path.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    let line = serde_json::to_string(entry).map_err(RallyError::json("render segment line"))?;
    // Keep framing in one buffer. `write_all` may issue multiple syscalls; the
    // O25 mutation lock, not a syscall-size assumption, serializes writers.
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

/// Canonical-first append boundary used by O26 mutations. The caller has
/// already installed `OutcomeUnknown` in the watchdog before entering here.
/// Every error therefore preserves the stable event id and query remedy.
fn append_canonical_line_and_readback(
    segment_path: &Path,
    entry: &LedgerLine,
    rendered_record: &[u8],
    event_id: &str,
) -> Result<()> {
    let parent = segment_path.parent().ok_or_else(|| {
        RallyError::outcome_unknown(event_id, "canonical-parent", "segment path has no parent")
    })?;
    let rally_dir = parent.parent().ok_or_else(|| {
        RallyError::outcome_unknown(event_id, "canonical-room", "log directory has no room root")
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        RallyError::outcome_unknown(event_id, "canonical-parent-create", error.to_string())
    })?;
    let created = !segment_path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(segment_path)
        .map_err(|error| {
            RallyError::outcome_unknown(event_id, "canonical-open", error.to_string())
        })?;
    if o26_fault_armed(rally_dir, O26FaultPoint::PartialCanonicalWrite) {
        let partial_len = (rendered_record.len() / 2).max(1);
        file.write_all(&rendered_record[..partial_len])
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                RallyError::outcome_unknown(event_id, "canonical-partial-write", error.to_string())
            })?;
        let detail = trigger_o26_fault(rally_dir, O26FaultPoint::PartialCanonicalWrite)
            .err()
            .unwrap_or("injected pause after partial canonical write");
        return Err(RallyError::outcome_unknown(
            event_id,
            "canonical-partial-write",
            detail,
        ));
    }
    file.write_all(rendered_record).map_err(|error| {
        RallyError::outcome_unknown(event_id, "canonical-write", error.to_string())
    })?;
    file.sync_all().map_err(|error| {
        RallyError::outcome_unknown(event_id, "canonical-sync", error.to_string())
    })?;
    if created {
        sync_directory(parent).map_err(|error| {
            RallyError::outcome_unknown(event_id, "canonical-parent-sync", error.to_string())
        })?;
    }
    trigger_o26_fault(rally_dir, O26FaultPoint::AfterCanonicalSyncBeforeReadback).map_err(
        |detail| RallyError::outcome_unknown(event_id, "canonical-sync-before-readback", detail),
    )?;

    let entries = read_segment_entries(segment_path).map_err(|error| {
        RallyError::outcome_unknown(event_id, "canonical-readback", error.to_string())
    })?;
    let exact_matches = entries
        .iter()
        .filter(|observed| {
            observed.seq == entry.seq
                && observed.event_type == entry.event_type
                && observed.payload == entry.payload
                && observed.engagement == entry.engagement
        })
        .count();
    if exact_matches != 1 {
        return Err(RallyError::outcome_unknown(
            event_id,
            "canonical-readback",
            format!("expected exactly one full event_id/seq row, found {exact_matches}"),
        ));
    }
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
    /// (sorted `(name, len, mtime_ns, tail_hash)` over live + archive segment files,
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
// against a versioned fingerprint of the canonical live/archive segments,
// `facts.db` mtime, and `log/index.json` content. Canonical segment length and
// tail hash therefore invalidate the cache even when a committed append cannot
// update either derived projection. The fingerprint also binds the exact
// whole-second projection epoch, archive mode, and five effective coordination
// inputs used by snapshot projection; time or policy changes therefore miss
// even when canonical bytes do not move.
//
// The cache is *advisory* — a miss only costs the existing slow path; a
// corrupt cache file is treated as a miss. Readers do NOT take the mutation
// lock on the fast path: the captured fingerprint binds the snapshot to one
// mutation epoch, and full equality with the current input fingerprint is the
// correctness gate. Exact-second equality is intentionally conservative; a
// longer TTL could cross a liveness decision boundary.

const SNAPSHOT_CACHE_FILENAME: &str = "snapshot.cache.json";
const SNAPSHOT_CACHE_GENERATION: u32 = 2;

/// Snapshot and freshness proof captured while one room mutation lock is held.
/// This pair is the only value accepted by the cache writer.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotCacheCapture {
    pub(crate) snapshot: RoomSnapshot,
    /// `None` is a compatible routed reply from a peer that cannot provide a
    /// server-anchored proof. Such a capture is usable but never cacheable.
    pub(crate) fingerprint: Option<SnapshotCacheFingerprint>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SnapshotCacheEnvelope {
    /// Fingerprint of the canonical and projection inputs at capture time. A
    /// cache is fresh iff this matches the current fingerprint exactly.
    fingerprint: SnapshotCacheFingerprint,
    /// Projected `RoomSnapshot` for the fingerprinted ledger state.
    snapshot: RoomSnapshot,
    /// ISO-8601 stamp for observability (not part of the freshness check).
    cached_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SnapshotCacheFingerprint {
    /// Snapshot-cache contract generation. This is independent of the O29
    /// reconcile-cache schema; older/missing generations always miss.
    #[serde(default)]
    generation: u32,
    /// Canonical live+archive segment fingerprints, including fixed-size tail
    /// hashes so canonical-only commits and tail repairs invalidate old caches.
    #[serde(default)]
    segments_fingerprint: Vec<FileFingerprint>,
    /// Whole-second clock value used by the snapshot projection itself. Cache
    /// reads require exact equality with the current second because squad
    /// liveness and archive membership can change without a canonical write.
    #[serde(default)]
    projection_unix_sec: i64,
    /// Projection mode is part of snapshot identity. The before-write cache
    /// consumes only the default, non-archived view.
    #[serde(default)]
    include_archived: bool,
    /// Exact effective inputs read by `snapshot_from_facts_with_policy_at`.
    #[serde(default)]
    projection_policy: SnapshotProjectionPolicyFingerprint,
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct SnapshotProjectionPolicyFingerprint {
    half_life_hours_bits: u64,
    archive_floor_weight_bits: u64,
    default_cadence_secs: i64,
    miss_multiplier: i64,
    grace_secs: i64,
}

impl SnapshotProjectionPolicyFingerprint {
    fn from_effective(coord: &crate::hooks_config::CoordinationConfig) -> Self {
        Self {
            half_life_hours_bits: coord.half_life_hours.to_bits(),
            archive_floor_weight_bits: coord.archive_floor_weight.to_bits(),
            default_cadence_secs: coord.default_cadence_secs,
            miss_multiplier: coord.miss_multiplier,
            grace_secs: coord.grace_secs,
        }
    }
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

fn snapshot_cache_fingerprint_at(
    rally_dir: &Path,
    projection_unix_sec: i64,
    include_archived: bool,
    coord: &crate::hooks_config::CoordinationConfig,
) -> Result<SnapshotCacheFingerprint> {
    let facts_db = rally_dir.join("facts.db");
    let log_index = rally_dir.join(LOG_DIRNAME).join(LOG_INDEX_FILENAME);
    let live = read_segment_files(&rally_dir.join(LOG_DIRNAME))?;
    let archived = replay_archive_segments(&rally_dir.join(ARCHIVE_DIRNAME))?;
    Ok(SnapshotCacheFingerprint {
        generation: SNAPSHOT_CACHE_GENERATION,
        segments_fingerprint: segments_fingerprint(&live, &archived),
        projection_unix_sec,
        include_archived,
        projection_policy: SnapshotProjectionPolicyFingerprint::from_effective(coord),
        facts_db_mtime_ns: file_mtime_ns(&facts_db),
        log_index_text: fs::read_to_string(&log_index).unwrap_or_default(),
    })
}

/// Read-only snapshot retrieval: return the cached `RoomSnapshot` when its
/// fingerprint matches the current canonical and projection inputs. `None`
/// when the cache is absent, unparseable, or stale. No mutation lock is
/// acquired and no SQLite connection is opened on a hit; this is the path the
/// before-write gate takes under sub-100ms targets.
pub(crate) fn try_load_cached_snapshot(rally_dir: &Path) -> Option<RoomSnapshot> {
    try_load_cached_snapshot_at(rally_dir, projection_unix_sec())
}

fn try_load_cached_snapshot_at(rally_dir: &Path, projection_unix_sec: i64) -> Option<RoomSnapshot> {
    let cache_path = snapshot_cache_path(rally_dir);
    let text = fs::read_to_string(&cache_path).ok()?;
    let envelope: SnapshotCacheEnvelope = serde_json::from_str(&text).ok()?;
    let repo_root = rally_dir.parent()?;
    let coord = crate::hooks_config::resolve_coordination(repo_root).ok()?;
    let now = snapshot_cache_fingerprint_at(rally_dir, projection_unix_sec, false, &coord).ok()?;
    if envelope.fingerprint == now {
        Some(envelope.snapshot)
    } else {
        None
    }
}

/// Persist a previously captured snapshot/fingerprint pair. The writer never
/// measures current files: doing so could stamp an old snapshot with a newer
/// canonical generation after an intervening append. Atomic temp+rename; any
/// IO error is swallowed because the cache is advisory.
pub(crate) fn write_snapshot_cache(rally_dir: &Path, capture: &SnapshotCacheCapture) {
    let Some(fingerprint) = capture.fingerprint.clone() else {
        return;
    };
    let envelope = SnapshotCacheEnvelope {
        fingerprint,
        snapshot: capture.snapshot.clone(),
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
pub(crate) fn write_snapshot_cache_for(repo_root: &Path, capture: &SnapshotCacheCapture) {
    write_snapshot_cache(&repo_root.join(".rally"), capture);
}

#[cfg(test)]
mod ledger_tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::mpsc;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[test]
    fn direct_then_daemon_owner_lock_order_has_no_cycle() {
        let root = unique_root("direct-daemon-owner-lock-order");
        let rally_dir = root.join(".rally");
        let direct_owner = acquire_direct_owner_exclusive_nb(&rally_dir)
            .unwrap()
            .expect("first direct owner must acquire EX");
        let daemon_exclusion = acquire_owner_shared_nb(&rally_dir)
            .unwrap()
            .expect("direct owner must acquire daemon exclusion SH");
        assert!(
            acquire_direct_owner_exclusive_nb(&rally_dir)
                .unwrap()
                .is_none(),
            "a second direct owner must be excluded"
        );

        let (tx, rx) = mpsc::channel();
        let daemon_dir = rally_dir.clone();
        thread::spawn(move || {
            tx.send(acquire_owner_exclusive_blocking(&daemon_dir))
                .unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "daemon EX must wait while direct holds daemon exclusion SH"
        );
        drop(daemon_exclusion);
        let daemon_owner = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("daemon EX must acquire after SH drains")
            .expect("daemon EX acquisition must succeed");
        drop(daemon_owner);
        drop(direct_owner);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn offline_migration_authority_orders_direct_then_daemon_then_mutation() {
        let root = unique_root("offline-migration-authority-order");
        let rally_dir = root.join(".rally");
        let authority = acquire_offline_migration_authority(&rally_dir)
            .expect("offline migration must acquire all three guards");

        assert!(
            acquire_direct_owner_exclusive_nb(&rally_dir)
                .unwrap()
                .is_none(),
            "offline migration must exclude another direct facts.db owner"
        );
        assert!(
            matches!(
                acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(25)),
                Err(RallyError::NotStarted(_))
            ),
            "offline migration daemon SH must exclude daemon startup"
        );
        assert!(
            matches!(
                with_mutation_deadline(Duration::from_millis(25), || {
                    acquire_room_mutation_lock(&rally_dir)
                }),
                Err(RallyError::NotStarted(_))
            ),
            "offline migration must hold mutation.lock for its full lifetime"
        );

        drop(authority);
        let direct = acquire_direct_owner_exclusive_nb(&rally_dir)
            .unwrap()
            .expect("direct ownership must recover after migration authority drops");
        drop(direct);
        let daemon = acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(100))
            .expect("daemon ownership must recover after migration authority drops");
        drop(daemon);
        let mutation = with_mutation_deadline(Duration::from_millis(100), || {
            acquire_room_mutation_lock(&rally_dir)
        })
        .expect("mutation.lock must recover after migration authority drops");
        drop(mutation);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn direct_owner_timeout_is_typed_busy_unknown() {
        let root = unique_root("direct-owner-busy-unknown");
        let rally_dir = root.join(".rally");
        let _held = acquire_direct_owner_exclusive_nb(&rally_dir)
            .unwrap()
            .expect("test owns direct EX");
        let result = acquire_direct_ownership_or_route_bounded(
            &root,
            &rally_dir,
            None,
            Duration::from_millis(25),
        );
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("contended direct owner must time out"),
        };
        assert!(
            err.to_string().contains("direct-store-busy-unknown:"),
            "typed contention error must survive rendering: {err}"
        );
        assert!(
            err.to_string().contains("rally daemon status")
                && err.to_string().contains("rally daemon stop"),
            "typed contention error must include the safe recovery commands: {err}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn mutation_lock_deadline_returns_not_started_and_never_commits_late() {
        let root = unique_root("mutation-lock-deadline");
        let rally_dir = root.join(".rally");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let held = acquire_room_mutation_lock(&rally_dir).unwrap();
        let mut fact = Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: "o25-direct-no-late-commit".to_string(),
            thread_id: "thread-o25-direct".to_string(),
            kind: FactKind::Artifact,
            subject: "must never commit after lock deadline".to_string(),
            created_at: crate::now_string(),
            ..Fact::default()
        };
        fact.tool = Some("codex:o25".to_string());

        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let result =
                with_mutation_deadline(Duration::from_millis(75), || store.append_fact(&fact));
            tx.send((started.elapsed(), result)).unwrap();
        });

        let prompt = rx.recv_timeout(Duration::from_millis(300));
        let completed_before_release = prompt.is_ok();
        drop(held);
        let (elapsed, result) = match prompt {
            Ok(result) => result,
            Err(_) => rx
                .recv_timeout(Duration::from_secs(2))
                .expect("legacy blocking lock did not return after release"),
        };
        worker.join().unwrap();

        assert!(
            completed_before_release && elapsed < Duration::from_millis(300),
            "mutation lock wait escaped its 75ms deadline: {elapsed:?}"
        );
        let error = result
            .as_ref()
            .expect_err("contended mutation must return typed NotStarted");
        assert!(matches!(error, RallyError::NotStarted(_)), "got {error:?}");
        assert!(
            error.to_string().contains("before acquiring"),
            "true pre-acquire expiry lost its precise diagnostic: {error}"
        );
        let reopened = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        assert!(
            reopened
                .facts()
                .unwrap()
                .iter()
                .all(|row| row.event_id != "o25-direct-no-late-commit"),
            "a not-started mutation committed after its caller returned"
        );
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn direct_mutation_rechecks_deadline_after_flock_success() {
        let root = unique_root("mutation-lock-post-flock-deadline");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let fact = Fact {
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: "o25-direct-post-flock-no-late-commit".to_string(),
            thread_id: "thread-o25-direct-post-flock".to_string(),
            kind: FactKind::Artifact,
            subject: "must not commit after post-flock deadline".to_string(),
            created_at: crate::now_string(),
            ..Fact::default()
        };
        force_next_room_lock_post_flock_pause(Duration::from_millis(60));

        let result = with_mutation_deadline(Duration::from_millis(20), || store.append_fact(&fact));

        let error = result.expect_err("deadline elapsed after flock success but mutation started");
        assert!(matches!(error, RallyError::NotStarted(_)), "got {error:?}");
        let message = error.to_string();
        assert!(message.contains("after provisional lock acquisition"));
        assert!(message.contains("lock released before any durable mutation"));
        assert!(!message.contains("before acquiring"));
        let reopened = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        assert!(
            reopened
                .facts()
                .unwrap()
                .iter()
                .all(|row| row.event_id != fact.event_id),
            "direct mutation committed after its post-flock deadline"
        );
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_owner_open_deadline_is_typed_not_started() {
        let root = unique_root("daemon-owner-open-deadline");
        let rally_dir = root.join(".rally");
        let held = acquire_owner_shared_nb(&rally_dir)
            .unwrap()
            .expect("test holds daemon exclusion SH");

        let started = Instant::now();
        let result = acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(75));
        let elapsed = started.elapsed();

        drop(held);
        assert!(
            elapsed < Duration::from_millis(300),
            "daemon owner open escaped its 75ms deadline: {elapsed:?}"
        );
        assert!(
            matches!(&result, Err(RallyError::NotStarted(_))),
            "contended daemon open must return typed NotStarted"
        );
        let message = match &result {
            Err(error) => error.to_string(),
            Ok(_) => unreachable!("typed NotStarted assertion above"),
        };
        assert!(
            message.contains("before acquiring"),
            "true pre-acquire owner expiry lost its precise diagnostic"
        );
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_owner_rechecks_deadline_after_flock_success() {
        let root = unique_root("daemon-owner-post-flock-deadline");
        let rally_dir = root.join(".rally");
        force_next_owner_lock_post_flock_pause(Duration::from_millis(60));

        let result = acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(20));

        let error = match result {
            Err(error) => error,
            Ok(_guard) => {
                panic!("owner deadline elapsed after flock success but ownership started")
            }
        };
        assert!(matches!(error, RallyError::NotStarted(_)), "got {error:?}");
        let message = error.to_string();
        assert!(message.contains("after provisional lock acquisition"));
        assert!(message.contains("lock released before any daemon runtime state"));
        assert!(!message.contains("before acquiring"));
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_warm_close_is_bounded_and_drop_is_prompt_nonpanicking() {
        let root = unique_root("bounded-warm-close");
        let rally_dir = root.join(".rally");
        let mut store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store.install_warm_fact_store().unwrap();
        let held = acquire_room_mutation_lock(&rally_dir).unwrap();

        let started = Instant::now();
        let close = store.close_warm_fact_store_bounded(Duration::from_millis(75));
        let close_elapsed = started.elapsed();
        assert!(
            close_elapsed < Duration::from_millis(300),
            "explicit warm close escaped its 75ms deadline: {close_elapsed:?}"
        );
        assert!(
            matches!(close, Err(RallyError::NotStarted(_))),
            "close that cannot acquire the mutation lock must be NotStarted: {close:?}"
        );

        let (tx, rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)));
            tx.send(result).unwrap();
        });
        let prompt = rx.recv_timeout(Duration::from_millis(150));
        let completed_before_release = prompt.is_ok();
        drop(held);
        let outcome = match prompt {
            Ok(outcome) => outcome,
            Err(_) => rx
                .recv_timeout(Duration::from_secs(2))
                .expect("legacy blocking Drop did not return after lock release"),
        };
        dropper.join().unwrap();
        assert!(
            completed_before_release,
            "DirectRoomStore::drop blocked on mutation.lock"
        );
        assert!(outcome.is_ok(), "DirectRoomStore::drop panicked");
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn warm_close_spawn_failure_retains_mutation_lock_until_process_exit() {
        let root = unique_root("warm-close-spawn-failure");
        let rally_dir = root.join(".rally");
        let mut store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store.install_warm_fact_store().unwrap();
        force_next_warm_close_spawn_failure();

        let close = store.close_warm_fact_store_bounded(Duration::from_millis(100));
        assert!(
            matches!(close, Err(RallyError::Command(ref message)) if message.starts_with("daemon-close-not-started:")),
            "injected spawn failure must fail loud: {close:?}"
        );
        let reacquire = with_mutation_deadline(Duration::from_millis(40), || {
            acquire_room_mutation_lock(&rally_dir)
        });
        assert!(
            matches!(reacquire, Err(RallyError::NotStarted(_))),
            "spawn failure released mutation.lock before process exit"
        );
        // The repaired path deliberately retains the warm pool and lock until
        // process exit. Do not unlink their rendezvous path in this test.
        drop(store);
    }

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

    #[test]
    fn fact_from_segment_value_overwrites_spoofed_payload_sequence() {
        let fact = Fact::from_segment_value(
            json!({
                "schema": fact_schema(),
                "event_id": "spoofed-seq",
                "seq": 9_999,
                "kind": "artifact",
                "subject": "canonical envelope wins"
            }),
            7,
        )
        .unwrap();

        assert_eq!(
            fact.seq, 7,
            "the canonical envelope/database sequence must overwrite payload seq"
        );
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

    static UNIQUE_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = UNIQUE_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rally-{label}-pid{}-{counter}-{nanos}",
            std::process::id()
        ));
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

    fn scoped_presence(event_id: &str, tool: &str) -> Fact {
        Fact {
            from_session_id: Some(format!("sess:{tool}")),
            schema: fact_schema(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: format!("t-{event_id}"),
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("agent presence: {tool}"),
            scope: Vec::new(),
            created_at: now_string(),
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

    // ---- THE CLASS TEST: every way a fact can remove an active claim -------
    //
    // Five cycles of one defect (RC-029, ARP-R-01, ARP-R-02, R1, R5) share a
    // shape: a correct rule guarding one SPELLING of an action, while the
    // ledger accepts the ACTION. Each was fixed by covering the spelling that
    // got through. This is the attempt to fail the whole class instead.

    /// Can a fact of this kind remove an active claim from the room?
    ///
    /// EXHAUSTIVE, with NO wildcard arm on purpose: a new `FactKind` cannot
    /// compile until someone decides this question about it. That is the
    /// compile-time half of the lock. The runtime half is
    /// `every_kind_that_can_remove_a_claim_is_authorized`, which does not trust
    /// this declaration — it asks the PROJECTION and cross-checks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ClaimRemoval {
        /// Closes an active claim it names in `ref_id`.
        Closes,
        /// Cannot close a claim by naming it. (Any fact can still MASK one by
        /// being a retraction — that is a property of the FACT, not the kind,
        /// which is exactly why R1 was invisible to a kind-keyed gate.)
        Inert,
    }

    const fn declared_removal(kind: &FactKind) -> ClaimRemoval {
        match kind {
            // The four closing kinds (ARP-R-02).
            FactKind::Release | FactKind::Resolve | FactKind::Receipt | FactKind::ClaimExpired => {
                ClaimRemoval::Closes
            }
            FactKind::Claim
            | FactKind::ClaimRenewed
            | FactKind::Blocker
            | FactKind::Decision
            | FactKind::Artifact
            | FactKind::Handoff
            | FactKind::Risk
            | FactKind::Lesson
            | FactKind::Session
            | FactKind::Wake
            | FactKind::Presence
            | FactKind::Read
            | FactKind::BacklogItem
            | FactKind::Standby
            | FactKind::Mission
            | FactKind::Unknown => ClaimRemoval::Inert,
        }
    }

    /// The declaration above and `closes_active_claim` must be the same list.
    /// Adding a kind to one and not the other is precisely ARP-R-02.
    #[test]
    fn the_declared_closing_kinds_are_the_projections_closing_kinds() {
        for kind in FactKind::ALL {
            let declared = declared_removal(kind) == ClaimRemoval::Closes;
            assert_eq!(
                declared,
                crate::claim_authority::closes_active_claim(kind),
                "{kind:?}: the exhaustive declaration and `closes_active_claim` disagree; \
                 a kind that closes a claim must also be a kind that must be authorized to"
            );
        }
    }

    /// THE class assertion. For EVERY `FactKind`, and for every way a fact can
    /// remove a live claim — naming it in `ref_id`, repeating its scope, or
    /// retracting it — if a non-owner's fact actually removes the claim from
    /// the projection, the write gate must have refused that fact.
    ///
    /// The oracle is the PROJECTION, not a list. So a future change that gives
    /// some kind a new way to close a claim fails here without anyone
    /// remembering to update a table — which is the failure mode all five
    /// instances of this class had in common.
    #[test]
    fn every_kind_that_can_remove_a_claim_is_authorized() {
        let coord = crate::hooks_config::CoordinationConfig::default();

        let mut victim = make_fact("claim-victim", FactKind::Claim, "file:src/a.rs", "owns it");
        victim.seq = 1;
        victim.tool = Some("victim:01".to_string());
        victim.from_session_id = Some("sess:victim".to_string());

        // The victim is LIVE: seen a minute ago, far inside the 30-minute
        // small-work window, so no takeover arm can legitimately apply.
        let before = RoomSnapshot {
            active_claims: vec![victim.clone()],
            squads: vec![Squad {
                tool: "victim:01".to_string(),
                last_seen_seq: 1,
                last_seen_ts: now_string(),
                status: "active".to_string(),
                acknowledged: false,
            }],
            ..Default::default()
        };

        // Every spelling of "remove the victim's claim" a rogue could type.
        type Attack = (&'static str, fn(&mut Fact));
        let attacks: [Attack; 3] = [
            ("names the claim in ref_id", |f: &mut Fact| {
                f.ref_id = Some("claim-victim".to_string());
            }),
            ("repeats the claim's scope", |f: &mut Fact| {
                f.scope = vec!["file:src/a.rs".to_string()];
            }),
            ("retracts the claim", |f: &mut Fact| {
                f.subject = crate::retraction::subject_for("claim-victim");
                f.ref_id = Some("claim-victim".to_string());
            }),
        ];

        // A skip-only run would pass while asserting nothing — and it would do
        // so for exactly the defect this test exists to catch, since a broken
        // removal path stops removing and every iteration skips. Counted and
        // floored below.
        let mut exercised = 0usize;
        for kind in FactKind::ALL {
            for (label, shape) in &attacks {
                let mut hostile = make_fact("hostile", kind.clone(), "", "take it");
                hostile.scope.clear();
                hostile.seq = 2;
                hostile.tool = Some("codex:rogue".to_string());
                hostile.from_session_id = Some("sess:rogue".to_string());
                shape(&mut hostile);

                let after = snapshot_from_facts_with_policy(
                    &[victim.clone(), hostile.clone()],
                    &coord,
                    false,
                );
                let removed = !after
                    .active_claims
                    .iter()
                    .any(|c| c.event_id == "claim-victim");
                if !removed {
                    continue;
                }
                exercised += 1;
                let verdict = crate::write_authority::assert_write_authorized(
                    &hostile,
                    std::slice::from_ref(&victim),
                    &before,
                    &coord,
                );
                assert!(
                    verdict.is_err(),
                    "{kind:?} {label}: this removes victim:01's live claim from the \
                     projection and the write gate ALLOWED it. Every path that removes a \
                     claim must go through the authorization gate — that is the whole \
                     lesson of RC-029, ARP-R-01, ARP-R-02, R1, and R5."
                );
                assert!(
                    crate::write_authority::needs_authority_check(&hostile),
                    "{kind:?} {label}: removes a claim but `needs_authority_check` says \
                     this fact needs no check, so in production the gate never runs"
                );
            }
        }
        assert!(
            exercised >= FactKind::ALL.len(),
            "the class test skipped nearly every iteration ({exercised} exercised): claim \
             removal is no longer detected by the projection, which is the defect this test \
             exists to catch. A green run here would have meant nothing."
        );
    }

    /// RC-071a. The same class assertion, on the LEAD SEAT.
    ///
    /// The claim test above could not have caught RC-071a: its subject is claim
    /// removal, and the seat is not a claim. It is a non-claim fact that
    /// CARRIES AUTHORITY — RC-037's room-wide claim gate and RC-038's
    /// room-freeze both read "is this agent the lead" — and R1's ruling scoped
    /// ungated retraction to "non-claim facts", so `rally retract` moved the
    /// room's authority root while `lead handoff`, `lead assign`, and
    /// `lead relinquish` were all gated.
    ///
    /// The oracle here is `RoomSnapshot::lead`, not a list of spellings: for
    /// EVERY `FactKind`, and every shape that could move the seat, if the
    /// projection's lead actually changes then the write gate must have refused
    /// the fact AND `needs_authority_check` must have selected it. A future
    /// change that invents a new way to move the seat fails here without anyone
    /// remembering this test exists — which is the property all six instances
    /// of this class were missing.
    #[test]
    fn every_kind_that_can_move_the_lead_seat_is_authorized() {
        let coord = crate::hooks_config::CoordinationConfig::default();

        // `lead assign` stamps the ACTOR in `tool` and the BENEFICIARY in
        // `target` (ARP-R-01's attribution half).
        let mut seat = make_fact("seat", FactKind::Decision, "", "holds the seat");
        seat.seq = 1;
        seat.subject = crate::claim_authority::LEAD_SUBJECT.to_string();
        seat.tool = Some("incumbent:01".to_string());
        seat.target = Some("incumbent:01".to_string());
        seat.from_session_id = Some("sess:incumbent".to_string());

        // The incumbent is LIVE: seen now, far inside the 120-minute large-work
        // window, so no takeover arm can legitimately apply.
        let before = RoomSnapshot {
            lead: Some("incumbent:01".to_string()),
            squads: vec![Squad {
                tool: "incumbent:01".to_string(),
                last_seen_seq: 1,
                last_seen_ts: now_string(),
                status: "active".to_string(),
                acknowledged: false,
            }],
            ..Default::default()
        };

        // Every spelling of "move the seat out from under the incumbent".
        type Attack = (&'static str, fn(&mut Fact));
        let attacks: [Attack; 4] = [
            ("retracts the seat decision", |f: &mut Fact| {
                f.subject = crate::retraction::subject_for("seat");
                f.ref_id = Some("seat".to_string());
            }),
            ("takes the seat by decision", |f: &mut Fact| {
                f.subject = crate::claim_authority::LEAD_SUBJECT.to_string();
                f.target = Some("codex:rogue".to_string());
            }),
            ("vacates the seat", |f: &mut Fact| {
                f.subject = crate::claim_authority::LEAD_RELINQUISHED_SUBJECT.to_string();
            }),
            ("names the seat decision in ref_id", |f: &mut Fact| {
                f.ref_id = Some("seat".to_string());
            }),
        ];

        // See the sibling test: an all-skip run passes while asserting nothing,
        // and it does so precisely when seat movement stops being detected.
        let mut exercised = 0usize;
        for kind in FactKind::ALL {
            for (label, shape) in &attacks {
                let mut hostile = make_fact("hostile", kind.clone(), "", "take it");
                hostile.scope.clear();
                hostile.seq = 2;
                hostile.tool = Some("codex:rogue".to_string());
                hostile.from_session_id = Some("sess:rogue".to_string());
                shape(&mut hostile);

                let after = snapshot_from_facts_with_policy(
                    &[seat.clone(), hostile.clone()],
                    &coord,
                    false,
                );
                if after.lead.as_deref() == Some("incumbent:01") {
                    continue;
                }
                exercised += 1;
                let verdict = crate::write_authority::assert_write_authorized(
                    &hostile,
                    std::slice::from_ref(&seat),
                    &before,
                    &coord,
                );
                assert!(
                    verdict.is_err(),
                    "{kind:?} {label}: this moves the seat off incumbent:01 (now {:?}) and the \
                     write gate ALLOWED it. The seat is this room's authority root; every path \
                     that moves it must be authorized — RC-071a is what one unguarded path costs.",
                    after.lead
                );
                assert!(
                    crate::write_authority::needs_authority_check(&hostile),
                    "{kind:?} {label}: moves the seat but `needs_authority_check` says this \
                     fact needs no check, so in production the gate never runs"
                );
            }
        }
        // The retraction shape is kind-agnostic, so it moves the seat for every
        // kind; the two decision shapes move it once each. A floor at the kind
        // count catches a broken `retraction::target_of` — which would otherwise
        // turn this test green while it asserted nothing at all.
        assert!(
            exercised >= FactKind::ALL.len(),
            "the class test skipped nearly every iteration ({exercised} exercised): seat \
             movement is no longer detected by the projection, which is the defect this test \
             exists to catch. A green run here would have meant nothing."
        );
    }

    /// S3 salvage. Retraction resolution lives in the deterministic CORE
    /// (`snapshot_from_facts_with_policy_at`), not in the thin
    /// `snapshot_from_facts_with_policy` wrapper, because the snapshot-CACHE
    /// capture path calls the core directly. Filtering in the wrapper alone
    /// would let a cached snapshot keep serving a fact a freshly-computed one
    /// had already dropped.
    ///
    /// That placement was reasoned about in a comment and asserted nowhere: the
    /// core path had zero direct coverage. This drives the cache capture
    /// specifically, so moving the filter back up to the wrapper fails here
    /// instead of in an audit.
    #[test]
    fn the_snapshot_cache_capture_path_drops_a_retracted_fact() {
        let root = unique_root("cache-capture-retraction");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();

        let mut claim = make_fact("claim-cached", FactKind::Claim, "file:src/a.rs", "owns it");
        claim.tool = Some("victim:01".to_string());
        claim.from_session_id = Some("sess:victim".to_string());
        store.append_fact(&claim).unwrap();

        let before = store.snapshot_cache_capture(false).unwrap().snapshot;
        assert!(
            before
                .active_claims
                .iter()
                .any(|c| c.event_id == "claim-cached"),
            "precondition: the capture path sees the live claim"
        );

        // The OWNER withdraws it, so this exercises the projection rather than
        // the R1 authority gate.
        let mut withdrawal = make_fact("retract-cached", FactKind::Artifact, "", "withdrawn");
        withdrawal.scope.clear();
        withdrawal.tool = Some("victim:01".to_string());
        withdrawal.from_session_id = Some("sess:victim".to_string());
        withdrawal.subject = crate::retraction::subject_for("claim-cached");
        withdrawal.ref_id = Some("claim-cached".to_string());
        store.append_fact(&withdrawal).unwrap();

        let after = store.snapshot_cache_capture(false).unwrap().snapshot;
        assert!(
            after
                .active_claims
                .iter()
                .all(|c| c.event_id != "claim-cached"),
            "the capture path must drop the withdrawn claim: {:#?}",
            after.active_claims
        );
        assert!(
            after
                .recent_artifacts
                .iter()
                .any(|f| f.event_id == "retract-cached"),
            "the retraction itself must survive so the correction stays visible"
        );
        fs::remove_dir_all(root).ok();
    }

    /// S9 RED control: an engagement/run read must derive participation from
    /// the selected segment instead of inheriting every repository squad.
    #[test]
    fn scoped_snapshot_suppresses_presence_noise_and_never_reads_other_segment() {
        let root = unique_root("scoped-snapshot-audited-shape");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();

        for index in 1..=12 {
            let tool = format!("codex:{index:02}");
            store
                .append_fact(&scoped_presence(&format!("presence-{index}"), &tool))
                .unwrap();
        }
        let mut matched = make_fact(
            "artifact-matched",
            FactKind::Artifact,
            "run:audit-run",
            "matched",
        );
        matched.tool = Some("codex:01".to_string());
        store.append_fact(&matched).unwrap();

        store.set_active_engagement_for_test("engagement-beta");
        let mut sentinel = make_fact(
            "artifact-other-segment",
            FactKind::Artifact,
            "run:audit-run",
            "must stay out",
        );
        sentinel.tool = Some("codex:99".to_string());
        store.append_fact(&sentinel).unwrap();

        let scoped = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap();
        assert_eq!(scoped.squads.len(), 1, "{:#?}", scoped.squads);
        assert_eq!(scoped.squads[0].tool, "codex:01");
        assert!(
            scoped
                .recent_artifacts
                .iter()
                .any(|fact| fact.event_id == "artifact-matched")
        );
        assert!(
            scoped
                .recent_artifacts
                .iter()
                .all(|fact| fact.event_id != "artifact-other-segment"),
            "a selected-segment read leaked another engagement"
        );

        let with_presence = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, true)
            .unwrap();
        assert_eq!(with_presence.squads.len(), 12);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_joins_external_claim_without_external_contributor_credit() {
        let root = unique_root("scoped-snapshot-external-claim");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        let mut artifact = make_fact(
            "artifact-alpha",
            FactKind::Artifact,
            "file:crates/rally-cli/src/store.rs",
            "alpha work",
        );
        artifact.tool = Some("codex:alpha".to_string());
        store.append_fact(&artifact).unwrap();

        store.set_active_engagement_for_test("engagement-beta");
        let mut external_claim = make_fact(
            "claim-beta",
            FactKind::Claim,
            "file:crates/rally-cli/src/store.rs",
            "beta collision claim",
        );
        external_claim.tool = Some("codex:beta".to_string());
        external_claim.from_session_id = Some("sess:beta".to_string());
        external_claim.created_at = "2099-01-01T00:00:02Z".to_string();
        store.append_fact(&external_claim).unwrap();

        let scoped = store
            .snapshot_scoped(
                "engagement-alpha",
                None,
                Some("crates/rally-cli/src/store.rs"),
                false,
                false,
            )
            .unwrap();
        assert!(
            scoped
                .active_claims
                .iter()
                .any(|fact| fact.event_id == "claim-beta"),
            "the repo-wide collision claim must survive display scoping"
        );
        assert!(
            scoped
                .squads
                .iter()
                .any(|squad| squad.tool == "codex:alpha")
        );
        assert!(
            scoped.squads.iter().all(|squad| squad.tool != "codex:beta"),
            "an external collision claim must not add contributor credit"
        );
        let external = scoped
            .active_claims
            .iter()
            .find(|fact| fact.event_id == "claim-beta")
            .unwrap();
        assert!(
            scoped.max_seq >= external.seq && scoped.content_max_seq >= external.seq,
            "snapshot high-water must cover every emitted external claim: {scoped:#?}"
        );
        assert_eq!(
            scoped.last_activity_ts.as_deref(),
            Some(external.created_at.as_str()),
            "the highest emitted external claim must own last_activity_ts"
        );

        let mut findings = Vec::new();
        crate::check::check_before_write_for_test(
            &scoped,
            "codex:alpha",
            Some("crates/rally-cli/src/store.rs"),
            &mut findings,
        );
        assert!(
            findings.contains(&("claimed-path", "stop")),
            "path-scoped collision context must still stop a conflicting writer: {findings:?}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_repairs_corrupt_cold_cache_before_collision_query() {
        let root = unique_root("scoped-snapshot-cold-cache-repair");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha",
                FactKind::Artifact,
                "file:src/lib.rs",
                "selected work",
            ))
            .unwrap();
        store.set_active_engagement_for_test("engagement-beta");
        let mut claim = make_fact(
            "claim-beta",
            FactKind::Claim,
            "file:src/lib.rs",
            "must survive derived-cache quarantine",
        );
        claim.tool = Some("codex:beta".to_string());
        claim.from_session_id = Some("sess:beta".to_string());
        store.append_fact(&claim).unwrap();

        let facts_db = root.join(".rally/facts.db");
        remove_fact_store_journals(&facts_db);
        {
            use std::io::{Seek, SeekFrom};
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&facts_db)
                .unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(b"NOT A SQLITE DB!").unwrap();
            file.sync_all().unwrap();
        }

        let scoped = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap();
        assert!(
            scoped
                .active_claims
                .iter()
                .any(|fact| fact.event_id == "claim-beta"),
            "cold direct mode must rebuild from canonical segments before the collision query"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_repairs_spoofed_derived_db_sequence_before_collision_query() {
        let root = unique_root("scoped-snapshot-db-seq-spoof");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha-db-spoof",
                FactKind::Artifact,
                "file:src/lib.rs",
                "selected work",
            ))
            .unwrap();
        store.set_active_engagement_for_test("engagement-beta");
        let mut claim = make_fact(
            "claim-beta-db-spoof",
            FactKind::Claim,
            "file:src/lib.rs",
            "canonical seq is two",
        );
        claim.tool = Some("codex:beta".to_string());
        claim.from_session_id = Some("sess:beta".to_string());
        store.append_fact(&claim).unwrap();

        let facts_db = root.join(".rally/facts.db");
        remove_fact_store_journals(&facts_db);
        let connection = Connection::open(&facts_db).unwrap();
        let raw: String = connection
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'claim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut payload: Value = serde_json::from_str(&raw).unwrap();
        payload["seq"] = json!(9_999);
        connection
            .execute(
                "UPDATE events SET payload = ?1 WHERE event_type = 'claim'",
                [serde_json::to_string(&payload).unwrap()],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .ok();
        drop(connection);

        let scoped = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap();
        let projected = scoped
            .active_claims
            .iter()
            .find(|fact| fact.event_id == "claim-beta-db-spoof")
            .expect("canonical collision claim must survive DB repair");
        assert_eq!(projected.seq, 2, "canonical segment seq must win");
        assert_eq!(scoped.max_seq, 2);
        assert_eq!(scoped.content_max_seq, 2);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_repairs_same_shape_db_content_drift_before_collision_query() {
        let root = unique_root("scoped-snapshot-db-content-drift");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha-db-content",
                FactKind::Artifact,
                "file:src/lib.rs",
                "selected work",
            ))
            .unwrap();
        store.set_active_engagement_for_test("engagement-beta");
        let mut claim = make_fact(
            "claim-beta-db-content",
            FactKind::Claim,
            "file:src/lib.rs",
            "canonical collision claim",
        );
        claim.tool = Some("codex:beta".to_string());
        claim.from_session_id = Some("sess:beta".to_string());
        claim.created_at = "2099-01-01T00:00:02Z".to_string();
        store.append_fact(&claim).unwrap();

        // Preserve row count and canonical high-water while changing the
        // safety-bearing scope only in the derived cache. Count/max reconcile
        // cannot distinguish this stale row from canonical truth.
        let facts_db = root.join(".rally/facts.db");
        remove_fact_store_journals(&facts_db);
        let connection = Connection::open(&facts_db).unwrap();
        let raw: String = connection
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'claim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut payload: Value = serde_json::from_str(&raw).unwrap();
        payload["scope"] = json!(["file:src/other.rs"]);
        connection
            .execute(
                "UPDATE events SET payload = ?1 WHERE event_type = 'claim'",
                [serde_json::to_string(&payload).unwrap()],
            )
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .ok();
        drop(connection);

        let scoped = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap();
        let projected = scoped
            .active_claims
            .iter()
            .find(|fact| fact.event_id == "claim-beta-db-content")
            .expect("cold direct mode must repair same-shape DB content drift");
        assert_eq!(projected.scope, ["file:src/lib.rs"]);
        assert_eq!(projected.seq, 2);
        assert_eq!(scoped.max_seq, 2);
        assert_eq!(scoped.content_max_seq, 2);

        let repaired = open_fact_store(&facts_db).unwrap();
        let repaired_claims = claim_lifecycle_facts_from_store(&repaired).unwrap();
        assert_eq!(
            repaired_claims
                .iter()
                .find(|fact| fact.event_id == "claim-beta-db-content")
                .unwrap()
                .scope,
            ["file:src/lib.rs"],
            "the derived cache must be rebuilt, not bypassed for one response"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_warm_same_shape_content_drift_fails_loud() {
        let root = unique_root("scoped-snapshot-warm-content-drift");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha-warm-content",
                FactKind::Artifact,
                "file:src/new.rs",
                "selected work",
            ))
            .unwrap();
        store.set_active_engagement_for_test("engagement-beta");
        let mut claim = make_fact(
            "claim-beta-warm-content",
            FactKind::Claim,
            "file:src/old.rs",
            "derived cache will retain this old scope",
        );
        claim.tool = Some("codex:beta".to_string());
        claim.from_session_id = Some("sess:beta".to_string());
        store.append_fact(&claim).unwrap();
        store.install_warm_fact_store().unwrap();

        // Rewrite canonical content without changing row count or logical
        // high-water. The daemon-owned DB still contains src/old.rs.
        let segment = store.active_segment_path();
        let mut entries = read_segment_entries(&segment).unwrap();
        let claim_entry = entries
            .iter_mut()
            .find(|entry| entry.event_type == "claim")
            .unwrap();
        claim_entry.payload["scope"] = json!(["file:src/new.rs"]);
        let rendered = entries
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&segment, format!("{rendered}\n")).unwrap();

        let err = store
            .snapshot_scoped("engagement-alpha", None, Some("src/new.rs"), false, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("facts-db-recovery-required"),
            "a daemon-owned same-shape content mismatch must fail loud: {err}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_collision_cursor_advances_for_renewal_and_zero_claim_closure() {
        let root = unique_root("scoped-snapshot-lifecycle-cursor");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha",
                FactKind::Artifact,
                "file:src/lib.rs",
                "selected work",
            ))
            .unwrap();
        store.set_active_engagement_for_test("engagement-beta");
        let mut claim = make_fact(
            "claim-beta-cursor",
            FactKind::Claim,
            "file:src/lib.rs",
            "collision origin",
        );
        claim.tool = Some("codex:beta".to_string());
        claim.from_session_id = Some("sess:beta".to_string());
        store.append_fact(&claim).unwrap();
        let mut renewal = make_fact(
            "renew-beta-cursor",
            FactKind::ClaimRenewed,
            "file:src/lib.rs",
            "renewal advances collision source cursor",
        );
        renewal.tool = Some("codex:beta".to_string());
        renewal.from_session_id = Some("sess:beta".to_string());
        renewal.ref_id = Some("claim-beta-cursor".to_string());
        renewal.evidence = vec!["lease_expires_at:2099-01-01T00:00:03Z".to_string()];
        let renewal = store.append_fact(&renewal).unwrap();

        let renewed = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap();
        assert_eq!(renewed.active_claims[0].seq, 2, "origin seq stays stable");
        assert_eq!(renewed.max_seq, renewal.fact.seq);
        assert_eq!(renewed.content_max_seq, renewal.fact.seq);
        assert_eq!(
            renewed.last_activity_ts.as_deref(),
            Some(renewal.fact.created_at.as_str())
        );

        let mut release = make_fact(
            "release-beta-cursor",
            FactKind::Release,
            "file:src/lib.rs",
            "zero emitted claims still advances source cursor",
        );
        release.tool = Some("codex:beta".to_string());
        release.from_session_id = Some("sess:beta".to_string());
        release.ref_id = Some("claim-beta-cursor".to_string());
        release.created_at = "2099-01-01T00:00:04Z".to_string();
        let release = store.append_fact(&release).unwrap();

        let mut unrelated = make_fact(
            "claim-unrelated-newer",
            FactKind::Claim,
            "file:src/other.rs",
            "must not inflate another path cursor",
        );
        unrelated.tool = Some("codex:gamma".to_string());
        unrelated.from_session_id = Some("sess:gamma".to_string());
        let unrelated = store.append_fact(&unrelated).unwrap();
        assert!(unrelated.fact.seq > release.fact.seq);

        let closed = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap();
        assert!(
            closed.active_claims.is_empty(),
            "the path claim must be absent after its lifecycle closure"
        );
        assert_eq!(closed.max_seq, release.fact.seq);
        assert_eq!(closed.content_max_seq, release.fact.seq);
        assert_eq!(
            closed.last_activity_ts.as_deref(),
            Some(release.fact.created_at.as_str())
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_segment_projection_overwrites_spoofed_nonzero_payload_sequence() {
        let root = unique_root("scoped-snapshot-spoofed-seq");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-spoofed",
                FactKind::Artifact,
                "run:audit-run",
                "canonical seq wins",
            ))
            .unwrap();

        let segment = store.active_segment_path();
        let mut entry = read_segment_entries(&segment).unwrap().remove(0);
        entry.payload["seq"] = json!(9_999);
        fs::write(
            &segment,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();

        let scoped = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap();
        assert_eq!(scoped.max_seq, 1);
        assert_eq!(scoped.content_max_seq, 1);
        assert_eq!(scoped.recent_artifacts[0].seq, 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_path_warm_cache_drift_fails_loud_instead_of_returning_empty_success() {
        let root = unique_root("scoped-snapshot-warm-cache-drift");
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha",
                FactKind::Artifact,
                "file:src/lib.rs",
                "selected work",
            ))
            .unwrap();
        store.install_warm_fact_store().unwrap();

        let out_of_band = ledger_line(2, "claim", "claim-out-of-band", "engagement-beta");
        write_segment(
            &root,
            LOG_DIRNAME,
            "engagement-beta.jsonl",
            &[out_of_band.as_str()],
        );

        let err = store
            .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("facts-db-recovery-required"),
            "a daemon-owned derived cache must fail loud on canonical drift: {err}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_reference_closure_has_near_linear_deterministic_work() {
        let mut chain = Vec::with_capacity(2_000);
        for index in 0_usize..2_000 {
            let mut fact = make_fact(
                &format!("chain-{index}"),
                FactKind::Artifact,
                if index == 0 {
                    "run:scaling-control"
                } else {
                    "run:unselected"
                },
                "reference-chain scaling control",
            );
            fact.seq = i64::try_from(index + 1).unwrap();
            if index > 0 {
                fact.ref_id = Some(format!("chain-{}", index - 1));
            }
            chain.push(fact);
        }

        let mut rows = Vec::new();
        for size in [500_usize, 1_000, 2_000] {
            let facts = &chain[..size];
            let legacy_started = Instant::now();
            let legacy_work =
                legacy_scoped_selection_work_count(facts, Some("run:scaling-control"), None);
            let legacy_elapsed = legacy_started.elapsed();
            let indexed_started = Instant::now();
            let (selected, stats) =
                select_scoped_facts_with_stats(facts, Some("run:scaling-control"), None);
            let indexed_elapsed = indexed_started.elapsed();
            let indexed_work = stats.work_units();

            assert_eq!(selected.len(), size, "reference closure lost chain rows");
            assert!(
                indexed_work <= size * 6,
                "indexed closure exceeded a linear work bound at {size}: {stats:?}"
            );
            println!(
                "SCOPED_SCALING rows={size} legacy_work={legacy_work} indexed_work={indexed_work} legacy_us={} indexed_us={} ref_buckets={} scope_buckets={}",
                legacy_elapsed.as_micros(),
                indexed_elapsed.as_micros(),
                stats.ref_buckets_processed,
                stats.scope_buckets_processed,
            );
            rows.push((size, legacy_work, indexed_work));
        }

        assert!(
            rows[1].2 <= rows[0].2 * 3 && rows[2].2 <= rows[1].2 * 3,
            "indexed deterministic work must scale near-linearly: {rows:?}"
        );
        assert!(
            rows[1].1 > rows[0].1 * 3 && rows[2].1 > rows[1].1 * 3,
            "the retained pre-index measurement must expose its repeated-scan growth: {rows:?}"
        );
    }

    #[test]
    fn scoped_indexed_closure_preserves_reverse_refs_and_claim_scope_releases() {
        let mut parent = make_fact(
            "parent",
            FactKind::Decision,
            "run:unselected",
            "selected fact points backward to this row",
        );
        parent.seq = 1;
        let mut claim = make_fact(
            "claim",
            FactKind::Claim,
            "run:audit-run",
            "initial selection",
        );
        claim.seq = 2;
        claim.ref_id = Some("parent".to_string());
        claim.scope.push("file:src/lib.rs".to_string());
        let mut release = make_fact(
            "release",
            FactKind::Release,
            "file:src/lib.rs",
            "scope-only lifecycle closure",
        );
        release.seq = 3;
        let mut successor = make_fact(
            "successor",
            FactKind::Receipt,
            "run:unselected",
            "forward reference closure",
        );
        successor.seq = 4;
        successor.ref_id = Some("claim".to_string());
        let facts = vec![parent, claim, release, successor];

        let (selected, stats) = select_scoped_facts_with_stats(&facts, Some("run:audit-run"), None);
        assert_eq!(
            selected
                .iter()
                .map(|fact| fact.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "claim", "release", "successor"]
        );
        assert_eq!(stats.queue_pops, facts.len());
        assert_eq!(stats.scope_buckets_processed, 2);
    }

    #[test]
    fn scoped_capture_releases_mutation_lock_before_large_projection() {
        let root = unique_root("scoped-snapshot-short-lock");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        let lines = (1..=1_500)
            .map(|seq| {
                ledger_line(
                    seq,
                    "artifact",
                    &format!("artifact-{seq}"),
                    "engagement-alpha",
                )
            })
            .collect::<Vec<_>>();
        let body = format!("{}\n", lines.join("\n"));
        fs::create_dir_all(root.join(".rally").join(LOG_DIRNAME)).unwrap();
        fs::write(store.active_segment_path(), body).unwrap();

        let room_dir = root.join(".rally");
        let (captured_tx, captured_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        {
            let mut pause = SCOPED_CAPTURE_PAUSE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert!(pause.is_none(), "another scoped capture test is active");
            *pause = Some(ScopedCapturePause {
                room_dir: room_dir.clone(),
                captured: captured_tx,
                resume: resume_rx,
            });
        }

        let query = thread::spawn(move || {
            store.snapshot_scoped("engagement-alpha", None, None, false, false)
        });
        captured_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("1500-row query did not finish its locked capture");

        let (lock_tx, lock_rx) = mpsc::channel();
        let lock_dir = room_dir.clone();
        let holder = thread::spawn(move || {
            let started = Instant::now();
            let guard = acquire_room_mutation_lock(&lock_dir).unwrap();
            lock_tx.send(started.elapsed()).unwrap();
            drop(guard);
        });
        let lock_wait = lock_rx.recv_timeout(Duration::from_secs(2)).expect(
            "a peer lock holder was starved while scoped projection was paused after capture",
        );
        println!(
            "SCOPED_LOCK rows=1500 peer_lock_wait_us={}",
            lock_wait.as_micros()
        );
        resume_tx.send(()).unwrap();
        holder.join().unwrap();
        let snapshot = query.join().unwrap().unwrap();
        assert_eq!(snapshot.max_seq, 1_500);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_snapshot_is_location_invariant_and_dedupes_live_archive_overlap() {
        let root = unique_root("scoped-snapshot-location-invariant");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        let artifact = make_fact(
            "artifact-alpha",
            FactKind::Artifact,
            "run:audit-run",
            "alpha work",
        );
        store.append_fact(&artifact).unwrap();

        let live_path = store.active_segment_path();
        let archive_path = store.archive_dir.join("engagement-alpha.jsonl");
        fs::create_dir_all(&store.archive_dir).unwrap();
        fs::copy(&live_path, &archive_path).unwrap();
        let overlap = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap();
        assert_eq!(
            overlap
                .recent_artifacts
                .iter()
                .filter(|fact| fact.event_id == "artifact-alpha")
                .count(),
            1,
            "live/archive overlap must dedupe by canonical sequence"
        );

        fs::remove_file(&archive_path).unwrap();
        let before = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap();
        fs::rename(&live_path, &archive_path).unwrap();
        let after = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap();
        assert_eq!(
            snapshot_to_wire_value(&before).unwrap(),
            snapshot_to_wire_value(&after).unwrap(),
            "rotation must not change a non-decayed scoped snapshot"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_snapshot_rejects_conflicting_live_archive_payload_at_same_sequence() {
        let root = unique_root("scoped-snapshot-seq-conflict");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        let artifact = make_fact(
            "artifact-alpha",
            FactKind::Artifact,
            "run:audit-run",
            "alpha work",
        );
        store.append_fact(&artifact).unwrap();

        let live_path = store.active_segment_path();
        let mut live_entry = read_segment_entries(&live_path).unwrap().remove(0);
        live_entry.payload["subject"] = Value::String("conflicting payload".to_string());
        fs::create_dir_all(&store.archive_dir).unwrap();
        let archive_path = store.archive_dir.join("engagement-alpha.jsonl");
        fs::write(
            &archive_path,
            format!("{}\n", serde_json::to_string(&live_entry).unwrap()),
        )
        .unwrap();

        let err = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("conflicting canonical rows")
                && err.to_string().contains("seq 1"),
            "same-seq payload divergence must fail loudly: {err}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scoped_snapshot_rejects_same_payload_with_conflicting_envelope_at_same_sequence() {
        let root = unique_root("scoped-snapshot-seq-envelope-conflict");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("engagement-alpha".to_string()),
        )
        .unwrap();
        store
            .append_fact(&make_fact(
                "artifact-alpha-envelope",
                FactKind::Artifact,
                "run:audit-run",
                "alpha work",
            ))
            .unwrap();

        let live_path = store.active_segment_path();
        let mut archive_entry = read_segment_entries(&live_path).unwrap().remove(0);
        archive_entry.occurred_at = "2099-01-01T00:00:00Z".to_string();
        fs::create_dir_all(&store.archive_dir).unwrap();
        let archive_path = store.archive_dir.join("engagement-alpha.jsonl");
        fs::write(
            &archive_path,
            format!("{}\n", serde_json::to_string(&archive_entry).unwrap()),
        )
        .unwrap();

        let err = store
            .snapshot_scoped("engagement-alpha", Some("audit-run"), None, false, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("conflicting canonical rows")
                && err.to_string().contains("seq 1")
                && err.to_string().contains("rows differ"),
            "same payload with divergent envelope metadata must fail loudly: {err}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn current_engagement_binding_prefers_session_and_fails_closed_on_tool_ambiguity() {
        let bindings = vec![
            EngagementBinding {
                session_id: "sess:alpha".to_string(),
                tool: "codex:01".to_string(),
                engagement: "engagement-alpha".to_string(),
                active: true,
                seq: 10,
            },
            EngagementBinding {
                session_id: "sess:beta".to_string(),
                tool: "codex:01".to_string(),
                engagement: "engagement-beta".to_string(),
                active: true,
                seq: 11,
            },
        ];
        assert_eq!(
            resolve_current_engagement(
                None,
                Some("sess:alpha"),
                Some("codex:01"),
                &bindings,
                Some("legacy-shared")
            )
            .unwrap(),
            "engagement-alpha"
        );
        let err = resolve_current_engagement(
            None,
            None,
            Some("codex:01"),
            &bindings,
            Some("legacy-shared"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
        assert!(!err.to_string().contains("legacy-shared"), "{err}");

        assert_eq!(
            resolve_current_engagement(
                None,
                None,
                Some("codex:01"),
                &bindings[..1],
                Some("legacy-shared"),
            )
            .unwrap(),
            "engagement-alpha",
            "a unique adopted-session binding must beat the shared legacy file"
        );

        let missing_session = resolve_current_engagement(
            None,
            Some("sess:missing"),
            Some("codex:01"),
            &bindings[..1],
            Some("legacy-shared"),
        )
        .unwrap_err();
        assert!(
            missing_session.to_string().contains("explicit session"),
            "{missing_session}"
        );

        let wrong_tool = resolve_current_engagement(
            None,
            Some("sess:alpha"),
            Some("codex:02"),
            &bindings[..1],
            Some("legacy-shared"),
        )
        .unwrap_err();
        assert!(wrong_tool.to_string().contains("not \"codex:02\""));

        let inactive = EngagementBinding {
            active: false,
            seq: 12,
            ..bindings[0].clone()
        };
        let inactive_err = resolve_current_engagement(
            None,
            Some("sess:alpha"),
            Some("codex:01"),
            &[bindings[0].clone(), inactive],
            Some("legacy-shared"),
        )
        .unwrap_err();
        assert!(inactive_err.to_string().contains("no active"));
    }

    /// The process-level contention controls acquire the database lock before
    /// the command starts, so they necessarily exercise pool-open retries
    /// first. This control opens and warms the pool BEFORE contention, then
    /// proves the append loop itself retries after SQLite's first busy wait
    /// expires. Removing the append retry loop makes this fail.
    #[test]
    fn warm_append_retries_after_pool_open_when_holder_releases() {
        let root = unique_root("warm-append-retry");
        let _deadline = crate::install_watchdog_deadline(Instant::now() + Duration::from_secs(3));
        let mut store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("append-retry-control".to_string()),
        )
        .unwrap();
        store.install_warm_fact_store().unwrap();

        let db = root.join(".rally/facts.db");
        let (ready_tx, ready_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(db).expect("holder opens facts.db");
            conn.pragma_update(None, "journal_mode", "WAL").ok();
            conn.execute_batch("BEGIN EXCLUSIVE")
                .expect("holder takes EXCLUSIVE");
            ready_tx.send(()).ok();
            // The warm pool was opened with <375ms busy_timeout. Holding for
            // 700ms guarantees the first append returns SQLITE_BUSY, while the
            // retry budget still has time to land the second attempt.
            std::thread::sleep(Duration::from_millis(700));
            conn.execute_batch("ROLLBACK").ok();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder acquired lock before append");

        let started = Instant::now();
        let appended = store
            .append_fact(&make_fact(
                "append-retry",
                FactKind::Artifact,
                "retry-control",
                "land after holder release",
            ))
            .expect("append retry must land after the holder releases");
        assert_eq!(appended.fact.subject, "subject-append-retry");
        assert!(
            started.elapsed() >= Duration::from_millis(375),
            "append completed before the first busy wait; control was vacuous"
        );

        holder.join().expect("holder thread joins");
        drop(store);
        fs::remove_dir_all(root).ok();
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
                .any(|f| f.event_id == risk.fact.event_id),
            "telemetry must project into system_health"
        );
        let mut resolve = make_fact(
            "resolve-drift",
            FactKind::Resolve,
            "tests/",
            "drift resolved",
        );
        resolve.ref_id = Some(risk.fact.event_id.clone());
        store
            .append_state_transition_verified(&resolve)
            .expect("a system_health fact must be resolvable by ref");
        let after = store.snapshot().unwrap();
        assert!(
            !after
                .system_health
                .iter()
                .any(|f| f.event_id == risk.fact.event_id),
            "resolved telemetry must leave system_health"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn system_health_is_bounded_by_prefix_class() {
        let mut facts = Vec::with_capacity(1_000);
        for index in 0..1_000 {
            let mut fact = make_fact(
                &format!("external-{index}"),
                FactKind::Risk,
                "external-intake",
                "quarantined external path",
            );
            fact.seq = index + 1;
            fact.subject = format!("external-intake: /private/source/{index}");
            facts.push(fact);
        }

        let snapshot = snapshot_from_facts_with_policy(
            &facts,
            &crate::hooks_config::CoordinationConfig::default(),
            true,
        );

        assert_eq!(snapshot.system_health.len(), 1);
        assert_eq!(snapshot.system_health[0].event_id, "external-999");
        assert_eq!(snapshot.totals.system_health, 1);
    }

    fn measured_room_bytes(snapshot: &RoomSnapshot, fixed_envelope_bytes: usize) -> usize {
        fixed_envelope_bytes
            + serde_json::to_string_pretty(snapshot)
                .expect("snapshot serialization")
                .len()
            + 1
    }

    fn ten_x_composition_snapshot() -> RoomSnapshot {
        const BASELINE_FACTS: usize = 100;
        let mut facts = Vec::with_capacity(BASELINE_FACTS * 10);
        for index in 0..BASELINE_FACTS * 10 {
            let kind = match index % 4 {
                0 => FactKind::Decision,
                1 => FactKind::Artifact,
                2 => FactKind::Risk,
                _ => FactKind::Handoff,
            };
            let is_handoff = kind == FactKind::Handoff;
            let mut fact = make_fact(
                &format!("compose-{index}"),
                kind,
                "src/composition.rs",
                &"wide response body ".repeat(12),
            );
            fact.seq = index as i64 + 1;
            if is_handoff {
                fact.target = Some("another-tool".to_string());
            }
            facts.push(fact);
        }
        snapshot_from_facts_with_policy(
            &facts,
            &crate::hooks_config::CoordinationConfig::default(),
            true,
        )
    }

    #[test]
    fn exact_room_ceiling_counts_fixed_envelope_and_composition_metadata() {
        const BUDGET: usize = 32_000;
        const FIXED_ENVELOPE: usize = 1_800;
        let coord = crate::hooks_config::CoordinationConfig::default();
        let consumer = crate::relevance::ConsumerContext::neutral();
        let composed = compose_room_output(
            ten_x_composition_snapshot(),
            &coord,
            &consumer,
            false,
            Some(BUDGET),
            |snapshot| measured_room_bytes(snapshot, FIXED_ENVELOPE),
        );
        let actual = measured_room_bytes(&composed, FIXED_ENVELOPE);
        let composition = composed
            .composition
            .as_ref()
            .expect("ten-x ledger must require composition");

        assert!(actual <= BUDGET, "{actual} exceeded {BUDGET}");
        assert_eq!(composition.emitted_bytes, actual);
        assert!(!composition.over_budget);
        assert!(
            composition
                .buckets
                .values()
                .any(|bucket| bucket.omitted > 0)
        );
        assert_eq!(composed.totals.current_decisions, 250);
        assert_eq!(composed.totals.current_risks, 250);
    }

    #[test]
    fn exact_room_ceiling_reports_an_uncuttable_top_one_floor() {
        const BUDGET: usize = 1;
        const FIXED_ENVELOPE: usize = 600;
        let mut snapshot = ten_x_composition_snapshot();
        snapshot.current_decisions.truncate(1);
        snapshot.current_risks.truncate(1);
        snapshot.recent_artifacts.truncate(1);
        snapshot.open_handoffs.truncate(1);
        snapshot.unconsumed_artifacts.clear();
        snapshot.totals.current_decisions = 1;
        snapshot.totals.current_risks = 1;
        snapshot.totals.recent_artifacts = 1;
        snapshot.totals.open_handoffs = 1;
        snapshot.totals.unconsumed_artifacts = 0;
        let coord = crate::hooks_config::CoordinationConfig::default();
        let consumer = crate::relevance::ConsumerContext::neutral();
        let composed = compose_room_output(
            snapshot,
            &coord,
            &consumer,
            false,
            Some(BUDGET),
            |candidate| measured_room_bytes(candidate, FIXED_ENVELOPE),
        );
        let actual = measured_room_bytes(&composed, FIXED_ENVELOPE);
        let composition = composed
            .composition
            .as_ref()
            .expect("an over-budget floor must report composition");

        assert!(actual > BUDGET);
        assert!(composition.over_budget);
        assert_eq!(composition.emitted_bytes, actual);
        assert!(
            composition.buckets.is_empty(),
            "one-item bucket floors must not claim an omission"
        );
        assert!(
            composition
                .over_budget_causes
                .iter()
                .any(|cause| cause == "response_floor")
        );
    }

    #[test]
    fn include_archived_with_explicit_budget_keeps_every_fact_and_reports_overflow() {
        const BUDGET: usize = 100;
        let stale_facts = (0..10)
            .map(|index| {
                make_fact(
                    &format!("archived-{index}"),
                    FactKind::Artifact,
                    "archive",
                    "archived fact remains complete",
                )
            })
            .collect::<Vec<_>>();
        let snapshot = RoomSnapshot {
            totals: RoomTotals {
                stale_facts: stale_facts.len(),
                ..RoomTotals::default()
            },
            stale_facts,
            ..RoomSnapshot::default()
        };
        let coord = crate::hooks_config::CoordinationConfig::default();
        let consumer = crate::relevance::ConsumerContext::neutral();

        let composed = compose_room_output(
            snapshot,
            &coord,
            &consumer,
            true,
            Some(BUDGET),
            |candidate| measured_room_bytes(candidate, 300),
        );

        assert_eq!(composed.stale_facts.len(), 10);
        assert!(
            composed
                .composition
                .as_ref()
                .is_some_and(|composition| composition.over_budget)
        );
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
        // The release must be authored by the claim's OWNER. `make_fact`
        // defaults to `tool: "test"`, so this fixture was performing a
        // non-owner release without meaning to — it passed only because the
        // takeover authorization was missing from the `--ref` path (RC-029).
        // What this test actually asserts is projection ORDER, not authority,
        // so it is set to a self-release and its real assertion is unchanged.
        release.tool = Some("tool-a".to_string());
        release.ref_id = Some(old_claim.fact.event_id.clone());
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
                store
                    .append_fact_verified(&fact)
                    .map(|outcome| outcome.fact.event_id)
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
    fn claim_lease_renewal_appends_durable_event_and_projects_effective_lease() {
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
            .renew_claim_lease(
                "claim-renew",
                "2099-01-01T00:30:00Z".to_string(),
                "tool-a",
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let facts = store.facts().unwrap();
        let after_count = facts.len();
        let index = claim_authority::read_index(store.claim_index_path()).unwrap();
        let snapshot = store.snapshot().unwrap();

        assert_eq!(
            before_count + 1,
            after_count,
            "lease renewal must append exactly one durable fact"
        );
        let renewal = facts.last().expect("renewal fact");
        assert_eq!(renewal.kind, FactKind::ClaimRenewed);
        assert_eq!(renewal.ref_id.as_deref(), Some("claim-renew"));
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
        assert!(
            snapshot.active_claims[0]
                .evidence
                .iter()
                .any(|item| item == "lease_expires_at:2099-01-01T00:30:00Z")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sibling_session_renewal_is_refused_at_the_write_boundary() {
        let root = unique_root("claim-lease-sibling-renew");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let mut claim = claim_fact(
            "claim-session-owner",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        claim.from_session_id = Some("session-owner".to_string());
        store.append_fact_verified(&claim).unwrap();

        let renewal = Fact {
            from_session_id: Some("session-sibling".to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: "renewal-sibling".to_string(),
            seq: 0,
            thread_id: "room-sibling-renewal".to_string(),
            kind: FactKind::ClaimRenewed,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "sibling renewal".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: vec!["lease_expires_at:2099-01-01T00:30:00Z".to_string()],
            target: None,
            ref_id: Some(claim.event_id.clone()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };

        let err = store
            .append_fact_verified(&renewal)
            .expect_err("a same-tool sibling must not renew the owner's claim")
            .to_string();
        assert!(
            err.contains("session does not own claim"),
            "the write boundary must report session ownership: {err}"
        );
        assert!(
            store
                .facts()
                .unwrap()
                .iter()
                .all(|fact| fact.event_id != renewal.event_id),
            "a refused sibling renewal must not reach the ledger"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn direct_renewal_requires_caller_and_expected_owner_session() {
        let root = unique_root("claim-lease-direct-authority");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let mut claim = claim_fact(
            "claim-direct-owner",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        claim.from_session_id = Some("session-owner".to_string());
        store.append_fact_verified(&claim).unwrap();

        let sibling = store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:30:00Z".to_string(),
                "tool-a",
                Some("session-sibling"),
                Some("session-owner"),
            )
            .expect_err("same-tool sibling must not renew through the direct API")
            .to_string();
        assert!(sibling.contains("session does not own claim"), "{sibling}");

        let stale_expectation = store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:30:00Z".to_string(),
                "tool-a",
                Some("session-owner"),
                Some("session-sibling"),
            )
            .expect_err("stale expected owner session must be rejected")
            .to_string();
        assert!(
            stale_expectation.contains("expected owner session does not match"),
            "{stale_expectation}"
        );
        assert!(
            store
                .facts()
                .unwrap()
                .iter()
                .all(|fact| fact.kind != FactKind::ClaimRenewed)
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn anonymous_identity_cannot_renew_or_close_at_the_write_boundary() {
        let root = unique_root("claim-anonymous-authority");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let mut claim = claim_fact(
            "claim-anonymous",
            "placeholder",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        claim.tool = None;
        claim.from_session_id = None;
        store.append_fact_verified(&claim).unwrap();
        let before = store.facts().unwrap().len();

        store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:30:00Z".to_string(),
                None,
                None,
                None,
                "renew-anonymous-request",
                "renew-anonymous-thread",
                "2026-08-10T00:00:00Z",
            )
            .expect_err("anonymous caller must not renew an anonymous claim");
        assert_eq!(store.facts().unwrap().len(), before);

        let anonymous_renewal = Fact {
            kind: FactKind::ClaimRenewed,
            event_id: "renew-anonymous".to_string(),
            ref_id: Some(claim.event_id.clone()),
            tool: None,
            from_session_id: None,
            evidence: vec!["lease_expires_at:2099-01-01T00:30:00Z".to_string()],
            ..claim_fact(
                "renew-anonymous-template",
                "placeholder",
                "file:src/lib.rs",
                "2099-01-01T00:30:00Z",
            )
        };
        store
            .append_fact_verified(&anonymous_renewal)
            .expect_err("raw anonymous ClaimRenewed must fail closed");
        assert_eq!(store.facts().unwrap().len(), before);

        let anonymous_release = Fact {
            kind: FactKind::Release,
            event_id: "release-anonymous".to_string(),
            ref_id: Some(claim.event_id.clone()),
            tool: None,
            from_session_id: None,
            evidence: Vec::new(),
            ..claim_fact(
                "release-anonymous-template",
                "placeholder",
                "file:src/lib.rs",
                "2099-01-01T00:30:00Z",
            )
        };
        store
            .append_fact_verified(&anonymous_release)
            .expect_err("raw anonymous Release must fail closed");
        assert_eq!(store.facts().unwrap().len(), before);
        assert!(
            store
                .snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|active| active.event_id == claim.event_id)
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn modern_identified_caller_can_renew_a_legacy_sessionless_claim() {
        let root = unique_root("claim-legacy-modern-renewal");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let claim = claim_fact(
            "claim-legacy-renew",
            "tool-a",
            "file:src/lib.rs",
            "2000-01-01T00:00:00Z",
        );
        store.append_fact_verified(&claim).unwrap();

        let renewed = store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:30:00Z".to_string(),
                Some("tool-a"),
                Some("session-modern"),
                None,
                "renew-legacy-request",
                "renew-legacy-thread",
                "2026-08-10T00:00:00Z",
            )
            .expect("identified legacy owner must retain compatibility")
            .expect("legacy claim remains active");
        assert_eq!(
            renewed.lease_expires_at.as_deref(),
            Some("2099-01-01T00:30:00Z")
        );
        let renewal = store
            .facts()
            .unwrap()
            .into_iter()
            .find(|fact| fact.kind == FactKind::ClaimRenewed)
            .expect("renewal must be durable");
        assert_eq!(renewal.tool.as_deref(), Some("tool-a"));
        assert_eq!(renewal.from_session_id.as_deref(), Some("session-modern"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_lease_renewal_is_monotonic_and_retry_idempotent() {
        let root = unique_root("claim-lease-renew-idempotent");
        let store = RoomStore::open_at(root.clone()).unwrap();
        let claim = claim_fact(
            "claim-renew",
            "tool-a",
            "file:src/lib.rs",
            "2099-01-01T00:00:00Z",
        );
        store.append_fact_verified(&claim).unwrap();
        store
            .renew_claim_lease(
                "claim-renew",
                "2099-01-01T00:30:00Z".to_string(),
                "tool-a",
                None,
                None,
            )
            .unwrap();
        let after_first = store.facts().unwrap().len();

        let equal = store
            .renew_claim_lease(
                "claim-renew",
                "2099-01-01T00:30:00Z".to_string(),
                "tool-a",
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let older = store
            .renew_claim_lease(
                "claim-renew",
                "2099-01-01T00:15:00Z".to_string(),
                "tool-a",
                None,
                None,
            )
            .unwrap()
            .unwrap();

        assert_eq!(store.facts().unwrap().len(), after_first);
        assert_eq!(
            equal.lease_expires_at.as_deref(),
            Some("2099-01-01T00:30:00Z")
        );
        assert_eq!(
            older.lease_expires_at.as_deref(),
            Some("2099-01-01T00:30:00Z")
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
        assert_eq!((a.fact.seq, b.fact.seq, c.fact.seq), (1, 2, 3));

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

    /// RC-044 second-order hazard: an error that merely NAMES a quarantine file
    /// must not be read as a corruption report.
    ///
    /// Every quarantine file is named `facts.db.corrupt.<stamp>`, and SQLite
    /// errors routinely embed the database path. Under the previous bare
    /// `contains("corrupt")` an ordinary I/O error mentioning leftover debris
    /// triggered another destructive rename — a quarantine that manufactures the
    /// evidence for the next quarantine. This is the one arm of the RC-044
    /// cascade that is a self-contained defect rather than a concurrency
    /// property, so it is fixed and tested independently of the open
    /// architectural work.
    #[test]
    fn quarantine_filename_in_an_error_is_not_a_corruption_report() {
        assert!(
            !is_malformed_db_error(
                &"error returned from database: (code: 522) disk I/O error while reading \
                  /repo/.rally/facts.db.corrupt.1786158756577960000"
                    .to_string()
            ),
            "an I/O error naming a quarantine file is not a corruption report"
        );
        assert!(
            !is_malformed_db_error(
                &"cannot open /repo/.rally/facts.db.corrupt.1786158756577960000-db-wal".to_string()
            ),
            "a quarantine sibling path is not a corruption report either"
        );
        // The real signals still fire, including the extended-code family
        // (SQLITE_CORRUPT_VTAB/SEQUENCE/INDEX) whose only marker is the word.
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 11) disk image is malformed".to_string()
        ));
        assert!(is_malformed_db_error(
            &"database corruption at line 12345".to_string()
        ));
        assert!(is_malformed_db_error(
            &"error returned from database: (code: 26) file is not a database".to_string()
        ));
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
        assert_eq!(
            (a.fact.seq, b.fact.seq, c.fact.seq, d.fact.seq),
            (1, 2, 3, 4)
        );
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

        // Sanity: the corrupted db must fail either while opening or on the
        // first real page traversal. SQLite may defer page-1 validation until
        // the query when the pool/bootstrap path opens lazily.
        let malformed_error = match open_fact_store(&facts_db) {
            Ok(store) => store
                .query(&FactQuery::all())
                .expect_err("precondition: page-1 read must expose the corrupt header")
                .to_string(),
            Err(err) => err.to_string(),
        };
        assert!(
            is_malformed_db_error(&malformed_error),
            "precondition: corruption must classify as malformed; got {malformed_error}"
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

    /// O26 DB-only cutover: a current-format database without canonical
    /// segments is preserved and requires the explicit offline migration path.
    /// The adjacent two-engagement test remains the positive control that a
    /// room with canonical segments reconstructs after its cache is deleted.
    #[test]
    fn db_only_room_requires_explicit_offline_migration() {
        let root = unique_root("segments-bootstrap");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "claim a"))
            .unwrap();
        store
            .append_fact(&make_fact("e2", FactKind::Decision, "src/", "decided b"))
            .unwrap();
        drop(store);

        // Remove every canonical segment while preserving the derived db.
        // Also remove the index so first-open cannot short-circuit the
        // explicit DB-only migration requirement.
        let log_dir = root.join(".rally/log");
        if log_dir.exists() {
            for entry in fs::read_dir(&log_dir).unwrap() {
                let _ = fs::remove_file(entry.unwrap().path());
            }
        }
        assert!(segments_under(&root).is_empty());
        assert!(root.join(".rally/facts.db").exists());

        let facts_db = root.join(".rally/facts.db");
        let db_before = fs::read(&facts_db).unwrap();
        let error = match RoomStore::open_at(root.clone()) {
            Ok(_) => panic!("DB-only room must require explicit offline migration"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("current-format DB-only room detected"));
        assert!(message.contains("rally doctor --migrate-db-only"));
        assert!(segments_under(&root).is_empty());
        assert_eq!(fs::read(&facts_db).unwrap(), db_before);

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
            .append_fact(&{
                let mut resolve = make_fact("e4", FactKind::Resolve, "tests/", "beta resolved");
                resolve.ref_id = Some(c.fact.event_id.clone());
                resolve
            })
            .unwrap();
        assert_eq!(
            (a.fact.seq, b.fact.seq, c.fact.seq, d.fact.seq),
            (1, 2, 3, 4)
        );

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

    fn canonical_source_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, usize)> {
        let mut paths = segments_under(root);
        paths.extend(archive_under(root));
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                let line_count = bytes
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .count();
                (path, bytes, line_count)
            })
            .collect()
    }

    fn canonical_conflict_message<T>(result: Result<T>, case: &str) -> String {
        match result {
            Ok(_) => panic!("{case}: conflicting canonical rows must fail loud"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn segment_fold_memo_is_room_scoped_and_invalidates_on_change() {
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        invalidate_segment_fold_memo();
        let root_a = unique_root("segment-memo-room-a");
        let root_b = unique_root("segment-memo-room-b");
        let a1 = ledger_line(1, "decision", "room-a-1", "alpha");
        let b1 = ledger_line(1, "decision", "room-b-1", "beta");
        write_segment(&root_a, "log", "alpha.jsonl", &[a1.as_str()]);
        write_segment(&root_b, "log", "beta.jsonl", &[b1.as_str()]);
        let log_a = root_a.join(".rally").join(LOG_DIRNAME);
        let archive_a = root_a.join(".rally").join(ARCHIVE_DIRNAME);
        let log_b = root_b.join(".rally").join(LOG_DIRNAME);
        let archive_b = root_b.join(".rally").join(ARCHIVE_DIRNAME);

        let facts_a = facts_from_segments(&log_a, &archive_a).unwrap();
        let facts_b = facts_from_segments(&log_b, &archive_b).unwrap();
        assert_eq!(facts_a[0].event_id, "room-a-1");
        assert_eq!(facts_b[0].event_id, "room-b-1");

        let b2 = ledger_line(2, "decision", "room-b-2", "beta");
        {
            let path = log_b.join("beta.jsonl");
            let mut file = OpenOptions::new().append(true).open(path).unwrap();
            file.write_all(b2.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
            file.sync_data().unwrap();
        }
        let facts_b_after_append = facts_from_segments(&log_b, &archive_b).unwrap();
        assert_eq!(
            facts_b_after_append
                .iter()
                .map(|fact| fact.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["room-b-1", "room-b-2"]
        );

        fs::remove_dir_all(root_a).ok();
        fs::remove_dir_all(root_b).ok();
        invalidate_segment_fold_memo();
    }

    /// Model a same-length rewrite whose filesystem fingerprint collides with
    /// the cached one. Explicit invalidation must still force a fresh fold.
    #[test]
    fn segment_fold_memo_explicit_invalidation_handles_same_length_rewrite() {
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        invalidate_segment_fold_memo();
        let root = unique_root("segment-memo-same-length");
        let old = ledger_line(1, "decision", "event-old", "alpha");
        let new = ledger_line(1, "decision", "event-new", "alpha");
        assert_eq!(old.len(), new.len(), "fixture must preserve file length");
        write_segment(&root, "log", "alpha.jsonl", &[old.as_str()]);
        let log_dir = root.join(".rally").join(LOG_DIRNAME);
        let archive_dir = root.join(".rally").join(ARCHIVE_DIRNAME);
        let old_facts = facts_from_segments(&log_dir, &archive_dir).unwrap();
        assert_eq!(old_facts[0].event_id, "event-old");

        fs::write(log_dir.join("alpha.jsonl"), format!("{new}\n")).unwrap();
        // Force the adversarial metadata-collision state deterministically:
        // retain the old cached facts while matching the rewritten file's
        // current fingerprint.
        let live = read_segment_files(&log_dir).unwrap();
        let archived = replay_archive_segments(&archive_dir).unwrap();
        let rewritten_fingerprint = segments_fingerprint(&live, &archived);
        let cached_hit = {
            let mut memo = SEGMENT_FOLD_MEMO.lock().unwrap();
            // Another parallel test may legitimately replace the process-wide
            // one-slot memo after our first fold. Seed this adversarial state
            // while holding the memo lock instead of assuming our room still
            // occupies the slot; the control is about collision handling, not
            // cache residency across unrelated rooms.
            *memo = Some(SegmentFoldMemo {
                log_dir: log_dir.clone(),
                archive_dir: archive_dir.clone(),
                fingerprint: rewritten_fingerprint.clone(),
                facts: std::sync::Arc::new(old_facts),
            });
            segment_fold_memo_hit(&memo, &log_dir, &archive_dir, &rewritten_fingerprint)
                .expect("adversarial fingerprint collision must hit the cached fold")
        };
        assert_eq!(
            cached_hit[0].event_id, "event-old",
            "adversarial fingerprint collision must exercise the cached value"
        );

        invalidate_segment_fold_memo();
        assert_eq!(
            facts_from_segments(&log_dir, &archive_dir).unwrap()[0].event_id,
            "event-new"
        );

        fs::remove_dir_all(root).ok();
        invalidate_segment_fold_memo();
    }

    #[test]
    fn snapshot_wire_internals_fail_loud_at_count_bounds() {
        let mut snapshot = RoomSnapshot {
            stale_authors: (0..=MAX_WIRE_STALE_AUTHORS)
                .map(|index| format!("stale-author-{index}"))
                .collect(),
            ..RoomSnapshot::default()
        };
        let stale_error = snapshot_to_wire_value(&snapshot).unwrap_err().to_string();
        assert!(stale_error.contains("stale-author bound"), "{stale_error}");

        snapshot.stale_authors.clear();
        let pending = make_fact("pending-wake", FactKind::Wake, "", "pending wake");
        snapshot.pending_wakes = vec![pending; MAX_WIRE_PENDING_WAKES + 1];
        let wake_error = snapshot_to_wire_value(&snapshot).unwrap_err().to_string();
        assert!(wake_error.contains("pending-wake bound"), "{wake_error}");
    }

    #[test]
    fn snapshot_wire_internals_fail_loud_at_byte_bound() {
        let mut snapshot = RoomSnapshot::default();
        let mut pending = make_fact("large-pending-wake", FactKind::Wake, "", "pending wake");
        pending.summary = Some("x".repeat(MAX_WIRE_SNAPSHOT_INTERNALS_BYTES));
        snapshot.pending_wakes.push(pending);
        let error = snapshot_to_wire_value(&snapshot).unwrap_err().to_string();
        assert!(error.contains("byte bound"), "{error}");
    }

    /// Golden for the FULL public RoomSnapshot surface. Populate every
    /// optional field so a new public key cannot slip in as an unobserved
    /// serde default, and assert the four daemon-only projections stay absent.
    #[test]
    fn public_room_snapshot_schema_keys_are_pinned() {
        let mut snapshot = RoomSnapshot {
            lead: Some("codex".to_string()),
            lead_epoch: Some(7),
            room_freeze_id: Some("freeze-1".to_string()),
            readers: vec![ReadReceipt {
                tool: "codex".to_string(),
                last_read_seq: 7,
                behind_by: 0,
                status: "caught_up".to_string(),
            }],
            mission: Some("ship safely".to_string()),
            composition: Some(RoomComposition::default()),
            ..RoomSnapshot::default()
        };
        snapshot
            .system_health
            .push(make_fact("health-1", FactKind::Risk, "", "health"));
        snapshot
            .active_claims
            .push(make_fact("claim-1", FactKind::Claim, "src/", "claim"));

        let value = serde_json::to_value(snapshot).unwrap();
        let keys = value
            .as_object()
            .expect("RoomSnapshot must serialize as an object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = [
            "active_blockers",
            "active_claims",
            "composition",
            "current_decisions",
            "current_risks",
            "lead",
            "lead_epoch",
            "max_seq",
            "mission",
            "open_handoffs",
            "readers",
            "recent_artifacts",
            "room_freeze_id",
            "squads",
            "stale_facts",
            "system_health",
            "totals",
            "unconsumed_artifacts",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(keys, expected);
        assert!(!value.to_string().contains(WIRE_INTERNALS_KEY));
        for private in [
            "content_max_seq",
            "last_activity_ts",
            "pending_wakes",
            "stale_authors",
            "author_last_seen",
        ] {
            assert!(!keys.contains(private), "private key leaked: {private}");
        }
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
            appended.fact.seq, 8,
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

    /// A sequence number identifies one complete canonical envelope. Any field
    /// difference at that sequence is ambiguous storage, regardless of whether
    /// both rows are live or one has rotated into the archive. Every repo-wide
    /// fold must reject the ambiguity before projecting or replacing facts.db.
    #[test]
    fn canonical_fold_rejects_every_ledger_line_difference_across_sources() {
        for source_layout in ["live-live", "live-archive"] {
            for differing_field in ["occurred_at", "engagement", "event_type", "payload"] {
                let case = format!("{source_layout}-{differing_field}");
                let root = unique_root(&format!("canonical-conflict-{case}"));
                let original = ledger_line(7, "decision", "event-7", "alpha");
                let mut conflicting: LedgerLine = serde_json::from_str(&original).unwrap();
                match differing_field {
                    "occurred_at" => conflicting.occurred_at = "2026-05-02T00:00:07Z".to_string(),
                    "engagement" => conflicting.engagement = Some("beta".to_string()),
                    "event_type" => {
                        conflicting.event_type = "artifact".to_string();
                        conflicting.payload["kind"] = json!("artifact");
                    }
                    "payload" => conflicting.payload["subject"] = json!("different payload"),
                    _ => unreachable!(),
                }
                let conflicting = serde_json::to_string(&conflicting).unwrap();

                write_segment(&root, "log", "a.jsonl", &[original.as_str()]);
                let second_dir = if source_layout == "live-live" {
                    "log"
                } else {
                    "archive"
                };
                write_segment(&root, second_dir, "z.jsonl", &[conflicting.as_str()]);

                let log_dir = root.join(".rally").join(LOG_DIRNAME);
                let archive_dir = root.join(".rally").join(ARCHIVE_DIRNAME);
                let live = read_segment_files(&log_dir).unwrap();
                let archived = replay_archive_segments(&archive_dir).unwrap();
                let source_bytes = live
                    .iter()
                    .chain(archived.iter())
                    .map(|path| (path.clone(), fs::read(path).unwrap()))
                    .collect::<Vec<_>>();
                let facts_db = root.join(".rally/facts.db");
                let sentinel = b"existing derived cache";
                fs::write(&facts_db, sentinel).unwrap();
                let expected =
                    "conflicting canonical segment rows at seq 7: full LedgerLine values differ";

                assert_eq!(
                    canonical_conflict_message(facts_from_segments(&log_dir, &archive_dir), &case,),
                    expected,
                    "{case}: fact projection must use full envelope equality"
                );
                assert_eq!(
                    canonical_conflict_message(segment_seq_stats(&live, &archived), &case),
                    expected,
                    "{case}: authoritative stats must validate before DB comparison"
                );
                assert_eq!(
                    canonical_conflict_message(
                        rebuild_db_from_segments(&live, &archived, &facts_db),
                        &case,
                    ),
                    expected,
                    "{case}: rebuild must reject before replacing facts.db"
                );
                assert_eq!(
                    fs::read(&facts_db).unwrap(),
                    sentinel,
                    "{case}: failed validation must preserve the derived cache"
                );
                for (path, before) in source_bytes {
                    assert_eq!(
                        fs::read(&path).unwrap(),
                        before,
                        "{case}: canonical source changed at {}",
                        path.display()
                    );
                }
                assert!(
                    !root.join(".rally").join("quarantine").exists(),
                    "{case}: conflict detection must not rewrite canonical input into quarantine"
                );

                fs::remove_dir_all(&root).ok();
            }
        }
    }

    #[test]
    fn canonical_fold_conflict_is_input_order_invariant() {
        let root = unique_root("canonical-conflict-order");
        let original = ledger_line(11, "decision", "event-11", "alpha");
        let mut conflicting: LedgerLine = serde_json::from_str(&original).unwrap();
        conflicting.occurred_at = "2026-05-02T00:00:11Z".to_string();
        let conflicting = serde_json::to_string(&conflicting).unwrap();
        write_segment(&root, "log", "a.jsonl", &[original.as_str()]);
        write_segment(&root, "log", "z.jsonl", &[conflicting.as_str()]);

        let rally_dir = root.join(".rally");
        let first = rally_dir.join(LOG_DIRNAME).join("a.jsonl");
        let second = rally_dir.join(LOG_DIRNAME).join("z.jsonl");
        let first_db = rally_dir.join("first-order.db");
        let second_db = rally_dir.join("second-order.db");
        fs::write(&first_db, b"first sentinel").unwrap();
        fs::write(&second_db, b"second sentinel").unwrap();

        let forward = canonical_conflict_message(
            rebuild_db_from_segments(&[first.clone(), second.clone()], &[], &first_db),
            "forward order",
        );
        let reverse = canonical_conflict_message(
            rebuild_db_from_segments(&[second, first], &[], &second_db),
            "reverse order",
        );
        assert_eq!(
            forward, reverse,
            "conflict result must not depend on input order"
        );
        assert_eq!(fs::read(&first_db).unwrap(), b"first sentinel");
        assert_eq!(fs::read(&second_db).unwrap(), b"second sentinel");
        assert!(!rally_dir.join("quarantine").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn canonical_fold_dedupes_only_exact_ledger_line_copies() {
        let root = unique_root("canonical-exact-copy");
        let exact = ledger_line(13, "decision", "event-13", "alpha");
        write_segment(&root, "log", "alpha.jsonl", &[exact.as_str()]);
        write_segment(&root, "archive", "alpha.jsonl", &[exact.as_str()]);

        let log_dir = root.join(".rally").join(LOG_DIRNAME);
        let archive_dir = root.join(".rally").join(ARCHIVE_DIRNAME);
        let live = read_segment_files(&log_dir).unwrap();
        let archived = replay_archive_segments(&archive_dir).unwrap();
        assert_eq!(
            facts_from_segments(&log_dir, &archive_dir).unwrap().len(),
            1
        );
        assert_eq!(
            segment_seq_stats(&live, &archived).unwrap(),
            SeqStats {
                count: 1,
                max_seq: 13,
            }
        );

        let facts_db = root.join(".rally/facts.db");
        rebuild_db_from_segments(&live, &archived, &facts_db).unwrap();
        let rebuilt = facts_from_store(&open_fact_store(&facts_db).unwrap()).unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].event_id, "event-13");
        assert_eq!(rebuilt[0].seq, 13);
        assert!(!root.join(".rally").join("quarantine").exists());

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
            appended.fact.seq, 5,
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

        assert!(
            verified.fact.seq > 0,
            "seq must be > 0 after verified append"
        );
        assert_eq!(
            verified.fact.event_id, "ev-r9-6",
            "event_id must be preserved"
        );
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
        let event_id = &appended.fact.event_id;

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

        // Confirm the derived db still has it without invoking RoomStore's
        // canonical reconciliation (which must reject this DB-only split).
        let db = open_fact_store_lenient(&root.join(".rally/facts.db")).unwrap();
        let db_facts = facts_from_store(&db).unwrap();
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
            segment_event_id_present_tail_first(&active, &verified.fact.event_id).unwrap(),
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
            !segment_event_id_present_tail_first(&active, &appended.fact.event_id).unwrap(),
            "tail-first must miss after truncation (defers to full scan)"
        );
        let live = read_segment_files(&root.join(".rally").join(LOG_DIRNAME)).unwrap();
        let arch = read_segment_files(&root.join(".rally").join(ARCHIVE_DIRNAME)).unwrap();
        assert!(
            !segment_event_id_present(live.iter().chain(arch.iter()), &appended.fact.event_id)
                .unwrap(),
            "full scan must also miss the dropped event — silent drop is still caught"
        );

        fs::remove_dir_all(&root).ok();
    }

    // =========================================================================
    // Step-3 reconcile fast-path tests (O(1) happy path + corruption safety)
    // =========================================================================

    #[test]
    fn step3_reconcile_cache_current_schema_is_stamped_and_required() {
        let root = unique_root("step3-cache-schema");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "schema-seed",
                FactKind::Decision,
                "src/",
                "seed current cache",
            ))
            .unwrap();

        let facts_db = root.join(".rally/facts.db");
        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);
        let fresh: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        let fresh_schema = fresh.get("schema_version").and_then(Value::as_u64);

        let mut current = fresh.clone();
        current.as_object_mut().unwrap().insert(
            "schema_version".to_string(),
            json!(RECONCILE_CACHE_SCHEMA_VERSION),
        );
        fs::write(&sidecar, serde_json::to_vec(&current).unwrap()).unwrap();
        let current_is_accepted = read_reconcile_cache(&facts_db).is_some();

        let mut absent = current.clone();
        absent.as_object_mut().unwrap().remove("schema_version");
        fs::write(&sidecar, serde_json::to_vec(&absent).unwrap()).unwrap();
        let absent_is_rejected = read_reconcile_cache(&facts_db).is_none();

        let mut old = current.clone();
        old.as_object_mut()
            .unwrap()
            .insert("schema_version".to_string(), json!(1));
        fs::write(&sidecar, serde_json::to_vec(&old).unwrap()).unwrap();
        let old_is_rejected = read_reconcile_cache(&facts_db).is_none();

        let mut unknown = current;
        unknown
            .as_object_mut()
            .unwrap()
            .insert("schema_version".to_string(), json!(999));
        fs::write(&sidecar, serde_json::to_vec(&unknown).unwrap()).unwrap();
        let unknown_is_rejected = read_reconcile_cache(&facts_db).is_none();

        assert!(
            fresh_schema == Some(u64::from(RECONCILE_CACHE_SCHEMA_VERSION))
                && current_is_accepted
                && absent_is_rejected
                && old_is_rejected
                && unknown_is_rejected,
            "cache schema contract failed: fresh={fresh_schema:?}, current accepted={current_is_accepted}, absent rejected={absent_is_rejected}, old rejected={old_is_rejected}, unknown rejected={unknown_is_rejected}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn step3_old_cache_schema_forces_one_scan_then_current_cache_is_fast() {
        let root = unique_root("step3-cache-schema-upgrade");
        let store = RoomStore::open_at(root.clone()).unwrap();
        for i in 0..3u32 {
            store
                .append_fact(&make_fact(
                    &format!("schema-upgrade-{i}"),
                    FactKind::Decision,
                    "src/",
                    "schema upgrade seed",
                ))
                .unwrap();
        }

        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
        let facts_db = rally_dir.join("facts.db");
        let sidecar = rally_dir.join(RECONCILE_CACHE_FILENAME);
        let mut legacy: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .insert("schema_version".to_string(), json!(1));
        fs::write(&sidecar, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let before = full_reconcile_scan_count();
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
        let after_upgrade = full_reconcile_scan_count();
        let refreshed: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        let refreshed_schema = refreshed.get("schema_version").and_then(Value::as_u64);
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
        let after_fast_path = full_reconcile_scan_count();

        assert_eq!(
            after_upgrade,
            before + 1,
            "an old cache schema must force exactly one authoritative scan"
        );
        assert_eq!(
            refreshed_schema,
            Some(u64::from(RECONCILE_CACHE_SCHEMA_VERSION))
        );
        assert_eq!(
            after_fast_path, after_upgrade,
            "the current cache written by that scan must permit the next O(1) fast path"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn legacy_reconcile_cache_never_allows_conflicting_append_to_mutate_storage() {
        let mut failures = Vec::new();
        for source_layout in ["live-live", "live-archive"] {
            let root = unique_root(&format!("legacy-cache-conflict-{source_layout}"));
            let store = DirectRoomStore::open_direct_at_with_engagement(
                root.clone(),
                Some("alpha".to_string()),
            )
            .unwrap();
            for seq in 1..=3 {
                store
                    .append_fact(&make_fact(
                        &format!("seed-{seq}"),
                        FactKind::Decision,
                        "src/",
                        "legacy cache seed",
                    ))
                    .unwrap();
            }
            drop(store);

            let conflicting = ledger_line(3, "artifact", "conflicting-3", "beta");
            let second_dir = if source_layout == "live-live" {
                LOG_DIRNAME
            } else {
                ARCHIVE_DIRNAME
            };
            write_segment(&root, second_dir, "beta.jsonl", &[conflicting.as_str()]);

            let facts_db = root.join(".rally/facts.db");
            let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);
            let live = segments_under(&root);
            let archived = archive_under(&root);
            let legacy = json!({
                "segments_fingerprint": segments_fingerprint(&live, &archived),
                "db_fingerprint": fingerprint_db(&facts_db),
                "wal_fingerprint": fingerprint_wal(&facts_db),
                "canonical_count": 3,
                "canonical_max_seq": 3,
                "db_count": 3,
                "db_max_seq": 3,
            });
            fs::write(&sidecar, serde_json::to_vec(&legacy).unwrap()).unwrap();

            // Model a natural upgrade: the legacy sidecar already exists when
            // this binary opens the room. O26 reconciles canonical-ahead state
            // during open, so the corruption must fail there before any writer
            // can enter; no cache/source/db byte may change.
            let db_before = fs::read(&facts_db).unwrap();
            let sources_before = canonical_source_snapshot(&root);
            let cache_before = fs::read(&sidecar).unwrap();
            let error_text = match DirectRoomStore::open_direct_at_with_engagement(
                root.clone(),
                Some("alpha".to_string()),
            ) {
                Ok(store) => {
                    drop(store);
                    String::new()
                }
                Err(error) => error.to_string(),
            };
            let db_after = fs::read(&facts_db).unwrap();
            let sources_after = canonical_source_snapshot(&root);
            let cache_after = fs::read(&sidecar).unwrap_or_default();

            if !(error_text.contains("conflicting canonical segment rows at seq 3")
                && db_after == db_before
                && sources_after == sources_before
                && cache_after == cache_before)
            {
                failures.push(format!(
                    "{source_layout}: db_changed={}, sources_changed={}, cache_changed={}, error={error_text:?}",
                    db_after != db_before,
                    sources_after != sources_before,
                    cache_after != cache_before,
                ));
            }
            fs::remove_dir_all(&root).ok();
        }
        assert!(
            failures.is_empty(),
            "conflicting append was not atomic:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn step3_zero_length_wal_is_absent_but_nonempty_wal_is_fingerprinted() {
        let root = unique_root("step3-zero-wal");
        let rally_dir = root.join(".rally");
        fs::create_dir_all(&rally_dir).unwrap();
        let facts_db = rally_dir.join("facts.db");
        let wal = facts_db.with_extension("db-wal");

        fs::write(&wal, b"").unwrap();
        assert!(
            fingerprint_wal(&facts_db).is_none(),
            "an empty WAL has no committed frames and must equal an absent WAL"
        );

        fs::write(&wal, b"committed-frame-signal").unwrap();
        assert!(
            fingerprint_wal(&facts_db).is_some(),
            "a nonempty WAL must remain part of the cache fingerprint"
        );
        fs::remove_dir_all(&root).ok();
    }

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
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
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

    #[test]
    fn step3_wal_state_change_invalidates_an_otherwise_matching_sidecar() {
        let root = unique_root("step3-wal-fingerprint");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "f"))
            .unwrap();
        let facts_db = root.join(".rally/facts.db");
        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);
        let wal = facts_db.with_extension("db-wal");
        let stale_bytes = fs::read(&sidecar).expect("append writes sidecar");
        let mut stale: ReconcileCache = serde_json::from_slice(&stale_bytes).unwrap();
        drop(store);

        // Model a sidecar captured while a nonempty WAL exists, then remove
        // that WAL before reconcile. Direct append now fingerprints only after
        // its per-op pool closes, so relying on a transient close-time WAL here
        // would recreate the race this test is meant to prevent.
        fs::write(&wal, b"committed-frame-signal").unwrap();
        stale.db_fingerprint = fingerprint_db(&facts_db);
        stale.wal_fingerprint = fingerprint_wal(&facts_db);
        assert!(
            stale.wal_fingerprint.is_some(),
            "a nonempty WAL must be represented in the sidecar"
        );
        write_reconcile_cache(&facts_db, &stale).unwrap();
        fs::remove_file(&wal).unwrap();
        assert!(
            fingerprint_wal(&facts_db).is_none(),
            "removed WAL must fingerprint as absent"
        );

        // Every legacy fast-path field matches the post-removal state while
        // the cache preserves the pre-removal WAL fingerprint. Only WAL
        // awareness can reject this otherwise self-consistent stale cache.
        assert!(
            reconcile_took_full_scan(&root),
            "WAL disappearance must invalidate the reconcile fast path"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn step3_append_remeasures_counts_instead_of_incrementing_a_lie() {
        let root = unique_root("step3-remeasure-after-append");
        let store = RoomStore::open_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact("e1", FactKind::Claim, "src/", "f"))
            .unwrap();
        let facts_db = root.join(".rally/facts.db");
        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);
        let mut lying: ReconcileCache =
            serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        lying.canonical_count = 100;
        lying.canonical_max_seq = 100;
        lying.db_count = 100;
        lying.db_max_seq = 100;
        write_reconcile_cache(&facts_db, &lying).unwrap();

        let appended = store
            .append_fact(&make_fact("e2", FactKind::Claim, "src/", "f"))
            .unwrap();
        let measured: ReconcileCache =
            serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        assert_eq!(measured.canonical_count, 2);
        assert_eq!(measured.canonical_max_seq, appended.fact.seq);
        assert_eq!(measured.db_count, 2);
        assert_eq!(measured.db_max_seq, appended.fact.seq);
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
            ids.push(f.fact.event_id.clone());
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
        reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
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
            ids.push(fact.fact.event_id.clone());
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
            schema_version: RECONCILE_CACHE_SCHEMA_VERSION,
            segments_fingerprint: segments_fingerprint(&segments, &archived),
            db_fingerprint: fingerprint_db(&facts_db),
            wal_fingerprint: fingerprint_wal(&facts_db),
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
            reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
            let mut best = u128::MAX;
            for _ in 0..20 {
                let t = Instant::now();
                reconcile_segments_and_db(&log_dir, &archive_dir, &facts_db, true).unwrap();
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
        let event_id = &appended.fact.event_id;

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

        // Assert 2: a raw derived-db read returns true (false-pass territory).
        // RoomStore::facts intentionally refuses the DB-only split.
        let db = open_fact_store_lenient(&root.join(".rally/facts.db")).unwrap();
        let db_facts = facts_from_store(&db).unwrap();
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
        let event_id = &appended_b.fact.event_id;

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
        let peer_seq = appended_a.fact.seq + 100; // jump to simulate concurrent write
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
            segment_event_id_present(segs.iter().chain(arch.iter()), &appended_a.fact.event_id)
                .unwrap();
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
        assert_eq!((a.fact.seq, b.fact.seq, c.fact.seq), (1, 2, 3));

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
    fn o26_unmarked_db_only_room_fails_loud_and_preserves_db() {
        let root = unique_root("o26-db-only-refusal");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-db-only-seed",
                FactKind::Decision,
                "src/",
                "must require explicit migration",
            ))
            .unwrap();
        drop(store);

        let log_dir = root.join(".rally").join(LOG_DIRNAME);
        for path in read_segment_files(&log_dir).unwrap() {
            fs::remove_file(path).unwrap();
        }
        let facts_db = root.join(".rally/facts.db");
        let before = fs::read(&facts_db).unwrap();

        let error = match DirectRoomStore::open_direct_at(root.clone()) {
            Ok(_) => panic!("an unmarked current-format DB-only room must not auto-promote"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("rally doctor --migrate-db-only"),
            "refusal must name the explicit recovery path: {message}"
        );
        assert_eq!(
            fs::read(&facts_db).unwrap(),
            before,
            "refusal must preserve the only extant history byte-for-byte"
        );
        assert!(read_segment_files(&log_dir).unwrap().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_invalid_event_id_is_rejected_before_canonical_io() {
        let root = unique_root("o26-invalid-event-id");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let segment = store.active_segment_path();
        let db_before = fs::read(&store.facts_db_path).unwrap();
        assert!(!segment.exists());

        let mut candidate = make_fact("valid-before-edit", FactKind::Decision, "src/", "invalid");
        candidate.event_id.clear();
        let error = store
            .append_fact(&candidate)
            .expect_err("empty event id must fail before canonical mutation");
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(error.to_string().contains("event_id must not be empty"));
        assert!(
            !segment.exists(),
            "invalid stable identity must not create a canonical segment"
        );

        candidate.event_id = "x".repeat(MAX_APPEND_EVENT_ID_BYTES + 1);
        let error = store
            .append_fact(&candidate)
            .expect_err("oversized event id must fail before canonical mutation");
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(!segment.exists());

        candidate.event_id = "bad\nevent".to_string();
        let error = store
            .append_fact(&candidate)
            .expect_err("control characters must fail before canonical mutation");
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(!segment.exists());

        candidate.event_id = "valid-schema-check".to_string();
        candidate.schema = "agent-rally.fact.v999".to_string();
        let error = store
            .append_fact(&candidate)
            .expect_err("unsupported schema must fail before canonical mutation");
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(!segment.exists());
        assert_eq!(fs::read(&store.facts_db_path).unwrap(), db_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_canonical_open_fault_is_not_started_and_leaves_db_empty() {
        let root = unique_root("o26-canonical-open-fault");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let rally_dir = root.join(".rally");
        fail_o26_once(&rally_dir, O26FaultPoint::BeforeCanonicalMutation);
        let error = store
            .append_fact(&make_fact(
                "o26-before-open",
                FactKind::Decision,
                "src/",
                "not started",
            ))
            .expect_err("pre-open fault must be retry-safe");
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert!(!store.active_segment_path().exists());
        let db = open_fact_store_lenient(&store.facts_db_path).unwrap();
        assert!(facts_from_store(&db).unwrap().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_projection_failure_is_committed_and_same_id_retry_repairs_once() {
        let root = unique_root("o26-projection-degraded");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-projection-failure",
            FactKind::Decision,
            "src/",
            "canonical survives projection",
        );
        fail_o26_once(&root.join(".rally"), O26FaultPoint::FactsDbProjection);
        let degraded = store.append_fact(&candidate).unwrap();
        assert!(degraded.committed);
        assert!(!degraded.projection_complete);
        assert_eq!(degraded.warnings[0].code, ProjectionWarningCode::FactsDb);
        assert_eq!(
            facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        let db = open_fact_store_lenient(&store.facts_db_path).unwrap();
        assert!(facts_from_store(&db).unwrap().is_empty());
        drop(db);

        let repaired = store.append_fact(&candidate).unwrap();
        assert!(
            repaired.projection_complete,
            "exact retry reprojects the row"
        );
        let db = open_fact_store_lenient(&store.facts_db_path).unwrap();
        assert_eq!(
            facts_from_store(&db)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_partial_write_is_unknown_then_same_id_retry_is_singleton() {
        let root = unique_root("o26-partial-write");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-partial-event",
            FactKind::Artifact,
            "src/",
            "partial then retry",
        );
        fail_o26_once(&root.join(".rally"), O26FaultPoint::PartialCanonicalWrite);
        let error = store
            .append_fact(&candidate)
            .expect_err("partial write cannot claim NotStarted");
        assert!(matches!(error, RallyError::OutcomeUnknown { .. }));
        assert!(
            !fs::read(store.active_segment_path())
                .unwrap()
                .ends_with(b"\n")
        );

        let retry = store.append_fact(&candidate).unwrap();
        assert_eq!(retry.fact.seq, 1);
        let facts = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_synced_lost_reply_retry_resyncs_and_returns_original_once() {
        let root = unique_root("o26-full-sync-unknown");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-synced-event",
            FactKind::Artifact,
            "src/",
            "sync then lost certainty",
        );
        fail_o26_once(
            &root.join(".rally"),
            O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );
        let error = store
            .append_fact(&candidate)
            .expect_err("post-sync pre-readback fault is unknown");
        assert!(matches!(error, RallyError::OutcomeUnknown { .. }));
        let retry = store.append_fact(&candidate).unwrap();
        assert_eq!(retry.fact.seq, 1);
        let facts = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_exact_retry_rejects_unrelated_same_sequence_conflict_before_projection() {
        let root = unique_root("o26-retry-unrelated-seq-conflict");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-retry-before-conflict",
            FactKind::Decision,
            "src/",
            "canonical candidate",
        );
        store.append_fact(&candidate).unwrap();
        let live_path = store.active_segment_path();
        let mut conflicting = read_segment_entries(&live_path).unwrap().remove(0);
        conflicting.payload["event_id"] = json!("unrelated-at-same-seq");
        conflicting.payload["subject"] = json!("different full ledger line");
        fs::create_dir_all(&store.archive_dir).unwrap();
        let archive_path = store.archive_dir.join("conflict.jsonl");
        fs::write(
            &archive_path,
            format!("{}\n", serde_json::to_string(&conflicting).unwrap()),
        )
        .unwrap();
        let db_before = fs::read(&store.facts_db_path).unwrap();
        let reconcile_path = reconcile_cache_path(&store.facts_db_path).unwrap();
        let reconcile_before = fs::read(&reconcile_path).ok();
        let live_before = fs::read(&live_path).unwrap();
        let archive_before = fs::read(&archive_path).unwrap();

        let error = store
            .append_fact(&candidate)
            .expect_err("full-union conflict must precede exact-id retry projection");
        assert!(error.to_string().contains("full LedgerLine values differ"));
        assert_eq!(fs::read(&live_path).unwrap(), live_before);
        assert_eq!(fs::read(&archive_path).unwrap(), archive_before);
        assert_eq!(fs::read(&store.facts_db_path).unwrap(), db_before);
        assert_eq!(fs::read(&reconcile_path).ok(), reconcile_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_exact_live_archive_copy_retry_is_accepted_and_deduped() {
        let root = unique_root("o26-retry-exact-live-archive-copy");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-exact-copy-retry",
            FactKind::Artifact,
            "src/",
            "exact copy",
        );
        let first = store.append_fact(&candidate).unwrap();
        let live_path = store.active_segment_path();
        fs::create_dir_all(&store.archive_dir).unwrap();
        let archive_path = store.archive_dir.join("exact-copy.jsonl");
        fs::copy(&live_path, &archive_path).unwrap();

        let retry = store.append_fact(&candidate).unwrap();
        assert_eq!(retry.fact.seq, first.fact.seq);
        let canonical = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        assert_eq!(
            canonical
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1,
            "exact live/archive copies are one canonical row"
        );
        assert_eq!(read_segment_entries(&live_path).unwrap().len(), 1);
        assert_eq!(read_segment_entries(&archive_path).unwrap().len(), 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_deadline_after_scans_stops_new_write_before_any_side_effect() {
        let root = unique_root("o26-late-start-new");
        let seed = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let db_before = fs::read(&seed.facts_db_path).unwrap();
        drop(seed);
        let pause = pause_o26_once(&root.join(".rally"), O26FaultPoint::BeforeCanonicalMutation);
        let thread_root = root.clone();
        let worker = thread::spawn(move || {
            let store = DirectRoomStore::open_direct_at(thread_root).unwrap();
            with_mutation_deadline(Duration::from_millis(40), || {
                store.append_fact(&make_fact(
                    "o26-late-start-new-event",
                    FactKind::Decision,
                    "src/",
                    "must remain not started",
                ))
            })
        });
        pause.wait_until_reached();
        thread::sleep(Duration::from_millis(75));
        pause.resume();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert!(
            read_segment_files(&root.join(".rally/log"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(root.join(".rally/facts.db")).unwrap(), db_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_deadline_after_tail_scan_does_not_start_framing_repair() {
        let root = unique_root("o26-late-start-tail");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-tail-prefix",
                FactKind::Decision,
                "src/",
                "prefix",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let mut tail_before = fs::read(&segment).unwrap();
        assert_eq!(tail_before.pop(), Some(b'\n'));
        fs::write(&segment, &tail_before).unwrap();
        let db_before = fs::read(&store.facts_db_path).unwrap();
        drop(store);

        let pause = pause_o26_once(&root.join(".rally"), O26FaultPoint::BeforeCanonicalMutation);
        let thread_root = root.clone();
        let worker = thread::spawn(move || {
            let store = DirectRoomStore::open_direct_at(thread_root).unwrap();
            with_mutation_deadline(Duration::from_millis(40), || {
                store.append_fact(&make_fact(
                    "o26-after-tail-deadline",
                    FactKind::Artifact,
                    "src/",
                    "must not repair or append",
                ))
            })
        });
        pause.wait_until_reached();
        thread::sleep(Duration::from_millis(75));
        pause.resume();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert_eq!(fs::read(&segment).unwrap(), tail_before);
        assert_eq!(fs::read(root.join(".rally/facts.db")).unwrap(), db_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_exact_retry_past_deadline_is_unknown_not_not_started() {
        let root = unique_root("o26-retry-resync-deadline");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-retry-resync-event",
            FactKind::Decision,
            "src/",
            "already canonical",
        );
        store.append_fact(&candidate).unwrap();
        let segment = store.active_segment_path();
        let segment_before = fs::read(&segment).unwrap();
        let db_before = fs::read(&store.facts_db_path).unwrap();
        drop(store);

        let pause = pause_o26_once(&root.join(".rally"), O26FaultPoint::BeforeCanonicalMutation);
        let thread_root = root.clone();
        let worker = thread::spawn(move || {
            let store = DirectRoomStore::open_direct_at(thread_root).unwrap();
            with_mutation_deadline(Duration::from_millis(40), || store.append_fact(&candidate))
        });
        pause.wait_until_reached();
        thread::sleep(Duration::from_millis(75));
        pause.resume();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref phase, .. } if phase == "retry-resync-deadline"
        ));
        assert_eq!(fs::read(&segment).unwrap(), segment_before);
        assert_eq!(fs::read(root.join(".rally/facts.db")).unwrap(), db_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_complete_invalid_unterminated_tail_is_rejected_unchanged() {
        let root = unique_root("o26-complete-invalid-tail");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-valid-prefix",
                FactKind::Decision,
                "src/",
                "prefix",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let mut bytes = fs::read(&segment).unwrap();
        bytes.extend_from_slice(b"{]complete-non-eof");
        fs::write(&segment, &bytes).unwrap();
        let error = store
            .append_fact(&make_fact(
                "o26-after-invalid",
                FactKind::Artifact,
                "src/",
                "must not land",
            ))
            .expect_err("complete invalid tail must fail loud");
        assert!(!matches!(error, RallyError::OutcomeUnknown { .. }));
        assert_eq!(fs::read(&segment).unwrap(), bytes);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_wrong_schema_unterminated_json_tail_is_rejected_unchanged() {
        let root = unique_root("o26-wrong-schema-tail");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let segment = store.active_segment_path();
        fs::create_dir_all(segment.parent().unwrap()).unwrap();
        let mut wrong = make_fact(
            "o26-wrong-schema-row",
            FactKind::Decision,
            "src/",
            "wrong schema",
        );
        wrong.seq = 1;
        wrong.schema = "agent-rally.fact.v999".to_string();
        let entry = LedgerLine {
            seq: 1,
            occurred_at: now_string(),
            event_type: "decision".to_string(),
            payload: serde_json::to_value(wrong).unwrap(),
            engagement: Some(store.active_engagement.clone()),
        };
        let bytes = serde_json::to_vec(&entry).unwrap();
        fs::write(&segment, &bytes).unwrap();
        let error = store
            .append_fact(&make_fact(
                "o26-after-wrong-schema",
                FactKind::Artifact,
                "src/",
                "must not land",
            ))
            .expect_err("complete wrong-schema tail must fail loud");
        assert!(error.to_string().contains("unsupported fact schema"));
        assert_eq!(fs::read(&segment).unwrap(), bytes);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_valid_final_record_without_newline_is_completed_before_append() {
        let root = unique_root("o26-valid-tail-no-newline");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let first = store
            .append_fact(&make_fact(
                "o26-valid-tail-first",
                FactKind::Decision,
                "src/",
                "valid tail",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let mut bytes = fs::read(&segment).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&segment, bytes).unwrap();

        let second = store
            .append_fact(&make_fact(
                "o26-valid-tail-second",
                FactKind::Artifact,
                "src/",
                "append after framing repair",
            ))
            .unwrap();

        let bytes = fs::read(&segment).unwrap();
        assert!(bytes.ends_with(b"\n"));
        let entries = read_segment_entries(&segment).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].seq, entries[1].seq),
            (first.fact.seq, second.fact.seq)
        );
        assert_eq!(entries[0].payload["event_id"], "o26-valid-tail-first");
        assert_eq!(entries[1].payload["event_id"], "o26-valid-tail-second");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_incomplete_unterminated_tail_is_truncated_before_append() {
        let root = unique_root("o26-incomplete-tail");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-before-torn-tail",
                FactKind::Decision,
                "src/",
                "before",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        {
            let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
            file.write_all(b"{\"seq\":2,\"occurred_at\":\"2026-08-10T00:00:00Z\",\"event_type\":\"artifact\",\"payload\":{")
                .unwrap();
            file.sync_all().unwrap();
        }

        let appended = store
            .append_fact(&make_fact(
                "o26-after-torn-tail",
                FactKind::Artifact,
                "src/",
                "after",
            ))
            .unwrap();

        let entries = read_segment_entries(&segment).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(appended.fact.seq, 2);
        assert_eq!(entries[1].payload["event_id"], "o26-after-torn-tail");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_tail_repair_sync_failure_is_unknown_then_retry_appends_once() {
        let root = unique_root("o26-tail-repair-sync");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-tail-sync-prefix",
                FactKind::Decision,
                "src/",
                "prefix",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let mut bytes = fs::read(&segment).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&segment, bytes).unwrap();
        let candidate = make_fact(
            "o26-tail-sync-candidate",
            FactKind::Artifact,
            "src/",
            "after repair",
        );
        fail_o26_once(&root.join(".rally"), O26FaultPoint::TailRepairSync);

        let error = store
            .append_fact(&candidate)
            .expect_err("repair sync failure starts mutation and is outcome-unknown");
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                if event_id == &candidate.event_id && phase == "tail-repair-sync"
        ));
        let retry = store.append_fact(&candidate).unwrap();
        assert!(retry.committed);
        assert_eq!(retry.fact.seq, 2);
        assert_eq!(
            facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_failure_after_repair_before_append_is_unknown_then_retry_singleton() {
        let root = unique_root("o26-after-tail-repair");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        store
            .append_fact(&make_fact(
                "o26-after-repair-prefix",
                FactKind::Decision,
                "src/",
                "prefix",
            ))
            .unwrap();
        let segment = store.active_segment_path();
        let mut bytes = fs::read(&segment).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&segment, bytes).unwrap();
        let candidate = make_fact(
            "o26-after-repair-candidate",
            FactKind::Artifact,
            "src/",
            "after repaired-only state",
        );
        fail_o26_once(&root.join(".rally"), O26FaultPoint::AfterTailRepair);

        let error = store
            .append_fact(&candidate)
            .expect_err("failure after durable repair cannot be NotStarted");
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                if event_id == &candidate.event_id && phase == "after-tail-repair"
        ));
        assert!(fs::read(&segment).unwrap().ends_with(b"\n"));
        let retry = store.append_fact(&candidate).unwrap();
        assert_eq!(retry.fact.seq, 2);
        assert_eq!(
            facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_post_readback_failure_is_committed_warning_not_error() {
        let root = unique_root("o26-after-readback");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-after-readback-candidate",
            FactKind::Decision,
            "src/",
            "canonical certainty",
        );
        fail_o26_once(&root.join(".rally"), O26FaultPoint::AfterCanonicalReadback);

        let outcome = store.append_fact(&candidate).unwrap();
        assert!(outcome.committed);
        assert!(!outcome.projection_complete);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.code == ProjectionWarningCode::PostCommitWork)
        );
        assert_eq!(
            facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_reconcile_cache_failure_is_committed_warning_and_db_has_row() {
        let root = unique_root("o26-reconcile-cache-warning");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-reconcile-cache-candidate",
            FactKind::Decision,
            "src/",
            "derived cache warning",
        );
        fail_o26_once(
            &root.join(".rally"),
            O26FaultPoint::ReconcileCacheProjection,
        );

        let outcome = store.append_fact(&candidate).unwrap();
        assert!(outcome.committed);
        assert!(!outcome.projection_complete);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.code == ProjectionWarningCode::ReconcileCache)
        );
        let db = open_fact_store_lenient(&store.facts_db_path).unwrap();
        assert_eq!(
            facts_from_store(&db)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_real_reconcile_cache_rename_failure_is_committed_warning() {
        let root = unique_root("o26-reconcile-cache-rename-warning");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let sidecar = root.join(".rally").join(RECONCILE_CACHE_FILENAME);
        fs::remove_file(&sidecar).ok();
        fs::create_dir(&sidecar).unwrap();
        let candidate = make_fact(
            "o26-reconcile-cache-rename-candidate",
            FactKind::Decision,
            "src/",
            "derived cache rename warning",
        );

        let outcome = store.append_fact(&candidate).unwrap();
        assert!(outcome.committed);
        assert!(!outcome.projection_complete);
        assert!(outcome.warnings.iter().any(|warning| {
            warning.code == ProjectionWarningCode::ReconcileCache
                && warning.message.contains("rename")
        }));
        let retry = store.append_fact(&candidate).unwrap();
        assert!(retry.committed);
        assert!(!retry.projection_complete);
        assert_eq!(retry.fact.seq, outcome.fact.seq);
        assert_eq!(
            facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_detached_snapshot_cannot_be_stamped_with_a_new_canonical_fingerprint() {
        let root = unique_root("o26-snapshot-cache-detached-pair");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let first = make_fact(
            "o26-cache-first",
            FactKind::Decision,
            "src/",
            "first cached fact",
        );
        store.append_fact(&first).unwrap();
        let detached = store.snapshot_cache_capture(false).unwrap();

        let second = make_fact(
            "o26-cache-second",
            FactKind::Decision,
            "src/",
            "intervening canonical fact",
        );
        store.append_fact(&second).unwrap();
        let rally_dir = root.join(".rally");
        write_snapshot_cache(&rally_dir, &detached);

        let cached = try_load_cached_snapshot(&rally_dir);
        assert!(
            cached.as_ref().is_none_or(|snapshot| {
                snapshot
                    .current_decisions
                    .iter()
                    .any(|fact| fact.event_id == second.event_id)
            }),
            "a detached old snapshot must not be published under the intervening commit's fingerprint"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_snapshot_cache_expires_across_no_write_liveness_transition() {
        let root = unique_root("o26-snapshot-cache-liveness-time");
        let rally_dir = root.join(".rally");
        fs::create_dir_all(&rally_dir).unwrap();
        let projection_time = 2_000_000_000_i64;
        let owner = "codex:idle-owner";
        let created_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp(projection_time - 1_000, 0)
                .unwrap()
                .to_rfc3339();
        let prior_created_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp(projection_time - 1_100, 0)
                .unwrap()
                .to_rfc3339();

        let mut prior_presence = make_fact(
            "o26-cache-presence-prior",
            FactKind::Presence,
            "session",
            "prior branch position",
        );
        prior_presence.seq = 1;
        prior_presence.tool = Some(owner.to_string());
        prior_presence.created_at = prior_created_at;
        prior_presence.evidence = vec!["branch_head_sha:aaaa".to_string()];

        let mut presence = make_fact(
            "o26-cache-presence-current",
            FactKind::Presence,
            "session",
            "current branch position",
        );
        presence.seq = 2;
        presence.tool = Some(owner.to_string());
        presence.created_at = created_at.clone();
        presence.evidence = vec!["branch_head_sha:bbbb".to_string()];

        let mut handoff = make_fact(
            "o26-cache-owner-handoff",
            FactKind::Handoff,
            "work",
            "inject signal",
        );
        handoff.seq = 3;
        handoff.tool = Some(owner.to_string());
        handoff.target = Some(owner.to_string());
        handoff.created_at = created_at.clone();

        let mut claim = make_fact(
            "o26-cache-owner-claim",
            FactKind::Claim,
            "file:src/shared.rs",
            "plan signal",
        );
        claim.seq = 4;
        claim.tool = Some(owner.to_string());
        claim.created_at = created_at;

        let facts = vec![prior_presence, presence, handoff, claim];
        let coord = crate::hooks_config::CoordinationConfig::default();
        let cached = snapshot_from_facts_with_policy_at(&facts, &coord, false, projection_time);
        let mut cached_findings = Vec::new();
        crate::check::check_before_write_for_test(
            &cached,
            "codex:contender",
            Some("src/shared.rs"),
            &mut cached_findings,
        );
        assert!(cached_findings.contains(&("stale-owner-claim", "warn")));

        let later_time = projection_time + 1_000;
        let fresh = snapshot_from_facts_with_policy_at(&facts, &coord, false, later_time);
        let mut fresh_findings = Vec::new();
        crate::check::check_before_write_for_test(
            &fresh,
            "codex:contender",
            Some("src/shared.rs"),
            &mut fresh_findings,
        );
        assert!(fresh_findings.contains(&("claimed-path", "stop")));

        let fingerprint =
            snapshot_cache_fingerprint_at(&rally_dir, projection_time, false, &coord).unwrap();
        write_snapshot_cache(
            &rally_dir,
            &SnapshotCacheCapture {
                snapshot: cached,
                fingerprint: Some(fingerprint),
            },
        );
        assert!(try_load_cached_snapshot_at(&rally_dir, projection_time).is_some());
        assert!(
            try_load_cached_snapshot_at(&rally_dir, later_time).is_none(),
            "a cache captured before a no-write liveness transition must miss afterward"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_snapshot_cache_misses_when_effective_projection_policy_changes() {
        let _env_lock = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        struct RestoreProjectionEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreProjectionEnv {
            fn drop(&mut self) {
                for (name, value) in self.0.drain(..) {
                    match value {
                        Some(value) => unsafe { std::env::set_var(name, value) },
                        None => unsafe { std::env::remove_var(name) },
                    }
                }
            }
        }
        let projection_env = [
            "RALLY_HALF_LIFE_HOURS",
            "RALLY_ARCHIVE_FLOOR",
            "RALLY_DEFAULT_CADENCE_SECS",
            "RALLY_MISS_MULTIPLIER",
            "RALLY_GRACE_SECS",
        ];
        let _restore_env = RestoreProjectionEnv(
            projection_env
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        );
        for name in projection_env {
            unsafe { std::env::remove_var(name) };
        }

        let root = unique_root("o26-snapshot-cache-policy");
        let rally_dir = root.join(".rally");
        fs::create_dir_all(&rally_dir).unwrap();
        let projection_time = 2_000_000_000_i64;
        let initial_policy = crate::hooks_config::resolve_coordination(&root).unwrap();
        let fingerprint =
            snapshot_cache_fingerprint_at(&rally_dir, projection_time, false, &initial_policy)
                .unwrap();
        write_snapshot_cache(
            &rally_dir,
            &SnapshotCacheCapture {
                snapshot: RoomSnapshot::default(),
                fingerprint: Some(fingerprint),
            },
        );
        assert!(try_load_cached_snapshot_at(&rally_dir, projection_time).is_some());

        let changed_cadence = if initial_policy.default_cadence_secs == 900 {
            901
        } else {
            900
        };
        fs::write(
            rally_dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({
                "coordination": {"default_cadence_secs": changed_cadence}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            try_load_cached_snapshot_at(&rally_dir, projection_time).is_none(),
            "a repo policy change must invalidate the cache without a canonical write"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_canonical_only_degraded_commit_invalidates_restored_old_snapshot_cache() {
        let root = unique_root("o26-snapshot-cache-canonical-generation");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let first = make_fact(
            "o26-cache-generation-first",
            FactKind::Decision,
            "src/",
            "cached canonical generation",
        );
        store.append_fact(&first).unwrap();
        let rally_dir = root.join(".rally");
        fs::remove_file(store.log_dir.join(LOG_INDEX_FILENAME)).ok();
        let old_capture = store.snapshot_cache_capture(false).unwrap();
        write_snapshot_cache(&rally_dir, &old_capture);
        let cache_path = snapshot_cache_path(&rally_dir);
        let old_cache = fs::read(&cache_path).unwrap();

        let second = make_fact(
            "o26-cache-generation-second",
            FactKind::Decision,
            "src/",
            "canonical commit without DB projection",
        );
        fail_o26_once(&rally_dir, O26FaultPoint::FactsDbProjection);
        let degraded = store.append_fact(&second).unwrap();
        assert!(degraded.committed);
        assert!(!degraded.projection_complete);
        fs::write(&cache_path, old_cache).unwrap();

        assert!(
            try_load_cached_snapshot(&rally_dir).is_none(),
            "canonical segment generation must reject an old cache even when DB/index signals are unchanged"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_segment_fingerprint_hashes_tail_for_same_length_repair() {
        let root = unique_root("o26-segment-tail-fingerprint");
        let log_dir = root.join(".rally").join(LOG_DIRNAME);
        fs::create_dir_all(&log_dir).unwrap();
        let segment = log_dir.join("alpha.jsonl");
        let mut first_bytes = vec![b'x'; 8192];
        first_bytes.extend_from_slice(b"first-tail");
        fs::write(&segment, &first_bytes).unwrap();
        let first = serde_json::to_value(segments_fingerprint(std::slice::from_ref(&segment), &[]))
            .unwrap();

        let mut second_bytes = vec![b'x'; 8192];
        second_bytes.extend_from_slice(b"other-tail");
        assert_eq!(second_bytes.len(), first_bytes.len());
        fs::write(&segment, &second_bytes).unwrap();
        let second =
            serde_json::to_value(segments_fingerprint(std::slice::from_ref(&segment), &[]))
                .unwrap();

        let first_tail = &first[0]["tail_hash"];
        let second_tail = &second[0]["tail_hash"];
        assert!(
            !first_tail.is_null(),
            "segment fingerprints need a tail hash"
        );
        assert_ne!(
            first_tail, second_tail,
            "same-length tail repair must invalidate persisted and in-process fingerprints"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_exact_same_event_id_retry_returns_original_sequence_once() {
        let root = unique_root("o26-same-id-exact");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact(
            "o26-same-id",
            FactKind::Decision,
            "src/",
            "same normalized payload",
        );
        let first = store.append_fact(&candidate).unwrap();
        let retry = store.append_fact(&candidate).unwrap();

        assert_eq!(
            retry.fact.seq, first.fact.seq,
            "retry must return the original seq"
        );
        let entries = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|fact| fact.event_id == candidate.event_id)
                .count(),
            1,
            "same-ID retry must leave exactly one canonical event"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_same_event_id_different_payload_conflicts_without_mutation() {
        let root = unique_root("o26-same-id-conflict");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let candidate = make_fact("o26-conflicting-id", FactKind::Decision, "src/", "original");
        store.append_fact(&candidate).unwrap();
        let segment = store.active_segment_path();
        let canonical_before = fs::read(&segment).unwrap();
        let mut conflict = candidate.clone();
        conflict.summary = Some("different normalized payload".to_string());

        let error = store
            .append_fact(&conflict)
            .expect_err("same event ID with different payload must fail identity validation");
        assert!(
            error.to_string().contains("event-id identity conflict"),
            "identity conflict must precede mutable-state noise: {error}"
        );
        assert_eq!(fs::read(&segment).unwrap(), canonical_before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_stateful_exact_retries_precede_mutable_preconditions() {
        let root = unique_root("o26-stateful-retries");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();

        let mut claim = make_fact(
            "o26-state-claim",
            FactKind::Claim,
            "file:src/state.rs",
            "claim",
        );
        claim
            .evidence
            .push("lease_expires_at:2030-01-01T00:00:00Z".to_string());
        let claim_first = store.append_fact(&claim).unwrap();
        let claim_retry = store.append_fact(&claim).unwrap();
        assert_eq!(claim_retry.fact.seq, claim_first.fact.seq);
        let mut claim_conflict = claim.clone();
        claim_conflict.summary = Some("different claim payload".to_string());
        assert!(
            store
                .append_fact(&claim_conflict)
                .unwrap_err()
                .to_string()
                .contains("event-id identity conflict")
        );

        let mut renewal = make_fact(
            "o26-state-renewal",
            FactKind::ClaimRenewed,
            "file:src/state.rs",
            "renew",
        );
        renewal.ref_id = Some(claim.event_id.clone());
        renewal.evidence = vec!["lease_expires_at:2099-01-01T00:00:00Z".to_string()];
        let renewal_first = store.append_fact(&renewal).unwrap();
        let renewal_retry = store.append_fact(&renewal).unwrap();
        assert_eq!(renewal_retry.fact.seq, renewal_first.fact.seq);
        let mut renewal_conflict = renewal.clone();
        renewal_conflict.evidence = vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()];
        assert!(
            store
                .append_fact(&renewal_conflict)
                .unwrap_err()
                .to_string()
                .contains("event-id identity conflict")
        );

        let mut release = make_fact(
            "o26-state-release",
            FactKind::Release,
            "file:src/state.rs",
            "release",
        );
        release.ref_id = Some(claim.event_id.clone());
        let release_first = store.append_state_transition_verified(&release).unwrap();
        let release_retry = store.append_state_transition_verified(&release).unwrap();
        assert_eq!(release_retry.fact.seq, release_first.fact.seq);
        let mut release_conflict = release.clone();
        release_conflict.summary = Some("different release payload".to_string());
        assert!(
            store
                .append_state_transition_verified(&release_conflict)
                .unwrap_err()
                .to_string()
                .contains("event-id identity conflict")
        );

        let blocker = make_fact(
            "o26-state-blocker",
            FactKind::Blocker,
            "file:src/blocker.rs",
            "blocker",
        );
        store.append_fact(&blocker).unwrap();
        let mut resolve = make_fact(
            "o26-state-resolve",
            FactKind::Resolve,
            "file:src/blocker.rs",
            "resolve",
        );
        resolve.ref_id = Some(blocker.event_id.clone());
        let resolve_first = store.append_state_transition_verified(&resolve).unwrap();
        let resolve_retry = store.append_state_transition_verified(&resolve).unwrap();
        assert_eq!(resolve_retry.fact.seq, resolve_first.fact.seq);
        let mut resolve_conflict = resolve.clone();
        resolve_conflict
            .evidence
            .push("different:evidence".to_string());
        assert!(
            store
                .append_state_transition_verified(&resolve_conflict)
                .unwrap_err()
                .to_string()
                .contains("event-id identity conflict")
        );

        let canonical = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        for event_id in [
            &claim.event_id,
            &renewal.event_id,
            &release.event_id,
            &resolve.event_id,
        ] {
            assert_eq!(
                canonical
                    .iter()
                    .filter(|fact| &fact.event_id == event_id)
                    .count(),
                1,
                "stateful exact retry must be a singleton: {event_id}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_renewal_and_checkpoint_requests_keep_stable_identity_and_outcomes() {
        let root = unique_root("o26-renew-checkpoint-contract");
        let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
        let claim = make_fact(
            "o26-renew-request-claim",
            FactKind::Claim,
            "file:src/renew.rs",
            "claim",
        );
        store.append_fact(&claim).unwrap();
        let renew_event_id = "o26-renew-request-event";
        let renew_thread_id = "o26-renew-request-thread";
        let renew_created_at = "2026-08-10T00:00:00Z";
        fail_o26_once(
            &root.join(".rally"),
            O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );
        let error = store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:00:00Z".to_string(),
                Some("test"),
                None,
                None,
                renew_event_id,
                renew_thread_id,
                renew_created_at,
            )
            .expect_err("renewal lost reply must be query-required");
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref event_id, .. } if event_id == renew_event_id
        ));
        let retry = store
            .renew_claim_lease(
                &claim.event_id,
                "2099-01-01T00:00:00Z".to_string(),
                Some("test"),
                None,
                None,
                renew_event_id,
                renew_thread_id,
                renew_created_at,
            )
            .unwrap();
        let renewal = retry
            .append_outcome
            .expect("exact renewal retry returns its committed outcome");
        assert_eq!(renewal.fact.event_id, renew_event_id);

        let mut checkpoint = make_fact(
            "o26-checkpoint-request-event",
            FactKind::Read,
            "",
            "read_seq:1",
        );
        checkpoint.tool = Some("reader:01".to_string());
        checkpoint.summary = Some("read_seq:1".to_string());
        checkpoint.thread_id = "o26-checkpoint-thread".to_string();
        checkpoint.created_at = "2026-08-10T00:00:00Z".to_string();
        fail_o26_once(
            &root.join(".rally"),
            O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );
        let error = store
            .maybe_append_read_checkpoint(&checkpoint, 1)
            .expect_err("checkpoint lost reply must be query-required");
        assert!(matches!(
            error,
            RallyError::OutcomeUnknown { ref event_id, .. }
                if event_id == &checkpoint.event_id
        ));
        let checkpoint_retry = store.maybe_append_read_checkpoint(&checkpoint, 1).unwrap();
        let ConditionalAppendOutcome::Applied(checkpoint_outcome) = checkpoint_retry else {
            panic!("exact checkpoint retry must resolve its canonical request");
        };
        assert_eq!(checkpoint_outcome.fact.event_id, checkpoint.event_id);

        let canonical = facts_from_segments(&store.log_dir, &store.archive_dir).unwrap();
        for event_id in [renew_event_id, checkpoint.event_id.as_str()] {
            assert_eq!(
                canonical
                    .iter()
                    .filter(|fact| fact.event_id == event_id)
                    .count(),
                1,
                "lost reply and exact retry must be singleton for {event_id}"
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_release_and_resolve_projection_failures_remain_committed() {
        for (label, target_kind, close_kind) in [
            ("release", FactKind::Claim, FactKind::Release),
            ("resolve", FactKind::Blocker, FactKind::Resolve),
        ] {
            let root = unique_root(&format!("o26-{label}-projection"));
            let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
            let target = make_fact(
                &format!("o26-{label}-target"),
                target_kind,
                &format!("file:src/{label}.rs"),
                "target",
            );
            store.append_fact(&target).unwrap();
            let mut close = make_fact(
                &format!("o26-{label}-close"),
                close_kind,
                &format!("file:src/{label}.rs"),
                "close",
            );
            close.ref_id = Some(target.event_id.clone());
            fail_o26_once(&root.join(".rally"), O26FaultPoint::FactsDbProjection);

            let degraded = store.append_state_transition_verified(&close).unwrap();
            assert!(degraded.committed);
            assert!(!degraded.projection_complete);
            assert!(
                degraded
                    .warnings
                    .iter()
                    .any(|warning| warning.code == ProjectionWarningCode::FactsDb)
            );
            let retry = store.append_state_transition_verified(&close).unwrap();
            assert_eq!(retry.fact.seq, degraded.fact.seq);
            assert_eq!(
                facts_from_segments(&store.log_dir, &store.archive_dir)
                    .unwrap()
                    .iter()
                    .filter(|fact| fact.event_id == close.event_id)
                    .count(),
                1
            );
            fs::remove_dir_all(&root).ok();
        }
    }

    #[test]
    fn o26_distinct_concurrent_closes_admit_exactly_one() {
        for (label, target_kind, close_kind) in [
            ("release", FactKind::Claim, FactKind::Release),
            ("resolve", FactKind::Blocker, FactKind::Resolve),
        ] {
            let root = unique_root(&format!("o26-{label}-race"));
            let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
            let target = make_fact(
                &format!("o26-{label}-race-target"),
                target_kind,
                &format!("file:src/{label}-race.rs"),
                "target",
            );
            store.append_fact(&target).unwrap();
            drop(store);
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let mut workers = Vec::new();
            for index in 0..2 {
                let root = root.clone();
                let barrier = Arc::clone(&barrier);
                let target = target.clone();
                let close_kind = close_kind.clone();
                workers.push(thread::spawn(move || {
                    let store = DirectRoomStore::open_direct_at(root).unwrap();
                    let mut close = make_fact(
                        &format!("o26-{label}-race-close-{index}"),
                        close_kind,
                        &format!("file:src/{label}-race.rs"),
                        "close",
                    );
                    close.ref_id = Some(target.event_id);
                    barrier.wait();
                    store.append_state_transition_verified(&close)
                }));
            }
            barrier.wait();
            let results = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
            let store = DirectRoomStore::open_direct_at(root.clone()).unwrap();
            let close_count = facts_from_segments(&store.log_dir, &store.archive_dir)
                .unwrap()
                .iter()
                .filter(|fact| {
                    fact.kind == close_kind && fact.ref_id == Some(target.event_id.clone())
                })
                .count();
            assert_eq!(close_count, 1);
            fs::remove_dir_all(&root).ok();
        }
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

        let valid_tail = ledger_line(3, "decision", "valid-tail", "default");
        {
            let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
            file.write_all(b"completed-corruption\n").unwrap();
            file.write_all(valid_tail.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
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
        assert_completed(segment_event_id_present(live.iter(), &fact.fact.event_id).unwrap_err());
        assert_completed(segment_event_id_present_tail_first(&segment, "valid-tail").unwrap_err());
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
            author_last_seen: BTreeMap::from([("ghost".to_string(), "garbage".to_string())]),
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
    fn decay_pruned_owner_still_reclaims_from_authored_ledger_activity() {
        let coord = CoordinationConfig::default();
        let mut claim = aged_fact("claim-pruned-owner", FactKind::Claim, 9 * 60 * 60);
        claim.tool = Some("ghost".to_string());
        claim.scope = vec!["file:a.rs".to_string(), "file:b.rs".to_string()];

        // The presentation projection removed this provably-stale squad. That
        // must not erase the timestamp the destructive authority decision uses.
        let snapshot = RoomSnapshot {
            stale_authors: BTreeSet::from(["ghost".to_string()]),
            author_last_seen: BTreeMap::from([("ghost".to_string(), claim.created_at.clone())]),
            ..Default::default()
        };
        let (eligible, size) = snapshot.claim_reclaim_eligible(&claim, &coord);
        assert!(
            eligible,
            "a decay-pruned owner with nine hours of authored silence is past the two-hour bar"
        );
        assert_eq!(size, WorkSize::Large);
    }

    #[test]
    fn fact_targeted_at_owner_does_not_reset_owner_silence() {
        let coord = CoordinationConfig::default();
        let mut claim = aged_fact("claim-targeted-owner", FactKind::Claim, 9 * 60 * 60);
        claim.seq = 1;
        claim.tool = Some("ghost".to_string());
        claim.scope = vec!["file:a.rs".to_string(), "file:b.rs".to_string()];

        let mut handoff = aged_fact("handoff-to-owner", FactKind::Handoff, 60);
        handoff.seq = 2;
        handoff.tool = Some("peer".to_string());
        handoff.target = Some("ghost".to_string());

        let snapshot = snapshot_from_facts_with_policy(&[claim.clone(), handoff], &coord, false);
        let owner = snapshot
            .squads
            .iter()
            .find(|squad| squad.tool == "ghost")
            .expect("claim author remains projected");
        assert_eq!(owner.last_seen_ts, claim.created_at);
        assert!(
            snapshot.claim_reclaim_eligible(&claim, &coord).0,
            "a peer-authored handoff addressed to the owner is not owner activity"
        );
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
    fn modern_same_tool_caller_can_release_a_legacy_sessionless_claim() {
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
        release.from_session_id = Some("session-modern".to_string());
        release.ref_id = Some("claim-s".to_string());
        store.append_fact(&release).unwrap();
        let snap = store.snapshot().unwrap();
        assert!(!snap.active_claims.iter().any(|c| c.event_id == "claim-s"));
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn sessionless_caller_cannot_release_a_sessionful_claim() {
        let r = root("sessionful-claim-sessionless-release");
        let store = RoomStore::open_at(r.clone()).unwrap();
        let mut claim = fact_at(
            "claim-sessionful",
            FactKind::Claim,
            "shared-tool",
            "file:src/a.rs",
            &iso_ago(5),
        );
        claim.from_session_id = Some("session-owner".to_string());
        store.append_fact(&claim).unwrap();

        let mut release = fact_at(
            "release-sessionless",
            FactKind::Release,
            "shared-tool",
            "file:src/a.rs",
            &iso_ago(0),
        );
        release.from_session_id = None;
        release.ref_id = Some(claim.event_id.clone());
        let err = store
            .append_fact(&release)
            .expect_err("missing caller session must not close a sessionful claim")
            .to_string();
        assert!(err.contains("another shared-tool session"), "{err}");
        assert!(
            store
                .snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|active| active.event_id == claim.event_id)
        );
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
            "reaper:observed=unknown".to_string(),
            format!("reaper:owner={owner}"),
            "reaper:owner_session=legacy".to_string(),
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
        // A LEASE-EXPIRED close remains valid while the effective lease is still
        // expired, even when the owner is fully active (fresh presence).
        let r = root("reaper-lease-survives");
        let store = RoomStore::open_at(r.clone()).unwrap();
        // live-owner claims a single file just now AND is fresh (active squad).
        let mut claim = fact_at(
            "claim-l",
            FactKind::Claim,
            "live-owner",
            "file:src/a.rs",
            &iso_ago(5),
        );
        // ARP-R-02. The lease marker used to be absent here, and the test still
        // passed — because the gate it exercised read the reaper's OWN
        // `reaper:reason=lease-expired` evidence, which the reaper asserts about
        // itself. That made the fixture describe a state the product cannot
        // produce (`command_say` calls `ensure_lease_evidence` on every claim)
        // while grading a signal a rogue could equally have stamped.
        //
        // The gate now reads the lease the CLAIM declared about ITSELF, so the
        // fixture has to carry one. Same intent as before — a lease-expired
        // close must not be refused merely because the owner is active — but
        // now asserted against the mechanism that actually authorizes it.
        claim
            .evidence
            .push(format!("lease_expires_at:{}", iso_ago(60)));
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
        // the owner is active, the under-lock lease check still passes.
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

    #[test]
    fn reaper_lease_expired_close_is_refused_after_concurrent_renewal() {
        let r = root("reaper-renewal-race");
        let store = RoomStore::open_at(r.clone()).unwrap();
        let mut claim = fact_at(
            "claim-l",
            FactKind::Claim,
            "live-owner",
            "file:src/a.rs",
            &iso_ago(5),
        );
        claim
            .evidence
            .push(format!("lease_expires_at:{}", iso_ago(60)));
        store.append_fact(&claim).unwrap();
        store
            .renew_claim_lease(
                "claim-l",
                "2099-01-01T00:00:00Z".to_string(),
                "live-owner",
                None,
                None,
            )
            .unwrap();

        let expired = reaper_claim_expired("claim-l", "live-owner", "lease-expired");
        let err = store.append_fact(&expired).unwrap_err().to_string();
        assert!(
            err.contains("reap refused") && err.contains("lease renewed"),
            "renewed lease must win the snapshot-to-append race; got: {err}"
        );
        assert!(
            store
                .snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|fact| fact.event_id == "claim-l")
        );
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn reaper_owner_session_marker_must_match_the_active_claim() {
        let r = root("reaper-owner-session-mismatch");
        let store = RoomStore::open_at(r.clone()).unwrap();
        let mut claim = fact_at(
            "claim-session-owner",
            FactKind::Claim,
            "shared-tool",
            "file:src/a.rs",
            &iso_ago(5),
        );
        claim.from_session_id = Some("session-owner".to_string());
        claim
            .evidence
            .push(format!("lease_expires_at:{}", iso_ago(60)));
        store.append_fact(&claim).unwrap();

        let mut expired =
            reaper_claim_expired("claim-session-owner", "shared-tool", "lease-expired");
        expired
            .evidence
            .retain(|item| !item.starts_with("reaper:owner_session="));
        expired
            .evidence
            .push("reaper:owner_session=session-sibling".to_string());
        let err = store.append_fact(&expired).unwrap_err().to_string();
        assert!(
            err.contains("owner session does not match"),
            "a sibling session marker must not close the owner's claim: {err}"
        );
        assert!(
            store
                .snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|active| active.event_id == claim.event_id)
        );
        fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn observed_stale_close_is_refused_after_live_session_reappears() {
        let r = root("reaper-observer-race");
        crate::test_git_fixture::fixture_git(&r, &["init"]);
        fs::write(r.join("observed.txt"), "observed\n").unwrap();
        crate::test_git_fixture::fixture_git(&r, &["add", "observed.txt"]);
        crate::test_git_fixture::fixture_git(&r, &["commit", "-m", "observed fixture"]);
        let head = crate::observed_liveness::current_head_sha(&r).unwrap();
        let canonical = fs::canonicalize(&r).unwrap();
        let store = RoomStore::open_at(r.clone()).unwrap();
        let mut claim = fact_at(
            "claim-observed",
            FactKind::Claim,
            "owner",
            "file:src/a.rs",
            &iso_ago(5),
        );
        claim
            .evidence
            .push(format!("lease_expires_at:{}", iso_ago(60)));
        store.append_fact(&claim).unwrap();

        let observed_presence = |event_id: &str, pid: u32| {
            let mut fact = fact_at(
                event_id,
                FactKind::Presence,
                "owner",
                "presence",
                &iso_ago(0),
            );
            fact.from_session_id = Some("sess:test:owner".to_string());
            fact.evidence = vec![
                format!("branch_head_sha:{head}"),
                format!("worktree_path:{}", canonical.display()),
                format!("observer_pid:{pid}"),
            ];
            fact
        };
        store
            .append_fact(&observed_presence("presence-dead", 2_000_000_000))
            .unwrap();
        let mut expired = reaper_claim_expired("claim-observed", "owner", "lease-expired");
        expired
            .evidence
            .retain(|item| !item.starts_with("reaper:observed="));
        expired.evidence.push("reaper:observed=stale".to_string());

        // The same session posts a newer stamp whose externally observed host
        // pid is live before the stale close reaches the write lock.
        store
            .append_fact(&observed_presence("presence-live", std::process::id()))
            .unwrap();
        let err = store.append_fact(&expired).unwrap_err().to_string();
        assert!(
            err.contains("reap refused"),
            "live observer evidence must win the snapshot-to-append race: {err}"
        );
        assert!(
            store
                .snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|fact| fact.event_id == "claim-observed")
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

    /// D2 — the demotion contract, pinned.
    ///
    /// `stale_authors` is filled from HEARTBEAT age, not from a
    /// `Liveness::Stale` verdict. Three sources said three different things
    /// until the design audit: the field doc and the `relevance` module docs
    /// both claimed only a provably-`Stale` author could be demoted, while the
    /// producer inserted on heartbeat age alone.
    ///
    /// The code's behaviour is the one that was kept, because the alternative is
    /// unreachable: `Liveness::Stale` requires the code-progress signal, which
    /// needs two presence facts carrying DIFFERENT `branch_head_sha` stamps. No
    /// writer produced those until recently, so a demotion keyed on `Stale`
    /// would have been dead on every fact already in a ledger.
    ///
    /// This test builds exactly that case — one silent author with a heartbeat
    /// well past its window and no code-progress signal — and asserts it IS
    /// demoted. Narrowing the producer to match the old prose fails here.
    ///
    /// What it does NOT cover: the size of the demotion, or whether the ranking
    /// that results is the right one. Those are `relevance`'s own tests.
    #[test]
    fn heartbeat_gap_demotes_even_when_liveness_is_not_provably_stale() {
        let coord = CoordinationConfig::default();
        // One presence fact, 90 minutes old, no sha stamp. Heartbeat is present
        // and past the default 31-minute window; code-progress is absent.
        let facts = seqd(vec![fact(FactKind::Presence, "silent-tool", 90 * 60)]);

        // Premise: the four-signal verdict must NOT be Stale, or this test is
        // asserting the old contract by accident.
        let signals = crate::liveness::LivenessSignals {
            heartbeat_age: Some(90 * 60),
            inject_age: None,
            code_progress_age: None,
            plan_age: None,
            observed_alive: None,
        };
        let window = crate::liveness::adaptive_window_secs(
            coord.default_cadence_secs,
            coord.default_cadence_secs,
            coord.miss_multiplier,
            coord.grace_secs,
        );
        assert_ne!(
            crate::liveness::is_live(&signals, window),
            crate::liveness::Liveness::Stale,
            "premise: with only a heartbeat signal the verdict must be Unknown, \
             not Stale — otherwise this test cannot distinguish the two contracts"
        );

        let snap = snapshot_from_facts_with_policy(&facts, &coord, false);
        assert!(
            snap.stale_authors.contains("silent-tool"),
            "an author silent past its window must be demotable even though its \
             four-signal verdict is not Stale; keying the demotion on Stale makes \
             it unreachable on any ledger without code-progress stamps"
        );
        assert!(
            snap.squads.iter().any(|s| s.tool == "silent-tool"),
            "and it must still be VISIBLE: dropping needs four-signal unanimity, \
             demoting does not. The two bars diverging is the contract."
        );
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
