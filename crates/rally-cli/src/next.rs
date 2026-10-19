use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeSet;

use crate::backlog::BacklogItem;
use crate::store::{Fact, FactKind, RoomSnapshot, Squad};
use crate::{FACT_SCHEMA, normalize_path, path_matches_scope, shell_quote};

/// Default window after which an unanswered handoff is de-prioritized out of
/// `next`'s waiting/ranking projection. It remains an open inbox obligation.
/// Overridable via
/// `coordination.stale_wait_secs` (config) or `RALLY_STALE_WAIT_SECS` (env) —
/// resolved by the caller and threaded in, so this module stays pure and the
/// window can flex per repo instead of being pinned at one day for everyone.
pub(crate) const DEFAULT_STALE_WAIT_SECS: i64 = 24 * 60 * 60;
pub(crate) const MAX_ENTER_ROWS: usize = 128;

/// Ranking bands. Work assigned to this tool BY NAME (named handoff 100,
/// targeted plan item 95, own blocker 90, own live claim 75) always outranks
/// the open queue, so an agent picks open-queue work up when it has nothing
/// assigned. A lease-expired claim is a housekeeping prompt, not work.
const NAMED_HANDOFF_SCORE: i64 = 100;
const OPEN_QUEUE_SCORE: i64 = 70;
const OPEN_QUEUE_REVALIDATE_SCORE: i64 = 50;
const EXPIRED_CLAIM_SCORE: i64 = 30;

