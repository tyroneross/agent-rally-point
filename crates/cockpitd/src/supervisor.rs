// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Session supervisor: spawn/track/kill sessions and persist their events.
//!
//! The real Claude and Codex adapters land in chunk B. This module defines the
//! `Adapter` trait surface they will implement, and provides a `FakeAdapter`
//! used in chunk-A tests.
//!
//! State machine: Active → AwaitingInput ↔ Active
//!                       → Paused → Active
//!                       → Stale
//!                       → Completed (terminal)
//!                       → Failed    (terminal)
//!                       → Killed    (terminal)
//!                       → Disconnected (terminal — reconnect creates new session)

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{Result, bail};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::{
    clock::Clock,
    model::{Event, Session, SessionStatus},
    store::Store,
};

// ── Adapter trait ─────────────────────────────────────────────────────────────

/// The interface a real agent adapter (Claude, Codex, …) must implement.
///
/// Chunk B fills in the real implementations; this surface must stay minimal so
/// the implementations are unconstrained on scheduling strategy.
pub trait Adapter: Send + 'static {
    /// Start the agent process for `session_id` with the given `prompt`.
    /// Implementations push `AdapterEvent`s to `tx` as the session runs.
    fn start(
        &mut self,
        session_id: Uuid,
        agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        tx: mpsc::Sender<AdapterEvent>,
    ) -> Result<()>;

    /// Send a new prompt turn (user message) to the running session.
    fn send(&mut self, session_id: Uuid, text: &str) -> Result<()>;

    /// Terminate the session immediately.
    fn kill(&mut self, session_id: Uuid) -> Result<()>;
}

/// Events pushed from an adapter back to the supervisor.
#[derive(Debug, Clone)]
pub enum AdapterEvent {
    /// A new event from the agent (to be persisted and broadcast).
    Event(Event),
    /// The agent subprocess ended normally.
    Completed,
    /// The agent subprocess ended with an error.
    Failed(String),
}

// ── Supervisor ────────────────────────────────────────────────────────────────

/// Live session state tracked in-memory.
pub(crate) struct LiveSession {
    #[allow(dead_code)] // used by chunk B for session lookup
    session_id: Uuid,
    status: SessionStatus,
    // tx for sending commands TO the adapter (e.g. steer, kill) lives here
    // in chunk B once the real adapters land. For now, unused — suppressed
    // by the #[allow(dead_code)] below.
    #[allow(dead_code)]
    _cmd_tx: Option<mpsc::Sender<()>>, // placeholder; real type defined in chunk B
    /// Abort handle for the async event-pump task (C1).
    _pump_abort: Option<tokio::task::AbortHandle>,
}

/// Supervises all active sessions: owns the Store and drives state transitions.
pub struct Supervisor<C: Clock> {
    pub(crate) store: Store,
    clock: C,
    pub(crate) sessions: HashMap<Uuid, LiveSession>,
    /// Adapter factory: maps agent_type → boxed adapter.
    /// In tests this is populated with a FakeAdapter.
    pub(crate) adapter: Box<dyn Adapter>,
    /// Pending event receivers for async pump hand-off (C1 transport).
    pub(crate) pending_pumps: HashMap<Uuid, mpsc::Receiver<AdapterEvent>>,
}

impl<C: Clock> Supervisor<C> {
    pub fn new(store: Store, clock: C, adapter: impl Adapter) -> Self {
        Self {
            store,
            clock,
            sessions: HashMap::new(),
            adapter: Box::new(adapter),
            pending_pumps: HashMap::new(),
        }
    }

    /// Launch a new session.
    ///
    /// Creates the session row, starts the adapter, and wires the event loop
    /// that persists events and drives status transitions.
    pub fn launch_session(
        &mut self,
        agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        owner_id: &str,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = self.clock.now();

        let session = Session {
            id,
            owner_id: owner_id.to_string(),
            agent_type: agent_type.to_string(),
            repo_path: repo_path.to_string(),
            status: SessionStatus::Active,
            title: None,
            created_at: now,
            last_seq: 0,
        };
        self.store.create_session(&session)?;

        let (tx, rx) = mpsc::channel::<AdapterEvent>(64);
        // Pass ownership of tx to the adapter. The adapter drops it when done,
        // which closes the channel and signals the drain loop (Disconnected).
        self.adapter.start(id, agent_type, repo_path, prompt, tx)?;

        self.sessions.insert(
            id,
            LiveSession {
                session_id: id,
                status: SessionStatus::Active,
                _cmd_tx: None, // TODO(chunk-B): wire real command channel
                _pump_abort: None,
            },
        );

        // Drain the event channel synchronously for the FakeAdapter path.
        // Real adapters will have an async pump in chunk C; here we use a
        // blocking helper so tests don't need an async runtime.
        self.drain_events(id, rx)?;

        Ok(id)
    }

