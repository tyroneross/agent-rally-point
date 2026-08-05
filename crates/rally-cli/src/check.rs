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
        // TTL-primary liveness (ADVISORY tier): a claim whose owner has gone
        // idle past the 15-minute threshold is "squatting" and must not
        // hard-block a peer's write (fact_182e8 gap 1: a dead owner's claims
        // squat forever because `rally say release` was owner-only). Such a
        // claim downgrades from a hard `stop` to a reclaimable `warn` so the
        // peer can proceed. This is advisory only — it does NOT itself release
        // the claim; the destructive takeover release applies a stricter
        // staleness bar (`takeover_eligible_owners`, 2h) so a busy-but-quiet
        // agent is not reclaimed out from under (independent-auditor HIGH).
        let stale_owners = snapshot.idle_owner_tools();
        for claim in &snapshot.active_claims {
            let is_different_tool = claim.tool.as_deref() != Some(tool);
            let exact_or_dir = claim
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path));
            let owner_is_stale = claim
                .tool
                .as_deref()
                .map(|o| stale_owners.contains(o))
                .unwrap_or(false);

            if exact_or_dir && is_different_tool && owner_is_stale {
                // Squatting claim: reclaimable, not a hard block.
                findings.push(CheckFinding {
                    code: "stale-owner-claim",
                    severity: "warn",
                    message: format!(
                        "path claimed by {} whose presence is idle (>15m) — not a hard \
                         block; proceed with coordination awareness. If the owner is \
                         truly gone (idle >2h), a lead can reclaim it with \
                         `rally say release --path {} --tool <lead>`",
                        claim.tool.as_deref().unwrap_or("unknown"),
                        path,
                    ),
                    fact_id: Some(claim.event_id.clone()),
                    owner: claim.tool.clone(),
                    path: Some(path.to_string()),
                    scope: Vec::new(),
                });
            } else if exact_or_dir && is_different_tool {
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
    // RC-038: an EMPTY scope matches every path, so an unscoped fact used to
    // apply to every write by every agent in the room. Distinguish the two
    // cases by code and severity rather than collapsing them: a fact that named
    // this path is evidence about this write; a fact that named nothing is
    // context, and context must not decide the write.
    for decision in &snapshot.current_decisions {
        let scoped_match = path.is_some_and(|path| {
            decision
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path))
        });
        if scoped_match {
            findings.push(CheckFinding {
                code: "binding-decision",
                severity: "info",
                message: decision.subject.clone(),
                fact_id: Some(decision.event_id.clone()),
                owner: None,
                path: path.map(str::to_string),
                scope: Vec::new(),
            });
        } else if decision.scope.is_empty() {
            findings.push(CheckFinding {
                code: "unscoped-decision",
                severity: "info",
                message: decision.subject.clone(),
                fact_id: Some(decision.event_id.clone()),
                owner: None,
                path: None,
                scope: Vec::new(),
            });
        }
    }
    for blocker in &snapshot.active_blockers {
        let scoped_match = path.is_some_and(|path| {
            blocker
                .scope
                .iter()
                .any(|scope| path_matches_scope(scope, path))
        });
        if scoped_match {
            findings.push(CheckFinding {
                code: "active-blocker",
                severity: "stop",
                message: blocker.subject.clone(),
                fact_id: Some(blocker.event_id.clone()),
                owner: None,
                path: path.map(str::to_string),
                scope: Vec::new(),
            });
        } else if blocker.scope.is_empty() {
            // RC-038, live-reproduced: one
            // `rally say blocker --subject "everything is blocked"` flipped
            // `check before-write` from allow to deny for EVERY agent, and
            // under RALLY_HOOK_STRICT=1 that became `permissionDecision: deny`
            // on every edit in the room. Any peer — or any commit touching the
            // git-tracked ledger — could post it.
            //
            // A room-wide freeze is still a real thing a lead needs, so the
            // capability is gated rather than removed, on the same rule
            // RC-037's `workspace:*` gate uses: a room-wide effect requires
            // the lead seat. The lead's unscoped blocker still stops every
            // write; anyone else's is surfaced as a warning the agent reads
            // and decides about.
            //
            // Residual risk, documented in docs/security/TRUST-MODEL.md: the
            // lead seat can be taken by first join, so an agent that enters an
            // empty room first can still freeze it. That is the same authority
            // rally already extends to the lead everywhere else, and it is far
            // narrower than "any fact from any writer".
            //
            // ARP-R-01 / D9. This used to read `blocker.tool == snapshot.lead`
            // — the CURRENT lead — which re-authorized the fact on every call
            // and made the verdict retroactive in both directions. Live: the
            // same fact id armed into a room-wide deny when its author later
            // took the seat, and the honest lead's freeze disarmed when anyone
            // else took it.
            //
            // The projection now decides this once, against the lead as of the
            // blocker's own seq, and publishes the answer as
            // `room_freeze_id`. This function REPORTS that decision; it does
            // not make one. `room_freeze_id` is None on a pre-fix daemon
            // payload, which reads as "no authorized freeze" — see the field's
            // doc for why that is the right degradation.
            let from_lead = snapshot.room_freeze_id.as_deref() == Some(blocker.event_id.as_str());
            if from_lead {
                findings.push(CheckFinding {
                    code: "room-freeze",
                    severity: "stop",
                    message: format!(
                        "{} — room-wide freeze declared by the lead ({}).",
                        blocker.subject,
                        blocker.tool.as_deref().unwrap_or("unknown"),
                    ),
                    fact_id: Some(blocker.event_id.clone()),
                    owner: blocker.tool.clone(),
                    path: path.map(str::to_string),
                    scope: Vec::new(),
                });
            } else {
                findings.push(CheckFinding {
                    code: "unscoped-blocker",
                    severity: "warn",
                    message: format!(
                        "{} — posted by {}, who does not hold the lead seat, and naming no \
                         scope, so it does not block this write. Re-post it with \
                         `--scope file:<path>` to stop writes to a specific path, or take \
                         the lead seat to declare a room-wide freeze.",
                        blocker.subject,
                        blocker.tool.as_deref().unwrap_or("an unidentified writer"),
                    ),
                    fact_id: Some(blocker.event_id.clone()),
                    owner: blocker.tool.clone(),
                    path: None,
                    scope: Vec::new(),
                });
            }
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

/// Test-only accessor so sibling modules (lib.rs unit tests) can exercise the
/// private `check_before_write` gate and read back `(code, severity)` pairs
/// without exposing the private `CheckFinding` type.
#[cfg(test)]
pub(crate) fn check_before_write_for_test(
    snapshot: &RoomSnapshot,
    tool: &str,
    path: Option<&str>,
    out: &mut Vec<(&'static str, &'static str)>,
) {
    let mut findings = Vec::new();
    check_before_write(snapshot, tool, path, &mut findings);
    out.extend(findings.into_iter().map(|f| (f.code, f.severity)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Fact, RoomSnapshot, Squad};

    fn claim_by(tool: &str, path: &str) -> Fact {
        Fact {
            tool: Some(tool.to_string()),
            scope: vec![format!("file:{path}")],
            event_id: format!("fact_{tool}"),
            ..Default::default()
        }
    }

    fn blocker(tool: &str, subject: &str, scope: Vec<&str>) -> Fact {
        Fact {
            tool: Some(tool.to_string()),
            subject: subject.to_string(),
            scope: scope.into_iter().map(str::to_string).collect(),
            event_id: format!("blocker_{tool}"),
            ..Default::default()
        }
    }

    /// RC-038 adversarial control. Revert the empty-scope branch to the old
    /// `scope.is_empty() || matches` condition at severity `stop` and this
    /// fails: one unscoped blocker from ANY writer denied every write by every
    /// agent in the room, and under `RALLY_HOOK_STRICT=1` that was a hard deny
    /// on every edit.
    #[test]
    fn before_write_unscoped_blocker_from_a_non_lead_does_not_stop_the_write() {
        let snapshot = RoomSnapshot {
            active_blockers: vec![blocker("rogue", "everything is blocked", vec![])],
            lead: Some("the-lead".to_string()),
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);

        assert!(
            !findings.iter().any(|f| f.severity == "stop"),
            "an unscoped blocker must not hard-stop an unrelated write; got {:?}",
            findings
                .iter()
                .map(|f| (f.code, f.severity))
                .collect::<Vec<_>>()
        );
        let f = findings
            .iter()
            .find(|f| f.code == "unscoped-blocker")
            .expect("the unscoped blocker must still be surfaced, just not as a stop");
        assert_eq!(f.severity, "warn");
        assert!(
            f.message.contains("everything is blocked"),
            "the agent must still read the blocker's subject"
        );
    }

    /// The capability survives the fix: the LEAD's unscoped blocker is a real
    /// room-wide freeze and still stops every write. RC-038 removes the DoS,
    /// not the freeze.
    #[test]
    fn before_write_unscoped_blocker_from_the_lead_freezes_the_room() {
        // ARP-R-01: `check` no longer decides WHICH unscoped blocker is an
        // authorized freeze — the projection does, at the blocker's own seq,
        // and publishes `room_freeze_id`. This test grades the reporting half,
        // so it states the projection's verdict directly. The DECIDING half
        // (including the retroactive arm and disarm that motivated the change)
        // is graded in `tests/room_freeze_admission_time.rs` against real
        // ledgers, because a hand-built snapshot cannot express "the seat
        // changed hands after this blocker was written".
        let b = blocker("the-lead", "release freeze", vec![]);
        let snapshot = RoomSnapshot {
            room_freeze_id: Some(b.event_id.clone()),
            active_blockers: vec![b],
            lead: Some("the-lead".to_string()),
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let f = findings
            .iter()
            .find(|f| f.code == "room-freeze")
            .expect("the lead's unscoped blocker must still freeze the room");
        assert_eq!(f.severity, "stop");
    }

    /// A room with no lead has nobody who can freeze it, so an unscoped
    /// blocker from anyone is advisory.
    #[test]
    fn before_write_unscoped_blocker_without_a_lead_does_not_stop_the_write() {
        let snapshot = RoomSnapshot {
            active_blockers: vec![blocker("someone", "everything is blocked", vec![])],
            lead: None,
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        assert!(!findings.iter().any(|f| f.severity == "stop"));
    }

    /// The other half of the same control: a blocker that NAMES this path
    /// still hard-stops. Deconfliction is narrowed, not removed.
    #[test]
    fn before_write_scoped_blocker_still_stops_the_write() {
        let snapshot = RoomSnapshot {
            active_blockers: vec![blocker(
                "peer",
                "migration in flight",
                vec!["file:src/foo.rs"],
            )],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let f = findings
            .iter()
            .find(|f| f.code == "active-blocker")
            .expect("a scoped blocker on this path must still fire");
        assert_eq!(f.severity, "stop");
    }

    /// A scoped blocker on a DIFFERENT path must not leak onto this write.
    #[test]
    fn before_write_blocker_scoped_elsewhere_is_silent() {
        let snapshot = RoomSnapshot {
            active_blockers: vec![blocker("peer", "other lane", vec!["file:src/other.rs"])],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        assert!(
            !findings
                .iter()
                .any(|f| f.code == "active-blocker" || f.code == "unscoped-blocker"),
            "a blocker scoped to another path must produce no blocker finding"
        );
    }

    /// RC-038 second half — an unscoped binding decision matched every path
    /// too. It stays visible (`info`, so it never gated the write) but is now
    /// labelled as unscoped so a reader is not told it applies to this path.
    #[test]
    fn before_write_unscoped_decision_is_labelled_unscoped() {
        let mut decision = blocker("lead", "prefer async over threads", vec![]);
        decision.event_id = "decision_lead".to_string();
        let snapshot = RoomSnapshot {
            current_decisions: vec![decision],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let f = findings
            .iter()
            .find(|f| f.code == "unscoped-decision")
            .expect("an unscoped decision must be surfaced as unscoped");
        assert_eq!(f.severity, "info");
        assert!(
            f.path.is_none(),
            "an unscoped decision must not be reported as applying to this path"
        );
    }

    fn squad(tool: &str, status: &str) -> Squad {
        Squad {
            tool: tool.to_string(),
            status: status.to_string(),
            acknowledged: true,
            ..Default::default()
        }
    }

    /// fact_182e8 gap 1 — a peer's `before-write` against a path claimed by a
    /// LIVE owner is a hard `stop` (unchanged behaviour).
    #[test]
    fn before_write_live_owner_claim_is_a_hard_stop() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("alpha", "src/foo.rs")],
            squads: vec![squad("alpha", "active")],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let f = findings
            .iter()
            .find(|f| f.fact_id.as_deref() == Some("fact_alpha"))
            .expect("a finding about alpha's claim must exist");
        assert_eq!(f.code, "claimed-path");
        assert_eq!(f.severity, "stop");
    }

    /// fact_182e8 gap 1 — when the owner is liveness-stale (squad idle), the
    /// same conflict downgrades from a hard `stop` to a reclaimable `warn` so
    /// the peer is not blocked by a dead owner's squatting claim.
    #[test]
    fn before_write_stale_owner_claim_downgrades_to_reclaimable_warn() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("dead-owner", "src/foo.rs")],
            squads: vec![squad("dead-owner", "idle")],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let f = findings
            .iter()
            .find(|f| f.fact_id.as_deref() == Some("fact_dead-owner"))
            .expect("a finding about the stale claim must exist");
        assert_eq!(
            f.code, "stale-owner-claim",
            "stale-owner conflict must use the reclaimable code"
        );
        assert_eq!(
            f.severity, "warn",
            "stale-owner conflict must not hard-block the peer"
        );
        assert!(
            !findings.iter().any(|f| f.code == "claimed-path"),
            "a stale-owner conflict must NOT also emit a hard claimed-path stop"
        );
    }
}
