// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{
    AckCommand, ArtifactCommand, BlockerCommand, ClaimCommand, CommonOptions, DecisionCommand,
    HandoffCommand, IdentityInitCommand, LessonCommand, PreflightCommand, ProfileCommand,
    ReleaseCommand, SubscribeCommand, TaskCommand, UnblockCommand,
};
use crate::output::{CliError, WriteOutput};
use crate::runtime::{new_id, now_rfc3339};
use rally_core::event::{
    AckPayload, ArtifactPayload, BlockerPayload, BlockerResolvedPayload, ClaimPayload,
    ClaimReleasePayload, DecisionPayload, EventBuilder, EventPayload, HandoffPayload,
    LessonPayload, ProfilePayload, SubscriptionPayload, TaskPayload,
};
use rally_core::preflight::{PreflightOptions, run_preflight};
use rally_core::store::ChannelStore;
use rally_protocol::event_value;
use rally_trust::{init_identity, load_signing_identity, sign_event};
use serde_json::{Value, json};

pub(super) fn execute_handoff(command: HandoffCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("handoff", &command.common)?;
    let payload = EventPayload::Handoff(HandoffPayload {
        subject: command.subject.clone(),
        to_tool: Some(command.to_tool.clone()),
        from_tool: Some(command.from_tool.clone()),
        requires_ack: command.requires_ack,
        ref_files: command.files,
        notes: command.notes,
    });
    let entry = context.append(context.event(
        payload,
        &command.from_tool,
        new_id("thr"),
        command.subject.clone(),
    ))?;
    Ok(context.output(
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
    let context = CommandContext::new(command.command, &command.common)?;
    let target = context.resolve_target(&command.identifier, "handoff", command.force, "ack")?;
    let payload = EventPayload::Ack(AckPayload {
        ref_handoff_id: target.reference.clone(),
        verdict: command.verdict.clone(),
        summary: command.summary,
        reason: command.reason,
        notes: None,
    });
    let tool = context.tool();
    let mut builder = context
        .event(
            payload,
            &tool,
            target.thread_id.clone(),
            command.identifier.clone(),
        )
        .causation_id(target.reference.clone());
    if let Some(correlation_id) = target.correlation_id.clone() {
        builder = builder.correlation_id(correlation_id);
    }
    let entry = context.append(builder)?;
    Ok(context.output(
        &entry,
        format!(
            "posted {} ack for {} local_seq={}",
            command.verdict,
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "verdict": command.verdict,
            "ref_handoff_id": target.reference,
            "resolved": target.resolved,
        }),
    ))
}

pub(super) fn execute_claim(command: ClaimCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("claim", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Claim(ClaimPayload {
        owner_tool: tool.clone(),
        resource: command.resource.clone(),
        subject: command.subject.clone(),
        notes: command.notes,
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), command.subject.clone()))?;
    Ok(context.output(
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
    let context = CommandContext::new("release", &command.common)?;
    let target = context.resolve_target(&command.identifier, "claim", command.force, "release")?;
    let tool = context.tool();
    let entry = context.append(
        context
            .event(
                EventPayload::ClaimRelease(ClaimReleasePayload {
                    ref_claim_id: target.reference.clone(),
                    reason: command.reason,
                }),
                &tool,
                target.thread_id,
                command.identifier.clone(),
            )
            .causation_id(target.reference.clone()),
    )?;
    Ok(context.output(
        &entry,
        format!(
            "released claim {} local_seq={}",
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "ref_claim_id": target.reference,
            "resolved": target.resolved,
        }),
    ))
}

pub(super) fn execute_blocker(command: BlockerCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("blocker", &command.common)?;
    let tool = context.tool();
    let entry = context.append(context.event(
        EventPayload::Blocker(BlockerPayload {
            subject: command.subject.clone(),
            reason: command.reason,
            severity: command.severity.clone(),
            resource: command.resource.clone(),
        }),
        &tool,
        new_id("thr"),
        command.subject.clone(),
    ))?;
    Ok(context.output(
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
    let context = CommandContext::new("unblock", &command.common)?;
    let target =
        context.resolve_target(&command.identifier, "blocker", command.force, "resolve")?;
    let tool = context.tool();
    let entry = context.append(
        context
            .event(
                EventPayload::BlockerResolved(BlockerResolvedPayload {
                    ref_blocker_id: target.reference.clone(),
                    resolution: command.resolution.clone(),
                }),
                &tool,
                target.thread_id,
                command.identifier.clone(),
            )
            .causation_id(target.reference.clone()),
    )?;
    Ok(context.output(
        &entry,
        format!(
            "resolved blocker {} local_seq={}",
            command.identifier,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "ref_blocker_id": target.reference,
            "resolution": command.resolution,
            "resolved": target.resolved,
        }),
    ))
}

pub(super) fn execute_profile(command: ProfileCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("profile", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Profile(ProfilePayload {
        tool: tool.clone(),
        capabilities: command.capabilities.clone(),
        watch: command.watch.clone(),
        current_task: command.current_task.clone(),
        branch: command.branch.clone(),
        availability: command.availability.clone(),
        notes: command.notes.clone(),
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), format!("profile {tool}")))?;
    Ok(context.output(
        &entry,
        format!(
            "updated profile {} local_seq={}",
            tool,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "tool": tool,
            "capabilities": command.capabilities,
            "watch": command.watch,
        }),
    ))
}

pub(super) fn execute_task(command: TaskCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("task", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Task(TaskPayload {
        subject: command.subject.clone(),
        status: command.status.clone(),
        owner_tool: command.owner_tool.clone(),
        depends_on: command.depends_on.clone(),
        artifacts: command.artifacts.clone(),
        verification: command.verification.clone(),
        notes: command.notes.clone(),
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), command.subject.clone()))?;
    Ok(context.output(
        &entry,
        format!(
            "posted task {} local_seq={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "subject": command.subject,
            "status": command.status,
            "owner_tool": command.owner_tool,
        }),
    ))
}

pub(super) fn execute_artifact(command: ArtifactCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("artifact", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Artifact(ArtifactPayload {
        subject: command.subject.clone(),
        artifact_kind: command.artifact_kind.clone(),
        uri: command.uri.clone(),
        ref_task_id: command.ref_task_id.clone(),
        summary: command.summary.clone(),
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), command.subject.clone()))?;
    Ok(context.output(
        &entry,
        format!(
            "recorded artifact {} local_seq={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "subject": command.subject,
            "artifact_kind": command.artifact_kind,
            "uri": command.uri,
            "ref_task_id": command.ref_task_id,
        }),
    ))
}

