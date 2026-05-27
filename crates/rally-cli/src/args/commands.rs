// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use super::{CommonOptions, WriteArgs, parse_i64, parse_usize};
use crate::output::CliError;
use crate::runtime::new_id;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct HandoffCommand {
    pub(crate) common: CommonOptions,
    pub(crate) to_tool: String,
    pub(crate) from_tool: String,
    pub(crate) subject: String,
    pub(crate) requires_ack: bool,
    pub(crate) files: Vec<String>,
    pub(crate) notes: Option<String>,
}

#[derive(Debug)]
pub(crate) struct AckCommand {
    pub(crate) command: &'static str,
    pub(crate) common: CommonOptions,
    pub(crate) identifier: String,
    pub(crate) verdict: String,
    pub(crate) summary: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) force: bool,
}

#[derive(Debug)]
pub(crate) struct ClaimCommand {
    pub(crate) common: CommonOptions,
    pub(crate) resource: String,
    pub(crate) subject: String,
    pub(crate) notes: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ReleaseCommand {
    pub(crate) common: CommonOptions,
    pub(crate) identifier: String,
    pub(crate) reason: Option<String>,
    pub(crate) force: bool,
}

#[derive(Debug)]
pub(crate) struct BlockerCommand {
    pub(crate) common: CommonOptions,
    pub(crate) subject: String,
    pub(crate) reason: String,
    pub(crate) severity: Option<String>,
    pub(crate) resource: Option<String>,
}

#[derive(Debug)]
pub(crate) struct UnblockCommand {
    pub(crate) common: CommonOptions,
    pub(crate) identifier: String,
    pub(crate) resolution: String,
    pub(crate) force: bool,
}

#[derive(Debug)]
pub(crate) struct ProfileCommand {
    pub(crate) common: CommonOptions,
    pub(crate) capabilities: Vec<String>,
    pub(crate) role: Option<String>,
    pub(crate) watch: Vec<String>,
    pub(crate) current_task: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) availability: Option<String>,
    pub(crate) notes: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TaskCommand {
    pub(crate) common: CommonOptions,
    pub(crate) subject: String,
    pub(crate) status: Option<String>,
    pub(crate) owner_tool: Option<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) artifacts: Vec<String>,
    pub(crate) verification: Option<String>,
    pub(crate) notes: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ArtifactCommand {
    pub(crate) common: CommonOptions,
    pub(crate) subject: String,
    pub(crate) artifact_kind: String,
    pub(crate) uri: Option<String>,
    pub(crate) ref_task_id: Option<String>,
    pub(crate) summary: Option<String>,
}

#[derive(Debug)]
pub(crate) struct DecisionCommand {
    pub(crate) common: CommonOptions,
    pub(crate) subject: String,
    pub(crate) status: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) supersedes: Vec<String>,
    pub(crate) rationale: Option<String>,
}

#[derive(Debug)]
pub(crate) struct LessonCommand {
    pub(crate) common: CommonOptions,
    pub(crate) subject: String,
    pub(crate) lesson_kind: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) source_event_ids: Vec<String>,
    pub(crate) confidence: Option<f64>,
}

#[derive(Debug)]
pub(crate) struct SubscribeCommand {
    pub(crate) common: CommonOptions,
    pub(crate) paths: Vec<String>,
    pub(crate) event_kinds: Vec<String>,
    pub(crate) threads: Vec<String>,
    pub(crate) tasks: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct WatchCommand {
    pub(crate) common: CommonOptions,
    pub(crate) tool: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) since_cursor: bool,
    pub(crate) peek: bool,
    pub(crate) from_start: bool,
    /// Optional safety cap used primarily by tests; `None` runs forever.
    pub(crate) max_seconds: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct PostCommand {
    pub(crate) common: CommonOptions,
    pub(crate) kind: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) subject: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) causation_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ReadCommand {
    pub(crate) command: &'static str,
    pub(crate) common: CommonOptions,
    pub(crate) identifier: Option<String>,
    pub(crate) since: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) limit: usize,
    pub(crate) ids: bool,
    pub(crate) stale_after_seconds: i64,
    /// When true, scope reads to events with `local_seq` greater than the
    /// per-(tool, session_id) cursor, advancing the cursor on success unless
    /// `peek` is also set.
    pub(crate) since_cursor: bool,
    /// When true with `since_cursor`, do not advance the cursor after read.
    pub(crate) peek: bool,
    /// Session id used to scope read cursors and presence.
    pub(crate) session_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct IdentityInitCommand {
    pub(crate) common: CommonOptions,
    pub(crate) tool: String,
}

