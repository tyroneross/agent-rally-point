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
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
