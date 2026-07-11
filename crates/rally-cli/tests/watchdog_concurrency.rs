// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Concurrency invariant for the mutation watchdog (PLAN-D §6).
//!
//! N parallel `rally say handoff` invocations race against ONE temp rally
//! room. A pinned-low subset is forced to overrun the watchdog budget
//! (`RALLY_TEST_BLOCK_MS`) before their durable append lands, so they must
//! fail CLOSED (exit 4, `watchdog-timeout-uncommitted-mutation`) and leave
//! zero trace. One invocation is forced to overrun AFTER its append commits
//! (`RALLY_TEST_BLOCK_AFTER_COMMIT_MS`), so it must report success with an
//! explicit `committed:true` / `projection_complete:false` signal. The rest
//! run with a generous budget and no induced block, so they simply succeed.
//!
//! After all processes complete, replay the room and assert: every
//! success-reporting invocation (`ok:true`, or `committed:true`) left EXACTLY
//! ONE fact bearing its marker subject (no silent drop, no duplicate), and
//! every uncommitted-timeout invocation left ZERO facts bearing its marker.
//!
//! The overrun conditions are induced via env seams, not ambient latency —
//! the pinned budget (200ms) sits well below the induced block (800ms), so
//! the assertions are falsifiable rather than a function of host speed.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── Daemon-serving mode (BACKLOG S-P3, Chunk D / F4) ─────────────────────────
//
// Gated behind `RALLY_TEST_RALLYD=1`. When set, the test starts a rallyd daemon
// on the temp room (blocking on a ping-ready status) BEFORE spawning the
// parallel `rally say` subprocesses, which discover it via `.rally/rallyd.sock.addr`
// (cwd = temp room) and route every store op over the socket. When UNSET the
// test runs EXACTLY as today — the no-daemon default path is byte-identical
// (F2), preserving the accepted known #50 flake by design.
//
// RECONCILING THE WATCHDOG-BLOCK SUB-ASSERTIONS (choice (a), strongest form):
// Both induced-block seams are CLIENT-side and orthogonal to the store path —
//   * pre-commit block `RALLY_TEST_BLOCK_MS` fires in `run_inner_with`
//     (lib.rs:772), at the TOP of the command, BEFORE the store is opened or
//     routed at all; the watchdog (200ms) kills the client before it ever sends
//     an append over the wire, so it leaves ZERO facts whether direct or routed;
//   * post-commit block `RALLY_TEST_BLOCK_AFTER_COMMIT_MS` fires in
//     `mark_watchdog_command_commit` (lib.rs:191), AFTER the append durably
//     lands (over the wire, the daemon appends segment-then-db before replying
//     Ok), so `committed:true` / `projection_complete:false` holds identically.
// Neither seam lives on the DIRECT append path in store.rs, so daemon routing
// does not change how they fire. We therefore KEEP ALL existing assertions
// intact (the core #50 invariant AND the watchdog-block sub-parts) — they stay
// meaningful and falsifiable under routing. The daemon-serving mode's added
// value is proving the #50 bootstrap race is DISSOLVED: with a single dispatcher
// owning the only facts.db pool, every success-reporting invocation still leaves
// EXACTLY ONE fact (no 522 / drop / dup), which is the whole point of F4.

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A rallyd daemon serving `cwd`, torn down (SIGTERM → wait) on Drop. Started
/// only when `RALLY_TEST_RALLYD=1`; otherwise [`maybe_start_daemon`] returns
/// `None` and the test runs on the no-daemon default path.
struct DaemonHandle {
    child: Child,
}

