// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{
    CommonOptions, HerdrInjectCommand, HookCommand, JudgeCommand, ReadCommand, RepairCommand,
    SetupCommand, StartCommand,
};
use crate::output::{CliError, WriteOutput};
use crate::resources::normalize_file_resource;
use crate::runtime::{new_id, now_rfc3339};
use rally_core::context::{
    ContextBrief, ContextInputs, ContextRecommendation, build_context_brief_from_inputs,
    build_work_packet,
};
use rally_core::cursors;
use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::event::{ClaimPayload, EventBuilder, EventPayload, EventRecord, ProfilePayload};
use rally_core::graph;
use rally_core::preflight::{PreflightOptions, run_preflight};
use rally_core::query::{filter_since, now_epoch_seconds, parse_since, record_id, related_records};
use rally_core::store::ChannelStore;
use rally_protocol::event_value;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

type DoctorGraphInputs = (
    Vec<rally_core::query::ActiveClaim>,
    Vec<rally_core::query::ActiveTask>,
    Vec<rally_core::query::PendingHandoff>,
    bool,
);

/// Gather the four inputs `execute_doctor` needs from the graph in one
/// shot. Returns Err on any graph error so the caller can decide to
/// fall back to TraceProjection.
fn doctor_inputs_from_graph(
    store: &ChannelStore,
    records: &[Value],
    tool: &str,
    now: f64,
) -> Result<DoctorGraphInputs, Box<dyn std::error::Error>> {
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())?;
    graph::catch_up(&mut conn, records, &now_rfc3339())?;
    Ok((
        graph::active_claims_typed(&conn, None, now)?,
        graph::active_tasks_typed(&conn, None)?,
        graph::pending_handoffs_typed(&conn, None, now)?,
        graph::latest_profile_typed(&conn, tool)?.is_some(),
    ))
}

/// Build a ContextBrief from the SQLite graph projection. The graph is
/// the source of truth for this surface — propagate errors up rather
/// than silently masking them with a stale in-memory projection.
fn brief_from_graph(
    store: &ChannelStore,
    records: &[Value],
    tool: &str,
    recent_limit: usize,
    now: f64,
) -> Result<ContextBrief, CliError> {
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime("context", format!("open graph: {err}")))?;
    graph::catch_up(&mut conn, records, &now_rfc3339())
        .map_err(|err| CliError::runtime("context", format!("graph catch_up: {err}")))?;
    let inputs = ContextInputs::from_graph(&conn, tool, recent_limit, now)
        .map_err(|err| CliError::runtime("context", format!("graph inputs: {err}")))?;
    Ok(build_context_brief_from_inputs(&inputs))
}

/// Filter `records` to those with `local_seq` strictly greater than the cursor
/// for `(tool, session_id)` under `store.channel_dir()`. Returns the filtered
/// records and the largest `local_seq` observed across the full input — the
/// caller advances the cursor to that value on successful read (unless --peek).
struct CursorScope {
    tool: String,
    session_id: String,
    max_seq: u64,
}

fn apply_cursor(
    store: &ChannelStore,
    records: Vec<Value>,
    command: &ReadCommand,
) -> (Vec<Value>, Option<CursorScope>) {
    if !command.since_cursor {
        return (records, None);
    }
    let (Some(tool), Some(session_id)) = (command.tool.as_deref(), command.session_id.as_deref())
    else {
        // --since-cursor without tool+session is a no-op; the cursor key would be ambiguous.
        return (records, None);
    };
    let cursor = cursors::read_cursor(store.channel_dir(), tool, session_id);
    let mut max_seq = cursor;
    let filtered: Vec<Value> = records
        .into_iter()
        .filter_map(|record| {
            let seq = record.get("local_seq").and_then(Value::as_u64).unwrap_or(0);
            if seq > max_seq {
                max_seq = seq;
            }
            if seq > cursor { Some(record) } else { None }
        })
        .collect();
    (
        filtered,
        Some(CursorScope {
            tool: tool.to_string(),
            session_id: session_id.to_string(),
            max_seq,
        }),
    )
}

fn maybe_advance_cursor(store: &ChannelStore, scope: Option<&CursorScope>, command: &ReadCommand) {
    let Some(scope) = scope else { return };
    if command.peek {
        return;
    }
    // Cursor advance is best-effort: a write failure must not poison the read.
    let _ = cursors::write_cursor(
        store.channel_dir(),
        &scope.tool,
        &scope.session_id,
        scope.max_seq,
        &now_rfc3339(),
    );
}

/// Open the graph projection, catch it up against the provided records,
/// and return the connection. Errors propagate as CliError so callers
/// surface graph failures rather than silently degrading.
fn graph_caught_up(
    command: &'static str,
    store: &ChannelStore,
    records: &[Value],
) -> Result<graph::GraphConnection, CliError> {
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime(command, format!("open graph: {err}")))?;
    graph::catch_up(&mut conn, records, &now_rfc3339())
        .map_err(|err| CliError::runtime(command, format!("graph catch_up: {err}")))?;
    Ok(conn)
}

pub(super) fn execute_inbox(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let (records, cursor_scope) = apply_cursor(&store, records, &command);
    // Inbox uses cursor-scoped record filtering — only events past the
    // per-session cursor count as "new." A persistent graph projection
    // can't model that (it reflects the full log), so this surface
    // stays on the in-memory record scan from rally-core::query.
    let pending = rally_core::query::pending_handoffs_at(&records, command.tool.as_deref(), now);
    let text = if pending.is_empty() {
        "No pending handoffs.".to_string()
    } else {
        pending
            .iter()
            .map(|item| {
                let files = if item.files.is_empty() {
                    String::new()
                } else {
                    format!(" files={}", item.files.join(","))
                };
                format!(
                    "{} from={} to={}: {}{}",
                    item.event_id,
                    item.from_tool.as_deref().unwrap_or("unknown"),
                    item.to_tool.as_deref().unwrap_or("all"),
                    item.subject,
                    files
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let output = query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "pending": pending,
            "cursor_advanced": cursor_scope.is_some() && !command.peek,
            "cursor_last_seq": cursor_scope.as_ref().map(|s| s.max_seq),
        }),
    );
    maybe_advance_cursor(&store, cursor_scope.as_ref(), &command);
    Ok(output)
}

pub(super) fn execute_claims(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let conn = graph_caught_up("claims", &store, &records)?;
    let claims = graph::active_claims_typed(&conn, command.tool.as_deref(), now)
        .map_err(|err| CliError::runtime("claims", format!("active_claims: {err}")))?;
    let text = if claims.is_empty() {
        "No active claims.".to_string()
    } else {
        claims
            .iter()
            .map(|item| {
                format!(
                    "{} owner={} resource={}\n  {}",
                    item.event_id,
                    item.owner_tool.as_deref().unwrap_or("unknown"),
                    item.resource,
                    item.subject
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "claims": claims,
        }),
    ))
}

pub(super) fn execute_blockers(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let conn = graph_caught_up("blockers", &store, &records)?;
    let blockers = graph::active_blockers_typed(&conn, command.tool.as_deref(), now)
        .map_err(|err| CliError::runtime("blockers", format!("active_blockers: {err}")))?;
    let text = if blockers.is_empty() {
        "No blockers.".to_string()
    } else {
        blockers
            .iter()
            .map(|item| {
                let resource = item
                    .resource
                    .as_ref()
                    .map(|resource| format!(" resource={resource}"))
                    .unwrap_or_default();
                format!(
                    "{} tool={} severity={}{}\n  {}",
                    item.event_id,
                    item.tool.as_deref().unwrap_or("unknown"),
                    item.severity,
                    resource,
                    item.subject
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "blockers": blockers,
        }),
    ))
}

pub(super) fn execute_conflicts(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, _now) = query_records(&command)?;
    let conn = graph_caught_up("conflicts", &store, &records)?;
    let conflicts = graph::claim_conflicts_typed(&conn)
        .map_err(|err| CliError::runtime("conflicts", format!("claim_conflicts: {err}")))?;
    let text = if conflicts.is_empty() {
        "No claim conflicts.".to_string()
    } else {
        conflicts
            .iter()
            .map(|item| {
                format!(
                    "{} claimed by {} ({})",
                    item.resource,
                    item.owners.join(", "),
                    item.claim_ids.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "conflicts": conflicts,
        }),
    ))
}

pub(super) fn execute_context(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    let brief = brief_from_graph(&store, &records, &tool, command.limit, now)?;
    let text = if let Some(priority) = &brief.top_priority {
        format!(
            "{}: {} ({})",
            brief.recommended_next_action.action, priority.subject, priority.event_id
        )
    } else {
        brief.recommended_next_action.reason.clone()
    };

    // When `--focus <event-id>` is set, attach a graph-backed neighborhood
    // + causal chain so the agent gets bounded context instead of the
    // firehose. Failure to open/catch-up the graph silently omits the
    // graph field rather than failing the whole command — the legacy
    // brief is still useful on its own.
    let graph_view = command
        .focus
        .as_deref()
        .map(|focus| focus_graph_view(&store, &records, focus));

    let mut data = json!({ "brief": brief });
    if let Some(Some(payload)) = graph_view {
        data["graph"] = payload;
    }
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        data,
    ))
}

/// Best-effort: open the graph projection, catch up to the latest log
/// position, and return `{subgraph, chain, status}` rooted at `focus`.
/// Returns None on any graph error — caller treats this as a silent omit.
fn focus_graph_view(store: &ChannelStore, records: &[Value], focus: &str) -> Option<Value> {
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339()).ok()?;
    graph::catch_up(&mut conn, records, &now_rfc3339()).ok()?;
    let meta = graph::read_meta(&conn).ok()?;
    let subgraph = graph::subgraph_around(&conn, focus, 1).ok()?;
    let chain = graph::causal_chain(&conn, focus, 5).ok()?;
    let target = graph::get_node(&conn, focus).ok()?;
    Some(json!({
        "focus": focus,
        "status": meta.status.as_str(),
        "last_applied_seq": meta.last_applied_seq,
        "target": target,
        "subgraph": subgraph,
        "chain": chain,
    }))
}

pub(super) fn execute_packet(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    let brief = brief_from_graph(&store, &records, &tool, command.limit, now)?;
    let packet = build_work_packet(&brief, command.limit);
    let text = format!(
        "{} packet for {}: {}",
        packet.packet_kind, packet.tool, packet.recommended_next_action.reason
    );
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "packet": packet,
        }),
    ))
}

