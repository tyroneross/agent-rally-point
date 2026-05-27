// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_core::context::{
    ContextBrief, ContextInputs, build_context_brief_from_inputs, build_work_packet,
};
use rally_core::graph;
use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::event::{EventBuilder, EventPayload, HandoffPayload};
use rally_core::query::{
    TraceProjection, active_blockers_at, active_claims_at, claim_conflicts, pending_handoffs_at,
    related_records, score_records,
};
use rally_core::store::{ChannelStore, store_entry_value};
use rally_core::sync::{SyncError, SyncErrorKind, build_sync_packet, import_sync_packet};
use rally_protocol::{event_hash, store_entry_hash};
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_channel(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Build a `ContextBrief` from records via the graph projection.
/// Records that are already store-entry-shaped (have a top-level
/// `event` field) are passed through with their existing origin /
/// trust_status. Raw events get wrapped with default `local` origin.
fn brief_from_records(records: &[Value], tool: &str, limit: usize, now: f64) -> ContextBrief {
    let dir = temp_channel("kernel-brief");
    std::fs::create_dir_all(&dir).unwrap();
    let now_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp(now as i64, 0)
        .unwrap()
        .to_rfc3339();
    let mut conn = graph::init(&dir, &now_rfc).unwrap();
    let wrapped: Vec<Value> = records
        .iter()
        .enumerate()
        .map(|(i, record)| {
            let seq = (i + 1) as i64;
            if record.get("event").is_some() {
                let mut wrapped = record.clone();
                if let Some(map) = wrapped.as_object_mut() {
                    map.entry("local_seq".to_string()).or_insert(json!(seq));
                    map.entry("origin".to_string()).or_insert(json!("local"));
                    map.entry("trust_status".to_string()).or_insert(json!("local"));
                }
                wrapped
            } else {
                json!({
                    "local_seq": seq,
                    "origin": "local",
                    "trust_status": "local",
                    "event": record
                })
            }
        })
        .collect();
    graph::catch_up(&mut conn, &wrapped, &now_rfc).unwrap();
    let inputs = ContextInputs::from_graph(&conn, tool, limit, now).unwrap();
    let brief = build_context_brief_from_inputs(&inputs);
    std::fs::remove_dir_all(&dir).ok();
    brief
}

fn record(kind: &str, id: &str, tool: &str, payload: Value) -> Value {
    json!({
        "specversion": "1.0",
        "id": id,
        "source": format!("urn:agent-rally-point:tool:{tool}"),
        "subject": "agent-rally-point",
        "time": "2026-05-26T18:00:00.000Z",
        "kind": kind,
        "type": format!("agent-rally.{kind}.v1"),
        "tool": tool,
        "model": "test",
        "run_id": "test-run",
        "app_slug": "agent-rally-point",
        "thread_id": "thr_11111111111111111111111111111111",
        "causation_id": null,
        "datacontenttype": "application/json",
        "dataschema": format!("urn:agent-rally-point:schema:{kind}.v1"),
        "payload": payload,
        "revision": 1
    })
}

fn typed_handoff(id: &str, subject: &str) -> EventBuilder {
    EventBuilder::new(
        id,
        EventPayload::Handoff(HandoffPayload {
            subject: subject.to_string(),
            to_tool: Some("codex".to_string()),
            from_tool: Some("pi".to_string()),
            requires_ack: true,
            ref_files: vec!["docs/SCHEMA.md".to_string()],
            notes: None,
        }),
        "pi",
        "test-run",
        "thr_11111111111111111111111111111111",
    )
    .model("test")
    .time("2026-05-26T18:00:00.000Z")
}

#[test]
fn pending_handoff_preserves_current_event_semantics() {
    let handoff = record(
        "handoff",
        "evt_handoff",
        "pi",
        json!({
            "from_tool": "pi",
            "to_tool": "codex",
            "subject": "review schema",
            "requires_ack": true,
            "ref_files": ["docs/SCHEMA.md"]
        }),
    );
    assert_eq!(
        pending_handoffs_at(
            std::slice::from_ref(&handoff),
            Some("codex"),
            1_779_829_200.0
        )[0]
        .event_id,
        "evt_handoff"
    );
    assert!(
        pending_handoffs_at(std::slice::from_ref(&handoff), Some("pi"), 1_779_829_200.0).is_empty()
    );

    let ack = record(
        "ack",
        "evt_ack",
        "codex",
        json!({"ref_handoff_id": "evt_handoff", "verdict": "done"}),
    );
    assert!(pending_handoffs_at(&[handoff, ack], Some("codex"), 1_779_829_200.0).is_empty());
}

#[test]
fn claims_conflicts_and_releases_are_derived_from_log() {
    let claim_a = record(
        "claim",
        "evt_claim_a",
        "pi",
        json!({"owner_tool": "pi", "resource": "file:docs", "subject": "edit docs"}),
    );
    let claim_b = record(
        "claim",
        "evt_claim_b",
        "codex",
        json!({"owner_tool": "codex", "resource": "file:docs/SCHEMA.md", "subject": "review schema"}),
    );
    let records = vec![claim_a.clone(), claim_b.clone()];
    assert_eq!(active_claims_at(&records, None, 1_779_829_200.0).len(), 2);
    let conflicts = claim_conflicts(&records);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].resource, "file:docs");

    let release = record(
        "claim-release",
        "evt_release",
        "codex",
        json!({"ref_claim_id": "evt_claim_b", "reason": "done"}),
    );
    let records = vec![claim_a, claim_b, release];
    assert_eq!(active_claims_at(&records, None, 1_779_829_200.0).len(), 1);
    assert!(claim_conflicts(&records).is_empty());
}