    /// Launch a session for the async transport path (C1).
    ///
    /// Creates the session row, starts the adapter, and stores the event receiver
    /// in `pending_pumps` so the transport can pick it up via `take_pending_pump`
    /// and spawn an async pump task with access to `Arc<Mutex<Store>>`.
    ///
    /// Does NOT drain synchronously — the pump is spawned by the caller (ws.rs).
    pub fn launch_session_async(
        &mut self,
        agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        owner_id: &str,
        event_tx: broadcast::Sender<Event>,
    ) -> Result<Uuid> {
        let _ = event_tx; // stored in AppState; passed to the pump by ws.rs
        let id = Uuid::new_v4();
        let now = self.clock.now();

        let session = Session {
            id,
            owner_id: owner_id.to_string(),
            agent_type: agent_type.to_string(),
            repo_path: repo_path.to_string(),
            status: SessionStatus::Active,
            title: None,
            created_at: now,
            last_seq: 0,
        };
        self.store.create_session(&session)?;

        let (tx, rx) = mpsc::channel::<AdapterEvent>(128);
        self.adapter.start(id, agent_type, repo_path, prompt, tx)?;

        self.sessions.insert(
            id,
            LiveSession {
                session_id: id,
                status: SessionStatus::Active,
                _cmd_tx: None,
                _pump_abort: None,
            },
        );

        // Store rx for the transport to pick up and pump asynchronously.
        self.pending_pumps.insert(id, rx);

        Ok(id)
    }

    /// Pop the pending event receiver for a just-launched session (for the async pump).
    pub fn take_pending_pump(&mut self, id: Uuid) -> Option<mpsc::Receiver<AdapterEvent>> {
        self.pending_pumps.remove(&id)
    }

    /// Update the live status for a session (called by the async pump).
    pub fn set_status(&mut self, session_id: Uuid, status: SessionStatus) {
        if let Some(live) = self.sessions.get_mut(&session_id) {
            live.status = status;
        }
    }

    /// Remove a session from the live map (called when the pump sees a terminal event).
    pub fn remove_live(&mut self, session_id: Uuid) {
        self.sessions.remove(&session_id);
    }

    /// Send a prompt to a running session via the adapter.
    pub fn adapter_send(&mut self, session_id: Uuid, text: &str) -> Result<()> {
        self.adapter.send(session_id, text)
    }

    /// Kill a running session.
    pub fn kill_session(&mut self, session_id: Uuid) -> Result<()> {
        if !self.sessions.contains_key(&session_id) {
            bail!("session {session_id} not found");
        }
        self.adapter.kill(session_id)?;
        self.transition(session_id, SessionStatus::Killed)?;
        self.sessions.remove(&session_id);
        Ok(())
    }

