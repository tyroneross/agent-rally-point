// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The rally binary is spawned through the shared choke point so this file
// cannot silently inherit the production 3s watchdog budget again. Included
// by path rather than via `mod support;` so the unrelated ChannelSandbox is
// not compiled into this binary.
#[path = "support/envelope.rs"]
mod envelope;
#[path = "support/rally_cmd.rs"]
mod rally_cmd;

use envelope::{assert_refusal, reject_watchdog};
use rally_cmd::rally_command;

struct Room {
    cwd: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

static ROOM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

impl Room {
    fn new() -> Self {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = ROOM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cwd = std::env::temp_dir().join(format!("rht-{pid}-{n}-{unique}-cwd"));
        let home = std::env::temp_dir().join(format!("rht-{pid}-{n}-{unique}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".rally")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let room = Self { cwd, home, bin };
        room.set_tmux_live(true);
        room
    }

    fn run(&self, session: &str, args: &[&str]) -> Output {
        let inherited = std::env::var("PATH").unwrap_or_default();
        rally_command()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            // Strip the ambient CI identity. `derive_endpoint` ranks
            // cloud job ABOVE managed pane, so under GitHub Actions every
            // simulated session in this file collapses to the same
            // `cloud:github-actions:<run-id>` — one run id shared by all
            // children — and receiver binding can no longer tell two
            // sessions apart. Locally these vars are unset, which is why
            // this only ever failed on the runner.
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("RALLY_SESSION_ID")
            .env_remove("RALLY_MANAGED_SESSION_MODE")
            .env("TERM_SESSION_ID", session)
            .env("PATH", format!("{}:{inherited}", self.bin.display()))
            .args(args)
            .output()
            .unwrap()
    }

    fn run_managed(&self, session: &str, args: &[&str]) -> Output {
        let inherited = std::env::var("PATH").unwrap_or_default();
        rally_command()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env("RALLY_SESSION_ID", session)
            .env("RALLY_MANAGED_SESSION_MODE", "persistent")
            .env("PATH", format!("{}:{inherited}", self.bin.display()))
            .args(args)
            .output()
            .unwrap()
    }

    fn json_parent(&self, session: &str, args: &[&str]) -> Value {
        let out = rally_command()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("RALLY_MANAGED_SESSION_MODE")
            .env("RALLY_SESSION_ID", session)
            .args(args)
            .output()
            .unwrap();
        Self::parse(args, out)
    }

    fn json(&self, session: &str, args: &[&str]) -> Value {
        Self::parse(args, self.run(session, args))
    }

    fn json_managed(&self, session: &str, args: &[&str]) -> Value {
        Self::parse(args, self.run_managed(session, args))
    }

    /// Parse a command's JSON envelope, refusing to hand back a watchdog
    /// envelope.
    ///
    /// This is the choke point for the SILENT half of the watchdog bug. Every
    /// `ok`-only assertion in this file — 23 of them assert `ok == true`, one
    /// asserts `ok == false` — is satisfiable by a watchdog envelope, because
    /// four of the five emission sites are `ok: false` and the post-commit
    /// projection-abandoned one is `ok: true`. Rejecting the shape once here
    /// means no assertion downstream has to remember to.
    fn parse(args: &[&str], out: Output) -> Value {
        let bytes = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let value: Value = serde_json::from_slice(bytes)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(bytes)));
        reject_watchdog(&value, &format!("{args:?}"));
        value
    }