/// What `build_next` does with an INBOUND targeted handoff older than the stale
/// window.
///
/// The two callers of `build_next` ask genuinely different questions, and one
/// answer cannot serve both:
///
/// * `command_next` asks **"what should I do?"** — and an obligation addressed to
///   this agent is still owed at any age. Dropping it at 24h is a reader-side
///   erasure of a sender-side obligation (intent.md defect #1), so this caller
///   passes [`StaleTargetedPolicy::Surface`].
/// * `command_self_exit_check` asks **"may this session stop existing?"** — it
///   reads `next.actionable` to compute `empty_cycle` (`lib.rs`), which drives the
///   tmux self-kill. If an ancient handoff made `actionable` permanently true, a
///   session holding one could NEVER self-exit. So that caller passes
///   [`StaleTargetedPolicy::Ignore`], preserving the pre-existing age drop.
///
/// The ancient handoff is not lost under `Ignore`: it is still in
/// `snapshot.open_obligations` and still rendered by `rally inbox` and by
/// `next.inbox`, neither of which age out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StaleTargetedPolicy {
    /// Keep an inbound targeted handoff as a `next` candidate at any age.
    Surface,
    /// Drop an inbound targeted handoff past the stale window (pre-existing
    /// behaviour; required so a session can reach `empty_cycle` and self-exit).
    Ignore,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct EntryData {
    tool: String,
    role: Option<String>,
    #[serde(rename = "do")]
    do_items: Vec<EntryItem>,
    do_not: Vec<EntryItem>,
    know: Vec<EntryItem>,
    verify: Vec<EntryItem>,
    respond_to: Vec<EntryItem>,
    ignore: Vec<EntryItem>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct EntryItem {
    reason: &'static str,
    event_id: String,
    kind: FactKind,
    subject: String,
    scope: Vec<String>,
    tool: Option<String>,
    target: Option<String>,
    evidence: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct AttentionItem {
    reason: &'static str,
    event_id: String,
    seq: i64,
    kind: FactKind,
    subject: String,
    scope: Vec<String>,
    tool: Option<String>,
    target: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct NextResult {
    mode: &'static str,
    pub(crate) action: &'static str,
    /// Layer 1 self-exit reads this: `true` when `next` surfaced real work to
    /// do, `false` when the agent should wait / proceed-solo (no addressed work).
    pub(crate) actionable: bool,
    reason: &'static str,
    score: i64,
    confidence: f64,
    requires_human: bool,
    stop_reason: Option<&'static str>,
    pub(crate) target_event_id: Option<String>,
    source_event_ids: Vec<String>,
    fact: Option<Fact>,
    suggested_claims: Vec<SuggestedClaim>,
    suggested_commands: Vec<String>,
    completion: CompletionContract,
    waiting_on: Vec<Fact>,
    alternatives: Vec<NextCandidateData>,
    /// #7: open backlog items whose deps are all satisfied and whose owned
    /// paths are not actively claimed. Read-only suggestion — agent must still
    /// `rally say claim` to take the work.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) suggested_backlog_items: Vec<SuggestedBacklogItem>,
    /// Visible peers ranked freshest-first, for choosing a handoff target.
    /// ADVISORY: a stale peer is still a legal `--target`; this only tells the
    /// sender who was seen recently so it does not hand work to a ghost by
    /// default. See [`PeerTargets`].
    pub(crate) peer_targets: PeerTargets,
    /// Open obligations addressed to this tool, from
    /// `snapshot.open_obligations`.
    ///
    /// EMBEDDED rather than fetched: the coordination hook renders this on every
    /// `start` / `idle` / `after-write` fire, and a separate `rally inbox` call
    /// would add one process spawn per fire. Spawn count is an explicitly
    /// attributed cost component in this repo (O39/O36), so the projection is
    /// built from the snapshot `build_next` already holds — zero extra store
    /// read.
    ///
    /// Additive: `NextResult` is `pub(crate)` + `Serialize` with no `Deserialize`
    /// and no exhaustive external match, and the published `next` schema does not
    /// set `additionalProperties: false`.
    pub(crate) inbox: crate::obligations::InboxResult,
}

/// How many inbox items ride inline in `next.inbox.items`.
///
/// The hook renders COUNTS, not items, so a short list is enough for a human
/// reading `rally next --json` directly. `inbox.count` stays exact regardless;
/// `rally inbox --tool <id>` is the full view.
pub(crate) const NEXT_INBOX_ITEMS_LIMIT: usize = 5;

/// How many ranked peers `next` lists inline. The counts cover the whole
/// room; the list is a shortlist so a 380-squad room does not print 380 rows.
pub(crate) const PEER_TARGETS_LIMIT: usize = 8;

/// Freshness-ranked handoff-target shortlist (see [`RoomSnapshot::ranked_peers`]).
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub(crate) struct PeerTargets {
    /// Room-wide tally over visible peers (self excluded).
    pub(crate) fresh: usize,
    pub(crate) stale: usize,
    pub(crate) unknown: usize,
    /// Freshest first: fresh → unknown → stale, then youngest `age_secs`.
    pub(crate) ranked: Vec<PeerTarget>,
    /// Peers not shown because the shortlist is capped at
    /// [`PEER_TARGETS_LIMIT`]. `rally room --json` lists every squad.
    pub(crate) truncated: usize,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct PeerTarget {
    pub(crate) tool: String,
    pub(crate) freshness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) age_secs: Option<i64>,
    pub(crate) window_secs: i64,
    pub(crate) last_seen_ts: String,
    pub(crate) status: String,
    pub(crate) acknowledged: bool,
}

impl PeerTarget {
    fn from_squad(squad: &Squad) -> Self {
        Self {
            tool: squad.tool.clone(),
            freshness: squad.freshness.clone(),
            age_secs: squad.age_secs,
            window_secs: squad.window_secs,
            last_seen_ts: squad.last_seen_ts.clone(),
            status: squad.status.clone(),
            acknowledged: squad.acknowledged,
        }
    }
}

/// Rank every visible peer of `tool` by freshness and cut the shortlist.
pub(crate) fn peer_targets(snapshot: &RoomSnapshot, tool: &str, limit: usize) -> PeerTargets {
    let ranked = snapshot.ranked_peers(Some(tool));
    let mut out = PeerTargets::default();
    for sq in &ranked {
        match sq.freshness.as_str() {
            crate::store::FRESHNESS_FRESH => out.fresh += 1,
            crate::store::FRESHNESS_STALE => out.stale += 1,
            _ => out.unknown += 1,
        }
    }
    out.ranked = ranked
        .iter()
        .take(limit)
        .map(|sq| PeerTarget::from_squad(sq))
        .collect();
    out.truncated = ranked.len().saturating_sub(out.ranked.len());
    out
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct NextCandidateData {
    action: &'static str,
    reason: &'static str,
    score: i64,
    confidence: f64,
    target_event_id: Option<String>,
    source_event_ids: Vec<String>,
    fact: Option<Fact>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SuggestedClaim {
    scope: String,
    command: String,
}

/// A backlog item suggested as next work when its deps are satisfied and its
/// owned paths are not actively claimed by another tool.
///
/// Read-only suggestion — the agent still must `rally say claim` to take it.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SuggestedBacklogItem {
    pub(crate) id: String,
    pub(crate) intent: String,
    pub(crate) owns: Vec<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) status: String,
    pub(crate) target: Option<String>,
    pub(crate) expected_by: Option<String>,
    pub(crate) event_id: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct ActionContract {
    actionable: bool,
    requires_human: bool,
    stop_reason: Option<&'static str>,
    suggested_claims: Vec<SuggestedClaim>,
    suggested_commands: Vec<String>,
    completion: CompletionContract,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct CompletionContract {
    record_kind: &'static str,
    evidence_required: bool,
    release_claims: bool,
    rerun_next: bool,
}

pub(crate) fn build_entry(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
) -> EntryData {
    // Named handoffs first, then the open queue under its own reason, so an
    // entering agent can tell "assigned to you" from "available to anyone".
    let respond_to = snapshot
        .open_handoffs
        .iter()
        .filter(|f| handoff_address(f, tool) == HandoffAddress::Named)
        .map(|f| entry_item("respond_to_handoff", f))
        .chain(
            snapshot
                .open_handoffs
                .iter()
                .filter(|f| handoff_address(f, tool) == HandoffAddress::OpenQueue)
                .map(|f| entry_item("open_queue_item", f)),
        )
        .take(MAX_ENTER_ROWS)
        .collect::<Vec<_>>();
    let mut do_items = respond_to.clone();
    do_items.extend(
        snapshot
            .active_claims
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| {
                if crate::claim_authority::lease_expired_at(f, chrono::Utc::now()) {
                    entry_item("release_or_renew_claim", f)
                } else {
                    entry_item("continue_or_release_claim", f)
                }
            }),
    );
    do_items.truncate(MAX_ENTER_ROWS);
    do_items.extend(
        snapshot
            .active_blockers
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| entry_item("resolve_owned_blocker", f)),
    );
    do_items.truncate(MAX_ENTER_ROWS);
    let mut do_not = Vec::new();
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() != Some(tool)
            && (paths.is_empty()
                || paths.iter().any(|path| {
                    claim
                        .scope
                        .iter()
                        .any(|scope| path_matches_scope(scope, path))
                }))
        {
            do_not.push(entry_item("avoid_claimed_scope", claim));
            if do_not.len() == MAX_ENTER_ROWS {
                break;
            }
        }
    }
    let know = snapshot
        .current_decisions
        .iter()
        .take(8)
        .map(|f| entry_item("decision", f))
        .collect::<Vec<_>>();
    let verify = snapshot
        .unconsumed_artifacts
        .iter()
        .take(8)
        .map(|f| entry_item("unconsumed_artifact", f))
        .collect::<Vec<_>>();
    EntryData {
        tool: tool.to_string(),
        role: role.map(str::to_string),
        do_items,
        do_not,
        know,
        verify,
        respond_to,
        ignore: Vec::new(),
    }
}

#[derive(Clone, Debug)]
struct NextCandidate {
    action: &'static str,
    reason: &'static str,
    score: i64,
    fact: Option<Fact>,
    source_event_ids: Vec<String>,
}

impl NextCandidate {
    fn from_fact(action: &'static str, reason: &'static str, score: i64, fact: &Fact) -> Self {
        Self {
            action,
            reason,
            score,
            fact: Some(fact.clone()),
            source_event_ids: vec![fact.event_id.clone()],
        }
    }

    fn synthetic(
        action: &'static str,
        reason: &'static str,
        score: i64,
        source_event_ids: Vec<String>,
        fact: Option<Fact>,
    ) -> Self {
        Self {
            action,
            reason,
            score,
            fact,
            source_event_ids,
        }
    }

    fn seq(&self) -> i64 {
        self.fact.as_ref().map(|fact| fact.seq).unwrap_or_default()
    }

    fn to_data(&self) -> NextCandidateData {
        let target_event_id = self.fact.as_ref().map(|fact| fact.event_id.clone());
        let score = self.score.clamp(5, 95);
        let confidence = (score as f64) / 100.0;
        NextCandidateData {
            action: self.action,
            reason: self.reason,
            score,
            confidence,
            target_event_id,
            source_event_ids: self.source_event_ids.clone(),
            fact: self.fact.clone(),
        }
    }
}

// One more parameter than clippy's default threshold, for the same reason
// `next_candidates` below carries the allow: a params struct here would exist
// only to satisfy a lint on a crate-private function with two call sites.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_next(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    limit: usize,
    backlog_items: Vec<BacklogItem>,
    stale_wait_secs: i64,
    stale_targeted: StaleTargetedPolicy,
    fs_signals: &crate::open_queue::FsSignals,
) -> NextResult {
    let waiting_on = waiting_on_facts(snapshot, tool, stale_wait_secs);
    // ONE projection of what this tool is owed and what it may pick up. The
    // ranking below and the embedded `inbox` are both read off it, so they
    // cannot disagree on the same snapshot.
    let open_queue = crate::open_queue::open_queue_items(
        snapshot,
        tool,
        fs_signals,
        chrono::Utc::now().timestamp(),
    );
    let waiting = !waiting_on.is_empty();

    // #7: filter backlog items to those ready for pickup:
    //   - status is not "done"
    //   - all depends_on ids are satisfied (done)
    //   - no owned path is actively claimed by another tool
    let all_item_ids: std::collections::BTreeSet<String> =
        backlog_items.iter().map(|i| i.id.clone()).collect();
    let done_ids: std::collections::BTreeSet<String> = backlog_items
        .iter()
        .filter(|i| i.status == "done")
        .map(|i| i.id.clone())
        .collect();
    // Collect paths actively claimed by any tool (for exclusion)
    let claimed_scopes: std::collections::BTreeSet<String> = snapshot
        .active_claims
        .iter()
        .flat_map(|c| c.scope.clone())
        .collect();
    let ready_backlog_items: Vec<BacklogItem> = backlog_items
        .iter()
        .filter(|item| item.status != "done")
        .filter(|item| {
            // All deps satisfied
            item.depends_on.iter().all(|dep| {
                // dep is satisfied if it's not in all_item_ids (unknown = considered done)
                // or if it's in done_ids
                !all_item_ids.contains(dep) || done_ids.contains(dep)
            })
        })
        .filter(|item| {
            // No owned path actively claimed by another tool
            item.owns.iter().all(|path| {
                let normalized = crate::normalize_path(path.clone());
                !claimed_scopes.contains(&normalized) && !claimed_scopes.contains(path)
            })
        })
        .cloned()
        .collect();
    let mut candidates = next_candidates(
        snapshot,
        tool,
        role,
        paths,
        waiting,
        &waiting_on,
        &backlog_items,
        stale_wait_secs,
        stale_targeted,
        &open_queue,
    );
    candidates.sort_by(compare_next_candidates);

    let top = candidates
        .first()
        .cloned()
        .unwrap_or_else(default_next_candidate);
    let alternatives = candidates
        .iter()
        .skip(1)
        .take(limit.saturating_sub(1))
        .map(NextCandidate::to_data)
        .collect::<Vec<_>>();
    let mode = next_mode(waiting, top.action);
    let contract = action_contract(&top, tool);
    let top_data = top.to_data();

    let suggested_backlog_items: Vec<SuggestedBacklogItem> = ready_backlog_items
        .into_iter()
        .map(|item| SuggestedBacklogItem {
            id: item.id,
            intent: item.intent,
            owns: item.owns,
            depends_on: item.depends_on,
            status: item.status,
            target: item.target,
            expected_by: item.expected_by,
            event_id: item.event_id,
        })
        .collect();

    NextResult {
        mode,
        action: top_data.action,
        actionable: contract.actionable,
        reason: top_data.reason,
        score: top_data.score,
        confidence: top_data.confidence,
        requires_human: contract.requires_human,
        stop_reason: contract.stop_reason,
        target_event_id: top_data.target_event_id,
        source_event_ids: top_data.source_event_ids,
        fact: top_data.fact,
        suggested_claims: contract.suggested_claims,
        suggested_commands: contract.suggested_commands,
        completion: contract.completion,
        waiting_on,
        alternatives,
        suggested_backlog_items,
        peer_targets: peer_targets(snapshot, tool, PEER_TARGETS_LIMIT),
        inbox: crate::obligations::build_inbox(
            snapshot,
            tool,
            NEXT_INBOX_ITEMS_LIMIT,
            stale_wait_secs,
            open_queue,
        ),
    }
}

fn waiting_on_facts(snapshot: &RoomSnapshot, tool: &str, stale_wait_secs: i64) -> Vec<Fact> {
    let stale_targets = snapshot.takeover_eligible_owners();
    snapshot
        .open_handoffs
        .iter()
        .chain(snapshot.active_blockers.iter())
        .filter(|fact| waiting_on_peer(fact, tool))
        .filter(|fact| !stale_wait_obligation(fact, &stale_targets, stale_wait_secs))
        .cloned()
        .collect()
}

// One more parameter than clippy's default threshold. The alternative — bundling
// these into a params struct — would add a type whose only purpose is to satisfy
// a lint, for a private function with a single call site.
#[allow(clippy::too_many_arguments)]
fn next_candidates(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    waiting: bool,
    waiting_on: &[Fact],
    backlog_items: &[BacklogItem],
    stale_wait_secs: i64,
    stale_targeted: StaleTargetedPolicy,
    open_queue: &[crate::open_queue::OpenQueueItem],
) -> Vec<NextCandidate> {
    let mut candidates = Vec::new();

    // Work assigned to this tool BY NAME. Read from `open_obligations`, the same
    // bucket `rally inbox` renders, so `next` can never rank a named handoff the
    // inbox does not hold (or miss one it does). See
    // `crate::obligations::build_obligation_view`.
    for handoff in crate::obligations::named_handoffs(snapshot, tool) {
        // Under `Surface`, age is not a reason to stop offering an obligation
        // addressed to this agent. Under `Ignore`, the pre-existing drop stands
        // so `command_self_exit_check` can still reach a non-actionable cycle —
        // see [`StaleTargetedPolicy`].
        let aged_out = stale_targeted == StaleTargetedPolicy::Ignore
            && stale_targeted_handoff(handoff, tool, stale_wait_secs);
        if !aged_out {
            candidates.push(NextCandidate::from_fact(
                "respond_to_handoff",
                "open_handoff_targeted_to_this_tool",
                boost_score(NAMED_HANDOFF_SCORE, handoff, role, paths),
                handoff,
            ));
        }
    }
    // The OPEN QUEUE: handoffs with no target (or the `all` broadcast). Nobody
    // owns them; any agent may take one. They rank below every class of work
    // assigned to this tool, so they surface when the agent has nothing
    // assigned — the assign-or-pick-up-when-idle model. Age never removes one;
    // it only marks it "revalidate before claiming" (see `open_queue`).
    for item in open_queue {
        let (reason, base) = if item.revalidate_before_claiming {
            (
                "open_queue_item_needs_revalidation",
                OPEN_QUEUE_REVALIDATE_SCORE,
            )
        } else {
            ("open_queue_item_available", OPEN_QUEUE_SCORE)
        };
        if let Some(fact) = snapshot
            .open_handoffs
            .iter()
            .find(|fact| fact.event_id == item.event_id)
        {
            candidates.push(NextCandidate::from_fact(
                "claim_open_queue_item",
                reason,
                base,
                fact,
            ));
        }
    }
    for blocker in &snapshot.active_blockers {
        if blocker.tool.as_deref() == Some(tool) {
            candidates.push(NextCandidate::from_fact(
                "resolve_owned_blocker",
                "owned_blocker_is_still_open",
                boost_score(90, blocker, role, paths),
                blocker,
            ));
        }
    }
    let now = chrono::Utc::now();
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() == Some(tool) {
            // `active_claims` holds EFFECTIVE claims (renewals folded in), so a
            // claim whose lease has passed was not renewed. It is no longer
            // "still active" work; it is a low-rank prompt to release it, or
            // renew it if this session still owns the work.
            if crate::claim_authority::lease_expired_at(claim, now) {
                candidates.push(NextCandidate::from_fact(
                    "release_or_renew_claim",
                    "owned_claim_lease_expired",
                    EXPIRED_CLAIM_SCORE,
                    claim,
                ));
            } else {
                candidates.push(NextCandidate::from_fact(
                    "continue_or_release_claim",
                    "owned_claim_is_still_active",
                    boost_score(75, claim, role, paths),
                    claim,
                ));
            }
        }
    }
    for artifact in &snapshot.unconsumed_artifacts {
        let authored_by_self = artifact.tool.as_deref() == Some(tool);
        let routed_elsewhere = artifact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all");
        if !authored_by_self && !routed_elsewhere {
            let (reason, base_score) = artifact_review_priority(artifact, tool, waiting);
            candidates.push(NextCandidate::from_fact(
                "review_artifact",
                reason,
                boost_artifact_review_score(base_score, artifact, role, paths),
                artifact,
            ));
        }
    }
    for handoff in waiting_on.iter().filter(|fact| fact.kind == "handoff") {
        if fact_is_weak(handoff) {
            candidates.push(NextCandidate::from_fact(
                "clarify_handoff",
                "outgoing_handoff_needs_more_context_for_the_target_agent",
                boost_score(55, handoff, role, paths),
                handoff,
            ));
        }
    }
    for item in backlog_items {
        if item.target.as_deref() == Some(tool) && backlog_item_requires_status_update(item) {
            let fact = backlog_item_as_fact(item);
            candidates.push(NextCandidate::from_fact(
                "update_plan_status",
                "targeted_backlog_plan_needs_status",
                95,
                &fact,
            ));
        }
    }

    if candidates.is_empty() {
        candidates.push(idle_next_candidate(waiting, waiting_on));
    }

    candidates
}

