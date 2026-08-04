// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! Daemon-first inject routing (move 2) — LIVE binary, against a TEST-DOUBLE
//! daemon honoring the `agent.register` + Receipt contract.
//!
//! These tests are the anti-dormancy gate: they prove the daemon-routed arm is
//! REACHABLE end-to-end (the feature is not machinery that never fires) and
//! that acceptance criterion 1 holds — when a session is daemon-registered,
//! `rally inject` writes ONLY the ledger Directive and fires ZERO tmux
//! `send-keys` (asserted via a `--tmux-bin` spy that records every invocation).
//!
//! The real `rally-termd`/ptyd daemon is out of scope for the CLI side; the
//! live-daemon flip (ptyd owning the launched pane) is the documented remaining
//! step. Here a minimal in-test double answers `agent.register` so the CLI's
//! registration → routing → no-keystroke path is exercised against a real
//! socket, not a stub.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const RALLY_BIN: &str = env!("CARGO_BIN_EXE_rally");
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}-{}-{}-{counter}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Behavioral knobs for the test-double daemon so individual tests can drive
/// the register-fail (F2) and pane-mismatch (F4) branches.
#[derive(Clone, Default)]
struct DoubleConfig {
    /// Pane id the double assigns on `agent.start` and reports on `pane.list`.
    pane_id: String,
    /// When true, `agent.register` replies with an `identity_conflict` error
    /// (drives the F2 register-fail → tmux fallback path).
    register_conflict: bool,
    /// When set, `agent.send` returns a Receipt whose `pane_id` is THIS value
    /// instead of `pane_id` (drives the F4 daemon_pane_mismatch path).
    send_pane_override: Option<String>,
}

/// Shared, append-only log of every JSON-RPC request the double received, so a
/// test can assert which verbs fired (and with what params).
type RequestLog = Arc<Mutex<Vec<Value>>>;

