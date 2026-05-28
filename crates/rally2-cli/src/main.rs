// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::{SecondsFormat, Utc};
use factstr::{EventQuery as FactQuery, EventStore, NewEvent};
use factstr_sqlite::SqliteStore;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_ENTER: &str = "agent-rally2.command.enter.v1";
const SCHEMA_SAY: &str = "agent-rally2.command.say.v1";
const SCHEMA_ROOM: &str = "agent-rally2.command.room.v1";
const SCHEMA_NEXT: &str = "agent-rally2.command.next.v1";
const SCHEMA_CHECK: &str = "agent-rally2.command.check.v1";
const SCHEMA_INSTALL: &str = "agent-rally2.command.install.v1";
const SCHEMA_RUN: &str = "agent-rally2.command.run.v1";
const SCHEMA_SESSIONS: &str = "agent-rally2.command.sessions.v1";
const SCHEMA_INJECT: &str = "agent-rally2.command.inject.v1";
const SCHEMA_SESSION_ACTION: &str = "agent-rally2.command.session-action.v1";
const FACT_SCHEMA: &str = "agent-rally2.fact.v1";
const DB_SCHEMA_VERSION: i64 = 2;
const INSTALL_MARKER: &str = "agent-rally2-install-v1";

macro_rules! cmd {
    ($($arg:expr),+ $(,)?) => {
        vec![$($arg.to_string()),+]
    };
}

fn main() -> ExitCode {
    let wants_json = env::args().any(|arg| arg == "--json");
    match run_inner() {
        Ok(output) => {
            let exit_code = output.exit_code;
            output.print();
            ExitCode::from(exit_code)
        }
        Err(err) => {
            let err = CliError::classify(err, wants_json);
            err.print();
            ExitCode::from(err.exit_code)
        }
    }
}

fn run_inner() -> Result<Output, String> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    let command = args
        .first()
        .cloned()
        .unwrap_or_else(|| "--help".to_string());
    if !args.is_empty() {
        args.remove(0);
    }
    match command.as_str() {
        "--help" | "-h" | "help" => Ok(Output::new(false, help_text(), json!({}))),
        "enter" => command_enter(ArgBag::new("enter", args)),
        "say" => command_say(ArgBag::new("say", args)),
        "room" => command_room(ArgBag::new("room", args)),
        "next" => command_next(ArgBag::new("next", args)),
        "check" => command_check(ArgBag::new("check", args)),
        "install" => command_install(ArgBag::new("install", args)),
        "run" => command_run(ArgBag::new("run", args)),
        "sessions" => command_sessions(ArgBag::new("sessions", args)),
        "inject" => command_inject(ArgBag::new("inject", args)),
        "attach" => command_session_action(ArgBag::new("attach", args), SessionAction::Attach),
        "capture" => command_session_action(ArgBag::new("capture", args), SessionAction::Capture),
        "stop" => command_session_action(ArgBag::new("stop", args), SessionAction::Stop),
        _ => Err(format!("unknown Rally 2 command {command}")),
    }
}

fn command_enter(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let tool = args.required("--tool")?;
    let session_id = args
        .one("--session-id")
        .unwrap_or_else(|| format!("session-{tool}"));
    let role = args.one("--role");
    let paths: Vec<String> = args.all("--path").into_iter().map(normalize_path).collect();
    let since = parse_i64_option(&args, "--since")?;
    let mut room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let cursor_before = since.unwrap_or_else(|| room.cursor_for(&tool).unwrap_or(0));
    let max_seq = snapshot.max_seq;
    let attention = build_attention(&snapshot, &tool, cursor_before, &paths);
    let entry = build_entry(&snapshot, &tool, role.as_deref(), &paths, &attention);
    let adapter = adapter_for(&tool);
    room.set_cursor(&tool, max_seq)?;
    let body = envelope(
        "enter",
        SCHEMA_ENTER,
        json!({
            "tool": tool,
            "session_id": session_id,
            "adapter": adapter,
            "cursor": {
                "before": cursor_before,
                "after": max_seq,
                "advanced": true
            },
            "entry": entry,
            "attention": attention,
            "room": snapshot.summary_json()
        }),
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
    Ok(Output::new(json_output, text, body))
}

fn command_say(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let kind = args
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "say requires a fact kind".to_string())?;
    if !matches!(
        kind.as_str(),
        "claim"
            | "release"
            | "blocker"
            | "resolve"
            | "decision"
            | "artifact"
            | "handoff"
            | "risk"
            | "lesson"
    ) {
        return Err(format!("unsupported fact kind {kind}"));
    }
    let tool = args.required("--tool")?;
    let subject = args
        .one("--subject")
        .unwrap_or_else(|| default_subject(&kind));
    let scope = scopes_from(&args);
    let evidence = args.all("--evidence");
    let now = now_string();
    let fact = json!({
        "schema": FACT_SCHEMA,
        "event_id": new_id("fact"),
        "thread_id": args.one("--thread-id").unwrap_or_else(|| new_id("room")),
        "created_at": now,
        "tool": tool,
        "role": args.one("--role"),
        "kind": kind,
        "subject": subject,
        "scope": scope,
        "summary": args.one("--summary"),
        "evidence": evidence,
        "target": args.one("--target").or_else(|| args.one("--to")),
        "ref": args.one("--ref"),
        "status": args.one("--status"),
        "severity": args.one("--severity"),
        "uri": args.one("--uri"),
        "origin": args.one("--origin").unwrap_or_else(|| "local".to_string()),
        "trust_status": args.one("--trust-status").unwrap_or_else(|| "local".to_string())
    });
    let mut room = RoomStore::open()?;
    let fact = room.append_fact(&fact)?;
    let snapshot = room.snapshot()?;
    let body = envelope(
        "say",
        SCHEMA_SAY,
        json!({
            "fact": fact,
            "room": snapshot.summary_json()
        }),
    );
    let text = format!(
        "said {} {}",
        body["data"]["fact"]["kind"].as_str().unwrap_or("fact"),
        body["data"]["fact"]["event_id"].as_str().unwrap_or("")
    );
    Ok(Output::new(json_output, text, body))
}

