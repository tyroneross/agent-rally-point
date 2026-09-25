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
    /// The first blocking finding's own text, not a generic stand-in: the
    /// agent-visible channel is where a colliding writer decides what to do
    /// next, and a sentence that names no holder and no path cannot support
    /// that decision.
    message: String,
    /// Carried so the hook renderer can compose an options-first block without
    /// re-deriving the holder from prose (or taking any new lookup). Both come
    /// straight from the finding that stopped the write.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Seconds since the holder's last presence beat. Same provenance as
    /// `owner` and `path` — copied off the blocking finding, not re-derived and
    /// not parsed out of `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_idle_secs: Option<i64>,
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
    /// Seconds since the owner's last presence beat, when the room knows it.
    /// Carried as a NUMBER so every consumer (the hook renderer, any future
    /// host) reads liveness the way it reads `owner` and `path` — off a field,
    /// never by parsing the sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_idle_secs: Option<i64>,
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
    caller_session: Option<&str>,
    path: Option<String>,
    strict: bool,
    snapshot: &RoomSnapshot,
    coord: &crate::hooks_config::CoordinationConfig,
) -> Result<CheckOutcome> {
    let mut findings = Vec::new();
    match phase.as_str() {
        "before-write" => {
            check_before_write_with_coord(snapshot, coord, &tool, path.as_deref(), &mut findings)
        }
        "before-complete" => check_before_complete(snapshot, &tool, caller_session, &mut findings),
        other => {
            return Err(RallyError::Usage(format!(
                "unsupported check phase {other}"
            )));
        }
    }
    let blocker = findings.iter().find(|finding| finding.severity == "stop");
    let stop = blocker.is_some();
    let visible_message = match blocker {
        Some(finding) => finding.message.clone(),
        None => "Rally check passed.".to_string(),
    };
    let visible_owner = blocker.and_then(|finding| finding.owner.clone());
    let visible_path = blocker.and_then(|finding| finding.path.clone());
    let visible_idle = blocker.and_then(|finding| finding.owner_idle_secs);
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
                    message: visible_message,
                    owner: visible_owner,
                    path: visible_path,
                    owner_idle_secs: visible_idle,
                },
            },
        },
        exit_code,
        finding_count,
    })
}

/// Seconds since the owner's last presence beat, straight off the squad
/// projection already carried by the snapshot in hand. This costs no new read:
/// `RoomSnapshot::squads` is projected once, before `build_check` is called.
///
/// `None` when the owner has no squad row, or its `last_seen_ts` did not parse
/// (`freshness == "unknown"`). Callers must degrade to the untimed sentence
/// rather than invent an age — a wrong "idle 3h" is exactly the claim that
/// talks a reader into editing someone's live file.
fn owner_idle_secs(snapshot: &RoomSnapshot, owner: &str) -> Option<i64> {
    snapshot.squad_for(owner).and_then(|sq| sq.age_secs)
}

/// One coarse unit, largest that keeps the number meaningful. The reader is
/// deciding "is this peer at the keyboard or gone", so `2h` and `3d` carry that
/// decision and `9,481s` does not.
pub(crate) fn humanize_age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5_400 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
fn check_before_write(
    snapshot: &RoomSnapshot,
    tool: &str,
    path: Option<&str>,
    findings: &mut Vec<CheckFinding>,
) {
    check_before_write_with_coord(snapshot, &Default::default(), tool, path, findings);
}

