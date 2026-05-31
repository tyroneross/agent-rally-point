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

pub mod auth;
pub mod ws;

use std::net::SocketAddr;

use anyhow::Result;
use tokio::sync::broadcast;

use crate::{
    approval::ApprovalManager,
    clock::Clock,
    model::Event,
    supervisor::Supervisor,
};

/// The serving-surface abstraction.
///
/// v1 implements `DirectWs` (loopback WebSocket).
/// A future `ZeroKnowledgeRelay` slots in here — the multi-user seam.
pub trait Transport: Send + 'static {
    /// Start serving, blocking until the server shuts down.
    fn serve(self) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Shared runtime state passed from `DirectWs` to each WebSocket handler.
pub struct AppState {
    /// In-memory event fan-out: events appended to sessions are broadcast here.
    pub event_tx: broadcast::Sender<Event>,
    /// Shared supervisor behind an Arc<Mutex<...>>.
    /// Store reads are routed through the supervisor's internal store.
    pub supervisor: std::sync::Arc<tokio::sync::Mutex<SupervisorBox>>,
    /// Shared approval manager.
    pub approval: std::sync::Arc<tokio::sync::Mutex<ApprovalBox>>,
}

/// Type-erased Supervisor (erases the Clock and Adapter type parameters).
pub struct SupervisorBox(pub Box<dyn ErasedSupervisor + Send>);

/// Type-erased ApprovalManager.
pub struct ApprovalBox(pub Box<dyn ErasedApproval + Send>);

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
    fn replay_from(&self, session_id: uuid::Uuid, from_seq: u64) -> anyhow::Result<Vec<crate::model::Event>>;
    fn update_session_status(&mut self, id: uuid::Uuid, status: &crate::model::SessionStatus) -> anyhow::Result<()>;
    fn append_event(&mut self, e: &crate::model::Event) -> anyhow::Result<u64>;
}

// ── Erased approval interface ─────────────────────────────────────────────────

pub trait ErasedApproval {
    fn resolve(&mut self, id: uuid::Uuid, decision: &str) -> Result<()>;
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

    fn replay_from(&self, session_id: uuid::Uuid, from_seq: u64) -> anyhow::Result<Vec<crate::model::Event>> {
        self.0.store.replay_from(session_id, from_seq)
    }

    fn update_session_status(&mut self, id: uuid::Uuid, status: &crate::model::SessionStatus) -> anyhow::Result<()> {
        self.0.store.update_session_status(id, status)
    }

    fn append_event(&mut self, e: &crate::model::Event) -> anyhow::Result<u64> {
        self.0.store.append_event(e)
    }
}

/// Wraps a concrete `ApprovalManager<C>`.
pub struct ConcreteApproval<C: Clock>(pub ApprovalManager<C>);

impl<C: Clock> ErasedApproval for ConcreteApproval<C> {
    fn resolve(&mut self, id: uuid::Uuid, decision: &str) -> Result<()> {
        self.0.resolve(id, decision)
    }
}

/// Build an `AppState` from concrete supervisor and approval manager.
///
/// Store reads (list_sessions, replay_from, etc.) are routed through the
/// supervisor's internal store via `ErasedSupervisor`, so there is no
/// separate store instance in AppState. This ensures in-memory tests use
/// the same SQLite connection that the supervisor writes to.
pub fn build_state<C: Clock>(
    supervisor: Supervisor<C>,
    approval: ApprovalManager<C>,
) -> AppState {
    let (event_tx, _) = broadcast::channel(512);
    AppState {
        event_tx,
        supervisor: std::sync::Arc::new(tokio::sync::Mutex::new(SupervisorBox(Box::new(
            ConcreteSupervisor(supervisor),
        )))),
        approval: std::sync::Arc::new(tokio::sync::Mutex::new(ApprovalBox(Box::new(
            ConcreteApproval(approval),
        )))),
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
