// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! `rally say receipt` warns, without failing, when the receipt text reads as
//! a new instruction, and points at `rally say handoff`. A plain ack stays
//! silent. The warning goes to stderr (senders often redirect `--json` stdout)
//! and to `data.warnings` with code `receipt:carries_instruction`.

mod support;

use serde_json::Value;
use support::channel_sandbox::ChannelSandbox;

const CODE: &str = "receipt:carries_instruction";

fn open_handoff(sandbox: &ChannelSandbox) -> String {
    let said = sandbox.rally_json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "lead:01",
        "--target",
        "worker:01",
        "--subject",
        "review the parser change",
        "--deliver",
        "record",
    ]);
    said["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff id")
        .to_string()
}

fn receipt(sandbox: &ChannelSandbox, handoff: &str, subject: &str) -> (Value, String) {
    let output = sandbox.rally_try(&[
        "say",
        "receipt",
        "--json",
        "--tool",
        "worker:01",
        "--ref",
        handoff,
        "--subject",
        subject,
    ]);
    assert!(
        output.status.success(),
        "receipt must commit even when it warns: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        serde_json::from_slice(&output.stdout).expect("receipt JSON"),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn has_warning(said: &Value) -> bool {
    said["data"]["warnings"]
        .as_array()
        .is_some_and(|w| w.iter().any(|item| item["code"] == CODE))
}

#[test]
fn receipt_carrying_an_instruction_warns_and_still_commits() {
    let sandbox = ChannelSandbox::spawn();
    let handoff = open_handoff(&sandbox);
    let (said, stderr) = receipt(
        &sandbox,
        &handoff,
        "received. Please rebase onto main and rerun the suite",
    );
    assert_eq!(said["data"]["say"]["committed"], true, "{said}");
    assert!(has_warning(&said), "{said}");
    assert!(
        stderr.contains("rally say handoff"),
        "stderr must suggest a handoff: {stderr}"
    );
}

#[test]
fn plain_receipt_does_not_warn() {
    let sandbox = ChannelSandbox::spawn();
    let handoff = open_handoff(&sandbox);
    let (said, stderr) = receipt(
        &sandbox,
        &handoff,
        "received; review done, 2 findings filed",
    );
    assert!(!has_warning(&said), "{said}");
    assert!(
        !stderr.contains(CODE) && !stderr.contains("new instruction"),
        "{stderr}"
    );
}