#[test]
fn trace_projection_derives_core_state_once() {
    let handoff = record(
        "handoff",
        "evt_handoff",
        "pi",
        json!({"from_tool": "pi", "to_tool": "codex", "subject": "review", "requires_ack": true}),
    );
    let claim_a = record(
        "claim",
        "evt_claim_a",
        "pi",
        json!({"owner_tool": "pi", "resource": "file:docs", "subject": "edit docs"}),
    );
    let claim_b = record(
        "claim",
        "evt_claim_b",
        "codex",
        json!({"owner_tool": "codex", "resource": "file:docs/SCHEMA.md", "subject": "review schema"}),
    );
    let records = vec![handoff, claim_a, claim_b];
    let projection = TraceProjection::from_records_at(&records, 1_779_829_200.0);

    assert_eq!(projection.pending_handoffs(Some("codex")).len(), 1);
    assert_eq!(projection.active_claims(None).len(), 2);
    assert_eq!(projection.claim_conflicts().len(), 1);
    assert!(projection.score(Some("codex")).0 < 100);
}

#[test]
fn context_brief_recommends_the_highest_priority_agent_action() {
    let handoff = record(
        "handoff",
        "evt_handoff",
        "pi",
        json!({"from_tool": "pi", "to_tool": "codex", "subject": "review", "requires_ack": true}),
    );
    let claim = record(
        "claim",
        "evt_claim",
        "codex",
        json!({"owner_tool": "codex", "resource": "file:docs", "subject": "edit docs"}),
    );
    let records = vec![claim, handoff];
    let brief = brief_from_records(&records, "codex", 5, 1_779_829_200.0);

    assert_eq!(brief.routing.action, "join_active");
    assert_eq!(brief.recommended_next_action.action, "ack_handoff");
    assert_eq!(
        brief.recommended_next_action.minimum_trust_for_automation,
        "trusted"
    );
    assert_eq!(
        brief.recommended_next_action.target.as_deref(),
        Some("evt_handoff")
    );
    assert_eq!(brief.top_priority.unwrap().kind, "handoff");
    assert_eq!(brief.relevant_changes.len(), 2);
}