fn check_before_write_with_coord(
    snapshot: &RoomSnapshot,
    coord: &crate::hooks_config::CoordinationConfig,
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
            owner_idle_secs: None,
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
        // the claim; destructive takeover uses the configured per-claim work
        // size threshold and durable authored activity.
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
            let owner = claim.tool.as_deref().unwrap_or("unknown owner");

            let idle_secs = owner_idle_secs(snapshot, owner);

            if exact_or_dir && is_different_tool && owner_is_stale {
                // Squatting claim: reclaimable, not a hard block.
                //
                // The two arms differ in the ONE thing the reader is deciding:
                // whether this claim is eligible for release with their OWN
                // tool id. The claim's work size and configured timeout matter.
                // The previous text handed the refused caller
                // `rally say release --path X --tool <owner>` — a pasteable
                // command whose only effect is to post AS the owner, since
                // identity here is self-asserted and unsigned (ARP-R-01).
                // Naming the owner as the actor in prose keeps the fact; the
                // command is gone.
                let silence = idle_secs
                    .map(|secs| format!("has been silent {}", humanize_age(secs)))
                    .unwrap_or_else(|| "has been idle past the 15m window".to_string());
                let (reclaimable, _) = snapshot.claim_reclaim_eligible(claim, coord);
                let message = if reclaimable {
                    format!(
                        "{} holds {} and {} — past this claim's reclaim threshold, so it is \
                         yours to reclaim. Your options: release it yourself with \
                         `rally say release --path {} --tool {}`, hand the change over \
                         with `rally say handoff --to {} --subject \"<change>\"`, or take \
                         another task with `rally next --tool {}`",
                        owner, path, silence, path, tool, owner, tool,
                    )
                } else {
                    format!(
                        "{} holds {} and {} — idle, but this claim is not proven eligible for \
                         takeover and {} may still be working. \
                         Your options: proceed with coordination awareness, hand the \
                         change over with `rally say handoff --to {} --subject \
                         \"<change>\"`, or take another task with `rally next --tool {}`. \
                         {} releases its own claim when it returns",
                        owner, path, silence, owner, owner, tool, owner,
                    )
                };
                findings.push(CheckFinding {
                    code: "stale-owner-claim",
                    severity: "warn",
                    message,
                    fact_id: Some(claim.event_id.clone()),
                    owner: claim.tool.clone(),
                    path: Some(path.to_string()),
                    owner_idle_secs: idle_secs,
                    scope: Vec::new(),
                });
            } else if exact_or_dir && is_different_tool {
                findings.push(CheckFinding {
                    code: "claimed-path",
                    severity: "stop",
                    // Names the holder, the path, and the claim id. The bare
                    // "another agent has claimed this path" was the single most
                    // frequent collision message in the room and identified
                    // none of the three, so the reader spent turns working out
                    // who to talk to. Same shape as `stale-owner-claim` above.
                    // The liveness clause is the deciding fact, not decoration:
                    // an A/B role-play reader given holder + path alone still
                    // judged the holder stale on its own and edited unclaimed.
                    // "active 2m ago" is what makes "hand it off" the obvious
                    // option rather than a suggestion to argue with. Age comes
                    // from the squad row already in this snapshot.
                    message: format!(
                        "{} holds {} (claim {}){} — hand the change over with \
                         `rally say handoff --to {} --subject \"<change>\"`, or take \
                         another task with `rally next --tool {}`",
                        owner,
                        path,
                        claim.event_id,
                        idle_secs
                            .map(|secs| format!(", active {} ago", humanize_age(secs)))
                            .unwrap_or_default(),
                        owner,
                        tool,
                    ),
                    fact_id: Some(claim.event_id.clone()),
                    owner: claim.tool.clone(),
                    path: Some(path.to_string()),
                    owner_idle_secs: idle_secs,
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
                            owner_idle_secs: None,
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
                owner_idle_secs: None,
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
                owner_idle_secs: None,
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
                owner_idle_secs: None,
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
                    owner_idle_secs: None,
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
                    owner_idle_secs: None,
                    scope: Vec::new(),
                });
            }
        }
    }
}

