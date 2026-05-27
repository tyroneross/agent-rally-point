// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct RallyWorkspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl RallyWorkspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
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

fn write_json(name: &str, value: &Value) -> PathBuf {
    let path = temp_path(name).with_extension("json");
    fs::write(&path, serde_json::to_string(value).unwrap()).unwrap();
    path
}

#[test]
fn agent_starts_from_preflight_and_clears_a_required_handoff() {
    let workspace = RallyWorkspace::new("journey-preflight-handoff");

    let handoff = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "review sync import",
        "--files",
        "crates/rally-core/src/sync.rs",
        "docs/SIGNED_EVENTS.md",
    ]);
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let preflight = workspace.json(&[
        "preflight",
        "--json",
        "--tool",
        "codex",
        "--session-id",
        "codex-journey",
        "--start-ping",
    ]);
    assert_eq!(preflight["routing"]["action"], "join_active");
    assert_eq!(preflight["pending_acks_for_me"][0]["event_id"], handoff_id);
    assert_eq!(
        preflight["pending_acks_for_me"][0]["files"][0],
        "crates/rally-core/src/sync.rs"
    );

    let context = workspace.json(&["context", "--json", "--tool", "codex"]);
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["action"],
        "ack_handoff"
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["target"],
        handoff_id
    );

    workspace.json(&[
        "ack",
        "--json",
        "--tool",
        "codex",
        "--summary",
        "review complete",
        handoff_id,
    ]);
    let inbox = workspace.json(&["inbox", "--json", "--tool", "codex"]);
    workspace.cleanup();

    assert_eq!(inbox["data"]["pending"].as_array().unwrap().len(), 0);
}

#[test]
fn agents_detect_and_resolve_overlapping_file_claims() {
    let workspace = RallyWorkspace::new("journey-file-claims");

    let codex_claim = workspace.json(&[
        "claim",
        "--json",
        "--tool",
        "codex",
        "--path",
        "crates/rally-core/src/query.rs",
        "--subject",
        "tighten projections",
    ]);
    let codex_claim_id = codex_claim["event_id"].as_str().unwrap();
    workspace.json(&[
        "claim",
        "--json",
        "--tool",
        "pi",
        "--path",
        "crates/rally-core/src/query.rs",
        "--subject",
        "review projections",
    ]);

    let conflicts = workspace.json(&["conflicts", "--json"]);
    assert_eq!(
        conflicts["data"]["conflicts"][0]["resource"],
        "file:crates/rally-core/src/query.rs"
    );
    assert_eq!(
        conflicts["data"]["conflicts"][0]["owners"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let diagnosis = workspace.json(&["diagnose", "--json"]);
    assert!(
        diagnosis["data"]["diagnosis"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "claim-conflict")
    );

    workspace.json(&[
        "release",
        "--json",
        "--tool",
        "codex",
        "--reason",
        "handed off to pi",
        codex_claim_id,
    ]);
    let cleared = workspace.json(&["conflicts", "--json"]);
    workspace.cleanup();

    assert_eq!(cleared["data"]["conflicts"].as_array().unwrap().len(), 0);
}

#[test]
fn signed_handoff_can_be_synced_to_another_workspace_and_acked() {
    let source = RallyWorkspace::new("journey-sync-source");
    let destination = RallyWorkspace::new("journey-sync-destination");
    let identity_dir = temp_path("journey-sync-identity");
    let identity_arg = identity_dir.to_str().unwrap();

    source.json(&[
        "identity",
        "init",
        "--json",
        "--identity-dir",
        identity_arg,
        "--tool",
        "codex",
    ]);
    let handoff = source.json(&[
        "handoff",
        "--json",
        "--identity-dir",
        identity_arg,
        "--sign",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "review signed sync packet",
    ]);
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let packet = source.json(&["sync", "export", "--json"]);
    let packet_path = write_json("journey-sync-packet", &packet);
    let trust_path = identity_dir.join("trust.toml");

    let imported = destination.json(&[
        "sync",
        "import",
        "--json",
        "--trust-policy",
        trust_path.to_str().unwrap(),
        packet_path.to_str().unwrap(),
    ]);
    assert_eq!(imported["data"]["trust_counts"]["trusted"], 1);

    let inbox = destination.json(&["inbox", "--json", "--tool", "pi"]);
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);
    assert_eq!(inbox["data"]["pending"][0]["origin"], "import:sync");
    assert_eq!(inbox["data"]["pending"][0]["trust_status"], "trusted");

    destination.json(&[
        "ack",
        "--json",
        "--tool",
        "pi",
        "--summary",
        "packet reviewed",
        handoff_id,
    ]);
    let cleared = destination.json(&["preflight", "--json", "--tool", "pi"]);

    fs::remove_file(packet_path).unwrap();
    source.cleanup();
    destination.cleanup();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(cleared["pending_acks_for_me"].as_array().unwrap().len(), 0);
}