fn backlog_item_requires_status_update(item: &BacklogItem) -> bool {
    matches!(item.status.as_str(), "open" | "planned" | "blocked")
}

fn backlog_item_as_fact(item: &BacklogItem) -> Fact {
    let mut scope = vec!["backlog-item".to_string()];
    scope.extend(item.owns.iter().map(|path| format!("owns:{path}")));
    scope.sort();
    scope.dedup();

    let mut evidence: Vec<String> = item
        .depends_on
        .iter()
        .map(|dep| format!("dep:{dep}"))
        .collect();
    if let Some(expected_by) = &item.expected_by {
        evidence.push(format!("expected_by:{expected_by}"));
    }
    evidence.sort();
    evidence.dedup();

    Fact {
        from_session_id: None,
        schema: FACT_SCHEMA.to_string(),
        event_id: item.event_id.clone(),
        seq: item.seq,
        thread_id: format!("backlog-{}", item.id.chars().take(32).collect::<String>()),
        kind: FactKind::BacklogItem,
        tool: item.tool.clone(),
        role: None,
        subject: item.intent.clone(),
        scope,
        created_at: item.created_at.clone(),
        summary: Some(format!("id:{}", item.id)),
        evidence,
        target: item.target.clone(),
        ref_id: None,
        status: Some(item.status.clone()),
        severity: None,
        uri: None,
        session: None,
    }
}

