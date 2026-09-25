// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! U5 (`et-native-rally-stage2-plan.md`, LD-H) — `say handoff --deliver
//! inject` (the default) must behave exactly like `--deliver record` when
//! Easy Terminal's rally-router already owns delivery to the resolved
//! target identity: the fact still commits, but rally writes ZERO bytes to
//! the target's pane.
//!
//! Ownership is decided from `RALLY_ET_ROUTER_HEALTH`, a path to a health
//! file the router writes atomically (`crates/rally-cli/src/et_router_health.rs`).
//! rally suppresses its own inject only when that file is a regular file (no
//! symlink), parses as `et.rally-router.health.v1`, is fresh
//! (`now_ms - updated_ms <= 10s`), not `degraded`, and lists the target
//! identity verbatim in `routed_identities`. Every other condition —
//! missing/unset, stale, degraded, absent identity, a symlink, or corrupt
//! JSON — falls back to today's behavior unchanged.
//!
//! Each test proves this by asserting on a `--tmux-bin` SPY log: the record
//! for a target rally would otherwise inject into is a session registered
//! with `rally run --backend tmux --tmux-bin <spy>`. A suppressed inject
//! leaves the spy log EMPTY (rally never shells the backend at all, not even
//! to probe liveness); an unsuppressed inject leaves it non-empty (rally
//! shells the backend to verify and write the pane), matching how
//! `handoff_delivery_escalation.rs::handoff_to_live_managed_session_is_injected_by_default`
//! asserts the un-suppressed path.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use support::channel_sandbox::ChannelSandbox;
use support::rally_cmd::rally_command;

const SENDER: &str = "sender:01";
/// Matches `et_router_health::FRESHNESS_WINDOW_MS`. Duplicated on purpose —
/// an integration test that imported the constant would pass even if the
/// production window and the test's fixture drifted apart together.
const FRESHNESS_WINDOW_MS: i64 = 10_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A tmux test double that logs every invocation (one line per call, the
/// full argv) so a test can tell "rally never touched the backend" from
/// "rally touched the backend and it happened to fail". Same shape as
/// `inject_security.rs::recording_tmux_stub` — duplicated locally rather
/// than shared, since `tests/support` is peer-owned scope for this task.
fn recording_tmux_stub(sandbox: &ChannelSandbox, tag: &str) -> (PathBuf, PathBuf) {
    let bin = sandbox.root().join(format!("tmux-{tag}-spy.sh"));
    let log = sandbox.root().join(format!("tmux-{tag}-spy.log"));
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n  list-panes) printf '%s\\n%s\\n%s\\n' 'rally-claude-{tag}' '@1' '%1' ;;\n  display-message) printf '%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\n' '%1' '101' '202' '/tmp/rally-test.sock' '0' '0' '0' '0' ;;\n  capture-pane) printf '%s\\n' 'unrelated pane content' ;;\nesac\nexit 0\n",
        log.display()
    );
    fs::write(&bin, body).expect("write tmux spy");
    let mut permissions = fs::metadata(&bin).expect("stat tmux spy").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&bin, permissions).expect("chmod tmux spy");
    fs::write(&log, "").expect("seed empty spy log");
    (bin, log)
}

fn spy_log_is_empty(log: &Path) -> bool {
    fs::read_to_string(log)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
}

/// Write an ET rally-router health file at `path`.
fn write_health(path: &Path, updated_ms: i64, degraded: bool, routed_identities: &[&str]) {
    let body = serde_json::json!({
        "schema": "et.rally-router.health.v1",
        "pid": std::process::id(),
        "updated_ms": updated_ms,
        "degraded": degraded,
        "routed_identities": routed_identities,
    });
    fs::write(path, serde_json::to_vec_pretty(&body).expect("serialize health fixture"))
        .expect("write health fixture");
}