#[test]
fn context_brief_includes_attuned_agent_facts() {
    let profile = record(
        "profile",
        "evt_profile",
        "codex",
        json!({
            "tool": "codex",
            "capabilities": ["rust", "review"],
            "watch": ["crates/rally-core"],
            "current_task": "evt_task",
            "branch": "codex/rally-attuned-events",
            "availability": "active"
        }),
    );
    let subscription = record(
        "subscription",
        "evt_subscription",
        "codex",
        json!({
            "tool": "codex",
            "paths": ["crates/rally-core"],
            "event_kinds": ["task", "decision"],
            "tasks": ["evt_task"]
        }),
    );
    let task = record(
        "task",
        "evt_task",
        "codex",
        json!({
            "subject": "finish context ranking",
            "status": "active",
            "owner_tool": "codex",
            "verification": "cargo test"
        }),
    );
    let artifact = record(
        "artifact",
        "evt_artifact",
        "codex",
        json!({
            "subject": "context schema",
            "artifact_kind": "json-schema",
            "uri": "docs/context.schema.json",
            "ref_task_id": "evt_task"
        }),
    );
    let decision = record(
        "decision",
        "evt_decision",
        "codex",
        json!({
            "subject": "agents use rally context for next action",
            "status": "binding",
            "scope": "agent-start"
        }),
    );
    let lesson = record(
        "lesson",
        "evt_lesson",
        "codex",
        json!({
            "subject": "avoid giant planning docs as control surfaces",
            "lesson_kind": "coordination",
            "source_event_ids": ["evt_decision"],
            "confidence": 0.9
        }),
    );
    let records = vec![profile, subscription, task, artifact, decision, lesson];
    let brief = brief_from_records(&records, "codex", 10, 1_779_829_200.0);

    assert_eq!(
        brief.profile.unwrap().capabilities,
        vec!["rust".to_string(), "review".to_string()]
    );
    assert_eq!(
        brief.subscription.unwrap().event_kinds,
        vec!["task".to_string(), "decision".to_string()]
    );
    assert_eq!(brief.recommended_next_action.action, "work_task");
    assert_eq!(
        brief.recommended_next_action.minimum_trust_for_automation,
        "trusted"
    );
    assert_eq!(brief.active_tasks[0].event_id, "evt_task");
    assert_eq!(brief.artifacts[0].ref_task_id.as_deref(), Some("evt_task"));
    assert_eq!(brief.decisions[0].status, "binding");
    assert_eq!(brief.lessons[0].confidence, Some(0.9));
    assert_eq!(brief.attuned_items[0].event_id, "evt_task");
    assert!(
        brief.attuned_items[0]
            .factors
            .contains(&"current_task:evt_task".to_string())
    );
    assert!(brief.attuned_items.iter().any(|item| {
        item.event_id == "evt_artifact"
            && item
                .factors
                .contains(&"subscribed_task:evt_task".to_string())
    }));
}

#[test]
fn context_brief_ranks_attuned_items_by_profile_subscription_path_and_trust() {
    let profile = record(
        "profile",
        "evt_profile",
        "codex",
        json!({
            "tool": "codex",
            "watch": ["crates/rally-core"],
            "current_task": "evt_task"
        }),
    );
    let subscription = record(
        "subscription",
        "evt_subscription",
        "codex",
        json!({
            "tool": "codex",
            "paths": ["crates/rally-core/src/context.rs"],
            "event_kinds": ["artifact", "decision"],
            "tasks": ["evt_task"]
        }),
    );
    let task = record(
        "task",
        "evt_task",
        "codex",
        json!({
            "subject": "finish intelligence ranking",
            "status": "active",
            "owner_tool": "codex"
        }),
    );
    let related_artifact = json!({
        "local_seq": 1,
        "origin": "remote:peer-a",
        "trust_status": "trusted",
        "event": record(
            "artifact",
            "evt_related_artifact",
            "pi",
            json!({
                "subject": "context ranking notes",
                "artifact_kind": "notes",
                "uri": "crates/rally-core/src/context.rs",
                "ref_task_id": "evt_task"
            }),
        )
    });
    let unrelated_artifact = json!({
        "local_seq": 2,
        "origin": "remote:peer-b",
        "trust_status": "untrusted",
        "event": record(
            "artifact",
            "evt_unrelated_artifact",
            "pi",
            json!({
                "subject": "website screenshot",
                "artifact_kind": "screenshot",
                "uri": "docs/marketing.png"
            }),
        )
    });
    let related_decision = record(
        "decision",
        "evt_context_decision",
        "pi",
        json!({
            "subject": "rank context by source-linked relevance",
            "status": "binding",
            "scope": "crates/rally-core/src/context.rs"
        }),
    );

    let records = vec![
        profile,
        subscription,
        task,
        unrelated_artifact,
        related_artifact,
        related_decision,
    ];
    let brief = brief_from_records(&records, "codex", 10, 1_779_829_200.0);

    let related = brief
        .attuned_items
        .iter()
        .find(|item| item.event_id == "evt_related_artifact")
        .unwrap();
    let unrelated = brief
        .attuned_items
        .iter()
        .find(|item| item.event_id == "evt_unrelated_artifact")
        .unwrap();

    assert!(related.score > unrelated.score);
    assert!(
        related
            .factors
            .contains(&"profile_watch:crates/rally-core".to_string())
    );
    assert!(
        related
            .factors
            .contains(&"subscribed_path:crates/rally-core/src/context.rs".to_string())
    );
    assert!(
        related
            .factors
            .contains(&"subscribed_kind:artifact".to_string())
    );
    assert!(related.factors.contains(&"trusted".to_string()));
    assert!(unrelated.factors.contains(&"untrusted".to_string()));
}

