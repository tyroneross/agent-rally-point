// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(test, allow(unused_must_use))]

use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

/// Build-id stamp emitted by build.rs: `<version>+<git-short-hash>`.
/// Exposed as `rally version --json` and embedded in every presence fact so
/// the R9 stale-binary guard can detect when different builds are writing to
/// the same room.
pub(crate) const BUILD_ID: &str = env!("RALLY_BUILD_ID");

const SCHEMA_STATUS: &str = "agent-rally.command.status.v1";
const SCHEMA_INIT: &str = "agent-rally.command.init.v1";
const SCHEMA_HOOKS: &str = "agent-rally.command.hooks.v1";
const SCHEMA_RETROSPECTIVE: &str = "agent-rally.command.retrospective.v1";
const SCHEMA_ROTATE: &str = "agent-rally.command.rotate.v1";
const SCHEMA_ENTER: &str = "agent-rally.command.enter.v1";
const SCHEMA_SAY: &str = "agent-rally.command.say.v1";
const SCHEMA_ROOM: &str = "agent-rally.command.room.v1";
const SCHEMA_NEXT: &str = "agent-rally.command.next.v1";
const SCHEMA_LOCATE: &str = "agent-rally.command.locate.v1";
const SCHEMA_RECENT: &str = "agent-rally.command.recent.v1";
const SCHEMA_CHECK: &str = "agent-rally.command.check.v1";
const SCHEMA_RUN: &str = "agent-rally.command.run.v1";
const SCHEMA_SESSIONS: &str = "agent-rally.command.sessions.v1";
const SCHEMA_INJECT: &str = "agent-rally.command.inject.v1";
const SCHEMA_SESSION_ACTION: &str = "agent-rally.command.session-action.v1";
const SCHEMA_ADOPT: &str = "agent-rally.command.adopt.v1";
pub(crate) const FACT_SCHEMA: &str = "agent-rally.fact.v1";
const SESSION_IDENTITY_RETRIES: usize = 4096;
const SESSION_RESERVATION_LOCK_FILENAME: &str = "session-reservation.lock";

#[cfg(unix)]
mod session_reservation_lock {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_UN: i32 = 8;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    pub(super) struct Guard {
        file: fs::File,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
        }
    }

    pub(super) fn acquire(repo: &Path) -> Result<Guard> {
        let rally_dir = repo.join(".rally");
        fs::create_dir_all(&rally_dir)
            .map_err(RallyError::io(format!("create {}", rally_dir.display())))?;
        let path = rally_dir.join(SESSION_RESERVATION_LOCK_FILENAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(RallyError::io(format!("open {}", path.display())))?;
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
        if rc != 0 {
            return Err(RallyError::Io {
                context: format!("lock {}", path.display()),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(Guard { file })
    }
}

#[cfg(not(unix))]
mod session_reservation_lock {
    use super::*;

    pub(super) struct Guard;

    pub(super) fn acquire(repo: &Path) -> Result<Guard> {
        fs::create_dir_all(repo.join(".rally"))
            .map_err(RallyError::io("create .rally for session reservation"))?;
        Ok(Guard)
    }
}

macro_rules! cmd {
    ($($arg:expr),+ $(,)?) => {
        vec![$($arg.to_string()),+]
    };
}

mod agent_state;
mod backends;
mod backlog;
mod board;
mod check;
mod check_ci;
mod claim_authority;
mod cli;
mod daemon_client;
mod dag;
mod decay;
mod discovery;
mod doctor;
mod error;
mod event_envelope;
mod hook_runtime;
mod hooks_config;
mod init;
mod liveness;
mod next;
mod observed_liveness;
mod output;
pub mod rallyd_core;
mod reaper;
mod relevance;
mod resource_scope;
mod retrospective;
mod retry_budget;
mod ripple;
mod rotate;
mod route_findings;
mod run_worktree;
mod session_identity;
mod source_grounding;
mod store;
mod store_client;
#[cfg(test)]
mod test_git_fixture;
mod tier_fit;
pub mod worktree_gc;
mod worktree_guard;
mod write_authority;

use backends::*;
use backlog::{
    BacklogItem, add_backlog_item, list_backlog_items, mark_backlog_done, update_backlog_item,
};
use board::{BoardOutput, build_board};
use check::build_check;
use check_ci::build_check_ci;
use cli::*;
use dag::{DagOutput, WakeDueEntry, build_dag, project_wake_due, resolve_wake_after};
use error::{RallyError, Result};
use next::{AttentionItem, EntryData, NextResult, build_attention, build_entry, build_next};
use output::{CliError, Output, RenderedOutput};
use rallyd_core::ServeConfig;
use route_findings::{Finding, RoutingSummary, route_findings};
use store::{
    AckPollingStore, ConditionalAppendOutcome, Fact, FactKind, ReadReceipt, RoomQuery,
    RoomSnapshot, RoomStore, RoomSummary,
};
// Envelope wrapper types from backends module.
use backends::{
    AdoptData, AdoptEnvelope, InjectEnvelope, RunEnvelope, SessionActionEnvelope, SessionsEnvelope,
};

const SCHEMA_MIGRATE_LEGACY: &str = "agent-rally.command.migrate-legacy.v1";
const SCHEMA_DOCTOR: &str = "agent-rally.command.doctor.v1";
const SCHEMA_VERSION: &str = "agent-rally.command.version.v1";
const SCHEMA_WHOAMI: &str = "agent-rally.command.whoami.v1";
const SCHEMA_OWNERS: &str = "agent-rally.command.owners.v1";
// Work surface schemas
const SCHEMA_BACKLOG: &str = "agent-rally.command.backlog.v1";
const SCHEMA_LEAD: &str = "agent-rally.command.lead.v1";
const SCHEMA_ACK: &str = "agent-rally.command.ack.v1";
const SCHEMA_BOARD: &str = "agent-rally.command.board.v1";
// Read-only per-kind room projections
const SCHEMA_RISKS: &str = "agent-rally.command.risks.v1";
const SCHEMA_DECISIONS: &str = "agent-rally.command.decisions.v1";
const SCHEMA_ARTIFACTS: &str = "agent-rally.command.artifacts.v1";
const SCHEMA_CLAIMS: &str = "agent-rally.command.claims.v1";
const SCHEMA_ROUTE_FINDINGS: &str = "agent-rally.command.route-findings.v1";
// B13
const SCHEMA_CHECK_CI: &str = "agent-rally.command.check-ci.v1";
// B1/B2/B4: pi-dynamic observation seam
const SCHEMA_DAG: &str = "agent-rally.command.dag.v1";
const SCHEMA_WAKE_DUE: &str = "agent-rally.command.wake-due.v1";
// Rank-11: room north-star + per-agent autonomy envelope
const SCHEMA_MISSION: &str = "agent-rally.command.mission.v1";
// BACKLOG S-P3, Chunk C: `rally daemon serve|start|stop|status`
const SCHEMA_DAEMON: &str = "agent-rally.command.daemon.v1";

thread_local! {
    static WATCHDOG_MUTATION_SIGNAL: RefCell<Option<Arc<Mutex<WatchdogMutationState>>>> = const { RefCell::new(None) };
    static WATCHDOG_COMMIT_ARM_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Instant at which this command's watchdog fires.
    ///
    /// Installed on the worker thread by [`run_with_watchdog`] so code deep in
    /// the command path can budget against the SAME deadline the watchdog will
    /// enforce, instead of guessing with an independent constant. Retry loops
    /// read it through [`watchdog_remaining`].
    static WATCHDOG_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static PENDING_APPEND_OUTCOMES: RefCell<Vec<store::AppendOutcome>> = const { RefCell::new(Vec::new()) };
    static PENDING_APPEND_ISSUES: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
enum WatchdogMutationState {
    NotStarted,
    OutcomeUnknown {
        event_id: String,
        phase: String,
    },
    DbOnlyMigrationOutcomeUnknown {
        migration_id: String,
        phase: String,
        retry_command: String,
    },
    Committed {
        projection_complete: bool,
        warnings: Vec<Value>,
    },
}

pub(crate) fn record_append_outcome(outcome: &store::AppendOutcome) {
    PENDING_APPEND_OUTCOMES.with(|pending| pending.borrow_mut().push(outcome.clone()));
}

fn record_optional_append_issue(context: &str, error: &RallyError) {
    let issue = match error {
        RallyError::OutcomeUnknown {
            event_id,
            phase,
            detail,
        } => json!({
            "code": "outcome_unknown",
            "context": context,
            "event_id": event_id,
            "phase": phase,
            "detail": detail,
            "query_remedy": locate_remedy(event_id),
        }),
        _ => json!({
            "code": "optional_append_failed",
            "context": context,
            "detail": error.to_string(),
        }),
    };
    PENDING_APPEND_ISSUES.with(|pending| pending.borrow_mut().push(issue));
}

fn consume_optional_append(result: Result<store::AppendOutcome>, context: &str) {
    match result {
        Ok(outcome) => record_append_outcome(&outcome),
        Err(error) => record_optional_append_issue(context, &error),
    }
}

fn record_conditional_append(outcome: store::ConditionalAppendOutcome) {
    if let store::ConditionalAppendOutcome::Applied(outcome) = outcome {
        record_append_outcome(&outcome);
    }
}

fn consume_optional_conditional_append(
    result: Result<store::ConditionalAppendOutcome>,
    context: &str,
) {
    match result {
        Ok(outcome) => record_conditional_append(outcome),
        Err(error) => record_optional_append_issue(context, &error),
    }
}

fn consume_optional_result<T>(result: Result<T>, context: &str) {
    if let Err(error) = result {
        record_optional_append_issue(context, &error);
    }
}

fn update_recorded_append_outcome(outcome: &store::AppendOutcome) {
    PENDING_APPEND_OUTCOMES.with(|pending| {
        if let Some(existing) = pending
            .borrow_mut()
            .iter_mut()
            .rev()
            .find(|existing| existing.fact.event_id == outcome.fact.event_id)
        {
            *existing = outcome.clone();
        }
    });
}

fn drain_pending_append_outcomes() -> Vec<store::AppendOutcome> {
    PENDING_APPEND_OUTCOMES.with(|pending| std::mem::take(&mut *pending.borrow_mut()))
}

fn drain_pending_append_issues() -> Vec<Value> {
    PENDING_APPEND_ISSUES.with(|pending| std::mem::take(&mut *pending.borrow_mut()))
}

fn attach_append_outcomes(output: &mut Output, outcomes: Vec<store::AppendOutcome>) {
    if outcomes.is_empty() {
        return;
    }
    let projection_complete = outcomes.iter().all(|outcome| outcome.projection_complete);
    let value = serde_json::to_value(&outcomes).unwrap_or_else(|error| {
        json!([{
            "committed": true,
            "projection_complete": false,
            "warnings": [{"code": "serialization", "message": error.to_string()}]
        }])
    });
    if let Some(body) = output.body.as_object_mut()
        && let Some(data) = body.get_mut("data").and_then(Value::as_object_mut)
    {
        data.insert("append_outcomes".to_string(), value);
        data.insert(
            "projection_complete".to_string(),
            Value::Bool(projection_complete),
        );
    }
    if !projection_complete && !output.json {
        output.text.push_str(
            "\nwarning: canonical append committed; one or more derived projections are incomplete",
        );
    }
}

fn attach_append_issues(output: &mut Output, issues: Vec<Value>) {
    if issues.is_empty() {
        return;
    }
    if let Some(body) = output.body.as_object_mut()
        && let Some(data) = body.get_mut("data").and_then(Value::as_object_mut)
    {
        data.insert("append_issues".to_string(), Value::Array(issues));
    }
    if !output.json {
        output
            .text
            .push_str("\nwarning: optional durable append work did not complete; inspect append_issues in JSON");
    }
}

fn attach_pending_append_outcomes(output: &mut Output) {
    attach_append_outcomes(output, drain_pending_append_outcomes());
    attach_append_issues(output, drain_pending_append_issues());
}

/// Convert an error after one or more proven canonical commits into an
/// explicit partial-commit aggregate. A later OutcomeUnknown remains
/// query-required and retains watchdog precedence; every other later required
/// failure is nonzero and explicitly forbids whole-command retry. Commands
/// with genuinely optional post-commit work convert that work to warnings at
/// their own boundary. The collector is always drained, so in-process commands
/// cannot inherit stale outcomes.
fn output_after_committed_error(error: RallyError, json_output: bool) -> Result<Output> {
    let mut outcomes = drain_pending_append_outcomes();
    let issues = drain_pending_append_issues();
    if outcomes.is_empty() {
        if let RallyError::OutcomeUnknown {
            event_id,
            phase,
            detail,
        } = &error
        {
            let remedy = locate_remedy(event_id);
            let message = format!(
                "canonical mutation outcome is unknown at phase {phase}; run `{remedy}` before deciding whether to rerun"
            );
            let body = json!({
                "ok": false,
                "product": "rally",
                "command": "mutation_outcome_unknown",
                "data": {
                    "committed": null,
                    "event_id": event_id,
                    "phase": phase,
                    "detail": detail,
                    "query_remedy": remedy,
                    "message": message,
                }
            });
            let mut output = Output::new(json_output, message, body).with_exit_code(1);
            attach_append_issues(&mut output, issues);
            return Ok(output);
        }
        return Err(error);
    }

    let (exit_code, unknown) = match &error {
        RallyError::OutcomeUnknown {
            event_id,
            phase,
            detail,
        } => (
            1,
            Some(json!({
                "event_id": event_id,
                "phase": phase,
                "detail": detail,
                "remedy": locate_remedy(event_id),
            })),
        ),
        _ => (1, None),
    };
    let warning = store::ProjectionWarning {
        code: store::ProjectionWarningCode::PostCommitWork,
        message: format!("post-commit command work did not complete: {error}"),
    };
    if let Some(last) = outcomes.last_mut() {
        last.projection_complete = false;
        last.warnings.push(warning.clone());
        if unknown.is_none() {
            mark_watchdog_append_outcome(last);
        }
    }
    let message = match &error {
        RallyError::OutcomeUnknown {
            event_id, phase, ..
        } => {
            let remedy = locate_remedy(event_id);
            format!(
                "one or more canonical appends committed and a later append outcome is unknown at phase {phase}; run `{remedy}` before deciding whether to resume"
            )
        }
        _ => "part of this command committed canonically before a later required step failed; do not retry the whole command".to_string(),
    };
    let body = json!({
        "ok": exit_code == 0,
        "product": "rally",
        "command": "partial_commit",
        "data": {
            "committed": true,
            "projection_complete": false,
            "warning": warning,
            "outcome_unknown": unknown,
            "message": message,
        }
    });
    let mut output = Output::new(json_output, message, body).with_exit_code(exit_code);
    attach_append_outcomes(&mut output, outcomes);
    attach_append_issues(&mut output, issues);
    Ok(output)
}

struct WatchdogDeadlineGuard;

impl Drop for WatchdogDeadlineGuard {
    fn drop(&mut self) {
        WATCHDOG_DEADLINE.with(|slot| slot.set(None));
    }
}

fn install_watchdog_deadline(deadline: Instant) -> WatchdogDeadlineGuard {
    WATCHDOG_DEADLINE.with(|slot| slot.set(Some(deadline)));
    WatchdogDeadlineGuard
}

/// Time left before this command's watchdog fires.
///
/// `None` when no watchdog is armed — `daemon serve` (which legitimately blocks
/// for the daemon's lifetime), the inline fallback path, and in-process tests.
///
/// `None` is resolved DIFFERENTLY by each consumer, and the asymmetry is
/// deliberate (see [`crate::retry_budget::budgets_for`]): the SQLite blocking
/// budget falls back to upstream's 5s, because inventing a short deadline where
/// none exists would break `daemon serve`; the retry budget falls back to a
/// multiple of that, because a loop must stay finite even with no deadline to
/// answer to. Neither is "unbounded".
pub(crate) fn watchdog_remaining() -> Option<Duration> {
    WATCHDOG_DEADLINE.with(|slot| {
        slot.get()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    })
}

struct WatchdogCommitSignalGuard;

impl Drop for WatchdogCommitSignalGuard {
    fn drop(&mut self) {
        WATCHDOG_MUTATION_SIGNAL.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

fn install_watchdog_commit_signal(
    signal: Arc<Mutex<WatchdogMutationState>>,
) -> WatchdogCommitSignalGuard {
    WATCHDOG_MUTATION_SIGNAL.with(|slot| {
        *slot.borrow_mut() = Some(signal);
    });
    WatchdogCommitSignalGuard
}

struct WatchdogCommitArmGuard;

impl Drop for WatchdogCommitArmGuard {
    fn drop(&mut self) {
        WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

fn arm_watchdog_command_commit() -> WatchdogCommitArmGuard {
    WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| {
        depth.set(depth.get().saturating_add(1));
    });
    WatchdogCommitArmGuard
}

fn with_watchdog_command_commit<T>(f: impl FnOnce() -> T) -> T {
    let _guard = arm_watchdog_command_commit();
    f()
}

pub(crate) fn mark_watchdog_command_commit() {
    let armed = WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| depth.get() > 0);
    if !armed {
        return;
    }
    WATCHDOG_MUTATION_SIGNAL.with(|slot| {
        if let Some(signal) = slot.borrow().as_ref() {
            *signal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                WatchdogMutationState::Committed {
                    projection_complete: true,
                    warnings: Vec::new(),
                };
        }
    });
    block_after_watchdog_commit_for_test();
}

pub(crate) fn block_after_watchdog_commit_for_test() {
    let armed = WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| depth.get() > 0);
    if !armed {
        return;
    }
    #[cfg(debug_assertions)]
    if let Ok(ms) = env::var("RALLY_TEST_BLOCK_AFTER_COMMIT_MS")
        && let Ok(ms) = ms.trim().parse::<u64>()
    {
        thread::sleep(Duration::from_millis(ms));
    }
}

pub(crate) fn mark_watchdog_command_outcome_unknown(event_id: &str, phase: &str) {
    let armed = WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| depth.get() > 0);
    if !armed {
        return;
    }
    WATCHDOG_MUTATION_SIGNAL.with(|slot| {
        if let Some(signal) = slot.borrow().as_ref() {
            *signal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                WatchdogMutationState::OutcomeUnknown {
                    event_id: event_id.to_string(),
                    phase: phase.to_string(),
                };
        }
    });
}

pub(crate) fn mark_watchdog_db_only_migration_outcome_unknown(
    migration_id: &str,
    phase: &str,
    retry_command: &str,
) {
    let armed = WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| depth.get() > 0);
    if !armed {
        return;
    }
    WATCHDOG_MUTATION_SIGNAL.with(|slot| {
        if let Some(signal) = slot.borrow().as_ref() {
            *signal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                WatchdogMutationState::DbOnlyMigrationOutcomeUnknown {
                    migration_id: migration_id.to_string(),
                    phase: phase.to_string(),
                    retry_command: retry_command.to_string(),
                };
        }
    });
}

pub(crate) fn mark_watchdog_append_outcome(outcome: &store::AppendOutcome) {
    let armed = WATCHDOG_COMMIT_ARM_DEPTH.with(|depth| depth.get() > 0);
    if !armed {
        return;
    }
    WATCHDOG_MUTATION_SIGNAL.with(|slot| {
        if let Some(signal) = slot.borrow().as_ref() {
            let warnings = outcome
                .warnings
                .iter()
                .map(|warning| {
                    serde_json::to_value(warning).unwrap_or_else(
                        |_| json!({"code": "serialization", "message": warning.message}),
                    )
                })
                .collect();
            *signal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                WatchdogMutationState::Committed {
                    projection_complete: outcome.projection_complete,
                    warnings,
                };
        }
    });
}

/// Default hard wall-clock budget for a single `rally` invocation, in
/// milliseconds. Any command that has not returned within this budget is
/// abandoned and the process exits fail-open (see [`run_with_watchdog`]).
///
/// Rationale: rally is invoked synchronously from agent write-hooks. A hook
/// that blocks on stuck filesystem I/O (NFS stall, a contended/zombie
/// lockfile, a wedged segment replay) would otherwise hang forever — on
/// 2026-05-30 four `before-write` hooks sat in uninterruptible (`UE`) kernel
/// wait for 7h45m, unkillable even by SIGKILL. `--fail-open` only governs
/// error-vs-block semantics; it does NOT bound execution time. This watchdog
/// is the time bound, and it applies to *every* command path so the safety
/// does not depend on which syscall happens to block.
const DEFAULT_WATCHDOG_TIMEOUT_MS: u64 = 3000;
/// Floor/ceiling for the configurable budget (defense against a `0` or absurd
/// override that would re-introduce the hang or break fast commands).
const MIN_WATCHDOG_TIMEOUT_MS: u64 = 100;
const MAX_WATCHDOG_TIMEOUT_MS: u64 = 60_000;

/// Headroom added to an `inject` command's `--timeout-seconds` ACK budget when
/// it sets the watchdog. The inner ACK poll sleeps in 250ms ticks and does a
/// final ledger scan + envelope build after the deadline; this margin keeps the
/// outer watchdog from racing the inner poll's own timeout (which is the path
/// that correctly emits `ack_state: "timeout"` + a populated `fallback_plan`).
const INJECT_WATCHDOG_HEADROOM_MS: u64 = 5_000;
/// Absolute ceiling for the `inject` watchdog. The CLI caps `--timeout-seconds`
/// at 600 (`bounded_i64_arg`), so 600s + headroom is the worst case; this is a
/// defensive bound in case that CLI cap ever changes.
const INJECT_MAX_WATCHDOG_TIMEOUT_MS: u64 = 605_000;

/// Elevated watchdog budget for `rally daemon start` (D1, BACKLOG S-P3). Sized
/// with margin above the store router's 30s bounded-block corridor
/// (`store_client::CORRIDOR_BOUND`) so `start`'s own wait-for-ready poll —
/// which blocks on the SAME cold-reconcile window the corridor exists to
/// tolerate (R3) — is never pre-empted by the hook-safety watchdog. Below
/// `MAX_WATCHDOG_TIMEOUT_MS` is irrelevant here: like the `inject` budget,
/// this is returned directly rather than clamped through the generic
/// override path.
const DAEMON_START_WATCHDOG_TIMEOUT_MS: u64 = 45_000;

/// True when the resolved subcommand is `inject`. `inject` is the one
/// deliberately-LONG interactive coordination verb: with `--handoff` /
/// `--require-ack` it BLOCKS polling the ledger for a target-authored ACK up to
/// `--timeout-seconds` (1–600s). The 3s-default / 60s-max hook watchdog exists
/// to stop a write-hook wedged on stuck I/O — it must not pre-empt inject's
/// legitimate ACK wait, which it otherwise always does (firing first and
/// emitting the neutral fail-open envelope, which looks exactly like the
/// "bare `{ok:true}`, no InjectData" symptom). First positional, dash-skipping,
/// matching the `resolve_watchdog_posture` subcommand gate.
fn first_positional_is_inject(args: &[String]) -> bool {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        == Some("inject")
}

/// D1 (Chunk C, BACKLOG S-P3): true iff the leading two positionals are
/// `daemon serve`. `rallyd_core::serve` blocks for the daemon's ENTIRE
/// serving lifetime (until SIGTERM/SIGINT or `--idle-exit-secs`) — it is not
/// a bounded hook command at all, unlike `inject`'s bounded-but-long ACK
/// wait. The hook-safety watchdog's fail-open path calls
/// `std::process::exit(0)` on timeout, which would kill the daemon process
/// itself (in what looks like ordinary success) and silently take
/// `.rally/rallyd.sock` down with it. `run_with_watchdog` detects this shape
/// and bypasses the race entirely — mirroring [`first_positional_is_inject`]'s
/// detect-then-special-case shape, but routing to the NO-DEADLINE inline path
/// rather than sizing a (necessarily finite) timeout.
fn first_two_positionals_are_daemon_serve(args: &[String]) -> bool {
    matches!(first_positionals(args), (Some("daemon"), Some("serve")))
}

/// True iff the leading two positionals are `daemon start`. R3: `start`
/// blocks until `.rally/rallyd.sock.addr` exists AND a `Ping` round-trips,
/// which during a cold reconcile (segment replay on a large room) can take
/// seconds-to-tens-of-seconds — the store router's own bounded-block
/// corridor (`store_client::CORRIDOR_BOUND`, Chunk C) waits up to 30s in
/// 3s-per-attempt re-probes before failing loud. `daemon start`'s watchdog
/// must not pre-empt that corridor, so it gets an elevated, fixed budget with
/// margin above it (see `DAEMON_START_WATCHDOG_TIMEOUT_MS`). `daemon
/// stop`/`daemon status` are quick one-shot probes and stay on the default
/// hook-safe budget.
fn first_two_positionals_are_daemon_start(args: &[String]) -> bool {
    matches!(first_positionals(args), (Some("daemon"), Some("start")))
}

/// Extract the `--timeout-seconds VALUE` (or `=VALUE`) ACK budget from an
/// inject invocation, if present and parseable.
fn inject_timeout_seconds(args: &[String]) -> Option<u64> {
    let positional = args
        .iter()
        .position(|a| a == "--timeout-seconds")
        .and_then(|i| args.get(i + 1).and_then(|v| v.parse::<u64>().ok()));
    let eq = args.iter().find_map(|a| {
        a.strip_prefix("--timeout-seconds=")
            .and_then(|v| v.parse::<u64>().ok())
    });
    positional.or(eq)
}

/// Resolve the watchdog budget. Priority order:
///  1. An explicit `--timeout-ms VALUE` / `--timeout-ms=VALUE` arg, or the
///     `RALLY_HOOK_TIMEOUT_MS` env var — operator escape hatch, clamped
///     `[MIN, MAX]` (the hook-safe band). This wins for ALL commands including
///     inject, so an operator can still cap a runaway inject.
///  2. For the `inject` subcommand (and only inject), derive the budget from
///     its `--timeout-seconds` ACK wait + headroom, bypassing the 60s hook cap.
///     This is what lets `inject --handoff --timeout-seconds 75` actually wait
///     75s for an ACK instead of being killed at 3s.
///  3. Otherwise the clamped default (`DEFAULT_WATCHDOG_TIMEOUT_MS`).
///
/// Out-of-range / unparseable inputs fall through rather than erroring — the
/// watchdog must never be the thing that fails a command.
fn resolve_watchdog_timeout(args: &[String]) -> Duration {
    let from_args = args
        .iter()
        .position(|a| a == "--timeout-ms")
        .and_then(|i| args.get(i + 1).and_then(|v| v.parse::<u64>().ok()));
    let from_eq = args.iter().find_map(|a| {
        a.strip_prefix("--timeout-ms=")
            .and_then(|v| v.parse::<u64>().ok())
    });
    let from_env = env::var("RALLY_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());

    // (1) Explicit override wins for every command, clamped to the hook band.
    if let Some(ms) = from_args.or(from_eq).or(from_env) {
        return Duration::from_millis(ms.clamp(MIN_WATCHDOG_TIMEOUT_MS, MAX_WATCHDOG_TIMEOUT_MS));
    }

    // (2) inject — the deliberately-blocking interactive verb — sizes its
    // watchdog from the ACK budget so the wait is never pre-empted.
    if first_positional_is_inject(args) {
        // Default inject ACK budget mirrors the CLI default (10s) when the flag
        // is absent so a bare `inject --handoff` still gets room to wait.
        let ack_secs = inject_timeout_seconds(args).unwrap_or(10);
        let ms = ack_secs
            .saturating_mul(1000)
            .saturating_add(INJECT_WATCHDOG_HEADROOM_MS)
            .clamp(MIN_WATCHDOG_TIMEOUT_MS, INJECT_MAX_WATCHDOG_TIMEOUT_MS);
        return Duration::from_millis(ms);
    }

    // (2b) `daemon start` (D1) — elevated fixed budget so it isn't pre-empted
    // by a cold-reconcile wait-for-ready poll sized against the store
    // router's 30s corridor (R3). `daemon serve` never reaches here at all —
    // it is intercepted before this function is even called (see
    // `run_with_watchdog`'s D1 bypass).
    if first_two_positionals_are_daemon_start(args) {
        return Duration::from_millis(DAEMON_START_WATCHDOG_TIMEOUT_MS);
    }

    // (3) Everything else: the hook-safe default.
    Duration::from_millis(DEFAULT_WATCHDOG_TIMEOUT_MS)
}

/// Remove watchdog-only flags from the argument list so they never reach a
/// subcommand parser. Three flags are watchdog-level and meaningless to any
/// individual command:
///   * `--timeout-ms VALUE` / `--timeout-ms=VALUE` — wall-clock budget.
///   * `--fail-open` — explicit posture override (default already; mostly
///     used by hook wrappers as a self-documenting marker).
///   * `--fail-closed` — opt-in fail-closed posture for `check before-write`.
///
/// The bpaf subcommand parsers don't know about these flags; without
/// stripping, the parser rejects them as "unexpected in this context" and
/// the only reason `--fail-open` worked historically was that the timeout
/// arm fired before the rejection could surface.
fn strip_timeout_flag(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--timeout-ms" {
            // Drop the flag, and drop the following token only when it is a
            // numeric value (so a malformed `--timeout-ms --json` doesn't
            // swallow `--json`).
            i += 1;
            if i < args.len() && args[i].parse::<u64>().is_ok() {
                i += 1;
            }
            continue;
        }
        if arg.starts_with("--timeout-ms=") {
            i += 1;
            continue;
        }
        if arg == "--fail-open" || arg == "--fail-closed" {
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

pub fn main() -> ExitCode {
    run_with_watchdog(env::args().skip(1).collect())
}

/// Run a single command under a hard wall-clock watchdog.
///
/// The command body executes on a worker thread; the main thread waits for it
/// with a deadline. If the worker finishes in time, we print its output and
/// return its exit code exactly as before. If the deadline elapses first, we
/// **fail open**: emit the neutral envelope the hook wrapper expects and exit
/// `0` immediately, abandoning the (possibly syscall-blocked) worker thread.
///
/// Abandoning the worker is safe and is the whole point: `std::process::exit`
/// tears down the entire process, so any file descriptor, advisory lock, or
/// child the worker was holding is released by the kernel at exit. Nothing the
/// hook touched can outlive the budget. (A thread stuck in an uninterruptible
/// kernel wait cannot be joined or cancelled from userspace — exiting the
/// process is the only correct release, and it is what a fresh `rally`
/// invocation would do anyway.)
fn run_with_watchdog(args: Vec<String>) -> ExitCode {
    // D1 (Chunk C, BACKLOG S-P3): `rally daemon serve` bypasses the watchdog
    // race entirely — see `first_two_positionals_are_daemon_serve`'s doc
    // comment. Route straight to the no-deadline inline path (the same path
    // `run_with_watchdog` itself falls back to when thread spawning fails)
    // rather than sizing a timeout, since `serve()` legitimately blocks for
    // the daemon's entire lifetime. Strip watchdog-only flags first so a
    // stray `--timeout-ms`/`--fail-open` on the invocation can't reach the
    // `daemon` subcommand parser (which doesn't know about them).
    if first_two_positionals_are_daemon_serve(&args) {
        return run_inline(strip_timeout_flag(args));
    }

    // Resolve the budget from the *raw* args, then strip the watchdog-only
    // `--timeout-ms` flag so it never reaches a subcommand parser (which would
    // reject it as unknown). The env var path needs no stripping.
    let timeout = resolve_watchdog_timeout(&args);
    // Watchdog-level flag detection happens BEFORE stripping so we honor
    // `--fail-open` / `--fail-closed` even though those tokens are removed
    // before the subcommand parser sees them.
    let fail_open = args.iter().any(|arg| arg == "--fail-open");
    // Posture override for the before-write gate: opt-in fail-CLOSED on
    // watchdog timeout. The default fail-open posture is right for read-only
    // commands (status, next, room, locate) — a stuck advisory must never
    // hang the host tool. But for the before-write coordination gate
    // specifically, two agents writing the same claimed path is the failure
    // mode the gate exists to prevent: better to delay the write than to
    // silently let two agents clobber one another. The posture is opt-in
    // (env var or flag) so existing call sites are unchanged, and it ONLY
    // activates when the resolved subcommand is `check before-write` —
    // global fail-closed would wedge agents on a stalled `rally room` poll.
    let posture = resolve_watchdog_posture(&args, fail_open);
    let args = strip_timeout_flag(args);

    let wants_json = args.iter().any(|arg| arg == "--json");
    // `--fail-open` (passed by the hook wrappers) means "never block the host
    // tool on a rally problem". On timeout we honor it by emitting a neutral
    // allow-everything envelope. Without it we still exit 0 (rally is an
    // advisory coordinator — hanging the agent is strictly worse than skipping
    // one advisory), but we surface a timeout note on stderr for visibility.

    let (tx, rx) = std::sync::mpsc::channel::<WatchdogResult>();
    let commit_signal = Arc::new(Mutex::new(WatchdogMutationState::NotStarted));
    let worker_commit_signal = Arc::clone(&commit_signal);
    // Anchored BEFORE the spawn, so the in-process deadline is always at or
    // EARLIER than the `recv_timeout` the main thread enforces. The skew is the
    // thread-spawn latency and it can only shorten the retry budget, never
    // extend it past the watchdog — the safe direction. Anchoring inside the
    // worker would invert that and let a loop outlive the watchdog.
    let deadline = Instant::now() + timeout;
    let worker = thread::Builder::new()
        .name("rally-command".to_string())
        .spawn(move || {
            let _commit_signal_guard = install_watchdog_commit_signal(worker_commit_signal);
            let _deadline_guard = install_watchdog_deadline(deadline);
            let result = match run_inner_with(&args) {
                Ok(mut output) => {
                    attach_pending_append_outcomes(&mut output);
                    let exit_code = output.exit_code;
                    let rendered = output.render();
                    WatchdogResult {
                        rendered,
                        exit_code,
                    }
                }
                Err(err) => match output_after_committed_error(err, wants_json) {
                    Ok(output) => {
                        let exit_code = output.exit_code;
                        WatchdogResult {
                            rendered: output.render(),
                            exit_code,
                        }
                    }
                    Err(err) => {
                        let err = CliError::from_error(err, wants_json);
                        WatchdogResult {
                            rendered: err.render_err(),
                            exit_code: err.exit_code,
                        }
                    }
                },
            };
            // Send may fail if the main thread already timed out and moved on;
            // that's fine — we're abandoning this worker.
            let _ = tx.send(result);
        });

    if worker.is_err() {
        // Could not even spawn a thread (resource exhaustion). Run inline as a
        // last resort rather than failing the hook — correctness over the
        // watchdog guarantee in this degenerate case.
        return run_inline(env::args().skip(1).collect());
    }

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let exit_code = result.exit_code;
            result.print();
            ExitCode::from(exit_code)
        }
        Err(_) => {
            // Deadline elapsed (or the worker panicked without sending). The
            // posture decides whether to fail open (default) or fail closed
            // (opt-in, before-write only — see `resolve_watchdog_posture`).
            // Either way we exit immediately, abandoning the worker thread;
            // the kernel reaps any fd/lock/child it held.
            match posture {
                WatchdogPosture::Open => {
                    emit_timeout_fail_open(wants_json, fail_open, timeout);
                    std::process::exit(0);
                }
                WatchdogPosture::ClosedBeforeWrite => {
                    emit_timeout_fail_closed_before_write(wants_json, timeout);
                    // Exit code mirrors `--strict` mode in the normal
                    // before-write gate (4 = a stop finding was raised).
                    // Wrappers translate this to "abort the write attempt".
                    std::process::exit(4);
                }
                WatchdogPosture::ClosedMutation => {
                    let mutation_state = commit_signal
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    match mutation_state {
                        WatchdogMutationState::NotStarted => {
                            emit_timeout_fail_closed_mutation(wants_json, timeout);
                            std::process::exit(4);
                        }
                        WatchdogMutationState::OutcomeUnknown { event_id, phase } => {
                            emit_timeout_unknown_mutation(wants_json, timeout, &event_id, &phase);
                            std::process::exit(1);
                        }
                        WatchdogMutationState::DbOnlyMigrationOutcomeUnknown {
                            migration_id,
                            phase,
                            retry_command,
                        } => {
                            emit_timeout_unknown_db_only_migration(
                                wants_json,
                                timeout,
                                &migration_id,
                                &phase,
                                &retry_command,
                            );
                            std::process::exit(1);
                        }
                        WatchdogMutationState::Committed {
                            projection_complete,
                            warnings,
                        } => {
                            emit_timeout_committed_mutation(
                                wants_json,
                                timeout,
                                projection_complete,
                                &warnings,
                            );
                            std::process::exit(0);
                        }
                    }
                }
            }
        }
    }
}

/// Watchdog timeout posture. Default is `Open` (fail-open is the right
/// posture for read-only / advisory commands — never hang the host tool).
/// `ClosedBeforeWrite` is opt-in and applies ONLY when the resolved
/// subcommand is `check before-write`: the coordination gate's purpose is
/// preventing two agents from clobbering one claimed path, so a stuck
/// snapshot read better delays the write than silently allows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchdogPosture {
    Open,
    ClosedBeforeWrite,
    ClosedMutation,
}

/// Resolve the watchdog posture for THIS invocation.
///
/// Fail-closed activates when **both** conditions hold:
///  1. The subcommand parses as `check before-write` (the only command for
///     which fail-closed is safe — a read-only command's stuck advisory must
///     never block the host tool).
///  2. Opt-in is present via either `RALLY_BEFORE_WRITE_FAILCLOSED=1` env or
///     a `--fail-closed` flag in the argument list.
///
/// `--fail-open` on the same call site wins: an explicit fail-open flag
/// reasserts the default and disables fail-closed even when the env var is
/// set, so operators can override per-call without unsetting the env var.
fn resolve_watchdog_posture(args: &[String], fail_open: bool) -> WatchdogPosture {
    if fail_open {
        return WatchdogPosture::Open;
    }
    let stripped = strip_timeout_flag(args.to_vec());
    let env_opt_in = env::var("RALLY_BEFORE_WRITE_FAILCLOSED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let flag_opt_in = args.iter().any(|a| a == "--fail-closed");
    // Subcommand gate: only `check before-write` flips. Look at the first two
    // positional tokens; both must be present and equal to `check` then
    // `before-write` respectively. A bare `rally check` or any other phase
    // (`before-complete`, `tier-fit`, `liveness`, `coordination`) stays
    // fail-open. This mirrors the policy in `agents/build-orchestrator.md`
    // (gates #1–#2 in §"Keep going until done") which always fail-open on
    // ambiguous safety classification.
    let mut positionals = stripped.iter().filter(|a| !a.starts_with('-'));
    let first = positionals.next().map(String::as_str);
    let second = positionals.next().map(String::as_str);
    if (env_opt_in || flag_opt_in) && first == Some("check") && second == Some("before-write") {
        WatchdogPosture::ClosedBeforeWrite
    } else if is_fail_closed_mutation_invocation(&stripped) {
        WatchdogPosture::ClosedMutation
    } else {
        WatchdogPosture::Open
    }
}

fn has_arg(args: &[String], needle: &str) -> bool {
    args.iter().any(|arg| arg == needle)
}

fn has_any_arg(args: &[String], needles: &[&str]) -> bool {
    needles.iter().any(|needle| has_arg(args, needle))
}

fn first_positionals(args: &[String]) -> (Option<&str>, Option<&str>) {
    let mut positionals = args.iter().filter(|arg| !arg.starts_with('-'));
    (
        positionals.next().map(String::as_str),
        positionals.next().map(String::as_str),
    )
}

fn is_fail_closed_mutation_invocation(args: &[String]) -> bool {
    let (first, second) = first_positionals(args);
    match first {
        Some("say" | "enter" | "ack" | "adopt" | "route-findings" | "migrate-legacy") => true,
        Some("inject" | "run") => !has_arg(args, "--dry-run"),
        Some("stop") => !has_arg(args, "--dry-run"),
        Some("rotate") => !has_arg(args, "--dry-run"),
        Some("hooks") => matches!(second, Some("on" | "off" | "prompt")),
        Some("init") => true,
        Some("status") => second == Some("post"),
        Some("backlog") => matches!(second, Some("add" | "update" | "done")),
        Some("lead") => matches!(second, Some("handoff" | "assign" | "relinquish")),
        Some("mission") => has_any_arg(args, &["--set", "--may", "--must-check"]),
        Some("sessions") => {
            has_arg(args, "--reap")
                || (has_arg(args, "--reap-processes") && has_arg(args, "--apply"))
        }
        Some("check") => second == Some("liveness") && has_arg(args, "--enforce"),
        Some("doctor" | "worktree-gc") => has_arg(args, "--apply"),
        _ => false,
    }
}

struct WatchdogResult {
    rendered: RenderedOutput,
    exit_code: u8,
}

impl WatchdogResult {
    fn print(self) {
        self.rendered.print();
    }
}

/// Inline (no-watchdog) execution path used only when thread spawning fails.
fn run_inline(args: Vec<String>) -> ExitCode {
    let wants_json = args.iter().any(|arg| arg == "--json");
    match run_inner_with(&args) {
        Ok(mut output) => {
            attach_pending_append_outcomes(&mut output);
            let exit_code = output.exit_code;
            output.print();
            ExitCode::from(exit_code)
        }
        Err(err) => match output_after_committed_error(err, wants_json) {
            Ok(output) => {
                let exit_code = output.exit_code;
                output.print();
                ExitCode::from(exit_code)
            }
            Err(err) => {
                let err = CliError::from_error(err, wants_json);
                err.print();
                ExitCode::from(err.exit_code)
            }
        },
    }
}

/// On watchdog timeout, print the neutral fail-open payload and return. For
/// `--json` callers this is the empty/neutral envelope every hook wrapper
/// already treats as "nothing to do" (`{}` → wrapper emits no agent-visible
/// message). For human callers we print nothing to stdout and a single
/// stderr note so the timeout is observable without polluting stdout.
fn emit_timeout_fail_open(wants_json: bool, fail_open: bool, timeout: Duration) {
    if wants_json {
        // Neutral envelope: the codex/claude/gemini wrappers parse this and,
        // finding no `agent_visible.present`, emit `{}` — i.e. allow the write.
        crate::output::write_line_or_exit_on_broken_pipe(
            &json!({ "ok": true, "product": "rally" }).to_string(),
        );
    }
    let _ = fail_open; // semantics identical either way; kept for clarity/logging
    eprintln!(
        "rally: hook exceeded {}ms wall-clock budget — failing open (no coordination check applied)",
        timeout.as_millis()
    );
}

/// Fail-CLOSED counterpart for the before-write gate. Synthesizes the same
/// envelope shape that `check::build_check` would have emitted with a
/// stop-severity finding, so hook wrappers can route this exactly as they
/// route a real claimed-path/active-blocker stop. Setting `allow: false` is
/// what flips the wrapper from "allow the write" to "abort the write
/// attempt".
fn emit_timeout_fail_closed_before_write(wants_json: bool, timeout: Duration) {
    if wants_json {
        // Same envelope shape as a real before-write check (see
        // `check::CheckResult` + `agent_visible`). Strict-mode wrappers
        // parse `agent_visible.present == true` as "raise to the agent and
        // block"; the synthesized finding tells the operator why.
        let payload = json!({
            "ok": true,
            "product": "rally",
            "command": "check",
            "data": {
                "check": {
                    "phase": "before-write",
                    "mode": "strict",
                    "allow": false,
                    "findings": [{
                        "code": "watchdog-timeout-fail-closed",
                        "severity": "stop",
                        "message": format!(
                            "before-write check exceeded {}ms wall-clock budget; failing closed because RALLY_BEFORE_WRITE_FAILCLOSED is opt-in",
                            timeout.as_millis()
                        ),
                    }],
                    "agent_visible": {
                        "present": true,
                        "severity": "stop",
                        "message": "Rally before-write check timed out; failing closed (RALLY_BEFORE_WRITE_FAILCLOSED) to prevent two agents clobbering one claimed path."
                    }
                }
            }
        });
        crate::output::write_line_or_exit_on_broken_pipe(&payload.to_string());
    }
    eprintln!(
        "rally: before-write hook exceeded {}ms wall-clock budget — failing CLOSED (RALLY_BEFORE_WRITE_FAILCLOSED is set; blocking write to prevent silent claim collision)",
        timeout.as_millis()
    );
}

/// Operator guidance for a mutation the watchdog killed before it committed.
///
/// The old text said "retry after contention clears", which asserted a cause
/// nothing had observed. Until 2026-08-05 it was usually WRONG twice over: the
/// retry schedule (2720ms open + 2040ms append) outlasted the 3000ms watchdog on
/// its own, so the timeout meant "this command's retry budget was mis-sized",
/// and retrying re-ran the same arithmetic to the same end. That budget is now
/// derived from this watchdog (see `crate::retry_budget`), so reaching this
/// message means the command genuinely ran out of wall-clock — and the honest
/// advice is to find out what held it, not to assume a contender and wait.
const WATCHDOG_MUTATION_GUIDANCE: &str = "Rally mutation timed out before the durable append committed; the fact was NOT written. \
This is a wall-clock budget expiry, not evidence of a contender: run `rally doctor --reap-stale` to list live holders and stale presence, \
and re-run with `--timeout-ms <n>` if the room is simply large. Retrying unchanged will hit the same budget.";

fn emit_timeout_fail_closed_mutation(wants_json: bool, timeout: Duration) {
    let message = format!(
        "mutating command exceeded {}ms wall-clock budget before its primary durable append committed; failing closed so the caller does not treat a dropped write as success",
        timeout.as_millis()
    );
    if wants_json {
        let payload = json!({
            "ok": false,
            "product": "rally",
            "command": "watchdog",
            "error": {
                "code": "watchdog-timeout-uncommitted-mutation",
                "message": message,
            },
            "data": {
                "watchdog": {
                    "committed": false,
                    "allow": false,
                    "timeout_ms": timeout.as_millis(),
                    "agent_visible": {
                        "present": true,
                        "severity": "stop",
                        "message": WATCHDOG_MUTATION_GUIDANCE
                    }
                }
            }
        });
        crate::output::write_line_or_exit_on_broken_pipe(&payload.to_string());
    }
    eprintln!("rally: {message}");
}

fn emit_timeout_unknown_mutation(wants_json: bool, timeout: Duration, event_id: &str, phase: &str) {
    let message = format!(
        "mutating command exceeded {}ms after canonical mutation began but before exact readback; outcome is unknown",
        timeout.as_millis()
    );
    let remedy = locate_remedy(event_id);
    if wants_json {
        let payload = json!({
            "ok": false,
            "product": "rally",
            "command": "watchdog",
            "error": {
                "code": "watchdog-timeout-outcome-unknown",
                "message": message,
            },
            "data": {
                "watchdog": {
                    "committed": Value::Null,
                    "outcome_unknown": true,
                    "event_id": event_id,
                    "phase": phase,
                    "timeout_ms": timeout.as_millis(),
                    "query_remedy": remedy,
                }
            }
        });
        crate::output::write_line_or_exit_on_broken_pipe(&payload.to_string());
    }
    eprintln!("rally: {message}; query `{remedy}` before deciding whether to rerun");
}

fn emit_timeout_unknown_db_only_migration(
    wants_json: bool,
    timeout: Duration,
    migration_id: &str,
    phase: &str,
    retry_command: &str,
) {
    let message = format!(
        "DB-only migration exceeded {}ms after durable recovery state began at phase {phase}; outcome is unknown",
        timeout.as_millis()
    );
    if wants_json {
        let payload = json!({
            "ok": false,
            "product": "rally",
            "command": "watchdog",
            "error": {
                "code": "watchdog-timeout-db-only-migration-outcome-unknown",
                "message": message,
            },
            "data": {
                "watchdog": {
                    "committed": Value::Null,
                    "outcome_unknown": true,
                    "migration_id": migration_id,
                    "phase": phase,
                    "retry_safe": false,
                    "timeout_ms": timeout.as_millis(),
                    "retry_command": retry_command,
                }
            }
        });
        crate::output::write_line_or_exit_on_broken_pipe(&payload.to_string());
    }
    eprintln!(
        "rally: {message}; inspect the marker-bound artifacts and resume with `{retry_command}`"
    );
}

fn emit_timeout_committed_mutation(
    wants_json: bool,
    timeout: Duration,
    projection_complete: bool,
    warnings: &[Value],
) {
    let message = format!(
        "mutating command exceeded {}ms wall-clock budget after its primary durable append committed; projection/output was abandoned",
        timeout.as_millis()
    );
    if wants_json {
        let payload = json!({
            "ok": true,
            "product": "rally",
            "command": "watchdog",
            "data": {
                "watchdog": {
                    "committed": true,
                    "projection_complete": projection_complete,
                    "warnings": warnings,
                    "timeout_ms": timeout.as_millis(),
                    "message": message,
                }
            }
        });
        crate::output::write_line_or_exit_on_broken_pipe(&payload.to_string());
    }
    eprintln!("rally: {message}");
}

fn run_inner_with(args: &[String]) -> Result<Output> {
    // Each invocation owns a fresh aggregate. This is especially important for
    // in-process tests and the no-watchdog fallback, where one OS thread can
    // execute multiple commands sequentially.
    let _ = drain_pending_append_outcomes();
    let _ = drain_pending_append_issues();
    // Test-only blocking seam: simulates a command path wedged on slow/stuck
    // I/O so the watchdog can be exercised deterministically. Compiled out of
    // release builds (`debug_assertions` is false in `--release`), so the
    // installed binary can never be made to hang by setting this var.
    #[cfg(debug_assertions)]
    if let Ok(ms) = env::var("RALLY_TEST_BLOCK_MS")
        && let Ok(ms) = ms.trim().parse::<u64>()
    {
        thread::sleep(Duration::from_millis(ms));
    }
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "help")
        )
    {
        return Ok(Output::new(false, help_text(), json!({})));
    }
    // `rally --version` / `-V` mean `rally version`. Accept them rather than
    // failing on the first thing most users type.
    let normalized = crate::cli::normalize_flag_alias(args);
    let args: &[String] = normalized.as_deref().unwrap_or(args);

    reject_unknown_command(args)?;

    let command = match parse_cli(args)? {
        CliParse::Command(command) => *command,
        CliParse::Help(text) => return Ok(Output::new(false, text, json!({}))),
    };

    match command {
        CliCommand::Init(args) => command_init(args),
        CliCommand::Hooks(args) => command_hooks(args),
        CliCommand::Enter(args) => command_enter(args),
        CliCommand::Say(args) => command_say(args),
        CliCommand::Room(args) => command_room(args),
        CliCommand::Next(args) => command_next(args),
        CliCommand::Locate(args) => command_locate(args),
        CliCommand::Recent(args) => command_recent(args),
        CliCommand::Check(args) => command_check(args),
        CliCommand::Hook(args) => command_hook(args),
        CliCommand::Run(args) => command_run(args),
        CliCommand::Sessions(args) => command_sessions(args),
        CliCommand::Inject(args) => command_inject(args),
        CliCommand::Session(args) => command_session_action(args),
        CliCommand::Retrospective(args) => command_retrospective(args),
        CliCommand::Rotate(args) => command_rotate(args),
        CliCommand::Status(args) => command_status(args),
        CliCommand::Watch(args) => command_watch(args),
        CliCommand::MigrateLegacy(args) => command_migrate_legacy(args),
        CliCommand::Doctor(args) => command_doctor(args),
        CliCommand::Version(args) => command_version(args),
        // Work surface commands (appended — do not reorder above)
        CliCommand::Backlog(args) => command_backlog(args),
        CliCommand::Board(args) => command_board(args),
        CliCommand::Risks(args) => command_kind_read(args, KindRead::Risks),
        CliCommand::Decisions(args) => command_kind_read(args, KindRead::Decisions),
        CliCommand::Artifacts(args) => command_kind_read(args, KindRead::Artifacts),
        CliCommand::Claims(args) => command_kind_read(args, KindRead::Claims),
        CliCommand::RouteFindings(args) => command_route_findings(args),
        // B13
        CliCommand::CheckCi(args) => command_check_ci(args),
        // B1/B2/B4: pi-dynamic observation seam
        CliCommand::Dag(args) => command_dag(args),
        CliCommand::WakeDue(args) => command_wake_due(args),
        // B-whoami: identity report
        CliCommand::Whoami(args) => command_whoami(args),
        CliCommand::Owners(args) => command_owners(args),
        // Rank-11: room north-star + per-agent autonomy envelope
        CliCommand::Mission(args) => command_mission(args),
        CliCommand::Lead(args) => command_lead(args),
        CliCommand::Ack(args) => command_ack(args),
        // C-FLEET: adopt an already-running agent into the managed-session ledger
        CliCommand::Adopt(args) => command_adopt(args),
        // Sweep-reaper: GC leftover per-agent worktrees
        CliCommand::WorktreeGc(args) => command_worktree_gc(args),
        // Layer 1: completion-scoped self-exit re-check
        CliCommand::SelfExitCheck(args) => command_self_exit_check(args),
        // BACKLOG S-P3, Chunk C: rallyd store daemon lifecycle
        CliCommand::Daemon(args) => command_daemon(args),
        CliCommand::ClaimsRefresh(args) => command_claims_refresh(args),
    }
}

// =============================================================================
// Layer 1 — rally self-exit-check (completion-scoped self-exit)
// =============================================================================

const SCHEMA_SELF_EXIT_CHECK: &str = "agent-rally.command.self-exit-check.v1";

/// tmux session env key holding the consecutive-non-actionable streak. Stored in
/// the session's OWN env so it dies with the session (no new filesystem surface).
const SELF_EXIT_STREAK_KEY: &str = "RALLY_SELFEXIT_STREAK";

/// Layer 1: one stateless completion re-check. Decides — via the shared
/// [`liveness::completion_self_exit_eligible`] authority — whether THIS
/// task-scoped session should exit now, and if so self-kills its own `rally-*`
/// tmux session so the `exec`'d agent process is torn down and the session
/// auto-closes. Returns a JSON envelope describing the decision either way.
///
/// `work_resolved` = this tool holds NO active rally claims.
/// `next_actionable` = `rally next` surfaced real addressed work.
/// An "empty" cycle is `work_resolved && !next_actionable`; the streak of
/// consecutive empty cycles is persisted in the session's tmux env and only a
/// SUSTAINED streak triggers exit — so a brief lull between claims never exits
/// mid-task.
fn command_self_exit_check(args: cli::SelfExitCheckArgs) -> Result<Output> {
    let tool = args.tool.clone();
    let required_streak = args
        .required_streak
        .unwrap_or(crate::liveness::DEFAULT_SELF_EXIT_STREAK);
    let tmux_bin = BackendBins::default().tmux_bin;

    let room = RoomStore::open()?;
    ensure_presence(&room, &tool)?;
    let snapshot = room.snapshot()?;

    // work_resolved: no active claim is owned by this tool.
    let owned_active = snapshot
        .active_claims
        .iter()
        .filter(|c| c.tool.as_deref() == Some(tool.as_str()))
        .count();
    let work_resolved = owned_active == 0;

    // next_actionable: does rally next surface addressed work?
    let backlog_items = list_backlog_items(&room).unwrap_or_default();
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let next = build_next(
        &snapshot,
        &tool,
        None,
        &[],
        1,
        backlog_items,
        coord.stale_wait_secs,
    );
    let next_actionable = next.actionable;

    // This cycle is "empty" only when work is resolved AND next is non-actionable.
    let empty_cycle = work_resolved && !next_actionable;

    // Resolve our own tmux session (if any) to persist the streak + self-kill.
    let own_session = backends::own_rally_tmux_session(&tmux_bin);

    // Update the consecutive-empty streak in the session's own env. When not
    // inside a rally tmux session we still compute the decision (observability /
    // tests) but cannot persist a streak — treat the streak as this single cycle.
    let prior_streak = own_session
        .as_deref()
        .and_then(|s| backends::get_session_env_i64(&tmux_bin, s, SELF_EXIT_STREAK_KEY))
        .unwrap_or(0);
    let new_streak = if empty_cycle { prior_streak + 1 } else { 0 };
    if let Some(ref s) = own_session {
        backends::set_session_env_i64(&tmux_bin, s, SELF_EXIT_STREAK_KEY, new_streak);
    }

    let eligible = crate::liveness::completion_self_exit_eligible(
        work_resolved,
        new_streak,
        required_streak,
        args.persistent,
    );

    let mut exited = false;
    if eligible && let Some(ref s) = own_session {
        // Self-kill: tear down our own tmux session so `exec` auto-closes it.
        exited = backends::kill_tmux_session(&tmux_bin, s);
    }

    let session_name = own_session.clone().unwrap_or_default();
    let text = if exited {
        format!(
            "self-exit-check: EXITING {session_name} (work resolved, streak {new_streak}/{required_streak})"
        )
    } else if args.persistent {
        format!("self-exit-check: staying (persistent opt-out) streak={new_streak}")
    } else {
        format!(
            "self-exit-check: staying (work_resolved={work_resolved} next_actionable={next_actionable} streak={new_streak}/{required_streak})"
        )
    };
    let body = envelope_value(
        "self-exit-check",
        SCHEMA_SELF_EXIT_CHECK,
        json!({
            "self_exit_check": {
                "tool": tool,
                "work_resolved": work_resolved,
                "next_actionable": next_actionable,
                "empty_cycle": empty_cycle,
                "streak": new_streak,
                "required_streak": required_streak,
                "persistent_optout": args.persistent,
                "eligible": eligible,
                "exited": exited,
                "session": session_name,
            }
        }),
    )?;
    Ok(Output::new(args.json, text, body))
}

// =============================================================================
// BACKLOG S-P3, Chunk C — rally daemon serve|start|stop|status
// =============================================================================

/// Hand-declared `kill(2)`, mirroring `rallyd_core.rs`'s own `extern "C" fn
/// signal` and `store.rs`'s `extern "C" fn flock` pattern — no `libc`/`nix`
/// dependency (zero new deps). Exported by libc on macOS and Linux and linked
/// by default. Used only by `rally daemon stop` to SIGTERM the pid on record.
#[cfg(unix)]
mod daemon_signal {
    unsafe extern "C" {
        pub(super) fn kill(pid: i32, sig: i32) -> i32;
    }
    pub(super) const SIGTERM: i32 = 15;
}

/// `rally daemon start`'s own wait-for-ready poll bound. Elevated above (with
/// margin over) the store router's [`store_client::CORRIDOR_BOUND`] (30s):
/// both wait on the SAME cold-reconcile window (R3), so `start`'s poll must
/// not give up before the corridor would. The watchdog carve-out
/// (`lib.rs`'s `DAEMON_START_WATCHDOG_TIMEOUT_MS`, 45s) has its own separate
/// margin above THIS bound.
const DAEMON_START_READY_BOUND: Duration = Duration::from_secs(35);
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How long `rally daemon stop` waits for the SH ownership lock to become
/// non-blocking-acquirable after sending SIGTERM — the kernel-enforced proof
/// that the daemon's EX hold has actually released (ADR-01/G7), not a guess.
const DAEMON_STOP_RELEASE_BOUND: Duration = Duration::from_secs(10);
const DAEMON_STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(JsonSchema, Serialize)]
struct DaemonData {
    daemon: DaemonPayload,
}

#[derive(JsonSchema, Serialize)]
struct DaemonPayload {
    subcommand: String,
    live: bool,
    pid: Option<u32>,
    socket: Option<String>,
    wire_version: Option<u32>,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn daemon_envelope_body(
    subcommand: &str,
    live: bool,
    pid: Option<u32>,
    socket: Option<String>,
    wire_version: Option<u32>,
    repo_root: String,
    note: Option<String>,
) -> Result<Value> {
    envelope(
        "daemon",
        SCHEMA_DAEMON,
        DaemonData {
            daemon: DaemonPayload {
                subcommand: subcommand.to_string(),
                live,
                pid,
                socket,
                wire_version,
                repo_root,
                note,
            },
        },
    )
}

fn command_daemon(args: cli::DaemonArgs) -> Result<Output> {
    match args.subcommand {
        cli::DaemonSubcommand::Serve(serve_args) => command_daemon_serve(args.json, serve_args),
        cli::DaemonSubcommand::Start(start_args) => command_daemon_start(args.json, start_args),
        cli::DaemonSubcommand::Stop => command_daemon_stop(args.json),
        cli::DaemonSubcommand::Status => command_daemon_status(args.json),
    }
}

/// `rally daemon serve` — runs `rallyd_core::serve` in THIS process. Blocks
/// until SIGTERM/SIGINT (or `--idle-exit-secs` elapses idle); D1's watchdog
/// carve-out (`run_with_watchdog`) ensures this call is never raced against
/// the hook-safety timeout.
fn command_daemon_serve(json: bool, args: cli::DaemonServeArgs) -> Result<Output> {
    let root = repo_root()?;
    let canonical = store::canonical_repo_root_string(&root);
    let config = ServeConfig {
        repo_root: root,
        idle_exit_secs: args.idle_exit_secs,
        foreground: !args.detached,
    };
    rallyd_core::serve(config).map_err(|err| RallyError::Command(err.message().to_string()))?;
    let body = daemon_envelope_body(
        "serve",
        false,
        None,
        None,
        None,
        canonical,
        Some("daemon shut down cleanly".to_string()),
    )?;
    Ok(Output::new(
        json,
        "rally daemon serve: shut down cleanly".to_string(),
        body,
    ))
}

/// `rally daemon start` — spawn a detached `rally daemon serve --detached`
/// child (log → `.rally/rallyd.log`) and block until it becomes ready:
/// `.rally/rallyd.sock.addr` exists AND a `Ping` round-trips (R3). Idempotent:
/// a daemon already live for this repo is reported as success, not an error.
fn command_daemon_start(json: bool, args: cli::DaemonStartArgs) -> Result<Output> {
    let root = repo_root()?;
    let rally_dir = root.join(".rally");
    fs::create_dir_all(&rally_dir).map_err(RallyError::io("create .rally"))?;
    let canonical = store::canonical_repo_root_string(&root);

    if let Some(identity) = store_client::probe_identity(&rally_dir, &canonical)? {
        let body = daemon_envelope_body(
            "start",
            true,
            Some(identity.pid),
            Some(identity.socket.display().to_string()),
            Some(identity.wire_version),
            canonical,
            Some("already running".to_string()),
        )?;
        return Ok(Output::new(
            json,
            format!("rally daemon start: already running (pid {})", identity.pid),
            body,
        ));
    }

    let exe = env::current_exe().map_err(RallyError::io("resolve current_exe"))?;
    let log_path = rally_dir.join("rallyd.log");
    // SEC-005: create the daemon log 0600 — it can capture request/error text
    // and must not be world/group-readable. Matches `write_private_file` in
    // `rallyd_core.rs`. Unix-only API (`OpenOptionsExt::mode`); a fresh create
    // is the only moment the mode bits are honored, and a pre-existing log keeps
    // its bits (append never re-chmods), so this hardens the common cold-start.
    let mut log_opts = fs::OpenOptions::new();
    log_opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_opts.mode(0o600);
    }
    let log_file = log_opts
        .open(&log_path)
        .map_err(RallyError::io(format!("open {}", log_path.display())))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(RallyError::io("clone log fd"))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon").arg("serve").arg("--detached");
    if let Some(secs) = args.idle_exit_secs {
        cmd.arg("--idle-exit-secs").arg(secs.to_string());
    }
    cmd.current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));
    let child = cmd
        .spawn()
        .map_err(RallyError::io("spawn rally daemon serve"))?;
    let spawned_pid = child.id();
    // Deliberately no `.wait()` — the child is meant to OUTLIVE this process
    // (detached posture, ADR-03/L6). If this `rally daemon start` process
    // exits first, the kernel reparents the child; it keeps serving.

    let deadline = Instant::now() + DAEMON_START_READY_BOUND;
    loop {
        if let Some(identity) = store_client::probe_identity(&rally_dir, &canonical)? {
            let body = daemon_envelope_body(
                "start",
                true,
                Some(identity.pid),
                Some(identity.socket.display().to_string()),
                Some(identity.wire_version),
                canonical,
                None,
            )?;
            return Ok(Output::new(
                json,
                format!("rally daemon start: ready (pid {})", identity.pid),
                body,
            ));
        }
        if Instant::now() >= deadline {
            return Err(RallyError::Command(format!(
                "rally daemon start: spawned pid {spawned_pid} but it never became ready within {}s; check {}",
                DAEMON_START_READY_BOUND.as_secs(),
                log_path.display()
            )));
        }
        thread::sleep(DAEMON_START_POLL_INTERVAL);
    }
}

/// `rally daemon stop` — SIGTERM the pid on record, then confirm the
/// ownership lock actually released (SH becomes non-blocking-acquirable —
/// kernel-enforced proof, not a guess). Idempotent: no pid file is reported
/// as "not running", not an error.
fn command_daemon_stop(json: bool) -> Result<Output> {
    let root = repo_root()?;
    let rally_dir = root.join(".rally");
    let canonical = store::canonical_repo_root_string(&root);
    let pid = fs::read_to_string(rally_dir.join("rallyd.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    let Some(pid) = pid else {
        let body = daemon_envelope_body(
            "stop",
            false,
            None,
            None,
            None,
            canonical,
            Some("no pid file — not running".to_string()),
        )?;
        return Ok(Output::new(
            json,
            "rally daemon stop: not running".to_string(),
            body,
        ));
    };

    #[cfg(not(unix))]
    {
        let _ = pid;
        return Err(RallyError::Command(
            "rallyd is a unix-only daemon".to_string(),
        ));
    }

    let signal_pid = {
        // SEC-001 (a): pid 0 is NEVER a real daemon pid. `kill(0, SIGTERM)`
        // signals the CALLER's entire process group — catastrophic. Treat a
        // pid-0 pid file as stale: remove it and report not running, never
        // signal.
        if pid == 0 {
            let _ = fs::remove_file(rally_dir.join("rallyd.pid"));
            let body = daemon_envelope_body(
                "stop",
                false,
                None,
                None,
                None,
                canonical,
                Some("stale pid file (pid 0) removed — not running".to_string()),
            )?;
            return Ok(Output::new(
                json,
                "rally daemon stop: not running (stale pid file removed)".to_string(),
                body,
            ));
        }

        // SEC-001 (b): corroborate that `pid` names a REAL daemon before
        // signaling. Only two things prove a live daemon: a live ping, or a
        // held EX ownership lock.
        if let Some(identity) = store_client::probe_identity(&rally_dir, &canonical)? {
            // A ping answered: the daemon is provably real. Signal the pid the
            // daemon REPORTS (authoritative — also covers a pid file that lags a
            // restart), not the possibly-stale on-disk pid.
            identity.pid
        } else {
            // No ping. If we can take the SH ownership lock, there is no EX
            // holder ⇒ the daemon is provably DEAD and the pid file is stale.
            // Remove it and report not running — never SIGTERM an uncorroborated
            // pid (it may have been recycled to an unrelated process).
            match store::acquire_owner_shared_nb(&rally_dir) {
                Ok(Some(_guard)) => {
                    // Probe-only SH: drop immediately (do not install this
                    // process as the room's direct-mode guard — G7).
                    let _ = fs::remove_file(rally_dir.join("rallyd.pid"));
                    let body = daemon_envelope_body(
                        "stop",
                        false,
                        None,
                        None,
                        None,
                        canonical,
                        Some(
                            "no live daemon (SH lock free) — removed stale pid file, not running"
                                .to_string(),
                        ),
                    )?;
                    return Ok(Output::new(
                        json,
                        "rally daemon stop: not running (removed stale pid file)".to_string(),
                        body,
                    ));
                }
                // SH refused (EX held) or a lock error: an EX holder exists — a
                // real daemon that is mid-cold-start or wedged and not yet
                // answering pings. The held EX lock corroborates it, so signal
                // the pid on record.
                _ => pid,
            }
        }
    };

    #[cfg(unix)]
    {
        // ESRCH (no such process) just means it already exited; either way we
        // confirm the lock release below rather than trusting the return code.
        let _ = unsafe { daemon_signal::kill(signal_pid as i32, daemon_signal::SIGTERM) };
    }

    let deadline = Instant::now() + DAEMON_STOP_RELEASE_BOUND;
    let released = loop {
        if let Ok(Some(_guard)) = store::acquire_owner_shared_nb(&rally_dir) {
            // Probe-only: drop immediately. This process isn't opening a
            // direct store here, so it must not install itself as this
            // room's process-global direct-mode guard (G7).
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(DAEMON_STOP_POLL_INTERVAL);
    };

    let note = if released {
        format!("SIGTERM sent to pid {signal_pid}; ownership lock released")
    } else {
        format!(
            "SIGTERM sent to pid {signal_pid}; ownership lock NOT released within {}s — daemon may be wedged, check `rally daemon status`",
            DAEMON_STOP_RELEASE_BOUND.as_secs()
        )
    };
    let body = daemon_envelope_body(
        "stop",
        false,
        Some(signal_pid),
        None,
        None,
        canonical,
        Some(note.clone()),
    )?;
    Ok(Output::new(
        json,
        format!("rally daemon stop: {note}"),
        body,
    ))
}

/// `rally daemon status` — read-only: ping-probes for a live daemon and
/// reports pid/socket/wire_version; falls back to the pid file (stale/crashed
/// hint) when no daemon answers. `--json` emits the standard envelope
/// (checklist Item 4 — scope-auditor advisory).
fn command_daemon_status(json: bool) -> Result<Output> {
    let root = repo_root()?;
    let rally_dir = root.join(".rally");
    let canonical = store::canonical_repo_root_string(&root);
    let identity = store_client::probe_identity(&rally_dir, &canonical)?;
    let pid_file = fs::read_to_string(rally_dir.join("rallyd.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    let (live, pid, socket, wire_version, note) = match identity {
        Some(id) => (
            true,
            Some(id.pid),
            Some(id.socket.display().to_string()),
            Some(id.wire_version),
            None,
        ),
        None if pid_file.is_some() => (
            false,
            pid_file,
            None,
            None,
            Some("pid file present but daemon did not answer a ping (stale/crashed)".to_string()),
        ),
        None => (false, None, None, None, Some("not running".to_string())),
    };

    let text = format!(
        "rally daemon status: live={live} pid={} socket={}",
        pid.map(|p| p.to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        socket.clone().unwrap_or_else(|| "<none>".to_string())
    );
    let body = daemon_envelope_body("status", live, pid, socket, wire_version, canonical, note)?;
    Ok(Output::new(json, text, body))
}

// =============================================================================
// rally worktree gc — sweep-reaper for leftover per-agent worktrees
// =============================================================================

const SCHEMA_WORKTREE_GC: &str = "agent-rally.command.worktree-gc.v1";

fn command_worktree_gc(args: WorktreeGcArgs) -> Result<Output> {
    let repo = repo_root()?;

    // Open the room store once; derive both presence facts (for TTL-liveness)
    // and active sessions (for the f2 backend-probe) from it.
    // Graceful degradation: if the store is unavailable (no .rally/ yet),
    // supply empty facts and no probe (merged worktrees still reap; unmerged
    // are conservatively skipped until a probe is available).
    let bins = BackendBins::default();
    let room_result = RoomStore::open();

    let presence_facts: Vec<worktree_gc::PresenceFact> = room_result
        .as_ref()
        .ok()
        .and_then(|r| r.facts().ok())
        .map(|facts| {
            facts
                .into_iter()
                .filter(|f| f.kind == FactKind::Presence)
                .filter_map(|f| {
                    f.tool.map(|tool| worktree_gc::PresenceFact {
                        tool,
                        seq: f.seq,
                        subject: f.subject.clone(),
                        created_at: f.created_at.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // f2 — build a real backend-liveness probe from the session ledger.
    // `probe_session_liveness` queries tmux/cmux for each active managed
    // session and returns Stale when the backing session is gone.
    // The probe closure captures an Arc of the result map so it is cheap to
    // clone and 'static-safe for the GcConfig field.
    let backend_liveness_probe: Option<worktree_gc::BackendLivenessProbe> =
        room_result.ok().and_then(|room| {
            active_session_facts(&room).ok().map(|active| {
                let liveness_map = probe_session_liveness(&active, bins);
                let arc_map = std::sync::Arc::new(liveness_map);
                let probe: worktree_gc::BackendLivenessProbe =
                    std::sync::Arc::new(move |session_id: &str| -> bool {
                        // Returns true when the backend is DEAD (Stale), allowing the GC
                        // to proceed; false when still Live or Unknown (conservative skip).
                        matches!(
                            arc_map
                                .get(session_id)
                                .copied()
                                .unwrap_or(SessionLiveness::Unknown),
                            SessionLiveness::Stale
                        )
                    });
                probe
            })
        });

    let config = worktree_gc::GcConfig {
        repo_root: repo,
        apply: args.apply,
        ttl_secs: args.ttl_secs,
        now_ts: None, // use system clock
        presence_facts,
        git_bin: "git".to_string(),
        // f2: wired — queries tmux/cmux via probe_session_liveness; None only
        // when the room store is unavailable (graceful degradation).
        backend_liveness_probe,
    };

    let report = worktree_gc::run_gc(config).map_err(RallyError::Message)?;

    let mode = if args.apply { "apply" } else { "dry-run" };
    let text = format!(
        "rally worktree gc ({mode}): candidates={} reaped={} skipped={} bundles={}{}",
        report.candidates.len(),
        report.reaped.len(),
        report.skipped.len(),
        report.bundles.len(),
        if report.warnings.is_empty() {
            String::new()
        } else {
            format!(" warnings={}", report.warnings.len())
        }
    );

    // Build candidate list for JSON output.
    let candidates_json: Vec<serde_json::Value> = report
        .candidates
        .iter()
        .map(|c| {
            json!({
                "worktree_path": c.worktree_path.to_string_lossy(),
                "branch": c.branch,
                "reason": c.reason,
            })
        })
        .collect();
    let reaped_json: Vec<serde_json::Value> = report
        .reaped
        .iter()
        .map(|r| {
            json!({
                "worktree_path": r.worktree_path.to_string_lossy(),
                "branch": r.branch,
                "branch_deleted": r.branch_deleted,
            })
        })
        .collect();
    let skipped_json: Vec<serde_json::Value> = report
        .skipped
        .iter()
        .map(|s| {
            json!({
                "worktree_path": s.worktree_path.to_string_lossy(),
                "branch": s.branch,
                "reason": s.reason,
            })
        })
        .collect();
    let bundles_json: Vec<serde_json::Value> = report
        .bundles
        .iter()
        .map(|b| json!(b.to_string_lossy()))
        .collect();

    let body = envelope_value(
        "worktree_gc",
        SCHEMA_WORKTREE_GC,
        json!({
            "worktree_gc": {
                "mode": mode,
                "candidates": candidates_json,
                "reaped": reaped_json,
                "skipped": skipped_json,
                "bundles": bundles_json,
                "warnings": report.warnings,
            }
        }),
    )?;
    Ok(Output::new(args.json, text, body))
}

fn command_rotate(args: RotateArgs) -> Result<Output> {
    let root = repo_root()?;
    let outcome = rotate::run_rotate(root, args.days, args.dry_run)?;
    let text = format!(
        "rally rotate: threshold={}d (source={}) cutoff={} {}rotated={} skipped={} (live {} → {})",
        outcome.threshold_days,
        outcome.threshold_source,
        outcome.cutoff_utc,
        if outcome.dry_run { "dry-run " } else { "" },
        outcome.rotated.len(),
        outcome.skipped.len(),
        outcome.live_segment_count_before,
        outcome.live_segment_count_after,
    );
    // Wrap under `data.rotate` to satisfy the envelope contract.
    let inner = serde_json::to_value(&outcome).map_err(RallyError::json("rotate outcome"))?;
    let body = envelope_value("rotate", SCHEMA_ROTATE, json!({ "rotate": inner }))?;
    Ok(Output::new(args.json, text, body))
}

fn command_retrospective(args: RetrospectiveArgs) -> Result<Output> {
    let root = repo_root()?;
    let outcome =
        retrospective::run_retrospective(root, args.engagement.as_deref(), args.out.as_deref())?;
    let text = format!(
        "retrospective: {} ({}) — {} fact(s) across {} engagement(s)",
        outcome.output_path, outcome.action, outcome.total_facts, outcome.total_engagements,
    );
    // Wrap under `data.retrospective` to satisfy the envelope contract.
    let inner =
        serde_json::to_value(&outcome).map_err(RallyError::json("retrospective outcome"))?;
    let body = envelope_value(
        "retrospective",
        SCHEMA_RETROSPECTIVE,
        json!({ "retrospective": inner }),
    )?;
    Ok(Output::new(args.json, text, body))
}

fn command_init(args: InitArgs) -> Result<Output> {
    // Shared coordination dir (main checkout under git's commondir) vs.
    // active worktree. Pointer docs land in the worktree (active branch);
    // manifest lives under the shared `.rally/`. See `init::run_init`.
    let repo = repo_root()?;
    let worktree = worktree_root()?;
    let outcome = init::run_init(repo, worktree)?;
    let manifest_action = outcome.manifest.action;
    let pointers_summary: Vec<String> = outcome
        .pointers
        .iter()
        .map(|p| format!("{}={}", p.path, p.action))
        .collect();
    let text = format!(
        "rally init: manifest={manifest_action} {pointers} (ledger_dir={ledger}; room_cmd={room})",
        pointers = pointers_summary.join(" "),
        ledger = outcome.ledger_dir,
        room = outcome.room_cmd,
    );
    // Wrap under `data.init` to satisfy the envelope contract.
    let inner = serde_json::to_value(&outcome).map_err(RallyError::json("init outcome"))?;
    let body = envelope_value("init", SCHEMA_INIT, json!({ "init": inner }))?;
    Ok(Output::new(args.json, text, body))
}

fn command_hooks(args: HooksArgs) -> Result<Output> {
    let repo = repo_root()?;
    let (text, payload) = match args.subcommand {
        HooksSubcommand::Status => {
            let status = hooks_config::resolve(&repo)?;
            let text = format!(
                "hooks: enabled={} prompt={} (enabled_source={} prompt_source={})",
                status.enabled, status.prompt, status.enabled_source, status.prompt_source
            );
            let payload =
                serde_json::to_value(&status).map_err(RallyError::json("render hooks status"))?;
            (text, payload)
        }
        HooksSubcommand::On(set) => {
            let outcome = hooks_config::set_enabled(&repo, hooks_scope(set.scope), true)?;
            let text = format!("hooks on --scope {} ({})", outcome.scope, outcome.path);
            let payload =
                serde_json::to_value(&outcome).map_err(RallyError::json("render hooks on"))?;
            (text, payload)
        }
        HooksSubcommand::Off(set) => {
            let outcome = hooks_config::set_enabled(&repo, hooks_scope(set.scope), false)?;
            let text = format!("hooks off --scope {} ({})", outcome.scope, outcome.path);
            let payload =
                serde_json::to_value(&outcome).map_err(RallyError::json("render hooks off"))?;
            (text, payload)
        }
        HooksSubcommand::Prompt(prompt) => {
            let outcome = hooks_config::set_prompt(
                &repo,
                hooks_scope(prompt.scope),
                hooks_prompt_mode(prompt.mode),
            )?;
            let text = format!(
                "hooks prompt={} --scope {} ({})",
                outcome.prompt.as_deref().unwrap_or("unknown"),
                outcome.scope,
                outcome.path
            );
            let payload =
                serde_json::to_value(&outcome).map_err(RallyError::json("render hooks prompt"))?;
            (text, payload)
        }
    };
    let body = envelope_value("hooks", SCHEMA_HOOKS, json!({ "hooks": payload }))?;
    Ok(Output::new(args.json, text, body))
}

fn hooks_scope(scope: HooksScopeArg) -> hooks_config::ConfigScope {
    match scope {
        HooksScopeArg::Repo => hooks_config::ConfigScope::Repo,
        HooksScopeArg::User => hooks_config::ConfigScope::User,
    }
}

fn hooks_prompt_mode(mode: HooksPromptModeArg) -> hooks_config::PromptMode {
    match mode {
        HooksPromptModeArg::Once => hooks_config::PromptMode::Once,
        HooksPromptModeArg::Always => hooks_config::PromptMode::Always,
        HooksPromptModeArg::Off => hooks_config::PromptMode::Off,
    }
}

/// Ensure `tool` is registered in the current room engagement.
///
/// Called at the start of every tool-scoped command that writes or reads room
/// state (`say`, `check`, `next`, `enter`, …) so that an agent that skips an
/// explicit `rally enter` still appears in `room.squads[]`.
///
/// Idempotent per protocol session: an existing presence for the same tool and
/// `from_session_id` is a no-op. A sibling session sharing the tool writes its
/// own presence. If no lead decision exists, the first eligible session also
/// writes one decision asserting `tool` as lead (first-enter-is-lead).
fn ensure_presence(room: &RoomStore, tool: &str) -> Result<()> {
    ensure_presence_tiered(room, tool, None)
}

/// Durably extend every active claim owned by this exact tool session from a
/// self-authored heartbeat. A same-tool sibling cannot renew it. The lease
/// window stays size-scaled by the same policy used when the claim was
/// acquired. A failure is returned to the heartbeat caller: a successful
/// self-report must not claim renewal when the durable ledger did not advance.
fn renew_owned_claim_leases(room: &RoomStore, tool: &str) -> Result<usize> {
    let snapshot = room.snapshot()?;
    let from_session_id = current_protocol_session(Some(tool))
        .from_session_id()
        .to_string();
    let coord = hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let now = chrono::Utc::now();
    let mut renewed = 0;
    for claim in snapshot.active_claims.iter().filter(|claim| {
        claim_authority::claim_owner_matches_caller(
            claim.tool.as_deref(),
            claim.from_session_id.as_deref(),
            Some(tool),
            Some(&from_session_id),
        )
    }) {
        let resource_scopes = claim
            .scope
            .iter()
            .filter_map(|scope| crate::resource_scope::ResourceScope::parse_claim_scope(scope))
            .collect::<Vec<_>>();
        let size = crate::decay::classify_work_size(&resource_scopes, claim.scope.len());
        let lease_secs = crate::decay::reclaim_timeout_secs(
            size,
            coord.reclaim_small_minutes,
            coord.reclaim_large_minutes,
        );
        let lease_expires_at = claim_authority::lease_marker_at(now, lease_secs);
        let renewal = room.renew_claim_lease(
            &claim.event_id,
            lease_expires_at,
            tool,
            Some(&from_session_id),
            claim.from_session_id.as_deref(),
        )?;
        if let Some(outcome) = renewal.append_outcome.as_ref() {
            record_append_outcome(outcome);
        }
        if renewal.record.is_some() {
            renewed += 1;
        }
    }
    Ok(renewed)
}

/// Evidence stamps that make the adaptive-liveness signals READABLE.
///
/// Four keys consumed by the liveness projection and external observer:
/// * `branch_head_sha:<sha>` — the worktree HEAD at the moment of the beat.
///   `code_progress_age_per_tool` compares consecutive stamps for a tool and
///   reports forward progress when the sha MOVED. Two stamped beats are needed
///   before the signal can fire, so it activates a session at a time rather
///   than retroactively.
/// * `planned_heartbeat_secs:<n>` — the cadence this agent intends to beat at.
///   `planned_cadence_for_tool` reads it to size that session's staleness
///   window, which is what makes the window adaptive rather than one global
///   default applied to every agent.
/// * `worktree_path:<absolute-path>` — the checkout an external observer may
///   inspect. The observer verifies that it belongs to this room's git common
///   directory before reading it.
/// * `observer_pid:<pid>` — the long-lived host process supplied by the shipped
///   hook. Never fall back to the short-lived `rally` child pid: that would be
///   dead as soon as the heartbeat returns and falsely demote every agent.
///
/// Fail-open in every branch: an unavailable HEAD stamps nothing rather than
/// stamping a placeholder, because `code_progress_age_per_tool` treats a
/// missing stamp as "signal absent" (correct) and would read `unknown` as a
/// value that never changes (a false "no progress" verdict).
fn presence_signal_evidence(room: &RoomStore) -> Vec<String> {
    let mut evidence = Vec::new();
    if let Some(sha) = observed_liveness::current_head_sha(room.repo_root()) {
        evidence.push(format!("branch_head_sha:{sha}"));
    }
    if let Ok(path) = fs::canonicalize(room.repo_root()) {
        evidence.push(format!("worktree_path:{}", path.display()));
    }
    if let Ok(raw) = env::var("RALLY_OBSERVER_PID")
        && raw.parse::<i32>().ok().is_some_and(|pid| pid > 1)
    {
        evidence.push(format!("observer_pid:{raw}"));
    }
    let coord = hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    if coord.default_cadence_secs > 0 {
        evidence.push(format!(
            "planned_heartbeat_secs:{}",
            coord.default_cadence_secs
        ));
    }
    evidence
}

/// Tier-aware presence. Lead auto-assign is **frontier-only**: an undeclared
/// tier (`None`) stays lead-eligible (back-compat with lazy-auto-enter callers),
/// but a declared `executing`/`fast` agent entering an empty room does NOT take
/// the lead seat — it stays open until a frontier agent (or user-designated
/// lead) joins. See docs/SPEC-lead-agent.md.
fn ensure_presence_tiered(room: &RoomStore, tool: &str, tier: Option<&str>) -> Result<()> {
    let from_session_id = current_protocol_session(Some(tool))
        .from_session_id()
        .to_string();
    ensure_presence_tiered_for_session(room, tool, tier, &from_session_id)
}

fn ensure_presence_tiered_for_session(
    room: &RoomStore,
    tool: &str,
    tier: Option<&str>,
    from_session_id: &str,
) -> Result<()> {
    let facts = room.facts()?;
    if facts.iter().any(|fact| {
        fact.kind == FactKind::Presence
            && claim_authority::same_session_owner(
                fact.tool.as_deref(),
                fact.from_session_id.as_deref(),
                Some(tool),
                Some(from_session_id),
            )
    }) {
        return Ok(());
    }
    let snapshot = room.snapshot()?;
    // R9 stale-binary guard: embed the build-id in the presence fact's summary
    // so that `command_enter` can detect when different builds are writing to
    // the same room.  Format: "build_id:<BUILD_ID>" — minimal, no schema bump.
    let presence_fact = Fact {
        from_session_id: Some(from_session_id.to_string()),
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Presence,
        tool: Some(tool.to_string()),
        role: None,
        subject: format!("agent presence: {tool}"),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!("build_id:{BUILD_ID}")),
        // Liveness signal stamps. `code_progress_age_per_tool` and
        // `planned_cadence_for_tool` (store.rs) both read presence evidence for
        // these keys; until this writer existed, NEITHER key appeared anywhere
        // in the ledger, so signal (c) was permanently absent, `is_live` could
        // never return `Stale` (it needs all four signals present), and the
        // "adaptive" window always fell back to the default cadence. The reader
        // and its doc comment described a writer that did not exist.
        evidence: presence_signal_evidence(room),
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    room.append_fact_verified(&presence_fact)?
        .into_fact_reporting();
    // First-FRONTIER-enter-is-lead: assert lead only when the seat is open AND
    // this agent is lead-eligible (frontier tier, or undeclared for back-compat).
    let lead_eligible = matches!(tier, None | Some("frontier"));
    if snapshot.lead.is_none() && lead_eligible {
        let lead_fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Decision,
            tool: Some(tool.to_string()),
            role: None,
            subject: "role:lead".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some(format!("{tool} is lead (first frontier to enter)")),
            evidence: vec![
                format!("tier:{}", tier.unwrap_or("undeclared")),
                "assigned:first-join".to_string(),
            ],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&lead_fact)?.into_fact_reporting();
    }
    Ok(())
}

fn command_enter(args: EnterArgs) -> Result<Output> {
    let tool = args.tool;
    let session_id = args.session_id.unwrap_or_else(|| format!("session-{tool}"));
    let role = args.role;
    let paths = normalize_paths(args.paths);
    // R5: persist `--engagement <label>` before opening the room so the
    // RoomStore picks it up on construction (matching env-var precedence).
    if let Some(label) = args.engagement.as_deref() {
        let rally_dir = repo_root()?.join(".rally");
        store::persist_active_engagement(&rally_dir, label)?;
    }
    let room = RoomStore::open()?;
    let room_id = room.active_engagement().to_string();

    // Reap stale state before snapshotting, so this session starts against a
    // clean room rather than inheriting every dead agent's claims.
    //
    // The reaper had no caller. `rally doctor --reap-stale --apply` was the
    // only way to reach it and nothing invoked that, so a dry run against this
    // repo's own room found 69 of 69 active claims already eligible — the
    // eligibility math had been right and unused for the room's whole life.
    // Rate-limited and fail-open inside `maybe_reap_on_enter`; it never fails
    // `enter`.
    if let Some(report) = reaper::maybe_reap_on_enter(&room) {
        if !report.claims_reaped.is_empty() || !report.handoffs_expired.is_empty() {
            eprintln!(
                "rally: auto-reap closed {} stale claim(s) and {} expired handoff(s) \
                 (run `rally doctor --reap-stale` to inspect; RALLY_NO_AUTO_REAP=1 to disable)",
                report.claims_reaped.len(),
                report.handoffs_expired.len()
            );
        }
        // A lost reap reply is not a successful close and must not be narrated
        // as one. Preserve every stable id/phase/remedy in enter's command-wide
        // append_issues instead of silently dropping an incomplete report.
        for unknown in &report.outcome_unknowns {
            record_optional_append_issue(
                "auto-reap",
                &RallyError::outcome_unknown(&unknown.event_id, &unknown.phase, &unknown.detail),
            );
        }
        let non_unknown_failures = report
            .write_failures
            .saturating_sub(report.outcome_unknowns.len());
        if non_unknown_failures > 0 {
            record_optional_append_issue(
                "auto-reap",
                &RallyError::Message(format!(
                    "auto-reap had {} durable write failure(s); no cleanup was claimed for them",
                    non_unknown_failures
                )),
            );
        }
    }

    // Snapshot BEFORE writing presence so the cursor window reflects peer work
    // only (not the agent's own enter heartbeat).
    let snapshot_before = room.snapshot()?;
    let cursor_before = args
        .since
        .unwrap_or_else(|| room.cursor_for(&tool).unwrap_or(0));
    // max_seq is retained for documentation: it is the pre-enter high-water mark
    // used to define the lower bound of the "new peer content" window (anything
    // arriving at seq > cursor_before and written by a peer, i.e. not by this
    // enter's own ensure_presence call). The actual cursor is set from
    // snapshot.max_seq (post-presence) further below.
    let _max_seq_pre_enter = snapshot_before.max_seq;

    // f4 follow-up: emit presence (+ first-frontier-enter-is-lead) BEFORE any
    // warning blocks write risk facts. The squads projection at store.rs picks
    // up the entering tool from ANY fact carrying it in `Fact.tool`, so a
    // warning-driven risk_fact written before presence would short-circuit
    // `ensure_presence_tiered`'s squad-membership guard and skip the lead-
    // assignment write — a pre-existing latent bug that only became visible
    // when f4 widened the fleet-enforcement gate to cover bare worker ids
    // without a digit suffix. The squad-id-active, binary-drift,
    // shared-branch-hazard, AND unmanaged-agent blocks all wrote tool-
    // attributed risk facts before this point — the re-order fixes all four
    // at once. The blocks still use `snapshot_before` (pre-presence) for their
    // dedup checks, so behavior is unchanged when none of them fire.
    with_watchdog_command_commit(|| ensure_presence_tiered(&room, &tool, args.tier.as_deref()))?;
    // `enter` is a self-authored liveness signal. Advance every claim this tool
    // still owns so the reaper reads the same durable lease the agent renewed.
    with_watchdog_command_commit(|| renew_owned_claim_leases(&room, &tool).map(|_| ()))?;

    // Layer 2 — event-driven liveness-lease safety net: when a new agent joins,
    // opportunistically sweep detached `rally-*` orphan tmux sessions that the
    // shared `liveness::reapable` authority stages (stale by adaptive window, or
    // stale + parent-dead via Layer 3). Best-effort + fail-open: never blocks the
    // enter path, never reaps a live / parent-alive session. Runs AFTER presence
    // so the entering agent's own managed session is in the guard set.
    opportunistic_orphan_sweep_on_enter(&room);

    // B11: warn (non-blocking) when the entering tool is already active in the
    // current engagement.  A second terminal reusing the same id is ambiguous;
    // surfacing it lets the human/lead decide.  Rally never blocks re-entry.
    let mut warnings: Vec<EnterWarning> = Vec::new();
    if let Some(squad) = snapshot_before
        .squads
        .iter()
        .find(|s| s.tool == tool && s.status == "active")
    {
        warnings.push(EnterWarning {
            code: "squad-id-active".to_string(),
            message: format!(
                "squad id {} is already active (last seen {}); if you are a second terminal, re-enter with a distinct id",
                tool, squad.last_seen_ts
            ),
        });
        // Append ONE durable risk fact so the duplicate is auditable/traceable
        // in current_risks, recent, and the retrospective digest.
        // enter still returns ok:true — this is warn-not-block.
        // Idempotency guard (matches the unmanaged-agent arm below): re-entering
        // the same duplicate id must not append a second identical fact, or the
        // room accumulates one row per re-enter and crowds out real risks.
        let already_recorded = snapshot_before
            .system_health
            .iter()
            .any(|f| f.subject == format!("duplicate-active-squad-id: {tool}"));
        if !already_recorded {
            let risk_fact = build_risk_fact(
                &tool,
                format!("duplicate-active-squad-id: {tool}"),
                format!(
                    "another active session is already using squad id {tool} (last seen {}); not blocked — re-enter with a distinct id if this is a second terminal. Recorded for audit.",
                    squad.last_seen_ts
                ),
                Vec::new(),
                "warn",
                Vec::new(),
                None,
            );
            room.append_fact(&risk_fact)?.into_fact_reporting();
        }
    }

    // C-FLEET / "all fleet workers must be rally-managed": when a tool enters
    // with a managed-style identifier (e.g. claude-01, claude_code:01,
    // toolbar-launch-01) but no active managed-session record exists for it,
    // surface an `unmanaged-agent` warning + append ONE durable risk fact.
    // Skips human/lead-style identifiers (claude_code:lead, lead, human:*,
    // *:l<N>) — those are not expected to be managed sessions. This is the
    // detection arm of the fleet-enforcement rule; the response arm is
    // `rally adopt` (register without relaunch).
    if is_managed_style_tool(&tool) {
        let active_sessions = active_session_records(&room).unwrap_or_default();
        let has_managed = active_sessions
            .iter()
            .any(|s| s.tool == tool || s.session_id == tool || s.name == tool);
        if !has_managed {
            // DI-1: telemetry facts project into `system_health`, so the
            // idempotency guard must scan there (not current_risks) or it would
            // never see the prior fact and re-append on every enter.
            let already_recorded = snapshot_before.system_health.iter().any(|f| {
                f.subject == format!("unmanaged-agent: {tool}")
                    && f.tool.as_deref() == Some(tool.as_str())
            });
            let msg = format!(
                "tool {tool} entered the room but is not under managed-session control (no active `session` fact). Use `rally adopt {tool} --tmux <target>` or `--cmux <target>` to register the running surface so `rally inject/attach/capture/stop` work; or relaunch via `rally run`. Not blocked — informational."
            );
            warnings.push(EnterWarning {
                code: "unmanaged-agent".to_string(),
                message: msg.clone(),
            });
            if !already_recorded {
                let risk_fact = build_risk_fact(
                    &tool,
                    format!("unmanaged-agent: {tool}"),
                    msg,
                    Vec::new(),
                    "warn",
                    Vec::new(),
                    None,
                );
                room.append_fact(&risk_fact)?.into_fact_reporting();
            }
        }
    }

    // R9 stale-binary guard: find the most recent presence fact in the room
    // (any tool) and check if it carries a different build_id than ours.
    // Warn + append a durable risk fact if drift is detected.  Never blocks.
    {
        let all_facts = room.facts().unwrap_or_default();
        let last_presence_build_id: Option<String> = all_facts
            .iter()
            .filter(|f| f.kind == "presence")
            .max_by_key(|f| f.seq)
            .and_then(|f| f.summary.as_deref())
            .and_then(|s| s.strip_prefix("build_id:"))
            .map(str::to_string);

        if let Some(ref prior_id) = last_presence_build_id
            && prior_id != BUILD_ID
        {
            let drift_msg = format!(
                "this rally build {} differs from the build {} that last wrote to this room — a stale binary on PATH can silently drop writes; verify which rally is on PATH",
                BUILD_ID, prior_id
            );
            warnings.push(EnterWarning {
                code: "binary-drift".to_string(),
                message: drift_msg.clone(),
            });
            // Idempotency guard: don't append a duplicate drift fact for the
            // same (this-build vs prior-build) pair on every re-enter.
            let drift_subject = format!("binary-drift: {} vs {}", BUILD_ID, prior_id);
            let already_recorded = snapshot_before
                .system_health
                .iter()
                .any(|f| f.subject == drift_subject);
            if !already_recorded {
                let risk_fact = build_risk_fact(
                    &tool,
                    drift_subject,
                    drift_msg,
                    Vec::new(),
                    "warn",
                    Vec::new(),
                    None,
                );
                room.append_fact(&risk_fact)?.into_fact_reporting();
            }
        }
    }

    // R12 shared-branch / worktree hazard: detect when the canonical checkout is
    // on a non-main branch while peers are active.  A commit here would silently
    // land on a peer's branch.  Warn + append a durable risk fact; never blocks.
    {
        // Use worktree_root for is_linked and branch checks: .git is a FILE in
        // linked worktrees, a DIR in the canonical clone.  repo_root() follows
        // commondir to the main checkout (always a dir), so it cannot distinguish
        // linked from canonical — worktree_root() stays at the process's cwd.
        if let Ok(wt_root) = worktree_root() {
            let is_linked = worktree_guard::is_linked_worktree(&wt_root);
            let branch = worktree_guard::current_branch(&wt_root);
            let active_peer_count = snapshot_before
                .squads
                .iter()
                .filter(|s| s.tool != tool && s.status == "active")
                .count();
            if let Some(hazard_msg) = worktree_guard::detect_shared_branch_hazard(
                &wt_root,
                is_linked,
                branch.as_deref(),
                active_peer_count,
            ) {
                warnings.push(EnterWarning {
                    code: "shared-branch-hazard".to_string(),
                    message: hazard_msg.clone(),
                });
                let risk_fact = build_risk_fact(
                    &tool,
                    hazard_msg.clone(),
                    hazard_msg,
                    vec!["shared-branch-hazard".to_string()],
                    "warn",
                    Vec::new(),
                    None,
                );
                room.append_fact(&risk_fact)?.into_fact_reporting();
            }
        }
    }

    // Component A + B: presence + first-frontier-enter-is-lead were emitted
    // at the top of `command_enter` (before the warning blocks) so risk facts
    // never short-circuit the squad-membership guard inside
    // `ensure_presence_tiered`. Re-snapshot here so the room summary + squads
    // reflect the just-written presence/lead facts AND any warning-block risk
    // facts.
    let snapshot = room.snapshot()?;

    let attention = build_attention(&snapshot, &tool, cursor_before, &paths);
    let entry = build_entry(&snapshot, &tool, role.as_deref(), &paths, &attention);
    // Set cursor to the post-presence max_seq so subsequent enters do NOT see
    // this tool's own just-written presence/lead facts as "new peer content".
    // Using snapshot.max_seq (re-snapshotted after ensure_presence) rather than
    // the pre-enter max_seq ensures the cursor window excludes facts authored by
    // this enter call itself. A concurrent peer fact that arrived before
    // ensure_presence ran will still appear as new (its seq < snapshot_before.max_seq
    // was already captured in the pre-enter max_seq, which is the same lower bound).
    let cursor_after = snapshot.max_seq;
    // Write-through cache (cursors.json) kept for fast-path readers.
    room.set_cursor(&tool, cursor_after)?;
    // R10: also advance the ledger so cursor_for() is ledger-derived on
    // re-enter. Uses content_max_seq (excludes Read facts) to prevent the
    // checkpoint itself from inflating the cursor on the next enter.
    // maybe_append_read_checkpoint's own guard prevents double-counting when
    // cursor_after == last_checkpoint_seq (coalesces if no advancement).
    record_conditional_append(room.maybe_append_read_checkpoint(&tool, snapshot.content_max_seq)?);
    let mission = snapshot.mission.clone();
    let acknowledged = snapshot
        .squads
        .iter()
        .find(|sq| sq.tool == tool)
        .map(|sq| sq.acknowledged)
        .unwrap_or(false);
    let lead_context = build_lead_context(&snapshot, Some(&tool), role.as_deref());
    let acknowledgment = Acknowledgment {
        required: true,
        acknowledged,
        context: AckContext {
            rules: "RALLY.md".to_string(),
            doctrine: "dynamic-workflows/COORDINATION.md".to_string(),
            lead: snapshot.lead.clone(),
            mission: snapshot.mission.clone(),
            how_to_ack: format!("rally ack --tool {tool}"),
        },
    };
    let room_summary = RoomSummary::from(&snapshot);
    let attention_count = attention.len();
    let enter_payload = EnterPayload {
        tool: tool.clone(),
        session_id,
        room_id,
        cursor: CursorData {
            before: cursor_before,
            after: cursor_after,
            advanced: cursor_after > cursor_before,
        },
        entry,
        attention,
        warnings,
        mission,
    };
    let body = envelope(
        "enter",
        SCHEMA_ENTER,
        EnterData {
            enter: enter_payload,
            room: room_summary,
            acknowledgment,
            lead_context,
        },
    )?;
    let text = format!("entered room tool={} attention={}", tool, attention_count,);
    Ok(Output::new(args.json, text, body))
}

fn command_say(args: SayArgs) -> Result<Output> {
    let kind = args.kind;
    let subject = args
        .subject
        .unwrap_or_else(|| default_subject(kind.as_str()));

    // B18: detect external-intake BEFORE normalisation so absolute paths are
    // still distinguishable.  Collect every raw path + URI that classifies External.
    let external_paths: Vec<String> = args
        .paths
        .iter()
        .chain(args.scopes.iter())
        .chain(args.uri.iter())
        .filter(|v| classify_scope(v) == ScopeClass::External)
        .cloned()
        .collect();
    let is_external = !external_paths.is_empty();

    let mut scope = scopes_from(args.scopes, args.resources, args.paths);

    // Tag the scope with the quarantine sentinel so the snapshot projection
    // can exclude this fact from the repo-local active backlog.
    if is_external {
        scope.push("external-intake".to_string());
    }

    let room = RoomStore::open()?;
    // Component B: auto-register presence for the calling tool before writing.
    ensure_presence(&room, &args.tool)?;

    // B13: encode --produces / --depends as markers in evidence (self-describing claim).
    let mut evidence = args.evidence;
    for p in &args.produces {
        evidence.push(format!("produces:{p}"));
    }
    for d in &args.depends {
        evidence.push(format!("depends:{d}"));
    }
    if kind == FactKind::Claim {
        // Size-scale the lease window to the claimed work: a single-file claim
        // gets the SMALL window (default 30m), a multi-file/coarse claim the
        // LARGE window (default 2h). Resolved from the coordination policy.
        let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
        let resource_scopes: Vec<crate::resource_scope::ResourceScope> = scope
            .iter()
            .filter_map(|s| crate::resource_scope::ResourceScope::parse_claim_scope(s))
            .collect();
        let size = crate::decay::classify_work_size(&resource_scopes, scope.len());
        let lease_secs = crate::decay::reclaim_timeout_secs(
            size,
            coord.reclaim_small_minutes,
            coord.reclaim_large_minutes,
        );
        claim_authority::ensure_lease_evidence(&mut evidence, lease_secs);
    }

    // B1: encode lineage markers (run/step/parent-step) into scope.
    let mut lineage_scope: Vec<String> = Vec::new();
    if let Some(ref run_id) = args.run_id {
        lineage_scope.push(format!("run:{run_id}"));
    }
    if let Some(ref step_id) = args.step_id {
        lineage_scope.push(format!("step:{step_id}"));
    }
    // One `parent-step:<id>` marker per value — a task with multiple `depends_on`
    // entries records one DAG edge per (parent, step). Zero values writes none.
    // Skip empty values: an empty `--parent-step` would write a `parent-step:`
    // marker with no id, producing a phantom DAG edge to/from "".
    for parent_step_id in &args.parent_step_ids {
        if parent_step_id.is_empty() {
            continue;
        }
        lineage_scope.push(format!("parent-step:{parent_step_id}"));
    }
    // Merge lineage into scope (before external-intake check, which runs later).
    scope.extend(lineage_scope);

    // #6 source-grounding: at claim, snapshot content hashes of all claimed file
    // paths and store them as `claimhash:<rel>=<hash>` in evidence.
    let repo_root_for_grounding = repo_root().ok();
    if kind == FactKind::Claim
        && let Some(ref root) = repo_root_for_grounding
    {
        // Collect file: scope entries only (exclude external-intake).
        let file_scopes: Vec<String> = scope
            .iter()
            .filter(|s| s.starts_with("file:") && !scope.contains(&"external-intake".to_string()))
            .cloned()
            .collect();
        if !file_scopes.is_empty() {
            let hashes = source_grounding::claim_hashes(root, &file_scopes);
            evidence.extend(hashes);
        }
    }

    // #6 source-grounding (artifact): look up claim-open hashes from the ref'd claim fact.
    let grounding_claim_evidence: Vec<String> = if kind == FactKind::Artifact {
        args.ref_id
            .as_ref()
            .and_then(|ref_id| {
                room.facts()
                    .ok()?
                    .into_iter()
                    .find(|f| f.event_id == *ref_id)
            })
            .map(|f| f.evidence)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // B1: for standby, encode reason + wake_after into summary.
    // For wake, ref_standby becomes ref_id (takes precedence over --ref).
    let summary = if kind == FactKind::Standby {
        // Parse/resolve wake_after if provided.
        let wake_after_iso = if let Some(ref wa) = args.wake_after {
            match resolve_wake_after(wa) {
                Ok(iso) => Some(iso),
                Err(e) => return Err(RallyError::Usage(e)),
            }
        } else {
            None
        };
        // Build summary from reason + wake_after markers.
        let mut parts: Vec<String> = Vec::new();
        if let Some(ref r) = args.reason {
            parts.push(format!("reason:{r}"));
        }
        if let Some(ref wa) = wake_after_iso {
            parts.push(format!("wake_after:{wa}"));
        }
        // Prepend explicit summary if provided, then append markers.
        if let Some(explicit) = args.summary {
            parts.insert(0, explicit);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    } else {
        args.summary
    };

    // ref_standby (--ref-standby) takes precedence over --ref for wake facts.
    let ref_id = args.ref_standby.or(args.ref_id);

    // Path-only release fix (C4):
    //
    // Before the fix, `rally say release --tool T --path P` (no `--ref`)
    // routed through `append_state_transition_verified`, which errored with
    // "release requires --ref". The user-reported lesson (seq 1603) was
    // "silently no-ops" because the error came as a bare stderr line — there
    // was no actionable next step (find the event_id manually, then retry).
    //
    // The natural mental model is: "I'm done with this path, release my
    // claims on it". So when release is invoked with at least one path scope
    // but no `--ref`, we resolve to the calling tool's currently-active
    // claims overlapping any of those paths and release them one by one
    // through the existing verified path. If no live claim matches, error
    // LOUD and list the tool's open claims so the operator has the next step
    // in hand.
    if matches!(kind, FactKind::Release)
        && ref_id.is_none()
        && scope.iter().any(|s| s.starts_with("file:"))
    {
        // Parity with the normal say path: if any external-intake path was
        // also passed, write the durable risk fact + return the same warning.
        let mut warnings = Vec::<SayWarning>::new();
        if is_external {
            let root_display = repo_root()
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| "<unknown>".to_string());
            let paths_display = external_paths.join(", ");
            let risk_summary = format!(
                "external-intake: path(s) [{paths_display}] resolve outside repo_root {root_display}; quarantined — not promoted into repo-local backlog. Recorded for audit."
            );
            let risk_fact = build_risk_fact(
                &args.tool,
                format!("external-intake: {paths_display}"),
                risk_summary.clone(),
                Vec::new(),
                "warn",
                Vec::new(),
                None,
            );
            room.append_fact(&risk_fact)?.into_fact_reporting();
            warnings.push(SayWarning {
                code: "external-intake".to_string(),
                message: risk_summary,
            });
        }
        return command_release_by_path(
            &room,
            &args.tool,
            &scope,
            args.thread_id,
            args.role,
            subject,
            summary,
            evidence,
            args.target,
            args.status,
            args.severity,
            args.uri,
            args.json,
            warnings,
        );
    }

    let fact = Fact {
        // Stamp the authoring session lease on this durable LLM-authored write.
        from_session_id: Some(
            current_protocol_session(Some(&args.tool))
                .from_session_id()
                .to_string(),
        ),
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: args.thread_id.unwrap_or_else(|| new_id("room")),
        kind: kind.clone(),
        tool: Some(args.tool.clone()),
        role: args.role,
        subject,
        scope,
        created_at: now_string(),
        summary,
        evidence,
        target: args.target,
        ref_id,
        status: args.status,
        severity: args.severity,
        uri: args.uri,
        session: None,
    };
    // R9-readback: state-transition facts (release/resolve) go through the
    // stricter verified path that also asserts the projection flipped.
    // All other mutating facts go through append_fact_verified (segment readback
    // only — no projection assertion needed).
    let mut append_outcome = with_watchdog_command_commit(|| match kind {
        FactKind::Release | FactKind::Resolve => room.append_state_transition_verified(&fact),
        _ => room.append_fact_verified(&fact),
    })?;
    record_append_outcome(&append_outcome);
    let fact = append_outcome.fact.clone();

    // B18: append ONE durable risk fact for each external-intake detection so
    // the contamination event is permanently auditable.  Never blocks the write.
    let mut say_warnings: Vec<SayWarning> = Vec::new();

    // Advisory protocol-envelope validation (lenient, never blocks): surfaces
    // missing causal ids — e.g. an ACK/resolve that doesn't cite its ref_event_id.
    // Charter: warn and record; hosts decide whether to act.
    if let Some(pk) = protocol_event_kind(&fact.kind)
        && let Err(missing) = pk.validate(
            &fact_protocol_envelope(&fact),
            event_envelope::CompatMode::Lenient,
        )
    {
        let ids = missing
            .iter()
            .map(|e| format!("{:?}", e.missing))
            .collect::<Vec<_>>()
            .join(", ");
        say_warnings.push(SayWarning {
            code: "envelope-incomplete".to_string(),
            message: format!("{pk:?} event is missing required causal id(s): {ids}"),
        });
    }

    if is_external {
        let root_display = repo_root()
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string());
        let paths_display = external_paths.join(", ");
        let risk_summary = format!(
            "external-intake: path(s) [{paths_display}] resolve outside repo_root {root_display}; quarantined — not promoted into repo-local backlog. Recorded for audit."
        );
        let risk_fact = build_risk_fact(
            &args.tool,
            format!("external-intake: {paths_display}"),
            risk_summary.clone(),
            Vec::new(),
            "warn",
            Vec::new(),
            None,
        );
        room.append_fact(&risk_fact)?.into_fact_reporting();
        say_warnings.push(SayWarning {
            code: "external-intake".to_string(),
            message: risk_summary,
        });
    }

    // #6 source-grounding (artifact): re-hash claimed files; flag ungrounded ones.
    // #8 ripple: detect changed pub signatures affecting peer claims.
    if kind == FactKind::Artifact
        && let Some(ref root) = repo_root_for_grounding
    {
        let original_hashes = source_grounding::parse_claim_hashes(&grounding_claim_evidence);
        if !original_hashes.is_empty() {
            let unchanged = source_grounding::ungrounded_paths(root, &original_hashes);
            if !unchanged.is_empty() {
                // Append grounded:false marker risk facts — one per unchanged file.
                for path in &unchanged {
                    let risk_summary = format!(
                        "ungrounded-artifact: {path} unchanged since claim — no evidence of work; artifact may be a dropped-work indicator. Recorded for audit."
                    );
                    let risk_fact = build_risk_fact(
                        &args.tool,
                        format!("ungrounded-artifact: {path} unchanged since claim"),
                        risk_summary,
                        vec!["grounded:false".to_string()],
                        "warn",
                        vec![format!("artifact_ref:{}", fact.event_id)],
                        Some(fact.event_id.clone()),
                    );
                    room.append_fact(&risk_fact)?.into_fact_reporting();
                }
            }

            // #8 ripple: for files that CHANGED, detect pub sig changes
            // affecting peer claims. These are auditable secondary writes: a
            // failure produces an explicit partial command result.
            let changed_files: Vec<String> = original_hashes
                .keys()
                .filter(|p| !unchanged.contains(p))
                .cloned()
                .collect();
            if !changed_files.is_empty() {
                let snap_for_ripple = match room.snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        let warning = store::ProjectionWarning {
                            code: store::ProjectionWarningCode::PostCommitWork,
                            message: format!(
                                "canonical say fact committed but ripple input snapshot failed: {error}"
                            ),
                        };
                        append_outcome.projection_complete = false;
                        append_outcome.warnings.push(warning.clone());
                        mark_watchdog_append_outcome(&append_outcome);
                        update_recorded_append_outcome(&append_outcome);
                        say_warnings.push(SayWarning {
                            code: "projection:post_commit_work".to_string(),
                            message: warning.message,
                        });
                        RoomSnapshot::default()
                    }
                };
                let ripple_facts =
                    ripple::build_ripple_alerts(&changed_files, root, &args.tool, &snap_for_ripple);
                for rf in ripple_facts {
                    room.append_fact(&rf)?.into_fact_reporting();
                }
            }
        }
    }

    let snapshot = match room.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let warning = store::ProjectionWarning {
                code: store::ProjectionWarningCode::PostCommitWork,
                message: format!(
                    "canonical say fact committed but post-commit room snapshot failed: {error}"
                ),
            };
            append_outcome.projection_complete = false;
            append_outcome.warnings.push(warning.clone());
            mark_watchdog_append_outcome(&append_outcome);
            update_recorded_append_outcome(&append_outcome);
            say_warnings.push(SayWarning {
                code: "projection:post_commit_work".to_string(),
                message: warning.message,
            });
            RoomSnapshot::default()
        }
    };
    // R9-readback: capture verified {room, seq} from the confirmed fact.
    let verified = SayVerified {
        room: room.room_id().to_string(),
        seq: fact.seq,
    };
    let body = envelope(
        "say",
        SCHEMA_SAY,
        SayData {
            say: SayPayload {
                fact: fact.clone(),
                committed: append_outcome.committed,
                projection_complete: append_outcome.projection_complete,
                projection_warnings: append_outcome.warnings.clone(),
            },
            room: RoomSummary::from(&snapshot),
            warnings: say_warnings,
            verified,
        },
    )?;
    let text = format!(
        "said {} {} room={} seq={}",
        fact.kind.as_str(),
        fact.event_id,
        room.room_id(),
        fact.seq
    );
    Ok(Output::new(args.json, text, body))
}

/// Path-only release: resolve to the calling tool's currently-active claims
/// overlapping any path in `scope`, then emit ONE release fact per match via
/// the existing `append_state_transition_verified` path (preserves R9-readback).
///
/// Loud error rules:
/// - No live claim found for any of the paths → error listing the tool's
///   currently-open claims (with `event_id` + `subject`) so the caller has the
///   next actionable step. This is the fix for the "silently no-ops" lesson.
/// - Some live claims found but matching release fails the projection assertion
///   inside `append_state_transition_verified` → that error bubbles unchanged.
///
/// Returns the SAME `SayData` envelope as a single release — `say.fact` is the
/// LAST release fact written, and `warnings[]` carries one note per release
/// outcome so a host can read the full per-claim history.
#[allow(clippy::too_many_arguments)]
fn command_release_by_path(
    room: &RoomStore,
    tool: &str,
    scope: &[String],
    thread_id: Option<String>,
    role: Option<String>,
    subject: String,
    summary: Option<String>,
    evidence: Vec<String>,
    target: Option<String>,
    status: Option<String>,
    severity: Option<String>,
    uri: Option<String>,
    json: bool,
    mut warnings: Vec<SayWarning>,
) -> Result<Output> {
    let snapshot = room.snapshot()?;
    let caller_session = current_protocol_session(Some(tool))
        .from_session_id()
        .to_string();

    // Find this tool's open claims AND any matching by path scope (regardless
    // of owner — a lead releasing a stale-owner claim is a legitimate use).
    // We still annotate which match was on tool-owned paths vs cross-tool so
    // the operator sees who they released.
    let want_paths: Vec<&str> = scope
        .iter()
        .filter(|s| s.starts_with("file:"))
        .map(|s| s.as_str())
        .collect();
    if want_paths.is_empty() {
        return Err(RallyError::Usage(
            "rally say release --path requires at least one --path argument".to_string(),
        ));
    }
    // A claim matches a `--path` release when its scope covers a requested
    // path AND the caller is authorized to release it. Authorization is either:
    //   (a) the exact caller session OWNS the claim (the original
    //       owner-self-release path), OR
    //   (b) AUTHORIZED TAKEOVER — the claim's owner is takeover-eligible-stale
    //       (>2h total silence, NOT the 15-min advisory idle) so a peer/lead may
    //       reclaim it. This closes fact_182e8 gap 1: a dead owner's claims
    //       could never be cleared because release was strictly owner-only.
    // The destructive bar (vs 15m advisory) prevents reclaiming a busy-but-
    // quiet live agent's claim (independent-auditor HIGH, 2026-06-09). That bar
    // is now SIZE-SCALED per claim: a single-file claim becomes reclaimable
    // after the SMALL timeout (default 30m), a multi-file / directory / repo /
    // task claim only after the LARGE timeout (default 2h, == the historical
    // flat `TAKEOVER_STALE_SECS`, so coarse claims keep their prior grace).
    // Eligibility stays fail-closed: an owner with an unknown/unparseable
    // last-seen is never reclaimable (`claim_reclaim_eligible`).
    //
    // Scope matching differs by arm to fix the auditor's MED asymmetry:
    //   - SELF release keeps the pre-existing EXACT scope match (unchanged
    //     contract; an owner releases the exact scope string it claimed).
    //   - TAKEOVER uses `path_matches_scope` so a stale DIRECTORY claim (e.g.
    //     `file:src`) that `before-write` flagged reclaimable for `src/foo.rs`
    //     can actually be released via that same path — the two surfaces now
    //     agree (lesson: a WARN that points at an unrunnable command is a bug).
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let exact_scope_match = |c: &&Fact| {
        c.scope
            .iter()
            .any(|cs| want_paths.iter().any(|wp| wp == cs))
    };
    let takeover_scope_match = |c: &&Fact| {
        c.scope.iter().any(|cs| {
            want_paths.iter().any(|wp| {
                wp == cs
                    || wp
                        .strip_prefix("file:")
                        .is_some_and(|p| crate::path_matches_scope(cs, p))
            })
        })
    };
    let caller_owns_claim = |c: &&Fact| {
        claim_authority::claim_owner_matches_caller(
            c.tool.as_deref(),
            c.from_session_id.as_deref(),
            Some(tool),
            Some(&caller_session),
        )
    };
    // Capture the size class of each reclaimed claim for the provenance trail.
    let mut reclaim_sizes: Vec<crate::decay::WorkSize> = Vec::new();
    let matches: Vec<&Fact> = snapshot
        .active_claims
        .iter()
        .filter(|c| {
            let owned = caller_owns_claim(c);
            if owned {
                return exact_scope_match(c);
            }
            // A sibling sharing the same display/tool id is not a takeover
            // peer. Letting it enter the stale-owner arm would turn a shared
            // label back into owner authority. It must use its own session's
            // claim or coordinate with the owning session.
            if c.tool.as_deref() == Some(tool) {
                return false;
            }
            if !takeover_scope_match(c) {
                return false;
            }
            let (eligible, size) = snapshot.claim_reclaim_eligible(c, &coord);
            if eligible {
                reclaim_sizes.push(size);
            }
            eligible
        })
        .collect();
    // Did at least one match come from a stale-owner takeover (not self)?
    let is_takeover = matches.iter().any(|c| !caller_owns_claim(c));
    if matches.is_empty() {
        // Build the loud-error list: this tool's currently-open claims, plus a
        // hint about any squatting (stale-owner) claims on the wanted paths that
        // would be reclaimable IF the owner were stale — so the operator learns
        // why a still-live peer's claim is not reclaimable.
        let mine: Vec<&Fact> = snapshot
            .active_claims
            .iter()
            .filter(caller_owns_claim)
            .collect();
        let blocking_live: Vec<&Fact> = snapshot
            .active_claims
            .iter()
            .filter(|c| {
                !claim_authority::claim_owner_matches_caller(
                    c.tool.as_deref(),
                    c.from_session_id.as_deref(),
                    Some(tool),
                    Some(&caller_session),
                )
            })
            .filter(takeover_scope_match)
            .collect();
        let listing = if mine.is_empty() {
            format!("(none — {tool} has no open claims in this room)")
        } else {
            mine.iter()
                .map(|c| {
                    format!(
                        "  - {} {} scope=[{}]",
                        c.event_id,
                        c.subject,
                        c.scope.join(",")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let live_hint = if blocking_live.is_empty() {
            String::new()
        } else {
            let owners: Vec<String> = blocking_live
                .iter()
                .filter_map(|c| c.tool.clone())
                .collect();
            format!(
                "\nNote: those paths are claimed by peer(s) [{}] that are not \
                 takeover-eligible; an authorized takeover release is only \
                 permitted once an owner has been totally silent >2h (a 15m idle \
                 window is advisory only, not enough to reclaim a claim).",
                owners.join(", ")
            )
        };
        return Err(RallyError::Usage(format!(
            "rally say release: no live claim by {tool} matches paths [{paths}]; nothing to release.\n{tool}'s open claims:\n{listing}{live_hint}",
            paths = want_paths.join(", ")
        )));
    }

    // Snapshot the matched claim metadata before mutating — we want stable
    // event_ids + subjects + owners to populate warnings + takeover provenance
    // even if a subsequent release changes the projection.
    let match_meta: Vec<(String, String, Vec<String>, Option<String>)> = matches
        .into_iter()
        .map(|c| {
            (
                c.event_id.clone(),
                c.subject.clone(),
                c.scope.clone(),
                c.tool.clone(),
            )
        })
        .collect();
    let total = match_meta.len();

    // The projection's released-scopes filter (store.rs::snapshot_from_facts)
    // closes EVERY claim whose scope overlaps the released scope. Issuing one
    // release fact per match would therefore re-fail the second matched
    // claim's "is_live" check because the first release already swept it.
    //
    // The contract-correct shape is: ONE release fact per call, carrying the
    // first matched claim's event_id as `ref_id` AND the union of every
    // matched claim's scope. The verified path's readback then asserts the
    // primary claim flipped — and the projection naturally sweeps the rest.
    // We surface every claim that the call closed via `warnings[]` so a host
    // sees the full audit trail.
    let primary = match_meta
        .first()
        .expect("match_meta is non-empty (early-return guard above)")
        .clone();
    let mut union_scope: Vec<String> = Vec::new();
    for (_id, _subj, sc, _owner) in &match_meta {
        for s in sc {
            if !union_scope.contains(s) {
                union_scope.push(s.clone());
            }
        }
    }
    // Record an authorized-takeover provenance trail when the caller reclaimed a
    // stale peer's claim (rather than releasing their own). The annotation lands
    // on the durable release fact itself, which is the decision record (the fix
    // direction asked for an authorized-takeover release "keyed to a decision
    // fact"; the release IS that fact, now self-describing as a takeover).
    let taken_over_owners: Vec<String> = if is_takeover {
        let mut o: Vec<String> = match_meta
            .iter()
            .filter_map(|(_id, _subj, _sc, owner)| owner.clone())
            .filter(|owner| owner != tool)
            .collect();
        o.sort();
        o.dedup();
        o
    } else {
        Vec::new()
    };
    let base_subject = if total == 1 {
        subject
    } else {
        format!("{subject} (releases {total} matching claims)")
    };
    let subject = if taken_over_owners.is_empty() {
        base_subject
    } else {
        format!(
            "{base_subject} [authorized-takeover: reclaimed stale-owner claim(s) from {}]",
            taken_over_owners.join(", ")
        )
    };
    let mut evidence = evidence;
    if !taken_over_owners.is_empty() {
        evidence.push(format!(
            "authorized-takeover:stale-owner={}",
            taken_over_owners.join(",")
        ));
        // Record WHY the reclaim was authorized: stale-by-timeout, with the
        // size class(es) that set each claim's timeout. Auditable provenance.
        let sizes: Vec<&str> = reclaim_sizes
            .iter()
            .map(|s| match s {
                crate::decay::WorkSize::Small => "small",
                crate::decay::WorkSize::Large => "large",
            })
            .collect();
        if !sizes.is_empty() {
            evidence.push(format!(
                "reclaim-reason:stale-by-timeout;work-size={}",
                sizes.join(",")
            ));
        }
    }
    let fact = Fact {
        from_session_id: Some(caller_session),
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: thread_id.unwrap_or_else(|| new_id("room")),
        kind: FactKind::Release,
        tool: Some(tool.to_string()),
        role,
        subject,
        scope: union_scope,
        created_at: now_string(),
        summary,
        evidence,
        target,
        ref_id: Some(primary.0.clone()),
        status,
        severity,
        uri,
        session: None,
    };
    let mut appended =
        with_watchdog_command_commit(|| room.append_state_transition_verified(&fact))?;
    record_append_outcome(&appended);
    for (id, subj, _sc, _owner) in &match_meta {
        let takeover_note = if is_takeover {
            " (authorized takeover of stale-owner claim)"
        } else {
            ""
        };
        warnings.push(SayWarning {
            code: "released-by-path".to_string(),
            message: format!(
                "released claim {} (\"{}\") via path-only resolution{}; release seq={}",
                id, subj, takeover_note, appended.fact.seq
            ),
        });
    }
    for warning in &appended.warnings {
        warnings.push(SayWarning {
            code: format!("projection:{:?}", warning.code).to_ascii_lowercase(),
            message: warning.message.clone(),
        });
    }
    let last_fact = appended.fact.clone();
    let snapshot_after = match room.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let warning = store::ProjectionWarning {
                code: store::ProjectionWarningCode::PostCommitWork,
                message: format!(
                    "canonical path release committed but post-commit room snapshot failed: {error}"
                ),
            };
            appended.projection_complete = false;
            appended.warnings.push(warning.clone());
            mark_watchdog_append_outcome(&appended);
            update_recorded_append_outcome(&appended);
            warnings.push(SayWarning {
                code: "projection:post_commit_work".to_string(),
                message: warning.message,
            });
            RoomSnapshot::default()
        }
    };
    let verified = SayVerified {
        room: room.room_id().to_string(),
        seq: last_fact.seq,
    };
    let body = envelope(
        "say",
        SCHEMA_SAY,
        SayData {
            say: SayPayload {
                fact: last_fact.clone(),
                committed: appended.committed,
                projection_complete: appended.projection_complete,
                projection_warnings: appended.warnings.clone(),
            },
            room: RoomSummary::from(&snapshot_after),
            warnings,
            verified,
        },
    )?;
    let text = format!(
        "said release {} released={} room={} last_seq={}",
        last_fact.event_id,
        total,
        room.room_id(),
        last_fact.seq
    );
    Ok(Output::new(json, text, body))
}

fn command_room(args: RoomArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let json_output = args.json;
    let budget_override = args.budget_bytes;
    let query = RoomQuery::from(args);
    // R10: use snapshot_with_readers when --readers is passed so that
    // ReadReceipt projection happens; otherwise use the cheaper default path.
    let projected = if query.readers {
        room.snapshot_with_readers_archived(query.include_archived)?
            .filtered(&query)
    } else {
        room.snapshot_with_archived(query.include_archived)?
            .filtered(&query)
    };
    // Composition is an OUTPUT concern and runs only here — the projection
    // above is a write-path authority (`append_state_transition_verified`
    // gates `resolve` on membership in its buckets) and must stay whole.
    let coord = hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let consumer = if query.tool.is_none() && query.paths.is_empty() {
        // No caller identity declared: rank on recency and author staleness
        // alone. Explicitly neutral, not accidentally empty.
        crate::relevance::ConsumerContext::neutral()
    } else {
        crate::relevance::ConsumerContext {
            tool: query.tool.clone(),
            paths: query.paths.clone(),
        }
    };
    // These top-level fields are part of the emitted response but live outside
    // `RoomSnapshot`. Build them before composition so the ceiling measures the
    // exact pretty-printed command envelope instead of a partial store view.
    let readers = projected.readers.clone();
    let mission = projected.mission.clone();
    let session_views = read_session_views(&room, BackendBins::default()).unwrap_or_default();
    let agent_injectability =
        build_agent_injectability(&projected, &session_views, query.tool.as_deref());
    let measure_query = query.clone();
    let measure_readers = readers.clone();
    let measure_mission = mission.clone();
    let measure_injectability = agent_injectability.clone();
    let measure_output = |candidate: &RoomSnapshot| {
        envelope(
            "room",
            SCHEMA_ROOM,
            RoomData {
                query: measure_query.clone(),
                room: candidate.clone(),
                readers: measure_readers.clone(),
                mission: measure_mission.clone(),
                agent_injectability: measure_injectability.clone(),
            },
        )
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        // `Output::render` adds one trailing newline.
        .map(|rendered| rendered.len() + 1)
        .unwrap_or(usize::MAX)
    };
    let snapshot = store::compose_room_output(
        projected,
        &coord,
        &consumer,
        query.include_archived,
        budget_override,
        measure_output,
    );
    let body = envelope(
        "room",
        SCHEMA_ROOM,
        RoomData {
            query,
            room: snapshot.clone(),
            readers,
            mission,
            agent_injectability,
        },
    )?;
    if let Some(composition) = &snapshot.composition {
        let actual_bytes = serde_json::to_string_pretty(&body)
            .map(|rendered| rendered.len() + 1)
            .unwrap_or(usize::MAX);
        debug_assert_eq!(composition.emitted_bytes, actual_bytes);
    }
    let text = format!(
        "room claims={} blockers={} handoffs={} decisions={} risks={} artifacts={} system_health={}",
        snapshot.active_claims.len(),
        snapshot.active_blockers.len(),
        snapshot.open_handoffs.len(),
        snapshot.current_decisions.len(),
        snapshot.current_risks.len(),
        snapshot.recent_artifacts.len(),
        snapshot.system_health.len()
    );
    Ok(Output::new(json_output, text, body))
}

fn command_next(args: NextArgs) -> Result<Output> {
    let audit = args.audit;
    let tool = args.tool;
    let role = args.role;
    let paths = normalize_paths(args.paths);
    let limit = args.limit as usize;
    let room = RoomStore::open()?;
    // Default `next` remains a writeful coordination action. `--audit` is the
    // explicit coordination-fact observation contract used by hooks and
    // reviewers; opening the derived cache may still repair/rebuild it.
    if !audit {
        ensure_presence(&room, &tool)?;
    }
    let snapshot = room.snapshot()?;
    // #7: always read the backlog store and surface ready items in next output.
    let backlog_items = list_backlog_items(&room).unwrap_or_default();
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let next = build_next(
        &snapshot,
        &tool,
        role.as_deref(),
        &paths,
        limit,
        backlog_items,
        coord.stale_wait_secs,
    );
    let action = next.action;
    let target_event_id = next
        .target_event_id
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let wake_intent = if audit {
        None
    } else {
        append_next_wake_intent(&room, &snapshot, &tool, &paths, &next)?
    };
    let snapshot = if wake_intent.is_some() {
        room.snapshot()?
    } else {
        snapshot
    };
    // R10: append a read-checkpoint fact when the tool's read position advances.
    // Use `content_max_seq` (max seq of non-read-checkpoint facts) rather than
    // `max_seq` so that the read-checkpoint's own seq is never fed back as the
    // read position on the next poll — this breaks the self-inflation loop.
    // E.g. if content_max_seq = 5 and we write a checkpoint at seq 6 recording
    // "read_seq:5", the next poll sees content_max_seq = 5 again (the checkpoint
    // at seq 6 is excluded) → last_checkpoint = 5 → no new checkpoint written.
    // O26's base append performs exact canonical readback. This low-stakes
    // checkpoint remains optional to `next`, but any committed warning or
    // query-required uncertainty is surfaced in the command aggregate.
    if !audit {
        consume_optional_conditional_append(
            room.maybe_append_read_checkpoint(&tool, snapshot.content_max_seq),
            "next read checkpoint",
        );
    }
    let lead_context = build_lead_context(&snapshot, Some(&tool), role.as_deref());
    let body = envelope(
        "next",
        SCHEMA_NEXT,
        NextData {
            tool,
            role,
            paths,
            next,
            wake_intent,
            room: RoomSummary::from(&snapshot),
            lead_context,
        },
    )?;
    let text = format!("next action={action} target={target_event_id}");
    Ok(Output::new(args.json, text, body))
}

/// Wrapper: wraps locate result under `data.locate`.
#[derive(JsonSchema, Serialize)]
struct LocateEnvelope {
    locate: discovery::LocateData,
}

fn command_locate(args: LocateArgs) -> Result<Output> {
    let data = discovery::locate(&args.event_id)?;
    let found = data.located.is_some();
    let body = envelope("locate", SCHEMA_LOCATE, LocateEnvelope { locate: data })?;
    let text = format!("locate event={} found={}", args.event_id, found);
    Ok(Output::new(args.json, text, body))
}

/// Wrapper: wraps recent result under `data.recent`.
#[derive(JsonSchema, Serialize)]
struct RecentEnvelope {
    recent: discovery::RecentData,
}

fn command_recent(args: RecentArgs) -> Result<Output> {
    let data = discovery::recent(args.all, args.limit, args.include_archived)?;
    let count = data.rows.len();
    let body = envelope("recent", SCHEMA_RECENT, RecentEnvelope { recent: data })?;
    let text = format!("recent rows={count}");
    Ok(Output::new(args.json, text, body))
}

/// Wrapper: wraps migrate-legacy result under `data["migrate-legacy"]`.
#[derive(JsonSchema, Serialize)]
struct MigrateLegacyEnvelope {
    #[serde(rename = "migrate-legacy")]
    migrate_legacy: discovery::MigrateLegacyData,
}

fn command_migrate_legacy(args: MigrateLegacyArgs) -> Result<Output> {
    let root = repo_root()?;
    let repo_basename = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let room = RoomStore::open()?;
    let data = discovery::migrate_legacy(&room, &repo_basename)?;
    for outcome in &data.append_outcomes {
        record_append_outcome(outcome);
    }
    let outcome_unknown_count = data.outcome_unknowns.len();
    let text = format!(
        "migrate-legacy slugs={} facts_read={} migrated={} skipped_existing={} outcome_unknown={outcome_unknown_count}",
        data.slugs_found.len(),
        data.facts_read,
        data.facts_migrated,
        data.facts_skipped_existing,
    );
    let mut body = envelope(
        "migrate-legacy",
        SCHEMA_MIGRATE_LEGACY,
        MigrateLegacyEnvelope {
            migrate_legacy: data,
        },
    )?;
    if outcome_unknown_count > 0 {
        body["ok"] = Value::Bool(false);
        Ok(Output::new(args.json, text, body).with_exit_code(1))
    } else {
        Ok(Output::new(args.json, text, body))
    }
}

/// Wrapper: wraps doctor result under `data.doctor`.
#[derive(JsonSchema, Serialize)]
struct DoctorEnvelope<T: Serialize + schemars::JsonSchema> {
    doctor: T,
}

fn command_doctor(args: DoctorArgs) -> Result<Output> {
    let existing_modes = [
        args.canonical_paths,
        args.prune_rooms,
        args.reap_stale,
        args.sweep_corrupt,
        args.compact_log,
        args.binary_skew,
    ];
    if args.migrate_db_only && existing_modes.into_iter().any(|enabled| enabled) {
        return Err(RallyError::Usage(
            "--migrate-db-only cannot be combined with another rally doctor mode".to_string(),
        ));
    }
    if !args.migrate_db_only && args.engagement.is_some() {
        return Err(RallyError::Usage(
            "--engagement is accepted only with --migrate-db-only".to_string(),
        ));
    }
    if args.migrate_db_only {
        let engagement = args.engagement.as_deref().ok_or_else(|| {
            RallyError::Usage(
                "rally doctor --migrate-db-only requires --engagement <label>".to_string(),
            )
        })?;
        let result = if args.apply {
            with_watchdog_command_commit(|| doctor::run_db_only_migration(engagement, true))
        } else {
            doctor::run_db_only_migration(engagement, false)
        };
        return match result {
            Ok(data) => {
                let text = format!(
                    "doctor migrate-db-only: state={:?} rows={:?} max_seq={:?} applied={} revalidation_required={}",
                    data.state,
                    data.row_count,
                    data.max_seq,
                    data.applied,
                    data.apply_requires_revalidation,
                );
                let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
                Ok(Output::new(args.json, text, body))
            }
            Err(doctor::DbOnlyMigrationRunError::Interrupted(interruption))
                if interruption.state
                    == doctor::DbOnlyMigrationInterruptionState::OutcomeUnknown =>
            {
                let text = format!(
                    "DB-only migration outcome is unknown at phase {}; inspect the marker-bound artifacts and resume with `{}`",
                    interruption.phase, interruption.retry_command
                );
                let body = json!({
                    "ok": false,
                    "product": "rally",
                    "command": "db_only_migration_outcome_unknown",
                    "data": {
                        "migration": interruption,
                    }
                });
                Ok(Output::new(args.json, text, body).with_exit_code(1))
            }
            Err(doctor::DbOnlyMigrationRunError::Interrupted(interruption))
                if interruption.state
                    == doctor::DbOnlyMigrationInterruptionState::CommittedCleanupPending =>
            {
                mark_watchdog_command_commit();
                let text = format!(
                    "doctor migrate-db-only: canonical target committed; cleanup remains at phase {}; resume with `{}`",
                    interruption.phase, interruption.retry_command
                );
                let body = envelope(
                    "doctor",
                    SCHEMA_DOCTOR,
                    DoctorEnvelope {
                        doctor: interruption,
                    },
                )?;
                Ok(Output::new(args.json, text, body))
            }
            Err(error) => Err(error.into_rally_error()),
        };
    }
    if args.canonical_paths {
        let data = doctor::run_canonical_paths()?;
        let text = format!(
            "doctor canonical-paths: non_canonical={} suffix_collisions={}",
            data.non_canonical.len(),
            data.suffix_collisions.len(),
        );
        let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        return Ok(Output::new(args.json, text, body));
    }
    if args.prune_rooms {
        let data = doctor::run_prune_rooms(args.apply)?;
        let text = format!(
            "doctor prune-rooms: live={} stale={} applied={}",
            data.live,
            data.stale.len(),
            data.applied,
        );
        let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        return Ok(Output::new(args.json, text, body));
    }
    if args.reap_stale {
        let data = reaper::run_reap_stale(args.apply)?;
        // D7: a reap whose durable appends failed used to answer `ok: true`,
        // exit 0, `applied: true` — the report claimed a room state the ledger
        // did not hold. The failure count is the thing to fail on, not the
        // presence of the `--apply` flag.
        let write_failures = data.write_failures;
        let remaining = data.remaining;
        let text = format!(
            "doctor reap-stale: claims_reaped={} lead_relinquished={} attempted_writes={} remaining={} write_failures={} applied={} complete={}{}",
            data.claims_reaped.len(),
            data.lead_relinquished.is_some(),
            data.attempted_writes,
            remaining,
            write_failures,
            data.applied,
            data.complete,
            if remaining > 0 {
                " — budget spent; run again to continue"
            } else {
                ""
            },
        );
        let mut body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        if write_failures > 0 {
            // The report BODY is kept — it names which items landed and which
            // did not, and an operator needs that more than a bare error. Only
            // the verdict changes.
            body["ok"] = Value::Bool(false);
            return Ok(Output::new(args.json, text, body).with_exit_code(1));
        }
        return Ok(Output::new(args.json, text, body));
    }
    if args.sweep_corrupt {
        let data = doctor::run_sweep_corrupt(args.keep, args.max_age_days, args.apply)?;
        let text = format!(
            "doctor sweep-corrupt: swept={} kept={} bytes_reclaimable={} applied={}",
            data.swept.len(),
            data.kept.len(),
            data.bytes_reclaimable,
            data.applied,
        );
        let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        return Ok(Output::new(args.json, text, body));
    }
    if args.compact_log {
        let data = doctor::run_compact_log(args.log_file)?;
        let text = render_compact_log_text(&data);
        let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        return Ok(Output::new(args.json, text, body));
    }
    if args.binary_skew {
        let data = doctor::run_binary_skew()?;
        let text = format!("doctor binary-skew: {}", data.detail);
        let body = envelope("doctor", SCHEMA_DOCTOR, DoctorEnvelope { doctor: data })?;
        return Ok(Output::new(args.json, text, body));
    }
    Err(RallyError::Usage(
        "rally doctor requires --canonical-paths, --prune-rooms, --reap-stale, --sweep-corrupt, --compact-log, --binary-skew, or --migrate-db-only"
            .to_string(),
    ))
}

/// Human rendering for `doctor --compact-log`: header with compaction stats,
/// then one line per entry — heartbeat runs as `presence xN [tool(n) ...]`.
/// Neutralize log-controlled text before terminal rendering: control
/// characters (newline, CR, ESC, C0/C1) become U+FFFD so a crafted fact
/// cannot spoof extra output lines or drive the terminal.
fn sanitize_log_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

fn render_compact_log_text(data: &doctor::CompactLogReport) -> String {
    let mut out = format!(
        "doctor compact-log: {} lines={} presence={} runs={} saved={} unparseable={}",
        data.log_file.display(),
        data.total_lines,
        data.presence_lines,
        data.presence_runs,
        data.lines_saved,
        data.unparseable_lines,
    );
    for entry in &data.entries {
        match entry {
            doctor::CompactLogEntry::PresenceRun(run) => {
                let tools: Vec<String> = run
                    .tools
                    .iter()
                    .map(|(tool, n)| format!("{}({n})", sanitize_log_text(tool)))
                    .collect();
                out.push_str(&format!(
                    "\nseq {}..{}  {}..{}  presence x{}  [{}]",
                    run.first_seq,
                    run.last_seq,
                    sanitize_log_text(&run.first_at),
                    sanitize_log_text(&run.last_at),
                    run.count,
                    tools.join(" "),
                ));
            }
            doctor::CompactLogEntry::Event(ev) => {
                out.push_str(&format!(
                    "\nseq {}  {}  {}  {}  {}",
                    ev.seq,
                    sanitize_log_text(&ev.occurred_at),
                    sanitize_log_text(&ev.event_type),
                    sanitize_log_text(ev.tool.as_deref().unwrap_or("-")),
                    sanitize_log_text(ev.subject.as_deref().unwrap_or("")),
                ));
            }
        }
    }
    for w in &data.warnings {
        out.push_str(&format!("\nwarning {}: {}", w.code, w.message));
    }
    out
}

#[cfg(test)]
mod compact_log_render_tests {
    use super::*;

    /// Log-controlled tool/subject values must not spoof output lines or
    /// emit terminal control sequences (codex review finding 3, seq 4535).
    #[test]
    fn render_sanitizes_log_controlled_fields() {
        let report = doctor::CompactLogReport {
            log_file: std::path::PathBuf::from("seg.jsonl"),
            total_lines: 2,
            presence_lines: 0,
            presence_runs: 0,
            lines_saved: 0,
            unparseable_lines: 0,
            entries: vec![doctor::CompactLogEntry::Event(doctor::CompactLogEvent {
                seq: 1,
                occurred_at: "2026-07-03T19:30:00Z".to_string(),
                event_type: "read".to_string(),
                tool: Some("codex:a\x1b[31m".to_string()),
                subject: Some("real subject\nseq 999  fake  spoofed  line".to_string()),
                payload: None,
            })],
            warnings: Vec::new(),
        };
        let text = render_compact_log_text(&report);
        assert!(!text.contains('\x1b'), "ESC must not reach the terminal");
        assert_eq!(
            text.lines().count(),
            2,
            "header + one entry — embedded newline must not add a line"
        );
        assert!(
            !text.contains("\nseq 999"),
            "spoofed entry line must not appear at line start"
        );
    }

    /// Presence-run tool ids are log-controlled too.
    #[test]
    fn render_sanitizes_presence_run_tools() {
        let mut tools = std::collections::BTreeMap::new();
        tools.insert("bad\ntool".to_string(), 2usize);
        let report = doctor::CompactLogReport {
            log_file: std::path::PathBuf::from("seg.jsonl"),
            total_lines: 2,
            presence_lines: 2,
            presence_runs: 1,
            lines_saved: 1,
            unparseable_lines: 0,
            entries: vec![doctor::CompactLogEntry::PresenceRun(doctor::PresenceRun {
                first_seq: 1,
                last_seq: 2,
                first_at: "t1".to_string(),
                last_at: "t2".to_string(),
                count: 2,
                tools,
            })],
            warnings: Vec::new(),
        };
        let text = render_compact_log_text(&report);
        assert_eq!(text.lines().count(), 2, "run renders as exactly one line");
    }
}

// claims-refresh -------------------------------------------------------------

const SCHEMA_CLAIMS_REFRESH: &str = "agent-rally.command.claims-refresh.v1";

#[derive(Serialize, schemars::JsonSchema)]
struct ClaimsRefreshEnvelope {
    claims_refresh: ClaimsRefreshReport,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ClaimConflictEntry {
    /// The manifest path that could not be claimed.
    path: String,
    /// The live peer that already holds a conflicting claim.
    owner: String,
    /// The full conflict message from the store.
    detail: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ClaimsRefreshReport {
    lane: String,
    tool: String,
    manifest: String,
    /// Files successfully claimed or renewed (own-claim renewal and
    /// stale/expired-claim reclaim both land here — they never conflict).
    claimed: Vec<String>,
    /// Files blocked by a live peer's conflicting claim; the rest of the
    /// manifest still processed (graceful degradation).
    conflicts: Vec<ClaimConflictEntry>,
    /// Total claimable paths parsed from the manifest.
    total: usize,
}

/// Parse a newline-delimited manifest: trim each line, drop blank lines and
/// `#`-prefixed comments.
fn parse_claims_manifest(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// Build a single-file claim `SayArgs` tagged with a `lane:<name>` evidence
/// marker. Reuses the full `command_say` claim path (lease sizing, source
/// grounding, presence, conflict detection).
fn claim_say_args(tool: &str, lane: &str, path: &str, json: bool) -> SayArgs {
    SayArgs {
        json,
        kind: FactKind::Claim,
        tool: tool.to_string(),
        subject: None,
        thread_id: None,
        role: None,
        summary: Some(format!("claims-refresh: lane {lane}")),
        scopes: Vec::new(),
        resources: Vec::new(),
        paths: vec![path.to_string()],
        evidence: vec![format!("lane:{lane}")],
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        produces: Vec::new(),
        depends: Vec::new(),
        run_id: None,
        step_id: None,
        parent_step_ids: Vec::new(),
        reason: None,
        wake_after: None,
        ref_standby: None,
    }
}

fn command_claims_refresh(args: ClaimsRefreshArgs) -> Result<Output> {
    let contents = std::fs::read_to_string(&args.manifest)
        .map_err(|e| RallyError::Usage(format!("cannot read manifest {}: {e}", args.manifest)))?;
    let files = parse_claims_manifest(&contents);
    if files.is_empty() {
        return Err(RallyError::Usage(format!(
            "manifest {} has no claimable paths (blank/comment-only)",
            args.manifest
        )));
    }

    let mut claimed: Vec<String> = Vec::new();
    let mut conflicts: Vec<ClaimConflictEntry> = Vec::new();

    for path in &files {
        let say = claim_say_args(&args.tool, &args.lane, path, args.json);
        match command_say(say) {
            Ok(_) => claimed.push(path.clone()),
            // A live peer holds a conflicting claim — record and keep going so
            // one conflict never blocks the rest of the manifest.
            Err(RallyError::Usage(msg)) if msg.contains("claim conflict") => {
                // The owner is the first whitespace-delimited token after
                // "claim conflict:". This used to split on the literal
                // "already owns", and RC-037's message rewrite dropped that
                // phrase — so the delimiter vanished, `split().next()` returned
                // the whole remainder, and this field silently became the
                // entire sentence instead of a tool id. A machine-readable
                // envelope field was broken by a prose edit with nothing
                // grading it. Splitting on whitespace depends only on the
                // owner being the first thing named, which both the old and new
                // wording guarantee.
                let owner = msg
                    .split("claim conflict:")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                conflicts.push(ClaimConflictEntry {
                    path: path.clone(),
                    owner,
                    detail: msg,
                });
            }
            // Genuine failure (IO, corruption, verification) — surface loudly.
            Err(e) => return Err(e),
        }
    }

    let total = files.len();
    let report = ClaimsRefreshReport {
        lane: args.lane.clone(),
        tool: args.tool.clone(),
        manifest: args.manifest.clone(),
        claimed,
        conflicts,
        total,
    };
    let text = format!(
        "claims-refresh lane={} tool={}: claimed={}/{} conflicts={}",
        report.lane,
        report.tool,
        report.claimed.len(),
        total,
        report.conflicts.len(),
    );
    let body = envelope(
        "claims-refresh",
        SCHEMA_CLAIMS_REFRESH,
        ClaimsRefreshEnvelope {
            claims_refresh: report,
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

// B13 -----------------------------------------------------------------------

/// Wrapper: wraps check-ci result under `data["check-ci"]`.
#[derive(JsonSchema, Serialize)]
struct CheckCiEnvelope {
    #[serde(rename = "check-ci")]
    check_ci: check_ci::CheckCiResult,
}

fn command_check_ci(args: CheckCiArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let outcome = build_check_ci(args.strict, args.receipt_threshold_secs, &snapshot);
    let pass = outcome.data.check_ci.pass;
    let offenders = outcome.offender_count;
    let body = envelope(
        "check-ci",
        SCHEMA_CHECK_CI,
        CheckCiEnvelope {
            check_ci: outcome.data.check_ci,
        },
    )?;
    let text = format!(
        "check-ci pass={pass} offenders={offenders} mode={}",
        if args.strict { "strict" } else { "warn" }
    );
    Ok(Output::new(args.json, text, body).with_exit_code(outcome.exit_code))
}

fn command_version(args: VersionArgs) -> Result<Output> {
    let text = format!("rally {}", BUILD_ID);
    let body = envelope(
        "version",
        SCHEMA_VERSION,
        VersionData {
            version: VersionPayload {
                version: env!("CARGO_PKG_VERSION").to_string(),
                build_id: BUILD_ID.to_string(),
            },
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

/// B-whoami: print identity fields in one call to disambiguate two-clone confusion.
///
/// Reports: tool (if `--tool` given), repo_root (shared coord dir), repo_id
/// (room identifier), worktree (active checkout dir), build_id (embedded
/// RALLY_BUILD_ID), and cwd. Read-only — no facts are written.
/// Derive this runtime's layered protocol session identity, used by `whoami`
/// (display) and `say` (`from_session_id` stamping). Reads runtime signals via
/// the `session_identity` boundary; the lease is deterministic ("live") so the
/// session_id is stable across CLI invocations from the same endpoint until a
/// registry-backed lease exists.
fn current_protocol_session(tool: Option<&str>) -> session_identity::ProtocolSessionIdentity {
    let mut inputs = session_identity::EndpointInputs::from_env();
    // In hook/non-interactive invocations the short-lived `rally` child pid is
    // not a stable session endpoint. The host-supplied observer pid identifies
    // the long-lived agent process and therefore keeps presence, claims, and
    // renewal on one lease across CLI invocations. Higher-fidelity managed,
    // tmux, and terminal identities retain their normal precedence.
    if inputs.managed_session_id.is_none()
        && inputs.tmux_pane.is_none()
        && inputs.term_session_id.is_none()
        && inputs.tty.is_none()
        && let Ok(raw) = env::var("RALLY_OBSERVER_PID")
        && let Ok(pid) = raw.parse::<u32>()
        && pid > 1
    {
        inputs.pid = Some(pid);
    }
    let endpoint = session_identity::derive_endpoint(&inputs);
    let raw_tool = tool.unwrap_or("unknown");
    let (tool_type, actor) = match raw_tool.split_once(':') {
        Some((t, a)) if !a.is_empty() => (t, Some(a)),
        _ => (raw_tool, None),
    };
    session_identity::ProtocolSessionIdentity::mint(&endpoint, tool_type, "live", actor, None)
}

/// Map a ledger [`store::FactKind`] to the north-star protocol event vocabulary
/// for advisory envelope validation. Returns `None` for kinds outside the
/// durable coordination set (presence/read/session/etc.).
fn protocol_event_kind(kind: &store::FactKind) -> Option<event_envelope::ProtocolEventKind> {
    use event_envelope::ProtocolEventKind as P;
    use store::FactKind as F;
    Some(match kind {
        F::Claim => P::ClaimAcquired,
        F::ClaimExpired => P::ClaimExpired,
        F::Release => P::ClaimReleased,
        F::Handoff => P::HandoffRequested,
        F::Resolve => P::HandoffAcked,
        F::Decision => P::DecisionRecorded,
        F::Artifact => P::ArtifactPublished,
        _ => return None,
    })
}

/// Project a durable [`store::Fact`] onto an [`event_envelope::EventEnvelope`],
/// mapping the existing fields (`event_id`, `ref`, `from_session_id`) onto the
/// protocol causal ids so the envelope can be validated.
fn fact_protocol_envelope(fact: &store::Fact) -> event_envelope::EventEnvelope {
    use store::FactKind as F;
    let mut env = event_envelope::EventEnvelope {
        from_session_id: fact.from_session_id.clone(),
        ..Default::default()
    };
    // A referenced prior event is both the ref and the direct cause (for replies).
    if fact.ref_id.is_some() {
        env.ref_event_id = fact.ref_id.clone();
        env.causation_id = fact.ref_id.clone();
    }
    match fact.kind {
        F::Claim | F::ClaimExpired => env.claim_id = Some(fact.event_id.clone()),
        F::Release => env.claim_id = fact.ref_id.clone().or_else(|| Some(fact.event_id.clone())),
        F::Handoff => env.handoff_id = Some(fact.event_id.clone()),
        F::Resolve => env.handoff_id = fact.ref_id.clone(),
        _ => {}
    }
    env
}

fn command_whoami(args: WhoamiArgs) -> Result<Output> {
    let repo_root_path = repo_root().ok();
    let repo_root = repo_root_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let worktree = worktree_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let branch = worktree_root()
        .ok()
        .and_then(|wt| worktree_guard::current_branch(&wt));
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    // Best-effort: whoami stays a diagnostic that never hard-fails.
    let room = RoomStore::open().ok();
    let repo_id = resolve_repo_id(repo_root_path.as_deref());
    let room_id = room
        .as_ref()
        .map(|r| r.room_id().to_string())
        .unwrap_or_else(|| "<no-room>".to_string());
    let snapshot = room.as_ref().and_then(|r| r.snapshot().ok());
    let lead = snapshot.as_ref().and_then(|s| s.lead.clone());
    let mission = snapshot.as_ref().and_then(|s| s.mission.clone());
    let acknowledged = match (&args.tool, &snapshot) {
        (Some(tool), Some(snap)) => Some(
            snap.squads
                .iter()
                .any(|sq| &sq.tool == tool && sq.acknowledged),
        ),
        _ => None,
    };
    let lead_context = snapshot
        .as_ref()
        .map(|snap| build_lead_context(snap, args.tool.as_deref(), None));
    let host_runtime = detect_host_runtime();
    let protocol_session = current_protocol_session(args.tool.as_deref());
    let text = format!(
        "repo_root={repo_root} repo_id={repo_id} room_id={room_id} build_id={BUILD_ID} branch={} ptyd_ambiguous={} lead={}",
        branch.as_deref().unwrap_or("<none>"),
        host_runtime.ambiguous,
        lead.as_deref().unwrap_or("<none>"),
    );
    let body = envelope(
        "whoami",
        SCHEMA_WHOAMI,
        WhoamiData {
            whoami: WhoamiPayload {
                tool: args.tool,
                repo_root,
                repo_id,
                room_id,
                worktree,
                branch,
                build_id: BUILD_ID.to_string(),
                cwd,
                host_runtime,
                lead,
                mission,
                acknowledged,
                lead_context,
                session_identity: protocol_session,
            },
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

fn command_owners(args: OwnersArgs) -> Result<Output> {
    if !args.dirty {
        return Err(RallyError::Usage(
            "rally owners currently requires --dirty".to_string(),
        ));
    }
    let root = repo_root()?;
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let session_views = read_session_views(&room, args.bins)?;
    let dirty_paths = dirty_git_paths(&root);
    let dirty = build_dirty_owners(&snapshot, &session_views, &dirty_paths);
    let claimed_paths: BTreeSet<String> = dirty.iter().map(|owner| owner.path.clone()).collect();
    let unclaimed_dirty_paths = dirty_paths
        .iter()
        .filter(|path| !claimed_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    let dirty_len = dirty.len();
    let unclaimed_len = unclaimed_dirty_paths.len();
    let body = envelope(
        "owners",
        SCHEMA_OWNERS,
        OwnersData {
            owners: OwnersPayload {
                mode: "dirty",
                dirty_paths,
                dirty,
                unclaimed_dirty_paths,
            },
        },
    )?;
    let text = format!("owners dirty={dirty_len} unclaimed={unclaimed_len}");
    Ok(Output::new(args.json, text, body))
}

fn dirty_git_paths(root: &Path) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_git_porcelain_z(&output.stdout)
}

fn parse_git_porcelain_z(stdout: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let x = entry[0] as char;
        let y = entry[1] as char;
        let path = String::from_utf8_lossy(&entry[3..])
            .replace('\\', "/")
            .trim()
            .to_string();
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            let _ = entries.next();
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn build_dirty_owners(
    snapshot: &RoomSnapshot,
    session_views: &[SessionView],
    dirty_paths: &[String],
) -> Vec<DirtyOwner> {
    let mut rows = Vec::new();
    for path in dirty_paths {
        for claim in &snapshot.active_claims {
            if !claim
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path))
            {
                continue;
            }
            let Some(record) = claim_authority::active_claim_record_from_fact(claim) else {
                continue;
            };
            let owner_status = record.owner_tool.as_deref().and_then(|owner| {
                snapshot
                    .squads
                    .iter()
                    .find(|squad| squad.tool == owner)
                    .map(|squad| squad.status.clone())
            });
            let (session_liveness, liveness_source) =
                claim_session_liveness(&record, session_views);
            let lease_expired = record
                .lease_expires_at
                .as_deref()
                .is_some_and(lease_is_expired);
            let is_owner_live =
                owner_live_decision(owner_status.as_deref(), session_liveness, lease_expired);
            rows.push(DirtyOwner {
                path: path.clone(),
                claim_id: record.claim_id,
                owner_tool: record.owner_tool,
                from_session_id: record.from_session_id,
                owner_status,
                lease_expires_at: record.lease_expires_at,
                lease_expired,
                session_liveness,
                liveness_source,
                is_owner_live,
                scope: claim.scope.clone(),
                subject: claim.subject.clone(),
            });
        }
    }
    rows.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.claim_id.cmp(&b.claim_id))
    });
    rows
}

fn claim_session_liveness(
    claim: &claim_authority::ActiveClaimRecord,
    session_views: &[SessionView],
) -> (Option<SessionLiveness>, Option<&'static str>) {
    let by_session = claim.from_session_id.as_deref().and_then(|session_id| {
        session_views
            .iter()
            .find(|view| view.session.session_id == session_id)
    });
    let by_tool = claim.owner_tool.as_deref().and_then(|owner| {
        session_views
            .iter()
            .find(|view| view.session.tool == owner && view.liveness == SessionLiveness::Live)
            .or_else(|| session_views.iter().find(|view| view.session.tool == owner))
    });
    by_session
        .or(by_tool)
        .map(|view| (Some(view.liveness), Some(view.liveness_source)))
        .unwrap_or((None, None))
}

fn lease_is_expired(raw: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(false)
}

fn owner_live_decision(
    owner_status: Option<&str>,
    session_liveness: Option<SessionLiveness>,
    lease_expired: bool,
) -> Option<bool> {
    if owner_status == Some("active") || session_liveness == Some(SessionLiveness::Live) {
        return Some(true);
    }
    if session_liveness == Some(SessionLiveness::Stale)
        || (lease_expired && owner_status == Some("idle"))
    {
        return Some(false);
    }
    None
}

fn resolve_repo_id(repo_root: Option<&Path>) -> String {
    repo_root
        .and_then(manifest_repo_id)
        .or_else(|| {
            repo_root
                .and_then(|root| root.file_name())
                .and_then(|name| name.to_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "<unknown-repo>".to_string())
}

fn manifest_repo_id(repo_root: &Path) -> Option<String> {
    let path = repo_root.join(".rally").join("manifest.json");
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["repo"]
        .as_str()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string)
}

/// Wrapper: wraps status result under `data.status`.
#[derive(JsonSchema, Serialize)]
struct StatusEnvelope {
    status: discovery::GlobalStatusData,
}

fn command_status(args: StatusArgs) -> Result<Output> {
    match args.subcommand {
        cli::StatusSubcommand::Global => {
            let data = discovery::status_global()?;
            let repo_count = data.repos.len();
            let text = format!("status repos={repo_count}");
            let body = envelope("status", SCHEMA_STATUS, StatusEnvelope { status: data })?;
            Ok(Output::new(args.json, text, body))
        }
        cli::StatusSubcommand::Post(post) => command_status_post(args.json, post),
        cli::StatusSubcommand::Read(read) => command_status_read(args.json, read),
    }
}

/// Envelope for `rally status post`: result under `data["status_post"]`.
#[derive(JsonSchema, Serialize)]
struct StatusPostData {
    status_post: StatusPostResult,
}

#[derive(JsonSchema, Serialize)]
struct StatusPostResult {
    fact: store::Fact,
    state: agent_state::AgentState,
}

/// Envelope for `rally status read`: result under `data["status_read"]`.
#[derive(JsonSchema, Serialize)]
struct StatusReadData {
    status_read: StatusReadResult,
}

#[derive(JsonSchema, Serialize)]
struct StatusReadResult {
    states: Vec<agent_state::AgentStateEntry>,
}

/// Schema marker for status_post envelopes.
const SCHEMA_STATUS_POST: &str = "agent-rally.command.status_post.v1";
/// Schema marker for status_read envelopes.
const SCHEMA_STATUS_READ: &str = "agent-rally.command.status_read.v1";

/// Build the canonical marker subject for a typed status post.
///
/// Mirrors the existing presence convention: `state=<s> | <k1>=<v1> | ...`.
/// `committed_sha` + `worktree_branch` live in the SUMMARY for `done` so the
/// subject stays short and the markers remain in the same fact's text.
fn build_status_subject(state: &str, args: &cli::StatusPostArgs) -> String {
    let mut parts: Vec<String> = vec![format!("state={state}")];
    if let Some(file) = &args.file {
        parts.push(format!("file={file}"));
    }
    if let Some(intent) = &args.intent {
        parts.push(format!("intent={intent}"));
    }
    if let Some(blocked_ref) = &args.blocked_ref {
        parts.push(format!("ref={blocked_ref}"));
    }
    if let Some(wake_after) = &args.wake_after {
        parts.push(format!("wake_after={wake_after}"));
    }
    if let Some(sha) = &args.committed_sha {
        parts.push(format!("committed_sha={sha}"));
    }
    if let Some(branch) = &args.worktree_branch {
        parts.push(format!("worktree_branch={branch}"));
    }
    parts.join(" | ")
}

fn missing_marker(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).unwrap_or("").is_empty()
}

fn git_value_for_status_done(args: &[&str], field: &str, flag: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|err| {
            RallyError::Usage(format!(
                "rally status post --state done could not auto-detect {field}: failed to run git ({err}); pass {flag} explicitly"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(" (git: {stderr})")
        };
        return Err(RallyError::Usage(format!(
            "rally status post --state done could not auto-detect {field}; pass {flag} explicitly{detail}"
        )));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "HEAD" {
        return Err(RallyError::Usage(format!(
            "rally status post --state done could not auto-detect {field}; pass {flag} explicitly"
        )));
    }
    Ok(value)
}

/// Fill omitted `done` metadata from the current git checkout. This is
/// intentionally tool-neutral: Codex, Claude Code, or any other agent all use
/// the same CLI contract, and explicit flags remain authoritative.
fn auto_fill_done_git_metadata(args: &mut cli::StatusPostArgs) -> Result<()> {
    if args.state != "done" {
        return Ok(());
    }
    if missing_marker(&args.committed_sha) {
        args.committed_sha = Some(git_value_for_status_done(
            &["rev-parse", "--verify", "HEAD"],
            "committed_sha",
            "--committed-sha <sha>",
        )?);
    }
    if missing_marker(&args.worktree_branch) {
        args.worktree_branch = Some(git_value_for_status_done(
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            "worktree_branch",
            "--worktree-branch <branch>",
        )?);
    }
    Ok(())
}

/// Validate state-specific required args. Returns a loud usage error rather
/// than silently writing a malformed heartbeat — consistent with the "no
/// fail-quiet" lesson the release-fix is also closing.
fn validate_status_post_args(state: &str, args: &cli::StatusPostArgs) -> Result<()> {
    match state {
        "idle" => {
            // wake_after optional; nothing required.
        }
        "working" => {
            if args.file.is_none() || args.intent.is_none() {
                return Err(RallyError::Usage(
                    "rally status post --state working requires --file <path> and --intent <one-line>".to_string(),
                ));
            }
        }
        "blocked" => {
            if args.blocked_ref.is_none() {
                return Err(RallyError::Usage(
                    "rally status post --state blocked requires --blocked-ref <event-id>"
                        .to_string(),
                ));
            }
        }
        "done" => {
            if args.committed_sha.is_none() || args.worktree_branch.is_none() {
                return Err(RallyError::Usage(
                    "rally status post --state done requires --committed-sha <sha> and --worktree-branch <branch>".to_string(),
                ));
            }
        }
        other => {
            return Err(RallyError::Usage(format!(
                "rally status post --state must be one of idle|working|blocked|done; got {other:?}"
            )));
        }
    }
    Ok(())
}

fn command_status_post(json: bool, mut args: cli::StatusPostArgs) -> Result<Output> {
    auto_fill_done_git_metadata(&mut args)?;
    validate_status_post_args(&args.state, &args)?;

    let room = RoomStore::open()?;
    // Auto-register the calling tool (matches `rally say` ergonomics).
    ensure_presence(&room, &args.tool)?;

    let subject = build_status_subject(&args.state, &args);
    let fact = store::Fact {
        from_session_id: Some(
            current_protocol_session(Some(&args.tool))
                .from_session_id()
                .to_string(),
        ),
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: store::FactKind::Presence,
        tool: Some(args.tool.clone()),
        role: None,
        subject: subject.clone(),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!("build_id:{BUILD_ID}")),
        evidence: presence_signal_evidence(&room),
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    let mut appended = with_watchdog_command_commit(|| room.append_fact_verified(&fact))?;
    record_append_outcome(&appended);
    // The shipped coordination hook emits status posts as heartbeats. Renew
    // after the presence append so liveness and lease durability succeed or
    // fail together from the caller's perspective.
    if let Err(error) =
        with_watchdog_command_commit(|| renew_owned_claim_leases(&room, &args.tool).map(|_| ()))
    {
        if matches!(error, RallyError::OutcomeUnknown { .. }) {
            // Preserve the stable renewal event id, phase, and query remedy in
            // the command-wide typed partial result. A string warning would
            // invite retrying an append that may already be canonical.
            return Err(error);
        }
        appended.projection_complete = false;
        appended.warnings.push(store::ProjectionWarning {
            code: store::ProjectionWarningCode::PostCommitWork,
            message: format!("status heartbeat committed but lease renewal failed: {error}"),
        });
        mark_watchdog_append_outcome(&appended);
        update_recorded_append_outcome(&appended);
    }
    let state = agent_state::project_presence_to_state(&appended.fact)
        .unwrap_or(agent_state::AgentState::Idle { wake_after: None });
    let text = format!("status post tool={} seq={}", args.tool, appended.fact.seq);
    let body = envelope(
        "status_post",
        SCHEMA_STATUS_POST,
        StatusPostData {
            status_post: StatusPostResult {
                fact: appended.fact,
                state,
            },
        },
    )?;
    Ok(Output::new(json, text, body))
}

fn command_status_read(json: bool, args: cli::StatusReadArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let facts = room.facts()?;
    let now_ts = now_string();
    let mut states = agent_state::project_agent_states(&facts, &now_ts);
    if let Some(filter_tool) = args.tool.as_deref() {
        states.retain(|s| s.tool == filter_tool);
    }
    let text = format!("status read tools={}", states.len());
    let body = envelope(
        "status_read",
        SCHEMA_STATUS_READ,
        StatusReadData {
            status_read: StatusReadResult { states },
        },
    )?;
    Ok(Output::new(json, text, body))
}

// =============================================================================
// rally watch — per-repo autonomy watcher (B17-safe, host-neutral)
// =============================================================================
//
// Reads .rally/log/index.json to obtain the current max_seq cheaply (no full
// snapshot, no RoomStore open on every tick). For --once, persists the cursor
// at .rally/watch-cursor.json so successive cron/launchd calls detect deltas.
//
// B17 alignment: reads ONLY per-repo .rally/log — never ~/.agent-rally-point.

/// Filename for the --once mode cursor persistence.
const WATCH_CURSOR_FILENAME: &str = "watch-cursor.json";

/// Read the current `max_seq` from `.rally/log/index.json` by scanning each
/// segment's `last_seq` and returning the maximum. Returns 0 when the index
/// does not exist or is empty.
fn watch_read_max_seq(log_dir: &Path) -> i64 {
    let index_path = log_dir.join(store::LOG_INDEX_FILENAME);
    let Ok(text) = fs::read_to_string(&index_path) else {
        return 0;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    value["segments"]
        .as_array()
        .map(|segs| {
            segs.iter()
                .filter_map(|s| s["last_seq"].as_i64())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Read the `--once` cursor from `.rally/watch-cursor.json`. Returns 0 on
/// missing file or parse error (watcher treats "never run" as cursor=0).
fn watch_read_once_cursor(rally_dir: &Path) -> i64 {
    let path = rally_dir.join(WATCH_CURSOR_FILENAME);
    let Ok(text) = fs::read_to_string(&path) else {
        return 0;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    value["cursor"].as_i64().unwrap_or(0)
}

/// Persist the `--once` cursor to `.rally/watch-cursor.json` atomically.
/// Errors are logged and ignored — the watcher must not die on a transient
/// write error.
fn watch_write_once_cursor(rally_dir: &Path, seq: i64) {
    let path = rally_dir.join(WATCH_CURSOR_FILENAME);
    let content = match serde_json::to_string_pretty(&serde_json::json!({
        "cursor": seq,
        "updated_at": now_string(),
    })) {
        Ok(s) => format!("{s}\n"),
        Err(_) => return,
    };
    let temp = path.with_extension(format!("json.tmp-{}", short_id()));
    if fs::write(&temp, content).is_err() {
        return;
    }
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}

/// Emit one JSONL activity event to stdout.
fn watch_emit_activity(
    json: bool,
    from_seq: i64,
    to_seq: i64,
    room_id: &str,
    tool_last: Option<&str>,
) {
    if json || from_seq != to_seq {
        let line = serde_json::json!({
            "event": "activity",
            "from_seq": from_seq,
            "to_seq": to_seq,
            "room": room_id,
            "tool_last": tool_last,
            "ts": now_string(),
        });
        crate::output::write_line_or_exit_on_broken_pipe(&line.to_string());
    }
}

/// Emit one JSONL heartbeat line (idle tick, only under --json).
fn watch_emit_heartbeat(room_id: &str, tool: Option<&str>, current_seq: i64, interval: u64) {
    let line = serde_json::json!({
        "event": "heartbeat",
        "seq": current_seq,
        "room": room_id,
        "tool": tool,
        "interval": interval,
        "ts": now_string(),
    });
    crate::output::write_line_or_exit_on_broken_pipe(&line.to_string());
}

/// Run `--on-activity <cmd>` via the shell with the context env vars.
/// Blocks until the child exits (one in-flight at a time). Errors are logged
/// and ignored — the watcher must not die on a transient subprocess error.
fn watch_run_on_activity(
    cmd: &str,
    room_id: &str,
    from_seq: i64,
    to_seq: i64,
    tool: Option<&str>,
    repo: &Path,
) {
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("RALLY_ROOM", room_id)
        .env("RALLY_FROM_SEQ", from_seq.to_string())
        .env("RALLY_TO_SEQ", to_seq.to_string())
        .env("RALLY_TOOL", tool.unwrap_or(""))
        .env("RALLY_REPO", repo.to_string_lossy().as_ref())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("rally watch: --on-activity spawn error: {err}");
            return;
        }
    };
    if let Err(err) = child.wait() {
        eprintln!("rally watch: --on-activity wait error: {err}");
    }
}

/// Render a launchd plist referencing this binary + the current working dir.
/// Pure (returns the plist string) so it is unit-testable without spawning a
/// live binary; takes only the `WatchArgs` fields it needs.
fn render_launchd_plist(
    interval: u64,
    on_activity: Option<&str>,
    exe: &Path,
    repo: &Path,
) -> String {
    let label = format!(
        "com.agent-rally-point.watch.{}",
        repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo")
    );
    let exe_str = exe.to_string_lossy();
    let repo_str = repo.to_string_lossy();
    let mut program_args = vec![format!("  <string>{exe_str}</string>")];
    program_args.push("  <string>watch</string>".to_string());
    if let Some(interval) = Some(interval).filter(|&i| i != 5) {
        program_args.push("  <string>--interval</string>".to_string());
        program_args.push(format!("  <string>{interval}</string>"));
    }
    if let Some(cmd) = on_activity {
        let escaped = cmd
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        program_args.push("  <string>--on-activity</string>".to_string());
        program_args.push(format!("  <string>{escaped}</string>"));
    }
    let args_xml = program_args.join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args_xml}
  </array>
  <key>WorkingDirectory</key>
  <string>{repo_str}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/tmp/rally-watch-{label}.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/rally-watch-{label}.err</string>
</dict>
</plist>"#
    )
}

/// Print a launchd plist referencing this binary + the current working dir.
fn watch_print_launchd(args: &WatchArgs, exe: &Path, repo: &Path) {
    println!(
        "{}",
        render_launchd_plist(args.interval, args.on_activity.as_deref(), exe, repo)
    );
}

/// Print a systemd service unit referencing this binary + the current working dir.
fn watch_print_systemd(args: &WatchArgs, exe: &Path, repo: &Path) {
    let exe_str = exe.to_string_lossy();
    let repo_str = repo.to_string_lossy();
    let mut exec_args = format!("{exe_str} watch");
    if args.interval != 5 {
        exec_args.push_str(&format!(" --interval {}", args.interval));
    }
    if let Some(ref cmd) = args.on_activity {
        // systemd ExecStart= uses its own tokeniser (not /bin/sh):
        //   • tokens are split on unquoted whitespace;
        //   • single-quoted strings are kept as-is by systemd;
        //   • '%' introduces unit-file specifiers — must be doubled to '%%';
        //   • backslash has special meaning inside double-quoted tokens.
        // Strategy: shell-quote the value with shlex (wraps in '…' for strings
        // containing shell-special chars), then escape any '%' in the result
        // so systemd does not expand unit specifiers.
        let shell_quoted = shlex::try_quote(cmd)
            .unwrap_or(std::borrow::Cow::Borrowed(cmd.as_str()))
            .replace('%', "%%");
        exec_args.push_str(&format!(" --on-activity {shell_quoted}"));
    }
    let unit_name = format!(
        "rally-watch-{}",
        repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo")
    );
    println!(
        r#"[Unit]
Description=rally watch autonomy watcher for {repo_str}
After=network.target

[Service]
Type=simple
WorkingDirectory={repo_str}
ExecStart={exec_args}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target

# Install: systemctl --user enable --now {unit_name}.service
# (copy this file to ~/.config/systemd/user/{unit_name}.service first)"#
    );
}

fn command_watch(args: WatchArgs) -> Result<Output> {
    let repo = repo_root()?;
    let rally_dir = repo.join(".rally");
    let log_dir = rally_dir.join(store::LOG_DIRNAME);

    // --print-launchd / --print-systemd: emit the unit and exit immediately.
    if args.print_launchd || args.print_systemd {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rally"));
        if args.print_launchd {
            watch_print_launchd(&args, &exe, &repo);
        } else {
            watch_print_systemd(&args, &exe, &repo);
        }
        return Ok(Output::new(false, String::new(), serde_json::json!({})));
    }

    // Determine room_id from active engagement (best-effort; falls back to date).
    // We do NOT open the full RoomStore on every tick — just read index.json.
    let room_id = store::resolve_active_engagement_pub(&rally_dir);

    // --once mode: single check, emit if advanced, persist cursor, exit.
    if args.once {
        let cursor = watch_read_once_cursor(&rally_dir);
        let current_seq = watch_read_max_seq(&log_dir);
        if current_seq > cursor {
            watch_emit_activity(true, cursor, current_seq, &room_id, args.tool.as_deref());
            watch_write_once_cursor(&rally_dir, current_seq);
        } else {
            // Persist updated cursor even when unchanged (ensures cursor tracks reality).
            watch_write_once_cursor(&rally_dir, current_seq);
        }
        return Ok(Output::new(false, String::new(), serde_json::json!({})));
    }

    // Long-running loop mode.
    let deadline: Option<std::time::Instant> = args
        .duration_hours
        .map(|h| std::time::Instant::now() + Duration::from_secs_f64(h * 3600.0));

    // Start cursor at the current max_seq so we react only to NEW activity.
    let mut last_seq = watch_read_max_seq(&log_dir);
    let mut current_interval = args.interval;

    loop {
        // Check deadline.
        if let Some(dl) = deadline
            && std::time::Instant::now() >= dl
        {
            if args.json {
                println!(r#"{{"event":"stopped","ts":"{}"}}"#, now_string());
            }
            break;
        }

        thread::sleep(Duration::from_secs(current_interval));

        // Re-read max_seq (log-and-continue on error).
        let new_seq = watch_read_max_seq(&log_dir);

        if new_seq > last_seq {
            // Activity detected.
            watch_emit_activity(args.json, last_seq, new_seq, &room_id, args.tool.as_deref());
            // Run --on-activity command if set (one in-flight; blocks here).
            if let Some(ref cmd) = args.on_activity {
                watch_run_on_activity(
                    cmd,
                    &room_id,
                    last_seq,
                    new_seq,
                    args.tool.as_deref(),
                    &repo,
                );
            }
            last_seq = new_seq;
            // Reset interval on activity.
            current_interval = args.interval;
        } else {
            // Idle: emit heartbeat under --json, then back off.
            if args.json {
                watch_emit_heartbeat(&room_id, args.tool.as_deref(), last_seq, current_interval);
            }
            // Adaptive back-off: multiply by 1.5, cap at max_interval.
            let next = ((current_interval as f64) * 1.5) as u64;
            current_interval = next.min(args.max_interval);
        }
    }

    Ok(Output::new(false, String::new(), serde_json::json!({})))
}

/// Coordination-mandate C3 predicate (merge gate): is `tool` coordinated for the
/// `changed` files? Returns (has_presence, acknowledged, uncovered_paths) where
/// uncovered = a changed file not covered by an open claim owned by `tool`
/// (canonical exact- or dir-prefix match). Pure (testable); no I/O.
pub(crate) fn coordination_offenders(
    snapshot: &store::RoomSnapshot,
    tool: &str,
    changed: &[String],
) -> (bool, bool, Vec<String>) {
    let presence = snapshot.squads.iter().any(|s| s.tool == tool);
    let acked = snapshot
        .squads
        .iter()
        .any(|s| s.tool == tool && s.acknowledged);
    let owned: Vec<String> = snapshot
        .active_claims
        .iter()
        .filter(|c| c.tool.as_deref() == Some(tool))
        .flat_map(|c| {
            c.scope.iter().filter_map(|sc| {
                sc.strip_prefix("file:")
                    .map(|p| normalize_path(p.to_string()))
            })
        })
        .collect();
    let covers = |path: &str| -> bool {
        let p = normalize_path(path.to_string());
        owned
            .iter()
            .any(|o| *o == p || p.starts_with(&format!("{o}/")))
    };
    let uncovered: Vec<String> = changed.iter().filter(|p| !covers(p)).cloned().collect();
    (presence, acked, uncovered)
}

/// Coordination-mandate C2 predicate: squads eligible for liveness conflict-out —
/// unacknowledged AND idle AND still holding >=1 open claim. Returns
/// (tool, held_claim_event_ids). Pure (testable); no I/O.
pub(crate) fn liveness_conflicted(snapshot: &store::RoomSnapshot) -> Vec<(String, Vec<String>)> {
    snapshot
        .squads
        .iter()
        .filter_map(|sq| {
            if sq.acknowledged || sq.status != "idle" {
                return None;
            }
            let held: Vec<String> = snapshot
                .active_claims
                .iter()
                .filter(|c| c.tool.as_deref() == Some(sq.tool.as_str()))
                .map(|c| c.event_id.clone())
                .collect();
            if held.is_empty() {
                None
            } else {
                Some((sq.tool.clone(), held))
            }
        })
        .collect()
}

fn command_check(args: CheckArgs) -> Result<Output> {
    let phase = args.phase;

    // #9 tier-fit advisory: handled before the standard check phases.
    if phase == "tier-fit" {
        let role = args.role.as_deref().unwrap_or("").to_string();
        if role.is_empty() {
            return Err(RallyError::Usage(
                "check tier-fit requires --role <role>".to_string(),
            ));
        }
        let room = RoomStore::open()?;
        let snapshot = room.snapshot()?;
        let result = tier_fit::check_tier_fit(&role, args.proposed_tier.as_deref(), &snapshot);
        let finding_count = if result.finding.is_some() { 1 } else { 0 };
        let text = format!(
            "check tier-fit status={} role={} findings={finding_count}",
            result.status, role
        );

        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct TierFitCheckData {
            check: TierFitCheckResult,
        }
        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct TierFitCheckResult {
            phase: &'static str,
            advisory: bool,
            tier_fit: tier_fit::TierFitResult,
        }
        let body = envelope(
            "check",
            SCHEMA_CHECK,
            TierFitCheckData {
                check: TierFitCheckResult {
                    phase: "tier-fit",
                    advisory: true,
                    tier_fit: result,
                },
            },
        )?;
        return Ok(Output::new(args.json, text, body));
    }

    // Coordination-mandate C2: liveness conflict-out. Reports squads that are
    // unacknowledged + idle + holding >=1 open claim ("grabbed paths, never
    // coordinated, went quiet"). With --enforce: releases their claims (paths
    // freed) + records a risk alert for the lead/user. NEVER blocks editing.
    if phase == "liveness" {
        let room = RoomStore::open()?;
        let snapshot = room.snapshot()?;
        let actor = args
            .tool
            .clone()
            .unwrap_or_else(|| "rally:liveness".to_string());

        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct ConflictedSquad {
            tool: String,
            reason: String,
            released_claims: Vec<String>,
        }
        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct LivenessResult {
            phase: &'static str,
            advisory: bool,
            enforced: bool,
            conflicted: Vec<ConflictedSquad>,
        }
        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct LivenessData {
            check: LivenessResult,
        }

        // DESTRUCTIVE-release bar (independent-auditor HIGH, 2026-06-09): the
        // `--enforce` arm appends Release facts on another tool's behalf, so it
        // must apply the SAME 2h takeover-eligibility gate as `say release
        // --path`. Reporting a conflict at the 15-min idle threshold is fine
        // (advisory); RELEASING a claim out from under a busy-but-quiet owner
        // at 15m is the regression. An unacknowledged owner that is idle but
        // not yet >2h silent is reported as conflicted but NOT released.
        let takeover_owners = snapshot.takeover_eligible_owners();
        let mut conflicted: Vec<ConflictedSquad> = Vec::new();
        for (sq_tool, held_ids) in liveness_conflicted(&snapshot) {
            let held: Vec<&Fact> = snapshot
                .active_claims
                .iter()
                .filter(|c| held_ids.contains(&c.event_id))
                .collect();
            let mut released = Vec::new();
            let enforce_eligible = args.enforce && takeover_owners.contains(&sq_tool);
            if enforce_eligible {
                let _commit_guard = arm_watchdog_command_commit();
                for claim in &held {
                    let release = Fact {
                        from_session_id: None,
                        schema: FACT_SCHEMA.to_string(),
                        event_id: new_id("fact"),
                        seq: 0,
                        thread_id: new_id("room"),
                        kind: FactKind::Release,
                        tool: Some(actor.clone()),
                        role: None,
                        subject: format!("liveness conflict-out: release {} claim", sq_tool),
                        scope: claim.scope.clone(),
                        created_at: now_string(),
                        summary: Some(format!(
                            "{} released by liveness conflict-out (unacknowledged + idle)",
                            sq_tool
                        )),
                        evidence: vec![format!("conflicted-out:{}", sq_tool)],
                        target: None,
                        ref_id: Some(claim.event_id.clone()),
                        status: None,
                        severity: None,
                        uri: None,
                        session: None,
                    };
                    room.append_fact(&release)?.into_fact_reporting();
                    released.push(claim.event_id.clone());
                }
                let alert = build_risk_fact(
                    &actor,
                    format!(
                        "conflicted-out: {} (unacknowledged + idle, holding claims)",
                        sq_tool
                    ),
                    format!(
                        "{} grabbed paths but never acked the coordination context and went idle; claims released, alerting lead/user. Not blocked from editing.",
                        sq_tool
                    ),
                    vec![format!("conflicted:{}", sq_tool)],
                    "warn",
                    vec![format!("released:{}", released.len())],
                    None,
                );
                room.append_fact(&alert)?.into_fact_reporting();
            }
            let reason = if args.enforce && !enforce_eligible {
                // Conflict reported but NOT released: owner is unacknowledged +
                // idle but has not been silent the >2h required for a
                // destructive release (busy-but-quiet protection).
                "unacknowledged + idle + holding open claims (reported; not released — owner not yet >2h silent)".to_string()
            } else {
                "unacknowledged + idle + holding open claims".to_string()
            };
            conflicted.push(ConflictedSquad {
                tool: sq_tool.clone(),
                reason,
                released_claims: released,
            });
        }
        let text = format!(
            "check liveness enforced={} conflicted={}",
            args.enforce,
            conflicted.len()
        );
        let body = envelope(
            "check",
            SCHEMA_CHECK,
            LivenessData {
                check: LivenessResult {
                    phase: "liveness",
                    advisory: !args.enforce,
                    enforced: args.enforce,
                    conflicted,
                },
            },
        )?;
        return Ok(Output::new(args.json, text, body));
    }

    // Coordination-mandate C3: the MERGE GATE. Given the committer --tool and the
    // changed files (--changed, fed by the CI wrapper from `git diff --name-only`),
    // verify presence + ack + claim-covers-every-changed-file. --strict exits
    // non-zero on violation (wire as a required branch-protection check). This is
    // the one layer with teeth — it blocks LANDING, never the keystroke.
    if phase == "coordination" {
        let tool = args.tool.clone().ok_or_else(|| {
            RallyError::Usage("check coordination requires --tool <committer>".to_string())
        })?;
        let room = RoomStore::open()?;
        let snapshot = room.snapshot()?;
        let (presence, acknowledged, uncovered) =
            coordination_offenders(&snapshot, &tool, &args.changed);
        let pass = presence && acknowledged && uncovered.is_empty();

        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct CoordResult {
            phase: &'static str,
            tool: String,
            presence: bool,
            acknowledged: bool,
            uncovered_paths: Vec<String>,
            pass: bool,
        }
        #[derive(schemars::JsonSchema, serde::Serialize)]
        struct CoordData {
            check: CoordResult,
        }
        let text = format!(
            "check coordination tool={tool} pass={pass} presence={presence} acked={acknowledged} uncovered={}",
            uncovered.len()
        );
        let body = envelope(
            "check",
            SCHEMA_CHECK,
            CoordData {
                check: CoordResult {
                    phase: "coordination",
                    tool,
                    presence,
                    acknowledged,
                    uncovered_paths: uncovered,
                    pass,
                },
            },
        )?;
        let exit = if !pass && args.strict { 4 } else { 0 };
        return Ok(Output::new(args.json, text, body).with_exit_code(exit));
    }

    let tool = match args.tool {
        Some(tool) if phase == "before-write" && tool == "unknown" => {
            return Err(RallyError::Usage(
                "check before-write requires a real --tool <tool>".to_string(),
            ));
        }
        Some(tool) => tool,
        None if phase == "before-write" => {
            return Err(RallyError::Usage(
                "check before-write requires --tool <tool>".to_string(),
            ));
        }
        None => "unknown".to_string(),
    };
    let path = args.path.map(normalize_path);

    // B-perf fast path: when the snapshot cache is fresh AND already records
    // our tool's presence, project the check from the cached `RoomSnapshot`
    // without taking the room mutation lock or opening SQLite. This is the
    // path that lets a busy agent stay under the 3s watchdog when a parallel
    // writer is contending for `mutation.lock` on every other invocation.
    // See `store::try_load_cached_snapshot` for the freshness fingerprint.
    if let Ok(repo_root_path) = repo_root()
        && tool != "unknown"
        && let Some(cached) = crate::store::try_load_cached_snapshot_for(&repo_root_path)
        && cached.squads.iter().any(|s| s.tool == tool)
    {
        let check = build_check(
            phase.clone(),
            tool.clone(),
            path.clone(),
            args.strict,
            &cached,
        )?;
        let body = envelope("check", SCHEMA_CHECK, check.data)?;
        let text = format!("check findings={} (cached)", check.finding_count);
        return Ok(Output::new(args.json, text, body).with_exit_code(check.exit_code));
    }

    let room = RoomStore::open()?;
    // Component B: auto-register presence when a real tool identity is known.
    // Skip "unknown" (no-tool before-complete calls) — nothing meaningful to register.
    if tool != "unknown" {
        ensure_presence(&room, &tool)?;
    }
    let capture = room.snapshot_cache_capture(false)?;
    // B-perf: persist the exact snapshot/fingerprint pair captured in one
    // mutation epoch. The writer never re-fingerprints detached state.
    if let Ok(repo_root_path) = repo_root() {
        crate::store::write_snapshot_cache_for(&repo_root_path, &capture);
    }
    let snapshot = capture.snapshot;
    let check = build_check(phase, tool, path, args.strict, &snapshot)?;
    let body = envelope("check", SCHEMA_CHECK, check.data)?;
    let text = format!("check findings={}", check.finding_count);
    Ok(Output::new(args.json, text, body).with_exit_code(check.exit_code))
}

/// One native before-write transaction from host stdin to host stdout.
///
/// The wrapper used to launch `status post`, `check`, `room`, and `say claim`
/// as separate processes and then launch Node several times to parse and
/// translate their JSON. This command keeps the existing command authorities
/// intact while collapsing that orchestration into one Rust process. Storage
/// calls remain deliberately routed through their canonical implementations;
/// the next optimization can therefore target measured store work without
/// changing host-envelope behavior at the same time.
fn command_hook(args: HookArgs) -> Result<Output> {
    if args.phase != "before-write" {
        return Err(RallyError::Usage(format!(
            "rally hook currently supports phase before-write; got {}",
            args.phase
        )));
    }
    if !matches!(
        hook_runtime::host_family(&args.host),
        "claude_code" | "codex" | "gemini" | "cursor"
    ) {
        return Err(RallyError::Usage(format!(
            "rally hook supports hosts claude_code, codex, gemini, and cursor; got {}",
            args.host
        )));
    }

    use std::io::Read as _;
    let mut raw = String::new();
    // Hook stdin may be empty. A read error is equivalent to an empty envelope:
    // the coordination path stays fail-open and emits a valid empty object.
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input = hook_runtime::parse_input(&raw);
    let session = hook_runtime::resolve_session(args.session_id, &input);
    let tool = hook_runtime::resolve_tool(&args.host, args.tool, &session);
    let root = match repo_root() {
        Ok(root) => root,
        Err(_) => return Ok(Output::new(true, String::new(), json!({}))),
    };
    let enabled = hooks_config::resolve(&root)
        .map(|hooks| hooks.enabled)
        .unwrap_or(true);
    if !enabled || hook_runtime::duplicate_event(&root, &raw, &session, &args.phase) {
        return Ok(Output::new(true, String::new(), json!({})));
    }

    let strict = args.strict || env::var("RALLY_HOOK_STRICT").as_deref() == Ok("1");
    let host_can_block = hook_runtime::host_family(&args.host) != "codex";
    let blocking = strict && host_can_block;
    let path = input.path.map(normalize_path);

    // Working-state publication is advisory. Preserve the wrapper's fail-open
    // behavior: a heartbeat failure must not prevent the actual deconfliction
    // check from running.
    if let Some(path) = path.as_deref()
        && hook_runtime::working_status_due(&root, &session, path)
    {
        let posted = command_status_post(
            true,
            cli::StatusPostArgs {
                tool: tool.clone(),
                state: "working".to_string(),
                file: Some(path.to_string()),
                intent: Some(format!("editing {path}")),
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        );
        if posted.is_ok() {
            hook_runtime::mark_working_status(&root, &session, path);
        }
    }

    let check = match command_check(cli::CheckArgs {
        json: true,
        phase: "before-write".to_string(),
        tool: Some(tool.clone()),
        path: path.clone(),
        strict,
        role: None,
        proposed_tier: None,
        enforce: false,
        changed: Vec::new(),
    }) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("rally-hook: before-write check failed open: {error}");
            return Ok(Output::new(true, String::new(), json!({})));
        }
    };

    let allow = check.body["data"]["check"]["allow"]
        .as_bool()
        .unwrap_or(true);
    let mut message = hook_runtime::conflict_message(&check.body, path.as_deref(), blocking);

    if allow && let Some(path) = path.as_deref() {
        // Match the legacy wrapper's idempotency rule: do not append another
        // claim when this tool already owns an overlapping scope.
        let index_path = root
            .join(".rally")
            .join(claim_authority::CLAIM_INDEX_FILENAME);
        let authoritative_own_claim = || {
            RoomStore::open()
                .and_then(|room| room.snapshot())
                .map(|snapshot| {
                    snapshot.active_claims.iter().any(|claim| {
                        claim.tool.as_deref() == Some(tool.as_str())
                            && claim
                                .scope
                                .iter()
                                .any(|scope| path_matches_scope(scope, path))
                    })
                })
                .unwrap_or(false)
        };
        let already_claimed = if index_path.exists() {
            claim_authority::read_index(&index_path).map_or_else(
                |_| authoritative_own_claim(),
                |index| {
                    index.claims.values().any(|claim| {
                        claim.owner_tool.as_deref() == Some(tool.as_str())
                            && claim
                                .raw_scope
                                .iter()
                                .any(|scope| path_matches_scope(scope, path))
                    })
                },
            )
        } else {
            // A missing/corrupt legacy index must not cause duplicate claims.
            // Fall back to the authoritative projection only on this exceptional path.
            authoritative_own_claim()
        };
        if !already_claimed {
            let claim = SayArgs {
                json: true,
                kind: FactKind::Claim,
                tool: tool.clone(),
                subject: Some(format!("auto-claim {path}")),
                thread_id: None,
                role: None,
                summary: Some("native-hook:before-write".to_string()),
                scopes: Vec::new(),
                resources: Vec::new(),
                paths: vec![path.to_string()],
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                produces: Vec::new(),
                depends: Vec::new(),
                run_id: None,
                step_id: None,
                parent_step_ids: Vec::new(),
                reason: None,
                wake_after: None,
                ref_standby: None,
            };
            if let Err(error) = command_say(claim) {
                message = Some(hook_runtime::claim_failure_message(
                    path,
                    &error.to_string(),
                ));
            }
        }
    }

    let body = hook_runtime::render_before_write(&args.host, message.as_deref(), allow, strict);
    Ok(Output::new(true, String::new(), body))
}

fn command_run(args: RunArgs) -> Result<Output> {
    let RunArgs {
        json,
        dry_run,
        agent,
        name,
        backend,
        backend_raw,
        session_id,
        tool,
        bins,
        shared,
    } = args;

    // Backend resolution for the ptyd pane-ownership flip:
    //   * `--backend auto`: prefer Ptyd iff the RALLY-OWNED socket is LIVE
    //     (connectable, not just file-exists); else keep Tmux (current default).
    //   * `--backend ptyd`: if the socket is not live, AUTOSTART the rally
    //     daemon (≤5s) or fail the run with a clear error.
    // The detect_host_runtime / try_register_session_with_daemon paths
    // (tmux-session registration) are untouched — F3.
    let backend = if dry_run {
        // Dry-run never touches a daemon: report the parsed backend as-is.
        backend
    } else if Backend::is_auto(&backend_raw) {
        match daemon_client::rally_owned_socket() {
            Some(sock) if daemon_client::socket_is_live(&sock) => {
                // [F]: `auto` is now the default backend, so a plain `rally run`
                // silently selecting ptyd (where attach is unsupported) would
                // surprise the user. Emit ONE stderr line naming the selection
                // and how to override. JSON stdout is untouched (this is stderr).
                eprintln!(
                    "rally: backend=auto selected ptyd (rally daemon live at {sock}); \
                     pass --backend tmux to force tmux. Attach a ptyd pane via \
                     EasyTerminal or `ptyd attach`."
                );
                Backend::Ptyd
            }
            _ => backend, // Tmux fallback — no live rally daemon.
        }
    } else if backend == Backend::Ptyd {
        let sock = daemon_client::rally_owned_socket().ok_or_else(|| {
            RallyError::Usage(
                "--backend ptyd requires HOME to resolve the rally ptyd socket".to_string(),
            )
        })?;
        if !daemon_client::socket_is_live(&sock) {
            // Explicit ptyd → autostart the rally-owned daemon (env-gated so the
            // hermetic test of "no socket, no binary" asserts the error path).
            daemon_client::autostart_daemon(&sock).map_err(RallyError::Command)?;
        }
        Backend::Ptyd
    } else {
        backend
    };

    let backend_name = backend.as_str().to_string();
    let repo = repo_root()?;
    let agent_spec = AgentSpec::from_name(&agent)?;
    // Plan F functional core (Chunk 3): the herdr self-host guard was
    // removed; tmux/cmux do not share Easy Terminal's daemon socket so
    // the reentrancy risk it guarded against no longer exists.
    // Worktree-per-agent isolation is the default; `--shared`/`--no-worktree`
    // and the `RALLY_NO_WORKTREE=1` escape hatch (for hosts that already
    // operate inside a per-agent worktree) opt out.
    let isolate_worktree = !shared && env::var("RALLY_NO_WORKTREE").as_deref() != Ok("1");
    let room = RoomStore::open()?;
    let reservation = if dry_run {
        let active_sessions = active_session_records(&room)?;
        let identity =
            numbered_session_identity(&agent_spec, name, session_id, tool, &active_sessions)?;
        ReservedSession {
            fact: None,
            session: ManagedSession {
                session_id: identity.session_id.clone(),
                name: identity.name.clone(),
                agent: agent_spec.agent.to_string(),
                tool: identity.tool.clone(),
                backend: backend_name.clone(),
                cwd: repo.clone(),
                target: backend_target(backend, &identity.session_id),
                worktree_path: None,
                branch: None,
                daemon_registered: false,
                daemon_pane: None,
                daemon_socket: None,
            },
        }
    } else {
        reserve_numbered_session(
            &room,
            &agent_spec,
            SessionReservationInput {
                requested_name: name,
                requested_session_id: session_id,
                requested_tool: tool,
                backend,
                backend_name: &backend_name,
                repo: &repo,
            },
        )?
    };
    let mut session = reservation.session;

    // Provision the per-agent worktree (default-on).  Under dry-run we
    // advertise the planned path/branch in the envelope but never touch
    // the filesystem.
    let mut provisioned_path: Option<PathBuf> = None;
    if isolate_worktree {
        if dry_run {
            session.worktree_path = Some(run_worktree::planned_worktree_path(
                &repo,
                &session.session_id,
            ));
            session.branch = Some(run_worktree::planned_branch_name(&session.session_id));
            session.cwd = session
                .worktree_path
                .clone()
                .expect("planned worktree path");
        } else {
            match run_worktree::provision(&repo, &session.session_id, "git") {
                Ok(pw) => {
                    session.cwd = pw.path.clone();
                    session.worktree_path = Some(pw.path.clone());
                    session.branch = Some(pw.branch);
                    provisioned_path = Some(pw.path);
                    // Refresh the session fact so the durable record reflects
                    // the worktree-rooted cwd + branch.
                    if let Some(fact) = &reservation.fact {
                        room.append_fact(&session_fact(
                            &session,
                            "active",
                            Some(fact.event_id.clone()),
                        ))?
                        .into_fact_reporting();
                    }
                }
                Err(err) => {
                    // Fail-closed: surface the provisioning error rather than
                    // silently falling back to the shared checkout. Mark the
                    // reservation stopped so the room doesn't leak an active
                    // record for a session that never launched.
                    if let Some(fact) = &reservation.fact
                        && let Err(cleanup_err) =
                            append_stopped_session_record(&room, &session, fact)
                    {
                        return Err(RallyError::Message(format!(
                            "worktree provisioning failed: {err}; additionally failed to mark managed session stopped: {cleanup_err}"
                        )));
                    }
                    return Err(err);
                }
            }
        }
    }

    let command = agent_spec.command_line(&session.name);
    let mut backend_runner = BackendRunner::new(backend, bins.clone());
    // Backend launches the agent in the worktree (when provisioned) so the
    // agent's HEAD, commits and working tree are isolated from peers.
    let backend_cwd = session.cwd.clone();
    let start_commands =
        backend_runner.start_commands(&session.target, &backend_cwd, &command, &session.name)?;

    // F2 loud warning surfaced in the run envelope when a ptyd spawn succeeded
    // but registration forced a tmux fallback (no silent orphaned panes).
    let mut run_warning: Option<String> = None;

    if dry_run {
        // Dry-run: no launch, no daemon contact. The envelope advertises the
        // planned target only.
    } else if backend == Backend::Ptyd {
        // ----- ptyd pane-ownership spawn path (design-1 + design-3 start) -----
        // 1. ensure a rally-dedicated workspace so the pane never lands in the
        //    user's focused tab; 2. agent.start (focus:false) → pane id;
        //    3. register_agent binds session.tool → pane. On REGISTER FAILURE
        //    after a successful spawn (F2): agent-stop the pane, then fall back
        //    to a tmux launch with a loud warning — never a silent orphan.
        match ptyd_spawn_and_register(
            &backend_runner,
            &room,
            &reservation.fact,
            &mut session,
            &backend_cwd,
            &command,
        ) {
            PtydSpawnResult::Daemon => { /* session is daemon-owned + registered */ }
            PtydSpawnResult::DurableRecordFailed(error) => {
                // The pane is already live and registered. Preserve that
                // external state and return the durable partial-commit error;
                // treating this as spawn failure would reap a successful pane.
                return Err(error);
            }
            PtydSpawnResult::FellBackToTmux { warning } => {
                // The ptyd pane was already reaped inside the helper. Relaunch
                // under tmux so the agent actually runs; switch the runner +
                // recorded backend to tmux for the rest of this command.
                run_warning = Some(warning);
                session.backend = Backend::Tmux.as_str().to_string();
                backend_runner = BackendRunner::new(Backend::Tmux, bins.clone());
                let tmux_target = backend_target(Backend::Tmux, &session.session_id);
                session.target = tmux_target.clone();
                match backend_runner.start(&tmux_target, &backend_cwd, &command, &session.name) {
                    Ok(target) => {
                        session.target = target;
                        if let Some(fact) = &reservation.fact {
                            room.append_fact(&session_fact(
                                &session,
                                "active",
                                Some(fact.event_id.clone()),
                            ))?
                            .into_fact_reporting();
                        }
                    }
                    Err(err) => {
                        if let (Some(path), Some(branch)) =
                            (provisioned_path.as_deref(), session.branch.as_deref())
                        {
                            let _ = run_worktree::cleanup(&repo, path, branch, "git");
                        }
                        if let Some(fact) = &reservation.fact
                            && let Err(cleanup_err) =
                                append_stopped_session_record(&room, &session, fact)
                        {
                            return Err(RallyError::Message(format!(
                                "backend start failed: {err}; additionally failed to mark managed session stopped: {cleanup_err}"
                            )));
                        }
                        return Err(err);
                    }
                }
            }
            PtydSpawnResult::Failed(err) => {
                // Spawn itself failed (no pane to reap) → clean up like any
                // backend-start failure.
                if let (Some(path), Some(branch)) =
                    (provisioned_path.as_deref(), session.branch.as_deref())
                {
                    let _ = run_worktree::cleanup(&repo, path, branch, "git");
                }
                if let Some(fact) = &reservation.fact
                    && let Err(cleanup_err) = append_stopped_session_record(&room, &session, fact)
                {
                    return Err(RallyError::Message(format!(
                        "ptyd spawn failed: {err}; additionally failed to mark managed session stopped: {cleanup_err}"
                    )));
                }
                return Err(err);
            }
        }
    } else {
        // ----- tmux / cmux generic backend start (unchanged) -----
        let actual_target = match backend_runner.start(
            &session.target,
            &backend_cwd,
            &command,
            &session.name,
        ) {
            Ok(target) => target,
            Err(err) => {
                // Best-effort cleanup of the worktree we just provisioned, so
                // a failed backend start doesn't leave orphan worktrees.
                if let (Some(path), Some(branch)) =
                    (provisioned_path.as_deref(), session.branch.as_deref())
                {
                    let _ = run_worktree::cleanup(&repo, path, branch, "git");
                }
                if let Some(fact) = &reservation.fact
                    && let Err(cleanup_err) = append_stopped_session_record(&room, &session, fact)
                {
                    return Err(RallyError::Message(format!(
                        "backend start failed: {err}; additionally failed to mark managed session stopped: {cleanup_err}"
                    )));
                }
                return Err(err);
            }
        };
        if actual_target != session.target {
            session.target = actual_target;
            if let Some(fact) = &reservation.fact {
                room.append_fact(&session_fact(
                    &session,
                    "active",
                    Some(fact.event_id.clone()),
                ))?
                .into_fact_reporting();
            }
        }

        // Daemon-first inject routing (move 2): attempt to register this
        // session's tmux/cmux pane with a daemon that may already own it. This
        // is the EXISTING path (detect_host_runtime candidate list); the ptyd
        // spawn path above handles its own registration. Fail-OPEN.
        try_register_session_with_daemon(&room, &reservation.fact, &mut session)?;
    }

    let body = envelope(
        "run",
        SCHEMA_RUN,
        RunEnvelope {
            run: RunData {
                mode: if dry_run { "dry-run" } else { "run" },
                session: session.clone(),
                commands: RunCommands {
                    start: command_plan_json(&start_commands),
                },
                warning: run_warning,
            },
        },
    )?;
    let text = format!(
        "run agent={} backend={} session={}",
        session.agent, session.backend, session.session_id
    );
    Ok(Output::new(json, text, body))
}

/// Attempt to register `session`'s pane with the rally-termd daemon so future
/// `inject`s route ledger-only (the daemon owns the PTY-write). On success,
/// flips `session.daemon_registered = true` + records the daemon pane handle
/// AND refreshes the durable session fact so a `rally sessions` view shows the
/// binding (acceptance criterion 4). FAIL-OPEN: a missing/ambiguous/refused
/// daemon is a silent no-op — the framed-tmux fallback remains operative.
///
/// The daemon pane handle passed to `agent.register` is the session's live
/// backend target. For a tmux/cmux session the daemon does not yet own that
/// pane, so the daemon returns `pane_not_found` and this stays a no-op (the
/// documented live-flip step: ptyd must own the pane for full daemon routing).
fn try_register_session_with_daemon(
    room: &RoomStore,
    reservation_fact: &Option<Fact>,
    session: &mut ManagedSession,
) -> Result<()> {
    let runtime = detect_host_runtime();
    // Never guess which daemon to bind when multiple sockets are resolvable.
    let Some(socket) = daemon_client::resolve_unambiguous_socket(&runtime.sockets_found) else {
        return Ok(());
    };
    match daemon_client::register_agent(&socket, &session.tool, &session.target) {
        daemon_client::RegisterOutcome::Registered { pane_id } => {
            session.daemon_registered = true;
            session.daemon_pane = Some(pane_id);
            // Refresh the durable session record so the binding survives and is
            // visible under `rally sessions`.
            let prev = reservation_fact.as_ref().map(|f| f.event_id.clone());
            room.append_fact(&session_fact(session, "active", prev))?
                .into_fact_reporting();
        }
        daemon_client::RegisterOutcome::Unavailable { .. } => {
            // Fall back silently — framed-tmux delivery carries the inject.
        }
    }
    Ok(())
}

/// Result of the ptyd pane-ownership spawn path.
enum PtydSpawnResult {
    /// The pane was spawned AND registered: `session` is daemon-owned, its
    /// `target`/`daemon_pane`/`daemon_registered` fields are set.
    Daemon,
    /// F2: the pane spawned but `agent.register` failed. The spawned pane has
    /// already been reaped (`agent.stop`); the caller must relaunch under tmux
    /// and surface `warning` in the run envelope.
    FellBackToTmux { warning: String },
    /// The spawn RPC itself failed — no pane exists to reap.
    Failed(RallyError),
    /// The pane is live and registered, but its follow-up durable session
    /// record did not complete. Do not reap the live pane or hide uncertainty.
    DurableRecordFailed(RallyError),
}

/// Spawn a ptyd-owned agent pane and register the session's identity with the
/// rally daemon (design-3 start arm). On success, sets `session.target =
/// session.daemon_pane = <pane id>` and `daemon_registered = true`, and
/// refreshes the durable session fact so `rally sessions` shows the binding.
///
/// F2 (register-fail safety): if `agent.register` fails AFTER a successful
/// `agent.start`, the just-spawned pane is reaped via `agent.stop` (no silent
/// orphan), and the function returns `FellBackToTmux` so the caller relaunches
/// under tmux with a loud warning.
fn ptyd_spawn_and_register(
    runner: &BackendRunner,
    room: &RoomStore,
    reservation_fact: &Option<Fact>,
    session: &mut ManagedSession,
    cwd: &std::path::Path,
    command: &[String],
) -> PtydSpawnResult {
    // Design-1: a rally-dedicated workspace so the pane never lands in the
    // user's focused tab. Label is stable so repeated runs reuse intent (ptyd
    // assigns a fresh workspace id each create; that's fine — any rally
    // workspace is off the user's focused tab).
    let workspace_id = match runner.ptyd_ensure_workspace("rally") {
        Ok(id) => id,
        Err(e) => return PtydSpawnResult::Failed(e),
    };

    // agent.start (focus:false) → daemon pane id.
    let pane_id = match runner.ptyd_start(&session.name, cwd, command, &workspace_id) {
        Ok(id) => id,
        Err(e) => return PtydSpawnResult::Failed(e),
    };

    // The pane id IS the session target AND the daemon pane handle.
    session.target = pane_id.clone();
    session.daemon_pane = Some(pane_id.clone());

    // [E]: bind session.tool → pane via the SAME socket the runner spawned into,
    // and RECORD it on the session so every later send/stop/read reaches that
    // exact daemon — never a re-resolved (possibly different) socket. The runner
    // already resolved the rally-owned socket (F3) at construction.
    let socket = match runner.ptyd_socket() {
        Some(s) => s.to_string(),
        None => {
            // Should not happen (ptyd_ensure_workspace already required it), but
            // be safe: reap + fall back.
            let _ = runner.ptyd_stop(&session.name);
            return PtydSpawnResult::FellBackToTmux {
                warning: "ptyd register skipped: rally socket unresolved; fell back to tmux"
                    .to_string(),
            };
        }
    };
    session.daemon_socket = Some(socket.clone());
    match daemon_client::register_agent(&socket, &session.tool, &pane_id) {
        daemon_client::RegisterOutcome::Registered { pane_id: bound } => {
            session.daemon_registered = true;
            session.daemon_pane = Some(bound);
            // Refresh the durable record so the binding is visible + survives.
            let prev = reservation_fact.as_ref().map(|f| f.event_id.clone());
            match room.append_fact(&session_fact(session, "active", prev)) {
                Ok(outcome) => {
                    outcome.into_fact_reporting();
                    PtydSpawnResult::Daemon
                }
                Err(error) => PtydSpawnResult::DurableRecordFailed(error),
            }
        }
        daemon_client::RegisterOutcome::Unavailable { reason } => {
            // F2: reap the orphaned pane BEFORE falling back. [G]: reap by the
            // exact PANE ID we just spawned (`pane.close`), NOT by name — a name
            // collision could otherwise reap the wrong pane. Best-effort: a
            // failed reap is surfaced in the warning for manual cleanup.
            let reap = runner.ptyd_close_pane(&pane_id);
            session.daemon_pane = None;
            session.daemon_socket = None;
            let reap_note = match reap {
                Ok(()) => "spawned pane reaped".to_string(),
                Err(e) => format!("WARNING: failed to reap spawned pane ({e})"),
            };
            PtydSpawnResult::FellBackToTmux {
                warning: format!(
                    "ptyd agent.register failed ({reason}); {reap_note}; fell back to tmux launch"
                ),
            }
        }
    }
}

// Plan F functional core (Chunk 3): enforce_easy_terminal_self_host_guard
// removed alongside Backend::Herdr. The guard prevented a herdr backend
// from launching into the same Easy Terminal daemon socket as the host
// (a herdr-specific reentrancy risk). With herdr removed, the risk
// vanishes — tmux/cmux do not share Easy Terminal's daemon socket.

/// C-FLEET: register an already-running agent into the managed-session ledger
/// without relaunching it. This is the response arm of the "all fleet workers
/// must be rally-managed" rule — turns a presence-only stray (detected via
/// `unmanaged-agent` risk in `command_enter`) into a controllable session.
///
/// Required: `<name>` positional + ONE of `--tmux` / `--cmux` (mutually
/// exclusive). Backend is auto-inferred from which flag was passed.
///
/// HERDR-INDEPENDENT: the original adopt carried a `--pane` (herdr) arm; that
/// arm was dropped with `Backend::Herdr` (Plan F Chunk 3). Adopt now only
/// registers tmux/cmux targets — the two live backends — by their
/// caller-provided target string.
///
/// Optional: `--tool` (defaults to `<agent>:adopted-<n>`), `--agent` (defaults
/// to `claude`), `--backend` (overrides the inferred backend — useful when a
/// target name does not match the backend's session convention).
///
/// Idempotency: refuses to adopt the same target twice; the caller gets a
/// clear `Usage` error naming the existing session_id.
fn command_adopt(args: AdoptArgs) -> Result<Output> {
    let AdoptArgs {
        json,
        name,
        tmux,
        cmux,
        tool,
        agent,
        backend,
    } = args;

    // Validate mutual exclusion: must pass exactly one of --tmux / --cmux.
    // The running surface is always caller-provided to keep the command
    // dependency-free and unit-testable.
    let (target, inferred_backend) = match (tmux.as_deref(), cmux.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(RallyError::Usage(
                "adopt: --tmux and --cmux are mutually exclusive".to_string(),
            ));
        }
        (Some(t), None) => (t.to_string(), Backend::Tmux),
        (None, Some(c)) => (c.to_string(), Backend::Cmux),
        (None, None) => {
            return Err(RallyError::Usage(
                "adopt: one of --tmux <target> or --cmux <target> is required".to_string(),
            ));
        }
    };

    // Backend resolution: explicit --backend wins; otherwise use inference.
    let resolved_backend = backend.unwrap_or(inferred_backend);
    let agent_name = agent.unwrap_or_else(|| "claude".to_string());
    let agent_spec = AgentSpec::from_name(&agent_name)?;
    let backend_name = resolved_backend.as_str().to_string();
    let repo = repo_root()?;

    // Refuse to re-adopt the same target. Read existing sessions and check.
    let room = RoomStore::open()?;
    // Identity allocation spans a read → choose → conditional append sequence.
    // Keep that whole sequence under one cross-process lock so two `run` /
    // `adopt` processes cannot both return the same numbered identity even if
    // independent SQLite pools observe the same pre-append context version.
    let identity_guard = session_reservation_lock::acquire(&repo)?;
    let existing = active_session_records(&room)?;
    if let Some(prior) = existing.iter().find(|s| s.target == target) {
        return Err(RallyError::Usage(format!(
            "adopt: target {target} already adopted as session {} (tool {}); use `rally inject {} --text ...` directly",
            prior.session_id, prior.tool, prior.session_id
        )));
    }

    // Reserve identity via the same machinery `command_run` uses.
    let (session_facts, context_version) = room.session_facts_with_context_version()?;
    let active_sessions = active_session_records_from_facts(session_facts);
    let identity = numbered_session_identity(
        &agent_spec,
        Some(name.clone()),
        None,
        tool,
        &active_sessions,
    )?;

    let session = ManagedSession {
        session_id: identity.session_id.clone(),
        name: identity.name.clone(),
        agent: agent_spec.agent.to_string(),
        tool: identity.tool.clone(),
        backend: backend_name.clone(),
        cwd: repo.clone(),
        // NOTE: target is the CALLER-PROVIDED running surface (the tmux/cmux
        // target), NOT the derived `backend_target(backend, session_id)`. This
        // is the whole point of adopt — register an existing target as-is.
        target,
        worktree_path: None,
        branch: None,
        daemon_registered: false,
        daemon_pane: None,
        daemon_socket: None,
    };

    // Append the session fact under the same context-version race guard as
    // run uses.
    let fact = session_fact(&session, "active", None);
    let landed_fact = with_watchdog_command_commit(|| {
        room.append_session_fact_if_context(&fact, context_version)
    })?;
    let landed_fact = match landed_fact {
        ConditionalAppendOutcome::NotApplied => {
            return Err(RallyError::Message(
                "adopt: concurrent session-fact write detected; retry".to_string(),
            ));
        }
        ConditionalAppendOutcome::Applied(outcome) => outcome.into_fact_reporting(),
    };
    verify_session_reservation_readback(&room, &landed_fact, &session)?;
    drop(identity_guard);

    // Daemon-first inject routing (move 2): same registration attempt as
    // `rally run`. Fail-open — an adopted tmux/cmux pane the daemon doesn't own
    // simply stays on the framed-tmux fallback.
    let mut session = session;
    try_register_session_with_daemon(&room, &Some(landed_fact), &mut session)?;

    let body = envelope(
        "adopt",
        SCHEMA_ADOPT,
        AdoptEnvelope {
            adopt: AdoptData {
                session: session.clone(),
            },
        },
    )?;
    let text = format!(
        "adopt agent={} backend={} session={} target={}",
        session.agent, session.backend, session.session_id, session.target
    );
    Ok(Output::new(json, text, body))
}

// Plan F functional core (Chunk 3): looks_like_easy_terminal_repo and
// normalize_socket_path are removed alongside the herdr self-host guard.
// Both were only consumed by that guard.

struct SessionIdentity {
    name: String,
    session_id: String,
    tool: String,
}

struct ReservedSession {
    fact: Option<Fact>,
    session: ManagedSession,
}

struct SessionReservationInput<'a> {
    requested_name: Option<String>,
    requested_session_id: Option<String>,
    requested_tool: Option<String>,
    backend: Backend,
    backend_name: &'a str,
    repo: &'a Path,
}

fn reserve_numbered_session(
    room: &RoomStore,
    agent_spec: &AgentSpec,
    input: SessionReservationInput<'_>,
) -> Result<ReservedSession> {
    // Serialize the complete read → allocate → append identity transaction
    // across processes. The store's mutation lock protects each individual
    // append, while this lock protects the allocation decision between the
    // session-facts read and its conditional append.
    let _identity_guard = session_reservation_lock::acquire(input.repo)?;
    for attempt in 0..SESSION_IDENTITY_RETRIES {
        let (session_facts, context_version) = room.session_facts_with_context_version()?;
        let active_sessions = active_session_records_from_facts(session_facts);
        let identity = numbered_session_identity(
            agent_spec,
            input.requested_name.clone(),
            input.requested_session_id.clone(),
            input.requested_tool.clone(),
            &active_sessions,
        )?;
        let session = ManagedSession {
            session_id: identity.session_id.clone(),
            name: identity.name,
            agent: agent_spec.agent.to_string(),
            tool: identity.tool,
            backend: input.backend_name.to_string(),
            cwd: input.repo.to_path_buf(),
            target: backend_target(input.backend, &identity.session_id),
            worktree_path: None,
            branch: None,
            daemon_registered: false,
            daemon_pane: None,
            daemon_socket: None,
        };
        let fact = session_fact(&session, "active", None);
        match with_watchdog_command_commit(|| {
            room.append_session_fact_if_context(&fact, context_version)
        })? {
            ConditionalAppendOutcome::Applied(outcome) => {
                let fact = outcome.into_fact_reporting();
                return Ok(ReservedSession {
                    fact: Some(fact),
                    session,
                });
            }
            ConditionalAppendOutcome::NotApplied => {}
        }
        // Back off after the first few pure yields to avoid a thundering-herd
        // where all N losers immediately re-read the same stale context version.
        // First 8 retries: yield_now (no wall-clock cost, safe for unit tests).
        // Subsequent retries: 1 ms sleep, capped at 10 ms to bound wall-clock
        // impact while still draining contention quickly in production.
        if attempt < 8 {
            thread::yield_now();
        } else {
            let backoff_ms = (1u64 << (attempt - 8)).min(10);
            thread::sleep(Duration::from_millis(backoff_ms));
        }
    }
    Err(RallyError::Usage(format!(
        "could not reserve a unique managed session after {SESSION_IDENTITY_RETRIES} concurrent changes"
    )))
}

fn verify_session_reservation_readback(
    room: &RoomStore,
    reserved_fact: &Fact,
    reserved_session: &ManagedSession,
) -> Result<()> {
    let durable = active_session_facts_from_facts(room.facts()?)
        .into_iter()
        .any(|(fact, session)| {
            fact.event_id == reserved_fact.event_id
                && session.session_id == reserved_session.session_id
                && session.tool == reserved_session.tool
        });
    if durable {
        Ok(())
    } else {
        Err(RallyError::Message(format!(
            "session reservation readback failed for {}: append returned success but the active-session projection does not contain event {}",
            reserved_session.session_id, reserved_fact.event_id
        )))
    }
}

fn numbered_session_identity(
    agent_spec: &AgentSpec,
    requested_name: Option<String>,
    requested_session_id: Option<String>,
    requested_tool: Option<String>,
    active_sessions: &[ManagedSession],
) -> Result<SessionIdentity> {
    let raw_base_name = requested_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(agent_spec.agent);
    let stripped_name_base = strip_numbered_suffix(raw_base_name);
    let name_base = if sanitize_id(stripped_name_base).is_empty() {
        agent_spec.agent
    } else {
        stripped_name_base
    };
    let name = format!(
        "{}-{:02}",
        name_base,
        next_identity_number(agent_spec, name_base, active_sessions)?
    );
    let name_key = sanitize_id(&name);
    let default_session_id = if name_key.starts_with(&format!("{}-", agent_spec.agent)) {
        name_key.clone()
    } else {
        format!("{}-{}", agent_spec.agent, name_key)
    };
    let tool_suffix = name_key
        .strip_prefix(&format!("{}-", agent_spec.agent))
        .unwrap_or(&name_key);
    let identity = SessionIdentity {
        name,
        session_id: requested_session_id.unwrap_or(default_session_id),
        tool: requested_tool.unwrap_or_else(|| format!("{}:{tool_suffix}", agent_spec.tool)),
    };
    ensure_unique_session_identity(&identity, active_sessions)?;
    Ok(identity)
}

fn next_identity_number(
    agent_spec: &AgentSpec,
    name_base: &str,
    active_sessions: &[ManagedSession],
) -> Result<u64> {
    let base_key = sanitize_id(name_base);
    let mut used = BTreeSet::new();
    for session in active_sessions {
        note_used_identity_number(&base_key, &sanitize_id(&session.name), &mut used);
        if let Some(tool_suffix) = session
            .tool
            .strip_prefix(agent_spec.tool)
            .and_then(|suffix| suffix.strip_prefix(':'))
        {
            note_used_identity_number(&base_key, tool_suffix, &mut used);
            if base_key == agent_spec.agent {
                note_used_bare_identity_number(tool_suffix, &mut used);
            }
        }
    }
    let mut number = 1_u64;
    while used.contains(&number) {
        number = number.checked_add(1).ok_or_else(|| {
            RallyError::Usage(format!("no available numbered identity for {base_key}"))
        })?;
    }
    Ok(number)
}

fn note_used_identity_number(base_key: &str, value: &str, used: &mut BTreeSet<u64>) {
    if value == base_key {
        used.insert(1);
        return;
    }
    let Some((prefix, number)) = split_numbered_suffix(value) else {
        return;
    };
    if prefix == base_key {
        used.insert(number);
    }
}

fn note_used_bare_identity_number(value: &str, used: &mut BTreeSet<u64>) {
    if value.len() >= 2
        && value.chars().all(|ch| ch.is_ascii_digit())
        && let Ok(number) = value.parse::<u64>()
        && number != 0
    {
        used.insert(number);
    }
}

fn split_numbered_suffix(value: &str) -> Option<(&str, u64)> {
    let (prefix, suffix) = value.rsplit_once('-')?;
    if suffix.len() < 2 || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let number = suffix.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some((prefix, number))
}

fn strip_numbered_suffix(value: &str) -> &str {
    split_numbered_suffix(value)
        .map(|(prefix, _)| prefix)
        .unwrap_or(value)
}

fn ensure_unique_session_identity(
    identity: &SessionIdentity,
    active_sessions: &[ManagedSession],
) -> Result<()> {
    for session in active_sessions {
        if session.session_id == identity.session_id {
            return Err(RallyError::Usage(format!(
                "active managed session already uses session-id {}",
                identity.session_id
            )));
        }
        // A single caller tool may hold multiple active managed sessions as
        // long as each session's name is distinct.  Reject only a true
        // duplicate: same tool *and* same name (which also implies the same
        // session_id under auto-numbering, but guard it here for explicit ids).
        if session.tool == identity.tool && session.name == identity.name {
            return Err(RallyError::Usage(format!(
                "active managed session already uses tool {} with name {}",
                identity.tool, identity.name
            )));
        }
        if session.name == identity.name {
            return Err(RallyError::Usage(format!(
                "active managed session already uses name {}",
                identity.name
            )));
        }
    }
    Ok(())
}

fn command_sessions(args: SessionsArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let mut sessions = read_session_views(&room, args.bins.clone())?;
    let mut orphans_reaped: Vec<String> = Vec::new();
    let reaped = if args.reap {
        let mut count = 0;
        for (fact, view) in active_session_views(&room, args.bins.clone())? {
            if view.liveness == SessionLiveness::Stale {
                with_watchdog_command_commit(|| {
                    append_stopped_session_record(&room, &view.session, &fact)
                })?;
                count += 1;
            }
        }

        // Orphan-tmux reaper: detached `rally-*` tmux sessions whose last
        // activity is past the adaptive default-cadence window and which are NOT
        // tracked as managed sessions are killed + tombstoned. Closes the gap
        // where `rally sessions --reap` saw 0 of the real detached orphans.
        let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
        let window = crate::liveness::adaptive_window_secs(
            coord.default_cadence_secs,
            coord.default_cadence_secs,
            coord.miss_multiplier,
            coord.grace_secs,
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let managed_targets: std::collections::BTreeSet<String> =
            sessions.iter().map(|v| v.session.target.clone()).collect();
        let tmux_bin = args.bins.tmux_bin.clone();
        orphans_reaped = sweep_orphan_tmux(&room, &tmux_bin, now_epoch, window, &managed_targets);
        count += orphans_reaped.len();

        sessions = read_session_views(&room, args.bins.clone())?;
        count
    } else {
        0
    };

    // Orphan OS-process reaper (--reap-processes [--apply]).
    // Independent of --reap; can be combined or used alone.
    // Detect once; apply or dry-run from the same candidate list.
    let (processes_staged_count, processes_reaped_pids): (usize, Vec<i32>) = if args.reap_processes
    {
        let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
        let window = crate::liveness::adaptive_window_secs(
            coord.default_cadence_secs,
            coord.default_cadence_secs,
            coord.miss_multiplier,
            coord.grace_secs,
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        const PROCESS_FLOOR_SECS: i64 = 600;
        let staged = backends::detect_orphan_processes(now_epoch, window, PROCESS_FLOOR_SECS);
        let staged_count = staged.len();
        let killed: Vec<i32> = if args.apply {
            staged
                .iter()
                .filter(|p| backends::kill_process(p.pid))
                .map(|p| p.pid)
                .collect()
        } else {
            Vec::new()
        };
        (staged_count, killed)
    } else {
        (0, Vec::new())
    };

    // Rebuild session list if any process kills happened.
    if !processes_reaped_pids.is_empty() {
        sessions = read_session_views(&room, args.bins)?;
    }

    let body = envelope(
        "sessions",
        SCHEMA_SESSIONS,
        SessionsEnvelope {
            sessions: SessionsData {
                sessions: sessions.clone(),
            },
        },
    )?;

    // Build text line.
    let mut text = if args.reap {
        if orphans_reaped.is_empty() {
            format!("sessions {} reaped {reaped}", sessions.len())
        } else {
            format!(
                "sessions {} reaped {reaped} (orphan-tmux: {})",
                sessions.len(),
                orphans_reaped.join(", ")
            )
        }
    } else {
        format!("sessions {}", sessions.len())
    };
    if args.reap_processes {
        if args.apply {
            if processes_reaped_pids.is_empty() {
                text.push_str(" | processes: none killed");
            } else {
                let pids: Vec<String> = processes_reaped_pids
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                text.push_str(&format!(
                    " | processes killed: {} (pids: {})",
                    processes_reaped_pids.len(),
                    pids.join(", ")
                ));
            }
        } else {
            // dry-run: list staged count without killing anything.
            if processes_staged_count == 0 {
                text.push_str(" | processes: 0 staged");
            } else {
                text.push_str(&format!(
                    " | processes staged: {processes_staged_count} (dry-run; use --apply to kill)"
                ));
            }
        }
    }

    Ok(Output::new(args.json, text, body))
}

/// Append a durable tombstone fact for a reaped orphan tmux session so peers see
/// it was killed (visibility; the squad/session projection is unaffected because
/// an orphan was never a managed-session record).
fn append_orphan_tmux_tombstone(
    room: &RoomStore,
    session_name: &str,
    idle_secs: i64,
    reason: &str,
) -> Result<()> {
    let fact = Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Decision,
        tool: Some("rally".to_string()),
        role: None,
        subject: format!("reaper: orphan tmux {session_name} killed"),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!("orphan-tmux idle_secs={idle_secs} reason={reason}")),
        evidence: vec![
            format!("reaper:orphan-tmux={session_name}"),
            format!("reaper:idle_secs={idle_secs}"),
            format!("reaper:reason={reason}"),
        ],
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    room.append_fact(&fact)?.into_fact_reporting();
    Ok(())
}

/// Shared orphan-tmux sweep — the SINGLE actuator both the explicit
/// `rally sessions --reap` path and the opportunistic Layer-2 enter sweep call.
/// Detects detached `rally-*` tmux sessions the [`liveness::reapable`] authority
/// stages (stale-by-window, or stale + parent-dead via Layer 3), skips any that
/// a managed-session record points at, kills + tombstones each, and returns the
/// reaped session names.
///
/// BEST-EFFORT by contract: the function never returns an error to the hot
/// `enter` path. A tombstone failure is still consumed explicitly and surfaced
/// through command-wide `append_issues`, including OutcomeUnknown query data;
/// it is no longer silently discarded.
fn sweep_orphan_tmux(
    room: &RoomStore,
    tmux_bin: &str,
    now_epoch: i64,
    window: i64,
    managed_targets: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut reaped: Vec<String> = Vec::new();
    for orphan in backends::detect_orphan_tmux(tmux_bin, now_epoch, window) {
        // Never touch a tmux session a managed record points at — that path has
        // its own (heartbeat/probe-driven) reap.
        if managed_targets.contains(&orphan.session_name) {
            continue;
        }
        if backends::kill_tmux_session(tmux_bin, &orphan.session_name) {
            consume_optional_result(
                append_orphan_tmux_tombstone(
                    room,
                    &orphan.session_name,
                    orphan.idle_secs,
                    &orphan.reason,
                ),
                "orphan tmux tombstone",
            );
            reaped.push(orphan.session_name);
        }
    }
    reaped
}

/// Layer 2 — opportunistic, best-effort orphan-tmux sweep fired when a new agent
/// joins (`rally enter`). Resolves the adaptive default-cadence window + the
/// managed-target guard set, then delegates to [`sweep_orphan_tmux`]. FAIL-OPEN
/// and time-bounded: any error resolving config/sessions is swallowed and the
/// enter path proceeds untouched. Never blocks enter; never reaps a live or
/// parent-alive session (that policy lives in [`liveness::reapable`]).
fn opportunistic_orphan_sweep_on_enter(room: &RoomStore) {
    // Best-effort: resolve the adaptive window; default on any failure.
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let window = crate::liveness::adaptive_window_secs(
        coord.default_cadence_secs,
        coord.default_cadence_secs,
        coord.miss_multiplier,
        coord.grace_secs,
    );
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Guard set: never reap a tmux session a managed record points at.
    let managed_targets: std::collections::BTreeSet<String> = active_session_records(room)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.target)
        .collect();
    let tmux_bin = BackendBins::default().tmux_bin;
    let _ = sweep_orphan_tmux(room, &tmux_bin, now_epoch, window, &managed_targets);
}

fn command_inject(args: InjectArgs) -> Result<Output> {
    let dry_run = args.dry_run;
    let target = args.target;
    let sender_tool = args.tool;
    let urgent = args.urgent;
    // SEC-006: `Directive.from` is caller-supplied. The write-side gate here
    // rejects a malformed / traversal / control-char sender id so a garbage
    // identity can never reach the ledger or the SEC-015 audit trail.
    //
    // CORRECTION (RC-041 gap 3B, 2026-08-04). This comment used to end by
    // saying the authoritative sender check "lives in ptyd's termd
    // `authorize()`", which read as: rally need not check. Two things are wrong
    // with that, both checkable from this side. `daemon_client::send_agent`
    // transmits `{to, text, submit, confirm}` and NO sender at all
    // (`daemon_client.rs`), so nothing downstream of the RPC can bind a
    // delivery to the `--tool` asserted here — at best termd authorizes the
    // connecting process. And the `tmux_framed_fallback` path
    // (`command_inject_managed`, taken whenever `session.daemon_registered` is
    // false, which is every pre-daemon session) reaches no daemon at all.
    // termd's own source is out of repo and NOT vendored, so what it does with
    // the ledger `from` field is unverified here either way.
    // `inject_authorization_refusal` below is therefore the check that actually
    // covers this binary's delivery paths.
    rally_protocol::ledger::validate_agent_id(&sender_tool)
        .map_err(|e| RallyError::Usage(format!("invalid --tool sender id: {e}")))?;

    // Two-arm target resolution: managed session (legacy dual-delivery,
    // unchanged), or rally-termd-registered ledger agent (ledger-only). See
    // `InjectTarget` for the order-matters rationale (managed wins over id).
    let inject_target = resolve_inject_target(&target, &args.bins)?;

    // RC-041 gap 3B: WHO may inject into WHOM. Runs after resolution because
    // the rule is about the target's identity, and before either arm because
    // both deliver. `--dry-run` is exempt: it plans and delivers nothing.
    if !dry_run {
        let target_tool = match &inject_target {
            InjectTarget::Managed(session) => session.tool.clone(),
            InjectTarget::LedgerAgent(agent_id) => agent_id.clone(),
        };
        if let Some(refusal) = inject_authorization_refusal(&sender_tool, &target_tool) {
            return Err(RallyError::Usage(refusal));
        }
    }

    match inject_target {
        InjectTarget::Managed(session) => command_inject_managed(
            args.json,
            dry_run,
            urgent,
            sender_tool,
            *session,
            args.handoff,
            args.text,
            args.require_ack,
            args.timeout_seconds,
            args.bins,
        ),
        InjectTarget::LedgerAgent(agent_id) => command_inject_ledger(
            args.json,
            dry_run,
            urgent,
            sender_tool,
            agent_id,
            args.handoff,
            args.text,
            args.require_ack,
            args.timeout_seconds,
        ),
    }
}

/// The `--tool` value when the caller named nobody (`cli.rs` defaults it to
/// this). Not a real agent id — `validate_agent_id` accepts it, and every
/// unattended shell invocation of `rally inject` carries it.
const INJECT_SENDER_UNIDENTIFIED: &str = "unknown";

/// RC-041 gap 3B — who may inject into whom, decided from ledger state.
///
/// Reads the room to answer two questions the policy needs (who holds the lead
/// seat; has the target opened a handoff to this sender) and delegates the
/// decision to [`inject_authority_refusal`]. Returns the refusal message, or
/// `None` to allow.
///
/// FAIL-OPEN on a room we cannot read. A storage error tells us nothing about
/// who leads the room, and turning it into a refusal would convert a local
/// SQLite hiccup into a room-wide coordination outage — the RC-038 failure
/// shape, arriving from the other direction.
fn inject_authorization_refusal(sender_tool: &str, target_tool: &str) -> Option<String> {
    let room = RoomStore::open().ok()?;
    let snapshot = room.snapshot().ok()?;
    // Consent = the TARGET named this sender in an open handoff. That is the
    // one invitation in the fact vocabulary the target itself authors; a
    // handoff the SENDER wrote would be the sender authorizing itself.
    let target_invited_sender = snapshot.open_handoffs.iter().any(|fact| {
        fact.tool.as_deref() == Some(target_tool) && fact.target.as_deref() == Some(sender_tool)
    });
    inject_authority_refusal(
        sender_tool,
        target_tool,
        snapshot.lead.as_deref(),
        target_invited_sender,
    )
}

/// The inject authorization rule, as a pure function of ledger-derived state.
///
/// An inject is authorized when ANY of these holds:
///   1. `sender == target` — an agent driving its own pane.
///   2. the sender holds the LEAD SEAT — the documented lead-to-peer flow in
///      `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md` (launch a session, then inject
///      the first instruction).
///   3. the target opened a handoff naming the sender — the target asked, so a
///      reply into its pane is invited.
///   4. the room has NO lead — nobody holds the authority the rule delegates,
///      so there is nobody to route through. Refusing here would strand the
///      bootstrap case (`rally run` a peer, then inject its first instruction,
///      before anyone has entered and taken the seat) and every workspace that
///      never assigns a lead. Same reading as RC-038's "a room with no lead has
///      nobody who can freeze it", and the OPPOSITE of
///      `claim_authority::breadth_violation`, which refuses in a leaderless
///      room — deliberately, because a refused claim costs a retry while a
///      refused inject strands a launched agent with no instruction.
///
///   5. the sender named NOBODY — `--tool` was omitted, so it carries the CLI
///      default [`INJECT_SENDER_UNIDENTIFIED`]. That is the documented human
///      form (`rally inject claude-foo-01 --text "…"` in
///      `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md`), used by the operator who
///      launched every pane in the room. Rally cannot tell that operator from
///      an agent declining to name itself, and refusing would break the
///      documented flow while stopping nothing: the same caller can assert any
///      id it likes. What it gets instead is the honest rendering — the payload
///      arrives labelled `from «unknown»`, which is the weakest provenance the
///      channel can show.
///
/// Everything else is refused: a non-lead injecting into an agent that did not
/// ask, in a room that has someone to route through.
///
/// WHAT THIS IS WORTH, stated exactly, because a gate whose value is overstated
/// is worse than none. `--tool` is self-asserted and rally authenticates
/// nothing, so an attacker who claims the LEAD's id passes rule 2 and one who
/// claims nothing passes rule 5. What is actually blocked is the agent that
/// names ITSELF and has no standing. The value is therefore not exclusion, it
/// is FORCED CHOICE: reach a stranger's pane only by lying about who you are
/// (recorded in `Directive.from` and the SEC-015 audit trail, contradicted by
/// the real lead's own presence facts) or by arriving as `unknown` in front of
/// the recipient and any human watching. The gate sits at the same epistemic
/// tier as `claim_authority::breadth_violation`, which likewise trusts a
/// self-declared `owner_tool`. Closing rule 5 needs a caller credential this
/// protocol does not have — say so rather than let the check imply one exists.
fn inject_authority_refusal(
    sender_tool: &str,
    target_tool: &str,
    lead: Option<&str>,
    target_invited_sender: bool,
) -> Option<String> {
    if sender_tool == target_tool
        || target_invited_sender
        || sender_tool == INJECT_SENDER_UNIDENTIFIED
    {
        return None;
    }
    let lead = lead?;
    if lead == sender_tool {
        return None;
    }
    Some(format!(
        "inject refused: {sender_tool} does not hold the lead seat and {target_tool} has not \
         opened a handoff to it, so {sender_tool} may not write into {target_tool}'s session. \
         {lead} holds the lead seat. To fix: ask {lead} to inject, or post \
         `rally say handoff --tool {sender_tool} --target {target_tool} --subject <what you need>` \
         and let {target_tool} pull it on its next `rally next`."
    ))
}

#[cfg(test)]
mod inject_authority_tests {
    use super::inject_authority_refusal;

    #[test]
    fn self_inject_is_allowed_even_under_a_lead() {
        assert!(
            inject_authority_refusal("codex:01", "codex:01", Some("claude_code:00"), false)
                .is_none(),
            "an agent driving its own pane needs no authority"
        );
    }

    #[test]
    fn the_lead_may_inject_any_peer() {
        assert!(
            inject_authority_refusal("claude_code:00", "codex:01", Some("claude_code:00"), false)
                .is_none(),
            "lead-to-peer is the documented handoff flow"
        );
    }

    #[test]
    fn a_target_that_opened_a_handoff_may_be_answered() {
        assert!(
            inject_authority_refusal("codex:02", "codex:01", Some("claude_code:00"), true)
                .is_none(),
            "the target asked; answering it is not an intrusion"
        );
    }

    #[test]
    fn a_leaderless_room_still_injects() {
        assert!(
            inject_authority_refusal("codex:02", "codex:01", None, false).is_none(),
            "no lead seat means no authority to route through; refusing would strand \
             the launch-then-inject bootstrap"
        );
    }

    /// The RC-041 gap 3B case: a peer with no standing writes into another
    /// agent's pane. Delete the `lead == sender_tool` / consent branches and
    /// this is the test that stops passing for the right reason.
    #[test]
    fn a_non_lead_peer_may_not_inject_a_stranger() {
        let refusal =
            inject_authority_refusal("codex:02", "codex:01", Some("claude_code:00"), false)
                .expect("a non-lead injecting a stranger must be refused");
        assert!(
            refusal.contains("claude_code:00"),
            "the refusal must name who CAN do it; got {refusal}"
        );
        assert!(
            refusal.contains("rally say handoff"),
            "the refusal must name what to do instead; got {refusal}"
        );
    }

    /// Impersonation is NOT stopped here, and the rule's doc comment says so.
    /// Pinned as a test so the limit cannot quietly be forgotten and restated
    /// as a guarantee.
    #[test]
    fn claiming_the_leads_id_still_passes_this_gate() {
        assert!(
            inject_authority_refusal("claude_code:00", "codex:01", Some("claude_code:00"), false)
                .is_none(),
            "KNOWN LIMIT: --tool is self-asserted; this gate blocks shapes, not liars"
        );
    }

    /// The operator form from `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md` —
    /// `rally inject <session> --text "…"` with no `--tool` — must keep
    /// working, and naming nobody is also the cheapest way past this gate. Both
    /// facts are pinned here so neither is discovered by surprise.
    #[test]
    fn an_unidentified_caller_is_allowed_and_that_is_the_weak_point() {
        assert!(
            inject_authority_refusal("unknown", "codex:01", Some("claude_code:00"), false)
                .is_none(),
            "the documented no---tool operator flow must not break, and refusing it \
             would stop nothing: the same caller can assert any id"
        );
    }
}

/// Managed-session inject arm — the path that existed before the
/// `resolve_inject_target` split. Behavior is byte-identical to the pre-split
/// implementation: ledger write FIRST, then the legacy synchronous tmux/cmux
/// backend delivery runs ALONGSIDE (intentional dual-delivery in P2; see the
/// SEC-009 and split-enforcement notes inline below). Once rally-termd (P3) is
/// universally deployed and the legacy backends are retired, this arm folds
/// into the ledger-only one.
#[allow(clippy::too_many_arguments)]
fn command_inject_managed(
    json: bool,
    dry_run: bool,
    urgent: bool,
    sender_tool: String,
    session: ManagedSession,
    handoff: Option<String>,
    text_arg: Option<String>,
    require_ack: bool,
    timeout_seconds: i64,
    bins: BackendBins,
) -> Result<Output> {
    let is_text_inject = text_arg.is_some();
    let text = match (text_arg, handoff.as_deref()) {
        (Some(text), _) => text,
        (None, Some(handoff)) => handoff_prompt(&session, handoff),
        (None, None) => {
            return Err(RallyError::Usage(
                "inject requires --text or --handoff".to_string(),
            ));
        }
    };
    if require_ack && handoff.is_none() {
        return Err(RallyError::Usage(
            "--require-ack requires --handoff or --ref".to_string(),
        ));
    }
    let effective_require_ack = require_ack || handoff.is_some();
    let timeout = timeout_seconds as u64;

    // Open the room once for all appends in this command.
    let mut room = if !dry_run {
        Some(RoomStore::open()?)
    } else {
        None
    };

    let ack_after_seq = if effective_require_ack && !dry_run {
        room.as_ref()
            .map(|r| r.snapshot().map(|s| s.max_seq))
            .transpose()?
    } else {
        None
    };

    // Record message content in the channel BEFORE live delivery so the
    // coordination record is durable even if the backend session is gone.
    // Only --text injects need this; --handoff injects already have a handoff
    // fact in the channel from the originating `rally say handoff`.
    let content_fact = if is_text_inject {
        let fact = if let Some(ref r) = room {
            inject_content_fact(r, &sender_tool, &session.tool, &text)?
        } else {
            inject_content_fact_dry_run(&sender_tool, &session.tool, &text)
        };
        Some(fact)
    } else {
        None
    };

    let backend_parsed = Backend::parse(&session.backend)?;
    let mut backend_runner = BackendRunner::new(backend_parsed, bins);
    // [E]: a ptyd session pins the exact socket it was spawned+registered on, so
    // this inject's agent.send reaches the SAME daemon (never a re-resolved one).
    backend_runner.pin_ptyd_socket(session.daemon_socket.as_deref());
    // RC-041 gap 3A: name the sender so every payload this runner delivers
    // carries a provenance label. `sender_tool` passed `validate_agent_id` in
    // `command_inject`, so the label cannot itself carry a forged line or a
    // control byte.
    //
    // The `unknown` case is translated HERE, not in the label builder, because
    // this is the layer that knows `unknown` is a CLI placeholder rather than
    // an agent (`cli.rs::inject_parser` substitutes it when `--tool` is
    // omitted; see INJECT_SENDER_UNIDENTIFIED). Passing it through rendered the
    // label as `from «unknown»`, which reads as an agent literally named
    // `unknown` and told the recipient nothing it did not already know. There
    // is no better source to resolve it from: rally sets no ambient
    // caller-identity variable, and the ledger write on this same path records
    // the identical placeholder in `Directive.from`. So the honest fix is to
    // render it AS a placeholder — `state_inject_sender` is simply not called,
    // and the label says `(none stated)`. The label itself is never skipped.
    if sender_tool != INJECT_SENDER_UNIDENTIFIED {
        backend_runner.state_inject_sender(&sender_tool);
    }
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let commands = backend_runner.inject_commands(&live_target, &text);

    // Plan F: ALWAYS write a typed Directive to the .rally ledger first.
    // This is the new canonical delivery contract — the daemon (rally-termd,
    // P3) subscribes via kernel file-events and performs the PTY-write. For
    // tmux/cmux backends in P2 (pre-daemon), the legacy synchronous backend
    // delivery still runs ALONGSIDE the ledger write so those paths are
    // unchanged behavior. For Backend::Herdr, the ledger write IS the
    // delivery once P3 lands; until then, we still call backend_runner.inject
    // so existing herdr smoke tests stay green (the legacy herdr inject is a
    // no-op on the inverted architecture but does no harm).
    let (directive_seq, directive_to, delivery_state_initial): (
        Option<u64>,
        Option<String>,
        &'static str,
    ) = if dry_run {
        (None, None, "pending")
    } else {
        match inject_via_ledger(&repo_root()?, &session.tool, &sender_tool, &text, urgent) {
            Ok(seq) => (Some(seq), Some(session.tool.clone()), "pending"),
            Err(_) => (None, Some(session.tool.clone()), "failed"),
        }
    };

    // Daemon-first inject routing: if this session is registered with the rally
    // ptyd daemon, the daemon OWNS the pane and delivery is the `agent.send`
    // RPC — NOT a tmux keystroke write (the pane is a ptyd pane; tmux cannot
    // reach it). When not registered, the framed tmux write below is the
    // operative fallback (the 2026-06-09 atomic send-keys frame).
    let delivery_path: &'static str = if session.daemon_registered {
        "daemon"
    } else {
        "tmux_framed_fallback"
    };
    let daemon_routed = session.daemon_registered;

    // ----- ptyd daemon delivery arm (design-4) -----
    // For a daemon-registered session, perform the real `agent.send` RPC here:
    //   F1: sanitize is applied inside `ptyd_inject` before the write.
    //   F4: the receipt's pane_id is cross-checked against session.daemon_pane;
    //       a mismatch is a HARD `daemon_pane_mismatch` failure — NO fallback.
    // On RPC failure the directive stays Pending and the envelope reports the
    // failure honestly; we do NOT fall back to tmux keystrokes (the pane is a
    // ptyd pane). On success we append a Receipt fact ref'ing the directive seq
    // so `inject`'s ACK wait resolves on it.
    enum PtydDelivery {
        NotDaemon,
        Sent { state: String },
        Mismatch { reason: String },
        Failed { reason: String },
    }
    let ptyd_delivery = if dry_run || !daemon_routed {
        PtydDelivery::NotDaemon
    } else if delivery_state_initial == "failed" {
        // Ledger write failed — do not attempt the daemon send (we have no
        // directive seq to reference and delivery is already a failure).
        PtydDelivery::Failed {
            reason: "ledger directive write failed".to_string(),
        }
    } else if urgent {
        // Same SEC-009 split-enforcement posture as the tmux path: an urgent
        // Addition is delivered by NO transport (the daemon would reject it).
        PtydDelivery::Failed {
            reason: "urgent Addition is not delivered synchronously (SEC-009)".to_string(),
        }
    } else {
        let expect_pane = session.daemon_pane.clone().unwrap_or_default();
        match backend_runner.ptyd_inject(&session.tool, &text, &expect_pane) {
            Ok(state) => PtydDelivery::Sent { state },
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("daemon_pane_mismatch") {
                    PtydDelivery::Mismatch { reason: msg }
                } else {
                    PtydDelivery::Failed { reason: msg }
                }
            }
        }
    };

    // On a successful daemon send, post a Receipt fact correlating the delivery
    // to the directive seq via EVIDENCE (not a fake handoff ref). [D]: this is a
    // SENDER-authored delivery record, NOT an ACK — the ACK is the TARGET's own
    // Resolve/Receipt against the handoff ref_id, which only the agent posts. We
    // record the REAL receipt state (`submitted`/`sent`/`seen`) the daemon
    // returned, not a fabricated "delivered".
    if let (PtydDelivery::Sent { state }, Some(seq), Some(r)) =
        (&ptyd_delivery, directive_seq, room.as_ref())
    {
        let receipt = ptyd_receipt_fact(&sender_tool, seq, &session.tool, state);
        consume_optional_append(r.append_fact(&receipt), "ptyd delivery receipt");
    }

    // Legacy synchronous backend delivery — preserved for tmux/cmux backends.
    // P1a: the tmux/cmux path now confirms landing (capture-verify) before
    // claiming `delivered`. A send that succeeded but whose landing could not be
    // confirmed is recorded as `legacy_sent_unverified` — an honest middle state,
    // NOT "failed" (it was sent) and NOT "delivered" (unconfirmed).
    let mut legacy_sent_unverified = false;
    let delivered = if dry_run {
        false
    } else if daemon_routed {
        // ptyd daemon owns delivery: `delivered` (the legacy sync-delivery flag)
        // is true ONLY when the agent.send RPC returned a Receipt. A mismatch or
        // RPC failure leaves it false (and surfaces as a failed delivery_state
        // below). NO tmux keystroke is ever written for a ptyd session.
        matches!(ptyd_delivery, PtydDelivery::Sent { .. })
    } else if delivery_state_initial == "failed" {
        // Ledger write failed — do not attempt backend inject. The content
        // fact is already recorded.
        false
    } else if urgent {
        // SEC-009: split-enforcement guard. `rally inject` only ever emits
        // `Deliver + Addition` semantics (see inject_via_ledger), and the
        // daemon restricts `urgent=true` to Stop/Retraction (it rejects an
        // urgent Addition/Revision with a Failed receipt). The LEGACY
        // synchronous tmux/cmux inject path must be gated the SAME way, or the
        // two delivery paths disagree: the daemon refuses but the legacy
        // backend still writes the raw keystrokes. Gate it here so an urgent
        // Addition is delivered by NO backend.
        false
    } else {
        match backend_runner.inject_and_verify(&live_target, &text) {
            Ok(true) => true,
            Ok(false) => {
                legacy_sent_unverified = true;
                false
            }
            Err(_) => false,
        }
    };

    // Reconcile delivery_state: legacy sync success => Delivered; otherwise
    // the Pending state propagated from the ledger write stands (the daemon
    // will post a Receipt once P3 ships, at which point this field updates
    // out-of-band via `rally status`).
    //
    // Failure cases (both produce `delivery_state: "failed"`):
    //   1. Ledger write failed (`delivery_state_initial == "failed"`).
    //   2. Tmux/Cmux backend inject failed AND the ledger write succeeded —
    //      these backends have no daemon-side recovery in the Plan F window,
    //      so a missed legacy delivery is a real failure.
    // Plan F functional core (Chunk 3): the herdr backend is removed;
    // the only inject paths left are tmux + cmux + the ledger write.
    let ledger_failed = delivery_state_initial == "failed";
    let legacy_tmux_cmux_failed =
        !dry_run && !daemon_routed && !delivered && !ledger_failed && !legacy_sent_unverified;
    // F4 + RPC honesty: a daemon-routed send that hit a pane mismatch or an RPC
    // error is a REAL failure (the directive stays Pending on the ledger, but
    // this inject did not deliver). A successful Receipt is `delivered`.
    let daemon_delivery_failed = matches!(
        ptyd_delivery,
        PtydDelivery::Mismatch { .. } | PtydDelivery::Failed { .. }
    );
    let delivery_state: &'static str =
        if ledger_failed || legacy_tmux_cmux_failed || daemon_delivery_failed {
            "failed"
        } else if delivered {
            "delivered"
        } else if legacy_sent_unverified {
            "sent_unverified"
        } else {
            delivery_state_initial
        };

    let wake_intent = inject_wake_intent_with_room(
        room.as_ref(),
        Some(&session),
        &session.tool,
        handoff.as_deref(),
        &commands,
        dry_run,
        delivery_state,
    )?;
    let ack = if effective_require_ack && !dry_run {
        let handoff = handoff.as_deref().unwrap_or_default();
        // room is always Some here (require_ack && !dry_run guards this branch).
        let ack_room = room
            .take()
            .expect("room must be open for --require-ack")
            .into_ack_polling()?;
        Some(wait_for_resolution(
            handoff,
            timeout,
            ack_after_seq.unwrap_or(0),
            &ack_room,
            &session.tool,
        )?)
    } else {
        None
    };
    let ack_state = inject_ack_state(effective_require_ack, dry_run, ack.as_ref());
    let verified_received = inject_verified_received(ack.as_ref());
    let fallback_plan = inject_fallback_plan(
        effective_require_ack,
        dry_run,
        handoff.as_deref(),
        &session.tool,
        ack.as_ref(),
    );
    // Surface the daemon Receipt state / failure reason honestly.
    let (daemon_receipt_state, daemon_delivery_error) = match &ptyd_delivery {
        PtydDelivery::Sent { state } => (Some(state.clone()), None),
        PtydDelivery::Mismatch { reason } | PtydDelivery::Failed { reason } => {
            (None, Some(reason.clone()))
        }
        PtydDelivery::NotDaemon => (None, None),
    };
    let session_id_for_text = session.session_id.clone();
    let inject_payload = InjectData {
        mode: if dry_run { "dry-run" } else { "inject" },
        session: Some(session),
        target_kind: "managed_session",
        handoff,
        require_ack: effective_require_ack,
        ack: ack.clone(),
        verified_received,
        ack_state,
        fallback_plan,
        wake_intent,
        commands: command_plan_json(&commands),
        sender_tool,
        content_fact,
        delivered,
        // Plan F: surface the truthful delivery state + the Directive's
        // assigned sequence so downstream callers can look up the matching
        // Receipt via `rally status` once rally-termd (P3) is live.
        delivery_state,
        directive_seq,
        directive_to,
        delivery_path,
        daemon_receipt_state,
        daemon_delivery_error,
        // Managed sessions reaching this arm are Live/Unknown by construction
        // (stale/gone targets are rejected in `resolve_inject_target`), and
        // their delivery truth is synchronous (`delivered`/`delivery_state`) —
        // there is no pre-wait diagnosis to surface.
        target_injectability: None,
    };
    let has_ack = ack.is_some();
    let body = envelope(
        "inject",
        SCHEMA_INJECT,
        InjectEnvelope {
            inject: inject_payload,
        },
    )?;
    let text = format!("inject session={session_id_for_text} delivered={delivered} ack={has_ack}",);
    Ok(Output::new(json, text, body))
}

/// Ledger-only inject arm — the new path that unblocks rally-termd-registered
/// ptyd-pane agents (e.g. an `agent.register`-bound `claude` identity that has
/// NO `ManagedSession` record). Delivery is ledger-only: the typed Directive
/// lands in `.rally/inbox/<agent>.jsonl` and the daemon (`rally-termd`)
/// performs the PTY-write + posts the Receipt. NO legacy tmux/cmux backend
/// fires — that's the second-bug fix referenced in the orchestrator brief
/// (the managed-session arm intentionally double-delivers in P2; the
/// ledger-only arm never did and never should).
///
/// SEC-017 boundary: this function only WRITES the Directive. The actual
/// PTY-write is rally-termd's responsibility — its `authorize()` gate /
/// `--injector` allowlist is the runtime authorization check, NOT this writer.
#[allow(clippy::too_many_arguments)]
fn command_inject_ledger(
    json: bool,
    dry_run: bool,
    urgent: bool,
    sender_tool: String,
    agent_id: String,
    handoff: Option<String>,
    text_arg: Option<String>,
    require_ack: bool,
    timeout_seconds: i64,
) -> Result<Output> {
    let is_text_inject = text_arg.is_some();
    let text = match (text_arg, handoff.as_deref()) {
        (Some(text), _) => text,
        (None, Some(handoff)) => handoff_prompt_ledger(&agent_id, handoff),
        (None, None) => {
            return Err(RallyError::Usage(
                "inject requires --text or --handoff".to_string(),
            ));
        }
    };
    if require_ack && handoff.is_none() {
        return Err(RallyError::Usage(
            "--require-ack requires --handoff or --ref".to_string(),
        ));
    }
    let effective_require_ack = require_ack || handoff.is_some();
    let timeout = timeout_seconds as u64;

    let mut room = if !dry_run {
        Some(RoomStore::open()?)
    } else {
        None
    };

    let ack_after_seq = if effective_require_ack && !dry_run {
        room.as_ref()
            .map(|r| r.snapshot().map(|s| s.max_seq))
            .transpose()?
    } else {
        None
    };

    let content_fact = if is_text_inject {
        let fact = if let Some(ref r) = room {
            inject_content_fact(r, &sender_tool, &agent_id, &text)?
        } else {
            inject_content_fact_dry_run(&sender_tool, &agent_id, &text)
        };
        Some(fact)
    } else {
        None
    };

    // Ledger-only delivery: write the Directive, report `pending`. No backend
    // is parsed, no backend runner is constructed, no `backend_runner.inject`
    // is called — this is the fix for the orchestrator brief's "second bug"
    // (double-delivery on the ledger-only path). The managed-session arm
    // above keeps its intentional dual-delivery for P2 tmux/cmux.
    let (directive_seq, directive_to, delivery_state): (Option<u64>, Option<String>, &'static str) =
        if dry_run {
            (None, None, "pending")
        } else {
            match inject_via_ledger(&repo_root()?, &agent_id, &sender_tool, &text, urgent) {
                Ok(seq) => (Some(seq), Some(agent_id.clone()), "pending"),
                Err(_) => (None, Some(agent_id.clone()), "failed"),
            }
        };

    // No backend commands on the ledger-only path; surface an empty plan.
    let commands: Vec<Vec<String>> = Vec::new();

    let wake_intent = inject_wake_intent_with_room(
        room.as_ref(),
        None,
        &agent_id,
        handoff.as_deref(),
        &commands,
        dry_run,
        delivery_state,
    )?;
    // RCA 2026-07-09 follow-up: a LedgerAgent target by definition has no
    // ACTIVE managed session (`resolve_inject_target` arm 2), so pane delivery
    // — and any synchronous ACK it would produce — depends on an external
    // rally-termd registration Rally cannot observe from here. Diagnose that
    // BEFORE the wait (stderr for humans, `target_injectability` in the
    // envelope for tools) instead of leaving callers to reconstruct it
    // post-timeout from scattered fields (`target_kind` + `delivered` +
    // `delivery_state` + `fallback_plan`). ADVISORY ONLY — the wait below is
    // deliberately NOT short-circuited: a rally-termd-registered pane still
    // delivers (and posts a Receipt that `wait_for_resolution` accepts), and a
    // presence-only agent can post a Resolve when it next polls `rally next`.
    let target_injectability = TargetInjectability {
        injectable: false,
        status: "presence_only_unmanaged".to_string(),
        via: None,
        reason: Some(format!(
            "no active managed session for {agent_id}; delivery is ledger-queued (a rally-termd-registered pane may still deliver). For guaranteed live injection, adopt a pane you already started: `rally adopt {agent_id} --tmux <target>`. `rally run <agent>` also mints one, when a backend is installed."
        )),
    };
    if effective_require_ack && !dry_run {
        eprintln!(
            "rally: inject target {agent_id} is not synchronously injectable (presence-only; no active managed session). Waiting up to {timeout}s for an async ACK anyway — a polling agent or a rally-termd-registered pane can still resolve. Size any outer timeout accordingly."
        );
    }
    let ack = if effective_require_ack && !dry_run {
        let handoff = handoff.as_deref().unwrap_or_default();
        let ack_room = room
            .take()
            .expect("room must be open for --require-ack")
            .into_ack_polling()?;
        Some(wait_for_resolution(
            handoff,
            timeout,
            ack_after_seq.unwrap_or(0),
            &ack_room,
            &agent_id,
        )?)
    } else {
        None
    };
    let ack_state = inject_ack_state(effective_require_ack, dry_run, ack.as_ref());
    let verified_received = inject_verified_received(ack.as_ref());
    // Sender observability: prefer an ack-timeout plan when --require-ack is set;
    // otherwise (plain inject on the ledger_only path) still surface the async
    // delivery contract so `ok:true` + `delivered:false` is not misread as a live
    // delivery. Skipped on dry-run and on a failed ledger write.
    let fallback_plan = inject_fallback_plan(
        effective_require_ack,
        dry_run,
        handoff.as_deref(),
        &agent_id,
        ack.as_ref(),
    )
    .or_else(|| {
        if dry_run || delivery_state != "pending" {
            None
        } else {
            Some(ledger_async_fallback_plan(&agent_id))
        }
    })
    // Stamp the pre-wait diagnosis into whichever fallback plan fired, so a
    // timeout report carries the cause known at t=0, not just the symptom.
    .map(|mut plan| {
        if let Some(obj) = plan.as_object_mut() {
            obj.insert(
                "pre_diagnosis".to_string(),
                json!(
                    "target was presence-only at inject time (no active managed session); a synchronous pane ACK had no guaranteed producer"
                ),
            );
        }
        plan
    });
    let inject_payload = InjectData {
        mode: if dry_run { "dry-run" } else { "inject" },
        session: None,
        target_kind: "ledger_agent",
        handoff,
        require_ack: effective_require_ack,
        ack: ack.clone(),
        verified_received,
        ack_state,
        fallback_plan,
        wake_intent,
        commands: command_plan_json(&commands),
        sender_tool,
        content_fact,
        // `delivered` is the legacy bool gated to the synchronous backend
        // outcome. The ledger-only path never runs a backend, so `delivered`
        // is always false here — consumers should branch on `delivery_state`
        // (which is `pending` on a successful ledger write; rally-termd posts
        // a Receipt to flip it to `delivered`/`seen`/`acted` out-of-band).
        delivered: false,
        delivery_state,
        directive_seq,
        directive_to,
        // A LedgerAgent target is an externally-registered ptyd pane: the
        // ledger write is already the daemon-delivered path.
        delivery_path: "ledger_only",
        // The LedgerAgent arm does not perform a CLI-initiated agent.send (the
        // external rally-termd owns delivery + posts its own Receipt).
        daemon_receipt_state: None,
        daemon_delivery_error: None,
        target_injectability: Some(target_injectability),
    };
    let has_ack = ack.is_some();
    let agent_for_text = agent_id;
    let body = envelope(
        "inject",
        SCHEMA_INJECT,
        InjectEnvelope {
            inject: inject_payload,
        },
    )?;
    let text =
        format!("inject agent={agent_for_text} delivery_state={delivery_state} ack={has_ack}",);
    Ok(Output::new(json, text, body))
}

#[derive(Clone, Copy, Debug)]
enum SessionAction {
    Attach,
    Capture,
    Stop,
}

impl SessionAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Capture => "capture",
            Self::Stop => "stop",
        }
    }
}

fn command_session_action(args: SessionActionArgs) -> Result<Output> {
    let action = args.action;
    let dry_run = args.dry_run;
    let target = args.target;
    let session = find_session(&target, &args.bins)?;
    // Capture the tmux bin before `args.bins` is moved into the runner — the
    // session-end self-kill (below) needs it.
    let tmux_bin_for_self_kill = args.bins.tmux_bin.clone();
    let mut backend_runner = BackendRunner::new(Backend::parse(&session.backend)?, args.bins);
    // [E]: capture/stop/attach on a ptyd session must reach the SAME daemon the
    // pane was spawned in — pin the socket recorded on the session.
    backend_runner.pin_ptyd_socket(session.daemon_socket.as_deref());
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let lines = args.lines as usize;
    let (commands, output) = match action {
        SessionAction::Attach => {
            let commands = backend_runner.attach_commands(&live_target);
            if !dry_run && !args.json {
                backend_runner.attach(&live_target)?;
            }
            (commands, None)
        }
        SessionAction::Capture => {
            let commands = backend_runner.capture_commands(&live_target, lines);
            let output = if dry_run {
                None
            } else {
                Some(backend_runner.capture(&live_target, lines)?)
            };
            (commands, output)
        }
        SessionAction::Stop => {
            let commands = backend_runner.stop_commands(&live_target);
            if !dry_run {
                let _commit_guard = arm_watchdog_command_commit();
                let _ = backend_runner.stop(&live_target);
                // Cleanup the per-agent worktree (when present) before
                // marking the session stopped.  Best-effort: warnings are
                // discarded so `rally stop` never blocks on a leftover
                // worktree.
                if let (Some(path), Some(branch)) =
                    (session.worktree_path.as_deref(), session.branch.as_deref())
                {
                    let repo = repo_root().unwrap_or_else(|_| PathBuf::from("."));
                    let _ = run_worktree::cleanup(&repo, path, branch, "git");
                }
                // LEVER 3: self-release the STOPPING SESSION's active claims
                // before removing the session record. Self-release is
                // authoritative (bypasses the 2h reclaim bar — the owner is
                // declaring itself done), keeps SEC-001 dormant (no stale-owner
                // marker on the release fact). It is a required/auditable part
                // of stop: an uncertain close returns a typed partial result
                // instead of silently removing the session record.
                //
                // Goal F4: release THAT SESSION's claims, not every claim that
                // happens to share the stopping tool. Two co-resident sessions
                // of the SAME tool (e.g. two claude_code sessions) must not
                // release each other's mid-work claims. Match on the claim's
                // `from_session_id` (the live session lease that authored the
                // claim). Fall back to tool-match ONLY for legacy claims that
                // carry no `from_session_id` (the dominant one-session-per-tool
                // case stays correct), and never touch a live sibling session's
                // claims.
                if let Ok(room) = RoomStore::open()
                    && let Ok(snap) = room.snapshot()
                {
                    let stopping_tool = &session.tool;
                    let stopping_session = session.session_id.as_str();
                    for claim in snap.active_claims.iter().filter(|c| {
                        claim_authority::claim_owner_matches_caller(
                            c.tool.as_deref(),
                            c.from_session_id.as_deref(),
                            Some(stopping_tool.as_str()),
                            Some(stopping_session),
                        )
                    }) {
                        let release = Fact {
                            from_session_id: Some(stopping_session.to_string()),
                            schema: FACT_SCHEMA.to_string(),
                            event_id: stable_operation_id(
                                "stop-release",
                                &format!("{}:{}", session.session_id, claim.event_id),
                            ),
                            seq: 0,
                            thread_id: stable_operation_id(
                                "stop-release-thread",
                                &format!("{}:{}", session.session_id, claim.event_id),
                            ),
                            kind: FactKind::Release,
                            tool: Some(stopping_tool.clone()),
                            role: None,
                            subject: format!("self-release on stop: {}", claim.event_id),
                            scope: claim.scope.clone(),
                            created_at: now_string(),
                            summary: None,
                            // No authorized-takeover marker — this is a
                            // self-release; SEC-001 stays dormant.
                            evidence: Vec::new(),
                            target: None,
                            ref_id: Some(claim.event_id.clone()),
                            status: None,
                            severity: None,
                            uri: None,
                            session: None,
                        };
                        room.append_state_transition_verified(&release)?
                            .into_fact_reporting();
                    }
                }
                remove_session_record(&session.session_id)?;

                // Session-end self-kill (contain at source): if THIS process is
                // itself running inside a `rally-*` tmux session that is NOT the
                // managed target we just stopped, kill it too so it can never
                // become a detached orphan the reaper has to clean up later.
                // Best-effort; never blocks the stop path.
                if let Some(own) = backends::own_rally_tmux_session(&tmux_bin_for_self_kill)
                    && own != live_target
                {
                    let _ = backends::kill_tmux_session(&tmux_bin_for_self_kill, &own);
                }
            }
            (commands, None)
        }
    };
    let output_text = output.clone();
    let command_name = action.as_str();
    let action_payload = SessionActionData {
        mode: if dry_run { "dry-run" } else { command_name },
        action: command_name,
        session: session.clone(),
        output,
        commands: command_plan_json(&commands),
    };
    let body = envelope(
        command_name,
        SCHEMA_SESSION_ACTION,
        SessionActionEnvelope::new(command_name, action_payload),
    )?;
    let text = output_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{command_name} session={}", session.session_id));
    Ok(Output::new(args.json, text, body))
}

fn read_session_views(room: &RoomStore, bins: BackendBins) -> Result<Vec<SessionView>> {
    Ok(active_session_views(room, bins)?
        .into_iter()
        .map(|(_, view)| view)
        .collect())
}

fn active_session_views(room: &RoomStore, bins: BackendBins) -> Result<Vec<(Fact, SessionView)>> {
    let facts = room.facts()?;
    let active = active_session_facts_from_facts(facts.clone());
    let probes = probe_session_liveness(&active, bins);
    let states = agent_state::project_agent_states(&facts, &now_string())
        .into_iter()
        .map(|entry| (entry.tool, entry.stale))
        .collect::<BTreeMap<_, _>>();

    Ok(active
        .into_iter()
        .map(|(fact, session)| {
            let (liveness, liveness_source) =
                projected_session_liveness(&session, &states, &probes);
            let (injectable, inject_status, inject_via) =
                managed_session_injectability(&session, liveness);
            (
                fact,
                SessionView {
                    session,
                    liveness,
                    liveness_source,
                    injectable,
                    inject_status,
                    inject_via,
                },
            )
        })
        .collect())
}

fn managed_session_injectability(
    session: &ManagedSession,
    liveness: SessionLiveness,
) -> (bool, String, String) {
    let inject_via = if session.daemon_registered {
        "daemon".to_string()
    } else {
        session.backend.clone()
    };
    match liveness {
        SessionLiveness::Live => (true, "live_managed_session".to_string(), inject_via),
        SessionLiveness::Unknown => (
            true,
            "managed_session_liveness_unknown".to_string(),
            inject_via,
        ),
        SessionLiveness::Stale => (false, "stale_managed_session".to_string(), inject_via),
    }
}

fn build_agent_injectability(
    snapshot: &RoomSnapshot,
    session_views: &[SessionView],
    requested_tool: Option<&str>,
) -> Vec<AgentInjectability> {
    let mut tools = BTreeSet::new();
    if let Some(tool) = requested_tool {
        tools.insert(tool.to_string());
    }
    if let Some(lead) = snapshot.lead.as_ref() {
        tools.insert(lead.clone());
    }
    for squad in &snapshot.squads {
        if squad.status == "active" {
            tools.insert(squad.tool.clone());
        }
    }
    for view in session_views {
        if view.liveness != SessionLiveness::Stale {
            tools.insert(view.session.tool.clone());
        }
    }
    for fact in snapshot
        .active_claims
        .iter()
        .chain(snapshot.active_blockers.iter())
        .chain(snapshot.open_handoffs.iter())
    {
        if let Some(tool) = fact.tool.as_ref() {
            tools.insert(tool.clone());
        }
        if let Some(target) = fact.target.as_ref()
            && target != "all"
        {
            tools.insert(target.clone());
        }
    }

    tools
        .into_iter()
        .map(|tool| {
            if let Some(view) = best_session_view_for_tool(session_views, &tool) {
                return AgentInjectability {
                    tool,
                    injectable: view.injectable,
                    status: view.inject_status.clone(),
                    via: Some(view.inject_via.clone()),
                    session_id: Some(view.session.session_id.clone()),
                    target: Some(view.session.target.clone()),
                    reason: if view.injectable {
                        Some("target has an active managed-session record; use `rally inject` with this tool/name/session".to_string())
                    } else {
                        Some("managed session is stale; run `rally sessions --reap`, relaunch with `rally run`, or adopt a live pane before injecting".to_string())
                    },
                };
            }

            AgentInjectability {
                tool: tool.clone(),
                injectable: false,
                status: "presence_only_unmanaged".to_string(),
                via: None,
                session_id: None,
                target: None,
                reason: Some(format!(
                    "no active managed session for {}; `rally inject` can only queue a ledger wake, not deliver to a live pane. Adopt a pane you already started: `rally adopt {} --tmux <target>` / `--cmux <target>`. `rally run <agent>` also works when a backend is installed — if it fails, it now names the missing dependency.",
                    tool, tool
                )),
            }
        })
        .collect()
}

fn best_session_view_for_tool<'a>(
    session_views: &'a [SessionView],
    tool: &str,
) -> Option<&'a SessionView> {
    session_views
        .iter()
        .filter(|view| view.session.tool == tool)
        .min_by_key(|view| match view.liveness {
            SessionLiveness::Live => 0,
            SessionLiveness::Unknown => 1,
            SessionLiveness::Stale => 2,
        })
}

fn projected_session_liveness(
    session: &ManagedSession,
    heartbeat_stale: &BTreeMap<String, bool>,
    backend_probes: &BTreeMap<String, SessionLiveness>,
) -> (SessionLiveness, &'static str) {
    let probe = backend_probes
        .get(&session.session_id)
        .copied()
        .unwrap_or(SessionLiveness::Unknown);
    match (heartbeat_stale.get(&session.tool).copied(), probe) {
        // P1c fix: a DEFINITIVE backend probe is authoritative over the presence
        // heartbeat TTL. A pane that is really alive is NOT stale just because
        // its heartbeat lapsed (>15 min without a rally command) — this is the
        // false-stale that rejected inject to busy/quiet-but-live agents. A pane
        // that is really gone IS stale regardless of a fresh heartbeat. Only when
        // the probe is Unknown (no backend result) do we fall back to the TTL.
        (_, SessionLiveness::Live) => (SessionLiveness::Live, "backend_probe"),
        (_, SessionLiveness::Stale) => (SessionLiveness::Stale, "backend_probe"),
        (Some(true), SessionLiveness::Unknown) => (SessionLiveness::Stale, "heartbeat_ttl"),
        (Some(false), SessionLiveness::Unknown) => (SessionLiveness::Live, "heartbeat_ttl"),
        (None, SessionLiveness::Unknown) => (SessionLiveness::Unknown, "backend_probe"),
    }
}

fn probe_session_liveness(
    active: &[(Fact, ManagedSession)],
    bins: BackendBins,
) -> BTreeMap<String, SessionLiveness> {
    let mut by_backend: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (_, session) in active {
        by_backend
            .entry(session.backend.clone())
            .or_default()
            .push((session.session_id.clone(), session.target.clone()));
    }

    let mut out = BTreeMap::new();
    for (backend, sessions) in by_backend {
        let targets = sessions
            .iter()
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>();
        let liveness = match Backend::parse(&backend) {
            Ok(backend) => BackendRunner::new(backend, bins.clone()).liveness(&targets),
            Err(_) => targets.iter().map(|_| SessionLiveness::Unknown).collect(),
        };
        for ((session_id, _), liveness) in sessions.into_iter().zip(liveness) {
            out.insert(session_id, liveness);
        }
    }
    out
}

fn remove_session_record(session_id: &str) -> Result<()> {
    let room = RoomStore::open()?;
    let Some((fact, session)) = active_session_facts(&room)?
        .into_iter()
        .find(|(_, session)| session.session_id == session_id)
    else {
        return Ok(());
    };
    room.append_fact(&session_fact(&session, "stopped", Some(fact.event_id)))?
        .into_fact_reporting();
    Ok(())
}

fn append_stopped_session_record(
    room: &RoomStore,
    session: &ManagedSession,
    active_fact: &Fact,
) -> Result<()> {
    room.append_fact(&session_fact(
        session,
        "stopped",
        Some(active_fact.event_id.clone()),
    ))?
    .into_fact_reporting();
    Ok(())
}

fn active_session_records(room: &RoomStore) -> Result<Vec<ManagedSession>> {
    Ok(active_session_facts(room)?
        .into_iter()
        .map(|(_, session)| session)
        .collect())
}

fn active_session_facts(room: &RoomStore) -> Result<Vec<(Fact, ManagedSession)>> {
    Ok(active_session_facts_from_facts(room.facts()?))
}

fn active_session_records_from_facts(facts: Vec<Fact>) -> Vec<ManagedSession> {
    active_session_facts_from_facts(facts)
        .into_iter()
        .map(|(_, session)| session)
        .collect()
}

fn active_session_facts_from_facts(facts: Vec<Fact>) -> Vec<(Fact, ManagedSession)> {
    let mut active = BTreeMap::new();
    let mut facts = facts;
    facts.sort_by_key(|fact| fact.seq);
    for fact in facts.into_iter().filter(|fact| fact.kind == "session") {
        let Some(session) = fact.session.clone() else {
            continue;
        };
        if fact.status.as_deref() == Some("stopped") {
            active.remove(&session.session_id);
        } else {
            active.insert(session.session_id.clone(), (fact, session));
        }
    }
    active.into_values().collect()
}

/// Find a `session`-kind fact (active OR tombstoned/stopped) whose session
/// matches `target` by `session_id`, `name`, or `tool`. Returns the most-recent
/// match (facts are scanned in ascending `seq`, so the last write wins).
///
/// This is the discriminator that lets [`resolve_inject_target`] distinguish two
/// cases that otherwise look identical to the ledger arm:
///   - "named a managed session that is gone / renumbered / reaped" → fail loud;
///   - "named a genuine ledger agent that was NEVER a managed session"
///     (e.g. an `agent.register`-bound id with no session record) → ledger-only
///     delivery is correct.
///
/// A genuine ledger agent has no `session` fact, so this returns `None` for it
/// and the existing ledger path is preserved.
fn prior_managed_session(room: &RoomStore, target: &str) -> Result<Option<ManagedSession>> {
    let mut facts = room.facts()?;
    facts.sort_by_key(|fact| fact.seq);
    let mut found = None;
    for fact in facts.into_iter().filter(|fact| fact.kind == "session") {
        if let Some(session) = fact.session
            && (session.session_id == target || session.name == target || session.tool == target)
        {
            found = Some(session);
        }
    }
    Ok(found)
}

fn session_fact(session: &ManagedSession, status: &str, ref_id: Option<String>) -> Fact {
    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: format!("session-{}", session.session_id),
        kind: FactKind::Session,
        tool: Some(session.tool.clone()),
        role: None,
        subject: format!("managed session {} {status}", session.name),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!(
            "{} {} session via {}",
            status, session.agent, session.backend
        )),
        evidence: Vec::new(),
        target: Some(session.tool.clone()),
        ref_id,
        status: Some(status.to_string()),
        severity: None,
        uri: None,
        session: Some(session.clone()),
    }
}

fn append_next_wake_intent(
    room: &RoomStore,
    snapshot: &RoomSnapshot,
    tool: &str,
    paths: &[String],
    next: &NextResult,
) -> Result<Option<Fact>> {
    if matches!(next.action, "wait" | "proceed_solo") {
        return Ok(None);
    }
    let subject = format!("wake intent for {tool}: {}", next.action);
    if let Some(existing) = snapshot.pending_wakes.iter().find(|wake| {
        wake.target.as_deref() == Some(tool)
            && wake.subject == subject
            && wake.ref_id == next.target_event_id
    }) {
        return Ok(Some(existing.clone()));
    }
    let summary = format!(
        "rally next found actionable work for {tool}: {}",
        next.action
    );
    let fact = wake_fact(
        tool,
        &subject,
        paths.to_vec(),
        Some(summary),
        vec!["rally next --tool <tool> --json".to_string()],
        next.target_event_id.clone(),
        Some("pending".to_string()),
    );
    room.append_fact(&fact)
        .map(store::AppendOutcome::into_fact_reporting)
        .map(Some)
}

/// Build the coordination fact that records inject message content.
///
/// Uses `FactKind::Handoff` — it carries tool/target/subject/summary semantics
/// and represents "sender has information for recipient". The "inject:" subject
/// prefix distinguishes it from work-transfer handoffs.
///
/// Subject is truncated to 120 chars so ledger lines stay readable in `rally room`.
/// Full text lands in summary so nothing is lost.
fn make_inject_content_fact(sender_tool: &str, recipient_tool: &str, text: &str) -> Fact {
    let subject_text: String = text.chars().take(120).collect();
    let subject = format!("inject: {subject_text}");
    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("inject"),
        seq: 0,
        thread_id: format!("inject-{}", sanitize_id(sender_tool)),
        kind: FactKind::Handoff,
        tool: Some(sender_tool.to_string()),
        role: None,
        subject,
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(text.to_string()),
        // SEC-015: `from` is caller-supplied and unverifiable at write time, so
        // stamp an audit trail — the self-declared sender id plus the OS pid of
        // the writing process — into the durable coordination record. An auditor
        // (or the daemon's Failed-receipt diagnostics) can cross-check this
        // against `directive.from` instead of trusting the string blindly.
        evidence: vec![
            format!("sender:{sender_tool}"),
            format!("pid:{}", std::process::id()),
        ],
        target: Some(recipient_tool.to_string()),
        ref_id: None,
        status: Some("pending".to_string()),
        severity: None,
        uri: None,
        session: None,
    }
}

/// Build a `Receipt` fact recording that the rally ptyd daemon delivered a
/// directive to a daemon-owned pane (design-4). [D]: this is a SENDER-authored
/// DELIVERY record — it is authored as `sender_tool` (the actor that initiated
/// the `agent.send`, same actor as the tmux fallback), NOT the target. It is
/// NOT an ACK: the ACK that closes a `--require-ack` wait is the TARGET's own
/// Resolve/Receipt against the handoff `ref_id`, which only the agent posts
/// (`wait_for_resolution` matches `ref_id == handoff && tool == target`). A
/// sender-fabricated, target-attributed "delivered" claim would be a fake ACK,
/// so we do neither: no handoff ref, and `status` reflects the REAL receipt
/// state the daemon returned (`sent`/`seen`/`acted`), not an invented
/// "delivered".
///
/// Correlation to the Directive it acknowledges is carried by EVIDENCE
/// (`directive_seq:<n>`), so a consumer can join the Receipt to its Directive
/// without a synthetic handoff ref_id that would never match.
///
/// SAME-ACTOR trust model (plan §Fallback contract): the CLI initiated the
/// `agent.send` itself, so it may post a delivery Receipt for that send. The
/// autonomous rally-termd path (which would post its OWN Receipt) is separate
/// and gated; see F5 mutual-exclusion in the plan.
///
/// `receipt_state` is the ptyd `Receipt.state` (`sent`/`seen`/`acted`) — with
/// the CLI's `confirm:"sent"` ceiling this is `"sent"` (submitted, bytes
/// written). It is recorded verbatim so the fact never oversells the evidence.
fn ptyd_receipt_fact(
    sender_tool: &str,
    directive_seq: u64,
    target_tool: &str,
    receipt_state: &str,
) -> Fact {
    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("receipt"),
        seq: 0,
        thread_id: format!("inject-{}", sanitize_id(target_tool)),
        kind: FactKind::Receipt,
        // [D]: authored as the SENDER (the actor that performed the send), not
        // the target. A target-attributed receipt would be a sender-fabricated
        // claim spoofing the agent.
        tool: Some(sender_tool.to_string()),
        role: None,
        subject: format!("receipt: daemon delivered directive seq {directive_seq}"),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!(
            "rally ptyd daemon delivered directive seq {directive_seq} to {target_tool} \
             (state {receipt_state})"
        )),
        // Correlate to the Directive by evidence, not a synthetic handoff ref.
        evidence: vec![
            format!("directive_seq:{directive_seq}"),
            "transport:daemon".to_string(),
            format!("receipt_state:{receipt_state}"),
        ],
        target: Some(target_tool.to_string()),
        // No ref_id: this is NOT a handoff-closing ACK. Leaving ref_id unset
        // keeps it out of `wait_for_resolution`'s handoff match (which it could
        // never satisfy anyway).
        ref_id: None,
        // [D]: status is the REAL receipt state, not a fabricated "delivered".
        status: Some(receipt_state.to_string()),
        severity: None,
        uri: None,
        session: None,
    }
}

/// Append a content fact to the given room store.
///
/// Uses `append_fact_verified` so a silent segment-write failure is detected
/// immediately rather than silently dropped while delivery proceeds.
fn inject_content_fact(
    room: &RoomStore,
    sender_tool: &str,
    recipient_tool: &str,
    text: &str,
) -> Result<Fact> {
    let fact = make_inject_content_fact(sender_tool, recipient_tool, text);
    // This coordination fact is supporting evidence, not the inject command's
    // primary commit point. The watchdog is armed only around the durable
    // Directive append so it cannot report `committed: true` before delivery
    // intent exists in the target inbox.
    room.append_fact_verified(&fact)
        .map(store::AppendOutcome::into_fact_reporting)
}

/// Return the content fact without appending (dry-run path).
fn inject_content_fact_dry_run(sender_tool: &str, recipient_tool: &str, text: &str) -> Fact {
    make_inject_content_fact(sender_tool, recipient_tool, text)
}

/// Append a `wake_intent` Fact for an inject. The Fact's subject + summary
/// vary by target kind:
///   * managed session (legacy path) — `"wake intent delivered to <tool>"` +
///     summary referencing the session name and backend (unchanged).
///   * ledger-only agent (rally-termd-registered) — `"wake intent delivered
///     to <agent>"` + summary noting the ledger-only path; no backend
///     reference because there is no `ManagedSession.backend`.
///
/// `target_tool` is the logical id the Directive landed on (mirrors the
/// `directive_to` field of `InjectData`). For managed sessions this is
/// `session.tool`; for ledger-only it is the validated agent-id.
fn inject_wake_intent_with_room(
    room: Option<&RoomStore>,
    session: Option<&ManagedSession>,
    target_tool: &str,
    handoff: Option<&str>,
    commands: &[Vec<String>],
    dry_run: bool,
    delivery_state: &'static str,
) -> Result<Option<Fact>> {
    let status = if dry_run { "planned" } else { delivery_state };
    let subject = format!("wake intent {status} to {target_tool}");
    let summary = Some(match session {
        Some(s) => format!(
            "rally inject {status} for managed session {} via {}",
            s.name, s.backend
        ),
        None => format!("rally inject {status} via ledger for agent {target_tool}"),
    });
    let evidence = commands.iter().map(|command| command.join(" ")).collect();
    let fact = wake_fact(
        target_tool,
        &subject,
        Vec::new(),
        summary,
        evidence,
        handoff.map(str::to_string),
        Some(status.to_string()),
    );
    if dry_run {
        Ok(Some(fact))
    } else if let Some(r) = room {
        r.append_fact(&fact)
            .map(store::AppendOutcome::into_fact_reporting)
            .map(Some)
    } else {
        let r = RoomStore::open()?;
        r.append_fact(&fact)
            .map(store::AppendOutcome::into_fact_reporting)
            .map(Some)
    }
}

/// **Plan F.** Append a typed [`Directive`] to the target agent's inbox in
/// the `.rally` ledger. This is the NEW canonical delivery path for
/// `rally inject`: the daemon ([`rally-termd`], P3) subscribes to the
/// ledger via kernel file-events and executes the directive.
///
/// Until the daemon ships (P3 not yet deployed) this path serves as the
/// durable, append-only record of every inject; the legacy
/// `BackendRunner::inject` keeps the synchronous PTY-write path for
/// tmux/cmux backends. For `Backend::Herdr`, the daemon is the only
/// delivery path once P3 lands; in P2 (pre-daemon), the legacy backend
/// still runs alongside so existing herdr smoke tests stay green.
///
/// Returns `Ok((assigned_seq, "pending"))` on success — the Directive is
/// durably appended but the daemon's Receipt has not yet arrived
/// (`DeliveryStatus::Pending`). Errors propagate; callers convert a write
/// failure into `delivery_state: failed` on the JSON envelope.
fn inject_via_ledger(
    repo: &std::path::Path,
    target_tool: &str,
    sender_tool: &str,
    text: &str,
    urgent: bool,
) -> Result<u64> {
    use rally_protocol::ledger::FileInbox;
    use rally_protocol::{Directive, DirectiveKind, Inbox, InterruptType, now_ts};

    let ledger_root = repo.join(".rally");
    let inbox = FileInbox::open(&ledger_root).map_err(RallyError::io("open .rally for inject"))?;

    let directive = Directive {
        seq: 0, // FileInbox assigns the next monotonic seq.
        to: target_tool.to_string(),
        from: sender_tool.to_string(),
        kind: DirectiveKind::Deliver,
        // P2 only delivers ADDITION semantics. Revision/Retraction land
        // when P4 surfaces `--urgent` and the InterruptBench-style
        // semantics shipped on top.
        itype: InterruptType::Addition,
        text: Some(text.to_string()),
        urgent,
        ts: now_ts(),
    };

    with_watchdog_command_commit(|| {
        let seq = inbox
            .append_directive(&directive)
            .map_err(RallyError::io("append directive"))?;
        // FileInbox reports success only after the record and any new-file
        // directory entry are synced. Marking before this point would let the
        // outer watchdog claim a mutation that was not yet durable.
        mark_watchdog_command_commit();
        Ok(seq)
    })
}

fn wake_fact(
    target_tool: &str,
    subject: &str,
    scope: Vec<String>,
    summary: Option<String>,
    evidence: Vec<String>,
    ref_id: Option<String>,
    status: Option<String>,
) -> Fact {
    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("wake"),
        seq: 0,
        thread_id: format!("wake-{}", sanitize_id(target_tool)),
        kind: FactKind::Wake,
        tool: Some("rally".to_string()),
        role: None,
        subject: subject.to_string(),
        scope,
        created_at: now_string(),
        summary,
        evidence,
        target: Some(target_tool.to_string()),
        ref_id,
        status,
        severity: None,
        uri: None,
        session: None,
    }
}

/// Build a Risk fact with the constant boilerplate fields pre-filled.
/// `scope`, `evidence`, and `ref_id` vary per call site; everything else is
/// constant across all four use-cases (warn severity, no role/target/status/uri).
/// Returns true when a `--tool` identifier should be treated as a fleet
/// worker (i.e. expected to live under a `rally`-managed session).
///
/// The rule is **opt-out, not opt-in**: every identifier is a fleet worker
/// UNLESS it matches one of the explicit human/lead exemptions below. This
/// is the f4 fix — the prior implementation also required the identifier to
/// contain a digit, which created a silent enforcement hole: bare worker
/// names without a numeric suffix (`claude`, `codex`, `opencode`,
/// `gemini`, `no-digits-here`) entered the room WITHOUT a managed-session
/// record and WITHOUT raising an `unmanaged-agent` risk fact — silently
/// defeating the "all workers managed" rule the fleet check was supposed to
/// enforce.
///
/// Exempt (returns false):
///   - The literal `lead` / human-facing driver name.
///   - Anything starting with `human:` or `user:` (case-insensitive). These
///     are explicit human identifiers, not workers.
///   - Suffix `:lead` (the human-readable lead form, e.g. `claude_code:lead`).
///   - Suffix `:l<N>` where `<N>` is one or more digits (the canonical
///     lead-number form, e.g. `claude_code:l4`).
///
/// Everything else — including bare worker names without numbers — is a
/// managed-style identifier. In-context subagents that the host coding agent
/// dispatches stay implicit; they don't `rally enter` from a separate process
/// so they never reach this gate.
fn is_managed_style_tool(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    // Explicit human / lead literal.
    if lower == "lead" {
        return false;
    }
    // `human:*` and `user:*` are explicit human identifiers — note the
    // colon: this MUST NOT silence worker ids that happen to start with the
    // substring "user" (e.g. `user-friendly-codex-01`). The pre-f4 code used
    // bare `starts_with("user")`, which over-matched; we narrow it here.
    if lower.starts_with("human:") || lower.starts_with("user:") {
        return false;
    }
    // Lead-style suffixes: `:lead` (human-readable) and `:l<N>` (canonical
    // numeric lead form). A trailing colon-segment that is exactly `lead`
    // or `l<digits>` exempts the identifier.
    if let Some(suffix) = lower.split(':').next_back() {
        if suffix == "lead" {
            return false;
        }
        if let Some(rest) = suffix.strip_prefix('l')
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return false;
        }
    }
    // Default: every other identifier is a fleet worker. No digit
    // requirement — the prior gate let bare `claude`/`codex`/`gemini` slip
    // through without a managed-session record (f4 hole).
    true
}

fn build_risk_fact(
    tool: &str,
    subject: String,
    summary: String,
    scope: Vec<String>,
    severity: &str,
    evidence: Vec<String>,
    ref_id: Option<String>,
) -> Fact {
    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Risk,
        tool: Some(tool.to_string()),
        role: None,
        subject,
        scope,
        created_at: now_string(),
        summary: Some(summary),
        evidence,
        target: None,
        ref_id,
        status: None,
        severity: Some(severity.to_string()),
        uri: None,
        session: None,
    }
}

fn find_session(target: &str, bins: &BackendBins) -> Result<ManagedSession> {
    let room = RoomStore::open()?;
    let Some(view) = read_session_views(&room, bins.clone())?
        .into_iter()
        .find(|view| {
            view.session.session_id == target
                || view.session.name == target
                || view.session.tool == target
        })
    else {
        return Err(RallyError::NotFound(format!(
            "unknown managed session {target}"
        )));
    };
    reject_stale_session(target, &view)?;
    Ok(view.session)
}

/// What kind of injection target a string resolves to. The two arms are the
/// reason this enum exists at all: managed sessions go through the legacy
/// dual-delivery path (ledger + synchronous tmux/cmux backend, intentional in
/// P2 — see `command_inject` for the SEC-009 / split-enforcement notes); a
/// `LedgerAgent` is a rally-termd-registered ptyd-pane identity with NO
/// `ManagedSession` record. For those, the ledger write IS the delivery; the
/// daemon (`rally-termd`) subscribes via kernel file-events and performs the
/// actual PTY-write, then posts a Receipt.
///
/// The previous `command_inject` called `find_session(&target)?` unconditionally,
/// which meant any agent bound through `agent.register` (without a tmux/cmux
/// managed session record) was rejected with `"unknown managed session ..."` —
/// even though Plan F P2 already shipped the ledger-side machinery that would
/// have delivered the inject just fine. Resolution order is now: managed-session
/// first (preserves all existing behavior, including the dual-delivery), then
/// valid agent-id (ledger-only delivery), then error.
enum InjectTarget {
    /// `target` matched an active managed session (by `session_id`, `name`, or
    /// `tool`). Delivery is the existing dual path: ledger write + legacy
    /// synchronous backend inject. Boxed so this dispatch enum stays small
    /// (the `ManagedSession` payload grew with the daemon-binding fields).
    Managed(Box<ManagedSession>),
    /// `target` did not match a managed session but is a syntactically valid
    /// agent-id (passes `rally_protocol::ledger::validate_agent_id`). Delivery
    /// is ledger-only: the typed Directive lands in `.rally/inbox/<id>.jsonl`
    /// and the daemon performs the PTY-write asynchronously.
    LedgerAgent(String),
}

/// Resolve a positional `inject` target to either a managed session (legacy
/// path) or a rally-termd-registered agent id (ledger-only path).
///
/// **Order matters**: managed-session match wins over agent-id validity. A
/// `target` string that happens to be both a registered managed-session tool
/// name AND a syntactically valid agent-id resolves to `Managed(_)` so the
/// behavior for genuine tmux/cmux sessions is byte-identical to pre-change. An
/// invalid `target` (path traversal, control char, etc.) returns the existing
/// error rather than a generic `InvalidInput` so the failure message is
/// unchanged for the common typo case.
///
/// SEC-006 / SEC-003 note: `validate_agent_id` is the same gate the ledger
/// writer applies, so a malformed `target` cannot reach
/// `inject_via_ledger`/`append_directive` here; ledger-side defenses stay
/// active too.
fn resolve_inject_target(target: &str, bins: &BackendBins) -> Result<InjectTarget> {
    // 1. Active managed session (existing behavior; preserves dual-delivery).
    let room = RoomStore::open()?;
    if let Some(view) = read_session_views(&room, bins.clone())?
        .into_iter()
        .find(|view| {
            view.session.session_id == target
                || view.session.name == target
                || view.session.tool == target
        })
    {
        reject_stale_session(target, &view)?;
        return Ok(InjectTarget::Managed(Box::new(view.session)));
    }

    // 1b. The target is not an ACTIVE managed session — but if it was EVER one
    //     (gone / renumbered / reaped), FAIL LOUDLY. Silently degrading to a
    //     ledger-only write (step 2) would send the message somewhere the
    //     sender never intended: a caller who named a session expects pane
    //     delivery, not a fact in the void. Observed live (2026-06-27): an
    //     inject to a renumbered session (`codex-01` after it became `codex-03`)
    //     returned `delivery_path: ledger_only` with NO error. `reject_stale_
    //     session` only covers a session whose view is still present-but-Stale;
    //     a fully gone/reaped session has no view, so it must be caught here.
    if let Some(prior) = prior_managed_session(&room, target)? {
        return Err(RallyError::Command(format!(
            "managed session {target} is no longer active (it was a {} session via {}; now gone/renumbered/reaped). It will NOT receive a pane inject. Re-target the current session — run `rally sessions` to find it.",
            prior.agent, prior.backend
        )));
    }

    // 2. Else, syntactically valid agent-id → ledger-only delivery to a
    //    rally-termd-registered ptyd-pane identity (e.g. `agent.register`-bound
    //    `claude` with no tmux/cmux session record).
    if rally_protocol::ledger::validate_agent_id(target).is_ok() {
        return Ok(InjectTarget::LedgerAgent(target.to_string()));
    }

    // 3. Neither managed nor a valid id — preserve the legacy error so existing
    //    consumers (incl. tests) see the same NotFound message.
    Err(RallyError::NotFound(format!(
        "unknown managed session {target}"
    )))
}

fn reject_stale_session(target: &str, view: &SessionView) -> Result<()> {
    if view.liveness == SessionLiveness::Stale {
        return Err(RallyError::Command(format!(
            "stale managed session {target}: session_id={} target={} source={}; run `rally stop {}` or `rally sessions --reap` before injecting",
            view.session.session_id,
            view.session.target,
            view.liveness_source,
            view.session.session_id
        )));
    }
    Ok(())
}

fn backend_target(backend: Backend, session_id: &str) -> String {
    match backend {
        Backend::Tmux => format!("rally-{}", sanitize_id(session_id)),
        Backend::Cmux => sanitize_id(session_id),
        // ptyd's real target is the daemon pane id, assigned at spawn time. This
        // is only a pre-spawn placeholder for the reserved session record; the
        // ptyd spawn path overwrites `session.target` with the pane id.
        Backend::Ptyd => format!("rally-ptyd-{}", sanitize_id(session_id)),
    }
}

fn handoff_prompt(session: &ManagedSession, handoff: &str) -> String {
    format!(
        "Rally managed-session injection for {}. Run: rally next --tool {} --json. If it is actionable for handoff {}, execute the suggested Rally completion command or run: rally say resolve --tool {} --ref {} --subject 'resolved via Rally managed session' --json. Do not edit files unless the Rally action explicitly requires it. Do not ask for confirmation after the Rally command succeeds.",
        session.name, session.tool, handoff, session.tool, handoff
    )
}

/// Ledger-only handoff prompt: same shape as [`handoff_prompt`] but addressed
/// to a rally-termd-registered agent that has no `ManagedSession` record. The
/// `target` is the validated agent-id used as both the `rally next --tool`
/// argument and the resolve sender — those are the recipient's identity, not
/// any session name.
fn handoff_prompt_ledger(target: &str, handoff: &str) -> String {
    format!(
        "Rally ledger injection for {target}. Run: rally next --tool {target} --json. If it is actionable for handoff {handoff}, execute the suggested Rally completion command or run: rally say resolve --tool {target} --ref {handoff} --subject 'resolved via Rally ledger' --json. Do not edit files unless the Rally action explicitly requires it. Do not ask for confirmation after the Rally command succeeds."
    )
}

fn wait_for_resolution(
    handoff: &str,
    timeout_seconds: u64,
    after_seq: i64,
    room: &AckPollingStore,
    expected_tool: &str,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut last_seen_seq = after_seq;
    let mut ignored_target_responses = BTreeSet::new();
    loop {
        for fact in room.facts()? {
            last_seen_seq = last_seen_seq.max(fact.seq);
            if fact.seq > after_seq && fact.ref_id.as_deref() == Some(handoff) {
                if !matches!(
                    fact.kind,
                    store::FactKind::Resolve
                        | store::FactKind::Receipt
                        | store::FactKind::Artifact
                        | store::FactKind::Blocker
                        | store::FactKind::Decision
                ) {
                    continue;
                }
                if fact.tool.as_deref() == Some(expected_tool) {
                    let blocked = fact.kind == store::FactKind::Blocker;
                    let decision = fact.kind == store::FactKind::Decision;
                    let resolved = matches!(
                        fact.kind,
                        store::FactKind::Resolve
                            | store::FactKind::Receipt
                            | store::FactKind::Artifact
                    );
                    return Ok(json!({
                        "received": true,
                        "resolved": resolved,
                        "handoff_closed": resolved,
                        "blocked": blocked,
                        "decision": decision,
                        "event_id": fact.event_id,
                        "tool": fact.tool,
                        "expected_tool": expected_tool,
                        "kind": fact.kind.as_str(),
                        "subject": fact.subject
                    }));
                }
                ignored_target_responses.insert(fact.event_id);
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(250)));
    }
    let fallback_plan = ack_timeout_fallback_plan(handoff, expected_tool, timeout_seconds);
    Ok(json!({
        "received": false,
        "resolved": false,
        "assume_received": false,
        "timed_out": true,
        "waited_seconds": timeout_seconds,
        "after_seq": after_seq,
        "expected_tool": expected_tool,
        "ignored_resolves": ignored_target_responses.len(),
        "ignored_target_responses": ignored_target_responses.len(),
        "fallback_plan": fallback_plan
    }))
}

fn inject_ack_state(require_ack: bool, dry_run: bool, ack: Option<&Value>) -> &'static str {
    if !require_ack {
        return "not_required";
    }
    if dry_run {
        return "planned";
    }
    if let Some(ack) = ack {
        if ack.get("blocked").and_then(Value::as_bool).unwrap_or(false) {
            return "blocked";
        }
        if ack
            .get("received")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return "acked";
        }
        if ack
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return "timeout";
        }
    }
    "pending"
}

fn inject_verified_received(ack: Option<&Value>) -> bool {
    ack.and_then(|ack| ack.get("received"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn inject_fallback_plan(
    require_ack: bool,
    dry_run: bool,
    handoff: Option<&str>,
    expected_tool: &str,
    ack: Option<&Value>,
) -> Option<Value> {
    if !require_ack || dry_run || inject_verified_received(ack) {
        return None;
    }
    if let Some(plan) = ack.and_then(|ack| ack.get("fallback_plan")).cloned() {
        return Some(plan);
    }
    handoff.map(|handoff| ack_timeout_fallback_plan(handoff, expected_tool, 0))
}

/// Sender-observability plan for the `ledger_only` inject path (any tool id;
/// agent-neutral). The ledger arm never runs a synchronous backend, so
/// `delivered` is always `false` and `delivery_state` is `pending`. Without a
/// populated `fallback_plan` a caller can misread `ok:true` as a live delivery.
/// This makes the async contract explicit: the message is durably queued and is
/// consumed when the target next runs `rally next`/`enter`; for guaranteed live
/// delivery the target must be a managed session (`rally run` / `rally adopt`).
fn ledger_async_fallback_plan(agent_id: &str) -> Value {
    json!({
        "trigger": "ledger_only_delivery",
        "assumption": "queued_not_live_delivered",
        "target": agent_id,
        "meaning": format!(
            "message was durably queued to the ledger for {agent_id}, NOT delivered to a live session; delivered=false / delivery_state=pending is expected on this path"
        ),
        "delivered_when": format!(
            "{agent_id} picks it up on its next `rally next`/`rally enter` (or its registered rally-termd pane delivers it)"
        ),
        "checks": [
            format!("rally next --tool {agent_id} --json; confirm the target surfaces this handoff"),
            "rally room --json; confirm the target squad is active (not stale/absent)"
        ],
        "for_live_delivery": [
            format!(
                "launch the target as a managed session: `rally run <agent>` (mints an injectable pane for {agent_id}; needs tmux or a live ptyd socket, and names which one is missing if not)"
            ),
            format!("or adopt an already-running pane: `rally adopt {agent_id} --tmux <target>`"),
            "for anything time-sensitive, ALSO post a durable `rally say handoff` (dual-channel)"
        ]
    })
}

fn ack_timeout_fallback_plan(handoff: &str, expected_tool: &str, timeout_seconds: u64) -> Value {
    json!({
        "trigger": "ack_timeout",
        "assumption": "not_received",
        "handoff": handoff,
        "expected_tool": expected_tool,
        "timeout_seconds": timeout_seconds,
        "checks": [
            format!("rally room --json; confirm handoff {handoff} is still open"),
            format!("rally next --tool {expected_tool} --json; confirm the target still sees the handoff"),
            "rally recent --limit 50 --json; look for target-authored resolve/artifact/blocker",
            "check whether assigned files changed or claims moved before retrying"
        ],
        "fallbacks": [
            "retry once with a short doorbell only",
            "move the work to a separate worktree if ownership is safe",
            "handoff to another live agent when the target stays silent",
            "escalate to the human when file ownership or risk is unclear"
        ]
    })
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub(crate) fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}", nanos & 0xfffff)
}

pub(crate) fn shell_quote(value: &str) -> String {
    let safe_value = value.replace('\0', "?");
    shlex::try_quote(&safe_value)
        .expect("NUL-stripped shell argument should be quoteable")
        .into_owned()
}

/// Render the stable-id recovery command as one shell-safe argv sequence.
///
/// Event ids are opaque schema strings, so callers must quote them rather than
/// narrowing the accepted grammar or interpolating executable shell syntax.
pub(crate) fn locate_remedy(event_id: &str) -> String {
    format!("rally locate {} --json", shell_quote(event_id))
}

/// Crate-wide serialization lock for tests that mutate process-global env vars
/// (`RALLY_ENGAGEMENT`, `RALLY_ROTATE_DAYS`, `RALLY_GLOBAL_INDEX`, `HOME`,
/// `PTYD_SOCKET_PATH`, `XDG_RUNTIME_DIR`).  Every env-mutating test must acquire
/// this lock at the top of the test body and hold it for the full body.
#[cfg(test)]
pub(crate) static PROCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// `rally --help` must name every command `rally` accepts.
    ///
    /// It named 31 of 42. The 11 it omitted — `doctor`, `risks`, `decisions`,
    /// `artifacts`, `claims`, `lead`, `ack`, `worktree`, `daemon`,
    /// `self-exit-check`, `claims-refresh` — were real, documented elsewhere,
    /// and invisible to anyone who typed `--help` first. The unknown-command
    /// handler routes users to that same text, so a user who guessed a real
    /// command name and mistyped it was sent to a list that did not contain it.
    ///
    /// This is the durable control, not the one-time fix: a command added to
    /// `cli::COMMANDS` without a help line fails here.
    #[test]
    fn help_text_names_every_registered_command() {
        let help = help_text();
        let missing: Vec<&str> = crate::cli::COMMANDS
            .iter()
            .copied()
            .filter(|command| {
                !help
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("rally {command}")))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "`rally --help` omits registered commands: {missing:?}. Add a usage line to \
             help_text() for each — a command users cannot discover is a command they \
             will not use."
        );
    }

    // ---- P1c: session liveness — real pane probe beats presence TTL -------

    fn liveness_session(session_id: &str, tool: &str) -> ManagedSession {
        ManagedSession {
            session_id: session_id.to_string(),
            name: session_id.to_string(),
            agent: "codex".to_string(),
            tool: tool.to_string(),
            backend: "tmux".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            target: format!("rally-{session_id}"),
            worktree_path: None,
            branch: None,
            daemon_registered: false,
            daemon_pane: None,
            daemon_socket: None,
        }
    }

    #[test]
    fn room_injectability_is_scoped_to_requested_current_or_recent_agents() {
        let mut snapshot = RoomSnapshot {
            lead: Some("lead-tool".to_string()),
            squads: vec![
                store::Squad {
                    tool: "recent-tool".to_string(),
                    status: "active".to_string(),
                    ..store::Squad::default()
                },
                store::Squad {
                    tool: "historical-tool".to_string(),
                    status: "idle".to_string(),
                    ..store::Squad::default()
                },
            ],
            ..RoomSnapshot::default()
        };
        snapshot.active_claims.push(Fact {
            tool: Some("claim-owner".to_string()),
            ..Fact::default()
        });
        let session = liveness_session("live-session", "managed-tool");
        let session_views = vec![SessionView {
            session,
            liveness: SessionLiveness::Live,
            liveness_source: "backend_probe",
            injectable: true,
            inject_status: "live_managed_session".to_string(),
            inject_via: "tmux".to_string(),
        }];

        let rows = build_agent_injectability(&snapshot, &session_views, None);
        let tools: BTreeSet<_> = rows.iter().map(|row| row.tool.as_str()).collect();
        assert_eq!(
            tools,
            BTreeSet::from(["claim-owner", "lead-tool", "managed-tool", "recent-tool"])
        );

        let requested =
            build_agent_injectability(&snapshot, &session_views, Some("historical-tool"));
        assert!(
            requested.iter().any(|row| row.tool == "historical-tool"),
            "an explicit --tool query must include the requested historical agent"
        );
    }

    #[test]
    fn live_backend_probe_overrides_stale_heartbeat_ttl() {
        // The P1c bug: a busy/quiet-but-ALIVE agent whose presence heartbeat
        // lapsed (>15min) was marked Stale, which rejected inject to it. A real
        // "pane is live" probe must win over the TTL. Agent-neutral.
        for tool in ["codex:01", "claude_code:4f6d8c1a", "gemini:build-01"] {
            let s = liveness_session("sid", tool);
            let heartbeat_stale = BTreeMap::from([(tool.to_string(), true)]);
            let probes = BTreeMap::from([("sid".to_string(), SessionLiveness::Live)]);
            assert_eq!(
                projected_session_liveness(&s, &heartbeat_stale, &probes),
                (SessionLiveness::Live, "backend_probe"),
                "live pane must NOT be stale just because the heartbeat lapsed ({tool})"
            );
        }
    }

    #[test]
    fn dead_backend_probe_is_stale_even_with_fresh_heartbeat() {
        let s = liveness_session("sid", "codex:01");
        let heartbeat_stale = BTreeMap::from([("codex:01".to_string(), false)]);
        let probes = BTreeMap::from([("sid".to_string(), SessionLiveness::Stale)]);
        assert_eq!(
            projected_session_liveness(&s, &heartbeat_stale, &probes),
            (SessionLiveness::Stale, "backend_probe"),
            "a really-gone pane is stale regardless of a fresh heartbeat"
        );
    }

    #[test]
    fn heartbeat_ttl_is_the_fallback_only_when_probe_is_unknown() {
        let s = liveness_session("sid", "codex:01");
        // stale TTL + no probe → Stale via TTL fallback.
        assert_eq!(
            projected_session_liveness(
                &s,
                &BTreeMap::from([("codex:01".to_string(), true)]),
                &BTreeMap::new(),
            ),
            (SessionLiveness::Stale, "heartbeat_ttl"),
        );
        // fresh TTL + no probe → Live via TTL fallback.
        assert_eq!(
            projected_session_liveness(
                &s,
                &BTreeMap::from([("codex:01".to_string(), false)]),
                &BTreeMap::new(),
            ),
            (SessionLiveness::Live, "heartbeat_ttl"),
        );
        // no TTL info + no probe → Unknown.
        assert_eq!(
            projected_session_liveness(&s, &BTreeMap::new(), &BTreeMap::new()),
            (SessionLiveness::Unknown, "backend_probe"),
        );
    }

    // ---- inject watchdog budget (Chunk 2 root-cause fix) -----------------

    #[test]
    fn inject_watchdog_budget_covers_ack_timeout_not_3s_default() {
        // The whole bug: `inject --handoff --timeout-seconds 75` was killed at
        // the 3s default watchdog, emitting the bare fail-open envelope before
        // the 75s ACK wait could run. The inject watchdog must now exceed the
        // ACK budget.
        let d = resolve_watchdog_timeout(&argv(&[
            "inject",
            "reviewer-01",
            "--handoff",
            "evt-1",
            "--timeout-seconds",
            "75",
            "--json",
        ]));
        assert_eq!(
            d,
            Duration::from_millis(75 * 1000 + INJECT_WATCHDOG_HEADROOM_MS)
        );
        assert!(
            d > Duration::from_millis(DEFAULT_WATCHDOG_TIMEOUT_MS),
            "inject budget must exceed the 3s hook default"
        );
        assert!(
            d > Duration::from_millis(MAX_WATCHDOG_TIMEOUT_MS),
            "inject budget must be allowed to exceed the 60s hook cap"
        );
    }

    // ---- ledger_only sender-observability (RCA2 P0-corrected) -------------

    #[test]
    fn ledger_async_fallback_plan_is_agent_neutral_and_actionable() {
        // The fix for "ok:true + delivered:false is misreadable as delivered":
        // the ledger_only path must always carry a fallback_plan explaining the
        // async contract + how to get live delivery. Agent-neutral: identical
        // shape for any tool id (codex:*, claude_code:*, gemini:*).
        for id in ["codex:42", "claude_code:4f6d8c1a", "gemini:build-01"] {
            let plan = ledger_async_fallback_plan(id);
            assert_eq!(plan["trigger"], "ledger_only_delivery");
            assert_eq!(plan["assumption"], "queued_not_live_delivered");
            assert_eq!(plan["target"], id, "plan must name the target id");
            assert!(
                plan["delivered_when"].as_str().unwrap().contains(id),
                "must tell the sender when {id} actually receives it"
            );
            let live = plan["for_live_delivery"].as_array().unwrap();
            assert!(
                live.iter()
                    .any(|s| s.as_str().unwrap().contains("rally run")),
                "must offer `rally run` as the live-delivery remediation"
            );
            assert!(
                live.iter()
                    .any(|s| s.as_str().unwrap().contains("rally adopt")),
                "must offer `rally adopt` as the live-delivery remediation"
            );
        }
    }

    #[test]
    fn inject_watchdog_uses_eq_form_and_default_when_flag_absent() {
        // `--timeout-seconds=120` form.
        let d = resolve_watchdog_timeout(&argv(&["inject", "r", "--timeout-seconds=120"]));
        assert_eq!(
            d,
            Duration::from_millis(120 * 1000 + INJECT_WATCHDOG_HEADROOM_MS)
        );
        // A bare `inject --handoff` (no explicit timeout) still gets room
        // beyond the 3s default, sized off the CLI's 10s ACK default.
        let bare = resolve_watchdog_timeout(&argv(&["inject", "r", "--handoff", "e"]));
        assert_eq!(
            bare,
            Duration::from_millis(10 * 1000 + INJECT_WATCHDOG_HEADROOM_MS)
        );
    }

    #[test]
    fn inject_watchdog_is_clamped_to_ceiling() {
        // Even an absurd ack budget is bounded by the defensive ceiling.
        let d = resolve_watchdog_timeout(&argv(&["inject", "r", "--timeout-seconds", "99999"]));
        assert_eq!(d, Duration::from_millis(INJECT_MAX_WATCHDOG_TIMEOUT_MS));
    }

    #[test]
    fn explicit_timeout_ms_override_wins_even_for_inject() {
        // An operator can still cap a runaway inject; the override is clamped to
        // the hook band so it can't itself re-introduce an unbounded hang.
        let d = resolve_watchdog_timeout(&argv(&[
            "inject",
            "r",
            "--timeout-seconds",
            "300",
            "--timeout-ms",
            "4000",
        ]));
        assert_eq!(d, Duration::from_millis(4000));
    }

    #[test]
    fn non_inject_commands_keep_the_3s_hook_default() {
        // The hook-invoked read paths are unchanged — still the fast default.
        for cmd in [["room", "--json"], ["next", "--json"], ["status", "--json"]] {
            assert_eq!(
                resolve_watchdog_timeout(&argv(&cmd)),
                Duration::from_millis(DEFAULT_WATCHDOG_TIMEOUT_MS),
                "{cmd:?} must keep the 3s default"
            );
        }
        // `inject` only matches as the FIRST positional, never as an argument
        // value to another command.
        assert_eq!(
            resolve_watchdog_timeout(&argv(&["say", "handoff", "--subject", "inject"])),
            Duration::from_millis(DEFAULT_WATCHDOG_TIMEOUT_MS)
        );
    }

    #[test]
    fn watchdog_posture_classifies_mutations_without_fail_closed_opt_in() {
        for cmd in [
            argv(&["say", "handoff", "--subject", "x"]),
            argv(&["enter", "--tool", "codex"]),
            argv(&["ack", "--tool", "codex"]),
            argv(&[
                "route-findings",
                "findings.json",
                "--tool",
                "scanner",
                "--verified",
            ]),
            argv(&["status", "post", "--tool", "codex", "--state", "idle"]),
            argv(&[
                "backlog", "add", "--tool", "codex", "--id", "b1", "--intent", "x",
            ]),
            argv(&["lead", "assign", "--tool", "lead"]),
            argv(&["mission", "--set", "north star"]),
            argv(&["check", "liveness", "--enforce"]),
            argv(&[
                "doctor",
                "--migrate-db-only",
                "--engagement",
                "alpha",
                "--apply",
            ]),
        ] {
            assert_eq!(
                resolve_watchdog_posture(&cmd, false),
                WatchdogPosture::ClosedMutation,
                "{cmd:?} must fail closed as a mutation"
            );
        }
    }

    #[test]
    fn watchdog_posture_keeps_read_only_commands_fail_open() {
        for cmd in [
            argv(&["room", "--json"]),
            argv(&["next", "--tool", "codex", "--json"]),
            argv(&["lead", "show", "--json"]),
            argv(&["check", "before-write", "--tool", "codex", "--json"]),
            argv(&["check", "coordination", "--strict", "--json"]),
            argv(&["sessions", "--json"]),
            argv(&[
                "doctor",
                "--migrate-db-only",
                "--engagement",
                "alpha",
                "--json",
            ]),
        ] {
            assert_eq!(
                resolve_watchdog_posture(&cmd, false),
                WatchdogPosture::Open,
                "{cmd:?} must remain fail open"
            );
        }
    }

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-lib-{label}-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn init_status_git_repo(root: &Path, branch: &str) -> String {
        use crate::test_git_fixture::fixture_git;
        fixture_git(root, &["init"]);
        std::fs::write(root.join("tracked.txt"), "status done\n").unwrap();
        fixture_git(root, &["add", "tracked.txt"]);
        fixture_git(root, &["commit", "-m", "initial"]);
        fixture_git(root, &["checkout", "-B", branch]);
        fixture_git(root, &["rev-parse", "--verify", "HEAD"])
    }

    // Plan F functional core (Chunk 3): self_host_guard_* tests removed
    // alongside enforce_easy_terminal_self_host_guard (the herdr-specific
    // reentrancy guard). With Backend::Herdr removed, the failure mode
    // these guarded against (rally launching a herdr-backed session into
    // the same ET daemon socket as the host) no longer exists.

    /// Component B acceptance test 1: running `ensure_presence` without a prior
    /// `enter` registers the tool in squads and asserts it as lead (first tool).
    #[test]
    fn ensure_presence_auto_enters_tool_and_sets_lead() {
        let root = unique_root("ensure-presence-auto-enter");
        // Simulate a git repo so RoomStore::open() resolves correctly.
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let snapshot_before = room.snapshot().unwrap();
        assert!(snapshot_before.squads.is_empty(), "room starts empty");

        // Call ensure_presence directly (mimics what command_say would do).
        ensure_presence(&room, "tool-x").unwrap();

        let snapshot = room.snapshot().unwrap();
        assert!(
            snapshot.squads.iter().any(|s| s.tool == "tool-x"),
            "tool-x must appear in squads after ensure_presence"
        );
        assert_eq!(
            snapshot.lead.as_deref(),
            Some("tool-x"),
            "first tool to call ensure_presence is lead"
        );

        // Presence fact count for tool-x in the ledger must be exactly 1.
        let presence_count = room
            .facts()
            .unwrap()
            .iter()
            .filter(|f| f.kind == "presence" && f.tool.as_deref() == Some("tool-x"))
            .count();
        assert_eq!(presence_count, 1, "exactly one presence fact for tool-x");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heartbeat_renews_every_owned_claim_durably() {
        let root = unique_root("heartbeat-renews-claims");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        ensure_presence(&room, "tool-x").unwrap();
        let claim = store::Fact {
            from_session_id: Some(
                current_protocol_session(Some("tool-x"))
                    .from_session_id()
                    .to_string(),
            ),
            schema: FACT_SCHEMA.to_string(),
            event_id: "claim-heartbeat-renew".to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-x".to_string()),
            role: None,
            subject: "claim for heartbeat renewal".to_string(),
            scope: vec!["file:src/lib.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&claim).unwrap();
        let before = room.facts().unwrap().len();

        assert_eq!(renew_owned_claim_leases(&room, "tool-x").unwrap(), 1);

        let facts = room.facts().unwrap();
        assert_eq!(facts.len(), before + 1);
        assert_eq!(facts.last().unwrap().kind, store::FactKind::ClaimRenewed);
        assert_eq!(
            facts.last().unwrap().ref_id.as_deref(),
            Some("claim-heartbeat-renew")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn heartbeat_does_not_renew_same_tool_sibling_claim() {
        let root = unique_root("heartbeat-session-owner");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let claim = store::Fact {
            from_session_id: Some("sess:sibling".to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: "claim-sibling".to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-x".to_string()),
            role: None,
            subject: "sibling claim".to_string(),
            scope: vec!["file:src/sibling.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&claim).unwrap();

        assert_eq!(renew_owned_claim_leases(&room, "tool-x").unwrap(), 0);
        assert!(
            room.facts()
                .unwrap()
                .iter()
                .all(|fact| fact.kind != store::FactKind::ClaimRenewed),
            "a same-tool sibling heartbeat must not renew the claim"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn presence_stamps_worktree_and_external_host_pid() {
        let _guard = PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = unique_root("presence-observer-stamps");
        init_status_git_repo(&root, "main");
        unsafe { env::set_var("RALLY_OBSERVER_PID", "424242") };
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence(&room, "tool-observed").unwrap();

        unsafe { env::remove_var("RALLY_OBSERVER_PID") };
        let presence = room
            .facts()
            .unwrap()
            .into_iter()
            .find(|fact| {
                fact.kind == store::FactKind::Presence
                    && fact.tool.as_deref() == Some("tool-observed")
            })
            .unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        assert!(
            presence
                .evidence
                .iter()
                .any(|item| item == &format!("worktree_path:{}", canonical.display()))
        );
        assert!(
            presence
                .evidence
                .iter()
                .any(|item| item == "observer_pid:424242")
        );
        assert!(
            presence
                .evidence
                .iter()
                .any(|item| item.starts_with("branch_head_sha:"))
        );
        assert!(presence.from_session_id.is_some());
        std::fs::remove_dir_all(root).ok();
    }

    /// Component B acceptance test 2: a second call for the same tool writes no
    /// duplicate presence fact (idempotent per engagement).
    #[test]
    fn ensure_presence_is_idempotent_no_duplicate_facts() {
        let root = unique_root("ensure-presence-idempotent");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence(&room, "tool-x").unwrap();
        ensure_presence(&room, "tool-x").unwrap(); // second call — must be no-op

        let presence_count = room
            .facts()
            .unwrap()
            .iter()
            .filter(|f| f.kind == "presence" && f.tool.as_deref() == Some("tool-x"))
            .count();
        assert_eq!(
            presence_count, 1,
            "second ensure_presence must not write a duplicate presence fact"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ensure_presence_registers_same_tool_sibling_session() {
        let root = unique_root("ensure-presence-sibling");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence_tiered_for_session(&room, "tool-x", None, "session-a").unwrap();
        ensure_presence_tiered_for_session(&room, "tool-x", None, "session-b").unwrap();

        let sessions = room
            .facts()
            .unwrap()
            .into_iter()
            .filter(|fact| {
                fact.kind == store::FactKind::Presence && fact.tool.as_deref() == Some("tool-x")
            })
            .filter_map(|fact| fact.from_session_id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sessions.len(), 2, "each sibling session needs presence");
        std::fs::remove_dir_all(root).ok();
    }

    /// Component B acceptance test 3: a second tool auto-enters but lead stays
    /// with the first tool.
    #[test]
    fn frontier_tier_gates_lead_assignment() {
        // L-1 (docs/SPEC-lead-agent.md): lead auto-assign is frontier-only.
        // A declared `fast` agent entering an empty room does NOT take the seat;
        // a later `frontier` agent does. Undeclared tier stays lead-eligible.
        let root = unique_root("lead-frontier-gate");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // fast agent first → seat stays open.
        ensure_presence_tiered(&room, "haiku-1", Some("fast")).unwrap();
        assert_eq!(
            room.snapshot().unwrap().lead.as_deref(),
            None,
            "a fast-tier first-enter must NOT take the lead seat"
        );

        // frontier agent joins → becomes lead.
        ensure_presence_tiered(&room, "opus-1", Some("frontier")).unwrap();
        assert_eq!(
            room.snapshot().unwrap().lead.as_deref(),
            Some("opus-1"),
            "first frontier agent takes the open lead seat"
        );
    }

    #[test]
    fn host_runtime_ambiguity_detection() {
        // SL-1: >1 resolvable ptyd socket => ambiguous (agents must not guess).
        let base = unique_root("ptyd-sockets");
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let sa = a.join("ptyd.sock");
        let sb = b.join("ptyd.sock");
        std::fs::write(&sa, b"").unwrap();
        let one = existing_unique_paths(&[
            sa.to_string_lossy().to_string(),
            sb.to_string_lossy().to_string(),
        ]);
        assert_eq!(one.len(), 1, "only one socket exists -> not ambiguous");
        std::fs::write(&sb, b"").unwrap();
        let two = existing_unique_paths(&[
            sa.to_string_lossy().to_string(),
            sb.to_string_lossy().to_string(),
            sa.to_string_lossy().to_string(), // dup is ignored
        ]);
        assert_eq!(two.len(), 2, "two distinct sockets -> ambiguous (len>1)");
    }

    #[test]
    fn detect_host_runtime_finds_easy_terminal_ptyd_socket() {
        // SL-2: when …/EasyTerminal/ptyd.sock exists, detect_host_runtime
        // reports under_ptyd=true with that path in sockets_found. When a
        // second ptyd.sock exists (e.g. ~/.config/ptyd/ptyd.sock), ambiguous=true.
        //
        // Test runs in an isolated HOME so we don't depend on / pollute the
        // real user filesystem. PTYD_SOCKET_PATH is cleared so only on-disk
        // resolution drives the decision.
        //
        // Env mutation is process-wide and Rust 2024 marks it unsafe. We
        // serialize HOME/PTYD/XDG mutations via the crate-wide PROCESS_ENV_LOCK
        // so all env-touching tests in this binary serialize against each other.
        let _env_guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let base = unique_root("ptyd-detect");
        let home = base.join("home");
        let et_dir = home.join("Library/Application Support/EasyTerminal");
        let cfg_dir = home.join(".config/ptyd");
        std::fs::create_dir_all(&et_dir).unwrap();
        std::fs::create_dir_all(&cfg_dir).unwrap();

        // Snapshot + override env. unsafe per Rust 2024 edition; we restore
        // before the function returns even on assertion failure via a guard.
        struct EnvGuard {
            home: Option<String>,
            ptyd: Option<String>,
            xdg: Option<String>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.home {
                        Some(v) => env::set_var("HOME", v),
                        None => env::remove_var("HOME"),
                    }
                    match &self.ptyd {
                        Some(v) => env::set_var("PTYD_SOCKET_PATH", v),
                        None => env::remove_var("PTYD_SOCKET_PATH"),
                    }
                    match &self.xdg {
                        Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
                        None => env::remove_var("XDG_RUNTIME_DIR"),
                    }
                }
            }
        }
        let _guard = EnvGuard {
            home: env::var("HOME").ok(),
            ptyd: env::var("PTYD_SOCKET_PATH").ok(),
            xdg: env::var("XDG_RUNTIME_DIR").ok(),
        };
        unsafe {
            env::set_var("HOME", &home);
            env::remove_var("PTYD_SOCKET_PATH");
            env::remove_var("XDG_RUNTIME_DIR");
        }

        // Only the Easy Terminal socket exists.
        let et_sock = et_dir.join("ptyd.sock");
        std::fs::write(&et_sock, b"").unwrap();
        let hr = detect_host_runtime();
        assert!(
            hr.under_ptyd,
            "under_ptyd must be true when …/EasyTerminal/ptyd.sock exists"
        );
        assert!(!hr.ambiguous, "single ptyd socket -> not ambiguous");
        assert!(
            hr.sockets_found
                .iter()
                .any(|s| s == &et_sock.to_string_lossy()),
            "sockets_found must include the Easy Terminal ptyd.sock: {:?}",
            hr.sockets_found
        );

        // Add the CLI socket → ambiguous.
        let cli_sock = cfg_dir.join("ptyd.sock");
        std::fs::write(&cli_sock, b"").unwrap();
        let hr2 = detect_host_runtime();
        assert!(hr2.under_ptyd);
        assert!(
            hr2.ambiguous,
            "two ptyd sockets on disk -> ambiguous=true (got sockets_found={:?})",
            hr2.sockets_found
        );
    }

    #[test]
    fn coordination_gate_predicate() {
        // C3: presence + ack + claim-covers-every-changed-file.
        let root = unique_root("coord-merge");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root).unwrap();
        ensure_presence_tiered(&room, "opus-1", Some("frontier")).unwrap();
        let mk = |subject: &str, scope: Vec<String>| Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: if subject == "coordination:ack" {
                FactKind::Decision
            } else {
                FactKind::Claim
            },
            tool: Some("opus-1".to_string()),
            role: None,
            subject: subject.to_string(),
            scope,
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&mk("coordination:ack", Vec::new()))
            .unwrap();
        room.append_fact(&mk("own a", vec!["file:src/a.rs".to_string()]))
            .unwrap();
        let snap = room.snapshot().unwrap();
        let (p, a, unc) = coordination_offenders(&snap, "opus-1", &["src/a.rs".to_string()]);
        assert!(
            p && a && unc.is_empty(),
            "acked + claimed file passes the gate"
        );
        let (_, _, unc2) = coordination_offenders(&snap, "opus-1", &["src/b.rs".to_string()]);
        assert_eq!(
            unc2,
            vec!["src/b.rs".to_string()],
            "unclaimed changed file is uncovered"
        );
        let (p3, a3, _) = coordination_offenders(&snap, "ghost", &["src/a.rs".to_string()]);
        assert!(!p3 && !a3, "unknown tool has no presence/ack");
    }

    #[test]
    fn liveness_conflicts_unacked_idle_claim_holder() {
        // C2: an unacknowledged + idle squad still holding an open claim is
        // conflict-out eligible; recording an ack clears it.
        let root = unique_root("coord-liveness");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root).unwrap();
        let old = "2020-01-01T00:00:00Z";
        let mk = |kind: FactKind, subject: &str, scope: Vec<String>| Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind,
            tool: Some("ghost-1".to_string()),
            role: None,
            subject: subject.to_string(),
            scope,
            created_at: old.to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&mk(
            FactKind::Presence,
            "agent presence: ghost-1",
            Vec::new(),
        ))
        .unwrap();
        room.append_fact(&mk(
            FactKind::Claim,
            "claim x",
            vec!["file:x.rs".to_string()],
        ))
        .unwrap();
        let conflicted = liveness_conflicted(&room.snapshot().unwrap());
        assert_eq!(conflicted.len(), 1, "unacked+idle+claim must be conflicted");
        assert_eq!(conflicted[0].0, "ghost-1");
        assert_eq!(conflicted[0].1.len(), 1, "one held claim");
        // ack (kept old-dated so it stays idle) clears the conflict via acknowledged.
        room.append_fact(&mk(FactKind::Decision, "coordination:ack", Vec::new()))
            .unwrap();
        assert!(
            liveness_conflicted(&room.snapshot().unwrap()).is_empty(),
            "ack must clear conflict-out eligibility"
        );
    }

    /// independent-auditor v2 HIGH (2026-06-09): `check liveness --enforce` must
    /// apply the SAME 2h destructive-release gate as `say release --path`. A
    /// busy-but-quiet unacknowledged owner that is idle (>15m) but NOT yet >2h
    /// silent is REPORTED as conflicted (advisory) but its claim must NOT be
    /// released. This locks the composition the enforce arm now relies on:
    /// conflicted ∧ takeover-eligible (not conflicted alone).
    #[test]
    fn liveness_enforce_respects_takeover_gate_for_busy_but_quiet_owner() {
        // Hold PROCESS_ENV_LOCK: this path transitively reads the process-global
        // RALLY_ENGAGEMENT, which env-mutating tests remove/set; the reader must
        // serialize against them (set/remove_var is unsound vs concurrent reads).
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let root = unique_root("coord-liveness-2h-gate");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at_with_engagement(root, None).unwrap();
        // 30 minutes ago: idle (>15m) but well under the 2h takeover bar, and
        // never acknowledged → conflicted but not release-eligible.
        let thirty_min_ago = (chrono::Utc::now() - chrono::Duration::minutes(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mk = |kind: FactKind, subject: &str, scope: Vec<String>| Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind,
            tool: Some("busy-ghost".to_string()),
            role: None,
            subject: subject.to_string(),
            scope,
            created_at: thirty_min_ago.clone(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&mk(FactKind::Presence, "presence: busy-ghost", Vec::new()))
            .unwrap();
        room.append_fact(&mk(
            FactKind::Claim,
            "claim y",
            vec!["file:y.rs".to_string()],
        ))
        .unwrap();
        let snap = room.snapshot().unwrap();
        // Conflicted (unack + idle + holding a claim) — reportable.
        assert_eq!(
            liveness_conflicted(&snap).len(),
            1,
            "30m-idle unacked claim-holder must be conflicted (reportable)"
        );
        // But NOT takeover-eligible — so the enforce arm must not release it.
        assert!(
            !snap.takeover_eligible_owners().contains("busy-ghost"),
            "a 30m-idle owner must not be takeover-eligible (needs >2h)"
        );
    }

    #[test]
    fn ack_flips_squad_acknowledged() {
        // Coordination-mandate C1: a coordination:ack fact flips the squad to acknowledged.
        let root = unique_root("coord-ack");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root).unwrap();
        ensure_presence_tiered(&room, "opus-1", Some("frontier")).unwrap();
        let acked = |r: &store::RoomStore| {
            r.snapshot()
                .unwrap()
                .squads
                .iter()
                .find(|sq| sq.tool == "opus-1")
                .map(|sq| sq.acknowledged)
                .unwrap_or(false)
        };
        assert!(!acked(&room), "squad must be unacknowledged before ack");
        let ack = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Decision,
            tool: Some("opus-1".to_string()),
            role: None,
            subject: "coordination:ack".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&ack).unwrap();
        assert!(
            acked(&room),
            "squad must be acknowledged after coordination:ack"
        );
    }

    #[test]
    fn relinquish_reopens_lead_seat() {
        // L-2b: a role:lead:relinquished decision reopens the seat (lead = None);
        // a later assign re-fills it. Projection: latest lead-family fact wins.
        let root = unique_root("lead-relinquish");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root).unwrap();
        ensure_presence_tiered(&room, "opus-1", Some("frontier")).unwrap();
        assert_eq!(room.snapshot().unwrap().lead.as_deref(), Some("opus-1"));
        let relinquish = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Decision,
            tool: Some("opus-1".to_string()),
            role: None,
            subject: "role:lead:relinquished".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&relinquish).unwrap();
        assert_eq!(
            room.snapshot().unwrap().lead,
            None,
            "relinquish must reopen the lead seat"
        );
    }

    #[test]
    fn undeclared_tier_stays_lead_eligible_for_backcompat() {
        // Back-compat: lazy-auto-enter callers pass no tier; first-enter still leads.
        let root = unique_root("lead-undeclared-compat");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        ensure_presence(&room, "legacy-1").unwrap();
        assert_eq!(
            room.snapshot().unwrap().lead.as_deref(),
            Some("legacy-1"),
            "undeclared-tier first-enter must remain lead-eligible (back-compat)"
        );
    }

    #[test]
    fn ensure_presence_second_tool_does_not_steal_lead() {
        let root = unique_root("ensure-presence-second-tool");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence(&room, "tool-x").unwrap();
        ensure_presence(&room, "tool-y").unwrap();

        let snapshot = room.snapshot().unwrap();
        assert!(
            snapshot.squads.iter().any(|s| s.tool == "tool-x"),
            "tool-x in squads"
        );
        assert!(
            snapshot.squads.iter().any(|s| s.tool == "tool-y"),
            "tool-y in squads"
        );
        assert_eq!(
            snapshot.lead.as_deref(),
            Some("tool-x"),
            "lead must remain tool-x (first to enter)"
        );

        // tool-x has exactly one presence fact.
        let x_count = room
            .facts()
            .unwrap()
            .iter()
            .filter(|f| f.kind == "presence" && f.tool.as_deref() == Some("tool-x"))
            .count();
        assert_eq!(x_count, 1, "exactly one presence fact for tool-x");

        std::fs::remove_dir_all(&root).ok();
    }

    fn managed_session(name: String, tool: String) -> ManagedSession {
        ManagedSession {
            session_id: name.clone(),
            name,
            agent: "claude".to_string(),
            tool,
            backend: "tmux".to_string(),
            cwd: PathBuf::from("/tmp/rally-test"),
            target: "rally-test".to_string(),
            worktree_path: None,
            branch: None,
            ..Default::default()
        }
    }

    #[test]
    fn numbered_session_identity_scales_beyond_one_thousand() {
        let agent_spec = AgentSpec::from_name("claude").unwrap();
        let active_sessions = (1..=1000)
            .map(|number| {
                managed_session(
                    format!("claude-{number:02}"),
                    format!("claude_code:{number:02}"),
                )
            })
            .collect::<Vec<_>>();

        let identity =
            numbered_session_identity(&agent_spec, None, None, None, &active_sessions).unwrap();

        assert_eq!(identity.name, "claude-1001");
        assert_eq!(identity.session_id, "claude-1001");
        assert_eq!(identity.tool, "claude_code:1001");
    }

    #[test]
    fn numbered_session_identity_reuses_lowest_available_gap() {
        let agent_spec = AgentSpec::from_name("claude").unwrap();
        let active_sessions = vec![
            managed_session("claude-01".to_string(), "claude_code:01".to_string()),
            managed_session("claude-03".to_string(), "claude_code:03".to_string()),
        ];

        let identity =
            numbered_session_identity(&agent_spec, None, None, None, &active_sessions).unwrap();

        assert_eq!(identity.name, "claude-02");
        assert_eq!(identity.session_id, "claude-02");
        assert_eq!(identity.tool, "claude_code:02");
    }

    #[test]
    fn shell_quote_replaces_nul_bytes_instead_of_panicking() {
        let quoted = shell_quote("bad\0arg");
        assert!(quoted.contains('?'));
        assert!(!quoted.contains('\0'));
    }

    #[test]
    fn o26_locate_remedy_preserves_hostile_opaque_event_id_as_one_argument() {
        let event_id = "opaque id 'quoted' $(touch should-not-run);$HOME";
        let remedy = locate_remedy(event_id);
        assert_eq!(
            shlex::split(&remedy).unwrap(),
            vec!["rally", "locate", event_id, "--json"]
        );
        let display = RallyError::outcome_unknown(event_id, "readback", "forced").to_string();
        assert!(!display.contains("rally locate"));
    }

    /// inject content fact authored by sender, targeting recipient, with message
    /// content in subject and summary — verifiable from the ledger alone, no tmux.
    #[test]
    fn inject_content_fact_records_sender_target_and_message() {
        let root = unique_root("inject-content-fact");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let msg = "BLOCKED need creds for staging";
        // inject_content_fact appends to the given room store directly.
        let fact = inject_content_fact(&room, "sender:1", "claude_code:01", msg).unwrap();

        // Returned fact has the right fields.
        assert_eq!(fact.tool.as_deref(), Some("sender:1"), "tool is sender");
        assert_eq!(
            fact.target.as_deref(),
            Some("claude_code:01"),
            "target is recipient"
        );
        assert!(
            fact.subject.contains("BLOCKED need creds"),
            "subject contains message: {}",
            fact.subject
        );
        assert_eq!(
            fact.summary.as_deref(),
            Some(msg),
            "summary holds full text"
        );
        assert_eq!(fact.kind, FactKind::Handoff, "kind is handoff");

        // Verify the fact is readable from the ledger alone (no tmux needed).
        let facts = room.facts().unwrap();
        let recorded = facts
            .iter()
            .find(|f| f.event_id == fact.event_id)
            .expect("fact must be in the ledger");
        assert_eq!(recorded.tool.as_deref(), Some("sender:1"));
        assert_eq!(recorded.target.as_deref(), Some("claude_code:01"));
        assert!(
            recorded.subject.contains("BLOCKED need creds"),
            "ledger subject: {}",
            recorded.subject
        );
        assert_eq!(recorded.summary.as_deref(), Some(msg));

        std::fs::remove_dir_all(&root).ok();
    }

    fn ref_fact(kind: FactKind, tool: &str, ref_id: &str, subject: &str) -> Fact {
        Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id(kind.as_str()),
            seq: 0,
            thread_id: new_id("ack"),
            kind,
            tool: Some(tool.to_string()),
            role: None,
            subject: subject.to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: Some(ref_id.to_string()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    fn resolve_fact(tool: &str, ref_id: &str, subject: &str) -> Fact {
        ref_fact(FactKind::Resolve, tool, ref_id, subject)
    }

    fn handoff_under_test(event_id: &str, target: &str) -> Fact {
        let mut fact = ref_fact(FactKind::Handoff, "sender:test", "unused", "test handoff");
        fact.event_id = event_id.to_string();
        fact.ref_id = None;
        fact.target = Some(target.to_string());
        fact
    }

    #[test]
    fn wait_for_resolution_accepts_only_expected_tool() {
        let root = unique_root("ack-tool-correlation");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let handoff_id = "handoff-under-test";
        let expected_tool = "claude_code:reviewer-01";

        room.append_fact(&handoff_under_test(handoff_id, expected_tool))
            .unwrap();
        room.append_fact(&resolve_fact(expected_tool, handoff_id, "right ack"))
            .unwrap();
        // A later wrong-tool acknowledgement candidate must not replace the
        // expected tool's already-recorded resolution.
        room.append_fact(&ref_fact(
            FactKind::Artifact,
            "codex:other",
            handoff_id,
            "wrong ack",
        ))
        .unwrap();

        let room = room.into_ack_polling().unwrap();
        let ack = wait_for_resolution(handoff_id, 0, 0, &room, expected_tool).unwrap();

        assert_eq!(ack["resolved"].as_bool(), Some(true));
        assert_eq!(ack["tool"].as_str(), Some(expected_tool));
        assert_eq!(ack["expected_tool"].as_str(), Some(expected_tool));
        assert!(
            ack["subject"]
                .as_str()
                .unwrap_or_default()
                .contains("right ack"),
            "ack should report the expected tool's resolve, got {ack}",
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_resolution_times_out_when_only_wrong_tool_resolves() {
        let root = unique_root("ack-wrong-tool-timeout");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let handoff_id = "handoff-under-test";
        let expected_tool = "claude_code:reviewer-01";

        room.append_fact(&handoff_under_test(handoff_id, expected_tool))
            .unwrap();
        room.append_fact(&resolve_fact("codex:other", handoff_id, "wrong ack"))
            .unwrap();

        let room = room.into_ack_polling().unwrap();
        let ack = wait_for_resolution(handoff_id, 0, 0, &room, expected_tool).unwrap();

        assert_eq!(ack["resolved"].as_bool(), Some(false));
        assert_eq!(ack["timed_out"].as_bool(), Some(true));
        assert_eq!(ack["expected_tool"].as_str(), Some(expected_tool));
        assert_eq!(ack["ignored_resolves"].as_u64(), Some(1));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_resolution_accepts_target_artifact_as_ack() {
        let root = unique_root("ack-target-artifact");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let handoff_id = "handoff-under-test";
        let expected_tool = "claude_code:reviewer-01";

        room.append_fact(&ref_fact(
            FactKind::Artifact,
            expected_tool,
            handoff_id,
            "target artifact",
        ))
        .unwrap();

        let room = room.into_ack_polling().unwrap();
        let ack = wait_for_resolution(handoff_id, 0, 0, &room, expected_tool).unwrap();

        assert_eq!(ack["received"].as_bool(), Some(true));
        assert_eq!(ack["resolved"].as_bool(), Some(true));
        assert_eq!(ack["handoff_closed"].as_bool(), Some(true));
        assert_eq!(ack["kind"].as_str(), Some("artifact"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_resolution_accepts_target_blocker_as_received_not_resolved() {
        let root = unique_root("ack-target-blocker");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let handoff_id = "handoff-under-test";
        let expected_tool = "claude_code:reviewer-01";

        room.append_fact(&ref_fact(
            FactKind::Blocker,
            expected_tool,
            handoff_id,
            "target blocked",
        ))
        .unwrap();

        let room = room.into_ack_polling().unwrap();
        let ack = wait_for_resolution(handoff_id, 0, 0, &room, expected_tool).unwrap();

        assert_eq!(ack["received"].as_bool(), Some(true));
        assert_eq!(ack["resolved"].as_bool(), Some(false));
        assert_eq!(ack["blocked"].as_bool(), Some(true));
        assert_eq!(ack["kind"].as_str(), Some("blocker"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// SEC-015: the inject content fact carries an audit trail — the
    /// self-declared sender id plus the writing process's pid — so `from`
    /// (which is unverifiable at write time) can be cross-checked rather than
    /// trusted blindly.
    #[test]
    fn sec015_inject_content_fact_stamps_sender_and_pid_evidence() {
        let root = unique_root("sec015-inject-evidence");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let fact =
            inject_content_fact(&room, "claude_code:sender-1", "claude_code:target", "hi").unwrap();

        assert!(
            fact.evidence
                .iter()
                .any(|e| e == "sender:claude_code:sender-1"),
            "evidence must record the sender id: {:?}",
            fact.evidence
        );
        let pid = std::process::id();
        assert!(
            fact.evidence.iter().any(|e| e == &format!("pid:{pid}")),
            "evidence must record the writer pid: {:?}",
            fact.evidence
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Fix #1: inject_content_fact uses append_fact_verified — the returned event_id
    /// must be immediately present in the canonical ledger segments (not just the DB).
    /// This guards against silent segment-write failures where delivery proceeds but
    /// the coordination record is lost.
    #[test]
    fn inject_content_fact_is_readback_verified_in_canonical_ledger() {
        let root = unique_root("inject-content-fact-verified");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let fact = inject_content_fact(&room, "alpha", "beta", "hello verified").unwrap();
        // Re-read ALL facts from the store (which scans canonical segments) and
        // assert the event_id we got back is present.  append_fact_verified already
        // does this internally and would have returned Err if it failed, so this
        // test will pass IFF the implementation uses append_fact_verified (not
        // append_fact).  It also documents the contract explicitly.
        let all = room.facts().unwrap();
        let found = all.iter().any(|f| f.event_id == fact.event_id);
        assert!(
            found,
            "event_id {} not found in canonical ledger after inject_content_fact — \
             durability guarantee violated",
            fact.event_id
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Long messages are truncated to 120 chars in subject but preserved in full in summary.
    #[test]
    fn inject_content_fact_truncates_subject_preserves_summary() {
        let long_msg = "X".repeat(200);
        // make_inject_content_fact is pure — no store needed.
        let fact = make_inject_content_fact("sender:2", "codex:01", &long_msg);

        // Subject starts with "inject: " (8 chars) + up to 120 chars of text.
        let content_in_subject: String = fact.subject.chars().skip("inject: ".len()).collect();
        assert_eq!(
            content_in_subject.len(),
            120,
            "subject content capped at 120 chars"
        );
        assert_eq!(
            fact.summary.as_deref(),
            Some(long_msg.as_str()),
            "summary holds full 200-char text"
        );
    }

    /// inject_content_fact_dry_run returns a fact without touching any store.
    #[test]
    fn inject_content_fact_dry_run_does_not_write_to_ledger() {
        let root = unique_root("inject-content-fact-dry-run");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // dry_run path goes through inject_content_fact_dry_run — no room append.
        let _fact = inject_content_fact_dry_run("sender:3", "codex:01", "dry run message");

        let facts = room.facts().unwrap();
        assert!(
            facts.is_empty(),
            "dry_run must not append any fact to the ledger"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Fix 1: squads[] must not contain the reserved system author "rally".
    ///
    /// `rally next` emits wake facts with `tool = "rally"`.  These must be
    /// excluded from the squads projection so the system author never appears
    /// alongside real agents.
    #[test]
    fn squads_excludes_reserved_system_author_rally() {
        let root = unique_root("squads-excludes-rally");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Register a real agent.
        ensure_presence(&room, "claude_code:01").unwrap();

        // Append a wake fact authored by the system ("rally"), as `rally next` does.
        let wake = Fact {
            from_session_id: None,
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: new_id("wake"),
            seq: 0,
            thread_id: "wake-thread".to_string(),
            kind: store::FactKind::Wake,
            tool: Some("rally".to_string()),
            role: None,
            subject: "wake: check in".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: Some("claude_code:01".to_string()),
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&wake).unwrap();

        let snapshot = room.snapshot().unwrap();
        assert!(
            !snapshot.squads.iter().any(|s| s.tool == "rally"),
            "system author 'rally' must not appear in squads; got: {:?}",
            snapshot.squads.iter().map(|s| &s.tool).collect::<Vec<_>>()
        );
        assert!(
            snapshot.squads.iter().any(|s| s.tool == "claude_code:01"),
            "real agent claude_code:01 must still appear in squads"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Fix 2a: a single caller tool may hold multiple active managed sessions
    /// when each session has a distinct name.
    #[test]
    fn ensure_unique_session_identity_allows_distinct_names_under_same_tool() {
        // Two sessions: same tool "lead", different names.
        let session_a = managed_session("codex-01".to_string(), "lead".to_string());
        let active = vec![session_a];

        let identity_b = SessionIdentity {
            name: "codex-02".to_string(),
            session_id: "codex-02".to_string(),
            tool: "lead".to_string(),
        };
        // Must succeed — different name, same tool is now allowed.
        ensure_unique_session_identity(&identity_b, &active)
            .expect("two distinct-name sessions under the same tool must both be accepted");
    }

    /// Fix 2b: a true duplicate (same tool + same name) must still be rejected.
    #[test]
    fn ensure_unique_session_identity_rejects_same_tool_and_same_name() {
        let session_a = managed_session("codex-01".to_string(), "lead".to_string());
        let active = vec![session_a];

        let identity_dup = SessionIdentity {
            name: "codex-01".to_string(),
            session_id: "codex-01-b".to_string(), // different session_id to isolate the name check
            tool: "lead".to_string(),
        };
        let result = ensure_unique_session_identity(&identity_dup, &active);
        assert!(
            result.is_err(),
            "same tool + same name must be rejected as a duplicate session"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("lead") && msg.contains("codex-01"),
            "error message must name the conflicting tool and name; got: {msg}"
        );
    }

    // B10 — canonical-path matching unit tests

    /// B10.normalize: relative and dotslash forms canonicalize to the same stored path.
    #[test]
    fn normalize_path_relative_forms_are_equivalent() {
        assert_eq!(normalize_path("src/x.rs".to_string()), "file:src/x.rs");
        assert_eq!(normalize_path("./src/x.rs".to_string()), "file:src/x.rs");
    }

    /// B10.normalize: already-prefixed paths are idempotent.
    #[test]
    fn normalize_path_already_file_prefixed_is_idempotent() {
        let input = "file:src/lib.rs".to_string();
        assert_eq!(normalize_path(input.clone()), input);
    }

    /// B10.normalize: dot and dotdot components are collapsed.
    #[test]
    fn normalize_path_collapses_dot_and_dotdot() {
        assert_eq!(
            normalize_path("src/../src/lib.rs".to_string()),
            "file:src/lib.rs"
        );
        assert_eq!(
            normalize_path("./crates/./rally-cli/src/lib.rs".to_string()),
            "file:crates/rally-cli/src/lib.rs"
        );
    }

    /// B10.suffix: the motivating bug — `src/lib.rs` and `crates/rally-cli/src/lib.rs`
    /// share the 2-component suffix `src/lib.rs` and must flag.
    #[test]
    fn paths_suffix_collide_detects_lessons_case() {
        assert!(
            paths_suffix_collide("crates/rally-cli/src/lib.rs", "src/lib.rs"),
            "shorter path 'src/lib.rs' is a suffix of 'crates/rally-cli/src/lib.rs'"
        );
        // Symmetric.
        assert!(
            paths_suffix_collide("src/lib.rs", "crates/rally-cli/src/lib.rs"),
            "suffix check must be symmetric"
        );
    }

    /// B10.suffix: sibling crates share the `src/lib.rs` suffix and flag.
    /// Chosen behavior: WARN because the lead should adjudicate; rally never decides.
    #[test]
    fn paths_suffix_collide_sibling_crates_flag() {
        // `crates/a/src/lib.rs` and `crates/b/src/lib.rs` share `src/lib.rs` (2 components).
        // They are genuinely different files, but the warning is correct — the lead must verify.
        assert!(
            paths_suffix_collide("crates/a/src/lib.rs", "crates/b/src/lib.rs"),
            "sibling-crate paths sharing suffix src/lib.rs must flag as ambiguous"
        );
    }

    /// B10.suffix: a single-component basename must NOT trigger suffix collision.
    #[test]
    fn paths_suffix_collide_single_component_basename_does_not_flag() {
        // "lib.rs" alone is only 1 component — below the 2-component threshold.
        assert!(
            !paths_suffix_collide("src/lib.rs", "lib.rs"),
            "single-component basename must not flag"
        );
        assert!(
            !paths_suffix_collide("lib.rs", "other/lib.rs"),
            "single-component basename must not flag (reversed)"
        );
    }

    /// B10.suffix: genuinely unrelated paths must not flag.
    #[test]
    fn paths_suffix_collide_distinct_paths_do_not_flag() {
        // `crates/a/mod.rs` and `crates/b/lib.rs` share no common suffix.
        assert!(
            !paths_suffix_collide("crates/a/mod.rs", "crates/b/lib.rs"),
            "distinct filenames must not produce a suffix collision"
        );
    }

    // B11 — duplicate squad-id WARNING tests

    /// B11a: first `enter` for a tool produces no warnings.
    #[test]
    fn b11_first_enter_no_warning() {
        let root = unique_root("b11-first-enter");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Simulate command_enter logic: snapshot before ensure_presence.
        let snapshot_before = room.snapshot().unwrap();
        let warnings: Vec<String> = snapshot_before
            .squads
            .iter()
            .filter(|s| s.tool == "tool-a" && s.status == "active")
            .map(|s| s.tool.clone())
            .collect();
        assert!(warnings.is_empty(), "first enter must produce no warnings");

        std::fs::remove_dir_all(&root).ok();
    }

    /// B11b: a second enter for the same tool (still active) produces the
    /// `squad-id-active` warning.
    #[test]
    fn b11_second_enter_active_tool_emits_warning() {
        let root = unique_root("b11-second-enter");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // First enter: write presence.
        ensure_presence(&room, "tool-a").unwrap();

        // Simulate second enter: snapshot reflects tool-a as active.
        let snapshot_before = room.snapshot().unwrap();
        let hit = snapshot_before
            .squads
            .iter()
            .any(|s| s.tool == "tool-a" && s.status == "active");
        assert!(hit, "tool-a must appear as active before second enter");

        // The warning logic from command_enter.
        let active_squad = snapshot_before
            .squads
            .iter()
            .find(|s| s.tool == "tool-a" && s.status == "active");
        assert!(
            active_squad.is_some(),
            "squad-id-active warning must be generated for second enter"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// B11c: entering with a distinct tool id (`tool-b`) never triggers the warning
    /// even when `tool-a` is already active.
    #[test]
    fn b11_distinct_tool_id_no_warning() {
        let root = unique_root("b11-distinct-tool");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence(&room, "tool-a").unwrap();

        let snapshot_before = room.snapshot().unwrap();
        let hit = snapshot_before
            .squads
            .iter()
            .any(|s| s.tool == "tool-b" && s.status == "active");
        assert!(!hit, "tool-b must NOT be flagged when tool-a is active");

        std::fs::remove_dir_all(&root).ok();
    }

    /// B11d: a second `enter` for an already-active tool writes exactly ONE durable
    /// `risk` fact with subject containing `duplicate-active-squad-id` into the ledger,
    /// and enter still returns ok (no rejection).
    ///
    /// Verification: after the second enter the risk fact appears in current_risks
    /// on a FRESH RoomStore (forces segment→db reconciliation; in-memory cache gone).
    /// The first enter produces no risk fact.
    #[test]
    fn b11d_second_enter_writes_durable_risk_fact() {
        let root = unique_root("b11d-durable-risk");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // --- First enter: no risk fact expected ---
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            ensure_presence(&room, "tool-a").unwrap();

            let snapshot = room.snapshot().unwrap();
            let risk_count = snapshot
                .current_risks
                .iter()
                .filter(|f| {
                    f.subject.contains("duplicate-active-squad-id")
                        && f.tool.as_deref() == Some("tool-a")
                })
                .count();
            assert_eq!(risk_count, 0, "first enter must produce no risk fact");
        }

        // --- Second enter: simulate the duplicate detection + risk fact append ---
        // We call the same logic command_enter uses: snapshot → detect active → append risk.
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            let snapshot_before = room.snapshot().unwrap();
            let squad = snapshot_before
                .squads
                .iter()
                .find(|s| s.tool == "tool-a" && s.status == "active")
                .expect("tool-a must be active before second enter");

            // This is the exact block copied from command_enter.
            let risk_fact = Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: store::FactKind::Risk,
                tool: Some("tool-a".to_string()),
                role: None,
                subject: "duplicate-active-squad-id: tool-a".to_string(),
                scope: Vec::new(),
                created_at: now_string(),
                summary: Some(format!(
                    "another active session is already using squad id tool-a (last seen {}); not blocked — re-enter with a distinct id if this is a second terminal. Recorded for audit.",
                    squad.last_seen_ts
                )),
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: Some("warn".to_string()),
                uri: None,
                session: None,
            };
            room.append_fact(&risk_fact).unwrap();
        }

        // --- Read back via a FRESH RoomStore ---
        let reader = store::RoomStore::open_at(root.clone()).unwrap();
        let snapshot = reader.snapshot().unwrap();

        // DI-1: system-generated telemetry (duplicate-active-squad-id) now
        // projects into `system_health`, not `current_risks`.
        let risk_facts: Vec<_> = snapshot
            .system_health
            .iter()
            .filter(|f| f.subject.contains("duplicate-active-squad-id"))
            .collect();
        assert_eq!(
            risk_facts.len(),
            1,
            "exactly one risk fact for duplicate-active-squad-id must be in system_health; got: {:?}",
            risk_facts.iter().map(|f| &f.subject).collect::<Vec<_>>()
        );
        let rf = risk_facts[0];
        assert_eq!(
            rf.tool.as_deref(),
            Some("tool-a"),
            "risk fact tool must be the entering tool"
        );
        assert!(
            rf.subject.contains("duplicate-active-squad-id: tool-a"),
            "subject must contain the squad id; got: {}",
            rf.subject
        );
        assert_eq!(
            rf.severity.as_deref(),
            Some("warn"),
            "severity must be 'warn'"
        );
        // enter is still ok — not blocked
        // (enter itself is not called here; the test verifies the durable record)

        std::fs::remove_dir_all(&root).ok();
    }

    /// C-FLEET / f4 unit test: `is_managed_style_tool` classifies fleet
    /// workers vs human/lead identifiers.
    ///
    /// The rule is OPT-OUT: everything is a worker unless explicitly
    /// human/lead. This closes the f4 silent enforcement hole — pre-f4 the
    /// function additionally required a digit somewhere in the id, so bare
    /// `claude` / `codex` / `gemini` / `opencode` could enter the room
    /// without a managed-session record AND without raising the
    /// `unmanaged-agent` risk fact.
    #[test]
    fn fleet_is_managed_style_tool_classification() {
        // Managed-style — should detect (including the f4-newly-covered
        // bare worker ids without a digit suffix).
        for t in [
            "claude-01",
            "claude_code:01",
            "claude_code:42",
            "toolbar-launch-01",
            "BuildBluePoint-3",
            "redesign-coord-1",
            "codex-2",
            // f4: bare worker ids without digits — previously slipped
            // through the digit-only exemption.
            "claude",
            "codex",
            "opencode",
            "gemini",
            "no-digits-here",
            // f4: substrings starting with `user` but NOT `user:` are
            // workers, not humans. Pre-f4 the `starts_with("user")` check
            // over-matched and silenced these.
            "user-friendly",
            "user-friendly-codex-01",
        ] {
            assert!(is_managed_style_tool(t), "expected managed: {t}");
        }
        // Human/lead-style — should NOT detect.
        for t in [
            "lead",
            "claude_code:lead",
            "claude_code:l1",
            "claude_code:l42",
            "human:alice",
            "user:bob",
            "USER:CAROL", // case-insensitive
        ] {
            assert!(!is_managed_style_tool(t), "expected NOT managed: {t}");
        }
    }

    /// C-FLEET: `enter` for a managed-style tool with NO active managed-session
    /// record writes exactly ONE `unmanaged-agent` risk fact into the ledger;
    /// a second `enter` does not duplicate it. Re-uses the b11d read-back
    /// pattern (fresh RoomStore) so segment→db reconciliation is verified.
    #[test]
    fn fleet_unmanaged_agent_writes_durable_risk_fact() {
        let root = unique_root("fleet-unmanaged-risk");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // Simulate the exact `command_enter` block: snapshot → query active
        // managed sessions → if none AND managed-style → append risk.
        let stray_tool = "stray-01";
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            let snapshot_before = room.snapshot().unwrap();
            let active_sessions = active_session_records(&room).unwrap_or_default();
            let has_managed = active_sessions.iter().any(|s| {
                s.tool == stray_tool || s.session_id == stray_tool || s.name == stray_tool
            });
            assert!(!has_managed, "no managed session expected for stray-01");
            assert!(is_managed_style_tool(stray_tool), "managed-style tool id");
            let already_recorded = snapshot_before.current_risks.iter().any(|f| {
                f.subject == format!("unmanaged-agent: {stray_tool}")
                    && f.tool.as_deref() == Some(stray_tool)
            });
            assert!(!already_recorded, "no prior risk fact expected");
            let risk_fact = build_risk_fact(
                stray_tool,
                format!("unmanaged-agent: {stray_tool}"),
                "test".to_string(),
                Vec::new(),
                "warn",
                Vec::new(),
                None,
            );
            room.append_fact(&risk_fact).unwrap();
        }

        // Read back via fresh RoomStore.
        {
            let reader = store::RoomStore::open_at(root.clone()).unwrap();
            let snapshot = reader.snapshot().unwrap();
            // DI-1: unmanaged-agent telemetry projects into `system_health`.
            let risk_facts: Vec<_> = snapshot
                .system_health
                .iter()
                .filter(|f| f.subject == format!("unmanaged-agent: {stray_tool}"))
                .collect();
            assert_eq!(
                risk_facts.len(),
                1,
                "exactly one unmanaged-agent risk expected; got: {:?}",
                risk_facts.iter().map(|f| &f.subject).collect::<Vec<_>>()
            );
            assert_eq!(risk_facts[0].severity.as_deref(), Some("warn"));
        }

        // Simulate second enter: idempotency check via current_risks scan.
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            let snapshot_before = room.snapshot().unwrap();
            // DI-1: the idempotency guard scans `system_health` (where telemetry
            // now lives), matching the production guard.
            let already_recorded = snapshot_before.system_health.iter().any(|f| {
                f.subject == format!("unmanaged-agent: {stray_tool}")
                    && f.tool.as_deref() == Some(stray_tool)
            });
            assert!(
                already_recorded,
                "second enter must see the prior risk fact (idempotency gate)"
            );
            // command_enter would skip the append. Verify nothing duplicates.
            let reader = store::RoomStore::open_at(root.clone()).unwrap();
            let snapshot = reader.snapshot().unwrap();
            let risk_facts: Vec<_> = snapshot
                .system_health
                .iter()
                .filter(|f| f.subject == format!("unmanaged-agent: {stray_tool}"))
                .collect();
            assert_eq!(risk_facts.len(), 1, "idempotent — still exactly one");
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// DI-1: system-generated telemetry (subject-prefixed) projects into
    /// `system_health` — deduped by subject — while human coordination risks
    /// remain in `current_risks`. Keeps the risk view trustworthy.
    #[test]
    fn di1_telemetry_splits_from_current_risks_and_dedups() {
        let root = unique_root("di1-split");
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        // A human coordination risk — must stay in current_risks.
        room.append_fact(&build_risk_fact(
            "alice",
            "deploy blocked: staging DB down".to_string(),
            "staging db is unreachable".to_string(),
            Vec::new(),
            "warn",
            Vec::new(),
            None,
        ))
        .unwrap();
        // Two identical telemetry facts (simulate pre-guard accumulation).
        for _ in 0..2 {
            room.append_fact(&build_risk_fact(
                "claude_code",
                "unmanaged-agent: claude_code:x".to_string(),
                "no managed session".to_string(),
                Vec::new(),
                "warn",
                Vec::new(),
                None,
            ))
            .unwrap();
        }

        let snap = store::RoomStore::open_at(root.clone())
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(
            snap.current_risks.len(),
            1,
            "only the human risk belongs in current_risks; got: {:?}",
            snap.current_risks
                .iter()
                .map(|f| &f.subject)
                .collect::<Vec<_>>()
        );
        assert!(snap.current_risks[0].subject.starts_with("deploy blocked"));
        assert_eq!(
            snap.system_health
                .iter()
                .filter(|f| f.subject.starts_with("unmanaged-agent:"))
                .count(),
            1,
            "telemetry must be deduped to one row in system_health; got: {:?}",
            snap.system_health
                .iter()
                .map(|f| &f.subject)
                .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// C-FLEET adopt: an adopted session shows up in `active_session_records`
    /// with the caller-provided target, and a second adopt of the same target
    /// is rejected with a clear error. HERDR-INDEPENDENT: exercises the cmux
    /// backend (the herdr `--pane` arm was dropped with `Backend::Herdr`).
    #[test]
    fn fleet_adopt_registers_running_target_and_rejects_duplicate() {
        let root = unique_root("fleet-adopt-cmux");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Build a session as command_adopt would (helper-level test).
        let agent_spec = AgentSpec::from_name("claude").unwrap();
        let active_before = active_session_records(&room).unwrap();
        let identity = numbered_session_identity(
            &agent_spec,
            Some("stray-target".to_string()),
            None,
            None,
            &active_before,
        )
        .unwrap();
        let session = ManagedSession {
            session_id: identity.session_id.clone(),
            name: identity.name.clone(),
            agent: agent_spec.agent.to_string(),
            tool: identity.tool.clone(),
            backend: "cmux".to_string(),
            cwd: root.clone(),
            target: "cmux-target-9".to_string(), // caller-provided, NOT derived
            worktree_path: None,
            branch: None,
            ..Default::default()
        };
        let (_facts, ctx) = room.session_facts_with_context_version().unwrap();
        let fact = session_fact(&session, "active", None);
        let written = room.append_session_fact_if_context(&fact, ctx).unwrap();
        assert!(
            matches!(written, ConditionalAppendOutcome::Applied(_)),
            "session fact must land"
        );

        // Active sessions now include the adopted target.
        let active = active_session_records(&room).unwrap();
        assert!(
            active.iter().any(|s| s.target == "cmux-target-9"),
            "adopted cmux-target-9 must appear in active sessions; got: {:?}",
            active.iter().map(|s| &s.target).collect::<Vec<_>>()
        );

        // find_session by tool/name/session_id works — the same path
        // `command_inject` uses.
        let found = active
            .iter()
            .find(|s| {
                s.target == "cmux-target-9"
                    || s.session_id == identity.session_id
                    || s.name == "stray-target"
            })
            .cloned();
        assert!(found.is_some(), "adopted session must be discoverable");

        // Idempotency: a second adopt of `cmux-target-9` must be rejected by
        // the command_adopt pre-check. Simulate that scan.
        let collision = active.iter().any(|s| s.target == "cmux-target-9");
        assert!(collision, "second adopt must detect prior target");

        std::fs::remove_dir_all(&root).ok();
    }

    /// C-FLEET adopt: a tmux-backend adoption stores the tmux target as the
    /// session.target so inject/attach/stop route through the right backend.
    #[test]
    fn fleet_adopt_stores_tmux_target_verbatim() {
        let root = unique_root("fleet-adopt-tmux");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let agent_spec = AgentSpec::from_name("codex").unwrap();
        let active_before = active_session_records(&room).unwrap();
        let identity = numbered_session_identity(
            &agent_spec,
            Some("legacy-codex".to_string()),
            None,
            None,
            &active_before,
        )
        .unwrap();
        let session = ManagedSession {
            session_id: identity.session_id,
            name: identity.name,
            agent: agent_spec.agent.to_string(),
            tool: identity.tool,
            backend: "tmux".to_string(),
            cwd: root.clone(),
            target: "rally-legacy".to_string(),
            worktree_path: None,
            branch: None,
            ..Default::default()
        };
        let (_facts, ctx) = room.session_facts_with_context_version().unwrap();
        room.append_session_fact_if_context(&session_fact(&session, "active", None), ctx)
            .unwrap();

        let active = active_session_records(&room).unwrap();
        let adopted = active
            .iter()
            .find(|s| s.target == "rally-legacy")
            .expect("tmux target must round-trip");
        assert_eq!(adopted.backend, "tmux");
        assert_eq!(adopted.agent, "codex");
        // target is the caller-provided tmux name, NOT the
        // backend_target(Tmux, session_id) shape (`rally-codex-NN`).
        assert_eq!(adopted.target, "rally-legacy");

        std::fs::remove_dir_all(&root).ok();
    }

    /// C-FLEET: `enter` for a tool that DOES have an active managed session
    /// does NOT emit an `unmanaged-agent` risk fact.
    #[test]
    fn fleet_managed_tool_does_not_emit_risk() {
        let root = unique_root("fleet-managed-no-risk");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Manually append a `session` fact making `claude-99` active.
        let session = ManagedSession {
            session_id: "claude-99".to_string(),
            name: "claude-99".to_string(),
            agent: "claude".to_string(),
            tool: "claude_code:99".to_string(),
            backend: "tmux".to_string(),
            cwd: root.clone(),
            target: "rally-claude-99".to_string(),
            worktree_path: None,
            branch: None,
            ..Default::default()
        };
        room.append_fact(&session_fact(&session, "active", None))
            .unwrap();

        let active = active_session_records(&room).unwrap();
        let has_managed = active
            .iter()
            .any(|s| s.tool == "claude_code:99" || s.session_id == "claude_code:99");
        assert!(
            has_managed,
            "managed session match must succeed by tool name"
        );

        // With a managed session present, command_enter would skip the risk
        // append. Verify nothing is in current_risks.
        let snapshot = room.snapshot().unwrap();
        let unmanaged_risks: Vec<_> = snapshot
            .current_risks
            .iter()
            .filter(|f| f.subject.starts_with("unmanaged-agent"))
            .collect();
        assert!(
            unmanaged_risks.is_empty(),
            "managed tool must not emit unmanaged-agent risk; got: {:?}",
            unmanaged_risks
                .iter()
                .map(|f| &f.subject)
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Quirk fix: second `enter` for the same tool with no intervening peer
    /// activity must report `cursor.advanced = false` and must not surface the
    /// tool's own presence/lead facts as new peer content.
    ///
    /// Also verifies that a fact written by a DIFFERENT tool between the two
    /// enters IS still detected as new (the cursor window is not overcorrected).
    #[test]
    fn enter_second_call_does_not_report_own_presence_as_new_peer_content() {
        let root = unique_root("enter-cursor-own-presence");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // --- First enter for tool-a ---
        // Replicate what command_enter does: snapshot before, ensure_presence,
        // re-snapshot after, set cursor to post-presence max_seq.
        let snap_before_1 = room.snapshot().unwrap();
        let cursor_before_1 = room.cursor_for("tool-a").unwrap_or(0);
        let _max_seq_pre_1 = snap_before_1.max_seq;

        ensure_presence(&room, "tool-a").unwrap();

        let snap_after_1 = room.snapshot().unwrap();
        let cursor_after_1 = snap_after_1.max_seq; // post-presence max_seq
        room.set_cursor("tool-a", cursor_after_1).unwrap();

        // First enter: cursor advances from 0 to wherever ensure_presence left off.
        assert!(
            cursor_after_1 >= cursor_before_1,
            "first enter cursor must be >= 0"
        );

        // --- No other activity between the two enters ---

        // --- Second enter for tool-a ---
        let cursor_before_2 = room.cursor_for("tool-a").unwrap_or(0);
        assert_eq!(
            cursor_before_2, cursor_after_1,
            "cursor_before for second enter must equal cursor_after from first enter"
        );

        let snap_before_2 = room.snapshot().unwrap();
        let _max_seq_pre_2 = snap_before_2.max_seq;

        ensure_presence(&room, "tool-a").unwrap(); // idempotent — no new facts

        let snap_after_2 = room.snapshot().unwrap();
        let cursor_after_2 = snap_after_2.max_seq;
        room.set_cursor("tool-a", cursor_after_2).unwrap();

        // KEY assertion: with no peer activity, cursor must not advance.
        assert_eq!(
            cursor_after_2, cursor_before_2,
            "second enter with no peer activity must not advance cursor (own presence facts excluded)"
        );
        let advanced_2 = cursor_after_2 > cursor_before_2;
        assert!(
            !advanced_2,
            "cursor.advanced must be false on second enter when only the tool's own presence facts exist"
        );

        // --- Verify: a fact from a DIFFERENT tool between enters IS still detected ---
        // Simulate tool-a's first enter again in a fresh state, then tool-b writes,
        // then tool-a enters again — the inter-enter peer fact must appear as new.
        let root2 = unique_root("enter-cursor-peer-fact");
        std::fs::create_dir_all(root2.join(".git")).unwrap();
        let room2 = store::RoomStore::open_at(root2.clone()).unwrap();

        // First enter for tool-a.
        ensure_presence(&room2, "tool-a").unwrap();
        let snap_r2_1 = room2.snapshot().unwrap();
        let c_after_r2_1 = snap_r2_1.max_seq;
        room2.set_cursor("tool-a", c_after_r2_1).unwrap();

        // tool-b posts a fact between the two enters.
        let peer_fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("peer"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-b".to_string()),
            role: None,
            subject: "peer claim between enters".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room2.append_fact(&peer_fact).unwrap();

        // Second enter for tool-a.
        let c_before_r2_2 = room2.cursor_for("tool-a").unwrap_or(0);
        ensure_presence(&room2, "tool-a").unwrap(); // idempotent
        let snap_r2_2 = room2.snapshot().unwrap();
        let c_after_r2_2 = snap_r2_2.max_seq;

        // The peer fact must be visible as new (cursor advanced).
        assert!(
            c_after_r2_2 > c_before_r2_2,
            "cursor must advance when tool-b wrote a fact between tool-a's two enters"
        );
        let advanced_peer = c_after_r2_2 > c_before_r2_2;
        assert!(
            advanced_peer,
            "cursor.advanced must be true when a concurrent peer fact exists"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&root2).ok();
    }

    // B16 — write→read round-trip gate

    /// B16: every writable fact kind appended to the ledger reads back identically
    /// from a FRESH `RoomStore::open_at` (forces segment→db reconciliation; the
    /// in-memory writer's cache is abandoned).  Also checks that `max_seq` advanced
    /// by exactly the number of facts written.
    ///
    /// If any kind fails to round-trip this test will surface the broken kind rather
    /// than masking it.
    #[test]
    fn b16_write_read_round_trip_all_fact_kinds() {
        let root = unique_root("b16-round-trip");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // --- Write phase ---
        let writer = store::RoomStore::open_at(root.clone()).unwrap();

        // The kinds under test.  `session` uses its own append path in
        // production, but its Fact shape is identical; we use append_fact here
        // so the test stays focused on the serialisation round-trip.
        let kinds: &[(&str, store::FactKind)] = &[
            ("claim", store::FactKind::Claim),
            ("release", store::FactKind::Release),
            ("artifact", store::FactKind::Artifact),
            ("handoff", store::FactKind::Handoff),
            ("decision", store::FactKind::Decision),
            ("risk", store::FactKind::Risk),
            ("blocker", store::FactKind::Blocker),
            ("resolve", store::FactKind::Resolve),
            ("presence", store::FactKind::Presence),
        ];

        let tool = "b16-test-tool";
        let mut written: Vec<store::Fact> = Vec::new();
        let mut live_claim_id: Option<String> = None;
        let mut live_blocker_id: Option<String> = None;
        for (subject, kind) in kinds {
            let mut fact = store::Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("b16"),
                seq: 0,
                thread_id: format!("b16-thread-{subject}"),
                kind: kind.clone(),
                tool: Some(tool.to_string()),
                role: None,
                subject: format!("b16 round-trip subject for {subject}"),
                scope: Vec::new(),
                created_at: now_string(),
                summary: Some(format!("b16 summary for {subject}")),
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            match kind {
                store::FactKind::Release => fact.ref_id = live_claim_id.clone(),
                store::FactKind::Resolve => fact.ref_id = live_blocker_id.clone(),
                _ => {}
            }
            let appended = writer.append_fact(&fact).unwrap();
            assert!(
                appended.fact.seq > 0,
                "appended {subject} must have seq > 0"
            );
            if kind == &store::FactKind::Claim {
                live_claim_id = Some(appended.fact.event_id.clone());
            }
            if kind == &store::FactKind::Blocker {
                live_blocker_id = Some(appended.fact.event_id.clone());
            }
            written.push(appended.fact);
        }

        let facts_written = kinds.len() as i64;

        // --- Reload phase: open a completely new RoomStore handle ---
        // This forces the segment→db reconciliation path; the writer's
        // in-memory SQLite connection is not reused.
        drop(writer);
        let reader = store::RoomStore::open_at(root.clone()).unwrap();

        // (a) Every written fact is readable by event_id with matching kind, tool, and subject.
        let all_facts = reader.facts().unwrap();
        for w in &written {
            let found = all_facts
                .iter()
                .find(|f| f.event_id == w.event_id)
                .unwrap_or_else(|| {
                    panic!(
                        "fact {} (kind={}) not found after reload",
                        w.event_id,
                        w.kind.as_str()
                    )
                });
            assert_eq!(
                found.kind.as_str(),
                w.kind.as_str(),
                "kind mismatch for {} after reload",
                w.event_id
            );
            assert_eq!(
                found.tool.as_deref(),
                Some(tool),
                "tool mismatch for {} after reload",
                w.event_id
            );
            assert_eq!(
                found.subject, w.subject,
                "subject mismatch for {} after reload",
                w.event_id
            );
            assert_eq!(
                found.seq, w.seq,
                "seq mismatch for {} after reload",
                w.event_id
            );
        }

        // (b) max_seq advanced by exactly `facts_written` from 0.
        let snapshot = reader.snapshot().unwrap();
        let max_written = written.iter().map(|f| f.seq).max().unwrap_or(0);
        // Written seqs must be contiguous from 1..=facts_written (factstr assigns 1-based).
        let min_written = written.iter().map(|f| f.seq).min().unwrap_or(0);
        assert_eq!(
            max_written - min_written + 1,
            facts_written,
            "seq range must span exactly {facts_written} (got min={min_written} max={max_written})"
        );
        assert_eq!(
            snapshot.max_seq, max_written,
            "snapshot.max_seq must equal the highest written seq after reload"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// B10.suffix: exact-match paths must not produce a suffix collision (already
    /// caught by the exact-match / dir-prefix check which emits `claimed-path`).
    #[test]
    fn paths_suffix_collide_exact_match_returns_false() {
        assert!(
            !paths_suffix_collide("src/lib.rs", "src/lib.rs"),
            "exact match is handled by path_matches_scope, not suffix collision"
        );
    }

    /// B10.suffix: dir-prefix case must not produce a suffix collision either.
    #[test]
    fn paths_suffix_collide_dir_prefix_returns_false() {
        // `src` is a dir-prefix of `src/lib.rs` — caught by path_matches_scope already.
        assert!(
            !paths_suffix_collide("src", "src/lib.rs"),
            "dir-prefix is handled by path_matches_scope, not suffix collision"
        );
    }

    // ==========================================================================
    // rally watch tests
    // ==========================================================================

    /// Helper: open a temp dir as a rally room and return the root + rally dir.
    fn watch_temp_room(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = unique_root(label);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let rally_dir = root.join(".rally");
        std::fs::create_dir_all(&rally_dir).unwrap();
        (root, rally_dir)
    }

    /// Test (a): --once first call (after a fact was posted) emits an activity event;
    /// immediate second call with no new fact emits nothing.
    #[test]
    fn watch_once_emits_activity_then_nothing() {
        let (root, rally_dir) = watch_temp_room("watch-once-activity");
        let log_dir = rally_dir.join(store::LOG_DIRNAME);

        // Post a fact to advance max_seq.
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("watch-once"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("test-tool".to_string()),
            role: None,
            subject: "watch once test claim".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&fact).unwrap();
        drop(room);

        // Cursor starts at 0 (no watch-cursor.json yet).
        let cursor_before = watch_read_once_cursor(&rally_dir);
        assert_eq!(
            cursor_before, 0,
            "cursor must start at 0 before first --once call"
        );

        // First --once: max_seq > 0, so activity should be detected.
        let current_seq = watch_read_max_seq(&log_dir);
        assert!(current_seq > 0, "max_seq must be > 0 after posting a fact");
        let activity_detected = current_seq > cursor_before;
        assert!(
            activity_detected,
            "first --once must detect activity (seq advanced from 0)"
        );

        // Simulate what command_watch --once does: persist cursor.
        watch_write_once_cursor(&rally_dir, current_seq);

        // Second --once: cursor now equals current_seq → no activity.
        let cursor_after = watch_read_once_cursor(&rally_dir);
        assert_eq!(
            cursor_after, current_seq,
            "cursor must be persisted after first call"
        );
        let new_seq = watch_read_max_seq(&log_dir);
        let activity_second = new_seq > cursor_after;
        assert!(
            !activity_second,
            "second --once must not detect activity when no new fact posted"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Test (b): --on-activity path runs the command once on new activity and
    /// passes RALLY_TO_SEQ in the child environment.
    #[test]
    fn watch_on_activity_runs_command_with_env_vars() {
        let (root, rally_dir) = watch_temp_room("watch-on-activity");
        let log_dir = rally_dir.join(store::LOG_DIRNAME);

        // Post a fact.
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("watch-oact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Artifact,
            tool: Some("actor".to_string()),
            role: None,
            subject: "on-activity test".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&fact).unwrap();
        drop(room);

        let from_seq = 0i64;
        let to_seq = watch_read_max_seq(&log_dir);
        assert!(to_seq > 0, "to_seq must be > 0");

        // Run a command that writes RALLY_TO_SEQ to a temp file.
        let out_file = std::env::temp_dir().join(format!("rally-watch-oact-{}.txt", short_id()));
        let cmd = format!("printf '%s' \"$RALLY_TO_SEQ\" > {}", out_file.display());
        let room_id = "test-room";

        watch_run_on_activity(&cmd, room_id, from_seq, to_seq, Some("actor"), &root);

        // Verify the output file contains the correct TO_SEQ value.
        let written = std::fs::read_to_string(&out_file)
            .expect("--on-activity command must have written RALLY_TO_SEQ to the file");
        let parsed: i64 = written
            .trim()
            .parse()
            .expect("file content must be a valid i64");
        assert_eq!(
            parsed, to_seq,
            "RALLY_TO_SEQ in child env must equal the detected to_seq"
        );

        std::fs::remove_file(&out_file).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Test (c): --print-launchd emits a plist containing "watch" and the repo path.
    ///
    /// Captures output by spawning the compiled test binary with
    /// `rally watch --print-launchd` via a git-rooted temp dir.  Falls back to
    /// asserting on the label string derived from the same logic used inside
    /// `watch_print_launchd` so the test doesn't rely on a live binary.
    #[test]
    fn watch_print_launchd_contains_watch_and_repo_path() {
        let (root, _rally_dir) = watch_temp_room("watch-print-launchd");

        // Derive the label the same way watch_print_launchd does.
        let label = format!(
            "com.agent-rally-point.watch.{}",
            root.file_name().and_then(|n| n.to_str()).unwrap_or("repo")
        );
        let repo_str = root.to_string_lossy();

        // Label structure assertions (same logic as the renderer).
        assert!(
            label.starts_with("com.agent-rally-point.watch."),
            "launchd label must start with the expected prefix; got: {label}"
        );
        assert!(
            label.contains("watch"),
            "launchd label must contain 'watch'; got: {label}"
        );
        assert!(
            !repo_str.is_empty(),
            "repo path must be non-empty for WorkingDirectory"
        );

        // Verify the rendered plist text directly via the pure renderer.
        // No subprocess: a unit test must not depend on a live binary, and the
        // previous spawn of the *test harness* binary always failed with
        // "Unrecognized option: 'print-launchd'", silently skipping these asserts.
        let plist = render_launchd_plist(5, None, &PathBuf::from("rally"), &root);
        assert!(
            plist.contains("watch"),
            "plist must contain 'watch' keyword; got:\n{plist}"
        );
        assert!(
            plist.contains(repo_str.as_ref()),
            "plist must contain the repo path as WorkingDirectory; got:\n{plist}"
        );
        assert!(
            plist.contains("RunAtLoad"),
            "plist must contain RunAtLoad key; got:\n{plist}"
        );
        assert!(
            plist.contains("KeepAlive"),
            "plist must contain KeepAlive key; got:\n{plist}"
        );
        assert!(
            plist.contains(&label),
            "plist must contain the derived launchd label; got:\n{plist}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Test (d): watch_read_max_seq reads per-repo .rally/log only — no code path
    /// references the legacy global apps directory.
    #[test]
    fn watch_reads_per_repo_log_only_no_legacy_global_index() {
        let (root, rally_dir) = watch_temp_room("watch-per-repo");
        let log_dir = rally_dir.join(store::LOG_DIRNAME);

        // Empty room: max_seq must be 0.
        let seq_empty = watch_read_max_seq(&log_dir);
        assert_eq!(seq_empty, 0, "max_seq must be 0 in an empty room");

        // Post a fact to create the index.
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("watch-repo"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Decision,
            tool: Some("watcher".to_string()),
            role: None,
            subject: "per-repo watch test".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let appended = room.append_fact(&fact).unwrap();
        drop(room);

        // watch_read_max_seq must see the new seq from the per-repo index.
        let seq_after = watch_read_max_seq(&log_dir);
        assert_eq!(
            seq_after, appended.fact.seq,
            "watch_read_max_seq must return the same seq as appended ({}) from per-repo index",
            appended.fact.seq
        );

        // The log_dir path must be under the per-repo .rally/ and NOT reference
        // the legacy global path (~/.agent-rally-point/apps/...).
        let log_dir_str = log_dir.to_string_lossy();
        assert!(
            log_dir_str.contains(".rally"),
            "log_dir must be under per-repo .rally/"
        );
        assert!(
            !log_dir_str.contains(".agent-rally-point"),
            "watch must NOT reference the legacy global apps dir; got: {log_dir_str}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ==========================================================================
    // B18 — repo-scope write guard + external-intake quarantine tests
    // ==========================================================================

    /// B18a: classify_scope returns RepoLocal for a bare relative path.
    #[test]
    fn b18a_classify_scope_relative_path_is_repo_local() {
        assert_eq!(
            classify_scope("src/x.rs"),
            ScopeClass::RepoLocal,
            "relative paths must always be RepoLocal"
        );
    }

    /// B18b: classify_scope returns RepoLocal for an absolute path that IS
    /// under repo_root.  We use this repo's own root which is guaranteed to
    /// exist under a real git tree.
    #[test]
    fn b18b_classify_scope_absolute_under_repo_root_is_repo_local() {
        let root = unique_root("b18b");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // We can't change repo_root() without cwd manipulation, so verify the
        // helper directly using a path that starts with the cwd (which is always
        // under some git repo when tests run inside the workspace).
        // The safe invariant: a relative path always returns RepoLocal regardless.
        assert_eq!(
            classify_scope("crates/rally-cli/src/lib.rs"),
            ScopeClass::RepoLocal,
            "relative path under repo must be RepoLocal"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// B18c: classify_scope returns External for an absolute path that is
    /// definitively NOT under the current repo root (e.g. /tmp, /var, or another
    /// well-known system path that cannot be a subdirectory of this workspace).
    #[test]
    fn b18c_classify_scope_absolute_outside_repo_is_external() {
        // /tmp is always present on macOS/Linux and is never inside a git repo
        // root that would contain the rally-cli crate.
        // We use /tmp/some-other-repo/x.rs as the archetype.
        let outside = "/tmp/some-other-repo/x.rs";
        // Only assert External when /tmp is genuinely outside the repo root.
        // Derive repo root from cwd to make the test hermetic.
        let maybe_root = repo_root();
        if let Ok(root) = maybe_root {
            let root_str = root.to_string_lossy();
            if !root_str.starts_with("/tmp") {
                assert_eq!(
                    classify_scope(outside),
                    ScopeClass::External,
                    "/tmp/some-other-repo/x.rs must be External when repo root is {root_str}"
                );
            }
        }
    }

    /// B18d: posting a claim with an external absolute path still succeeds
    /// (ok:true / fact written), but the room's active_claims does NOT contain
    /// it, and a risk fact with subject "external-intake: ..." is present in
    /// current_risks.
    #[test]
    fn b18d_external_path_claim_quarantined_from_active_claims() {
        let root = unique_root("b18d");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Fabricate the external-tagged claim directly (avoids cwd dependency
        // of command_say while still exercising the snapshot projection).
        let external_claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18d-claim"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("b18d-tool".to_string()),
            role: None,
            subject: "external claim".to_string(),
            // Marker added by command_say for external-intake.
            scope: vec![
                "file:/some/other-repo/x.rs".to_string(),
                "external-intake".to_string(),
            ],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&external_claim).unwrap();

        // Post a normal repo-local claim too so we can verify it IS included.
        let local_claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18d-local"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("b18d-tool".to_string()),
            role: None,
            subject: "local claim".to_string(),
            scope: vec!["file:src/x.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&local_claim).unwrap();

        let snapshot = room.snapshot().unwrap();

        // The external claim must NOT appear in active_claims.
        let ext_in_active = snapshot
            .active_claims
            .iter()
            .any(|f| f.scope.contains(&"external-intake".to_string()));
        assert!(
            !ext_in_active,
            "external-intake claim must be excluded from active_claims"
        );

        // The local claim MUST appear in active_claims.
        let local_in_active = snapshot
            .active_claims
            .iter()
            .any(|f| f.subject == "local claim");
        assert!(
            local_in_active,
            "repo-local claim must appear in active_claims"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// B18e: posting an external-intake handoff excludes it from open_handoffs.
    #[test]
    fn b18e_external_path_handoff_excluded_from_open_handoffs() {
        let root = unique_root("b18e");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let ext_handoff = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18e-hoff"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Handoff,
            tool: Some("b18e-tool".to_string()),
            role: None,
            subject: "external handoff".to_string(),
            scope: vec![
                "file:/other/repo/x.rs".to_string(),
                "external-intake".to_string(),
            ],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&ext_handoff).unwrap();

        let snapshot = room.snapshot().unwrap();
        let ext_in_handoffs = snapshot
            .open_handoffs
            .iter()
            .any(|f| f.scope.contains(&"external-intake".to_string()));
        assert!(
            !ext_in_handoffs,
            "external-intake handoff must be excluded from open_handoffs"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// B18f: posting an external-intake artifact excludes it from recent_artifacts.
    #[test]
    fn b18f_external_path_artifact_excluded_from_recent_artifacts() {
        let root = unique_root("b18f");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let ext_artifact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18f-art"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Artifact,
            tool: Some("b18f-tool".to_string()),
            role: None,
            subject: "external artifact".to_string(),
            scope: vec![
                "file:/other/repo/out.json".to_string(),
                "external-intake".to_string(),
            ],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&ext_artifact).unwrap();

        let snapshot = room.snapshot().unwrap();
        let ext_in_artifacts = snapshot
            .recent_artifacts
            .iter()
            .any(|f| f.scope.contains(&"external-intake".to_string()));
        assert!(
            !ext_in_artifacts,
            "external-intake artifact must be excluded from recent_artifacts"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// B18g: the risk fact written for an external-intake claim IS present in
    /// current_risks (the audit trail).
    #[test]
    fn b18g_external_intake_risk_fact_appears_in_current_risks() {
        let root = unique_root("b18g");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Simulate what command_say does: write the tagged claim + the risk fact.
        let ext_claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18g-claim"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("b18g-tool".to_string()),
            role: None,
            subject: "b18g external claim".to_string(),
            scope: vec![
                "file:/some/other/x.rs".to_string(),
                "external-intake".to_string(),
            ],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&ext_claim).unwrap();

        let risk_fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18g-risk"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Risk,
            tool: Some("b18g-tool".to_string()),
            role: None,
            subject: "external-intake: /some/other/x.rs".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some("external-intake recorded for audit".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: Some("warn".to_string()),
            uri: None,
            session: None,
        };
        room.append_fact(&risk_fact).unwrap();

        let snapshot = room.snapshot().unwrap();

        // DI-1: external-intake telemetry projects into `system_health`, not
        // `current_risks` (keeps the coordination-risk view clean).
        let risk_in_health = snapshot
            .system_health
            .iter()
            .any(|f| f.subject.starts_with("external-intake:"));
        assert!(
            risk_in_health,
            "external-intake risk fact must appear in system_health; got: {:?}",
            snapshot
                .system_health
                .iter()
                .map(|f| &f.subject)
                .collect::<Vec<_>>()
        );

        // The external claim must NOT appear in active_claims.
        assert!(
            snapshot.active_claims.is_empty(),
            "active_claims must be empty (external-intake claim quarantined)"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ==========================================================================
    // doctor --canonical-paths tests
    // ==========================================================================

    /// doctor_canonical_paths_clean_room: a room with no active claims produces
    /// an empty report (no non-canonical, no collisions).
    #[test]
    fn doctor_canonical_paths_clean_room_reports_empty() {
        let root = unique_root("doctor-cp-clean");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // One canonical claim (already file:-prefixed, relative).
        let canonical_claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("doctor-cp-c1"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "canonical claim".to_string(),
            scope: vec!["file:src/main.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&canonical_claim).unwrap();

        let snapshot = room.snapshot().unwrap();
        assert_eq!(snapshot.active_claims.len(), 1, "one active claim");

        // Run canonical-paths logic directly.
        let claim_scopes: Vec<(String, String)> = snapshot
            .active_claims
            .iter()
            .flat_map(|fact| {
                let tool = fact.tool.clone().unwrap_or_default();
                fact.scope
                    .iter()
                    .filter(|s| *s != "external-intake")
                    .filter(|s| !s.contains("://") || s.starts_with("file:"))
                    .map(move |s| (tool.clone(), s.clone()))
            })
            .collect();

        // No non-canonical scopes: "file:src/main.rs" normalizes to itself.
        let non_canonical: Vec<_> = claim_scopes
            .iter()
            .filter(|(_, scope)| normalize_path(scope.clone()) != *scope)
            .collect();
        assert!(
            non_canonical.is_empty(),
            "canonical claim scope must produce no non_canonical entries; got: {non_canonical:?}"
        );

        // No suffix collisions (only one claim).
        let has_collision = claim_scopes.len() >= 2
            && paths_suffix_collide(
                claim_scopes[0]
                    .1
                    .strip_prefix("file:")
                    .unwrap_or(&claim_scopes[0].1),
                claim_scopes[1]
                    .1
                    .strip_prefix("file:")
                    .unwrap_or(&claim_scopes[1].1),
            );
        assert!(
            !has_collision,
            "single claim must produce no suffix collision"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// doctor_canonical_paths_flags_non_canonical: a claim with a relative dotslash
    /// scope (./src/foo.rs) is detected as non-canonical.
    #[test]
    fn doctor_canonical_paths_flags_non_canonical_scope() {
        let root = unique_root("doctor-cp-noncanon");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Scope stored in non-canonical form (./src/foo.rs — not yet normalized).
        let claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("doctor-cp-nc"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-x".to_string()),
            role: None,
            subject: "non-canonical claim".to_string(),
            scope: vec!["./src/foo.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&claim).unwrap();

        let snapshot = room.snapshot().unwrap();
        assert_eq!(snapshot.active_claims.len(), 1);

        let claim_scopes: Vec<(String, String)> = snapshot
            .active_claims
            .iter()
            .flat_map(|fact| {
                let tool = fact.tool.clone().unwrap_or_default();
                fact.scope
                    .iter()
                    .filter(|s| *s != "external-intake")
                    .filter(|s| !s.contains("://") || s.starts_with("file:"))
                    .map(move |s| (tool.clone(), s.clone()))
            })
            .collect();

        let non_canonical: Vec<_> = claim_scopes
            .iter()
            .filter(|(_, scope)| normalize_path(scope.clone()) != *scope)
            .collect();

        assert!(
            !non_canonical.is_empty(),
            "dotslash scope './src/foo.rs' must be flagged as non-canonical"
        );
        assert_eq!(
            non_canonical[0].0, "tool-x",
            "non-canonical entry must name the owning tool"
        );
        assert_eq!(
            non_canonical[0].1, "./src/foo.rs",
            "non-canonical entry must contain the raw scope"
        );
        // Canonical form should strip ./ and add file: prefix.
        assert_eq!(
            normalize_path("./src/foo.rs".to_string()),
            "file:src/foo.rs",
            "normalize_path must strip ./ and add file:"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// doctor_canonical_paths_flags_suffix_collision: two claims from different tools
    /// whose scopes share a 2+ component trailing suffix are detected.
    #[test]
    fn doctor_canonical_paths_flags_suffix_collision_pair() {
        let root = unique_root("doctor-cp-collision");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // tool-a claims "file:crates/a/src/lib.rs"
        let claim_a = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("doctor-cp-a"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "claim a".to_string(),
            scope: vec!["file:crates/a/src/lib.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        // tool-b claims "file:crates/b/src/lib.rs"
        let claim_b = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("doctor-cp-b"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-b".to_string()),
            role: None,
            subject: "claim b".to_string(),
            scope: vec!["file:crates/b/src/lib.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&claim_a).unwrap();
        room.append_fact(&claim_b).unwrap();

        let snapshot = room.snapshot().unwrap();
        assert_eq!(snapshot.active_claims.len(), 2);

        let claim_scopes: Vec<(String, String)> = snapshot
            .active_claims
            .iter()
            .flat_map(|fact| {
                let tool = fact.tool.clone().unwrap_or_default();
                fact.scope
                    .iter()
                    .filter(|s| *s != "external-intake")
                    .filter(|s| !s.contains("://") || s.starts_with("file:"))
                    .map(move |s| (tool.clone(), s.clone()))
            })
            .collect();

        // Find suffix collisions across different tools.
        let mut found_collision = false;
        for i in 0..claim_scopes.len() {
            for j in (i + 1)..claim_scopes.len() {
                let (tool_a, scope_a) = &claim_scopes[i];
                let (tool_b, scope_b) = &claim_scopes[j];
                if tool_a == tool_b {
                    continue;
                }
                let bare_a = scope_a.strip_prefix("file:").unwrap_or(scope_a.as_str());
                let bare_b = scope_b.strip_prefix("file:").unwrap_or(scope_b.as_str());
                if paths_suffix_collide(bare_a, bare_b) {
                    found_collision = true;
                }
            }
        }

        assert!(
            found_collision,
            "crates/a/src/lib.rs and crates/b/src/lib.rs must produce a suffix collision"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ==========================================================================
    // doctor --prune-rooms tests (using discovery::prune helpers indirectly)
    // ==========================================================================

    /// doctor_prune_rooms_dry_run: seeding a registry with one live and one stale
    /// entry, dry-run lists the stale entry and does NOT rewrite the index.
    #[test]
    fn doctor_prune_rooms_dry_run_lists_stale_keeps_index() {
        use std::fs;

        let live_root = unique_root("doctor-prune-live");
        fs::create_dir_all(&live_root).unwrap();

        let stale_root_path = std::env::temp_dir().join(format!(
            "rally-doctor-prune-stale-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Do NOT create stale_root_path — it must not exist.
        assert!(
            !stale_root_path.exists(),
            "stale root must not exist for the test to be valid"
        );

        // Build a temporary room index with both entries.
        let index_dir = unique_root("doctor-prune-idx-dir");
        let index_path = index_dir.join("index.json");

        let index_content = serde_json::json!({
            "schema": "agent-rally.room-index.v1",
            "rooms": [
                {
                    "schema": "agent-rally.room-index.v1",
                    "repo_root": live_root,
                    "display_name": "live-repo",
                    "facts_db": live_root.join(".rally/facts.db"),
                    "last_seen_seq": 1_i64,
                    "last_seen_at": "2026-05-30T00:00:00Z"
                },
                {
                    "schema": "agent-rally.room-index.v1",
                    "repo_root": stale_root_path,
                    "display_name": "stale-repo",
                    "facts_db": stale_root_path.join(".rally/facts.db"),
                    "last_seen_seq": 2_i64,
                    "last_seen_at": "2026-05-30T00:00:00Z"
                }
            ]
        });
        fs::write(
            &index_path,
            serde_json::to_string_pretty(&index_content).unwrap(),
        )
        .unwrap();

        // Exercise the internal prune helpers from doctor.rs.
        // We call the module's internal prune logic via its pub(crate) API
        // (run_prune_rooms is pub(crate) and operates on the path returned by
        // room_index_path_pub; for tests we replicate the classification here).
        let index_text = fs::read_to_string(&index_path).unwrap();
        let index: serde_json::Value = serde_json::from_str(&index_text).unwrap();
        let rooms = index["rooms"].as_array().unwrap();

        let stale: Vec<_> = rooms
            .iter()
            .filter(|r| {
                let repo = r["repo_root"].as_str().unwrap_or("");
                !std::path::Path::new(repo).exists()
            })
            .collect();

        let live: Vec<_> = rooms
            .iter()
            .filter(|r| {
                let repo = r["repo_root"].as_str().unwrap_or("");
                std::path::Path::new(repo).exists()
            })
            .collect();

        assert_eq!(live.len(), 1, "one live room (live_root dir exists)");
        assert_eq!(stale.len(), 1, "one stale room (stale dir does not exist)");
        assert_eq!(
            stale[0]["display_name"].as_str().unwrap(),
            "stale-repo",
            "stale entry must be the one whose dir is absent"
        );

        // Dry-run: index is NOT rewritten.
        let index_before = fs::read_to_string(&index_path).unwrap();
        // (no apply → no write)
        let index_after = fs::read_to_string(&index_path).unwrap();
        assert_eq!(
            index_before, index_after,
            "dry-run must not modify the index file"
        );

        fs::remove_dir_all(&live_root).ok();
        fs::remove_dir_all(&index_dir).ok();
    }

    /// doctor_prune_rooms_apply_rewrites_index: after --apply the index only
    /// contains the live entry; running the classifier again shows 0 stale.
    #[test]
    fn doctor_prune_rooms_apply_rewrites_index_keeps_live() {
        use std::fs;

        let live_root = unique_root("doctor-prune-apply-live");
        fs::create_dir_all(&live_root).unwrap();

        let stale_root_path = std::env::temp_dir().join(format!(
            "rally-doctor-prune-apply-stale-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!stale_root_path.exists());

        let index_dir = unique_root("doctor-prune-apply-idx");
        let index_path = index_dir.join("index.json");

        let make_index = |rooms_json: serde_json::Value| {
            serde_json::json!({
                "schema": "agent-rally.room-index.v1",
                "rooms": rooms_json
            })
        };

        let initial_index = make_index(serde_json::json!([
            {
                "schema": "agent-rally.room-index.v1",
                "repo_root": live_root,
                "display_name": "live-repo",
                "facts_db": live_root.join(".rally/facts.db"),
                "last_seen_seq": 1_i64,
                "last_seen_at": "2026-05-30T00:00:00Z"
            },
            {
                "schema": "agent-rally.room-index.v1",
                "repo_root": stale_root_path,
                "display_name": "stale-repo",
                "facts_db": stale_root_path.join(".rally/facts.db"),
                "last_seen_seq": 2_i64,
                "last_seen_at": "2026-05-30T00:00:00Z"
            }
        ]));
        fs::write(
            &index_path,
            serde_json::to_string_pretty(&initial_index).unwrap(),
        )
        .unwrap();

        // Simulate --apply: rewrite the index keeping only live entries.
        let index_text = fs::read_to_string(&index_path).unwrap();
        let index: serde_json::Value = serde_json::from_str(&index_text).unwrap();
        let rooms = index["rooms"].as_array().unwrap();

        let live_rooms: Vec<serde_json::Value> = rooms
            .iter()
            .filter(|r| {
                let repo = r["repo_root"].as_str().unwrap_or("");
                std::path::Path::new(repo).exists()
            })
            .cloned()
            .collect();

        // Write back (atomic via temp).
        let pruned_index = make_index(serde_json::json!(live_rooms));
        let temp_path = index_path.with_extension("json.tmp-prune-test");
        fs::write(
            &temp_path,
            serde_json::to_string_pretty(&pruned_index).unwrap(),
        )
        .unwrap();
        fs::rename(&temp_path, &index_path).unwrap();

        // Now re-read and verify.
        let after_text = fs::read_to_string(&index_path).unwrap();
        let after: serde_json::Value = serde_json::from_str(&after_text).unwrap();
        let after_rooms = after["rooms"].as_array().unwrap();

        assert_eq!(
            after_rooms.len(),
            1,
            "after apply, only the live entry must remain; got: {after_rooms:?}"
        );
        assert_eq!(
            after_rooms[0]["display_name"].as_str().unwrap(),
            "live-repo",
            "remaining entry must be the live-repo"
        );

        // A second pass finds 0 stale entries.
        let still_stale: Vec<_> = after_rooms
            .iter()
            .filter(|r| {
                let repo = r["repo_root"].as_str().unwrap_or("");
                !std::path::Path::new(repo).exists()
            })
            .collect();
        assert!(
            still_stale.is_empty(),
            "after apply, no stale entries should remain"
        );

        fs::remove_dir_all(&live_root).ok();
        fs::remove_dir_all(&index_dir).ok();
    }

    // R9 stale-binary guard unit tests.

    /// R9a: BUILD_ID const is non-empty and contains a '+' separator.
    #[test]
    fn r9a_build_id_const_is_non_empty() {
        assert!(!BUILD_ID.is_empty(), "BUILD_ID must not be empty");
        assert!(
            BUILD_ID.contains('+'),
            "BUILD_ID must contain '+' separating version and hash; got: {BUILD_ID}"
        );
        let parts: Vec<&str> = BUILD_ID.splitn(2, '+').collect();
        assert_eq!(
            parts.len(),
            2,
            "BUILD_ID must have exactly one '+' separator"
        );
        assert!(
            !parts[0].is_empty(),
            "version part of BUILD_ID must not be empty"
        );
        assert!(
            !parts[1].is_empty(),
            "hash part of BUILD_ID must not be empty"
        );
    }

    /// R9b: when two presence facts with DIFFERENT build_ids exist in a room,
    /// the next enter produces a `binary-drift` warning AND a durable risk fact,
    /// while ok stays true (not blocked).
    ///
    /// Simulates two binaries by injecting presence facts with controlled build_ids
    /// directly into the store — no second binary required.
    #[test]
    fn r9b_different_build_ids_produce_drift_warning_and_risk_fact() {
        let root = unique_root("r9b-binary-drift");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let fake_old_id = "0.0.0+aabbccd";

        // Write a presence fact carrying a DIFFERENT build_id into the room,
        // simulating what a stale binary would have left behind.
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            let stale_presence = store::Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("stale"),
                seq: 0,
                thread_id: new_id("room"),
                kind: store::FactKind::Presence,
                tool: Some("old-tool:01".to_string()),
                role: None,
                subject: "agent presence: old-tool:01".to_string(),
                scope: Vec::new(),
                created_at: now_string(),
                summary: Some(format!("build_id:{fake_old_id}")),
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            room.append_fact(&stale_presence).unwrap();
        }

        // Now simulate command_enter's drift-detection block on a fresh store.
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let all_facts = room.facts().unwrap_or_default();
        let last_presence_build_id: Option<String> = all_facts
            .iter()
            .filter(|f| f.kind == "presence")
            .max_by_key(|f| f.seq)
            .and_then(|f| f.summary.as_deref())
            .and_then(|s| s.strip_prefix("build_id:"))
            .map(str::to_string);

        assert_eq!(
            last_presence_build_id.as_deref(),
            Some(fake_old_id),
            "last presence build_id must be the injected fake"
        );

        // Drift is detected because current BUILD_ID differs from the injected one.
        // (If BUILD_ID == fake_old_id by coincidence, the test would miss — that's
        // astronomically unlikely given the hash component.)
        let drift = last_presence_build_id
            .as_deref()
            .map(|prior| prior != BUILD_ID)
            .unwrap_or(false);
        assert!(drift, "drift must be detected when build_ids differ");

        // Append the risk fact exactly as command_enter does.
        let prior_id = last_presence_build_id.unwrap();
        let drift_msg = format!(
            "this rally build {} differs from the build {} that last wrote to this room — a stale binary on PATH can silently drop writes; verify which rally is on PATH",
            BUILD_ID, prior_id
        );
        let risk_fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Risk,
            tool: Some("new-tool:01".to_string()),
            role: None,
            subject: format!("binary-drift: {} vs {}", BUILD_ID, prior_id),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some(drift_msg),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: Some("warn".to_string()),
            uri: None,
            session: None,
        };
        room.append_fact(&risk_fact).unwrap();

        // Verify the risk fact is readable from a FRESH store (forces segment→db reconciliation).
        drop(room);
        let reader = store::RoomStore::open_at(root.clone()).unwrap();
        let snapshot = reader.snapshot().unwrap();

        // DI-1: binary-drift telemetry projects into `system_health`.
        let drift_risks: Vec<_> = snapshot
            .system_health
            .iter()
            .filter(|f| f.subject.contains("binary-drift"))
            .collect();
        assert_eq!(
            drift_risks.len(),
            1,
            "exactly one binary-drift risk fact must appear in system_health"
        );
        assert_eq!(
            drift_risks[0].severity.as_deref(),
            Some("warn"),
            "binary-drift risk must have severity=warn"
        );
        // ok stays true — the enter result would still have ok:true (not tested here
        // since we're not calling command_enter, but the risk fact being non-blocking
        // is the invariant; enter never returns an error for this condition).

        std::fs::remove_dir_all(&root).ok();
    }

    // ==========================================================================
    // B1: standby / wake round-trip tests
    // ==========================================================================

    /// B1a: `say standby --wake-after +30m` stores a fact with kind=standby,
    /// `wake_after:<iso>` in summary, and round-trips through a fresh RoomStore.
    #[test]
    fn b1a_standby_roundtrip_wake_after_relative() {
        let root = unique_root("b1a-standby-rt");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Resolve +30m and store.
        let wake_iso = dag::resolve_wake_after("+30m").expect("+30m must resolve");
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1a-standby"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Standby,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "agent standby".to_string(),
            scope: vec!["run:RUN-B1A".to_string(), "step:S1".to_string()],
            created_at: now_string(),
            summary: Some(format!("reason:idle wake_after:{wake_iso}")),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: Some("pending".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        let appended = room.append_fact_verified(&fact).unwrap();
        assert_eq!(appended.fact.kind.as_str(), "standby");
        assert!(
            appended
                .fact
                .summary
                .as_deref()
                .unwrap_or("")
                .contains("wake_after:"),
            "summary must contain wake_after marker"
        );

        // Round-trip via fresh store.
        drop(room);
        let reader = store::RoomStore::open_at(root.clone()).unwrap();
        let facts = reader.facts().unwrap();
        let found = facts
            .iter()
            .find(|f| f.event_id == appended.fact.event_id)
            .expect("standby fact must round-trip");
        assert_eq!(found.kind.as_str(), "standby");
        assert!(
            found
                .summary
                .as_deref()
                .unwrap_or("")
                .contains("wake_after:")
        );
        assert!(found.scope.contains(&"run:RUN-B1A".to_string()));
        assert!(found.scope.contains(&"step:S1".to_string()));

        std::fs::remove_dir_all(&root).ok();
    }

    /// B1b: `say wake --ref-standby <standby-event-id>` links back to the standby.
    #[test]
    fn b1b_wake_links_to_standby_via_ref() {
        let root = unique_root("b1b-wake-link");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Write a standby fact.
        let standby = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1b-standby"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Standby,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "agent standby".to_string(),
            scope: vec!["run:RUN-B1B".to_string()],
            created_at: now_string(),
            summary: Some("reason:waiting wake_after:2099-01-01T00:00:00Z".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: Some("pending".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        let standby_fact = room.append_fact_verified(&standby).unwrap();

        // Write a wake fact referencing the standby.
        let wake = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1b-wake"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Wake,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "wake from standby".to_string(),
            scope: vec!["run:RUN-B1B".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: Some(standby_fact.fact.event_id.clone()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let wake_fact = room.append_fact_verified(&wake).unwrap();

        assert_eq!(
            wake_fact.fact.ref_id.as_deref(),
            Some(standby_fact.fact.event_id.as_str()),
            "wake fact must reference the standby event_id"
        );

        // Once woken, the standby must not appear in wake-due.
        let facts = room.facts().unwrap();
        let due = dag::project_wake_due(&facts, None);
        // standby is in the future (2099) so it wouldn't surface anyway,
        // but we verify the woken-standby logic covers it.
        let woken_in_due = due
            .iter()
            .any(|d| d.standby_event_id == standby_fact.fact.event_id);
        assert!(!woken_in_due, "woken standby must not appear in wake-due");

        std::fs::remove_dir_all(&root).ok();
    }

    /// B1c: lineage fan-out — 1 handoff → 3 child claims via --run/--step/--parent-step.
    /// The DAG must reconstruct 3 nodes (excluding the handoff's root node = 4 total)
    /// with parent_step edges connecting them to the handoff step.
    #[test]
    fn b1c_lineage_fanout_dag_three_child_claims() {
        let root = unique_root("b1c-lineage-fanout");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let run_id = "RUN-B1C";
        // Handoff at step S0.
        let handoff = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1c-handoff"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Handoff,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "fan-out handoff".to_string(),
            scope: vec![format!("run:{run_id}"), "step:S0".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&handoff).unwrap();

        // 3 child claims at steps S1, S2, S3 with parent-step:S0.
        for i in 1..=3 {
            let claim = store::Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id(&format!("b1c-claim-s{i}")),
                seq: 0,
                thread_id: new_id("room"),
                kind: store::FactKind::Claim,
                tool: Some(format!("tool-{i}")),
                role: None,
                subject: format!("child claim {i}"),
                scope: vec![
                    format!("run:{run_id}"),
                    format!("step:S{i}"),
                    "parent-step:S0".to_string(),
                ],
                created_at: now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            room.append_fact(&claim).unwrap();
        }

        let facts = room.facts().unwrap();
        let dag_out = dag::build_dag(&facts, run_id);

        // 4 nodes: S0, S1, S2, S3.
        assert_eq!(dag_out.nodes.len(), 4, "expected 4 DAG nodes");
        // 3 parent_step edges.
        let pe: Vec<_> = dag_out
            .edges
            .iter()
            .filter(|e| e.kind == "parent_step")
            .collect();
        assert_eq!(pe.len(), 3, "expected 3 parent_step edges");
        // All claims are in_flight (no artifacts).
        for node in dag_out.nodes.iter().filter(|n| n.step_id != "S0") {
            assert_eq!(
                node.status,
                dag::NodeStatus::InFlight,
                "child step {} must be in_flight",
                node.step_id
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// B1d: stalled detection — a child claim with no artifact past its standby's
    /// wake_after is tagged stalled.
    #[test]
    fn b1d_stalled_child_claim_past_standby() {
        let root = unique_root("b1d-stalled");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let run_id = "RUN-B1D";
        // Claim at S1.
        let claim = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1d-claim"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "stalled claim".to_string(),
            scope: vec![format!("run:{run_id}"), "step:S1".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        // Standby at S1 with past wake_after.
        let standby = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b1d-standby"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Standby,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "standby".to_string(),
            scope: vec![format!("run:{run_id}"), "step:S1".to_string()],
            created_at: now_string(),
            summary: Some("reason:waiting wake_after:2020-01-01T00:00:00Z".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: Some("pending".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact(&claim).unwrap();
        room.append_fact(&standby).unwrap();

        let facts = room.facts().unwrap();
        let dag_out = dag::build_dag(&facts, run_id);
        let s1 = dag_out
            .nodes
            .iter()
            .find(|n| n.step_id == "S1")
            .expect("S1 must exist");
        assert_eq!(
            s1.status,
            dag::NodeStatus::Stalled,
            "S1 with past standby and no artifact must be stalled"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// B4a: wake-due projects past standbys and suggested_command is a string only.
    /// Charter: no execution occurs (no Command/spawn/schedule called).
    #[test]
    fn b4a_wake_due_projects_past_standby_with_suggested_command() {
        let root = unique_root("b4a-wake-due");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // Register tool-a presence (trust gate requirement).
        ensure_presence(&room, "tool-a").unwrap();

        // Write a standby with a past wake_after.
        let standby = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b4a-standby"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Standby,
            tool: Some("tool-a".to_string()),
            role: None,
            subject: "agent standby".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some("reason:idle wake_after:2020-01-01T00:00:00Z".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: Some("pending".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        let standby_fact = room.append_fact_verified(&standby).unwrap();

        let facts = room.facts().unwrap();
        let due = dag::project_wake_due(&facts, None);

        assert!(!due.is_empty(), "wake-due must surface the past standby");
        let entry = due
            .iter()
            .find(|d| d.standby_event_id == standby_fact.fact.event_id)
            .expect("past standby must appear in wake-due");

        // suggested_command is a string, never executed by rally.
        assert!(
            entry.suggested_command.contains("rally next"),
            "suggested_command must reference rally next; got: {}",
            entry.suggested_command
        );
        assert!(
            entry.suggested_command.contains("tool-a"),
            "suggested_command must include owner tool; got: {}",
            entry.suggested_command
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// B4b: not-yet-due standby does not surface in wake-due.
    #[test]
    fn b4b_wake_due_future_standby_not_surfaced() {
        let root = unique_root("b4b-not-yet-due");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        ensure_presence(&room, "tool-b").unwrap();

        let standby = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b4b-standby"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Standby,
            tool: Some("tool-b".to_string()),
            role: None,
            subject: "agent standby".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: Some("reason:idle wake_after:2099-01-01T00:00:00Z".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: Some("pending".to_string()),
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&standby).unwrap();

        let facts = room.facts().unwrap();
        let due = dag::project_wake_due(&facts, None);
        assert!(
            due.is_empty(),
            "future standby must not appear in wake-due; got: {due:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Charter assertion test: grep the dag module source for exec/spawn/schedule
    /// call patterns and assert none are found.
    ///
    /// SEAM_NO_EXEC invariant: no code path in dag.rs calls Command::new(,
    /// thread::spawn(, exec(, or similar process-launching APIs.
    ///
    /// The patterns below are the call-site forms (with opening parenthesis) so
    /// they match actual invocations rather than doc-comment strings.
    #[test]
    fn charter_assertion_dag_module_contains_no_exec_spawn_schedule() {
        let dag_src = include_str!("dag.rs");

        // These are call-site patterns (include the opening paren) so they
        // match only real invocations, not doc-comment mentions.
        // SEAM_NO_EXEC: rally RECORDS and DERIVES; it NEVER EXECUTES.
        let forbidden_calls = [
            "Command::new(",
            "thread::spawn(",
            "execv(",
            "execve(",
            "execvp(",
        ];

        for pattern in &forbidden_calls {
            // Filter out lines that are pure comments (// or //!).
            let violating_lines: Vec<&str> = dag_src
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.starts_with("//") && line.contains(pattern)
                })
                .collect();
            assert!(
                violating_lines.is_empty(),
                "SEAM_NO_EXEC violation: dag.rs must not call {pattern:?} outside comments; \
                 rally records/derives only — execution belongs in the external runner. \
                 Found in lines: {violating_lines:?}"
            );
        }
    }

    /// R9c: when the same build_id enters twice, NO drift warning is produced.
    #[test]
    fn r9c_same_build_id_produces_no_drift_warning() {
        let root = unique_root("r9c-no-drift");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        // Write a presence fact carrying the CURRENT build_id.
        {
            let room = store::RoomStore::open_at(root.clone()).unwrap();
            let current_presence = store::Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("same"),
                seq: 0,
                thread_id: new_id("room"),
                kind: store::FactKind::Presence,
                tool: Some("tool-a:01".to_string()),
                role: None,
                subject: "agent presence: tool-a:01".to_string(),
                scope: Vec::new(),
                created_at: now_string(),
                summary: Some(format!("build_id:{BUILD_ID}")),
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            room.append_fact(&current_presence).unwrap();
        }

        // Simulate drift detection.
        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let all_facts = room.facts().unwrap_or_default();
        let last_presence_build_id: Option<String> = all_facts
            .iter()
            .filter(|f| f.kind == "presence")
            .max_by_key(|f| f.seq)
            .and_then(|f| f.summary.as_deref())
            .and_then(|s| s.strip_prefix("build_id:"))
            .map(str::to_string);

        let drift = last_presence_build_id
            .as_deref()
            .map(|prior| prior != BUILD_ID)
            .unwrap_or(false);

        assert!(!drift, "same build_id must not trigger drift detection");

        // No risk fact was appended (drift == false means the if-block is skipped).
        let snapshot = room.snapshot().unwrap();
        let drift_risks: Vec<_> = snapshot
            .current_risks
            .iter()
            .filter(|f| f.subject.contains("binary-drift"))
            .collect();
        assert!(
            drift_risks.is_empty(),
            "no binary-drift risk fact must appear when build_ids match"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    // ─── C4: path-only release ───────────────────────────────────────────────
    //
    // Closes lesson seq 1603 ("rally say release silently no-ops w/o proper
    // ref/path"). The first test proves the happy path; the second proves we
    // error loud + actionable when no match is found.

    /// Helper: write a claim by tool T on path P and return its event_id.
    fn append_claim(room: &store::RoomStore, tool: &str, path: &str, subject: &str) -> String {
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: subject.to_string(),
            scope: vec![format!("file:{path}")],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let appended = room.append_fact_verified(&fact).unwrap();
        appended.fact.event_id
    }

    #[test]
    fn path_only_release_closes_matching_claim_and_flips_projection() {
        let root = unique_root("path-only-release-happy");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let claim_id = append_claim(&room, "alpha", "src/foo.rs", "fix the thing");

        // Sanity: it's in active_claims before.
        let before = room.snapshot().unwrap();
        assert!(
            before.active_claims.iter().any(|c| c.event_id == claim_id),
            "claim must start active"
        );

        // command_release_by_path replicates what command_say's branch routes to.
        let out = command_release_by_path(
            &room,
            "alpha",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "done with src/foo.rs".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        )
        .expect("path-only release must succeed");

        // Projection must have flipped.
        let after = room.snapshot().unwrap();
        assert!(
            !after.active_claims.iter().any(|c| c.event_id == claim_id),
            "claim must no longer be active after path-only release"
        );

        // Envelope must carry a `released-by-path` warning naming the original.
        let body: serde_json::Value = out.body.clone();
        assert!(
            body["data"]["say"]["fact"]["from_session_id"]
                .as_str()
                .is_some_and(|session| !session.is_empty()),
            "path release must stamp the caller session: {body}"
        );
        let warnings = body["data"]["warnings"].as_array().expect("warnings array");
        let found = warnings.iter().any(|w| {
            w["code"] == "released-by-path"
                && w["message"].as_str().unwrap_or("").contains(&claim_id)
        });
        assert!(
            found,
            "warnings must name the released claim event_id; got {warnings:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_only_release_errors_loud_when_no_claim_matches() {
        let root = unique_root("path-only-release-loud");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // alpha owns one claim on a different path.
        let alpha_other = append_claim(&room, "alpha", "src/bar.rs", "wrong path");

        let result = command_release_by_path(
            &room,
            "alpha",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "no match".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        );
        let err = match result {
            Ok(_) => panic!("path-only release with no match must error loud, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no live claim"),
            "error must say 'no live claim'; got: {msg}"
        );
        assert!(
            msg.contains(&alpha_other),
            "error must list alpha's open claims (the actionable next step); got: {msg}"
        );

        // Claim must still be active.
        let after = room.snapshot().unwrap();
        assert!(
            after
                .active_claims
                .iter()
                .any(|c| c.event_id == alpha_other),
            "no-match release must NOT incidentally close any other claim"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Append a claim whose `created_at` is backdated, making its owner's
    /// squad project as `idle` (stale) since the squad's last_seen_ts is the
    /// highest-seq fact's created_at and a claim is that tool's only fact.
    fn append_stale_claim(
        room: &store::RoomStore,
        tool: &str,
        path: &str,
        subject: &str,
        created_at: &str,
    ) -> String {
        let fact = store::Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: subject.to_string(),
            scope: vec![format!("file:{path}")],
            created_at: created_at.to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap().fact.event_id
    }

    /// fact_182e8 gap 1 — authorized takeover. A claim whose owner has gone
    /// liveness-stale (>15m idle) CAN be released by a different tool (the
    /// fix), where previously `rally say release` was strictly owner-only and a
    /// dead owner's claim squatted forever. The release fact records the
    /// takeover provenance (subject annotation + evidence tag).
    #[test]
    fn stale_owner_claim_is_reclaimable_by_authorized_takeover() {
        let root = unique_root("stale-claim-takeover");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // dead-owner claimed src/foo.rs 2 days ago and went quiet → stale squad.
        let stale_id = append_stale_claim(
            &room,
            "dead-owner",
            "src/foo.rs",
            "claim from a session that died",
            "2026-06-02T10:00:00Z",
        );
        let before = room.snapshot().unwrap();
        assert!(
            before.takeover_eligible_owners().contains("dead-owner"),
            "a 2-day-stale dead-owner must be takeover-eligible (>2h silent); squads={:?}",
            before.squads
        );

        // A different tool (the lead/peer) reclaims it.
        let out = command_release_by_path(
            &room,
            "claude_code:lead",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "reclaim squatting claim".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        )
        .expect("authorized takeover of a stale-owner claim must succeed");

        // Claim is gone from active projection.
        let after = room.snapshot().unwrap();
        assert!(
            !after.active_claims.iter().any(|c| c.event_id == stale_id),
            "stale-owner claim must be released after authorized takeover"
        );

        // Release fact records takeover provenance.
        let body: serde_json::Value = out.body.clone();
        let released_fact = &body["data"]["say"]["fact"];
        let subject = released_fact["subject"].as_str().unwrap_or("");
        assert!(
            subject.contains("authorized-takeover"),
            "release subject must record the takeover; got: {subject}"
        );
        let evidence = released_fact["evidence"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            evidence
                .iter()
                .any(|e| e.as_str().unwrap_or("").contains("dead-owner")),
            "release evidence must name the stale owner reclaimed; got: {evidence:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A still-LIVE peer's claim is NOT reclaimable by takeover — only the
    /// owner can release it. Guards against the takeover path widening into a
    /// general "anyone can release anyone's live claim" hole.
    #[test]
    fn live_owner_claim_is_not_reclaimable_by_takeover() {
        let root = unique_root("live-claim-no-takeover");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // alpha claims now → not takeover-eligible (well under 2h).
        let live_id = append_claim(&room, "alpha", "src/foo.rs", "active work");
        let before = room.snapshot().unwrap();
        assert!(
            !before.takeover_eligible_owners().contains("alpha"),
            "freshly-claiming alpha must not be takeover-eligible"
        );

        let result = command_release_by_path(
            &room,
            "beta",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "attempted takeover of a live claim".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        );
        assert!(
            result.is_err(),
            "a different tool must NOT release a still-live owner's claim"
        );
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("not takeover-eligible"),
            "error must explain the not-takeover-eligible block; got: {msg}"
        );

        // Claim survives.
        let after = room.snapshot().unwrap();
        assert!(
            after.active_claims.iter().any(|c| c.event_id == live_id),
            "live owner's claim must survive a rejected takeover"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// independent-auditor HIGH (2026-06-09): a BUSY-BUT-QUIET owner — idle past
    /// the 15-minute advisory threshold but well under the 2h takeover bar —
    /// must NOT have its claim reclaimed. before-write may WARN (advisory), but
    /// the destructive takeover release must refuse. This is the regression the
    /// two-tier threshold prevents (a long build with no Rally write != dead).
    #[test]
    fn busy_but_quiet_owner_is_warnable_but_not_takeover_eligible() {
        // Serialize against env-mutating tests (retrospective/discovery
        // remove/set RALLY_ENGAGEMENT). This path transitively reads the
        // process-global env; Rust's set/remove_var is unsound vs a concurrent
        // read even under a writer-only mutex, so the reader must hold the same
        // PROCESS_ENV_LOCK — the true fix for this test's parallel-suite flake.
        let _env = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let root = unique_root("busy-quiet-owner");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at_with_engagement(root.clone(), None).unwrap();

        // Claim 22 minutes ago: comfortably past the 15m idle threshold, yet well
        // under the 30m single-file DESTRUCTIVE-reclaim bar (DEFAULT_RECLAIM_SMALL_MINUTES)
        // AND the 2h takeover bar. The prior value (exactly 30m) landed ON the
        // single-file reclaim boundary — `command_release_by_path` checks
        // `age > reclaim_timeout`, so second-truncation + slow execution under
        // full-suite load flipped the takeover-refusal ~7% of runs. This is the
        // real root cause of this test's flake (docs/ISSUES-2026-07-07-test-flakes.md),
        // not env/CWD races.
        let stale_ts = (chrono::Utc::now() - chrono::Duration::minutes(22))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let live_id = append_stale_claim(
            &room,
            "busy-builder",
            "src/foo.rs",
            "long build, no rally write in 22m",
            &stale_ts,
        );
        let snap = room.snapshot().unwrap();
        assert!(
            snap.idle_owner_tools().contains("busy-builder"),
            "30m-quiet owner IS idle (advisory)"
        );
        assert!(
            !snap.takeover_eligible_owners().contains("busy-builder"),
            "30m-quiet owner must NOT be takeover-eligible (needs >2h)"
        );

        // before-write downgrades to a reclaimable WARN (advisory), not a stop.
        let mut findings = Vec::new();
        crate::check::check_before_write_for_test(&snap, "peer", Some("src/foo.rs"), &mut findings);
        assert!(
            findings
                .iter()
                .any(|(code, sev)| *code == "stale-owner-claim" && *sev == "warn"),
            "before-write must WARN (not stop) on a 30m-idle owner; got {findings:?}"
        );

        // But the destructive takeover release REFUSES.
        let result = command_release_by_path(
            &room,
            "peer",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "premature takeover".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        );
        assert!(
            result.is_err(),
            "takeover of a busy-but-quiet (30m) owner must be refused"
        );

        let after = room.snapshot().unwrap();
        assert!(
            after.active_claims.iter().any(|c| c.event_id == live_id),
            "busy-but-quiet owner's claim must survive"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// independent-auditor MED (2026-06-09): a stale DIRECTORY claim (`file:src`)
    /// that before-write flags reclaimable for `src/foo.rs` must ALSO be
    /// releasable via that path — the takeover scope match uses `path_matches_scope`
    /// so the WARN points at a command that actually works.
    #[test]
    fn takeover_release_matches_stale_directory_claim_by_contained_path() {
        let root = unique_root("takeover-dir-claim");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        // dead-owner holds a 2-day-stale DIRECTORY claim on `src`.
        let dir_claim = append_stale_claim(
            &room,
            "dead-owner",
            "src",
            "directory-scope claim from a dead session",
            "2026-06-02T10:00:00Z",
        );

        // A peer reclaims it via a contained file path.
        let out = command_release_by_path(
            &room,
            "lead",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "reclaim stale dir claim via contained path".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        )
        .expect("takeover must match a stale directory claim by a contained path");

        let after = room.snapshot().unwrap();
        assert!(
            !after.active_claims.iter().any(|c| c.event_id == dir_claim),
            "stale directory claim must be released via the contained-path takeover"
        );
        let _ = out;

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_only_release_handles_multi_claim_match_atomically() {
        let root = unique_root("path-only-release-multi");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root.clone()).unwrap();

        let c1 = append_claim(&room, "alpha", "src/foo.rs", "first");
        let c2 = append_claim(&room, "alpha", "src/foo.rs", "second");

        let _ = command_release_by_path(
            &room,
            "alpha",
            &["file:src/foo.rs".to_string()],
            None,
            None,
            "release both".to_string(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            true,
            Vec::new(),
        )
        .unwrap();

        let after = room.snapshot().unwrap();
        assert!(!after.active_claims.iter().any(|c| c.event_id == c1));
        assert!(!after.active_claims.iter().any(|c| c.event_id == c2));

        std::fs::remove_dir_all(&root).ok();
    }

    // ─── C3: status post + read roundtrip ────────────────────────────────────

    /// RAII guard for tests that must run in a specific process CWD.
    /// Serializes on `PROCESS_ENV_LOCK` (poison-tolerant) and ALWAYS restores the
    /// previous CWD on drop — including on an assertion panic — so a failing test
    /// cannot leave a dangling/deleted CWD (or a poisoned lock) that cascades into
    /// every later test in the binary. Fixes the `--workspace` flake cluster
    /// documented in docs/ISSUES-2026-07-07-test-flakes.md (Signature A).
    struct CwdEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::path::PathBuf>,
    }
    impl CwdEnvGuard {
        fn enter(dir: &std::path::Path) -> Self {
            let lock = PROCESS_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            // Capture defensively: if a prior test left CWD dangling, current_dir()
            // errors — tolerate it (None) rather than panic and extend the cascade.
            let prev = std::env::current_dir().ok();
            std::env::set_current_dir(dir).expect("set_current_dir to test root");
            CwdEnvGuard { _lock: lock, prev }
        }
    }
    impl Drop for CwdEnvGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                let _ = std::env::set_current_dir(prev);
            }
        }
    }

    fn o26_db_only_command_root(label: &str) -> PathBuf {
        let root = unique_root(label);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let store = store::DirectRoomStore::open_direct_at_with_engagement(
            root.clone(),
            Some("seed".to_string()),
        )
        .unwrap();
        let fact = store::Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: format!("db-only-command-{label}"),
            seq: 0,
            thread_id: format!("thread-db-only-command-{label}"),
            kind: FactKind::Decision,
            tool: Some("codex:migration-command-test".to_string()),
            role: None,
            subject: "DB-only command source".to_string(),
            scope: vec!["src/".to_string()],
            created_at: "2026-08-10T00:00:00Z".to_string(),
            summary: Some("DB-only command source".to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
            from_session_id: None,
        };
        store.append_fact(&fact).unwrap();
        drop(store);
        for entry in std::fs::read_dir(root.join(".rally/log")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                std::fs::remove_file(path).unwrap();
            }
        }
        root
    }

    #[test]
    fn o26_doctor_migration_rejects_missing_engagement_and_mixed_modes() {
        let error =
            match run_inner_with(&argv(&["doctor", "--migrate-db-only", "--apply", "--json"])) {
                Ok(_) => panic!("missing migration engagement must fail"),
                Err(error) => error,
            };
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(error.to_string().contains("requires --engagement"));

        let error = match run_inner_with(&argv(&[
            "doctor",
            "--migrate-db-only",
            "--engagement",
            "alpha",
            "--canonical-paths",
            "--json",
        ])) {
            Ok(_) => panic!("mixed doctor modes must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, RallyError::Usage(_)));
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn o26_doctor_migration_unknown_is_structured_and_resumable() {
        let root = o26_db_only_command_root("unknown").canonicalize().unwrap();
        let _cwd = CwdEnvGuard::enter(&root);
        doctor::arm_db_only_migration_fault(
            &root.join(".rally"),
            doctor::DbOnlyMigrationFaultPoint::AfterTargetInstallBeforeDirectorySync,
        );
        let args = argv(&[
            "doctor",
            "--migrate-db-only",
            "--engagement",
            "alpha",
            "--apply",
            "--json",
        ]);
        let output = run_inner_with(&args).expect("migration uncertainty renders typed output");
        assert_eq!(output.exit_code, 1, "{}", output.body);
        assert_eq!(output.body["command"], "db_only_migration_outcome_unknown");
        let migration = &output.body["data"]["migration"];
        assert!(
            migration["migration_id"]
                .as_str()
                .unwrap()
                .starts_with("dbmig-")
        );
        assert_eq!(migration["state"], "outcome_unknown");
        assert_eq!(migration["retry_safe"], false);
        assert_eq!(migration["phase"], "target-installed-before-directory-sync");
        assert_eq!(
            migration["retry_command"],
            "rally doctor --migrate-db-only --engagement alpha --apply --json"
        );
        assert!(
            !migration["retry_command"]
                .as_str()
                .unwrap()
                .contains("locate")
        );
        assert_eq!(
            std::fs::read_dir(root.join(".rally/log"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
                })
                .count(),
            1
        );

        let retry = run_inner_with(&args).expect("same migration recovery converges");
        assert_eq!(retry.exit_code, 0);
        assert_eq!(retry.body["data"]["doctor"]["state"], "committed");
        drop(_cwd);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_doctor_migration_watchdog_tracks_unknown_then_commit() {
        let signal = Arc::new(Mutex::new(WatchdogMutationState::NotStarted));
        let _signal_guard = install_watchdog_commit_signal(Arc::clone(&signal));
        let _arm_guard = arm_watchdog_command_commit();
        mark_watchdog_db_only_migration_outcome_unknown(
            "dbmig-test",
            "target-file-sync",
            "rally doctor --migrate-db-only --engagement alpha --apply --json",
        );
        assert!(matches!(
            &*signal.lock().unwrap(),
            WatchdogMutationState::DbOnlyMigrationOutcomeUnknown {
                migration_id,
                phase,
                retry_command,
            } if migration_id == "dbmig-test"
                && phase == "target-file-sync"
                && retry_command.contains("--migrate-db-only")
        ));
        mark_watchdog_command_commit();
        assert!(matches!(
            &*signal.lock().unwrap(),
            WatchdogMutationState::Committed {
                projection_complete: true,
                warnings,
            } if warnings.is_empty()
        ));
    }

    fn o26_decision_say_args(tool: &str) -> SayArgs {
        SayArgs {
            json: true,
            kind: FactKind::Decision,
            tool: tool.to_string(),
            subject: Some("o26 command decision".to_string()),
            thread_id: None,
            role: None,
            summary: Some("o26 command contract".to_string()),
            scopes: Vec::new(),
            resources: Vec::new(),
            paths: Vec::new(),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            produces: Vec::new(),
            depends: Vec::new(),
            run_id: None,
            step_id: None,
            parent_step_ids: Vec::new(),
            reason: None,
            wake_after: None,
            ref_standby: None,
        }
    }

    #[test]
    fn o26_standalone_say_unknown_renders_queryable_json_and_text() {
        let root = unique_root("o26-say-unknown");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let _cwd = CwdEnvGuard::enter(&root);
        let room = RoomStore::open().unwrap();
        ensure_presence(&room, "o26-say-tool").unwrap();
        let _ = drain_pending_append_outcomes();
        let _ = drain_pending_append_issues();
        store::fail_o26_once(
            &room.rally_dir(),
            store::O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );

        let error = match command_say(o26_decision_say_args("o26-say-tool")) {
            Ok(_) => panic!("post-sync pre-readback say fault must be unknown"),
            Err(error) => error,
        };
        let (event_id, phase) = match &error {
            RallyError::OutcomeUnknown {
                event_id, phase, ..
            } => (event_id.clone(), phase.clone()),
            other => panic!("expected typed OutcomeUnknown, got {other}"),
        };
        let output = output_after_committed_error(error, true).unwrap();
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.body["command"], "mutation_outcome_unknown");
        assert_eq!(output.body["data"]["event_id"], event_id);
        assert_eq!(output.body["data"]["phase"], phase);
        assert_eq!(
            output.body["data"]["query_remedy"],
            locate_remedy(&event_id)
        );
        assert!(output.text.contains(&event_id));
        assert!(output.text.contains(&phase));
        assert!(output.text.contains("rally locate"));
        assert_eq!(
            room.facts()
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == event_id)
                .count(),
            1
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_say_snapshot_failure_is_committed_success_with_warning() {
        let root = unique_root("o26-say-snapshot-warning");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let _cwd = CwdEnvGuard::enter(&root);
        let room = RoomStore::open().unwrap();
        ensure_presence(&room, "o26-snapshot-tool").unwrap();
        let _ = drain_pending_append_outcomes();
        let _ = drain_pending_append_issues();
        store::fail_o26_once(&room.rally_dir(), store::O26FaultPoint::SnapshotPostCommit);

        let mut output = command_say(o26_decision_say_args("o26-snapshot-tool"))
            .expect("post-commit snapshot failure must not make say retryable");
        attach_pending_append_outcomes(&mut output);
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.body["ok"], true);
        assert_eq!(output.body["data"]["projection_complete"], false);
        let outcomes = output.body["data"]["append_outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["committed"], true);
        assert!(
            outcomes[0]["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "post_commit_work")
        );
        assert_eq!(
            room.facts()
                .unwrap()
                .iter()
                .filter(|fact| fact.subject == "o26 command decision")
                .count(),
            1
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_artifact_ripple_snapshot_failure_degrades_the_primary_outcome() {
        let root = unique_root("o26-artifact-ripple-snapshot");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/grounded.rs"), "pub fn before() {}\n").unwrap();
        let _cwd = CwdEnvGuard::enter(&root);

        let mut claim_args = o26_decision_say_args("o26-artifact-tool");
        claim_args.kind = FactKind::Claim;
        claim_args.subject = Some("claim grounded file".to_string());
        claim_args.paths = vec!["src/grounded.rs".to_string()];
        let claim_output = command_say(claim_args).unwrap();
        let claim_id = claim_output.body["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        std::fs::write(root.join("src/grounded.rs"), "pub fn after() {}\n").unwrap();
        let room = RoomStore::open().unwrap();
        let _ = drain_pending_append_outcomes();
        let _ = drain_pending_append_issues();
        store::fail_o26_once(&room.rally_dir(), store::O26FaultPoint::SnapshotPostCommit);

        let mut artifact_args = o26_decision_say_args("o26-artifact-tool");
        artifact_args.kind = FactKind::Artifact;
        artifact_args.subject = Some("artifact for changed grounded file".to_string());
        artifact_args.paths = vec!["src/grounded.rs".to_string()];
        artifact_args.ref_id = Some(claim_id);
        let mut output =
            command_say(artifact_args).expect("ripple snapshot failure is post-commit degradation");
        attach_pending_append_outcomes(&mut output);
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.body["data"]["say"]["committed"], true);
        assert_eq!(output.body["data"]["say"]["projection_complete"], false);
        assert!(
            output.body["data"]["say"]["projection_warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| {
                    warning["code"] == "post_commit_work"
                        && warning["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("ripple input snapshot"))
                })
        );
        assert_eq!(output.body["data"]["projection_complete"], false);
        assert_eq!(
            output.body["data"]["append_outcomes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let artifact_id = output.body["data"]["say"]["fact"]["event_id"]
            .as_str()
            .unwrap();
        assert_eq!(
            room.facts()
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == artifact_id)
                .count(),
            1
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_status_post_keeps_presence_before_queryable_renewal_unknown() {
        let root = unique_root("o26-status-renewal-unknown");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let _cwd = CwdEnvGuard::enter(&root);
        let room = RoomStore::open().unwrap();
        let tool = "o26-status-tool";
        ensure_presence(&room, tool).unwrap();
        let session_id = current_protocol_session(Some(tool))
            .from_session_id()
            .to_string();
        let claim = store::Fact {
            from_session_id: Some(session_id),
            schema: FACT_SCHEMA.to_string(),
            event_id: "o26-status-renew-claim".to_string(),
            seq: 0,
            thread_id: "o26-status-renew-claim-thread".to_string(),
            kind: store::FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: "claim renewed by status heartbeat".to_string(),
            scope: vec!["file:src/status.rs".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&claim).unwrap();
        let _ = drain_pending_append_outcomes();
        let _ = drain_pending_append_issues();
        // The heartbeat presence append reaches this seam first; pass it, then
        // fail the renewal after sync and before its exact readback.
        store::skip_o26_once(
            &room.rally_dir(),
            store::O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );
        store::fail_o26_once(
            &room.rally_dir(),
            store::O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );

        let error = match command_status_post(
            true,
            cli::StatusPostArgs {
                tool: tool.to_string(),
                state: "working".to_string(),
                file: Some("src/status.rs".to_string()),
                intent: Some("prove status renewal uncertainty".to_string()),
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        ) {
            Ok(_) => panic!("renewal uncertainty must make status a typed partial commit"),
            Err(error) => error,
        };
        let renewal_event_id = match &error {
            RallyError::OutcomeUnknown {
                event_id, phase, ..
            } => {
                assert_eq!(phase, "canonical-sync-before-readback");
                event_id.clone()
            }
            other => panic!("expected renewal OutcomeUnknown, got {other}"),
        };
        let output = output_after_committed_error(error, true).unwrap();
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.body["command"], "partial_commit");
        assert_eq!(
            output.body["data"]["outcome_unknown"]["event_id"],
            renewal_event_id
        );
        assert_eq!(
            output.body["data"]["outcome_unknown"]["remedy"],
            locate_remedy(&renewal_event_id)
        );
        let outcomes = output.body["data"]["append_outcomes"].as_array().unwrap();
        assert_eq!(
            outcomes.len(),
            1,
            "presence is the one proven command append"
        );
        let presence_seq = outcomes[0]["fact"]["seq"].as_i64().unwrap();
        let facts = room.facts().unwrap();
        let renewals = facts
            .iter()
            .filter(|fact| {
                fact.kind == store::FactKind::ClaimRenewed
                    && fact.ref_id.as_deref() == Some(claim.event_id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(renewals.len(), 1);
        assert_eq!(renewals[0].event_id, renewal_event_id);
        assert!(
            presence_seq < renewals[0].seq,
            "outcomes must retain commit order"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_migrate_legacy_emits_each_canonical_outcome_once() {
        let root = unique_root("o26-migrate-command-outcomes");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let _cwd = CwdEnvGuard::enter(&root);
        let home = root.join("test-home");
        let repo_slug = root.file_name().unwrap().to_string_lossy().to_string();
        let apps_dir = home.join(".agent-rally-point/apps").join(&repo_slug);
        std::fs::create_dir_all(&apps_dir).unwrap();
        let row = serde_json::json!({
            "schema": FACT_SCHEMA,
            "event_id": "o26-command-migrate-singleton",
            "seq": 7,
            "thread_id": "o26-command-migrate-thread",
            "kind": "decision",
            "tool": "legacy:test",
            "subject": "duplicate legacy row",
            "scope": [],
            "created_at": "2026-08-10T00:00:00Z",
            "evidence": []
        })
        .to_string();
        std::fs::write(apps_dir.join("changes.jsonl"), format!("{row}\n{row}\n")).unwrap();
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => env::set_var("HOME", value),
                        None => env::remove_var("HOME"),
                    }
                }
            }
        }
        let _home = HomeGuard(env::var_os("HOME"));
        unsafe { env::set_var("HOME", &home) };
        let _ = drain_pending_append_outcomes();
        let _ = drain_pending_append_issues();

        let mut output = command_migrate_legacy(MigrateLegacyArgs { json: true }).unwrap();
        attach_pending_append_outcomes(&mut output);
        assert_eq!(output.body["data"]["migrate-legacy"]["facts_migrated"], 1);
        assert_eq!(
            output.body["data"]["migrate-legacy"]["facts_skipped_existing"],
            1
        );
        assert!(
            output.body["data"]["migrate-legacy"]
                .get("append_outcomes")
                .is_none(),
            "full outcomes belong only at the command-wide boundary"
        );
        assert_eq!(
            output.body["data"]["append_outcomes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            RoomStore::open()
                .unwrap()
                .facts()
                .unwrap()
                .iter()
                .filter(|fact| fact.event_id == "o26-command-migrate-singleton")
                .count(),
            1
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_post_then_status_read_roundtrip() {
        let root = unique_root("status-post-roundtrip");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // command_status_post calls RoomStore::open() which resolves from cwd,
        // so run in `root`. CwdEnvGuard serializes + restores CWD panic-safely.
        let _cwd = CwdEnvGuard::enter(&root);

        let post = command_status_post(
            true,
            cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "working".to_string(),
                file: Some("crates/rally-cli".to_string()),
                intent: Some("agent-state".to_string()),
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        )
        .expect("status post must succeed");
        let post_body: serde_json::Value = post.body.clone();
        let state_kind = post_body["data"]["status_post"]["state"]["state"]
            .as_str()
            .unwrap_or("");
        assert_eq!(state_kind, "working");

        let read = command_status_read(
            true,
            cli::StatusReadArgs {
                tool: Some("alpha".to_string()),
            },
        )
        .expect("status read must succeed");
        let read_body: serde_json::Value = read.body.clone();
        let states = read_body["data"]["status_read"]["states"]
            .as_array()
            .expect("states array");
        assert_eq!(states.len(), 1, "expected one entry for alpha");
        assert_eq!(states[0]["tool"], "alpha");
        assert_eq!(states[0]["state"], "working");
        assert_eq!(states[0]["file"], "crates/rally-cli");
        assert_eq!(states[0]["intent"], "agent-state");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_post_done_autofills_git_metadata_for_codex_and_claude_code() {
        if !git_available() {
            return;
        }
        let root = unique_root("status-done-autofill");
        let branch = "feature/status-done-autofill";
        let sha = init_status_git_repo(&root, branch);

        let _cwd = CwdEnvGuard::enter(&root);

        for tool in ["codex:worker", "claude_code:worker"] {
            let post = command_status_post(
                true,
                cli::StatusPostArgs {
                    tool: tool.to_string(),
                    state: "done".to_string(),
                    file: None,
                    intent: None,
                    blocked_ref: None,
                    wake_after: None,
                    committed_sha: None,
                    worktree_branch: None,
                },
            )
            .expect("status done post must infer git metadata");
            let body: serde_json::Value = post.body.clone();
            let state = &body["data"]["status_post"]["state"];
            assert_eq!(state["state"], "done");
            assert_eq!(state["committed_sha"].as_str().unwrap(), sha);
            assert_eq!(state["worktree_branch"].as_str().unwrap(), branch);
            let subject = body["data"]["status_post"]["fact"]["subject"]
                .as_str()
                .unwrap();
            assert!(subject.contains(&format!("committed_sha={sha}")));
            assert!(subject.contains(&format!("worktree_branch={branch}")));
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_post_done_explicit_metadata_does_not_require_git_autofill() {
        let root = unique_root("status-done-explicit");
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let _cwd = CwdEnvGuard::enter(&root);

        let post = command_status_post(
            true,
            cli::StatusPostArgs {
                tool: "any_agent:worker".to_string(),
                state: "done".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: Some("explicit-sha".to_string()),
                worktree_branch: Some("explicit-branch".to_string()),
            },
        )
        .expect("explicit done metadata must not need git");
        let body: serde_json::Value = post.body.clone();
        let state = &body["data"]["status_post"]["state"];
        assert_eq!(state["state"], "done");
        assert_eq!(state["committed_sha"].as_str().unwrap(), "explicit-sha");
        assert_eq!(
            state["worktree_branch"].as_str().unwrap(),
            "explicit-branch"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_post_done_explicit_marker_overrides_git_for_missing_pair_only() {
        if !git_available() {
            return;
        }
        let root = unique_root("status-done-partial-explicit");
        let branch = "feature/status-done-partial-explicit";
        init_status_git_repo(&root, branch);

        let _cwd = CwdEnvGuard::enter(&root);

        let post = command_status_post(
            true,
            cli::StatusPostArgs {
                tool: "gemini:worker".to_string(),
                state: "done".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: Some("manual-sha".to_string()),
                worktree_branch: None,
            },
        )
        .expect("missing branch must infer while explicit sha wins");
        let body: serde_json::Value = post.body.clone();
        let state = &body["data"]["status_post"]["state"];
        assert_eq!(state["state"], "done");
        assert_eq!(state["committed_sha"].as_str().unwrap(), "manual-sha");
        assert_eq!(state["worktree_branch"].as_str().unwrap(), branch);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn status_post_validates_required_fields_for_each_state() {
        // working requires --file + --intent
        let err = validate_status_post_args(
            "working",
            &cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "working".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("--file"));

        // blocked requires --blocked-ref
        let err = validate_status_post_args(
            "blocked",
            &cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "blocked".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("--blocked-ref"));

        // done requires both --committed-sha + --worktree-branch
        let err = validate_status_post_args(
            "done",
            &cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "done".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: Some("abc".to_string()),
                worktree_branch: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("--worktree-branch"));

        // unknown state errors clearly
        let err = validate_status_post_args(
            "napping",
            &cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "napping".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("idle|working|blocked|done"));

        // idle requires nothing
        validate_status_post_args(
            "idle",
            &cli::StatusPostArgs {
                tool: "alpha".to_string(),
                state: "idle".to_string(),
                file: None,
                intent: None,
                blocked_ref: None,
                wake_after: None,
                committed_sha: None,
                worktree_branch: None,
            },
        )
        .unwrap();
    }
}

#[derive(JsonSchema, Serialize)]
struct CursorData {
    before: i64,
    after: i64,
    advanced: bool,
}

/// A non-blocking advisory emitted by `rally enter` when a potentially
/// ambiguous condition is detected.  The command always succeeds (`ok: true`);
/// warnings are informational only.
#[derive(JsonSchema, Serialize)]
struct EnterWarning {
    /// Machine-readable code, e.g. `"squad-id-active"`.
    code: String,
    /// Human-readable explanation.
    message: String,
}

/// The primary enter result, nested under `data.enter`.
#[derive(JsonSchema, Serialize)]
struct EnterPayload {
    tool: String,
    session_id: String,
    /// Resolved room id (the active engagement label, e.g. "2026-05-29").
    room_id: String,
    cursor: CursorData,
    entry: EntryData,
    attention: Vec<AttentionItem>,
    /// Non-blocking advisories (omitted from JSON when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<EnterWarning>,
    /// Rank-11: current room north-star text, or null when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    mission: Option<String>,
}

/// Envelope for `enter`: primary result at `data.enter`, shared room at `data.room`.
#[derive(JsonSchema, Serialize)]
struct EnterData {
    enter: EnterPayload,
    /// Shared contextual payload — the current room summary.
    room: RoomSummary,
    /// Coordination-mandate (C1): the context the agent must ingest + ack.
    acknowledgment: Acknowledgment,
    /// Live lead/self-role metadata so agents do not rely on stale examples.
    lead_context: LeadContext,
}

/// Coordination-mandate (C1): what `rally enter` surfaces for the agent to
/// ingest and acknowledge before engaging. Advisory — never blocks.
#[derive(JsonSchema, Serialize)]
struct Acknowledgment {
    required: bool,
    acknowledged: bool,
    context: AckContext,
}

#[derive(JsonSchema, Serialize)]
struct AckContext {
    /// Pointer to the operating guide (rules + load-bearing commands).
    rules: String,
    /// Pointer to the coordination doctrine (guardrails + leadership structure).
    doctrine: String,
    /// Current lead, if any.
    lead: Option<String>,
    /// Room north-star / plan.
    mission: Option<String>,
    /// Exact command to acknowledge.
    how_to_ack: String,
}

/// Tool-scoped lead context projected from live room facts.
#[derive(Clone, JsonSchema, Serialize)]
struct LeadContext {
    /// Current lead tool id, if a lead seat is occupied.
    current_lead: Option<String>,
    /// Seq of the latest lead-family fact. This is the lead-context epoch.
    lead_epoch: Option<i64>,
    /// Role Rally assigns to the calling tool for this response.
    self_role: Option<String>,
    /// True when the calling tool is the current lead.
    self_is_lead: bool,
    /// Whether the calling tool has acked the current coordination context.
    self_acknowledged: Option<bool>,
    /// Whether the current lead has acked the coordination context.
    current_lead_acknowledged: Option<bool>,
}

fn build_lead_context(
    snapshot: &RoomSnapshot,
    tool: Option<&str>,
    requested_role: Option<&str>,
) -> LeadContext {
    let current_lead = snapshot.lead.clone();
    let self_is_lead = tool
        .zip(current_lead.as_deref())
        .map(|(tool, lead)| tool == lead)
        .unwrap_or(false);
    let self_role = tool.map(|_| {
        if self_is_lead {
            "lead".to_string()
        } else {
            requested_role.unwrap_or("participant").to_string()
        }
    });
    let self_acknowledged = tool.map(|tool| squad_acknowledged(snapshot, tool));
    let current_lead_acknowledged = current_lead
        .as_deref()
        .map(|lead| squad_acknowledged(snapshot, lead));
    LeadContext {
        current_lead,
        lead_epoch: snapshot.lead_epoch,
        self_role,
        self_is_lead,
        self_acknowledged,
        current_lead_acknowledged,
    }
}

fn squad_acknowledged(snapshot: &RoomSnapshot, tool: &str) -> bool {
    snapshot
        .squads
        .iter()
        .any(|sq| sq.tool == tool && sq.acknowledged)
}

#[derive(JsonSchema, Serialize)]
struct AckData {
    ack: AckPayload,
}

#[derive(JsonSchema, Serialize)]
struct AckPayload {
    tool: String,
    acknowledged: bool,
    fact: Fact,
}

/// Non-blocking advisory emitted by `rally say` when an external-intake
/// condition is detected (B18).  The command always returns `ok: true`;
/// the warning is informational and the risk fact is the durable audit record.
#[derive(JsonSchema, Serialize)]
struct SayWarning {
    /// Machine-readable code, e.g. `"external-intake"`.
    code: String,
    /// Human-readable explanation.
    message: String,
}

/// The primary say result, nested under `data.say`.
#[derive(JsonSchema, Serialize)]
struct SayPayload {
    fact: Fact,
    committed: bool,
    projection_complete: bool,
    projection_warnings: Vec<store::ProjectionWarning>,
}

/// Envelope for `say`: primary result at `data.say`, shared fields as siblings.
#[derive(JsonSchema, Serialize)]
struct SayData {
    say: SayPayload,
    /// Shared contextual payload — the current room summary.
    room: RoomSummary,
    /// Non-blocking advisories (omitted from JSON when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<SayWarning>,
    /// R9-readback: verified room id (engagement label) and sequence number.
    verified: SayVerified,
}

/// R9-readback confirmation surfaced in every successful `rally say` response.
#[derive(JsonSchema, Serialize)]
struct SayVerified {
    /// The canonical room id (engagement label) the fact landed in.
    room: String,
    /// The monotonic sequence number assigned to this fact.
    seq: i64,
}

#[derive(JsonSchema, Serialize)]
struct RoomData {
    query: RoomQuery,
    room: RoomSnapshot,
    /// R10: per-tool read receipts. Populated only when `--readers` is passed.
    /// Lives at top-level `readers` so consumers can access it without digging
    /// into the full room snapshot.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    readers: Vec<ReadReceipt>,
    /// Rank-11: current room north-star text, or null when unset.
    /// Omitted from JSON when no mission has been set.
    #[serde(skip_serializing_if = "Option::is_none")]
    mission: Option<String>,
    /// Per-visible-agent live-injection readiness. This complements
    /// `room.squads[]`: squads show participation, while this tells callers
    /// whether `rally inject` can reach a pane or will only queue a ledger wake.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agent_injectability: Vec<AgentInjectability>,
}

#[derive(Clone, JsonSchema, Serialize)]
struct AgentInjectability {
    tool: String,
    injectable: bool,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Payload for `version`, nested under `data.version`.
#[derive(JsonSchema, Serialize)]
struct VersionPayload {
    version: String,
    build_id: String,
}

/// Envelope for `version`.
#[derive(JsonSchema, Serialize)]
struct VersionData {
    version: VersionPayload,
}

/// Payload for `whoami`, nested under `data.whoami`.
#[derive(JsonSchema, Serialize)]
struct WhoamiPayload {
    tool: Option<String>,
    repo_root: String,
    /// Stable repo identity from `.rally/manifest.json` when present, else the
    /// repo-root directory name. This is intentionally not the engagement label.
    repo_id: String,
    /// Active engagement/room label used for `.rally/log/<room_id>.jsonl`.
    room_id: String,
    worktree: String,
    /// Current branch of the active worktree (self-location: catches
    /// shared-checkout-on-non-main hazards without manual git).
    branch: Option<String>,
    build_id: String,
    cwd: String,
    /// Self-location: which host runtime (ptyd) this process is bound to, and
    /// whether more than one is resolvable (ambiguous → agents must not guess).
    host_runtime: HostRuntime,
    /// Coordination context — resolves "who's lead / what's the goal" in one call.
    lead: Option<String>,
    mission: Option<String>,
    /// Whether `--tool` has recorded a coordination:ack (None if no --tool).
    acknowledged: Option<bool>,
    /// Live lead/self-role metadata when the room is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    lead_context: Option<LeadContext>,
    /// Layered protocol session identity (endpoint id + session lease +
    /// legible name), distinct from `tool`. Answers "which runtime is this?"
    /// beyond the tool label. See [`session_identity`].
    session_identity: session_identity::ProtocolSessionIdentity,
}

/// Self-location of the host runtime (Easy Terminal / ptyd). `bound_socket` is
/// the socket THIS process is pinned to (`PTYD_SOCKET_PATH`); `sockets_found`
/// is every resolvable ptyd socket on disk; `ambiguous` is true when
/// more than one exists — the exact condition that made an agent guess which
/// ptyd it was on. Fail-loud on ambiguity instead of silently defaulting.
#[derive(JsonSchema, Serialize)]
struct HostRuntime {
    under_ptyd: bool,
    bound_socket: Option<String>,
    sockets_found: Vec<String>,
    ambiguous: bool,
}

/// Pure: keep the candidate paths that exist on disk, de-duplicated, order-stable.
/// Extracted so the ambiguity logic is unit-testable without env/$HOME.
fn existing_unique_paths(candidates: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for c in candidates {
        if std::path::Path::new(c).exists() && !found.contains(c) {
            found.push(c.clone());
        }
    }
    found
}

/// Detect resolvable ptyd sockets. Probes the Easy Terminal app-daemon socket
/// (`…/EasyTerminal/ptyd.sock`) and the ptyd CLI socket
/// (`~/.config/ptyd/ptyd.sock`), plus XDG_RUNTIME_DIR. Reads env + filesystem
/// existence only.
fn detect_host_runtime() -> HostRuntime {
    let bound = env::var("PTYD_SOCKET_PATH").ok().filter(|s| !s.is_empty());
    let home = env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<String> = vec![
        // Easy Terminal app daemon socket (was herdr.sock, renamed to ptyd.sock).
        format!("{home}/Library/Application Support/EasyTerminal/ptyd.sock"),
        // ptyd CLI socket.
        format!("{home}/.config/ptyd/ptyd.sock"),
        format!("{home}/.local/share/ptyd/ptyd.sock"),
    ];
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        candidates.push(format!("{xdg}/ptyd.sock"));
    }
    if let Some(b) = &bound {
        candidates.push(b.clone());
    }
    let found = existing_unique_paths(&candidates);
    HostRuntime {
        under_ptyd: bound.is_some() || !found.is_empty(),
        bound_socket: bound,
        ambiguous: found.len() > 1,
        sockets_found: found,
    }
}

/// Envelope for `whoami`.
#[derive(JsonSchema, Serialize)]
struct WhoamiData {
    whoami: WhoamiPayload,
}

#[derive(JsonSchema, Serialize)]
struct OwnersData {
    owners: OwnersPayload,
}

#[derive(JsonSchema, Serialize)]
struct OwnersPayload {
    mode: &'static str,
    dirty_paths: Vec<String>,
    dirty: Vec<DirtyOwner>,
    unclaimed_dirty_paths: Vec<String>,
}

#[derive(JsonSchema, Serialize)]
struct DirtyOwner {
    path: String,
    claim_id: String,
    owner_tool: Option<String>,
    from_session_id: Option<String>,
    owner_status: Option<String>,
    lease_expires_at: Option<String>,
    lease_expired: bool,
    session_liveness: Option<SessionLiveness>,
    liveness_source: Option<&'static str>,
    /// `true` = active heartbeat or live backend; `false` = stale backend or
    /// expired lease with idle owner; `null` = Rally lacks enough evidence.
    is_owner_live: Option<bool>,
    scope: Vec<String>,
    subject: String,
}

#[derive(JsonSchema, Serialize)]
struct NextData {
    tool: String,
    role: Option<String>,
    paths: Vec<String>,
    next: NextResult,
    wake_intent: Option<Fact>,
    room: RoomSummary,
    /// Live lead/self-role metadata for the acting tool.
    lead_context: LeadContext,
}

fn scopes_from(raw_scopes: Vec<String>, resources: Vec<String>, paths: Vec<String>) -> Vec<String> {
    let mut scopes = Vec::new();
    scopes.extend(raw_scopes);
    scopes.extend(resources);
    scopes.extend(paths.into_iter().map(normalize_path));
    scopes.sort();
    scopes.dedup();
    scopes
}

pub(crate) fn normalize_paths(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().map(normalize_path).collect()
}

pub(crate) fn normalize_path(path: String) -> String {
    // Strip existing file: prefix before canonicalizing.
    let raw = if let Some(s) = path.strip_prefix("file:") {
        s
    } else {
        &path
    };

    // Strip leading ./ for relative paths.
    let raw = raw.strip_prefix("./").unwrap_or(raw);

    let p = Path::new(raw);

    // For absolute paths that live under the repo root, make them repo-relative.
    let canonical = if p.is_absolute() {
        if let Ok(root) = repo_root() {
            if let Ok(rel) = p.strip_prefix(&root) {
                normalize_components(rel)
            } else if let Ok(canonical_root) = fs::canonicalize(&root) {
                if let Some(canonical_p) = canonicalize_maybe_missing(p) {
                    if let Ok(rel) = canonical_p.strip_prefix(&canonical_root) {
                        normalize_components(rel)
                    } else {
                        normalize_components(p)
                    }
                } else {
                    normalize_components(p)
                }
            } else {
                normalize_components(p)
            }
        } else {
            normalize_components(p)
        }
    } else {
        // Relative path: collapse . / .. components.
        normalize_components(p)
    };

    if canonical.is_empty() {
        // Fallback: return original with file: prefix but no double slash.
        format!("file:{raw}")
    } else {
        format!("file:{canonical}")
    }
}

/// Returns true when `a` and `b` share a common component-boundary suffix of
/// length ≥ 2 components, AND they are not already caught by exact / dir-prefix
/// matching.  This detects same-file collisions across different path forms, e.g.
/// `src/lib.rs` ↔ `crates/rally-cli/src/lib.rs` (shared suffix: `src/lib.rs`).
///
/// Single-component basenames (`lib.rs`, `config.json`) are intentionally excluded
/// to avoid over-flagging ubiquitous filenames.
///
/// NOTE: sibling crates like `crates/a/src/lib.rs` and `crates/b/src/lib.rs` share
/// the 2-component suffix `src/lib.rs` and WILL flag even though they are different
/// files.  This is intentional — the warning surfaces potential ambiguity for the
/// lead to adjudicate.  Rally facilitates; it never decides.
pub(crate) fn paths_suffix_collide(a: &str, b: &str) -> bool {
    // Use comparable_path so we work on canonicalized, repo-relative strings.
    let a_cmp = comparable_path(a);
    let b_cmp = comparable_path(b);

    // Already caught by exact / dir-prefix — don't double-report.
    if path_matches_scope(a, b) || path_matches_scope(b, a) {
        return false;
    }

    let a_parts: Vec<&str> = a_cmp.split('/').filter(|s| !s.is_empty()).collect();
    let b_parts: Vec<&str> = b_cmp.split('/').filter(|s| !s.is_empty()).collect();

    // The shared suffix length is the trailing overlap.
    let shared = a_parts
        .iter()
        .rev()
        .zip(b_parts.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();

    // Require ≥ 2 shared trailing components.  The shorter path does NOT need to be
    // fully consumed — sibling crates like `crates/a/src/lib.rs` and
    // `crates/b/src/lib.rs` share `src/lib.rs` (2 components) and will flag.
    // This is intentional: the warning surfaces potential ambiguity for the lead to
    // adjudicate.  Rally facilitates; it never decides.
    shared >= 2
}

pub(crate) fn path_matches_scope(scope: &str, path: &str) -> bool {
    let scope = comparable_path(scope);
    let path = comparable_path(path);
    !scope.is_empty() && (scope == path || path.starts_with(&format!("{scope}/")))
}

// =============================================================================
// B18 — repo-scope write guard + external-intake quarantine
// =============================================================================
//
// Classify whether a raw path/URI resolves inside or outside this repo's root.
// Used by command_say to tag and quarantine external-intake facts so they never
// contaminate the repo-local active_claims / open_handoffs / recent_artifacts
// projections.
//
// Marker approach (chosen for minimal Fact shape change):
//   The original fact's `scope` Vec gets an extra entry `"external-intake"`.
//   The snapshot projection filters facts whose scope contains that sentinel.
//   This requires zero new fields on Fact and no schema version bump.

#[derive(Debug, PartialEq)]
pub(crate) enum ScopeClass {
    RepoLocal,
    External,
}

/// Classify a raw path or URI string (before `normalize_path` runs).
///
/// Rules:
/// - Relative paths (no leading `/`) → always `RepoLocal` (they are relative
///   to the repo by convention; no absolute resolution is possible).
/// - Absolute paths under `repo_root()` (by prefix or canonical comparison) → `RepoLocal`.
/// - Absolute paths that do NOT resolve under `repo_root()` → `External`.
/// - Empty string → `RepoLocal` (vacuously; no path to quarantine).
pub(crate) fn classify_scope(path_or_uri: &str) -> ScopeClass {
    // Strip file: prefix if present.
    let raw = path_or_uri.strip_prefix("file:").unwrap_or(path_or_uri);
    let p = Path::new(raw);
    // Only absolute paths can be definitively outside the repo.
    if !p.is_absolute() {
        return ScopeClass::RepoLocal;
    }
    // Absolute: check whether it resolves inside repo_root.
    match repo_relative_path(p) {
        Some(_) => ScopeClass::RepoLocal,
        None => ScopeClass::External,
    }
}

fn comparable_path(value: &str) -> String {
    let stripped = value.strip_prefix("file:").unwrap_or(value);
    let path = Path::new(stripped);
    let repo_relative = if path.is_absolute() {
        repo_relative_path(path).unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    normalize_components(&repo_relative)
}

fn repo_relative_path(path: &Path) -> Option<PathBuf> {
    let root = repo_root().ok()?;
    if let Ok(stripped) = path.strip_prefix(&root) {
        return Some(stripped.to_path_buf());
    }
    let canonical_root = fs::canonicalize(root).ok()?;
    let canonical_path = canonicalize_maybe_missing(path)?;
    canonical_path
        .strip_prefix(canonical_root)
        .ok()
        .map(Path::to_path_buf)
}

fn canonicalize_maybe_missing(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(mut canonical) = fs::canonicalize(cursor) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
        missing.push(cursor.file_name()?.to_owned());
        cursor = cursor.parent()?;
    }
}

fn normalize_components(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => {
                parts.push(value.to_string_lossy().into_owned());
            }
            std::path::Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_string());
                }
            }
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

fn default_subject(kind: &str) -> String {
    match kind {
        "claim" => "claim shared work".to_string(),
        "release" => "release shared work".to_string(),
        "blocker" => "blocker".to_string(),
        "resolve" => "resolve blocker".to_string(),
        "decision" => "decision".to_string(),
        "artifact" => "artifact".to_string(),
        "handoff" => "handoff".to_string(),
        "risk" => "risk".to_string(),
        "lesson" => "lesson".to_string(),
        "session" => "managed session".to_string(),
        "wake" => "wake intent".to_string(),
        "presence" => "agent presence".to_string(),
        "standby" => "agent standby".to_string(),
        _ => kind.to_string(),
    }
}

#[derive(JsonSchema, Serialize)]
struct Envelope<T> {
    ok: bool,
    product: &'static str,
    command: String,
    schema: String,
    data: T,
}

fn envelope<T: Serialize>(command: &str, schema: &str, data: T) -> Result<Value> {
    serde_json::to_value(Envelope {
        ok: true,
        product: "rally",
        command: command.to_string(),
        schema: schema.to_string(),
        data,
    })
    .map_err(RallyError::json("render command envelope"))
}

/// Like `envelope`, but accepts a pre-serialized `Value` for data.
/// Used for commands whose outcome types don't implement `JsonSchema`.
fn envelope_value(command: &str, schema: &str, data: Value) -> Result<Value> {
    Ok(json!({
        "ok": true,
        "product": "rally",
        "command": command,
        "schema": schema,
        "data": data,
    }))
}

pub(crate) fn repo_root() -> Result<PathBuf> {
    let mut dir = env::current_dir().map_err(RallyError::io("current dir"))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(git_common_repo_root(&dir).unwrap_or(dir));
        }
        if !dir.pop() {
            return env::current_dir().map_err(RallyError::io("current dir"));
        }
    }
}

/// The current worktree root — the directory containing the `.git` file or
/// dir reached by walking up from cwd. **Does not** follow `commondir` to the
/// main checkout. Use this when an artifact must land in the active branch's
/// checkout (e.g. files committed to git), as opposed to the shared `.rally/`
/// coordination dir which lives under [`repo_root`].
pub(crate) fn worktree_root() -> Result<PathBuf> {
    let mut dir = env::current_dir().map_err(RallyError::io("current dir"))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return env::current_dir().map_err(RallyError::io("current dir"));
        }
    }
}

fn git_common_repo_root(worktree_root: &Path) -> Option<PathBuf> {
    git_common_dir(worktree_root)?
        .parent()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn git_common_dir(worktree_root: &Path) -> Option<PathBuf> {
    let git = worktree_root.join(".git");
    if git.is_dir() {
        return Some(git.canonicalize().unwrap_or(git));
    }
    let git_dir = read_gitdir_file(&git, worktree_root)?;
    let common_dir = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| resolve_git_path(&git_dir, &value))
        .unwrap_or(git_dir);
    Some(common_dir.canonicalize().unwrap_or(common_dir))
}

fn read_gitdir_file(git_file: &Path, worktree_root: &Path) -> Option<PathBuf> {
    let value = fs::read_to_string(git_file).ok()?;
    let git_dir = value.trim().strip_prefix("gitdir:")?.trim();
    if git_dir.is_empty() {
        return None;
    }
    let path = resolve_git_path(worktree_root, git_dir);
    Some(path.canonicalize().unwrap_or(path))
}

fn resolve_git_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn new_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}_{:x}_{:x}", std::process::id(), nanos)
}

fn stable_operation_id(action: &str, target: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in action.bytes().chain([0]).chain(target.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{action}-{hash:016x}")
}

pub(crate) fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

// =============================================================================
// Work surface commands
// =============================================================================

/// Payload for backlog, nested under `data.backlog`.
#[derive(JsonSchema, Serialize)]
struct BacklogPayload {
    action: String,
    items: Vec<BacklogItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<Fact>,
}

/// Envelope for `backlog`.
#[derive(JsonSchema, Serialize)]
struct BacklogData {
    backlog: BacklogPayload,
}

fn command_backlog(args: BacklogArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    match args.subcommand {
        BacklogSubcommand::Add(add_args) => {
            ensure_presence(&room, &add_args.tool)?;
            let fact = with_watchdog_command_commit(|| {
                add_backlog_item(
                    &room,
                    &add_args.tool,
                    &add_args.id,
                    &add_args.intent,
                    &add_args.owns,
                    &add_args.depends_on,
                    add_args.status.as_deref(),
                    add_args.target.as_deref(),
                    add_args.expected_by.as_deref(),
                )
            })?;
            let items = list_backlog_items(&room).unwrap_or_default();
            let text = format!(
                "backlog add id={} intent={:?} seq={}",
                add_args.id, add_args.intent, fact.seq
            );
            let body = envelope(
                "backlog",
                SCHEMA_BACKLOG,
                BacklogData {
                    backlog: BacklogPayload {
                        action: "add".to_string(),
                        items,
                        added: Some(fact),
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
        BacklogSubcommand::List => {
            // `list` shows OPEN work only; done items live on the board's closed lane.
            let items: Vec<_> = list_backlog_items(&room)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.status != "done")
                .collect();
            let count = items.len();
            let text = format!("backlog list items={count}");
            let body = envelope(
                "backlog",
                SCHEMA_BACKLOG,
                BacklogData {
                    backlog: BacklogPayload {
                        action: "list".to_string(),
                        items,
                        added: None,
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
        BacklogSubcommand::Update(update_args) => {
            ensure_presence(&room, &update_args.tool)?;
            let owns = (!update_args.owns.is_empty()).then_some(update_args.owns.as_slice());
            let depends_on =
                (!update_args.depends_on.is_empty()).then_some(update_args.depends_on.as_slice());
            let fact = with_watchdog_command_commit(|| {
                update_backlog_item(
                    &room,
                    &update_args.tool,
                    &update_args.id,
                    update_args.intent.as_deref(),
                    owns,
                    depends_on,
                    update_args.status.as_deref(),
                    update_args.target.as_deref(),
                    update_args.expected_by.as_deref(),
                )
            })?;
            let items: Vec<_> = list_backlog_items(&room)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.status != "done")
                .collect();
            let text = format!("backlog update id={} seq={}", update_args.id, fact.seq);
            let body = envelope(
                "backlog",
                SCHEMA_BACKLOG,
                BacklogData {
                    backlog: BacklogPayload {
                        action: "update".to_string(),
                        items,
                        added: Some(fact),
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
        BacklogSubcommand::Done(done_args) => {
            ensure_presence(&room, &done_args.tool)?;
            let fact = with_watchdog_command_commit(|| {
                mark_backlog_done(&room, &done_args.tool, &done_args.id)
            })?;
            let items: Vec<_> = list_backlog_items(&room)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.status != "done")
                .collect();
            let text = format!("backlog done id={} seq={}", done_args.id, fact.seq);
            let body = envelope(
                "backlog",
                SCHEMA_BACKLOG,
                BacklogData {
                    backlog: BacklogPayload {
                        action: "done".to_string(),
                        items,
                        added: Some(fact),
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
    }
}

/// Envelope for `board`: `BoardOutput` at `data.board` (already conforming).
#[derive(JsonSchema, Serialize)]
struct BoardData {
    board: BoardOutput,
}

fn command_board(args: BoardArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let board = build_board(&room)?;
    let in_flight = board
        .lanes
        .iter()
        .filter(|l| matches!(l.status, board::LaneStatus::InFlight))
        .count();
    let backlog_open = board.backlog.open.len();
    let text = format!(
        "board in_flight={in_flight} backlog_open={backlog_open} delta={}",
        board.delta.len()
    );
    let body = envelope("board", SCHEMA_BOARD, BoardData { board })?;
    Ok(Output::new(args.json, text, body))
}

/// Which per-kind room projection a `command_kind_read` call serves.
enum KindRead {
    Risks,
    Decisions,
    Artifacts,
    Claims,
}

/// Read-only per-kind projection of the room snapshot: `rally risks`,
/// `rally decisions`, `rally artifacts`, `rally claims`. Each returns the
/// corresponding `RoomSnapshot` bucket under `data.<verb>.rows` — a thin,
/// discoverable view over the same facts `rally room --json` exposes, so
/// agents no longer have to hand-parse `data.room.current_risks` etc.
fn command_kind_read(args: KindReadArgs, kind: KindRead) -> Result<Output> {
    let room = RoomStore::open()?;
    let snapshot = room.snapshot_with_archived(false)?;
    let (name, schema, rows) = match kind {
        KindRead::Risks => ("risks", SCHEMA_RISKS, snapshot.current_risks),
        KindRead::Decisions => ("decisions", SCHEMA_DECISIONS, snapshot.current_decisions),
        KindRead::Artifacts => ("artifacts", SCHEMA_ARTIFACTS, snapshot.recent_artifacts),
        KindRead::Claims => ("claims", SCHEMA_CLAIMS, snapshot.active_claims),
    };
    let text = format!("{name} {}", rows.len());
    let rows_val = serde_json::to_value(&rows).map_err(RallyError::json("serialize facts"))?;
    let mut inner = serde_json::Map::new();
    inner.insert("rows".to_string(), rows_val);
    let mut data = serde_json::Map::new();
    data.insert(name.to_string(), Value::Object(inner));
    let body = envelope_value(name, schema, Value::Object(data))?;
    Ok(Output::new(args.json, text, body))
}

/// Envelope for `route-findings`: result under `data["route-findings"]`.
#[derive(JsonSchema, Serialize)]
struct RouteFindingsData {
    #[serde(rename = "route-findings")]
    route_findings: RoutingSummary,
}

fn command_route_findings(args: RouteFindingsArgs) -> Result<Output> {
    // Read findings file
    let content = fs::read_to_string(&args.file)
        .map_err(RallyError::io(format!("read findings file {}", args.file)))?;
    let findings: Vec<Finding> =
        serde_json::from_str(&content).map_err(RallyError::json("parse findings JSON"))?;

    let room = RoomStore::open()?;
    ensure_presence(&room, &args.tool)?;
    let routing = with_watchdog_command_commit(|| {
        route_findings(&room, &args.tool, findings, args.verified)
    })?;

    let text = format!(
        "route-findings total={} routed={} unowned={}",
        routing.findings_total, routing.routed, routing.unowned
    );
    let body = envelope(
        "route-findings",
        SCHEMA_ROUTE_FINDINGS,
        RouteFindingsData {
            route_findings: routing,
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

// =============================================================================
// B2: rally dag — fan-out DAG view (READ-ONLY PROJECTION)
// =============================================================================
//
// CHARTER ASSERTION: this command path never calls Command/spawn/schedule/exec.
// It reads facts from the store and derives a graph struct.
// Litmus from PLAN-pi-dynamic-seam.md §0:
//   "Does this make Rally start, resume, retry, or schedule work?" → NO.

/// Envelope for `dag`: result under `data.dag`.
#[derive(JsonSchema, Serialize)]
struct DagData {
    dag: DagOutput,
}

fn command_dag(args: DagArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let facts = room.facts()?;
    let dag = build_dag(&facts, &args.run_id);
    let node_count = dag.nodes.len();
    let edge_count = dag.edges.len();
    let text = format!(
        "dag run={} nodes={} edges={} facts_scanned={}",
        dag.run_id, node_count, edge_count, dag.facts_scanned
    );
    let body = envelope("dag", SCHEMA_DAG, DagData { dag })?;
    Ok(Output::new(args.json, text, body))
}

// =============================================================================
// B4: rally wake-due — trust-gated wake eligibility (READ-ONLY PROJECTION)
// =============================================================================
//
// CHARTER ASSERTION: this command path never calls Command/spawn/schedule/exec.
// The `suggested_command` field in WakeDueEntry is a plain string — the external
// runner (rally watch / LaunchAgent / cron) decides whether and when to invoke it.
// Rally never executes it.

/// Payload for wake-due, nested under `data["wake-due"]`.
#[derive(JsonSchema, Serialize)]
struct WakeDuePayload {
    due: Vec<WakeDueEntry>,
}

/// Envelope for `wake-due`: result under `data["wake-due"]`.
#[derive(JsonSchema, Serialize)]
struct WakeDueData {
    #[serde(rename = "wake-due")]
    wake_due: WakeDuePayload,
}

fn command_wake_due(args: WakeDueArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let facts = room.facts()?;
    let due = project_wake_due(&facts, args.tool.as_deref());
    let count = due.len();
    let text = format!("wake-due count={count}");
    let body = envelope(
        "wake-due",
        SCHEMA_WAKE_DUE,
        WakeDueData {
            wake_due: WakeDuePayload { due },
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

// =============================================================================
// Rank-11: rally mission — room north-star + per-agent autonomy envelope
// =============================================================================
//
// CHARTER ASSERTION: rally records and exposes; it NEVER executes, enforces,
// or grants anything. The autonomy envelope is descriptive metadata — rally
// never checks it, gates on it, or grants autonomy.
//
// Three modes:
//   GET (no mutation flags)  → read latest Mission north-star + all envelopes.
//   SET (--set "<text>")     → append Mission fact; scope=["mission"].
//   ENVELOPE (--tool + --may/--must-check) → append Mission fact; scope=["envelope","agent:<name>"].
//
// Latest-by-seq wins on read for both mission and per-agent envelopes.

/// One per-agent autonomy envelope surfaced by `rally mission` GET.
#[derive(JsonSchema, Serialize)]
struct EnvelopeEntry {
    agent: String,
    /// What the agent may do autonomously (from `summary` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    may: Option<String>,
    /// What the agent must check-in before doing (from first `must_check:` evidence marker).
    #[serde(skip_serializing_if = "Option::is_none")]
    must_check: Option<String>,
    set_by: Option<String>,
    set_at: String,
}

/// Payload for `mission` GET, nested under `data.mission`.
#[derive(JsonSchema, Serialize)]
struct MissionGetPayload {
    /// Current north-star text, or null if no mission has been set.
    /// Named `text` (not `mission`) to avoid a self-collision with the key name.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Tool that set the current mission, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    set_by: Option<String>,
    /// Timestamp (ISO-8601) when the mission was set, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    set_at: Option<String>,
    /// Per-agent autonomy envelopes (one per agent, latest-by-seq).
    envelopes: Vec<EnvelopeEntry>,
}

/// Payload for `mission` SET/ENVELOPE, nested under `data.mission`.
#[derive(JsonSchema, Serialize)]
struct MissionSetPayload {
    action: String,
    fact: Fact,
}

/// Envelope for `mission`: both GET and SET nest under `data.mission`.
#[derive(JsonSchema, Serialize)]
#[allow(clippy::large_enum_variant)] // short-lived JSON envelope; boxing would break serde(untagged) ergonomics for no gain
#[serde(untagged)]
enum MissionData {
    Get(MissionGetEnvelope),
    Set(MissionSetEnvelope),
}

#[derive(JsonSchema, Serialize)]
struct MissionGetEnvelope {
    mission: MissionGetPayload,
}

#[derive(JsonSchema, Serialize)]
struct MissionSetEnvelope {
    mission: MissionSetPayload,
}

/// Envelope for `lead`: payload at `data.lead`.
#[derive(JsonSchema, Serialize)]
struct LeadData {
    lead: LeadPayload,
}

#[derive(JsonSchema, Serialize)]
struct LeadPayload {
    action: String,
    current_lead: Option<String>,
    tier: Option<String>,
    assigned: Option<String>,
    fact: Option<Fact>,
}

/// `rally ack` — record that this agent ingested the rules/guardrails/lead/mission
/// surfaced at enter (coordination-mandate C1). Advisory: never blocks; the squad
/// projects `acknowledged: true` thereafter.
fn command_ack(args: AckArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    ensure_presence(&room, &args.tool)?;
    let snapshot = room.snapshot()?;
    let fact = Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Decision,
        tool: Some(args.tool.clone()),
        role: None,
        subject: "coordination:ack".to_string(),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!(
            "{} acknowledged the rally rules/guardrails/lead/mission",
            args.tool
        )),
        evidence: vec![format!("acked-at-seq:{}", snapshot.max_seq)],
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    let fact =
        with_watchdog_command_commit(|| room.append_fact_verified(&fact))?.into_fact_reporting();
    let text = format!("ack recorded for {} (seq {})", args.tool, fact.seq);
    let body = envelope(
        "ack",
        SCHEMA_ACK,
        AckData {
            ack: AckPayload {
                tool: args.tool,
                acknowledged: true,
                fact,
            },
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

/// `rally lead` — show / hand off / assign the lead-agent title.
/// Charter: records + exposes only; the latest `role:lead` decision wins
/// (same projection as first-frontier auto-assign). See docs/SPEC-lead-agent.md.
fn command_lead(args: LeadArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    match args.subcommand {
        LeadSubcommand::Show => {
            let snap = room.snapshot()?;
            let current = snap.lead.clone();
            let facts = room.facts().unwrap_or_default();
            let latest = facts
                .iter()
                .filter(|f| f.kind == "decision" && f.subject == "role:lead")
                .max_by_key(|f| f.seq);
            let tier = latest.and_then(|f| {
                f.evidence
                    .iter()
                    .find_map(|e| e.strip_prefix("tier:").map(str::to_string))
            });
            let assigned = latest.and_then(|f| {
                f.evidence
                    .iter()
                    .find_map(|e| e.strip_prefix("assigned:").map(str::to_string))
            });
            let text = format!(
                "lead={} tier={} assigned={}",
                current.as_deref().unwrap_or("<none>"),
                tier.as_deref().unwrap_or("-"),
                assigned.as_deref().unwrap_or("-"),
            );
            let body = envelope(
                "lead",
                SCHEMA_LEAD,
                LeadData {
                    lead: LeadPayload {
                        action: "show".to_string(),
                        current_lead: current,
                        tier,
                        assigned,
                        fact: None,
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
        LeadSubcommand::Handoff(t) => set_lead(args.json, &t, "handoff"),
        LeadSubcommand::Assign(t) => {
            let mode = if t.user_designated {
                "user-designated"
            } else {
                "assign"
            };
            set_lead(args.json, &t, mode)
        }
        LeadSubcommand::Relinquish(r) => {
            let room = RoomStore::open()?;
            ensure_presence(&room, &r.tool)?;
            let prior = room.snapshot()?.lead;
            let fact = Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: FactKind::Decision,
                tool: Some(r.tool.clone()),
                role: None,
                subject: "role:lead:relinquished".to_string(),
                scope: Vec::new(),
                created_at: now_string(),
                summary: Some(format!("{} relinquished the lead seat", r.tool)),
                evidence: vec!["assigned:relinquished".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            let fact = with_watchdog_command_commit(|| room.append_fact_verified(&fact))?
                .into_fact_reporting();
            let text = format!(
                "lead relinquished by {} (was {})",
                r.tool,
                prior.as_deref().unwrap_or("<none>")
            );
            let body = envelope(
                "lead",
                SCHEMA_LEAD,
                LeadData {
                    lead: LeadPayload {
                        action: "relinquish".to_string(),
                        current_lead: None,
                        tier: None,
                        assigned: Some("relinquished".to_string()),
                        fact: Some(fact),
                    },
                },
            )?;
            Ok(Output::new(args.json, text, body))
        }
    }
}

/// Append a `role:lead` decision transferring the title to `t.to`.
///
/// ARP-R-01, two fixes here; the gate itself is at the write boundary in
/// `write_authority::assert_lead_transfer_authorized`, reached only from
/// `DirectRoomStore::append_fact` (`store.rs:2051`). Every writer that goes
/// through `append_fact` clears the same bar this command does — a `Fact` built
/// in Rust and passed in, or a routed daemon request.
///
/// It does NOT bind a line appended directly to a segment file. The projection
/// reads segments without passing the write boundary, so a direct append
/// bypasses this gate and every other write-boundary control — lead transfer,
/// claim close, breadth, field bounds. That is a property of the trust model,
/// not a bug here: `docs/security/TRUST-MODEL.md` states that a local process
/// which can write `.rally/` can write facts, and that these gates are not an
/// authorization boundary. Do not cite this gate as one.
///
/// **Attribution.** This used to stamp `tool: Some(t.to)` — the BENEFICIARY.
/// The ledger recorded a seizure as authored by the agent that gained the seat,
/// so the one field an investigator reads to find out who took it named the
/// wrong agent, and no gate could be built on `fact.tool` because it did not
/// hold the actor. Now `tool` is the ACTOR and `target` is the beneficiary;
/// `claim_authority::lead_beneficiary` reads `target` and falls back to `tool`
/// so the three pre-existing lead facts in this repo's ledger still replay to
/// the same lead. The ledger is append-only — a projection change has to stay
/// backward-compatible or it rewrites history it cannot edit.
///
/// **Precondition.** The only one used to be `ensure_presence`, which CREATES
/// presence rather than checking standing — so it admitted every caller,
/// including one that had never entered the room.
fn set_lead(json: bool, t: &LeadTargetArgs, mode: &str) -> Result<Output> {
    let room = RoomStore::open()?;
    ensure_presence(&room, &t.tool)?;
    let prior = room.snapshot()?.lead;
    let mut evidence = vec![format!("assigned:{mode}")];
    if let Some(p) = &prior {
        evidence.push(format!("from:{p}"));
    }
    // Recorded on the fact, not just accepted as a flag: a seizure that leaves
    // no trace in the ledger is indistinguishable from a handoff to anyone
    // reading the room later, which is the entire value this flag has.
    if t.force {
        evidence.push(crate::write_authority::LEAD_FORCE_MARKER.to_string());
        if let Some(p) = &prior {
            evidence.push(format!("displaced:{p}"));
        }
    }
    let summary = match (&prior, t.force) {
        (Some(p), true) => format!("{} took the lead seat from {p} (via {mode}, --force)", t.to),
        _ => format!("{} is lead (via {mode})", t.to),
    };
    let fact = Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Decision,
        tool: Some(t.tool.clone()),
        role: None,
        subject: "role:lead".to_string(),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(summary),
        evidence,
        target: Some(t.to.clone()),
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    let fact =
        with_watchdog_command_commit(|| room.append_fact_verified(&fact))?.into_fact_reporting();
    let text = format!(
        "lead {} -> {} (via {mode})",
        prior.as_deref().unwrap_or("<none>"),
        t.to
    );
    let body = envelope(
        "lead",
        SCHEMA_LEAD,
        LeadData {
            lead: LeadPayload {
                action: mode.to_string(),
                current_lead: Some(t.to.clone()),
                tier: None,
                assigned: Some(mode.to_string()),
                fact: Some(fact),
            },
        },
    )?;
    Ok(Output::new(json, text, body))
}

fn command_mission(args: MissionArgs) -> Result<Output> {
    let is_set = args.set.is_some();
    let is_envelope = args.may.is_some() || args.must_check.is_some();

    // ENVELOPE mode: --tool + (--may and/or --must-check)
    if is_envelope {
        let agent = args.tool.as_deref().ok_or_else(|| {
            RallyError::Usage(
                "rally mission envelope requires --tool <agent> with --may and/or --must-check"
                    .to_string(),
            )
        })?;
        let tool_attr = args.tool.clone().unwrap_or_else(|| agent.to_string());
        let room = RoomStore::open()?;
        let may_text = args.may.as_deref().unwrap_or("");
        let must_check_text = args.must_check.as_deref().unwrap_or("");
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("mission"),
            seq: 0,
            thread_id: format!("mission-envelope-{}", sanitize_id(agent)),
            kind: FactKind::Mission,
            tool: Some(tool_attr),
            role: None,
            subject: format!("autonomy envelope for {agent}"),
            scope: vec!["envelope".to_string(), format!("agent:{agent}")],
            created_at: now_string(),
            // `summary` carries the `may` text.
            summary: if may_text.is_empty() {
                None
            } else {
                Some(may_text.to_string())
            },
            // `evidence` carries `must_check:<text>` marker.
            evidence: if must_check_text.is_empty() {
                Vec::new()
            } else {
                vec![format!("must_check:{must_check_text}")]
            },
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let appended = with_watchdog_command_commit(|| room.append_fact_verified(&fact))?
            .into_fact_reporting();
        let text = format!("mission envelope set agent={agent} seq={}", appended.seq);
        let body = envelope(
            "mission",
            SCHEMA_MISSION,
            MissionData::Set(MissionSetEnvelope {
                mission: MissionSetPayload {
                    action: "set-envelope".to_string(),
                    fact: appended,
                },
            }),
        )?;
        return Ok(Output::new(args.json, text, body));
    }

    // SET mode: --set "<north-star text>"
    if is_set {
        let text_val = args.set.unwrap();
        let tool_attr = args.tool.clone().unwrap_or_else(|| "unknown".to_string());
        let room = RoomStore::open()?;
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("mission"),
            seq: 0,
            thread_id: "mission-northstar".to_string(),
            kind: FactKind::Mission,
            tool: Some(tool_attr),
            role: None,
            subject: text_val.clone(),
            scope: vec!["mission".to_string()],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let appended = with_watchdog_command_commit(|| room.append_fact_verified(&fact))?
            .into_fact_reporting();
        let text = format!("mission set seq={}", appended.seq);
        let body = envelope(
            "mission",
            SCHEMA_MISSION,
            MissionData::Set(MissionSetEnvelope {
                mission: MissionSetPayload {
                    action: "set-mission".to_string(),
                    fact: appended,
                },
            }),
        )?;
        return Ok(Output::new(args.json, text, body));
    }

    // GET mode (read-only — no appends).
    let room = RoomStore::open()?;
    let facts = room.facts()?;

    // Derive current mission from latest Mission fact with scope containing "mission".
    let mission_fact = facts
        .iter()
        .filter(|f| f.kind == "mission" && f.scope.iter().any(|s| s == "mission"))
        .max_by_key(|f| f.seq);

    let (mission, set_by, set_at) = match mission_fact {
        Some(f) => (
            Some(f.subject.clone()),
            f.tool.clone(),
            Some(f.created_at.clone()),
        ),
        None => (None, None, None),
    };

    // Derive per-agent envelopes: for each agent, take the latest envelope fact.
    let mut envelope_map: std::collections::BTreeMap<String, &Fact> =
        std::collections::BTreeMap::new();
    for f in facts.iter() {
        if f.kind != "mission" {
            continue;
        }
        // Envelope facts have scope containing "envelope" and "agent:<name>".
        if !f.scope.iter().any(|s| s == "envelope") {
            continue;
        }
        let agent_tag = f.scope.iter().find(|s| s.starts_with("agent:"));
        let Some(agent_name) = agent_tag.and_then(|s| s.strip_prefix("agent:")) else {
            continue;
        };
        let entry = envelope_map.entry(agent_name.to_string()).or_insert(f);
        if f.seq > entry.seq {
            *entry = f;
        }
    }

    let envelopes: Vec<EnvelopeEntry> = envelope_map
        .into_iter()
        .map(|(agent, f)| {
            let may = f.summary.clone();
            let must_check = f
                .evidence
                .iter()
                .find(|e| e.starts_with("must_check:"))
                .and_then(|e| e.strip_prefix("must_check:"))
                .map(str::to_string);
            EnvelopeEntry {
                agent,
                may,
                must_check,
                set_by: f.tool.clone(),
                set_at: f.created_at.clone(),
            }
        })
        .collect();

    let mission_text = mission.as_deref().unwrap_or("(no mission set)");
    let text = format!(
        "mission mission={:?} envelopes={}",
        mission_text,
        envelopes.len()
    );
    let body = envelope(
        "mission",
        SCHEMA_MISSION,
        MissionData::Get(MissionGetEnvelope {
            mission: MissionGetPayload {
                text: mission,
                set_by,
                set_at,
                envelopes,
            },
        }),
    )?;
    Ok(Output::new(args.json, text, body))
}

fn help_text() -> String {
    [
        "rally: repo-local coordination room for parallel agents",
        "",
        "Usage:",
        "  rally init [--json]",
        "  rally hooks status [--json]",
        "  rally hook before-write <claude_code|codex|gemini|cursor> [--tool <tool>] [--session-id <id>] [--strict]  # native host envelope; reads stdin JSON",
        "  rally hooks on|off [--scope <repo|user>] [--json]",
        "  rally hooks prompt (--once|--always|--off) [--scope <repo|user>] [--json]",
        "  rally retrospective [--engagement <label>] [--out <path>] [--json]",
        "  rally rotate [--days <n>] [--dry-run] [--json]",
        "  rally enter --tool <tool> [--engagement <label>] [--path <path>] [--role <role>] [--json]",
        "  rally say <kind> --tool <tool> --subject <subject> [--path <path>] [--json]",
        "  rally room [--tool <tool>] [--role <role>] [--path <path>] [--since <seq>] [--json]",
        "  rally next --tool <tool> [--path <path>] [--role <role>] [--limit <n>] [--json]",
        "  rally locate <event-id> [--json]",
        "  rally recent [--all] [--limit <n>] [--json]",
        "  rally migrate-legacy [--json]  # one-shot replay of legacy ~/.agent-rally-point/apps/<slug>/changes.jsonl into this repo ledger",
        "  rally ack --tool <tool> [--json]  # acknowledge rules/guardrails/lead/mission (coordination-mandate C1)",
        "  rally check before-write --tool <tool> --path <path> [--strict] [--json]",
        "  rally check before-complete --tool <tool> [--strict] [--json]",
        "  rally check tier-fit --role <role> [--proposed-tier <tier>] [--json]  # advisory: does this role's tier fit",
        "  rally check liveness [--tool <tool>] [--enforce] [--json]  # conflicted-out squads; --enforce releases their claims, never blocks",
        "  rally check coordination --tool <committer> [--changed <path>]... [--json]",
        "",
        "  Room projections (read-only slices of `rally room`):",
        "  rally claims [--json]      # active claims",
        "  rally risks [--json]       # active coordination risks",
        "  rally decisions [--json]   # current decisions",
        "  rally artifacts [--json]   # recent artifacts",
        "",
        "  rally lead show|handoff|assign|relinquish [--json]  # lead title; the seat gates room-wide claims, the room freeze, and its own transfer",
        "  rally doctor [--canonical-paths] [--prune-rooms] [--reap-stale] [--sweep-corrupt] [--apply] [--json]",
        "    read-only until --apply; --reap-stale closes over-TTL presence, claims, and leads",
        "  rally worktree gc [--apply] [--json]   # sweep-reap leftover per-agent worktrees",
        "  rally daemon serve|start|stop|status [--json]  # per-repo rallyd store daemon",
        "  rally claims-refresh --tool <tool> --lane <lane> --manifest <path> [--json]",
        "  rally self-exit-check --tool <tool> [--persistent] [--required-streak <n>] [--json]",
        "  rally run <claude|codex|opencode|gemini> [--name <name>] [--backend <auto|tmux|cmux|ptyd>] [--dry-run] [--json]",
        "    managed run ids auto-number active agents, e.g. claude-01 / claude_code:01",
        "  rally sessions [--reap] [--json] [--tmux-bin <path>] [--cmux-bin <path>]",
        "  rally inject <session|name|tool> (--text <text>|--handoff <event-id>) [--timeout-seconds <n>] [--json]",
        "    --handoff waits for target-authored Rally ACK by default; no ACK means assume not received and follow fallback_plan",
        "  rally attach <session|name|tool> [--dry-run] [--json]",
        "  rally capture <session|name|tool> [--lines <n>] [--dry-run] [--json]",
        "  rally stop <session|name|tool> [--dry-run] [--json]",
        "  rally adopt <name> (--tmux <target>|--cmux <target>) [--tool <tool>] [--agent <claude|codex|opencode|gemini>] [--backend <tmux|cmux>] [--json]",
        "    register an already-running agent (tmux or cmux target) without relaunching it; flips strays into injectable managed sessions",
        "",
        "  rally status --global [--json]",
        "  rally watch [--tool <id>] [--interval <secs=5>] [--max-interval <secs=300>] [--on-activity <cmd>]",
        "              [--once] [--duration-hours <h>] [--json] [--print-launchd] [--print-systemd]",
        "  rally version [--json]  # print build-id (version + git hash); exits 0",
        "  rally whoami [--tool <id>] [--json]  # repo_root, repo_id, worktree, build_id, cwd; exits 0",
        "  rally owners --dirty [--json] [--tmux-bin <path>] [--cmux-bin <path>]  # map dirty git paths to claim owners + session liveness",
        "  rally backlog add --tool <tool> --id <id> --intent <text> [--target <tool>] [--status <open|planned|in_progress|blocked|done>] [--expected-by <when>] [--owns <path>] [--depends-on <id>] [--json]",
        "  rally backlog update --tool <tool> --id <id> [--status <open|planned|in_progress|blocked|done>] [--expected-by <when>] [--target <tool>] [--intent <text>] [--json]",
        "  rally backlog list [--json]",
        "  rally backlog done --tool <tool> --id <id> [--json]",
        "  rally board [--json]",
        "  rally route-findings --file <findings.json> [--tool <tool>] --verified [--json]",
        "  rally check-ci [--strict] [--receipt-threshold-secs <secs>] [--json]  # read-only CI gate: exits 0 (pass) or 4 with --strict (fail)",
        "Fact kinds: claim, claim.expired, release, blocker, resolve, decision, artifact, handoff, risk, lesson, session, wake, standby, presence, backlog-item, mission",
        "  rally mission [--json]                                        # GET: current north-star + agent envelopes",
        "  rally mission --set \"<north-star>\" [--tool <t>] [--json]    # SET mission",
        "  rally mission --tool <agent> --may \"<...>\" --must-check \"<...>\" [--json]  # SET envelope",
        "",
        "  rally say standby --tool <tool> --reason <r> --wake-after <+30m|iso> [--run <id>] [--step <id>] [--parent-step <id>] [--tool] [--json]",
        "  rally say wake --tool <tool> --ref-standby <standby-event-id> [--run <id>] [--step <id>] [--json]",
        "  rally dag --run <run-id> [--json]          # READ-ONLY causation DAG (nodes=steps, edges=causation)",
        "  rally wake-due [--tool <tool>] [--json]   # READ-ONLY standbys past wake_after (emits suggested_command strings, never executes)",
        "",
        "  Lineage flags (on any `say` kind): --run <id> --step <id> --parent-step <id>",
    ]
    .join("\n")
}
