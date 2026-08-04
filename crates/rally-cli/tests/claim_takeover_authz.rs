// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial controls for claim-takeover authorization (RC-029).
//!
//! A claim only means something if another agent cannot take it away. Before
//! this suite, one could: `claim_authority::later_fact_refs_claim` closes an
//! active claim on ANY later `Resolve | Release | Receipt | ClaimExpired`
//! carrying its `event_id`, regardless of author, and the 30-minute / 2-hour
//! takeover authorization lived only in `command_release_by_path` — which
//! `command_say` reaches ONLY when `--ref` is absent. Passing `--ref` walked
//! past the control entirely.
//!
//! Reproduced end to end against a claim seconds old: a non-owner resolved it,
//! `active_claims` went 1 -> 0, the non-owner re-claimed the same path, and the
//! room reported the attacker as owner.
//!
//! Every test here performs the hostile action and asserts REJECTION. A test
//! that merely exercises the happy path would not have caught this.

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
        let cwd = std::env::temp_dir().join(format!("cta-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("cta-{name}-{nanos}-home"));
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

    /// Parse the command's JSON envelope. Refusals are written to STDERR (the
    /// CLI's error channel) while successes go to stdout, so a suite that
    /// asserts on rejection has to read both — reading stdout alone would make
    /// every refusal look like a crash.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
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

    /// Claim `path` for `tool` and return the claim's `event_id`.
    fn claim(&self, tool: &str, path: &str) -> String {
        let v = self.json(&[
            "say",
            "claim",
            "--tool",
            tool,
            "--path",
            path,
            "--subject",
            "owns it",
            "--json",
        ]);
        assert_eq!(v["ok"], Value::Bool(true), "claim should succeed: {v}");
        v["data"]["say"]["fact"]["event_id"]
            .as_str()
            .expect("claim fact carries an event_id")
            .to_string()
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
    v["error"].as_str().unwrap_or_default().to_string()
}

/// THE defect. A non-owner resolves a live claim by ref and takes the path.
#[test]
fn non_owner_cannot_resolve_a_live_claim_by_ref() {
    let ws = Workspace::new("resolve-strip");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "resolve",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "strip it",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(false),
        "a non-owner resolving a live claim must be REFUSED, got: {v}"
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

/// Same bypass through `release --ref`, which shares the close projection.
#[test]
fn non_owner_cannot_release_a_live_claim_by_ref() {
    let ws = Workspace::new("release-strip");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "release",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "strip it",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(false),
        "a non-owner releasing a live claim must be REFUSED, got: {v}"
    );
    assert_eq!(
        ws.active_claim_owners(),
        vec!["victim:01".to_string()],
        "the victim must still own the claim"
    );
    ws.cleanup();
}

/// The full attack, end to end: strip then seize. This is the assertion that
/// matters — not that one command errored, but that ownership did not move.
#[test]
fn a_stripped_claim_cannot_be_seized_by_the_stripper() {
    let ws = Workspace::new("strip-and-seize");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let _ = ws.json(&[
        "say",
        "resolve",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "strip",
        "--json",
    ]);
    let seize = ws.json(&[
        "say",
        "claim",
        "--tool",
        "codex:rogue",
        "--path",
        "src/lib.rs",
        "--subject",
        "mine now",
        "--json",
    ]);

    assert_eq!(
        seize["ok"],
        Value::Bool(false),
        "re-claiming the victim's path must still conflict, got: {seize}"
    );
    assert_eq!(
        ws.active_claim_owners(),
        vec!["victim:01".to_string()],
        "ownership must not have moved to the attacker"
    );
    ws.cleanup();
}

/// The gate must not break the normal path: releasing your OWN claim has no
/// time bar and must always work.
#[test]
fn owner_can_always_self_release_by_ref() {
    let ws = Workspace::new("self-release");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "release",
        "--tool",
        "victim:01",
        "--ref",
        &claim,
        "--subject",
        "done with it",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "self-release must succeed with no time bar, got: {v}"
    );
    assert!(
        ws.active_claim_owners().is_empty(),
        "the claim should be gone after self-release"
    );
    ws.cleanup();
}

/// Resolving a NON-claim (a blocker) by ref is unaffected — the gate applies
/// only to refs that name an active claim.
#[test]
fn resolving_a_peers_blocker_is_still_allowed() {
    let ws = Workspace::new("blocker-resolve");
    let v = ws.json(&[
        "say",
        "blocker",
        "--tool",
        "victim:01",
        "--subject",
        "ci is red",
        "--json",
    ]);
    let blocker = v["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let resolved = ws.json(&[
        "say",
        "resolve",
        "--tool",
        "codex:peer",
        "--ref",
        blocker,
        "--subject",
        "ci is green again",
        "--json",
    ]);

    assert_eq!(
        resolved["ok"],
        Value::Bool(true),
        "a peer resolving a blocker is normal coordination and must still work, got: {resolved}"
    );
    ws.cleanup();
}

/// The refusal must be actionable: it names the owner, the actor, and the
/// reclaim window, so a blocked agent knows what to do instead of retrying.
#[test]
fn refusal_names_owner_actor_and_reclaim_window() {
    let ws = Workspace::new("refusal-text");
    let claim = ws.claim("victim:01", "src/lib.rs");
    let v = ws.json(&[
        "say",
        "release",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "strip",
        "--json",
    ]);
    let err = error_text(&v);
    assert!(err.contains("victim:01"), "names the owner: {err}");
    assert!(err.contains("codex:rogue"), "names the actor: {err}");
    assert!(
        err.contains("30 minutes"),
        "names the size-scaled reclaim window: {err}"
    );
    ws.cleanup();
}
