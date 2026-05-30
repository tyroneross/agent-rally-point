use bpaf::{Args, OptionParser, ParseFailure, Parser, construct, long, positional};

use crate::SessionAction;
use crate::backends::Backend;
use crate::error::{RallyError, Result};
use crate::store::FactKind;

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
    /// Aggregate status across all known repo rooms (required flag).
    pub(crate) global: bool,
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
}

#[derive(Clone, Debug)]
pub(crate) struct BacklogAddArgs {
    pub(crate) tool: String,
    pub(crate) id: String,
    pub(crate) intent: String,
    pub(crate) owns: Vec<String>,
    pub(crate) depends_on: Vec<String>,
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

#[derive(Clone, Debug)]
pub(crate) struct BackendBins {
    pub(crate) tmux_bin: String,
    pub(crate) herdr_bin: String,
    pub(crate) cmux_bin: String,
}

impl Default for BackendBins {
    fn default() -> Self {
        Self {
            tmux_bin: "tmux".to_string(),
            herdr_bin: "herdr".to_string(),
            cmux_bin: "cmux".to_string(),
        }
    }
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
        route_findings
    ])
    .to_options()
}

fn status_parser() -> impl Parser<StatusArgs> {
    let json = json_flag();
    let global = long("global").switch();
    construct!(StatusArgs { json, global })
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
    construct!(EnterArgs {
        json,
        tool,
        session_id,
        role,
        paths,
        since,
        engagement
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
    let kind = positional::<String>("KIND").parse(parse_fact_kind);
    construct!(
        json, tool, subject, thread_id, role, summary, scopes, resources, paths, evidence, target,
        ref_id, status, severity, uri, kind
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
    let phase = positional::<String>("PHASE")
        .optional()
        .map(|phase| phase.unwrap_or_else(|| "before-write".to_string()));
    construct!(json, tool, path, strict, phase).map(|(json, tool, path, strict, phase)| CheckArgs {
        json,
        phase,
        tool,
        path,
        strict,
    })
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
    construct!(json, dry_run, name, backend, session_id, tool, bins, agent).map(
        |(json, dry_run, name, backend, session_id, tool, bins, agent)| RunArgs {
            json,
            dry_run,
            agent,
            name,
            backend,
            session_id,
            tool,
            bins,
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
        target
    )
    .map(
        |(json, dry_run, text, handoff, require_ack, timeout_seconds, bins, tool, target)| {
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
                Err(RallyError::Usage(
                    "--max-interval must be > 0".to_string(),
                ))
            }
        })
        .fallback(300u64);
    let on_activity = optional_string_arg("on-activity", "CMD");
    let once = long("once").switch();
    let duration_hours = string_arg("duration-hours", "HOURS")
        .parse(|v| {
            v.parse::<f64>().map_err(|_| {
                RallyError::Usage(format!("invalid --duration-hours value {v}"))
            })
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
    let tmux_bin = optional_string_arg("tmux-bin", "PATH")
        .map(|value| value.unwrap_or_else(|| "tmux".to_string()));
    let herdr_bin = optional_string_arg("herdr-bin", "PATH")
        .map(|value| value.unwrap_or_else(|| "herdr".to_string()));
    let cmux_bin = optional_string_arg("cmux-bin", "PATH")
        .map(|value| value.unwrap_or_else(|| "cmux".to_string()));
    construct!(BackendBins {
        tmux_bin,
        herdr_bin,
        cmux_bin
    })
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

    let json = json_flag();
    let subcommand = construct!([add_parser, list_parser]);
    construct!(json, subcommand).map(|(json, subcommand)| BacklogArgs { json, subcommand })
}

fn board_parser() -> impl Parser<BoardArgs> {
    let json = json_flag();
    construct!(BoardArgs { json })
}

fn route_findings_parser() -> impl Parser<RouteFindingsArgs> {
    let json = json_flag();
    let file = string_arg("file", "PATH");
    let tool = optional_string_arg("tool", "TOOL")
        .map(|v| v.unwrap_or_else(|| "unknown".to_string()));
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
