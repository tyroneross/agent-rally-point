// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — diagnostics and remediation for path hygiene, room registry, and stale state.
//!
//! Six independent modes:
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply
//!   --reap-stale       reap over-TTL in-room claims and stale lead leases (dry-run; commit with --apply)
//!   --sweep-corrupt    sweep disposable facts.db.corrupt.* snapshots (dry-run; remove with --apply)
//!   --compact-log      render a diagnostic log with presence/heartbeat runs collapsed into counts
//!   --binary-skew      compare the RUNNING binary's build stamp against this repo's HEAD

use schemars::JsonSchema;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discovery::{
    DiscoveryWarning, KnownRoom, RoomIndex, read_room_index_at, room_index_path,
    write_room_index_at,
};
use crate::error::Result;
use crate::store::RoomStore;
use crate::{normalize_path, paths_suffix_collide};

/// Prefix of every quarantined snapshot the store writes on corrupt-db detection.
const CORRUPT_PREFIX: &str = "facts.db.corrupt.";
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
    /// Snapshots swept (removed with --apply; listed only in dry-run).
    pub(crate) swept: Vec<CorruptSnapshot>,
    /// Bytes reclaimed (--apply) or reclaimable (dry-run) by the swept set.
    pub(crate) bytes_reclaimable: u64,
    /// Whether files were actually removed (`--apply`).
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

    if apply {
        for snap in &swept {
            for name in &snap.files {
                let path = dir.join(name);
                if let Err(e) = fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warnings.push(DiscoveryWarning {
                        code: "sweep_remove_failed".to_string(),
                        message: format!("cannot remove {}: {e}", path.display()),
                        path: Some(path.clone()),
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
        bytes_reclaimable,
        applied: apply,
        keep,
        max_age_days,
        warnings,
    }
}

/// `rally doctor --sweep-corrupt`: resolve the current room's `.rally` dir and
/// sweep disposable `facts.db.corrupt.*` snapshots under the retention policy.
pub(crate) fn run_sweep_corrupt(
    keep: Option<i64>,
    max_age_days: Option<i64>,
    apply: bool,
) -> Result<SweepCorruptReport> {
    let room = RoomStore::open()?;
    let dir = room.rally_dir();
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
    fn sweep_dry_run_reports_then_apply_removes() {
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

        fs::remove_dir_all(&dir).ok();
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
