// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end cover for handoff DELIVERY: send-time liveness + fallback, and
//! the after-the-fact `rally handoffs --undelivered` audit.
//!
//! # The fixture is real evidence, not a shape
//!
//! `fixtures/undelivered-handoff-ledger.jsonl` is 57 events lifted verbatim
//! from the `ross-labs-astro` room (2026-09-13), scrubbed of home paths,
//! hostnames and addresses and renumbered contiguously. Nothing else was
//! edited — subjects, authors, targets, `created_at` and event ordering are the
//! ones the real agents wrote. It carries three targeted handoffs whose
//! outcomes differ, which is what makes it a test rather than a demo:
//!
//! | fixture seq | event id | from → to | outcome |
//! |---|---|---|---|
//! | 55 | `fact_2b7b_18d5033b70b1b380` | `codex:sol-v6` → `codex:motion-review` | **never picked up** |
//! | 8 | `fact_12117_18d4fd9e64231f48` | `codex:clarity-v5` → `claude:clarity-review` | delivered |
//! | 27 | `fact_4a22_18d4feacdf7b4f80` | `claude:clarity-review` → `codex:clarity-v5` | delivered |
//! | 30 | `fact_789d_18d4feb8bf759fe0` | `claude:clarity-review` → `codex:clarity-v5` | delivered |
//!
//! (Fixture seq 21 is an untargeted broadcast handoff and has no addressee
//! whose silence could be measured.)
//!
//! The undelivered one is the incident: its target registered at 22:20:01Z,
//! wrote its last fact at 22:20:40Z, and the handoff arrived at 22:53:00Z. It
//! sat unread for over an hour and nothing told anyone.
//!
//! A view that simply returned every handoff would pass a one-case fixture. It
//! fails this one, because `must_not_list_a_handoff_the_target_answered` holds
//! the delivered pair out.
//!
//! Loaded as raw JSONL rather than a copied `facts.db`: the DB is a derived
//! cache `RoomStore` rebuilds from the log (`docs/RALLY_ARCHITECTURE.md`), so
//! JSONL is both the repo's fixture idiom and the smaller committed artifact.

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use support::rally_cmd::rally_command;

/// The real handoff that went nowhere. Present verbatim in the fixture.
const UNDELIVERED_EVENT_ID: &str = "fact_2b7b_18d5033b70b1b380";
/// Its target — registered once, then silent.
const DEAD_TARGET: &str = "codex:motion-review";
/// A handoff from the same fixture whose target DID act afterwards: fixture
/// seq 8, discharged by `codex:clarity-v5`'s own later receipt and release.
const DELIVERED_EVENT_ID: &str = "fact_12117_18d4fd9e64231f48";
/// Two more delivered handoffs, addressed the other way. These are the rows the
/// consumption index actually decides, so they are the ones a broken filter
/// would leak.
const DELIVERED_REPLY_IDS: [&str; 2] = ["fact_4a22_18d4feacdf7b4f80", "fact_789d_18d4feb8bf759fe0"];

static NONCE: AtomicUsize = AtomicUsize::new(0);

struct Room {
    cwd: PathBuf,
    home: PathBuf,
}

