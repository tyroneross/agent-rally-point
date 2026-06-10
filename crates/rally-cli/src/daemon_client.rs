// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Minimal client for the rally-termd (ptyd) daemon — daemon-first inject
//! routing (move 2, `docs/PLAN-daemon-first-inject-routing.md`).
//!
//! This is the CLI side of the two-tier inject design: when the rally-termd
//! daemon is reachable, `rally run`/`rally adopt` register the launched pane's
//! logical agent-id with the daemon (`agent.register`). A registered session's
//! `inject` then routes LEDGER-ONLY — the daemon owns the PTY and performs the
//! write + posts a Receipt, so the CLI never puppets the TUI with keystrokes.
//! When no daemon is reachable, registration is a graceful no-op and `inject`
//! keeps the framed-tmux fallback.
//!
//! ## Wire protocol
//!
//! ptyd speaks line-delimited JSON-RPC over a unix socket (ptyd
//! `src/wire.rs`): write `{"id","method","params"}\n`, read one response line.
//! `agent.register` params are `{pane, identity, transport, transcript_path,
//! force}` and the success result is
//! `{pane_id, identity, transport_resolved, rebound}` (ptyd `main.rs`
//! `"agent.register"` arm). We mirror that shape with no dependency on the ptyd
//! crate (the CLI must stay buildable without it).
//!
//! ## Failure posture — fail-OPEN
//!
//! Every entry point degrades gracefully: a missing/ambiguous socket, a refused
//! connection, a daemon error, or a malformed reply all return
//! `Ok(None)`/`RegisterOutcome::Unavailable` rather than aborting the run. The
//! framed-tmux fallback is always the safety net, so "no daemon" must never be
//! a hard error.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

/// Default round-trip timeout for a daemon request. Registration is a cheap
/// local-socket call; if the daemon does not answer quickly it is treated as
/// unavailable and the caller falls back.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);

/// Env override for the RALLY-OWNED ptyd socket used by the spawn path (`rally
/// run --backend ptyd`). F3: this is DELIBERATELY separate from
/// `detect_host_runtime`'s candidate list (which includes Easy Terminal's
/// production daemon). The spawn path must NEVER default into a user-facing
/// daemon — rally agents own their own daemon at this socket.
pub(crate) const RALLY_PTYD_SOCKET_ENV: &str = "RALLY_PTYD_SOCKET";
/// Env override for the ptyd binary used to autostart the rally-owned daemon.
pub(crate) const RALLY_PTYD_BIN_ENV: &str = "RALLY_PTYD_BIN";

/// Resolve the RALLY-OWNED ptyd socket path (F3). Precedence:
///   1. `$RALLY_PTYD_SOCKET` (explicit override — tests + ET opt-in use this).
///   2. `~/.local/share/rally/ptyd.sock` (the rally-dedicated default).
///
/// This NEVER returns the Easy Terminal production socket — the rally daemon is
/// a distinct instance with its own state dir. `detect_host_runtime`'s wider
/// candidate scan (used for tmux-session registration) is intentionally NOT
/// consulted here.
pub(crate) fn rally_owned_socket() -> Option<String> {
    if let Ok(explicit) = std::env::var(RALLY_PTYD_SOCKET_ENV) {
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(format!("{home}/.local/share/rally/ptyd.sock"))
}

/// The rally-owned ptyd state dir (sibling of the socket default). Passed to an
/// autostarted daemon via `PTYD_STATE_DIR` so it never shares Easy Terminal's
/// persisted session tree.
fn rally_owned_state_dir() -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(format!("{home}/.local/share/rally/ptyd-state"))
}