pub(super) fn execute_start(command: StartCommand) -> Result<WriteOutput, CliError> {
    let mut common = command.common.clone();
    common.tool = Some(command.tool.clone());
    let store = common.channel_store("start")?;
    let preflight = run_preflight(
        &store,
        &PreflightOptions {
            tool: command.tool.clone(),
            model: common.model.clone(),
            session_id: command.session_id.clone(),
            start_ping: true,
            stale_after_seconds: command.stale_after_seconds,
            recent_limit: 5,
        },
    )
    .map_err(|err| CliError::runtime("start", format!("failed to start session: {err}")))?;

    let records = store
        .load_records_cached()
        .map_err(|err| CliError::runtime("start", format!("failed to load channel: {err}")))?;
    let max_seq = records
        .iter()
        .filter_map(|record| record.get("local_seq").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let cursor_before =
        cursors::read_cursor(store.channel_dir(), &command.tool, &command.session_id);
    let unseen_count = records
        .iter()
        .filter(|record| {
            record
                .get("local_seq")
                .and_then(Value::as_u64)
                .is_some_and(|seq| seq > cursor_before)
        })
        .count();
    let cursor_advanced = !command.peek;
    if cursor_advanced {
        cursors::write_cursor(
            store.channel_dir(),
            &command.tool,
            &command.session_id,
            max_seq,
            &now_rfc3339(),
        )
        .map_err(|err| CliError::runtime("start", format!("failed to write cursor: {err}")))?;
    }

    let now = now_epoch_seconds();
    let brief = brief_from_graph(&store, &records, &command.tool, command.limit, now)?;
    let packet = build_work_packet(&brief, command.limit);
    let agent_visible = agent_visible_from_context(&brief.recommended_next_action);
    let checkpoint = store
        .checkpoint_status()
        .map_err(|err| CliError::runtime("start", format!("failed to read checkpoint: {err}")))?;
    let warnings = start_warnings(&command.tool, &preflight, &checkpoint);
    let watch_command = format!(
        "rally watch --tool {} --session-id {} --since-cursor",
        command.tool, command.session_id
    );
    let text = format!(
        "started rally session tool={} session={} action={} warnings={}",
        command.tool,
        command.session_id,
        brief.recommended_next_action.action,
        warnings.len()
    );
    Ok(WriteOutput {
        json: common.json || !command.human,
        text,
        body: json!({
            "ok": true,
            "command": "start",
            "schema": "agent-rally.command.start.v1",
            "channel": store.channel_dir().display().to_string(),
            "tool": command.tool,
            "session_id": command.session_id,
            "started_process": false,
            "preflight": preflight,
            "context": { "brief": brief },
            "packet": packet,
            "agent_visible": agent_visible,
            "checkpoint": checkpoint,
            "cursor": {
                "before": cursor_before,
                "after": if cursor_advanced { max_seq } else { cursor_before },
                "max_seq": max_seq,
                "unseen_count": unseen_count,
                "advanced": cursor_advanced
            },
            "warnings": warnings,
            "next_commands": {
                "watch": watch_command,
                "packet": format!("rally packet --tool {} --json", command.tool),
                "context": format!("rally context --tool {} --json", command.tool)
            }
        }),
    })
}

pub(super) fn execute_doctor(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    let diagnosis = diagnose_records(
        &records,
        DiagnoseOptions {
            state_records: Some(&records),
            tool: command.tool.as_deref(),
            stale_after_seconds: command.stale_after_seconds,
            since: command.since.as_deref(),
            now_epoch_seconds: now,
        },
    );
    let checkpoint = store.checkpoint_status().map_err(|err| {
        CliError::runtime(command.command, format!("failed to read checkpoint: {err}"))
    })?;
    let setup = setup_status(&store)?;
    let (active_claims, active_tasks, pending_handoffs, profile_present) =
        doctor_inputs_from_graph(&store, &records, &tool, now)
            .map_err(|err| CliError::runtime("doctor", format!("graph: {err}")))?;
    let mut findings = Vec::new();
    for finding in diagnosis.findings {
        findings.push(json!({
            "severity": finding.severity,
            "code": finding.code,
            "message": finding.message,
            "event_id": finding.event_id,
        }));
    }
    for claim in &active_claims {
        if claim.owner_tool.as_deref() == Some("unknown") {
            findings.push(json!({
                "severity": enforcement_severity(&setup.enforcement),
                "code": "anonymous-claim",
                "message": "active claim has owner_tool=unknown",
                "event_id": claim.event_id,
                "resource": claim.resource,
            }));
        }
    }
    for task in &active_tasks {
        if task.owner_tool.as_deref() == Some("unknown") {
            findings.push(json!({
                "severity": enforcement_severity(&setup.enforcement),
                "code": "anonymous-task",
                "message": "active task has owner_tool=unknown",
                "event_id": task.event_id,
            }));
        }
    }
    for handoff in &pending_handoffs {
        if handoff.from_tool.as_deref() == Some("unknown") {
            findings.push(json!({
                "severity": enforcement_severity(&setup.enforcement),
                "code": "anonymous-handoff",
                "message": "pending handoff has from_tool=unknown",
                "event_id": handoff.event_id,
            }));
        }
    }
    if !checkpoint.valid {
        findings.push(json!({
            "severity": "P2",
            "code": "checkpoint-invalid",
            "message": checkpoint.reason.as_deref().unwrap_or("checkpoint is invalid"),
        }));
    }
    if command.tool.is_some() && !profile_present {
        findings.push(json!({
            "severity": "P3",
            "code": "missing-profile",
            "message": format!("tool {tool} has not written a profile"),
        }));
    }
    let p1 = findings.iter().any(|finding| finding["severity"] == "P1");
    let status = if p1 {
        "fail"
    } else if findings.is_empty() {
        "pass"
    } else {
        "warn"
    };
    let text = format!("doctor {status} findings={}", findings.len());
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "doctor": {
                "status": status,
                "tool": command.tool,
                "enforcement": setup.enforcement,
                "findings": findings,
                "checkpoint": checkpoint,
                "installed_tools": setup.tools,
            }
        }),
    ))
}

pub(super) fn execute_next(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    // Graph is the source of truth for `rally next`. Propagate errors
    // rather than masking with TraceProjection — agents need to know
    // when the projection cache is broken.
    let graph_view = open_graph_view(&store, &records, &tool, now)?;
    let recommendation = next_recommendation(&tool, command.limit, &graph_view);
    let text = recommendation
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("idle")
        .to_string();
    Ok(query_output(
        "next",
        &command.common,
        &store,
        text,
        json!({ "next": recommendation }),
    ))
}

/// Bundle of graph-derived inputs to `next_recommendation`. Graph is
/// the source of truth — there is no fallback path.
pub(crate) struct GraphView {
    pub profile: Option<rally_core::query::AgentProfile>,
    pub pending_handoffs: Vec<Value>,
    pub active_blockers: Vec<Value>,
    pub owned_active_tasks: Vec<Value>,
    pub unowned_active_tasks: Vec<Value>,
    pub recent_artifacts: Vec<Value>,
    pub unconsumed_ids: HashSet<String>,
}

/// Open + catch up the graph, then load all the projections
/// `next_recommendation` needs in one place. Errors propagate — agents
/// need to know when the projection cache is unreadable.
fn open_graph_view(
    store: &ChannelStore,
    records: &[Value],
    tool: &str,
    _now: f64,
) -> Result<GraphView, CliError> {
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime("next", format!("open graph: {err}")))?;
    graph::catch_up(&mut conn, records, &now_rfc3339())
        .map_err(|err| CliError::runtime("next", format!("graph catch_up: {err}")))?;
    let profile = graph::latest_profile_typed(&conn, tool)
        .map_err(|err| CliError::runtime("next", format!("profile: {err}")))?;
    let pending_handoffs = graph::pending_handoffs(&conn, None)
        .map_err(|err| CliError::runtime("next", format!("pending_handoffs: {err}")))?;
    let active_blockers = graph::active_blockers(&conn, None)
        .map_err(|err| CliError::runtime("next", format!("active_blockers: {err}")))?;
    let owned_active_tasks = graph::active_tasks(&conn, None)
        .map_err(|err| CliError::runtime("next", format!("active_tasks: {err}")))?;
    let recent_artifacts = graph::recent_artifacts(&conn, 200)
        .map_err(|err| CliError::runtime("next", format!("recent_artifacts: {err}")))?;
    let unconsumed_rows = graph::unconsumed_artifacts(&conn, 200)
        .map_err(|err| CliError::runtime("next", format!("unconsumed: {err}")))?;
    let unconsumed_ids: HashSet<String> = unconsumed_rows
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let (owned_active_tasks, unowned_active_tasks) = owned_active_tasks
        .into_iter()
        .partition::<Vec<_>, _>(|t| t["owner_tool"].as_str().is_some());
    Ok(GraphView {
        profile,
        pending_handoffs,
        active_blockers,
        owned_active_tasks,
        unowned_active_tasks,
        recent_artifacts,
        unconsumed_ids,
    })
}

pub(super) fn execute_setup(command: SetupCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("setup")?;
    let mut status = setup_status(&store)?;
    let mut installed_path = None;
    let mut installed_files = Vec::<String>::new();
    let mut modified_external_config = None;
    let mut backup_paths = Vec::<String>::new();
    let mut verification = Vec::<Value>::new();
    match command.action.as_str() {
        "status" => {}
        "verify" => {
            verification = verify_setup(command.target.as_deref())?;
        }
        "enforcement" => {
            let mode = command.target.as_deref().ok_or_else(|| {
                CliError::usage("setup", "setup enforcement requires off, warn, or strict")
            })?;
            if !matches!(mode, "off" | "warn" | "strict") {
                return Err(CliError::usage(
                    "setup",
                    "enforcement must be off, warn, or strict",
                ));
            }
            if command.dry_run {
                installed_files.push(setup_config_path(&store).display().to_string());
            } else {
                backup_paths.extend(backup_file(&setup_config_path(&store))?);
                write_setup_config(&store, mode)?;
            }
            status = setup_status(&store)?;
        }
        "install" => {
            let adapter = command
                .target
                .as_deref()
                .ok_or_else(|| CliError::usage("setup", "setup install requires a tool id"))?;
            if !matches!(
                adapter,
                "pi" | "claude" | "codex" | "gemini" | "cmux" | "herdr"
            ) {
                return Err(CliError::usage(
                    "setup",
                    "unknown adapter; expected pi, claude, codex, gemini, cmux, or herdr",
                ));
            }
            let install = if command.dry_run {
                plan_install(&store, adapter)?
            } else if matches!(adapter, "cmux" | "herdr") {
                write_adapter_install(&store, adapter)?
            } else {
                write_tool_install(adapter)?
            };
            installed_path = install.primary_path;
            installed_files = install.files;
            modified_external_config = install.modified_config;
            backup_paths = install.backup_paths;
        }
        "uninstall" => {
            let adapter = command
                .target
                .as_deref()
                .ok_or_else(|| CliError::usage("setup", "setup uninstall requires a tool id"))?;
            let uninstall = if command.dry_run {
                plan_uninstall(adapter)?
            } else {
                uninstall_tool_or_adapter(adapter)?
            };
            installed_files = uninstall.files;
            modified_external_config = uninstall.modified_config;
            backup_paths = uninstall.backup_paths;
        }
        value => {
            return Err(CliError::usage(
                "setup",
                format!("unknown setup action {value}"),
            ));
        }
    }
    Ok(query_output(
        "setup",
        &command.common,
        &store,
        format!(
            "setup enforcement={} tools={}",
            status.enforcement,
            status.tools.len()
        ),
        json!({
            "setup": {
                "schema": "agent-rally.command.setup.v1",
                "enforcement": status.enforcement,
                "tools": status.tools,
                "startup": status.startup,
                "installed_path": installed_path,
                "installed_files": installed_files,
                "modified_external_config": modified_external_config,
                "backup_paths": backup_paths,
                "dry_run": command.dry_run,
                "verification": verification,
            }
        }),
    ))
}

pub(super) fn execute_judge(command: JudgeCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("judge")?;
    let judgment = build_judgment(
        &store,
        &command.common,
        JudgmentInput {
            command: "judge",
            phase: command.phase,
            path: command.path,
            session_id: command.session_id,
            auto_claim: command.auto_claim,
            fail_open: command.fail_open,
        },
    )?;
    Ok(WriteOutput {
        json: true,
        text: format!(
            "{} allow={} decision={}",
            judgment.severity, judgment.allow, judgment.decision
        ),
        body: json!({
            "ok": true,
            "command": "judge",
            "schema": "agent-rally.command.judge.v1",
            "channel": store.channel_dir().display().to_string(),
            "data": { "judgment": judgment.to_json() }
        }),
    })
}

pub(super) fn execute_hook(command: HookCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("hook")?;
    let phase = command.phase.clone();
    let judgment = build_judgment(
        &store,
        &command.common,
        JudgmentInput {
            command: "hook",
            phase: command.phase,
            path: command.path,
            session_id: command.session_id,
            auto_claim: command.auto_claim,
            fail_open: command.fail_open,
        },
    )?;
    Ok(WriteOutput {
        json: true,
        text: format!(
            "hook {phase} allow={} decision={}",
            judgment.allow, judgment.decision
        ),
        body: json!({
            "ok": true,
            "command": "hook",
            "schema": "agent-rally.command.hook.v1",
            "channel": store.channel_dir().display().to_string(),
            "data": {
                "hook": {
                    "phase": phase,
                    "allow": judgment.allow,
                    "agent_visible": judgment.agent_visible(),
                    "judgment": judgment.to_json()
                }
            }
        }),
    })
}

