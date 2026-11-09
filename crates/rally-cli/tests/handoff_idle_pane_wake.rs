// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! A targeted `say handoff` to an identity with NO Rally managed session, but
//! bound to a live ptyd pane (Easy Terminal), wakes that pane through the
//! pane's inbox (`inbox.enqueue`) instead of recording only. ptyd types the
//! queued line when the pane is idle, which is the case a record-only handoff
//! never reaches: an idle pane has no next turn for its prompt hook.
//!
//! Gates, each covered here: the sender must be allowed to direct the target
//! (lead, target consent, or self); exactly one live pane may be bound (a
//! pinned socket wins); the pane's working directory must belong to the room's
//! repository; ET's router owning the identity suppresses the wake; a pending
//! earlier wake coalesces later ones; a refused, malformed or timed-out
//! enqueue degrades to record-only. Queued is never reported as delivered.
//!
//! Test-double daemons answer `pane.list` and `inbox.enqueue` on short Unix
//! sockets; `HOME` is sandboxed so the real Easy Terminal socket is never a
//! candidate.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::channel_sandbox::ChannelSandbox;
use support::rally_cmd::rally_command;

const SENDER: &str = "sender:01";

type RequestLog = Arc<Mutex<Vec<Value>>>;

#[derive(Clone, Copy, PartialEq)]
enum EnqueueReply {
    Queued,
    Refused,
    Malformed,
    /// Reply after the CLI's 3 s daemon timeout.
    Hang,
}

#[derive(Clone)]
struct Pane {
    id: &'static str,
    identity: String,
    cwd: Option<PathBuf>,
}

fn unique(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{tag}{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn short_socket(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hipw-{}.sock", unique(tag)))
}

fn spawn_daemon(socket: &Path, panes: Vec<Pane>, reply: EnqueueReply) -> RequestLog {
    spawn_pane_server(socket, panes, reply, None)
}

fn spawn_pane_server(
    socket: &Path,
    panes: Vec<Pane>,
    reply: EnqueueReply,
    mut rendezvous: Option<Arc<Barrier>>,
) -> RequestLog {
    let _ = std::fs::remove_file(socket);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let listener = UnixListener::bind(socket).expect("bind double daemon");
    let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let log_thread = Arc::clone(&log);
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
                Some("pane.list") => {
                    if let Some(rendezvous) = rendezvous.take() {
                        rendezvous.wait();
                    }
                    let rows: Vec<Value> = panes
                        .iter()
                        .map(|p| {
                            let mut row = json!({"pane_id": p.id, "liveness": "live",
                                "rally_identity": p.identity});
                            if let Some(cwd) = &p.cwd {
                                row["foreground_cwd"] = json!(cwd.display().to_string());
                            }
                            row
                        })
                        .collect();
                    json!({"id": id, "result": {"type": "pane_list", "panes": rows}})
                }
                Some("inbox.enqueue") => match reply {
                    EnqueueReply::Queued => json!({"id": id, "result": {
                        "id": format!("msg-{}", req["params"]["client_id"].as_str().unwrap_or("")),
                        "state": "queued", "address": format!("pane:{}", panes[0].id),
                        "duplicate": false, "truncated": false, "route": "inbox",
                        "hook_capable": true, "delivery": "hook_when_busy_or_typed_when_idle"}}),
                    EnqueueReply::Refused => json!({"id": id, "error": {
                        "code": "capability_denied", "message": "not a pane process"}}),
                    EnqueueReply::Malformed => json!({"id": id, "result": {"state": "queued"}}),
                    EnqueueReply::Hang => {
                        thread::sleep(Duration::from_secs(5));
                        json!({"id": id, "result": {"id": "late", "state": "queued"}})
                    }
                },
                // Every queued message is still waiting in the pane.
                Some("inbox.show") => json!({"id": id, "result": {
                    "id": req["params"]["id"], "state": "queued"}}),
                _ => json!({"id": id, "error": {"code": "unknown_method", "message": "no"}}),
            };
            let mut stream = stream;
            let _ = writeln!(stream, "{body}");
        }
    });
    log
}

fn pane(sandbox: &ChannelSandbox, id: &'static str, identity: &str) -> Pane {
    Pane {
        id,
        identity: identity.to_string(),
        cwd: Some(sandbox.cwd().to_path_buf()),
    }
}