impl Room {
    /// A disposable room seeded with the fixture ledger.
    fn new(name: &str) -> Self {
        let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "rally-handoff-delivery-{name}-{}-{stamp}-{nonce}",
            std::process::id()
        ));
        let cwd = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(cwd.join(".rally/log")).expect("create room");
        fs::create_dir_all(&home).expect("create home");
        // A stub `.git` is enough for repo-root discovery and keeps every git
        // scope env var pointed away from the real checkout.
        fs::create_dir_all(cwd.join(".git")).expect("create stub git");

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/undelivered-handoff-ledger.jsonl");
        fs::copy(&fixture, cwd.join(".rally/log/fixture.jsonl")).expect("seed fixture ledger");

        Self { cwd, home }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = rally_command();
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .env("RALLY_GLOBAL_INDEX", "0")
            .env("RALLY_NO_AUTO_REAP", "1")
            .env("RALLY_ENGAGEMENT", "handoff-delivery-fixture")
            .env_remove("GITHUB_ACTIONS")
            .args(args);
        cmd
    }

    /// Run and parse the JSON envelope, failing loudly with both streams.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.command(args).output().expect("spawn rally");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "rally {args:?} failed: status={:?}\nstdout={stdout}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("rally {args:?} emitted non-JSON ({error}): {stdout}"))
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        if let Some(base) = self.cwd.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn rows(envelope: &Value) -> &Vec<Value> {
    envelope["data"]["handoffs"]["rows"]
        .as_array()
        .expect("handoffs.rows array")
}

fn event_ids(envelope: &Value) -> Vec<String> {
    rows(envelope)
        .iter()
        .map(|row| row["event_id"].as_str().expect("event_id").to_string())
        .collect()
}

// =============================================================================
// `rally handoffs --undelivered`
// =============================================================================

/// The incident, replayed. This is the regression that matters: if this row
/// stops appearing, a handoff can again go unread for an hour with nobody told.
#[test]
fn lists_the_real_handoff_that_never_reached_its_target() {
    let room = Room::new("lists-real");
    let envelope = room.json(&["handoffs", "--undelivered", "--json"]);
    let ids = event_ids(&envelope);
    assert!(
        ids.iter().any(|id| id == UNDELIVERED_EVENT_ID),
        "the 22:53:00Z handoff to {DEAD_TARGET} must be listed as undelivered; got {ids:?}"
    );

    let row = rows(&envelope)
        .iter()
        .find(|row| row["event_id"] == UNDELIVERED_EVENT_ID)
        .expect("the undelivered row");
    assert_eq!(row["target"], DEAD_TARGET);
    assert_eq!(row["from"], "codex:sol-v6");
    assert_eq!(row["undelivered"], true);
    assert_eq!(
        row["created_at"], "2026-09-13T22:53:00Z",
        "age must come from the fact's own created_at, not the ledger's \
         replay-stamped occurred_at"
    );
    // The target DID exist here — it registered and then went quiet. That is a
    // different operator story from a target that was never in the room, so the
    // view has to tell them apart.
    assert_eq!(row["target_ever_seen"], true);
    assert_eq!(
        row["target_last_seen"], "2026-09-13T22:20:01Z",
        "last presence must be the 22:20:01Z registration, 32 minutes BEFORE the handoff"
    );
    assert!(
        row["age_secs"].as_i64().expect("age_secs") > 0,
        "a handoff from the past must have a positive age"
    );
}

/// Precision, not just recall. A view that returned every handoff would satisfy
/// the test above and be useless.
#[test]
fn must_not_list_a_handoff_the_target_answered() {
    let room = Room::new("precision");
    let ids = event_ids(&room.json(&["handoffs", "--undelivered", "--json"]));
    assert!(
        !ids.iter().any(|id| id == DELIVERED_EVENT_ID),
        "{DELIVERED_EVENT_ID}'s target acted after it; it is delivered, not undelivered. got {ids:?}"
    );
    for delivered in DELIVERED_REPLY_IDS {
        assert!(
            !ids.iter().any(|id| id == delivered),
            "{delivered} was answered by its target; it must not be listed. got {ids:?}"
        );
    }
    // The fixture holds four TARGETED handoffs and exactly one is unread. An
    // implementation that returned every handoff would satisfy the recall test
    // and fail here.
    assert_eq!(
        ids,
        vec![UNDELIVERED_EVENT_ID.to_string()],
        "exactly one of the fixture's four targeted handoffs is unread"
    );
}

