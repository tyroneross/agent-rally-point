// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial controls for lead-seat authorization (ARP-R-01).
//!
//! The lead seat is rally's authority root. Every control added in the prior
//! cycle reads "is this agent the lead": RC-037 gates room-wide claims on it,
//! RC-038 gates the room-wide freeze on it. Before this suite, `set_lead`'s
//! only precondition was `ensure_presence`, which CREATES presence rather than
//! checking standing — so one command took the seat from a live incumbent and
//! re-opened both controls at once. Reproduced live against a live incumbent
//! and against a `--user-designated` one; `lead relinquish --tool rogue`
//! vacated the seat to null.
//!
//! Every test here performs the hostile action and asserts the outcome. The
//! negative controls matter as much as the refusals: a gate that also blocks
//! the legitimate handoff has replaced a security defect with an availability
//! one, and this suite has already caught exactly that (the projection read
//! `fact.tool` while the gate read `fact.target`, so a genuine handoff reported
//! success while the seat did not move).
//!
//! # Read `impersonation_is_not_stopped_and_this_test_says_so` before trusting
//! # anything here
//!
//! `--tool` is self-asserted. These gates close the path where an agent acts
//! under its OWN name; they do not stop one willing to claim another's. That
//! test asserts the residual rather than describing it, so it cannot rot into a
//! belief that the seat is defended.

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
        let cwd = std::env::temp_dir().join(format!("lsa-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("lsa-{name}-{nanos}-home"));
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

    /// Refusals go to STDERR, successes to STDOUT; a suite that asserts on
    /// rejection has to read both or every refusal looks like a crash.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?} did not emit JSON ({e})\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            )
        })
    }

    fn lead(&self) -> Option<String> {
        self.json(&["room", "--json"])["data"]["room"]["lead"]
            .as_str()
            .map(str::to_string)
    }

    /// Seat `tool` as lead in a room that has none (the first-join path, which
    /// stays open by design).
    fn seat(&self, tool: &str) {
        let v = self.json(&["lead", "assign", "--tool", tool, "--to", tool, "--json"]);
        assert_eq!(v["ok"], Value::Bool(true), "first-join seating: {v}");
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn ok(v: &Value) -> bool {
    v["ok"] == Value::Bool(true)
}

fn error_text(v: &Value) -> String {
    v["error"].as_str().unwrap_or_default().to_string()
}

/// THE defect. A non-incumbent takes the seat under its own name.
///
/// Neuter `assert_lead_transfer_authorized` and this passes: `rally lead assign
/// --tool rogue --to rogue` succeeded against a live incumbent, and everything
/// gated on the seat followed.
#[test]
fn non_incumbent_cannot_assign_the_lead_seat() {
    let ws = Workspace::new("assign");
    ws.seat("victim");

    let v = ws.json(&[
        "lead", "assign", "--tool", "rogue", "--to", "rogue", "--json",
    ]);

    assert!(!ok(&v), "a non-incumbent must not take the seat: {v}");
    let err = error_text(&v);
    assert!(
        err.contains("victim"),
        "refusal must name the incumbent: {err}"
    );
    assert!(err.contains("rogue"), "refusal must name the actor: {err}");
    assert_eq!(ws.lead().as_deref(), Some("victim"), "seat must not move");
    ws.cleanup();
}

/// The ADJACENT move to the reported one: same seizure, routed through
/// `handoff` instead of `assign`. A gate on one subcommand is a gate on one
/// spelling — `handoff` and `assign` both call `set_lead`, so both must clear
/// the same bar.
#[test]
fn non_incumbent_cannot_hand_the_seat_to_itself_via_handoff() {
    let ws = Workspace::new("handoff-seize");
    ws.seat("victim");

    let v = ws.json(&[
        "lead", "handoff", "--tool", "rogue", "--to", "rogue", "--json",
    ]);

    assert!(!ok(&v), "handoff is not a seizure loophole: {v}");
    assert_eq!(ws.lead().as_deref(), Some("victim"));
    ws.cleanup();
}

/// Another adjacent move: rather than take the seat, VACATE it. An attacker
/// that cannot hold the seat can still deny it to the honest lead, which
/// disarms every lead-gated control in the room just as effectively.
#[test]
fn non_incumbent_cannot_relinquish_the_seat() {
    let ws = Workspace::new("relinquish");
    ws.seat("victim");

    let v = ws.json(&["lead", "relinquish", "--tool", "rogue", "--json"]);

    assert!(!ok(&v), "a non-incumbent must not vacate the seat: {v}");
    assert_eq!(
        ws.lead().as_deref(),
        Some("victim"),
        "the seat must still be held"
    );
    ws.cleanup();
}

/// A third adjacent move: `--user-designated` is a MODE, not an authority. It
/// exists so a human's choice supersedes a first-join lead; if it also bypassed
/// the gate, the refusal above would be one flag deep.
#[test]
fn user_designated_is_a_mode_not_an_authority() {
    let ws = Workspace::new("user-designated");
    ws.seat("victim");

    let v = ws.json(&[
        "lead",
        "assign",
        "--tool",
        "rogue",
        "--to",
        "rogue",
        "--user-designated",
        "--json",
    ]);

    assert!(!ok(&v), "--user-designated must not bypass the gate: {v}");
    assert_eq!(ws.lead().as_deref(), Some("victim"));
    ws.cleanup();
}

/// NEGATIVE CONTROL, and the one this suite has already earned its keep on: a
/// genuine handoff by the incumbent must MOVE THE SEAT.
///
/// Asserting `ok == true` alone is not enough — the first version of the
/// ARP-R-01 fix returned success here while the seat stayed put, because the
/// write gate read the new attribution (`target`) and the room projection still
/// read the old one (`tool`). The observable outcome is the assertion.
#[test]
fn the_incumbent_can_hand_the_seat_off_and_it_actually_moves() {
    let ws = Workspace::new("genuine-handoff");
    ws.seat("victim");

    let v = ws.json(&[
        "lead", "handoff", "--tool", "victim", "--to", "helper", "--json",
    ]);

    assert!(ok(&v), "the incumbent may hand off its own seat: {v}");
    assert_eq!(
        ws.lead().as_deref(),
        Some("helper"),
        "the seat must actually move — success without movement is the defect \
         this assertion exists to catch"
    );
    ws.cleanup();
}

/// NEGATIVE CONTROL: the incumbent may vacate its own seat, reopening it.
#[test]
fn the_incumbent_can_relinquish_its_own_seat() {
    let ws = Workspace::new("self-relinquish");
    ws.seat("victim");

    let v = ws.json(&["lead", "relinquish", "--tool", "victim", "--json"]);

    assert!(ok(&v), "the incumbent may vacate its own seat: {v}");
    assert_eq!(ws.lead(), None, "the seat must reopen");
    ws.cleanup();
}

/// NEGATIVE CONTROL: an empty seat is first-join, by design and by prior
/// documentation. The fix must not turn a leaderless room into a permanently
/// leaderless one.
#[test]
fn an_empty_seat_is_still_first_join() {
    let ws = Workspace::new("first-join");
    let v = ws.json(&[
        "lead", "assign", "--tool", "alpha", "--to", "alpha", "--json",
    ]);
    assert!(ok(&v), "an empty seat admits the first caller: {v}");
    assert_eq!(ws.lead().as_deref(), Some("alpha"));
    ws.cleanup();
}

/// `--force` is the deliberate path, and it must be RECORDED as one. A seizure
/// that leaves no trace is indistinguishable from a handoff to anyone reading
/// the room afterwards, which is the flag's entire value.
#[test]
fn force_seizure_succeeds_and_is_recorded_as_a_seizure() {
    let ws = Workspace::new("force");
    ws.seat("victim");

    let v = ws.json(&[
        "lead", "assign", "--tool", "rogue", "--to", "rogue", "--force", "--json",
    ]);
    assert!(ok(&v), "--force is a documented path: {v}");
    assert_eq!(ws.lead().as_deref(), Some("rogue"));

    let evidence = v["data"]["lead"]["fact"]["evidence"]
        .as_array()
        .expect("the lead fact carries evidence")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        evidence.iter().any(|e| e.contains("seizure-acknowledged")),
        "the seizure must be on the record: {evidence:?}"
    );
    assert!(
        evidence.iter().any(|e| *e == "displaced:victim"),
        "the record must name who was displaced: {evidence:?}"
    );
    ws.cleanup();
}