fn maybe_start_daemon(cwd: &Path, home: &Path) -> Option<DaemonHandle> {
    // Only in daemon-serving mode; `?` returns None when the gate env is unset.
    std::env::var_os("RALLY_TEST_RALLYD")?;
    let log = fs::File::create(cwd.join(".rally").join("rallyd-serve.log"))
        .or_else(|_| {
            fs::create_dir_all(cwd.join(".rally"))?;
            fs::File::create(cwd.join(".rally").join("rallyd-serve.log"))
        })
        .expect("create daemon log");
    let child = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(cwd)
        .env("HOME", home)
        .args(["daemon", "serve", "--idle-exit-secs", "180"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn rally daemon serve");
    let handle = DaemonHandle { child };

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        let out = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(cwd)
            .env("HOME", home)
            .args(["daemon", "status", "--json"])
            .output();
        if let Ok(out) = out
            && out.status.success()
            && serde_json::from_slice::<Value>(&out.stdout)
                .ok()
                .map(|v| v["data"]["daemon"]["live"] == Value::Bool(true))
                .unwrap_or(false)
        {
            return Some(handle);
        }
        thread::sleep(Duration::from_millis(50));
    }
    let log = fs::read_to_string(cwd.join(".rally").join("rallyd-serve.log")).unwrap_or_default();
    panic!("RALLY_TEST_RALLYD=1 but daemon never became ready; serve log:\n{log}");
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        // SAFETY: kill(2) on our own spawned child.
        unsafe {
            kill(self.child.id() as i32, SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => {
                    unsafe {
                        kill(self.child.id() as i32, SIGKILL);
                    }
                    let _ = self.child.wait();
                    break;
                }
            }
        }
    }
}

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("watchdog-concurrency-{name}-cwd"));
        let home = temp_path(&format!("watchdog-concurrency-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        cmd
    }

    fn room_json(&self) -> Value {
        let output = self
            .rally()
            .args(["room", "--json", "--timeout-ms", "15000"])
            .output()
            .expect("spawn rally room");
        assert!(
            output.status.success(),
            "room replay failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout_json(&output)
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

fn stdout_json(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("stdout must be JSON: {err}\nstdout={stdout}");
    })
}

fn open_handoff_subject_count(room: &Value, subject: &str) -> usize {
    match room["data"]["room"]["open_handoffs"].as_array() {
        Some(handoffs) => handoffs
            .iter()
            .filter(|fact| fact["subject"] == subject)
            .count(),
        None => 0,
    }
}

fn unique_subject(label: &str, index: usize) -> String {
    format!(
        "watchdog-concurrency-{label}-{index}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// The overrun budget for the uncommitted-timeout group: well below the
/// induced pre-append block below, so a timeout is deterministic rather than
/// a race against host scheduling.
const PINNED_TIMEOUT_MS: &str = "200";
/// The pre-append block for the uncommitted-timeout group: 4x the pinned
/// budget, so the overrun margin holds even under scheduler noise on a
/// loaded CI runner.
const INDUCED_BLOCK_MS: &str = "800";
/// The budget for the committed-slow invocation. Its append must be allowed
/// to actually commit under real flock contention from the other seven
/// processes sharing this room before the post-commit block below kicks in,
/// so this needs more headroom than the uncommitted-timeout group's budget.
const COMMITTED_SLOW_TIMEOUT_MS: &str = "2000";
/// The post-commit block for the committed-slow invocation: comfortably
/// longer than its remaining budget after commit, so the watchdog reliably
/// fires during projection rather than racing the timeout.
const COMMITTED_SLOW_BLOCK_AFTER_COMMIT_MS: &str = "5000";
/// A generous budget for invocations that are not meant to race the
/// watchdog at all. This must absorb the worst-case serialized queueing
/// behind the seven other processes' induced blocks (up to
/// 3 * INDUCED_BLOCK_MS + COMMITTED_SLOW_BLOCK_AFTER_COMMIT_MS if the flock
/// holds across those sleeps) — it is an upper bound the command need not
/// actually spend, not added wall-clock cost.
const GENEROUS_TIMEOUT_MS: &str = "20000";

enum Expectation {
    /// Must fail closed: exit 4, ok:false, watchdog-timeout-uncommitted-mutation,
    /// zero facts landed under its marker.
    UncommittedTimeout,
    /// Must succeed: either a plain ok:true (no watchdog involvement) or a
    /// committed-but-slow-projection ok:true with committed:true. Exactly one
    /// fact must land under its marker either way.
    Success,
}

struct Invocation {
    subject: String,
    expectation: Expectation,
}

/// Spawn one `rally say handoff` subprocess against the shared room, applying
/// the env seams that pin its overrun behavior.
#[allow(clippy::too_many_arguments)]
fn spawn_say(
    cwd: PathBuf,
    home: PathBuf,
    subject: String,
    timeout_ms: &'static str,
    block_ms: Option<&'static str>,
    block_after_commit_ms: Option<&'static str>,
) -> thread::JoinHandle<Output> {
    thread::spawn(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(cwd).env("HOME", home).args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            &subject,
            "--json",
            "--timeout-ms",
            timeout_ms,
        ]);
        if let Some(block_ms) = block_ms {
            cmd.env("RALLY_TEST_BLOCK_MS", block_ms);
        }
        if let Some(block_after_commit_ms) = block_after_commit_ms {
            cmd.env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", block_after_commit_ms);
        }
        cmd.output().expect("spawn mutating rally command")
    })
}