/// A stateful test-double rally ptyd daemon. Learns `workspace.list`,
/// `workspace.create`, `agent.start`, `agent.register`, `agent.send`,
/// `agent.stop`, `pane.close`, and `pane.list` (the verbs the ptyd
/// pane-ownership flip drives), and records EVERY request to `log`. Accepts
/// connections until the listener is dropped.
fn spawn_double_daemon(socket: PathBuf, cfg: DoubleConfig) -> RequestLog {
    let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let log_thread = Arc::clone(&log);
    // [C]: track created workspaces (label → id) so `workspace.list` reflects
    // them and the CLI's list-then-reuse path can be exercised faithfully.
    let workspaces: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(&socket).expect("bind double daemon socket");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let req: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            log_thread.lock().unwrap().push(req.clone());
            let id = req["id"].clone();
            let pane = cfg.pane_id.clone();
            let reply = match req["method"].as_str() {
                // [C]: list reflects every workspace created so far, so the CLI's
                // list-then-reuse path returns an existing id on the 2nd+ run.
                Some("workspace.list") => {
                    let ws = workspaces.lock().unwrap();
                    let entries: Vec<Value> = ws
                        .iter()
                        .map(|(label, wsid)| {
                            serde_json::json!({
                                "workspace_id": wsid, "label": label, "number": 1,
                                "focused": false, "tab_count": 1, "pane_count": 1,
                                "agent_status": "idle", "active_tab_id": "rally-tab-1"
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "id": id,
                        "result": { "type": "workspace_list", "workspaces": entries }
                    })
                }
                Some("workspace.create") => {
                    let label = req["params"]["label"]
                        .as_str()
                        .unwrap_or("rally")
                        .to_string();
                    let mut ws = workspaces.lock().unwrap();
                    let wsid = format!("rally-ws-{}", ws.len() + 1);
                    ws.push((label.clone(), wsid.clone()));
                    serde_json::json!({
                        "id": id,
                        "result": {
                            "type": "workspace_created",
                            "workspace": { "workspace_id": wsid, "label": label,
                                "number": 1, "focused": false, "tab_count": 1, "pane_count": 1,
                                "agent_status": "idle", "active_tab_id": "rally-tab-1" },
                            "tab": { "tab_id": "rally-tab-1", "workspace_id": wsid,
                                "label": "1", "number": 1, "focused": false, "pane_count": 1,
                                "agent_status": "idle" },
                            "root_pane": pane_info(&pane)
                        }
                    })
                }
                Some("agent.start") => serde_json::json!({
                    "id": id,
                    "result": { "type": "agent_started", "pane": pane_info(&pane) }
                }),
                Some("agent.register") => {
                    if cfg.register_conflict {
                        serde_json::json!({
                            "id": id,
                            "error": { "code": "identity_conflict",
                                "message": "identity already bound to another live pane" }
                        })
                    } else {
                        serde_json::json!({
                            "id": id,
                            "result": {
                                "type": "registered",
                                "pane_id": pane,
                                "identity": req["params"]["identity"],
                                "transport_resolved": "pty",
                                "rebound": false
                            }
                        })
                    }
                }
                Some("agent.send") => {
                    let receipt_pane = cfg
                        .send_pane_override
                        .clone()
                        .unwrap_or_else(|| pane.clone());
                    let params = &req["params"];
                    // Mirror ptyd's legacy-vs-non-legacy split (ptyd
                    // src/main.rs:670-674): a call is LEGACY iff only name/text
                    // were supplied — ANY of to/submit/confirm/confirm_timeout_ms/
                    // paste_frame opts into the framing path.
                    let non_legacy = params.get("to").is_some()
                        || params.get("submit").is_some()
                        || params.get("confirm").is_some()
                        || params.get("confirm_timeout_ms").is_some()
                        || params.get("paste_frame").is_some();
                    // FAITHFUL submission semantics (ptyd src/main.rs:724-728 +
                    // src/comms.rs:48-50): on the non-legacy path `submit`
                    // defaults to FALSE; only an explicit `submit:true` appends
                    // the CR that actually submits. A non-legacy send WITHOUT
                    // submit:true pastes-without-submitting — exactly the L5
                    // inject-no-submit bug. The legacy path writes verbatim (no
                    // implicit newline) and reports "sent".
                    let submit = params
                        .get("submit")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let submitted = if non_legacy { submit } else { false };
                    // confirm ceiling → receipt state (ptyd src/comms.rs:195-202,
                    // src/main.rs:729-731). Absent on the non-legacy path defaults
                    // to "seen"; legacy is always "sent".
                    let confirm = params
                        .get("confirm")
                        .and_then(Value::as_str)
                        .unwrap_or(if non_legacy { "seen" } else { "sent" });
                    let state = match confirm {
                        "none" | "sent" => "sent",
                        "acted" => "acted",
                        _ => "seen",
                    };
                    serde_json::json!({
                        "id": id,
                        "result": {
                            "type": "receipt",
                            "to": params["to"],
                            "pane_id": receipt_pane,
                            "transport": "keystroke",
                            "state": state,
                            // Record the framing decision the double actually
                            // observed so a test can assert the CLI asked for a
                            // SUBMITTED, sent-confirmed delivery (not a silent
                            // paste). `submitted` is the bug-catching signal.
                            "evidence": {
                                "bytes_written": 0,
                                "echo_matched": false,
                                "submitted": submitted,
                                "submit_requested": submit,
                                "confirm": confirm,
                                "legacy": !non_legacy
                            }
                        }
                    })
                }
                Some("agent.stop") => serde_json::json!({
                    "id": id, "result": { "type": "ok" }
                }),
                // [G]: reap-by-id verb (ptyd main.rs:567).
                Some("pane.close") => serde_json::json!({
                    "id": id, "result": { "type": "ok" }
                }),
                Some("pane.list") => serde_json::json!({
                    "id": id,
                    "result": { "type": "pane_list", "panes": [ pane_info(&pane) ] }
                }),
                _ => serde_json::json!({
                    "id": id,
                    "error": { "code": "unknown_method", "message": "test double" }
                }),
            };
            let mut w = stream;
            let mut out = serde_json::to_string(&reply).unwrap();
            out.push('\n');
            let _ = w.write_all(out.as_bytes());
            let _ = w.flush();
        }
    });
    log
}

/// Minimal `PaneInfo` shape (ptyd `protocol.rs:345`) — only the fields the CLI
/// reads (`pane_id`) need to be meaningful; the rest satisfy the decoder.
fn pane_info(pane_id: &str) -> Value {
    serde_json::json!({
        "pane_id": pane_id,
        "terminal_id": pane_id,
        "workspace_id": "rally-ws-1",
        "tab_id": "rally-tab-1",
        "focused": false,
        "liveness": "live",
        "agent_status": "idle",
        "created_at_ms": 0,
        "last_activity_at_ms": 0,
        "revision": 1
    })
}

/// Count how many logged requests used `method`.
fn count_method(log: &RequestLog, method: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|r| r["method"] == method)
        .count()
}

/// Create a spy `tmux` shell script at `path` that appends each invocation
/// (all argv) to `log`. Any `send-keys` call is therefore recorded for the
/// zero-keystroke assertion. The spy exits 0 so the backend "succeeds".
fn write_tmux_spy(path: &Path, log: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
        log.display()
    );
    fs::write(path, script).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

struct Sandbox {
    cwd: PathBuf,
    home: PathBuf,
    socket: PathBuf,
    tmux_spy: PathBuf,
    tmux_log: PathBuf,
    log: RequestLog,
}

