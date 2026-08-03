// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Deny-by-default command authorization (spec §9).
//!
//! Policy engine: given a tool name, its arguments, and an `AuthzPolicy`,
//! returns a `Decision` of `Permit` (auto-allowed) or `RequireApproval` (gate
//! on explicit user action before proceeding).
//!
//! Rules (in priority order):
//! 1. Free-form shell / unknown tools → always `RequireApproval`.
//! 2. Explicit deny-list entry → `RequireApproval` (overrides allowlist).
//! 3. Allowlisted tool → `Permit`.
//! 4. Default → `RequireApproval` (deny-by-default).
//!
//! The function `decide` is a pure, deterministic function with no I/O — easy
//! to unit-test and easy to audit.
//!
//! ## Integration note
//! The `decide` function is wired at the point where `tool_call` Events flow
//! through the supervisor/transport pump. When a `tool_call` event arrives:
//!   - If `decide → Permit`, the event is broadcast normally.
//!   - If `decide → RequireApproval`, a pending `Approval` is created, the
//!     `approval_request` frame is broadcast, and the pump parks until a client
//!     resolves it.
//!
//! ## This policy is not enforced against the agent (ARP-003)
//!
//! "Deny-by-default" here means the *event stream* is held and, on denial, not
//! forwarded. It does not mean the tool did not run. Cockpit spawns the agent
//! CLI as a child process and only reads its stdout; a `tool_call` is observed
//! after the child has already decided to act, and parking our reader neither
//! stops the child nor tells it anything. See `transport::ws::run_pump` for the
//! full statement and `arp003_execution_gate_definition_of_done` in
//! `tests/e2e.rs` for what would close the gap.
//!
//! The decision function itself is real, pure, and tested. Its output is a
//! recommendation surfaced to an operator, not a control on the agent.

use serde::{Deserialize, Serialize};

// ── Decision ──────────────────────────────────────────────────────────────────

/// The outcome of a policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// The tool call is permitted automatically — no user action required.
    Permit,
    /// The tool call requires explicit user approval before proceeding.
    RequireApproval,
}

// ── AuthzPolicy ───────────────────────────────────────────────────────────────

/// Authorization policy configuration.
///
/// `allowlist`: set of tool names that are permitted without approval.
/// `denylist`:  set of tool names that always require approval even if also on the allowlist.
///
/// Default policy is conservative: empty allowlist (everything requires approval).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthzPolicy {
    /// Tools that auto-permit without a separate approval gate.
    pub allowlist: Vec<String>,
    /// Tools that always require approval regardless of allowlist.
    pub denylist: Vec<String>,
}

impl AuthzPolicy {
    /// Create a policy from an explicit allowlist; empty denylist.
    pub fn with_allowlist(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowlist: tools.into_iter().map(Into::into).collect(),
            denylist: vec![],
        }
    }

    /// Conservative default: read-only filesystem commands auto-permitted;
    /// shell execution always requires approval.
    pub fn conservative() -> Self {
        Self::with_allowlist(["read_file", "list_files", "get_session", "list_sessions"])
    }
}

// ── Shell-like tool heuristics ────────────────────────────────────────────────

/// Returns `true` if the tool name looks like free-form shell execution.
///
/// Matches: "shell", "bash", "sh", "cmd", "exec", "run", "terminal",
/// and any tool whose args contain a "cmd" or "command" key (heuristic).
fn is_shell_like(tool: &str, args: &serde_json::Value) -> bool {
    let lower = tool.to_lowercase();
    if matches!(
        lower.as_str(),
        "shell" | "bash" | "sh" | "cmd" | "exec" | "run" | "terminal" | "powershell" | "zsh"
    ) {
        return true;
    }

    // Heuristic: if args contains a "cmd" or "command" key it's likely shell
    if let Some(obj) = args.as_object()
        && (obj.contains_key("cmd") || obj.contains_key("command"))
    {
        return true;
    }

    false
}

// ── Policy decision ───────────────────────────────────────────────────────────