/// Without the flag the view is a full census, and the counts let a reader tell
/// "3 rows because there are 3" from "3 rows because I asked for 3".
#[test]
fn reports_totals_separately_from_the_returned_page() {
    let room = Room::new("totals");
    let envelope = room.json(&["handoffs", "--json"]);
    let payload = &envelope["data"]["handoffs"];
    let targeted = payload["targeted_total"].as_u64().expect("targeted_total");
    let undelivered = payload["undelivered_total"]
        .as_u64()
        .expect("undelivered_total");
    assert!(
        undelivered >= 1 && undelivered < targeted,
        "fixture must contain BOTH delivered and undelivered handoffs \
         (undelivered={undelivered}, targeted={targeted})"
    );
    assert_eq!(payload["undelivered_only"], false);

    let limited = room.json(&["handoffs", "--limit", "1", "--json"]);
    assert_eq!(rows(&limited).len(), 1);
    assert_eq!(
        limited["data"]["handoffs"]["targeted_total"]
            .as_u64()
            .expect("targeted_total"),
        targeted,
        "--limit truncates the page, never the total"
    );
}

/// `room.open_handoffs` answers "is this obligation still live", which is not
/// "did anyone read it". On this fixture it lists TWO handoffs, one of which
/// the target already answered — so an agent triaging from the room sees a
/// resolved request and an unread one side by side, indistinguishable.
///
/// The converse also holds and is why this view scans the whole ledger rather
/// than the projection: measured on the full 15,139-event `ross-labs-astro`
/// room, `open_handoffs` was EMPTY while eight targeted handoffs sat
/// unconsumed. That ledger is too large to commit, so the assertion here is the
/// half this fixture can prove.
#[test]
fn distinguishes_unread_from_merely_open() {
    let room = Room::new("open-vs-unread");
    let snapshot = room.json(&["room", "--json"]);
    let open: Vec<String> = snapshot["data"]["room"]["open_handoffs"]
        .as_array()
        .expect("open_handoffs")
        .iter()
        .map(|h| h["event_id"].as_str().expect("event_id").to_string())
        .collect();
    let undelivered = event_ids(&room.json(&["handoffs", "--undelivered", "--json"]));

    assert!(
        open.len() > undelivered.len(),
        "the room projection must be the LOOSER answer here: open={open:?}, \
         undelivered={undelivered:?}"
    );
    assert!(
        open.iter().any(|id| id == UNDELIVERED_EVENT_ID)
            && undelivered.iter().any(|id| id == UNDELIVERED_EVENT_ID),
        "both views agree the genuinely-unread one is outstanding"
    );
    let answered_but_open: Vec<&String> =
        open.iter().filter(|id| !undelivered.contains(id)).collect();
    assert!(
        !answered_but_open.is_empty(),
        "the fixture must contain a handoff the room calls open and the target \
         already answered — that gap is the reason this view exists"
    );
}

// =============================================================================
// Send-time delivery
// =============================================================================

