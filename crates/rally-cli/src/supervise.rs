// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Supervision projection — what a lead WOULD reclaim and where it WOULD go.
//!
//! # Charter assertion (RALLY RECORDS AND DERIVES; IT NEVER EXECUTES)
//!
//! This module is a READ-ONLY PROJECTION. It scans facts and squads, derives
//! reclaim candidates and ranked handoff targets, and returns them. Every
//! command it emits is a plain `String` for the external runner (`rally watch`,
//! a LaunchAgent, cron, or a human) to decide about. It NEVER calls
//! process::Command, thread::spawn, exec, or any scheduling API.
//!
//! Grep invariant (checked by charter_test):
//!   `SEAM_NO_EXEC: supervise.rs contains zero calls to Command/spawn/exec`
//!
//! Runner-boundary litmus:
//!   "Does this make Rally start, resume, retry, or schedule work?" → NO.
//!
//! # Why this exists
//!
//! Detection, reclamation, assignment, and execution each already ship as
//! separate commands (`dag`, `check liveness`, `doctor --reap-stale`,
//! `say handoff`, `watch`). Nothing composed them, so the judgment "this claim
//! is abandoned, and THAT peer should take it" lived in no command and had to be
//! re-derived by hand every time. This is that composition, and only that.
//!
//! # The eligibility rule, and why it is not freshness
//!
//! `Squad::freshness` is a heartbeat-only verdict and is documented ADVISORY —
//! it "never gates a write, a target, or a takeover". Reclaiming another agent's
//! claim is a takeover, so freshness must NOT decide it. Eligibility here is the
//! four-signal [`liveness::reapable`] verdict, which is fail-closed on
//! `Unknown`, AND a writer-stamped expired lease. Both, never either.
//!
//! Freshness is used for exactly one thing: RANKING handoff targets, which is
//! the advisory use its own documentation endorses.

use crate::liveness::{Liveness, reapable};
use crate::store::{FactKind, Squad, squad_freshness_rank};

/// A claim a lead could propose reclaiming, with the reason it is (or is not)
/// eligible. Emitted for BOTH outcomes on purpose: a candidate that was examined
/// and refused is the useful half of the answer when work looks stuck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimCandidate {
    /// The claim fact's event id — the `--ref` an operator would act on.
    pub(crate) claim_event_id: String,
    /// Tool id currently holding the claim.
    pub(crate) owner_tool: String,
    /// The claim's subject line, verbatim from the ledger (peer-authored text).
    pub(crate) subject: String,
    /// Scope entries the claim reserves.
    pub(crate) scope: Vec<String>,
    /// `--step` marker when the claim carries fan-out lineage.
    pub(crate) step: Option<String>,
    /// Whether the writer-stamped lease has passed `now`.
    pub(crate) lease_expired: bool,
    /// Four-signal verdict for the owner: `live` | `stale` | `unknown`.
    pub(crate) owner_liveness: String,
    /// True only when the lease expired AND the owner is reapable.
    pub(crate) eligible: bool,
    /// Why not, when `eligible` is false. `None` when eligible.
    pub(crate) held_because: Option<String>,
}

/// A peer that could receive reclaimed work. Ranked, never auto-selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandoffTarget {
    pub(crate) tool: String,
    /// Advisory heartbeat verdict — the endorsed use of freshness.
    pub(crate) freshness: String,
    pub(crate) age_secs: Option<i64>,
}

/// The full supervision projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Supervision {
    pub(crate) run_id: Option<String>,
    pub(crate) candidates: Vec<ReclaimCandidate>,
    pub(crate) targets: Vec<HandoffTarget>,
    pub(crate) facts_scanned: usize,
    /// Commands the EXTERNAL RUNNER may choose to invoke. Rally never runs them.
    pub(crate) suggested_commands: Vec<String>,
}

/// Owner liveness, supplied by the caller so this module stays pure and
/// deterministically testable (the established rally-cli convention).
#[derive(Clone, Copy, Debug)]
pub(crate) struct OwnerVerdict {
    pub(crate) liveness: Liveness,
    pub(crate) parent_alive: Option<bool>,
}

