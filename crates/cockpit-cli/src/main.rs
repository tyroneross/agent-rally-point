// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `cockpit-cli` — the headless phone stand-in that drives cockpitd over the
//! wire for end-to-end verification and manual use.
//!
//! Subcommands:
//!   cockpit-cli [--addr <ws-url>] [--token <t>] list
//!   cockpit-cli ... open <session_id> [--from-seq N]
//!   cockpit-cli ... send <session_id> <text>
//!   cockpit-cli ... approve <approval_id> allow|deny
//!   cockpit-cli ... launch <agent_type> <repo_path> [prompt]
//!
//! Defaults:
//!   --addr  ws://127.0.0.1:8787   (override: COCKPIT_ADDR)
//!   --token <empty>                (override: COCKPIT_TOKEN)

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Reusable client primitives ────────────────────────────────────────────────
// Factored out so the e2e test can import them directly.

pub struct WsClient {
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

impl WsClient {
    /// Connect to `addr` (ws://…) and authenticate with `token`.
    /// Returns the client after `hello_ok` is received.
    pub async fn connect(addr: &str, token: &str) -> Result<Self> {
        let (ws, _) = connect_async(addr)
            .await
            .context("connect to cockpitd")?;
        let (mut sink, mut stream) = ws.split();

        // Send hello.
        let hello = json!({"t": "hello", "token": token, "protocol": 1});
        sink.send(Message::Text(hello.to_string().into()))
            .await
            .context("send hello")?;

        // Wait for hello_ok.
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let v: Value = serde_json::from_str(&text).context("parse hello_ok")?;
                let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
                if t == "hello_ok" {
                    Ok(Self { sink, stream })
                } else if t == "error" {
                    bail!(
                        "auth failed: {}",
                        v.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                    )
                } else {
                    bail!("unexpected first frame: {text}")
                }
            }
            other => bail!("unexpected ws message: {other:?}"),
        }
    }

    /// Send a JSON frame.
    pub async fn send_frame(&mut self, v: &Value) -> Result<()> {
        self.sink
            .send(Message::Text(v.to_string().into()))
            .await
            .context("send frame")
    }

    /// Receive the next text frame as a JSON Value.
    pub async fn recv_frame(&mut self) -> Result<Value> {
        loop {
            match self.stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).context("parse frame");
                }
                Some(Ok(Message::Ping(_))) => continue,
                Some(Ok(Message::Close(_))) => bail!("server closed connection"),
                Some(Err(e)) => bail!("recv error: {e}"),
                None => bail!("connection closed"),
                _ => continue,
            }
        }
    }

    /// Receive frames until a matching `t` is seen. Returns that frame.
    pub async fn recv_until(&mut self, expected_t: &str) -> Result<Value> {
        loop {
            let v = self.recv_frame().await?;
            let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
            if t == expected_t {
                return Ok(v);
            }
            // Print other frames so the user can see them.
            eprintln!("[recv] {}", serde_json::to_string(&v).unwrap_or_default());
        }
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Parse global flags.
    let addr = consume_flag(&mut args, "--addr")
        .or_else(|| std::env::var("COCKPIT_ADDR").ok())
        .unwrap_or_else(|| "ws://127.0.0.1:8787".into());

    let token = consume_flag(&mut args, "--token")
        .or_else(|| std::env::var("COCKPIT_TOKEN").ok())
        .unwrap_or_default();

    if args.is_empty() {
        print_usage();
        return Ok(());
    }

    let subcmd = args.remove(0);

    match subcmd.as_str() {
        "list" => cmd_list(&addr, &token).await,
        "open" => cmd_open(&addr, &token, &args).await,
        "send" => cmd_send(&addr, &token, &args).await,
        "approve" => cmd_approve(&addr, &token, &args).await,
        "launch" => cmd_launch(&addr, &token, &args).await,
        other => {
            eprintln!("unknown subcommand: {other}");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: cockpit-cli [--addr <ws-url>] [--token <t>] <subcommand>
subcommands:
  list
  open <session_id> [--from-seq N]
  send <session_id> <text>
  approve <approval_id> allow|deny
  launch <agent_type> <repo_path> [prompt]"
    );
}

fn consume_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    for i in 0..args.len() {
        if args[i] == flag && i + 1 < args.len() {
            args.remove(i); // remove flag
            return Some(args.remove(i)); // remove value
        }
    }
    None
}

