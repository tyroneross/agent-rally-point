// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

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

    fn log_events(&self) -> Vec<Value> {
        let log_dir = self.cwd.join(".rally/log");
        let log_path = fs::read_dir(log_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .expect("rally jsonl log exists");
        fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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

#[test]
fn before_write_gate_cannot_be_bypassed_by_warn_mode_missing_path_or_unknown_tool() {
    let workspace = Workspace::new("rally-before-write-gate");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude",
        "--path",
        "src/lib.rs",
        "--subject",
        "own lib",
    ]);
    let (warn_check, warn_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--path",
        "src/lib.rs",
    ]);
    assert!(warn_output.status.success());
    assert_eq!(warn_check["data"]["check"]["mode"], "warn");
    assert_eq!(warn_check["data"]["check"]["allow"], false);
    assert!(
        warn_check["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "claimed-path")
    );

    // `claude` claimed first above, so it holds the first-join lead seat, and
    // its unscoped blocker is a genuine room-wide freeze.
    //
    // RC-038 changed what an unscoped blocker means. It used to hard-stop every
    // write from every writer, so this assertion passed for a reason it did not
    // state — the blocker's AUTHOR was never checked, and a rogue peer's
    // identical fact denied the whole room. The freeze is now authority-gated
    // (see `check::check_before_write`) and reports under `room-freeze`, so
    // this test keeps its original subject (strict mode must still deny) while
    // no longer certifying the denial-of-service alongside it.
    workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "claude",
        "--subject",
        "global freeze",
    ]);
    let (missing_path_check, missing_path_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex",
        "--strict",
    ]);
    assert_eq!(missing_path_output.status.code(), Some(4));
    assert_eq!(missing_path_check["data"]["check"]["allow"], false);
    let missing_path_findings = missing_path_check["data"]["check"]["findings"]
        .as_array()
        .unwrap();
    assert!(
        missing_path_findings
            .iter()
            .any(|finding| finding["code"] == "missing-path")
    );
    assert!(
        missing_path_findings
            .iter()
            .any(|finding| finding["code"] == "room-freeze")
    );

    let omitted_tool =
        workspace.output(&["check", "before-write", "--json", "--path", "src/lib.rs"]);
    assert_eq!(omitted_tool.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&omitted_tool.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("check before-write requires --tool <tool>")
    );

    let unknown_tool = workspace.output(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "unknown",
        "--path",
        "src/lib.rs",
    ]);
    assert_eq!(unknown_tool.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&unknown_tool.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("requires a real --tool")
    );

    workspace.cleanup();
}

/// RC-038 end-to-end adversarial control, through the real binary.
///
/// Live-reproduced before the fix: `rally say blocker` with no scope, from a
/// peer holding no authority, flipped `check before-write` from `allow: true`
/// to `allow: false` for every agent and every path, and `--strict` returned
/// exit 4 — which the coordination hook translates to
/// `permissionDecision: "deny"` under `RALLY_HOOK_STRICT=1`. One unauthenticated
/// fact, writable by any peer or by any commit touching the git-tracked ledger,
/// stopped every edit in the room.
///
/// Revert the authority gate in `check::check_before_write` and this fails.
#[test]
fn unscoped_blocker_from_a_non_lead_does_not_deny_every_write() {
    let workspace = Workspace::new("rally-rc038-unscoped-blocker");

    // `lead_agent` enters first and takes the first-join lead seat.
    workspace.json(&["enter", "--json", "--tool", "lead_agent"]);
    workspace.json(&["enter", "--json", "--tool", "rogue"]);

    let (before, _) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "lead_agent",
        "--path",
        "src/lib.rs",
    ]);
    assert_eq!(
        before["data"]["check"]["allow"], true,
        "baseline: the room must allow this write before the blocker lands"
    );

    workspace.json(&[
        "say",
        "blocker",
        "--json",
        "--tool",
        "rogue",
        "--subject",
        "everything is blocked",
    ]);

    let (after, after_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "lead_agent",
        "--path",
        "src/lib.rs",
        "--strict",
    ]);
    assert_eq!(
        after["data"]["check"]["allow"], true,
        "a non-lead's unscoped blocker must not deny an unrelated write"
    );
    assert_eq!(
        after_output.status.code(),
        Some(0),
        "strict mode must not turn a non-lead's unscoped blocker into a hard deny"
    );
    let findings = after["data"]["check"]["findings"].as_array().unwrap();
    let finding = findings
        .iter()
        .find(|finding| finding["code"] == "unscoped-blocker")
        .expect("the blocker must still be surfaced to the agent, just not as a stop");
    assert_eq!(finding["severity"], "warn");
    assert!(
        finding["message"]
            .as_str()
            .unwrap()
            .contains("everything is blocked"),
        "the agent must still be able to read what the blocker said"
    );

    workspace.cleanup();
}

