// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! A targeted `say handoff` to an identity with NO Rally managed session, but
//! bound to a live ptyd pane (Easy Terminal), must wake that pane through the
//! pane's inbox (`inbox.enqueue`) instead of recording only. ptyd types the
//! queued line when the pane is idle, which is the case a record-only handoff
//! never reaches: an idle pane has no next turn for its prompt hook.
//!
//! Queued is not delivered: the envelope reports `queued_pane_inbox` with the
//! pane message id, and the room gets a wake fact whose delivery reason is
//! `queued_awaiting_receipt`. The handoff stays open until the receiver acks.
//!
//! A test-double daemon answers `pane.list` and `inbox.enqueue` on a socket
//! passed through `PTYD_SOCKET_PATH`; `HOME` is sandboxed so the real Easy
//! Terminal socket is never a candidate.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use support::channel_sandbox::ChannelSandbox;
use support::rally_cmd::rally_command;

const SENDER: &str = "sender:01";

type RequestLog = Arc<Mutex<Vec<Value>>>;

#[derive(Clone, Copy, PartialEq)]
enum EnqueueReply {
    Queued,
    Refused,
}

fn short_socket(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "hipw-{tag}-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Serve `pane.list` (one live pane bound to `bound_identity`) and
/// `inbox.enqueue`, logging every request.
fn spawn_daemon(socket: &Path, bound_identity: &str, reply: EnqueueReply) -> RequestLog {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).expect("bind double daemon");
    let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let log_thread = Arc::clone(&log);
    let identity = bound_identity.to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            log_thread.lock().unwrap().push(req.clone());
            let id = req["id"].clone();
            let body = match req["method"].as_str() {
                Some("pane.list") => json!({"id": id, "result": {"type": "pane_list", "panes": [
                    {"pane_id": "pane-7", "liveness": "live", "rally_identity": identity}
                ]}}),
                Some("inbox.enqueue") if reply == EnqueueReply::Queued => json!({"id": id,
                    "result": {"id": "msg-42", "state": "queued", "address": "et:pane-7",
                        "duplicate": false, "truncated": false, "route": "inbox",
                        "hook_capable": true, "delivery": "hook_when_busy_or_typed_when_idle"}}),
                Some("inbox.enqueue") => json!({"id": id, "error": {
                    "code": "capability_denied", "message": "not a pane process"}}),
                _ => json!({"id": id, "error": {"code": "unknown_method", "message": "no"}}),
            };
            let mut stream = stream;
            let _ = writeln!(stream, "{body}");
        }
    });
    log
}

fn rally(sandbox: &ChannelSandbox, socket: &Path, args: &[&str]) -> Value {
    let output = rally_command()
        .args(args)
        .current_dir(sandbox.cwd())
        .env("HOME", sandbox.home())
        .env_remove("PWD")
        .env_remove("RALLY_ET_ROUTER_HEALTH")
        .env_remove("RALLY_PTYD_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("PTYD_SOCKET_PATH", socket)
        .output()
        .expect("spawn rally");
    assert!(
        output.status.success(),
        "rally {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("rally JSON")
}

fn handoff(sandbox: &ChannelSandbox, socket: &Path, target: &str) -> Value {
    rally(
        sandbox,
        socket,
        &[
            "say",
            "handoff",
            "--json",
            "--tool",
            SENDER,
            "--target",
            target,
            "--subject",
            "review the staged diff",
        ],
    )
}

fn enqueues(log: &RequestLog) -> Vec<Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r["method"] == "inbox.enqueue")
        .cloned()
        .collect()
}

