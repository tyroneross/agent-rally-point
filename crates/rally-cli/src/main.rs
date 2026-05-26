// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, SecondsFormat, Utc};
use rally_core::event::{
    AckPayload, BlockerPayload, BlockerResolvedPayload, ClaimPayload, ClaimReleasePayload,
    EventBuilder, EventPayload, HandoffPayload,
};
use rally_core::store::{ChannelStore, load_records};
use rally_protocol::{event_id, event_value, sha256_hash};
use rally_trust::{
    PublicKeyStore, TrustContext, TrustPolicy, TrustStatus, classify, classify_with_policy,
    load_trust_file,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static FALLBACK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rally-rs: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("verify") => {
            let Some(options) = VerifyOptions::parse(args)? else {
                usage();
                return Ok(ExitCode::from(2));
            };
            match verify(&options) {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(err) if options.json => {
                    eprintln!(
                        "{}",
                        json!({
                            "ok": false,
                            "command": "verify",
                            "error": err.to_string(),
                            "exit_code": 1,
                        })
                    );
                    Ok(ExitCode::FAILURE)
                }
                Err(err) => Err(err),
            }
        }
        Some("handoff") => run_write("handoff", args, parse_handoff, execute_handoff),
        Some("ack") => run_write("ack", args, |args| parse_ack(args, "done"), execute_ack),
        Some("reject") => run_write(
            "reject",
            args,
            |args| parse_ack(args, "rejected"),
            execute_ack,
        ),
        Some("needs-info") => run_write(
            "needs-info",
            args,
            |args| parse_ack(args, "needs-info"),
            execute_ack,
        ),
        Some("claim") => run_write("claim", args, parse_claim, execute_claim),
        Some("release") => run_write("release", args, parse_release, execute_release),
        Some("blocker") => run_write("blocker", args, parse_blocker, execute_blocker),
        Some("unblock") => run_write("unblock", args, parse_unblock, execute_unblock),
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_write<T>(
    command: &'static str,
    args: impl Iterator<Item = String>,
    parse: fn(WriteArgs) -> Result<T, CliError>,
    execute: fn(T) -> Result<WriteOutput, CliError>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args: Vec<String> = args.collect();
    let wants_json = args.iter().any(|arg| arg == "--json");
    let args = match WriteArgs::parse(command, args.into_iter()) {
        Ok(args) => args,
        Err(mut err) => {
            err.json = wants_json;
            err.emit();
            return Ok(ExitCode::from(err.exit_code));
        }
    };
    let json = args.common.json;
    match parse(args).and_then(execute) {
        Ok(output) => {
            output.emit();
            Ok(ExitCode::SUCCESS)
        }
        Err(mut err) => {
            err.json |= json;
            err.emit();
            Ok(ExitCode::from(err.exit_code))
        }
    }
}

#[derive(Clone, Debug)]
struct CommonOptions {
    channel_dir: Option<PathBuf>,
    json: bool,
    tool: Option<String>,
    model: Option<String>,
    run_id: Option<String>,
}

impl CommonOptions {
    fn new() -> Self {
        Self {
            channel_dir: None,
            json: false,
            tool: None,
            model: None,
            run_id: None,
        }
    }

    fn channel_store(&self, command: &'static str) -> Result<ChannelStore, CliError> {
        self.channel_dir
            .clone()
            .map(ChannelStore::new)
            .ok_or_else(|| CliError::usage(command, "--channel-dir is required for Rust writes"))
    }

    fn tool(&self) -> String {
        self.tool.clone().unwrap_or_else(|| "unknown".to_string())
    }

    fn model(&self) -> String {
        self.model.clone().unwrap_or_else(|| "unknown".to_string())
    }

    fn run_id(&self) -> String {
        self.run_id
            .clone()
            .unwrap_or_else(|| "agent-rally-cli-rs".to_string())
    }
}

#[derive(Clone, Debug)]
struct WriteArgs {
    command: &'static str,
    common: CommonOptions,
    positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    bools: Vec<String>,
}

impl WriteArgs {
    fn parse(command: &'static str, args: impl Iterator<Item = String>) -> Result<Self, CliError> {
        let mut common = CommonOptions::new();
        let mut positional = Vec::new();
        let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut bools = Vec::new();
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => common.json = true,
                "--channel-dir" => {
                    common.channel_dir = Some(PathBuf::from(take_value(command, &arg, &mut args)?));
                }
                "--tool" | "--from-tool" => {
                    let value = take_value(command, &arg, &mut args)?;
                    if arg == "--tool" {
                        common.tool = Some(value);
                    } else {
                        flags.entry(arg).or_default().push(value);
                    }
                }
                "--model" => common.model = Some(take_value(command, &arg, &mut args)?),
                "--run-id" => common.run_id = Some(take_value(command, &arg, &mut args)?),
                "--no-ack" | "--force" => bools.push(arg),
                "--files" => {
                    let mut values = Vec::new();
                    while args.peek().is_some_and(|value| !value.starts_with('-')) {
                        values.push(args.next().expect("peeked value exists"));
                    }
                    flags.insert(arg, values);
                }
                "--to" | "--subject" | "--notes" | "--summary" | "--reason" | "--resource"
                | "--path" | "--severity" | "--resolution" => {
                    let value = take_value(command, &arg, &mut args)?;
                    flags.entry(arg).or_default().push(value);
                }
                value if value.starts_with('-') => {
                    return Err(CliError::usage(command, format!("unknown option {value}")));
                }
                value => positional.push(value.to_string()),
            }
        }

        Ok(Self {
            command,
            common,
            positional,
            flags,
            bools,
        })
    }

    fn one(&self, flag: &str) -> Option<String> {
        self.flags
            .get(flag)
            .and_then(|values| values.last())
            .cloned()
    }

    fn required(&self, flag: &str) -> Result<String, CliError> {
        self.one(flag)
            .ok_or_else(|| CliError::usage(self.command, format!("{flag} is required")))
    }

    fn has(&self, flag: &str) -> bool {
        self.bools.iter().any(|value| value == flag)
    }

    fn files(&self) -> Vec<String> {
        self.flags.get("--files").cloned().unwrap_or_default()
    }

    fn identifier(&self) -> Result<String, CliError> {
        match self.positional.as_slice() {
            [value] => Ok(value.clone()),
            [] => Err(CliError::usage(
                self.command,
                format!("{} requires an identifier", self.command),
            )),
            _ => Err(CliError::usage(
                self.command,
                format!("{} accepts one identifier", self.command),
            )),
        }
    }
}

