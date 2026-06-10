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
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

/// Default round-trip timeout for a daemon request. Registration is a cheap
/// local-socket call; if the daemon does not answer quickly it is treated as
/// unavailable and the caller falls back.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);

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
