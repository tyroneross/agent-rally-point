// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Seq stress for the Stage 0 append path (docs/design/append-and-return-writes.md
//! §7.2): many concurrent `rally say` PROCESSES against one pre-populated room
//! must leave unique, contiguous canonical seqs, unique event ids, no torn
//! lines, and a derived view that agrees with the log.
//!
//! The room is synthetic and lives under the OS temp dir; HOME is pointed at a
//! scratch dir and session env is scrubbed so no live `.rally/` is touched.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const PRE_EXISTING: i64 = 1_200;
const WORKERS: usize = 8;
const SAYS_PER_WORKER: usize = 5;

fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rally-seq-stress-{nanos}"));
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    root
}

/// Two segments of realistic rows (one archived engagement, one active).
fn seed_room(root: &Path) {
    let log = root.join(".rally/log");
    fs::create_dir_all(&log).unwrap();
    fs::write(root.join(".rally/active-engagement"), "stress-active\n").unwrap();
    let mut old = String::new();
    let mut active = String::new();
    for seq in 1..=PRE_EXISTING {
        let engagement = if seq <= PRE_EXISTING / 2 {
            "stress-old"
        } else {
            "stress-active"
        };
        let line = json!({
            "seq": seq,
            "occurred_at": "2026-09-01T00:00:00Z",
            "event_type": "artifact",
            "engagement": engagement,
            "payload": {
                "schema": "agent-rally.fact.v1",
                "event_id": format!("fact_seed_{seq:06}"),
                "seq": seq,
                "thread_id": format!("room_seed_{seq:06}"),
                "kind": "artifact",
                "tool": format!("seed-{}:x", seq % 40),
                "from_session_id": format!("sess:seed-{}", seq % 40),
                "subject": format!("seed artifact {seq}"),
                "summary": "y".repeat(300 + (seq as usize * 37) % 900),
                "scope": [],
                "evidence": [],
                "created_at": "2026-09-01T00:00:00Z",
            },
        });
        let target = if engagement == "stress-old" {
            &mut old
        } else {
            &mut active
        };
        target.push_str(&line.to_string());
        target.push('\n');
    }
    fs::write(log.join("stress-old.jsonl"), old).unwrap();
    fs::write(log.join("stress-active.jsonl"), active).unwrap();
}

fn rally(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rally"));
    command.current_dir(root).env("HOME", root.join("home"));
    for (key, _) in std::env::vars() {
        if key.starts_with("RALLY_")
            || key.starts_with("CLAUDE")
            || key.starts_with("CODEX")
            || key.starts_with("EASY_TERMINAL")
        {
            command.env_remove(key);
        }
    }
    command
        .env("RALLY_HOOKS", "off")
        .env("RALLY_DAEMON_AUTOSTART", "0")
        .env("RALLY_NO_AUTO_REAP", "1");
    command
}

fn run_ok(root: &Path, args: &[&str]) -> Value {
    let output = rally(root)
        .args(["--timeout-ms", "120000"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "rally {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

#[test]
fn concurrent_say_processes_allocate_unique_contiguous_seqs() {
    let root = temp_root();
    seed_room(&root);
    // Warm: first open replays the seed into facts.db; the first say creates
    // the derived indexes, so the measured says run on the Stage 0 path.
    run_ok(&root, &["room", "--json"]);
    run_ok(
        &root,
        &[
            "say",
            "artifact",
            "--tool",
            "warm:0",
            "--subject",
            "warm",
            "--json",
        ],
    );

    let workers: Vec<_> = (0..WORKERS)
        .map(|worker| {
            let root = root.clone();
            thread::spawn(move || {
                for index in 0..SAYS_PER_WORKER {
                    let subject = format!("stress {worker}-{index}");
                    run_ok(
                        &root,
                        &[
                            "say",
                            "artifact",
                            "--tool",
                            &format!("stress{worker}:x"),
                            "--subject",
                            &subject,
                            "--json",
                        ],
                    );
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    let mut seqs = Vec::new();
    let mut event_ids = BTreeSet::new();
    let mut stress_subjects = 0usize;
    for name in ["stress-old.jsonl", "stress-active.jsonl"] {
        let bytes = fs::read(root.join(".rally/log").join(name)).unwrap();
        assert!(bytes.ends_with(b"\n"), "{name} must end on a full line");
        for raw in bytes
            .split(|byte| *byte == b'\n')
            .filter(|raw| !raw.is_empty())
        {
            let line: Value = serde_json::from_slice(raw)
                .unwrap_or_else(|error| panic!("torn or corrupt line in {name}: {error}"));
            seqs.push(line["seq"].as_i64().unwrap());
            let event_id = line["payload"]["event_id"].as_str().unwrap().to_string();
            assert!(
                event_ids.insert(event_id.clone()),
                "duplicate event id {event_id}"
            );
            if line["payload"]["subject"]
                .as_str()
                .is_some_and(|subject| subject.starts_with("stress "))
            {
                stress_subjects += 1;
            }
        }
    }
    assert_eq!(
        stress_subjects,
        WORKERS * SAYS_PER_WORKER,
        "no drop, no duplicate"
    );
    let total = seqs.len() as i64;
    let unique: BTreeSet<i64> = seqs.iter().copied().collect();
    assert_eq!(unique.len() as i64, total, "duplicate seq allocated");
    let expected: BTreeSet<i64> = (1..=total).collect();
    assert_eq!(unique, expected, "seqs must be exactly 1..={total}");
    assert!(total > PRE_EXISTING + (WORKERS * SAYS_PER_WORKER) as i64);

    // The derived view agrees with the log.
    let room = run_ok(&root, &["room", "--json"]);
    assert_eq!(room["data"]["room"]["max_seq"], json!(total));
    fs::remove_dir_all(&root).ok();
}
