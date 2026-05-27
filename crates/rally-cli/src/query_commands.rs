// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{CommonOptions, HerdrInjectCommand, ReadCommand, SetupCommand, StartCommand};
use crate::output::{CliError, WriteOutput};
use crate::runtime::now_rfc3339;
use rally_core::context::{build_context_brief, build_work_packet};
use rally_core::cursors;
use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::event::{EventPayload, EventRecord};
use rally_core::preflight::{PreflightOptions, run_preflight};
use rally_core::query::{
    TraceProjection, active_blockers_at, active_claims_at, claim_conflicts, filter_since,
    now_epoch_seconds, parse_since, pending_handoffs_at, record_id, related_records, score_records,
};
use rally_core::store::ChannelStore;
use rally_protocol::event_value;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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

pub(super) fn execute_inbox(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
    let (records, cursor_scope) = apply_cursor(&store, records, &command);
    let pending = pending_handoffs_at(&records, command.tool.as_deref(), now);
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
    let claims = active_claims_at(&records, command.tool.as_deref(), now);
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
    let blockers = active_blockers_at(&records, command.tool.as_deref(), now);
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
    let conflicts = claim_conflicts(&records);
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
    let projection = TraceProjection::from_records_at(&records, now);
    let brief = build_context_brief(&projection, &tool, command.limit);
    let text = if let Some(priority) = &brief.top_priority {
        format!(
            "{}: {} ({})",
            brief.recommended_next_action.action, priority.subject, priority.event_id
        )
    } else {
        brief.recommended_next_action.reason.clone()
    };
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "brief": brief,
        }),
    ))
}

pub(super) fn execute_packet(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let tool = command.common.tool();
    let (store, records, now) = query_records(&command)?;
    let projection = TraceProjection::from_records_at(&records, now);
    let brief = build_context_brief(&projection, &tool, command.limit);
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

    let projection = TraceProjection::from_records_at(&records, now_epoch_seconds());
    let brief = build_context_brief(&projection, &command.tool, command.limit);
    let packet = build_work_packet(&brief, command.limit);
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
    let projection = TraceProjection::from_records_at(&records, now);
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
    let active_claims = projection.active_claims(None);
    let active_tasks = projection.active_tasks(None);
    let pending_handoffs = projection.pending_handoffs(None);
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
    if command.tool.is_some() && projection.profile(&tool).is_none() {
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

pub(super) fn execute_setup(command: SetupCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("setup")?;
    let mut status = setup_status(&store)?;
    let mut installed_path = None;
    match command.action.as_str() {
        "status" => {}
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
            write_setup_config(&store, mode)?;
            status = setup_status(&store)?;
        }
        "install" => {
            let adapter = command
                .target
                .as_deref()
                .ok_or_else(|| CliError::usage("setup", "setup install requires cmux or herdr"))?;
            if !matches!(adapter, "cmux" | "herdr") {
                return Err(CliError::usage(
                    "setup",
                    "unknown adapter; expected cmux or herdr",
                ));
            }
            installed_path = Some(write_adapter_install(&store, adapter)?);
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
            }
        }),
    ))
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

fn write_adapter_install(store: &ChannelStore, adapter: &str) -> Result<String, CliError> {
    let dir = store.channel_dir().join("rally/adapters");
    fs::create_dir_all(&dir).map_err(|err| {
        CliError::runtime("setup", format!("failed to create adapter dir: {err}"))
    })?;
    let path = dir.join(format!("{adapter}-rally-start.md"));
    let body = match adapter {
        "cmux" => {
            "# cmux Rally integration\n\nRun `rally <tool>` when an agent pane starts. Use `rally cmux packet --tool <tool>` for feed/workspace payloads.\n"
        }
        "herdr" => {
            "# Herdr Rally integration\n\nRun `rally <tool>` when an agent pane starts. Use `rally herdr packet --tool <tool>` and honor `rally herdr inject` gates.\n"
        }
        _ => unreachable!(),
    };
    fs::write(&path, body).map_err(|err| {
        CliError::runtime("setup", format!("failed to write adapter file: {err}"))
    })?;
    Ok(path.display().to_string())
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
                        "purpose": "workspace/feed-friendly packet export"
                    },
                    "herdr": {
                        "command": "rally herdr packet --tool <tool> --json",
                        "side_effects": false,
                        "purpose": "prompt payload export; actual injection remains adapter-owned"
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
    let projection = TraceProjection::from_records_at(&records, now);
    let brief = build_context_brief(&projection, &tool, command.limit);
    let packet = build_work_packet(&brief, command.limit);
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
    let (store, records, _now) = query_records(&command)?;
    let (score, findings) = score_records(&records, command.tool.as_deref());
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
