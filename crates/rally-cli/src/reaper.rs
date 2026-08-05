// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! In-room stale-state REAPER — the actuator that physically removes over-TTL
//! claims and stale squad-lead leases.
//!
//! Entry point: `run_reap_stale(apply)`.
//!
//! Design principles:
//! - COMPOSE existing eligibility functions; do NOT reimplement staleness math.
//! - FAIL-CLOSED: any claim whose owner timestamp is unparseable or whose squad
//!   entry is absent is NEVER staged for removal. This guarantee is INHERITED
//!   from `claim_reclaim_eligible` and `takeover_eligible_owners`.
//! - Idempotent: re-running on an already-clean room is a safe no-op because
//!   `active_claims` only surfaces claims that are not yet closed.
//! - When `apply` is false the report describes WHAT WOULD happen (dry-run).

use schemars::JsonSchema;
use serde::Serialize;

use crate::error::Result;
use crate::store::{Fact, FactKind, RoomStore};
use crate::{FACT_SCHEMA, new_id, now_string};

/// Default handoff expiry: 30 days.
///
/// Chosen to match the log-rotation threshold rather than the 24 h
/// `stale_wait_secs` de-prioritisation, because those answer different
/// questions. 24 h is "stop ranking this first"; expiry is "this obligation is
/// over". A handoff unanswered for a month is not pending work — measured at 42
/// of 51 open handoffs in this repo's own room. Override with
/// `coordination.handoff_expiry_secs` or `RALLY_HANDOFF_EXPIRY_SECS`; `0`
/// disables expiry.
pub(crate) const DEFAULT_HANDOFF_EXPIRY_SECS: i64 = 30 * 24 * 60 * 60;

// =============================================================================
// Output types
// =============================================================================

/// A claim that was (or would be) reaped.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ReapedClaim {
    /// The `event_id` of the original claim fact.
    pub(crate) claim_id: String,
    /// The tool that held (owns) the claim.
    pub(crate) owner_tool: String,
    /// Scopes the claim covered.
    pub(crate) scope: Vec<String>,
    /// The `lease_expires_at` evidence value from the claim, if any.
    pub(crate) lease_expires_at: Option<String>,
    /// Why this claim was reaped: "owner-stale" | "lease-expired" |
    /// "owner-stale+lease-expired".
    pub(crate) reason: String,
}

/// A handoff that was (or would be) expired.
///
/// Handoffs had no expiry verdict at all: `open_handoffs` closes one only on a
/// `Resolve`/`Receipt`/`Artifact` that references it, so an unanswered handoff
/// was immortal. `next` de-prioritises after `stale_wait_secs` (24 h), which
/// changes ranking and nothing else — measured at 42 of 51 open handoffs older
/// than 30 days, every one of them still projected, ranked, and budgeted on
/// every room read. Claims got a lease; handoffs never did. This closes that
/// parity gap.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ReapedHandoff {
    /// The `event_id` of the original handoff fact.
    pub(crate) handoff_id: String,
    /// The tool that wrote the handoff.
    pub(crate) from_tool: String,
    /// The handoff's target, when it named one.
    pub(crate) target: Option<String>,
    /// The handoff's `created_at`.
    pub(crate) created_at: String,
    /// Whole days the handoff sat unanswered.
    pub(crate) age_days: i64,
}

/// Result returned by `run_reap_stale`.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ReapReport {
    /// Claims that were reaped (ClaimExpired fact appended for each).
    pub(crate) claims_reaped: Vec<ReapedClaim>,
    /// Handoffs that were expired (a `Resolve` fact appended for each).
    #[serde(default)]
    pub(crate) handoffs_expired: Vec<ReapedHandoff>,
    /// Tools whose squad status was idle long enough to be cleared (informational;
    /// squads are not directly removable — this records which tools were stale).
    pub(crate) squads_idle_cleared: Vec<String>,
    /// Tool whose lead lease was relinquished, if any.
    ///
    /// Under `apply` this is populated ONLY after the relinquish fact is
    /// durably appended (D7). It previously reported the tool whether or not
    /// the write landed, so a caller could read "seat is open" off a report
    /// whose durable write had failed.
    pub(crate) lead_relinquished: Option<String>,
    /// Number of inspected items that were KEPT rather than closed: claims with
    /// a future-dated lease, an unparseable owner timestamp, or a live owner;
    /// handoffs inside their TTL or with an unparseable `created_at`; and
    /// anything whose durable append failed, including a stale lead whose
    /// relinquish did not land (D7).
    pub(crate) preserved_future_or_active: usize,
    /// The pass ran in write mode (`apply=true`) — NOT that every staged write
    /// landed. Per-item success is carried by the item lists: an append that
    /// failed leaves its entry out of `claims_reaped` / `handoffs_expired` /
    /// `lead_relinquished` and lands in `preserved_future_or_active` instead.
    pub(crate) applied: bool,
}

// =============================================================================
// Core logic
// =============================================================================

/// Reap over-TTL claims and stale lead leases in the current room.
///
/// When `apply` is false: compute eligibility + populate the report, but write
/// no facts (dry-run). When `apply` is true: append one `ClaimExpired` fact per
/// eligible claim via `append_state_transition_verified` (preserves the
/// mutation lock + SEC-001 re-check), then a `Decision` relinquish fact if the
/// lead is stale.
pub(crate) fn run_reap_stale(apply: bool) -> Result<ReapReport> {
    let room = RoomStore::open()?;
    run_reap_stale_in_room(&room, apply)
}