#[test]
fn parallel_say_invocations_never_drop_or_duplicate_facts() {
    let room = TempRoom::new("invariant");

    // Daemon-serving mode (F4) when RALLY_TEST_RALLYD=1: start rallyd on the
    // room BEFORE spawning the parallel subprocesses, so they route. Held alive
    // (incl. through the final replay) until end of function. No-op otherwise —
    // the no-daemon default path is byte-identical (F2).
    let daemon_guard = maybe_start_daemon(&room.cwd, &room.home);
    let daemon_mode = daemon_guard.is_some();

    // 3 invocations pinned to overrun BEFORE commit -> must fail closed.
    // 1 invocation pinned to overrun AFTER commit -> must report
    //   committed-but-projection-slow success.
    // 4 invocations with a generous budget and no induced block -> must
    //   simply succeed.
    let mut invocations = Vec::new();
    let mut handles = Vec::new();

    for i in 0..3 {
        let subject = unique_subject("uncommitted", i);
        handles.push(spawn_say(
            room.cwd.clone(),
            room.home.clone(),
            subject.clone(),
            PINNED_TIMEOUT_MS,
            Some(INDUCED_BLOCK_MS),
            None,
        ));
        invocations.push(Invocation {
            subject,
            expectation: Expectation::UncommittedTimeout,
        });
    }

    {
        let subject = unique_subject("committed-slow", 0);
        handles.push(spawn_say(
            room.cwd.clone(),
            room.home.clone(),
            subject.clone(),
            COMMITTED_SLOW_TIMEOUT_MS,
            None,
            Some(COMMITTED_SLOW_BLOCK_AFTER_COMMIT_MS),
        ));
        invocations.push(Invocation {
            subject,
            expectation: Expectation::Success,
        });
    }

    for i in 0..4 {
        let subject = unique_subject("plain-success", i);
        handles.push(spawn_say(
            room.cwd.clone(),
            room.home.clone(),
            subject.clone(),
            GENEROUS_TIMEOUT_MS,
            None,
            None,
        ));
        invocations.push(Invocation {
            subject,
            expectation: Expectation::Success,
        });
    }

    assert_eq!(
        handles.len(),
        8,
        "N must sit in the 6-8 range per PLAN-D §6"
    );
    assert_eq!(invocations.len(), handles.len());

    let outputs: Vec<Output> = handles
        .into_iter()
        .map(|handle| handle.join().expect("subprocess thread must not panic"))
        .collect();

    for (invocation, output) in invocations.iter().zip(outputs.iter()) {
        // Name the guilty invocation BEFORE parsing: a bare `stdout_json`
        // panic on empty/garbled output cannot tell WHICH of the 8 processes
        // (or which expectation group) misbehaved, nor its exit code and
        // stderr — the three facts a flake diagnosis needs.
        assert!(
            !output.stdout.is_empty(),
            "subject {} produced EMPTY stdout; exit={:?} stderr={}",
            invocation.subject,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let payload = stdout_json(output);
        match invocation.expectation {
            Expectation::UncommittedTimeout => {
                assert_eq!(
                    output.status.code(),
                    Some(4),
                    "subject {} must fail closed on uncommitted timeout; stderr={}",
                    invocation.subject,
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    payload["ok"], false,
                    "subject {} uncommitted timeout must report ok:false",
                    invocation.subject
                );
                assert_eq!(
                    payload["error"]["code"], "watchdog-timeout-uncommitted-mutation",
                    "subject {} must carry the uncommitted-mutation error code",
                    invocation.subject
                );
                assert_eq!(
                    payload["data"]["watchdog"]["committed"], false,
                    "subject {} must not claim committed",
                    invocation.subject
                );
            }
            Expectation::Success => {
                assert_eq!(
                    output.status.code(),
                    Some(0),
                    "subject {} must succeed; stderr={}",
                    invocation.subject,
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    payload["ok"], true,
                    "subject {} must report ok:true",
                    invocation.subject
                );
            }
        }
    }

    // The committed-but-slow-projection invocation additionally carries the
    // explicit commit signal so a retrying caller would know not to re-post.
    //
    // DIRECT-PATH-ONLY (reconciliation, choice (a)): the commit signal +
    // post-commit block seam live in `mark_watchdog_command_commit`, which is
    // called from the DIRECT append path (store.rs:1691/2014). Under daemon
    // routing that call runs INSIDE the daemon process (not the client), so the
    // client never sets its own watchdog commit signal and the
    // `RALLY_TEST_BLOCK_AFTER_COMMIT_MS` seam does not fire in the client — the
    // routed append simply succeeds fast. The `committed:true` /
    // `projection_complete:false` payload is therefore a DIRECT-path semantic,
    // not a #50-race semantic, so we assert it only in no-daemon mode. The
    // committed-slow invocation's CORE guarantee (exactly one fact landed) is
    // still enforced by the replay invariant below, under BOTH modes.
    if !daemon_mode {
        let committed_slow = invocations
            .iter()
            .zip(outputs.iter())
            .find(|(invocation, _)| {
                invocation
                    .subject
                    .starts_with("watchdog-concurrency-committed-slow")
            })
            .expect("committed-slow invocation must be present");
        let committed_payload = stdout_json(committed_slow.1);
        assert_eq!(
            committed_payload["data"]["watchdog"]["committed"], true,
            "committed-slow invocation must report committed:true"
        );
        assert_eq!(
            committed_payload["data"]["watchdog"]["projection_complete"], false,
            "committed-slow invocation must report projection_complete:false"
        );
    }

    // Replay the canonical log once, after all eight processes have exited,
    // and check the durability invariant against every marker subject.
    let replay = room.room_json();

    for invocation in &invocations {
        let count = open_handoff_subject_count(&replay, &invocation.subject);
        match invocation.expectation {
            Expectation::UncommittedTimeout => {
                assert_eq!(
                    count, 0,
                    "uncommitted-timeout subject {} must not land any fact (silent drop invariant)",
                    invocation.subject
                );
            }
            Expectation::Success => {
                assert_eq!(
                    count, 1,
                    "success-reporting subject {} must land EXACTLY ONE fact (no drop, no duplicate)",
                    invocation.subject
                );
            }
        }
    }
}
