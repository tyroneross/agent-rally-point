// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::output::{CliError, WriteOutput};
use crate::{
    AckCommand, BlockerCommand, ClaimCommand, CommonOptions, HandoffCommand, IdentityInitCommand,
    PreflightCommand, ReleaseCommand, UnblockCommand, new_id, now_rfc3339,
};
use rally_core::event::{
    AckPayload, BlockerPayload, BlockerResolvedPayload, ClaimPayload, ClaimReleasePayload,
    EventBuilder, EventPayload, HandoffPayload,
};
use rally_core::preflight::{PreflightOptions, run_preflight};
use rally_core::store::ChannelStore;
use rally_protocol::event_value;
use rally_trust::{init_identity, load_signing_identity, sign_event};
use serde_json::{Value, json};

pub(super) fn execute_handoff(command: HandoffCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("handoff")?;
    let payload = EventPayload::Handoff(HandoffPayload {
        subject: command.subject.clone(),
        to_tool: Some(command.to_tool.clone()),
        from_tool: Some(command.from_tool.clone()),
        requires_ack: command.requires_ack,
        ref_files: command.files,
        notes: command.notes,
    });
    let entry = append(
        &store,
        EventBuilder::new(
            new_id("evt"),
            payload,
            &command.from_tool,
            command.common.run_id(),
            new_id("thr"),
        )
        .model(command.common.model())
        .subject(command.subject.clone())
        .time(now_rfc3339()),
        &command.common,
        "handoff",
    )?;
    Ok(write_output(
        "handoff",
        &command.common,
        &store,
        &entry,
        format!(
            "posted handoff {} local_seq={} to={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default(),
            command.to_tool
        ),
        json!({
            "to_tool": command.to_tool,
            "from_tool": command.from_tool,
            "subject": command.subject,
        }),
    ))
}

pub(super) fn execute_ack(command: AckCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store(command.command)?;
    let records = store.load_records().map_err(|err| {
        CliError::runtime(command.command, format!("failed to load channel: {err}"))
    })?;
    let target = find_record(&records, &command.identifier, Some("handoff"));
    if target.is_none() && !command.force {
        return Err(CliError::not_found(
            command.command,
            format!(
                "no handoff found for {:?}; pass --force to ack anyway",
                command.identifier
            ),
        ));
    }
    let reference = canonical_reference(target.as_ref(), &command.identifier);
    let tool = command.common.tool();
    let payload = EventPayload::Ack(AckPayload {
        ref_handoff_id: reference.clone(),
        verdict: command.verdict.clone(),
        summary: command.summary,
        reason: command.reason,
        notes: None,
    });
    let mut builder = EventBuilder::new(
        new_id("evt"),
        payload,
        &tool,
        command.common.run_id(),
        target
            .as_ref()
            .and_then(|record| event_field(record, "thread_id"))
            .unwrap_or_else(|| new_id("thr")),
    )
    .model(command.common.model())
    .subject(command.identifier.clone())
    .time(now_rfc3339())
    .causation_id(reference.clone());
    if let Some(correlation_id) = target
        .as_ref()
        .and_then(|record| event_field(record, "correlation_id"))
    {
        builder = builder.correlation_id(correlation_id);
    }
    let entry = append(&store, builder, &command.common, command.command)?;
    Ok(write_output(
        command.command,
        &command.common,
        &store,
        &entry,
        format!(
            "posted {} ack for {} local_seq={}",
            command.verdict,
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "verdict": command.verdict,
            "ref_handoff_id": reference,
            "resolved": target.is_some(),
        }),
    ))
}

pub(super) fn execute_claim(command: ClaimCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("claim")?;
    let tool = command.common.tool();
    let payload = EventPayload::Claim(ClaimPayload {
        owner_tool: tool.clone(),
        resource: command.resource.clone(),
        subject: command.subject.clone(),
        notes: command.notes,
    });
    let entry = append(
        &store,
        EventBuilder::new(
            new_id("evt"),
            payload,
            &tool,
            command.common.run_id(),
            new_id("thr"),
        )
        .model(command.common.model())
        .subject(command.subject.clone())
        .time(now_rfc3339()),
        &command.common,
        "claim",
    )?;
    Ok(write_output(
        "claim",
        &command.common,
        &store,
        &entry,
        format!(
            "posted claim {} local_seq={} resource={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default(),
            command.resource
        ),
        json!({
            "tool": tool,
            "resource": command.resource,
            "subject": command.subject,
        }),
    ))
}