/// RC-037 end-to-end adversarial control, through the real binary.
///
/// Live-reproduced before the fix: one `rally say claim --scope workspace:zzz`
/// made every later claim of any path, by any other agent, fail to append —
/// permanently, because nothing expires a claim whose owner keeps posting
/// presence. The rejection also named a file the coarse claimant did not own.
///
/// Revert `resource_scope::root_contains` and the first assertion fails;
/// revert `claim_authority::breadth_violation` and the third fails.
#[test]
fn coarse_claim_does_not_lock_the_room_out_of_claiming() {
    let workspace = Workspace::new("rally-rc037-coarse-claim");

    workspace.json(&["enter", "--json", "--tool", "lead_agent"]);
    workspace.json(&["enter", "--json", "--tool", "rogue"]);

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "rogue",
        "--scope",
        "workspace:zzz",
        "--subject",
        "coarse claim",
    ]);

    // 1. The lockout is gone: an opaque workspace identifier says nothing about
    //    whether src/lib.rs lives inside it, so it no longer conflicts.
    let peer_claim = workspace.output(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "lead_agent",
        "--path",
        "src/lib.rs",
        "--subject",
        "normal work",
    ]);
    assert!(
        peer_claim.status.success(),
        "a coarse claim by one agent must not block every other claim in the room; stderr: {}",
        String::from_utf8_lossy(&peer_claim.stderr)
    );

    // 2. A genuine same-path conflict is still rejected, and the message now
    //    names the scope the OWNER holds rather than the one the claimant asked
    //    for.
    let colliding = workspace.output(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "rogue",
        "--path",
        "src/lib.rs",
        "--subject",
        "collide",
    ]);
    assert_eq!(colliding.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&colliding.stderr).unwrap();
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("lead_agent holds file:src/lib.rs"),
        "the rejection must name the real owner and the scope it really holds; got: {message}"
    );

    // 3. Room-wide breadth is still expressible, and still gated: the lead may
    //    take it, a peer may not.
    let rogue_wildcard = workspace.output(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "rogue",
        "--scope",
        "workspace:*",
        "--subject",
        "room-wide grab",
    ]);
    assert_eq!(rogue_wildcard.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&rogue_wildcard.stderr).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("only the lead may hold it")
    );

    workspace.cleanup();
}

#[test]
fn before_write_live_sequence_reproduces_auto_claim_facts() {
    let workspace = Workspace::new("rally-before-write-live-sequence");
    fs::create_dir_all(workspace.cwd.join("src")).unwrap();
    fs::write(workspace.cwd.join("src/lib.rs"), "").unwrap();

    let (check, check_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/lib.rs",
    ]);
    assert!(check_output.status.success());
    assert_eq!(check["data"]["check"]["allow"], true);
    assert_eq!(check["data"]["check"]["agent_visible"]["present"], false);

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/lib.rs",
        "--subject",
        "auto-claim src/lib.rs",
    ]);

    let events = workspace.log_events();
    assert!(
        events.iter().any(|event| {
            event["event_type"] == "presence"
                && event["payload"]["tool"] == "codex:01"
                && event["payload"]["subject"] == "agent presence: codex:01"
        }),
        "live check must lazy-register codex presence: {events:#?}"
    );
    assert!(
        events.iter().any(|event| {
            event["event_type"] == "claim"
                && event["payload"]["tool"] == "codex:01"
                && event["payload"]["subject"] == "auto-claim src/lib.rs"
                && event["payload"]["scope"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|scope| scope == "file:src/lib.rs")
        }),
        "explicit live claim must reproduce the old auto-claim path fact: {events:#?}"
    );

    workspace.cleanup();
}

