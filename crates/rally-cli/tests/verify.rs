// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_jsonl(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{name}-{}.jsonl",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn verify_reports_unsigned_python_style_record() {
    let path = temp_jsonl(
        "rally-cli-unsigned",
        r#"{"id":"evt_11111111111111111111111111111111","kind":"handoff","type":"agent-rally.handoff.created.v1","tool":"codex","payload":{"subject":"smoke"},"revision":3}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .arg("verify")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("evt_11111111111111111111111111111111 unsigned"));
    assert!(stdout.contains("summary records=1 unsigned=1"));
}

#[test]
fn usage_errors_exit_nonzero_for_agent_automation() {
    let output = Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: rally-rs verify <changes.jsonl>"));
}
