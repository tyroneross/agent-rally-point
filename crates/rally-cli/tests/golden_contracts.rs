// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use std::collections::BTreeMap;
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

    fn json(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap();
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

#[derive(Default)]
struct Normalizer {
    events: BTreeMap<String, String>,
    threads: BTreeMap<String, String>,
    keys: BTreeMap<String, String>,
}

impl Normalizer {
    fn normalize(&mut self, value: Value) -> Value {
        self.normalize_at(None, value)
    }

    fn normalize_at(&mut self, key: Option<&str>, value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let value = if matches!(
                            key.as_str(),
                            "channel" | "identity_dir" | "presence_written"
                        ) {
                            Value::String(format!("<{key}>"))
                        } else if matches!(
                            key.as_str(),
                            "time" | "received_at" | "signed_at" | "exported_at"
                        ) {
                            Value::String("<timestamp>".to_string())
                        } else if key == "age_seconds" || key == "log_bytes" {
                            Value::Number(0.into())
                        } else if key == "available_on_path" {
                            Value::Bool(false)
                        } else if matches!(key.as_str(), "event_hash" | "prev_entry_hash") {
                            Value::String("sha256:<hash>".to_string())
                        } else if key == "signature" && value.is_string() {
                            Value::String("<signature>".to_string())
                        } else {
                            self.normalize_at(Some(&key), value)
                        };
                        (key, value)
                    })
                    .collect(),
            ),
            Value::Array(values) => {
                let mut values = values
                    .into_iter()
                    .map(|value| self.normalize_at(key, value))
                    .collect::<Vec<_>>();
                if matches!(key, Some("source_event_ids" | "claim_ids" | "depends_on")) {
                    values.sort_by_key(|value| value.to_string());
                }
                Value::Array(values)
            }
            Value::String(value) => Value::String(self.normalize_string(key, value)),
            value => value,
        }
    }

    fn normalize_string(&mut self, key: Option<&str>, value: String) -> String {
        if value.starts_with("evt_") {
            return stable_token(&mut self.events, value, "evt");
        }
        if value.starts_with("thr_") {
            return stable_token(&mut self.threads, value, "thr");
        }
        if value.starts_with("key_") {
            return stable_token(&mut self.keys, value, "key");
        }
        if value.starts_with("sha256:") {
            return "sha256:<hash>".to_string();
        }
        if key == Some("signature") {
            return "<signature>".to_string();
        }
        if value.contains("/rally-golden-") || value.contains("/rally-cli-") {
            return "<temp-path>".to_string();
        }
        value
    }
}

fn stable_token(values: &mut BTreeMap<String, String>, value: String, prefix: &str) -> String {
    if let Some(existing) = values.get(&value) {
        return existing.clone();
    }
    let token = format!("{prefix}_{}", values.len() + 1);
    values.insert(value, token.clone());
    token
}

fn assert_golden(name: &str, actual: Value) {
    let mut normalizer = Normalizer::default();
    let actual = normalizer.normalize(actual);
    let actual_text = format!("{}\n", serde_json::to_string_pretty(&actual).unwrap());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);

    if std::env::var_os("RALLY_UPDATE_GOLDENS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual_text).unwrap();
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read golden {}: {err}; rerun with RALLY_UPDATE_GOLDENS=1",
            path.display()
        )
    });
    assert_eq!(expected, actual_text, "golden mismatch for {name}");
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rally-golden-{name}-{}",
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
fn context_and_packet_json_contracts_match_goldens() {
    let workspace = RallyWorkspace::new("context-packet");

    workspace.json(&[
        "profile",
        "--json",
        "--tool",
        "codex-reviewer",
        "--role",
        "reviewer",
        "--capability",
        "review",
        "--watch",
        "crates/rally-core",
    ]);
    workspace.json(&[
        "artifact",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "packet implementation notes",
        "--artifact-kind",
        "review-notes",
        "--uri",
        "crates/rally-core/src/context.rs",
    ]);
    workspace.json(&[
        "decision",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "packets are read-only derived state",
        "--status",
        "binding",
        "--scope",
        "crates/rally-core/src/context.rs",
    ]);
    workspace.json(&[
        "lesson",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "packets preserve source-linked context",
        "--lesson-kind",
        "coordination",
        "--confidence",
        "0.8",
    ]);

    assert_golden(
        "context_reviewer.json",
        workspace.json(&[
            "context",
            "--json",
            "--tool",
            "codex-reviewer",
            "--limit",
            "4",
        ]),
    );
    assert_golden(
        "packet_reviewer.json",
        workspace.json(&[
            "packet",
            "--json",
            "--tool",
            "codex-reviewer",
            "--limit",
            "4",
        ]),
    );
    assert_golden(
        "cmux_packet_reviewer.json",
        workspace.json(&[
            "cmux",
            "packet",
            "--json",
            "--tool",
            "codex-reviewer",
            "--limit",
            "4",
        ]),
    );
    assert_golden(
        "herdr_packet_reviewer.json",
        workspace.json(&[
            "herdr",
            "packet",
            "--json",
            "--tool",
            "codex-reviewer",
            "--limit",
            "4",
        ]),
    );

    workspace.cleanup();
}

#[test]
fn adapter_contract_and_checkpoint_json_contracts_match_goldens() {
    let workspace = RallyWorkspace::new("adapter-checkpoint");
    workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "checkpoint this",
    ]);

    assert_golden(
        "adapter_contract.json",
        workspace.json(&["adapter", "contract", "--json"]),
    );
    assert_golden(
        "checkpoint_rebuild.json",
        workspace.json(&["checkpoint", "rebuild", "--json"]),
    );
    assert_golden(
        "checkpoint_status_valid.json",
        workspace.json(&["checkpoint", "status", "--json"]),
    );

    workspace.cleanup();
}

#[test]
fn herdr_gate_json_contract_matches_golden() {
    let workspace = RallyWorkspace::new("herdr-gate");
    let handoff = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "inject this into herdr",
        "--files",
        "crates/rally-core/src/context.rs",
    ]);
    let handoff_id = handoff["event_id"].as_str().unwrap();

    assert_golden(
        "herdr_inject_unsigned.json",
        workspace.json(&["herdr", "inject", "--json", handoff_id]),
    );

    workspace.cleanup();
}

#[test]
fn sync_json_contracts_match_goldens() {
    let source = RallyWorkspace::new("sync-source");
    let destination = RallyWorkspace::new("sync-destination");
    let identity_dir = temp_path("identity");
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
    source.json(&[
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
        "--files",
        "docs/SIGNED_EVENTS.md",
    ]);

    let packet = source.json(&["sync", "export", "--json"]);
    assert_golden("sync_export_signed.json", packet.clone());

    let packet_path = write_json("sync-packet", &packet);
    let trust_path = identity_dir.join("trust.toml");
    assert_golden(
        "sync_import_trusted.json",
        destination.json(&[
            "sync",
            "import",
            "--json",
            "--trust-policy",
            trust_path.to_str().unwrap(),
            packet_path.to_str().unwrap(),
        ]),
    );

    fs::remove_file(packet_path).ok();
    fs::remove_dir_all(identity_dir).ok();
    source.cleanup();
    destination.cleanup();
}