fn check_before_complete(
    snapshot: &RoomSnapshot,
    tool: &str,
    caller_session: Option<&str>,
    findings: &mut Vec<CheckFinding>,
) {
    for claim in &snapshot.active_claims {
        if crate::claim_authority::claim_owner_matches_caller(
            claim.tool.as_deref(),
            claim.from_session_id.as_deref(),
            Some(tool),
            caller_session,
        ) {
            findings.push(CheckFinding {
                code: "owned-active-claim",
                severity: "stop",
                message: "release or explain this active claim before completion".to_string(),
                fact_id: Some(claim.event_id.clone()),
                owner: None,
                path: None,
                owner_idle_secs: None,
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
                owner_idle_secs: None,
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

    /// A squad whose presence is `age_secs` old, with `last_seen_ts` set to
    /// match so `takeover_eligible_owners` (which parses the timestamp, not the
    /// age field) agrees with what the fixture says.
    fn squad_aged(tool: &str, status: &str, age_secs: i64) -> Squad {
        let seen = chrono::Utc::now() - chrono::Duration::seconds(age_secs);
        Squad {
            tool: tool.to_string(),
            status: status.to_string(),
            acknowledged: true,
            age_secs: Some(age_secs),
            last_seen_ts: seen.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ..Default::default()
        }
    }

    fn message_for(snapshot: &RoomSnapshot, code: &str) -> String {
        let mut findings = Vec::new();
        check_before_write(snapshot, "beta", Some("src/foo.rs"), &mut findings);
        findings
            .iter()
            .find(|f| f.code == code)
            .unwrap_or_else(|| panic!("expected a {code} finding, got {findings:#?}"))
            .message
            .clone()
    }

    /// GAP 2 — a collision message that names the holder but not how long ago
    /// it was active leaves the reader to guess liveness, and in the A/B a
    /// reader guessed "stale" and edited unclaimed. The age must be stated.
    #[test]
    fn live_owner_collision_states_how_recently_the_holder_was_active() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("alpha", "src/foo.rs")],
            squads: vec![squad_aged("alpha", "active", 120)],
            ..Default::default()
        };
        let msg = message_for(&snapshot, "claimed-path");
        assert!(
            msg.contains("active 2m ago"),
            "live-owner collision must state the holder's last-active age: {msg}"
        );
    }

    /// GAP 2c — the timing exists to change the decision, so the two stale
    /// states must not read the same. Inside the silence window the claim is
    /// not the reader's to release; past it, it is.
    #[test]
    fn the_recommended_option_changes_with_the_holders_silence() {
        let idle_only = RoomSnapshot {
            active_claims: vec![claim_by("dead-owner", "src/foo.rs")],
            squads: vec![squad_aged("dead-owner", "idle", 20 * 60)],
            ..Default::default()
        };
        let msg = message_for(&idle_only, "stale-owner-claim");
        assert!(
            msg.contains("silent 20m"),
            "idle message must state the silence: {msg}"
        );
        assert!(
            !msg.contains("rally say release"),
            "inside the silence window the claim is NOT the reader's to release: {msg}"
        );

        let small_claim = RoomSnapshot {
            active_claims: vec![claim_by("dead-owner", "src/foo.rs")],
            squads: vec![squad_aged("dead-owner", "idle", 45 * 60)],
            ..Default::default()
        };
        let msg = message_for(&small_claim, "stale-owner-claim");
        assert!(
            msg.contains("rally say release --path src/foo.rs --tool beta"),
            "a single-file claim uses the default 30m reclaim threshold: {msg}"
        );
        let mut findings = Vec::new();
        let longer_policy = crate::hooks_config::CoordinationConfig {
            reclaim_small_minutes: 60,
            ..Default::default()
        };
        check_before_write_with_coord(
            &small_claim,
            &longer_policy,
            "beta",
            Some("src/foo.rs"),
            &mut findings,
        );
        let msg = &findings
            .iter()
            .find(|f| f.code == "stale-owner-claim")
            .expect("claim finding")
            .message;
        assert!(
            !msg.contains("rally say release"),
            "a configured 60m threshold keeps a 45m claim with its owner: {msg}"
        );

        let reclaimable = RoomSnapshot {
            active_claims: vec![claim_by("dead-owner", "src/foo.rs")],
            squads: vec![squad_aged("dead-owner", "idle", 3 * 60 * 60)],
            ..Default::default()
        };
        let msg = message_for(&reclaimable, "stale-owner-claim");
        assert!(
            msg.contains("silent 3h"),
            "reclaimable message must state the silence: {msg}"
        );
        assert!(
            msg.contains("rally say release --path src/foo.rs --tool beta"),
            "past the silence window the reader releases it with its OWN tool id: {msg}"
        );
    }

    /// The age is absent whenever the room cannot vouch for it, and absence
    /// must degrade to the untimed sentence rather than to a fabricated age.
    #[test]
    fn an_unknown_holder_age_omits_the_clause_instead_of_inventing_one() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("ghost", "src/foo.rs")],
            squads: Vec::new(),
            ..Default::default()
        };
        let msg = message_for(&snapshot, "claimed-path");
        assert!(
            !msg.contains("active"),
            "no squad row means no age claim: {msg}"
        );
        assert_actionable("claimed-path", &msg, "ghost", "src/foo.rs");
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

    /// Commands the CLI actually accepts, verified against `rally say --help`
    /// (kinds `handoff` / `release`; flags `--to`, `--path`, `--tool`,
    /// `--subject`) and `rally next --help` (`--tool`).
    const ALLOWED_COMMANDS: [&str; 3] = [
        "rally say handoff --to ",
        "rally next --tool ",
        "rally say release --path ",
    ];

    /// Suggestions that must never reach an agent.
    ///
    /// `claim --queue` and `rally say ask` do not exist at all. `rally say
    /// release --path X --tool <lead>` exists but instructs the caller that was
    /// just refused to post as the LEAD — identity here is self-asserted and
    /// unsigned, so the refusal would be coaching the reader around itself.
    /// ARP-R-01 removed that shape once already (`claim_authority.rs`).
    // This denylist's VALUE is the banned string, so the repo-wide sweep
    // (`tests/no_lead_impersonation_advice.rs`) has to skip exactly this line
    // and nothing else — the ban cannot state what it bans otherwise. The
    // marker is per-line and deliberate, which is what keeps it reviewable.
    const BANNED_SUGGESTIONS: [&str; 4] = [
        "claim --queue",
        "rally say ask",
        "--tool <lead>",  // ARP-R-01-ALLOW
        "--tool <owner>", // ARP-R-01-ALLOW
    ];

    /// A collision message is only useful if the reader can act on it alone:
    /// it names WHO holds the contested thing, names WHAT is contested, and
    /// every command it offers exists.
    fn assert_actionable(label: &str, msg: &str, holder: &str, contested: &str) {
        assert!(
            msg.contains(holder),
            "{label} must name the holder ({holder}): {msg}"
        );
        assert!(
            msg.contains(contested),
            "{label} must name the contested path/scope ({contested}): {msg}"
        );
        for banned in BANNED_SUGGESTIONS {
            assert!(
                !msg.contains(banned),
                "{label} suggests {banned}, which must never be offered: {msg}"
            );
        }
        let mut commands = 0usize;
        for span in msg.split('`').skip(1).step_by(2) {
            if !span.starts_with("rally ") {
                continue;
            }
            commands += 1;
            assert!(
                ALLOWED_COMMANDS.iter().any(|ok| span.starts_with(ok)),
                "{label} offers `{span}`, which is not a verified rally command"
            );
        }
        assert!(
            commands > 0,
            "{label} offers the reader no command at all: {msg}"
        );
    }

    #[test]
    fn every_collision_message_names_the_holder_the_path_and_a_real_command() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("alpha", "src/foo.rs")],
            squads: vec![squad("alpha", "active")],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_before_write(&snapshot, "beta", Some("src/foo.rs"), &mut findings);
        let live = findings
            .iter()
            .find(|f| f.code == "claimed-path")
            .expect("live-owner collision finding");
        assert_actionable("claimed-path", &live.message, "alpha", "src/foo.rs");
        assert!(
            live.message.contains("fact_alpha"),
            "claimed-path must name the claim id: {}",
            live.message
        );

        let stale_snapshot = RoomSnapshot {
            active_claims: vec![claim_by("dead-owner", "src/foo.rs")],
            squads: vec![squad("dead-owner", "idle")],
            ..Default::default()
        };
        let mut stale_findings = Vec::new();
        check_before_write(
            &stale_snapshot,
            "beta",
            Some("src/foo.rs"),
            &mut stale_findings,
        );
        let stale = stale_findings
            .iter()
            .find(|f| f.code == "stale-owner-claim")
            .expect("stale-owner collision finding");
        assert_actionable(
            "stale-owner-claim",
            &stale.message,
            "dead-owner",
            "src/foo.rs",
        );

        let conflict = crate::claim_authority::conflict_message(
            &crate::claim_authority::ClaimConflict {
                existing_claim_id: "fact_alpha".to_string(),
                existing_owner: Some("alpha".to_string()),
                scope: "file:src/foo.rs".to_string(),
                existing_scope: "dir:src".to_string(),
            },
            None,
            None,
        );
        assert_actionable("claim conflict", &conflict, "alpha", "dir:src");
    }

    /// The agent-visible channel is what the hook renders from; it must carry
    /// the blocking finding's own text plus the structured holder/path, not a
    /// generic sentence the renderer would have to parse back out of prose.
    #[test]
    fn agent_visible_carries_the_blocking_finding_and_its_holder() {
        let snapshot = RoomSnapshot {
            active_claims: vec![claim_by("alpha", "src/foo.rs")],
            squads: vec![squad("alpha", "active")],
            ..Default::default()
        };
        let outcome = build_check(
            "before-write".to_string(),
            "beta".to_string(),
            None,
            Some("src/foo.rs".to_string()),
            false,
            &snapshot,
            &Default::default(),
        )
        .expect("check builds");
        let visible = &outcome.data.check.agent_visible;
        assert!(visible.present);
        assert_eq!(visible.owner.as_deref(), Some("alpha"));
        assert_eq!(visible.path.as_deref(), Some("src/foo.rs"));
        assert!(
            visible.message.contains("alpha") && visible.message.contains("src/foo.rs"),
            "agent_visible message must name holder and path: {}",
            visible.message
        );
    }
}
