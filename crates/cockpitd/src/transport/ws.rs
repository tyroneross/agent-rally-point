// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! axum WebSocket server implementation of the cockpit wire protocol.
//!
//! Features:
//! - Binds to `127.0.0.1:<port>` (default 8787, override via `COCKPIT_ADDR`).
//!   Non-loopback binds are refused unless the operator acknowledges the risk
//!   (see `crate::policy`).
//! - Auth: first client frame must be `hello {token, protocol:1}`.
//! - Handles all commands from COCKPIT-WIRE.md.
//! - Fan-out: events from running sessions are broadcast to all subscribed clients.
//! - Seq-numbered replay: `open_session {from_seq:N}` → events with seq>N, no gaps/dupes.
//! - ~50ms output coalescing window.
//!
//! ## Ownership (ARP-005)
//!
//! Every authenticated connection acts as a [`Principal`]. The principal is
//! written to `sessions.owner_id` at launch, and the mutating commands check it:
//!
//! | Command | Owner-checked |
//! |---|---|
//! | `send_prompt`, `steer`, `close_session`, `approve` | yes — non-owner gets `forbidden` |
//! | `list_sessions`, `open_session`, `get_audit` | no — deliberately unscoped |
//!
//! Reads stay unscoped on purpose. With per-connection principals, scoping
//! reads would break the reconnect-and-replay invariant that the iOS client
//! depends on: a phone that drops WiFi comes back as a new principal and would
//! lose its own timeline. Read isolation needs stable per-client credentials,
//! which the shared bearer token cannot provide. A client that wants stable
//! *control* across reconnects sends `client_id` in `hello`.
//!
//! ## What the approval gate below does NOT do (ARP-003)
//!
//! See the `run_pump` doc comment. Short version: the gate filters the event
//! stream. It does not stop the child agent from running the tool.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    VERSION,
    authz::{self, AuthzPolicy},
    model::{Approval, Event, SessionStatus},
    policy,
    protocol::{ApproveDecision, ClientCommand, ServerEvent},
    supervisor::AdapterEvent,
    transport::{AppState, auth::Principal},
};

// H1a: `approval` has been removed from AppState. All approval operations
// (insert, get, resolve) go through `state.supervisor` which owns the single
// authoritative store for sessions + events + approvals.

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the axum WebSocket server. Blocks until the server shuts down.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(listener, state).await
}

/// Serve on a listener the caller already bound.
///
/// Tests use this: binding to port 0 and *then* handing the socket over removes
/// the window where another test grabs the port between allocation and bind.
pub async fn serve_on(listener: tokio::net::TcpListener, state: AppState) -> Result<()> {
    // H2: spawn the TTL auto-deny sweep task before accepting connections.
    // The task runs in the background for the lifetime of the server.
    // Interval is configurable via COCKPIT_SWEEP_INTERVAL_MS (default 5 s).
    crate::transport::sweep::spawn_sweep_task(
        Arc::clone(&state.supervisor),
        Arc::clone(&state.approval_gates),
    );

    let state = Arc::new(state);

    let app = Router::new().route("/", get(ws_handler)).with_state(state);

    match listener.local_addr() {
        Ok(addr) => info!("cockpitd {} serving on ws://{}", VERSION, addr),
        Err(e) => warn!("cockpitd {} serving (local_addr unavailable: {e})", VERSION),
    }

    axum::serve(listener, app).await?;
    Ok(())
}

// ── WebSocket upgrade handler ─────────────────────────────────────────────────

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

// ── Per-connection handler ────────────────────────────────────────────────────

/// Client context for a single WebSocket connection.
struct ClientConn {
    state: Arc<AppState>,
    /// The identity this connection acts as. Owns every session it launches.
    principal: Principal,
    /// Sessions this client is subscribed to (for fan-out).
    subscribed: Vec<Uuid>,
    /// Global broadcast receiver (receives all session events).
    event_rx: broadcast::Receiver<Event>,
}

impl ClientConn {
    fn new(state: Arc<AppState>, principal: Principal) -> Self {
        let event_rx = state.event_tx.subscribe();
        Self {
            state,
            principal,
            subscribed: Vec::new(),
            event_rx,
        }
    }
}

// ── Ownership enforcement (ARP-005) ───────────────────────────────────────────

/// Result of comparing a connection's principal against a resource's owner.
#[derive(Debug, PartialEq, Eq)]
enum OwnerVerdict {
    /// The caller owns the resource.
    Owned,
    /// The resource does not exist.
    NotFound,
    /// The resource exists and belongs to a different principal.
    Forbidden,
}

/// Compare `owner` (as recorded in the store) against the caller's principal.
///
/// `None` means the row is missing.
fn judge_owner(owner: Option<String>, principal: &Principal) -> OwnerVerdict {
    match owner {
        None => OwnerVerdict::NotFound,
        Some(o) if o == principal.as_str() => OwnerVerdict::Owned,
        Some(_) => OwnerVerdict::Forbidden,
    }
}

