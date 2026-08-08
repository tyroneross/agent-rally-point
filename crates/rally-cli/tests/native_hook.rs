// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end contract for `rally hook before-write <host>`.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("native-hook-{name}-{nonce}"));
        let cwd = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOK_TIMEOUT_MS", "15000")
            .env_remove("RALLY_HOOKS")
            .output()
            .unwrap()
    }

    fn hook(&self, host: &str, tool: &str, input: &str, strict: bool) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rally"));
        command
            .arg("hook")
            .args(["--tool", tool])
            .args(if strict { vec!["--strict"] } else { Vec::new() })
            .args(["before-write", host])
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOK_TIMEOUT_MS", "15000")
            .env_remove("RALLY_HOOKS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "native hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "native hook emitted invalid JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn room(&self) -> Value {
        let output = self.output(&["room", "--json"]);
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(base) = self.cwd.parent() {
            fs::remove_dir_all(base).ok();
        }
    }
}

#[test]
fn allowed_write_auto_claims_once_and_emits_no_context() {
    let workspace = Workspace::new("allow");
    let input = r#"{"session_id":"s1","tool_input":{"file_path":"src/lib.rs"}}"#;

    assert_eq!(
        workspace.hook("codex", "codex:01", input, false),
        serde_json::json!({})
    );
    assert_eq!(
        workspace.hook("codex", "codex:01", input, false),
        serde_json::json!({})
    );

    let room = workspace.room();
    let claims = room["data"]["room"]["active_claims"].as_array().unwrap();
    let owned = claims
        .iter()
        .filter(|claim| {
            claim["tool"] == "codex:01"
                && claim["scope"]
                    .as_array()
                    .is_some_and(|scope| scope.iter().any(|item| item == "file:src/lib.rs"))
        })
        .count();
    assert_eq!(
        owned, 1,
        "repeated writes must not duplicate the auto-claim"
    );
}

