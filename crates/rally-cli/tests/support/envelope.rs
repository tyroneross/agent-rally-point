// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! Tell a typed refusal apart from a watchdog envelope.
//!
//! # The failure this prevents
//!
//! A rally command that refuses emits, from `CliError::error_text`:
//!
//! ```json
//! { "ok": false, "product": "rally", "error": "<message string>", "exit_code": 2 }
//! ```
//!
//! A rally command that blows its wall-clock budget emits one of five
//! watchdog envelopes (`crates/rally-cli/src/lib.rs`), and every one of them
//! carries `"command": "watchdog"`:
//!
//! | site | `ok` | `error` |
//! |------|------|---------|
//! | `timeout_fail_open_payload` | `false` | absent |
//! | `emit_timeout_fail_closed_mutation` | `false` | object `{code, message}` |
//! | outcome-unknown | `false` | object |
//! | db-only-migration outcome-unknown | `false` | object |
//! | post-commit projection abandoned | **`true`** | absent |
//!
//! So `assert_eq!(body["ok"], false)` is satisfied by a timeout, and
//! `assert_eq!(body["ok"], true)` is satisfied by the fifth one. A test that
//! asserts only on `ok` cannot tell "the system correctly refused" from "the
//! system ran out of time", and passes either way while proving nothing.
//!
//! The visible half of the same bug is `body["error"].as_str().unwrap()`
//! panicking on `None` when `error` arrived as an object or not at all —
//! observed at `referenced_handoff_targeting.rs:424` and `:581`. The panic is
//! the lucky case. It is loud. The silent pass is the defect.
//!
//! # Contract
//!
//! Assert refusals through [`assert_refusal`]. It fails with the watchdog's
//! own diagnosis when the envelope is a watchdog envelope, and never reads a
//! timeout as a refusal.

use serde_json::Value;

/// Every watchdog envelope sets this `command`, and no real subcommand does.
const WATCHDOG_COMMAND: &str = "watchdog";

/// Describe the watchdog envelope in `body`, or `None` if it is not one.
///
/// The discriminator is `command == "watchdog"`, which holds across all five
/// emission sites regardless of whether `error` is a string, an object, or
/// absent, and regardless of `ok`.
pub fn watchdog_diagnosis(body: &Value) -> Option<String> {
    if body.get("command").and_then(Value::as_str) != Some(WATCHDOG_COMMAND) {
        return None;
    }
    let code = body
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("watchdog-timeout-fail-open");
    let elapsed = body
        .get("data")
        .and_then(|d| d.get("elapsed_ms"))
        .or_else(|| {
            body.get("data")
                .and_then(|d| d.get("watchdog"))
                .and_then(|w| w.get("timeout_ms"))
        })
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Some(format!(
        "the command hit the wall-clock watchdog ({code}, budget/elapsed {elapsed}ms) \
         and never reached the code path under test. This is NOT the refusal this \
         assertion is about. Raise the budget at the ONE place it is set — \
         TEST_WATCHDOG_TIMEOUT_MS in tests/support/rally_cmd.rs — or find what held \
         the command; do not add a per-call-site override"
    ))
}

/// Fail loudly if `body` is a watchdog envelope.
///
/// Call this before any assertion whose truth a timeout could satisfy —
/// including `assert_eq!(body["ok"], true)`, because the post-commit
/// projection-abandoned envelope is `ok: true`.
#[track_caller]
pub fn reject_watchdog(body: &Value, context: &str) {
    if let Some(why) = watchdog_diagnosis(body) {
        panic!("{context}: {why}\nenvelope: {body}");
    }
}

/// Return the typed refusal message from `body`, or panic with a legible
/// reason.
///
/// Three distinct failures, three distinct messages — none of them a bare
/// `Option::unwrap()` on `None`:
///
/// 1. watchdog envelope -> names the timeout and where the budget lives;
/// 2. not a refusal at all (`ok` is not `false`) -> says so;
/// 3. `error` present but not a string -> prints the shape it did get.
#[track_caller]
pub fn refusal_message(body: &Value) -> &str {
    reject_watchdog(body, "expected a typed refusal");
    match body.get("ok") {
        Some(Value::Bool(false)) => {}
        other => panic!(
            "expected a refusal (ok:false), got ok:{}\nenvelope: {body}",
            other.map(|v| v.to_string()).unwrap_or("<absent>".into())
        ),
    }
    match body.get("error") {
        Some(Value::String(s)) => s,
        Some(other) => panic!(
            "refusal `error` must be a string, got {}: {other}\nenvelope: {body}",
            shape_of(other)
        ),
        None => panic!("refusal carries no `error` field\nenvelope: {body}"),
    }
}

