// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! #9 Tier-fit advisory.
//!
//! `rally check tier-fit --role <role> [--proposed-tier <t>] [--json]`
//!
//! HOST-RELATIVE: never hardcodes model names or tier ranks. Compares a
//! proposed tier string against a room-resident calibration fact
//! (a `decision` fact tagged `tier-calibration` in its scope or subject).
//!
//! If proposed tier exceeds the cheapest-sufficient tier recorded in the
//! calibration, emits an info-severity `tier_mismatch` finding.
//! Never blocks. If no calibration fact exists, returns a neutral
//! "no calibration" result.

use crate::store::RoomSnapshot;
use schemars::JsonSchema;
use serde::Serialize;

/// Outcome of a tier-fit check.
#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct TierFitResult {
    /// Advisory only — never causes a non-zero exit code.
    pub(crate) advisory: bool,
    /// "ok" | "mismatch" | "no_calibration"
    pub(crate) status: String,
    /// Human-readable explanation.
    pub(crate) message: String,
    /// The proposed tier string (echoed for traceability).
    pub(crate) proposed_tier: Option<String>,
    /// The calibrated cheapest-sufficient tier for this role, if found.
    pub(crate) calibrated_tier: Option<String>,
    /// The role this check was run for.
    pub(crate) role: String,
    /// The event_id of the calibration fact used, if any.
    pub(crate) calibration_fact_id: Option<String>,
    /// A `tier_mismatch` finding when the proposed tier exceeds the calibration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finding: Option<TierFitFinding>,
}

#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct TierFitFinding {
    pub(crate) code: &'static str,
    pub(crate) severity: &'static str,
    pub(crate) message: String,
}

/// Run the tier-fit advisory check.
///
/// - `role`: the role label to look up in the calibration fact.
/// - `proposed_tier`: the tier string the caller wants to use (e.g. "opus", "sonnet").
/// - `snapshot`: current room snapshot, queried for `tier-calibration` decision facts.
///
/// The calibration fact is a `decision` fact whose `subject` or `scope` contains
/// `tier-calibration`. Its `summary` encodes entries as
/// `role:<role>=cheapest:<tier>` (one per line or semicolon-separated). Example:
///   `role:executor=cheapest:sonnet; role:planner=cheapest:opus`
///
/// If no calibration fact is found, returns `status: "no_calibration"` (neutral).
/// Never panics, never blocks.
pub(crate) fn check_tier_fit(
    role: &str,
    proposed_tier: Option<&str>,
    snapshot: &RoomSnapshot,
) -> TierFitResult {
    // Find the most recent `tier-calibration` decision fact.
    let calibration = snapshot
        .current_decisions
        .iter()
        .filter(|f| {
            f.subject.contains("tier-calibration")
                || f.scope.iter().any(|s| s.contains("tier-calibration"))
        })
        .max_by_key(|f| f.seq);

    let Some(cal_fact) = calibration else {
        return TierFitResult {
            advisory: true,
            status: "no_calibration".to_string(),
            message: "no tier-calibration decision fact found in room; advisory skipped"
                .to_string(),
            proposed_tier: proposed_tier.map(str::to_string),
            calibrated_tier: None,
            role: role.to_string(),
            calibration_fact_id: None,
            finding: None,
        };
    };

    let cal_id = cal_fact.event_id.clone();

    // Parse `role:<role>=cheapest:<tier>` entries from summary.
    // Also accept them in evidence entries.
    let cheapest = parse_calibration_for_role(role, cal_fact);

    let Some(cheapest_tier) = cheapest else {
        return TierFitResult {
            advisory: true,
            status: "no_calibration".to_string(),
            message: format!(
                "tier-calibration fact found ({}), but no entry for role '{role}'; advisory skipped",
                cal_id
            ),
            proposed_tier: proposed_tier.map(str::to_string),
            calibrated_tier: None,
            role: role.to_string(),
            calibration_fact_id: Some(cal_id),
            finding: None,
        };
    };

    let Some(proposed) = proposed_tier else {
        // No proposed tier: just report the calibration, no mismatch possible.
        return TierFitResult {
            advisory: true,
            status: "ok".to_string(),
            message: format!(
                "calibrated cheapest-sufficient tier for role '{role}' is '{cheapest_tier}'; no proposed tier to compare"
            ),
            proposed_tier: None,
            calibrated_tier: Some(cheapest_tier),
            role: role.to_string(),
            calibration_fact_id: Some(cal_id),
            finding: None,
        };
    };

    // Compare: if proposed is the same as calibrated cheapest, it's fine.
    // The host calibration uses ordinal strings; we compare as-recorded.
    // Mismatch = proposed != cheapest_tier (host decides what "exceeds" means;
    // we surface the discrepancy neutrally without ranking tiers ourselves).
    if proposed == cheapest_tier {
        TierFitResult {
            advisory: true,
            status: "ok".to_string(),
            message: format!(
                "proposed tier '{proposed}' matches calibrated cheapest-sufficient tier '{cheapest_tier}' for role '{role}'"
            ),
            proposed_tier: Some(proposed.to_string()),
            calibrated_tier: Some(cheapest_tier),
            role: role.to_string(),
            calibration_fact_id: Some(cal_id),
            finding: None,
        }
    } else {
        let msg = format!(
            "proposed tier '{proposed}' differs from calibrated cheapest-sufficient tier '{cheapest_tier}' for role '{role}'; consider using '{cheapest_tier}' — advisory only"
        );
        TierFitResult {
            advisory: true,
            status: "mismatch".to_string(),
            message: msg.clone(),
            proposed_tier: Some(proposed.to_string()),
            calibrated_tier: Some(cheapest_tier),
            role: role.to_string(),
            calibration_fact_id: Some(cal_id),
            finding: Some(TierFitFinding {
                code: "tier_mismatch",
                severity: "info",
                message: msg,
            }),
        }
    }
}