pub(super) fn execute_repair(command: RepairCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("repair")?;
    match command.action.as_str() {
        "checkpoint" | "checkpoint-invalid" => {
            let status = store.rebuild_checkpoint().map_err(|err| {
                CliError::runtime("repair", format!("failed to rebuild checkpoint: {err}"))
            })?;
            Ok(query_output(
                "repair",
                &command.common,
                &store,
                format!("checkpoint rebuilt records={}", status.records),
                json!({
                    "repair": {
                        "action": command.action,
                        "repaired": true,
                        "checkpoint": status
                    }
                }),
            ))
        }
        "profile" => {
            let tool = command.common.tool();
            if tool == "unknown" {
                return Err(CliError::usage("repair", "profile repair requires --tool"));
            }
            let event = EventBuilder::new(
                new_id("evt"),
                EventPayload::Profile(ProfilePayload {
                    tool: tool.clone(),
                    capabilities: Vec::new(),
                    role: None,
                    watch: Vec::new(),
                    current_task: None,
                    branch: None,
                    availability: Some("available".to_string()),
                    notes: Some("created by rally repair profile".to_string()),
                }),
                &tool,
                command.common.run_id(),
                new_id("thr"),
            )
            .model(command.common.model())
            .subject(format!("profile {tool}"))
            .time(now_rfc3339());
            let entry = store.append_typed(event).map_err(|err| {
                CliError::runtime("repair", format!("failed to append profile: {err}"))
            })?;
            Ok(query_output(
                "repair",
                &command.common,
                &store,
                format!("profile repaired tool={tool}"),
                json!({
                    "repair": {
                        "action": "profile",
                        "repaired": true,
                        "event_id": event_field(&entry, "id")
                    }
                }),
            ))
        }
        "doctor" => Ok(query_output(
            "repair",
            &command.common,
            &store,
            "run rally doctor to inspect repairable issues".to_string(),
            json!({
                "repair": {
                    "action": "doctor",
                    "repaired": false,
                    "next_commands": [
                        "rally doctor --tool <tool> --json",
                        "rally repair checkpoint --json",
                        "rally repair profile --tool <tool> --json"
                    ]
                }
            }),
        )),
        other => Err(CliError::usage(
            "repair",
            format!("unknown repair action {other}; expected checkpoint or profile"),
        )),
    }
}

pub(super) fn execute_ci_gate(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("ci:gate")?;
    let records = load_records_cached_or_empty(&store, "ci:gate")?;
    let now = now_epoch_seconds();
    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime("ci:gate", format!("open graph: {err}")))?;
    graph::catch_up(&mut conn, &records, &now_rfc3339())
        .map_err(|err| CliError::runtime("ci:gate", format!("graph catch_up: {err}")))?;
    let conflicts = graph::claim_conflicts_typed(&conn)
        .map_err(|err| CliError::runtime("ci:gate", format!("conflicts: {err}")))?;
    let blockers = graph::active_blockers_typed(&conn, None, now)
        .map_err(|err| CliError::runtime("ci:gate", format!("blockers: {err}")))?;
    let handoffs = graph::pending_handoffs_typed(&conn, None, now)
        .map_err(|err| CliError::runtime("ci:gate", format!("handoffs: {err}")))?;
    let checkpoint = store
        .checkpoint_status()
        .map_err(|err| CliError::runtime("ci:gate", format!("failed to read checkpoint: {err}")))?;
    let checkpoint_ok = !checkpoint.exists || checkpoint.valid;
    let failures = json!({
        "claim_conflicts": conflicts,
        "active_blockers": blockers,
        "pending_handoffs": handoffs,
        "checkpoint_valid": checkpoint_ok,
    });
    let pass = failures["claim_conflicts"]
        .as_array()
        .is_none_or(Vec::is_empty)
        && failures["active_blockers"]
            .as_array()
            .is_none_or(Vec::is_empty)
        && failures["pending_handoffs"]
            .as_array()
            .is_none_or(Vec::is_empty)
        && checkpoint_ok;
    if !pass {
        return Err(CliError::runtime(
            "ci:gate",
            format!("rally coordination gate failed: {failures}"),
        ));
    }
    Ok(query_output(
        "ci:gate",
        &command.common,
        &store,
        "rally ci gate passed".to_string(),
        json!({
            "gate": {
                "status": "pass",
                "checkpoint": checkpoint,
            }
        }),
    ))
}

struct JudgmentInput {
    command: &'static str,
    phase: String,
    path: Option<String>,
    session_id: Option<String>,
    auto_claim: bool,
    fail_open: bool,
}

struct JudgmentResult {
    allow: bool,
    fail_open_requested: bool,
    decision: String,
    severity: String,
    safe_to_write: bool,
    context_stale: bool,
    reasons: Vec<Value>,
    required_actions: Vec<String>,
    auto_claimed: Option<String>,
    resource: Option<String>,
    new_events_since_cursor: u64,
    pending_handoffs: Vec<Value>,
    claim_conflicts: Vec<Value>,
}

impl JudgmentResult {
    fn to_json(&self) -> Value {
        json!({
            "allow": self.allow,
            "fail_open_requested": self.fail_open_requested,
            "decision": self.decision,
            "severity": self.severity,
            "safe_to_write": self.safe_to_write,
            "context_stale": self.context_stale,
            "new_events_since_cursor": self.new_events_since_cursor,
            "resource": self.resource,
            "auto_claimed": self.auto_claimed,
            "reasons": self.reasons,
            "required_actions": self.required_actions,
            "agent_visible": self.agent_visible(),
            "pending_handoffs": self.pending_handoffs,
            "claim_conflicts": self.claim_conflicts,
        })
    }

    fn agent_visible(&self) -> Value {
        let source_event_ids = self
            .pending_handoffs
            .iter()
            .filter_map(|item| item.get("event_id").and_then(Value::as_str))
            .chain(
                self.reasons
                    .iter()
                    .filter_map(|item| item.get("event_id").and_then(Value::as_str)),
            )
            .map(str::to_string)
            .collect::<Vec<_>>();
        if let Some(handoff) = self.pending_handoffs.first() {
            let subject = handoff
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("pending handoff");
            let from = handoff
                .get("from_tool")
                .and_then(Value::as_str)
                .unwrap_or("another agent");
            return agent_visible(
                true,
                &self.severity,
                format!(
                    "Rally obligation: pending handoff from {from}. Subject: {subject}. Required action: inspect the source_event_ids and respond with ack, needs-info, or reject."
                ),
                Some(self.decision.as_str()),
                source_event_ids,
                false,
            );
        }
        if !self.reasons.is_empty() || self.context_stale {
            let message = self
                .reasons
                .first()
                .and_then(|reason| reason.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("new coordination events exist since this session cursor");
            return agent_visible(
                true,
                &self.severity,
                format!(
                    "Rally coordination notice: {message}. Decision: {}.",
                    self.decision
                ),
                Some(self.decision.as_str()),
                source_event_ids,
                self.allow,
            );
        }
        inactive_agent_visible()
    }
}

fn build_judgment(
    store: &ChannelStore,
    common: &CommonOptions,
    input: JudgmentInput,
) -> Result<JudgmentResult, CliError> {
    let tool = common.tool();
    let records = load_records_cached_or_empty(store, input.command)?;
    let now = now_epoch_seconds();
    let setup = setup_status(store)?;
    let mut reasons = Vec::new();
    let mut required_actions = Vec::new();
    let path_resource = input.path.as_ref().map(|path| {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normalize_file_resource(path, &cwd)
    });

    if setup.enforcement == "strict" && tool == "unknown" {
        reasons.push(
            json!({"code": "anonymous-tool", "message": "strict mode requires a stable --tool"}),
        );
        required_actions.push("rerun with --tool <stable-id>".to_string());
    }

    let checkpoint = store.checkpoint_status().map_err(|err| {
        CliError::runtime(input.command, format!("failed to read checkpoint: {err}"))
    })?;
    if checkpoint.exists && !checkpoint.valid {
        reasons.push(json!({
            "code": "checkpoint-invalid",
            "message": checkpoint.reason.as_deref().unwrap_or("checkpoint invalid")
        }));
        required_actions.push("rally repair checkpoint --json".to_string());
    }

    let mut conn = graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime(input.command, format!("open graph: {err}")))?;
    graph::catch_up(&mut conn, &records, &now_rfc3339())
        .map_err(|err| CliError::runtime(input.command, format!("graph catch_up: {err}")))?;
    let pending = graph::pending_handoffs_typed(&conn, Some(&tool), now)
        .map_err(|err| CliError::runtime(input.command, format!("handoffs: {err}")))?;
    let blockers = graph::active_blockers_typed(&conn, None, now)
        .map_err(|err| CliError::runtime(input.command, format!("blockers: {err}")))?;
    let all_claims = graph::active_claims_typed(&conn, None, now)
        .map_err(|err| CliError::runtime(input.command, format!("claims: {err}")))?;
    if !pending.is_empty() {
        reasons.push(json!({
            "code": "pending-handoff",
            "message": "required handoff is assigned to this tool",
            "event_id": pending[0].event_id
        }));
        required_actions.push(format!(
            "rally ack --tool {tool} <handoff-id> --summary <summary>"
        ));
    }

    for blocker in &blockers {
        if path_resource
            .as_ref()
            .is_none_or(|resource| blocker.resource.as_ref() == Some(resource))
        {
            reasons.push(json!({
                "code": "active-blocker",
                "message": blocker.subject,
                "event_id": blocker.event_id,
                "resource": blocker.resource
            }));
        }
    }
    if !blockers.is_empty() {
        required_actions
            .push("resolve or acknowledge active blockers before continuing".to_string());
    }

    let mut path_conflicts = Vec::new();
    let mut has_own_claim = false;
    if let Some(resource) = &path_resource {
        for claim in &all_claims {
            if &claim.resource != resource {
                continue;
            }
            if claim.owner_tool.as_deref() == Some(&tool) {
                has_own_claim = true;
            } else {
                path_conflicts.push(json!({
                    "event_id": claim.event_id,
                    "owner_tool": claim.owner_tool,
                    "resource": claim.resource,
                    "subject": claim.subject
                }));
            }
        }
        if !path_conflicts.is_empty() {
            reasons.push(json!({
                "code": "claim-conflict",
                "message": "another active claim owns this path",
                "resource": resource,
                "conflicts": path_conflicts
            }));
            required_actions.push("pause or coordinate with the claim owner".to_string());
        }
    }

    let mut auto_claimed = None;
    if matches!(input.phase.as_str(), "before-write" | "write")
        && input.auto_claim
        && path_resource.is_some()
        && path_conflicts.is_empty()
        && reasons.is_empty()
        && !has_own_claim
        && tool != "unknown"
    {
        let resource = path_resource.clone().unwrap();
        let event = EventBuilder::new(
            new_id("evt"),
            EventPayload::Claim(ClaimPayload {
                owner_tool: tool.clone(),
                resource: resource.clone(),
                subject: format!("auto-claim {resource}"),
                notes: Some("created by rally hook before-write --auto-claim".to_string()),
            }),
            &tool,
            common.run_id(),
            new_id("thr"),
        )
        .model(common.model())
        .subject(format!("auto-claim {resource}"))
        .time(now_rfc3339());
        let entry = store.append_typed(event).map_err(|err| {
            CliError::runtime(input.command, format!("failed to auto-claim path: {err}"))
        })?;
        auto_claimed = event_field(&entry, "id");
    }

    let max_seq = records
        .iter()
        .filter_map(|record| record.get("local_seq").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let cursor = input
        .session_id
        .as_ref()
        .map(|session_id| cursors::read_cursor(store.channel_dir(), &tool, session_id))
        .unwrap_or(max_seq);
    let new_events_since_cursor = max_seq.saturating_sub(cursor);
    let context_stale = input.session_id.is_some() && new_events_since_cursor > 0;
    if context_stale {
        reasons.push(json!({
            "code": "context-stale",
            "message": "new coordination events exist since this session cursor",
            "new_events_since_cursor": new_events_since_cursor
        }));
        required_actions.push(format!(
            "rally watch --tool {tool} --session-id <session> --since-cursor"
        ));
    }

    let stop_codes = [
        "anonymous-tool",
        "checkpoint-invalid",
        "active-blocker",
        "claim-conflict",
    ];
    let has_stop_reason = reasons.iter().any(|reason| {
        reason
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| stop_codes.contains(&code))
    });
    let pending_handoff = !pending.is_empty();
    let allow = !has_stop_reason && !pending_handoff;
    let decision = if has_stop_reason {
        "pause"
    } else if pending_handoff {
        "ack_handoff"
    } else if context_stale {
        "refresh_context"
    } else {
        "continue"
    };
    let severity = if !allow {
        "stop"
    } else if context_stale || !reasons.is_empty() {
        "warn"
    } else {
        "ok"
    };
    Ok(JudgmentResult {
        allow,
        fail_open_requested: input.fail_open,
        decision: decision.to_string(),
        severity: severity.to_string(),
        safe_to_write: allow && matches!(input.phase.as_str(), "before-write" | "write"),
        context_stale,
        reasons,
        required_actions,
        auto_claimed,
        resource: path_resource,
        new_events_since_cursor,
        pending_handoffs: pending.into_iter().map(|item| json!(item)).collect(),
        claim_conflicts: path_conflicts,
    })
}

