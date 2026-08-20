// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! A send reports what actually happened to it, and says why.
//!
//! # What was wrong
//!
//! `rally inject` returned `ok: true` and exit 0 for ENQUEUE, not for delivery.
//! Observed live (O08): `data.inject.delivered` was false, no ACK arrived, and
//! the cause — the target had no managed session — was written to stderr.
//! Stderr is not a status channel; no programmatic caller reads it.
//!
//! `delivery_state` already told the truth about the receipt (`pending` means
//! none has arrived). What no field could say is WHY, and the two reasons a
//! wake sits pending need opposite handling. Measured on this repo's room, of
//! 620 unresolved wakes:
//!
//! * 403 were session-scoped — queued for a target that HAS a transport, and
//!   never consumed. Nobody was listening. The runner is the thing to fix.
//! * 217 targeted a bare tool name with no managed session — no live address
//!   at all. The address is the thing to fix.
//!
//! Both were spelled `pending` with no recorded cause, so the ledger could not
//! tell them apart and neither could a supervision pass.
//!
//! # What this file does NOT assert
//!
//! That an undeliverable send is refused. It must not be. An agent that is not
//! running now may return in a minute and find the work via `rally next`, and
//! that durability is the ledger's whole value; refusing to RECORD intent
//! because the recipient is asleep destroys it. At scale absence is the normal
//! state, so the contract is record-first, deliver-opportunistically, report
//! honestly. Every test here therefore asserts the fact was WRITTEN alongside
//! the honest status — a passing status assertion with a missing fact is a
//! regression, not a fix.

mod support;

use serde_json::Value;
use std::fs;
use std::path::Path;
use support::channel_sandbox::ChannelSandbox;

fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Every wake fact on the canonical ledger. Read from the JSONL segments, not
/// a projection: the point is what was DURABLY recorded.
fn wake_facts(rally_dir: &Path) -> Vec<Value> {
    let log = rally_dir.join("log");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&log) else {
        return out;
    };
    let mut paths: Vec<_> = entries
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
            if fact["kind"] == Value::String("wake".to_string()) {
                out.push(fact);
            }
        }
    }
    out
}

/// The `delivery_reason:` marker carried on a wake fact's evidence.
fn recorded_reason(fact: &Value) -> Option<String> {
    fact["evidence"].as_array()?.iter().find_map(|e| {
        e.as_str()
            .and_then(|s| s.strip_prefix("delivery_reason:"))
            .map(str::to_string)
    })
}

// =============================================================================
// no live address — the 217-class
// =============================================================================

/// FALSIFIER B'. A send to a target with no managed session must still write
/// the fact, must return a non-success delivery state with a typed reason, and
/// that reason must be readable back off the fact.
///
/// All three together. Dropping the first would recreate the refuse-on-absent
/// design this replaced; dropping the second or third leaves the caller back at
/// `ok: true` with the cause on stderr.
#[test]
fn an_absent_target_is_recorded_and_reported_honestly() {
    let sandbox = ChannelSandbox::spawn();
    let envelope = sandbox.rally_json(&[
        "inject",
        "claude_code",
        "--json",
        "--text",
        "does this still get recorded?",
        "--tool",
        "sender:01",
    ]);
    let inject = &envelope["data"]["inject"];

    // 1. The fact exists. This is the assertion that keeps the change on the
    //    record-first side of the line.
    let facts = wake_facts(&sandbox.rally_dir());
    assert_eq!(
        facts.len(),
        1,
        "intent must be RECORDED for an absent target, not refused: {inject}"
    );
    assert_ne!(
        inject["wake_intent"],
        Value::Null,
        "the envelope must carry the fact it wrote: {inject}"
    );

    // 2. The envelope is honest about what happened.
    assert_eq!(
        inject["delivered"].as_bool(),
        Some(false),
        "nothing was delivered: {inject}"
    );
    assert_eq!(
        inject["reached_target"].as_bool(),
        Some(false),
        "nothing reached the target: {inject}"
    );
    assert_eq!(
        inject["queued"].as_bool(),
        Some(true),
        "but it is durably queued and still reachable: {inject}"
    );
    assert_eq!(
        inject["delivery_reason"].as_str(),
        Some("queued_no_managed_session"),
        "the reason must be typed, not prose: {inject}"
    );
    assert!(
        inject["delivery_detail"]
            .as_str()
            .is_some_and(|d| d.contains("no managed session")),
        "the caller must be told what to do about it in the envelope, not on \
         stderr: {inject}"
    );

    // 3. The reason survives on the fact, so a wake found later says why it is
    //    pending instead of sitting mute.
    assert_eq!(
        recorded_reason(&facts[0]).as_deref(),
        Some("queued_no_managed_session"),
        "the cause must be readable off the fact: {}",
        facts[0]
    );
    assert_eq!(
        facts[0]["status"].as_str(),
        Some("pending"),
        "status stays the REAL receipt state; the cause rides alongside it, it \
         does not replace it: {}",
        facts[0]
    );
}

