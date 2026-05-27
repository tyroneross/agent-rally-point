// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::DateTime;
use rusqlite::Connection;
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

    fn json(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn json_with_status(&self, args: &[&str]) -> (Value, Output) {
        let output = self.output(args);
        let value = serde_json::from_slice(&output.stdout).unwrap();
        (value, output)
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally2"))
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

#[test]
fn rally2_agent_enters_room_checks_work_and_says_artifact() {
    let workspace = Workspace::new("rally2-room");

    let claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude",
        "--path",
        "src/room.rs",
        "--subject",
        "shape room projection",
    ]);
    assert_eq!(claim["schema"], "agent-rally2.command.say.v1");
    assert_matches_schema("agent-rally2.command.say.v1.json", &claim);
    assert_matches_schema("agent-rally2.fact.v1.json", &claim["data"]["fact"]);
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap();

    workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "Rally 2 uses enter/say/room/check",
        "--path",
        "src/room.rs",
    ]);

    let enter = workspace.json(&[
        "enter",
        "--json",
        "--tool",
        "codex",
        "--session-id",
        "codex-main",
        "--path",
        "src/room.rs",
    ]);
    assert_eq!(enter["schema"], "agent-rally2.command.enter.v1");
    assert_matches_schema("agent-rally2.command.enter.v1.json", &enter);
    assert_eq!(enter["data"]["adapter"]["adapter"], "codex");
    assert_eq!(enter["data"]["adapter"]["first_class"], true);
    assert!(
        enter["data"]["entry"]["do_not"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["event_id"] == claim_id)
    );
    assert!(
        enter["data"]["entry"]["know"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["subject"] == "Rally 2 uses enter/say/room/check")
    );

    let (check, check_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        "src/room.rs",
        "--strict",
    ]);
    assert_eq!(check_output.status.code(), Some(4));
    assert_eq!(check["schema"], "agent-rally2.command.check.v1");
    assert_matches_schema("agent-rally2.command.check.v1.json", &check);
    assert_eq!(check["data"]["check"]["allow"], false);
    assert!(
        check["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "claimed-path")
    );

    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude",
        "--subject",
        "room projection implemented",
        "--uri",
        "src/room.rs",
        "--evidence",
        "cargo test -p rally2-cli rally2_agent_enters_room_checks_work_and_says_artifact",
    ]);
    let room_without_export = workspace.json(&["room", "--json"]);
    assert!(!workspace.cwd.join("HANDOFF.md").exists());
    assert_eq!(room_without_export["data"]["exported_handoff"], Value::Null);

    let room = workspace.json(&["room", "--json", "--export-handoff"]);
    assert_eq!(room["schema"], "agent-rally2.command.room.v1");
    assert_matches_schema("agent-rally2.command.room.v1.json", &room);
    assert_eq!(
        room["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        room["data"]["room"]["current_decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        room["data"]["room"]["recent_artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        room["data"]["room"]["adapters"].as_array().unwrap().len(),
        6
    );
    let handoff = fs::read_to_string(workspace.cwd.join("HANDOFF.md")).unwrap();
    assert!(handoff.contains("## Do Not Touch"));
    assert!(handoff.contains("## Active Work"));
    assert!(handoff.contains("room projection implemented"));
    assert!(workspace.cwd.join(".rally2/facts.jsonl").exists());
    assert!(workspace.cwd.join(".rally2/room.db").exists());
    let conn = Connection::open(workspace.cwd.join(".rally2/room.db")).unwrap();
    let graph_edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    assert!(graph_edges >= 4);

    workspace.cleanup();
}

#[test]
fn rally2_is_not_a_legacy_command_fallback() {
    let workspace = Workspace::new("rally2-no-legacy");
    let help = workspace.output(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally2 enter --tool <tool>"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally2 next --tool <tool>"));

    let output = workspace.output(&["context", "--json", "--tool", "codex"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["product"], "rally2");
    assert_eq!(error["exit_code"], 2);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("unknown Rally 2 command context")
    );
    workspace.cleanup();
}

