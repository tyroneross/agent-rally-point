// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{CommonOptions, ReadCommand};
use crate::output::{CliError, WriteOutput};
use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::event::EventRecord;
use rally_core::query::{
    active_blockers_at, active_claims_at, claim_conflicts, filter_since, now_epoch_seconds,
    parse_since, pending_handoffs_at, record_id, related_records, score_records,
};
use rally_core::store::ChannelStore;
use rally_protocol::event_value;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn execute_inbox(command: ReadCommand) -> Result<WriteOutput, CliError> {
    let (store, records, now) = query_records(&command)?;
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
    Ok(query_output(
        command.command,
        &command.common,
        &store,
        text,
        json!({
            "pending": pending,
        }),
    ))
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
            "schema": format!("agent-rally.command.{command}.v1"),
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
    let records = store.load_records().map_err(|err| {
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