/// True iff a daemon is LIVE at `socket` — connectable AND answers a cheap
/// `pane.list` probe. `auto` backend selection requires liveness, not mere
/// file-existence: a stale socket file from a crashed daemon must NOT win.
pub(crate) fn socket_is_live(socket: &str) -> bool {
    if !Path::new(socket).exists() {
        return false;
    }
    round_trip(socket, "pane.list", &json!({}), DAEMON_TIMEOUT)
        .ok()
        .and_then(|reply| {
            reply
                .get("result")
                .and_then(|r| r.get("type"))
                .and_then(|t| t.as_str())
                .map(|t| t == "pane_list")
        })
        .unwrap_or(false)
}

/// Result of an `agent.register` attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    /// The daemon accepted the registration; the agent-id is now bound to a
    /// daemon-owned pane. Carries the daemon's pane handle for the session
    /// record (`ManagedSession::daemon_pane`).
    Registered { pane_id: String },
    /// No daemon was reachable (socket absent, ambiguous, refused, timed out)
    /// or it returned an error. The caller keeps the framed-tmux fallback. The
    /// reason is advisory only — it never blocks the run.
    Unavailable { reason: String },
}

/// The success branch of the `agent.register` JSON-RPC reply
/// (`ResponseResult::Registered` in ptyd `main.rs`).
#[derive(Debug, Deserialize)]
struct RegisteredResult {
    pane_id: String,
    /// The identity the daemon actually bound. Verified against the requested
    /// identity in `parse_register_reply` (see its doc comment) — a mismatch is
    /// not trusted as a successful registration.
    identity: Option<String>,
    #[allow(dead_code)]
    transport_resolved: Option<String>,
    #[allow(dead_code)]
    rebound: Option<bool>,
}

/// Resolve the single unambiguous daemon socket path. Returns `None` when no
/// socket exists OR when more than one resolvable socket is present (the
/// `whoami` ambiguity rule: an agent must never guess which daemon to bind).
///
/// `candidates` is the same precedence list `detect_host_runtime` builds; the
/// caller passes it in so socket resolution stays in one place and is testable.
pub(crate) fn resolve_unambiguous_socket(found: &[String]) -> Option<String> {
    match found {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Register `identity` (the logical agent-id, e.g. `claude_code:01`) with the
/// daemon at `socket`, binding it to the daemon-owned pane named `pane`.
///
/// `pane` is the pane handle the daemon already owns. For a session that ptyd
/// itself launched the pane id is known; for a `rally run` tmux/cmux session
/// the pane is NOT a ptyd pane, so the daemon returns `pane_not_found` and we
/// surface `Unavailable` (the framed-tmux fallback then carries delivery). This
/// is the documented boundary: full daemon routing for `rally run` panes
/// requires ptyd to own the pane, which is the live-flip step tracked in the
/// plan.
///
/// Fail-open: any I/O or protocol failure becomes `Unavailable`.
pub(crate) fn register_agent(socket: &str, identity: &str, pane: &str) -> RegisterOutcome {
    if !Path::new(socket).exists() {
        return RegisterOutcome::Unavailable {
            reason: format!("daemon socket {socket} not present"),
        };
    }
    let params = json!({
        "pane": pane,
        "identity": identity,
        // transport defaults are resolved daemon-side; omit to take its default.
        "force": false,
    });
    match round_trip(socket, "agent.register", &params, DAEMON_TIMEOUT) {
        Ok(reply) => parse_register_reply(&reply, identity),
        Err(e) => RegisterOutcome::Unavailable {
            reason: format!("daemon register call failed: {e}"),
        },
    }
}

/// Parse an `agent.register` reply into a `RegisterOutcome`. A JSON-RPC reply is
/// either `{"result": {...Registered}}` or `{"error": {...}}`.
///
/// `requested_identity` is verified against the daemon's echoed `identity`
/// (security-reviewer LOW/MED, 2026-06-09): a daemon that bound a DIFFERENT
/// identity than the one we asked for must NOT be trusted as a successful
/// registration — treat the mismatch as `Unavailable` and fall back. The reply
/// is only honored when the bound identity matches (or is absent, which older
/// daemons may do — then we trust the pane handle only).
fn parse_register_reply(reply: &serde_json::Value, requested_identity: &str) -> RegisterOutcome {
    if let Some(err) = reply.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.as_str())
            .unwrap_or("unknown daemon error");
        return RegisterOutcome::Unavailable {
            reason: format!("daemon refused register: {msg}"),
        };
    }
    match reply
        .get("result")
        .cloned()
        .map(serde_json::from_value::<RegisteredResult>)
    {
        Some(Ok(r)) => {
            // Identity-echo verification (security-reviewer LOW/MED, 2026-06-09):
            // a daemon that echoed a DIFFERENT identity than the one we asked to
            // bind must NOT be trusted as a successful registration — fall back.
            // Older daemons may omit the echo entirely; absent is trusted (we
            // rely on the pane handle alone), but a present-and-mismatched echo
            // is rejected.
            if let Some(bound) = r.identity.as_deref() {
                if bound != requested_identity {
                    return RegisterOutcome::Unavailable {
                        reason: format!(
                            "daemon bound identity {bound:?} but {requested_identity:?} was requested"
                        ),
                    };
                }
            }
            RegisterOutcome::Registered { pane_id: r.pane_id }
        }
        Some(Err(e)) => RegisterOutcome::Unavailable {
            reason: format!("malformed register result: {e}"),
        },
        None => RegisterOutcome::Unavailable {
            reason: "register reply had neither result nor error".to_string(),
        },
    }
}

