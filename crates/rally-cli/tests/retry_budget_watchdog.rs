// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Adversarial controls for the retry-budget / watchdog-budget contradiction
//! measured 2026-08-05.
//!
//! # The defect
//!
//! `open_fact_store` retried a locked database 16 times at `20ms * attempt`
//! (2720ms) and the append loop 16 more at `15ms * attempt` (2040ms). Both
//! counts were picked independently of `DEFAULT_WATCHDOG_TIMEOUT_MS` (3000ms)
//! and of each other, so a mutation that entered retry had 4760ms of scheduled
//! sleep inside a 3000ms budget and could not finish inside its own watchdog.
//! It died at the watchdog reporting `watchdog-timeout-uncommitted-mutation`.
//!
//! # What these controls convict
//!
//! [`contended_write_finishes_inside_its_own_watchdog`] is the one that fails
//! when the fix is reverted. A real `BEGIN EXCLUSIVE` holder keeps the lock for
//! longer than the retry budget but the command must still return INSIDE its
//! watchdog with an honest error, instead of being killed BY the watchdog.
//!
//! # A correction to the reported trigger, recorded here because the test is
//! where a future reader will look
//!
//! The defect was reported as "a stale or zero-length `facts.db-wal`/`-shm`
//! pair makes SQLite report busy/locked on open". Measured on 2026-08-05 with
//! both the 0.1.7 and 0.2.0 binaries on an empty scratch repo, that is NOT what
//! happens — see [`stale_wal_is_not_what_trips_the_retry_path`], which pins the
//! negative result so nobody re-derives it. The amplifier (retry budget vs
//! watchdog budget) is real and reproducible; the stale-WAL trigger is not.
//! The WAL left behind by an orphaned pool is a FINGERPRINT of the holder, not
//! the cause of the lock.

use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Watchdog budget every timed control pins, so assertions are falsifiable
/// rather than a function of host speed.
const WATCHDOG_MS: u64 = 3000;

/// Budgets the fix derives from [`WATCHDOG_MS`], restated here so the holder
/// durations below can be placed against them deliberately rather than by feel.
/// Kept in sync by `retry_budget.rs`'s own unit tests, which assert these exact
/// values at the 3000ms default.
const BUSY_TIMEOUT_MS: u64 = WATCHDOG_MS / 8; // 375ms — one in-SQLite block
const OPEN_RETRY_BUDGET_MS: u64 = WATCHDOG_MS / 3; // 1000ms — the open loop

/// How long the adversarial holder keeps the write lock when the control needs
/// the write to FAIL: longer than the whole composed budget, so the command
/// cannot succeed by waiting and the only question is whether it returns inside
/// its budget or gets killed at it.
const HOLD_MS: u64 = 6000;

/// How long the holder keeps the lock when the control needs the write to
/// SUCCEED **via rally's own retry loop**.
///
/// This has to sit strictly between the two budgets:
///
/// ```text
/// busy 375ms  <  HOLD_RETRYABLE_MS 700ms  <  open retry budget 1000ms
/// ```
///
/// Below `busy` the hold is absorbed entirely inside SQLite's busy handler,
/// `is_db_locked` never fires, and `RetryBudget::next_backoff` is never called —
/// so the control would pass even with rally's retry loop deleted, which is
/// exactly the vacuity an independent audit found in the first draft (it used
/// 400ms against a 750ms busy timeout). Above the open budget the write cannot
/// succeed at all.
const HOLD_RETRYABLE_MS: u64 = 700;

/// Enforced at COMPILE time: if anyone retunes the budgets so the hold no
/// longer sits between them, this stops the build rather than letting the
/// control quietly go vacuous again.
const _: () = assert!(
    HOLD_RETRYABLE_MS > BUSY_TIMEOUT_MS && HOLD_RETRYABLE_MS < OPEN_RETRY_BUDGET_MS,
    "HOLD_RETRYABLE_MS must sit strictly between the busy timeout and the open      retry budget, or write_still_retries_... proves nothing",
);

