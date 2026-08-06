// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Reaper report integrity — the report must not claim a write that did not land.
//!
//! D7: the lead-relinquish path discarded its append result and reported the
//! seat as reopened regardless. A caller reading `lead_relinquished: "<tool>"`
//! off that report believes the seat is open while the ledger still holds the
//! lease — the exact false-success class the claim and handoff paths already
//! guard against by counting a failed append as `preserved` and omitting the
//! entry.
//!
//! D8: the auto-reap rate limiter is a plain read-then-write with no lock, so
//! its bound is SEQUENTIAL ONLY. These tests grade the bound the code actually
//! delivers (one pass per interval when enters do not overlap) rather than the
//! concurrent bound the old comment claimed and the code never had.
//!
//! Failure injection is filesystem-level: `.rally/log/` is made read-only, so
//! the canonical segment append fails while every read path (snapshot,
//! reconcile into the derived `facts.db`, projection) still works. That is
//! narrower than the `RALLY_TEST_BLOCK_*` watchdog hooks in
//! `watchdog_write_durability.rs`, which inject latency rather than write
//! failure; no append-failure hook exists in the binary, so the failure is
//! injected through the filesystem instead.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock budget for every spawned command. Generous enough that a cold
/// SQLite open on a loaded machine does not trip the mutation watchdog and turn
/// a real verdict into a timeout.
const TIMEOUT_MS: &str = "20000";

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("reaper-integrity-{name}-cwd"));
        let home = temp_path(&format!("reaper-integrity-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            // Auto-reap is off by default; the D7 tests drive the reaper through
            // `doctor` and must not also get an implicit pass on `enter`.
            .env("RALLY_NO_AUTO_REAP", "1");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.rally()
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("spawn rally {args:?}: {e}"))
    }

    fn run_ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "rally {args:?} failed: status={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        stdout_json(&out)
    }

    fn init_observed_worktree(&self) {
        let git_ok = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&self.cwd)
                .args(args)
                .output()
                .unwrap_or_else(|error| panic!("spawn git {args:?}: {error}"));
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git_ok(&["init", "--quiet"]);
        fs::write(self.cwd.join("observed.txt"), "observed\n").expect("write observed fixture");
        git_ok(&["add", "observed.txt"]);
        git_ok(&[
            "-c",
            "user.name=Rally Test",
            "-c",
            "user.email=rally-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "observed fixture",
        ]);
    }

    fn log_dir(&self) -> PathBuf {
        self.cwd.join(".rally").join("log")
    }

    /// The reaper's report from `doctor --reap-stale`, applied or dry-run.
    fn reap(&self, apply: bool) -> (Value, String) {
        let (report, stderr, envelope_ok, exit) = self.reap_full(apply);
        assert!(
            exit == Some(0) && envelope_ok,
            "this room's reap was expected to succeed: exit={exit:?} ok={envelope_ok} \
             report={report} stderr={stderr}"
        );
        (report, stderr)
    }

    /// The reap plus the two verdict channels a caller actually reads: the
    /// envelope's `ok` and the process exit code. `reap` asserts both are
    /// healthy; the failed-write tests need to inspect them.
    fn reap_full(&self, apply: bool) -> (Value, String, bool, Option<i32>) {
        let mut args = vec![
            "doctor",
            "--reap-stale",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ];
        if apply {
            args.push("--apply");
        }
        let out = self.run(&args);
        let body = stdout_json(&out);
        let report = body["data"]["doctor"].clone();
        (
            report,
            String::from_utf8_lossy(&out.stderr).into_owned(),
            body["ok"] == Value::Bool(true),
            out.status.code(),
        )
    }

    fn current_lead(&self) -> Value {
        let body = self.run_ok(&["lead", "show", "--json", "--timeout-ms", TIMEOUT_MS]);
        body["data"]["lead"]["current_lead"].clone()
    }

    fn active_claim_subjects(&self) -> Vec<String> {
        let body = self.run_ok(&["room", "--json", "--timeout-ms", TIMEOUT_MS]);
        body["data"]["room"]["active_claims"]
            .as_array()
            .map(|claims| {
                claims
                    .iter()
                    .filter_map(|c| c["subject"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Rewrite every ledger line so its `occurred_at` / `created_at` sit
    /// `hours` in the past, then drop the DERIVED caches so the next read
    /// rebuilds from the canonical segments.
    ///
    /// Owner-staleness (the only signal that relinquishes a lead) is measured
    /// against `last_seen_ts`, which comes verbatim from the ledger line. No
    /// CLI flag backdates a fact, so the fixture edits the canonical segment —
    /// the same surface `store::facts_from_segments` reads.
    fn backdate_ledger(&self, hours: i64) {
        let stamp = (chrono::Utc::now() - chrono::Duration::hours(hours))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let mut rewritten = 0usize;
        for entry in fs::read_dir(self.log_dir()).expect("read log dir") {
            let path = entry.expect("log dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let raw = fs::read_to_string(&path).expect("read segment");
            let mut out = String::new();
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let mut record: Value = serde_json::from_str(line).expect("segment line is JSON");
                record["occurred_at"] = Value::String(stamp.clone());
                record["payload"]["created_at"] = Value::String(stamp.clone());
                out.push_str(&serde_json::to_string(&record).expect("re-encode segment line"));
                out.push('\n');
                rewritten += 1;
            }
            fs::write(&path, out).expect("write backdated segment");
        }
        assert!(
            rewritten > 0,
            "fixture must have backdated at least one fact"
        );

        let rally = self.cwd.join(".rally");
        for derived in [
            "facts.db",
            "facts.db-wal",
            "facts.db-shm",
            ".reconcile-cache.json",
        ] {
            fs::remove_file(rally.join(derived)).ok();
        }
    }

    /// Make the canonical ledger unwritable: appends fail, reads still work.
    fn freeze_ledger(&self) {
        set_mode(&self.log_dir(), 0o555);
        for entry in fs::read_dir(self.log_dir()).expect("read log dir") {
            set_mode(&entry.expect("log dir entry").path(), 0o444);
        }
    }

    fn thaw_ledger(&self) {
        if !self.log_dir().exists() {
            return;
        }
        set_mode(&self.log_dir(), 0o755);
        if let Ok(entries) = fs::read_dir(self.log_dir()) {
            for entry in entries.flatten() {
                set_mode(&entry.path(), 0o644);
            }
        }
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        // A frozen ledger would defeat remove_dir_all and leak the fixture.
        self.thaw_ledger();
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {:o} {}: {e}", mode, path.display()));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {
    panic!("reaper write-integrity fixtures need unix permission bits");
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

fn stdout_json(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("stdout must be JSON: {err}\nstdout={stdout}"))
}

/// A room whose lead has been silent for 3 hours — the reaper's relinquish
/// pre-condition (`takeover_eligible_owners` at the 2h bar).
fn room_with_stale_lead(name: &str, lead: &str) -> TempRoom {
    let room = TempRoom::new(name);
    room.run_ok(&[
        "enter",
        "--tool",
        lead,
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    room.run_ok(&[
        "lead",
        "assign",
        "--tool",
        lead,
        "--to",
        lead,
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    room.backdate_ledger(3);
    room
}

// =============================================================================
// D7 — the report must not claim a relinquish whose write failed
// =============================================================================

/// ADVERSARIAL CONTROL for D7. The lead IS stale, so the reaper stages the
/// relinquish; the ledger is read-only, so the append fails. Before the fix the
/// append result was discarded and the report still said
/// `lead_relinquished: "stale-lead:01"` with `applied: true` — a caller reading
/// that believes the seat is open while `lead show` still names the same tool.
///
/// Revert the D7 fix (restore `let _ = room.append_fact_verified(...)`) and
/// this fails on the first assertion.
#[test]
fn failed_lead_relinquish_write_is_not_reported_as_applied() {
    let room = room_with_stale_lead("d7-failed-write", "stale-lead:01");
    assert_eq!(
        room.current_lead(),
        Value::String("stale-lead:01".to_string()),
        "fixture precondition: the lead seat is held"
    );

    room.freeze_ledger();
    let (report, stderr, envelope_ok, exit) = room.reap_full(true);
    room.thaw_ledger();

    assert_eq!(
        report["lead_relinquished"],
        Value::Null,
        "a relinquish whose durable append failed must NOT appear in the report; \
         got {report} (stderr={stderr})"
    );
    assert!(
        stderr.contains("keeping lead stale-lead:01"),
        "a dropped relinquish must say so on stderr; stderr={stderr}"
    );

    // D7, second half. The item list was already honest; the SUMMARY was not.
    // `applied` was a copy of `--apply`, the failure was counted into a field
    // whose own doc says "future-dated lease, owner unparseable, or owner still
    // active", and the envelope answered `ok: true` at exit 0. A caller that
    // read the summary rather than diffing the lists — which is what a script
    // does — could not tell an unwritable ledger from a healthy one.
    assert_eq!(
        report["write_failures"], 1,
        "a failed durable append must be counted as a WRITE FAILURE; got {report}"
    );
    assert_eq!(
        report["preserved_future_or_active"], 0,
        "and must NOT be laundered into the kept-by-policy count, which means \
         something entirely different; got {report}"
    );
    assert_eq!(
        report["applied"],
        Value::Bool(false),
        "`applied` must mean the staged writes landed, not that --apply was \
         passed; got {report}"
    );
    assert!(
        !envelope_ok,
        "the envelope must not answer ok:true for a pass whose writes failed; \
         got {report}"
    );
    assert_ne!(
        exit,
        Some(0),
        "and the exit code must not say success either; a script reading only \
         the exit code is the most common caller there is"
    );

    // Ground truth, independent of the report: the seat never reopened.
    assert_eq!(
        room.current_lead(),
        Value::String("stale-lead:01".to_string()),
        "the lead lease is still held — which is exactly why the report must not \
         claim it was relinquished"
    );
}

/// NEGATIVE CONTROL. Same fixture, writable ledger: the relinquish must still
/// be reported AND must still land. This is what stops the D7 fix from being
/// satisfied by never reporting a relinquish at all.
#[test]
fn successful_lead_relinquish_is_reported_and_lands() {
    let room = room_with_stale_lead("d7-success", "stale-lead:02");

    let (report, stderr) = room.reap(true);

    assert_eq!(
        report["lead_relinquished"],
        Value::String("stale-lead:02".to_string()),
        "a stale lead whose relinquish committed must be reported; got {report} \
         (stderr={stderr})"
    );
    assert_eq!(report["applied"], Value::Bool(true));
    assert_eq!(
        report["preserved_future_or_active"], 0,
        "nothing was preserved on the success path; got {report}"
    );
    assert_eq!(
        room.current_lead(),
        Value::Null,
        "the relinquish fact must actually reopen the seat"
    );
}

/// Dry-run parity: the verdict is reported, nothing is written, and the seat
/// stays held. `applied: false` is what tells the caller the report is a
/// prediction — the D7 fix must not start suppressing dry-run verdicts.
#[test]
fn dry_run_reports_the_relinquish_verdict_without_writing() {
    let room = room_with_stale_lead("d7-dry-run", "stale-lead:03");

    let (report, _stderr) = room.reap(false);

    assert_eq!(
        report["lead_relinquished"],
        Value::String("stale-lead:03".to_string())
    );
    assert_eq!(report["applied"], Value::Bool(false));
    assert_eq!(
        room.current_lead(),
        Value::String("stale-lead:03".to_string()),
        "a dry run must not reopen the seat"
    );
}

/// The convention the D7 fix mirrors, pinned so it cannot regress in the other
/// direction: a claim whose `ClaimExpired` append fails is reported as
/// PRESERVED, not as reaped.
#[test]
fn failed_claim_expiry_write_is_not_reported_as_reaped() {
    let room = TempRoom::new("d7-claim-convention");
    room.run_ok(&[
        "enter",
        "--tool",
        "owner:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    // An explicitly-stamped past lease: the writer-stamped signal the reaper
    // trusts, with no dependence on owner staleness.
    room.run_ok(&[
        "say",
        "claim",
        "--tool",
        "owner:01",
        "--subject",
        "claim-frozen",
        "--scope",
        "file:src/frozen.rs",
        "--evidence",
        "lease_expires_at:2000-01-01T00:00:00Z",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);

    room.freeze_ledger();
    let (report, stderr, envelope_ok, exit) = room.reap_full(true);
    room.thaw_ledger();

    assert_eq!(
        report["claims_reaped"].as_array().map(Vec::len),
        Some(0),
        "a claim whose expiry append failed must not be reported as reaped; \
         got {report} (stderr={stderr})"
    );
    assert!(
        report["write_failures"].as_u64().unwrap_or(0) >= 1,
        "the un-closed claim must be counted as a WRITE FAILURE, not as kept by \
         policy; got {report}"
    );
    assert_eq!(
        report["applied"],
        Value::Bool(false),
        "`applied` must mean the staged writes landed; got {report}"
    );
    assert!(
        !envelope_ok,
        "envelope must not answer ok:true; got {report}"
    );
    assert_ne!(
        exit,
        Some(0),
        "exit code must not say success; got {report}"
    );
    assert_eq!(
        room.active_claim_subjects(),
        vec!["claim-frozen".to_string()],
        "the claim is still live — the report must agree"
    );
}

// =============================================================================
// D8 — the rate limiter's real bound is sequential, and only sequential
// =============================================================================

/// D8, option (b): the marker is a read-then-write with no lock, so it bounds
/// how OFTEN a pass runs, never how MANY run concurrently. This grades the
/// guarantee the code actually delivers — a second `enter` inside the interval
/// runs no pass — and deliberately asserts nothing about N overlapping enters,
/// because the code establishes no such bound and the comment no longer claims
/// one.
#[test]
fn auto_reap_interval_gate_holds_for_sequential_enters_only() {
    let room = TempRoom::new("d8-sequential-gate");
    room.init_observed_worktree();
    let owner_enter = room
        .rally()
        .env("RALLY_OBSERVER_PID", "2000000000")
        .args([
            "enter",
            "--tool",
            "owner:01",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ])
        .output()
        .expect("spawn owner enter");
    assert!(
        owner_enter.status.success(),
        "owner enter must succeed: {}",
        String::from_utf8_lossy(&owner_enter.stderr)
    );
    let seed_claim = |subject: &str, path: &str| {
        room.run_ok(&[
            "say",
            "claim",
            "--tool",
            "owner:01",
            "--subject",
            subject,
            "--scope",
            path,
            "--evidence",
            "lease_expires_at:2000-01-01T00:00:00Z",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ]);
    };
    seed_claim("claim-a", "file:src/a.rs");

    // The owner is externally observed dead with an unchanged worktree HEAD,
    // making its expired claim eligible under the same rule production uses.
    // Opt in to auto-reap for the two enters below (default is OFF).
    let enter = |tool: &str| -> String {
        let out = room
            .rally()
            .env_remove("RALLY_NO_AUTO_REAP")
            .env("RALLY_AUTO_REAP_INTERVAL_SECS", "3600")
            .args([
                "enter",
                "--tool",
                tool,
                "--json",
                "--timeout-ms",
                TIMEOUT_MS,
            ])
            .output()
            .expect("spawn rally enter");
        assert!(
            out.status.success(),
            "enter must not fail: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let first = enter("peer:01");
    assert!(
        first.contains("auto-reap closed 1 stale claim"),
        "the first enter of an interval must run a pass; stderr={first}"
    );

    // A fresh eligible claim appears INSIDE the interval. The gate must hold
    // anyway: rate-limited means rate-limited, not "reap whenever there is work".
    seed_claim("claim-b", "file:src/b.rs");
    let second = enter("peer:02");
    assert!(
        !second.contains("auto-reap closed"),
        "a second enter inside the interval must run no pass; stderr={second}"
    );
    assert_eq!(
        room.active_claim_subjects(),
        vec!["claim-b".to_string()],
        "claim-b must survive the second enter"
    );

    // The marker is the whole mechanism — the gate is exactly as strong as this
    // file's presence and freshness, and no stronger.
    let marker = room.cwd.join(".rally").join(".last-auto-reap");
    assert!(
        marker.is_file(),
        "the interval marker must exist after a pass"
    );
}
