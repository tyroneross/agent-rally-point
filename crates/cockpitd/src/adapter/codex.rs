// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Codex adapter: drives `codex exec --json` as a managed subprocess.
//!
//! ## Surface choice (TAG:ASSUMED — recorded here per plan §B2):
//!
//!   **Chosen: `codex exec --json`** (newline-delimited JSONL events on stdout).
//!
//!   Rationale:
//!   - `codex exec --json` is a stable, documented flag confirmed via `codex exec --help`.
//!   - `codex app-server` is marked `[experimental]` and listens on `stdio://` as a
//!     JSON-RPC server — bidirectional, but we'd need to be a JSON-RPC *client*, which
//!     adds a protocol layer and RPC schema dependency. Overkill for v1.
//!   - `codex exec --json` is a simpler read-only-output loop. We send follow-up
//!     prompts via `codex exec resume <session_id> --json <prompt>` in a new process.
//!
//!   Resume: `codex exec resume <session_id> <prompt> --json`
//!
//! ## Codex JSONL event shapes (confirmed via `codex exec --help` + Codex docs):
//!   Every line is a JSON object with a `type` field (string).
//!   Known types observed: "session_started", "message", "tool_call", "tool_result",
//!   "diff", "error", "completed".
//!
//!   { "type": "session_started", "session_id": "...", ... }
//!   { "type": "message",         "role": "assistant"|"user", "content": "...", ... }
//!   { "type": "tool_call",       "tool": "...", "args": {...}, "id": "...", ... }
//!   { "type": "tool_result",     "tool_call_id": "...", "output": "...", ... }
//!   { "type": "diff",            "path": "...", "diff": "...", ... }
//!   { "type": "approval_request","id":"...", "tool":"...", "args":{...}, ... }
//!   { "type": "completed",       "exit_code": 0, ... }
//!   { "type": "error",           "message": "...", ... }
//!
//! Unknown event types are logged + skipped — never fatal.
//!
//! ## Binary path injection:
//!   Set `CodexConfig.binary` to point at the mock binary in tests.

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

// ── Config ─────────────────────────────────────────────────────────────────

/// Configuration for the Codex adapter.
#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// Path to the `codex` executable. Default: `"codex"`.
    pub binary: PathBuf,
    /// Extra flags passed after `exec`.
    pub extra_flags: Vec<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("codex"),
            extra_flags: vec![],
        }
    }
}

impl CodexConfig {
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: path.into(),
            ..Default::default()
        }
    }
}

// ── CodexAdapter ──────────────────────────────────────────────────────────────

/// Drives `codex exec --json` per session.
///
/// Initial run: `codex exec --json <prompt>`
/// Follow-on turns: `codex exec resume <session_id> --json <prompt>`
pub struct CodexAdapter {
    config: CodexConfig,
    /// Active child processes keyed by session_id.
    processes: std::collections::HashMap<Uuid, Child>,
    /// Captured Codex session/thread IDs keyed by our session_id.
    codex_session_ids: std::collections::HashMap<Uuid, String>,
}

impl CodexAdapter {
    pub fn new(config: CodexConfig) -> Self {
        Self {
            config,
            processes: Default::default(),
            codex_session_ids: Default::default(),
        }
    }

    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self::new(CodexConfig::with_binary(path))
    }
}

impl Adapter for CodexAdapter {
    fn start(
        &mut self,
        session_id: Uuid,
        _agent_type: &str,
        repo_path: &str,
        prompt: Option<&str>,
        tx: mpsc::Sender<AdapterEvent>,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.arg("exec").arg("--json");

        for flag in &self.config.extra_flags {
            cmd.arg(flag);
        }

        if let Some(p) = prompt {
            cmd.arg(p);
        }

        cmd.current_dir(repo_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("spawn codex subprocess")?;

        let stdout = child
            .stdout
            .take()
            .context("codex stdout missing after spawn")?;

        self.processes.insert(session_id, child);

        // Spawn the read task.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            read_loop(session_id, stdout, tx2).await;
        });

