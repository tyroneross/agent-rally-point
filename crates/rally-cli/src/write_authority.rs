// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The single place a fact is checked for the authority to make the change it
//! encodes.
//!
//! # Why this module exists
//!
//! Before it, rally had authorization logic in three places and a different
//! answer in each:
//!
//! * `command_release_by_path` (`lib.rs`) held the takeover rules, and
//!   `command_say` reached it ONLY when `--ref` was absent (RC-029).
//! * `append_state_transition_verified` (`store.rs`) gated two of the four
//!   kinds that close a claim, by hand-copied match arms (ARP-R-02).
//! * `set_lead` (`lib.rs`) gated nothing at all (ARP-R-01).
//!
//! Each of those was a correct rule on a path the attack did not take. The
//! lesson from three cycles of this defect class is not "add another gate" —
//! it is that a gate placed at a COMMAND is a gate on one spelling of an
//! action, and the ledger accepts the action, not the spelling. So authority is
//! asserted HERE, at the write boundary, on the fact.
//!
//! # Where this runs, and why that matters
//!
//! [`assert_write_authorized`] is called from `DirectRoomStore::append_fact`,
//! under the room mutation lock. That is the ONE function every durable write
//! passes through, on BOTH store modes:
//!
//! * Direct mode — the CLI opens the store in-process and calls it.
//! * Routed mode — `rallyd_core::run_op` handles `AppendFact`,
//!   `AppendFactVerified`, and `AppendStateTransitionVerified` by calling the
//!   very same `DirectRoomStore` methods (`rallyd_core.rs`, the `StoreOp` match).
//!
//! A gate in `lib.rs` would run only in the client process, so in routed mode a
//! hand-built request would reach the daemon ungated. A gate here is enforced by
//! whichever process owns the ledger. `crates/rally-cli/tests/write_authority_daemon_parity.rs`
//! asserts that equivalence rather than assuming it — the design audit's D1/D6
//! findings are exactly what happens when a wire boundary is assumed to preserve
//! a property nobody tested.
//!
//! # ADMISSION-TIME authority (design audit D9)
//!
//! rally evaluated authority at two different times. A wildcard claim was
//! authorized once at append and then survived every later lead change; an
//! unscoped blocker was re-authorized against the CURRENT lead on every
//! `check`. That asymmetry is the mechanism behind the retroactive arm/disarm
//! reproduced against RC-038 — the same fact id flipped between
//! `unscoped-blocker`/allow and `room-freeze`/deny as the seat changed hands
//! underneath it.
//!
//! This module picks ADMISSION-TIME and applies it everywhere: a fact's
//! authority is decided against the room as of the fact's own `seq`, once, and
//! does not change afterwards. See [`claim_authority::lead_as_of`]. Projection
//! then reports that decision rather than re-deriving it.
//!
//! # WHAT THIS MODULE CANNOT DO — read before trusting any gate here
//!
//! Every check below reads `fact.tool`, and `fact.tool` is SELF-ASSERTED. It
//! arrives from `--tool` on the command line and is bound to nothing: no
//! session lease, no credential, no registry. `ensure_presence` CREATES presence
//! for whatever name it is handed rather than checking standing.
//!
//! So the honest description of every gate here is:
//!
//! > It closes the path where an agent acts destructively **under its own
//! > name**. It does not stop an agent willing to pass `--tool <incumbent>`.
//!
//! That is a real reduction — the act becomes a forgery rather than a permitted
//! operation, it is attributable as one, and an honest agent can no longer do
//! it by accident or by following a refusal message's advice. It is NOT an
//! authorization boundary, and this codebase has already had to retract one
//! claim of exactly that shape (`docs/security/TRUST-MODEL.md`, the "raises the
//! bar from any writer to the first writer" retraction). Do not restate it.
//!
//! Closing the impersonation path requires a minted session lease that `tool`
//! and `role` are DERIVED from rather than asserted alongside — see the
//! authority-model entry in `docs/ROOT-CAUSE-REGISTER.md`. That choice is owed
//! and unmade. `crates/rally-cli/tests/lead_seat_authz.rs` contains a live
//! impersonation test that documents the residual by asserting what actually
//! happens, so the gap stays visible instead of being remembered.

use crate::claim_authority;
use crate::error::{RallyError, Result};
use crate::hooks_config::CoordinationConfig;
use crate::store::{Fact, FactKind, RoomSnapshot};

