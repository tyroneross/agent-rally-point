// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_core::diagnose::{DiagnoseOptions, diagnose_records};
use rally_core::event::{EventBuilder, EventPayload, HandoffPayload};
use rally_core::query::{
    active_blockers_at, active_claims_at, claim_conflicts, pending_handoffs_at, related_records,
    score_records,
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
