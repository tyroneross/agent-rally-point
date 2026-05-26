// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use crate::event::{EventKind, EventPayload, EventRecord};
use chrono::{DateTime, Utc};
use rally_protocol::event_value;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingHandoff {
    pub event_id: String,
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_status: Option<String>,
    pub from_tool: Option<String>,
    pub to_tool: Option<String>,
    pub subject: String,
    pub age_seconds: Option<i64>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveClaim {
    pub event_id: String,
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_status: Option<String>,
    pub owner_tool: Option<String>,
    pub resource: String,
    pub subject: String,
    pub age_seconds: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveBlocker {
    pub event_id: String,
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_status: Option<String>,
    pub tool: Option<String>,
    pub subject: String,
    pub resource: Option<String>,
    pub severity: String,
    pub age_seconds: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClaimConflict {
    pub resource: String,
    pub claim_ids: Vec<String>,
    pub owners: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScoreFinding {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub event_id: Option<String>,
}

pub fn now_epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn parse_since(value: Option<&str>, now: f64) -> Result<Option<f64>, CoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some((number, unit)) = text.split_at_checked(text.len().saturating_sub(1)) {
        if let Ok(amount) = number.parse::<f64>() {
            let seconds = match unit {
                "s" => Some(amount),
                "m" => Some(amount * 60.0),
                "h" => Some(amount * 3600.0),
                "d" => Some(amount * 86400.0),
                _ => None,
            };
            if let Some(seconds) = seconds {
                return Ok(Some(now - seconds));
            }
        }
    }
    DateTime::parse_from_rfc3339(text)
        .map(|datetime| Some(datetime.timestamp() as f64))
        .map_err(|_| CoreError::InvalidSince(value.to_string()))
}

pub fn record_epoch(record: &Value) -> f64 {
    let Ok(record) = EventRecord::parse(record) else {
        return 0.0;
    };
    if let Some(ts) = record.event.get("ts").and_then(Value::as_f64) {
        return ts;
    }
    record
        .event
        .get("time")
        .and_then(Value::as_str)
        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        .map(|datetime| datetime.with_timezone(&Utc).timestamp() as f64)
        .unwrap_or(0.0)
}

pub fn filter_since(records: &[Value], cutoff: Option<f64>) -> Vec<Value> {
    let Some(cutoff) = cutoff else {
        return records.to_vec();
    };
    records
        .iter()
        .filter(|record| record_epoch(record) >= cutoff)
        .cloned()
        .collect()
}

pub fn record_id(record: &Value) -> String {
    let event = event_value(record).unwrap_or_else(|_| record.clone());
    if let Some(id) = event
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return id.to_string();
    }
    if let Some(id) = payload_string(&event, "id") {
        return id;
    }
    if let Some(revision) = event
        .get("revision")
        .or_else(|| record.get("revision"))
        .and_then(value_to_display_string)
    {
        return format!("rev:{revision}");
    }
    "(no-id)".to_string()
}

pub fn record_aliases(record: &Value) -> BTreeSet<String> {
    let event = event_value(record).unwrap_or_else(|_| record.clone());
    let mut aliases = BTreeSet::from([record_id(record)]);
    for value in [
        event.get("id"),
        payload_value(&event, "id"),
        event.get("revision"),
        record.get("revision"),
        record.get("local_seq"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(alias) = value_to_display_string(value) {
            aliases.insert(alias.clone());
            if value.is_number() {
                aliases.insert(format!("rev:{alias}"));
            }
        }
    }
    aliases
}

pub fn related_records(records: &[Value], identifier: &str) -> Vec<Value> {
    let mut ids = BTreeSet::from([identifier.to_string()]);
    let mut changed = true;
    while changed {
        changed = false;
        for record in records {
            let values = relation_values(record);
            if values.iter().any(|value| ids.contains(value)) {
                for value in values {
                    if ids.insert(value) {
                        changed = true;
                    }
                }
            }
        }
    }
    records
        .iter()
        .filter(|record| {
            relation_values(record)
                .iter()
                .any(|value| ids.contains(value))
        })
        .cloned()
        .collect()
}

pub fn pending_handoffs(records: &[Value], tool: Option<&str>) -> Vec<PendingHandoff> {
    pending_handoffs_at(records, tool, now_epoch_seconds())
}

pub fn pending_handoffs_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<PendingHandoff> {
    let acked = referenced_ids(records, EventKind::Ack);
    let mut out = Vec::new();
    for record in records {
        let Ok(parsed) = EventRecord::parse(record) else {
            continue;
        };
        let Some(EventPayload::Handoff(payload)) = parsed.payload else {
            continue;
        };
        if !payload.requires_ack || !record_aliases(record).is_disjoint(&acked) {
            continue;
        }
        let to_tool = payload.to_tool;
        let from_tool = payload.from_tool.or(parsed.tool);
        if let Some(tool) = tool {
            if !matches!(to_tool.as_deref(), Some(value) if value == tool || value == "all")
                && to_tool.is_some()
            {
                continue;
            }
            if from_tool.as_deref() == Some(tool) {
                continue;
            }
        }
        out.push(PendingHandoff {
            event_id: record_id(record),
            thread_id: parsed.thread_id,
            origin: record_origin(record),
            trust_status: record_trust_status(record),
            from_tool,
            to_tool,
            subject: payload.subject,
            age_seconds: age_seconds(record, now),
            files: payload.ref_files,
        });
    }
    out
}

pub fn active_claims(records: &[Value], tool: Option<&str>) -> Vec<ActiveClaim> {
    active_claims_at(records, tool, now_epoch_seconds())
}

pub fn active_claims_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<ActiveClaim> {
    let released = referenced_ids(records, EventKind::ClaimRelease);
    let mut out = Vec::new();
    for record in records {
        let Ok(parsed) = EventRecord::parse(record) else {
            continue;
        };
        let Some(EventPayload::Claim(payload)) = parsed.payload else {
            continue;
        };
        if !record_aliases(record).is_disjoint(&released) {
            continue;
        }
        let owner = Some(payload.owner_tool);
        if tool.is_some() && owner.as_deref() != tool {
            continue;
        }
        out.push(ActiveClaim {
            event_id: record_id(record),
            thread_id: parsed.thread_id,
            origin: record_origin(record),
            trust_status: record_trust_status(record),
            owner_tool: owner,
            resource: payload.resource,
            subject: payload.subject,
            age_seconds: age_seconds(record, now),
        });
    }
    out
}

pub fn claim_conflicts(records: &[Value]) -> Vec<ClaimConflict> {
    let claims = active_claims(records, None);
    let mut by_resource: BTreeMap<String, Vec<ActiveClaim>> = BTreeMap::new();
    for left_index in 0..claims.len() {
        for right in claims.iter().skip(left_index + 1) {
            let left = &claims[left_index];
            if left.owner_tool == right.owner_tool
                || !resources_overlap(&left.resource, &right.resource)
            {
                continue;
            }
            let group_key = if left.resource.len() <= right.resource.len() {
                left.resource.clone()
            } else {
                right.resource.clone()
            };
            let bucket = by_resource.entry(group_key).or_default();
            for claim in [left, right] {
                if !bucket.iter().any(|item| item.event_id == claim.event_id) {
                    bucket.push(claim.clone());
                }
            }
        }
    }
    by_resource
        .into_iter()
        .filter_map(|(resource, claims)| {
            let owners: BTreeSet<String> = claims
                .iter()
                .map(|claim| {
                    claim
                        .owner_tool
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string())
                })
                .collect();
            (claims.len() > 1 && owners.len() > 1).then(|| ClaimConflict {
                resource,
                claim_ids: claims.into_iter().map(|claim| claim.event_id).collect(),
                owners: owners.into_iter().collect(),
            })
        })
        .collect()
}

pub fn active_blockers(records: &[Value], tool: Option<&str>) -> Vec<ActiveBlocker> {
    active_blockers_at(records, tool, now_epoch_seconds())
}

pub fn active_blockers_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<ActiveBlocker> {
    let resolved = referenced_ids(records, EventKind::BlockerResolved);
    let mut out = Vec::new();
    for record in records {
        let Ok(parsed) = EventRecord::parse(record) else {
            continue;
        };
        let Some(EventPayload::Blocker(payload)) = parsed.payload else {
            continue;
        };
        if !record_aliases(record).is_disjoint(&resolved) {
            continue;
        }
        let producer = parsed.tool;
        if tool.is_some() && producer.as_deref() != tool {
            continue;
        }
        out.push(ActiveBlocker {
            event_id: record_id(record),
            thread_id: parsed.thread_id,
            origin: record_origin(record),
            trust_status: record_trust_status(record),
            tool: producer,
            subject: payload.subject,
            resource: payload.resource,
            severity: payload.severity.unwrap_or_else(|| "blocked".to_string()),
            age_seconds: age_seconds(record, now),
        });
    }
    out
}

pub fn score_records(records: &[Value], tool: Option<&str>) -> (i64, Vec<ScoreFinding>) {
    let mut findings = Vec::new();
    let ids = known_ids(records);
    for item in pending_handoffs(records, tool) {
        findings.push(ScoreFinding {
            severity: "P1".to_string(),
            code: "open-required-handoff".to_string(),
            event_id: Some(item.event_id),
            message: format!(
                "required handoff to {} is still open: {}",
                item.to_tool.unwrap_or_else(|| "unknown".to_string()),
                item.subject
            ),
        });
    }

    for record in records {
        let Ok(parsed) = EventRecord::parse(record) else {
            continue;
        };
        if let Some(causation) = parsed.causation_id.filter(|value| !value.is_empty()) {
            if !ids.contains(&causation) {
                findings.push(ScoreFinding {
                    severity: "P2".to_string(),
                    code: "dangling-causation".to_string(),
                    event_id: Some(record_id(record)),
                    message: format!(
                        "causation_id {causation} does not resolve in this trace window"
                    ),
                });
            }
        }
        if let Some(EventPayload::Ack(payload) | EventPayload::Feedback(payload)) = parsed.payload {
            if !payload.ref_handoff_id.is_empty() {
                let reference = payload.ref_handoff_id;
                if !ids.contains(&reference) {
                    findings.push(ScoreFinding {
                        severity: "P2".to_string(),
                        code: "dangling-reference".to_string(),
                        event_id: Some(record_id(record)),
                        message: format!(
                            "{} references missing handoff/event {reference}",
                            kind_label(&parsed.kind)
                        ),
                    });
                }
            }
        }
    }

    for (reference, verdict) in final_ack_by_handoff(records) {
        if verdict == "needs-info" {
            findings.push(ScoreFinding {
                severity: "P2".to_string(),
                code: "unresolved-needs-info".to_string(),
                event_id: Some(reference),
                message: "handoff is still waiting on more information".to_string(),
            });
        }
    }

    let score = 100
        - findings
            .iter()
            .map(|finding| match finding.severity.as_str() {
                "P1" => 25,
                "P2" => 10,
                "P3" => 3,
                _ => 0,
            })
            .sum::<i64>();
    (score.max(0), findings)
}

fn referenced_ids(records: &[Value], kind: EventKind) -> BTreeSet<String> {
    records
        .iter()
        .filter_map(|record| EventRecord::parse(record).ok())
        .filter(|record| record.kind == kind)
        .filter_map(|record| match record.payload? {
            EventPayload::Ack(payload) | EventPayload::Feedback(payload) => {
                Some(payload.ref_handoff_id)
            }
            EventPayload::ClaimRelease(payload) => Some(payload.ref_claim_id),
            EventPayload::BlockerResolved(payload) => Some(payload.ref_blocker_id),
            _ => None,
        })
        .collect()
}

fn known_ids(records: &[Value]) -> BTreeSet<String> {
    records.iter().flat_map(record_aliases).collect()
}

fn final_ack_by_handoff(records: &[Value]) -> BTreeMap<String, String> {
    let mut latest = BTreeMap::new();
    for record in records {
        let Ok(parsed) = EventRecord::parse(record) else {
            continue;
        };
        if parsed.kind != EventKind::Ack {
            continue;
        }
        if let Some(EventPayload::Ack(payload)) = parsed.payload {
            latest.insert(payload.ref_handoff_id, payload.verdict);
        }
    }
    latest
}

fn kind_label(kind: &EventKind) -> &str {
    match kind {
        EventKind::Handoff => "handoff",
        EventKind::Ack => "ack",
        EventKind::Feedback => "feedback",
        EventKind::Claim => "claim",
        EventKind::ClaimRelease => "claim-release",
        EventKind::Blocker => "blocker",
        EventKind::BlockerResolved => "blocker-resolved",
        EventKind::Other(value) if value.is_empty() => "event",
        EventKind::Other(value) => value.as_str(),
    }
}

fn relation_values(record: &Value) -> BTreeSet<String> {
    let event = event_value(record).unwrap_or_else(|_| record.clone());
    [
        string_field(&event, "id"),
        string_field(&event, "thread_id"),
        string_field(&event, "correlation_id"),
        string_field(&event, "causation_id"),
        payload_string(&event, "id"),
        payload_string(&event, "ref_handoff_id"),
        payload_string(&event, "ref_event_id"),
        payload_string(&event, "ref_claim_id"),
        payload_string(&event, "ref_blocker_id"),
        payload_string(&event, "checkpoint_id"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn record_origin(record: &Value) -> Option<String> {
    string_field(record, "origin")
}

fn record_trust_status(record: &Value) -> Option<String> {
    string_field(record, "trust_status")
}

fn resources_overlap(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let Some(left_path) = left
        .strip_prefix("file:")
        .map(|value| value.trim_matches('/'))
    else {
        return false;
    };
    let Some(right_path) = right
        .strip_prefix("file:")
        .map(|value| value.trim_matches('/'))
    else {
        return false;
    };
    left_path.starts_with(&format!("{right_path}/"))
        || right_path.starts_with(&format!("{left_path}/"))
}

fn age_seconds(record: &Value, now: f64) -> Option<i64> {
    let epoch = record_epoch(record);
    (epoch > 0.0).then_some((now - epoch).max(0.0) as i64)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn payload_value<'a>(event: &'a Value, key: &str) -> Option<&'a Value> {
    event.get("payload").and_then(|payload| payload.get(key))
}

fn payload_string(event: &Value, key: &str) -> Option<String> {
    payload_value(event, key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn value_to_display_string(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
        return Some(value.to_string());
    }
    if let Some(value) = value.as_i64() {
        return Some(value.to_string());
    }
    if let Some(value) = value.as_u64() {
        return Some(value.to_string());
    }
    None
}