pub(super) fn execute_release(command: ReleaseCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("release")?;
    let records = store
        .load_records()
        .map_err(|err| CliError::runtime("release", format!("failed to load channel: {err}")))?;
    let target = find_record(&records, &command.identifier, Some("claim"));
    if target.is_none() && !command.force {
        return Err(CliError::not_found(
            "release",
            format!(
                "no claim found for {:?}; pass --force to release anyway",
                command.identifier
            ),
        ));
    }
    let reference = canonical_reference(target.as_ref(), &command.identifier);
    let tool = command.common.tool();
    let entry = append(
        &store,
        EventBuilder::new(
            new_id("evt"),
            EventPayload::ClaimRelease(ClaimReleasePayload {
                ref_claim_id: reference.clone(),
                reason: command.reason,
            }),
            &tool,
            command.common.run_id(),
            target
                .as_ref()
                .and_then(|record| event_field(record, "thread_id"))
                .unwrap_or_else(|| new_id("thr")),
        )
        .model(command.common.model())
        .subject(command.identifier.clone())
        .time(now_rfc3339())
        .causation_id(reference.clone()),
        &command.common,
        "release",
    )?;
    Ok(write_output(
        "release",
        &command.common,
        &store,
        &entry,
        format!(
            "released claim {} local_seq={}",
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "ref_claim_id": reference,
            "resolved": target.is_some(),
        }),
    ))
}

pub(super) fn execute_blocker(command: BlockerCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("blocker")?;
    let tool = command.common.tool();
    let entry = append(
        &store,
        EventBuilder::new(
            new_id("evt"),
            EventPayload::Blocker(BlockerPayload {
                subject: command.subject.clone(),
                reason: command.reason,
                severity: command.severity.clone(),
                resource: command.resource.clone(),
            }),
            &tool,
            command.common.run_id(),
            new_id("thr"),
        )
        .model(command.common.model())
        .subject(command.subject.clone())
        .time(now_rfc3339()),
        &command.common,
        "blocker",
    )?;
    Ok(write_output(
        "blocker",
        &command.common,
        &store,
        &entry,
        format!(
            "posted blocker {} local_seq={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "tool": tool,
            "resource": command.resource,
            "subject": command.subject,
            "severity": command.severity,
        }),
    ))
}

pub(super) fn execute_unblock(command: UnblockCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("unblock")?;
    let records = store
        .load_records()
        .map_err(|err| CliError::runtime("unblock", format!("failed to load channel: {err}")))?;
    let target = find_record(&records, &command.identifier, Some("blocker"));
    if target.is_none() && !command.force {
        return Err(CliError::not_found(
            "unblock",
            format!(
                "no blocker found for {:?}; pass --force to resolve anyway",
                command.identifier
            ),
        ));
    }
    let reference = canonical_reference(target.as_ref(), &command.identifier);
    let tool = command.common.tool();
    let entry = append(
        &store,
        EventBuilder::new(
            new_id("evt"),
            EventPayload::BlockerResolved(BlockerResolvedPayload {
                ref_blocker_id: reference.clone(),
                resolution: command.resolution.clone(),
            }),
            &tool,
            command.common.run_id(),
            target
                .as_ref()
                .and_then(|record| event_field(record, "thread_id"))
                .unwrap_or_else(|| new_id("thr")),
        )
        .model(command.common.model())
        .subject(command.identifier.clone())
        .time(now_rfc3339())
        .causation_id(reference.clone()),
        &command.common,
        "unblock",
    )?;
    Ok(write_output(
        "unblock",
        &command.common,
        &store,
        &entry,
        format!(
            "resolved blocker {} local_seq={}",
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "ref_blocker_id": reference,
            "resolution": command.resolution,
            "resolved": target.is_some(),
        }),
    ))
}

