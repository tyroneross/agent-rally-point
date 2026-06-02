// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # `ChannelSandbox` — Plan F's daemonless ledger round-trip testbed
//!
//! Derived from the earlier `HerdrSandbox` (kept on
//! `lane/herdr-harness-rust` @ `1fdb9e9`) but DROPS the ptyd spawn — in the
//! Plan F architecture, rally-cli writes typed Directives to the `.rally`
//! ledger and the daemon (rally-termd, P3) is the consumer. P2's round-trip
//! is therefore writer → ledger → reader, no PTY in the loop.
//!
//! The point of this sandbox: a fresh scratch workspace in a temp dir,
//! RAII-cleaned, so a test can:
//! 1. Spin up a scratch rally workspace (its own `.git/`, isolated `HOME`).
//! 2. Register a managed session via `rally run --backend tmux
//!    --tmux-bin /usr/bin/true` (the existing test idiom — no real tmux
//!    needed, the session record lands in the workspace's `.rally/facts.db`).
//! 3. Invoke the REAL `rally inject` binary against that workspace.
//! 4. Read the Directive back via `rally_protocol::ledger::FileInbox` —
//!    PROVING the writer + reader agree byte-for-byte (this is the H1
//!    contract round-trip at the LIVE-binary level, not just the type
//!    level).
//! 5. Optionally append a Receipt (simulating a self-acking agent or the
//!    future daemon) and assert `rally status` (P4) would see Delivered.
//!
//! ## What this catches
//! - Wire-format regressions between the rally-cli binary writer and the
//!   `FileInbox` reader (would surface as `read_since` returning an empty
//!   vec or `InvalidData`).
//! - The new `delivery_state` envelope field landing on the JSON envelope
//!   (`data.inject.delivery_state`).
//! - Atomic-append correctness under a fresh inbox.
//!
//! ## What this DOES NOT catch
//! - Cross-process kernel file-event push (that's P3's TermdSandbox).
//! - Multi-writer concurrency (deferred to a perf-pass; P2 is single-writer
//!   on the rally side by design).
//!
//! ## Safety
//! Inherits HerdrSandbox's SAFETY ethos: a typo'd path must NEVER touch a
//! live `.rally` tree. Every sandbox root is under `std::env::temp_dir()`
//! with a uniqueness suffix; `Drop` rm-rfs only the sandbox root.
#![allow(dead_code)] // shared test support: not every test exercises every method.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rally_protocol::ledger::FileInbox;
use rally_protocol::{Directive, Inbox, Receipt};
use serde_json::Value;

/// The built `rally` binary under test (cargo wires this for integration tests).
const RALLY_BIN: &str = env!("CARGO_BIN_EXE_rally");

/// Monotonic suffix source for unique scratch roots.
static SANDBOX_SEQ: AtomicU64 = AtomicU64::new(0);

/// A `rally inject` outcome, parsed from the JSON envelope.
#[derive(Debug, Clone)]
pub struct InjectOutcome {
    /// `data.inject.delivered` — legacy bool field.
    pub delivered: bool,
    /// `data.inject.delivery_state` — Plan F's truthful state.
    pub delivery_state: String,
    /// `data.inject.directive_seq` — assigned per-inbox sequence.
    pub directive_seq: Option<u64>,
    /// `data.inject.directive_to` — logical agent id targeted.
    pub directive_to: Option<String>,
    /// Full parsed `data.inject` for richer assertions.
    pub raw: Value,
}

/// A throwaway scratch ledger root, torn down on `Drop`.
pub struct ChannelSandbox {
    /// Scratch root; removed on drop.
    root: PathBuf,
    /// `cwd` inside the sandbox where rally runs.
    cwd: PathBuf,
    /// Isolated `HOME` so rally subprocesses don't touch `~/.config`.
    home: PathBuf,
}

impl ChannelSandbox {
    /// Stand up a fresh scratch workspace.
    pub fn spawn() -> Self {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SANDBOX_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!("rally-channel-sandbox-{suffix}"));
        fs::create_dir_all(&root).expect("create sandbox root");