fn command_room(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let mut room = RoomStore::open()?;
    let query = RoomQuery::from_args(&args)?;
    let raw_snapshot = room.snapshot()?;
    let snapshot = raw_snapshot.clone().filtered(&query);
    let exported_handoff = if args.flag("--export-handoff") {
        Some(room.export_handoff(&raw_snapshot, query.tool.as_deref())?)
    } else {
        None
    };
    let body = envelope(
        "room",
        SCHEMA_ROOM,
        json!({
            "query": query.to_json(),
            "room": snapshot.to_json(),
            "exported_handoff": exported_handoff
        }),
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

fn command_next(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let tool = args.required("--tool")?;
    let role = args.one("--role");
    let paths: Vec<String> = args.all("--path").into_iter().map(normalize_path).collect();
    let limit = parse_i64_option(&args, "--limit")?
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let mut room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let next = build_next(&snapshot, &tool, role.as_deref(), &paths, limit);
    let body = envelope(
        "next",
        SCHEMA_NEXT,
        json!({
            "tool": tool,
            "role": role,
            "paths": paths,
            "next": next,
            "room": snapshot.summary_json()
        }),
    );
    let text = format!(
        "next action={} target={}",
        body["data"]["next"]["action"].as_str().unwrap_or("unknown"),
        body["data"]["next"]["target_event_id"]
            .as_str()
            .unwrap_or("none")
    );
    Ok(Output::new(json_output, text, body))
}

fn command_check(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let phase = args
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| "before-write".to_string());
    let tool = args.one("--tool").unwrap_or_else(|| "unknown".to_string());
    let path = args.one("--path").map(normalize_path);
    let strict = args.flag("--strict");
    let mut room = RoomStore::open()?;
    let snapshot = room.snapshot()?;
    let mut findings = Vec::new();
    match phase.as_str() {
        "before-write" => check_before_write(&snapshot, &tool, path.as_deref(), &mut findings),
        "after-artifact" => check_after_artifact(&args, &mut findings),
        "before-complete" => check_before_complete(&snapshot, &tool, &mut findings),
        other => findings.push(json!({
            "code": "unknown-phase",
            "severity": "warn",
            "message": format!("unknown check phase {other}"),
        })),
    }
    let stop = findings.iter().any(|f| f["severity"] == "stop");
    let allow = !stop || !strict;
    let exit_code = if strict && stop { 4 } else { 0 };
    let body = envelope(
        "check",
        SCHEMA_CHECK,
        json!({
            "check": {
                "phase": phase,
                "tool": tool,
                "path": path,
                "allow": allow,
                "mode": if strict { "strict" } else { "warn" },
                "findings": findings,
                "agent_visible": {
                    "present": stop,
                    "severity": if stop { "stop" } else { "info" },
                    "message": if stop {
                        "Rally check found room facts that should stop or redirect this write."
                    } else {
                        "Rally check passed."
                    }
                }
            }
        }),
    );
    let text = format!(
        "check {} allow={} findings={}",
        body["data"]["check"]["phase"].as_str().unwrap_or("check"),
        body["data"]["check"]["allow"].as_bool().unwrap_or(false),
        body["data"]["check"]["findings"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    Ok(Output::new(json_output, text, body).with_exit_code(exit_code))
}

fn command_install(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let dry_run = args.flag("--dry-run");
    let uninstall = args.flag("--uninstall");
    let target = args
        .positional
        .first()
        .cloned()
        .unwrap_or_else(|| "all".to_string());
    let home = args
        .one("--home")
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    let rally2_bin = args
        .one("--rally2-bin")
        .unwrap_or_else(|| "rally2".to_string());
    let adapters = install_targets(&target)?;
    let mut installed = Vec::new();

    for adapter in adapters {
        let mut plan = install_plan(adapter, &home, &rally2_bin)?;
        let actions = if uninstall {
            apply_uninstall_plan(&plan, dry_run)?
        } else {
            apply_install_plan(&plan, dry_run)?
        };
        plan.actions = actions;
        installed.push(plan.to_json());
    }

    let mode = if uninstall {
        if dry_run {
            "dry-run-uninstall"
        } else {
            "uninstall"
        }
    } else if dry_run {
        "dry-run"
    } else {
        "install"
    };
    let file_count = installed
        .iter()
        .flat_map(|adapter| adapter["actions"].as_array().into_iter().flatten())
        .filter(|action| action["action"] != "skip")
        .count();
    let body = envelope(
        "install",
        SCHEMA_INSTALL,
        json!({
            "target": normalize_install_target(&target),
            "mode": mode,
            "home": home.display().to_string(),
            "rally2_bin": rally2_bin,
            "adapters": installed
        }),
    );
    let text = format!(
        "install target={} mode={} changed={file_count}",
        body["data"]["target"].as_str().unwrap_or("all"),
        mode
    );
    Ok(Output::new(json_output, text, body))
}

fn command_run(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let dry_run = args.flag("--dry-run");
    let agent = args
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "run requires an agent name".to_string())?;
    let backend = normalize_backend(args.one("--backend").as_deref().unwrap_or("tmux"))?;
    let repo = repo_root()?;
    let agent_spec = AgentSpec::from_name(&agent)?;
    let name = args
        .one("--name")
        .unwrap_or_else(|| format!("{}-{}", agent_spec.agent, short_id()));
    let session_id = args
        .one("--session-id")
        .unwrap_or_else(|| format!("{}-{}", agent_spec.agent, sanitize_id(&name)));
    let tool = args
        .one("--tool")
        .unwrap_or_else(|| format!("{}:{}", agent_spec.tool, sanitize_id(&name)));
    let backend_target = backend_target(&backend, &session_id);
    let command = agent_spec.command_line(&name);
    let backend_runner = BackendRunner::new(&backend, &args)?;
    let start_commands = backend_runner.start_commands(&backend_target, &repo, &command, &name);

    let actual_target = if dry_run {
        backend_target.clone()
    } else {
        backend_runner.start(&backend_target, &repo, &command, &name)?
    };
    if !dry_run {
        write_session_record(&ManagedSession {
            session_id: session_id.clone(),
            name: name.clone(),
            agent: agent_spec.agent.to_string(),
            tool: tool.clone(),
            backend: backend.clone(),
            cwd: repo.clone(),
            target: actual_target.clone(),
        })?;
    }

    let body = envelope(
        "run",
        SCHEMA_RUN,
        json!({
            "mode": if dry_run { "dry-run" } else { "run" },
            "session": {
                "session_id": session_id,
                "name": name,
                "agent": agent_spec.agent,
                "tool": tool,
                "backend": backend,
                "cwd": repo.display().to_string(),
                "target": actual_target
            },
            "commands": {
                "start": command_plan_json(&start_commands)
            }
        }),
    );
    let text = format!(
        "run agent={} backend={} session={}",
        body["data"]["session"]["agent"]
            .as_str()
            .unwrap_or("unknown"),
        body["data"]["session"]["backend"]
            .as_str()
            .unwrap_or("unknown"),
        body["data"]["session"]["session_id"]
            .as_str()
            .unwrap_or("unknown")
    );
    Ok(Output::new(json_output, text, body))
}

fn command_sessions(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let sessions = read_session_records()?;
    let body = envelope(
        "sessions",
        SCHEMA_SESSIONS,
        json!({
            "sessions": sessions.iter().map(ManagedSession::to_json).collect::<Vec<_>>()
        }),
    );
    let text = format!("sessions {}", sessions.len());
    Ok(Output::new(json_output, text, body))
}

fn command_inject(args: ArgBag) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let dry_run = args.flag("--dry-run");
    let target = args
        .positional
        .first()
        .cloned()
        .ok_or_else(|| "inject requires a session id, name, or tool".to_string())?;
    let session = find_session(&target)?;
    let handoff = args.one("--handoff").or_else(|| args.one("--ref"));
    let text = match (args.one("--text"), handoff.as_deref()) {
        (Some(text), _) => text,
        (None, Some(handoff)) => handoff_prompt(&session, handoff),
        (None, None) => return Err("inject requires --text or --handoff".to_string()),
    };
    let require_ack = args.flag("--require-ack");
    if require_ack && handoff.is_none() {
        return Err("--require-ack requires --handoff or --ref".to_string());
    }
    let timeout = parse_i64_option(&args, "--timeout-seconds")?
        .unwrap_or(60)
        .clamp(1, 600) as u64;
    let backend_runner = BackendRunner::new(&session.backend, &args)?;
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let commands = backend_runner.inject_commands(&live_target, &text);
    if !dry_run {
        backend_runner.inject(&live_target, &text)?;
    }
    let ack = if require_ack && !dry_run {
        let handoff = handoff.as_deref().unwrap_or_default();
        Some(wait_for_resolution(handoff, timeout)?)
    } else {
        None
    };
    let body = envelope(
        "inject",
        SCHEMA_INJECT,
        json!({
            "mode": if dry_run { "dry-run" } else { "inject" },
            "session": session.to_json(),
            "handoff": handoff,
            "require_ack": require_ack,
            "ack": ack,
            "commands": command_plan_json(&commands)
        }),
    );
    let text = format!(
        "inject session={} ack={}",
        session.session_id,
        body["data"]["ack"].is_object()
    );
    Ok(Output::new(json_output, text, body))
}

#[derive(Clone, Copy)]
enum SessionAction {
    Attach,
    Capture,
    Stop,
}

fn command_session_action(args: ArgBag, action: SessionAction) -> Result<Output, String> {
    let json_output = args.flag("--json");
    let dry_run = args.flag("--dry-run");
    let target = args
        .positional
        .first()
        .cloned()
        .ok_or_else(|| format!("{} requires a session id, name, or tool", args.command))?;
    let session = find_session(&target)?;
    let backend_runner = BackendRunner::new(&session.backend, &args)?;
    let live_target = if dry_run {
        session.target.clone()
    } else {
        backend_runner.live_target(&session)?
    };
    let lines = parse_i64_option(&args, "--lines")?
        .unwrap_or(120)
        .clamp(1, 2000) as usize;
    let (commands, output) = match action {
        SessionAction::Attach => {
            let commands = backend_runner.attach_commands(&live_target);
            if !dry_run && !json_output {
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
    let command_name = args.command;
    let body = envelope(
        command_name,
        SCHEMA_SESSION_ACTION,
        json!({
            "mode": if dry_run { "dry-run" } else { command_name },
            "action": command_name,
            "session": session.to_json(),
            "output": output,
            "commands": command_plan_json(&commands)
        }),
    );
    let text =
        output_text.unwrap_or_else(|| format!("{command_name} session={}", session.session_id));
    Ok(Output::new(json_output, text, body))
}

#[derive(Clone, Debug)]
struct AgentSpec {
    agent: &'static str,
    tool: &'static str,
    command: &'static str,
}

impl AgentSpec {
    fn from_name(agent: &str) -> Result<Self, String> {
        match agent {
            "claude" | "claude_code" | "claude-code" => Ok(Self {
                agent: "claude",
                tool: "claude_code",
                command: "claude",
            }),
            "codex" => Ok(Self {
                agent: "codex",
                tool: "codex",
                command: "codex",
            }),
            "opencode" | "ocode" | "oc" => Ok(Self {
                agent: "opencode",
                tool: "opencode",
                command: "opencode",
            }),
            "gemini" => Ok(Self {
                agent: "gemini",
                tool: "gemini",
                command: "gemini",
            }),
            other => Err(format!("unsupported agent {other}")),
        }
    }

    fn command_line(&self, name: &str) -> Vec<String> {
        match self.agent {
            "claude" => cmd![self.command, "--name", name],
            _ => cmd![self.command],
        }
    }
}

#[derive(Clone, Debug)]
struct ManagedSession {
    session_id: String,
    name: String,
    agent: String,
    tool: String,
    backend: String,
    cwd: PathBuf,
    target: String,
}

impl ManagedSession {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            session_id: value.get("session_id")?.as_str()?.to_string(),
            name: value.get("name")?.as_str()?.to_string(),
            agent: value.get("agent")?.as_str()?.to_string(),
            tool: value.get("tool")?.as_str()?.to_string(),
            backend: value.get("backend")?.as_str()?.to_string(),
            cwd: PathBuf::from(value.get("cwd")?.as_str()?),
            target: value
                .get("target")
                .and_then(Value::as_str)
                .map(ToString::to_string)?,
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "name": self.name,
            "agent": self.agent,
            "tool": self.tool,
            "backend": self.backend,
            "cwd": self.cwd.display().to_string(),
            "target": self.target
        })
    }
}

struct BackendRunner {
    backend: String,
    tmux_bin: String,
    herdr_bin: String,
    cmux_bin: String,
}

impl BackendRunner {
    fn new(backend: &str, args: &ArgBag) -> Result<Self, String> {
        Ok(Self {
            backend: normalize_backend(backend)?,
            tmux_bin: args.one("--tmux-bin").unwrap_or_else(|| "tmux".to_string()),
            herdr_bin: args
                .one("--herdr-bin")
                .unwrap_or_else(|| "herdr".to_string()),
            cmux_bin: args.one("--cmux-bin").unwrap_or_else(|| "cmux".to_string()),
        })
    }

    fn start_commands(
        &self,
        target: &str,
        cwd: &Path,
        command: &[String],
        name: &str,
    ) -> Vec<Vec<String>> {
        match self.backend.as_str() {
            "tmux" => vec![tmux_start_command(&self.tmux_bin, target, cwd, command)],
            "herdr" => vec![herdr_start_command(&self.herdr_bin, target, cwd, command)],
            "cmux" => vec![cmux_start_command(
                &self.cmux_bin,
                target,
                cwd,
                command,
                name,
            )],
            _ => Vec::new(),
        }
    }

    fn start(
        &self,
        target: &str,
        cwd: &Path,
        command: &[String],
        name: &str,
    ) -> Result<String, String> {
        let commands = self.start_commands(target, cwd, command, name);
        match self.backend.as_str() {
            "tmux" | "herdr" => run_commands(&commands).map(|()| target.to_string()),
            "cmux" => {
                let output = run_command_output(first_command(&commands)?)?;
                Ok(parse_cmux_start_target(&output, target))
            }
            other => Err(format!("unsupported backend {other}")),
        }
    }

    fn live_target(&self, session: &ManagedSession) -> Result<String, String> {
        if self.backend == "herdr" {
            herdr_live_pane(&self.herdr_bin, session)
        } else {
            Ok(session.target.clone())
        }
    }

    fn inject_commands(&self, target: &str, text: &str) -> Vec<Vec<String>> {
        match self.backend.as_str() {
            "tmux" => tmux_inject_commands(&self.tmux_bin, target, text),
            "herdr" => vec![
                cmd![&self.herdr_bin, "pane", "send-text", target, "\u{15}"],
                cmd![&self.herdr_bin, "pane", "send-text", target, text],
                cmd![&self.herdr_bin, "pane", "send-keys", target, "enter"],
            ],
            "cmux" => vec![
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "ctrl+u"],
                cmd![&self.cmux_bin, "send", "--workspace", target, text],
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "enter"],
            ],
            _ => Vec::new(),
        }
    }

    fn inject(&self, target: &str, text: &str) -> Result<(), String> {
        match self.backend.as_str() {
            "tmux" | "herdr" | "cmux" => run_commands(&self.inject_commands(target, text)),
            other => Err(format!("unsupported backend {other}")),
        }
    }

    fn attach_commands(&self, target: &str) -> Vec<Vec<String>> {
        match self.backend.as_str() {
            "tmux" => vec![cmd![&self.tmux_bin, "attach", "-t", target]],
            "herdr" => vec![cmd![&self.herdr_bin, "agent", "attach", target]],
            "cmux" => vec![cmd![
                &self.cmux_bin,
                "select-workspace",
                "--workspace",
                target,
            ]],
            _ => Vec::new(),
        }
    }

    fn attach(&self, target: &str) -> Result<(), String> {
        match self.backend.as_str() {
            "tmux" | "herdr" | "cmux" => run_commands(&self.attach_commands(target)),
            other => Err(format!("unsupported backend {other}")),
        }
    }

    fn capture_commands(&self, target: &str, lines: usize) -> Vec<Vec<String>> {
        match self.backend.as_str() {
            "tmux" => vec![cmd![
                &self.tmux_bin,
                "capture-pane",
                "-pt",
                target,
                "-S",
                format!("-{lines}"),
            ]],
            "herdr" => vec![cmd![
                &self.herdr_bin,
                "agent",
                "read",
                target,
                "--source",
                "recent-unwrapped",
                "--lines",
                lines,
            ]],
            "cmux" => vec![cmd![
                &self.cmux_bin,
                "read-screen",
                "--workspace",
                target,
                "--scrollback",
                "--lines",
                lines,
            ]],
            _ => Vec::new(),
        }
    }

    fn capture(&self, target: &str, lines: usize) -> Result<String, String> {
        match self.backend.as_str() {
            "tmux" | "herdr" | "cmux" => {
                run_command_output(first_command(&self.capture_commands(target, lines))?)
            }
            other => Err(format!("unsupported backend {other}")),
        }
    }

    fn stop_commands(&self, target: &str) -> Vec<Vec<String>> {
        match self.backend.as_str() {
            "tmux" => vec![cmd![&self.tmux_bin, "kill-session", "-t", target]],
            "herdr" => vec![cmd![&self.herdr_bin, "pane", "close", target]],
            "cmux" => vec![cmd![
                &self.cmux_bin,
                "close-workspace",
                "--workspace",
                target
            ]],
            _ => Vec::new(),
        }
    }

    fn stop(&self, target: &str) -> Result<(), String> {
        match self.backend.as_str() {
            "tmux" | "herdr" | "cmux" => run_commands(&self.stop_commands(target)),
            other => Err(format!("unsupported backend {other}")),
        }
    }
}