#[test]
fn reviewer_profile_shapes_attunement_and_recommendations() {
    let profile = record(
        "profile",
        "evt_profile",
        "codex-reviewer",
        json!({
            "tool": "codex-reviewer",
            "role": "reviewer",
            "capabilities": ["rust", "review"],
            "watch": ["crates/rally-core"]
        }),
    );
    let subscription = record(
        "subscription",
        "evt_subscription",
        "codex-reviewer",
        json!({
            "tool": "codex-reviewer",
            "paths": ["crates/rally-core/src/context.rs"],
            "event_kinds": ["artifact", "decision"]
        }),
    );
    let artifact = record(
        "artifact",
        "evt_review_packet",
        "codex",
        json!({
            "subject": "attunement ranking review packet",
            "artifact_kind": "review-packet",
            "uri": "crates/rally-core/src/context.rs"
        }),
    );
    let decision = record(
        "decision",
        "evt_arch_decision",
        "codex",
        json!({
            "subject": "context recommendations are advisory",
            "status": "binding",
            "scope": "crates/rally-core/src/context.rs"
        }),
    );
    let brief = brief_from_records(
        &[profile, subscription, decision, artifact],
        "codex-reviewer",
        10,
        1_779_829_200.0,
    );

    assert_eq!(
        brief.profile.as_ref().unwrap().role.as_deref(),
        Some("reviewer")
    );
    assert_eq!(brief.attuned_items[0].event_id, "evt_review_packet");
    assert!(
        brief.attuned_items[0]
            .factors
            .contains(&"role:reviewer".to_string())
    );
    assert_eq!(brief.recommended_next_action.action, "review_artifact");
    assert_eq!(
        brief.recommended_next_action.target.as_deref(),
        Some("evt_review_packet")
    );
}

#[test]
fn work_packet_shapes_role_specific_briefs_and_trust_contract() {
    let profile = record(
        "profile",
        "evt_profile",
        "codex-reviewer",
        json!({
            "tool": "codex-reviewer",
            "role": "reviewer",
            "capabilities": ["review"],
            "watch": ["crates/rally-core"]
        }),
    );
    let artifact = json!({
        "local_seq": 1,
        "origin": "import:sync",
        "trust_status": "untrusted",
        "event": record(
            "artifact",
            "evt_review_target",
            "codex",
            json!({
                "subject": "packet implementation notes",
                "artifact_kind": "review-notes",
                "uri": "crates/rally-core/src/context.rs"
            }),
        )
    });
    let decision = record(
        "decision",
        "evt_decision",
        "codex",
        json!({
            "subject": "packets are read-only derived state",
            "status": "binding",
            "scope": "crates/rally-core/src/context.rs"
        }),
    );
    let brief = brief_from_records(
        &[profile, artifact, decision],
        "codex-reviewer",
        10,
        1_779_829_200.0,
    );
    let packet = build_work_packet(&brief, 10);

    assert_eq!(packet.role, "reviewer");
    assert_eq!(packet.packet_kind, "review");
    assert!(packet.build_targets.is_empty());
    assert!(
        packet
            .review_targets
            .iter()
            .any(|item| item.event_id == "evt_review_target")
    );
    assert!(
        packet
            .files
            .contains(&"crates/rally-core/src/context.rs".to_string())
    );
    assert_eq!(
        packet.trust_summary.minimum_trust_for_automation,
        packet.recommended_next_action.minimum_trust_for_automation
    );
    assert_eq!(
        packet.trust_summary.recommendation_automation_allowed,
        packet.recommended_next_action.trust.automation_allowed
    );
    assert_eq!(packet.trust_summary.untrusted, 1);
    assert!(
        packet
            .trust_risks
            .iter()
            .any(|item| item.event_id == "evt_review_target")
    );
}