fn take_value(
    command: &'static str,
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<String, CliError> {
    args.next()
        .ok_or_else(|| CliError::usage(command, format!("{flag} requires a value")))
}

#[derive(Debug)]
struct CliError {
    command: &'static str,
    message: String,
    exit_code: u8,
    json: bool,
}

impl CliError {
    fn usage(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 2,
            json: false,
        }
    }

    fn runtime(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 1,
            json: false,
        }
    }

    fn not_found(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 3,
            json: false,
        }
    }

    fn emit(&self) {
        if self.json {
            eprintln!(
                "{}",
                json!({
                    "ok": false,
                    "command": self.command,
                    "error": self.message,
                    "exit_code": self.exit_code,
                })
            );
        } else {
            eprintln!("rally-rs {}: {}", self.command, self.message);
        }
    }
}

#[derive(Debug)]
struct WriteOutput {
    json: bool,
    text: String,
    body: Value,
}

impl WriteOutput {
    fn emit(&self) {
        if self.json {
            println!("{}", self.body);
        } else {
            println!("{}", self.text);
        }
    }
}

#[derive(Debug)]
struct HandoffCommand {
    common: CommonOptions,
    to_tool: String,
    from_tool: String,
    subject: String,
    requires_ack: bool,
    files: Vec<String>,
    notes: Option<String>,
}