fn tmux_start_command(bin: &str, session: &str, cwd: &Path, command: &[String]) -> Vec<String> {
    let shell_command = format!(
        "cd {} && exec {}",
        shell_quote(&cwd.display().to_string()),
        shell_words(command)
    );
    cmd![
        bin,
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        "140",
        "-y",
        "50",
        shell_command,
    ]
}

fn tmux_inject_commands(bin: &str, session: &str, text: &str) -> Vec<Vec<String>> {
    let buffer = format!("rally-inject-{session}");
    vec![
        cmd![bin, "send-keys", "-t", session, "C-u"],
        cmd![bin, "set-buffer", "-b", &buffer, text],
        cmd![bin, "paste-buffer", "-b", buffer, "-t", session],
        cmd![bin, "send-keys", "-t", session, "Enter"],
    ]
}

fn herdr_start_command(bin: &str, target: &str, cwd: &Path, command: &[String]) -> Vec<String> {
    let mut args = cmd![
        bin,
        "agent",
        "start",
        target,
        "--cwd",
        cwd.display(),
        "--no-focus",
        "--",
    ];
    args.extend(command.iter().cloned());
    args
}

fn cmux_start_command(
    bin: &str,
    target: &str,
    cwd: &Path,
    command: &[String],
    name: &str,
) -> Vec<String> {
    let layout = json!({
        "pane": {
            "surfaces": [
                {
                    "type": "terminal",
                    "command": shell_words(command)
                }
            ]
        }
    })
    .to_string();
    cmd![
        bin,
        "new-workspace",
        "--name",
        name,
        "--description",
        target,
        "--cwd",
        cwd.display(),
        "--layout",
        layout,
        "--focus",
        "false",
    ]
}