/// Evidence marker recording a deliberate, operator-acknowledged lead seizure.
/// See [`assert_lead_transfer_authorized`] for what this does and does not mean.
pub(crate) const LEAD_FORCE_MARKER: &str = "lead-seizure-acknowledged";

/// Does this fact need the full authority check — the one that costs a facts
/// load and a projection?
///
/// Kept as one predicate so the cost is paid by the writes that change who may
/// do what, and by nothing else. Ordinary traffic (presence, read, artifact,
/// handoff, risk) pays one `matches!`, one string-prefix test, and the field
/// bounds.
///
/// The retraction arm (R1) is deliberately NOT gated on `fact.kind`. A
/// retraction's wire kind is whatever the writing binary happened to accept —
/// `artifact` from this CLI, and an unknown-kind remap from a peer that types
/// build-loop's `retract` kind directly — so keying the check off any of those
/// spellings would rebuild the exact defect this module exists to close. It is
/// keyed off the ACT instead, and [`crate::retraction::target_of`] short-
/// circuits on the `retract:` subject prefix, so an ordinary fact pays one
/// string comparison for it.
pub(crate) fn needs_authority_check(fact: &Fact) -> bool {
    claim_authority::closes_active_claim(&fact.kind)
        || claim_authority::is_lead_decision(fact)
        || crate::retraction::target_of(fact).is_some()
}

/// Assert `fact` may be written, given the room as it stands immediately before
/// it (`snapshot`, projected from `facts_before`).
///
/// `facts_before` is every fact already durable — this write is not in it. That
/// slice IS the admission-time view (D9): resolving the incumbent over it asks
/// "who held the seat at the moment this fact was offered", which is the
/// question authority should answer. `fact.seq` is not yet assigned at this
/// point in `append_fact`, so it must not be used as the cutoff;
/// [`claim_authority::lead_as_of`] exists for the PROJECTION side, where the
/// slice does contain later facts and the cutoff has to be explicit.
///
/// Returns `Ok(())` when the write is authorized, or a `RallyError::Usage`
/// naming the actor, the incumbent, and the way forward.
pub(crate) fn assert_write_authorized(
    fact: &Fact,
    facts_before: &[Fact],
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    assert_field_bounds(fact)?;
    assert_claim_close_authorized(fact, snapshot, coord)?;
    assert_release_sweep_authorized(fact, snapshot, coord)?;
    assert_retraction_authorized(fact, snapshot, coord)?;
    assert_lead_transfer_authorized(fact, facts_before, snapshot, coord)?;
    Ok(())
}

/// R5. Who may write a release whose SCOPE sweeps somebody else's live claim.
///
/// `claim_authority::later_release_overlaps_claim_scope` closes EVERY active
/// claim whose scope overlaps the release's own free-text `fact.scope`. The
/// close gate authorized only the claim named by `ref_id` and never read
/// `fact.scope`, so authorization and effect were keyed off two different
/// fields — and naming your own claim in `--ref` satisfied a gate that had
/// nothing to do with the claims actually being closed:
///
/// ```text
/// rally say release --tool rogue --ref <rogue's own claim> --scope file:<victim's path>
/// ```
///
/// Arm 1 passed on the rogue's own claim; the victim's claim was swept.
///
/// Every claim the sweep would take is now checked under the same three-arm
/// policy, so the rogue's own claim passes on arm 1 and the victim's does not
/// pass at all unless its owner is genuinely stale. `command_release_by_path`'s
/// legitimate multi-claim atomic release is unaffected: it already applies the
/// stale-owner bar before writing, so each swept claim clears arm 1 or arm 2.
///
/// The swept set is decided by CALLING the projection's predicate, not by
/// restating it. ARP-R-02 is what restating costs. This rides the snapshot the
/// gate already loaded for closing kinds — no extra read.
fn assert_release_sweep_authorized(
    fact: &Fact,
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    if fact.kind != FactKind::Release || fact.scope.is_empty() {
        return Ok(());
    }
    for claim in &snapshot.active_claims {
        if claim_authority::later_release_overlaps_claim_scope(fact, claim) {
            authorize_claim_removal(fact, claim, "release", snapshot, coord)?;
        }
    }
    Ok(())
}

