// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The reaper must COMPLETE on a ledger the size of a real one.
//!
//! # What was wrong
//!
//! `rally doctor --reap-stale --apply` could not finish. Measured against a
//! synthetic ledger sized like this repo's own — 6,563 facts, 63 lease-expired
//! claims — a full drain took 40.6 s while the mutation watchdog allows 3 s, so
//! every attempt returned `watchdog-timeout-uncommitted-mutation` and NOTHING was
//! reaped. The cleanup that would have shrunk the working set was blocked by the
//! size of the working set (design audit D10 / RC-058).
//!
//! # The criterion, and why the budget clock is injected
//!
//! A wall-clock threshold in a test is a machine-speed assertion: it fails on a
//! loaded CI box and passes on a fast laptop regardless of the code. These tests
//! inject the reaper's debug-only logical budget clock, while giving the outer
//! process watchdog generous headroom. One logical step crosses the production
//! 2s reap budget after a known number of writes, so pass partitioning is exact
//! and independent of host scheduling.
//!
//! # What these tests do NOT cover
//!
//! * They do not assert the reaper is FAST. A pass is bounded because it stops
//!   when its budget is spent, not because the per-append cost came down enough.
//!   The four full ledger reads per verified append are untouched and stay open
//!   in the register.
//! * They do not cover concurrency. One process reaps; RC-057's unbounded
//!   concurrent-pass question is separate and still open.
//! * The synthetic ledger is one segment of uniform records. A real ledger has
//!   many segments and a rotated archive, which the fold cost scales with
//!   differently.

#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Facts in the synthetic ledger. Chosen to match the measured size of this
/// repo's own room (6,442 live records on 2026-08-04) — the expensive dimension,
/// because every verified append re-reads all of them.
const LEDGER_FACTS: usize = 6_500;

/// Lease-expired claims to drain. Lower than the 63 the real ledger carried, to
/// bound test runtime: the per-pass cost is set by `LEDGER_FACTS`, and the claim
/// count only decides how many passes a drain takes.
const EXPIRED_CLAIMS: usize = 24;

/// Passes allowed before a drain is declared stuck. Generous: the point is to
/// prove the queue SHRINKS to zero, not how quickly.
const MAX_PASSES: usize = 60;

struct Room {
    cwd: PathBuf,
    home: PathBuf,
}