fn idle_next_candidate(waiting: bool, waiting_on: &[Fact]) -> NextCandidate {
    if waiting {
        NextCandidate::synthetic(
            "wait",
            "waiting_on_peer_with_no_useful_alternate_work",
            10,
            waiting_on
                .iter()
                .map(|fact| fact.event_id.clone())
                .collect(),
            waiting_on.first().cloned(),
        )
    } else {
        NextCandidate::synthetic(
            "proceed_solo",
            "no_room_items_require_action",
            5,
            Vec::new(),
            None,
        )
    }
}

fn default_next_candidate() -> NextCandidate {
    NextCandidate::synthetic("proceed_solo", "empty_room", 5, Vec::new(), None)
}

fn compare_next_candidates(left: &NextCandidate, right: &NextCandidate) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| right.seq().cmp(&left.seq()))
        .then_with(|| left.action.cmp(right.action))
}

fn next_mode(waiting: bool, action: &str) -> &'static str {
    if waiting && action != "wait" {
        "useful_while_waiting"
    } else if waiting {
        "waiting"
    } else if action == "proceed_solo" {
        "idle"
    } else {
        "direct"
    }
}

fn action_contract(candidate: &NextCandidate, tool: &str) -> ActionContract {
    let actionable = !matches!(candidate.action, "wait" | "proceed_solo");
    let stop_reason = match candidate.action {
        "wait" => Some("waiting_on_peer_with_no_useful_alternate_work"),
        "proceed_solo" => Some("no_actionable_room_item"),
        _ => None,
    };
    let suggested_claims = candidate
        .fact
        .as_ref()
        .filter(|_| {
            actionable
                && candidate.action != "continue_or_release_claim"
                && candidate.action != "release_or_renew_claim"
                && candidate.action != "update_plan_status"
        })
        .map(|fact| suggested_claims(tool, fact))
        .unwrap_or_default();
    let suggested_commands = if actionable {
        suggested_commands(tool, candidate)
    } else {
        Vec::new()
    };
    ActionContract {
        actionable,
        requires_human: false,
        stop_reason,
        suggested_claims,
        suggested_commands,
        completion: completion_contract(candidate.action, actionable),
    }
}

fn suggested_claims(tool: &str, fact: &Fact) -> Vec<SuggestedClaim> {
    let scopes = executable_scopes(fact);
    scopes
        .into_iter()
        .map(|scope| {
            let path = command_path(&scope);
            let tool = shell_quote(tool);
            let path = shell_quote(&path);
            SuggestedClaim {
                scope,
                command: format!(
                    "rally say claim --tool {tool} --subject \"act on next\" --path {path} --json"
                ),
            }
        })
        .collect()
}

fn suggested_commands(tool: &str, candidate: &NextCandidate) -> Vec<String> {
    let Some(fact) = candidate.fact.as_ref() else {
        return Vec::new();
    };
    if candidate.action == "update_plan_status" {
        return backlog_id(fact)
            .map(|id| {
                vec![format!(
                    "rally backlog update --tool {} --id {} --status in_progress --expected-by \"<next checkpoint>\" --json",
                    shell_quote(tool),
                    shell_quote(&id),
                )]
            })
            .unwrap_or_default();
    }
    if candidate.action == "release_or_renew_claim" {
        // Release first: it is the completion this prompt exists for, and the
        // hook selects the command by that prefix. Renewal only works from the
        // session that took the claim (`latest_renewed_lease` is self-only).
        let tool_arg = shell_quote(tool);
        return vec![
            format!(
                "rally say release --tool {tool_arg} --ref {} --subject \"lease expired; released\" --json",
                shell_quote(&fact.event_id)
            ),
            format!("rally status post --tool {tool_arg} --state working --json"),
        ];
    }
    let mut commands = executable_scopes(fact)
        .into_iter()
        .map(|scope| {
            let path = command_path(&scope);
            let tool = shell_quote(tool);
            let path = shell_quote(&path);
            format!("rally check before-write --tool {tool} --path {path} --strict --json")
        })
        .collect::<Vec<_>>();
    let tool_arg = shell_quote(tool);
    let event_arg = shell_quote(&fact.event_id);
    match candidate.action {
        "respond_to_handoff" => commands.push(format!(
            "rally say resolve --tool {tool} --ref {} --subject \"responded to handoff\" --json",
            event_arg,
            tool = tool_arg
        )),
        "resolve_owned_blocker" => commands.push(format!(
            "rally say resolve --tool {tool} --ref {} --subject \"resolved blocker\" --json",
            event_arg,
            tool = tool_arg
        )),
        "claim_open_queue_item" => {
            // Read the item's current state first (read-only), then claim the
            // scope before working. Completion is a resolve that names it.
            commands.insert(
                0,
                format!("rally locate {} --json", event_arg),
            );
            commands.push(format!(
                "rally say resolve --tool {tool} --ref {} --subject \"done: <what landed>\" --evidence \"<verification>\" --json",
                event_arg,
                tool = tool_arg
            ));
        }
        "continue_or_release_claim" => commands.push(format!(
            "rally say release --tool {tool} --ref {} --subject \"done\" --json",
            event_arg,
            tool = tool_arg
        )),
        "review_artifact" => commands.push(format!(
            "rally say resolve --tool {tool} --ref {} --subject \"reviewed artifact\" --evidence \"<verification>\" --json",
            event_arg,
            tool = tool_arg
        )),
        "clarify_handoff" => commands.push(format!(
            "rally say handoff --tool {tool} --ref {} --handoff-state rejected --subject \"clarify handoff\" --summary \"<needed context>\" --json",
            event_arg,
            tool = tool_arg
        )),
        _ => {}
    }
    commands
}