        Ok(())
    }

    fn send(&mut self, session_id: Uuid, text: &str) -> Result<()> {
        // Sending a new prompt to a running codex exec requires spawning a
        // resume subprocess. We fire-and-forget it (the output would overlap
        // with the original stream, so in a real implementation the supervisor
        // would close the first session and open a new one). For now this is
        // best-effort. TAG:ASSUMED: resume creates a new event stream.
        let codex_id = self
            .codex_session_ids
            .get(&session_id)
            .cloned()
            .unwrap_or_default();

        if codex_id.is_empty() {
            anyhow::bail!("no codex session_id captured for {session_id}; cannot send prompt");
        }

        let mut cmd = std::process::Command::new(&self.config.binary);
        cmd.arg("exec").arg("resume").arg(&codex_id).arg("--json").arg(text);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn().context("spawn codex resume")?;
        Ok(())
    }

    fn kill(&mut self, session_id: Uuid) -> Result<()> {
        self.codex_session_ids.remove(&session_id);
        if let Some(mut child) = self.processes.remove(&session_id) {
            let _ = child.start_kill();
        }
        Ok(())
    }
}

// ── Read loop ─────────────────────────────────────────────────────────────────

async fn read_loop(
    session_id: Uuid,
    stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<AdapterEvent>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    // We need to communicate captured session_id back for resume. We use
    // a simple in-band approach: callers that need the id can read from
    // metadata on the first event.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(v) => {
                let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match map_codex_event(session_id, event_type, &v) {
                    MapResult::Event(evt) => {
                        if tx.send(evt).await.is_err() {
                            break;
                        }
                    }
                    MapResult::Terminal(terminal) => {
                        let _ = tx.send(terminal).await;
                        break;
                    }
                    MapResult::Skip => {
                        debug!("codex: skipping unknown event type {:?}", event_type);
                    }
                }
            }
            Err(e) => {
                warn!("codex: failed to parse line as JSON: {e} — line: {line}");
            }
        }
    }

    // Ensure terminal event is always sent.
    let _ = tx.send(AdapterEvent::Completed).await;
}

enum MapResult {
    Event(AdapterEvent),
    Terminal(AdapterEvent),
    Skip,
}

