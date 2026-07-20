// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — diagnostics and remediation for path hygiene, room registry, and stale state.
//!
//! Five independent modes:
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply
//!   --reap-stale       reap over-TTL in-room claims and stale lead leases (dry-run; commit with --apply)
//!   --sweep-corrupt    sweep disposable facts.db.corrupt.* snapshots (dry-run; remove with --apply)
//!   --compact-log      render a diagnostic log with presence/heartbeat runs collapsed into counts

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
    pub(crate) tool: Option<String>,
    pub(crate) subject: Option<String>,
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
            unparseable_lines += 1;
            continue;
        };
        let Some(event_type) = v.get("event_type").and_then(|e| e.as_str()) else {
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
