use bpaf::{Args, OptionParser, ParseFailure, Parser, construct, long, positional};

use crate::SessionAction;
use crate::backends::Backend;
use crate::error::{RallyError, Result};
use crate::store::FactKind;

pub(crate) enum CliCommand {
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
}

pub(crate) enum CliParse {
    Command(Box<CliCommand>),
    Help(String),
}

#[derive(Clone, Debug)]
pub(crate) struct EnterArgs {
    pub(crate) json: bool,
    pub(crate) tool: String,
    pub(crate) session_id: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) since: Option<i64>,
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
    pub(crate) include_legacy: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RecentArgs {
    pub(crate) json: bool,
    pub(crate) all: bool,
    pub(crate) include_legacy: bool,
    pub(crate) limit: i64,
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
    "enter", "say", "room", "next", "check", "run", "sessions", "inject", "attach", "capture",
    "stop", "locate", "recent",
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

    construct!([
        enter, say, room, next, locate, recent, check, run, sessions, inject, attach, capture, stop
    ])
    .to_options()
}

fn enter_parser() -> impl Parser<EnterArgs> {
    let json = json_flag();
    let tool = string_arg("tool", "TOOL");
    let session_id = optional_string_arg("session-id", "SESSION_ID");
    let role = optional_string_arg("role", "ROLE");
    let paths = many_string_arg("path", "PATH");
    let since = optional_i64_arg("since", "SEQ");
    construct!(EnterArgs {
        json,
        tool,
        session_id,
        role,
        paths,
        since
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
    construct!(RoomArgs {
        json,
        tool,
        role,
        paths,
        event_id,
        thread_id,
        since
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
    let include_legacy = long("include-legacy").switch();
    let event_id = positional::<String>("EVENT_ID");
    construct!(json, include_legacy, event_id).map(|(json, include_legacy, event_id)| LocateArgs {
        json,
        event_id,
        include_legacy,
    })
}

fn recent_parser() -> impl Parser<RecentArgs> {
    let json = json_flag();
    let all = long("all").switch();
    let include_legacy = long("include-legacy").switch();
    let limit = bounded_i64_arg("limit", "N", 20, 1, 500);
    construct!(RecentArgs {
        json,
        all,
        include_legacy,
        limit
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
    let target = positional::<String>("TARGET");
    construct!(
        json,
        dry_run,
        text,
        handoff,
        require_ack,
        timeout_seconds,
        bins,
        target
    )
    .map(
        |(json, dry_run, text, handoff, require_ack, timeout_seconds, bins, target)| InjectArgs {
            json,
            dry_run,
            target,
            text,
            handoff,
            require_ack,
            timeout_seconds,
            bins,
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