/// ARP-R-01, attribution half. The ledger must record the ACTOR as the author.
///
/// `set_lead` stamped `tool = <the beneficiary>`, so a seizure was recorded as
/// authored by the agent that GAINED the seat. The one field an investigator
/// reads to find out who took it named the wrong agent — and no gate could be
/// built on `fact.tool`, because it did not hold the actor. The gate above and
/// this assertion are the same fix.
#[test]
fn the_lead_fact_records_the_actor_as_author_and_the_beneficiary_as_target() {
    let ws = Workspace::new("attribution");
    ws.seat("victim");

    let v = ws.json(&[
        "lead", "handoff", "--tool", "victim", "--to", "helper", "--json",
    ]);
    let fact = &v["data"]["lead"]["fact"];

    assert_eq!(
        fact["tool"].as_str(),
        Some("victim"),
        "author must be the agent that RAN the command: {fact}"
    );
    assert_eq!(
        fact["target"].as_str(),
        Some("helper"),
        "beneficiary belongs in target: {fact}"
    );
    ws.cleanup();
}

/// A legacy lead fact carries the beneficiary in `tool` and no `target`. The
/// ledger is append-only, so a projection change that cannot read the old shape
/// silently rewrites history it has no way to edit. Three such facts exist in
/// this repo's own ledger.
#[test]
fn a_legacy_lead_fact_still_projects_to_the_same_lead() {
    let ws = Workspace::new("legacy");
    // A room seeded through the old shape: the seating path writes
    // `target`, so emulate the legacy fact by seating and then asserting the
    // fallback directly through a hand-written segment line.
    let log_dir = ws.cwd.join(".rally").join("log");
    fs::create_dir_all(&log_dir).unwrap();
    let legacy = serde_json::json!({
        "seq": 1,
        "occurred_at": "2026-05-30T00:00:00Z",
        "event_type": "decision",
        "payload": {
            "schema": "rally.fact.v1",
            "event_id": "fact_legacy_lead",
            "seq": 1,
            "thread_id": "room_legacy",
            "kind": "decision",
            "tool": "claude_code:l4",
            "subject": "role:lead",
            "scope": [],
            "created_at": "2026-05-30T00:00:00Z",
            "evidence": ["assigned:first-join"]
        }
    });
    fs::write(log_dir.join("2026-05-30.jsonl"), format!("{legacy}\n")).unwrap();

    assert_eq!(
        ws.lead().as_deref(),
        Some("claude_code:l4"),
        "a pre-fix lead fact must still project to its beneficiary"
    );
    ws.cleanup();
}

