use schemars::JsonSchema;
use serde::Serialize;

use crate::store::{Fact, FactKind, RoomSnapshot};
use crate::{normalize_path, path_matches_scope, shell_quote};

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
    attention: Vec<AttentionItem>,
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
    actionable: bool,
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
    /// Active claims owned by this tool whose declared base is being changed by
    /// another agent — surfaced before the conflict reaches a merge.
    stale_bases: Vec<StaleBase>,
    /// Soft gate: true when at least one stale base is confirmed (pins differ).
    /// The agent should reconcile via the finding's suggested_command before
    /// building further. Advisory — it does not force `requires_human`.
    coordination_required: bool,
    alternatives: Vec<NextCandidateData>,
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

/// A consumer claim whose declared `depends` contract is being changed by
/// another active producer claim. This is the predictive collision signal:
/// the consumer's base is shifting *before* a merge surfaces the conflict.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct StaleBase {
    /// Bare contract name shared by the producer and consumer (pin stripped).
    pub(crate) contract: String,
    pub(crate) consumer_event_id: String,
    pub(crate) consumer_tool: Option<String>,
    pub(crate) consumer_subject: String,
    pub(crate) consumer_pin: Option<String>,
    pub(crate) producer_event_id: String,
    pub(crate) producer_tool: Option<String>,
    pub(crate) producer_pin: Option<String>,
    /// True when both sides pinned a version (`name@hash`) and the hashes
    /// differ — a confirmed stale base, not just an overlapping contract.
    pub(crate) confirmed: bool,
    /// The cheap coordinating move: raise a blocker referencing the producer so
    /// the change is reconciled before the consumer builds further.
    pub(crate) suggested_command: String,
}

/// Split a contract token `name@hash` into its bare name and optional pin.
fn contract_parts(token: &str) -> (&str, Option<&str>) {
    match token.split_once('@') {
        Some((name, pin)) => (name, Some(pin)),
        None => (token, None),
    }
}

/// Cross-match active claims: any claim that `depends` on a contract another
/// active claim (owned by a different tool) `produces` is flagged. Rally only
/// matches declared strings — agents own the reasoning about what a contract is.
pub(crate) fn stale_base_findings(active_claims: &[Fact]) -> Vec<StaleBase> {
    let mut findings = Vec::new();
    for consumer in active_claims {
        for dep in &consumer.depends {
            let (dep_name, dep_pin) = contract_parts(dep);
            // Defense in depth: facts written before validation (or by another
            // tool) could carry an empty contract name; never match on it.
            if dep_name.is_empty() {
                continue;
            }
            for producer in active_claims {
                if producer.event_id == consumer.event_id {
                    continue;
                }
                // Coordination is cross-agent: a tool changing its own base is not a collision.
                if producer.tool.is_some() && producer.tool == consumer.tool {
                    continue;
                }
                for prod in &producer.produces {
                    let (prod_name, prod_pin) = contract_parts(prod);
                    if prod_name != dep_name {
                        continue;
                    }
                    let confirmed = matches!((dep_pin, prod_pin), (Some(d), Some(p)) if d != p);
                    let severity = if confirmed { "high" } else { "medium" };
                    let consumer_tool_arg = shell_quote(consumer.tool.as_deref().unwrap_or("you"));
                    let producer_arg = shell_quote(&producer.event_id);
                    let subject_arg = shell_quote(&format!("stale base: {dep_name}"));
                    let suggested_command = format!(
                        "rally say blocker --tool {consumer_tool_arg} --ref {producer_arg} --subject {subject_arg} --severity {severity} --json"
                    );
                    findings.push(StaleBase {
                        contract: dep_name.to_string(),
                        consumer_event_id: consumer.event_id.clone(),
                        consumer_tool: consumer.tool.clone(),
                        consumer_subject: consumer.subject.clone(),
                        consumer_pin: dep_pin.map(str::to_string),
                        producer_event_id: producer.event_id.clone(),
                        producer_tool: producer.tool.clone(),
                        producer_pin: prod_pin.map(str::to_string),
                        confirmed,
                        suggested_command,
                    });
                }
            }
        }
    }
    findings
}

