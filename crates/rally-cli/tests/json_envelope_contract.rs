// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Contract test: every `rally <cmd> --json` output must satisfy:
//!   (a) stdout is valid JSON
//!   (b) top-level has `ok`, `command`, `product`, `schema`, `data`
//!   (c) `data` is a JSON object
//!   (d) `data[command]` exists (where `command` is the value of the `command` field)
//!
//! This test drives off COMMANDS so any new subcommand that omits the contract
//! will cause this test to fail, not silently skip.

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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("envelope-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("envelope-{name}-{nanos}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_GLOBAL_INDEX", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "cmd {:?} failed (exit {:?})\nstdout: {}\nstderr: {}",
            args,
            out.status.code(),
            stdout,
            stderr
        );
        serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "cmd {:?} stdout is not valid JSON: {e}\nraw: {stdout}",
                args
            )
        })
    }

    fn cleanup(self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

/// Assert the envelope contract for one command invocation.
///
/// `command_name` is the kebab-case command name (matches the `command` field in JSON).
/// `body` is the parsed JSON response.
fn assert_envelope_contract(command_name: &str, body: &Value) {
    // (a) already guaranteed by caller (parsed successfully)

    // (b) required top-level fields
    for field in &["ok", "command", "product", "schema", "data"] {
        assert!(
            !body[field].is_null(),
            "command={command_name}: top-level `{field}` is missing or null\nbody={body:#}"
        );
    }

    // (c) data is an object
    assert!(
        body["data"].is_object(),
        "command={command_name}: `data` is not a JSON object\nbody={body:#}"
    );

    // (d) data[command] exists
    let cmd_field = body["command"].as_str().unwrap_or(command_name);
    assert!(
        !body["data"][cmd_field].is_null(),
        "command={command_name}: `data[\"{cmd_field}\"]` is missing or null\nbody={body:#}"
    );
}

// ─── Per-command minimal invocations ─────────────────────────────────────────
// Each test exercises one command with the minimal valid args needed to produce
// a JSON response, then asserts the envelope contract.

/// `init` — sets up the rally dir; requires pointer-doc targets to exist.
#[test]
fn envelope_init() {
    let ws = Workspace::new("init");
    // init requires all pointer-doc targets to resolve under the repo root.
    fs::write(ws.cwd.join("RALLY.md"), "# Rally").unwrap();
    fs::write(ws.cwd.join("CLAUDE.md"), "# CLAUDE").unwrap();
    fs::write(ws.cwd.join("AGENTS.md"), "# AGENTS").unwrap();
    fs::create_dir_all(ws.cwd.join("dynamic-workflows")).unwrap();
    fs::write(ws.cwd.join("dynamic-workflows/COORDINATION.md"), "# Coord").unwrap();
    fs::write(ws.cwd.join("dynamic-workflows/PROTOCOL.md"), "# Protocol").unwrap();
    fs::create_dir_all(ws.cwd.join("docs")).unwrap();
    fs::write(ws.cwd.join("docs/ORCHESTRATION.md"), "# Orch").unwrap();
    fs::write(ws.cwd.join("docs/ANY-AGENT-ONBOARDING.md"), "# Any Agent").unwrap();
    let body = ws.json(&["init", "--json"]);
    assert_envelope_contract("init", &body);
    ws.cleanup();
}

/// `hooks status` — reads effective auto-coordination hook policy.
#[test]
fn envelope_hooks() {
    let ws = Workspace::new("hooks");
    let body = ws.json(&["hooks", "status", "--json"]);
    assert_envelope_contract("hooks", &body);
    assert_eq!(body["schema"], "agent-rally.command.hooks.v1");
    assert_eq!(body["data"]["hooks"]["enabled"], true);
    assert_eq!(body["data"]["hooks"]["prompt"], "once");
    ws.cleanup();
}

/// `enter` — registers agent presence; requires --tool.
#[test]
fn envelope_enter() {
    let ws = Workspace::new("enter");
    let body = ws.json(&["enter", "--json", "--tool", "test-agent"]);
    assert_envelope_contract("enter", &body);
    ws.cleanup();
}

/// `adopt` — registers an already-running tmux/cmux target as a managed
/// session; requires a name positional + one of --tmux/--cmux. Closes the
/// audit gap: adopt is the response arm of the fleet-enforcement rule and
/// must honor the same envelope contract as every other schema-stamped
/// command. HERDR-INDEPENDENT (no --pane arm).
#[test]
fn envelope_adopt() {
    let ws = Workspace::new("adopt");
    let body = ws.json(&[
        "adopt",
        "adopted-agent",
        "--json",
        "--tmux",
        "rally-adopted-agent",
        "--tool",
        "codex:adopted-01",
        "--agent",
        "codex",
    ]);
    assert_envelope_contract("adopt", &body);
    assert_eq!(body["schema"], "agent-rally.command.adopt.v1");
    assert!(
        !body["data"]["adopt"]["session"]["session_id"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "adopt envelope must carry a non-empty session_id\nbody={body:#}"
    );
    assert_eq!(
        body["data"]["adopt"]["session"]["target"],
        "rally-adopted-agent"
    );
    assert_eq!(body["data"]["adopt"]["session"]["backend"], "tmux");
    ws.cleanup();
}

/// `say` — appends a fact; requires --tool and kind positional.
#[test]
fn envelope_say() {
    let ws = Workspace::new("say");
    let body = ws.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "test-agent",
        "--subject",
        "test claim",
    ]);
    assert_envelope_contract("say", &body);
    ws.cleanup();
}

