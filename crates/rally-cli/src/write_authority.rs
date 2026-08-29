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
//! and `role` are DERIVED from rather than asserted alongside. That choice is
//! owed and unmade. `crates/rally-cli/tests/lead_seat_authz.rs` contains a live
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
///
/// That same call selects for BOTH retraction arms — the claim one (R1) and the
/// lead-seat one (RC-071a) — so gating the seat's removal added no new
/// admission cost and no new spelling to remember.
pub(crate) fn needs_authority_check(fact: &Fact) -> bool {
    claim_authority::closes_active_claim(&fact.kind)
        || crate::store::is_session_close_attempt(fact)
        || claim_authority::is_lead_decision(fact)
        || crate::retraction::target_of(fact).is_some()
}

pub(crate) fn needs_session_lifecycle_check(fact: &Fact) -> bool {
    crate::store::is_session_close_attempt(fact)
        || (fact.tool.as_deref().is_some_and(|tool| !tool.is_empty())
            && fact
                .from_session_id
                .as_deref()
                .is_some_and(|session| !session.is_empty()))
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
    assert_session_lifecycle_authorized(fact, facts_before)?;
    assert_claim_close_authorized(fact, snapshot, coord)?;
    assert_release_sweep_authorized(fact, snapshot, coord)?;
    assert_retraction_authorized(fact, snapshot, coord)?;
    assert_lead_retraction_authorized(fact, facts_before, snapshot, coord)?;
    assert_lead_transfer_authorized(fact, facts_before, snapshot, coord)?;
    Ok(())
}