/// How the test runs rally relative to the doubles.
enum Sockets<'a> {
    /// `PTYD_SOCKET_PATH` pins this socket.
    Pinned(&'a Path),
    /// No pin; candidates come from `XDG_RUNTIME_DIR` and `HOME`.
    Unpinned { xdg: &'a Path, home: &'a Path },
}

fn base_command(sandbox: &ChannelSandbox, sockets: &Sockets) -> Command {
    let mut cmd = rally_command();
    cmd.current_dir(sandbox.cwd())
        .env_remove("PWD")
        .env_remove("RALLY_ET_ROUTER_HEALTH")
        .env_remove("RALLY_PTYD_SOCKET")
        .env_remove("PTYD_SOCKET_PATH")
        .env_remove("XDG_RUNTIME_DIR");
    match sockets {
        Sockets::Pinned(socket) => {
            cmd.env("HOME", sandbox.home())
                .env("PTYD_SOCKET_PATH", socket);
        }
        Sockets::Unpinned { xdg, home } => {
            cmd.env("HOME", home).env("XDG_RUNTIME_DIR", xdg);
        }
    }
    cmd
}

fn run(mut cmd: Command, args: &[&str]) -> Value {
    let output = cmd.args(args).output().expect("spawn rally");
    assert!(
        output.status.success(),
        "rally {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("rally JSON")
}

fn rally(sandbox: &ChannelSandbox, socket: &Path, args: &[&str]) -> Value {
    run(base_command(sandbox, &Sockets::Pinned(socket)), args)
}

fn handoff_args<'a>(tool: &'a str, target: &'a str, subject: &'a str) -> Vec<&'a str> {
    vec![
        "say",
        "handoff",
        "--json",
        "--tool",
        tool,
        "--target",
        target,
        "--subject",
        subject,
    ]
}

fn handoff(sandbox: &ChannelSandbox, socket: &Path, target: &str) -> Value {
    rally(
        sandbox,
        socket,
        &handoff_args(SENDER, target, "review the staged diff"),
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

fn detail(said: &Value) -> String {
    said["data"]["delivery"]["detail"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn handoff_id(said: &Value) -> String {
    said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff id")
        .to_string()
}

#[test]
fn handoff_to_unmanaged_bound_pane_queues_a_pane_inbox_wake() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("ok");
    let target = "easy-terminal:idle-a";
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-7", target)],
        EnqueueReply::Queued,
    );

    let said = handoff(&sandbox, &socket, target);
    let delivery = &said["data"]["delivery"];
    let handoff_id = handoff_id(&said);

    assert_eq!(delivery["status"], "queued_pane_inbox", "{said}");
    assert_eq!(
        delivery["pane_message_id"],
        format!("msg-{handoff_id}"),
        "{said}"
    );
    assert!(detail(&said).contains("not yet acknowledged"), "{said}");

    let sent = enqueues(&log);
    assert_eq!(sent.len(), 1, "exactly one enqueue: {sent:?}");
    let params = &sent[0]["params"];
    // Addressed to the resolved pane, not the identity (N2).
    assert_eq!(params["to"], "pane-7");
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
        })
        .unwrap_or_else(|| panic!("wake fact {wake_id} missing: {recent}"));
    assert_eq!(wake["ref"], handoff_id.as_str(), "{wake}");
    assert_eq!(wake["status"], "pending", "{wake}");
    let evidence = wake["evidence"].to_string();
    assert!(
        evidence.contains("delivery_reason:queued_awaiting_receipt"),
        "{wake}"
    );
    assert!(evidence.contains("transport:et_pane_inbox"), "{wake}");
    assert!(evidence.contains("authority_basis:lead_lease"), "{wake}");

    // Still an open obligation: queued is not an ack.
    let room = rally(&sandbox, &socket, &["room", "--json"]);
    let open = room["data"]["room"]["open_handoffs"].to_string();
    assert!(open.contains(&handoff_id), "handoff must stay open: {room}");
    let _ = std::fs::remove_file(&socket);
}