fn next_recommendation(tool: &str, limit: usize, view: &GraphView) -> Value {
    let role = view
        .profile
        .as_ref()
        .and_then(|p| p.role.as_deref())
        .unwrap_or("general");
    let capabilities = view
        .profile
        .as_ref()
        .map(|p| p.capabilities.clone())
        .unwrap_or_default();
    let mut candidates = Vec::<Value>::new();

    // ── Pending handoffs ──────────────────────────────────────────────
    for handoff in view.pending_handoffs.iter() {
        if handoff["to_tool"].as_str() != Some(tool) {
            continue;
        }
        let event_id = handoff["event_id"].as_str().unwrap_or("").to_string();
        let subject = handoff["subject"].as_str().unwrap_or("").to_string();
        candidates.push(candidate(
            "pick_up_handoff",
            &event_id,
            &subject,
            100.0,
            json!({ "handoff": 100.0 }),
            vec![event_id.clone()],
            vec!["required handoff is addressed to this tool".to_string()],
        ));
    }

    // ── Active blockers ───────────────────────────────────────────────
    for blocker in view.active_blockers.iter() {
        let score = if blocker["tool"].as_str() == Some(tool) {
            90.0
        } else {
            70.0
        };
        let event_id = blocker["event_id"].as_str().unwrap_or("").to_string();
        let subject = blocker["subject"].as_str().unwrap_or("").to_string();
        candidates.push(candidate(
            "unblock_peer",
            &event_id,
            &subject,
            score,
            json!({ "blocker": score }),
            vec![event_id.clone()],
            vec!["active blocker is preventing progress".to_string()],
        ));
    }

    // ── Active tasks (owned + unowned) ────────────────────────────────
    for task in view.owned_active_tasks.iter() {
        if task["owner_tool"].as_str() != Some(tool) {
            continue;
        }
        let event_id = task["event_id"].as_str().unwrap_or("").to_string();
        let subject = task["subject"].as_str().unwrap_or("").to_string();
        candidates.push(candidate(
            "progress_owned_task",
            &event_id,
            &subject,
            80.0,
            json!({ "owned_task": 80.0, "role_match": role_match_bonus(role, &capabilities, "task") }),
            vec![event_id.clone()],
            vec!["open task is already owned by this tool".to_string()],
        ));
    }
    for task in view.unowned_active_tasks.iter() {
        let bonus = role_match_bonus(role, &capabilities, "task");
        let event_id = task["event_id"].as_str().unwrap_or("").to_string();
        let subject = task["subject"].as_str().unwrap_or("").to_string();
        candidates.push(candidate(
            "claim_task",
            &event_id,
            &subject,
            55.0 + bonus,
            json!({ "unowned_task": 55.0, "role_match": bonus }),
            vec![event_id.clone()],
            vec!["open task has no owner".to_string()],
        ));
    }

    // ── Unconsumed artifacts ──────────────────────────────────────────
    // Only artifacts with no later non-producer activity surface.
    for artifact in view.recent_artifacts.iter().take(limit.max(3)) {
        let event_id = artifact["event_id"].as_str().unwrap_or("").to_string();
        if !view.unconsumed_ids.contains(&event_id) {
            continue;
        }
        let subject = artifact["subject"].as_str().unwrap_or("").to_string();
        let bonus = role_match_bonus(role, &capabilities, "artifact");
        candidates.push(candidate(
            if bonus > 0.0 {
                "review_artifact"
            } else {
                "consume_artifact"
            },
            &event_id,
            &subject,
            35.0 + bonus,
            json!({ "artifact": 35.0, "role_match": bonus }),
            vec![event_id.clone()],
            vec!["no follow-up activity from another peer — needs review".to_string()],
        ));
    }
    candidates = normalize_candidates(candidates);
    if candidates.is_empty() {
        return json!({
            "action_kind": "idle",
            "target_event_id": null,
            "subject": "no actionable work",
            "reasoning": ["no pending handoffs, blockers, tasks, or artifacts scored above threshold"],
            "source_event_ids": [],
            "score": 0.0,
            "factors": {},
            "agent_visible": inactive_agent_visible(),
            "alternatives": []
        });
    }
    let top = candidates.remove(0);
    let alternatives = candidates.into_iter().take(3).collect::<Vec<_>>();
    let agent_visible = agent_visible_from_next(&top);
    json!({
        "action_kind": top["action_kind"],
        "target_event_id": top["target_event_id"],
        "subject": top["subject"],
        "reasoning": top["reasoning"],
        "source_event_ids": top["source_event_ids"],
        "score": top["score"],
        "factors": top["factors"],
        "agent_visible": agent_visible,
        "alternatives": alternatives,
    })
}

fn normalize_candidates(candidates: Vec<Value>) -> Vec<Value> {
    let mut by_target = BTreeMap::<String, Value>::new();
    for candidate in candidates {
        let key = candidate_key(&candidate);
        match by_target.remove(&key) {
            Some(existing) => {
                let merged = merge_duplicate_candidate(existing, candidate);
                by_target.insert(key, merged);
            }
            None => {
                by_target.insert(key, candidate);
            }
        }
    }

    let mut normalized = by_target.into_values().collect::<Vec<_>>();
    normalized.sort_by(|left, right| {
        candidate_score(right)
            .partial_cmp(&candidate_score(left))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    normalized
}

fn candidate_key(candidate: &Value) -> String {
    candidate
        .get("target_event_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                candidate
                    .get("action_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                candidate
                    .get("subject")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )
        })
}

fn merge_duplicate_candidate(left: Value, right: Value) -> Value {
    let (mut kept, duplicate) = if candidate_score(&right) > candidate_score(&left) {
        (right, left)
    } else {
        (left, right)
    };
    append_unique_strings(&mut kept, &duplicate, "source_event_ids");
    append_unique_strings(&mut kept, &duplicate, "reasoning");
    kept
}

fn candidate_score(candidate: &Value) -> f64 {
    candidate
        .get("score")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn append_unique_strings(target: &mut Value, source: &Value, field: &str) {
    let existing = target
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut values = existing
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();

    for value in source
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
    }

    target[field] = json!(values);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_candidates_keeps_highest_score_per_target() {
        let candidates = vec![
            candidate(
                "consume_artifact",
                "evt_same",
                "same work",
                35.0,
                json!({ "artifact": 35.0 }),
                vec!["evt_same".to_string()],
                vec!["recent artifact may need follow-up".to_string()],
            ),
            candidate(
                "progress_owned_task",
                "evt_same",
                "same work",
                80.0,
                json!({ "owned_task": 80.0 }),
                vec!["evt_same".to_string(), "evt_task".to_string()],
                vec!["open task is already owned by this tool".to_string()],
            ),
            candidate(
                "claim_task",
                "evt_other",
                "other work",
                55.0,
                json!({ "unowned_task": 55.0 }),
                vec!["evt_other".to_string()],
                vec!["open task has no owner".to_string()],
            ),
        ];

        let normalized = normalize_candidates(candidates);

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0]["target_event_id"], "evt_same");
        assert_eq!(normalized[0]["action_kind"], "progress_owned_task");
        assert_eq!(normalized[0]["score"], 80.0);
        assert_eq!(
            normalized[0]["source_event_ids"],
            json!(["evt_same", "evt_task"])
        );
        assert_eq!(
            normalized[0]["reasoning"],
            json!([
                "open task is already owned by this tool",
                "recent artifact may need follow-up"
            ])
        );
        assert_eq!(normalized[1]["target_event_id"], "evt_other");
    }
}

fn inactive_agent_visible() -> Value {
    agent_visible(false, "info", "", None, Vec::new(), true)
}

fn agent_visible(
    present: bool,
    severity: &str,
    message: impl Into<String>,
    required_action: Option<&str>,
    source_event_ids: Vec<String>,
    automation_allowed: bool,
) -> Value {
    json!({
        "present": present,
        "severity": severity,
        "message": message.into(),
        "required_action": required_action,
        "source_event_ids": source_event_ids,
        "automation_allowed": automation_allowed,
    })
}

fn agent_visible_from_next(next: &Value) -> Value {
    let action = next
        .get("action_kind")
        .and_then(Value::as_str)
        .unwrap_or("idle");
    if action == "idle" {
        return inactive_agent_visible();
    }
    let subject = next
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("recommended work");
    let source_event_ids = next
        .get("source_event_ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let severity = match action {
        "pick_up_handoff" | "unblock_peer" => "stop",
        "progress_owned_task" | "claim_task" => "warn",
        _ => "info",
    };
    agent_visible(
        true,
        severity,
        format!(
            "Rally recommendation: {action}. Subject: {subject}. Treat pending handoffs, blockers, and conflicts as obligations before unrelated work."
        ),
        Some(action),
        source_event_ids,
        !matches!(severity, "stop"),
    )
}

fn agent_visible_from_context(recommendation: &ContextRecommendation) -> Value {
    let action = recommendation.action.as_str();
    if action == "proceed_solo" {
        return inactive_agent_visible();
    }
    let severity = match action {
        "ack_handoff" | "resolve_blocker" | "resolve_claim_conflict" => "stop",
        "continue_claim" | "work_task" | "refresh_context" => "warn",
        _ => "info",
    };
    agent_visible(
        true,
        severity,
        format!("Rally recommendation: {action}. {}", recommendation.reason),
        Some(action),
        recommendation.source_event_ids.clone(),
        recommendation.trust.automation_allowed && !matches!(severity, "stop"),
    )
}

fn candidate(
    action_kind: &str,
    target_event_id: &str,
    subject: &str,
    score: f64,
    factors: Value,
    source_event_ids: Vec<String>,
    reasoning: Vec<String>,
) -> Value {
    json!({
        "action_kind": action_kind,
        "target_event_id": target_event_id,
        "subject": subject,
        "reasoning": reasoning,
        "source_event_ids": source_event_ids,
        "score": score,
        "factors": factors,
    })
}

fn role_match_bonus(role: &str, capabilities: &[String], kind: &str) -> f64 {
    let has_capability = |needle: &str| capabilities.iter().any(|value| value.contains(needle));
    match kind {
        "artifact" if role.contains("review") || has_capability("review") => 25.0,
        "task" if role.contains("builder") || has_capability("build") => 15.0,
        "task" if role.contains("architect") || has_capability("design") => 10.0,
        _ => 0.0,
    }
}

fn load_records_cached_or_empty(
    store: &ChannelStore,
    command: &'static str,
) -> Result<Vec<Value>, CliError> {
    if !store.changes_path().exists() {
        return Ok(Vec::new());
    }
    store
        .load_records_cached()
        .map_err(|err| CliError::runtime(command, format!("failed to load channel: {err}")))
}

fn start_warnings(
    tool: &str,
    preflight: &rally_core::preflight::PreflightEnvelope,
    checkpoint: &rally_core::store::CheckpointStatus,
) -> Vec<Value> {
    let mut warnings = Vec::new();
    if tool == "unknown" {
        warnings.push(json!({
            "code": "unknown-tool",
            "message": "tool id is unknown; use a stable harness id"
        }));
    }
    for claim in &preflight.active_claims {
        if claim.owner_tool.as_deref() == Some("unknown") {
            warnings.push(json!({
                "code": "anonymous-claim",
                "event_id": claim.event_id,
                "resource": claim.resource,
                "message": "active claim has owner_tool=unknown"
            }));
        }
    }
    if !checkpoint.valid {
        warnings.push(json!({
            "code": "stale-checkpoint",
            "message": checkpoint.reason.as_deref().unwrap_or("checkpoint is not valid")
        }));
    }
    warnings
}

#[derive(Clone, Debug)]
struct SetupStatus {
    enforcement: String,
    tools: Vec<Value>,
    startup: Vec<String>,
}

fn setup_status(store: &ChannelStore) -> Result<SetupStatus, CliError> {
    let enforcement = read_setup_config(store).unwrap_or_else(|| "warn".to_string());
    let known = ["pi", "claude", "codex", "gemini", "cursor", "cmux", "herdr"];
    let tools = known
        .iter()
        .map(|tool| {
            json!({
                "tool": tool,
                "available_on_path": executable_on_path(tool),
                "startup_command": if matches!(*tool, "cmux" | "herdr") {
                    format!("rally {tool} packet --tool <agent>")
                } else {
                    format!("rally {tool}")
                }
            })
        })
        .collect::<Vec<_>>();
    Ok(SetupStatus {
        enforcement,
        tools,
        startup: vec![
            "rally pi".to_string(),
            "rally claude".to_string(),
            "rally codex".to_string(),
            "rally start <custom-tool>".to_string(),
        ],
    })
}

