// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Old-reader / new-writer round-trip over the canonical segment.
//!
//! ## The incident these tests encode
//!
//! A room ran two rally versions at once. The newer binary appended a
//! `session.closed` row. `FactKind` has a `#[serde(other)] Unknown` arm, so the
//! older binary deserialised that kind to `Unknown`, whose `as_str()` is
//! `"unknown"` — and `validate_canonical_line` compared the envelope
//! `event_type` against it, so `"session.closed" != "unknown"` graded the row
//! CORRUPT. Because `inspect_active_segment_tail` validates every completed line
//! before any append, that single row blocked every subsequent WRITE by every
//! older binary in the room. The preserved segment was 1160 lines of valid JSON
//! with no seq breaks and no envelope/payload disagreement: nothing was damaged.
//! Version skew was being graded as data damage.
//!
//! ## Why the fixture kind is synthetic
//!
//! The fixture writes `rally.test.future-kind.v0`, which no `FactKind` variant
//! will ever claim. Using a real future kind (`session.closed`) would make this
//! suite pass for the wrong reason the day someone adds that variant — the test
//! would then be exercising a KNOWN kind and would silently stop covering the
//! forward-compatible path. A kind that is unknown by construction cannot rot.
//!
//! `old reader` here is the binary under test, which does not know the fixture
//! kind; `new writer` is the hand-appended row standing in for a newer binary.
//! That is the same asymmetry the incident had, without needing to build two
//! binaries in CI.

#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A kind no `FactKind` variant will ever claim. See the module docs.
const FUTURE_KIND: &str = "rally.test.future-kind.v0";

static NEXT_ROOM: AtomicU64 = AtomicU64::new(1);

struct Room {
    cwd: PathBuf,
    home: PathBuf,
    session_id: String,
}

impl Room {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_ROOM.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cwd =
            std::env::temp_dir().join(format!("rally-fwdcompat-{name}-{pid}-{nanos}-{nonce}"));
        let home =
            std::env::temp_dir().join(format!("rally-fwdcompat-{name}-{pid}-{nanos}-{nonce}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            cwd,
            home,
            session_id: format!("fwdcompat-{name}-{nanos}-{nonce}"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_GLOBAL_INDEX", "0")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID")
            .env("RALLY_SESSION_ID", &self.session_id)
            .env("RALLY_ENGAGEMENT", "seed")
            .args(args)
            .output()
            .unwrap()
    }

    /// Write one fact through the real CLI so the room is created exactly the way
    /// production creates it — no hand-built `.rally` tree that could drift from
    /// what the binary actually produces.
    fn seed(&self, subject: &str) -> Output {
        let output = self.run(&[
            "say",
            "decision",
            "--tool",
            "codex:fwdcompat-test",
            "--subject",
            subject,
            "--json",
        ]);
        assert_success("seed", &output);
        output
    }

    fn segment(&self) -> PathBuf {
        self.cwd.join(".rally/log/seed.jsonl")
    }

    fn segment_lines(&self) -> Vec<String> {
        fs::read_to_string(self.segment())
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    fn max_seq(&self) -> i64 {
        self.segment_lines()
            .iter()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["seq"]
                    .as_i64()
                    .unwrap()
            })
            .max()
            .unwrap_or(0)
    }

    fn append_raw(&self, line: &str) {
        let mut text = fs::read_to_string(self.segment()).unwrap();
        text.push_str(line);
        text.push('\n');
        fs::write(self.segment(), text).unwrap();
    }