/// Determine whether a tool call is permitted or requires approval.
///
/// This is a pure function — no I/O, no side effects, fully deterministic.
///
/// # Arguments
/// - `tool`   — the tool name as emitted by the adapter (e.g. "bash", "read_file")
/// - `args`   — the tool's argument JSON object
/// - `policy` — the active `AuthzPolicy`
pub fn decide(tool: &str, args: &serde_json::Value, policy: &AuthzPolicy) -> Decision {
    // Rule 1: shell-like tools always require approval (highest priority).
    if is_shell_like(tool, args) {
        return Decision::RequireApproval;
    }

    // Rule 2: explicit deny-list overrides the allowlist.
    if policy.denylist.iter().any(|d| d == tool) {
        return Decision::RequireApproval;
    }

    // Rule 3: explicit allowlist → permit.
    if policy.allowlist.iter().any(|a| a == tool) {
        return Decision::Permit;
    }

    // Rule 4: deny-by-default.
    Decision::RequireApproval
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_args() -> serde_json::Value {
        json!({})
    }

    // ── authz::allowlisted → Permit ───────────────────────────────────────────

    #[test]
    fn allowlisted_tool_permits() {
        let policy = AuthzPolicy::with_allowlist(["read_file", "list_files"]);
        assert_eq!(
            decide("read_file", &empty_args(), &policy),
            Decision::Permit
        );
        assert_eq!(
            decide("list_files", &empty_args(), &policy),
            Decision::Permit
        );
    }

    // ── authz::unknown tool → RequireApproval ─────────────────────────────────

    #[test]
    fn unknown_tool_requires_approval() {
        let policy = AuthzPolicy::with_allowlist(["read_file"]);
        assert_eq!(
            decide("write_file", &empty_args(), &policy),
            Decision::RequireApproval,
            "unknown tool not in allowlist must require approval"
        );
    }

    // ── authz::shell-like → RequireApproval ───────────────────────────────────

    #[test]
    fn shell_like_tool_names_require_approval() {
        let policy = AuthzPolicy::with_allowlist(["bash", "shell", "exec"]);
        // Shell-like by name overrides even if on the allowlist.
        for name in &[
            "bash", "shell", "sh", "cmd", "exec", "run", "terminal", "zsh",
        ] {
            assert_eq!(
                decide(name, &empty_args(), &policy),
                Decision::RequireApproval,
                "shell-like tool '{name}' must require approval regardless of allowlist"
            );
        }
    }

    #[test]
    fn tool_with_cmd_arg_requires_approval() {
        let policy = AuthzPolicy::with_allowlist(["my_tool"]);
        // Tool named "my_tool" (allowlisted) but has a "cmd" arg → shell-like heuristic.
        let args_with_cmd = json!({"cmd": "rm -rf /"});
        assert_eq!(
            decide("my_tool", &args_with_cmd, &policy),
            Decision::RequireApproval,
            "tool with 'cmd' arg must require approval"
        );
    }

    #[test]
    fn tool_with_command_arg_requires_approval() {
        let policy = AuthzPolicy::with_allowlist(["my_tool"]);
        let args_with_command = json!({"command": "ls /tmp"});
        assert_eq!(
            decide("my_tool", &args_with_command, &policy),
            Decision::RequireApproval,
            "tool with 'command' arg must require approval"
        );
    }

    // ── authz::deny-list overrides allowlist ──────────────────────────────────

    #[test]
    fn denylist_overrides_allowlist() {
        let policy = AuthzPolicy {
            allowlist: vec!["read_file".into(), "dangerous_tool".into()],
            denylist: vec!["dangerous_tool".into()],
        };
        // On allowlist but also on denylist → RequireApproval.
        assert_eq!(
            decide("dangerous_tool", &empty_args(), &policy),
            Decision::RequireApproval,
            "denylist must override allowlist"
        );
        // Normal allowlisted tool not on denylist.
        assert_eq!(
            decide("read_file", &empty_args(), &policy),
            Decision::Permit
        );
    }

    // ── authz::empty policy → deny-by-default ────────────────────────────────

    #[test]
    fn empty_policy_denies_everything() {
        let policy = AuthzPolicy::default();
        assert_eq!(
            decide("read_file", &empty_args(), &policy),
            Decision::RequireApproval,
            "empty policy must deny everything by default"
        );
        assert_eq!(
            decide("any_tool", &empty_args(), &policy),
            Decision::RequireApproval,
        );
    }

    // ── authz::conservative preset ────────────────────────────────────────────

    #[test]
    fn conservative_policy_permits_read_operations() {
        let policy = AuthzPolicy::conservative();
        assert_eq!(
            decide("read_file", &empty_args(), &policy),
            Decision::Permit
        );
        assert_eq!(
            decide("list_files", &empty_args(), &policy),
            Decision::Permit
        );
    }

    #[test]
    fn conservative_policy_denies_write_operations() {
        let policy = AuthzPolicy::conservative();
        assert_eq!(
            decide("write_file", &empty_args(), &policy),
            Decision::RequireApproval
        );
        assert_eq!(
            decide("delete_file", &empty_args(), &policy),
            Decision::RequireApproval
        );
    }

    // ── authz::explicit deny on empty tool name ───────────────────────────────

    #[test]
    fn empty_tool_name_requires_approval() {
        let policy = AuthzPolicy::with_allowlist(["read_file"]);
        assert_eq!(
            decide("", &empty_args(), &policy),
            Decision::RequireApproval,
            "empty tool name must require approval"
        );
    }
}