/// Map a parsed Codex JSONL line to an `AdapterEvent` (or Skip).
fn map_codex_event(session_id: Uuid, event_type: &str, v: &Value) -> MapResult {
    let now = Utc::now();
    match event_type {
        "session_started" => {
            let codex_id = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "system".into(),
                kind: "status".into(),
                content: format!("codex_session_started:{}", codex_id),
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({ "codex_session_id": codex_id }),
            }))
        }

        "message" => {
            let role = v
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("assistant");
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let sender = if role == "user" { "user" } else { "agent" };
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: sender.into(),
                kind: "message".into(),
                content,
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({}),
            }))
        }

        "tool_call" => {
            let tool = v
                .get("tool")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let args = v.get("args").cloned().unwrap_or(serde_json::json!({}));
            let id = v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let content = serde_json::to_string(&args).unwrap_or_default();
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "agent".into(),
                kind: "tool_call".into(),
                content: format!("{tool}: {content}"),
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({ "tool": tool, "args": args, "id": id }),
            }))
        }

        "tool_result" => {
            let output = v
                .get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "agent".into(),
                kind: "tool_result".into(),
                content: output,
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({}),
            }))
        }

        "diff" => {
            let path = v
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let diff = v
                .get("diff")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "agent".into(),
                kind: "diff".into(),
                content: diff,
                requires_user_input: false,
                created_at: now,
                metadata: serde_json::json!({ "path": path }),
            }))
        }

        "approval_request" => {
            // Translate to an approval_request Event so the supervisor can
            // persist it and route it through the ApprovalManager.
            let approval_id = v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let tool = v
                .get("tool")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let args = v.get("args").cloned().unwrap_or(serde_json::json!({}));
            MapResult::Event(AdapterEvent::Event(Event {
                session_id,
                seq: 0,
                sender: "system".into(),
                kind: "approval_request".into(),
                content: format!("approval needed for tool: {tool}"),
                requires_user_input: true,
                created_at: now,
                metadata: serde_json::json!({
                    "approval_id": approval_id,
                    "tool": tool,
                    "args": args,
                }),
            }))
        }

        "completed" => {
            let exit_code = v
                .get("exit_code")
                .and_then(|e| e.as_i64())
                .unwrap_or(0);
            if exit_code == 0 {
                MapResult::Terminal(AdapterEvent::Completed)
            } else {
                MapResult::Terminal(AdapterEvent::Failed(format!(
                    "codex exited with code {exit_code}"
                )))
            }
        }

        "error" => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            MapResult::Terminal(AdapterEvent::Failed(msg))
        }

        _ => MapResult::Skip,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn mock_codex_path() -> PathBuf {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        PathBuf::from(manifest).join("tests/mock-bin/codex")
    }

    async fn run_mock_codex(session_id: Uuid) -> Vec<AdapterEvent> {
        let (tx, mut rx) = mpsc::channel::<AdapterEvent>(64);
        let mut adapter = CodexAdapter::with_binary(mock_codex_path());

        adapter
            .start(session_id, "codex", "/tmp", Some("test prompt"), tx)
            .expect("start mock codex");

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

    /// B2-1: mock codex emits expected event sequence.
    #[tokio::test]
    async fn mock_codex_event_sequence() {
        let session_id = Uuid::new_v4();
        let events = run_mock_codex(session_id).await;

        assert!(
            events.len() >= 3,
            "expected at least 3 events, got {}: {events:?}",
            events.len()
        );

        // First event should be session_started status.
        match &events[0] {
            AdapterEvent::Event(e) => {
                assert_eq!(e.kind, "status");
                let captured = e
                    .metadata
                    .get("codex_session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert_eq!(captured, "mock-codex-session-456");
            }
            other => panic!("expected status event, got {:?}", other),
        }

        // Expect a message event.
        let has_message = events.iter().any(|e| {
            matches!(e, AdapterEvent::Event(ev) if ev.kind == "message")
        });
        assert!(has_message, "expected message event");

        // Expect a tool_call event.
        let has_tool_call = events.iter().any(|e| {
            matches!(e, AdapterEvent::Event(ev) if ev.kind == "tool_call")
        });
        assert!(has_tool_call, "expected tool_call event");

        // Last event should be Completed.
        assert!(
            matches!(events.last().unwrap(), AdapterEvent::Completed),
            "last event should be Completed"
        );
    }

    /// B2-2: approval_request event sets requires_user_input.
    #[tokio::test]
    async fn approval_request_sets_requires_user_input() {
        let session_id = Uuid::new_v4();
        let events = run_mock_codex(session_id).await;

        let approval_evt = events.iter().find(|e| {
            matches!(e, AdapterEvent::Event(ev) if ev.kind == "approval_request")
        });
        assert!(approval_evt.is_some(), "expected approval_request event");

        if let Some(AdapterEvent::Event(e)) = approval_evt {
            assert!(
                e.requires_user_input,
                "approval_request must set requires_user_input=true"
            );
            assert!(
                e.metadata.get("approval_id").is_some(),
                "approval_request should carry approval_id in metadata"
            );
        }
    }

    /// B2-3: events carry correct session_id.
    #[tokio::test]
    async fn mock_codex_events_carry_session_id() {
        let session_id = Uuid::new_v4();
        let events = run_mock_codex(session_id).await;

        for event in &events {
            if let AdapterEvent::Event(e) = event {
                assert_eq!(e.session_id, session_id);
            }
        }
    }

    /// B2-4: unknown event types skipped.
    #[tokio::test]
    async fn unknown_event_type_skipped() {
        let v = serde_json::json!({ "type": "some_future_codex_type" });
        let result = map_codex_event(Uuid::new_v4(), "some_future_codex_type", &v);
        assert!(matches!(result, MapResult::Skip));
    }

    /// B2-live: optional live smoke (requires COCKPIT_LIVE=1).
    #[tokio::test]
    #[ignore]
    async fn live_smoke_codex() {
        if std::env::var("COCKPIT_LIVE").as_deref() != Ok("1") {
            return;
        }
        let session_id = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<AdapterEvent>(64);
        let mut adapter = CodexAdapter::new(CodexConfig::default());
        adapter
            .start(session_id, "codex", "/tmp", Some("echo hello"), tx)
            .expect("start live codex");

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            let done = matches!(&e, AdapterEvent::Completed | AdapterEvent::Failed(_));
            events.push(e);
            if done {
                break;
            }
        }
        assert!(!events.is_empty(), "live codex should emit at least one event");
    }
}
