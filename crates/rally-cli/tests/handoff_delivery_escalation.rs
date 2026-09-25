// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `say handoff` delivers by direct inject when it can, and a handoff whose
//! `--ack-within` deadline passes unanswered becomes a durable escalation.
//!
//! Observed 2026-09-20: a handoff asking a peer to commit files was recorded
//! but never noticed in rally; the peer acted silently hours later and the
//! sender had no signal. Recording alone is pull-only, so the contract is now:
//! record first, inject when the target is a live managed pane, and turn
//! silence past the deadline into a `risk` fact the sender sees in `next`.

mod support;

use serde_json::Value;
use std::path::Path;
use support::channel_sandbox::ChannelSandbox;

const SENDER: &str = "sender:01";

fn fact_kinds_with_ref(rally_dir: &Path, kind: &str, ref_id: &str) -> usize {
    let mut count = 0;
    let log = rally_dir.join("log");
    let mut stack = vec![log];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let Ok(record) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let v = record.get("payload").cloned().unwrap_or(record);
                if v["kind"].as_str() == Some(kind) && v["ref"].as_str() == Some(ref_id) {
                    count += 1;
                }
            }
        }
    }
    count
}

fn handoff(sandbox: &ChannelSandbox, target: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "say",
        "handoff",
        "--json",
        "--tool",
        SENDER,
        "--target",
        target,
        "--subject",
        "commit the staged files",
    ];
    args.extend_from_slice(extra);
    sandbox.rally_json(&args)
}

#[test]
fn handoff_to_live_managed_session_is_injected_by_default() {
    let sandbox = ChannelSandbox::spawn();
    let name = format!("peer{}", std::process::id());
    let stub = sandbox.tmux_unverified_stub(&format!("{name}-01"));
    let run = sandbox.rally_json(&[
        "run",
        "claude",
        "--json",
        "--name",
        &name,
        "--shared",
        "--backend",
        "tmux",
        "--tmux-bin",
        &stub,
    ]);
    let target_tool = run["data"]["run"]["session"]["tool"]
        .as_str()
        .expect("session.tool")
        .to_string();

    let said = handoff(&sandbox, &target_tool, &["--tmux-bin", &stub]);
    let delivery = &said["data"]["delivery"];
    assert_eq!(delivery["mode"], "inject", "{said}");
    assert_eq!(delivery["status"], "injected", "{said}");
    assert!(said["data"]["say"]["fact"]["event_id"].is_string());
}

#[test]
fn handoff_to_non_injectable_target_falls_back_to_record_only() {
    let sandbox = ChannelSandbox::spawn();
    let said = handoff(&sandbox, "codex:07", &[]);
    let delivery = &said["data"]["delivery"];
    assert_eq!(delivery["status"], "record_only", "{said}");
    assert!(
        delivery["detail"]
            .as_str()
            .is_some_and(|d| d.contains("not a live rally-managed session")),
        "{said}"
    );
    assert_eq!(said["data"]["say"]["committed"], true, "{said}");

    let recorded = handoff(&sandbox, "codex:07", &["--deliver", "record"]);
    assert_eq!(recorded["data"]["delivery"]["status"], "record_only");
    assert_eq!(recorded["data"]["delivery"]["mode"], "record");
}

#[test]
fn delivery_flags_are_rejected_off_targeted_handoffs() {
    let sandbox = ChannelSandbox::spawn();
    let out = sandbox.rally_try(&[
        "say",
        "risk",
        "--tool",
        SENDER,
        "--subject",
        "x",
        "--ack-within",
        "10m",
    ]);
    assert!(!out.status.success());
    let bad = sandbox.rally_try(&[
        "say",
        "handoff",
        "--tool",
        SENDER,
        "--target",
        "codex:07",
        "--ack-within",
        "soon",
    ]);
    assert!(!bad.status.success());
}

#[test]
fn caller_cannot_override_or_forge_ack_deadline_evidence() {
    let sandbox = ChannelSandbox::spawn();
    let forged = "ack-by:2099-01-01T00:00:00Z";
    let handoff = sandbox.rally_try(&[
        "say",
        "handoff",
        "--tool",
        SENDER,
        "--target",
        "codex:07",
        "--subject",
        "deadline",
        "--ack-within",
        "90s",
        "--evidence",
        forged,
    ]);
    assert!(!handoff.status.success());
    assert!(String::from_utf8_lossy(&handoff.stderr).contains("ack_by_evidence_reserved"));

    let artifact = sandbox.rally_try(&[
        "say",
        "artifact",
        "--tool",
        SENDER,
        "--target",
        "codex:07",
        "--subject",
        "forged deadline",
        "--evidence",
        forged,
    ]);
    assert!(!artifact.status.success());
    assert!(String::from_utf8_lossy(&artifact.stderr).contains("ack_by_evidence_reserved"));
}