// S1: who may direct whom. A non-lead sender without the target's consent is
// refused exactly as `rally inject` would refuse it, before any socket write.
#[test]
fn non_lead_sender_without_consent_never_enqueues() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("auth");
    let target = "easy-terminal:idle-auth";
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-a", target)],
        EnqueueReply::Queued,
    );
    // lead:01 enters first and takes the lead seat.
    rally(&sandbox, &socket, &["enter", "--tool", "lead:01", "--json"]);
    let said = rally(
        &sandbox,
        &socket,
        &handoff_args("peer:02", target, "do the thing"),
    );
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(
        detail(&said).contains("pane wake refused"),
        "detail names the refusal: {said}"
    );
    assert!(enqueues(&log).is_empty(), "no pane may be typed into");

    // Consent: once the target opens a handoff to peer:02, peer:02 may wake it.
    rally(
        &sandbox,
        &socket,
        &[
            "say",
            "handoff",
            "--json",
            "--tool",
            target,
            "--target",
            "peer:02",
            "--subject",
            "send me the parser task",
            "--deliver",
            "record",
        ],
    );
    let consented = rally(
        &sandbox,
        &socket,
        &handoff_args("peer:02", target, "parser task"),
    );
    assert_eq!(
        consented["data"]["delivery"]["status"], "queued_pane_inbox",
        "{consented}"
    );
    assert_eq!(enqueues(&log).len(), 1);
    let _ = std::fs::remove_file(&socket);
}

// S3: a pending earlier wake absorbs later handoffs to the same target; once
// the earlier handoff is acknowledged, the next one wakes again.
#[test]
fn pending_pane_wake_coalesces_later_handoffs() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("coal");
    let target = "easy-terminal:idle-co";
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-c", target)],
        EnqueueReply::Queued,
    );
    let first = handoff(&sandbox, &socket, target);
    assert_eq!(first["data"]["delivery"]["status"], "queued_pane_inbox");
    let first_wake = first["data"]["delivery"]["wake_event_id"].clone();

    let second = handoff(&sandbox, &socket, target);
    assert_eq!(
        second["data"]["delivery"]["status"], "coalesced_pane_wake",
        "{second}"
    );
    assert_eq!(second["data"]["delivery"]["wake_event_id"], first_wake);
    assert!(detail(&second).contains("coalesced into"), "{second}");
    assert_eq!(enqueues(&log).len(), 1, "second handoff must not enqueue");

    // Both handoffs acknowledged: the pending wake closes with them.
    for said in [&first, &second] {
        let id = handoff_id(said);
        rally(
            &sandbox,
            &socket,
            &[
                "say",
                "receipt",
                "--json",
                "--tool",
                target,
                "--ref",
                &id,
                "--subject",
                "received",
            ],
        );
    }
    let third = handoff(&sandbox, &socket, target);
    assert_eq!(
        third["data"]["delivery"]["status"], "queued_pane_inbox",
        "{third}"
    );
    assert_eq!(enqueues(&log).len(), 2);
    let _ = std::fs::remove_file(&socket);
}

// N1: a lead's handoff is never coalesced under a non-lead's pending wake.
#[test]
fn lead_handoff_after_a_non_lead_wake_is_enqueued() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("lead");
    let target = "easy-terminal:idle-ld";
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-l", target)],
        EnqueueReply::Queued,
    );
    rally(&sandbox, &socket, &["enter", "--tool", "lead:01", "--json"]);
    // The target invites peer:02, so peer:02 may wake it (target consent).
    let mut invite = handoff_args(target, "peer:02", "send me work");
    invite.extend(["--deliver", "record"]);
    rally(&sandbox, &socket, &invite);
    let peer = rally(&sandbox, &socket, &handoff_args("peer:02", target, "a"));
    assert_eq!(
        peer["data"]["delivery"]["status"], "queued_pane_inbox",
        "{peer}"
    );

    let lead = rally(&sandbox, &socket, &handoff_args("lead:01", target, "b"));
    assert_eq!(
        lead["data"]["delivery"]["status"], "queued_pane_inbox",
        "a lead must not be coalesced under a peer's wake: {lead}"
    );
    assert_eq!(enqueues(&log).len(), 2);

    // A second peer handoff IS absorbed by the lead's queued wake.
    let again = rally(&sandbox, &socket, &handoff_args("peer:02", target, "c"));
    assert_eq!(
        again["data"]["delivery"]["status"], "coalesced_pane_wake",
        "{again}"
    );
    assert_eq!(enqueues(&log).len(), 2);
    let _ = std::fs::remove_file(&socket);
}

// S2: two panes bound to one identity on one daemon is ambiguous.
#[test]
fn two_panes_bound_to_one_identity_are_ambiguous() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("amb");
    let target = "easy-terminal:twin";
    let log = spawn_daemon(
        &socket,
        vec![
            pane(&sandbox, "pane-1", target),
            pane(&sandbox, "pane-2", target),
        ],
        EnqueueReply::Queued,
    );
    let said = handoff(&sandbox, &socket, target);
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(detail(&said).contains("ambiguous: 2"), "{said}");
    assert!(enqueues(&log).is_empty());
    let _ = std::fs::remove_file(&socket);
}