fn parse_cmux_start_target(output: &str, fallback: &str) -> String {
    output
        .split_whitespace()
        .find_map(|word| {
            let value = word.trim_matches(|ch: char| {
                ch == '"' || ch == '\'' || ch == ',' || ch == ';' || ch == '.'
            });
            if value.starts_with("workspace:") {
                Some(value.to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            output
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_cmux_start_target;

    #[test]
    fn cmux_start_target_uses_workspace_ref_from_status_output() {
        assert_eq!(
            parse_cmux_start_target("OK workspace:11\n", "claude-cmux-smoke"),
            "workspace:11"
        );
    }
}

fn herdr_live_pane(bin: &str, session: &ManagedSession) -> Result<String, String> {
    let command = cmd![bin, "agent", "list"];
    let output = run_command_output(&command)?;
    if output.trim().is_empty() {
        return Ok(session.target.clone());
    }
    let value: Value =
        serde_json::from_str(&output).map_err(|err| format!("parse herdr agent list: {err}"))?;
    let agents = value
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .ok_or_else(|| "herdr agent list did not return agents".to_string())?;
    for agent in agents {
        let matches = ["name", "pane_id", "terminal_id", "label"]
            .iter()
            .filter_map(|key| agent.get(*key).and_then(Value::as_str))
            .any(|value| {
                value == session.target
                    || value == session.session_id
                    || value == session.name
                    || value == session.tool
            });
        if matches {
            if let Some(pane_id) = agent.get("pane_id").and_then(Value::as_str) {
                return Ok(pane_id.to_string());
            }
        }
    }
    Err(format!(
        "herdr managed session {} is not currently live",
        session.session_id
    ))
}

fn command_plan_json(commands: &[Vec<String>]) -> Vec<Value> {
    commands.iter().map(|command| json!(command)).collect()
}

fn first_command(commands: &[Vec<String>]) -> Result<&[String], String> {
    commands
        .first()
        .map(Vec::as_slice)
        .ok_or_else(|| "empty command plan".to_string())
}

fn run_commands(commands: &[Vec<String>]) -> Result<(), String> {
    for command in commands {
        run_command_owned(command)?;
    }
    Ok(())
}

fn run_command_owned(args: &[String]) -> Result<(), String> {
    let (bin, rest) = args
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let status = Command::new(bin)
        .args(rest)
        .status()
        .map_err(|err| format!("run {bin}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{bin} exited with {status}"))
    }
}

fn run_command_output(args: &[String]) -> Result<String, String> {
    let (bin, rest) = args
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let output = Command::new(bin)
        .args(rest)
        .output()
        .map_err(|err| format!("run {bin}: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!(
            "{bin} exited with {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn shell_words(words: &[String]) -> String {
    shlex::try_join(words.iter().map(String::as_str)).expect("agent command contains NUL byte")
}

fn sessions_path() -> Result<PathBuf, String> {
    let root = repo_root()?;
    let dir = root.join(".rally2");
    fs::create_dir_all(&dir).map_err(|err| format!("create .rally2: {err}"))?;
    Ok(dir.join("sessions.json"))
}

fn read_session_records() -> Result<Vec<ManagedSession>, String> {
    let path = sessions_path()?;
    let text = fs::read_to_string(path).unwrap_or_else(|_| "[]".to_string());
    let value: Value =
        serde_json::from_str(&text).map_err(|err| format!("parse sessions.json: {err}"))?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(ManagedSession::from_value)
        .collect())
}

fn write_session_record(session: &ManagedSession) -> Result<(), String> {
    let mut sessions = read_session_records()?;
    sessions.retain(|existing| existing.session_id != session.session_id);
    sessions.push(session.clone());
    write_session_records(&sessions)
}

fn remove_session_record(session_id: &str) -> Result<(), String> {
    let mut sessions = read_session_records()?;
    sessions.retain(|existing| existing.session_id != session_id);
    write_session_records(&sessions)
}

fn write_session_records(sessions: &[ManagedSession]) -> Result<(), String> {
    let path = sessions_path()?;
    let value = json!(
        sessions
            .iter()
            .map(ManagedSession::to_json)
            .collect::<Vec<_>>()
    );
    fs::write(
        &path,
        serde_json::to_string_pretty(&value).unwrap_or_default(),
    )
    .map_err(|err| format!("write {}: {err}", path.display()))
}

fn find_session(target: &str) -> Result<ManagedSession, String> {
    read_session_records()?
        .into_iter()
        .find(|session| {
            session.session_id == target || session.name == target || session.tool == target
        })
        .ok_or_else(|| format!("unknown managed session {target}"))
}

fn backend_target(backend: &str, session_id: &str) -> String {
    match backend {
        "tmux" => format!("rally-{}", sanitize_id(session_id)),
        "herdr" => sanitize_id(session_id),
        "cmux" => sanitize_id(session_id),
        _ => sanitize_id(session_id),
    }
}

fn handoff_prompt(session: &ManagedSession, handoff: &str) -> String {
    format!(
        "Rally managed-session injection for {}. Run: rally2 next --tool {} --json. If it is actionable for handoff {}, execute the suggested Rally completion command or run: rally2 say resolve --tool {} --ref {} --subject 'resolved via Rally managed session' --json. Do not edit files unless the Rally action explicitly requires it. Do not ask for confirmation after the Rally command succeeds.",
        session.name, session.tool, handoff, session.tool, handoff
    )
}

fn wait_for_resolution(handoff: &str, timeout_seconds: u64) -> Result<Value, String> {
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
    Err(format!(
        "timed out after {timeout_seconds}s waiting for resolve fact for {handoff}"
    ))
}

fn normalize_backend(backend: &str) -> Result<String, String> {
    match backend {
        "auto" | "tmux" => Ok("tmux".to_string()),
        "herdr" => Ok("herdr".to_string()),
        "cmux" => Ok("cmux".to_string()),
        other => Err(format!("unsupported backend {other}")),
    }
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

fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}", nanos & 0xfffff)
}

#[derive(Clone, Debug)]
struct InstallPlan {
    adapter: &'static str,
    files: Vec<InstallFile>,
    hook_configs: Vec<HookConfig>,
    actions: Vec<Value>,
}

impl InstallPlan {
    fn to_json(&self) -> Value {
        json!({
            "adapter": self.adapter,
            "actions": self.actions
        })
    }
}

#[derive(Clone, Debug)]
struct InstallFile {
    path: PathBuf,
    content: String,
    executable: bool,
    kind: &'static str,
}

#[derive(Clone, Debug)]
struct HookConfig {
    path: PathBuf,
    entries: Vec<HookEntry>,
}

#[derive(Clone, Debug)]
struct HookEntry {
    event: &'static str,
    matcher: &'static str,
    command: String,
}

fn install_targets(target: &str) -> Result<Vec<&'static str>, String> {
    match normalize_install_target(target).as_str() {
        "all" => Ok(vec!["codex", "claude_code", "pi", "herdr", "cmux", "ci"]),
        "codex" => Ok(vec!["codex"]),
        "claude_code" => Ok(vec!["claude_code"]),
        "pi" => Ok(vec!["pi"]),
        "herdr" => Ok(vec!["herdr"]),
        "cmux" => Ok(vec!["cmux"]),
        "ci" => Ok(vec!["ci"]),
        other => Err(format!("unsupported install target {other}")),
    }
}

fn normalize_install_target(target: &str) -> String {
    match target {
        "claude" | "claude-code" | "claude_code" => "claude_code".to_string(),
        other => other.to_string(),
    }
}

fn install_plan(
    adapter: &'static str,
    home: &Path,
    rally2_bin: &str,
) -> Result<InstallPlan, String> {
    let mut files = Vec::new();
    let mut hook_configs = Vec::new();

    match adapter {
        "codex" => {
            let hook = home.join(".codex/hooks/rally2-hook.sh");
            files.push(InstallFile {
                path: hook.clone(),
                content: guard_hook("codex", rally2_bin),
                executable: true,
                kind: "hook-script",
            });
            hook_configs.push(HookConfig {
                path: home.join(".codex/hooks.json"),
                entries: codex_hook_entries(&hook),
            });
        }
        "claude_code" => {
            let hook = home.join(".claude/hooks/rally2-hook.sh");
            files.push(InstallFile {
                path: hook.clone(),
                content: guard_hook("claude_code", rally2_bin),
                executable: true,
                kind: "hook-script",
            });
            hook_configs.push(HookConfig {
                path: home.join(".claude/settings.json"),
                entries: claude_hook_entries(&hook),
            });
        }
        "pi" => {
            let extension = home.join(".pi/agent/extensions/rally2-guard.ts");
            files.push(InstallFile {
                path: extension,
                content: pi_guard_extension(rally2_bin),
                executable: false,
                kind: "pi-extension",
            });
        }
        "herdr" => {
            files.push(InstallFile {
                path: home.join(".config/herdr/integrations/rally2.json"),
                content: herdr_integration(rally2_bin),
                executable: false,
                kind: "herdr-integration",
            });
        }
        "cmux" => {
            files.push(InstallFile {
                path: home.join(".config/cmux/rally2-integration.json"),
                content: cmux_integration(rally2_bin),
                executable: false,
                kind: "cmux-integration",
            });
        }
        "ci" => {
            files.push(InstallFile {
                path: home.join(".config/rally2/ci/github-actions-rally2.yml"),
                content: ci_workflow(rally2_bin),
                executable: false,
                kind: "ci-workflow",
            });
        }
        _ => return Err(format!("unsupported install adapter {adapter}")),
    }

    Ok(InstallPlan {
        adapter,
        files,
        hook_configs,
        actions: Vec::new(),
    })
}

fn apply_install_plan(plan: &InstallPlan, dry_run: bool) -> Result<Vec<Value>, String> {
    let mut actions = Vec::new();
    for file in &plan.files {
        let changed =
            dry_run || fs::read_to_string(&file.path).ok().as_deref() != Some(&file.content);
        if !dry_run {
            write_owned_file(file)?;
        }
        actions.push(json!({
            "path": file.path.display().to_string(),
            "kind": file.kind,
            "action": if changed { "write" } else { "skip" },
            "executable": file.executable,
            "owned": true
        }));
    }
    for config in &plan.hook_configs {
        let content = merge_hook_config(config, true)?;
        let changed = dry_run || fs::read_to_string(&config.path).ok().as_deref() != Some(&content);
        if !dry_run {
            write_text_file(&config.path, &content, false)?;
        }
        actions.push(json!({
            "path": config.path.display().to_string(),
            "kind": "hook-config",
            "action": if changed { "merge" } else { "skip" },
            "executable": false,
            "owned": false
        }));
    }
    Ok(actions)
}

fn apply_uninstall_plan(plan: &InstallPlan, dry_run: bool) -> Result<Vec<Value>, String> {
    let mut actions = Vec::new();
    for file in &plan.files {
        let owned = marker_owned(&file.path);
        let action = if owned { "remove" } else { "skip" };
        if owned && !dry_run {
            fs::remove_file(&file.path)
                .map_err(|err| format!("remove {}: {err}", file.path.display()))?;
        }
        actions.push(json!({
            "path": file.path.display().to_string(),
            "kind": file.kind,
            "action": action,
            "executable": file.executable,
            "owned": owned
        }));
    }
    for config in &plan.hook_configs {
        let before = fs::read_to_string(&config.path).unwrap_or_default();
        let content = merge_hook_config(config, false)?;
        let changed = before != content;
        if changed && !dry_run {
            write_text_file(&config.path, &content, false)?;
        }
        actions.push(json!({
            "path": config.path.display().to_string(),
            "kind": "hook-config",
            "action": if changed { "unmerge" } else { "skip" },
            "executable": false,
            "owned": false
        }));
    }
    Ok(actions)
}

fn write_owned_file(file: &InstallFile) -> Result<(), String> {
    write_text_file(&file.path, &file.content, file.executable)
}

fn write_text_file(path: &Path, content: &str, executable: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(path, content).map_err(|err| format!("write {}: {err}", path.display()))?;
    set_executable(path, executable)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if executable {
        let mut perms = fs::metadata(path)
            .map_err(|err| format!("stat {}: {err}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .map_err(|err| format!("chmod {}: {err}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

fn marker_owned(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|content| content.contains(INSTALL_MARKER))
        .unwrap_or(false)
}

fn merge_hook_config(config: &HookConfig, install: bool) -> Result<String, String> {
    let mut value = fs::read_to_string(&config.path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .unwrap_or_else(|| json!({}));
    if !value.is_object() {
        value = json!({});
    }
    let root = value.as_object_mut().unwrap();
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    for entry in &config.entries {
        let event_hooks = hooks.entry(entry.event).or_insert_with(|| json!([]));
        if !event_hooks.is_array() {
            *event_hooks = json!([]);
        }
        let event_hooks = event_hooks.as_array_mut().unwrap();
        remove_rally2_hook_entries(event_hooks);
        if install {
            event_hooks.push(json!({
                "matcher": entry.matcher,
                "hooks": [{
                    "type": "command",
                    "command": entry.command
                }]
            }));
        }
    }
    serde_json::to_string_pretty(&value).map_err(|err| format!("render hook config: {err}"))
}

fn remove_rally2_hook_entries(event_hooks: &mut Vec<Value>) {
    for entry in event_hooks.iter_mut() {
        if let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(INSTALL_MARKER))
            });
        }
    }
    event_hooks.retain(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
}

fn codex_hook_entries(hook: &Path) -> Vec<HookEntry> {
    let hook = shell_quote(&hook.display().to_string());
    vec![hook_entry(
        "PreToolUse",
        "Write|Edit|MultiEdit|NotebookEdit",
        &hook,
        "before-write",
        "codex",
    )]
}

fn claude_hook_entries(hook: &Path) -> Vec<HookEntry> {
    let hook = shell_quote(&hook.display().to_string());
    vec![hook_entry(
        "PreToolUse",
        "Write|Edit|MultiEdit|NotebookEdit",
        &hook,
        "before-write",
        "claude_code",
    )]
}

fn hook_entry(
    event: &'static str,
    matcher: &'static str,
    hook: &str,
    phase: &str,
    adapter: &str,
) -> HookEntry {
    HookEntry {
        event,
        matcher,
        command: format!("RALLY2_INSTALL_MARKER={INSTALL_MARKER} /bin/sh {hook} {phase} {adapter}"),
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn shell_quote(value: &str) -> String {
    shlex::try_quote(value)
        .expect("shell argument contains NUL byte")
        .into_owned()
}

fn guard_hook(adapter: &str, rally2_bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# {marker}
# Installed by `rally2 install {adapter}`. DO NOT EDIT MANUALLY.
set -u

phase="${{1:-before-write}}"
tool="${{2:-{adapter}}}"
RALLY2_BIN="${{RALLY2_BIN:-{rally2_bin}}}"
payload="$(cat 2>/dev/null || true)"

json_field() {{
  key="$1"
  PAYLOAD="$payload" KEY="$key" node -e '
const data = (() => {{ try {{ return JSON.parse(process.env.PAYLOAD || "{{}}"); }} catch (_) {{ return {{}}; }} }})();
const key = process.env.KEY;
for (const container of [data, data.tool_input || {{}}, data.toolInput || {{}}, data.input || {{}}]) {{
  if (container && container[key]) {{
    process.stdout.write(String(container[key]));
    break;
  }}
}}
' 2>/dev/null || true
}}

path="$(json_field path)"
[ -n "$path" ] || path="$(json_field file_path)"
[ -n "$path" ] || path="$(json_field filePath)"
[ -n "$path" ] || path="$(json_field notebook_path)"
session_id="$(json_field session_id)"
[ -n "$session_id" ] || session_id="$(json_field sessionId)"
[ -n "$session_id" ] || session_id="session-$tool"

run_check() {{
  if [ -n "$path" ]; then
    "$RALLY2_BIN" check before-write --tool "$tool" --path "$path" --strict --json 2>/dev/null || true
  else
    "$RALLY2_BIN" check before-write --tool "$tool" --strict --json 2>/dev/null || true
  fi
}}

json_escape() {{
  node -e 'let input = ""; process.stdin.on("data", chunk => input += chunk); process.stdin.on("end", () => process.stdout.write(JSON.stringify(input)));'
}}

case "$phase" in
  before-write)
    output="$(run_check)"
    if printf '%s' "$output" | grep -q '"allow": false'; then
      reason="$(printf 'Rally 2 blocked this write:\n%s' "$output" | json_escape)"
      printf '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}}}\n' "$reason"
    else
      printf '{{}}\n'
    fi
    ;;
  *)
    printf '{{}}\n'
    ;;
esac
"#,
        marker = INSTALL_MARKER,
        adapter = adapter,
        rally2_bin = rally2_bin.replace('"', "\\\"")
    )
}

fn pi_guard_extension(rally2_bin: &str) -> String {
    format!(
        r#"// {marker}
// Installed by `rally2 install pi`. DO NOT EDIT MANUALLY.
import {{ spawnSync }} from "node:child_process";
import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";

const RALLY2_BIN = process.env.RALLY2_BIN || {bin};

function runRally2(args: string[], input?: unknown): string {{
  const result = spawnSync(RALLY2_BIN, args, {{
    input: input === undefined ? undefined : JSON.stringify(input),
    encoding: "utf8",
    stdio: [input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
    timeout: 10000,
  }});
  return result.stdout || result.stderr || "";
}}

function pathFromTool(event: any): string | undefined {{
  const input = event?.input || {{}};
  return input.path || input.file_path || input.filePath || input.notebook_path;
}}

export default function rally2Guard(pi: ExtensionAPI) {{
  pi.on("tool_call", async (event) => {{
    const name = event.toolName;
    if (!["write", "edit", "serena_replace_content", "serena_replace_symbol_body"].includes(name)) return;
    const path = pathFromTool(event);
    const args = ["check", "before-write", "--tool", "pi", "--json", "--strict"];
    if (path) args.push("--path", path);
    const output = runRally2(args, event);
    try {{
      const parsed = JSON.parse(output || "{{}}");
      if (parsed?.data?.check?.allow === false) {{
        return {{ block: true, reason: `Rally 2 blocked write:\n${{output}}` }};
      }}
    }} catch (_) {{}}
  }});
}}
"#,
        marker = INSTALL_MARKER,
        bin = serde_json::to_string(rally2_bin).unwrap_or_else(|_| "\"rally2\"".to_string())
    )
}

fn herdr_integration(rally2_bin: &str) -> String {
    json!({
        "marker": INSTALL_MARKER,
        "adapter": "herdr",
        "remote_safe": true,
        "commands": {
            "enter": format!("{rally2_bin} enter --tool herdr --json"),
            "room": format!("{rally2_bin} room --json"),
            "before_write": format!("{rally2_bin} check before-write --tool herdr --path <path> --strict --json"),
            "before_complete": format!("{rally2_bin} check before-complete --tool herdr --strict --json")
        },
        "notes": [
            "Use rally2 run --backend herdr to start managed agent panes.",
            "Use rally2 inject to route actionable work into managed Herdr panes.",
            "Remote Herdr sessions can run the same commands on the remote checkout because Rally 2 state is repo-local."
        ]
    })
    .to_string()
}

fn cmux_integration(rally2_bin: &str) -> String {
    json!({
        "marker": INSTALL_MARKER,
        "adapter": "cmux",
        "commands": {
            "enter": format!("{rally2_bin} enter --tool cmux --json"),
            "room": format!("{rally2_bin} room --json"),
            "before_write": format!("{rally2_bin} check before-write --tool cmux --path <path> --strict --json")
        },
        "notes": [
            "Use rally2 run --backend cmux to start managed workspaces.",
            "Use rally2 inject to route actionable work into managed cmux workspaces."
        ]
    })
    .to_string()
}

fn ci_workflow(rally2_bin: &str) -> String {
    format!(
        r#"# {marker}
# Copy into .github/workflows/rally2.yml when this repository has rally2 on PATH.
name: Rally 2

on:
  pull_request:
  push:

jobs:
  rally2:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Check Rally 2 room before completion
        run: {rally2_bin} check before-complete --tool ci --strict --json
"#,
        marker = INSTALL_MARKER,
        rally2_bin = rally2_bin
    )
}

#[derive(Clone, Debug)]
struct Fact {
    event_id: String,
    seq: i64,
    thread_id: String,
    kind: String,
    tool: Option<String>,
    role: Option<String>,
    subject: String,
    scope: Vec<String>,
    created_at: String,
    summary: Option<String>,
    evidence: Vec<String>,
    target: Option<String>,
    ref_id: Option<String>,
    status: Option<String>,
    severity: Option<String>,
    uri: Option<String>,
    origin: String,
    trust_status: String,
}

impl Fact {
    fn from_value(value: &Value, seq: i64) -> Self {
        Self {
            event_id: value["event_id"].as_str().unwrap_or("").to_string(),
            seq: value["seq"].as_i64().unwrap_or(seq),
            thread_id: value["thread_id"].as_str().unwrap_or("").to_string(),
            kind: value["kind"].as_str().unwrap_or("unknown").to_string(),
            tool: value["tool"].as_str().map(str::to_string),
            role: value["role"].as_str().map(str::to_string),
            subject: value["subject"].as_str().unwrap_or("").to_string(),
            scope: value["scope"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            created_at: value["created_at"].as_str().unwrap_or("").to_string(),
            summary: value["summary"].as_str().map(str::to_string),
            evidence: value["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            target: value["target"].as_str().map(str::to_string),
            ref_id: value["ref"].as_str().map(str::to_string),
            status: value["status"].as_str().map(str::to_string),
            severity: value["severity"].as_str().map(str::to_string),
            uri: value["uri"].as_str().map(str::to_string),
            origin: value["origin"].as_str().unwrap_or("local").to_string(),
            trust_status: value["trust_status"]
                .as_str()
                .unwrap_or("local")
                .to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "event_id": self.event_id,
            "seq": self.seq,
            "thread_id": self.thread_id,
            "kind": self.kind,
            "tool": self.tool,
            "role": self.role,
            "subject": self.subject,
            "scope": self.scope,
            "created_at": self.created_at,
            "summary": self.summary,
            "evidence": self.evidence,
            "target": self.target,
            "ref": self.ref_id,
            "status": self.status,
            "severity": self.severity,
            "uri": self.uri,
            "origin": self.origin,
            "trust_status": self.trust_status
        })
    }
}

#[derive(Clone, Debug, Default)]
struct RoomSnapshot {
    max_seq: i64,
    active_claims: Vec<Fact>,
    active_blockers: Vec<Fact>,
    open_handoffs: Vec<Fact>,
    current_decisions: Vec<Fact>,
    current_risks: Vec<Fact>,
    recent_artifacts: Vec<Fact>,
    unconsumed_artifacts: Vec<Fact>,
    stale_facts: Vec<Fact>,
    trust_summary: BTreeMap<String, usize>,
    adapters: Vec<Value>,
}

impl RoomSnapshot {
    fn to_json(&self) -> Value {
        json!({
            "max_seq": self.max_seq,
            "active_claims": facts_json(&self.active_claims),
            "active_blockers": facts_json(&self.active_blockers),
            "open_handoffs": facts_json(&self.open_handoffs),
            "current_decisions": facts_json(&self.current_decisions),
            "current_risks": facts_json(&self.current_risks),
            "recent_artifacts": facts_json(&self.recent_artifacts),
            "unconsumed_artifacts": facts_json(&self.unconsumed_artifacts),
            "stale_facts": facts_json(&self.stale_facts),
            "trust_summary": self.trust_summary,
            "adapters": self.adapters
        })
    }

    fn summary_json(&self) -> Value {
        json!({
            "max_seq": self.max_seq,
            "active_claims": self.active_claims.len(),
            "active_blockers": self.active_blockers.len(),
            "open_handoffs": self.open_handoffs.len(),
            "current_decisions": self.current_decisions.len(),
            "current_risks": self.current_risks.len(),
            "recent_artifacts": self.recent_artifacts.len(),
            "unconsumed_artifacts": self.unconsumed_artifacts.len(),
            "stale_facts": self.stale_facts.len(),
            "trust_summary": self.trust_summary
        })
    }

    fn filtered(self, query: &RoomQuery) -> Self {
        if query.is_empty() {
            return self;
        }
        Self {
            max_seq: self.max_seq,
            active_claims: filter_facts(self.active_claims, query),
            active_blockers: filter_facts(self.active_blockers, query),
            open_handoffs: filter_facts(self.open_handoffs, query),
            current_decisions: filter_facts(self.current_decisions, query),
            current_risks: filter_facts(self.current_risks, query),
            recent_artifacts: filter_facts(self.recent_artifacts, query),
            unconsumed_artifacts: filter_facts(self.unconsumed_artifacts, query),
            stale_facts: filter_facts(self.stale_facts, query),
            trust_summary: self.trust_summary,
            adapters: self.adapters,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RoomQuery {
    tool: Option<String>,
    role: Option<String>,
    paths: Vec<String>,
    event_id: Option<String>,
    thread_id: Option<String>,
    since: Option<i64>,
}

impl RoomQuery {
    fn from_args(args: &ArgBag) -> Result<Self, String> {
        Ok(Self {
            tool: args.one("--tool"),
            role: args.one("--role"),
            paths: args.all("--path").into_iter().map(normalize_path).collect(),
            event_id: args.one("--event"),
            thread_id: args.one("--thread"),
            since: parse_i64_option(args, "--since")?,
        })
    }

    fn is_empty(&self) -> bool {
        self.tool.is_none()
            && self.role.is_none()
            && self.paths.is_empty()
            && self.event_id.is_none()
            && self.thread_id.is_none()
            && self.since.is_none()
    }

    fn matches(&self, fact: &Fact) -> bool {
        if let Some(tool) = &self.tool {
            let tool_matches = fact.tool.as_deref() == Some(tool.as_str())
                || fact.target.as_deref() == Some(tool.as_str());
            if !tool_matches {
                return false;
            }
        }
        if let Some(role) = &self.role {
            if fact.role.as_deref() != Some(role.as_str()) {
                return false;
            }
        }
        if !self.paths.is_empty()
            && !self
                .paths
                .iter()
                .any(|path| fact.scope.iter().any(|scope| scope == path))
        {
            return false;
        }
        if let Some(event_id) = &self.event_id {
            let related = fact.event_id == *event_id || fact.ref_id.as_deref() == Some(event_id);
            if !related {
                return false;
            }
        }
        if let Some(thread_id) = &self.thread_id {
            if fact.thread_id != *thread_id {
                return false;
            }
        }
        if let Some(since) = self.since {
            if fact.seq <= since {
                return false;
            }
        }
        true
    }

    fn to_json(&self) -> Value {
        json!({
            "tool": self.tool,
            "role": self.role,
            "paths": self.paths,
            "event": self.event_id,
            "thread": self.thread_id,
            "since": self.since
        })
    }
}

struct RoomStore {
    root: PathBuf,
    fact_store: SqliteStore,
    db_path: PathBuf,
}

impl RoomStore {
    fn open() -> Result<Self, String> {
        let root = repo_root()?;
        let dir = root.join(".rally2");
        fs::create_dir_all(&dir).map_err(|err| format!("create .rally2: {err}"))?;
        let fact_store_path = dir.join("facts.db");
        let fact_store =
            SqliteStore::open(&fact_store_path).map_err(|err| format!("open fact store: {err}"))?;
        Ok(Self {
            root,
            fact_store,
            db_path: dir.join("room.db"),
        })
    }

    fn append_fact(&mut self, fact: &Value) -> Result<Value, String> {
        let mut fact = fact.clone();
        let event_type = fact["kind"].as_str().unwrap_or("fact").to_string();
        let result = self
            .fact_store
            .append(vec![NewEvent::new(event_type, fact.clone())])
            .map_err(|err| format!("append fact: {err}"))?;
        if let Some(object) = fact.as_object_mut() {
            object.insert("seq".to_string(), json!(result.last_sequence_number));
        }
        self.rebuild_projection()?;
        Ok(fact)
    }

    fn facts(&self) -> Result<Vec<Fact>, String> {
        let query = self
            .fact_store
            .query(&FactQuery::all())
            .map_err(|err| format!("query facts: {err}"))?;
        Ok(query
            .event_records
            .into_iter()
            .map(|record| Fact::from_value(&record.payload, record.sequence_number as i64))
            .collect())
    }

    fn connection(&self) -> Result<Connection, String> {
        let conn = Connection::open(&self.db_path).map_err(|err| format!("open room db: {err}"))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            "#,
        )
        .map_err(|err| format!("init room meta: {err}"))?;
        let version = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| format!("read schema version: {err}"))?;
        if version.as_deref() != Some(&DB_SCHEMA_VERSION.to_string()) {
            conn.execute_batch(
                r#"
                DROP TABLE IF EXISTS facts;
                DROP TABLE IF EXISTS scopes;
                DROP TABLE IF EXISTS edges;
                "#,
            )
            .map_err(|err| format!("reset stale room projection: {err}"))?;
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS facts(
                seq INTEGER PRIMARY KEY,
                event_id TEXT NOT NULL UNIQUE,
                thread_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                tool TEXT,
                role TEXT,
                subject TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                summary TEXT,
                evidence_json TEXT NOT NULL,
                target TEXT,
                ref_id TEXT,
                status TEXT,
                severity TEXT,
                uri TEXT,
                origin TEXT NOT NULL,
                trust_status TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_facts_kind ON facts(kind);
            CREATE INDEX IF NOT EXISTS idx_facts_tool ON facts(tool);
            CREATE INDEX IF NOT EXISTS idx_facts_role ON facts(role);
            CREATE INDEX IF NOT EXISTS idx_facts_thread ON facts(thread_id);
            CREATE INDEX IF NOT EXISTS idx_facts_target ON facts(target);
            CREATE INDEX IF NOT EXISTS idx_facts_ref ON facts(ref_id);
            CREATE TABLE IF NOT EXISTS scopes(
                event_id TEXT NOT NULL,
                scope TEXT NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY(event_id, scope)
            );
            CREATE INDEX IF NOT EXISTS idx_scopes_scope ON scopes(scope);
            CREATE TABLE IF NOT EXISTS edges(
                from_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                to_id TEXT NOT NULL,
                PRIMARY KEY(from_id, relation, to_id)
            );
            CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_id);
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_id);
            CREATE INDEX IF NOT EXISTS idx_edges_relation ON edges(relation);
            CREATE TABLE IF NOT EXISTS cursors(
                tool TEXT PRIMARY KEY,
                last_seq INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|err| format!("apply room schema: {err}"))?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![DB_SCHEMA_VERSION.to_string()],
        )
        .map_err(|err| format!("write schema version: {err}"))?;
        Ok(conn)
    }

    fn rebuild_projection(&self) -> Result<(), String> {
        let facts = self.facts()?;
        let mut conn = self.connection()?;
        let tx = conn
            .transaction()
            .map_err(|err| format!("start room projection tx: {err}"))?;
        tx.execute("DELETE FROM scopes", [])
            .map_err(|err| format!("clear scopes: {err}"))?;
        tx.execute("DELETE FROM edges", [])
            .map_err(|err| format!("clear edges: {err}"))?;
        tx.execute("DELETE FROM facts", [])
            .map_err(|err| format!("clear facts: {err}"))?;
        for fact in facts {
            tx.execute(
                "INSERT INTO facts(seq,event_id,thread_id,kind,tool,role,subject,scope_json,created_at,summary,evidence_json,target,ref_id,status,severity,uri,origin,trust_status)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                params![
                    fact.seq,
                    fact.event_id,
                    fact.thread_id,
                    fact.kind,
                    fact.tool,
                    fact.role,
                    fact.subject,
                    json!(fact.scope).to_string(),
                    fact.created_at,
                    fact.summary,
                    json!(fact.evidence).to_string(),
                    fact.target,
                    fact.ref_id,
                    fact.status,
                    fact.severity,
                    fact.uri,
                    fact.origin,
                    fact.trust_status,
                ],
            )
            .map_err(|err| format!("insert fact: {err}"))?;
            for scope in fact.scope {
                tx.execute(
                    "INSERT OR REPLACE INTO scopes(event_id, scope, kind) VALUES(?1,?2,?3)",
                    params![fact.event_id, scope, fact.kind],
                )
                .map_err(|err| format!("insert scope: {err}"))?;
                tx.execute(
                    "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'scoped_to',?2)",
                    params![fact.event_id, scope],
                )
                .map_err(|err| format!("insert scope edge: {err}"))?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'in_thread',?2)",
                params![fact.event_id, fact.thread_id],
            )
            .map_err(|err| format!("insert thread edge: {err}"))?;
            if let Some(tool) = fact.tool {
                tx.execute(
                    "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'authored_by',?2)",
                    params![fact.event_id, tool],
                )
                .map_err(|err| format!("insert tool edge: {err}"))?;
            }
            if let Some(role) = fact.role {
                tx.execute(
                    "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'role',?2)",
                    params![fact.event_id, role],
                )
                .map_err(|err| format!("insert role edge: {err}"))?;
            }
            if let Some(target) = fact.target {
                tx.execute(
                    "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'targets',?2)",
                    params![fact.event_id, target],
                )
                .map_err(|err| format!("insert target edge: {err}"))?;
            }
            if let Some(ref_id) = fact.ref_id {
                tx.execute(
                    "INSERT OR REPLACE INTO edges(from_id, relation, to_id) VALUES(?1,'refers_to',?2)",
                    params![fact.event_id, ref_id],
                )
                .map_err(|err| format!("insert ref edge: {err}"))?;
            }
        }
        tx.commit()
            .map_err(|err| format!("commit room projection: {err}"))?;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<RoomSnapshot, String> {
        self.rebuild_projection()?;
        let facts = self.projected_facts()?;
        let max_seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);
        let resolved = facts
            .iter()
            .filter(|f| f.kind == "resolve" || f.kind == "release")
            .filter_map(|f| f.ref_id.clone())
            .collect::<BTreeSet<_>>();
        let released_scopes = facts
            .iter()
            .filter(|f| f.kind == "release")
            .flat_map(|f| f.scope.clone())
            .collect::<BTreeSet<_>>();
        let mut trust_summary = BTreeMap::new();
        for fact in &facts {
            *trust_summary.entry(fact.trust_status.clone()).or_insert(0) += 1;
        }
        let active_claims = facts
            .iter()
            .filter(|f| f.kind == "claim")
            .filter(|f| !resolved.contains(&f.event_id))
            .filter(|f| !f.scope.iter().any(|scope| released_scopes.contains(scope)))
            .cloned()
            .collect::<Vec<_>>();
        let active_blockers = facts
            .iter()
            .filter(|f| f.kind == "blocker")
            .filter(|f| !resolved.contains(&f.event_id))
            .cloned()
            .collect::<Vec<_>>();
        let open_handoffs = facts
            .iter()
            .filter(|f| f.kind == "handoff")
            .filter(|f| !resolved.contains(&f.event_id))
            .cloned()
            .collect::<Vec<_>>();
        let current_decisions = facts
            .iter()
            .filter(|f| f.kind == "decision")
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>();
        let current_risks = facts
            .iter()
            .filter(|f| f.kind == "risk")
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>();
        let recent_artifacts = facts
            .iter()
            .filter(|f| f.kind == "artifact")
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>();
        let consumed_refs = facts
            .iter()
            .filter(|f| f.kind == "handoff" || f.kind == "resolve")
            .filter_map(|f| f.ref_id.clone())
            .collect::<BTreeSet<_>>();
        let unconsumed_artifacts = recent_artifacts
            .iter()
            .filter(|f| !consumed_refs.contains(&f.event_id))
            .cloned()
            .collect::<Vec<_>>();
        Ok(RoomSnapshot {
            max_seq,
            active_claims,
            active_blockers,
            open_handoffs,
            current_decisions,
            current_risks,
            recent_artifacts,
            unconsumed_artifacts,
            stale_facts: Vec::new(),
            trust_summary,
            adapters: adapter_contracts(),
        })
    }

    fn projected_facts(&self) -> Result<Vec<Fact>, String> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT seq,event_id,thread_id,kind,tool,role,subject,scope_json,created_at,summary,evidence_json,target,ref_id,status,severity,uri,origin,trust_status
                 FROM facts ORDER BY seq ASC",
            )
            .map_err(|err| format!("prepare projected facts: {err}"))?;
        let rows = stmt
            .query_map([], |row| {
                let scope_json: String = row.get(7)?;
                let evidence_json: String = row.get(10)?;
                let scope = serde_json::from_str::<Vec<String>>(&scope_json).unwrap_or_default();
                let evidence =
                    serde_json::from_str::<Vec<String>>(&evidence_json).unwrap_or_default();
                Ok(Fact {
                    seq: row.get(0)?,
                    event_id: row.get(1)?,
                    thread_id: row.get(2)?,
                    kind: row.get(3)?,
                    tool: row.get(4)?,
                    role: row.get(5)?,
                    subject: row.get(6)?,
                    scope,
                    created_at: row.get(8)?,
                    summary: row.get(9)?,
                    evidence,
                    target: row.get(11)?,
                    ref_id: row.get(12)?,
                    status: row.get(13)?,
                    severity: row.get(14)?,
                    uri: row.get(15)?,
                    origin: row.get(16)?,
                    trust_status: row.get(17)?,
                })
            })
            .map_err(|err| format!("query projected facts: {err}"))?;
        let mut facts = Vec::new();
        for row in rows {
            facts.push(row.map_err(|err| format!("read projected fact: {err}"))?);
        }
        Ok(facts)
    }

    fn export_handoff(
        &self,
        snapshot: &RoomSnapshot,
        reader_tool: Option<&str>,
    ) -> Result<String, String> {
        let path = self.root.join("HANDOFF.md");
        fs::write(&path, render_handoff(snapshot, reader_tool))
            .map_err(|err| format!("write HANDOFF.md: {err}"))?;
        Ok(path.display().to_string())
    }

    fn cursor_for(&self, tool: &str) -> Result<i64, String> {
        let conn = self.connection()?;
        conn.query_row(
            "SELECT last_seq FROM cursors WHERE tool=?1",
            params![tool],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| format!("read cursor: {err}"))
        .map(|value| value.unwrap_or(0))
    }

    fn set_cursor(&self, tool: &str, seq: i64) -> Result<(), String> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO cursors(tool,last_seq,updated_at) VALUES(?1,?2,?3)
             ON CONFLICT(tool) DO UPDATE SET last_seq=excluded.last_seq, updated_at=excluded.updated_at",
            params![tool, seq, now_string()],
        )
        .map_err(|err| format!("write cursor: {err}"))?;
        Ok(())
    }
}

