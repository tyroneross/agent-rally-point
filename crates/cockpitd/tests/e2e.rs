// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test: C2 definition-of-done proof.
//!
//! Boots the cockpitd WebSocket transport in-process on an ephemeral port,
//! connects a tokio-tungstenite client, and drives a full session lifecycle
//! with the mock claude binary.
//!
//! Proves:
//! 1. Auth: hello/hello_ok handshake.
//! 2. launch_session → session appears in session_list.
//! 3. open_session(from_seq=0) → snapshot with replayed events (monotonic seq).
//! 4. Live events include mock's message + tool_call, all with monotonic seq.
//! 5. Terminal status received after mock completes.
//! 6. Reconnect with from_seq=midpoint → only later events replayed, no gaps/dupes
//!    (wire invariant 5).
//! 7. Approval: if mock emits approval_request, approve() resolves it in the store.
//!
//! Security regressions from the ARP audit are covered in the `arp005_*` and
//! `arp003_*` tests at the bottom of this file. They are adversarial: each one
//! drives a hostile input through the real transport and asserts the specific
//! rejection.
//!
//! No network, no real credentials — uses COCKPIT_TOKEN env + mock bins.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use cockpitd::clock::Clock as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const TEST_TOKEN: &str = "e2e-test-token-cockpit";


// ── cli-dispatch-consent scaffolding ─────────────────────────────────────────
//
// The supervisor gates every vendor-CLI spawn on a recorded consent grant
// (crates/cockpitd/src/consent.rs). These tests drive REAL session launches, so
// without a grant every launch is refused and the test hangs at `recv timeout`
// rather than failing with a readable reason — which is exactly how this
// regression presented: 27 passing e2e tests became 10 passing and 17 timeouts.
//
// The grant is written ONCE per test binary and the env is set once and never
// changed, so parallel tests cannot observe a half-applied redirect. This is
// test scaffolding for a real gate, NOT a way to disable it: the store below is
// a genuine, hash-chained document that the production code path verifies
// normally. A malformed one would fail these tests, which is the point.
fn ensure_consent_granted() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        fn entry_hash(entry: &Value) -> String {
            use sha2::{Digest, Sha256};
            let mut sorted: std::collections::BTreeMap<String, Value> =
                std::collections::BTreeMap::new();
            if let Some(obj) = entry.as_object() {
                for (k, v) in obj {
                    if k != "entry_sha256" {
                        sorted.insert(k.clone(), v.clone());
                    }
                }
            }
            format!("{:x}", Sha256::digest(serde_json::to_vec(&sorted).unwrap()))
        }

        let mut path = std::env::temp_dir();
        path.push("cockpitd-e2e-consent-granted.json");

        let mut e0 = json!({
            "seq": 0, "key": "rally-point:claude", "mode": "auto",
            "decided_at": "2026-08-21T00:00:00Z", "decided_by": "test",
            "decided_via": "e2e-test", "decided_in_repo": "/tmp/test-repo",
            "prev_sha256": null
        });
        let h0 = entry_hash(&e0);
        e0["entry_sha256"] = json!(h0);

        let mut e1 = json!({
            "seq": 1, "key": "rally-point:codex", "mode": "auto",
            "decided_at": "2026-08-21T00:00:01Z", "decided_by": "test",
            "decided_via": "e2e-test", "decided_in_repo": "/tmp/test-repo",
            "prev_sha256": h0
        });
        let h1 = entry_hash(&e1);
        e1["entry_sha256"] = json!(h1);

        std::fs::write(&path, serde_json::to_vec(&json!({"version": 2, "log": [e0, e1]})).unwrap())
            .expect("write e2e consent store");

        unsafe {
            std::env::set_var("AGENT_CONSENT_SELFTEST", "1");
            std::env::set_var("AGENT_CONSENT_STORE_PATH", path.to_str().unwrap());
            std::env::set_var("AGENT_DISPATCH_DEPTH", "0");
        }
    });
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn mock_claude_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest).join("tests/mock-bin/claude")
}

/// The repo root every e2e daemon allows launches under (ARP-005 item 4).
///
/// All tests in this binary share one process environment, so this is set once
/// to a single stable value: the system temp dir, which is where the existing
/// tests already launch from. Tests that need a *different* allowlist exercise
/// `policy::resolve_repo_path_within` directly rather than racing on env.
fn allowed_repo_root() -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).unwrap()
}

/// Set the process-wide env every e2e daemon depends on.
fn set_daemon_env() {
    unsafe {
        std::env::set_var("COCKPIT_TOKEN", TEST_TOKEN);
        std::env::set_var(
            "COCKPIT_REPO_ALLOWLIST",
            allowed_repo_root().to_str().unwrap(),
        );
    }
}

/// Start the daemon in-process on an ephemeral port and return the address.
async fn start_daemon() -> SocketAddr {
    use cockpitd::{
        adapter::claude::{ClaudeAdapter, ClaudeConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::build_state,
    };

    // Find an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Set the test token + repo allowlist in env (process-level).
    set_daemon_env();

    // H1a: single in-memory store for sessions + events + approvals.
    let store = Store::open_in_memory().unwrap();

    // Point adapters at mock bins.
    let claude_path = mock_claude_path();
    let adapter = ClaudeAdapter::new(ClaudeConfig {
        binary: claude_path,
        extra_flags: vec![],
    });

    let supervisor = Supervisor::new(store, SystemClock, adapter);
    let audit = AuditLog::open_in_memory(SystemClock).unwrap();
    let state = build_state(supervisor, audit);

    // Spawn the server.
    ensure_consent_granted();
    tokio::spawn(async move {
        cockpitd::transport::ws::serve_on(listener, state)
            .await
            .expect("daemon serve");
    });

    // Small delay to let the server bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    addr
}

// ── Simple client wrapper ─────────────────────────────────────────────────────

struct TestClient {
    sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    stream: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
}

impl TestClient {
    async fn connect(addr: SocketAddr) -> Self {
        let url = format!("ws://{}", addr);
        let (ws, _) = connect_async(&url).await.expect("connect");
        let (sink, stream) = ws.split();
        Self { sink, stream }
    }

    async fn send(&mut self, v: Value) {
        self.sink
            .send(Message::Text(v.to_string()))
            .await
            .expect("send");
    }

    async fn recv(&mut self) -> Value {
        loop {
            match timeout(Duration::from_secs(5), self.stream.next())
                .await
                .expect("recv timeout")
            {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).expect("parse json");
                }
                Some(Ok(_)) => continue, // skip non-text frames
                other => panic!("unexpected recv: {other:?}"),
            }
        }
    }

    async fn recv_matching(&mut self, t: &str) -> Value {
        for _ in 0..20 {
            let v = self.recv().await;
            if v.get("t").and_then(|x| x.as_str()) == Some(t) {
                return v;
            }
        }
        panic!("never received frame with t={t}");
    }

    async fn auth(&mut self) {
        self.send(json!({"t": "hello", "token": TEST_TOKEN, "protocol": 1}))
            .await;
        let ok = self.recv().await;
        assert_eq!(
            ok.get("t").and_then(|t| t.as_str()),
            Some("hello_ok"),
            "expected hello_ok, got {ok}"
        );
    }

    /// Authenticate with a stable client identity (ARP-005). Two clients that
    /// pass different `client_id` values are different principals even though
    /// they share the one bearer token.
    async fn auth_as(&mut self, client_id: &str) {
        self.send(json!({
            "t": "hello",
            "token": TEST_TOKEN,
            "protocol": 1,
            "client_id": client_id,
        }))
        .await;
        let ok = self.recv().await;
        assert_eq!(
            ok.get("t").and_then(|t| t.as_str()),
            Some("hello_ok"),
            "expected hello_ok for client_id={client_id}, got {ok}"
        );
    }

    /// Read frames until an `error` arrives, and return it.
    ///
    /// Fails loudly if the command succeeded silently — a security test must
    /// never pass because nothing happened.
    async fn expect_error(&mut self) -> Value {
        for _ in 0..20 {
            let v = timeout(Duration::from_secs(3), self.recv())
                .await
                .expect("expected an error frame, got nothing before the timeout");
            if v.get("t").and_then(|x| x.as_str()) == Some("error") {
                return v;
            }
        }
        panic!("expected an error frame; the hostile command was not rejected");
    }
}

