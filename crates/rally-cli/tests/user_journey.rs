// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::DateTime;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Serializes the heavy `rally run` managed-session tests against each other.
/// Each spawns subprocesses that write session-reservation facts to one SQLite
/// store; running several concurrently compounds write-lock contention past the
/// retry budget — a test-isolation artifact, not a runtime bug (see BACKLOG
/// B-test-flake / B-write-burst-scale). Poison-tolerant so a panicking holder
/// cannot wedge the rest of the suite.
static RALLY_RUN_GUARD: Mutex<()> = Mutex::new(());
fn serialize_rally_run() -> MutexGuard<'static, ()> {
    RALLY_RUN_GUARD.lock().unwrap_or_else(|p| p.into_inner())
}

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
    /// When true, passes RALLY_GLOBAL_INDEX=1 to every command.
    global_index: bool,
    /// When true, passes RALLY_NO_WORKTREE=1 to every command so `rally run`
    /// skips the per-agent worktree provisioning step.  Default = true,
    /// because the bulk of the test corpus uses stub `.git` directories
    /// where `git worktree add` cannot succeed and where the agent-launch
    /// surface is what's being tested, not the isolation.  Tests that
    /// exercise the default-on worktree provisioning use [`real_repo`]
    /// instead.
    suppress_worktree: bool,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            cwd,
            home,
            global_index: false,
            suppress_worktree: true,
        }
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
            suppress_worktree: true,
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
        if self.suppress_worktree {
            cmd.env("RALLY_NO_WORKTREE", "1");
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

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// True when the `git` binary is on PATH; used to skip worktree-isolation
/// tests in stripped CI environments rather than failing them spuriously.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Initialize a real git repository at `root` with one empty commit on
/// `main`.  Used by tests that exercise the default-on worktree
/// provisioning path (which calls real `git worktree add`).
fn init_real_repo(root: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "rally@example.test"]);
    run(&["config", "user.name", "Rally Test"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}

/// Build a [`Workspace`] backed by a REAL git repo (not a stub `.git` dir)
/// AND with the worktree-isolation env-var escape OFF, so `rally run`
/// exercises its default-on per-agent worktree provisioning.  This is
/// the harness for the Phase 1b acceptance tests.
fn real_repo_workspace(name: &str) -> Workspace {
    let cwd = temp_path(&format!("{name}-cwd"));
    let home = temp_path(&format!("{name}-home"));
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    init_real_repo(&cwd);
    Workspace {
        cwd,
        home,
        global_index: false,
        suppress_worktree: false, // default-ON path under test.
    }
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
    assert_matches_schema("agent-rally.fact.v1.json", &claim["data"]["say"]["fact"]);
    let claim_id = claim["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
    assert_eq!(enter["data"]["enter"]["cursor"]["before"], 0);
    // Component B: say claim (claude) wrote presence(1)+lead(2)+claim(3);
    // say decision (pi) wrote presence(4)+decision(5). codex enter writes
    // presence(6) via ensure_presence (lead already set by claude), then the
    // f4-widened fleet check writes an `unmanaged-agent` risk(7) because
    // `codex` has no active managed-session record. cursor_after is set to
    // post-presence max_seq=7 so codex's own presence + risk facts are
    // excluded from "new peer content" on the next enter.
    assert_eq!(enter["data"]["enter"]["cursor"]["after"], 7);
    assert_eq!(enter["data"]["enter"]["cursor"]["advanced"], true);
    assert!(
        enter["data"]["enter"]["entry"]["do_not"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["event_id"] == claim_id)
    );
    assert!(
        enter["data"]["enter"]["entry"]["know"]
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
    // R10/cursor: second enter's cursor_before = 7 (ledger-derived from the
    // Read checkpoint written by the first enter, which recorded
    // content_max_seq=7). The second enter detects codex as already active
    // (B11) and writes a durable risk fact AFTER ensure_presence runs
    // (post-f4-order: presence first, then warning-block risk facts so the
    // squad-membership guard inside ensure_presence is not short-circuited).
    // Seq breakdown after first enter:
    //   1: presence(claude), 2: lead(claude), 3: claim, 4: presence(pi), 5: decision
    //   6: presence(codex) — first enter's ensure_presence (now FIRST)
    //   7: f4 unmanaged-agent risk(codex) — codex has no managed session
    //   8: Read checkpoint (first enter's maybe_append_read_checkpoint at content_max=7)
    //   9: B11 risk fact (second enter's duplicate-active-squad detection;
    //      the unmanaged-agent risk dedups via already_recorded check)
    // cursor_after = snapshot.max_seq = 9.
    assert_eq!(enter_again["data"]["enter"]["cursor"]["before"], 7);
    assert_eq!(enter_again["data"]["enter"]["cursor"]["after"], 9);
    assert_eq!(enter_again["data"]["enter"]["cursor"]["advanced"], true);

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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();
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
    let artifact_id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();

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
fn rally_next_prioritizes_targeted_ack_artifact_over_stale_handoff() {
    let workspace = Workspace::new("rally-next-targeted-ack-artifact");
    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "claude_code",
        "--target",
        "codex",
        "--subject",
        "message",
        "--evidence",
        r#"{"requires_ack":true,"payload":{"summary":"review branch"}}"#,
    ]);
    let artifact_id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();
    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "claude_code",
        "--target",
        "codex",
        "--subject",
        "newer handoff",
    ]);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex", "--limit", "4"]);
    assert_eq!(next["schema"], "agent-rally.command.next.v1");
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["action"], "review_artifact");
    assert_eq!(
        next["data"]["next"]["reason"],
        "targeted_peer_artifact_requires_ack"
    );
    assert_eq!(next["data"]["next"]["target_event_id"], artifact_id);
    assert!(
        next["data"]["next"]["alternatives"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["action"] == "respond_to_handoff"
                && item["target_event_id"] == handoff_id),
        "newer handoff should stay visible as an alternative"
    );

    workspace.cleanup();
}