fn build_entry(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    attention: &[Value],
) -> Value {
    let respond_to = snapshot
        .open_handoffs
        .iter()
        .filter(|f| {
            f.target
                .as_deref()
                .is_none_or(|target| target == tool || target == "all")
        })
        .map(|f| entry_item("respond_to_handoff", f))
        .collect::<Vec<_>>();
    let mut do_items = respond_to.clone();
    do_items.extend(
        snapshot
            .active_claims
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| entry_item("continue_or_release_claim", f)),
    );
    do_items.extend(
        snapshot
            .active_blockers
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| entry_item("resolve_owned_blocker", f)),
    );
    let mut do_not = Vec::new();
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() != Some(tool)
            && (paths.is_empty() || paths.iter().any(|path| claim.scope.contains(path)))
        {
            do_not.push(entry_item("avoid_claimed_scope", claim));
        }
    }
    let know = snapshot
        .current_decisions
        .iter()
        .take(8)
        .map(|f| entry_item("decision", f))
        .collect::<Vec<_>>();
    let verify = snapshot
        .unconsumed_artifacts
        .iter()
        .take(8)
        .map(|f| entry_item("unconsumed_artifact", f))
        .collect::<Vec<_>>();
    json!({
        "tool": tool,
        "role": role,
        "do": do_items,
        "do_not": do_not,
        "know": know,
        "verify": verify,
        "respond_to": respond_to,
        "ignore": [],
        "attention": attention,
        "adapter_contracts": adapter_contracts()
    })
}

