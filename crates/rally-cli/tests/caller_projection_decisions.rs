// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Decisions read from the bounded caller projection (AGEN-ENTER-m6ztgb9h6p7barc8cr79q).
//!
//! `SnapshotForEnter` / `SnapshotForObligationTarget` bound their lists so a
//! large room cannot overflow the 8 MiB frame. A decision that writes to the
//! ledger or tells an agent what to do must never read a row that bound could
//! evict. Each test floods exactly the bucket one decision reads past its
//! optional bound and asserts the decision through the real CLI, routed
//! through a real `rally daemon serve`.

#![cfg(unix)]
#![allow(clippy::zombie_processes)]

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIGTERM: i32 = 15;
const ME: &str = "worker-01";
const ENGAGEMENT: &str = "caller-projection-decisions";
/// Raised watchdog: these rooms hold thousands of facts and the suite runs on
/// a loaded machine; a timeout here would certify nothing.
const WATCHDOG_MS: &str = "60000";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

struct Room {
    cwd: PathBuf,
    home: PathBuf,
    seq: i64,
    segment: Option<fs::File>,
}

impl Room {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("cpd-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("cpd-{name}-{nanos}-home"));
        let rally = cwd.join(".rally");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(rally.join("log")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(rally.join("active-engagement"), format!("{ENGAGEMENT}\n")).unwrap();
        let segment =
            fs::File::create(rally.join("log").join(format!("{ENGAGEMENT}.jsonl"))).unwrap();
        Self {
            cwd,
            home,
            seq: 0,
            segment: Some(segment),
        }
    }

    /// Append one fact to the segment. `fact` holds the kind-specific fields.
    fn fact(&mut self, mut fact: Value) {
        self.seq += 1;
        let seq = self.seq;
        let map = fact.as_object_mut().unwrap();
        map.insert("schema".into(), json!("agent-rally.fact.v1"));
        map.insert("seq".into(), json!(seq));
        map.entry("event_id")
            .or_insert(json!(format!("fixture-{seq}")));
        map.insert("thread_id".into(), json!(format!("fixture-thread-{seq}")));
        map.entry("created_at").or_insert(json!(ago(0)));
        let created_at = map["created_at"].clone();
        let kind = map["kind"].clone();
        writeln!(
            self.segment.as_mut().unwrap(),
            "{}",
            json!({
                "seq": seq,
                "occurred_at": created_at,
                "event_type": kind,
                "payload": fact,
                "engagement": ENGAGEMENT,
            })
        )
        .unwrap();
    }

    fn seal(&mut self) {
        if let Some(segment) = self.segment.take() {
            segment.sync_all().unwrap();
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_HOOK_TIMEOUT_MS", WATCHDOG_MS)
            // `self-exit-check` resolves its own tmux session from these; the
            // fixture must never address a real terminal.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .args(args);
        cmd
    }

    /// Re-open the segment for append after CLI writes, continuing their seq.
    fn reopen(&mut self) {
        self.seq = self
            .ledger()
            .iter()
            .filter_map(|f| f["seq"].as_i64())
            .max()
            .unwrap_or(0);
        self.segment = Some(
            fs::OpenOptions::new()
                .append(true)
                .open(
                    self.cwd
                        .join(".rally/log")
                        .join(format!("{ENGAGEMENT}.jsonl")),
                )
                .unwrap(),
        );
    }

    fn json_as(&self, session: &str, args: &[&str]) -> Value {
        let out = self
            .command(args)
            .env("RALLY_SESSION_ID", session)
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env_remove("RALLY_MANAGED_SESSION_MODE")
            .env_remove("PWD")
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?}: {e}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        assert_eq!(value["ok"], json!(true), "cmd {args:?} failed: {value}");
        value
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let value: Value = serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?} did not emit JSON ({e})\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            )
        });
        assert_eq!(value["ok"], json!(true), "cmd {args:?} failed: {value}");
        value
    }

    fn ledger(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.cwd.join(".rally").join("log"))
            .unwrap()
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            for line in fs::read_to_string(&path).unwrap().lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && let Some(payload) = v.get("payload")
                {
                    out.push(payload.clone());
                }
            }
        }
        out
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn padding(bytes: usize) -> String {
    "p".repeat(bytes)
}

