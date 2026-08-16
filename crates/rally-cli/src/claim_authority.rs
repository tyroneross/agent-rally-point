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
        .filter_map(|fact| active_claim_record_from_facts(fact, facts))
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

/// The fact kinds that CLOSE an active claim.
///
/// ARP-R-02. This used to be a `matches!` arm inside `later_fact_refs_claim`
/// and a SECOND, hand-copied pair of match arms in `store.rs` deciding which
/// closes were authorization-checked. The two drifted immediately: the
/// projection closed on four kinds, the gate covered two, and `Receipt` and
/// `ClaimExpired` reached the ledger with no ownership check at all. A rogue
/// posting `rally say receipt --tool rogue --ref <claim-id>` took any live
/// claim, seconds old, with 30 minutes of lease remaining — reproduced end to
/// end, `active_claims` 1 -> 0 -> 1 with the owner flipped.
///
/// One list, read by BOTH the projection and the write gate, so a fifth
/// closing kind cannot silently add a fifth bypass: adding a kind here changes
/// what closes a claim and what must be authorized to close one, in the same
/// edit.
pub(crate) fn closes_active_claim(kind: &FactKind) -> bool {
    matches!(
        kind,
        FactKind::Resolve | FactKind::Release | FactKind::Receipt | FactKind::ClaimExpired
    )
}

fn later_fact_refs_claim(fact: &Fact, claim_id: &str) -> bool {
    closes_active_claim(&fact.kind) && fact.ref_id.as_deref() == Some(claim_id)
}

/// A `Release` closes EVERY active claim whose scope overlaps the release
/// scope, regardless of the claim's owner. This is intentional for the
/// multi-claim atomic-release contract (`command_release_by_path` writes ONE
/// release carrying the union of matched scopes and relies on this sweep).
///
/// # R5 — the scope note that used to live here was wrong, and how
///
/// It read: on a NORMAL ledger this cannot over-close a foreign claim, because
/// same-scope different-owner claims are rejected at append time
/// (`store::append_fact` claim-conflict detection), so two live claims never
/// share a scope; the over-close risk is latent only for an imported or corrupt
/// ledger.
///
/// The premise is true and the conclusion does not follow. Claim-conflict
/// detection runs on CLAIMS. A release is not a claim, so nothing ever checked
/// that a release's free-text `--scope` was a scope its author had claimed. A
/// rogue could name the victim's path directly:
///
/// ```text
/// rally say release --tool rogue --ref <rogue's own claim> --scope file:<victim's path>
/// ```
///
/// `assert_claim_close_authorized` authorized the claim named by `ref_id` —
/// the rogue's own, so arm 1 passed — and never read `fact.scope` at all,
/// while this sweep closed the victim's claim on the scope match. The
/// authorization and the effect were keyed off two different fields.
///
/// The gate now checks every claim this predicate would sweep, by CALLING this
/// predicate rather than restating it (`write_authority::assert_release_sweep_authorized`).
/// Sweep and authorization read the same rule from the same place, so the two
/// cannot drift the way the four closing kinds and their two-kind gate did in
/// ARP-R-02.
pub(crate) fn later_release_overlaps_claim_scope(fact: &Fact, claim: &Fact) -> bool {
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

/// Project one active claim with its latest valid durable renewal applied.
///
/// A renewal is valid only when it references the claim and names the same
/// owner. That keeps a hand-edited or future untrusted ledger row from
/// extending another tool's claim merely by guessing its event id.
pub(crate) fn active_claim_record_from_facts(
    fact: &Fact,
    facts: &[Fact],
) -> Option<ActiveClaimRecord> {
    let mut record = active_claim_record_from_fact(fact)?;
    if let Some(expires_at) = latest_renewed_lease(fact, facts) {
        record.lease_expires_at = Some(expires_at);
    }
    Some(record)
}

/// Return the active claim record for `claim_id`, including durable renewal.
pub(crate) fn active_claim_record(facts: &[Fact], claim_id: &str) -> Option<ActiveClaimRecord> {
    facts
        .iter()
        .find(|fact| fact.event_id == claim_id && is_active_claim_fact(fact, facts))
        .and_then(|fact| active_claim_record_from_facts(fact, facts))
}

/// Clone an active claim fact with its effective lease marker projected into
/// evidence. The original event id, owner, scope, and authored timestamp stay
/// unchanged; only the derived lease view advances.
pub(crate) fn project_effective_claim(fact: &Fact, facts: &[Fact]) -> Fact {
    let mut projected = fact.clone();
    if let Some(expires_at) = latest_renewed_lease(fact, facts) {
        projected
            .evidence
            .retain(|item| !item.starts_with(LEASE_EVIDENCE_PREFIX));
        projected
            .evidence
            .push(format!("{LEASE_EVIDENCE_PREFIX}{expires_at}"));
    }
    projected
}

pub(crate) fn latest_renewed_lease(claim: &Fact, facts: &[Fact]) -> Option<String> {
    facts
        .iter()
        .filter(|fact| {
            fact.kind == FactKind::ClaimRenewed
                && fact.ref_id.as_deref() == Some(claim.event_id.as_str())
                && claim_owner_matches_caller(
                    claim.tool.as_deref(),
                    claim.from_session_id.as_deref(),
                    fact.tool.as_deref(),
                    fact.from_session_id.as_deref(),
                )
        })
        .filter_map(|fact| {
            let lease = lease_expires_at(fact)?;
            let parsed = parse_time(&lease)?;
            Some((parsed, lease))
        })
        // Projection remains monotonic even for an imported or hand-edited
        // ledger that bypassed the append-boundary ordering guard.
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, lease)| lease)
}

