// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Agent Cockpit daemon library.
//!
//! Supervises Claude Code and Codex sessions on an always-on Mac and serves a
//! normalized, sequence-numbered event stream to clients (the iOS app, or the
//! `cockpit-cli` phone stand-in) over a WebSocket bound to the loopback /
//! tailnet interface.
//!
//! Module map (built chunk-by-chunk per docs/plans/2026-05-31-ios-agent-cockpit-PLAN.md):
//! - `protocol`   — wire envelope + schema (A1)
//! - `model`      — Session / Event / Approval domain types (A1)
//! - `store`      — SQLite event store with replay-from-seq (A2)
//! - `clock`      — injectable Clock trait (A3)
//! - `supervisor` — session lifecycle + status state machine (A3)
//! - `adapter`    — agent adapters: Claude, Codex (B1/B2)
//! - `approval`   — pending-approval state machine with TTL/auto-deny (B3)
//! - `transport`  — WebSocket server + auth + Transport trait (C1)

/// Crate version string, surfaced in the daemon hello banner and protocol handshake.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── A1 ────────────────────────────────────────────────────────────────────────
pub mod model;
pub mod protocol;

// ── A2 ────────────────────────────────────────────────────────────────────────
pub mod store;

// ── A3 ────────────────────────────────────────────────────────────────────────
pub mod clock;
pub mod supervisor;

// ── B1/B2 ─────────────────────────────────────────────────────────────────────
pub mod adapter;

// ── B3 ────────────────────────────────────────────────────────────────────────
pub mod approval;

// ── C1 ────────────────────────────────────────────────────────────────────────
pub mod transport;

// ── F2 ────────────────────────────────────────────────────────────────────────
pub mod audit;

// ── F3 ────────────────────────────────────────────────────────────────────────
pub mod authz;

// ── F4 ────────────────────────────────────────────────────────────────────────
pub mod crypto;

#[cfg(test)]
mod smoke {
    #[test]
    fn version_is_nonempty() {
        assert!(!super::VERSION.is_empty());
    }
}