/// A live `rally daemon serve` bound to a room. SIGTERM + reap on Drop.
struct Daemon {
    child: Child,
    addr: PathBuf,
}

impl Daemon {
    fn start(room: &Room) -> Self {
        let log_path = room.cwd.join(".rally").join("rallyd-serve.log");
        let log = fs::File::create(&log_path).unwrap();
        let child = room
            .command(&["daemon", "serve", "--idle-exit-secs", "300"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        let mut daemon = Daemon {
            child,
            addr: room.cwd.join(".rally").join("rallyd.sock.addr"),
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if daemon.addr.exists() && room.run(&["daemon", "status", "--json"]).status.success() {
                return daemon;
            }
            if let Ok(Some(status)) = daemon.child.try_wait() {
                panic!(
                    "rallyd exited before ready ({status}): {}",
                    fs::read_to_string(&log_path).unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
        daemon.stop();
        panic!(
            "rallyd not ready in 60s: {}",
            fs::read_to_string(&log_path).unwrap_or_default()
        );
    }

    fn stop(&mut self) {
        // SAFETY: `kill(2)` on our own spawned child.
        unsafe {
            kill(self.child.id() as i32, SIGTERM);
        }
        let _ = self.child.wait();
        fs::remove_file(&self.addr).ok();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn no_response_risks(room: &Room) -> usize {
    room.ledger()
        .iter()
        .filter(|f| f["kind"] == "risk")
        .filter(|f| {
            f["subject"]
                .as_str()
                .is_some_and(|s| s.starts_with("no-response:"))
        })
        .count()
}

/// D1: the "already escalated" guard must see every no-response risk for the
/// caller's own handoffs, however many there are. 400 overdue handoffs, each
/// already escalated by a 1.5 KB risk: the risks exceed one optional bucket.
#[test]
fn next_never_re_escalates_handoffs_whose_risks_exceed_the_bucket_bound() {
    const OVERDUE: usize = 400;
    let mut room = Room::new("d1");
    for n in 0..OVERDUE {
        room.fact(json!({
            "event_id": format!("overdue-{n}"),
            "kind": "handoff",
            "tool": ME,
            "target": format!("peer-{n}"),
            "subject": format!("overdue handoff {n}"),
            "evidence": [format!("ack-by:{}", ago(3_600))],
            "created_at": ago(7_200),
        }));
    }
    for n in 0..OVERDUE {
        room.fact(json!({
            "kind": "risk",
            "tool": ME,
            "target": ME,
            "subject": format!("no-response: peer-{n} did not ack handoff overdue-{n}"),
            "summary": padding(1_500),
            "severity": "warn",
            "scope": [format!("no-response-to:overdue-{n}")],
            "ref": format!("overdue-{n}"),
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    assert_eq!(no_response_risks(&room), OVERDUE);
    for run in 1..=2 {
        let next = room.json(&["next", "--tool", ME, "--json"]);
        assert_eq!(
            next["data"]["overdue_handoffs"].as_array().map(Vec::len),
            Some(OVERDUE),
            "run {run}: every overdue handoff must be visible: {}",
            next["data"]["overdue_handoffs"]
        );
        assert_eq!(
            no_response_risks(&room),
            OVERDUE,
            "run {run} appended duplicate no-response risks"
        );
    }
}

/// D2: liveness of the peer the caller waits on must be exact. Participant
/// squads (authors of open handoffs, claims and blockers: 1,500) exceed the
/// squad bound; the caller's handoff target is the oldest-seen squad and
/// takeover-eligible (silent 3 h).
#[test]
fn next_sees_a_dead_waited_on_peer_past_the_squad_bound() {
    const PARTICIPANTS: usize = 600;
    let mut room = Room::new("d2");
    room.fact(json!({
        "kind": "presence",
        "tool": "dead-peer",
        "subject": "presence",
        "created_at": ago(3 * 3_600),
    }));
    // Written before the flood so dead-peer is the oldest-seen squad however
    // the squad projection credits a fact's target.
    room.fact(json!({
        "event_id": "wait-on-dead",
        "kind": "handoff",
        "tool": ME,
        "target": "dead-peer",
        "subject": "waiting on a silent peer",
        "created_at": ago(600),
    }));
    for n in 0..PARTICIPANTS {
        room.fact(json!({
            "kind": "handoff",
            "tool": format!("p-{n}"),
            "target": format!("p-{}", n + 1),
            "subject": format!("participant handoff {n}"),
        }));
        room.fact(json!({
            "kind": "claim",
            "tool": format!("c-{n}"),
            "subject": format!("participant claim {n}"),
            "scope": [format!("file:src/c{n}.rs")],
            "evidence": ["lease_expires_at:2099-01-01T00:00:00Z"],
        }));
        if n % 2 == 0 {
            room.fact(json!({
                "kind": "blocker",
                "tool": format!("b-{n}"),
                "subject": format!("participant blocker {n}"),
                "scope": [format!("file:src/b{n}.rs")],
            }));
        }
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    let next = room.json(&["next", "--audit", "--tool", ME, "--json"]);
    assert_eq!(
        next["data"]["room"]["open_handoffs"],
        json!(PARTICIPANTS + 1),
        "fixture must be loaded"
    );
    let waiting = next["data"]["next"]["waiting_on"].as_array().unwrap();
    assert!(
        !waiting.iter().any(|f| f["event_id"] == "wait-on-dead"),
        "a takeover-eligible target must not hold the caller in wait: {waiting:?}"
    );
}

/// D3: accepted-but-open named work must stay visible however many handoffs
/// the caller has sent. 600 outgoing handoffs plus one accepted incoming one.
#[test]
fn accepted_named_work_survives_more_outgoing_handoffs_than_the_bound() {
    const OUTGOING: usize = 600;
    let mut room = Room::new("d3");
    room.seal();
    // Oldest rows: the incoming handoff and the caller's typed acceptance,
    // written by the real CLI so the protocol markers are exact.
    let incoming = room.json_as(
        "lead-session-1",
        &[
            "say",
            "handoff",
            "--json",
            "--tool",
            "lead-01",
            "--target",
            ME,
            "--subject",
            "accepted work for worker-01",
        ],
    )["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    room.json_as(
        "worker-session-1",
        &[
            "say",
            "handoff",
            "--json",
            "--tool",
            ME,
            "--ref",
            &incoming,
            "--handoff-state",
            "accepted",
            "--subject",
            "reply",
            "--summary",
            "accepted",
        ],
    );
    room.reopen();
    for n in 0..OUTGOING {
        room.fact(json!({
            "kind": "handoff",
            "tool": ME,
            "target": format!("peer-{n}"),
            "subject": format!("outgoing {n}"),
            "summary": padding(400),
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    let next = room.json(&["next", "--audit", "--tool", ME, "--json"]);
    assert_eq!(
        next["data"]["room"]["open_handoffs"],
        json!(OUTGOING + 1),
        "fixture must be loaded"
    );
    let named = |v: &Value| {
        v["data"]["next"]["target_event_id"] == incoming.as_str()
            || v["data"]["next"]["alternatives"]
                .as_array()
                .is_some_and(|alts| {
                    alts.iter()
                        .any(|a| a["target_event_id"] == incoming.as_str())
                })
    };
    assert!(
        named(&next),
        "next lost accepted named work: {}",
        next["data"]["next"]
    );
    assert_eq!(next["data"]["next"]["inbox"]["accepted_open"], json!(1));
    let check = room.json(&["self-exit-check", "--tool", ME, "--persistent", "--json"]);
    assert_eq!(
        check["data"]["self_exit_check"]["next_actionable"],
        json!(true),
        "self-exit-check counted an empty cycle with accepted work open: {check}"
    );
    assert_eq!(
        check["data"]["self_exit_check"]["empty_cycle"],
        json!(false)
    );
}

/// D5 (+ D4): a room whose snapshot is over the frame WITHOUT stale facts.
/// Actionable `next` writes a wake and must keep answering; `enter` must report
/// the true attention total; `inbox` must answer.
#[test]
fn next_after_a_wake_stays_bounded_when_the_room_is_over_the_frame() {
    const ELSEWHERE: usize = 8_000;
    const OPEN_QUEUE: usize = 600;
    let mut room = Room::new("d5");
    room.fact(json!({
        "event_id": "named-to-me",
        "kind": "handoff",
        "tool": "lead-01",
        "target": ME,
        "subject": "work for worker-01",
    }));
    for n in 0..ELSEWHERE {
        room.fact(json!({
            "kind": "handoff",
            "tool": format!("p-{n}"),
            "target": format!("p-{}", n + 1),
            "subject": format!("peer handoff {n}"),
            "summary": padding(1_000),
        }));
    }
    for n in 0..OPEN_QUEUE {
        room.fact(json!({
            "kind": "handoff",
            "tool": "queue-poster",
            "subject": format!("open queue item {n}"),
        }));
    }
    room.seal();
    // Precondition: open handoffs alone (no stale facts) outweigh the 8 MiB
    // frame, so a full snapshot cannot cross it whatever the daemon drops.
    let segment = room
        .cwd
        .join(".rally/log")
        .join(format!("{ENGAGEMENT}.jsonl"));
    assert!(fs::metadata(&segment).unwrap().len() > 8 * 1024 * 1024);
    let _daemon = Daemon::start(&room);

    let enter = room.json(&["enter", "--tool", ME, "--json"]);
    let total = enter["data"]["enter"]["attention_total"].as_u64().unwrap() as usize;
    assert!(
        total > OPEN_QUEUE,
        "first enter must count every open-queue item and the named handoff, got {total}"
    );
    assert_eq!(
        enter["data"]["room"]["open_handoffs"].as_u64().unwrap() as usize,
        1 + ELSEWHERE + OPEN_QUEUE
    );
    for run in 1..=2 {
        let next = room.json(&["next", "--tool", ME, "--json"]);
        assert_eq!(
            next["data"]["next"]["actionable"],
            json!(true),
            "run {run}: {next}"
        );
        assert!(
            !next["data"]["wake_intent"].is_null(),
            "run {run}: actionable next must write or coalesce a wake"
        );
    }
    let inbox = room.json(&["inbox", "--tool", ME, "--json"]);
    assert_eq!(inbox["data"]["inbox"]["count"], json!(1));
    assert_eq!(
        inbox["data"]["inbox"]["open_queue"]["count"]
            .as_u64()
            .unwrap() as usize,
        OPEN_QUEUE
    );
}

/// D6: `--path` relevance. Every claim is guaranteed, so a peer claim on the
/// caller's path is visible however many peer claims (600 x ~1 KB) exist.
#[test]
fn enter_path_sees_the_oldest_peer_claim_past_the_bucket_bound() {
    const CLAIMS: usize = 600;
    let mut room = Room::new("d6");
    for n in 0..CLAIMS {
        room.fact(json!({
            "event_id": format!("claim-{n}"),
            "kind": "claim",
            "tool": format!("claimer-{n}"),
            "subject": format!("claim {n}"),
            "scope": [format!("file:src/f{n}.rs")],
            "summary": padding(1_000),
            "evidence": ["lease_expires_at:2099-01-01T00:00:00Z"],
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    let enter = room.json(&["enter", "--tool", ME, "--path", "src/f0.rs", "--json"]);
    let do_not = enter["data"]["enter"]["entry"]["do_not"]
        .as_array()
        .unwrap();
    assert!(
        do_not.iter().any(|item| item["event_id"] == "claim-0"),
        "the oldest peer claim on --path must be avoided: {do_not:?}"
    );
}

/// R1: a sender whose already-escalated handoffs (each with its no-response
/// risk) exceed the must-keep budget still gets `enter` and `next`; `next`
/// twice appends zero duplicate risks; an older un-escalated overdue handoff
/// is never evicted and is escalated exactly once.
#[test]
fn escalated_pairs_past_the_budget_are_omitted_together_and_never_re_escalated() {
    const ESCALATED: usize = 1_400;
    let mut room = Room::new("r1");
    room.fact(json!({
        "event_id": "not-yet-escalated",
        "kind": "handoff",
        "tool": ME,
        "target": "slow-peer",
        "subject": "oldest overdue handoff",
        "evidence": [format!("ack-by:{}", ago(3_600))],
        "created_at": ago(7_200),
    }));
    for n in 0..ESCALATED {
        room.fact(json!({
            "event_id": format!("escalated-{n}"),
            "kind": "handoff",
            "tool": ME,
            "target": format!("peer-{n}"),
            "subject": format!("escalated handoff {n}"),
            "summary": padding(2_000),
            "evidence": [format!("ack-by:{}", ago(3_600))],
            "created_at": ago(7_200),
        }));
        room.fact(json!({
            "kind": "risk",
            "tool": ME,
            "target": ME,
            "subject": format!("no-response: peer-{n} did not ack handoff escalated-{n}"),
            "severity": "warn",
            "scope": [format!("no-response-to:escalated-{n}")],
            "ref": format!("escalated-{n}"),
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    let enter = room.json(&["enter", "--tool", ME, "--json"]);
    assert!(
        enter["data"]["room"]["omitted"]["escalated_handoffs"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "fixture must push escalated pairs past the budget: {}",
        enter["data"]["room"]
    );
    assert_eq!(enter["data"]["room"]["open_handoffs"], json!(ESCALATED + 1));
    let escalations_of = |room: &Room, id: &str| {
        room.ledger()
            .iter()
            .filter(|f| f["kind"] == "risk" && f["ref"] == id)
            .count()
    };
    for run in 1..=2 {
        room.json(&["next", "--tool", ME, "--json"]);
        assert_eq!(
            no_response_risks(&room),
            ESCALATED + 1,
            "run {run}: exactly one new risk overall"
        );
        assert_eq!(escalations_of(&room, "not-yet-escalated"), 1, "run {run}");
    }
}

/// R2: live peer claims past the old 3 MiB guarantee are should-keep: an
/// unrelated caller's `enter`, `next` and `inbox` succeed and report the exact
/// omitted claim count.
#[test]
fn peer_claims_past_the_budget_trim_with_an_exact_count_instead_of_failing() {
    const CLAIMS: usize = 7_000;
    let mut room = Room::new("r2");
    for n in 0..CLAIMS {
        room.fact(json!({
            "kind": "claim",
            "tool": format!("claimer-{n}"),
            "subject": format!("claim {n}"),
            "scope": [format!("file:src/c{n}.rs")],
            "summary": padding(450),
            "evidence": ["lease_expires_at:2099-01-01T00:00:00Z"],
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    for args in [
        &["enter", "--tool", ME, "--json"][..],
        &["next", "--audit", "--tool", ME, "--json"][..],
    ] {
        let out = room.json(args);
        let summary = &out["data"]["room"];
        assert_eq!(summary["active_claims"], json!(CLAIMS), "{args:?}");
        let omitted = summary["omitted"]["active_claims"].as_u64().unwrap_or(0);
        assert!(
            omitted > 0,
            "{args:?} must report omitted claims: {summary}"
        );
    }
    room.json(&["inbox", "--tool", ME, "--json"]);
}

const FLOODED: &str = "flooded-01";
const FLOODED_OWN: usize = 1_700;

/// A room where `FLOODED`'s OWN un-escalated handoffs exceed the must-keep
/// bound, and `ME` has one ordinary handoff.
fn over_limit_room(name: &str) -> Room {
    let mut room = Room::new(name);
    room.fact(json!({
        "event_id": "for-me",
        "kind": "handoff",
        "tool": "lead-01",
        "target": ME,
        "subject": "work for worker-01",
    }));
    for n in 0..FLOODED_OWN {
        room.fact(json!({
            "kind": "handoff",
            "tool": FLOODED,
            "target": format!("peer-{n}"),
            "subject": format!("own {n}"),
            "summary": padding(2_000),
        }));
    }
    room.seal();
    room
}

/// R5: the over-limit tool gets the typed `caller-projection-too-large`
/// error through the daemon, naming only its own rows.
#[test]
fn over_limit_tool_gets_a_typed_error_naming_only_its_own_rows() {
    let room = over_limit_room("r5");
    let _daemon = Daemon::start(&room);
    let out = room.run(&["next", "--audit", "--tool", FLOODED, "--json"]);
    assert!(!out.status.success());
    let body = if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    let err: Value = serde_json::from_slice(body).unwrap();
    let message = err["error"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("caller-projection-too-large: "),
        "{err}"
    );
    assert!(
        message.contains(&format!("{FLOODED_OWN} un-escalated handoffs you sent")),
        "message must name the caller's own rows: {message}"
    );
    assert!(!message.contains("peer-"), "no peer rows named: {message}");
}

#[test]
fn caller_over_projection_budget_can_still_post_from_a_new_session() {
    let room = over_limit_room("presence-lead");
    let _daemon = Daemon::start(&room);
    let say = room.json_as(
        "new-session",
        &[
            "say",
            "artifact",
            "--tool",
            FLOODED,
            "--subject",
            "status update",
            "--json",
        ],
    );
    assert_eq!(say["data"]["say"]["committed"], json!(true));
}

/// R4: multi-tool `inbox` reports the over-limit tool's error in its own
/// entry and returns the other tools normally.
#[test]
fn multi_tool_inbox_returns_the_others_when_one_tool_is_over_the_limit() {
    let room = over_limit_room("r4");
    let _daemon = Daemon::start(&room);
    let inbox = room.json(&["inbox", "--tool", FLOODED, "--tool", ME, "--json"]);
    let data = &inbox["data"];
    assert_eq!(data["inboxes"][ME]["count"], json!(1), "{data}");
    assert!(
        data["errors"][FLOODED]
            .as_str()
            .is_some_and(|e| e.starts_with("caller-projection-too-large")),
        "{data}"
    );
    assert!(data["inboxes"].get(FLOODED).is_none());
}

/// F1: "escalated" means a no-response risk exists, by the escalation code's
/// own predicate. An overdue handoff carrying only an UNRELATED risk is not
/// escalated, so it stays must-keep under budget pressure and `next`
/// escalates it exactly once.
#[test]
fn an_unrelated_risk_does_not_mark_a_handoff_escalated() {
    const ESCALATED: usize = 1_400;
    let mut room = Room::new("f1");
    room.fact(json!({
        "event_id": "has-unrelated-risk",
        "kind": "handoff",
        "tool": ME,
        "target": "slow-peer",
        "subject": "oldest overdue handoff",
        "evidence": [format!("ack-by:{}", ago(3_600))],
        "created_at": ago(7_200),
    }));
    room.fact(json!({
        "kind": "risk",
        "tool": ME,
        "subject": "flaky CI on this branch",
        "severity": "warn",
        "ref": "has-unrelated-risk",
        "created_at": ago(7_100),
    }));
    for n in 0..ESCALATED {
        room.fact(json!({
            "event_id": format!("escalated-{n}"),
            "kind": "handoff",
            "tool": ME,
            "target": format!("peer-{n}"),
            "subject": format!("escalated handoff {n}"),
            "summary": padding(2_000),
            "evidence": [format!("ack-by:{}", ago(3_600))],
            "created_at": ago(7_000),
        }));
        room.fact(json!({
            "kind": "risk",
            "tool": ME,
            "target": ME,
            "subject": format!("no-response: peer-{n} did not ack handoff escalated-{n}"),
            "severity": "warn",
            "ref": format!("escalated-{n}"),
        }));
    }
    room.seal();
    let _daemon = Daemon::start(&room);
    let no_response_for = |room: &Room| {
        room.ledger()
            .iter()
            .filter(|f| f["kind"] == "risk" && f["ref"] == "has-unrelated-risk")
            .filter(|f| {
                f["subject"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("no-response:"))
            })
            .count()
    };
    for run in 1..=2 {
        let next = room.json(&["next", "--tool", ME, "--json"]);
        assert!(
            next["data"]["room"]["omitted"]["escalated_handoffs"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "fixture must push escalated pairs past the budget"
        );
        assert_eq!(no_response_for(&room), 1, "run {run}");
    }
}

#[test]
fn addressed_flood_is_pageable_and_accepted_work_count_stays_exact() {
    exercise_addressed_flood(true);
}

#[test]
fn direct_addressed_flood_is_pageable_and_accepted_work_count_stays_exact() {
    exercise_addressed_flood(false);
}

fn exercise_addressed_flood(routed: bool) {
    const ADDRESSED: usize = 4_000;
    let mut room = Room::new("addressed-pages");
    room.seal();
    let accepted = room.json_as(
        "sender",
        &[
            "say",
            "handoff",
            "--tool",
            "sender",
            "--target",
            ME,
            "--subject",
            "accepted oldest assignment",
            "--json",
        ],
    )["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    room.json_as(
        "receiver",
        &[
            "say",
            "handoff",
            "--tool",
            ME,
            "--ref",
            &accepted,
            "--handoff-state",
            "accepted",
            "--subject",
            "accept",
            "--json",
        ],
    );
    room.reopen();
    for n in 0..ADDRESSED {
        room.fact(json!({
            "event_id": format!("addressed-{n}"), "kind": "handoff",
            "tool": "sender", "from_session_id": "sess:parent:sender#live", "target": ME, "subject": format!("assignment {n}"),
            "summary": padding(1_000),
        }));
    }
    room.seal();
    let _daemon = routed.then(|| Daemon::start(&room));
    let enter = room.json(&["enter", "--tool", ME, "--json"]);
    assert_eq!(
        enter["data"]["enter"]["addressed_page"]["total"],
        json!(ADDRESSED + 1)
    );
    let next = room.json(&["next", "--audit", "--tool", ME, "--json"]);
    assert_eq!(next["data"]["next"]["actionable"], json!(true));
    assert_eq!(next["data"]["next"]["inbox"]["accepted_open"], json!(1));
    let check = room.json(&["self-exit-check", "--tool", ME, "--persistent", "--json"]);
    assert_eq!(
        check["data"]["self_exit_check"]["empty_cycle"],
        json!(false)
    );
    let inbox = room.json(&["inbox", "--tool", ME, "--limit", "20", "--json"]);
    let inbox = &inbox["data"]["inbox"];
    assert_eq!(inbox["count"], json!(ADDRESSED));
    assert_eq!(inbox["accepted_open"], json!(1));
    assert!(
        !inbox["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["event_id"] == accepted)
    );
    let before = inbox["addressed_page"]["next_before"]
        .as_i64()
        .unwrap()
        .to_string();
    let older = room.json(&[
        "inbox", "--tool", ME, "--before", &before, "--limit", "20", "--json",
    ]);
    let first_ids = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["event_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        older["data"]["inbox"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| !first_ids.contains(f["event_id"].as_str().unwrap()))
    );
    for (n, id) in first_ids.iter().enumerate() {
        room.json_as(
            "receiver",
            &[
                "say",
                "handoff",
                "--tool",
                ME,
                "--ref",
                id,
                "--handoff-state",
                if n % 2 == 0 { "accepted" } else { "rejected" },
                "--subject",
                "reject",
                "--json",
            ],
        );
    }
    let after = room.json(&["inbox", "--tool", ME, "--json"]);
    assert_eq!(
        after["data"]["inbox"]["count"],
        json!(ADDRESSED - first_ids.len())
    );
    assert_eq!(
        after["data"]["inbox"]["accepted_open"],
        json!(1 + first_ids.len().div_ceil(2))
    );
}

#[test]
fn later_enter_attention_counts_trimmed_rows_above_its_cursor_exactly() {
    exercise_later_attention(true);
}

#[test]
fn direct_later_enter_attention_counts_trimmed_rows_above_its_cursor_exactly() {
    exercise_later_attention(false);
}

fn exercise_later_attention(routed: bool) {
    let mut room = Room::new("attention-cursor");
    room.seal();
    let first = room.json(&["enter", "--tool", ME, "--json"]);
    assert!(first["data"]["enter"]["cursor"]["after"].as_i64().unwrap() > 0);
    room.reopen();
    for n in 0..800 {
        room.fact(json!({"kind": "handoff", "tool": "queue-poster", "subject": format!("available {n}"), "summary": padding(1_000)}));
    }
    room.seal();
    let _daemon = routed.then(|| Daemon::start(&room));
    let second = room.json(&["enter", "--tool", ME, "--json"]);
    assert!(
        second["data"]["enter"]["cursor"]["before"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert_eq!(second["data"]["enter"]["attention_total"], json!(800));
}

#[test]
fn direct_oversized_sender_can_be_acknowledged_by_ref() {
    let mut room = Room::new("large-sender");
    room.fact(json!({
        "event_id": "large-sender", "kind": "handoff", "tool": "sender".repeat(256 * 1_024),
        "from_session_id": "sess:parent:sender#live", "target": ME, "subject": "assigned work",
    }));
    room.seal();
    let inbox = room.json(&["inbox", "--tool", ME, "--json"]);
    let items = inbox["data"]["inbox"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        items[0]["ack_command"]
            .as_str()
            .unwrap()
            .contains("say receipt")
    );
    room.json_as(
        "receiver",
        &[
            "say",
            "receipt",
            "--tool",
            ME,
            "--ref",
            "large-sender",
            "--subject",
            "acked",
            "--json",
        ],
    );
    let after = room.json(&["inbox", "--tool", ME, "--json"]);
    assert_eq!(after["data"]["inbox"]["count"], json!(0));
}