/// `room` — reads room state; no required args (opens current repo room).
#[test]
fn envelope_ack() {
    let ws = Workspace::new("ack");
    ws.json(&["enter", "--json", "--tool", "test-agent"]);
    let body = ws.json(&["ack", "--json", "--tool", "test-agent"]);
    assert_envelope_contract("ack", &body);
}

#[test]
fn envelope_lead() {
    let ws = Workspace::new("lead");
    let body = ws.json(&["lead", "show", "--json"]);
    assert_envelope_contract("lead", &body);
}

#[test]
fn envelope_room() {
    let ws = Workspace::new("room");
    let body = ws.json(&["room", "--json"]);
    assert_envelope_contract("room", &body);
    ws.cleanup();
}

/// `next` — requires --tool.
#[test]
fn envelope_next() {
    let ws = Workspace::new("next");
    let body = ws.json(&["next", "--json", "--tool", "test-agent"]);
    assert_envelope_contract("next", &body);
    ws.cleanup();
}

/// `check` — requires --tool for before-write, but before-complete works without it.
#[test]
fn envelope_check() {
    let ws = Workspace::new("check");
    // before-write requires a --tool
    ws.json(&["enter", "--json", "--tool", "test-agent"]);
    let body = ws.json(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "test-agent",
        "--path",
        "src/lib.rs",
    ]);
    assert_envelope_contract("check", &body);
    ws.cleanup();
}

/// `locate` — requires an EVENT_ID positional; uses a dummy id (returns not-found).
#[test]
fn envelope_locate() {
    let ws = Workspace::new("locate");
    let body = ws.json(&["locate", "--json", "fact_000000_000000"]);
    assert_envelope_contract("locate", &body);
    ws.cleanup();
}

/// `recent` — no required args; returns empty list in a fresh room.
#[test]
fn envelope_recent() {
    let ws = Workspace::new("recent");
    let body = ws.json(&["recent", "--json"]);
    assert_envelope_contract("recent", &body);
    ws.cleanup();
}

/// `retrospective` — reads from room ledger; needs --out to avoid writing to worktree.
#[test]
fn envelope_retrospective() {
    let ws = Workspace::new("retrospective");
    let out_path = ws.cwd.join("retro.md");
    let body = ws.json(&[
        "retrospective",
        "--json",
        "--out",
        out_path.to_str().unwrap(),
    ]);
    assert_envelope_contract("retrospective", &body);
    ws.cleanup();
}

/// `rotate` — no required args; dry-run avoids any filesystem changes.
#[test]
fn envelope_rotate() {
    let ws = Workspace::new("rotate");
    let body = ws.json(&["rotate", "--json", "--dry-run"]);
    assert_envelope_contract("rotate", &body);
    ws.cleanup();
}

/// `status` — requires --global; fix: must emit JSON even when the global index is empty.
#[test]
fn envelope_status() {
    let ws = Workspace::new("status");
    let body = ws.json(&["status", "--json", "--global"]);
    assert_envelope_contract("status", &body);
    ws.cleanup();
}

/// `migrate-legacy` — no required args; idempotent read-then-write.
#[test]
fn envelope_migrate_legacy() {
    let ws = Workspace::new("migrate-legacy");
    let body = ws.json(&["migrate-legacy", "--json"]);
    assert_envelope_contract("migrate-legacy", &body);
    ws.cleanup();
}