#[derive(Debug)]
pub(crate) struct SyncExportCommand {
    pub(crate) read: ReadCommand,
}

#[derive(Debug)]
pub(crate) struct SyncImportCommand {
    pub(crate) common: CommonOptions,
    pub(crate) packet_path: PathBuf,
    pub(crate) origin: String,
    pub(crate) trust_policy: Option<PathBuf>,
    pub(crate) no_default_trust_policy: bool,
}

#[derive(Debug)]
pub(crate) struct PreflightCommand {
    pub(crate) common: CommonOptions,
    pub(crate) session_id: String,
    pub(crate) start_ping: bool,
    pub(crate) stale_after_seconds: i64,
}

#[derive(Debug)]
pub(crate) struct StartCommand {
    pub(crate) common: CommonOptions,
    pub(crate) tool: String,
    pub(crate) session_id: String,
    pub(crate) stale_after_seconds: i64,
    pub(crate) limit: usize,
    pub(crate) peek: bool,
    pub(crate) human: bool,
}

#[derive(Debug)]
pub(crate) struct SetupCommand {
    pub(crate) common: CommonOptions,
    pub(crate) action: String,
    pub(crate) target: Option<String>,
}

#[derive(Debug)]
pub(crate) struct HerdrInjectCommand {
    pub(crate) common: CommonOptions,
    pub(crate) identifier: String,
    pub(crate) strict: bool,
    pub(crate) force: bool,
}

pub(crate) fn parse_identity_init(args: WriteArgs) -> Result<IdentityInitCommand, CliError> {
    Ok(IdentityInitCommand {
        tool: args
            .common
            .tool
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        common: args.common,
    })
}

pub(crate) fn parse_preflight(args: WriteArgs) -> Result<PreflightCommand, CliError> {
    let stale_after_seconds = args
        .one("--stale-after-seconds")
        .map(|value| parse_i64(args.command, "--stale-after-seconds", &value))
        .transpose()?
        .unwrap_or(300);
    let session_id = args
        .one("--session-id")
        .or_else(|| args.common.run_id.clone())
        .unwrap_or_else(|| new_id("session"));
    let start_ping = args.has("--start-ping");
    Ok(PreflightCommand {
        common: args.common,
        session_id,
        start_ping,
        stale_after_seconds,
    })
}

pub(crate) fn parse_start(args: WriteArgs) -> Result<StartCommand, CliError> {
    let tool = match (args.common.tool.clone(), args.positional.as_slice()) {
        (Some(tool), []) => tool,
        (None, [tool]) => tool.clone(),
        (Some(_), [..]) => {
            return Err(CliError::usage(
                args.command,
                "use either positional tool or --tool, not both",
            ));
        }
        (None, []) => return Err(CliError::usage(args.command, "start requires a tool id")),
        (None, [..]) => {
            return Err(CliError::usage(
                args.command,
                "start accepts at most one positional tool id",
            ));
        }
    };
    let stale_after_seconds = args
        .one("--stale-after-seconds")
        .map(|value| parse_i64(args.command, "--stale-after-seconds", &value))
        .transpose()?
        .unwrap_or(300);
    let limit = args
        .one("--limit")
        .map(|value| parse_usize(args.command, "--limit", &value))
        .transpose()?
        .unwrap_or(20);
    let session_id = args
        .one("--session-id")
        .or_else(|| args.common.run_id.clone())
        .unwrap_or_else(|| new_id("session"));
    let peek = args.has("--peek");
    let human = args.has("--human");
    Ok(StartCommand {
        common: args.common,
        tool,
        session_id,
        stale_after_seconds,
        limit,
        peek,
        human,
    })
}

pub(crate) fn parse_setup(args: WriteArgs) -> Result<SetupCommand, CliError> {
    let (action, target) = match args.positional.as_slice() {
        [] => ("status".to_string(), None),
        [action] => (action.clone(), None),
        [action, target] => (action.clone(), Some(target.clone())),
        _ => {
            return Err(CliError::usage(
                args.command,
                "setup accepts: setup | setup install <cmux|herdr> | setup enforcement <off|warn|strict>",
            ));
        }
    };
    Ok(SetupCommand {
        common: args.common,
        action,
        target,
    })
}