/// THE RESIDUAL, asserted rather than described.
///
/// `--tool` is bound to nothing. Every gate in `write_authority` reads it, so
/// an actor willing to pass `--tool <incumbent>` clears all of them. This test
/// asserts what ACTUALLY happens today, so the gap is visible in the test
/// output instead of living only in a comment somebody has to remember.
///
/// If a session-lease model ever lands (see the authority-model entry in
/// `docs/ROOT-CAUSE-REGISTER.md`), this test FAILS — and that failure is the
/// signal to flip the assertion and mark ARP-R-01 controlled. Until then,
/// ARP-R-01 is `mitigated`, not `controlled`, and this is why.
#[test]
fn impersonation_is_not_stopped_and_this_test_says_so() {
    let ws = Workspace::new("impersonation");
    ws.seat("victim");

    // The rogue does not argue with the gate. It asserts it IS the incumbent.
    let v = ws.json(&[
        "lead", "handoff", "--tool", "victim", "--to", "rogue", "--json",
    ]);

    assert!(
        ok(&v),
        "DOCUMENTED RESIDUAL: `--tool` is self-asserted, so impersonating the \
         incumbent clears the gate. If this now FAILS, identity became \
         authoritative — flip this assertion, and ARP-R-01 can finally be \
         marked controlled. See docs/security/TRUST-MODEL.md."
    );
    assert_eq!(
        ws.lead().as_deref(),
        Some("rogue"),
        "and the seat moves, because the gate had nothing to check against"
    );
    ws.cleanup();
}
