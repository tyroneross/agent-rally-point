// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Agent adapters: bridge between the `Supervisor` and the live CLI subprocesses.
//!
//! The `Adapter` trait (previously in `supervisor`) is the canonical surface.
//! Moving it here keeps supervisor.rs free of process-spawning concerns.
//!
//! Command channel type is also defined here (resolves the `TODO(chunk-B)`
//! in supervisor.rs): `SessionCommand` is the real message type for sending
//! instructions to a live subprocess.

pub mod claude;
pub mod codex;

use uuid::Uuid;

// Re-export so callers can use `adapter::Adapter` etc.
pub use crate::supervisor::Adapter;

// ── SessionCommand ─────────────────────────────────────────────────────────────

/// Commands that can be sent to a running live session.
///
/// This resolves `TODO(chunk-B)` in supervisor.rs: the `LiveSession._cmd_tx`
/// will carry `mpsc::Sender<SessionCommand>`.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Send a new user-turn prompt.
    SendPrompt(String),
    /// Inject a steering message mid-run (e.g. "focus on X").
    Steer(String),
    /// Resolve a pending approval.
    Approve {
        id: Uuid,
        decision: ApprovalDecision,
    },
    /// Terminate the session immediately.
    Kill,
}

/// Decision for an approval gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

impl ApprovalDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
        }
    }
}
