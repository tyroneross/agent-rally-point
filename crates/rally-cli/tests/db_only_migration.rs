// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! CLI-level gates for the explicit DB-only recovery command.
//!
//! These tests deliberately create a current-format `facts.db` through the
//! public CLI, remove its canonical JSONL segment, and then exercise only the
//! documented offline migration command. The database is evidence: every
//! scenario proves its bytes remain unchanged.

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_ROOM: AtomicU64 = AtomicU64::new(1);

struct Room {
    cwd: PathBuf,
    home: PathBuf,
    session_id: String,
}

impl Room {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_ROOM.fetch_add(1, Ordering::Relaxed);
        let cwd = std::env::temp_dir().join(format!(
            "rally-db-only-cli-{name}-{}-{nanos}-{nonce}-cwd",
            std::process::id()
        ));
        let home = std::env::temp_dir().join(format!(
            "rally-db-only-cli-{name}-{}-{nanos}-{nonce}-home",
            std::process::id()
        ));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            cwd,
            home,
            session_id: format!("db-only-cli-{name}-{nanos}-{nonce}"),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rally"));
        command
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_GLOBAL_INDEX", "0")
            .env("RALLY_SESSION_ID", &self.session_id)
            .env("RALLY_ENGAGEMENT", "seed")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_ok_json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_success(args, &output);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "command {args:?} did not emit JSON: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn seed_history(&self) {
        for subject in ["first DB-only fact", "second DB-only fact"] {
            self.run_ok_json(&[
                "say",
                "decision",
                "--tool",
                "codex:db-only-cli-test",
                "--subject",
                subject,
                "--json",
            ]);
        }
        assert!(self.facts_db().is_file(), "CLI setup must create facts.db");
        assert_eq!(self.jsonl_files().len(), 1, "setup must be canonical first");
    }

    fn remove_canonical_history(&self) {
        for path in self.jsonl_files() {
            fs::remove_file(path).unwrap();
        }
        assert!(self.jsonl_files().is_empty());
        assert!(self.facts_db().is_file());
    }

    fn make_db_only(&self) {
        self.seed_history();
        self.remove_canonical_history();
    }

    fn rally_dir(&self) -> PathBuf {
        self.cwd.join(".rally")
    }

    fn facts_db(&self) -> PathBuf {
        self.rally_dir().join("facts.db")
    }

    fn target(&self) -> PathBuf {
        self.rally_dir().join("log/alpha.jsonl")
    }

    fn jsonl_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dirname in ["log", "archive"] {
            let dir = self.rally_dir().join(dirname);
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            files.extend(entries.filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension().and_then(|value| value.to_str()) == Some("jsonl")).then_some(path)
            }));
        }
        let legacy = self.rally_dir().join("ledger.jsonl");
        if legacy.exists() {
            files.push(legacy);
        }
        files.sort();
        files
    }

    fn migration_metadata(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.rally_dir()) else {
            return Vec::new();
        };
        let mut paths = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.contains("db-only-migration"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn assert_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "command {args:?} failed (exit {:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn snapshot_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, current: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = fs::read_dir(current) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                walk(root, &path, files);
            } else if metadata.is_file() {
                files.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn migration_args(apply: bool) -> Vec<&'static str> {
    let mut args = vec!["doctor", "--migrate-db-only", "--engagement", "alpha"];
    if apply {
        args.push("--apply");
    }
    args.push("--json");
    args
}

