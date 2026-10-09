// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Stage 0 append-path proofs and read-relevance probes
//! (docs/design/append-and-return-writes.md §4 Stage 0, §7).
//!
//! Every test builds a disposable synthetic room under the OS temp dir; none
//! of them opens a live `.rally/`.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::store::history_probe;

pub(crate) struct SyntheticRoom {
    pub(crate) root: PathBuf,
}

impl Drop for SyntheticRoom {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn unique_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("rally-s0-{label}-{nanos}"));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    root
}

/// Small deterministic PRNG (xorshift64*) so the generator needs no new
/// dependency and produces the same room on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub(crate) const ACTIVE_ENGAGEMENT: &str = "s0-active";

fn ts(secs_ago: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build a room with `events` canonical rows spread over `segments` files
/// (the last one is the active engagement and holds ~40% of the rows), with
/// the same kind mix and line-size spread as `scripts/bench_append_path.py`:
/// hundreds of long-departed tools, presence, claims (mostly released),
/// handoffs (mostly resolved), decisions, risks, artifacts.
pub(crate) fn synthetic_room(label: &str, events: usize, segments: usize) -> SyntheticRoom {
    let root = unique_root(label);
    let rally = root.join(".rally");
    let log = rally.join("log");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        rally.join("active-engagement"),
        format!("{ACTIVE_ENGAGEMENT}\n"),
    )
    .unwrap();
    let mut rng = Rng(0x5eed_1009_u64);
    let tools: Vec<String> = (0..120)
        .map(|i| format!("old-agent-{i:03}:{:08x}", rng.below(1 << 32)))
        .collect();
    let active_rows = events * 2 / 5;
    let old_rows = events - active_rows;
    let per_old = (old_rows / segments.saturating_sub(1).max(1)).max(1);
    let mut open_claims: Vec<Value> = Vec::new();
    let mut open_handoffs: Vec<Value> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let horizon = 60 * 24 * 3600_i64;
    for i in 0..events {
        let engagement = if i < old_rows {
            format!(
                "s0-old-{:02}",
                (i / per_old).min(segments.saturating_sub(2))
            )
        } else {
            ACTIVE_ENGAGEMENT.to_string()
        };
        let seq = i as i64 + 1;
        let age = horizon - (horizon * i as i64 / events as i64) + 7 * 24 * 3600;
        let tool = tools[rng.below(tools.len() as u64) as usize].clone();
        let created = ts(age);
        let event_id = format!("fact_{:04x}_{:016x}", rng.below(1 << 16), rng.next());
        let mut payload = json!({
            "created_at": created,
            "event_id": event_id,
            "evidence": [],
            "from_session_id": format!("sess:{tool}"),
            "kind": "artifact",
            "ref": null,
            "role": null,
            "schema": crate::FACT_SCHEMA,
            "scope": [],
            "seq": seq,
            "severity": null,
            "status": null,
            "subject": format!("artifact {i}"),
            "summary": null,
            "target": null,
            "thread_id": format!("room_{:016x}", rng.next()),
            "tool": tool,
            "uri": null,
        });
        let r = rng.unit();
        if r < 0.22 {
            payload["kind"] = json!("presence");
            payload["subject"] = json!(format!("agent presence: {tool}"));
            payload["evidence"] = json!(["planned_heartbeat_secs:300"]);
        } else if r < 0.28 {
            let path = format!("src/m{}/f{}.rs", rng.below(200), rng.below(30));
            payload["kind"] = json!("claim");
            payload["subject"] = json!(format!("edit {path}"));
            payload["scope"] = json!([format!("file:{path}")]);
            payload["evidence"] = json!([format!("lease_expires_at:{}", ts(age - 1800))]);
            open_claims.push(payload.clone());
        } else if r < 0.36 && !open_claims.is_empty() {
            let claim = open_claims.remove(rng.below(open_claims.len() as u64) as usize);
            payload["kind"] = json!("release");
            payload["tool"] = claim["tool"].clone();
            payload["from_session_id"] = claim["from_session_id"].clone();
            payload["subject"] = json!("release");
            payload["ref"] = claim["event_id"].clone();
            payload["scope"] = claim["scope"].clone();
        } else if (0.36..0.44).contains(&r) {
            let target = tools[rng.below(tools.len() as u64) as usize].clone();
            payload["kind"] = json!("handoff");
            payload["subject"] = json!(format!("handoff to {target}"));
            payload["target"] = json!(target);
            open_handoffs.push(payload.clone());
        } else if (0.44..0.53).contains(&r) && !open_handoffs.is_empty() {
            let handoff = open_handoffs.remove(rng.below(open_handoffs.len() as u64) as usize);
            payload["kind"] = json!("resolve");
            payload["tool"] = handoff["target"].clone();
            payload["from_session_id"] = json!(format!(
                "sess:{}",
                handoff["target"].as_str().unwrap_or_default()
            ));
            payload["subject"] = json!("resolve handoff");
            payload["ref"] = handoff["event_id"].clone();
        } else if r < 0.58 {
            payload["kind"] = json!("decision");
            payload["subject"] = json!("decision: keep segments canonical");
        } else if r < 0.61 {
            payload["kind"] = json!("risk");
            payload["severity"] = json!("warn");
            payload["subject"] = json!("risk: projection lag");
        }
        // Line-size spread: p50 ~700 B, ~1% around 2 KB, a few ~10 KB.
        let pad = {
            let u = rng.unit();
            if u < 0.0005 {
                9_000
            } else if u < 0.01 {
                1_600
            } else {
                250 + rng.below(350) as usize
            }
        };
        payload["summary"] = json!("x".repeat(pad));
        let kind = payload["kind"].as_str().unwrap().to_string();
        let line = json!({
            "seq": seq,
            "occurred_at": created,
            "event_type": kind,
            "payload": payload,
            "engagement": engagement,
        });
        match files.last_mut() {
            Some((name, body)) if *name == engagement => {
                body.push_str(&line.to_string());
                body.push('\n');
            }
            _ => files.push((engagement.clone(), format!("{line}\n"))),
        }
    }
    for (name, body) in files {
        std::fs::write(log.join(format!("{name}.jsonl")), body).unwrap();
    }
    SyntheticRoom { root }
}

