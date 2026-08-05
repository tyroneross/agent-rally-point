// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Direct-vs-routed behaviour parity for the four `#[serde(skip)]` snapshot
//! projections (design audit D1 and D6).
//!
//! # What was wrong
//!
//! `RoomSnapshot::{content_max_seq, last_activity_ts, pending_wakes,
//! stale_authors}` are `#[serde(skip)]` so they stay out of the public room
//! JSON. The daemon serialized the snapshot with the same impl, so routing also
//! dropped them — and three behaviours changed depending on whether `rallyd`
//! happened to be running:
//!
//! 1. **Ranking.** `apply_budget` demotes items whose author is in
//!    `stale_authors`. Empty over the wire means nothing is ever demoted, so the
//!    SAME ledger composes into a different room.
//! 2. **Read checkpoints.** `next` passes `content_max_seq` to
//!    `maybe_append_read_checkpoint`, which coalesces at `read_seq <= last`. A
//!    routed caller passed 0, so no checkpoint was ever written and its read
//!    position never advanced.
//! 3. **Wake coalescing.** `append_next_wake_intent` looks for a matching entry
//!    in `pending_wakes` before appending. Empty means the guard never matches,
//!    so a routed caller appends a DUPLICATE wake intent on every poll.
//! 4. **Global status age.** `status_global` reads `last_activity_ts` from the
//!    snapshot. Empty over the wire makes an active room look timestamp-less.
//!
//! # Why these tests are shaped this way
//!
//! Each assertion is made on **observable ledger or CLI output**, never on the
//! wire helpers directly. A test that called `store::snapshot_to_wire_value`
//! would stay green if the daemon stopped calling it — the exact
//! control-not-on-the-path failure the root-cause register names as its closing
//! question. Here the only way to satisfy the assertions is for the real
//! `rallyd` reply path to carry the fields.
//!
//! # What these tests do NOT cover
//!
//! * Parity is asserted for ONE stale author and ONE live author. It does not
//!   establish that the relevance model itself is right — only that both modes
//!   run the same one.
//! * Daemon/client build compatibility is a separate transport contract. These
//!   tests require the daemon they start to exercise the current binary.

#![cfg(unix)]
#![allow(clippy::zombie_processes)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Liveness window used by every test here: cadence 300 s × 1 miss + 0 grace.
/// An author whose newest fact is older than this is `stale_authors` material.
const CADENCE_SECS: &str = "300";

/// Watchdog budget for every command here, raised from the 3 s default.
///
/// NOT a masking of the cost problem the register tracks as RC-058: a first
/// `say` into a fresh room measures ~1.7 s on a quiet machine, which leaves no
/// headroom under a loaded test harness, and a suite that fails on machine load
/// certifies nothing. The cost itself is a separate open entry.
const WATCHDOG_MS: &str = "30000";

/// How far back the stale author's only fact is dated. Comfortably past the
/// 300 s window, so a slow test run cannot drift it back inside.
const STALE_AGE_SECS: i64 = 3_600;

/// How far back the LIVE author's risk is dated. Older than the stale author's
/// risk on purpose: without the demotion the stale author's item ranks HIGHER
/// (it is newer), and with it the order flips. That inversion is what makes the
/// omission set discriminating rather than incidental.
const LIVE_RISK_AGE_SECS: i64 = 5_400;

struct Room {
    cwd: PathBuf,
    home: PathBuf,
}

