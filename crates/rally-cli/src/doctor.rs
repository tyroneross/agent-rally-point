// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — diagnostics and remediation for path hygiene, room registry, and stale state.
//!
//! Nine independent modes:
//!   (no mode)          ledger-health — the default; see below
//!   --ledger-health    read the JSONL ledger as raw text and report its health
//!   --repair-ledger    renumber rows to unique increasing seqs (dry-run; write with --apply)
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply
//!   --reap-stale       reap over-TTL in-room claims and stale lead leases (dry-run; commit with --apply)
//!   --sweep-corrupt    move facts.db.corrupt.* snapshots to the archive (dry-run; move with --apply)
//!   --compact-log      render a diagnostic log with presence/heartbeat runs collapsed into counts
//!   --binary-skew      compare the RUNNING binary's build stamp against this repo's HEAD
//!   --migrate-db-only  inspect or explicitly migrate a current-format DB-only room offline
//!
//! # Two rules this module exists under
//!
//! **Doctor must work ON a broken store.** Corruption is doctor's trigger
//! condition, not an excuse to bail. Every mode that only needs a path resolves
//! it from `repo_root()` rather than opening a `RoomStore` — `run_sweep_corrupt`
//! used to open one purely to learn `repo_root/.rally`, which meant a ledger with
//! two rows at one seq took down the repair tool with the same error the operator
//! was already staring at. `--ledger-health` reads raw files and touches neither
//! the store nor the derived DB, so it is the mode guaranteed to answer; bare
//! `rally doctor` runs it, because it is what a human types first when the room
//! is broken.
//!
//! **Doctor never deletes.** Anything taken out of the live store is MOVED under
//! `.rally/archive/` — swept snapshots to `archive/swept/<stamp>/`, pre-repair
//! segments to `archive/pre-repair/<stamp>/` — and the move happens before any
//! rewrite. A quarantined `facts.db.corrupt.*` file is the forensic record of the
//! incident that produced it; deleting it as "cleanup" destroys the evidence
//! needed to explain the outage. Retiring anything from the archive is a human
//! decision made elsewhere.

#[cfg(unix)]
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discovery::{
    DiscoveryWarning, KnownRoom, RoomIndex, read_room_index_at, room_index_path,
    write_room_index_at,
};
use crate::error::{RallyError, Result};
use crate::store::{
    ARCHIVE_DIRNAME, DB_ONLY_MIGRATION_MARKER_FILENAME, DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME,
    DbOnlyMigrationSegment, DbOnlyMigrationSourceRow, LEDGER_FILENAME, LOG_DIRNAME, RoomStore,
    acquire_offline_migration_authority, canonical_repo_root_string, ensure_new_mutation_can_start,
    is_reserved_fixture_engagement, observe_offline_migration_authority,
    render_db_only_migration_segment, sync_directory, validate_scoped_engagement,
    verify_db_only_migration_extension, verify_db_only_migration_segment,
};
use crate::{
    mark_watchdog_command_commit, mark_watchdog_db_only_migration_outcome_unknown, normalize_path,
    now_string, paths_suffix_collide, repo_root, shell_quote,
};

/// Prefix of every quarantined snapshot the store writes on corrupt-db detection.
const CORRUPT_PREFIX: &str = "facts.db.corrupt.";

/// Subdirectory of `.rally/archive/` where doctor parks anything it takes out of
/// the live store. Doctor ARCHIVES; it never deletes. Whatever lands here stays
/// until a human decides otherwise.
const SWEPT_SUBDIR: &str = "swept";

/// seq -> the full serialized row that claimed it. Full-line equality is the
/// store's own rule for whether a repeated seq is benign.
type SeqRows = BTreeMap<i64, String>;
pub(crate) const DB_ONLY_MIGRATION_RECEIPT_FILENAME: &str = "db-only-migration.v1.receipt.json";
const DB_ONLY_MIGRATION_RECEIPT_STAGE_FILENAME: &str = "db-only-migration.v1.receipt.tmp";
const DB_ONLY_MIGRATION_MARKER_SCHEMA: &str = "agent-rally.db-only-migration.v1";
const DB_ONLY_MIGRATION_RECEIPT_SCHEMA: &str = "agent-rally.db-only-migration-receipt.v1";
/// Default number of newest corrupt snapshots to retain for forensics.
pub(crate) const SWEEP_DEFAULT_KEEP: i64 = 1;
/// Default: also retain any snapshot newer than this many days.
pub(crate) const SWEEP_DEFAULT_MAX_AGE_DAYS: i64 = 7;
const NS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;

// =============================================================================
// Output types
// =============================================================================

/// An active claim scope that is not already in canonical form.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct NonCanonicalScope {
    /// The tool that owns the claim.
    pub(crate) tool: String,
    /// The raw scope stored in the fact.
    pub(crate) scope: String,
    /// What `normalize_path` would produce.
    pub(crate) canonical: String,
}

/// A pair of active claim scopes (from different tools) whose paths
/// share a 2+ component trailing suffix — the ambiguous same-file-different-spelling case.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SuffixCollision {
    pub(crate) tool_a: String,
    pub(crate) scope_a: String,
    pub(crate) tool_b: String,
    pub(crate) scope_b: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CanonicalPathsReport {
    pub(crate) non_canonical: Vec<NonCanonicalScope>,
    pub(crate) suffix_collisions: Vec<SuffixCollision>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// One entry's classification for prune-rooms.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct StaleRoom {
    pub(crate) repo_root: PathBuf,
    pub(crate) display_name: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct PruneRoomsReport {
    /// Number of rooms whose `repo_root` directory still exists.
    pub(crate) live: usize,
    /// Entries that would be (or were) removed.
    pub(crate) stale: Vec<StaleRoom>,
    /// Whether the index was actually rewritten.
    pub(crate) applied: bool,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DbOnlyMigrationState {
    DryRun,
    Committed,
    AlreadyCommitted,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub(crate) struct DbOnlyMigrationDbBinding {
    pub(crate) sha256: String,
    pub(crate) byte_len: u64,
    pub(crate) row_count: u64,
    pub(crate) logical_max_seq: i64,
    pub(crate) normalized_rows_sha256: String,
    pub(crate) normalized_rows_len: u64,
    pub(crate) wal_present: bool,
    pub(crate) wal_len: u64,
    pub(crate) shm_present: bool,
    pub(crate) shm_len: u64,
    pub(crate) shm_sha256: Option<String>,
    pub(crate) journal_present: bool,
    pub(crate) journal_len: u64,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DbOnlyMigrationReport {
    pub(crate) state: DbOnlyMigrationState,
    pub(crate) applied: bool,
    pub(crate) apply_requires_revalidation: bool,
    pub(crate) repo_root: PathBuf,
    pub(crate) engagement: String,
    pub(crate) migration_id: Option<String>,
    pub(crate) source_token: Option<String>,
    pub(crate) observed_blockers: Vec<String>,
    pub(crate) owner_observation: String,
    pub(crate) target_path: PathBuf,
    pub(crate) marker_path: PathBuf,
    pub(crate) receipt_path: PathBuf,
    pub(crate) row_count: Option<u64>,
    pub(crate) max_seq: Option<i64>,
    pub(crate) db_sha256: Option<String>,
    pub(crate) normalized_rows_sha256: Option<String>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DbOnlyMigrationInterruptionState {
    Prepared,
    OutcomeUnknown,
    CommittedCleanupPending,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct DbOnlyMigrationInterruption {
    pub(crate) state: DbOnlyMigrationInterruptionState,
    pub(crate) migration_id: String,
    pub(crate) phase: String,
    pub(crate) retry_safe: bool,
    pub(crate) retry_command: String,
    pub(crate) detail: String,
}

#[derive(Debug)]
pub(crate) enum DbOnlyMigrationRunError {
    Interrupted(DbOnlyMigrationInterruption),
    Other(RallyError),
}

impl DbOnlyMigrationRunError {
    pub(crate) fn into_rally_error(self) -> RallyError {
        match self {
            Self::Interrupted(interruption) => RallyError::Command(interruption.to_string()),
            Self::Other(error) => error,
        }
    }
}

impl fmt::Display for DbOnlyMigrationRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted(interruption) => interruption.fmt(formatter),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DbOnlyMigrationRunError {}

impl fmt::Display for DbOnlyMigrationInterruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            DbOnlyMigrationInterruptionState::Prepared => "prepared",
            DbOnlyMigrationInterruptionState::OutcomeUnknown => "outcome unknown",
            DbOnlyMigrationInterruptionState::CommittedCleanupPending => {
                "committed cleanup pending"
            }
        };
        write!(
            formatter,
            "db-only migration {state}: migration_id={} phase={} retry_safe={} {}; {}",
            self.migration_id, self.phase, self.retry_safe, self.detail, self.retry_command
        )
    }
}

impl From<RallyError> for DbOnlyMigrationRunError {
    fn from(error: RallyError) -> Self {
        Self::Other(error)
    }
}

type DbOnlyMigrationResult<T> = std::result::Result<T, DbOnlyMigrationRunError>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DbOnlyMigrationMarker {
    schema: String,
    migration_id: String,
    created_at: String,
    canonical_repo_root: String,
    engagement: String,
    target_relative_path: String,
    temp_relative_path: String,
    db: DbOnlyMigrationDbBinding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DbOnlyMigrationReceipt {
    schema: String,
    completed_at: String,
    marker: DbOnlyMigrationMarker,
    target_sha256: String,
    target_len: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DbOnlyMigrationFaultPoint {
    MutateShmAfterDbRead,
    #[cfg(test)]
    ExpireDeadlineBeforeMarker,
    AfterMarkerSync,
    AfterTempSync,
    AfterTempReadback,
    AfterDbRevalidation,
    AfterTargetInstallBeforeDirectorySync,
    AfterTargetDirectorySync,
    AfterReceiptSync,
    AfterMarkerRemoval,
}

#[cfg(test)]
fn db_only_migration_faults()
-> &'static Mutex<BTreeMap<(PathBuf, DbOnlyMigrationFaultPoint), usize>> {
    static FAULTS: OnceLock<Mutex<BTreeMap<(PathBuf, DbOnlyMigrationFaultPoint), usize>>> =
        OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
pub(crate) fn arm_db_only_migration_fault(rally_dir: &Path, point: DbOnlyMigrationFaultPoint) {
    *db_only_migration_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((rally_dir.to_path_buf(), point))
        .or_default() += 1;
}

#[cfg(test)]
fn take_db_only_migration_fault(rally_dir: &Path, point: DbOnlyMigrationFaultPoint) -> bool {
    let mut faults = db_only_migration_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (rally_dir.to_path_buf(), point);
    let Some(count) = faults.get_mut(&key) else {
        return false;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        faults.remove(&key);
    }
    true
}

#[cfg(not(test))]
fn take_db_only_migration_fault(_rally_dir: &Path, _point: DbOnlyMigrationFaultPoint) -> bool {
    false
}

// =============================================================================
// canonical-paths logic
// =============================================================================

pub(crate) fn run_canonical_paths() -> Result<CanonicalPathsReport> {
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;

    // Collect (tool, scope) pairs from active claims only.
    let claim_scopes: Vec<(String, String)> = snapshot
        .active_claims
        .iter()
        .flat_map(|fact| {
            let tool = fact.tool.clone().unwrap_or_default();
            fact.scope
                .iter()
                // Skip the external-intake sentinel — it is a system tag, not a path.
                .filter(|s| *s != "external-intake")
                // Only path-like scopes (file: prefix or relative/absolute without scheme).
                .filter(|s| !s.contains("://") || s.starts_with("file:"))
                .map(move |s| (tool.clone(), s.clone()))
        })
        .collect();

    // Non-canonical: scopes where normalize_path changes the value.
    let non_canonical: Vec<NonCanonicalScope> = claim_scopes
        .iter()
        .filter_map(|(tool, scope)| {
            let canonical = normalize_path(scope.clone());
            if canonical != *scope {
                Some(NonCanonicalScope {
                    tool: tool.clone(),
                    scope: scope.clone(),
                    canonical,
                })
            } else {
                None
            }
        })
        .collect();

    // Suffix collisions: pairs of scopes from different tools that suffix-collide.
    // Strip file: prefix for the comparison (paths_suffix_collide works on either).
    let mut suffix_collisions: Vec<SuffixCollision> = Vec::new();
    for i in 0..claim_scopes.len() {
        for j in (i + 1)..claim_scopes.len() {
            let (tool_a, scope_a) = &claim_scopes[i];
            let (tool_b, scope_b) = &claim_scopes[j];
            // Only flag cross-tool collisions (same tool claiming the same file is fine).
            if tool_a == tool_b {
                continue;
            }
            let bare_a = scope_a.strip_prefix("file:").unwrap_or(scope_a.as_str());
            let bare_b = scope_b.strip_prefix("file:").unwrap_or(scope_b.as_str());
            if paths_suffix_collide(bare_a, bare_b) {
                suffix_collisions.push(SuffixCollision {
                    tool_a: tool_a.clone(),
                    scope_a: scope_a.clone(),
                    tool_b: tool_b.clone(),
                    scope_b: scope_b.clone(),
                });
            }
        }
    }

    Ok(CanonicalPathsReport {
        non_canonical,
        suffix_collisions,
        warnings: Vec::new(),
    })
}

// =============================================================================
// Explicit offline DB-only migration
// =============================================================================

#[derive(Clone, Debug, Eq, PartialEq)]
struct PhysicalDbToken {
    sha256: String,
    byte_len: u64,
    wal_present: bool,
    wal_len: u64,
    shm_present: bool,
    shm_len: u64,
    shm_sha256: Option<String>,
    journal_present: bool,
    journal_len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SidecarToken {
    present: bool,
    byte_len: u64,
    sha256: Option<String>,
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn safe_file_bytes(path: &Path, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(RallyError::io(format!("stat {label} {}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(RallyError::Usage(format!(
            "{label} {} must be a regular file, not a symlink or special file",
            path.display()
        )));
    }
    fs::read(path).map_err(RallyError::io(format!("read {label} {}", path.display())))
}

fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RallyError::Usage(format!(
            "{label} {} is a symlink; migration refuses to follow it",
            path.display()
        ))),
        Ok(_) => Err(RallyError::Usage(format!(
            "{label} {} must be a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RallyError::io(format!("stat {label} {}", path.display()))(
            error,
        )),
    }
}

fn ensure_real_directory(path: &Path, label: &str, required: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RallyError::Usage(format!(
            "{label} {} is a symlink; migration refuses to follow it",
            path.display()
        ))),
        Ok(_) => Err(RallyError::Usage(format!(
            "{label} {} must be a directory",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(RallyError::Usage(format!(
            "required {label} {} does not exist",
            path.display()
        ))),
        Err(error) => Err(RallyError::io(format!("stat {label} {}", path.display()))(
            error,
        )),
    }
}

fn inspect_sidecar(path: &Path, label: &str, allow_nonempty: bool) -> Result<SidecarToken> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let bytes = fs::read(path).map_err(RallyError::io(format!(
                "read SQLite {label} {}",
                path.display()
            )))?;
            let len = u64::try_from(bytes.len()).map_err(|error| {
                RallyError::Message(format!("SQLite {label} length overflow: {error}"))
            })?;
            let metadata_after = fs::symlink_metadata(path).map_err(RallyError::io(format!(
                "restat SQLite {label} {}",
                path.display()
            )))?;
            if metadata_after.file_type().is_symlink()
                || !metadata_after.file_type().is_file()
                || metadata_after.len() != len
            {
                return Err(RallyError::Command(format!(
                    "SQLite {label} {} changed while its migration identity was read; no migration state was published",
                    path.display()
                )));
            }
            if len > 0 && !allow_nonempty {
                return Err(RallyError::Usage(format!(
                    "offline DB-only migration refuses nonempty SQLite WAL/rollback recovery sidecar {label} {} ({len} bytes); preserve it and close/checkpoint SQLite before retrying",
                    path.display()
                )));
            }
            Ok(SidecarToken {
                present: true,
                byte_len: len,
                sha256: Some(sha256_bytes(&bytes)),
            })
        }
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RallyError::Usage(format!(
            "SQLite {label} {} is a symlink; migration refuses to follow it",
            path.display()
        ))),
        Ok(_) => Err(RallyError::Usage(format!(
            "SQLite {label} {} must be a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SidecarToken {
            present: false,
            byte_len: 0,
            sha256: None,
        }),
        Err(error) => Err(RallyError::io(format!(
            "stat SQLite {label} {}",
            path.display()
        ))(error)),
    }
}

fn inspect_physical_db(facts_db_path: &Path) -> Result<PhysicalDbToken> {
    let bytes = safe_file_bytes(facts_db_path, "facts.db")?;
    let byte_len = u64::try_from(bytes.len())
        .map_err(|error| RallyError::Message(format!("facts.db length overflow: {error}")))?;
    let metadata_after = fs::symlink_metadata(facts_db_path).map_err(RallyError::io(format!(
        "restat facts.db {}",
        facts_db_path.display()
    )))?;
    if metadata_after.file_type().is_symlink()
        || !metadata_after.file_type().is_file()
        || metadata_after.len() != byte_len
    {
        return Err(RallyError::Command(
            "facts.db changed while its migration source token was being read; no migration state was published"
                .to_string(),
        ));
    }
    let wal = inspect_sidecar(
        &facts_db_path.with_extension("db-wal"),
        "facts.db-wal",
        false,
    )?;
    let shm = inspect_sidecar(
        &facts_db_path.with_extension("db-shm"),
        "facts.db-shm",
        true,
    )?;
    let journal = inspect_sidecar(
        &facts_db_path.with_extension("db-journal"),
        "facts.db-journal",
        false,
    )?;
    Ok(PhysicalDbToken {
        sha256: sha256_bytes(&bytes),
        byte_len,
        wal_present: wal.present,
        wal_len: wal.byte_len,
        shm_present: shm.present,
        shm_len: shm.byte_len,
        shm_sha256: shm.sha256,
        journal_present: journal.present,
        journal_len: journal.byte_len,
    })
}

fn sqlite_read_error(context: &str, error: rusqlite::Error) -> RallyError {
    RallyError::Message(format!("{context}: {error}"))
}

#[cfg(unix)]
fn immutable_sqlite_uri(path: &Path) -> String {
    let mut encoded = String::from("file:");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded.push_str("?mode=ro&immutable=1");
    encoded
}

#[cfg(not(unix))]
fn immutable_sqlite_uri(_path: &Path) -> String {
    String::new()
}

/// Read the current-format source without invoking the ordinary store open.
/// The normal store bootstrap is intentionally read-write: it enables WAL and
/// runs schema DDL. Migration inspection must instead leave every source byte
/// and sidecar identity unchanged.
#[cfg(unix)]
fn read_db_only_rows(facts_db_path: &Path) -> Result<Vec<DbOnlyMigrationSourceRow>> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let connection = Connection::open_with_flags(immutable_sqlite_uri(facts_db_path), flags)
        .map_err(|error| sqlite_read_error("open facts.db read-only", error))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| sqlite_read_error("set facts.db query_only", error))?;

    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| sqlite_read_error("run facts.db integrity_check", error))?;
    if integrity != "ok" {
        return Err(RallyError::Usage(format!(
            "facts.db integrity_check returned {integrity:?}; migration preserves the DB and refuses to quarantine or rewrite it"
        )));
    }
    let store_format: Option<String> = connection
        .query_row(
            "SELECT value FROM store_metadata WHERE key = 'store_format_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| sqlite_read_error("read facts.db store format", error))?;
    if store_format.as_deref() != Some("1") {
        return Err(RallyError::Usage(format!(
            "facts.db has unsupported store_format_version {store_format:?}; expected current format 1"
        )));
    }

    let mut statement = connection
        .prepare(
            "SELECT sequence_number, occurred_at, event_type, payload \
             FROM events ORDER BY sequence_number ASC",
        )
        .map_err(|error| sqlite_read_error("prepare facts.db event scan", error))?;
    let mapped = statement
        .query_map([], |row| {
            let payload_text: String = row.get(3)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                payload_text,
            ))
        })
        .map_err(|error| sqlite_read_error("query facts.db events", error))?;
    let mut rows = Vec::new();
    for mapped_row in mapped {
        let (database_seq, occurred_at, event_type, payload_text) =
            mapped_row.map_err(|error| sqlite_read_error("decode facts.db event row", error))?;
        let payload = serde_json::from_str(&payload_text).map_err(RallyError::json(format!(
            "parse facts.db payload at seq {database_seq}"
        )))?;
        rows.push(DbOnlyMigrationSourceRow {
            database_seq,
            occurred_at,
            event_type,
            payload,
        });
    }
    drop(statement);
    connection
        .close()
        .map_err(|(_, error)| sqlite_read_error("close facts.db read-only", error))?;
    Ok(rows)
}