/// The send half of the same property. Every target in the fixture is long
/// stale, so any handoff addressed into it exercises the not-live path.
#[test]
fn a_handoff_to_a_dead_target_also_lands_in_the_base_inbox() {
    let room = Room::new("fallback");
    let envelope = room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        DEAD_TARGET,
        "--subject",
        "does this reach anyone",
        "--json",
    ]);
    let delivery = &envelope["data"]["delivery"];
    assert_eq!(delivery["target_live"], false);
    assert_eq!(
        delivery["fallback_inbox"], "codex",
        "a `codex:*` target falls back to the `codex` inbox a fresh session polls"
    );
    assert_eq!(delivery["reason"], "fallback_delivered");
    assert!(
        delivery["last_seen"].is_string(),
        "the warning and the JSON must both name WHEN the target was last seen"
    );

    let codes: Vec<&str> = envelope["data"]["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .map(|w| w["code"].as_str().expect("code"))
        .collect();
    assert!(
        codes.contains(&"handoff-target-not-live"),
        "the send must warn visibly, not only in JSON; got {codes:?}"
    );

    // The claim above is about JSON. This is the claim that matters: the base
    // inbox really holds it.
    let inbox = room.json(&["inbox", "--tool", "codex", "--json"]);
    let subjects: Vec<String> = inbox["data"]["inbox"]["items"]
        .as_array()
        .expect("inbox items")
        .iter()
        .map(|item| item["subject"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        subjects
            .iter()
            .any(|s| s.contains("does this reach anyone") && s.contains("fallback")),
        "the base `codex` inbox must hold the fallback copy; got {subjects:?}"
    );
}

/// The original always commits to the requested target. Fallback is an
/// ADDITION, never a redirect — a returning session must still find its own
/// handoff in its own inbox.
#[test]
fn the_fallback_copy_never_replaces_the_original() {
    let room = Room::new("original-kept");
    room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        DEAD_TARGET,
        "--subject",
        "original must survive",
        "--json",
    ]);
    let inbox = room.json(&["inbox", "--tool", DEAD_TARGET, "--json"]);
    let subjects: Vec<String> = inbox["data"]["inbox"]["items"]
        .as_array()
        .expect("inbox items")
        .iter()
        .map(|item| item["subject"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        subjects.iter().any(|s| s == "original must survive"),
        "the requested target keeps the unmodified original; got {subjects:?}"
    );
}

/// `--target-policy exact` is the operator saying "this one receiver or
/// nobody". Honouring it is a hard requirement: a handoff copied into a shared
/// base inbox can be answered by a third party.
#[test]
fn target_policy_exact_forbids_the_fallback_copy() {
    let room = Room::new("policy-exact");
    let envelope = room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        DEAD_TARGET,
        "--target-policy",
        "exact",
        "--subject",
        "no fan-out please",
        "--json",
    ]);
    let delivery = &envelope["data"]["delivery"];
    assert_eq!(delivery["target_live"], false);
    assert_eq!(delivery["reason"], "policy_forbids_fallback");
    assert!(
        delivery["fallback_inbox"].is_null(),
        "an explicit exact policy must suppress the copy entirely"
    );

    // Still loud. Suppressing the copy must not suppress the warning — that
    // would turn an explicit policy into a silent drop.
    let codes: Vec<&str> = envelope["data"]["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .map(|w| w["code"].as_str().expect("code"))
        .collect();
    assert!(codes.contains(&"handoff-target-not-live"), "got {codes:?}");

    let inbox = room.json(&["inbox", "--tool", "codex", "--json"]);
    let subjects: Vec<String> = inbox["data"]["inbox"]["items"]
        .as_array()
        .expect("inbox items")
        .iter()
        .map(|item| item["subject"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !subjects.iter().any(|s| s.contains("no fan-out please")),
        "no copy may reach the base inbox under --target-policy exact; got {subjects:?}"
    );
}

/// The other `--target-policy` values answer "who may reply to the referenced
/// fact", which is meaningless without a ref. That refusal must survive the
/// `exact` carve-out.
#[test]
fn the_reply_binding_policies_still_require_a_ref() {
    let room = Room::new("policy-requires-ref");
    let out = room
        .command(&[
            "say",
            "handoff",
            "--tool",
            "claude_code:sender",
            "--to",
            DEAD_TARGET,
            "--target-policy",
            "third-party",
            "--subject",
            "no ref here",
            "--json",
        ])
        .output()
        .expect("spawn rally");
    assert!(!out.status.success(), "third-party without --ref must fail");
    // The typed code may ride either stream depending on how far the refusal
    // gets before the envelope is built; the test is that it is EMITTED.
    let emitted = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        emitted.contains("handoff_target_policy_requires_ref"),
        "the refusal must keep its typed code; got {emitted}"
    );
}

