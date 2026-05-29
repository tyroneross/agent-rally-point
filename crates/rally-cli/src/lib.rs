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
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

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
mod check;
mod cli;
mod discovery;
mod error;
mod next;
mod output;
mod store;

use backends::*;
use check::build_check;
use cli::*;
use error::{RallyError, Result};
use next::{AttentionItem, EntryData, NextResult, build_attention, build_entry, build_next};
use output::{CliError, Output};
use store::{Fact, FactKind, RoomQuery, RoomSnapshot, RoomStore, RoomSummary};

pub fn main() -> ExitCode {
    let wants_json = env::args().any(|arg| arg == "--json");
    match run_inner() {
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

fn run_inner() -> Result<Output> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "help")
        )
    {
        return Ok(Output::new(false, help_text(), json!({})));
    }
    reject_unknown_command(&args)?;

    match parse_cli(&args)? {
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
    }
}

fn command_enter(args: EnterArgs) -> Result<Output> {
    let tool = args.tool;
    let session_id = args.session_id.unwrap_or_else(|| format!("session-{tool}"));
    let role = args.role;
    let paths = normalize_paths(args.paths);
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let cursor_before = args
        .since
        .unwrap_or_else(|| room.cursor_for(&tool).unwrap_or(0));
    let max_seq = snapshot.max_seq;
    let attention = build_attention(&snapshot, &tool, cursor_before, &paths);
    let entry = build_entry(&snapshot, &tool, role.as_deref(), &paths, &attention);
    let _ = room.set_cursor(&tool, max_seq);
    let body = envelope(
        "enter",
        SCHEMA_ENTER,
        EnterData {
            tool,
            session_id,
            cursor: CursorData {
                before: cursor_before,
                after: max_seq,
                advanced: true,
            },
            entry,
            attention,
            room: RoomSummary::from(&snapshot),
        },
    );
    let text = format!(
        "entered room tool={} do={} do_not={} attention={}",
        body["data"]["tool"].as_str().unwrap_or("unknown"),
        body["data"]["entry"]["do"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        body["data"]["entry"]["do_not"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        body["data"]["attention"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    Ok(Output::new(args.json, text, body))
}

fn command_say(args: SayArgs) -> Result<Output> {
    let kind = args.kind;
    let subject = args
        .subject
        .unwrap_or_else(|| default_subject(kind.as_str()));
    let scope = scopes_from(args.scopes, args.resources, args.paths);
    let fact = Fact {
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("fact"),
        seq: 0,
        thread_id: args.thread_id.unwrap_or_else(|| new_id("room")),
        kind,
        tool: Some(args.tool),
        role: args.role,
        subject,
        scope,
        created_at: now_string(),
        summary: args.summary,
        evidence: args.evidence,
        target: args.target,
        ref_id: args.ref_id,
        status: args.status,
        severity: args.severity,
        uri: args.uri,
        session: None,
    };
    let room = RoomStore::open()?;
    let fact = room.append_fact(&fact)?;
    let snapshot = room.snapshot()?;
    let body = envelope(
        "say",
        SCHEMA_SAY,
        SayData {
            fact: fact.clone(),
            room: RoomSummary::from(&snapshot),
        },
    );
    let text = format!("said {} {}", fact.kind.as_str(), fact.event_id);
    Ok(Output::new(args.json, text, body))
}

fn command_room(args: RoomArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let json_output = args.json;
    let query = RoomQuery::from(args);
    let snapshot = room.snapshot()?.filtered(&query);
    let body = envelope(
        "room",
        SCHEMA_ROOM,
        RoomData {
            query,
            room: snapshot.clone(),
        },
    );
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
    let snapshot = room.snapshot()?;
    let next = build_next(&snapshot, &tool, role.as_deref(), &paths, limit);
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
    );
    let text = format!("next action={action} target={target_event_id}");
    Ok(Output::new(args.json, text, body))
}

fn command_locate(args: LocateArgs) -> Result<Output> {
    let data = discovery::locate(&args.event_id, args.include_legacy)?;
    let found = data.located.is_some();
    let body = envelope("locate", SCHEMA_LOCATE, data);
    let text = format!("locate event={} found={}", args.event_id, found);
    Ok(Output::new(args.json, text, body))
}

fn command_recent(args: RecentArgs) -> Result<Output> {
    let data = discovery::recent(args.all, args.include_legacy, args.limit)?;
    let count = data.rows.len();
    let body = envelope("recent", SCHEMA_RECENT, data);
    let text = format!("recent rows={count}");
    Ok(Output::new(args.json, text, body))
}

fn command_check(args: CheckArgs) -> Result<Output> {
    let phase = args.phase;
    let tool = args.tool.unwrap_or_else(|| "unknown".to_string());
    let path = args.path.map(normalize_path);
    let room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let check = build_check(phase, tool, path, args.strict, &snapshot)?;
    let body = envelope("check", SCHEMA_CHECK, check.data);
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
        backend_runner.start_commands(&session.target, &repo, &command, &session.name);

    let actual_target = if dry_run {
        session.target.clone()
    } else {
        match backend_runner.start(&session.target, &repo, &command, &session.name) {
            Ok(target) => target,
            Err(err) => {
                if let Some(fact) = &reservation.fact {
                    let _ = room.append_fact(&session_fact(
                        &session,
                        "stopped",
                        Some(fact.event_id.clone()),
                    ));
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
        RunData {
            mode: if dry_run { "dry-run" } else { "run" },
            session: session.clone(),
            commands: RunCommands {
                start: command_plan_json(&start_commands),
            },
        },
    );
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
    for _ in 0..SESSION_IDENTITY_RETRIES {
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
        if session.tool == identity.tool {
            return Err(RallyError::Usage(format!(
                "active managed session already uses tool {}",
                identity.tool
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
        SessionsData {
            sessions: sessions.clone(),
        },
    );
    let text = format!("sessions {}", sessions.len());
    Ok(Output::new(args.json, text, body))
}

fn command_inject(args: InjectArgs) -> Result<Output> {
    let dry_run = args.dry_run;
    let target = args.target;
    let session = find_session(&target)?;
    let handoff = args.handoff;
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
    let backend_runner = BackendRunner::new(Backend::parse(&session.backend)?, args.bins);
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let commands = backend_runner.inject_commands(&live_target, &text);
    if !dry_run {
        backend_runner.inject(&live_target, &text)?;
    }
    let wake_intent = inject_wake_intent(&session, handoff.as_deref(), &commands, dry_run)?;
    let ack = if require_ack && !dry_run {
        let handoff = handoff.as_deref().unwrap_or_default();
        Some(wait_for_resolution(handoff, timeout)?)
    } else {
        None
    };
    let body = envelope(
        "inject",
        SCHEMA_INJECT,
        InjectData {
            mode: if dry_run { "dry-run" } else { "inject" },
            session: session.clone(),
            handoff,
            require_ack,
            ack,
            wake_intent,
            commands: command_plan_json(&commands),
        },
    );
    let text = format!(
        "inject session={} ack={}",
        session.session_id,
        body["data"]["ack"].is_object()
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
    let body = envelope(
        command_name,
        SCHEMA_SESSION_ACTION,
        SessionActionData {
            mode: if dry_run { "dry-run" } else { command_name },
            action: command_name,
            session: session.clone(),
            output,
            commands: command_plan_json(&commands),
        },
    );
    let text =
        output_text.unwrap_or_else(|| format!("{command_name} session={}", session.session_id));
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

fn inject_wake_intent(
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
    } else {
        let room = RoomStore::open()?;
        room.append_fact(&fact).map(Some)
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

fn wait_for_resolution(handoff: &str, timeout_seconds: u64) -> Result<Value> {
    for _ in 0..timeout_seconds {
        let room = RoomStore::open()?;
        for fact in room.facts()? {
            if fact.kind == "resolve" && fact.ref_id.as_deref() == Some(handoff) {
                return Ok(json!({
                    "resolved": true,
                    "event_id": fact.event_id,
                    "tool": fact.tool,
                    "subject": fact.subject
                }));
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(RallyError::Message(format!(
        "timed out after {timeout_seconds}s waiting for resolve fact for {handoff}"
    )))
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
    shlex::try_quote(value)
        .expect("shell argument contains NUL byte")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[derive(JsonSchema, Serialize)]
struct CursorData {
    before: i64,
    after: i64,
    advanced: bool,
}

#[derive(JsonSchema, Serialize)]
struct EnterData {
    tool: String,
    session_id: String,
    cursor: CursorData,
    entry: EntryData,
    attention: Vec<AttentionItem>,
    room: RoomSummary,
}

#[derive(JsonSchema, Serialize)]
struct SayData {
    fact: Fact,
    room: RoomSummary,
}

#[derive(JsonSchema, Serialize)]
struct RoomData {
    query: RoomQuery,
    room: RoomSnapshot,
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
    if path.starts_with("file:") {
        return path;
    }
    let stripped = path.strip_prefix("./").unwrap_or(&path);
    format!("file:{stripped}")
}

pub(crate) fn path_matches_scope(scope: &str, path: &str) -> bool {
    let scope = comparable_path(scope);
    let path = comparable_path(path);
    !scope.is_empty() && (scope == path || path.starts_with(&format!("{scope}/")))
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

fn envelope<T: Serialize>(command: &str, schema: &str, data: T) -> Value {
    serde_json::to_value(Envelope {
        ok: true,
        product: "rally",
        command: command.to_string(),
        schema: schema.to_string(),
        data,
    })
    .unwrap_or_else(|_| json!({}))
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

fn help_text() -> String {
    [
        "rally: repo-local coordination room for parallel agents",
        "",
        "Usage:",
        "  rally enter --tool <tool> [--path <path>] [--role <role>] [--json]",
        "  rally say <kind> --tool <tool> --subject <subject> [--path <path>] [--json]",
        "  rally room [--tool <tool>] [--role <role>] [--path <path>] [--since <seq>] [--json]",
        "  rally next --tool <tool> [--path <path>] [--role <role>] [--limit <n>] [--json]",
        "  rally locate <event-id> [--include-legacy] [--json]",
        "  rally recent [--all] [--include-legacy] [--limit <n>] [--json]",
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
        "Fact kinds: claim, release, blocker, resolve, decision, artifact, handoff, risk, lesson, session, wake",
    ]
    .join("\n")
}
