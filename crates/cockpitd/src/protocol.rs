// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Wire frame envelope.
//!
//! Every frame carries a `"t"` tag that discriminates the variant.
//! Serde's `#[serde(tag = "t", rename_all = "snake_case")]` encodes this.
//!
//! Resilience invariants (COCKPIT-WIRE.md §Invariants):
//! - Unknown `t` values MUST NOT fail decode. A catch-all `Unknown` variant
//!   absorbs any unrecognised frame so the caller can log-and-ignore.
//! - `agent_type` is a plain `String` (see model.rs).
//! - `Event.kind` and `SessionStatus` tolerate unknowns (see model.rs).
//!
//! Note on JsonSchema: `Uuid` and `DateTime<Utc>` don't implement `JsonSchema`
//! with the workspace dependency versions, so we hand-craft the schema in
//! `protocol_schema()` rather than using `#[derive(JsonSchema)]` on the full
//! frame types. The schema function still returns a valid, non-null JSON Schema
//! document satisfying the A1 contract.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{Approval, Event, Session, SessionStatus};

// ── Client → server commands ──────────────────────────────────────────────────

/// Commands sent by the client (iOS app or cockpit-cli) to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Auth handshake; must be the first frame.
    Hello { token: String, protocol: u32 },
    /// Request current session list.
    ListSessions,
    /// Subscribe to a session and replay events from `from_seq` (exclusive).
    OpenSession { session_id: Uuid, from_seq: u64 },
    /// Send a new prompt turn.
    SendPrompt { session_id: Uuid, text: String },
    /// Inject a steering message mid-run.
    Steer { session_id: Uuid, text: String },
    /// Resolve a pending approval.
    Approve {
        approval_id: Uuid,
        decision: ApproveDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Start a new agent session.
    LaunchSession {
        /// Open string — "claude", "codex", …
        agent_type: String,
        repo_path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Stop/kill a session.
    CloseSession { session_id: Uuid },
    /// Keepalive.
    Ping,
    /// Retrieve audit log entries. Optionally filter by session_id and cap with limit.
    GetAudit {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    /// Forward-compat catch-all; any unknown `t` decodes here.
    #[serde(other)]
    Unknown,
}

/// Approval decision values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproveDecision {
    Allow,
    Deny,
}

// ── Server → client events ────────────────────────────────────────────────────

/// Events emitted by the daemon to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Auth accepted.
    HelloOk {
        server_version: String,
        protocol: u32,
    },
    /// Protocol or command error.
    Error { code: String, message: String },
    /// Snapshot of all sessions.
    SessionList { sessions: Vec<Session> },
    /// Sent in response to `open_session`: replays + current state.
    Snapshot {
        session_id: Uuid,
        session: Session,
        events: Vec<Event>,
        cursor_seq: u64,
    },
    /// Live delta; carries the event's own `seq`.
    Event {
        session_id: Uuid,
        event: crate::model::Event,
    },
    /// Status transition notification.
    SessionStatus {
        session_id: Uuid,
        status: SessionStatus,
    },
    /// A tool call requires user approval.
    ApprovalRequest { approval: Approval },
    /// Keepalive reply.
    Pong,
    /// Response to `get_audit`: ordered list of audit entries.
    AuditList {
        entries: Vec<crate::audit::AuditEntry>,
    },
    /// Forward-compat catch-all.
    #[serde(other)]
    Unknown,
}

// ── Schema generation ─────────────────────────────────────────────────────────

