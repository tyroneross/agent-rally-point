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
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const RALLY_BIN: &str = env!("CARGO_BIN_EXE_rally");

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// A test-double rally-termd: a unix socket that answers `agent.register` with
/// a `Registered` result. Accepts connections until the listener is dropped.
/// Returns the socket path; the listener thread detaches.
fn spawn_double_daemon(socket: PathBuf) {
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
            let mut w = stream;
            let reply = if req["method"] == "agent.register" {
                serde_json::json!({
                    "id": req["id"],
                    "result": {
                        "pane_id": "double-pane-1",
                        "identity": req["params"]["identity"],
                        "transport_resolved": "pty",
                        "rebound": false
                    }
                })
            } else {
                serde_json::json!({
                    "id": req["id"],
                    "error": { "code": "unknown_method", "message": "test double" }
                })
            };
            let mut out = serde_json::to_string(&reply).unwrap();
            out.push('\n');
            let _ = w.write_all(out.as_bytes());
            let _ = w.flush();
        }
    });
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
}

impl Sandbox {
    fn new() -> Self {
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
        spawn_double_daemon(socket.clone());
        Self {
            cwd,
            home,
            socket,
            tmux_spy,
            tmux_log,
        }
    }

    fn rally(&self, args: &[&str]) -> Value {
        let out = Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("PTYD_SOCKET_PATH", &self.socket)
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