    /// A well-formed row of a kind this binary does not know — what a newer
    /// binary's append looks like from here.
    fn append_future_kind_row(&self, event_id: &str) -> i64 {
        let seq = self.max_seq() + 1;
        let line = json!({
            "seq": seq,
            "occurred_at": "2026-08-29T03:40:00Z",
            "event_type": FUTURE_KIND,
            "payload": {
                "created_at": "2026-08-29T03:40:00Z",
                "event_id": event_id,
                "evidence": ["protocol:written_by=a-newer-binary"],
                "kind": FUTURE_KIND,
                "ref": Value::Null,
                "role": Value::Null,
                "schema": "agent-rally.fact.v1",
                "scope": [],
                "seq": seq,
                "severity": Value::Null,
                "status": "closed",
                "subject": "written by a binary newer than this reader",
                "summary": Value::Null,
                "target": Value::Null,
                "thread_id": "fwdcompat-future-thread",
                "tool": "codex:newer-binary",
                "uri": Value::Null,
            },
            "engagement": "seed",
        });
        self.append_raw(&serde_json::to_string(&line).unwrap());
        seq
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed (exit {:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

// ---------------------------------------------------------------------------
// The forward-compatible path: version skew must not look like damage
// ---------------------------------------------------------------------------

/// The read half. Before the fix this exited 1 with
/// `completed canonical segment corruption ... does not match payload kind "unknown"`.
#[test]
fn unknown_kind_does_not_block_reads() {
    let room = Room::new("read");
    room.seed("first fact");
    room.append_future_kind_row("future-row-read-evt");

    let output = room.run(&["room", "--json"]);
    assert_success("rally room over a future-kind row", &output);
    serde_json::from_str::<Value>(&stdout(&output)).expect("rally room emits JSON");
}

/// The write half, and the reason this defect took a whole room down rather than
/// one row: `append_fact_under_lock` validates every completed line before it
/// appends, so one unreadable row wedged every writer.
#[test]
fn unknown_kind_does_not_block_writes() {
    let room = Room::new("write");
    room.seed("first fact");
    room.append_future_kind_row("future-row-write-evt");

    let output = room.run(&[
        "say",
        "decision",
        "--tool",
        "codex:fwdcompat-test",
        "--subject",
        "written after the future-kind row",
        "--json",
    ]);
    assert_success("rally say after a future-kind row", &output);
    let body: Value = serde_json::from_str(&stdout(&output)).expect("rally say emits JSON");
    assert_eq!(body["ok"], json!(true), "say did not commit: {body}");
}

/// Skipping must not mean deleting. The row a newer binary wrote stays on disk
/// byte-for-byte so the binary that CAN read it still sees it — the incident's
/// proposed workaround (`doctor --sweep-corrupt`, archiving 117MB of history)
/// was rejected precisely because it traded history for readability.
#[test]
fn unknown_kind_row_is_preserved_verbatim() {
    let room = Room::new("preserve");
    room.seed("first fact");
    room.append_future_kind_row("future-row-preserve-evt");
    let before = room
        .segment_lines()
        .into_iter()
        .find(|line| line.contains(FUTURE_KIND))
        .expect("fixture row is on disk");

    room.seed("second fact");

    let after = room
        .segment_lines()
        .into_iter()
        .find(|line| line.contains(FUTURE_KIND))
        .expect("future-kind row survives a subsequent append");
    assert_eq!(before, after, "the future-kind row was rewritten");
}

/// "Skipped" in the projection sense: an unreadable kind must not become work.
/// `FactKind::Unknown` is in no claim, handoff, blocker, decision, artifact, or
/// `next` bucket, so the ROW never surfaces — asserted here so a future
/// projection change cannot quietly break it.
///
/// The row's `tool` DOES still reach the agent roster, and that is deliberate.
/// The roster is built from the envelope metadata every `agent-rally.fact.v1`
/// row carries, not from interpreting the kind — a control run confirms a
/// KNOWN-kind fact from a never-entered tool populates `squads` identically, so
/// this is pre-existing kind-independent behavior, not something the
/// forward-compat path introduced. Keeping it is also the useful choice: the
/// peer named in the roster is exactly the binary the operator has to upgrade,
/// and suppressing it would hide that while the warning tells them to look.
#[test]
fn unknown_kind_is_absent_from_work_surfaces() {
    let room = Room::new("surface");
    room.seed("first fact");
    room.append_future_kind_row("future-row-surface-evt");

    for args in [
        vec!["room", "--json"],
        vec!["next", "--tool", "codex:fwdcompat-test", "--json"],
    ] {
        let output = room.run(&args);
        assert_success(&format!("{args:?}"), &output);
        let body = stdout(&output);
        // The row's own identity is what must never appear: an event_id in a
        // work surface means something consumed a fact it cannot interpret.
        assert!(
            !body.contains("future-row-surface-evt"),
            "{args:?} surfaced the unreadable row as work: {body}"
        );
        assert!(
            !body.contains(FUTURE_KIND),
            "{args:?} surfaced the unreadable kind: {body}"
        );
    }

    // The peer signal, on the other hand, must survive — it names who to upgrade.
    let output = room.run(&["room", "--json"]);
    assert!(
        stdout(&output).contains("codex:newer-binary"),
        "the roster dropped the peer that wrote the unreadable row, so nothing \
         tells the operator which binary is ahead"
    );
}

/// A skipped row must be visible to the operator, or a room silently drops data
/// every reader assumes it has. The warning names the kind so the operator can
/// tell which binary to upgrade.
#[test]
fn unknown_kind_emits_a_warning() {
    let room = Room::new("warn");
    room.seed("first fact");
    room.append_future_kind_row("future-row-warn-evt");

    let output = room.run(&["room", "--json"]);
    assert_success("rally room", &output);
    let warning = stderr(&output);
    assert!(
        warning.contains(FUTURE_KIND) && warning.contains("warning"),
        "no forward-compat warning naming {FUTURE_KIND}: {warning}"
    );
}

// ---------------------------------------------------------------------------
// The fail-loud path: genuine structural damage must still stop everything
// ---------------------------------------------------------------------------

/// Unparseable JSON is damage, not skew.
#[test]
fn malformed_json_still_fails_loud() {
    let room = Room::new("badjson");
    room.seed("first fact");
    room.append_raw("{\"seq\": 99, \"event_type\": \"decision\", ");

    let output = room.run(&["room", "--json"]);
    assert!(
        !output.status.success(),
        "malformed JSON was tolerated: {}",
        stdout(&output)
    );
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("corruption"),
        "malformed JSON did not report corruption"
    );
}

/// A non-positive seq is damage: it breaks the ordering the whole fold depends
/// on, and no version of any binary would ever write one.
#[test]
fn broken_seq_still_fails_loud() {
    let room = Room::new("badseq");
    room.seed("first fact");
    let mut line: Value = serde_json::from_str(room.segment_lines().last().unwrap()).unwrap();
    line["seq"] = json!(0);
    line["payload"]["seq"] = json!(0);
    line["payload"]["event_id"] = json!("future-row-badseq-evt");
    room.append_raw(&serde_json::to_string(&line).unwrap());

    let output = room.run(&["room", "--json"]);
    assert!(
        !output.status.success(),
        "a non-positive seq was tolerated: {}",
        stdout(&output)
    );
}

/// The narrow case the fix must NOT swallow: a kind this reader DOES know,
/// disagreeing with its envelope. That is an inconsistent row, not a newer one,
/// and it is the exact comparison the forward-compat arm relaxes — so if the
/// relaxation were written one condition too wide, this is the test that fails.
#[test]
fn known_kind_envelope_mismatch_still_fails_loud() {
    let room = Room::new("mismatch");
    room.seed("first fact");
    let seq = room.max_seq() + 1;
    let line = json!({
        "seq": seq,
        "occurred_at": "2026-08-29T03:40:00Z",
        "event_type": "decision",
        "payload": {
            "created_at": "2026-08-29T03:40:00Z",
            "event_id": "future-row-mismatch-evt",
            "evidence": [],
            // Disagrees with the envelope above, and BOTH spellings are kinds
            // this reader knows.
            "kind": "artifact",
            "ref": Value::Null,
            "role": Value::Null,
            "schema": "agent-rally.fact.v1",
            "scope": [],
            "seq": seq,
            "severity": Value::Null,
            "status": Value::Null,
            "subject": "envelope and payload disagree",
            "summary": Value::Null,
            "target": Value::Null,
            "thread_id": "fwdcompat-mismatch-thread",
            "tool": "codex:fwdcompat-test",
            "uri": Value::Null,
        },
        "engagement": "seed",
    });
    room.append_raw(&serde_json::to_string(&line).unwrap());

    let output = room.run(&["room", "--json"]);
    assert!(
        !output.status.success(),
        "a known-kind envelope/payload mismatch was tolerated: {}",
        stdout(&output)
    );
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("does not match payload kind"),
        "mismatch did not report the kind disagreement"
    );
}

/// An unknown envelope kind whose payload does NOT agree with it is damage, not
/// skew: a newer binary writes both fields from the same value, so a row where
/// they differ was not produced by any binary.
#[test]
fn unknown_kind_disagreeing_with_its_payload_still_fails_loud() {
    let room = Room::new("halfunknown");
    room.seed("first fact");
    let seq = room.max_seq() + 1;
    let line = json!({
        "seq": seq,
        "occurred_at": "2026-08-29T03:40:00Z",
        "event_type": FUTURE_KIND,
        "payload": {
            "created_at": "2026-08-29T03:40:00Z",
            "event_id": "future-row-halfunknown-evt",
            "evidence": [],
            "kind": "decision",
            "ref": Value::Null,
            "role": Value::Null,
            "schema": "agent-rally.fact.v1",
            "scope": [],
            "seq": seq,
            "severity": Value::Null,
            "status": Value::Null,
            "subject": "envelope claims a future kind, payload claims a known one",
            "summary": Value::Null,
            "target": Value::Null,
            "thread_id": "fwdcompat-halfunknown-thread",
            "tool": "codex:fwdcompat-test",
            "uri": Value::Null,
        },
        "engagement": "seed",
    });
    room.append_raw(&serde_json::to_string(&line).unwrap());

    let output = room.run(&["room", "--json"]);
    assert!(
        !output.status.success(),
        "an envelope/payload kind disagreement was tolerated: {}",
        stdout(&output)
    );
}

/// `#[serde(other)]` maps a future payload kind to `Unknown`. The literal
/// envelope kind `unknown` must not make that disagreement disappear after
/// deserialization.
#[test]
fn literal_unknown_envelope_cannot_hide_a_future_payload_kind() {
    let room = Room::new("unknown-envelope");
    room.seed("first fact");
    let seq = room.max_seq() + 1;
    let line = json!({
        "seq": seq,
        "occurred_at": "2026-08-29T03:40:00Z",
        "event_type": "unknown",
        "payload": {
            "created_at": "2026-08-29T03:40:00Z",
            "event_id": "future-row-unknown-envelope-evt",
            "evidence": [],
            "kind": FUTURE_KIND,
            "ref": Value::Null,
            "role": Value::Null,
            "schema": "agent-rally.fact.v1",
            "scope": [],
            "seq": seq,
            "severity": Value::Null,
            "status": Value::Null,
            "subject": "unknown envelope disagrees with future payload kind",
            "summary": Value::Null,
            "target": Value::Null,
            "thread_id": "fwdcompat-unknown-envelope-thread",
            "tool": "codex:fwdcompat-test",
            "uri": Value::Null,
        },
        "engagement": "seed",
    });
    room.append_raw(&serde_json::to_string(&line).unwrap());

    let output = room.run(&["room", "--json"]);
    assert!(
        !output.status.success(),
        "a literal unknown envelope hid a different payload kind: {}",
        stdout(&output)
    );
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("does not match payload kind"),
        "mismatch did not report the literal kind disagreement"
    );
}

// ---------------------------------------------------------------------------
// Writer discipline: the other half of the mixed-version fix
// ---------------------------------------------------------------------------

/// Reader tolerance only protects binaries built after the fix. The floor is
/// what stops a newer binary from writing kinds the room's OTHER binaries still
/// cannot read.
#[test]
fn schema_floor_reports_room_and_binary_generation() {
    let room = Room::new("floor");
    room.seed("first fact");

    let output = room.run(&["doctor", "--schema-floor", "--json"]);
    assert_success("doctor --schema-floor", &output);
    let body: Value = serde_json::from_str(&stdout(&output)).expect("schema-floor emits JSON");
    let report = &body["data"]["doctor"];
    assert_eq!(
        report["room_generation"],
        json!(1),
        "a room with no recorded floor must read as generation 1, not as whatever \
         this binary happens to be: {body}"
    );
    assert_eq!(
        report["raised"],
        json!(false),
        "a read-only mode wrote: {body}"
    );
    assert!(
        !room.cwd.join(".rally/schema-floor.json").exists(),
        "a read-only mode created the floor file"
    );
}

/// The floor file must be a sibling of `.rally/log/`, never a file inside it:
/// `rally watch` polls the segment index under `log/` for its `max_seq`, so a
/// write in that directory self-triggers every watcher in the room.
#[test]
fn schema_floor_apply_records_the_floor_outside_the_log_dir() {
    let room = Room::new("floorapply");
    room.seed("first fact");

    let output = room.run(&["doctor", "--schema-floor", "--apply", "--json"]);
    assert_success("doctor --schema-floor --apply", &output);

    let floor_path = room.cwd.join(".rally/schema-floor.json");
    // Generation 1 == generation 1, so --apply correctly has nothing to raise
    // and must not write. Either outcome is fine; what is NOT fine is the file
    // landing under log/.
    assert!(
        !room.cwd.join(".rally/log/schema-floor.json").exists(),
        "the floor file landed inside log/, where it self-triggers rally watch"
    );
    if floor_path.exists() {
        let recorded: Value =
            serde_json::from_str(&fs::read_to_string(&floor_path).unwrap()).unwrap();
        assert_eq!(recorded["schema"], json!("agent-rally.schema-floor.v1"));
    }
}

/// A room that records a floor NEWER than this binary must still be readable and
/// writable at the kinds this binary does know. Refusing outright would make one
/// upgraded peer lock every other binary out of the room — a worse version of
/// the bug being fixed.
#[test]
fn a_room_floor_above_this_binary_does_not_block_known_kinds() {
    let room = Room::new("aheadfloor");
    room.seed("first fact");
    fs::write(
        room.cwd.join(".rally/schema-floor.json"),
        serde_json::to_string_pretty(&json!({
            "schema": "agent-rally.schema-floor.v1",
            "kind_generation": 99,
            "recorded_at": "2026-08-29T03:40:00Z",
            "recorded_by": "a newer binary",
        }))
        .unwrap(),
    )
    .unwrap();

    let output = room.run(&[
        "say",
        "decision",
        "--tool",
        "codex:fwdcompat-test",
        "--subject",
        "known kind under a newer floor",
        "--json",
    ]);
    assert_success("say under a room floor above this binary", &output);
}

/// Write a floor the room could not otherwise reach.
///
/// Generation 0 is below every shipped kind, so it turns the gate — inert while
/// every kind is generation 1 and every room floors at 1 — into a live refusal.
/// Testing only the inert path would prove the gate compiles, not that it binds.
fn record_floor(room: &Room, generation: u32) {
    fs::write(
        room.cwd.join(".rally/schema-floor.json"),
        serde_json::to_string_pretty(&json!({
            "schema": "agent-rally.schema-floor.v1",
            "kind_generation": generation,
            "recorded_at": "2026-08-29T03:40:00Z",
            "recorded_by": "forward_compat_roundtrip test",
        }))
        .unwrap(),
    )
    .unwrap();
}

/// The writer half of the mixed-version fix, end to end: a kind above the room's
/// recorded floor is refused at the write boundary, and the refusal names both
/// ways out. This is the control that would have stopped the incident at its
/// source rather than after the ledger was already wedged.
#[test]
fn a_kind_above_the_room_floor_is_refused_at_the_cli() {
    let room = Room::new("refuse");
    room.seed("first fact");
    record_floor(&room, 0);

    let output = room.run(&[
        "say",
        "decision",
        "--tool",
        "codex:fwdcompat-test",
        "--subject",
        "above the room floor",
        "--json",
    ]);
    assert!(
        !output.status.success(),
        "a kind above the room floor was written anyway: {}",
        stdout(&output)
    );
    let message = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        message.contains("dual-write") && message.contains("rally doctor --schema-floor --apply"),
        "the refusal does not tell the operator how to proceed: {message}"
    );
}

