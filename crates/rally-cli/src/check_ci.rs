// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// B13: `rally check ci` — read-only CI gate for room health.
//
// Inspects the room snapshot for:
//   (a) unresolved blockers
//   (b) claims whose `depends:<x>` markers are not satisfied (dep-not-met)
//   (c) handoffs without a receipt past a configurable time threshold
//
// Returns exit 0 (pass) always; with `--strict` returns exit 4 (fail) when any
// offender is found.  Read-only — no facts are written.

use chrono::DateTime;
use schemars::JsonSchema;
use serde::Serialize;

use crate::store::RoomSnapshot;

/// One offender found by `check ci`.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CiOffender {
    /// Machine-readable code: `"unresolved-blocker"` | `"dep-not-met"` | `"unreceipted-handoff"`.
    pub(crate) code: &'static str,
    /// The `event_id` of the offending fact.
    pub(crate) fact_id: String,
    /// Short human-readable explanation.
    pub(crate) message: String,
    /// The unsatisfied dependency target (for `dep-not-met` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dep: Option<String>,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct CheckCiData {
    pub(crate) check_ci: CheckCiResult,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct CheckCiResult {
    pub(crate) pass: bool,
    pub(crate) mode: &'static str,
    pub(crate) receipt_threshold_secs: u64,
    pub(crate) offenders: Vec<CiOffender>,
}

pub(crate) struct CheckCiOutcome {
    pub(crate) data: CheckCiData,
    pub(crate) exit_code: u8,
    pub(crate) offender_count: usize,
}

/// Run the CI gate against `snapshot`.  Pure function — no I/O.
pub(crate) fn build_check_ci(
    strict: bool,
    receipt_threshold_secs: u64,
    snapshot: &RoomSnapshot,
) -> CheckCiOutcome {
    let mut offenders: Vec<CiOffender> = Vec::new();

    // (a) Unresolved blockers.
    for blocker in &snapshot.active_blockers {
        offenders.push(CiOffender {
            code: "unresolved-blocker",
            fact_id: blocker.event_id.clone(),
            message: format!("unresolved blocker: {}", blocker.subject),
            dep: None,
        });
    }

    // (b) Claims with unsatisfied `depends:<x>` markers.
    //
    // A `depends:<x>` is satisfied when at least one other fact (any kind) in
    // the room carries `produces:<x>` in its evidence.  The comparison is
    // case-insensitive exact match on the token after the prefix.
    let all_produced: std::collections::BTreeSet<String> = snapshot
        .active_claims
        .iter()
        .chain(snapshot.recent_artifacts.iter())
        .flat_map(|f| f.evidence.iter())
        .filter_map(|e| e.strip_prefix("produces:"))
        .map(|v| v.to_lowercase())
        .collect();

    for claim in &snapshot.active_claims {
        for evidence_item in &claim.evidence {
            if let Some(dep) = evidence_item.strip_prefix("depends:")
                && !all_produced.contains(&dep.to_lowercase())
            {
                offenders.push(CiOffender {
                    code: "dep-not-met",
                    fact_id: claim.event_id.clone(),
                    message: format!(
                        "claim '{}' depends on '{}' but no fact produces it",
                        claim.subject, dep
                    ),
                    dep: Some(dep.to_string()),
                });
            }
        }
    }

    // (c) Handoffs without a receipt past the threshold.
    //
    // A handoff is "receipted" when a `FactKind::Receipt` fact with `ref_id`
    // pointing to it exists.  We approximate this by checking if the handoff's
    // `event_id` appears in the resolved set (receipts close handoffs in the
    // snapshot projection — so a handoff that is still in `open_handoffs` has
    // no receipt yet).  Age is measured from `created_at`.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for handoff in &snapshot.open_handoffs {
        let age_secs = DateTime::parse_from_rfc3339(&handoff.created_at)
            .map(|dt| now_secs.saturating_sub(u64::try_from(dt.timestamp()).unwrap_or(0)))
            .unwrap_or(0);
        if age_secs > receipt_threshold_secs {
            offenders.push(CiOffender {
                code: "unreceipted-handoff",
                fact_id: handoff.event_id.clone(),
                message: format!(
                    "handoff '{}' has no receipt after {}s (threshold {}s)",
                    handoff.subject, age_secs, receipt_threshold_secs
                ),
                dep: None,
            });
        }
    }

    let pass = offenders.is_empty();
    let exit_code = if strict && !pass { 4 } else { 0 };
    let offender_count = offenders.len();

    CheckCiOutcome {
        data: CheckCiData {
            check_ci: CheckCiResult {
                pass,
                mode: if strict { "strict" } else { "warn" },
                receipt_threshold_secs,
                offenders,
            },
        },
        exit_code,
        offender_count,
    }
}
