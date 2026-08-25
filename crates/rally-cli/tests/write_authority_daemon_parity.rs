// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Direct-vs-routed parity for every write-boundary authority gate.
//!
//! # Why this file exists
//!
//! Every live reproduction in this security cycle ran in DIRECT mode. The
//! daemon path (`RoomStore::Routed`) was never exercised, so "the gate holds"
//! was a statement about one of two ways rally writes.
//!
//! That gap is not hypothetical here. The design audit found three snapshot
//! fields (`content_max_seq`, `pending_wakes`, `stale_authors`) marked
//! `#[serde(skip)]`, so they arrive EMPTY over the daemon wire — silently
//! changing relevance ranking, read-checkpoint writing, and wake coalescing
//! depending on whether rallyd happens to be running. A gate placed in the
//! client would have exactly that shape: enforced when the CLI writes, absent
//! when the daemon does.
//!
//! `write_authority::assert_write_authorized` is called from
//! `DirectRoomStore::append_fact`, which `rallyd_core::run_op` also calls for
//! `AppendFact`, `AppendFactVerified`, and `AppendStateTransitionVerified`. So
//! the gate SHOULD hold on both. This file asserts it rather than reasoning
//! about it, because reading the code is how the two-of-four claim-close gap
//! (ARP-R-02) survived review: the reasoning was sound and the coverage was not.
//!
//! Each test runs the SAME hostile sequence twice — once against a room with no
//! daemon, once with `rally daemon serve` live — and asserts the outcomes match.
//!
//! # What this file does NOT prove
//!
//! Parity grades EQUIVALENCE, not EXISTENCE. Mutation-validated: neutering the
//! claim-close gate or the lead-transfer gate leaves every test here GREEN,
//! because removing a gate removes it from both modes and the two still agree.
//! `claim_takeover_authz.rs` and `lead_seat_authz.rs` are what prove the gates
//! exist; this file proves rallyd cannot be used to step around them. Neither
//! question is answered by the other suite, and reading only one of them is how
//! a reviewer concludes more than the evidence supports.
//!
//! # What this file refuses to grade
//!
//! An authorization verdict and a lost race are different facts, and until
//! 2026-08-24 this file scored them the same way. Rally answers a refused write
//! and a write that never committed with the SAME envelope shape —
//! `{"ok": false, "command": "partial_commit"}` — so `ok()` returning `false`
//! meant either "the gate said no" or "the host was too busy for the write to
//! happen". Under the pre-push gate's parallel workspace build the second case
//! fired routinely, and the suite reported it as the first: an
//! authorization-parity failure, in the file whose entire purpose is to be
//! believed about exactly that. A blocking gate that fails for a reason its own
//! message disclaims is training to retry until green, which is the state in
//! which a real regression gets waved through.
//!
//! So the timing budget is now separated from the observation window
//! ([`WATCHDOG_BUDGET_MS`]), and every envelope is screened before it is scored
//! ([`timing_defect`]). A run that loses the race aborts as a TEST SETUP DEFECT
//! naming the resource condition, and never reaches the parity comparison.
//! Mutation-validated 2026-08-24: with the authority gate stripped from the
//! routed path only, three tests still fail on genuine parity assertions and
//! zero are misreported as setup defects, so the screen does not swallow the
//! divergence it sits in front of.
//!
//! Re-validated 2026-08-25, and the second pass corrects an inference the
//! paragraph above invites. Those three tests are `lead-seat`,
//! `lead-seat-retract` and `claim-close`. `reaper-lease-expiry` — the case whose
//! flakiness prompted the fix — is NOT among them, and it stays green when
//! `assert_write_authorized` is stripped from the routed path. That is not
//! insensitivity in the test; the reaper write is refused by THREE independent
//! store-side guards, and removing any one or two of them leaves the other(s)
//! refusing:
//!
//! 1. `append_fact_under_lock`'s reaper owner-session check (`store.rs`,
//!    "owner session does not match the reaper evidence"),
//! 2. the same function's under-lock eligibility re-check ("no longer eligible
//!    under the mutation lock (owner revived or lease renewed)"),
//! 3. `assert_write_authorized` itself, via `needs_authority_check`.
//!
//! All three had to be stripped from the routed path at once before
//! `reaper-lease-expiry` reported the divergence — which it then did, on a
//! genuine parity assertion (`left: (false, true, 0)` vs `right: (true, true,
//! 0)`: direct refused the forged reaper, routed accepted it), with zero setup
//! defects, 5/5 under load average climbing 40 → 321 on 16 cores. So the screen
//! does not mask a real divergence on this case either, under load or at rest.
//! Anyone shortening this to "the reaper case is mutation-covered by stripping
//! the authority gate" will measure the wrong thing and conclude the test is
//! blind when it is layered.
//!
//! # Why the field-bound payload is 8 KiB
//!
//! `field_bounds_are_identical_in_direct_and_routed_mode` uses an 8 KiB
//! subject: large enough to exceed ARP-R-04's 4 KiB subject bound, but small
//! enough for every supported host to pass it to the CLI as one argument.
//! The older 200 KB fixture exceeded Linux's per-argument limit before rally
//! could inspect it, and Rally's current 8 MiB daemon-frame limit means that
//! size no longer proves the historical transport divergence anyway. This
//! suite proves direct/routed equivalence; `retrospective_sanitizer.rs` proves
//! the field bound exists and independently exercises renderer volume limits.

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