/// ARP-R-04, durable half. Bound the free-text fields at the door.
///
/// The renderer-side escaping fixes are necessary and not sufficient: every
/// renderer has to get it right independently, and the retrospective renderer
/// did not. Bounding here removes the volume half of the whole defect family at
/// one point. Thresholds are measured — see `rally_protocol::ledger`.
pub(crate) fn assert_field_bounds(fact: &Fact) -> Result<()> {
    assert_identity_fields_are_single_line(fact)?;
    rally_protocol::ledger::validate_fact_text_bounds(&rally_protocol::ledger::FactTextFields {
        subject: &fact.subject,
        summary: fact.summary.as_deref(),
        evidence: &fact.evidence,
        scope: &fact.scope,
        uri: fact.uri.as_deref(),
    })
    .map_err(|err| RallyError::Usage(err.to_string()))
}

/// An identity field may not contain a line break or a control character.
///
/// Found while hardening the retrospective renderer (ARP-R-04): `rally say
/// --tool $'atk\n## FORGED'` was ACCEPTED at the write boundary, and every
/// surface that renders a tool id inherited the exposure. The renderer fix
/// neutralizes it on the way out, which is necessary — but it made the render
/// side the SOLE barrier, and "every renderer independently gets this right" is
/// the assumption that produced ARP-R-04 in the first place.
///
/// Deliberately NARROWER than `rally_protocol::ledger::validate_agent_id`,
/// which additionally enforces an `[A-Za-z0-9:_-]` allowlist. That allowlist is
/// right for a filename stem and would reject ids already present in live
/// ledgers (anything carrying a `.` or `/`), and this gate runs on every append
/// including replay of existing rooms. Rejecting a line break costs nothing any
/// real id needs and removes the class that forges structure; tightening to the
/// full allowlist is a separate, migration-shaped decision.
fn assert_identity_fields_are_single_line(fact: &Fact) -> Result<()> {
    for (field, value) in [
        ("tool", fact.tool.as_deref()),
        ("target", fact.target.as_deref()),
        ("role", fact.role.as_deref()),
    ] {
        let Some(value) = value else { continue };
        if let Some(bad) = value.chars().find(|c| c.is_control()) {
            return Err(RallyError::Usage(format!(
                "{field} contains the control character {bad:?}. An identity field is \
                 rendered into agent context and into git-tracked documents, where a line \
                 break lets it forge a heading, a list item, or a new speaker. Identities \
                 are single-line by contract (ARP-R-04)."
            )));
        }
    }
    Ok(())
}

/// ARP-R-02. Who may close somebody else's live claim.
///
/// The gate this replaces lived in `append_state_transition_verified` and was
/// reached from exactly two call sites — the `Release` arm and the `Resolve`
/// arm. But `claim_authority::closes_active_claim` closes a claim on FOUR
/// kinds. `Receipt` and `ClaimExpired` reached the ledger with no ownership
/// check at all, and `rally say receipt --tool rogue --ref <claim-id>` took any
/// live claim in the room.
///
/// This version is keyed off `closes_active_claim` itself, so the set of kinds
/// that close a claim and the set that must be authorized to close one are the
/// same list by construction. A fifth closing kind cannot add a fifth bypass.
///
/// The policy itself lives in [`authorize_claim_removal`], which retraction
/// (R1) also calls. A ref naming no active claim is not this gate's business
/// and passes through.
fn assert_claim_close_authorized(
    fact: &Fact,
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    if !claim_authority::closes_active_claim(&fact.kind) {
        return Ok(());
    }
    let Some(ref_id) = fact.ref_id.as_deref() else {
        return Ok(());
    };
    let Some(claim) = snapshot.active_claims.iter().find(|c| c.event_id == ref_id) else {
        return Ok(());
    };
    authorize_claim_removal(fact, claim, fact.kind.as_str(), snapshot, coord)
}

/// R1. Who may retract a fact that WITHDRAWS somebody else's live claim.
///
/// A retraction drops its target from every projection bucket
/// (`store::snapshot_from_facts_with_policy_at`). Point one at an active claim
/// and the claim is gone from `rally room`, from `check before-write`, and from
/// every peer's session-start context — the same observable effect a `release`
/// has, reached by a path the close gate never saw because a retraction is not
/// one of the four closing kinds.
///
/// Same three-arm policy as [`assert_claim_close_authorized`], reused rather
/// than restated: one authority policy, two entry points. Arm 3 (typed reaper
/// lease expiry) requires `FactKind::ClaimExpired` and so is structurally
/// unreachable for a retraction, which is correct — a retraction is a
/// correction, not reaper cleanup.
///
/// Retraction of anything that is NOT an active claim stays ungated. That is
/// the deliberate ruling, not an oversight: the whole point of retraction is
/// that an honest mistake stays fixable without asking permission, and a wrong
/// artifact, decision, or risk harms nobody's write safety by being withdrawn.
fn assert_retraction_authorized(
    fact: &Fact,
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    let Some(target) = crate::retraction::target_of(fact) else {
        return Ok(());
    };
    let Some(claim) = snapshot.active_claims.iter().find(|c| c.event_id == target) else {
        return Ok(());
    };
    authorize_claim_removal(fact, claim, "retract", snapshot, coord)
}

