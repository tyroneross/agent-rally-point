// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Machine-initiated writes must name the actor that caused them.
//!
//! # What was wrong
//!
//! `wake_fact` and the three reaper `Fact` builders hardcoded
//! `tool: Some("rally")`, `from_session_id: None`, `role: None`. Measured over
//! this repo's own room (12,746 facts, read-only), the automatic writers had
//! produced 644 of 645 wake facts and 274 of 278 `claim.expired` facts with no
//! actor.
//!
//! The five exceptions are the reason this file asserts behaviour rather than
//! percentages. Every one of them was hand-written through `rally say` (which
//! has always stamped `tool` and `from_session_id`), not produced by the
//! automatic path — so "no code path can produce an attributed wake" was false
//! as a statement about the ledger and true only of `wake_fact` and the reaper.
//! A test that asserted "100% unattributed" would have been red against real
//! data on day one.
//!
//! # What the absence cost
//!
//! An unresolved wake could be counted but not routed: with no originator on
//! the fact, nobody can be asked whether the work still matters. A reaped claim
//! could not name the process that reaped it, which is the record a contested
//! ownership dispute is decided on.
//!
//! # The coupling this file pins
//!
//! Attribution was not a free relabel. The reaper held TWO authorities that
//! rode on its own anonymity, and naming it revoked both until they were
//! re-based onto the `role: "system"` marker:
//!
//! * expired-lease claim closure was gated on `tool == "rally"`
//!   (`write_authority::is_typed_reaper_lease_expiry`);
//! * unanswered-handoff expiry passed only through the legacy
//!   `from_session_id.is_none()` arm of `store::handoff_closer_matches_target`.
//!
//! So every test here asserts attribution AND effect together. An attributed
//! reap that no longer reaps is not a fix, and the two assertions must not be
//! separable — that is the whole failure mode.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TIMEOUT_MS: &str = "20000";

