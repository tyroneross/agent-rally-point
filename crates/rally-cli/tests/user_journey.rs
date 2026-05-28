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
        Command::new(env!("CARGO_BIN_EXE_rally"))
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
fn rally_uses_factstr_sqlite_as_the_fact_store() {
    let workspace = Workspace::new("rally-factstr-store");

    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "complete fact",
    ]);
    assert!(workspace.cwd.join(".rally/facts.db").exists());
    assert!(!workspace.cwd.join(".rally/facts.jsonl").exists());

    fs::remove_file(workspace.cwd.join(".rally/room.db")).unwrap();

    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["ok"], true);
    assert_eq!(room["data"]["room"]["max_seq"], 1);

    workspace.cleanup();
}

#[test]
fn rally_agent_enters_room_checks_work_and_says_artifact() {
    let workspace = Workspace::new("rally-room");

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
    assert_eq!(claim["schema"], "agent-rally.command.say.v1");
    assert_matches_schema("agent-rally.command.say.v1.json", &claim);
    assert_matches_schema("agent-rally.fact.v1.json", &claim["data"]["fact"]);
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap();

    workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "Rally uses enter/say/room/check",
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
    assert_eq!(enter["schema"], "agent-rally.command.enter.v1");
    assert_matches_schema("agent-rally.command.enter.v1.json", &enter);
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
            .any(|item| item["subject"] == "Rally uses enter/say/room/check")
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
    assert_eq!(check["schema"], "agent-rally.command.check.v1");
    assert_matches_schema("agent-rally.command.check.v1.json", &check);
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
        "cargo test -p rally-cli rally_agent_enters_room_checks_work_and_says_artifact",
    ]);
    let room_without_export = workspace.json(&["room", "--json"]);
    assert!(!workspace.cwd.join("HANDOFF.md").exists());
    assert_eq!(room_without_export["data"]["exported_handoff"], Value::Null);

    let room = workspace.json(&["room", "--json", "--export-handoff"]);
    assert_eq!(room["schema"], "agent-rally.command.room.v1");
    assert_matches_schema("agent-rally.command.room.v1.json", &room);
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
    assert!(workspace.cwd.join(".rally/facts.db").exists());
    assert!(workspace.cwd.join(".rally/room.db").exists());
    let conn = Connection::open(workspace.cwd.join(".rally/room.db")).unwrap();
    let graph_edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    assert!(graph_edges >= 4);

    workspace.cleanup();
}

#[test]
fn rally_is_not_a_command_fallback() {
    let workspace = Workspace::new("rally-no-fallback");
    let help = workspace.output(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally enter --tool <tool>"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally next --tool <tool>"));

    let output = workspace.output(&["context", "--json", "--tool", "codex"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["product"], "rally");
    assert_eq!(error["exit_code"], 2);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("unknown Rally command context")
    );
    workspace.cleanup();
}