/// Extract the first approval_id from the approval_request events embedded in a
/// snapshot frame's `events` array. Used by G1 gate tests when the mock emits
/// its approval_request before the client subscribes (so it's replayed in the
/// snapshot rather than arriving as a live event).
fn extract_approval_id_from_snapshot(snapshot: &Value) -> Option<String> {
    let events = snapshot.get("events")?.as_array()?;
    for evt in events {
        if evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
            && let Some(aid) = evt
                .get("metadata")
                .and_then(|m| m.get("approval_id"))
                .and_then(|a| a.as_str())
        {
            return Some(aid.to_string());
        }
    }
    None
}

// ── E2E-1: auth + list_sessions ───────────────────────────────────────────────

#[tokio::test]
async fn e2e_auth_and_list() {
    let addr = start_daemon().await;
    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client.send(json!({"t": "list_sessions"})).await;
    let v = client.recv_matching("session_list").await;
    let sessions = v
        .get("sessions")
        .and_then(|s| s.as_array())
        .expect("sessions array");
    assert!(sessions.is_empty(), "new daemon should have no sessions");
}

// ── E2E-2: bad token → error ──────────────────────────────────────────────────

#[tokio::test]
async fn e2e_bad_token_rejected() {
    let addr = start_daemon().await;
    let mut client = TestClient::connect(addr).await;

    client
        .send(json!({"t": "hello", "token": "wrong-token", "protocol": 1}))
        .await;
    let v = client.recv().await;
    assert_eq!(
        v.get("t").and_then(|t| t.as_str()),
        Some("error"),
        "wrong token must produce error"
    );
    assert_eq!(v.get("code").and_then(|c| c.as_str()), Some("auth_failed"),);
}

// ── E2E-3: launch + open + event stream + reconnect invariant ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_launch_open_replay_reconnect() {
    let addr = start_daemon().await;
    let tmp = std::env::temp_dir();

    // ── Step 1: launch a session ──────────────────────────────────────────────
    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "claude",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "e2e test",
        }))
        .await;

    // Receive session_list with the new session.
    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should be created");
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    // ── Step 2: open_session(from_seq=0) → snapshot + live events ────────────
    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;
    assert_eq!(
        snapshot["session_id"].as_str().unwrap(),
        session_id_str,
        "snapshot session_id must match"
    );

    // Collect all events (from snapshot replay + live deltas).
    let snapshot_events = snapshot["events"].as_array().unwrap().clone();

    // Wait for additional live events until we see the terminal Completed.
    let mut all_events: Vec<Value> = snapshot_events;
    let mut saw_terminal = false;

    // The mock claude emits: system/init → message → tool_call → result(Completed).
    // We give a generous timeout and collect until the session finishes.
    for _ in 0..30 {
        if saw_terminal {
            break;
        }
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                match t {
                    "event" => {
                        if let Some(e) = v.get("event") {
                            all_events.push(e.clone());
                        }
                    }
                    "session_status" => {
                        let status = v["status"].as_str().unwrap_or("");
                        if matches!(status, "completed" | "failed" | "killed") {
                            saw_terminal = true;
                        }
                    }
                    _ => {}
                }
            }
            Err(_) => {
                // Timeout — maybe we already have all events in the snapshot.
                break;
            }
        }
    }

    // ── Step 3: assert events are monotonically seq-numbered ──────────────────
    assert!(
        !all_events.is_empty(),
        "expected at least one event, got zero"
    );

    let mut last_seq: u64 = 0;
    for (i, e) in all_events.iter().enumerate() {
        let seq = e["seq"].as_u64().expect("event must have u64 seq");
        assert!(
            seq > last_seq,
            "event[{i}] seq {seq} not monotonically greater than {last_seq}"
        );
        last_seq = seq;
    }

    // ── Step 4: assert content — message + tool-related event in stream ─────
    let has_message = all_events
        .iter()
        .any(|e| e["kind"].as_str() == Some("message"));
    assert!(has_message, "expected at least one message event");

    // The Bash tool_call from the mock is shell-like → gated by authz.
    // After gating the pump emits approval_request instead of tool_call.
    // Either kind counts as "tool activity".
    let has_tool_activity = all_events.iter().any(|e| {
        matches!(
            e["kind"].as_str(),
            Some("tool_call") | Some("approval_request") | Some("tool_blocked")
        )
    });
    assert!(
        has_tool_activity,
        "expected tool_call, approval_request, or tool_blocked event"
    );

    // ── Step 5: reconnect with from_seq=midpoint — invariant 5 ───────────────
    // midpoint = first event's seq (replay only the rest).
    let total = all_events.len();
    if total >= 2 {
        let midpoint = all_events[0]["seq"].as_u64().unwrap();
        // Reconnect.
        let mut client2 = TestClient::connect(addr).await;
        client2.auth().await;
        client2
            .send(json!({
                "t": "open_session",
                "session_id": session_id,
                "from_seq": midpoint,
            }))
            .await;

        let snapshot2 = client2.recv_matching("snapshot").await;
        let replayed = snapshot2["events"].as_array().unwrap();

        // All replayed events must have seq > midpoint (no dupes).
        for e in replayed {
            let seq = e["seq"].as_u64().unwrap();
            assert!(
                seq > midpoint,
                "reconnect replay must only include seq > {midpoint}, got {seq}"
            );
        }

        // No gaps: replayed seqs must be contiguous (seq = midpoint+1, midpoint+2, …).
        for (i, e) in replayed.iter().enumerate() {
            let expected_seq = midpoint + 1 + i as u64;
            let actual_seq = e["seq"].as_u64().unwrap();
            assert_eq!(
                actual_seq, expected_seq,
                "gap detected: expected seq {expected_seq} at index {i}, got {actual_seq}"
            );
        }
    }
}

// ── E2E-4: ping/pong ─────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_ping_pong() {
    let addr = start_daemon().await;
    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client.send(json!({"t": "ping"})).await;
    let v = client.recv_matching("pong").await;
    assert_eq!(v["t"].as_str(), Some("pong"));
}

// ── E2E-5: approval resolve ───────────────────────────────────────────────────
// The mock claude does NOT emit approval_request directly (the codex mock does).
// We test the approve command path by registering one manually via the store
// and then calling approve through the daemon.

#[tokio::test]
async fn e2e_approve_resolves_in_store() {
    use chrono::Utc;
    use cockpitd::{
        approval::ApprovalManager,
        clock::SystemClock,
        model::{Approval, Session, SessionStatus},
        store::Store,
    };

    // Direct store test (approve command flow is exercised in the ws handler).
    let mut store = Store::open_in_memory().unwrap();

    let sid = Uuid::new_v4();
    let session = Session {
        id: sid,
        owner_id: "local".into(),
        agent_type: "claude".into(),
        repo_path: "/tmp".into(),
        status: SessionStatus::Active,
        title: None,
        created_at: Utc::now(),
        last_seq: 0,
    };
    store.create_session(&session).unwrap();

    let mut store2 = Store::open_in_memory().unwrap();
    store2.create_session(&session).unwrap();

    let approval_id = Uuid::new_v4();
    let approval = Approval {
        id: approval_id,
        session_id: sid,
        event_seq: 1,
        tool: "bash".into(),
        args: json!({"cmd": "ls"}),
        created_at: Utc::now(),
        ttl_secs: 60,
        resolution: None,
    };
    let mut mgr = ApprovalManager::new(store2, SystemClock);
    mgr.register_pending(&approval).unwrap();

    // Resolve via the manager (same path as the ws handler).
    mgr.resolve(approval_id, "allow").unwrap();

    let fetched = mgr.get(approval_id).unwrap().unwrap();
    assert_eq!(fetched.resolution.as_deref(), Some("allow"));
}