// X1: two sockets advertise the same identity. Unpinned → ambiguous, no
// enqueue anywhere. Pinned → only the pinned daemon is consulted and woken.
#[test]
fn two_sockets_for_one_identity_need_a_pin() {
    let sandbox = ChannelSandbox::spawn();
    let target = "easy-terminal:dual";
    let base = std::env::temp_dir().join(unique("hx"));
    let xdg = base.join("x");
    let home = base.join("h");
    let xdg_socket = xdg.join("ptyd.sock");
    let home_socket = home.join(".config/ptyd/ptyd.sock");
    let xdg_log = spawn_daemon(
        &xdg_socket,
        vec![pane(&sandbox, "pane-x", target)],
        EnqueueReply::Queued,
    );
    let home_log = spawn_daemon(
        &home_socket,
        vec![pane(&sandbox, "pane-h", target)],
        EnqueueReply::Queued,
    );

    let unpinned = run(
        base_command(
            &sandbox,
            &Sockets::Unpinned {
                xdg: &xdg,
                home: &home,
            },
        ),
        &handoff_args(SENDER, target, "x"),
    );
    assert_eq!(
        unpinned["data"]["delivery"]["status"], "record_only",
        "{unpinned}"
    );
    assert!(detail(&unpinned).contains("ambiguous: 2"), "{unpinned}");
    assert!(enqueues(&xdg_log).is_empty() && enqueues(&home_log).is_empty());

    let pinned = rally(&sandbox, &xdg_socket, &handoff_args(SENDER, target, "y"));
    assert_eq!(
        pinned["data"]["delivery"]["status"], "queued_pane_inbox",
        "{pinned}"
    );
    assert_eq!(enqueues(&xdg_log).len(), 1);
    assert!(
        enqueues(&home_log).is_empty(),
        "the unpinned daemon is never used"
    );
    let _ = std::fs::remove_dir_all(&base);
}

// S2: a pane working outside this room's repository is not woken, and neither
// is one that reports no working directory.
#[test]
fn pane_outside_the_room_repository_is_not_woken() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("repo");
    let foreign = std::env::temp_dir().join(unique("hipw-foreign"));
    std::fs::create_dir_all(&foreign).unwrap();
    let log = spawn_daemon(
        &socket,
        vec![
            Pane {
                id: "pane-f",
                identity: "easy-terminal:foreign".to_string(),
                cwd: Some(foreign.clone()),
            },
            Pane {
                id: "pane-n",
                identity: "easy-terminal:nocwd".to_string(),
                cwd: None,
            },
        ],
        EnqueueReply::Queued,
    );
    let said = handoff(&sandbox, &socket, "easy-terminal:foreign");
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(detail(&said).contains("outside this room"), "{said}");
    let said = handoff(&sandbox, &socket, "easy-terminal:nocwd");
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(detail(&said).contains("no working directory"), "{said}");
    assert!(enqueues(&log).is_empty());
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&foreign);
}

// A7: ET's router owning the identity suppresses the wake even with a bound
// pane: one writer per pane.
#[test]
fn router_owned_target_with_a_bound_pane_never_enqueues() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("rt");
    let target = "easy-terminal:routed";
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-r", target)],
        EnqueueReply::Queued,
    );
    let health = sandbox.root().join("health.json");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::fs::write(
        &health,
        serde_json::to_vec(&json!({"schema": "et.rally-router.health.v1",
            "pid": std::process::id(), "updated_ms": now_ms, "degraded": false,
            "routed_identities": [target]}))
        .unwrap(),
    )
    .unwrap();
    let mut cmd = base_command(&sandbox, &Sockets::Pinned(&socket));
    cmd.env("RALLY_ET_ROUTER_HEALTH", &health);
    let said = run(cmd, &handoff_args(SENDER, target, "x"));
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(detail(&said).contains("et-router"), "{said}");
    assert!(enqueues(&log).is_empty());
    let _ = std::fs::remove_file(&socket);
}

