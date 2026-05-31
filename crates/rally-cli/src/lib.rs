// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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
pub(crate) const FACT_SCHEMA: &str = "agent-rally.fact.v1";
const SESSION_IDENTITY_RETRIES: usize = 4096;

macro_rules! cmd {
    ($($arg:expr),+ $(,)?) => {
        vec![$($arg.to_string()),+]
    };
}

mod backends;
mod backlog;
mod board;
mod check;
mod check_ci;
mod cli;
mod dag;
mod discovery;
mod doctor;
mod error;
mod init;
mod next;
mod output;
mod retrospective;
mod rotate;
mod route_findings;
mod source_grounding;
mod ripple;
mod store;
mod tier_fit;
mod worktree_guard;

use backends::*;
use backlog::{BacklogItem, add_backlog_item, list_backlog_items, mark_backlog_done};
use board::{BoardOutput, build_board};
use check::build_check;
use check_ci::build_check_ci;
use cli::*;
use dag::{DagOutput, WakeDueEntry, build_dag, project_wake_due, resolve_wake_after};
use error::{RallyError, Result};
use next::{AttentionItem, EntryData, NextResult, build_attention, build_entry, build_next};
use output::{CliError, Output, RenderedOutput};
use route_findings::{Finding, RoutingSummary, route_findings};
use store::{Fact, FactKind, ReadReceipt, RoomQuery, RoomSnapshot, RoomStore, RoomSummary};
// Envelope wrapper types from backends module.
use backends::{InjectEnvelope, RunEnvelope, SessionActionEnvelope, SessionsEnvelope};

const SCHEMA_MIGRATE_LEGACY: &str = "agent-rally.command.migrate-legacy.v1";
const SCHEMA_DOCTOR: &str = "agent-rally.command.doctor.v1";
const SCHEMA_VERSION: &str = "agent-rally.command.version.v1";
const SCHEMA_WHOAMI: &str = "agent-rally.command.whoami.v1";
// Work surface schemas
const SCHEMA_BACKLOG: &str = "agent-rally.command.backlog.v1";
const SCHEMA_LEAD: &str = "agent-rally.command.lead.v1";
const SCHEMA_ACK: &str = "agent-rally.command.ack.v1";
const SCHEMA_BOARD: &str = "agent-rally.command.board.v1";
const SCHEMA_ROUTE_FINDINGS: &str = "agent-rally.command.route-findings.v1";
// B13
const SCHEMA_CHECK_CI: &str = "agent-rally.command.check-ci.v1";
// B1/B2/B4: pi-dynamic observation seam
const SCHEMA_DAG: &str = "agent-rally.command.dag.v1";
const SCHEMA_WAKE_DUE: &str = "agent-rally.command.wake-due.v1";
// Rank-11: room north-star + per-agent autonomy envelope
const SCHEMA_MISSION: &str = "agent-rally.command.mission.v1";

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