pub(super) fn execute_identity_init(command: IdentityInitCommand) -> Result<WriteOutput, CliError> {
    let identity_dir = command.common.identity_dir()?;
    let allowed_kinds = [
        "handoff",
        "ack",
        "feedback",
        "claim",
        "claim-release",
        "blocker",
        "blocker-resolved",
    ];
    let identity = init_identity(&identity_dir, &command.tool, &allowed_kinds)
        .map_err(|err| CliError::runtime("identity:init", err.to_string()))?;
    Ok(WriteOutput {
        json: command.common.json,
        text: format!(
            "initialized identity {} tool={} dir={}",
            identity.key_id,
            command.tool,
            identity_dir.display()
        ),
        body: json!({
            "ok": true,
            "command": "identity:init",
            "schema": "agent-rally.command.identity-init.v1",
            "identity_dir": identity_dir.display().to_string(),
            "key_id": identity.key_id,
            "public_key": identity.public_key,
            "tool": command.tool,
        }),
    })
}

pub(super) fn execute_preflight(command: PreflightCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("preflight")?;
    let envelope = run_preflight(
        &store,
        &PreflightOptions {
            tool: command.common.tool(),
            model: command.common.model.clone(),
            session_id: command.session_id,
            start_ping: command.start_ping,
            stale_after_seconds: command.stale_after_seconds,
            recent_limit: 5,
        },
    )
    .map_err(|err| CliError::runtime("preflight", format!("failed to preflight: {err}")))?;
    let text = format!(
        "{} action={} pending_acks={} active_peers={}",
        envelope.coordination_status,
        envelope.routing.action,
        envelope.pending_acks_for_me.len(),
        envelope.active_peers.len()
    );
    let body = serde_json::to_value(&envelope)
        .map_err(|err| CliError::runtime("preflight", err.to_string()))?;
    Ok(WriteOutput {
        json: command.common.json,
        text,
        body,
    })
}

fn append(
    store: &ChannelStore,
    event: EventBuilder,
    options: &CommonOptions,
    command: &'static str,
) -> Result<Value, CliError> {
    if !options.sign {
        return store
            .append_typed(event)
            .map_err(|err| CliError::runtime(command, format!("failed to append event: {err}")));
    }
    let identity_dir = options.identity_dir()?;
    let identity =
        load_signing_identity(&identity_dir, options.key_id.as_deref()).map_err(|err| {
            CliError::runtime(command, format!("failed to load signing identity: {err}"))
        })?;
    let mut event = event
        .build()
        .map_err(|err| CliError::runtime(command, format!("failed to build event: {err}")))?;
    sign_event(&mut event, &identity, &now_rfc3339())
        .map_err(|err| CliError::runtime(command, format!("failed to sign event: {err}")))?;
    store
        .append_event(event)
        .map_err(|err| CliError::runtime(command, format!("failed to append event: {err}")))
}

fn write_output(
    command: &'static str,
    options: &CommonOptions,
    store: &ChannelStore,
    entry: &Value,
    text: String,
    extra: Value,
) -> WriteOutput {
    let mut body = json!({
        "ok": true,
        "command": command,
        "schema": format!("agent-rally.command.{command}.v1"),
        "channel": store.channel_dir().display().to_string(),
        "event_id": event_field(entry, "id"),
        "local_seq": local_seq(entry),
        "event": entry.get("event").cloned().unwrap_or(Value::Null),
    });
    if let (Some(object), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        object.extend(extra.clone());
    }
    WriteOutput {
        json: options.json,
        text,
        body,
    }
}

fn find_record(records: &[Value], identifier: &str, kind: Option<&str>) -> Option<Value> {
    records
        .iter()
        .find(|record| {
            if let Some(kind) = kind {
                if event_field(record, "kind").as_deref() != Some(kind) {
                    return false;
                }
            }
            event_field(record, "id").as_deref() == Some(identifier)
                || record
                    .get("local_seq")
                    .and_then(Value::as_u64)
                    .is_some_and(|seq| seq.to_string() == identifier)
        })
        .cloned()
}

fn canonical_reference(record: Option<&Value>, fallback: &str) -> String {
    record
        .and_then(|value| event_field(value, "id"))
        .unwrap_or_else(|| fallback.to_string())
}

fn event_field(record: &Value, key: &str) -> Option<String> {
    event_value(record)
        .ok()
        .and_then(|event| event.get(key).and_then(Value::as_str).map(str::to_string))
}

fn local_seq(entry: &Value) -> Option<u64> {
    entry.get("local_seq").and_then(Value::as_u64)
}
