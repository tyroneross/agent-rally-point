use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{RallyError, Result};
use crate::path_matches_scope;
use crate::store::RoomSnapshot;

#[derive(JsonSchema, Serialize)]
pub(crate) struct CheckData {
    check: CheckResult,
}

#[derive(JsonSchema, Serialize)]
struct CheckResult {
    phase: String,
    tool: String,
    path: Option<String>,
    allow: bool,
    mode: &'static str,
    findings: Vec<CheckFinding>,
    agent_visible: AgentVisible,
}

#[derive(JsonSchema, Serialize)]
struct AgentVisible {
    present: bool,
    severity: &'static str,
    message: &'static str,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct CheckFinding {
    code: &'static str,
    severity: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scope: Vec<String>,
}

pub(crate) struct CheckOutcome {
    pub(crate) data: CheckData,
    pub(crate) exit_code: u8,
    pub(crate) finding_count: usize,
}

pub(crate) fn build_check(
    phase: String,
    tool: String,
    path: Option<String>,
    strict: bool,
    snapshot: &RoomSnapshot,
) -> Result<CheckOutcome> {
    let mut findings = Vec::new();
    match phase.as_str() {
        "before-write" => check_before_write(snapshot, &tool, path.as_deref(), &mut findings),
        "before-complete" => check_before_complete(snapshot, &tool, &mut findings),
        "ci" => check_ci(snapshot, &mut findings),
        other => {
            return Err(RallyError::Usage(format!(
                "unsupported check phase {other}"
            )));
        }
    }
    let stop = findings.iter().any(|finding| finding.severity == "stop");
    let allow = !stop || !strict;
    let exit_code = if strict && stop { 4 } else { 0 };
    let finding_count = findings.len();
    Ok(CheckOutcome {
        data: CheckData {
            check: CheckResult {
                phase,
                tool,
                path,
                allow,
                mode: if strict { "strict" } else { "warn" },
                findings,
                agent_visible: AgentVisible {
                    present: stop,
                    severity: if stop { "stop" } else { "info" },
                    message: if stop {
                        "Rally check found room facts that should stop or redirect this write."
                    } else {
                        "Rally check passed."
                    },
                },
            },
        },
        exit_code,
        finding_count,
    })
}

fn check_before_write(
    snapshot: &RoomSnapshot,
    tool: &str,
    path: Option<&str>,
    findings: &mut Vec<CheckFinding>,
) {
    let Some(path) = path else {
        findings.push(CheckFinding {
            code: "missing-path",
            severity: "warn",
            message: "before-write checks are stronger with --path".to_string(),
            fact_id: None,
            owner: None,
            path: None,
            scope: Vec::new(),
        });
        return;
    };
    for claim in &snapshot.active_claims {
        if claim
            .scope
            .iter()
            .any(|scope| path_matches_scope(scope, path))
            && claim.tool.as_deref() != Some(tool)
        {
            findings.push(CheckFinding {
                code: "claimed-path",
                severity: "stop",
                message: "another agent has claimed this path".to_string(),
                fact_id: Some(claim.event_id.clone()),
                owner: claim.tool.clone(),
                path: Some(path.to_string()),
                scope: Vec::new(),
            });
        }
    }
    for decision in &snapshot.current_decisions {
        if decision
            .scope
            .iter()
            .any(|scope| path_matches_scope(scope, path))
            || decision.scope.is_empty()
        {
            findings.push(CheckFinding {
                code: "binding-decision",
                severity: "info",
                message: decision.subject.clone(),
                fact_id: Some(decision.event_id.clone()),
                owner: None,
                path: Some(path.to_string()),
                scope: Vec::new(),
            });
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.scope.is_empty()
            || blocker
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path))
        {
            findings.push(CheckFinding {
                code: "active-blocker",
                severity: "stop",
                message: blocker.subject.clone(),
                fact_id: Some(blocker.event_id.clone()),
                owner: None,
                path: Some(path.to_string()),
                scope: Vec::new(),
            });
        }
    }
    // Nudge the agent to declare what this change produces/depends on, so peers
    // get a predictive stale-base warning. Informational — never blocks a write.
    findings.push(CheckFinding {
        code: "declare-contracts",
        severity: "info",
        message: "declare what this change produces/depends on: \
            rally say claim --tool <you> --produces <contract> --depends <contract>@<hash>"
            .to_string(),
        fact_id: None,
        owner: None,
        path: Some(path.to_string()),
        scope: Vec::new(),
    });
}

/// CI gate: inspect the whole room and stop a merge on unreconciled coordination
/// state. `confirmed` stale bases and active blockers are hard stops; in-flight
/// (incomplete) handoffs are warnings. Run as `rally check ci --strict`.
fn check_ci(snapshot: &RoomSnapshot, findings: &mut Vec<CheckFinding>) {
    for stale in crate::next::stale_base_findings(&snapshot.active_claims) {
        if !stale.confirmed {
            continue;
        }
        findings.push(CheckFinding {
            code: "confirmed-stale-base",
            severity: "stop",
            message: format!(
                "{} changes contract {} that {} depends on (pins differ)",
                stale.producer_tool.as_deref().unwrap_or("a peer"),
                stale.contract,
                stale.consumer_tool.as_deref().unwrap_or("another agent"),
            ),
            fact_id: Some(stale.consumer_event_id.clone()),
            owner: stale.producer_tool.clone(),
            path: None,
            scope: vec![stale.contract.clone()],
        });
    }
    for blocker in &snapshot.active_blockers {
        findings.push(CheckFinding {
            code: "active-blocker",
            severity: "stop",
            message: blocker.subject.clone(),
            fact_id: Some(blocker.event_id.clone()),
            owner: blocker.tool.clone(),
            path: None,
            scope: blocker.scope.clone(),
        });
    }
    for handoff in &snapshot.open_handoffs {
        findings.push(CheckFinding {
            code: "open-handoff",
            severity: "warn",
            message: format!("delegated work is not complete: {}", handoff.subject),
            fact_id: Some(handoff.event_id.clone()),
            owner: handoff.target.clone(),
            path: None,
            scope: Vec::new(),
        });
    }
}

fn check_before_complete(snapshot: &RoomSnapshot, tool: &str, findings: &mut Vec<CheckFinding>) {
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() == Some(tool) {
            findings.push(CheckFinding {
                code: "owned-active-claim",
                severity: "stop",
                message: "release or explain this active claim before completion".to_string(),
                fact_id: Some(claim.event_id.clone()),
                owner: None,
                path: None,
                scope: claim.scope.clone(),
            });
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.tool.as_deref() == Some(tool) {
            findings.push(CheckFinding {
                code: "owned-active-blocker",
                severity: "warn",
                message: "completion still has an active blocker from this tool".to_string(),
                fact_id: Some(blocker.event_id.clone()),
                owner: None,
                path: None,
                scope: Vec::new(),
            });
        }
    }
}
