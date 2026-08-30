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

use crate::error::{RallyError, Result};
#[cfg(test)]
use crate::new_id;
use crate::store::{AppendOutcome, Fact, FactKind, RoomStore};
use crate::{FACT_SCHEMA, now_string};
use schemars::JsonSchema;
use serde::Serialize;

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

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ReapOutcomeUnknown {
    pub(crate) event_id: String,
    pub(crate) phase: String,
    pub(crate) detail: String,
    pub(crate) remedy: String,
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
    /// Number of inspected items KEPT by POLICY rather than closed: claims with
    /// a future-dated lease, an unparseable owner timestamp, or a live owner;
    /// handoffs inside their TTL or with an unparseable `created_at`; and the
    /// auto-reap per-pass cap.
    ///
    /// A failed durable write is NOT counted here. It used to be, which made a
    /// broken ledger indistinguishable from a healthy one whose owners were all
    /// still working — the count said "kept on purpose" for something nobody
    /// chose to keep (D7).
    pub(crate) preserved_future_or_active: usize,
    /// Eligible items this pass did NOT attempt because its wall-clock budget was
    /// spent. Zero means the pass reached everything it judged eligible.
    ///
    /// Non-zero is not an error: run the command again. The reaper is
    /// idempotent, and each pass starts from a fresh projection.
    #[serde(default)]
    pub(crate) remaining: usize,
    /// Durable writes this apply pass attempted, whether they landed or failed.
    /// Zero for dry runs and apply passes with no eligible work.
    #[serde(default)]
    pub(crate) attempted_writes: usize,
    /// Items whose durable append FAILED: a `ClaimExpired`, a handoff `Resolve`,
    /// or the lead relinquish that did not reach the ledger.
    ///
    /// Non-zero means the room is not in the state this report describes. The
    /// only previous signal was a stderr line nothing parses.
    #[serde(default)]
    pub(crate) write_failures: usize,
    /// Canonical commits, including any projection degradation. These are also
    /// registered with the command-wide aggregate so CLI JSON cannot hide them.
    #[serde(skip)]
    #[schemars(skip)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) append_outcomes: Vec<AppendOutcome>,
    /// Pre-readback uncertainty kept structured with its stable query remedy.
    #[serde(default)]
    pub(crate) outcome_unknowns: Vec<ReapOutcomeUnknown>,
    /// At least one write was attempted and every attempted write landed.
    ///
    /// This used to be a copy of the `--apply` argument, so `rally doctor
    /// --reap-stale --apply --json` answered `applied: true` and `ok: true`
    /// against a fully unwritable ledger (D7). `attempted_writes` distinguishes
    /// a no-op from an attempted pass, and `complete` distinguishes a partial
    /// budgeted pass from a fully drained one.
    pub(crate) applied: bool,
    /// The apply pass reached every eligible action and every attempted write
    /// landed. False for dry runs, partial passes, and write failures.
    #[serde(default)]
    pub(crate) complete: bool,
}

