// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally graph` — query the derived graph projection.
//!
//! Subcommands:
//!   * `rally graph init`             — create or refresh the projection DB.
//!   * `rally graph build`            — incremental catch-up from `changes.jsonl`.
//!   * `rally graph rebuild`          — full replay from scratch.
//!   * `rally graph status`           — meta (HWM, schema version, status) + counts.
//!   * `rally graph subgraph <id>`    — subgraph around a node (1-depth default).
//!   * `rally graph causal <id>`      — causal chain (back-walk on ancestor edges).
//!   * `rally graph deps <id>`        — transitive depends_on for a task.
//!   * `rally graph unconsumed`       — artifacts with no reviewed_by edge.

use crate::args::WriteArgs;
use crate::output::{CliError, WriteOutput};
use crate::runtime::now_rfc3339;
use rally_core::graph;
use rally_core::store::ChannelStore;
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

const SCHEMA: &str = "agent-rally.command.graph.v1";

pub(crate) fn run(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args: Vec<String> = args.collect();
    let wants_json = args.iter().any(|arg| arg == "--json");
    let sub = match args.first().cloned() {
        Some(value) => value,
        None => {
            emit_usage_error(
                wants_json,
                "graph requires a subcommand: init|build|rebuild|status|subgraph|causal|deps|unconsumed",
            );
            return Ok(ExitCode::from(2));
        }
    };
    args.remove(0);

    let (command, positional_required) = match sub.as_str() {
        "init" => ("graph:init", false),
        "build" => ("graph:build", false),
        "rebuild" => ("graph:rebuild", false),
        "status" => ("graph:status", false),
        "subgraph" => ("graph:subgraph", true),
        "causal" => ("graph:causal", true),
        "deps" => ("graph:deps", true),
        "unconsumed" => ("graph:unconsumed", false),
        other => {
            emit_usage_error(wants_json, &format!("unknown graph subcommand {other:?}"));
            return Ok(ExitCode::from(2));
        }
    };

    let parsed = WriteArgs::parse(command, args.into_iter());
    let parsed = match parsed {
        Ok(args) => args,
        Err(mut err) => {
            err.json = wants_json;
            err.emit();
            return Ok(ExitCode::from(err.exit_code));
        }
    };
    let json_mode = parsed.common.json;

    let positional = if positional_required {
        match parsed.positional.first().cloned() {
            Some(value) => Some(value),
            None => {
                let mut err = CliError::usage(command, format!("{command} requires <event-id>"));
                err.json = json_mode;
                err.emit();
                return Ok(ExitCode::from(err.exit_code));
            }
        }
    } else {
        None
    };

    let result = match sub.as_str() {
        "init" => cmd_init(&parsed),
        "build" => cmd_build(&parsed, false),
        "rebuild" => cmd_build(&parsed, true),
        "status" => cmd_status(&parsed),
        "subgraph" => cmd_subgraph(&parsed, positional.unwrap()),
        "causal" => cmd_causal(&parsed, positional.unwrap()),
        "deps" => cmd_deps(&parsed, positional.unwrap()),
        "unconsumed" => cmd_unconsumed(&parsed),
        _ => unreachable!("subcommand already validated"),
    };

    match result {
        Ok(output) => {
            output.emit();
            Ok(ExitCode::SUCCESS)
        }
        Err(mut err) => {
            err.json |= json_mode;
            err.emit();
            Ok(ExitCode::from(err.exit_code))
        }
    }
}

fn emit_usage_error(json: bool, message: &str) {
    let mut err = CliError::usage("graph", message.to_string());
    err.json = json;
    err.emit();
}

// ───────────────────────── command bodies ─────────────────────────

fn cmd_init(args: &WriteArgs) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    store
        .ensure_dir()
        .map_err(|err| CliError::runtime(args.command, format!("ensure channel: {err}")))?;
    graph::init(store.channel_dir(), &now_rfc3339())
        .map_err(|err| CliError::runtime(args.command, format!("init graph: {err}")))?;
    let path = graph::graph_path(store.channel_dir());
    Ok(output(
        args.command,
        store.channel_dir(),
        format!("initialized {}", path.display()),
        json!({
            "graph": {
                "path": path.display().to_string(),
                "schema_version": graph::SCHEMA_VERSION,
            }
        }),
    ))
}

fn cmd_build(args: &WriteArgs, full: bool) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let records = load_records(args.command, &store)?;
    let mut conn = open_graph(args.command, store.channel_dir())?;
    let meta = if full {
        graph::rebuild(&mut conn, &records, &now_rfc3339())
            .map_err(|err| CliError::runtime(args.command, format!("rebuild: {err}")))?
    } else {
        graph::catch_up(&mut conn, &records, &now_rfc3339())
            .map_err(|err| CliError::runtime(args.command, format!("catch_up: {err}")))?
    };
    let stats = graph::stats(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("stats: {err}")))?;
    let node_total = stats["nodes"]
        .as_object()
        .map(|o| o.values().filter_map(Value::as_i64).sum::<i64>())
        .unwrap_or(0);
    Ok(output(
        args.command,
        store.channel_dir(),
        format!(
            "status={} last_applied_seq={} nodes={node_total}",
            meta.status.as_str(),
            meta.last_applied_seq,
        ),
        json!({
            "graph": {
                "schema_version": meta.schema_version,
                "last_applied_seq": meta.last_applied_seq,
                "status": meta.status.as_str(),
                "last_built_at": meta.last_built_at,
                "stats": stats,
            }
        }),
    ))
}

