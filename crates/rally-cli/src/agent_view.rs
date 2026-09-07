// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//! Bounded advisory observation. Never used as the authority for a write check.
use crate::cli::RoomArgs;
use crate::error::{RallyError, Result};
use crate::output::Output;
use crate::store::{Fact, RoomSnapshot, RoomStore};
use serde_json::{Value, json};

pub(crate) fn command(args: RoomArgs) -> Result<Output> {
    let tool = args
        .tool
        .as_deref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| RallyError::Usage("room --compact requires --tool".into()))?;
    if args.readers
        || args.include_archived
        || args.event_id.is_some()
        || args.thread_id.is_some()
        || args.role.is_some()
        || args.actionable
    {
        return Err(RallyError::Usage("room --compact supports --tool, --path, --since and --budget-bytes; use full room for other filters".into()));
    }
    let snapshot = RoomStore::open()?.snapshot()?;
    let paths = crate::normalize_paths(args.paths.clone());
    let body = compose(
        &snapshot,
        tool,
        &paths,
        args.since,
        args.budget_bytes.unwrap_or(6000),
    );
    Ok(Output::new(
        args.json,
        format!(
            "compact room seq={} overflow={} (advisory; check before writing)",
            snapshot.max_seq, body["data"]["budget"]["over_budget"]
        ),
        body,
    ))
}

fn compose(
    snapshot: &RoomSnapshot,
    tool: &str,
    paths: &[String],
    since: Option<i64>,
    budget: usize,
) -> Value {
    let addressed =
        |f: &&Fact| f.tool.as_deref() == Some(tool) || f.target.as_deref() == Some(tool);
    let scoped = |f: &&Fact| {
        paths
            .iter()
            .any(|p| f.scope.iter().any(|s| crate::path_matches_scope(s, p)))
    };
    let relevant = |f: &&Fact| paths.is_empty() || f.scope.is_empty() || addressed(f) || scoped(f);
    // Critical rows retain their full payload, even when that exceeds the byte
    // target. A watermark never hides an old but still-active obligation.
    let claims: Vec<_> = snapshot
        .active_claims
        .iter()
        .filter(|f| paths.is_empty() || addressed(f) || scoped(f))
        .collect();
    let blockers: Vec<_> = snapshot.active_blockers.iter().filter(relevant).collect();
    let obligations: Vec<_> = snapshot
        .open_obligations
        .iter()
        .filter(|f| f.target.as_deref() == Some(tool))
        .collect();
    let decisions: Vec<_> = snapshot.current_decisions.iter().filter(relevant).collect();
    let mut peers: Vec<_> = snapshot
        .squads
        .iter()
        .filter(|s| s.tool != tool && s.freshness == "fresh")
        .collect();
    peers.sort_by(|a, b| {
        b.last_seen_seq
            .cmp(&a.last_seen_seq)
            .then(a.tool.cmp(&b.tool))
    });
    let peer_rows: Vec<_> = peers
        .iter()
        .take(8)
        .map(|s| json!({"tool":s.tool,"freshness":s.freshness,"last_seen_seq":s.last_seen_seq}))
        .collect();
    let mut body = json!({
        "ok":true,"product":"rally","command":"room","schema":"agent-rally.command.room.compact.v1",
        "data":{
            "advisory":true,"tool":tool,"paths":paths,"max_seq":snapshot.max_seq,
            "content_max_seq":snapshot.content_max_seq,
            "since":since,"changed":since.is_none_or(|s| snapshot.max_seq > s),
            "lead":snapshot.lead,"lead_epoch":snapshot.lead_epoch,"mission":snapshot.mission,
            "room_freeze_id":snapshot.room_freeze_id,
            "claims":claims,"blockers":blockers,"obligations":obligations,"decisions":decisions,
            "peers":peer_rows,
            "inventory":{
                "squads_total":snapshot.squads.len(),"peers_omitted":snapshot.squads.len().saturating_sub(peer_rows.len()),
                "claims_total":snapshot.active_claims.len(),"blockers_total":snapshot.active_blockers.len(),
                "artifacts_total":snapshot.unconsumed_artifacts.len(),"artifacts_included":false,
                "risks_total":snapshot.current_risks.len(),"risks_included":false,
                "system_health_total":snapshot.system_health.len(),"system_health_included":false
            },
            "expand":{"room":["rally","room","--json"],"inbox":["rally","inbox","--tool",tool,"--json"],"write_check":["rally","check","before-write","--tool",tool,"--path","<path>","--strict","--json"]},
            "budget":{"bytes":budget,"emitted_bytes":0,"over_budget":false,"reason":null}
        }
    });
    // Remove optional roster entries first; never cut safety/task facts. Measure
    // the complete pretty JSON envelope including its trailing newline.
    loop {
        let size = serde_json::to_string_pretty(&body)
            .expect("JSON values serialize")
            .len()
            + 1;
        if budget != 0 && size > budget && !body["data"]["peers"].as_array().unwrap().is_empty() {
            body["data"]["peers"].as_array_mut().unwrap().pop();
            let n = body["data"]["inventory"]["peers_omitted"].as_u64().unwrap();
            body["data"]["inventory"]["peers_omitted"] = json!(n + 1);
            continue;
        }
        let overflow = budget != 0 && size > budget;
        let reason = if overflow {
            json!("critical context exceeds budget; retained in full")
        } else {
            Value::Null
        };
        if body["data"]["budget"]["emitted_bytes"] == size
            && body["data"]["budget"]["over_budget"] == overflow
            && body["data"]["budget"]["reason"] == reason
        {
            break;
        }
        body["data"]["budget"]["emitted_bytes"] = json!(size);
        body["data"]["budget"]["over_budget"] = json!(overflow);
        body["data"]["budget"]["reason"] = reason;
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FactKind, Squad};

    #[test]
    fn crowded_roster_is_bounded_and_critical_overflow_is_honest() {
        let mut snapshot = RoomSnapshot::default();
        for i in 0..800 {
            snapshot.squads.push(Squad {
                tool: format!("peer:{i}"),
                freshness: "fresh".into(),
                ..Default::default()
            });
        }
        let compact = compose(&snapshot, "codex:a", &[], None, 6000);
        let bytes = serde_json::to_string_pretty(&compact).unwrap().len() + 1;
        assert!(bytes <= 6000);
        assert_eq!(compact["data"]["budget"]["emitted_bytes"], bytes);
        assert_eq!(compact["data"]["inventory"]["squads_total"], 800);
        assert!(bytes * 2 < serde_json::to_vec(&snapshot).unwrap().len());
        let blocker = crate::Fact {
            kind: FactKind::Blocker,
            event_id: "critical".into(),
            subject: "x".repeat(7000),
            ..Default::default()
        };
        snapshot.active_blockers.push(blocker);
        snapshot.max_seq = 20;
        let overflow = compose(&snapshot, "codex:a", &[], Some(20), 6000);
        assert_eq!(overflow["data"]["changed"], false);
        assert_eq!(overflow["data"]["blockers"][0]["event_id"], "critical");
        assert_eq!(overflow["data"]["budget"]["over_budget"], true);
        assert_eq!(
            overflow["data"]["budget"]["emitted_bytes"],
            serde_json::to_string_pretty(&overflow).unwrap().len() + 1
        );
    }
}