#[cfg(not(unix))]
fn read_db_only_rows(_facts_db_path: &Path) -> Result<Vec<DbOnlyMigrationSourceRow>> {
    Err(RallyError::Usage(
        "rally doctor --migrate-db-only is unsupported on this platform".to_string(),
    ))
}

fn inspect_closed_db_candidate(
    facts_db_path: &Path,
    engagement: &str,
) -> Result<(DbOnlyMigrationSegment, DbOnlyMigrationDbBinding)> {
    let before = inspect_physical_db(facts_db_path)?;
    let rows = read_db_only_rows(facts_db_path)?;
    if take_db_only_migration_fault(
        facts_db_path.parent().unwrap_or_else(|| Path::new(".")),
        DbOnlyMigrationFaultPoint::MutateShmAfterDbRead,
    ) {
        let shm_path = facts_db_path.with_extension("db-shm");
        fs::write(&shm_path, b"changed during read").map_err(RallyError::io(format!(
            "inject SHM mutation {}",
            shm_path.display()
        )))?;
    }
    let candidate = render_db_only_migration_segment(rows, engagement)?;
    let after = inspect_physical_db(facts_db_path)?;
    if before != after {
        return Err(RallyError::Command(format!(
            "facts.db changed across the closed migration row read (before {}/{}, after {}/{}); no marker was published",
            before.sha256, before.byte_len, after.sha256, after.byte_len
        )));
    }
    let binding = DbOnlyMigrationDbBinding {
        sha256: after.sha256,
        byte_len: after.byte_len,
        row_count: candidate.row_count,
        logical_max_seq: candidate.max_seq,
        normalized_rows_sha256: sha256_bytes(&candidate.bytes),
        normalized_rows_len: u64::try_from(candidate.bytes.len()).map_err(|error| {
            RallyError::Message(format!("normalized row length overflow: {error}"))
        })?,
        wal_present: after.wal_present,
        wal_len: after.wal_len,
        shm_present: after.shm_present,
        shm_len: after.shm_len,
        shm_sha256: after.shm_sha256,
        journal_present: after.journal_present,
        journal_len: after.journal_len,
    };
    Ok((candidate, binding))
}

fn source_token(binding: &DbOnlyMigrationDbBinding) -> Result<String> {
    let rendered =
        serde_json::to_vec(binding).map_err(RallyError::json("render DB-only source token"))?;
    Ok(format!("db-only-v1:{}", sha256_bytes(&rendered)))
}

fn migration_id(binding: &DbOnlyMigrationDbBinding, engagement: &str) -> String {
    let identity = format!(
        "{}:{}:{}:{}",
        binding.sha256, binding.normalized_rows_sha256, binding.row_count, engagement
    );
    format!("dbmig-{}", &sha256_bytes(identity.as_bytes())[..24])
}

fn migration_remedy(engagement: &str) -> String {
    format!(
        "rally doctor --migrate-db-only --engagement {} --apply --json",
        shell_quote(engagement)
    )
}

fn mark_migration_watchdog(marker: &DbOnlyMigrationMarker, phase: &str) {
    mark_watchdog_db_only_migration_outcome_unknown(
        &marker.migration_id,
        phase,
        &migration_remedy(&marker.engagement),
    );
}

fn interruption(
    marker: &DbOnlyMigrationMarker,
    state: DbOnlyMigrationInterruptionState,
    phase: &str,
    detail: impl Into<String>,
) -> DbOnlyMigrationRunError {
    DbOnlyMigrationRunError::Interrupted(DbOnlyMigrationInterruption {
        state,
        migration_id: marker.migration_id.clone(),
        phase: phase.to_string(),
        retry_safe: state != DbOnlyMigrationInterruptionState::OutcomeUnknown,
        retry_command: migration_remedy(&marker.engagement),
        detail: detail.into(),
    })
}

fn classify_interruption(
    error: DbOnlyMigrationRunError,
    marker: &DbOnlyMigrationMarker,
    state: DbOnlyMigrationInterruptionState,
    phase: &str,
) -> DbOnlyMigrationRunError {
    match error {
        DbOnlyMigrationRunError::Interrupted(_) => error,
        DbOnlyMigrationRunError::Other(error) => {
            interruption(marker, state, phase, error.to_string())
        }
    }
}

fn maybe_fault(
    rally_dir: &Path,
    marker: &DbOnlyMigrationMarker,
    point: DbOnlyMigrationFaultPoint,
    state: DbOnlyMigrationInterruptionState,
    phase: &str,
) -> DbOnlyMigrationResult<()> {
    if take_db_only_migration_fault(rally_dir, point) {
        return Err(interruption(
            marker,
            state,
            phase,
            "injected path-scoped migration interruption",
        ));
    }
    Ok(())
}

fn validate_migration_topology(root: &Path) -> Result<()> {
    let rally_dir = root.join(".rally");
    ensure_real_directory(&rally_dir, ".rally directory", true)?;
    ensure_real_directory(
        &rally_dir.join(LOG_DIRNAME),
        "canonical log directory",
        false,
    )?;
    ensure_real_directory(
        &rally_dir.join(ARCHIVE_DIRNAME),
        "canonical archive directory",
        false,
    )?;
    ensure_regular_or_missing(&rally_dir.join("facts.db"), "facts.db")?
        .then_some(())
        .ok_or_else(|| {
            RallyError::Usage(
                "facts.db does not exist; there is no DB-only history to migrate".to_string(),
            )
        })?;
    for (filename, label) in [
        (DB_ONLY_MIGRATION_MARKER_FILENAME, "migration marker"),
        (
            DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME,
            "migration marker staging file",
        ),
        (DB_ONLY_MIGRATION_RECEIPT_FILENAME, "migration receipt"),
        (
            DB_ONLY_MIGRATION_RECEIPT_STAGE_FILENAME,
            "migration receipt staging file",
        ),
        ("direct.owner.lock", "direct-owner lock"),
        ("rallyd.owner.lock", "daemon-owner lock"),
        ("mutation.lock", "mutation lock"),
    ] {
        ensure_regular_or_missing(&rally_dir.join(filename), label)?;
    }
    Ok(())
}

