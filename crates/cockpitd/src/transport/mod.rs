// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Transport layer: WebSocket server + auth + `Transport` trait.
//!
//! The `Transport` trait is the multi-user seam: `DirectWs` implements it for
//! the v1 loopback WebSocket. A future `ZeroKnowledgeRelay` (Tailnet / mTLS)
//! would implement the same trait without touching session/adapter logic.
//!
//! Chunk C1 wires:
//!   - `DirectWs`: axum + tokio-tungstenite on `127.0.0.1:<port>`
//!   - Auth: `hello {token,protocol:1}` → `hello_ok` or `error` + close
//!   - Every command in the wire contract (COCKPIT-WIRE.md)
//!   - Fan-out: events from running sessions broadcast to all subscribed clients
//!   - Seq-numbered replay: reconnect with `from_seq=N` → events with seq>N
//!
//! H1a — unified approval store:
//!   `AppState` no longer carries a separate approval box. All approval
//!   operations (`insert_approval`, `get_approval`, `resolve_approval`) are
//!   routed through the supervisor's store, which already owns the session
//!   rows — satisfying the FK constraint on `approvals.session_id → sessions.id`
//!   and eliminating the silent `let _ =` that masked FK errors in G1.

pub mod auth;
pub mod relay;
pub mod seams;
pub mod sweep;
pub mod ws;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use anyhow::Result;
use tokio::sync::{Notify, broadcast};
use uuid::Uuid;

use crate::{audit::AuditLog, clock::Clock, model::Event, supervisor::Supervisor};

/// The serving-surface abstraction.
///
/// v1 implements `DirectWs` (loopback WebSocket).
/// A future `ZeroKnowledgeRelay` slots in here — the multi-user seam.
pub trait Transport: Send + 'static {
    /// Start serving, blocking until the server shuts down.
    fn serve(self) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Shared runtime state passed from `DirectWs` to each WebSocket handler.
///
/// H1a: `approval` has been removed. All approval operations go through
/// `supervisor` (single store, FK always satisfied).
pub struct AppState {
    /// In-memory event fan-out: events appended to sessions are broadcast here.
    pub event_tx: broadcast::Sender<Event>,
    /// Shared supervisor behind an Arc<Mutex<...>>.
    /// Store reads *and* approval operations are routed through the supervisor's
    /// internal store — the single source of truth for sessions + events +
    /// approvals.
    pub supervisor: std::sync::Arc<tokio::sync::Mutex<SupervisorBox>>,
    /// Append-only audit log (intentionally isolated in its own store).
    pub audit: std::sync::Arc<tokio::sync::Mutex<AuditBox>>,
    /// Per-session approval gate: a pump waiting for approval resolution
    /// stores a `Notify` here; the `Approve` WS command signals it.
    /// Keyed by approval_id (not session_id) so concurrent approvals per
    /// session are individually gated.
    pub approval_gates: Arc<std::sync::Mutex<HashMap<Uuid, Arc<Notify>>>>,
}

/// Type-erased AuditLog (erases the Clock type parameter).
pub struct AuditBox(pub Box<dyn ErasedAudit + Send>);

/// Type-erased Supervisor (erases the Clock and Adapter type parameters).
pub struct SupervisorBox(pub Box<dyn ErasedSupervisor + Send>);

// ── Erased supervisor interface ───────────────────────────────────────────────

pub trait ErasedSupervisor {
    /// Launch a session via the async path: stores pending pump in supervisor,
    /// caller must call `take_pending_pump` to get the rx and spawn the pump.
    fn launch_session(
        &mut self,
        agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        owner_id: &str,
        event_tx: broadcast::Sender<Event>,
    ) -> Result<uuid::Uuid>;

    /// Pop the pending event receiver for a just-launched session.
    fn take_pending_pump(
        &mut self,
        id: uuid::Uuid,
    ) -> Option<tokio::sync::mpsc::Receiver<crate::supervisor::AdapterEvent>>;