fn read_setup_config(store: &ChannelStore) -> Option<String> {
    let path = setup_config_path(store);
    let text = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<Value>(&text).ok()?;
    value
        .get("enforcement")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn write_setup_config(store: &ChannelStore, enforcement: &str) -> Result<(), CliError> {
    let path = setup_config_path(store);
    fs::create_dir_all(path.parent().unwrap())
        .map_err(|err| CliError::runtime("setup", format!("failed to create config dir: {err}")))?;
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "schema": "agent-rally.setup.v1",
            "enforcement": enforcement,
        }))
        .unwrap(),
    )
    .map_err(|err| CliError::runtime("setup", format!("failed to write config: {err}")))
}

fn setup_config_path(store: &ChannelStore) -> PathBuf {
    store.channel_dir().join("rally/config.json")
}

struct AdapterInstall {
    primary_path: Option<String>,
    files: Vec<String>,
    modified_config: Option<String>,
    backup_paths: Vec<String>,
}

fn plan_install(store: &ChannelStore, tool: &str) -> Result<AdapterInstall, CliError> {
    let files = install_paths(store, tool)?;
    Ok(AdapterInstall {
        primary_path: files.first().cloned(),
        modified_config: modified_config_for(tool, &files),
        files,
        backup_paths: Vec::new(),
    })
}

fn plan_uninstall(tool: &str) -> Result<AdapterInstall, CliError> {
    let files = install_paths(&ChannelStore::new("."), tool)?
        .into_iter()
        .filter(|path| Path::new(path).exists())
        .collect::<Vec<_>>();
    Ok(AdapterInstall {
        primary_path: None,
        modified_config: modified_config_for(tool, &files),
        files,
        backup_paths: Vec::new(),
    })
}

fn install_paths(store: &ChannelStore, tool: &str) -> Result<Vec<String>, CliError> {
    Ok(match tool {
        "pi" => vec![
            pi_extensions_dir()?
                .join("rally-judgment.ts")
                .display()
                .to_string(),
        ],
        "claude" => {
            let dir = home_dot(".claude")?;
            vec![
                dir.join("hooks/rally-hook.sh").display().to_string(),
                dir.join("settings.json").display().to_string(),
            ]
        }
        "codex" => {
            let dir = codex_home()?;
            vec![
                dir.join("rally-hook.sh").display().to_string(),
                dir.join("hooks.json").display().to_string(),
                dir.join("config.toml").display().to_string(),
            ]
        }
        "gemini" => {
            let dir = home_dot(".gemini")?;
            vec![
                dir.join("rally-hook.sh").display().to_string(),
                dir.join("settings.json").display().to_string(),
            ]
        }
        "cmux" => {
            let dir = adapter_config_dir("RALLY_CMUX_CONFIG_DIR", "cmux")?;
            vec![
                store
                    .channel_dir()
                    .join("rally/adapters/cmux-rally-start.md")
                    .display()
                    .to_string(),
                dir.join("rally-agent-wrapper.sh").display().to_string(),
                dir.join("cmux.json").display().to_string(),
            ]
        }
        "herdr" => {
            let dir = adapter_config_dir("RALLY_HERDR_CONFIG_DIR", "herdr")?;
            vec![
                store
                    .channel_dir()
                    .join("rally/adapters/herdr-rally-start.md")
                    .display()
                    .to_string(),
                dir.join("integrations/rally-agent-start.sh")
                    .display()
                    .to_string(),
                dir.join("config.toml").display().to_string(),
            ]
        }
        _ => return Err(CliError::usage("setup", "unknown tool")),
    })
}

fn modified_config_for(tool: &str, files: &[String]) -> Option<String> {
    match tool {
        "claude" | "gemini" => files
            .iter()
            .find(|path| path.ends_with("settings.json"))
            .cloned(),
        "codex" => files
            .iter()
            .find(|path| path.ends_with("hooks.json"))
            .cloned(),
        "cmux" => files
            .iter()
            .find(|path| path.ends_with("cmux.json"))
            .cloned(),
        "herdr" => files
            .iter()
            .find(|path| path.ends_with("config.toml"))
            .cloned(),
        _ => None,
    }
}

fn verify_setup(target: Option<&str>) -> Result<Vec<Value>, CliError> {
    let tools = match target {
        Some(tool) => vec![tool],
        None => vec!["pi", "claude", "codex", "gemini", "cmux", "herdr"],
    };
    tools
        .into_iter()
        .map(verify_setup_tool)
        .collect::<Result<Vec<_>, _>>()
}

fn verify_setup_tool(tool: &str) -> Result<Value, CliError> {
    let files = install_paths(&ChannelStore::new("."), tool)?;
    let marker_ok = match tool {
        "pi" => file_contains(files.first(), "rally-pi-judgment-extension-marker"),
        "claude" | "codex" | "gemini" => {
            files
                .iter()
                .any(|path| path.ends_with("rally-hook.sh") && Path::new(path).exists())
                && files
                    .iter()
                    .any(|path| file_contains(Some(path), "rally-hook.sh"))
        }
        "cmux" => files
            .iter()
            .any(|path| file_contains(Some(path), "rally-agent")),
        "herdr" => files
            .iter()
            .any(|path| file_contains(Some(path), "integrations.rally")),
        _ => return Err(CliError::usage("setup", "unknown tool")),
    };
    Ok(json!({
        "tool": tool,
        "installed": marker_ok,
        "files": files.iter().map(|path| json!({
            "path": path,
            "exists": Path::new(path).exists()
        })).collect::<Vec<_>>()
    }))
}

fn file_contains(path: Option<&String>, needle: &str) -> bool {
    path.and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|text| text.contains(needle))
}

fn write_tool_install(tool: &str) -> Result<AdapterInstall, CliError> {
    match tool {
        "pi" => install_pi_extension(),
        "claude" => install_claude_hooks(),
        "codex" => install_codex_hooks(),
        "gemini" => install_gemini_hooks(),
        _ => Err(CliError::usage("setup", "unknown tool for install")),
    }
}

fn install_pi_extension() -> Result<AdapterInstall, CliError> {
    let dir = pi_extensions_dir()?;
    fs::create_dir_all(&dir).map_err(|err| {
        CliError::runtime("setup", format!("failed to create Pi extension dir: {err}"))
    })?;
    let path = dir.join("rally-judgment.ts");
    let backup_paths = backup_file(&path)?.into_iter().collect::<Vec<_>>();
    fs::write(&path, pi_rally_extension()).map_err(|err| {
        CliError::runtime("setup", format!("failed to write Pi extension: {err}"))
    })?;
    Ok(AdapterInstall {
        primary_path: Some(path.display().to_string()),
        files: vec![path.display().to_string()],
        modified_config: None,
        backup_paths,
    })
}