fn canonical_jsonl_paths(dir: &Path, label: &str) -> Result<Vec<PathBuf>> {
    if !ensure_real_directory(dir, label, false)? {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(RallyError::io(format!("read_dir {}", dir.display())))? {
        let entry = entry.map_err(RallyError::io(format!("read entry {}", dir.display())))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(RallyError::io(format!(
            "stat canonical source {}",
            path.display()
        )))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(RallyError::Usage(format!(
                "canonical source {} must be a regular file; migration refuses symlinks and special files",
                path.display()
            )));
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

fn validate_canonical_source_set(root: &Path, allowed_target: Option<&Path>) -> Result<()> {
    let rally_dir = root.join(".rally");
    let legacy = rally_dir.join(LEDGER_FILENAME);
    match fs::symlink_metadata(&legacy) {
        Ok(_) => {
            return Err(RallyError::Usage(format!(
                "canonical source {} already exists; DB-only migration requires no legacy ledger",
                legacy.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RallyError::io(format!(
                "stat legacy canonical source {}",
                legacy.display()
            ))(error));
        }
    }
    let archive = canonical_jsonl_paths(
        &rally_dir.join(ARCHIVE_DIRNAME),
        "canonical archive directory",
    )?;
    if let Some(path) = archive.first() {
        return Err(RallyError::Usage(format!(
            "canonical source {} already exists; DB-only migration refuses mixed history",
            path.display()
        )));
    }
    for path in canonical_jsonl_paths(&rally_dir.join(LOG_DIRNAME), "canonical log directory")? {
        if allowed_target.is_some_and(|allowed| allowed == path) {
            continue;
        }
        return Err(RallyError::Usage(format!(
            "canonical source {} already exists; DB-only migration refuses mixed history",
            path.display()
        )));
    }
    Ok(())
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T> {
    let bytes = safe_file_bytes(path, label)?;
    serde_json::from_slice(&bytes).map_err(RallyError::json(format!(
        "parse {label} {}",
        path.display()
    )))
}

fn write_new_synced(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(RallyError::io(format!("create {label} {}", path.display())))?;
    file.write_all(bytes)
        .map_err(RallyError::io(format!("write {label} {}", path.display())))?;
    file.sync_all()
        .map_err(RallyError::io(format!("sync {label} {}", path.display())))?;
    let observed = safe_file_bytes(path, label)?;
    if observed != bytes {
        return Err(RallyError::Message(format!(
            "{label} {} failed exact readback",
            path.display()
        )));
    }
    Ok(())
}

fn install_no_clobber(staging: &Path, target: &Path, expected: &[u8], label: &str) -> Result<()> {
    match fs::hard_link(staging, target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let observed = safe_file_bytes(target, label)?;
            if observed != expected {
                return Err(RallyError::Usage(format!(
                    "existing {label} {} differs from the marker-bound content; evidence was preserved",
                    target.display()
                )));
            }
        }
        Err(error) => {
            return Err(RallyError::io(format!(
                "install {label} {} without clobber",
                target.display()
            ))(error));
        }
    }
    let observed = safe_file_bytes(target, label)?;
    if observed != expected {
        return Err(RallyError::Message(format!(
            "installed {label} {} failed exact readback",
            target.display()
        )));
    }
    let file = fs::File::open(target)
        .map_err(RallyError::io(format!("open {label} {}", target.display())))?;
    file.sync_all()
        .map_err(RallyError::io(format!("sync {label} {}", target.display())))?;
    Ok(())
}

fn validate_marker(marker: &DbOnlyMigrationMarker, expected: &DbOnlyMigrationMarker) -> Result<()> {
    if marker.schema != DB_ONLY_MIGRATION_MARKER_SCHEMA {
        return Err(RallyError::Usage(format!(
            "migration marker has unsupported schema {:?}; evidence was preserved",
            marker.schema
        )));
    }
    chrono::DateTime::parse_from_rfc3339(&marker.created_at).map_err(|error| {
        RallyError::Usage(format!(
            "migration marker has invalid created_at {:?}: {error}; evidence was preserved",
            marker.created_at
        ))
    })?;
    let mut expected = expected.clone();
    expected.created_at.clone_from(&marker.created_at);
    if marker != &expected {
        return Err(RallyError::Usage(format!(
            "migration marker binding mismatch for migration {}; DB, engagement, target, counts, or checksums changed; evidence was preserved",
            marker.migration_id
        )));
    }
    Ok(())
}

fn expected_marker(
    root: &Path,
    engagement: &str,
    binding: DbOnlyMigrationDbBinding,
) -> DbOnlyMigrationMarker {
    let id = migration_id(&binding, engagement);
    DbOnlyMigrationMarker {
        schema: DB_ONLY_MIGRATION_MARKER_SCHEMA.to_string(),
        migration_id: id.clone(),
        created_at: now_string(),
        canonical_repo_root: canonical_repo_root_string(root),
        engagement: engagement.to_string(),
        target_relative_path: format!(".rally/{LOG_DIRNAME}/{engagement}.jsonl"),
        temp_relative_path: format!(".rally/{LOG_DIRNAME}/.db-only-migration.v1-{id}.segment.tmp"),
        db: binding,
    }
}

fn load_or_publish_marker(
    rally_dir: &Path,
    expected: &DbOnlyMigrationMarker,
) -> DbOnlyMigrationResult<DbOnlyMigrationMarker> {
    let marker_path = rally_dir.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
    let stage_path = rally_dir.join(DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME);
    if ensure_regular_or_missing(&marker_path, "migration marker")? {
        let marker: DbOnlyMigrationMarker = read_json_file(&marker_path, "migration marker")?;
        validate_marker(&marker, expected)?;
        if ensure_regular_or_missing(&stage_path, "migration marker staging file")? {
            let staged: DbOnlyMigrationMarker =
                read_json_file(&stage_path, "migration marker staging file")?;
            if staged != marker {
                return Err(RallyError::Usage(
                    "migration marker staging file differs from the installed marker; evidence was preserved"
                        .to_string(),
                )
                .into());
            }
        }
        let file = fs::File::open(&marker_path).map_err(RallyError::io(format!(
            "open migration marker {}",
            marker_path.display()
        )))?;
        file.sync_all().map_err(RallyError::io(format!(
            "sync migration marker {}",
            marker_path.display()
        )))?;
        sync_directory(rally_dir)?;
        return Ok(marker);
    }

    let marker = if ensure_regular_or_missing(&stage_path, "migration marker staging file")? {
        let staged: DbOnlyMigrationMarker =
            read_json_file(&stage_path, "migration marker staging file")?;
        validate_marker(&staged, expected)?;
        staged
    } else {
        let rendered = serde_json::to_vec_pretty(expected)
            .map_err(RallyError::json("render migration marker"))?;
        write_new_synced(&stage_path, &rendered, "migration marker staging file")?;
        expected.clone()
    };
    let staged_bytes = safe_file_bytes(&stage_path, "migration marker staging file")?;
    install_no_clobber(&stage_path, &marker_path, &staged_bytes, "migration marker")?;
    sync_directory(rally_dir)?;
    let installed: DbOnlyMigrationMarker = read_json_file(&marker_path, "migration marker")?;
    if installed != marker {
        return Err(RallyError::Usage(
            "installed migration marker differs from its create-new staging file; evidence was preserved"
                .to_string(),
        )
        .into());
    }
    Ok(marker)
}

fn ensure_candidate_temp(
    root: &Path,
    rally_dir: &Path,
    marker: &DbOnlyMigrationMarker,
    candidate: &DbOnlyMigrationSegment,
    allow_prefix_repair: bool,
) -> DbOnlyMigrationResult<PathBuf> {
    let log_dir = rally_dir.join(LOG_DIRNAME);
    if !ensure_real_directory(&log_dir, "canonical log directory", false)? {
        fs::create_dir(&log_dir).map_err(RallyError::io(format!(
            "create canonical log directory {}",
            log_dir.display()
        )))?;
        sync_directory(rally_dir)?;
    }
    let temp_path = root.join(&marker.temp_relative_path);
    if temp_path.parent() != Some(log_dir.as_path()) {
        return Err(RallyError::Usage(
            "migration marker temp path escapes the canonical log directory; evidence was preserved"
                .to_string(),
        )
        .into());
    }
    if ensure_regular_or_missing(&temp_path, "marker-bound migration temp")? {
        let observed = safe_file_bytes(&temp_path, "marker-bound migration temp")?;
        if observed != candidate.bytes {
            if !allow_prefix_repair || !candidate.bytes.starts_with(&observed) {
                return Err(RallyError::Usage(format!(
                    "existing migration temp {} cannot be repaired: the canonical target is already present or its bytes are not an exact prefix of the marker-bound candidate; evidence was preserved",
                    temp_path.display(),
                ))
                .into());
            }
            let mut file =
                OpenOptions::new()
                    .append(true)
                    .open(&temp_path)
                    .map_err(RallyError::io(format!(
                        "open migration temp {}",
                        temp_path.display()
                    )))?;
            file.write_all(&candidate.bytes[observed.len()..])
                .map_err(RallyError::io(format!(
                    "complete migration temp {}",
                    temp_path.display()
                )))?;
            file.sync_all().map_err(RallyError::io(format!(
                "sync migration temp {}",
                temp_path.display()
            )))?;
        } else {
            let file = fs::File::open(&temp_path).map_err(RallyError::io(format!(
                "open migration temp {}",
                temp_path.display()
            )))?;
            file.sync_all().map_err(RallyError::io(format!(
                "sync migration temp {}",
                temp_path.display()
            )))?;
        }
    } else {
        write_new_synced(&temp_path, &candidate.bytes, "marker-bound migration temp")?;
    }
    sync_directory(&log_dir)?;
    Ok(temp_path)
}

fn validate_receipt_static(
    receipt: &DbOnlyMigrationReceipt,
    root: &Path,
    engagement: &str,
) -> Result<()> {
    if receipt.schema != DB_ONLY_MIGRATION_RECEIPT_SCHEMA {
        return Err(RallyError::Usage(format!(
            "migration receipt has unsupported schema {:?}; evidence was preserved",
            receipt.schema
        )));
    }
    let expected_migration_id = migration_id(&receipt.marker.db, engagement);
    let expected_target = format!(".rally/{LOG_DIRNAME}/{engagement}.jsonl");
    let expected_temp =
        format!(".rally/{LOG_DIRNAME}/.db-only-migration.v1-{expected_migration_id}.segment.tmp");
    if receipt.marker.schema != DB_ONLY_MIGRATION_MARKER_SCHEMA
        || receipt.marker.canonical_repo_root != canonical_repo_root_string(root)
        || receipt.marker.engagement != engagement
        || receipt.marker.target_relative_path != expected_target
        || receipt.marker.temp_relative_path != expected_temp
        || receipt.marker.migration_id != expected_migration_id
        || receipt.target_len == 0
        || receipt.target_sha256.len() != 64
        || receipt.target_sha256 != receipt.marker.db.normalized_rows_sha256
        || receipt.target_len != receipt.marker.db.normalized_rows_len
        || receipt.marker.db.wal_len != 0
        || receipt.marker.db.journal_len != 0
        || receipt.marker.db.shm_present != receipt.marker.db.shm_sha256.is_some()
    {
        return Err(RallyError::Usage(
            "migration receipt binding mismatch for repo, engagement, target, or checksum; evidence was preserved"
                .to_string(),
        ));
    }
    chrono::DateTime::parse_from_rfc3339(&receipt.completed_at).map_err(|error| {
        RallyError::Usage(format!(
            "migration receipt has invalid completed_at {:?}: {error}; evidence was preserved",
            receipt.completed_at
        ))
    })?;
    chrono::DateTime::parse_from_rfc3339(&receipt.marker.created_at).map_err(|error| {
        RallyError::Usage(format!(
            "migration receipt has invalid marker created_at {:?}: {error}; evidence was preserved",
            receipt.marker.created_at
        ))
    })?;
    let temp = Path::new(&receipt.marker.temp_relative_path);
    let expected_parent = Path::new(".rally").join(LOG_DIRNAME);
    if temp.is_absolute()
        || temp.parent() != Some(expected_parent.as_path())
        || temp.file_name().and_then(|value| value.to_str()).is_none()
    {
        return Err(RallyError::Usage(
            "migration receipt contains an unsafe temp path; evidence was preserved".to_string(),
        ));
    }
    Ok(())
}

fn publish_receipt(
    rally_dir: &Path,
    marker: &DbOnlyMigrationMarker,
    candidate: &DbOnlyMigrationSegment,
) -> DbOnlyMigrationResult<DbOnlyMigrationReceipt> {
    let receipt_path = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
    let stage_path = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_STAGE_FILENAME);
    let expected = DbOnlyMigrationReceipt {
        schema: DB_ONLY_MIGRATION_RECEIPT_SCHEMA.to_string(),
        completed_at: now_string(),
        marker: marker.clone(),
        target_sha256: sha256_bytes(&candidate.bytes),
        target_len: u64::try_from(candidate.bytes.len())
            .map_err(|error| RallyError::Message(format!("target length overflow: {error}")))?,
    };
    if ensure_regular_or_missing(&receipt_path, "migration receipt")? {
        let receipt: DbOnlyMigrationReceipt = read_json_file(&receipt_path, "migration receipt")?;
        let mut expected_same_time = expected.clone();
        expected_same_time
            .completed_at
            .clone_from(&receipt.completed_at);
        if receipt != expected_same_time {
            return Err(RallyError::Usage(
                "existing migration receipt differs from the committed marker/target binding; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
        return Ok(receipt);
    }
    let receipt = if ensure_regular_or_missing(&stage_path, "migration receipt staging file")? {
        let staged: DbOnlyMigrationReceipt =
            read_json_file(&stage_path, "migration receipt staging file")?;
        let mut expected_same_time = expected.clone();
        expected_same_time
            .completed_at
            .clone_from(&staged.completed_at);
        if staged != expected_same_time {
            return Err(RallyError::Usage(
                "migration receipt staging file differs from the committed marker/target binding; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
        staged
    } else {
        let rendered = serde_json::to_vec_pretty(&expected)
            .map_err(RallyError::json("render migration receipt"))?;
        write_new_synced(&stage_path, &rendered, "migration receipt staging file")?;
        expected
    };
    let staged_bytes = safe_file_bytes(&stage_path, "migration receipt staging file")?;
    install_no_clobber(
        &stage_path,
        &receipt_path,
        &staged_bytes,
        "migration receipt",
    )?;
    sync_directory(rally_dir)?;
    Ok(receipt)
}

fn migration_report(
    state: DbOnlyMigrationState,
    applied: bool,
    apply_requires_revalidation: bool,
    root: &Path,
    marker: &DbOnlyMigrationMarker,
    owner_observation: String,
    observed_blockers: Vec<String>,
) -> Result<DbOnlyMigrationReport> {
    Ok(DbOnlyMigrationReport {
        state,
        applied,
        apply_requires_revalidation,
        repo_root: root.to_path_buf(),
        engagement: marker.engagement.clone(),
        migration_id: Some(marker.migration_id.clone()),
        source_token: Some(source_token(&marker.db)?),
        observed_blockers,
        owner_observation,
        target_path: root.join(&marker.target_relative_path),
        marker_path: root.join(".rally").join(DB_ONLY_MIGRATION_MARKER_FILENAME),
        receipt_path: root.join(".rally").join(DB_ONLY_MIGRATION_RECEIPT_FILENAME),
        row_count: Some(marker.db.row_count),
        max_seq: Some(marker.db.logical_max_seq),
        db_sha256: Some(marker.db.sha256.clone()),
        normalized_rows_sha256: Some(marker.db.normalized_rows_sha256.clone()),
        warnings: Vec::new(),
    })
}

fn blocked_dry_run_report(
    root: &Path,
    engagement: &str,
    owner_observation: String,
    observed_blockers: Vec<String>,
) -> DbOnlyMigrationReport {
    let rally_dir = root.join(".rally");
    DbOnlyMigrationReport {
        state: DbOnlyMigrationState::DryRun,
        applied: false,
        apply_requires_revalidation: true,
        repo_root: root.to_path_buf(),
        engagement: engagement.to_string(),
        migration_id: None,
        source_token: None,
        observed_blockers,
        owner_observation,
        target_path: rally_dir
            .join(LOG_DIRNAME)
            .join(format!("{engagement}.jsonl")),
        marker_path: rally_dir.join(DB_ONLY_MIGRATION_MARKER_FILENAME),
        receipt_path: rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME),
        row_count: None,
        max_seq: None,
        db_sha256: None,
        normalized_rows_sha256: None,
        warnings: Vec::new(),
    }
}

fn receipt_prefix_candidate(
    root: &Path,
    receipt: &DbOnlyMigrationReceipt,
) -> Result<DbOnlyMigrationSegment> {
    let target = root.join(&receipt.marker.target_relative_path);
    let bytes = safe_file_bytes(&target, "migration target")?;
    let prefix_len = usize::try_from(receipt.target_len)
        .map_err(|error| RallyError::Message(format!("receipt target length overflow: {error}")))?;
    if bytes.len() < prefix_len {
        return Err(RallyError::Usage(format!(
            "migration target {} is shorter than its immutable receipt; evidence was preserved",
            target.display()
        )));
    }
    let prefix = bytes[..prefix_len].to_vec();
    if sha256_bytes(&prefix) != receipt.target_sha256 || prefix.last() != Some(&b'\n') {
        return Err(RallyError::Usage(format!(
            "migration target {} diverges from its immutable receipt-bound prefix; evidence was preserved",
            target.display()
        )));
    }
    let candidate = DbOnlyMigrationSegment {
        bytes: prefix,
        row_count: receipt.marker.db.row_count,
        max_seq: receipt.marker.db.logical_max_seq,
    };
    verify_db_only_migration_extension(&target, &candidate, &receipt.marker.engagement)?;
    Ok(candidate)
}

fn remove_regular_file(path: &Path, label: &str) -> Result<()> {
    if !ensure_regular_or_missing(path, label)? {
        return Ok(());
    }
    fs::remove_file(path).map_err(RallyError::io(format!("remove {label} {}", path.display())))
}

fn same_file_identity(left: &Path, right: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let left_metadata = fs::symlink_metadata(left).map_err(RallyError::io(format!(
            "stat migration temp {}",
            left.display()
        )))?;
        let right_metadata = fs::symlink_metadata(right).map_err(RallyError::io(format!(
            "stat migration target {}",
            right.display()
        )))?;
        Ok(left_metadata.dev() == right_metadata.dev()
            && left_metadata.ino() == right_metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

fn verify_committed_cleanup_temp(
    root: &Path,
    receipt: &DbOnlyMigrationReceipt,
    candidate: &DbOnlyMigrationSegment,
    temp_path: &Path,
) -> Result<()> {
    let temp_bytes = safe_file_bytes(temp_path, "marker-bound migration temp")?;
    if temp_bytes == candidate.bytes {
        return verify_db_only_migration_segment(temp_path, candidate);
    }

    let target_path = root.join(&receipt.marker.target_relative_path);
    let aliases_target = same_file_identity(temp_path, &target_path)?;
    if !aliases_target {
        let target_bytes = safe_file_bytes(&target_path, "migration target")?;
        if temp_bytes != target_bytes {
            return Err(RallyError::Usage(format!(
                "marker-bound migration temp {} differs from both the immutable receipt prefix and committed target {}; evidence was preserved",
                temp_path.display(),
                target_path.display()
            )));
        }
    }

    verify_db_only_migration_extension(
        temp_path,
        candidate,
        &receipt.marker.engagement,
    )
    .map_err(|error| {
        RallyError::Usage(format!(
            "marker-bound migration temp {} is not a valid receipt-bound canonical extension: {error}; evidence was preserved",
            temp_path.display()
        ))
    })
}

fn cleanup_committed_migration(
    root: &Path,
    receipt: &DbOnlyMigrationReceipt,
    candidate: &DbOnlyMigrationSegment,
) -> DbOnlyMigrationResult<()> {
    let rally_dir = root.join(".rally");
    let log_dir = rally_dir.join(LOG_DIRNAME);
    let marker = &receipt.marker;
    let temp_path = root.join(&marker.temp_relative_path);
    if ensure_regular_or_missing(&temp_path, "marker-bound migration temp")? {
        verify_committed_cleanup_temp(root, receipt, candidate, &temp_path)?;
        remove_regular_file(&temp_path, "marker-bound migration temp").map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "temp-cleanup",
                error.to_string(),
            )
        })?;
        sync_directory(&log_dir).map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "temp-cleanup-sync",
                error.to_string(),
            )
        })?;
    }

    let marker_stage = rally_dir.join(DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME);
    if ensure_regular_or_missing(&marker_stage, "migration marker staging file")? {
        let staged: DbOnlyMigrationMarker =
            read_json_file(&marker_stage, "migration marker staging file")?;
        if staged != *marker {
            return Err(RallyError::Usage(
                "migration marker staging file differs during committed cleanup; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
        remove_regular_file(&marker_stage, "migration marker staging file").map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "marker-stage-cleanup",
                error.to_string(),
            )
        })?;
    }

    let receipt_stage = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_STAGE_FILENAME);
    if ensure_regular_or_missing(&receipt_stage, "migration receipt staging file")? {
        let staged: DbOnlyMigrationReceipt =
            read_json_file(&receipt_stage, "migration receipt staging file")?;
        if staged != *receipt {
            return Err(RallyError::Usage(
                "migration receipt staging file differs during committed cleanup; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
        remove_regular_file(&receipt_stage, "migration receipt staging file").map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "receipt-stage-cleanup",
                error.to_string(),
            )
        })?;
    }
    sync_directory(&rally_dir).map_err(|error| {
        interruption(
            marker,
            DbOnlyMigrationInterruptionState::CommittedCleanupPending,
            "staging-cleanup-sync",
            error.to_string(),
        )
    })?;

    let marker_path = rally_dir.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
    if ensure_regular_or_missing(&marker_path, "migration marker")? {
        let installed: DbOnlyMigrationMarker = read_json_file(&marker_path, "migration marker")?;
        if installed != *marker {
            return Err(RallyError::Usage(
                "installed marker differs during committed cleanup; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
        remove_regular_file(&marker_path, "migration marker").map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "marker-cleanup",
                error.to_string(),
            )
        })?;
        maybe_fault(
            &rally_dir,
            marker,
            DbOnlyMigrationFaultPoint::AfterMarkerRemoval,
            DbOnlyMigrationInterruptionState::CommittedCleanupPending,
            "after-marker-removal-before-parent-sync",
        )?;
        sync_directory(&rally_dir).map_err(|error| {
            interruption(
                marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "marker-cleanup-sync",
                error.to_string(),
            )
        })?;
    }
    Ok(())
}

fn recover_from_receipt(
    root: &Path,
    engagement: &str,
    apply: bool,
    owner_observation: String,
) -> DbOnlyMigrationResult<DbOnlyMigrationReport> {
    let rally_dir = root.join(".rally");
    let receipt_path = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
    let receipt: DbOnlyMigrationReceipt = read_json_file(&receipt_path, "migration receipt")?;
    validate_receipt_static(&receipt, root, engagement)?;
    if apply {
        mark_migration_watchdog(&receipt.marker, "receipt-recovery-validation");
    }
    let target = root.join(&receipt.marker.target_relative_path);
    validate_canonical_source_set(root, Some(&target))?;
    let candidate = receipt_prefix_candidate(root, &receipt)?;

    let marker_path = rally_dir.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
    if ensure_regular_or_missing(&marker_path, "migration marker")? {
        let marker: DbOnlyMigrationMarker = read_json_file(&marker_path, "migration marker")?;
        if marker != receipt.marker {
            return Err(RallyError::Usage(
                "migration marker differs from the immutable receipt; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
    }
    let receipt_stage = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_STAGE_FILENAME);
    if ensure_regular_or_missing(&receipt_stage, "migration receipt staging file")? {
        let staged: DbOnlyMigrationReceipt =
            read_json_file(&receipt_stage, "migration receipt staging file")?;
        if staged != receipt {
            return Err(RallyError::Usage(
                "migration receipt staging file differs from the immutable receipt; evidence was preserved"
                    .to_string(),
            )
            .into());
        }
    }

    if apply {
        let classify_committed = |error: RallyError, phase: &str| {
            interruption(
                &receipt.marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                phase,
                error.to_string(),
            )
        };
        let target_file = fs::File::open(&target)
            .map_err(RallyError::io(format!(
                "open migration target {}",
                target.display()
            )))
            .map_err(|error| classify_committed(error, "receipt-recovery-target-open"))?;
        target_file
            .sync_all()
            .map_err(RallyError::io(format!(
                "sync migration target {}",
                target.display()
            )))
            .map_err(|error| classify_committed(error, "receipt-recovery-target-sync"))?;
        sync_directory(&rally_dir.join(LOG_DIRNAME))
            .map_err(|error| classify_committed(error, "receipt-recovery-log-sync"))?;
        let receipt_file = fs::File::open(&receipt_path)
            .map_err(RallyError::io(format!(
                "open migration receipt {}",
                receipt_path.display()
            )))
            .map_err(|error| classify_committed(error, "receipt-recovery-receipt-open"))?;
        receipt_file
            .sync_all()
            .map_err(RallyError::io(format!(
                "sync migration receipt {}",
                receipt_path.display()
            )))
            .map_err(|error| classify_committed(error, "receipt-recovery-receipt-sync"))?;
        sync_directory(&rally_dir)
            .map_err(|error| classify_committed(error, "receipt-recovery-rally-sync"))?;
        mark_watchdog_command_commit();
        cleanup_committed_migration(root, &receipt, &candidate).map_err(|error| {
            classify_interruption(
                error,
                &receipt.marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "receipt-recovery-cleanup",
            )
        })?;
    }
    migration_report(
        DbOnlyMigrationState::AlreadyCommitted,
        apply,
        !apply,
        root,
        &receipt.marker,
        owner_observation,
        Vec::new(),
    )
    .map_err(Into::into)
}

fn install_migration_target(
    root: &Path,
    rally_dir: &Path,
    marker: &DbOnlyMigrationMarker,
    temp_path: &Path,
    candidate: &DbOnlyMigrationSegment,
) -> DbOnlyMigrationResult<PathBuf> {
    let target = root.join(&marker.target_relative_path);
    let existed = ensure_regular_or_missing(&target, "migration target")?;
    if existed {
        verify_db_only_migration_segment(&target, candidate)?;
    } else {
        mark_migration_watchdog(marker, "target-hard-link");
        match fs::hard_link(temp_path, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_db_only_migration_segment(&target, candidate)?;
            }
            Err(error) => {
                return Err(interruption(
                    marker,
                    DbOnlyMigrationInterruptionState::OutcomeUnknown,
                    "target-install",
                    format!(
                        "atomic no-clobber target publication returned {error}; inspect the marker-bound target before deciding whether to rerun"
                    ),
                ));
            }
        }
    }
    mark_migration_watchdog(marker, "target-file-sync");
    let target_file = fs::File::open(&target).map_err(|error| {
        interruption(
            marker,
            DbOnlyMigrationInterruptionState::OutcomeUnknown,
            "target-sync-open",
            error.to_string(),
        )
    })?;
    target_file.sync_all().map_err(|error| {
        interruption(
            marker,
            DbOnlyMigrationInterruptionState::OutcomeUnknown,
            "target-sync",
            error.to_string(),
        )
    })?;
    if !existed {
        maybe_fault(
            rally_dir,
            marker,
            DbOnlyMigrationFaultPoint::AfterTargetInstallBeforeDirectorySync,
            DbOnlyMigrationInterruptionState::OutcomeUnknown,
            "target-installed-before-directory-sync",
        )?;
    }
    mark_migration_watchdog(marker, "target-directory-sync");
    sync_directory(&rally_dir.join(LOG_DIRNAME)).map_err(|error| {
        interruption(
            marker,
            DbOnlyMigrationInterruptionState::OutcomeUnknown,
            "target-directory-sync",
            error.to_string(),
        )
    })?;
    mark_watchdog_command_commit();
    verify_db_only_migration_segment(&target, candidate).map_err(|error| {
        interruption(
            marker,
            DbOnlyMigrationInterruptionState::CommittedCleanupPending,
            "committed-target-readback",
            error.to_string(),
        )
    })?;
    maybe_fault(
        rally_dir,
        marker,
        DbOnlyMigrationFaultPoint::AfterTargetDirectorySync,
        DbOnlyMigrationInterruptionState::CommittedCleanupPending,
        "target-directory-synced",
    )?;
    Ok(target)
}

/// Explicit DB-only recovery entry point. Dry-run is intentionally optimistic
/// and byte-inert; apply acquires the full offline authority set and repeats
/// every source, sidecar, canonical, marker, and checksum check before writing.
pub(crate) fn run_db_only_migration_at(
    root: &Path,
    engagement: &str,
    apply: bool,
) -> DbOnlyMigrationResult<DbOnlyMigrationReport> {
    let engagement = validate_scoped_engagement(engagement)?;
    if is_reserved_fixture_engagement(&engagement) {
        return Err(RallyError::Usage(format!(
            "engagement label {engagement:?} is reserved for committed test fixtures and cannot receive migrated history"
        ))
        .into());
    }
    validate_migration_topology(root)?;
    let rally_dir = root.join(".rally");
    let owner_observation = observe_offline_migration_authority(&rally_dir)?;

    // Apply authority is deliberately acquired before any DB open/hash or
    // receipt cleanup. Dry-run performs only the optimistic observation above.
    let _authority = if apply {
        Some(acquire_offline_migration_authority(&rally_dir)?)
    } else {
        None
    };

    let receipt_path = rally_dir.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
    if ensure_regular_or_missing(&receipt_path, "migration receipt")? {
        return recover_from_receipt(
            root,
            &engagement,
            apply,
            if apply {
                "exclusive_offline_authority_acquired".to_string()
            } else {
                owner_observation
            },
        );
    }

    let marker_path = rally_dir.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
    let marker_exists = ensure_regular_or_missing(&marker_path, "migration marker")?;
    let marker_stage = rally_dir.join(DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME);
    let marker_stage_exists =
        ensure_regular_or_missing(&marker_stage, "migration marker staging file")?;
    if apply && (marker_exists || marker_stage_exists) {
        let recovery_marker: DbOnlyMigrationMarker = if marker_exists {
            read_json_file(&marker_path, "migration marker")?
        } else {
            read_json_file(&marker_stage, "migration marker staging file")?
        };
        mark_watchdog_db_only_migration_outcome_unknown(
            &recovery_marker.migration_id,
            "recovery-source-revalidation",
            &migration_remedy(&engagement),
        );
    }
    let target = rally_dir
        .join(LOG_DIRNAME)
        .join(format!("{engagement}.jsonl"));
    let canonical_blocker =
        match validate_canonical_source_set(root, marker_exists.then_some(target.as_path())) {
            Ok(()) => None,
            Err(error) if apply => return Err(error.into()),
            Err(error) => Some(error.to_string()),
        };

    let facts_db_path = rally_dir.join("facts.db");
    let (candidate, binding) = match inspect_closed_db_candidate(&facts_db_path, &engagement) {
        Ok(inspected) => inspected,
        Err(error) if !apply => {
            let mut blockers = Vec::new();
            if owner_observation != "clear_at_optimistic_inspection" {
                blockers.push(owner_observation.clone());
            }
            if let Some(canonical) = canonical_blocker {
                blockers.push(canonical);
            }
            blockers.push(error.to_string());
            return Ok(blocked_dry_run_report(
                root,
                &engagement,
                owner_observation,
                blockers,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let expected = expected_marker(root, &engagement, binding);

    if !apply {
        let marker = if marker_exists {
            let marker: DbOnlyMigrationMarker = read_json_file(&marker_path, "migration marker")?;
            validate_marker(&marker, &expected)?;
            marker
        } else if marker_stage_exists {
            let marker: DbOnlyMigrationMarker =
                read_json_file(&marker_stage, "migration marker staging file")?;
            validate_marker(&marker, &expected)?;
            marker
        } else {
            expected
        };
        return migration_report(
            DbOnlyMigrationState::DryRun,
            false,
            true,
            root,
            &marker,
            owner_observation.clone(),
            {
                let mut blockers = Vec::new();
                if owner_observation != "clear_at_optimistic_inspection" {
                    blockers.push(owner_observation);
                }
                if let Some(canonical) = canonical_blocker {
                    blockers.push(canonical);
                }
                blockers
            },
        )
        .map_err(Into::into);
    }

    if !marker_exists && !marker_stage_exists {
        #[cfg(test)]
        if take_db_only_migration_fault(
            &rally_dir,
            DbOnlyMigrationFaultPoint::ExpireDeadlineBeforeMarker,
        ) {
            crate::store::expire_mutation_deadline_for_test();
        }
        ensure_new_mutation_can_start(&marker_path)?;
    }
    mark_migration_watchdog(&expected, "marker-publication");
    let marker = load_or_publish_marker(&rally_dir, &expected).map_err(|error| {
        classify_interruption(
            error,
            &expected,
            DbOnlyMigrationInterruptionState::Prepared,
            "marker-publication",
        )
    })?;
    maybe_fault(
        &rally_dir,
        &marker,
        DbOnlyMigrationFaultPoint::AfterMarkerSync,
        DbOnlyMigrationInterruptionState::Prepared,
        "marker-synced",
    )?;
    let marker_target = root.join(&marker.target_relative_path);
    validate_canonical_source_set(root, Some(&marker_target)).map_err(|error| {
        interruption(
            &marker,
            DbOnlyMigrationInterruptionState::Prepared,
            "post-marker-canonical-validation",
            error.to_string(),
        )
    })?;

    let marker_target_exists = ensure_regular_or_missing(&marker_target, "migration target")
        .map_err(|error| {
            interruption(
                &marker,
                DbOnlyMigrationInterruptionState::Prepared,
                "target-preflight",
                error.to_string(),
            )
        })?;
    if marker_target_exists {
        verify_db_only_migration_segment(&marker_target, &candidate).map_err(|error| {
            interruption(
                &marker,
                DbOnlyMigrationInterruptionState::Prepared,
                "target-preflight",
                error.to_string(),
            )
        })?;
    }
    mark_migration_watchdog(&marker, "temp-preparation");
    let temp_path =
        ensure_candidate_temp(root, &rally_dir, &marker, &candidate, !marker_target_exists)
            .map_err(|error| match error {
                DbOnlyMigrationRunError::Interrupted(_) => error,
                DbOnlyMigrationRunError::Other(error) => interruption(
                    &marker,
                    DbOnlyMigrationInterruptionState::Prepared,
                    "temp-prepare",
                    error.to_string(),
                ),
            })?;
    maybe_fault(
        &rally_dir,
        &marker,
        DbOnlyMigrationFaultPoint::AfterTempSync,
        DbOnlyMigrationInterruptionState::Prepared,
        "temp-synced",
    )?;
    verify_db_only_migration_segment(&temp_path, &candidate).map_err(|error| {
        interruption(
            &marker,
            DbOnlyMigrationInterruptionState::Prepared,
            "temp-readback",
            error.to_string(),
        )
    })?;
    maybe_fault(
        &rally_dir,
        &marker,
        DbOnlyMigrationFaultPoint::AfterTempReadback,
        DbOnlyMigrationInterruptionState::Prepared,
        "temp-readback",
    )?;

    mark_migration_watchdog(&marker, "db-revalidation");
    let (revalidated_candidate, revalidated_binding) =
        inspect_closed_db_candidate(&facts_db_path, &engagement).map_err(|error| {
            interruption(
                &marker,
                DbOnlyMigrationInterruptionState::Prepared,
                "db-revalidation",
                error.to_string(),
            )
        })?;
    if revalidated_binding != marker.db || revalidated_candidate != candidate {
        return Err(interruption(
            &marker,
            DbOnlyMigrationInterruptionState::Prepared,
            "db-revalidation-mismatch",
            "facts.db binding mismatch after temp preparation; marker, temp, DB, and sidecars were preserved",
        ));
    }
    maybe_fault(
        &rally_dir,
        &marker,
        DbOnlyMigrationFaultPoint::AfterDbRevalidation,
        DbOnlyMigrationInterruptionState::Prepared,
        "db-revalidated",
    )?;

    mark_migration_watchdog(&marker, "target-publication");
    install_migration_target(root, &rally_dir, &marker, &temp_path, &candidate).map_err(
        |error| {
            classify_interruption(
                error,
                &marker,
                DbOnlyMigrationInterruptionState::Prepared,
                "target-publication",
            )
        },
    )?;
    mark_watchdog_command_commit();
    let receipt =
        publish_receipt(&rally_dir, &marker, &candidate).map_err(|error| match error {
            DbOnlyMigrationRunError::Interrupted(_) => error,
            DbOnlyMigrationRunError::Other(error) => interruption(
                &marker,
                DbOnlyMigrationInterruptionState::CommittedCleanupPending,
                "receipt-publication",
                error.to_string(),
            ),
        })?;
    maybe_fault(
        &rally_dir,
        &marker,
        DbOnlyMigrationFaultPoint::AfterReceiptSync,
        DbOnlyMigrationInterruptionState::CommittedCleanupPending,
        "receipt-synced",
    )?;
    cleanup_committed_migration(root, &receipt, &candidate).map_err(|error| {
        classify_interruption(
            error,
            &marker,
            DbOnlyMigrationInterruptionState::CommittedCleanupPending,
            "committed-cleanup",
        )
    })?;
    migration_report(
        DbOnlyMigrationState::Committed,
        true,
        false,
        root,
        &marker,
        "exclusive_offline_authority_acquired".to_string(),
        Vec::new(),
    )
    .map_err(Into::into)
}

pub(crate) fn run_db_only_migration(
    engagement: &str,
    apply: bool,
) -> DbOnlyMigrationResult<DbOnlyMigrationReport> {
    let root = crate::repo_root().map_err(DbOnlyMigrationRunError::from)?;
    run_db_only_migration_at(&root, engagement, apply)
}

// =============================================================================
// sweep-corrupt logic
// =============================================================================

/// One quarantined `facts.db.corrupt.<stamp>` snapshot plus any `-db-shm` /
/// `-db-wal` siblings sharing the same stamp.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CorruptSnapshot {
    /// The stamp infix (nanoseconds-since-epoch, as written by the store).
    pub(crate) stamp: String,
    /// Age in days derived from the stamp vs. now. Negative clamps to 0.
    pub(crate) age_days: f64,
    /// Total bytes across the base file and its siblings.
    pub(crate) bytes: u64,
    /// File names in this snapshot group (base + siblings), sorted.
    pub(crate) files: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SweepCorruptReport {
    /// The `.rally` store directory that was scanned.
    pub(crate) rally_dir: PathBuf,
    /// Snapshots retained (newest --keep, or newer than --max-age-days).
    pub(crate) kept: Vec<CorruptSnapshot>,
    /// Snapshots swept — **moved** into `archived_dir` with --apply, listed only
    /// in dry-run. Never deleted: see `archived_dir`.
    pub(crate) swept: Vec<CorruptSnapshot>,
    /// Where swept snapshots are moved to. Sweeping ARCHIVES; it never deletes,
    /// so the forensic record of a past corruption survives the cleanup that
    /// followed it. Removing anything from here is a human decision.
    pub(crate) archived_dir: PathBuf,
    /// Bytes moved out of the live store (--apply) or movable (dry-run). Not
    /// "reclaimed" — the bytes still exist under `archived_dir`.
    pub(crate) bytes_reclaimable: u64,
    /// Whether files were actually moved (`--apply`).
    pub(crate) applied: bool,
    pub(crate) keep: i64,
    pub(crate) max_age_days: i64,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// Parse the stamp infix out of a `facts.db.corrupt.<stamp>[-db-shm|-db-wal]`
/// file name. Returns `None` for non-matching names.
fn snapshot_stamp(name: &str) -> Option<String> {
    let rest = name.strip_prefix(CORRUPT_PREFIX)?;
    // Sibling suffixes are `-db-shm` / `-db-wal`; the stamp is the leading run
    // of digits before any `-`.
    let stamp: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if stamp.is_empty() { None } else { Some(stamp) }
}

/// Pure core: classify corrupt snapshots in `dir` under the retention policy.
/// `now_ns` is injected so tests are deterministic. Never removes files unless
/// `apply` is true. Missing dir → empty report (nothing to sweep).
pub(crate) fn sweep_corrupt_in_dir(
    dir: &Path,
    keep: i64,
    max_age_days: i64,
    apply: bool,
    now_ns: u128,
) -> SweepCorruptReport {
    use std::collections::BTreeMap;

    let keep = keep.max(0);
    let max_age_days = max_age_days.max(0);
    let mut warnings: Vec<DiscoveryWarning> = Vec::new();

    // Group files by stamp.
    let mut groups: BTreeMap<String, (u64, Vec<String>)> = BTreeMap::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(stamp) = snapshot_stamp(&name) else {
                    continue;
                };
                let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let slot = groups.entry(stamp).or_insert_with(|| (0, Vec::new()));
                slot.0 += bytes;
                slot.1.push(name);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warnings.push(DiscoveryWarning {
                code: "sweep_read_dir_failed".to_string(),
                message: format!("cannot read {}: {e}", dir.display()),
                path: Some(dir.to_path_buf()),
                count: None,
            });
        }
    }

    // Build snapshots, newest-first.
    let mut snapshots: Vec<CorruptSnapshot> = groups
        .into_iter()
        .map(|(stamp, (bytes, mut files))| {
            files.sort();
            let stamp_ns: u128 = stamp.parse().unwrap_or(0);
            let age_days = if now_ns > stamp_ns {
                (now_ns - stamp_ns) as f64 / NS_PER_DAY
            } else {
                0.0
            };
            CorruptSnapshot {
                stamp,
                age_days,
                bytes,
                files,
            }
        })
        .collect();
    // Sort by stamp descending (newest first). Parse each stamp once.
    snapshots.sort_by(|a, b| {
        let sa: u128 = a.stamp.parse().unwrap_or(0);
        let sb: u128 = b.stamp.parse().unwrap_or(0);
        sb.cmp(&sa)
    });

    let mut kept: Vec<CorruptSnapshot> = Vec::new();
    let mut swept: Vec<CorruptSnapshot> = Vec::new();
    for (rank, snap) in snapshots.into_iter().enumerate() {
        let keep_by_rank = (rank as i64) < keep;
        let keep_by_age = snap.age_days < max_age_days as f64;
        if keep_by_rank || keep_by_age {
            kept.push(snap);
        } else {
            swept.push(snap);
        }
    }

    let bytes_reclaimable: u64 = swept.iter().map(|s| s.bytes).sum();

    // ARCHIVE, NEVER DELETE. A quarantined `facts.db.corrupt.*` snapshot is the
    // forensic record of a corruption event — the single most valuable artifact
    // to still have when diagnosing why a store broke. Sweeping used to
    // `fs::remove_file` it, which destroyed that evidence as part of "cleanup".
    // Doctor now moves it under `.rally/archive/swept/<stamp>/`; deciding it is
    // finally disposable is a human call, made somewhere else.
    let archived_dir = dir.join(ARCHIVE_DIRNAME).join(SWEPT_SUBDIR);
    if apply {
        for snap in &swept {
            let dest_dir = archived_dir.join(&snap.stamp);
            if let Err(e) = fs::create_dir_all(&dest_dir) {
                warnings.push(DiscoveryWarning {
                    code: "sweep_archive_mkdir_failed".to_string(),
                    message: format!("cannot create {}: {e}", dest_dir.display()),
                    path: Some(dest_dir.clone()),
                    count: None,
                });
                continue;
            }
            for name in &snap.files {
                let src = dir.join(name);
                let dest = dest_dir.join(name);
                if !src.exists() {
                    continue;
                }
                // Same filesystem in the normal case; fall back to copy+remove
                // only when rename cannot cross the boundary, and only after the
                // copy has succeeded, so no path loses the file.
                let moved = match fs::rename(&src, &dest) {
                    Ok(()) => true,
                    Err(_) => match fs::copy(&src, &dest) {
                        Ok(_) => fs::remove_file(&src).is_ok(),
                        Err(e) => {
                            warnings.push(DiscoveryWarning {
                                code: "sweep_archive_failed".to_string(),
                                message: format!(
                                    "cannot archive {} to {}: {e}",
                                    src.display(),
                                    dest.display()
                                ),
                                path: Some(src.clone()),
                                count: None,
                            });
                            false
                        }
                    },
                };
                if !moved && src.exists() {
                    warnings.push(DiscoveryWarning {
                        code: "sweep_archive_incomplete".to_string(),
                        message: format!(
                            "{} was left in place; nothing was deleted",
                            src.display()
                        ),
                        path: Some(src.clone()),
                        count: None,
                    });
                }
            }
        }
    }

    SweepCorruptReport {
        rally_dir: dir.to_path_buf(),
        kept,
        swept,
        archived_dir,
        bytes_reclaimable,
        applied: apply,
        keep,
        max_age_days,
        warnings,
    }
}

// =============================================================================
// ledger-health logic
// =============================================================================
//
// The mode that has to work when nothing else does. It reads the JSONL segments
// as raw text, pulls only `seq` out of each line, and never opens the store or
// the derived DB — so a ledger that fails canonical validation, a half-written
// segment, or a missing facts.db all still produce a report instead of the same
// hard error every other command returns.
//
// Read-only, always. Repair lives in `run_repair_ledger`, is dry-run by default,
// and archives before it writes.

/// One finding about the ledger, with the command that addresses it.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LedgerFinding {
    pub(crate) code: String,
    /// `error` blocks every store-opening command; `warn` is degraded but usable;
    /// `info` is context.
    pub(crate) severity: String,
    pub(crate) message: String,
    /// The exact command that fixes this, when one exists.
    pub(crate) remedy: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LedgerSegmentHealth {
    pub(crate) path: PathBuf,
    pub(crate) rows: usize,
    /// 1-based line numbers that are not valid JSON.
    pub(crate) unparseable_lines: Vec<usize>,
    /// 1-based line numbers whose object carries no integer `seq`.
    pub(crate) missing_seq_lines: Vec<usize>,
    pub(crate) duplicate_seqs: Vec<i64>,
    pub(crate) min_seq: Option<i64>,
    pub(crate) max_seq: Option<i64>,
    /// 1-based line numbers where seq goes backwards relative to the line before.
    pub(crate) out_of_order_lines: Vec<usize>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct LedgerHealthReport {
    pub(crate) rally_dir: PathBuf,
    pub(crate) rally_dir_exists: bool,
    pub(crate) segments: Vec<LedgerSegmentHealth>,
    /// Seqs claimed by more than one row across all segments — the class that
    /// makes `canonical_segment_entries` refuse, taking every command with it.
    pub(crate) conflicting_seqs: Vec<i64>,
    pub(crate) derived_db_present: bool,
    pub(crate) quarantined_snapshots: usize,
    pub(crate) findings: Vec<LedgerFinding>,
    /// True when nothing at `error` severity was found.
    pub(crate) healthy: bool,
    /// True when `--repair-ledger` can mechanically resolve what was found.
    pub(crate) repairable: bool,
}

fn segment_paths_raw(rally_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for sub in [LOG_DIRNAME, ARCHIVE_DIRNAME] {
        let dir = rally_dir.join(sub);
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    let monolith = rally_dir.join(LEDGER_FILENAME);
    if monolith.is_file() {
        out.push(monolith);
    }
    out.sort();
    out
}

/// Pure core so tests can point it at a fixture directory.
pub(crate) fn ledger_health_in_dir(rally_dir: &Path) -> LedgerHealthReport {
    let mut findings: Vec<LedgerFinding> = Vec::new();
    let rally_dir_exists = rally_dir.is_dir();
    let mut segments: Vec<LedgerSegmentHealth> = Vec::new();

    if !rally_dir_exists {
        findings.push(LedgerFinding {
            code: "no_rally_dir".to_string(),
            severity: "info".to_string(),
            message: format!("{} does not exist — this repo has no room", rally_dir.display()),
            remedy: Some("rally init".to_string()),
        });
    }

    for path in segment_paths_raw(rally_dir) {
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                findings.push(LedgerFinding {
                    code: "segment_unreadable".to_string(),
                    severity: "error".to_string(),
                    message: format!("cannot read {}: {e}", path.display()),
                    remedy: None,
                });
                continue;
            }
        };
        let mut health = LedgerSegmentHealth {
            path: path.clone(),
            rows: 0,
            unparseable_lines: Vec::new(),
            missing_seq_lines: Vec::new(),
            duplicate_seqs: Vec::new(),
            min_seq: None,
            max_seq: None,
            out_of_order_lines: Vec::new(),
        };
        let mut seen_in_segment: SeqRows = SeqRows::new();
        let mut prev: Option<i64> = None;
        for (idx, line) in text.lines().enumerate() {
            let lineno = idx + 1;
            if line.trim().is_empty() {
                continue;
            }
            health.rows += 1;
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    health.unparseable_lines.push(lineno);
                    continue;
                }
            };
            let Some(seq) = value.get("seq").and_then(|s| s.as_i64()) else {
                health.missing_seq_lines.push(lineno);
                continue;
            };
            health.min_seq = Some(health.min_seq.map_or(seq, |m: i64| m.min(seq)));
            health.max_seq = Some(health.max_seq.map_or(seq, |m: i64| m.max(seq)));
            if prev.is_some_and(|p| seq < p) {
                health.out_of_order_lines.push(lineno);
            }
            prev = Some(seq);
            if seen_in_segment.insert(seq, line.to_string()).is_some() {
                health.duplicate_seqs.push(seq);
            }
        }
        health.duplicate_seqs.sort_unstable();
        health.duplicate_seqs.dedup();
        segments.push(health);
    }

    // Recompute conflicts across every segment with full-line equality, matching
    // the store's own rule: a repeated seq is fine only if the rows are identical.
    let mut conflicting: Vec<i64> = Vec::new();
    let mut by_seq: SeqRows = SeqRows::new();
    for path in segment_paths_raw(rally_dir) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(seq) = value.get("seq").and_then(|s| s.as_i64()) else {
                continue;
            };
            match by_seq.get(&seq) {
                Some(existing) if existing != line => conflicting.push(seq),
                Some(_) => {}
                None => {
                    by_seq.insert(seq, line.to_string());
                }
            }
        }
    }
    conflicting.sort_unstable();
    conflicting.dedup();

    if !conflicting.is_empty() {
        findings.push(LedgerFinding {
            code: "conflicting_seqs".to_string(),
            severity: "error".to_string(),
            message: format!(
                "{} sequence number(s) carry two different rows: {}. Canonical folding \
                 refuses to pick a winner, so EVERY store-opening command fails with the \
                 same error until this is resolved.",
                conflicting.len(),
                conflicting
                    .iter()
                    .take(8)
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            remedy: Some("rally doctor --repair-ledger        # then --apply".to_string()),
        });
    }

    let unparseable: usize = segments.iter().map(|s| s.unparseable_lines.len()).sum();
    if unparseable > 0 {
        findings.push(LedgerFinding {
            code: "unparseable_lines".to_string(),
            severity: "error".to_string(),
            message: format!("{unparseable} line(s) are not valid JSON"),
            remedy: Some(
                "inspect the reported lines; repair does not rewrite unparseable rows".to_string(),
            ),
        });
    }
    let out_of_order: usize = segments.iter().map(|s| s.out_of_order_lines.len()).sum();
    if out_of_order > 0 {
        findings.push(LedgerFinding {
            code: "out_of_order_rows".to_string(),
            severity: "warn".to_string(),
            message: format!("{out_of_order} row(s) have a seq lower than the row before them"),
            remedy: Some("rally doctor --repair-ledger        # renumbers in file order".to_string()),
        });
    }

    let derived_db_present = rally_dir.join("facts.db").is_file();
    let quarantined = fs::read_dir(rally_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(CORRUPT_PREFIX)
                })
                .count()
        })
        .unwrap_or(0);
    if quarantined > 0 {
        findings.push(LedgerFinding {
            code: "quarantined_snapshots".to_string(),
            severity: "info".to_string(),
            message: format!(
                "{quarantined} quarantined facts.db.corrupt.* snapshot(s) — evidence of an \
                 earlier corruption, kept on purpose"
            ),
            remedy: Some(
                "rally doctor --sweep-corrupt --apply   # archives them, never deletes".to_string(),
            ),
        });
    }
    if !derived_db_present && rally_dir_exists {
        findings.push(LedgerFinding {
            code: "no_derived_db".to_string(),
            severity: "info".to_string(),
            message: "facts.db is absent; it is a disposable cache and will be rebuilt \
                      from the JSONL ledger on the next successful open"
                .to_string(),
            remedy: None,
        });
    }

    let healthy = !findings.iter().any(|f| f.severity == "error");
    let repairable = !conflicting.is_empty() || out_of_order > 0;
    LedgerHealthReport {
        rally_dir: rally_dir.to_path_buf(),
        rally_dir_exists,
        segments,
        conflicting_seqs: conflicting,
        derived_db_present,
        quarantined_snapshots: quarantined,
        findings,
        healthy,
        repairable,
    }
}

pub(crate) fn run_ledger_health() -> Result<LedgerHealthReport> {
    Ok(ledger_health_in_dir(&repo_root()?.join(".rally")))
}

// =============================================================================
// repair-ledger logic
// =============================================================================

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RepairedSegment {
    pub(crate) path: PathBuf,
    pub(crate) rows: usize,
    pub(crate) rows_renumbered: usize,
    pub(crate) first_change_line: Option<usize>,
    pub(crate) archived_to: Option<PathBuf>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RepairLedgerReport {
    pub(crate) rally_dir: PathBuf,
    pub(crate) segments: Vec<RepairedSegment>,
    pub(crate) rows_renumbered: usize,
    pub(crate) applied: bool,
    /// Where the untouched originals were copied before any rewrite.
    pub(crate) archive_dir: PathBuf,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// Assign strictly increasing, unique seqs across every live segment in file
/// order, preserving each row's content otherwise.
///
/// Order is what carries meaning here, not the absolute numbers: seq is a log
/// ordinal, and cursors that point past a shifted row re-read at most the rows
/// that moved. Every original is copied into `.rally/archive/pre-repair/<stamp>/`
/// BEFORE the first byte is written, and nothing is ever deleted.
pub(crate) fn repair_ledger_in_dir(
    rally_dir: &Path,
    apply: bool,
    stamp: &str,
) -> Result<RepairLedgerReport> {
    let archive_dir = rally_dir
        .join(ARCHIVE_DIRNAME)
        .join("pre-repair")
        .join(stamp);
    let mut warnings: Vec<DiscoveryWarning> = Vec::new();
    let mut segments: Vec<RepairedSegment> = Vec::new();
    let mut total_renumbered = 0usize;

    // Only the live log directory is renumbered. Archived segments are history
    // and are never rewritten.
    let live_dir = rally_dir.join(LOG_DIRNAME);
    let mut paths: Vec<PathBuf> = fs::read_dir(&live_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    let mut next_seq: i64 = 1;
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(RallyError::io(format!("read segment {}", path.display())))?;
        let mut rows = 0usize;
        let mut renumbered = 0usize;
        let mut first_change: Option<usize> = None;
        let mut out_lines: Vec<String> = Vec::new();
        let mut unparseable = false;

        for (idx, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            rows += 1;
            let mut value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    // Never rewrite a line we cannot parse — copy it through
                    // untouched and refuse to renumber this segment.
                    unparseable = true;
                    out_lines.push(line.to_string());
                    continue;
                }
            };
            let current = value.get("seq").and_then(|s| s.as_i64());
            if current != Some(next_seq)
                && let Some(obj) = value.as_object_mut()
            {
                obj.insert("seq".to_string(), serde_json::json!(next_seq));
                renumbered += 1;
                if first_change.is_none() {
                    first_change = Some(idx + 1);
                }
            }
            next_seq += 1;
            out_lines.push(
                serde_json::to_string(&value)
                    .map_err(|e| RallyError::Message(format!("reserialize row: {e}")))?,
            );
        }

        if unparseable {
            warnings.push(DiscoveryWarning {
                code: "repair_skipped_unparseable".to_string(),
                message: format!(
                    "{} contains unparseable line(s); left completely untouched",
                    path.display()
                ),
                path: Some(path.clone()),
                count: None,
            });
            segments.push(RepairedSegment {
                path,
                rows,
                rows_renumbered: 0,
                first_change_line: None,
                archived_to: None,
            });
            continue;
        }

        let mut archived_to = None;
        if apply && renumbered > 0 {
            // Archive BEFORE writing. If this fails, the rewrite does not happen.
            fs::create_dir_all(&archive_dir).map_err(RallyError::io(format!(
                "create archive dir {}",
                archive_dir.display()
            )))?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "segment.jsonl".to_string());
            let dest = archive_dir.join(&name);
            fs::copy(&path, &dest).map_err(RallyError::io(format!(
                "archive {} to {}",
                path.display(),
                dest.display()
            )))?;
            archived_to = Some(dest);

            let body = format!("{}\n", out_lines.join("\n"));
            let tmp = path.with_extension("jsonl.repair-tmp");
            fs::write(&tmp, &body)
                .map_err(RallyError::io(format!("write {}", tmp.display())))?;
            fs::rename(&tmp, &path)
                .map_err(RallyError::io(format!("replace {}", path.display())))?;
        }

        total_renumbered += renumbered;
        segments.push(RepairedSegment {
            path,
            rows,
            rows_renumbered: renumbered,
            first_change_line: first_change,
            archived_to,
        });
    }

    Ok(RepairLedgerReport {
        rally_dir: rally_dir.to_path_buf(),
        segments,
        rows_renumbered: total_renumbered,
        applied: apply,
        archive_dir,
        warnings,
    })
}

pub(crate) fn run_repair_ledger(apply: bool) -> Result<RepairLedgerReport> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .to_string();
    repair_ledger_in_dir(&repo_root()?.join(".rally"), apply, &stamp)
}

