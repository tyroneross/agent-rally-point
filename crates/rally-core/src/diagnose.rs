// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::query::{ActiveBlocker, ActiveClaim, ClaimConflict, ScoreFinding, TraceProjection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    let score_projection = TraceProjection::from_records(records);
    let state_projection = TraceProjection::from_records_at(state, options.now_epoch_seconds);
    let (score, score_findings) = score_projection.score(options.tool);
    let mut findings: Vec<DiagnoseFinding> =
        score_findings.into_iter().map(from_score_finding).collect();

    for blocker in state_projection.active_blockers(options.tool) {
        findings.push(from_blocker(blocker));
    }
    for conflict in state_projection.claim_conflicts() {
        findings.push(from_conflict(conflict, options.since));
    }
    for claim in state_projection.active_claims(options.tool) {
        if claim
            .age_seconds
            .is_some_and(|age| age >= options.stale_after_seconds)
        {
            findings.push(from_stale_claim(claim));
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
        severity: "P1".to_string(),
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
        severity: "P1".to_string(),
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
        severity: "P2".to_string(),
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