/// The three-arm policy itself, shared by every path that removes an active
/// claim from the room.
///
/// Three ways past, in order of how often they are the real answer:
///
/// 1. **Self-close.** The actor owns the claim. Always allowed, no time bar —
///    releasing your own work is the normal path and most closes are this.
/// 2. **Stale-owner takeover.** The owner has been silent past the size-scaled
///    reclaim timeout (`claim_reclaim_eligible`, fail-closed: an owner whose
///    `last_seen_ts` is missing or unparseable is never reclaimable). This is
///    the same authority `command_release_by_path` already applied, reused
///    rather than re-implemented, so there is one policy and not two.
/// 3. **Typed expired-lease cleanup.** Lease expiry is a cleanup signal, not
///    peer authority. Only a `ClaimExpired` transition shaped by the reaper may
///    use it. The transition must carry the claim ref, owner/session, reason,
///    and external-observation verdict that the store revalidates under the
///    mutation lock. An ordinary `Release`, `Resolve`, `Receipt`, or retraction
///    never gains authority solely because time passed.
///
/// `action` names the act in the refusal, because the caller knows the spelling
/// and this function should not have to. Keeping the policy in ONE body is the
/// whole point: ARP-R-02 happened because the closing-kind list and the
/// authorized-kind list were maintained separately and drifted, and R1 happened
/// because a second removal path never reached the list at all.
fn authorize_claim_removal(
    fact: &Fact,
    claim: &Fact,
    action: &str,
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    let claim_id = claim.event_id.as_str();
    // 1. Self-close. Session identity is authoritative when present. A modern
    // stamped caller retains one-way compatibility with a historical
    // sessionless claim only when both name the same present, nonblank tool.
    if claim_authority::claim_owner_matches_caller(
        claim.tool.as_deref(),
        claim.from_session_id.as_deref(),
        fact.tool.as_deref(),
        fact.from_session_id.as_deref(),
    ) {
        return Ok(());
    }
    let typed_lease_expiry =
        is_typed_reaper_lease_expiry(fact, claim) && claim_lease_expired(claim);
    // 2. Stale-owner takeover.
    let (takeover_eligible, size) = snapshot.claim_reclaim_eligible(claim, coord);
    // A sibling process sharing the same tool label is not an external peer.
    // Never let that label alias another session's ownership, even after the
    // stale/lease windows; an explicit reaper fact remains a different actor.
    if !typed_lease_expiry
        && claim.from_session_id.is_some()
        && claim_authority::same_nonblank_tool(claim.tool.as_deref(), fact.tool.as_deref())
    {
        let actor = fact.tool.as_deref().unwrap_or("<unknown>");
        return Err(RallyError::Usage(format!(
            "{action} failed: claim {claim_id} belongs to another {actor} session; shared tool labels do not confer claim authority. Ask the owning session to release it."
        )));
    }
    if takeover_eligible {
        return Ok(());
    }
    // 3. The reaper/operator path presented a typed, under-lock-verifiable
    // ClaimExpired transition for a lease that has actually run out.
    if typed_lease_expiry {
        return Ok(());
    }

    let owner = claim.tool.as_deref().unwrap_or("<unknown>");
    let actor = fact.tool.as_deref().unwrap_or("<unknown>");
    let timeout_minutes = match size {
        crate::decay::WorkSize::Small => coord.reclaim_small_minutes,
        crate::decay::WorkSize::Large => coord.reclaim_large_minutes,
    };
    Err(RallyError::Usage(format!(
        "{action} failed: claim {claim_id} is owned by {owner} and {actor} is not the owner. \
         An ordinary non-owner close is allowed only after the owner has been silent for \
         {timeout_minutes} minutes; lease expiry is reserved for typed ClaimExpired cleanup. \
         Ask {owner} to release it, wait out the reclaim window, or run the reaper/operator \
         cleanup path."
    )))
}

