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
//! **Safety.** Rotation only *moves* — never deletes. Archived segments
//! remain in `.rally/archive/<engagement>.jsonl`, replay still unions
//! them (R5's `reconcile_segments_and_db` walks both dirs). A
//! roundtrip after rotation reconstructs the same `facts.db`.
//!
//! **Idempotency.** Re-running `rally rotate` on already-rotated state is
//! a no-op. If the same engagement label exists in both live and archive
//! at the time of rotation (concurrent writers), the rotation refuses to
//! move that file to avoid clobbering the archived copy; the caller
//! resolves manually or waits for the live segment to roll over.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::error::{RallyError, Result};
use crate::store::{ARCHIVE_DIRNAME, LOG_DIRNAME};

const DEFAULT_THRESHOLD_DAYS: i64 = 90;
const THRESHOLD_ENV_VAR: &str = "RALLY_ROTATE_DAYS";
const MANIFEST_THRESHOLD_FIELD: &str = "rotate_threshold_days";

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
    let file = fs::File::open(path).map_err(RallyError::io(format!("read {}", path.display())))?;
    let mut latest: Option<DateTime<Utc>> = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(RallyError::io(format!("read {}", path.display())))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(ts_str) = value.get("occurred_at").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Ok(ts) = DateTime::parse_from_rfc3339(ts_str) {
            let ts_utc = ts.with_timezone(&Utc);
            latest = Some(latest.map(|cur| cur.max(ts_utc)).unwrap_or(ts_utc));
        }
    }
    Ok(latest)
}

/// Move `src` to `dst`, refusing if `dst` already exists.
fn safe_move(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(RallyError::Message(format!(
            "rotation target {} already exists; refusing to clobber",
            dst.display()
        )));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(RallyError::io(format!("create {}", parent.display())))?;
    }
    fs::rename(src, dst).map_err(RallyError::io(format!(
        "move {} -> {}",
        src.display(),
        dst.display()
    )))
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
    let log_dir = repo_root.join(".rally").join(LOG_DIRNAME);
    let archive_dir = repo_root.join(".rally").join(ARCHIVE_DIRNAME);
    let (threshold_days, threshold_source) = resolve_threshold(flag_days, &repo_root);
    let cutoff = Utc::now() - Duration::days(threshold_days);

    let live_before = segment_files(&log_dir)?;
    let mut rotated = Vec::new();
    let mut skipped = Vec::new();

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
        let dst = archive_dir.join(&name);
        if dst.exists() {
            skipped.push(SkippedSegment {
                segment: name,
                reason: format!("archive collision: {} already exists", dst.display()),
            });
            continue;
        }
        if dry_run {
            rotated.push(RotatedSegment {
                segment: name,
                from: path.display().to_string(),
                to: dst.display().to_string(),
                reason: "would_rotate",
            });
            continue;
        }
        safe_move(path, &dst)?;
        rotated.push(RotatedSegment {
            segment: name,
            from: path.display().to_string(),
            to: dst.display().to_string(),
            reason: "rotated",
        });
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
    use crate::store::{Fact, FactKind, RoomStore};
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
        fs::create_dir_all(log_dir).unwrap();
        let path = log_dir.join(format!("{label}.jsonl"));
        let mut content = String::new();
        for &seq in seqs {
            let line = serde_json::json!({
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
                    "created_at": occurred_at,
                },
                "engagement": label,
            });
            content.push_str(&line.to_string());
            content.push('\n');
        }
        fs::write(&path, content).unwrap();
    }

    /// Rotate an old segment, keep a recent one, confirm replay still
    /// reconstructs full history.
    #[test]
    fn rotates_old_segments_keeps_recent_replay_preserves_history() {
        // SAFETY: env mutation, single-threaded test scope.
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
        // SAFETY: env mutation, single-threaded test scope.
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
        // SAFETY: env mutation, single-threaded test scope.
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
        // SAFETY: env mutation, single-threaded test scope.
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
        // SAFETY: env mutation, single-threaded test scope.
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
