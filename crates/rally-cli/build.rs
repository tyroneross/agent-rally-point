// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Build script for rally-cli.
//!
//! Emits `RALLY_BUILD_ID` = `<CARGO_PKG_VERSION>+<git-short-hash>`.
//! Falls back to `<version>+nogit` when git is unavailable or the working tree
//! is not a git repo.  Never panics — a degraded stamp is still useful.

use std::process::Command;

fn main() {
    // Re-run when the git HEAD changes (HEAD pointer or packed-refs).
    // We best-effort this — if .git doesn't exist the tell-rebuild lines are
    // just ignored.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    println!("cargo:rerun-if-changed=build.rs");

    let version = env!("CARGO_PKG_VERSION");

    // Attempt to read a short git hash.  Errors (no git, not a repo, detached
    // HEAD with no commits, etc.) all fall through to the `nogit` sentinel.
    let hash = git_short_hash().unwrap_or_else(|| "nogit".to_string());

    let build_id = format!("{version}+{hash}");
    println!("cargo:rustc-env=RALLY_BUILD_ID={build_id}");
}

/// Run `git rev-parse --short HEAD` and return the trimmed output, or `None`
/// on any failure (command not found, non-zero exit, empty output).
fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
