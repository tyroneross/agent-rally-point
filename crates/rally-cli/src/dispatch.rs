// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::args::{
    WriteArgs, parse_ack, parse_artifact, parse_blocker, parse_claim, parse_decision,
    parse_handoff, parse_herdr_inject, parse_identity_init, parse_lesson, parse_preflight,
    parse_profile, parse_read, parse_release, parse_subscribe, parse_sync_export,
    parse_sync_import, parse_task, parse_unblock,
};
use crate::output::{CliError, WriteOutput};
use crate::query_commands::{
    execute_adapter_contract, execute_blockers, execute_checkpoint_rebuild,
    execute_checkpoint_status, execute_claims, execute_cmux_packet, execute_conflicts,
    execute_context, execute_diagnose, execute_herdr_inject, execute_herdr_packet, execute_inbox,
    execute_packet, execute_replay, execute_report, execute_score, execute_thread,
};
use crate::sync_commands::{execute_sync_export, execute_sync_import};
use crate::verify_commands::{VerifyOptions, verify};
use crate::write_commands::{
    execute_ack, execute_artifact, execute_blocker, execute_claim, execute_decision,
    execute_handoff, execute_identity_init, execute_lesson, execute_preflight, execute_profile,
    execute_release, execute_subscribe, execute_task, execute_unblock,
};
use serde_json::json;
use std::env;
use std::process::ExitCode;

pub(crate) fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("verify") => run_verify(args),
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
        Some("profile") => run_write("profile", args, parse_profile, execute_profile),
        Some("task") => run_write("task", args, parse_task, execute_task),
        Some("artifact") => run_write("artifact", args, parse_artifact, execute_artifact),
        Some("decision") => run_write("decision", args, parse_decision, execute_decision),
        Some("lesson") => run_write("lesson", args, parse_lesson, execute_lesson),
        Some("subscribe") => run_write("subscribe", args, parse_subscribe, execute_subscribe),
        Some("inbox") => run_read("inbox", args, parse_read, execute_inbox),
        Some("claims") => run_read("claims", args, parse_read, execute_claims),
        Some("blockers") => run_read("blockers", args, parse_read, execute_blockers),
        Some("conflicts") => run_read("conflicts", args, parse_read, execute_conflicts),
        Some("context") => run_read("context", args, parse_read, execute_context),
        Some("packet") => run_read("packet", args, parse_read, execute_packet),
        Some("diagnose") => run_read("diagnose", args, parse_read, execute_diagnose),
        Some("score") => run_read("score", args, parse_read, execute_score),
        Some("thread") => run_read("thread", args, parse_read, execute_thread),
        Some("replay") => run_read("replay", args, parse_read, execute_replay),
        Some("report") => run_read("report", args, parse_read, execute_report),
        Some("identity") => run_identity(args),
        Some("sync") => run_sync(args),
        Some("herdr") => run_herdr(args),
        Some("cmux") => run_cmux(args),
        Some("adapter") => run_adapter(args),
        Some("checkpoint") => run_checkpoint(args),
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_herdr(args: impl Iterator<Item = String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        Some("inject") => {
            args.remove(0);
            run_write(
                "herdr:inject",
                args.into_iter(),
                parse_herdr_inject,
                execute_herdr_inject,
            )
        }
        Some("packet") => {
            args.remove(0);
            run_read(
                "herdr:packet",
                args.into_iter(),
                parse_read,
                execute_herdr_packet,
            )
        }
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_cmux(args: impl Iterator<Item = String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        Some("packet") => {
            args.remove(0);
            run_read(
                "cmux:packet",
                args.into_iter(),
                parse_read,
                execute_cmux_packet,
            )
        }
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_adapter(args: impl Iterator<Item = String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        Some("contract") => {
            args.remove(0);
            run_read(
                "adapter:contract",
                args.into_iter(),
                parse_read,
                execute_adapter_contract,
            )
        }
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_checkpoint(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        Some("status") => {
            args.remove(0);
            run_read(
                "checkpoint:status",
                args.into_iter(),
                parse_read,
                execute_checkpoint_status,
            )
        }
        Some("rebuild") => {
            args.remove(0);
            run_read(
                "checkpoint:rebuild",
                args.into_iter(),
                parse_read,
                execute_checkpoint_rebuild,
            )
        }
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

fn run_verify(args: impl Iterator<Item = String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
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

fn usage() {
    eprintln!(
        "usage: rally verify [--json] [--trust-policy <trust.toml>] [--no-default-trust-policy] <changes.jsonl>"
    );
    eprintln!(
        "       rally handoff --to <tool> --subject <text> [--from-tool <tool>] [--files <path>...] [--notes <text>] [--no-ack] [--sign] [--json]"
    );
    eprintln!("       rally preflight --tool <tool> [--session-id <id>] [--start-ping] [--json]");
    eprintln!("       rally ack|reject|needs-info [--tool <tool>] [--force] <handoff-id>");
    eprintln!("       rally claim --path <path>|--resource <id> --subject <text>");
    eprintln!(
        "       rally release [--force] <claim-id> | blocker --subject <text> | unblock [--force] <blocker-id> --resolution <text>"
    );
    eprintln!(
        "       rally profile [--tool <tool>] [--role <name>] [--capability <name>]... [--watch <path>]... [--current-task <id>] [--branch <name>] [--availability <state>] [--json]"
    );
    eprintln!(
        "       rally task --subject <text> [--owner <tool>] [--status <state>] [--depends-on <id>]... [--artifact <id>]... [--verification <text>] [--json]"
    );
    eprintln!(
        "       rally artifact --subject <text> [--artifact-kind <kind>] [--uri <uri>] [--ref-task <id>] [--summary <text>] [--json]"
    );
    eprintln!(
        "       rally decision --subject <text> [--status <state>] [--scope <scope>] [--supersedes <id>]... [--rationale <text>] [--json]"
    );
    eprintln!(
        "       rally lesson --subject <text> [--lesson-kind <kind>] [--source-event <id>]... [--confidence <0-1>] [--json]"
    );
    eprintln!(
        "       rally subscribe [--tool <tool>] [--path <path>]... [--event-kind <kind>]... [--thread <id>]... [--task <id>]... [--json]"
    );
    eprintln!(
        "       rally context|packet|inbox|claims|blockers|conflicts|diagnose|score|report|replay [--json] [--since <window>] [--tool <tool>]"
    );
    eprintln!("       rally thread [--json] <event-id>");
    eprintln!("       rally identity init [--identity-dir <dir>] --tool <tool> [--json]");
    eprintln!("       rally sync export [--json] [--since <window>] > packet.json");
    eprintln!("       rally sync import [--json] [--trust-policy <trust.toml>] <packet.json>");
    eprintln!("       rally herdr inject [--json] [--force] <handoff-id>");
    eprintln!("       rally adapter contract [--json]");
    eprintln!("       rally cmux packet [--tool <tool>] [--json]");
    eprintln!("       rally herdr packet [--tool <tool>] [--json]");
    eprintln!("       rally checkpoint status|rebuild [--json]");
}