fn backlog_id(fact: &Fact) -> Option<String> {
    fact.summary
        .as_deref()
        .and_then(|summary| summary.strip_prefix("id:"))
        .and_then(|rest| rest.split('\n').next())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn completion_contract(action: &str, actionable: bool) -> CompletionContract {
    let record_kind = match action {
        "respond_to_handoff" | "resolve_owned_blocker" | "claim_open_queue_item" => "resolve",
        "continue_or_release_claim" => "artifact_or_release",
        "release_or_renew_claim" => "release",
        "review_artifact" => "resolve",
        "clarify_handoff" => "handoff",
        "update_plan_status" => "backlog_update",
        _ => "none",
    };
    CompletionContract {
        record_kind,
        evidence_required: actionable && action != "update_plan_status",
        release_claims: actionable && action != "update_plan_status",
        rerun_next: actionable,
    }
}

fn executable_scopes(fact: &Fact) -> Vec<String> {
    let mut scopes = fact.scope.clone();
    if scopes.is_empty()
        && let Some(uri) = &fact.uri
    {
        if uri.starts_with("file:") {
            scopes.push(uri.clone());
        } else if !uri.contains("://") {
            scopes.push(normalize_path(uri.clone()));
        }
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn command_path(scope: &str) -> String {
    scope.strip_prefix("file:").unwrap_or(scope).to_string()
}

/// Who a handoff is addressed to, from one reader's point of view.
///
/// The open queue is BY DESIGN: a handoff with no target (or the `all`
/// broadcast) is unowned work any agent may take. It is not assigned to the
/// reader, so it must never be ranked or worded as if it were — the defect
/// `assigned_to_tool`'s `is_none_or` used to encode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandoffAddress {
    /// `target == tool`: assigned to this tool by name.
    Named,
    /// No target, a blank target, or `all`: the open queue.
    OpenQueue,
    /// Addressed to somebody else.
    Elsewhere,
}

pub(crate) fn handoff_address(fact: &Fact, tool: &str) -> HandoffAddress {
    match fact.target.as_deref().map(str::trim) {
        Some(target) if target == tool => HandoffAddress::Named,
        None | Some("") | Some("all") => HandoffAddress::OpenQueue,
        Some(_) => HandoffAddress::Elsewhere,
    }
}

fn waiting_on_peer(fact: &Fact, tool: &str) -> bool {
    fact.tool.as_deref() == Some(tool)
        && fact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all")
}

fn stale_wait_obligation(
    fact: &Fact,
    stale_targets: &BTreeSet<String>,
    stale_wait_secs: i64,
) -> bool {
    fact.target
        .as_deref()
        .is_some_and(|target| stale_targets.contains(target))
        || fact_age_secs(fact).is_some_and(|age| stale_wait_age(age, stale_wait_secs))
}

fn stale_targeted_handoff(fact: &Fact, tool: &str, stale_wait_secs: i64) -> bool {
    fact.target.as_deref() == Some(tool)
        && fact_age_secs(fact).is_some_and(|age| stale_wait_age(age, stale_wait_secs))
}

/// STRICT greater-than, matching `decay::is_archivable`: an obligation exactly
/// at the window is still active. A non-positive window falls back to the
/// default rather than marking every handoff stale.
fn stale_wait_age(age_secs: i64, stale_wait_secs: i64) -> bool {
    let window = if stale_wait_secs > 0 {
        stale_wait_secs
    } else {
        DEFAULT_STALE_WAIT_SECS
    };
    age_secs > window
}

fn fact_age_secs(fact: &Fact) -> Option<i64> {
    let seen = chrono::DateTime::parse_from_rfc3339(&fact.created_at).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - seen.timestamp())
}

fn fact_is_weak(fact: &Fact) -> bool {
    fact.summary
        .as_deref()
        .is_none_or(|summary| summary.trim().is_empty())
        && fact.evidence.is_empty()
}

fn artifact_review_priority(artifact: &Fact, tool: &str, waiting: bool) -> (&'static str, i64) {
    let directly_targeted = artifact.target.as_deref() == Some(tool);
    // `fact_requires_ack` lives in `store.rs` beside the obligation projection so
    // ranking and projection share ONE definition of "requires ack". Two copies
    // would let `next` rank an item the inbox does not hold, or the reverse.
    if directly_targeted && crate::store::fact_requires_ack(artifact) {
        return ("targeted_peer_artifact_requires_ack", 110);
    }
    if directly_targeted {
        return ("targeted_peer_artifact_requires_attention", 100);
    }
    if artifact.target.as_deref() == Some("all") && crate::store::fact_requires_ack(artifact) {
        return ("broadcast_peer_artifact_requires_ack", 90);
    }
    (
        "unconsumed_peer_artifact_can_be_checked_while_waiting",
        if waiting { 80 } else { 65 },
    )
}

fn boost_artifact_review_score(
    base: i64,
    fact: &Fact,
    role: Option<&str>,
    paths: &[String],
) -> i64 {
    boost_score(base.min(100), fact, role, paths) + base.saturating_sub(100)
}

fn boost_score(base: i64, fact: &Fact, role: Option<&str>, paths: &[String]) -> i64 {
    let role_boost = match (role, fact.kind.as_str()) {
        (Some("reviewer" | "qa"), "artifact") => 10,
        (Some("builder"), "claim") => 5,
        _ => 0,
    };
    let path_boost = if !paths.is_empty()
        && paths
            .iter()
            .any(|path| fact.scope.iter().any(|scope| scope == path))
    {
        5
    } else {
        0
    };
    (base + role_boost + path_boost).min(100)
}

/// Attention block for `rally enter`.
///
/// Two windows, on purpose:
///
/// * **Cursor-gated** (`fact.seq > cursor_before`) for claims, decisions, risks,
///   and unconsumed artifacts. Those genuinely are "new since you last looked",
///   and re-showing them on every enter is the spam this gate exists to prevent.
/// * **Never gated** for an open obligation addressed to this tool.
///   `command_enter` sets the cursor to `snapshot.max_seq` after building this
///   block, so under the cursor rule an obligation seen ONCE is never surfaced by
///   `enter` again, however long it goes unanswered (intent.md defect #2). A read
///   position records what an agent SAW, not what it ANSWERED.
pub(crate) fn build_attention(
    snapshot: &RoomSnapshot,
    tool: &str,
    cursor_before: i64,
    paths: &[String],
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    let attention_item = |reason: &'static str, fact: &Fact| AttentionItem {
        reason,
        event_id: fact.event_id.clone(),
        seq: fact.seq,
        kind: fact.kind.clone(),
        subject: fact.subject.clone(),
        scope: fact.scope.clone(),
        tool: fact.tool.clone(),
        target: fact.target.clone(),
    };
    // Scoped so `push_fact`'s mutable borrow of `items` ends before the
    // cursor-independent pass below reads `items` back.
    {
        let mut push_fact = |reason: &'static str, fact: &Fact| {
            if fact.seq > cursor_before {
                items.push(attention_item(reason, fact));
            }
        };
        for handoff in &snapshot.open_handoffs {
            match handoff_address(handoff, tool) {
                HandoffAddress::Named => push_fact("handoff_assigned", handoff),
                HandoffAddress::OpenQueue => push_fact("open_queue_item", handoff),
                HandoffAddress::Elsewhere => {}
            }
        }
        for claim in &snapshot.active_claims {
            if claim.tool.as_deref() != Some(tool)
                && (paths.is_empty()
                    || paths.iter().any(|path| {
                        claim
                            .scope
                            .iter()
                            .any(|scope| path_matches_scope(scope, path))
                    }))
            {
                push_fact("claimed_scope", claim);
            }
        }
        for fact in snapshot
            .current_decisions
            .iter()
            .chain(snapshot.current_risks.iter())
            .chain(snapshot.unconsumed_artifacts.iter())
        {
            push_fact("new_room_fact", fact);
        }
    }

    // Obligations addressed to this tool, cursor-independent. A distinct reason
    // so a caller can tell a FRESH item (`handoff_assigned` / `new_room_fact`,
    // both cursor-gated) from a PERSISTENT unanswered one.
    //
    // De-duplicated against what the loops above already pushed: an open handoff
    // newer than the cursor is both an `open_handoff` and an `open_obligation`,
    // and listing it twice would make `attention.len()` — which `enter` reports
    // as `attention_count` — overstate the work.
    let already_listed = items
        .iter()
        .map(|item| item.event_id.clone())
        .collect::<BTreeSet<_>>();
    for obligation in &snapshot.open_obligations {
        if obligation.target.as_deref() == Some(tool)
            && !already_listed.contains(&obligation.event_id)
        {
            items.push(attention_item("open_obligation", obligation));
        }
    }
    items
}

pub(crate) fn bound_enter_attention(
    snapshot: &RoomSnapshot,
    tool: &str,
    mut items: Vec<AttentionItem>,
) -> (Vec<AttentionItem>, usize, usize, usize) {
    let visible_obligation_ids = snapshot
        .open_obligations
        .iter()
        .map(|fact| fact.event_id.as_str())
        .collect::<BTreeSet<_>>();
    let built_obligations = items
        .iter()
        .filter(|item| {
            visible_obligation_ids.contains(item.event_id.as_str())
                || snapshot.open_handoffs.iter().any(|fact| {
                    fact.event_id == item.event_id && fact.target.as_deref() == Some(tool)
                })
                || snapshot.unconsumed_artifacts.iter().any(|fact| {
                    fact.event_id == item.event_id
                        && fact.target.as_deref() == Some(tool)
                        && crate::store::fact_requires_ack(fact)
                })
        })
        .count();
    let exact_obligations = if snapshot.open_obligations_target.as_deref() == Some(tool) {
        snapshot.open_obligations_total.max(built_obligations)
    } else {
        snapshot
            .open_obligations
            .iter()
            .filter(|fact| fact.target.as_deref() == Some(tool))
            .count()
            .max(built_obligations)
    };
    let non_obligation_items = items.len().saturating_sub(built_obligations);
    let total = non_obligation_items.saturating_add(exact_obligations);
    items.truncate(MAX_ENTER_ROWS);
    let emitted = items.len();
    let omitted = total.saturating_sub(emitted);
    (items, total, emitted, omitted)
}