fn install_claude_hooks() -> Result<AdapterInstall, CliError> {
    let dir = home_dot(".claude")?;
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).map_err(|err| {
        CliError::runtime("setup", format!("failed to create Claude hooks dir: {err}"))
    })?;
    let script = hooks_dir.join("rally-hook.sh");
    let mut backup_paths = backup_file(&script)?.into_iter().collect::<Vec<_>>();
    write_executable(&script, &native_hook_script("claude"))?;
    let settings = dir.join("settings.json");
    backup_paths.extend(backup_file(&settings)?);
    let mut value = read_json_object_or_default(&settings)?;
    add_native_hook(
        &mut value,
        "SessionStart",
        Some("startup|clear"),
        &format!(
            "bash {} start claude",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    add_native_hook(
        &mut value,
        "UserPromptSubmit",
        None,
        &format!(
            "bash {} idle claude",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    add_native_hook(
        &mut value,
        "PreToolUse",
        Some("Write|Edit|NotebookEdit"),
        &format!(
            "bash {} before-write claude",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    add_native_hook(
        &mut value,
        "Stop",
        None,
        &format!(
            "bash {} after-write claude",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    write_json_pretty(&settings, &value)?;
    Ok(AdapterInstall {
        primary_path: Some(script.display().to_string()),
        files: vec![script.display().to_string(), settings.display().to_string()],
        modified_config: Some(settings.display().to_string()),
        backup_paths,
    })
}

fn install_codex_hooks() -> Result<AdapterInstall, CliError> {
    let dir = codex_home()?;
    fs::create_dir_all(&dir)
        .map_err(|err| CliError::runtime("setup", format!("failed to create Codex home: {err}")))?;
    let script = dir.join("rally-hook.sh");
    let mut backup_paths = backup_file(&script)?.into_iter().collect::<Vec<_>>();
    write_executable(&script, &native_hook_script("codex"))?;
    let hooks = dir.join("hooks.json");
    backup_paths.extend(backup_file(&hooks)?);
    let mut value = read_json_object_or_default(&hooks)?;
    add_native_hook(
        &mut value,
        "SessionStart",
        None,
        &format!(
            "bash {} start codex",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    add_native_hook(
        &mut value,
        "UserPromptSubmit",
        None,
        &format!(
            "bash {} idle codex",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    add_native_hook(
        &mut value,
        "PreToolUse",
        None,
        &format!(
            "bash {} before-write codex",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    add_native_hook(
        &mut value,
        "Stop",
        None,
        &format!(
            "bash {} after-write codex",
            shell_quote(&script.display().to_string())
        ),
        5000,
    );
    write_json_pretty(&hooks, &value)?;
    let config = dir.join("config.toml");
    backup_paths.extend(backup_file(&config)?);
    upsert_marked_block(
        &config,
        "# BEGIN rally codex hooks",
        "# END rally codex hooks",
        "# BEGIN rally codex hooks\n[features]\nhooks = true\n# END rally codex hooks\n",
    )?;
    Ok(AdapterInstall {
        primary_path: Some(hooks.display().to_string()),
        files: vec![
            script.display().to_string(),
            hooks.display().to_string(),
            config.display().to_string(),
        ],
        modified_config: Some(hooks.display().to_string()),
        backup_paths,
    })
}

fn install_gemini_hooks() -> Result<AdapterInstall, CliError> {
    let dir = home_dot(".gemini")?;
    fs::create_dir_all(&dir)
        .map_err(|err| CliError::runtime("setup", format!("failed to create Gemini dir: {err}")))?;
    let script = dir.join("rally-hook.sh");
    let mut backup_paths = backup_file(&script)?.into_iter().collect::<Vec<_>>();
    write_executable(&script, &native_hook_script("gemini"))?;
    let settings = dir.join("settings.json");
    backup_paths.extend(backup_file(&settings)?);
    let mut value = read_json_object_or_default(&settings)?;
    add_native_hook(
        &mut value,
        "SessionStart",
        None,
        &format!(
            "bash {} start gemini",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    add_native_hook(
        &mut value,
        "BeforeAgent",
        None,
        &format!(
            "bash {} idle gemini",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    add_native_hook(
        &mut value,
        "BeforeTool",
        None,
        &format!(
            "bash {} before-write gemini",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    add_native_hook(
        &mut value,
        "AfterAgent",
        None,
        &format!(
            "bash {} after-write gemini",
            shell_quote(&script.display().to_string())
        ),
        10000,
    );
    write_json_pretty(&settings, &value)?;
    Ok(AdapterInstall {
        primary_path: Some(settings.display().to_string()),
        files: vec![script.display().to_string(), settings.display().to_string()],
        modified_config: Some(settings.display().to_string()),
        backup_paths,
    })
}

fn uninstall_tool_or_adapter(tool: &str) -> Result<AdapterInstall, CliError> {
    let mut files = Vec::new();
    let mut modified_config = None;
    let mut backup_paths = Vec::new();
    if tool == "pi" {
        let path = pi_extensions_dir()?.join("rally-judgment.ts");
        if path.exists() {
            backup_paths.extend(backup_file(&path)?);
            fs::remove_file(&path).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove Pi extension: {err}"))
            })?;
        }
        files.push(path.display().to_string());
    } else if tool == "claude" {
        let dir = home_dot(".claude")?;
        let script = dir.join("hooks/rally-hook.sh");
        if script.exists() {
            backup_paths.extend(backup_file(&script)?);
            fs::remove_file(&script).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove Claude hook: {err}"))
            })?;
        }
        files.push(script.display().to_string());
        let settings = dir.join("settings.json");
        if settings.exists() {
            backup_paths.extend(backup_file(&settings)?);
            let mut value = read_json_object_or_default(&settings)?;
            remove_rally_native_hooks(&mut value);
            write_json_pretty(&settings, &value)?;
            modified_config = Some(settings.display().to_string());
        }
    } else if tool == "codex" {
        let dir = codex_home()?;
        let script = dir.join("rally-hook.sh");
        if script.exists() {
            backup_paths.extend(backup_file(&script)?);
            fs::remove_file(&script).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove Codex hook: {err}"))
            })?;
        }
        files.push(script.display().to_string());
        let hooks = dir.join("hooks.json");
        if hooks.exists() {
            backup_paths.extend(backup_file(&hooks)?);
            let mut value = read_json_object_or_default(&hooks)?;
            remove_rally_native_hooks(&mut value);
            write_json_pretty(&hooks, &value)?;
            modified_config = Some(hooks.display().to_string());
        }
        let config = dir.join("config.toml");
        if config.exists() {
            backup_paths.extend(backup_file(&config)?);
            let existing = fs::read_to_string(&config).map_err(|err| {
                CliError::runtime("setup", format!("failed to read Codex config: {err}"))
            })?;
            let next = remove_marked_block(
                &existing,
                "# BEGIN rally codex hooks",
                "# END rally codex hooks",
            );
            fs::write(&config, next).map_err(|err| {
                CliError::runtime("setup", format!("failed to write Codex config: {err}"))
            })?;
        }
    } else if tool == "gemini" {
        let dir = home_dot(".gemini")?;
        let script = dir.join("rally-hook.sh");
        if script.exists() {
            backup_paths.extend(backup_file(&script)?);
            fs::remove_file(&script).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove Gemini hook: {err}"))
            })?;
        }
        files.push(script.display().to_string());
        let settings = dir.join("settings.json");
        if settings.exists() {
            backup_paths.extend(backup_file(&settings)?);
            let mut value = read_json_object_or_default(&settings)?;
            remove_rally_native_hooks(&mut value);
            write_json_pretty(&settings, &value)?;
            modified_config = Some(settings.display().to_string());
        }
    } else if tool == "cmux" {
        let dir = adapter_config_dir("RALLY_CMUX_CONFIG_DIR", "cmux")?;
        let wrapper = dir.join("rally-agent-wrapper.sh");
        if wrapper.exists() {
            backup_paths.extend(backup_file(&wrapper)?);
            fs::remove_file(&wrapper).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove cmux wrapper: {err}"))
            })?;
        }
        files.push(wrapper.display().to_string());
        let config = dir.join("cmux.json");
        if config.exists() {
            backup_paths.extend(backup_file(&config)?);
            let text = fs::read_to_string(&config).map_err(|err| {
                CliError::runtime("setup", format!("failed to read cmux config: {err}"))
            })?;
            let mut value: Value = serde_json::from_str(&text).map_err(|err| {
                CliError::runtime("setup", format!("failed to parse cmux config: {err}"))
            })?;
            if let Some(commands) = value
                .as_object_mut()
                .and_then(|object| object.get_mut("commands"))
                .and_then(Value::as_array_mut)
            {
                commands.retain(|command| {
                    command.get("name").and_then(Value::as_str) != Some("rally-agent")
                });
                fs::write(&config, serde_json::to_vec_pretty(&value).unwrap()).map_err(|err| {
                    CliError::runtime("setup", format!("failed to write cmux config: {err}"))
                })?;
                modified_config = Some(config.display().to_string());
            }
        }
    } else if tool == "herdr" {
        let dir = adapter_config_dir("RALLY_HERDR_CONFIG_DIR", "herdr")?;
        let wrapper = dir.join("integrations/rally-agent-start.sh");
        if wrapper.exists() {
            backup_paths.extend(backup_file(&wrapper)?);
            fs::remove_file(&wrapper).map_err(|err| {
                CliError::runtime("setup", format!("failed to remove herdr wrapper: {err}"))
            })?;
        }
        files.push(wrapper.display().to_string());
        let config = dir.join("config.toml");
        if config.exists() {
            backup_paths.extend(backup_file(&config)?);
            let existing = fs::read_to_string(&config).map_err(|err| {
                CliError::runtime("setup", format!("failed to read herdr config: {err}"))
            })?;
            let next = remove_marked_block(
                &existing,
                "# BEGIN rally integration",
                "# END rally integration",
            );
            fs::write(&config, next).map_err(|err| {
                CliError::runtime("setup", format!("failed to write herdr config: {err}"))
            })?;
            modified_config = Some(config.display().to_string());
        }
    } else {
        return Err(CliError::usage("setup", "unknown tool for uninstall"));
    }
    Ok(AdapterInstall {
        primary_path: None,
        files,
        modified_config,
        backup_paths,
    })
}

fn home_dot(name: &str) -> Result<PathBuf, CliError> {
    let home = env::var("HOME")
        .map_err(|_| CliError::runtime("setup", format!("HOME is required for {name}")))?;
    Ok(PathBuf::from(home).join(name))
}

fn pi_extensions_dir() -> Result<PathBuf, CliError> {
    if let Ok(value) = env::var("PI_CODING_AGENT_DIR") {
        return Ok(PathBuf::from(value).join("extensions"));
    }
    Ok(home_dot(".pi")?.join("agent/extensions"))
}

fn codex_home() -> Result<PathBuf, CliError> {
    if let Ok(value) = env::var("CODEX_HOME") {
        return Ok(PathBuf::from(value));
    }
    home_dot(".codex")
}

fn read_json_object_or_default(path: &Path) -> Result<Value, CliError> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path).map_err(|err| {
        CliError::runtime("setup", format!("failed to read {}: {err}", path.display()))
    })?;
    serde_json::from_str::<Value>(&text).map_err(|err| {
        CliError::runtime(
            "setup",
            format!("failed to parse {}: {err}", path.display()),
        )
    })
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            CliError::runtime(
                "setup",
                format!("failed to create {}: {err}", parent.display()),
            )
        })?;
    }
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).map_err(|err| {
        CliError::runtime(
            "setup",
            format!("failed to write {}: {err}", path.display()),
        )
    })
}

fn backup_file(path: &Path) -> Result<Option<String>, CliError> {
    if !path.exists() {
        return Ok(None);
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let backup = path.with_file_name(format!("{file_name}.rally.bak"));
    fs::copy(path, &backup).map_err(|err| {
        CliError::runtime(
            "setup",
            format!("failed to back up {}: {err}", path.display()),
        )
    })?;
    Ok(Some(backup.display().to_string()))
}

fn add_native_hook(
    value: &mut Value,
    event: &str,
    matcher: Option<&str>,
    command: &str,
    timeout: u64,
) {
    if !value.is_object() {
        *value = json!({});
    }
    let object = value.as_object_mut().unwrap();
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let event_hooks = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    if !event_hooks.is_array() {
        *event_hooks = json!([]);
    }
    let entries = event_hooks.as_array_mut().unwrap();
    if entries
        .iter()
        .any(|entry| entry.to_string().contains(command))
    {
        return;
    }
    let mut entry = json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": timeout
        }]
    });
    if let Some(matcher) = matcher {
        entry["matcher"] = Value::String(matcher.to_string());
    }
    entries.push(entry);
}

fn remove_rally_native_hooks(value: &mut Value) {
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for entries in hooks.values_mut() {
        let Some(array) = entries.as_array_mut() else {
            continue;
        };
        array.retain(|entry| !entry.to_string().contains("rally-hook.sh"));
    }
}

fn upsert_marked_block(path: &Path, begin: &str, end: &str, block: &str) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            CliError::runtime(
                "setup",
                format!("failed to create {}: {err}", parent.display()),
            )
        })?;
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    let next = replace_marked_block(&existing, begin, end, block);
    fs::write(path, next).map_err(|err| {
        CliError::runtime(
            "setup",
            format!("failed to write {}: {err}", path.display()),
        )
    })
}

fn native_hook_script(default_tool: &str) -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail
phase="${1:-idle}"
tool="${2:-DEFAULT_TOOL}"
input="$(cat || true)"
meta="$({ printf '%s' "$input" | node -e '
let data=""; process.stdin.on("data", c => data += c); process.stdin.on("end", () => {
  try {
    const value = JSON.parse(data || "{}");
    const toolInput = value.tool_input || value.toolInput || value.input || value;
    const path = toolInput.file_path || toolInput.filePath || toolInput.path || toolInput.notebook_path || "";
    const session = value.session_id || value.sessionId || "";
    process.stdout.write(JSON.stringify({path, session}));
  } catch (_) { process.stdout.write("{}"); }
});
' ; } 2>/dev/null)"
path="$({ printf '%s' "$meta" | node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(0,"utf8")||"{}"); process.stdout.write(v.path||""); } catch (_) {}' ; } 2>/dev/null)"
session="$({ printf '%s' "$meta" | node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(0,"utf8")||"{}"); process.stdout.write(v.session||""); } catch (_) {}' ; } 2>/dev/null)"
if [ -z "$session" ]; then session="${RALLY_SESSION_ID:-${tool}-$(date +%s)}"; fi
if [ "$phase" = "start" ]; then
  args=(start "$tool" --session-id "$session" --json)
else
  args=(hook "$phase" --tool "$tool" --session-id "$session" --json --fail-open)
  if [ -n "$path" ]; then
    args+=(--path "$path")
    if [ "$phase" = "before-write" ]; then args+=(--auto-claim); fi
  fi
