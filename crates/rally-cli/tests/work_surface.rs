// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the work-surface group:
//! - backlog add/list
//! - next returns a deps-met backlog item + excludes dep-blocked
//! - board projects lanes
//! - route-findings maps a finding to the claim owner

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
        let cwd = std::env::temp_dir().join(format!("ws-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("ws-{name}-{nanos}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cmd {:?} failed\nstderr: {}\nstdout: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

// ─── Backlog add / list ───────────────────────────────────────────────────────

#[test]
fn backlog_add_and_list_round_trip() {
    let ws = Workspace::new("backlog-add-list");

    // Add a backlog item
    let add = ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "task-1",
        "--intent", "implement the widget",
        "--owns", "crates/widget/src/lib.rs",
    ]);
    assert_eq!(add["ok"], true);
    assert_eq!(add["data"]["backlog"]["action"], "add");
    let added_id = add["data"]["backlog"]["added"]["event_id"].as_str().unwrap();
    assert!(!added_id.is_empty());

    // The items array in the add response should include our item
    let items = add["data"]["backlog"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "task-1");
    assert_eq!(items[0]["intent"], "implement the widget");
    assert_eq!(items[0]["status"], "open");

    // List should return the same item
    let list = ws.json(&["backlog", "list", "--json"]);
    assert_eq!(list["ok"], true);
    assert_eq!(list["data"]["backlog"]["action"], "list");
    let list_items = list["data"]["backlog"]["items"].as_array().unwrap();
    assert_eq!(list_items.len(), 1);
    assert_eq!(list_items[0]["id"], "task-1");
    assert_eq!(list_items[0]["owns"][0], "crates/widget/src/lib.rs");

    ws.cleanup();
}

#[test]
fn backlog_add_with_depends_on() {
    let ws = Workspace::new("backlog-deps");

    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "dep-task",
        "--intent", "prerequisite work",
    ]);
    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "main-task",
        "--intent", "depends on dep-task",
        "--depends-on", "dep-task",
    ]);

    let list = ws.json(&["backlog", "list", "--json"]);
    let items = list["data"]["backlog"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let main = items.iter().find(|i| i["id"] == "main-task").unwrap();
    assert_eq!(main["depends_on"][0], "dep-task");

    ws.cleanup();
}

// ─── next returns dep-met backlog item + excludes dep-blocked ─────────────────

#[test]
fn next_returns_suggested_backlog_item_when_deps_met() {
    let ws = Workspace::new("next-backlog-depmet");

    // Add a backlog item with no deps — should surface in next
    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "ready-task",
        "--intent", "no deps, ready to pick up",
        "--owns", "src/ready.rs",
    ]);

    // Add a dep-blocked item — should NOT surface
    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "blocked-task",
        "--intent", "depends on missing",
        "--depends-on", "ready-task",
    ]);

    let next = ws.json(&["next", "--json", "--tool", "tool-a"]);
    assert_eq!(next["ok"], true);

    let suggested = next["data"]["next"]["suggested_backlog_items"]
        .as_array()
        .unwrap();
    // ready-task has no deps → should appear
    assert!(
        suggested.iter().any(|i| i["id"] == "ready-task"),
        "ready-task must appear in suggested_backlog_items; got: {suggested:?}"
    );
    // blocked-task depends on ready-task (not done) → must NOT appear
    assert!(
        !suggested.iter().any(|i| i["id"] == "blocked-task"),
        "blocked-task must NOT appear (dep not satisfied)"
    );

    ws.cleanup();
}

#[test]
fn next_excludes_backlog_item_whose_path_is_claimed() {
    let ws = Workspace::new("next-backlog-claimed");

    // Another tool claims the path the backlog item owns
    ws.json(&[
        "say", "claim",
        "--json",
        "--tool", "other-tool",
        "--path", "src/owned.rs",
        "--subject", "other-tool owns this",
    ]);

    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "claimed-path-task",
        "--intent", "owns a path claimed by another",
        "--owns", "src/owned.rs",
    ]);

    let next = ws.json(&["next", "--json", "--tool", "tool-a"]);
    let empty = vec![];
    let suggested = next["data"]["next"]["suggested_backlog_items"]
        .as_array()
        .unwrap_or(&empty);
    assert!(
        !suggested.iter().any(|i| i["id"] == "claimed-path-task"),
        "claimed-path-task must NOT surface since its path is actively claimed"
    );

    ws.cleanup();
}

