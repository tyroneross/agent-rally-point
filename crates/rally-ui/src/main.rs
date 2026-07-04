// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally-ui` entrypoint.
//!
//! Usage:
//!   rally-ui serve   -- start the localhost dashboard (default)
//!   rally-ui         -- same as serve
//!
//! Environment:
//!   RALLY_UI_ADDR      = host:port to bind (default 127.0.0.1:8899)
//!   RALLY_UI_RALLY_BIN = path/name of the `rally` binary to spawn per room
//!                        (default "rally", resolved from PATH)

mod registry;
mod room_source;
mod server;

use std::sync::Arc;

use anyhow::{Context, Result};

const DEFAULT_ADDR: &str = "127.0.0.1:8899";
const DEFAULT_RALLY_BIN: &str = "rally";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcmd = args.first().map(String::as_str).unwrap_or("serve");

    match subcmd {
        "serve" | "" => serve().await,
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: rally-ui [serve]");
            std::process::exit(1);
        }
    }
}

async fn serve() -> Result<()> {
    let addr = std::env::var("RALLY_UI_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let rally_bin =
        std::env::var("RALLY_UI_RALLY_BIN").unwrap_or_else(|_| DEFAULT_RALLY_BIN.to_string());

    let state = server::AppState {
        rally_bin: Arc::new(rally_bin),
    };
    let app = server::router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("rally-ui listening on http://{addr}");
    tracing::info!(%addr, "rally-ui listening");

    axum::serve(listener, app)
        .await
        .context("serving rally-ui")?;
    Ok(())
}
