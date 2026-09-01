// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! Shared test support for rally-cli integration tests.
//!
//! Each module here is a reusable verification primitive — write the
//! test infra ONCE, import it everywhere. The Plan F `ChannelSandbox`
//! supersedes the earlier `HerdrSandbox` (kept on
//! `lane/herdr-harness-rust` for the daemon-side TermdSandbox to derive
//! from in P3).

pub mod channel_sandbox;
/// Envelope-shape assertions: tell a typed refusal from a watchdog timeout.
pub mod envelope;
/// The ONE way to spawn the `rally` binary from a test.
pub mod rally_cmd;