#[derive(Debug)]
struct AckCommand {
    command: &'static str,
    common: CommonOptions,
    identifier: String,
    verdict: String,
    summary: Option<String>,
    reason: Option<String>,
    force: bool,
}

#[derive(Debug)]
struct ClaimCommand {
    common: CommonOptions,
    resource: String,
    subject: String,
    notes: Option<String>,
}

#[derive(Debug)]
struct ReleaseCommand {
    common: CommonOptions,
    identifier: String,
    reason: Option<String>,
    force: bool,
}

#[derive(Debug)]
struct BlockerCommand {
    common: CommonOptions,
    subject: String,
    reason: String,
    severity: Option<String>,
    resource: Option<String>,
}

#[derive(Debug)]
struct UnblockCommand {
    common: CommonOptions,
    identifier: String,
    resolution: String,
    force: bool,
}

fn parse_handoff(args: WriteArgs) -> Result<HandoffCommand, CliError> {
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

fn parse_ack(args: WriteArgs, verdict: &'static str) -> Result<AckCommand, CliError> {
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

fn parse_claim(args: WriteArgs) -> Result<ClaimCommand, CliError> {
    Ok(ClaimCommand {
        resource: resource_arg(&args, true)?.expect("required resource exists"),
        subject: args.required("--subject")?,
        notes: args.one("--notes"),
        common: args.common,
    })
}

fn parse_release(args: WriteArgs) -> Result<ReleaseCommand, CliError> {
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

fn parse_blocker(args: WriteArgs) -> Result<BlockerCommand, CliError> {
    let subject = args.required("--subject")?;
    Ok(BlockerCommand {
        reason: args.one("--reason").unwrap_or_else(|| subject.clone()),
        severity: args.one("--severity"),
        resource: resource_arg(&args, false)?,
        common: args.common,
        subject,
    })
}

fn parse_unblock(args: WriteArgs) -> Result<UnblockCommand, CliError> {
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

fn resource_arg(args: &WriteArgs, required: bool) -> Result<Option<String>, CliError> {
    if let Some(resource) = args.one("--resource") {
        return Ok(Some(resource));
    }
    if let Some(path) = args.one("--path") {
        return Ok(Some(format!("file:{}", path.replace('\\', "/"))));
    }
    if required {
        Err(CliError::usage(
            args.command,
            "--resource or --path is required",
        ))
    } else {
        Ok(None)
    }
}

fn execute_handoff(command: HandoffCommand) -> Result<WriteOutput, CliError> {
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

fn execute_ack(command: AckCommand) -> Result<WriteOutput, CliError> {
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
    let entry = append(&store, builder, command.command)?;
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

fn execute_claim(command: ClaimCommand) -> Result<WriteOutput, CliError> {
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

fn execute_release(command: ReleaseCommand) -> Result<WriteOutput, CliError> {
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

fn execute_blocker(command: BlockerCommand) -> Result<WriteOutput, CliError> {
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

fn execute_unblock(command: UnblockCommand) -> Result<WriteOutput, CliError> {
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

fn append(
    store: &ChannelStore,
    event: EventBuilder,
    command: &'static str,
) -> Result<Value, CliError> {
    store
        .append_typed(event)
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

fn now_rfc3339() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn new_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return format!("{prefix}_{}", hex_bytes(&bytes));
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let counter = FALLBACK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{prefix}:{nanos}:{}:{counter}", std::process::id());
    let hash = sha256_hash(seed.as_bytes());
    format!("{prefix}_{}", &hash["sha256:".len().."sha256:".len() + 32])
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[derive(Debug)]
struct VerifyOptions {
    path: String,
    json: bool,
    trust_policy: Option<PathBuf>,
    no_default_trust_policy: bool,
}

impl VerifyOptions {
    fn parse(
        args: impl Iterator<Item = String>,
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let mut options = Self {
            path: String::new(),
            json: false,
            trust_policy: None,
            no_default_trust_policy: false,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => options.json = true,
                "--no-default-trust-policy" => options.no_default_trust_policy = true,
                "--trust-policy" => {
                    let Some(path) = args.next() else {
                        return Err("--trust-policy requires a path".into());
                    };
                    options.trust_policy = Some(PathBuf::from(path));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option {value}").into());
                }
                value => {
                    if !options.path.is_empty() {
                        return Err(format!("unexpected extra argument {value}").into());
                    }
                    options.path = value.to_string();
                }
            }
        }
        Ok((!options.path.is_empty()).then_some(options))
    }
}

fn usage() {
    eprintln!(
        "usage: rally-rs verify [--json] [--trust-policy <trust.toml>] [--no-default-trust-policy] <changes.jsonl>"
    );
    eprintln!(
        "       rally-rs handoff --channel-dir <dir> --to <tool> --subject <text> [--from-tool <tool>] [--files <path>...] [--notes <text>] [--no-ack] [--json]"
    );
    eprintln!(
        "       rally-rs ack|reject|needs-info --channel-dir <dir> [--tool <tool>] [--force] <handoff-id>"
    );
    eprintln!(
        "       rally-rs claim --channel-dir <dir> --path <path>|--resource <id> --subject <text>"
    );
    eprintln!(
        "       rally-rs release --channel-dir <dir> [--force] <claim-id> | blocker --channel-dir <dir> --subject <text> | unblock --channel-dir <dir> [--force] <blocker-id> --resolution <text>"
    );
}

fn verify(options: &VerifyOptions) -> Result<(), Box<dyn std::error::Error>> {
    let records = load_records(&options.path)?;
    let trust = load_trust_context(options)?;
    let mut counts: BTreeMap<TrustStatus, usize> = BTreeMap::new();
    let mut json_events = Vec::new();

    for record in &records {
        let classification = if let Some(context) = trust.as_ref() {
            classify_with_policy(record, &context.keys, Some(&context.policy))?
        } else {
            classify(record, &PublicKeyStore::new())?
        };
        *counts.entry(classification.status).or_default() += 1;
        let id = event_id(record).unwrap_or_else(|_| "<missing-id>".to_string());
        if options.json {
            json_events.push(json!({
                "id": id,
                "status": classification.status,
                "key_id": classification.key_id,
            }));
        } else {
            let key = classification
                .key_id
                .as_deref()
                .map(|key_id| format!(" key_id={key_id}"))
                .unwrap_or_default();
            println!("{id} {}{key}", classification.status);
        }
    }

    if options.json {
        let trust_policy = trust.and_then(|context| context.source);
        println!(
            "{}",
            json!({
                "ok": true,
                "command": "verify",
                "schema": "agent-rally.command.verify.v1",
                "data": {
                    "records": records.len(),
                    "trust_policy": trust_policy,
                    "counts": counts,
                    "events": json_events,
                }
            })
        );
    } else {
        print!("summary records={}", records.len());
        for (status, count) in counts {
            print!(" {status}={count}");
        }
        println!();
    }
    Ok(())
}

struct LoadedTrust {
    keys: PublicKeyStore,
    policy: TrustPolicy,
    source: Option<String>,
}

fn load_trust_context(
    options: &VerifyOptions,
) -> Result<Option<LoadedTrust>, Box<dyn std::error::Error>> {
    let Some(path) = trust_policy_path(options) else {
        return Ok(None);
    };
    if options.trust_policy.is_none() && !path.exists() {
        return Ok(None);
    }
    let TrustContext { keys, policy } = load_trust_file(&path)?;
    Ok(Some(LoadedTrust {
        keys,
        policy,
        source: Some(path.display().to_string()),
    }))
}

fn trust_policy_path(options: &VerifyOptions) -> Option<PathBuf> {
    if let Some(path) = options.trust_policy.clone() {
        return Some(path);
    }
    if options.no_default_trust_policy {
        return None;
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity/trust.toml"))
}
