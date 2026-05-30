// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::DateTime;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
    /// When true, passes RALLY_GLOBAL_INDEX=1 to every command.
    global_index: bool,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home, global_index: false }
    }

    fn new_with_home(name: &str, home: &Path) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            cwd,
            home: home.to_path_buf(),
            global_index: false,
        }
    }

    /// Enable RALLY_GLOBAL_INDEX=1 for all commands run through this workspace.
    fn with_global_index(mut self) -> Self {
        self.global_index = true;
        self
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        if self.global_index {
            cmd.env("RALLY_GLOBAL_INDEX", "1");
        }
        cmd.args(args).output().unwrap()
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
    fs::create_dir_all(workspace.cwd.join(".rally")).unwrap();
    fs::write(workspace.cwd.join(".rally/room.db"), "legacy projection").unwrap();

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
    assert!(!workspace.cwd.join(".rally/room.db").exists());

    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["ok"], true);
    // Component B: say auto-registers presence (1) + lead (2) + artifact (3).
    assert_eq!(room["data"]["room"]["max_seq"], 3);

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
    assert_eq!(enter["data"]["cursor"]["before"], 0);
    // Component B: say claim (claude) wrote presence(1)+lead(2)+claim(3);
    // say decision (pi) wrote presence(4)+decision(5). codex enter writes
    // presence(6) via ensure_presence (lead already set by claude).
    // cursor_after is set to post-presence max_seq=6 so that codex's own
    // presence fact is excluded from "new peer content" on the next enter.
    assert_eq!(enter["data"]["cursor"]["after"], 6);
    assert_eq!(enter["data"]["cursor"]["advanced"], true);
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
    let enter_again = workspace.json(&[
        "enter",
        "--json",
        "--tool",
        "codex",
        "--session-id",
        "codex-main",
        "--path",
        "src/room.rs",
    ]);
    // R10/cursor: second enter's cursor_before = 6 (ledger-derived from the
    // Read checkpoint written by the first enter, which recorded content_max_seq=6).
    // The second enter detects codex as already active (B11) and writes a durable
    // risk fact before ensure_presence runs; then ensure_presence is idempotent
    // (no new presence/lead).
    // Seq breakdown after first enter:
    //   1: presence(claude), 2: lead(claude), 3: claim, 4: presence(pi), 5: decision
    //   6: presence(codex) — first enter's ensure_presence
    //   7: Read checkpoint (first enter's maybe_append_read_checkpoint at content_max=6)
    //   8: B11 risk fact (second enter's drift detection)
    // cursor_after = snapshot.max_seq = 8.
    assert_eq!(enter_again["data"]["cursor"]["before"], 6);
    assert_eq!(enter_again["data"]["cursor"]["after"], 8);
    assert_eq!(enter_again["data"]["cursor"]["advanced"], true);

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
    let room = workspace.json(&["room", "--json"]);
    assert!(!workspace.cwd.join("HANDOFF.md").exists());
    assert_eq!(room["schema"], "agent-rally.command.room.v1");
    assert_matches_schema("agent-rally.command.room.v1.json", &room);
    assert_eq!(
        room["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // At least 2 decisions: the original "Rally uses enter/say/room/check"
    // plus the "role:lead" decision written by `enter` (Component A).
    assert!(
        room["data"]["room"]["current_decisions"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
    assert_eq!(
        room["data"]["room"]["recent_artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(workspace.cwd.join(".rally/facts.db").exists());
    assert!(workspace.cwd.join(".rally/cursors.json").exists());

    workspace.cleanup();
}

#[test]
fn rally_is_not_a_command_fallback() {
    let workspace = Workspace::new("rally-no-fallback");
    let help = workspace.output(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally enter --tool <tool>"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("rally next --tool <tool>"));

    for command in ["run", "inject", "attach", "capture", "stop"] {
        let output = workspace.output(&[command, "--help"]);
        assert!(
            output.status.success(),
            "{command} --help failed\nstderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

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

    let missing_value = workspace.output(&["next", "--tool", "--json"]);
    assert!(!missing_value.status.success());
    let error: Value = serde_json::from_slice(&missing_value.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("`--tool` requires an argument")
    );
    workspace.cleanup();
}

#[test]
fn rally_before_write_matches_directory_scopes() {
    let workspace = Workspace::new("rally-path-scope");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude",
        "--path",
        "src",
        "--subject",
        "own source tree",
    ]);

    let (check, output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        "./src/lib.rs",
        "--strict",
    ]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(check["data"]["check"]["allow"], false);
    assert!(
        check["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "claimed-path")
    );

    let (parent_dir_check, parent_dir_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        "other/../src/lib.rs",
        "--strict",
    ]);
    assert_eq!(parent_dir_output.status.code(), Some(4));
    assert_eq!(parent_dir_check["data"]["check"]["allow"], false);

    let absolute = workspace.cwd.join("src/lib.rs");
    let (absolute_check, absolute_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        absolute.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(absolute_output.status.code(), Some(4));
    assert_eq!(absolute_check["data"]["check"]["allow"], false);

    let room = workspace.json(&["room", "--json", "--path", "src/lib.rs"]);
    assert_eq!(
        room["data"]["room"]["active_claims"]
            .as_array()
            .unwrap()
            .len(),
        1
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
        "review notes ready",
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
                .contains("rally say resolve --tool codex --ref"))
    );
    assert_eq!(next["data"]["next"]["completion"]["record_kind"], "resolve");
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

    workspace.json(&[
        "say",
        "resolve",
        "--json",
        "--tool",
        "codex",
        "--ref",
        artifact_id,
        "--subject",
        "reviewed artifact",
        "--evidence",
        "notes checked",
    ]);
    let room = workspace.json(&["room", "--json"]);
    assert!(
        !room["data"]["room"]["unconsumed_artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["event_id"] == artifact_id),
        "artifact review resolve should consume the reviewed artifact"
    );

    workspace.cleanup();
}

#[test]
fn rally_artifact_ref_consumes_handoff_but_not_blocker_or_claim() {
    // intent: an artifact fact that references a handoff via --ref should
    // close that handoff (drop it from room.open_handoffs and next).
    // Blockers must still require resolve, and claims must still require
    // release — artifact --ref is not a universal closer.
    let workspace = Workspace::new("rally-artifact-closes-handoff");

    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code",
        "--subject",
        "needs work",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();

    let blocker = workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "broken pipeline",
    ]);
    let blocker_id = blocker["data"]["fact"]["event_id"].as_str().unwrap();

    let claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "owning the rewrite",
        "--path",
        "src/lib.rs",
    ]);
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap();

    // Pre-condition: handoff is open before any artifact references it.
    let room_before = workspace.json(&["room", "--json"]);
    assert!(
        room_before["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == handoff_id),
        "handoff should be open before artifact references it"
    );

    // Artifact references each fact via --ref. Only the handoff should close.
    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "delivered work",
        "--ref",
        handoff_id,
        "--uri",
        "docs/done.md",
        "--evidence",
        "cargo test",
    ]);
    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "blocker artifact attempt",
        "--ref",
        blocker_id,
        "--evidence",
        "cargo test",
    ]);
    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "claim artifact attempt",
        "--ref",
        claim_id,
        "--evidence",
        "cargo test",
    ]);

    let room_after = workspace.json(&["room", "--json"]);
    let open_handoffs = room_after["data"]["room"]["open_handoffs"]
        .as_array()
        .unwrap();
    assert!(
        !open_handoffs.iter().any(|f| f["event_id"] == handoff_id),
        "artifact --ref <handoff> should close the handoff"
    );

    // Blockers and claims must NOT be closed by an artifact --ref.
    let active_blockers = room_after["data"]["room"]["active_blockers"]
        .as_array()
        .unwrap();
    assert!(
        active_blockers.iter().any(|f| f["event_id"] == blocker_id),
        "artifact --ref must not close a blocker (only resolve does)"
    );
    let active_claims = room_after["data"]["room"]["active_claims"]
        .as_array()
        .unwrap();
    assert!(
        active_claims.iter().any(|f| f["event_id"] == claim_id),
        "artifact --ref must not close a claim (only release does)"
    );

    // The consumed handoff should also disappear from `next`'s waiting_on.
    let next = workspace.json(&["next", "--json", "--tool", "claude_code"]);
    assert!(
        !next["data"]["next"]["waiting_on"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["event_id"] == handoff_id),
        "closed handoff should not appear in next.waiting_on"
    );

    workspace.cleanup();
}