/// The wall-clock budget handed to every `rally` command this file runs.
///
/// # Why this exists, and why it is not a widened sleep
///
/// Rally bounds EVERY command with a hook-safety watchdog that fails CLOSED at
/// `DEFAULT_WATCHDOG_TIMEOUT_MS` (3s, `rally-cli/src/lib.rs`). That budget is a
/// property of the PRODUCT — it stops an agent write-hook from wedging on stuck
/// I/O — and it already has its own suites: `watchdog_timeout.rs`,
/// `retry_budget_watchdog.rs`, `watchdog_write_durability.rs`.
///
/// This file grades a different question: does an authority gate return the SAME
/// verdict through the CLI and through rallyd. Leaving a 3s wall-clock budget
/// inside that observation makes host load a hidden input to an authorization
/// verdict. Measured 2026-08-24 on a 16-core host: run this binary alone and it
/// passes 6/6 in ~4.6s; run it under the pre-push gate's parallel workspace
/// build (load average ~280) and it fails 4/4 — `say claim` comes back
/// `partial_commit` with `mutation-not-started: deadline elapsed before
/// acquiring …/mutation.lock`, and `doctor --reap-stale` comes back
/// `watchdog-timeout-uncommitted-mutation`. Neither is an authorization result.
/// Both were being scored as one.
///
/// 60_000 is `MAX_WATCHDOG_TIMEOUT_MS`, the ceiling rally clamps the override
/// to. Raising it costs nothing in wall-clock: nothing in this file sleeps on
/// this value and no test waits for it to elapse. It only stops a contended host
/// from converting a slow write into a fake refusal. The budget is separated
/// from the observation window, not widened inside it.
const WATCHDOG_BUDGET_MS: &str = "60000";

/// Stable markers for "this command lost a race with the host and never reached
/// an authority decision".
///
/// Every entry is a literal from rally's resource/timing paths, never from an
/// authority path:
///
/// * `watchdog-timeout` — the prefix shared by all four fail-closed watchdog
///   error codes (`lib.rs`: `…-fail-closed`, `…-uncommitted-mutation`,
///   `…-outcome-unknown`, `…-db-only-migration-outcome-unknown`).
/// * `mutation-not-started:` — `store.rs` / `store_client.rs` / `rallyd_core.rs`
///   emit this when a deadline elapsed before any durable mutation began.
/// * `mutation-outcome-unknown:` — `error.rs`, the ambiguous sibling.
/// * `retry budget exhausted` — `store.rs`, the contended-database retry ceiling.
/// * `pool timed out …` — `store.rs`, sqlx connection-pool starvation.
///
/// This set is deliberately CONSERVATIVE, and the asymmetry matters. A neutered
/// gate answers `ok: true`, which carries no marker at all, so nothing added
/// here can ever hide a missing gate — but anything added here that is NOT
/// purely a timing condition would start discarding real refusals. When in
/// doubt, leave a marker out and let the verdict be graded.
const TIMING_MARKERS: [&str; 5] = [
    "watchdog-timeout",
    "mutation-not-started:",
    "mutation-outcome-unknown:",
    "retry budget exhausted",
    "pool timed out while waiting for an open connection",
];

