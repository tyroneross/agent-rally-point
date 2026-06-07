// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Two concerns covered together because they share the same workspace
//! plumbing and the same `RALLY_TEST_BLOCK_MS` / `RALLY_HOOK_TIMEOUT_MS`
//! seam:
//!
//! 1. **Warm snapshot perf.** After a single `rally check before-write`
//!    populates `.rally/snapshot.cache.json`, subsequent invocations
//!    should hit the cache and return well under the 3s default watchdog
//!    budget — even on a CI box without controlling background load. We
//!    assert a generous 1s bound so the test is robust to spawn cost and
//!    macOS code-signing overhead; the real target is "fast enough that
//!    the watchdog never trips warmly", and 1s on idle CI is a clean
//!    regression signal vs. the 3050–8118ms observed under contention
//!    pre-fix.
//!
//! 2. **Fail-closed posture for before-write only.** When
//!    `RALLY_BEFORE_WRITE_FAILCLOSED=1` is set and the watchdog fires on
//!    `check before-write`, rally must emit a STOP envelope and exit 4
//!    instead of the neutral allow-everything envelope. Read-only
//!    commands (`rally room`, `rally next`, etc.) must STILL fail open on
//!    the same env var — fail-closed on a stuck advisory poll would
//!    wedge agents.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn run_envs(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .args(args);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
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

// -------------------------------------------------------------------------
// 1. Warm snapshot perf
// -------------------------------------------------------------------------

#[test]
fn warm_before_write_check_is_fast() {
    let ws = Workspace::new("rally-warm-perf");

    // Seed a real-shape room: one tool entered, one claim made by a
    // different tool (so the cache must cover both presence + claim
    // projections).
    assert!(
        ws.run(&[
            "say",
            "claim",
            "--json",
            "--tool",
            "peer",
            "--path",
            "src/lib.rs",
            "--subject",
            "peer owns lib",
        ])
        .status
        .success(),
        "seed claim must succeed"
    );

    // FIRST call: cold — populates `snapshot.cache.json`. This is the slow
    // path; we don't assert a wall-clock bound on it (it's the very thing
    // the cache exists to bypass).
    let cold = ws.run(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/other.rs",
    ]);
    assert!(cold.status.success(), "cold check must succeed");

    // Cache file should now exist after the cold path wrote it.
    let cache_path = ws.cwd.join(".rally").join("snapshot.cache.json");
    assert!(
        cache_path.exists(),
        "slow path must populate .rally/snapshot.cache.json"
    );

    // WARM calls: 5 invocations from the same tool, no intervening
    // mutation. Each must hit the cache and return well under the 3s
    // watchdog budget. We assert a 1s wall-clock bound per invocation —
    // generous enough to absorb test-binary spawn cost on CI, tight
    // enough to catch a regression where the cache is bypassed.
    let mut max_warm = Duration::from_millis(0);
    for i in 0..5 {
        let started = Instant::now();
        let warm = ws.run(&[
            "check",
            "before-write",
            "--json",
            "--tool",
            "codex:01",
            "--path",
            "src/other.rs",
        ]);
        let elapsed = started.elapsed();
        assert!(
            warm.status.success(),
            "warm check #{i} must succeed: stderr={}",
            String::from_utf8_lossy(&warm.stderr)
        );
        if elapsed > max_warm {
            max_warm = elapsed;
        }
    }
    assert!(
        max_warm < Duration::from_secs(1),
        "warm before-write check must return inside 1s, observed max {max_warm:?}"
    );

    // Text-mode output marks the cached branch — proves the fast path is
    // actually firing rather than the slow path happening to be fast on
    // an empty-ish room.
    let text = ws.run(&[
        "check",
        "before-write",
        "--tool",
        "codex:01",
        "--path",
        "src/other.rs",
    ]);
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains("(cached)"),
        "warm text mode must mark the cached branch; got: {stdout}"
    );

    ws.cleanup();
}

#[test]
fn snapshot_cache_invalidates_on_mutation() {
    // A writer's append must invalidate the read-side cache so a
    // subsequent reader observes the new fact, not the cached snapshot.
    let ws = Workspace::new("rally-cache-invalidate");

    // Warm up the cache with a known tool.
    let _ = ws.run(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/x.rs",
    ]);
    let cache_path = ws.cwd.join(".rally").join("snapshot.cache.json");
    assert!(cache_path.exists());

    // Now a different agent claims the same path the reader will check
    // next. If the read path silently used the stale cache it would
    // report `allow=true`; instead it must see the new claim.
    let claim = ws.run(&[
        "say",
        "claim",
        "--json",
        "--tool",
        "claude_code:01",
        "--path",
        "src/x.rs",
        "--subject",
        "claude owns x",
    ]);
    assert!(claim.status.success());

    let check = ws.run(&[
        "check",
        "before-write",
        "--json",
        "--tool",
        "codex:01",
        "--path",
        "src/x.rs",
    ]);
    assert!(check.status.success());
    let body: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(
        body["data"]["check"]["allow"], false,
        "cache must have invalidated on the claim append; body={body}"
    );
    let findings = body["data"]["check"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["code"] == "claimed-path"),
        "post-mutation read must surface the new claimed-path stop: {findings:#?}"
    );

    ws.cleanup();
}

// -------------------------------------------------------------------------
// 2. Fail-closed posture (opt-in, before-write only)
// -------------------------------------------------------------------------

