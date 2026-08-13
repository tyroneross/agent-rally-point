// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally rotate` — move old per-engagement segments from `.rally/log/`
//! into `.rally/archive/` so the active log dir stays bounded.
//!
//! **Eligibility rule.** A segment is rotatable if EVERY line in it has
//! `occurred_at` older than the rotation threshold (default 90 days). One
//! recent line keeps the whole segment live — partial rotation of a
//! single segment file would split events across live/archive in ways
//! that needlessly complicate replay's seq dedup.
//!
//! **Threshold resolution** (priority order):
//! 1. Explicit `--days <N>` flag on the CLI.
//! 2. `RALLY_ROTATE_DAYS` env var.
//! 3. `.rally/manifest.json` `rotate_threshold_days` field.
//! 4. Built-in default = 90.
//!
//! **Safety.** Rotation holds the room mutation lock across strict canonical
//! preflight and durable installation. It hard-links a no-clobber archive
//! target, syncs the target and archive directory, then unlinks the live name
//! and syncs the log directory. Replay continues to union both locations.
//!
//! **Idempotency.** Re-running `rally rotate` on already-rotated state is
//! a no-op. An exact existing archive copy completes an interrupted unlink;
//! differing content advances to a no-clobber generational name.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};
use crate::store::{
    ARCHIVE_DIRNAME, ARCHIVED_MONOLITH_FILENAME, LOG_DIRNAME, acquire_room_mutation_lock,
    ensure_new_mutation_can_start, rotation_segment_occurred_at_values,
};

const DEFAULT_THRESHOLD_DAYS: i64 = 90;
const THRESHOLD_ENV_VAR: &str = "RALLY_ROTATE_DAYS";
const MANIFEST_THRESHOLD_FIELD: &str = "rotate_threshold_days";

#[cfg(test)]
struct RotationPause {
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static ROTATION_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, RotationPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

#[cfg(test)]
static ROTATION_PRE_LINK_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, RotationPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

#[cfg(test)]
static ROTATION_SOURCE_LINK_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, RotationPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

#[cfg(test)]
static FAIL_AFTER_ARCHIVE_SYNC: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));

#[cfg(test)]
static FAIL_PARENT_SYNC: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));

#[cfg(test)]
fn pause_rotation_before_install_once(
    rally_dir: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let replaced = ROTATION_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            rally_dir.to_path_buf(),
            RotationPause {
                reached: reached_tx,
                resume: resume_rx,
            },
        );
    assert!(replaced.is_none(), "rotation pause already armed for path");
    (reached_rx, resume_tx)
}

#[cfg(test)]
fn pause_rotation_before_install_if_armed(rally_dir: &Path) {
    let pause = ROTATION_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(rally_dir);
    if let Some(pause) = pause {
        pause.reached.send(()).unwrap();
        pause
            .resume
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}

#[cfg(not(test))]
fn pause_rotation_before_install_if_armed(_rally_dir: &Path) {}

#[cfg(test)]
fn pause_rotation_before_link_once(
    rally_dir: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let replaced = ROTATION_PRE_LINK_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            rally_dir.to_path_buf(),
            RotationPause {
                reached: reached_tx,
                resume: resume_rx,
            },
        );
    assert!(replaced.is_none(), "rotation pre-link pause already armed");
    (reached_rx, resume_tx)
}

#[cfg(test)]
fn pause_rotation_before_link_if_armed(rally_dir: &Path) {
    let pause = ROTATION_PRE_LINK_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(rally_dir);
    if let Some(pause) = pause {
        pause.reached.send(()).unwrap();
        pause
            .resume
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}

#[cfg(not(test))]
fn pause_rotation_before_link_if_armed(_rally_dir: &Path) {}

#[cfg(test)]
fn pause_rotation_before_source_link_once(
    source: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let replaced = ROTATION_SOURCE_LINK_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            source.to_path_buf(),
            RotationPause {
                reached: reached_tx,
                resume: resume_rx,
            },
        );
    assert!(
        replaced.is_none(),
        "rotation source-link pause already armed"
    );
    (reached_rx, resume_tx)
}

