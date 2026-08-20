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
use std::process::Command;
use std::thread;
use std::time::Duration;
use support::channel_sandbox::ChannelSandbox;

/// `--tmux-bin /usr/bin/true` makes tmux subcommands succeed without doing
/// anything; that's the existing test idiom (see tests/user_journey.rs). The
/// per-test counter avoids name collisions when cargo runs tests in parallel.
fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}-{}", N.fetch_add(1, Ordering::Relaxed))
}

fn post_target_ack_after_delay(
    sandbox: &ChannelSandbox,
    target_tool: &str,
    handoff_id: &str,
) -> thread::JoinHandle<()> {
    let cwd = sandbox.cwd().to_path_buf();
    let home = sandbox.home().to_path_buf();
    let target_tool = target_tool.to_string();
    let handoff_id = handoff_id.to_string();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let output = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(cwd)
            .env("HOME", home)
            .env_remove("PWD")
            .args([
                "say",
                "resolve",
                "--json",
                "--tool",
                target_tool.as_str(),
                "--ref",
                handoff_id.as_str(),
                "--subject",
                "target received the injected handoff",
            ])
            .output()
            .expect("spawn target ACK");
        assert!(
            output.status.success(),
            "target ACK failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

fn assert_final_ack_truth(inject: &serde_json::Value) {
    assert_eq!(inject["ack"]["received"].as_bool(), Some(true));
    assert_eq!(inject["verified_received"].as_bool(), Some(true));
    assert_eq!(inject["ack_state"].as_str(), Some("acked"));
    assert_eq!(inject["delivery_reason"].as_str(), Some("delivered"));
    assert!(
        inject["delivery_detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("received it")),
        "final guidance must reflect the target ACK: {inject}"
    );
    assert_eq!(inject["reached_target"].as_bool(), Some(true));
    assert_eq!(inject["queued"].as_bool(), Some(false));
    assert!(
        inject["fallback_plan"].is_null(),
        "verified receipt must not retain a queued-delivery fallback: {inject}"
    );
}

#[test]
fn published_inject_v1_keeps_additive_delivery_truth_fields_optional() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schemas/agent-rally.command.inject.v1.json");
    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read published inject schema"))
            .expect("parse published inject schema");
    let inject_schema = &schema["properties"]["data"]["properties"]["inject"];
    let required = inject_schema["required"]
        .as_array()
        .expect("published inject required array");
    let properties = inject_schema["properties"]
        .as_object()
        .expect("published inject properties object");

    for field in [
        "delivery_reason",
        "delivery_detail",
        "reached_target",
        "queued",
    ] {
        assert!(
            properties.contains_key(field),
            "current v1 schema must describe additive field {field}"
        );
        assert!(
            !required.iter().any(|name| name.as_str() == Some(field)),
            "legacy v1 envelopes that predate {field} must remain valid"
        );
    }
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
fn target_ack_reconciles_managed_sent_unverified_to_final_delivery_truth() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("ack-managed");
    let target = sandbox.add_tmux_session(&name);
    let target_tool = format!("claude_code:{target}");
    let handoff = sandbox.rally_json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "sender:01",
        "--target",
        &target_tool,
        "--subject",
        "managed ACK reconciliation",
    ]);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff event_id");
    let resolver = post_target_ack_after_delay(&sandbox, &target_tool, handoff_id);
    let tmux_stub = sandbox.tmux_unverified_stub(&target);

    // The tmux double reports the registered pane live and makes the pane write
    // succeed while capture returns unrelated nonempty output, producing a
    // real SentUnverified attempt before the target-authored Rally ACK arrives.
    let envelope = sandbox.rally_json(&[
        "inject",
        &target,
        "--json",
        "--handoff",
        handoff_id,
        "--require-ack",
        "--timeout-seconds",
        "3",
        "--tool",
        "sender:01",
        "--tmux-bin",
        tmux_stub.as_str(),
    ]);
    resolver.join().expect("target ACK thread");
    let inject = &envelope["data"]["inject"];

    assert_eq!(
        inject["delivered"].as_bool(),
        Some(false),
        "compatibility bool retains the unverified synchronous attempt: {inject}"
    );
    assert_eq!(
        inject["delivery_state"].as_str(),
        Some("sent_unverified"),
        "compatibility state retains the attempt-time result: {inject}"
    );
    assert_eq!(
        inject["wake_intent"]["status"].as_str(),
        Some("sent_unverified"),
        "durable wake records the original transport attempt: {inject}"
    );
    assert_final_ack_truth(inject);
}

