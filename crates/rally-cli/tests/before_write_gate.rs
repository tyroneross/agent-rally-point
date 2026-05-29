// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn json_with_status(&self, args: &[&str]) -> (Value, Output) {
        let output = self.output(args);
        let value = serde_json::from_slice(&output.stdout).unwrap();
        (value, output)
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
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

#[test]
fn before_write_gate_cannot_be_bypassed_by_warn_mode_missing_path_or_unknown_tool() {
    let workspace = Workspace::new("rally-before-write-gate");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude",
        "--path",
        "src/lib.rs",
        "--subject",
        "own lib",
    ]);
    let (warn_check, warn_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        "src/lib.rs",
    ]);
    assert!(warn_output.status.success());
    assert_eq!(warn_check["data"]["check"]["mode"], "warn");
    assert_eq!(warn_check["data"]["check"]["allow"], false);
    assert!(
        warn_check["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "claimed-path")
    );

    workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "claude",
        "--subject",
        "global freeze",
    ]);
    let (missing_path_check, missing_path_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--strict",
    ]);
    assert_eq!(missing_path_output.status.code(), Some(4));
    assert_eq!(missing_path_check["data"]["check"]["allow"], false);
    let missing_path_findings = missing_path_check["data"]["check"]["findings"]
        .as_array()
        .unwrap();
    assert!(
        missing_path_findings
            .iter()
            .any(|finding| finding["code"] == "missing-path")
    );
    assert!(
        missing_path_findings
            .iter()
            .any(|finding| finding["code"] == "active-blocker")
    );

    let omitted_tool =
        workspace.output(&["check", "before-write", "--json", "--path", "src/lib.rs"]);
    assert_eq!(omitted_tool.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&omitted_tool.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("check before-write requires --tool <tool>")
    );

    let unknown_tool = workspace.output(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "unknown",
        "--path",
        "src/lib.rs",
    ]);
    assert_eq!(unknown_tool.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&unknown_tool.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("requires a real --tool")
    );

    workspace.cleanup();
}
