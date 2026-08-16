// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-squad staleness surfacing and freshness-ranked handoff targets
//! (operator note 2026-08-15, task 842733db).
//!
//! # What was wrong
//!
//! `room --json` listed every squad with `last_seen_ts`, but nothing said which
//! ones were current: this repo's room showed ~380 squads, most long dead,
//! indistinguishable at a glance. `next` offered no ranking when a sender chose
//! a handoff target, and `say --target <ghost>` said nothing.
//!
//! # The contract these tests pin (charter: advise / rank, never gate)
//!
//! * every squad row carries `age_secs`, `window_secs`, `freshness`;
//! * the `room` human line reports `squads=N fresh=X stale=Y`;
//! * `next.peer_targets` ranks visible peers freshest-first, self excluded;
//! * `say --target <stale peer>` commits, delivers, AND attaches a
//!   `stale-target` warning naming fresher peers; a fresh target gets none.
//!
//! Assertions are made on CLI output only, so a refactor that stops wiring the
//! fields through the real command path fails here.

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cadence 300 s × 1 miss + 60 s grace → 360 s window. A 3 600 s-old
/// heartbeat is unambiguously stale; a wall-clock-now one is fresh.
const CADENCE_SECS: &str = "300";
const STALE_AGE_SECS: i64 = 3_600;
const WATCHDOG_MS: &str = "30000";

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
        let cwd = std::env::temp_dir().join(format!("sqf-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("sqf-{name}-{nanos}-home"));
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

    fn text(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cmd {args:?} failed\nstderr: {}\nstdout: {}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn say_ok(&self, args: &[&str]) -> Value {
        let mut full = vec!["say"];
        full.extend_from_slice(args);
        full.push("--json");
        let v = self.json(&full);
        assert_eq!(v["ok"], Value::Bool(true), "say {args:?}: {v}");
        v
    }

    fn segments(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.cwd.join(".rally").join("log")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect()
    }

    /// Rewrite `payload.created_at` for facts selected by `select`, dating
    /// them `age_secs` in the past. The projection is pure over the fact
    /// slice, so this is how a freshness verdict is made deterministic
    /// without sleeping through a real window.
    fn backdate(&self, age_secs: i64, select: impl Fn(&Value) -> bool) {
        let stamp = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
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

    /// Drop the derived SQLite projection so the next read rebuilds from the
    /// (edited) segments — the product's own recovery path.
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

/// Three peers: `ghost` (every fact backdated an hour → stale), `live-a` and
/// `live-b` (wall-clock now → fresh), plus the caller `me`.
fn seed(room: &Room) {
    for tool in ["ghost", "live-a", "live-b", "me"] {
        room.say_ok(&["risk", "--tool", tool, "--subject", "hello"]);
    }
    room.backdate(STALE_AGE_SECS, |f| f["tool"] == "ghost");
    room.reimport_segments();
}

fn squads(room: &Room) -> Vec<Value> {
    room.json(&["room", "--json"])["data"]["room"]["squads"]
        .as_array()
        .cloned()
        .expect("room.squads")
}

fn squad<'a>(rows: &'a [Value], tool: &str) -> &'a Value {
    rows.iter()
        .find(|s| s["tool"] == tool)
        .unwrap_or_else(|| panic!("no squad row for {tool}: {rows:?}"))
}

#[test]
fn room_rows_carry_freshness_and_human_line_counts_fresh_vs_stale() {
    let room = Room::new("room");
    seed(&room);

    let rows = squads(&room);
    let ghost = squad(&rows, "ghost");
    assert_eq!(ghost["freshness"], "stale", "{ghost}");
    assert_eq!(ghost["window_secs"], 360, "{ghost}");
    assert!(
        ghost["age_secs"].as_i64().is_some_and(|a| a > 360),
        "stale age must exceed the window: {ghost}"
    );
    for live in ["live-a", "live-b", "me"] {
        let row = squad(&rows, live);
        assert_eq!(row["freshness"], "fresh", "{row}");
        assert!(
            row["age_secs"]
                .as_i64()
                .is_some_and(|a| (0..=360).contains(&a)),
            "{row}"
        );
    }
    // Stale by heartbeat is still VISIBLE (four-signal drop needs unanimity);
    // the row says stale instead of the room hiding it.
    assert_eq!(rows.len(), 4, "{rows:?}");

    let line = room.text(&["room"]);
    assert!(
        line.contains("squads=4 fresh=3 stale=1"),
        "human line must tally freshness; got: {line}"
    );
}

#[test]
fn next_ranks_peer_targets_freshest_first_and_excludes_self() {
    let room = Room::new("next");
    seed(&room);

    let next = room.json(&["next", "--tool", "me", "--json"]);
    let targets = &next["data"]["next"]["peer_targets"];
    assert_eq!(targets["fresh"], 2, "{targets}");
    assert_eq!(targets["stale"], 1, "{targets}");
    assert_eq!(targets["truncated"], 0, "{targets}");
    let ranked: Vec<&str> = targets["ranked"]
        .as_array()
        .expect("ranked list")
        .iter()
        .map(|p| p["tool"].as_str().unwrap())
        .collect();
    assert_eq!(ranked.len(), 3, "{ranked:?}");
    assert!(
        !ranked.contains(&"me"),
        "self is never a target: {ranked:?}"
    );
    assert_eq!(
        ranked[2], "ghost",
        "the stale peer ranks last, after every fresh peer: {ranked:?}"
    );
    assert!(
        ranked[..2].iter().all(|t| *t == "live-a" || *t == "live-b"),
        "fresh peers first: {ranked:?}"
    );
    assert_eq!(targets["ranked"][2]["freshness"], "stale");
    assert_eq!(targets["ranked"][0]["freshness"], "fresh");

    let line = room.text(&["next", "--tool", "me"]);
    assert!(
        line.contains("peers_fresh=2 peers_stale=1"),
        "next human line must tally peer freshness; got: {line}"
    );
}

#[test]
fn say_to_stale_target_warns_and_still_delivers_but_fresh_target_does_not() {
    let room = Room::new("say");
    seed(&room);

    // Stale target: committed, targeted as asked, warned.
    let stale = room.say_ok(&[
        "handoff",
        "--tool",
        "me",
        "--target",
        "ghost",
        "--subject",
        "please pick this up",
    ]);
    assert_eq!(stale["data"]["say"]["committed"], true, "{stale}");
    assert_eq!(
        stale["data"]["say"]["fact"]["target"], "ghost",
        "never re-targeted: {stale}"
    );
    let warnings = stale["data"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let stale_warning = warnings
        .iter()
        .find(|w| w["code"] == "stale-target")
        .unwrap_or_else(|| panic!("expected a stale-target warning; got {warnings:?}"));
    let msg = stale_warning["message"].as_str().unwrap();
    assert!(msg.contains("target ghost was last seen"), "{msg}");
    assert!(msg.contains("fresher peers:"), "{msg}");
    assert!(msg.contains("live-a") && msg.contains("live-b"), "{msg}");
    assert!(
        !msg.contains("me ("),
        "sender is not its own alternative: {msg}"
    );
    assert!(msg.contains("Delivered anyway"), "{msg}");

    // The handoff really is on the ledger, addressed to the ghost.
    let open = room.json(&["room", "--json"])["data"]["room"]["open_handoffs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        open.iter()
            .any(|h| h["target"] == "ghost" && h["tool"] == "me"),
        "stale-target handoff must still be delivered: {open:?}"
    );

    // Fresh target: no stale-target warning.
    let fresh = room.say_ok(&[
        "handoff",
        "--tool",
        "me",
        "--target",
        "live-a",
        "--subject",
        "please pick this up",
    ]);
    let warnings = fresh["data"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        warnings.iter().all(|w| w["code"] != "stale-target"),
        "fresh target must not warn: {warnings:?}"
    );

    // Broadcast: no stale-target warning either.
    let all = room.say_ok(&[
        "artifact",
        "--tool",
        "me",
        "--target",
        "all",
        "--subject",
        "fyi",
    ]);
    let warnings = all["data"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        warnings.iter().all(|w| w["code"] != "stale-target"),
        "broadcast must not warn: {warnings:?}"
    );
}