/// Generate a JSON Schema document for the cockpit wire protocol.
///
/// Returns a non-null `serde_json::Value` with keys `"client_command"` and
/// `"server_event"` each containing a JSON Schema 2020-12 object that
/// describes the valid frame shapes for that direction.
///
/// Note: `Uuid` and `DateTime<Utc>` don't implement `schemars::JsonSchema`
/// with the workspace-pinned crate versions, so this schema is hand-authored
/// rather than derived. It faithfully mirrors COCKPIT-WIRE.md §Frame shape.
pub fn protocol_schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Cockpit Wire Protocol v1",
        "description": "JSON Schema for the cockpit daemon ↔ client WebSocket frames.",
        "client_command": {
            "title": "ClientCommand",
            "description": "Commands sent by the client (iOS app or cockpit-cli) to the daemon.",
            "oneOf": [
                {
                    "title": "hello",
                    "type": "object",
                    "required": ["t", "token", "protocol"],
                    "properties": {
                        "t": { "const": "hello" },
                        "token": { "type": "string" },
                        "protocol": { "type": "integer", "const": 1 }
                    }
                },
                {
                    "title": "list_sessions",
                    "type": "object",
                    "required": ["t"],
                    "properties": { "t": { "const": "list_sessions" } }
                },
                {
                    "title": "open_session",
                    "type": "object",
                    "required": ["t", "session_id", "from_seq"],
                    "properties": {
                        "t": { "const": "open_session" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "from_seq": { "type": "integer", "minimum": 0 }
                    }
                },
                {
                    "title": "send_prompt",
                    "type": "object",
                    "required": ["t", "session_id", "text"],
                    "properties": {
                        "t": { "const": "send_prompt" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "text": { "type": "string" }
                    }
                },
                {
                    "title": "steer",
                    "type": "object",
                    "required": ["t", "session_id", "text"],
                    "properties": {
                        "t": { "const": "steer" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "text": { "type": "string" }
                    }
                },
                {
                    "title": "approve",
                    "type": "object",
                    "required": ["t", "approval_id", "decision"],
                    "properties": {
                        "t": { "const": "approve" },
                        "approval_id": { "type": "string", "format": "uuid" },
                        "decision": { "type": "string", "enum": ["allow", "deny"] },
                        "reason": { "type": "string" }
                    }
                },
                {
                    "title": "launch_session",
                    "type": "object",
                    "required": ["t", "agent_type", "repo_path"],
                    "properties": {
                        "t": { "const": "launch_session" },
                        "agent_type": { "type": "string" },
                        "repo_path": { "type": "string" },
                        "prompt": { "type": "string" }
                    }
                },
                {
                    "title": "close_session",
                    "type": "object",
                    "required": ["t", "session_id"],
                    "properties": {
                        "t": { "const": "close_session" },
                        "session_id": { "type": "string", "format": "uuid" }
                    }
                },
                {
                    "title": "ping",
                    "type": "object",
                    "required": ["t"],
                    "properties": { "t": { "const": "ping" } }
                }
            ]
        },
        "server_event": {
            "title": "ServerEvent",
            "description": "Events emitted by the daemon to the client.",
            "oneOf": [
                {
                    "title": "hello_ok",
                    "type": "object",
                    "required": ["t", "server_version", "protocol"],
                    "properties": {
                        "t": { "const": "hello_ok" },
                        "server_version": { "type": "string" },
                        "protocol": { "type": "integer" }
                    }
                },
                {
                    "title": "error",
                    "type": "object",
                    "required": ["t", "code", "message"],
                    "properties": {
                        "t": { "const": "error" },
                        "code": { "type": "string" },
                        "message": { "type": "string" }
                    }
                },
                {
                    "title": "session_list",
                    "type": "object",
                    "required": ["t", "sessions"],
                    "properties": {
                        "t": { "const": "session_list" },
                        "sessions": { "type": "array", "items": { "$ref": "#/$defs/Session" } }
                    }
                },
                {
                    "title": "snapshot",
                    "type": "object",
                    "required": ["t", "session_id", "session", "events", "cursor_seq"],
                    "properties": {
                        "t": { "const": "snapshot" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "session": { "$ref": "#/$defs/Session" },
                        "events": { "type": "array", "items": { "$ref": "#/$defs/Event" } },
                        "cursor_seq": { "type": "integer", "minimum": 0 }
                    }
                },
                {
                    "title": "event",
                    "type": "object",
                    "required": ["t", "session_id", "event"],
                    "properties": {
                        "t": { "const": "event" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "event": { "$ref": "#/$defs/Event" }
                    }
                },
                {
                    "title": "session_status",
                    "type": "object",
                    "required": ["t", "session_id", "status"],
                    "properties": {
                        "t": { "const": "session_status" },
                        "session_id": { "type": "string", "format": "uuid" },
                        "status": { "$ref": "#/$defs/SessionStatus" }
                    }
                },
                {
                    "title": "approval_request",
                    "type": "object",
                    "required": ["t", "approval"],
                    "properties": {
                        "t": { "const": "approval_request" },
                        "approval": { "$ref": "#/$defs/Approval" }
                    }
                },
                {
                    "title": "pong",
                    "type": "object",
                    "required": ["t"],
                    "properties": { "t": { "const": "pong" } }
                }
            ]
        },
        "$defs": {
            "SessionStatus": {
                "type": "string",
                "enum": ["active", "awaiting_input", "paused", "stale",
                         "completed", "failed", "killed", "disconnected"]
            },
            "Session": {
                "type": "object",
                "required": ["id", "owner_id", "agent_type", "repo_path", "status", "created_at", "last_seq"],
                "properties": {
                    "id": { "type": "string", "format": "uuid" },
                    "owner_id": { "type": "string" },
                    "agent_type": { "type": "string", "description": "Open string — 'claude', 'codex', or any future agent." },
                    "repo_path": { "type": "string" },
                    "status": { "$ref": "#/$defs/SessionStatus" },
                    "title": { "type": ["string", "null"] },
                    "created_at": { "type": "string", "format": "date-time" },
                    "last_seq": { "type": "integer", "minimum": 0 }
                }
            },
            "Event": {
                "type": "object",
                "required": ["session_id", "seq", "sender", "kind", "content",
                             "requires_user_input", "created_at", "metadata"],
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "seq": { "type": "integer", "minimum": 1 },
                    "sender": { "type": "string", "enum": ["agent", "user", "system"] },
                    "kind": { "type": "string", "description": "Open string — unknown kinds tolerated." },
                    "content": { "type": "string" },
                    "requires_user_input": { "type": "boolean" },
                    "created_at": { "type": "string", "format": "date-time" },
                    "metadata": { "type": "object" }
                }
            },
            "Approval": {
                "type": "object",
                "required": ["id", "session_id", "event_seq", "tool", "args", "created_at", "ttl_secs"],
                "properties": {
                    "id": { "type": "string", "format": "uuid" },
                    "session_id": { "type": "string", "format": "uuid" },
                    "event_seq": { "type": "integer", "minimum": 0 },
                    "tool": { "type": "string" },
                    "args": { "type": "object" },
                    "created_at": { "type": "string", "format": "date-time" },
                    "ttl_secs": { "type": "integer", "minimum": 0 },
                    "resolution": { "type": ["string", "null"],
                                    "enum": ["allow", "deny", "auto_denied", "aborted", null] }
                }
            }
        }
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn rt_val<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        let s = serde_json::to_string(v).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    // ── ClientCommand round-trips ─────────────────────────────────────────────

    #[test]
    fn hello_round_trip() {
        let cmd = ClientCommand::Hello {
            token: "tok".into(),
            protocol: 1,
        };
        let back: ClientCommand = rt_val(&cmd);
        let s = serde_json::to_string(&back).unwrap();
        assert!(s.contains(r#""t":"hello""#));
        assert!(s.contains("tok"));
    }

    #[test]
    fn list_sessions_round_trip() {
        let cmd = ClientCommand::ListSessions;
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""t":"list_sessions""#));
        let _back: ClientCommand = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn open_session_round_trip() {
        let id = Uuid::new_v4();
        let cmd = ClientCommand::OpenSession {
            session_id: id,
            from_seq: 42,
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: ClientCommand = serde_json::from_str(&s).unwrap();
        let s2 = serde_json::to_string(&back).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn send_prompt_round_trip() {
        let cmd = ClientCommand::SendPrompt {
            session_id: Uuid::new_v4(),
            text: "hello agent".into(),
        };
        let back: ClientCommand = rt_val(&cmd);
        let s = serde_json::to_string(&back).unwrap();
        assert!(s.contains(r#""t":"send_prompt""#));
    }

    #[test]
    fn steer_round_trip() {
        let cmd = ClientCommand::Steer {
            session_id: Uuid::new_v4(),
            text: "focus on auth".into(),
        };
        let back: ClientCommand = rt_val(&cmd);
        let s = serde_json::to_string(&back).unwrap();
        assert!(s.contains(r#""t":"steer""#));
    }

    #[test]
    fn approve_round_trip() {
        let cmd = ClientCommand::Approve {
            approval_id: Uuid::new_v4(),
            decision: ApproveDecision::Allow,
            reason: Some("looks safe".into()),
        };
        let back: ClientCommand = rt_val(&cmd);
        let s = serde_json::to_string(&back).unwrap();
        assert!(s.contains(r#""t":"approve""#));
        assert!(s.contains("allow"));
    }

    #[test]
    fn launch_session_round_trip() {
        // agent_type is an open string — "gemini" must survive the round-trip
        let cmd = ClientCommand::LaunchSession {
            agent_type: "gemini".into(),
            repo_path: "/tmp/repo".into(),
            prompt: None,
        };
        let back: ClientCommand = rt_val(&cmd);
        let s = serde_json::to_string(&back).unwrap();
        assert!(s.contains("gemini"));
        assert!(s.contains(r#""t":"launch_session""#));
    }

    #[test]
    fn close_session_round_trip() {
        let cmd = ClientCommand::CloseSession {
            session_id: Uuid::new_v4(),
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""t":"close_session""#));
        let _: ClientCommand = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn ping_round_trip() {
        let cmd = ClientCommand::Ping;
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains(r#""t":"ping""#));
        let _: ClientCommand = serde_json::from_str(&s).unwrap();
    }

    // ── ServerEvent round-trips ───────────────────────────────────────────────

    #[test]
    fn hello_ok_round_trip() {
        let evt = ServerEvent::HelloOk {
            server_version: "0.1.0".into(),
            protocol: 1,
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains(r#""t":"hello_ok""#));
        let _: ServerEvent = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn error_round_trip() {
        let evt = ServerEvent::Error {
            code: "auth_failed".into(),
            message: "bad token".into(),
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains(r#""t":"error""#));
        let _: ServerEvent = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn pong_round_trip() {
        let evt = ServerEvent::Pong;
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains(r#""t":"pong""#));
        let _: ServerEvent = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn session_status_event_round_trip() {
        let evt = ServerEvent::SessionStatus {
            session_id: Uuid::new_v4(),
            status: crate::model::SessionStatus::AwaitingInput,
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("awaiting_input"));
        let _: ServerEvent = serde_json::from_str(&s).unwrap();
    }

    // ── Resilience: unknown values MUST NOT panic ─────────────────────────────

    #[test]
    fn unknown_client_command_t_decodes_without_panic() {
        // A future protocol version might add a "set_theme" command.
        let raw = json!({ "t": "set_theme", "value": "dark" });
        let s = raw.to_string();
        let cmd: ClientCommand = serde_json::from_str(&s).unwrap();
        assert!(matches!(cmd, ClientCommand::Unknown));
    }

    #[test]
    fn unknown_server_event_t_decodes_without_panic() {
        let raw = json!({ "t": "future_server_event", "data": 42 });
        let s = raw.to_string();
        let evt: ServerEvent = serde_json::from_str(&s).unwrap();
        assert!(matches!(evt, ServerEvent::Unknown));
    }

    #[test]
    fn unknown_session_status_decodes_without_panic() {
        use crate::model::SessionStatus;
        // A future daemon might emit "migrating" as a status.
        let raw = json!("migrating");
        let status: SessionStatus = serde_json::from_value(raw).unwrap();
        assert!(matches!(status, SessionStatus::Unknown));
    }

    #[test]
    fn unknown_event_kind_decodes_without_panic() {
        use crate::model::Event;
        // "future_kind" must be accepted as a plain string.
        let raw = json!({
            "session_id": Uuid::new_v4(),
            "seq": 1_u64,
            "sender": "agent",
            "kind": "future_kind",
            "content": "payload",
            "requires_user_input": false,
            "created_at": Utc::now().to_rfc3339(),
            "metadata": {}
        });
        let evt: Event = serde_json::from_value(raw).unwrap();
        assert_eq!(evt.kind, "future_kind");
    }

    #[test]
    fn unknown_agent_type_decodes_without_panic() {
        use crate::model::Session;
        let raw = json!({
            "id": Uuid::new_v4(),
            "owner_id": "local",
            "agent_type": "gemini",        // open string — must survive
            "repo_path": "/tmp/repo",
            "status": "active",
            "title": null,
            "created_at": Utc::now().to_rfc3339(),
            "last_seq": 0_u64,
        });
        let session: Session = serde_json::from_value(raw).unwrap();
        assert_eq!(session.agent_type, "gemini");
    }

    // ── Schema ────────────────────────────────────────────────────────────────

    #[test]
    fn schema_generation_returns_non_null() {
        let schema = protocol_schema();
        assert!(!schema.is_null());
        assert!(schema.get("client_command").is_some());
        assert!(schema.get("server_event").is_some());
    }

    #[test]
    fn schema_has_defs_section() {
        let schema = protocol_schema();
        assert!(schema.get("$defs").is_some());
        let defs = &schema["$defs"];
        assert!(defs.get("Session").is_some());
        assert!(defs.get("Event").is_some());
        assert!(defs.get("Approval").is_some());
        assert!(defs.get("SessionStatus").is_some());
    }
}