/// Timing assertions are opt-in; shape assertions always run.
///
/// The `elapsed <` bounds below are real invariants but they are measured
/// against a DEBUG binary whose own startup consumes a variable share of the
/// same budget. On a saturated host they fail for reasons that carry no
/// information about the fix — and a gate that certifies failures is worse than
/// no gate (the same lesson RC-044 records about flaky concurrency runs). The
/// SHAPE assertions — no `watchdog-timeout-uncommitted-mutation`, an error that
/// names what was observed, a write that actually lands — convict the reverted
/// fix on their own and run unconditionally.
///
/// Set `RALLY_TIMING_TESTS=1` on a quiesced host to enforce the wall-clock
/// bounds too.
fn timing_assertions_enabled() -> bool {
    std::env::var("RALLY_TIMING_TESTS").is_ok_and(|v| v.trim() == "1")
}

/// Assert `elapsed < bound` only when timing assertions are enabled.
fn assert_within(elapsed: Duration, bound: Duration, what: &str) {
    if timing_assertions_enabled() {
        assert!(
            elapsed < bound,
            "{what}: {elapsed:?} reached the {bound:?} bound"
        );
    } else if elapsed >= bound {
        eprintln!(
            "[timing] {what}: {elapsed:?} reached the {bound:?} bound \
             (not enforced; set RALLY_TIMING_TESTS=1 on a quiesced host)"
        );
    }
}

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("retry-budget-{name}-cwd"));
        let home = temp_path(&format!("retry-budget-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        let room = Self { cwd, home };
        // Seed the room so `facts.db` exists for the holder to lock.
        let out = room.rally(&["enter", "--tool", "claude_code:seed", "--json"]);
        assert!(
            out.status.success(),
            "seed enter failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        room
    }

    fn rally(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            // Hooks off: this control measures the store path, not hook wiring.
            .env("RALLY_HOOKS_DISABLED", "1")
            .args(args);
        cmd.output().expect("spawn rally")
    }

    fn facts_db(&self) -> PathBuf {
        self.cwd.join(".rally").join("facts.db")
    }

    fn wal(&self) -> PathBuf {
        self.cwd.join(".rally").join("facts.db-wal")
    }

    fn shm(&self) -> PathBuf {
        self.cwd.join(".rally").join("facts.db-shm")
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
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

/// A thread holding a genuine SQLite `BEGIN EXCLUSIVE` write lock on `db`.
///
/// Not a seam or an injected fault: the same condition a second rally process
/// or an orphaned connection pool creates. `ready` fires only once the lock is
/// actually held, so the control cannot race ahead of it and measure an
/// uncontended write by accident.
struct LockHolder {
    handle: Option<thread::JoinHandle<()>>,
}

impl LockHolder {
    fn hold(db: PathBuf, duration: Duration) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let conn = Connection::open(&db).expect("holder opens facts.db");
            conn.pragma_update(None, "journal_mode", "WAL").ok();
            conn.execute_batch("BEGIN EXCLUSIVE")
                .expect("holder takes EXCLUSIVE");
            ready_tx.send(()).ok();
            thread::sleep(duration);
            conn.execute_batch("ROLLBACK").ok();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("holder must acquire the lock before the control proceeds");
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

fn stdout_json(output: &Output) -> Option<Value> {
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).ok()
}

/// THE CONTROL THAT FAILS WHEN THE FIX IS REVERTED.
///
/// With a genuine holder the write cannot succeed, and it is not supposed to.
/// What it MUST do is come back inside the budget the caller asked for.
///
/// * Before the fix: the open loop alone schedules 2720ms of sleep inside a
///   3000ms budget, so the command is still sleeping when the watchdog fires.
///   Observed 3.038s / 3.049s, exit 4, `watchdog-timeout-uncommitted-mutation`.
/// * After the fix: the budgets are derived from the REMAINING watchdog (a
///   third for each retry loop, an eighth for each in-SQLite block), so the
///   composed worst case stops with headroom to spare and the command returns a
///   real error naming what it observed.
///
/// The assertion is deliberately on the WATCHDOG ERROR CODE and the elapsed
/// bound rather than on exit status alone: a command that fails for the right
/// reason inside its budget is the passing state, and being killed by its own
/// retry schedule is the failing one.
#[test]
fn contended_write_finishes_inside_its_own_watchdog() {
    let room = TempRoom::new("contended");
    let _holder = LockHolder::hold(room.facts_db(), Duration::from_millis(HOLD_MS));

    let started = Instant::now();
    let out = room.rally(&[
        "say",
        "claim",
        "--tool",
        "claude_code:contended",
        "--subject",
        "contended-write",
        "--json",
        "--timeout-ms",
        &WATCHDOG_MS.to_string(),
    ]);
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // The regression signature: killed BY the watchdog rather than returning
    // inside it. This is what the pre-fix code does, every time.
    let watchdog_killed = stdout.contains("watchdog-timeout-uncommitted-mutation")
        || stderr.contains("watchdog-timeout-uncommitted-mutation");
    assert!(
        !watchdog_killed,
        "the mutation was killed by its own watchdog instead of returning \
         inside it — the retry budget outlasts the watchdog budget again.\n\
         elapsed={elapsed:?}\nstdout={stdout}\nstderr={stderr}",
    );

    // And it must actually come back before the deadline, with room to spare.
    assert_within(
        elapsed,
        Duration::from_millis(WATCHDOG_MS),
        "contended mutation",
    );

    // It must fail — a holder is genuinely there — and say so honestly.
    assert!(
        !out.status.success(),
        "a write against a held EXCLUSIVE lock must not report success.\n\
         stdout={stdout}",
    );
    let said_something_true = stdout.contains("retry budget exhausted")
        || stderr.contains("retry budget exhausted")
        || stdout.contains("database is locked")
        || stderr.contains("database is locked");
    assert!(
        said_something_true,
        "the error must name what was observed (budget exhausted / locked db), \
         not assert an unobserved cause.\nstdout={stdout}\nstderr={stderr}",
    );
}

/// The genuine-contention half of the contract: a holder that RELEASES inside
/// the budget must leave the write succeeding, so the fix cannot be gamed by
/// simply not retrying.
///
/// A "fix" that dropped retries altogether would pass the control above and
/// fail this one.
#[test]
fn write_still_retries_and_succeeds_when_the_holder_releases() {
    let room = TempRoom::new("releases");
    // Strictly between the busy timeout and the open-loop budget: long enough
    // that SQLite gives up and returns SQLITE_BUSY (so rally's retry loop is
    // genuinely entered), short enough that a retrying writer still lands the
    // fact. See HOLD_RETRYABLE_MS for why 400ms was vacuous.
    let _holder = LockHolder::hold(room.facts_db(), Duration::from_millis(HOLD_RETRYABLE_MS));

    let started = Instant::now();
    let out = room.rally(&[
        "say",
        "claim",
        "--tool",
        "claude_code:releases",
        "--subject",
        "released-write",
        "--json",
        "--timeout-ms",
        &WATCHDOG_MS.to_string(),
    ]);
    let elapsed = started.elapsed();

    assert!(
        out.status.success(),
        "a writer that retries within budget must land the write once the \
         holder releases at {HOLD_RETRYABLE_MS}ms (elapsed={elapsed:?})\n\
         stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_within(
        elapsed,
        Duration::from_millis(WATCHDOG_MS),
        "write after holder released",
    );

    // The fact is really there — not a success report over a dropped write.
    let room_out = room.rally(&["room", "--json", "--timeout-ms", "15000"]);
    let json = stdout_json(&room_out).expect("room --json");
    let claims = json["data"]["room"]["active_claims"].as_array().cloned();
    let landed = match claims {
        Some(list) => list.iter().any(|f| f["subject"] == "released-write"),
        // Some room shapes summarize counts rather than listing facts; fall
        // back to the raw text so the assertion still means something.
        None => String::from_utf8_lossy(&room_out.stdout).contains("released-write"),
    };
    assert!(
        landed,
        "write reported success but left no fact in the room"
    );
}

/// Boundary case: the budget must hold at a watchdog small enough that the
/// OLD schedule's very first sleeps would already blow it.
///
/// The adjacent move to the default-budget control. At 300ms the pre-fix open
/// loop reached its 16-attempt count long after the watchdog had fired; the
/// derived budget scales down with the watchdog instead.
#[test]
fn budget_scales_down_with_a_small_watchdog() {
    let room = TempRoom::new("smallbudget");
    let _holder = LockHolder::hold(room.facts_db(), Duration::from_millis(HOLD_MS));

    // 1000ms, not the 400ms first drafted: at 400ms the modelled margin after
    // the derived budgets is ~120ms, which is less than this debug binary's own
    // startup, so the control measured process spawn rather than the fix.
    const SMALL_MS: u64 = 1000;
    let started = Instant::now();
    let out = room.rally(&[
        "say",
        "claim",
        "--tool",
        "claude_code:small",
        "--subject",
        "small-budget-write",
        "--json",
        "--timeout-ms",
        &SMALL_MS.to_string(),
    ]);
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !stdout.contains("watchdog-timeout-uncommitted-mutation")
            && !stderr.contains("watchdog-timeout-uncommitted-mutation"),
        "a {SMALL_MS}ms budget must bound the retry loop too, not just the \
         default one.\nelapsed={elapsed:?}\nstdout={stdout}\nstderr={stderr}",
    );
    assert_within(
        elapsed,
        Duration::from_millis(SMALL_MS),
        "small-budget mutation",
    );
}

/// NEGATIVE CONTROL — pins the measurement that corrects the reported trigger.
///
/// The defect report attributed the retry storm to a stale or zero-length
/// `-wal`/`-shm` pair. It does not do that. SQLite recovers a zero-length WAL,
/// a garbage non-empty WAL, a mismatched-salt WAL, and a garbage non-empty SHM
/// without ever reporting busy/locked, so none of them reach the retry path.
///
/// Keeping this as a test rather than a note means the correction is checked
/// rather than remembered: if a future SQLite or a future `open_fact_store`
/// ever DOES make a stale WAL block, this fails and the trigger story gets
/// re-opened with evidence.
#[test]
fn stale_wal_is_not_what_trips_the_retry_path() {
    let room = TempRoom::new("stalewal");

    // Measured variants, all fast (0.049s–0.073s) on both 0.1.7 and 0.2.0.
    let variants: [(&str, &[u8], &[u8]); 4] = [
        ("zero-length", &[], &[]),
        ("garbage-nonempty", &[0xde, 0xad, 0xbe, 0xef], &[]),
        // Valid WAL magic, salt that cannot match the db.
        (
            "mismatched-salt",
            &[
                0x37, 0x7f, 0x06, 0x82, 0x00, 0x2d, 0xe2, 0x18, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
                0x00, 0x01, 0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 0xBB, 0xBB,
            ],
            &[],
        ),
        ("garbage-shm", &[], &[0xca, 0xfe, 0xba, 0xbe]),
    ];

    for (label, wal_bytes, shm_bytes) in variants {
        fs::write(room.wal(), wal_bytes).expect("plant wal");
        fs::write(room.shm(), shm_bytes).expect("plant shm");

        let started = Instant::now();
        let out = room.rally(&[
            "say",
            "claim",
            "--tool",
            "claude_code:stalewal",
            "--subject",
            &format!("stale-wal-{label}"),
            "--json",
            "--timeout-ms",
            &WATCHDOG_MS.to_string(),
        ]);
        let elapsed = started.elapsed();

        assert!(
            out.status.success(),
            "a {label} WAL must not fail the write — it is not a lock \
             condition.\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        // Generous bound: the point is "nowhere near a retry storm", not a
        // microbenchmark. A real retry storm took 3000ms+.
        assert_within(
            elapsed,
            Duration::from_millis(1500),
            &format!("{label} WAL (a retry storm took 3000ms+)"),
        );
    }
}