    /// Query current status from live map (falls back to store).
    pub fn status(&self, session_id: Uuid) -> Option<SessionStatus> {
        self.sessions
            .get(&session_id)
            .map(|s| s.status.clone())
            .or_else(|| {
                self.store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .map(|s| s.status)
            })
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Drain all events from `rx` until the channel closes.
    ///
    /// For FakeAdapter, the channel is pre-loaded and closed immediately.
    /// Real adapters (chunk B) will use an async pump instead.
    fn drain_events(
        &mut self,
        session_id: Uuid,
        mut rx: mpsc::Receiver<AdapterEvent>,
    ) -> Result<()> {
        // Use try_recv loop: FakeAdapter sends all events then drops sender.
        loop {
            match rx.try_recv() {
                Ok(AdapterEvent::Event(mut evt)) => {
                    evt.session_id = session_id;
                    let seq = self.store.append_event(&evt)?;
                    // Reflect requires_user_input → AwaitingInput
                    if evt.requires_user_input {
                        self.transition(session_id, SessionStatus::AwaitingInput)?;
                    }
                    let _ = seq; // seq is tracked in store
                }
                Ok(AdapterEvent::Completed) => {
                    self.transition(session_id, SessionStatus::Completed)?;
                    self.sessions.remove(&session_id);
                    break;
                }
                Ok(AdapterEvent::Failed(msg)) => {
                    tracing::warn!("session {session_id} failed: {msg}");
                    self.transition(session_id, SessionStatus::Failed)?;
                    self.sessions.remove(&session_id);
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // Channel empty but not closed — adapter still running.
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Sender dropped without sending Completed/Failed.
                    // Treat as disconnected (caller can retry).
                    self.transition(session_id, SessionStatus::Disconnected)?;
                    self.sessions.remove(&session_id);
                    break;
                }
            }
        }
        Ok(())
    }

    fn transition(&mut self, session_id: Uuid, new_status: SessionStatus) -> Result<()> {
        self.store.update_session_status(session_id, &new_status)?;
        if let Some(live) = self.sessions.get_mut(&session_id) {
            live.status = new_status;
        }
        Ok(())
    }
}

// ── FakeAdapter ───────────────────────────────────────────────────────────────

/// A scripted adapter for tests. Emits a fixed sequence of events synchronously
/// when `start` is called, then sends a terminal event (Completed or Failed).
pub struct FakeAdapter {
    /// Events to emit, in order. The last entry should be Completed or Failed.
    pub script: Vec<AdapterEvent>,
    /// If true, the start call returns an error (for error-path tests).
    pub fail_start: bool,
    /// Capture kill calls for assertion.
    pub killed: Arc<Mutex<Vec<Uuid>>>,
}

impl FakeAdapter {
    pub fn with_script(events: Vec<AdapterEvent>) -> Self {
        Self {
            script: events,
            fail_start: false,
            killed: Arc::new(Mutex::new(vec![])),
        }
    }

    /// Convenience: 3 message events + Completed.
    pub fn three_messages(session_id: Uuid, now: chrono::DateTime<Utc>) -> Self {
        let make = |kind: &str, content: &str| {
            AdapterEvent::Event(Event {
                session_id, // will be overwritten by supervisor
                seq: 0,
                sender: "agent".into(),
                kind: kind.into(),
                content: content.into(),
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({}),
            })
        };
        Self::with_script(vec![
            make("message", "hello"),
            make("message", "world"),
            make("tool_call", "ls /tmp"),
            AdapterEvent::Completed,
        ])
    }
}

impl Adapter for FakeAdapter {
    fn start(
        &mut self,
        _session_id: Uuid,
        _agent_type: &str,
        _repo_path: &str,
        _prompt: Option<&str>,
        tx: mpsc::Sender<AdapterEvent>,
    ) -> Result<()> {
        if self.fail_start {
            bail!("FakeAdapter: forced start failure");
        }
        // Push all scripted events synchronously.
        for evt in self.script.drain(..) {
            // Use blocking_send would require a runtime; use try_send since
            // channel capacity is 64 and scripts are short.
            tx.try_send(evt).ok();
        }
        // Drop tx so the channel closes (signals Disconnected if no terminal
        // event was in the script).
        drop(tx);
        Ok(())
    }

    fn send(&mut self, _session_id: Uuid, _text: &str) -> Result<()> {
        Ok(()) // no-op in fake
    }