#[derive(Clone, Debug)]
struct NextCandidate {
    action: &'static str,
    reason: &'static str,
    score: i64,
    fact: Option<Fact>,
    source_event_ids: Vec<String>,
}

impl NextCandidate {
    fn from_fact(action: &'static str, reason: &'static str, score: i64, fact: &Fact) -> Self {
        Self {
            action,
            reason,
            score,
            fact: Some(fact.clone()),
            source_event_ids: vec![fact.event_id.clone()],
        }
    }

    fn synthetic(
        action: &'static str,
        reason: &'static str,
        score: i64,
        source_event_ids: Vec<String>,
        fact: Option<Fact>,
    ) -> Self {
        Self {
            action,
            reason,
            score,
            fact,
            source_event_ids,
        }
    }

    fn seq(&self) -> i64 {
        self.fact.as_ref().map(|fact| fact.seq).unwrap_or_default()
    }

    fn to_json(&self) -> Value {
        let target_event_id = self.fact.as_ref().map(|fact| fact.event_id.clone());
        let confidence = (self.score.clamp(5, 95) as f64) / 100.0;
        json!({
            "action": self.action,
            "reason": self.reason,
            "score": self.score,
            "confidence": confidence,
            "target_event_id": target_event_id,
            "source_event_ids": self.source_event_ids,
            "fact": self.fact.as_ref().map(Fact::to_json)
        })
    }
}