/// Collect every diagnostic string in a rally envelope: the values of `code` and
/// `message` keys, at any depth.
///
/// Keyed rather than scanning the whole serialized envelope on purpose. This
/// file deliberately feeds rally hostile payloads — an 8 KiB subject, a tool id
/// carrying `\n## FORGED HEADING` — and those land under `subject` / `tool`. A
/// blind substring search over the envelope would let a caller-supplied string
/// decide whether the run counts as a setup defect.
fn collect_diagnostics(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (key, val) in map {
                if (key == "code" || key == "message")
                    && let Some(s) = val.as_str()
                {
                    out.push(s.to_string());
                }
                collect_diagnostics(val, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_diagnostics(item, out);
            }
        }
        _ => {}
    }
}

/// `Some(diagnostic)` when this envelope is a resource/timing outcome rather
/// than an authority verdict.
///
/// This is the discriminator the file was missing. A genuine refusal and a
/// watchdog expiry are INDISTINGUISHABLE at the top level — both answer
/// `{"ok": false, "command": "partial_commit"}`. The refusal says
/// `reap refused: … owner session does not match the reaper evidence`; the
/// expiry says `mutation-not-started: deadline elapsed …`. Only the diagnostic
/// text separates them, so only the diagnostic text can be trusted to.
fn timing_defect(v: &Value) -> Option<String> {
    let mut diagnostics = Vec::new();
    collect_diagnostics(v, &mut diagnostics);
    diagnostics
        .into_iter()
        .find(|d| TIMING_MARKERS.iter().any(|marker| d.contains(marker)))
}

/// Abort with the diagnosis the failure actually has.
///
/// The whole point of this function is the first line of its message. A failure
/// here is a broken PRECONDITION — the write under test never happened — and
/// reporting it under a test named `…authorization_is_identical…` is how a
/// saturated CI host teaches a reviewer to retry until green, which is how a
/// real regression eventually gets waved through.
fn setup_defect(args: &[&str], reason: &str, envelope: &Value) -> ! {
    panic!(
        "TEST SETUP DEFECT — this is NOT an authorization-parity failure.\n\
         `rally {}` never reached an authority decision. It returned a \
         resource/timing outcome:\n    {reason}\n\
         This file grades whether a gate answers the same through the CLI and \
         through rallyd. It cannot grade a write that never committed, so the \
         run is reported as a broken precondition instead of being scored as a \
         refusal.\n\
         The command budget is already {WATCHDOG_BUDGET_MS}ms — rally's own \
         ceiling — so reaching this means the host is saturated far past what \
         this suite is meant to tolerate, or a genuine contention regression \
         exists. Do NOT widen a sleep and do NOT retry until green.\n\
         full envelope: {envelope}",
        args.join(" "),
    )
}

struct Room {
    cwd: PathBuf,
    home: PathBuf,
    session_id: String,
}