#[test]
fn target_ack_reconciles_ledger_queue_to_final_delivery_truth() {
    let sandbox = ChannelSandbox::spawn();
    let target = "ledger-ack-agent";
    let handoff = sandbox.rally_json(&[
        "say",
        "handoff",
        "--json",
        "--tool",
        "sender:01",
        "--target",
        target,
        "--subject",
        "ledger ACK reconciliation",
    ]);
    let handoff_id = handoff["data"]["say"]["fact"]["event_id"]
        .as_str()
        .expect("handoff event_id");
    let resolver = post_target_ack_after_delay(&sandbox, target, handoff_id);

    let envelope = sandbox.rally_json(&[
        "inject",
        target,
        "--json",
        "--handoff",
        handoff_id,
        "--require-ack",
        "--timeout-seconds",
        "3",
        "--tool",
        "sender:01",
    ]);
    resolver.join().expect("target ACK thread");
    let inject = &envelope["data"]["inject"];

    assert_eq!(inject["target_kind"].as_str(), Some("ledger_agent"));
    assert_eq!(
        inject["delivered"].as_bool(),
        Some(false),
        "ledger-only compatibility bool stays tied to synchronous transport: {inject}"
    );
    assert_eq!(
        inject["delivery_state"].as_str(),
        Some("pending"),
        "ledger compatibility state retains the append-time result: {inject}"
    );
    assert_eq!(
        inject["wake_intent"]["status"].as_str(),
        Some("pending"),
        "durable wake records the original ledger append state: {inject}"
    );
    assert_final_ack_truth(inject);
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

#[test]
fn managed_session_backend_failure_marks_wake_failed_not_delivered() {
    // Regression for stale managed targets: a vanished tmux pane makes the
    // legacy backend command fail. The wake fact must not claim delivered.
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("stale");
    let target = sandbox.add_tmux_session(&name);

    let envelope = sandbox.rally_json(&[
        "inject",
        &target,
        "--json",
        "--text",
        "wake stale target",
        "--tool",
        "claude_code:test-sender",
        "--tmux-bin",
        "/usr/bin/false",
    ]);
    let inject = &envelope["data"]["inject"];

    assert_eq!(inject["delivered"].as_bool(), Some(false));
    assert_eq!(inject["delivery_state"].as_str(), Some("failed"));
    assert_eq!(inject["wake_intent"]["kind"].as_str(), Some("wake"));
    assert_eq!(inject["wake_intent"]["status"].as_str(), Some("failed"));
    assert!(
        inject["wake_intent"]["subject"]
            .as_str()
            .unwrap_or("")
            .contains("failed"),
        "wake subject must surface the failed state: {inject:?}",
    );
}

// ---------------------------------------------------------------------------
// feat/inject-ledger-target — ledger-only delivery to a rally-termd-registered
// agent (no managed session). These tests prove the new `resolve_inject_target`
// resolution order and the second-bug fix (no double-delivery on the
// ledger-only path).
// ---------------------------------------------------------------------------

/// PROBLEM: before this change, `rally inject` with a target that wasn't a
/// registered managed session errored `"unknown managed session ..."` — even
/// though the target was a perfectly valid `agent.register`-bound ptyd-pane
/// identity. PROOF: a fresh sandbox (no `rally run` first) accepts an inject
/// against a valid agent-id and writes the Directive to the ledger.
#[test]
fn inject_to_unregistered_valid_agent_id_writes_ledger_directive() {
    let sandbox = ChannelSandbox::spawn();
    // No `add_tmux_session` call — the sandbox has NO registered managed
    // sessions. The target is a syntactically valid agent-id only.
    let agent = "claude";

    let outcome = sandbox.inject_unregistered(agent, "claude_code:test-sender", "wake up");

    // Directive landed.
    assert!(
        outcome.directive_seq.is_some(),
        "ledger-only inject must assign a Directive seq; outcome={outcome:?}",
    );
    // Reports `pending` (rally-termd will post a Receipt when it executes the
    // PTY-write; absent the daemon the state stays `pending`).
    assert_eq!(
        outcome.delivery_state, "pending",
        "ledger-only inject must report pending (no legacy backend to flip it); outcome={outcome:?}",
    );
    assert_eq!(
        outcome.raw["wake_intent"]["status"].as_str(),
        Some("pending"),
        "wake fact must mirror the pending ledger-only state, not claim delivered",
    );
    assert!(
        !outcome.delivered,
        "the `delivered` legacy bool tracks the synchronous backend outcome; \
         the ledger-only path never runs one. outcome={outcome:?}",
    );
    // directive_to mirrors the resolved agent id (not a session.tool).
    assert_eq!(
        outcome.directive_to.as_deref(),
        Some(agent),
        "directive_to must be the resolved agent-id on the ledger-only path",
    );
    // target_kind discriminator is the authoritative shape signal.
    assert_eq!(
        outcome.raw["target_kind"].as_str(),
        Some("ledger_agent"),
        "target_kind must signal ledger_agent for an unregistered valid id",
    );
    // session is null on the ledger-only path (was unconditionally required
    // before the change — that's the root cause of the bug we're fixing).
    assert!(
        outcome.raw["session"].is_null(),
        "session must be null when there is no ManagedSession backing the target",
    );
    // RCA 2026-07-09 follow-up: the ledger arm surfaces the pre-wait
    // injectability diagnosis at t=0 so callers don't reconstruct it
    // post-timeout from scattered fields.
    assert_eq!(
        outcome.raw["target_injectability"]["injectable"].as_bool(),
        Some(false),
        "ledger-agent target must report injectable=false (no live pane rally can see)",
    );
    assert_eq!(
        outcome.raw["target_injectability"]["status"].as_str(),
        Some("presence_only_unmanaged"),
        "status must reuse the room agent_injectability vocabulary",
    );

    // Reader-side proof: the Directive is on disk under the agent's inbox,
    // not under any session.tool name.
    let directives = sandbox.read_directives(agent, 0);
    assert_eq!(
        directives.len(),
        1,
        "exactly one Directive in the agent's ledger inbox",
    );
    let d = &directives[0];
    assert_eq!(
        d.to, agent,
        "Directive.to is the agent-id, not a session.tool"
    );
    assert_eq!(d.from, "claude_code:test-sender");
    assert_eq!(d.text.as_deref(), Some("wake up"));
    assert!(!d.urgent);
}

/// SECOND-BUG FIX: the orchestrator brief calls out that the managed-session
/// arm DELIBERATELY double-delivers (ledger Directive + legacy tmux/cmux
/// backend, intentional in P2). The ledger-only arm MUST NOT — it has no
/// backend to call. PROOF: a ledger-only inject's `commands` plan is empty
/// (the legacy backend was never queried) and the `delivered` bool is always
/// false (no backend success could have flipped it true).
#[test]
fn inject_ledger_only_does_not_double_deliver_via_backend() {
    let sandbox = ChannelSandbox::spawn();
    let agent = "termd-pane-agent";

    let outcome = sandbox.inject_unregistered(agent, "claude_code:test-sender", "x");

    // The legacy `delivered` bool tracks the synchronous backend outcome. On
    // the ledger-only path there is no backend; this MUST be false.
    assert!(
        !outcome.delivered,
        "ledger-only inject must NOT report `delivered: true` — that would mean \
         the legacy synchronous backend ran ALONGSIDE the ledger write, which is \
         the second bug we're fixing. outcome={outcome:?}",
    );
    // The `commands` plan is the legacy backend's keystroke plan. Empty here
    // proves the backend path was never traversed.
    let commands = outcome.raw["commands"].as_array().expect("commands array");
    assert!(
        commands.is_empty(),
        "ledger-only inject must have an empty commands plan (no backend to \
         build keystrokes for); got {commands:?}",
    );
}

/// RCA 2026-07-09: `inject --require-ack` against a presence-only agent used
/// to block the full timeout and report the cause only via scattered
/// post-hoc fields. The WARN-and-wait fix keeps the wait (a polling agent or
/// rally-termd-registered pane can still resolve — an async producer exists)
/// but surfaces the diagnosis at t=0 in `target_injectability` and stamps
/// `pre_diagnosis` into the timeout fallback plan. This test drives the full
/// timeout path: 1s wait, no resolver → `ack_state: timeout` with the cause
/// pre-diagnosed, NOT an error and NOT a short-circuit.
#[test]
fn inject_ledger_ack_timeout_carries_pre_wait_injectability_diagnosis() {
    let sandbox = ChannelSandbox::spawn();
    let agent = "presence-only-agent";

    let envelope = sandbox.rally_json(&[
        "inject",
        agent,
        "--json",
        "--handoff",
        "handoff-rca-followup",
        "--require-ack",
        "--timeout-seconds",
        "1",
        "--tool",
        "claude_code:test-sender",
    ]);
    let inject = envelope
        .pointer("/data/inject")
        .unwrap_or_else(|| panic!("envelope has no /data/inject: {envelope}"));

    // The wait ran and timed out — it was NOT short-circuited into an error
    // or a fabricated ack.
    assert_eq!(
        inject["ack_state"].as_str(),
        Some("timeout"),
        "no resolver exists, so the (kept) wait must time out; inject={inject}",
    );
    // The t=0 diagnosis is in the envelope...
    assert_eq!(
        inject["target_injectability"]["status"].as_str(),
        Some("presence_only_unmanaged"),
        "pre-wait diagnosis must be surfaced on the ack-timeout path; inject={inject}",
    );
    // ...and the timeout fallback plan carries the pre-diagnosed CAUSE, not
    // just the symptom.
    let pre_diagnosis = inject["fallback_plan"]["pre_diagnosis"]
        .as_str()
        .unwrap_or_else(|| panic!("fallback_plan must carry pre_diagnosis: {inject}"));
    assert!(
        pre_diagnosis.contains("presence-only"),
        "pre_diagnosis must name the t=0 cause; got {pre_diagnosis}",
    );
}

/// PROOF of the resolution-order contract: managed-session match wins over
/// agent-id validity. If a `target` string happens to be both a registered
/// managed-session tool AND a syntactically valid agent-id, the managed arm
/// fires and the legacy dual-delivery is preserved.
#[test]
fn inject_managed_session_wins_over_agent_id_when_both_resolve() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("dual");
    let target = sandbox.add_tmux_session(&name);
    // `target` is now BOTH a valid managed-session name AND a valid agent-id
    // (it's an allowlist-clean string). The managed-session arm must fire.

    let outcome = sandbox.inject(&target, "claude_code:test-sender", "hello");

    assert_eq!(
        outcome.raw["target_kind"].as_str(),
        Some("managed_session"),
        "a target that matches an active managed session MUST resolve as \
         managed_session, NOT ledger_agent (resolution-order contract)",
    );
    // session must be present (preserves existing consumers that read
    // /data/inject/session/name etc.).
    assert!(
        outcome.raw["session"].is_object(),
        "session must be present on the managed-session arm; got {:?}",
        outcome.raw["session"],
    );
    // commands plan is non-empty (the legacy backend produces tmux keystrokes
    // even with the tmux-true stub).
    let commands = outcome.raw["commands"].as_array().expect("commands array");
    assert!(
        !commands.is_empty(),
        "managed-session inject must produce a backend command plan; got empty"
    );
}