    fn kill_session(&mut self, session_id: uuid::Uuid) -> Result<()>;
    fn send_prompt(&mut self, session_id: uuid::Uuid, text: &str) -> Result<()>;
    fn status(&self, session_id: uuid::Uuid) -> Option<crate::model::SessionStatus>;

    /// Update live status for a session (called by async pump).
    fn set_status(&mut self, session_id: uuid::Uuid, status: crate::model::SessionStatus);
    /// Remove session from live map (called by async pump on terminal event).
    fn remove_live(&mut self, session_id: uuid::Uuid);

    // ── Store passthrough (avoids separate store in AppState) ─────────────────
    fn list_sessions(&self) -> anyhow::Result<Vec<crate::model::Session>>;
    fn get_session(&self, id: uuid::Uuid) -> anyhow::Result<Option<crate::model::Session>>;
    fn replay_from(
        &self,
        session_id: uuid::Uuid,
        from_seq: u64,
    ) -> anyhow::Result<Vec<crate::model::Event>>;
    fn update_session_status(
        &mut self,
        id: uuid::Uuid,
        status: &crate::model::SessionStatus,
    ) -> anyhow::Result<()>;
    fn append_event(&mut self, e: &crate::model::Event) -> anyhow::Result<u64>;

    // ── Approval passthrough via supervisor's store ───────────────────────────
    /// Insert a pending approval row into the supervisor's store (which already
    /// holds the session row, satisfying the FK constraint).
    fn insert_approval(&mut self, a: &crate::model::Approval) -> anyhow::Result<()>;
    /// Look up an approval by ID in the supervisor's store.
    fn get_approval(&self, id: uuid::Uuid) -> anyhow::Result<Option<crate::model::Approval>>;
    /// Resolve an approval in the supervisor's store.
    fn resolve_approval(&mut self, id: uuid::Uuid, decision: &str) -> anyhow::Result<()>;
    /// List all pending (unresolved) approvals from the supervisor's store.
    fn list_pending_approvals(&self) -> anyhow::Result<Vec<crate::model::Approval>>;
}

// ── Erased audit interface ────────────────────────────────────────────────────

pub trait ErasedAudit {
    fn append(
        &mut self,
        actor: &str,
        action: &str,
        session_id: Option<uuid::Uuid>,
        detail: serde_json::Value,
    ) -> Result<uuid::Uuid>;

    fn list(
        &self,
        session_id: Option<uuid::Uuid>,
        limit: Option<u64>,
    ) -> Result<Vec<crate::audit::AuditEntry>>;
}

// ── Concrete erased wrappers ──────────────────────────────────────────────────

/// Wraps a concrete `Supervisor<C>` so it can be stored in `AppState`.
pub struct ConcreteSupervisor<C: Clock>(pub Supervisor<C>);

impl<C: Clock> ErasedSupervisor for ConcreteSupervisor<C> {
    fn launch_session(
        &mut self,
        agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        owner_id: &str,
        event_tx: broadcast::Sender<Event>,
    ) -> Result<uuid::Uuid> {
        self.0
            .launch_session_async(agent_type, repo_path, prompt, owner_id, event_tx)
    }

    fn take_pending_pump(
        &mut self,
        id: uuid::Uuid,
    ) -> Option<tokio::sync::mpsc::Receiver<crate::supervisor::AdapterEvent>> {
        self.0.take_pending_pump(id)
    }

    fn kill_session(&mut self, session_id: uuid::Uuid) -> Result<()> {
        self.0.kill_session(session_id)
    }

    fn send_prompt(&mut self, session_id: uuid::Uuid, text: &str) -> Result<()> {
        self.0.adapter_send(session_id, text)
    }

    fn status(&self, session_id: uuid::Uuid) -> Option<crate::model::SessionStatus> {
        self.0.status(session_id)
    }

    fn set_status(&mut self, session_id: uuid::Uuid, status: crate::model::SessionStatus) {
        self.0.set_status(session_id, status);
    }