#[test]
fn before_write_fails_closed_on_watchdog_timeout_when_configured() {
    let ws = Workspace::new("rally-failclosed-before-write");

    let started = Instant::now();
    let output = ws.run_envs(
        &[
            "check",
            "before-write",
            "--json",
            "--tool",
            "codex",
            "--path",
            "src/lib.rs",
            "--strict",
        ],
        &[
            // Force the watchdog to fire.
            ("RALLY_TEST_BLOCK_MS", "5000"),
            ("RALLY_HOOK_TIMEOUT_MS", "200"),
            // Opt in to fail-closed for this command shape.
            ("RALLY_BEFORE_WRITE_FAILCLOSED", "1"),
        ],
    );
    let elapsed = started.elapsed();

    // 1. Returned well within the watchdog budget — the fail-closed path
    //    must NOT block longer than the timeout (defense against a
    //    regression where the synthesized envelope is computed on the
    //    slow side of the timeout).
    assert!(
        elapsed < Duration::from_secs(3),
        "fail-closed timeout took too long: {elapsed:?}"
    );

    // 2. Exit code 4 = strict mode + stop finding (mirrors the real
    //    before-write strict exit on a claimed-path collision).
    assert_eq!(
        output.status.code(),
        Some(4),
        "fail-closed on timeout must exit 4; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 3. Stdout must carry a stop-shaped envelope with `allow:false` and
    //    a `watchdog-timeout-fail-closed` finding. The wrappers route on
    //    `agent_visible.present == true` + the finding code.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value =
        serde_json::from_str(stdout.trim()).expect("fail-closed stdout must be JSON");
    assert_eq!(parsed["data"]["check"]["allow"], Value::Bool(false));
    assert_eq!(parsed["data"]["check"]["phase"], "before-write");
    assert_eq!(
        parsed["data"]["check"]["agent_visible"]["present"],
        Value::Bool(true)
    );
    let findings = parsed["data"]["check"]["findings"]
        .as_array()
        .expect("findings array");
    assert!(
        findings
            .iter()
            .any(|f| f["code"] == "watchdog-timeout-fail-closed" && f["severity"] == "stop"),
        "synthesized fail-closed finding missing: {findings:#?}"
    );

    // 4. Stderr carries the human-readable explanation so an operator
    //    debugging a blocked write knows why.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failing CLOSED") || stderr.contains("fail-closed"),
        "stderr must explain the closed posture: {stderr}"
    );

    ws.cleanup();
}

#[test]
fn read_only_command_still_fails_open_on_watchdog_timeout() {
    // The fail-closed posture is scoped to `check before-write` ONLY. A
    // stuck `rally room` must still fail open — that command is purely
    // advisory and fail-closed on it would block an agent from polling.
    let ws = Workspace::new("rally-failopen-readonly");

    let output = ws.run_envs(
        &["room", "--json"],
        &[
            ("RALLY_TEST_BLOCK_MS", "5000"),
            ("RALLY_HOOK_TIMEOUT_MS", "200"),
            // Even with fail-closed opt-in, read-only commands stay open.
            ("RALLY_BEFORE_WRITE_FAILCLOSED", "1"),
        ],
    );

    // Exit 0 (fail-open), neutral envelope, never a strict-mode 4.
    assert!(
        output.status.success(),
        "read-only command must fail open even when RALLY_BEFORE_WRITE_FAILCLOSED is set; code={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("stdout JSON");
    // Neutral envelope shape: no `agent_visible.present == true`.
    let agent_visible_present = parsed
        .get("agent_visible")
        .and_then(|v| v.get("present"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || parsed
            .get("data")
            .and_then(|d| d.get("check"))
            .and_then(|c| c.get("agent_visible"))
            .and_then(|v| v.get("present"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    assert!(
        !agent_visible_present,
        "read-only fail-open must not synthesize an agent_visible.present finding: {parsed}"
    );

    ws.cleanup();
}

#[test]
fn before_write_default_still_fails_open() {
    // Sanity: without the env var, `check before-write` still fails OPEN
    // on a watchdog timeout. The new fail-closed path is strictly
    // opt-in; existing callers see the legacy behavior.
    let ws = Workspace::new("rally-failopen-default");
    let output = ws.run_envs(
        &[
            "check",
            "before-write",
            "--json",
            "--tool",
            "codex",
            "--path",
            "src/lib.rs",
            "--strict",
        ],
        &[
            ("RALLY_TEST_BLOCK_MS", "5000"),
            ("RALLY_HOOK_TIMEOUT_MS", "200"),
            // RALLY_BEFORE_WRITE_FAILCLOSED is deliberately unset.
        ],
    );
    assert!(
        output.status.success(),
        "default before-write must fail open on watchdog timeout; code={:?}",
        output.status.code()
    );

    ws.cleanup();
}

#[test]
fn fail_open_flag_overrides_failclosed_env() {
    // `--fail-open` on the call site must beat
    // RALLY_BEFORE_WRITE_FAILCLOSED=1 — per-call escape hatch for an
    // operator who needs to unblock without unsetting the env var.
    let ws = Workspace::new("rally-failopen-override");
    let output = ws.run_envs(
        &[
            "check",
            "before-write",
            "--json",
            "--tool",
            "codex",
            "--path",
            "src/lib.rs",
            "--strict",
            "--fail-open",
        ],
        &[
            ("RALLY_TEST_BLOCK_MS", "5000"),
            ("RALLY_HOOK_TIMEOUT_MS", "200"),
            ("RALLY_BEFORE_WRITE_FAILCLOSED", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "--fail-open must beat RALLY_BEFORE_WRITE_FAILCLOSED; code={:?}",
        output.status.code()
    );
    ws.cleanup();
}
