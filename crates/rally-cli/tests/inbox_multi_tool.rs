// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally inbox` with repeated `--tool` reads several inboxes in one process.
//!
//! Measured 2026-09-25: Easy Terminal's rally router ran one `rally inbox`
//! process per routed identity on every ledger change — a 40-pane run spawned
//! 40 processes (40 store opens) per change. The multi form returns every
//! named inbox from one store open under `data.inboxes`; the single form must
//! stay exactly what it was (`data.tool` + `data.inbox`).

mod support;

use serde_json::Value;
use support::channel_sandbox::ChannelSandbox;

const SENDER: &str = "sender:01";

fn handoff(sandbox: &ChannelSandbox, target: &str, subject: &str, extra: &[&str]) -> String {
    let mut args = vec![
        "say",
        "handoff",
        "--json",
        "--tool",
        SENDER,
        "--target",
        target,
        "--subject",
        subject,
    ];
    args.extend_from_slice(extra);
    let said = sandbox.rally_json(&args);
    said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff event id")
        .to_string()
}

/// Ages tick between two calls; everything else must match exactly.
fn without_ages(mut inbox: Value) -> Value {
    inbox["oldest_age_secs"] = Value::Null;
    if let Some(items) = inbox["items"].as_array_mut() {
        for item in items {
            item["age_secs"] = Value::Null;
            item["stale"] = Value::Null;
        }
    }
    inbox
}

#[test]
fn single_tool_form_keeps_the_original_envelope_shape() {
    let sandbox = ChannelSandbox::spawn();
    handoff(&sandbox, "codex:07", "one", &[]);

    let single = sandbox.rally_json(&["inbox", "--json", "--tool", "codex:07"]);
    let data = single["data"].as_object().expect("data object");
    let mut keys = data.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(keys, vec!["inbox".to_string(), "tool".to_string()]);
    assert_eq!(single["data"]["tool"], "codex:07");
    assert_eq!(single["schema"], "agent-rally.command.inbox.v1");
    assert_eq!(single["command"], "inbox");
    assert!(single["data"].get("inboxes").is_none());
    assert_eq!(single["data"]["inbox"]["count"], 1);
}

#[test]
fn multi_tool_form_equals_the_union_of_single_calls() {
    let sandbox = ChannelSandbox::spawn();
    let a1 = handoff(
        &sandbox,
        "codex:07",
        "first for a",
        &["--ack-within", "90s"],
    );
    handoff(&sandbox, "codex:07", "second for a", &[]);
    handoff(&sandbox, "claude:02", "only for b", &[]);

    let tools = ["codex:07", "claude:02", "nobody:99"];
    let multi = sandbox.rally_json(&[
        "inbox", "--json", "--tool", tools[0], "--tool", tools[1], "--tool", tools[2],
    ]);
    assert_eq!(multi["schema"], "agent-rally.command.inbox.v1");
    assert!(multi["data"].get("inbox").is_none(), "{multi}");
    let inboxes = multi["data"]["inboxes"].as_object().expect("inboxes map");
    assert_eq!(inboxes.len(), tools.len());

    for tool in tools {
        let single = sandbox.rally_json(&["inbox", "--json", "--tool", tool]);
        assert_eq!(
            without_ages(inboxes[tool].clone()),
            without_ages(single["data"]["inbox"].clone()),
            "multi entry for {tool} differs from the single call"
        );
    }
    assert_eq!(inboxes["codex:07"]["count"], 2);
    assert_eq!(inboxes["claude:02"]["count"], 1);
    assert_eq!(inboxes["nobody:99"]["count"], 0);

    // ack_by rides along per item exactly as in the single form.
    let items = inboxes["codex:07"]["items"].as_array().unwrap();
    let with_deadline = items.iter().find(|i| i["event_id"] == a1.as_str()).unwrap();
    assert!(with_deadline["ack_by"].is_string(), "{with_deadline}");
    assert!(with_deadline["ack_by_ms"].is_u64(), "{with_deadline}");
    let without = items.iter().find(|i| i["event_id"] != a1.as_str()).unwrap();
    assert!(without.get("ack_by").is_none() && without.get("ack_by_ms").is_none());
    for item in inboxes["claude:02"]["items"].as_array().unwrap() {
        assert!(item.get("ack_by").is_none(), "{item}");
    }
}

#[test]
fn duplicate_tools_collapse_to_one_entry() {
    let sandbox = ChannelSandbox::spawn();
    handoff(&sandbox, "codex:07", "one", &[]);
    let multi = sandbox.rally_json(&[
        "inbox", "--json", "--tool", "codex:07", "--tool", "codex:07",
    ]);
    let inboxes = multi["data"]["inboxes"].as_object().expect("inboxes map");
    assert_eq!(inboxes.len(), 1);
    assert_eq!(inboxes["codex:07"]["count"], 1);
}

#[test]
fn more_than_128_tools_is_a_typed_usage_error() {
    let sandbox = ChannelSandbox::spawn();
    let names = (0..129).map(|i| format!("t:{i}")).collect::<Vec<_>>();
    let mut args = vec!["inbox", "--json"];
    for name in &names {
        args.push("--tool");
        args.push(name);
    }
    let out = sandbox.rally_try(&args);
    assert_eq!(out.status.code(), Some(2), "usage exit code expected");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("inbox-too-many-tools"), "{all}");

    // Exactly the cap is accepted.
    let mut args = vec!["inbox", "--json"];
    for name in &names[..128] {
        args.push("--tool");
        args.push(name);
    }
    let ok = sandbox.rally_json(&args);
    assert_eq!(ok["data"]["inboxes"].as_object().unwrap().len(), 128);
}
