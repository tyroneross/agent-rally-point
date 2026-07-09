// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Mutating-command watchdog durability.
//!
//! A timed-out mutating command must never return the old bare `ok:true`
//! envelope before its primary durable append commits. If the append has
//! committed but later projection/output work is slow, the timeout may report
//! success only with an explicit committed signal so callers do not retry and
//! duplicate the fact.

use rally_protocol::Inbox;
use rally_protocol::ledger::FileInbox;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("watchdog-durability-{name}-cwd"));
        let home = temp_path(&format!("watchdog-durability-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        cmd
    }

    fn room_json(&self) -> Value {
        let output = self
            .rally()
            .args(["room", "--json", "--timeout-ms", "15000"])
            .output()
            .expect("spawn rally room");
        assert!(
            output.status.success(),
            "room replay failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout_json(&output)
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn stdout_json(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("stdout must be JSON: {err}\nstdout={stdout}");
    })
}

fn open_handoff_subject_count(room: &Value, subject: &str) -> usize {
    match room["data"]["room"]["open_handoffs"].as_array() {
        Some(handoffs) => handoffs
            .iter()
            .filter(|fact| fact["subject"] == subject)
            .count(),
        None => 0,
    }
}

fn unique_subject(label: &str) -> String {
    format!(
        "watchdog-durability-{label}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[test]
fn uncommitted_mutation_timeout_fails_closed_and_does_not_claim_success() {
    let room = TempRoom::new("uncommitted");
    let subject = unique_subject("uncommitted");

    let output = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            &subject,
            "--json",
            "--timeout-ms",
            "300",
        ])
        .env("RALLY_TEST_BLOCK_MS", "5000")
        .output()
        .expect("spawn mutating rally command");

    assert_eq!(
        output.status.code(),
        Some(4),
        "uncommitted timeout must fail closed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = stdout_json(&output);
    assert_eq!(payload["ok"], false);
    assert_eq!(
        payload["error"]["code"],
        "watchdog-timeout-uncommitted-mutation"
    );
    assert_eq!(payload["data"]["watchdog"]["committed"], false);

    let replay = room.room_json();
    assert_eq!(
        open_handoff_subject_count(&replay, &subject),
        0,
        "uncommitted timeout must not land the handoff"
    );
}

#[test]
fn committed_mutation_timeout_reports_committed_and_survives_replay_once() {
    let room = TempRoom::new("committed");
    let subject = unique_subject("committed");

    let output = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            &subject,
            "--json",
            "--timeout-ms",
            "2000",
        ])
        .env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", "5000")
        .output()
        .expect("spawn mutating rally command");

    assert_eq!(
        output.status.code(),
        Some(0),
        "committed timeout must preserve success; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = stdout_json(&output);
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["command"], "watchdog");
    assert_eq!(payload["data"]["watchdog"]["committed"], true);
    assert_eq!(payload["data"]["watchdog"]["projection_complete"], false);

    let replay = room.room_json();
    assert_eq!(
        open_handoff_subject_count(&replay, &subject),
        1,
        "committed timeout must replay exactly one durable handoff"
    );
}

#[test]
fn inject_timeout_marks_committed_only_after_directive_is_durable() {
    let room = TempRoom::new("inject-committed");
    // Initialize the room before the timed invocation so only the inject path
    // participates in the watchdog assertion.
    let _ = room.room_json();

    let output = room
        .rally()
        .args([
            "inject",
            "watchdog-target",
            "--tool",
            "codex",
            "--text",
            "durable directive",
            "--json",
            "--timeout-ms",
            "300",
        ])
        .env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", "5000")
        .output()
        .expect("spawn timed inject");

    assert_eq!(output.status.code(), Some(0));
    let payload = stdout_json(&output);
    assert_eq!(payload["command"], "watchdog");
    assert_eq!(payload["data"]["watchdog"]["committed"], true);

    let inbox = FileInbox::open(room.cwd.join(".rally")).expect("open directive ledger");
    let directives = inbox
        .read_since("watchdog-target", 0)
        .expect("replay durable directive");
    assert_eq!(directives.len(), 1);
    assert_eq!(directives[0].seq, 1);
    assert_eq!(directives[0].text.as_deref(), Some("durable directive"));
}