#[test]
fn rally2_next_finds_useful_work_while_waiting() {
    let workspace = Workspace::new("rally2-next-useful");
    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code",
        "--subject",
        "need review",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();
    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "adapter notes ready",
        "--uri",
        "docs/notes.md",
        "--evidence",
        "notes captured",
    ]);
    let artifact_id = artifact["data"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex", "--limit", "4"]);
    assert_eq!(next["schema"], "agent-rally2.command.next.v1");
    assert_matches_schema("agent-rally2.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["mode"], "useful_while_waiting");
    assert_eq!(next["data"]["next"]["action"], "review_artifact");
    assert_eq!(next["data"]["next"]["target_event_id"], artifact_id);
    assert!(
        next["data"]["next"]["waiting_on"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["event_id"] == handoff_id)
    );
    assert!(
        next["data"]["next"]["alternatives"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["action"] == "clarify_handoff")
    );

    workspace.cleanup();
}

#[test]
fn rally2_next_waits_only_when_no_useful_work_exists() {
    let workspace = Workspace::new("rally2-next-wait");
    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code",
        "--subject",
        "review requested",
        "--summary",
        "Claude has enough context to review the clean rewrite.",
        "--evidence",
        "cargo test -p rally2-cli",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex"]);
    assert_matches_schema("agent-rally2.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["mode"], "waiting");
    assert_eq!(next["data"]["next"]["action"], "wait");
    assert!(
        next["data"]["next"]["source_event_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str() == Some(handoff_id))
    );
    assert!(
        next["data"]["next"]["alternatives"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    workspace.cleanup();
}

#[test]
fn rally2_exposes_required_first_class_adapters() {
    let workspace = Workspace::new("rally2-adapters");
    let expected = ["codex", "claude_code", "pi", "herdr", "cmux", "ci"];

    for tool in expected {
        let entered = workspace.json(&["enter", "--json", "--tool", tool]);
        assert_eq!(entered["data"]["adapter"]["adapter"], tool);
        assert_eq!(entered["data"]["adapter"]["first_class"], true);
        assert_eq!(
            entered["data"]["adapter"]["commands"]["enter"],
            format!("rally2 enter --tool {tool} --json")
        );
        assert_eq!(
            entered["data"]["adapter"]["commands"]["next"],
            format!("rally2 next --tool {tool} --json")
        );
        assert_eq!(entered["data"]["adapter"]["surfaces"]["idle_next"], true);
        assert!(
            entered["data"]["adapter"]["model_visible"]
                .as_str()
                .unwrap()
                .contains("next")
        );
    }

    let room = workspace.json(&["room", "--json"]);
    let adapters = room["data"]["room"]["adapters"].as_array().unwrap();
    for tool in expected {
        let adapter = adapters
            .iter()
            .find(|adapter| adapter["adapter"] == tool)
            .unwrap();
        assert_eq!(
            adapter["commands"]["check_before_write"],
            format!("rally2 check before-write --tool {tool} --path <path> --json")
        );
        assert_eq!(
            adapter["commands"]["next_for_path"],
            format!("rally2 next --tool {tool} --path <path> --json")
        );
    }

    workspace.cleanup();
}

#[test]
fn rally2_entry_and_handoff_split_response_and_work_buckets() {
    let workspace = Workspace::new("rally2-entry-buckets");
    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--path",
        "src/owned.rs",
        "--subject",
        "codex active work",
    ]);
    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code",
        "--path",
        "src/other.rs",
        "--subject",
        "claude owns other work",
    ]);
    workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "pi",
        "--target",
        "codex",
        "--subject",
        "codex should review",
    ]);

    let enter = workspace.json(&["enter", "--json", "--tool", "codex"]);
    let do_items = enter["data"]["entry"]["do"].as_array().unwrap();
    let respond_to = enter["data"]["entry"]["respond_to"].as_array().unwrap();
    assert!(
        respond_to
            .iter()
            .all(|item| item["reason"] == "respond_to_handoff")
    );
    assert!(
        do_items
            .iter()
            .any(|item| item["reason"] == "respond_to_handoff")
    );
    assert!(
        do_items
            .iter()
            .any(|item| item["reason"] == "continue_or_release_claim")
    );
    assert!(do_items.len() > respond_to.len());

    workspace.json(&["room", "--json", "--tool", "codex", "--export-handoff"]);
    let handoff = fs::read_to_string(workspace.cwd.join("HANDOFF.md")).unwrap();
    let do_not_touch = handoff.split("## Active Work").next().unwrap();
    let active_work = handoff.split("## Active Work").nth(1).unwrap();
    assert!(do_not_touch.contains("claude owns other work"));
    assert!(!do_not_touch.contains("codex active work"));
    assert!(active_work.contains("codex active work"));

    workspace.cleanup();
}

