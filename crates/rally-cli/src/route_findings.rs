// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Route findings — canonical-path-match each `{file, severity, description,
//! evidence}` finding to an active claim's owner, then post a typed `handoff`
//! fact to that tool. Unclaimed files get a `risk` fact tagged `unowned`.
//!
//! Requires `--verified` flag: the caller must affirm that FP-adjudication has
//! already happened. Without `--verified`, the command refuses with an error.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{RallyError, Result};
use crate::store::{Fact, FactKind, RoomStore};
use crate::{FACT_SCHEMA, new_id, normalize_path, now_string, paths_suffix_collide};

// ─── Input types ─────────────────────────────────────────────────────────────

/// One finding from the input JSON array.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Finding {
    pub(crate) file: String,
    pub(crate) severity: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) evidence: Vec<String>,
}

// ─── Output types ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RoutedFinding {
    pub(crate) file: String,
    pub(crate) severity: String,
    pub(crate) description: String,
    pub(crate) routed_to: Option<String>,
    pub(crate) fact_kind: String,
    pub(crate) event_id: String,
    pub(crate) unowned: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RoutingSummary {
    pub(crate) findings_total: usize,
    pub(crate) routed: usize,
    pub(crate) unowned: usize,
    pub(crate) routed_findings: Vec<RoutedFinding>,
    pub(crate) artifact_event_id: String,
}

// ─── Path-matching helpers ────────────────────────────────────────────────────

/// Given the normalized finding path and the normalized scope entry, return
/// true if the scope "covers" this path.
fn scope_matches_finding(scope: &str, finding_path: &str) -> bool {
    // Exact or prefix match (scope is a directory prefix)
    if scope == finding_path {
        return true;
    }
    // Strip file: prefix for comparison
    let s = scope.strip_prefix("file:").unwrap_or(scope);
    let f = finding_path.strip_prefix("file:").unwrap_or(finding_path);
    if s == f {
        return true;
    }
    // Directory prefix: scope is ancestor of finding
    if f.starts_with(&format!("{s}/")) {
        return true;
    }
    // Suffix collision (2-component shared suffix)
    paths_suffix_collide(s, f)
}

/// Find the owner tool for a finding path by checking active_claims scopes.
fn find_owner<'a>(finding_path: &str, active_claims: &'a [Fact]) -> Option<&'a str> {
    for claim in active_claims {
        for scope in &claim.scope {
            if scope == "external-intake" {
                continue;
            }
            if scope_matches_finding(scope, finding_path) {
                return claim.tool.as_deref();
            }
        }
    }
    None
}

// ─── Core routing ─────────────────────────────────────────────────────────────