fi
rally_output="$(rally "${args[@]}" 2>/dev/null || true)"
printf '%s' "$rally_output" | node -e '
const fs = require("fs");
const raw = fs.readFileSync(0, "utf8");
const phase = process.argv[1] || "idle";
const tool = process.argv[2] || "DEFAULT_TOOL";
function nativeEvent(tool, phase) {
  if (tool === "gemini") return {start:"SessionStart", idle:"BeforeAgent", "before-write":"BeforeTool", "after-write":"AfterAgent"}[phase] || "BeforeAgent";
  return {start:"SessionStart", idle:"UserPromptSubmit", "before-write":"PreToolUse", "after-write":"Stop"}[phase] || "UserPromptSubmit";
}
function output(value) { process.stdout.write(JSON.stringify(value)); }
let parsed = {};
try { parsed = JSON.parse(raw || "{}"); } catch (_) { output({}); process.exit(0); }
const hook = parsed?.data?.hook || {};
const judgment = hook?.judgment || parsed?.data?.judgment || {};
const visible = hook?.agent_visible || judgment?.agent_visible || parsed?.agent_visible || parsed?.data?.next?.agent_visible || {};
if (!visible.present) { output({}); process.exit(0); }
const event = nativeEvent(tool, phase);
const message = visible.message || "Rally has a pending coordination obligation.";
const severity = visible.severity || "warn";
const allow = hook.allow ?? judgment.allow ?? true;
const stop = severity === "stop" || allow === false;
if (tool === "gemini") {
  if (event === "SessionStart" || event === "BeforeAgent") {
    output({hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "BeforeTool") {
    output(stop ? {decision: "deny", reason: message} : {hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "AfterAgent") {
    output(stop ? {decision: "deny", reason: message} : {systemMessage: message});
  } else {
    output({systemMessage: message});
  }
} else {
  if (event === "SessionStart" || event === "UserPromptSubmit") {
    output({hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "PreToolUse") {
    output(stop ? {hookSpecificOutput: {hookEventName: event, permissionDecision: "deny", permissionDecisionReason: message}} : {hookSpecificOutput: {hookEventName: event, additionalContext: message}});
  } else if (event === "Stop") {
    output(stop ? {decision: "block", reason: message} : {systemMessage: message});
  } else {
    output({systemMessage: message});
  }
}
' "$phase" "$tool"
"#
    .replace("DEFAULT_TOOL", default_tool)
}

fn pi_rally_extension() -> &'static str {
    r#"// rally-pi-judgment-extension-marker v1
// Installed by `rally setup install pi`. DO NOT EDIT MANUALLY.
import { spawnSync } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

function runRally(args: string[], input?: unknown): string {
  const result = spawnSync("rally", args, {
    input: input === undefined ? undefined : JSON.stringify(input),
    encoding: "utf8",
    stdio: [input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
    timeout: 10000,
  });
  return result.stdout || result.stderr || "";
}

function pathFromTool(event: any): string | undefined {
  const input = event?.input || {};
  return input.path || input.file_path || input.filePath || input.notebook_path;
}

function agentVisible(output: string): { key: string; message: string } | undefined {
  try {
    const parsed = JSON.parse(output || "{}");
    const visible = parsed?.agent_visible || parsed?.data?.hook?.agent_visible || parsed?.data?.hook?.judgment?.agent_visible || parsed?.data?.next?.agent_visible;
    if (!visible?.present || !visible?.message) return undefined;
    const ids = Array.isArray(visible.source_event_ids) ? visible.source_event_ids.join(",") : "";
    return { key: `${visible.required_action || "notice"}:${ids}:${visible.message}`, message: visible.message };
  } catch (_) {
    return undefined;
  }
}

export default function rallyJudgment(pi: ExtensionAPI) {
  let lastDelivered = "";

  function deliver(output: string, triggerTurn: boolean) {
    const visible = agentVisible(output);
    if (!visible || visible.key === lastDelivered) return undefined;
    lastDelivered = visible.key;
    const message = {
      customType: "rally",
      content: visible.message,
      display: true,
    };
    if (triggerTurn) {
      try { pi.sendMessage(message, { triggerTurn: true, deliverAs: "followUp" }); } catch (_) {}
    }
    return message;
  }

  pi.on("session_start", async (_event, ctx) => {
    const session = ctx.sessionManager.getSessionId?.() || `${Date.now()}`;
    const output = runRally(["start", "pi", "--session-id", session, "--json"]);
    deliver(output, true);
  });

  pi.on("before_agent_start", async (_event, _ctx) => {
    const output = runRally(["hook", "idle", "--tool", "pi", "--json", "--fail-open"]);
    const message = deliver(output, false);
    if (message) return { message };
  });

  pi.on("tool_call", async (event) => {
    const name = event.toolName;
    if (!["write", "edit", "serena_replace_content", "serena_replace_symbol_body"].includes(name)) return;
    const path = pathFromTool(event);
    const args = ["hook", "before-write", "--tool", "pi", "--json"];
    if (path) args.push("--path", path, "--auto-claim");
    const output = runRally(args, event);
    try {
      const parsed = JSON.parse(output);
      const hook = parsed?.data?.hook || parsed?.data?.judgment || parsed;
      if (hook?.allow === false || hook?.judgment?.allow === false) {
        const visible = agentVisible(output);
        return { block: true, reason: visible?.message || `Rally blocked write: ${output}` };
      }
    } catch (_) {}
  });
}
"#
}

fn write_adapter_install(store: &ChannelStore, adapter: &str) -> Result<AdapterInstall, CliError> {
    let dir = store.channel_dir().join("rally/adapters");
    fs::create_dir_all(&dir).map_err(|err| {
        CliError::runtime("setup", format!("failed to create adapter dir: {err}"))
    })?;
    let path = dir.join(format!("{adapter}-rally-start.md"));
    let body = match adapter {
        "cmux" => {
            "# cmux Rally integration\n\nRun `rally <tool>` when an agent pane starts. Use `rally cmux packet --tool <tool>` for feed/workspace payloads. cmux should surface Rally obligations through operator-visible notifications/sidebar by default. Sending text into an agent pane is explicit opt-in only.\n"
        }
        "herdr" => {
            "# Herdr Rally integration\n\nRun `rally <tool>` when an agent pane starts. Use `rally herdr packet --tool <tool>` and honor `rally herdr inject` gates. Herdr should report Rally obligations as pane/agent state by default. Sending text into an agent terminal is explicit opt-in only.\n"
        }
        _ => unreachable!(),
    };
    let mut backup_paths = backup_file(&path)?.into_iter().collect::<Vec<_>>();
    fs::write(&path, body).map_err(|err| {
        CliError::runtime("setup", format!("failed to write adapter file: {err}"))
    })?;
    let mut files = vec![path.display().to_string()];
    let modified_config = match adapter {
        "cmux" => install_cmux_adapter(&mut files, &mut backup_paths)?,
        "herdr" => install_herdr_adapter(&mut files, &mut backup_paths)?,
        _ => unreachable!(),
    };
    Ok(AdapterInstall {
        primary_path: Some(path.display().to_string()),
        files,
        modified_config,
        backup_paths,
    })
}

fn install_cmux_adapter(
    files: &mut Vec<String>,
    backup_paths: &mut Vec<String>,
) -> Result<Option<String>, CliError> {
    let dir = adapter_config_dir("RALLY_CMUX_CONFIG_DIR", "cmux")?;
    fs::create_dir_all(&dir)
        .map_err(|err| CliError::runtime("setup", format!("failed to create cmux dir: {err}")))?;
    let wrapper = dir.join("rally-agent-wrapper.sh");
    backup_paths.extend(backup_file(&wrapper)?);
    write_executable(
        &wrapper,
        r#"#!/usr/bin/env bash
set -euo pipefail
tool="${1:-claude}"
if [ "$#" -gt 0 ]; then shift; fi
session="${RALLY_SESSION_ID:-${tool}-$(date +%s)}"
packet="${TMPDIR:-/tmp}/rally-${tool}-${session}.json"
rally start "$tool" --session-id "$session" --json > "$packet" || true
message="$(node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); const m=v.agent_visible?.present ? v.agent_visible.message : ""; process.stdout.write(m || ""); } catch (_) {}' "$packet")"
if [ -n "$message" ] && command -v cmux >/dev/null 2>&1; then
  cmux notify --title "Rally" --subtitle "$tool" --body "$message" >/dev/null 2>&1 || true
  cmux set-status rally "blocked" --icon flag --color "ff9500" --priority 90 >/dev/null 2>&1 || true
fi
if [ "${RALLY_INJECT_AGENT_INPUT:-0}" = "1" ] && [ -n "$message" ]; then
  printf '%s\n' "$message" > "${TMPDIR:-/tmp}/rally-${tool}-${session}.agent-message.txt"
fi
exec "$tool" "$@"
"#,
    )?;
    files.push(wrapper.display().to_string());

    let config = dir.join("cmux.json");
    backup_paths.extend(backup_file(&config)?);
    let mut value = if config.exists() {
        let text = fs::read_to_string(&config).map_err(|err| {
            CliError::runtime("setup", format!("failed to read cmux config: {err}"))
        })?;
        serde_json::from_str::<Value>(&text).map_err(|err| {
            CliError::runtime("setup", format!("failed to parse cmux config: {err}"))
        })?
    } else {
        json!({ "commands": [] })
    };
    let commands = value
        .as_object_mut()
        .and_then(|object| object.get_mut("commands"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CliError::runtime("setup", "cmux config must contain commands array"))?;
    if !commands
        .iter()
        .any(|command| command.get("name").and_then(Value::as_str) == Some("rally-agent"))
    {
        commands.push(json!({
            "name": "rally-agent",
            "workspace": {
                "cwd": env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| ".".to_string()),
                "layout": {
                    "pane": {
                        "surfaces": [
                            {
                                "type": "terminal",
                                "name": "agent",
                                "command": format!("{} claude", shell_quote(&wrapper.display().to_string()))
                            }
                        ]
                    }
                }
            }
        }));
        fs::write(&config, serde_json::to_vec_pretty(&value).unwrap()).map_err(|err| {
            CliError::runtime("setup", format!("failed to write cmux config: {err}"))
        })?;
    }
    files.push(config.display().to_string());
    Ok(Some(config.display().to_string()))
}

fn install_herdr_adapter(
    files: &mut Vec<String>,
    backup_paths: &mut Vec<String>,
) -> Result<Option<String>, CliError> {
    let dir = adapter_config_dir("RALLY_HERDR_CONFIG_DIR", "herdr")?;
    let integration_dir = dir.join("integrations");
    fs::create_dir_all(&integration_dir).map_err(|err| {
        CliError::runtime(
            "setup",
            format!("failed to create herdr integration dir: {err}"),
        )
    })?;
    let wrapper = integration_dir.join("rally-agent-start.sh");
    backup_paths.extend(backup_file(&wrapper)?);
    write_executable(
        &wrapper,
        r#"#!/usr/bin/env bash
set -euo pipefail
tool="${1:-claude}"
if [ "$#" -gt 0 ]; then shift; fi
session="${RALLY_SESSION_ID:-${tool}-$(date +%s)}"
packet="${TMPDIR:-/tmp}/rally-${tool}-${session}.json"
rally start "$tool" --session-id "$session" --json > "$packet" || true
message="$(node -e 'const fs=require("fs"); try { const v=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); const m=v.agent_visible?.present ? v.agent_visible.message : ""; process.stdout.write(m || ""); } catch (_) {}' "$packet")"
if [ -n "$message" ] && command -v herdr >/dev/null 2>&1 && [ -n "${HERDR_PANE_ID:-}" ]; then
  herdr pane report-agent "$HERDR_PANE_ID" --source rally --agent "$tool" --state blocked --message "$message" >/dev/null 2>&1 || true
fi
if [ "${RALLY_INJECT_AGENT_INPUT:-0}" = "1" ] && [ -n "$message" ]; then
  printf '%s\n' "$message" > "${TMPDIR:-/tmp}/rally-${tool}-${session}.agent-message.txt"
fi
exec "$tool" "$@"
"#,
    )?;
    files.push(wrapper.display().to_string());

    let config = dir.join("config.toml");
    backup_paths.extend(backup_file(&config)?);
    let marker_begin = "# BEGIN rally integration";
    let marker_end = "# END rally integration";
    let block = format!(
        "{marker_begin}\n[integrations.rally]\nstartup_wrapper = \"{}\"\npacket_command = \"rally herdr packet --tool <tool> --json\"\ninject_command = \"rally herdr inject <event-id> --json\"\n{marker_end}\n",
        toml_escape(&wrapper.display().to_string())
    );
    let existing = fs::read_to_string(&config).unwrap_or_default();
    let next = replace_marked_block(&existing, marker_begin, marker_end, &block);
    fs::write(&config, next).map_err(|err| {
        CliError::runtime("setup", format!("failed to write herdr config: {err}"))
    })?;
    files.push(config.display().to_string());
    Ok(Some(config.display().to_string()))
}

fn adapter_config_dir(env_key: &str, app: &str) -> Result<PathBuf, CliError> {
    if let Ok(value) = env::var(env_key) {
        return Ok(PathBuf::from(value));
    }
    if let Ok(value) = env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(value).join(app));
    }
    let home = env::var("HOME").map_err(|_| {
        CliError::runtime(
            "setup",
            format!("HOME is required to install {app} integration"),
        )
    })?;
    Ok(PathBuf::from(home).join(".config").join(app))
}

fn write_executable(path: &Path, body: &str) -> Result<(), CliError> {
    fs::write(path, body)
        .map_err(|err| CliError::runtime("setup", format!("failed to write script: {err}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|err| CliError::runtime("setup", format!("failed to stat script: {err}")))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .map_err(|err| CliError::runtime("setup", format!("failed to chmod script: {err}")))?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn replace_marked_block(existing: &str, begin: &str, end: &str, block: &str) -> String {
    let Some(start) = existing.find(begin) else {
        let separator = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        return format!("{existing}{separator}{block}");
    };
    let Some(relative_end) = existing[start..].find(end) else {
        return format!("{existing}\n{block}");
    };
    let end_index = start + relative_end + end.len();
    let trailing_newline = existing[end_index..]
        .strip_prefix('\n')
        .unwrap_or(&existing[end_index..]);
    format!("{}{}{}", &existing[..start], block, trailing_newline)
}

fn remove_marked_block(existing: &str, begin: &str, end: &str) -> String {
    let Some(start) = existing.find(begin) else {
        return existing.to_string();
    };
    let Some(relative_end) = existing[start..].find(end) else {
        return existing.to_string();
    };
    let end_index = start + relative_end + end.len();
    let trailing = existing[end_index..]
        .strip_prefix('\n')
        .unwrap_or(&existing[end_index..]);
    format!("{}{}", &existing[..start], trailing)
}

fn enforcement_severity(enforcement: &str) -> &'static str {
    match enforcement {
        "strict" => "P1",
        "warn" => "P2",
        _ => "P3",
    }
}

pub(super) fn execute_checkpoint_status(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store(command.command)?;
    let status = store.checkpoint_status().map_err(|err| {
        CliError::runtime(
            command.command,
            format!("failed to read checkpoint status: {err}"),
        )
    })?;
    let text = if status.valid {
        format!("checkpoint valid records={}", status.records)
    } else {
        format!(
            "checkpoint invalid: {}",
            status.reason.as_deref().unwrap_or("unknown reason")
        )
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({ "checkpoint": status }),
    ))
}

pub(super) fn execute_checkpoint_rebuild(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store(command.command)?;
    let status = store.rebuild_checkpoint().map_err(|err| {
        CliError::runtime(
            command.command,
            format!("failed to rebuild checkpoint: {err}"),
        )
    })?;
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        format!("checkpoint rebuilt records={}", status.records),
        json!({ "checkpoint": status }),
    ))
}

pub(super) fn execute_adapter_contract(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store(command.command)?;
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        "adapter contract v1".to_string(),
        json!({
            "contract": {
                "schema": "agent-rally.adapter.contract.v1",
                "consumes": [
                    "agent-rally.command.context.v1",
                    "agent-rally.command.packet.v1",
                    "agent-rally.command.herdr-inject.v1"
                ],
                "trust_rules": {
                    "must_honor_ready_to_inject_false": true,
                    "override_requires_explicit_operator_action": true,
                    "automation_field": "recommended_next_action.trust.automation_allowed",
                    "minimum_trust_field": "recommended_next_action.trust.required"
                },
                "stable_fields": [
                    "command",
                    "schema",
                    "agent_visible",
                    "data.packet.tool",
                    "data.packet.role",
                    "data.packet.packet_kind",
                    "data.packet.recommended_next_action",
                    "data.packet.trust_summary",
                    "data.packet.source_event_ids",
                    "data.packet.focus",
                    "data.gate.ready_to_inject",
                    "data.gate.trust"
                ],
                "adapters": {
                    "cmux": {
                        "command": "rally cmux packet --tool <tool> --json",
                        "side_effects": false,
                        "purpose": "workspace/feed-friendly packet export",
                        "operator_visibility_default": "cmux notify/sidebar status",
                        "agent_input_injection": "explicit opt-in only"
                    },
                    "herdr": {
                        "command": "rally herdr packet --tool <tool> --json",
                        "side_effects": false,
                        "purpose": "prompt payload export; actual injection remains adapter-owned",
                        "operator_visibility_default": "pane.report_agent / sidebar state",
                        "agent_input_injection": "explicit opt-in only"
                    }
                }
            }
        }),
    ))
}

