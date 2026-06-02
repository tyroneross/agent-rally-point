// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # Plan F P2 — ChannelSandbox round-trip
//!
//! The LIVE-binary verification that rally-cli's `command_inject` writes
//! a typed `Directive` to the `.rally` ledger and that the canonical
//! `FileInbox` reader can read it back byte-identical.
//!
//! These tests do NOT spawn ptyd / rally-termd (that's P3's TermdSandbox).
//! They prove H1 at the binary boundary: the writer side of the contract
//! agrees with the reader side, both sourced from `rally-protocol`.
//!
//! ## What this catches
//! - JSON envelope regressions (`data.inject.delivery_state`,
//!   `directive_seq`, `directive_to`).
//! - Wire-format drift between rally-cli and `rally-protocol`.
//! - The "ledger write happens even when backend inject fails" contract.

mod support;

use rally_protocol::{DeliveryStatus, DirectiveKind, InterruptType, Receipt, now_ts};
use support::channel_sandbox::ChannelSandbox;

/// `--tmux-bin /usr/bin/true` makes tmux subcommands succeed without doing
/// anything; that's the existing test idiom (see tests/user_journey.rs). The
/// per-test counter avoids name collisions when cargo runs tests in parallel.
fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}-{}", N.fetch_add(1, Ordering::Relaxed))
}

#[test]
fn inject_writes_directive_to_ledger_for_managed_session() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("rt");
    let target = sandbox.add_tmux_session(&name);
    // `rally run claude --name rt-0` creates session named `rt-0-01` with
    // tool `claude_code:rt-0-01`.
    let expected_tool = format!("claude_code:{target}");

    let outcome = sandbox.inject(&target, "claude_code:test-sender", "hello agent");

    assert!(
        outcome.delivery_state == "pending" || outcome.delivery_state == "delivered",
        "delivery_state must be pending|delivered after a successful ledger write, got {:?}",
        outcome.delivery_state
    );
    assert!(
        outcome.directive_seq.is_some(),
        "directive_seq must be set when the ledger write succeeded; outcome={:?}",
        outcome
    );
    assert_eq!(
        outcome.directive_to.as_deref(),
        Some(expected_tool.as_str())
    );

    let directives = sandbox.read_directives(&expected_tool, 0);
    assert_eq!(directives.len(), 1, "exactly one Directive on the ledger");
    let d = &directives[0];
    assert_eq!(d.seq, 1, "first directive in fresh inbox is seq=1");
    assert_eq!(d.to, expected_tool);
    assert_eq!(d.from, "claude_code:test-sender");
    assert_eq!(d.kind, DirectiveKind::Deliver);
    assert_eq!(d.itype, InterruptType::Addition);
    assert_eq!(d.text.as_deref(), Some("hello agent"));
    assert!(!d.urgent, "P2 default is async (urgent=false)");
    assert!(d.ts > 0.0, "timestamp must be set");
}

#[test]
fn inject_assigns_monotonic_sequence_across_multiple_calls() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("rt");
    let target = sandbox.add_tmux_session(&name);
    let expected_tool = format!("claude_code:{target}");

    let s1 = sandbox.inject(&target, "s", "msg 1").directive_seq.unwrap();
    let s2 = sandbox.inject(&target, "s", "msg 2").directive_seq.unwrap();
    let s3 = sandbox.inject(&target, "s", "msg 3").directive_seq.unwrap();

    assert_eq!((s1, s2, s3), (1, 2, 3));

    let directives = sandbox.read_directives(&expected_tool, 0);
    let seqs: Vec<u64> = directives.iter().map(|d| d.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);

    let texts: Vec<&str> = directives
        .iter()
        .map(|d| d.text.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(texts, vec!["msg 1", "msg 2", "msg 3"]);
}

#[test]
fn read_since_after_inject_filters_correctly() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("rt");
    let target = sandbox.add_tmux_session(&name);
    let expected_tool = format!("claude_code:{target}");

    for i in 0..5 {
        sandbox.inject(&target, "s", &format!("msg-{i}"));
    }
    assert_eq!(sandbox.read_directives(&expected_tool, 0).len(), 5);
    assert_eq!(sandbox.read_directives(&expected_tool, 2).len(), 3);
    assert_eq!(sandbox.read_directives(&expected_tool, 5).len(), 0);
}

#[test]
fn receipt_roundtrip_simulating_self_ack_or_daemon() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("rt");
    let target = sandbox.add_tmux_session(&name);
    let expected_tool = format!("claude_code:{target}");
    let outcome = sandbox.inject(&target, "s", "stop");
    let seq = outcome.directive_seq.expect("seq assigned");

    sandbox.append_receipt(&Receipt {
        ref_seq: seq,
        to: expected_tool.clone(),
        status: DeliveryStatus::Delivered,
        by: "rally-termd-test-double".to_string(),
        evidence: Some("bytes-written=4".to_string()),
        error: None,
        ts: now_ts(),
    });

    let receipts = sandbox.read_receipts(&expected_tool, 0);
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].ref_seq, seq);
    assert_eq!(receipts[0].status, DeliveryStatus::Delivered);
}

#[test]
fn urgent_flag_propagates_to_directive_urgent_field() {
    // Plan F P4: --urgent on the CLI writes a Directive with urgent: true.
    // The daemon then decides whether to honor the override (Stop|Retraction
    // only — Deliver+Addition with urgent=true is rejected by the daemon
    // per the contract). Here we only verify the writer side: the flag
    // PROPAGATES correctly to the ledger.
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("urgent");
    let target = sandbox.add_tmux_session(&name);
    let expected_tool = format!("claude_code:{target}");

    let outcome = sandbox.inject_with_flags(&target, "s", "stop", true);
    assert!(outcome.directive_seq.is_some());

    let directives = sandbox.read_directives(&expected_tool, 0);
    assert_eq!(directives.len(), 1);
    assert!(
        directives[0].urgent,
        "the --urgent CLI flag must propagate as Directive::urgent=true"
    );
}

#[test]
fn urgent_default_false_when_flag_not_passed() {
    // Default `urgent: false` keeps the contract safe for the common case.
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("urgent-default");
    let target = sandbox.add_tmux_session(&name);
    let expected_tool = format!("claude_code:{target}");

    sandbox.inject(&target, "s", "ordinary message");
    let directives = sandbox.read_directives(&expected_tool, 0);
    assert_eq!(directives.len(), 1);
    assert!(!directives[0].urgent, "default urgent must be false");
}

#[test]
fn delivery_state_field_is_pending_or_delivered_never_unknown() {
    // Plan F H5: never silent-false. `delivered: false` due to a backend
    // hiccup must NEVER manifest as `delivery_state: unknown` — the
    // ledger write is the truthful source.
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("rt");
    let target = sandbox.add_tmux_session(&name);

    let outcome = sandbox.inject(&target, "s", "this is durable");

    assert!(
        ["pending", "delivered"].contains(&outcome.delivery_state.as_str()),
        "delivery_state must be one of pending|delivered after a successful ledger write; got {:?}",
        outcome.delivery_state
    );
    assert_ne!(
        outcome.delivery_state, "unknown",
        "H5: never silent-unknown — every inject MUST report a truthful state"
    );
}