/// Assert that `body` is a typed refusal whose message contains `needle`.
///
/// Replaces the pair
/// ```ignore
/// assert_eq!(body["ok"], false);
/// assert!(body["error"].as_str().unwrap().contains(needle));
/// ```
/// which passes on a watchdog timeout at the first line and panics
/// uninformatively at the second.
#[track_caller]
pub fn assert_refusal(body: &Value, needle: &str) {
    let message = refusal_message(body);
    assert!(
        message.contains(needle),
        "refusal message did not mention `{needle}`\nmessage: {message}\nenvelope: {body}"
    );
}

fn shape_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn typed_refusal() -> Value {
        json!({
            "ok": false,
            "product": "rally",
            "error": "handoff_third_party_reply_forbidden: receiver is bound",
            "exit_code": 2
        })
    }

    /// Shape copied from `emit_timeout_fail_closed_mutation`.
    fn watchdog_fail_closed() -> Value {
        json!({
            "ok": false,
            "product": "rally",
            "command": "watchdog",
            "error": { "code": "watchdog-timeout-uncommitted-mutation", "message": "..." },
            "data": { "watchdog": { "committed": false, "timeout_ms": 3000 } }
        })
    }

    /// Shape copied from `timeout_fail_open_payload` — no `error` field.
    fn watchdog_fail_open() -> Value {
        json!({
            "ok": false,
            "product": "rally",
            "command": "watchdog",
            "schema": "agent-rally.command.watchdog.v1",
            "data": { "watchdog_timeout": true, "elapsed_ms": 3001 }
        })
    }

    /// The insidious one: `ok` is TRUE, so an `assert!(body["ok"] == true)`
    /// success assertion also passes on a timeout.
    fn watchdog_projection_abandoned() -> Value {
        json!({
            "ok": true,
            "product": "rally",
            "command": "watchdog",
            "data": { "watchdog": { "committed": true, "timeout_ms": 3000 } }
        })
    }

    #[test]
    fn typed_refusal_passes() {
        assert_refusal(&typed_refusal(), "handoff_third_party_reply_forbidden");
    }

    #[test]
    fn typed_refusal_with_the_wrong_code_fails() {
        let err = std::panic::catch_unwind(|| {
            assert_refusal(&typed_refusal(), "some_other_refusal");
        })
        .unwrap_err();
        let msg = panic_text(&err);
        assert!(msg.contains("did not mention"), "got: {msg}");
    }

    #[test]
    fn every_watchdog_shape_is_detected() {
        for (name, body) in [
            ("fail-closed", watchdog_fail_closed()),
            ("fail-open", watchdog_fail_open()),
            ("projection-abandoned", watchdog_projection_abandoned()),
        ] {
            assert!(
                watchdog_diagnosis(&body).is_some(),
                "{name} envelope not recognised as a watchdog envelope"
            );
        }
        assert!(watchdog_diagnosis(&typed_refusal()).is_none());
    }

    /// The regression under test: a watchdog timeout must NOT read as a
    /// refusal. Pre-fix this assertion pair passed line 1 and panicked on
    /// line 2 with `Option::unwrap()` on `None`.
    #[test]
    fn watchdog_envelope_fails_as_a_timeout_not_as_a_refusal() {
        for body in [
            watchdog_fail_closed(),
            watchdog_fail_open(),
            watchdog_projection_abandoned(),
        ] {
            let err = std::panic::catch_unwind(|| {
                assert_refusal(&body, "handoff_third_party_reply_forbidden");
            })
            .unwrap_err();
            let msg = panic_text(&err);
            assert!(
                msg.contains("wall-clock watchdog"),
                "watchdog timeout must name itself; got: {msg}"
            );
            assert!(
                !msg.contains("called `Option::unwrap()`"),
                "watchdog timeout must not surface as an unwrap panic; got: {msg}"
            );
        }
    }

    #[test]
    fn success_envelope_is_not_a_refusal() {
        let ok = json!({ "ok": true, "product": "rally", "command": "say", "data": {} });
        let err = std::panic::catch_unwind(|| refusal_message(&ok)).unwrap_err();
        assert!(panic_text(&err).contains("expected a refusal"));
    }

    fn panic_text(err: &Box<dyn std::any::Any + Send>) -> String {
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    }
}