#[test]
fn rally2_installs_adapter_glue_without_touching_legacy_hooks() {
    let workspace = Workspace::new("rally2-install");
    fs::create_dir_all(workspace.home.join(".codex")).unwrap();
    fs::write(
        workspace.home.join(".codex/rally-hook.sh"),
        "#!/bin/sh\nrally start codex\n",
    )
    .unwrap();
    fs::write(
        workspace.home.join(".codex/hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bash '/tmp/rally-hook.sh' start codex"}]}]}}"#,
    )
    .unwrap();

    let dry_run = workspace.json(&[
        "install",
        "codex",
        "--json",
        "--dry-run",
        "--rally2-bin",
        "/tmp/rally2",
    ]);
    assert_eq!(dry_run["schema"], "agent-rally2.command.install.v1");
    assert_matches_schema("agent-rally2.command.install.v1.json", &dry_run);
    assert_eq!(dry_run["data"]["mode"], "dry-run");
    assert!(!workspace.home.join(".codex/hooks/rally2-hook.sh").exists());

    let installed = workspace.json(&["install", "codex", "--json", "--rally2-bin", "/tmp/rally2"]);
    assert_eq!(installed["data"]["adapters"][0]["adapter"], "codex");
    assert!(
        installed["data"]["adapters"][0]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("Rally 1"))
    );
    let script = fs::read_to_string(workspace.home.join(".codex/hooks/rally2-hook.sh")).unwrap();
    assert!(script.contains("agent-rally2-install-v1"));
    assert!(script.contains("/tmp/rally2"));
    assert!(script.contains("next --tool"));
    assert!(script.contains("Rally 2 next"));
    let hooks = fs::read_to_string(workspace.home.join(".codex/hooks.json")).unwrap();
    assert!(hooks.contains("rally2-hook.sh"));
    assert!(!hooks.contains("\"Stop\""));
    assert!(!hooks.contains("before-complete codex"));
    assert!(hooks.contains("rally-hook.sh"));
    assert!(workspace.home.join(".codex/rally-hook.sh").exists());

    let reinstalled =
        workspace.json(&["install", "codex", "--json", "--rally2-bin", "/tmp/rally2"]);
    let config_actions = reinstalled["data"]["adapters"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|action| action["kind"] == "hook-config")
        .count();
    assert_eq!(config_actions, 1);

    let uninstalled = workspace.json(&["install", "codex", "--json", "--uninstall"]);
    assert_eq!(uninstalled["data"]["mode"], "uninstall");
    assert!(!workspace.home.join(".codex/hooks/rally2-hook.sh").exists());
    let hooks = fs::read_to_string(workspace.home.join(".codex/hooks.json")).unwrap();
    assert!(!hooks.contains("rally2-hook.sh"));
    assert!(hooks.contains("rally-hook.sh"));

    workspace.cleanup();
}

#[test]
fn rally2_installs_every_required_adapter_surface() {
    let workspace = Workspace::new("rally2-install-all");
    let installed = workspace.json(&[
        "install",
        "all",
        "--json",
        "--rally2-bin",
        "/opt/bin/rally2",
    ]);
    assert_matches_schema("agent-rally2.command.install.v1.json", &installed);
    assert_eq!(installed["data"]["adapters"].as_array().unwrap().len(), 6);
    assert!(workspace.home.join(".codex/hooks/rally2-hook.sh").exists());
    assert!(workspace.home.join(".claude/hooks/rally2-hook.sh").exists());
    assert!(
        workspace
            .home
            .join(".pi/agent/extensions/rally2-room.ts")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/herdr/integrations/rally2.json")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/cmux/rally2-integration.json")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/rally2/ci/github-actions-rally2.yml")
            .exists()
    );

    let pi_extension =
        fs::read_to_string(workspace.home.join(".pi/agent/extensions/rally2-room.ts")).unwrap();
    assert!(pi_extension.contains("rally2Room"));
    assert!(pi_extension.contains("/opt/bin/rally2"));
    assert!(pi_extension.contains("\"next\", \"--tool\", \"pi\""));

    let herdr_integration = fs::read_to_string(
        workspace
            .home
            .join(".config/herdr/integrations/rally2.json"),
    )
    .unwrap();
    assert!(herdr_integration.contains("\"next\""));
    assert!(herdr_integration.contains("waiting_on"));

    let cmux_integration =
        fs::read_to_string(workspace.home.join(".config/cmux/rally2-integration.json")).unwrap();
    assert!(cmux_integration.contains("\"next\""));

    let ci_workflow = fs::read_to_string(
        workspace
            .home
            .join(".config/rally2/ci/github-actions-rally2.yml"),
    )
    .unwrap();
    assert!(ci_workflow.contains("next --tool ci --json"));

    workspace.cleanup();
}

