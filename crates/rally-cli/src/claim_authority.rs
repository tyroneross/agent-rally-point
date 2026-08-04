use crate::resource_scope::ResourceScope;
use crate::store::{Fact, FactKind};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub(crate) const CLAIM_INDEX_FILENAME: &str = "claim-index.json";
const LEASE_EVIDENCE_PREFIX: &str = "lease_expires_at:";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActiveClaimRecord {
    pub(crate) claim_id: String,
    pub(crate) owner_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from_session_id: Option<String>,
    pub(crate) raw_scope: Vec<String>,
    pub(crate) resource_scopes: Vec<ResourceScope>,
    pub(crate) lease_expires_at: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActiveClaimIndex {
    pub(crate) claims: BTreeMap<String, ActiveClaimRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClaimConflict {
    pub(crate) existing_claim_id: String,
    pub(crate) existing_owner: Option<String>,
    /// The scope the INCOMING claim asked for.
    pub(crate) scope: String,
    /// The scope the EXISTING owner actually holds.
    ///
    /// RC-037: the rejection message used to render `scope` in both slots, so
    /// a rogue holding `workspace:zzz` produced
    /// `claim conflict: codex:99 already owns file:src/lib.rs` — naming a file
    /// its owner had never claimed. The reader was told the wrong thing about
    /// who owns what, which is worse than no message at all when the next step
    /// is deciding whether to negotiate or take over.
    pub(crate) existing_scope: String,
}

/// Stamp a `lease_expires_at:` evidence marker with an EXPLICIT lease window
/// (seconds). Used so a claim's lease window can be SIZE-SCALED (a single-file
/// claim gets the small window, a coarse claim the large one) — see
/// `decay::reclaim_timeout_secs`. No-op when a lease marker already exists.
pub(crate) fn ensure_lease_evidence(evidence: &mut Vec<String>, lease_secs: i64) {
    if evidence
        .iter()
        .any(|item| item.starts_with(LEASE_EVIDENCE_PREFIX))
    {
        return;
    }
    evidence.push(format!(
        "{LEASE_EVIDENCE_PREFIX}{}",
        lease_marker_at(Utc::now(), lease_secs)
    ));
}

/// The `lease_expires_at` timestamp string for `now + lease_secs`.
/// (Distinct from `lease_expires_at(&Fact)` below, which READS a lease marker
/// off an existing claim.)
pub(crate) fn lease_marker_at(now: DateTime<Utc>, lease_secs: i64) -> String {
    (now + Duration::seconds(lease_secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn active_claim_records(facts: &[Fact]) -> Vec<ActiveClaimRecord> {
    facts
        .iter()
        .filter(|fact| is_active_claim_fact(fact, facts))
        .filter_map(active_claim_record_from_fact)
        .collect()
}

pub(crate) fn is_active_claim_fact(fact: &Fact, facts: &[Fact]) -> bool {
    fact.kind == FactKind::Claim
        && !fact.scope.iter().any(|scope| scope == "external-intake")
        && !claim_closed_by_later_fact(fact, facts)
}

fn claim_closed_by_later_fact(claim: &Fact, facts: &[Fact]) -> bool {
    facts.iter().any(|fact| {
        fact.seq > claim.seq
            && (later_fact_refs_claim(fact, claim.event_id.as_str())
                || later_release_overlaps_claim_scope(fact, claim))
    })
}

fn later_fact_refs_claim(fact: &Fact, claim_id: &str) -> bool {
    matches!(
        fact.kind,
        FactKind::Resolve | FactKind::Release | FactKind::Receipt | FactKind::ClaimExpired
    ) && fact.ref_id.as_deref() == Some(claim_id)
}

/// A `Release` closes EVERY active claim whose scope overlaps the release
/// scope, regardless of the claim's owner. This is intentional for the
/// multi-claim atomic-release contract (`command_release_by_path` writes ONE
/// release carrying the union of matched scopes and relies on this sweep).
///
/// Scope note (independent-auditor MED, 2026-06-09): on a NORMAL ledger this
/// cannot over-close a foreign claim, because same-scope different-owner claims
/// are rejected at append time (`store::append_fact` claim-conflict detection),
/// so two live claims never share a scope. The over-close risk is latent only
/// for an imported / hand-edited / corrupt ledger that already violates that
/// invariant — in which case the projection faithfully reflects the (already
/// inconsistent) ledger rather than masking it. The authorization gate that
/// decides WHO may write a takeover release lives in `command_release_by_path`
/// (2h stale-owner bar); this projection is downstream of that decision.
fn later_release_overlaps_claim_scope(fact: &Fact, claim: &Fact) -> bool {
    fact.kind == FactKind::Release
        && fact.scope.iter().any(|release_scope| {
            claim
                .scope
                .iter()
                .any(|claim_scope| claim_scope == release_scope)
        })
}

pub(crate) fn active_claim_record_from_fact(fact: &Fact) -> Option<ActiveClaimRecord> {
    let resource_scopes = fact
        .scope
        .iter()
        .filter_map(|scope| ResourceScope::parse_claim_scope(scope))
        .collect::<Vec<_>>();
    if resource_scopes.is_empty() {
        return None;
    }
    Some(ActiveClaimRecord {
        claim_id: fact.event_id.clone(),
        owner_tool: fact.tool.clone(),
        from_session_id: fact.from_session_id.clone(),
        raw_scope: fact.scope.clone(),
        resource_scopes,
        lease_expires_at: lease_expires_at(fact),
    })
}

pub(crate) fn detect_conflict(facts: &[Fact], incoming: &Fact) -> Option<ClaimConflict> {
    if incoming.kind != FactKind::Claim {
        return None;
    }
    let incoming = active_claim_record_from_fact(incoming)?;
    for existing in active_claim_records(facts) {
        if existing.owner_tool == incoming.owner_tool {
            continue;
        }
        for new_scope in &incoming.resource_scopes {
            for existing_scope in &existing.resource_scopes {
                if new_scope.conflicts_with(existing_scope) {
                    return Some(ClaimConflict {
                        existing_claim_id: existing.claim_id,
                        existing_owner: existing.owner_tool,
                        scope: new_scope.canonical_key(),
                        existing_scope: existing_scope.canonical_key(),
                    });
                }
            }
        }
    }
    None
}

/// The tool holding the lead seat, derived from the ledger.
///
/// Mirrors the projection in `store::snapshot_from_facts_with_policy`: the
/// latest lead-family decision wins, and a `role:lead:relinquished` reopens the
/// seat. Lifted out so the claim-breadth gate can answer "is this agent the
/// lead" without building a full room snapshot on every claim append.
pub(crate) fn lead_from_facts(facts: &[Fact]) -> Option<String> {
    facts
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::Decision
                && (fact.subject == "role:lead" || fact.subject == "role:lead:relinquished")
        })
        .max_by_key(|fact| fact.seq)
        .filter(|fact| fact.subject == "role:lead")
        .and_then(|fact| fact.tool.clone())
}

/// RC-037, second half: who may hold a ROOM-WIDE claim.
///
/// `workspace:*` and `repo:*` mean "I own everything in this room". That is a
/// legitimate thing for a lead to assert while it reorganizes a tree, and an
/// unacceptable thing for any agent to assert unilaterally — every other
/// agent's claim then fails to append, and (per RC-037) the hook swallowed
/// that failure, so deconfliction died room-wide with no signal.
///
/// The rule is deliberately narrow: it gates the EXPLICIT wildcard only. A
/// path-shaped or opaque coarse claim (`workspace:crates/rally-cli`,
/// `repo:agent-rally-point`) is unaffected, because after the `root_contains`
/// fix those no longer swallow the room.
///
/// Returns the refusal message when the claim must be rejected.
pub(crate) fn breadth_violation(incoming: &Fact, facts: &[Fact]) -> Option<String> {
    let record = active_claim_record_from_fact(incoming)?;
    let wildcard = record.resource_scopes.iter().find(|scope| {
        scope.resource_type.is_namespace_root()
            && scope.identifier == crate::resource_scope::WILDCARD_IDENTIFIER
    })?;

    let lead = lead_from_facts(facts);
    let claimer = record.owner_tool.as_deref().unwrap_or("<unknown tool>");
    match lead.as_deref() {
        Some(lead_tool) if lead_tool == claimer => None,
        Some(lead_tool) => Some(format!(
            "claim refused: scope {} is room-wide and only the lead may hold it. \
             {lead_tool} holds the lead seat, not {claimer}. Claim the specific paths \
             you are editing, or ask {lead_tool} to hand off the lead first.",
            wildcard.canonical_key()
        )),
        None => Some(format!(
            "claim refused: scope {} is room-wide and only the lead may hold it. \
             This room has no lead. Claim the specific paths you are editing, or take \
             the lead seat first with \
             `rally lead assign --tool {claimer} --to {claimer}`.",
            wildcard.canonical_key()
        )),
    }
}

pub(crate) fn index_from_facts(facts: &[Fact]) -> ActiveClaimIndex {
    let claims = active_claim_records(facts)
        .into_iter()
        .map(|claim| (claim.claim_id.clone(), claim))
        .collect();
    ActiveClaimIndex { claims }
}

pub(crate) fn write_index_from_facts(path: &Path, facts: &[Fact]) -> Result<(), String> {
    write_index(path, &index_from_facts(facts))
}

#[allow(dead_code)]
pub(crate) fn read_index(path: &Path) -> Result<ActiveClaimIndex, String> {
    if !path.exists() {
        return Ok(ActiveClaimIndex::default());
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("parse {}: {err}", path.display()))
}

#[allow(dead_code)]
pub(crate) fn renew_claim_lease(
    path: &Path,
    claim_id: &str,
    lease_expires_at: String,
) -> Result<Option<ActiveClaimRecord>, String> {
    let mut index = read_index(path)?;
    let Some(record) = index.claims.get_mut(claim_id) else {
        return Ok(None);
    };
    record.lease_expires_at = Some(lease_expires_at);
    let updated = record.clone();
    write_index(path, &index)?;
    Ok(Some(updated))
}

#[allow(dead_code)]
pub(crate) fn expired_claims(
    index: &ActiveClaimIndex,
    facts: &[Fact],
    now: DateTime<Utc>,
) -> Vec<ActiveClaimRecord> {
    let already_expired = facts
        .iter()
        .filter(|fact| fact.kind == FactKind::ClaimExpired)
        .filter_map(|fact| fact.ref_id.clone())
        .collect::<BTreeSet<_>>();

    index
        .claims
        .values()
        .filter(|claim| !already_expired.contains(&claim.claim_id))
        .filter(|claim| {
            claim
                .lease_expires_at
                .as_deref()
                .and_then(parse_time)
                .is_some_and(|expires| expires <= now)
        })
        .cloned()
        .collect()
}

fn write_index(path: &Path, index: &ActiveClaimIndex) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(index).map_err(|err| format!("render index: {err}"))?;
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temp, text).map_err(|err| format!("write {}: {err}", temp.display()))?;
    fs::rename(&temp, path).map_err(|err| format!("replace {}: {err}", path.display()))
}

fn lease_expires_at(fact: &Fact) -> Option<String> {
    fact.evidence
        .iter()
        .find_map(|item| item.strip_prefix(LEASE_EVIDENCE_PREFIX))
        .map(str::to_string)
}

#[allow(dead_code)]
fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FACT_SCHEMA, now_string};

    fn fact(id: &str, tool: &str, scope: Vec<&str>) -> Fact {
        Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: id.to_string(),
            seq: id.bytes().map(i64::from).sum::<i64>(),
            thread_id: "thread".to_string(),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: "claim".to_string(),
            scope: scope.into_iter().map(str::to_string).collect(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    #[test]
    fn claim_authority_rejects_conflicting_exclusive_owner() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let incoming = fact("claim-b", "tool-b", vec!["file:./src/lib.rs"]);
        let conflict = detect_conflict(&[existing], &incoming).unwrap();
        assert_eq!(conflict.existing_claim_id, "claim-a");
        assert_eq!(conflict.scope, "file:src/lib.rs");
    }

    fn lead_decision(tool: &str, seq: i64) -> Fact {
        Fact {
            kind: FactKind::Decision,
            subject: "role:lead".to_string(),
            seq,
            ..fact("lead-decision", tool, vec![])
        }
    }

    /// RC-037 adversarial control, message half. Revert the `existing_scope`
    /// field and this fails: the rejection named the scope the CLAIMER asked
    /// for as the scope the OWNER holds, so a rogue holding `workspace:zzz`
    /// produced "codex:99 already owns file:src/lib.rs".
    #[test]
    fn claim_conflict_names_the_scope_the_owner_actually_holds() {
        let existing = fact("claim-a", "tool-a", vec!["workspace:*"]);
        let incoming = fact("claim-b", "tool-b", vec!["file:src/lib.rs"]);
        let conflict = detect_conflict(&[existing], &incoming).unwrap();
        assert_eq!(
            conflict.existing_scope, "workspace:*",
            "the message must report what the owner holds"
        );
        assert_eq!(
            conflict.scope, "file:src/lib.rs",
            "and separately what was requested"
        );
    }

    /// RC-037 adversarial control, lockout half, at the authority layer.
    /// Remove the `breadth_violation` call and a non-lead can hold
    /// `workspace:*`, which conflicts with every later claim in the room.
    #[test]
    fn breadth_gate_refuses_room_wide_claim_from_a_non_lead() {
        let lead = lead_decision("tool-lead", 1);
        let incoming = fact("claim-rogue", "tool-rogue", vec!["workspace:*"]);
        let refusal = breadth_violation(&incoming, &[lead])
            .expect("a non-lead must not hold a room-wide claim");
        assert!(refusal.contains("only the lead may hold it"), "{refusal}");
        assert!(refusal.contains("tool-lead"), "{refusal}");
    }

    #[test]
    fn breadth_gate_refuses_room_wide_claim_when_the_room_has_no_lead() {
        let incoming = fact("claim-rogue", "tool-rogue", vec!["repo:*"]);
        let refusal =
            breadth_violation(&incoming, &[]).expect("no lead means nobody may claim room-wide");
        assert!(refusal.contains("no lead"), "{refusal}");
    }

    /// The lead keeps the capability — breadth is authority-gated, not banned.
    #[test]
    fn breadth_gate_allows_the_lead_to_claim_room_wide() {
        let lead = lead_decision("tool-lead", 1);
        let incoming = fact("claim-lead", "tool-lead", vec!["workspace:*"]);
        assert!(breadth_violation(&incoming, &[lead]).is_none());
    }

    /// The gate is narrow on purpose: a coarse-but-specific claim is ordinary
    /// work and must not need the lead seat.
    #[test]
    fn breadth_gate_ignores_non_wildcard_coarse_claims() {
        for scope in ["workspace:crates/rally-cli", "repo:agent-rally-point"] {
            let incoming = fact("claim-x", "tool-x", vec![scope]);
            assert!(
                breadth_violation(&incoming, &[]).is_none(),
                "{scope} is bounded and must not require the lead seat"
            );
        }
    }

    #[test]
    fn lead_from_facts_reopens_the_seat_on_relinquish() {
        let claimed = lead_decision("tool-lead", 1);
        let relinquished = Fact {
            subject: "role:lead:relinquished".to_string(),
            seq: 2,
            ..lead_decision("tool-lead", 2)
        };
        assert_eq!(
            lead_from_facts(std::slice::from_ref(&claimed)).as_deref(),
            Some("tool-lead")
        );
        assert_eq!(lead_from_facts(&[claimed, relinquished]), None);
    }

    #[test]
    fn claim_authority_allows_same_owner_idempotent_overlap() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let incoming = fact("claim-b", "tool-a", vec!["file:./src/lib.rs"]);
        assert!(detect_conflict(&[existing], &incoming).is_none());
    }

    #[test]
    fn active_claim_record_preserves_authoring_session_id() {
        let mut claim = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        claim.from_session_id = Some("sess:term:host:abc#live".to_string());

        let record = active_claim_record_from_fact(&claim).unwrap();

        assert_eq!(
            record.from_session_id.as_deref(),
            Some("sess:term:host:abc#live")
        );
    }

    #[test]
    fn claim_authority_rebuild_ignores_released_claims() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let release = Fact {
            kind: FactKind::Release,
            ref_id: Some("claim-a".to_string()),
            seq: existing.seq + 1,
            ..fact("release-a", "tool-a", vec![])
        };
        let index = index_from_facts(&[existing, release]);
        assert!(index.claims.is_empty());
    }

    #[test]
    fn claim_authority_old_release_does_not_suppress_later_claim_same_scope() {
        let old_claim = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let release = Fact {
            kind: FactKind::Release,
            ref_id: Some("claim-a".to_string()),
            scope: vec!["file:src/lib.rs".to_string()],
            seq: old_claim.seq + 1,
            ..fact("release-a", "tool-a", vec![])
        };
        let mut later_claim = fact("claim-b", "tool-b", vec!["file:src/lib.rs"]);
        later_claim.seq = release.seq + 1;

        let index = index_from_facts(&[old_claim, release, later_claim]);

        assert_eq!(index.claims.len(), 1);
        assert!(index.claims.contains_key("claim-b"));
    }

    #[test]
    fn claim_authority_lease_renewal_is_index_only() {
        let temp = std::env::temp_dir().join(format!(
            "rally-claim-index-test-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut claim = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        ensure_lease_evidence(
            &mut claim.evidence,
            crate::decay::DEFAULT_RECLAIM_SMALL_MINUTES * 60,
        );
        write_index_from_facts(&temp, &[claim]).unwrap();
        let before_len = read_index(&temp).unwrap().claims.len();
        let renewed = renew_claim_lease(&temp, "claim-a", "2099-01-01T00:00:00Z".to_string())
            .unwrap()
            .unwrap();
        let after = read_index(&temp).unwrap();
        assert_eq!(before_len, 1);
        assert_eq!(after.claims.len(), 1);
        assert_eq!(
            renewed.lease_expires_at.as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        let _ = fs::remove_file(temp);
    }

    #[test]
    fn claim_authority_expiry_emits_each_claim_once() {
        let mut claim = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        claim
            .evidence
            .push("lease_expires_at:2000-01-01T00:00:00Z".to_string());
        let index = index_from_facts(&[claim.clone()]);
        let expired = expired_claims(&index, &[claim.clone()], Utc::now());
        assert_eq!(expired.len(), 1);

        let expired_event = Fact {
            kind: FactKind::ClaimExpired,
            ref_id: Some("claim-a".to_string()),
            seq: claim.seq + 1,
            ..fact("expired-a", "rally", vec![])
        };
        let expired_again = expired_claims(&index, &[claim, expired_event], Utc::now());
        assert!(expired_again.is_empty());
    }
}