#[test]
fn rally_artifact_ref_consumes_targeted_handoff_only_from_target_tool() {
    // intent: a target-authored artifact that references a handoff via --ref
    // should close that handoff (drop it from room.open_handoffs and next).
    // Evidence from non-target tools must not ACK/complete someone else's
    // targeted handoff. Blockers still require resolve, and claims still
    // require release — artifact --ref is not a universal closer.
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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let blocker = workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "broken pipeline",
    ]);
    let blocker_id = blocker["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
    let claim_id = claim["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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

    // Non-target commentary/evidence that references the targeted handoff does
    // not close it.
    workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "codex:monitor",
        "--subject",
        "transport observation",
        "--ref",
        handoff_id,
        "--evidence",
        "delivered but not acked",
    ]);
    let room_after_monitor_artifact = workspace.json(&["room", "--json"]);
    assert!(
        room_after_monitor_artifact["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == handoff_id),
        "non-target artifact --ref must not close a targeted handoff"
    );

    // A wrong-tool resolve is rejected before it can write a bogus closeout.
    let wrong_resolve = workspace.output(&[
        "say",
        "resolve",
        "--json",
        "--tool",
        "codex:monitor",
        "--ref",
        handoff_id,
        "--subject",
        "wrong target resolve",
    ]);
    assert!(
        !wrong_resolve.status.success(),
        "wrong-tool resolve must fail"
    );
    let wrong_resolve_text = format!(
        "{}{}",
        String::from_utf8_lossy(&wrong_resolve.stdout),
        String::from_utf8_lossy(&wrong_resolve.stderr)
    );
    assert!(
        wrong_resolve_text.contains("targeted to claude_code"),
        "wrong-tool resolve should explain target mismatch; got {wrong_resolve_text}"
    );
    let room_after_wrong_resolve = workspace.json(&["room", "--json"]);
    assert!(
        room_after_wrong_resolve["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == handoff_id),
        "rejected wrong-tool resolve must not close a targeted handoff"
    );

    // Target-authored artifact references each fact via --ref. Only the
    // handoff should close.
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
    let risk_id = risk["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
fn rally_backlog_plan_status_is_actionable_for_target() {
    let workspace = Workspace::new("rally-backlog-plan-status");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code",
        "--path",
        "docs/ORCHESTRATION.md",
        "--subject",
        "peer owns implementation surface",
    ]);

    let added = workspace.json(&[
        "backlog",
        "add",
        "--tool",
        "claude_code",
        "--id",
        "arp-plan",
        "--intent",
        "publish the ARP lane plan and timeline",
        "--target",
        "codex",
        "--status",
        "planned",
        "--expected-by",
        "noon",
        "--owns",
        "docs/ORCHESTRATION.md",
        "--json",
    ]);
    assert_eq!(added["data"]["backlog"]["items"][0]["target"], "codex");
    assert_eq!(added["data"]["backlog"]["items"][0]["expected_by"], "noon");
    assert_eq!(added["data"]["backlog"]["items"][0]["status"], "planned");

    let next = workspace.json(&["next", "--json", "--tool", "codex", "--limit", "3"]);
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["action"], "update_plan_status");
    assert_eq!(
        next["data"]["next"]["reason"],
        "targeted_backlog_plan_needs_status"
    );
    assert_eq!(next["data"]["next"]["fact"]["kind"], "backlog_item");
    assert!(
        next["data"]["next"]["suggested_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command
                .as_str()
                .unwrap()
                .contains("rally backlog update --tool codex --id arp-plan"))
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
            .iter()
            .all(|command| !command.as_str().unwrap().contains("before-write"))
    );

    let updated = workspace.json(&[
        "backlog",
        "update",
        "--tool",
        "codex",
        "--id",
        "arp-plan",
        "--status",
        "in_progress",
        "--expected-by",
        "next checkpoint",
        "--json",
    ]);
    assert_eq!(
        updated["data"]["backlog"]["items"][0]["status"],
        "in_progress"
    );
    assert_eq!(
        updated["data"]["backlog"]["items"][0]["target"], "codex",
        "status updates must preserve the assigned owner"
    );
    assert_eq!(
        updated["data"]["backlog"]["items"][0]["expected_by"],
        "next checkpoint"
    );

    let after_update = workspace.json(&["next", "--json", "--tool", "codex", "--limit", "3"]);
    assert_ne!(
        after_update["data"]["next"]["action"], "update_plan_status",
        "in_progress status should clear the immediate status-update obligation"
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
    let do_items = enter["data"]["enter"]["entry"]["do"].as_array().unwrap();
    let respond_to = enter["data"]["enter"]["entry"]["respond_to"]
        .as_array()
        .unwrap();
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
    let _run_guard = serialize_rally_run();
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
    assert_eq!(run["data"]["run"]["session"]["name"], "reviewer-01");
    assert_eq!(
        run["data"]["run"]["session"]["session_id"],
        "claude-reviewer-01"
    );
    assert_eq!(run["data"]["run"]["session"]["agent"], "claude");
    assert_eq!(
        run["data"]["run"]["session"]["tool"],
        "claude_code:reviewer-01"
    );
    assert_eq!(run["data"]["run"]["session"]["backend"], "tmux");
    assert_eq!(
        run["data"]["run"]["session"]["target"],
        "rally-claude-reviewer-01"
    );
    assert!(
        run["data"]["run"]["commands"]["start"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|command| command.as_array().into_iter().flatten())
            .any(|arg| arg.as_str().unwrap().contains("claude"))
    );

    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(sessions["schema"], "agent-rally.command.sessions.v1");
    assert_matches_schema("agent-rally.command.sessions.v1.json", &sessions);
    assert_eq!(
        sessions["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        sessions["data"]["sessions"]["sessions"][0]["name"],
        "reviewer-01"
    );
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
    assert_eq!(inject["data"]["inject"]["session"]["name"], "reviewer-01");
    // tmux inject is now TWO commands: a C-u clear, then a SINGLE atomic
    // bracketed-paste-framed `send-keys -H <hex…>` write whose trailing CR
    // submits (ptyd frame_line port — replaces the old 4-command set-buffer /
    // paste-buffer / separate-Enter sequence that never submitted in Codex).
    let cmds = inject["data"]["inject"]["commands"].as_array().unwrap();
    assert_eq!(cmds.len(), 2, "framed inject is clear + one atomic write");
    let second: Vec<&str> = cmds[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(second.contains(&"-H"), "framed write uses hex send-keys");
    // The decoded hex frame must end in a submit CR (0x0d) right after the
    // bracketed-paste close marker (`~` == 0x7e).
    let hex_start = second.iter().position(|a| *a == "-H").unwrap() + 1;
    let frame: Vec<u8> = second[hex_start..]
        .iter()
        .map(|t| u8::from_str_radix(t, 16).unwrap())
        .collect();
    assert_eq!(
        *frame.last().unwrap(),
        0x0d,
        "frame submits with trailing CR"
    );
    assert_eq!(
        frame[frame.len() - 2],
        0x7e,
        "CR sits after the close marker"
    );
    // No legacy paste-buffer / separate Enter survives anywhere in the plan.
    for c in cmds {
        let toks: Vec<&str> = c
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert!(!toks.contains(&"paste-buffer") && !toks.contains(&"Enter"));
    }

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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();
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
    assert_eq!(acked["data"]["inject"]["ack"]["resolved"], true);
    assert_eq!(acked["data"]["inject"]["ack"]["received"], true);
    assert_eq!(acked["data"]["inject"]["ack_state"], "acked");
    assert_eq!(acked["data"]["inject"]["verified_received"], true);
    assert_eq!(
        acked["data"]["inject"]["ack"]["tool"],
        "claude_code:reviewer-01"
    );
    assert_eq!(
        acked["data"]["inject"]["ack"]["subject"],
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
    assert_eq!(capture["data"]["capture"]["action"], "capture");
    assert_eq!(capture["data"]["capture"]["output"], "");
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
    assert_eq!(attach["data"]["attach"]["commands"][0][1], "attach");

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
    assert_eq!(dry_run["data"]["run"]["mode"], "dry-run");
    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(
        sessions["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let stop = workspace.json(&[
        "stop",
        "reviewer-01",
        "--json",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(stop["schema"], "agent-rally.command.session-action.v1");
    assert_matches_schema("agent-rally.command.session-action.v1.json", &stop);
    assert_eq!(stop["data"]["stop"]["action"], "stop");
    let sessions = workspace.json(&["sessions", "--json"]);
    assert_eq!(
        sessions["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(!workspace.cwd.join(".rally/sessions.json").exists());

    workspace.cleanup();
}

#[test]
fn rally_stop_tombstones_session_when_backend_stop_fails() {
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-stop-stale-target");

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "stoppable",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    let session_id = run["data"]["run"]["session"]["session_id"]
        .as_str()
        .unwrap();

    let stop = workspace.json(&["stop", session_id, "--json", "--tmux-bin", "/usr/bin/false"]);
    assert_eq!(stop["schema"], "agent-rally.command.session-action.v1");
    assert_eq!(stop["data"]["stop"]["session"]["session_id"], session_id);

    let sessions = workspace.json(&["sessions", "--json", "--tmux-bin", "/usr/bin/true"]);
    assert_eq!(
        sessions["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "explicit stop must tombstone the session even when backend stop fails"
    );

    workspace.cleanup();
}

#[test]
fn stale_managed_session_projects_reaps_and_blocks_inject() {
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-stale-session-projection");
    let fake_tmux = workspace.cwd.join("fake-tmux-stale.sh");
    write_executable(
        &fake_tmux,
        r#"#!/bin/sh
case "$1" in
  new-session) exit 0 ;;
  list-panes) echo "no server running on /tmp/rally-test" >&2; exit 1 ;;
  kill-session) echo "can't find session: $3" >&2; exit 1 ;;
  *) exit 0 ;;
esac
"#,
    );
    let fake_tmux = fake_tmux.to_string_lossy().to_string();

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "stale",
        "--backend",
        "tmux",
        "--tmux-bin",
        &fake_tmux,
    ]);
    let session_id = run["data"]["run"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let name = run["data"]["run"]["session"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    let sessions = workspace.json(&["sessions", "--json", "--tmux-bin", &fake_tmux]);
    let rows = sessions["data"]["sessions"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"], session_id);
    assert_eq!(rows[0]["liveness"], "stale");
    assert_eq!(rows[0]["liveness_source"], "backend_probe");

    let inject = workspace.output(&[
        "inject",
        &name,
        "--json",
        "--text",
        "must not route to stale session",
        "--tool",
        "claude_code:test-sender",
        "--tmux-bin",
        &fake_tmux,
    ]);
    assert!(!inject.status.success());
    let stderr = String::from_utf8_lossy(&inject.stderr);
    assert!(
        stderr.contains("stale managed session"),
        "inject must fail loud for stale managed sessions; stderr={stderr}"
    );

    let reaped = workspace.json(&["sessions", "--reap", "--json", "--tmux-bin", &fake_tmux]);
    assert_eq!(
        reaped["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "--reap must tombstone stale sessions"
    );
    let reaped_again = workspace.json(&["sessions", "--reap", "--json", "--tmux-bin", &fake_tmux]);
    assert_eq!(
        reaped_again["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "--reap must be idempotent"
    );

    workspace.cleanup();
}

#[test]
fn gone_managed_session_blocks_inject_instead_of_silent_ledger_fallback() {
    // Regression for the live-caught silent-degradation bug (2026-06-27): an
    // inject naming a managed session that is gone/renumbered/reaped must FAIL
    // LOUDLY, not silently degrade to a ledger-only write. The session name is a
    // syntactically valid agent-id, so without the `prior_managed_session` gate
    // it would resolve to `LedgerAgent` and the message would vanish into an
    // inbox the sender never intended.
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-gone-session-inject");
    let fake_tmux = workspace.cwd.join("fake-tmux-gone.sh");
    write_executable(
        &fake_tmux,
        r#"#!/bin/sh
case "$1" in
  new-session) exit 0 ;;
  list-panes) echo "no server running on /tmp/rally-test" >&2; exit 1 ;;
  kill-session) echo "can't find session: $3" >&2; exit 1 ;;
  *) exit 0 ;;
esac
"#,
    );
    let fake_tmux = fake_tmux.to_string_lossy().to_string();

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "ghost",
        "--backend",
        "tmux",
        "--tmux-bin",
        &fake_tmux,
    ]);
    let name = run["data"]["run"]["session"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    // Reap tombstones the stale session — it is now fully gone from active
    // session state (the historical `session` facts remain in the ledger).
    let reaped = workspace.json(&["sessions", "--reap", "--json", "--tmux-bin", &fake_tmux]);
    assert_eq!(
        reaped["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "--reap must tombstone the stale session before the inject"
    );

    let inject = workspace.output(&[
        "inject",
        &name,
        "--json",
        "--text",
        "must not vanish into a ledger inbox",
        "--tool",
        "claude_code:test-sender",
        "--tmux-bin",
        &fake_tmux,
    ]);
    assert!(
        !inject.status.success(),
        "inject to a gone managed session must fail; stdout={} stderr={}",
        String::from_utf8_lossy(&inject.stdout),
        String::from_utf8_lossy(&inject.stderr),
    );
    let stderr = String::from_utf8_lossy(&inject.stderr);
    assert!(
        stderr.contains("no longer active"),
        "inject must fail loud for a gone/renumbered managed session (not silent ledger fallback); stderr={stderr}"
    );

    workspace.cleanup();
}

#[test]
fn rally_inject_require_ack_timeout_returns_ok_with_timeout_ack() {
    // Verifies that when --require-ack times out (no resolver writes a resolve
    // fact), the command exits 0 with ok:true and a structured timeout ack,
    // NOT an error envelope.  The message was durably recorded (content_fact
    // present) before the ack wait began — the caller must NOT re-inject.
    let workspace = Workspace::new("rally-inject-ack-timeout");

    workspace.json(&[
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

    let handoff = workspace.json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--target",
        "claude_code:reviewer-01",
        "--subject",
        "handoff for ack-timeout test",
    ]);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    // Inject with --require-ack but NO resolver thread — must time out.
    let (body, output) = workspace.json_with_status(&[
        "inject",
        "reviewer-01",
        "--json",
        "--handoff",
        handoff_id,
        "--require-ack",
        "--timeout-seconds",
        "1",
        "--tmux-bin",
        "/usr/bin/true",
    ]);

    // Must be ok:true (inject succeeded; only ack is pending).
    assert_eq!(output.status.code(), Some(0), "ack-timeout must exit 0");
    assert_eq!(body["ok"], true, "ack-timeout must return ok:true");
    assert_eq!(
        body["data"]["inject"]["ack_state"], "timeout",
        "ack_state must surface timeout"
    );
    assert_eq!(
        body["data"]["inject"]["verified_received"], false,
        "target receipt must be false until target-authored Rally evidence appears"
    );

    // delivery + content fact must be present (message was recorded before wait).
    assert_eq!(
        body["data"]["inject"]["delivered"], true,
        "delivered must be true even on ack-timeout"
    );
    // --handoff inject: content_fact is None (handoff fact already in channel).
    // That's expected — just confirm the field exists (it's null/absent for --handoff).

    // ack must be the structured timeout object.
    let ack = &body["data"]["inject"]["ack"];
    assert_eq!(
        ack["resolved"], false,
        "ack.resolved must be false on timeout"
    );
    assert_eq!(
        ack["received"], false,
        "ack.received must be false on timeout"
    );
    assert_eq!(
        ack["assume_received"], false,
        "timeout means assume the target did not receive/read the injection"
    );
    assert_eq!(ack["timed_out"], true, "ack.timed_out must be true");
    assert!(
        ack["waited_seconds"].as_u64().unwrap_or(0) >= 1,
        "ack.waited_seconds must reflect the timeout duration"
    );
    assert!(!ack["after_seq"].is_null(), "ack.after_seq must be present");
    assert_eq!(
        body["data"]["inject"]["fallback_plan"]["assumption"], "not_received",
        "timeout must return a fallback plan that treats missing ACK as not received"
    );

    workspace.cleanup();
}

#[test]
fn rally_run_assigns_numbered_agent_ids() {
    let _run_guard = serialize_rally_run();
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
    assert_eq!(first_claude["data"]["run"]["session"]["name"], "claude-01");
    assert_eq!(
        first_claude["data"]["run"]["session"]["session_id"],
        "claude-01"
    );
    assert_eq!(
        first_claude["data"]["run"]["session"]["tool"],
        "claude_code:01"
    );
    assert_eq!(
        first_claude["data"]["run"]["session"]["target"],
        "rally-claude-01"
    );

    let second_claude = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(second_claude["data"]["run"]["session"]["name"], "claude-02");
    assert_eq!(
        second_claude["data"]["run"]["session"]["session_id"],
        "claude-02"
    );
    assert_eq!(
        second_claude["data"]["run"]["session"]["tool"],
        "claude_code:02"
    );

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
    assert_eq!(reviewer["data"]["run"]["session"]["name"], "reviewer-01");
    assert_eq!(
        reviewer["data"]["run"]["session"]["session_id"],
        "claude-reviewer-01"
    );
    assert_eq!(
        reviewer["data"]["run"]["session"]["tool"],
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
    assert_eq!(
        second_reviewer["data"]["run"]["session"]["name"],
        "reviewer-02"
    );
    assert_eq!(
        second_reviewer["data"]["run"]["session"]["tool"],
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
    assert_eq!(first_codex["data"]["run"]["session"]["name"], "codex-01");
    assert_eq!(
        first_codex["data"]["run"]["session"]["session_id"],
        "codex-01"
    );
    assert_eq!(first_codex["data"]["run"]["session"]["tool"], "codex:01");

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
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-run-parallel-numbered-ids");
    // Scale concurrency to the host. The reservation is CAS-atomic (uniqueness
    // holds at any N — that is what this test asserts), so the only thing a
    // fixed high N buys is over-subscription on constrained CI runners (24
    // processes on 2 cores => spurious watchdog timeouts). Scale to the machine:
    // full stress locally, still-meaningful concurrency on a small runner.
    let n: usize = std::thread::available_parallelism()
        .map(|p| (p.get() * 4).clamp(8, 24))
        .unwrap_or(8);
    let handles = (0..n)
        .map(|_| {
            let cwd = workspace.cwd.clone();
            let home = workspace.home.clone();
            thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_rally"))
                    .current_dir(cwd)
                    .env("HOME", home)
                    .env("RALLY_NO_WORKTREE", "1")
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

    let sessions_resp = workspace.json(&["sessions", "--json"]);
    let sessions = sessions_resp["data"]["sessions"]["sessions"]
        .as_array()
        .unwrap();
    assert_eq!(sessions.len(), n);
    let names = sessions
        .iter()
        .map(|session| session["name"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let tools = sessions
        .iter()
        .map(|session| session["tool"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    // The correctness property: N concurrent launches yield N DISTINCT ids.
    assert_eq!(names.len(), n);
    assert_eq!(tools.len(), n);
    assert!(names.contains("claude-01"));
    assert!(names.contains(&format!("claude-{n:02}")));
    assert!(tools.contains("claude_code:01"));
    assert!(tools.contains(&format!("claude_code:{n:02}")));

    workspace.cleanup();
}

#[test]
fn rally_run_removes_session_reservation_when_backend_start_fails() {
    let _run_guard = serialize_rally_run();
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
    assert_eq!(
        sessions["data"]["sessions"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let next = workspace.json(&["next", "--json", "--tool", "codex"]);
    assert_matches_schema("agent-rally.command.next.v1.json", &next);
    assert_eq!(next["data"]["next"]["action"], "respond_to_handoff");
    assert_eq!(next["data"]["wake_intent"]["kind"], "wake");
    assert_eq!(next["data"]["wake_intent"]["target"], "codex");
    assert_eq!(next["data"]["wake_intent"]["ref"], handoff_id);
    assert_eq!(next["data"]["wake_intent"]["status"], "pending");
    let next_wake_id = next["data"]["wake_intent"]["event_id"].as_str().unwrap();
    let located_next_wake = workspace.json(&["locate", next_wake_id, "--json"]);
    assert_eq!(
        located_next_wake["data"]["locate"]["located"]["source"],
        "room"
    );

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
    assert_eq!(
        run["data"]["run"]["session"]["tool"],
        "claude_code:reviewer-01"
    );
    let inject = workspace.json(&[
        "inject",
        "reviewer-01",
        "--json",
        "--handoff",
        handoff_id,
        "--timeout-seconds",
        "1",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_matches_schema("agent-rally.command.inject.v1.json", &inject);
    assert_eq!(inject["data"]["inject"]["wake_intent"]["kind"], "wake");
    assert_eq!(
        inject["data"]["inject"]["wake_intent"]["target"],
        "claude_code:reviewer-01"
    );
    assert_eq!(inject["data"]["inject"]["wake_intent"]["ref"], handoff_id);
    assert_eq!(
        inject["data"]["inject"]["wake_intent"]["status"],
        "delivered"
    );
    assert_eq!(
        inject["data"]["inject"]["require_ack"], true,
        "--handoff injects require target ACK by default"
    );
    assert_eq!(
        inject["data"]["inject"]["ack_state"], "timeout",
        "without a target-authored Rally response, inject must time out"
    );
    assert_eq!(
        inject["data"]["inject"]["verified_received"], false,
        "no target ACK means assume the injected prompt was not received"
    );
    assert_eq!(
        inject["data"]["inject"]["ack"]["received"], false,
        "timeout ACK object must explicitly mark received=false"
    );
    assert_eq!(
        inject["data"]["inject"]["fallback_plan"]["fallbacks"]
            .as_array()
            .unwrap()
            .len(),
        4,
        "timeout must return concrete fallback choices"
    );

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
    let decision_id = decision["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();
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
    let artifact_id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();

    // Cross-repo locate: repo_b can locate a fact written by repo_a via the
    // global rooms index.
    let located = repo_b.json(&["locate", decision_id, "--json"]);
    assert_matches_schema("agent-rally.command.locate.v1.json", &located);
    assert_eq!(located["data"]["locate"]["located"]["source"], "room");
    assert_eq!(
        located["data"]["locate"]["located"]["fact"]["event_id"].as_str(),
        Some(decision_id)
    );
    assert!(
        located["data"]["locate"]["located"]["repo_root"]
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
        not_found["data"]["locate"]["located"].is_null(),
        "legacy-only event must NOT be found by locate after flag retirement; got: {:?}",
        not_found["data"]["locate"]["located"]
    );

    // recent --all returns room-based rows (including artifact from repo_b);
    // the legacy-only record is not present.
    let recent = repo_b.json(&["recent", "--all", "--json", "--limit", "10"]);
    assert_matches_schema("agent-rally.command.recent.v1.json", &recent);
    assert!(
        recent["data"]["recent"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some(artifact_id)),
        "recent must include the room-based artifact fact"
    );
    assert!(
        !recent["data"]["recent"]["rows"]
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
    let artifact_id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();

    assert_eq!(fs::read_to_string(&index_path).unwrap(), "{not-json");

    let recent = workspace.json(&["recent", "--all", "--json", "--limit", "10"]);
    assert!(
        recent["data"]["recent"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "room_index_unreadable")
    );
    assert!(
        recent["data"]["recent"]["rows"]
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
        recent["data"]["recent"]["warnings"]
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
        suppress_worktree: true,
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

// Plan F functional core (Chunk 3): the herdr backend is removed; the
// `rally_uses_native_herdr_and_cmux_managed_session_commands` test split
// into two: (1) `rally_run_rejects_herdr_backend_with_clear_error` and
// (2) `rally_uses_native_cmux_managed_session_commands` (the cmux half
// is preserved verbatim — tmux + cmux are the only remaining backends).
#[test]
fn rally_run_rejects_herdr_backend_with_clear_error() {
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-herdr-removed");

    // `rally run --backend herdr` must now fail with a clear, actionable
    // error pointing at Plan F. The 34-caller audit
    // (tools/check_inject_callsites.sh) is unaffected because no rally
    // CALLER passes `--backend herdr` on the inject critical path
    // (audit-verified pre-removal).
    let output = workspace.output(&[
        "run",
        "claude",
        "--json",
        "--name",
        "herdr-removed",
        "--backend",
        "herdr",
    ]);
    assert!(
        !output.status.success(),
        "--backend herdr must fail; got success"
    );
    // The error envelope is JSON-on-stderr per rally's error contract.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("backend \\\"herdr\\\" is removed (Plan F)")
            && stderr.contains(".rally ledger"),
        "error must reference Plan F and the ledger; got: {stderr}"
    );

    workspace.cleanup();
}

#[test]
fn rally_uses_native_cmux_managed_session_commands() {
    let _run_guard = serialize_rally_run();
    let workspace = Workspace::new("rally-native-session-backends");

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
    assert_eq!(cmux["data"]["run"]["session"]["backend"], "cmux");
    assert_eq!(
        cmux["data"]["run"]["session"]["target"],
        "workspace:cmux-builder"
    );
    assert_eq!(
        cmux["data"]["run"]["commands"]["start"][0][1],
        "new-workspace"
    );
    assert!(
        !cmux["data"]["run"]["commands"]["start"][0]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--command")
    );
    assert!(
        cmux["data"]["run"]["commands"]["start"][0]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--layout")
    );
    let cmux_layout = cmux["data"]["run"]["commands"]["start"][0]
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
    assert_eq!(cmux_inject["data"]["inject"]["commands"][0][1], "send-key");
    assert_eq!(cmux_inject["data"]["inject"]["commands"][0][4], "ctrl+u");
    assert_eq!(cmux_inject["data"]["inject"]["commands"][1][1], "send");
    assert_eq!(
        cmux_inject["data"]["inject"]["commands"][1][4],
        "hello cmux"
    );
    assert_eq!(cmux_inject["data"]["inject"]["commands"][2][1], "send-key");
    assert_eq!(cmux_inject["data"]["inject"]["commands"][2][4], "enter");

    let cmux_stop = workspace.json(&["stop", "cmux-builder-01", "--json", "--cmux-bin", cmux_bin]);
    assert_eq!(
        cmux_stop["data"]["stop"]["commands"][0][1],
        "close-workspace"
    );

    // Plan F functional core (Chunk 3): herdr_stop assertion removed
    // (Backend::Herdr is gone; the corresponding rally_run_rejects_*
    // test above covers the negative path).

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
    let claim_id = claim["data"]["say"]["fact"]["event_id"].as_str().unwrap();
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
        handoff["data"]["say"]["fact"]["summary"], "Reviewer has enough context to proceed.",
        "space-separated --summary must round-trip into fact.summary"
    );
    assert_eq!(handoff["data"]["say"]["fact"]["subject"], "needs review");

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
        decision["data"]["say"]["fact"]["summary"], "Adopt the finite pickup protocol.",
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
        "say",
        "claim",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "pre-claim for release test",
        "--path",
        "src/lib.rs",
    ]);
    let pre_claim_id = pre_claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();

    // Write a blocker to resolve.
    let pre_blocker = workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "pre-blocker for resolve test",
        "--path",
        "src/lib.rs",
    ]);
    let pre_blocker_id = pre_blocker["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();

    // Kinds that don't need a ref.
    let simple_kinds = ["decision", "artifact", "handoff", "risk", "lesson"];
    for kind in simple_kinds {
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
        assert_eq!(
            fact["data"]["say"]["fact"]["kind"], kind,
            "kind mismatch for {kind}"
        );
        assert_eq!(fact["data"]["say"]["fact"]["schema"], "agent-rally.fact.v1");
        DateTime::parse_from_rfc3339(fact["data"]["say"]["fact"]["created_at"].as_str().unwrap())
            .unwrap();
        assert_matches_schema("agent-rally.fact.v1.json", &fact["data"]["say"]["fact"]);
    }

    // R9: release --ref <live-claim>.
    let release_fact = workspace.json(&[
        "say",
        "release",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "release",
        "--path",
        "src/lib.rs",
        "--ref",
        pre_claim_id,
    ]);
    assert_eq!(release_fact["data"]["say"]["fact"]["kind"], "release");
    assert_eq!(
        release_fact["data"]["say"]["fact"]["schema"],
        "agent-rally.fact.v1"
    );
    assert_matches_schema(
        "agent-rally.fact.v1.json",
        &release_fact["data"]["say"]["fact"],
    );
    // R9-readback: verified {room, seq} must be present in the response.
    assert!(
        release_fact["data"]["verified"]["seq"]
            .as_i64()
            .unwrap_or(0)
            > 0,
        "release must return verified.seq > 0"
    );
    assert!(
        !release_fact["data"]["verified"]["room"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "release must return verified.room"
    );

    // R9: resolve --ref <live-blocker>.
    let resolve_fact = workspace.json(&[
        "say",
        "resolve",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "resolve",
        "--path",
        "src/lib.rs",
        "--ref",
        pre_blocker_id,
    ]);
    assert_eq!(resolve_fact["data"]["say"]["fact"]["kind"], "resolve");
    assert_eq!(
        resolve_fact["data"]["say"]["fact"]["schema"],
        "agent-rally.fact.v1"
    );
    assert_matches_schema(
        "agent-rally.fact.v1.json",
        &resolve_fact["data"]["say"]["fact"],
    );

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
    let lead_context_a = &enter_a["data"]["lead_context"];
    assert_eq!(lead_context_a["current_lead"], "alpha");
    let lead_epoch = lead_context_a["lead_epoch"]
        .as_i64()
        .expect("enter should expose lead_epoch");
    assert!(lead_epoch > 0, "lead_epoch should be a ledger seq");
    assert_eq!(lead_context_a["self_role"], "lead");
    assert_eq!(lead_context_a["self_is_lead"], true);
    assert_eq!(lead_context_a["self_acknowledged"], false);
    assert_eq!(lead_context_a["current_lead_acknowledged"], false);
    // room_id must be a non-null, non-empty string (Component A requirement).
    let room_id = enter_a["data"]["enter"]["room_id"].as_str().unwrap();
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
    let lead_context_b = &enter_b["data"]["lead_context"];
    assert_eq!(lead_context_b["current_lead"], "alpha");
    assert_eq!(lead_context_b["lead_epoch"], lead_epoch);
    assert_eq!(lead_context_b["self_role"], "participant");
    assert_eq!(lead_context_b["self_is_lead"], false);
    assert_eq!(lead_context_b["self_acknowledged"], false);
    assert_eq!(lead_context_b["current_lead_acknowledged"], false);
    let room_id_b = enter_b["data"]["enter"]["room_id"].as_str().unwrap();
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
    let squads_replay = room_replay["data"]["room"]["squads"].as_array().unwrap();
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
    let rows = recent["data"]["recent"]["rows"].as_array().unwrap();
    assert!(
        rows.iter().any(|r| r["fact"]["kind"] == "presence"),
        "presence facts appear in recent rows"
    );

    workspace.cleanup();
}

#[test]
fn next_exposes_tool_scoped_lead_context() {
    let workspace = Workspace::new("rally-next-lead-context");

    let enter_a = workspace.json(&["enter", "--json", "--tool", "alpha"]);
    let lead_epoch = enter_a["data"]["lead_context"]["lead_epoch"]
        .as_i64()
        .expect("enter should expose lead_epoch");
    let ack = workspace.json(&["ack", "--json", "--tool", "alpha"]);
    assert_eq!(ack["data"]["ack"]["acknowledged"], true);

    let next_b = workspace.json(&["next", "--json", "--tool", "beta"]);
    let lead_context = &next_b["data"]["lead_context"];
    assert_eq!(lead_context["current_lead"], "alpha");
    assert_eq!(lead_context["lead_epoch"], lead_epoch);
    assert_eq!(lead_context["self_role"], "participant");
    assert_eq!(lead_context["self_is_lead"], false);
    assert_eq!(lead_context["self_acknowledged"], false);
    assert_eq!(lead_context["current_lead_acknowledged"], true);

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
    fs::write(
        unrelated_dir.join("changes.jsonl"),
        format!("{unrelated_fact}\n"),
    )
    .unwrap();

    // First migrate-legacy run.
    let first = workspace.json(&["migrate-legacy", "--json"]);
    assert!(
        first["ok"].as_bool().unwrap_or(false),
        "migrate-legacy must return ok:true on first run"
    );
    let migrated = first["data"]["migrate-legacy"]["facts_migrated"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(migrated, 1, "first run must migrate exactly 1 fact");
    assert_eq!(
        first["data"]["migrate-legacy"]["facts_skipped_existing"]
            .as_u64()
            .unwrap_or(99),
        0,
        "no facts should be skipped on first run"
    );

    // Verify the fact appears in recent. The migrated legacy fact carries an
    // OLD `created_at` (its original timestamp), so recency-decay archives it
    // out of the default `recent` view once it is past the archive floor
    // (~14d at the default 48h half-life). It is still losslessly retrievable
    // via `recent --include-archived` — the documented retrieval path.
    let recent = workspace.json(&["recent", "--include-archived", "--json"]);
    assert!(
        recent["data"]["recent"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some("evt_migrate_test_001")),
        "migrated fact must appear in recent --include-archived after first run"
    );

    // Unrelated fact must NOT appear (different slug).
    assert!(
        !recent["data"]["recent"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["fact"]["event_id"].as_str() == Some("evt_unrelated_999")),
        "unrelated-slug fact must NOT be migrated"
    );

    // Second migrate-legacy run: idempotent.
    let second = workspace.json(&["migrate-legacy", "--json"]);
    assert_eq!(
        second["data"]["migrate-legacy"]["facts_migrated"]
            .as_u64()
            .unwrap_or(99),
        0,
        "second run must migrate 0 facts (already in ledger)"
    );
    assert_eq!(
        second["data"]["migrate-legacy"]["facts_skipped_existing"]
            .as_u64()
            .unwrap_or(0),
        1,
        "second run must count 1 skipped-existing"
    );

    // Legacy file untouched (non-destructive migrator).
    assert!(
        legacy_file.exists(),
        "migrate-legacy must not delete the legacy file"
    );

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
    let claim_id = claim["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
        evidence.contains(&"produces:src/auth.rs"),
        "evidence must contain produces:src/auth.rs; got: {:?}",
        evidence
    );
    assert!(
        evidence.contains(&"produces:src/auth/token.rs"),
        "evidence must contain produces:src/auth/token.rs; got: {:?}",
        evidence
    );
    assert!(
        evidence.contains(&"depends:src/config.rs"),
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
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();

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
    let receipt_id = receipt["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    assert!(!receipt_id.is_empty(), "receipt must have a valid event_id");

    // The receipt's ref must point to the handoff.
    assert_eq!(
        receipt["data"]["say"]["fact"]["ref"].as_str(),
        Some(handoff_id),
        "receipt ref must equal the handoff event_id"
    );
    assert_eq!(
        receipt["data"]["say"]["fact"]["kind"].as_str(),
        Some("receipt"),
        "fact kind must be 'receipt'"
    );

    // Verify the handoff is now closed (removed from open_handoffs).
    let room_after = workspace.json(&["room", "--json"]);
    let open_handoffs = room_after["data"]["room"]["open_handoffs"]
        .as_array()
        .unwrap();
    assert!(
        !open_handoffs
            .iter()
            .any(|f| f["event_id"].as_str() == Some(handoff_id)),
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
    assert_eq!(result["data"]["check-ci"]["pass"], true);
    assert_eq!(
        result["data"]["check-ci"]["offenders"]
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
    let blocker_id = blocker["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let (result, output) = workspace.json_with_status(&["check-ci", "--json", "--strict"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "check-ci --strict must exit 4 when an unresolved blocker exists; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["data"]["check-ci"]["pass"], false);
    let offenders = result["data"]["check-ci"]["offenders"].as_array().unwrap();
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
    assert_eq!(result["data"]["check-ci"]["pass"], false);
    let offenders = result["data"]["check-ci"]["offenders"].as_array().unwrap();
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
    let offenders2 = result2["data"]["check-ci"]["offenders"].as_array().unwrap();
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
        "say",
        "claim",
        "--json",
        "--tool",
        "test-agent",
        "--subject",
        "working on mylib",
        "--path",
        "mylib.rs",
    ]);
    assert!(claim["ok"].as_bool().unwrap_or(false), "claim must succeed");
    let claim_id = claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Post artifact WITHOUT modifying mylib.rs (unchanged → ungrounded).
    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "test-agent",
        "--subject",
        "done with mylib",
        "--ref",
        &claim_id,
    ]);
    assert!(
        artifact["ok"].as_bool().unwrap_or(false),
        "artifact must succeed"
    );

    // Room must have a risk fact with subject containing "ungrounded-artifact"
    // and scope containing "grounded:false".
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ungrounded: Vec<_> = risks
        .iter()
        .filter(|r| {
            r["subject"]
                .as_str()
                .unwrap_or("")
                .contains("ungrounded-artifact")
        })
        .collect();
    assert!(
        !ungrounded.is_empty(),
        "expected ungrounded-artifact risk fact; risks: {:?}",
        risks
            .iter()
            .map(|r| r["subject"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );
    let scope = ungrounded[0]["scope"].as_array().unwrap();
    assert!(
        scope.iter().any(|s| s.as_str() == Some("grounded:false")),
        "risk scope must contain grounded:false; scope: {:?}",
        scope
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
        "say",
        "claim",
        "--json",
        "--tool",
        "test-agent",
        "--subject",
        "working on changed",
        "--path",
        "changed.rs",
    ]);
    let claim_id = claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Modify the file before posting the artifact.
    fs::write(&src, b"fn after_modification() {}").unwrap();

    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "test-agent",
        "--subject",
        "done with changed",
        "--ref",
        &claim_id,
    ]);
    assert!(
        artifact["ok"].as_bool().unwrap_or(false),
        "artifact must succeed"
    );

    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ungrounded: Vec<_> = risks
        .iter()
        .filter(|r| {
            r["subject"]
                .as_str()
                .unwrap_or("")
                .contains("ungrounded-artifact")
        })
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
        "say",
        "claim",
        "--json",
        "--tool",
        "peer-tool",
        "--subject",
        "peer owns consumer",
        "--path",
        "consumer/main.rs",
    ]);
    assert!(peer_claim["ok"].as_bool().unwrap_or(false));
    let peer_claim_id = peer_claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    // my-tool claims src/provider.rs
    let my_claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "my-tool",
        "--subject",
        "my-tool owns provider",
        "--path",
        "src/provider.rs",
    ]);
    assert!(my_claim["ok"].as_bool().unwrap_or(false));
    let my_claim_id = my_claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Modify src/provider.rs (so grounding sees it as changed).
    fs::write(
        &provider,
        b"pub fn shared_api() -> i32 { 1 } pub fn new_fn() {}",
    )
    .unwrap();

    // my-tool posts artifact closing its claim (ref → my_claim_id).
    let artifact = workspace.json(&[
        "say",
        "artifact",
        "--json",
        "--tool",
        "my-tool",
        "--subject",
        "provider updated",
        "--ref",
        &my_claim_id,
    ]);
    assert!(
        artifact["ok"].as_bool().unwrap_or(false),
        "artifact must succeed"
    );

    // Room must have a ripple-alert risk fact targeting peer-tool.
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"].as_array().unwrap();
    let ripple: Vec<_> = risks
        .iter()
        .filter(|r| r["subject"].as_str().unwrap_or("").contains("ripple-alert"))
        .collect();
    assert!(
        !ripple.is_empty(),
        "expected ripple-alert risk fact; risks: {:?}",
        risks
            .iter()
            .map(|r| r["subject"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ripple[0]["severity"].as_str().unwrap_or(""),
        "warn",
        "ripple-alert severity must be warn"
    );
    assert!(
        ripple[0]["subject"]
            .as_str()
            .unwrap_or("")
            .contains("peer-tool"),
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
        "check",
        "tier-fit",
        "--json",
        "--role",
        "executor",
        "--proposed-tier",
        "opus",
    ]);
    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "tier-fit must succeed (advisory)"
    );
    let status = result["data"]["check"]["tier_fit"]["status"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        status, "no_calibration",
        "must return no_calibration when no fact present"
    );
    workspace.cleanup();
}

/// With a calibration fact, mismatch emits a tier_mismatch finding.
#[test]
fn advisory_9_tier_fit_mismatch_emits_finding_vs_calibration() {
    let workspace = Workspace::new("advisory-9-mismatch");

    // Post a tier-calibration decision fact.
    workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "lead",
        "--subject",
        "tier-calibration",
        "--scope",
        "tier-calibration",
        "--summary",
        "role:executor=cheapest:sonnet",
    ]);

    let result = workspace.json(&[
        "check",
        "tier-fit",
        "--json",
        "--role",
        "executor",
        "--proposed-tier",
        "opus",
    ]);
    assert!(result["ok"].as_bool().unwrap_or(false));
    let status = result["data"]["check"]["tier_fit"]["status"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        status, "mismatch",
        "must be mismatch when proposed tier != calibrated cheapest"
    );
    let finding_code = result["data"]["check"]["tier_fit"]["finding"]["code"]
        .as_str()
        .unwrap_or("");
    assert_eq!(finding_code, "tier_mismatch");
    let finding_severity = result["data"]["check"]["tier_fit"]["finding"]["severity"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        finding_severity, "info",
        "tier mismatch is advisory info, not blocking"
    );

    workspace.cleanup();
}

/// With a matching tier, tier-fit returns ok.
#[test]
fn advisory_9_tier_fit_ok_when_matching_calibration() {
    let workspace = Workspace::new("advisory-9-ok");

    workspace.json(&[
        "say",
        "decision",
        "--json",
        "--tool",
        "lead",
        "--subject",
        "tier-calibration",
        "--scope",
        "tier-calibration",
        "--summary",
        "role:executor=cheapest:sonnet",
    ]);

    let result = workspace.json(&[
        "check",
        "tier-fit",
        "--json",
        "--role",
        "executor",
        "--proposed-tier",
        "sonnet",
    ]);
    assert!(result["ok"].as_bool().unwrap_or(false));
    let status = result["data"]["check"]["tier_fit"]["status"]
        .as_str()
        .unwrap_or("");
    assert_eq!(status, "ok");

    workspace.cleanup();
}

/// B-whoami smoke test: `rally whoami --json` exits 0 and returns repo_root + build_id.
///
/// Fields are nested under `data.whoami`, matching the JSON envelope contract.
#[test]
fn rally_whoami_json_exits_zero_and_returns_identity() {
    let workspace = Workspace::new("rally-whoami");

    let result = workspace.json(&["whoami", "--json"]);
    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "whoami must return ok:true; got: {result}"
    );

    let whoami = &result["data"]["whoami"];
    let repo_root = whoami["repo_root"].as_str().unwrap_or("");
    assert!(!repo_root.is_empty(), "repo_root must be non-empty");
    let repo_id = whoami["repo_id"].as_str().unwrap_or("");
    assert!(!repo_id.is_empty(), "repo_id must be non-empty");
    let room_id = whoami["room_id"].as_str().unwrap_or("");
    assert!(!room_id.is_empty(), "room_id must be non-empty");

    let build_id = whoami["build_id"].as_str().unwrap_or("");
    assert!(!build_id.is_empty(), "build_id must be non-empty");
    assert!(
        build_id.contains('+'),
        "build_id must be <version>+<hash>; got: {build_id}"
    );

    // cwd and worktree must also be present and non-empty.
    let cwd = whoami["cwd"].as_str().unwrap_or("");
    assert!(!cwd.is_empty(), "cwd must be non-empty");
    let worktree = whoami["worktree"].as_str().unwrap_or("");
    assert!(!worktree.is_empty(), "worktree must be non-empty");

    workspace.cleanup();
}

/// `repo_id` is the stable repo identity, not the active engagement label.
#[test]
fn rally_whoami_repo_id_uses_manifest_not_active_engagement() {
    let workspace = Workspace::new("rally-whoami-repo-id");
    let rally_dir = workspace.cwd.join(".rally");
    fs::create_dir_all(&rally_dir).unwrap();
    fs::write(
        rally_dir.join("manifest.json"),
        r#"{"schema":"agent-rally.manifest.v1","repo":"agent-rally-point"}"#,
    )
    .unwrap();
    // Use a non-reserved engagement label: `test` is a reserved fixture
    // engagement that live sessions intentionally redirect away from (so
    // production facts never leak into the committed test.jsonl segment).
    // This test's actual subject is repo_id-from-manifest, so any normal
    // engagement label serves the scaffolding.
    fs::write(rally_dir.join("active-engagement"), "sprint-9\n").unwrap();

    let result = workspace.json(&["whoami", "--json"]);
    assert!(result["ok"].as_bool().unwrap_or(false));
    let whoami = &result["data"]["whoami"];
    assert_eq!(
        whoami["repo_id"].as_str().unwrap_or(""),
        "agent-rally-point"
    );
    assert_eq!(whoami["room_id"].as_str().unwrap_or(""), "sprint-9");

    workspace.cleanup();
}

/// B-whoami: --tool flag is echoed back in the output.
#[test]
fn rally_whoami_with_tool_reflects_tool_in_output() {
    let workspace = Workspace::new("rally-whoami-tool");

    let enter = workspace.json(&["enter", "--json", "--tool", "claude_code:01"]);
    let lead_epoch = enter["data"]["lead_context"]["lead_epoch"]
        .as_i64()
        .expect("enter should expose lead_epoch");
    let ack = workspace.json(&["ack", "--json", "--tool", "claude_code:01"]);
    assert_eq!(ack["data"]["ack"]["acknowledged"], true);

    let result = workspace.json(&["whoami", "--json", "--tool", "claude_code:01"]);
    assert!(result["ok"].as_bool().unwrap_or(false));

    let whoami = &result["data"]["whoami"];
    let tool = whoami["tool"].as_str().unwrap_or("");
    assert_eq!(tool, "claude_code:01", "tool must be echoed back");
    assert_eq!(whoami["acknowledged"], true);
    let lead_context = &whoami["lead_context"];
    assert_eq!(lead_context["current_lead"], "claude_code:01");
    assert_eq!(lead_context["lead_epoch"], lead_epoch);
    assert_eq!(lead_context["self_role"], "lead");
    assert_eq!(lead_context["self_is_lead"], true);
    assert_eq!(lead_context["self_acknowledged"], true);
    assert_eq!(lead_context["current_lead_acknowledged"], true);

    workspace.cleanup();
}

#[test]
fn rally_owners_dirty_maps_dirty_path_to_claim_session() {
    if !git_available() {
        return;
    }
    let workspace = real_repo_workspace("rally-owners-dirty");
    let src_dir = workspace.cwd.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "pub fn value() -> i32 { 1 }\n").unwrap();
    let add = Command::new("git")
        .arg("-C")
        .arg(&workspace.cwd)
        .args(["add", "src/lib.rs"])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let commit = Command::new("git")
        .arg("-C")
        .arg(&workspace.cwd)
        .args(["commit", "-q", "-m", "add lib"])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    let claim = workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex:worker-01",
        "--subject",
        "work on lib",
        "--path",
        "src/lib.rs",
    ]);
    let claim_id = claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    let from_session_id = claim["data"]["say"]["fact"]["from_session_id"]
        .as_str()
        .expect("claim must be stamped with authoring session id")
        .to_string();

    fs::write(src_dir.join("lib.rs"), "pub fn value() -> i32 { 2 }\n").unwrap();

    let owners = workspace.json(&["owners", "--dirty", "--json"]);
    assert_eq!(owners["schema"], "agent-rally.command.owners.v1");
    let dirty = owners["data"]["owners"]["dirty"].as_array().unwrap();
    let row = dirty
        .iter()
        .find(|row| row["path"] == "src/lib.rs" && row["claim_id"] == claim_id)
        .unwrap_or_else(|| panic!("dirty owner row missing; body={owners:#}"));
    assert_eq!(row["owner_tool"], "codex:worker-01");
    assert_eq!(row["from_session_id"], from_session_id);
    assert_eq!(row["owner_status"], "active");
    assert_eq!(row["lease_expired"], false);
    assert_eq!(row["is_owner_live"], true);

    workspace.cleanup();
}

// =============================================================================
// Rank-11: rally mission — room north-star + per-agent autonomy envelope
// =============================================================================

/// (a) `mission --set` then `mission` (GET) returns the text + set_by.
#[test]
fn mission_set_then_get_returns_text_and_set_by() {
    let workspace = Workspace::new("rally-mission-set-get");

    // SET
    let set_result = workspace.json(&[
        "mission",
        "--json",
        "--set",
        "ship the MVP by end of sprint",
        "--tool",
        "claude_code:01",
    ]);
    assert_eq!(set_result["ok"], true);
    assert_eq!(set_result["data"]["mission"]["action"], "set-mission");
    let seq = set_result["data"]["mission"]["fact"]["seq"]
        .as_i64()
        .unwrap_or(0);
    assert!(seq > 0, "seq must be > 0 after set");

    // GET
    let get_result = workspace.json(&["mission", "--json"]);
    assert_eq!(get_result["ok"], true);
    assert_eq!(
        get_result["data"]["mission"]["text"],
        "ship the MVP by end of sprint"
    );
    assert_eq!(get_result["data"]["mission"]["set_by"], "claude_code:01");
    assert!(
        get_result["data"]["mission"]["set_at"].is_string(),
        "set_at must be a string timestamp"
    );

    workspace.cleanup();
}

/// (b) A second `--set` supersedes the first (latest-by-seq wins).
#[test]
fn mission_second_set_supersedes_first() {
    let workspace = Workspace::new("rally-mission-supersede");

    workspace.json(&[
        "mission",
        "--json",
        "--set",
        "old mission",
        "--tool",
        "lead:01",
    ]);
    workspace.json(&[
        "mission",
        "--json",
        "--set",
        "new mission",
        "--tool",
        "lead:01",
    ]);

    let get_result = workspace.json(&["mission", "--json"]);
    assert_eq!(get_result["ok"], true);
    assert_eq!(
        get_result["data"]["mission"]["text"], "new mission",
        "second set must supersede first"
    );

    workspace.cleanup();
}

/// (c) The mission appears in `enter --json` output after being set.
#[test]
fn mission_appears_in_enter_json_after_set() {
    let workspace = Workspace::new("rally-mission-enter");

    // Set the mission first.
    workspace.json(&[
        "mission",
        "--json",
        "--set",
        "focus on stability",
        "--tool",
        "lead:01",
    ]);

    // Enter and check mission field.
    let enter = workspace.json(&["enter", "--json", "--tool", "claude_code:01"]);
    assert_eq!(enter["ok"], true);
    assert_eq!(
        enter["data"]["enter"]["mission"], "focus on stability",
        "mission must appear in enter output after being set"
    );

    workspace.cleanup();
}

/// Enter output has no `mission` key when none has been set (skip_serializing_if).
#[test]
fn mission_absent_from_enter_when_unset() {
    let workspace = Workspace::new("rally-mission-absent-enter");

    let enter = workspace.json(&["enter", "--json", "--tool", "tool-x"]);
    assert_eq!(enter["ok"], true);
    assert!(
        enter["data"]["enter"]["mission"].is_null(),
        "mission must not appear in enter output when unset; got: {}",
        enter["data"]["enter"]["mission"]
    );

    workspace.cleanup();
}

/// (d) Envelope set→get round-trips for a named agent.
#[test]
fn mission_envelope_set_then_get_round_trips() {
    let workspace = Workspace::new("rally-mission-envelope");

    // SET ENVELOPE
    let env_result = workspace.json(&[
        "mission",
        "--json",
        "--tool",
        "codex:01",
        "--may",
        "refactor within claimed files",
        "--must-check",
        "before touching shared interfaces",
    ]);
    assert_eq!(env_result["ok"], true);
    assert_eq!(env_result["data"]["mission"]["action"], "set-envelope");

    // GET: envelope must appear in envelopes array.
    let get_result = workspace.json(&["mission", "--json"]);
    assert_eq!(get_result["ok"], true);

    let envelopes = get_result["data"]["mission"]["envelopes"]
        .as_array()
        .expect("envelopes must be an array");
    let entry = envelopes
        .iter()
        .find(|e| e["agent"] == "codex:01")
        .expect("codex:01 envelope must be present");

    assert_eq!(entry["may"], "refactor within claimed files");
    assert_eq!(entry["must_check"], "before touching shared interfaces");
    assert_eq!(entry["set_by"], "codex:01");

    workspace.cleanup();
}

/// A second envelope set for the same agent supersedes (latest-by-seq wins).
#[test]
fn mission_envelope_second_set_supersedes() {
    let workspace = Workspace::new("rally-mission-envelope-supersede");

    workspace.json(&[
        "mission",
        "--json",
        "--tool",
        "codex:01",
        "--may",
        "old autonomy",
    ]);
    workspace.json(&[
        "mission",
        "--json",
        "--tool",
        "codex:01",
        "--may",
        "new autonomy",
    ]);

    let get_result = workspace.json(&["mission", "--json"]);
    let envelopes = get_result["data"]["mission"]["envelopes"]
        .as_array()
        .expect("envelopes must be an array");
    let entry = envelopes
        .iter()
        .find(|e| e["agent"] == "codex:01")
        .expect("codex:01 envelope must be present");

    assert_eq!(
        entry["may"], "new autonomy",
        "second envelope set must supersede first"
    );

    workspace.cleanup();
}

/// (e) Mission fact survives ledger replay: set, re-open store, confirm mission reads back.
#[test]
fn mission_fact_survives_ledger_replay() {
    let workspace = Workspace::new("rally-mission-replay");

    workspace.json(&[
        "mission",
        "--json",
        "--set",
        "survive the replay",
        "--tool",
        "lead",
    ]);

    // Re-read via room (fresh store open) — confirms segment→db replay keeps the fact.
    let room = workspace.json(&["room", "--json"]);
    assert_eq!(room["ok"], true);
    assert_eq!(
        room["data"]["mission"], "survive the replay",
        "mission must survive ledger replay and appear in room output"
    );

    // Also verify directly via mission GET (another fresh open).
    let get = workspace.json(&["mission", "--json"]);
    assert_eq!(
        get["data"]["mission"]["text"], "survive the replay",
        "mission must survive ledger replay and appear in mission GET"
    );

    workspace.cleanup();
}

/// GET when no mission is set returns null mission and empty envelopes.
#[test]
fn mission_get_with_no_mission_set() {
    let workspace = Workspace::new("rally-mission-empty");

    let result = workspace.json(&["mission", "--json"]);
    assert_eq!(result["ok"], true);
    assert!(
        result["data"]["mission"]["text"].is_null(),
        "mission text must be null when unset"
    );
    assert_eq!(
        result["data"]["mission"]["envelopes"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0,
        "envelopes must be empty when no envelopes have been set"
    );

    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// R12: shared-branch / worktree hazard detector integration tests
// ---------------------------------------------------------------------------

/// R12a: a second tool entering a canonical checkout that is on a non-main
/// branch while the first tool is active receives a `shared-branch-hazard`
/// warning and a durable risk fact is written to the room.
///
/// Setup:
///   - Tool-A enters first (establishes presence, no hazard — solo).
///   - The checkout's HEAD is set to a non-main branch.
///   - Tool-B enters: now there is one active peer (tool-A), canonical clone,
///     non-main branch — hazard must fire for tool-B.
///
/// Verification: `rally room --json` surfaces a `current_risks` entry whose
/// subject contains "shared-branch-hazard".
#[test]
fn r12a_shared_branch_hazard_fires_for_second_tool_on_non_main_branch() {
    let workspace = Workspace::new("r12a-shared-branch-hazard");

    // Simulate a non-main branch by writing the HEAD file.
    fs::write(
        workspace.cwd.join(".git").join("HEAD"),
        "ref: refs/heads/feat/dangerous-shared-branch\n",
    )
    .unwrap();

    // Tool-A enters first: canonical clone, non-main branch, BUT no peers yet
    // (active_peer_count == 0) — hazard must NOT fire for tool-A.
    let enter_a = workspace.json(&["enter", "--tool", "tool-a:01", "--json"]);
    assert_eq!(enter_a["ok"], true, "tool-a enter must succeed");
    let warnings_a = enter_a["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let has_hazard_a = warnings_a
        .iter()
        .any(|w| w["code"].as_str() == Some("shared-branch-hazard"));
    assert!(
        !has_hazard_a,
        "solo tool-A must NOT produce shared-branch-hazard warning; got: {:?}",
        warnings_a
    );

    // Tool-B enters: tool-A is active -> active_peer_count == 1 -> hazard fires.
    let enter_b = workspace.json(&["enter", "--tool", "tool-b:01", "--json"]);
    assert_eq!(
        enter_b["ok"], true,
        "tool-b enter must succeed (warn, not block)"
    );
    let warnings_b = enter_b["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let has_hazard_b = warnings_b
        .iter()
        .any(|w| w["code"].as_str() == Some("shared-branch-hazard"));
    assert!(
        has_hazard_b,
        "tool-B must get shared-branch-hazard warning; got warnings: {:?}",
        warnings_b
    );

    // Verify a durable risk fact was recorded (not just a transient warning).
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hazard_risk = risks.iter().find(|r| {
        r["subject"]
            .as_str()
            .map(|s| s.contains("shared-branch-hazard"))
            .unwrap_or(false)
    });
    assert!(
        hazard_risk.is_some(),
        "a durable shared-branch-hazard risk fact must appear in current_risks; got: {:?}",
        risks.iter().map(|r| &r["subject"]).collect::<Vec<_>>()
    );
    let risk = hazard_risk.unwrap();
    assert_eq!(
        risk["severity"].as_str(),
        Some("warn"),
        "shared-branch-hazard risk must have severity=warn"
    );

    workspace.cleanup();
}

/// R12b: entering the same canonical checkout when HEAD is on `main` must NOT
/// produce a shared-branch-hazard warning or risk fact, even with an active peer.
#[test]
fn r12b_no_hazard_on_main_branch() {
    let workspace = Workspace::new("r12b-no-hazard-main");

    // Explicitly set HEAD to main.
    fs::write(
        workspace.cwd.join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();

    // Tool-A enters first.
    let enter_a = workspace.json(&["enter", "--tool", "tool-a:01", "--json"]);
    assert_eq!(enter_a["ok"], true);

    // Tool-B enters with a peer active -- branch is main -> no hazard.
    let enter_b = workspace.json(&["enter", "--tool", "tool-b:01", "--json"]);
    assert_eq!(enter_b["ok"], true, "enter must succeed");
    let warnings_b = enter_b["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let has_hazard = warnings_b
        .iter()
        .any(|w| w["code"].as_str() == Some("shared-branch-hazard"));
    assert!(
        !has_hazard,
        "must NOT produce shared-branch-hazard when on main; warnings: {:?}",
        warnings_b
    );

    // Verify no risk fact recorded.
    let room = workspace.json(&["room", "--json"]);
    let risks = room["data"]["room"]["current_risks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let hazard_risk = risks.iter().any(|r| {
        r["subject"]
            .as_str()
            .map(|s| s.contains("shared-branch-hazard"))
            .unwrap_or(false)
    });
    assert!(
        !hazard_risk,
        "no shared-branch-hazard risk fact must appear when on main"
    );

    workspace.cleanup();
}

// C-FLEET / Plan F adoption integration tests.

/// C-FLEET: `rally enter --tool <managed-style>` against a room with no
/// active managed-session for that tool surfaces the `unmanaged-agent`
/// warning and appends a durable risk fact visible via `rally room --json`.
#[test]
fn rally_enter_emits_unmanaged_agent_for_presence_only_tool() {
    let workspace = Workspace::new("rally-fleet-unmanaged");
    let enter = workspace.json(&["enter", "--tool", "claude_code:99", "--json"]);
    let warnings = enter["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let has_unmanaged = warnings
        .iter()
        .any(|w| w["code"] == "unmanaged-agent" && w["message"].as_str().is_some());
    assert!(
        has_unmanaged,
        "expected unmanaged-agent warning; got warnings: {warnings:?}"
    );

    let room = workspace.json(&["room", "--json"]);
    // DI-1: unmanaged-agent telemetry projects into `system_health`, not
    // `current_risks` (which now shows only human coordination risks).
    let risks = room["data"]["room"]["system_health"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let unmanaged_count = risks
        .iter()
        .filter(|r| {
            r["subject"]
                .as_str()
                .map(|s| s.starts_with("unmanaged-agent: claude_code:99"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        unmanaged_count, 1,
        "expected exactly one unmanaged-agent risk fact; risks: {risks:?}"
    );

    workspace.cleanup();
}

/// C-FLEET: a tool that later runs `rally adopt` flips out of the
/// unmanaged-agent state. Under Plan F, adoption is limited to tmux/cmux
/// targets because Herdr is no longer a managed backend.
#[test]
fn rally_adopt_flips_stray_to_managed_with_cmux() {
    let workspace = Workspace::new("rally-fleet-adopt-cmux");

    let first_enter = workspace.json(&["enter", "--tool", "claude_code:42", "--json"]);
    let first_warnings = first_enter["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        first_warnings
            .iter()
            .any(|w| w["code"] == "unmanaged-agent"),
        "first enter must warn"
    );

    let adopt = workspace.json(&[
        "adopt",
        "stray-cmux",
        "--cmux",
        "workspace:42",
        "--tool",
        "claude_code:42",
        "--agent",
        "claude",
        "--json",
    ]);
    assert_eq!(adopt["schema"], "agent-rally.command.adopt.v1");
    assert_matches_schema("agent-rally.command.adopt.v1.json", &adopt);
    assert_eq!(adopt["data"]["adopt"]["session"]["target"], "workspace:42");
    assert_eq!(adopt["data"]["adopt"]["session"]["backend"], "cmux");
    assert_eq!(
        adopt["data"]["adopt"]["session"]["tool"], "claude_code:42",
        "explicit --tool must round-trip"
    );

    let second_enter = workspace.json(&["enter", "--tool", "claude_code:42", "--json"]);
    let second_warnings = second_enter["data"]["enter"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let still_unmanaged = second_warnings
        .iter()
        .any(|w| w["code"] == "unmanaged-agent");
    assert!(
        !still_unmanaged,
        "after adopt, unmanaged-agent warning must not fire; warnings: {second_warnings:?}"
    );

    let sessions = workspace.json(&["sessions", "--json"]);
    let targets: Vec<String> = sessions["data"]["sessions"]["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|s| s["target"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        targets.iter().any(|target| target == "workspace:42"),
        "adopted workspace:42 must show in sessions; got: {targets:?}"
    );

    workspace.cleanup();
}

/// C-FLEET: `rally adopt` with neither --tmux nor --cmux is a clear usage
/// error, not a silent success.
#[test]
fn rally_adopt_requires_tmux_or_cmux() {
    let workspace = Workspace::new("rally-fleet-adopt-noargs");
    let output = workspace.output(&["adopt", "foo", "--json"]);
    assert!(!output.status.success(), "expected non-zero exit");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let value: Value =
        serde_json::from_str(stderr.trim()).expect("stderr must be JSON on usage error");
    let msg = value["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("--tmux") && msg.contains("--cmux"),
        "expected --tmux/--cmux usage error; got: {msg}"
    );

    workspace.cleanup();
}

/// Duplicate adoption must be rejected by target, even if a later caller uses
/// a different `--tool` value.
#[test]
fn rally_adopt_rejects_duplicate_target_across_different_tools() {
    let workspace = Workspace::new("rally-adopt-target-dedup");

    let first = workspace.json(&[
        "adopt",
        "first-adoptee",
        "--json",
        "--tmux",
        "rally-shared",
        "--tool",
        "claude_code:adopt-a",
        "--backend",
        "tmux",
    ]);
    assert_eq!(first["data"]["adopt"]["session"]["target"], "rally-shared");

    let second = workspace.output(&[
        "adopt",
        "second-adoptee",
        "--json",
        "--tmux",
        "rally-shared",
        "--tool",
        "claude_code:adopt-b",
        "--backend",
        "tmux",
    ]);
    assert!(
        !second.status.success(),
        "duplicate-target adopt with different tool succeeded; stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already adopted") || stderr.contains("rally-shared"),
        "expected dedup error mentioning target; got stderr: {stderr}"
    );

    workspace.cleanup();
}

// ===========================================================================
// Phase 1b: per-agent linked worktree provisioning.
// ===========================================================================

/// Default behavior: `rally run` with no special flag and no env override
/// provisions a dedicated linked git worktree on a per-agent branch.
#[test]
fn rally_run_default_provisions_per_agent_worktree() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let _run_guard = serialize_rally_run();
    let workspace = real_repo_workspace("rally-run-default-worktree");

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

    let session_id = run["data"]["run"]["session"]["session_id"]
        .as_str()
        .expect("session_id");
    let branch = run["data"]["run"]["session"]["branch"]
        .as_str()
        .expect("branch must be populated under default isolation");
    let worktree_path = run["data"]["run"]["session"]["worktree_path"]
        .as_str()
        .expect("worktree_path must be populated under default isolation");
    let cwd = run["data"]["run"]["session"]["cwd"].as_str().expect("cwd");

    assert_eq!(branch, format!("rally/{session_id}"));
    assert_ne!(branch, "main", "agent branch must not be main");

    let wt = PathBuf::from(worktree_path);
    assert!(
        wt.exists(),
        "worktree path {} must exist on disk after rally run",
        wt.display()
    );
    let expected_wt = workspace
        .cwd
        .join(".rally")
        .join("worktrees")
        .join(session_id);
    assert_eq!(
        wt.canonicalize().unwrap_or(wt.clone()),
        expected_wt
            .canonicalize()
            .unwrap_or_else(|_| expected_wt.clone()),
        "worktree must live under .rally/worktrees/<session-id>/"
    );

    assert_eq!(cwd, worktree_path);

    let head = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        branch,
        "the worktree's HEAD must be the per-agent branch"
    );

    let start = run["data"]["run"]["commands"]["start"][0]
        .as_array()
        .unwrap();
    let joined = start
        .iter()
        .map(|v| v.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains(worktree_path),
        "backend start command must cd into the worktree; got: {joined}"
    );

    workspace.cleanup();
}

/// Two agents launched in the same repo get two distinct linked worktrees on
/// two distinct branches, but both resolve to the same `.rally/` room.
#[test]
fn two_rally_runs_get_distinct_worktrees_sharing_one_room() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let _run_guard = serialize_rally_run();
    let workspace = real_repo_workspace("rally-run-two-worktrees");

    let first = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "alpha",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    let second = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--name",
        "beta",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);

    let wt_a = first["data"]["run"]["session"]["worktree_path"]
        .as_str()
        .unwrap();
    let wt_b = second["data"]["run"]["session"]["worktree_path"]
        .as_str()
        .unwrap();
    let br_a = first["data"]["run"]["session"]["branch"].as_str().unwrap();
    let br_b = second["data"]["run"]["session"]["branch"].as_str().unwrap();
    assert_ne!(wt_a, wt_b, "the two worktrees must be distinct paths");
    assert_ne!(br_a, br_b, "the two agents must be on distinct branches");
    assert!(PathBuf::from(wt_a).exists());
    assert!(PathBuf::from(wt_b).exists());

    let canonical_room = workspace.cwd.join(".rally").join("facts.db");
    assert!(
        canonical_room.exists(),
        "canonical room must exist at {}",
        canonical_room.display()
    );
    assert!(
        !PathBuf::from(wt_a).join(".rally").join("facts.db").exists(),
        "linked worktree A must not carry its own facts.db"
    );
    assert!(
        !PathBuf::from(wt_b).join(".rally").join("facts.db").exists(),
        "linked worktree B must not carry its own facts.db"
    );

    let mut room_from_wt = Command::new(env!("CARGO_BIN_EXE_rally"));
    room_from_wt
        .current_dir(wt_a)
        .env("HOME", &workspace.home)
        .args(["sessions", "--json"]);
    let out = room_from_wt.output().unwrap();
    assert!(
        out.status.success(),
        "sessions from inside linked worktree must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let sessions = value["data"]["sessions"]["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        2,
        "sessions from one linked worktree must see both agents"
    );

    workspace.cleanup();
}

/// `rally stop` removes the per-agent worktree and, when safe, its branch.
#[test]
fn rally_stop_removes_per_agent_worktree() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let _run_guard = serialize_rally_run();
    let workspace = real_repo_workspace("rally-run-stop-removes-wt");

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--name",
        "stoppable",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    let session_id = run["data"]["run"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let worktree_path = run["data"]["run"]["session"]["worktree_path"]
        .as_str()
        .unwrap()
        .to_string();
    let branch = run["data"]["run"]["session"]["branch"]
        .as_str()
        .unwrap()
        .to_string();

    assert!(PathBuf::from(&worktree_path).exists());

    let stop = workspace.json(&["stop", &session_id, "--json", "--tmux-bin", "/usr/bin/true"]);
    assert_eq!(stop["schema"], "agent-rally.command.session-action.v1");

    assert!(
        !PathBuf::from(&worktree_path).exists(),
        "worktree path {worktree_path} must be removed by rally stop"
    );

    let wl = Command::new("git")
        .arg("-C")
        .arg(&workspace.cwd)
        .args(["worktree", "list"])
        .output()
        .unwrap();
    assert!(wl.status.success());
    let listing = String::from_utf8_lossy(&wl.stdout);
    assert!(
        !listing.contains(&worktree_path),
        "git worktree list must not mention removed worktree; got: {listing}"
    );

    let branch_check = Command::new("git")
        .arg("-C")
        .arg(&workspace.cwd)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .unwrap();
    assert!(
        !branch_check.status.success(),
        "empty per-agent branch {branch} must be deleted after stop"
    );

    workspace.cleanup();
}

/// `rally run --shared` and `--no-worktree` opt out of worktree isolation.
#[test]
fn rally_run_shared_flag_opts_out_of_worktree() {
    let _run_guard = serialize_rally_run();
    let mut workspace = Workspace::new("rally-run-shared-flag");
    workspace.suppress_worktree = false;

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--shared",
        "--name",
        "shared-mode",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(run["schema"], "agent-rally.command.run.v1");
    assert!(
        run["data"]["run"]["session"]["worktree_path"].is_null(),
        "--shared must skip worktree provisioning"
    );
    assert!(
        run["data"]["run"]["session"]["branch"].is_null(),
        "--shared must skip branch creation"
    );
    let cwd = run["data"]["run"]["session"]["cwd"].as_str().unwrap();
    let cwd_path = PathBuf::from(cwd);
    assert_eq!(
        cwd_path.canonicalize().unwrap_or(cwd_path),
        workspace
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| workspace.cwd.clone()),
        "--shared session's cwd must be the canonical checkout"
    );

    let run_alt = workspace.json(&[
        "run",
        "codex",
        "--json",
        "--no-worktree",
        "--name",
        "alt-mode",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert!(
        run_alt["data"]["run"]["session"]["worktree_path"].is_null(),
        "--no-worktree must also skip worktree provisioning"
    );

    workspace.cleanup();
}

/// `rally run --dry-run` reports the planned worktree path and branch without
/// touching the filesystem.
#[test]
fn rally_run_dry_run_reports_planned_worktree_without_touching_disk() {
    let _run_guard = serialize_rally_run();
    let mut workspace = Workspace::new("rally-run-dryrun-plan");
    workspace.suppress_worktree = false;

    let run = workspace.json(&[
        "run",
        "claude",
        "--json",
        "--dry-run",
        "--name",
        "planner",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert_eq!(run["data"]["run"]["mode"], "dry-run");
    let planned_wt = run["data"]["run"]["session"]["worktree_path"]
        .as_str()
        .expect("dry-run must still advertise planned worktree path");
    let planned_branch = run["data"]["run"]["session"]["branch"]
        .as_str()
        .expect("dry-run must still advertise planned branch");
    assert!(
        planned_wt.contains(".rally/worktrees/"),
        "planned worktree path must point under .rally/worktrees/; got {planned_wt}"
    );
    assert!(
        planned_branch.starts_with("rally/"),
        "planned branch must use rally/ prefix; got {planned_branch}"
    );
    assert!(
        !PathBuf::from(planned_wt).exists(),
        "dry-run must not create the worktree on disk"
    );

    workspace.cleanup();
}