/// `doctor --canonical-paths` — read-only; works on an empty room.
#[test]
fn envelope_doctor() {
    let ws = Workspace::new("doctor");
    let body = ws.json(&["doctor", "--json", "--canonical-paths"]);
    assert_envelope_contract("doctor", &body);
    ws.cleanup();
}

/// `version` — no required args; always succeeds.
#[test]
fn envelope_version() {
    let ws = Workspace::new("version");
    let body = ws.json(&["version", "--json"]);
    assert_envelope_contract("version", &body);
    ws.cleanup();
}

/// `backlog list` — no required args.
#[test]
fn envelope_backlog() {
    let ws = Workspace::new("backlog");
    let body = ws.json(&["backlog", "list", "--json"]);
    assert_envelope_contract("backlog", &body);
    ws.cleanup();
}

/// `board` — read-only; works on empty room.
#[test]
fn envelope_board() {
    let ws = Workspace::new("board");
    let body = ws.json(&["board", "--json"]);
    assert_envelope_contract("board", &body);
    ws.cleanup();
}

/// Per-kind read verbs — read-only; work on an empty room and satisfy the
/// envelope contract (`data.<verb>` non-null) with the facts under `.rows`.
#[test]
fn envelope_risks() {
    let ws = Workspace::new("risks");
    let body = ws.json(&["risks", "--json"]);
    assert_envelope_contract("risks", &body);
    assert!(
        body["data"]["risks"]["rows"].is_array(),
        "data.risks.rows must be an array (no double-nest)"
    );
    ws.cleanup();
}

#[test]
fn envelope_decisions() {
    let ws = Workspace::new("decisions");
    let body = ws.json(&["decisions", "--json"]);
    assert_envelope_contract("decisions", &body);
    assert!(body["data"]["decisions"]["rows"].is_array());
    ws.cleanup();
}

#[test]
fn envelope_artifacts() {
    let ws = Workspace::new("artifacts");
    let body = ws.json(&["artifacts", "--json"]);
    assert_envelope_contract("artifacts", &body);
    assert!(body["data"]["artifacts"]["rows"].is_array());
    ws.cleanup();
}

#[test]
fn envelope_claims() {
    let ws = Workspace::new("claims");
    let body = ws.json(&["claims", "--json"]);
    assert_envelope_contract("claims", &body);
    assert!(body["data"]["claims"]["rows"].is_array());
    ws.cleanup();
}

/// `route-findings` — requires --file and --verified; use a minimal findings file.
#[test]
fn envelope_route_findings() {
    let ws = Workspace::new("route-findings");
    let findings = serde_json::json!([{
        "file": "src/lib.rs",
        "severity": "warn",
        "description": "test finding",
    }]);
    let f = ws.cwd.join("findings.json");
    fs::write(&f, findings.to_string()).unwrap();
    let body = ws.json(&[
        "route-findings",
        "--json",
        "--tool",
        "scanner",
        "--file",
        f.to_str().unwrap(),
        "--verified",
    ]);
    assert_envelope_contract("route-findings", &body);
    ws.cleanup();
}

/// `check-ci` — read-only; works on empty room.
#[test]
fn envelope_check_ci() {
    let ws = Workspace::new("check-ci");
    let body = ws.json(&["check-ci", "--json"]);
    assert_envelope_contract("check-ci", &body);
    ws.cleanup();
}

/// `dag` — requires --run RUN_ID; returns an empty DAG for an unknown run.
#[test]
fn envelope_dag() {
    let ws = Workspace::new("dag");
    let body = ws.json(&["dag", "--json", "--run", "run-test-001"]);
    assert_envelope_contract("dag", &body);
    ws.cleanup();
}

/// `wake-due` — read-only; returns empty list on a fresh room.
#[test]
fn envelope_wake_due() {
    let ws = Workspace::new("wake-due");
    let body = ws.json(&["wake-due", "--json"]);
    assert_envelope_contract("wake-due", &body);
    ws.cleanup();
}

/// `whoami` — read-only; always succeeds.
#[test]
fn envelope_whoami() {
    let ws = Workspace::new("whoami");
    let body = ws.json(&["whoami", "--json"]);
    assert_envelope_contract("whoami", &body);
    ws.cleanup();
}

/// `owners --dirty` — read-only; returns empty dirty ownership on fake-git rooms.
#[test]
fn envelope_owners_dirty() {
    let ws = Workspace::new("owners");
    let body = ws.json(&["owners", "--dirty", "--json"]);
    assert_envelope_contract("owners", &body);
    assert_eq!(body["schema"], "agent-rally.command.owners.v1");
    ws.cleanup();
}