// ─── Board lane projection ────────────────────────────────────────────────────

#[test]
fn board_projects_claim_lanes_and_backlog() {
    let ws = Workspace::new("board-lanes");

    // An in-flight claim
    ws.json(&[
        "say", "claim",
        "--json",
        "--tool", "tool-a",
        "--path", "src/active.rs",
        "--subject", "active work",
    ]);

    // A backlog item
    ws.json(&[
        "backlog", "add",
        "--json",
        "--tool", "tool-a",
        "--id", "board-task",
        "--intent", "pending backlog item",
    ]);

    let board = ws.json(&["board", "--json"]);
    assert_eq!(board["ok"], true);

    let lanes = board["data"]["board"]["lanes"].as_array().unwrap();
    assert_eq!(lanes.len(), 1, "one claim in lanes");
    assert_eq!(lanes[0]["status"], "in_flight");
    assert_eq!(lanes[0]["owner"], "tool-a");

    let backlog_open = board["data"]["board"]["backlog"]["open"].as_array().unwrap();
    assert_eq!(backlog_open.len(), 1);
    assert_eq!(backlog_open[0]["id"], "board-task");

    // Delta must be non-empty
    let delta = board["data"]["board"]["delta"].as_array().unwrap();
    assert!(!delta.is_empty(), "delta must have at least one entry");

    ws.cleanup();
}

// ─── Route findings ───────────────────────────────────────────────────────────

#[test]
fn route_findings_maps_finding_to_claim_owner() {
    let ws = Workspace::new("route-findings-owned");

    // A tool claims a path
    ws.json(&[
        "say", "claim",
        "--json",
        "--tool", "owner-tool",
        "--path", "src/lib.rs",
        "--subject", "owns src/lib.rs",
    ]);

    // Write a findings file
    let findings = serde_json::json!([{
        "file": "src/lib.rs",
        "severity": "error",
        "description": "null pointer dereference at line 42",
        "evidence": ["static analysis pass 1"]
    }]);
    let findings_path = ws.cwd.join("findings.json");
    fs::write(&findings_path, findings.to_string()).unwrap();

    // Route without --verified → should fail
    let no_verified = ws.run(&[
        "route-findings",
        "--json",
        "--tool", "scanner",
        "--file", findings_path.to_str().unwrap(),
    ]);
    assert!(!no_verified.status.success(), "must refuse without --verified");

    // Route with --verified
    let routed = ws.json(&[
        "route-findings",
        "--json",
        "--tool", "scanner",
        "--file", findings_path.to_str().unwrap(),
        "--verified",
    ]);
    assert_eq!(routed["ok"], true);
    let routing = &routed["data"]["route-findings"];
    assert_eq!(routing["findings_total"], 1);
    assert_eq!(routing["routed"], 1);
    assert_eq!(routing["unowned"], 0);
    let rf = &routing["routed_findings"][0];
    assert_eq!(rf["routed_to"], "owner-tool");
    assert_eq!(rf["fact_kind"], "handoff");
    assert_eq!(rf["unowned"], false);

    ws.cleanup();
}

#[test]
fn route_findings_unowned_path_emits_risk() {
    let ws = Workspace::new("route-findings-unowned");

    // No active claims in the room
    let findings = serde_json::json!([{
        "file": "unclaimed/path.rs",
        "severity": "warn",
        "description": "unused variable",
        "evidence": []
    }]);
    let findings_path = ws.cwd.join("findings.json");
    fs::write(&findings_path, findings.to_string()).unwrap();

    let routed = ws.json(&[
        "route-findings",
        "--json",
        "--tool", "scanner",
        "--file", findings_path.to_str().unwrap(),
        "--verified",
    ]);
    let routing = &routed["data"]["route-findings"];
    assert_eq!(routing["findings_total"], 1);
    assert_eq!(routing["routed"], 0);
    assert_eq!(routing["unowned"], 1);
    let rf = &routing["routed_findings"][0];
    assert_eq!(rf["fact_kind"], "risk");
    assert_eq!(rf["unowned"], true);

    ws.cleanup();
}

// ─── Help exits ──────────────────────────────────────────────────────────────

#[test]
fn work_surface_help_exits_zero() {
    let ws = Workspace::new("help-exits");
    for cmd in &["backlog", "board", "route-findings"] {
        let out = ws.run(&[cmd, "--help"]);
        assert!(
            out.status.success(),
            "{cmd} --help must exit 0\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    ws.cleanup();
}