pub(super) fn execute_decision(command: DecisionCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("decision", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Decision(DecisionPayload {
        subject: command.subject.clone(),
        status: command.status.clone(),
        scope: command.scope.clone(),
        supersedes: command.supersedes.clone(),
        rationale: command.rationale.clone(),
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), command.subject.clone()))?;
    Ok(context.output(
        &entry,
        format!(
            "recorded decision {} local_seq={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "subject": command.subject,
            "status": command.status,
            "scope": command.scope,
        }),
    ))
}

pub(super) fn execute_lesson(command: LessonCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("lesson", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Lesson(LessonPayload {
        subject: command.subject.clone(),
        lesson_kind: command.lesson_kind.clone(),
        scope: command.scope.clone(),
        source_event_ids: command.source_event_ids.clone(),
        confidence: command.confidence,
    });
    let entry =
        context.append(context.event(payload, &tool, new_id("thr"), command.subject.clone()))?;
    Ok(context.output(
        &entry,
        format!(
            "recorded lesson {} local_seq={}",
            event_field(&entry, "id").unwrap_or_default(),
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "subject": command.subject,
            "lesson_kind": command.lesson_kind,
            "confidence": command.confidence,
        }),
    ))
}

pub(super) fn execute_subscribe(command: SubscribeCommand) -> Result<WriteOutput, CliError> {
    let context = CommandContext::new("subscribe", &command.common)?;
    let tool = context.tool();
    let payload = EventPayload::Subscription(SubscriptionPayload {
        tool: tool.clone(),
        paths: command.paths.clone(),
        event_kinds: command.event_kinds.clone(),
        threads: command.threads.clone(),
        tasks: command.tasks.clone(),
    });
    let entry = context.append(context.event(
        payload,
        &tool,
        new_id("thr"),
        format!("subscription {tool}"),
    ))?;
    Ok(context.output(
        &entry,
        format!(
            "updated subscription {} local_seq={}",
            tool,
            local_seq(&entry).unwrap_or_default()
        ),
        json!({
            "tool": tool,
            "paths": command.paths,
            "event_kinds": command.event_kinds,
            "threads": command.threads,
            "tasks": command.tasks,
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
        "profile",
        "task",
        "artifact",
        "decision",
        "lesson",
        "subscription",
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

struct CommandContext<'a> {
    command: &'static str,
    common: &'a CommonOptions,
    store: ChannelStore,
}

impl<'a> CommandContext<'a> {
    fn new(command: &'static str, common: &'a CommonOptions) -> Result<Self, CliError> {
        Ok(Self {
            command,
            common,
            store: common.channel_store(command)?,
        })
    }

    fn tool(&self) -> String {
        self.common.tool()
    }

    fn event(
        &self,
        payload: EventPayload,
        tool: &str,
        thread_id: String,
        subject: String,
    ) -> EventBuilder {
        EventBuilder::new(
            new_id("evt"),
            payload,
            tool,
            self.common.run_id(),
            thread_id,
        )
        .model(self.common.model())
        .subject(subject)
        .time(now_rfc3339())
    }

    fn append(&self, event: EventBuilder) -> Result<Value, CliError> {
        if !self.common.sign {
            return self.store.append_typed(event).map_err(|err| {
                CliError::runtime(self.command, format!("failed to append event: {err}"))
            });
        }
        let identity_dir = self.common.identity_dir()?;
        let identity = load_signing_identity(&identity_dir, self.common.key_id.as_deref())
            .map_err(|err| {
                CliError::runtime(
                    self.command,
                    format!("failed to load signing identity: {err}"),
                )
            })?;
        let mut event = event.build().map_err(|err| {
            CliError::runtime(self.command, format!("failed to build event: {err}"))
        })?;
        sign_event(&mut event, &identity, &now_rfc3339()).map_err(|err| {
            CliError::runtime(self.command, format!("failed to sign event: {err}"))
        })?;
        self.store.append_event(event).map_err(|err| {
            CliError::runtime(self.command, format!("failed to append event: {err}"))
        })
    }

    fn output(&self, entry: &Value, text: String, extra: Value) -> WriteOutput {
        let mut body = json!({
            "ok": true,
            "command": self.command,
            "schema": format!("agent-rally.command.{}.v1", self.command),
            "channel": self.store.channel_dir().display().to_string(),
            "event_id": event_field(entry, "id"),
            "local_seq": local_seq(entry),
            "event": entry.get("event").cloned().unwrap_or(Value::Null),
        });
        if let (Some(object), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            object.extend(extra.clone());
        }
        WriteOutput {
            json: self.common.json,
            text,
            body,
        }
    }

    fn resolve_target(
        &self,
        identifier: &str,
        kind: &'static str,
        force: bool,
        force_action: &'static str,
    ) -> Result<ResolvedTarget, CliError> {
        let records = self.store.load_records().map_err(|err| {
            CliError::runtime(self.command, format!("failed to load channel: {err}"))
        })?;
        let record = find_record(&records, identifier, Some(kind));
        if record.is_none() && !force {
            return Err(CliError::not_found(
                self.command,
                format!(
                    "no {kind} found for {identifier:?}; pass --force to {force_action} anyway"
                ),
            ));
        }
        let reference = record
            .as_ref()
            .and_then(|value| event_field(value, "id"))
            .unwrap_or_else(|| identifier.to_string());
        let thread_id = record
            .as_ref()
            .and_then(|value| event_field(value, "thread_id"))
            .unwrap_or_else(|| new_id("thr"));
        let correlation_id = record
            .as_ref()
            .and_then(|value| event_field(value, "correlation_id"));
        Ok(ResolvedTarget {
            reference,
            thread_id,
            correlation_id,
            resolved: record.is_some(),
        })
    }
}

struct ResolvedTarget {
    reference: String,
    thread_id: String,
    correlation_id: Option<String>,
    resolved: bool,
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

fn event_field(record: &Value, key: &str) -> Option<String> {
    event_value(record)
        .ok()
        .and_then(|event| event.get(key).and_then(Value::as_str).map(str::to_string))
}

fn local_seq(entry: &Value) -> Option<u64> {
    entry.get("local_seq").and_then(Value::as_u64)
}