/// Default auto-reap interval: **0 — OFF**. Opt in with
/// `coordination.auto_reap_interval_secs` or `RALLY_AUTO_REAP_INTERVAL_SECS`.
///
/// It shipped ON for exactly one commit. An independent audit measured three
/// separate failures, all against the release binary, and each of them is worse
/// than the stale state the reap was cleaning up:
///
/// 1. **`rally enter` failed for every concurrent agent.** 8 concurrent enters
///    against a room with 6 eligible claims: auto-reap ON gave 8/8 exit 4
///    ("mutating command exceeded 3000ms wall-clock budget BEFORE its primary
///    durable append committed; failing closed"); OFF gave 8/8 exit 0. The
///    "never fails enter" property was implemented for the reaper's own `Err`
///    return and never held against the mutation watchdog sitting above it.
/// 2. **It closed LIVE agents' claims.** Nothing in production renews
///    `lease_expires_at` — `renew_claim_lease` has no production caller — so
///    every single-file claim expires 30 minutes after it is made and every
///    coarse claim after 2 hours, regardless of whether the owner is working.
///    Measured: an active owner's claim was closed by a PEER's `enter`, the
///    owner was never told, and a third agent then claimed the same file with
///    no conflict. That is the collision Rally exists to prevent.
/// 3. **RC-044** already records concurrent `rally enter` as an unfixed
///    store-corruption path. Adding a mutation-heavy pass to it widened a known
///    defect without a control.
///
/// So the call site stays — the reaper is reachable, which was the actual
/// finding — but it is opt-in until lease renewal exists and concurrent enter
/// is bounded. Turning it on by default before then trades a slow leak for a
/// fast outage.
pub(crate) const DEFAULT_AUTO_REAP_INTERVAL_SECS: i64 = 0;

/// Marker recording the last auto-reap, relative to `.rally/`.
const AUTO_REAP_MARKER: &str = ".last-auto-reap";

/// Run the reaper from `rally enter`, at most once per interval.
///
/// The reaper was correct and unreachable. `--reap-stale --apply` is the only
/// caller, nothing invokes it, and a dry run against this repo's own room
/// reported **69 of 69 active claims already eligible** — every claim in the
/// room was reapable and none had been reaped. A verdict nothing acts on is not
/// a policy; this is the call site.
///
/// `enter` is the right hook because it is the one command every agent runs,
/// once, at the start of a session — the moment stale state costs the most and
/// a few milliseconds of cleanup costs the least.
///
/// Three properties this must not violate:
/// - **Never fails `enter`.** Any error is reported to stderr and swallowed.
///   Cleanup is not worth blocking an agent's session start.
/// - **Rate-limited by a file marker, not by the ledger — SEQUENTIALLY only.**
///   The marker is `.rally/.last-auto-reap`; a missing or unparseable marker
///   reaps (fail-toward-cleanup, since the reaper's own eligibility math is
///   fail-closed). This bounds how OFTEN a pass may run, not how MANY may run
///   at once: see D8 at the marker write below for the concurrent bound the
///   check-and-set does not deliver.
/// - **Opt-out.** `RALLY_NO_AUTO_REAP=1`, or `auto_reap_interval_secs: 0`.
///
/// Returns the report when a reap ran, `None` when it was skipped.
pub(crate) fn maybe_reap_on_enter(room: &RoomStore) -> Option<ReapReport> {
    if std::env::var("RALLY_NO_AUTO_REAP").is_ok_and(|v| v == "1") {
        return None;
    }
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let interval = coord.auto_reap_interval_secs;
    if interval <= 0 {
        return None;
    }

    let marker = room.repo_root().join(".rally").join(AUTO_REAP_MARKER);
    let now = chrono::Utc::now();
    if let Ok(text) = std::fs::read_to_string(&marker)
        && let Ok(last) = chrono::DateTime::parse_from_rfc3339(text.trim())
    {
        let age = (now - last.with_timezone(&chrono::Utc)).num_seconds();
        // A FUTURE-dated marker reads as `age < interval` forever, which
        // disables the reaper permanently. Same-UID writes are conceded by the
        // trust model, but a marker that silently switches off a load-bearing
        // control should not also be undetectable: treat a future stamp as
        // stale and reap.
        if (0..interval).contains(&age) {
            return None;
        }
    }

    // Stamp the interval BEFORE reaping.
    //
    // D8 — THE BOUND THIS DOES NOT DELIVER. The comment here used to claim "at
    // most one extra pass runs instead of one per agent". It does not. The read
    // above and the write below are two separate syscalls with no lock, no
    // `O_EXCL`, and no compare-and-swap, so N processes that all read the stale
    // marker before any of them writes it ALL proceed: under N concurrent
    // `rally enter` calls, N passes fire. The honest bound is:
    //
    // - **Sequential:** at most one pass per `interval` — the age check above
    //   is a real gate once a previous pass's marker is visible.
    // - **Concurrent:** UNBOUNDED in the number of overlapping enters. Nothing
    //   here serialises them.
    //
    // Why the racy version stays rather than growing a lock here: the correct
    // primitive already exists as `store::acquire_room_mutation_lock`, and it
    // is private to `store.rs`. A second, differently-shaped lock in this file
    // would be the divergence, not the fix — the fix is to export that one.
    // Meanwhile the exposure is capped from the other side: auto-reap is OFF by
    // default (`DEFAULT_AUTO_REAP_INTERVAL_SECS`), each pass is
    // `ReapMode::LeaseOnly` and capped at `AUTO_REAP_MAX_FACTS`, and the reap
    // is idempotent, so a duplicated pass wastes work rather than corrupting
    // state. That is a cost argument, not a bound, and it is the reason the
    // "concurrent enter is bounded" gate on turning this default back on is
    // still OPEN.
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Surface a failed marker write (D7-adjacent). Silently dropping it turns
    // the rate limit off entirely — an unwritable `.rally/` means every single
    // `enter` reaps, which is the measured-outage condition, not a slow leak.
    if let Err(e) = std::fs::write(
        &marker,
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ) {
        eprintln!(
            "rally: auto-reap rate-limit marker not written ({}): {e} — the next enter will reap again",
            marker.display()
        );
    }

    match run_reap_stale_in_room_with_mode(room, true, ReapMode::LeaseOnly) {
        Ok(report) => Some(report),
        Err(err) => {
            eprintln!("rally: auto-reap skipped ({err})");
            None
        }
    }
}