fn build_next(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    limit: usize,
) -> Value {
    let waiting_on = snapshot
        .open_handoffs
        .iter()
        .chain(snapshot.active_blockers.iter())
        .filter(|fact| waiting_on_peer(fact, tool))
        .cloned()
        .collect::<Vec<_>>();
    let waiting = !waiting_on.is_empty();
    let mut candidates = Vec::new();

    for handoff in &snapshot.open_handoffs {
        if assigned_to_tool(handoff, tool) {
            candidates.push(NextCandidate::from_fact(
                "respond_to_handoff",
                "open_handoff_targeted_to_this_tool",
                boost_score(100, handoff, role, paths),
                handoff,
            ));
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.tool.as_deref() == Some(tool) {
            candidates.push(NextCandidate::from_fact(
                "resolve_owned_blocker",
                "owned_blocker_is_still_open",
                boost_score(90, blocker, role, paths),
                blocker,
            ));
        }
    }
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() == Some(tool) {
            candidates.push(NextCandidate::from_fact(
                "continue_or_release_claim",
                "owned_claim_is_still_active",
                boost_score(75, claim, role, paths),
                claim,
            ));
        }
    }
    for artifact in &snapshot.unconsumed_artifacts {
        let authored_by_self = artifact.tool.as_deref() == Some(tool);
        let routed_elsewhere = artifact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all");
        if !authored_by_self && !routed_elsewhere {
            candidates.push(NextCandidate::from_fact(
                "review_artifact",
                "unconsumed_peer_artifact_can_be_checked_while_waiting",
                boost_score(if waiting { 80 } else { 65 }, artifact, role, paths),
                artifact,
            ));
        }
    }
    for handoff in waiting_on.iter().filter(|fact| fact.kind == "handoff") {
        if fact_is_weak(handoff) {
            candidates.push(NextCandidate::from_fact(
                "clarify_handoff",
                "outgoing_handoff_needs_more_context_for_the_target_agent",
                boost_score(55, handoff, role, paths),
                handoff,
            ));
        }
    }

    if candidates.is_empty() {
        if waiting {
            candidates.push(NextCandidate::synthetic(
                "wait",
                "waiting_on_peer_with_no_useful_alternate_work",
                10,
                waiting_on
                    .iter()
                    .map(|fact| fact.event_id.clone())
                    .collect(),
                waiting_on.first().cloned(),
            ));
        } else {
            candidates.push(NextCandidate::synthetic(
                "proceed_solo",
                "no_room_items_require_action",
                5,
                Vec::new(),
                None,
            ));
        }
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.seq().cmp(&left.seq()))
            .then_with(|| left.action.cmp(right.action))
    });

    let top = candidates.first().cloned().unwrap_or_else(|| {
        NextCandidate::synthetic("proceed_solo", "empty_room", 5, Vec::new(), None)
    });
    let alternatives = candidates
        .iter()
        .skip(1)
        .take(limit.saturating_sub(1))
        .map(NextCandidate::to_json)
        .collect::<Vec<_>>();
    let mode = if waiting && top.action != "wait" {
        "useful_while_waiting"
    } else if waiting {
        "waiting"
    } else if top.action == "proceed_solo" {
        "idle"
    } else {
        "direct"
    };
    let waiting_json = facts_json(&waiting_on);
    let top_json = top.to_json();
    let contract = action_contract(&top, tool);

    json!({
        "mode": mode,
        "action": top_json["action"].clone(),
        "actionable": contract["actionable"].clone(),
        "reason": top_json["reason"].clone(),
        "score": top_json["score"].clone(),
        "confidence": top_json["confidence"].clone(),
        "requires_human": contract["requires_human"].clone(),
        "stop_reason": contract["stop_reason"].clone(),
        "target_event_id": top_json["target_event_id"].clone(),
        "source_event_ids": top_json["source_event_ids"].clone(),
        "fact": top_json["fact"].clone(),
        "suggested_claims": contract["suggested_claims"].clone(),
        "suggested_commands": contract["suggested_commands"].clone(),
        "completion": contract["completion"].clone(),
        "waiting_on": waiting_json,
        "alternatives": alternatives
    })
}

fn action_contract(candidate: &NextCandidate, tool: &str) -> Value {
    let actionable = !matches!(candidate.action, "wait" | "proceed_solo");
    let stop_reason = match candidate.action {
        "wait" => Some("waiting_on_peer_with_no_useful_alternate_work"),
        "proceed_solo" => Some("no_actionable_room_item"),
        _ => None,
    };
    let suggested_claims = candidate
        .fact
        .as_ref()
        .filter(|_| actionable && candidate.action != "continue_or_release_claim")
        .map(|fact| suggested_claims(tool, fact))
        .unwrap_or_default();
    let suggested_commands = if actionable {
        suggested_commands(tool, candidate)
    } else {
        Vec::new()
    };
    json!({
        "actionable": actionable,
        "requires_human": false,
        "stop_reason": stop_reason,
        "suggested_claims": suggested_claims,
        "suggested_commands": suggested_commands,
        "completion": completion_contract(candidate.action, actionable)
    })
}

fn suggested_claims(tool: &str, fact: &Fact) -> Vec<Value> {
    let scopes = executable_scopes(fact);
    scopes
        .into_iter()
        .map(|scope| {
            let path = command_path(&scope);
            json!({
                "scope": scope,
                "command": format!("rally2 say claim --tool {tool} --subject \"act on next\" --path {path} --json")
            })
        })
        .collect()
}

fn suggested_commands(tool: &str, candidate: &NextCandidate) -> Vec<String> {
    let Some(fact) = candidate.fact.as_ref() else {
        return Vec::new();
    };
    let mut commands = executable_scopes(fact)
        .into_iter()
        .map(|scope| {
            let path = command_path(&scope);
            format!("rally2 check before-write --tool {tool} --path {path} --strict --json")
        })
        .collect::<Vec<_>>();
    match candidate.action {
        "respond_to_handoff" => commands.push(format!(
            "rally2 say resolve --tool {tool} --ref {} --subject \"responded to handoff\" --json",
            fact.event_id
        )),
        "resolve_owned_blocker" => commands.push(format!(
            "rally2 say resolve --tool {tool} --ref {} --subject \"resolved blocker\" --json",
            fact.event_id
        )),
        "continue_or_release_claim" => commands.push(format!(
            "rally2 say release --tool {tool} --ref {} --subject \"done\" --json",
            fact.event_id
        )),
        "review_artifact" => commands.push(format!(
            "rally2 say artifact --tool {tool} --ref {} --subject \"reviewed artifact\" --uri {} --evidence \"<verification>\" --json",
            fact.event_id,
            fact.uri.as_deref().unwrap_or("<path>")
        )),
        "clarify_handoff" => commands.push(format!(
            "rally2 say handoff --tool {tool} --target {} --ref {} --subject \"clarify handoff\" --summary \"<needed context>\" --json",
            fact.target.as_deref().unwrap_or("<target-tool>"),
            fact.event_id
        )),
        _ => {}
    }
    commands
}

fn completion_contract(action: &str, actionable: bool) -> Value {
    let record_kind = match action {
        "respond_to_handoff" | "resolve_owned_blocker" => "resolve",
        "continue_or_release_claim" => "artifact_or_release",
        "review_artifact" => "artifact",
        "clarify_handoff" => "handoff",
        _ => "none",
    };
    json!({
        "record_kind": record_kind,
        "evidence_required": actionable,
        "release_claims": actionable,
        "rerun_next": actionable
    })
}

fn executable_scopes(fact: &Fact) -> Vec<String> {
    let mut scopes = fact.scope.clone();
    if scopes.is_empty() {
        if let Some(uri) = &fact.uri {
            if uri.starts_with("file:") {
                scopes.push(uri.clone());
            } else if !uri.contains("://") {
                scopes.push(normalize_path(uri.clone()));
            }
        }
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn command_path(scope: &str) -> String {
    scope.strip_prefix("file:").unwrap_or(scope).to_string()
}

fn assigned_to_tool(fact: &Fact, tool: &str) -> bool {
    fact.target
        .as_deref()
        .is_none_or(|target| target == tool || target == "all")
}

fn waiting_on_peer(fact: &Fact, tool: &str) -> bool {
    fact.tool.as_deref() == Some(tool)
        && fact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all")
}

fn fact_is_weak(fact: &Fact) -> bool {
    fact.summary
        .as_deref()
        .is_none_or(|summary| summary.trim().is_empty())
        && fact.evidence.is_empty()
}

fn boost_score(base: i64, fact: &Fact, role: Option<&str>, paths: &[String]) -> i64 {
    let role_boost = match (role, fact.kind.as_str()) {
        (Some("reviewer" | "qa"), "artifact") => 10,
        (Some("builder"), "claim") => 5,
        _ => 0,
    };
    let path_boost = if !paths.is_empty()
        && paths
            .iter()
            .any(|path| fact.scope.iter().any(|scope| scope == path))
    {
        5
    } else {
        0
    };
    (base + role_boost + path_boost).min(100)
}

fn build_attention(
    snapshot: &RoomSnapshot,
    tool: &str,
    cursor_before: i64,
    paths: &[String],
) -> Vec<Value> {
    let mut items = Vec::new();
    let mut push_fact = |reason: &str, fact: &Fact| {
        if fact.seq > cursor_before {
            items.push(json!({
                "reason": reason,
                "event_id": fact.event_id,
                "seq": fact.seq,
                "kind": fact.kind,
                "subject": fact.subject,
                "scope": fact.scope,
                "tool": fact.tool,
                "target": fact.target
            }));
        }
    };
    for handoff in &snapshot.open_handoffs {
        if handoff
            .target
            .as_deref()
            .is_none_or(|target| target == tool || target == "all")
        {
            push_fact("handoff_assigned", handoff);
        }
    }
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() != Some(tool)
            && (paths.is_empty() || paths.iter().any(|path| claim.scope.contains(path)))
        {
            push_fact("claimed_scope", claim);
        }
    }
    for fact in snapshot
        .current_decisions
        .iter()
        .chain(snapshot.current_risks.iter())
        .chain(snapshot.unconsumed_artifacts.iter())
    {
        push_fact("new_room_fact", fact);
    }
    items
}

fn entry_item(reason: &str, fact: &Fact) -> Value {
    json!({
        "reason": reason,
        "event_id": fact.event_id,
        "kind": fact.kind,
        "subject": fact.subject,
        "scope": fact.scope,
        "tool": fact.tool,
        "target": fact.target,
        "evidence": fact.evidence
    })
}

fn adapter_contracts() -> Vec<Value> {
    [
        adapter_contract(
            "codex",
            false,
            false,
            true,
            false,
            "Write-boundary guard only. Managed sessions own live Rally delivery.",
        ),
        adapter_contract(
            "claude_code",
            false,
            false,
            true,
            false,
            "Write-boundary guard only. Managed sessions own live Rally delivery.",
        ),
        adapter_contract(
            "pi",
            false,
            false,
            true,
            false,
            "Write-boundary guard only. Managed sessions own live Rally delivery.",
        ),
        adapter_contract(
            "herdr",
            false,
            false,
            false,
            false,
            "Managed-session backend metadata. Use rally2 run/inject/capture for live delivery.",
        ),
        adapter_contract(
            "cmux",
            false,
            false,
            false,
            false,
            "Managed-session backend metadata. Use rally2 run/inject/capture for live delivery.",
        ),
        adapter_contract(
            "ci",
            false,
            false,
            true,
            true,
            "Read room/check output in automation and publish evidence facts when useful.",
        ),
    ]
    .into_iter()
    .collect()
}

fn adapter_contract(
    adapter: &str,
    startup_enter: bool,
    loop_enter: bool,
    before_write_check: bool,
    completion_prompt: bool,
    model_visible: &str,
) -> Value {
    json!({
        "adapter": adapter,
        "first_class": true,
        "model_visible": model_visible,
        "commands": {
            "enter": format!("rally2 enter --tool {adapter} --json"),
            "check_before_write": format!("rally2 check before-write --tool {adapter} --path <path> --json"),
            "say_artifact": format!("rally2 say artifact --tool {adapter} --subject <subject> --uri <path> --evidence <evidence> --json"),
            "room": "rally2 room --json"
        },
        "surfaces": {
            "startup_enter": startup_enter,
            "loop_enter": loop_enter,
            "before_write_check": before_write_check,
            "completion_prompt": completion_prompt
        }
    })
}

fn adapter_for(tool: &str) -> Value {
    let normalized = match tool {
        "claude" | "claude-code" | "claude_code" => "claude_code",
        "codex" => "codex",
        "pi" => "pi",
        "herdr" => "herdr",
        "cmux" => "cmux",
        "ci" => "ci",
        _ => "generic",
    };
    adapter_contracts()
        .into_iter()
        .find(|adapter| adapter["adapter"] == normalized)
        .unwrap_or_else(|| {
            json!({
                "adapter": normalized,
                "first_class": false,
                "surfaces": {
                    "startup_enter": true,
                    "loop_enter": false,
                    "before_write_check": false,
                    "completion_prompt": false
                }
            })
        })
}

fn render_handoff(snapshot: &RoomSnapshot, reader_tool: Option<&str>) -> String {
    let mut out = String::from("# Rally Handoff\n\n");
    let do_not_touch = snapshot
        .active_claims
        .iter()
        .filter(|fact| reader_tool.is_none_or(|tool| fact.tool.as_deref() != Some(tool)))
        .cloned()
        .collect::<Vec<_>>();
    let active_work = snapshot
        .active_claims
        .iter()
        .filter(|fact| reader_tool.is_some_and(|tool| fact.tool.as_deref() == Some(tool)))
        .cloned()
        .collect::<Vec<_>>();
    push_section(&mut out, "Do Not Touch", &do_not_touch);
    push_section(&mut out, "Active Work", &active_work);
    push_section(&mut out, "Open Handoffs", &snapshot.open_handoffs);
    push_section(&mut out, "Blockers", &snapshot.active_blockers);
    push_section(&mut out, "Decisions", &snapshot.current_decisions);
    push_section(&mut out, "Risks", &snapshot.current_risks);
    push_section(&mut out, "Recent Artifacts", &snapshot.recent_artifacts);
    out.push_str("## Evidence\n");
    let mut wrote = false;
    for fact in &snapshot.recent_artifacts {
        for evidence in &fact.evidence {
            wrote = true;
            out.push_str(&format!("- {}: {}\n", fact.subject, evidence));
        }
    }
    if !wrote {
        out.push_str("- None\n");
    }
    out.push('\n');
    out.push_str("## Next Attention Points\n");
    for fact in snapshot
        .open_handoffs
        .iter()
        .chain(snapshot.active_blockers.iter())
        .chain(snapshot.unconsumed_artifacts.iter())
    {
        out.push_str(&format!("- {} [{}]\n", fact.subject, fact.event_id));
    }
    out
}

fn push_section(out: &mut String, heading: &str, facts: &[Fact]) {
    out.push_str(&format!("## {heading}\n"));
    if facts.is_empty() {
        out.push_str("- None\n\n");
        return;
    }
    for fact in facts {
        let scope = if fact.scope.is_empty() {
            String::new()
        } else {
            format!(" ({})", fact.scope.join(", "))
        };
        out.push_str(&format!(
            "- {}{} [{}]\n",
            fact.subject, scope, fact.event_id
        ));
    }
    out.push('\n');
}

fn trusted_for_automation(fact: &Fact) -> bool {
    fact.origin == "local" || matches!(fact.trust_status.as_str(), "local" | "trusted")
}

fn filter_facts(facts: Vec<Fact>, query: &RoomQuery) -> Vec<Fact> {
    facts
        .into_iter()
        .filter(|fact| query.matches(fact))
        .collect()
}

fn check_before_write(
    snapshot: &RoomSnapshot,
    tool: &str,
    path: Option<&str>,
    findings: &mut Vec<Value>,
) {
    let Some(path) = path else {
        findings.push(json!({
            "code": "missing-path",
            "severity": "warn",
            "message": "before-write checks are stronger with --path"
        }));
        return;
    };
    for claim in &snapshot.active_claims {
        if claim.scope.iter().any(|scope| scope == path)
            && claim.tool.as_deref() != Some(tool)
            && trusted_for_automation(claim)
        {
            findings.push(json!({
                "code": "claimed-path",
                "severity": "stop",
                "message": "another agent has claimed this path",
                "fact_id": claim.event_id,
                "owner": claim.tool,
                "path": path
            }));
        }
    }
    for decision in &snapshot.current_decisions {
        if decision.scope.iter().any(|scope| scope == path) || decision.scope.is_empty() {
            findings.push(json!({
                "code": "binding-decision",
                "severity": "info",
                "message": decision.subject,
                "fact_id": decision.event_id,
                "path": path
            }));
        }
    }
    for blocker in &snapshot.active_blockers {
        if (blocker.scope.is_empty() || blocker.scope.iter().any(|scope| scope == path))
            && trusted_for_automation(blocker)
        {
            findings.push(json!({
                "code": "active-blocker",
                "severity": "stop",
                "message": blocker.subject,
                "fact_id": blocker.event_id,
                "path": path
            }));
        }
    }
}

fn check_after_artifact(args: &ArgBag, findings: &mut Vec<Value>) {
    if args.one("--evidence").is_none() {
        findings.push(json!({
            "code": "missing-evidence",
            "severity": "warn",
            "message": "artifact completion should include evidence for the next agent"
        }));
    }
    if args.one("--target").is_none() && args.one("--to").is_none() {
        findings.push(json!({
            "code": "missing-route",
            "severity": "info",
            "message": "consider routing the artifact with --target when another agent should act"
        }));
    }
}

fn check_before_complete(snapshot: &RoomSnapshot, tool: &str, findings: &mut Vec<Value>) {
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() == Some(tool) {
            findings.push(json!({
                "code": "owned-active-claim",
                "severity": "stop",
                "message": "release or explain this active claim before completion",
                "fact_id": claim.event_id,
                "scope": claim.scope
            }));
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.tool.as_deref() == Some(tool) && trusted_for_automation(blocker) {
            findings.push(json!({
                "code": "owned-active-blocker",
                "severity": "warn",
                "message": "completion still has an active blocker from this tool",
                "fact_id": blocker.event_id
            }));
        }
    }
}

fn facts_json(facts: &[Fact]) -> Vec<Value> {
    facts.iter().map(Fact::to_json).collect()
}

fn scopes_from(args: &ArgBag) -> Vec<String> {
    let mut scopes = Vec::new();
    scopes.extend(args.all("--scope"));
    scopes.extend(args.all("--resource"));
    scopes.extend(args.all("--path").into_iter().map(normalize_path));
    scopes.sort();
    scopes.dedup();
    scopes
}

fn normalize_path(path: String) -> String {
    if path.starts_with("file:") {
        return path;
    }
    let stripped = path.strip_prefix("./").unwrap_or(&path);
    format!("file:{stripped}")
}

fn parse_i64_option(args: &ArgBag, name: &str) -> Result<Option<i64>, String> {
    args.one(name)
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| format!("invalid {name} value {value}"))
        })
        .transpose()
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
        _ => kind.to_string(),
    }
}

