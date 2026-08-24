// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("TERM_SESSION_ID", session)
            .env("PATH", format!("{}:{inherited}", self.bin.display()))
            .args(args)
            .output()
            .unwrap()
    }

    fn run_managed(&self, session: &str, args: &[&str]) -> Output {
        let inherited = std::env::var("PATH").unwrap_or_default();
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_SESSION_ID", session)
            .env("PATH", format!("{}:{inherited}", self.bin.display()))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, session: &str, args: &[&str]) -> Value {
        let out = self.run(session, args);
        let bytes = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(bytes)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(bytes)))
    }

    fn json_managed(&self, session: &str, args: &[&str]) -> Value {
        let out = self.run_managed(session, args);
        let bytes = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(bytes)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(bytes)))
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
        let child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .env("PATH", format!("{}:{inherited}", room.bin.display()))
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start rally daemon");
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if room
                .run("daemon-probe", &["daemon", "status", "--json"])
                .status
                .success()
            {
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
    assert_eq!(wrong["ok"], false);
    assert!(
        wrong["error"]
            .as_str()
            .unwrap()
            .contains("handoff_target_mismatch")
    );
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
    assert_eq!(spoof["ok"], false);
    assert!(
        spoof["error"]
            .as_str()
            .unwrap()
            .contains("handoff_reply_author_mismatch")
    );

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
    assert_eq!(implicit["ok"], false);
    assert!(
        implicit["error"]
            .as_str()
            .unwrap()
            .contains("handoff_reply_state_required")
    );

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
    assert_eq!(bypass["ok"], false);
    assert!(
        bypass["error"]
            .as_str()
            .unwrap()
            .contains("handoff_third_party_reply_forbidden")
    );

    let snapshot = room.json("session-author", &["room", "--json"]);
    let reply_count = snapshot["data"]["room"]["open_handoffs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|fact| fact["ref"] == handoff)
        .count();
    assert_eq!(reply_count, 0, "rejections must append no reply fact");
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
    assert_eq!(zero["ok"], false);
    assert!(
        zero["error"]
            .as_str()
            .unwrap()
            .contains("handoff_target_zero_match")
    );

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
    assert_eq!(multi["ok"], false);
    assert!(
        multi["error"]
            .as_str()
            .unwrap()
            .contains("handoff_target_multi_match")
    );

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
    assert_eq!(stale["ok"], false);
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("handoff_target_zero_match")
    );
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
    assert_eq!(rejected["ok"], false);
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("handoff_target_stale_or_unknown")
    );
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
    assert_eq!(conflict["ok"], false);
    assert!(
        conflict["error"]
            .as_str()
            .unwrap()
            .contains("handoff_idempotency_conflict")
    );
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
    assert_eq!(changed_correlation["ok"], false);
    assert!(
        changed_correlation["error"]
            .as_str()
            .unwrap()
            .contains("handoff_correlation_mismatch")
    );

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
    assert_eq!(spoof["ok"], false);
    assert!(
        spoof["error"]
            .as_str()
            .unwrap()
            .contains("handoff_protocol_evidence_reserved")
    );

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
    assert_eq!(unreferenced_spoof["ok"], false);
    assert!(
        unreferenced_spoof["error"]
            .as_str()
            .unwrap()
            .contains("handoff_protocol_evidence_reserved")
    );
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