/// `rally doctor --sweep-corrupt`: resolve the current room's `.rally` dir and
/// sweep disposable `facts.db.corrupt.*` snapshots under the retention policy.
pub(crate) fn run_sweep_corrupt(
    keep: Option<i64>,
    max_age_days: Option<i64>,
    apply: bool,
) -> Result<SweepCorruptReport> {
    // Deliberately NOT `RoomStore::open()`. This mode exists for a store that is
    // too damaged to open, and it opened one only to learn `repo_root/.rally` —
    // a path it can compute directly. That single call made the repair tool fail
    // on precisely the input it exists to repair: a ledger with two rows at the
    // same seq took down `--sweep-corrupt` with the same error as `rally room`.
    let dir = repo_root()?.join(".rally");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(sweep_corrupt_in_dir(
        &dir,
        keep.unwrap_or(SWEEP_DEFAULT_KEEP),
        max_age_days.unwrap_or(SWEEP_DEFAULT_MAX_AGE_DAYS),
        apply,
        now_ns,
    ))
}

// =============================================================================
// compact-log logic
// =============================================================================

/// A run of 2+ consecutive presence/heartbeat log lines, collapsed into one
/// summarized entry instead of repeating each heartbeat.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct PresenceRun {
    pub(crate) first_seq: i64,
    pub(crate) last_seq: i64,
    pub(crate) first_at: String,
    pub(crate) last_at: String,
    /// Total heartbeat lines absorbed into this run.
    pub(crate) count: usize,
    /// Heartbeat count per tool within the run (sorted by tool id).
    pub(crate) tools: std::collections::BTreeMap<String, usize>,
}

