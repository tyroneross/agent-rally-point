// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
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

struct RallyWorkspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl RallyWorkspace {
    fn new(name: &str) -> Self {
        let cwd = temp_channel(&format!("{name}-cwd"));
        let home = temp_channel(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        run_rally_in(&self.cwd, &self.home, args)
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn run_rally(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rally"))
        .args(args)
        .output()
        .unwrap()
}

fn run_rally_in(cwd: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("RALLY_CMUX_CONFIG_DIR")
        .env_remove("RALLY_HERDR_CONFIG_DIR")
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

    let output = Command::new(env!("CARGO_BIN_EXE_rally"))
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
    let workspace = RallyWorkspace::new("rally-cli-typed-handoff");
    let handoff = json_stdout(workspace.run(&[
        "handoff",
        "--json",
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

    let ack = json_stdout(workspace.run(&[
        "ack",
        "--json",
        "--tool",
        "codex",
        "--summary",
        "done",
        handoff_id,
    ]));
    workspace.cleanup();

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
    let workspace = RallyWorkspace::new("rally-cli-typed-lifecycles");
    let claim = json_stdout(workspace.run(&[
        "claim",
        "--json",
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

    let release = json_stdout(workspace.run(&[
        "release", "--json", "--tool", "codex", "--reason", "done", claim_id,
    ]));
    assert_eq!(release["event"]["type"], "agent-rally.claim.released.v1");
    assert_eq!(release["event"]["causation_id"], claim_id);
    assert_eq!(release["event"]["thread_id"], claim["event"]["thread_id"]);

    let blocker = json_stdout(workspace.run(&[
        "blocker",
        "--json",
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

    let unblock = json_stdout(workspace.run(&[
        "unblock",
        "--json",
        "--tool",
        "codex",
        blocker_id,
        "--resolution",
        "branch supplied",
    ]));
    workspace.cleanup();

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
fn strict_enforcement_rejects_anonymous_writes() {
    let workspace = RallyWorkspace::new("rally-cli-strict-enforcement");
    json_stdout(workspace.run(&["setup", "enforcement", "strict", "--json"]));

    let anonymous_claim = workspace.run(&[
        "claim",
        "--json",
        "--path",
        "crates/rally-cli/src/main.rs",
        "--subject",
        "anonymous edit",
    ]);
    assert_eq!(anonymous_claim.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&anonymous_claim.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("strict enforcement")
    );

    let anonymous_owner = workspace.run(&[
        "task",
        "--json",
        "--tool",
        "pi",
        "--owner",
        "unknown",
        "--subject",
        "anonymous owner",
    ]);
    assert_eq!(anonymous_owner.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&anonymous_owner.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("owner_tool=unknown")
    );

    let named_claim = json_stdout(workspace.run(&[
        "claim",
        "--json",
        "--tool",
        "pi",
        "--path",
        "crates/rally-cli/src/main.rs",
        "--subject",
        "named edit",
    ]));
    assert_eq!(named_claim["tool"], "pi");
    workspace.cleanup();
}

#[test]
fn setup_install_writes_adapter_hooks() {
    let workspace = RallyWorkspace::new("rally-cli-adapter-install");
    let cmux = json_stdout(workspace.run(&["setup", "install", "cmux", "--json"]));
    let cmux_config = workspace.home.join(".config/cmux/cmux.json");
    let cmux_wrapper = workspace.home.join(".config/cmux/rally-agent-wrapper.sh");
    assert_eq!(
        cmux["data"]["setup"]["modified_external_config"],
        cmux_config.display().to_string()
    );
    assert!(cmux_wrapper.exists());
    let cmux_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cmux_config).unwrap()).unwrap();
    assert!(
        cmux_json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "rally-agent")
    );

    let herdr = json_stdout(workspace.run(&["setup", "install", "herdr", "--json"]));
    let herdr_config = workspace.home.join(".config/herdr/config.toml");
    let herdr_wrapper = workspace
        .home
        .join(".config/herdr/integrations/rally-agent-start.sh");
    assert_eq!(
        herdr["data"]["setup"]["modified_external_config"],
        herdr_config.display().to_string()
    );
    assert!(herdr_wrapper.exists());
    let config_text = fs::read_to_string(herdr_config).unwrap();
    assert!(config_text.contains("[integrations.rally]"));
    assert!(config_text.contains("packet_command"));
    workspace.cleanup();
}

#[test]
fn hook_before_write_auto_claims_and_blocks_conflicts() {
    let workspace = RallyWorkspace::new("rally-cli-hook-judge");
    let hook = json_stdout(workspace.run(&[
        "hook",
        "before-write",
        "--json",
        "--tool",
        "pi",
        "--path",
        "src/lib.rs",
        "--auto-claim",
    ]));
    let judgment = &hook["data"]["hook"]["judgment"];
    assert_eq!(judgment["allow"], true);
    assert_eq!(judgment["decision"], "continue");
    assert_eq!(judgment["resource"], "file:src/lib.rs");
    assert!(judgment["auto_claimed"].as_str().is_some());

    let conflict = json_stdout(workspace.run(&[
        "judge",
        "--json",
        "--tool",
        "codex",
        "--phase",
        "before-write",
        "--path",
        "src/lib.rs",
    ]));
    let judgment = &conflict["data"]["judgment"];
    assert_eq!(judgment["allow"], false);
    assert_eq!(judgment["decision"], "pause");
    assert_eq!(judgment["claim_conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(judgment["agent_visible"]["present"], true);
    workspace.cleanup();
}

#[test]
fn next_recommends_highest_scoring_action() {
    let workspace = RallyWorkspace::new("rally-cli-next");
    json_stdout(workspace.run(&[
        "task",
        "--json",
        "--tool",
        "pi",
        "--owner",
        "pi",
        "--subject",
        "owned task",
    ]));
    let owned = json_stdout(workspace.run(&["next", "--json", "--tool", "pi"]));
    assert_eq!(owned["data"]["next"]["action_kind"], "progress_owned_task");

    json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--tool",
        "codex",
        "--from-tool",
        "codex",
        "--to",
        "pi",
        "--subject",
        "review this first",
    ]));
    let handoff = json_stdout(workspace.run(&["next", "--json", "--tool", "pi"]));
    assert_eq!(handoff["data"]["next"]["action_kind"], "pick_up_handoff");
    assert_eq!(handoff["data"]["next"]["subject"], "review this first");
    assert_eq!(handoff["data"]["next"]["agent_visible"]["present"], true);
    assert_eq!(handoff["data"]["next"]["agent_visible"]["severity"], "stop");
    assert_eq!(
        handoff["data"]["next"]["alternatives"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    workspace.cleanup();
}

#[test]
fn context_focus_attaches_graph_neighborhood() {
    // intent: `rally context --focus <event-id>` must return both the
    // legacy brief AND a graph-backed neighborhood + causal chain rooted
    // at the focus node. Without focus, the response omits the graph
    // field. This is the additive migration path — old callers
    // unchanged, new callers get bounded context.
    let workspace = RallyWorkspace::new("rally-cli-context-focus");

    // Build a small causal chain: task T1, derived task T2 depends on T1,
    // artifact A1 produced by T1.
    let t1 = json_stdout(workspace.run(&[
        "task",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "foundation task",
    ]));
    let t1_id = t1["event_id"].as_str().unwrap().to_string();

    json_stdout(workspace.run(&[
        "task",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "follow-up task",
        "--depends-on",
        &t1_id,
    ]));

    json_stdout(workspace.run(&[
        "artifact",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "deliverable for foundation",
        "--artifact-kind",
        "code",
        "--ref-task",
        &t1_id,
    ]));

    // Without --focus, brief is present but graph is omitted.
    let plain = json_stdout(workspace.run(&["context", "--json", "--tool", "pi"]));
    assert!(
        plain["data"]["brief"].is_object(),
        "plain context must include brief"
    );
    assert!(
        plain["data"].get("graph").is_none(),
        "plain context must not include graph field"
    );

    // With --focus, both brief and graph appear.
    let focused =
        json_stdout(workspace.run(&["context", "--json", "--tool", "pi", "--focus", &t1_id]));
    assert!(focused["data"]["brief"].is_object());
    let graph = &focused["data"]["graph"];
    assert!(
        graph.is_object(),
        "focused context must include graph field"
    );
    assert_eq!(graph["focus"], t1_id);
    let node_count = graph["subgraph"]["nodes"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    assert!(
        node_count >= 2,
        "subgraph around the foundation task must include at least itself plus a neighbor"
    );
    workspace.cleanup();
}

#[test]
fn next_drops_artifacts_after_non_producer_activity() {
    // intent: once a peer other than the artifact's producer posts a
    // follow-up event, that artifact must stop surfacing in `rally next`
    // recommendations. This was the stale-recommendation bug — reviewed
    // work kept resurfacing because the projection had no way to mark it
    // consumed.
    let workspace = RallyWorkspace::new("rally-cli-next-unconsumed");

    // pi ships an artifact. Until anyone else posts, it's a valid candidate.
    let art = json_stdout(workspace.run(&[
        "artifact",
        "--json",
        "--tool",
        "pi",
        "--subject",
        "fresh artifact awaiting review",
        "--artifact-kind",
        "code",
    ]));
    let art_id = art["event_id"].as_str().unwrap().to_string();

    let before = json_stdout(workspace.run(&["next", "--json", "--tool", "pi"]));
    assert_eq!(
        before["data"]["next"]["target_event_id"], art_id,
        "while no other peer has acted, pi's own artifact must be the top candidate"
    );

    // claude_code posts any event — a handoff is the cheapest realistic one.
    // The artifact above is now "consumed" (someone else engaged with the work).
    json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--tool",
        "claude_code",
        "--from-tool",
        "claude_code",
        "--to",
        "pi",
        "--subject",
        "looked at it",
    ]));

    let after = json_stdout(workspace.run(&["next", "--json", "--tool", "pi"]));
    let target = after["data"]["next"]["target_event_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let alts: Vec<String> = after["data"]["next"]["alternatives"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|alt| alt["target_event_id"].as_str().map(str::to_string))
        .collect();

    assert_ne!(
        target, art_id,
        "artifact must drop from top candidate after non-producer activity"
    );
    assert!(
        !alts.contains(&art_id),
        "artifact must not appear in alternatives either"
    );
    workspace.cleanup();
}

#[test]
fn setup_install_and_uninstall_agent_wrapper() {
    let workspace = RallyWorkspace::new("rally-cli-agent-wrapper");
    let install = json_stdout(workspace.run(&["setup", "install", "pi", "--json"]));
    let extension = workspace
        .home
        .join(".pi/agent/extensions/rally-judgment.ts");
    assert_eq!(
        install["data"]["setup"]["installed_path"],
        extension.display().to_string()
    );
    assert!(extension.exists());
    assert!(
        fs::read_to_string(&extension)
            .unwrap()
            .contains("before_agent_start")
    );

    let uninstall = json_stdout(workspace.run(&["setup", "uninstall", "pi", "--json"]));
    assert!(
        uninstall["data"]["setup"]["installed_files"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    assert!(!extension.exists());

    let claude = json_stdout(workspace.run(&["setup", "install", "claude", "--json"]));
    let claude_settings = workspace.home.join(".claude/settings.json");
    let claude_hook = workspace.home.join(".claude/hooks/rally-hook.sh");
    assert_eq!(
        claude["data"]["setup"]["modified_external_config"],
        claude_settings.display().to_string()
    );
    assert!(claude_hook.exists());
    assert!(
        fs::read_to_string(&claude_settings)
            .unwrap()
            .contains("PreToolUse")
    );
    assert!(
        fs::read_to_string(&claude_hook)
            .unwrap()
            .contains("additionalContext")
    );

    let codex = json_stdout(workspace.run(&["setup", "install", "codex", "--json"]));
    let codex_hooks = workspace.home.join(".codex/hooks.json");
    let codex_config = workspace.home.join(".codex/config.toml");
    assert_eq!(
        codex["data"]["setup"]["modified_external_config"],
        codex_hooks.display().to_string()
    );
    assert!(
        fs::read_to_string(&codex_hooks)
            .unwrap()
            .contains("PreToolUse")
    );
    assert!(
        fs::read_to_string(&codex_config)
            .unwrap()
            .contains("hooks = true")
    );

    let gemini = json_stdout(workspace.run(&["setup", "install", "gemini", "--json"]));
    let gemini_settings = workspace.home.join(".gemini/settings.json");
    assert_eq!(
        gemini["data"]["setup"]["modified_external_config"],
        gemini_settings.display().to_string()
    );
    assert!(
        fs::read_to_string(&gemini_settings)
            .unwrap()
            .contains("BeforeTool")
    );

    let dry_workspace = RallyWorkspace::new("rally-cli-setup-dry-run");
    let dry = json_stdout(dry_workspace.run(&["setup", "install", "codex", "--dry-run", "--json"]));
    let dry_codex_hooks = dry_workspace.home.join(".codex/hooks.json");
    assert_eq!(dry["data"]["setup"]["dry_run"], true);
    assert_eq!(
        dry["data"]["setup"]["modified_external_config"],
        dry_codex_hooks.display().to_string()
    );
    assert!(!dry_codex_hooks.exists());

    let verify_before = json_stdout(dry_workspace.run(&["setup", "verify", "codex", "--json"]));
    assert_eq!(
        verify_before["data"]["setup"]["verification"][0]["installed"],
        false
    );
    json_stdout(dry_workspace.run(&["setup", "install", "codex", "--json"]));
    let second_install = json_stdout(dry_workspace.run(&["setup", "install", "codex", "--json"]));
    assert!(
        !second_install["data"]["setup"]["backup_paths"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let verify_after = json_stdout(dry_workspace.run(&["setup", "verify", "codex", "--json"]));
    assert_eq!(
        verify_after["data"]["setup"]["verification"][0]["installed"],
        true
    );
    dry_workspace.cleanup();
    workspace.cleanup();
}

#[test]
fn repair_profile_and_ci_gate() {
    let workspace = RallyWorkspace::new("rally-cli-repair-ci");
    let repair = json_stdout(workspace.run(&["repair", "profile", "--json", "--tool", "pi"]));
    assert_eq!(repair["data"]["repair"]["repaired"], true);
    assert!(repair["data"]["repair"]["event_id"].as_str().is_some());

    let pass = json_stdout(workspace.run(&["ci", "gate", "--json", "--tool", "ci"]));
    assert_eq!(pass["data"]["gate"]["status"], "pass");

    json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--tool",
        "pi",
        "--from-tool",
        "pi",
        "--to",
        "codex",
        "--subject",
        "required review",
    ]));
    let fail = workspace.run(&["ci", "gate", "--json", "--tool", "ci"]);
    assert!(!fail.status.success());
    let body: serde_json::Value = serde_json::from_slice(&fail.stderr).unwrap();
    assert_eq!(body["ok"], false);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("coordination gate failed")
    );
    workspace.cleanup();
}

#[test]
fn query_commands_project_typed_state() {
    let workspace = RallyWorkspace::new("rally-cli-query-state");
    let handoff = json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "review schema",
    ]));
    let handoff_id = handoff["event_id"].as_str().unwrap();
    json_stdout(workspace.run(&[
        "claim",
        "--json",
        "--tool",
        "codex",
        "--resource",
        "file:docs",
        "--subject",
        "docs edit",
    ]));
    json_stdout(workspace.run(&[
        "claim",
        "--json",
        "--tool",
        "pi",
        "--path",
        "docs/SCHEMA.md",
        "--subject",
        "schema edit",
    ]));
    let blocker = json_stdout(workspace.run(&[
        "blocker",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "need branch",
    ]));
    let blocker_id = blocker["event_id"].as_str().unwrap();

    let inbox = json_stdout(workspace.run(&["inbox", "--json", "--tool", "codex"]));
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);

    let claims = json_stdout(workspace.run(&["claims", "--json", "--tool", "codex"]));
    assert_eq!(claims["data"]["claims"].as_array().unwrap().len(), 1);
    assert_eq!(claims["data"]["claims"][0]["resource"], "file:docs");

    let blockers = json_stdout(workspace.run(&["blockers", "--json", "--tool", "codex"]));
    assert_eq!(blockers["data"]["blockers"][0]["event_id"], blocker_id);

    let conflicts = json_stdout(workspace.run(&["conflicts", "--json"]));
    assert_eq!(conflicts["data"]["conflicts"][0]["resource"], "file:docs");
    assert_eq!(
        conflicts["data"]["conflicts"][0]["owners"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let context = json_stdout(workspace.run(&["context", "--json", "--tool", "codex"]));
    assert_eq!(context["data"]["brief"]["routing"]["action"], "join_active");
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["action"],
        "ack_handoff"
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["target"],
        handoff_id
    );
    assert_eq!(context["data"]["brief"]["top_priority"]["kind"], "handoff");

    let diagnosis = json_stdout(workspace.run(&["diagnose", "--json", "--tool", "codex"]));
    assert_eq!(diagnosis["data"]["diagnosis"]["status"], "stuck");
    assert!(
        diagnosis["data"]["diagnosis"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "active-blocker")
    );

    let score = json_stdout(workspace.run(&["score", "--json", "--tool", "codex"]));
    assert!(score["data"]["score"].as_i64().unwrap() < 100);

    let thread = json_stdout(workspace.run(&["thread", "--json", handoff_id]));
    assert_eq!(thread["data"]["identifier"], handoff_id);
    assert_eq!(thread["data"]["events"][0]["event"]["id"], handoff_id);

    let replay = json_stdout(workspace.run(&["replay", "--json", "--limit", "2"]));
    assert_eq!(replay["data"]["events"].as_array().unwrap().len(), 2);

    let report = json_stdout(workspace.run(&["report", "--json"]));
    workspace.cleanup();

    assert_eq!(report["data"]["records"], 4);
    assert_eq!(report["data"]["counts"]["claim"], 2);
    assert_eq!(report["data"]["counts"]["handoff"], 1);
}

#[test]
fn preflight_routes_agents_and_writes_presence() {
    let workspace = RallyWorkspace::new("rally-cli-preflight");
    let handoff = json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "take the next PR",
    ]));
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let codex = json_stdout(workspace.run(&[
        "preflight",
        "--json",
        "--tool",
        "codex",
        "--session-id",
        "codex-session",
        "--start-ping",
    ]));
    assert_eq!(codex["ok"], true);
    assert_eq!(codex["command"], "preflight");
    assert_eq!(codex["schema"], "agent-rally.command.preflight.v1");
    assert_eq!(codex["coordination_status"], "active");
    assert_eq!(codex["routing"]["action"], "join_active");
    assert_eq!(codex["pending_acks_for_me"][0]["event_id"], handoff_id);
    assert!(
        codex["presence_written"]
            .as_str()
            .unwrap()
            .contains("codex-session")
    );

    let pi = json_stdout(workspace.run(&[
        "preflight",
        "--json",
        "--tool",
        "pi",
        "--session-id",
        "pi-session",
    ]));
    workspace.cleanup();

    assert_eq!(pi["pending_acks_for_me"].as_array().unwrap().len(), 0);
    assert_eq!(pi["routing"]["action"], "join_active");
    assert_eq!(pi["active_peers"][0]["tool"], "codex");
    assert_eq!(pi["recent_changes"][0]["event_id"], handoff_id);
}

#[test]
fn attuned_fact_commands_feed_context_brief() {
    let workspace = RallyWorkspace::new("rally-cli-attuned-context");

    json_stdout(workspace.run(&[
        "profile",
        "--json",
        "--tool",
        "codex",
        "--capability",
        "rust",
        "--capability",
        "review",
        "--role",
        "reviewer",
        "--watch",
        "crates/rally-core",
        "--current-task",
        "task-local",
        "--branch",
        "codex/rally-attuned-events",
        "--availability",
        "active",
    ]));
    json_stdout(workspace.run(&[
        "subscribe",
        "--json",
        "--tool",
        "codex",
        "--path",
        "crates/rally-core",
        "--event-kind",
        "task",
        "--event-kind",
        "decision",
        "--task",
        "task-local",
    ]));
    let task = json_stdout(workspace.run(&[
        "task",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "finish context ranking",
        "--status",
        "active",
        "--verification",
        "cargo test",
    ]));
    let task_id = task["event_id"].as_str().unwrap();
    json_stdout(workspace.run(&[
        "artifact",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "context contract",
        "--artifact-kind",
        "schema",
        "--uri",
        "docs/context.schema.json",
        "--ref-task",
        task_id,
    ]));
    json_stdout(workspace.run(&[
        "decision",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "agents use rally context for next action",
        "--status",
        "binding",
        "--scope",
        "agent-start",
    ]));
    json_stdout(workspace.run(&[
        "lesson",
        "--json",
        "--tool",
        "codex",
        "--subject",
        "avoid giant planning docs as control surfaces",
        "--lesson-kind",
        "coordination",
        "--source-event",
        task_id,
        "--confidence",
        "0.9",
    ]));

    let context = json_stdout(workspace.run(&["context", "--json", "--tool", "codex"]));
    workspace.cleanup();

    assert_eq!(context["data"]["brief"]["profile"]["tool"], "codex");
    assert_eq!(
        context["data"]["brief"]["profile"]["capabilities"][0],
        "rust"
    );
    assert_eq!(context["data"]["brief"]["profile"]["role"], "reviewer");
    assert_eq!(
        context["data"]["brief"]["subscription"]["event_kinds"][1],
        "decision"
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["action"],
        "work_task"
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["target"],
        task_id
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["minimum_trust_for_automation"],
        "trusted"
    );
    assert_eq!(
        context["data"]["brief"]["active_tasks"][0]["event_id"],
        task_id
    );
    assert_eq!(
        context["data"]["brief"]["attuned_items"][0]["event_id"],
        task_id
    );
    assert!(
        context["data"]["brief"]["attuned_items"][0]["factors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|factor| factor == "unresolved:task")
    );
    assert_eq!(
        context["data"]["brief"]["artifacts"][0]["ref_task_id"],
        task_id
    );
    assert_eq!(
        context["data"]["brief"]["decisions"][0]["status"],
        "binding"
    );
    assert_eq!(context["data"]["brief"]["lessons"][0]["confidence"], 0.9);
}

#[test]
fn packet_command_emits_role_shaped_contract() {
    let workspace = RallyWorkspace::new("rally-cli-packet-contract");

    json_stdout(workspace.run(&[
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
    ]));
    json_stdout(workspace.run(&[
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
    ]));
    json_stdout(workspace.run(&[
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
    ]));

    let packet = json_stdout(workspace.run(&["packet", "--json", "--tool", "codex-reviewer"]));
    workspace.cleanup();

    assert_eq!(packet["command"], "packet");
    assert_eq!(packet["schema"], "agent-rally.command.packet.v1");
    assert_eq!(packet["data"]["packet"]["role"], "reviewer");
    assert_eq!(packet["data"]["packet"]["packet_kind"], "review");
    assert!(
        packet["data"]["packet"]["review_targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "artifact")
    );
    assert_eq!(
        packet["data"]["packet"]["trust_summary"]["minimum_trust_for_automation"],
        "local-or-trusted"
    );
    assert!(packet["data"]["packet"]["build_targets"].is_null());
}

#[test]
fn herdr_inject_gate_surfaces_trust_and_requires_override_for_unsigned_handoffs() {
    let workspace = RallyWorkspace::new("rally-cli-herdr-gate");
    let handoff = json_stdout(workspace.run(&[
        "handoff",
        "--json",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "inject this into herdr",
    ]));
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let gate = json_stdout(workspace.run(&["herdr", "inject", "--json", handoff_id]));
    assert_eq!(gate["command"], "herdr:inject");
    assert_eq!(gate["schema"], "agent-rally.command.herdr-inject.v1");
    assert_eq!(gate["data"]["gate"]["action"], "refuse");
    assert_eq!(gate["data"]["gate"]["ready_to_inject"], false);
    assert_eq!(gate["data"]["gate"]["trust"]["trust_status"], "unsigned");

    let override_gate =
        json_stdout(workspace.run(&["herdr", "inject", "--json", "--force", handoff_id]));
    workspace.cleanup();

    assert_eq!(override_gate["data"]["gate"]["action"], "override");
    assert_eq!(override_gate["data"]["gate"]["ready_to_inject"], true);
    assert_eq!(override_gate["data"]["gate"]["override_used"], true);
}

#[test]
fn identity_init_and_signed_write_verify_as_trusted() {
    let workspace = RallyWorkspace::new("rally-cli-signed-channel");
    let identity_dir = temp_channel("rally-cli-identity");
    let identity_arg = identity_dir.to_str().unwrap();

    let identity = json_stdout(workspace.run(&[
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

    let handoff = json_stdout(workspace.run(&[
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
        "signed review",
    ]));
    assert_eq!(handoff["event"]["signature"]["key_id"], identity["key_id"]);
    assert_eq!(
        handoff["event"]["signature"]["canonicalization"],
        "rally-json-v1"
    );

    let trust_path = identity_dir.join("trust.toml");
    let signed_changes = Path::new(handoff["channel"].as_str().unwrap()).join("changes.jsonl");
    let verify = json_stdout(workspace.run(&[
        "verify",
        "--json",
        "--trust-policy",
        trust_path.to_str().unwrap(),
        signed_changes.to_str().unwrap(),
    ]));
    workspace.cleanup();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(verify["data"]["counts"]["trusted"], 1);
    assert_eq!(verify["data"]["events"][0]["status"], "trusted");
    assert_eq!(verify["data"]["events"][0]["key_id"], identity["key_id"]);
}

#[test]
fn sync_export_import_round_trips_signed_events() {
    let source = RallyWorkspace::new("rally-cli-sync-source");
    let dest = RallyWorkspace::new("rally-cli-sync-dest");
    let identity_dir = temp_channel("rally-cli-sync-identity");
    let identity_arg = identity_dir.to_str().unwrap();

    let identity = json_stdout(source.run(&[
        "identity",
        "init",
        "--json",
        "--identity-dir",
        identity_arg,
        "--tool",
        "codex",
    ]));
    let key_id = identity["key_id"].as_str().unwrap();
    let handoff = json_stdout(source.run(&[
        "handoff",
        "--json",
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

    let packet = json_stdout(source.run(&["sync", "export", "--json"]));
    assert_eq!(packet["schema"], "agent-rally.sync.packet.v1");
    assert_eq!(packet["count"], 1);
    assert_eq!(packet["events"][0]["id"], handoff_id);
    assert!(packet["events"][0].get("local_seq").is_none());
    let packet_path = write_json("rally-cli-sync-packet", &packet);

    let trust_path = identity_dir.join("trust.toml");
    let imported = json_stdout(dest.run(&[
        "sync",
        "import",
        "--json",
        "--trust-policy",
        trust_path.to_str().unwrap(),
        packet_path.to_str().unwrap(),
    ]));
    assert_eq!(imported["data"]["imported"], 1);
    assert_eq!(imported["data"]["duplicates"], 0);
    assert_eq!(imported["data"]["trust_counts"]["trusted"], 1);

    let inbox = json_stdout(dest.run(&["inbox", "--json", "--tool", "pi"]));
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);
    assert_eq!(inbox["data"]["pending"][0]["origin"], "import:sync");
    assert_eq!(inbox["data"]["pending"][0]["trust_status"], "trusted");

    let preflight = json_stdout(dest.run(&["preflight", "--json", "--tool", "pi"]));
    assert_eq!(preflight["pending_acks_for_me"][0]["origin"], "import:sync");
    assert_eq!(
        preflight["pending_acks_for_me"][0]["trust_status"],
        "trusted"
    );
    assert_eq!(preflight["recent_changes"][0]["origin"], "import:sync");
    assert_eq!(preflight["recent_changes"][0]["trust_status"], "trusted");

    let duplicate =
        json_stdout(dest.run(&["sync", "import", "--json", packet_path.to_str().unwrap()]));
    assert_eq!(duplicate["data"]["imported"], 0);
    assert_eq!(duplicate["data"]["duplicates"], 1);

    let mut conflict_packet = packet.clone();
    conflict_packet["events"][0]["payload"]["subject"] = serde_json::json!("different");
    let conflict_path = write_json("rally-cli-sync-conflict", &conflict_packet);
    let conflict =
        json_stdout(dest.run(&["sync", "import", "--json", conflict_path.to_str().unwrap()]));
    fs::remove_file(packet_path).unwrap();
    fs::remove_file(conflict_path).unwrap();
    source.cleanup();
    dest.cleanup();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(conflict["data"]["imported"], 0);
    assert_eq!(conflict["data"]["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(conflict["data"]["conflicts"][0]["event_id"], handoff_id);
}

#[test]
fn usage_errors_exit_nonzero_for_agent_automation() {
    let output = Command::new(env!("CARGO_BIN_EXE_rally")).output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: rally verify"));
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

    let output = Command::new(env!("CARGO_BIN_EXE_rally"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_rally"))
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
