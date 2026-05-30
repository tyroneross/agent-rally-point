use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{RallyError, Result};
use crate::path_matches_scope;
use crate::paths_suffix_collide;
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
        other => {
            return Err(RallyError::Usage(format!(
                "unsupported check phase {other}"
            )));
        }
    }
    let stop = findings.iter().any(|finding| finding.severity == "stop");
    let allow = !stop;
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
    if path.is_none() {
        findings.push(CheckFinding {
            code: "missing-path",
            severity: "warn",
            message: "before-write checks are stronger with --path".to_string(),
            fact_id: None,
            owner: None,
            path: None,
            scope: Vec::new(),
        });
    }
    if let Some(path) = path {
        for claim in &snapshot.active_claims {
            let is_different_tool = claim.tool.as_deref() != Some(tool);
            let exact_or_dir = claim
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path));

            if exact_or_dir && is_different_tool {
                findings.push(CheckFinding {
                    code: "claimed-path",
                    severity: "stop",
                    message: "another agent has claimed this path".to_string(),
                    fact_id: Some(claim.event_id.clone()),
                    owner: claim.tool.clone(),
                    path: Some(path.to_string()),
                    scope: Vec::new(),
                });
            } else if is_different_tool {
                // Suffix-collision: same file reached via a different path form.
                // Does NOT hard-block — emits a WARN for the lead to adjudicate.
                for scope in &claim.scope {
                    if paths_suffix_collide(scope, path) {
                        findings.push(CheckFinding {
                            code: "ambiguous-path-collision",
                            severity: "warn",
                            message: format!(
                                "submitted path '{}' may refer to the same file as claimed \
                                 path '{}' held by {} — lead should verify before writing",
                                path,
                                scope,
                                claim.tool.as_deref().unwrap_or("unknown"),
                            ),
                            fact_id: Some(claim.event_id.clone()),
                            owner: claim.tool.clone(),
                            path: Some(path.to_string()),
                            scope: Vec::new(),
                        });
                        // One warning per claim is enough; don't fan out across scopes.
                        break;
                    }
                }
            }
        }
    }
    for decision in &snapshot.current_decisions {
        if decision.scope.is_empty()
            || path.is_some_and(|path| {
                decision
                    .scope
                    .iter()
                    .any(|scope| path_matches_scope(scope, path))
            })
        {
            findings.push(CheckFinding {
                code: "binding-decision",
                severity: "info",
                message: decision.subject.clone(),
                fact_id: Some(decision.event_id.clone()),
                owner: None,
                path: path.map(str::to_string),
                scope: Vec::new(),
            });
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.scope.is_empty()
            || path.is_some_and(|path| {
                blocker
                    .scope
                    .iter()
                    .any(|scope| path_matches_scope(scope, path))
            })
        {
            findings.push(CheckFinding {
                code: "active-blocker",
                severity: "stop",
                message: blocker.subject.clone(),
                fact_id: Some(blocker.event_id.clone()),
                owner: None,
                path: path.map(str::to_string),
                scope: Vec::new(),
            });
        }
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