    fn artifact(&self) -> (String, String) {
        let v = self.json(
            "session-author",
            &[
                "say",
                "artifact",
                "--tool",
                "author:a",
                "--subject",
                "build",
                "--json",
            ],
        );
        (
            v["data"]["say"]["fact"]["event_id"]
                .as_str()
                .unwrap()
                .to_string(),
            v["data"]["say"]["fact"]["from_session_id"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    fn set_tmux_live(&self, live: bool) {
        let path = self.bin.join("tmux");
        fs::write(
            &path,
            if live {
                "#!/bin/sh\necho pane-one pane-two\nexit 0\n"
            } else {
                "#!/bin/sh\necho 'no server running' >&2\nexit 1\n"
            },
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn adopt(&self, name: &str, tool: &str, target: &str) -> String {
        let value = self.json(
            "session-adopter",
            &[
                "adopt", name, "--tool", tool, "--agent", "codex", "--tmux", target, "--json",
            ],
        );
        assert_eq!(value["ok"], true, "{value}");
        value["data"]["adopt"]["session"]["session_id"]
            .as_str()
            .expect("adopt session id")
            .to_string()
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

struct Daemon(Child);

impl Daemon {
    fn start(room: &Room) -> Self {
        let inherited = std::env::var("PATH").unwrap_or_default();
        let child = rally_command()
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env("PATH", format!("{}:{inherited}", room.bin.display()))
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start rally daemon");
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            // Readiness is `data.daemon.live`, not the exit status. `daemon
            // status` exits 0 while reporting `live: false`, so the old gate
            // returned on its first poll and handed the three
            // `..._in_direct_and_daemon_modes` tests into the startup window
            // instead of past it. See `rally_cmd::daemon_is_live`.
            let out = room.run("daemon-probe", &["daemon", "status", "--json"]);
            if rally_cmd::daemon_is_live(&out.stdout) {
                return daemon;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("rally daemon did not start")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

#[test]
fn referenced_handoff_blocks_wrong_target_before_append_and_binds_author_session() {
    let room = Room::new();
    let (artifact, author_session) = room.artifact();
    let wrong = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--target",
            "author:wrong",
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_refusal(&wrong, "handoff_target_mismatch");
    let before = room.json("session-reviewer", &["room", "--json"]);
    assert_eq!(
        before["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let ok = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_eq!(ok["ok"], true);
    let fact = &ok["data"]["say"]["fact"];
    assert_eq!(fact["target"], "author:a");
    assert!(
        fact["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &Value::String(format!("protocol:to_session_id={author_session}")))
    );
}

#[test]
fn concurrent_reply_batches_do_not_grant_same_file_edit_or_merge_authority() {
    same_file_collaboration(false);
}

#[test]
fn competing_merge_batches_recheck_same_file_authority_after_transfer() {
    same_file_collaboration(true);
}

fn same_file_collaboration(competing_merges: bool) {
    use std::process::Command;
    use std::sync::{Arc, Barrier};
    for routed in [false, true] {
        let room = Room::new();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(&room.cwd)
                .env("HOME", &room.home)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=Rally Test",
                    "-c",
                    "user.email=rally@example.test",
                ])
                .args(args)
                .output()
                .unwrap()
        };
        let git_ok = |args: &[&str]| {
            let out = git(args);
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git_ok(&["init", "--initial-branch=main"]);
        fs::write(room.cwd.join("shared.rs"), "base\n").unwrap();
        git_ok(&["add", "shared.rs"]);
        git_ok(&["commit", "-m", "base"]);
        let base = git_ok(&["rev-parse", "HEAD"]);
        for branch in ["codex/agent-a", "codex/agent-b"] {
            git_ok(&["branch", branch]);
        }
        let _daemon = routed.then(|| Daemon::start(&room));
        let tools = ["peer:a", "peer:b"];
        let sessions = ["session-peer-a", "session-peer-b"];
        let requests = tools.map(|tool| {
            let response = room.json(
                "session-sender",
                &[
                    "say",
                    "handoff",
                    "--tool",
                    "sender:s",
                    "--target",
                    tool,
                    "--path",
                    "shared.rs",
                    "--subject",
                    "edit shared file",
                    "--deliver",
                    "record",
                    "--json",
                ],
            );
            assert_eq!(response["ok"], true);
            response["data"]["say"]["fact"]["event_id"]
                .as_str()
                .unwrap()
                .to_owned()
        });
        for i in 0..2 {
            assert_eq!(
                room.json(sessions[i], &["ack", "--tool", tools[i], "--json"])["ok"],
                true
            );
            let accepted = room.json(
                sessions[i],
                &[
                    "say",
                    "handoff",
                    "--tool",
                    tools[i],
                    "--ref",
                    &requests[i],
                    "--handoff-state",
                    "accepted",
                    "--subject",
                    "accepted",
                    "--deliver",
                    "record",
                    "--json",
                ],
            );
            assert_eq!(accepted["ok"], true, "{accepted}");
        }
        let barrier = Arc::new(Barrier::new(6));
        let (claims, observations) = thread::scope(|scope| {
            let claims = (0..2)
                .map(|i| {
                    let barrier = barrier.clone();
                    let room = &room;
                    scope.spawn(move || {
                        barrier.wait();
                        (
                            i,
                            room.json(
                                sessions[i],
                                &[
                                    "say",
                                    "claim",
                                    "--tool",
                                    tools[i],
                                    "--path",
                                    "shared.rs",
                                    "--subject",
                                    "exclusive edit",
                                    "--json",
                                ],
                            ),
                        )
                    })
                })
                .collect::<Vec<_>>();
            let readers = (0..4)
                .map(|_| {
                    let barrier = barrier.clone();
                    let room = &room;
                    let requests = &requests;
                    scope.spawn(move || {
                        barrier.wait();
                        (0..3)
                            .map(|_| {
                                room.json(
                                    "session-observer",
                                    &[
                                        "replies",
                                        "--ref",
                                        &requests[0],
                                        "--ref",
                                        &requests[1],
                                        "--json",
                                    ],
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>();
            (
                claims
                    .into_iter()
                    .map(|t| t.join().unwrap())
                    .collect::<Vec<_>>(),
                readers
                    .into_iter()
                    .flat_map(|t| t.join().unwrap())
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(observations.len(), 12);
        for observation in observations {
            assert_eq!(observation["ok"], true);
            let rows = observation["data"]["replies"]["rows"].as_array().unwrap();
            assert_eq!(rows.len(), 2);
            for row in rows {
                assert_eq!(row["fact"]["scope"], serde_json::json!(["file:shared.rs"]));
                assert_eq!(row["reply"]["state"], "accepted");
                assert!(!row["reply_fact"].is_null());
            }
        }
        let winners = claims
            .iter()
            .filter(|(_, response)| response["ok"] == true)
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1, "{claims:?}");
        let winner = winners[0].0;
        for (i, response) in &claims {
            if *i != winner {
                assert_eq!(response["ok"], false);
                assert!(
                    response["error"]
                        .as_str()
                        .is_some_and(|message| message.contains("claim conflict")),
                    "{response}"
                );
            }
            let checked_output = room.run(
                sessions[*i],
                &[
                    "check",
                    "before-write",
                    "--tool",
                    tools[*i],
                    "--path",
                    "shared.rs",
                    "--strict",
                    "--json",
                ],
            );
            assert_eq!(
                checked_output.status.code(),
                Some(if *i == winner { 0 } else { 4 })
            );
            let check = Room::parse(&["check", "before-write"], checked_output);
            assert_eq!(check["data"]["check"]["phase"], "before-write");
            assert_eq!(check["data"]["check"]["allow"], *i == winner, "{check}");
        }
        let snapshot = room.json("session-observer", &["room", "--json"]);
        assert_eq!(
            snapshot["data"]["room"]["active_claims"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let canonical_claims = fs::read_dir(room.cwd.join(".rally/log"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .flat_map(|path| {
                fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .filter(|line| {
                line["payload"]["kind"] == "claim" && line["payload"]["subject"] == "exclusive edit"
            })
            .count();
        assert_eq!(
            canonical_claims, 1,
            "losing claim must not be appended canonically"
        );
        for i in 0..2 {
            let output = room.run(
                sessions[i],
                &[
                    "check",
                    "coordination",
                    "--tool",
                    tools[i],
                    "--changed",
                    "shared.rs",
                    "--strict",
                    "--json",
                ],
            );
            assert_eq!(output.status.code(), Some(if i == winner { 0 } else { 4 }));
            let checked = Room::parse(&["check", "coordination"], output);
            assert_eq!(checked["data"]["check"]["phase"], "coordination");
            assert_eq!(checked["data"]["check"]["pass"], i == winner);
        }
        let claim_id = winners[0].1["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap();
        let loser = 1 - winner;
        let unauthorized = room.json(
            sessions[loser],
            &[
                "say",
                "release",
                "--tool",
                tools[loser],
                "--ref",
                claim_id,
                "--subject",
                "steal claim",
                "--json",
            ],
        );
        assert_eq!(unauthorized["ok"], false, "{unauthorized}");
        let claim_read = room.json(
            "session-observer",
            &["replies", "--ref", claim_id, "--json"],
        );
        assert_eq!(
            claim_read["data"]["replies"]["rows"][0]["fact"]["kind"],
            "claim"
        );
        assert!(
            claim_read["data"]["replies"]["rows"][0]["reply"].is_null(),
            "a batch observation is not a file authorization"
        );
        assert_eq!(
            git_ok(&["rev-parse", "HEAD"]),
            base,
            "readers and claims never merge or edit the file"
        );
        assert_eq!(
            fs::read_to_string(room.cwd.join("shared.rs")).unwrap(),
            "base\n"
        );
        // Even when a caller ignores the denied Rally check, observation cannot
        // make two conflicting branch edits safe. Git must surface the conflict.
        git_ok(&["checkout", "codex/agent-a"]);
        fs::write(room.cwd.join("shared.rs"), "agent-a\n").unwrap();
        git_ok(&["commit", "-am", "agent a edit"]);
        git_ok(&["checkout", "codex/agent-b"]);
        fs::write(room.cwd.join("shared.rs"), "agent-b\n").unwrap();
        git_ok(&["commit", "-am", "agent b edit"]);
        git_ok(&["checkout", "main"]);
        let winner_branch = if winner == 0 {
            "codex/agent-a"
        } else {
            "codex/agent-b"
        };
        let loser_branch = if winner == 0 {
            "codex/agent-b"
        } else {
            "codex/agent-a"
        };
        // A reply batch grants no merge authority. Two agents request a merge
        // concurrently while four observers read batches; only the current
        // claim owner may invoke Git. This tests cooperative callers, not an
        // atomic Git landing queue or agents bypassing the coordination gate.
        let merge_wave = |owner: usize, should_succeed: bool| {
            let barrier = Arc::new(Barrier::new(6));
            thread::scope(|scope| {
                let mergers = (0..2)
                    .map(|i| {
                        let barrier = barrier.clone();
                        let room = &room;
                        let git = &git;
                        scope.spawn(move || {
                            barrier.wait();
                            let output = room.run(
                                sessions[i],
                                &[
                                    "check",
                                    "coordination",
                                    "--tool",
                                    tools[i],
                                    "--changed",
                                    "shared.rs",
                                    "--strict",
                                    "--json",
                                ],
                            );
                            assert_eq!(
                                output.status.code(),
                                Some(if i == owner { 0 } else { 4 }),
                                "merge wave owner={owner}, actor={i}, routed={routed}: {}",
                                String::from_utf8_lossy(&output.stdout)
                            );
                            let check = Room::parse(&["check", "coordination"], output);
                            assert_eq!(check["data"]["check"]["phase"], "coordination");
                            assert_eq!(check["data"]["check"]["pass"], i == owner);
                            if i != owner {
                                return false;
                            }
                            let branch = ["codex/agent-a", "codex/agent-b"][i];
                            let merged = git(&["merge", "--no-edit", branch]);
                            assert_eq!(
                                merged.status.success(),
                                should_succeed,
                                "owner merge: {} {}",
                                String::from_utf8_lossy(&merged.stdout),
                                String::from_utf8_lossy(&merged.stderr)
                            );
                            true
                        })
                    })
                    .collect::<Vec<_>>();
                let readers = (0..4)
                    .map(|_| {
                        let barrier = barrier.clone();
                        let room = &room;
                        let requests = &requests;
                        scope.spawn(move || {
                            barrier.wait();
                            for _ in 0..3 {
                                let batch = room.json(
                                    "session-observer",
                                    &[
                                        "replies",
                                        "--ref",
                                        &requests[0],
                                        "--ref",
                                        &requests[1],
                                        "--json",
                                    ],
                                );
                                assert_eq!(batch["ok"], true);
                                let rows = batch["data"]["replies"]["rows"].as_array().unwrap();
                                assert_eq!(rows.len(), 2);
                                for row in rows {
                                    assert_eq!(row["reply"]["state"], "accepted");
                                    assert_eq!(
                                        row["fact"]["scope"],
                                        serde_json::json!(["file:shared.rs"])
                                    );
                                }
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                let invoked_git = mergers
                    .into_iter()
                    .map(|job| usize::from(job.join().unwrap()))
                    .sum::<usize>();
                assert_eq!(invoked_git, 1, "only the current owner may attempt a merge");
                for reader in readers {
                    reader.join().unwrap();
                }
            });
        };
        if competing_merges {
            merge_wave(winner, true);
        } else {
            git_ok(&["merge", "--ff-only", winner_branch]);
        }
        let accepted_head = git_ok(&["rev-parse", "HEAD"]);
        assert!(!git(&["merge", "--no-edit", loser_branch]).status.success());
        assert_eq!(git_ok(&["rev-parse", "HEAD"]), accepted_head);
        let conflicted = fs::read_to_string(room.cwd.join("shared.rs")).unwrap();
        assert!(
            conflicted.contains("<<<<<<<")
                && conflicted.contains("agent-a")
                && conflicted.contains("agent-b")
        );
        git_ok(&["merge", "--abort"]);
        assert_eq!(
            fs::read_to_string(room.cwd.join("shared.rs")).unwrap(),
            if winner == 0 {
                "agent-a\n"
            } else {
                "agent-b\n"
            }
        );
        let released = room.json(
            sessions[winner],
            &[
                "say",
                "release",
                "--tool",
                tools[winner],
                "--ref",
                claim_id,
                "--subject",
                "handover",
                "--json",
            ],
        );
        assert_eq!(released["ok"], true, "{released}");
        let next_claim = room.json(
            sessions[loser],
            &[
                "say",
                "claim",
                "--tool",
                tools[loser],
                "--path",
                "shared.rs",
                "--subject",
                "next owner",
                "--json",
            ],
        );
        assert_eq!(next_claim["ok"], true, "{next_claim}");
        if competing_merges {
            // Both old accepted replies remain visible. Ownership has changed:
            // the old owner is refused, and the new owner's conflicting branch
            // must fail visibly without replacing the first accepted commit.
            merge_wave(loser, false);
            assert_eq!(git_ok(&["rev-parse", "HEAD"]), accepted_head);
            let conflict = fs::read_to_string(room.cwd.join("shared.rs")).unwrap();
            assert!(
                conflict.contains("<<<<<<<")
                    && conflict.contains("agent-a")
                    && conflict.contains("agent-b")
            );
            git_ok(&["merge", "--abort"]);
            assert_eq!(git_ok(&["rev-parse", "HEAD"]), accepted_head);
        }
        let old_owner = room.json(
            sessions[winner],
            &[
                "check",
                "before-write",
                "--tool",
                tools[winner],
                "--path",
                "shared.rs",
                "--strict",
                "--json",
            ],
        );
        assert_eq!(
            old_owner["data"]["check"]["allow"], false,
            "a stale batch result cannot grant old ownership: {old_owner}"
        );
    }
}

#[test]
fn inbox_suggested_acceptance_keeps_bound_work_open_until_evidenced_completion() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let request = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--deliver",
            "record",
            "--json",
        ],
    );
    let handoff = request["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let inbox = room.json("session-author", &["inbox", "--tool", "author:a", "--json"]);
    let command = inbox["data"]["inbox"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["event_id"] == handoff)
        .unwrap()["ack_command"]
        .as_str()
        .unwrap();
    assert!(command.contains("--handoff-state accepted"), "{command}");
    let words = shlex::split(command).unwrap();
    let args = words.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    let wrong_session = room.json("session-sibling", &args);
    assert_refusal(&wrong_session, "handoff_reply_author_mismatch");
    let accepted = room.json("session-author", &args);
    assert_eq!(accepted["ok"], true, "{accepted}");
    let inbox = room.json("session-author", &["inbox", "--tool", "author:a", "--json"]);
    assert_eq!(inbox["data"]["inbox"]["count"], 0);
    assert_eq!(inbox["data"]["inbox"]["accepted_open"], 1);
    let snapshot = room.json("session-author", &["room", "--json"]);
    assert!(
        snapshot["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| fact["event_id"] == handoff)
    );
    let completed = room.json(
        "session-author",
        &[
            "say",
            "artifact",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--subject",
            "review results",
            "--evidence",
            "review checks passed",
            "--json",
        ],
    );
    assert_eq!(completed["ok"], true, "{completed}");
    let snapshot = room.json("session-author", &["room", "--json"]);
    assert!(
        !snapshot["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| fact["event_id"] == handoff)
    );
}

#[test]
fn handoff_injection_returns_pending_by_default_and_waits_only_when_requested() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let request = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "async handoff",
            "--deliver",
            "record",
            "--json",
        ],
    );
    let handoff = request["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let args = [
        "inject",
        "author:a",
        "--tool",
        "author:a",
        "--handoff",
        handoff,
        "--timeout-seconds",
        "1",
        "--json",
    ];
    let pending = room.json("session-author", &args);
    let injection = &pending["data"]["inject"];
    assert_eq!(injection["ack_state"], "pending");
    assert_eq!(injection["verified_received"], false);
    assert!(injection["ack"].is_null());
    assert_ne!(injection["fallback_plan"]["trigger"], "ack_timeout");
    let mut waiting = args.to_vec();
    waiting.push("--wait-ack");
    let waited = room.json("session-author", &waiting);
    assert_eq!(waited["data"]["inject"]["ack_state"], "timeout");
    assert_eq!(waited["data"]["inject"]["verified_received"], false);
    let accepted = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--handoff-state",
            "accepted",
            "--subject",
            "accepted",
            "--deliver",
            "record",
            "--json",
        ],
    );
    assert_eq!(accepted["ok"], true);
    let received = room.json("session-author", &args);
    assert_eq!(received["data"]["inject"]["ack_state"], "acked");
    assert_eq!(received["data"]["inject"]["ack"]["resolved"], false);
    assert_eq!(received["data"]["inject"]["mode"], "already-received");
    let batch = room.json(
        "session-reviewer",
        &["replies", "--ref", handoff, "--ref", "missing", "--json"],
    );
    assert_eq!(
        batch["data"]["replies"]["rows"][0]["reply"]["state"],
        "accepted"
    );
    assert_eq!(
        batch["data"]["replies"]["rows"][0]["reply_fact"]["event_id"],
        accepted["data"]["say"]["fact"]["event_id"]
    );
    assert_eq!(batch["data"]["replies"]["rows"][1]["found"], false);
}

#[test]
fn unbound_inbox_acceptance_binds_receiver_without_claiming_completion() {
    let room = Room::new();
    room.artifact();
    let request = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--target",
            "author:a",
            "--subject",
            "unbound work",
            "--deliver",
            "record",
            "--json",
        ],
    );
    let id = request["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let inbox = room.json("session-author", &["inbox", "--tool", "author:a", "--json"]);
    let words = shlex::split(
        inbox["data"]["inbox"]["items"][0]["ack_command"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let args = words.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    assert!(args.contains(&"accepted"));
    assert_eq!(room.json("session-author", &args)["ok"], true);
    let inbox = room.json("session-author", &["inbox", "--tool", "author:a", "--json"]);
    assert_eq!(inbox["data"]["inbox"]["accepted_open"], 1);
    assert_refusal(
        &room.json("session-sibling", &args),
        "handoff_reply_author_mismatch",
    );
    let batch = room.json("session-reviewer", &["replies", "--ref", id, "--json"]);
    assert_eq!(
        batch["data"]["replies"]["rows"][0]["reply"]["state"],
        "accepted"
    );
    assert!(batch["data"]["replies"]["rows"][0]["completion_status"].is_null());
}

#[test]
fn retry_is_idempotent_and_only_exact_bound_receiver_can_reply() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let args = [
        "say",
        "handoff",
        "--tool",
        "reviewer:r",
        "--ref",
        &artifact,
        "--subject",
        "review",
        "--json",
    ];
    let first = room.json("session-reviewer", &args);
    let second = room.json("session-reviewer", &args);
    let handoff = first["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(second["data"]["say"]["fact"]["event_id"], handoff);
    let snapshot = room.json("session-reviewer", &["room", "--json"]);
    assert_eq!(
        snapshot["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let spoof = room.json(
        "session-spoof",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            &handoff,
            "--handoff-state",
            "acked",
            "--subject",
            "ACK",
            "--json",
        ],
    );
    assert_refusal(&spoof, "handoff_reply_author_mismatch");

    let reply = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            &handoff,
            "--handoff-state",
            "acked",
            "--subject",
            "ACK",
            "--json",
        ],
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["data"]["say"]["fact"]["target"], "reviewer:r");
    assert!(
        reply["data"]["say"]["fact"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "protocol:event_kind=handoff.acked")
    );
}

#[test]
fn third_party_cannot_bypass_a_bound_receiver_or_implicit_reply_state() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let initial = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_eq!(initial["ok"], true, "{initial}");
    let handoff = initial["data"]["say"]["fact"]["event_id"].as_str().unwrap();

    let implicit = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--subject",
            "looks good",
            "--json",
        ],
    );
    assert_refusal(&implicit, "handoff_reply_state_required");

    let attacker_session = room.adopt("attacker", "attacker:x", "pane-one");
    let bypass = room.json(
        "session-attacker",
        &[
            "say",
            "handoff",
            "--tool",
            "attacker:x",
            "--ref",
            handoff,
            "--target-policy",
            "third-party",
            "--target",
            &attacker_session,
            "--handoff-state",
            "acked",
            "--subject",
            "forged ACK",
            "--json",
        ],
    );
    assert_refusal(&bypass, "handoff_third_party_reply_forbidden");

    let snapshot = room.json("session-author", &["room", "--json"]);
    let reply_count = snapshot["data"]["room"]["open_handoffs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|fact| fact["ref"] == handoff)
        .count();
    assert_eq!(reply_count, 0, "rejections must append no reply fact");
}

/// `protocol:` evidence is reserved on `say handoff`, but not on the other
/// kinds a handoff is allowed to cite. So an author could plant
/// `protocol:correlation_id=<foreign>` on an ordinary artifact, and every
/// handoff citing that artifact used to adopt the planted value as its own
/// correlation and `thread_id` — a binding that contradicts the thread of the
/// artifact it cites. Correlation now comes from the cited fact's `thread_id`.
#[test]
fn handoff_correlation_comes_from_the_cited_fact_not_a_planted_marker() {
    let room = Room::new();
    let poisoned = room.json(
        "session-author",
        &[
            "say",
            "artifact",
            "--tool",
            "author:a",
            "--subject",
            "build",
            "--evidence",
            "protocol:correlation_id=someone-elses-thread",
            "--json",
        ],
    );
    assert_eq!(poisoned["ok"], true, "{poisoned}");
    let artifact = poisoned["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();
    let artifact_thread = poisoned["data"]["say"]["fact"]["thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(artifact_thread, "someone-elses-thread");

    let handoff = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            artifact,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_eq!(handoff["ok"], true, "{handoff}");
    let fact = &handoff["data"]["say"]["fact"];
    assert_eq!(
        fact["thread_id"], artifact_thread,
        "handoff must join the thread of the fact it cites"
    );
    let evidence = fact["evidence"].as_array().unwrap();
    assert!(
        evidence.contains(&Value::String(format!(
            "protocol:correlation_id={artifact_thread}"
        ))),
        "{evidence:?}"
    );
    assert!(
        !evidence
            .iter()
            .any(|entry| entry.as_str() == Some("protocol:correlation_id=someone-elses-thread")),
        "planted correlation must not survive into the handoff binding: {evidence:?}"
    );

    // The planted value is also not accepted when named explicitly:
    // `--thread-id` is still checked against the correlation Rally derived
    // from the artifact.
    let explicit = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            artifact,
            "--thread-id",
            "someone-elses-thread",
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_refusal(&explicit, "handoff_correlation_mismatch");
}

#[test]
fn third_party_resolution_is_exact_and_rejects_zero_multi_and_stale_sessions() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let zero = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--target-policy",
            "third-party",
            "--target",
            "missing",
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_refusal(&zero, "handoff_target_zero_match");

    let first = room.adopt("review-one", "reviewer:shared", "pane-one");
    let _second = room.adopt("review-two", "reviewer:shared", "pane-two");
    let multi = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--target-policy",
            "third-party",
            "--target",
            "reviewer:shared",
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_refusal(&multi, "handoff_target_multi_match");

    let exact = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--target-policy",
            "third-party",
            "--target",
            &first,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_eq!(exact["ok"], true, "{exact}");
    assert_eq!(exact["data"]["say"]["fact"]["target"], "reviewer:shared");
    let routed_handoff = exact["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let ack = room.json_managed(
        &first,
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:shared",
            "--ref",
            routed_handoff,
            "--handoff-state",
            "acked",
            "--subject",
            "ACK",
            "--json",
        ],
    );
    assert_eq!(
        ack["ok"], true,
        "selected managed receiver must reply: {ack}"
    );

    room.set_tmux_live(false);
    let stale = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--target-policy",
            "third-party",
            "--target",
            &first,
            "--subject",
            "stale review",
            "--json",
        ],
    );
    assert_refusal(&stale, "handoff_target_zero_match");
}

#[test]
fn selected_third_party_managed_receiver_can_ack_and_resolve_direct_and_daemon() {
    for routed in [false, true] {
        let room = Room::new();
        let _daemon = routed.then(|| Daemon::start(&room));
        let (artifact, _) = room.artifact();
        let receiver = room.adopt("managed-reviewer", "reviewer:managed", "pane-one");
        let request = room.json(
            "session-requester",
            &[
                "say",
                "handoff",
                "--tool",
                "requester:r",
                "--ref",
                &artifact,
                "--target-policy",
                "third-party",
                "--target",
                &receiver,
                "--subject",
                "review",
                "--json",
            ],
        );
        assert_eq!(request["ok"], true, "routed={routed}: {request}");
        let handoff = request["data"]["say"]["fact"]["event_id"].as_str().unwrap();
        let ack = room.json_managed(
            &receiver,
            &[
                "say",
                "handoff",
                "--tool",
                "reviewer:managed",
                "--ref",
                handoff,
                "--handoff-state",
                "acked",
                "--subject",
                "ACK",
                "--json",
            ],
        );
        assert_eq!(ack["ok"], true, "routed={routed}: {ack}");
        let resolved = room.json_managed(
            &receiver,
            &[
                "say",
                "resolve",
                "--tool",
                "reviewer:managed",
                "--ref",
                handoff,
                "--subject",
                "review complete",
                "--json",
            ],
        );
        assert_eq!(resolved["ok"], true, "routed={routed}: {resolved}");
    }
}

#[test]
fn default_ref_author_rejects_a_stale_managed_author() {
    let room = Room::new();
    let managed = room.adopt("managed-author", "author:managed", "pane-one");
    let artifact = room.json_managed(
        &managed,
        &[
            "say",
            "artifact",
            "--tool",
            "author:managed",
            "--subject",
            "build",
            "--json",
        ],
    );
    assert_eq!(artifact["ok"], true, "{artifact}");
    let artifact_id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();
    room.set_tmux_live(false);
    let rejected = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            artifact_id,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_refusal(&rejected, "handoff_target_stale_or_unknown");
}

#[test]
fn retry_key_requires_full_semantics_and_protocol_namespace_is_reserved() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let first = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--summary",
            "first",
            "--idempotency-key",
            "operation-1",
            "--json",
        ],
    );
    assert_eq!(first["ok"], true, "{first}");
    let conflict = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--summary",
            "changed",
            "--idempotency-key",
            "operation-1",
            "--json",
        ],
    );
    assert_refusal(&conflict, "handoff_idempotency_conflict");
    let changed_correlation = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--summary",
            "first",
            "--thread-id",
            "forged-correlation",
            "--idempotency-key",
            "operation-1",
            "--json",
        ],
    );
    assert_refusal(&changed_correlation, "handoff_correlation_mismatch");

    let spoof = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "spoof",
            "--evidence",
            "protocol:to_session_id=forged",
            "--json",
        ],
    );
    assert_refusal(&spoof, "handoff_protocol_evidence_reserved");

    let unreferenced_spoof = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--target",
            "author:a",
            "--subject",
            "spoof without ref",
            "--evidence",
            "protocol:to_session_id=forged",
            "--json",
        ],
    );
    assert_refusal(&unreferenced_spoof, "handoff_protocol_evidence_reserved");
}

#[test]
fn distinct_receiver_states_do_not_collapse_and_delivery_is_not_ack() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let initial = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--json",
        ],
    );
    let handoff = initial["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let before_reply = room.json("session-reviewer", &["room", "--json"]);
    assert_eq!(
        before_reply["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "durable delivery alone must remain an open handoff, not an ACK"
    );

    let ack = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--handoff-state",
            "acked",
            "--subject",
            "ACK",
            "--json",
        ],
    );
    let acceptance = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--handoff-state",
            "accepted",
            "--subject",
            "ACCEPT",
            "--json",
        ],
    );
    let rework = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            handoff,
            "--handoff-state",
            "rejected",
            "--subject",
            "REWORK",
            "--evidence",
            "missing test receipt",
            "--json",
        ],
    );
    assert_eq!(ack["ok"], true, "{ack}");
    assert_eq!(acceptance["ok"], true, "{acceptance}");
    assert_eq!(rework["ok"], true, "{rework}");
    assert_ne!(
        ack["data"]["say"]["fact"]["event_id"],
        acceptance["data"]["say"]["fact"]["event_id"]
    );
    assert_ne!(
        acceptance["data"]["say"]["fact"]["event_id"],
        rework["data"]["say"]["fact"]["event_id"]
    );
    assert!(
        ack["data"]["say"]["fact"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "protocol:event_kind=handoff.acked")
    );
    assert!(
        acceptance["data"]["say"]["fact"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "protocol:event_kind=handoff.accepted")
    );
    assert!(
        rework["data"]["say"]["fact"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "protocol:event_kind=handoff.rejected")
    );
    let after_states = room.json("session-author", &["room", "--json"]);
    assert_eq!(
        after_states["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "ACK/accept/rework proof facts must not project as new requests"
    );
}

#[test]
fn exact_binding_round_trips_in_direct_and_daemon_modes() {
    for routed in [false, true] {
        let room = Room::new();
        let _daemon = routed.then(|| Daemon::start(&room));
        let (artifact, author_session) = room.artifact();
        let value = room.json(
            "session-reviewer",
            &[
                "say",
                "handoff",
                "--tool",
                "reviewer:r",
                "--ref",
                &artifact,
                "--subject",
                "review",
                "--json",
            ],
        );
        assert_eq!(value["ok"], true, "routed={routed}: {value}");
        let fact = &value["data"]["say"]["fact"];
        assert_eq!(fact["target"], "author:a");
        assert!(fact["evidence"].as_array().unwrap().iter().any(|entry| {
            entry == &Value::String(format!("protocol:to_session_id={author_session}"))
        }));
        let located = room.json(
            "session-reviewer",
            &["locate", fact["event_id"].as_str().unwrap(), "--json"],
        );
        assert_eq!(located["ok"], true, "routed={routed}: {located}");
    }
}

#[test]
fn only_bound_session_can_resolve_strict_handoff_in_direct_and_daemon_modes() {
    for routed in [false, true] {
        let room = Room::new();
        let _daemon = routed.then(|| Daemon::start(&room));
        let (artifact, _) = room.artifact();
        let initial = room.json(
            "session-reviewer",
            &[
                "say",
                "handoff",
                "--tool",
                "reviewer:r",
                "--ref",
                &artifact,
                "--subject",
                "review",
                "--json",
            ],
        );
        let handoff = initial["data"]["say"]["fact"]["event_id"].as_str().unwrap();
        let next = room.json("session-author", &["next", "--tool", "author:a", "--json"]);
        assert!(next.to_string().contains("rally say resolve"), "{next}");

        let sibling = room.json(
            "session-author-sibling",
            &[
                "say",
                "resolve",
                "--tool",
                "author:a",
                "--ref",
                handoff,
                "--subject",
                "forged completion",
                "--json",
            ],
        );
        assert_eq!(sibling["ok"], false, "routed={routed}: {sibling}");
        assert!(
            sibling.to_string().contains("cannot resolve"),
            "routed={routed}: {sibling}"
        );

        for kind in ["receipt", "artifact"] {
            let sibling_closer = room.json(
                "session-author-sibling",
                &[
                    "say",
                    kind,
                    "--tool",
                    "author:a",
                    "--ref",
                    handoff,
                    "--subject",
                    "sibling evidence",
                    "--json",
                ],
            );
            assert_eq!(
                sibling_closer["ok"], true,
                "routed={routed}: {sibling_closer}"
            );
            let snapshot = room.json("session-author", &["room", "--json"]);
            assert!(
                snapshot["data"]["room"]["open_handoffs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|fact| fact["event_id"] == handoff),
                "{kind} from a same-tool sibling must not close the strict handoff: {snapshot}"
            );
        }

        let bound = room.json(
            "session-author",
            &[
                "say",
                "resolve",
                "--tool",
                "author:a",
                "--ref",
                handoff,
                "--subject",
                "responded to handoff",
                "--json",
            ],
        );
        assert_eq!(bound["ok"], true, "routed={routed}: {bound}");
    }
}

#[test]
fn actionable_room_hides_closed_inventory_but_history_remains_queryable() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let resolved = room.json(
        "session-author",
        &[
            "say",
            "resolve",
            "--tool",
            "author:a",
            "--ref",
            &artifact,
            "--subject",
            "done",
            "--json",
        ],
    );
    assert_eq!(resolved["ok"], true, "{resolved}");
    let raw = room.json("session-author", &["room", "--json"]);
    let actionable = room.json("session-author", &["room", "--actionable", "--json"]);
    assert!(
        raw["data"]["room"]["recent_artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| fact["event_id"] == artifact)
    );
    assert!(
        !actionable["data"]["room"]["recent_artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fact| fact["event_id"] == artifact)
    );
    let history = room.json("session-author", &["locate", &artifact, "--json"]);
    assert_eq!(history["ok"], true, "{history}");
}

#[test]
fn documented_receiver_ack_executes_against_bound_handoff() {
    let room = Room::new();
    let (artifact, _) = room.artifact();
    let sent = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &artifact,
            "--subject",
            "review",
            "--json",
        ],
    );
    assert_eq!(sent["ok"], true, "{sent}");
    let handoff = sent["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    // Execute the actual documentation, so a stale example cannot pass merely
    // because a separately maintained test command is correct.
    let doc = include_str!("../../../skills/agent-rally-point/references/handoffs.md");
    let receiving = doc.split("## Receiving a Handoff").nth(1).unwrap();
    let example = receiving
        .split("```bash\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let expanded = example
        .replace("\\\n", " ")
        .replace("$TOOL", "author:a")
        .replace("<their-event-id>", handoff)
        .replace("<their-tool>", "reviewer:r")
        .replace("<lane>", "contract");
    let argv = shlex::split(&expanded).unwrap();
    assert_eq!(argv[0], "rally");
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let ack = room.json("session-author", &args);
    assert_eq!(ack["ok"], true, "{ack}");
    assert!(
        ack["data"]["say"]["fact"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "protocol:event_kind=handoff.acked")
    );
    let inbox = room.json("session-author", &["inbox", "--tool", "author:a", "--json"]);
    assert_eq!(
        inbox["data"]["inbox"]["count"], 0,
        "typed ACK must clear receipt obligation: {inbox}"
    );
    let open = room.json("session-author", &["room", "--json"]);
    assert!(
        open["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == handoff),
        "ACK must not complete task"
    );
    let before_seq = open["data"]["room"]["max_seq"].clone();
    let repeated = room.json(
        "session-author",
        &[
            "inject",
            "author:a",
            "--tool",
            "author:a",
            "--handoff",
            handoff,
            "--timeout-seconds",
            "1",
            "--json",
        ],
    );
    assert_eq!(repeated["data"]["inject"]["mode"], "already-received");
    assert_eq!(repeated["data"]["inject"]["verified_received"], true);
    assert_eq!(repeated["data"]["inject"]["ack"]["resolved"], false);
    assert!(repeated["data"]["inject"]["directive_seq"].is_null());
    assert_eq!(
        room.json("session-author", &["room", "--json"])["data"]["room"]["max_seq"],
        before_seq
    );
    let retry = room.json("session-author", &args);
    assert_eq!(
        ack["data"]["say"]["fact"]["event_id"],
        retry["data"]["say"]["fact"]["event_id"]
    );
    let withdrawn = room.json(
        "session-author",
        &[
            "retract",
            ack["data"]["say"]["fact"]["event_id"].as_str().unwrap(),
            "--tool",
            "author:a",
            "--reason",
            "ACK withdrawn",
            "--json",
        ],
    );
    assert_eq!(withdrawn["ok"], true, "{withdrawn}");
    assert_eq!(
        room.json("session-author", &["inbox", "--tool", "author:a", "--json"])["data"]["inbox"]["count"],
        1
    );
    let retry = room.json(
        "session-author",
        &[
            "inject",
            "author:a",
            "--tool",
            "author:a",
            "--handoff",
            handoff,
            "--timeout-seconds",
            "1",
            "--json",
        ],
    );
    assert_ne!(retry["data"]["inject"]["mode"], "already-received");
    assert_eq!(
        retry["data"]["inject"]["verified_received"], false,
        "withdrawn ACK must not satisfy reuse or polling: {retry}"
    );
    let withdrawn = room.json(
        "session-author",
        &[
            "retract",
            handoff,
            "--tool",
            "author:a",
            "--reason",
            "request withdrawn",
            "--json",
        ],
    );
    assert_eq!(withdrawn["ok"], true, "{withdrawn}");
    let retry = room.json(
        "session-author",
        &[
            "inject",
            "author:a",
            "--tool",
            "author:a",
            "--handoff",
            handoff,
            "--timeout-seconds",
            "1",
            "--json",
        ],
    );
    assert_ne!(
        retry["data"]["inject"]["verified_received"], true,
        "withdrawal is not completion: {retry}"
    );
}

#[test]
fn ordinary_parent_lease_can_receive_referenced_handoff_without_managed_runner() {
    let room = Room::new();
    let who = room.json_parent("gui-author", &["whoami", "--tool", "cursor:a", "--json"]);
    assert_eq!(
        who["data"]["whoami"]["session_identity"]["session_id"],
        "sess:parent:gui-author#live"
    );
    let artifact = room.json_parent(
        "gui-author",
        &[
            "say",
            "artifact",
            "--tool",
            "cursor:a",
            "--subject",
            "ready",
            "--json",
        ],
    );
    assert_eq!(artifact["ok"], true, "{artifact}");
    assert_eq!(
        artifact["data"]["say"]["fact"]["from_session_id"],
        who["data"]["whoami"]["session_identity"]["session_id"]
    );
    let id = artifact["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap();
    let handoff = room.json_parent(
        "gui-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "antigravity:r",
            "--ref",
            id,
            "--subject",
            "reviewed",
            "--json",
        ],
    );
    assert_eq!(handoff["ok"], true, "{handoff}");
    let delivery = room.json_parent(
        "gui-reviewer",
        &[
            "inject",
            "cursor:a",
            "--tool",
            "antigravity:r",
            "--intent",
            "inform",
            "--text",
            "read your handoff",
            "--json",
        ],
    );
    assert_eq!(
        delivery["data"]["inject"]["commands"],
        serde_json::json!([])
    );
    assert_eq!(delivery["data"]["inject"]["delivery_path"], "ledger_only");
    assert_eq!(delivery["data"]["inject"]["delivered"], false);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"].as_str().unwrap();
    let ack = room.json_parent(
        "gui-author",
        &[
            "say",
            "handoff",
            "--tool",
            "cursor:a",
            "--ref",
            handoff_id,
            "--handoff-state",
            "acked",
            "--subject",
            "received",
            "--json",
        ],
    );
    assert_eq!(ack["ok"], true, "{ack}");
    assert_eq!(
        ack["data"]["say"]["fact"]["from_session_id"],
        "sess:parent:gui-author#live"
    );
    let sessions = room.json_parent("gui-author", &["sessions", "--json"]);
    assert_eq!(sessions["ok"], true, "{sessions}");
}

#[test]
fn fresh_parent_lifecycle_exports_clear_an_inherited_managed_marker() {
    let room = Room::new();
    let opened = room.json_managed(
        "old-runner",
        &[
            "session",
            "ensure",
            "--tool",
            "cursor:new",
            "--session-id",
            "new-parent",
            "--resource",
            "task:parent-test",
            "--json",
        ],
    );
    assert_eq!(opened["ok"], true, "{opened}");
    let environment = opened["data"]["session"]["environment"]
        .as_object()
        .unwrap();
    assert_eq!(environment["RALLY_MANAGED_SESSION_MODE"], "");
    let run = |args: &[&str]| {
        let out = rally_command()
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .envs(
                environment
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str().unwrap())),
            )
            .args(args)
            .output()
            .unwrap();
        Room::parse(args, out)
    };
    let who = run(&["whoami", "--tool", "cursor:new", "--json"]);
    assert_eq!(
        who["data"]["whoami"]["session_identity"]["session_id"],
        "sess:parent:new-parent#live"
    );
    let closed = run(&["session", "close", "--tool", "cursor:new", "--json"]);
    assert_eq!(closed["ok"], true, "{closed}");
    assert_eq!(
        closed["data"]["session"]["session_id"],
        "sess:parent:new-parent#live"
    );
    assert_eq!(
        closed["data"]["session"]["released_claim_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn status_reply_binds_an_unbound_legacy_directive() {
    let room = Room::new();
    // A legacy directive: a handoff with no `--ref`, so it carries no exact
    // `protocol:to_session_id` binding.
    let sent = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--target",
            "reviewer:r",
            "--subject",
            "please review",
            "--json",
        ],
    );
    assert_eq!(sent["ok"], true, "{sent}");
    let directive = sent["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    // A different tool still cannot answer a directive aimed at reviewer:r.
    let wrong = room.run(
        "session-other",
        &[
            "say",
            "handoff",
            "--tool",
            "other:o",
            "--ref",
            &directive,
            "--handoff-state",
            "acked",
            "--subject",
            "ack",
            "--json",
        ],
    );
    assert!(!wrong.status.success());
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("handoff_reply_author_mismatch"),
        "{}",
        String::from_utf8_lossy(&wrong.stderr)
    );

    // Regression: the named receiver's status reply used to be refused with
    // handoff_reply_unbound_legacy. It now binds the directive it answers.
    let ack = room.json(
        "session-reviewer",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &directive,
            "--handoff-state",
            "acked",
            "--subject",
            "ack",
            "--json",
        ],
    );
    assert_eq!(ack["ok"], true, "{ack}");
    let evidence = ack["data"]["say"]["fact"]["evidence"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|v| v == "protocol:event_kind=handoff.acked")
    );
    // Once bound, another session of the same tool cannot take it over.
    let hijack = room.run(
        "session-reviewer-2",
        &[
            "say",
            "handoff",
            "--tool",
            "reviewer:r",
            "--ref",
            &directive,
            "--handoff-state",
            "accepted",
            "--subject",
            "mine",
            "--json",
        ],
    );
    assert!(!hijack.status.success());
    assert!(
        String::from_utf8_lossy(&hijack.stderr).contains("handoff_reply_author_mismatch"),
        "{}",
        String::from_utf8_lossy(&hijack.stderr)
    );
}

/// AGEN-HANDOFF (2026-11-08): a sender that wanted to cancel its own handoff
/// tried `say resolve`, `--handoff-state withdrawn` and a self-ack, and each
/// refusal named only who may NOT act. `rally retract` was the right move and
/// no refusal mentioned it. Every refusal a sender hits now names it, the
/// sender still cannot resolve or answer its own request, and the retract
/// the hint names succeeds.
#[test]
fn sender_cancel_refusals_point_to_retract_and_retract_succeeds() {
    let room = Room::new();
    let sent = room.json(
        "session-author",
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--target",
            "reviewer:r",
            "--subject",
            "please review",
            "--json",
        ],
    );
    assert_eq!(sent["ok"], true, "{sent}");
    let directive = sent["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    let hint = format!("rally retract {directive} --tool author:a");

    let attempts: [&[&str]; 3] = [
        &[
            "say",
            "resolve",
            "--tool",
            "author:a",
            "--ref",
            &directive,
            "--subject",
            "cancel",
            "--json",
        ],
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            &directive,
            "--handoff-state",
            "withdrawn",
            "--subject",
            "cancel",
            "--json",
        ],
        &[
            "say",
            "handoff",
            "--tool",
            "author:a",
            "--ref",
            &directive,
            "--handoff-state",
            "acked",
            "--subject",
            "cancel",
            "--json",
        ],
    ];
    for args in attempts {
        let out = room.run("session-author", args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.status.success(), "{args:?} must stay refused: {text}");
        assert!(
            text.contains(&hint),
            "{args:?} refusal must name the retract command: {text}"
        );
    }

    // A third party is refused without being told to retract someone else's ask.
    let other = room.run(
        "session-other",
        &[
            "say",
            "resolve",
            "--tool",
            "other:o",
            "--ref",
            &directive,
            "--subject",
            "x",
            "--json",
        ],
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&other.stdout),
        String::from_utf8_lossy(&other.stderr)
    );
    assert!(!other.status.success(), "{text}");
    assert!(
        !text.contains("rally retract"),
        "only the sender is told to retract: {text}"
    );

    let withdrawn = room.json(
        "session-author",
        &[
            "retract",
            &directive,
            "--tool",
            "author:a",
            "--reason",
            "no longer needed",
            "--json",
        ],
    );
    assert_eq!(withdrawn["ok"], true, "{withdrawn}");
}