fn cmd_status(args: &WriteArgs) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let conn = open_graph(args.command, store.channel_dir())?;
    let meta = graph::read_meta(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("meta: {err}")))?;
    let stats = graph::stats(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("stats: {err}")))?;
    Ok(output(
        args.command,
        store.channel_dir(),
        format!(
            "status={} last_applied_seq={}",
            meta.status.as_str(),
            meta.last_applied_seq
        ),
        json!({
            "graph": {
                "schema_version": meta.schema_version,
                "last_applied_seq": meta.last_applied_seq,
                "status": meta.status.as_str(),
                "last_built_at": meta.last_built_at,
                "stats": stats,
            }
        }),
    ))
}

fn cmd_subgraph(args: &WriteArgs, id: String) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let depth = parse_uint(args.one("--limit").as_deref(), 1).max(1).min(10);
    catch_up_before_read(args.command, &store)?;
    let conn = open_graph(args.command, store.channel_dir())?;
    let subgraph = graph::subgraph_around(&conn, &id, depth)
        .map_err(|err| CliError::runtime(args.command, format!("subgraph: {err}")))?;
    let meta = graph::read_meta(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("meta: {err}")))?;
    Ok(output(
        args.command,
        store.channel_dir(),
        format!(
            "subgraph root={id} depth={depth} nodes={} edges={}",
            subgraph["nodes"].as_array().map(Vec::len).unwrap_or(0),
            subgraph["edges"].as_array().map(Vec::len).unwrap_or(0)
        ),
        json!({
            "graph": {
                "status": meta.status.as_str(),
                "last_applied_seq": meta.last_applied_seq,
                "subgraph": subgraph,
            }
        }),
    ))
}

fn cmd_causal(args: &WriteArgs, id: String) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let depth = parse_uint(args.one("--limit").as_deref(), 5).max(1).min(20);
    catch_up_before_read(args.command, &store)?;
    let conn = open_graph(args.command, store.channel_dir())?;
    let chain = graph::causal_chain(&conn, &id, depth)
        .map_err(|err| CliError::runtime(args.command, format!("causal: {err}")))?;
    let meta = graph::read_meta(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("meta: {err}")))?;
    let root = graph::get_node(&conn, &id)
        .map_err(|err| CliError::runtime(args.command, format!("get root: {err}")))?;
    Ok(output(
        args.command,
        store.channel_dir(),
        format!("causal target={id} ancestors={}", chain.len()),
        json!({
            "graph": {
                "status": meta.status.as_str(),
                "target": root,
                "chain": chain,
            }
        }),
    ))
}

fn cmd_deps(args: &WriteArgs, id: String) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let depth = parse_uint(args.one("--limit").as_deref(), 10)
        .max(1)
        .min(20);
    catch_up_before_read(args.command, &store)?;
    let conn = open_graph(args.command, store.channel_dir())?;
    let deps = graph::transitive_deps(&conn, &id, depth)
        .map_err(|err| CliError::runtime(args.command, format!("deps: {err}")))?;
    let meta = graph::read_meta(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("meta: {err}")))?;
    Ok(output(
        args.command,
        store.channel_dir(),
        format!("deps task={id} count={}", deps.len()),
        json!({
            "graph": {
                "status": meta.status.as_str(),
                "task": id,
                "deps": deps,
            }
        }),
    ))
}

fn cmd_unconsumed(args: &WriteArgs) -> Result<WriteOutput, CliError> {
    let store = args.common.channel_store(args.command)?;
    let limit = parse_uint(args.one("--limit").as_deref(), 20)
        .max(1)
        .min(200);
    catch_up_before_read(args.command, &store)?;
    let conn = open_graph(args.command, store.channel_dir())?;
    let rows = graph::unconsumed_artifacts(&conn, limit)
        .map_err(|err| CliError::runtime(args.command, format!("unconsumed: {err}")))?;
    let meta = graph::read_meta(&conn)
        .map_err(|err| CliError::runtime(args.command, format!("meta: {err}")))?;
    Ok(output(
        args.command,
        store.channel_dir(),
        format!("unconsumed artifacts={}", rows.len()),
        json!({
            "graph": {
                "status": meta.status.as_str(),
                "artifacts": rows,
            }
        }),
    ))
}

// ───────────────────────── shared helpers ─────────────────────────

fn load_records(command: &'static str, store: &ChannelStore) -> Result<Vec<Value>, CliError> {
    if !store.changes_path().exists() {
        return Ok(Vec::new());
    }
    store
        .load_records_cached()
        .map_err(|err| CliError::runtime(command, format!("load records: {err}")))
}

fn open_graph(command: &'static str, channel: &Path) -> Result<graph::GraphConnection, CliError> {
    graph::init(channel, &now_rfc3339())
        .map_err(|err| CliError::runtime(command, format!("open graph: {err}")))
}

/// Lazy-on-read catch-up: ensure the projection has applied every event in
/// the log before a query reads it. Cheap at our scale; keeps queries
/// honest about freshness rather than returning stale results silently.
fn catch_up_before_read(command: &'static str, store: &ChannelStore) -> Result<(), CliError> {
    let records = load_records(command, store)?;
    let mut conn = open_graph(command, store.channel_dir())?;
    graph::catch_up(&mut conn, &records, &now_rfc3339())
        .map_err(|err| CliError::runtime(command, format!("catch_up: {err}")))?;
    Ok(())
}

fn parse_uint(value: Option<&str>, default: u32) -> u32 {
    value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(default)
}

fn output(command: &'static str, channel: &Path, text: String, data: Value) -> WriteOutput {
    WriteOutput {
        json: true,
        text,
        body: json!({
            "ok": true,
            "command": command,
            "schema": SCHEMA,
            "channel": channel.display().to_string(),
            "data": data,
        }),
    }
}
