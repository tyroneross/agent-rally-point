use bpaf::{Args, OptionParser, ParseFailure, Parser, construct, long, positional};

use crate::SessionAction;
use crate::backends::Backend;
use crate::error::{RallyError, Result};
use crate::store::FactKind;

#[allow(clippy::large_enum_variant)] // short-lived dispatch enum; boxing adds indirection for no runtime benefit
pub(crate) enum CliCommand {
    Init(InitArgs),
    Enter(EnterArgs),
    Say(SayArgs),
    Room(RoomArgs),
    Next(NextArgs),
    Check(CheckArgs),
    Run(RunArgs),
    Sessions(SessionsArgs),
    Inject(InjectArgs),
    Session(SessionActionArgs),
    Locate(LocateArgs),
    Recent(RecentArgs),
    Retrospective(RetrospectiveArgs),
    Rotate(RotateArgs),
    Status(StatusArgs),
    Watch(WatchArgs),
    MigrateLegacy(MigrateLegacyArgs),
    Doctor(DoctorArgs),
    Version(VersionArgs),
    // Work surface commands (appended — do not reorder above)
    Backlog(BacklogArgs),
    Board(BoardArgs),
    RouteFindings(RouteFindingsArgs),
    /// B13: CI gate — read-only health check of the room state.
    CheckCi(CheckCiArgs),
    /// B1/B2: fan-out DAG view derived from lineage markers.
    Dag(DagArgs),
    /// B4: trust-gated wake eligibility projection.
    WakeDue(WakeDueArgs),
    /// B-whoami: identity report — repo_root, repo_id, worktree, build_id, cwd.
    Whoami(WhoamiArgs),
    /// Rank-11: room north-star + per-agent autonomy envelope.
    Mission(MissionArgs),
    Lead(LeadArgs),
    Ack(AckArgs),
    /// C-FLEET: register an already-running agent (a tmux or cmux target)
    /// into the managed-session ledger without relaunching it.
    Adopt(AdoptArgs),
}

pub(crate) enum CliParse {
    Command(Box<CliCommand>),
    Help(String),
}

