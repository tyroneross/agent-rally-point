// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::query::{ActiveBlocker, ActiveClaim, ClaimConflict, ScoreFinding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnoseFinding {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub event_id: Option<String>,
    pub recommendation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub status: String,
    pub score: i64,
    pub findings: Vec<DiagnoseFinding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiagnoseOptions<'a> {
    pub state_records: Option<&'a [Value]>,
    pub tool: Option<&'a str>,
    pub stale_after_seconds: i64,
    pub since: Option<&'a str>,
    pub now_epoch_seconds: f64,
}

impl Default for DiagnoseOptions<'_> {
    fn default() -> Self {
        Self {
            state_records: None,
            tool: None,
            stale_after_seconds: 24 * 3600,
            since: None,
            now_epoch_seconds: crate::query::now_epoch_seconds(),
        }
    }
}

pub fn diagnose_records(records: &[Value], options: DiagnoseOptions<'_>) -> Diagnosis {
    let state = options.state_records.unwrap_or(records);
    // Build a throwaway graph in a temp dir to source the same queries
    // production callers use. diagnose_records doesn't have a channel
    // store available — it's called by `rally diagnose` with raw records
    // — so we materialize a per-call projection here.
    let temp_dir = std::env::temp_dir().join(format!(
        "rally-diagnose-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&temp_dir);
    let result = diagnose_via_graph(&temp_dir, records, state, &options);
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}

fn diagnose_via_graph(
    channel_dir: &Path,
    records: &[Value],
    state: &[Value],
    options: &DiagnoseOptions<'_>,
) -> Diagnosis {
    let now_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp(
        options.now_epoch_seconds as i64,
        0,
    )
    .map(|dt| dt.to_rfc3339())
    .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let Ok(mut conn) = crate::graph::init(channel_dir, &now_rfc) else {
        return Diagnosis {
            status: "stuck".to_string(),
            score: 0,
            findings: vec![DiagnoseFinding {
                severity: crate::severity::P1.to_string(),
                code: "graph-unavailable".to_string(),
                message: "could not open the graph projection".to_string(),
                event_id: None,
                recommendation: Some("rally graph rebuild --tool ci --json".to_string()),
            }],
        };
    };
    // Wrap raw events with synthetic local_seq so catch_up's gate doesn't
    // skip them. The test fixtures used by diagnose pass bare events;
    // production callers pass store entries that already have local_seq.
    let wrapped: Vec<Value> = state
        .iter()
        .enumerate()
        .map(|(i, record)| {
            if record.get("local_seq").is_some() {
                record.clone()
            } else if record.get("event").is_some() {
                let mut out = record.clone();
                if let Some(map) = out.as_object_mut() {
                    map.insert("local_seq".into(), serde_json::json!((i + 1) as i64));
                }
                out
            } else {
                serde_json::json!({
                    "local_seq": (i + 1) as i64,
                    "origin": "local",
                    "trust_status": "local",
                    "event": record
                })
            }
        })
        .collect();
    if crate::graph::catch_up(&mut conn, &wrapped, &now_rfc).is_err() {
        return Diagnosis {
            status: "stuck".to_string(),
            score: 0,
            findings: vec![DiagnoseFinding {
                severity: crate::severity::P1.to_string(),
                code: "graph-catch-up-failed".to_string(),
                message: "failed to apply records to graph projection".to_string(),
                event_id: None,
                recommendation: Some("rally graph rebuild --tool ci --json".to_string()),
            }],
        };
    }

    let (score, score_findings) = crate::graph::score(&conn, records, options.tool, options.now_epoch_seconds)
        .unwrap_or((0, Vec::new()));
    let mut findings: Vec<DiagnoseFinding> =
        score_findings.into_iter().map(from_score_finding).collect();

    if let Ok(blockers) = crate::graph::active_blockers_typed(&conn, options.tool, options.now_epoch_seconds) {
        for blocker in blockers {
            findings.push(from_blocker(blocker));
        }
    }
    if let Ok(conflicts) = crate::graph::claim_conflicts_typed(&conn) {
        for conflict in conflicts {
            findings.push(from_conflict(conflict, options.since));
        }
    }
    if let Ok(claims) = crate::graph::active_claims_typed(&conn, options.tool, options.now_epoch_seconds) {
        for claim in claims {
            if claim
                .age_seconds
                .is_some_and(|age| age >= options.stale_after_seconds)
            {
                findings.push(from_stale_claim(claim));
            }
        }
    }

    Diagnosis {
        status: if findings.is_empty() {
            "healthy".to_string()
        } else {
            "stuck".to_string()
        },
        score,
        findings,
    }
}

fn from_score_finding(finding: ScoreFinding) -> DiagnoseFinding {
    let recommendation = finding
        .event_id
        .as_ref()
        .map(|event_id| format!("rally thread {event_id}"))
        .or_else(|| Some("rally replay --since 2h".to_string()));
    DiagnoseFinding {
        severity: finding.severity,
        code: finding.code,
        message: finding.message,
        event_id: finding.event_id,
        recommendation,
    }
}

fn from_blocker(blocker: ActiveBlocker) -> DiagnoseFinding {
    DiagnoseFinding {
        severity: crate::severity::P1.to_string(),
        code: "active-blocker".to_string(),
        event_id: Some(blocker.event_id.clone()),
        message: format!(
            "blocker from {}: {}",
            blocker.tool.unwrap_or_else(|| "unknown".to_string()),
            blocker.subject
        ),
        recommendation: Some(format!("rally thread {}", blocker.event_id)),
    }
}

fn from_conflict(conflict: ClaimConflict, since: Option<&str>) -> DiagnoseFinding {
    DiagnoseFinding {
        severity: crate::severity::P1.to_string(),
        code: "claim-conflict".to_string(),
        event_id: conflict.claim_ids.first().cloned(),
        message: format!(
            "resource {} is claimed by {}",
            conflict.resource,
            conflict.owners.join(", ")
        ),
        recommendation: Some(
            since
                .map(|since| format!("rally conflicts --since {since}"))
                .unwrap_or_else(|| "rally conflicts".to_string()),
        ),
    }
}

fn from_stale_claim(claim: ActiveClaim) -> DiagnoseFinding {
    DiagnoseFinding {
        severity: crate::severity::P2.to_string(),
        code: "stale-claim".to_string(),
        event_id: Some(claim.event_id.clone()),
        message: format!(
            "claim on {} by {} is stale",
            claim.resource,
            claim.owner_tool.unwrap_or_else(|| "unknown".to_string())
        ),
        recommendation: Some(format!(
            "rally release {} --reason 'done or abandoned'",
            claim.event_id
        )),
    }
}
