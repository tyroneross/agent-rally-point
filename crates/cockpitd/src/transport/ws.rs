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

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

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
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    authz::{self, AuthzPolicy},
    model::{Approval, Event, SessionStatus},
    protocol::{ApproveDecision, ClientCommand, ServerEvent},
    supervisor::AdapterEvent,
    transport::AppState,
    VERSION,
};

// H1a: `approval` has been removed from AppState. All approval operations
// (insert, get, resolve) go through `state.supervisor` which owns the single
// authoritative store for sessions + events + approvals.

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

            // H1a: all approvals live in the supervisor's store (single store).
            // Look up the session_id for audit, then resolve in one place.
            let (session_id_for_audit, resolve_result) = {
                let mut sup = ctx.state.supervisor.lock().await;
                let session_id = sup.0.get_approval(approval_id)
                    .ok()
                    .flatten()
                    .map(|a| a.session_id);
                let result = sup.0.resolve_approval(approval_id, decision_str);
                (session_id, result)
            };

            if resolve_result.is_ok() {
                // Audit the resolution.
                {
                    let mut audit = ctx.state.audit.lock().await;
                    let _ = audit.0.append(
                        "client",
                        "approval:resolved",
                        session_id_for_audit,
                        serde_json::json!({
                            "approval_id": approval_id.to_string(),
                            "decision": decision_str,
                        }),
                    );
                }
                // Signal the per-approval gate so the waiting pump can
                // read the resolution and continue or block the tool.
                let gate = {
                    let gates = ctx.state.approval_gates.lock().unwrap();
                    gates.get(&approval_id).cloned()
                };
                if let Some(notify) = gate {
                    notify.notify_one();
                }
            } else {
                let err_msg = resolve_result.unwrap_err().to_string();
                let err = ServerEvent::Error {
                    code: "approve_failed".into(),
                    message: err_msg,
                };
                let _ = sink
                    .send(Message::Text(serde_json::to_string(&err).unwrap().into()))
                    .await;
            }
        }

        ClientCommand::GetAudit { session_id, limit } => {
            let audit = ctx.state.audit.lock().await;
            match audit.0.list(session_id, limit) {
                Ok(entries) => {
                    let frame = ServerEvent::AuditList { entries };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
                        .await;
                }
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "audit_failed".into(),
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
                    // Audit: session launched.
                    {
                        let mut audit = ctx.state.audit.lock().await;
                        let _ = audit.0.append(
                            "client",
                            "session:launch",
                            Some(sid),
                            serde_json::json!({
                                "agent_type": agent_type,
                                "repo_path": repo_path,
                            }),
                        );
                    }

                    // Spawn the async pump for this session.
                    let rx = {
                        let mut sup = ctx.state.supervisor.lock().await;
                        sup.0.take_pending_pump(sid)
                    };
                    if let Some(rx) = rx {
                        let event_tx = ctx.state.event_tx.clone();
                        let sup_arc = ctx.state.supervisor.clone();
                        let audit_arc = ctx.state.audit.clone();
                        let gates_arc = ctx.state.approval_gates.clone();
                        tokio::spawn(async move {
                            run_pump(sid, rx, event_tx, sup_arc, audit_arc, gates_arc).await;
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
/// ## Authz enforcement (G1 + H1b)
/// When the adapter emits a `tool_call` Event, `authz::decide` is called with
/// the conservative policy.  Decision outcomes:
/// - `Permit` → broadcast the event as normal.
/// - `RequireApproval` → register a pending Approval, broadcast an
///   `approval_request` event so subscribed clients can respond, then PAUSE
///   this session's pump by awaiting a per-approval `Notify`.  The `Approve`
///   WS command calls `notify.notify_one()` after resolving the approval row.
///   After wakeup the pump reads the resolution:
///   - `allow` → continue (broadcast the tool_call event).
///   - `deny` / `auto_denied` / anything else → emit a `tool_blocked` status
///     event and skip forwarding the tool_call result.
///
/// H1b: native `approval_request` events from the Codex adapter are now gated
/// the same way: the pump parks on a `Notify` until the client resolves the
/// approval via the `Approve` WS command.
///
/// The gate is per-approval (not global), so other sessions' pumps are never
/// blocked.  A tokio `Notify` is used rather than a channel so spurious wakeups
/// are harmless (we re-check the DB after every notify).
///
/// ## Multi-block turns (G2)
/// Each content block in an assistant turn is already mapped to its own
/// AdapterEvent by the adapter layer; the pump handles them as independent
/// events with no special logic here.
///
/// H1a: the separate `approval` Arc<Mutex<ApprovalBox>> parameter has been
/// removed. All approval operations go through `supervisor` (single store).
///
/// Runs as a tokio task per session. Terminates when the adapter closes the channel.
async fn run_pump(
    session_id: Uuid,
    mut rx: tokio::sync::mpsc::Receiver<AdapterEvent>,
    event_tx: broadcast::Sender<Event>,
    supervisor: Arc<Mutex<crate::transport::SupervisorBox>>,
    audit: Arc<Mutex<crate::transport::AuditBox>>,
    gates: Arc<std::sync::Mutex<HashMap<Uuid, Arc<Notify>>>>,
) {
    // Conservative authz policy: read-only ops auto-permitted; everything else
    // (including shell tools and unknown tools) requires approval.
    let policy = AuthzPolicy::conservative();

    while let Some(adapter_evt) = rx.recv().await {
        match adapter_evt {
            AdapterEvent::Event(mut evt) => {
                evt.session_id = session_id;

                // ── G1: authz gate for tool_call events ───────────────────────
                if evt.kind == "tool_call" {
                    let tool_name = evt
                        .metadata
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        // Codex adapter stores tool name under "tool"
                        .or_else(|| evt.metadata.get("tool").and_then(|v| v.as_str()))
                        .unwrap_or(&evt.content)
                        .to_string();
                    let tool_args = evt
                        .metadata
                        .get("input")
                        .or_else(|| evt.metadata.get("args"))
                        .cloned()
                        .unwrap_or(serde_json::json!({}));

                    let decision = authz::decide(&tool_name, &tool_args, &policy);

                    if decision == authz::Decision::RequireApproval {
                        // Persist the tool_call event first (so it gets a seq).
                        let seq = {
                            let mut sup = supervisor.lock().await;
                            match sup.0.append_event(&evt) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!("pump: persist tool_call failed for {session_id}: {e}");
                                    continue;
                                }
                            }
                        };
                        evt.seq = seq;

                        // Mint a fresh approval_id (tool_call events don't carry one).
                        let approval_uuid = Uuid::new_v4();
                        let pending = Approval {
                            id: approval_uuid,
                            session_id,
                            event_seq: seq,
                            tool: tool_name.clone(),
                            args: tool_args.clone(),
                            created_at: evt.created_at,
                            ttl_secs: 300,
                            resolution: None,
                        };

                        // H1a: insert into the single authoritative store (the
                        // supervisor's store). The session row was created before
                        // this point, so the FK constraint is always satisfied.
                        // No `let _ =` — surface the error so FK violations are
                        // visible rather than silently ignored.
                        {
                            let mut sup = supervisor.lock().await;
                            if let Err(e) = sup.0.insert_approval(&pending) {
                                warn!("pump: insert_approval failed for {session_id}: {e}");
                                continue;
                            }
                        }

                        // Create per-approval Notify and register in gates map.
                        let notify = Arc::new(Notify::new());
                        {
                            let mut g = gates.lock().unwrap();
                            g.insert(approval_uuid, notify.clone());
                        }

                        // Persist and broadcast approval_request so it appears in
                        // snapshot replays for late-joining clients.
                        let frame = ServerEvent::ApprovalRequest {
                            approval: pending.clone(),
                        };
                        let mut approval_evt = Event {
                            session_id,
                            seq: 0, // assigned by store below
                            sender: "system".into(),
                            kind: "approval_request".into(),
                            content: format!("approval needed for tool: {tool_name}"),
                            requires_user_input: true,
                            created_at: evt.created_at,
                            metadata: serde_json::json!({
                                "approval_id": approval_uuid.to_string(),
                                "tool": tool_name,
                                "args": tool_args,
                                "ttl_secs": 300,
                                "__approval_frame": serde_json::to_value(&frame).unwrap_or_default(),
                            }),
                        };
                        let approval_seq = {
                            let mut sup = supervisor.lock().await;
                            sup.0.append_event(&approval_evt).unwrap_or(seq + 1)
                        };
                        approval_evt.seq = approval_seq;
                        let _ = event_tx.send(approval_evt);

                        {
                            let mut sup = supervisor.lock().await;
                            sup.0.set_status(session_id, SessionStatus::AwaitingInput);
                        }

                        // PAUSE: await resolution of this approval.
                        notify.notified().await;

                        // Remove gate (cleanup regardless of outcome).
                        {
                            let mut g = gates.lock().unwrap();
                            g.remove(&approval_uuid);
                        }

                        // Check resolution from the supervisor's store (where we
                        // registered the approval in insert_approval above).
                        let resolution = {
                            let sup = supervisor.lock().await;
                            sup.0
                                .get_approval(approval_uuid)
                                .ok()
                                .flatten()
                                .and_then(|a| a.resolution)
                        };

                        if resolution.as_deref() == Some("allow") {
                            // Permitted — broadcast the original tool_call event.
                            {
                                let mut sup = supervisor.lock().await;
                                sup.0.set_status(session_id, SessionStatus::Active);
                            }
                            let _ = event_tx.send(evt);
                        } else {
                            // Denied — emit a tool_blocked status event (do NOT
                            // feed the tool result back to the agent).
                            let denial_reason = resolution.as_deref().unwrap_or("denied");
                            let mut blocked_evt = Event {
                                session_id,
                                seq: 0,
                                sender: "system".into(),
                                kind: "tool_blocked".into(),
                                content: format!("tool '{tool_name}' blocked by authz ({denial_reason})"),
                                requires_user_input: false,
                                created_at: evt.created_at,
                                metadata: serde_json::json!({
                                    "approval_id": approval_uuid.to_string(),
                                    "tool": tool_name,
                                    "reason": denial_reason,
                                }),
                            };
                            let blocked_seq = {
                                let mut sup = supervisor.lock().await;
                                sup.0.append_event(&blocked_evt).unwrap_or(seq + 1)
                            };
                            blocked_evt.seq = blocked_seq;
                            {
                                let mut sup = supervisor.lock().await;
                                sup.0.set_status(session_id, SessionStatus::Active);
                            }
                            let _ = event_tx.send(blocked_evt);
                        }

                        // Audit the gate decision.
                        {
                            let mut aud = audit.lock().await;
                            let _ = aud.0.append(
                                "system",
                                "session:tool_gate",
                                Some(session_id),
                                serde_json::json!({
                                    "approval_id": approval_uuid.to_string(),
                                    "tool": tool_name,
                                    "resolution": resolution,
                                }),
                            );
                        }

                        continue; // already handled
                    }
                    // Decision::Permit falls through to normal broadcast below.
                }

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

                // Audit: session event (lifecycle transitions)
                if matches!(evt.kind.as_str(), "status" | "approval_request") {
                    let mut aud = audit.lock().await;
                    let _ = aud.0.append(
                        "system",
                        &format!("session:{}", evt.kind),
                        Some(session_id),
                        serde_json::json!({ "seq": seq, "content": evt.content }),
                    );
                }

                // H1b: Gate native approval_request events from the Codex
                // adapter (and any future adapter) the same way as tool_call
                // events gated in G1. The pump parks on a per-approval Notify
                // until the client resolves the approval over the wire.
                if evt.kind == "approval_request" {
                    let approval_id_str = evt
                        .metadata
                        .get("approval_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let tool = evt
                        .metadata
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = evt
                        .metadata
                        .get("args")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));

                    // Parse the adapter-supplied ID (must be a valid UUID for
                    // the wire round-trip; mock emits a valid UUID from H1b).
                    let approval_uuid = Uuid::parse_str(approval_id_str)
                        .unwrap_or_else(|_| Uuid::new_v4());

                    let pending = Approval {
                        id: approval_uuid,
                        session_id,
                        event_seq: seq,
                        tool: tool.clone(),
                        args: args.clone(),
                        created_at: evt.created_at,
                        ttl_secs: 300,
                        resolution: None,
                    };

                    // H1a: insert into the single store (no separate approval store).
                    {
                        let mut sup = supervisor.lock().await;
                        if let Err(e) = sup.0.insert_approval(&pending) {
                            warn!("pump: insert_approval (native) failed for {session_id}: {e}");
                            // Do not skip — still gate the pump so the session
                            // doesn't proceed without client acknowledgement.
                            // The row may already exist (idempotent insert failure
                            // on retry); the gate ensures ordering.
                        }
                    }

                    // Create per-approval Notify and register in gates map.
                    let notify = Arc::new(Notify::new());
                    {
                        let mut g = gates.lock().unwrap();
                        g.insert(approval_uuid, notify.clone());
                    }

                    // Build the approval_request frame for the broadcast.
                    // The event was already persisted (seq assigned above).
                    let frame = ServerEvent::ApprovalRequest {
                        approval: pending.clone(),
                    };
                    // Update the persisted event's metadata to embed the frame
                    // so late-joining clients can recover it from the snapshot.
                    let broadcast_evt = Event {
                        session_id,
                        seq,
                        sender: "system".into(),
                        kind: "approval_request".into(),
                        content: format!("approval needed for tool: {tool}"),
                        requires_user_input: true,
                        created_at: evt.created_at,
                        metadata: serde_json::json!({
                            "approval_id": approval_uuid.to_string(),
                            "tool": tool,
                            "args": args,
                            "ttl_secs": 300,
                            "__approval_frame": serde_json::to_value(&frame).unwrap_or_default(),
                        }),
                    };
                    let _ = event_tx.send(broadcast_evt);
                    {
                        let mut sup = supervisor.lock().await;
                        sup.0.set_status(session_id, SessionStatus::AwaitingInput);
                    }

                    // PAUSE: await resolution of this approval.
                    notify.notified().await;

                    // Remove gate (cleanup regardless of outcome).
                    {
                        let mut g = gates.lock().unwrap();
                        g.remove(&approval_uuid);
                    }

                    // Read resolution from the single store.
                    let resolution = {
                        let sup = supervisor.lock().await;
                        sup.0
                            .get_approval(approval_uuid)
                            .ok()
                            .flatten()
                            .and_then(|a| a.resolution)
                    };

                    if resolution.as_deref() == Some("allow") {
                        // Permitted — session continues normally.
                        {
                            let mut sup = supervisor.lock().await;
                            sup.0.set_status(session_id, SessionStatus::Active);
                        }
                        // Do NOT re-broadcast the approval_request event; it was
                        // already sent above. The pump continues to the next event.
                    } else {
                        // Denied — emit tool_blocked and mark session active.
                        let denial_reason = resolution.as_deref().unwrap_or("denied");
                        let mut blocked_evt = Event {
                            session_id,
                            seq: 0,
                            sender: "system".into(),
                            kind: "tool_blocked".into(),
                            content: format!("tool '{tool}' blocked by authz ({denial_reason})"),
                            requires_user_input: false,
                            created_at: evt.created_at,
                            metadata: serde_json::json!({
                                "approval_id": approval_uuid.to_string(),
                                "tool": tool,
                                "reason": denial_reason,
                            }),
                        };
                        let blocked_seq = {
                            let mut sup = supervisor.lock().await;
                            sup.0.append_event(&blocked_evt).unwrap_or(seq + 1)
                        };
                        blocked_evt.seq = blocked_seq;
                        {
                            let mut sup = supervisor.lock().await;
                            sup.0.set_status(session_id, SessionStatus::Active);
                        }
                        let _ = event_tx.send(blocked_evt);
                    }

                    // Audit the gate decision.
                    {
                        let mut aud = audit.lock().await;
                        let _ = aud.0.append(
                            "system",
                            "session:native_tool_gate",
                            Some(session_id),
                            serde_json::json!({
                                "approval_id": approval_uuid.to_string(),
                                "tool": tool,
                                "resolution": resolution,
                            }),
                        );
                    }

                    continue; // already handled
                }

                // Update live status if awaiting input (non-approval events).
                if evt.requires_user_input {
                    let mut sup = supervisor.lock().await;
                    sup.0.set_status(session_id, SessionStatus::AwaitingInput);
                }

                // Broadcast to all subscribed clients.
                let _ = event_tx.send(evt);
            }

            AdapterEvent::Completed => {
                {
                    let mut aud = audit.lock().await;
                    let _ = aud.0.append(
                        "system",
                        "session:completed",
                        Some(session_id),
                        serde_json::json!({}),
                    );
                }
                let mut sup = supervisor.lock().await;
                let _ = sup.0.update_session_status(session_id, &SessionStatus::Completed);
                sup.0.set_status(session_id, SessionStatus::Completed);
                sup.0.remove_live(session_id);
                break;
            }

            AdapterEvent::Failed(msg) => {
                warn!("session {session_id} adapter failed: {msg}");
                {
                    let mut aud = audit.lock().await;
                    let _ = aud.0.append(
                        "system",
                        "session:failed",
                        Some(session_id),
                        serde_json::json!({"msg": msg}),
                    );
                }
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