/// Serialize on the crate-wide env lock, enter `dir`, scrub session env, and
/// restore on drop.
pub(crate) struct RoomCwd {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<PathBuf>,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

pub(crate) const TEST_SESSION: &str = "s0-relevance-session";

const SCRUBBED_ENV: &[&str] = &[
    "RALLY_TOOL_ID",
    "RALLY_ENGAGEMENT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_SESSION_ID",
    "CODEX_COMPANION_SESSION_ID",
    "CODEX_SESSION_ID",
];

impl RoomCwd {
    pub(crate) fn enter(dir: &Path) -> Self {
        let lock = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = std::env::current_dir().ok();
        let mut saved = Vec::new();
        for key in SCRUBBED_ENV {
            saved.push((key.to_string(), std::env::var_os(key)));
            // SAFETY: serialized by PROCESS_ENV_LOCK, restored on drop.
            unsafe { std::env::remove_var(key) };
        }
        for (key, value) in [
            ("RALLY_SESSION_ID", TEST_SESSION),
            ("RALLY_HOOKS", "off"),
            ("RALLY_DAEMON_AUTOSTART", "0"),
            ("RALLY_NO_AUTO_REAP", "1"),
        ] {
            saved.push((key.to_string(), std::env::var_os(key)));
            // SAFETY: serialized by PROCESS_ENV_LOCK, restored on drop.
            unsafe { std::env::set_var(key, value) };
        }
        std::env::set_current_dir(dir).expect("enter synthetic room");
        Self {
            _lock: lock,
            prev,
            saved,
        }
    }
}

impl Drop for RoomCwd {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: still serialized by PROCESS_ENV_LOCK.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(&key, value),
                    None => std::env::remove_var(&key),
                }
            }
        }
        if let Some(prev) = &self.prev {
            let _ = std::env::set_current_dir(prev);
        }
    }
}

/// Run one CLI command in-process (same entry as the binary minus the
/// watchdog thread), returning the JSON body.
pub(crate) fn rally(args: &[&str]) -> Value {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let _ = crate::drain_pending_append_outcomes();
    let _ = crate::drain_pending_append_issues();
    match crate::run_inner_with(&args) {
        Ok(output) => output.body,
        Err(error) => panic!("rally {args:?} failed: {error}"),
    }
}

/// Simulate a fresh CLI process: forget every in-process memo, then reset the
/// probe counters.
pub(crate) fn fresh_process() {
    crate::store::forget_process_memos_for_test();
    history_probe::reset();
}

pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, history_probe::Counts, f64) {
    fresh_process();
    let start = Instant::now();
    let value = f();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    (value, history_probe::counts(), ms)
}

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

pub(crate) const ME: &str = "me:1";
pub(crate) const PEER: &str = "peer:1";
const PEER_SESSION: &str = "sess:peer-session#live";

/// The session id the CLI stamps for `tool` under [`RoomCwd`].
pub(crate) fn session_of(tool: &str) -> String {
    crate::current_protocol_session(Some(tool))
        .from_session_id()
        .to_string()
}

fn now_fact(kind: crate::store::FactKind, tool: &str, subject: &str) -> crate::store::Fact {
    let session = if tool == PEER {
        PEER_SESSION.to_string()
    } else {
        session_of(tool)
    };
    crate::store::Fact {
        schema: crate::FACT_SCHEMA.to_string(),
        event_id: crate::new_id("fact"),
        thread_id: crate::new_id("room"),
        kind,
        tool: Some(tool.to_string()),
        from_session_id: Some(session),
        subject: subject.to_string(),
        created_at: crate::now_string(),
        ..Default::default()
    }
}

/// A synthetic room plus a small LIVE layer whose relevance is known:
/// * `ME` and `PEER` are present now; every generated tool last acted at
///   least 7 days ago (departed roster).
/// * `PEER` holds a live claim on `src/live.rs` (lease in the future).
/// * a departed tool holds an expired claim on `src/old.rs`.
/// * one OPEN handoff targets `ME`; one handoff to `ME` is already resolved;
///   one open handoff targets someone else.
pub(crate) struct RelevanceRoom {
    pub(crate) open_handoff_to_me: String,
    pub(crate) resolved_handoff_to_me: String,
    pub(crate) handoff_to_other: String,
    pub(crate) live_claim: String,
    pub(crate) expired_claim: String,
    pub(crate) root: PathBuf,
}