/// Build the wire error for a failed ownership check.
///
/// `forbidden` and `not_found` are kept distinct because an operator debugging a
/// two-client setup needs to know which of the two happened.
///
/// This does leak an existence signal. An earlier version of this comment argued
/// the leak did not matter because session IDs are unguessable v4 UUIDs — which
/// is beside the point while `list_sessions` returns every session to every
/// authenticated caller regardless of owner. Nothing here needs guessing. Reads
/// are deliberately unscoped today (see the table in the module header); if that
/// changes, this distinction should be revisited with it.
fn owner_error(verdict: &OwnerVerdict, kind: &str, id: Uuid) -> ServerEvent {
    match verdict {
        OwnerVerdict::NotFound => ServerEvent::Error {
            code: "not_found".into(),
            message: format!("{kind} {id} not found"),
        },
        _ => ServerEvent::Error {
            code: "forbidden".into(),
            message: format!(
                "{kind} {id} belongs to another client; send_prompt, steer, \
                 close_session, and approve are restricted to the owning client"
            ),
        },
    }
}

/// Owner check for a session. Returns `true` when the caller may proceed;
/// otherwise sends the rejection frame and returns `false`.
async fn allow_session_op(
    ctx: &ClientConn,
    session_id: Uuid,
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
) -> bool {
    let owner = {
        let sup = ctx.state.supervisor.lock().await;
        sup.0.session_owner(session_id).ok().flatten()
    };
    let verdict = judge_owner(owner, &ctx.principal);
    if verdict == OwnerVerdict::Owned {
        return true;
    }
    warn!(
        "principal {} refused on session {session_id} ({verdict:?})",
        ctx.principal
    );
    let err = owner_error(&verdict, "session", session_id);
    let _ = sink
        .send(Message::Text(serde_json::to_string(&err).unwrap()))
        .await;
    false
}

