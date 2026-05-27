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
                            "channel" | "identity_dir" | "presence_written" | "installed_path"
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

fn assert_matches_schema(schema_name: &str, value: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schemas")
        .join(schema_name);
    let schema: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    validate_schema(&schema, value, "$");
}

fn validate_schema(schema: &Value, value: &Value, path: &str) {
    if let Some(expected) = schema.get("const") {
        assert_eq!(expected, value, "schema const mismatch at {path}");
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        assert!(
            options.contains(value),
            "schema enum mismatch at {path}: {value}"
        );
    }
    if let Some(type_schema) = schema.get("type") {
        assert!(
            type_matches(type_schema, value),
            "schema type mismatch at {path}: {value}"
        );
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("schema required used on non-object at {path}"));
        for key in required.iter().filter_map(Value::as_str) {
            assert!(
                object.contains_key(key),
                "schema missing required key {path}.{key}"
            );
        }
    }
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (key, property_schema) in properties {
            if let Some(child) = object.get(key) {
                validate_schema(property_schema, child, &format!("{path}.{key}"));
            }
        }
    }
    if let (Some(item_schema), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, child) in array.iter().enumerate() {
            validate_schema(item_schema, child, &format!("{path}[{index}]"));
        }
    }
}

fn type_matches(type_schema: &Value, value: &Value) -> bool {
    if let Some(types) = type_schema.as_array() {
        return types.iter().any(|schema| type_matches(schema, value));
    }
    match type_schema.as_str().unwrap() {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => {
            value.as_f64().is_some() || value.as_i64().is_some() || value.as_u64().is_some()
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        other => panic!("unsupported schema type {other}"),
    }
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
    assert_golden(
        "start_pi.json",
        workspace.json(&["pi", "--session-id", "pi-golden", "--limit", "4"]),
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
    assert_golden("setup_status.json", workspace.json(&["setup", "--json"]));
    assert_golden(
        "setup_enforcement_strict.json",
        workspace.json(&["setup", "enforcement", "strict", "--json"]),
    );
    assert_golden(
        "setup_install_cmux.json",
        workspace.json(&["setup", "install", "cmux", "--json"]),
    );
    assert_golden(
        "doctor_claude.json",
        workspace.json(&["doctor", "--json", "--tool", "claude"]),
    );

    workspace.cleanup();
}

#[test]
fn golden_outputs_match_formal_json_schemas() {
    let workspace = RallyWorkspace::new("schema-validation");
    workspace.json(&[
        "artifact",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "schema validation",
        "--artifact-kind",
        "test-contract",
        "--uri",
        "docs/schemas",
    ]);
    let start = workspace.json(&["pi", "--session-id", "schema-session"]);
    assert_matches_schema("agent-rally.command.start.v1.json", &start);
    let packet = workspace.json(&["packet", "--json", "--tool", "pi"]);
    assert_matches_schema("agent-rally.command.packet.v1.json", &packet);
    let doctor = workspace.json(&["doctor", "--json", "--tool", "pi"]);
    assert_matches_schema("agent-rally.command.doctor.v1.json", &doctor);
    let setup = workspace.json(&["setup", "--json"]);
    assert_matches_schema("agent-rally.command.setup.v1.json", &setup);
    let next = workspace.json(&["next", "--json", "--tool", "pi"]);
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    let judge = workspace.json(&["judge", "--json", "--tool", "pi"]);
    assert_matches_schema("agent-rally.command.judge.v1.json", &judge);
    let hook = workspace.json(&["hook", "idle", "--json", "--tool", "pi"]);
    assert_matches_schema("agent-rally.command.hook.v1.json", &hook);
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
