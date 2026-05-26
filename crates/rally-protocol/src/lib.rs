// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Remote-ready Rally event protocol primitives.
//!
//! The important boundary is between a portable Rally event and local store
//! metadata. Events are the immutable, signable, syncable units. Store metadata
//! such as local sequence/revision, import origin, and received timestamps is
//! intentionally excluded from canonical event bytes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// Current canonicalization profile for Rust-native Rally event bytes.
pub const CANONICALIZATION_VERSION: &str = "rally-json-v1";

/// Local metadata fields that are never part of signed/canonical event bytes.
pub const LOCAL_METADATA_FIELDS: &[&str] = &[
    "revision",
    "local_seq",
    "received_at",
    "origin",
    "imported_at",
    "store",
    "sync",
];

/// A portable Rally event envelope.
///
/// This struct documents the Rust-native portable shape. `changes.jsonl` stores
/// these events inside `StoreEntry`; sync/export code can also handle portable
/// events directly before local replica metadata is attached.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RallyEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub specversion: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub app_slug: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub datacontenttype: Option<String>,
    #[serde(default)]
    pub dataschema: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub signature: Option<SignatureEnvelope>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Local append/import metadata wrapped around a portable event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StoreEntry {
    #[serde(default)]
    pub local_seq: Option<u64>,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    pub event: RallyEvent,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Signature metadata carried on a signed event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SignatureEnvelope {
    pub version: String,
    pub algorithm: String,
    pub key_id: String,
    pub signed_at: String,
    pub signature: String,
    #[serde(default)]
    pub canonicalization: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HandoffCreated {
    pub subject: String,
    #[serde(default)]
    pub to_tool: Option<String>,
    #[serde(default)]
    pub from_tool: Option<String>,
    #[serde(default)]
    pub requires_ack: Option<bool>,
    #[serde(default)]
    pub ref_files: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HandoffAcknowledged {
    pub ref_handoff_id: String,
    pub verdict: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClaimCreated {
    pub owner_tool: String,
    pub resource: String,
    pub subject: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClaimReleased {
    pub ref_claim_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlockerRaised {
    pub subject: String,
    pub reason: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlockerResolved {
    pub ref_blocker_id: String,
    pub resolution: String,
}

#[derive(Debug)]
pub enum ProtocolError {
    ExpectedObject,
    MissingEventObject,
    MissingId,
    InvalidJsonlLine {
        line: usize,
        source: serde_json::Error,
    },
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpectedObject => write!(f, "expected a JSON object record"),
            Self::MissingEventObject => write!(f, "store entry is missing an event object"),
            Self::MissingId => write!(f, "event is missing string id"),
            Self::InvalidJsonlLine { line, source } => {
                write!(f, "invalid JSONL record on line {line}: {source}")
            }
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<serde_json::Error> for ProtocolError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<std::io::Error> for ProtocolError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Extract the portable event object from a portable event or store entry.
pub fn event_value(record: &Value) -> Result<Value, ProtocolError> {
    let object = record.as_object().ok_or(ProtocolError::ExpectedObject)?;
    if let Some(event) = object.get("event") {
        if event.is_object() {
            Ok(event.clone())
        } else {
            Err(ProtocolError::MissingEventObject)
        }
    } else {
        Ok(Value::Object(object.clone()))
    }
}

/// Return the portable event id from a portable event or store entry.
pub fn event_id(record: &Value) -> Result<String, ProtocolError> {
    let event = event_value(record)?;
    event
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ProtocolError::MissingId)
}

/// Return the portable event object, preserving signatures and excluding local metadata.
pub fn portable_event_value(record: &Value) -> Result<Value, ProtocolError> {
    let event = event_value(record)?;
    let mut object = event
        .as_object()
        .ok_or(ProtocolError::ExpectedObject)?
        .clone();
    for key in LOCAL_METADATA_FIELDS {
        object.remove(*key);
    }
    Ok(Value::Object(object))
}

/// Normalize a record to the object whose bytes are signed/verified.
///
/// The signature envelope and local store metadata are removed so events can be
/// imported into a remote/local log with different replica metadata without
/// invalidating trust.
pub fn canonical_event_value(record: &Value) -> Result<Value, ProtocolError> {
    let portable = portable_event_value(record)?;
    let mut object = portable
        .as_object()
        .ok_or(ProtocolError::ExpectedObject)?
        .clone();
    object.remove("signature");
    Ok(Value::Object(object))
}

/// Return stable compact JSON bytes for a portable event.
///
/// `serde_json` serializes maps in deterministic key order with default
/// features. Rally keeps this profile versioned so a stricter RFC 8785/JCS
/// profile can be introduced later without changing existing signatures.
pub fn canonical_event_bytes(record: &Value) -> Result<Vec<u8>, ProtocolError> {
    Ok(serde_json::to_vec(&canonical_event_value(record)?)?)
}

/// Read newline-delimited JSON records, skipping blank and malformed trailing lines.
pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<Value>, ProtocolError> {
    let text = fs::read_to_string(path)?;
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some((index + 1, trimmed))
        })
        .collect();
    let last_index = lines.len().saturating_sub(1);
    let has_trailing_newline = text.ends_with('\n');
    let mut records = Vec::new();
    for (index, (line_number, trimmed)) in lines.into_iter().enumerate() {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => records.push(value),
            Err(err) if err.is_eof() && index == last_index && !has_trailing_newline => continue,
            Err(err) => {
                return Err(ProtocolError::InvalidJsonlLine {
                    line: line_number,
                    source: err,
                });
            }
        }
    }
    Ok(records)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
    pub event_id: String,
    pub first_index: usize,
    pub duplicate_index: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MergeReport {
    pub events: Vec<Value>,
    pub duplicate_count: usize,
    pub conflicts: Vec<MergeConflict>,
}

/// Idempotently merge records by portable event id.
///
/// Exact duplicate event ids with identical canonical bytes are ignored. The
/// same id with different canonical bytes is reported as a conflict rather than
/// hidden by last-writer-wins behavior.
pub fn merge_unique_by_id(
    records: impl IntoIterator<Item = Value>,
) -> Result<MergeReport, ProtocolError> {
    let mut report = MergeReport::default();
    let mut seen: HashMap<String, (usize, Vec<u8>)> = HashMap::new();

    for (index, record) in records.into_iter().enumerate() {
        let id = event_id(&record)?;
        let canonical = canonical_event_bytes(&record)?;
        if let Some((first_index, first_bytes)) = seen.get(&id) {
            if first_bytes == &canonical {
                report.duplicate_count += 1;
            } else {
                report.conflicts.push(MergeConflict {
                    event_id: id,
                    first_index: *first_index,
                    duplicate_index: index,
                });
            }
            continue;
        }
        seen.insert(id, (index, canonical));
        report.events.push(portable_event_value(&record)?);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn event() -> Value {
        json!({
            "specversion": "1.0",
            "id": "evt_11111111111111111111111111111111",
            "source": "urn:agent-rally-point:tool:pi",
            "subject": "agent-rally-point",
            "time": "2026-05-26T18:00:00.000Z",
            "kind": "claim",
            "type": "agent-rally.claim.created.v1",
            "tool": "pi",
            "model": "unknown",
            "run_id": "r1",
            "app_slug": "agent-rally-point",
            "thread_id": "thr_22222222222222222222222222222222",
            "causation_id": null,
            "datacontenttype": "application/json",
            "dataschema": "urn:agent-rally-point:schema:claim.created.v1",
            "payload": {"owner_tool": "pi", "resource": "file:docs/SCHEMA.md", "subject": "edit schema"}
        })
    }

    #[test]
    fn canonical_bytes_exclude_signature() {
        let unsigned = event();
        let mut signed = event();
        signed.as_object_mut().unwrap().insert(
            "signature".into(),
            json!({
                "version": "rally-signature-v1",
                "algorithm": "ed25519",
                "key_id": "key_demo",
                "signed_at": "2026-05-26T18:00:01.000Z",
                "signature": "AAAA"
            }),
        );

        assert_eq!(
            canonical_event_bytes(&unsigned).unwrap(),
            canonical_event_bytes(&signed).unwrap()
        );
    }

    #[test]
    fn canonical_bytes_exclude_local_revision_metadata() {
        let mut left = event();
        let mut right = event();
        left.as_object_mut()
            .unwrap()
            .insert("revision".into(), json!(1));
        right
            .as_object_mut()
            .unwrap()
            .insert("revision".into(), json!(99));

        assert_eq!(
            canonical_event_bytes(&left).unwrap(),
            canonical_event_bytes(&right).unwrap()
        );
    }

    #[test]
    fn canonical_bytes_exclude_store_metadata() {
        let wrapped_a = json!({
            "local_seq": 1,
            "received_at": "2026-05-26T18:00:02.000Z",
            "origin": "local",
            "event": event()
        });
        let wrapped_b = json!({
            "local_seq": 44,
            "received_at": "2027-01-01T00:00:00.000Z",
            "origin": "remote:peer-a",
            "event": event()
        });

        assert_eq!(
            canonical_event_bytes(&wrapped_a).unwrap(),
            canonical_event_bytes(&wrapped_b).unwrap()
        );
    }

    #[test]
    fn canonical_bytes_use_utf8_for_non_ascii_strings() {
        let mut record = event();
        record["payload"]["subject"] = json!("cafe \u{e9}");

        let bytes = canonical_event_bytes(&record).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("cafe \u{e9}"));
        assert!(!rendered.contains("\\u00e9"));
    }

    #[test]
    fn reads_portable_event_jsonl_records() {
        let path = std::env::temp_dir().join(format!(
            "rally-event-{}.jsonl",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            format!("\n{}\n{{", serde_json::to_string(&event()).unwrap()),
        )
        .unwrap();

        let records = read_jsonl(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(
            event_id(&records[0]).unwrap(),
            "evt_11111111111111111111111111111111"
        );
    }

    #[test]
    fn read_jsonl_rejects_incomplete_middle_line() {
        let path = std::env::temp_dir().join(format!(
            "rally-corrupt-middle-{}.jsonl",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            format!(
                "{}\n{{\n{}\n",
                serde_json::to_string(&event()).unwrap(),
                serde_json::to_string(&event()).unwrap()
            ),
        )
        .unwrap();

        let err = read_jsonl(&path).unwrap_err();
        fs::remove_file(path).unwrap();

        assert!(matches!(
            err,
            ProtocolError::InvalidJsonlLine { line: 2, .. }
        ));
    }

    #[test]
    fn read_jsonl_rejects_corrupt_final_line_with_newline() {
        let path = std::env::temp_dir().join(format!(
            "rally-corrupt-final-{}.jsonl",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            format!("{}\n{{\n", serde_json::to_string(&event()).unwrap()),
        )
        .unwrap();

        let err = read_jsonl(&path).unwrap_err();
        fs::remove_file(path).unwrap();

        assert!(matches!(
            err,
            ProtocolError::InvalidJsonlLine { line: 2, .. }
        ));
    }

    #[test]
    fn reads_store_entry_records() {
        let wrapped = json!({
            "local_seq": 12,
            "received_at": "2026-05-26T18:00:02.000Z",
            "origin": "remote:peer-a",
            "event": event()
        });

        assert_eq!(
            event_id(&wrapped).unwrap(),
            "evt_11111111111111111111111111111111"
        );
        let canonical = canonical_event_value(&wrapped).unwrap();
        assert!(canonical.get("local_seq").is_none());
        assert_eq!(canonical["payload"]["resource"], "file:docs/SCHEMA.md");
    }

    #[test]
    fn merge_duplicate_event_ids_is_idempotent() {
        let report = merge_unique_by_id(vec![event(), event()]).unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.duplicate_count, 1);
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn merge_preserves_signature_on_surviving_event() {
        let mut signed = event();
        signed.as_object_mut().unwrap().insert(
            "signature".into(),
            json!({
                "version": "rally-signature-v1",
                "algorithm": "ed25519",
                "key_id": "key_demo",
                "signed_at": "2026-05-26T18:00:01.000Z",
                "signature": "AAAA"
            }),
        );
        signed
            .as_object_mut()
            .unwrap()
            .insert("revision".into(), json!(17));

        let report = merge_unique_by_id(vec![signed]).unwrap();

        assert!(report.events[0].get("signature").is_some());
        assert!(report.events[0].get("revision").is_none());
    }

    #[test]
    fn merge_same_id_different_bytes_reports_conflict() {
        let left = event();
        let mut right = event();
        right["payload"]["subject"] = json!("different subject");

        let report = merge_unique_by_id(vec![left, right]).unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.duplicate_count, 0);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].event_id,
            "evt_11111111111111111111111111111111"
        );
    }
}