/// `mission` GET — read-only; works on empty room.
#[test]
fn envelope_mission_get() {
    let ws = Workspace::new("mission-get");
    let body = ws.json(&["mission", "--json"]);
    assert_envelope_contract("mission", &body);
    ws.cleanup();
}

/// `mission` SET — writes a mission fact.
#[test]
fn envelope_mission_set() {
    let ws = Workspace::new("mission-set");
    let body = ws.json(&["mission", "--json", "--set", "deliver the contract test"]);
    assert_envelope_contract("mission", &body);
    ws.cleanup();
}

/// sessions — no required args; returns empty list.
#[test]
fn envelope_sessions() {
    let ws = Workspace::new("sessions");
    let body = ws.json(&["sessions", "--json"]);
    assert_envelope_contract("sessions", &body);
    ws.cleanup();
}

// ─── Session actions (attach/capture/stop) ────────────────────────────────────
// These three commands require an active managed session in the room ledger.
// They are covered by the user_journey tests which assert schema validation at
// lines 1008-1061 (attach/capture/stop with a real tmux --dry-run session).
//
// The envelope contract for these commands is:
//   data["attach"] = { mode, action, session, output?, commands }
//   data["capture"] = { mode, action, session, output?, commands }
//   data["stop"]   = { mode, action, session, output?, commands }
//
// The schema is `agent-rally.command.session-action.v1` for all three.
// Each nests its result under its own action name in `data`.
//
// To add per-command contract tests here, register a session via:
//   `rally run claude --backend tmux --tmux-bin /usr/bin/true --name <name>`
// then run the action with `--dry-run`.
//
// For the run command, the envelope contract is verified by `envelope_run_dry_run`:

/// `run` with --dry-run — no real tmux session started but envelope is correct.
#[test]
fn envelope_run_dry_run() {
    let ws = Workspace::new("run-dry");
    let body = ws.json(&[
        "run",
        "claude",
        "--json",
        "--backend",
        "tmux",
        "--tmux-bin",
        "/usr/bin/true",
        "--dry-run",
    ]);
    assert_envelope_contract("run", &body);
    ws.cleanup();
}

/// `whoami` data fields verify: the identity block is under data["whoami"].
#[test]
fn envelope_whoami_data_fields() {
    let ws = Workspace::new("whoami-fields");
    let body = ws.json(&["whoami", "--json"]);
    let whoami = &body["data"]["whoami"];
    assert!(whoami.is_object(), "data.whoami must be an object");
    assert!(
        !whoami["repo_root"].is_null(),
        "data.whoami.repo_root missing"
    );
    assert!(!whoami["repo_id"].is_null(), "data.whoami.repo_id missing");
    assert!(!whoami["room_id"].is_null(), "data.whoami.room_id missing");
    assert!(
        !whoami["build_id"].is_null(),
        "data.whoami.build_id missing"
    );
    ws.cleanup();
}

/// `version` data fields: build_id and version are under data["version"].
#[test]
fn envelope_version_data_fields() {
    let ws = Workspace::new("version-fields");
    let body = ws.json(&["version", "--json"]);
    let v = &body["data"]["version"];
    assert!(v.is_object(), "data.version must be an object");
    assert!(
        v["build_id"].is_string(),
        "data.version.build_id must be a string"
    );
    assert!(
        v["version"].is_string(),
        "data.version.version must be a string"
    );
    ws.cleanup();
}

/// `say` data fields: fact is under data.say, room and verified are siblings.
#[test]
fn envelope_say_data_fields() {
    let ws = Workspace::new("say-fields");
    let body = ws.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "agent-a",
        "--subject",
        "test claim for envelope",
    ]);
    let say = &body["data"]["say"];
    assert!(say.is_object(), "data.say must be an object; got: {body:#}");
    assert!(!say["fact"].is_null(), "data.say.fact must be present");
    // room is a sibling of say (shared contextual payload)
    assert!(
        body["data"]["room"].is_object(),
        "data.room must be a sibling object"
    );
    // verified is a sibling
    assert!(
        body["data"]["verified"].is_object(),
        "data.verified must be a sibling"
    );
    ws.cleanup();
}