fn liveness_word(l: Liveness) -> &'static str {
    match l {
        Liveness::Live => "live",
        Liveness::Stale => "stale",
        Liveness::Unknown => "unknown",
    }
}

/// Pull `lease_expires_at:<rfc3339>` out of a claim's evidence list.
fn lease_of(evidence: &[String]) -> Option<&str> {
    evidence
        .iter()
        .find_map(|item| item.strip_prefix("lease_expires_at:"))
}

/// Pull a `--step` lineage marker out of a fact's scope entries.
fn step_of(scope: &[String]) -> Option<String> {
    scope
        .iter()
        .find_map(|s| s.strip_prefix("step:"))
        .map(str::to_string)
}

fn in_run(scope: &[String], run_id: Option<&str>) -> bool {
    match run_id {
        None => true,
        Some(want) => scope
            .iter()
            .any(|s| s.strip_prefix("run:").is_some_and(|got| got == want)),
    }
}

/// Build the supervision projection.
///
/// `owner_verdict` is consulted per owning tool; a tool with no verdict is
/// treated as [`Liveness::Unknown`], which is fail-closed and therefore never
/// eligible. `now_rfc3339` is injected rather than read from the clock.
pub(crate) fn project_supervision<F>(
    facts: &[crate::store::Fact],
    squads: &[Squad],
    run_id: Option<&str>,
    now_rfc3339: &str,
    self_tool: Option<&str>,
    owner_verdict: F,
) -> Supervision
where
    F: Fn(&str) -> OwnerVerdict,
{
    let now = chrono::DateTime::parse_from_rfc3339(now_rfc3339).ok();

    // Claims closed by a later release/resolve/expiry are not candidates.
    let closed: std::collections::BTreeSet<&str> = facts
        .iter()
        .filter(|f| {
            matches!(
                f.kind,
                FactKind::Release | FactKind::Resolve | FactKind::ClaimExpired
            )
        })
        .filter_map(|f| f.ref_id.as_deref())
        .collect();

    // The newest renewal per claim carries the authoritative lease.
    let mut renewed: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for f in facts.iter().filter(|f| f.kind == FactKind::ClaimRenewed) {
        if let (Some(target), Some(lease)) = (f.ref_id.as_deref(), lease_of(&f.evidence)) {
            renewed.insert(target, lease);
        }
    }

    let mut candidates = Vec::new();
    for f in facts.iter().filter(|f| f.kind == FactKind::Claim) {
        if closed.contains(f.event_id.as_str()) || !in_run(&f.scope, run_id) {
            continue;
        }
        let owner = f.tool.clone().unwrap_or_default();
        if owner.is_empty() {
            continue;
        }

        let lease = renewed
            .get(f.event_id.as_str())
            .copied()
            .or_else(|| lease_of(&f.evidence));

        let lease_expired = match (lease, now) {
            (Some(l), Some(n)) => chrono::DateTime::parse_from_rfc3339(l)
                .map(|exp| exp <= n)
                .unwrap_or(false),
            _ => false,
        };

        let verdict = owner_verdict(&owner);
        let owner_reapable = reapable(verdict.liveness, verdict.parent_alive);
        let eligible = lease_expired && owner_reapable;

        // Name the FIRST reason that holds it, most-specific first, so the
        // operator reads a cause rather than inferring one from two booleans.
        let held_because = if eligible {
            None
        } else if !lease_expired && !owner_reapable {
            Some(format!(
                "lease still valid and owner is {}",
                liveness_word(verdict.liveness)
            ))
        } else if !lease_expired {
            Some("lease still valid".to_string())
        } else {
            Some(match verdict.liveness {
                Liveness::Live => "owner is live".to_string(),
                Liveness::Unknown => {
                    "owner liveness unknown — fail-closed, never reaped".to_string()
                }
                Liveness::Stale => "owner stale but its parent process is alive".to_string(),
            })
        };

        candidates.push(ReclaimCandidate {
            claim_event_id: f.event_id.clone(),
            owner_tool: owner,
            subject: f.subject.clone(),
            scope: f.scope.clone(),
            step: step_of(&f.scope),
            lease_expired,
            owner_liveness: liveness_word(verdict.liveness).to_string(),
            eligible,
            held_because,
        });
    }

    candidates.sort_by(|a, b| {
        b.eligible
            .cmp(&a.eligible)
            .then_with(|| a.claim_event_id.cmp(&b.claim_event_id))
    });

    // Targets: everyone EXCEPT the owners we are proposing to reclaim from, and
    // except ourselves. Handing work back to the agent it was just taken from
    // is the obvious wrong answer, so it is excluded structurally.
    let reclaim_owners: std::collections::BTreeSet<&str> = candidates
        .iter()
        .filter(|c| c.eligible)
        .map(|c| c.owner_tool.as_str())
        .collect();

    let mut targets: Vec<HandoffTarget> = squads
        .iter()
        .filter(|s| Some(s.tool.as_str()) != self_tool)
        .filter(|s| !reclaim_owners.contains(s.tool.as_str()))
        .map(|s| HandoffTarget {
            tool: s.tool.clone(),
            freshness: s.freshness.clone(),
            age_secs: s.age_secs,
        })
        .collect();
    targets.sort_by(|a, b| {
        let ra = squads
            .iter()
            .find(|s| s.tool == a.tool)
            .map_or(1, squad_freshness_rank);
        let rb = squads
            .iter()
            .find(|s| s.tool == b.tool)
            .map_or(1, squad_freshness_rank);
        ra.cmp(&rb)
            .then_with(|| a.age_secs.unwrap_or(i64::MAX).cmp(&b.age_secs.unwrap_or(i64::MAX)))
            .then_with(|| a.tool.cmp(&b.tool))
    });

    // SEAM_NO_EXEC: these are STRINGS. Rally does not run them; the external
    // runner decides. Only eligible candidates get a command — proposing an
    // action for a held candidate would invite running it.
    let suggested_commands = candidates
        .iter()
        .filter(|c| c.eligible)
        .map(|c| {
            format!(
                "rally doctor --reap-stale --apply --json   # frees {} held by {}",
                c.claim_event_id, c.owner_tool
            )
        })
        .collect();

    Supervision {
        run_id: run_id.map(str::to_string),
        candidates,
        targets,
        facts_scanned: facts.len(),
        suggested_commands,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Fact;

    fn claim(id: &str, tool: &str, lease: &str, scope: &[&str]) -> Fact {
        Fact {
            event_id: id.to_string(),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            subject: format!("work by {tool}"),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            evidence: vec![format!("lease_expires_at:{lease}")],
            ..Default::default()
        }
    }

    fn squad(tool: &str, freshness: &str, age: i64) -> Squad {
        Squad {
            tool: tool.to_string(),
            freshness: freshness.to_string(),
            age_secs: Some(age),
            ..Default::default()
        }
    }

    const NOW: &str = "2026-08-17T12:00:00Z";
    const PAST: &str = "2026-08-17T10:00:00Z";
    const FUTURE: &str = "2026-08-17T14:00:00Z";

    fn stale_orphan(_t: &str) -> OwnerVerdict {
        OwnerVerdict {
            liveness: Liveness::Stale,
            parent_alive: None,
        }
    }

    #[test]
    fn expired_lease_plus_stale_owner_is_eligible() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let s = project_supervision(&facts, &[], None, NOW, None, stale_orphan);
        assert_eq!(s.candidates.len(), 1);
        assert!(s.candidates[0].eligible);
        assert!(s.candidates[0].held_because.is_none());
        assert_eq!(s.suggested_commands.len(), 1);
    }

    #[test]
    fn a_valid_lease_is_never_eligible_even_when_the_owner_is_stale() {
        let facts = vec![claim("c1", "agent:1", FUTURE, &[])];
        let s = project_supervision(&facts, &[], None, NOW, None, stale_orphan);
        assert!(!s.candidates[0].eligible);
        assert_eq!(s.candidates[0].held_because.as_deref(), Some("lease still valid"));
        assert!(s.suggested_commands.is_empty(), "held candidates get no command");
    }

    #[test]
    fn unknown_liveness_is_fail_closed_even_with_an_expired_lease() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let s = project_supervision(&facts, &[], None, NOW, None, |_| OwnerVerdict {
            liveness: Liveness::Unknown,
            parent_alive: None,
        });
        assert!(!s.candidates[0].eligible);
        assert!(
            s.candidates[0]
                .held_because
                .as_deref()
                .unwrap()
                .contains("fail-closed")
        );
    }

    #[test]
    fn a_live_owner_holds_its_claim_past_the_lease() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let s = project_supervision(&facts, &[], None, NOW, None, |_| OwnerVerdict {
            liveness: Liveness::Live,
            parent_alive: None,
        });
        assert!(!s.candidates[0].eligible);
        assert_eq!(s.candidates[0].held_because.as_deref(), Some("owner is live"));
    }

    #[test]
    fn a_stale_owner_with_a_live_parent_is_kept() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let s = project_supervision(&facts, &[], None, NOW, None, |_| OwnerVerdict {
            liveness: Liveness::Stale,
            parent_alive: Some(true),
        });
        assert!(!s.candidates[0].eligible);
        assert!(s.candidates[0].held_because.as_deref().unwrap().contains("parent"));
    }

    #[test]
    fn released_and_expired_claims_drop_out() {
        let mut released = Fact {
            event_id: "r1".to_string(),
            kind: FactKind::Release,
            ref_id: Some("c1".to_string()),
            ..Default::default()
        };
        released.tool = Some("agent:1".to_string());
        let facts = vec![claim("c1", "agent:1", PAST, &[]), released];
        let s = project_supervision(&facts, &[], None, NOW, None, stale_orphan);
        assert!(s.candidates.is_empty(), "a released claim is not a candidate");
    }

    #[test]
    fn a_renewal_supersedes_the_original_lease() {
        let renewal = Fact {
            event_id: "n1".to_string(),
            kind: FactKind::ClaimRenewed,
            ref_id: Some("c1".to_string()),
            evidence: vec![format!("lease_expires_at:{FUTURE}")],
            ..Default::default()
        };
        let facts = vec![claim("c1", "agent:1", PAST, &[]), renewal];
        let s = project_supervision(&facts, &[], None, NOW, None, stale_orphan);
        assert!(
            !s.candidates[0].eligible,
            "a renewed lease must beat the original expired one"
        );
    }

    #[test]
    fn run_id_filters_candidates_by_lineage() {
        let facts = vec![
            claim("c1", "agent:1", PAST, &["run:alpha", "step:p01"]),
            claim("c2", "agent:2", PAST, &["run:beta"]),
        ];
        let s = project_supervision(&facts, &[], Some("alpha"), NOW, None, stale_orphan);
        assert_eq!(s.candidates.len(), 1);
        assert_eq!(s.candidates[0].claim_event_id, "c1");
        assert_eq!(s.candidates[0].step.as_deref(), Some("p01"));
    }

    #[test]
    fn targets_rank_fresh_first_and_exclude_self_and_the_reclaimed_owner() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let squads = vec![
            squad("agent:1", "stale", 9_000), // reclaimed from — must not receive
            squad("agent:2", "stale", 8_000),
            squad("agent:3", "fresh", 30),
            squad("me", "fresh", 5), // self
        ];
        let s = project_supervision(&facts, &squads, None, NOW, Some("me"), stale_orphan);
        let tools: Vec<&str> = s.targets.iter().map(|t| t.tool.as_str()).collect();
        assert_eq!(tools, vec!["agent:3", "agent:2"]);
    }

    #[test]
    fn an_unparseable_now_never_expires_a_lease() {
        let facts = vec![claim("c1", "agent:1", PAST, &[])];
        let s = project_supervision(&facts, &[], None, "not-a-timestamp", None, stale_orphan);
        assert!(
            !s.candidates[0].eligible,
            "a clock we cannot read must not authorize a takeover"
        );
    }
}