pub(crate) fn parse_herdr_inject(args: WriteArgs) -> Result<HerdrInjectCommand, CliError> {
    Ok(HerdrInjectCommand {
        identifier: args.identifier()?,
        strict: true,
        force: args.has("--force"),
        common: args.common,
    })
}

pub(crate) fn parse_sync_export(args: WriteArgs) -> Result<SyncExportCommand, CliError> {
    Ok(SyncExportCommand {
        read: parse_read(args)?,
    })
}

pub(crate) fn parse_sync_import(args: WriteArgs) -> Result<SyncImportCommand, CliError> {
    let packet_path = args.identifier()?;
    let origin = args
        .one("--origin")
        .unwrap_or_else(|| "import:sync".to_string());
    let trust_policy = args.one("--trust-policy").map(PathBuf::from);
    let no_default_trust_policy = args.has("--no-default-trust-policy");
    Ok(SyncImportCommand {
        packet_path: PathBuf::from(packet_path),
        origin,
        trust_policy,
        no_default_trust_policy,
        common: args.common,
    })
}

pub(crate) fn parse_profile(args: WriteArgs) -> Result<ProfileCommand, CliError> {
    Ok(ProfileCommand {
        capabilities: args.all("--capability"),
        role: args.one("--role"),
        watch: args.all("--watch"),
        current_task: args.one("--current-task"),
        branch: args.one("--branch"),
        availability: args.one("--availability"),
        notes: args.one("--notes"),
        common: args.common,
    })
}

pub(crate) fn parse_task(args: WriteArgs) -> Result<TaskCommand, CliError> {
    Ok(TaskCommand {
        subject: args.required("--subject")?,
        status: args.one("--status"),
        owner_tool: args.one("--owner").or_else(|| args.common.tool.clone()),
        depends_on: args.all("--depends-on"),
        artifacts: args.all("--artifact"),
        verification: args.one("--verification"),
        notes: args.one("--notes"),
        common: args.common,
    })
}

pub(crate) fn parse_artifact(args: WriteArgs) -> Result<ArtifactCommand, CliError> {
    Ok(ArtifactCommand {
        subject: args.required("--subject")?,
        artifact_kind: args
            .one("--artifact-kind")
            .unwrap_or_else(|| "note".to_string()),
        uri: args.one("--uri"),
        ref_task_id: args.one("--ref-task"),
        summary: args.one("--summary"),
        common: args.common,
    })
}

pub(crate) fn parse_decision(args: WriteArgs) -> Result<DecisionCommand, CliError> {
    Ok(DecisionCommand {
        subject: args.required("--subject")?,
        status: args.one("--status"),
        scope: args.one("--scope"),
        supersedes: args.all("--supersedes"),
        rationale: args.one("--rationale"),
        common: args.common,
    })
}

pub(crate) fn parse_lesson(args: WriteArgs) -> Result<LessonCommand, CliError> {
    let confidence = args
        .one("--confidence")
        .map(|value| {
            value
                .parse::<f64>()
                .map_err(|_| CliError::usage(args.command, "--confidence must be a decimal number"))
        })
        .transpose()?;
    Ok(LessonCommand {
        subject: args.required("--subject")?,
        lesson_kind: args.one("--lesson-kind"),
        scope: args.one("--scope"),
        source_event_ids: args.all("--source-event"),
        confidence,
        common: args.common,
    })
}

pub(crate) fn parse_subscribe(args: WriteArgs) -> Result<SubscribeCommand, CliError> {
    Ok(SubscribeCommand {
        paths: args.all("--path"),
        event_kinds: args.all("--event-kind"),
        threads: args.all("--thread"),
        tasks: args.all("--task"),
        common: args.common,
    })
}

pub(crate) fn parse_watch(args: WriteArgs) -> Result<WatchCommand, CliError> {
    let max_seconds = args
        .one("--max-seconds")
        .map(|v| parse_usize(args.command, "--max-seconds", &v))
        .transpose()?
        .map(|v| v as u64);
    Ok(WatchCommand {
        tool: args.common.tool.clone(),
        kind: args.one("--kind"),
        thread: args.one("--thread"),
        session_id: args.one("--session-id"),
        since_cursor: args.has("--since-cursor"),
        peek: args.has("--peek"),
        from_start: args.has("--from-start"),
        max_seconds,
        common: args.common,
    })
}

