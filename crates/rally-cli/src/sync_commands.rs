// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{SyncExportCommand, SyncImportCommand};
use crate::output::{CliError, WriteOutput};
use crate::query_commands::query_records;
use crate::runtime::now_rfc3339;
use crate::trust_policy::{LoadedTrust, load_optional_trust};
use rally_core::sync::{SyncError, SyncErrorKind, build_sync_packet, import_sync_packet};
use rally_trust::{PublicKeyStore, TrustStatus, classify, classify_with_policy};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

pub(crate) fn execute_sync_export(command: SyncExportCommand) -> Result<WriteOutput, CliError> {
    let (store, records, _now) = query_records(&command.read)?;
    let packet = build_sync_packet(
        store.channel_dir().display().to_string(),
        now_rfc3339(),
        &records,
    )
    .map_err(|err| sync_error("sync:export", err))?;
    let text = serde_json::to_string_pretty(&packet)
        .map_err(|err| CliError::runtime("sync:export", err.to_string()))?;
    Ok(WriteOutput {
        json: command.read.common.json,
        text,
        body: packet,
    })
}

pub(crate) fn execute_sync_import(command: SyncImportCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("sync:import")?;
    let packet = read_sync_packet(&command.packet_path)?;
    let trust = load_optional_trust(
        command.trust_policy.as_ref(),
        command.no_default_trust_policy,
    )
    .map_err(|err| CliError::runtime("sync:import", err))?;
    let summary = import_sync_packet(&store, &packet, &command.origin, |event| {
        classify_status_for_import(event, trust.as_ref())
            .map(|status| status.to_string())
            .map_err(|err| SyncError::new(err.to_string()))
    })
    .map_err(|err| sync_error("sync:import", err))?;

    let body = json!({
        "ok": true,
        "command": "sync:import",
        "schema": "agent-rally.command.sync-import.v1",
        "channel": store.channel_dir().display().to_string(),
        "origin": command.origin,
        "data": {
            "imported": summary.imported,
            "duplicates": summary.duplicates,
            "conflicts": summary.conflicts,
            "invalid": summary.invalid,
            "trust_counts": summary.trust_counts,
        }
    });
    Ok(WriteOutput {
        json: command.common.json,
        text: format!(
            "imported={} duplicates={} conflicts={} invalid={}",
            summary.imported,
            summary.duplicates,
            body["data"]["conflicts"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
            summary.invalid
        ),
        body,
    })
}

fn read_sync_packet(path: &PathBuf) -> Result<Value, CliError> {
    let text = fs::read_to_string(path)
        .map_err(|err| CliError::runtime("sync:import", format!("failed to read packet: {err}")))?;
    serde_json::from_str(&text)
        .map_err(|err| CliError::runtime("sync:import", format!("invalid packet JSON: {err}")))
}

fn sync_error(command: &'static str, err: SyncError) -> CliError {
    match err.kind() {
        SyncErrorKind::Usage => CliError::usage(command, err.to_string()),
        SyncErrorKind::Runtime => CliError::runtime(command, err.to_string()),
    }
}

fn classify_status_for_import(
    event: &Value,
    trust: Option<&LoadedTrust>,
) -> Result<TrustStatus, rally_trust::TrustError> {
    let classification = if let Some(context) = trust {
        classify_with_policy(event, &context.keys, Some(&context.policy))?
    } else {
        classify(event, &PublicKeyStore::new())?
    };
    Ok(classification.status)
}
