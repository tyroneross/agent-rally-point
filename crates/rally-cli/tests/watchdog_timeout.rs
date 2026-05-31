// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Wall-clock watchdog: a `rally` invocation whose command body blocks longer
//! than the budget MUST fail open — return within the budget, exit 0, emit the
//! neutral JSON envelope, and leave no surviving child process.
//!
//! Regression for the 2026-05-30 leak where four `before-write` hooks sat in
//! uninterruptible (`UE`) kernel wait for 7h45m. The fix bounds every command
//! path with a hard deadline (`RALLY_HOOK_TIMEOUT_MS` / `--timeout-ms`).
//!
//! The blocking is simulated via `RALLY_TEST_BLOCK_MS`, a seam that only exists
//! in debug builds (`cfg(debug_assertions)`); these tests run against the debug
//! binary that `cargo test` builds, so the seam is live here.

use std::process::Command;
use std::time::{Duration, Instant};

fn rally() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rally"))
}

/// The command body sleeps 30s but the budget is 300ms → must return fast,
/// exit 0, and print the neutral envelope, not hang.
#[test]
fn hook_that_blocks_fails_open_within_budget() {
    let budget_ms: u64 = 300;
    let started = Instant::now();

    let output = rally()
        .args([
            "check",
            "before-write",
            "--tool",
            "codex",
            "--json",
            "--fail-open",
        ])
        .env("RALLY_TEST_BLOCK_MS", "30000")
        .env("RALLY_HOOK_TIMEOUT_MS", budget_ms.to_string())
        .output()
        .expect("spawn rally");

    let elapsed = started.elapsed();

    // 1. Returned well within a small multiple of the budget (not after 30s).
    assert!(
        elapsed < Duration::from_secs(5),
        "watchdog did not fire: invocation took {elapsed:?} (budget {budget_ms}ms)"
    );

    // 2. Exited 0 (fail-open: never block the host tool).
    assert!(
        output.status.success(),
        "expected exit 0 on timeout, got {:?}; stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // 3. Emitted the neutral JSON envelope on stdout that the hook wrappers
    //    parse as "nothing to do" (no `agent_visible.present`).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON on timeout");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
    assert_eq!(parsed["product"], "rally");
    assert!(
        parsed.get("agent_visible").is_none()
            || parsed["agent_visible"]["present"] != serde_json::Value::Bool(true),
        "neutral envelope must not assert an agent-visible obligation: {parsed}"
    );

    // 4. No surviving rally child: once the process exits, the kernel reaps any
    //    fd/lock/child the abandoned worker held. Confirm no rally worker
    //    process lingers shortly after exit.
    let ps = Command::new("ps")
        .args(["-axww", "-o", "command"])
        .output()
        .expect("ps");
    let listing = String::from_utf8_lossy(&ps.stdout);
    let survivors = listing
        .lines()
        .filter(|l| l.contains("RALLY_TEST_BLOCK_MS") || l.contains("rally-command"))
        .count();
    assert_eq!(
        survivors, 0,
        "watchdog left a surviving rally worker process behind"
    );
}

/// `--timeout-ms` argument takes effect and is honored just like the env var.
#[test]
fn timeout_ms_flag_is_honored() {
    let started = Instant::now();
    let output = rally()
        .args([
            "check",
            "before-write",
            "--tool",
            "codex",
            "--json",
            "--timeout-ms",
            "200",
        ])
        .env("RALLY_TEST_BLOCK_MS", "30000")
        .output()
        .expect("spawn rally");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "--timeout-ms not honored: took {elapsed:?}"
    );
    assert!(output.status.success(), "expected exit 0 on timeout");
}

/// `--timeout-ms` is consumed by the watchdog and must NOT leak into the
/// subcommand parser (which would reject it as an unknown argument).
#[test]
fn timeout_ms_flag_does_not_break_real_command() {
    let output = rally()
        .args([
            "check",
            "before-write",
            "--tool",
            "codex",
            "--path",
            "/tmp/x",
            "--json",
            "--timeout-ms",
            "3000",
        ])
        .output()
        .expect("spawn rally");
    assert!(
        output.status.success(),
        "--timeout-ms leaked into the command parser; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"command\"") && stdout.contains("check"),
        "expected a real check envelope, got: {stdout}"
    );
}

/// A fast command under the budget behaves exactly as before — real output,
/// real exit code, no watchdog interference.
#[test]
fn fast_command_is_unaffected() {
    let output = rally()
        .args(["version", "--json"])
        .env("RALLY_HOOK_TIMEOUT_MS", "3000")
        .output()
        .expect("spawn rally");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rally") || stdout.contains("version") || stdout.contains("build"),
        "version output unexpectedly empty: {stdout}"
    );
}