/// PROOF the invalid-id arm preserves the existing error message so an
/// operator who typo'd a session name doesn't get a confusing path-traversal
/// error. The lib's `resolve_inject_target` returns `unknown managed session
/// {target}` for both (a) a clean string that just doesn't match any session
/// AND (b) garbage that can't be a valid agent-id either. Together with
/// `inject_security.rs::sec006_malformed_sender_ids_rejected` (which covers
/// the sender id), this gates the target-side too.
#[test]
fn inject_to_invalid_target_id_is_rejected_at_resolution() {
    let sandbox = ChannelSandbox::spawn();

    // `..` is not a valid agent-id AND not a registered session. Must error.
    let out = sandbox.rally_try(&[
        "inject",
        "..",
        "--json",
        "--text",
        "x",
        "--tool",
        "claude_code:test-sender",
    ]);
    assert!(
        !out.status.success(),
        "an invalid target id MUST be rejected; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // A target carrying a path separator likewise must not write anything to
    // a ledger inbox.
    let bad = sandbox.rally_try(&[
        "inject",
        "bad/slash",
        "--json",
        "--text",
        "x",
        "--tool",
        "claude_code:test-sender",
    ]);
    assert!(
        !bad.status.success(),
        "a target id with a path separator MUST be rejected; stdout={} stderr={}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr),
    );
}