/// Which staleness signals a reap pass is allowed to act on.
///
/// The two signals are not equally trustworthy, and wiring the reaper to
/// `rally enter` is what made the difference matter.
///
/// - **Lease expiry** is stamped by the claim's OWN writer, at claim time, and
///   is monotonic — a past lease cannot un-expire. Backdating one only expires
///   the backdater's own claim.
/// - **Owner staleness** is derived from `last_seen_ts`, which is the
///   `created_at` of the highest-seq fact naming that tool — a value written
///   verbatim from the ledger line, not from the reader's clock. `.rally/log/`
///   is git-tracked, so one committed line carrying `tool: "victim:01"`, a high
///   `seq`, and a `created_at` three hours in the past makes the victim look
///   stale to every reader. Under the old design that only mattered if a human
///   ran `doctor --reap-stale --apply`; wired to `enter`, it would fire on the
///   next session start of any agent and close every one of the victim's
///   claims. The under-lock revival guard re-reads the same poisoned projection
///   and confirms the reap rather than blocking it.
///
/// So the automatic path takes only the signal an attacker cannot aim at
/// someone else, and owner-staleness stays behind the deliberate operator
/// command. That keeps the fix to "the reaper had no caller" without turning a
/// peer-controlled timestamp into a claim-destruction primitive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReapMode {
    /// Act on both signals. `rally doctor --reap-stale --apply` — a human ran it.
    Full,
    /// Act only on writer-stamped lease expiry. The automatic `enter` path.
    LeaseOnly,
}

/// Most facts one automatic pass may append.
///
/// A ledger seeded with thousands of eligible claims would otherwise make the
/// first `enter` of each interval spend its whole budget in the reap loop, and
/// the hook's watchdog kills the process group at 5s — costing that agent its
/// presence registration in exchange for cleanup nobody asked for. Capping
/// resumes on the next interval instead; the reaper is idempotent, so a partial
/// pass is a shorter pass, not a broken one.
const AUTO_REAP_MAX_FACTS: usize = 200;

/// Inner implementation — takes an explicit `&RoomStore` so tests can inject a
/// temp store without touching the process-global cwd.
pub(crate) fn run_reap_stale_in_room(room: &RoomStore, apply: bool) -> Result<ReapReport> {
    run_reap_stale_in_room_with_mode(room, apply, ReapMode::Full)
}

