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

// ---------------------------------------------------------------------------
// ARP-R-02 — the two closing kinds the first fix missed.
//
// The six tests above cover Resolve and Release. `claim_authority`'s own
// projection closes a claim on FOUR kinds, and `assert_claim_release_authorized`
// had exactly two call sites, so `Receipt` and `ClaimExpired` reached the ledger
// with no ownership check at all. Reproduced live against a claim seconds old
// with a 30-minute lease: `say receipt --tool rogue --ref <cid>` returned ok,
// `active_claims` went to 0, and rogue then claimed the path.
//
// WHY THE ORIGINAL SUITE MISSED IT — recorded here because the lesson is
// structural, not incidental. Every test above is an adversarial control and
// every one was mutation-validated: neutering the gate killed four of six, which
// is the correct signature. None of that could detect Receipt or ClaimExpired,
// because MUTATION VALIDATION ONLY GRADES CODE THAT EXISTS. There was no gate on
// those paths to mutate, so the technique reported a healthy result about the
// half of the problem it could see. Coverage of the ORACLE (does a test exist
// per closing kind?) is a different question from strength of the tests present,
// and only the first one would have caught this.
//
// The structural fix is upstream of these tests: the gate is now keyed off
// `claim_authority::closes_active_claim`, the same predicate the projection
// uses, so a fifth closing kind cannot add a fifth bypass. These tests are the
// evidence that the four known kinds are closed today.
// ---------------------------------------------------------------------------

/// `Receipt` closes a claim in the projection, so it must clear the same bar.
#[test]
fn non_owner_cannot_close_a_live_claim_with_a_receipt() {
    let ws = Workspace::new("receipt-strip");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "receipt",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "done",
        "--json",
    ]);

    assert_ne!(
        v["ok"],
        Value::Bool(true),
        "a receipt from a non-owner must not close the claim: {v}"
    );
    assert_eq!(
        ws.active_claim_owners(),
        vec!["victim:01".to_string()],
        "the claim must still be standing, and still be victim:01's"
    );
    ws.cleanup();
}

/// `ClaimExpired` likewise — this is the reaper's kind, and a rogue can spell it
/// just as easily as the reaper can.
#[test]
fn non_owner_cannot_close_a_live_claim_with_claim_expired() {
    let ws = Workspace::new("claim-expired-strip");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "claim.expired",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "expired",
        "--json",
    ]);

    assert_ne!(v["ok"], Value::Bool(true), "must be refused: {v}");
    assert_eq!(ws.active_claim_owners(), vec!["victim:01".to_string()]);
    ws.cleanup();
}

/// The ADJACENT move: `claim_expired` is an ALIAS for `claim.expired`
/// (`FactKind::from_str` accepts both). A gate keyed on the spelling rather
/// than the parsed kind would close one and leave the other open, which is the
/// exact shape of the defect this whole cycle is about.
#[test]
fn the_claim_expired_underscore_alias_is_gated_too() {
    let ws = Workspace::new("claim-expired-alias");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "claim_expired",
        "--tool",
        "codex:rogue",
        "--ref",
        &claim,
        "--subject",
        "expired",
        "--json",
    ]);

    assert_ne!(
        v["ok"],
        Value::Bool(true),
        "the alias must be refused too: {v}"
    );
    assert_eq!(ws.active_claim_owners(), vec!["victim:01".to_string()]);
    ws.cleanup();
}

/// STRIP-THEN-SEIZE via receipt: the full attack, not just its first step. The
/// assertion is that ownership did not move — a refusal that still let the path
/// change hands would be worthless.
#[test]
fn a_receipt_stripped_claim_cannot_be_seized_by_the_stripper() {
    let ws = Workspace::new("receipt-seize");
    ws.claim("victim:01", "src/lib.rs");

    let _ = ws.run(&[
        "say",
        "receipt",
        "--tool",
        "codex:rogue",
        "--ref",
        "ignored",
        "--subject",
        "done",
    ]);
    let seize = ws.json(&[
        "say",
        "claim",
        "--tool",
        "codex:rogue",
        "--path",
        "src/lib.rs",
        "--subject",
        "mine",
        "--json",
    ]);

    assert_ne!(
        seize["ok"],
        Value::Bool(true),
        "the path must still be victim:01's: {seize}"
    );
    assert_eq!(ws.active_claim_owners(), vec!["victim:01".to_string()]);
    ws.cleanup();
}

/// NEGATIVE CONTROL: a receipt is a normal coordination fact. Gating the
/// claim-closing case must not break receipts that close a HANDOFF, which is
/// what receipts are mostly for.
#[test]
fn a_receipt_against_a_peers_handoff_is_still_allowed() {
    let ws = Workspace::new("receipt-handoff");
    let handoff = ws.json(&[
        "say",
        "handoff",
        "--tool",
        "victim:01",
        "--to",
        "codex:rogue",
        "--subject",
        "please take this",
        "--json",
    ]);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff carries an event_id");

    let v = ws.json(&[
        "say",
        "receipt",
        "--tool",
        "codex:rogue",
        "--ref",
        handoff_id,
        "--subject",
        "got it",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "acknowledging a handoff addressed to you is normal coordination: {v}"
    );
    ws.cleanup();
}

/// NEGATIVE CONTROL: the owner may close its own claim with a receipt.
#[test]
fn the_owner_can_close_its_own_claim_with_a_receipt() {
    let ws = Workspace::new("receipt-self");
    let claim = ws.claim("victim:01", "src/lib.rs");

    let v = ws.json(&[
        "say",
        "receipt",
        "--tool",
        "victim:01",
        "--ref",
        &claim,
        "--subject",
        "done",
        "--json",
    ]);

    assert_eq!(
        v["ok"],
        Value::Bool(true),
        "self-close is the normal path: {v}"
    );
    assert!(
        ws.active_claim_owners().is_empty(),
        "and it must actually close the claim"
    );
    ws.cleanup();
}