/// A disposable room. The live `.rally/` room is at 98.65% of the 8 MiB wire
/// cap (O43) and is never written by a test.
struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("attribution-{name}-cwd"));
        let home = temp_path(&format!("attribution-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_NO_AUTO_REAP", "1")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("GITHUB_RUN_ID");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.rally()
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("spawn rally {args:?}: {e}"))
    }

    /// A real worktree with a commit. Observed-liveness needs a HEAD to
    /// compare against; without one the auto-reap preserves every claim and a
    /// test built on it would pass for the wrong reason.
    fn init_observed_worktree(&self) {
        let git_ok = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&self.cwd)
                .args(args)
                .output()
                .unwrap_or_else(|error| panic!("spawn git {args:?}: {error}"));
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git_ok(&["init", "--quiet"]);
        fs::write(self.cwd.join("observed.txt"), "observed\n").expect("write observed fixture");
        git_ok(&["add", "observed.txt"]);
        git_ok(&[
            "-c",
            "user.name=Rally Test",
            "-c",
            "user.email=rally-test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "observed fixture",
        ]);
    }

    fn run_ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "rally {args:?} failed: status={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        stdout_json(&out)
    }

    /// Every fact on the canonical ledger, in file order. Read from the JSONL
    /// segments rather than a projection so the assertions grade what was
    /// durably written, not what a view chose to surface.
    fn facts(&self) -> Vec<Value> {
        let log = self.cwd.join(".rally").join("log");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&log) else {
            return out;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(record) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let fact = record.get("payload").cloned().unwrap_or(record);
                out.push(fact);
            }
        }
        out
    }

    fn facts_of_kind(&self, kind: &str) -> Vec<Value> {
        self.facts()
            .into_iter()
            .filter(|f| f["kind"] == Value::String(kind.to_string()))
            .collect()
    }

    fn active_claim_count(&self) -> usize {
        let body = self.run_ok(&["room", "--json", "--timeout-ms", TIMEOUT_MS]);
        body["data"]["room"]["active_claims"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    }

    fn squad_tools(&self) -> Vec<String> {
        let body = self.run_ok(&["room", "--json", "--timeout-ms", TIMEOUT_MS]);
        body["data"]["room"]["squads"]
            .as_array()
            .map(|squads| {
                squads
                    .iter()
                    .filter_map(|s| s["tool"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "{name}-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn stdout_json(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("stdout must be JSON: {err}\nstdout={stdout}"))
}

/// The question every assertion in this file is really asking: can this fact
/// say who caused it?
fn names_an_actor(fact: &Value) -> bool {
    let tool = fact["tool"].as_str().unwrap_or("");
    let session = fact["from_session_id"].as_str().unwrap_or("");
    (!tool.is_empty() && tool != "rally") || !session.is_empty()
}

fn is_system_role(fact: &Value) -> bool {
    fact["role"] == Value::String("system".to_string())
}

/// Seed a claim whose lease has already run out, owned by `owner`.
fn seed_expired_claim(room: &TempRoom, owner: &str, subject: &str, scope: &str) {
    let out = room
        .rally()
        .env("RALLY_SESSION_ID", "owner-session")
        .args([
            "say",
            "claim",
            "--tool",
            owner,
            "--subject",
            subject,
            "--scope",
            scope,
            "--evidence",
            "lease_expires_at:2000-01-01T00:00:00Z",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ])
        .output()
        .expect("spawn claim");
    assert!(
        out.status.success(),
        "seeding the expired claim must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// =============================================================================
// wake
// =============================================================================

/// FALSIFIER A, wake half. `rally next --tool X` must produce a wake fact that
/// names X.
///
/// `append_next_wake_intent` already took `tool: &str` and simply never passed
/// it to `wake_fact`, so this fails on the pre-change binary with
/// `tool == "rally"` — the actor was in scope the whole time and was dropped
/// one call frame before it was written.
#[test]
fn a_wake_intent_names_the_agent_that_caused_it() {
    let room = TempRoom::new("wake-attribution");
    room.run_ok(&[
        "enter",
        "--tool",
        "waker:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    // A handoff addressed to `waker:01` gives `next` something actionable, which
    // is the precondition for it writing a wake intent at all.
    room.run_ok(&[
        "say",
        "handoff",
        "--tool",
        "peer:01",
        "--target",
        "waker:01",
        "--subject",
        "please pick this up",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    room.run_ok(&[
        "next",
        "--tool",
        "waker:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);

    let wakes = room.facts_of_kind("wake");
    assert!(
        !wakes.is_empty(),
        "fixture must produce at least one wake fact, else this test grades nothing"
    );
    for wake in &wakes {
        assert!(
            names_an_actor(wake),
            "wake fact must name the agent that caused it, got tool={:?} from_session_id={:?}: {wake}",
            wake["tool"],
            wake["from_session_id"]
        );
        assert_eq!(
            wake["tool"],
            Value::String("waker:01".to_string()),
            "the actor is the tool that ran `rally next`, not the reserved system author: {wake}"
        );
        assert!(
            is_system_role(wake),
            "a wake is still machine-initiated; that fact moved to `role`, it was not dropped: {wake}"
        );
    }
}

/// The actor and the target are different questions and both must survive.
/// Recording only the target is the state this change replaced; recording only
/// the actor would be the same defect pointed the other way.
#[test]
fn a_wake_intent_records_actor_and_target_separately() {
    let room = TempRoom::new("wake-actor-vs-target");
    room.run_ok(&[
        "enter",
        "--tool",
        "waker:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    room.run_ok(&[
        "say",
        "handoff",
        "--tool",
        "peer:01",
        "--target",
        "waker:01",
        "--subject",
        "please pick this up",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    room.run_ok(&[
        "next",
        "--tool",
        "waker:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);

    let wakes = room.facts_of_kind("wake");
    assert!(!wakes.is_empty(), "fixture must produce a wake fact");
    for wake in &wakes {
        assert_eq!(
            wake["target"],
            Value::String("waker:01".to_string()),
            "target must still record who is being woken: {wake}"
        );
        // `!is_empty()` is not enough here: the pre-change binary wrote
        // `tool: "rally"`, which is non-empty and names nobody. Planting that
        // exact mutant left this assertion green, so it asserts the actor is a
        // real identity rather than merely present.
        assert!(
            names_an_actor(wake),
            "actor must be recorded alongside the target, and must not be the \
             anonymous reserved author: {wake}"
        );
    }
}

// =============================================================================
// reaper
// =============================================================================

/// FALSIFIER A, `claim.expired` half — and the coupling test.
///
/// An auto-reap fired by `rally enter --tool reaper-agent:01` must attribute
/// the close to `reaper-agent:01` AND still close the claim. Splitting these
/// two assertions into separate tests would let a regression that revokes the
/// reaper's authority pass one of them, which is exactly what happened while
/// this change was being written: naming the actor made
/// `is_typed_reaper_lease_expiry` reject the fact, and the reap silently
/// stopped reaping while still reporting.
#[test]
fn an_auto_reap_names_the_entering_agent_and_still_reaps() {
    let room = TempRoom::new("reap-attribution");
    room.init_observed_worktree();
    // A pid that is not running, so the owner is externally observed dead and
    // its expired claim becomes eligible under the same rule production uses.
    let owner_enter = room
        .rally()
        .env("RALLY_SESSION_ID", "owner-session")
        .env("RALLY_OBSERVER_PID", "2000000000")
        .args([
            "enter",
            "--tool",
            "owner:01",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ])
        .output()
        .expect("spawn owner enter");
    assert!(
        owner_enter.status.success(),
        "owner enter must succeed: {}",
        String::from_utf8_lossy(&owner_enter.stderr)
    );
    seed_expired_claim(&room, "owner:01", "expired work", "file:src/a.rs");
    assert_eq!(
        room.active_claim_count(),
        1,
        "fixture must start with the expired claim live"
    );

    let enter = room
        .rally()
        .env_remove("RALLY_NO_AUTO_REAP")
        .env("RALLY_AUTO_REAP_INTERVAL_SECS", "3600")
        .args([
            "enter",
            "--tool",
            "reaper-agent:01",
            "--json",
            "--timeout-ms",
            TIMEOUT_MS,
        ])
        .output()
        .expect("spawn reaping enter");
    assert!(
        enter.status.success(),
        "the reaping enter must succeed: {}",
        String::from_utf8_lossy(&enter.stderr)
    );

    let expiries = room.facts_of_kind("claim.expired");
    assert!(
        !expiries.is_empty(),
        "the auto-reap must have written a claim.expired fact; stderr={}",
        String::from_utf8_lossy(&enter.stderr)
    );
    for expiry in &expiries {
        assert_eq!(
            expiry["tool"],
            Value::String("reaper-agent:01".to_string()),
            "the claim-takeover audit trail must name the agent that reaped: {expiry}"
        );
        assert!(
            expiry["from_session_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "and the session it reaped from, so two sessions of one tool are distinguishable: {expiry}"
        );
        assert!(
            is_system_role(expiry),
            "while still recording that no human chose this: {expiry}"
        );
        // The former owner is the other half of the record and must not have
        // been displaced by the actor.
        let evidence = expiry["evidence"].as_array().cloned().unwrap_or_default();
        assert!(
            evidence
                .iter()
                .any(|e| e.as_str() == Some("reaper:owner=owner:01")),
            "the reaped claim's former owner must still be recorded: {expiry}"
        );
    }

    assert_eq!(
        room.active_claim_count(),
        0,
        "attributing the reaper must not revoke its authority to close an expired lease"
    );
}

/// The standalone operator path has no entering agent, so it attributes to the
/// invoking process instead: `tool` stays `"rally"` (nothing else would be
/// honest) but the session lease is real, which is what makes two operators
/// reaping one room distinguishable.
#[test]
fn an_operator_reap_records_the_invoking_session() {
    let room = TempRoom::new("operator-reap-attribution");
    room.run_ok(&[
        "enter",
        "--tool",
        "owner:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    seed_expired_claim(&room, "owner:01", "expired work", "file:src/a.rs");

    let out = room.run(&[
        "doctor",
        "--reap-stale",
        "--apply",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    assert!(
        out.status.success(),
        "operator reap must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let expiries = room.facts_of_kind("claim.expired");
    assert!(!expiries.is_empty(), "operator reap must write an expiry");
    for expiry in &expiries {
        assert!(
            names_an_actor(expiry),
            "an operator reap must still be attributable to its invocation: {expiry}"
        );
        assert!(
            expiry["from_session_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "the invoking process's session lease is the attribution here: {expiry}"
        );
        assert!(is_system_role(expiry), "still machine-initiated: {expiry}");
    }
    assert_eq!(
        room.active_claim_count(),
        0,
        "the operator path must still close the expired claim"
    );
}

// =============================================================================
// the exclusion that makes attribution safe
// =============================================================================

/// Naming the actor must not enrol it as a participating agent.
///
/// `squads[]` excluded system writes by testing `tool == "rally"`, which worked
/// only while such writes had no real actor to enrol. If that exclusion had not
/// moved to the `role` marker, an agent's first `rally enter` would have added
/// every tool it touched during the reap to the roster — the projection would
/// have started reporting agents that were never in the room.
#[test]
fn a_system_write_does_not_enrol_its_actor_as_a_participating_agent() {
    let room = TempRoom::new("system-write-not-a-squad");
    room.run_ok(&[
        "enter",
        "--tool",
        "owner:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    seed_expired_claim(&room, "owner:01", "expired work", "file:src/a.rs");

    // The operator reap writes a system-authored fact whose actor label is
    // `rally`; no agent by that name ever entered the room.
    room.run(&[
        "doctor",
        "--reap-stale",
        "--apply",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);

    let squads = room.squad_tools();
    assert!(
        !squads.iter().any(|t| t == "rally"),
        "the system author must never appear in squads[]: {squads:?}"
    );
    assert!(
        squads.iter().any(|t| t == "owner:01"),
        "a real participating agent must still appear: {squads:?}"
    );
}

// =============================================================================
// the reserved marker
// =============================================================================

/// A hand-written reaper close is refused, and the victim's claim survives.
///
/// # What this proves, and what it does not
///
/// Mutation-checking originally exposed an important limit: neutering the
/// marker arm of `write_authority::is_typed_reaper_lease_expiry` (the
/// `is_system_authored` call) leaves THIS integration test green. So this is
/// still not the test of that arm.
///
/// The reason is that the refusal arrives earlier, from the store's under-lock
/// re-checks: a forged `reaper:owner_session=` is rejected with "owner session
/// does not match the reaper evidence", and correcting it only reaches "no
/// longer eligible under the mutation lock (owner revived or lease renewed)".
/// Those re-read the live claim while holding the mutation lock, which is why
/// they cannot be satisfied from the command line at all.
///
/// That leaves the marker arm doing one narrow job: authorizing the LeaseOnly
/// auto-reap when a lease has expired but the owner is not YET stale-eligible.
/// `an_auto_reap_names_the_entering_agent_and_still_reaps` covers that
/// direction positively — it goes red when the reaper's attribution changes
/// without the arm being re-based, which is exactly what happened while this
/// change was being written.
///
/// The arm's negative direction is now locked by the narrow unit test
/// `write_authority::tests::an_ordinary_actor_cannot_use_an_otherwise_valid_typed_expiry`.
/// That test holds every typed marker and the expired lease constant, accepts
/// the system-authored form, then removes only the system role and requires a
/// refusal. This integration test remains valuable for the command/store
/// boundary without pretending to isolate authority it cannot reach.
#[test]
fn a_forged_reaper_close_without_the_system_marker_is_refused() {
    let room = TempRoom::new("forged-reaper-close");
    room.run_ok(&[
        "enter",
        "--tool",
        "victim:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    let claim = room.run_ok(&[
        "say",
        "claim",
        "--tool",
        "victim:01",
        "--path",
        "src/lib.rs",
        "--subject",
        "victim owns this",
        "--evidence",
        "lease_expires_at:2000-01-01T00:00:00Z",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    let claim_id = claim["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("claim event_id")
        .to_string();
    assert_eq!(room.active_claim_count(), 1, "fixture claim must be live");

    let ref_marker = format!("reaper:ref_id={claim_id}");
    let forged = room.run(&[
        "say",
        "claim.expired",
        "--tool",
        "rogue:01",
        "--ref",
        &claim_id,
        "--subject",
        "forged reaper close",
        "--evidence",
        &ref_marker,
        "--evidence",
        "reaper:reason=lease-expired",
        "--evidence",
        "reaper:observed=stale",
        "--evidence",
        "reaper:owner=victim:01",
        "--evidence",
        "reaper:owner_session=legacy",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    assert!(
        !forged.status.success(),
        "hand-written reaper evidence must not confer reaper authority, got exit {:?}",
        forged.status.code()
    );
    assert_eq!(
        room.active_claim_count(),
        1,
        "the victim's claim must survive a forged close"
    );

    // The other spelling of the same authority. Reserving only the role would
    // have left this open and the whole move would be lateral.
    let impersonated = room.run(&[
        "say",
        "claim.expired",
        "--tool",
        "rally",
        "--ref",
        &claim_id,
        "--subject",
        "impersonating the system author",
        "--evidence",
        &ref_marker,
        "--evidence",
        "reaper:reason=lease-expired",
        "--evidence",
        "reaper:observed=stale",
        "--evidence",
        "reaper:owner=victim:01",
        "--evidence",
        "reaper:owner_session=legacy",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    assert!(
        !impersonated.status.success(),
        "the reserved system author must not be usable by hand, got exit {:?}",
        impersonated.status.code()
    );
    assert_eq!(
        room.active_claim_count(),
        1,
        "the victim's claim must survive an impersonated system close"
    );
}

/// Both authority markers are refused at the `say` boundary.
///
/// Asserted on a plain `decision` fact with no `--ref`, deliberately: that
/// reaches no claim gate, no lease check, and no under-lock re-validation, so
/// the only thing that can refuse it is the reserved-marker guard. The
/// forged-close test above cannot make that isolation, because the store
/// refuses it for other reasons first.
///
/// Mutation-checked in both directions: disabling either guard turns this red.
#[test]
fn the_reserved_authority_markers_cannot_be_claimed_by_hand() {
    let room = TempRoom::new("reserved-system-role");
    room.run_ok(&[
        "enter",
        "--tool",
        "impostor:01",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);

    let forged_role = room.run(&[
        "say",
        "decision",
        "--tool",
        "impostor:01",
        "--role",
        "system",
        "--subject",
        "pretending to be the machine",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    assert!(
        !forged_role.status.success(),
        "a hand-set system role must be refused, got exit {:?}",
        forged_role.status.code()
    );

    // The other spelling of the same authority. `is_typed_reaper_lease_expiry`
    // accepts EITHER marker, so reserving only the role would leave the gate
    // exactly as forgeable as it was before and the move would be lateral.
    let forged_author = room.run(&[
        "say",
        "decision",
        "--tool",
        "rally",
        "--subject",
        "impersonating the system author",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
    assert!(
        !forged_author.status.success(),
        "the reserved system author must be refused, got exit {:?}",
        forged_author.status.code()
    );

    // An ordinary role is unaffected — the guard is a reserved word, not a ban
    // on the field.
    room.run_ok(&[
        "say",
        "decision",
        "--tool",
        "impostor:01",
        "--role",
        "reviewer",
        "--subject",
        "an ordinary role still works",
        "--json",
        "--timeout-ms",
        TIMEOUT_MS,
    ]);
}