/// Parse the cheapest-sufficient tier for `role` from a calibration fact.
///
/// Supported encodings (checked in order):
/// 1. `summary` text with `role:<role>=cheapest:<tier>` entries (space/; separated).
/// 2. `evidence` entries with the same pattern.
fn parse_calibration_for_role(role: &str, fact: &crate::store::Fact) -> Option<String> {
    let needle = format!("role:{role}=cheapest:");

    // Check summary.
    if let Some(summary) = &fact.summary {
        if let Some(tier) = extract_tier_from_text(summary, &needle) {
            return Some(tier);
        }
    }

    // Check evidence entries.
    for ev in &fact.evidence {
        if let Some(tier) = extract_tier_from_text(ev, &needle) {
            return Some(tier);
        }
    }

    None
}

fn extract_tier_from_text(text: &str, needle: &str) -> Option<String> {
    // Split on whitespace, commas, and semicolons to tokenize entries.
    for token in text.split([' ', ';', ',', '\n', '\r', '\t']) {
        let token = token.trim();
        if let Some(rest) = token.strip_prefix(needle) {
            let tier: String = rest
                .chars()
                .take_while(|&c| c.is_alphanumeric() || c == '-' || c == '_')
                .collect();
            if !tier.is_empty() {
                return Some(tier);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Fact, FactKind, RoomSnapshot};

    fn make_calibration_fact(summary: &str, event_id: &str) -> Fact {
        Fact {
            from_session_id: None,
            principal_id: None,
            schema: "agent-rally.fact.v1".to_string(),
            event_id: event_id.to_string(),
            seq: 1,
            thread_id: "t1".to_string(),
            kind: FactKind::Decision,
            tool: Some("lead".to_string()),
            role: None,
            subject: "tier-calibration".to_string(),
            scope: vec!["tier-calibration".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
            summary: Some(summary.to_string()),
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    fn snapshot_with_decision(fact: Fact) -> RoomSnapshot {
        RoomSnapshot {
            current_decisions: vec![fact],
            ..Default::default()
        }
    }

    #[test]
    fn tier_fit_no_calibration_fact_returns_neutral() {
        let snapshot = RoomSnapshot::default();
        let result = check_tier_fit("executor", Some("opus"), &snapshot);
        assert_eq!(result.status, "no_calibration");
        assert!(result.finding.is_none());
        assert!(result.advisory);
    }

    #[test]
    fn tier_fit_matching_tier_returns_ok() {
        let cal = make_calibration_fact("role:executor=cheapest:sonnet", "cal-001");
        let snapshot = snapshot_with_decision(cal);
        let result = check_tier_fit("executor", Some("sonnet"), &snapshot);
        assert_eq!(result.status, "ok");
        assert!(result.finding.is_none());
        assert_eq!(result.calibrated_tier.as_deref(), Some("sonnet"));
    }

    #[test]
    fn tier_fit_mismatch_emits_finding() {
        let cal = make_calibration_fact("role:executor=cheapest:sonnet", "cal-002");
        let snapshot = snapshot_with_decision(cal);
        let result = check_tier_fit("executor", Some("opus"), &snapshot);
        assert_eq!(result.status, "mismatch");
        assert!(result.finding.is_some());
        let finding = result.finding.unwrap();
        assert_eq!(finding.code, "tier_mismatch");
        assert_eq!(finding.severity, "info");
        assert!(finding.message.contains("opus"));
        assert!(finding.message.contains("sonnet"));
    }

    #[test]
    fn tier_fit_no_proposed_tier_returns_ok_with_calibration() {
        let cal = make_calibration_fact("role:planner=cheapest:opus", "cal-003");
        let snapshot = snapshot_with_decision(cal);
        let result = check_tier_fit("planner", None, &snapshot);
        assert_eq!(result.status, "ok");
        assert_eq!(result.calibrated_tier.as_deref(), Some("opus"));
        assert!(result.finding.is_none());
    }

    #[test]
    fn tier_fit_calibration_fact_present_but_no_entry_for_role_is_neutral() {
        let cal = make_calibration_fact("role:executor=cheapest:sonnet", "cal-004");
        let snapshot = snapshot_with_decision(cal);
        // Role "reviewer" not in calibration.
        let result = check_tier_fit("reviewer", Some("haiku"), &snapshot);
        assert_eq!(result.status, "no_calibration");
        assert!(result.finding.is_none());
    }

    #[test]
    fn tier_fit_semicolon_separated_entries_parsed() {
        let cal = make_calibration_fact(
            "role:executor=cheapest:sonnet; role:planner=cheapest:opus",
            "cal-005",
        );
        let snapshot = snapshot_with_decision(cal);
        let r1 = check_tier_fit("executor", Some("haiku"), &snapshot);
        assert_eq!(r1.status, "mismatch");
        assert_eq!(r1.calibrated_tier.as_deref(), Some("sonnet"));
        let r2 = check_tier_fit("planner", Some("opus"), &snapshot);
        assert_eq!(r2.status, "ok");
    }
}