// ===========================================================================
// Spawn path (rally run --backend ptyd) — ptyd OWNS the pane via RPC.
// Unlike `register_agent` (fail-open), spawn/inject errors here are REAL
// errors: a ptyd-backed `rally run` must not silently degrade to a phantom
// session. The caller decides any fallback (F2).
// ===========================================================================

/// Outcome of an `agent.start` spawn RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartOutcome {
    /// ptyd spawned the agent; `pane_id` is the daemon-owned handle that
    /// becomes both `session.target` and `session.daemon_pane`.
    Started { pane_id: String },
    /// The daemon refused or was unreachable. `reason` is surfaced to the user.
    Failed { reason: String },
}

/// The success branch of an `agent.start` reply (`AgentStarted { pane }`).
#[derive(Debug, Deserialize)]
struct AgentStartedResult {
    pane: PaneIdOnly,
}

#[derive(Debug, Deserialize)]
struct PaneIdOnly {
    pane_id: String,
}

/// Spawn an agent pane OWNED by the rally ptyd daemon. F3/design-1: always
/// `focus:false` and a dedicated `workspace_id` so the pane never lands in a
/// user's focused tab. `command` is the agent argv (e.g. `["claude","--name",
/// "x"]`).
///
/// Wire: `agent.start {name, cwd, command, focus:false, workspace_id}` → ptyd
/// `main.rs` `"agent.start"` arm → `ResponseResult::AgentStarted { pane }`
/// (verified against ptyd `protocol.rs:120` + `tests/agent_lifecycle.rs:201`).
pub(crate) fn start_agent(
    socket: &str,
    name: &str,
    cwd: &Path,
    command: &[String],
    workspace_id: &str,
) -> StartOutcome {
    let params = json!({
        "name": name,
        "cwd": cwd.display().to_string(),
        "command": command,
        // Design-1: rally panes NEVER steal the user's focused tab.
        "focus": false,
        "workspace_id": workspace_id,
    });
    match round_trip(socket, "agent.start", &params, DAEMON_TIMEOUT) {
        Ok(reply) => parse_start_reply(&reply),
        Err(e) => StartOutcome::Failed {
            reason: format!("agent.start call failed: {e}"),
        },
    }
}

