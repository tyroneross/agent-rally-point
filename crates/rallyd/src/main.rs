// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rallyd` — thin entry point for the per-repo single-writer store daemon
//! (BACKLOG S-P3, ADR-03). All logic lives in `rally_cli::rallyd_core`; this
//! bin only parses args into a [`ServeConfig`] and calls
//! [`rally_cli::rallyd_core::serve`].
//!
//! Chunk A ships a MINIMAL arg surface so `cargo build -p rallyd` passes and
//! the frozen `serve` seam is exercised. Chunk B fills the real parser
//! (`--idle-exit-secs`, `--foreground`, `--repo-root`).

use rally_cli::rallyd_core::{ServeConfig, serve};
use std::path::PathBuf;

fn main() {
    let mut repo_root: Option<PathBuf> = None;
    let mut idle_exit_secs: Option<u64> = None;
    let mut foreground = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo-root" => repo_root = args.next().map(PathBuf::from),
            "--idle-exit-secs" => {
                idle_exit_secs = match args.next().and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) => Some(n),
                    None => {
                        eprintln!("rallyd: --idle-exit-secs requires a non-negative integer");
                        std::process::exit(2);
                    }
                };
            }
            "--foreground" => foreground = true,
            "-h" | "--help" => {
                println!(
                    "rallyd — per-repo single-writer store daemon\n\n\
                     USAGE:\n    rallyd [--repo-root <path>] [--idle-exit-secs <n>] [--foreground]"
                );
                return;
            }
            other => {
                eprintln!("rallyd: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let repo_root =
        repo_root.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let config = ServeConfig {
        repo_root,
        idle_exit_secs,
        foreground,
    };

    if let Err(err) = serve(config) {
        eprintln!("rallyd: {err}");
        std::process::exit(1);
    }
}
