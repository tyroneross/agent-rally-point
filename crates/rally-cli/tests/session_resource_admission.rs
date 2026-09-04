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
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("rally-admission-cwd-{nonce}"));
        let home = std::env::temp_dir().join(format!("rally-admission-home-{nonce}"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run(&self, session_id: Option<&str>, close_token: Option<&str>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rally"));
        command
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_DAEMON_AUTOSTART", "0")
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("RALLY_SESSION_ID")
            .env_remove("RALLY_SESSION_CLOSE_TOKEN");
        if let Some(session_id) = session_id {
            command.env("RALLY_SESSION_ID", session_id);
        }
        if let Some(close_token) = close_token {
            command.env("RALLY_SESSION_CLOSE_TOKEN", close_token);
        }
        command.args(args).output().unwrap()
    }

    fn json(&self, session_id: Option<&str>, close_token: Option<&str>, args: &[&str]) -> Value {
        let output = self.run(session_id, close_token, args);
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn ensure_args<'a>(tool: &'a str, session_id: &'a str, adapter: &'a str) -> [&'a str; 12] {
    [
        "session",
        "ensure",
        "--json",
        "--tool",
        tool,
        "--session-id",
        session_id,
        "--adapter",
        adapter,
        "--resource",
        "task:shared-work-context",
        "--lifecycle-close",
    ]
}

#[test]
fn rally_admits_one_harness_before_launch_and_hands_the_resource_to_any_adapter() {
    let workspace = Workspace::new();
    let owner_args = ensure_args("codex:owner", "owner-lease", "codex");
    let owner = workspace.json(Some("owner-lease"), None, &owner_args);
    assert_eq!(owner["data"]["session"]["admission"]["state"], "granted");
    assert_eq!(
        owner["data"]["session"]["admission"]["resources"],
        serde_json::json!(["task:shared-work-context"])
    );
    let owner_claim = owner["data"]["session"]["admission"]["claim_id"]
        .as_str()
        .unwrap()
        .to_string();
    let close_token = owner["data"]["session"]["environment"]["RALLY_SESSION_CLOSE_TOKEN"]
        .as_str()
        .unwrap()
        .to_string();

    let repeat = workspace.json(Some("owner-lease"), Some(&close_token), &owner_args);
    assert_eq!(
        repeat["data"]["session"]["admission"]["claim_id"],
        owner_claim
    );

    let contender_args = ensure_args("claude_code:next", "contender-lease", "claude_code");
    let contender = workspace.run(Some("contender-lease"), None, &contender_args);
    assert!(!contender.status.success());
    let contender_error = format!(
        "{}{}",
        String::from_utf8_lossy(&contender.stdout),
        String::from_utf8_lossy(&contender.stderr)
    );
    assert!(
        contender_error.contains("claim conflict")
            && contender_error.contains("task:shared-work-context"),
        "unexpected conflict output: {contender_error}"
    );

    let claude_current = workspace.json(
        None,
        None,
        &["session", "current", "--json", "--tool", "claude_code:next"],
    );
    assert_eq!(
        claude_current["data"]["session"]["total"], 0,
        "a refused contender must not become an active parent harness lease"
    );

    let closed = workspace.json(
        Some("owner-lease"),
        Some(&close_token),
        &[
            "session",
            "close",
            "--json",
            "--tool",
            "codex:owner",
            "--session-id",
            "owner-lease",
        ],
    );
    assert_eq!(closed["data"]["session"]["action"], "close");

    let generic_args = ensure_args("aider:next", "generic-lease", "aider");
    let generic = workspace.json(Some("generic-lease"), None, &generic_args);
    assert_eq!(generic["data"]["session"]["admission"]["state"], "granted");
    assert_ne!(
        generic["data"]["session"]["admission"]["claim_id"], owner_claim,
        "a later handoff must create a new admission generation"
    );
}

#[test]
fn session_admission_rejects_nonexclusive_resources() {
    let workspace = Workspace::new();
    for resource in ["shared:task:work", "advisory:task:work", "task:"] {
        let output = workspace.run(
            Some("invalid-lease"),
            None,
            &[
                "session",
                "ensure",
                "--json",
                "--tool",
                "codex:invalid",
                "--session-id",
                "invalid-lease",
                "--resource",
                resource,
            ],
        );
        assert!(
            !output.status.success(),
            "resource unexpectedly admitted: {resource}"
        );
    }
}

#[test]
fn run_dry_run_reports_planned_admission_without_claiming() {
    let workspace = Workspace::new();
    let planned = workspace.json(
        None,
        None,
        &[
            "run",
            "codex",
            "--json",
            "--dry-run",
            "--shared",
            "--resource",
            "task:dry-run-context",
        ],
    );
    assert_eq!(planned["data"]["run"]["admission"]["state"], "planned");
    assert!(planned["data"]["run"]["admission"]["claim_id"].is_null());

    let log_dir = workspace.cwd.join(".rally/log");
    if log_dir.exists() {
        let log = fs::read_dir(log_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert!(!log.contains("protocol:session_admission=v1"));
    }
}

#[test]
fn failed_launch_rolls_back_only_the_new_admission_claim() {
    let workspace = Workspace::new();
    let existing = workspace.json(
        Some("rollback-lease"),
        None,
        &[
            "say",
            "claim",
            "--json",
            "--tool",
            "codex:rollback",
            "--subject",
            "keep this independent claim",
            "--resource",
            "device:keep",
        ],
    );
    let existing_id = existing["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    let launch = workspace.run(
        Some("rollback-lease"),
        None,
        &[
            "run",
            "codex",
            "--json",
            "--tool",
            "codex:rollback",
            "--session-id",
            "rollback-lease",
            "--shared",
            "--backend",
            "tmux",
            "--tmux-bin",
            "/definitely/missing/rally-test-tmux",
            "--resource",
            "task:failed-launch",
        ],
    );
    assert!(!launch.status.success());

    let room = workspace.json(None, None, &["room", "--json"]);
    let active = room["data"]["room"]["active_claims"].as_array().unwrap();
    assert!(
        active.iter().any(|claim| claim["event_id"] == existing_id),
        "launch rollback released an unrelated exact-session claim"
    );
    assert!(
        active.iter().all(|claim| claim["scope"]
            .as_array()
            .is_none_or(|scopes| scopes.iter().all(|scope| scope != "task:failed-launch"))),
        "failed launch left its admission claim active"
    );
}
