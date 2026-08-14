// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end adversarial controls for the retraction/release authority family
//! (R1 and R5), driven through the real `rally` binary.
//!
//! `write_authority.rs` unit-tests the POLICY. This suite tests that the
//! shipped commands actually reach it, which is the half this defect class
//! keeps getting wrong: RC-029, ARP-R-01, ARP-R-02, R1, and R5 were all a
//! correct rule guarding one spelling of an action while the ledger accepted
//! another. A policy test passes in every one of those worlds.
//!
//! Every test here performs the hostile action and asserts REJECTION.

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
        let cwd = std::env::temp_dir().join(format!("rra-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("rra-{name}-{nanos}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .args(args)
            .output()
            .unwrap()
    }

    /// Every `rally` invocation is a fresh process, so an unpinned caller gets
    /// a fresh `sess:proc:...#live` identity — and the gate correctly reads two
    /// invocations by one tool as two SIBLING SESSIONS, which is not ownership.
    /// A test that means "the same agent, twice" has to say so.
    fn run_as_session(&self, session_id: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_SESSION_ID", session_id)
            .args(args)
            .output()
            .unwrap()
    }

    fn json_as_session(&self, session_id: &str, args: &[&str]) -> Value {
        let out = self.run_as_session(session_id, args);
        Self::parse_json(args, &out)
    }

    /// Refusals go to STDERR and successes to stdout, so a suite that asserts
    /// on rejection has to read both — reading stdout alone would make every
    /// refusal look like a crash.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        Self::parse_json(args, &out)
    }

    fn parse_json(args: &[&str], out: &Output) -> Value {
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {:?} did not emit JSON ({e})\nstderr: {}\nstdout: {}",
                args,
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            )
        })
    }

    /// Claim `path` for `tool` under a pinned session and return the claim's
    /// `event_id`.
    fn claim_as(&self, tool: &str, session_id: &str, path: &str) -> String {
        let v = self.json_as_session(
            session_id,
            &[
                "say",
                "claim",
                "--tool",
                tool,
                "--path",
                path,
                "--subject",
                "owns it",
                "--json",
            ],
        );
        assert_eq!(v["ok"], Value::Bool(true), "claim should succeed: {v}");
        v["data"]["say"]["fact"]["event_id"]
            .as_str()
            .expect("claim fact carries an event_id")
            .to_string()
    }

    /// Claim for a victim we never act as again — session identity is
    /// irrelevant when every later actor is a different tool.
    fn claim(&self, tool: &str, path: &str) -> String {
        self.claim_as(tool, &format!("sess:test:{tool}"), path)
    }

    fn active_claim_owners(&self) -> Vec<String> {
        let v = self.json(&["room", "--json"]);
        v["data"]["room"]["active_claims"]
            .as_array()
            .expect("active_claims is an array")
            .iter()
            .map(|c| c["tool"].as_str().unwrap_or("<none>").to_string())
            .collect()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn error_text(v: &Value) -> String {
    if let Some(error) = v["error"].as_str() {
        return error.to_string();
    }
    if let Some(message) = v["data"]["warning"]["message"].as_str() {
        return message.to_string();
    }
    let warnings = v["data"]["append_outcomes"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|outcome| outcome["warnings"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert_eq!(
        warnings.len(),
        1,
        "refusal JSON must carry exactly one unambiguous append warning: {v}"
    );
    warnings[0]["message"]
        .as_str()
        .expect("the sole append warning must carry a message")
        .to_string()
}

// ---------------------------------------------------------------- R1 --------

/// R1, THE defect, through the shipped command. `rally retract` drops its
/// target from every projection bucket, so pointing one at a live claim strips
/// the claim exactly as `release --ref` does — and the write-authority gate
/// never saw it, because a retraction is not one of the four closing kinds.
#[test]
fn a_non_owner_cannot_retract_a_live_claim() {
    let ws = Workspace::new("retract-strip");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "retract",
        &claim,
        "--tool",
        "codex:rogue",
        "--reason",
        "strip it",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(false),
        "a non-owner retracting a live claim must be REFUSED, got: {v}"
    );
    let err = error_text(&v);
    assert!(
        err.contains("victim:01") && err.contains("not the owner"),
        "the refusal must name the owner so the caller knows who to ask; got: {err}"
    );
    assert_eq!(
        ws.active_claim_owners(),
        vec!["victim:01".to_string()],
        "the victim must still own the claim"
    );
    ws.cleanup();
}

/// A second process wearing the owner's tool label is not the owner. Found by
/// this suite rather than reasoned about: the first draft of
/// `the_owner_may_retract_its_own_claim` did not pin `RALLY_SESSION_ID`, and the
/// gate refused it — correctly, because `--tool` is self-asserted and a shared
/// label is not an identity.
#[test]
fn a_sibling_session_wearing_the_owners_label_cannot_retract_the_claim() {
    let ws = Workspace::new("retract-sibling");
    let claim = ws.claim_as("victim:01", "sess:test:owner-one", "src/lib.rs");

    let v = ws.json_as_session(
        "sess:test:owner-TWO",
        &[
            "retract",
            &claim,
            "--tool",
            "victim:01",
            "--reason",
            "not actually mine",
            "--json",
        ],
    );
    assert_eq!(
        v["ok"],
        Value::Bool(false),
        "a sibling session must not inherit the owner's authority, got: {v}"
    );
    assert!(
        error_text(&v).contains("another victim:01 session"),
        "the refusal must say the label is not the identity; got: {}",
        error_text(&v)
    );
    assert_eq!(
        ws.active_claim_owners(),
        vec!["victim:01".to_string()],
        "the claim must survive"
    );
    ws.cleanup();
}

/// The other half of R1: retraction of a fact that is NOT an active claim stays
/// ungated. Retraction exists so an honest mistake stays fixable without asking
/// permission, and a wrong artifact harms nobody's write safety by being
/// withdrawn. Without this test, a later tightening could gate all retraction
/// and nothing would object.
#[test]
fn anyone_may_still_retract_a_non_claim_fact() {
    let ws = Workspace::new("retract-artifact");
    let v = ws.json(&[
        "say",
        "artifact",
        "--tool",
        "author:01",
        "--subject",
        "wrong port number",
        "--json",
    ]);
    let target = v["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("artifact carries an event_id")
        .to_string();

    let v = ws.json(&[
        "retract",
        &target,
        "--tool",
        "someone:else",
        "--reason",
        "the port is 8080",
        "--json",
    ]);
    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "retracting a non-claim fact must stay open to anyone, got: {v}"
    );
    ws.cleanup();
}

/// The owner withdrawing its own claim is the normal path and must not have
/// been broken by the gate.
#[test]
fn the_owner_may_retract_its_own_claim() {
    let ws = Workspace::new("retract-self");
    let owner = "sess:test:owner-one";
    let claim = ws.claim_as("victim:01", owner, "src/lib.rs");

    let v = ws.json_as_session(
        owner,
        &[
            "retract",
            &claim,
            "--tool",
            "victim:01",
            "--reason",
            "claimed the wrong file",
            "--json",
        ],
    );
    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "the owner must be able to withdraw its own claim, got: {v}"
    );
    assert!(
        ws.active_claim_owners().is_empty(),
        "the withdrawn claim must leave the active set"
    );
    ws.cleanup();
}

// ---------------------------------------------------------------- R5 --------

/// R5, THE defect, through the shipped command. A release closes every active
/// claim whose scope overlaps its own free-text `--scope`, while the close gate
/// authorized only the claim named by `--ref` and never read `fact.scope`. So
/// naming your OWN claim in `--ref` satisfied a gate that had nothing to do
/// with the claim actually being closed.
#[test]
fn a_release_cannot_sweep_a_live_peers_claim_by_scope() {
    let ws = Workspace::new("release-sweep");
    let _victim = ws.claim("victim:01", "src/victim.rs");
    let rogue_claim = ws.claim_as("codex:rogue", "sess:test:rogue", "src/rogue.rs");

    let v = ws.json_as_session(
        "sess:test:rogue",
        &[
            "say",
            "release",
            "--tool",
            "codex:rogue",
            "--ref",
            &rogue_claim,
            "--path",
            "src/victim.rs",
            "--subject",
            "done",
            "--json",
        ],
    );

    assert_eq!(
        v["ok"],
        Value::Bool(false),
        "a release sweeping a live peer's claim by scope must be REFUSED, got: {v}"
    );
    let err = error_text(&v);
    assert!(
        err.contains("victim:01"),
        "the refusal must name the claim the SWEEP would take, not the one in --ref; got: {err}"
    );

    let owners = ws.active_claim_owners();
    assert!(
        owners.contains(&"victim:01".to_string()),
        "the victim must still own its claim; owners: {owners:?}"
    );
    ws.cleanup();
}

/// The ordinary release — your own claim, by ref — must be untouched by the
/// sweep gate. Without this, the R5 fix could pass by refusing everything.
#[test]
fn the_ordinary_release_of_your_own_claim_still_works() {
    let ws = Workspace::new("release-self");
    let owner = "sess:test:owner-one";
    let claim = ws.claim_as("worker:01", owner, "src/mine.rs");

    let v = ws.json_as_session(
        owner,
        &[
            "say",
            "release",
            "--tool",
            "worker:01",
            "--ref",
            &claim,
            "--subject",
            "done",
            "--json",
        ],
    );
    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "releasing your own claim must still work, got: {v}"
    );
    assert!(
        ws.active_claim_owners().is_empty(),
        "the released claim must leave the active set"
    );
    ws.cleanup();
}