pub(crate) fn build_entry(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    attention: &[AttentionItem],
) -> EntryData {
    let respond_to = snapshot
        .open_handoffs
        .iter()
        .filter(|f| {
            f.target
                .as_deref()
                .is_none_or(|target| target == tool || target == "all")
        })
        .map(|f| entry_item("respond_to_handoff", f))
        .collect::<Vec<_>>();
    let mut do_items = respond_to.clone();
    do_items.extend(
        snapshot
            .active_claims
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| entry_item("continue_or_release_claim", f)),
    );
    do_items.extend(
        snapshot
            .active_blockers
            .iter()
            .filter(|f| f.tool.as_deref() == Some(tool))
            .map(|f| entry_item("resolve_owned_blocker", f)),
    );
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
        attention: attention.to_vec(),
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
        let confidence = (self.score.clamp(5, 95) as f64) / 100.0;
        NextCandidateData {
            action: self.action,
            reason: self.reason,
            score: self.score,
            confidence,
            target_event_id,
            source_event_ids: self.source_event_ids.clone(),
            fact: self.fact.clone(),
        }
    }
}

pub(crate) fn build_next(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    limit: usize,
) -> NextResult {
    let waiting_on = waiting_on_facts(snapshot, tool);
    let waiting = !waiting_on.is_empty();
    let mut candidates = next_candidates(snapshot, tool, role, paths, waiting, &waiting_on);
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
    let stale_bases = stale_base_findings(&snapshot.active_claims)
        .into_iter()
        .filter(|finding| finding.consumer_tool.as_deref() == Some(tool))
        .collect::<Vec<_>>();
    let coordination_required = stale_bases.iter().any(|finding| finding.confirmed);

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
        stale_bases,
        coordination_required,
        alternatives,
    }
}

fn waiting_on_facts(snapshot: &RoomSnapshot, tool: &str) -> Vec<Fact> {
    snapshot
        .open_handoffs
        .iter()
        .chain(snapshot.active_blockers.iter())
        .filter(|fact| waiting_on_peer(fact, tool))
        .cloned()
        .collect()
}

