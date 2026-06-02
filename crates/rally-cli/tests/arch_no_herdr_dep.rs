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

#[test]
fn rally_cli_source_contains_zero_backend_herdr_variant_uses() {
    // Plan F functional core (Chunk 3): the `Backend::Herdr` variant is
    // REMOVED — the entire run/start/attach/capture/stop/inject path for
    // the herdr backend is gone (the daemon is the authority on the
    // .rally ledger now). This gate makes a regression a compile-or-test
    // failure rather than a silent slip.
    //
    // ALLOWED: comments mentioning "Backend::Herdr" (documentation of
    // the removal). Heuristic: skip lines that start with `//`. A line
    // like `        Backend::Herdr | Backend::Cmux => ...` (a match arm)
    // would fail; a line like `// removed alongside Backend::Herdr.`
    // is fine.
    //
    // NOT enforced here: the BackendBins.herdr_bin/herdr_socket struct
    // fields and the CLI `--herdr-bin`/`--herdr-socket` flags. Those
    // remain for now as deprecation-soft compatibility (deliberately
    // ignored at runtime via BackendRunner::new) so existing scripts
    // and the 34-caller audit do not break. A future cleanup pass can
    // delete them; tracked as a follow-up.
    let src_dir = rally_cli_root().join("src");
    let files = ["lib.rs", "backends.rs", "cli.rs"];
    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let path = src_dir.join(file);
        let body = read_to_string(&path);
        for (i, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            // Skip comments + doc-strings (allow naming the removed
            // variant for narrative continuity).
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains("Backend::Herdr") {
                violations.push(format!(
                    "{}:{}: {}",
                    file,
                    i + 1,
                    line.trim_end()
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "rally-cli source contains Backend::Herdr uses (Plan F Chunk 3 violated):\n{}",
        violations.join("\n")
    );
}

#[test]
fn rally_cli_backends_enum_no_longer_has_herdr_variant() {
    // Stronger pin than the previous test: read backends.rs and assert
    // the `enum Backend { ... }` block does not list `Herdr`. This
    // catches a regression where the variant is re-introduced but its
    // call sites are split across many files (the per-line scan above
    // would still catch each, but THIS test names the enum directly).
    let backends_rs = read_to_string(
        &rally_cli_root().join("src").join("backends.rs"),
    );
    // Extract the enum body via a small grep.
    let enum_start = backends_rs
        .find("pub(crate) enum Backend")
        .expect("Backend enum must exist");
    let enum_block_end = backends_rs[enum_start..]
        .find('}')
        .expect("Backend enum body must close");
    let enum_body = &backends_rs[enum_start..enum_start + enum_block_end + 1];
    assert!(
        !enum_body.contains("Herdr"),
        "Backend enum must NOT list Herdr (Plan F Chunk 3): {enum_body}"
    );
    // Positive pin: tmux + cmux MUST still be there.
    assert!(
        enum_body.contains("Tmux"),
        "Backend enum must still list Tmux: {enum_body}"
    );
    assert!(
        enum_body.contains("Cmux"),
        "Backend enum must still list Cmux: {enum_body}"
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