// A7: refused, malformed and timed-out enqueues all degrade to record-only
// with the reason, and never report a pane message.
#[test]
fn failed_enqueues_fall_back_to_record_only_with_reason() {
    for (tag, reply, needle) in [
        ("rf", EnqueueReply::Refused, "capability_denied"),
        (
            "mf",
            EnqueueReply::Malformed,
            "malformed inbox.enqueue reply",
        ),
        ("hg", EnqueueReply::Hang, "inbox.enqueue call failed"),
    ] {
        let sandbox = ChannelSandbox::spawn();
        let socket = short_socket(tag);
        let target = format!("easy-terminal:fail-{tag}");
        let log = spawn_daemon(&socket, vec![pane(&sandbox, "pane-x", &target)], reply);
        let said = handoff(&sandbox, &socket, &target);
        let delivery = &said["data"]["delivery"];
        assert_eq!(delivery["status"], "record_only", "{tag}: {said}");
        assert!(
            detail(&said).contains("wake failed") && detail(&said).contains(needle),
            "{tag}: {said}"
        );
        assert!(delivery.get("pane_message_id").is_none(), "{tag}: {said}");
        assert!(delivery.get("wake_event_id").is_none(), "{tag}: {said}");
        assert_eq!(enqueues(&log).len(), 1, "{tag}");
        let _ = std::fs::remove_file(&socket);
    }
}

#[test]
fn unbound_target_records_only_and_never_enqueues() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("unb");
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-o", "easy-terminal:someone-else")],
        EnqueueReply::Queued,
    );

    let said = handoff(&sandbox, &socket, "easy-terminal:idle-c");
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(
        detail(&said).contains("no live ptyd pane is bound"),
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
    let log = spawn_daemon(
        &socket,
        vec![pane(&sandbox, "pane-d", target)],
        EnqueueReply::Queued,
    );
    let mut args = handoff_args(SENDER, target, "x");
    args.extend(["--deliver", "record"]);
    let said = rally(&sandbox, &socket, &args);
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(enqueues(&log).is_empty());
    let _ = std::fs::remove_file(&socket);
}

/// Run rally from `cwd` (another room), pinned to `socket`, with the
/// sandbox's isolated HOME.
fn rally_in(sandbox: &ChannelSandbox, cwd: &Path, socket: &Path, args: &[&str]) -> Value {
    let mut cmd = base_command(sandbox, &Sockets::Pinned(socket));
    cmd.current_dir(cwd);
    run(cmd, args)
}

fn open_handoff_ids(room: &Value) -> String {
    room["data"]["room"]["open_handoffs"].to_string()
}

// 2026-10-31 regression: rooms are keyed by the writer's cwd. A lead sending
// `say handoff --to X` from repo A, while X's pane works in room B, wrote the
// obligation to A and refused the wake ("outside this room"); X, watching B,
// never saw it. The handoff must land in X's room B and wake X's pane there.
#[test]
fn targeted_handoff_from_another_room_lands_in_the_recipient_room() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("xroom");
    let target = "easy-terminal:xroom";
    // Room B: a second repository with an existing room, where the sender
    // holds the lead (as the team lead did in the home room).
    let room_b = sandbox.cwd().join("nested-room-b");
    std::fs::create_dir_all(room_b.join(".git")).unwrap();
    let log = spawn_daemon(
        &socket,
        vec![Pane {
            id: "pane-b",
            identity: target.to_string(),
            cwd: Some(room_b.clone()),
        }],
        EnqueueReply::Queued,
    );
    rally_in(
        &sandbox,
        &room_b,
        &socket,
        &["enter", "--tool", SENDER, "--json"],
    );

    // The sender's cwd is room A (the sandbox cwd).
    let said = handoff(&sandbox, &socket, target);
    let id = handoff_id(&said);
    assert_eq!(
        said["data"]["delivery"]["status"], "queued_pane_inbox",
        "the recipient's pane in room B is woken: {said}"
    );
    assert_eq!(enqueues(&log).len(), 1, "{said}");
    let warnings = said["data"]["warnings"].to_string();
    assert!(
        warnings.contains("handoff:routed_to_recipient_room"),
        "the sender is told where the handoff went: {said}"
    );

    // Visible to the recipient in room B, and not left as an orphan in A.
    let in_b = rally_in(&sandbox, &room_b, &socket, &["room", "--json"]);
    assert!(open_handoff_ids(&in_b).contains(&id), "room B: {in_b}");
    let in_a = rally(&sandbox, &socket, &["room", "--json"]);
    assert!(!open_handoff_ids(&in_a).contains(&id), "room A: {in_a}");
    let _ = std::fs::remove_file(&socket);
}

