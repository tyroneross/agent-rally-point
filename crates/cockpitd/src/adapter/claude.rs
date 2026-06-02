// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Claude adapter: drives `claude -p --output-format stream-json
//! --input-format stream-json --verbose` as a managed subprocess.
//!
//! ## Real CLI surface (confirmed via `claude -p --help`):
//!   claude -p <prompt>
//!          --output-format stream-json
//!          --input-format stream-json
//!          --verbose
//!          --resume <session_id>      (to continue a prior session)
//!          --session-id <uuid>        (use a specific session ID)
//!
//! ## Stream-JSON event shapes (from Claude docs / COCKPIT-WIRE.md):
//!   { "type": "system",    "subtype": "init",   "session_id": "...", ... }
//!   { "type": "assistant", "message": { "content": [...] }, ... }
//!   { "type": "user",      "message": { "content": [...] }, ... }
//!   { "type": "result",    "result": "...", "is_error": bool, ... }
//!
//! Unknown event types are logged + skipped — never fatal (COCKPIT-WIRE §3).
//!
//! ## Binary path injection (for tests):
//!   Set `ClaudeConfig.binary` to the path of the mock shell script.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::model::Event;
use crate::supervisor::{Adapter, AdapterEvent};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the Claude adapter.
///
/// `binary` defaults to `"claude"` (resolved via PATH). Override to point at
/// the mock binary in tests.
#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    /// Path to the `claude` executable. Default: `"claude"`.
    pub binary: PathBuf,
    /// Extra flags to pass (e.g. `--dangerously-skip-permissions`).
    pub extra_flags: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            extra_flags: vec![],
        }
    }
}

impl ClaudeConfig {
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: path.into(),
            ..Default::default()
        }
    }
}

// ── ClaudeAdapter ─────────────────────────────────────────────────────────────

/// Drives a `claude` subprocess per session.
///
/// Each `start()` call spawns one process. `send()` writes a stream-json
/// frame to stdin. The read loop runs in a tokio task and pushes `AdapterEvent`s
/// onto the channel until the process exits.
pub struct ClaudeAdapter {
    config: ClaudeConfig,
    /// Active child processes keyed by session_id.
    processes: std::collections::HashMap<Uuid, Child>,
    /// stdin handles keyed by session_id (separate because `Child` consumes stdin).
    stdin_handles: std::collections::HashMap<Uuid, tokio::process::ChildStdin>,
}

impl ClaudeAdapter {
    pub fn new(config: ClaudeConfig) -> Self {
        Self {
            config,
            processes: Default::default(),
            stdin_handles: Default::default(),
        }
    }

    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self::new(ClaudeConfig::with_binary(path))
    }
}

impl Adapter for ClaudeAdapter {
    fn start(
        &mut self,
        session_id: Uuid,
        _agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        tx: mpsc::Sender<AdapterEvent>,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.arg("-p")
            .arg("--output-format").arg("stream-json")
            .arg("--input-format").arg("stream-json")
            .arg("--verbose");

        for flag in &self.config.extra_flags {
            cmd.arg(flag);
        }

        if let Some(p) = prompt {
            cmd.arg(p);
        }

        // Set cwd to repo_path so the agent operates in the right workspace.
        cmd.current_dir(repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("spawn claude subprocess")?;

        let stdin = child
            .stdin
            .take()
            .context("claude stdin missing after spawn")?;
        let stdout = child
            .stdout
            .take()
            .context("claude stdout missing after spawn")?;

        self.stdin_handles.insert(session_id, stdin);
        // We store the child but don't await it here — the read task holds
        // the process alive. On kill() we drop/abort the child.
        self.processes.insert(session_id, child);

        // Spawn the read task.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            read_loop(session_id, stdout, tx2).await;
        });

        Ok(())
    }

    fn send(&mut self, session_id: Uuid, text: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        // Build the stream-json input frame before borrowing stdin.
        let frame = serde_json::json!({ "type": "user", "message": text });
        let mut line = serde_json::to_string(&frame).unwrap();
        line.push('\n');
        let bytes = line.into_bytes();

        let stdin = self
            .stdin_handles
            .get_mut(&session_id)
            .context("no stdin for session (not started or already dead)")?;

        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(h) => h.block_on(async { stdin.write_all(&bytes).await })
                .context("write to claude stdin"),
            Err(_) => anyhow::bail!("send() called outside tokio runtime"),
        }
    }

    fn kill(&mut self, session_id: Uuid) -> Result<()> {
        self.stdin_handles.remove(&session_id);
        if let Some(mut child) = self.processes.remove(&session_id) {
            // start_kill is the safe async-friendly way; ignore errors (process
            // may have already exited).
            let _ = child.start_kill();
        }
        Ok(())
    }
}

// ── Read loop ─────────────────────────────────────────────────────────────────