#[cfg(test)]
fn pause_rotation_before_source_link_if_armed(source: &Path) {
    let pause = ROTATION_SOURCE_LINK_PAUSES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(source);
    if let Some(pause) = pause {
        pause.reached.send(()).unwrap();
        pause
            .resume
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}

#[cfg(not(test))]
fn pause_rotation_before_source_link_if_armed(_source: &Path) {}

#[cfg(test)]
fn fail_rotation_after_archive_sync_once(rally_dir: &Path) {
    let inserted = FAIL_AFTER_ARCHIVE_SYNC
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(rally_dir.to_path_buf());
    assert!(inserted, "rotation fault already armed for path");
}

#[cfg(test)]
fn fail_after_archive_sync_if_armed(rally_dir: &Path) -> Result<()> {
    let armed = FAIL_AFTER_ARCHIVE_SYNC
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(rally_dir);
    if armed {
        return Err(RallyError::Message(format!(
            "injected rotation failure after durable archive sync for {}",
            rally_dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_after_archive_sync_if_armed(_rally_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
fn fail_rotation_parent_sync_once(rally_dir: &Path) {
    let inserted = FAIL_PARENT_SYNC
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(rally_dir.to_path_buf());
    assert!(
        inserted,
        "rotation parent-sync fault already armed for path"
    );
}

#[cfg(test)]
fn fail_parent_sync_if_armed(rally_dir: &Path) -> Result<()> {
    if FAIL_PARENT_SYNC
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(rally_dir)
    {
        return Err(RallyError::Message(format!(
            "injected rotation parent-directory sync failure for {}",
            rally_dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_parent_sync_if_armed(_rally_dir: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct RotatedSegment {
    pub(crate) segment: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) reason: &'static str, // "rotated" | "would_rotate" (dry-run)
}

#[derive(Debug, Serialize)]
pub(crate) struct SkippedSegment {
    pub(crate) segment: String,
    pub(crate) reason: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RotateOutcome {
    pub(crate) threshold_days: i64,
    pub(crate) threshold_source: &'static str, // "flag" | "env" | "manifest" | "default"
    pub(crate) cutoff_utc: String,
    pub(crate) dry_run: bool,
    pub(crate) rotated: Vec<RotatedSegment>,
    pub(crate) skipped: Vec<SkippedSegment>,
    pub(crate) live_segment_count_before: usize,
    pub(crate) live_segment_count_after: usize,
}

/// Resolve the rotation threshold using the documented priority order.
fn resolve_threshold(flag_days: Option<i64>, repo_root: &Path) -> (i64, &'static str) {
    if let Some(days) = flag_days {
        return (days, "flag");
    }
    if let Ok(raw) = env::var(THRESHOLD_ENV_VAR)
        && let Ok(days) = raw.trim().parse::<i64>()
        && days > 0
    {
        return (days, "env");
    }
    let manifest_path = repo_root.join(".rally").join("manifest.json");
    if let Ok(text) = fs::read_to_string(&manifest_path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(days) = value.get(MANIFEST_THRESHOLD_FIELD).and_then(|v| v.as_i64())
        && days > 0
    {
        return (days, "manifest");
    }
    (DEFAULT_THRESHOLD_DAYS, "default")
}

/// Walk every line of a segment and return the **maximum** (latest)
/// occurred_at timestamp it carries. `None` if the segment is empty or
/// contains no parseable timestamps.
fn segment_max_occurred_at(path: &Path) -> Result<Option<DateTime<Utc>>> {
    let mut latest: Option<DateTime<Utc>> = None;
    for ts_str in rotation_segment_occurred_at_values(path)? {
        let ts = DateTime::parse_from_rfc3339(&ts_str).map_err(|error| {
            RallyError::Message(format!(
                "canonical segment {} has invalid occurred_at {:?}: {}",
                path.display(),
                ts_str,
                error
            ))
        })?;
        let ts_utc = ts.with_timezone(&Utc);
        latest = Some(latest.map(|cur| cur.max(ts_utc)).unwrap_or(ts_utc));
    }
    Ok(latest)
}

fn sync_file(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(RallyError::io(format!("fsync {}", path.display())))
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(RallyError::io(format!(
            "fsync directory {}",
            path.display()
        )))
}

fn sync_rotation_parent(rally_dir: &Path) -> Result<()> {
    fail_parent_sync_if_armed(rally_dir)?;
    sync_directory(rally_dir)
}

fn files_identical(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = fs::metadata(left).map_err(RallyError::io(format!(
        "stat rotation source {}",
        left.display()
    )))?;
    let right_meta = fs::metadata(right).map_err(RallyError::io(format!(
        "stat rotation target {}",
        right.display()
    )))?;
    if !left_meta.is_file() || !right_meta.is_file() {
        return Err(RallyError::Message(format!(
            "rotation source/target must be regular files: {} and {}",
            left.display(),
            right.display()
        )));
    }
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_file =
        fs::File::open(left).map_err(RallyError::io(format!("read {}", left.display())))?;
    let mut right_file =
        fs::File::open(right).map_err(RallyError::io(format!("read {}", right.display())))?;
    let mut left_bytes = [0_u8; 16 * 1024];
    let mut right_bytes = [0_u8; 16 * 1024];
    loop {
        let left_read = left_file
            .read(&mut left_bytes)
            .map_err(RallyError::io(format!("read {}", left.display())))?;
        let right_read = right_file
            .read(&mut right_bytes)
            .map_err(RallyError::io(format!("read {}", right.display())))?;
        if left_read != right_read || left_bytes[..left_read] != right_bytes[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

#[derive(Debug)]
enum ArchiveTarget {
    Missing(PathBuf),
    ExactCopy(PathBuf),
}

impl ArchiveTarget {
    fn path(&self) -> &Path {
        match self {
            Self::Missing(path) | Self::ExactCopy(path) => path,
        }
    }
}

fn archive_target_name(segment: &str, generation: u32) -> Result<String> {
    if generation == 0 {
        return Ok(segment.to_string());
    }
    let stem = segment.strip_suffix(".jsonl").ok_or_else(|| {
        RallyError::Message(format!("rotation source is not a JSONL segment: {segment}"))
    })?;
    Ok(format!("{stem}.rotated-{generation:04}.jsonl"))
}

fn choose_archive_target(
    source: &Path,
    archive_dir: &Path,
    segment: &str,
) -> Result<ArchiveTarget> {
    // This exact archive name is the one-time R5 migration backup and replay
    // excludes it. A valid engagement with the same label must therefore
    // start at generation 1 so its canonical rows remain replayable.
    let mut generation = u32::from(segment == ARCHIVED_MONOLITH_FILENAME);
    loop {
        let candidate = archive_dir.join(archive_target_name(segment, generation)?);
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ArchiveTarget::Missing(candidate));
            }
            Err(error) => {
                return Err(RallyError::io(format!("stat {}", candidate.display()))(
                    error,
                ));
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(RallyError::Message(format!(
                    "rotation archive collision is not a regular file: {}",
                    candidate.display()
                )));
            }
            Ok(_) if files_identical(source, &candidate)? => {
                return Ok(ArchiveTarget::ExactCopy(candidate));
            }
            Ok(_) => {
                generation = generation.checked_add(1).ok_or_else(|| {
                    RallyError::Message(format!(
                        "rotation archive generation overflow for {segment}"
                    ))
                })?;
            }
        }
    }
}

fn durable_install_rotation(
    rally_dir: &Path,
    log_dir: &Path,
    archive_dir: &Path,
    source: &Path,
    segment: &str,
    mut planned_target: ArchiveTarget,
    mutation_started: &mut bool,
) -> Result<PathBuf> {
    let target = loop {
        match planned_target {
            ArchiveTarget::ExactCopy(target) => {
                if !*mutation_started {
                    // Resync/unlink is the first effect for an interrupted
                    // exact-copy retry, so it owns the batch's final admission.
                    ensure_new_mutation_can_start(&target)?;
                    *mutation_started = true;
                }
                break target;
            }
            ArchiveTarget::Missing(target) => {
                pause_rotation_before_link_if_armed(rally_dir);
                pause_rotation_before_source_link_if_armed(source);
                if !*mutation_started {
                    // Target selection and byte comparisons can be unbounded.
                    // This is the final check immediately before the batch's
                    // first durable link. A started batch must finish instead
                    // of later returning the retry-safe NotStarted class.
                    ensure_new_mutation_can_start(&target)?;
                }
                match fs::hard_link(source, &target) {
                    Ok(()) => {
                        *mutation_started = true;
                        break target;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        planned_target = choose_archive_target(source, archive_dir, segment)?;
                        continue;
                    }
                    Err(error) => {
                        return Err(RallyError::io(format!(
                            "install rotation archive link {} -> {}",
                            source.display(),
                            target.display()
                        ))(error));
                    }
                }
            }
        }
    };

    sync_file(&target)?;
    sync_directory(archive_dir)?;
    fail_after_archive_sync_if_armed(rally_dir)?;
    fs::remove_file(source).map_err(RallyError::io(format!(
        "unlink rotated source {}",
        source.display()
    )))?;
    sync_directory(log_dir)?;
    Ok(target)
}

/// Enumerate `.jsonl` files in a dir (no tmp files), sorted by filename.
fn segment_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).map_err(RallyError::io(format!("read_dir {}", dir.display())))? {
        let entry = entry.map_err(RallyError::io(format!("readdir {}", dir.display())))?;
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

/// Run the rotation routine.
pub(crate) fn run_rotate(
    repo_root: PathBuf,
    flag_days: Option<i64>,
    dry_run: bool,
) -> Result<RotateOutcome> {
    let rally_dir = repo_root.join(".rally");
    let log_dir = rally_dir.join(LOG_DIRNAME);
    let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
    let (threshold_days, threshold_source) = resolve_threshold(flag_days, &repo_root);
    let cutoff = Utc::now() - Duration::days(threshold_days);
    let _mutation_guard = acquire_room_mutation_lock(&rally_dir)?;

    let live_before = segment_files(&log_dir)?;
    let mut rotated = Vec::new();
    let mut skipped = Vec::new();
    let mut plans = Vec::<(PathBuf, String, ArchiveTarget)>::new();

    // Full canonical preflight precedes every durable side effect. Archive
    // corruption must not allow a partial move of otherwise-valid live files.
    for archived in segment_files(&archive_dir)? {
        segment_max_occurred_at(&archived)?;
    }

    for path in &live_before {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let latest = match segment_max_occurred_at(path)? {
            Some(ts) => ts,
            None => {
                skipped.push(SkippedSegment {
                    segment: name,
                    reason: "no parseable timestamp".to_string(),
                });
                continue;
            }
        };
        if latest > cutoff {
            // Segment has at least one entry newer than the cutoff. Keep it
            // live in its entirety — partial rotation of a single segment
            // would split events across live/archive and break the seq
            // contiguity assumed by `rebuild_db_from_segments`.
            continue;
        }
        let target = choose_archive_target(path, &archive_dir, &name)?;
        plans.push((path.clone(), name, target));
    }

    if dry_run {
        for (source, segment, target) in &plans {
            rotated.push(RotatedSegment {
                segment: segment.clone(),
                from: source.display().to_string(),
                to: target.path().display().to_string(),
                reason: "would_rotate",
            });
        }
    } else if !plans.is_empty() {
        pause_rotation_before_install_if_armed(&rally_dir);
        ensure_new_mutation_can_start(&archive_dir)?;
        let mut mutation_started = false;
        if !archive_dir.exists() {
            fs::create_dir(&archive_dir)
                .map_err(RallyError::io(format!("create {}", archive_dir.display())))?;
            mutation_started = true;
        } else if !archive_dir.is_dir() {
            return Err(RallyError::Message(format!(
                "rotation archive path is not a directory: {}",
                archive_dir.display()
            )));
        }
        // Persist or re-prove the archive-directory entry on every attempt.
        // A prior create may have returned before its parent fsync completed.
        sync_rotation_parent(&rally_dir)?;

        for (source, segment, planned_target) in plans {
            let target = durable_install_rotation(
                &rally_dir,
                &log_dir,
                &archive_dir,
                &source,
                &segment,
                planned_target,
                &mut mutation_started,
            )?;
            rotated.push(RotatedSegment {
                segment,
                from: source.display().to_string(),
                to: target.display().to_string(),
                reason: "rotated",
            });
        }
    }

    let live_after = segment_files(&log_dir)?;
    Ok(RotateOutcome {
        threshold_days,
        threshold_source,
        cutoff_utc: cutoff.to_rfc3339(),
        dry_run,
        rotated,
        skipped,
        live_segment_count_before: live_before.len(),
        live_segment_count_after: live_after.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FACT_SCHEMA;
    use crate::now_string;
    use crate::store::{Fact, FactKind, RoomStore, with_mutation_deadline};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-rotate-{label}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_fact(seq: i64, subject: &str) -> Fact {
        Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: format!("e{seq}"),
            seq,
            thread_id: format!("t-{seq}"),
            kind: FactKind::Decision,
            tool: Some("test".to_string()),
            role: None,
            subject: subject.to_string(),
            scope: vec!["src/".to_string()],
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

    /// Write a synthetic old segment directly to disk (bypassing
    /// RoomStore) so we can pick the timestamp. Each line gets the same
    /// timestamp and seq. The cache rebuilds when RoomStore::open_at
    /// reads it.
    fn write_segment(log_dir: &Path, label: &str, occurred_at: &str, seqs: &[i64]) {
        write_segment_with_engagement(log_dir, label, label, occurred_at, seqs);
    }

    fn write_segment_with_engagement(
        log_dir: &Path,
        file_stem: &str,
        engagement: &str,
        occurred_at: &str,
        seqs: &[i64],
    ) {
        write_segment_with_times(
            log_dir,
            file_stem,
            Some(engagement),
            occurred_at,
            occurred_at,
            seqs,
        );
    }

    fn write_segment_with_times(
        log_dir: &Path,
        file_stem: &str,
        engagement: Option<&str>,
        occurred_at: &str,
        created_at: &str,
        seqs: &[i64],
    ) {
        fs::create_dir_all(log_dir).unwrap();
        let path = log_dir.join(format!("{file_stem}.jsonl"));
        let mut content = String::new();
        for &seq in seqs {
            let mut line = serde_json::json!({
                "seq": seq,
                "occurred_at": occurred_at,
                "event_type": "decision",
                "payload": {
                    "schema": FACT_SCHEMA,
                    "event_id": format!("e{seq}"),
                    "seq": seq,
                    "thread_id": format!("t-{seq}"),
                    "kind": "decision",
                    "subject": format!("subject {seq}"),
                    "scope": ["src/"],
                    "tool": "test",
                    "created_at": created_at,
                },
            });
            if let Some(engagement) = engagement {
                line["engagement"] = serde_json::Value::String(engagement.to_string());
            }
            content.push_str(&line.to_string());
            content.push('\n');
        }
        fs::write(&path, content).unwrap();
    }

    #[test]
    fn o26_rotation_rejects_incomplete_tail_without_moving_bytes() {
        let root = unique_root("rotation-incomplete-tail");
        let log_dir = root.join(".rally/log");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let mut original = fs::read(&source).unwrap();
        original.extend_from_slice(br#"{"seq":2,"occurred_at":"#);
        fs::write(&source, &original).unwrap();

        let error = run_rotate(root.clone(), Some(90), false)
            .expect_err("an incomplete canonical tail must block rotation");
        assert!(error.to_string().contains("canonical"));
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(!root.join(".rally/archive/alpha.jsonl").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_rejects_completed_corruption_without_partial_moves() {
        let root = unique_root("rotation-completed-corruption");
        let log_dir = root.join(".rally/log");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        write_segment(&log_dir, "beta", &old_ts, &[2]);
        let corrupt = log_dir.join("beta.jsonl");
        let mut corrupt_bytes = fs::read(&corrupt).unwrap();
        corrupt_bytes.extend_from_slice(b"not-json\n");
        fs::write(&corrupt, &corrupt_bytes).unwrap();

        run_rotate(root.clone(), Some(90), false)
            .expect_err("completed corruption must fail before moving alpha");
        assert!(log_dir.join("alpha.jsonl").exists());
        assert_eq!(fs::read(&corrupt).unwrap(), corrupt_bytes);
        assert!(!root.join(".rally/archive").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_accepts_valid_final_record_without_newline() {
        let root = unique_root("rotation-valid-no-newline");
        let log_dir = root.join(".rally/log");
        let archive_dir = root.join(".rally/archive");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let mut source_bytes = fs::read(&source).unwrap();
        assert_eq!(source_bytes.pop(), Some(b'\n'));
        fs::write(&source, &source_bytes).unwrap();

        let outcome = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(outcome.rotated.len(), 1);
        assert!(!source.exists());
        assert_eq!(
            fs::read(archive_dir.join("alpha.jsonl")).unwrap(),
            source_bytes
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_rejects_wrong_schema_archive_before_live_move() {
        let root = unique_root("rotation-wrong-schema-archive");
        let log_dir = root.join(".rally/log");
        let archive_dir = root.join(".rally/archive");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "beta", &old_ts, &[2]);
        write_segment(&archive_dir, "alpha", &old_ts, &[1]);
        let live = log_dir.join("beta.jsonl");
        let live_bytes = fs::read(&live).unwrap();
        let corrupt = archive_dir.join("alpha.jsonl");
        let line = fs::read_to_string(&corrupt).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        value["payload"]["schema"] = serde_json::Value::String("unsupported.fact".to_string());
        let corrupt_bytes = value.to_string().into_bytes();
        fs::write(&corrupt, &corrupt_bytes).unwrap();

        let error = run_rotate(root.clone(), Some(90), false)
            .expect_err("a complete wrong-schema archive row must block rotation");
        assert!(error.to_string().contains("unsupported fact schema"));
        assert_eq!(fs::read(&live).unwrap(), live_bytes);
        assert_eq!(fs::read(&corrupt).unwrap(), corrupt_bytes);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_rejects_invalid_archive_timestamp_before_live_move() {
        let root = unique_root("rotation-invalid-archive-timestamp");
        let log_dir = root.join(".rally/log");
        let archive_dir = root.join(".rally/archive");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "beta", &old_ts, &[2]);
        write_segment(&archive_dir, "alpha", &old_ts, &[1]);
        let live = log_dir.join("beta.jsonl");
        let live_bytes = fs::read(&live).unwrap();
        let corrupt = archive_dir.join("alpha.jsonl");
        let line = fs::read_to_string(&corrupt).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        value["occurred_at"] = serde_json::Value::String("not-rfc3339".to_string());
        let corrupt_bytes = format!("{}\n", value).into_bytes();
        fs::write(&corrupt, &corrupt_bytes).unwrap();

        let error = run_rotate(root.clone(), Some(90), false)
            .expect_err("an invalid archive timestamp must block every live move");
        assert!(error.to_string().contains("invalid occurred_at"));
        assert_eq!(fs::read(&live).unwrap(), live_bytes);
        assert_eq!(fs::read(&corrupt).unwrap(), corrupt_bytes);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_uses_generation_on_different_content_collision() {
        let root = unique_root("rotation-generation-collision");
        let log_dir = root.join(".rally/log");
        let archive_dir = root.join(".rally/archive");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&archive_dir, "alpha", &old_ts, &[1]);
        write_segment(&log_dir, "alpha", &old_ts, &[2]);

        let outcome = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(outcome.rotated.len(), 1);
        assert_eq!(outcome.rotated[0].segment, "alpha.jsonl");
        assert_eq!(
            Path::new(&outcome.rotated[0].to)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("alpha.rotated-0001.jsonl")
        );
        assert!(archive_dir.join("alpha.jsonl").exists());
        assert!(archive_dir.join("alpha.rotated-0001.jsonl").exists());
        assert!(!log_dir.join("alpha.jsonl").exists());

        // A later segment with the same engagement must advance again instead
        // of treating the first collision as a permanent skip condition.
        write_segment(&log_dir, "alpha", &old_ts, &[3]);
        let second = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(second.rotated.len(), 1);
        assert_eq!(
            Path::new(&second.rotated[0].to)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("alpha.rotated-0002.jsonl")
        );
        assert!(archive_dir.join("alpha.rotated-0002.jsonl").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_reserved_monolith_engagement_rotates_to_replayable_generation() {
        let root = unique_root("rotation-reserved-monolith-engagement");
        let log_dir = root.join(".rally/log");
        let archive_dir = root.join(".rally/archive");
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let current_ts = now_string();
        write_segment_with_times(
            &log_dir,
            "ledger-pre-segment",
            Some("ledger-pre-segment"),
            &old_ts,
            &current_ts,
            &[1],
        );

        let outcome = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(outcome.rotated.len(), 1);
        assert_eq!(
            Path::new(&outcome.rotated[0].to)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("ledger-pre-segment.rotated-0001.jsonl")
        );
        assert!(!archive_dir.join(ARCHIVED_MONOLITH_FILENAME).exists());
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert!(
            store
                .facts()
                .unwrap()
                .iter()
                .any(|fact| fact.event_id == "e1")
        );
        assert!(
            store
                .snapshot_scoped("ledger-pre-segment", None, None, false, false)
                .unwrap()
                .current_decisions
                .iter()
                .any(|fact| fact.event_id == "e1")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_scoped_replay_prefers_explicit_engagement_over_archive_filename() {
        let root = unique_root("rotation-scoped-stamp-wins");
        let archive_dir = root.join(".rally/archive");
        let current_ts = now_string();
        write_segment_with_times(&archive_dir, "legacy", None, &current_ts, &current_ts, &[1]);
        write_segment_with_times(
            &archive_dir,
            "alpha.rotated-0001",
            Some("alpha"),
            &current_ts,
            &current_ts,
            &[2],
        );

        let store = RoomStore::open_at(root.clone()).unwrap();
        assert!(
            store
                .snapshot_scoped("legacy", None, None, false, false)
                .unwrap()
                .current_decisions
                .iter()
                .any(|fact| fact.event_id == "e1"),
            "an unstamped legacy exact-name row must retain filename fallback"
        );
        assert!(
            store
                .snapshot_scoped("alpha.rotated-0001", None, None, false, false)
                .unwrap()
                .current_decisions
                .iter()
                .all(|fact| fact.event_id != "e2"),
            "an explicit alpha stamp must override its generational filename"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_scoped_replay_reads_generational_archives_by_row_engagement() {
        let root = unique_root("rotation-scoped-generations");
        let archive_dir = root.join(".rally/archive");
        // This gate isolates storage-location selection. Keep the decision
        // current so the unrelated recency projection cannot archive it.
        let current_ts = now_string();
        write_segment_with_engagement(
            &archive_dir,
            "alpha.rotated-0001",
            "alpha",
            &current_ts,
            &[1],
        );

        let store = RoomStore::open_at(root.clone()).unwrap();
        let snapshot = store
            .snapshot_scoped("alpha", None, None, false, false)
            .unwrap();
        assert!(
            snapshot
                .current_decisions
                .iter()
                .any(|fact| fact.event_id == "e1"),
            "scoped replay must select generational archives by LedgerLine.engagement"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_crash_after_durable_link_retries_by_exact_copy() {
        let root = unique_root("rotation-crash-after-link");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let source_bytes = fs::read(&source).unwrap();

        fail_rotation_after_archive_sync_once(&rally_dir);
        run_rotate(root.clone(), Some(90), false)
            .expect_err("injected crash must stop before source unlink");
        let target = archive_dir.join("alpha.jsonl");
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&target).unwrap(), source_bytes);

        let retry = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(retry.rotated.len(), 1);
        assert!(!source.exists());
        assert_eq!(fs::read(&target).unwrap(), source_bytes);
        let store = RoomStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store
                .facts()
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == "e1")
                .count(),
            1
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_serializes_append_and_same_id_retry_under_one_lock() {
        let root = unique_root("rotation-append-race");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        fs::write(
            rally_dir.join(crate::store::ACTIVE_ENGAGEMENT_FILENAME),
            "alpha\n",
        )
        .unwrap();

        let (rotation_reached, resume_rotation) = pause_rotation_before_install_once(&rally_dir);
        let rotate_root = root.clone();
        let rotate_handle = std::thread::spawn(move || run_rotate(rotate_root, Some(90), false));
        rotation_reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        let append_fact = make_fact(2, "append serialized behind rotation");
        let append_root = root.clone();
        let append_fact_for_thread = append_fact.clone();
        let (append_started_tx, append_started_rx) = std::sync::mpsc::channel();
        let (append_done_tx, append_done_rx) = std::sync::mpsc::channel();
        let append_handle = std::thread::spawn(move || {
            append_started_tx.send(()).unwrap();
            let result = RoomStore::open_at(append_root)
                .and_then(|store| store.append_fact(&append_fact_for_thread));
            append_done_tx.send(result).unwrap();
        });
        append_started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(
            append_done_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "append must wait while rotation holds the shared mutation lock"
        );

        resume_rotation.send(()).unwrap();
        let rotated = rotate_handle.join().unwrap().unwrap();
        assert_eq!(rotated.rotated.len(), 1);
        let appended = append_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        append_handle.join().unwrap();
        let retry = RoomStore::open_at(root.clone())
            .unwrap()
            .append_fact(&append_fact)
            .unwrap();
        assert_eq!(retry.fact.seq, appended.fact.seq);

        let facts = RoomStore::open_at(root.clone()).unwrap().facts().unwrap();
        assert_eq!(facts.len(), 2, "rotation+append must preserve the union");
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.event_id == append_fact.event_id)
                .count(),
            1,
            "same-id retry must remain singleton across rotation"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_rechecks_anchored_deadline_before_first_install() {
        let root = unique_root("rotation-late-start");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let source_bytes = fs::read(&source).unwrap();

        let (rotation_reached, resume_rotation) = pause_rotation_before_install_once(&rally_dir);
        let rotate_root = root.clone();
        let rotate_handle = std::thread::spawn(move || {
            with_mutation_deadline(std::time::Duration::from_millis(40), || {
                run_rotate(rotate_root, Some(90), false)
            })
        });
        rotation_reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(75));
        resume_rotation.send(()).unwrap();

        let error = rotate_handle
            .join()
            .unwrap()
            .expect_err("expired pre-install deadline must remain NotStarted");
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!rally_dir.join(ARCHIVE_DIRNAME).exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_rechecks_deadline_immediately_before_archive_link() {
        let root = unique_root("rotation-pre-link-late-start");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
        fs::create_dir_all(&archive_dir).unwrap();
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let source_bytes = fs::read(&source).unwrap();

        let (link_reached, resume_link) = pause_rotation_before_link_once(&rally_dir);
        let rotate_root = root.clone();
        let rotate_handle = std::thread::spawn(move || {
            with_mutation_deadline(std::time::Duration::from_millis(40), || {
                run_rotate(rotate_root, Some(90), false)
            })
        });
        link_reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(75));
        resume_link.send(()).unwrap();

        let error = rotate_handle
            .join()
            .unwrap()
            .expect_err("an expired pre-link deadline must remain NotStarted");
        assert!(matches!(error, RallyError::NotStarted(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!archive_dir.join("alpha.jsonl").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_never_returns_not_started_after_first_segment_commits() {
        let root = unique_root("rotation-multi-plan-started");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
        fs::create_dir_all(&archive_dir).unwrap();
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        write_segment(&log_dir, "beta", &old_ts, &[2]);
        let alpha_source = log_dir.join("alpha.jsonl");
        let beta_source = log_dir.join("beta.jsonl");

        let (second_link_reached, resume_second_link) =
            pause_rotation_before_source_link_once(&beta_source);
        let rotate_root = root.clone();
        let rotate_handle = std::thread::spawn(move || {
            with_mutation_deadline(std::time::Duration::from_millis(50), || {
                run_rotate(rotate_root, Some(90), false)
            })
        });
        second_link_reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(
            !alpha_source.exists() && archive_dir.join("alpha.jsonl").exists(),
            "the first source must already be durably installed and unlinked"
        );
        std::thread::sleep(std::time::Duration::from_millis(80));
        resume_second_link.send(()).unwrap();

        let outcome = rotate_handle
            .join()
            .unwrap()
            .expect("a started batch must finish instead of claiming NotStarted");
        assert_eq!(outcome.rotated.len(), 2);
        assert!(!beta_source.exists());
        assert!(archive_dir.join("beta.jsonl").exists());
        assert!(
            run_rotate(root.clone(), Some(90), false)
                .unwrap()
                .rotated
                .is_empty()
        );
        let facts = RoomStore::open_at(root.clone()).unwrap().facts().unwrap();
        assert_eq!(facts.len(), 2);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_rotation_retry_resyncs_existing_archive_parent_before_unlink() {
        let root = unique_root("rotation-parent-sync-retry");
        let rally_dir = root.join(".rally");
        let log_dir = rally_dir.join(LOG_DIRNAME);
        let archive_dir = rally_dir.join(ARCHIVE_DIRNAME);
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "alpha", &old_ts, &[1]);
        let source = log_dir.join("alpha.jsonl");
        let source_bytes = fs::read(&source).unwrap();

        fail_rotation_parent_sync_once(&rally_dir);
        run_rotate(root.clone(), Some(90), false)
            .expect_err("first parent sync failure must stop before archive install");
        assert!(archive_dir.is_dir());
        assert_eq!(fs::read(&source).unwrap(), source_bytes);

        fail_rotation_parent_sync_once(&rally_dir);
        run_rotate(root.clone(), Some(90), false)
            .expect_err("retry must resync the pre-existing archive parent before unlink");
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!archive_dir.join("alpha.jsonl").exists());

        let retry = run_rotate(root.clone(), Some(90), false).unwrap();
        assert_eq!(retry.rotated.len(), 1);
        assert!(!source.exists());
        fs::remove_dir_all(&root).ok();
    }

    /// Rotate an old segment, keep a recent one, confirm replay still
    /// reconstructs full history.
    #[test]
    fn rotates_old_segments_keeps_recent_replay_preserves_history() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
            env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("rotate-basic");

        // Bootstrap a real room first so facts.db exists and is consistent,
        // then layer the synthetic segments on top.
        let store = RoomStore::open_at(root.clone()).unwrap();
        store.append_fact(&make_fact(1, "real append")).unwrap();
        drop(store);

        // Remove the natural segment + cache so we control state.
        let log_dir = root.join(".rally/log");
        for entry in fs::read_dir(&log_dir).unwrap() {
            let _ = fs::remove_file(entry.unwrap().path());
        }
        let facts_db = root.join(".rally/facts.db");
        let _ = fs::remove_file(&facts_db);
        let _ = fs::remove_file(facts_db.with_extension("db-shm"));
        let _ = fs::remove_file(facts_db.with_extension("db-wal"));

        // Old segment (200 days ago): seqs 1, 2.
        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "2024-old", &old_ts, &[1, 2]);

        // Recent segment (today): seqs 3, 4.
        let new_ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "today", &new_ts, &[3, 4]);

        // Pre-rotation: 2 live segments, 0 archived, 4 events total.
        let pre_live = segment_files(&log_dir).unwrap();
        assert_eq!(pre_live.len(), 2);
        let archive_dir = root.join(".rally/archive");
        let pre_archive = segment_files(&archive_dir).unwrap_or_default();
        assert!(pre_archive.is_empty());

        // Rotate with 90-day threshold (default).
        let outcome = run_rotate(root.clone(), None, false).unwrap();
        assert_eq!(outcome.threshold_days, DEFAULT_THRESHOLD_DAYS);
        assert_eq!(outcome.threshold_source, "default");
        assert_eq!(outcome.rotated.len(), 1, "old segment should rotate");
        assert_eq!(outcome.rotated[0].segment, "2024-old.jsonl");
        assert!(outcome.skipped.is_empty());
        assert_eq!(outcome.live_segment_count_before, 2);
        assert_eq!(outcome.live_segment_count_after, 1);

        // Filesystem reflects the move.
        assert!(!log_dir.join("2024-old.jsonl").exists());
        assert!(archive_dir.join("2024-old.jsonl").exists());
        assert!(log_dir.join("today.jsonl").exists());

        // Replay still reconstructs full history (4 events).
        let store = RoomStore::open_at(root.clone()).unwrap();
        let facts = store.facts().unwrap();
        assert_eq!(facts.len(), 4, "replay must see archive + live");

        fs::remove_dir_all(&root).ok();
    }

    /// A segment with at least one fresh line stays live (no partial rotation).
    #[test]
    fn segment_with_recent_event_stays_live() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
            env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("rotate-mixed");
        let log_dir = root.join(".rally/log");
        fs::create_dir_all(&log_dir).unwrap();

        // Mixed segment: one ancient line + one recent line.
        let ancient_ts =
            (Utc::now() - Duration::days(365)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let recent_ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let path = log_dir.join("mixed.jsonl");
        let line_old = serde_json::json!({
            "seq": 1, "occurred_at": ancient_ts, "event_type": "decision",
            "payload": {"schema": FACT_SCHEMA, "event_id": "e1", "seq": 1, "kind": "decision", "subject": "old", "scope": [], "thread_id": "t1", "created_at": ancient_ts}
        }).to_string();
        let line_new = serde_json::json!({
            "seq": 2, "occurred_at": recent_ts, "event_type": "decision",
            "payload": {"schema": FACT_SCHEMA, "event_id": "e2", "seq": 2, "kind": "decision", "subject": "new", "scope": [], "thread_id": "t2", "created_at": recent_ts}
        }).to_string();
        fs::write(&path, format!("{line_old}\n{line_new}\n")).unwrap();

        let outcome = run_rotate(root.clone(), Some(90), false).unwrap();
        assert!(
            outcome.rotated.is_empty(),
            "mixed segment must not rotate; got {:?}",
            outcome.rotated
        );
        assert!(path.exists(), "mixed segment must remain live");

        fs::remove_dir_all(&root).ok();
    }

    /// `--dry-run` reports without moving.
    #[test]
    fn dry_run_reports_without_moving() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
            env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("rotate-dry");
        let log_dir = root.join(".rally/log");
        fs::create_dir_all(&log_dir).unwrap();

        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "old-only", &old_ts, &[1]);

        let outcome = run_rotate(root.clone(), None, true).unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.rotated.len(), 1);
        assert_eq!(outcome.rotated[0].reason, "would_rotate");
        // File untouched.
        assert!(log_dir.join("old-only.jsonl").exists());

        fs::remove_dir_all(&root).ok();
    }

    /// Idempotent — running twice on already-rotated state is a no-op.
    #[test]
    fn rotation_is_idempotent() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
            env::remove_var(crate::store::ENGAGEMENT_ENV_VAR);
        }
        let root = unique_root("rotate-idempotent");
        let log_dir = root.join(".rally/log");
        fs::create_dir_all(&log_dir).unwrap();

        let old_ts =
            (Utc::now() - Duration::days(200)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_segment(&log_dir, "old", &old_ts, &[1]);

        let r1 = run_rotate(root.clone(), None, false).unwrap();
        assert_eq!(r1.rotated.len(), 1);
        let r2 = run_rotate(root.clone(), None, false).unwrap();
        assert!(r2.rotated.is_empty(), "second run rotates nothing");
        assert!(
            r2.skipped.is_empty(),
            "no leftover live segments → no skips"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Threshold-source precedence: flag > env > manifest > default.
    #[test]
    fn threshold_source_priority() {
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: env mutation serialized by PROCESS_ENV_LOCK above.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
        }
        let root = unique_root("rotate-threshold");
        fs::create_dir_all(root.join(".rally")).unwrap();

        // Default → 90.
        let r = run_rotate(root.clone(), None, true).unwrap();
        assert_eq!(r.threshold_days, 90);
        assert_eq!(r.threshold_source, "default");

        // Manifest → 30.
        fs::write(
            root.join(".rally/manifest.json"),
            serde_json::json!({"rotate_threshold_days": 30}).to_string(),
        )
        .unwrap();
        let r = run_rotate(root.clone(), None, true).unwrap();
        assert_eq!(r.threshold_days, 30);
        assert_eq!(r.threshold_source, "manifest");

        // Env → 15 (beats manifest).
        // SAFETY: env mutation, single-threaded test scope.
        unsafe {
            env::set_var(THRESHOLD_ENV_VAR, "15");
        }
        let r = run_rotate(root.clone(), None, true).unwrap();
        assert_eq!(r.threshold_days, 15);
        assert_eq!(r.threshold_source, "env");

        // Flag → 7 (beats env).
        let r = run_rotate(root.clone(), Some(7), true).unwrap();
        assert_eq!(r.threshold_days, 7);
        assert_eq!(r.threshold_source, "flag");

        // SAFETY: env cleanup.
        unsafe {
            env::remove_var(THRESHOLD_ENV_VAR);
        }
        fs::remove_dir_all(&root).ok();
    }
}
