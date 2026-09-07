// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
#![cfg(unix)]
#[path = "support/rally_cmd.rs"]
mod rally_cmd;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Output};

struct Room(PathBuf);
impl Room {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("rally-optimization-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join(".git")).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        rally_cmd::rally_command()
            .current_dir(&self.0)
            .env("RALLY_SESSION_ID", "optimization-contract")
            .env("RALLY_HOOKS", "off")
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let p = self.run(args);
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        let value: Value = serde_json::from_slice(&p.stdout).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        assert_ne!(value["command"], "watchdog", "{value}");
        value
    }
}
impl Drop for Room {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn compact_cli_observes_without_writes_and_does_not_hide_old_active_claim() {
    let room = Room::new();
    let claimed = room.json(&[
        "say",
        "claim",
        "--tool",
        "claude:test",
        "--subject",
        "owned",
        "--path",
        "src/shared.rs",
        "--json",
    ]);
    let id = claimed["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let before = room.json(&["room", "--json"])["data"]["room"]["max_seq"]
        .as_i64()
        .unwrap();
    let seq = before.to_string();
    for _ in 0..2 {
        let view = room.json(&[
            "room",
            "--compact",
            "--tool",
            "gemini:test",
            "--path",
            "src/shared.rs",
            "--since",
            &seq,
            "--json",
        ]);
        assert_eq!(view["data"]["changed"], false);
        assert_eq!(view["data"]["claims"][0]["event_id"], id);
        assert_eq!(view["data"]["advisory"], true);
    }
    let after = room.json(&["room", "--json"]);
    assert_eq!(after["data"]["room"]["max_seq"], before);
    assert!(!room.run(&["room", "--compact", "--json"]).status.success());
}

#[test]
fn custom_host_argv_is_explicit_and_preserves_literal_arguments() {
    let room = Room::new();
    for host in [
        "codex",
        "claude",
        "gemini",
        "cursor",
        "rosslabs-agent-harness",
        "future-llm",
    ] {
        let argv = json!([
            "/host executable",
            "literal $(touch should-not-exist)",
            "--some-flag"
        ])
        .to_string();
        let value = room.json(&[
            "run",
            host,
            "--command-json",
            &argv,
            "--dry-run",
            "--backend",
            "tmux",
            "--shared",
            "--json",
        ]);
        assert_eq!(value["data"]["run"]["session"]["agent"], host);
        assert!(
            value
                .to_string()
                .contains("literal $(touch should-not-exist)")
        );
        assert!(!room.0.join("should-not-exist").exists());
    }
    for argv in ["[]", "{}", "[\"\"]", "[\"x\\u0000y\"]"] {
        assert!(
            !room
                .run(&[
                    "run",
                    "custom",
                    "--command-json",
                    argv,
                    "--dry-run",
                    "--json"
                ])
                .status
                .success()
        );
    }
    assert!(
        !room
            .run(&[
                "run",
                "custom",
                "--command-json",
                "[\"harness\"]",
                "--task",
                "prompt",
                "--dry-run",
                "--json"
            ])
            .status
            .success()
    );
}

#[test]
fn committed_timeout_includes_exact_fact_locator() {
    if !cfg!(debug_assertions) {
        return;
    } // test-only delay is absent in release
    let room = Room::new();
    room.json(&["room", "--json"]);
    let p = rally_cmd::rally_command()
        .current_dir(&room.0)
        .env("RALLY_SESSION_ID", "optimization-contract")
        .env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", "5000")
        .args([
            "say",
            "handoff",
            "--tool",
            "cursor:test",
            "--subject",
            "timeout locator",
            "--timeout-ms",
            "2000",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
    let value: Value = serde_json::from_slice(&p.stdout).unwrap();
    assert_eq!(value["command"], "watchdog");
    let id = value["data"]["watchdog"]["event_id"]
        .as_str()
        .expect("committed fact id");
    assert!(
        value["data"]["watchdog"]["query_remedy"]
            .as_str()
            .unwrap()
            .contains(id)
    );
    let full = room.json(&["room", "--json"]);
    assert_eq!(full["data"]["room"]["open_handoffs"][0]["event_id"], id);
}