/// Run `rally <args>` hermetically inside `sandbox`, with `RALLY_ET_ROUTER_HEALTH`
/// set to `router_health` (or unset when `None`). Panics on non-zero exit so
/// a test sees stderr instead of a bare parse failure.
fn run_rally(sandbox: &ChannelSandbox, args: &[&str], router_health: Option<&Path>) -> Value {
    let mut cmd = rally_command();
    cmd.args(args)
        .current_dir(sandbox.cwd())
        .env("HOME", sandbox.home())
        .env_remove("PWD")
        .env_remove("RALLY_PTYD_SOCKET")
        .env_remove("PTYD_SOCKET_PATH");
    match router_health {
        Some(path) => {
            cmd.env("RALLY_ET_ROUTER_HEALTH", path);
        }
        None => {
            cmd.env_remove("RALLY_ET_ROUTER_HEALTH");
        }
    }
    let output = cmd.output().expect("spawn rally");
    assert!(
        output.status.success(),
        "rally {args:?} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "rally {args:?} did not return JSON: {e}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// A registered session and its tmux spy: `tool` is the identity a
/// `say handoff --target` resolves against, `bin` is the EXECUTABLE spy
/// script passed as `--tmux-bin`, and `log` is the file that script appends
/// its argv to on every invocation.
struct RecordedSession {
    tool: String,
    bin: PathBuf,
    log: PathBuf,
}

/// Register a live tmux-managed session behind a recording spy. Mirrors
/// `handoff_delivery_escalation.rs::handoff_to_live_managed_session_is_injected_by_default`,
/// which is the un-suppressed baseline this file's tests diverge from.
fn add_recorded_session(sandbox: &ChannelSandbox, tag: &str) -> RecordedSession {
    // `rally run --name <tag> --shared` assigns the session `<tag>-01` (the
    // first pane of the first tab); the stub's `list-panes` output must
    // report a pane matching that FULL name, or the backend liveness probe
    // (`resolve_inject_target`) reports the session stale and every "normal
    // inject" test below falls into the wrong branch for the wrong reason.
    let (bin, log) = recording_tmux_stub(sandbox, &format!("{tag}-01"));
    let run = run_rally(
        sandbox,
        &[
            "run",
            "claude",
            "--json",
            "--name",
            tag,
            "--shared",
            "--backend",
            "tmux",
            "--tmux-bin",
            bin.to_str().expect("utf-8 spy path"),
        ],
        None,
    );
    let target_tool = run["data"]["run"]["session"]["tool"]
        .as_str()
        .expect("session.tool")
        .to_string();
    // `rally run` itself talks to tmux (to seed the session); the assertion
    // in every test below is about the SUBSEQUENT `say handoff`, so reset
    // the log to empty right before returning control to the caller.
    fs::write(&log, "").expect("reset spy log after session setup");
    RecordedSession {
        tool: target_tool,
        bin,
        log,
    }
}

fn send_handoff(
    sandbox: &ChannelSandbox,
    session: &RecordedSession,
    router_health: Option<&Path>,
    extra: &[&str],
) -> Value {
    let mut args: Vec<&str> = vec![
        "say",
        "handoff",
        "--json",
        "--tool",
        SENDER,
        "--target",
        &session.tool,
        "--subject",
        "commit the staged files",
        "--tmux-bin",
    ];
    let bin_str = session.bin.to_str().expect("utf-8 spy path");
    args.push(bin_str);
    args.extend_from_slice(extra);
    run_rally(sandbox, &args, router_health)
}

// ---------------------------------------------------------------------------
// (1) Fresh health file listing the target → record-only via ET's router;
//     rally never touches the backend at all (spy log stays empty).
// ---------------------------------------------------------------------------

#[test]
fn fresh_healthy_listed_identity_is_record_only_with_no_backend_call() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-fresh");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    write_health(&health_path, now_ms(), false, &[session.tool.as_str()]);

    let said = send_handoff(&sandbox, &session, Some(&health_path), &[]);
    let delivery = &said["data"]["delivery"];

    assert_eq!(delivery["mode"], "inject", "{said}");
    assert_eq!(delivery["status"], "record_only", "{said}");
    assert!(
        delivery["detail"]
            .as_str()
            .is_some_and(|d| d.contains("et-router")),
        "detail should name the et-router reason: {said}"
    );
    assert_eq!(said["data"]["say"]["committed"], true, "{said}");
    assert!(
        spy_log_is_empty(&session.log),
        "et-router coverage must suppress EVERY backend call, not just send-keys: {}",
        fs::read_to_string(&session.log).unwrap_or_default()
    );
}

// ---------------------------------------------------------------------------
// (2)-(7): every other condition falls back to today's un-suppressed inject.
// ---------------------------------------------------------------------------

fn assert_normal_inject_path(said: &Value, session: &RecordedSession) {
    let delivery = &said["data"]["delivery"];
    assert_eq!(delivery["mode"], "inject", "{said}");
    assert_eq!(
        delivery["status"], "injected",
        "expected the normal un-suppressed inject path: {said}"
    );
    assert!(
        !spy_log_is_empty(&session.log),
        "normal inject path must reach the backend (spy log should be non-empty); envelope={said}\nlog_path={}\nlog_contents={:?}",
        session.log.display(),
        fs::read_to_string(&session.log)
    );
}

#[test]
fn stale_health_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-stale");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    // 1s past the 10s freshness window.
    write_health(
        &health_path,
        now_ms() - FRESHNESS_WINDOW_MS - 1_000,
        false,
        &[session.tool.as_str()],
    );

    let said = send_handoff(&sandbox, &session, Some(&health_path), &[]);
    assert_normal_inject_path(&said, &session);
}

