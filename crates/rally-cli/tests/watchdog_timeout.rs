// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Wall-clock watchdog: a `rally` invocation whose command body blocks longer
//! than the budget MUST fail open — return within the budget, exit 0, emit the
//! named timeout JSON envelope, and leave no surviving child process.
//!
//! Regression for the 2026-05-30 leak where four `before-write` hooks sat in
//! uninterruptible (`UE`) kernel wait for 7h45m. The fix bounds every command
//! path with a hard deadline (`RALLY_HOOK_TIMEOUT_MS` / `--timeout-ms`).
//!
//! The blocking is simulated via `RALLY_TEST_BLOCK_MS`, a seam that only exists
//! in debug builds (`cfg(debug_assertions)`); these tests run against the debug
//! binary that `cargo test` builds, so the seam is live here.
//!
//! ## Hermetic rooms
//!
//! Every `rally` invocation here runs inside a throwaway temp room — a fresh
//! temp dir with a bare `.git/` (so `repo_root()` resolves there) and an
//! isolated `HOME` — mirroring the pattern in `cli_guardrails.rs`. This matters
//! for two reasons:
//!
//!  1. **Cold-ledger cost.** The committed production room (`.rally/`) carries a
//!     ~2400-fact JSONL ledger; `facts.db` is gitignored, so on a cold CI runner
//!     `rally check` must rebuild it from JSONL on first read. That rebuild plus
//!     process spawn can exceed a 3s watchdog budget and fire the fail-open
//!     envelope — making the timing-sensitive `..._does_not_break_real_command`
//!     test flap on CI (green locally where `facts.db` is already warm). An
//!     empty temp room has nothing to rebuild, so the real envelope returns
//!     fast and deterministically.
//!  2. **Ledger pollution.** `rally check before-write` writes a binding-decision
//!     audit fact. Running against the production room writes those into the
//!     committed ledger on every local test run. A temp room keeps the
//!     production ledger pristine.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A throwaway rally room: a temp cwd with a bare `.git/` plus an isolated HOME.
/// Drop cleans up both directories.
struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("watchdog-{name}-cwd"));
        let home = temp_path(&format!("watchdog-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp room .git");
        fs::create_dir_all(&home).expect("create temp home");
        Self { cwd, home }
    }

    /// A `rally` command rooted in this hermetic room (isolated cwd + HOME).
    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        cmd
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn assert_matches_schema(schema_name: &str, value: &serde_json::Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schemas")
        .join(schema_name);
    let schema: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    validate_schema(&schema, value, "$");
}

fn validate_schema(schema: &serde_json::Value, value: &serde_json::Value, path: &str) {
    if let Some(expected) = schema.get("const") {
        assert_eq!(expected, value, "schema const mismatch at {path}");
    }
    if let Some(type_schema) = schema.get("type") {
        assert!(
            type_matches(type_schema, value),
            "schema type mismatch at {path}: {value}"
        );
    }
    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("schema required used on non-object at {path}"));
        for key in required.iter().filter_map(serde_json::Value::as_str) {
            assert!(
                object.contains_key(key),
                "schema missing required key {path}.{key}"
            );
        }
    }
    if let (Some(properties), Some(object)) = (
        schema
            .get("properties")
            .and_then(serde_json::Value::as_object),
        value.as_object(),
    ) {
        for (key, property_schema) in properties {
            if let Some(child) = object.get(key) {
                validate_schema(property_schema, child, &format!("{path}.{key}"));
            }
        }
    }
}

fn type_matches(type_schema: &serde_json::Value, value: &serde_json::Value) -> bool {
    match type_schema.as_str().unwrap() {
        "object" => value.is_object(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        other => panic!("unsupported schema type {other}"),
    }
}

/// The command body sleeps 30s but the budget is 300ms → must return fast,
/// exit 0, and print a named fail-open timeout envelope, not hang.
#[test]
fn hook_that_blocks_fails_open_within_budget() {
    let room = TempRoom::new("blocks-fails-open");
    let budget_ms: u64 = 300;
    let started = Instant::now();

    let output = room
        .rally()
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

    // 3. Emitted a distinguishable timeout envelope. Exit 0 preserves the
    //    never-gate charter; ok:false says the command itself did not finish.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON on timeout");
    assert_matches_schema("agent-rally.command.watchdog.v1.json", &parsed);
    assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
    assert_eq!(parsed["product"], "rally");
    assert_eq!(parsed["command"], "watchdog");
    assert_eq!(parsed["schema"], "agent-rally.command.watchdog.v1");
    assert_eq!(
        parsed["data"]["watchdog_timeout"],
        serde_json::Value::Bool(true)
    );
    assert!(
        parsed["data"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("did not complete")),
        "timeout reason must name the incomplete command: {parsed}"
    );
    assert!(
        parsed["data"]["elapsed_ms"]
            .as_u64()
            .is_some_and(|ms| ms >= budget_ms),
        "timeout envelope must report measured elapsed_ms: {parsed}"
    );
    assert!(
        parsed.get("agent_visible").is_none()
            || parsed["agent_visible"]["present"] != serde_json::Value::Bool(true),
        "fail-open envelope must not assert an agent-visible obligation: {parsed}"
    );

    // 4. No surviving rally child: once the process exits, the kernel reaps any
    //    fd/lock/child the abandoned worker held. Confirm no rally worker
    //    process lingers shortly after exit.
    if let Ok(ps) = Command::new("ps").args(["-axww", "-o", "command"]).output()
        && ps.status.success()
    {
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
}

/// `--timeout-ms` argument takes effect and is honored just like the env var.
#[test]
fn timeout_ms_flag_is_honored() {
    let room = TempRoom::new("timeout-ms-honored");
    let started = Instant::now();
    let output = room
        .rally()
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
    let room = TempRoom::new("does-not-break-real-command");
    // Budget is generous on purpose: this test asserts the flag *parses* and
    // does not leak into the subcommand — not that the command is fast. The
    // hermetic room already removes the cold-ledger cost; the 15s budget is
    // defense-in-depth against a pathologically slow runner so a real (non
    // fail-open) envelope is what we assert on.
    let output = room
        .rally()
        .args([
            "check",
            "before-write",
            "--tool",
            "codex",
            "--path",
            "/tmp/x",
            "--json",
            "--timeout-ms",
            "15000",
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
    let room = TempRoom::new("fast-command");
    let output = room
        .rally()
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
