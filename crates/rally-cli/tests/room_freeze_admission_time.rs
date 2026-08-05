// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial controls for admission-time room-freeze authority (ARP-R-01,
//! design audit D9).
//!
//! `check_before_write` decided whether an unscoped blocker was a room-wide
//! freeze by comparing its author against the CURRENT lead. That made the
//! verdict a function of the room's present state rather than of the fact being
//! judged, and the same fact id moved BOTH ways:
//!
//! * **Retroactive arm.** A non-lead posts an unscoped blocker; it projects as
//!   `unscoped-blocker` / allow. That agent later takes the seat, and the same
//!   blocker re-projects as `room-freeze` / deny. A room-wide denial armed
//!   after the fact, by a write that touched nothing.
//! * **Retroactive disarm.** The honest lead declares a freeze; anyone else
//!   takes the seat and it degrades to allow. The room's only stop control was
//!   removable in one command.
//!
//! The fix picks ADMISSION-TIME authority — the lead as of the blocker's own
//! `seq` — and applies it in the projection, which publishes `room_freeze_id`.
//! `check` reports that; it no longer decides it.
//!
//! These tests drive the real binary against real ledgers, because the property
//! under test is "the seat changed hands AFTER this blocker was written", which
//! a hand-built snapshot cannot express. That is precisely why the unit test in
//! `check.rs` did not catch this.

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
        let cwd = std::env::temp_dir().join(format!("rfat-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("rfat-{name}-{nanos}-home"));
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

    fn seat(&self, tool: &str) {
        let v = self.json(&["lead", "assign", "--tool", tool, "--to", tool, "--json"]);
        assert_eq!(v["ok"], Value::Bool(true), "seating {tool}: {v}");
    }

    fn unscoped_blocker(&self, tool: &str, subject: &str) {
        let v = self.json(&[
            "say",
            "blocker",
            "--tool",
            tool,
            "--subject",
            subject,
            "--json",
        ]);
        assert_eq!(v["ok"], Value::Bool(true), "posting blocker: {v}");
    }

    /// The finding codes `rally check before-write` raises for `tool`.
    fn before_write_codes(&self, tool: &str, path: &str) -> Vec<String> {
        let v = self.json(&[
            "check",
            "before-write",
            "--tool",
            tool,
            "--path",
            path,
            "--json",
        ]);
        v["data"]["check"]["findings"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f["code"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Does the strict gate STOP this write? Exit 4 is the strict-mode stop.
    fn strict_stops(&self, tool: &str, path: &str) -> bool {
        let out = self.run(&[
            "check",
            "before-write",
            "--tool",
            tool,
            "--path",
            path,
            "--strict",
            "--json",
        ]);
        out.status.code() == Some(4)
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

/// RETROACTIVE ARM. A non-lead's unscoped blocker must NOT become a room-wide
/// freeze when its author later takes the seat.
///
/// Revert the projection to the current-lead comparison and this fails: the
/// blocker's finding flips from `unscoped-blocker` to `room-freeze` and the
/// strict gate starts denying every write in the room, without the attacker
/// writing anything after the blocker except a lead assignment.
#[test]
fn a_blocker_does_not_arm_when_its_author_later_takes_the_seat() {
    let ws = Workspace::new("arm");
    // Seat someone ELSE first. This matters: rally auto-seats the first agent
    // to write anything as lead ("assigned:first-join"), so a room is only
    // leaderless until its first fact. An earlier draft of this test had rogue
    // post into an empty room and was surprised to find the blocker legitimately
    // authorized — rogue had become lead by writing it. The scenario has to put
    // rogue's blocker on the record while somebody else holds the seat.
    ws.seat("victim");
    ws.unscoped_blocker("rogue", "everything is blocked");

    let before = ws.before_write_codes("beta", "src/foo.rs");
    assert!(
        before.iter().any(|c| c == "unscoped-blocker"),
        "a non-lead's unscoped blocker is advisory: {before:?}"
    );
    assert!(
        !ws.strict_stops("beta", "src/foo.rs"),
        "and must not stop the write"
    );

    // The seat moves to rogue through the LEGITIMATE path, so nothing here
    // depends on the seizure gate — the question is purely whether an
    // already-written blocker gains authority it did not have.
    let v = ws.json(&[
        "lead", "handoff", "--tool", "victim", "--to", "rogue", "--json",
    ]);
    assert_eq!(v["ok"], Value::Bool(true), "handoff: {v}");

    let after = ws.before_write_codes("beta", "src/foo.rs");
    assert!(
        !after.iter().any(|c| c == "room-freeze"),
        "the blocker was written when rogue held no seat, so taking the seat \
         afterwards must not arm it retroactively: {after:?}"
    );
    assert!(
        after.iter().any(|c| c == "unscoped-blocker"),
        "it stays advisory, and stays visible: {after:?}"
    );
    assert!(
        !ws.strict_stops("beta", "src/foo.rs"),
        "a room-wide deny must not appear from a lead assignment alone"
    );
    ws.cleanup();
}

/// RETROACTIVE DISARM. The honest lead's freeze must SURVIVE a later change of
/// seat. Otherwise the room's only stop control is removable in one command by
/// exactly the agent it was meant to stop.
#[test]
fn a_legitimate_freeze_is_not_disarmed_by_a_later_seat_change() {
    let ws = Workspace::new("disarm");
    ws.seat("honest-lead");
    ws.unscoped_blocker("honest-lead", "release freeze: hold all writes");

    assert!(
        ws.before_write_codes("beta", "src/foo.rs")
            .iter()
            .any(|c| c == "room-freeze"),
        "the lead's unscoped blocker freezes the room"
    );
    assert!(
        ws.strict_stops("beta", "src/foo.rs"),
        "and stops writes under --strict"
    );

    // The seat moves. The freeze was authorized when it was written.
    let v = ws.json(&[
        "lead",
        "handoff",
        "--tool",
        "honest-lead",
        "--to",
        "successor",
        "--json",
    ]);
    assert_eq!(v["ok"], Value::Bool(true), "handoff: {v}");

    let after = ws.before_write_codes("beta", "src/foo.rs");
    assert!(
        after.iter().any(|c| c == "room-freeze"),
        "the freeze was authorized at its own seq and must survive the handoff: {after:?}"
    );
    assert!(
        ws.strict_stops("beta", "src/foo.rs"),
        "a freeze that any seat change disarms is not a control"
    );
    ws.cleanup();
}

/// NEGATIVE CONTROL: the capability itself survives. RC-038 removed the DoS,
/// not the freeze — a lead must still be able to stop the room.
#[test]
fn the_lead_can_still_freeze_the_room() {
    let ws = Workspace::new("capability");
    ws.seat("the-lead");
    ws.unscoped_blocker("the-lead", "release freeze");

    assert!(
        ws.before_write_codes("beta", "src/foo.rs")
            .iter()
            .any(|c| c == "room-freeze")
    );
    assert!(ws.strict_stops("beta", "src/foo.rs"));
    ws.cleanup();
}

/// NEGATIVE CONTROL: a non-lead's unscoped blocker is surfaced as a warning the
/// agent reads and decides about — it is not silently dropped. Losing the
/// signal would trade a DoS for a blind spot.
#[test]
fn a_non_lead_unscoped_blocker_is_still_surfaced_as_a_warning() {
    let ws = Workspace::new("surfaced");
    ws.seat("the-lead");
    ws.unscoped_blocker("someone-else", "I think everything is broken");

    let codes = ws.before_write_codes("beta", "src/foo.rs");
    assert!(
        codes.iter().any(|c| c == "unscoped-blocker"),
        "the agent must still read it: {codes:?}"
    );
    assert!(
        !ws.strict_stops("beta", "src/foo.rs"),
        "but it must not stop the write"
    );
    ws.cleanup();
}