#[test]
fn degraded_health_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-degraded");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    write_health(&health_path, now_ms(), true, &[session.tool.as_str()]);

    let said = send_handoff(&sandbox, &session, Some(&health_path), &[]);
    assert_normal_inject_path(&said, &session);
}

#[test]
fn target_absent_from_routed_identities_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-absent");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    write_health(&health_path, now_ms(), false, &["some-other-identity:01"]);

    let said = send_handoff(&sandbox, &session, Some(&health_path), &[]);
    assert_normal_inject_path(&said, &session);
}

#[test]
fn symlink_health_file_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-symlink");
    let session = add_recorded_session(&sandbox, &name);

    let real_path = sandbox.root().join("health-real.json");
    write_health(&real_path, now_ms(), false, &[session.tool.as_str()]);
    let link_path = sandbox.root().join("health-link.json");
    std::os::unix::fs::symlink(&real_path, &link_path).expect("create health symlink");

    let said = send_handoff(&sandbox, &session, Some(&link_path), &[]);
    assert_normal_inject_path(&said, &session);
}

#[test]
fn corrupt_json_health_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-corrupt");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    fs::write(&health_path, "{ this is not valid json ").expect("write corrupt health fixture");

    let said = send_handoff(&sandbox, &session, Some(&health_path), &[]);
    assert_normal_inject_path(&said, &session);
}

#[test]
fn unset_env_falls_back_to_normal_inject() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-unset");
    let session = add_recorded_session(&sandbox, &name);

    // Even though a perfectly healthy file exists on disk, `--deliver
    // inject`'s default behavior must be untouched when the env var naming
    // it is never set.
    let health_path = sandbox.root().join("health.json");
    write_health(&health_path, now_ms(), false, &[session.tool.as_str()]);

    let said = send_handoff(&sandbox, &session, None, &[]);
    assert_normal_inject_path(&said, &session);
}

// ---------------------------------------------------------------------------
// The fact committed is unaffected by which arm ran: same kind/target/
// subject/ack_by shape, whether or not the et-router suppressed the inject.
// ---------------------------------------------------------------------------

#[test]
fn suppressed_and_unsuppressed_deliveries_commit_the_same_fact_shape() {
    let sandbox = ChannelSandbox::spawn();
    let name = unique_name("u5-factshape");
    let session = add_recorded_session(&sandbox, &name);

    let health_path = sandbox.root().join("health.json");
    write_health(&health_path, now_ms(), false, &[session.tool.as_str()]);
    let suppressed = send_handoff(
        &sandbox,
        &session,
        Some(&health_path),
        &["--ack-within", "10m"],
    );

    let unsuppressed = send_handoff(&sandbox, &session, None, &["--ack-within", "10m"]);

    for said in [&suppressed, &unsuppressed] {
        assert_eq!(said["data"]["say"]["fact"]["kind"], "handoff", "{said}");
        assert_eq!(said["data"]["say"]["fact"]["target"], session.tool, "{said}");
        assert_eq!(said["data"]["say"]["committed"], true, "{said}");
        assert!(said["data"]["delivery"]["ack_by"].is_string(), "{said}");
    }
}
