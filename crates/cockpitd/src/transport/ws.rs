// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! axum WebSocket server implementation of the cockpit wire protocol.
//!
//! Features:
//! - Binds to `127.0.0.1:<port>` (default 8787, override via `COCKPIT_ADDR`).
//! - Auth: first client frame must be `hello {token, protocol:1}`.
//! - Handles all commands from COCKPIT-WIRE.md.
//! - Fan-out: events from running sessions are broadcast to all subscribed clients.
//! - Seq-numbered replay: `open_session {from_seq:N}` → events with seq>N, no gaps/dupes.
//! - ~50ms output coalescing window.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    model::{Event, SessionStatus},
    protocol::{ApproveDecision, ClientCommand, ServerEvent},
    supervisor::AdapterEvent,
    transport::AppState,
    VERSION,
};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the axum WebSocket server. Blocks until the server shuts down.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let state = Arc::new(state);

    let app = Router::new()
        .route("/", get(ws_handler))
        .with_state(state);

    info!("cockpitd {} serving on ws://{}", VERSION, addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── WebSocket upgrade handler ─────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

// ── Per-connection handler ────────────────────────────────────────────────────

/// Client context for a single WebSocket connection.
struct ClientConn {
    state: Arc<AppState>,
    /// Sessions this client is subscribed to (for fan-out).
    subscribed: Vec<Uuid>,
    /// Global broadcast receiver (receives all session events).
    event_rx: broadcast::Receiver<Event>,
}

impl ClientConn {
    fn new(state: Arc<AppState>) -> Self {
        let event_rx = state.event_tx.subscribe();
        Self {
            state,
            subscribed: Vec::new(),
            event_rx,
        }
    }
}

/// Handle a single WebSocket connection.
async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    // ── Auth: wait for hello ──────────────────────────────────────────────────
    let authed = match stream.next().await {
        Some(Ok(Message::Text(text))) => {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.get("t").and_then(|t| t.as_str()) == Some("hello") => {
                    let token = v
                        .get("token")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    match super::auth::validate_token(token) {
                        Ok(()) => {
                            let ok = ServerEvent::HelloOk {
                                server_version: VERSION.to_string(),
                                protocol: 1,
                            };
                            let _ = sink
                                .send(Message::Text(serde_json::to_string(&ok).unwrap().into()))
                                .await;
                            true
                        }
                        Err(reason) => {
                            let err = ServerEvent::Error {
                                code: "auth_failed".into(),
                                message: reason.to_string(),
                            };
                            let _ = sink
                                .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                                .await;
                            false
                        }
                    }
                }
                _ => {
                    let err = ServerEvent::Error {
                        code: "bad_handshake".into(),
                        message: "first frame must be hello".into(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                    false
                }
            }
        }
        _ => false,
    };

    if !authed {
        debug!("client rejected (bad auth)");
        return;
    }

    info!("client authenticated");

    let mut ctx = ClientConn::new(state);
    let coalesce_window = Duration::from_millis(50);
    let mut pending_events: Vec<(Uuid, Event)> = Vec::new();
    let mut flush_deadline: Option<tokio::time::Instant> = None;

    // ── Main event loop ───────────────────────────────────────────────────────
    loop {
        // Determine how long until flush.
        let until_flush = flush_deadline.map(|d| {
            let now = tokio::time::Instant::now();
            if d > now { d - now } else { Duration::ZERO }
        });

        tokio::select! {
            // Incoming client command.
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_command(&text, &mut ctx, &mut sink, &mut pending_events, &mut flush_deadline, coalesce_window).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("client disconnected");
                        break;
                    }
                    Some(Ok(_)) => {} // binary/ping frames ignored
                    Some(Err(e)) => {
                        warn!("ws recv error: {e}");
                        break;
                    }
                }
            }

            // Incoming broadcast event (fan-out from running sessions).
            evt = ctx.event_rx.recv() => {
                match evt {
                    Ok(event) => {
                        if ctx.subscribed.contains(&event.session_id) {
                            pending_events.push((event.session_id, event));
                            if flush_deadline.is_none() {
                                flush_deadline = Some(tokio::time::Instant::now() + coalesce_window);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("broadcast lagged {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }

            // Coalesce window expired — flush pending events.
            _ = async {
                match until_flush {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            } => {
                flush_pending_events(&mut pending_events, &mut flush_deadline, &mut sink).await;
            }
        }
    }

    // Flush any remaining buffered events on disconnect.
    flush_pending_events(&mut pending_events, &mut flush_deadline, &mut sink).await;
}

/// Flush all pending coalesced events to the client.
async fn flush_pending_events(
    pending: &mut Vec<(Uuid, Event)>,
    flush_deadline: &mut Option<tokio::time::Instant>,
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
) {
    for (session_id, event) in pending.drain(..) {
        let frame = ServerEvent::Event {
            session_id,
            event,
        };
        let _ = sink
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await;
    }
    *flush_deadline = None;
}

// ── Command dispatch ──────────────────────────────────────────────────────────

async fn handle_command(
    text: &str,
    ctx: &mut ClientConn,
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    pending_events: &mut Vec<(Uuid, Event)>,
    flush_deadline: &mut Option<tokio::time::Instant>,
    _coalesce_window: Duration,
) {
    let cmd = match serde_json::from_str::<ClientCommand>(text) {
        Ok(c) => c,
        Err(e) => {
            let err = ServerEvent::Error {
                code: "parse_error".into(),
                message: e.to_string(),
            };
            let _ = sink
                .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                .await;
            return;
        }
    };

    match cmd {
        ClientCommand::Hello { .. } => {
            // Re-hello after auth is a no-op (idempotent).
        }

        ClientCommand::Ping => {
            let _ = sink
                .send(Message::Text(
                    serde_json::to_string(&ServerEvent::Pong).unwrap().into(),
                ))
                .await;
        }

        ClientCommand::ListSessions => {
            let sessions = {
                let sup = ctx.state.supervisor.lock().await;
                sup.0.list_sessions().unwrap_or_default()
            };
            let frame = ServerEvent::SessionList { sessions };
            let _ = sink
                .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                .await;
        }

        ClientCommand::OpenSession {
            session_id,
            from_seq,
        } => {
            // Flush any pending events first (ordering: replay comes first).
            flush_pending_events(pending_events, flush_deadline, sink).await;

            let (session, events) = {
                let sup = ctx.state.supervisor.lock().await;
                let session = sup.0.get_session(session_id).ok().flatten();
                let events = sup.0.replay_from(session_id, from_seq).unwrap_or_default();
                (session, events)
            };

            match session {
                None => {
                    let err = ServerEvent::Error {
                        code: "not_found".into(),
                        message: format!("session {session_id} not found"),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                    return;
                }
                Some(session) => {
                    let cursor_seq = events.last().map(|e| e.seq).unwrap_or(from_seq);
                    let frame = ServerEvent::Snapshot {
                        session_id,
                        session,
                        events,
                        cursor_seq,
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                        .await;

                    // Subscribe to live deltas.
                    if !ctx.subscribed.contains(&session_id) {
                        ctx.subscribed.push(session_id);
                    }
                }
            }
        }

        ClientCommand::SendPrompt { session_id, text } => {
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.send_prompt(session_id, &text) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "send_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                }
            }
        }

        ClientCommand::Steer { session_id, text } => {
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.send_prompt(session_id, &text) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "steer_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                }
            }
        }

        ClientCommand::Approve {
            approval_id,
            decision,
            reason: _,
        } => {
            let decision_str = match decision {
                ApproveDecision::Allow => "allow",
                ApproveDecision::Deny => "deny",
            };
            let mut approval = ctx.state.approval.lock().await;
            match approval.0.resolve(approval_id, decision_str) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "approve_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                }
            }
        }

        ClientCommand::LaunchSession {
            agent_type,
            repo_path,
            prompt,
        } => {
            let session_id = {
                let mut sup = ctx.state.supervisor.lock().await;
                let event_tx = ctx.state.event_tx.clone();
                sup.0.launch_session(&agent_type, &repo_path, prompt.as_deref(), "local", event_tx)
            };

            match session_id {
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "launch_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                    return;
                }
                Ok(sid) => {
                    // Spawn the async pump for this session.
                    let rx = {
                        let mut sup = ctx.state.supervisor.lock().await;
                        sup.0.take_pending_pump(sid)
                    };
                    if let Some(rx) = rx {
                        let event_tx = ctx.state.event_tx.clone();
                        let sup_arc = ctx.state.supervisor.clone();
                        tokio::spawn(async move {
                            run_pump(sid, rx, event_tx, sup_arc).await;
                        });
                    }

                    // Send session_list so client sees the new session.
                    let sessions = {
                        let sup = ctx.state.supervisor.lock().await;
                        sup.0.list_sessions().unwrap_or_default()
                    };
                    let frame = ServerEvent::SessionList { sessions };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                        .await;
                }
            }
        }

        ClientCommand::CloseSession { session_id } => {
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.kill_session(session_id) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "close_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                        .await;
                }
            }
        }

        ClientCommand::Unknown => {
            debug!("unknown command frame received — ignored (forward-compat)");
        }
    }
}

// ── Async event pump ──────────────────────────────────────────────────────────

/// Drains AdapterEvents from `rx`, persists them to the supervisor's store, and
/// broadcasts them as cockpit Events to all subscribed clients.
///
/// Runs as a tokio task per session. Terminates when the adapter closes the channel.
async fn run_pump(
    session_id: Uuid,
    mut rx: tokio::sync::mpsc::Receiver<AdapterEvent>,
    event_tx: broadcast::Sender<Event>,
    supervisor: Arc<Mutex<crate::transport::SupervisorBox>>,
) {
    while let Some(adapter_evt) = rx.recv().await {
        match adapter_evt {
            AdapterEvent::Event(mut evt) => {
                evt.session_id = session_id;

                // Persist and assign seq via supervisor's store.
                let seq = {
                    let mut sup = supervisor.lock().await;
                    match sup.0.append_event(&evt) {
                        Ok(seq) => seq,
                        Err(e) => {
                            warn!("pump: failed to persist event for {session_id}: {e}");
                            continue;
                        }
                    }
                };
                evt.seq = seq;

                // Update live status if awaiting input.
                if evt.requires_user_input {
                    let mut sup = supervisor.lock().await;
                    sup.0.set_status(session_id, SessionStatus::AwaitingInput);
                }

                // Broadcast to all subscribed clients.
                let _ = event_tx.send(evt);
            }

            AdapterEvent::Completed => {
                let mut sup = supervisor.lock().await;
                let _ = sup.0.update_session_status(session_id, &SessionStatus::Completed);
                sup.0.set_status(session_id, SessionStatus::Completed);
                sup.0.remove_live(session_id);
                break;
            }

            AdapterEvent::Failed(msg) => {
                warn!("session {session_id} adapter failed: {msg}");
                let mut sup = supervisor.lock().await;
                let _ = sup.0.update_session_status(session_id, &SessionStatus::Failed);
                sup.0.set_status(session_id, SessionStatus::Failed);
                sup.0.remove_live(session_id);
                break;
            }
        }
    }

    debug!("pump for session {session_id} exited");
}
