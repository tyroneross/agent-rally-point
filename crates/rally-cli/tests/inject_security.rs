// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! Adversarial tests for the control-plane security review (2026-06-02),
//! write-side (`rally inject`). LIVE binary — no stubs.
//!
//!   * SEC-006 — a malformed / traversal sender id (`--tool`) is rejected at
//!     the write boundary; no directive and no audit fact are written.
//!   * SEC-009 — an `--urgent` inject (which is an urgent Addition under the
//!     current CLI semantics) is delivered by NO legacy backend; the urgent
//!     path is reserved to Stop/Retraction and the daemon would reject it.

mod support;

use support::channel_sandbox::ChannelSandbox;

fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}-{}", N.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// SEC-006 — forged / malformed sender rejected at the write boundary
// ---------------------------------------------------------------------------

#[test]
fn sec006_traversal_sender_id_rejected() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("sec006");
    let target = sandbox.add_tmux_session(&name);

    // A `--tool` (Directive.from) carrying a path traversal must be refused
    // BEFORE any ledger write — validate_agent_id rejects it.
    let out = sandbox.rally_try(&[
        "inject",
        &target,
        "--json",
        "--text",
        "pwn",
        "--tool",
        "../../etc/passwd",
        "--tmux-bin",
        "/usr/bin/true",
    ]);
    assert!(
        !out.status.success(),
        "traversal sender id MUST be rejected; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // No directive was written for the real target's tool inbox.
    let expected_tool = format!("claude_code:{target}");
    assert!(
        sandbox.read_directives(&expected_tool, 0).is_empty(),
        "a rejected inject must not have written a directive"
    );
}

#[test]
fn sec006_malformed_sender_ids_rejected() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("sec006b");
    let target = sandbox.add_tmux_session(&name);

    for bad in ["", "bad/slash", ".hidden", "has space", "a\\b"] {
        let out = sandbox.rally_try(&[
            "inject",
            &target,
            "--json",
            "--text",
            "x",
            "--tool",
            bad,
            "--tmux-bin",
            "/usr/bin/true",
        ]);
        assert!(
            !out.status.success(),
            "malformed sender {bad:?} must be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// feat/inject-ledger-target — target-side gate for the ledger-only arm.
//
// The pre-change resolution went through `find_session(&target)?` which
// implicitly bounded the input to the managed-session id space. The new
// `resolve_inject_target` falls back to `validate_agent_id` for unregistered
// strings, so the target-side gate must reject the same SEC-003 / SEC-006
// adversarial inputs as the sender-side gate.
// ---------------------------------------------------------------------------

#[test]
fn target_traversal_id_does_not_resolve_to_ledger_agent() {
    let sandbox = ChannelSandbox::spawn();
    // No `add_tmux_session` — there is no managed session to mask the
    // resolution. The traversal target must NOT silently resolve via the
    // ledger arm; it must be rejected.
    let out = sandbox.rally_try(&[
        "inject",
        "../../etc/passwd",
        "--json",
        "--text",
        "pwn",
        "--tool",
        "claude_code:test-sender",
    ]);
    assert!(
        !out.status.success(),
        "traversal target id MUST be rejected; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // No directive was written to the obvious adversarial inbox name (an
    // implementation that mistakenly sanitized then wrote would land here).
    assert!(
        sandbox.read_directives("etc/passwd", 0).is_empty(),
        "a rejected target must not have written a directive anywhere",
    );
    assert!(
        sandbox.read_directives("..", 0).is_empty(),
        "a rejected target must not have written a directive anywhere",
    );
}

#[test]
fn target_malformed_ids_rejected_at_resolution() {
    let sandbox = ChannelSandbox::spawn();
    for bad in ["", "bad/slash", ".hidden", "has space", "a\\b"] {
        let out = sandbox.rally_try(&[
            "inject",
            bad,
            "--json",
            "--text",
            "x",
            "--tool",
            "claude_code:test-sender",
        ]);
        assert!(
            !out.status.success(),
            "malformed target {bad:?} must be rejected; stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

// ---------------------------------------------------------------------------
// SEC-009 — urgent inject is delivered by NO backend
// ---------------------------------------------------------------------------

#[test]
fn sec009_urgent_addition_is_not_delivered_by_any_backend() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("sec009");
    let target = sandbox.add_tmux_session(&name);

    // `--urgent` => urgent Addition (the only itype the CLI emits). The legacy
    // tmux/cmux synchronous inject MUST be gated off (split-enforcement guard),
    // so `delivered` is false and the state is not "delivered".
    let outcome =
        sandbox.inject_with_flags(&target, "claude_code:test-sender", "URGENT do X", true);
    assert!(
        !outcome.delivered,
        "urgent Addition must NOT be delivered by the legacy backend; outcome={outcome:?}"
    );
    assert_ne!(
        outcome.delivery_state, "delivered",
        "urgent Addition delivery_state must not be 'delivered'; outcome={outcome:?}"
    );

    // A NON-urgent inject of the same shape still reports a normal state — proves
    // the gate is specific to urgent, not a blanket break.
    let normal = sandbox.inject(&target, "claude_code:test-sender", "normal do X");
    assert!(
        normal.delivery_state == "pending" || normal.delivery_state == "delivered",
        "non-urgent inject must still write/deliver normally; outcome={normal:?}"
    );
}