#[test]
fn json_dry_run_apply_and_retry_are_byte_safe_and_exactly_singleton() {
    let room = Room::new("lifecycle");
    room.make_db_only();
    let db_before = fs::read(room.facts_db()).unwrap();
    let tree_before = snapshot_files(&room.cwd);

    let dry_run = room.run_ok_json(&migration_args(false));
    assert_eq!(dry_run["ok"], true, "dry-run envelope: {dry_run:#}");
    assert_eq!(dry_run["command"], "doctor");
    assert_eq!(dry_run["data"]["doctor"]["state"], "dry_run");
    assert_eq!(dry_run["data"]["doctor"]["applied"], false);
    assert_eq!(
        dry_run["data"]["doctor"]["apply_requires_revalidation"],
        true
    );
    let row_count = dry_run["data"]["doctor"]["row_count"]
        .as_u64()
        .expect("dry-run reports its complete DB row count");
    assert!(row_count >= 2, "both seeded decisions must survive");
    assert!(dry_run["data"]["doctor"]["source_token"].is_string());
    assert_eq!(
        snapshot_files(&room.cwd),
        tree_before,
        "dry-run must not create locks, marker state, canonical history, or alter DB bytes"
    );

    let applied = room.run_ok_json(&migration_args(true));
    assert_eq!(applied["ok"], true, "apply envelope: {applied:#}");
    assert_eq!(applied["data"]["doctor"]["state"], "committed");
    assert_eq!(applied["data"]["doctor"]["applied"], true);
    assert_eq!(applied["data"]["doctor"]["row_count"], row_count);
    assert_eq!(fs::read(room.facts_db()).unwrap(), db_before);
    assert_eq!(room.jsonl_files(), vec![room.target()]);
    let target_before_retry = fs::read(room.target()).unwrap();
    assert_eq!(
        target_before_retry
            .iter()
            .filter(|byte| **byte == b'\n')
            .count() as u64,
        row_count
    );

    let retry = room.run_ok_json(&migration_args(true));
    assert_eq!(retry["ok"], true, "retry envelope: {retry:#}");
    assert_eq!(retry["data"]["doctor"]["state"], "already_committed");
    assert_eq!(fs::read(room.facts_db()).unwrap(), db_before);
    assert_eq!(fs::read(room.target()).unwrap(), target_before_retry);
    assert_eq!(room.jsonl_files(), vec![room.target()]);

    let metadata = room.migration_metadata();
    assert_eq!(metadata.len(), 1, "only the immutable receipt may remain");
    assert!(
        metadata[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("receipt.json")
    );
}

#[test]
fn concurrent_apply_is_singleflight_and_preserves_db() {
    let room = Room::new("concurrent");
    room.make_db_only();
    let db_before = fs::read(room.facts_db()).unwrap();
    let args = migration_args(true);

    let first = room
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let second = room
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let first_output = first.wait_with_output().unwrap();
    let second_output = second.wait_with_output().unwrap();

    let mut committed = 0;
    let mut successful = 0;
    let mut reported_row_count = None;
    for output in [&first_output, &second_output] {
        if output.status.success() {
            successful += 1;
            let body: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "successful apply did not emit JSON: {error}\n{}",
                    output_text(output)
                )
            });
            let state = body["data"]["doctor"]["state"].as_str().unwrap_or_default();
            assert!(
                matches!(state, "committed" | "already_committed"),
                "unexpected successful state {state:?}: {body:#}"
            );
            let row_count = body["data"]["doctor"]["row_count"]
                .as_u64()
                .expect("successful migration reports its row count");
            if let Some(expected) = reported_row_count {
                assert_eq!(row_count, expected, "both invocations must bind one source");
            } else {
                reported_row_count = Some(row_count);
            }
            committed += usize::from(state == "committed");
        } else {
            let text = output_text(output);
            assert!(
                text.contains("offline migration authority is busy")
                    || text.contains("another direct Rally process owns facts.db"),
                "the losing invocation must fail closed on ownership, not corrupt or guess: {text}"
            );
        }
    }

    assert!(successful >= 1, "one migration invocation must commit");
    assert_eq!(
        committed, 1,
        "exactly one invocation may publish the target"
    );
    assert_eq!(room.jsonl_files(), vec![room.target()]);
    assert_eq!(fs::read(room.facts_db()).unwrap(), db_before);
    assert_eq!(
        fs::read(room.target())
            .unwrap()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        reported_row_count.expect("one invocation succeeds") as usize
    );
}

struct Daemon<'a> {
    room: &'a Room,
    child: Option<Child>,
}

impl<'a> Daemon<'a> {
    fn start(room: &'a Room) -> Self {
        let log = fs::File::create(room.rally_dir().join("db-only-daemon-test.log")).unwrap();
        let child = room
            .command(&["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let mut daemon = Self {
            room,
            child: Some(child),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let status = daemon.room.run(&["daemon", "status", "--json"]);
            if status.status.success() && daemon.room.rally_dir().join("rallyd.sock.addr").exists()
            {
                return daemon;
            }
            if daemon
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "daemon did not become ready: {}",
            fs::read_to_string(room.rally_dir().join("db-only-daemon-test.log"))
                .unwrap_or_default()
        );
    }

    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = self.room.run(&["daemon", "stop", "--json"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn remove_rendezvous_while_owner_remains(&mut self) {
        assert!(
            self.child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_none(),
            "daemon process must still own its lock"
        );
        for filename in ["rallyd.sock.addr", "rallyd.sock"] {
            let path = self.room.rally_dir().join(filename);
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove daemon rendezvous: {error}"),
            }
        }
    }

    fn force_kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn live_and_unresponsive_daemon_owner_refuse_before_migration_artifacts() {
    let room = Room::new("live-daemon");
    room.seed_history();
    let mut daemon = Daemon::start(&room);
    room.remove_canonical_history();
    let db_before = fs::read(room.facts_db()).unwrap();

    let output = room.run(&migration_args(true));
    assert!(!output.status.success(), "live daemon must refuse apply");
    let text = output_text(&output);
    assert!(
        text.contains("rally daemon stop"),
        "refusal must give the exact recovery command: {text}"
    );
    assert!(
        text.contains("live or unresponsive daemon") || text.contains("daemon owns facts.db"),
        "refusal must identify the owner gate: {text}"
    );
    assert_eq!(fs::read(room.facts_db()).unwrap(), db_before);
    assert!(room.jsonl_files().is_empty());
    assert!(
        room.migration_metadata().is_empty(),
        "owner refusal must precede marker, temp, target, or receipt creation"
    );

    daemon.remove_rendezvous_while_owner_remains();
    let wedged = room.run(&migration_args(true));
    assert!(
        !wedged.status.success(),
        "unresponsive daemon owner must refuse apply"
    );
    let text = output_text(&wedged);
    assert!(text.contains("rally daemon stop"), "{text}");
    assert!(text.contains("live or unresponsive daemon"), "{text}");
    assert_eq!(fs::read(room.facts_db()).unwrap(), db_before);
    assert!(room.jsonl_files().is_empty());
    assert!(room.migration_metadata().is_empty());
    daemon.force_kill();
}
