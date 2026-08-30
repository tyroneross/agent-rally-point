// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Zero-history contract for `rally help frame`.
//!
//! Claude contributed the original red contract. This version keeps its four
//! invariants while parsing the canonical four-column glossary directly:
//! store-free access, exactly eight fields, complete semantics per field, and
//! a responsibility category that grants neither scope nor authority.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FRAME_FIELDS: [&str; 8] = [
    "sender",
    "intent",
    "control-attempt",
    "sender-type",
    "room-position",
    "responsibility",
    "authority",
    "guide",
];

fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rally-frame-help-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("create isolated help directory");
    dir
}

fn run_help_frame(cwd: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rally"))
        .args(["help", "frame"])
        .current_dir(cwd)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_RUN_ID")
        .env_remove("RALLY_SESSION_ID")
        .env_remove("RALLY_OBSERVER_PID")
        .env_remove("RALLY_HOOK_SOURCE")
        .env_remove("TERM_SESSION_ID")
        .env_remove("TMUX_PANE")
        .env_remove("TTY")
        .output()
        .expect("run rally help frame")
}

fn glossary(text: &str) -> BTreeMap<String, [String; 3]> {
    let mut rows = BTreeMap::new();
    let mut in_table = false;
    for line in text.lines() {
        if line == "Field | source and assurance | receiver behavior | unknown or default" {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        let cells: Vec<String> = line
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect();
        assert_eq!(
            cells.len(),
            4,
            "each frame glossary row must have four columns: {line:?}"
        );
        assert!(
            rows.insert(
                cells[0].clone(),
                [cells[1].clone(), cells[2].clone(), cells[3].clone()]
            )
            .is_none(),
            "duplicate frame glossary row: {}",
            cells[0]
        );
    }
    rows
}

#[test]
fn help_frame_is_store_free_and_initializes_nothing() {
    let dir = tmp_dir("outside");
    let output = run_help_frame(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.starts_with("Rally message frame\n"),
        "stdout: {stdout}"
    );
    assert!(!dir.join(".rally").exists());
    assert!(!dir.join(".git").exists());
    fs::remove_dir_all(dir).expect("remove isolated help directory");
}

#[test]
fn help_frame_defines_exactly_the_runtime_fields_with_complete_semantics() {
    let dir = tmp_dir("glossary");
    let output = run_help_frame(&dir);
    fs::remove_dir_all(dir).expect("remove isolated help directory");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("UTF-8 help output");
    let rows = glossary(&text);
    let fields: Vec<&str> = rows.keys().map(String::as_str).collect();
    let mut expected = FRAME_FIELDS.to_vec();
    expected.sort_unstable();
    assert_eq!(fields, expected, "frame field set drifted\n{text}");

    for field in FRAME_FIELDS {
        let [source, effect, fallback] = rows.get(field).expect("canonical field row");
        assert!(!source.is_empty(), "{field} lacks source or assurance");
        assert!(!effect.is_empty(), "{field} lacks receiver behavior");
        assert!(
            !fallback.is_empty(),
            "{field} lacks unknown/default handling"
        );
    }
}

#[test]
fn help_frame_keeps_status_duty_intent_and_authority_semantically_separate() {
    let dir = tmp_dir("meaning");
    let output = run_help_frame(&dir);
    fs::remove_dir_all(dir).expect("remove isolated help directory");
    let text = String::from_utf8(output.stdout).expect("UTF-8 help output");
    let rows = glossary(&text);

    assert!(rows["sender"][0].contains("unverified"));
    assert!(rows["intent"][1].contains("receiver-decided"));
    assert!(rows["intent"][2].contains("fails closed"));
    assert!(rows["control-attempt"][0].contains("derived from intent"));
    assert!(rows["control-attempt"][1].contains("evaluate authority"));
    assert!(rows["sender-type"][1].contains("never grants authority"));
    assert!(rows["room-position"][1].contains("not command authority"));
    assert!(rows["responsibility"][0].contains("unverified and unscoped"));
    assert!(rows["responsibility"][1].contains("grants neither work scope nor authority"));
    assert!(rows["authority"][0].contains("derived"));
    assert!(rows["authority"][1].contains("control attempt was allowed"));
    assert!(rows["authority"][2].contains("not proof"));
    assert_eq!(rows["guide"][2], "rally help frame");
}