pub(super) fn execute_cmux_packet(command: ReadCommand) -> Result<WriteOutput, CliError> {
    execute_adapter_packet(command, "cmux")
}

pub(super) fn execute_herdr_packet(command: ReadCommand) -> Result<WriteOutput, CliError> {
    execute_adapter_packet(command, "herdr")
}

fn execute_adapter_packet(
    command: ReadCommand,
    adapter: &'static str,
) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    let brief = brief_from_graph(&store, &records, &tool, command.limit, now)?;
    let packet = build_work_packet(&brief, command.limit);
    let agent_visible = agent_visible_from_context(&packet.recommended_next_action);
    let ready_to_act = packet.recommended_next_action.trust.automation_allowed;
    let title = format!("Rally {} packet for {}", packet.packet_kind, packet.tool);
    let body = adapter_body(
        &packet.packet_kind,
        &packet.role,
        &packet.recommended_next_action.reason,
        &packet.files,
    );
    let adapter_data = match adapter {
        "cmux" => json!({
            "adapter": "cmux",
            "schema": "agent-rally.adapter.cmux-packet.v1",
            "available_on_path": executable_on_path("cmux"),
            "side_effects": false,
            "suggested_commands": ["cmux feed tui", "cmux open ."],
            "operator_visibility": {
                "default": "notify/sidebar",
                "suggested_commands": ["cmux notify --title Rally --body <agent_visible.message>", "cmux set-status rally <status>"]
            },
            "agent_input_injection": {
                "default_enabled": false,
                "requires_explicit_operator_action": true,
                "message": agent_visible
            },
            "work_item": {
                "title": title,
                "body": body,
                "files": packet.files,
                "source_event_ids": packet.source_event_ids,
                "ready_to_act": ready_to_act,
                "trust": packet.recommended_next_action.trust,
            },
            "packet": packet,
        }),
        "herdr" => json!({
            "adapter": "herdr",
            "schema": "agent-rally.adapter.herdr-packet.v1",
            "side_effects": false,
            "ready_to_inject": ready_to_act,
            "operator_visibility": {
                "default": "pane.report_agent",
                "suggested_commands": ["herdr pane report-agent <pane> --source rally --state blocked --message <agent_visible.message>"]
            },
            "agent_input_injection": {
                "default_enabled": false,
                "requires_explicit_operator_action": true,
                "message": agent_visible
            },
            "prompt": body,
            "trust": packet.recommended_next_action.trust,
            "packet": packet,
        }),
        _ => unreachable!(),
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        format!("{adapter} packet for {tool}"),
        json!({ "adapter": adapter_data }),
    ))
}

fn adapter_body(kind: &str, role: &str, reason: &str, files: &[String]) -> String {
    let files = if files.is_empty() {
        "none".to_string()
    } else {
        files.join(", ")
    };
    format!("Rally {kind} packet ({role}). Next action: {reason}. Files: {files}.")
}

fn executable_on_path(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|path| Path::new(&path).join(name).is_file())
    })
}

pub(super) fn execute_herdr_inject(command: HerdrInjectCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("herdr:inject")?;
    let records = store.load_records().map_err(|err| {
        CliError::runtime("herdr:inject", format!("failed to load channel: {err}"))
    })?;
    let record = records
        .iter()
        .find(|record| record_id(record) == command.identifier)
        .ok_or_else(|| {
            CliError::not_found(
                "herdr:inject",
                format!("handoff {} was not found", command.identifier),
            )
        })?;
    let parsed = EventRecord::parse(record).map_err(|err| {
        CliError::runtime("herdr:inject", format!("failed to parse event: {err}"))
    })?;
    let Some(EventPayload::Handoff(payload)) = parsed.payload.as_ref() else {
        return Err(CliError::usage(
            "herdr:inject",
            format!("{} is not a handoff event", command.identifier),
        ));
    };
    let origin = record
        .get("origin")
        .and_then(Value::as_str)
        .unwrap_or("local");
    let trust_status = record
        .get("trust_status")
        .and_then(Value::as_str)
        .unwrap_or("unsigned");
    let trusted = trust_status == "trusted";
    let ready_to_inject = trusted || command.force || !command.strict;
    let action = if ready_to_inject {
        if command.force && !trusted {
            "override"
        } else {
            "ready"
        }
    } else {
        "refuse"
    };
    let text = if ready_to_inject {
        format!(
            "herdr inject {action}: {} trust_status={trust_status} origin={origin}",
            command.identifier
        )
    } else {
        format!(
            "herdr inject refused: {} trust_status={trust_status} origin={origin}; use --force to override",
            command.identifier
        )
    };
    Ok(query_output(
        "herdr:inject",
        &command.common,
        &store,
        text,
        json!({
            "gate": {
                "action": action,
                "ready_to_inject": ready_to_inject,
                "strict": command.strict,
                "override_used": command.force,
                "override_flag": "--force",
                "trust": {
                    "origin": origin,
                    "trust_status": trust_status,
                    "required": "trusted"
                },
                "handoff": {
                    "event_id": command.identifier,
                    "subject": payload.subject,
                    "from_tool": payload.from_tool,
                    "to_tool": payload.to_tool,
                    "files": payload.ref_files,
                    "notes": payload.notes
                }
            }
        }),
    ))
}

pub(super) fn execute_diagnose(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let diagnosis = diagnose_records(
        &records,
        DiagnoseOptions {
            state_records: Some(&records),
            tool: command.tool.as_deref(),
            stale_after_seconds: command.stale_after_seconds,
            since: command.since.as_deref(),
            now_epoch_seconds: now,
        },
    );
    let text = if diagnosis.findings.is_empty() {
        format!("{} score={}", diagnosis.status, diagnosis.score)
    } else {
        let mut lines = vec![format!("{} score={}", diagnosis.status, diagnosis.score)];
        lines.extend(diagnosis.findings.iter().map(|finding| {
            let event = finding
                .event_id
                .as_ref()
                .map(|event_id| format!(" [{event_id}]"))
                .unwrap_or_default();
            format!(
                "{} {}{}: {}",
                finding.severity, finding.code, event, finding.message
            )
        }));
        lines.join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "diagnosis": diagnosis,
        }),
    ))
}

pub(super) fn execute_score(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let conn = graph_caught_up("score", &store, &records)?;
    let (score, findings) = graph::score(&conn, &records, command.tool.as_deref(), now)
        .map_err(|err| CliError::runtime("score", format!("score: {err}")))?;
    let text = if findings.is_empty() {
        format!("score={score}")
    } else {
        let mut lines = vec![format!("score={score}")];
        lines.extend(findings.iter().map(|finding| {
            let event = finding
                .event_id
                .as_ref()
                .map(|event_id| format!(" [{event_id}]"))
                .unwrap_or_default();
            format!(
                "{} {}{}: {}",
                finding.severity, finding.code, event, finding.message
            )
        }));
        lines.join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "score": score,
            "findings": findings,
        }),
    ))
}

pub(super) fn execute_thread(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let identifier = command
        .identifier
        .clone()
        .ok_or_else(|| CliError::usage(command.command, "thread requires an identifier"))?;
    let (store, records, _now) = query_records(&command)?;
    let events = related_records(&records, &identifier);
    let events = limit_records(events, command.limit);
    let text = if events.is_empty() {
        format!("No related events for {identifier}.")
    } else {
        events
            .iter()
            .map(|record| format_record_line(record, command.ids))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "identifier": identifier,
            "events": events,
        }),
    ))
}

pub(super) fn execute_replay(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, _now) = query_records(&command)?;
    let events = limit_records(
        filter_thread(records, command.thread.as_deref()),
        command.limit,
    );
    let text = if events.is_empty() {
        "No events.".to_string()
    } else {
        events
            .iter()
            .map(|record| format_record_line(record, true))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "events": events,
        }),
    ))
}

pub(super) fn execute_report(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, _now) = query_records(&command)?;
    let events = limit_records(
        filter_thread(records, command.thread.as_deref()),
        command.limit,
    );
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in &events {
        *counts.entry(record_kind(record)).or_default() += 1;
    }
    let text = if events.is_empty() {
        "No events.".to_string()
    } else {
        let counts_text = counts
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut lines = vec![format!("summary records={} {}", events.len(), counts_text)];
        lines.extend(
            events
                .iter()
                .map(|record| format_record_line(record, command.ids)),
        );
        lines.join("\n")
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "records": events.len(),
            "counts": counts,
            "events": events,
        }),
    ))
}

fn query_output(
    command: &'static str,
    options: &CommonOptions,
    store: &ChannelStore,
    text: String,
    data: Value,
) -> WriteOutput {
    WriteOutput {
        json: options.json,
        text,
        body: json!({
            "ok": true,
            "command": command,
            "schema": format!("agent-rally.command.{}.v1", command.replace(':', "-")),
            "channel": store.channel_dir().display().to_string(),
            "data": data,
        }),
    }
}

pub(super) fn query_records(
    command: &ReadCommand,
) -> Result<(ChannelStore, Vec<Value>, f64), CliError> {
    let store = command.common.channel_store(command.command)?;
    let now = now_epoch_seconds();
    let cutoff = parse_since(command.since.as_deref(), now)
        .map_err(|err| CliError::usage(command.command, err.to_string()))?;
    let records = store.load_records_cached().map_err(|err| {
        CliError::runtime(command.command, format!("failed to load channel: {err}"))
    })?;
    Ok((store, filter_since(&records, cutoff), now))
}

fn limit_records(records: Vec<Value>, limit: usize) -> Vec<Value> {
    records.into_iter().take(limit).collect()
}

fn filter_thread(records: Vec<Value>, thread: Option<&str>) -> Vec<Value> {
    let Some(thread) = thread else {
        return records;
    };
    records
        .into_iter()
        .filter(|record| event_field(record, "thread_id").as_deref() == Some(thread))
        .collect()
}

fn format_record_line(record: &Value, include_id: bool) -> String {
    let id = if include_id {
        format!("{} ", record_id(record))
    } else {
        String::new()
    };
    format!(
        "{id}{} {}: {}",
        record_kind(record),
        record_tool(record),
        record_subject(record)
    )
}

fn record_kind(record: &Value) -> String {
    EventRecord::parse(record)
        .map(|record| record.kind.label().to_string())
        .unwrap_or_else(|_| "event".to_string())
}

fn record_tool(record: &Value) -> String {
    event_field(record, "tool").unwrap_or_else(|| "unknown".to_string())
}

fn record_subject(record: &Value) -> String {
    EventRecord::parse(record)
        .map(|record| record.subject_label())
        .unwrap_or_else(|_| "(no subject)".to_string())
}

fn event_field(record: &Value, key: &str) -> Option<String> {
    event_value(record)
        .ok()
        .and_then(|event| event.get(key).and_then(Value::as_str).map(str::to_string))
}