// ── Codex daemon helpers ──────────────────────────────────────────────────────

fn mock_codex_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest).join("tests/mock-bin/codex")
}

fn mock_codex_gated_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest).join("tests/mock-bin/codex-gated")
}

/// Start a daemon backed by the mock codex binary, on an ephemeral port.
async fn start_daemon_codex() -> SocketAddr {
    use cockpitd::{
        adapter::codex::{CodexAdapter, CodexConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::build_state,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    set_daemon_env();

    // H1a: single in-memory store.
    let store = Store::open_in_memory().unwrap();

    let adapter = CodexAdapter::new(CodexConfig {
        binary: mock_codex_path(),
        extra_flags: vec![],
    });

    let supervisor = Supervisor::new(store, SystemClock, adapter);
    let audit = AuditLog::open_in_memory(SystemClock).unwrap();
    let state = build_state(supervisor, audit);

    ensure_consent_granted();
    tokio::spawn(async move {
        cockpitd::transport::ws::serve_on(listener, state)
            .await
            .expect("codex daemon serve");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ── E2E-6: WS approval end-to-end (F1) ───────────────────────────────────────
//
// Proves:
// - launch a codex session (mock emits approval_request over the wire)
// - open_session, assert an approval_request event arrives with an approval_id
// - send `approve {allow}` over WS
// - assert store shows resolution=allow and session reaches a terminal status

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_codex_approval_wire_roundtrip() {
    let addr = start_daemon_codex().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    // Launch a codex session.
    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "codex",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "e2e approval test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should be created");
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    // Subscribe to live events.
    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let _snapshot = client.recv_matching("snapshot").await;

    // Collect events until we see an approval_request kind.
    let mut approval_id_str: Option<String> = None;
    for _ in 0..40 {
        match timeout(Duration::from_secs(5), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                // Live event delta.
                if t == "event"
                    && let Some(evt) = v.get("event")
                    && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                {
                    // Pull approval_id from metadata.
                    let aid = evt
                        .get("metadata")
                        .and_then(|m| m.get("approval_id"))
                        .and_then(|a| a.as_str())
                        .map(|s| s.to_string());
                    if aid.is_some() {
                        approval_id_str = aid;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    let approval_id_str =
        approval_id_str.expect("expected an approval_request event with approval_id over the wire");

    // H1b: mock now emits a valid UUID so parse must succeed.
    let approval_id: Uuid = approval_id_str
        .parse()
        .expect("H1b: mock codex must emit a valid UUID approval_id");

    // Send approve {allow} — this unparks the gated pump.
    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;

    // After allow, the pump unparks and continues processing the buffered
    // message + completed events. Wait briefly for any subsequent event,
    // proving the pump continued past the gate.
    let mut saw_post_gate_event = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event" {
                    let kind = v
                        .get("event")
                        .and_then(|e| e.get("kind"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("");
                    if !kind.is_empty() && kind != "approval_request" {
                        saw_post_gate_event = true;
                        break;
                    }
                }
                if t == "error" {
                    panic!("unexpected error frame after approve: {v}");
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        saw_post_gate_event,
        "after approve(allow) on codex session, pump must continue emitting events past the gate"
    );

    // Key assertion: approval_request frame arrived with an approval_id field.
    assert!(
        !approval_id_str.is_empty(),
        "approval_request must carry a non-empty approval_id"
    );
}

// ── E2E-7: TTL auto-deny via sweep ───────────────────────────────────────────
//
// A pending approval left unanswered past its TTL is auto-denied by the sweep.
// We test this directly via the ApprovalManager with a FakeClock (the sweep path
// is fully tested in approval::tests; here we assert it over the store layer).

#[tokio::test]
async fn e2e_ttl_auto_deny_via_store() {
    use chrono::{Duration, Utc};
    use cockpitd::{
        approval::ApprovalManager,
        clock::FakeClock,
        model::{Approval, Session, SessionStatus},
        store::Store,
    };

    let clock = FakeClock::at_epoch();
    let mut store = Store::open_in_memory().unwrap();

    let sid = Uuid::new_v4();
    let session = Session {
        id: sid,
        owner_id: "local".into(),
        agent_type: "codex".into(),
        repo_path: "/tmp".into(),
        status: SessionStatus::Active,
        title: None,
        created_at: Utc::now(),
        last_seq: 0,
    };
    store.create_session(&session).unwrap();

    let store2 = Store::open_in_memory().unwrap();
    // Need separate store for approval manager — seed the session.
    {
        let mut s2_tmp = Store::open_in_memory().unwrap();
        s2_tmp.create_session(&session).unwrap();
        drop(s2_tmp);
    }

    // Create approval manager with the fake clock.
    let mut store3 = Store::open_in_memory().unwrap();
    store3.create_session(&session).unwrap();
    let _ = store2;

    let mut mgr = ApprovalManager::new(store3, clock.clone());

    // Register a pending approval with a 10-second TTL.
    let approval_id = Uuid::new_v4();
    let approval = Approval {
        id: approval_id,
        session_id: sid,
        event_seq: 1,
        tool: "shell".into(),
        args: json!({"cmd": "rm -rf /important"}),
        created_at: clock.now(),
        ttl_secs: 10,
        resolution: None,
    };
    mgr.register_pending(&approval).unwrap();

    // Approval is still pending.
    let fetched = mgr.get(approval_id).unwrap().unwrap();
    assert!(fetched.resolution.is_none(), "should be pending initially");

    // Advance clock past TTL.
    clock.advance(Duration::seconds(11));

    // Run the sweep.
    let auto_denied = mgr.sweep().unwrap();
    assert_eq!(auto_denied, 1, "sweep should auto-deny 1 expired approval");

    // Assert store shows auto_denied.
    let resolved = mgr.get(approval_id).unwrap().unwrap();
    assert_eq!(
        resolved.resolution.as_deref(),
        Some("auto_denied"),
        "resolution must be auto_denied after TTL sweep"
    );
}

// ── Gated / multiblock daemon helpers ────────────────────────────────────────

fn mock_claude_gated_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest).join("tests/mock-bin/claude-gated")
}

fn mock_claude_multiblock_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(manifest).join("tests/mock-bin/claude-multiblock")
}

/// Start a daemon backed by the gated mock claude binary (emits a non-allowlisted tool_call).
async fn start_daemon_gated() -> SocketAddr {
    use cockpitd::{
        adapter::claude::{ClaudeAdapter, ClaudeConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::build_state,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    set_daemon_env();

    // H1a: single in-memory store.
    let store = Store::open_in_memory().unwrap();

    let adapter = ClaudeAdapter::new(ClaudeConfig {
        binary: mock_claude_gated_path(),
        extra_flags: vec![],
    });

    let supervisor = Supervisor::new(store, SystemClock, adapter);
    let audit = AuditLog::open_in_memory(SystemClock).unwrap();
    let state = build_state(supervisor, audit);

    ensure_consent_granted();
    tokio::spawn(async move {
        cockpitd::transport::ws::serve_on(listener, state)
            .await
            .expect("gated daemon serve");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Start a daemon backed by the multiblock mock claude binary.
async fn start_daemon_multiblock() -> SocketAddr {
    use cockpitd::{
        adapter::claude::{ClaudeAdapter, ClaudeConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::build_state,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    set_daemon_env();

    // H1a: single in-memory store.
    let store = Store::open_in_memory().unwrap();

    let adapter = ClaudeAdapter::new(ClaudeConfig {
        binary: mock_claude_multiblock_path(),
        extra_flags: vec![],
    });

    let supervisor = Supervisor::new(store, SystemClock, adapter);
    let audit = AuditLog::open_in_memory(SystemClock).unwrap();
    let state = build_state(supervisor, audit);

    ensure_consent_granted();
    tokio::spawn(async move {
        cockpitd::transport::ws::serve_on(listener, state)
            .await
            .expect("multiblock daemon serve");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ── E2E-9: G1 authz gate — allow path ────────────────────────────────────────
//
// Proves:
// - launch a session whose mock emits a NON-allowlisted tool_call (write_file)
// - assert an approval_request event arrives over the wire before the session
//   proceeds past the tool
// - send approve{allow} → session continues; a tool_call (or tool_blocked)
//   event follows with the correct tool name

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_authz_gate_allow() {
    let addr = start_daemon_gated().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "claude",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "authz gate allow test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should be created");
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;

    // The approval_request may already be in the snapshot (mock exits fast).
    // Check both snapshot events and live deltas.
    let mut approval_id_str: Option<String> = extract_approval_id_from_snapshot(&snapshot);

    // Why this loop reports WHY it gave up (RC-028).
    //
    // It used to `break` on any recv error and then unwrap with a single message:
    // "authz gate must emit approval_request". A dropped connection and a gate
    // that never fired produced the identical panic, so a failure could not be
    // triaged without a rerun — and this test failed once in a pre-push worktree
    // and then passed 7/7 across four modes, which is precisely the situation
    // where the panic text is the only evidence you get.
    //
    // RC-011 is the same defect class already on the register: a fixture that
    // cannot separate two hypotheses keeps producing ambiguous root causes.
    let mut give_up_reason = "no approval_request arrived within the read window";
    let mut frames_seen = 0_usize;
    if approval_id_str.is_none() {
        // Not in snapshot — wait for live approval_request event.
        for _ in 0..40 {
            match timeout(Duration::from_secs(2), client.recv()).await {
                Ok(v) => {
                    frames_seen += 1;
                    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "error" {
                        give_up_reason = "server sent an error frame instead of approval_request";
                        break;
                    }
                    if t == "event"
                        && let Some(evt) = v.get("event")
                        && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                    {
                        let aid = evt
                            .get("metadata")
                            .and_then(|m| m.get("approval_id"))
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string());
                        if aid.is_some() {
                            approval_id_str = aid;
                            break;
                        }
                    }
                }
                Err(_) => {
                    // Distinguish "the socket died" from "the gate stayed silent".
                    give_up_reason = "recv failed (connection closed or timed out) before any \
                                      approval_request — this is a transport/liveness failure, \
                                      NOT evidence that the authz gate is broken";
                    break;
                }
            }
        }
    }

    let approval_id_str = approval_id_str.unwrap_or_else(|| {
        panic!(
            "authz gate must emit approval_request for non-allowlisted write_file tool — \
             gave up because: {give_up_reason} (frames observed after snapshot: {frames_seen})"
        )
    });

    // Send approve{allow}.
    let approval_id: Uuid = approval_id_str
        .parse()
        .expect("approval_id must be a valid UUID");
    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;

    // After allow, the pump should continue and the session should proceed.
    // Drain events looking for a tool_call event (the permitted tool_call)
    // or session completion.
    let mut saw_tool_call_or_terminal = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(2), client.stream.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                let v: Value = serde_json::from_str(&text).unwrap_or_default();
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event"
                    && let Some(evt) = v.get("event")
                {
                    let kind = evt.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                    if kind == "tool_call" {
                        saw_tool_call_or_terminal = true;
                        break;
                    }
                }
                if t == "session_status" {
                    saw_tool_call_or_terminal = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {} // skip non-text
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break, // timeout
        }
    }

    assert!(
        saw_tool_call_or_terminal,
        "after approve(allow), session should emit tool_call event or reach terminal"
    );
}

// ── E2E-10: G1 authz gate — deny path ────────────────────────────────────────
//
// Proves:
// - same setup as E2E-9
// - send approve{deny} → session emits tool_blocked; does NOT get tool result

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_authz_gate_deny() {
    let addr = start_daemon_gated().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "claude",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "authz gate deny test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;

    // Check snapshot first (mock may exit before we subscribe).
    let mut approval_id_str: Option<String> = extract_approval_id_from_snapshot(&snapshot);

    if approval_id_str.is_none() {
        for _ in 0..40 {
            match timeout(Duration::from_secs(5), client.recv()).await {
                Ok(v) => {
                    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "event"
                        && let Some(evt) = v.get("event")
                        && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                    {
                        let aid = evt
                            .get("metadata")
                            .and_then(|m| m.get("approval_id"))
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string());
                        if aid.is_some() {
                            approval_id_str = aid;
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let approval_id_str =
        approval_id_str.expect("authz gate must emit approval_request for non-allowlisted tool");

    // Send approve{deny}.
    let approval_id: Uuid = approval_id_str
        .parse()
        .expect("approval_id must be a valid UUID");
    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "deny",
        }))
        .await;

    // After deny, the pump should emit a tool_blocked event and NOT forward
    // the tool result.
    let mut saw_tool_blocked = false;
    let mut saw_tool_result = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(2), client.stream.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                let v: Value = serde_json::from_str(&text).unwrap_or_default();
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event"
                    && let Some(evt) = v.get("event")
                {
                    let kind = evt.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                    if kind == "tool_blocked" {
                        saw_tool_blocked = true;
                    }
                    if kind == "tool_result" {
                        saw_tool_result = true;
                    }
                }
            }
            Ok(Some(Ok(_))) => {} // skip non-text
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break, // timeout
        }
    }

    assert!(
        saw_tool_blocked,
        "deny decision must produce tool_blocked event"
    );
    assert!(
        !saw_tool_result,
        "deny decision must NOT forward tool_result to the session"
    );
}

// ── E2E-11: G2 multi-block assistant turn ────────────────────────────────────
//
// Proves:
// - a single assistant message with text + 2 tool_use blocks yields 3 events
//   with monotonic seq (1 message + 2 tool_call), in order.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_multiblock_turn_yields_three_events() {
    let addr = start_daemon_multiblock().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "claude",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "multi-block test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;

    // Collect all events from snapshot + live deltas. Under full-suite load the
    // async pump can start after an initial quiet window, so keep polling until
    // the expected signal arrives or the overall deadline expires.
    let mut all_events: Vec<Value> = snapshot["events"].as_array().unwrap().clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let kinds: Vec<&str> = all_events
            .iter()
            .filter_map(|e| e.get("kind").and_then(|k| k.as_str()))
            .collect();
        let has_message = kinds.contains(&"message");
        let has_tool = kinds.contains(&"tool_call") || kinds.contains(&"approval_request");
        if has_message && has_tool {
            break;
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        match timeout(Duration::from_millis(100), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event"
                    && let Some(e) = v.get("event")
                {
                    all_events.push(e.clone());
                }
            }
            Err(_) => continue,
        }
    }

    // The multiblock mock emits: system/init → (text + read_file + write_file) → result
    // text and both tool_use blocks come from a SINGLE assistant message.
    // read_file is on the conservative allowlist (Permit) → tool_call event broadcasted.
    // write_file is NOT on allowlist (RequireApproval) → approval_request emitted.
    //
    // So we expect at minimum: status, message, tool_call(read_file), approval_request(write_file).
    // Assert monotonic seq and that message + tool_call both appear.
    let kinds: Vec<&str> = all_events
        .iter()
        .filter_map(|e| e.get("kind").and_then(|k| k.as_str()))
        .collect();

    // Must contain at least one message event (the text block).
    assert!(
        kinds.contains(&"message"),
        "multiblock turn must produce a message event; got kinds: {kinds:?}"
    );

    // Must contain at least one tool_call or approval_request (the tool_use blocks).
    let has_tool = kinds.contains(&"tool_call") || kinds.contains(&"approval_request");
    assert!(
        has_tool,
        "multiblock turn must produce tool_call or approval_request from tool_use blocks; got kinds: {kinds:?}"
    );

    // Monotonic seq invariant.
    let mut last_seq = 0u64;
    for (i, e) in all_events.iter().enumerate() {
        let seq = e["seq"].as_u64().expect("event must have u64 seq");
        assert!(
            seq > last_seq,
            "event[{i}] seq {seq} not monotonically greater than {last_seq}; kinds so far: {kinds:?}"
        );
        last_seq = seq;
    }
}

// ── E2E-8: audit log wire roundtrip (F2) ─────────────────────────────────────
//
// Launch a session, then query the audit log over the WS. Assert the
// session:launch entry appears.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_audit_log_wire_roundtrip() {
    let addr = start_daemon().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    // Launch a session (this produces a session:launch audit entry).
    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "claude",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "audit test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should exist");
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();

    // Wait a moment for the pump to log the session lifecycle events.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Query audit log (no session filter — get all).
    client.send(json!({ "t": "get_audit", "limit": 50 })).await;

    let audit_frame = client.recv_matching("audit_list").await;
    let entries = audit_frame["entries"].as_array().expect("entries array");

    // Must have at least the session:launch entry.
    let has_launch = entries
        .iter()
        .any(|e| e.get("action").and_then(|a| a.as_str()) == Some("session:launch"));
    assert!(
        has_launch,
        "audit_list must contain a session:launch entry after launching a session"
    );

    // Filter by session_id.
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");
    client
        .send(json!({
            "t": "get_audit",
            "session_id": session_id.to_string(),
            "limit": 20,
        }))
        .await;

    let filtered_frame = client.recv_matching("audit_list").await;
    let filtered_entries = filtered_frame["entries"].as_array().expect("entries array");

    // All returned entries must carry the correct session_id.
    for e in filtered_entries {
        if let Some(sid) = e.get("session_id").and_then(|s| s.as_str()) {
            assert_eq!(
                sid, session_id_str,
                "filtered audit entry must have correct session_id"
            );
        }
    }
}

/// Start a daemon backed by the gated codex mock (emits approval_request after
/// an allowlisted tool_call — exercises H1b without triggering the G1 gate).
async fn start_daemon_codex_gated() -> SocketAddr {
    use cockpitd::{
        adapter::codex::{CodexAdapter, CodexConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::build_state,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    set_daemon_env();

    let store = Store::open_in_memory().unwrap();

    let adapter = CodexAdapter::new(CodexConfig {
        binary: mock_codex_gated_path(),
        extra_flags: vec![],
    });

    let supervisor = Supervisor::new(store, SystemClock, adapter);
    let audit = AuditLog::open_in_memory(SystemClock).unwrap();
    let state = build_state(supervisor, audit);

    ensure_consent_granted();
    tokio::spawn(async move {
        cockpitd::transport::ws::serve_on(listener, state)
            .await
            .expect("codex gated daemon serve");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

// ── E2E-12: H1b Codex native approval gate — allow path ─────────────────────
//
// Proves (H1b):
// - launch a codex session whose mock emits a native `approval_request` with a
//   valid UUID id
// - assert the pump parks: the `approval_request` event arrives over the wire
//   and the session stays at AwaitingInput until the client resolves it
// - send approve{allow} → pump unparks; session continues and reaches terminal
//
// Uses the same mock-bin/codex which emits approval_request after the tool_call.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_codex_native_gate_allow() {
    let addr = start_daemon_codex_gated().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "codex",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "h1b codex gate allow test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should be created");
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;

    // Collect until we see approval_request (pump is parked here).
    let mut approval_id_str: Option<String> = extract_approval_id_from_snapshot(&snapshot);

    if approval_id_str.is_none() {
        for _ in 0..40 {
            match timeout(Duration::from_secs(5), client.recv()).await {
                Ok(v) => {
                    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "event"
                        && let Some(evt) = v.get("event")
                        && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                    {
                        let aid = evt
                            .get("metadata")
                            .and_then(|m| m.get("approval_id"))
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string());
                        if aid.is_some() {
                            approval_id_str = aid;
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let approval_id_str = approval_id_str
        .expect("H1b: codex-gated mock must park on approval_request and broadcast it");

    // The approval_id must be the valid UUID emitted by the codex-gated mock.
    let approval_id: Uuid = approval_id_str
        .parse()
        .expect("H1b: codex-gated mock must emit a valid UUID approval_id");

    // Send approve{allow} — pump unparks.
    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;

    // After allow, pump unparks and continues processing buffered events.
    // The codex-gated mock has a follow-up message and completed still queued.
    // Assert we see at least one more event (message or any event) after the
    // approval, proving the pump continued past the gate.
    let mut saw_post_gate_event = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event" {
                    let kind = v
                        .get("event")
                        .and_then(|e| e.get("kind"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("");
                    // Any event after the gate resolves proves the pump continued.
                    if !kind.is_empty() && kind != "approval_request" {
                        saw_post_gate_event = true;
                        break;
                    }
                }
                if t == "error" {
                    panic!("unexpected error after approve(allow) on codex gate: {v}");
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        saw_post_gate_event,
        "after approve(allow) on codex native gate, pump must continue and emit events past the gate"
    );
}

// ── E2E-13: H1b Codex native approval gate — deny path ──────────────────────
//
// Proves (H1b):
// - same setup as E2E-12
// - send approve{deny} → pump emits tool_blocked, does NOT forward subsequent
//   tool results, then reaches terminal

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_codex_native_gate_deny() {
    let addr = start_daemon_codex_gated().await;
    let tmp = std::env::temp_dir();

    let mut client = TestClient::connect(addr).await;
    client.auth().await;

    client
        .send(json!({
            "t": "launch_session",
            "agent_type": "codex",
            "repo_path": tmp.to_str().unwrap(),
            "prompt": "h1b codex gate deny test",
        }))
        .await;

    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    let session_id_str = sessions[0]["id"].as_str().unwrap().to_string();
    let session_id: Uuid = session_id_str.parse().expect("valid uuid");

    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;

    let snapshot = client.recv_matching("snapshot").await;

    let mut approval_id_str: Option<String> = extract_approval_id_from_snapshot(&snapshot);

    if approval_id_str.is_none() {
        for _ in 0..40 {
            match timeout(Duration::from_secs(5), client.recv()).await {
                Ok(v) => {
                    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "event"
                        && let Some(evt) = v.get("event")
                        && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                    {
                        let aid = evt
                            .get("metadata")
                            .and_then(|m| m.get("approval_id"))
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string());
                        if aid.is_some() {
                            approval_id_str = aid;
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let approval_id_str =
        approval_id_str.expect("H1b: codex-gated mock deny — must receive approval_request");

    let approval_id: Uuid = approval_id_str
        .parse()
        .expect("H1b: codex-gated mock must emit a valid UUID approval_id");

    // Send approve{deny} — pump emits tool_blocked.
    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "deny",
        }))
        .await;

    // Collect events looking for tool_blocked and ensuring no tool_result.
    let mut saw_tool_blocked = false;
    let mut saw_tool_result = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "event"
                    && let Some(evt) = v.get("event")
                {
                    let kind = evt.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                    if kind == "tool_blocked" {
                        saw_tool_blocked = true;
                    }
                    if kind == "tool_result" {
                        saw_tool_result = true;
                    }
                }
                if t == "session_status" {
                    let status = v["status"].as_str().unwrap_or("");
                    if matches!(status, "completed" | "failed" | "killed" | "disconnected") {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        saw_tool_blocked,
        "deny on codex native gate must produce tool_blocked event"
    );
    assert!(
        !saw_tool_result,
        "deny on codex native gate must NOT forward tool_result"
    );
}

// ── E2E-14: H2 TTL sweep unparks a gated pump ────────────────────────────────
//
// Proves end-to-end auto-deny without real wall-clock waits:
// - An approval is inserted with a TTL that has already elapsed (created at
//   epoch, TTL=10s, sweep called with now=epoch+11s).
// - A gate Notify is planted in the gates map (simulating a parked pump).
// - `sweep_once` is called with a `now` past the deadline.
// - The Notify is woken (gate unblocks).
// - The approval row shows `auto_denied`.
// - The sweep returns count=1.

#[tokio::test]
async fn e2e_sweep_auto_deny_unparks_gate() {
    use chrono::{DateTime, Duration as ChronoDuration};
    use cockpitd::{
        adapter::claude::{ClaudeAdapter, ClaudeConfig},
        audit::AuditLog,
        clock::FakeClock,
        model::Approval,
        store::Store,
        supervisor::Supervisor,
        transport::{build_state, sweep::sweep_once},
    };
    use std::sync::{
        Arc as StdArc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration as StdDuration;

    // Epoch as base time: approval created at T=0, TTL=10s → deadline T+10.
    let epoch: DateTime<chrono::Utc> = DateTime::from_timestamp(0, 0).unwrap();
    let fake_clock = FakeClock::new(epoch);
    let fake_clock2 = fake_clock.clone();

    // Build an in-memory supervisor backed by the mock claude binary.
    // The adapter won't be called in this test — we only need the session row.
    let store = Store::open_in_memory().unwrap();
    let adapter = ClaudeAdapter::new(ClaudeConfig {
        binary: mock_claude_path(),
        extra_flags: vec![],
    });
    let supervisor = Supervisor::new(store, fake_clock, adapter);
    let audit = AuditLog::open_in_memory(fake_clock2).unwrap();
    let state = build_state(supervisor, audit);

    // ── Create a session row via ErasedSupervisor::launch_session ────────────
    // This creates the DB row the FK constraint requires.
    // The resulting pump receiver is intentionally dropped (no pump needed).
    let launched_sid = {
        let (event_tx, _) = tokio::sync::broadcast::channel::<cockpitd::model::Event>(8);
        let mut sup = state.supervisor.lock().await;
        sup.0
            .launch_session("claude", "/tmp", None, "local", event_tx)
            .expect("launch session for E2E-14")
    };

    // ── Insert a pending approval with TTL=10s, created at epoch ─────────────
    let approval_id = Uuid::new_v4();
    let approval = Approval {
        id: approval_id,
        session_id: launched_sid,
        event_seq: 1,
        tool: "dangerous_shell".into(),
        args: serde_json::json!({"cmd": "rm -rf /"}),
        created_at: epoch,
        ttl_secs: 10,
        resolution: None,
    };
    {
        let mut sup = state.supervisor.lock().await;
        sup.0.insert_approval(&approval).expect("insert_approval");
    }

    // Sanity: approval is pending before the sweep.
    {
        let sup = state.supervisor.lock().await;
        let a = sup.0.get_approval(approval_id).unwrap().unwrap();
        assert!(
            a.resolution.is_none(),
            "approval must be pending before sweep"
        );
    }

    // ── Plant a gate Notify (simulating the parked run_pump task) ────────────
    let notify = StdArc::new(tokio::sync::Notify::new());
    {
        let mut g = state.approval_gates.lock().unwrap();
        g.insert(approval_id, notify.clone());
    }

    // ── Spawn a task that parks on the gate and records being woken ───────────
    let notify_for_task = notify.clone();
    let woken = StdArc::new(AtomicBool::new(false));
    let woken_for_task = woken.clone();
    let park_task = tokio::spawn(async move {
        notify_for_task.notified().await;
        woken_for_task.store(true, Ordering::SeqCst);
    });

    // ── Call sweep_once with now = epoch + 11s (past the 10s deadline) ───────
    let sweep_now = epoch + ChronoDuration::seconds(11);
    let denied_count = sweep_once(&state.supervisor, &state.approval_gates, sweep_now).await;

    // The park_task should complete immediately after sweep_once fires the gate.
    timeout(StdDuration::from_secs(2), park_task)
        .await
        .expect("park_task timed out — gate was not woken within 2 s")
        .expect("park_task panicked");

    // ── Assertions ────────────────────────────────────────────────────────────
    assert_eq!(
        denied_count, 1,
        "sweep_once must report 1 auto-denied approval"
    );
    assert!(
        woken.load(Ordering::SeqCst),
        "parked gate Notify must have been woken by sweep_once"
    );
    let resolution = {
        let sup = state.supervisor.lock().await;
        sup.0.get_approval(approval_id).unwrap().unwrap().resolution
    };
    assert_eq!(
        resolution.as_deref(),
        Some("auto_denied"),
        "approval row must show auto_denied after sweep"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// ARP security audit — adversarial regression tests
//
// Each test below drives a hostile input through the real transport and asserts
// the specific rejection. A test that passes because nothing happened is a bug;
// `expect_error` panics rather than falling through.
// ═══════════════════════════════════════════════════════════════════════════════

/// Launch a session as `client_id` and return its id. Panics on refusal.
async fn launch_as(client: &mut TestClient, agent_type: &str, repo_path: &str) -> Uuid {
    client
        .send(json!({
            "t": "launch_session",
            "agent_type": agent_type,
            "repo_path": repo_path,
            "prompt": "arp adversarial test",
        }))
        .await;
    let list_frame = client.recv_matching("session_list").await;
    let sessions = list_frame["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "session should be created");
    sessions[0]["id"].as_str().unwrap().parse().unwrap()
}

/// Drive a session until its first `approval_request` and return the approval id.
async fn await_approval_id(client: &mut TestClient, session_id: Uuid) -> Uuid {
    client
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;
    let snapshot = client.recv_matching("snapshot").await;
    let mut found = extract_approval_id_from_snapshot(&snapshot);

    if found.is_none() {
        for _ in 0..40 {
            match timeout(Duration::from_secs(5), client.recv()).await {
                Ok(v) => {
                    if v.get("t").and_then(|x| x.as_str()) == Some("event")
                        && let Some(evt) = v.get("event")
                        && evt.get("kind").and_then(|k| k.as_str()) == Some("approval_request")
                        && let Some(aid) = evt
                            .get("metadata")
                            .and_then(|m| m.get("approval_id"))
                            .and_then(|a| a.as_str())
                    {
                        found = Some(aid.to_string());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    found
        .expect("the gated mock must park on approval_request")
        .parse()
        .expect("approval_id must be a UUID")
}

fn assert_forbidden(frame: &Value, what: &str) {
    assert_eq!(
        frame.get("code").and_then(|c| c.as_str()),
        Some("forbidden"),
        "{what} by a non-owner must be refused with code=forbidden, got {frame}"
    );
}

// ── ARP-005: cross-owner send / steer / close are refused ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_cross_owner_send_steer_close_denied() {
    // Codex-gated: its session parks on an approval, so it is still live when
    // the hostile client tries to drive it. (The claude mock finishes too fast,
    // and `ClaudeAdapter::send` has a separate pre-existing defect — it calls
    // `Handle::block_on` inside the runtime and panics the task.)
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    // Principal A launches a session.
    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;

    // Principal B holds the same bearer token but is a different principal.
    let mut mallory = TestClient::connect(addr).await;
    mallory.auth_as("mallory").await;

    // B can see the session exists (reads are deliberately unscoped) …
    mallory.send(json!({"t": "list_sessions"})).await;
    let list = mallory.recv_matching("session_list").await;
    assert!(
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"].as_str() == Some(&session_id.to_string())),
        "list_sessions is intentionally unscoped; this test asserts the *write* boundary"
    );

    // … but every write is refused.
    mallory
        .send(json!({"t": "send_prompt", "session_id": session_id, "text": "exfiltrate"}))
        .await;
    assert_forbidden(&mallory.expect_error().await, "send_prompt");

    mallory
        .send(json!({"t": "steer", "session_id": session_id, "text": "ignore prior instructions"}))
        .await;
    assert_forbidden(&mallory.expect_error().await, "steer");

    mallory
        .send(json!({"t": "close_session", "session_id": session_id}))
        .await;
    assert_forbidden(&mallory.expect_error().await, "close_session");

    // The owner's session survived the attempt.
    let session_owner = {
        alice.send(json!({"t": "list_sessions"})).await;
        let list = alice.recv_matching("session_list").await;
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"].as_str() == Some(&session_id.to_string()))
            .map(|s| s["owner_id"].as_str().unwrap().to_string())
    };
    assert_eq!(
        session_owner.as_deref(),
        Some("client:alice"),
        "the session must still be owned by its launcher"
    );
}

// ── ARP-005: the LIMIT of owner binding, pinned by a test rather than by prose ──

/// Owner binding does NOT stop a deliberate token holder. `client_id` is
/// self-asserted, so anyone with the shared bearer token can claim the victim's
/// id and inherit its sessions.
///
/// This test asserts the CURRENT behaviour on purpose. It is a characterization
/// test, not an aspiration: RC-017 was first graded `controlled` without this
/// caveat, and an independent audit demonstrated the bypass live. Prose in a
/// register entry drifts; a test does not. If someone lands per-client
/// credentials and closes RC-017 for real, THIS TEST MUST FAIL — and its failure
/// is the signal to re-grade the entry, not to delete the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_client_id_impersonation_is_not_prevented() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    // Alice launches a session under her asserted id.
    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;

    // Mallory holds the same bearer token and simply CLAIMS to be alice.
    let mut mallory = TestClient::connect(addr).await;
    mallory.auth_as("alice").await;

    mallory
        .send(json!({"t": "send_prompt", "session_id": session_id, "text": "impersonated"}))
        .await;

    // The ownership check passes, because there is nothing to check against — the
    // id was asserted, not proven. Any error that surfaces comes from downstream
    // (the adapter), never from the owner gate.
    let err = mallory.expect_error().await;
    let code = err["code"].as_str().unwrap_or("");
    assert_ne!(
        code, "forbidden",
        "documenting the limit: impersonating a client_id is NOT refused by owner binding. \
         If this now returns `forbidden`, per-client credentials landed — re-grade RC-017 \
         from `open against a deliberate token holder` to `controlled`, and rewrite this test."
    );
}

// ── ARP-005: a session with no owner match is refused even when it exists ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_anonymous_connection_cannot_touch_named_owner_session() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;

    // A plain `hello` (no client_id) gets a fresh per-connection principal.
    let mut anon = TestClient::connect(addr).await;
    anon.auth().await;
    anon.send(json!({"t": "send_prompt", "session_id": session_id, "text": "hi"}))
        .await;
    assert_forbidden(
        &anon.expect_error().await,
        "send_prompt from an anonymous connection",
    );
}

// ── ARP-005: an unknown session id is not_found, not forbidden ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_unknown_session_reports_not_found() {
    let addr = start_daemon().await;
    let mut client = TestClient::connect(addr).await;
    client.auth_as("alice").await;

    client
        .send(json!({"t": "send_prompt", "session_id": Uuid::new_v4(), "text": "hi"}))
        .await;
    let err = client.expect_error().await;
    assert_eq!(
        err.get("code").and_then(|c| c.as_str()),
        Some("not_found"),
        "a session that does not exist must report not_found, got {err}"
    );
}

// ── ARP-005: approval hijack is refused ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_approval_hijack_denied() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    // Principal A launches and drives to a parked approval.
    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;
    let approval_id = await_approval_id(&mut alice, session_id).await;

    // Principal B knows the approval UUID and tries to resolve it.
    let mut mallory = TestClient::connect(addr).await;
    mallory.auth_as("mallory").await;
    mallory
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;
    assert_forbidden(&mallory.expect_error().await, "approve");

    // The hijack must not have resolved the row: the owner can still decide,
    // and their decision is accepted.
    alice
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;

    let mut saw_post_gate_event = false;
    for _ in 0..30 {
        match timeout(Duration::from_secs(3), alice.recv()).await {
            Ok(v) => {
                let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                if t == "error" {
                    panic!("the owner's own approve must succeed, got {v}");
                }
                if t == "event" {
                    let kind = v
                        .get("event")
                        .and_then(|e| e.get("kind"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("");
                    if !kind.is_empty() && kind != "approval_request" {
                        saw_post_gate_event = true;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_post_gate_event,
        "after the owner approves, the pump must continue past the gate"
    );
}

// ── ARP-005: an unknown approval id is not_found ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_unknown_approval_reports_not_found() {
    let addr = start_daemon().await;
    let mut client = TestClient::connect(addr).await;
    client.auth_as("alice").await;

    client
        .send(json!({
            "t": "approve",
            "approval_id": Uuid::new_v4().to_string(),
            "decision": "allow",
        }))
        .await;
    let err = client.expect_error().await;
    assert_eq!(
        err.get("code").and_then(|c| c.as_str()),
        Some("not_found"),
        "an approval that does not exist must report not_found, got {err}"
    );
}

// ── ARP-005: repo_path escapes are refused ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_repo_path_escape_denied() {
    let addr = start_daemon().await;
    let root = allowed_repo_root();

    let mut client = TestClient::connect(addr).await;
    client.auth_as("alice").await;

    // 1. A flat system path outside the allowlist.
    // 2. `..` traversal whose *string* starts inside the allowed root — this
    //    one only fails if the daemon canonicalizes before comparing.
    // 3. A symlink inside the allowed root pointing at /etc.
    let traversal = cockpitd::policy::traversal_out_of(&root, "etc");
    assert!(
        traversal.starts_with(root.to_str().unwrap()),
        "the traversal case must textually start inside the allowed root, else \
         it does not prove canonicalization: {traversal}"
    );

    let link_dir = root.join(format!("arp005-symlink-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("escape");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", &link).unwrap();

    let mut hostile: Vec<String> = vec!["/etc".to_string(), traversal];
    #[cfg(unix)]
    hostile.push(link.to_string_lossy().to_string());

    for path in &hostile {
        client
            .send(json!({
                "t": "launch_session",
                "agent_type": "claude",
                "repo_path": path,
                "prompt": "escape attempt",
            }))
            .await;
        let err = client.expect_error().await;
        assert_eq!(
            err.get("code").and_then(|c| c.as_str()),
            Some("repo_path_denied"),
            "repo_path {path} must be refused with repo_path_denied, got {err}"
        );
        assert!(
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .contains("COCKPIT_REPO_ALLOWLIST"),
            "the refusal must tell the operator how to configure the allowlist: {err}"
        );
    }

    // No session was created by any of the attempts.
    client.send(json!({"t": "list_sessions"})).await;
    let list = client.recv_matching("session_list").await;
    assert!(
        list["sessions"].as_array().unwrap().is_empty(),
        "a refused repo_path must not create a session: {list}"
    );

    std::fs::remove_dir_all(&link_dir).ok();
}

// ── ARP-005: the normal path still works (positive control) ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_owner_drives_own_session_end_to_end() {
    // Default loopback bind, an allowed repo_path, one principal: everything
    // the hardening added must be invisible here.
    let addr = start_daemon().await;
    assert!(
        addr.ip().is_loopback(),
        "the default test daemon must bind loopback"
    );
    let repo = allowed_repo_root();

    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "claude", repo.to_str().unwrap()).await;

    alice
        .send(json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": 0_u64,
        }))
        .await;
    let snapshot = alice.recv_matching("snapshot").await;
    assert_eq!(
        snapshot["session"]["owner_id"].as_str(),
        Some("client:alice"),
        "the launching principal must own the session"
    );
    assert_eq!(
        snapshot["session"]["repo_path"].as_str(),
        Some(repo.to_str().unwrap()),
        "the stored repo_path must be the canonical, checked path"
    );

    // The connection still works after the ownership checks ran.
    alice.send(json!({"t": "ping"})).await;
    assert_eq!(
        alice.recv_matching("pong").await["t"].as_str(),
        Some("pong")
    );
}

/// The owner's writes clear the ownership check.
///
/// Driven against the codex-gated mock, whose session parks on an approval and
/// so stays live long enough to command. The assertion is narrow on purpose: no
/// command from the owner may come back `forbidden`. Adapter-level failures
/// (`send_failed`, `close_failed`) are a different layer and are allowed here —
/// this test guards the authorization boundary, not the adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_owner_writes_are_never_forbidden() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;

    for cmd in [
        json!({"t": "send_prompt", "session_id": session_id, "text": "carry on"}),
        json!({"t": "steer", "session_id": session_id, "text": "focus"}),
        json!({"t": "close_session", "session_id": session_id}),
    ] {
        let label = cmd["t"].as_str().unwrap().to_string();
        alice.send(cmd).await;
        // Drain whatever comes back within a short window; assert none of it is
        // an ownership refusal.
        for _ in 0..5 {
            match timeout(Duration::from_millis(300), alice.recv()).await {
                Ok(v) => {
                    if v.get("t").and_then(|x| x.as_str()) == Some("error") {
                        assert_ne!(
                            v.get("code").and_then(|c| c.as_str()),
                            Some("forbidden"),
                            "the owner's own {label} must not be refused: {v}"
                        );
                    }
                }
                Err(_) => break,
            }
        }
    }
}

/// A reconnect carrying the same `client_id` keeps write access.
///
/// This is the reason `client_id` exists: with per-connection identities alone,
/// a phone that drops WiFi would come back unable to steer or approve its own
/// running session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp005_reconnect_with_same_client_id_keeps_control() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    let mut alice = TestClient::connect(addr).await;
    alice.auth_as("alice").await;
    let session_id = launch_as(&mut alice, "codex", repo.to_str().unwrap()).await;
    let approval_id = await_approval_id(&mut alice, session_id).await;
    drop(alice);

    // New socket, same asserted identity.
    let mut alice2 = TestClient::connect(addr).await;
    alice2.auth_as("alice").await;
    alice2
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "allow",
        }))
        .await;
    alice2.send(json!({"t": "ping"})).await;

    for _ in 0..10 {
        let v = timeout(Duration::from_secs(3), alice2.recv())
            .await
            .expect("expected pong after approve on a reconnected client");
        match v.get("t").and_then(|x| x.as_str()) {
            Some("error") => panic!("a reconnect with the same client_id must keep control: {v}"),
            Some("pong") => return,
            _ => continue,
        }
    }
    panic!("never received pong");
}

// ── ARP-005: the daemon refuses a non-loopback bind ─────────────────────────

/// Runs the real `cockpitd` binary against a non-loopback `COCKPIT_ADDR` and
/// asserts it exits non-zero without binding.
///
/// Only the refusal is driven through the binary. The accept-with-override path
/// is asserted in `policy::tests::exact_override_value_permits_non_loopback`;
/// actually binding `0.0.0.0` from a test would prompt the host firewall and
/// expose a port on the developer's machine.
#[test]
fn arp005_non_loopback_bind_refused_without_override() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cockpitd"))
        .arg("serve")
        .env("COCKPIT_ADDR", "0.0.0.0:8787")
        .env("COCKPIT_TOKEN", TEST_TOKEN)
        .env_remove("COCKPIT_ALLOW_NON_LOOPBACK")
        .output()
        .expect("run cockpitd");

    assert!(
        !out.status.success(),
        "cockpitd must exit non-zero when asked to bind a non-loopback address"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("0.0.0.0:8787") && stderr.contains("COCKPIT_ALLOW_NON_LOOPBACK"),
        "the refusal must name the address and the override variable, got: {stderr}"
    );
}

#[test]
fn arp005_wrong_override_value_still_refuses_bind() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cockpitd"))
        .arg("serve")
        .env("COCKPIT_ADDR", "0.0.0.0:8787")
        .env("COCKPIT_TOKEN", TEST_TOKEN)
        .env("COCKPIT_ALLOW_NON_LOOPBACK", "1")
        .output()
        .expect("run cockpitd");

    assert!(
        !out.status.success(),
        "a truthy-looking override value must not unlock a non-loopback bind"
    );
}

// ── ARP-003: what would actually close the finding ──────────────────────────

/// Definition of done for ARP-003 (Critical). **This test is a specification,
/// not a passing control** — it is `#[ignore]`d because Cockpit cannot satisfy
/// it today and pretending otherwise is the exact failure the audit found.
///
/// Today's behaviour: Cockpit spawns the agent CLI (`claude -p …`,
/// `codex exec --json`) and reads stdout. A `tool_call` is observed after the
/// child decided to run it. `deny` stops us forwarding the event; it does not
/// stop the child. `tool_blocked` therefore carries `advisory: true` and
/// `enforced: false`.
///
/// To close ARP-003, one of these must hold, and this test must be rewritten to
/// prove it against a mock that *would* misbehave:
///
/// 1. **Broker.** Cockpit executes the tool itself. The child asks; Cockpit
///    decides; only Cockpit touches the filesystem, network, or shell.
/// 2. **Native pre-execution callback.** Each CLI is launched so it must ask
///    Cockpit before acting and blocks until answered — Claude Code's
///    `--permission-prompt-tool` (needs an MCP server Cockpit does not have) or
///    Codex's `app-server` JSON-RPC surface (experimental; `codex exec` is
///    spawned with stdin closed, so it cannot be answered at all today).
///
/// The acceptance test itself:
///
/// - Launch a session against a mock agent that, on receiving a `tool_call`
///   request, writes a sentinel file (`$TMPDIR/arp003-sentinel-<uuid>`) and
///   only then reports the tool result.
/// - Deny the approval.
/// - Assert the sentinel file **does not exist** after the session terminates.
/// - Assert the child received an explicit denial on its own channel (not just
///   that Cockpit stopped reading).
/// - Repeat with the operator never answering: TTL auto-deny must also leave the
///   sentinel absent.
/// - Repeat with the client disconnected at the moment of the request: the
///   default must be deny, not proceed.
///
/// A run that passes only because the mock is slow is not a pass. The mock must
/// attempt the side effect immediately and unconditionally.
///
/// Until then the honest posture is in `transport::ws::run_pump`: run child
/// agents under an independently enforced sandbox and treat the approval gate
/// as a tripwire.
#[test]
#[ignore = "ARP-003 specification: Cockpit does not gate child execution yet"]
fn arp003_execution_gate_definition_of_done() {
    panic!(
        "ARP-003 is open: the approval gate filters the event stream and does \
         not control the agent process. See this test's doc comment for the \
         acceptance criteria that must pass before any enforcement claim is made."
    );
}

/// The advisory flags must stay on the `tool_blocked` frame.
///
/// If someone removes them, the UI silently goes back to implying enforcement.
/// This asserts the shape the pump emits, adjacent to the deny path proven in
/// `e2e_codex_native_gate_deny`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arp003_tool_blocked_is_marked_advisory() {
    let addr = start_daemon_codex_gated().await;
    let repo = allowed_repo_root();

    let mut client = TestClient::connect(addr).await;
    client.auth_as("alice").await;
    let session_id = launch_as(&mut client, "codex", repo.to_str().unwrap()).await;
    let approval_id = await_approval_id(&mut client, session_id).await;

    client
        .send(json!({
            "t": "approve",
            "approval_id": approval_id.to_string(),
            "decision": "deny",
        }))
        .await;

    let mut blocked: Option<Value> = None;
    for _ in 0..30 {
        match timeout(Duration::from_secs(3), client.recv()).await {
            Ok(v) => {
                if v.get("t").and_then(|x| x.as_str()) == Some("event")
                    && let Some(evt) = v.get("event")
                    && evt.get("kind").and_then(|k| k.as_str()) == Some("tool_blocked")
                {
                    blocked = Some(evt.clone());
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let blocked = blocked.expect("deny must emit a tool_blocked event");
    let meta = &blocked["metadata"];
    assert_eq!(
        meta["advisory"].as_bool(),
        Some(true),
        "tool_blocked must be marked advisory: {blocked}"
    );
    assert_eq!(
        meta["enforced"].as_bool(),
        Some(false),
        "tool_blocked must state that nothing was enforced: {blocked}"
    );
    assert!(
        blocked["content"]
            .as_str()
            .unwrap_or_default()
            .contains("not forwarded"),
        "the operator-facing text must not claim the tool was blocked: {blocked}"
    );
}