pub(crate) fn route_findings(
    room: &RoomStore,
    sender_tool: &str,
    findings: Vec<Finding>,
    verified: bool,
) -> Result<RoutingSummary> {
    if !verified {
        return Err(RallyError::Usage(
            "route-findings refused: --verified flag is required to confirm FP-adjudication has already happened. Re-run with --verified after reviewing findings for false positives.".to_string(),
        ));
    }

    let snapshot = room.snapshot()?;
    let active_claims = &snapshot.active_claims;

    let total = findings.len();
    let mut routed_count = 0usize;
    let mut unowned_count = 0usize;
    let mut routed_findings: Vec<RoutedFinding> = Vec::new();

    for finding in &findings {
        let normalized = normalize_path(finding.file.clone());

        let owner = find_owner(&normalized, active_claims);

        let (fact_kind, fact) = if let Some(owner_tool) = owner {
            // Route to owner via handoff fact
            let fact = Fact {
                from_session_id: None,
                principal_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("finding"),
                seq: 0,
                thread_id: new_id("findings"),
                kind: FactKind::Handoff,
                tool: Some(sender_tool.to_string()),
                role: None,
                subject: format!(
                    "finding routed to {}: [{}] {}",
                    owner_tool, finding.severity, finding.description
                ),
                scope: vec![normalized.clone()],
                created_at: now_string(),
                summary: Some(format!(
                    "file: {}\nseverity: {}\ndescription: {}",
                    finding.file, finding.severity, finding.description
                )),
                evidence: finding.evidence.clone(),
                target: Some(owner_tool.to_string()),
                ref_id: None,
                status: Some("pending".to_string()),
                severity: Some(finding.severity.clone()),
                uri: None,
                session: None,
            };
            routed_count += 1;
            ("handoff", room.append_fact_verified(&fact)?)
        } else {
            // No active claim owner — emit a risk fact tagged unowned
            let fact = Fact {
                from_session_id: None,
                principal_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("finding"),
                seq: 0,
                thread_id: new_id("findings"),
                kind: FactKind::Risk,
                tool: Some(sender_tool.to_string()),
                role: None,
                subject: format!(
                    "unowned finding: [{}] {} in {}",
                    finding.severity, finding.description, finding.file
                ),
                scope: vec![normalized.clone(), "unowned".to_string()],
                created_at: now_string(),
                summary: Some(format!(
                    "file: {}\nseverity: {}\ndescription: {}\nNo active claim covers this path.",
                    finding.file, finding.severity, finding.description
                )),
                evidence: finding.evidence.clone(),
                target: None,
                ref_id: None,
                status: None,
                severity: Some(finding.severity.clone()),
                uri: None,
                session: None,
            };
            unowned_count += 1;
            ("risk", room.append_fact_verified(&fact)?)
        };

        routed_findings.push(RoutedFinding {
            file: finding.file.clone(),
            severity: finding.severity.clone(),
            description: finding.description.clone(),
            routed_to: owner.map(str::to_string),
            fact_kind: fact_kind.to_string(),
            event_id: fact.event_id.clone(),
            unowned: owner.is_none(),
        });
    }

    // Emit a routing-summary artifact fact
    let summary_fact = Fact {
        from_session_id: None,
        principal_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: new_id("findings-summary"),
        seq: 0,
        thread_id: new_id("findings"),
        kind: FactKind::Artifact,
        tool: Some(sender_tool.to_string()),
        role: None,
        subject: format!(
            "findings routing summary: {total} total, {routed_count} routed, {unowned_count} unowned"
        ),
        scope: vec!["findings-routing".to_string()],
        created_at: now_string(),
        summary: Some(format!(
            "total={total} routed={routed_count} unowned={unowned_count}"
        )),
        evidence: routed_findings
            .iter()
            .map(|r| format!("{}:{}", r.file, r.fact_kind))
            .collect(),
        target: None,
        ref_id: None,
        status: None,
        severity: None,
        uri: None,
        session: None,
    };
    let summary_appended = room.append_fact_verified(&summary_fact)?;

    Ok(RoutingSummary {
        findings_total: total,
        routed: routed_count,
        unowned: unowned_count,
        routed_findings,
        artifact_event_id: summary_appended.event_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Fact, FactKind, RoomStore};
    use crate::{FACT_SCHEMA, new_id, now_string};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_room() -> (RoomStore, std::path::PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-rf-test-{nanos}"));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let room = RoomStore::open_at(root.clone()).unwrap();
        (room, root)
    }

    fn add_claim(room: &RoomStore, tool: &str, path: &str) -> Fact {
        let fact = Fact {
            from_session_id: None,
            principal_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("claim"),
            seq: 0,
            thread_id: new_id("thread"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim {path}"),
            scope: vec![format!("file:{path}")],
            created_at: now_string(),
            summary: None,
            evidence: vec![],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    #[test]
    fn route_findings_refuses_without_verified() {
        let (room, root) = test_room();
        let findings = vec![Finding {
            file: "src/lib.rs".to_string(),
            severity: "error".to_string(),
            description: "null deref".to_string(),
            evidence: vec![],
        }];
        let result = route_findings(&room, "scanner", findings, false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("--verified"),
            "error should mention --verified: {msg}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn route_findings_maps_finding_to_claim_owner() {
        let (room, root) = test_room();
        add_claim(&room, "tool-a", "src/lib.rs");

        let findings = vec![Finding {
            file: "src/lib.rs".to_string(),
            severity: "error".to_string(),
            description: "null deref at line 42".to_string(),
            evidence: vec!["static analysis".to_string()],
        }];
        let summary = route_findings(&room, "scanner", findings, true).unwrap();

        assert_eq!(summary.findings_total, 1);
        assert_eq!(summary.routed, 1);
        assert_eq!(summary.unowned, 0);
        let rf = &summary.routed_findings[0];
        assert_eq!(rf.routed_to.as_deref(), Some("tool-a"));
        assert_eq!(rf.fact_kind, "handoff");
        assert!(!rf.unowned);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn route_findings_emits_risk_for_unclaimed_path() {
        let (room, root) = test_room();
        // No claim for src/main.rs

        let findings = vec![Finding {
            file: "src/main.rs".to_string(),
            severity: "warn".to_string(),
            description: "unused variable".to_string(),
            evidence: vec![],
        }];
        let summary = route_findings(&room, "scanner", findings, true).unwrap();

        assert_eq!(summary.findings_total, 1);
        assert_eq!(summary.routed, 0);
        assert_eq!(summary.unowned, 1);
        let rf = &summary.routed_findings[0];
        assert!(rf.unowned);
        assert_eq!(rf.fact_kind, "risk");
        assert!(rf.routed_to.is_none());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn route_findings_suffix_match_routes_to_owner() {
        let (room, root) = test_room();
        // Claim covers the longer canonical path
        add_claim(&room, "tool-b", "crates/rally-cli/src/lib.rs");

        // Finding comes in as the shorter path — suffix match should link them
        let findings = vec![Finding {
            file: "src/lib.rs".to_string(),
            severity: "warn".to_string(),
            description: "ambiguous suffix".to_string(),
            evidence: vec![],
        }];
        let summary = route_findings(&room, "scanner", findings, true).unwrap();

        // suffix collision means it routes to tool-b
        assert_eq!(summary.routed, 1);
        assert_eq!(
            summary.routed_findings[0].routed_to.as_deref(),
            Some("tool-b")
        );

        std::fs::remove_dir_all(root).ok();
    }
}