    fn remove_live(&mut self, session_id: uuid::Uuid) {
        self.0.remove_live(session_id);
    }

    fn list_sessions(&self) -> anyhow::Result<Vec<crate::model::Session>> {
        self.0.store.list_sessions()
    }

    fn get_session(&self, id: uuid::Uuid) -> anyhow::Result<Option<crate::model::Session>> {
        self.0.store.get_session(id)
    }

    fn replay_from(
        &self,
        session_id: uuid::Uuid,
        from_seq: u64,
    ) -> anyhow::Result<Vec<crate::model::Event>> {
        self.0.store.replay_from(session_id, from_seq)
    }

    fn update_session_status(
        &mut self,
        id: uuid::Uuid,
        status: &crate::model::SessionStatus,
    ) -> anyhow::Result<()> {
        self.0.store.update_session_status(id, status)
    }

    fn append_event(&mut self, e: &crate::model::Event) -> anyhow::Result<u64> {
        self.0.store.append_event(e)
    }

    fn insert_approval(&mut self, a: &crate::model::Approval) -> anyhow::Result<()> {
        self.0.store.insert_approval(a)
    }

    fn get_approval(&self, id: uuid::Uuid) -> anyhow::Result<Option<crate::model::Approval>> {
        self.0.store.get_approval(id)
    }

    fn resolve_approval(&mut self, id: uuid::Uuid, decision: &str) -> anyhow::Result<()> {
        self.0.store.resolve_approval(id, decision)
    }

    fn list_pending_approvals(&self) -> anyhow::Result<Vec<crate::model::Approval>> {
        use crate::approval::StorePendingExt as _;
        self.0.store.list_pending_approvals()
    }
}

/// Wraps a concrete `AuditLog<C>`.
pub struct ConcreteAudit<C: Clock>(pub AuditLog<C>);

impl<C: Clock> ErasedAudit for ConcreteAudit<C> {
    fn append(
        &mut self,
        actor: &str,
        action: &str,
        session_id: Option<uuid::Uuid>,
        detail: serde_json::Value,
    ) -> Result<uuid::Uuid> {
        self.0.append(actor, action, session_id, detail)
    }

    fn list(
        &self,
        session_id: Option<uuid::Uuid>,
        limit: Option<u64>,
    ) -> Result<Vec<crate::audit::AuditEntry>> {
        self.0.list(session_id, limit)
    }
}

/// Build an `AppState` from a concrete supervisor and audit log.
///
/// H1a: the `ApprovalManager` parameter has been removed. All approval
/// operations are routed through the supervisor's internal store — a single
/// SQLite connection that owns sessions + events + approvals. The FK constraint
/// on `approvals.session_id → sessions.id` is always satisfied because the
/// session row is created before any approval row, and there is no longer a
/// `let _ =` masking FK violations.
///
/// The audit log retains its own separate store (intentional isolation for the
/// append-only immutable record).
pub fn build_state<C: Clock>(supervisor: Supervisor<C>, audit: AuditLog<C>) -> AppState {
    let (event_tx, _) = broadcast::channel(512);
    AppState {
        event_tx,
        supervisor: std::sync::Arc::new(tokio::sync::Mutex::new(SupervisorBox(Box::new(
            ConcreteSupervisor(supervisor),
        )))),
        audit: std::sync::Arc::new(tokio::sync::Mutex::new(AuditBox(Box::new(ConcreteAudit(
            audit,
        ))))),
        approval_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
    }
}

/// The `DirectWs` transport: axum WebSocket server on `127.0.0.1:<port>`.
pub struct DirectWs {
    pub addr: SocketAddr,
    pub state: AppState,
}

impl DirectWs {
    pub fn new(addr: SocketAddr, state: AppState) -> Self {
        Self { addr, state }
    }
}

impl Transport for DirectWs {
    async fn serve(self) -> Result<()> {
        ws::serve(self.addr, self.state).await
    }
}