    fn kill(&mut self, session_id: Uuid) -> Result<()> {
        self.killed.lock().unwrap().push(session_id);
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::store::Store;

    fn open_supervisor(script: Vec<AdapterEvent>) -> Supervisor<FakeClock> {
        let store = Store::open_in_memory().unwrap();
        let clock = FakeClock::at_epoch();
        let adapter = FakeAdapter::with_script(script);
        Supervisor::new(store, clock, adapter)
    }

    fn make_event(session_id: Uuid, content: &str) -> AdapterEvent {
        AdapterEvent::Event(Event {
            session_id,
            seq: 0,
            sender: "agent".into(),
            kind: "message".into(),
            content: content.into(),
            requires_user_input: false,
            created_at: Utc::now(),
            metadata: serde_json::json!({}),
        })
    }

    // ── A3-1: FakeAdapter emits events with monotonic seq, terminal Completed ──

    #[test]
    fn launch_records_events_with_monotonic_seq_and_completes() {
        let placeholder = Uuid::nil(); // supervisor assigns the real ID
        let script = vec![
            make_event(placeholder, "line 1"),
            make_event(placeholder, "line 2"),
            make_event(placeholder, "line 3"),
            AdapterEvent::Completed,
        ];
        let mut sup = open_supervisor(script);
        let sid = sup
            .launch_session("claude", "/tmp/repo", Some("do work"), "local")
            .unwrap();

        let events = sup.store.replay_from(sid, 0).unwrap();
        assert_eq!(events.len(), 3, "3 events stored");
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[2].seq, 3);
        assert_eq!(events[0].content, "line 1");

        let _status = sup.status(sid);
        // Session removed from live map after terminal; check store
        let stored_status = sup.store.get_session(sid).unwrap().unwrap().status;
        assert!(
            matches!(stored_status, SessionStatus::Completed),
            "expected Completed, got {stored_status:?}"
        );
    }

    // ── A3-2: last_seq updated in store ───────────────────────────────────────

    #[test]
    fn last_seq_updated_after_events() {
        let placeholder = Uuid::nil();
        let script = vec![
            make_event(placeholder, "a"),
            make_event(placeholder, "b"),
            AdapterEvent::Completed,
        ];
        let mut sup = open_supervisor(script);
        let sid = sup
            .launch_session("codex", "/tmp/r", None, "local")
            .unwrap();

        let session = sup.store.get_session(sid).unwrap().unwrap();
        assert_eq!(session.last_seq, 2);
    }

    // ── A3-3: kill transitions to Killed ──────────────────────────────────────

    #[test]
    fn kill_transitions_to_killed() {
        // Test kill by manually inserting a live session into the map.
        // (FakeAdapter with empty script → Disconnected immediately via drain,
        // so we bypass launch and insert the row + live entry directly.)
        let script = vec![]; // unused — we bypass launch
        let mut sup = open_supervisor(script);

        // We need the session to remain live to test kill. Create a session
        // where the adapter never sends anything (channel stays open in
        // a real async setting). Because FakeAdapter drops tx immediately,
        // drain will see Disconnected. So we test kill by manually inserting
        // the session into the map and calling kill.

        // Create session in store directly.
        let sid = Uuid::new_v4();
        let now = Utc::now();
        let session = Session {
            id: sid,
            owner_id: "local".into(),
            agent_type: "claude".into(),
            repo_path: "/tmp".into(),
            status: SessionStatus::Active,
            title: None,
            created_at: now,
            last_seq: 0,
        };
        sup.store.create_session(&session).unwrap();

        // Insert into live map so kill can find it
        sup.sessions.insert(
            sid,
            LiveSession {
                session_id: sid,
                status: SessionStatus::Active,
                _cmd_tx: None,
                _pump_abort: None,
            },
        );

        sup.kill_session(sid).unwrap();

        let stored = sup.store.get_session(sid).unwrap().unwrap();
        assert!(
            matches!(stored.status, SessionStatus::Killed),
            "expected Killed, got {:?}",
            stored.status
        );
        // Session removed from live map
        assert!(!sup.sessions.contains_key(&sid));
    }

    // ── A3-4: disconnect when adapter channel closes without terminal event ────

    #[test]
    fn disconnected_when_channel_closes_without_terminal() {
        // Empty script → FakeAdapter drops tx immediately.
        let script = vec![];
        let mut sup = open_supervisor(script);
        let sid = sup
            .launch_session("claude", "/tmp/r", None, "local")
            .unwrap();

        let stored = sup.store.get_session(sid).unwrap().unwrap();
        assert!(
            matches!(stored.status, SessionStatus::Disconnected),
            "expected Disconnected, got {:?}",
            stored.status
        );
    }

    // ── A3-5: open string agent_type survives full launch cycle ───────────────

    #[test]
    fn open_agent_type_survives_launch() {
        let _placeholder = Uuid::nil();
        let script = vec![AdapterEvent::Completed];
        let mut sup = open_supervisor(script);
        let sid = sup
            .launch_session("gemini", "/tmp/repo", None, "local")
            .unwrap();

        let stored = sup.store.get_session(sid).unwrap().unwrap();
        assert_eq!(stored.agent_type, "gemini");
    }
}