fn stable_reaper_event_id(action: &str, target: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in action.bytes().chain([0]).chain(target.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("reaper-{action}-{hash:016x}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaperAppendDisposition {
    Committed,
    OutcomeUnknown,
}

fn record_reaper_result(
    result: Result<AppendOutcome>,
    append_outcomes: &mut Vec<AppendOutcome>,
    outcome_unknowns: &mut Vec<ReapOutcomeUnknown>,
) -> std::result::Result<ReaperAppendDisposition, RallyError> {
    match result {
        Ok(outcome) => {
            crate::record_append_outcome(&outcome);
            append_outcomes.push(outcome);
            Ok(ReaperAppendDisposition::Committed)
        }
        Err(RallyError::OutcomeUnknown {
            event_id,
            phase,
            detail,
        }) => {
            outcome_unknowns.push(ReapOutcomeUnknown {
                remedy: crate::locate_remedy(&event_id),
                event_id,
                phase,
                detail,
            });
            Ok(ReaperAppendDisposition::OutcomeUnknown)
        }
        Err(error) => Err(error),
    }
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
    // `rally doctor --reap-stale` has no entering agent, so the honest actor is
    // the invoking `rally` process. It keeps `tool: "rally"` but now carries a
    // real session lease, so two operators reaping one room are distinguishable
    // in the audit trail.
    run_reap_stale_in_room_as(&room, apply, &crate::SystemActor::invoking_process())
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
/// 2. **It closed LIVE agents' claims.** At the time it shipped, nothing in
///    production renewed `lease_expires_at`, so every single-file claim expired
///    30 minutes after it was made and every coarse claim after 2 hours,
///    regardless of whether the owner was working. Presence/status heartbeats
///    now durably renew owned claims; observed-liveness safety remains a
///    separate gate before changing the default.
///    Measured: an active owner's claim was closed by a PEER's `enter`, the
///    owner was never told, and a third agent then claimed the same file with
///    no conflict. That is the collision Rally exists to prevent.
/// 3. **RC-044** already records concurrent `rally enter` as an unfixed
///    store-corruption path. Adding a mutation-heavy pass to it widened a known
///    defect without a control.
///
/// Durable renewal and observed-dead corroboration now make the unlocked
/// decision session-specific. The call site stays opt-in because the store's
/// under-lock recheck must receive the same session key after this lane merges,
/// legacy claims have no session-specific observer stamp, and storage/transport
/// scale gates remain open. Single-flight and bounds are necessary rather than
/// sufficient for a default flip.
pub(crate) const DEFAULT_AUTO_REAP_INTERVAL_SECS: i64 = 0;

/// Marker recording the last auto-reap, relative to `.rally/`.
const AUTO_REAP_MARKER: &str = ".last-auto-reap";
const AUTO_REAP_FLIGHT: &str = ".auto-reap-in-flight";

/// Atomically admit one automatic pass across concurrent processes.
///
/// The same non-blocking kernel advisory lock primitive used by store
/// ownership admits exactly one entrant. The kernel releases it on normal
/// drop and process death; the lock file may remain forever without blocking a
/// later pass. Platforms without this primitive fail closed and skip auto-reap.
fn try_auto_reap_flight(room: &RoomStore) -> Option<crate::store::OwnerGuard> {
    let rally_dir = room.repo_root().join(".rally");
    match crate::store::acquire_named_exclusive_nb(&rally_dir, AUTO_REAP_FLIGHT) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => None,
        Err(err) => {
            eprintln!(
                "rally: auto-reap single-flight unavailable ({}): {err}",
                rally_dir.join(AUTO_REAP_FLIGHT).display()
            );
            None
        }
    }
}

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
/// - **Rate-limited and single-flight by files outside the ledger.**
///   The marker is `.rally/.last-auto-reap`; a missing or unparseable marker
///   reaps (fail-toward-cleanup, since the reaper's own eligibility math is
///   fail-closed). `.auto-reap-in-flight` uses a non-blocking kernel advisory
///   lock so overlapping enters cannot each run a pass and crashed holders
///   recover automatically.
/// - **Opt-out.** `RALLY_NO_AUTO_REAP=1`, or `auto_reap_interval_secs: 0`.
///
/// Returns the report when a reap ran, `None` when it was skipped.
pub(crate) fn maybe_reap_on_enter(room: &RoomStore, entering_tool: &str) -> Option<ReapReport> {
    if std::env::var("RALLY_NO_AUTO_REAP").is_ok_and(|v| v == "1") {
        return None;
    }
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();
    let interval = coord.auto_reap_interval_secs;
    if interval <= 0 {
        return None;
    }

    let _flight = try_auto_reap_flight(room)?;

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

    // Stamp the interval BEFORE reaping while the cross-process flight token is
    // held. Concurrent enters cannot pass this marker check-and-set together.
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

    // The agent whose `rally enter` triggered this pass is the actor. It is
    // the party that benefits from the reap and the party a reaped claim's
    // former owner would need to contact, so it is the party the ledger must
    // name.
    match run_reap_stale_in_room_with_mode_as(
        room,
        true,
        ReapMode::LeaseOnly,
        &crate::SystemActor::for_tool(entering_tool),
    ) {
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
/// - **Lease expiry** is stamped by the claim's OWN writer and advanced by its
///   self-authored heartbeat. The append boundary re-checks the effective lease
///   under lock, so a reap computed before a concurrent renewal is refused.
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
/// So the automatic path requires BOTH writer-stamped lease expiry and an
/// external observed-dead verdict. Owner-staleness stays behind the deliberate
/// operator command. Unknown observer evidence never authorizes auto-removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReapMode {
    /// Act on both signals. `rally doctor --reap-stale --apply` — a human ran it.
    Full,
    /// Act only on writer-stamped lease expiry corroborated by observed death.
    /// The automatic `enter` path.
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

fn auto_candidate_limit_reached(mode: ReapMode, attempted_actions: usize) -> bool {
    mode == ReapMode::LeaseOnly && attempted_actions >= AUTO_REAP_MAX_FACTS
}

/// Wall-clock budget one `--apply` pass may spend appending, in milliseconds.
///
/// # Why a budget rather than a bigger timeout
///
/// One verified append re-reads the whole ledger four times — `segment_seq_stats`
/// and `read_db_event_stats` (both inside the post-append sidecar refresh),
/// `refresh_log_index`, and the claim-index fold — and design audit D10
/// (RC-058) counts the rest. Measured on a synthetic ledger the size of this
/// repo's own (6,563 facts, 63 expired claims): 40.6 s before this run's cost
/// cuts, 29.5 s after. The mutation watchdog allows 3 s by default, so a full
/// drain COULD NOT COMPLETE, and the cleanup that would shrink the working set
/// was blocked by the size of the working set.
///
/// Raising the timeout to fit would hide that. Instead the pass stops when its
/// budget is spent and REPORTS what it did not reach, so every invocation
/// finishes and the operator can see the queue draining. The reaper is
/// idempotent, so a partial pass is a shorter pass, not a broken one — the same
/// argument [`AUTO_REAP_MAX_FACTS`] already makes for the automatic path.
///
/// The per-append cost is a separate open entry. This makes the tool usable; it
/// does not make it cheap.
const DEFAULT_REAP_APPLY_BUDGET_MS: u64 = 2_000;

/// Resolve the apply budget. `RALLY_REAP_BUDGET_MS` raises it for a deliberate
/// bulk drain (pair it with `--timeout-ms`); `0` disables the budget entirely,
/// which is the old unbounded behaviour and is available on purpose.
fn reap_apply_budget() -> Option<std::time::Duration> {
    let ms = std::env::var("RALLY_REAP_BUDGET_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_REAP_APPLY_BUDGET_MS);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Budget clock for one reap pass. Production reads monotonic wall time. Debug
/// integration tests may inject a logical millisecond step per attempted action
/// so queue partitioning is deterministic on a loaded runner.
struct ReapBudgetClock {
    started: std::time::Instant,
    #[cfg(debug_assertions)]
    logical_step_ms: Option<u64>,
}

impl ReapBudgetClock {
    fn start() -> Self {
        Self {
            started: std::time::Instant::now(),
            #[cfg(debug_assertions)]
            logical_step_ms: std::env::var("RALLY_TEST_REAP_CLOCK_STEP_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok()),
        }
    }

    fn elapsed(&self, _attempted_actions: usize) -> std::time::Duration {
        #[cfg(debug_assertions)]
        if let Some(step_ms) = self.logical_step_ms {
            return std::time::Duration::from_millis(
                step_ms.saturating_mul(_attempted_actions as u64),
            );
        }
        self.started.elapsed()
    }
}

/// Inner implementation — takes an explicit `&RoomStore` so tests can inject a
/// temp store without touching the process-global cwd.
/// Test-only shim: reap with the invoking process as the actor.
///
/// `#[cfg(test)]` is the guardrail, not an artefact of dead-code warnings. Every
/// production entry point must name its actor, because the one that did not —
/// `maybe_reap_on_enter`, which had the entering agent in its caller and never
/// asked for it — is precisely how the claim-takeover audit trail ended up
/// unable to say who reaped a claim. A new production caller that wants the
/// implicit actor now has to fail to compile first.
#[cfg(test)]
pub(crate) fn run_reap_stale_in_room(room: &RoomStore, apply: bool) -> Result<ReapReport> {
    run_reap_stale_in_room_as(room, apply, &crate::SystemActor::invoking_process())
}

/// As [`run_reap_stale_in_room`], with the acting identity supplied by the
/// caller. Every reaper fact this pass writes is attributed to `actor`.
pub(crate) fn run_reap_stale_in_room_as(
    room: &RoomStore,
    apply: bool,
    actor: &crate::SystemActor,
) -> Result<ReapReport> {
    run_reap_stale_in_room_with_mode_as(room, apply, ReapMode::Full, actor)
}

/// Test-only shim. See [`run_reap_stale_in_room`] for why this is gated.
#[cfg(test)]
pub(crate) fn run_reap_stale_in_room_with_mode(
    room: &RoomStore,
    apply: bool,
    mode: ReapMode,
) -> Result<ReapReport> {
    run_reap_stale_in_room_with_mode_as(room, apply, mode, &crate::SystemActor::invoking_process())
}

pub(crate) fn run_reap_stale_in_room_with_mode_as(
    room: &RoomStore,
    apply: bool,
    mode: ReapMode,
    actor: &crate::SystemActor,
) -> Result<ReapReport> {
    let budget = apply.then(reap_apply_budget).flatten();
    run_reap_stale_in_room_with_budget(room, apply, mode, budget, actor)
}

fn run_reap_stale_in_room_with_budget(
    room: &RoomStore,
    apply: bool,
    mode: ReapMode,
    budget: Option<std::time::Duration>,
    actor: &crate::SystemActor,
) -> Result<ReapReport> {
    // The clock starts BEFORE the projection, not before the append loop. The
    // opening `snapshot()` is a full ledger read and is the same cost the
    // watchdog is measuring; a budget that ignored it would be a budget for the
    // cheap half of the pass.
    let budget_clock = ReapBudgetClock::start();
    let snapshot = room.snapshot()?;
    let coord = crate::hooks_config::resolve_coordination(room.repo_root()).unwrap_or_default();

    let mut claims_reaped: Vec<ReapedClaim> = Vec::new();
    let mut preserved: usize = 0;
    let mut write_failures: usize = 0;
    let mut remaining: usize = 0;
    let mut append_outcomes = Vec::new();
    let mut outcome_unknowns = Vec::new();
    // One counter governs the whole queue, independently of whether an append
    // succeeds. Action zero is the global forward-progress floor; every later
    // claim, handoff, or lead action observes the same elapsed-time budget.
    let mut attempted_actions: usize = 0;

    // Identify the legacy stale-owner set at snapshot time (squad-level, 2h
    // bar). Lead relinquish still uses this tool-scoped projection. Sessionful
    // claim eligibility below instead requires the exact observed session.
    let stale_owners = snapshot.takeover_eligible_owners();

    // Compute the lease-expired claim_id set: claims whose OWN lease timestamp
    // has provably passed NOW, regardless of owner-squad liveness.
    // fail-closed: expired_claims only includes claims with a parseable
    // lease_expires_at <= now; unparseable or missing lease → not included.
    let facts = room.facts()?;
    let observed_sessions = crate::observed_liveness::observe_sessions(room.repo_root(), &facts);
    let claim_index = crate::claim_authority::index_from_facts(&facts);
    let lease_expired_ids: std::collections::BTreeSet<String> =
        crate::claim_authority::expired_claims(&claim_index, &facts, chrono::Utc::now())
            .into_iter()
            .map(|r| r.claim_id)
            .collect();

    // --- Evaluate each active claim ---
    for claim in &snapshot.active_claims {
        let (legacy_owner_eligible, _size) = snapshot.claim_reclaim_eligible(claim, &coord);
        let lease_eligible = lease_expired_ids.contains(&claim.event_id);
        let lease_boundary = claim
            .evidence
            .iter()
            .find_map(|item| item.strip_prefix("lease_expires_at:"))
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|time| time.with_timezone(&chrono::Utc));
        let observed = if lease_eligible {
            observed_sessions.for_claim_since(
                claim.tool.as_deref(),
                claim.from_session_id.as_deref(),
                lease_boundary,
            )
        } else {
            observed_sessions.for_claim(claim.tool.as_deref(), claim.from_session_id.as_deref())
        };
        // A sessionful claim never falls back to tool-wide activity. Exact
        // observed death is its owner-stale authority; Unknown stays closed.
        // Only legacy claims without a session id retain the historical squad
        // timestamp predicate because no more precise owner exists for them.
        let owner_eligible = if claim.from_session_id.is_some() {
            observed == crate::observed_liveness::ObservedLiveness::Stale
        } else {
            legacy_owner_eligible
        };
        let observed_verdict = crate::liveness::is_live(
            &crate::liveness::LivenessSignals {
                observed_alive: observed.as_signal(),
                ..Default::default()
            },
            0,
        );

        // External live evidence is a veto in both automatic and explicit
        // modes. The automatic enter path additionally requires observed-dead
        // corroboration; Unknown remains fail-closed. A human-invoked Full pass
        // retains the legacy ability to clean old, unstamped ledgers.
        if observed_verdict == crate::liveness::Liveness::Live {
            preserved += 1;
            continue;
        }

        // A claim is reaped when EITHER its owner is provably stale OR its own
        // lease has provably expired. Sessionful owners require exact observed
        // death; only sessionless legacy owners use squad age. Both signals are
        // fail-closed.
        //
        // ReapMode::LeaseOnly drops the owner-stale signal. `last_seen_ts` is
        // the `created_at` of the highest-seq fact naming that tool, written
        // verbatim from a git-tracked ledger line, so one committed fact
        // carrying a victim's id and a backdated timestamp makes the victim
        // look stale to every reader. The lease is stamped by the claim's own
        // writer and durably renewed by its heartbeat. The automatic mode also
        // requires the external observed-dead verdict computed above.
        let eligible = match mode {
            ReapMode::Full => owner_eligible || lease_eligible,
            ReapMode::LeaseOnly => {
                lease_eligible && observed_verdict == crate::liveness::Liveness::Stale
            }
        };
        if !eligible {
            preserved += 1;
            continue;
        }
        if auto_candidate_limit_reached(mode, attempted_actions) {
            remaining += 1;
            continue;
        }
        // Automatic enter cleanup never starts a destructive action after its
        // budget is spent. A deliberate Full pass retains the historical
        // one-action progress floor so a slow ledger can still be drained.
        if budget.is_some_and(|b| budget_clock.elapsed(attempted_actions) >= b)
            && (mode == ReapMode::LeaseOnly || attempted_actions > 0)
        {
            remaining += 1;
            continue;
        }
        attempted_actions += 1;

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
            let action_target = claim.event_id.as_str();
            let expired_fact = Fact {
                // This is the claim-takeover audit trail. Naming the reaper is
                // the whole point of it: 98.6% of `claim.expired` facts on disk
                // say only "rally", which is unusable in a contested-ownership
                // dispute. The claim's FORMER owner is already recorded in
                // `evidence` (`reaper:owner=` / `reaper:owner_session=`); this
                // records the party that closed it.
                from_session_id: actor.session_field(),
                schema: FACT_SCHEMA.to_string(),
                event_id: stable_reaper_event_id("claim-expired", action_target),
                seq: 0,
                thread_id: stable_reaper_event_id("claim-thread", action_target),
                kind: FactKind::ClaimExpired,
                tool: actor.tool_field(),
                role: actor.role_field(),
                subject: format!(
                    "reaper: claim {} expired (reason={}, owner: {})",
                    claim.event_id,
                    reason,
                    claim.tool.as_deref().unwrap_or("unknown")
                ),
                scope: claim.scope.clone(),
                created_at: now_string(),
                summary: Some(format!("reaper:reason={}", reaped.reason)),
                // Stamp the reap reasons onto evidence so the append boundary
                // can re-check owner age, effective durable lease, and an
                // observed-stale verdict under the mutation lock. The owner is
                // explicit so no subject parsing participates in authority.
                evidence: vec![
                    format!("reaper:ref_id={}", claim.event_id),
                    format!("reaper:reason={}", reaped.reason),
                    format!("reaper:observed={}", observed.as_str()),
                    format!(
                        "reaper:owner={}",
                        claim.tool.as_deref().unwrap_or("unknown")
                    ),
                    format!(
                        "reaper:owner_session={}",
                        claim.from_session_id.as_deref().unwrap_or("legacy")
                    ),
                ],
                target: None,
                ref_id: Some(claim.event_id.clone()),
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            // The action id and payload are derived from the claim, so an
            // uncertain first attempt can be resolved or retried without
            // minting a second close event.
            match record_reaper_result(
                room.append_fact_verified(&expired_fact),
                &mut append_outcomes,
                &mut outcome_unknowns,
            ) {
                Ok(ReaperAppendDisposition::Committed) => {}
                Ok(ReaperAppendDisposition::OutcomeUnknown) => {
                    write_failures += 1;
                    continue;
                }
                Err(e) => {
                    // Log but do not abort the whole reap run.
                    eprintln!(
                        "reaper: skipping {} (already closed or lock error): {}",
                        claim.event_id, e
                    );
                    write_failures += 1;
                    continue;
                }
            }
        }

        claims_reaped.push(reaped);
    }

    // Automatic enter cleanup has exactly one destructive responsibility:
    // close claims whose writer-stamped lease expired and whose exact owner
    // session is externally observed dead. Handoff expiry and lead relinquish
    // remain deliberate operator actions in Full mode; they have different
    // authority signals and must not consume this pass's cap or time budget.
    if mode == ReapMode::LeaseOnly {
        return Ok(ReapReport {
            claims_reaped,
            handoffs_expired: Vec::new(),
            squads_idle_cleared: Vec::new(),
            lead_relinquished: None,
            preserved_future_or_active: preserved,
            remaining,
            attempted_writes: if apply { attempted_actions } else { 0 },
            write_failures,
            append_outcomes,
            outcome_unknowns,
            applied: apply && attempted_actions > 0 && write_failures == 0,
            complete: apply && remaining == 0 && write_failures == 0,
        });
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
            if auto_candidate_limit_reached(mode, attempted_actions) {
                remaining += 1;
                continue;
            }
            if budget.is_some_and(|b| budget_clock.elapsed(attempted_actions) >= b)
                && (mode == ReapMode::LeaseOnly || attempted_actions > 0)
            {
                remaining += 1;
                continue;
            }
            attempted_actions += 1;

            let reaped = ReapedHandoff {
                handoff_id: handoff.event_id.clone(),
                from_tool: handoff.tool.clone().unwrap_or_default(),
                target: handoff.target.clone(),
                created_at: handoff.created_at.clone(),
                age_days: age_secs / 86_400,
            };

            if apply {
                let action_target = handoff.event_id.as_str();
                let expiry_fact = Fact {
                    from_session_id: actor.session_field(),
                    schema: FACT_SCHEMA.to_string(),
                    event_id: stable_reaper_event_id("handoff-expired", action_target),
                    seq: 0,
                    thread_id: stable_reaper_event_id("handoff-thread", action_target),
                    kind: FactKind::Resolve,
                    tool: actor.tool_field(),
                    role: actor.role_field(),
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
                match record_reaper_result(
                    room.append_fact_verified(&expiry_fact),
                    &mut append_outcomes,
                    &mut outcome_unknowns,
                ) {
                    Ok(ReaperAppendDisposition::Committed) => {}
                    Ok(ReaperAppendDisposition::OutcomeUnknown) => {
                        write_failures += 1;
                        continue;
                    }
                    Err(e) => {
                        eprintln!(
                            "reaper: skipping handoff {} (already closed or lock error): {}",
                            handoff.event_id, e
                        );
                        write_failures += 1;
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
            let budget_spent = budget.is_some_and(|b| budget_clock.elapsed(attempted_actions) >= b)
                && (mode == ReapMode::LeaseOnly || attempted_actions > 0);
            if auto_candidate_limit_reached(mode, attempted_actions) || budget_spent {
                remaining += 1;
                None
            } else {
                attempted_actions += 1;
                let mut relinquish_committed = true;
                if apply {
                    let lead_epoch = snapshot.lead_epoch.unwrap_or_default();
                    let action_target = format!("{lead_tool}:{lead_epoch}");
                    let relinquish_fact = Fact {
                        // A lead seat changing hands with no named party is the
                        // least diagnosable event in the room. The stale lead is
                        // in `evidence`; the actor that took the seat away is
                        // here.
                        from_session_id: actor.session_field(),
                        schema: FACT_SCHEMA.to_string(),
                        event_id: stable_reaper_event_id("lead-relinquished", &action_target),
                        seq: 0,
                        thread_id: stable_reaper_event_id("lead-thread", &action_target),
                        kind: FactKind::Decision,
                        tool: actor.tool_field(),
                        role: actor.role_field(),
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
                    match record_reaper_result(
                        room.append_fact_verified(&relinquish_fact),
                        &mut append_outcomes,
                        &mut outcome_unknowns,
                    ) {
                        Ok(ReaperAppendDisposition::Committed) => {}
                        Ok(ReaperAppendDisposition::OutcomeUnknown) => {
                            write_failures += 1;
                            relinquish_committed = false;
                        }
                        Err(e) => {
                            eprintln!(
                                "reaper: keeping lead {lead_tool} (relinquish append failed): {e}"
                            );
                            write_failures += 1;
                            relinquish_committed = false;
                        }
                    }
                }
                // Dry-run keeps `relinquish_committed = true`: nothing was
                // written, and the report already says so via `applied: false`.
                if relinquish_committed {
                    Some(lead_tool.clone())
                } else {
                    None
                }
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
        remaining,
        attempted_writes: if apply { attempted_actions } else { 0 },
        write_failures,
        append_outcomes,
        outcome_unknowns,
        applied: apply && attempted_actions > 0 && write_failures == 0,
        complete: apply && remaining == 0 && write_failures == 0,
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    /// The acting identity these unit tests reap as. Named rather than
    /// defaulted so a test asserting attribution reads against a value it can
    /// see, and so the production paths keep their required-actor contract.
    fn test_actor() -> crate::SystemActor {
        crate::SystemActor::invoking_process()
    }

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

    fn init_observed_worktree(root: &std::path::Path) {
        crate::test_git_fixture::fixture_git(root, &["init"]);
        fs::write(root.join("observed.txt"), "observed\n").unwrap();
        crate::test_git_fixture::fixture_git(root, &["add", "observed.txt"]);
        crate::test_git_fixture::fixture_git(root, &["commit", "-m", "observed fixture"]);
    }

    fn append_observed_presence(
        room: &RoomStore,
        tool: &str,
        worktree: &std::path::Path,
        pid: u32,
    ) {
        append_observed_presence_for_session(
            room,
            tool,
            &format!("sess:test:{tool}"),
            worktree,
            pid,
        );
    }

    fn append_observed_presence_for_session(
        room: &RoomStore,
        tool: &str,
        from_session_id: &str,
        worktree: &std::path::Path,
        pid: u32,
    ) {
        let head = crate::observed_liveness::current_head_sha(worktree).expect("fixture HEAD");
        let worktree = fs::canonicalize(worktree).unwrap();
        let fact = Fact {
            from_session_id: Some(from_session_id.to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("observed presence: {tool}"),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: vec![
                format!("branch_head_sha:{head}"),
                format!("worktree_path:{}", worktree.display()),
                format!("observer_pid:{pid}"),
            ],
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
        room.append_fact_verified(&fact).unwrap().fact
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
        room.append_fact_verified(&fact).unwrap().fact
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
        room.append_fact_verified(&fact).unwrap().fact
    }

    fn append_lead(room: &RoomStore, tool: &str, ago_secs: i64) -> Fact {
        let fact = Fact {
            from_session_id: None,
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Decision,
            tool: Some(tool.to_string()),
            role: None,
            subject: "role:lead".to_string(),
            scope: Vec::new(),
            created_at: past_ts(ago_secs),
            summary: Some(format!("{tool} is lead")),
            evidence: vec!["assigned:test".to_string()],
            target: Some(tool.to_string()),
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fact).unwrap().fact
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

    /// A zero-duration budget models a projection that has already consumed
    /// the whole allowance. The first eligible handoff must still land; later
    /// passes must keep shrinking the queue instead of reporting the same
    /// `remaining` count forever.
    #[test]
    fn zero_budget_handoff_only_queue_makes_global_progress() {
        let root = unique_root("handoff-budget-floor");
        let room = RoomStore::open_at(root.clone()).unwrap();
        let ago = DEFAULT_HANDOFF_EXPIRY_SECS + 60;
        append_handoff(&room, "handoff-budget-a", "author-a", ago);
        append_handoff(&room, "handoff-budget-b", "author-b", ago);

        let first = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(first.handoffs_expired.len(), 1);
        assert_eq!(first.attempted_writes, 1);
        assert_eq!(first.remaining, 1);
        assert!(first.applied);
        assert!(!first.complete);

        let second = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(second.handoffs_expired.len(), 1);
        assert_eq!(second.attempted_writes, 1);
        assert_eq!(second.remaining, 0);
        assert!(second.applied);
        assert!(second.complete);
        assert!(room.snapshot().unwrap().open_handoffs.is_empty());

        let no_op = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(no_op.attempted_writes, 0);
        assert!(!no_op.applied, "a no-op did not apply a durable write");
        assert!(no_op.complete, "a no-op queue is fully drained");

        fs::remove_dir_all(root).ok();
    }

    /// Claims, handoffs, and lead cleanup share one budget. A zero budget must
    /// not reset its progress floor for each category and accidentally perform
    /// three unbounded appends in one pass.
    #[test]
    fn zero_budget_is_global_across_claim_handoff_and_lead() {
        let root = unique_root("mixed-budget-floor");
        let room = RoomStore::open_at(root.clone()).unwrap();
        let stale = 3 * 60 * 60;
        append_presence(&room, "stale-owner", stale);
        append_claim(&room, "claim-mixed-budget", "stale-owner");
        append_handoff(
            &room,
            "handoff-mixed-budget",
            "handoff-author",
            DEFAULT_HANDOFF_EXPIRY_SECS + 60,
        );
        append_lead(&room, "stale-owner", stale);

        let first = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(first.claims_reaped.len(), 1);
        assert!(first.handoffs_expired.is_empty());
        assert!(first.lead_relinquished.is_none());
        assert_eq!(first.attempted_writes, 1);
        assert_eq!(first.remaining, 2);
        assert!(!first.complete);

        let second = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(second.handoffs_expired.len(), 1);
        assert!(second.lead_relinquished.is_none());
        assert_eq!(second.attempted_writes, 1);
        assert_eq!(second.remaining, 1);

        let third = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(third.lead_relinquished.as_deref(), Some("stale-owner"));
        assert_eq!(third.attempted_writes, 1);
        assert_eq!(third.remaining, 0);
        assert!(third.complete);
        assert!(room.snapshot().unwrap().lead.is_none());

        fs::remove_dir_all(root).ok();
    }

    /// Lead-only rooms get the same first-action floor as claim-only and
    /// handoff-only rooms.
    #[test]
    fn zero_budget_lead_only_queue_makes_progress() {
        let root = unique_root("lead-budget-floor");
        let room = RoomStore::open_at(root.clone()).unwrap();
        let stale = 3 * 60 * 60;
        append_presence(&room, "stale-lead", stale);
        append_lead(&room, "stale-lead", stale);

        let report = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::Full,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(report.lead_relinquished.as_deref(), Some("stale-lead"));
        assert_eq!(report.attempted_writes, 1);
        assert_eq!(report.remaining, 0);
        assert!(report.applied);
        assert!(report.complete);

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
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();

        // LEASE-expired, not owner-stale: the automatic path deliberately acts
        // only on the writer-stamped signal (see `ReapMode`), because
        // owner-staleness is derived from a peer-writable timestamp.
        append_observed_presence(&room, "owner", &root, 2_000_000_000);
        append_claim_with_lease(&room, "claim-leased", "owner", &past_ts(60 * 60));

        let first = maybe_reap_on_enter(&room, "test-agent").expect("first enter must reap");
        assert_eq!(first.claims_reaped.len(), 1);

        // Second call inside the interval is skipped entirely — ten agents
        // entering at once must not each run a reap pass.
        append_claim_with_lease(&room, "claim-leased-2", "owner", &past_ts(60 * 60));
        assert!(
            maybe_reap_on_enter(&room, "test-agent").is_none(),
            "a second enter inside the interval must not reap again"
        );

        unsafe { std::env::remove_var("RALLY_AUTO_REAP_INTERVAL_SECS") };
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn concurrent_auto_reap_admission_is_single_flight() {
        let root = unique_root("auto-reap-single-flight");
        let contenders = 12;
        let start = std::sync::Arc::new(std::sync::Barrier::new(contenders));
        let hold = std::sync::Arc::new(std::sync::Barrier::new(contenders));
        let mut joins = Vec::new();
        for _ in 0..contenders {
            let root = root.clone();
            let start = start.clone();
            let hold = hold.clone();
            joins.push(std::thread::spawn(move || {
                let room = RoomStore::open_at(root).unwrap();
                start.wait();
                let flight = try_auto_reap_flight(&room);
                // Keep the winner's advisory lock alive until every contender
                // has attempted admission; this grades overlapping enters,
                // not sequential reuse after a completed pass.
                hold.wait();
                flight.is_some()
            }));
        }
        let admitted = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1, "exactly one overlapping enter may reap");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stale_auto_reap_lock_file_does_not_block_recovery() {
        let root = unique_root("auto-reap-crash-recovery");
        let room = RoomStore::open_at(root.clone()).unwrap();
        let lock_path = root.join(".rally").join(AUTO_REAP_FLIGHT);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, "left behind by a crashed process\n").unwrap();

        let flight = try_auto_reap_flight(&room);
        assert!(
            flight.is_some(),
            "file existence must not masquerade as a live holder after a crash"
        );
        drop(flight);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn automatic_candidate_limit_counts_attempts_not_successes() {
        assert!(!auto_candidate_limit_reached(
            ReapMode::LeaseOnly,
            AUTO_REAP_MAX_FACTS - 1
        ));
        assert!(auto_candidate_limit_reached(
            ReapMode::LeaseOnly,
            AUTO_REAP_MAX_FACTS
        ));
        assert!(
            !auto_candidate_limit_reached(ReapMode::Full, usize::MAX),
            "the operator-invoked mode is governed by its wall-clock budget"
        );
    }

    #[test]
    fn automatic_pass_starts_no_action_after_budget_is_spent() {
        let root = unique_root("auto-reap-zero-budget");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();
        append_observed_presence(&room, "owner", &root, 2_000_000_000);
        let claim = append_claim_with_lease(&room, "claim-zero-budget", "owner", &past_ts(3600));

        let report = run_reap_stale_in_room_with_budget(
            &room,
            true,
            ReapMode::LeaseOnly,
            Some(std::time::Duration::ZERO),
            &test_actor(),
        )
        .unwrap();
        assert_eq!(report.attempted_writes, 0);
        assert_eq!(report.remaining, 1);
        assert!(
            room.snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|active| active.event_id == claim.event_id),
            "budget exhaustion must preserve the claim"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn automatic_pass_preserves_quiet_live_agent_and_reaps_crashed_agent() {
        let root = unique_root("observed-live-and-crashed");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_observed_presence(&room, "quiet-live", &root, std::process::id());
        let live_claim =
            append_claim_with_lease(&room, "claim-quiet-live", "quiet-live", &past_ts(60 * 60));
        append_observed_presence(&room, "crashed", &root, 2_000_000_000);
        let crashed_claim =
            append_claim_with_lease(&room, "claim-crashed", "crashed", &past_ts(60 * 60));

        let report = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();

        assert_eq!(report.claims_reaped.len(), 1);
        assert_eq!(report.claims_reaped[0].claim_id, crashed_claim.event_id);
        let active = room.snapshot().unwrap().active_claims;
        assert!(
            active
                .iter()
                .any(|claim| claim.event_id == live_claim.event_id)
        );
        assert!(
            !active
                .iter()
                .any(|claim| claim.event_id == crashed_claim.event_id)
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn automatic_pass_only_processes_observer_eligible_expired_claims() {
        let root = unique_root("automatic-claims-only");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_observed_presence(&room, "crashed", &root, 2_000_000_000);
        let claim = append_claim_with_lease(&room, "claim-crashed-only", "crashed", &past_ts(3600));
        let handoff = append_handoff(
            &room,
            "handoff-automatic-must-ignore",
            "author",
            DEFAULT_HANDOFF_EXPIRY_SECS + 60,
        );
        let stale = 3 * 60 * 60;
        append_presence(&room, "stale-lead", stale);
        append_lead(&room, "stale-lead", stale);

        let report = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();

        assert_eq!(
            report
                .claims_reaped
                .iter()
                .map(|entry| entry.claim_id.as_str())
                .collect::<Vec<_>>(),
            vec![claim.event_id.as_str()]
        );
        assert!(
            report.handoffs_expired.is_empty(),
            "automatic mode must never expire handoffs"
        );
        assert!(
            report.lead_relinquished.is_none(),
            "automatic mode must never relinquish the lead"
        );
        assert_eq!(
            report.attempted_writes, 1,
            "only the observer-eligible expired claim may consume the automatic budget"
        );
        let snapshot = room.snapshot().unwrap();
        assert!(
            snapshot
                .open_handoffs
                .iter()
                .any(|open| open.event_id == handoff.event_id)
        );
        assert_eq!(snapshot.lead.as_deref(), Some("stale-lead"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn live_same_tool_sibling_does_not_protect_dead_claim_owner() {
        let root = unique_root("session-observed-sibling");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();

        append_observed_presence_for_session(
            &room,
            "shared-tool",
            "session-dead",
            &root,
            2_000_000_000,
        );
        append_observed_presence_for_session(
            &room,
            "shared-tool",
            "session-live",
            &root,
            std::process::id(),
        );
        let dead_claim = append_session_claim_with_lease(
            &room,
            "claim-dead-session",
            "shared-tool",
            "session-dead",
            &past_ts(3600),
        );
        let live_claim = append_session_claim_with_lease(
            &room,
            "claim-live-session",
            "shared-tool",
            "session-live",
            &past_ts(3600),
        );

        let report = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();
        assert_eq!(
            report
                .claims_reaped
                .iter()
                .map(|claim| claim.claim_id.as_str())
                .collect::<Vec<_>>(),
            vec![dead_claim.event_id.as_str()],
            "the exact dead session is eligible even while its sibling is live"
        );
        assert!(
            report
                .claims_reaped
                .iter()
                .all(|claim| claim.claim_id != live_claim.event_id),
            "the live sibling's own claim must survive"
        );
        let active = room.snapshot().unwrap().active_claims;
        assert!(
            active
                .iter()
                .all(|claim| claim.event_id != dead_claim.event_id),
            "the exact dead session must close under the store mutation lock"
        );
        assert!(
            active
                .iter()
                .any(|claim| claim.event_id == live_claim.event_id),
            "the live sibling must remain active after the same applied pass"
        );
        fs::remove_dir_all(root).ok();
    }

    /// The automatic reap decision is THREE-way on the external observation,
    /// and the third arm is the one nothing else pinned.
    /// [`crate::observed_liveness::ObservedLiveness`] reaches
    /// [`crate::liveness::is_live`] as `observed_alive`, so the arms are:
    ///
    /// | observation           | `observed_alive` | `ReapMode::LeaseOnly` |
    /// |-----------------------|------------------|-----------------------|
    /// | observed live         | `Some(true)`     | preserved (veto)      |
    /// | observed dead         | `Some(false)`    | REAPED                |
    /// | unavailable/unstamped | `None`           | preserved (fail-closed) |
    ///
    /// "Unavailable/unstamped" means presence with NO `worktree_path:` stamp
    /// at all — there is nothing to observe. A session that IS worktree-
    /// stamped but never carried an `observer_pid:` is a different arm since
    /// the observer fail-open fix (task 2914419f): with a readable quiescent
    /// worktree and authored-fact silence past the 2h takeover bar it grades
    /// Stale, which `ReapMode::Full` accepts as owner-stale authority even on
    /// an unexpired lease — the SAME authority the takeover release already
    /// extends at that bar, and an owner renewing its lease authors
    /// ClaimRenewed facts that reset the silence clock. Graded by
    /// `unstamped_observer_session_fails_closed_after_takeover_bar`.
    ///
    /// `automatic_pass_preserves_quiet_live_agent_and_reaps_crashed_agent`
    /// grades the first two arms. The third had no test, which left the
    /// contract able to collapse into two: relaxing the rule to
    /// `observed_verdict != Liveness::Live` — the intuitive "only decline when
    /// we can SEE it alive" reading — keeps every other test in this file green
    /// while making every claim on a host whose observation never resolves
    /// automatically reapable. That is the destructive direction, and
    /// `liveness.rs` pins reaper removal as FAIL-CLOSED precisely because
    /// removal cannot be undone. Grading all three arms in one pass is what
    /// makes that relaxation fail here instead of shipping.
    ///
    /// The second pass asserts the trade this fail-closed rule actually makes.
    /// An unobserved room is not left UNREAPABLE — `ReapMode::Full`, the
    /// human-invoked `rally doctor --reap-stale --apply`, still closes the same
    /// claim on the lease signal alone. It is left needing a human. That second
    /// pass doubles as the non-vacuity guard: it proves the unobserved claim
    /// was lease-eligible the whole time, so its survival above is the
    /// observation rule doing work rather than an inert fixture.
    #[test]
    fn automatic_reap_decision_is_three_way_on_observation() {
        let root = unique_root("observed-three-way");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();

        // Arm 1 — observed live: this process is running, so `kill -0` answers
        // yes and the observer returns Live.
        append_observed_presence(&room, "observed-live", &root, std::process::id());
        let live_claim = append_claim_with_lease(
            &room,
            "claim-observed-live",
            "observed-live",
            &past_ts(3600),
        );

        // Arm 2 — observed dead: a pid far above the system maximum cannot
        // exist, and the worktree HEAD still matches the stamped beat, so the
        // observer is allowed destructive certainty.
        append_observed_presence(&room, "observed-dead", &root, 2_000_000_000);
        let dead_claim = append_claim_with_lease(
            &room,
            "claim-observed-dead",
            "observed-dead",
            &past_ts(3600),
        );

        // Arm 3 — unavailable: presence with no `worktree_path:` /
        // `observer_pid:` stamps, which is every host that has not installed
        // the coordination hook. The observation index has no entry for the tool
        // and the reaper falls back to Unknown.
        append_presence(&room, "unobserved", 5);
        let unobserved_claim =
            append_claim_with_lease(&room, "claim-unobserved", "unobserved", &past_ts(3600));

        let automatic = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();

        let reaped: Vec<&str> = automatic
            .claims_reaped
            .iter()
            .map(|entry| entry.claim_id.as_str())
            .collect();
        assert_eq!(
            reaped,
            vec![dead_claim.event_id.as_str()],
            "the automatic pass must close the observed-dead claim and ONLY that \
             claim: observed-live is vetoed and unavailable observation is \
             fail-closed"
        );

        let active = room.snapshot().unwrap().active_claims;
        let still_active = |id: &str| active.iter().any(|claim| claim.event_id == id);
        assert!(
            still_active(&live_claim.event_id),
            "observed live must survive an automatic pass even with an expired lease"
        );
        assert!(
            still_active(&unobserved_claim.event_id),
            "unavailable observation must NOT authorize automatic removal — this is \
             the arm that silently disappears if the rule is relaxed to \
             `observed_verdict != Live`"
        );
        assert!(
            !still_active(&dead_claim.event_id),
            "observed dead with an expired lease is the one automatic-reap case"
        );

        // The operator path, and the non-vacuity guard for the assertion above.
        let operator = run_reap_stale_in_room_with_mode(&room, true, ReapMode::Full).unwrap();
        let operator_reaped: Vec<&str> = operator
            .claims_reaped
            .iter()
            .map(|entry| entry.claim_id.as_str())
            .collect();
        assert_eq!(
            operator_reaped,
            vec![unobserved_claim.event_id.as_str()],
            "an unobserved host is not left unreapable: the human-invoked Full pass \
             still closes the same lease-expired claim, which also proves that claim \
             was eligible all along and the automatic pass spared it by rule"
        );
        assert!(
            room.snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|claim| claim.event_id == live_claim.event_id),
            "observed live is a veto in BOTH modes — a human pass must not close it \
             either"
        );

        fs::remove_dir_all(root).ok();
    }

    /// End-to-end grade of the observer fail-open fix (task 2914419f, F4):
    /// a session that stamped its worktree but never an `observer_pid:` used
    /// to grade Unknown forever and was never reap-eligible. Two directions:
    ///
    /// 1. Silent past the 2h takeover bar (presence AND claim both 3h old,
    ///    quiescent worktree) → observed Stale → a human-invoked
    ///    `ReapMode::Full` pass reaps it even though its lease has not
    ///    expired — the previously-preserved case, accepted deliberately: it
    ///    is the same owner-stale authority the takeover release extends at
    ///    that bar. Fails on the pre-fix build (Unknown → preserved forever).
    /// 2. Same shape but the session authored a fact minutes ago → the
    ///    silence clock resets → observed Unknown → preserved. Fails on a
    ///    version that anchors the bar on the write-once presence stamp's own
    ///    age.
    #[test]
    fn unstamped_observer_session_fails_closed_after_takeover_bar() {
        let root = unique_root("unstamped-observer-takeover");
        init_observed_worktree(&root);
        // The probe now counts untracked non-ignored files as activity, and
        // this very test writes ledger facts into `<root>/.rally/` — ignore
        // the ledger so the fixture models a quiescent per-agent worktree
        // (the fail-open shape this test grades), then backdate the fixture
        // files so nothing postdates the 3h-old stamp.
        fs::write(root.join(".gitignore"), ".rally/\n").unwrap();
        crate::test_git_fixture::fixture_git(&root, &["add", ".gitignore"]);
        crate::test_git_fixture::fixture_git(&root, &["commit", "-m", "ignore ledger"]);
        for fixture_file in ["observed.txt", ".gitignore"] {
            std::process::Command::new("touch")
                .args([
                    "-t",
                    "200001010000",
                    root.join(fixture_file).to_str().unwrap(),
                ])
                .status()
                .unwrap();
        }
        let room = RoomStore::open_at(root.clone()).unwrap();
        let head = crate::observed_liveness::current_head_sha(&root).expect("fixture HEAD");
        let worktree = fs::canonicalize(&root).unwrap();
        let future_lease = (chrono::Utc::now() + chrono::Duration::hours(4))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let stamped_presence = |tool: &str, session: &str, ago_secs: i64| Fact {
            from_session_id: Some(session.to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Presence,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("observed presence: {tool}"),
            scope: Vec::new(),
            created_at: past_ts(ago_secs),
            summary: None,
            // Worktree-stamped, but NO observer_pid: — the fail-open shape.
            evidence: vec![
                format!("branch_head_sha:{head}"),
                format!("worktree_path:{}", worktree.display()),
            ],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        let backdated_claim = |event_id: &str, tool: &str, session: &str, ago_secs: i64| Fact {
            from_session_id: Some(session.to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}/{session}"),
            scope: vec![format!("file:src/{event_id}.rs")],
            created_at: past_ts(ago_secs),
            summary: None,
            evidence: vec![format!("lease_expires_at:{future_lease}")],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };

        // Direction 1 — silent 3h: everything this session ever authored is
        // past the takeover bar.
        room.append_fact_verified(&stamped_presence("gone-agent", "session-gone", 3 * 3600))
            .unwrap();
        let gone_claim = room
            .append_fact_verified(&backdated_claim(
                "claim-gone-unstamped",
                "gone-agent",
                "session-gone",
                3 * 3600,
            ))
            .unwrap()
            .fact;

        // Direction 2 — same 3h-old stamp and claim, but the session authored
        // a fact minutes ago (any ledger write resets the silence clock).
        room.append_fact_verified(&stamped_presence("busy-agent", "session-busy", 3 * 3600))
            .unwrap();
        let busy_claim = room
            .append_fact_verified(&backdated_claim(
                "claim-busy-unstamped",
                "busy-agent",
                "session-busy",
                3 * 3600,
            ))
            .unwrap()
            .fact;
        let fresh_note = Fact {
            from_session_id: Some("session-busy".to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Artifact,
            tool: Some("busy-agent".to_string()),
            role: None,
            subject: "still here, still working".to_string(),
            scope: Vec::new(),
            created_at: now_string(),
            summary: None,
            evidence: vec!["progress checkpoint".to_string()],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&fresh_note).unwrap();

        let operator = run_reap_stale_in_room_with_mode(&room, true, ReapMode::Full).unwrap();
        let reaped: Vec<&str> = operator
            .claims_reaped
            .iter()
            .map(|entry| entry.claim_id.as_str())
            .collect();
        assert_eq!(
            reaped,
            vec![gone_claim.event_id.as_str()],
            "the 3h-silent unstamped-observer session must be reaped, and ONLY it"
        );
        let active = room.snapshot().unwrap().active_claims;
        assert!(
            active
                .iter()
                .any(|claim| claim.event_id == busy_claim.event_id),
            "a fresh authored fact must preserve the same-shaped session"
        );

        fs::remove_dir_all(root).ok();
    }

    /// RESIDUAL-RISK CONTROL for the observer fail-open fix (task 2914419f,
    /// fix-critique's open objection).
    ///
    /// The fix trades a fail-OPEN for a fail-CLOSED: a never-observed session
    /// that is silent past the 2h takeover bar now grades Stale, and
    /// [`ReapMode::Full`] releases its claims even on an UNEXPIRED lease
    /// (`unstamped_observer_session_fails_closed_after_takeover_bar` grades
    /// that, deliberately). The user-visible failure mode that buys is a LIVE
    /// session — one long tool call, or read-only work on a host with no
    /// coordination hook to stamp `observer_pid:` — losing claims it still
    /// holds.
    ///
    /// What bounds that failure mode is that it takes a HUMAN: `ReapMode::Full`
    /// is reachable only from `rally doctor --reap-stale --apply`, and the
    /// report labels the release `owner-stale` rather than `lease-expired` so
    /// the operator can see the verdict rests on a heuristic. The AUTOMATIC
    /// `enter` path is [`ReapMode::LeaseOnly`], which drops the owner-stale
    /// signal entirely and demands a writer-stamped expiry corroborated by
    /// observed death.
    ///
    /// That bound is the whole argument for keeping the bar at 2h, so it is
    /// pinned here rather than left to the eligibility expression. Fails on any
    /// build that lets the owner-stale signal reach the automatic arm — the
    /// change that would turn an accepted operator-gated risk into silent
    /// background claim loss for every unstamped session in the fleet.
    #[test]
    fn automatic_reap_preserves_a_silent_unstamped_session_on_an_unexpired_lease() {
        let root = unique_root("unstamped-observer-automatic");
        init_observed_worktree(&root);
        // Same quiescent-worktree fixture as the Full-mode sibling: ignore the
        // ledger this test writes, then backdate the fixture files so nothing
        // postdates the 3h-old presence stamp.
        fs::write(
            root.join(".gitignore"),
            ".rally/
",
        )
        .unwrap();
        crate::test_git_fixture::fixture_git(&root, &["add", ".gitignore"]);
        crate::test_git_fixture::fixture_git(&root, &["commit", "-m", "ignore ledger"]);
        for fixture_file in ["observed.txt", ".gitignore"] {
            std::process::Command::new("touch")
                .args([
                    "-t",
                    "200001010000",
                    root.join(fixture_file).to_str().unwrap(),
                ])
                .status()
                .unwrap();
        }
        let room = RoomStore::open_at(root.clone()).unwrap();
        let head = crate::observed_liveness::current_head_sha(&root).expect("fixture HEAD");
        let worktree = fs::canonicalize(&root).unwrap();
        let future_lease = (chrono::Utc::now() + chrono::Duration::hours(4))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        room.append_fact_verified(&Fact {
            from_session_id: Some("session-quiet".to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: new_id("fact"),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Presence,
            tool: Some("quiet-agent".to_string()),
            role: None,
            subject: "observed presence: quiet-agent".to_string(),
            scope: Vec::new(),
            created_at: past_ts(3 * 3600),
            summary: None,
            // Worktree-stamped, no observer_pid: — the fail-closed shape.
            evidence: vec![
                format!("branch_head_sha:{head}"),
                format!("worktree_path:{}", worktree.display()),
            ],
            target: None,
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        })
        .unwrap();
        let quiet_claim = room
            .append_fact_verified(&Fact {
                from_session_id: Some("session-quiet".to_string()),
                schema: FACT_SCHEMA.to_string(),
                event_id: "claim-quiet-unstamped".to_string(),
                seq: 0,
                thread_id: new_id("room"),
                kind: FactKind::Claim,
                tool: Some("quiet-agent".to_string()),
                role: None,
                subject: "claim: quiet-agent".to_string(),
                scope: vec!["file:src/quiet.rs".to_string()],
                created_at: past_ts(3 * 3600),
                summary: None,
                evidence: vec![format!("lease_expires_at:{future_lease}")],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            })
            .unwrap()
            .fact;

        // Sanity: the SAME fixture is reap-eligible under the operator-invoked
        // mode, so a green assertion below cannot come from a fixture that
        // simply never graded Stale.
        let operator_preview =
            run_reap_stale_in_room_with_mode(&room, false, ReapMode::Full).unwrap();
        assert_eq!(
            operator_preview
                .claims_reaped
                .iter()
                .map(|entry| entry.claim_id.as_str())
                .collect::<Vec<_>>(),
            vec![quiet_claim.event_id.as_str()],
            "fixture must be owner-stale, or this test proves nothing"
        );
        assert_eq!(
            operator_preview.claims_reaped[0].reason, "owner-stale",
            "and the operator report must name the heuristic, not a stamped expiry"
        );

        let automatic = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();
        assert!(
            automatic.claims_reaped.is_empty(),
            "the automatic enter path must never release an unexpired lease on an \
             owner-staleness heuristic; reaped={:?}",
            automatic
                .claims_reaped
                .iter()
                .map(|entry| (entry.claim_id.as_str(), entry.reason.as_str()))
                .collect::<Vec<_>>()
        );
        assert!(
            room.snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|claim| claim.event_id == quiet_claim.event_id),
            "the claim must still be active after the automatic pass"
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn expired_lease_authorizes_typed_cleanup_but_not_an_ordinary_peer_release() {
        let root = unique_root("typed-expiry-authority");
        init_observed_worktree(&root);
        let room = RoomStore::open_at(root.clone()).unwrap();
        let expired = past_ts(3600);

        // A live external observer is a veto. Lease expiry alone must not turn
        // an ordinary peer Release into owner authority.
        append_observed_presence_for_session(
            &room,
            "live-owner",
            "session-live",
            &root,
            std::process::id(),
        );
        let live_claim = append_session_claim_with_lease(
            &room,
            "claim-live-expired",
            "live-owner",
            "session-live",
            &expired,
        );
        let before_release = room.facts().unwrap().len();
        let peer_release = Fact {
            from_session_id: Some("session-peer".to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: "release-peer-expired".to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Release,
            tool: Some("peer".to_string()),
            role: None,
            subject: "ordinary peer release".to_string(),
            scope: live_claim.scope.clone(),
            created_at: now_string(),
            summary: None,
            evidence: Vec::new(),
            target: None,
            ref_id: Some(live_claim.event_id.clone()),
            status: None,
            severity: None,
            uri: None,
            session: None,
        };
        room.append_fact_verified(&peer_release)
            .expect_err("expired lease alone must not authorize an ordinary peer Release");
        assert_eq!(
            room.facts().unwrap().len(),
            before_release,
            "refused peer Release must not append"
        );
        assert!(
            room.snapshot()
                .unwrap()
                .active_claims
                .iter()
                .any(|claim| claim.event_id == live_claim.event_id),
            "live observed claim must survive the refused peer Release"
        );

        // Full/operator mode may still emit the typed ClaimExpired transition
        // for an expired claim whose observation is Unknown.
        let manual_claim = append_session_claim_with_lease(
            &room,
            "claim-manual-expired",
            "manual-owner",
            "session-manual",
            &expired,
        );
        let manual = run_reap_stale_in_room_with_mode(&room, true, ReapMode::Full).unwrap();
        assert_eq!(
            manual
                .claims_reaped
                .iter()
                .map(|entry| entry.claim_id.as_str())
                .collect::<Vec<_>>(),
            vec![manual_claim.event_id.as_str()]
        );

        // Automatic cleanup retains its stronger exact-Stale requirement.
        append_observed_presence_for_session(
            &room,
            "dead-owner",
            "session-dead",
            &root,
            2_000_000_000,
        );
        let dead_claim = append_session_claim_with_lease(
            &room,
            "claim-auto-expired",
            "dead-owner",
            "session-dead",
            &expired,
        );
        let automatic = run_reap_stale_in_room_with_mode(&room, true, ReapMode::LeaseOnly).unwrap();
        assert_eq!(
            automatic
                .claims_reaped
                .iter()
                .map(|entry| entry.claim_id.as_str())
                .collect::<Vec<_>>(),
            vec![dead_claim.event_id.as_str()]
        );

        let facts = room.facts().unwrap();
        for (claim_id, reason, observed) in [
            (manual_claim.event_id.as_str(), "lease-expired", "unknown"),
            (
                dead_claim.event_id.as_str(),
                "owner-stale+lease-expired",
                "stale",
            ),
        ] {
            let expiry = facts
                .iter()
                .find(|fact| {
                    fact.kind == FactKind::ClaimExpired && fact.ref_id.as_deref() == Some(claim_id)
                })
                .expect("typed cleanup must append ClaimExpired");
            assert_eq!(expiry.tool.as_deref(), Some("rally"));
            assert!(
                expiry
                    .evidence
                    .iter()
                    .any(|item| item == &format!("reaper:reason={reason}"))
            );
            assert!(
                expiry
                    .evidence
                    .iter()
                    .any(|item| item == &format!("reaper:observed={observed}"))
            );
        }
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
            "auto-reap must stay opt-in until concurrent enter is bounded and the operator flips the destructive default"
        );
        assert!(
            maybe_reap_on_enter(&room, "test-agent").is_none(),
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
        let result = maybe_reap_on_enter(&room, "test-agent");
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

    #[test]
    fn o26_reaper_surfaces_committed_projection_warning() {
        let root = unique_root("o26-projection-warning");
        let room = RoomStore::open_at(root.clone()).unwrap();
        append_presence(&room, "stale-tool", 3 * 60 * 60);
        let claim = append_claim(&room, "claim-o26-projection", "stale-tool");
        crate::store::fail_o26_once(
            &room.rally_dir(),
            crate::store::O26FaultPoint::FactsDbProjection,
        );

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert_eq!(report.claims_reaped.len(), 1);
        assert_eq!(report.append_outcomes.len(), 1);
        let outcome = &report.append_outcomes[0];
        assert!(outcome.committed);
        assert!(!outcome.projection_complete);
        assert!(!outcome.warnings.is_empty());
        assert_eq!(
            outcome.fact.ref_id.as_deref(),
            Some(claim.event_id.as_str())
        );
        assert!(report.outcome_unknowns.is_empty());
        assert_eq!(report.write_failures, 0);

        let matching = room
            .facts()
            .unwrap()
            .into_iter()
            .filter(|fact| fact.event_id == outcome.fact.event_id)
            .count();
        assert_eq!(matching, 1, "the degraded canonical close is a singleton");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn o26_reaper_unknown_is_queryable_and_next_pass_does_not_duplicate() {
        let root = unique_root("o26-outcome-unknown");
        let room = RoomStore::open_at(root.clone()).unwrap();
        append_presence(&room, "stale-tool", 3 * 60 * 60);
        let claim = append_claim(&room, "claim-o26-unknown", "stale-tool");
        crate::store::fail_o26_once(
            &room.rally_dir(),
            crate::store::O26FaultPoint::AfterCanonicalSyncBeforeReadback,
        );

        let first = run_reap_stale_in_room(&room, true).unwrap();

        assert!(first.claims_reaped.is_empty());
        assert!(first.append_outcomes.is_empty());
        assert_eq!(first.outcome_unknowns.len(), 1);
        assert_eq!(first.write_failures, 1);
        let unknown = &first.outcome_unknowns[0];
        assert_eq!(unknown.phase, "canonical-sync-before-readback");
        assert!(unknown.remedy.contains(&unknown.event_id));
        assert_eq!(
            unknown.event_id,
            stable_reaper_event_id("claim-expired", &claim.event_id)
        );

        let second = run_reap_stale_in_room(&room, true).unwrap();
        assert!(second.claims_reaped.is_empty());
        assert!(second.append_outcomes.is_empty());
        assert!(second.outcome_unknowns.is_empty());
        let matching = room
            .facts()
            .unwrap()
            .into_iter()
            .filter(|fact| fact.event_id == unknown.event_id)
            .count();
        assert_eq!(matching, 1, "lost reply plus next pass must append once");
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
                from_session_id: Some("sess-tool-a".to_string()),
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
        room.append_fact_verified(&fact).unwrap().fact
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
            crate::claim_authority::claim_owner_matches_caller(
                c.tool.as_deref(),
                c.from_session_id.as_deref(),
                Some(stopping_tool),
                Some(stopping_session),
            )
        }) {
            let release = Fact {
                from_session_id: Some(stopping_session.to_string()),
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
        room.append_fact_verified(&fact).unwrap().fact
    }

    fn append_session_claim_with_lease(
        room: &RoomStore,
        event_id: &str,
        tool: &str,
        from_session_id: &str,
        lease_ts: &str,
    ) -> Fact {
        let fact = Fact {
            from_session_id: Some(from_session_id.to_string()),
            schema: FACT_SCHEMA.to_string(),
            event_id: event_id.to_string(),
            seq: 0,
            thread_id: new_id("room"),
            kind: FactKind::Claim,
            tool: Some(tool.to_string()),
            role: None,
            subject: format!("claim: {tool}/{from_session_id}"),
            scope: vec![format!("file:src/session_{event_id}.rs")],
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
        room.append_fact_verified(&fact).unwrap().fact
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

    #[test]
    fn durable_renewal_after_original_expiry_survives_reap() {
        let root = unique_root("durable-renewal-survives-reap");
        let room = RoomStore::open_at(root.clone()).unwrap();
        append_presence(&room, "live-owner", 5);
        let past_lease = past_ts(3600);
        let claim = append_claim_with_lease(
            &room,
            "claim-renewed-after-expiry",
            "live-owner",
            &past_lease,
        );
        let renewed_until = {
            use chrono::{SecondsFormat, Utc};
            (Utc::now() + chrono::Duration::seconds(3600))
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        room.renew_claim_lease(
            &claim.event_id,
            renewed_until.clone(),
            "live-owner",
            None,
            None,
        )
        .unwrap()
        .expect("active claim must renew");

        let report = run_reap_stale_in_room(&room, true).unwrap();

        assert!(report.claims_reaped.is_empty());
        let active = room
            .snapshot()
            .unwrap()
            .active_claims
            .into_iter()
            .find(|fact| fact.event_id == claim.event_id)
            .expect("renewed claim must remain active");
        assert!(
            active
                .evidence
                .iter()
                .any(|item| item == &format!("lease_expires_at:{renewed_until}"))
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