impl Sandbox {
    fn new() -> Self {
        Self::with_config(DoubleConfig {
            pane_id: "double-pane-1".to_string(),
            ..Default::default()
        })
    }

    fn with_config(cfg: DoubleConfig) -> Self {
        let root = std::env::temp_dir().join(unique("daemon-inject"));
        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        // Keep the socket path short (unix sun_path limit ~104 bytes).
        let socket = std::env::temp_dir().join(format!("{}.sock", unique("dtd")));
        let tmux_spy = root.join("tmux-spy.sh");
        let tmux_log = root.join("tmux-invocations.log");
        write_tmux_spy(&tmux_spy, &tmux_log);
        let log = spawn_double_daemon(socket.clone(), cfg);
        Self {
            cwd,
            home,
            socket,
            tmux_spy,
            tmux_log,
            log,
        }
    }

    /// Run rally with the double wired BOTH as the legacy registration socket
    /// (`PTYD_SOCKET_PATH`, used by detect_host_runtime) AND as the RALLY-OWNED
    /// ptyd socket (`RALLY_PTYD_SOCKET`, used by the spawn/inject path). All
    /// paths are temp + hermetic; the real `~/.local/share/rally` and the Easy
    /// Terminal socket are NEVER touched.
    fn rally(&self, args: &[&str]) -> Value {
        let out = Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("PTYD_SOCKET_PATH", &self.socket)
            .env("RALLY_PTYD_SOCKET", &self.socket)
            .env_remove("PWD")
            .output()
            .expect("spawn rally");
        assert!(
            out.status.success(),
            "rally {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn tmux_send_keys_count(&self) -> usize {
        match fs::read_to_string(&self.tmux_log) {
            Ok(s) => s.lines().filter(|l| l.contains("send-keys")).count(),
            Err(_) => 0, // no log written = the spy was never called at all
        }
    }

    /// Whether the tmux spy ever ran a `new-session` (proves a tmux LAUNCH, the
    /// F2 fallback evidence).
    fn tmux_new_session_count(&self) -> usize {
        match fs::read_to_string(&self.tmux_log) {
            Ok(s) => s.lines().filter(|l| l.contains("new-session")).count(),
            Err(_) => 0,
        }
    }

    /// Read the room ledger as a flat list of FACT JSON values. Each ledger
    /// line is a `LedgerLine` envelope (`{seq, event_type, payload, ..}`); the
    /// fact lives under `payload`, so we unwrap it.
    fn ledger_facts(&self) -> Vec<Value> {
        let log_dir = self.cwd.join(".rally").join("log");
        let mut facts = Vec::new();
        if let Ok(entries) = fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && let Ok(body) = fs::read_to_string(entry.path())
                {
                    for line in body.lines().filter(|l| !l.trim().is_empty()) {
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            // Unwrap the LedgerLine envelope to the fact.
                            let fact = v.get("payload").cloned().unwrap_or(v);
                            facts.push(fact);
                        }
                    }
                }
            }
        }
        facts
    }
}