/// Handle a single WebSocket connection.
async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    // ── Auth: wait for hello ──────────────────────────────────────────────────
    let mut principal: Option<Principal> = None;
    let authed = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.get("t").and_then(|t| t.as_str()) == Some("hello") => {
                let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
                match super::auth::validate_token(token) {
                    Ok(()) => {
                        // ARP-005: mint this connection's identity. A client that
                        // asserts `client_id` keeps its sessions across reconnects;
                        // otherwise the identity dies with the socket.
                        let claimed = v.get("client_id").and_then(|c| c.as_str());
                        principal = Some(Principal::resolve(claimed));
                        let ok = ServerEvent::HelloOk {
                            server_version: VERSION.to_string(),
                            protocol: 1,
                        };
                        let _ = sink
                            .send(Message::Text(serde_json::to_string(&ok).unwrap()))
                            .await;
                        true
                    }
                    Err(reason) => {
                        let err = ServerEvent::Error {
                            code: "auth_failed".into(),
                            message: reason.to_string(),
                        };
                        let _ = sink
                            .send(Message::Text(serde_json::to_string(&err).unwrap()))
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
                    .send(Message::Text(serde_json::to_string(&err).unwrap()))
                    .await;
                false
            }
        },
        _ => false,
    };

    if !authed {
        debug!("client rejected (bad auth)");
        return;
    }

    let principal = principal.unwrap_or_else(Principal::per_connection);
    info!("client authenticated as {principal}");

    let mut ctx = ClientConn::new(state, principal);
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
        let frame = ServerEvent::Event { session_id, event };
        let _ = sink
            .send(Message::Text(serde_json::to_string(&frame).unwrap()))
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
                .send(Message::Text(serde_json::to_string(&err).unwrap()))
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
                    serde_json::to_string(&ServerEvent::Pong).unwrap(),
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
                .send(Message::Text(serde_json::to_string(&frame).unwrap()))
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
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
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
                        .send(Message::Text(serde_json::to_string(&frame).unwrap()))
                        .await;

                    // Subscribe to live deltas.
                    if !ctx.subscribed.contains(&session_id) {
                        ctx.subscribed.push(session_id);
                    }
                }
            }
        }

        ClientCommand::SendPrompt { session_id, text } => {
            // ARP-005: only the launching principal may drive the session.
            if !allow_session_op(ctx, session_id, sink).await {
                return;
            }
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.send_prompt(session_id, &text) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "send_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                }
            }
        }

        ClientCommand::Steer { session_id, text } => {
            // ARP-005: steering is a write; same owner check as send_prompt.
            if !allow_session_op(ctx, session_id, sink).await {
                return;
            }
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.send_prompt(session_id, &text) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "steer_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
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

            // ARP-005: an approval inherits its session's owner. Resolving one
            // decides whether another client's agent proceeds, so it is a write
            // and gets the same check as send/steer/close.
            let (session_id_for_audit, owner) = {
                let sup = ctx.state.supervisor.lock().await;
                let session_id = sup
                    .0
                    .get_approval(approval_id)
                    .ok()
                    .flatten()
                    .map(|a| a.session_id);
                let owner = sup.0.approval_owner(approval_id).ok().flatten();
                (session_id, owner)
            };

            let verdict = judge_owner(owner, &ctx.principal);
            if verdict != OwnerVerdict::Owned {
                warn!(
                    "principal {} refused on approval {approval_id} ({verdict:?})",
                    ctx.principal
                );
                let err = owner_error(&verdict, "approval", approval_id);
                let _ = sink
                    .send(Message::Text(serde_json::to_string(&err).unwrap()))
                    .await;
                return;
            }

            // H1a: all approvals live in the supervisor's store (single store).
            let resolve_result = {
                let mut sup = ctx.state.supervisor.lock().await;
                sup.0.resolve_approval(approval_id, decision_str)
            };

            match resolve_result {
                Ok(_) => {
                    // Audit the resolution.
                    {
                        let mut audit = ctx.state.audit.lock().await;
                        let _ = audit.0.append(
                            ctx.principal.as_str(),
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
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let err = ServerEvent::Error {
                        code: "approve_failed".into(),
                        message: err_msg,
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                }
            }
        }

        ClientCommand::GetAudit { session_id, limit } => {
            let audit = ctx.state.audit.lock().await;
            match audit.0.list(session_id, limit) {
                Ok(entries) => {
                    let frame = ServerEvent::AuditList { entries };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&frame).unwrap()))
                        .await;
                }
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "audit_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                }
            }
        }

        ClientCommand::LaunchSession {
            agent_type,
            repo_path,
            prompt,
        } => {
            // ARP-005: repo_path becomes the child process's working directory.
            // Canonicalize it and require it to sit inside a configured root
            // before anything is spawned. The canonical path is what gets
            // handed to the adapter, so the directory that was checked is the
            // directory the child runs in.
            let resolved_repo = match policy::resolve_repo_path(&repo_path) {
                Ok(p) => p,
                Err(rejection) => {
                    warn!("principal {} launch refused: {rejection}", ctx.principal);
                    {
                        let mut audit = ctx.state.audit.lock().await;
                        let _ = audit.0.append(
                            ctx.principal.as_str(),
                            "session:launch_refused",
                            None,
                            serde_json::json!({
                                "agent_type": agent_type,
                                "repo_path": repo_path,
                                "reason": rejection.to_string(),
                            }),
                        );
                    }
                    let err = ServerEvent::Error {
                        code: "repo_path_denied".into(),
                        message: rejection.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                    return;
                }
            };
            let repo_path = resolved_repo.to_string_lossy().to_string();

            let session_id = {
                let mut sup = ctx.state.supervisor.lock().await;
                let event_tx = ctx.state.event_tx.clone();
                sup.0.launch_session(
                    &agent_type,
                    &repo_path,
                    prompt.as_deref(),
                    ctx.principal.as_str(),
                    event_tx,
                )
            };

            match session_id {
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "launch_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                }
                Ok(sid) => {
                    // Audit: session launched.
                    {
                        let mut audit = ctx.state.audit.lock().await;
                        let _ = audit.0.append(
                            ctx.principal.as_str(),
                            "session:launch",
                            Some(sid),
                            serde_json::json!({
                                "agent_type": agent_type,
                                "repo_path": repo_path,
                                "owner": ctx.principal.as_str(),
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
                        .send(Message::Text(serde_json::to_string(&frame).unwrap()))
                        .await;
                }
            }
        }

        ClientCommand::CloseSession { session_id } => {
            // ARP-005: killing another client's agent is a write.
            if !allow_session_op(ctx, session_id, sink).await {
                return;
            }
            let mut sup = ctx.state.supervisor.lock().await;
            match sup.0.kill_session(session_id) {
                Ok(()) => {}
                Err(e) => {
                    let err = ServerEvent::Error {
                        code: "close_failed".into(),
                        message: e.to_string(),
                    };
                    let _ = sink
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
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
/// # THIS IS NOT AN EXECUTION GATE (ARP-003)
///
/// Read this before trusting anything below it.
///
/// Cockpit spawns the agent CLI as a child process (`claude -p …`,
/// `codex exec --json`) and reads its stdout. A `tool_call` reaches this
/// function *after* the child has already decided to run the tool — often after
/// it has already run it. Pausing this loop pauses **our reading of the child's
/// output**. It does not pause the child. It sends the child nothing. The
/// adapter's read task and the child process keep running while the pump is
/// parked.
///
/// So the true guarantee of the "approval gate" is:
///
/// > The tool has already run, or may run at any moment regardless of the
/// > decision. Cockpit filters what the operator is shown. It does not control
/// > what the agent does.
///
/// A `deny` therefore means "not forwarded to the UI", never "prevented".
/// The `tool_blocked` event carries `advisory: true` and `enforced: false` to
/// say this on the wire.
///
/// What the gate is still worth: it surfaces tool activity for review, it
/// records an audit trail, and it holds the session in `awaiting_input` so an
/// operator sees the decision point. Treat it as a tripwire, not a control.
///
/// Anything that needs real containment must come from outside this process —
/// run the child under an OS sandbox (`sandbox-exec`, a container, a
/// least-privilege user) with the permissions you are willing to grant
/// unconditionally.
///
/// The closing conditions for this finding live in
/// `arp003_execution_gate_definition_of_done` in `tests/e2e.rs`.
///
/// ## Mechanics (G1 + H1b)
/// When the adapter emits a `tool_call` Event, `authz::decide` is called with
/// the conservative policy.  Decision outcomes:
/// - `Permit` → broadcast the event as normal.
/// - `RequireApproval` → register a pending Approval, broadcast an
///   `approval_request` event so subscribed clients can respond, then park this
///   session's pump on a per-approval `Notify`.  The `Approve` WS command calls
///   `notify.notify_one()` after resolving the approval row.
///   After wakeup the pump reads the resolution:
///   - `allow` → continue (broadcast the tool_call event).
///   - `deny` / `auto_denied` / anything else → emit an advisory `tool_blocked`
///     event and stop forwarding that tool_call to clients.
///
/// H1b: native `approval_request` events from the Codex adapter follow the same
/// path: the pump parks on a `Notify` until the client resolves the approval via
/// the `Approve` WS command. Same caveat — Codex is spawned with stdin closed,
/// so there is not even a channel on which to answer it.
///
/// The park is per-approval (not global), so other sessions' pumps are never
/// stalled.  A tokio `Notify` is used rather than a channel so spurious wakeups
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
                            // Denied — stop forwarding this tool_call to clients
                            // and say so. ARP-003: the child agent was never told
                            // about the decision and may already have run the
                            // tool, so this is advisory, not enforcement. The
                            // `kind` stays `tool_blocked` for wire compatibility
                            // (docs/plans/COCKPIT-WIRE.md); the honest semantics
                            // ride in the metadata flags and the content string.
                            let denial_reason = resolution.as_deref().unwrap_or("denied");
                            let mut blocked_evt = Event {
                                session_id,
                                seq: 0,
                                sender: "system".into(),
                                kind: "tool_blocked".into(),
                                content: format!(
                                    "tool '{tool_name}' not forwarded ({denial_reason}) — \
                                     advisory only, the agent process was not prevented from running it"
                                ),
                                requires_user_input: false,
                                created_at: evt.created_at,
                                metadata: serde_json::json!({
                                    "approval_id": approval_uuid.to_string(),
                                    "tool": tool_name,
                                    "reason": denial_reason,
                                    // ARP-003 honesty flags — see run_pump docs.
                                    "advisory": true,
                                    "enforced": false,
                                    "semantics": "not forwarded to clients; the child agent was not stopped and may have already executed this tool",
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
                    let approval_uuid =
                        Uuid::parse_str(approval_id_str).unwrap_or_else(|_| Uuid::new_v4());

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
                        // Denied — advisory only. See the ARP-003 note on
                        // run_pump: Codex is spawned with stdin closed, so the
                        // denial cannot reach the child even in principle.
                        let denial_reason = resolution.as_deref().unwrap_or("denied");
                        let mut blocked_evt = Event {
                            session_id,
                            seq: 0,
                            sender: "system".into(),
                            kind: "tool_blocked".into(),
                            content: format!(
                                "tool '{tool}' not forwarded ({denial_reason}) — advisory only, \
                                 the agent process was not prevented from running it"
                            ),
                            requires_user_input: false,
                            created_at: evt.created_at,
                            metadata: serde_json::json!({
                                "approval_id": approval_uuid.to_string(),
                                "tool": tool,
                                "reason": denial_reason,
                                // ARP-003 honesty flags — see run_pump docs.
                                "advisory": true,
                                "enforced": false,
                                "semantics": "not forwarded to clients; the child agent was not stopped and may have already executed this tool",
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
                let _ = sup
                    .0
                    .update_session_status(session_id, &SessionStatus::Completed);
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
                let _ = sup
                    .0
                    .update_session_status(session_id, &SessionStatus::Failed);
                sup.0.set_status(session_id, SessionStatus::Failed);
                sup.0.remove_live(session_id);
                break;
            }
        }
    }

    debug!("pump for session {session_id} exited");
}
