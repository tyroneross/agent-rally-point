use crate::resource_scope::ResourceScope;
use crate::store::{Fact, FactKind};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub(crate) const CLAIM_INDEX_FILENAME: &str = "claim-index.json";
const DEFAULT_CLAIM_LEASE_SECS: i64 = 30 * 60;
const LEASE_EVIDENCE_PREFIX: &str = "lease_expires_at:";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActiveClaimRecord {
    pub(crate) claim_id: String,
    pub(crate) owner_tool: Option<String>,
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
    pub(crate) scope: String,
}

pub(crate) fn ensure_default_lease_evidence(evidence: &mut Vec<String>) {
    if evidence
        .iter()
        .any(|item| item.starts_with(LEASE_EVIDENCE_PREFIX))
    {
        return;
    }
    evidence.push(format!(
        "{LEASE_EVIDENCE_PREFIX}{}",
        default_lease_expires_at(Utc::now())
    ));
}

pub(crate) fn default_lease_expires_at(now: DateTime<Utc>) -> String {
    (now + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339_opts(SecondsFormat::Secs, true)
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
                    });
                }
            }
        }
    }
    None
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

    #[test]
    fn claim_authority_allows_same_owner_idempotent_overlap() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let incoming = fact("claim-b", "tool-a", vec!["file:./src/lib.rs"]);
        assert!(detect_conflict(&[existing], &incoming).is_none());
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
        ensure_default_lease_evidence(&mut claim.evidence);
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