        let cwd = root.join("cwd");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("create scratch cwd");
        fs::create_dir_all(&home).expect("create scratch HOME");
        // .git so rally's repo_root() walk anchors here, not on the host repo.
        fs::create_dir_all(cwd.join(".git")).expect("create scratch .git");

        Self { root, cwd, home }
    }

    /// Scratch root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repo "root" rally finds.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The `.rally` directory (populated by `rally run` on first invocation).
    pub fn rally_dir(&self) -> PathBuf {
        self.cwd.join(".rally")
    }

    /// Run `rally <args>` inside this sandbox, returning the parsed JSON
    /// envelope. Panics on non-zero exit so the caller sees stderr.
    pub fn rally_json(&self, args: &[&str]) -> Value {
        let output = Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env_remove("PWD")
            .output()
            .expect("spawn rally");

        if !output.status.success() {
            panic!(
                "rally {:?} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "rally {:?} did not return JSON: {e}\nstdout:\n{}",
                args,
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Run `rally <args>` inside this sandbox WITHOUT panicking on failure.
    /// Returns the raw process output so a caller can assert a NON-zero exit
    /// (e.g. a rejected/forged input). The success path uses [`rally_json`].
    pub fn rally_try(&self, args: &[&str]) -> std::process::Output {
        Command::new(RALLY_BIN)
            .args(args)
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env_remove("PWD")
            .output()
            .expect("spawn rally")
    }

    /// Register a managed tmux-backed session named `name` using `/usr/bin/true`
    /// as the tmux stub. Returns the rally-assigned target name (e.g.
    /// `reviewer-01` for `--name reviewer`).
    pub fn add_tmux_session(&self, name: &str) -> String {
        let run = self.rally_json(&[
            "run",
            "claude",
            "--json",
            "--name",
            name,
            "--backend",
            "tmux",
            "--tmux-bin",
            "/usr/bin/true",
        ]);
        run["data"]["run"]["session"]["name"]
            .as_str()
            .expect("session.name")
            .to_string()
    }

    /// Invoke `rally inject` inside this sandbox. Returns the parsed outcome.
    pub fn inject(&self, target: &str, sender_tool: &str, text: &str) -> InjectOutcome {
        self.inject_with_flags(target, sender_tool, text, false)
    }

    /// Variant that accepts the Plan F `--urgent` flag.
    pub fn inject_with_flags(
        &self,
        target: &str,
        sender_tool: &str,
        text: &str,
        urgent: bool,
    ) -> InjectOutcome {
        let mut args: Vec<&str> = vec![
            "inject",
            target,
            "--json",
            "--text",
            text,
            "--tool",
            sender_tool,
            "--tmux-bin",
            "/usr/bin/true",
        ];
        if urgent {
            args.push("--urgent");
        }
        let envelope = self.rally_json(&args);
        let inject = envelope
            .pointer("/data/inject")
            .unwrap_or_else(|| panic!("envelope has no /data/inject: {envelope}"))
            .clone();

        InjectOutcome {
            delivered: inject
                .get("delivered")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            delivery_state: inject
                .get("delivery_state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            directive_seq: inject.get("directive_seq").and_then(Value::as_u64),
            directive_to: inject
                .get("directive_to")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw: inject,
        }
    }

    /// Read Directives via the canonical FileInbox reader (P3 daemon will use
    /// the same path).
    pub fn read_directives(&self, target: &str, after_seq: u64) -> Vec<Directive> {
        let inbox = FileInbox::open(self.rally_dir()).expect("open scratch FileInbox");
        inbox
            .read_since(target, after_seq)
            .expect("read directives")
    }

    /// Append a Receipt (simulating a self-acking agent or rally-termd
    /// posting back).
    pub fn append_receipt(&self, receipt: &Receipt) {
        let inbox = FileInbox::open(self.rally_dir()).expect("open scratch FileInbox");
        inbox.append_receipt(receipt).expect("append receipt");
    }

    /// Read Receipts back.
    pub fn read_receipts(&self, target: &str, after_ref_seq: u64) -> Vec<Receipt> {
        let inbox = FileInbox::open(self.rally_dir()).expect("open scratch FileInbox");
        inbox
            .read_receipts_since(target, after_ref_seq)
            .expect("read receipts")
    }
}

impl Drop for ChannelSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