#[test]
fn handoff_to_unmanaged_bound_pane_queues_a_pane_inbox_wake() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("ok");
    let target = "easy-terminal:idle-a";
    let log = spawn_daemon(&socket, target, EnqueueReply::Queued);

    let said = handoff(&sandbox, &socket, target);
    let delivery = &said["data"]["delivery"];
    let handoff_id = said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff id")
        .to_string();

    assert_eq!(delivery["status"], "queued_pane_inbox", "{said}");
    assert_eq!(delivery["pane_message_id"], "msg-42", "{said}");
    assert!(
        delivery["detail"]
            .as_str()
            .is_some_and(|d| d.contains("not yet acknowledged")),
        "queued must not read as delivered: {said}"
    );

    let sent = enqueues(&log);
    assert_eq!(sent.len(), 1, "exactly one enqueue: {sent:?}");
    let params = &sent[0]["params"];
    assert_eq!(params["to"], target);
    assert_eq!(params["ref"], handoff_id.as_str());
    assert_eq!(params["client_id"], handoff_id.as_str());
    let text = params["text"].as_str().unwrap_or_default();
    assert!(text.contains(&handoff_id), "text names the handoff: {text}");
    assert!(
        !text.contains("review the staged diff"),
        "sender-authored subject must not be typed into the pane: {text}"
    );

    // The wake is recorded durably, as queued, against the handoff.
    let wake_id = delivery["wake_event_id"].as_str().expect("wake id");
    let recent = rally(
        &sandbox,
        &socket,
        &["recent", "--json", "--all", "--limit", "50"],
    );
    let wake = recent
        .pointer("/data/recent/rows")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .map(|row| &row["fact"])
                .find(|f| f["event_id"] == wake_id)
                .cloned()
        });
    let wake = wake.unwrap_or_else(|| panic!("wake fact {wake_id} missing: {recent}"));
    {
        assert_eq!(wake["ref"], handoff_id.as_str(), "{wake}");
        assert_eq!(wake["status"], "pending", "{wake}");
        let evidence = wake["evidence"].to_string();
        assert!(
            evidence.contains("delivery_reason:queued_awaiting_receipt"),
            "{wake}"
        );
        assert!(evidence.contains("pane_inbox_message:msg-42"), "{wake}");
    }

    // Still an open obligation: queued is not an ack.
    let room = rally(&sandbox, &socket, &["room", "--json"]);
    let open = room["data"]["room"]["open_handoffs"].to_string();
    assert!(open.contains(&handoff_id), "handoff must stay open: {room}");
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn refused_pane_enqueue_falls_back_to_record_only_with_reason() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("no");
    let target = "easy-terminal:idle-b";
    let log = spawn_daemon(&socket, target, EnqueueReply::Refused);

    let said = handoff(&sandbox, &socket, target);
    let delivery = &said["data"]["delivery"];
    assert_eq!(delivery["status"], "record_only", "{said}");
    assert!(
        delivery["detail"]
            .as_str()
            .is_some_and(|d| d.contains("wake failed") && d.contains("capability_denied")),
        "{said}"
    );
    assert!(delivery.get("pane_message_id").is_none(), "{said}");
    assert_eq!(enqueues(&log).len(), 1);
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn unbound_target_records_only_and_never_enqueues() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("unb");
    let log = spawn_daemon(&socket, "easy-terminal:someone-else", EnqueueReply::Queued);

    let said = handoff(&sandbox, &socket, "easy-terminal:idle-c");
    let delivery = &said["data"]["delivery"];
    assert_eq!(delivery["status"], "record_only", "{said}");
    assert!(
        delivery["detail"]
            .as_str()
            .is_some_and(|d| d.contains("no live ptyd pane is bound")),
        "{said}"
    );
    assert!(enqueues(&log).is_empty(), "no pane may be typed into");
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn deliver_record_never_touches_the_pane_inbox() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("rec");
    let target = "easy-terminal:idle-d";
    let log = spawn_daemon(&socket, target, EnqueueReply::Queued);

    let said = rally(
        &sandbox,
        &socket,
        &[
            "say",
            "handoff",
            "--json",
            "--tool",
            SENDER,
            "--target",
            target,
            "--subject",
            "x",
            "--deliver",
            "record",
        ],
    );
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(enqueues(&log).is_empty());
    let _ = std::fs::remove_file(&socket);
}