/// R3. Retraction resolution, applied over the caller's in-memory `facts`.
///
/// The room projection drops retracted facts BEFORE projecting active claims
/// (`store::snapshot_from_facts_with_policy_at`); this function reads raw
/// facts. Without the same resolution the two disagree, and the disagreement
/// has a direction: a retracted claim is invisible in `rally room` and in
/// `check before-write`, yet still refuses every later claim on its scope. The
/// owner is told to negotiate with a claim nobody can see.
///
/// Resolved by REUSING the projection's own filter rather than special-casing
/// claims, so the two answers agree by construction. That also covers the
/// second-order case: a `release` that was itself retracted no longer closes
/// the claim it named, exactly as the projection already has it.
///
/// Cost: one pass over facts the caller already holds, and the clone only when
/// the room actually contains a retraction. No additional ledger read — the
/// same pattern and the same reasoning as the snapshot core.
fn resolve_retractions(facts: &[Fact]) -> Option<Vec<Fact>> {
    let retracted = crate::retraction::retracted_ids(facts);
    if retracted.is_empty() {
        return None;
    }
    Some(
        facts
            .iter()
            .filter(|f| !retracted.contains(&f.event_id))
            .cloned()
            .collect(),
    )
}

pub(crate) fn detect_conflict(facts: &[Fact], incoming: &Fact) -> Option<ClaimConflict> {
    if incoming.kind != FactKind::Claim {
        return None;
    }
    let resolved = resolve_retractions(facts);
    let facts: &[Fact] = resolved.as_deref().unwrap_or(facts);
    let incoming = active_claim_record_from_fact(incoming)?;
    for existing in active_claim_records(facts) {
        if same_session_owner(
            existing.owner_tool.as_deref(),
            existing.from_session_id.as_deref(),
            incoming.owner_tool.as_deref(),
            incoming.from_session_id.as_deref(),
        ) {
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

/// Whether two facts belong to the same lease owner.
///
/// Session identity plus tool family is authoritative when present. Exact
/// tool equality is a compatibility fallback only when both facts predate
/// session stamping. A sessionful fact never aliases a sessionless fact, and
/// sibling sessions of one tool never inherit each other's authority.
pub(crate) fn same_session_owner(
    left_tool: Option<&str>,
    left_session: Option<&str>,
    right_tool: Option<&str>,
    right_session: Option<&str>,
) -> bool {
    match (left_session, right_session) {
        // A protocol session plus tool family is the principal. Host hooks and
        // orchestrated agents may use different suffixes inside one host
        // session, while unrelated tool families sharing a terminal session
        // remain distinct peers.
        (Some(left), Some(right)) => {
            !left.trim().is_empty() && left == right && same_tool_family(left_tool, right_tool)
        }
        (None, None) => same_nonblank_tool(left_tool, right_tool),
        _ => false,
    }
}

fn same_tool_family(left_tool: Option<&str>, right_tool: Option<&str>) -> bool {
    fn family(tool: &str) -> &str {
        tool.trim().split(':').next().unwrap_or_default()
    }
    matches!(
        (left_tool, right_tool),
        (Some(left), Some(right))
            if !family(left).is_empty() && family(left) == family(right)
    )
}

/// Whether two identities assert the same present, nonblank tool exactly.
///
/// Absence is not an identity. In particular, `(None, None)` and two blank
/// strings never become owner authority merely because `Option` equality says
/// they have the same shape.
pub(crate) fn same_nonblank_tool(left_tool: Option<&str>, right_tool: Option<&str>) -> bool {
    matches!(
        (left_tool, right_tool),
        (Some(left), Some(right)) if !left.trim().is_empty() && left == right
    )
}

/// Whether `caller` owns `claim`, including the one-way legacy fallback.
///
/// Session identity is exact whenever the claim carries one. A historical
/// sessionless claim may still be acted on by a modern session that asserts the
/// same present, nonblank tool; the converse is never allowed.
pub(crate) fn claim_owner_matches_caller(
    claim_tool: Option<&str>,
    claim_session: Option<&str>,
    caller_tool: Option<&str>,
    caller_session: Option<&str>,
) -> bool {
    same_session_owner(claim_tool, claim_session, caller_tool, caller_session)
        || (claim_session.is_none() && same_nonblank_tool(claim_tool, caller_tool))
}

/// Is this a lead-family decision (seat taken, or seat reopened)?
pub(crate) fn is_lead_decision(fact: &Fact) -> bool {
    fact.kind == FactKind::Decision
        && (fact.subject == LEAD_SUBJECT || fact.subject == LEAD_RELINQUISHED_SUBJECT)
}

pub(crate) const LEAD_SUBJECT: &str = "role:lead";
pub(crate) const LEAD_RELINQUISHED_SUBJECT: &str = "role:lead:relinquished";

/// Who a lead-family decision names as the INCOMING lead.
///
/// ARP-R-01, attribution half. `set_lead` used to stamp `fact.tool = <the
/// beneficiary>`, so the ledger recorded a seizure as authored by the agent
/// that GAINED the seat — the one field an investigator would read to find out
/// who took it named the wrong agent, and no authorization gate could be built
/// on `fact.tool` because it did not hold the actor.
///
/// New lead facts stamp `tool` = the ACTOR and `target` = the BENEFICIARY.
/// Legacy facts (three exist in this repo's ledger, all pre-dating the fix)
/// carry no `target`, so `tool` is still read as the beneficiary for those. The
/// ledger is append-only; an old room must keep replaying to the same lead.
pub(crate) fn lead_beneficiary(fact: &Fact) -> Option<String> {
    fact.target.clone().or_else(|| fact.tool.clone())
}

/// The tool holding the lead seat AS OF `seq` — that is, considering only
/// lead-family decisions at or before `seq`.
///
/// ARP-R-01, retroactive half. Every lead-gated control used to compare against
/// the CURRENT lead, which made the gate's verdict a function of the room's
/// present state rather than of the fact being judged. Live consequence, both
/// directions, on the SAME fact id:
///
/// * **Retroactive arm.** A non-lead posts an unscoped blocker; it projects as
///   `unscoped-blocker` / allow:true. That agent later takes the seat, and the
///   same blocker re-projects as `room-freeze` / allow:false — a room-wide DoS
///   armed after the fact, by a write that touched nothing.
/// * **Retroactive disarm.** The honest lead declares a freeze; anyone else
///   takes the seat and the freeze degrades to allow:true. The room's only stop
///   control was removable in one command.
///
/// Authority is a property of the moment a fact was written. A fact carries its
/// own `seq`, so ask the question at that point.
pub(crate) fn lead_as_of(facts: &[Fact], seq: i64) -> Option<String> {
    lead_of(facts.iter().filter(move |fact| fact.seq <= seq))
}

/// The seat derivation itself: latest lead-family decision wins, and a
/// `role:lead:relinquished` reopens the seat.
///
/// ONE body, FOUR entry points — [`lead_as_of`], [`projected_lead`],
/// [`projected_lead_with_retraction`], and the room projection itself via
/// [`lead_and_epoch_of`] — for the same reason
/// `write_authority::authorize_claim_removal` is one body: ARP-R-01 was two
/// projections of one fact drifting apart, and the answer to that is not a
/// third copy written more carefully.
///
/// The fourth entry point is the load-bearing one. RC-071a's whole correctness
/// argument is "the gate reads the same seat the room shows", and until the
/// projection called this, that agreement was held by a hand-copy in
/// `store::snapshot_from_facts_with_policy_at` — the literal shape of ARP-R-01,
/// sitting underneath the gate that cites ARP-R-01 as its reason to share.
fn lead_of<'a>(facts: impl Iterator<Item = &'a Fact>) -> Option<String> {
    facts
        .filter(|fact| is_lead_decision(fact))
        .max_by_key(|fact| fact.seq)
        .filter(|fact| fact.subject == LEAD_SUBJECT)
        .and_then(lead_beneficiary)
}

/// The seat AND its epoch, for the room projection.
///
/// The epoch is the latest lead-family decision's `seq` — a cheap staleness
/// handle for agents — and it is a property of the FACT rather than of the
/// seat, which is why the projection computed it locally and ended up
/// re-deriving the seat alongside it. Both come from one pass here now, so the
/// projection and the gates cannot answer differently.
pub(crate) fn lead_and_epoch_of(facts: &[Fact]) -> (Option<String>, Option<i64>) {
    let epoch = facts
        .iter()
        .filter(|fact| is_lead_decision(fact))
        .map(|fact| fact.seq)
        .max();
    (lead_of(facts.iter()), epoch)
}

/// The tool holding the lead seat, as the ROOM PROJECTION reports it —
/// retraction-resolved, and the answer every gate outside the projection should
/// be asking for. Lets a gate answer "is this agent the lead" without building
/// a full room snapshot on every append.
///
/// RC-071a. The predecessor of this function (`lead_from_facts`) read RAW
/// facts, so it counted a lead decision that had been WITHDRAWN.
/// `store::snapshot_from_facts_with_policy_at` drops
/// retracted facts from every bucket before deriving the seat, so the two
/// disagreed the moment any lead decision was retracted: `rally room` reported
/// no lead while [`breadth_violation`] still reported one. That is the same
/// projection-vs-gate split R3 fixed for claims, on the seat instead.
///
/// Resolution reuses [`crate::retraction::retracted_ids`] — the projection's
/// own filter — rather than restating it, so the two answers agree by
/// construction. That includes agreeing where the filter is BLUNT: resolution
/// is flat, so retracting a retraction does not restore its target, here or in
/// `rally room`.
pub(crate) fn projected_lead(facts: &[Fact]) -> Option<String> {
    projected_lead_with_retraction(facts, None)
}

/// [`projected_lead`], evaluated as if `also_retracted` were withdrawn too.
///
/// This is how the write gate asks the question it actually needs answered —
/// "does admitting this retraction MOVE the seat?" — without predicting which
/// spellings can move it. The oracle is the projection, run twice.
pub(crate) fn projected_lead_with_retraction(
    facts: &[Fact],
    also_retracted: Option<&str>,
) -> Option<String> {
    let mut retracted = crate::retraction::retracted_ids(facts);
    if let Some(target) = also_retracted {
        retracted.insert(target.to_string());
    }
    lead_of(facts.iter().filter(|f| !retracted.contains(&f.event_id)))
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
///
/// # RC-071a: which lead this reads, and why that changed
///
/// This used to call [`lead_from_facts`] on RAW facts, so a lead decision that
/// had been retracted still conferred room-wide capability here while
/// `rally room` already reported no lead. That was left open deliberately and
/// recorded as owed, because resolving it in isolation would have been the
/// worse defect: retraction of a lead decision was UNGATED, so reading the
/// resolved seat would have let any agent strip the lead's room-wide authority
/// with one `rally retract`.
///
/// The seat's own removal is gated now
/// (`write_authority::assert_lead_retraction_authorized`), so the reason to
/// keep the disagreement is gone and this reads [`projected_lead`] — the same
/// seat `rally room` shows. A legitimately withdrawn lead decision now costs
/// the room-wide capability it should never have kept conferring.
pub(crate) fn breadth_violation(incoming: &Fact, facts: &[Fact]) -> Option<String> {
    let record = active_claim_record_from_fact(incoming)?;
    let wildcard = record.resource_scopes.iter().find(|scope| {
        scope.resource_type.is_namespace_root()
            && scope.identifier == crate::resource_scope::WILDCARD_IDENTIFIER
    })?;

    let lead = projected_lead(facts);
    let claimer = record.owner_tool.as_deref().unwrap_or("<unknown tool>");
    match lead.as_deref() {
        Some(lead_tool) if lead_tool == claimer => None,
        Some(lead_tool) => Some(format!(
            "claim refused: scope {} is room-wide and only the lead may hold it. \
             {lead_tool} holds the lead seat, not {claimer}. Claim the specific paths \
             you are editing, or ask {lead_tool} to hand off the lead first.",
            wildcard.canonical_key()
        )),
        // ARP-R-01. This message used to end with
        // "…or take the lead seat first with
        //  `rally lead assign --tool {claimer} --to {claimer}`."
        //
        // It printed the bypass to the caller it had just refused. The gate's
        // whole content is "only the lead may do this", and the refusal handed
        // over the one command that made the reader the lead — an unauthorized
        // agent was told, in the refusal itself, exactly how to become
        // authorized. `set_lead` had no precondition at the time, so the
        // instruction worked.
        //
        // The seat is gated now (`write_authority::assert_lead_transfer_authorized`),
        // so the command no longer succeeds against a live incumbent under a
        // rogue's own name. The instruction still does not belong here: a
        // refusal explains the boundary and names who can move it. It does not
        // coach around it.
        None => Some(format!(
            "claim refused: scope {} is room-wide and only the lead may hold it. \
             This room has no lead, so no agent currently holds room-wide authority. \
             Claim the specific paths you are editing.",
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
    fn host_and_orchestrator_tool_ids_in_one_session_are_one_claim_principal() {
        let mut host = fact(
            "claim-host",
            "claude_code:5b130dd1-a78f-4658-b1d6-283a3898b437",
            vec!["file:src/lib.rs"],
        );
        host.from_session_id = Some("sess:term:host:shared#live".to_string());
        let mut orchestrator = fact(
            "claim-orchestrator",
            "claude_code:release-cleanup-c5f8ebd7",
            vec!["file:src/lib.rs"],
        );
        orchestrator.from_session_id = Some("sess:term:host:shared#live".to_string());

        assert!(
            detect_conflict(std::slice::from_ref(&host), &orchestrator).is_none(),
            "tool labels inside one host session must not conflict with their own auto-claim"
        );

        orchestrator.from_session_id = Some("sess:term:host:sibling#live".to_string());
        assert!(
            detect_conflict(&[host], &orchestrator).is_some(),
            "a different host session must remain a distinct claim principal"
        );
    }

    #[test]
    fn unrelated_tool_families_in_one_terminal_session_remain_distinct() {
        assert!(!same_session_owner(
            Some("victim:01"),
            Some("sess:term:shared#live"),
            Some("codex:rogue"),
            Some("sess:term:shared#live")
        ));
    }

    /// A retraction exactly as `rally retract` writes it.
    fn retraction(id: &str, target: &str, seq: i64) -> Fact {
        let mut f = fact(id, "retractor", vec![]);
        f.kind = FactKind::Artifact;
        f.seq = seq;
        f.subject = crate::retraction::subject_for(target);
        f.ref_id = Some(target.to_string());
        f
    }

    /// R3, THE defect. `rally room` filters retracted facts before projecting
    /// active claims; `detect_conflict` read raw facts. A retracted claim was
    /// therefore invisible in the room and in `check before-write`, yet still
    /// refused every later claim on its scope — the owner was told to negotiate
    /// with a claim nobody could see.
    #[test]
    fn a_retracted_claim_no_longer_blocks_its_scope() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let incoming = fact("claim-b", "tool-b", vec!["file:src/lib.rs"]);
        assert!(
            detect_conflict(std::slice::from_ref(&existing), &incoming).is_some(),
            "baseline: a live claim must still block"
        );

        let withdrawn = retraction("r-1", "claim-a", 900);
        assert!(
            detect_conflict(&[existing, withdrawn], &incoming).is_none(),
            "a withdrawn claim must stop blocking, matching what `rally room` shows"
        );
    }

    /// Second order, and the reason this reuses the projection's own filter
    /// rather than special-casing claims: a `release` that was ITSELF retracted
    /// no longer closes the claim it named, so the claim is live again and
    /// blocks again — exactly as the projection already has it.
    #[test]
    fn a_retracted_release_leaves_its_claim_blocking_again() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let mut release = fact("rel-1", "tool-a", vec![]);
        release.kind = FactKind::Release;
        release.ref_id = Some("claim-a".to_string());
        release.seq = 800;
        let incoming = fact("claim-b", "tool-b", vec!["file:src/lib.rs"]);

        assert!(
            detect_conflict(&[existing.clone(), release.clone()], &incoming).is_none(),
            "baseline: a released claim does not block"
        );

        let undo = retraction("r-1", "rel-1", 900);
        let conflict = detect_conflict(&[existing, release, undo], &incoming)
            .expect("withdrawing the release must revive the claim it closed");
        assert_eq!(conflict.existing_claim_id, "claim-a");
    }

    /// The token carrier is gone (R2), so a summary token cannot silently
    /// un-block a scope its author does not own.
    #[test]
    fn a_summary_token_does_not_unblock_a_scope() {
        let existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        let mut noise = fact("n-1", "tool-b", vec![]);
        noise.kind = FactKind::Artifact;
        noise.seq = 900;
        noise.subject = "just a note".to_string();
        noise.summary = Some("[retracts=claim-a]".to_string());
        let incoming = fact("claim-b", "tool-b", vec!["file:src/lib.rs"]);

        assert!(
            detect_conflict(&[existing, noise], &incoming).is_some(),
            "a token-only fact withdraws nothing, so the claim must still block"
        );
    }

    #[test]
    fn same_tool_sibling_session_cannot_bypass_claim_conflict() {
        let mut existing = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        existing.from_session_id = Some("session-a".to_string());
        existing.evidence = vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()];
        let mut incoming = fact("claim-b", "tool-a", vec!["file:src/lib.rs"]);
        incoming.from_session_id = Some("session-b".to_string());

        let conflict = detect_conflict(&[existing], &incoming)
            .expect("an expired lease remains held until session-aware reaping closes it");
        assert_eq!(conflict.existing_claim_id, "claim-a");
    }

    #[test]
    fn anonymous_or_blank_tools_are_never_the_same_owner() {
        assert!(!same_session_owner(None, None, None, None));
        assert!(!same_session_owner(Some(""), None, Some(""), None));
        assert!(!same_session_owner(
            Some("  "),
            Some("session-a"),
            Some("  "),
            Some("session-a")
        ));
        assert!(same_session_owner(
            Some("tool-a"),
            None,
            Some("tool-a"),
            None
        ));
        assert!(claim_owner_matches_caller(
            Some("tool-a"),
            None,
            Some("tool-a"),
            Some("session-modern")
        ));
        assert!(!claim_owner_matches_caller(None, None, None, None));
    }

    #[test]
    fn anonymous_renewal_never_projects_as_owner_activity() {
        let mut claim = fact("claim-anonymous", "placeholder", vec!["file:src/lib.rs"]);
        claim.tool = None;
        claim.from_session_id = None;
        let mut renewal = Fact {
            kind: FactKind::ClaimRenewed,
            event_id: "renewal-anonymous".to_string(),
            ref_id: Some(claim.event_id.clone()),
            from_session_id: None,
            evidence: vec!["lease_expires_at:2099-01-01T00:00:00Z".to_string()],
            ..fact("renewal-template", "placeholder", vec![])
        };
        renewal.tool = None;

        assert_eq!(latest_renewed_lease(&claim, &[renewal]), None);
    }

    #[test]
    fn sibling_renewal_does_not_extend_claim_lease() {
        let mut claim = fact("claim-a", "tool-a", vec!["file:src/lib.rs"]);
        claim.from_session_id = Some("session-a".to_string());
        claim.evidence = vec!["lease_expires_at:2026-01-01T00:00:00Z".to_string()];
        let renewal = Fact {
            kind: FactKind::ClaimRenewed,
            event_id: "renewal-b".to_string(),
            ref_id: Some(claim.event_id.clone()),
            from_session_id: Some("session-b".to_string()),
            evidence: vec!["lease_expires_at:2099-01-01T00:00:00Z".to_string()],
            ..fact("renewal-template", "tool-a", vec![])
        };

        assert_eq!(
            latest_renewed_lease(&claim, &[renewal]),
            None,
            "a same-tool sibling must not renew another session's lease"
        );
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
    fn projected_lead_reopens_the_seat_on_relinquish() {
        let claimed = lead_decision("tool-lead", 1);
        let relinquished = Fact {
            subject: "role:lead:relinquished".to_string(),
            seq: 2,
            ..lead_decision("tool-lead", 2)
        };
        assert_eq!(
            projected_lead(std::slice::from_ref(&claimed)).as_deref(),
            Some("tool-lead")
        );
        assert_eq!(projected_lead(&[claimed, relinquished]), None);
    }

    /// RC-071a, the register entry's own subject. The room projection drops a
    /// retracted lead decision before deriving the seat; this gate read raw
    /// facts, so `rally room` reported no lead while the room-wide claim gate
    /// still reported one. Revert `breadth_violation` to the raw derivation and
    /// this fails: the withdrawn seat keeps conferring room-wide authority.
    #[test]
    fn a_retracted_lead_decision_no_longer_confers_room_wide_authority() {
        let seat = lead_decision("tool-lead", 1);
        let incoming = fact("claim-lead", "tool-lead", vec!["workspace:*"]);
        assert!(
            breadth_violation(&incoming, std::slice::from_ref(&seat)).is_none(),
            "baseline: the seated lead may hold a room-wide claim"
        );

        let withdrawn = retraction("r-1", &seat.event_id, 900);
        let facts = [seat, withdrawn];
        assert_eq!(
            projected_lead(&facts),
            None,
            "precondition: the projection reports no lead once the seat decision is withdrawn"
        );
        let refusal = breadth_violation(&incoming, &facts)
            .expect("a withdrawn seat confers no room-wide authority");
        assert!(
            refusal.contains("no lead"),
            "the refusal must report the room the projection actually shows; got: {refusal}"
        );
    }

    /// Resolution is FLAT — `retracted_ids` collects every target in one pass —
    /// so retracting the retraction does not restore the seat. Pinned because
    /// this function's entire correctness argument is "the same answer
    /// `rally room` gives", and that has to include the blunt cases.
    #[test]
    fn retraction_resolution_does_not_nest() {
        let seat = lead_decision("tool-lead", 1);
        let withdrawn = retraction("r-1", &seat.event_id, 900);
        let undo = retraction("r-2", "r-1", 901);
        assert_eq!(projected_lead(&[seat, withdrawn, undo]), None);
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
