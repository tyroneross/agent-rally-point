// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::query::{
    active_blockers_at, active_claims_at, claim_conflicts, pending_handoffs_at, related_records,
    score_records,
};
use rally_core::store::ChannelStore;
use serde_json::{Value, json};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[test]
fn pending_handoff_matches_python_inbox_semantics() {
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
    let channel = std::env::temp_dir().join(format!(
        "rally-core-store-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&channel).unwrap();
    fs::write(
        channel.join("changes.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&record(
                "handoff",
                "evt_handoff",
                "pi",
                json!({"to_tool": "codex", "subject": "review"})
            ))
            .unwrap()
        ),
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