#[test]
fn claude_conflict_is_advisory_by_default_and_blocking_only_in_strict_mode() {
    let workspace = Workspace::new("claude-conflict");
    let claim = workspace.output(&[
        "say",
        "claim",
        "--tool",
        "peer",
        "--path",
        "src/lib.rs",
        "--subject",
        "peer owns lib",
        "--json",
    ]);
    assert!(claim.status.success());
    let input = r#"{"tool_input":{"file_path":"src/lib.rs"}}"#;

    let advisory = workspace.hook("claude_code", "claude_code:01", input, false);
    assert_eq!(
        advisory["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert!(
        advisory["systemMessage"]
            .as_str()
            .unwrap()
            .starts_with("Rally: PROCEEDING WITH WARNING")
    );

    let strict = workspace.hook("claude_code", "claude_code:01", input, true);
    assert_eq!(strict["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        strict["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .starts_with("Rally: STOPPED")
    );
}

#[test]
fn codex_conflict_is_visible_but_never_receives_claude_permission_fields() {
    let workspace = Workspace::new("codex-conflict");
    assert!(
        workspace
            .output(&[
                "say",
                "claim",
                "--tool",
                "peer",
                "--path",
                "src/lib.rs",
                "--subject",
                "peer owns lib",
                "--json",
            ])
            .status
            .success()
    );
    let input = r#"{"tool_input":{"file_path":"src/lib.rs"}}"#;
    let output = workspace.hook("codex", "codex:01", input, true);
    assert!(
        output["systemMessage"]
            .as_str()
            .unwrap()
            .starts_with("Rally: PROCEEDING WITH WARNING")
    );
    assert!(output.get("hookSpecificOutput").is_none());
    assert!(output.get("permission").is_none());
}

#[test]
fn missing_or_invalid_input_stays_fail_open() {
    let workspace = Workspace::new("invalid");
    assert_eq!(
        workspace.hook("codex", "codex:01", "not-json", false),
        serde_json::json!({})
    );
}

#[test]
fn unsupported_host_is_rejected_before_coordination() {
    let workspace = Workspace::new("unsupported-host");
    let output = workspace.output(&["hook", "before-write", "unknown-host"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("supports hosts claude_code, codex, gemini, and cursor")
    );
    assert!(!workspace.cwd.join(".rally").exists());
}

#[test]
fn observer_pid_keeps_one_identity_when_host_omits_session_id() {
    let workspace = Workspace::new("observer-identity");
    for path in ["src/a.rs", "src/b.rs"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .args(["hook", "before-write", "codex"])
            .current_dir(&workspace.cwd)
            .env("HOME", &workspace.home)
            .env("RALLY_OBSERVER_PID", "4242")
            .env_remove("RALLY_HOOKS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            r#"{{"tool_input":{{"file_path":"{path}"}}}}"#
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
    }

    let room = workspace.room();
    let owners = room["data"]["room"]["active_claims"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|claim| claim["tool"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(owners, ["codex:ppid-4242", "codex:ppid-4242"]);
}

#[test]
fn shipped_wrapper_uses_native_before_write_without_node() {
    let workspace = Workspace::new("wrapper-no-node");
    let hook =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh");
    let mut child = Command::new(hook)
        .args(["before-write", "codex"])
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env("PATH", "/usr/bin:/bin")
        .env("RALLY_BIN", env!("CARGO_BIN_EXE_rally"))
        .env("RALLY_SESSION_ID", "native-no-node")
        .env("RALLY_HOOK_TIMEOUT_MS", "15000")
        .env_remove("RALLY_HOOKS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_input":{"file_path":"src/no-node.rs"}}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("node is not on PATH"),
        "native before-write must not emit the legacy Node dependency warning"
    );
}

#[test]
fn idle_transition_makes_same_path_publish_working_again() {
    let workspace = Workspace::new("working-transition");
    let hook =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh");
    let run_hook = |phase: &str, input: &str| {
        let mut child = Command::new(&hook)
            .args([phase, "codex"])
            .current_dir(&workspace.cwd)
            .env("HOME", &workspace.home)
            .env("PATH", "/usr/bin:/bin")
            .env("RALLY_BIN", env!("CARGO_BIN_EXE_rally"))
            .env_remove("RALLY_HOOKS")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    };
    let input = r#"{"session_id":"transition","tool_input":{"file_path":"src/transition.rs"}}"#;

    run_hook("before-write", input);
    let first_status = workspace.output(&["status", "read", "--json"]);
    let first_body: Value = serde_json::from_slice(&first_status.stdout).unwrap();
    let first_seq = first_body["data"]["status_read"]["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["tool"] == "codex:transition")
        .unwrap()["last_seen_seq"]
        .as_u64()
        .unwrap();
    run_hook("idle", "");
    run_hook("before-write", input);

    let status = workspace.output(&["status", "read", "--json"]);
    assert!(status.status.success());
    let body: Value = serde_json::from_slice(&status.stdout).unwrap();
    let state = body["data"]["status_read"]["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["tool"] == "codex:transition")
        .unwrap();
    assert_eq!(state["state"], "working");
    assert_eq!(state["file"], "file:src/transition.rs");
    assert!(state["last_seen_seq"].as_u64().unwrap() > first_seq);
}

#[test]
fn warm_native_hook_has_an_opt_in_twenty_millisecond_gate() {
    let workspace = Workspace::new("warm-timing");
    let input = r#"{"session_id":"timing","tool_input":{"file_path":"src/lib.rs"}}"#;
    // First write publishes status + claim; second rebuilds the cache after
    // the claim mutation. Neither is part of the steady-state gate.
    for _ in 0..3 {
        assert_eq!(
            workspace.hook("codex", "codex:timing", input, false),
            serde_json::json!({})
        );
    }
    let mut samples = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let output = workspace.hook("codex", "codex:timing", input, false);
        samples.push(started.elapsed());
        assert_eq!(output, serde_json::json!({}));
    }
    samples.sort();
    let p95 = samples[19];
    eprintln!("warm native before-write p95={p95:?}");
    let bound = if std::env::var("RALLY_TIMING_TESTS").as_deref() == Ok("1") {
        Duration::from_millis(20)
    } else {
        // Default CI gate detects catastrophic regressions without treating a
        // contended shared runner as product latency evidence.
        Duration::from_millis(250)
    };
    assert!(
        p95 < bound,
        "warm native before-write p95 {p95:?} >= {bound:?}"
    );
}