pub(crate) fn parse_post(args: WriteArgs) -> Result<PostCommand, CliError> {
    let kind = args.required("--kind")?;
    let payload_str = args.one("--payload").unwrap_or_else(|| "{}".to_string());
    let payload: serde_json::Value = serde_json::from_str(&payload_str).map_err(|err| {
        CliError::usage(args.command, format!("--payload must be valid JSON: {err}"))
    })?;
    if !payload.is_object() {
        return Err(CliError::usage(
            args.command,
            "--payload must be a JSON object",
        ));
    }
    Ok(PostCommand {
        kind,
        payload,
        subject: args.one("--subject"),
        thread_id: args.one("--thread-id"),
        causation_id: args.one("--causation-id"),
        common: args.common,
    })
}

pub(crate) fn parse_read(args: WriteArgs) -> Result<ReadCommand, CliError> {
    let limit = args
        .one("--limit")
        .map(|value| parse_usize(args.command, "--limit", &value))
        .transpose()?
        .unwrap_or(200);
    let stale_after_seconds = args
        .one("--stale-after-seconds")
        .map(|value| parse_i64(args.command, "--stale-after-seconds", &value))
        .transpose()?
        .unwrap_or(24 * 3600);
    let identifier = match args.positional.as_slice() {
        [] => None,
        [value] => Some(value.clone()),
        _ => {
            return Err(CliError::usage(
                args.command,
                format!("{} accepts at most one identifier", args.command),
            ));
        }
    };
    let since_cursor = args.has("--since-cursor");
    let peek = args.has("--peek");
    let session_id = args.one("--session-id");
    Ok(ReadCommand {
        command: args.command,
        since: args.one("--since"),
        tool: args.common.tool.clone(),
        thread: args.one("--thread"),
        limit,
        ids: args.has("--ids"),
        stale_after_seconds,
        since_cursor,
        peek,
        session_id,
        common: args.common,
        identifier,
    })
}

pub(crate) fn parse_handoff(args: WriteArgs) -> Result<HandoffCommand, CliError> {
    let from_tool = args
        .one("--from-tool")
        .or_else(|| args.common.tool.clone())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(HandoffCommand {
        to_tool: args.required("--to")?,
        subject: args.required("--subject")?,
        requires_ack: !args.has("--no-ack"),
        files: args.files(),
        notes: args.one("--notes"),
        common: args.common,
        from_tool,
    })
}

pub(crate) fn parse_ack(args: WriteArgs, verdict: &'static str) -> Result<AckCommand, CliError> {
    let identifier = args.identifier()?;
    let summary = args.one("--summary");
    let reason = args.one("--reason");
    let force = args.has("--force");
    Ok(AckCommand {
        command: args.command,
        common: args.common,
        identifier,
        verdict: verdict.to_string(),
        summary,
        reason,
        force,
    })
}

pub(crate) fn parse_claim(args: WriteArgs) -> Result<ClaimCommand, CliError> {
    Ok(ClaimCommand {
        resource: required_resource_arg(&args)?,
        subject: args.required("--subject")?,
        notes: args.one("--notes"),
        common: args.common,
    })
}

pub(crate) fn parse_release(args: WriteArgs) -> Result<ReleaseCommand, CliError> {
    let identifier = args.identifier()?;
    let reason = args.one("--reason");
    let force = args.has("--force");
    Ok(ReleaseCommand {
        common: args.common,
        identifier,
        reason,
        force,
    })
}

pub(crate) fn parse_blocker(args: WriteArgs) -> Result<BlockerCommand, CliError> {
    let subject = args.required("--subject")?;
    Ok(BlockerCommand {
        reason: args.one("--reason").unwrap_or_else(|| subject.clone()),
        severity: args.one("--severity"),
        resource: optional_resource_arg(&args),
        common: args.common,
        subject,
    })
}

pub(crate) fn parse_unblock(args: WriteArgs) -> Result<UnblockCommand, CliError> {
    let identifier = args.identifier()?;
    let resolution = args.required("--resolution")?;
    let force = args.has("--force");
    Ok(UnblockCommand {
        common: args.common,
        identifier,
        resolution,
        force,
    })
}

fn required_resource_arg(args: &WriteArgs) -> Result<String, CliError> {
    optional_resource_arg(args)
        .ok_or_else(|| CliError::usage(args.command, "--resource or --path is required"))
}

fn optional_resource_arg(args: &WriteArgs) -> Option<String> {
    if let Some(resource) = args.one("--resource") {
        return Some(resource);
    }
    if let Some(path) = args.one("--path") {
        let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        return Some(crate::resources::normalize_file_resource(&path, &workdir));
    }
    None
}