fn parse_start_reply(reply: &serde_json::Value) -> StartOutcome {
    if let Some(err) = reply.get("error") {
        return StartOutcome::Failed {
            reason: format!("daemon refused agent.start: {}", err_message(err)),
        };
    }
    match reply
        .get("result")
        .cloned()
        .map(serde_json::from_value::<AgentStartedResult>)
    {
        Some(Ok(r)) => StartOutcome::Started {
            pane_id: r.pane.pane_id,
        },
        Some(Err(e)) => StartOutcome::Failed {
            reason: format!("malformed agent.start result: {e}"),
        },
        None => StartOutcome::Failed {
            reason: "agent.start reply had neither result nor error".to_string(),
        },
    }
}

/// Ensure a rally-dedicated workspace exists, returning its id so spawned panes
/// land there instead of the user's focused tab. ptyd's `agent.start` rejects an
/// unknown `workspace_id` ("no such workspace", pane.rs:3205), so a valid one is
/// required up front.
///
/// [C]: LIST-then-REUSE by label, so repeated `rally run`s do NOT pile up N
/// workspaces (each with its own orphan root shell). We first `workspace.list`
/// and reuse the existing rally workspace when one carries `label`; only when
/// none exists do we `workspace.create`. Wire:
///   * `workspace.list {}` → `WorkspaceList { workspaces:[{workspace_id,label,..}] }`
///     (ptyd `main.rs:489` + `protocol.rs:79`, `WorkspaceInfo` `protocol.rs:424`).
///   * `workspace.create {label}` → `WorkspaceCreated { workspace, .. }`
///     (ptyd `main.rs:511` + `protocol.rs:88`).
pub(crate) fn ensure_rally_workspace(socket: &str, label: &str) -> Result<String, String> {
    // 1. Reuse an existing rally workspace if one is already labeled `label`.
    if let Some(existing) = find_workspace_by_label(socket, label)? {
        return Ok(existing);
    }
    // 2. None found → create one.
    let reply = round_trip(
        socket,
        "workspace.create",
        &json!({ "label": label }),
        DAEMON_TIMEOUT,
    )
    .map_err(|e| format!("workspace.create call failed: {e}"))?;
    if let Some(err) = reply.get("error") {
        return Err(format!(
            "daemon refused workspace.create: {}",
            err_message(err)
        ));
    }
    reply
        .get("result")
        .and_then(|r| r.get("workspace"))
        .and_then(|w| w.get("workspace_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "workspace.create reply missing workspace_id".to_string())
}

/// [C]: find an existing workspace whose `label` matches, returning its
/// `workspace_id`. A `workspace.list` failure is fatal here (the caller would
/// otherwise create a duplicate); a daemon that simply has no matching workspace
/// returns `Ok(None)`. Wire: ptyd `main.rs:489` (`workspace_list` result type,
/// `WorkspaceInfo{workspace_id,label}` at `protocol.rs:424-426`).
fn find_workspace_by_label(socket: &str, label: &str) -> Result<Option<String>, String> {
    let reply = round_trip(socket, "workspace.list", &json!({}), DAEMON_TIMEOUT)
        .map_err(|e| format!("workspace.list call failed: {e}"))?;
    if let Some(err) = reply.get("error") {
        return Err(format!(
            "daemon refused workspace.list: {}",
            err_message(err)
        ));
    }
    let Some(workspaces) = reply
        .get("result")
        .and_then(|r| r.get("workspaces"))
        .and_then(|w| w.as_array())
    else {
        return Err("workspace.list reply missing workspaces array".to_string());
    };
    Ok(workspaces
        .iter()
        .find(|w| w.get("label").and_then(|l| l.as_str()) == Some(label))
        .and_then(|w| w.get("workspace_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

/// The parsed `Receipt` reply of an `agent.send` (ptyd `protocol.rs:160`,
/// built by `build_receipt` at ptyd `main.rs:1148`). We read `pane_id` (F4
/// cross-check) and `state` (ack-state mapping).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendReceipt {
    pub(crate) pane_id: String,
    pub(crate) state: String,
}

/// Outcome of an `agent.send` RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    /// The daemon accepted the send and returned a Receipt.
    Sent(SendReceipt),
    /// The daemon refused or was unreachable.
    Failed { reason: String },
}

/// Deliver `text` to the daemon-owned pane bound to `identity` via `agent.send`.
/// The caller MUST pass already-sanitized text (F1: `sanitize_inject_text`
/// applied before this call).
///
/// ## Why `submit:true` + `confirm:"sent"` (NOT a bare `{to,text}`)
///
/// ptyd treats the PRESENCE of `to` (or any of `submit`/`confirm`/
/// `confirm_timeout_ms`/`paste_frame`) as the NON-legacy framing path
/// (`legacy = !(...)`, ptyd `src/main.rs:670-674`). A `{to,text}` send is
/// therefore NON-legacy, and the non-legacy arm defaults `submit` to FALSE
/// (`src/main.rs:724-728`) — the text would be pasted into the agent's TUI
/// input box and NEVER submitted (the L5 inject-no-submit failure the tmux
/// framer fixes with a trailing CR). We MUST pass `submit:true` so ptyd's
/// `frame_line` appends the carriage return that submits the line
/// (`src/comms.rs:48-50`: `if submit { out.push(CR) }`).
///
/// We also pass `confirm:"sent"`. The non-legacy arm otherwise defaults
/// `confirm` to `"seen"` with a 4000ms timeout (`src/main.rs:729-736`), and
/// `confirm:"seen"` makes `deliver_line` BLOCK waiting for the agent to echo
/// the text back (`src/pane.rs:1640-1688`) — up to 4s, which EXCEEDS this
/// CLI's 3s round-trip read timeout (`DAEMON_TIMEOUT`), so a successful write
/// is reported as a spurious "failed" and a retry would DOUBLE-paste.
/// `confirm:"sent"` parses to `ReceiptState::Sent` (`src/comms.rs:198`);
/// `deliver_line` then sees `want_seen == false` and returns IMMEDIATELY after
/// the write (`src/pane.rs:1640,1646-1662`), well under 3s.
///
/// Wire: `agent.send {to, text, submit:true, confirm:"sent"}` → ptyd
/// `src/main.rs:665` non-legacy arm → `ResponseResult::Receipt { to, pane_id,
/// transport, state, evidence }` (ptyd `src/protocol.rs:160`, built by
/// `build_receipt` at `src/main.rs:1148`). `state` is `"sent"` for this
/// ceiling.
pub(crate) fn send_agent(socket: &str, identity: &str, text: &str) -> SendOutcome {
    let params = json!({
        "to": identity,
        "text": text,
        // Append the submitting CR (else ptyd pastes-without-submitting; [A]/[B]).
        "submit": true,
        // Resolve on bytes-written, NOT echo-seen — keeps the round trip < 3s.
        "confirm": "sent",
    });
    match round_trip(socket, "agent.send", &params, DAEMON_TIMEOUT) {
        Ok(reply) => parse_send_reply(&reply),
        Err(e) => SendOutcome::Failed {
            reason: format!("agent.send call failed: {e}"),
        },
    }
}

fn parse_send_reply(reply: &serde_json::Value) -> SendOutcome {
    if let Some(err) = reply.get("error") {
        return SendOutcome::Failed {
            reason: format!("daemon refused agent.send: {}", err_message(err)),
        };
    }
    let Some(result) = reply.get("result") else {
        return SendOutcome::Failed {
            reason: "agent.send reply had neither result nor error".to_string(),
        };
    };
    let pane_id = result.get("pane_id").and_then(|v| v.as_str());
    let state = result.get("state").and_then(|v| v.as_str());
    match (pane_id, state) {
        (Some(pane_id), Some(state)) => SendOutcome::Sent(SendReceipt {
            pane_id: pane_id.to_string(),
            state: state.to_string(),
        }),
        _ => SendOutcome::Failed {
            reason: format!("malformed agent.send receipt: {result}"),
        },
    }
}

/// Stop (reap) the daemon-owned pane named `name` via `agent.stop`. Used by
/// `rally stop` on a ptyd session (the user addresses sessions by name). Wire:
/// `agent.stop {name}` → `ResponseResult::Ok` (ptyd `main.rs:852` +
/// `tests/agent_lifecycle.rs:365`).
pub(crate) fn stop_agent(socket: &str, name: &str) -> Result<(), String> {
    let reply = round_trip(
        socket,
        "agent.stop",
        &json!({ "name": name }),
        DAEMON_TIMEOUT,
    )
    .map_err(|e| format!("agent.stop call failed: {e}"))?;
    if let Some(err) = reply.get("error") {
        return Err(format!("daemon refused agent.stop: {}", err_message(err)));
    }
    Ok(())
}

/// [G]: Stop (reap) the daemon-owned pane by its PANE ID via `pane.close`. The
/// F2 register-fail rollback holds the exact pane id it just spawned, so it
/// reaps THAT pane — reaping by name (`agent.stop`) could hit a different pane
/// on a label collision. Wire: `pane.close {pane_id}` → `ResponseResult::Ok`
/// (ptyd `main.rs:567` — `close_pane(pane_id)` then `Ok{}`; `pane_not_found`
/// error otherwise).
pub(crate) fn close_pane_by_id(socket: &str, pane_id: &str) -> Result<(), String> {
    let reply = round_trip(
        socket,
        "pane.close",
        &json!({ "pane_id": pane_id }),
        DAEMON_TIMEOUT,
    )
    .map_err(|e| format!("pane.close call failed: {e}"))?;
    if let Some(err) = reply.get("error") {
        return Err(format!("daemon refused pane.close: {}", err_message(err)));
    }
    Ok(())
}

/// Read recent reconstructed text from the daemon-owned pane named `name` via
/// `agent.read`. Wire: `agent.read {name, source:"recent", lines}` →
/// `ResponseResult::AgentRead { text, bytes }` (ptyd `main.rs:785` +
/// `protocol.rs:129`).
pub(crate) fn read_agent(socket: &str, name: &str, lines: usize) -> Result<String, String> {
    let params = json!({ "name": name, "source": "recent", "lines": lines });
    let reply = round_trip(socket, "agent.read", &params, DAEMON_TIMEOUT)
        .map_err(|e| format!("agent.read call failed: {e}"))?;
    if let Some(err) = reply.get("error") {
        return Err(format!("daemon refused agent.read: {}", err_message(err)));
    }
    reply
        .get("result")
        .and_then(|r| r.get("text"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "agent.read reply missing text".to_string())
}

/// Liveness probe: returns the set of live pane ids the daemon reports via
/// `pane.list`. The caller maps its session targets against this set. Wire:
/// `pane.list {}` → `ResponseResult::PaneList { panes:[{pane_id,..}] }` (ptyd
/// `main.rs:433` + `protocol.rs:70`). On any failure returns `None` so the
/// caller can map to `Unknown` (never a false `Stale`).
pub(crate) fn live_pane_ids(socket: &str) -> Option<Vec<String>> {
    let reply = round_trip(socket, "pane.list", &json!({}), DAEMON_TIMEOUT).ok()?;
    let panes = reply.get("result")?.get("panes")?.as_array()?;
    Some(
        panes
            .iter()
            .filter_map(|p| {
                p.get("pane_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect(),
    )
}

/// Autostart the rally-owned ptyd daemon for an explicit `--backend ptyd` run.
/// Spawns `<bin> server` detached with the rally-owned socket + state dir env,
/// then waits ≤5s for the socket to become LIVE. The binary is `$RALLY_PTYD_BIN`
/// if set, else `ptyd` resolved on PATH (installed at `~/.local/bin/ptyd`).
///
/// Returns `Ok(())` once the socket answers, or an `Err` describing why it
/// could not be started (missing binary, never bound) — the caller fails the
/// run with that message.
pub(crate) fn autostart_daemon(socket: &str) -> Result<(), String> {
    let bin = ptyd_binary().ok_or_else(|| {
        format!(
            "rally ptyd daemon is not live at {socket} and no ptyd binary was found \
             (set {RALLY_PTYD_BIN_ENV} or install `ptyd` on PATH at ~/.local/bin/ptyd)"
        )
    })?;
    let state_dir = rally_owned_state_dir()
        .ok_or_else(|| "cannot resolve rally ptyd state dir (HOME unset)".to_string())?;
    if let Some(parent) = Path::new(socket).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::create_dir_all(&state_dir);

    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("server")
        .env("PTYD_SOCKET_PATH", socket)
        .env("PTYD_STATE_DIR", &state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // [H]: detach into a NEW session via setsid() so the daemon has no
    // controlling terminal — a SIGHUP when the launching terminal closes will
    // NOT reach it. Without this, closing the shell that ran `rally run` would
    // kill the rally daemon (and every agent pane it owns). `setsid()` runs in
    // the forked child before exec; it fails only if the child is already a
    // session leader (it is not), so an error there is genuinely exceptional.
    //
    // SAFETY: `pre_exec` runs in the child after fork, before exec. `setsid` is
    // async-signal-safe and touches no parent-process state, satisfying the
    // post-fork restrictions. We declare `setsid` directly (it is in libc) to
    // avoid taking a new direct crate dependency for one syscall.
    unsafe {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        cmd.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("failed to spawn `{} server`: {e}", bin.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket_is_live(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "rally ptyd daemon did not bind {socket} within 5s after autostart"
    ))
}

/// Resolve the ptyd binary for autostart: `$RALLY_PTYD_BIN` (must exist) else
/// `ptyd` found on PATH.
fn ptyd_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(RALLY_PTYD_BIN_ENV) {
        if !explicit.is_empty() {
            let p = PathBuf::from(&explicit);
            return p.exists().then_some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("ptyd"))
        .find(|cand| cand.is_file())
}

/// Pull a human-readable message out of a JSON-RPC `error` body
/// (`{"code","message"}`), falling back to a bare string error.
fn err_message(err: &serde_json::Value) -> String {
    err.get("message")
        .and_then(|m| m.as_str())
        .or_else(|| err.as_str())
        .unwrap_or("unknown daemon error")
        .to_string()
}

/// One line-delimited JSON-RPC round trip: connect, write
/// `{"id","method","params"}\n`, read one reply line, parse it. Mirrors ptyd
/// `wire::round_trip_timeout`.
fn round_trip(
    socket: &str,
    method: &str,
    params: &serde_json::Value,
    timeout: Duration,
) -> std::io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let req = json!({ "id": "rally-cli", "method": method, "params": params });
    let mut line = serde_json::to_string(&req).map_err(|e| std::io::Error::other(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    if resp.trim().is_empty() {
        return Err(std::io::Error::other("empty daemon reply"));
    }
    serde_json::from_str(resp.trim())
        .map_err(|e| std::io::Error::other(format!("bad reply json: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// A test-double daemon: accepts ONE connection, reads the JSON-RPC request
    /// line, and replies with `canned`. Returns the parsed request for asserts.
    fn spawn_double(
        socket: String,
        canned: serde_json::Value,
    ) -> thread::JoinHandle<serde_json::Value> {
        let listener = UnixListener::bind(&socket).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut req_line = String::new();
            reader.read_line(&mut req_line).unwrap();
            let mut w = stream;
            let mut out = serde_json::to_string(&canned).unwrap();
            out.push('\n');
            w.write_all(out.as_bytes()).unwrap();
            w.flush().unwrap();
            serde_json::from_str(req_line.trim()).unwrap()
        })
    }

    fn temp_sock(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "rally-daemon-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Unix socket paths are length-limited; keep it short.
        format!("{}.sock", dir.display())
    }

    #[test]
    fn resolve_unambiguous_socket_requires_exactly_one() {
        assert_eq!(resolve_unambiguous_socket(&[]), None);
        assert_eq!(
            resolve_unambiguous_socket(&["a.sock".to_string()]),
            Some("a.sock".to_string())
        );
        // Ambiguous (>1) → None: never guess which daemon to bind.
        assert_eq!(
            resolve_unambiguous_socket(&["a.sock".to_string(), "b.sock".to_string()]),
            None
        );
    }

    #[test]
    fn register_agent_against_test_double_returns_registered() {
        let sock = temp_sock("ok");
        let canned = json!({
            "id": "rally-cli",
            "result": { "pane_id": "pane-7", "identity": "claude_code:01",
                        "transport_resolved": "pty", "rebound": false }
        });
        let handle = spawn_double(sock.clone(), canned);

        let outcome = register_agent(&sock, "claude_code:01", "pane-7");
        assert_eq!(
            outcome,
            RegisterOutcome::Registered {
                pane_id: "pane-7".to_string()
            }
        );

        // The request the CLI sent must be a well-formed agent.register call.
        let req = handle.join().unwrap();
        assert_eq!(req["method"], "agent.register");
        assert_eq!(req["params"]["identity"], "claude_code:01");
        assert_eq!(req["params"]["pane"], "pane-7");
        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn register_agent_maps_daemon_error_to_unavailable() {
        let sock = temp_sock("err");
        let canned = json!({
            "id": "rally-cli",
            "error": { "code": "pane_not_found", "message": "no such pane" }
        });
        let handle = spawn_double(sock.clone(), canned);

        let outcome = register_agent(&sock, "claude_code:01", "ghost-pane");
        match outcome {
            RegisterOutcome::Unavailable { reason } => {
                assert!(reason.contains("no such pane"), "got: {reason}");
            }
            other => panic!("daemon error must map to Unavailable, got {other:?}"),
        }
        handle.join().unwrap();
        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn register_agent_missing_socket_is_unavailable_not_error() {
        let outcome = register_agent("/nonexistent/ptyd.sock", "x:01", "p");
        assert!(matches!(outcome, RegisterOutcome::Unavailable { .. }));
    }

    #[test]
    fn register_agent_rejects_identity_echo_mismatch() {
        // security-reviewer LOW/MED: a daemon that bound a DIFFERENT identity
        // than requested must NOT be honored — fall back to the framed path.
        let sock = temp_sock("idmismatch");
        let canned = json!({
            "id": "rally-cli",
            "result": { "pane_id": "pane-9", "identity": "evil_agent:99",
                        "transport_resolved": "pty", "rebound": false }
        });
        let handle = spawn_double(sock.clone(), canned);

        let outcome = register_agent(&sock, "claude_code:01", "pane-9");
        match outcome {
            RegisterOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("evil_agent:99") && reason.contains("claude_code:01"),
                    "mismatch reason must name both identities; got: {reason}"
                );
            }
            other => panic!("identity-echo mismatch must map to Unavailable, got {other:?}"),
        }
        handle.join().unwrap();
        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn register_agent_trusts_absent_identity_echo() {
        // Older daemons may omit the identity echo; absent is trusted (pane
        // handle only). Matching echo is the common modern case.
        let sock = temp_sock("idabsent");
        let canned = json!({
            "id": "rally-cli",
            "result": { "pane_id": "pane-3", "transport_resolved": "pty", "rebound": false }
        });
        let handle = spawn_double(sock.clone(), canned);

        let outcome = register_agent(&sock, "claude_code:01", "pane-3");
        assert_eq!(
            outcome,
            RegisterOutcome::Registered {
                pane_id: "pane-3".to_string()
            }
        );
        handle.join().unwrap();
        std::fs::remove_file(&sock).ok();
    }
}
