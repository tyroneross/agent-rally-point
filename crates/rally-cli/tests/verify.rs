// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
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

#[test]
fn verify_reports_unsigned_store_entry() {
    let path = temp_jsonl(
        "rally-cli-unsigned",
        r#"{"local_seq":1,"received_at":"2026-05-26T18:00:00.000Z","origin":"local","event":{"id":"evt_11111111111111111111111111111111","kind":"handoff","type":"agent-rally.handoff.created.v1","tool":"codex","payload":{"subject":"smoke"}}}"#,
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
        &serde_json::to_string(&json!({
            "local_seq": 1,
            "received_at": "2026-05-26T18:00:01.000Z",
            "origin": "local",
            "event": record
        }))
        .unwrap(),
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
    assert_eq!(body["records"], 1);
    assert_eq!(body["counts"]["trusted"], 1);
    assert_eq!(body["events"][0]["status"], "trusted");
    assert_eq!(body["events"][0]["key_id"], "key_codex_test");
}