/// `exact` answers "may this send fan out", which only a handoff can do.
/// Accepting it on another kind would turn a typed refusal into a silent no-op.
#[test]
fn target_policy_exact_is_still_refused_on_a_non_handoff_kind() {
    let room = Room::new("policy-exact-scope");
    let out = room
        .command(&[
            "say",
            "artifact",
            "--tool",
            "claude_code:sender",
            "--to",
            DEAD_TARGET,
            "--target-policy",
            "exact",
            "--subject",
            "artifacts do not fan out",
            "--json",
        ])
        .output()
        .expect("spawn rally");
    assert!(
        !out.status.success(),
        "a flag that cannot affect the command must be refused, not accepted"
    );
    let emitted = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        emitted.contains("handoff_target_policy_requires_ref"),
        "got {emitted}"
    );
}

/// A send whose target is already a base inbox has nowhere broader to go. The
/// answer is to say so, not to invent an inbox.
#[test]
fn a_dead_base_target_reports_that_nothing_else_has_the_handoff() {
    let room = Room::new("no-base");
    let envelope = room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        "codex",
        "--subject",
        "nowhere to fall back to",
        "--json",
    ]);
    let delivery = &envelope["data"]["delivery"];
    assert_eq!(delivery["reason"], "no_base_inbox");
    assert!(delivery["fallback_inbox"].is_null());
}

/// The send-time wake and the one `rally next` mints for the same handoff are
/// the same fact, not two spellings of it.
///
/// They must share an identity or a returning target sees the work twice, and
/// the D6 coalescing invariant (`routed_next_coalesces_wake_intents`) breaks —
/// it did, on the first full gate run of this feature.
#[test]
fn the_delivery_wake_coalesces_with_the_one_next_would_mint() {
    let room = Room::new("wake-coalesce");
    let sent = room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        DEAD_TARGET,
        "--subject",
        "one wake only",
        "--json",
    ]);
    let handoff_id = sent["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("event_id")
        .to_string();
    // The target comes back and polls, twice.
    room.json(&["next", "--tool", DEAD_TARGET, "--json"]);
    room.json(&["next", "--tool", DEAD_TARGET, "--json"]);

    // Keyed on `ref`, not on the target: the fixture already carries a wake
    // for an EARLIER handoff to the same target, and those must stay distinct.
    // One wake per handoff is the invariant; one wake per target is not.
    let recent = room.json(&["recent", "--limit", "300", "--json"]);
    let wakes = recent["data"]["recent"]["rows"]
        .as_array()
        .expect("recent rows")
        .iter()
        .filter_map(|row| row.get("fact"))
        .filter(|fact| fact["kind"] == "wake" && fact["ref"] == handoff_id.as_str())
        .count();
    assert_eq!(
        wakes, 1,
        "one handoff plus two polls must leave exactly one wake citing {handoff_id}"
    );
}

/// The two halves must agree. A send that reports a fallback inbox and an audit
/// that cannot find the copy would be two views of one event disagreeing — the
/// failure mode this whole module exists to remove.
#[test]
fn the_audit_view_finds_the_copy_the_send_reported() {
    let room = Room::new("round-trip");
    let sent = room.json(&[
        "say",
        "handoff",
        "--tool",
        "claude_code:sender",
        "--to",
        DEAD_TARGET,
        "--subject",
        "round trip",
        "--json",
    ]);
    let reported = sent["data"]["delivery"]["fallback_inbox"]
        .as_str()
        .expect("fallback_inbox");
    let sent_id = sent["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("event_id");

    let audit = room.json(&["handoffs", "--json"]);
    let row = rows(&audit)
        .iter()
        .find(|row| row["event_id"] == sent_id)
        .expect("the handoff just sent must appear in the audit view");
    assert_eq!(
        row["fallback_inbox"], reported,
        "`handoffs` must pair the copy with its original"
    );
    assert_eq!(
        row["undelivered"], true,
        "a fallback copy is an extra chance to be read, not proof anyone read it"
    );
}