impl Room {
    /// Write a synthetic ledger segment directly.
    ///
    /// Seeding through `rally say` would take thousands of process launches and
    /// hours. The segment format is the canonical record, and the first read
    /// reconciles it into `facts.db` — the same path a fresh clone takes.
    fn seeded(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("reaper-scale-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("reaper-scale-{name}-{nanos}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        let log = cwd.join(".rally").join("log");
        fs::create_dir_all(&log).unwrap();
        fs::create_dir_all(&home).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stamp = |secs_ago: i64| -> String {
            let t = chrono::DateTime::from_timestamp(now - secs_ago, 0).unwrap();
            t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        };

        let mut seq: i64 = 0;
        let mut lines: Vec<String> = Vec::with_capacity(LEDGER_FACTS);
        let mut emit = |kind: &str,
                        tool: &str,
                        subject: String,
                        age: i64,
                        scope: Vec<String>,
                        evidence: Vec<String>| {
            seq += 1;
            let payload = json!({
                "created_at": stamp(age),
                "event_id": format!("fact_scale_{seq:08x}"),
                "evidence": evidence,
                "kind": kind,
                "ref": Value::Null,
                "role": Value::Null,
                "schema": "agent-rally.fact.v1",
                "scope": scope,
                "seq": 0,
                "severity": Value::Null,
                "status": Value::Null,
                "subject": subject,
                "summary": Value::Null,
                "target": Value::Null,
                "tool": tool,
                "uri": Value::Null,
            });
            lines.push(
                json!({
                    "seq": seq,
                    "occurred_at": stamp(age),
                    "event_type": kind,
                    "payload": payload,
                })
                .to_string(),
            );
        };

        // Filler: ordinary traffic from many peers, spread across a month, so
        // the projection has real work to do rather than one repeated record.
        let filler = LEDGER_FACTS.saturating_sub(EXPIRED_CLAIMS * 2);
        for i in 0..filler {
            let tool = format!("peer:{:03}", i % 60);
            let age = 60 + ((i as i64) * 37) % (30 * 86_400);
            if i % 5 == 0 {
                emit(
                    "presence",
                    &tool,
                    format!("presence {tool}"),
                    age,
                    Vec::new(),
                    vec!["planned_heartbeat_secs:300".to_string()],
                );
            } else {
                emit(
                    "artifact",
                    &tool,
                    format!("synthetic artifact {i} {}", "y".repeat(60)),
                    age,
                    Vec::new(),
                    Vec::new(),
                );
            }
        }

        // The drain target: claims whose OWN lease stamp is provably in the
        // past, owned by tools silent for over a week. Writer-stamped expiry is
        // the signal the reaper trusts without consulting owner liveness.
        for k in 0..EXPIRED_CLAIMS {
            let owner = format!("stale-owner:{k:03}");
            emit(
                "presence",
                &owner,
                format!("presence {owner}"),
                8 * 86_400,
                Vec::new(),
                vec!["planned_heartbeat_secs:300".to_string()],
            );
            emit(
                "claim",
                &owner,
                format!("synthetic claim {k}"),
                7 * 86_400,
                vec![format!("file:src/gen/mod_{k}.rs")],
                vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
            );
        }

        fs::write(log.join("synthetic.jsonl"), lines.join("\n") + "\n").unwrap();
        Room { cwd, home }
    }

    fn json(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .args(args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?} did not emit JSON ({e})\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            )
        })
    }

    fn active_claim_count(&self) -> usize {
        let v = self.json(&["room", "--json", "--timeout-ms", "60000"], &[]);
        v["data"]["room"]["active_claims"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

/// THE control. A repo-sized ledger with `EXPIRED_CLAIMS` eligible claims must
/// drain to zero under deterministic one-write budget slices.
#[test]
fn a_repo_sized_ledger_drains_under_an_injected_budget_clock() {
    let room = Room::seeded("drain");

    // Vacuity: without this, a broken fixture that seeds no eligible claim would
    // "drain" on pass one and prove nothing.
    let dry = room.json(
        &["doctor", "--reap-stale", "--json", "--timeout-ms", "60000"],
        &[],
    );
    let eligible = dry["data"]["doctor"]["claims_reaped"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    assert_eq!(
        eligible, EXPIRED_CLAIMS,
        "fixture must present {EXPIRED_CLAIMS} eligible claims; got {eligible}: {dry}"
    );

    let mut passes = 0usize;
    let mut reaped_total = 0usize;
    loop {
        passes += 1;
        assert!(
            passes <= MAX_PASSES,
            "drain did not finish in {MAX_PASSES} passes; reaped {reaped_total} of {EXPIRED_CLAIMS}"
        );

        let report = room.json(
            &[
                "doctor",
                "--reap-stale",
                "--apply",
                "--json",
                "--timeout-ms",
                "60000",
            ],
            &[("RALLY_TEST_REAP_CLOCK_STEP_MS", "2001")],
        );

        let doctor = &report["data"]["doctor"];
        let reaped = doctor["claims_reaped"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        let remaining = doctor["remaining"].as_u64().unwrap_or(0);
        assert_eq!(
            reaped, 1,
            "one 2001ms logical step must admit exactly one write per 2000ms pass: {report}"
        );
        assert_eq!(
            doctor["write_failures"], 0,
            "pass {passes} had durable write failures: {report}"
        );
        reaped_total += reaped;

        if remaining == 0 {
            break;
        }
        // Forward progress. A budget that could spend itself on the opening
        // projection would return `remaining: N` forever without shrinking the
        // queue — a bounded pass that never finishes the job.
        assert!(
            reaped >= 1,
            "pass {passes} reported {remaining} remaining but reaped nothing, so \
             the queue cannot shrink: {report}"
        );
    }

    assert_eq!(
        reaped_total, EXPIRED_CLAIMS,
        "every eligible claim must be reaped across the passes"
    );
    assert_eq!(
        room.active_claim_count(),
        0,
        "ground truth, independent of the report: the room must hold no active \
         claims once the drain reports nothing remaining"
    );
}

/// A smaller logical step admits exactly two actions before the third observes
/// the 2000ms budget as spent. This pins the injected clock itself so the drain
/// test cannot turn green because the seam was ignored.
#[test]
fn injected_clock_controls_exact_pass_partition() {
    let room = Room::seeded("logical-partition");
    let report = room.json(
        &[
            "doctor",
            "--reap-stale",
            "--apply",
            "--json",
            "--timeout-ms",
            "60000",
        ],
        &[("RALLY_TEST_REAP_CLOCK_STEP_MS", "1001")],
    );
    assert_eq!(
        report["data"]["doctor"]["claims_reaped"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "two 1001ms logical steps cross the 2000ms budget: {report}"
    );
    assert_eq!(
        report["data"]["doctor"]["remaining"],
        (EXPIRED_CLAIMS - 2) as u64
    );
    assert_eq!(report["data"]["doctor"]["complete"], false);
}