/// list — print all sessions.
async fn cmd_list(addr: &str, token: &str) -> Result<()> {
    let mut client = WsClient::connect(addr, token).await?;
    client.send_frame(&json!({"t": "list_sessions"})).await?;
    let v = client.recv_until("session_list").await?;
    let sessions = v
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    if sessions.is_empty() {
        println!("(no sessions)");
    } else {
        for s in &sessions {
            println!(
                "{} {} {} {}",
                s.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("agent_type").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("repo_path").and_then(|v| v.as_str()).unwrap_or("?"),
            );
        }
    }
    Ok(())
}

/// open <session_id> [--from-seq N] — stream events.
async fn cmd_open(addr: &str, token: &str, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: open <session_id> [--from-seq N]");
    }
    let session_id = &args[0];
    let from_seq: u64 = args
        .windows(2)
        .find(|w| w[0] == "--from-seq")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(0);

    let mut client = WsClient::connect(addr, token).await?;
    client
        .send_frame(&json!({
            "t": "open_session",
            "session_id": session_id,
            "from_seq": from_seq,
        }))
        .await?;

    loop {
        let v = client.recv_frame().await?;
        let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
        match t {
            "snapshot" => {
                let events = v
                    .get("events")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                for e in &events {
                    print_event(e);
                }
            }
            "event" => {
                if let Some(e) = v.get("event") {
                    print_event(e);
                    // Stop if terminal status.
                    let kind = e.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                    if kind == "status" {
                        let content = e.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        if content.starts_with("completed")
                            || content.starts_with("failed")
                            || content.starts_with("killed")
                        {
                            break;
                        }
                    }
                }
            }
            "session_status" => {
                let status = v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown");
                println!("[status] {status}");
                if matches!(status, "completed" | "failed" | "killed" | "disconnected") {
                    break;
                }
            }
            "error" => {
                eprintln!(
                    "error: {}",
                    v.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                );
                break;
            }
            _ => {
                eprintln!("[{}] {}", t, serde_json::to_string(&v).unwrap_or_default());
            }
        }
    }
    Ok(())
}

fn print_event(e: &Value) {
    let seq = e.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
    let kind = e.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
    let sender = e.get("sender").and_then(|s| s.as_str()).unwrap_or("?");
    let content = e.get("content").and_then(|c| c.as_str()).unwrap_or("");
    println!("[{seq}] {sender}/{kind}: {content}");
}

/// send <session_id> <text>
async fn cmd_send(addr: &str, token: &str, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: send <session_id> <text>");
    }
    let session_id = &args[0];
    let text = args[1..].join(" ");

    let mut client = WsClient::connect(addr, token).await?;
    client
        .send_frame(&json!({
            "t": "send_prompt",
            "session_id": session_id,
            "text": text,
        }))
        .await?;
    println!("sent");
    Ok(())
}

/// approve <approval_id> allow|deny
async fn cmd_approve(addr: &str, token: &str, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: approve <approval_id> allow|deny");
    }
    let approval_id = &args[0];
    let decision = &args[1];
    if decision != "allow" && decision != "deny" {
        bail!("decision must be 'allow' or 'deny'");
    }

    let mut client = WsClient::connect(addr, token).await?;
    client
        .send_frame(&json!({
            "t": "approve",
            "approval_id": approval_id,
            "decision": decision,
        }))
        .await?;
    println!("approved: {decision}");
    Ok(())
}

/// launch <agent_type> <repo_path> [prompt]
async fn cmd_launch(addr: &str, token: &str, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: launch <agent_type> <repo_path> [prompt]");
    }
    let agent_type = &args[0];
    let repo_path = &args[1];
    let prompt: Option<&str> = if args.len() > 2 {
        Some(&args[2])
    } else {
        None
    };

    let mut frame = json!({
        "t": "launch_session",
        "agent_type": agent_type,
        "repo_path": repo_path,
    });
    if let Some(p) = prompt {
        frame["prompt"] = json!(p);
    }

    let mut client = WsClient::connect(addr, token).await?;
    client.send_frame(&frame).await?;

    let v = client.recv_until("session_list").await?;
    let sessions = v
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(last) = sessions.last() {
        println!(
            "launched: {}",
            last.get("id").and_then(|v| v.as_str()).unwrap_or("?")
        );
    }
    Ok(())
}
