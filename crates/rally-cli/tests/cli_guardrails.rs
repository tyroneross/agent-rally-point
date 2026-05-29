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

fn json_error(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2));
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn subcommand_help_handles_positionals_without_panicking() {
    let workspace = Workspace::new("rally-subcommand-help-positionals");

    for command in [
        "say", "check", "locate", "run", "inject", "attach", "capture", "stop",
    ] {
        let output = workspace.output(&[command, "--help"]);
        assert!(
            output.status.success(),
            "{command} --help failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage"),
            "{command} --help did not print usage"
        );
    }

    workspace.cleanup();
}

#[test]
fn bounded_numeric_flags_reject_out_of_range_values() {
    let workspace = Workspace::new("rally-bounded-numeric-flags");

    let high = workspace.output(&["next", "--json", "--tool", "codex", "--limit", "999"]);
    let body = json_error(&high);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("--limit must be between 1 and 20")
    );

    let low = workspace.output(&["capture", "missing-session", "--json", "--lines", "0"]);
    let body = json_error(&low);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("--lines must be between 1 and 2000")
    );

    workspace.cleanup();
}

#[test]
fn aliased_flags_reject_both_names_instead_of_dropping_one() {
    let workspace = Workspace::new("rally-aliased-flags");

    let target = workspace.output(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude",
        "--to",
        "other",
        "--subject",
        "ambiguous target",
    ]);
    let body = json_error(&target);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("cannot use --target and --to together")
    );

    let handoff = workspace.output(&[
        "inject",
        "some-session",
        "--json",
        "--handoff",
        "fact_a",
        "--ref",
        "fact_b",
    ]);
    let body = json_error(&handoff);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("cannot use --handoff and --ref together")
    );

    workspace.cleanup();
}