fn next_candidates(
    snapshot: &RoomSnapshot,
    tool: &str,
    role: Option<&str>,
    paths: &[String],
    waiting: bool,
    waiting_on: &[Fact],
) -> Vec<NextCandidate> {
    let mut candidates = Vec::new();

    for handoff in &snapshot.open_handoffs {
        if assigned_to_tool(handoff, tool) {
            candidates.push(NextCandidate::from_fact(
                "respond_to_handoff",
                "open_handoff_targeted_to_this_tool",
                boost_score(100, handoff, role, paths),
                handoff,
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
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() == Some(tool) {
            candidates.push(NextCandidate::from_fact(
                "continue_or_release_claim",
                "owned_claim_is_still_active",
                boost_score(75, claim, role, paths),
                claim,
            ));
        }
    }
    for artifact in &snapshot.unconsumed_artifacts {
        let authored_by_self = artifact.tool.as_deref() == Some(tool);
        let routed_elsewhere = artifact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all");
        if !authored_by_self && !routed_elsewhere {
            candidates.push(NextCandidate::from_fact(
                "review_artifact",
                "unconsumed_peer_artifact_can_be_checked_while_waiting",
                boost_score(if waiting { 80 } else { 65 }, artifact, role, paths),
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

    if candidates.is_empty() {
        candidates.push(idle_next_candidate(waiting, waiting_on));
    }

    candidates
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
        .filter(|_| actionable && candidate.action != "continue_or_release_claim")
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
        "continue_or_release_claim" => commands.push(format!(
            "rally say release --tool {tool} --ref {} --subject \"done\" --json",
            event_arg,
            tool = tool_arg
        )),
        "review_artifact" => commands.push(format!(
            "rally say artifact --tool {tool} --ref {} --subject \"reviewed artifact\" --uri {} --evidence \"<verification>\" --json",
            event_arg,
            shell_quote(fact.uri.as_deref().unwrap_or("<path>")),
            tool = tool_arg
        )),
        "clarify_handoff" => commands.push(format!(
            "rally say handoff --tool {tool} --target {} --ref {} --subject \"clarify handoff\" --summary \"<needed context>\" --json",
            shell_quote(fact.target.as_deref().unwrap_or("<target-tool>")),
            event_arg,
            tool = tool_arg
        )),
        _ => {}
    }
    commands
}

fn completion_contract(action: &str, actionable: bool) -> CompletionContract {
    let record_kind = match action {
        "respond_to_handoff" | "resolve_owned_blocker" => "resolve",
        "continue_or_release_claim" => "artifact_or_release",
        "review_artifact" => "artifact",
        "clarify_handoff" => "handoff",
        _ => "none",
    };
    CompletionContract {
        record_kind,
        evidence_required: actionable,
        release_claims: actionable,
        rerun_next: actionable,
    }
}

fn executable_scopes(fact: &Fact) -> Vec<String> {
    let mut scopes = fact.scope.clone();
    if scopes.is_empty() {
        if let Some(uri) = &fact.uri {
            if uri.starts_with("file:") {
                scopes.push(uri.clone());
            } else if !uri.contains("://") {
                scopes.push(normalize_path(uri.clone()));
            }
        }
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn command_path(scope: &str) -> String {
    scope.strip_prefix("file:").unwrap_or(scope).to_string()
}

fn assigned_to_tool(fact: &Fact, tool: &str) -> bool {
    fact.target
        .as_deref()
        .is_none_or(|target| target == tool || target == "all")
}

fn waiting_on_peer(fact: &Fact, tool: &str) -> bool {
    fact.tool.as_deref() == Some(tool)
        && fact
            .target
            .as_deref()
            .is_some_and(|target| target != tool && target != "all")
}

fn fact_is_weak(fact: &Fact) -> bool {
    fact.summary
        .as_deref()
        .is_none_or(|summary| summary.trim().is_empty())
        && fact.evidence.is_empty()
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

pub(crate) fn build_attention(
    snapshot: &RoomSnapshot,
    tool: &str,
    cursor_before: i64,
    paths: &[String],
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    let mut push_fact = |reason: &'static str, fact: &Fact| {
        if fact.seq > cursor_before {
            items.push(AttentionItem {
                reason,
                event_id: fact.event_id.clone(),
                seq: fact.seq,
                kind: fact.kind.clone(),
                subject: fact.subject.clone(),
                scope: fact.scope.clone(),
                tool: fact.tool.clone(),
                target: fact.target.clone(),
            });
        }
    };
    for handoff in &snapshot.open_handoffs {
        if handoff
            .target
            .as_deref()
            .is_none_or(|target| target == tool || target == "all")
        {
            push_fact("handoff_assigned", handoff);
        }
    }
    for claim in &snapshot.active_claims {
        if claim.tool.as_deref() != Some(tool)
            && (paths.is_empty() || paths.iter().any(|path| claim.scope.contains(path)))
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
    for finding in stale_base_findings(&snapshot.active_claims) {
        if finding.consumer_tool.as_deref() != Some(tool) {
            continue;
        }
        if let Some(producer) = snapshot
            .active_claims
            .iter()
            .find(|claim| claim.event_id == finding.producer_event_id)
        {
            // A live collision is surfaced every enter until resolved, even when
            // the producer claim predates this tool's cursor — it is a derived
            // finding, not a since-cursor "new fact".
            items.push(AttentionItem {
                reason: "stale_base",
                event_id: producer.event_id.clone(),
                seq: producer.seq,
                kind: producer.kind.clone(),
                subject: producer.subject.clone(),
                scope: producer.scope.clone(),
                tool: producer.tool.clone(),
                target: producer.target.clone(),
            });
        }
    }
    items
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