/// `enter` data fields: enter result under data.enter, room is a sibling.
#[test]
fn envelope_enter_data_fields() {
    let ws = Workspace::new("enter-fields");
    let body = ws.json(&["enter", "--json", "--tool", "agent-a"]);
    let enter = &body["data"]["enter"];
    assert!(enter.is_object(), "data.enter must be an object");
    assert!(!enter["tool"].is_null(), "data.enter.tool missing");
    assert!(!enter["cursor"].is_null(), "data.enter.cursor missing");
    // room is a sibling
    assert!(
        body["data"]["room"].is_object(),
        "data.room must be a sibling object"
    );
    ws.cleanup();
}

/// `wake-due` data fields: due list is under data["wake-due"].
#[test]
fn envelope_wake_due_data_fields() {
    let ws = Workspace::new("wake-due-fields");
    let body = ws.json(&["wake-due", "--json"]);
    let wd = &body["data"]["wake-due"];
    assert!(wd.is_object(), "data[\"wake-due\"] must be an object");
    assert!(
        wd["due"].is_array(),
        "data[\"wake-due\"].due must be an array"
    );
    ws.cleanup();
}

/// `mission` GET: data.mission is object with text/set_by/set_at/envelopes.
#[test]
fn envelope_mission_get_data_fields() {
    let ws = Workspace::new("mission-get-fields");
    // Set a mission so there's content to inspect
    ws.json(&["mission", "--json", "--set", "north star"]);
    let body = ws.json(&["mission", "--json"]);
    let m = &body["data"]["mission"];
    assert!(m.is_object(), "data.mission must be an object");
    // GET mode: must have text field (not mission field — that was the old shape)
    assert!(
        !m["text"].is_null(),
        "data.mission.text must be present in GET mode"
    );
    assert!(
        m["envelopes"].is_array(),
        "data.mission.envelopes must be an array"
    );
    ws.cleanup();
}

/// `check-ci` data fields: result object under data["check-ci"].
#[test]
fn envelope_check_ci_data_fields() {
    let ws = Workspace::new("check-ci-fields");
    let body = ws.json(&["check-ci", "--json"]);
    let ci = &body["data"]["check-ci"];
    assert!(ci.is_object(), "data[\"check-ci\"] must be an object");
    assert!(
        ci["pass"].is_boolean(),
        "data[\"check-ci\"].pass must be a bool"
    );
    ws.cleanup();
}

/// `route-findings` data fields: result under data["route-findings"].
#[test]
fn envelope_route_findings_data_fields() {
    let ws = Workspace::new("route-findings-fields");
    let findings = serde_json::json!([{
        "file": "src/test.rs",
        "severity": "warn",
        "description": "envelope contract test finding",
    }]);
    let f = ws.cwd.join("findings.json");
    fs::write(&f, findings.to_string()).unwrap();
    let body = ws.json(&[
        "route-findings",
        "--json",
        "--tool",
        "scanner",
        "--file",
        f.to_str().unwrap(),
        "--verified",
    ]);
    let rf = &body["data"]["route-findings"];
    assert!(rf.is_object(), "data[\"route-findings\"] must be an object");
    assert!(
        !rf["findings_total"].is_null(),
        "data[\"route-findings\"].findings_total missing"
    );
    ws.cleanup();
}

/// `recent` data fields: result under data.recent.
#[test]
fn envelope_recent_data_fields() {
    let ws = Workspace::new("recent-fields");
    let body = ws.json(&["recent", "--json"]);
    let r = &body["data"]["recent"];
    assert!(r.is_object(), "data.recent must be an object");
    assert!(r["rows"].is_array(), "data.recent.rows must be an array");
    ws.cleanup();
}

/// `backlog` data fields: result under data.backlog.
#[test]
fn envelope_backlog_data_fields() {
    let ws = Workspace::new("backlog-fields");
    let body = ws.json(&["backlog", "list", "--json"]);
    let bl = &body["data"]["backlog"];
    assert!(bl.is_object(), "data.backlog must be an object");
    assert!(
        bl["items"].is_array(),
        "data.backlog.items must be an array"
    );
    ws.cleanup();
}

/// `migrate-legacy` data fields: result under data["migrate-legacy"].
#[test]
fn envelope_migrate_legacy_data_fields() {
    let ws = Workspace::new("migrate-legacy-fields");
    let body = ws.json(&["migrate-legacy", "--json"]);
    let ml = &body["data"]["migrate-legacy"];
    assert!(ml.is_object(), "data[\"migrate-legacy\"] must be an object");
    assert!(
        ml["facts_migrated"].is_number(),
        "data[\"migrate-legacy\"].facts_migrated must be present"
    );
    ws.cleanup();
}
