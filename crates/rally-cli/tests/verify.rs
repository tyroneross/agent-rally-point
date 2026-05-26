// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use rally_core::store::store_entry_value;
use rally_protocol::{CANONICALIZATION_VERSION, canonical_event_bytes};
use serde_json::json;

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

fn write_json(name: &str, value: &serde_json::Value) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{name}-{}.json",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&path, serde_json::to_string(value).unwrap()).unwrap();
    path
}

fn temp_channel(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn run_rally(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .args(args)
        .output()
        .unwrap()
}

fn json_stdout(output: std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn verify_reports_unsigned_store_entry() {
    let entry = store_entry_value(
        json!({
            "id": "evt_11111111111111111111111111111111",
            "kind": "handoff",
            "type": "agent-rally.handoff.created.v1",
            "tool": "codex",
            "payload": {"subject": "smoke"}
        }),
        1,
        None,
        "local",
    )
    .unwrap();
    let path = temp_jsonl(
        "rally-cli-unsigned",
        &format!("{}\n", serde_json::to_string(&entry).unwrap()),
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
fn write_commands_append_typed_handoff_and_ack_events() {
    let channel = temp_channel("rally-cli-typed-handoff");
    let channel_arg = channel.to_str().unwrap();
    let handoff = json_stdout(run_rally(&[
        "handoff",
        "--json",
        "--channel-dir",
        channel_arg,
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "review schema",
        "--files",
        "docs/SCHEMA.md",
    ]));

    let handoff_id = handoff["event_id"].as_str().unwrap();
    assert_eq!(handoff["ok"], true);
    assert_eq!(handoff["command"], "handoff");
    assert_eq!(handoff["local_seq"], 1);
    assert_eq!(handoff["event"]["kind"], "handoff");
    assert_eq!(handoff["event"]["type"], "agent-rally.handoff.created.v1");
    assert_eq!(
        handoff["event"]["dataschema"],
        "urn:agent-rally-point:schema:handoff.created.v1"
    );
    assert_eq!(handoff["event"]["payload"]["to_tool"], "codex");
    assert_eq!(
        handoff["event"]["payload"]["ref_files"][0],
        "docs/SCHEMA.md"
    );

    let ack = json_stdout(run_rally(&[
        "ack",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--summary",
        "done",
        handoff_id,
    ]));
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(ack["ok"], true);
    assert_eq!(ack["command"], "ack");
    assert_eq!(ack["local_seq"], 2);
    assert_eq!(ack["event"]["type"], "agent-rally.handoff.acknowledged.v1");
    assert_eq!(
        ack["event"]["dataschema"],
        "urn:agent-rally-point:schema:handoff.acknowledged.v1"
    );
    assert_eq!(ack["event"]["causation_id"], handoff_id);
    assert_eq!(ack["event"]["thread_id"], handoff["event"]["thread_id"]);
    assert_eq!(ack["event"]["payload"]["ref_handoff_id"], handoff_id);
    assert_eq!(ack["event"]["payload"]["verdict"], "done");
}

#[test]
fn write_commands_cover_claim_and_blocker_lifecycles() {
    let channel = temp_channel("rally-cli-typed-lifecycles");
    let channel_arg = channel.to_str().unwrap();
    let claim = json_stdout(run_rally(&[
        "claim",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--path",
        "crates/rally-cli/src/main.rs",
        "--subject",
        "wire typed writes",
    ]));
    let claim_id = claim["event_id"].as_str().unwrap();
    assert_eq!(claim["event"]["type"], "agent-rally.claim.created.v1");
    assert_eq!(
        claim["event"]["payload"]["resource"],
        "file:crates/rally-cli/src/main.rs"
    );

    let release = json_stdout(run_rally(&[
        "release",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--reason",
        "done",
        claim_id,
    ]));
    assert_eq!(release["event"]["type"], "agent-rally.claim.released.v1");
    assert_eq!(release["event"]["causation_id"], claim_id);
    assert_eq!(release["event"]["thread_id"], claim["event"]["thread_id"]);

    let blocker = json_stdout(run_rally(&[
        "blocker",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--subject",
        "need branch",
        "--reason",
        "which branch?",
        "--severity",
        "blocked",
    ]));
    let blocker_id = blocker["event_id"].as_str().unwrap();
    assert_eq!(blocker["event"]["type"], "agent-rally.blocker.raised.v1");
    assert_eq!(blocker["event"]["payload"]["reason"], "which branch?");

    let unblock = json_stdout(run_rally(&[
        "unblock",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        blocker_id,
        "--resolution",
        "branch supplied",
    ]));
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(unblock["event"]["type"], "agent-rally.blocker.resolved.v1");
    assert_eq!(unblock["event"]["causation_id"], blocker_id);
    assert_eq!(unblock["event"]["thread_id"], blocker["event"]["thread_id"]);
}

#[test]
fn write_command_usage_errors_honor_json_mode() {
    let output = run_rally(&["handoff", "--json", "--to", "codex"]);

    assert_eq!(output.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["command"], "handoff");
    assert_eq!(body["exit_code"], 2);
    assert!(body["error"].as_str().unwrap().contains("--subject"));
}

#[test]
fn query_commands_project_typed_state() {
    let channel = temp_channel("rally-cli-query-state");
    let channel_arg = channel.to_str().unwrap();
    let handoff = json_stdout(run_rally(&[
        "handoff",
        "--json",
        "--channel-dir",
        channel_arg,
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "review schema",
    ]));
    let handoff_id = handoff["event_id"].as_str().unwrap();
    json_stdout(run_rally(&[
        "claim",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--resource",
        "file:docs",
        "--subject",
        "docs edit",
    ]));
    json_stdout(run_rally(&[
        "claim",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "pi",
        "--path",
        "docs/SCHEMA.md",
        "--subject",
        "schema edit",
    ]));
    let blocker = json_stdout(run_rally(&[
        "blocker",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
        "--subject",
        "need branch",
    ]));
    let blocker_id = blocker["event_id"].as_str().unwrap();

    let inbox = json_stdout(run_rally(&[
        "inbox",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
    ]));
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);

    let claims = json_stdout(run_rally(&[
        "claims",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
    ]));
    assert_eq!(claims["data"]["claims"].as_array().unwrap().len(), 1);
    assert_eq!(claims["data"]["claims"][0]["resource"], "file:docs");

    let blockers = json_stdout(run_rally(&[
        "blockers",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
    ]));
    assert_eq!(blockers["data"]["blockers"][0]["event_id"], blocker_id);

    let conflicts = json_stdout(run_rally(&[
        "conflicts",
        "--json",
        "--channel-dir",
        channel_arg,
    ]));
    assert_eq!(conflicts["data"]["conflicts"][0]["resource"], "file:docs");
    assert_eq!(
        conflicts["data"]["conflicts"][0]["owners"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let diagnosis = json_stdout(run_rally(&[
        "diagnose",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
    ]));
    assert_eq!(diagnosis["data"]["diagnosis"]["status"], "stuck");
    assert!(
        diagnosis["data"]["diagnosis"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "active-blocker")
    );

    let score = json_stdout(run_rally(&[
        "score",
        "--json",
        "--channel-dir",
        channel_arg,
        "--tool",
        "codex",
    ]));
    assert!(score["data"]["score"].as_i64().unwrap() < 100);

    let thread = json_stdout(run_rally(&[
        "thread",
        "--json",
        "--channel-dir",
        channel_arg,
        handoff_id,
    ]));
    assert_eq!(thread["data"]["identifier"], handoff_id);
    assert_eq!(thread["data"]["events"][0]["event"]["id"], handoff_id);

    let replay = json_stdout(run_rally(&[
        "replay",
        "--json",
        "--channel-dir",
        channel_arg,
        "--limit",
        "2",
    ]));
    assert_eq!(replay["data"]["events"].as_array().unwrap().len(), 2);

    let report = json_stdout(run_rally(&[
        "report",
        "--json",
        "--channel-dir",
        channel_arg,
    ]));
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(report["data"]["records"], 4);
    assert_eq!(report["data"]["counts"]["claim"], 2);
    assert_eq!(report["data"]["counts"]["handoff"], 1);
}

#[test]
fn identity_init_and_signed_write_verify_as_trusted() {
    let channel = temp_channel("rally-cli-signed-channel");
    let identity_dir = temp_channel("rally-cli-identity");
    let channel_arg = channel.to_str().unwrap();
    let identity_arg = identity_dir.to_str().unwrap();

    let identity = json_stdout(run_rally(&[
        "identity",
        "init",
        "--json",
        "--identity-dir",
        identity_arg,
        "--tool",
        "codex",
    ]));
    assert_eq!(identity["ok"], true);
    assert!(identity["key_id"].as_str().unwrap().starts_with("key_"));

    let handoff = json_stdout(run_rally(&[
        "handoff",
        "--json",
        "--channel-dir",
        channel_arg,
        "--identity-dir",
        identity_arg,
        "--sign",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "signed review",
    ]));
    assert_eq!(handoff["event"]["signature"]["key_id"], identity["key_id"]);
    assert_eq!(
        handoff["event"]["signature"]["canonicalization"],
        "rally-json-v1"
    );

    let verify = json_stdout(run_rally(&[
        "verify",
        "--json",
        "--trust-policy",
        identity_dir.join("trust.toml").to_str().unwrap(),
        channel.join("changes.jsonl").to_str().unwrap(),
    ]));
    fs::remove_dir_all(channel).unwrap();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(verify["data"]["counts"]["trusted"], 1);
    assert_eq!(verify["data"]["events"][0]["status"], "trusted");
    assert_eq!(verify["data"]["events"][0]["key_id"], identity["key_id"]);
}

#[test]
fn sync_export_import_round_trips_signed_events() {
    let source = temp_channel("rally-cli-sync-source");
    let dest = temp_channel("rally-cli-sync-dest");
    let identity_dir = temp_channel("rally-cli-sync-identity");
    let source_arg = source.to_str().unwrap();
    let dest_arg = dest.to_str().unwrap();
    let identity_arg = identity_dir.to_str().unwrap();

    let identity = json_stdout(run_rally(&[
        "identity",
        "init",
        "--json",
        "--identity-dir",
        identity_arg,
        "--tool",
        "codex",
    ]));
    let key_id = identity["key_id"].as_str().unwrap();
    let handoff = json_stdout(run_rally(&[
        "handoff",
        "--json",
        "--channel-dir",
        source_arg,
        "--identity-dir",
        identity_arg,
        "--key-id",
        key_id,
        "--sign",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "sync me",
    ]));
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let packet = json_stdout(run_rally(&[
        "sync",
        "export",
        "--json",
        "--channel-dir",
        source_arg,
    ]));
    assert_eq!(packet["schema"], "agent-rally.sync.packet.v1");
    assert_eq!(packet["count"], 1);
    assert_eq!(packet["events"][0]["id"], handoff_id);
    assert!(packet["events"][0].get("local_seq").is_none());
    let packet_path = write_json("rally-cli-sync-packet", &packet);

    let imported = json_stdout(run_rally(&[
        "sync",
        "import",
        "--json",
        "--channel-dir",
        dest_arg,
        "--trust-policy",
        identity_dir.join("trust.toml").to_str().unwrap(),
        packet_path.to_str().unwrap(),
    ]));
    assert_eq!(imported["data"]["imported"], 1);
    assert_eq!(imported["data"]["duplicates"], 0);
    assert_eq!(imported["data"]["trust_counts"]["trusted"], 1);

    let inbox = json_stdout(run_rally(&[
        "inbox",
        "--json",
        "--channel-dir",
        dest_arg,
        "--tool",
        "pi",
    ]));
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);

    let duplicate = json_stdout(run_rally(&[
        "sync",
        "import",
        "--json",
        "--channel-dir",
        dest_arg,
        packet_path.to_str().unwrap(),
    ]));
    assert_eq!(duplicate["data"]["imported"], 0);
    assert_eq!(duplicate["data"]["duplicates"], 1);

    let mut conflict_packet = packet.clone();
    conflict_packet["events"][0]["payload"]["subject"] = serde_json::json!("different");
    let conflict_path = write_json("rally-cli-sync-conflict", &conflict_packet);
    let conflict = json_stdout(run_rally(&[
        "sync",
        "import",
        "--json",
        "--channel-dir",
        dest_arg,
        conflict_path.to_str().unwrap(),
    ]));
    fs::remove_file(packet_path).unwrap();
    fs::remove_file(conflict_path).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(dest).unwrap();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(conflict["data"]["imported"], 0);
    assert_eq!(conflict["data"]["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(conflict["data"]["conflicts"][0]["event_id"], handoff_id);
}

#[test]
fn usage_errors_exit_nonzero_for_agent_automation() {
    let output = Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: rally-rs verify"));
}

#[test]
fn verify_json_uses_explicit_trust_policy() {
    let mut record = json!({
        "id": "evt_11111111111111111111111111111111",
        "kind": "handoff",
        "type": "agent-rally.handoff.created.v1",
        "tool": "codex",
        "payload": {"subject": "trusted smoke"}
    });
    let signing_key = SigningKey::from_bytes(&[11; 32]);
    let signature = signing_key.sign(&canonical_event_bytes(&record).unwrap());
    record.as_object_mut().unwrap().insert(
        "signature".into(),
        json!({
            "version": "rally-signature-v1",
            "algorithm": "ed25519",
            "key_id": "key_codex_test",
            "signed_at": "2026-05-26T18:00:00.000Z",
            "signature": STANDARD.encode(signature.to_bytes()),
            "canonicalization": CANONICALIZATION_VERSION
        }),
    );
    let changes = temp_jsonl(
        "rally-cli-trusted",
        &format!(
            "{}\n",
            serde_json::to_string(&store_entry_value(record, 1, None, "local").unwrap()).unwrap()
        ),
    );
    let trust = std::env::temp_dir().join(format!(
        "rally-cli-trust-{}.toml",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(
        &trust,
        format!(
            r#"
[[keys]]
key_id = "key_codex_test"
public_key = "{}"
trusted_tools = ["codex"]
allowed_kinds = ["handoff"]
"#,
            STANDARD.encode(signing_key.verifying_key().to_bytes())
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .arg("verify")
        .arg("--json")
        .arg("--trust-policy")
        .arg(&trust)
        .arg(&changes)
        .output()
        .unwrap();
    fs::remove_file(changes).unwrap();
    fs::remove_file(trust).unwrap();

    assert!(output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["command"], "verify");
    assert_eq!(body["schema"], "agent-rally.command.verify.v1");
    assert_eq!(body["data"]["records"], 1);
    assert_eq!(body["data"]["counts"]["trusted"], 1);
    assert_eq!(body["data"]["events"][0]["status"], "trusted");
    assert_eq!(body["data"]["events"][0]["key_id"], "key_codex_test");
}

#[test]
fn verify_json_errors_use_command_envelope() {
    let path = temp_jsonl("rally-cli-bad-store", "{\n");

    let output = Command::new(env!("CARGO_BIN_EXE_rally-rs"))
        .arg("verify")
        .arg("--json")
        .arg(&path)
        .output()
        .unwrap();
    fs::remove_file(path).unwrap();

    assert!(!output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["command"], "verify");
    assert_eq!(body["exit_code"], 1);
    assert!(body["error"].as_str().unwrap().contains("invalid"));
}