fn entry_item(reason: &'static str, fact: &Fact) -> EntryItem {
    EntryItem {
        reason,
        event_id: fact.event_id.clone(),
        kind: fact.kind.clone(),
        subject: fact.subject.clone(),
        scope: fact.scope.clone(),
        tool: fact.tool.clone(),
        target: fact.target.clone(),
        evidence: fact.evidence.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Squad;

    fn handoff(id: &str, target: &str, created_at: &str) -> Fact {
        Fact {
            from_session_id: None,
            schema: crate::FACT_SCHEMA.to_string(),
            event_id: id.to_string(),
            seq: 1,
            thread_id: "thread-test".to_string(),
            kind: FactKind::Handoff,
            tool: Some("codex".to_string()),
            role: None,
            subject: "handoff".to_string(),
            scope: Vec::new(),
            created_at: created_at.to_string(),
            summary: Some("handoff summary".to_string()),
            evidence: Vec::new(),
            target: Some(target.to_string()),
            ref_id: None,
            status: None,
            severity: None,
            uri: None,
            session: None,
        }
    }

    fn squad(tool: &str, last_seen_ts: &str, status: &str) -> Squad {
        Squad {
            tool: tool.to_string(),
            last_seen_seq: 1,
            last_seen_ts: last_seen_ts.to_string(),
            status: status.to_string(),
            acknowledged: true,
            ..Default::default()
        }
    }

    #[test]
    fn stale_outgoing_handoff_does_not_force_wait() {
        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(handoff(
            "old-handoff",
            "claude_code:l4",
            "2000-01-01T00:00:00Z",
        ));
        snapshot
            .squads
            .push(squad("claude_code:l4", "2000-01-01T00:00:00Z", "idle"));

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );

        assert_eq!(result.action, "proceed_solo");
        assert!(
            result.waiting_on.is_empty(),
            "stale outgoing handoff must not remain a wait obligation"
        );
    }

    #[test]
    fn current_outgoing_handoff_can_still_wait() {
        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(handoff(
            "fresh-handoff",
            "claude_code:review",
            "2999-01-01T00:00:00Z",
        ));
        snapshot.squads.push(squad(
            "claude_code:review",
            "2999-01-01T00:00:00Z",
            "active",
        ));

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );

        assert_eq!(result.action, "wait");
        assert_eq!(result.waiting_on.len(), 1);
    }

    /// `Surface` — the `rally next` path. A targeted handoff older than the
    /// stale window is STILL work this agent owes an answer to.
    ///
    /// This replaces `stale_targeted_handoff_is_not_actionable`, which asserted
    /// the defect: it pinned "an inbound targeted handoff stops being actionable
    /// at 24h", which is the reader-side erasure goal.md C4 removes.
    #[test]
    fn surface_policy_keeps_an_ancient_targeted_handoff_actionable() {
        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(handoff(
            "old-targeted-handoff",
            "codex",
            "2000-01-01T00:00:00Z",
        ));
        // Named work is read from the obligation bucket, as `rally inbox` does.
        snapshot.open_obligations = snapshot.open_handoffs.clone();

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );
        assert_eq!(result.action, "respond_to_handoff");
        assert!(result.actionable, "age is not a reason to stop offering it");
        assert_eq!(
            result.target_event_id.as_deref(),
            Some("old-targeted-handoff")
        );
    }

    /// `Ignore` — the `command_self_exit_check` path. The drop is preserved
    /// there and ONLY there: without it an ancient handoff makes `actionable`
    /// permanently true, `empty_cycle` never holds, and a session holding one can
    /// never self-exit.
    #[test]
    fn ignore_policy_still_drops_an_ancient_targeted_handoff() {
        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(handoff(
            "old-targeted-handoff",
            "codex",
            "2000-01-01T00:00:00Z",
        ));
        // Named work is read from the obligation bucket, as `rally inbox` does.
        snapshot.open_obligations = snapshot.open_handoffs.clone();

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Ignore,
            &crate::open_queue::FsSignals::default(),
        );
        assert_eq!(result.action, "proceed_solo");
        assert!(
            !result.actionable,
            "self-exit must still be able to reach an empty cycle"
        );
    }

    /// The TTL arithmetic itself, independent of which policy consumes it.
    /// `stale_wait_age` still backs the OUTBOUND `stale_wait_obligation` path,
    /// which is unchanged by this work.
    #[test]
    fn stale_wait_ttl_is_strict_at_the_window() {
        assert!(!stale_wait_age(
            DEFAULT_STALE_WAIT_SECS,
            DEFAULT_STALE_WAIT_SECS
        ));
        assert!(stale_wait_age(
            DEFAULT_STALE_WAIT_SECS + 1,
            DEFAULT_STALE_WAIT_SECS
        ));
    }

    /// A malformed `created_at` fails OPEN in the age predicate: unparseable is
    /// never a reason to drop an addressed handoff, under either policy.
    #[test]
    fn bad_timestamps_fail_open_under_both_policies() {
        let malformed = handoff("bad-time", "codex", "not-a-timestamp");
        assert!(
            !stale_targeted_handoff(&malformed, "codex", DEFAULT_STALE_WAIT_SECS),
            "malformed timestamps must remain actionable"
        );

        let mut snapshot = RoomSnapshot::default();
        snapshot.open_handoffs.push(malformed.clone());
        snapshot.open_obligations.push(malformed);
        for policy in [StaleTargetedPolicy::Surface, StaleTargetedPolicy::Ignore] {
            let result = build_next(
                &snapshot,
                "codex",
                None,
                &[],
                10,
                Vec::new(),
                DEFAULT_STALE_WAIT_SECS,
                policy,
                &crate::open_queue::FsSignals::default(),
            );
            assert_eq!(result.action, "respond_to_handoff", "policy={policy:?}");
        }
    }

    /// goal.md C4, second half: `enter`'s attention block still lists a targeted
    /// open obligation after the cursor has passed it, while every other class
    /// keeps the cursor window.
    #[test]
    fn attention_surfaces_an_obligation_past_the_cursor_but_still_gates_other_facts() {
        let mut obligation = handoff("owed", "codex", "2000-01-01T00:00:00Z");
        obligation.seq = 3;
        let mut decision = handoff("seen-decision", "codex", "2000-01-01T00:00:00Z");
        decision.kind = FactKind::Decision;
        decision.seq = 4;

        let snapshot = RoomSnapshot {
            open_obligations: vec![obligation.clone()],
            current_decisions: vec![decision],
            ..RoomSnapshot::default()
        };

        // Cursor well past both facts — the "entered the room again" case.
        let items = build_attention(&snapshot, "codex", 99, &[]);
        assert_eq!(
            items
                .iter()
                .map(|item| (item.reason, item.event_id.as_str()))
                .collect::<Vec<_>>(),
            vec![("open_obligation", "owed")],
            "only the obligation bypasses the cursor gate"
        );
    }

    /// One obligation must never be listed twice. An open handoff newer than the
    /// cursor is both `handoff_assigned` and `open_obligation`; `enter` reports
    /// `attention.len()` as `attention_count`, so a duplicate would overstate the
    /// work owed.
    #[test]
    fn attention_does_not_list_one_obligation_twice() {
        let mut owed = handoff("owed", "codex", "2999-01-01T00:00:00Z");
        owed.seq = 7;
        let snapshot = RoomSnapshot {
            open_handoffs: vec![owed.clone()],
            open_obligations: vec![owed],
            ..RoomSnapshot::default()
        };

        let items = build_attention(&snapshot, "codex", 0, &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].reason, "handoff_assigned");
    }

    #[test]
    fn enter_attention_and_entry_lists_are_bounded_without_a_duplicate_copy() {
        let mut snapshot = RoomSnapshot::default();
        for index in 0..(MAX_ENTER_ROWS + 12) {
            let mut owed = handoff(&format!("owed-{index}"), "codex", "2999-01-01T00:00:00Z");
            owed.seq = index as i64 + 1;
            snapshot.open_handoffs.push(owed.clone());
            snapshot.open_obligations.push(owed);
        }

        let (attention, total, emitted, omitted) = bound_enter_attention(
            &snapshot,
            "codex",
            build_attention(&snapshot, "codex", 0, &[]),
        );
        assert_eq!(total, MAX_ENTER_ROWS + 12);
        assert_eq!(emitted, MAX_ENTER_ROWS);
        assert_eq!(omitted, 12);

        let entry = build_entry(&snapshot, "codex", None, &[]);
        let value = serde_json::to_value(entry).unwrap();
        assert!(value.get("attention").is_none(), "attention must ship once");
        assert_eq!(
            value["respond_to"].as_array().unwrap().len(),
            MAX_ENTER_ROWS
        );
        assert_eq!(value["do"].as_array().unwrap().len(), MAX_ENTER_ROWS);
        assert_eq!(attention.len(), MAX_ENTER_ROWS);
    }

    #[test]
    fn enter_attention_counts_target_transport_omissions_exactly() {
        let mut snapshot = RoomSnapshot {
            open_obligations_target: Some("codex".to_string()),
            open_obligations_total: MAX_ENTER_ROWS + 12,
            open_obligations_handoffs: 0,
            open_obligations_artifacts: MAX_ENTER_ROWS + 12,
            open_obligations_omitted: 12,
            ..RoomSnapshot::default()
        };
        for index in 0..MAX_ENTER_ROWS {
            let mut artifact = handoff(
                &format!("artifact-{index}"),
                "codex",
                "2999-01-01T00:00:00Z",
            );
            artifact.kind = FactKind::Artifact;
            artifact.evidence.push("requires_ack:true".to_string());
            artifact.seq = index as i64 + 1;
            snapshot.open_obligations.push(artifact);
        }

        let (attention, total, emitted, omitted) = bound_enter_attention(
            &snapshot,
            "codex",
            build_attention(&snapshot, "codex", i64::MAX, &[]),
        );
        assert_eq!(attention.len(), MAX_ENTER_ROWS);
        assert_eq!(total, MAX_ENTER_ROWS + 12);
        assert_eq!(emitted, MAX_ENTER_ROWS);
        assert_eq!(omitted, 12);
    }

    /// `next.inbox` rides along on the same snapshot read the ranking used.
    #[test]
    fn next_embeds_the_inbox_for_the_calling_tool() {
        let mut owed = handoff("owed", "codex", "2000-01-01T00:00:00Z");
        owed.seq = 2;
        let snapshot = RoomSnapshot {
            open_obligations: vec![owed],
            ..RoomSnapshot::default()
        };

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );
        assert_eq!(result.inbox.count, 1);
        assert_eq!(result.inbox.handoffs, 1);
        assert_eq!(result.inbox.items[0].event_id, "owed");
    }

    #[test]
    fn clarify_handoff_command_auto_binds_the_original_author() {
        let fact = handoff("bound-handoff", "receiver", "2999-01-01T00:00:00Z");
        let candidate = NextCandidate::from_fact("clarify_handoff", "weak_handoff", 1, &fact);
        let commands = suggested_commands("receiver", &candidate);
        let command = commands.last().expect("clarification command");
        assert!(command.contains("say handoff --tool receiver --ref bound-handoff"));
        assert!(command.contains("--handoff-state rejected"));
        assert!(!command.contains("--target receiver"));
    }

    #[test]
    fn targeted_planned_backlog_item_requires_status_update() {
        let snapshot = RoomSnapshot::default();
        let item = BacklogItem {
            id: "plan-1".to_string(),
            intent: "publish the ARP lane plan".to_string(),
            owns: vec!["docs/ORCHESTRATION.md".to_string()],
            depends_on: Vec::new(),
            status: "planned".to_string(),
            target: Some("codex".to_string()),
            expected_by: Some("noon".to_string()),
            tool: Some("claude_code".to_string()),
            created_at: "2026-07-02T12:00:00Z".to_string(),
            event_id: "backlog-plan-1".to_string(),
            seq: 42,
        };

        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            vec![item],
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );

        assert_eq!(result.action, "update_plan_status");
        assert!(result.actionable);
        assert_eq!(result.target_event_id.as_deref(), Some("backlog-plan-1"));
        assert!(
            result
                .suggested_commands
                .iter()
                .any(|command| command.contains("rally backlog update --tool codex"))
        );
        assert!(result.suggested_claims.is_empty());
        assert!(
            result
                .suggested_commands
                .iter()
                .all(|command| !command.contains("before-write"))
        );
        assert_eq!(result.completion.record_kind, "backlog_update");
        assert!(!result.completion.release_claims);
    }

    fn ranked_squad(tool: &str, freshness: &str, age_secs: Option<i64>) -> Squad {
        Squad {
            tool: tool.to_string(),
            last_seen_seq: 1,
            last_seen_ts: "2026-08-15T00:00:00Z".to_string(),
            status: "idle".to_string(),
            acknowledged: false,
            age_secs,
            window_secs: 1860,
            freshness: freshness.to_string(),
        }
    }

    /// GOLDEN: `next.peer_targets` ranks visible peers freshest-first, excludes
    /// the caller, tallies the whole room, and caps the inline list — the
    /// counts still cover every peer, and `truncated` says how many were cut.
    #[test]
    fn peer_targets_rank_fresh_first_and_cap_the_shortlist() {
        let snapshot = RoomSnapshot {
            squads: vec![
                ranked_squad("stale-a", crate::store::FRESHNESS_STALE, Some(80_000)),
                ranked_squad("fresh-slow", crate::store::FRESHNESS_FRESH, Some(900)),
                ranked_squad("codex", crate::store::FRESHNESS_FRESH, Some(1)),
                ranked_squad("fresh-quick", crate::store::FRESHNESS_FRESH, Some(30)),
                ranked_squad("mystery", crate::store::FRESHNESS_UNKNOWN, None),
                ranked_squad("stale-b", crate::store::FRESHNESS_STALE, Some(4_000)),
            ],
            ..RoomSnapshot::default()
        };

        let all = peer_targets(&snapshot, "codex", PEER_TARGETS_LIMIT);
        assert_eq!((all.fresh, all.stale, all.unknown), (2, 2, 1));
        assert_eq!(all.truncated, 0);
        assert_eq!(
            all.ranked
                .iter()
                .map(|p| p.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh-quick", "fresh-slow", "mystery", "stale-b", "stale-a"]
        );
        assert!(
            all.ranked.iter().all(|p| p.tool != "codex"),
            "the caller is never its own handoff target"
        );

        let capped = peer_targets(&snapshot, "codex", 2);
        assert_eq!(
            capped
                .ranked
                .iter()
                .map(|p| p.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh-quick", "fresh-slow"]
        );
        assert_eq!(
            capped.truncated, 3,
            "cut rows are counted, not silently dropped"
        );
        assert_eq!(
            (capped.fresh, capped.stale, capped.unknown),
            (2, 2, 1),
            "tallies cover the whole room regardless of the cap"
        );

        // And it rides along in the full `next` result.
        let result = build_next(
            &snapshot,
            "codex",
            None,
            &[],
            10,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            StaleTargetedPolicy::Surface,
            &crate::open_queue::FsSignals::default(),
        );
        assert_eq!(result.peer_targets.ranked[0].tool, "fresh-quick");
        assert_eq!(result.peer_targets.stale, 2);
    }

    fn open_queue_handoff(id: &str, seq: i64, created_at: &str) -> Fact {
        let mut fact = handoff(id, "unused", created_at);
        fact.target = None;
        fact.seq = seq;
        // Posted by a peer: an agent is never offered its own posting.
        fact.tool = Some("claude_code:01".to_string());
        fact
    }

    fn named_handoff(id: &str, seq: i64) -> Fact {
        let mut fact = handoff(id, "codex", &chrono::Utc::now().to_rfc3339());
        fact.seq = seq;
        fact.tool = Some("claude_code:01".to_string());
        fact
    }

    fn next_for(snapshot: &RoomSnapshot, policy: StaleTargetedPolicy) -> NextResult {
        build_next(
            snapshot,
            "codex",
            None,
            &[],
            20,
            Vec::new(),
            DEFAULT_STALE_WAIT_SECS,
            policy,
            &crate::open_queue::FsSignals::default(),
        )
    }

    /// Acceptance 1: work assigned BY NAME outranks the open queue, and the open
    /// queue surfaces once nothing is assigned.
    #[test]
    fn named_assignment_outranks_open_queue_which_surfaces_when_idle() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let pool = open_queue_handoff("pool-item", 9, &fresh);
        let named = named_handoff("named-item", 1);
        let snapshot = RoomSnapshot {
            open_handoffs: vec![pool.clone(), named.clone()],
            open_obligations: vec![named],
            ..RoomSnapshot::default()
        };
        let result = next_for(&snapshot, StaleTargetedPolicy::Surface);
        assert_eq!(result.action, "respond_to_handoff");
        assert_eq!(result.target_event_id.as_deref(), Some("named-item"));
        assert_eq!(result.alternatives[0].action, "claim_open_queue_item");

        let idle = RoomSnapshot {
            open_handoffs: vec![pool],
            ..RoomSnapshot::default()
        };
        let result = next_for(&idle, StaleTargetedPolicy::Surface);
        assert_eq!(result.action, "claim_open_queue_item");
        assert_eq!(result.reason, "open_queue_item_available");
        assert!(result.actionable);
        assert!(
            result.suggested_commands[0].starts_with("rally locate pool-item"),
            "read current state before claiming: {:?}",
            result.suggested_commands
        );
        assert_ne!(
            result.reason, "open_handoff_targeted_to_this_tool",
            "an untargeted item is never reported as targeted"
        );
    }

    /// Acceptance: no age limit hides an open-queue item, under either policy;
    /// age only marks it "revalidate before claiming".
    #[test]
    fn ancient_open_queue_item_is_revalidated_not_hidden() {
        let snapshot = RoomSnapshot {
            open_handoffs: vec![open_queue_handoff("fact_97f3", 1, "2026-06-04T21:35:56Z")],
            ..RoomSnapshot::default()
        };
        for policy in [StaleTargetedPolicy::Surface, StaleTargetedPolicy::Ignore] {
            let result = next_for(&snapshot, policy);
            assert_eq!(result.action, "claim_open_queue_item", "policy={policy:?}");
            assert_eq!(result.reason, "open_queue_item_needs_revalidation");
            assert_eq!(result.inbox.open_queue.count, 1);
            assert!(result.inbox.open_queue.items[0].revalidate_before_claiming);
            assert_eq!(result.inbox.count, 0, "not counted as owed by name");
        }
    }

    /// Acceptance 5: `next` and `inbox` read one obligation projection. On the
    /// same snapshot, every named handoff `next` ranks is an inbox item, every
    /// open-queue item `next` ranks is in the inbox open queue, and the inbox
    /// `next` embeds equals the standalone inbox.
    #[test]
    fn next_and_inbox_agree_on_one_snapshot() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let named_a = named_handoff("named-a", 2);
        let named_b = named_handoff("named-b", 3);
        // Closed by its sender but never acked by the receiver: still owed.
        let sender_closed = named_handoff("named-sender-closed", 4);
        let snapshot = RoomSnapshot {
            open_handoffs: vec![
                named_a.clone(),
                named_b.clone(),
                open_queue_handoff("pool-1", 5, &fresh),
                open_queue_handoff("pool-old", 6, "2000-01-01T00:00:00Z"),
            ],
            open_obligations: vec![named_a, named_b, sender_closed],
            ..RoomSnapshot::default()
        };
        let next = next_for(&snapshot, StaleTargetedPolicy::Surface);
        let ranked = std::iter::once((next.action, next.target_event_id.clone()))
            .chain(
                next.alternatives
                    .iter()
                    .map(|alt| (alt.action, alt.target_event_id.clone())),
            )
            .collect::<Vec<_>>();
        let ids_for = |action: &str| {
            let mut ids = ranked
                .iter()
                .filter(|(a, _)| *a == action)
                .filter_map(|(_, id)| id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };

        let inbox = crate::obligations::build_inbox(
            &snapshot,
            "codex",
            20,
            DEFAULT_STALE_WAIT_SECS,
            crate::open_queue::open_queue_items(
                &snapshot,
                "codex",
                &crate::open_queue::FsSignals::default(),
                chrono::Utc::now().timestamp(),
            ),
        );
        let mut inbox_named = inbox
            .items
            .iter()
            .filter(|item| item.kind == FactKind::Handoff)
            .map(|item| item.event_id.clone())
            .collect::<Vec<_>>();
        inbox_named.sort();
        let mut inbox_pool = inbox
            .open_queue
            .items
            .iter()
            .map(|item| item.event_id.clone())
            .collect::<Vec<_>>();
        inbox_pool.sort();

        assert_eq!(ids_for("respond_to_handoff"), inbox_named);
        assert_eq!(inbox_named.len(), 3, "sender-closed handoff is still owed");
        assert_eq!(ids_for("claim_open_queue_item"), inbox_pool);
        assert_eq!(next.inbox.count, inbox.count);
        assert_eq!(next.inbox.open_queue.count, inbox.open_queue.count);
        assert_eq!(next.inbox.open_queue.revalidate, 1);
    }

    fn owned_claim(id: &str, lease: &str) -> Fact {
        let mut fact = handoff(id, "unused", "2026-10-02T14:27:51Z");
        fact.kind = FactKind::Claim;
        fact.target = None;
        fact.scope = vec!["file:src/a.rs".to_string()];
        fact.evidence = vec![format!("lease_expires_at:{lease}")];
        fact
    }

    /// Acceptance 4: a claim past `lease_expires_at` with no renewal is not
    /// "still active"; it is a low-rank release-or-renew prompt below the open
    /// queue. A live lease keeps the old wording.
    #[test]
    fn lease_expired_claim_is_a_low_rank_release_or_renew_prompt() {
        let expired = owned_claim("fact_dc3e", "2026-10-02T14:57:51Z");
        let live = owned_claim(
            "live",
            &(chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        );
        let snapshot = RoomSnapshot {
            active_claims: vec![expired.clone()],
            ..RoomSnapshot::default()
        };
        let result = next_for(&snapshot, StaleTargetedPolicy::Surface);
        assert_eq!(result.action, "release_or_renew_claim");
        assert_eq!(result.reason, "owned_claim_lease_expired");
        assert_ne!(result.reason, "owned_claim_is_still_active");
        assert!(
            result.suggested_commands[0]
                .starts_with("rally say release --tool codex --ref fact_dc3e")
        );
        assert!(result.suggested_claims.is_empty());

        let with_pool = RoomSnapshot {
            active_claims: vec![expired],
            open_handoffs: vec![open_queue_handoff(
                "pool",
                9,
                &chrono::Utc::now().to_rfc3339(),
            )],
            ..RoomSnapshot::default()
        };
        assert_eq!(
            next_for(&with_pool, StaleTargetedPolicy::Surface).action,
            "claim_open_queue_item"
        );

        let live_only = RoomSnapshot {
            active_claims: vec![live],
            ..RoomSnapshot::default()
        };
        let result = next_for(&live_only, StaleTargetedPolicy::Surface);
        assert_eq!(result.reason, "owned_claim_is_still_active");
    }

    /// `enter` keeps "assigned to you" and "available to anyone" apart.
    #[test]
    fn entry_and_attention_label_open_queue_items_separately() {
        let fresh = chrono::Utc::now().to_rfc3339();
        let mut named = named_handoff("named", 2);
        named.seq = 2;
        let snapshot = RoomSnapshot {
            open_handoffs: vec![named, open_queue_handoff("pool", 3, &fresh)],
            ..RoomSnapshot::default()
        };
        let entry = build_entry(&snapshot, "codex", None, &[]);
        let reasons = entry
            .respond_to
            .iter()
            .map(|item| (item.reason, item.event_id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![("respond_to_handoff", "named"), ("open_queue_item", "pool")]
        );
        let attention = build_attention(&snapshot, "codex", 0, &[]);
        assert!(
            attention
                .iter()
                .any(|item| item.reason == "open_queue_item" && item.event_id == "pool")
        );
        assert!(
            !attention
                .iter()
                .any(|item| item.reason == "handoff_assigned" && item.event_id == "pool")
        );
    }
}