impl Room {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("swi-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("swi-{name}-{nanos}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".rally")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_GLOBAL_INDEX", "1")
            .env("RALLY_DEFAULT_CADENCE_SECS", CADENCE_SECS)
            .env("RALLY_MISS_MULTIPLIER", "1")
            .env("RALLY_HOOK_TIMEOUT_MS", WATCHDOG_MS)
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?} did not emit JSON ({e})\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            )
        })
    }

    fn say_risk(&self, tool: &str, subject: &str) -> String {
        let v = self.json(&[
            "say",
            "risk",
            "--tool",
            tool,
            "--subject",
            subject,
            "--json",
        ]);
        assert_eq!(v["ok"], Value::Bool(true), "say risk: {v}");
        v["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn log_dir(&self) -> PathBuf {
        self.cwd.join(".rally").join("log")
    }

    fn segments(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.log_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect()
    }

    /// Every fact currently in the room's live segments.
    fn ledger_facts(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for seg in self.segments() {
            let Ok(text) = fs::read_to_string(&seg) else {
                continue;
            };
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && let Some(payload) = v.get("payload")
                {
                    out.push(payload.clone());
                }
            }
        }
        out
    }

    fn count_kind_by_tool(&self, kind: &str, tool: &str) -> usize {
        self.ledger_facts()
            .iter()
            .filter(|f| f["kind"] == kind)
            .filter(|f| f["tool"] == tool || f["target"] == tool)
            .count()
    }

    /// Rewrite `payload.created_at` for the facts selected by `select`, dating
    /// them `age_secs` in the past.
    ///
    /// Backdating is how a liveness verdict is made deterministic without
    /// sleeping through a real window: `heartbeat_age` is the age of a tool's
    /// highest-seq fact, and the projection is pure over the fact slice.
    fn backdate(&self, age_secs: i64, select: impl Fn(&Value) -> bool) {
        let stamp = rfc3339_secs_ago(age_secs);
        for seg in self.segments() {
            let Ok(text) = fs::read_to_string(&seg) else {
                continue;
            };
            let mut rewritten = String::with_capacity(text.len());
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let mut v: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => {
                        rewritten.push_str(line);
                        rewritten.push('\n');
                        continue;
                    }
                };
                if select(&v["payload"]) {
                    v["payload"]["created_at"] = Value::String(stamp.clone());
                }
                rewritten.push_str(&serde_json::to_string(&v).unwrap());
                rewritten.push('\n');
            }
            fs::write(&seg, rewritten).unwrap();
        }
    }

    /// Drop the SQLite projection and its caches so the next read rebuilds them
    /// from the segments.
    ///
    /// `DirectRoomStore::facts` reconciles segments into `facts.db` by event id
    /// and skips ids it already holds, so editing a segment line in place does
    /// NOT change what a read returns. Removing the derived files is the
    /// product's own recovery path — the same one `facts_from_db_with_query_recovery`
    /// takes on a malformed database.
    fn reimport_segments(&self) {
        let rally = self.cwd.join(".rally");
        for name in [
            "facts.db",
            "facts.db-wal",
            "facts.db-shm",
            ".reconcile-cache.json",
            "snapshot.cache.json",
        ] {
            fs::remove_file(rally.join(name)).ok();
        }
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn rfc3339_secs_ago(secs: i64) -> String {
    let now = chrono::Utc::now() - chrono::Duration::seconds(secs);
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A live `rally daemon serve` bound to a room. SIGTERM + reap on Drop.
struct Daemon {
    child: Child,
    addr: PathBuf,
    stopped: bool,
}

impl Daemon {
    /// Starts the current test binary's daemon or fails the test with its log.
    /// A green routed-path test must prove that the daemon actually ran.
    fn start(room: &Room) -> Result<Self, String> {
        let log_path = room.cwd.join(".rally").join("rallyd-serve.log");
        let log = fs::File::create(&log_path)
            .map_err(|e| format!("create {}: {e}", log_path.display()))?;
        let child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_GLOBAL_INDEX", "1")
            .env("RALLY_DEFAULT_CADENCE_SECS", CADENCE_SECS)
            .env("RALLY_MISS_MULTIPLIER", "1")
            .env("RALLY_HOOK_TIMEOUT_MS", WATCHDOG_MS)
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| format!("spawn rallyd: {e}"))?;
        let mut d = Daemon {
            child,
            addr: room.cwd.join(".rally").join("rallyd.sock.addr"),
            stopped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if d.addr.exists() && room.run(&["daemon", "status", "--json"]).status.success() {
                return Ok(d);
            }
            if let Ok(Some(status)) = d.child.try_wait() {
                d.stopped = true;
                let log = fs::read_to_string(&log_path).unwrap_or_default();
                return Err(format!(
                    "rallyd exited before becoming ready ({status}); log:\n{log}"
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
        d.stop();
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        Err(format!(
            "rallyd did not become ready within 20s; log:\n{log}"
        ))
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // SAFETY: `kill(2)` on our own spawned child.
        unsafe {
            kill(self.child.id() as i32, SIGTERM);
        }
        let _ = self.child.wait();
        // The socket pointer must be gone before a later command is expected to
        // take the DIRECT path; otherwise "direct" is a claim, not a fact.
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.addr.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        fs::remove_file(&self.addr).ok();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Seed a room with one stale-authored risk and one live-authored risk.
///
/// Returns `(stale_risk_id, live_risk_id)`.
///
/// No command runs after `reimport_segments`: the first READ rebuilds the
/// SQLite projection from the segments, and only `live-peer`'s presence fact is
/// left at wall-clock now, which is what makes it the live author.
fn seed_two_authors(room: &Room) -> (String, String) {
    // Long subjects so the two risks dominate the budget and a tight ceiling
    // has to choose between them.
    let filler = "x".repeat(220);
    let stale_id = room.say_risk("stale-peer", &format!("stale author risk {filler}"));
    let live_id = room.say_risk("live-peer", &format!("live author risk {filler}"));
    // live-peer's heartbeat is the age of its HIGHEST-SEQ fact, so it needs one
    // that outlives the backdating below. A short artifact costs a few dozen
    // bytes of budget and keeps the author demonstrably live.
    let v = room.json(&[
        "say",
        "artifact",
        "--tool",
        "live-peer",
        "--subject",
        "still here",
        "--json",
    ]);
    assert_eq!(v["ok"], Value::Bool(true), "live-peer heartbeat: {v}");

    // Everything stale-peer ever wrote, including its presence: its newest fact
    // IS its heartbeat.
    room.backdate(STALE_AGE_SECS, |f| f["tool"] == "stale-peer");
    // live-peer's RISK only. Its heartbeat artifact stays at now, so the author
    // is live while its item is the older of the two.
    let live_risk = live_id.clone();
    room.backdate(LIVE_RISK_AGE_SECS, move |f| {
        f["event_id"] == live_risk.as_str()
    });
    room.reimport_segments();

    (stale_id, live_id)
}

/// Seed a room with two ordinary peers and no timestamp surgery.
///
/// The checkpoint and wake tests do not need a staleness verdict, so they do not
/// pay for the SQLite reimport.
fn seed_plain(room: &Room) {
    room.say_risk("peer-one", "first risk");
    room.say_risk("peer-two", "second risk");
}

fn latest_fact_timestamp(room: &Room) -> String {
    room.ledger_facts()
        .into_iter()
        .max_by_key(|fact| fact["seq"].as_i64().unwrap_or_default())
        .and_then(|fact| fact["created_at"].as_str().map(str::to_string))
        .expect("seeded room must contain a timestamped fact")
}

fn status_last_activity(room: &Room) -> String {
    let status = room.json(&["status", "--global", "--json"]);
    let canonical = fs::canonicalize(&room.cwd).unwrap_or_else(|_| room.cwd.clone());
    status["data"]["status"]["repos"]
        .as_array()
        .and_then(|repos| {
            repos.iter().find(|repo| {
                repo["repo"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == canonical)
            })
        })
        .and_then(|repo| repo["last_activity_ts"].as_str())
        .unwrap_or_else(|| panic!("global status omitted last_activity_ts: {status}"))
        .to_string()
}

fn assert_no_wire_internals(value: &Value, surface: &str) {
    let json = serde_json::to_string(value).expect("serialize public JSON");
    assert!(
        !json.contains("\"__internals\""),
        "{surface} leaked the daemon-only __internals side-channel: {json}"
    );
}

/// Assert the seeded room is actually in the state the D1 assertion needs.
///
/// Written because the first version of this file passed WITHOUT it, and for the
/// wrong reason: the backdating never reached the read path, both risks carried
/// the same timestamp, and the omission was decided by the seq tie-break — which
/// is identical in both modes, so the parity assertion held while proving
/// nothing. The premise is now checked rather than assumed.
fn assert_ranking_premise(room: &Room, stale_id: &str, live_id: &str) {
    let v = room.json(&["room", "--json", "--budget-bytes", "1000000"]);
    let risks = v["data"]["room"]["current_risks"].as_array().cloned();
    let risks = risks.expect("room must expose current_risks");
    let at = |id: &str| -> String {
        risks
            .iter()
            .find(|f| f["event_id"] == id)
            .and_then(|f| f["created_at"].as_str())
            .unwrap_or_else(|| panic!("risk {id} missing from the room: {v}"))
            .to_string()
    };
    let stale_at = at(stale_id);
    let live_at = at(live_id);
    assert!(
        stale_at > live_at,
        "premise: the STALE author's risk must be the NEWER of the two, so that \
         recency alone would rank it first and only the stale-author demotion can \
         reverse that. stale={stale_at} live={live_at} — backdating did not reach \
         the read path."
    );

    let squads = v["data"]["room"]["squads"].as_array().cloned().unwrap();
    let status = |tool: &str| -> String {
        squads
            .iter()
            .find(|s| s["tool"] == tool)
            .and_then(|s| s["status"].as_str())
            .unwrap_or("missing")
            .to_string()
    };
    assert_eq!(
        status("live-peer"),
        "active",
        "premise: live-peer must be inside the liveness window"
    );
    assert_ne!(
        status("stale-peer"),
        "active",
        "premise: stale-peer must be outside the liveness window"
    );
}

/// The ordered event ids emitted in `current_risks`, plus whatever the
/// composition block says was omitted from that bucket.
fn risk_composition(room: &Room, budget: usize) -> (Vec<String>, Vec<String>) {
    let budget = budget.to_string();
    let v = room.json(&["room", "--json", "--budget-bytes", &budget]);
    assert_eq!(v["ok"], Value::Bool(true), "room: {v}");
    let emitted = v["data"]["room"]["current_risks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["event_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let omitted = v["data"]["room"]["composition"]["buckets"]["current_risks"]["omitted_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (emitted, omitted)
}

/// D1 — the same ledger must compose into the same room in both modes.
///
/// The room is seeded ONCE and read twice: routed first (with the daemon up),
/// then direct (after it is down and its socket pointer is gone). Same segment
/// bytes, same event ids, so the comparison is exact rather than structural.
///
/// A budget SWEEP rather than one number: the discriminating ceiling depends on
/// the never-cut reserve, which is a function of the room rather than of this
/// test. The sweep also carries its own vacuity check — at least one budget must
/// have forced a choice between the two risks, or the parity assertion proved
/// nothing.
#[test]
fn direct_and_routed_compose_the_same_room() {
    let room = Room::new("compose");
    let (stale_id, live_id) = seed_two_authors(&room);
    assert_ranking_premise(&room, &stale_id, &live_id);

    let budgets: Vec<usize> = (2..=14).map(|n| n * 250).collect();

    let mut daemon = Daemon::start(&room)
        .unwrap_or_else(|e| panic!("routed composition control requires rallyd: {e}"));
    let routed: Vec<_> = budgets
        .iter()
        .map(|b| risk_composition(&room, *b))
        .collect();
    daemon.stop();
    drop(daemon);

    let direct: Vec<_> = budgets
        .iter()
        .map(|b| risk_composition(&room, *b))
        .collect();

    for (i, budget) in budgets.iter().enumerate() {
        assert_eq!(
            direct[i], routed[i],
            "budget {budget}: direct and routed composed the SAME ledger differently.\n\
             direct  emitted={:?} omitted={:?}\n\
             routed  emitted={:?} omitted={:?}\n\
             stale-peer risk={stale_id} live-peer risk={live_id}",
            direct[i].0, direct[i].1, routed[i].0, routed[i].1,
        );
    }

    // Vacuity check: parity across budgets that all emit everything would be
    // satisfied by any implementation, including the broken one.
    let discriminating = direct
        .iter()
        .any(|(emitted, omitted)| emitted.len() == 1 && omitted.len() == 1);
    assert!(
        discriminating,
        "no budget in the sweep forced a choice between the two risks, so the \
         parity assertion above is vacuous. direct results: {direct:?}"
    );

    // And the choice must be the RELEVANCE one: the stale author's item is the
    // one dropped, even though it is the NEWER of the two. Without
    // `stale_authors` the recency spine alone would keep it and drop the other.
    let (_, omitted) = direct
        .iter()
        .find(|(emitted, omitted)| emitted.len() == 1 && omitted.len() == 1)
        .unwrap();
    assert_eq!(
        omitted[0], stale_id,
        "the stale author's risk should be the one demoted out of the budget; \
         dropping the live author's older item instead means the demotion did \
         not run"
    );
}

/// D6, first consequence — `next` must record a read checkpoint in both modes.
///
/// `maybe_append_read_checkpoint` coalesces when `read_seq <= last_checkpoint`.
/// A routed caller whose `content_max_seq` arrived as 0 therefore wrote NOTHING,
/// every time, and its read position never moved.
#[test]
fn routed_next_advances_the_read_checkpoint() {
    let direct_room = Room::new("ckpt-direct");
    seed_plain(&direct_room);
    let v = direct_room.json(&["next", "--tool", "reader-peer", "--json"]);
    assert_eq!(v["ok"], Value::Bool(true), "direct next: {v}");
    let direct_checkpoints = direct_room.count_kind_by_tool("read", "reader-peer");
    assert!(
        direct_checkpoints >= 1,
        "premise broken: direct `next` did not write a read checkpoint, so the \
         routed comparison below would be meaningless"
    );

    let routed_room = Room::new("ckpt-routed");
    seed_plain(&routed_room);
    let mut daemon = Daemon::start(&routed_room)
        .unwrap_or_else(|e| panic!("routed checkpoint control requires rallyd: {e}"));
    let v = routed_room.json(&["next", "--tool", "reader-peer", "--json"]);
    assert_eq!(v["ok"], Value::Bool(true), "routed next: {v}");
    let routed_checkpoints = routed_room.count_kind_by_tool("read", "reader-peer");
    daemon.stop();

    assert_eq!(
        routed_checkpoints, direct_checkpoints,
        "routed `next` wrote {routed_checkpoints} read checkpoints, direct wrote \
         {direct_checkpoints}. A routed caller that records no checkpoint re-reads \
         the same facts forever."
    );
}

/// D6, second consequence — repeated `next` must not stack duplicate wake
/// intents in routed mode.
///
/// `append_next_wake_intent` dedupes against `snapshot.pending_wakes`. Empty over
/// the wire means the guard never matched, so every poll appended another
/// intent for the same target.
#[test]
fn routed_next_coalesces_wake_intents() {
    fn poll_twice(room: &Room) -> usize {
        // A handoff addressed to the poller is actionable work, which is what
        // makes `next` mint a wake intent at all.
        let v = room.json(&[
            "say",
            "handoff",
            "--tool",
            "peer-one",
            "--to",
            "waker-peer",
            "--subject",
            "please pick this up",
            "--json",
        ]);
        assert_eq!(v["ok"], Value::Bool(true), "handoff: {v}");
        for _ in 0..2 {
            let v = room.json(&["next", "--tool", "waker-peer", "--json"]);
            assert_eq!(v["ok"], Value::Bool(true), "next: {v}");
        }
        room.count_kind_by_tool("wake", "waker-peer")
    }

    let direct_room = Room::new("wake-direct");
    seed_plain(&direct_room);
    let direct_wakes = poll_twice(&direct_room);
    assert_eq!(
        direct_wakes, 1,
        "premise: two direct polls must coalesce to exactly one wake intent"
    );

    let routed_room = Room::new("wake-routed");
    seed_plain(&routed_room);
    let mut daemon = Daemon::start(&routed_room)
        .unwrap_or_else(|e| panic!("routed wake control requires rallyd: {e}"));
    let routed_wakes = poll_twice(&routed_room);
    daemon.stop();

    assert_eq!(
        routed_wakes, direct_wakes,
        "routed polling produced {routed_wakes} wake intents where direct \
         produced {direct_wakes}; the coalescing guard reads `pending_wakes`, \
         which routing was dropping"
    );
    assert_eq!(
        routed_wakes, 1,
        "two routed polls must coalesce to exactly one wake intent"
    );
}

/// `status --global` consumes `last_activity_ts`; both store modes must report
/// the timestamp of the ledger's highest-seq fact rather than a default null.
#[test]
fn direct_and_routed_status_report_last_activity() {
    let room = Room::new("last-activity");
    seed_plain(&room);
    let expected = latest_fact_timestamp(&room);

    let direct = status_last_activity(&room);
    assert_eq!(direct, expected, "direct status used the wrong room age");

    let mut daemon = Daemon::start(&room)
        .unwrap_or_else(|e| panic!("routed status control requires rallyd: {e}"));
    let routed = status_last_activity(&room);
    daemon.stop();
    assert_eq!(routed, expected, "routed status dropped last_activity_ts");
}

/// The wire side-channel must never become part of the public room response.
#[test]
fn wire_internals_are_absent_from_public_room_json() {
    let room = Room::new("public-json");
    seed_plain(&room);

    let direct = room.json(&["room", "--json"]);
    assert_no_wire_internals(&direct, "direct room JSON");

    let mut daemon = Daemon::start(&room)
        .unwrap_or_else(|e| panic!("routed public-JSON control requires rallyd: {e}"));
    let routed = room.json(&["room", "--json"]);
    daemon.stop();
    assert_no_wire_internals(&routed, "routed room JSON");
}

/// The ADJACENT move — a FIFTH skipped field, added later, silently dropped
/// again.
///
/// The three tests above prove the four fields that exist today survive routing.
/// None of them would notice a new `#[serde(skip)]` field, and that is exactly
/// how this defect class arrived: a serialization annotation chosen for the
/// public schema quietly decided a wire question too. So the invariant is
/// asserted structurally over the source rather than behaviourally over one
/// snapshot.
///
/// Reading the source is deliberate. The alternative — enumerating skipped
/// fields at runtime — cannot work: a skipped field is by definition absent from
/// the serialized form, so there is nothing to reflect over.
///
#[test]
fn every_skipped_snapshot_field_rides_the_wire_side_channel() {
    let store_rs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("store.rs");
    let src = fs::read_to_string(&store_rs).expect("read store.rs");

    let body = |decl: &str| -> String {
        let start = src
            .find(decl)
            .unwrap_or_else(|| panic!("{decl} not found in store.rs"));
        let rest = &src[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("unterminated {decl}"));
        rest[..end].to_string()
    };

    let snapshot_body = body("struct RoomSnapshot {");
    let internals_body = body("struct SnapshotInternals {");
    let impl_body = body("impl RoomSnapshot {");

    let mut skipped: Vec<String> = Vec::new();
    let mut pending = false;
    for line in snapshot_body.lines() {
        let trimmed = line.trim();
        if trimmed == "#[serde(skip)]" {
            pending = true;
            continue;
        }
        if pending && let Some(rest) = trimmed.strip_prefix("pub(crate) ") {
            let name = rest.split(':').next().unwrap_or("").trim();
            skipped.push(name.to_string());
            pending = false;
        }
    }

    assert!(
        !skipped.is_empty(),
        "premise: RoomSnapshot should still have skipped fields; if it genuinely \
         has none, delete this test rather than letting it pass vacuously"
    );

    for field in &skipped {
        assert!(
            internals_body.contains(&format!("pub(crate) {field}:")),
            "RoomSnapshot.{field} is #[serde(skip)] but is not carried by \
             SnapshotInternals, so it arrives empty in routed mode. Either add it \
             to SnapshotInternals (and to internals()/restore_internals()), or \
             state in its doc comment why an empty value over the daemon wire is \
             correct."
        );
        assert!(
            impl_body.contains(&format!("{field}: self.{field}")),
            "RoomSnapshot.{field} is not lifted into SnapshotInternals"
        );
        assert!(
            impl_body.contains(&format!("self.{field} = internals.{field}")),
            "SnapshotInternals.{field} is not restored into RoomSnapshot"
        );
    }
}