#[test]
fn rally_resolve_closes_risks_in_room_projection() {
    let workspace = Workspace::new("rally-resolve-risks");

    let risk = workspace.json(&[
        "say",
        "risk",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "help path is broken",
        "--severity",
        "high",
    ]);
    let risk_id = risk["data"]["fact"]["event_id"].as_str().unwrap();

    let room_before = workspace.json(&["room", "--json"]);
    assert!(
        room_before["data"]["room"]["current_risks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == risk_id),
        "unresolved risk should be current"
    );

    workspace.json(&[
        "say",
        "resolve",
        "--json",
        "--tool",
        "codex",
        "--ref",
        risk_id,
        "--subject",
        "risk fixed",
    ]);

    let room_after = workspace.json(&["room", "--json"]);
    assert!(
        !room_after["data"]["room"]["current_risks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == risk_id),
        "resolved risk should not remain current"
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

    let room = workspace.json(&["room", "--json", "--tool", "codex"]);
    let active_claims = room["data"]["room"]["active_claims"].as_array().unwrap();
    assert!(
        active_claims
            .iter()
            .any(|claim| claim["subject"] == "codex active work")
    );
    assert!(
        !active_claims
            .iter()
            .any(|claim| claim["subject"] == "claude owns other work")
    );

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
    assert_eq!(run["data"]["session"]["name"], "reviewer-01");
    assert_eq!(run["data"]["session"]["session_id"], "claude-reviewer-01");
    assert_eq!(run["data"]["session"]["agent"], "claude");
    assert_eq!(run["data"]["session"]["tool"], "claude_code:reviewer-01");
    assert_eq!(run["data"]["session"]["backend"], "tmux");
    assert_eq!(run["data"]["session"]["target"], "rally-claude-reviewer-01");
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
    assert_eq!(sessions["data"]["sessions"][0]["name"], "reviewer-01");
    assert!(!workspace.cwd.join(".rally/sessions.json").exists());
    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["data"]["room"]["max_seq"], 1);

    let inject = workspace.json(&[
        "inject",
        "reviewer-01",
        "--json",
        "--text",
        "hello from rally",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(inject["schema"], "agent-rally.command.inject.v1");
    assert_matches_schema("agent-rally.command.inject.v1.json", &inject);
    assert_eq!(inject["data"]["session"]["name"], "reviewer-01");
    assert_eq!(inject["data"]["commands"].as_array().unwrap().len(), 4);

    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code:reviewer-01",
        "--subject",
        "managed session handoff",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();
    // R9-readback: do NOT pre-resolve the handoff here — the thread below is the
    // sole resolver.  A double-resolve of the same ref is now correctly blocked by
    // the R9 state-transition check (the handoff would no longer be in open_handoffs).
    let resolver_cwd = workspace.cwd.clone();
    let resolver_home = workspace.home.clone();
    let resolver_handoff = handoff_id.to_string();
    let resolver = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let output = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(resolver_cwd)
            .env("HOME", resolver_home)
            .args([
                "say",
                "resolve",
                "--json",
                "--tool",
                "claude_code:reviewer-01",
                "--ref",
                resolver_handoff.as_str(),
                "--subject",
                "managed session handoff resolved after inject",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    });
    let acked = workspace.json(&[
        "inject",
        "reviewer-01",
        "--json",
        "--handoff",
        handoff_id,
        "--require-ack",
        "--timeout-seconds",
        "3",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    resolver.join().unwrap();
    assert_eq!(acked["data"]["ack"]["resolved"], true);
    assert_eq!(acked["data"]["ack"]["tool"], "claude_code:reviewer-01");
    assert_eq!(
        acked["data"]["ack"]["subject"],
        "managed session handoff resolved after inject"
    );

    let capture = workspace.json(&[
        "capture",
        "reviewer-01",
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
    let capture_text = workspace.output(&[
        "capture",
        "reviewer-01",
        "--lines",
        "20",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert!(capture_text.status.success());
    assert_eq!(
        String::from_utf8_lossy(&capture_text.stdout).trim(),
        "capture session=claude-reviewer-01"
    );

    let attach = workspace.json(&[
        "attach",
        "reviewer-01",
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

    let stop = workspace.json(&[
        "stop",
        "reviewer-01",
        "--json",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(stop["schema"], "agent-rally.command.session-action.v1");
    assert_matches_schema("agent-rally.command.session-action.v1.json", &stop);
    assert_eq!(stop["data"]["action"], "stop");
    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 0);
    assert!(!workspace.cwd.join(".rally/sessions.json").exists());

    workspace.cleanup();
}

#[test]
fn rally_run_assigns_numbered_agent_ids() {
    let workspace = Workspace::new("rally-run-numbered-ids");

    let first_claude = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(first_claude["data"]["session"]["name"], "claude-01");
    assert_eq!(first_claude["data"]["session"]["session_id"], "claude-01");
    assert_eq!(first_claude["data"]["session"]["tool"], "claude_code:01");
    assert_eq!(first_claude["data"]["session"]["target"], "rally-claude-01");

    let second_claude = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(second_claude["data"]["session"]["name"], "claude-02");
    assert_eq!(second_claude["data"]["session"]["session_id"], "claude-02");
    assert_eq!(second_claude["data"]["session"]["tool"], "claude_code:02");

    let reviewer = workspace.json(&[
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
    assert_eq!(reviewer["data"]["session"]["name"], "reviewer-01");
    assert_eq!(
        reviewer["data"]["session"]["session_id"],
        "claude-reviewer-01"
    );
    assert_eq!(
        reviewer["data"]["session"]["tool"],
        "claude_code:reviewer-01"
    );

    let second_reviewer = workspace.json(&[
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
    assert_eq!(second_reviewer["data"]["session"]["name"], "reviewer-02");
    assert_eq!(
        second_reviewer["data"]["session"]["tool"],
        "claude_code:reviewer-02"
    );

    let first_codex = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(first_codex["data"]["session"]["name"], "codex-01");
    assert_eq!(first_codex["data"]["session"]["session_id"], "codex-01");
    assert_eq!(first_codex["data"]["session"]["tool"], "codex:01");

    // A second session with the same --tool but a different name is now
    // allowed (a lead may hold multiple managed sessions).  The rejection
    // guard fires only on a true duplicate: same session-id.
    // Force the collision by pinning --session-id to the already-live id.
    let duplicate_session_id = workspace.output(&[
        "run",
        "codex",
        "--json",
        "--backend",
        "tmux",
        "--session-id",
        "codex-01",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert!(!duplicate_session_id.status.success());
    let stderr = String::from_utf8_lossy(&duplicate_session_id.stderr);
    assert!(
        stderr.contains("already uses") && stderr.contains("codex-01"),
        "error must name the conflicting session-id; got: {stderr}"
    );

    workspace.cleanup();
}

#[test]
fn rally_run_reserves_numbered_ids_under_parallel_launch() {
    let workspace = Workspace::new("rally-run-parallel-numbered-ids");
    let handles = (0..24)
        .map(|_| {
            let cwd = workspace.cwd.clone();
            let home = workspace.home.clone();
            thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_rally"))
                    .current_dir(cwd)
                    .env("HOME", home)
                    .args([
                        "run",
                        "claude",
                        "--json",
                        "--backend",
                        "tmux",
                        "--tmux-bin",
                        "/usr/bin/true",
                    ])
                    .output()
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let output = handle.join().unwrap();
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let sessions = workspace.json(&["sessions", "--json"]);
    let sessions = sessions["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 24);
    let names = sessions
        .iter()
        .map(|session| session["name"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let tools = sessions
        .iter()
        .map(|session| session["tool"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), 24);
    assert_eq!(tools.len(), 24);
    assert!(names.contains("claude-01"));
    assert!(names.contains("claude-24"));
    assert!(tools.contains("claude_code:01"));
    assert!(tools.contains("claude_code:24"));

    workspace.cleanup();
}

#[test]
fn rally_run_removes_session_reservation_when_backend_start_fails() {
    let workspace = Workspace::new("rally-run-failed-start");

    let output = workspace.output(&[
        "run",
        "claude",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/false",
    ]);
    assert!(!output.status.success());

    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["data"]["sessions"].as_array().unwrap().len(), 0);

    workspace.cleanup();
}

#[test]
fn rally_next_and_inject_emit_wake_intent_facts() {
    let workspace = Workspace::new("rally-wake-intent");

    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "claude_code",
        "--target",
        "codex",
        "--subject",
        "wake codex for review",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex"]);
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["action"], "respond_to_handoff");
    assert_eq!(next["data"]["wake_intent"]["kind"], "wake");
    assert_eq!(next["data"]["wake_intent"]["target"], "codex");
    assert_eq!(next["data"]["wake_intent"]["ref"], handoff_id);
    assert_eq!(next["data"]["wake_intent"]["status"], "pending");
    let next_wake_id = next["data"]["wake_intent"]["event_id"].as_str().unwrap();
    let located_next_wake = workspace.json(&["locate", next_wake_id, "--json"]);
    assert_eq!(located_next_wake["data"]["located"]["source"], "room");

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
    assert_eq!(run["data"]["session"]["tool"], "claude_code:reviewer-01");
    let inject = workspace.json(&[
        "inject",
        "reviewer-01",
        "--json",
        "--handoff",
        handoff_id,
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_matches_schema("agent-rally.command.inject.v1.json", &inject);
    assert_eq!(inject["data"]["wake_intent"]["kind"], "wake");
    assert_eq!(
        inject["data"]["wake_intent"]["target"],
        "claude_code:reviewer-01"
    );
    assert_eq!(inject["data"]["wake_intent"]["ref"], handoff_id);
    assert_eq!(inject["data"]["wake_intent"]["status"], "delivered");

    workspace.cleanup();
}

/// B17: verify that `locate` and `recent` use only the rooms registry (per-repo
/// `.rally/log`) — legacy-only facts in `apps/<slug>/changes.jsonl` are NOT
/// returned because the `--include-legacy` flag has been retired. Cross-repo
/// rooms-based discovery still works normally via the global index.
#[test]
fn rally_locate_and_recent_discover_rooms_without_legacy() {
    let home = temp_path("rally-discovery-home");
    // B17: global index is opt-in; these workspaces use cross-repo locate/recent.
    let repo_a = Workspace::new_with_home("rally-discovery-a", &home).with_global_index();
    let repo_b = Workspace::new_with_home("rally-discovery-b", &home).with_global_index();

    let decision = repo_a.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "repo a decision",
    ]);
    let decision_id = decision["data"]["fact"]["event_id"].as_str().unwrap();
    let artifact = repo_b.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--subject",
        "repo b artifact",
        "--uri",
        "file:artifact.md",
    ]);
    let artifact_id = artifact["data"]["fact"]["event_id"].as_str().unwrap();

    // Cross-repo locate: repo_b can locate a fact written by repo_a via the
    // global rooms index.
    let located = repo_b.json(&["locate", decision_id, "--json"]);
    assert_matches_schema("agent-rally.command.locate.v1.json", &located);
    assert_eq!(located["data"]["located"]["source"], "room");
    assert_eq!(
        located["data"]["located"]["fact"]["event_id"].as_str(),
        Some(decision_id)
    );
    assert!(
        located["data"]["located"]["repo_root"]
            .as_str()
            .unwrap()
            .contains("rally-discovery-a-cwd")
    );

    // Seed a legacy-only fact under apps/ — it must NOT surface in locate or recent.
    let legacy_dir = home.join(".agent-rally-point/apps/repo_legacy");
    fs::create_dir_all(&legacy_dir).unwrap();
    fs::write(
        legacy_dir.join("changes.jsonl"),
        r#"{"local_seq":7,"event":{"id":"evt_legacy_wake","kind":"handoff","subject":"legacy wake","time":"2026-05-28T00:00:00Z"}}"#,
    )
    .unwrap();

    // locate does NOT find the legacy-only event (no --include-legacy flag exists).
    let not_found = repo_b.json(&["locate", "evt_legacy_wake", "--json"]);
    assert!(
        not_found["data"]["located"].is_null(),
        "legacy-only event must NOT be found by locate after flag retirement; got: {:?}",
        not_found["data"]["located"]
    );

    // recent --all returns room-based rows (including artifact from repo_b);
    // the legacy-only record is not present.
    let recent = repo_b.json(&["recent", "--all", "--json", "--limit", "10"]);
    assert_matches_schema("agent-rally.command.recent.v1.json", &recent);
    assert!(
        recent["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some(artifact_id)),
        "recent must include the room-based artifact fact"
    );
    assert!(
        !recent["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["record"]["event"]["id"] == "evt_legacy_wake"),
        "recent must NOT include the legacy-only event after flag retirement"
    );

    repo_a.cleanup();
    repo_b.cleanup();
    fs::remove_dir_all(home).ok();
}

#[test]
fn rally_refresh_does_not_clobber_corrupt_room_index() {
    let home = temp_path("rally-corrupt-index-home");
    // B17: global index is opt-in; this test exercises the corrupt-index recovery path.
    let workspace = Workspace::new_with_home("rally-corrupt-index", &home).with_global_index();
    let index_path = home.join(".agent-rally-point/rooms/v1/index.json");
    fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    fs::write(&index_path, "{not-json").unwrap();

    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "keeps corrupt index",
    ]);
    let artifact_id = artifact["data"]["fact"]["event_id"].as_str().unwrap();

    assert_eq!(fs::read_to_string(&index_path).unwrap(), "{not-json");

    let recent = workspace.json(&["recent", "--all", "--json", "--limit", "10"]);
    assert!(
        recent["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "room_index_unreadable")
    );
    assert!(
        recent["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some(artifact_id))
    );

    workspace.cleanup();
    fs::remove_dir_all(home).ok();
}

#[test]
fn rally_no_global_index_env_var_skips_home_index() {
    // R3: per-repo segmentation — `RALLY_NO_GLOBAL_INDEX=1` opts every rally
    // invocation out of writing to or reading from the home-dir index file.
    // Per-repo facts (in <repo_root>/.rally/) keep working normally; only the
    // cross-repo "what other rooms exist?" surface goes silent.
    let home = temp_path("rally-no-global-index-home");
    let workspace = Workspace::new_with_home("rally-no-global-index", &home);
    let index_path = home.join(".agent-rally-point/rooms/v1/index.json");

    // Sanity: at the moment, this directory shouldn't exist yet.
    assert!(!index_path.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env("RALLY_NO_GLOBAL_INDEX", "1")
        .args([
            "say",
            "artifact",
            "--json",
            "--tool",
            "codex",
            "--subject",
            "no-global-index write",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The home-dir index file must not have been created.
    assert!(
        !index_path.exists(),
        "RALLY_NO_GLOBAL_INDEX=1 must skip writes to {}",
        index_path.display()
    );

    // The per-repo segment log + db, however, *must* exist — coordination
    // within this repo is unaffected. R5 superseded the R1 monolith
    // (`.rally/ledger.jsonl`) with per-engagement segments under
    // `.rally/log/<engagement>.jsonl`; at least one segment must be present
    // after a successful `rally enter` / `rally say` round-trip.
    let log_dir = workspace.cwd.join(".rally/log");
    assert!(
        log_dir.exists() && log_dir.is_dir(),
        "expected .rally/log/ to exist"
    );
    let segment_count = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x == "jsonl")
        })
        .count();
    assert!(
        segment_count >= 1,
        "expected at least one .rally/log/*.jsonl segment, got {segment_count}"
    );
    assert!(workspace.cwd.join(".rally/facts.db").exists());

    // `recent --all` still works; it just collapses to "this repo only" and
    // emits no `room_index_unreadable` warning.
    let recent_output = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env("RALLY_NO_GLOBAL_INDEX", "1")
        .args(["recent", "--all", "--json", "--limit", "10"])
        .output()
        .unwrap();
    assert!(recent_output.status.success());
    let recent: Value = serde_json::from_slice(&recent_output.stdout).unwrap();
    assert!(
        recent["data"]["warnings"]
            .as_array()
            .map(|w| w
                .iter()
                .all(|warning| warning["code"] != "room_index_unreadable"))
            .unwrap_or(true),
        "no room_index_unreadable warning expected in fully-isolated mode"
    );

    workspace.cleanup();
    fs::remove_dir_all(home).ok();
}

#[test]
fn linked_git_worktree_uses_common_room() {
    let home = temp_path("rally-common-room-home");
    let primary = Workspace::new_with_home("rally-common-room-main", &home);
    let linked = Workspace {
        cwd: temp_path("rally-common-room-linked"),
        home: home.clone(),
        global_index: false,
    };
    fs::create_dir_all(&linked.cwd).unwrap();
    let linked_git_dir = primary.cwd.join(".git/worktrees/rally-common-room-linked");
    fs::create_dir_all(&linked_git_dir).unwrap();
    fs::write(linked_git_dir.join("commondir"), "../..\n").unwrap();
    fs::write(
        linked.cwd.join(".git"),
        format!("gitdir: {}\n", linked_git_dir.display()),
    )
    .unwrap();

    linked.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "shared room from linked worktree",
    ]);

    assert!(primary.cwd.join(".rally/facts.db").exists());
    assert!(!linked.cwd.join(".rally/facts.db").exists());
    let room = primary.json(&["room", "--json"]);
    // Component B: say claim auto-registers presence(1)+lead(2)+claim(3).
    assert_eq!(room["data"]["room"]["max_seq"], 3);

    linked.cleanup();
    primary.cleanup();
    fs::remove_dir_all(home).ok();
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
    assert_eq!(
        herdr["data"]["session"]["target"],
        "claude-herdr-reviewer-01"
    );
    assert_eq!(herdr["data"]["commands"]["start"][0][1], "agent");
    assert_eq!(herdr["data"]["commands"]["start"][0][2], "start");

    let herdr_inject = workspace.json(&[
        "inject",
        "herdr-reviewer-01",
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
        "herdr-reviewer-01",
        "--json",
        "--dry-run",
        "--lines",
        "30",
    ]);
    assert_matches_schema("agent-rally.command.session-action.v1.json", &herdr_capture);
    assert_eq!(herdr_capture["data"]["commands"][0][1], "agent");
    assert_eq!(herdr_capture["data"]["commands"][0][2], "read");

    let cmux_bin = workspace.cwd.join("fake-cmux");
    fs::write(
        &cmux_bin,
        "#!/bin/sh\nif [ \"$1\" = \"new-workspace\" ]; then echo workspace:cmux-builder; fi\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&cmux_bin).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cmux_bin, permissions).unwrap();
    let cmux_bin = cmux_bin.to_str().unwrap();

    let cmux = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--name",
        "cmux-builder",
        "--backend",
        "cmux",
        "--cmux-bin",
        cmux_bin,
    ]);
    assert_eq!(cmux["schema"], "agent-rally.command.run.v1");
    assert_matches_schema("agent-rally.command.run.v1.json", &cmux);
    assert_eq!(cmux["data"]["session"]["backend"], "cmux");
    assert_eq!(cmux["data"]["session"]["target"], "workspace:cmux-builder");
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
        "cmux-builder-01",
        "--json",
        "--text",
        "hello cmux",
        "--cmux-bin",
        cmux_bin,
    ]);
    assert_eq!(cmux_inject["data"]["commands"][0][1], "send-key");
    assert_eq!(cmux_inject["data"]["commands"][0][4], "ctrl+u");
    assert_eq!(cmux_inject["data"]["commands"][1][1], "send");
    assert_eq!(cmux_inject["data"]["commands"][1][4], "hello cmux");
    assert_eq!(cmux_inject["data"]["commands"][2][1], "send-key");
    assert_eq!(cmux_inject["data"]["commands"][2][4], "enter");

    let cmux_stop = workspace.json(&["stop", "cmux-builder-01", "--json", "--cmux-bin", cmux_bin]);
    assert_eq!(cmux_stop["data"]["commands"][0][1], "close-workspace");

    let herdr_stop = workspace.json(&[
        "stop",
        "herdr-reviewer-01",
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
    // Component B: codex's first say auto-wrote a role:lead decision (tool=codex),
    // so querying by tool=codex returns 1 decision.
    assert_eq!(
        by_tool["data"]["room"]["current_decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
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

    // Component B: say claim (codex) wrote presence(1)+lead(2)+claim(3).
    // say decision (pi) wrote presence(4)+decision(5). Use --since 3 so
    // the claim (seq=3) is excluded; the pi decision (seq=5) is included.
    let since = workspace.json(&["room", "--json", "--since", "3"]);
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
        handoff["data"]["fact"]["summary"], "Reviewer has enough context to proceed.",
        "space-separated --summary must round-trip into fact.summary"
    );
    assert_eq!(handoff["data"]["fact"]["subject"], "needs review");

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
        decision["data"]["fact"]["summary"], "Adopt the finite pickup protocol.",
        "--summary=value form must round-trip into fact.summary"
    );

    workspace.cleanup();
}

#[test]
fn rally_rejects_unknown_flags() {
    let workspace = Workspace::new("rally-argbag-flags");

    let output = workspace.output(&[
        "say",
        "--future-flag",
        "claim",
        "--json",
        "--tool=codex",
        "--subject",
        "unknown flags stay flags",
    ]);
    assert_eq!(output.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("`--future-flag` is not expected")
    );

    workspace.cleanup();
}

#[test]
fn rally_check_covers_completion_boundaries() {
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

    let removed_phase = workspace.output(&["check", "after-artifact", "--json", "--tool", "codex"]);
    assert_eq!(removed_phase.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&removed_phase.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("unsupported check phase after-artifact")
    );

    workspace.cleanup();
}

#[test]
fn rally_supports_all_required_fact_kinds() {
    let workspace = Workspace::new("rally-facts");

    // R9-readback: release requires --ref <live-claim> and resolve requires
    // --ref <live-target>.  Write prerequisite facts first, then use their
    // event_ids for the state-transition facts.

    // Write a claim to release (also triggers presence+lead on first say).
    let pre_claim = workspace.json(&[
        "say", "claim", "--json", "--tool", "codex",
        "--subject", "pre-claim for release test", "--path", "src/lib.rs",
    ]);
    let pre_claim_id = pre_claim["data"]["fact"]["event_id"].as_str().unwrap();

    // Write a blocker to resolve.
    let pre_blocker = workspace.json(&[
        "say", "blocker", "--json", "--tool", "codex",
        "--subject", "pre-blocker for resolve test", "--path", "src/lib.rs",
    ]);
    let pre_blocker_id = pre_blocker["data"]["fact"]["event_id"].as_str().unwrap();

    // Kinds that don't need a ref.
    let simple_kinds = ["decision", "artifact", "handoff", "risk", "lesson"];
    for kind in simple_kinds {
        let fact = workspace.json(&[
            "say", kind, "--json", "--tool", "codex",
            "--subject", kind, "--path", "src/lib.rs", "--evidence", "observed",
        ]);
        assert_eq!(fact["data"]["fact"]["kind"], kind, "kind mismatch for {kind}");
        assert_eq!(fact["data"]["fact"]["schema"], "agent-rally.fact.v1");
        DateTime::parse_from_rfc3339(fact["data"]["fact"]["created_at"].as_str().unwrap()).unwrap();
        assert_matches_schema("agent-rally.fact.v1.json", &fact["data"]["fact"]);
    }

    // R9: release --ref <live-claim>.
    let release_fact = workspace.json(&[
        "say", "release", "--json", "--tool", "codex",
        "--subject", "release", "--path", "src/lib.rs",
        "--ref", pre_claim_id,
    ]);
    assert_eq!(release_fact["data"]["fact"]["kind"], "release");
    assert_eq!(release_fact["data"]["fact"]["schema"], "agent-rally.fact.v1");
    assert_matches_schema("agent-rally.fact.v1.json", &release_fact["data"]["fact"]);
    // R9-readback: verified {room, seq} must be present in the response.
    assert!(
        release_fact["data"]["verified"]["seq"].as_i64().unwrap_or(0) > 0,
        "release must return verified.seq > 0"
    );
    assert!(
        !release_fact["data"]["verified"]["room"].as_str().unwrap_or("").is_empty(),
        "release must return verified.room"
    );

    // R9: resolve --ref <live-blocker>.
    let resolve_fact = workspace.json(&[
        "say", "resolve", "--json", "--tool", "codex",
        "--subject", "resolve", "--path", "src/lib.rs",
        "--ref", pre_blocker_id,
    ]);
    assert_eq!(resolve_fact["data"]["fact"]["kind"], "resolve");
    assert_eq!(resolve_fact["data"]["fact"]["schema"], "agent-rally.fact.v1");
    assert_matches_schema("agent-rally.fact.v1.json", &resolve_fact["data"]["fact"]);

    workspace.cleanup();
}

/// Component A: presence substrate (B16 acceptance test).
///
/// Verifies:
/// - `enter` as tool X → squads[] contains X, lead == X, room_id non-null.
/// - Second `enter` as tool Y → Y added to squads, lead still X.
/// - Presence fact survives a ledger replay (segment round-trip).
#[test]
fn presence_substrate_enter_writes_presence_and_lead() {
    let workspace = Workspace::new("rally-presence");

    // --- First enter: tool "alpha" ---
    let enter_a = workspace.json(&["enter", "--json", "--tool", "alpha"]);
    assert_eq!(enter_a["schema"], "agent-rally.command.enter.v1");
    // room_id must be a non-null, non-empty string (Component A requirement).
    let room_id = enter_a["data"]["room_id"].as_str().unwrap();
    assert!(!room_id.is_empty(), "room_id must be non-empty");

    // Room projection after first enter must have alpha in squads and as lead.
    let room_a = workspace.json(&["room", "--json"]);
    let squads_a = room_a["data"]["room"]["squads"].as_array().unwrap();
    assert!(
        squads_a.iter().any(|s| s["tool"] == "alpha"),
        "squads must contain alpha after enter"
    );
    assert_eq!(
        room_a["data"]["room"]["lead"], "alpha",
        "first entrant is lead"
    );

    // --- Second enter: tool "beta" ---
    let enter_b = workspace.json(&["enter", "--json", "--tool", "beta"]);
    let room_id_b = enter_b["data"]["room_id"].as_str().unwrap();
    assert_eq!(room_id, room_id_b, "room_id stable across enters");

    let room_b = workspace.json(&["room", "--json"]);
    let squads_b = room_b["data"]["room"]["squads"].as_array().unwrap();
    assert!(
        squads_b.iter().any(|s| s["tool"] == "alpha"),
        "alpha still in squads after beta enters"
    );
    assert!(
        squads_b.iter().any(|s| s["tool"] == "beta"),
        "beta in squads after entering"
    );
    // Lead must still be alpha (first entrant).
    assert_eq!(
        room_b["data"]["room"]["lead"], "alpha",
        "lead stays with first entrant"
    );

    // --- B16 round-trip: delete facts.db, reopen from segments, re-check ---
    // Presence and lead facts must survive a full ledger replay.
    let facts_db = workspace.cwd.join(".rally/facts.db");
    std::fs::remove_file(&facts_db).ok();
    let _ = std::fs::remove_file(facts_db.with_extension("db-shm"));
    let _ = std::fs::remove_file(facts_db.with_extension("db-wal"));

    let room_replay = workspace.json(&["room", "--json"]);
    let squads_replay = room_replay["data"]["room"]["squads"]
        .as_array()
        .unwrap();
    assert!(
        squads_replay.iter().any(|s| s["tool"] == "alpha"),
        "alpha survives ledger replay"
    );
    assert!(
        squads_replay.iter().any(|s| s["tool"] == "beta"),
        "beta survives ledger replay"
    );
    assert_eq!(
        room_replay["data"]["room"]["lead"], "alpha",
        "lead survives ledger replay"
    );

    // Presence facts are readable via `recent` (the main non-room read path).
    let recent = workspace.json(&["recent", "--json"]);
    let rows = recent["data"]["rows"].as_array().unwrap();
    assert!(
        rows.iter().any(|r| r["fact"]["kind"] == "presence"),
        "presence facts appear in recent rows"
    );

    workspace.cleanup();
}

/// B17 — migrate-legacy: one-shot replay of legacy changes.jsonl into per-repo
/// ledger; idempotent on second run; legacy file untouched.
#[test]
fn rally_migrate_legacy_replays_and_is_idempotent() {
    let home = temp_path("rally-migrate-legacy-home");
    let workspace = Workspace::new_with_home("rally-migrate-legacy", &home);

    // Seed a legacy channel whose slug matches the workspace repo basename.
    let repo_basename = workspace
        .cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("rally-migrate-legacy")
        .to_string();

    // Construct a minimal current-format rally fact line.
    let fact_json = serde_json::json!({
        "schema": "agent-rally.fact.v1",
        "event_id": "evt_migrate_test_001",
        "seq": 1,
        "thread_id": "thr_migrate_test",
        "kind": "decision",
        "tool": "codex",
        "role": null,
        "subject": "legacy migrate decision",
        "scope": [],
        "created_at": "2026-05-28T12:00:00Z",
        "summary": "seeded for B17 migrate test",
        "evidence": [],
        "target": null,
        "ref_id": null,
        "status": null,
        "severity": null,
        "uri": null,
        "session": null,
    })
    .to_string();

    // Seed legacy channel under the matching slug directory.
    let apps_dir = home.join(".agent-rally-point/apps").join(&repo_basename);
    fs::create_dir_all(&apps_dir).unwrap();
    let legacy_file = apps_dir.join("changes.jsonl");
    fs::write(&legacy_file, format!("{fact_json}\n")).unwrap();

    // Also seed a non-matching slug to confirm it is not migrated.
    let unrelated_dir = home.join(".agent-rally-point/apps/_unscoped");
    fs::create_dir_all(&unrelated_dir).unwrap();
    let unrelated_fact = serde_json::json!({
        "schema": "agent-rally.fact.v1",
        "event_id": "evt_unrelated_999",
        "seq": 1,
        "thread_id": "thr_unrelated",
        "kind": "decision",
        "tool": "codex",
        "role": null,
        "subject": "unrelated repo decision",
        "scope": [],
        "created_at": "2026-05-28T12:00:00Z",
        "summary": null,
        "evidence": [],
        "target": null,
        "ref_id": null,
        "status": null,
        "severity": null,
        "uri": null,
        "session": null,
    })
    .to_string();
    fs::write(unrelated_dir.join("changes.jsonl"), format!("{unrelated_fact}\n")).unwrap();

    // First migrate-legacy run.
    let first = workspace.json(&["migrate-legacy", "--json"]);
    assert!(
        first["ok"].as_bool().unwrap_or(false),
        "migrate-legacy must return ok:true on first run"
    );
    let migrated = first["data"]["facts_migrated"].as_u64().unwrap_or(0);
    assert_eq!(migrated, 1, "first run must migrate exactly 1 fact");
    assert_eq!(
        first["data"]["facts_skipped_existing"].as_u64().unwrap_or(99),
        0,
        "no facts should be skipped on first run"
    );

    // Verify the fact appears in recent.
    let recent = workspace.json(&["recent", "--json"]);
    assert!(
        recent["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some("evt_migrate_test_001")),
        "migrated fact must appear in recent after first run"
    );

    // Unrelated fact must NOT appear (different slug).
    assert!(
        !recent["data"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some("evt_unrelated_999")),
        "unrelated-slug fact must NOT be migrated"
    );

    // Second migrate-legacy run: idempotent.
    let second = workspace.json(&["migrate-legacy", "--json"]);
    assert_eq!(
        second["data"]["facts_migrated"].as_u64().unwrap_or(99),
        0,
        "second run must migrate 0 facts (already in ledger)"
    );
    assert_eq!(
        second["data"]["facts_skipped_existing"].as_u64().unwrap_or(0),
        1,
        "second run must count 1 skipped-existing"
    );

    // Legacy file untouched (non-destructive migrator).
    assert!(legacy_file.exists(), "migrate-legacy must not delete the legacy file");

    workspace.cleanup();
    fs::remove_dir_all(home).ok();
}

// =============================================================================
// B13 tests: produces/depends round-trip, receipt links a handoff, check ci
// =============================================================================

/// B13-1: `rally say claim --produces`/`--depends` round-trips through the fact.
///
/// The markers are encoded as `produces:<x>` / `depends:<x>` in the fact's
/// `evidence` array.  This test verifies the claim is readable back from the
/// room with both markers present.
#[test]
fn b13_produces_depends_round_trip_on_claim() {
    let workspace = Workspace::new("b13-produces-depends");

    let claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code:01",
        "--subject",
        "implement auth module",
        "--produces",
        "src/auth.rs",
        "--produces",
        "src/auth/token.rs",
        "--depends",
        "src/config.rs",
    ]);
    assert_eq!(claim["schema"], "agent-rally.command.say.v1");
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap();

    // Read back via room and verify evidence markers are present.
    let room = workspace.json(&["room", "--json"]);
    let claims = room["data"]["room"]["active_claims"].as_array().unwrap();
    let stored_claim = claims
        .iter()
        .find(|c| c["event_id"].as_str() == Some(claim_id))
        .expect("claim must appear in active_claims");

    let evidence: Vec<&str> = stored_claim["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str())
        .collect();

    assert!(
        evidence.iter().any(|e| *e == "produces:src/auth.rs"),
        "evidence must contain produces:src/auth.rs; got: {:?}",
        evidence
    );
    assert!(
        evidence.iter().any(|e| *e == "produces:src/auth/token.rs"),
        "evidence must contain produces:src/auth/token.rs; got: {:?}",
        evidence
    );
    assert!(
        evidence.iter().any(|e| *e == "depends:src/config.rs"),
        "evidence must contain depends:src/config.rs; got: {:?}",
        evidence
    );

    workspace.cleanup();
}

/// B13-2: a receipt fact links to a handoff and closes it from `open_handoffs`.
///
/// The 2-hop chain is: handoff → receipt → (handoff closed).
/// The receipt is emitted via `rally say receipt --ref <handoff-id>`.
#[test]
fn b13_receipt_links_handoff_and_closes_it() {
    let workspace = Workspace::new("b13-receipt-links-handoff");

    // Emit a handoff.
    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex:01",
        "--target",
        "claude_code:01",
        "--subject",
        "please review auth module",
    ]);
    let handoff_id = handoff["data"]["fact"]["event_id"].as_str().unwrap();

    // Verify handoff is open.
    let room_before = workspace.json(&["room", "--json"]);
    assert!(
        room_before["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"].as_str() == Some(handoff_id)),
        "handoff must be open before receipt"
    );

    // Emit a receipt referencing the handoff.
    let receipt = workspace.json(&[
        "say",
        "receipt",
        "--json",
        "--tool",
        "claude_code:01",
        "--subject",
        "acknowledged auth review request",
        "--ref",
        handoff_id,
        "--summary",
        "started reviewing the auth module",
    ]);
    assert_eq!(receipt["schema"], "agent-rally.command.say.v1");
    let receipt_id = receipt["data"]["fact"]["event_id"].as_str().unwrap();
    assert!(!receipt_id.is_empty(), "receipt must have a valid event_id");

    // The receipt's ref must point to the handoff.
    assert_eq!(
        receipt["data"]["fact"]["ref"].as_str(),
        Some(handoff_id),
        "receipt ref must equal the handoff event_id"
    );
    assert_eq!(
        receipt["data"]["fact"]["kind"].as_str(),
        Some("receipt"),
        "fact kind must be 'receipt'"
    );

    // Verify the handoff is now closed (removed from open_handoffs).
    let room_after = workspace.json(&["room", "--json"]);
    let open_handoffs = room_after["data"]["room"]["open_handoffs"]
        .as_array()
        .unwrap();
    assert!(
        !open_handoffs.iter().any(|f| f["event_id"].as_str() == Some(handoff_id)),
        "handoff must be closed after receipt"
    );

    workspace.cleanup();
}

/// B13-3a: `rally check ci --strict` exits 0 when the room is clean.
#[test]
fn b13_check_ci_strict_exits_zero_clean_room() {
    let workspace = Workspace::new("b13-check-ci-clean");

    // No blockers, no unsatisfied depends, no old handoffs — room is clean.
    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex:01",
        "--subject",
        "clean room claim",
    ]);

    let (result, output) = workspace.json_with_status(&["check-ci", "--json", "--strict"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "check-ci --strict must exit 0 on a clean room; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["schema"], "agent-rally.command.check-ci.v1");
    assert_eq!(result["data"]["check_ci"]["pass"], true);
    assert_eq!(
        result["data"]["check_ci"]["offenders"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    workspace.cleanup();
}

/// B13-3b: `rally check ci --strict` exits 4 with an unresolved blocker.
#[test]
fn b13_check_ci_strict_exits_4_with_unresolved_blocker() {
    let workspace = Workspace::new("b13-check-ci-blocker");

    let blocker = workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "codex:01",
        "--subject",
        "CI pipeline broken",
    ]);
    let blocker_id = blocker["data"]["fact"]["event_id"].as_str().unwrap();

    let (result, output) = workspace.json_with_status(&["check-ci", "--json", "--strict"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "check-ci --strict must exit 4 when an unresolved blocker exists; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["data"]["check_ci"]["pass"], false);
    let offenders = result["data"]["check_ci"]["offenders"].as_array().unwrap();
    assert!(
        offenders
            .iter()
            .any(|o| o["code"] == "unresolved-blocker" && o["fact_id"] == blocker_id),
        "offenders must include the unresolved blocker; got: {:?}",
        offenders
    );

    workspace.cleanup();
}

/// B13-3c: `rally check ci --strict` exits 4 with a dep-not-met offender.
///
/// A claim with `depends:src/config.rs` is an offender when no fact in the
/// room carries `produces:src/config.rs` in its evidence.
#[test]
fn b13_check_ci_strict_exits_4_with_dep_not_met() {
    let workspace = Workspace::new("b13-check-ci-dep-not-met");

    // A claim with an unsatisfied depends.
    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex:01",
        "--subject",
        "auth impl",
        "--depends",
        "src/config.rs",
    ]);

    let (result, output) = workspace.json_with_status(&["check-ci", "--json", "--strict"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "check-ci --strict must exit 4 with an unsatisfied dependency; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["data"]["check_ci"]["pass"], false);
    let offenders = result["data"]["check_ci"]["offenders"].as_array().unwrap();
    assert!(
        offenders.iter().any(|o| o["code"] == "dep-not-met"),
        "offenders must include a dep-not-met entry; got: {:?}",
        offenders
    );

    // Satisfying the dep removes the offender.
    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code:01",
        "--subject",
        "config module done",
        "--evidence",
        "produces:src/config.rs",
    ]);

    let (result2, output2) = workspace.json_with_status(&["check-ci", "--json", "--strict"]);
    // The dep-not-met offender is gone; the blocker list is also empty.
    // (active_claims may still include the original claim with depends marker,
    //  but now produces:src/config.rs is in the room so it is satisfied.)
    let offenders2 = result2["data"]["check_ci"]["offenders"].as_array().unwrap();
    assert!(
        !offenders2.iter().any(|o| o["code"] == "dep-not-met"),
        "dep-not-met offender must be gone after a produces fact is emitted; got: {:?} (exit: {})",
        offenders2,
        output2.status.code().unwrap_or(-1)
    );

    workspace.cleanup();
}

/// B13-4: `rally check ci --help` exits 0.
#[test]
fn b13_check_ci_help_exits_zero() {
    let workspace = Workspace::new("b13-check-ci-help");
    let output = workspace.output(&["check-ci", "--help"]);
    assert!(
        output.status.success(),
        "check-ci --help must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("strict") || stdout.contains("CI"),
        "help text must mention --strict; got: {stdout}"
    );

    workspace.cleanup();
}
// #6 Source-grounding integration tests
// =========================================================================

/// claim→artifact with unchanged file: expect grounded:false risk and ungrounded-artifact subject.
/// claim→artifact with changed file: no grounded:false risk.
#[test]
fn advisory_6_source_grounding_unchanged_file_flags_ungrounded_artifact() {
    let workspace = Workspace::new("advisory-6-unchanged");

    // Create a source file.
    let src = workspace.cwd.join("mylib.rs");
    fs::write(&src, b"fn placeholder() {}").unwrap();

    // claim with --path pointing to mylib.rs
    let claim = workspace.json(&[
        "say", "claim",
        "--json",
        "--tool", "test-agent",
        "--subject", "working on mylib",
        "--path", "mylib.rs",
    ]);
    assert!(claim["ok"].as_bool().unwrap_or(false), "claim must succeed");
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap().to_string();

    // Post artifact WITHOUT modifying mylib.rs (unchanged → ungrounded).
    let artifact = workspace.json(&[
        "say", "artifact",
        "--json",
        "--tool", "test-agent",
        "--subject", "done with mylib",
        "--ref", &claim_id,
    ]);
    assert!(artifact["ok"].as_bool().unwrap_or(false), "artifact must succeed");

    // Room must have a risk fact with subject containing "ungrounded-artifact"
    // and scope containing "grounded:false".
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ungrounded: Vec<_> = risks.iter()
        .filter(|r| r["subject"].as_str().unwrap_or("").contains("ungrounded-artifact"))
        .collect();
    assert!(
        !ungrounded.is_empty(),
        "expected ungrounded-artifact risk fact; risks: {:?}",
        risks.iter().map(|r| r["subject"].as_str().unwrap_or("")).collect::<Vec<_>>()
    );
    let scope = ungrounded[0]["scope"].as_array().unwrap();
    assert!(
        scope.iter().any(|s| s.as_str() == Some("grounded:false")),
        "risk scope must contain grounded:false; scope: {:?}", scope
    );
    assert_eq!(
        ungrounded[0]["severity"].as_str().unwrap_or(""),
        "warn",
        "risk severity must be warn"
    );

    workspace.cleanup();
}

#[test]
fn advisory_6_source_grounding_changed_file_does_not_flag() {
    let workspace = Workspace::new("advisory-6-changed");

    let src = workspace.cwd.join("changed.rs");
    fs::write(&src, b"fn before() {}").unwrap();

    let claim = workspace.json(&[
        "say", "claim",
        "--json",
        "--tool", "test-agent",
        "--subject", "working on changed",
        "--path", "changed.rs",
    ]);
    let claim_id = claim["data"]["fact"]["event_id"].as_str().unwrap().to_string();

    // Modify the file before posting the artifact.
    fs::write(&src, b"fn after_modification() {}").unwrap();

    let artifact = workspace.json(&[
        "say", "artifact",
        "--json",
        "--tool", "test-agent",
        "--subject", "done with changed",
        "--ref", &claim_id,
    ]);
    assert!(artifact["ok"].as_bool().unwrap_or(false), "artifact must succeed");

    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ungrounded: Vec<_> = risks.iter()
        .filter(|r| r["subject"].as_str().unwrap_or("").contains("ungrounded-artifact"))
        .collect();
    assert!(
        ungrounded.is_empty(),
        "changed file must NOT produce ungrounded-artifact risk; risks: {:?}",
        ungrounded
    );

    workspace.cleanup();
}

// =========================================================================
// #8 Ripple detector integration test
// =========================================================================

/// Ripple alert fires when a changed file's pub fn is referenced by a peer claim's file.
#[test]
fn advisory_8_ripple_alert_fires_on_pub_sig_change_affecting_peer_claim() {
    let workspace = Workspace::new("advisory-8-ripple");

    // Set up source structure.
    fs::create_dir_all(workspace.cwd.join("src")).unwrap();
    fs::create_dir_all(workspace.cwd.join("consumer")).unwrap();

    let provider = workspace.cwd.join("src/provider.rs");
    fs::write(&provider, b"pub fn shared_api() -> i32 { 0 }").unwrap();

    let consumer = workspace.cwd.join("consumer/main.rs");
    fs::write(&consumer, b"let x = shared_api();").unwrap();

    // peer-tool claims consumer/main.rs
    let peer_claim = workspace.json(&[
        "say", "claim",
        "--json",
        "--tool", "peer-tool",
        "--subject", "peer owns consumer",
        "--path", "consumer/main.rs",
    ]);
    assert!(peer_claim["ok"].as_bool().unwrap_or(false));
    let peer_claim_id = peer_claim["data"]["fact"]["event_id"].as_str().unwrap().to_string();

    // my-tool claims src/provider.rs
    let my_claim = workspace.json(&[
        "say", "claim",
        "--json",
        "--tool", "my-tool",
        "--subject", "my-tool owns provider",
        "--path", "src/provider.rs",
    ]);
    assert!(my_claim["ok"].as_bool().unwrap_or(false));
    let my_claim_id = my_claim["data"]["fact"]["event_id"].as_str().unwrap().to_string();

    // Modify src/provider.rs (so grounding sees it as changed).
    fs::write(&provider, b"pub fn shared_api() -> i32 { 1 } pub fn new_fn() {}").unwrap();

    // my-tool posts artifact closing its claim (ref → my_claim_id).
    let artifact = workspace.json(&[
        "say", "artifact",
        "--json",
        "--tool", "my-tool",
        "--subject", "provider updated",
        "--ref", &my_claim_id,
    ]);
    assert!(artifact["ok"].as_bool().unwrap_or(false), "artifact must succeed");

    // Room must have a ripple-alert risk fact targeting peer-tool.
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ripple: Vec<_> = risks.iter()
        .filter(|r| r["subject"].as_str().unwrap_or("").contains("ripple-alert"))
        .collect();
    assert!(
        !ripple.is_empty(),
        "expected ripple-alert risk fact; risks: {:?}",
        risks.iter().map(|r| r["subject"].as_str().unwrap_or("")).collect::<Vec<_>>()
    );
    assert_eq!(
        ripple[0]["severity"].as_str().unwrap_or(""),
        "warn",
        "ripple-alert severity must be warn"
    );
    assert!(
        ripple[0]["subject"].as_str().unwrap_or("").contains("peer-tool"),
        "ripple-alert subject must name the affected peer; got: {:?}",
        ripple[0]["subject"]
    );

    // Suppress unused variable warning for peer_claim_id.
    let _ = peer_claim_id;

    workspace.cleanup();
}

// =========================================================================
// #9 Tier-fit advisory integration tests
// =========================================================================

/// tier-fit --help exits 0.
#[test]
fn advisory_9_tier_fit_help_exits_zero() {
    let workspace = Workspace::new("advisory-9-help");
    let output = workspace.output(&["check", "tier-fit", "--help"]);
    assert!(
        output.status.success(),
        "rally check tier-fit --help must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    workspace.cleanup();
}

/// Without a calibration fact, tier-fit returns neutral no_calibration status.
#[test]
fn advisory_9_tier_fit_no_calibration_returns_neutral() {
    let workspace = Workspace::new("advisory-9-no-cal");
    let result = workspace.json(&[
        "check", "tier-fit",
        "--json",
        "--role", "executor",
        "--proposed-tier", "opus",
    ]);
    assert!(result["ok"].as_bool().unwrap_or(false), "tier-fit must succeed (advisory)");
    let status = result["data"]["check"]["tier_fit"]["status"].as_str().unwrap_or("");
    assert_eq!(status, "no_calibration", "must return no_calibration when no fact present");
    workspace.cleanup();
}

/// With a calibration fact, mismatch emits a tier_mismatch finding.
#[test]
fn advisory_9_tier_fit_mismatch_emits_finding_vs_calibration() {
    let workspace = Workspace::new("advisory-9-mismatch");

    // Post a tier-calibration decision fact.
    workspace.json(&[
        "say", "decision",
        "--json",
        "--tool", "lead",
        "--subject", "tier-calibration",
        "--scope", "tier-calibration",
        "--summary", "role:executor=cheapest:sonnet",
    ]);

    let result = workspace.json(&[
        "check", "tier-fit",
        "--json",
        "--role", "executor",
        "--proposed-tier", "opus",
    ]);
    assert!(result["ok"].as_bool().unwrap_or(false));
    let status = result["data"]["check"]["tier_fit"]["status"].as_str().unwrap_or("");
    assert_eq!(status, "mismatch", "must be mismatch when proposed tier != calibrated cheapest");
    let finding_code = result["data"]["check"]["tier_fit"]["finding"]["code"].as_str().unwrap_or("");
    assert_eq!(finding_code, "tier_mismatch");
    let finding_severity = result["data"]["check"]["tier_fit"]["finding"]["severity"].as_str().unwrap_or("");
    assert_eq!(finding_severity, "info", "tier mismatch is advisory info, not blocking");

    workspace.cleanup();
}

/// With a matching tier, tier-fit returns ok.
#[test]
fn advisory_9_tier_fit_ok_when_matching_calibration() {
    let workspace = Workspace::new("advisory-9-ok");

    workspace.json(&[
        "say", "decision",
        "--json",
        "--tool", "lead",
        "--subject", "tier-calibration",
        "--scope", "tier-calibration",
        "--summary", "role:executor=cheapest:sonnet",
    ]);

    let result = workspace.json(&[
        "check", "tier-fit",
        "--json",
        "--role", "executor",
        "--proposed-tier", "sonnet",
    ]);
    assert!(result["ok"].as_bool().unwrap_or(false));
    let status = result["data"]["check"]["tier_fit"]["status"].as_str().unwrap_or("");
    assert_eq!(status, "ok");

    workspace.cleanup();
}

/// B-whoami smoke test: `rally whoami --json` exits 0 and returns repo_root + build_id.
///
/// Fields are flat in `data` (no nested "whoami" key) — mirrors how `version`
/// serialises its data.
#[test]
fn rally_whoami_json_exits_zero_and_returns_identity() {
    let workspace = Workspace::new("rally-whoami");

    let result = workspace.json(&["whoami", "--json"]);
    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "whoami must return ok:true; got: {result}"
    );

    let data = &result["data"];
    let repo_root = data["repo_root"].as_str().unwrap_or("");
    assert!(!repo_root.is_empty(), "repo_root must be non-empty");

    let build_id = data["build_id"].as_str().unwrap_or("");
    assert!(!build_id.is_empty(), "build_id must be non-empty");
    assert!(
        build_id.contains('+'),
        "build_id must be <version>+<hash>; got: {build_id}"
    );

    // cwd and worktree must also be present and non-empty.
    let cwd = data["cwd"].as_str().unwrap_or("");
    assert!(!cwd.is_empty(), "cwd must be non-empty");
    let worktree = data["worktree"].as_str().unwrap_or("");
    assert!(!worktree.is_empty(), "worktree must be non-empty");

    workspace.cleanup();
}

/// B-whoami: --tool flag is echoed back in the output.
#[test]
fn rally_whoami_with_tool_reflects_tool_in_output() {
    let workspace = Workspace::new("rally-whoami-tool");

    let result = workspace.json(&["whoami", "--json", "--tool", "claude_code:01"]);
    assert!(result["ok"].as_bool().unwrap_or(false));

    let tool = result["data"]["tool"].as_str().unwrap_or("");
    assert_eq!(tool, "claude_code:01", "tool must be echoed back");

    workspace.cleanup();
}