#[test]
fn rally2_room_is_queryable_by_tool_role_path_event_thread_and_since() {
    let workspace = Workspace::new("rally2-query");
    let claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--role",
        "builder",
        "--thread-id",
        "thread-query",
        "--path",
        "src/main.rs",
        "--subject",
        "codex owns main",
    ]);
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap();
    workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "pi",
        "--role",
        "reviewer",
        "--thread-id",
        "thread-other",
        "--path",
        "docs/spec.md",
        "--subject",
        "docs decision",
    ]);

    let by_tool = workspace.json(&["room", "--json", "--tool", "codex"]);
    assert_eq!(by_tool["data"]["query"]["tool"], "codex");
    assert_eq!(
        by_tool["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        by_tool["data"]["room"]["current_decisions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let by_role_path = workspace.json(&[
        "room",
        "--json",
        "--role",
        "builder",
        "--path",
        "src/main.rs",
    ]);
    assert_eq!(
        by_role_path["data"]["room"]["active_claims"][0]["event_id"],
        claim_id
    );

    let by_event = workspace.json(&["room", "--json", "--event", claim_id]);
    assert_eq!(
        by_event["data"]["room"]["active_claims"][0]["event_id"],
        claim_id
    );

    let by_thread = workspace.json(&["room", "--json", "--thread", "thread-query"]);
    assert_eq!(
        by_thread["data"]["room"]["active_claims"][0]["thread_id"],
        "thread-query"
    );

    let since = workspace.json(&["room", "--json", "--since", "1"]);
    assert_eq!(
        since["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        since["data"]["room"]["current_decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    workspace.cleanup();
}

#[test]
fn rally2_json_errors_use_agent_cli_exit_codes() {
    let workspace = Workspace::new("rally2-json-errors");
    let unknown = workspace.output(&["nope", "--json"]);
    assert_eq!(unknown.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["product"], "rally2");
    assert_eq!(body["exit_code"], 2);

    let invalid = workspace.output(&["room", "--json", "--since", "later"]);
    assert_eq!(invalid.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(body["exit_code"], 2);
    assert!(body["error"].as_str().unwrap().contains("invalid --since"));

    workspace.cleanup();
}

#[test]
fn rally2_flags_do_not_silently_consume_positionals() {
    let workspace = Workspace::new("rally2-argbag-flags");

    let fact = workspace.json(&[
        "say",
        "--future-flag",
        "claim",
        "--json",
        "--tool=codex",
        "--subject",
        "unknown flags stay flags",
    ]);
    assert_eq!(fact["data"]["fact"]["kind"], "claim");
    assert_eq!(fact["data"]["fact"]["tool"], "codex");

    let install = workspace.json(&["install", "--future-flag", "codex", "--json", "--dry-run"]);
    assert_eq!(install["data"]["target"], "codex");
    assert_eq!(install["data"]["mode"], "dry-run");

    workspace.cleanup();
}

#[test]
fn rally2_check_covers_artifacts_and_completion_boundaries() {
    let workspace = Workspace::new("rally2-check-phases");
    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--path",
        "src/lib.rs",
        "--subject",
        "finish lib",
    ]);

    let after_artifact = workspace.json(&[
        "check",
        "after-artifact",
        "--json",
        "--tool",
        "codex",
        "--strict",
    ]);
    assert_eq!(after_artifact["data"]["check"]["allow"], true);
    assert!(
        after_artifact["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "missing-evidence")
    );

    let (before_complete, before_complete_output) = workspace.json_with_status(&[
        "check",
        "before-complete",
        "--json",
        "--tool",
        "codex",
        "--strict",
    ]);
    assert_eq!(before_complete_output.status.code(), Some(4));
    assert_eq!(before_complete["data"]["check"]["allow"], false);
    assert!(
        before_complete["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "owned-active-claim")
    );

    workspace.cleanup();
}

#[test]
fn rally2_supports_all_required_fact_kinds() {
    let workspace = Workspace::new("rally2-facts");
    let kinds = [
        "claim", "release", "blocker", "resolve", "decision", "artifact", "handoff", "risk",
        "lesson",
    ];

    for kind in kinds {
        let fact = workspace.json(&[
            "say",
            kind,
            "--json",
            "--tool",
            "codex",
            "--subject",
            kind,
            "--path",
            "src/lib.rs",
            "--evidence",
            "observed",
        ]);
        assert_eq!(fact["data"]["fact"]["kind"], kind);
        assert_eq!(fact["data"]["fact"]["schema"], "agent-rally2.fact.v1");
        DateTime::parse_from_rfc3339(fact["data"]["fact"]["created_at"].as_str().unwrap()).unwrap();
        assert_matches_schema("agent-rally2.fact.v1.json", &fact["data"]["fact"]);
    }

    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["data"]["room"]["max_seq"], 9);
    workspace.cleanup();
}
