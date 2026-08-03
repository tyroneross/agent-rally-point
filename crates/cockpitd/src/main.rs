// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `cockpitd` entrypoint.
//!
//! Usage:
//!   cockpitd serve          -- start the WebSocket server (C1)
//!   cockpitd               -- same as serve (default)
//!
//! Environment:
//!   COCKPIT_ADDR             = host:port to bind (default 127.0.0.1:8787).
//!                              Non-loopback is refused; see below.
//!   COCKPIT_ALLOW_NON_LOOPBACK = must equal "i-understand-the-risk" to bind
//!                              anything other than loopback.
//!   COCKPIT_TOKEN            = bearer token required in hello frame
//!   COCKPIT_REPO_ALLOWLIST   = colon-separated directories a session may be
//!                              launched in (default: $HOME)
//!   COCKPIT_DB               = path to SQLite database (default cockpitd.db)

use std::net::SocketAddr;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("serve");

    match subcmd {
        "serve" | "" => serve_cmd().await,
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: cockpitd [serve]");
            std::process::exit(1);
        }
    }
}

async fn serve_cmd() -> Result<()> {
    use cockpitd::{
        adapter::claude::{ClaudeAdapter, ClaudeConfig},
        audit::AuditLog,
        clock::SystemClock,
        store::Store,
        supervisor::Supervisor,
        transport::{DirectWs, Transport, build_state},
    };

    let addr: SocketAddr = std::env::var("COCKPIT_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .expect("invalid COCKPIT_ADDR");

    // ARP-005: refuse to expose a one-shared-token daemon beyond loopback
    // unless the operator has said, in words, that they know what that means.
    if let Err(refusal) = cockpitd::policy::check_bind_addr_from_env(&addr) {
        eprintln!("cockpitd: {refusal}");
        std::process::exit(1);
    }

    // Surface the repo allowlist at startup so an operator sees the boundary
    // before a launch is refused mid-session.
    let roots = cockpitd::policy::configured_roots();
    if roots.is_empty() {
        tracing::warn!(
            "no repo roots configured — every launch_session will be refused. \
             Set {}=<colon-separated directories>.",
            cockpitd::policy::REPO_ALLOWLIST_ENV
        );
    } else {
        tracing::info!(
            "sessions may be launched under: {}",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let db_path = std::env::var("COCKPIT_DB").unwrap_or_else(|_| "cockpitd.db".into());
    let audit_db_path =
        std::env::var("COCKPIT_AUDIT_DB").unwrap_or_else(|_| format!("{db_path}.audit.db"));

    // H1a: single store for sessions + events + approvals. The audit log
    // retains its own separate store (intentional isolated record).
    let store = Store::open(&db_path)?;

    let clock = SystemClock;
    let clock3 = SystemClock;

    // Default: use the real claude binary. Override via COCKPIT_CLAUDE_BIN for tests.
    let claude_bin = std::env::var("COCKPIT_CLAUDE_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));

    let adapter = ClaudeAdapter::new(ClaudeConfig {
        binary: claude_bin,
        extra_flags: vec![],
    });
    let supervisor = Supervisor::new(store, clock, adapter);
    let audit = AuditLog::open(&audit_db_path, clock3)?;

    let state = build_state(supervisor, audit);

    tracing::info!("cockpitd {} — serving on ws://{}", cockpitd::VERSION, addr);
    DirectWs::new(addr, state).serve().await
}
