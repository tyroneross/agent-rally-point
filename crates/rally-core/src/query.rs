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

#[derive(Clone, Debug)]
pub struct TraceProjection {
    records: Vec<ProjectedRecord>,
    known_ids: BTreeSet<String>,
    acked_handoffs: BTreeSet<String>,
    released_claims: BTreeSet<String>,
    resolved_blockers: BTreeSet<String>,
    final_ack_by_handoff: BTreeMap<String, String>,
    now: f64,
}

#[derive(Clone, Debug)]
struct ProjectedRecord {
    parsed: EventRecord,
    id: String,
    aliases: BTreeSet<String>,
    origin: Option<String>,
    trust_status: Option<String>,
    age_seconds: Option<i64>,
}

impl TraceProjection {
    pub fn from_records(records: &[Value]) -> Self {
        Self::from_records_at(records, now_epoch_seconds())
    }

    pub fn from_records_at(records: &[Value], now: f64) -> Self {
        let records = records
            .iter()
            .filter_map(|record| {
                let parsed = EventRecord::parse(record).ok()?;
                Some(ProjectedRecord {
                    parsed,
                    id: record_id(record),
                    aliases: record_aliases(record),
                    origin: record_origin(record),
                    trust_status: record_trust_status(record),
                    age_seconds: age_seconds(record, now),
                })
            })
            .collect::<Vec<_>>();

        let known_ids = records
            .iter()
            .flat_map(|record| record.aliases.iter().cloned())
            .collect();
        let acked_handoffs = referenced_ids_for(&records, EventKind::Ack);
        let released_claims = referenced_ids_for(&records, EventKind::ClaimRelease);
        let resolved_blockers = referenced_ids_for(&records, EventKind::BlockerResolved);
        let final_ack_by_handoff = final_ack_by_handoff_for(&records);

        Self {
            records,
            known_ids,
            acked_handoffs,
            released_claims,
            resolved_blockers,
            final_ack_by_handoff,
            now,
        }
    }

    pub fn pending_handoffs(&self, tool: Option<&str>) -> Vec<PendingHandoff> {
        self.records
            .iter()
            .filter_map(|record| {
                let Some(EventPayload::Handoff(payload)) = record.parsed.payload.as_ref() else {
                    return None;
                };
                if !payload.requires_ack || !record.aliases.is_disjoint(&self.acked_handoffs) {
                    return None;
                }
                let to_tool = payload.to_tool.clone();
                let from_tool = payload.from_tool.clone().or(record.parsed.tool.clone());
                if let Some(tool) = tool {
                    if !matches!(to_tool.as_deref(), Some(value) if value == tool || value == "all")
                        && to_tool.is_some()
                    {
                        return None;
                    }
                    if from_tool.as_deref() == Some(tool) {
                        return None;
                    }
                }
                Some(PendingHandoff {
                    event_id: record.id.clone(),
                    thread_id: record.parsed.thread_id.clone(),
                    origin: record.origin.clone(),
                    trust_status: record.trust_status.clone(),
                    from_tool,
                    to_tool,
                    subject: payload.subject.clone(),
                    age_seconds: record.age_seconds,
                    files: payload.ref_files.clone(),
                })
            })
            .collect()
    }

    pub fn active_claims(&self, tool: Option<&str>) -> Vec<ActiveClaim> {
        self.records
            .iter()
            .filter_map(|record| {
                let Some(EventPayload::Claim(payload)) = record.parsed.payload.as_ref() else {
                    return None;
                };
                if !record.aliases.is_disjoint(&self.released_claims) {
                    return None;
                }
                let owner = Some(payload.owner_tool.clone());
                if tool.is_some() && owner.as_deref() != tool {
                    return None;
                }
                Some(ActiveClaim {
                    event_id: record.id.clone(),
                    thread_id: record.parsed.thread_id.clone(),
                    origin: record.origin.clone(),
                    trust_status: record.trust_status.clone(),
                    owner_tool: owner,
                    resource: payload.resource.clone(),
                    subject: payload.subject.clone(),
                    age_seconds: record.age_seconds,
                })
            })
            .collect()
    }