// When the recipient's pane works in a directory with no room yet, the
// handoff stays in the sender's room and the sender is told why.
#[test]
fn targeted_handoff_to_a_pane_without_a_room_warns_and_stays_here() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("xgap");
    let target = "easy-terminal:xgap";
    let elsewhere = sandbox.root().join("no-room");
    std::fs::create_dir_all(elsewhere.join(".git")).unwrap();
    let _log = spawn_daemon(
        &socket,
        vec![Pane {
            id: "pane-g",
            identity: target.to_string(),
            cwd: Some(elsewhere.clone()),
        }],
        EnqueueReply::Queued,
    );
    let said = handoff(&sandbox, &socket, target);
    let id = handoff_id(&said);
    let warnings = said["data"]["warnings"].to_string();
    assert!(
        warnings.contains("handoff:recipient_room_unknown"),
        "the gap is named: {said}"
    );
    let in_a = rally(&sandbox, &socket, &["room", "--json"]);
    assert!(open_handoff_ids(&in_a).contains(&id), "room A: {in_a}");
    assert!(
        !elsewhere.join(".rally").exists(),
        "no room is created in the recipient's directory"
    );
    let _ = std::fs::remove_file(&socket);
}

/// Room B (sibling of the sandbox cwd) with an existing room, bound to one
/// pane for `target`. `setup` runs rally commands in B before the daemon
/// starts answering for the pane.
fn routed_room(
    sandbox: &ChannelSandbox,
    tag: &str,
    target: &str,
    setup: &[&[&str]],
) -> (PathBuf, PathBuf, RequestLog) {
    let socket = short_socket(tag);
    let room_b = sandbox.root().join(format!("room-{tag}"));
    std::fs::create_dir_all(room_b.join(".git")).unwrap();
    let log = spawn_daemon(
        &socket,
        vec![Pane {
            id: "pane-b",
            identity: target.to_string(),
            cwd: Some(room_b.clone()),
        }],
        EnqueueReply::Queued,
    );
    for args in setup {
        rally_in(sandbox, &room_b, &socket, args);
    }
    (socket, room_b, log)
}

fn lead_of(sandbox: &ChannelSandbox, dir: &Path, socket: &Path) -> Value {
    rally_in(sandbox, dir, socket, &["room", "--json"])["data"]["room"]["lead"].clone()
}

// Review fix 1: routing into a room with an OPEN lead seat must not hand the
// visiting sender that seat.
#[test]
fn routed_handoff_never_takes_the_recipient_rooms_open_lead_seat() {
    let sandbox = ChannelSandbox::spawn();
    let target = "easy-terminal:xopen";
    // An executing-tier agent creates room B without taking the lead seat.
    let (socket, room_b, _log) = routed_room(
        &sandbox,
        "xopen",
        target,
        &[&[
            "enter",
            "--tool",
            "worker:09",
            "--tier",
            "executing",
            "--json",
        ]],
    );
    assert!(
        lead_of(&sandbox, &room_b, &socket).is_null(),
        "B starts leaderless"
    );

    let said = handoff(&sandbox, &socket, target);
    let id = handoff_id(&said);
    assert!(
        said["data"]["warnings"]
            .to_string()
            .contains("handoff:routed_to_recipient_room"),
        "{said}"
    );
    let in_b = rally_in(&sandbox, &room_b, &socket, &["room", "--json"]);
    assert!(open_handoff_ids(&in_b).contains(&id), "room B: {in_b}");
    assert!(
        lead_of(&sandbox, &room_b, &socket).is_null(),
        "the sender must not become B's lead: {in_b}"
    );
    let _ = std::fs::remove_file(&socket);
}

// Review fix 3: in a room led by someone else, a non-lead sender without the
// recipient's consent records the handoff there but does not wake the pane.
#[test]
fn routed_handoff_into_a_room_with_another_lead_is_record_only() {
    let sandbox = ChannelSandbox::spawn();
    let target = "easy-terminal:xother";
    let (socket, room_b, log) = routed_room(
        &sandbox,
        "xother",
        target,
        &[&["enter", "--tool", "other-lead:01", "--json"]],
    );
    let said = handoff(&sandbox, &socket, target);
    let id = handoff_id(&said);
    assert_eq!(said["data"]["delivery"]["status"], "record_only", "{said}");
    assert!(detail(&said).contains("pane wake refused"), "{said}");
    assert!(
        detail(&said).contains("room for pull"),
        "detail says where it waits: {said}"
    );
    assert!(enqueues(&log).is_empty(), "no pane may be typed into");
    let in_b = rally_in(&sandbox, &room_b, &socket, &["room", "--json"]);
    assert!(open_handoff_ids(&in_b).contains(&id), "room B: {in_b}");
    assert_eq!(lead_of(&sandbox, &room_b, &socket), "other-lead:01");
    let _ = std::fs::remove_file(&socket);
}