/// `ok: true` is not a delivery claim and never was — it reports that the
/// COMMAND succeeded, which for an absent target it did: intent was durably
/// recorded. The fix is not to make `ok` lie in the other direction, it is that
/// a caller can now branch on delivery without interpreting `ok`.
#[test]
fn command_success_and_delivery_success_are_separately_answerable() {
    let sandbox = ChannelSandbox::spawn();
    let envelope = sandbox.rally_json(&[
        "inject",
        "codex",
        "--json",
        "--text",
        "recorded, not delivered",
        "--tool",
        "sender:01",
    ]);

    assert_eq!(
        envelope["ok"].as_bool(),
        Some(true),
        "recording intent for an absent agent is a successful command"
    );
    assert_eq!(
        envelope["data"]["inject"]["reached_target"].as_bool(),
        Some(false),
        "and an undelivered message, at the same time"
    );
}

// =============================================================================
// nobody listening — the 403-class
// =============================================================================

/// The two queued reasons must be DISTINCT values, because they call for
/// opposite responses: re-address one, wait on or restart the runner for the
/// other. Collapsing both to `pending` is the state this replaced.
#[test]
fn a_queued_send_distinguishes_no_address_from_no_listener() {
    let sandbox = ChannelSandbox::spawn();

    // No managed session: no live address.
    let absent = sandbox.rally_json(&[
        "inject",
        "claude_code",
        "--json",
        "--text",
        "no address",
        "--tool",
        "sender:01",
    ]);

    // A managed session exists, so there IS an address. `/usr/bin/true` makes
    // every tmux subcommand succeed without doing anything, which both keeps
    // the session's liveness probe healthy and lets the backend write "succeed"
    // against a pane nothing is reading.
    let name = unique_name("listener");
    let target = sandbox.add_tmux_session(&name);
    let present = sandbox.rally_json(&[
        "inject",
        &target,
        "--json",
        "--text",
        "no listener",
        "--tool",
        "sender:01",
        "--tmux-bin",
        "/usr/bin/true",
    ]);

    let absent_reason = absent["data"]["inject"]["delivery_reason"]
        .as_str()
        .unwrap_or("");
    let present_reason = present["data"]["inject"]["delivery_reason"]
        .as_str()
        .unwrap_or("");

    assert_eq!(absent_reason, "queued_no_managed_session");
    assert_ne!(
        absent_reason, present_reason,
        "a target with a session and a target without one must not report the \
         same reason; telling them apart is the point. absent={absent_reason} \
         present={present_reason}"
    );
    assert!(
        !present_reason.is_empty(),
        "a managed-session send must also carry a typed reason: {present}"
    );
}

// =============================================================================
// a failed transport attempt remains queued
// =============================================================================

/// A failed backend write reports a FAILURE reason while preserving the
/// durable queue truth. The transport attempt failed, but the ledger write
/// already landed, so the target can still consume the queued copy later.
#[test]
fn a_failed_backend_write_is_reported_as_failed_and_still_queued() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("broken");
    let target = sandbox.add_tmux_session(&name);

    // `/usr/bin/false` makes every tmux subcommand fail, so the legacy backend
    // write fails while the ledger write already landed.
    let envelope = sandbox.rally_json(&[
        "inject",
        &target,
        "--json",
        "--text",
        "this write fails",
        "--tool",
        "sender:01",
        "--tmux-bin",
        "/usr/bin/false",
    ]);
    let inject = &envelope["data"]["inject"];

    assert_eq!(
        inject["delivery_state"].as_str(),
        Some("failed"),
        "the pre-existing state field must keep its meaning: {inject}"
    );
    assert_eq!(
        inject["reached_target"].as_bool(),
        Some(false),
        "a failed write reached nobody: {inject}"
    );
    assert_eq!(
        inject["delivery_reason"].as_str(),
        Some("failed_backend_inject"),
        "the failed transport must retain its typed failure reason: {inject}"
    );
    assert_eq!(
        inject["queued"].as_bool(),
        Some(true),
        "the ledger write landed, so the durable copy remains queued: {inject}"
    );
    assert_eq!(
        inject["wake_intent"]["status"].as_str(),
        Some("failed"),
        "and the fact must not claim delivered — the discipline this extends: \
         {inject}"
    );
}

// =============================================================================
// dry run
// =============================================================================

/// A dry run says so, and writes nothing. Without a distinct reason a caller
/// cannot tell a planned send from a queued one.
#[test]
fn a_dry_run_reports_itself_and_writes_nothing() {
    let sandbox = ChannelSandbox::spawn();
    let envelope = sandbox.rally_json(&[
        "inject",
        "claude_code",
        "--json",
        "--dry-run",
        "--text",
        "not actually sent",
        "--tool",
        "sender:01",
    ]);
    let inject = &envelope["data"]["inject"];

    assert_eq!(
        inject["delivery_reason"].as_str(),
        Some("planned_dry_run"),
        "a dry run must be distinguishable from a queued send: {inject}"
    );
    assert_eq!(inject["reached_target"].as_bool(), Some(false));
    assert_eq!(
        inject["queued"].as_bool(),
        Some(false),
        "nothing is queued by a dry run: {inject}"
    );
    assert!(
        wake_facts(&sandbox.rally_dir()).is_empty(),
        "a dry run must not write a wake fact"
    );
}
