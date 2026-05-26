// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::store::ChannelStore;
use rally_protocol::{canonical_event_bytes, event_id, portable_event_value};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug)]
pub struct SyncError {
    message: String,
    kind: SyncErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncErrorKind {
    Usage,
    Runtime,
}

impl SyncError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::runtime(message)
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SyncErrorKind::Runtime,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SyncErrorKind::Usage,
        }
    }

    pub fn kind(&self) -> SyncErrorKind {
        self.kind
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SyncError {}

#[derive(Clone, Debug, Serialize)]
pub struct SyncImportSummary {
    pub imported: usize,
    pub duplicates: usize,
    pub conflicts: Vec<Value>,
    pub invalid: usize,
    pub trust_counts: BTreeMap<String, usize>,
}

pub fn build_sync_packet(
    source_channel: impl Into<String>,
    exported_at: impl Into<String>,
    records: &[Value],
) -> Result<Value, SyncError> {
    let events = records
        .iter()
        .map(portable_event_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| SyncError::new(err.to_string()))?;
    Ok(json!({
        "schema": "agent-rally.sync.packet.v1",
        "exported_at": exported_at.into(),
        "source_channel": source_channel.into(),
        "count": events.len(),
        "events": events,
    }))
}

pub fn import_sync_packet<F>(
    store: &ChannelStore,
    packet: &Value,
    origin: &str,
    mut classify_trust: F,
) -> Result<SyncImportSummary, SyncError>
where
    F: FnMut(&Value) -> Result<String, SyncError>,
{
    let packet_events = packet_events(packet)?;
    let existing = store
        .load_records()
        .map_err(|err| SyncError::new(format!("failed to load channel: {err}")))?;
    let mut known: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for record in &existing {
        let id = event_id(record).map_err(|err| SyncError::new(err.to_string()))?;
        let canonical =
            canonical_event_bytes(record).map_err(|err| SyncError::new(err.to_string()))?;
        known.insert(id, canonical);
    }

    let mut imported = 0_usize;
    let mut duplicates = 0_usize;
    let mut invalid = 0_usize;
    let mut conflicts = Vec::new();
    let mut trust_counts: BTreeMap<String, usize> = BTreeMap::new();

    for raw in packet_events {
        let event = match portable_event_value(raw) {
            Ok(event) => event,
            Err(_) => {
                invalid += 1;
                continue;
            }
        };
        let id = match event_id(&event) {
            Ok(id) => id,
            Err(_) => {
                invalid += 1;
                continue;
            }
        };
        let canonical = match canonical_event_bytes(&event) {
            Ok(bytes) => bytes,
            Err(_) => {
                invalid += 1;
                continue;
            }
        };
        let status = classify_trust(&event)?;
        *trust_counts.entry(status.clone()).or_default() += 1;
        if let Some(existing) = known.get(&id) {
            if existing == &canonical {
                duplicates += 1;
            } else {
                conflicts.push(
                    json!({"event_id": id, "reason": "same id with different canonical bytes"}),
                );
            }
            continue;
        }
        store
            .append_event_with_origin_and_trust(event, origin, Some(&status))
            .map_err(|err| SyncError::new(format!("failed to append import: {err}")))?;
        known.insert(id, canonical);
        imported += 1;
    }

    Ok(SyncImportSummary {
        imported,
        duplicates,
        conflicts,
        invalid,
        trust_counts,
    })
}

fn packet_events(packet: &Value) -> Result<&[Value], SyncError> {
    packet
        .get("events")
        .or_else(|| packet.get("packet").and_then(|packet| packet.get("events")))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| SyncError::usage("packet must contain an events array"))
}
