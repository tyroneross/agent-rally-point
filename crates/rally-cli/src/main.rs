// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, SecondsFormat, Utc};
use rally_core::store::{ChannelStore, load_records};
use rally_core::sync::{SyncError, SyncErrorKind, build_sync_packet, import_sync_packet};
use rally_protocol::{event_id, sha256_hash};
use rally_trust::{
    PublicKeyStore, TrustContext, TrustPolicy, TrustStatus, classify, classify_with_policy,
    load_trust_file,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod output;
mod query_commands;
mod write_commands;

use output::{CliError, WriteOutput};
use query_commands::{
    execute_blockers, execute_claims, execute_conflicts, execute_diagnose, execute_inbox,
    execute_replay, execute_report, execute_score, execute_thread, query_records,
};
use write_commands::{
    execute_ack, execute_blocker, execute_claim, execute_handoff, execute_identity_init,
    execute_preflight, execute_release, execute_unblock,
};

static FALLBACK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rally: {err}");
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
        Some("preflight") => run_write("preflight", args, parse_preflight, execute_preflight),
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
        Some("inbox") => run_read("inbox", args, parse_read, execute_inbox),
        Some("claims") => run_read("claims", args, parse_read, execute_claims),
        Some("blockers") => run_read("blockers", args, parse_read, execute_blockers),
        Some("conflicts") => run_read("conflicts", args, parse_read, execute_conflicts),
        Some("diagnose") => run_read("diagnose", args, parse_read, execute_diagnose),
        Some("score") => run_read("score", args, parse_read, execute_score),
        Some("thread") => run_read("thread", args, parse_read, execute_thread),
        Some("replay") => run_read("replay", args, parse_read, execute_replay),
        Some("report") => run_read("report", args, parse_read, execute_report),
        Some("identity") => run_identity(args),
        Some("sync") => run_sync(args),
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_identity(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    if args.first().is_some_and(|value| value == "init") {
        args.remove(0);
        return run_write(
            "identity:init",
            args.into_iter(),
            parse_identity_init,
            execute_identity_init,
        );
    }
    usage();
    Ok(ExitCode::from(2))
}

fn run_sync(args: impl Iterator<Item = String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        Some("export") => {
            args.remove(0);
            run_read(
                "sync:export",
                args.into_iter(),
                parse_sync_export,
                execute_sync_export,
            )
        }
        Some("import") => {
            args.remove(0);
            run_write(
                "sync:import",
                args.into_iter(),
                parse_sync_import,
                execute_sync_import,
            )
        }
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

fn run_read<T>(
    command: &'static str,
    args: impl Iterator<Item = String>,
    parse: fn(WriteArgs) -> Result<T, CliError>,
    execute: fn(T) -> Result<WriteOutput, CliError>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    run_write(command, args, parse, execute)
}

#[derive(Clone, Debug)]
struct CommonOptions {
    channel_dir: Option<PathBuf>,
    json: bool,
    tool: Option<String>,
    model: Option<String>,
    run_id: Option<String>,
    identity_dir: Option<PathBuf>,
    key_id: Option<String>,
    sign: bool,
}

impl CommonOptions {
    fn new() -> Self {
        Self {
            channel_dir: None,
            json: false,
            tool: None,
            model: None,
            run_id: None,
            identity_dir: None,
            key_id: None,
            sign: false,
        }
    }

    fn channel_store(&self, command: &'static str) -> Result<ChannelStore, CliError> {
        self.channel_dir
            .clone()
            .map(ChannelStore::new)
            .ok_or_else(|| CliError::usage(command, "--channel-dir is required"))
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
            .unwrap_or_else(|| "rally-cli".to_string())
    }

    fn identity_dir(&self) -> Result<PathBuf, CliError> {
        self.identity_dir
            .clone()
            .or_else(default_identity_dir)
            .ok_or_else(|| CliError::usage("identity", "HOME is required for default identity dir"))
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
                "--identity-dir" => {
                    common.identity_dir =
                        Some(PathBuf::from(take_value(command, &arg, &mut args)?));
                }
                "--key-id" => common.key_id = Some(take_value(command, &arg, &mut args)?),
                "--no-ack"
                | "--force"
                | "--ids"
                | "--sign"
                | "--start-ping"
                | "--human"
                | "--no-default-trust-policy" => {
                    if arg == "--sign" {
                        common.sign = true;
                    }
                    bools.push(arg)
                }
                "--files" => {
                    let mut values = Vec::new();
                    while args.peek().is_some_and(|value| !value.starts_with('-')) {
                        values.push(args.next().expect("peeked value exists"));
                    }
                    flags.insert(arg, values);
                }
                "--to"
                | "--subject"
                | "--notes"
                | "--summary"
                | "--reason"
                | "--resource"
                | "--path"
                | "--severity"
                | "--resolution"
                | "--since"
                | "--thread"
                | "--limit"
                | "--stale-after-seconds"
                | "--session-id"
                | "--origin"
                | "--trust-policy" => {
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

fn parse_usize(command: &'static str, flag: &str, value: &str) -> Result<usize, CliError> {
    value
        .parse()
        .map_err(|_| CliError::usage(command, format!("{flag} must be a positive integer")))
}

fn parse_i64(command: &'static str, flag: &str, value: &str) -> Result<i64, CliError> {
    value
        .parse()
        .map_err(|_| CliError::usage(command, format!("{flag} must be an integer")))
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

#[derive(Debug)]
struct ReadCommand {
    command: &'static str,
    common: CommonOptions,
    identifier: Option<String>,
    since: Option<String>,
    tool: Option<String>,
    thread: Option<String>,
    limit: usize,
    ids: bool,
    stale_after_seconds: i64,
}

#[derive(Debug)]
struct IdentityInitCommand {
    common: CommonOptions,
    tool: String,
}

#[derive(Debug)]
struct SyncExportCommand {
    read: ReadCommand,
}

#[derive(Debug)]
struct SyncImportCommand {
    common: CommonOptions,
    packet_path: PathBuf,
    origin: String,
    trust_policy: Option<PathBuf>,
    no_default_trust_policy: bool,
}

#[derive(Debug)]
struct PreflightCommand {
    common: CommonOptions,
    session_id: String,
    start_ping: bool,
    stale_after_seconds: i64,
}

fn parse_identity_init(args: WriteArgs) -> Result<IdentityInitCommand, CliError> {
    Ok(IdentityInitCommand {
        tool: args
            .common
            .tool
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        common: args.common,
    })
}

fn parse_preflight(args: WriteArgs) -> Result<PreflightCommand, CliError> {
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

fn parse_sync_export(args: WriteArgs) -> Result<SyncExportCommand, CliError> {
    Ok(SyncExportCommand {
        read: parse_read(args)?,
    })
}

fn parse_sync_import(args: WriteArgs) -> Result<SyncImportCommand, CliError> {
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

fn parse_read(args: WriteArgs) -> Result<ReadCommand, CliError> {
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
    Ok(ReadCommand {
        command: args.command,
        since: args.one("--since"),
        tool: args.common.tool.clone(),
        thread: args.one("--thread"),
        limit,
        ids: args.has("--ids"),
        stale_after_seconds,
        common: args.common,
        identifier,
    })
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

fn execute_sync_export(command: SyncExportCommand) -> Result<WriteOutput, CliError> {
    let (store, records, _now) = query_records(&command.read)?;
    let packet = build_sync_packet(
        store.channel_dir().display().to_string(),
        now_rfc3339(),
        &records,
    )
    .map_err(|err| sync_error("sync:export", err))?;
    let text = serde_json::to_string_pretty(&packet)
        .map_err(|err| CliError::runtime("sync:export", err.to_string()))?;
    Ok(WriteOutput {
        json: command.read.common.json,
        text,
        body: packet,
    })
}

fn execute_sync_import(command: SyncImportCommand) -> Result<WriteOutput, CliError> {
    let store = command.common.channel_store("sync:import")?;
    let packet = read_sync_packet(&command.packet_path)?;
    let trust = load_trust_for_sync(
        command.trust_policy.as_ref(),
        command.no_default_trust_policy,
    )
    .map_err(|err| CliError::runtime("sync:import", err))?;
    let summary = import_sync_packet(&store, &packet, &command.origin, |event| {
        classify_status_for_import(event, trust.as_ref())
            .map(|status| status.to_string())
            .map_err(|err| SyncError::new(err.to_string()))
    })
    .map_err(|err| sync_error("sync:import", err))?;

    let body = json!({
        "ok": true,
        "command": "sync:import",
        "schema": "agent-rally.command.sync-import.v1",
        "channel": store.channel_dir().display().to_string(),
        "origin": command.origin,
        "data": {
            "imported": summary.imported,
            "duplicates": summary.duplicates,
            "conflicts": summary.conflicts,
            "invalid": summary.invalid,
            "trust_counts": summary.trust_counts,
        }
    });
    Ok(WriteOutput {
        json: command.common.json,
        text: format!(
            "imported={} duplicates={} conflicts={} invalid={}",
            summary.imported,
            summary.duplicates,
            body["data"]["conflicts"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
            summary.invalid
        ),
        body,
    })
}

fn read_sync_packet(path: &PathBuf) -> Result<Value, CliError> {
    let text = fs::read_to_string(path)
        .map_err(|err| CliError::runtime("sync:import", format!("failed to read packet: {err}")))?;
    serde_json::from_str(&text)
        .map_err(|err| CliError::runtime("sync:import", format!("invalid packet JSON: {err}")))
}

fn sync_error(command: &'static str, err: SyncError) -> CliError {
    match err.kind() {
        SyncErrorKind::Usage => CliError::usage(command, err.to_string()),
        SyncErrorKind::Runtime => CliError::runtime(command, err.to_string()),
    }
}

fn classify_status_for_import(
    event: &Value,
    trust: Option<&LoadedTrust>,
) -> Result<TrustStatus, rally_trust::TrustError> {
    let classification = if let Some(context) = trust {
        classify_with_policy(event, &context.keys, Some(&context.policy))?
    } else {
        classify(event, &PublicKeyStore::new())?
    };
    Ok(classification.status)
}

fn load_trust_for_sync(
    trust_policy: Option<&PathBuf>,
    no_default_trust_policy: bool,
) -> Result<Option<LoadedTrust>, String> {
    let Some(path) = trust_policy.cloned().or_else(|| {
        (!no_default_trust_policy)
            .then(default_trust_policy_path)
            .flatten()
    }) else {
        return Ok(None);
    };
    if trust_policy.is_none() && !path.exists() {
        return Ok(None);
    }
    let TrustContext { keys, policy } =
        load_trust_file(&path).map_err(|err| format!("failed to load trust policy: {err}"))?;
    Ok(Some(LoadedTrust {
        keys,
        policy,
        source: Some(path.display().to_string()),
    }))
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
        "usage: rally verify [--json] [--trust-policy <trust.toml>] [--no-default-trust-policy] <changes.jsonl>"
    );
    eprintln!(
        "       rally handoff --channel-dir <dir> --to <tool> --subject <text> [--from-tool <tool>] [--files <path>...] [--notes <text>] [--no-ack] [--sign] [--json]"
    );
    eprintln!(
        "       rally preflight --channel-dir <dir> --tool <tool> [--session-id <id>] [--start-ping] [--json]"
    );
    eprintln!(
        "       rally ack|reject|needs-info --channel-dir <dir> [--tool <tool>] [--force] <handoff-id>"
    );
    eprintln!(
        "       rally claim --channel-dir <dir> --path <path>|--resource <id> --subject <text>"
    );
    eprintln!(
        "       rally release --channel-dir <dir> [--force] <claim-id> | blocker --channel-dir <dir> --subject <text> | unblock --channel-dir <dir> [--force] <blocker-id> --resolution <text>"
    );
    eprintln!(
        "       rally inbox|claims|blockers|conflicts|diagnose|score|report|replay --channel-dir <dir> [--json] [--since <window>] [--tool <tool>]"
    );
    eprintln!("       rally thread --channel-dir <dir> [--json] <event-id>");
    eprintln!("       rally identity init [--identity-dir <dir>] --tool <tool> [--json]");
    eprintln!(
        "       rally sync export --channel-dir <dir> [--json] [--since <window>] > packet.json"
    );
    eprintln!(
        "       rally sync import --channel-dir <dir> [--json] [--trust-policy <trust.toml>] <packet.json>"
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
    default_trust_policy_path()
}

fn default_trust_policy_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity/trust.toml"))
}

fn default_identity_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity"))
}
