// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally doctor` — diagnostics and remediation for path hygiene, room registry, and stale state.
//!
//! Four independent modes:
//!   --canonical-paths  scan active claims for non-canonical scopes and suffix collisions
//!   --prune-rooms      classify registry entries as live/stale; remove stale ones with --apply
//!   --reap-stale       reap over-TTL in-room claims and stale lead leases (dry-run; commit with --apply)
//!   --sweep-corrupt    sweep disposable facts.db.corrupt.* snapshots (dry-run; remove with --apply)

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