/// Reads newline-delimited stream-json from Claude's stdout and pushes
/// `AdapterEvent`s to `tx`. Terminates when the process closes stdout.
async fn read_loop(
    session_id: Uuid,
    stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<AdapterEvent>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(v) => {
                let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let evts = map_claude_events(session_id, event_type, &v);
                if evts.is_empty() {
                    debug!("claude: skipping unknown event type {:?}", event_type);
                }
                for evt in evts {
                    let done = matches!(&evt, AdapterEvent::Completed | AdapterEvent::Failed(_));
                    if tx.send(evt).await.is_err() {
                        return; // Receiver dropped — supervisor is gone.
                    }
                    if done {
                        return;
                    }
                }
            }
            Err(e) => {
                warn!("claude: failed to parse line as JSON: {e} — line: {line}");
            }
        }
    }

    // Process closed stdout (normal exit or kill). Send terminal status.
    let _ = tx.send(AdapterEvent::Completed).await;
}

/// Map a parsed Claude stream-json object to zero or more `AdapterEvent`s.
///
/// Returns an empty `Vec` for event types we don't handle (logged + skipped by
/// caller).  An `assistant` turn with N content blocks produces N events so
/// that every block gets its own persisted row with a monotonic seq number.
///
/// Claude stream-json format (confirmed from `claude --verbose` output):
/// - `system` with `subtype:"init"` carries `session_id`
/// - `assistant` carries `message.content` (array of blocks)
/// - `result` with `is_error:true` → Failed; `is_error:false` → terminal status
pub(crate) fn map_claude_events(session_id: Uuid, event_type: &str, v: &Value) -> Vec<AdapterEvent> {
    let now = Utc::now();
    match event_type {
        "system" => {
            // Capture session_id from init event and emit a system status event.
            let captured_session_id = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            if subtype == "init" {
                let event = Event {
                    session_id,
                    seq: 0, // assigned by store
                    sender: "system".into(),
                    kind: "status".into(),
                    content: format!("session_started:{}", captured_session_id),
                    requires_user_input: false,
                    created_at: now,
                    metadata: serde_json::json!({ "claude_session_id": captured_session_id }),
                };
                vec![AdapterEvent::Event(event)]
            } else {
                vec![]
            }
        }

        "assistant" => {
            // Extract ALL content blocks from message.content array.
            // Each block becomes its own AdapterEvent so every block gets a
            // distinct persisted row with a monotonic seq number (G2).
            let content_blocks = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            if let Some(blocks) = content_blocks {
                let mut events = Vec::new();
                for block in blocks {
                    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            let text = block
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(AdapterEvent::Event(Event {
                                session_id,
                                seq: 0,
                                sender: "agent".into(),
                                kind: "message".into(),
                                content: text,
                                requires_user_input: false,
                                created_at: now,
                                metadata: serde_json::json!({}),
                            }));
                        }
                        "tool_use" => {
                            let tool_name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let tool_id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or(serde_json::json!({}));
                            let content = serde_json::to_string(&input).unwrap_or_default();
                            events.push(AdapterEvent::Event(Event {
                                session_id,
                                seq: 0,
                                sender: "agent".into(),
                                kind: "tool_call".into(),
                                content: format!("{tool_name}: {content}"),
                                requires_user_input: false,
                                created_at: now,
                                metadata: serde_json::json!({
                                    "tool_id": tool_id,
                                    "tool_name": tool_name,
                                    "input": input,
                                }),
                            }));
                        }
                        _ => {
                            debug!("claude: skipping unknown content block type {:?}", block_type);
                        }
                    }
                }
                events
            } else {
                vec![]
            }
        }

        "user" => {
            // User turn echoed back — we don't need to re-emit this.
            vec![]
        }

        "tool_result" => {
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            vec![AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "agent".into(),
                kind: "tool_result".into(),
                content,
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({}),
            })]
        }

        "result" => {
            // Terminal event from claude -p.
            let is_error = v
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            if is_error {
                let msg = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                vec![AdapterEvent::Failed(msg)]
            } else {
                vec![AdapterEvent::Completed]
            }
        }

        _ => vec![], // unknown type: caller logs + skips
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn mock_claude_path() -> PathBuf {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        PathBuf::from(manifest).join("tests/mock-bin/claude")
    }

    /// Drive the mock claude and collect all AdapterEvents until the channel closes.
    async fn run_mock_claude(session_id: Uuid) -> Vec<AdapterEvent> {
        let (tx, mut rx) = mpsc::channel::<AdapterEvent>(64);
        let mut adapter = ClaudeAdapter::with_binary(mock_claude_path());

        // Use /tmp as repo_path so it always exists.
        adapter
            .start(session_id, "claude", "/tmp", Some("test prompt"), tx)
            .expect("start mock claude");

        let mut events = Vec::new();
        loop {
            match rx.recv().await {
                Some(e) => {
                    let done = matches!(&e, AdapterEvent::Completed | AdapterEvent::Failed(_));
                    events.push(e);
                    if done {
                        break;
                    }
                }
                None => break,
            }
        }
        events
    }

    /// B1-1: mock claude emits expected event sequence.
    #[tokio::test]
    async fn mock_claude_event_sequence() {
        let session_id = Uuid::new_v4();
        let events = run_mock_claude(session_id).await;

        // Expect: system/init → message → tool_call → Completed
        assert!(
            events.len() >= 3,
            "expected at least 3 events, got {}: {events:?}",
            events.len()
        );

        // First event: status from system/init with session_id captured.
        let first = &events[0];
        match first {
            AdapterEvent::Event(e) => {
                assert_eq!(e.kind, "status", "first event should be status (system init)");
                assert!(
                    e.metadata.get("claude_session_id").is_some(),
                    "system init should capture claude_session_id"
                );
                let captured = e
                    .metadata
                    .get("claude_session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert_eq!(
                    captured, "mock-session-123",
                    "captured session_id should match mock"
                );
            }
            other => panic!("expected status event, got {:?}", other),
        }

        // Check that we have a message event somewhere.
        let has_message = events.iter().any(|e| {
            matches!(e, AdapterEvent::Event(ev) if ev.kind == "message")
        });
        assert!(has_message, "expected at least one message event");

        // Check that we have a tool_call event.
        let has_tool_call = events.iter().any(|e| {
            matches!(e, AdapterEvent::Event(ev) if ev.kind == "tool_call")
        });
        assert!(has_tool_call, "expected at least one tool_call event");

        // Last event should be Completed.
        let last = events.last().unwrap();
        assert!(
            matches!(last, AdapterEvent::Completed),
            "last event should be Completed, got {:?}",
            last
        );
    }

    /// B1-2: all events carry the correct session_id.
    #[tokio::test]
    async fn mock_claude_events_carry_session_id() {
        let session_id = Uuid::new_v4();
        let events = run_mock_claude(session_id).await;

        for event in &events {
            if let AdapterEvent::Event(e) = event {
                assert_eq!(
                    e.session_id, session_id,
                    "event session_id mismatch: {:?}",
                    e
                );
            }
        }
    }

    /// B1-3: unknown event types are skipped (no panic, no error).
    #[tokio::test]
    async fn unknown_event_types_are_skipped() {
        let v = serde_json::json!({ "type": "some_future_type", "data": "x" });
        let result = map_claude_events(Uuid::new_v4(), "some_future_type", &v);
        assert!(result.is_empty(), "unknown event type should map to empty vec");
    }

    /// B1-4: result with is_error:true maps to Failed.
    #[tokio::test]
    async fn result_is_error_maps_to_failed() {
        let v = serde_json::json!({ "type": "result", "is_error": true, "result": "something broke" });
        let result = map_claude_events(Uuid::new_v4(), "result", &v);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], AdapterEvent::Failed(_)));
    }

    /// B1-5: result with is_error:false maps to Completed.
    #[tokio::test]
    async fn result_ok_maps_to_completed() {
        let v = serde_json::json!({ "type": "result", "is_error": false, "result": "done" });
        let result = map_claude_events(Uuid::new_v4(), "result", &v);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], AdapterEvent::Completed));
    }

    /// G2-1: assistant turn with text + two tool_use blocks yields 3 events with correct kinds.
    #[tokio::test]
    async fn multi_block_assistant_turn_yields_all_events() {
        let session_id = Uuid::new_v4();
        let v = serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "msg_multi",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "I'll do two things." },
                    { "type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "/a"} },
                    { "type": "tool_use", "id": "t2", "name": "write_file", "input": {"path": "/b", "content": "x"} }
                ]
            }
        });
        let events = map_claude_events(session_id, "assistant", &v);
        assert_eq!(events.len(), 3, "expected 3 events: 1 message + 2 tool_call");

        // kinds in order
        let kinds: Vec<&str> = events.iter().map(|e| match e {
            AdapterEvent::Event(ev) => ev.kind.as_str(),
            _ => "terminal",
        }).collect();
        assert_eq!(kinds, vec!["message", "tool_call", "tool_call"]);

        // tool names in metadata
        if let AdapterEvent::Event(ev1) = &events[1] {
            assert_eq!(ev1.metadata["tool_name"], "read_file");
        }
        if let AdapterEvent::Event(ev2) = &events[2] {
            assert_eq!(ev2.metadata["tool_name"], "write_file");
        }
    }

    /// B1-live: optional live smoke (requires COCKPIT_LIVE=1).
    #[tokio::test]
    #[ignore]
    async fn live_smoke_claude() {
        if std::env::var("COCKPIT_LIVE").as_deref() != Ok("1") {
            return;
        }
        let session_id = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<AdapterEvent>(64);
        let mut adapter = ClaudeAdapter::new(ClaudeConfig::default());
        adapter
            .start(
                session_id,
                "claude",
                "/tmp",
                Some("Say 'hello cockpit' and nothing else."),
                tx,
            )
            .expect("start live claude");

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            let done = matches!(&e, AdapterEvent::Completed | AdapterEvent::Failed(_));
            events.push(e);
            if done {
                break;
            }
        }
        assert!(!events.is_empty(), "live claude should emit at least one event");
    }
}