fn reaper_marker<'a>(fact: &'a Fact, key: &str) -> Option<&'a str> {
    let prefix = format!("reaper:{key}=");
    fact.evidence
        .iter()
        .find_map(|item| item.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Does this fact carry the typed evidence that routes lease expiry through
/// the store's under-lock reaper checks?
fn is_typed_reaper_lease_expiry(fact: &Fact, claim: &Fact) -> bool {
    if fact.kind != FactKind::ClaimExpired || fact.tool.as_deref() != Some("rally") {
        return false;
    }
    let Some(ref_id) = fact.ref_id.as_deref() else {
        return false;
    };
    let expected_owner = claim.tool.as_deref().unwrap_or("unknown");
    let expected_session = claim.from_session_id.as_deref().unwrap_or("legacy");
    reaper_marker(fact, "ref_id") == Some(ref_id)
        && matches!(
            reaper_marker(fact, "reason"),
            Some("lease-expired" | "owner-stale+lease-expired")
        )
        && reaper_marker(fact, "owner") == Some(expected_owner)
        && reaper_marker(fact, "owner_session") == Some(expected_session)
        && matches!(reaper_marker(fact, "observed"), Some("stale" | "unknown"))
}

/// Has this claim's own `lease_expires_at:` marker passed?
///
/// Fail-CLOSED on an absent or unparseable marker: "I could not read the lease"
/// must not mean "the lease is over". A claim with no lease marker is never
/// closable by this arm — self-close and stale-owner takeover still apply.
fn claim_lease_expired(claim: &Fact) -> bool {
    const PREFIX: &str = "lease_expires_at:";
    claim
        .evidence
        .iter()
        .find_map(|item| item.strip_prefix(PREFIX))
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw.trim()).ok())
        .is_some_and(|expires| expires <= chrono::Utc::now())
}

/// ARP-R-01. Who may take, hand off, or vacate the lead seat.
///
/// Before this gate, `set_lead`'s only precondition was
/// `ensure_presence(&room, &t.tool)` — which CREATES presence rather than
/// checking standing. No comparison against the incumbent, none against `--to`.
/// Live: `rally lead assign --tool rogue --to rogue` succeeded against a live
/// incumbent AND against a `--user-designated` one, and
/// `rally lead relinquish --tool rogue` vacated the seat to null. No
/// impersonation was needed; rogue operated under its own honest name.
///
/// That single command was the room's authority root. Every control added in
/// the prior cycle hangs off the lead seat — RC-037's room-wide claim gate and
/// RC-038's room-freeze gate both read "is this agent the lead" — so one
/// unauthenticated command re-opened both: take the seat, claim `workspace:*`,
/// and every other agent is locked out of claiming, room-wide.
///
/// Four ways past:
///
/// 1. **Leaderless room.** Anyone may take an empty seat. This is first-join,
///    and it is a deliberate, documented weakness rather than an oversight —
///    see `docs/security/TRUST-MODEL.md`. It is not made worse here.
/// 2. **Genuine handoff.** The ACTOR is the incumbent. Handing off your own
///    seat, or vacating it, is the normal path.
/// 3. **Stale incumbent.** The incumbent has been silent past the large-work
///    reclaim window. Without this, a crashed lead freezes the seat forever and
///    the reaper — which legitimately relinquishes a stale lead, stamping
///    `reaper:stale-lead=` — could not do its job. The staleness is measured
///    from the squad projection, exactly as claim takeover is, so there is one
///    liveness policy.
/// 4. **Acknowledged seizure.** The fact carries the [`LEAD_FORCE_MARKER`]
///    evidence, written by `rally lead assign --force`.
///
/// Arm 4 is a SPEED BUMP, not a credential, and calling it one would repeat the
/// mistake this cycle is cleaning up. rally's trust boundary is the UID: there
/// is no out-of-band channel on this machine, so anything that can run
/// `rally lead assign` can also pass `--force`. What arm 4 buys is that the
/// seizure is deliberate, is recorded in the ledger as a seizure, and names the
/// incumbent it displaced — an auditable act instead of an indistinguishable
/// one. That is the most an unauthenticated identity model can offer, and the
/// module header says why.
fn assert_lead_transfer_authorized(
    fact: &Fact,
    facts_before: &[Fact],
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    if !claim_authority::is_lead_decision(fact) {
        return Ok(());
    }
    // ADMISSION-TIME (D9). `facts_before` excludes this write, so the incumbent
    // it yields is the one holding the seat at the moment this fact was
    // offered. See `assert_write_authorized` for why the cutoff is the slice
    // and not `fact.seq`.
    let incumbent = claim_authority::lead_from_facts(facts_before);

    // 1. Leaderless.
    let Some(incumbent) = incumbent else {
        return Ok(());
    };
    let actor = fact.tool.as_deref().unwrap_or("<unknown>");
    // 2. Genuine handoff / self-relinquish.
    if actor == incumbent {
        return Ok(());
    }
    // 3. Stale incumbent.
    if lead_is_stale(snapshot, &incumbent, coord) {
        return Ok(());
    }
    // 4. Acknowledged seizure.
    if fact
        .evidence
        .iter()
        .any(|item| item.trim() == LEAD_FORCE_MARKER)
    {
        return Ok(());
    }

    let beneficiary = claim_authority::lead_beneficiary(fact);
    let beneficiary = beneficiary.as_deref().unwrap_or("<unknown>");
    let verb = if fact.subject == claim_authority::LEAD_RELINQUISHED_SUBJECT {
        format!("cannot vacate the seat {incumbent} holds")
    } else {
        format!("cannot transfer the seat from {incumbent} to {beneficiary}")
    };
    Err(RallyError::Usage(format!(
        "lead transfer refused: {incumbent} holds the lead seat and {actor} {verb}. \
         The seat moves when its holder hands it off ({incumbent} runs the command), or when \
         its holder has been silent past the reclaim window. If you are deliberately taking it \
         from a live lead, say so on the record with `--force`, which writes the seizure and the \
         displaced incumbent into the ledger."
    )))
}

