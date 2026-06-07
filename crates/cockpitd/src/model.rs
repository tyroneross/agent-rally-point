// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Domain types: Session, Event, Approval, SessionStatus.
//!
//! Wire invariants (COCKPIT-WIRE.md §Invariants):
//! - `agent_type` is an open `String` — never an enum.
//! - `SessionStatus` tolerates unknown variants (catch-all `Unknown`).
//! - `Event.kind` is a plain `String` — unknown kinds MUST NOT fail decode.
//! - `metadata`/`args` are `serde_json::Value`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── SessionStatus ─────────────────────────────────────────────────────────────

/// Status of a supervised session. The `#[serde(other)]` variant makes
/// unknown future statuses decode as `Unknown` rather than failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionStatus {
    Active,
    AwaitingInput,
    Paused,
    Stale,
    Completed,
    Failed,
    Killed,
    #[default]
    Disconnected,
    /// Forward-compat catch-all: an unknown status string decodes here.
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Active => "active",
            Self::AwaitingInput => "awaiting_input",
            Self::Paused => "paused",
            Self::Stale => "stale",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Disconnected => "disconnected",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

impl SessionStatus {
    /// True when no further transitions are possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Killed | Self::Disconnected
        )
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

/// A supervised agent session.
///
/// `agent_type` is an open string (invariant 1 of COCKPIT-WIRE.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    /// Multi-user seam; default "local".
    pub owner_id: String,
    /// Open string — "claude", "codex", or any future agent.
    pub agent_type: String,
    pub repo_path: String,
    pub status: SessionStatus,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Highest seq assigned to this session (0 = no events yet).
    pub last_seq: u64,
}

// ── Event ─────────────────────────────────────────────────────────────────────

/// A sequence-numbered event on a session stream.
///
/// `kind` is a plain String so unknown future kinds decode without error.
/// Known kinds: message | tool_call | tool_result | diff | status |
///              approval_request | error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub session_id: Uuid,
    /// Monotonic per-session, starts at 1.
    pub seq: u64,
    /// "agent" | "user" | "system"
    pub sender: String,
    /// Open string — unknown kinds must not fail decode.
    pub kind: String,
    pub content: String,
    pub requires_user_input: bool,
    pub created_at: DateTime<Utc>,
    /// Free-form JSON object.
    pub metadata: serde_json::Value,
}

// ── Approval ──────────────────────────────────────────────────────────────────

/// A pending tool-call approval (also emitted as an Event of kind "approval_request").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: Uuid,
    pub session_id: Uuid,
    /// Seq of the event that triggered this approval.
    pub event_seq: u64,
    pub tool: String,
    /// Free-form JSON object matching the tool's argument schema.
    pub args: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub ttl_secs: u64,
    /// null | "allow" | "deny" | "auto_denied" | "aborted"
    pub resolution: Option<String>,
}