/// A log line passed through individually (non-presence, or a lone heartbeat
/// with no neighbor to collapse into).
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CompactLogEvent {
    pub(crate) seq: i64,
    pub(crate) occurred_at: String,
    pub(crate) event_type: String,
    /// Convenience extraction of `payload.tool` for rendering.
    pub(crate) tool: Option<String>,
    /// Convenience extraction of `payload.subject` for rendering.
    pub(crate) subject: Option<String>,
    /// The full fact payload, passed through unchanged — summary, target,
    /// evidence, scope, ref, and any future fields survive compaction.
    /// Only presence lines absorbed into a [`PresenceRun`] are summarized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<serde_json::Value>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub(crate) enum CompactLogEntry {
    PresenceRun(PresenceRun),
    Event(CompactLogEvent),
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CompactLogReport {
    /// The segment file that was read.
    pub(crate) log_file: PathBuf,
    pub(crate) total_lines: usize,
    /// Lines whose event_type is presence (all of them, collapsed or not).
    pub(crate) presence_lines: usize,
    /// Number of collapsed runs (each absorbing 2+ presence lines).
    pub(crate) presence_runs: usize,
    /// Lines removed from the rendering by collapsing (absorbed − run summaries).
    pub(crate) lines_saved: usize,
    pub(crate) unparseable_lines: usize,
    pub(crate) entries: Vec<CompactLogEntry>,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// Event types that count as heartbeat traffic. Presence facts are the
/// liveness heartbeat (see liveness.rs: "Heartbeat / presence last_seen").
fn is_heartbeat(event_type: &str) -> bool {
    event_type == "presence"
}

/// Flush a pending run of consecutive presence lines into `entries`.
/// A single buffered heartbeat passes through as a normal event — only
/// repetition collapses.
fn flush_presence_run(pending: &mut Vec<CompactLogEvent>, entries: &mut Vec<CompactLogEntry>) {
    match pending.len() {
        0 => {}
        1 => entries.push(CompactLogEntry::Event(pending.remove(0))),
        _ => {
            let mut tools = std::collections::BTreeMap::new();
            for ev in pending.iter() {
                *tools
                    .entry(ev.tool.clone().unwrap_or_else(|| "unknown".to_string()))
                    .or_insert(0) += 1;
            }
            let first = &pending[0];
            let last = &pending[pending.len() - 1];
            entries.push(CompactLogEntry::PresenceRun(PresenceRun {
                first_seq: first.seq,
                last_seq: last.seq,
                first_at: first.occurred_at.clone(),
                last_at: last.occurred_at.clone(),
                count: pending.len(),
                tools,
            }));
            pending.clear();
        }
    }
}

/// Pure core: compact one segment's jsonl contents. Consecutive presence
/// (heartbeat) lines collapse into a [`PresenceRun`]; everything else passes
/// through in order. Unparseable lines are counted, never fatal.
pub(crate) fn compact_log_content(path: &Path, contents: &str) -> CompactLogReport {
    let mut entries: Vec<CompactLogEntry> = Vec::new();
    let mut pending: Vec<CompactLogEvent> = Vec::new();
    let mut total_lines = 0usize;
    let mut presence_lines = 0usize;
    let mut unparseable_lines = 0usize;

    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total_lines += 1;
        let parsed: Option<serde_json::Value> = serde_json::from_str(line).ok();
        let Some(v) = parsed else {
            // Corruption breaks consecutiveness: heartbeats on either side of
            // an unparseable line are NOT consecutive and must not merge.
            flush_presence_run(&mut pending, &mut entries);
            unparseable_lines += 1;
            continue;
        };
        let Some(event_type) = v.get("event_type").and_then(|e| e.as_str()) else {
            flush_presence_run(&mut pending, &mut entries);
            unparseable_lines += 1;
            continue;
        };
        let payload = v.get("payload");
        let ev = CompactLogEvent {
            seq: v.get("seq").and_then(|s| s.as_i64()).unwrap_or(0),
            occurred_at: v
                .get("occurred_at")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            event_type: event_type.to_string(),
            tool: payload
                .and_then(|p| p.get("tool"))
                .and_then(|t| t.as_str())
                .map(str::to_string),
            subject: payload
                .and_then(|p| p.get("subject"))
                .and_then(|s| s.as_str())
                .map(str::to_string),
            payload: payload.cloned(),
        };
        if is_heartbeat(event_type) {
            presence_lines += 1;
            pending.push(ev);
        } else {
            flush_presence_run(&mut pending, &mut entries);
            entries.push(CompactLogEntry::Event(ev));
        }
    }
    flush_presence_run(&mut pending, &mut entries);

    let presence_runs = entries
        .iter()
        .filter(|e| matches!(e, CompactLogEntry::PresenceRun(_)))
        .count();
    let absorbed: usize = entries
        .iter()
        .filter_map(|e| match e {
            CompactLogEntry::PresenceRun(r) => Some(r.count),
            CompactLogEntry::Event(_) => None,
        })
        .sum();
    let lines_saved = absorbed.saturating_sub(presence_runs);

    CompactLogReport {
        log_file: path.to_path_buf(),
        total_lines,
        presence_lines,
        presence_runs,
        lines_saved,
        unparseable_lines,
        entries,
        warnings: Vec::new(),
    }
}

/// `rally doctor --compact-log [--log-file PATH]`: read a diagnostic log
/// segment (default: the current room's active segment) and return it with
/// heartbeat runs collapsed. Read-only — the segment file is never modified.
pub(crate) fn run_compact_log(log_file: Option<String>) -> Result<CompactLogReport> {
    let path = match log_file {
        Some(p) => PathBuf::from(p),
        None => RoomStore::open()?.active_segment_path(),
    };
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(compact_log_content(&path, &contents)),
        Err(e) => {
            let mut report = compact_log_content(&path, "");
            report.warnings.push(DiscoveryWarning {
                code: "compact_log_read_failed".to_string(),
                message: format!("cannot read {}: {e}", path.display()),
                path: Some(path),
                count: None,
            });
            Ok(report)
        }
    }
}

// =============================================================================
// prune-rooms logic
// =============================================================================

/// Classify a `KnownRoom` as live or stale.
///
/// Conservative definition of stale: the `repo_root` directory does not exist.
/// A room that is merely unreadable (permissions, temporarily unmounted) is kept.
fn is_stale(room: &KnownRoom) -> bool {
    !room.repo_root.exists()
}

pub(crate) fn run_prune_rooms(apply: bool) -> Result<PruneRoomsReport> {
    let index_path = match room_index_path() {
        Some(p) => p,
        None => {
            return Ok(PruneRoomsReport {
                live: 0,
                stale: Vec::new(),
                applied: false,
                warnings: vec![DiscoveryWarning {
                    code: "global_index_disabled".to_string(),
                    message: "RALLY_NO_GLOBAL_INDEX is set; prune-rooms unavailable".to_string(),
                    path: None,
                    count: None,
                }],
            });
        }
    };

    let index = read_room_index_at(&index_path)?;

    let mut live_rooms: Vec<KnownRoom> = Vec::new();
    let mut stale_rooms: Vec<StaleRoom> = Vec::new();

    for room in index.rooms {
        if is_stale(&room) {
            stale_rooms.push(StaleRoom {
                repo_root: room.repo_root,
                display_name: room.display_name,
            });
        } else {
            live_rooms.push(room);
        }
    }

    let live_count = live_rooms.len();

    if apply && !stale_rooms.is_empty() {
        let updated = RoomIndex {
            schema: index.schema,
            rooms: live_rooms,
        };
        write_room_index_at(&index_path, &updated)?;
    }

    Ok(PruneRoomsReport {
        live: live_count,
        stale: stale_rooms,
        applied: apply,
        warnings: Vec::new(),
    })
}

// =============================================================================
// binary-skew logic
// =============================================================================

/// The build stamp `build.rs` embeds as `RALLY_BUILD_ID`, split into its parts.
///
/// Format is `<version>+<git-short-hash>[-dirty]`, or `<version>+nogit` when
/// the build had no git available. `nogit` is parsed as "no commit" rather than
/// a commit literally named `nogit`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedBuildId {
    pub(crate) version: String,
    /// The short hash the binary was built from. `None` for a `+nogit` build or
    /// a stamp that does not carry the `+<hash>` suffix at all.
    pub(crate) commit: Option<String>,
    /// The build tree had uncommitted build-relevant changes, so `commit` names
    /// the parent commit rather than the exact source that was compiled.
    pub(crate) dirty: bool,
}

/// Split a `RALLY_BUILD_ID` stamp. Never fails — an unrecognised stamp yields
/// the whole string as `version` with no commit, which downgrades the check to
/// "cannot compare" instead of producing a wrong verdict.
pub(crate) fn parse_build_id(build_id: &str) -> ParsedBuildId {
    let Some((version, rest)) = build_id.split_once('+') else {
        return ParsedBuildId {
            version: build_id.to_string(),
            commit: None,
            dirty: false,
        };
    };
    let (hash, dirty) = match rest.strip_suffix("-dirty") {
        Some(h) => (h, true),
        None => (rest, false),
    };
    let commit = if hash.is_empty() || hash == "nogit" {
        None
    } else {
        Some(hash.to_string())
    };
    ParsedBuildId {
        version: version.to_string(),
        commit,
        dirty,
    }
}

/// What the running binary's stamp says relative to the repo it is operating on.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkewVerdict {
    /// Binary commit == repo HEAD. The strongest statement available.
    InSync,
    /// Binary commit is an ancestor of HEAD: the repo has moved on, so the
    /// binary is missing every commit since. This is the finding worth acting on.
    BinaryBehindHead,
    /// Binary commit is a real commit here but NOT an ancestor of HEAD — a
    /// different branch, or a rebased/amended history. Not ordered, so not
    /// reported as "behind".
    Diverged,
    /// The binary carries no commit (`+nogit`) but its version differs from the
    /// repo's `Cargo.toml`. Version-level skew only — a commit-level difference
    /// at the same version is invisible to this branch.
    VersionMismatch,
    /// No commit in the stamp and the versions agree. Consistent as far as this
    /// check can see, which is not the same as in sync.
    VersionOnlyMatch,
    /// Nothing could be compared: no HEAD, unreadable git, or a commit the repo
    /// does not contain.
    Unknown,
}