// Review fix 2: path scopes of a routed handoff stay relative to the sender's
// repository; no base markers are hashed against the wrong tree.
#[test]
fn routed_handoff_records_scope_root_and_no_base_markers() {
    let sandbox = ChannelSandbox::spawn();
    let target = "easy-terminal:xpath";
    let (socket, _room_b, _log) = routed_room(
        &sandbox,
        "xpath",
        target,
        &[&["enter", "--tool", SENDER, "--json"]],
    );
    std::fs::write(sandbox.cwd().join("notes.txt"), "x").unwrap();
    let mut args = handoff_args(SENDER, target, "edit the notes");
    args.extend(["--path", "notes.txt"]);
    let said = rally(&sandbox, &socket, &args);
    let evidence = said["data"]["say"]["fact"]["evidence"].to_string();
    assert!(evidence.contains("scope_root:"), "{said}");
    assert!(
        !evidence.contains("basehash:") && !evidence.contains("base_commit:"),
        "{said}"
    );
    let _ = std::fs::remove_file(&socket);
}

// AGEN-DELIVERY (2026-11-08): `rally routes` listed every unmanaged actor as
// `polling_only` without looking at pane bindings, while `say handoff` woke
// the same actor through its bound pane. Routes now reads the same verdict
// delivery uses, so the route shown is the route a handoff takes.
#[test]
fn routes_report_the_pane_verdict_handoff_delivery_uses() {
    let sandbox = ChannelSandbox::spawn();
    let socket = short_socket("routes");
    let foreign = sandbox.root().join("empty-room");
    std::fs::create_dir_all(foreign.join(".rally")).unwrap();
    // A second repository WITH a room: handoffs to its pane are routed there.
    let other_repo = std::env::temp_dir().join(unique("hipw-routes-room"));
    std::fs::create_dir_all(other_repo.join(".git")).unwrap();
    let mut init = base_command(&sandbox, &Sockets::Pinned(&socket));
    init.current_dir(&other_repo);
    run(
        init,
        &[
            "status",
            "post",
            "--json",
            "--tool",
            "easy-terminal:other-room",
            "--state",
            "idle",
        ],
    );
    let _log = spawn_daemon(
        &socket,
        vec![
            pane(&sandbox, "pane-r", "easy-terminal:bound"),
            Pane {
                id: "pane-x",
                identity: "easy-terminal:elsewhere".to_string(),
                cwd: Some(foreign.clone()),
            },
            Pane {
                id: "pane-y",
                identity: "easy-terminal:other-room".to_string(),
                cwd: Some(other_repo.clone()),
            },
        ],
        EnqueueReply::Queued,
    );
    // The sender joins first, so it holds the lead seat a pane wake requires.
    rally(
        &sandbox,
        &socket,
        &[
            "status", "post", "--json", "--tool", SENDER, "--state", "idle",
        ],
    );
    for actor in [
        "easy-terminal:bound",
        "easy-terminal:elsewhere",
        "easy-terminal:other-room",
        "easy-terminal:unbound",
    ] {
        rally(
            &sandbox,
            &socket,
            &[
                "status", "post", "--json", "--tool", actor, "--state", "idle",
            ],
        );
    }
    // A route preview must still report the recipient when another process
    // owns that room. The lock stays held until after the preview returns.
    let recipient_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(other_repo.join(".rally/direct.owner.lock"))
        .unwrap();
    recipient_lock.try_lock().expect("hold recipient room lock");
    let routes = rally(
        &sandbox,
        &socket,
        &["--timeout-ms", "5000", "routes", "--json"],
    );
    drop(recipient_lock);
    let items = routes["data"]["routes"]["items"]
        .as_array()
        .expect("routes items");
    let state = |actor: &str| {
        items
            .iter()
            .find(|r| r["actor"] == actor)
            .unwrap_or_else(|| panic!("{actor} missing: {routes}"))
            .clone()
    };
    let bound = state("easy-terminal:bound");
    assert_eq!(bound["state"], "pane_inbox", "{bound}");
    assert_eq!(bound["capabilities"]["pane_inbox"], true, "{bound}");
    assert!(
        bound["reason"].as_str().unwrap().contains("pane-r"),
        "{bound}"
    );
    assert_eq!(
        state("easy-terminal:elsewhere")["state"],
        "pane_outside_room"
    );
    assert_eq!(state("easy-terminal:unbound")["state"], "polling_only");
    let other = state("easy-terminal:other-room");
    assert_eq!(other["state"], "pane_inbox_recipient_room", "{other}");
    assert_eq!(other["capabilities"]["pane_inbox"], true, "{other}");

    // The handoff takes the route routes reported.
    let said = handoff(&sandbox, &socket, "easy-terminal:bound");
    assert_eq!(
        said["data"]["delivery"]["status"], "queued_pane_inbox",
        "{said}"
    );
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&foreign);
    let _ = std::fs::remove_dir_all(&other_repo);
}