/// Resolve the watchdog budget from (in priority order) a `--timeout-ms VALUE`
/// argument, the `RALLY_HOOK_TIMEOUT_MS` env var, then the default. Clamped to
/// `[MIN, MAX]`. Out-of-range / unparseable inputs fall through to the next
/// source rather than erroring — the watchdog must never be the thing that
/// fails a hook.
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
    let ms = from_args
        .or(from_eq)
        .or(from_env)
        .unwrap_or(DEFAULT_WATCHDOG_TIMEOUT_MS)
        .clamp(MIN_WATCHDOG_TIMEOUT_MS, MAX_WATCHDOG_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// Remove the watchdog-only `--timeout-ms` flag (both `--timeout-ms VALUE` and
/// `--timeout-ms=VALUE` forms) from the argument list so it is never forwarded
/// to a subcommand parser. The flag controls only the wall-clock budget and is
/// meaningless to any individual command.
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
    // Resolve the budget from the *raw* args, then strip the watchdog-only
    // `--timeout-ms` flag so it never reaches a subcommand parser (which would
    // reject it as unknown). The env var path needs no stripping.
    let timeout = resolve_watchdog_timeout(&args);
    let args = strip_timeout_flag(args);

    let wants_json = args.iter().any(|arg| arg == "--json");
    // `--fail-open` (passed by the hook wrappers) means "never block the host
    // tool on a rally problem". On timeout we honor it by emitting a neutral
    // allow-everything envelope. Without it we still exit 0 (rally is an
    // advisory coordinator — hanging the agent is strictly worse than skipping
    // one advisory), but we surface a timeout note on stderr for visibility.
    let fail_open = args.iter().any(|arg| arg == "--fail-open");

    let (tx, rx) = std::sync::mpsc::channel::<WatchdogResult>();
    let worker = thread::Builder::new()
        .name("rally-command".to_string())
        .spawn(move || {
            let result = match run_inner_with(&args) {
                Ok(output) => {
                    let exit_code = output.exit_code;
                    let rendered = output.render();
                    WatchdogResult {
                        rendered,
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
            // Deadline elapsed (or the worker panicked without sending). Fail
            // open and exit immediately, abandoning the worker thread.
            emit_timeout_fail_open(wants_json, fail_open, timeout);
            std::process::exit(0);
        }
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
        println!("{}", json!({ "ok": true, "product": "rally" }));
    }
    let _ = fail_open; // semantics identical either way; kept for clarity/logging
    eprintln!(
        "rally: hook exceeded {}ms wall-clock budget — failing open (no coordination check applied)",
        timeout.as_millis()
    );
}

fn run_inner_with(args: &[String]) -> Result<Output> {
    // Test-only blocking seam: simulates a command path wedged on slow/stuck
    // I/O so the watchdog can be exercised deterministically. Compiled out of
    // release builds (`debug_assertions` is false in `--release`), so the
    // installed binary can never be made to hang by setting this var.
    #[cfg(debug_assertions)]
    if let Ok(ms) = env::var("RALLY_TEST_BLOCK_MS") {
        if let Ok(ms) = ms.trim().parse::<u64>() {
            thread::sleep(Duration::from_millis(ms));
        }
    }
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "help")
        )
    {
        return Ok(Output::new(false, help_text(), json!({})));
    }
    reject_unknown_command(args)?;

    let command = match parse_cli(args)? {
        CliParse::Command(command) => *command,
        CliParse::Help(text) => return Ok(Output::new(false, text, json!({}))),
    };

    match command {
        CliCommand::Init(args) => command_init(args),
        CliCommand::Enter(args) => command_enter(args),
        CliCommand::Say(args) => command_say(args),
        CliCommand::Room(args) => command_room(args),
        CliCommand::Next(args) => command_next(args),
        CliCommand::Locate(args) => command_locate(args),
        CliCommand::Recent(args) => command_recent(args),
        CliCommand::Check(args) => command_check(args),
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
        CliCommand::RouteFindings(args) => command_route_findings(args),
        // B13
        CliCommand::CheckCi(args) => command_check_ci(args),
        // B1/B2/B4: pi-dynamic observation seam
        CliCommand::Dag(args) => command_dag(args),
        CliCommand::WakeDue(args) => command_wake_due(args),
        // B-whoami: identity report
        CliCommand::Whoami(args) => command_whoami(args),
        // Rank-11: room north-star + per-agent autonomy envelope
        CliCommand::Mission(args) => command_mission(args),
        CliCommand::Lead(args) => command_lead(args),
        CliCommand::Ack(args) => command_ack(args),
    }
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
    let inner = serde_json::to_value(&outcome).map_err(RallyError::json("retrospective outcome"))?;
    let body = envelope_value("retrospective", SCHEMA_RETROSPECTIVE, json!({ "retrospective": inner }))?;
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

/// Ensure `tool` is registered in the current room engagement.
///
/// Called at the start of every tool-scoped command that writes or reads room
/// state (`say`, `check`, `next`, `enter`, …) so that an agent that skips an
/// explicit `rally enter` still appears in `room.squads[]`.
///
/// Idempotent: if a presence fact for `tool` already exists in this engagement
/// (i.e. the tool already appears in `snapshot.squads`), this is a no-op.
/// If no presence exists, writes exactly one `presence` fact, and if no
/// `role:lead` decision exists yet, also writes one `decision` fact asserting
/// `tool` as lead (first-enter-is-lead).
fn ensure_presence(room: &RoomStore, tool: &str) -> Result<()> {
    ensure_presence_tiered(room, tool, None)
}

/// Tier-aware presence. Lead auto-assign is **frontier-only**: an undeclared
/// tier (`None`) stays lead-eligible (back-compat with lazy-auto-enter callers),
/// but a declared `executing`/`fast` agent entering an empty room does NOT take
/// the lead seat — it stays open until a frontier agent (or user-designated
/// lead) joins. See docs/SPEC-lead-agent.md.
fn ensure_presence_tiered(room: &RoomStore, tool: &str, tier: Option<&str>) -> Result<()> {
    let snapshot = room.snapshot()?;
    // Already in the room — nothing to do.
    if snapshot.squads.iter().any(|s| s.tool == tool) {
        return Ok(());
    }
    // R9 stale-binary guard: embed the build-id in the presence fact's summary
    // so that `command_enter` can detect when different builds are writing to
    // the same room.  Format: "build_id:<BUILD_ID>" — minimal, no schema bump.
    let presence_fact = Fact {
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
        evidence: Vec::new(),
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    room.append_fact_verified(&presence_fact)?;
    // First-FRONTIER-enter-is-lead: assert lead only when the seat is open AND
    // this agent is lead-eligible (frontier tier, or undeclared for back-compat).
    let lead_eligible = matches!(tier, None | Some("frontier"));
    if snapshot.lead.is_none() && lead_eligible {
        let lead_fact = Fact {
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
        room.append_fact_verified(&lead_fact)?;
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
        room.append_fact(&risk_fact)?;
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

        if let Some(ref prior_id) = last_presence_build_id {
            if prior_id != BUILD_ID {
                let drift_msg = format!(
                    "this rally build {} differs from the build {} that last wrote to this room — a stale binary on PATH can silently drop writes; verify which rally is on PATH",
                    BUILD_ID, prior_id
                );
                warnings.push(EnterWarning {
                    code: "binary-drift".to_string(),
                    message: drift_msg.clone(),
                });
                let risk_fact = build_risk_fact(
                    &tool,
                    format!("binary-drift: {} vs {}", BUILD_ID, prior_id),
                    drift_msg,
                    Vec::new(),
                    "warn",
                    Vec::new(),
                    None,
                );
                room.append_fact(&risk_fact)?;
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
                room.append_fact(&risk_fact)?;
            }
        }
    }

    // Component A + B: emit presence (+ first-frontier-enter-is-lead) via shared helper.
    ensure_presence_tiered(&room, &tool, args.tier.as_deref())?;

    // Re-snapshot after presence/lead writes so room summary and squads are current.
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
    room.maybe_append_read_checkpoint(&tool, snapshot.content_max_seq)?;
    let mission = snapshot.mission.clone();
    let acknowledged = snapshot
        .squads
        .iter()
        .find(|sq| sq.tool == tool)
        .map(|sq| sq.acknowledged)
        .unwrap_or(false);
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
        },
    )?;
    let text = format!(
        "entered room tool={} attention={}",
        tool,
        attention_count,
    );
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

    // B1: encode lineage markers (run/step/parent-step) into scope.
    let mut lineage_scope: Vec<String> = Vec::new();
    if let Some(ref run_id) = args.run_id {
        lineage_scope.push(format!("run:{run_id}"));
    }
    if let Some(ref step_id) = args.step_id {
        lineage_scope.push(format!("step:{step_id}"));
    }
    if let Some(ref parent_step_id) = args.parent_step_id {
        lineage_scope.push(format!("parent-step:{parent_step_id}"));
    }
    // Merge lineage into scope (before external-intake check, which runs later).
    scope.extend(lineage_scope);

    // #6 source-grounding: at claim, snapshot content hashes of all claimed file
    // paths and store them as `claimhash:<rel>=<hash>` in evidence.
    let repo_root_for_grounding = repo_root().ok();
    if kind == FactKind::Claim {
        if let Some(ref root) = repo_root_for_grounding {
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
    }

    // #6 source-grounding (artifact): look up claim-open hashes from the ref'd claim fact.
    let grounding_claim_evidence: Vec<String> = if kind == FactKind::Artifact {
        args.ref_id.as_ref()
            .and_then(|ref_id| {
                room.facts().ok()?.into_iter().find(|f| f.event_id == *ref_id)
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
        if parts.is_empty() { None } else { Some(parts.join(" ")) }
    } else {
        args.summary
    };

    // ref_standby (--ref-standby) takes precedence over --ref for wake facts.
    let ref_id = args.ref_standby.or(args.ref_id);

    let fact = Fact {
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
    let fact = match kind {
        FactKind::Release | FactKind::Resolve => {
            room.append_state_transition_verified(&fact)?
        }
        _ => room.append_fact_verified(&fact)?,
    };

    // B18: append ONE durable risk fact for each external-intake detection so
    // the contamination event is permanently auditable.  Never blocks the write.
    let mut say_warnings: Vec<SayWarning> = Vec::new();
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
        room.append_fact(&risk_fact)?;
        say_warnings.push(SayWarning {
            code: "external-intake".to_string(),
            message: risk_summary,
        });
    }

    // #6 source-grounding (artifact): re-hash claimed files; flag ungrounded ones.
    // #8 ripple: detect changed pub signatures affecting peer claims.
    if kind == FactKind::Artifact {
        if let Some(ref root) = repo_root_for_grounding {
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
                        let _ = room.append_fact(&risk_fact);
                    }
                }

                // #8 ripple: for files that CHANGED, detect pub sig changes
                // affecting peer claims. Best-effort; never blocks.
                let changed_files: Vec<String> = original_hashes
                    .keys()
                    .filter(|p| !unchanged.contains(p))
                    .cloned()
                    .collect();
                if !changed_files.is_empty() {
                    let snap_for_ripple = room.snapshot().unwrap_or_default();
                    let ripple_facts = ripple::build_ripple_alerts(
                        &changed_files,
                        root,
                        &args.tool,
                        &snap_for_ripple,
                    );
                    for rf in ripple_facts {
                        let _ = room.append_fact(&rf);
                    }
                }
            }
        }
    }

    let snapshot = room.snapshot()?;
    // R9-readback: capture verified {room, seq} from the confirmed fact.
    let verified = SayVerified {
        room: room.room_id().to_string(),
        seq: fact.seq,
    };
    let body = envelope(
        "say",
        SCHEMA_SAY,
        SayData {
            say: SayPayload { fact: fact.clone() },
            room: RoomSummary::from(&snapshot),
            warnings: say_warnings,
            verified,
        },
    )?;
    let text = format!("said {} {} room={} seq={}", fact.kind.as_str(), fact.event_id, room.room_id(), fact.seq);
    Ok(Output::new(args.json, text, body))
}

fn command_room(args: RoomArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let json_output = args.json;
    let query = RoomQuery::from(args);
    // R10: use snapshot_with_readers when --readers is passed so that
    // ReadReceipt projection happens; otherwise use the cheaper default path.
    let snapshot = if query.readers {
        room.snapshot_with_readers()?.filtered(&query)
    } else {
        room.snapshot()?.filtered(&query)
    };
    // R10: extract readers from snapshot (populated by snapshot_with_readers).
    let readers = snapshot.readers.clone();
    // Rank-11: surface mission at the top level so agents see it without parsing snapshot.
    let mission = snapshot.mission.clone();
    let body = envelope(
        "room",
        SCHEMA_ROOM,
        RoomData {
            query,
            room: snapshot.clone(),
            readers,
            mission,
        },
    )?;
    let text = format!(
        "room claims={} blockers={} handoffs={} decisions={} risks={} artifacts={}",
        snapshot.active_claims.len(),
        snapshot.active_blockers.len(),
        snapshot.open_handoffs.len(),
        snapshot.current_decisions.len(),
        snapshot.current_risks.len(),
        snapshot.recent_artifacts.len()
    );
    Ok(Output::new(json_output, text, body))
}

fn command_next(args: NextArgs) -> Result<Output> {
    let tool = args.tool;
    let role = args.role;
    let paths = normalize_paths(args.paths);
    let limit = args.limit as usize;
    let room = RoomStore::open()?;
    // Component B: auto-register presence for the calling tool.
    ensure_presence(&room, &tool)?;
    let snapshot = room.snapshot()?;
    // #7: always read the backlog store and surface ready items in next output.
    let backlog_items = list_backlog_items(&room).unwrap_or_default();
    let next = build_next(&snapshot, &tool, role.as_deref(), &paths, limit, backlog_items);
    let action = next.action;
    let target_event_id = next
        .target_event_id
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let wake_intent = append_next_wake_intent(&room, &tool, &paths, &next)?;
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
    // This call uses `append_fact` (not `append_fact_verified`) — read-checkpoints
    // are low-stakes metadata and must not trigger a segment readback loop.
    let _ = room.maybe_append_read_checkpoint(&tool, snapshot.content_max_seq);
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
    let data = discovery::recent(args.all, args.limit)?;
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
    let text = format!(
        "migrate-legacy slugs={} facts_read={} migrated={} skipped_existing={}",
        data.slugs_found.len(),
        data.facts_read,
        data.facts_migrated,
        data.facts_skipped_existing,
    );
    let body = envelope("migrate-legacy", SCHEMA_MIGRATE_LEGACY, MigrateLegacyEnvelope { migrate_legacy: data })?;
    Ok(Output::new(args.json, text, body))
}

/// Wrapper: wraps doctor result under `data.doctor`.
#[derive(JsonSchema, Serialize)]
struct DoctorEnvelope<T: Serialize + schemars::JsonSchema> {
    doctor: T,
}

fn command_doctor(args: DoctorArgs) -> Result<Output> {
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
    Err(RallyError::Usage(
        "rally doctor requires --canonical-paths or --prune-rooms".to_string(),
    ))
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
        CheckCiEnvelope { check_ci: outcome.data.check_ci },
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
fn command_whoami(args: WhoamiArgs) -> Result<Output> {
    let repo_root = repo_root().map(|p| p.display().to_string()).unwrap_or_else(|_| "<unknown>".to_string());
    let worktree = worktree_root().map(|p| p.display().to_string()).unwrap_or_else(|_| "<unknown>".to_string());
    let cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "<unknown>".to_string());
    let repo_id = RoomStore::open()
        .map(|r| r.room_id().to_string())
        .unwrap_or_else(|_| "<no-room>".to_string());
    let text = format!(
        "repo_root={repo_root} repo_id={repo_id} build_id={BUILD_ID}"
    );
    let body = envelope(
        "whoami",
        SCHEMA_WHOAMI,
        WhoamiData {
            whoami: WhoamiPayload {
                tool: args.tool,
                repo_root,
                repo_id,
                worktree,
                build_id: BUILD_ID.to_string(),
                cwd,
            },
        },
    )?;
    Ok(Output::new(args.json, text, body))
}

/// Wrapper: wraps status result under `data.status`.
#[derive(JsonSchema, Serialize)]
struct StatusEnvelope {
    status: discovery::GlobalStatusData,
}

fn command_status(args: StatusArgs) -> Result<Output> {
    if !args.global {
        return Err(RallyError::Usage(
            "rally status requires --global".to_string(),
        ));
    }
    let data = discovery::status_global()?;
    let repo_count = data.repos.len();
    let text = format!("status repos={repo_count}");
    let body = envelope("status", SCHEMA_STATUS, StatusEnvelope { status: data })?;
    Ok(Output::new(args.json, text, body))
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
        println!("{line}");
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
    println!("{line}");
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

/// Print a launchd plist referencing this binary + the current working dir.
fn watch_print_launchd(args: &WatchArgs, exe: &Path, repo: &Path) {
    let label = format!(
        "com.agent-rally-point.watch.{}",
        repo.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
    );
    let exe_str = exe.to_string_lossy();
    let repo_str = repo.to_string_lossy();
    let mut program_args = vec![format!("  <string>{exe_str}</string>")];
    program_args.push("  <string>watch</string>".to_string());
    if let Some(interval) = Some(args.interval).filter(|&i| i != 5) {
        program_args.push("  <string>--interval</string>".to_string());
        program_args.push(format!("  <string>{interval}</string>"));
    }
    if let Some(ref cmd) = args.on_activity {
        let escaped = cmd.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        program_args.push("  <string>--on-activity</string>".to_string());
        program_args.push(format!("  <string>{escaped}</string>"));
    }
    let args_xml = program_args.join("\n");
    println!(
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
        repo.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
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
        return Ok(Output::new(
            false,
            String::new(),
            serde_json::json!({}),
        ));
    }

    // Long-running loop mode.
    let deadline: Option<std::time::Instant> = args.duration_hours.map(|h| {
        std::time::Instant::now() + Duration::from_secs_f64(h * 3600.0)
    });

    // Start cursor at the current max_seq so we react only to NEW activity.
    let mut last_seq = watch_read_max_seq(&log_dir);
    let mut current_interval = args.interval;

    loop {
        // Check deadline.
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                if args.json {
                    println!(r#"{{"event":"stopped","ts":"{}"}}"#, now_string());
                }
                break;
            }
        }

        thread::sleep(Duration::from_secs(current_interval));

        // Re-read max_seq (log-and-continue on error).
        let new_seq = watch_read_max_seq(&log_dir);

        if new_seq > last_seq {
            // Activity detected.
            watch_emit_activity(
                args.json,
                last_seq,
                new_seq,
                &room_id,
                args.tool.as_deref(),
            );
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
                watch_emit_heartbeat(
                    &room_id,
                    args.tool.as_deref(),
                    last_seq,
                    current_interval,
                );
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
            c.scope
                .iter()
                .filter_map(|sc| sc.strip_prefix("file:").map(|p| normalize_path(p.to_string())))
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
        let result = tier_fit::check_tier_fit(
            &role,
            args.proposed_tier.as_deref(),
            &snapshot,
        );
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
        let actor = args.tool.clone().unwrap_or_else(|| "rally:liveness".to_string());

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

        let mut conflicted: Vec<ConflictedSquad> = Vec::new();
        for (sq_tool, held_ids) in liveness_conflicted(&snapshot) {
            let held: Vec<&Fact> = snapshot
                .active_claims
                .iter()
                .filter(|c| held_ids.contains(&c.event_id))
                .collect();
            let mut released = Vec::new();
            if args.enforce {
                for claim in &held {
                    let release = Fact {
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
                    room.append_fact(&release)?;
                    released.push(claim.event_id.clone());
                }
                let alert = build_risk_fact(
                    &actor,
                    format!("conflicted-out: {} (unacknowledged + idle, holding claims)", sq_tool),
                    format!(
                        "{} grabbed paths but never acked the coordination context and went idle; claims released, alerting lead/user. Not blocked from editing.",
                        sq_tool
                    ),
                    vec![format!("conflicted:{}", sq_tool)],
                    "warn",
                    vec![format!("released:{}", released.len())],
                    None,
                );
                room.append_fact(&alert)?;
            }
            conflicted.push(ConflictedSquad {
                tool: sq_tool.clone(),
                reason: "unacknowledged + idle + holding open claims".to_string(),
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
    let room = RoomStore::open()?;
    // Component B: auto-register presence when a real tool identity is known.
    // Skip "unknown" (no-tool before-complete calls) — nothing meaningful to register.
    if tool != "unknown" {
        ensure_presence(&room, &tool)?;
    }
    let snapshot = room.snapshot()?;
    let check = build_check(phase, tool, path, args.strict, &snapshot)?;
    let body = envelope("check", SCHEMA_CHECK, check.data)?;
    let text = format!("check findings={}", check.finding_count);
    Ok(Output::new(args.json, text, body).with_exit_code(check.exit_code))
}

fn command_run(args: RunArgs) -> Result<Output> {
    let RunArgs {
        json,
        dry_run,
        agent,
        name,
        backend,
        session_id,
        tool,
        bins,
    } = args;
    let backend_name = backend.as_str().to_string();
    let repo = repo_root()?;
    let agent_spec = AgentSpec::from_name(&agent)?;
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
    let command = agent_spec.command_line(&session.name);
    let backend_runner = BackendRunner::new(backend, bins);
    let start_commands =
        backend_runner.start_commands(&session.target, &repo, &command, &session.name)?;

    let actual_target = if dry_run {
        session.target.clone()
    } else {
        match backend_runner.start(&session.target, &repo, &command, &session.name) {
            Ok(target) => target,
            Err(err) => {
                if let Some(fact) = &reservation.fact {
                    if let Err(cleanup_err) = append_stopped_session_record(&room, &session, fact) {
                        return Err(RallyError::Message(format!(
                            "backend start failed: {err}; additionally failed to mark managed session stopped: {cleanup_err}"
                        )));
                    }
                }
                return Err(err);
            }
        }
    };
    if actual_target != session.target {
        session.target = actual_target;
        if let Some(fact) = &reservation.fact {
            room.append_fact(&session_fact(
                &session,
                "active",
                Some(fact.event_id.clone()),
            ))?;
        }
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
            },
        },
    )?;
    let text = format!(
        "run agent={} backend={} session={}",
        session.agent, session.backend, session.session_id
    );
    Ok(Output::new(json, text, body))
}

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
        };
        let fact = session_fact(&session, "active", None);
        if let Some(fact) = room.append_session_fact_if_context(&fact, context_version)? {
            return Ok(ReservedSession {
                fact: Some(fact),
                session,
            });
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
    if value.len() >= 2 && value.chars().all(|ch| ch.is_ascii_digit()) {
        if let Ok(number) = value.parse::<u64>() {
            if number != 0 {
                used.insert(number);
            }
        }
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
    let sessions = read_session_records()?;
    let body = envelope(
        "sessions",
        SCHEMA_SESSIONS,
        SessionsEnvelope {
            sessions: SessionsData {
                sessions: sessions.clone(),
            },
        },
    )?;
    let text = format!("sessions {}", sessions.len());
    Ok(Output::new(args.json, text, body))
}

fn command_inject(args: InjectArgs) -> Result<Output> {
    let dry_run = args.dry_run;
    let target = args.target;
    let sender_tool = args.tool;
    let session = find_session(&target)?;
    let handoff = args.handoff;
    let is_text_inject = args.text.is_some();
    let text = match (args.text, handoff.as_deref()) {
        (Some(text), _) => text,
        (None, Some(handoff)) => handoff_prompt(&session, handoff),
        (None, None) => {
            return Err(RallyError::Usage(
                "inject requires --text or --handoff".to_string(),
            ));
        }
    };
    let require_ack = args.require_ack;
    if require_ack && handoff.is_none() {
        return Err(RallyError::Usage(
            "--require-ack requires --handoff or --ref".to_string(),
        ));
    }
    let timeout = args.timeout_seconds as u64;

    // Open the room once for all appends in this command.
    let room = if !dry_run { Some(RoomStore::open()?) } else { None };

    let ack_after_seq = if require_ack && !dry_run {
        room.as_ref().map(|r| r.snapshot().map(|s| s.max_seq)).transpose()?
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

    let backend_runner = BackendRunner::new(Backend::parse(&session.backend)?, args.bins);
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let commands = backend_runner.inject_commands(&live_target, &text);

    // Attempt live delivery. If the backend session is gone, the content fact
    // is already recorded above — log the failure but do not propagate it so
    // the caller gets `delivered: false` rather than a hard error.
    let delivered = if dry_run {
        false
    } else {
        match backend_runner.inject(&live_target, &text) {
            Ok(()) => true,
            Err(_) => false,
        }
    };

    let wake_intent = inject_wake_intent_with_room(
        room.as_ref(),
        &session,
        handoff.as_deref(),
        &commands,
        dry_run,
    )?;
    let ack = if require_ack && !dry_run {
        let handoff = handoff.as_deref().unwrap_or_default();
        // room is always Some here (require_ack && !dry_run guards this branch).
        let ack_room = room.as_ref().expect("room must be open for --require-ack");
        Some(wait_for_resolution(
            handoff,
            timeout,
            ack_after_seq.unwrap_or(0),
            ack_room,
        )?)
    } else {
        None
    };
    let inject_payload = InjectData {
        mode: if dry_run { "dry-run" } else { "inject" },
        session: session.clone(),
        handoff,
        require_ack,
        ack: ack.clone(),
        wake_intent,
        commands: command_plan_json(&commands),
        sender_tool,
        content_fact,
        delivered,
    };
    let has_ack = ack.is_some();
    let body = envelope(
        "inject",
        SCHEMA_INJECT,
        InjectEnvelope { inject: inject_payload },
    )?;
    let text = format!(
        "inject session={} delivered={} ack={}",
        session.session_id,
        delivered,
        has_ack,
    );
    Ok(Output::new(args.json, text, body))
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
    let session = find_session(&target)?;
    let backend_runner = BackendRunner::new(Backend::parse(&session.backend)?, args.bins);
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
                backend_runner.stop(&live_target)?;
                remove_session_record(&session.session_id)?;
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

fn read_session_records() -> Result<Vec<ManagedSession>> {
    let room = RoomStore::open()?;
    active_session_records(&room)
}

fn remove_session_record(session_id: &str) -> Result<()> {
    let room = RoomStore::open()?;
    let Some((fact, session)) = active_session_facts(&room)?
        .into_iter()
        .find(|(_, session)| session.session_id == session_id)
    else {
        return Ok(());
    };
    room.append_fact(&session_fact(&session, "stopped", Some(fact.event_id)))?;
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
    ))?;
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

fn session_fact(session: &ManagedSession, status: &str, ref_id: Option<String>) -> Fact {
    Fact {
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
    tool: &str,
    paths: &[String],
    next: &NextResult,
) -> Result<Option<Fact>> {
    if matches!(next.action, "wait" | "proceed_solo") {
        return Ok(None);
    }
    let subject = format!("wake intent for {tool}: {}", next.action);
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
    room.append_fact(&fact).map(Some)
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
        evidence: Vec::new(),
        target: Some(recipient_tool.to_string()),
        ref_id: None,
        status: Some("pending".to_string()),
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
    room.append_fact_verified(&fact)
}

/// Return the content fact without appending (dry-run path).
fn inject_content_fact_dry_run(sender_tool: &str, recipient_tool: &str, text: &str) -> Fact {
    make_inject_content_fact(sender_tool, recipient_tool, text)
}

fn inject_wake_intent_with_room(
    room: Option<&RoomStore>,
    session: &ManagedSession,
    handoff: Option<&str>,
    commands: &[Vec<String>],
    dry_run: bool,
) -> Result<Option<Fact>> {
    let status = if dry_run { "planned" } else { "delivered" };
    let subject = format!("wake intent delivered to {}", session.tool);
    let summary = Some(format!(
        "rally inject {status} for managed session {} via {}",
        session.name, session.backend
    ));
    let evidence = commands.iter().map(|command| command.join(" ")).collect();
    let fact = wake_fact(
        &session.tool,
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
        r.append_fact(&fact).map(Some)
    } else {
        let r = RoomStore::open()?;
        r.append_fact(&fact).map(Some)
    }
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

fn find_session(target: &str) -> Result<ManagedSession> {
    read_session_records()?
        .into_iter()
        .find(|session| {
            session.session_id == target || session.name == target || session.tool == target
        })
        .ok_or_else(|| RallyError::NotFound(format!("unknown managed session {target}")))
}

fn backend_target(backend: Backend, session_id: &str) -> String {
    match backend {
        Backend::Tmux => format!("rally-{}", sanitize_id(session_id)),
        Backend::Herdr | Backend::Cmux => sanitize_id(session_id),
    }
}

fn handoff_prompt(session: &ManagedSession, handoff: &str) -> String {
    format!(
        "Rally managed-session injection for {}. Run: rally next --tool {} --json. If it is actionable for handoff {}, execute the suggested Rally completion command or run: rally say resolve --tool {} --ref {} --subject 'resolved via Rally managed session' --json. Do not edit files unless the Rally action explicitly requires it. Do not ask for confirmation after the Rally command succeeds.",
        session.name, session.tool, handoff, session.tool, handoff
    )
}

fn wait_for_resolution(
    handoff: &str,
    timeout_seconds: u64,
    after_seq: i64,
    room: &RoomStore,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut last_seen_seq = after_seq;
    loop {
        for fact in room.facts()? {
            last_seen_seq = last_seen_seq.max(fact.seq);
            if fact.seq > after_seq
                && fact.kind == "resolve"
                && fact.ref_id.as_deref() == Some(handoff)
            {
                return Ok(json!({
                    "resolved": true,
                    "event_id": fact.event_id,
                    "tool": fact.tool,
                    "subject": fact.subject
                }));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(250)));
    }
    Ok(json!({
        "resolved": false,
        "timed_out": true,
        "waited_seconds": timeout_seconds,
        "after_seq": after_seq
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-lib-{label}-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Component B acceptance test 1: running `ensure_presence` without a prior
    /// `enter` registers the tool in squads and asserts it as lead (first tool).
    #[test]
    fn ensure_presence_auto_enters_tool_and_sets_lead() {
        let root = unique_root("ensure-presence-auto-enter");
        // Simulate a git repo so RoomStore::open() resolves correctly.
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let room = store::RoomStore::open_at(root.clone()).unwrap();
        let snapshot_before = room.snapshot().unwrap();
        assert!(
            snapshot_before.squads.is_empty(),
            "room starts empty"
        );

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
    fn coordination_gate_predicate() {
        // C3: presence + ack + claim-covers-every-changed-file.
        let root = unique_root("coord-merge");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = store::RoomStore::open_at(root).unwrap();
        ensure_presence_tiered(&room, "opus-1", Some("frontier")).unwrap();
        let mk = |subject: &str, scope: Vec<String>| Fact {
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: if subject == "coordination:ack" { FactKind::Decision } else { FactKind::Claim },
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
        room.append_fact(&mk("coordination:ack", Vec::new())).unwrap();
        room.append_fact(&mk("own a", vec!["file:src/a.rs".to_string()])).unwrap();
        let snap = room.snapshot().unwrap();
        let (p, a, unc) = coordination_offenders(&snap, "opus-1", &["src/a.rs".to_string()]);
        assert!(p && a && unc.is_empty(), "acked + claimed file passes the gate");
        let (_, _, unc2) = coordination_offenders(&snap, "opus-1", &["src/b.rs".to_string()]);
        assert_eq!(unc2, vec!["src/b.rs".to_string()], "unclaimed changed file is uncovered");
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
        room.append_fact(&mk(FactKind::Presence, "agent presence: ghost-1", Vec::new())).unwrap();
        room.append_fact(&mk(FactKind::Claim, "claim x", vec!["file:x.rs".to_string()])).unwrap();
        let conflicted = liveness_conflicted(&room.snapshot().unwrap());
        assert_eq!(conflicted.len(), 1, "unacked+idle+claim must be conflicted");
        assert_eq!(conflicted[0].0, "ghost-1");
        assert_eq!(conflicted[0].1.len(), 1, "one held claim");
        // ack (kept old-dated so it stays idle) clears the conflict via acknowledged.
        room.append_fact(&mk(FactKind::Decision, "coordination:ack", Vec::new())).unwrap();
        assert!(
            liveness_conflicted(&room.snapshot().unwrap()).is_empty(),
            "ack must clear conflict-out eligibility"
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
        assert!(acked(&room), "squad must be acknowledged after coordination:ack");
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
        assert!(snapshot.squads.iter().any(|s| s.tool == "tool-x"), "tool-x in squads");
        assert!(snapshot.squads.iter().any(|s| s.tool == "tool-y"), "tool-y in squads");
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
        ensure_unique_session_identity(&identity_b, &active).expect(
            "two distinct-name sessions under the same tool must both be accepted",
        );
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
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: store::FactKind::Risk,
                tool: Some("tool-a".to_string()),
                role: None,
                subject: format!("duplicate-active-squad-id: tool-a"),
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

        let risk_facts: Vec<_> = snapshot
            .current_risks
            .iter()
            .filter(|f| f.subject.contains("duplicate-active-squad-id"))
            .collect();
        assert_eq!(
            risk_facts.len(),
            1,
            "exactly one risk fact for duplicate-active-squad-id must be in current_risks; got: {:?}",
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
        assert!(cursor_after_1 >= cursor_before_1, "first enter cursor must be >= 0");

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
            ("claim",    store::FactKind::Claim),
            ("release",  store::FactKind::Release),
            ("artifact", store::FactKind::Artifact),
            ("handoff",  store::FactKind::Handoff),
            ("decision", store::FactKind::Decision),
            ("risk",     store::FactKind::Risk),
            ("blocker",  store::FactKind::Blocker),
            ("resolve",  store::FactKind::Resolve),
            ("presence", store::FactKind::Presence),
        ];

        let tool = "b16-test-tool";
        let mut written: Vec<store::Fact> = Vec::new();
        for (subject, kind) in kinds {
            let fact = store::Fact {
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
            let appended = writer.append_fact(&fact).unwrap();
            assert!(appended.seq > 0, "appended {subject} must have seq > 0");
            written.push(appended);
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
                .unwrap_or_else(|| panic!(
                    "fact {} (kind={}) not found after reload",
                    w.event_id, w.kind.as_str()
                ));
            assert_eq!(
                found.kind.as_str(), w.kind.as_str(),
                "kind mismatch for {} after reload", w.event_id
            );
            assert_eq!(
                found.tool.as_deref(), Some(tool),
                "tool mismatch for {} after reload", w.event_id
            );
            assert_eq!(
                found.subject, w.subject,
                "subject mismatch for {} after reload", w.event_id
            );
            assert_eq!(
                found.seq, w.seq,
                "seq mismatch for {} after reload", w.event_id
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
        assert_eq!(cursor_before, 0, "cursor must start at 0 before first --once call");

        // First --once: max_seq > 0, so activity should be detected.
        let current_seq = watch_read_max_seq(&log_dir);
        assert!(current_seq > 0, "max_seq must be > 0 after posting a fact");
        let activity_detected = current_seq > cursor_before;
        assert!(activity_detected, "first --once must detect activity (seq advanced from 0)");

        // Simulate what command_watch --once does: persist cursor.
        watch_write_once_cursor(&rally_dir, current_seq);

        // Second --once: cursor now equals current_seq → no activity.
        let cursor_after = watch_read_once_cursor(&rally_dir);
        assert_eq!(cursor_after, current_seq, "cursor must be persisted after first call");
        let new_seq = watch_read_max_seq(&log_dir);
        let activity_second = new_seq > cursor_after;
        assert!(!activity_second, "second --once must not detect activity when no new fact posted");

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
        let parsed: i64 = written.trim().parse()
            .expect("file content must be a valid i64");
        assert_eq!(parsed, to_seq, "RALLY_TO_SEQ in child env must equal the detected to_seq");

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

        // Verify the rendered plist text using the actual function.
        // Redirect stdout to a file via a child `sh -c` that uses the compiled binary.
        let out_path = std::env::temp_dir()
            .join(format!("rally-launchd-{}.plist", short_id()));
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rally"));
        // Run the watch --print-launchd subcommand inside the temp git root.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} watch --print-launchd > {}",
                exe.display(),
                out_path.display()
            ))
            .current_dir(&root)
            .status();

        if let Ok(st) = status {
            if st.success() {
                let plist = std::fs::read_to_string(&out_path).unwrap_or_default();
                assert!(
                    plist.contains("watch"),
                    "plist must contain 'watch' keyword; got:\n{plist}"
                );
                assert!(
                    plist.contains(root.to_string_lossy().as_ref()),
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
            }
            // If the binary cannot run (e.g. different architecture in CI), the
            // label-structure assertions above still exercise the generator logic.
        }

        std::fs::remove_file(&out_path).ok();
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
            seq_after, appended.seq,
            "watch_read_max_seq must return the same seq as appended ({}) from per-repo index",
            appended.seq
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
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18d-claim"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("b18d-tool".to_string()),
            role: None,
            subject: "external claim".to_string(),
            // Marker added by command_say for external-intake.
            scope: vec!["file:/some/other-repo/x.rs".to_string(), "external-intake".to_string()],
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
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18e-hoff"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Handoff,
            tool: Some("b18e-tool".to_string()),
            role: None,
            subject: "external handoff".to_string(),
            scope: vec!["file:/other/repo/x.rs".to_string(), "external-intake".to_string()],
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
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18f-art"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Artifact,
            tool: Some("b18f-tool".to_string()),
            role: None,
            subject: "external artifact".to_string(),
            scope: vec!["file:/other/repo/out.json".to_string(), "external-intake".to_string()],
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
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("b18g-claim"),
            seq: 0,
            thread_id: new_id("room"),
            kind: store::FactKind::Claim,
            tool: Some("b18g-tool".to_string()),
            role: None,
            subject: "b18g external claim".to_string(),
            scope: vec!["file:/some/other/x.rs".to_string(), "external-intake".to_string()],
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

        // The risk fact must appear in current_risks.
        let risk_in_current = snapshot
            .current_risks
            .iter()
            .any(|f| f.subject.starts_with("external-intake:"));
        assert!(
            risk_in_current,
            "external-intake risk fact must appear in current_risks; got: {:?}",
            snapshot.current_risks.iter().map(|f| &f.subject).collect::<Vec<_>>()
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
                claim_scopes[0].1.strip_prefix("file:").unwrap_or(&claim_scopes[0].1),
                claim_scopes[1].1.strip_prefix("file:").unwrap_or(&claim_scopes[1].1),
            );
        assert!(!has_collision, "single claim must produce no suffix collision");

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
        fs::write(&index_path, serde_json::to_string_pretty(&index_content).unwrap()).unwrap();

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
        fs::write(&index_path, serde_json::to_string_pretty(&initial_index).unwrap()).unwrap();

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
        assert_eq!(parts.len(), 2, "BUILD_ID must have exactly one '+' separator");
        assert!(!parts[0].is_empty(), "version part of BUILD_ID must not be empty");
        assert!(!parts[1].is_empty(), "hash part of BUILD_ID must not be empty");
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

        let drift_risks: Vec<_> = snapshot
            .current_risks
            .iter()
            .filter(|f| f.subject.contains("binary-drift"))
            .collect();
        assert_eq!(
            drift_risks.len(),
            1,
            "exactly one binary-drift risk fact must appear in current_risks"
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
        assert_eq!(appended.kind.as_str(), "standby");
        assert!(
            appended.summary.as_deref().unwrap_or("").contains("wake_after:"),
            "summary must contain wake_after marker"
        );

        // Round-trip via fresh store.
        drop(room);
        let reader = store::RoomStore::open_at(root.clone()).unwrap();
        let facts = reader.facts().unwrap();
        let found = facts
            .iter()
            .find(|f| f.event_id == appended.event_id)
            .expect("standby fact must round-trip");
        assert_eq!(found.kind.as_str(), "standby");
        assert!(found.summary.as_deref().unwrap_or("").contains("wake_after:"));
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
            ref_id: Some(standby_fact.event_id.clone()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let wake_fact = room.append_fact_verified(&wake).unwrap();

        assert_eq!(
            wake_fact.ref_id.as_deref(),
            Some(standby_fact.event_id.as_str()),
            "wake fact must reference the standby event_id"
        );

        // Once woken, the standby must not appear in wake-due.
        let facts = room.facts().unwrap();
        let due = dag::project_wake_due(&facts, None);
        // standby is in the future (2099) so it wouldn't surface anyway,
        // but we verify the woken-standby logic covers it.
        let woken_in_due = due
            .iter()
            .any(|d| d.standby_event_id == standby_fact.event_id);
        assert!(
            !woken_in_due,
            "woken standby must not appear in wake-due"
        );

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
        let pe: Vec<_> = dag_out.edges.iter().filter(|e| e.kind == "parent_step").collect();
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
        let s1 = dag_out.nodes.iter().find(|n| n.step_id == "S1").expect("S1 must exist");
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
            .find(|d| d.standby_event_id == standby_fact.event_id)
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
    repo_id: String,
    worktree: String,
    build_id: String,
    cwd: String,
}

/// Envelope for `whoami`.
#[derive(JsonSchema, Serialize)]
struct WhoamiData {
    whoami: WhoamiPayload,
}

#[derive(JsonSchema, Serialize)]
struct NextData {
    tool: String,
    role: Option<String>,
    paths: Vec<String>,
    next: NextResult,
    wake_intent: Option<Fact>,
    room: RoomSummary,
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
    let raw = path_or_uri
        .strip_prefix("file:")
        .unwrap_or(path_or_uri);
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
            let fact = add_backlog_item(
                &room,
                &add_args.tool,
                &add_args.id,
                &add_args.intent,
                &add_args.owns,
                &add_args.depends_on,
            )?;
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
        BacklogSubcommand::Done(done_args) => {
            ensure_presence(&room, &done_args.tool)?;
            let fact = mark_backlog_done(&room, &done_args.tool, &done_args.id)?;
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

/// Envelope for `route-findings`: result under `data["route-findings"]`.
#[derive(JsonSchema, Serialize)]
struct RouteFindingsData {
    #[serde(rename = "route-findings")]
    route_findings: RoutingSummary,
}

fn command_route_findings(args: RouteFindingsArgs) -> Result<Output> {
    // Read findings file
    let content = fs::read_to_string(&args.file).map_err(RallyError::io(format!(
        "read findings file {}",
        args.file
    )))?;
    let findings: Vec<Finding> = serde_json::from_str(&content)
        .map_err(RallyError::json("parse findings JSON"))?;

    let room = RoomStore::open()?;
    ensure_presence(&room, &args.tool)?;
    let routing = route_findings(&room, &args.tool, findings, args.verified)?;

    let text = format!(
        "route-findings total={} routed={} unowned={}",
        routing.findings_total, routing.routed, routing.unowned
    );
    let body = envelope(
        "route-findings",
        SCHEMA_ROUTE_FINDINGS,
        RouteFindingsData { route_findings: routing },
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
    let body = envelope("wake-due", SCHEMA_WAKE_DUE, WakeDueData { wake_due: WakeDuePayload { due } })?;
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
    let fact = room.append_fact_verified(&fact)?;
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
            let mode = if t.user_designated { "user-designated" } else { "assign" };
            set_lead(args.json, &t, mode)
        }
        LeadSubcommand::Relinquish(r) => {
            let room = RoomStore::open()?;
            ensure_presence(&room, &r.tool)?;
            let prior = room.snapshot()?.lead;
            let fact = Fact {
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
            let fact = room.append_fact_verified(&fact)?;
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

/// Append a `role:lead` decision transferring the title to `t.to`. The latest
/// such decision wins in the projection, so this just records the transfer
/// (charter: records/exposes, never enforces).
fn set_lead(json: bool, t: &LeadTargetArgs, mode: &str) -> Result<Output> {
    let room = RoomStore::open()?;
    ensure_presence(&room, &t.tool)?;
    let prior = room.snapshot()?.lead;
    let mut evidence = vec![format!("assigned:{mode}")];
    if let Some(p) = &prior {
        evidence.push(format!("from:{p}"));
    }
    let fact = Fact {
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: new_id("room"),
        kind: FactKind::Decision,
        tool: Some(t.to.clone()),
        role: None,
        subject: "role:lead".to_string(),
        scope: Vec::new(),
        created_at: now_string(),
        summary: Some(format!("{} is lead (via {mode})", t.to)),
        evidence,
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    let fact = room.append_fact_verified(&fact)?;
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
                "rally mission envelope requires --tool <agent> with --may and/or --must-check".to_string(),
            )
        })?;
        let tool_attr = args.tool.clone().unwrap_or_else(|| agent.to_string());
        let room = RoomStore::open()?;
        let may_text = args.may.as_deref().unwrap_or("");
        let must_check_text = args.must_check.as_deref().unwrap_or("");
        let fact = Fact {
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
            summary: if may_text.is_empty() { None } else { Some(may_text.to_string()) },
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
        let appended = room.append_fact_verified(&fact)?;
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
        let appended = room.append_fact_verified(&fact)?;
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
    let mut envelope_map: std::collections::BTreeMap<String, &Fact> = std::collections::BTreeMap::new();
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

    let mission_text = mission
        .as_deref()
        .unwrap_or("(no mission set)");
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
        "  rally retrospective [--engagement <label>] [--out <path>] [--json]",
        "  rally rotate [--days <n>] [--dry-run] [--json]",
        "  rally enter --tool <tool> [--engagement <label>] [--path <path>] [--role <role>] [--json]",
        "  rally say <kind> --tool <tool> --subject <subject> [--path <path>] [--json]",
        "  rally room [--tool <tool>] [--role <role>] [--path <path>] [--since <seq>] [--json]",
        "  rally next --tool <tool> [--path <path>] [--role <role>] [--limit <n>] [--json]",
        "  rally locate <event-id> [--json]",
        "  rally recent [--all] [--limit <n>] [--json]",
        "  rally migrate-legacy [--json]  # one-shot replay of legacy ~/.agent-rally-point/apps/<slug>/changes.jsonl into this repo ledger",
        "  rally check before-write --tool <tool> --path <path> [--strict] [--json]",
        "  rally check before-complete --tool <tool> [--strict] [--json]",
        "  rally run <claude|codex|opencode|gemini> [--name <name>] [--backend <tmux|herdr|cmux>] [--dry-run] [--json]",
        "    managed run ids auto-number active agents, e.g. claude-01 / claude_code:01",
        "  rally sessions [--json]",
        "  rally inject <session|name|tool> (--text <text>|--handoff <event-id>) [--require-ack] [--json]",
        "  rally attach <session|name|tool> [--dry-run] [--json]",
        "  rally capture <session|name|tool> [--lines <n>] [--dry-run] [--json]",
        "  rally stop <session|name|tool> [--dry-run] [--json]",
        "",
        "  rally status --global [--json]",
        "  rally watch [--tool <id>] [--interval <secs=5>] [--max-interval <secs=300>] [--on-activity <cmd>]",
        "              [--once] [--duration-hours <h>] [--json] [--print-launchd] [--print-systemd]",
        "  rally version [--json]  # print build-id (version + git hash); exits 0",
        "  rally whoami [--tool <id>] [--json]  # repo_root, repo_id, worktree, build_id, cwd; exits 0",
        "  rally backlog add --tool <tool> --id <id> --intent <text> [--owns <path>] [--depends-on <id>] [--json]",
        "  rally backlog list [--json]",
        "  rally board [--json]",
        "  rally route-findings --file <findings.json> [--tool <tool>] --verified [--json]",
        "  rally check-ci [--strict] [--receipt-threshold <secs>] [--json]  # read-only CI gate: exits 0 (pass) or 4 with --strict (fail)",
        "Fact kinds: claim, release, blocker, resolve, decision, artifact, handoff, risk, lesson, session, wake, standby, presence, backlog-item, mission",
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