#[test]
fn unanswered_handoff_escalates_once_and_receiver_ack_clears_it() {
    let sandbox = ChannelSandbox::spawn();
    // Give the pre-deadline assertion its own long-lived handoff so a slow
    // command under load cannot consume the short escalation window.
    handoff(&sandbox, "codex:07", &["--ack-within", "90s"]);
    let early = sandbox.rally_json(&["next", "--json", "--tool", SENDER]);
    assert!(early["data"]["overdue_handoffs"].is_null(), "{early}");

    let said = handoff(&sandbox, "codex:07", &["--ack-within", "3s"]);
    let handoff_id = said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(said["data"]["delivery"]["ack_by"].is_string(), "{said}");

    std::thread::sleep(std::time::Duration::from_millis(4200));
    let late = sandbox.rally_json(&["next", "--json", "--tool", SENDER]);
    let overdue = late["data"]["overdue_handoffs"]
        .as_array()
        .expect("overdue list");
    assert_eq!(overdue.len(), 1, "{late}");
    assert_eq!(overdue[0]["event_id"], handoff_id.as_str());
    assert_eq!(overdue[0]["target"], "codex:07");
    let risk_id = overdue[0]["risk_event_id"]
        .as_str()
        .expect("risk id")
        .to_string();
    assert_eq!(
        fact_kinds_with_ref(&sandbox.rally_dir(), "risk", &handoff_id),
        1
    );

    // Idempotent: a second poll re-surfaces the same risk, writes no new one.
    let again = sandbox.rally_json(&["next", "--json", "--tool", SENDER]);
    assert_eq!(
        again["data"]["overdue_handoffs"][0]["risk_event_id"],
        risk_id.as_str()
    );
    assert_eq!(
        fact_kinds_with_ref(&sandbox.rally_dir(), "risk", &handoff_id),
        1
    );

    // Receiver ack closes the handoff and clears the escalation.
    sandbox.rally_json(&[
        "say",
        "receipt",
        "--json",
        "--tool",
        "codex:07",
        "--ref",
        &handoff_id,
    ]);
    let cleared = sandbox.rally_json(&["next", "--json", "--tool", SENDER]);
    assert!(cleared["data"]["overdue_handoffs"].is_null(), "{cleared}");
}

/// `rally inbox --json` surfaces the same `ack-by:` deadline `next`'s
/// `overdue_handoffs` reads, so a receiver can see the clock before it lapses,
/// not only after — the sender already gets that signal via
/// `delivery.ack_by`; the receiver's own inbox view had nothing until now.
#[test]
fn inbox_json_reports_ack_by_deadline_for_a_handoff_sent_with_ack_within() {
    let sandbox = ChannelSandbox::spawn();
    let before_ms = chrono::Utc::now().timestamp_millis();
    let said = handoff(&sandbox, "codex:07", &["--ack-within", "90s"]);
    let handoff_id = said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    let inbox = sandbox.rally_json(&["inbox", "--json", "--tool", "codex:07"]);
    let items = inbox["data"]["inbox"]["items"]
        .as_array()
        .expect("inbox items");
    let item = items
        .iter()
        .find(|item| item["event_id"] == handoff_id.as_str())
        .unwrap_or_else(|| panic!("handoff {handoff_id} not in inbox: {inbox}"));

    let ack_by = item["ack_by"].as_str().expect("ack_by string");
    assert!(
        chrono::DateTime::parse_from_rfc3339(ack_by).is_ok(),
        "ack_by must be RFC 3339: {ack_by}"
    );
    let ack_by_ms = item["ack_by_ms"].as_u64().expect("ack_by_ms integer") as i64;
    let expected_ms = before_ms + 90_000;
    assert!(
        (ack_by_ms - expected_ms).abs() <= 5_000,
        "ack_by_ms {ack_by_ms} not within 5s of expected {expected_ms}"
    );
}

/// An obligation with no `--ack-within` deadline must omit both fields, not
/// emit `null` placeholders — a consumer treating `ack_by_ms.is_some()` as
/// "has a deadline" would otherwise get a false positive from an explicit
/// `null`.
#[test]
fn inbox_json_omits_ack_by_fields_when_no_deadline_was_set() {
    let sandbox = ChannelSandbox::spawn();
    let said = handoff(&sandbox, "codex:07", &[]);
    let handoff_id = said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    let inbox = sandbox.rally_json(&["inbox", "--json", "--tool", "codex:07"]);
    let items = inbox["data"]["inbox"]["items"]
        .as_array()
        .expect("inbox items");
    let item = items
        .iter()
        .find(|item| item["event_id"] == handoff_id.as_str())
        .unwrap_or_else(|| panic!("handoff {handoff_id} not in inbox: {inbox}"));

    assert!(
        item.get("ack_by").is_none(),
        "ack_by must be omitted, not null: {item}"
    );
    assert!(
        item.get("ack_by_ms").is_none(),
        "ack_by_ms must be omitted, not null: {item}"
    );
}