impl Room {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("wadp-{name}-{nanos}-cwd"));
        let home = std::env::temp_dir().join(format!("wadp-{name}-{nanos}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".rally")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            cwd,
            home,
            session_id: format!("wadp-{name}-{nanos}"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_SESSION_ID", &self.session_id)
            // Take the product's 3s wall-clock budget out of the observation
            // window. See `WATCHDOG_BUDGET_MS`.
            .env("RALLY_HOOK_TIMEOUT_MS", WATCHDOG_BUDGET_MS)
            .args(args)
            .output()
            .unwrap()
    }

    /// Run `args` and return the envelope, having first screened it for
    /// resource/timing failure.
    ///
    /// The screen lives HERE, at the single door every observation in this file
    /// passes through, so no future call site can accidentally score a
    /// never-committed write as a verdict.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        let body = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let v: Value = serde_json::from_slice(body).unwrap_or_else(|e| {
            panic!(
                "cmd {args:?} did not emit JSON ({e})\nstderr: {}\nstdout: {}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            )
        });
        if let Some(reason) = timing_defect(&v) {
            setup_defect(args, &reason, &v);
        }
        v
    }

    fn ok(&self, args: &[&str]) -> bool {
        self.json(args)["ok"] == Value::Bool(true)
    }

    /// Mint a claim owned by `tool` over `path`, carrying any extra `evidence`.
    ///
    /// Both halves of the precondition are asserted here — the command
    /// succeeded, AND it handed back an event id — because every caller needs
    /// the claim to exist before the behaviour under test begins. The reaper
    /// case used to inline this and read `…["event_id"].as_str().expect("claim
    /// event_id")`, so a claim that never committed surfaced as a three-word
    /// panic inside a test whose name promises a statement about authorization.
    fn claim_with(&self, tool: &str, path: &str, evidence: &[&str]) -> String {
        let mut args = vec![
            "say",
            "claim",
            "--tool",
            tool,
            "--path",
            path,
            "--subject",
            "owns it",
        ];
        for item in evidence {
            args.push("--evidence");
            args.push(item);
        }
        args.push("--json");

        let v = self.json(&args);
        assert_eq!(
            v["ok"],
            Value::Bool(true),
            "TEST SETUP DEFECT: the claim this scenario is built on was refused, so nothing \
             downstream grades authorization parity: {v}"
        );
        v["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "TEST SETUP DEFECT: `say claim` reported success but projected no \
                     event_id, so the scenario has no claim to act on: {v}"
                )
            })
            .to_string()
    }

    fn claim(&self, tool: &str, path: &str) -> String {
        self.claim_with(tool, path, &[])
    }

    /// How many claims are standing, or a setup defect if the room did not
    /// project an answer.
    ///
    /// The absent case is NOT zero. This read used to end in `.unwrap_or(0)`,
    /// which silently reported a room that failed to project as a room holding
    /// no claims — and since this count is compared across the two store modes,
    /// a single degraded read surfaced as `left: (false, false, 0)` vs
    /// `right: (false, false, 1)`: a direct-vs-routed divergence that never
    /// happened, reported in the file whose entire job is to be believed about
    /// exactly that.
    fn active_claim_count(&self) -> usize {
        let v = self.json(&["room", "--json"]);
        v["data"]["room"]["active_claims"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_else(|| {
                setup_defect(
                    &["room", "--json"],
                    "the room projected no `active_claims` array, so the claim count is \
                     unknown — which is not the same as zero",
                    &v,
                )
            })
    }

    fn lead(&self) -> Option<String> {
        self.json(&["room", "--json"])["data"]["room"]["lead"]
            .as_str()
            .map(str::to_string)
    }

    /// The highest-seq `role:lead` decision — the fact the seat is derived
    /// from, read out of the projection the same way the gate derives it.
    fn seat_decision_id(&self) -> String {
        self.json(&["room", "--json"])["data"]["room"]["current_decisions"]
            .as_array()
            .expect("current_decisions is an array")
            .iter()
            .filter(|d| d["subject"].as_str() == Some("role:lead"))
            .max_by_key(|d| d["seq"].as_i64().unwrap_or(0))
            .and_then(|d| d["event_id"].as_str())
            .expect(
                "TEST SETUP DEFECT: the room carries no seated lead decision, so the retraction \
                 scenario has no seat to aim at",
            )
            .to_string()
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

/// A live `rally daemon serve` bound to a room. SIGTERM + reap on Drop.
struct Daemon {
    child: Child,
    stopped: bool,
}

impl Daemon {
    /// Start and block until the daemon answers a ping. Returns `None` when the
    /// daemon does not come up within the corridor — the caller then SKIPS
    /// rather than passing, because a test that silently degrades to the direct
    /// path while claiming to cover the routed one is worse than no test.
    fn start(room: &Room) -> Option<Self> {
        let log = fs::File::create(room.cwd.join(".rally").join("rallyd-serve.log")).ok()?;
        let child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .ok()?;
        let mut d = Daemon {
            child,
            stopped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if room.cwd.join(".rally").join("rallyd.sock.addr").exists()
                && room.run(&["daemon", "status", "--json"]).status.success()
            {
                return Some(d);
            }
            thread::sleep(Duration::from_millis(100));
        }
        d.stop();
        None
    }

    /// Confirm the daemon is actually SERVING this room's writes. Without this
    /// the whole file could pass while exercising the direct path twice.
    fn assert_serving(&self, room: &Room) {
        let v = room.json(&["daemon", "status", "--json"]);
        let text = v.to_string();
        assert!(
            text.contains("\"serving\"")
                || text.contains("\"running\"")
                || v["ok"] == Value::Bool(true),
            "daemon must be live before parity is claimed: {v}"
        );
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
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run `scenario` against a room with NO daemon and against a room WITH one,
/// and require the same verdict from both.
///
/// `scenario` returns whatever the caller wants compared; it must be derived
/// from observable CLI output only.
fn assert_parity<T: std::fmt::Debug + PartialEq>(
    name: &str,
    scenario: impl Fn(&Room) -> T,
) -> Option<T> {
    let direct_room = Room::new(&format!("{name}-direct"));
    let direct = scenario(&direct_room);

    let routed_room = Room::new(&format!("{name}-routed"));
    let Some(daemon) = Daemon::start(&routed_room) else {
        eprintln!(
            "SKIP {name}: `rally daemon serve` did not come up; routed-path parity NOT asserted"
        );
        return None;
    };
    daemon.assert_serving(&routed_room);
    let routed = scenario(&routed_room);
    drop(daemon);

    assert_eq!(
        direct, routed,
        "{name}: the write-boundary gate must give the same verdict in direct and \
         routed mode. A gate that holds only when rallyd is stopped is not a gate — \
         see the module doc for the three snapshot fields that already diverge here."
    );
    Some(direct)
}

/// ARP-R-02, all four closing kinds, on both store modes.
#[test]
fn claim_close_authorization_is_identical_in_direct_and_routed_mode() {
    assert_parity("claim-close", |room| {
        let cid = room.claim("victim", "src/lib.rs");
        let mut verdicts = Vec::new();
        for kind in [
            "resolve",
            "release",
            "receipt",
            "claim.expired",
            "claim_expired",
        ] {
            verdicts.push((
                kind,
                room.ok(&[
                    "say",
                    kind,
                    "--tool",
                    "rogue",
                    "--ref",
                    &cid,
                    "--subject",
                    "mine now",
                    "--json",
                ]),
            ));
        }
        // The claim must still be standing, and still be victim's.
        verdicts.push(("claim_survives", room.active_claim_count() == 1));
        verdicts
    });
}

/// The reaper's expired-lease authority, on both store modes.
///
/// This case is not covered by `claim_close_authorization_...` above, which
/// grades a rogue being REFUSED. This one grades the reaper being ALLOWED, and
/// the two can diverge independently: the arm that authorizes it
/// (`write_authority::is_typed_reaper_lease_expiry`) moved from keying on
/// `tool == "rally"` to keying on the `role: "system"` marker, and `role`
/// travels to the daemon as a serialized payload field rather than being
/// re-derived there. A gate that read an actor label the client computed but
/// the wire dropped would authorize in one mode and refuse in the other — the
/// exact shape this file exists to catch.
///
/// Both halves are compared: the verdict AND whether the claim actually left
/// the room. An "authorized" that does not close is not parity.
#[test]
fn reaper_lease_expiry_authorization_is_identical_in_direct_and_routed_mode() {
    assert_parity("reaper-lease-expiry", |room| {
        // A claim whose lease has already run out, owned by someone else.
        let cid = room.claim_with(
            "victim",
            "src/lib.rs",
            &["lease_expires_at:2000-01-01T00:00:00Z"],
        );

        // A hand-built ClaimExpired carrying the typed reaper evidence but NO
        // system role. It must be refused in both modes: the evidence set is
        // forgeable, the role marker is not (`say` reserves it), and that is
        // the whole reason the authority was moved onto the marker.
        let forged = room.ok(&[
            "say",
            "claim.expired",
            "--tool",
            "rogue",
            "--ref",
            &cid,
            "--subject",
            "forged reaper close",
            "--evidence",
            &format!("reaper:ref_id={cid}"),
            "--evidence",
            "reaper:reason=lease-expired",
            "--evidence",
            "reaper:observed=stale",
            "--evidence",
            "reaper:owner=victim",
            "--evidence",
            "reaper:owner_session=legacy",
            "--json",
        ]);

        // The genuine operator reaper, which mints the system role internally.
        let reaped = room.ok(&["doctor", "--reap-stale", "--apply", "--json"]);

        (forged, reaped, room.active_claim_count())
    });
}

/// ARP-R-01 on both store modes.
#[test]
fn lead_seat_authorization_is_identical_in_direct_and_routed_mode() {
    assert_parity("lead-seat", |room| {
        assert!(room.ok(&[
            "lead", "assign", "--tool", "victim", "--to", "victim", "--json"
        ]));
        let seize = room.ok(&[
            "lead", "assign", "--tool", "rogue", "--to", "rogue", "--json",
        ]);
        let vacate = room.ok(&["lead", "relinquish", "--tool", "rogue", "--json"]);
        let handoff = room.ok(&[
            "lead", "handoff", "--tool", "victim", "--to", "helper", "--json",
        ]);
        (seize, vacate, handoff, room.lead())
    });
}

/// RC-071a on both store modes.
///
/// This one is not covered by the lead-transfer case above even though both
/// move the same seat. A retraction is neither a claim-closing kind nor a lead
/// decision, so it reaches the gate through a THIRD selector
/// (`retraction::target_of`) — and "the seat is gated" was true of the transfer
/// spelling while being false of this one, which is the whole of RC-071a. A
/// selector that ran only in the client process would leave the seat movable
/// through rallyd.
#[test]
fn lead_seat_retraction_authorization_is_identical_in_direct_and_routed_mode() {
    assert_parity("lead-seat-retract", |room| {
        // First frontier to enter takes the open seat.
        assert!(room.ok(&["enter", "--tool", "victim", "--json"]));
        let seat = room.seat_decision_id();
        let seized = room.ok(&[
            "retract", &seat, "--tool", "rogue", "--reason", "take it", "--json",
        ]);
        let withdrawn = room.ok(&[
            "retract",
            &seat,
            "--tool",
            "victim",
            "--reason",
            "assigned in error",
            "--json",
        ]);
        (seized, withdrawn, room.lead())
    });
}

/// ARP-R-04 field bounds on both store modes. A bound enforced only when the
/// daemon is stopped is a bound a writer can step around by starting it.
#[test]
fn field_bounds_are_identical_in_direct_and_routed_mode() {
    assert_parity("field-bounds", |room| {
        let oversize_subject = "A".repeat(8_192);
        let oversize = room.ok(&[
            "say",
            "artifact",
            "--tool",
            "alpha",
            "--subject",
            &oversize_subject,
            "--json",
        ]);
        let newline_id = room.ok(&[
            "say",
            "artifact",
            "--tool",
            "atk\n## FORGED HEADING",
            "--subject",
            "hello",
            "--json",
        ]);
        let benign = room.ok(&[
            "say",
            "artifact",
            "--tool",
            "alpha",
            "--subject",
            "an ordinary note",
            "--json",
        ]);
        (oversize, newline_id, benign)
    });
}

/// The parity harness itself must be able to fail. If `Daemon::start` silently
/// returned a dead handle, every test above would compare the direct path with
/// itself and pass. This asserts the daemon is genuinely serving.
#[test]
fn the_daemon_fixture_actually_serves_before_parity_is_claimed() {
    let room = Room::new("fixture-selfcheck");
    let Some(daemon) = Daemon::start(&room) else {
        eprintln!("SKIP: daemon did not start; parity tests will skip too");
        return;
    };
    daemon.assert_serving(&room);
    // A routed write must land and be readable back through the daemon.
    let cid = room.claim("alpha", "src/only.rs");
    assert_eq!(room.active_claim_count(), 1, "routed append must land");
    assert!(
        room.ok(&[
            "say",
            "release",
            "--tool",
            "alpha",
            "--ref",
            &cid,
            "--subject",
            "done",
            "--json"
        ]),
        "routed self-release must be authorized"
    );
    assert_eq!(room.active_claim_count(), 0, "routed close must project");
    drop(daemon);
    let _ = Path::new(".");
}