/// Acceptance criterion 1 + anti-dormancy: a daemon-registered session's inject
/// writes the ledger Directive and fires ZERO tmux send-keys; the envelope
/// labels `delivery_path: "daemon"`.
#[test]
fn daemon_registered_session_injects_ledger_only_zero_send_keys() {
    let sb = Sandbox::new();

    // `rally run` registers the session with the (double) daemon via the
    // PTYD_SOCKET_PATH we set; --tmux-bin points the backend at the spy.
    let run = sb.rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "daemon-target",
        "--shared",
        "--backend",
        "tmux",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let session = &run["data"]["run"]["session"];
    assert_eq!(
        session["daemon_registered"], true,
        "run against a reachable daemon must register the session; session={session}"
    );
    assert_eq!(session["daemon_pane"], "double-pane-1");
    let target = session["name"].as_str().unwrap().to_string();

    // Inject text into the daemon-registered session.
    let inject = sb.rally(&[
        "inject",
        &target,
        "--json",
        "--text",
        "hello via daemon",
        "--tool",
        "claude_code:01",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let data = &inject["data"]["inject"];

    // Criterion 1a: the envelope labels the daemon path.
    assert_eq!(
        data["delivery_path"], "daemon",
        "daemon-registered inject must report delivery_path=daemon; got {data}"
    );
    // Criterion 1b: a ledger Directive WAS written (delivery is not lost).
    assert!(
        data["directive_seq"].is_u64(),
        "daemon inject must still write the ledger Directive; got {data}"
    );
    // Criterion 1c (THE anti-dormancy assertion): ZERO send-keys fired.
    assert_eq!(
        sb.tmux_send_keys_count(),
        0,
        "a daemon-routed inject MUST fire zero tmux send-keys; log had {} send-keys",
        sb.tmux_send_keys_count()
    );
}

/// Fallback contract (criterion 3): with NO daemon reachable, the same inject
/// falls back to the framed tmux write and reports
/// `delivery_path: "tmux_framed_fallback"` (and DOES use send-keys).
#[test]
fn no_daemon_session_falls_back_to_framed_tmux() {
    // A sandbox whose PTYD_SOCKET_PATH points at a non-existent socket.
    let root = std::env::temp_dir().join(unique("no-daemon-inject"));
    let cwd = root.join("cwd");
    let home = root.join("home");
    fs::create_dir_all(cwd.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let tmux_spy = root.join("tmux-spy.sh");
    let tmux_log = root.join("tmux.log");
    write_tmux_spy(&tmux_spy, &tmux_log);
    let dead_socket = root.join("nonexistent.sock");

    let rally = |args: &[&str]| -> Value {
        let out = Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("PTYD_SOCKET_PATH", &dead_socket)
            .env_remove("PWD")
            .output()
            .expect("spawn rally");
        assert!(
            out.status.success(),
            "rally {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let run = rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "fallback-target",
        "--shared",
        "--backend",
        "tmux",
        "--tmux-bin",
        tmux_spy.to_str().unwrap(),
    ]);
    let session = &run["data"]["run"]["session"];
    // No daemon reachable → not registered (fail-open, no error).
    assert_ne!(
        session["daemon_registered"], true,
        "no daemon → session must NOT be daemon_registered"
    );
    let target = session["name"].as_str().unwrap().to_string();

    let inject = rally(&[
        "inject",
        &target,
        "--json",
        "--text",
        "hello via tmux",
        "--tool",
        "claude_code:01",
        "--tmux-bin",
        tmux_spy.to_str().unwrap(),
    ]);
    assert_eq!(
        inject["data"]["inject"]["delivery_path"], "tmux_framed_fallback",
        "no-daemon inject must report the framed-tmux fallback path"
    );
}

// ===========================================================================
// ptyd pane-ownership flip — `rally run --backend ptyd` (the live spawn path).
// ===========================================================================

/// (a) `rally run --backend ptyd` against a live double daemon: the session is
/// daemon_registered, its target IS the double's pane id, and the envelope
/// reports backend `ptyd`. The double records `workspace.create`, `agent.start`
/// (focus:false), and `agent.register`.
#[test]
fn run_backend_ptyd_spawns_daemon_owned_pane() {
    let sb = Sandbox::with_config(DoubleConfig {
        pane_id: "ptyd-pane-7".to_string(),
        ..Default::default()
    });

    let run = sb.rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "ptyd-target",
        "--shared",
        "--backend",
        "ptyd",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let session = &run["data"]["run"]["session"];
    assert_eq!(
        session["backend"], "ptyd",
        "explicit --backend ptyd must record backend=ptyd; session={session}"
    );
    assert_eq!(
        session["daemon_registered"], true,
        "ptyd run must register the spawned pane; session={session}"
    );
    assert_eq!(session["daemon_pane"], "ptyd-pane-7");
    // The session TARGET is the daemon pane id (not a tmux session name).
    assert_eq!(session["target"], "ptyd-pane-7");

    // The double saw the spawn sequence and NO tmux launch.
    assert_eq!(count_method(&sb.log, "workspace.create"), 1);
    assert_eq!(count_method(&sb.log, "agent.start"), 1);
    assert_eq!(count_method(&sb.log, "agent.register"), 1);
    // agent.start must request focus:false (design-1: never the focused tab).
    let start = sb
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "agent.start")
        .cloned()
        .unwrap();
    assert_eq!(start["params"]["focus"], false);
    assert_eq!(
        sb.tmux_new_session_count(),
        0,
        "a ptyd spawn must NOT launch a tmux session"
    );
}

/// (b) inject to a ptyd-spawned session: delivery_path "daemon", ZERO send-keys,
/// the double received the SANITIZED text (raw \r and \x1b stripped), a Receipt
/// fact is present in the ledger, and the daemon receipt state is surfaced.
#[test]
fn inject_to_ptyd_session_is_daemon_only_sanitized_with_receipt() {
    let sb = Sandbox::with_config(DoubleConfig {
        pane_id: "ptyd-pane-7".to_string(),
        ..Default::default()
    });

    let run = sb.rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "ptyd-inj",
        "--shared",
        "--backend",
        "ptyd",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let target = run["data"]["run"]["session"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    // Send text carrying raw control bytes: a CR and an ESC sequence that, if
    // unsanitized, would be a paste-breakout. sanitize_inject_text drops every
    // C0 control + ESC, keeping printable chars + tab.
    let payload = "line1\rline2\x1b[201~evil";
    let inject = sb.rally(&[
        "inject",
        &target,
        "--json",
        "--text",
        payload,
        "--tool",
        "claude_code:01",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let data = &inject["data"]["inject"];

    assert_eq!(data["delivery_path"], "daemon", "got {data}");
    assert!(
        data["directive_seq"].is_u64(),
        "directive must be written; {data}"
    );
    assert_eq!(
        data["daemon_receipt_state"], "sent",
        "the daemon Receipt state must be surfaced; {data}"
    );
    // ZERO tmux send-keys — the pane is daemon-owned.
    assert_eq!(sb.tmux_send_keys_count(), 0);

    // The double received the SANITIZED text: no \r, no \x1b, no paste-end.
    let send = sb
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "agent.send")
        .cloned()
        .expect("double must have received an agent.send");

    // ROOT-CAUSE GUARD ([A]/[B]): the CLI MUST ask ptyd to SUBMIT the line
    // (append the CR), not paste-without-submitting. A bare `{to,text}` send
    // would leave submit=false and the agent would never receive the directive
    // (the L5 inject-no-submit failure). This is the assertion that fails if the
    // send_agent fix regresses.
    assert_eq!(
        send["params"]["submit"], true,
        "agent.send MUST request submit:true (append the submitting CR), else the \
         agent never submits the pasted directive; params={}",
        send["params"]
    );
    // And it must resolve on bytes-written, not echo-seen, to stay under the
    // CLI's 3s round-trip timeout (else a successful write reads as "failed").
    assert_eq!(
        send["params"]["confirm"], "sent",
        "agent.send MUST request confirm:\"sent\" so it resolves on bytes-written \
         (< 3s), not the default \"seen\" echo-wait (≤4s > the 3s read timeout); \
         params={}",
        send["params"]
    );

    let sent_text = send["params"]["text"].as_str().unwrap();
    // sanitize_inject_text drops every C0 control + ESC + CR/LF, keeping
    // printable chars + tab. So the CR is gone and the ESC that armed the
    // bracketed-paste end marker is gone (neutralizing the breakout); the
    // leftover PRINTABLE residue `[201~evil` is harmless text and may remain.
    assert!(
        !sent_text.contains('\r'),
        "CR must be stripped: {sent_text:?}"
    );
    assert!(
        !sent_text.contains('\u{1b}'),
        "ESC (the paste-end arming byte) must be stripped: {sent_text:?}"
    );
    // The full ESC-prefixed paste-end marker must NOT survive intact.
    assert!(
        !sent_text.contains("\u{1b}[201~"),
        "no functional paste-end marker may survive: {sent_text:?}"
    );
    // Printable residue survives in order (CR removed, so line1+line2 join).
    // RC-041 gap 3A prefixes every delivered payload with a provenance label,
    // so the payload no longer starts the line. Strip the label and re-assert
    // the ORIGINAL property against the body — the payload leads its own text,
    // with the neutered paste marker trailing it. Asserting `ends_with` instead
    // would have been wrong: the sanitized residue legitimately follows.
    let body = sent_text
        .split_once("] ")
        .map(|(_, rest)| rest)
        .unwrap_or(sent_text);
    assert!(body.starts_with("line1line2"), "got {sent_text:?}");

    // A Receipt fact ref'ing the directive is present in the ledger.
    let directive_seq = data["directive_seq"].as_u64().unwrap();
    let has_receipt = sb.ledger_facts().iter().any(|f| {
        f["kind"] == "receipt"
            && f["evidence"]
                .as_array()
                .map(|ev| {
                    ev.iter()
                        .any(|e| e == &format!("directive_seq:{directive_seq}"))
                })
                .unwrap_or(false)
    });
    assert!(
        has_receipt,
        "a Receipt fact ref'ing the directive seq must be posted; kinds present: {:?}",
        sb.ledger_facts()
            .iter()
            .map(|f| f["kind"].clone())
            .collect::<Vec<_>>()
    );
}

/// (c) register-failure path (F2): the double replies `identity_conflict` to
/// `agent.register`. The run must reap the spawned pane (`agent.stop`) AND fall
/// back to a tmux launch (`new-session`), with the F2 warning field set.
#[test]
fn ptyd_register_failure_reaps_pane_and_falls_back_to_tmux() {
    let sb = Sandbox::with_config(DoubleConfig {
        pane_id: "ptyd-pane-9".to_string(),
        register_conflict: true,
        ..Default::default()
    });

    let run = sb.rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "ptyd-f2",
        "--shared",
        "--backend",
        "ptyd",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let data = &run["data"]["run"];
    let session = &data["session"];

    // Fell back to tmux: backend rewritten, not daemon_registered.
    assert_eq!(
        session["backend"], "tmux",
        "F2 must fall back to tmux; {session}"
    );
    assert_ne!(session["daemon_registered"], true);
    // F2 [G]: the spawned pane was reaped BY PANE ID via pane.close (not by
    // name via agent.stop), and the closed pane is the one that was spawned.
    assert_eq!(
        count_method(&sb.log, "pane.close"),
        1,
        "the just-spawned pane must be reaped (by id) on register failure"
    );
    assert_eq!(
        count_method(&sb.log, "agent.stop"),
        0,
        "F2 reap must be by pane id (pane.close), never by name (agent.stop)"
    );
    let close = sb
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "pane.close")
        .cloned()
        .expect("pane.close must have fired");
    assert_eq!(
        close["params"]["pane_id"], "ptyd-pane-9",
        "pane.close must target the exact spawned pane id; {close}"
    );
    // F2: tmux actually launched the agent.
    assert_eq!(
        sb.tmux_new_session_count(),
        1,
        "register failure must relaunch under tmux"
    );
    // F2: the loud warning is present.
    assert!(
        data["warning"].as_str().unwrap_or("").contains("register"),
        "the run envelope must carry the F2 warning; data={data}"
    );
}

/// (d) receipt pane mismatch (F4): the double returns a DIFFERENT pane_id in the
/// send Receipt than the registered pane. The inject must report a
/// `daemon_pane_mismatch` failure and NOT be acked/delivered — no fallback.
#[test]
fn inject_receipt_pane_mismatch_is_hard_failure() {
    let sb = Sandbox::with_config(DoubleConfig {
        pane_id: "ptyd-pane-7".to_string(),
        send_pane_override: Some("ptyd-pane-OTHER".to_string()),
        ..Default::default()
    });

    let run = sb.rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "ptyd-f4",
        "--shared",
        "--backend",
        "ptyd",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let target = run["data"]["run"]["session"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    let inject = sb.rally(&[
        "inject",
        &target,
        "--json",
        "--text",
        "hello",
        "--tool",
        "claude_code:01",
        "--tmux-bin",
        sb.tmux_spy.to_str().unwrap(),
    ]);
    let data = &inject["data"]["inject"];

    assert_eq!(data["delivery_path"], "daemon");
    assert_eq!(
        data["delivery_state"], "failed",
        "an F4 pane mismatch must be a failed delivery; {data}"
    );
    assert_eq!(data["delivered"], false);
    assert!(
        data["daemon_delivery_error"]
            .as_str()
            .unwrap_or("")
            .contains("daemon_pane_mismatch"),
        "the F4 mismatch must be surfaced explicitly; {data}"
    );
    // NO fallback delivery: zero tmux send-keys.
    assert_eq!(sb.tmux_send_keys_count(), 0);
}

/// (e) `--backend auto` with no live rally socket → tmux (existing behavior
/// preserved): the session is launched under tmux, not ptyd.
#[test]
fn backend_auto_without_live_socket_uses_tmux() {
    let root = std::env::temp_dir().join(unique("auto-no-socket"));
    let cwd = root.join("cwd");
    let home = root.join("home");
    fs::create_dir_all(cwd.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let tmux_spy = root.join("tmux-spy.sh");
    let tmux_log = root.join("tmux.log");
    write_tmux_spy(&tmux_spy, &tmux_log);
    // RALLY_PTYD_SOCKET points at a non-existent socket → not live.
    let dead_socket = root.join("nonexistent.sock");

    let out = Command::new(RALLY_BIN)
        .args([
            "run",
            "claude",
            "--json",
            "--name",
            "auto-target",
            "--shared",
            "--backend",
            "auto",
            "--tmux-bin",
            tmux_spy.to_str().unwrap(),
        ])
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("RALLY_PTYD_SOCKET", &dead_socket)
        .env_remove("PTYD_SOCKET_PATH")
        .env_remove("PWD")
        .output()
        .expect("spawn rally");
    assert!(
        out.status.success(),
        "auto run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        run["data"]["run"]["session"]["backend"], "tmux",
        "auto with no live rally socket must select tmux; got {}",
        run["data"]["run"]["session"]
    );
}

/// (f) explicit `--backend ptyd`, no live socket, no RALLY_PTYD_BIN, empty PATH
/// dir → a CLEAR autostart-failure error (the real binary can't be hermetically
/// started here, so this asserts the error message path).
#[test]
fn backend_ptyd_no_socket_no_binary_errors_clearly() {
    let root = std::env::temp_dir().join(unique("ptyd-no-bin"));
    let cwd = root.join("cwd");
    let home = root.join("home");
    let empty_path = root.join("empty-path");
    fs::create_dir_all(cwd.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&empty_path).unwrap();
    let dead_socket = root.join("nonexistent.sock");

    let out = Command::new(RALLY_BIN)
        .args([
            "run",
            "claude",
            "--json",
            "--name",
            "no-bin",
            "--shared",
            "--backend",
            "ptyd",
        ])
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("RALLY_PTYD_SOCKET", &dead_socket)
        // Empty PATH dir → `ptyd` is not resolvable; no RALLY_PTYD_BIN set.
        .env("PATH", &empty_path)
        .env_remove("RALLY_PTYD_BIN")
        .env_remove("PTYD_SOCKET_PATH")
        .env_remove("PWD")
        .output()
        .expect("spawn rally");
    assert!(
        !out.status.success(),
        "ptyd run with no socket + no binary must FAIL, not succeed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ptyd") && (stderr.contains("not live") || stderr.contains("binary")),
        "error must clearly explain the missing rally ptyd daemon/binary; stderr: {stderr}"
    );
}

// ===========================================================================
// REAL-BINARY CONTRACT TEST — the oracle the test-double cannot be.
//
// This is the test that would have caught [A]/[B] (paste-without-submit). It
// drives the ACTUAL installed `ptyd` daemon (NOT a double), so it exercises
// ptyd's real `agent.send` submit/confirm semantics end-to-end:
//   `rally run --backend ptyd` autostarts a dedicated ptyd on a TEMP socket +
//   state dir → `rally inject` delivers a line → we read the pane scrollback
//   and assert the SUBMITTED line was actually RECEIVED by the child program
//   (not merely "bytes written"). A bare `{to,text}` send would leave submit
//   false and the child would never see a completed line.
//
// SKIPs cleanly (passes, prints why) when no real ptyd is available, so CI
// without ptyd stays green. NEVER touches the EasyTerminal socket or
// ~/.local/share/rally — a temp HOME + temp socket isolate it completely.
// ===========================================================================

/// Resolve a real ptyd binary: `$RALLY_PTYD_BIN`, else `ptyd` on PATH, else
/// the conventional install path. `None` → the contract test self-skips.
fn real_ptyd_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RALLY_PTYD_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("ptyd");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let home = std::env::var("HOME").ok()?;
    let conventional = PathBuf::from(home).join(".local/bin/ptyd");
    conventional.is_file().then_some(conventional)
}

/// Write a fake `claude` shim that, instead of an LLM TUI, runs a line reader:
/// it prints `GOT:<line>` for every COMPLETE (newline-terminated) line it reads
/// from its PTY, with terminal echo DISABLED so the only way a line appears is
/// if it was actually SUBMITTED (CR appended). A paste-without-submit leaves the
/// line buffered and unread → `GOT:` never appears. This is the bug oracle.
fn write_claude_line_reader_shim(path: &Path) {
    // `stty -echo` removes terminal echo so we don't get a false positive from
    // the PTY echoing keystrokes; `read -r` only returns on a full line.
    let script = "#!/bin/sh\nstty -echo 2>/dev/null\nwhile IFS= read -r line; do\n  printf 'GOT:%s\\n' \"$line\"\ndone\n";
    fs::write(path, script).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Kill the process holding `socket` (the autostarted ptyd daemon), then remove
/// the socket file. `lsof -t` returns the PID(s) bound to the unix socket; only
/// OUR temp socket is matched, so the ET / vendor daemons are never touched.
fn kill_socket_holder(socket: &Path) {
    if let Ok(out) = Command::new("lsof").arg("-t").arg(socket).output() {
        for pid in String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse::<i32>().ok())
        {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
    }
    let _ = fs::remove_file(socket);
}

#[test]
fn real_ptyd_inject_actually_submits_and_is_received() {
    let Some(ptyd_bin) = real_ptyd_binary() else {
        eprintln!(
            "SKIP real_ptyd_inject_actually_submits_and_is_received: no ptyd binary \
             (set RALLY_PTYD_BIN or install ptyd on PATH / ~/.local/bin/ptyd)"
        );
        return;
    };

    let root = std::env::temp_dir().join(unique("real-ptyd"));
    let cwd = root.join("cwd");
    let home = root.join("home");
    let bin_dir = root.join("bin");
    fs::create_dir_all(cwd.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    // Fake `claude` = a line reader echoing GOT:<line> per submitted line.
    write_claude_line_reader_shim(&bin_dir.join("claude"));
    // A single rally-owned socket (NOT the ET socket, NOT ~/.local/share/rally).
    let socket = std::env::temp_dir().join(format!("{}.sock", unique("rptd")));
    // PATH the child shells use must find the fake `claude` plus /bin, /usr/bin
    // (for sh/stty/printf). Put bin_dir FIRST so our shim shadows any real claude.
    let path_env = format!("{}:/usr/bin:/bin", bin_dir.display());

    // Helper: run a rally subcommand with the real ptyd autostarted on our temp
    // socket. RALLY_PTYD_BIN points at the real daemon; RALLY_PTYD_SOCKET is the
    // SINGLE rally socket (so [E] is real — registration + send hit the same one,
    // not a re-resolved second socket).
    let rally = |args: &[&str]| -> std::process::Output {
        Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("PATH", &path_env)
            .env("RALLY_PTYD_BIN", &ptyd_bin)
            .env("RALLY_PTYD_SOCKET", &socket)
            .env_remove("PTYD_SOCKET_PATH")
            .env_remove("PWD")
            .output()
            .expect("spawn rally")
    };

    // 1. `rally run --backend ptyd` autostarts the real daemon + spawns the pane.
    let run_out = rally(&[
        "run",
        "claude",
        "--json",
        "--name",
        "real-ptyd-target",
        "--shared",
        "--backend",
        "ptyd",
    ]);
    assert!(
        run_out.status.success(),
        "real ptyd run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr),
    );
    let run: Value = serde_json::from_slice(&run_out.stdout).unwrap();
    let session = &run["data"]["run"]["session"];
    assert_eq!(
        session["backend"], "ptyd",
        "real ptyd run must record backend=ptyd; {session}"
    );
    assert_eq!(
        session["daemon_registered"], true,
        "real ptyd run must register the spawned pane; {session}"
    );
    // [E]: the exact socket is pinned on the session.
    assert_eq!(
        session["daemon_socket"],
        socket.to_str().unwrap(),
        "the session must pin the rally-owned socket it spawned on; {session}"
    );
    let target = session["name"].as_str().unwrap().to_string();

    // Give the child shell a moment to exec + drop into its read loop.
    std::thread::sleep(std::time::Duration::from_millis(400));

    // 2. Inject a unique token. If the line is SUBMITTED (CR appended), the
    //    child's `read` completes and prints `GOT:<token>`; if it is merely
    //    pasted (the [A] bug), nothing is printed.
    let token = format!("CONTRACT_{}", std::process::id());
    let inject_out = rally(&[
        "inject",
        &target,
        "--json",
        "--text",
        &token,
        "--tool",
        "claude_code:01",
    ]);
    assert!(
        inject_out.status.success(),
        "real ptyd inject failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&inject_out.stdout),
        String::from_utf8_lossy(&inject_out.stderr),
    );
    let inject: Value = serde_json::from_slice(&inject_out.stdout).unwrap();
    let data = &inject["data"]["inject"];
    assert_eq!(data["delivery_path"], "daemon", "{data}");
    assert_eq!(
        data["daemon_receipt_state"], "sent",
        "confirm:\"sent\" must yield a 'sent' receipt within the 3s timeout; {data}"
    );

    // 3. THE ORACLE: read the pane scrollback and assert the child RECEIVED +
    //    processed the SUBMITTED line. Poll up to ~3s for the GOT:<token> line.
    let mut scrollback = String::new();
    let mut received = false;
    for _ in 0..30 {
        let cap = rally(&["capture", &target, "--json", "--lines", "50"]);
        if cap.status.success()
            && let Ok(v) = serde_json::from_slice::<Value>(&cap.stdout)
        {
            scrollback = v["data"]["capture"]["output"]
                .as_str()
                .unwrap_or("")
                .to_string();
            // Split rather than concatenated: RC-041 gap 3A puts the
            // provenance label between the child's `GOT:` echo and the token.
            // Against a real ptyd daemon the pane showed
            // `GOT:[rally: UNVERIFIED SENDER claude_code:01] CONTRACT_84016`,
            // i.e. the labelled line submitted and was read as one line — the
            // delivery this test grades succeeded, only the substring shape moved.
            if scrollback.contains("GOT:") && scrollback.contains(&token) {
                received = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Best-effort cleanup BEFORE asserting (so a failure still tears down):
    // reap the pane, then kill the autostarted daemon PROCESS it spawned. The
    // daemon is detached (setsid), so we identify it by the holder of our temp
    // socket (`lsof -t <socket>`) and never leak a ptyd across test runs. The ET
    // / vendor ptyds use OTHER sockets and are untouched.
    let _ = rally(&["stop", &target, "--json"]);
    kill_socket_holder(&socket);

    assert!(
        received,
        "the SUBMITTED line must be RECEIVED by the agent (child printed \
         'GOT:{token}'). A bare {{to,text}} send pastes-without-submitting and \
         the child never completes the line. Scrollback was:\n{scrollback}"
    );
}