fn populate_relevance_layer(root: &Path) -> RelevanceRoom {
    use crate::store::FactKind;
    let store = crate::store::RoomStore::open_at(root.to_path_buf()).unwrap();
    let append = |fact: &crate::store::Fact| store.append_fact(fact).unwrap().fact.event_id;
    for tool in [ME, PEER] {
        let mut presence = now_fact(FactKind::Presence, tool, &format!("agent presence: {tool}"));
        presence.evidence = vec!["planned_heartbeat_secs:300".to_string()];
        append(&presence);
    }
    let mut live = now_fact(FactKind::Claim, PEER, "edit src/live.rs");
    live.scope = vec!["file:src/live.rs".to_string()];
    live.evidence = vec![format!(
        "lease_expires_at:{}",
        (chrono::Utc::now() + chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )];
    let live_claim = append(&live);
    let mut expired = now_fact(FactKind::Claim, "old-departed:1", "edit src/old.rs");
    expired.from_session_id = Some("sess:old-departed".to_string());
    expired.scope = vec!["file:src/old.rs".to_string()];
    expired.created_at = ts(40 * 24 * 3600);
    expired.evidence = vec![format!("lease_expires_at:{}", ts(40 * 24 * 3600 - 1800))];
    let expired_claim = append(&expired);
    let mut open = now_fact(FactKind::Handoff, PEER, "please review src/live.rs");
    open.target = Some(ME.to_string());
    let open_handoff_to_me = append(&open);
    let mut done = now_fact(FactKind::Handoff, PEER, "already handled");
    done.target = Some(ME.to_string());
    let resolved_handoff_to_me = append(&done);
    let mut resolve = now_fact(FactKind::Resolve, ME, "resolved");
    resolve.ref_id = Some(resolved_handoff_to_me.clone());
    append(&resolve);
    let mut other = now_fact(FactKind::Handoff, PEER, "for someone else");
    other.target = Some("other:9".to_string());
    let handoff_to_other = append(&other);
    RelevanceRoom {
        open_handoff_to_me,
        resolved_handoff_to_me,
        handoff_to_other,
        live_claim,
        expired_claim,
        root: root.to_path_buf(),
    }
}

pub(crate) fn with_relevance_room(label: &str, events: usize, f: impl FnOnce(&RelevanceRoom)) {
    let room = synthetic_room(label, events, 6);
    let _cwd = RoomCwd::enter(&room.root);
    let rr = populate_relevance_layer(&room.root);
    // Warm the derived store the way a running room is warm: sidecars,
    // indexes and the reconcile proof exist before anything is measured.
    rally(&["room", "--json"]);
    f(&rr);
}

/// What `rally say` does up to and including its durable append — open the
/// store, the implicit presence guard (and presence append when needed),
/// then the fact itself — without the post-commit room summary.
fn say_append_path(tool: &str, subject: &str) -> crate::store::AppendOutcome {
    let room = crate::store::RoomStore::open().unwrap();
    crate::ensure_presence(&room, tool).unwrap();
    let fact = now_fact(crate::store::FactKind::Artifact, tool, subject);
    room.append_fact_verified(&fact).unwrap()
}

fn assert_no_history_work(label: &str, counts: history_probe::Counts) {
    assert_eq!(
        (
            counts.canonical_folds,
            counts.facts_db_full_loads,
            counts.snapshot_folds,
            counts.segment_file_parses
        ),
        (0, 0, 0, 0),
        "{label}: O(history) work on the steady-state path: {counts:?}"
    );
}

// ---------------------------------------------------------------------------
// Stage 0 write-path proofs (design §7.1)
// ---------------------------------------------------------------------------

/// The steady-state append path — open, implicit presence (a NEW session, so
/// presence is appended), and the fact — runs zero canonical folds, zero
/// full facts.db loads, zero snapshot folds and zero whole-segment parses,
/// across consecutive appends, and leaves sidecars a full scan agrees with.
#[test]
fn stage0_steady_state_append_path_does_no_history_work() {
    let room = synthetic_room("s0-append-path", 3_000, 6);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    rally(&[
        "say",
        "artifact",
        "--tool",
        "warm:1",
        "--subject",
        "warm",
        "--json",
    ]);
    for i in 0..4 {
        let tool = format!("probe{i}:x");
        let (outcome, counts, _) = measure(|| say_append_path(&tool, "probe"));
        assert!(
            outcome.projection_complete,
            "append {i} must project cleanly: {:?}",
            outcome.warnings
        );
        assert_no_history_work(&format!("append {i} (new session)"), counts);
    }
    // Same session again: the presence guard answers from the session index
    // and appends nothing.
    let (_, counts, _) = measure(|| say_append_path("probe0:x", "again"));
    assert_no_history_work("append (known session)", counts);
    assert!(
        crate::store::reconcile_sidecar_matches_full_scan_for_test(&room.root),
        "the one-row sidecar advance must agree with an authoritative full scan"
    );
}

/// Documents the single O(history) read left on `say`: its JSON envelope
/// carries a `RoomSummary`, which is a full snapshot fold. Everything else on
/// the command is O(1) in history.
#[test]
fn stage0_say_command_history_work_is_only_the_post_commit_room_summary() {
    let room = synthetic_room("s0-say-command", 3_000, 6);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    rally(&[
        "say",
        "artifact",
        "--tool",
        "warm:1",
        "--subject",
        "warm",
        "--json",
    ]);
    let (_, counts, _) = measure(|| {
        rally(&[
            "say",
            "artifact",
            "--tool",
            "cmd:1",
            "--subject",
            "probe",
            "--json",
        ])
    });
    assert_eq!(counts.canonical_folds, 0, "{counts:?}");
    assert_eq!(counts.segment_file_parses, 0, "{counts:?}");
    assert_eq!(
        (counts.facts_db_full_loads, counts.snapshot_folds),
        (1, 1),
        "exactly one post-commit snapshot (load + fold) for the envelope: {counts:?}"
    );
}

#[test]
#[ignore = "stage2: `say --json` builds RoomSummary from a full snapshot fold after the commit; needs an incremental/lazy projection or an envelope change"]
fn stage2_say_command_does_no_history_work() {
    let room = synthetic_room("s2-say-command", 3_000, 6);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    rally(&[
        "say",
        "artifact",
        "--tool",
        "warm:1",
        "--subject",
        "warm",
        "--json",
    ]);
    let (_, counts, _) = measure(|| {
        rally(&[
            "say",
            "artifact",
            "--tool",
            "cmd:1",
            "--subject",
            "probe",
            "--json",
        ])
    });
    assert_no_history_work("say --json", counts);
}

/// Missing sidecar → the append takes every original full path (and says
/// so in the probes), republishes the sidecar, and the NEXT append is back on
/// the O(1) path.
#[test]
fn stage0_missing_sidecar_falls_back_to_full_scan_then_recovers() {
    let room = synthetic_room("s0-fallback", 1_500, 4);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    say_append_path("warm:1", "warm");
    crate::store::remove_reconcile_sidecar_for_test(&room.root);
    // Bypass open-time reconcile so the APPEND sees the missing proof.
    let store = crate::store::RoomStore::open().unwrap();
    crate::store::remove_reconcile_sidecar_for_test(&room.root);
    let (outcome, counts, _) = measure(|| {
        store
            .append_fact_verified(&now_fact(crate::store::FactKind::Artifact, "warm:1", "x"))
            .unwrap()
    });
    assert!(outcome.projection_complete, "{:?}", outcome.warnings);
    assert!(
        counts.canonical_folds > 0 && counts.segment_file_parses > 0,
        "no proof must mean the authoritative path: {counts:?}"
    );
    assert!(crate::store::reconcile_sidecar_matches_full_scan_for_test(
        &room.root
    ));
    drop(store);
    let (_, counts, _) = measure(|| say_append_path("warm:1", "y"));
    assert_no_history_work("append after recovery", counts);
}

/// I1/I8: a cross-worktree `merge=union` that leaves two DIFFERENT rows with
/// one seq changes the segment fingerprint, so the next append cannot use the
/// sidecar high-water and the authoritative fold refuses the conflict instead
/// of allocating past it.
#[test]
fn stage0_union_merge_duplicate_seq_is_not_masked() {
    let room = synthetic_room("s0-union-merge", 1_500, 4);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    say_append_path("warm:1", "warm");
    let log = room.root.join(".rally/log");
    let active = log.join(format!("{ACTIVE_ENGAGEMENT}.jsonl"));
    let text = std::fs::read_to_string(&active).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let mut forged = last.clone();
    forged["payload"]["event_id"] = json!("fact_merged_from_other_branch");
    forged["payload"]["subject"] = json!("same seq, other branch");
    let other = log.join("s0-other-branch.jsonl");
    std::fs::write(&other, format!("{forged}\n")).unwrap();
    let store = crate::store::RoomStore::open_existing_at(room.root.clone());
    let error = match store {
        Err(error) => error.to_string(),
        Ok(Some(store)) => store
            .append_fact_verified(&now_fact(crate::store::FactKind::Artifact, "warm:1", "z"))
            .unwrap_err()
            .to_string(),
        Ok(None) => panic!("room must exist"),
    };
    assert!(
        error.contains("conflicting canonical segment rows"),
        "duplicate seq must surface, not be masked by the sidecar: {error}"
    );
}

/// I3: a torn final fragment changes the fingerprint, so the append takes the
/// full tail inspection, truncates the fragment under the lock, and the new
/// row lands on the next contiguous seq.
#[test]
fn stage0_torn_tail_still_repaired_before_append() {
    let room = synthetic_room("s0-torn-tail", 1_500, 4);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    let before = say_append_path("warm:1", "warm").fact.seq;
    let active = room
        .root
        .join(format!(".rally/log/{ACTIVE_ENGAGEMENT}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    std::io::Write::write_all(&mut file, br#"{"seq":999999,"occurred_at":"2026-"#).unwrap();
    drop(file);
    let store = crate::store::RoomStore::open().unwrap();
    let (outcome, counts, _) = measure(|| {
        store
            .append_fact_verified(&now_fact(
                crate::store::FactKind::Artifact,
                "warm:1",
                "after torn",
            ))
            .unwrap()
    });
    assert_eq!(outcome.fact.seq, before + 1);
    assert!(
        counts.segment_file_parses > 0,
        "full inspection must run: {counts:?}"
    );
    let text = std::fs::read_to_string(&active).unwrap();
    assert!(!text.contains("999999"), "torn fragment must be truncated");
    assert!(text.ends_with('\n'));
}

/// I7 on the scoped lifecycle path: a write from a CLOSED session is refused
/// under the mutation lock even though the check now reads only that
/// session's rows from the derived index.
#[test]
fn stage0_scoped_lifecycle_check_still_refuses_closed_session() {
    use crate::store::FactKind;
    let room = synthetic_room("s0-closed-session", 1_500, 4);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    say_append_path("warm:1", "warm");
    let store = crate::store::RoomStore::open().unwrap();
    let session = "sess:closing#live";
    let mut presence = now_fact(FactKind::Presence, "closer:1", "session active");
    presence.from_session_id = Some(session.to_string());
    presence.evidence = vec![
        "protocol:session_state=active".to_string(),
        crate::session_identity::session_close_token_hash_marker("secret"),
    ];
    store.append_fact(&presence).unwrap();
    let mut close = now_fact(FactKind::Session, "closer:1", "session closed");
    close.from_session_id = Some(session.to_string());
    close.evidence = vec![
        "protocol:session_state=closed".to_string(),
        format!(
            "{}secret",
            crate::session_identity::SESSION_CLOSE_TOKEN_REVEAL_PREFIX
        ),
    ];
    store.append_fact(&close).unwrap();
    let mut late = now_fact(FactKind::Artifact, "closer:1", "after close");
    late.from_session_id = Some(session.to_string());
    let (result, counts, _) = measure(|| store.append_fact(&late));
    let error = result.unwrap_err().to_string();
    assert!(error.contains("is closed"), "{error}");
    assert_eq!(
        counts.canonical_folds, 0,
        "scoped path must have answered: {counts:?}"
    );
}

/// Presence skip vs liveness/reaper windows (design §8 risk). The implicit
/// presence guard never re-posts for a session that already has presence, so
/// its "debounce" is unbounded in time — which is safe only because liveness
/// reads the age of the tool's NEWEST fact of any kind (store.rs signal (a)):
/// the command that skipped presence is itself the heartbeat. Assert both the
/// constant relation and the behaviour.
#[test]
fn presence_skip_never_outlives_liveness_or_reap_window() {
    use crate::store::FactKind;
    let liveness_window = crate::liveness::DEFAULT_CADENCE_SECS * crate::liveness::MISS_MULTIPLIER
        + crate::liveness::GRACE_SECS;
    let reap_window = crate::decay::DEFAULT_RECLAIM_SMALL_MINUTES * 60;
    // Heartbeat lag a skipped presence can introduce: zero, because the
    // skipping command appends its own fact in the same process.
    let presence_skip_heartbeat_lag_secs = 0_i64;
    assert!(presence_skip_heartbeat_lag_secs < liveness_window.min(reap_window));

    let room = synthetic_room("s0-presence-liveness", 1_500, 4);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    let store = crate::store::RoomStore::open().unwrap();
    let tool = "sleeper:1";
    // A presence posted two liveness windows ago.
    let mut old = now_fact(FactKind::Presence, tool, &format!("agent presence: {tool}"));
    old.created_at = ts(2 * liveness_window);
    old.evidence = vec!["planned_heartbeat_secs:300".to_string()];
    store.append_fact(&old).unwrap();
    let presence_rows = |store: &crate::store::RoomStore| {
        store
            .session_presence_facts(&session_of(tool))
            .unwrap()
            .len()
    };
    assert_eq!(presence_rows(&store), 1);
    drop(store);
    rally(&[
        "say",
        "artifact",
        "--tool",
        tool,
        "--subject",
        "still here",
        "--json",
    ]);
    let store = crate::store::RoomStore::open().unwrap();
    assert_eq!(presence_rows(&store), 1, "presence must not be re-posted");
    let snapshot = store.snapshot().unwrap();
    let squad = snapshot.squad_for(tool).expect("squad row");
    let age = squad.age_secs.expect("heartbeat age");
    assert!(
        age < liveness_window.min(reap_window),
        "the skipping command must refresh the heartbeat: age {age}s"
    );
}

// ---------------------------------------------------------------------------
// Perf gates (design §7.1). Run with:
//   cargo test --release -p rally-cli --lib perf_gate -- --ignored --nocapture
// ---------------------------------------------------------------------------

fn p50(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

#[test]
#[ignore = "perf gate: builds a 60k-event / ~45 MB room; run with --release"]
fn perf_gate_stage0_append_path_p50_under_150ms() {
    let room = synthetic_room("perf-append", 60_000, 48);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    say_append_path("warm:1", "warm");
    let mut samples = Vec::new();
    for i in 0..9 {
        let tool = format!("perf{i}:x");
        let (_, counts, ms) = measure(|| say_append_path(&tool, "perf"));
        assert_no_history_work("perf append", counts);
        samples.push(ms);
    }
    let p50 = p50(samples.clone());
    eprintln!("append path (new session each) samples {samples:?} p50 {p50:.1} ms");
    assert!(p50 < 150.0, "append path p50 {p50:.1} ms");
}

#[test]
#[ignore = "stage2: `say --json` still folds a full snapshot for its RoomSummary (~1 s at 60k events)"]
fn perf_gate_say_command_p50_under_150ms() {
    let room = synthetic_room("perf-say", 60_000, 48);
    let _cwd = RoomCwd::enter(&room.root);
    rally(&["room", "--json"]);
    rally(&[
        "say",
        "artifact",
        "--tool",
        "warm:1",
        "--subject",
        "warm",
        "--json",
    ]);
    let mut samples = Vec::new();
    for i in 0..9 {
        let tool = format!("perf{i}:x");
        let (_, _, ms) = measure(|| {
            rally(&[
                "say",
                "artifact",
                "--tool",
                &tool,
                "--subject",
                "perf",
                "--json",
            ])
        });
        samples.push(ms);
    }
    let p50 = p50(samples.clone());
    eprintln!("say --json samples {samples:?} p50 {p50:.1} ms");
    assert!(p50 < 150.0, "say --json p50 {p50:.1} ms");
}

// ---------------------------------------------------------------------------
// Read relevance (owner's ask: "test that Rally reads only the most relevant
// info"). For each read the hooks and agents actually call, report
// (a) O(history) load yes/no, (b) facts materialized vs items returned,
// (c) scoping and stale/dead exclusion. Passing assertions are regression
// guards; failing expectations are `#[ignore = "stage2: ..."]`.
// ---------------------------------------------------------------------------

fn len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn hook_before_write(rr: &RelevanceRoom, rel_path: &str) -> Value {
    let stdin = json!({
        "hook_event_name": "PreToolUse",
        "session_id": TEST_SESSION,
        "cwd": rr.root.display().to_string(),
        "tool_name": "Edit",
        "tool_input": {"file_path": rr.root.join(rel_path).display().to_string()},
    })
    .to_string();
    // The fixture runs with RALLY_HOOKS=off; the native hook honours that and
    // would return `{}` before reading anything. Lift it for this call only
    // (RoomCwd holds PROCESS_ENV_LOCK and restores the variable on drop).
    // SAFETY: serialized by PROCESS_ENV_LOCK, held by the caller's RoomCwd.
    unsafe { std::env::remove_var("RALLY_HOOKS") };
    let envelope = crate::hook_runtime::run_before_write(crate::hook_runtime::HookRequest {
        tool_arg: ME.to_string(),
        session_arg: Some(TEST_SESSION.to_string()),
        repo_root: Some(rr.root.clone()),
        strict: false,
        stdin,
    });
    // SAFETY: as above.
    unsafe { std::env::set_var("RALLY_HOOKS", "off") };
    envelope
}

struct ReadProbe {
    command: &'static str,
    counts: history_probe::Counts,
    returned: usize,
    ms: f64,
    body: Value,
}

impl ReadProbe {
    fn full_history_load(&self) -> bool {
        self.counts.facts_db_full_loads > 0
            || self.counts.canonical_folds > 0
            || self.counts.snapshot_folds > 0
    }
}

/// Measure one read on the snapshot-cache MISS path. `check` reuses an
/// exact-second snapshot cache that any append invalidates, so without this
/// the probe would report whichever side of a second boundary it landed on.
fn probe_read(
    command: &'static str,
    run: impl FnOnce() -> Value,
    returned: impl FnOnce(&Value) -> usize,
) -> ReadProbe {
    let _ = std::fs::remove_file(
        std::env::current_dir()
            .unwrap()
            .join(".rally/snapshot.cache.json"),
    );
    let (body, counts, ms) = measure(run);
    let returned = returned(&body);
    ReadProbe {
        command,
        counts,
        returned,
        ms,
        body,
    }
}

fn probe_room() -> ReadProbe {
    probe_read(
        "room --json",
        || rally(&["room", "--json"]),
        |body| {
            let room = &body["data"]["room"];
            [
                "squads",
                "active_claims",
                "open_handoffs",
                "recent_artifacts",
                "unconsumed_artifacts",
                "current_decisions",
                "current_risks",
                "resolved_unverified",
                "stale_facts",
            ]
            .iter()
            .map(|key| len(&room[*key]))
            .sum::<usize>()
                + len(&body["data"]["agent_injectability"])
        },
    )
}

fn probe_claims() -> ReadProbe {
    probe_read(
        "claims --json",
        || rally(&["claims", "--json"]),
        |body| len(&body["data"]["claims"]["rows"]),
    )
}

fn probe_next() -> ReadProbe {
    probe_read(
        "next --audit --json",
        || rally(&["next", "--tool", ME, "--audit", "--json"]),
        |body| {
            let next = &body["data"]["next"];
            1 + len(&next["alternatives"])
                + len(&next["peer_targets"]["ranked"])
                + len(&next["inbox"]["items"])
        },
    )
}

fn probe_inbox() -> ReadProbe {
    probe_read(
        "inbox --json",
        || rally(&["inbox", "--tool", ME, "--json"]),
        |body| len(&body["data"]["inbox"]["items"]),
    )
}

fn probe_before_write(path: &str) -> ReadProbe {
    let path = path.to_string();
    probe_read(
        "check before-write --json",
        move || {
            rally(&[
                "check",
                "before-write",
                "--tool",
                ME,
                "--path",
                &path,
                "--json",
            ])
        },
        |body| len(&body["data"]["check"]["findings"]),
    )
}

fn probe_before_complete() -> ReadProbe {
    probe_read(
        "check before-complete --json",
        || rally(&["check", "before-complete", "--tool", ME, "--json"]),
        |body| len(&body["data"]["check"]["findings"]),
    )
}

fn probe_status_read() -> ReadProbe {
    probe_read(
        "status read --json",
        || rally(&["status", "read", "--json"]),
        |body| len(&body["data"]["status_read"]["states"]),
    )
}

fn probe_hook(rr: &RelevanceRoom, rel_path: &str) -> ReadProbe {
    probe_read(
        "hook before-write (native)",
        || hook_before_write(rr, rel_path),
        |body| usize::from(!body.as_object().is_some_and(|map| map.is_empty())),
    )
}

fn probe_enter() -> ReadProbe {
    probe_read(
        "enter --json (SessionStart)",
        || rally(&["enter", "--tool", ME, "--json"]),
        |body| body["data"].as_object().map_or(0, |map| map.len()),
    )
}

/// Prints the per-command relevance table used in the Stage 0 report. Always
/// passes; the assertions live in the focused tests below.
#[test]
fn read_relevance_report() {
    with_relevance_room("relevance-report", 3_000, |rr| {
        let probes = vec![
            probe_room(),
            probe_claims(),
            probe_next(),
            probe_inbox(),
            probe_before_write("src/live.rs"),
            probe_before_complete(),
            probe_status_read(),
            probe_hook(rr, "src/live.rs"),
            probe_enter(),
        ];
        eprintln!("| command | full-history load | facts read | items returned | ms |");
        for probe in &probes {
            eprintln!(
                "| {} | {} (db loads {}, folds {}, snapshots {}) | {} | {} | {:.0} |",
                probe.command,
                if probe.full_history_load() {
                    "yes"
                } else {
                    "no"
                },
                probe.counts.facts_db_full_loads,
                probe.counts.canonical_folds,
                probe.counts.snapshot_folds,
                probe.counts.facts_materialized,
                probe.returned,
                probe.ms
            );
        }
    });
}

// ---- passing relevance guards ----------------------------------------------

#[test]
fn relevance_inbox_returns_only_callers_open_obligations() {
    with_relevance_room("relevance-inbox", 1_500, |rr| {
        let probe = probe_inbox();
        let ids: Vec<&str> = probe.body["data"]["inbox"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["event_id"].as_str())
            .collect();
        assert_eq!(ids, vec![rr.open_handoff_to_me.as_str()]);
        assert!(!ids.contains(&rr.resolved_handoff_to_me.as_str()));
        assert!(!ids.contains(&rr.handoff_to_other.as_str()));
    });
}

#[test]
fn relevance_next_primary_action_is_callers_open_handoff() {
    with_relevance_room("relevance-next", 1_500, |rr| {
        let probe = probe_next();
        let next = &probe.body["data"]["next"];
        assert_eq!(next["target_event_id"], json!(rr.open_handoff_to_me));
        assert_eq!(next["inbox"]["count"], json!(1));
        let text = next.to_string();
        assert!(!text.contains(&rr.resolved_handoff_to_me));
        assert!(!text.contains(&rr.handoff_to_other));
    });
}

#[test]
fn relevance_before_write_is_scoped_to_the_path_and_live_claims() {
    with_relevance_room("relevance-before-write", 1_500, |rr| {
        let blocked = probe_before_write("src/live.rs");
        let check = &blocked.body["data"]["check"];
        assert_eq!(check["allow"], json!(false));
        let claimed: Vec<&Value> = check["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["code"] == "claimed-path")
            .collect();
        assert_eq!(claimed.len(), 1, "{check}");
        assert_eq!(claimed[0]["fact_id"], json!(rr.live_claim));
        // The departed tool's EXPIRED claim does not block its path; it is
        // surfaced once, as path-scoped reclaim advice (warn), never a stop.
        let free = probe_before_write("src/old.rs");
        let check = &free.body["data"]["check"];
        assert_eq!(check["allow"], json!(true), "{check}");
        let on_old: Vec<&Value> = check["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["fact_id"] == json!(rr.expired_claim))
            .collect();
        assert_eq!(on_old.len(), 1, "{check}");
        assert_eq!(on_old[0]["code"], json!("stale-owner-claim"));
        assert_ne!(on_old[0]["severity"], json!("stop"));
        assert!(!check.to_string().contains(&rr.live_claim), "{check}");
    });
}

#[test]
#[ignore = "stage2: before-complete folds a full snapshot on a snapshot-cache miss (O(1) only on an exact-second cache hit)"]
fn stage2_before_complete_reads_no_history() {
    with_relevance_room("relevance-before-complete", 1_500, |_| {
        let probe = probe_before_complete();
        assert!(!probe.full_history_load(), "{:?}", probe.counts);
    });
}

#[test]
fn relevance_claims_lists_live_claim_first_and_marks_expired() {
    with_relevance_room("relevance-claims", 1_500, |rr| {
        let probe = probe_claims();
        let rows = probe.body["data"]["claims"]["rows"].as_array().unwrap();
        assert_eq!(rows[0]["event_id"], json!(rr.live_claim));
        assert_eq!(rows[0]["expired"], json!(false));
        for row in &rows[1..] {
            assert_eq!(row["expired"], json!(true), "{row}");
        }
    });
}

#[test]
fn relevance_room_drops_resolved_handoffs() {
    with_relevance_room("relevance-room", 1_500, |rr| {
        let probe = probe_room();
        let open: Vec<&str> = probe.body["data"]["room"]["open_handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["event_id"].as_str())
            .collect();
        assert!(open.contains(&rr.open_handoff_to_me.as_str()));
        assert!(!open.contains(&rr.resolved_handoff_to_me.as_str()));
    });
}

#[test]
fn relevance_status_read_marks_departed_tools_stale() {
    with_relevance_room("relevance-status", 1_500, |_| {
        let probe = probe_status_read();
        for state in probe.body["data"]["status_read"]["states"]
            .as_array()
            .unwrap()
        {
            let tool = state["tool"].as_str().unwrap_or_default();
            let fresh = tool == ME || tool == PEER;
            assert_eq!(state["stale"], json!(!fresh), "{state}");
        }
    });
}

#[test]
fn relevance_hook_before_write_is_scoped_to_the_target_path() {
    with_relevance_room("relevance-hook", 1_500, |rr| {
        let blocked = probe_hook(rr, "src/live.rs").body.to_string();
        assert!(
            blocked.contains(PEER),
            "live claim owner must be named: {blocked}"
        );
        let free = probe_hook(rr, "src/unclaimed.rs").body.to_string();
        assert!(
            !free.contains(PEER),
            "unrelated path must not cite peers: {free}"
        );
        assert!(!free.contains("old-departed"), "{free}");
    });
}

// ---- stage 2 failing expectations ------------------------------------------

macro_rules! stage2_no_full_history_load {
    ($name:ident, $reason:literal, $probe:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            with_relevance_room(stringify!($name), 1_500, |rr| {
                let _ = rr;
                let probe: ReadProbe = $probe(rr);
                assert!(
                    !probe.full_history_load(),
                    "{} loads full history: {:?}",
                    probe.command,
                    probe.counts
                );
            });
        }
    };
}

stage2_no_full_history_load!(
    stage2_room_reads_without_full_history_load,
    "stage2: room projects a full snapshot (2 full facts.db loads + 1 fold) per call",
    |_| probe_room()
);
stage2_no_full_history_load!(
    stage2_claims_reads_without_full_history_load,
    "stage2: claims folds a full snapshot to list active claims",
    |_| probe_claims()
);
stage2_no_full_history_load!(
    stage2_next_reads_without_full_history_load,
    "stage2: next --audit loads facts.db twice and folds a full snapshot",
    |_| probe_next()
);
stage2_no_full_history_load!(
    stage2_inbox_reads_without_full_history_load,
    "stage2: inbox folds a full snapshot to find the caller's obligations",
    |_| probe_inbox()
);
stage2_no_full_history_load!(
    stage2_before_write_reads_without_full_history_load,
    "stage2: check before-write folds a full snapshot (cache misses after every write)",
    |_| probe_before_write("src/live.rs")
);
stage2_no_full_history_load!(
    stage2_status_read_reads_without_full_history_load,
    "stage2: status read loads every fact to find the latest status per tool",
    |_| probe_status_read()
);
stage2_no_full_history_load!(
    stage2_hook_before_write_reads_without_full_history_load,
    "stage2: native before-write hook folds a full snapshot per edit",
    |rr: &RelevanceRoom| probe_hook(rr, "src/live.rs")
);
stage2_no_full_history_load!(
    stage2_enter_reads_without_full_history_load,
    "stage2: enter loads the full fact list for its context",
    |_| probe_enter()
);

#[test]
#[ignore = "stage2: room lists every author ever seen in squads and agent_injectability, including tools silent for 7+ days"]
fn stage2_room_excludes_departed_roster() {
    with_relevance_room("stage2-room-roster", 1_500, |_| {
        let probe = probe_room();
        let live = [ME, PEER];
        let squads = probe.body["data"]["room"]["squads"].as_array().unwrap();
        let departed: Vec<&str> = squads
            .iter()
            .filter_map(|squad| squad["tool"].as_str())
            .filter(|tool| !live.contains(tool))
            .collect();
        assert!(departed.is_empty(), "{} departed squads", departed.len());
        assert!(
            len(&probe.body["data"]["agent_injectability"]) <= live.len(),
            "agent_injectability lists departed tools"
        );
    });
}

#[test]
#[ignore = "stage2: claims returns expired claims (marked expired) alongside the live one"]
fn stage2_claims_excludes_expired_claims() {
    with_relevance_room("stage2-claims-expired", 1_500, |_| {
        let probe = probe_claims();
        let rows = probe.body["data"]["claims"]["rows"].as_array().unwrap();
        assert!(
            rows.iter().all(|row| row["expired"] == json!(false)),
            "{} expired rows returned",
            rows.iter()
                .filter(|row| row["expired"] == json!(true))
                .count()
        );
    });
}

#[test]
#[ignore = "stage2: status read returns a row for every tool ever seen, flagging departed ones stale instead of omitting them"]
fn stage2_status_read_excludes_departed_tools() {
    with_relevance_room("stage2-status-departed", 1_500, |_| {
        let probe = probe_status_read();
        let stale = probe.body["data"]["status_read"]["states"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|state| state["stale"] == json!(true))
            .count();
        assert_eq!(stale, 0, "{stale} departed tools returned");
    });
}

#[test]
#[ignore = "stage2: next ranks departed peers in peer_targets.ranked"]
fn stage2_next_peer_targets_exclude_departed() {
    with_relevance_room("stage2-next-peers", 1_500, |_| {
        let probe = probe_next();
        let ranked = probe.body["data"]["next"]["peer_targets"]["ranked"]
            .as_array()
            .unwrap();
        let departed = ranked
            .iter()
            .filter(|peer| {
                let tool = peer["tool"].as_str().unwrap_or_default();
                tool != ME && tool != PEER
            })
            .count();
        assert_eq!(departed, 0, "{departed} departed peers ranked: {ranked:?}");
    });
}

/// `enter` (SessionStart) appends presence and a read checkpoint; both
/// appends — including the checkpoint's coalescing guard — run without a
/// canonical fold. Its remaining full loads are the context reads (stage 2).
#[test]
fn stage0_enter_appends_do_no_canonical_fold() {
    with_relevance_room("s0-enter", 1_000, |_| {
        let probe = probe_enter();
        assert_eq!(
            (
                probe.counts.canonical_folds,
                probe.counts.segment_file_parses
            ),
            (0, 0),
            "{:?}",
            probe.counts
        );
    });
}