#[test]
fn active_tasks_use_latest_task_state_by_owner_and_subject() {
    let active = record(
        "task",
        "evt_task_active",
        "codex",
        json!({
            "subject": "finish intelligence ranking",
            "status": "active",
            "owner_tool": "codex"
        }),
    );
    let done = record(
        "task",
        "evt_task_done",
        "codex",
        json!({
            "subject": "finish intelligence ranking",
            "status": "done",
            "owner_tool": "codex"
        }),
    );

    // Build the graph from the two records and assert that the collapse
    // semantic (latest by (owner_tool, subject)) filters out the
    // closed-then-reopened task. This used to test TraceProjection
    // directly; the graph queries replicate the same collapse.
    let dir = temp_channel("active-tasks-collapse");
    std::fs::create_dir_all(&dir).unwrap();
    let mut conn = graph::init(&dir, "2026-05-27T00:00:00Z").unwrap();
    let wrapped: Vec<Value> = [active, done]
        .iter()
        .enumerate()
        .map(|(i, event)| {
            json!({
                "local_seq": (i + 1) as i64,
                "origin": "local",
                "trust_status": "local",
                "event": event
            })
        })
        .collect();
    graph::catch_up(&mut conn, &wrapped, "2026-05-27T00:00:00Z").unwrap();
    let active_tasks = graph::active_tasks_typed(&conn, Some("codex")).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        active_tasks.is_empty(),
        "done-status follow-up event must collapse with the active one and filter out"
    );
}

#[test]
fn diagnosis_combines_score_blockers_conflicts_and_stale_claims() {
    let handoff = record(
        "handoff",
        "evt_handoff",
        "pi",
        json!({"from_tool": "pi", "to_tool": "codex", "subject": "review", "requires_ack": true}),
    );
    let blocker = record(
        "blocker",
        "evt_blocker",
        "codex",
        json!({"subject": "need branch", "reason": "which branch?", "resource": "task:review"}),
    );
    let claim = record(
        "claim",
        "evt_claim",
        "codex",
        json!({"owner_tool": "codex", "resource": "file:src", "subject": "edit src"}),
    );
    let diagnosis = diagnose_records(
        &[handoff, blocker, claim],
        DiagnoseOptions {
            stale_after_seconds: 1,
            now_epoch_seconds: 1_779_829_200.0,
            ..DiagnoseOptions::default()
        },
    );

    assert_eq!(diagnosis.status, "stuck");
    assert!(diagnosis.score < 100);
    assert!(
        diagnosis
            .findings
            .iter()
            .any(|finding| finding.code == "open-required-handoff")
    );
    assert!(
        diagnosis
            .findings
            .iter()
            .any(|finding| finding.code == "active-blocker")
    );
    assert!(
        diagnosis
            .findings
            .iter()
            .any(|finding| finding.code == "stale-claim")
    );
}

#[test]
fn wrapped_store_entries_participate_in_related_records_and_blockers() {
    let blocker = record(
        "blocker",
        "evt_blocker",
        "codex",
        json!({"subject": "need branch", "reason": "which branch?"}),
    );
    let wrapped = json!({
        "local_seq": 4,
        "received_at": "2026-05-26T18:00:01.000Z",
        "origin": "remote:peer-a",
        "event": blocker
    });
    let unblock = record(
        "blocker-resolved",
        "evt_unblock",
        "pi",
        json!({"ref_blocker_id": "evt_blocker", "resolution": "branch supplied"}),
    );

    assert_eq!(
        related_records(&[wrapped.clone(), unblock.clone()], "evt_blocker").len(),
        2
    );
    assert_eq!(
        active_blockers_at(&[wrapped], None, 1_779_829_200.0).len(),
        1
    );
    assert!(active_blockers_at(&[unblock], None, 1_779_829_200.0).is_empty());
}