fn envelope(command: &str, schema: &str, data: Value) -> Value {
    json!({
        "ok": true,
        "product": "rally2",
        "command": command,
        "schema": schema,
        "data": data
    })
}

fn repo_root() -> Result<PathBuf, String> {
    let mut dir = env::current_dir().map_err(|err| format!("current dir: {err}"))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return env::current_dir().map_err(|err| format!("current dir: {err}"));
        }
    }
}

fn new_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}_{:x}_{:x}", std::process::id(), nanos)
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn help_text() -> String {
    [
        "rally2: repo-local coordination room for parallel agents",
        "",
        "Usage:",
        "  rally2 enter --tool <tool> [--path <path>] [--role <role>] [--json]",
        "  rally2 say <kind> --tool <tool> --subject <subject> [--path <path>] [--json]",
        "  rally2 room [--tool <tool>] [--role <role>] [--path <path>] [--since <seq>] [--json]",
        "  rally2 next --tool <tool> [--path <path>] [--role <role>] [--limit <n>] [--json]",
        "  rally2 check before-write --tool <tool> --path <path> [--strict] [--json]",
        "  rally2 check after-artifact --tool <tool> [--evidence <text>] [--target <tool>] [--json]",
        "  rally2 check before-complete --tool <tool> [--strict] [--json]",
        "  rally2 install <codex|claude_code|pi|herdr|cmux|ci|all> [--dry-run] [--uninstall] [--json]",
        "  rally2 run <claude|codex|opencode|gemini> [--name <name>] [--backend <tmux|herdr|cmux>] [--dry-run] [--json]",
        "  rally2 sessions [--json]",
        "  rally2 inject <session|name|tool> (--text <text>|--handoff <event-id>) [--require-ack] [--json]",
        "  rally2 attach <session|name|tool> [--dry-run] [--json]",
        "  rally2 capture <session|name|tool> [--lines <n>] [--dry-run] [--json]",
        "  rally2 stop <session|name|tool> [--dry-run] [--json]",
        "",
        "Fact kinds: claim, release, blocker, resolve, decision, artifact, handoff, risk, lesson",
    ]
    .join("\n")
}

struct ArgBag {
    command: &'static str,
    positional: Vec<String>,
    options: BTreeMap<String, Vec<String>>,
    flags: BTreeSet<String>,
}

impl ArgBag {
    fn new(command: &'static str, args: Vec<String>) -> Self {
        let mut positional = Vec::new();
        let mut options: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut flags = BTreeSet::new();
        let mut iter = args.into_iter().peekable();
        while let Some(arg) = iter.next() {
            if !arg.starts_with("--") {
                positional.push(arg);
                continue;
            }

            if let Some((name, value)) = arg.split_once('=') {
                if option_takes_value(name) {
                    options
                        .entry(name.to_string())
                        .or_default()
                        .push(value.to_string());
                } else {
                    flags.insert(name.to_string());
                }
                continue;
            }

            if option_takes_value(&arg) {
                if let Some(value) = iter.peek() {
                    if !value.starts_with("--") {
                        let value = iter.next().unwrap_or_default();
                        options.entry(arg).or_default().push(value);
                        continue;
                    }
                }
            }
            flags.insert(arg);
        }
        Self {
            command,
            positional,
            options,
            flags,
        }
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn one(&self, name: &str) -> Option<String> {
        self.options.get(name).and_then(|v| v.first()).cloned()
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.options.get(name).cloned().unwrap_or_default()
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.one(name)
            .ok_or_else(|| format!("{} requires {name}", self.command))
    }
}

fn option_takes_value(name: &str) -> bool {
    matches!(
        name,
        "--tool"
            | "--session-id"
            | "--role"
            | "--path"
            | "--since"
            | "--subject"
            | "--thread-id"
            | "--scope"
            | "--resource"
            | "--evidence"
            | "--target"
            | "--to"
            | "--ref"
            | "--status"
            | "--severity"
            | "--uri"
            | "--origin"
            | "--trust-status"
            | "--event"
            | "--thread"
            | "--home"
            | "--rally2-bin"
            | "--name"
            | "--backend"
            | "--tmux-bin"
            | "--herdr-bin"
            | "--cmux-bin"
            | "--text"
            | "--handoff"
            | "--timeout-seconds"
            | "--lines"
    )
}

struct Output {
    json: bool,
    text: String,
    body: Value,
    exit_code: u8,
}

impl Output {
    fn new(json: bool, text: String, body: Value) -> Self {
        Self {
            json,
            text,
            body,
            exit_code: 0,
        }
    }

    fn with_exit_code(mut self, exit_code: u8) -> Self {
        self.exit_code = exit_code;
        self
    }

    fn print(self) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&self.body).unwrap_or(self.body.to_string())
            );
        } else {
            println!("{}", self.text);
        }
    }
}

struct CliError {
    message: String,
    exit_code: u8,
    json: bool,
}

impl CliError {
    fn classify(message: String, json: bool) -> Self {
        let exit_code = if is_usage_error(&message) {
            2
        } else if is_not_found_error(&message) {
            3
        } else {
            1
        };
        Self {
            message,
            exit_code,
            json,
        }
    }

    fn print(&self) {
        if self.json {
            eprintln!(
                "{}",
                json!({
                    "ok": false,
                    "product": "rally2",
                    "error": self.message,
                    "exit_code": self.exit_code
                })
            );
        } else {
            eprintln!("rally2: {}", self.message);
        }
    }
}

fn is_usage_error(message: &str) -> bool {
    message.contains("requires")
        || message.starts_with("unknown Rally 2 command")
        || message.starts_with("unsupported")
        || message.starts_with("invalid")
}

fn is_not_found_error(message: &str) -> bool {
    message.contains("not found")
}