#[derive(Clone, Debug)]
pub(crate) struct InitArgs {
    pub(crate) json: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RotateArgs {
    pub(crate) json: bool,
    /// Override the rotation threshold (days). Falls back to the
    /// `RALLY_ROTATE_DAYS` env var, then `.rally/manifest.json`'s
    /// `rotate_threshold_days`, then a built-in default of 90.
    pub(crate) days: Option<i64>,
    /// Preview mode — list segments that would rotate without moving anything.
    pub(crate) dry_run: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RetrospectiveArgs {
    pub(crate) json: bool,
    /// Optional explicit output path. Defaults to `.rally/RETROSPECTIVE.md`
    /// under the shared repo root.
    pub(crate) out: Option<String>,
    /// Filter to a single engagement label. Default: all engagements present
    /// in the segment set (including the migrated archive).
    pub(crate) engagement: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct EnterArgs {
    pub(crate) json: bool,
    pub(crate) tool: String,
    pub(crate) session_id: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) since: Option<i64>,
    /// Optional engagement label. When set, persists to
    /// `.rally/active-engagement` so subsequent `say` calls in the same repo
    /// inherit it. Composes with the `RALLY_ENGAGEMENT` env var (env wins).
    pub(crate) engagement: Option<String>,
    /// Self-declared capability tier (frontier|executing|fast). Lead auto-assign
    /// is frontier-only; undeclared (None) stays lead-eligible (back-compat).
    pub(crate) tier: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SayArgs {
    pub(crate) json: bool,
    pub(crate) kind: FactKind,
    pub(crate) tool: String,
    pub(crate) subject: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) resources: Vec<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) target: Option<String>,
    pub(crate) ref_id: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) severity: Option<String>,
    pub(crate) uri: Option<String>,
    /// B13: predictive contract — paths/symbols this claim will produce.
    /// Stored as `produces:<x>` markers in the fact's `evidence` Vec.
    pub(crate) produces: Vec<String>,
    /// B13: predictive contract — paths/symbols this claim depends on.
    /// Stored as `depends:<x>` markers in the fact's `evidence` Vec.
    pub(crate) depends: Vec<String>,
    // B1 lineage markers — all optional; stored as scope markers.
    /// Lineage: run identifier shared by a fan-out batch. Stored as `run:<id>` in scope.
    pub(crate) run_id: Option<String>,
    /// Lineage: step identifier for this specific fact. Stored as `step:<id>` in scope.
    pub(crate) step_id: Option<String>,
    /// Lineage: parent step that caused this step. Stored as `parent-step:<id>` in scope.
    pub(crate) parent_step_id: Option<String>,
    // B1 standby-specific args (only meaningful when kind == standby)
    /// Standby: human-readable reason for going dormant. Encoded as `reason:<r>` in summary.
    pub(crate) reason: Option<String>,
    /// Standby: when to wake, as ISO-8601 or relative offset `+30m`/`+2h`. Encoded as `wake_after:<iso>` in summary.
    pub(crate) wake_after: Option<String>,
    // B1 wake-specific args (only meaningful when kind == wake)
    /// Wake: event-id of the standby fact being acknowledged. Stored in ref_id.
    pub(crate) ref_standby: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RoomArgs {
    pub(crate) json: bool,
    pub(crate) tool: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) event_id: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) since: Option<i64>,
    /// R10: project per-tool read receipts from ledger read-checkpoint facts.
    pub(crate) readers: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct NextArgs {
    pub(crate) json: bool,
    pub(crate) tool: String,
    pub(crate) role: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) limit: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct LocateArgs {
    pub(crate) json: bool,
    pub(crate) event_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RecentArgs {
    pub(crate) json: bool,
    pub(crate) all: bool,
    pub(crate) limit: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct MigrateLegacyArgs {
    pub(crate) json: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct VersionArgs {
    pub(crate) json: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct WhoamiArgs {
    pub(crate) json: bool,
    /// Optional tool/role label to echo back in the output.
    pub(crate) tool: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct DoctorArgs {
    pub(crate) json: bool,
    /// Report non-canonical and suffix-colliding claim scopes in the current room.
    pub(crate) canonical_paths: bool,
    /// Classify rooms registry entries as live/stale; with --apply, rewrite the index.
    pub(crate) prune_rooms: bool,
    /// Apply the prune (rewrite index); only meaningful with --prune-rooms.
    pub(crate) apply: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CheckArgs {
    pub(crate) json: bool,
    pub(crate) phase: String,
    pub(crate) tool: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) strict: bool,
    // #9 tier-fit advisory fields (None for non-tier-fit phases)
    pub(crate) role: Option<String>,
    pub(crate) proposed_tier: Option<String>,
    /// C2 liveness: --enforce releases conflicted-out squads' claims + alerts.
    pub(crate) enforce: bool,
    /// C3 coordination merge-gate: changed files (from `git diff --name-only`).
    pub(crate) changed: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RunArgs {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) agent: String,
    pub(crate) name: Option<String>,
    pub(crate) backend: Backend,
    pub(crate) session_id: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) bins: BackendBins,
    /// When true, launch the agent in the canonical shared checkout (today's
    /// behavior) instead of provisioning a dedicated linked worktree.
    /// Accepts both `--shared` and `--no-worktree` on the command line.
    /// Default = false (worktree-per-agent is the default, structural fix for
    /// the shared-branch hazard documented in worktree_guard.rs).
    pub(crate) shared: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionsArgs {
    pub(crate) json: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct InjectArgs {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) target: String,
    pub(crate) text: Option<String>,
    pub(crate) handoff: Option<String>,
    pub(crate) require_ack: bool,
    pub(crate) timeout_seconds: i64,
    pub(crate) bins: BackendBins,
    /// Identity of the agent sending the injection. Defaults to "unknown" when
    /// omitted. Stored in the coordination channel so recipients know the source.
    pub(crate) tool: String,
    /// Plan F sync override. When `true`, the Directive is written with
    /// `urgent: true` AND the daemon performs an immediate PTY-write
    /// instead of waiting for the agent's next checkpoint. RESTRICTED to
    /// `Stop|Retraction` semantics (see F plan §sync override; research
    /// §F4). The daemon (`rally-termd`) rejects `urgent` on
    /// `Deliver+Addition` / `Deliver+Revision` with a Failed Receipt to
    /// preserve TUI integrity.
    pub(crate) urgent: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionActionArgs {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) action: SessionAction,
    pub(crate) target: String,
    pub(crate) lines: i64,
    pub(crate) bins: BackendBins,
}

#[derive(Clone, Debug)]
pub(crate) struct StatusArgs {
    pub(crate) json: bool,
    pub(crate) subcommand: StatusSubcommand,
}

/// `rally status` modes.
///
/// - `Global`: existing multi-repo discovery aggregation (`--global` flag).
/// - `Post`: append a typed agent-state heartbeat (`post` subcommand).
/// - `Read`: project latest-per-tool state from the ledger (`read` subcommand).
///
/// `--global` is preserved as a flag (not a subcommand) so existing scripts
/// calling `rally status --global` keep working identically.
#[derive(Clone, Debug)]
pub(crate) enum StatusSubcommand {
    Global,
    Post(StatusPostArgs),
    Read(StatusReadArgs),
}

/// Typed status heartbeat: `rally status post --tool T --state <s> [opts]`.
///
/// Writes ONE `presence` fact whose `subject` carries the marker grammar
/// `agent_state::parse_marker_string` understands. Always append-only — never
/// overwrites a prior heartbeat. The latest-per-tool projection reads the
/// most-recent.
#[derive(Clone, Debug)]
pub(crate) struct StatusPostArgs {
    pub(crate) tool: String,
    /// One of `idle | working | blocked | done`. Validated in `command_status_post`.
    pub(crate) state: String,
    /// `state=working` requires `--file`.
    pub(crate) file: Option<String>,
    /// `state=working` requires `--intent`.
    pub(crate) intent: Option<String>,
    /// `state=blocked` requires `--blocked-ref`.
    pub(crate) blocked_ref: Option<String>,
    /// `state=idle` may carry `--wake-after <iso>`.
    pub(crate) wake_after: Option<String>,
    /// `state=done` requires `--committed-sha`. The Codex seam.
    pub(crate) committed_sha: Option<String>,
    /// `state=done` requires `--worktree-branch`. The Codex seam.
    pub(crate) worktree_branch: Option<String>,
}

/// Read the latest typed state per tool. `--tool` filters to one tool.
#[derive(Clone, Debug)]
pub(crate) struct StatusReadArgs {
    pub(crate) tool: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct WatchArgs {
    /// Optional tool label — used only to annotate heartbeat output, not to
    /// impersonate the engaged agent.
    pub(crate) tool: Option<String>,
    /// Polling interval in seconds (base; doubled toward max while idle).
    pub(crate) interval: u64,
    /// Maximum adaptive polling interval in seconds.
    pub(crate) max_interval: u64,
    /// Shell command to run when new activity is detected. Receives context
    /// via env vars: RALLY_ROOM, RALLY_FROM_SEQ, RALLY_TO_SEQ, RALLY_TOOL,
    /// RALLY_REPO.
    pub(crate) on_activity: Option<String>,
    /// Poll exactly once (for cron/launchd cadence); persist cursor in
    /// `.rally/watch-cursor.json`.
    pub(crate) once: bool,
    /// Bound the long-running loop to this many hours (default: unbounded).
    pub(crate) duration_hours: Option<f64>,
    /// Emit JSONL output (including idle heartbeats and stop events).
    pub(crate) json: bool,
    /// Print a ready launchd plist to stdout then exit.
    pub(crate) print_launchd: bool,
    /// Print a ready systemd unit to stdout then exit.
    pub(crate) print_systemd: bool,
}

// ─── Work surface args (appended — do not reorder above) ─────────────────────

/// `rally backlog add` or `rally backlog list`
#[derive(Clone, Debug)]
pub(crate) struct BacklogArgs {
    pub(crate) json: bool,
    pub(crate) subcommand: BacklogSubcommand,
}

#[derive(Clone, Debug)]
pub(crate) enum BacklogSubcommand {
    Add(BacklogAddArgs),
    List,
    Done(BacklogDoneArgs),
}

#[derive(Clone, Debug)]
pub(crate) struct BacklogAddArgs {
    pub(crate) tool: String,
    pub(crate) id: String,
    pub(crate) intent: String,
    pub(crate) owns: Vec<String>,
    pub(crate) depends_on: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct BacklogDoneArgs {
    pub(crate) tool: String,
    pub(crate) id: String,
}

/// `rally lead show|handoff|assign` — lead-agent title (records/exposes only).
#[derive(Clone, Debug)]
pub(crate) struct LeadArgs {
    pub(crate) json: bool,
    pub(crate) subcommand: LeadSubcommand,
}

#[derive(Clone, Debug)]
pub(crate) enum LeadSubcommand {
    Show,
    Handoff(LeadTargetArgs),
    Assign(LeadTargetArgs),
    Relinquish(LeadRelinquishArgs),
}

#[derive(Clone, Debug)]
pub(crate) struct LeadTargetArgs {
    pub(crate) tool: String,
    pub(crate) to: String,
    pub(crate) user_designated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct LeadRelinquishArgs {
    pub(crate) tool: String,
}

/// `rally ack --tool <t>` — acknowledge the coordination context (C1).
#[derive(Clone, Debug)]
pub(crate) struct AckArgs {
    pub(crate) json: bool,
    pub(crate) tool: String,
}

/// `rally board [--json]`
#[derive(Clone, Debug)]
pub(crate) struct BoardArgs {
    pub(crate) json: bool,
}

/// `rally route-findings --file <path> [--verified] [--json]`
#[derive(Clone, Debug)]
pub(crate) struct RouteFindingsArgs {
    pub(crate) json: bool,
    /// Path to a JSON file containing an array of `{file, severity, description, evidence?}`.
    pub(crate) file: String,
    /// Tool identity of the scanner/sender.
    pub(crate) tool: String,
    /// Affirm that FP-adjudication has already happened. Required — without it
    /// the command refuses.
    pub(crate) verified: bool,
}

/// C-FLEET: `rally adopt <name>` — register an already-running agent (a
/// tmux or cmux target) into the managed-session ledger without relaunching
/// it. ONE of `--tmux` or `--cmux` is required to identify the running
/// surface. HERDR-INDEPENDENT: the original `--pane` (herdr) arm and its
/// `herdr agent list` auto-discovery were dropped with `Backend::Herdr`
/// (Plan F Chunk 3); adopt now targets only the two live backends.
#[derive(Clone, Debug)]
pub(crate) struct AdoptArgs {
    pub(crate) json: bool,
    /// Human-friendly session name. Becomes `session.name`. Required.
    pub(crate) name: String,
    /// Identify the running surface via a tmux target (`rally-claude-foo`).
    /// Mutually exclusive with `--cmux`.
    pub(crate) tmux: Option<String>,
    /// Identify the running surface via a cmux target.
    /// Mutually exclusive with `--tmux`.
    pub(crate) cmux: Option<String>,
    /// Optional `--tool` identity. Defaults to `<agent>:adopted-<n>`.
    pub(crate) tool: Option<String>,
    /// Agent name (claude|codex|opencode|gemini). Defaults to `claude`.
    pub(crate) agent: Option<String>,
    /// Backend hint (tmux|cmux). Auto-inferred from --tmux/--cmux when omitted.
    pub(crate) backend: Option<Backend>,
}

#[derive(Clone, Debug)]
pub(crate) struct BackendBins {
    pub(crate) tmux_bin: String,
    pub(crate) cmux_bin: String,
}

// PROVENANCE: previously held `herdr_bin: String` and `herdr_socket: Option<String>`
// for the legacy `Backend::Herdr` lane. Both were dropped when the herdr backend
// was removed in Plan F (Chunk 3); the CLI surface (`--herdr-bin` / `--herdr-socket`)
// is removed in the same cleanup pass that retired this struct's herdr fields.

impl Default for BackendBins {
    fn default() -> Self {
        Self {
            tmux_bin: "tmux".to_string(),
            cmux_bin: "cmux".to_string(),
        }
    }
}

/// B2: arguments for `rally dag`.
#[derive(Clone, Debug)]
pub(crate) struct DagArgs {
    pub(crate) json: bool,
    /// Run identifier whose facts form the DAG (required).
    pub(crate) run_id: String,
}

/// B4: arguments for `rally wake-due`.
#[derive(Clone, Debug)]
pub(crate) struct WakeDueArgs {
    pub(crate) json: bool,
    /// Filter to a specific tool (optional; surfaces only standbys owned by this tool).
    pub(crate) tool: Option<String>,
}

/// B13: arguments for `rally check ci`.
#[derive(Clone, Debug)]
pub(crate) struct CheckCiArgs {
    pub(crate) json: bool,
    /// Exit 4 (fail) when any offender is found; default is exit 0 (warn only).
    pub(crate) strict: bool,
    /// Threshold in seconds after which an unreceipted handoff is flagged.
    /// Default: 3600 (1 hour).
    pub(crate) receipt_threshold_secs: u64,
}

/// Rank-11: `rally mission` args.
///
/// Three modes (mutually exclusive by flag presence):
///   - GET (no mutation flags)        → return current mission + envelopes.
///   - SET (`--set "<text>"`)         → append a Mission north-star fact.
///   - SET ENVELOPE (`--tool` + `--may` or `--must-check`) → append an envelope fact.
#[derive(Clone, Debug)]
pub(crate) struct MissionArgs {
    pub(crate) json: bool,
    /// North-star text (SET mission mode). When present, appends a Mission fact.
    pub(crate) set: Option<String>,
    /// Tool identity to attribute the mission write to (SET + envelope modes).
    /// Falls back to resolved presence tool on GET.
    pub(crate) tool: Option<String>,
    /// Envelope: what the named agent may do autonomously (SET ENVELOPE mode).
    pub(crate) may: Option<String>,
    /// Envelope: what the named agent must check-in before doing (SET ENVELOPE mode).
    pub(crate) must_check: Option<String>,
}

const COMMANDS: &[&str] = &[
    "init",
    "enter",
    "say",
    "room",
    "next",
    "check",
    "run",
    "sessions",
    "inject",
    "attach",
    "capture",
    "stop",
    "locate",
    "recent",
    "retrospective",
    "rotate",
    "status",
    "watch",
    "migrate-legacy",
    "doctor",
    "version",
    // Work surface commands (appended — do not reorder above)
    "backlog",
    "board",
    "route-findings",
    // B13: CI gate
    "check-ci",
    // B1/B2/B4: pi-dynamic observation seam
    "dag",
    "wake-due",
    // B-whoami: identity report
    "whoami",
    // Rank-11: room north-star + per-agent autonomy envelope
    "mission",
    // Lead-agent title surface (L-2)
    "lead",
    // Coordination-mandate ack (C1)
    "ack",
    // C-FLEET: register an already-running agent into the managed-session ledger
    "adopt",
];

pub(crate) fn reject_unknown_command(args: &[String]) -> Result<()> {
    let Some(command) = args.first() else {
        return Ok(());
    };
    if COMMANDS.contains(&command.as_str()) {
        Ok(())
    } else {
        Err(RallyError::Usage(format!(
            "unknown Rally command {command}"
        )))
    }
}

pub(crate) fn parse_cli(args: &[String]) -> Result<CliParse> {
    match cli_parser().run_inner(Args::from(args).set_name("rally")) {
        Ok(command) => Ok(CliParse::Command(Box::new(command))),
        Err(failure @ (ParseFailure::Stdout(..) | ParseFailure::Completion(_))) => {
            Ok(CliParse::Help(failure.unwrap_stdout()))
        }
        Err(failure @ ParseFailure::Stderr(_)) => {
            Err(RallyError::Usage(parse_failure_message(failure)))
        }
    }
}

fn parse_failure_message(failure: ParseFailure) -> String {
    match failure {
        ParseFailure::Stderr(_) => {
            format!("invalid arguments: {}", failure.unwrap_stderr().trim())
        }
        ParseFailure::Stdout(..) | ParseFailure::Completion(_) => failure.unwrap_stdout(),
    }
}

fn cli_parser() -> OptionParser<CliCommand> {
    let init = init_parser()
        .to_options()
        .command("init")
        .map(CliCommand::Init);
    let enter = enter_parser()
        .to_options()
        .command("enter")
        .map(CliCommand::Enter);
    let say = say_parser()
        .to_options()
        .command("say")
        .map(CliCommand::Say);
    let room = room_parser()
        .to_options()
        .command("room")
        .map(CliCommand::Room);
    let next = next_parser()
        .to_options()
        .command("next")
        .map(CliCommand::Next);
    let locate = locate_parser()
        .to_options()
        .command("locate")
        .map(CliCommand::Locate);
    let recent = recent_parser()
        .to_options()
        .command("recent")
        .map(CliCommand::Recent);
    let check = check_parser()
        .to_options()
        .command("check")
        .map(CliCommand::Check);
    let run = run_parser()
        .to_options()
        .command("run")
        .map(CliCommand::Run);
    let sessions = sessions_parser()
        .to_options()
        .command("sessions")
        .map(CliCommand::Sessions);
    let inject = inject_parser()
        .to_options()
        .command("inject")
        .map(CliCommand::Inject);
    let attach = session_action_parser(SessionAction::Attach)
        .to_options()
        .command("attach")
        .map(CliCommand::Session);
    let capture = session_action_parser(SessionAction::Capture)
        .to_options()
        .command("capture")
        .map(CliCommand::Session);
    let stop = session_action_parser(SessionAction::Stop)
        .to_options()
        .command("stop")
        .map(CliCommand::Session);
    let retrospective = retrospective_parser()
        .to_options()
        .command("retrospective")
        .map(CliCommand::Retrospective);
    let rotate = rotate_parser()
        .to_options()
        .command("rotate")
        .map(CliCommand::Rotate);
    let status = status_parser()
        .to_options()
        .command("status")
        .map(CliCommand::Status);
    let watch = watch_parser()
        .to_options()
        .command("watch")
        .map(CliCommand::Watch);
    let migrate_legacy = migrate_legacy_parser()
        .to_options()
        .command("migrate-legacy")
        .map(CliCommand::MigrateLegacy);
    let doctor = doctor_parser()
        .to_options()
        .descr("Read-only diagnostics: path hygiene (--canonical-paths) and room registry pruning (--prune-rooms).")
        .command("doctor")
        .map(CliCommand::Doctor);
    let version = version_parser()
        .to_options()
        .descr("Print the rally build-id (version + git hash). Exits 0.")
        .command("version")
        .map(CliCommand::Version);
    // B13: CI gate (read-only)
    let check_ci = check_ci_parser()
        .to_options()
        .descr("Read-only CI gate: exits 0 (pass) or 4 with --strict (fail) listing unresolved blockers, unsatisfied depends, and long-unreceipted handoffs.")
        .command("check-ci")
        .map(CliCommand::CheckCi);

    // B2: fan-out DAG view (read-only)
    let dag = dag_parser()
        .to_options()
        .descr("Read-only causation DAG derived from run/step/parent-step lineage markers. Nodes tagged landed|in-flight|stalled.")
        .command("dag")
        .map(CliCommand::Dag);

    // B4: trust-gated wake eligibility (read-only)
    let wake_due = wake_due_parser()
        .to_options()
        .descr("Read-only projection of standby facts whose wake_after has passed. Emits suggested_command strings only — never executes anything.")
        .command("wake-due")
        .map(CliCommand::WakeDue);

    // B-whoami: identity report (read-only)
    let whoami = whoami_parser()
        .to_options()
        .descr("Print identity: repo_root, repo_id, worktree, build_id, cwd. Read-only, --json supported.")
        .command("whoami")
        .map(CliCommand::Whoami);

    // Work surface commands (appended — do not reorder above)
    let backlog = backlog_parser()
        .to_options()
        .descr("Per-room claimable backlog: `add` an item or `list` open items.")
        .command("backlog")
        .map(CliCommand::Backlog);
    let board = board_parser()
        .to_options()
        .descr("Read-only board: lanes (in-flight/landed/closed), backlog, and delta.")
        .command("board")
        .map(CliCommand::Board);
    let route_findings = route_findings_parser()
        .to_options()
        .descr("Route findings from a JSON file to active claim owners; unowned → risk facts.")
        .command("route-findings")
        .map(CliCommand::RouteFindings);

    // Rank-11: room north-star + per-agent autonomy envelope (read-only get or append)
    let mission = mission_parser()
        .to_options()
        .descr("Get or set the room north-star (mission) and per-agent autonomy envelopes. Rally records and exposes only — never enforces.")
        .command("mission")
        .map(CliCommand::Mission);

    let lead = lead_parser()
        .to_options()
        .descr("Lead-agent title: show, hand off, or assign. Rally records/exposes only — never enforces.")
        .command("lead")
        .map(CliCommand::Lead);

    let ack = ack_parser()
        .to_options()
        .descr("Acknowledge the rally context (rules, guardrails, lead, mission) — coordination-mandate C1.")
        .command("ack")
        .map(CliCommand::Ack);

    let adopt = adopt_parser()
        .to_options()
        .descr("Register an already-running agent (tmux or cmux target) into the managed-session ledger without relaunching it. Use --tmux <target> or --cmux <target>; --backend (tmux|cmux) overrides the inferred backend.")
        .command("adopt")
        .map(CliCommand::Adopt);

    construct!([
        init,
        enter,
        say,
        room,
        next,
        locate,
        recent,
        check,
        run,
        sessions,
        inject,
        attach,
        capture,
        stop,
        retrospective,
        rotate,
        status,
        watch,
        migrate_legacy,
        doctor,
        version,
        backlog,
        board,
        route_findings,
        check_ci,
        dag,
        wake_due,
        whoami,
        mission,
        lead,
        ack,
        adopt
    ])
    .to_options()
}

fn status_parser() -> impl Parser<StatusArgs> {
    // `rally status post --tool T --state <s> [--file P] [--intent I]
    //  [--blocked-ref ID] [--wake-after ISO] [--committed-sha SHA]
    //  [--worktree-branch BRANCH]`
    let post_tool = string_arg("tool", "TOOL");
    let post_state = string_arg("state", "STATE");
    let post_file = optional_string_arg("file", "PATH");
    let post_intent = optional_string_arg("intent", "INTENT");
    let post_blocked_ref = optional_string_arg("blocked-ref", "EVENT_ID");
    let post_wake_after = optional_string_arg("wake-after", "ISO");
    let post_committed_sha = optional_string_arg("committed-sha", "SHA");
    let post_worktree_branch = optional_string_arg("worktree-branch", "BRANCH");
    let post_parser = construct!(
        post_tool,
        post_state,
        post_file,
        post_intent,
        post_blocked_ref,
        post_wake_after,
        post_committed_sha,
        post_worktree_branch
    )
    .map(
        |(tool, state, file, intent, blocked_ref, wake_after, committed_sha, worktree_branch)| {
            StatusPostArgs {
                tool,
                state,
                file,
                intent,
                blocked_ref,
                wake_after,
                committed_sha,
                worktree_branch,
            }
        },
    )
    .to_options()
    .descr("Append a typed agent-state heartbeat (idle|working|blocked|done).")
    .command("post")
    .map(StatusSubcommand::Post);

    // `rally status read [--tool T]`
    let read_tool = optional_string_arg("tool", "TOOL");
    let read_parser = read_tool
        .map(|tool| StatusReadArgs { tool })
        .to_options()
        .descr("Read the latest typed agent-state, projected per tool.")
        .command("read")
        .map(StatusSubcommand::Read);

    // `rally status --global` — existing multi-repo discovery aggregation,
    // preserved as a flag for back-compat.
    let global_parser = long("global")
        .switch()
        .guard(|on| *on, "rally status requires a subcommand (post|read) or --global")
        .map(|_| StatusSubcommand::Global);

    let json = json_flag();
    let subcommand = construct!([post_parser, read_parser, global_parser]);
    construct!(json, subcommand).map(|(json, subcommand)| StatusArgs { json, subcommand })
}

fn init_parser() -> impl Parser<InitArgs> {
    let json = json_flag();
    construct!(InitArgs { json })
}

fn rotate_parser() -> impl Parser<RotateArgs> {
    let json = json_flag();
    let days = optional_i64_arg("days", "N");
    let dry_run = dry_run_flag();
    construct!(RotateArgs {
        json,
        days,
        dry_run
    })
}

fn retrospective_parser() -> impl Parser<RetrospectiveArgs> {
    let json = json_flag();
    let out = optional_string_arg("out", "PATH");
    let engagement = optional_string_arg("engagement", "LABEL");
    construct!(RetrospectiveArgs {
        json,
        out,
        engagement
    })
}

fn enter_parser() -> impl Parser<EnterArgs> {
    let json = json_flag();
    let tool = string_arg("tool", "TOOL");
    let session_id = optional_string_arg("session-id", "SESSION_ID");
    let role = optional_string_arg("role", "ROLE");
    let paths = many_string_arg("path", "PATH");
    let since = optional_i64_arg("since", "SEQ");
    let engagement = optional_string_arg("engagement", "LABEL");
    let tier = optional_string_arg("tier", "TIER");
    construct!(EnterArgs {
        json,
        tool,
        session_id,
        role,
        paths,
        since,
        engagement,
        tier
    })
}

fn say_parser() -> impl Parser<SayArgs> {
    let json = json_flag();
    let tool = string_arg("tool", "TOOL");
    let subject = optional_string_arg("subject", "SUBJECT");
    let thread_id = optional_string_arg("thread-id", "THREAD_ID");
    let role = optional_string_arg("role", "ROLE");
    let summary = optional_string_arg("summary", "SUMMARY");
    let scopes = many_string_arg("scope", "SCOPE");
    let resources = many_string_arg("resource", "RESOURCE");
    let paths = many_string_arg("path", "PATH");
    let evidence = many_string_arg("evidence", "EVIDENCE");
    let target = target_arg();
    let ref_id = optional_string_arg("ref", "EVENT_ID");
    let status = optional_string_arg("status", "STATUS");
    let severity = optional_string_arg("severity", "SEVERITY");
    let uri = optional_string_arg("uri", "URI");
    // B13: predictive contract markers (repeatable)
    let produces = many_string_arg("produces", "PATH_OR_SYMBOL");
    let depends = many_string_arg("depends", "PATH_OR_SYMBOL");
    // B1: lineage markers (all optional)
    let run_id = optional_string_arg("run", "RUN_ID");
    let step_id = optional_string_arg("step", "STEP_ID");
    let parent_step_id = optional_string_arg("parent-step", "STEP_ID");
    // B1: standby/wake specific args
    let reason = optional_string_arg("reason", "REASON");
    let wake_after = optional_string_arg("wake-after", "OFFSET_OR_ISO");
    let ref_standby = optional_string_arg("ref-standby", "EVENT_ID");
    let kind = positional::<String>("KIND").parse(parse_fact_kind);
    construct!(
        json,
        tool,
        subject,
        thread_id,
        role,
        summary,
        scopes,
        resources,
        paths,
        evidence,
        target,
        ref_id,
        status,
        severity,
        uri,
        produces,
        depends,
        run_id,
        step_id,
        parent_step_id,
        reason,
        wake_after,
        ref_standby,
        kind
    )
    .map(
        |(
            json,
            tool,
            subject,
            thread_id,
            role,
            summary,
            scopes,
            resources,
            paths,
            evidence,
            target,
            ref_id,
            status,
            severity,
            uri,
            produces,
            depends,
            run_id,
            step_id,
            parent_step_id,
            reason,
            wake_after,
            ref_standby,
            kind,
        )| SayArgs {
            json,
            kind,
            tool,
            subject,
            thread_id,
            role,
            summary,
            scopes,
            resources,
            paths,
            evidence,
            target,
            ref_id,
            status,
            severity,
            uri,
            produces,
            depends,
            run_id,
            step_id,
            parent_step_id,
            reason,
            wake_after,
            ref_standby,
        },
    )
}

fn room_parser() -> impl Parser<RoomArgs> {
    let json = json_flag();
    let tool = optional_string_arg("tool", "TOOL");
    let role = optional_string_arg("role", "ROLE");
    let paths = many_string_arg("path", "PATH");
    let event_id = optional_string_arg("event", "EVENT_ID");
    let thread_id = optional_string_arg("thread", "THREAD_ID");
    let since = optional_i64_arg("since", "SEQ");
    let readers = long("readers")
        .help("R10: project per-tool read receipts from ledger read-checkpoint facts")
        .switch();
    construct!(RoomArgs {
        json,
        tool,
        role,
        paths,
        event_id,
        thread_id,
        since,
        readers
    })
}

fn next_parser() -> impl Parser<NextArgs> {
    let json = json_flag();
    let tool = string_arg("tool", "TOOL");
    let role = optional_string_arg("role", "ROLE");
    let paths = many_string_arg("path", "PATH");
    let limit = bounded_i64_arg("limit", "N", 5, 1, 20);
    construct!(NextArgs {
        json,
        tool,
        role,
        paths,
        limit
    })
}

fn locate_parser() -> impl Parser<LocateArgs> {
    let json = json_flag();
    let event_id = positional::<String>("EVENT_ID");
    construct!(json, event_id).map(|(json, event_id)| LocateArgs { json, event_id })
}

fn recent_parser() -> impl Parser<RecentArgs> {
    let json = json_flag();
    let all = long("all").switch();
    let limit = bounded_i64_arg("limit", "N", 20, 1, 500);
    construct!(RecentArgs { json, all, limit })
}

fn migrate_legacy_parser() -> impl Parser<MigrateLegacyArgs> {
    let json = json_flag();
    construct!(MigrateLegacyArgs { json })
}

fn doctor_parser() -> impl Parser<DoctorArgs> {
    let json = json_flag();
    let canonical_paths = long("canonical-paths")
        .help("Report non-canonical scopes and suffix collisions in active claims")
        .switch();
    let prune_rooms = long("prune-rooms")
        .help("Classify rooms registry entries as live/stale (dry-run by default)")
        .switch();
    let apply = long("apply")
        .help("Apply the prune: rewrite the registry index, keeping only live entries")
        .switch();
    construct!(DoctorArgs {
        json,
        canonical_paths,
        prune_rooms,
        apply
    })
}

fn check_parser() -> impl Parser<CheckArgs> {
    let json = json_flag();
    let tool = optional_string_arg("tool", "TOOL");
    let path = optional_string_arg("path", "PATH");
    let strict = long("strict").switch();
    // #9 tier-fit advisory args (ignored for non-tier-fit phases)
    let role = optional_string_arg("role", "ROLE");
    let proposed_tier = optional_string_arg("proposed-tier", "TIER");
    let enforce = long("enforce")
        .help("liveness: release conflicted-out squads' claims + alert (never blocks).")
        .switch();
    let changed = many_string_arg("changed", "PATH");
    let phase = positional::<String>("PHASE")
        .optional()
        .map(|phase| phase.unwrap_or_else(|| "before-write".to_string()));
    construct!(
        json,
        tool,
        path,
        strict,
        role,
        proposed_tier,
        enforce,
        changed,
        phase
    )
    .map(
        |(json, tool, path, strict, role, proposed_tier, enforce, changed, phase)| CheckArgs {
            json,
            phase,
            tool,
            path,
            strict,
            role,
            proposed_tier,
            enforce,
            changed,
        },
    )
}

fn run_parser() -> impl Parser<RunArgs> {
    let json = json_flag();
    let dry_run = dry_run_flag();
    let name = optional_string_arg("name", "NAME");
    let backend = backend_arg();
    let session_id = optional_string_arg("session-id", "SESSION_ID");
    let tool = optional_string_arg("tool", "TOOL");
    let bins = backend_bins_parser();
    let agent = positional::<String>("AGENT");
    // Two spellings, both meaning "opt out of per-agent worktree provisioning".
    // Either flag yields `shared = true`; default = false (isolated).
    let shared = long("shared").switch();
    let no_worktree = long("no-worktree").switch();
    let shared = construct!(shared, no_worktree).map(|(a, b)| a || b);
    construct!(
        json, dry_run, name, backend, session_id, tool, bins, shared, agent
    )
    .map(
        |(json, dry_run, name, backend, session_id, tool, bins, shared, agent)| RunArgs {
            json,
            dry_run,
            agent,
            name,
            backend,
            session_id,
            tool,
            bins,
            shared,
        },
    )
}

fn sessions_parser() -> impl Parser<SessionsArgs> {
    let json = json_flag();
    construct!(SessionsArgs { json })
}

fn inject_parser() -> impl Parser<InjectArgs> {
    let json = json_flag();
    let dry_run = dry_run_flag();
    let text = optional_string_arg("text", "TEXT");
    let handoff = handoff_arg();
    let require_ack = long("require-ack").switch();
    let timeout_seconds = bounded_i64_arg("timeout-seconds", "SECONDS", 60, 1, 600);
    let bins = backend_bins_parser();
    let tool = optional_string_arg("tool", "TOOL")
        .map(|value| value.unwrap_or_else(|| "unknown".to_string()));
    // Plan F sync override. Restricted by the daemon to Stop|Retraction
    // semantics — see InjectArgs::urgent docstring + F plan §sync override.
    let urgent = long("urgent")
        .help("Plan F sync override: daemon writes the directive synchronously to the PTY. Only honored for Stop|Retraction; the daemon REJECTS urgent on Deliver+Addition/Revision to protect TUI integrity.")
        .switch();
    let target = positional::<String>("TARGET");
    construct!(
        json,
        dry_run,
        text,
        handoff,
        require_ack,
        timeout_seconds,
        bins,
        tool,
        urgent,
        target
    )
    .map(
        |(json, dry_run, text, handoff, require_ack, timeout_seconds, bins, tool, urgent, target)| {
            InjectArgs {
                json,
                dry_run,
                target,
                text,
                handoff,
                require_ack,
                timeout_seconds,
                bins,
                tool,
                urgent,
            }
        },
    )
}

fn session_action_parser(action: SessionAction) -> impl Parser<SessionActionArgs> {
    let json = json_flag();
    let dry_run = dry_run_flag();
    let lines = bounded_i64_arg("lines", "N", 120, 1, 2000);
    let bins = backend_bins_parser();
    let target = positional::<String>("TARGET");
    construct!(json, dry_run, lines, bins, target).map(
        move |(json, dry_run, lines, bins, target)| SessionActionArgs {
            json,
            dry_run,
            action,
            target,
            lines,
            bins,
        },
    )
}

fn watch_parser() -> impl Parser<WatchArgs> {
    let tool = optional_string_arg("tool", "TOOL");
    let interval = string_arg("interval", "SECS")
        .parse(|v| parse_i64_arg("interval", v))
        .parse(|v| {
            if v > 0 {
                Ok(v as u64)
            } else {
                Err(RallyError::Usage("--interval must be > 0".to_string()))
            }
        })
        .fallback(5u64);
    let max_interval = string_arg("max-interval", "SECS")
        .parse(|v| parse_i64_arg("max-interval", v))
        .parse(|v| {
            if v > 0 {
                Ok(v as u64)
            } else {
                Err(RallyError::Usage("--max-interval must be > 0".to_string()))
            }
        })
        .fallback(300u64);
    let on_activity = optional_string_arg("on-activity", "CMD");
    let once = long("once").switch();
    let duration_hours = string_arg("duration-hours", "HOURS")
        .parse(|v| {
            v.parse::<f64>()
                .map_err(|_| RallyError::Usage(format!("invalid --duration-hours value {v}")))
        })
        .optional();
    let json = json_flag();
    let print_launchd = long("print-launchd").switch();
    let print_systemd = long("print-systemd").switch();
    construct!(
        tool,
        interval,
        max_interval,
        on_activity,
        once,
        duration_hours,
        json,
        print_launchd,
        print_systemd
    )
    .map(
        |(
            tool,
            interval,
            max_interval,
            on_activity,
            once,
            duration_hours,
            json,
            print_launchd,
            print_systemd,
        )| WatchArgs {
            tool,
            interval,
            max_interval,
            on_activity,
            once,
            duration_hours,
            json,
            print_launchd,
            print_systemd,
        },
    )
}

fn backend_bins_parser() -> impl Parser<BackendBins> {
    // PROVENANCE: previously also parsed `--herdr-bin <PATH>` and `--herdr-socket <PATH>`
    // for `Backend::Herdr`. Both flags were ignored at runtime once Plan F retired the
    // herdr lane (BackendRunner discarded the fields). They are now removed from the
    // CLI surface entirely so `rally inject --help` no longer advertises a no-op flag.
    let tmux_bin = optional_string_arg("tmux-bin", "PATH")
        .map(|value| value.unwrap_or_else(|| "tmux".to_string()));
    let cmux_bin = optional_string_arg("cmux-bin", "PATH")
        .map(|value| value.unwrap_or_else(|| "cmux".to_string()));
    construct!(BackendBins { tmux_bin, cmux_bin })
}

fn json_flag() -> impl Parser<bool> {
    long("json").switch()
}

fn dry_run_flag() -> impl Parser<bool> {
    long("dry-run").switch()
}

fn string_arg(name: &'static str, metavar: &'static str) -> impl Parser<String> {
    long(name).argument::<String>(metavar)
}

fn optional_string_arg(name: &'static str, metavar: &'static str) -> impl Parser<Option<String>> {
    string_arg(name, metavar).optional()
}

fn many_string_arg(name: &'static str, metavar: &'static str) -> impl Parser<Vec<String>> {
    string_arg(name, metavar).many()
}

fn optional_i64_arg(name: &'static str, metavar: &'static str) -> impl Parser<Option<i64>> {
    string_arg(name, metavar)
        .parse(move |value| parse_i64_arg(name, value))
        .optional()
}

fn bounded_i64_arg(
    name: &'static str,
    metavar: &'static str,
    default: i64,
    min: i64,
    max: i64,
) -> impl Parser<i64> {
    string_arg(name, metavar)
        .parse(move |value| parse_i64_arg(name, value))
        .parse(move |value| {
            if (min..=max).contains(&value) {
                Ok(value)
            } else {
                Err(RallyError::Usage(format!(
                    "--{name} must be between {min} and {max}, got {value}"
                )))
            }
        })
        .fallback(default)
}

fn backend_arg() -> impl Parser<Backend> {
    string_arg("backend", "BACKEND")
        .parse(|value| Backend::parse(&value))
        .fallback(Backend::Tmux)
}

fn target_arg() -> impl Parser<Option<String>> {
    let target = optional_string_arg("target", "TOOL");
    let to = optional_string_arg("to", "TOOL");
    construct!(target, to).parse(|(target, to)| match (target, to) {
        (Some(_), Some(_)) => Err(RallyError::Usage(
            "cannot use --target and --to together".to_string(),
        )),
        (target, to) => Ok(target.or(to)),
    })
}

fn handoff_arg() -> impl Parser<Option<String>> {
    let handoff = optional_string_arg("handoff", "EVENT_ID");
    let ref_id = optional_string_arg("ref", "EVENT_ID");
    construct!(handoff, ref_id).parse(|(handoff, ref_id)| match (handoff, ref_id) {
        (Some(_), Some(_)) => Err(RallyError::Usage(
            "cannot use --handoff and --ref together".to_string(),
        )),
        (handoff, ref_id) => Ok(handoff.or(ref_id)),
    })
}

fn parse_fact_kind(value: String) -> Result<FactKind> {
    FactKind::parse(&value)
        .ok_or_else(|| RallyError::Usage(format!("unsupported fact kind {value}")))
}

fn parse_i64_arg(name: &str, value: String) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| RallyError::Usage(format!("invalid --{name} value {value}")))
}

fn version_parser() -> impl Parser<VersionArgs> {
    let json = json_flag();
    construct!(VersionArgs { json })
}

fn whoami_parser() -> impl Parser<WhoamiArgs> {
    let json = json_flag();
    let tool = optional_string_arg("tool", "TOOL");
    construct!(WhoamiArgs { json, tool })
}

// ─── Work surface parsers (appended) ─────────────────────────────────────────

fn backlog_parser() -> impl Parser<BacklogArgs> {
    // `rally backlog add --tool .. --id .. --intent .. [--owns ..] [--depends-on ..]`
    let tool = string_arg("tool", "TOOL");
    let id = string_arg("id", "ID");
    let intent = string_arg("intent", "INTENT");
    let owns = many_string_arg("owns", "PATH");
    let depends_on = many_string_arg("depends-on", "ID");
    let add_parser = construct!(tool, id, intent, owns, depends_on)
        .map(|(tool, id, intent, owns, depends_on)| BacklogAddArgs {
            tool,
            id,
            intent,
            owns,
            depends_on,
        })
        .to_options()
        .descr("Add a new backlog item to the room ledger.")
        .command("add")
        .map(BacklogSubcommand::Add);

    // `rally backlog list [--json]`
    let list_parser = bpaf::pure(())
        .to_options()
        .descr("List open backlog items.")
        .command("list")
        .map(|_| BacklogSubcommand::List);

    // `rally backlog done --tool .. --id ..`
    let done_tool = string_arg("tool", "TOOL");
    let done_id = string_arg("id", "ID");
    let done_parser = construct!(done_tool, done_id)
        .map(|(tool, id)| BacklogDoneArgs { tool, id })
        .to_options()
        .descr("Mark a backlog item done (closes it; drops out of `list`).")
        .command("done")
        .map(BacklogSubcommand::Done);

    let json = json_flag();
    let subcommand = construct!([add_parser, list_parser, done_parser]);
    construct!(json, subcommand).map(|(json, subcommand)| BacklogArgs { json, subcommand })
}

fn ack_parser() -> impl Parser<AckArgs> {
    let json = json_flag();
    let tool = string_arg("tool", "TOOL");
    construct!(AckArgs { json, tool })
}

/// C-FLEET: parser for `rally adopt <name> [--tmux …|--cmux …] [--tool …]
/// [--agent …] [--backend …]`. Mutual exclusion between `--tmux` and `--cmux`
/// is validated inside `command_adopt`. HERDR-INDEPENDENT: no `--pane` arm.
fn adopt_parser() -> impl Parser<AdoptArgs> {
    let json = json_flag();
    let tmux = optional_string_arg("tmux", "TARGET");
    let cmux = optional_string_arg("cmux", "TARGET");
    let tool = optional_string_arg("tool", "TOOL");
    let agent = optional_string_arg("agent", "AGENT");
    let backend = long("backend")
        .help("Backend hint (tmux|cmux). Auto-inferred from --tmux/--cmux when omitted.")
        .argument::<String>("BACKEND")
        .optional()
        .parse(|opt| match opt {
            None => Ok(None),
            Some(s) => Backend::parse(&s).map(Some),
        });
    // Positional MUST be the rightmost item per bpaf convention.
    let name = positional::<String>("NAME");
    construct!(json, tmux, cmux, tool, agent, backend, name).map(
        |(json, tmux, cmux, tool, agent, backend, name)| AdoptArgs {
            json,
            name,
            tmux,
            cmux,
            tool,
            agent,
            backend,
        },
    )
}

fn lead_parser() -> impl Parser<LeadArgs> {
    let show = bpaf::pure(())
        .to_options()
        .descr("Show the current lead, its tier, and how it was assigned.")
        .command("show")
        .map(|_| LeadSubcommand::Show);
    let h_tool = string_arg("tool", "TOOL");
    let h_to = string_arg("to", "TOOL");
    let handoff = construct!(h_tool, h_to)
        .map(|(tool, to)| LeadTargetArgs {
            tool,
            to,
            user_designated: false,
        })
        .to_options()
        .descr("Hand the lead title to another (frontier) agent.")
        .command("handoff")
        .map(LeadSubcommand::Handoff);
    let a_tool = string_arg("tool", "TOOL");
    let a_to = string_arg("to", "TOOL");
    let a_ud = long("user-designated")
        .help("Mark as user-designated (supersedes a first-join lead).")
        .switch();
    let assign = construct!(a_tool, a_to, a_ud)
        .map(|(tool, to, user_designated)| LeadTargetArgs {
            tool,
            to,
            user_designated,
        })
        .to_options()
        .descr("Assign the lead (user-designated supersedes first-join).")
        .command("assign")
        .map(LeadSubcommand::Assign);
    let r_tool = string_arg("tool", "TOOL");
    let relinquish = construct!(r_tool)
        .map(|tool| LeadRelinquishArgs { tool })
        .to_options()
        .descr("Relinquish the lead title (reopens the seat).")
        .command("relinquish")
        .map(LeadSubcommand::Relinquish);
    let json = json_flag();
    let subcommand = construct!([show, handoff, assign, relinquish]);
    construct!(json, subcommand).map(|(json, subcommand)| LeadArgs { json, subcommand })
}

fn board_parser() -> impl Parser<BoardArgs> {
    let json = json_flag();
    construct!(BoardArgs { json })
}

fn route_findings_parser() -> impl Parser<RouteFindingsArgs> {
    let json = json_flag();
    let file = string_arg("file", "PATH");
    let tool =
        optional_string_arg("tool", "TOOL").map(|v| v.unwrap_or_else(|| "unknown".to_string()));
    let verified = long("verified")
        .help("Affirm that FP-adjudication has already happened (required)")
        .switch();
    construct!(json, file, tool, verified).map(|(json, file, tool, verified)| RouteFindingsArgs {
        json,
        file,
        tool,
        verified,
    })
}

// B2: dag parser
fn dag_parser() -> impl Parser<DagArgs> {
    let json = json_flag();
    let run_id = string_arg("run", "RUN_ID");
    construct!(json, run_id).map(|(json, run_id)| DagArgs { json, run_id })
}

// B4: wake-due parser
fn wake_due_parser() -> impl Parser<WakeDueArgs> {
    let json = json_flag();
    let tool = optional_string_arg("tool", "TOOL");
    construct!(json, tool).map(|(json, tool)| WakeDueArgs { json, tool })
}

// Rank-11: mission parser
fn mission_parser() -> impl Parser<MissionArgs> {
    let json = json_flag();
    let set = optional_string_arg("set", "TEXT");
    let tool = optional_string_arg("tool", "TOOL");
    let may = optional_string_arg("may", "TEXT");
    let must_check = optional_string_arg("must-check", "TEXT");
    construct!(json, set, tool, may, must_check).map(|(json, set, tool, may, must_check)| {
        MissionArgs {
            json,
            set,
            tool,
            may,
            must_check,
        }
    })
}

// B13: check-ci parser
fn check_ci_parser() -> impl Parser<CheckCiArgs> {
    let json = json_flag();
    let strict = long("strict")
        .help("Exit 4 when any offender is found (default: exit 0, warn only)")
        .switch();
    let receipt_threshold_secs = string_arg("receipt-threshold-secs", "SECS")
        .parse(|v| {
            v.parse::<u64>().map_err(|_| {
                RallyError::Usage(format!("invalid --receipt-threshold-secs value {v}"))
            })
        })
        .fallback(3600u64);
    construct!(json, strict, receipt_threshold_secs).map(
        |(json, strict, receipt_threshold_secs)| CheckCiArgs {
            json,
            strict,
            receipt_threshold_secs,
        },
    )
}