#[test]
fn rally_next_finds_useful_work_while_waiting() {
    let workspace = Workspace::new("rally-next-useful");
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
    assert_eq!(next["schema"], "agent-rally.command.next.v1");
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["mode"], "useful_while_waiting");
    assert_eq!(next["data"]["next"]["action"], "review_artifact");
    assert_eq!(next["data"]["next"]["actionable"], true);
    assert_eq!(next["data"]["next"]["requires_human"], false);
    assert_eq!(next["data"]["next"]["stop_reason"], Value::Null);
    assert_eq!(next["data"]["next"]["target_event_id"], artifact_id);
    assert!(
        next["data"]["next"]["suggested_claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["scope"] == "file:docs/notes.md")
    );
    assert!(
        next["data"]["next"]["suggested_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item
                .as_str()
                .unwrap()
                .contains("rally say artifact --tool codex --ref"))
    );
    assert_eq!(
        next["data"]["next"]["completion"]["record_kind"],
        "artifact"
    );
    assert_eq!(next["data"]["next"]["completion"]["rerun_next"], true);
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
fn rally_next_waits_only_when_no_useful_work_exists() {
    let workspace = Workspace::new("rally-next-wait");
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
        "cargo test -p rally-cli",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex"]);
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["mode"], "waiting");
    assert_eq!(next["data"]["next"]["action"], "wait");
    assert_eq!(next["data"]["next"]["actionable"], false);
    assert_eq!(
        next["data"]["next"]["stop_reason"],
        "waiting_on_peer_with_no_useful_alternate_work"
    );
    assert!(
        next["data"]["next"]["suggested_claims"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        next["data"]["next"]["suggested_commands"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(next["data"]["next"]["completion"]["record_kind"], "none");
    assert_eq!(next["data"]["next"]["completion"]["rerun_next"], false);
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
fn rally_exposes_required_first_class_adapters() {
    let workspace = Workspace::new("rally-adapters");
    let expected = ["codex", "claude_code", "pi", "herdr", "cmux", "ci"];

    for tool in expected {
        let entered = workspace.json(&["enter", "--json", "--tool", tool]);
        assert_eq!(entered["data"]["adapter"]["adapter"], tool);
        assert_eq!(entered["data"]["adapter"]["first_class"], true);
        assert_eq!(
            entered["data"]["adapter"]["commands"]["enter"],
            format!("rally enter --tool {tool} --json")
        );
        assert!(entered["data"]["adapter"]["commands"]["next"].is_null());
        if matches!(tool, "codex" | "claude_code" | "pi") {
            assert_eq!(
                entered["data"]["adapter"]["surfaces"]["startup_enter"],
                false
            );
            assert_eq!(
                entered["data"]["adapter"]["surfaces"]["completion_prompt"],
                false
            );
            assert!(
                entered["data"]["adapter"]["model_visible"]
                    .as_str()
                    .unwrap()
                    .contains("Write-boundary guard only")
            );
        }
        assert_eq!(entered["data"]["adapter"]["surfaces"]["loop_enter"], false);
        assert!(entered["data"]["adapter"]["surfaces"]["idle_next"].is_null());
        assert!(
            !entered["data"]["adapter"]["model_visible"]
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
            format!("rally check before-write --tool {tool} --path <path> --json")
        );
        assert!(adapter["commands"]["next_for_path"].is_null());
    }

    workspace.cleanup();
}

#[test]
fn rally_entry_and_handoff_split_response_and_work_buckets() {
    let workspace = Workspace::new("rally-entry-buckets");
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
fn rally_installs_guard_adapter_glue() {
    let workspace = Workspace::new("rally-install");

    let dry_run = workspace.json(&[
        "install",
        "codex",
        "--json",
        "--dry-run",
        "--rally-bin",
        "/tmp/rally",
    ]);
    assert_eq!(dry_run["schema"], "agent-rally.command.install.v1");
    assert_matches_schema("agent-rally.command.install.v1.json", &dry_run);
    assert_eq!(dry_run["data"]["mode"], "dry-run");
    assert!(!workspace.home.join(".codex/hooks/rally-hook.sh").exists());

    let installed = workspace.json(&["install", "codex", "--json", "--rally-bin", "/tmp/rally"]);
    assert_eq!(installed["data"]["adapters"][0]["adapter"], "codex");
    let script = fs::read_to_string(workspace.home.join(".codex/hooks/rally-hook.sh")).unwrap();
    assert!(script.contains("agent-rally-install-v1"));
    assert!(script.contains("/tmp/rally"));
    assert!(!script.contains("next --tool"));
    assert!(!script.contains("Rally next"));
    assert!(!script.contains("Rally room"));
    assert!(!script.contains("user-prompt"));
    assert!(!script.contains("before-complete"));
    let hooks = fs::read_to_string(workspace.home.join(".codex/hooks.json")).unwrap();
    assert!(hooks.contains("rally-hook.sh"));
    assert!(!hooks.contains("session-start codex"));
    assert!(!hooks.contains("\"UserPromptSubmit\""));
    assert!(!hooks.contains("\"Stop\""));
    assert!(!hooks.contains("before-complete codex"));

    let reinstalled = workspace.json(&["install", "codex", "--json", "--rally-bin", "/tmp/rally"]);
    let config_actions = reinstalled["data"]["adapters"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|action| action["kind"] == "hook-config")
        .count();
    assert_eq!(config_actions, 1);

    let uninstalled = workspace.json(&["install", "codex", "--json", "--uninstall"]);
    assert_eq!(uninstalled["data"]["mode"], "uninstall");
    assert!(!workspace.home.join(".codex/hooks/rally-hook.sh").exists());
    let hooks = fs::read_to_string(workspace.home.join(".codex/hooks.json")).unwrap();
    assert!(!hooks.contains("rally-hook.sh"));

    workspace.cleanup();
}

#[test]
fn rally_installs_every_required_adapter_surface() {
    let workspace = Workspace::new("rally-install-all");
    let installed = workspace.json(&["install", "all", "--json", "--rally-bin", "/opt/bin/rally"]);
    assert_matches_schema("agent-rally.command.install.v1.json", &installed);
    assert_eq!(installed["data"]["adapters"].as_array().unwrap().len(), 6);
    assert!(workspace.home.join(".codex/hooks/rally-hook.sh").exists());
    assert!(workspace.home.join(".claude/hooks/rally-hook.sh").exists());
    assert!(
        workspace
            .home
            .join(".pi/agent/extensions/rally-guard.ts")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/herdr/integrations/rally.json")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/cmux/rally-integration.json")
            .exists()
    );
    assert!(
        workspace
            .home
            .join(".config/rally/ci/github-actions-rally.yml")
            .exists()
    );

    let pi_extension =
        fs::read_to_string(workspace.home.join(".pi/agent/extensions/rally-guard.ts")).unwrap();
    assert!(pi_extension.contains("rallyGuard"));
    assert!(pi_extension.contains("/opt/bin/rally"));
    assert!(!pi_extension.contains("\"next\", \"--tool\", \"pi\""));
    assert!(!pi_extension.contains("sendMessage"));
    assert!(!pi_extension.contains("session_start"));
    assert!(!pi_extension.contains("before_agent_start"));

    let herdr_integration =
        fs::read_to_string(workspace.home.join(".config/herdr/integrations/rally.json")).unwrap();
    assert!(!herdr_integration.contains("\"next\""));
    assert!(herdr_integration.contains("rally run --backend herdr"));

    let cmux_integration =
        fs::read_to_string(workspace.home.join(".config/cmux/rally-integration.json")).unwrap();
    assert!(!cmux_integration.contains("\"next\""));

    let ci_workflow = fs::read_to_string(
        workspace
            .home
            .join(".config/rally/ci/github-actions-rally.yml"),
    )
    .unwrap();
    assert!(!ci_workflow.contains("next --tool ci --json"));

    workspace.cleanup();
}

#[test]
fn rally_runs_and_injects_managed_tmux_sessions() {
    let workspace = Workspace::new("rally-run-tmux");

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "reviewer",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(run["schema"], "agent-rally.command.run.v1");
    assert_matches_schema("agent-rally.command.run.v1.json", &run);
    assert_eq!(run["data"]["session"]["name"], "reviewer");
    assert_eq!(run["data"]["session"]["agent"], "claude");
    assert_eq!(run["data"]["session"]["tool"], "claude_code:reviewer");
    assert_eq!(run["data"]["session"]["backend"], "tmux");
    assert_eq!(run["data"]["session"]["target"], "rally-claude-reviewer");
    assert!(
        run["data"]["commands"]["start"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|command| command.as_array().into_iter().flatten())
            .any(|arg| arg.as_str().unwrap().contains("claude"))
    );

    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["schema"], "agent-rally.command.sessions.v1");
    assert_matches_schema("agent-rally.command.sessions.v1.json", &sessions);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(sessions["data"]["sessions"][0]["name"], "reviewer");

    let inject = workspace.json(&[
        "inject",
        "reviewer",
        "--json",
        "--text",
        "hello from rally",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(inject["schema"], "agent-rally.command.inject.v1");
    assert_matches_schema("agent-rally.command.inject.v1.json", &inject);
    assert_eq!(inject["data"]["session"]["name"], "reviewer");
    assert_eq!(inject["data"]["commands"].as_array().unwrap().len(), 4);

    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code:reviewer",
        "--subject",
        "managed session handoff",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();
    workspace.json(&[
        "say",
        "resolve",
        "--json",
        "--tool",
        "claude_code:reviewer",
        "--ref",
        handoff_id,
        "--subject",
        "managed session handoff resolved",
    ]);
    let acked = workspace.json(&[
        "inject",
        "reviewer",
        "--json",
        "--handoff",
        handoff_id,
        "--require-ack",
        "--timeout-seconds",
        "1",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(acked["data"]["ack"]["resolved"], true);
    assert_eq!(acked["data"]["ack"]["tool"], "claude_code:reviewer");

    let capture = workspace.json(&[
        "capture",
        "reviewer",
        "--json",
        "--lines",
        "20",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(capture["schema"], "agent-rally.command.session-action.v1");
    assert_matches_schema("agent-rally.command.session-action.v1.json", &capture);
    assert_eq!(capture["data"]["action"], "capture");
    assert_eq!(capture["data"]["output"], "");

    let attach = workspace.json(&[
        "attach",
        "reviewer",
        "--json",
        "--dry-run",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(attach["schema"], "agent-rally.command.session-action.v1");
    assert_matches_schema("agent-rally.command.session-action.v1.json", &attach);
    assert_eq!(attach["data"]["commands"][0][1], "attach");

    let dry_run = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--name",
        "builder",
        "--backend",
        "tmux",
        "--dry-run",
    ]);
    assert_eq!(dry_run["data"]["mode"], "dry-run");
    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 1);

    let stop = workspace.json(&["stop", "reviewer", "--json", "--tmux-bin", "/usr/bin/true"]);
    assert_eq!(stop["schema"], "agent-rally.command.session-action.v1");
    assert_matches_schema("agent-rally.command.session-action.v1.json", &stop);
    assert_eq!(stop["data"]["action"], "stop");
    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 0);

    workspace.cleanup();
}

#[test]
fn rally_uses_native_herdr_and_cmux_managed_session_commands() {
    let workspace = Workspace::new("rally-native-session-backends");

    let herdr = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "herdr-reviewer",
        "--backend",
        "herdr",
        "--herdr-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(herdr["schema"], "agent-rally.command.run.v1");
    assert_matches_schema("agent-rally.command.run.v1.json", &herdr);
    assert_eq!(herdr["data"]["session"]["backend"], "herdr");
    assert_eq!(herdr["data"]["session"]["target"], "claude-herdr-reviewer");
    assert_eq!(herdr["data"]["commands"]["start"][0][1], "agent");
    assert_eq!(herdr["data"]["commands"]["start"][0][2], "start");

    let herdr_inject = workspace.json(&[
        "inject",
        "herdr-reviewer",
        "--json",
        "--text",
        "hello herdr",
        "--herdr-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(herdr_inject["data"]["commands"][0][1], "pane");
    assert_eq!(herdr_inject["data"]["commands"][0][2], "send-text");
    assert_eq!(herdr_inject["data"]["commands"][0][4], "\u{15}");
    assert_eq!(herdr_inject["data"]["commands"][1][1], "pane");
    assert_eq!(herdr_inject["data"]["commands"][1][2], "send-text");
    assert_eq!(herdr_inject["data"]["commands"][1][4], "hello herdr");
    assert_eq!(herdr_inject["data"]["commands"][2][1], "pane");
    assert_eq!(herdr_inject["data"]["commands"][2][2], "send-keys");
    assert_eq!(herdr_inject["data"]["commands"][2][4], "enter");

    let herdr_capture = workspace.json(&[
        "capture",
        "herdr-reviewer",
        "--json",
        "--dry-run",
        "--lines",
        "30",
    ]);
    assert_matches_schema("agent-rally.command.session-action.v1.json", &herdr_capture);
    assert_eq!(herdr_capture["data"]["commands"][0][1], "agent");
    assert_eq!(herdr_capture["data"]["commands"][0][2], "read");

    let cmux = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--name",
        "cmux-builder",
        "--backend",
        "cmux",
        "--cmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(cmux["schema"], "agent-rally.command.run.v1");
    assert_matches_schema("agent-rally.command.run.v1.json", &cmux);
    assert_eq!(cmux["data"]["session"]["backend"], "cmux");
    assert_eq!(cmux["data"]["session"]["target"], "codex-cmux-builder");
    assert_eq!(cmux["data"]["commands"]["start"][0][1], "new-workspace");
    assert!(
        !cmux["data"]["commands"]["start"][0]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--command")
    );
    assert!(
        cmux["data"]["commands"]["start"][0]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--layout")
    );
    let cmux_layout = cmux["data"]["commands"]["start"][0]
        .as_array()
        .unwrap()
        .windows(2)
        .find(|pair| pair[0] == "--layout")
        .and_then(|pair| pair[1].as_str())
        .unwrap();
    let cmux_layout: serde_json::Value = serde_json::from_str(cmux_layout).unwrap();
    assert_eq!(cmux_layout["pane"]["surfaces"][0]["type"], "terminal");
    assert_eq!(cmux_layout["pane"]["surfaces"][0]["command"], "codex");

    let cmux_inject = workspace.json(&[
        "inject",
        "cmux-builder",
        "--json",
        "--text",
        "hello cmux",
        "--cmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(cmux_inject["data"]["commands"][0][1], "send-key");
    assert_eq!(cmux_inject["data"]["commands"][0][4], "ctrl+u");
    assert_eq!(cmux_inject["data"]["commands"][1][1], "send");
    assert_eq!(cmux_inject["data"]["commands"][1][4], "hello cmux");
    assert_eq!(cmux_inject["data"]["commands"][2][1], "send-key");
    assert_eq!(cmux_inject["data"]["commands"][2][4], "enter");

    let cmux_stop = workspace.json(&[
        "stop",
        "cmux-builder",
        "--json",
        "--cmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(cmux_stop["data"]["commands"][0][1], "close-workspace");

    let herdr_stop = workspace.json(&[
        "stop",
        "herdr-reviewer",
        "--json",
        "--herdr-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(herdr_stop["data"]["commands"][0][1], "pane");
    assert_eq!(herdr_stop["data"]["commands"][0][2], "close");

    workspace.cleanup();
}

#[test]
fn rally_room_is_queryable_by_tool_role_path_event_thread_and_since() {
    let workspace = Workspace::new("rally-query");
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
fn rally_json_errors_use_agent_cli_exit_codes() {
    let workspace = Workspace::new("rally-json-errors");
    let unknown = workspace.output(&["nope", "--json"]);
    assert_eq!(unknown.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["product"], "rally");
    assert_eq!(body["exit_code"], 2);

    let invalid = workspace.output(&["room", "--json", "--since", "later"]);
    assert_eq!(invalid.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(body["exit_code"], 2);
    assert!(body["error"].as_str().unwrap().contains("invalid --since"));

    workspace.cleanup();
}

#[test]
fn rally_say_persists_summary_round_trip() {
    let workspace = Workspace::new("rally-summary-roundtrip");

    // space-separated form
    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code",
        "--subject",
        "needs review",
        "--summary",
        "Reviewer has enough context to proceed.",
    ]);
    assert_eq!(
        handoff["data"]["fact"]["summary"],
        "Reviewer has enough context to proceed.",
        "space-separated --summary must round-trip into fact.summary"
    );
    assert_eq!(handoff["data"]["fact"]["subject"], "needs review");

    // equals form
    let decision = workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "binding call",
        "--summary=Adopt the finite pickup protocol.",
    ]);
    assert_eq!(
        decision["data"]["fact"]["summary"],
        "Adopt the finite pickup protocol.",
        "--summary=value form must round-trip into fact.summary"
    );

    workspace.cleanup();
}

#[test]
fn rally_flags_do_not_silently_consume_positionals() {
    let workspace = Workspace::new("rally-argbag-flags");

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
fn rally_check_covers_artifacts_and_completion_boundaries() {
    let workspace = Workspace::new("rally-check-phases");
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
fn rally_supports_all_required_fact_kinds() {
    let workspace = Workspace::new("rally-facts");
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
        assert_eq!(fact["data"]["fact"]["schema"], "agent-rally.fact.v1");
        DateTime::parse_from_rfc3339(fact["data"]["fact"]["created_at"].as_str().unwrap()).unwrap();
        assert_matches_schema("agent-rally.fact.v1.json", &fact["data"]["fact"]);
    }

    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["data"]["room"]["max_seq"], 9);
    workspace.cleanup();
}