#[test]
fn score_reports_dangling_references() {
    let ack = record(
        "ack",
        "evt_ack",
        "codex",
        json!({"ref_handoff_id": "evt_missing", "verdict": "done"}),
    );
    let (_score, findings) = score_records(&[ack], None);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "dangling-reference")
    );
}

#[test]
fn channel_store_loads_changes_jsonl() {
    let channel = temp_channel("rally-core-store");
    fs::create_dir_all(&channel).unwrap();
    let entry = store_entry_value(
        record(
            "handoff",
            "evt_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "review"}),
        ),
        1,
        None,
        "local",
    )
    .unwrap();
    fs::write(
        channel.join("changes.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();

    let records = ChannelStore::new(&channel).load_records().unwrap();
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(
        pending_handoffs_at(&records, Some("codex"), 1_779_829_200.0).len(),
        1
    );
}

#[test]
fn channel_store_appends_hash_chained_entries() {
    let channel = temp_channel("rally-core-append");
    let store = ChannelStore::new(&channel);

    let first = store
        .append_event(record(
            "handoff",
            "evt_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "review"}),
        ))
        .unwrap();
    let second = store
        .append_event(record(
            "ack",
            "evt_ack",
            "codex",
            json!({"ref_handoff_id": "evt_handoff", "verdict": "done"}),
        ))
        .unwrap();

    let text = fs::read_to_string(channel.join("changes.jsonl")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(first["local_seq"], 1);
    assert_eq!(second["local_seq"], 2);
    assert_eq!(first["origin"], "local");
    assert_eq!(first["event_hash"], event_hash(&first).unwrap());
    assert_eq!(second["prev_entry_hash"], store_entry_hash(lines[0]));
    assert_eq!(lines.len(), 2);
}

#[test]
fn channel_store_appends_typed_events() {
    let channel = temp_channel("rally-core-typed-append");
    let store = ChannelStore::new(&channel);

    let entry = store
        .append_typed(typed_handoff("evt_typed_handoff", "typed review"))
        .unwrap();
    let records = store.load_records().unwrap();
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(entry["event"]["kind"], "handoff");
    assert_eq!(entry["event"]["payload"]["subject"], "typed review");
    assert_eq!(
        pending_handoffs_at(&records, Some("codex"), 1_779_829_200.0)[0].files,
        vec!["docs/SCHEMA.md".to_string()]
    );
}

#[test]
fn channel_store_rejects_corrupt_and_tampered_logs() {
    let corrupt = temp_channel("rally-core-corrupt");
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("changes.jsonl"), "{\n").unwrap();
    assert!(ChannelStore::new(&corrupt).load_records().is_err());
    fs::remove_dir_all(corrupt).unwrap();

    let tampered = temp_channel("rally-core-tampered");
    fs::create_dir_all(&tampered).unwrap();
    let mut entry = store_entry_value(
        record(
            "handoff",
            "evt_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "review"}),
        ),
        1,
        None,
        "local",
    )
    .unwrap();
    entry["event_hash"] = json!("sha256:bad");
    fs::write(
        tampered.join("changes.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();
    assert!(ChannelStore::new(&tampered).load_records().is_err());
    fs::remove_dir_all(tampered).unwrap();
}

#[test]
fn channel_store_rejects_blank_whitespace_and_partial_lines() {
    let entry = store_entry_value(
        record(
            "handoff",
            "evt_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "review"}),
        ),
        1,
        None,
        "local",
    )
    .unwrap();
    let line = serde_json::to_string(&entry).unwrap();

    for (name, contents) in [
        ("blank", format!("{line}\n\n")),
        ("whitespace", format!(" {line}\n")),
        ("partial", line),
    ] {
        let channel = temp_channel(&format!("rally-core-{name}"));
        fs::create_dir_all(&channel).unwrap();
        fs::write(channel.join("changes.jsonl"), contents).unwrap();
        assert!(
            ChannelStore::new(&channel).load_records().is_err(),
            "{name} log should be rejected"
        );
        fs::remove_dir_all(channel).unwrap();
    }
}

#[test]
fn channel_store_serializes_concurrent_appenders() {
    let channel = temp_channel("rally-core-concurrent");
    let store = ChannelStore::new(&channel);
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();

    for index in 0..8 {
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            store
                .append_event(record(
                    "handoff",
                    &format!("evt_handoff_{index}"),
                    "pi",
                    json!({"to_tool": "codex", "subject": format!("review {index}")}),
                ))
                .unwrap()
        }));
    }

    let mut seqs = handles
        .into_iter()
        .map(|handle| handle.join().unwrap()["local_seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>());

    let records = store.load_records().unwrap();
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(records.len(), 8);
}

#[test]
fn channel_store_checkpoint_is_rebuildable_cache() {
    let channel = temp_channel("rally-core-checkpoint");
    let store = ChannelStore::new(&channel);
    store
        .append_event(record(
            "handoff",
            "evt_checkpoint_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "checkpoint review"}),
        ))
        .unwrap();

    let strict = store.load_records().unwrap();
    let rebuilt = store.rebuild_checkpoint().unwrap();
    let cached = store.load_records_cached().unwrap();
    let status = store.checkpoint_status().unwrap();
    fs::remove_dir_all(channel).unwrap();

    assert_eq!(rebuilt.records, 1);
    assert!(status.valid);
    assert_eq!(strict, cached);
}

#[test]
fn sync_packet_round_trips_through_core() {
    let source_channel = temp_channel("rally-core-sync-source");
    let source = ChannelStore::new(&source_channel);
    source
        .append_event(record(
            "handoff",
            "evt_sync_handoff",
            "pi",
            json!({"to_tool": "codex", "subject": "sync review"}),
        ))
        .unwrap();
    let records = source.load_records().unwrap();
    let packet = build_sync_packet("source", "2026-05-26T18:00:00.000Z", &records).unwrap();
    assert_eq!(packet["schema"], "agent-rally.sync.packet.v1");
    assert_eq!(packet["count"], 1);
    assert!(
        packet["events"][0]
            .as_object()
            .unwrap()
            .get("local_seq")
            .is_none()
    );

    let target_channel = temp_channel("rally-core-sync-target");
    let target = ChannelStore::new(&target_channel);
    let summary = import_sync_packet(&target, &packet, "remote:test", |_| {
        Ok::<_, SyncError>("trusted".to_string())
    })
    .unwrap();
    assert_eq!(summary.imported, 1);
    assert_eq!(summary.duplicates, 0);
    assert_eq!(summary.trust_counts.get("trusted"), Some(&1));
    let imported_records = target.load_records().unwrap();
    assert_eq!(imported_records[0]["origin"], "remote:test");
    assert_eq!(imported_records[0]["trust_status"], "trusted");
    let pending = pending_handoffs_at(&imported_records, Some("codex"), 1_779_829_200.0);
    assert_eq!(pending[0].origin.as_deref(), Some("remote:test"));
    assert_eq!(pending[0].trust_status.as_deref(), Some("trusted"));

    let duplicate = import_sync_packet(&target, &packet, "remote:test", |_| {
        Ok::<_, SyncError>("trusted".to_string())
    })
    .unwrap();
    fs::remove_dir_all(source_channel).unwrap();
    fs::remove_dir_all(target_channel).unwrap();

    assert_eq!(duplicate.imported, 0);
    assert_eq!(duplicate.duplicates, 1);
    assert!(duplicate.conflicts.is_empty());
}

#[test]
fn sync_import_rejects_packets_without_events_as_usage() {
    let target_channel = temp_channel("rally-core-sync-invalid");
    let target = ChannelStore::new(&target_channel);
    let err = import_sync_packet(
        &target,
        &json!({"schema": "agent-rally.sync.packet.v1"}),
        "remote:test",
        |_| Ok::<_, SyncError>("trusted".to_string()),
    )
    .unwrap_err();
    fs::remove_dir_all(target_channel).ok();

    assert_eq!(err.kind(), SyncErrorKind::Usage);
    assert_eq!(err.to_string(), "packet must contain an events array");
}
