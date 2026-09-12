// SPDX-License-Identifier: Apache-2.0
//! One capability projection for humans and agents. Terminal echo is never an ACK.
use crate::backends::{Backend, BackendRunner, SessionLiveness, SessionView};
use crate::cli::{BackendBins, CliCommand, CliParse};
use crate::error::{RallyError, Result};
use crate::output::Output;
use crate::store::{Fact, FactKind, RoomStore};
use bpaf::{Parser, construct, long};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MARKER: &str = "route-probe-v1:";
const MAX_ROUTES: usize = 128;
const TRANSPORT_MARKER: &str = "route-probe-transport-v1:";

#[derive(Clone, Debug)]
pub(crate) struct RoutesArgs {
    pub json: bool,
    pub probe: Option<String>,
    pub tool: Option<String>,
    pub timeout_seconds: u64,
}

pub(crate) fn parser() -> impl Parser<RoutesArgs> {
    let json = long("json").switch();
    let probe = long("probe").help("Send an explicit connection-check handoff to this exact managed actor; requires --tool.").argument::<String>("ACTOR").optional();
    let tool = long("tool").argument::<String>("SENDER").optional();
    let timeout_seconds = long("timeout-seconds")
        .argument::<u64>("SECONDS")
        .fallback(20)
        .guard(|n| (1..=120).contains(n), "timeout must be 1..120 seconds");
    construct!(RoutesArgs {
        json,
        probe,
        tool,
        timeout_seconds
    })
}