#[test]
fn before_write_live_sequence_does_not_claim_when_gate_stops() {
    let workspace = Workspace::new("rally-before-write-live-blocked");
    fs::create_dir_all(workspace.cwd.join("src")).unwrap();
    fs::write(workspace.cwd.join("src/lib.rs"), "").unwrap();

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "peer",
        "--path",
        "src/lib.rs",
        "--subject",
        "peer owns lib",
    ]);

    let (check, check_output) = workspace.json_with_status(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/lib.rs",
    ]);
    assert!(check_output.status.success());
    assert_eq!(check["data"]["check"]["allow"], false);
    assert_eq!(check["data"]["check"]["agent_visible"]["present"], true);
    assert!(
        check["data"]["check"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "claimed-path")
    );

    let events = workspace.log_events();
    assert!(
        events.iter().any(|event| {
            event["event_type"] == "presence" && event["payload"]["tool"] == "codex:01"
        }),
        "blocked check still reproduces codex presence: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| {
            event["event_type"] == "claim" && event["payload"]["tool"] == "codex:01"
        }),
        "wrapper-compatible sequence must not claim when the gate stops: {events:#?}"
    );

    workspace.cleanup();
}

// B10 — canonical-path matching integration tests

/// B10a (lessons case): tool X claims `crates/rally-cli/src/lib.rs`; tool Y checks
/// `src/lib.rs`.  These are the same 2-component suffix.  The gate must emit an
/// `ambiguous-path-collision` WARN — not silently allow the write.
#[test]
fn b10_suffix_collision_lessons_case_is_flagged() {
    let workspace = Workspace::new("b10-lessons-case");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code:01",
        "--path",
        "crates/rally-cli/src/lib.rs",
        "--subject",
        "own rally-cli lib",
    ]);

    let result = workspace.json(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/lib.rs",
    ]);

    let findings = result["data"]["check"]["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["code"] == "ambiguous-path-collision"),
        "must emit ambiguous-path-collision when suffix collides; got findings: {findings:#?}"
    );
    // Must NOT be silently allowed — allow should be true only when there are no stop findings,
    // but a WARN does not block, so allow=true is expected here (the signal is the finding itself).
    let has_code = findings
        .iter()
        .any(|f| f["code"] == "ambiguous-path-collision" && f["severity"] == "warn");
    assert!(
        has_code,
        "ambiguous-path-collision must have severity=warn; findings: {findings:#?}"
    );

    workspace.cleanup();
}

/// B10b (absolute-vs-relative): tool X claims `src/x` by relative path; tool Y checks
/// an absolute path pointing to the same file.  Must resolve to an exact match → STOP
/// `claimed-path`.
#[test]
fn b10_absolute_vs_relative_exact_match_is_stop() {
    let workspace = Workspace::new("b10-abs-vs-rel");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code:01",
        "--path",
        "src/x.rs",
        "--subject",
        "own src/x",
    ]);

    // Construct the absolute path from the workspace cwd.
    let abs_path = workspace.cwd.join("src").join("x.rs");
    let abs_str = abs_path.to_str().unwrap();

    let result = workspace.json(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        abs_str,
    ]);

    let findings = result["data"]["check"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["code"] == "claimed-path"),
        "absolute path pointing to claimed relative path must produce claimed-path STOP; \
         findings: {findings:#?}"
    );
    assert_eq!(
        result["data"]["check"]["allow"], false,
        "allow must be false when a STOP finding is present"
    );

    workspace.cleanup();
}

/// B10c (single-component basename): tool X claims `src/lib.rs`; tool Y checks bare
/// `lib.rs`.  Single-component baseline — must NOT flag as ambiguous-path-collision.
#[test]
fn b10_single_component_basename_does_not_flag() {
    let workspace = Workspace::new("b10-single-component");

    workspace.json(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code:01",
        "--path",
        "src/lib.rs",
        "--subject",
        "own src/lib",
    ]);

    let result = workspace.json(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "lib.rs",
    ]);

    let findings = result["data"]["check"]["findings"].as_array().unwrap();
    assert!(
        !findings
            .iter()
            .any(|f| f["code"] == "ambiguous-path-collision"),
        "single-component basename must not trigger ambiguous-path-collision; \
         findings: {findings:#?}"
    );
    assert!(
        !findings.iter().any(|f| f["code"] == "claimed-path"),
        "bare 'lib.rs' must not be an exact/dir-prefix match for 'src/lib.rs'; \
         findings: {findings:#?}"
    );

    workspace.cleanup();
}
