// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # Plan F H4 — architectural lint
//!
//! The whole point of Plan F is to INVERT the herdr-era dependency: rally
//! writes the .rally ledger; the daemon SUBSCRIBES. The risk H4 calls out
//! is that under deadline pressure, this regresses to "rally calls the
//! daemon" again. This test makes that regression a compile-or-test
//! failure rather than a silent slip.
//!
//! ## The contract this test enforces
//! - **rally-cli's `command_inject` must not shell or link the `ptyd` or
//!   `herdr` binaries on its critical path.** The inject path appends a
//!   typed Directive to the ledger; the daemon picks it up.
//! - **rally-cli's Cargo manifest must not declare `ptyd` as a direct
//!   dependency.** Path deps in the workspace catch a sloppy refactor
//!   that adds `ptyd = { path = "..." }` "just for one helper".
//! - **The rally-protocol crate must NOT depend on ptyd either** (it's
//!   the shared coupling surface; if it depended on the daemon binary it
//!   would defeat the inversion).
//!
//! ## What this does NOT enforce
//! - The full `Backend::Herdr` enum removal is OUT OF SCOPE for P2 (see
//!   the F-build plan's DECISION 2026-06-02). `rally run --backend herdr`
//!   and adjacent operations (`start`, `attach`, `capture`, `stop`) keep
//!   working unchanged. The contract here scopes specifically to
//!   `command_inject`'s critical path.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from the running test binary's manifest dir to find the
/// rally-cli crate root (`crates/rally-cli/`).
fn rally_cli_root() -> PathBuf {
    // cargo sets CARGO_MANIFEST_DIR to the crate being tested.
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        p.ends_with("crates/rally-cli") || p.join("Cargo.toml").exists(),
        "CARGO_MANIFEST_DIR not where expected: {}",
        p.display()
    );
    p
}

fn workspace_root() -> PathBuf {
    rally_cli_root()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

#[test]
fn rally_cli_cargo_does_not_depend_on_ptyd() {
    // Direct dependency check: no `ptyd = ...` line in the [dependencies]
    // (or [dev-dependencies]) of rally-cli's Cargo.toml.
    let cargo = read_to_string(&rally_cli_root().join("Cargo.toml"));
    for (i, line) in cargo.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("#") {
            continue;
        }
        // Naive but accurate: any "ptyd" or "herdr" token in a dep
        // declaration would fail this. Comments and doc-strings are
        // skipped via the `starts_with("#")` guard.
        assert!(
            !trimmed.starts_with("ptyd"),
            "rally-cli/Cargo.toml line {}: rally-cli MUST NOT declare ptyd as a dependency (H4 inversion): {}",
            i + 1,
            trimmed
        );
        assert!(
            !trimmed.starts_with("herdr"),
            "rally-cli/Cargo.toml line {}: rally-cli MUST NOT declare herdr as a dependency: {}",
            i + 1,
            trimmed
        );
    }
}

#[test]
fn rally_protocol_cargo_does_not_depend_on_ptyd() {
    // The shared crate is the entire coupling surface; if IT depended on
    // ptyd, the H4 inversion would be defeated through the back door.
    let path = workspace_root()
        .join("crates")
        .join("rally-protocol")
        .join("Cargo.toml");
    let cargo = read_to_string(&path);
    for (i, line) in cargo.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("#") {
            continue;
        }
        assert!(
            !trimmed.starts_with("ptyd"),
            "rally-protocol/Cargo.toml line {}: MUST NOT declare ptyd as a dependency: {}",
            i + 1,
            trimmed
        );
        assert!(
            !trimmed.starts_with("herdr"),
            "rally-protocol/Cargo.toml line {}: MUST NOT declare herdr as a dependency: {}",
            i + 1,
            trimmed
        );
    }
}

#[test]
fn command_inject_does_not_shell_ptyd_or_herdr() {
    // Inspect the inject implementation source for any Command::new("ptyd")
    // / Command::new("herdr") invocation. The legacy backend.inject path
    // for Backend::Herdr DOES still shell out (preserved for pre-daemon
    // backward compat per the DECISION) — but command_inject itself must
    // NOT directly shell either binary; that goes through BackendRunner.
    let lib_rs = read_to_string(&rally_cli_root().join("src").join("lib.rs"));
    let inject_fn = extract_function_body(&lib_rs, "fn command_inject")
        .expect("command_inject must exist in lib.rs");
    for (i, line) in inject_fn.lines().enumerate() {
        let trimmed = line.trim();
        assert!(
            !trimmed.contains("Command::new(\"ptyd\")"),
            "command_inject line ~{}: MUST NOT shell ptyd directly (H4): {}",
            i + 1,
            trimmed
        );
        assert!(
            !trimmed.contains("Command::new(\"herdr\")"),
            "command_inject line ~{}: MUST NOT shell herdr directly (H4): {}",
            i + 1,
            trimmed
        );
    }
}

#[test]
fn inject_via_ledger_writes_to_rally_protocol_not_backend() {
    // Pin the new function exists and references rally_protocol — proves
    // the inversion is wired (not just allowed). A refactor that bypasses
    // the ledger by reverting to a backend-only path would fail this.
    let lib_rs = read_to_string(&rally_cli_root().join("src").join("lib.rs"));
    assert!(
        lib_rs.contains("fn inject_via_ledger"),
        "rally-cli must define `inject_via_ledger` (the new inverted-dep entry)"
    );
    let body = extract_function_body(&lib_rs, "fn inject_via_ledger")
        .expect("inject_via_ledger body must be extractable");
    assert!(
        body.contains("rally_protocol::"),
        "inject_via_ledger MUST go through rally_protocol — that IS the inversion. Body:\n{body}"
    );
    assert!(
        body.contains("append_directive"),
        "inject_via_ledger MUST call append_directive on the Inbox trait"
    );
}

/// Best-effort extract a `fn <name>(...)` body — finds the function
/// signature, walks `{` to balanced `}`. Returns `None` if the function
/// isn't found. Good enough for an arch lint that just greps for
/// no-no patterns.
fn extract_function_body<'a>(source: &'a str, sig_prefix: &str) -> Option<&'a str> {
    let start = source.find(sig_prefix)?;
    let after = &source[start..];
    let open = after.find('{')?;
    let body_start = start + open;
    let mut depth = 0_i32;
    let bytes = source.as_bytes();
    for i in body_start..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[body_start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}