/// Git facts about the repo under inspection, injected so the classifier is
/// pure and both branches are testable without building throwaway repos.
#[derive(Clone, Debug, Default)]
pub(crate) struct RepoSkewFacts {
    /// `git rev-parse --short HEAD`.
    pub(crate) head_short: Option<String>,
    /// The repo contains the binary's commit object (`git cat-file -e`).
    pub(crate) build_commit_present: bool,
    /// `git merge-base --is-ancestor <build-commit> HEAD` succeeded.
    pub(crate) build_commit_is_ancestor: bool,
    /// `git rev-list --count <build-commit>..HEAD`.
    pub(crate) commits_behind: Option<i64>,
    /// `version` from the repo's workspace `Cargo.toml`.
    pub(crate) manifest_version: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct BinarySkewReport {
    /// The repo this binary was pointed at.
    pub(crate) repo_root: PathBuf,
    /// The running binary's full `RALLY_BUILD_ID` stamp.
    pub(crate) binary_build_id: String,
    pub(crate) binary_version: String,
    /// Short hash from the stamp; absent for a `+nogit` build.
    pub(crate) binary_commit: Option<String>,
    /// The binary was built from a tree with uncommitted changes.
    pub(crate) binary_dirty: bool,
    pub(crate) repo_head: Option<String>,
    pub(crate) repo_version: Option<String>,
    pub(crate) verdict: SkewVerdict,
    /// Commits on HEAD that the binary does not contain. Only meaningful for
    /// `binary_behind_head`.
    pub(crate) commits_behind: Option<i64>,
    /// One sentence stating what was compared and what that does or does not prove.
    pub(crate) detail: String,
    pub(crate) warnings: Vec<DiscoveryWarning>,
}

/// Pure core: classify the running binary against the repo.
///
/// The check reports only what the existing build stamp can support. `build.rs`
/// already embeds `<version>+<short-hash>[-dirty]`, so a commit-level comparison
/// is available for normal builds; a `+nogit` build carries only a version, and
/// the verdict says so rather than implying commit-level confidence it does not
/// have. Diagnostic only — every failure path yields a verdict plus a warning,
/// never an error.
pub(crate) fn classify_binary_skew(
    repo_root: &Path,
    build_id: &str,
    facts: &RepoSkewFacts,
) -> BinarySkewReport {
    let parsed = parse_build_id(build_id);
    let mut warnings: Vec<DiscoveryWarning> = Vec::new();
    let mut commits_behind = None;

    let (verdict, detail) = match (&parsed.commit, &facts.head_short) {
        (Some(commit), Some(head)) if commit == head => (
            SkewVerdict::InSync,
            format!("binary was built from HEAD ({head})"),
        ),
        (Some(commit), Some(head)) if !facts.build_commit_present => {
            warnings.push(DiscoveryWarning {
                code: "skew_build_commit_unknown".to_string(),
                message: format!(
                    "commit {commit} from the running binary is not in this repo; \
                     the binary was built elsewhere, or its commit was garbage-collected"
                ),
                path: Some(repo_root.to_path_buf()),
                count: None,
            });
            (
                SkewVerdict::Unknown,
                format!(
                    "binary commit {commit} is absent from this repo, so it cannot be ordered against HEAD ({head})"
                ),
            )
        }
        (Some(commit), Some(head)) if facts.build_commit_is_ancestor => {
            commits_behind = facts.commits_behind;
            let behind = facts
                .commits_behind
                .map(|n| format!("{n} commit(s)"))
                .unwrap_or_else(|| "an unknown number of commits".to_string());
            (
                SkewVerdict::BinaryBehindHead,
                format!(
                    "binary was built at {commit}, {behind} behind HEAD ({head}) — \
                     it is missing every change since; rebuild and reinstall to pick them up"
                ),
            )
        }
        (Some(commit), Some(head)) => (
            SkewVerdict::Diverged,
            format!(
                "binary commit {commit} is not an ancestor of HEAD ({head}) — \
                 a different branch or a rewritten history, so 'behind' does not apply"
            ),
        ),
        (Some(commit), None) => {
            warnings.push(DiscoveryWarning {
                code: "skew_head_unresolved".to_string(),
                message: format!("cannot read HEAD at {}", repo_root.display()),
                path: Some(repo_root.to_path_buf()),
                count: None,
            });
            (
                SkewVerdict::Unknown,
                format!("binary reports commit {commit} but this repo's HEAD is unreadable"),
            )
        }
        (None, _) => {
            // `+nogit` build: version is the only comparable field. Say plainly
            // what that misses rather than implying commit-level confidence.
            let caveat = "this compares versions only and cannot see commit-level skew: \
                          a binary many commits old at the same version reads as consistent";
            match (&facts.manifest_version, &parsed.version) {
                (Some(repo_v), bin_v) if repo_v != bin_v => (
                    SkewVerdict::VersionMismatch,
                    format!(
                        "binary carries no commit stamp; version {bin_v} differs from the repo's {repo_v} — {caveat}"
                    ),
                ),
                (Some(repo_v), _) => (
                    SkewVerdict::VersionOnlyMatch,
                    format!(
                        "binary carries no commit stamp; version matches the repo's {repo_v} — {caveat}"
                    ),
                ),
                (None, _) => {
                    warnings.push(DiscoveryWarning {
                        code: "skew_manifest_version_unreadable".to_string(),
                        message: format!("cannot read a version from {}", repo_root.display()),
                        path: Some(repo_root.to_path_buf()),
                        count: None,
                    });
                    (
                        SkewVerdict::Unknown,
                        "binary carries no commit stamp and the repo's version is unreadable — nothing to compare".to_string(),
                    )
                }
            }
        }
    };

    // A dirty build stamp names the PARENT commit, not the source that was
    // compiled, so even `in_sync` is only "built from a tree based on HEAD".
    let detail = if parsed.dirty {
        format!(
            "{detail}. The stamp is -dirty: the binary was built from uncommitted changes, so its commit names the parent, not the compiled source"
        )
    } else {
        detail
    };

    BinarySkewReport {
        repo_root: repo_root.to_path_buf(),
        binary_build_id: build_id.to_string(),
        binary_version: parsed.version,
        binary_commit: parsed.commit,
        binary_dirty: parsed.dirty,
        repo_head: facts.head_short.clone(),
        repo_version: facts.manifest_version.clone(),
        verdict,
        commits_behind,
        detail,
        warnings,
    }
}

/// Run `git -C <repo_root> <args>`, returning trimmed stdout on success.
/// `None` on any failure — git absent, non-zero exit, empty or non-UTF8 output.
fn git_in(repo_root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Manifest paths searched for the repo's declared version, in order. The
/// rally-cli crate manifest comes FIRST because that is the version the running
/// binary's `CARGO_PKG_VERSION` came from; this repo's workspace `Cargo.toml`
/// declares no `version` key at all (`[workspace.package]` carries edition,
/// license, and rust-version only), so rooting the lookup there would report
/// "version unreadable" on a healthy checkout.
const VERSION_MANIFESTS: [&str; 2] = ["crates/rally-cli/Cargo.toml", "Cargo.toml"];

/// TOML table headers whose `version` key names the package's OWN version.
/// Anything under `[dependencies]` and friends is a dependency requirement and
/// must never be mistaken for it.
const VERSION_SECTIONS: [&str; 2] = ["package", "workspace.package"];

/// `version = "..."` from a `[package]` or `[workspace.package]` table.
/// Deliberately a line scan, not a TOML parse: this is the fallback for `+nogit`
/// builds only, and a parser dependency would cost more than the branch is worth.
fn parse_manifest_version(text: &str) -> Option<String> {
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = header.to_string();
            continue;
        }
        if !VERSION_SECTIONS.contains(&section.as_str()) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let v = value.trim().trim_matches('"').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn manifest_version(repo_root: &Path) -> Option<String> {
    VERSION_MANIFESTS.iter().find_map(|rel| {
        fs::read_to_string(repo_root.join(rel))
            .ok()
            .and_then(|text| parse_manifest_version(&text))
    })
}

/// Collect the repo-side git facts the classifier needs. Every probe is
/// best-effort; a failure narrows the verdict rather than erroring.
fn collect_skew_facts(repo_root: &Path, build_commit: Option<&str>) -> RepoSkewFacts {
    let head_short = git_in(repo_root, &["rev-parse", "--short", "HEAD"]);
    let mut facts = RepoSkewFacts {
        head_short,
        manifest_version: manifest_version(repo_root),
        ..RepoSkewFacts::default()
    };
    let Some(commit) = build_commit else {
        return facts;
    };
    facts.build_commit_present = git_in(repo_root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .is_some()
        // `cat-file -e` prints nothing on success, so `git_in`'s empty-output
        // rule reports None for a commit that DOES exist. Re-ask with a command
        // that produces output.
        || git_in(repo_root, &["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")]).is_some();
    if facts.build_commit_present {
        facts.build_commit_is_ancestor = std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["merge-base", "--is-ancestor", commit, "HEAD"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if facts.build_commit_is_ancestor {
            facts.commits_behind = git_in(
                repo_root,
                &["rev-list", "--count", &format!("{commit}..HEAD")],
            )
            .and_then(|s| s.parse().ok());
        }
    }
    facts
}

/// `rally doctor --binary-skew`: compare the RUNNING binary's `RALLY_BUILD_ID`
/// against the repo it is operating on.
///
/// Diagnostic only. It never blocks a command and never exits non-zero on skew —
/// the point is that an agent on a stale `~/.local/bin/rally` gets told so,
/// not that its work is stopped.
///
/// Reached from `command_doctor` via `DoctorArgs::binary_skew`. Wiring the call
/// site is the whole point: the measured failure was that every agent on this
/// machine silently ran a `~/.local/bin/rally` old enough to predate the
/// `--version` fix, so `rally room --json` returned 1.99 MB where a current
/// build returned 230 KB, and nothing anywhere said the binary was stale.
pub(crate) fn run_binary_skew() -> Result<BinarySkewReport> {
    let room = RoomStore::open()?;
    let repo_root = room.repo_root().to_path_buf();
    let parsed = parse_build_id(crate::BUILD_ID);
    let facts = collect_skew_facts(&repo_root, parsed.commit.as_deref());
    Ok(classify_binary_skew(&repo_root, crate::BUILD_ID, &facts))
}

#[cfg(test)]
mod binary_skew_tests {
    use super::*;

    fn facts(head: &str, present: bool, ancestor: bool, behind: Option<i64>) -> RepoSkewFacts {
        RepoSkewFacts {
            head_short: Some(head.to_string()),
            build_commit_present: present,
            build_commit_is_ancestor: ancestor,
            commits_behind: behind,
            manifest_version: Some("0.1.7".to_string()),
        }
    }

    #[test]
    fn parse_build_id_splits_version_commit_and_dirty() {
        assert_eq!(
            parse_build_id("0.1.7+6e20ee8"),
            ParsedBuildId {
                version: "0.1.7".to_string(),
                commit: Some("6e20ee8".to_string()),
                dirty: false,
            }
        );
        assert_eq!(
            parse_build_id("0.1.7+0448c33-dirty"),
            ParsedBuildId {
                version: "0.1.7".to_string(),
                commit: Some("0448c33".to_string()),
                dirty: true,
            }
        );
        // `nogit` is build.rs's "git was unavailable" sentinel, not a commit.
        assert_eq!(parse_build_id("0.1.7+nogit").commit, None);
        // An unrecognised stamp must not invent a commit to compare.
        assert_eq!(parse_build_id("weird").commit, None);
        assert_eq!(parse_build_id("weird").version, "weird");
    }

    #[test]
    fn binary_built_from_head_is_in_sync() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+6e20ee8",
            &facts("6e20ee8", true, true, Some(0)),
        );
        assert_eq!(r.verdict, SkewVerdict::InSync);
        assert!(r.warnings.is_empty());
        assert_eq!(r.commits_behind, None, "in-sync reports no behind-count");
    }

    /// The measured real case (2026-08-04): a binary stamped 0448c33 while the
    /// repo sits at 6e20ee8. Ancestor => ordered => "behind", with the count.
    #[test]
    fn ancestor_commit_is_reported_behind_with_a_count() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+0448c33",
            &facts("6e20ee8", true, true, Some(1)),
        );
        assert_eq!(r.verdict, SkewVerdict::BinaryBehindHead);
        assert_eq!(r.commits_behind, Some(1));
        assert!(r.detail.contains("0448c33") && r.detail.contains("6e20ee8"));
        assert!(
            r.detail.contains("rebuild"),
            "a behind verdict must say what to do; got: {}",
            r.detail
        );
    }

    #[test]
    fn non_ancestor_commit_is_diverged_not_behind() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+aaaaaaa",
            &facts("6e20ee8", true, false, None),
        );
        assert_eq!(
            r.verdict,
            SkewVerdict::Diverged,
            "an unordered pair must not be claimed as 'behind'"
        );
        assert_eq!(r.commits_behind, None);
    }

    #[test]
    fn commit_absent_from_the_repo_is_unknown_and_warns() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+deadbee",
            &facts("6e20ee8", false, false, None),
        );
        assert_eq!(r.verdict, SkewVerdict::Unknown);
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.warnings[0].code, "skew_build_commit_unknown");
    }

    #[test]
    fn unreadable_head_is_unknown_not_a_false_in_sync() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+6e20ee8",
            &RepoSkewFacts::default(),
        );
        assert_eq!(r.verdict, SkewVerdict::Unknown);
        assert_eq!(r.warnings[0].code, "skew_head_unresolved");
    }

    /// A `+nogit` binary can only be compared by version, and the report has to
    /// say so — otherwise a matching version reads as proof of no skew.
    #[test]
    fn nogit_build_falls_back_to_version_and_states_the_limit() {
        let mismatch = classify_binary_skew(
            Path::new("/repo"),
            "0.1.6+nogit",
            &facts("6e20ee8", false, false, None),
        );
        assert_eq!(mismatch.verdict, SkewVerdict::VersionMismatch);
        assert!(mismatch.detail.contains("cannot see commit-level skew"));

        let matched = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+nogit",
            &facts("6e20ee8", false, false, None),
        );
        assert_eq!(matched.verdict, SkewVerdict::VersionOnlyMatch);
        assert!(
            matched.detail.contains("cannot see commit-level skew"),
            "a version-only match must not read as commit-level confidence"
        );
    }

    #[test]
    fn dirty_stamp_is_disclosed_even_when_the_commit_matches_head() {
        let r = classify_binary_skew(
            Path::new("/repo"),
            "0.1.7+6e20ee8-dirty",
            &facts("6e20ee8", true, true, Some(0)),
        );
        assert_eq!(r.verdict, SkewVerdict::InSync);
        assert!(r.binary_dirty);
        assert!(
            r.detail.contains("-dirty"),
            "an in-sync verdict from a dirty build must disclose the caveat; got: {}",
            r.detail
        );
    }

    /// A dependency requirement must never be read as the package's own version.
    #[test]
    fn manifest_version_reads_the_package_table_not_dependencies() {
        let toml = "[package]\nname = \"rally-cli\"\nversion = \"0.1.7\"\n\n\
                    [dependencies]\nserde = { version = \"1.0\" }\n";
        assert_eq!(parse_manifest_version(toml), Some("0.1.7".to_string()));

        // This repo's own workspace manifest shape: no version key anywhere.
        let workspace = "[workspace]\nmembers = [\"crates/rally-cli\"]\n\n\
                         [workspace.package]\nedition = \"2024\"\nrust-version = \"1.89\"\n";
        assert_eq!(
            parse_manifest_version(workspace),
            None,
            "a manifest with no version key must report None, not a wrong value"
        );

        // A version-bearing dependency table alone yields nothing.
        assert_eq!(
            parse_manifest_version("[dependencies]\nserde = { version = \"1.0\" }\n"),
            None
        );
    }

    /// Exercises the real I/O collector (git + manifest) against this checkout,
    /// which the pure classifier tests cannot reach. Self-skipping rather than
    /// brittle: a source tarball or a git-less CI box has no HEAD to read, and
    /// that is a legitimate `Unknown`, not a failure.
    #[test]
    fn collect_skew_facts_reads_this_checkout() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/rally-cli has a workspace root")
            .to_path_buf();

        let head = git_in(&repo_root, &["rev-parse", "--short", "HEAD"]);
        let Some(head) = head else {
            return; // no git / not a checkout
        };

        // The binary's OWN commit against this repo: `cat-file -e` prints
        // nothing on success, which is exactly the case that would silently
        // report a present commit as absent if the collector trusted stdout.
        let f = collect_skew_facts(&repo_root, Some(&head));
        assert_eq!(f.head_short.as_deref(), Some(head.as_str()));
        assert!(
            f.build_commit_present,
            "HEAD must resolve as present in its own repo — the empty-stdout \
             `cat-file -e` case is the one that regresses here"
        );
        assert!(f.build_commit_is_ancestor, "HEAD is an ancestor of itself");
        assert_eq!(f.commits_behind, Some(0));
        assert_eq!(
            f.manifest_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "the manifest lookup must find the same version the binary was built with"
        );

        // A commit that cannot exist stays absent rather than defaulting to present.
        let bogus =
            collect_skew_facts(&repo_root, Some("0000000000000000000000000000000000000000"));
        assert!(!bogus.build_commit_present);
        assert!(!bogus.build_commit_is_ancestor);
    }

    /// The check is diagnostic: no branch returns Err, and every unresolvable
    /// case lands on Unknown plus a warning.
    #[test]
    fn no_input_shape_produces_a_hard_failure() {
        for stamp in ["", "+", "0.1.7+", "0.1.7+nogit-dirty", "garbage+x+y"] {
            let r = classify_binary_skew(Path::new("/repo"), stamp, &RepoSkewFacts::default());
            assert!(
                !r.detail.is_empty(),
                "every stamp must yield a stated verdict; {stamp:?} did not"
            );
        }
    }
}

#[cfg(test)]
mod compact_log_tests {
    use super::*;

    /// Build one segment jsonl line in the LedgerLine shape.
    fn line(seq: i64, event_type: &str, tool: &str, subject: &str) -> String {
        serde_json::json!({
            "seq": seq,
            "occurred_at": format!("2026-07-03T19:{:02}:00Z", seq % 60),
            "event_type": event_type,
            "payload": {"tool": tool, "subject": subject},
            "engagement": "test"
        })
        .to_string()
    }

    /// Repeated heartbeats collapse into summarized runs; interleaved
    /// non-presence lines pass through and split the runs.
    #[test]
    fn presence_runs_collapse_into_counts() {
        let contents = [
            line(1, "presence", "codex:a", "agent presence: codex:a"),
            line(2, "presence", "codex:a", "agent presence: codex:a"),
            line(
                3,
                "presence",
                "claude_code:b",
                "agent presence: claude_code:b",
            ),
            line(4, "read", "codex:a", "room read"),
            line(5, "presence", "codex:a", "agent presence: codex:a"),
            line(6, "presence", "codex:a", "agent presence: codex:a"),
        ]
        .join("\n");
        let r = compact_log_content(Path::new("test.jsonl"), &contents);

        assert_eq!(r.total_lines, 6);
        assert_eq!(r.presence_lines, 5);
        assert_eq!(r.presence_runs, 2);
        assert_eq!(r.unparseable_lines, 0);
        // 5 presence lines render as 2 run entries → 3 lines saved.
        assert_eq!(r.lines_saved, 3);
        assert_eq!(r.entries.len(), 3, "run + read + run");

        let CompactLogEntry::PresenceRun(first) = &r.entries[0] else {
            panic!("entry 0 must be a presence run");
        };
        assert_eq!((first.first_seq, first.last_seq, first.count), (1, 3, 3));
        assert_eq!(first.tools.get("codex:a"), Some(&2));
        assert_eq!(first.tools.get("claude_code:b"), Some(&1));

        let CompactLogEntry::Event(read) = &r.entries[1] else {
            panic!("entry 1 must be the read event");
        };
        assert_eq!(read.event_type, "read");
        assert_eq!(read.seq, 4);

        let CompactLogEntry::PresenceRun(second) = &r.entries[2] else {
            panic!("entry 2 must be a presence run");
        };
        assert_eq!((second.first_seq, second.last_seq, second.count), (5, 6, 2));
    }

    /// A lone heartbeat between other events is NOT a flood — it passes
    /// through as a normal event, uncollapsed.
    #[test]
    fn single_presence_line_passes_through() {
        let contents = [
            line(1, "read", "codex:a", "room read"),
            line(2, "presence", "codex:a", "agent presence: codex:a"),
            line(3, "claim", "codex:a", "edit file"),
        ]
        .join("\n");
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.presence_runs, 0);
        assert_eq!(r.lines_saved, 0);
        assert_eq!(r.entries.len(), 3);
        let CompactLogEntry::Event(mid) = &r.entries[1] else {
            panic!("lone presence stays an event");
        };
        assert_eq!(mid.event_type, "presence");
    }

    /// Garbage lines are counted and skipped; blank lines are ignored; the
    /// rest of the log still compacts.
    #[test]
    fn unparseable_and_blank_lines_are_tolerated() {
        let contents = format!(
            "{}\nnot-json at all\n\n{{\"no_event_type\":true}}\n{}\n{}\n",
            line(1, "wake", "codex:a", "wake"),
            line(2, "presence", "codex:a", "hb"),
            line(3, "presence", "codex:a", "hb"),
        );
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.total_lines, 5, "blank line not counted");
        assert_eq!(r.unparseable_lines, 2);
        assert_eq!(r.presence_runs, 1);
        assert_eq!(r.entries.len(), 2, "wake event + one run");
    }

    /// Regression (codex review seq 4535, finding 2): an unparseable line
    /// BETWEEN heartbeats breaks consecutiveness — the runs on either side
    /// must not merge across the corruption.
    #[test]
    fn unparseable_line_splits_presence_runs() {
        let contents = format!(
            "{}\n{}\ncorrupted-not-json\n{}\n{}\n",
            line(1, "presence", "codex:a", "hb"),
            line(2, "presence", "codex:a", "hb"),
            line(3, "presence", "codex:a", "hb"),
            line(4, "presence", "codex:a", "hb"),
        );
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.unparseable_lines, 1);
        assert_eq!(r.presence_runs, 2, "corruption splits the run in two");
        assert_eq!(r.entries.len(), 2);
        let (CompactLogEntry::PresenceRun(a), CompactLogEntry::PresenceRun(b)) =
            (&r.entries[0], &r.entries[1])
        else {
            panic!("both entries must be runs");
        };
        assert_eq!((a.first_seq, a.last_seq, a.count), (1, 2, 2));
        assert_eq!((b.first_seq, b.last_seq, b.count), (3, 4, 2));

        // Missing event_type (parseable JSON, still not a valid line) splits
        // a would-be run into two singleton pass-through events.
        let contents = format!(
            "{}\n{{\"no_event_type\":true}}\n{}\n",
            line(1, "presence", "codex:a", "hb"),
            line(2, "presence", "codex:a", "hb"),
        );
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.presence_runs, 0, "singletons on each side, no run");
        assert_eq!(r.entries.len(), 2);
    }

    /// Regression (codex review seq 4535, finding 1): a pass-through event
    /// keeps its FULL payload — summary, target, evidence, scope, ref, and
    /// unknown future fields all survive compaction unchanged.
    #[test]
    fn passthrough_event_retains_full_payload() {
        let payload = serde_json::json!({
            "tool": "codex:a",
            "subject": "handoff subject",
            "summary": "the long summary",
            "target": "claude_code:b",
            "evidence": ["commit:abc123", "test:green"],
            "scope": ["file:crates/rally-cli/src/doctor.rs"],
            "ref": "fact_123",
            "future_field": {"nested": true}
        });
        let contents = serde_json::json!({
            "seq": 7,
            "occurred_at": "2026-07-03T19:30:00Z",
            "event_type": "handoff",
            "payload": payload,
        })
        .to_string();
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.entries.len(), 1);
        let CompactLogEntry::Event(ev) = &r.entries[0] else {
            panic!("handoff passes through as an event");
        };
        assert_eq!(
            ev.payload.as_ref().expect("payload retained"),
            &payload,
            "payload must pass through byte-identical"
        );
        assert_eq!(ev.subject.as_deref(), Some("handoff subject"));
    }

    /// A log that ends mid-heartbeat-run still flushes the trailing run.
    #[test]
    fn trailing_run_is_flushed() {
        let contents = [
            line(1, "presence", "codex:a", "hb"),
            line(2, "presence", "codex:a", "hb"),
        ]
        .join("\n");
        let r = compact_log_content(Path::new("test.jsonl"), &contents);
        assert_eq!(r.presence_runs, 1);
        assert_eq!(r.entries.len(), 1);
    }

    /// Empty contents → empty report, never errors.
    #[test]
    fn empty_log_is_empty_report() {
        let r = compact_log_content(Path::new("test.jsonl"), "");
        assert_eq!(r.total_lines, 0);
        assert!(r.entries.is_empty());
    }
}

#[cfg(test)]
mod ledger_health_tests {
    use super::*;

    fn fixture(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rally-ledger-{label}-{nanos}"));
        fs::create_dir_all(dir.join(LOG_DIRNAME)).unwrap();
        dir
    }

    fn row(seq: i64, subject: &str) -> String {
        format!(
            r#"{{"seq":{seq},"event_type":"artifact","payload":{{"subject":"{subject}"}}}}"#
        )
    }

    fn write_segment(dir: &Path, name: &str, rows: &[String]) {
        fs::write(
            dir.join(LOG_DIRNAME).join(name),
            format!("{}\n", rows.join("\n")),
        )
        .unwrap();
    }

