// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Core of the per-repo `rallyd` single-writer store daemon (BACKLOG S-P3,
//! ADR-03, ADR-05).
//!
//! This module lives INSIDE `rally-cli` on purpose: the daemon dispatcher needs
//! `pub(crate)` access to the `RoomStore`/`DirectRoomStore` facade (the warm
//! pool via `fact_store_handle()`, the per-request `set_engagement_scope`, the
//! owner-lock helpers) without any store internals leaking to the public API.
//! The `crates/rallyd` bin is a thin shell that only calls [`serve`].
//!
//! ## Chunk A status — FROZEN SIGNATURE, STUB BODY
//!
//! [`ServeConfig`] and the [`serve`] signature are FINAL as of Chunk A (R5):
//! Chunk B fills the body only (owner-EX acquire → bind socket + write
//! `.addr`/pid → open the direct store + install the warm pool → accept loop +
//! single dispatcher). A mid-window signature change would break Chunk C's
//! `lib.rs` dispatch call site at merge, so any change is a stop-the-line
//! Chunk-A amendment.
//!
//! ## Why `serve` returns [`ServeError`] and not `crate::Result<()>`
//!
//! `serve` is `pub` (the `crates/rallyd` bin calls it across the crate
//! boundary). `crate::error::RallyError` is `pub(crate)`, so returning
//! `crate::Result<()>` would leak a crate-private type through a public
//! signature (E0446). [`ServeError`] is a `pub` wrapper the daemon builds from
//! internal `RallyError`s; it keeps store internals private while giving the
//! bin a first-class error type.

use std::path::PathBuf;

/// Configuration for a `rallyd` serve loop, parsed from the `crates/rallyd`
/// bin's args (or `rally daemon serve`). FROZEN in Chunk A (R5).
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Repo root whose `.rally/` the daemon owns and serves.
    pub repo_root: PathBuf,
    /// Optional idle-exit window: exit after this many idle seconds. `None`
    /// (default) = serve until signalled. Used mainly for test hygiene against
    /// orphaned daemons.
    pub idle_exit_secs: Option<u64>,
    /// Run in the foreground (log → stderr). `false` = the detached posture the
    /// `rally daemon start` parent spawns (log → `.rally/rallyd.log`). In Chunk
    /// A this only shapes logging; the field is frozen for Chunk B.
    pub foreground: bool,
}

/// Error returned by [`serve`]. A `pub` type so the `crates/rallyd` bin can
/// surface it without exposing `rally-cli`'s crate-private `RallyError`.
#[derive(Debug)]
pub struct ServeError {
    message: String,
}

impl ServeError {
    /// Construct a serve error with a human-readable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServeError {}

/// Run the `rallyd` serve loop for `config.repo_root`.
///
/// **Chunk A stub:** the signature is FINAL; the body is unimplemented and
/// returns a [`ServeError`] so `cargo build -p rallyd` passes from Chunk A
/// onward while the daemon core is filled in Chunk B. Do NOT change this
/// signature outside a stop-the-line Chunk-A amendment.
pub fn serve(config: ServeConfig) -> Result<(), ServeError> {
    let _ = config;
    Err(ServeError::new(
        "rallyd serve is not implemented in this build (Chunk A stub; Chunk B fills the body)",
    ))
}