fn assert_session_lifecycle_authorized(fact: &Fact, facts_before: &[Fact]) -> Result<()> {
    if !needs_session_lifecycle_check(fact) {
        return Ok(());
    }
    let Some(tool) = fact.tool.as_deref() else {
        return Ok(());
    };
    let Some(session_id) = fact.from_session_id.as_deref() else {
        return Ok(());
    };
    let exact = |candidate: &&Fact| {
        candidate.tool.as_deref() == Some(tool)
            && candidate.from_session_id.as_deref() == Some(session_id)
    };
    let already_closed = facts_before
        .iter()
        .filter(exact)
        .any(crate::store::is_session_close_fact);
    if already_closed {
        return Err(RallyError::Usage(format!(
            "session lease {session_id} is closed for {tool}; start a new parent lease"
        )));
    }

    let latest_close_hash = facts_before
        .iter()
        .filter(exact)
        .filter(|candidate| {
            candidate.kind == FactKind::Presence
                && candidate
                    .evidence
                    .iter()
                    .any(|item| item == "protocol:session_state=active")
        })
        .filter_map(|candidate| {
            candidate.evidence.iter().find_map(|item| {
                item.strip_prefix(crate::session_identity::SESSION_CLOSE_TOKEN_HASH_PREFIX)
            })
        })
        .next_back();

    if crate::store::is_session_close_attempt(fact) {
        let expected = latest_close_hash.ok_or_else(|| {
            RallyError::Usage(format!(
                "session close refused: {tool} {session_id} has no active close-token lease"
            ))
        })?;
        let supplied = fact
            .evidence
            .iter()
            .find_map(|item| {
                item.strip_prefix(crate::session_identity::SESSION_CLOSE_TOKEN_REVEAL_PREFIX)
            })
            .ok_or_else(|| {
                RallyError::Usage(
                    "session close refused: missing one-time close-token proof".to_string(),
                )
            })?;
        if crate::session_identity::session_close_token_hash(supplied) != expected {
            return Err(RallyError::Usage(
                "session close refused: close-token proof does not match the active lease"
                    .to_string(),
            ));
        }
    } else if fact.kind == FactKind::Presence {
        let proposed = fact.evidence.iter().find_map(|item| {
            item.strip_prefix(crate::session_identity::SESSION_CLOSE_TOKEN_HASH_PREFIX)
        });
        if let Some(expected) = latest_close_hash
            && proposed != Some(expected)
        {
            return Err(RallyError::Usage(
                "session ensure refused: an existing lease cannot rotate its close token"
                    .to_string(),
            ));
        }
    }
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
/// Retraction of anything that is neither an active claim nor the seated lead
/// decision stays ungated. That is the deliberate ruling, not an oversight: the
/// whole point of retraction is that an honest mistake stays fixable without
/// asking permission, and a wrong artifact, ordinary decision, or risk harms
/// nobody's write safety by being withdrawn. The lead seat is the one non-claim
/// exception, and it has its own arm — see
/// [`assert_lead_retraction_authorized`] for why the line is drawn at
/// "authority-carrying facts are gated, prose is free" rather than at "claims".
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
    // This arm used to require `tool == "rally"`, which made the reaper's
    // authority to close an expired lease inseparable from its inability to say
    // who it was. Attributing the reaper would therefore have silently REVOKED
    // that authority — `rally enter`'s auto-reap fails closed here with "X is
    // not the owner" the moment the fact names X.
    //
    // The authority now rides the `role: "system"` marker, which is what the
    // magic tool name was really standing in for. `is_system_authored` still
    // accepts the legacy `"rally"` author, so historical facts and
    // `SystemActor::invoking_process` are unaffected.
    //
    // This is NOT a widening. The substantive gate is, and always was, the
    // typed evidence below — ref, reason, owner, owner session, and observed
    // verdict, every one of which the store revalidates under the mutation
    // lock against the live claim. The label alone never authorized anything.
    // `command_say` additionally refuses a caller-supplied `--role system`, so
    // the marker is mintable only by `SystemActor` — strictly narrower than the
    // `--tool rally` spelling it replaces, which any caller could pass.
    if fact.kind != FactKind::ClaimExpired || !crate::store::is_system_authored(fact) {
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
    //
    // RC-071a: `projected_lead`, not the raw derivation. A lead decision the
    // incumbent has legitimately retracted leaves the room leaderless in every
    // projection, and a gate that still saw an incumbent there would refuse the
    // next honest `lead assign` — wedging the seat shut in a room that reports
    // it empty.
    let incumbent = claim_authority::projected_lead(facts_before);

    // 1. Leaderless.
    let Some(incumbent) = incumbent else {
        return Ok(());
    };
    let actor = fact.tool.as_deref().unwrap_or("<unknown>");
    // Arms 2-4.
    if lead_seat_change_allowed(fact, &incumbent, snapshot, coord) {
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

/// RC-071a. Who may retract the decision the LEAD SEAT rests on.
///
/// The judged ruling behind R1 scoped ungated retraction to "non-claim facts",
/// and the lead seat fell through that scoping: it is a non-claim fact that
/// CARRIES AUTHORITY. Every control in this room hangs off the seat — RC-037's
/// room-wide claim gate and RC-038's room-freeze both read "is this agent the
/// lead", and [`assert_lead_transfer_authorized`] gates `lead handoff`,
/// `lead assign`, and `lead relinquish` for exactly that reason. Retraction
/// reached the same effect by the path that gate never saw. That is the sixth
/// appearance of one defect (RC-029, ARP-R-01, ARP-R-02, R1, RC-071, this): a
/// correct rule guarding one SPELLING while the ledger accepts the ACT.
///
/// The operator's ruling, and the rule this arm encodes:
///
/// > **Authority-carrying facts are gated. Prose is free.**
///
/// So the boundary is no longer "is the target a claim" but "does withdrawing
/// the target move authority". Retracting an artifact, a risk, a lesson, or an
/// ordinary decision stays open to anyone — an honest mistake has to stay
/// fixable without asking permission.
///
/// # The oracle is the projection, run twice — not a list of spellings
///
/// This arm does not ask "is the target a lead decision". It asks the room:
/// derive the seat from `facts_before`, derive it again with the target
/// withdrawn, and gate only when the two answers differ. Every way a SINGLE
/// retraction can move the seat is covered by construction, including the ones
/// nobody enumerated:
///
/// * withdrawing the seated `role:lead` decision (the seat empties, or falls
///   back to an earlier lead — either way it moves);
/// * withdrawing a `role:lead:relinquished` that had reopened the seat, which
///   REVIVES the lead it vacated (ungated in practice, because a reopened seat
///   has no incumbent to authorize against — see the arms below).
///
/// Keying off `is_lead_decision` alone would answer "yes, gate it" for a
/// SUPERSEDED lead decision that moves nothing, and would have to grow a new
/// arm for every future spelling. Listing spellings is the defect this class
/// keeps re-teaching; asking the projection is the fix.
///
/// **The word SINGLE is load-bearing.** A SEQUENCE that first moves the seat's
/// authorization INPUT is not covered: arm 3 reads a liveness projection that
/// ungated retractions can regress. The residual is named here rather than
/// implied absent.
///
/// # Arms
///
/// Same policy as every other seat change, through
/// [`lead_seat_change_allowed`] — one body, two entry points, for the reason
/// [`authorize_claim_removal`] is one body. A retraction that leaves the seat
/// where it is never reaches the arms at all, and a LEADERLESS room is ungated:
/// arm 1 of the transfer gate already lets ANY agent seat ANY tool in an empty
/// room (`rally lead assign --tool rogue --to <former lead>` succeeds today —
/// measured, not assumed), so reviving a former lead by retraction grants no
/// capability that is not already one typed command away. That equivalence is
/// about CAPABILITY only: installing an absent lead in an empty room is a
/// denial vector, and it is arm 1's, documented in
/// `docs/security/TRUST-MODEL.md` as the first-join weakness.
///
/// Arm 4 (`--force`) is reachable here even though `rally retract` never writes
/// the marker, so reaching it takes a hand-built fact. Keeping it costs one
/// policy body instead of two, and an actor who can hand-build a marker can
/// equally run `lead assign --force`, so it widens no capability. It is NOT
/// equally auditable, and the honest statement of the difference is: `set_lead`
/// records `assigned:`, `from:`, `displaced:` and a seizure summary on a
/// `decision`, while a marker-bearing retraction records a withdrawal on an
/// `artifact` and names no displaced incumbent. A seizure should be typed.
/// RC-071b carries the owed decision on whether this arm should require a
/// validated `displaced:` entry.
///
/// The claim-close policy's arm 3 (typed `ClaimExpired` lease cleanup) has no
/// analogue here, exactly as it has none for claim retraction: the seat carries
/// no lease, and its liveness bar is [`lead_is_stale`].
fn assert_lead_retraction_authorized(
    fact: &Fact,
    facts_before: &[Fact],
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> Result<()> {
    let Some(target) = crate::retraction::target_of(fact) else {
        return Ok(());
    };
    // ADMISSION-TIME (D9), same slice and same reasoning as the transfer arm.
    let Some(incumbent) = claim_authority::projected_lead(facts_before) else {
        return Ok(());
    };
    let after = claim_authority::projected_lead_with_retraction(facts_before, Some(&target));
    if after.as_deref() == Some(incumbent.as_str()) {
        return Ok(());
    }
    if lead_seat_change_allowed(fact, &incumbent, snapshot, coord) {
        return Ok(());
    }

    let actor = fact.tool.as_deref().unwrap_or("<unknown>");
    let outcome = match after.as_deref() {
        Some(next) => format!("hands the seat to {next}"),
        None => "vacates the seat".to_string(),
    };
    Err(RallyError::Usage(format!(
        "retract failed: {incumbent} holds the lead seat by {target}, and withdrawing it \
         {outcome}. {actor} is not {incumbent}, so this is a seat change and follows the same \
         rule as `rally lead handoff`: the holder withdraws it, or the holder has been silent \
         past the reclaim window. Retracting a fact that carries no authority is still open to \
         anyone. If you are deliberately taking the seat from a live lead, use \
         `rally lead assign --force`, which records the seizure and the incumbent it displaced."
    )))
}

/// The seat-change policy, shared by every path that moves the lead seat.
///
/// Arms, in the order they are usually the real answer:
///
/// 2. **Genuine handoff.** The ACTOR is the incumbent. Handing off your own
///    seat, vacating it, or withdrawing the decision you hold it by.
/// 3. **Stale incumbent.** The incumbent has been silent past the large-work
///    reclaim window, measured from the squad projection exactly as claim
///    takeover is, so there is one liveness policy.
/// 4. **Acknowledged seizure.** The fact carries [`LEAD_FORCE_MARKER`].
///
/// (Arm 1, the leaderless room, is decided by each caller before it gets here:
/// an absent incumbent is not a seat change to authorize.)
///
/// Arm 2 compares TOOL only, with no session check — deliberately asymmetric
/// with [`authorize_claim_removal`], which refuses a sibling session wearing the
/// owner's label. The seat is a tool-level concept: `projected_lead` yields a
/// tool name, `set_lead` writes `from_session_id: None`, and there is no session
/// on the incumbent side to compare against. A sibling shell running
/// `--tool <incumbent>` therefore can move the seat. That is the same residual
/// the module header names — `tool` is self-asserted — and not a second one.
///
/// Arm 3's input is a projection an ungated retraction can regress: see RC-071b.
fn lead_seat_change_allowed(
    fact: &Fact,
    incumbent: &str,
    snapshot: &RoomSnapshot,
    coord: &CoordinationConfig,
) -> bool {
    // 2. Genuine handoff / self-relinquish / self-correction. Absence is not
    // identity: a fact carrying no `tool` never matches the incumbent.
    if fact.tool.as_deref() == Some(incumbent) {
        return true;
    }
    // 3. Stale incumbent.
    if lead_is_stale(snapshot, incumbent, coord) {
        return true;
    }
    // 4. Acknowledged seizure.
    fact.evidence
        .iter()
        .any(|item| item.trim() == LEAD_FORCE_MARKER)
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
            ..Default::default()
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

    fn refusal_with(fact: &Fact, facts_before: &[Fact], snapshot: &RoomSnapshot) -> String {
        let coord = CoordinationConfig::default();
        match assert_write_authorized(fact, facts_before, snapshot, &coord) {
            Ok(()) => panic!("expected a refusal, the write was authorized"),
            Err(err) => err.to_string(),
        }
    }

    fn authorized_with(fact: &Fact, facts_before: &[Fact], snapshot: &RoomSnapshot) {
        let coord = CoordinationConfig::default();
        assert_write_authorized(fact, facts_before, snapshot, &coord)
            .unwrap_or_else(|err| panic!("expected authorization, got refusal: {err}"));
    }

    fn refusal(fact: &Fact, snapshot: &RoomSnapshot) -> String {
        refusal_with(fact, &[], snapshot)
    }

    fn authorized(fact: &Fact, snapshot: &RoomSnapshot) {
        authorized_with(fact, &[], snapshot)
    }

    fn active_session_presence(tool: &str, session: &str, token: &str) -> Fact {
        Fact {
            event_id: "session-active".to_string(),
            seq: 1,
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            from_session_id: Some(session.to_string()),
            subject: "session active".to_string(),
            evidence: vec![
                "protocol:session_state=active".to_string(),
                crate::session_identity::session_close_token_hash_marker(token),
            ],
            created_at: iso_ago(0),
            ..Fact::default()
        }
    }

    fn session_close(tool: &str, session: &str, token: &str) -> Fact {
        Fact {
            event_id: "session-close".to_string(),
            seq: 2,
            kind: FactKind::Session,
            tool: Some(tool.to_string()),
            from_session_id: Some(session.to_string()),
            subject: "session closed".to_string(),
            evidence: vec![
                "protocol:session_state=closed".to_string(),
                format!(
                    "{}{}",
                    crate::session_identity::SESSION_CLOSE_TOKEN_REVEAL_PREFIX,
                    token
                ),
            ],
            created_at: iso_ago(0),
            ..Fact::default()
        }
    }

    #[test]
    fn session_close_requires_the_registered_one_time_token() {
        let presence = active_session_presence("codex:victim", "sess:victim", "secret");
        let wrong = session_close("codex:victim", "sess:victim", "wrong");
        let err = refusal_with(
            &wrong,
            std::slice::from_ref(&presence),
            &RoomSnapshot::default(),
        );
        assert!(err.contains("does not match"), "{err}");

        let correct = session_close("codex:victim", "sess:victim", "secret");
        authorized_with(
            &correct,
            std::slice::from_ref(&presence),
            &RoomSnapshot::default(),
        );
    }

    #[test]
    fn closed_session_cannot_author_any_later_fact_or_rotate_its_close_token() {
        let presence = active_session_presence("codex:victim", "sess:victim", "secret");
        let closed = session_close("codex:victim", "sess:victim", "secret");
        let facts = vec![presence.clone(), closed];
        let claim = Fact {
            event_id: "later-claim".to_string(),
            seq: 3,
            kind: FactKind::Claim,
            tool: presence.tool.clone(),
            from_session_id: presence.from_session_id.clone(),
            subject: "must fail".to_string(),
            scope: vec!["file:src/a.rs".to_string()],
            ..Fact::default()
        };
        assert!(refusal_with(&claim, &facts, &RoomSnapshot::default()).contains("is closed"));

        let artifact = Fact {
            event_id: "later-artifact".to_string(),
            seq: 3,
            kind: FactKind::Artifact,
            tool: presence.tool.clone(),
            from_session_id: presence.from_session_id.clone(),
            subject: "must also fail".to_string(),
            ..Fact::default()
        };
        assert!(refusal_with(&artifact, &facts, &RoomSnapshot::default()).contains("is closed"));

        let mut duplicate_close = session_close("codex:victim", "sess:victim", "secret");
        duplicate_close.event_id = "forged-duplicate-close".to_string();
        duplicate_close.seq = 3;
        assert!(
            refusal_with(&duplicate_close, &facts, &RoomSnapshot::default()).contains("is closed")
        );

        let rotated = active_session_presence("codex:victim", "sess:victim", "rotated");
        assert!(
            refusal_with(
                &rotated,
                std::slice::from_ref(&presence),
                &RoomSnapshot::default()
            )
            .contains("cannot rotate")
        );
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

    /// Arm 3's negative control. Every typed reaper marker and the expired
    /// lease match the live claim; only the durable system-author marker
    /// differs. Removing the `is_system_authored` arm therefore makes this
    /// ordinary actor authorized and turns the test red.
    #[test]
    fn an_ordinary_actor_cannot_use_an_otherwise_valid_typed_expiry() {
        let (mut claim, mut snapshot) = room_with_claim("victim:01", 60);
        claim
            .evidence
            .push("lease_expires_at:2000-01-01T00:00:00Z".to_string());
        snapshot.active_claims = vec![claim.clone()];

        let mut expiry = Fact {
            event_id: "expiry-1".to_string(),
            kind: FactKind::ClaimExpired,
            tool: Some("reaper-agent:01".to_string()),
            role: Some(crate::SYSTEM_ROLE.to_string()),
            from_session_id: Some("sess:reaper-agent:01".to_string()),
            ref_id: Some(claim.event_id.clone()),
            subject: "typed lease expiry".to_string(),
            evidence: vec![
                format!("reaper:ref_id={}", claim.event_id),
                "reaper:reason=lease-expired".to_string(),
                "reaper:owner=victim:01".to_string(),
                "reaper:owner_session=sess:victim:01".to_string(),
                "reaper:observed=unknown".to_string(),
            ],
            created_at: iso_ago(0),
            ..Fact::default()
        };

        assert!(
            claim_lease_expired(&claim),
            "the fixture lease must be expired"
        );
        assert!(
            is_typed_reaper_lease_expiry(&expiry, &claim),
            "the complete typed evidence must authorize a system-authored expiry"
        );
        authorized(&expiry, &snapshot);

        expiry.role = None;
        assert!(
            !is_typed_reaper_lease_expiry(&expiry, &claim),
            "the same typed evidence must not authorize an ordinary actor"
        );
        let err = refusal(&expiry, &snapshot);
        assert!(
            err.contains("not the owner") && err.contains("typed ClaimExpired cleanup"),
            "the refusal must identify the missing authority, not malformed evidence; got: {err}"
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

    // ---- RC-071a: retracting the fact the lead seat rests on ---------------

    /// A `role:lead` decision as `rally lead assign` writes it: `tool` = the
    /// ACTOR, `target` = the BENEFICIARY (ARP-R-01's attribution half).
    fn lead_decision(id: &str, actor: &str, beneficiary: &str, seq: i64) -> Fact {
        Fact {
            event_id: id.to_string(),
            kind: FactKind::Decision,
            subject: claim_authority::LEAD_SUBJECT.to_string(),
            tool: Some(actor.to_string()),
            target: Some(beneficiary.to_string()),
            from_session_id: Some(format!("sess:{actor}")),
            seq,
            created_at: iso_ago(0),
            ..Fact::default()
        }
    }

    /// A room seated by `lead`, who has been silent for `lead_silent_secs`.
    /// The seat uses the LARGE (120 min) window — the coarsest thing in the
    /// room gets the most patient timeout.
    fn room_with_lead(lead: &str, lead_silent_secs: i64) -> (Vec<Fact>, RoomSnapshot) {
        let seat = lead_decision("lead-1", lead, lead, 1);
        let snapshot = RoomSnapshot {
            lead: Some(lead.to_string()),
            squads: vec![squad(lead, lead_silent_secs)],
            ..Default::default()
        };
        (vec![seat], snapshot)
    }

    /// RC-071a, THE defect. The lead seat is a non-claim fact that CARRIES
    /// AUTHORITY, and R1's ruling scoped ungated retraction to "non-claim
    /// facts". `lead handoff`/`assign`/`relinquish` were gated while the
    /// retraction of the decision underneath the seat walked straight past —
    /// one command, and RC-037's room-wide claim gate plus RC-038's room-freeze
    /// both re-open. Disable `assert_lead_retraction_authorized` and this test
    /// is the one that goes red.
    #[test]
    fn a_non_owner_cannot_retract_the_decision_the_lead_seat_rests_on() {
        // Lead seen a minute ago: nowhere near the 120-minute large window.
        let (facts, snapshot) = room_with_lead("lead:01", 60);
        let fact = retraction_by("codex:rogue", "lead-1");

        assert!(
            needs_authority_check(&fact),
            "a seat-targeting retraction must reach the authority gate at all"
        );
        let err = refusal_with(&fact, &facts, &snapshot);
        assert!(
            err.contains("retract failed") && err.contains("lead seat"),
            "the refusal must name the act and what it would move; got: {err}"
        );
        assert!(
            err.contains("lead:01"),
            "the refusal must name the incumbent so the caller knows who to ask; got: {err}"
        );
    }

    /// Arm 2. Withdrawing the decision you hold the seat by is the ordinary
    /// self-correction path — the same authority `lead relinquish` already has.
    #[test]
    fn the_lead_can_retract_the_decision_it_holds_the_seat_by() {
        let (facts, snapshot) = room_with_lead("lead:01", 60);
        authorized_with(&retraction_by("lead:01", "lead-1"), &facts, &snapshot);
    }

    /// Arm 3. Same large-work silence window `lead assign` takes over on,
    /// reused rather than restated, so a crashed lead never freezes the seat.
    #[test]
    fn a_stale_leads_seat_decision_can_be_retracted_by_a_peer() {
        let fact = retraction_by("codex:peer", "lead-1");

        let (facts, fresh) = room_with_lead("lead:01", 119 * 60);
        assert!(
            refusal_with(&fact, &facts, &fresh).contains("retract failed"),
            "119 minutes of silence is inside the large window and must still refuse"
        );

        let (facts, stale) = room_with_lead("lead:01", 121 * 60);
        authorized_with(&fact, &facts, &stale);
    }

    /// Arm 4. An acknowledged seizure is reachable through this spelling too.
    /// Making retraction STRICTER than `lead assign --force` would buy nothing
    /// — the actor would use the typed command, which is more legible — while
    /// costing a second policy body to keep in sync.
    #[test]
    fn an_acknowledged_seizure_marker_authorizes_the_seat_retraction() {
        let (facts, snapshot) = room_with_lead("lead:01", 60);
        let mut fact = retraction_by("codex:rogue", "lead-1");
        fact.evidence.push(LEAD_FORCE_MARKER.to_string());
        authorized_with(&fact, &facts, &snapshot);
    }

    /// The invariant that keeps this fix from making the room brittle, and the
    /// operator's ruling in one line: authority-carrying facts are gated, prose
    /// is free. A live seat does not gate the withdrawal of an unrelated note.
    #[test]
    fn retracting_a_fact_that_carries_no_authority_stays_ungated() {
        let (facts, snapshot) = room_with_lead("lead:01", 60);
        authorized_with(
            &retraction_by("codex:rogue", "some-artifact-id"),
            &facts,
            &snapshot,
        );
    }

    /// A SUPERSEDED lead decision carries no authority — the seat does not rest
    /// on it — so withdrawing it moves nothing and stays open. The gate asks
    /// "does the seat move", not "is the target a lead decision", and this is
    /// the difference showing up.
    #[test]
    fn retracting_a_superseded_lead_decision_stays_ungated() {
        let facts = vec![
            lead_decision("lead-1", "lead:01", "lead:01", 1),
            lead_decision("lead-2", "lead:01", "lead:02", 2),
        ];
        let snapshot = RoomSnapshot {
            lead: Some("lead:02".to_string()),
            squads: vec![squad("lead:02", 60), squad("lead:01", 60)],
            ..Default::default()
        };
        authorized_with(&retraction_by("codex:rogue", "lead-1"), &facts, &snapshot);
    }

    /// The case a spelling-keyed gate would still have missed after the obvious
    /// fix: withdrawing the CURRENT seat decision does not empty the seat here,
    /// it falls the room back to the EARLIER lead. Authority still moves, so it
    /// is still the incumbent's call, and the refusal says where the seat would
    /// have gone.
    #[test]
    fn a_retraction_that_falls_the_seat_back_to_an_earlier_lead_is_gated() {
        let facts = vec![
            lead_decision("lead-1", "lead:01", "lead:01", 1),
            lead_decision("lead-2", "lead:01", "lead:02", 2),
        ];
        let snapshot = RoomSnapshot {
            lead: Some("lead:02".to_string()),
            squads: vec![squad("lead:02", 60), squad("lead:01", 60)],
            ..Default::default()
        };
        let err = refusal_with(&retraction_by("codex:rogue", "lead-2"), &facts, &snapshot);
        assert!(
            err.contains("lead:02") && err.contains("hands the seat to lead:01"),
            "the refusal must name the incumbent and where the seat would land; got: {err}"
        );
    }

    /// A LEADERLESS room is ungated, matching arm 1 of the transfer gate:
    /// anyone may take an empty seat, so restoring a former lead by withdrawing
    /// the relinquish is strictly weaker than the `lead assign` that is already
    /// permitted there.
    #[test]
    fn reviving_the_seat_in_a_leaderless_room_stays_ungated() {
        let relinquish = Fact {
            event_id: "lead-2".to_string(),
            subject: claim_authority::LEAD_RELINQUISHED_SUBJECT.to_string(),
            seq: 2,
            ..lead_decision("lead-2", "lead:01", "lead:01", 2)
        };
        let facts = vec![lead_decision("lead-1", "lead:01", "lead:01", 1), relinquish];
        let snapshot = RoomSnapshot {
            squads: vec![squad("lead:01", 60)],
            ..Default::default()
        };
        assert_eq!(
            claim_authority::projected_lead(&facts),
            None,
            "precondition: the relinquish left the room leaderless"
        );
        authorized_with(&retraction_by("codex:rogue", "lead-2"), &facts, &snapshot);
    }

    /// The availability half of RC-071a, and the reason the TRANSFER gate had
    /// to move to `projected_lead` in the same change.
    ///
    /// A lead may legitimately withdraw the decision it holds the seat by. The
    /// room then reports no lead — and a transfer gate still reading the RAW
    /// ledger would keep refusing every later `lead assign`, wedging a seat the
    /// room says is empty. Gating the seat's removal without this makes a
    /// security fix into an availability defect; revert `projected_lead` to the
    /// raw derivation in `assert_lead_transfer_authorized` and this goes red.
    #[test]
    fn a_seat_the_lead_withdrew_can_be_taken_by_the_next_agent() {
        let seat = lead_decision("lead-1", "lead:01", "lead:01", 1);
        let withdrawn = Fact {
            seq: 2,
            ..retraction_by("lead:01", "lead-1")
        };
        let facts = vec![seat, withdrawn];
        let snapshot = RoomSnapshot {
            squads: vec![squad("lead:01", 60)],
            ..Default::default()
        };
        assert_eq!(
            claim_authority::projected_lead(&facts),
            None,
            "precondition: the room reports no lead once the seat decision is withdrawn"
        );
        authorized_with(
            &lead_decision("lead-3", "codex:peer", "codex:peer", 3),
            &facts,
            &snapshot,
        );
    }

    /// Retraction resolution is FLAT, in the projection and therefore in this
    /// gate: `retraction::retracted_ids` collects every target in one pass, so
    /// retracting a retraction does not un-retract its target ("a fact a peer
    /// already consumed cannot be un-read"). Pinned because the gate's whole
    /// correctness argument is that it reads the seat the room shows — if that
    /// semantic ever changes, this gate has to change with it, and this test is
    /// where that conversation starts.
    #[test]
    fn the_gate_and_the_projection_agree_on_flat_retraction_resolution() {
        let seat = lead_decision("lead-1", "lead:01", "lead:01", 1);
        let withdrawn = Fact {
            seq: 2,
            ..retraction_by("lead:01", "lead-1")
        };
        let undo = Fact {
            event_id: "r-2".to_string(),
            seq: 3,
            ..retraction_by("lead:01", "r-1")
        };
        let facts = vec![seat, withdrawn, undo];
        let snapshot = RoomSnapshot {
            squads: vec![squad("lead:01", 60)],
            ..Default::default()
        };
        assert_eq!(
            claim_authority::projected_lead(&facts),
            None,
            "the seat stays withdrawn — retraction resolution does not nest"
        );
        // And the gate agrees: no incumbent, so nothing to authorize against.
        authorized_with(
            &lead_decision("lead-3", "codex:peer", "codex:peer", 4),
            &facts,
            &snapshot,
        );
    }
}