pub(crate) fn watchdog_budget(args: &[String]) -> std::time::Duration {
    // Status still has a process-level bound when a backend call stalls.
    // Explicit probes add their receiver wait to the discovery/write budget.
    let probe_wait = if args
        .iter()
        .any(|a| a == "--probe" || a.starts_with("--probe="))
    {
        args.iter()
            .find_map(|arg| {
                arg.strip_prefix("--timeout-seconds=")
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .or_else(|| {
                args.windows(2)
                    .find(|pair| pair[0] == "--timeout-seconds")
                    .and_then(|pair| pair[1].parse::<u64>().ok())
            })
            .unwrap_or(20)
            .clamp(1, 120)
    } else {
        0
    };
    std::time::Duration::from_secs(20 + probe_wait)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Probe {
    nonce: String,
    delivery_digest: String,
    endpoint: String,
    lead_epoch: Option<i64>,
    role: Option<String>,
    expires_at: i64,
}

fn role_for(facts: &[Fact], tool: &str, lead: Option<&str>) -> Option<String> {
    if lead == Some(tool) {
        return Some("lead".into());
    }
    let withdrawn = crate::retraction::retracted_ids(facts);
    facts
        .iter()
        .rev()
        .find(|f| {
            f.tool.as_deref() == Some(tool)
                && matches!(f.kind, FactKind::Presence | FactKind::Session)
                && f.role.is_some()
                && !withdrawn.contains(&f.event_id)
        })
        .and_then(|f| f.role.clone())
}

fn endpoint(view: &SessionView) -> String {
    let session = &view.session;
    crate::runtime_setup::digest(
        &serde_json::to_vec(&json!({
            "session_id":session.session_id,"tool":session.tool,"backend":session.backend,
            "target":session.target,"cwd":session.cwd,"tmux_binding":session.tmux_binding,
            "daemon_socket":session.daemon_socket,"daemon_pane":session.daemon_pane,
            "daemon_registered":session.daemon_registered,"adapter_id":session.adapter_id
        }))
        .unwrap(),
    )
}

fn connection(view: &SessionView, bins: BackendBins) -> std::result::Result<(), String> {
    if view.session.task_scoped {
        return Err("Bounded task workers do not accept later prompts".into());
    }
    if view.liveness != SessionLiveness::Live {
        return Err(format!("Session liveness is {}", view.inject_status));
    }
    let backend = Backend::parse(&view.session.backend).map_err(|e| e.to_string())?;
    let mut runner = BackendRunner::new(backend, bins);
    runner.pin_ptyd_socket(view.session.daemon_socket.as_deref());
    // Tmux live_target checks bound process, mode, input and synchronization.
    runner
        .live_target(&view.session)
        .map_err(|e| e.to_string())?;
    if backend == Backend::Tmux && view.session.tmux_binding.is_none() {
        return Err("Legacy unbound pane; adopt a concrete endpoint before probing".into());
    }
    if (backend.is_ptyd() || view.session.daemon_registered)
        && !view
            .session
            .daemon_socket
            .as_deref()
            .is_some_and(crate::daemon_client::socket_is_live)
    {
        return Err("Bound ptyd endpoint is unavailable".into());
    }
    Ok(())
}

fn probe_of(fact: &Fact) -> Option<Probe> {
    fact.evidence.iter().find_map(|s| {
        s.strip_prefix(MARKER)
            .and_then(|s| serde_json::from_str(s).ok())
    })
}

fn proof<'a>(
    facts: &'a [Fact],
    view: &SessionView,
    lead_epoch: Option<i64>,
    role: Option<&str>,
    now: i64,
) -> Option<(&'a Fact, &'a Fact, Probe)> {
    let withdrawn = crate::retraction::retracted_ids(facts);
    let expected = endpoint(view);
    // A newer failed probe invalidates an older success for the same endpoint.
    let (request, probe) = facts
        .iter()
        .rev()
        .filter(|f| {
            f.kind == FactKind::Handoff
                && f.target.as_deref() == Some(&view.session.tool)
                && !withdrawn.contains(&f.event_id)
        })
        .filter_map(|f| probe_of(f).map(|p| (f, p)))
        .find(|(_, p)| p.endpoint == expected)?;
    let session = crate::managed_protocol_session_id(&view.session.session_id, &view.session.tool);
    if crate::store::strict_handoff_target_session(request) != Some(session.as_str()) {
        return None;
    }
    if probe.expires_at <= now
        || probe.expires_at > now + 300
        || probe.lead_epoch != lead_epoch
        || probe.role.as_deref() != role
    {
        return None;
    }
    let epoch = format!(
        "lead-epoch:{}",
        lead_epoch
            .map(|n| n.to_string())
            .unwrap_or_else(|| "none".into())
    );
    // Receiver polling alone cannot establish that this transport sent the probe.
    // Require a separate observation of the same attempt; it never substitutes for ACK.
    let sent_marker = format!("{TRANSPORT_MARKER}{}", probe.endpoint);
    facts.iter().find(|f| {
        f.kind == FactKind::Artifact
            && f.seq > request.seq
            && !withdrawn.contains(&f.event_id)
            && f.ref_id.as_deref() == Some(request.event_id.as_str())
            && f.tool == request.tool
            && f.from_session_id == request.from_session_id
            && f.evidence.contains(&sent_marker)
    })?;
    let response = facts.iter().find(|f| {
        !withdrawn.contains(&f.event_id)
            && matches!(f.kind, FactKind::Artifact | FactKind::Receipt)
            && crate::receiver_response_matches(request, f, &view.session.tool)
            && f.evidence.iter().any(|s| s == &probe.nonce)
            && f.evidence.iter().any(|s| {
                s.starts_with("RALLY_ROUTE_DELIVERY_")
                    && crate::runtime_setup::digest(s.as_bytes()) == probe.delivery_digest
            })
            && f.evidence.iter().any(|s| s == &epoch)
            && (role.is_none() || f.role.as_deref() == role)
    })?;
    Some((request, response, probe))
}

fn route(
    view: &SessionView,
    facts: &[Fact],
    lead_epoch: Option<i64>,
    role: Option<&str>,
    bins: BackendBins,
    now: i64,
) -> Value {
    let transport = connection(view, bins);
    let receipt = proof(facts, view, lead_epoch, role, now);
    let ready = transport.is_ok() && receipt.is_some();
    let state = if ready {
        "ready"
    } else if transport.is_ok() {
        "testing_required"
    } else {
        "blocked"
    };
    let reason = match transport.as_ref() {
        Err(e) => e.clone(),
        Ok(()) if !ready => {
            "Transport responds; current receiver acknowledgement has not been proved".into()
        }
        Ok(()) => "Current endpoint returned the exact connection probe".into(),
    };
    json!({"actor":view.session.tool,"role":role,"session_id":view.session.session_id,"endpoint_generation":endpoint(view),"host":view.session.agent,"transport":view.inject_via,"lead_epoch":lead_epoch,"state":state,"reason":reason,"capabilities":{"ledger_queue":true,"transport_connected":transport.is_ok(),"live_prompt":ready,"receiver_ack":ready,"build_return_verified":false},"receipt":receipt.map(|(request,response,p)|json!({"request":request.event_id,"response":response.event_id,"expires_at":p.expires_at})),"next_action":if ready {json!({"command":"rally inject","target":view.session.tool,"require_ack":true,"handoff_required":true})} else {json!({"command":"rally routes","probe":view.session.tool,"sender_required":true})}})
}

/// Reuse the real parsers/handlers without recursively resetting append evidence.
fn say(args: Vec<String>) -> Result<Output> {
    match crate::cli::parse_cli(&args)? {
        CliParse::Command(c) => match *c {
            CliCommand::Say(s) => crate::command_say(s),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn run_probe(args: &RoutesArgs, room: &RoomStore, views: &[SessionView]) -> Result<Value> {
    let target = args.probe.as_deref().unwrap();
    let sender = args
        .tool
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RallyError::Usage("A connection probe requires --tool <sender>".into()))?;
    rally_protocol::ledger::validate_agent_id(sender)
        .map_err(|e| RallyError::Usage(format!("invalid sender: {e}")))?;
    if sender == crate::SYSTEM_TOOL {
        return Err(RallyError::Usage(
            "The system actor cannot author a connection probe".into(),
        ));
    }
    let matches: Vec<_> = views.iter().filter(|v| v.session.tool == target).collect();
    if matches.len() != 1 {
        return Err(RallyError::Usage(
            "Choose an actor with exactly one managed session; no target was guessed".into(),
        ));
    }
    let view = matches[0];
    connection(view, BackendBins::default()).map_err(RallyError::NotStarted)?;
    let authorization =
        crate::inject_authorization(sender, target, rally_protocol::MessageIntent::Directive);
    if let Some(refusal) = authorization.refusal {
        return Err(RallyError::Usage(refusal));
    }
    let facts = room.facts()?;
    let registration = facts
        .iter()
        .rev()
        .find(|fact| {
            fact.kind == FactKind::Session
                && fact.session.as_ref().is_some_and(|session| {
                    session.session_id == view.session.session_id && session.tool == target
                })
        })
        .ok_or_else(|| {
            RallyError::NotStarted(
                "Managed session registration is unavailable; rebind before probing".into(),
            )
        })?;
    // First participation can elect a lead. Freeze the probe epoch only after
    // that normal admission transition, so a fresh-room probe can pass.
    crate::ensure_presence(room, sender)?;
    let snapshot = room.snapshot()?;
    let role = role_for(&room.facts()?, target, snapshot.lead.as_deref());
    let nonce = format!("RALLY_CONNECTION_{}", uuid::Uuid::new_v4().simple());
    let delivery_token = format!("RALLY_ROUTE_DELIVERY_{}", uuid::Uuid::new_v4().simple());
    let probe = Probe {
        nonce: nonce.clone(),
        delivery_digest: crate::runtime_setup::digest(delivery_token.as_bytes()),
        endpoint: endpoint(view),
        lead_epoch: snapshot.lead_epoch,
        role: role.clone(),
        expires_at: chrono::Utc::now().timestamp() + 300,
    };
    let epoch = format!(
        "lead-epoch:{}",
        snapshot
            .lead_epoch
            .map(|n| n.to_string())
            .unwrap_or_else(|| "none".into())
    );
    let instructions = format!(
        "Connection check only. Do not edit files or start other work. Post an artifact referencing this handoff with --evidence {nonce} --evidence {epoch}{} and your exact --tool {target}. Also echo the RALLY_ROUTE_DELIVERY_ token from the live injected prompt via --evidence. If you only polled this handoff and have no delivery token, do not claim live delivery. This proves receipt only, not task completion.",
        role.as_ref()
            .map(|r| format!(" --role {}", crate::shell_quote(r)))
            .unwrap_or_default()
    );
    let posted = say(vec![
        "say".into(),
        "handoff".into(),
        "--ref".into(),
        registration.event_id.clone(),
        "--target-policy".into(),
        "third-party".into(),
        "--tool".into(),
        sender.into(),
        "--target".into(),
        view.session.session_id.clone(),
        "--subject".into(),
        format!("Connection check {nonce}"),
        "--summary".into(),
        instructions,
        "--evidence".into(),
        format!("{MARKER}{}", serde_json::to_string(&probe).unwrap()),
        "--json".into(),
    ])?;
    let request = posted.body["data"]["say"]["fact"]["event_id"].as_str().ok_or_else(||RallyError::Command("Probe handoff committed but its ID was unavailable; inspect the ledger before retrying".into()))?.to_string();
    let invoke = vec![
        "inject".into(),
        target.into(),
        "--tool".into(),
        sender.into(),
        "--intent".into(),
        "directive".into(),
        "--handoff".into(),
        request.clone(),
        "--require-ack".into(),
        "--timeout-seconds".into(),
        args.timeout_seconds.to_string(),
        "--json".into(),
    ];
    let output = match crate::cli::parse_cli(&invoke)? {
        CliParse::Command(c) => match *c {
            CliCommand::Inject(a) => crate::command_inject_probe(a, delivery_token)?,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let attempt = &output.body["data"]["inject"];
    if attempt["mode"] == "inject"
        && matches!(
            attempt["delivery_state"].as_str(),
            Some("sent_unverified" | "delivered")
        )
    {
        say(vec![
            "say".into(),
            "artifact".into(),
            "--tool".into(),
            sender.into(),
            "--ref".into(),
            request.clone(),
            "--subject".into(),
            "Connection probe transport observation".into(),
            "--evidence".into(),
            format!("{TRANSPORT_MARKER}{}", probe.endpoint),
            "--summary".into(),
            "Sender observed a transport write. This is not a receiver acknowledgement.".into(),
            "--json".into(),
        ])?;
    }
    Ok(json!({"request":request,"nonce":nonce,"injection":{
            "mode":attempt["mode"],"delivery_state":attempt["delivery_state"],
            "delivery_reason":attempt["delivery_reason"],"ack_state":attempt["ack_state"],
            "verified_received":attempt["verified_received"],"ack":attempt["ack"],
            "daemon_delivery_error":attempt["daemon_delivery_error"]
        },"note":"Only the subsequently validated receiver proof enables the route; no automatic resend."}))
}

pub(crate) fn command(args: RoutesArgs) -> Result<Output> {
    let room = RoomStore::open()?;
    let mut views = crate::read_session_views(&room, BackendBins::default())?;
    let probe = if args.probe.is_some() {
        Some(run_probe(&args, &room, &views)?)
    } else {
        None
    };
    if probe.is_some() {
        views = crate::read_session_views(&room, BackendBins::default())?;
    }
    let snapshot = room.snapshot()?;
    let facts = room.facts()?;
    let now = chrono::Utc::now().timestamp();
    let mut routes = vec![];
    // Put the explicitly tested target in the bounded projection.
    views.sort_by_key(|v| args.probe.as_deref() != Some(v.session.tool.as_str()));
    for view in views.iter().take(MAX_ROUTES) {
        let role = role_for(&facts, &view.session.tool, snapshot.lead.as_deref());
        routes.push(route(
            view,
            &facts,
            snapshot.lead_epoch,
            role.as_deref(),
            BackendBins::default(),
            now,
        ));
    }
    let managed: BTreeSet<_> = views.iter().map(|v| v.session.tool.as_str()).collect();
    let polling_total = snapshot
        .squads
        .iter()
        .filter(|s| !managed.contains(s.tool.as_str()))
        .count();
    for s in &snapshot.squads {
        if routes.len() >= MAX_ROUTES {
            break;
        }
        if !managed.contains(s.tool.as_str()) {
            routes.push(json!({"actor":s.tool,"role":role_for(&facts,&s.tool,snapshot.lead.as_deref()),"state":"polling_only","reason":"No registered live transport; durable queue only","capabilities":{"ledger_queue":true,"live_prompt":false,"receiver_ack":false},"next_action":{"command":"rally next","tool":s.tool}}));
        }
    }
    let total = views.len() + polling_total;
    let passed = args.probe.as_ref().is_none_or(|target| {
        routes
            .iter()
            .any(|r| r["actor"] == *target && r["state"] == "ready")
    });
    let mut text = String::from("Available routes (receiver proof expires after five minutes):");
    for r in &routes {
        text.push_str(&format!(
            "\n{}: {} — {}",
            r["actor"].as_str().unwrap_or("?"),
            r["state"].as_str().unwrap_or("unknown"),
            r["reason"].as_str().unwrap_or("")
        ));
    }
    text.push_str("\nUse rally setup for missing runtimes; rally routes --probe <actor> --tool <sender> tests a live agent connection.");
    let body = crate::envelope_value(
        "routes",
        "agent-rally.command.routes.v1",
        json!({"routes":{"items":routes,"total":total,"omitted":total.saturating_sub(MAX_ROUTES),"observed_at":now,"lead":snapshot.lead,"lead_epoch":snapshot.lead_epoch,"probe":probe,"policy":"Revalidate on send. Queued or transport-sent is not receiver acknowledgement."}}),
    )?;
    Ok(Output::new(args.json, text, body).with_exit_code(if passed { 0 } else { 4 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (SessionView, Vec<Fact>) {
        let session = serde_json::from_value(json!({"session_id":"test-generation","name":"worker","agent":"any-host","tool":"worker:one","backend":"tmux","cwd":"/tmp","target":"%1"})).unwrap();
        let view = SessionView {
            session,
            liveness: SessionLiveness::Live,
            liveness_source: "fixture",
            injectable: true,
            inject_status: "live".into(),
            inject_via: "tmux".into(),
        };
        let bound =
            crate::managed_protocol_session_id(&view.session.session_id, &view.session.tool);
        let p = Probe {
            nonce: "unique-probe".into(),
            delivery_digest: crate::runtime_setup::digest(b"RALLY_ROUTE_DELIVERY_fixture"),
            endpoint: endpoint(&view),
            lead_epoch: Some(7),
            role: Some("worker".into()),
            expires_at: 1200,
        };
        let request = Fact {
            kind: FactKind::Handoff,
            event_id: "request".into(),
            ref_id: Some("registration".into()),
            seq: 1,
            target: Some(view.session.tool.clone()),
            tool: Some("sender".into()),
            from_session_id: Some("sender-session".into()),
            evidence: vec![
                format!("{MARKER}{}", serde_json::to_string(&p).unwrap()),
                "protocol:bridge_version=fact-v1".into(),
                "protocol:event_kind=handoff.requested".into(),
                "protocol:target_policy=third-party".into(),
                "protocol:ref_event_id=registration".into(),
                "protocol:causation_id=registration".into(),
                "protocol:correlation_id=registration".into(),
                "protocol:handoff_id=registration".into(),
                "protocol:idempotency_key=connection-fixture".into(),
                format!("protocol:to_session_id={bound}"),
            ],
            ..Default::default()
        };
        let response = Fact {
            kind: FactKind::Artifact,
            event_id: "response".into(),
            seq: 2,
            tool: Some(view.session.tool.clone()),
            from_session_id: Some(bound),
            role: Some("worker".into()),
            ref_id: Some(request.event_id.clone()),
            evidence: vec![
                p.nonce,
                "lead-epoch:7".into(),
                "RALLY_ROUTE_DELIVERY_fixture".into(),
            ],
            ..Default::default()
        };
        let sent = Fact {
            kind: FactKind::Artifact,
            event_id: "send-observation".into(),
            seq: 3,
            tool: request.tool.clone(),
            from_session_id: request.from_session_id.clone(),
            ref_id: Some(request.event_id.clone()),
            evidence: vec![format!("{TRANSPORT_MARKER}{}", endpoint(&view))],
            ..Default::default()
        };
        (view, vec![request, response, sent])
    }
    #[test]
    fn only_correlated_current_receiver_proof_passes() {
        let (view, facts) = fixture();
        assert!(proof(&facts, &view, Some(7), Some("worker"), 1000).is_some());
        assert!(proof(&facts, &view, Some(8), Some("worker"), 1000).is_none());
        assert!(proof(&facts, &view, Some(7), Some("lead"), 1000).is_none());
        assert!(proof(&facts, &view, Some(7), Some("worker"), 1200).is_none());
        let mut changed = view.clone();
        changed.session.session_id = "restarted".into();
        assert!(proof(&facts, &changed, Some(7), Some("worker"), 1000).is_none());
        for field in [
            "actor",
            "session",
            "nonce",
            "delivery_token",
            "ledger_only",
            "epoch",
            "reference",
            "role",
            "sequence",
            "blocker",
            "unbound",
        ] {
            let mut bad = facts.clone();
            match field {
                "actor" => bad[1].tool = Some("impostor".into()),
                "session" => bad[1].from_session_id = Some("different-generation".into()),
                "nonce" => bad[1].evidence[0] = "stale-nonce".into(),
                "delivery_token" => bad[1].evidence[2] = "RALLY_ROUTE_DELIVERY_wrong".into(),
                "ledger_only" => {
                    bad[1].evidence.pop();
                }
                "epoch" => bad[1].evidence[1] = "lead-epoch:6".into(),
                "reference" => bad[1].ref_id = Some("another-request".into()),
                "role" => bad[1].role = Some("lead".into()),
                "sequence" => bad[1].seq = 0,
                "blocker" => bad[1].kind = FactKind::Blocker,
                "unbound" => bad[0].evidence.retain(|s| !s.starts_with("protocol:")),
                _ => unreachable!(),
            }
            assert!(
                proof(&bad, &view, Some(7), Some("worker"), 1000).is_none(),
                "accepted {field}"
            );
        }
    }
    #[test]
    fn withdrawals_and_newer_failed_probe_invalidate_old_success() {
        let (view, facts) = fixture();
        for id in ["request", "response", "send-observation"] {
            let mut bad = facts.clone();
            bad.push(Fact {
                kind: FactKind::Artifact,
                event_id: "withdrawal".into(),
                seq: 3,
                subject: format!("retract: {id}"),
                ..Default::default()
            });
            assert!(proof(&bad, &view, Some(7), Some("worker"), 1000).is_none());
        }
        assert!(proof(&facts[..2], &view, Some(7), Some("worker"), 1000).is_none());
        let mut newer = facts[0].clone();
        newer.event_id = "new-request".into();
        newer.seq = 3;
        let mut bad = facts;
        bad.push(newer);
        assert!(proof(&bad, &view, Some(7), Some("worker"), 1000).is_none());
    }
    #[test]
    fn probe_watchdog_covers_both_flag_spellings() {
        for args in [
            vec!["routes", "--probe", "worker", "--timeout-seconds", "120"],
            vec!["routes", "--probe=worker", "--timeout-seconds=120"],
        ] {
            assert_eq!(
                watchdog_budget(&args.into_iter().map(str::to_string).collect::<Vec<_>>()),
                std::time::Duration::from_secs(140)
            );
        }
        assert_eq!(
            watchdog_budget(&["routes".into()]),
            std::time::Duration::from_secs(20)
        );
    }
    #[test]
    fn endpoint_hash_ignores_labels_but_binds_transport() {
        let (view, _) = fixture();
        let mut changed = view.clone();
        changed.session.name = "renamed".into();
        assert_eq!(endpoint(&view), endpoint(&changed));
        changed.session.target = "replacement".into();
        assert_ne!(endpoint(&view), endpoint(&changed));
    }
}