/// The gate needs a way out, or the first person to add a kind hits a wall with
/// no exit. Raising the floor to this binary's generation unblocks the write.
#[test]
fn raising_the_floor_unblocks_the_refused_write() {
    let room = Room::new("raise");
    room.seed("first fact");
    record_floor(&room, 0);

    let raise = room.run(&["doctor", "--schema-floor", "--apply", "--json"]);
    assert_success("doctor --schema-floor --apply", &raise);
    let body: Value = serde_json::from_str(&stdout(&raise)).unwrap();
    let report = &body["data"]["doctor"];
    assert_eq!(
        report["raised"],
        json!(true),
        "the floor did not move: {body}"
    );
    assert_eq!(report["room_generation"], report["binary_generation"]);

    let output = room.run(&[
        "say",
        "decision",
        "--tool",
        "codex:fwdcompat-test",
        "--subject",
        "written after the floor was raised",
        "--json",
    ]);
    assert_success("say after raising the floor", &output);
}

/// A malformed floor file must degrade to the LOWEST floor, not the highest. The
/// tolerant direction here is the strict one: falling back to this binary's own
/// generation would let a damaged sidecar hand the newest binary permission to
/// write past every older reader.
#[test]
fn a_malformed_floor_file_degrades_to_generation_one() {
    let room = Room::new("badfloor");
    room.seed("first fact");
    fs::write(room.cwd.join(".rally/schema-floor.json"), "{ not json").unwrap();

    let output = room.run(&["doctor", "--schema-floor", "--json"]);
    assert_success("doctor --schema-floor over a malformed floor file", &output);
    let body: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        body["data"]["doctor"]["room_generation"],
        json!(1),
        "a malformed floor file must read as generation 1: {body}"
    );
}