struct RoomDaemon(Child);

impl RoomDaemon {
    fn start(sandbox: &ChannelSandbox, cwd: &Path, socket: &Path) -> Self {
        let mut command = base_command(sandbox, &Sockets::Pinned(socket));
        let daemon = Self(
            command
                .current_dir(cwd)
                .args(["daemon", "serve", "--idle-exit-secs", "180"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start Rally daemon"),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let status = rally_in(sandbox, cwd, socket, &["daemon", "status", "--json"]);
            if status["data"]["daemon"]["live"] == true {
                return daemon;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("Rally daemon not live in {}", cwd.display());
    }
}

impl Drop for RoomDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn concurrent_routes_resolve_nested_rooms_in_direct_and_daemon_modes() {
    let sandbox = ChannelSandbox::spawn();
    let outer = sandbox.cwd().canonicalize().unwrap();
    let inner = outer.join("nested");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    for routed in [false, true] {
        let outer_socket = short_socket("route-a");
        let inner_socket = short_socket("route-b");
        let rooms = [
            (&outer, &outer_socket, "easy-terminal:inner", &inner),
            (&inner, &inner_socket, "easy-terminal:outer", &outer),
        ];
        for (cwd, socket, actor, _) in rooms {
            rally_in(
                &sandbox,
                cwd,
                socket,
                &[
                    "status", "post", "--json", "--tool", actor, "--state", "idle",
                ],
            );
            let status = rally_in(&sandbox, cwd, socket, &["daemon", "status", "--json"]);
            assert_eq!(status["data"]["daemon"]["live"], false, "{status}");
        }
        let _daemons = routed
            .then(|| rooms.map(|(cwd, socket, _, _)| RoomDaemon::start(&sandbox, cwd, socket)));

        // Both previews reach pane.list only after opening their own room.
        // In direct mode an opposing foreign-room open would now contend.
        let rendezvous = Arc::new(Barrier::new(2));
        for (_, socket, actor, destination) in rooms {
            spawn_pane_server(
                socket,
                vec![Pane {
                    id: "remote-pane",
                    identity: actor.to_string(),
                    cwd: Some(destination.clone()),
                }],
                EnqueueReply::Queued,
                Some(Arc::clone(&rendezvous)),
            );
        }
        thread::scope(|scope| {
            let lookups = rooms.map(|(cwd, socket, actor, destination)| {
                let sandbox = &sandbox;
                scope.spawn(move || {
                    let response = rally_in(sandbox, cwd, socket, &["routes", "--json"]);
                    let route = response["data"]["routes"]["items"]
                        .as_array()
                        .unwrap_or_else(|| panic!("daemon={routed}: {response}"))
                        .iter()
                        .find(|route| route["actor"] == actor)
                        .unwrap_or_else(|| panic!("daemon={routed}, actor={actor}: {response}"));
                    assert_eq!(
                        route["state"], "pane_inbox_recipient_room",
                        "daemon={routed}: {route}"
                    );
                    assert!(
                        route["reason"]
                            .as_str()
                            .unwrap()
                            .contains(&format!("can route to {} if", destination.display())),
                        "daemon={routed}: {route}"
                    );
                })
            });
            for lookup in lookups {
                lookup.join().unwrap();
            }
        });
        let _ = std::fs::remove_file(outer_socket);
        let _ = std::fs::remove_file(inner_socket);
    }
}