    pub fn active_blockers(&self, tool: Option<&str>) -> Vec<ActiveBlocker> {
        self.records
            .iter()
            .filter_map(|record| {
                let Some(EventPayload::Blocker(payload)) = record.parsed.payload.as_ref() else {
                    return None;
                };
                if !record.aliases.is_disjoint(&self.resolved_blockers) {
                    return None;
                }
                let producer = record.parsed.tool.clone();
                if tool.is_some() && producer.as_deref() != tool {
                    return None;
                }
                Some(ActiveBlocker {
                    event_id: record.id.clone(),
                    thread_id: record.parsed.thread_id.clone(),
                    origin: record.origin.clone(),
                    trust_status: record.trust_status.clone(),
                    tool: producer,
                    subject: payload.subject.clone(),
                    resource: payload.resource.clone(),
                    severity: payload
                        .severity
                        .clone()
                        .unwrap_or_else(|| "blocked".to_string()),
                    age_seconds: record.age_seconds,
                })
            })
            .collect()
    }

    pub fn claim_conflicts(&self) -> Vec<ClaimConflict> {
        claim_conflicts_for(self.active_claims(None))
    }

    pub fn score(&self, tool: Option<&str>) -> (i64, Vec<ScoreFinding>) {
        let mut findings = Vec::new();
        for item in self.pending_handoffs(tool) {
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

        for record in &self.records {
            if let Some(causation) = record
                .parsed
                .causation_id
                .as_ref()
                .filter(|value| !value.is_empty())
            {
                if !self.known_ids.contains(causation) {
                    findings.push(ScoreFinding {
                        severity: "P2".to_string(),
                        code: "dangling-causation".to_string(),
                        event_id: Some(record.id.clone()),
                        message: format!(
                            "causation_id {causation} does not resolve in this trace window"
                        ),
                    });
                }
            }
            if let Some(EventPayload::Ack(payload) | EventPayload::Feedback(payload)) =
                record.parsed.payload.as_ref()
            {
                if !payload.ref_handoff_id.is_empty()
                    && !self.known_ids.contains(&payload.ref_handoff_id)
                {
                    findings.push(ScoreFinding {
                        severity: "P2".to_string(),
                        code: "dangling-reference".to_string(),
                        event_id: Some(record.id.clone()),
                        message: format!(
                            "{} references missing handoff/event {}",
                            record.parsed.kind.label(),
                            payload.ref_handoff_id
                        ),
                    });
                }
            }
        }

        for (reference, verdict) in &self.final_ack_by_handoff {
            if verdict == "needs-info" {
                findings.push(ScoreFinding {
                    severity: "P2".to_string(),
                    code: "unresolved-needs-info".to_string(),
                    event_id: Some(reference.clone()),
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

    pub fn now(&self) -> f64 {
        self.now
    }
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
    TraceProjection::from_records(records).pending_handoffs(tool)
}

pub fn pending_handoffs_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<PendingHandoff> {
    TraceProjection::from_records_at(records, now).pending_handoffs(tool)
}

pub fn active_claims(records: &[Value], tool: Option<&str>) -> Vec<ActiveClaim> {
    TraceProjection::from_records(records).active_claims(tool)
}

pub fn active_claims_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<ActiveClaim> {
    TraceProjection::from_records_at(records, now).active_claims(tool)
}

pub fn claim_conflicts(records: &[Value]) -> Vec<ClaimConflict> {
    TraceProjection::from_records(records).claim_conflicts()
}

fn claim_conflicts_for(claims: Vec<ActiveClaim>) -> Vec<ClaimConflict> {
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
    TraceProjection::from_records(records).active_blockers(tool)
}

pub fn active_blockers_at(records: &[Value], tool: Option<&str>, now: f64) -> Vec<ActiveBlocker> {
    TraceProjection::from_records_at(records, now).active_blockers(tool)
}

pub fn score_records(records: &[Value], tool: Option<&str>) -> (i64, Vec<ScoreFinding>) {
    TraceProjection::from_records(records).score(tool)
}

fn referenced_ids_for(records: &[ProjectedRecord], kind: EventKind) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| record.parsed.kind == kind)
        .filter_map(|record| match record.parsed.payload.as_ref()? {
            EventPayload::Ack(payload) | EventPayload::Feedback(payload) => {
                Some(payload.ref_handoff_id.clone())
            }
            EventPayload::ClaimRelease(payload) => Some(payload.ref_claim_id.clone()),
            EventPayload::BlockerResolved(payload) => Some(payload.ref_blocker_id.clone()),
            _ => None,
        })
        .collect()
}

fn final_ack_by_handoff_for(records: &[ProjectedRecord]) -> BTreeMap<String, String> {
    let mut latest = BTreeMap::new();
    for record in records {
        if record.parsed.kind != EventKind::Ack {
            continue;
        }
        if let Some(EventPayload::Ack(payload)) = record.parsed.payload.as_ref() {
            latest.insert(payload.ref_handoff_id.clone(), payload.verdict.clone());
        }
    }
    latest
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