pub(crate) fn run_reap_stale_in_room_with_mode(
    room: &RoomStore,
    apply: bool,
    mode: ReapMode,
) -> Result<ReapReport> {
    let snapshot = room.snapshot()?;
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();

    let mut claims_reaped: Vec<ReapedClaim> = Vec::new();
    let mut preserved: usize = 0;

    // Identify the stale-owner set at snapshot time (squad-level, 2h bar).
    // Used for the lead relinquish decision; claim eligibility is per-claim
    // (size-scaled via claim_reclaim_eligible, which composes the same logic).
    let stale_owners = snapshot.takeover_eligible_owners();

    // Compute the lease-expired claim_id set: claims whose OWN lease timestamp
    // has provably passed NOW, regardless of owner-squad liveness.
    // fail-closed: expired_claims only includes claims with a parseable
    // lease_expires_at <= now; unparseable or missing lease → not included.
    let facts = room.facts()?;
    let claim_index = crate::claim_authority::index_from_facts(&facts);
    let lease_expired_ids: std::collections::BTreeSet<String> =
        crate::claim_authority::expired_claims(&claim_index, &facts, chrono::Utc::now())
            .into_iter()
            .map(|r| r.claim_id)
            .collect();

    // --- Evaluate each active claim ---
    for claim in &snapshot.active_claims {
        let (owner_eligible, _size) = snapshot.claim_reclaim_eligible(claim, &coord);
        let lease_eligible = lease_expired_ids.contains(&claim.event_id);

        // A claim is reaped when EITHER its owner-squad is >timeout stale OR
        // its own lease has provably expired. Both signals are fail-closed.
        //
        // ReapMode::LeaseOnly drops the owner-stale signal. `last_seen_ts` is
        // the `created_at` of the highest-seq fact naming that tool, written
        // verbatim from a git-tracked ledger line, so one committed fact
        // carrying a victim's id and a backdated timestamp makes the victim
        // look stale to every reader. The lease is stamped by the claim's own
        // writer and is monotonic, so backdating one only expires the
        // backdater's own claim.
        let eligible = match mode {
            ReapMode::Full => owner_eligible || lease_eligible,
            ReapMode::LeaseOnly => lease_eligible,
        };
        if !eligible {
            preserved += 1;
            continue;
        }
        if mode == ReapMode::LeaseOnly && claims_reaped.len() >= AUTO_REAP_MAX_FACTS {
            preserved += 1;
            continue;
        }

        let reason = match (owner_eligible, lease_eligible) {
            (true, true) => "owner-stale+lease-expired",
            (true, false) => "owner-stale",
            (false, true) => "lease-expired",
            (false, false) => unreachable!(),
        }
        .to_string();

        // Build the lease_expires_at provenance from the claim's evidence, if any.
        let lease_expires_at = claim
            .evidence
            .iter()
            .find_map(|e| e.strip_prefix("lease_expires_at:"))
            .map(str::to_string);

        let reaped = ReapedClaim {
            claim_id: claim.event_id.clone(),
            owner_tool: claim.tool.clone().unwrap_or_default(),
            scope: claim.scope.clone(),
            lease_expires_at,
            reason: reason.clone(),
        };

        if apply {
            // Append a ClaimExpired fact that closes this claim.
            // `append_state_transition_verified` re-asserts eligibility under
            // the held mutation lock (SEC-001 safeguard for Release facts).
            // For ClaimExpired we use `append_fact_verified` (no pre-condition
            // check needed beyond the claim still being live — the projection
            // already handles duplicate ClaimExpired via ref_id dedup).
            let expired_fact = Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: FactKind::ClaimExpired,
                tool: Some("rally".to_string()),
                role: None,
                subject: format!(
                    "reaper: claim {} expired (reason={}, owner: {})",
                    claim.event_id,
                    reason,
                    claim.tool.as_deref().unwrap_or("unknown")
                ),
                scope: claim.scope.clone(),
                created_at: now_string(),
                summary: Some(format!("reaper:reason={}", reaped.reason)),
                // Stamp the reap reason onto evidence so the under-lock re-check
                // in `store::append_fact` (SEC-001 owner-revival guard) can
                // distinguish a racy OWNER-STALE close (must re-validate) from a
                // monotonic LEASE-EXPIRED close (exempt — a past lease cannot
                // un-expire). The owner-tool is stamped too so the guard knows
                // whose liveness to re-check without re-parsing the subject.
                evidence: vec![
                    format!("reaper:ref_id={}", claim.event_id),
                    format!("reaper:reason={}", reaped.reason),
                    format!(
                        "reaper:owner={}",
                        claim.tool.as_deref().unwrap_or("unknown")
                    ),
                ],
                target: None,
                ref_id: Some(claim.event_id.clone()),
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            // Best-effort: if the claim was concurrently closed before we got
            // the lock, skip it silently — the room is already clean.
            match room.append_fact_verified(&expired_fact) {
                Ok(_) => {}
                Err(e) => {
                    // Log but do not abort the whole reap run.
                    eprintln!(
                        "reaper: skipping {} (already closed or lock error): {}",
                        claim.event_id, e
                    );
                    preserved += 1;
                    continue;
                }
            }
        }

        claims_reaped.push(reaped);
    }

    // --- Handoff expiry ---
    //
    // Same fail-closed contract as claims: an unparseable `created_at` is
    // NEVER expired. A handoff closes on a later Resolve/Receipt/Artifact that
    // references it, so the reaper writes a `Resolve` with `from_session_id:
    // None` — which `handoff_closer_matches_target` treats as target-agnostic,
    // the same path legacy completion markers take.
    let handoff_ttl_secs = coord.handoff_expiry_secs;
    let now = chrono::Utc::now();
    let mut handoffs_expired: Vec<ReapedHandoff> = Vec::new();
    if handoff_ttl_secs > 0 {
        for handoff in &snapshot.open_handoffs {
            let Some(created) = chrono::DateTime::parse_from_rfc3339(&handoff.created_at)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc))
            else {
                // Fail closed: no parseable age means no expiry verdict.
                preserved += 1;
                continue;
            };
            let age_secs = (now - created).num_seconds();
            if age_secs < handoff_ttl_secs {
                preserved += 1;
                continue;
            }

            let reaped = ReapedHandoff {
                handoff_id: handoff.event_id.clone(),
                from_tool: handoff.tool.clone().unwrap_or_default(),
                target: handoff.target.clone(),
                created_at: handoff.created_at.clone(),
                age_days: age_secs / 86_400,
            };

            if apply {
                let expiry_fact = Fact {
                    from_session_id: None,
                    schema: FACT_SCHEMA.to_string(),
                    event_id: new_id("fact"),
                    seq: 0,
                    thread_id: new_id("room"),
                    kind: FactKind::Resolve,
                    tool: Some("rally".to_string()),
                    role: None,
                    subject: format!(
                        "reaper: handoff {} expired unanswered after {} days (from: {}, to: {})",
                        handoff.event_id,
                        reaped.age_days,
                        handoff.tool.as_deref().unwrap_or("unknown"),
                        handoff.target.as_deref().unwrap_or("all"),
                    ),
                    scope: handoff.scope.clone(),
                    created_at: now_string(),
                    summary: Some("reaper:reason=handoff-expired".to_string()),
                    evidence: vec![
                        format!("reaper:ref_id={}", handoff.event_id),
                        "reaper:reason=handoff-expired".to_string(),
                        format!("reaper:age_days={}", reaped.age_days),
                    ],
                    target: None,
                    ref_id: Some(handoff.event_id.clone()),
                    status: None,
                    severity: None,
                    uri: None,
                    session: None,
                };
                match room.append_fact_verified(&expiry_fact) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "reaper: skipping handoff {} (already closed or lock error): {}",
                            handoff.event_id, e
                        );
                        preserved += 1;
                        continue;
                    }
                }
            }

            handoffs_expired.push(reaped);
        }
    }

    // --- Lead relinquish ---
    // Only relinquish the lead when the lead's owning tool is in the
    // DESTRUCTIVE stale set (>2h silence). This is the same predicate used
    // for per-claim reclaim, just at the squad level.
    //
    // D7: the append result used to be DISCARDED (`let _ = ...`) while the
    // report still named the tool, so a failed durable write produced
    // `applied: true, lead_relinquished: "<tool>"` and a caller read the seat
    // as open while the ledger still held the lease. The claim and handoff
    // paths above already got this right; this one now mirrors them exactly —
    // log, count as preserved, omit from the report.
    let lead_relinquished: Option<String> = if let Some(lead_tool) = &snapshot.lead {
        if stale_owners.contains(lead_tool.as_str()) {
            let mut relinquish_committed = true;
            if apply {
                let relinquish_fact = Fact {
                    from_session_id: None,
                    schema: FACT_SCHEMA.to_string(),
                    event_id: new_id("fact"),
                    seq: 0,
                    thread_id: new_id("room"),
                    kind: FactKind::Decision,
                    tool: Some("rally".to_string()),
                    role: None,
                    subject: "role:lead:relinquished".to_string(),
                    scope: Vec::new(),
                    created_at: now_string(),
                    summary: Some(format!(
                        "reaper: lead {} relinquished (stale owner)",
                        lead_tool
                    )),
                    evidence: vec![format!("reaper:stale-lead={lead_tool}")],
                    target: None,
                    ref_id: None,
                    status: None,
                    severity: None,
                    uri: None,
                    session: None,
                };
                match room.append_fact_verified(&relinquish_fact) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "reaper: keeping lead {lead_tool} (relinquish append failed): {e}"
                        );
                        preserved += 1;
                        relinquish_committed = false;
                    }
                }
            }
            // Dry-run keeps `relinquish_committed = true`: nothing was written,
            // and the report already says so via `applied: false`.
            if relinquish_committed {
                Some(lead_tool.clone())
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Squads that are idle (advisory 15m) are surfaced for visibility; rally
    // does not physically remove squad entries (they are projections from
    // presence facts, not separate records). Report the stale (2h) set here
    // because that is the actionable tier.
    let squads_idle_cleared: Vec<String> = stale_owners.into_iter().collect();

    Ok(ReapReport {
        claims_reaped,
        handoffs_expired,
        squads_idle_cleared,
        lead_relinquished,
        preserved_future_or_active: preserved,
        applied: apply,
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RoomStore;
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    fn unique_root(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rally-reaper-{label}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Build a backdated RFC-3339 timestamp `ago_secs` seconds in the past.
    fn past_ts(ago_secs: i64) -> String {
        use chrono::{SecondsFormat, Utc};
        (Utc::now() - chrono::Duration::seconds(ago_secs))
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Append a Presence fact for `tool` with `created_at` set to `ago_secs`
    /// seconds in the past.  The squad projection uses the highest-seq fact's
    /// `created_at` as `last_seen_ts`, so ALL facts for this tool must be
    /// backdated consistently (see `append_stale_claim`).
    fn append_presence(room: &RoomStore, tool: &str, ago_secs: i64) {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("presence: {tool}"),
            scope: Vec::new(),
            created_at: past_ts(ago_secs),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap();
    }

    /// Append a Claim fact owned by `tool` with `created_at` set to `ago_secs`
    /// seconds in the past (large-scope → WorkSize::Large → 2h reclaim bar).
    ///
    /// IMPORTANT: the squad projection uses the HIGHEST-seq fact's `created_at`
    /// as `last_seen_ts`. Because the claim is appended AFTER the presence fact
    /// (higher seq), it overrides `last_seen_ts`. Both presence and claim must
    /// share the same `ago_secs` so the owner appears consistently stale.
    fn append_claim(room: &RoomStore, event_id: &str, tool: &str) -> Fact {
        append_claim_ago(room, event_id, tool, 3 * 60 * 60)
    }

    fn append_claim_ago(room: &RoomStore, event_id: &str, tool: &str, ago_secs: i64) -> Fact {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}"),
            // Multi-scope → WorkSize::Large → 2h bar.
            scope: vec![
                format!("file:src/a_{event_id}.rs"),
                format!("file:src/b_{event_id}.rs"),
            ],
            created_at: past_ts(ago_secs),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    /// Append a small single-file claim owned by `tool` (FRESH timestamp so the
    /// owner's last_seen_ts stays live — useful for self-release tests).
    fn append_small_claim(room: &RoomStore, event_id: &str, tool: &str) -> Fact {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}"),
            scope: vec![format!("file:src/{event_id}.rs")],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    /// Append a Handoff `ago_secs` seconds in the past.
    fn append_handoff(room: &RoomStore, event_id: &str, tool: &str, ago_secs: i64) -> Fact {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Handoff,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("handoff: {event_id}"),
            scope: Vec::new(),
            created_at: past_ts(ago_secs),
            summary: None,
            evidence: Vec::new(),
            target: Some("someone-else".to_string()),
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    // -------------------------------------------------------------------------
    // Handoff expiry — parity with claims
    // -------------------------------------------------------------------------

    /// Handoffs had no expiry verdict at all: nothing closed an unanswered one,
    /// so it stayed in `open_handoffs` forever. Delete the handoff-expiry block
    /// in `run_reap_stale_in_room` and this fails.
    #[test]
    fn over_ttl_handoff_is_expired_and_leaves_open_handoffs() {
        let root = unique_root("handoff-ttl");
        let room = RoomStore::open_at(root.clone()).unwrap();

        let ago = DEFAULT_HANDOFF_EXPIRY_SECS + 24 * 60 * 60;
        let handoff = append_handoff(&room, "handoff-ancient", "author", ago);
        assert_eq!(room.snapshot().unwrap().open_handoffs.len(), 1);

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(report.handoffs_expired.len(), 1);
        assert_eq!(report.handoffs_expired[0].handoff_id, handoff.event_id);
        assert!(report.handoffs_expired[0].age_days >= 30);
        assert!(
            room.snapshot().unwrap().open_handoffs.is_empty(),
            "an expired handoff must leave open_handoffs"
        );

        fs::remove_dir_all(root).ok();
    }

    /// A handoff inside the window is untouched — expiry closes obligations
    /// that are over, not ones nobody has gotten to yet.
    #[test]
    fn fresh_handoff_is_not_expired() {
        let root = unique_root("handoff-fresh");
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_handoff(&room, "handoff-fresh", "author", 60 * 60);
        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert!(report.handoffs_expired.is_empty());
        assert_eq!(room.snapshot().unwrap().open_handoffs.len(), 1);

        fs::remove_dir_all(root).ok();
    }

    /// Fail-closed, same contract as claims: an unparseable `created_at`
    /// yields no age, so it yields no expiry verdict.
    #[test]
    fn handoff_with_unparseable_timestamp_is_never_expired() {
        let root = unique_root("handoff-bad-ts");
        let room = RoomStore::open_at(root.clone()).unwrap();

        let mut fact = append_handoff(&room, "handoff-bad", "author", 60);
        fact.created_at = "not-a-timestamp".to_string();
        fact.event_id = "handoff-bad-2".to_string();
        room.append_fact_verified(&fact).unwrap();

        let report = run_reap_stale_in_room(&room, true).unwrap();
        assert!(
            !report
                .handoffs_expired
                .iter()
                .any(|h| h.handoff_id == "handoff-bad-2"),
            "an unparseable timestamp must never produce an expiry verdict"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Dry-run reports the verdict and writes nothing — the property that made
    /// `--reap-stale` safe to run must survive handoff expiry.
    #[test]
    fn handoff_expiry_dry_run_writes_nothing() {
        let root = unique_root("handoff-dry");
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_handoff(
            &room,
            "handoff-dry",
            "author",
            DEFAULT_HANDOFF_EXPIRY_SECS + 60,
        );
        let report = run_reap_stale_in_room(&room, false).unwrap();

        assert_eq!(report.handoffs_expired.len(), 1);
        assert!(!report.applied);
        assert_eq!(
            room.snapshot().unwrap().open_handoffs.len(),
            1,
            "a dry run must not close the handoff"
        );

        fs::remove_dir_all(root).ok();
    }

    // -------------------------------------------------------------------------
    // Auto-reap on enter — the missing call site
    // -------------------------------------------------------------------------

    /// The reaper was correct and unreachable: `--reap-stale --apply` was its
    /// only caller and nothing invoked it. Delete the `maybe_reap_on_enter`
    /// call in `command_enter` and the room never gets cleaned; delete the
    /// marker write here and every concurrent enter reaps.
    #[test]
    fn auto_reap_runs_once_then_respects_the_interval() {
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Auto-reap is OFF by default (see DEFAULT_AUTO_REAP_INTERVAL_SECS),
        // so this test opts in explicitly. It grades the rate limit, not the
        // default.
        unsafe { std::env::set_var("RALLY_AUTO_REAP_INTERVAL_SECS", "3600") };
        let root = unique_root("auto-reap");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // LEASE-expired, not owner-stale: the automatic path deliberately acts
        // only on the writer-stamped signal (see `ReapMode`), because
        // owner-staleness is derived from a peer-writable timestamp.
        append_presence(&room, "owner", 5);
        append_claim_with_lease(&room, "claim-leased", "owner", &past_ts(60 * 60));

        let first = maybe_reap_on_enter(&room).expect("first enter must reap");
        assert_eq!(first.claims_reaped.len(), 1);

        // Second call inside the interval is skipped entirely — ten agents
        // entering at once must not each run a reap pass.
        append_claim_with_lease(&room, "claim-leased-2", "owner", &past_ts(60 * 60));
        assert!(
            maybe_reap_on_enter(&room).is_none(),
            "a second enter inside the interval must not reap again"
        );

        unsafe { std::env::remove_var("RALLY_AUTO_REAP_INTERVAL_SECS") };
        fs::remove_dir_all(root).ok();
    }

    /// Auto-reap must be OFF unless someone opts in. It shipped on for one
    /// commit and an audit measured 8/8 concurrent `rally enter` failures plus
    /// live agents' claims being closed; flipping the default back on without
    /// lease renewal and a concurrency bound would re-introduce both.
    #[test]
    fn auto_reap_is_off_by_default() {
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = unique_root("auto-reap-default");
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_presence(&room, "owner", 5);
        append_claim_with_lease(&room, "claim-leased", "owner", &past_ts(60 * 60));

        assert_eq!(
            DEFAULT_AUTO_REAP_INTERVAL_SECS, 0,
            "auto-reap must stay opt-in until lease renewal exists"
        );
        assert!(
            maybe_reap_on_enter(&room).is_none(),
            "the default configuration must not reap on enter"
        );
        assert_eq!(
            room.snapshot().unwrap().active_claims.len(),
            1,
            "a claim must survive `enter` under default configuration"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Auto-reap is opt-out, and the opt-out is checked before any work.
    #[test]
    fn auto_reap_respects_the_opt_out() {
        let _guard = crate::PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = unique_root("auto-reap-off");
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_presence(&room, "stale-tool", 3 * 60 * 60);
        append_claim(&room, "claim-stale", "stale-tool");

        unsafe { std::env::set_var("RALLY_NO_AUTO_REAP", "1") };
        let result = maybe_reap_on_enter(&room);
        unsafe { std::env::remove_var("RALLY_NO_AUTO_REAP") };

        assert!(result.is_none(), "RALLY_NO_AUTO_REAP=1 must skip the reap");
        assert_eq!(
            room.snapshot().unwrap().active_claims.len(),
            1,
            "the claim must survive when auto-reap is off"
        );

        fs::remove_dir_all(root).ok();
    }

    // -------------------------------------------------------------------------
    // (a) Over-TTL claim is staged and, after apply, leaves active_claims
    // -------------------------------------------------------------------------

    #[test]
    fn over_ttl_claim_is_reaped_and_leaves_active_claims() {
        let root = unique_root("over-ttl");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Owner has been silent for 3 hours (> 2h LARGE bar).
        let ago = 3 * 60 * 60_i64;
        append_presence(&room, "stale-tool", ago);
        let claim = append_claim(&room, "claim-stale", "stale-tool");

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(report.claims_reaped.len(), 1, "one claim should be reaped");
        assert_eq!(report.claims_reaped[0].claim_id, claim.event_id);
        assert!(report.applied);

        // After apply the claim must no longer appear in active_claims.
        let snap = room.snapshot().unwrap();
        let still_active = snap
            .active_claims
            .iter()
            .any(|c| c.event_id == claim.event_id);
        assert!(!still_active, "reaped claim must leave active_claims");

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (b) Unparseable last_seen_ts → NEVER staged (fail-closed)
    // -------------------------------------------------------------------------

    #[test]
    fn unparseable_owner_ts_is_never_staged() {
        let root = unique_root("bad-ts");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Step 1: append the claim first (lower seq).
        // The claim uses a backdated ts so the owner would look stale IF the
        // ts were parseable — but we will override last_seen_ts in step 2.
        append_claim_ago(&room, "claim-bad-ts", "bad-ts-tool", 3 * 60 * 60);

        // Step 2: append a Presence with a deliberately broken timestamp LAST
        // (higher seq than the claim) so it wins the squad projection.
        // The squad's last_seen_ts becomes "NOT-A-VALID-TIMESTAMP", which
        // claim_reclaim_eligible cannot parse → fail-closed → never reaped.
        let bad_fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Presence,
            tool: Some("bad-ts-tool".to_string()),
            role: None,
            subject: "presence: bad-ts-tool".to_string(),
            scope: Vec::new(),
            created_at: "NOT-A-VALID-TIMESTAMP".to_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&bad_fact).unwrap();

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(
            report.claims_reaped.len(),
            0,
            "claim with unparseable owner ts must NOT be reaped (fail-closed)"
        );
        assert_eq!(
            report.preserved_future_or_active, 1,
            "bad-ts claim must be counted as preserved"
        );

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (c) Fresh-owner (or future-dated) claim is NEVER staged
    // -------------------------------------------------------------------------

    #[test]
    fn fresh_owner_claim_is_not_staged() {
        let root = unique_root("fresh-owner");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Owner is fresh (1 minute old) — well under any reclaim bar.
        // Both presence AND claim must use a fresh timestamp so the squad
        // projection sees a fresh last_seen_ts (claim is higher-seq, wins).
        append_presence(&room, "active-tool", 60);
        // Use append_claim_ago with a fresh timestamp (60s) so the claim does
        // not override the owner's last_seen_ts to appear stale.
        append_claim_ago(&room, "claim-fresh", "active-tool", 60);

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(
            report.claims_reaped.len(),
            0,
            "claim with fresh owner must not be reaped"
        );
        assert_eq!(report.preserved_future_or_active, 1);

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (d) Idempotent: second run finds nothing eligible
    // -------------------------------------------------------------------------

    #[test]
    fn idempotent_second_run_finds_nothing() {
        let root = unique_root("idempotent");
        let room = RoomStore::open_at(root.clone()).unwrap();

        let ago = 3 * 60 * 60_i64;
        append_presence(&room, "stale-tool", ago);
        append_claim(&room, "claim-idem", "stale-tool");

        // First run reaps.
        let first = run_reap_stale_in_room(&room, true).unwrap();
        assert_eq!(first.claims_reaped.len(), 1);

        // Second run finds nothing (claim is no longer in active_claims).
        let second = run_reap_stale_in_room(&room, true).unwrap();
        assert_eq!(
            second.claims_reaped.len(),
            0,
            "second run must find nothing eligible (idempotent)"
        );
        assert_eq!(second.preserved_future_or_active, 0);

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (e) Self-release in stop: only the stopping tool's claims, not peers'
    // -------------------------------------------------------------------------

    #[test]
    fn stop_self_release_only_releases_stopping_tool_claims() {
        let root = unique_root("self-release");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Two fresh tools; each holds a claim.
        append_presence(&room, "tool-a", 10);
        append_presence(&room, "tool-b", 10);
        let claim_a = append_small_claim(&room, "claim-a", "tool-a");
        let claim_b = append_small_claim(&room, "claim-b", "tool-b");

        // Simulate self-release on stop: release only tool-a's claims.
        let snap = room.snapshot().unwrap();
        for c in snap
            .active_claims
            .iter()
            .filter(|c| c.tool.as_deref() == Some("tool-a"))
        {
            let release = Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: FactKind::Release,
                tool: Some("tool-a".to_string()),
                role: None,
                subject: format!("self-release on stop: {}", c.event_id),
                scope: c.scope.clone(),
                created_at: now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: Some(c.event_id.clone()),
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            // D7 audit — this discard is NOT the D7 defect. Nothing here
            // reports the release as applied: the assertions below re-read the
            // room and grade the DURABLE outcome, so a dropped write shows up
            // as `a_still_live == true` and fails the test. The discard is what
            // lets the loop model the production stop path, which releases
            // best-effort across many claims.
            let _ = room.append_state_transition_verified(&release);
        }

        let snap_after = room.snapshot().unwrap();
        let a_still_live = snap_after
            .active_claims
            .iter()
            .any(|c| c.event_id == claim_a.event_id);
        let b_still_live = snap_after
            .active_claims
            .iter()
            .any(|c| c.event_id == claim_b.event_id);

        assert!(!a_still_live, "tool-a claim must be released after stop");
        assert!(
            b_still_live,
            "tool-b claim must NOT be touched by tool-a stop"
        );

        // Suppress unused-variable warning for claim_b event_id which is checked above.
        let _ = claim_b;

        fs::remove_dir_all(&root).ok();
    }

    /// Append a small single-file claim owned by `tool` and authored by the
    /// live session `from_session`. FRESH timestamp so the owner stays live.
    fn append_session_claim(
        room: &RoomStore,
        event_id: &str,
        tool: &str,
        from_session: &str,
    ) -> Fact {
        let fact = Fact {
            from_session_id: Some(from_session.to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}"),
            scope: vec![format!("file:src/{event_id}.rs")],
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    #[test]
    fn stop_self_release_is_session_scoped_for_same_tool() {
        // Two co-resident sessions of the SAME tool each hold a claim. Stopping
        // session A must release only A's claim; B's claim (a live sibling) must
        // survive. The fix matches on the claim's `from_session_id`, not just
        // the tool, so a shared tool no longer over-releases.
        let root = unique_root("session-scoped-release");
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_presence(&room, "claude_code", 10);
        let claim_a = append_session_claim(&room, "claim-sa", "claude_code", "sess-A");
        let claim_b = append_session_claim(&room, "claim-sb", "claude_code", "sess-B");

        // Simulate the stop self-release for session A using the SAME filter the
        // production stop path uses (session-then-tool fallback).
        let stopping_tool = "claude_code";
        let stopping_session = "sess-A";
        let snap = room.snapshot().unwrap();
        for c in snap.active_claims.iter().filter(|c| {
            c.from_session_id.as_deref() == Some(stopping_session)
                || (c.from_session_id.is_none() && c.tool.as_deref() == Some(stopping_tool))
        }) {
            let release = Fact {
                from_session_id: None,
                schema: FACT_SCHEMA.to_string(),
                event_id: new_id("fact"),
                seq: 0,
                thread_id: new_id("room"),
                kind: FactKind::Release,
                tool: Some(stopping_tool.to_string()),
                role: None,
                subject: format!("self-release on stop: {}", c.event_id),
                scope: c.scope.clone(),
                created_at: now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: Some(c.event_id.clone()),
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            // D7 audit — same verdict as the sibling test above: the discard is
            // safe because the grading is done on the re-read snapshot, not on
            // the append's return value.
            let _ = room.append_state_transition_verified(&release);
        }

        let snap_after = room.snapshot().unwrap();
        let a_live = snap_after
            .active_claims
            .iter()
            .any(|c| c.event_id == claim_a.event_id);
        let b_live = snap_after
            .active_claims
            .iter()
            .any(|c| c.event_id == claim_b.event_id);

        assert!(!a_live, "stopping session A must release A's own claim");
        assert!(
            b_live,
            "a live sibling session (B) of the SAME tool must NOT be released"
        );

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (f) Heartbeat parity: recency_weight and staleness verdict are tool-agnostic
    // -------------------------------------------------------------------------

    #[test]
    fn heartbeat_parity_vectors_match_expected() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/heartbeat_parity_vectors.json"
        );
        let raw = std::fs::read_to_string(path).expect("read heartbeat parity vectors");
        let v: serde_json::Value =
            serde_json::from_str(&raw).expect("parse heartbeat parity vectors");

        let hl_secs = v["half_life_secs"]
            .as_i64()
            .expect("half_life_secs must be i64");
        let heartbeat_minutes = v["heartbeat_minutes"]
            .as_i64()
            .expect("heartbeat_minutes must be i64");
        let stale_threshold_secs = heartbeat_minutes * 60;

        for (i, case) in v["cases"]
            .as_array()
            .expect("cases array")
            .iter()
            .enumerate()
        {
            let age_secs = case["age_secs"].as_i64().expect("age_secs");
            let expected_weight = case["expected_weight"].as_f64().expect("expected_weight");
            let stale_at_15m = case["stale_at_15m"].as_bool().expect("stale_at_15m");

            let got_weight = crate::decay::recency_weight(age_secs, hl_secs);
            assert!(
                (got_weight - expected_weight).abs() < 1e-4,
                "case {i}: age_secs={age_secs} got weight {got_weight}, expected {expected_weight}"
            );

            // Stale verdict: age > heartbeat threshold. Both tools use the same
            // monotonic wall-clock age, so the verdict must be identical.
            let got_stale = age_secs > stale_threshold_secs;
            assert_eq!(
                got_stale, stale_at_15m,
                "case {i}: age_secs={age_secs} stale verdict mismatch (got {got_stale}, expected {stale_at_15m})"
            );

            // Verify tool_a and tool_b both use the same curve (tool-agnostic).
            let tool_a = case["tool_a"].as_str().unwrap_or("");
            let tool_b = case["tool_b"].as_str().unwrap_or("");
            assert_ne!(
                tool_a, tool_b,
                "case {i}: fixture must name two distinct tools"
            );
            // The weight formula has no tool parameter — the assertion above
            // already proves both tools yield the same result for the same age.
        }
    }

    // -------------------------------------------------------------------------
    // Additional: dry-run does not write facts
    // -------------------------------------------------------------------------

    #[test]
    fn dry_run_does_not_write_facts() {
        let root = unique_root("dry-run");
        let room = RoomStore::open_at(root.clone()).unwrap();

        let ago = 3 * 60 * 60_i64;
        append_presence(&room, "stale-tool", ago);
        let claim = append_claim(&room, "claim-dry", "stale-tool");

        let snap_before = room.snapshot().unwrap();
        let count_before = snap_before.active_claims.len();

        let report = run_reap_stale_in_room(&room, false).unwrap();

        assert_eq!(
            report.claims_reaped.len(),
            1,
            "dry-run must report eligible claim"
        );
        assert!(!report.applied);

        let snap_after = room.snapshot().unwrap();
        assert_eq!(
            snap_after.active_claims.len(),
            count_before,
            "dry-run must not change active_claims"
        );
        let _ = claim;

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // Additional: squads_idle_cleared enumerates stale owners
    // -------------------------------------------------------------------------

    #[test]
    fn squads_idle_cleared_enumerates_stale_owners() {
        let root = unique_root("idle-cleared");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // One stale tool (3h silent) and one fresh tool (1m).
        let stale_ago = 3 * 60 * 60_i64;
        append_presence(&room, "stale-tool", stale_ago);
        append_presence(&room, "fresh-tool", 60);

        let report = run_reap_stale_in_room(&room, false).unwrap();

        let cleared: BTreeSet<_> = report.squads_idle_cleared.iter().cloned().collect();
        assert!(
            cleared.contains("stale-tool"),
            "stale-tool must appear in squads_idle_cleared"
        );
        assert!(
            !cleared.contains("fresh-tool"),
            "fresh-tool must NOT appear in squads_idle_cleared"
        );

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (g) Lease-expired claim with a LIVE owner is reaped (dual-signal fix)
    // -------------------------------------------------------------------------

    /// Append a Claim owned by `tool` with:
    ///   - `created_at` fresh (now) so the owner's squad last_seen_ts stays live
    ///   - `lease_expires_at:<ts>` evidence stamped with the given RFC-3339 string
    ///     Multi-scope so it has parseable ResourceScope entries and appears in active_claims.
    fn append_claim_with_lease(
        room: &RoomStore,
        event_id: &str,
        tool: &str,
        lease_ts: &str,
    ) -> Fact {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}"),
            scope: vec![
                format!("file:src/a_{event_id}.rs"),
                format!("file:src/b_{event_id}.rs"),
            ],
            created_at: now_string(),
            summary: None,
            evidence: vec![format!("lease_expires_at:{lease_ts}")],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap()
    }

    #[test]
    fn lease_expired_claim_with_live_owner_is_reaped() {
        // This is the 76-claim case the old logic missed: the owner squad IS
        // active (fresh presence), but the claim's own lease_expires_at is in
        // the past. The dual-signal fix must reap it via the lease signal.
        let root = unique_root("lease-expired-live-owner");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Owner is fresh (just now) — squad is NOT stale.
        append_presence(&room, "live-owner", 5);

        // Claim has a past lease (1 hour ago).
        let past_lease = past_ts(3600);
        let claim =
            append_claim_with_lease(&room, "claim-lease-expired", "live-owner", &past_lease);

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(
            report.claims_reaped.len(),
            1,
            "lease-expired claim with live owner must be reaped"
        );
        assert_eq!(report.claims_reaped[0].claim_id, claim.event_id);
        assert_eq!(
            report.claims_reaped[0].reason, "lease-expired",
            "reason must be lease-expired when only the lease signal fires"
        );
        assert!(report.applied);

        // The claim must no longer be active.
        let snap = room.snapshot().unwrap();
        assert!(
            !snap
                .active_claims
                .iter()
                .any(|c| c.event_id == claim.event_id),
            "reaped claim must leave active_claims"
        );

        fs::remove_dir_all(&root).ok();
    }

    // -------------------------------------------------------------------------
    // (h) Future-lease with live owner is PRESERVED (9-claim case)
    // -------------------------------------------------------------------------

    #[test]
    fn future_lease_with_live_owner_is_preserved() {
        // The owner is active AND the lease is in the future: neither signal
        // fires → claim must be kept.
        let root = unique_root("future-lease-live-owner");
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Owner is fresh.
        append_presence(&room, "live-owner", 5);

        // Claim has a future lease (1 hour from now).
        let future_lease = {
            use chrono::{SecondsFormat, Utc};
            (Utc::now() + chrono::Duration::seconds(3600))
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let claim =
            append_claim_with_lease(&room, "claim-future-lease", "live-owner", &future_lease);

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(
            report.claims_reaped.len(),
            0,
            "claim with future lease and live owner must NOT be reaped"
        );
        assert_eq!(
            report.preserved_future_or_active, 1,
            "future-lease claim must be counted as preserved"
        );

        // Must still be active.
        let snap = room.snapshot().unwrap();
        assert!(
            snap.active_claims
                .iter()
                .any(|c| c.event_id == claim.event_id),
            "future-lease claim must remain in active_claims"
        );

        fs::remove_dir_all(&root).ok();
    }
}
