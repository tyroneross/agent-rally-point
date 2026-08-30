// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end controls for claim ownership across short-lived CLI processes.

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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("cla-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("cla-{name}-{nanos}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run_as_session(&self, session_id: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env("RALLY_SESSION_ID", session_id)
            .args(args)
            .output()
            .unwrap()
    }

    fn json_as_session(&self, session_id: &str, args: &[&str]) -> Value {
        let output = self.run_as_session(session_id, args);
        let body = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        serde_json::from_slice(body).unwrap_or_else(|error| {
            panic!(
                "cmd {args:?} did not emit JSON ({error})\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout),
            )
        })
    }

    fn run_without_stable_session(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("RALLY_SESSION_ID")
            .env_remove("RALLY_OBSERVER_PID")
            .env_remove("TERM_SESSION_ID")
            .env_remove("TMUX_PANE")
            .env_remove("TTY")
            .args(args)
            .output()
            .unwrap()
    }

    fn claim(&self, tool: &str, session_id: &str, path: &str) -> String {
        let value = self.json_as_session(
            session_id,
            &[
                "say",
                "claim",
                "--tool",
                tool,
                "--path",
                path,
                "--subject",
                "owns it",
                "--json",
                // Override the hook-safety default (3000ms) with a generous
                // ceiling. Under parallel test/build load this process's
                // bootstrap + ledger append can occasionally exceed 3s; when
                // that happens the CLI's own watchdog fires first and returns
                // a `command: "watchdog"` envelope (no `data.say.fact`) even
                // though the mutation itself succeeds, which previously made
                // the `.expect("claim event id")` below panic. The 3s default
                // exists to protect live write-hooks from stuck I/O, which
                // doesn't apply to this test driver, so widening it here does
                // not mask a product race — it lets the command finish under
                // load instead of racing an unrelated safety timer.
                "--timeout-ms",
                "30000",
            ],
        );
        assert_eq!(value["ok"], true, "claim failed: {value}");
        value["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "claim event id missing from response shape (command={:?}): {value}",
                    value["command"]
                )
            })
            .to_string()
    }

    fn age_tool_facts(&self, tools: &[&str]) {
        let log = self.cwd.join(".rally/log");
        for entry in fs::read_dir(log).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            let rewritten = source
                .lines()
                .map(|line| {
                    let mut fact: Value = serde_json::from_str(line).unwrap();
                    if fact["payload"]["tool"]
                        .as_str()
                        .is_some_and(|tool| tools.contains(&tool))
                    {
                        let old = Value::String("2020-01-01T00:00:00Z".to_string());
                        fact["occurred_at"] = old.clone();
                        fact["payload"]["created_at"] = old;
                    }
                    serde_json::to_string(&fact).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(path, format!("{rewritten}\n")).unwrap();
        }
        for name in [
            "facts.db",
            "facts.db-shm",
            "facts.db-wal",
            "snapshot.cache.json",
            "claim-index.json",
            ".reconcile-cache.json",
        ] {
            fs::remove_file(self.cwd.join(".rally").join(name)).ok();
        }
        fs::remove_file(self.cwd.join(".rally/log/index.json")).ok();
    }

    fn active_claim_tools(&self) -> Vec<String> {
        let value = self.json_as_session("reader", &["room", "--json"]);
        value["data"]["room"]["active_claims"]
            .as_array()
            .expect("active claims")
            .iter()
            .filter_map(|fact| fact["tool"].as_str().map(str::to_string))
            .collect()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn finding_codes(value: &Value) -> Vec<&str> {
    value["data"]["check"]["findings"]
        .as_array()
        .expect("check findings")
        .iter()
        .filter_map(|finding| finding["code"].as_str())
        .collect()
}

#[test]
fn stable_session_owns_and_releases_claim_across_processes() {
    let workspace = Workspace::new("stable-session");
    let tool = "codex:stable";
    let session = "stable-session";
    workspace.claim(tool, session, "src/stable.rs");

    let before_output = workspace.run_as_session(
        session,
        &[
            "check",
            "before-complete",
            "--tool",
            tool,
            "--strict",
            "--json",
        ],
    );
    assert_eq!(before_output.status.code(), Some(4));
    let before: Value = serde_json::from_slice(&before_output.stdout).unwrap();
    assert!(finding_codes(&before).contains(&"owned-active-claim"));

    let release = workspace.json_as_session(
        session,
        &[
            "say",
            "release",
            "--tool",
            tool,
            "--path",
            "src/stable.rs",
            "--subject",
            "done",
            "--json",
        ],
    );
    assert_eq!(
        release["ok"], true,
        "stable-session release failed: {release}"
    );
    assert!(workspace.active_claim_tools().is_empty());
    workspace.cleanup();
}

#[test]
fn unpinned_process_lifecycle_fails_closed_instead_of_hiding_a_stranded_claim() {
    let workspace = Workspace::new("unpinned-process");
    let tool = "codex:unpinned";
    let claim = workspace.run_without_stable_session(&[
        "say",
        "claim",
        "--tool",
        tool,
        "--path",
        "src/unpinned.rs",
        "--subject",
        "owns it",
        "--json",
    ]);
    assert!(claim.status.success(), "claim setup failed: {claim:?}");

    let before = workspace.run_without_stable_session(&[
        "check",
        "before-complete",
        "--tool",
        tool,
        "--strict",
        "--json",
    ]);
    assert!(!before.status.success(), "unpinned check must fail closed");
    let error: Value = serde_json::from_slice(&before.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]
            .as_str()
            .is_some_and(|message| message.contains("RALLY_SESSION_ID")),
        "refusal must explain the stable-session remedy: {error}"
    );
    assert_eq!(workspace.active_claim_tools(), vec![tool.to_string()]);
    workspace.cleanup();
}

#[test]
fn manual_skill_exports_one_session_for_claim_check_and_release_children() {
    let skill = include_str!("../../../skills/agent-rally-point/SKILL.md");
    assert!(skill.contains("export RALLY_SESSION_ID"));
    assert!(
        skill.contains("rally enter --tool \"$TOOL\" --session-id \"$RALLY_SESSION_ID\" --json")
    );
}

#[test]
fn same_tool_sibling_is_not_reported_as_claim_owner() {
    let workspace = Workspace::new("sibling-check");
    let tool = "codex:shared";
    workspace.claim(tool, "owner-session", "src/owned.rs");

    let sibling = workspace.json_as_session(
        "sibling-session",
        &["check", "before-complete", "--tool", tool, "--json"],
    );
    assert!(
        !finding_codes(&sibling).contains(&"owned-active-claim"),
        "a shared label must not make a sibling session the owner: {sibling}"
    );
    assert_eq!(workspace.active_claim_tools(), vec![tool.to_string()]);
    workspace.cleanup();
}

#[test]
fn liveness_enforce_filters_one_target_and_never_releases_an_unselected_peer() {
    let workspace = Workspace::new("liveness-filter");
    workspace.claim("peer:a", "peer-a-session", "src/a.rs");
    workspace.claim("peer:b", "peer-b-session", "src/b.rs");
    workspace.age_tool_facts(&["peer:a", "peer:b"]);

    let enforced = workspace.json_as_session(
        "operator-session",
        &[
            "check",
            "liveness",
            "--tool",
            "peer:a",
            "--actor",
            "operator:01",
            "--enforce",
            "--json",
        ],
    );
    assert_eq!(enforced["ok"], true, "liveness enforce failed: {enforced}");
    assert_eq!(
        workspace.active_claim_tools(),
        vec!["peer:b".to_string()],
        "the exact --tool filter must leave every unselected peer untouched"
    );
    workspace.cleanup();
}

#[test]
fn liveness_enforce_requires_an_explicit_actor_and_exact_target() {
    let workspace = Workspace::new("liveness-explicit-authority");
    workspace.claim("peer:a", "peer-a-session", "src/a.rs");
    workspace.age_tool_facts(&["peer:a"]);

    let missing_actor = workspace.json_as_session(
        "operator-session",
        &[
            "check",
            "liveness",
            "--tool",
            "peer:a",
            "--enforce",
            "--json",
        ],
    );
    assert_eq!(missing_actor["ok"], false, "missing actor must fail closed");

    let whitespace_actor = workspace.json_as_session(
        "operator-session",
        &[
            "check",
            "liveness",
            "--tool",
            "peer:a",
            "--actor",
            "   ",
            "--enforce",
            "--json",
        ],
    );
    assert_eq!(
        whitespace_actor["ok"], false,
        "whitespace-only actor must fail closed"
    );

    let missing_target = workspace.json_as_session(
        "operator-session",
        &[
            "check",
            "liveness",
            "--actor",
            "operator:01",
            "--enforce",
            "--json",
        ],
    );
    assert_eq!(
        missing_target["ok"], false,
        "missing target must fail closed"
    );
    assert_eq!(workspace.active_claim_tools(), vec!["peer:a".to_string()]);
    workspace.cleanup();
}