/// Has the incumbent lead been silent past the large-work reclaim window?
///
/// Large-work window on purpose: the seat is the coarsest thing in the room, so
/// it gets the most patient timeout. Fail-CLOSED on a missing or unparseable
/// `last_seen_ts` — an incumbent whose liveness cannot be read is treated as
/// live, so a bad timestamp never hands the seat away.
fn lead_is_stale(snapshot: &RoomSnapshot, incumbent: &str, coord: &CoordinationConfig) -> bool {
    let timeout = coord.reclaim_large_minutes.saturating_mul(60);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    snapshot
        .squads
        .iter()
        .find(|sq| sq.tool == incumbent)
        .and_then(|sq| {
            chrono::DateTime::parse_from_rfc3339(&sq.last_seen_ts)
                .ok()
                .map(|dt| now_secs - dt.timestamp() > timeout)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Squad;
    use chrono::{Duration, SecondsFormat, Utc};

    fn iso_ago(secs: i64) -> String {
        (Utc::now() - Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    fn squad(tool: &str, silent_secs: i64) -> Squad {
        Squad {
            tool: tool.to_string(),
            last_seen_seq: 1,
            last_seen_ts: iso_ago(silent_secs),
            status: "idle".to_string(),
            acknowledged: false,
        }
    }

    /// One live single-file claim owned by `owner`, whose owner has been silent
    /// for `owner_silent_secs`. Single-file scope means the SMALL (30 min)
    /// reclaim window applies.
    fn room_with_claim(owner: &str, owner_silent_secs: i64) -> (Fact, RoomSnapshot) {
        let claim = Fact {
            event_id: "claim-1".to_string(),
            kind: FactKind::Claim,
            tool: Some(owner.to_string()),
            from_session_id: Some(format!("sess:{owner}")),
            scope: vec!["file:src/a.rs".to_string()],
            subject: "owns it".to_string(),
            created_at: iso_ago(0),
            ..Fact::default()
        };
        let snapshot = RoomSnapshot {
            active_claims: vec![claim.clone()],
            squads: vec![squad(owner, owner_silent_secs)],
            ..Default::default()
        };
        (claim, snapshot)
    }

    /// A retraction exactly as `rally retract` writes it: artifact kind, the
    /// anchored `retract: <id>` subject, `ref` naming the target.
    fn retraction_by(tool: &str, target: &str) -> Fact {
        Fact {
            event_id: "r-1".to_string(),
            kind: FactKind::Artifact,
            tool: Some(tool.to_string()),
            from_session_id: Some(format!("sess:{tool}")),
            subject: crate::retraction::subject_for(target),
            summary: Some(crate::retraction::summary_for(target, "withdrawn", None)),
            ref_id: Some(target.to_string()),
            status: Some("retraction".to_string()),
            created_at: iso_ago(0),
            ..Fact::default()
        }
    }

    fn refusal(fact: &Fact, snapshot: &RoomSnapshot) -> String {
        let coord = CoordinationConfig::default();
        match assert_write_authorized(fact, &[], snapshot, &coord) {
            Ok(()) => panic!("expected a refusal, the write was authorized"),
            Err(err) => err.to_string(),
        }
    }

    fn authorized(fact: &Fact, snapshot: &RoomSnapshot) {
        let coord = CoordinationConfig::default();
        assert_write_authorized(fact, &[], snapshot, &coord)
            .unwrap_or_else(|err| panic!("expected authorization, got refusal: {err}"));
    }

    /// R1, THE defect. A retraction drops its target from every projection, so
    /// pointing one at a live claim strips the claim exactly as a `release`
    /// does — and `needs_authority_check` never saw it, because a retraction is
    /// not one of the four closing kinds.
    #[test]
    fn non_owner_cannot_retract_a_live_claim() {
        // Owner seen 1 minute ago: nowhere near the 30-minute small window.
        let (claim, snapshot) = room_with_claim("victim:01", 60);
        let fact = retraction_by("codex:rogue", &claim.event_id);

        assert!(
            needs_authority_check(&fact),
            "a claim-targeting retraction must reach the authority gate at all"
        );
        let err = refusal(&fact, &snapshot);
        assert!(
            err.contains("retract failed"),
            "the refusal must name the act the caller attempted; got: {err}"
        );
        assert!(
            err.contains("victim:01") && err.contains("not the owner"),
            "the refusal must name the owner so the caller knows who to ask; got: {err}"
        );
    }

    /// Arm 1. Withdrawing your own claim is the normal path and stays free.
    #[test]
    fn the_owner_can_retract_its_own_claim() {
        let (claim, snapshot) = room_with_claim("victim:01", 60);
        authorized(&retraction_by("victim:01", &claim.event_id), &snapshot);
    }

    /// Arm 2. Same size-scaled silence window as `release`, reused rather than
    /// restated — a single-file claim opens at 30 minutes.
    #[test]
    fn a_stale_owners_claim_can_be_retracted_by_a_peer() {
        let (claim, fresh) = room_with_claim("victim:01", 29 * 60);
        let fact = retraction_by("codex:peer", &claim.event_id);
        assert!(
            refusal(&fact, &fresh).contains("not the owner"),
            "29 minutes of silence is inside the small window and must still refuse"
        );

        let (claim, stale) = room_with_claim("victim:01", 31 * 60);
        authorized(&retraction_by("codex:peer", &claim.event_id), &stale);
    }

    /// The judged ruling's invariant 3: retraction of a NON-claim fact stays
    /// ungated, so an honest mistake stays fixable without asking permission.
    /// This is the arm that keeps R1's fix from making the room brittle.
    #[test]
    fn retracting_a_non_claim_fact_stays_ungated() {
        let (_claim, snapshot) = room_with_claim("victim:01", 60);
        // Targets a fact that is not an active claim.
        let fact = retraction_by("codex:peer", "some-artifact-id");
        assert!(
            needs_authority_check(&fact),
            "the gate still LOOKS at every retraction"
        );
        authorized(&fact, &snapshot);
    }

    /// A sibling process wearing the owner's tool label is not the owner. Same
    /// rule the close path applies, reached through the retraction entry point.
    #[test]
    fn a_sibling_session_sharing_the_tool_label_cannot_retract_the_claim() {
        let (claim, snapshot) = room_with_claim("victim:01", 60);
        let mut fact = retraction_by("victim:01", &claim.event_id);
        fact.from_session_id = Some("sess:victim:01:OTHER".to_string());
        let err = refusal(&fact, &snapshot);
        assert!(
            err.contains("another victim:01 session"),
            "the refusal must say the label is not the identity; got: {err}"
        );
    }

    /// Two live claims on different paths: `victim:01` owns the victim path,
    /// `codex:rogue` owns its own. `owner_silent_secs` ages ONLY the victim.
    fn room_with_two_claims(owner_silent_secs: i64) -> RoomSnapshot {
        let mk = |id: &str, tool: &str, path: &str| Fact {
            event_id: id.to_string(),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            from_session_id: Some(format!("sess:{tool}")),
            scope: vec![format!("file:{path}")],
            subject: "owns it".to_string(),
            created_at: iso_ago(0),
            ..Fact::default()
        };
        RoomSnapshot {
            active_claims: vec![
                mk("claim-victim", "victim:01", "src/victim.rs"),
                mk("claim-rogue", "codex:rogue", "src/rogue.rs"),
            ],
            squads: vec![
                squad("victim:01", owner_silent_secs),
                squad("codex:rogue", 0),
            ],
            ..Default::default()
        }
    }

    /// A release naming the actor's OWN claim in `--ref` while carrying the
    /// VICTIM's path in `--scope`.
    fn sweeping_release(tool: &str, own_claim: &str, swept_path: &str) -> Fact {
        Fact {
            event_id: "rel-1".to_string(),
            kind: FactKind::Release,
            tool: Some(tool.to_string()),
            from_session_id: Some(format!("sess:{tool}")),
            ref_id: Some(own_claim.to_string()),
            scope: vec![format!("file:{swept_path}")],
            subject: "done".to_string(),
            created_at: iso_ago(0),
            ..Fact::default()
        }
    }

    /// R5, THE defect. `later_release_overlaps_claim_scope` closes every active
    /// claim whose scope overlaps the release's free-text `--scope`, while the
    /// close gate authorized only the claim named by `--ref` and never read
    /// `fact.scope`. Authorization and effect were keyed off two different
    /// fields, so naming your own claim satisfied a gate that had nothing to do
    /// with the claim actually being closed.
    #[test]
    fn a_release_cannot_sweep_a_live_peers_claim_by_scope() {
        // Victim seen 1 minute ago — nowhere near the 30-minute small window.
        let snapshot = room_with_two_claims(60);
        let fact = sweeping_release("codex:rogue", "claim-rogue", "src/victim.rs");

        assert!(
            claim_authority::later_release_overlaps_claim_scope(&fact, &snapshot.active_claims[0]),
            "precondition: this release DOES sweep the victim's claim"
        );
        let err = refusal(&fact, &snapshot);
        assert!(
            err.contains("release failed") && err.contains("victim:01"),
            "the refusal must name the claim the SWEEP would take, not the ref; got: {err}"
        );
    }

    /// The takeover arm still works through the sweep path — the size-scaled
    /// silence window is the same one `release --ref` uses, because it is the
    /// same policy body.
    #[test]
    fn a_release_may_sweep_a_stale_owners_claim() {
        let fact = sweeping_release("codex:rogue", "claim-rogue", "src/victim.rs");
        assert!(
            refusal(&fact, &room_with_two_claims(29 * 60)).contains("not the owner"),
            "29 minutes is inside the small window and must still refuse"
        );
        authorized(&fact, &room_with_two_claims(31 * 60));
    }

    /// Releasing your own claim by scope is the ordinary path and stays free.
    #[test]
    fn a_release_may_sweep_its_own_claim_by_scope() {
        let snapshot = room_with_two_claims(60);
        authorized(
            &sweeping_release("codex:rogue", "claim-rogue", "src/rogue.rs"),
            &snapshot,
        );
    }

    /// A release carrying no scope sweeps nothing, so the sweep arm must not
    /// invent a refusal for it — `release --ref <own-claim>` with no `--scope`
    /// is the most common release there is.
    #[test]
    fn a_scopeless_release_is_untouched_by_the_sweep_arm() {
        let snapshot = room_with_two_claims(60);
        let mut fact = sweeping_release("codex:rogue", "claim-rogue", "src/victim.rs");
        fact.scope.clear();
        authorized(&fact, &snapshot);
    }

    /// R2 + R1 together. The `retracts=` summary token is no longer a detection
    /// carrier, so it cannot be used to smuggle a claim strip past the gate
    /// under an innocuous subject — the act has exactly two spellings and the
    /// gate covers both.
    #[test]
    fn a_summary_token_cannot_smuggle_a_claim_strip_past_the_gate() {
        let (claim, snapshot) = room_with_claim("victim:01", 60);
        let smuggled = Fact {
            event_id: "s-1".to_string(),
            kind: FactKind::Artifact,
            tool: Some("codex:rogue".to_string()),
            subject: "just a note".to_string(),
            summary: Some(format!("nothing to see [retracts={}]", claim.event_id)),
            created_at: iso_ago(0),
            ..Fact::default()
        };
        assert!(
            crate::retraction::target_of(&smuggled).is_none(),
            "the token carrier is gone, so this withdraws nothing"
        );
        // It reaches the ledger — and, withdrawing nothing, leaves the claim up.
        authorized(&smuggled, &snapshot);
        assert!(
            !crate::retraction::retracted_ids(std::slice::from_ref(&smuggled))
                .contains(&claim.event_id),
            "the claim must remain live"
        );
    }
}