    #[test]
    fn clean_ledger_is_healthy() {
        let dir = fixture("clean");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), row(2, "two"), row(3, "three")]);
        let report = ledger_health_in_dir(&dir);
        assert!(report.healthy, "findings: {:?}", report.findings);
        assert!(report.conflicting_seqs.is_empty());
        assert_eq!(report.segments[0].rows, 3);
        fs::remove_dir_all(&dir).ok();
    }

    /// The exact shape that bricked a real room: two DIFFERENT rows at one seq.
    #[test]
    fn divergent_rows_at_one_seq_are_an_error_with_a_remedy() {
        let dir = fixture("conflict");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), row(2, "two"), row(2, "DIVERGENT")]);
        let report = ledger_health_in_dir(&dir);
        assert!(!report.healthy);
        assert_eq!(report.conflicting_seqs, vec![2]);
        assert!(report.repairable);
        let finding = report
            .findings
            .iter()
            .find(|f| f.code == "conflicting_seqs")
            .expect("conflicting_seqs finding");
        assert_eq!(finding.severity, "error");
        assert!(
            finding.remedy.as_deref().unwrap_or("").contains("--repair-ledger"),
            "the finding must hand over the fixing command"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// An IDENTICAL repeated row is benign — the store's own rule. Flagging it
    /// would send operators rewriting a ledger that is fine.
    #[test]
    fn identical_repeated_row_is_not_a_conflict() {
        let dir = fixture("identical");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), row(2, "two"), row(2, "two")]);
        let report = ledger_health_in_dir(&dir);
        assert!(report.conflicting_seqs.is_empty(), "{:?}", report.findings);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unparseable_line_is_reported_not_swallowed() {
        let dir = fixture("unparseable");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), "{not json".to_string()]);
        let report = ledger_health_in_dir(&dir);
        assert!(!report.healthy);
        assert_eq!(report.segments[0].unparseable_lines, vec![2]);
        fs::remove_dir_all(&dir).ok();
    }

    /// The whole point: this mode must answer without opening the store or the
    /// derived DB, so it still works when every other command is failing.
    #[test]
    fn works_with_no_derived_db_present() {
        let dir = fixture("nodb");
        write_segment(&dir, "a.jsonl", &[row(1, "one")]);
        assert!(!dir.join("facts.db").exists());
        let report = ledger_health_in_dir(&dir);
        assert!(report.healthy);
        assert!(!report.derived_db_present);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_rally_dir_reports_instead_of_erroring() {
        let dir = std::env::temp_dir().join("rally-ledger-absent-xyz-does-not-exist");
        let report = ledger_health_in_dir(&dir);
        assert!(!report.rally_dir_exists);
        assert!(report.findings.iter().any(|f| f.code == "no_rally_dir"));
    }

    #[test]
    fn repair_dry_run_writes_nothing() {
        let dir = fixture("dryrun");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), row(2, "two"), row(2, "DIVERGENT")]);
        let before = fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap();
        let report = repair_ledger_in_dir(&dir, false, "stamp").unwrap();
        assert!(!report.applied);
        assert_eq!(report.rows_renumbered, 1);
        assert_eq!(
            before,
            fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap(),
            "dry run must not touch the segment"
        );
        assert!(!report.archive_dir.exists(), "dry run creates no archive");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repair_apply_archives_the_original_before_rewriting() {
        let dir = fixture("apply");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), row(2, "two"), row(2, "DIVERGENT")]);
        let before = fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap();

        let report = repair_ledger_in_dir(&dir, true, "stamp").unwrap();
        assert!(report.applied);
        assert_eq!(report.rows_renumbered, 1);

        // The pre-repair original survives, byte for byte. Nothing is deleted.
        let archived = report.archive_dir.join("a.jsonl");
        assert!(archived.is_file(), "{} archived", archived.display());
        assert_eq!(before, fs::read_to_string(&archived).unwrap());

        // And the live segment now has unique, strictly increasing seqs.
        let after = fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap();
        let seqs: Vec<i64> = after
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);

        // Repair is idempotent: re-running finds nothing left to do.
        let again = repair_ledger_in_dir(&dir, true, "stamp2").unwrap();
        assert_eq!(again.rows_renumbered, 0);
        fs::remove_dir_all(&dir).ok();
    }

    /// Content other than `seq` must survive the rewrite untouched.
    #[test]
    fn repair_preserves_every_other_field() {
        let dir = fixture("preserve");
        write_segment(&dir, "a.jsonl", &[row(9, "keep-me"), row(9, "and-me")]);
        repair_ledger_in_dir(&dir, true, "s").unwrap();
        let after = fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap();
        assert!(after.contains("keep-me"));
        assert!(after.contains("and-me"));
        assert!(after.contains("\"event_type\":\"artifact\""));
        fs::remove_dir_all(&dir).ok();
    }

    /// A segment we cannot fully parse is left completely alone. Renumbering
    /// around a row we do not understand risks writing a worse file than we found.
    #[test]
    fn repair_refuses_a_segment_with_unparseable_rows() {
        let dir = fixture("refuse");
        write_segment(&dir, "a.jsonl", &[row(1, "one"), "{broken".to_string(), row(1, "dup")]);
        let before = fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap();
        let report = repair_ledger_in_dir(&dir, true, "s").unwrap();
        assert_eq!(report.rows_renumbered, 0);
        assert!(report.warnings.iter().any(|w| w.code == "repair_skipped_unparseable"));
        assert_eq!(
            before,
            fs::read_to_string(dir.join(LOG_DIRNAME).join("a.jsonl")).unwrap()
        );
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rally-sweep-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plant a corrupt snapshot group (base + shm + wal siblings) at `stamp_ns`.
    fn plant(dir: &Path, stamp_ns: u128, bytes: usize) {
        let base = dir.join(format!("{CORRUPT_PREFIX}{stamp_ns}"));
        fs::write(&base, vec![b'x'; bytes]).unwrap();
        fs::write(
            dir.join(format!("{CORRUPT_PREFIX}{stamp_ns}-db-shm")),
            b"shm",
        )
        .unwrap();
        fs::write(
            dir.join(format!("{CORRUPT_PREFIX}{stamp_ns}-db-wal")),
            b"wal",
        )
        .unwrap();
    }

    fn snapshot_bases(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(CORRUPT_PREFIX) && snapshot_stamp(n).is_some())
            .collect();
        v.sort();
        v
    }

    const DAY_NS: u128 = 86_400 * 1_000_000_000;

    /// Rigged FAILING case first: with old snapshots present, a dry-run reports
    /// them swept but removes NOTHING (the debris is still on disk). Then the
    /// PASSING case: `--apply` actually removes the swept set, keeping newest-1
    /// plus anything inside the age window.
    #[test]
    fn sweep_dry_run_reports_then_apply_archives() {
        let dir = unique_dir("mixed");
        // now = 100 days after epoch, in ns.
        let now_ns: u128 = 100 * DAY_NS;
        // Three snapshots: 1 day old (fresh), 20 days old, 40 days old.
        let fresh = now_ns - DAY_NS; // age 1d
        let mid = now_ns - 20 * DAY_NS; // age 20d
        let old = now_ns - 40 * DAY_NS; // age 40d
        plant(&dir, fresh, 10);
        plant(&dir, mid, 20);
        plant(&dir, old, 30);

        // keep=1 newest (fresh), max_age_days=7 (only fresh qualifies by age).
        // So mid + old should be swept.

        // --- Rigged failing state: dry-run must NOT delete ---
        let dry = sweep_corrupt_in_dir(&dir, 1, 7, false, now_ns);
        assert_eq!(dry.swept.len(), 2, "mid+old classified for sweep");
        assert_eq!(dry.kept.len(), 1, "fresh retained (newest + in age window)");
        assert!(!dry.applied);
        assert!(dry.bytes_reclaimable > 0);
        // All 9 files (3 groups x 3 files) still present — dry-run is inert.
        assert_eq!(
            snapshot_bases(&dir).len(),
            9,
            "dry-run removed nothing (rigged pre-state)"
        );

        // --- Passing state: apply removes exactly the swept groups ---
        let applied = sweep_corrupt_in_dir(&dir, 1, 7, true, now_ns);
        assert_eq!(applied.swept.len(), 2);
        assert!(applied.applied);
        let remaining = snapshot_bases(&dir);
        assert_eq!(remaining.len(), 3, "only the fresh group's 3 files remain");
        assert!(
            remaining.iter().all(|n| n.contains(&fresh.to_string())),
            "the retained files all belong to the fresh snapshot"
        );
        assert!(applied.warnings.is_empty());

        // THE INVARIANT: sweeping ARCHIVES, it never deletes. Every file that
        // left the live directory must be present, byte for byte, under the
        // archive — a quarantined snapshot is the forensic record of the
        // corruption that produced it.
        let archived: Vec<PathBuf> = walk_files(&applied.archived_dir);
        assert_eq!(
            archived.len(),
            6,
            "the two swept groups (3 files each) are all under {}",
            applied.archived_dir.display()
        );
        for stamp in [mid, old] {
            let base = applied
                .archived_dir
                .join(stamp.to_string())
                .join(format!("{CORRUPT_PREFIX}{stamp}"));
            assert!(base.is_file(), "{} archived", base.display());
            assert_eq!(
                fs::read(&base).unwrap().len(),
                if stamp == mid { 20 } else { 30 },
                "archived bytes are the original bytes"
            );
        }

        fs::remove_dir_all(&dir).ok();
    }

    /// No file may vanish. Counting live + archived before and after is the
    /// cheapest statement of "doctor never deletes" that cannot be satisfied by
    /// a partial move.
    #[test]
    fn sweep_conserves_every_file() {
        let dir = unique_dir("conserve");
        let now_ns: u128 = 100 * DAY_NS;
        for age in [1u128, 20, 40, 60] {
            plant(&dir, now_ns - age * DAY_NS, 8);
        }
        let before = walk_files(&dir).len();
        let report = sweep_corrupt_in_dir(&dir, 1, 7, true, now_ns);
        let after = walk_files(&dir).len();
        assert_eq!(before, after, "sweep deleted {} file(s)", before - after);
        assert!(report.swept.len() >= 2);
        assert!(report.warnings.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// Recursive file list, so archived files under subdirectories are counted.
    fn walk_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_files(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    /// Age-window override: a snapshot older than the --keep rank is still
    /// retained if it is inside --max-age-days.
    #[test]
    fn sweep_age_window_overrides_keep_rank() {
        let dir = unique_dir("agewin");
        let now_ns: u128 = 100 * DAY_NS;
        plant(&dir, now_ns - DAY_NS, 1); // 1d
        plant(&dir, now_ns - 2 * DAY_NS, 1); // 2d
        plant(&dir, now_ns - 3 * DAY_NS, 1); // 3d
        // keep=1 but max_age_days=30 → all three are inside the window → keep all.
        let r = sweep_corrupt_in_dir(&dir, 1, 30, true, now_ns);
        assert_eq!(r.swept.len(), 0, "age window retains all three");
        assert_eq!(r.kept.len(), 3);
        assert_eq!(snapshot_bases(&dir).len(), 9, "nothing removed");
        fs::remove_dir_all(&dir).ok();
    }

    /// Missing directory → empty report, never errors.
    #[test]
    fn sweep_missing_dir_is_empty() {
        let dir = std::env::temp_dir().join("rally-sweep-does-not-exist-xyz");
        let r = sweep_corrupt_in_dir(&dir, 1, 7, true, 100 * DAY_NS);
        assert!(r.swept.is_empty() && r.kept.is_empty());
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn snapshot_stamp_parses_base_and_siblings() {
        assert_eq!(
            snapshot_stamp("facts.db.corrupt.1782828299464852000"),
            Some("1782828299464852000".to_string())
        );
        assert_eq!(
            snapshot_stamp("facts.db.corrupt.1782828299464852000-db-wal"),
            Some("1782828299464852000".to_string())
        );
        assert_eq!(snapshot_stamp("facts.db"), None);
        assert_eq!(snapshot_stamp("facts.db-wal"), None);
    }
}

#[cfg(all(test, unix))]
mod o26_db_only_migration_tests {
    use super::*;
    use crate::FACT_SCHEMA;
    use crate::store::{
        DirectRoomStore, Fact, FactKind, acquire_named_exclusive_nb,
        acquire_owner_exclusive_blocking,
    };
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    fn unique_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rally-o26-db-only-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn fact(event_id: &str, seq_hint: i64) -> Fact {
        Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: seq_hint,
            thread_id: format!("thread-{event_id}"),
            kind: FactKind::Decision,
            tool: Some("codex:migration-test".to_string()),
            role: None,
            subject: format!("decision {event_id}"),
            scope: vec!["src/".to_string()],
            created_at: format!("2026-08-10T00:00:0{seq_hint}Z"),
            summary: Some(format!("summary {event_id}")),
            evidence: vec![format!("evidence:{event_id}")],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
            from_session_id: None,
        }
    }

    fn db_only_room(label: &str) -> (PathBuf, Vec<Fact>) {
        let root = unique_root(label);
        let store =
            DirectRoomStore::open_direct_at_with_engagement(root.clone(), Some("seed".to_string()))
                .unwrap();
        let expected = vec![fact("db-only-a", 0), fact("db-only-b", 0)];
        for item in &expected {
            store.append_fact(item).unwrap();
        }
        drop(store);
        let log_dir = root.join(".rally/log");
        for entry in fs::read_dir(&log_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                fs::remove_file(path).unwrap();
            }
        }
        assert!(root.join(".rally/facts.db").is_file());
        (root, expected)
    }

    fn file_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(base: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            if !path.exists() {
                return;
            }
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.insert(
                        path.strip_prefix(base).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn jsonl_files(root: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(root.join(".rally/log"))
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    fn append_valid_canonical_extension(target: &Path, event_id: &str) -> Vec<u8> {
        let existing = fs::read_to_string(target).unwrap();
        let mut extension: serde_json::Value =
            serde_json::from_str(existing.lines().last().unwrap()).unwrap();
        let next_seq = extension["seq"].as_i64().unwrap() + 1;
        extension["seq"] = serde_json::json!(next_seq);
        extension["occurred_at"] = serde_json::json!("2026-08-10T00:01:00Z");
        extension["payload"]["seq"] = serde_json::json!(next_seq);
        extension["payload"]["event_id"] = serde_json::json!(event_id);
        extension["payload"]["thread_id"] = serde_json::json!(format!("thread-{event_id}"));
        extension["payload"]["created_at"] = serde_json::json!("2026-08-10T00:01:00Z");
        extension["payload"]["subject"] = serde_json::json!("post migration append");
        let mut bytes = serde_json::to_vec(&extension).unwrap();
        bytes.push(b'\n');
        OpenOptions::new()
            .append(true)
            .open(target)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        fs::read(target).unwrap()
    }

    #[test]
    fn db_only_migration_dry_run_writes_nothing() {
        let (root, _) = db_only_room("dry-run");
        let before = file_tree(&root);
        let report = run_db_only_migration_at(&root, "alpha", false).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::DryRun);
        assert!(!report.applied);
        assert_eq!(report.row_count, Some(2));
        assert_eq!(file_tree(&root), before, "dry-run must be byte-inert");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_dry_run_surfaces_owner_and_unsafe_source_blockers() {
        let (owned_root, _) = db_only_room("dry-owner-observation");
        let owned_rally = owned_root.join(".rally");
        let daemon_owner = acquire_owner_exclusive_blocking(&owned_rally).unwrap();
        let before = file_tree(&owned_root);
        let report = run_db_only_migration_at(&owned_root, "alpha", false).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::DryRun);
        assert!(report.source_token.is_some());
        assert_eq!(report.observed_blockers, vec!["daemon_owner_busy"]);
        assert_eq!(file_tree(&owned_root), before);
        drop(daemon_owner);

        let (wal_root, _) = db_only_room("dry-wal-blocker");
        let wal = wal_root.join(".rally/facts.db-wal");
        fs::write(&wal, b"uncheckpointed WAL evidence").unwrap();
        let before = file_tree(&wal_root);
        let report = run_db_only_migration_at(&wal_root, "alpha", false).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::DryRun);
        assert!(report.source_token.is_none());
        assert!(
            report
                .observed_blockers
                .iter()
                .any(|blocker| blocker.contains("WAL"))
        );
        assert_eq!(file_tree(&wal_root), before);

        fs::remove_dir_all(owned_root).ok();
        fs::remove_dir_all(wal_root).ok();
    }

    #[test]
    fn db_only_migration_apply_installs_one_exact_canonical_segment() {
        let (root, _) = db_only_room("apply");
        let db = root.join(".rally/facts.db");
        let db_before = fs::read(&db).unwrap();
        let report = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::Committed);
        assert!(report.applied);
        assert_eq!(report.row_count, Some(2));
        assert_eq!(jsonl_files(&root), vec![report.target_path.clone()]);
        assert!(
            !report.marker_path.exists(),
            "verified commit consumes marker"
        );
        assert!(
            report.receipt_path.is_file(),
            "verified commit retains receipt"
        );
        assert_eq!(fs::read(&db).unwrap(), db_before, "facts.db is preserved");
        let lines = fs::read_to_string(&report.target_path).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.ends_with('\n'));
        for line in lines.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["engagement"], "alpha");
        }
        let retry = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(retry.state, DbOnlyMigrationState::AlreadyCommitted);
        assert_eq!(jsonl_files(&root).len(), 1, "successful retry is singleton");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_receipt_accepts_only_valid_append_only_extension() {
        let (root, _) = db_only_room("receipt-extension");
        let committed = run_db_only_migration_at(&root, "alpha", true).unwrap();
        let mut lines = fs::read_to_string(&committed.target_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut extension: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        let committed_max = committed
            .max_seq
            .expect("committed migration reports max seq");
        extension["seq"] = serde_json::json!(committed_max + 1);
        extension["occurred_at"] = serde_json::json!("2026-08-10T00:01:00Z");
        extension["payload"]["seq"] = serde_json::json!(committed_max + 1);
        extension["payload"]["event_id"] = serde_json::json!("post-migration-append");
        extension["payload"]["thread_id"] = serde_json::json!("thread-post-migration-append");
        extension["payload"]["created_at"] = serde_json::json!("2026-08-10T00:01:00Z");
        extension["payload"]["subject"] = serde_json::json!("post migration append");
        lines.push(serde_json::to_string(&extension).unwrap());
        let extended = format!("{}\n", lines.join("\n")).into_bytes();
        fs::write(&committed.target_path, &extended).unwrap();

        let retry = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(retry.state, DbOnlyMigrationState::AlreadyCommitted);
        assert_eq!(fs::read(&committed.target_path).unwrap(), extended);
        assert_eq!(jsonl_files(&root).len(), 1);

        let (divergent_root, _) = db_only_room("receipt-divergent");
        let divergent = run_db_only_migration_at(&divergent_root, "alpha", true).unwrap();
        let mut divergent_bytes = fs::read(&divergent.target_path).unwrap();
        divergent_bytes[0] = if divergent_bytes[0] == b'{' {
            b'['
        } else {
            b'{'
        };
        fs::write(&divergent.target_path, &divergent_bytes).unwrap();
        let receipt_before = fs::read(&divergent.receipt_path).unwrap();
        let error = run_db_only_migration_at(&divergent_root, "alpha", true).unwrap_err();
        assert!(
            error.to_string().contains("receipt-bound prefix"),
            "{error}"
        );
        assert_eq!(fs::read(&divergent.target_path).unwrap(), divergent_bytes);
        assert_eq!(fs::read(&divergent.receipt_path).unwrap(), receipt_before);

        let (missing_root, _) = db_only_room("receipt-missing-target");
        let missing = run_db_only_migration_at(&missing_root, "alpha", true).unwrap();
        fs::remove_file(&missing.target_path).unwrap();
        let receipt_before = fs::read(&missing.receipt_path).unwrap();
        let error = run_db_only_migration_at(&missing_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("migration target"), "{error}");
        assert!(!missing.target_path.exists());
        assert_eq!(fs::read(&missing.receipt_path).unwrap(), receipt_before);

        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(divergent_root).ok();
        fs::remove_dir_all(missing_root).ok();
    }

    #[test]
    fn db_only_migration_cleanup_accepts_hardlinked_temp_with_valid_target_extension() {
        let (root, _) = db_only_room("receipt-hardlink-extension");
        let rally = root.join(".rally");
        let db = rally.join("facts.db");
        let db_before = fs::read(&db).unwrap();
        arm_db_only_migration_fault(&rally, DbOnlyMigrationFaultPoint::AfterReceiptSync);
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("committed"), "{error}");

        let marker_path = rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
        let receipt_path = rally.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
        let marker: DbOnlyMigrationMarker =
            read_json_file(&marker_path, "migration marker").unwrap();
        let temp = root.join(&marker.temp_relative_path);
        let target = root.join(&marker.target_relative_path);
        assert!(temp.is_file());
        assert!(target.is_file());
        let receipt_before = fs::read(&receipt_path).unwrap();

        let extended = append_valid_canonical_extension(&target, "post-receipt-append");
        assert_eq!(
            fs::read(&temp).unwrap(),
            extended,
            "the post-receipt append reaches the hard-linked temp inode"
        );

        let retry = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(retry.state, DbOnlyMigrationState::AlreadyCommitted);
        assert_eq!(fs::read(&target).unwrap(), extended);
        assert_eq!(fs::read(&receipt_path).unwrap(), receipt_before);
        assert_eq!(fs::read(&db).unwrap(), db_before);
        assert!(!marker_path.exists());
        assert!(!temp.exists());
        assert_eq!(jsonl_files(&root), vec![target]);
        fs::remove_dir_all(root).ok();

        let (divergent_root, _) = db_only_room("receipt-divergent-temp");
        let divergent_rally = divergent_root.join(".rally");
        arm_db_only_migration_fault(
            &divergent_rally,
            DbOnlyMigrationFaultPoint::AfterReceiptSync,
        );
        run_db_only_migration_at(&divergent_root, "alpha", true).unwrap_err();
        let divergent_marker_path = divergent_rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME);
        let divergent_receipt_path = divergent_rally.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
        let divergent_marker: DbOnlyMigrationMarker =
            read_json_file(&divergent_marker_path, "migration marker").unwrap();
        let divergent_temp = divergent_root.join(&divergent_marker.temp_relative_path);
        let divergent_target = divergent_root.join(&divergent_marker.target_relative_path);
        fs::remove_file(&divergent_temp).unwrap();
        fs::write(&divergent_temp, b"divergent temp evidence").unwrap();
        let target_before = fs::read(&divergent_target).unwrap();
        let receipt_before = fs::read(&divergent_receipt_path).unwrap();
        let error = run_db_only_migration_at(&divergent_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("migration temp"), "{error}");
        assert_eq!(
            fs::read(&divergent_temp).unwrap(),
            b"divergent temp evidence"
        );
        assert_eq!(fs::read(&divergent_target).unwrap(), target_before);
        assert_eq!(fs::read(&divergent_receipt_path).unwrap(), receipt_before);
        assert!(divergent_marker_path.is_file());
        fs::remove_dir_all(divergent_root).ok();
    }

    #[test]
    fn db_only_migration_refuses_live_or_unresponsive_owners_without_canonical_write() {
        let (live_root, _) = db_only_room("live-owner");
        let live_rally = live_root.join(".rally");
        let daemon = acquire_owner_exclusive_blocking(&live_rally).unwrap();
        let error = run_db_only_migration_at(&live_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("rally daemon stop"));
        assert!(jsonl_files(&live_root).is_empty());
        assert!(!live_rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME).exists());
        drop(daemon);

        let (busy_root, _) = db_only_room("busy-owner");
        let busy_rally = busy_root.join(".rally");
        let direct = acquire_named_exclusive_nb(&busy_rally, "direct.owner.lock")
            .unwrap()
            .expect("test owns direct lock");
        let error = run_db_only_migration_at(&busy_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("offline migration authority"));
        assert!(jsonl_files(&busy_root).is_empty());
        assert!(!busy_rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME).exists());
        drop(direct);
        fs::remove_dir_all(live_root).ok();
        fs::remove_dir_all(busy_root).ok();
    }

    #[test]
    fn db_only_migration_expired_prepublication_deadline_is_not_started() {
        let (root, _) = db_only_room("deadline-before-marker");
        let rally = root.join(".rally");
        let db_before = fs::read(rally.join("facts.db")).unwrap();
        arm_db_only_migration_fault(
            &rally,
            DbOnlyMigrationFaultPoint::ExpireDeadlineBeforeMarker,
        );
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        crate::store::clear_mutation_deadline_for_test();
        assert!(
            matches!(
                error,
                DbOnlyMigrationRunError::Other(RallyError::NotStarted(_))
            ),
            "{error}"
        );
        assert_eq!(fs::read(rally.join("facts.db")).unwrap(), db_before);
        assert!(!rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME).exists());
        assert!(!rally.join(DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME).exists());
        assert!(jsonl_files(&root).is_empty());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_refuses_wal_or_any_canonical_source() {
        let (wal_root, _) = db_only_room("wal");
        let wal = wal_root.join(".rally/facts.db-wal");
        fs::write(&wal, b"pending sqlite commit").unwrap();
        let wal_before = fs::read(&wal).unwrap();
        let error = run_db_only_migration_at(&wal_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("WAL"));
        assert_eq!(fs::read(&wal).unwrap(), wal_before);
        assert!(jsonl_files(&wal_root).is_empty());

        let (canonical_root, _) = db_only_room("canonical");
        let existing = canonical_root.join(".rally/log/existing.jsonl");
        fs::write(&existing, b"canonical evidence\n").unwrap();
        let before = fs::read(&existing).unwrap();
        let error = run_db_only_migration_at(&canonical_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("canonical source"));
        assert_eq!(fs::read(&existing).unwrap(), before);
        assert!(
            !canonical_root
                .join(".rally")
                .join(DB_ONLY_MIGRATION_MARKER_FILENAME)
                .exists()
        );
        fs::remove_dir_all(wal_root).ok();
        fs::remove_dir_all(canonical_root).ok();
    }

    #[test]
    fn db_only_migration_rejects_invalid_or_reserved_engagement_before_io() {
        for (case, engagement) in [
            ("empty", ""),
            ("parent", "../escape"),
            ("separator", "alpha/beta"),
            ("whitespace", " alpha"),
            ("reserved", "test"),
        ] {
            let root = unique_root(case);
            let error = run_db_only_migration_at(&root, engagement, true).unwrap_err();
            assert!(error.to_string().contains("engagement"), "{case}: {error}");
            assert!(
                !root.join(".rally").exists(),
                "{case}: invalid identity must fail before creating migration state"
            );
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn db_only_migration_refuses_nonempty_wal_and_rollback_journal() {
        for suffix in ["db-wal", "db-journal"] {
            let (root, _) = db_only_room(suffix);
            let sidecar = root.join(".rally/facts.db").with_extension(suffix);
            fs::write(&sidecar, format!("pending {suffix}")).unwrap();
            let before = fs::read(&sidecar).unwrap();
            let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
            assert!(error.to_string().contains(suffix), "{suffix}: {error}");
            assert_eq!(fs::read(&sidecar).unwrap(), before);
            assert!(jsonl_files(&root).is_empty());
            assert!(
                !root
                    .join(".rally")
                    .join(DB_ONLY_MIGRATION_MARKER_FILENAME)
                    .exists()
            );
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn db_only_migration_allows_stable_shm_but_rejects_change_or_symlink() {
        let (stable_root, _) = db_only_room("stable-shm");
        let stable_shm = stable_root.join(".rally/facts.db-shm");
        fs::write(&stable_shm, b"stable standalone wal index").unwrap();
        let before = fs::read(&stable_shm).unwrap();
        let report = run_db_only_migration_at(&stable_root, "alpha", true).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::Committed);
        assert_eq!(fs::read(&stable_shm).unwrap(), before);

        let (changing_root, _) = db_only_room("changing-shm");
        let changing_rally = changing_root.join(".rally");
        let changing_shm = changing_rally.join("facts.db-shm");
        fs::write(&changing_shm, b"before").unwrap();
        arm_db_only_migration_fault(
            &changing_rally,
            DbOnlyMigrationFaultPoint::MutateShmAfterDbRead,
        );
        let error = run_db_only_migration_at(&changing_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("changed across"), "{error}");
        assert!(jsonl_files(&changing_root).is_empty());

        let (symlink_root, _) = db_only_room("symlink-shm");
        let external = unique_root("symlink-shm-external").join("shm");
        fs::write(&external, b"external shm").unwrap();
        std::os::unix::fs::symlink(&external, symlink_root.join(".rally/facts.db-shm")).unwrap();
        let external_before = fs::read(&external).unwrap();
        let error = run_db_only_migration_at(&symlink_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        assert_eq!(fs::read(&external).unwrap(), external_before);
        assert!(jsonl_files(&symlink_root).is_empty());

        fs::remove_dir_all(stable_root).ok();
        fs::remove_dir_all(changing_root).ok();
        fs::remove_dir_all(symlink_root).ok();
        fs::remove_dir_all(external.parent().unwrap()).ok();
    }

    #[test]
    fn db_only_migration_rejects_symlinked_authority_and_evidence_paths() {
        use std::os::unix::fs::symlink;

        let root = unique_root("rally-symlink");
        let external = unique_root("rally-symlink-external");
        symlink(&external, root.join(".rally")).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert!(file_tree(&external).is_empty());
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(external).ok();

        let (root, _) = db_only_room("log-symlink");
        let external = unique_root("log-symlink-external");
        let log = root.join(".rally/log");
        fs::remove_dir_all(&log).unwrap();
        symlink(&external, &log).unwrap();
        let before = file_tree(&external);
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(file_tree(&external), before);
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(external).ok();

        let (root, _) = db_only_room("db-symlink");
        let db = root.join(".rally/facts.db");
        let external = unique_root("db-symlink-external").join("preserved.db");
        fs::rename(&db, &external).unwrap();
        symlink(&external, &db).unwrap();
        let before = fs::read(&external).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&external).unwrap(), before);
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(external.parent().unwrap()).ok();

        for filename in [
            DB_ONLY_MIGRATION_MARKER_FILENAME,
            DB_ONLY_MIGRATION_RECEIPT_FILENAME,
        ] {
            let (root, _) = db_only_room(filename);
            let external = unique_root("metadata-symlink-external").join("evidence");
            fs::write(&external, b"external evidence").unwrap();
            symlink(&external, root.join(".rally").join(filename)).unwrap();
            let before = fs::read(&external).unwrap();
            let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
            assert!(error.to_string().contains("symlink"));
            assert_eq!(fs::read(&external).unwrap(), before);
            fs::remove_dir_all(root).ok();
            fs::remove_dir_all(external.parent().unwrap()).ok();
        }

        let (root, _) = db_only_room("target-symlink");
        let external = unique_root("target-symlink-external").join("target");
        fs::write(&external, b"external target").unwrap();
        symlink(&external, root.join(".rally/log/alpha.jsonl")).unwrap();
        let before = fs::read(&external).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&external).unwrap(), before);
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(external.parent().unwrap()).ok();

        let (root, _) = db_only_room("temp-symlink");
        let rally = root.join(".rally");
        arm_db_only_migration_fault(&rally, DbOnlyMigrationFaultPoint::AfterMarkerSync);
        run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME)).unwrap(),
        )
        .unwrap();
        let temp = root.join(marker["temp_relative_path"].as_str().unwrap());
        let external = unique_root("temp-symlink-external").join("temp");
        fs::write(&external, b"external temp").unwrap();
        symlink(&external, &temp).unwrap();
        let before = fs::read(&external).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&external).unwrap(), before);
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(external.parent().unwrap()).ok();
    }

    #[test]
    fn db_only_migration_rejects_marker_collision_or_binding_mismatch() {
        let (root, _) = db_only_room("marker-collision");
        let marker = root.join(".rally").join(DB_ONLY_MIGRATION_MARKER_FILENAME);
        fs::write(&marker, br#"{"schema":"unknown.migration.v9"}"#).unwrap();
        let marker_before = fs::read(&marker).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("marker"));
        assert_eq!(fs::read(&marker).unwrap(), marker_before);
        assert!(jsonl_files(&root).is_empty());
        fs::remove_dir_all(root).ok();

        let (root, _) = db_only_room("marker-binding");
        arm_db_only_migration_fault(
            &root.join(".rally"),
            DbOnlyMigrationFaultPoint::AfterMarkerSync,
        );
        run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        let marker = root.join(".rally").join(DB_ONLY_MIGRATION_MARKER_FILENAME);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        value["db"]["sha256"] = serde_json::Value::String("00".repeat(32));
        fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();
        let altered = fs::read(&marker).unwrap();
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("binding mismatch"));
        assert_eq!(fs::read(&marker).unwrap(), altered);
        assert!(jsonl_files(&root).is_empty());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_retries_prepared_marker_and_temp_idempotently() {
        for (label, point) in [
            ("after-marker", DbOnlyMigrationFaultPoint::AfterMarkerSync),
            ("after-temp", DbOnlyMigrationFaultPoint::AfterTempSync),
            ("after-verify", DbOnlyMigrationFaultPoint::AfterTempReadback),
            (
                "after-revalidate",
                DbOnlyMigrationFaultPoint::AfterDbRevalidation,
            ),
        ] {
            let (root, _) = db_only_room(label);
            let rally = root.join(".rally");
            arm_db_only_migration_fault(&rally, point);
            let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
            assert!(error.to_string().contains("prepared"));
            assert!(rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME).is_file());
            assert!(jsonl_files(&root).is_empty());
            let report = run_db_only_migration_at(&root, "alpha", true).unwrap();
            assert_eq!(report.state, DbOnlyMigrationState::Committed);
            assert_eq!(jsonl_files(&root).len(), 1);
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn db_only_migration_repairs_only_unpublished_marker_bound_temp_prefix() {
        let (root, _) = db_only_room("temp-prefix-repair");
        let rally = root.join(".rally");
        arm_db_only_migration_fault(&rally, DbOnlyMigrationFaultPoint::AfterMarkerSync);
        run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        let marker: DbOnlyMigrationMarker = read_json_file(
            &rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME),
            "migration marker",
        )
        .unwrap();
        let (candidate, _) = inspect_closed_db_candidate(&rally.join("facts.db"), "alpha").unwrap();
        let temp = root.join(&marker.temp_relative_path);
        let prefix_len = candidate.bytes.len() / 2;
        fs::write(&temp, &candidate.bytes[..prefix_len]).unwrap();
        let report = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::Committed);
        assert_eq!(fs::read(&report.target_path).unwrap(), candidate.bytes);

        let (linked_root, _) = db_only_room("linked-temp-no-repair");
        let linked_rally = linked_root.join(".rally");
        arm_db_only_migration_fault(
            &linked_rally,
            DbOnlyMigrationFaultPoint::AfterTargetInstallBeforeDirectorySync,
        );
        run_db_only_migration_at(&linked_root, "alpha", true).unwrap_err();
        let marker: DbOnlyMigrationMarker = read_json_file(
            &linked_rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME),
            "migration marker",
        )
        .unwrap();
        let temp = linked_root.join(&marker.temp_relative_path);
        let target = linked_root.join(&marker.target_relative_path);
        let shortened = fs::metadata(&temp).unwrap().len() - 1;
        OpenOptions::new()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_len(shortened)
            .unwrap();
        let before = fs::read(&target).unwrap();
        let error = run_db_only_migration_at(&linked_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("target"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), before);
        assert_eq!(fs::read(&temp).unwrap(), before);

        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(linked_root).ok();
    }

    #[test]
    fn db_only_migration_recovers_unknown_target_install_exactly_once() {
        let (root, _) = db_only_room("after-target");
        let rally = root.join(".rally");
        arm_db_only_migration_fault(
            &rally,
            DbOnlyMigrationFaultPoint::AfterTargetInstallBeforeDirectorySync,
        );
        let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("outcome unknown"));
        assert_eq!(jsonl_files(&root).len(), 1);
        assert!(rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME).is_file());
        let first = fs::read(&jsonl_files(&root)[0]).unwrap();
        let report = run_db_only_migration_at(&root, "alpha", true).unwrap();
        assert_eq!(report.state, DbOnlyMigrationState::Committed);
        assert_eq!(jsonl_files(&root).len(), 1);
        assert_eq!(fs::read(&report.target_path).unwrap(), first);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_recovers_committed_cleanup_pending() {
        for (label, point) in [
            (
                "after-dir-sync",
                DbOnlyMigrationFaultPoint::AfterTargetDirectorySync,
            ),
            ("after-receipt", DbOnlyMigrationFaultPoint::AfterReceiptSync),
            (
                "after-marker-remove",
                DbOnlyMigrationFaultPoint::AfterMarkerRemoval,
            ),
        ] {
            let (root, _) = db_only_room(label);
            let rally = root.join(".rally");
            arm_db_only_migration_fault(&rally, point);
            let error = run_db_only_migration_at(&root, "alpha", true).unwrap_err();
            assert!(error.to_string().contains("committed"));
            assert_eq!(jsonl_files(&root).len(), 1);
            let report = run_db_only_migration_at(&root, "alpha", true).unwrap();
            let expected_state = if point == DbOnlyMigrationFaultPoint::AfterTargetDirectorySync {
                DbOnlyMigrationState::Committed
            } else {
                DbOnlyMigrationState::AlreadyCommitted
            };
            assert_eq!(report.state, expected_state);
            assert!(!report.marker_path.exists());
            assert!(report.receipt_path.is_file());
            assert_eq!(jsonl_files(&root).len(), 1);
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn db_only_migration_marker_fences_direct_open_until_doctor_recovers() {
        for (label, point) in [
            (
                "direct-fence-pre-receipt",
                DbOnlyMigrationFaultPoint::AfterTargetDirectorySync,
            ),
            (
                "direct-fence-cleanup-pending",
                DbOnlyMigrationFaultPoint::AfterReceiptSync,
            ),
        ] {
            let (root, _) = db_only_room(label);
            let rally = root.join(".rally");
            arm_db_only_migration_fault(&rally, point);
            run_db_only_migration_at(&root, "alpha", true).unwrap_err();
            let before = file_tree(&root);

            let error = match DirectRoomStore::open_direct_at_with_engagement(
                root.clone(),
                Some("alpha".to_string()),
            ) {
                Ok(_) => panic!("ordinary direct open must defer to doctor while a marker exists"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("migrate-db-only"), "{error}");
            assert_eq!(
                file_tree(&root),
                before,
                "the fenced direct open must not reconcile or mutate any migration evidence"
            );

            let recovered = run_db_only_migration_at(&root, "alpha", true).unwrap();
            let expected_state = if point == DbOnlyMigrationFaultPoint::AfterTargetDirectorySync {
                DbOnlyMigrationState::Committed
            } else {
                DbOnlyMigrationState::AlreadyCommitted
            };
            assert_eq!(recovered.state, expected_state);
            assert!(!recovered.marker_path.exists());

            let direct = DirectRoomStore::open_direct_at_with_engagement(
                root.clone(),
                Some("alpha".to_string()),
            )
            .unwrap();
            let appended = direct
                .append_fact(&fact(&format!("after-recovery-{label}"), 3))
                .unwrap();
            assert!(appended.committed);
            drop(direct);
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn db_only_migration_marker_or_stage_fences_direct_open() {
        let root = unique_root("malformed-marker-fence");
        let store = DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("alpha".to_string()),
        )
        .unwrap();
        store.append_fact(&fact("marker-fence-seed", 1)).unwrap();
        drop(store);
        let marker = root.join(".rally").join(DB_ONLY_MIGRATION_MARKER_FILENAME);
        fs::write(&marker, b"not valid marker JSON").unwrap();
        let before = file_tree(&root);
        let error = match DirectRoomStore::open_direct_at(root.clone()) {
            Ok(_) => panic!("malformed marker must fence direct open"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("migrate-db-only"), "{error}");
        assert_eq!(file_tree(&root), before);

        fs::remove_file(&marker).unwrap();
        let outside = root.join("outside-marker");
        fs::write(&outside, b"external evidence").unwrap();
        std::os::unix::fs::symlink(&outside, &marker).unwrap();
        let before = file_tree(&root);
        let error = match DirectRoomStore::open_direct_at(root.clone()) {
            Ok(_) => panic!("symlink marker must fence direct open"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("migrate-db-only"), "{error}");
        assert_eq!(file_tree(&root), before);
        assert_eq!(fs::read(&outside).unwrap(), b"external evidence");

        fs::remove_file(&marker).unwrap();
        let marker_stage = root
            .join(".rally")
            .join(DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME);
        fs::write(&marker_stage, b"prepared marker staging evidence").unwrap();
        let before = file_tree(&root);
        let error = match DirectRoomStore::open_direct_at(root.clone()) {
            Ok(_) => panic!("marker staging evidence must fence direct open"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("migrate-db-only"), "{error}");
        assert_eq!(
            file_tree(&root),
            before,
            "the stage-file fence must not reconcile the DB or mutate recovery evidence"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn db_only_migration_preserves_mismatched_temp_and_receipt_evidence() {
        let (temp_root, _) = db_only_room("temp-mismatch");
        let rally = temp_root.join(".rally");
        arm_db_only_migration_fault(&rally, DbOnlyMigrationFaultPoint::AfterMarkerSync);
        run_db_only_migration_at(&temp_root, "alpha", true).unwrap_err();
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(rally.join(DB_ONLY_MIGRATION_MARKER_FILENAME)).unwrap(),
        )
        .unwrap();
        let temp = temp_root.join(marker["temp_relative_path"].as_str().unwrap());
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .unwrap();
        file.write_all(b"wrong temp evidence").unwrap();
        file.sync_all().unwrap();
        let error = run_db_only_migration_at(&temp_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("temp"));
        assert_eq!(fs::read(&temp).unwrap(), b"wrong temp evidence");
        assert!(jsonl_files(&temp_root).is_empty());
        fs::remove_dir_all(temp_root).ok();

        let (receipt_root, _) = db_only_room("receipt-mismatch");
        let rally = receipt_root.join(".rally");
        arm_db_only_migration_fault(&rally, DbOnlyMigrationFaultPoint::AfterTargetDirectorySync);
        run_db_only_migration_at(&receipt_root, "alpha", true).unwrap_err();
        let receipt = rally.join(DB_ONLY_MIGRATION_RECEIPT_FILENAME);
        fs::write(&receipt, br#"{"schema":"wrong.receipt.v9"}"#).unwrap();
        let before = fs::read(&receipt).unwrap();
        let error = run_db_only_migration_at(&receipt_root, "alpha", true).unwrap_err();
        assert!(error.to_string().contains("receipt"));
        assert_eq!(fs::read(&receipt).unwrap(), before);
        assert_eq!(jsonl_files(&receipt_root).len(), 1);
        fs::remove_dir_all(receipt_root).ok();
    }
}
