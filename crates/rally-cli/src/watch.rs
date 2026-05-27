// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `rally watch` — block on changes.jsonl, emit new matching events as JSON lines.
//!
//! Uses the `notify` crate for kqueue/inotify/ReadDirectoryChangesW wakeups so
//! idle = zero CPU and latency is ~ms. A slow safety-net poll catches any
//! coalesced/missed events.
//!
//! Production fan-out (multi-consumer, sinks, configured filter rules) lives
//! in the sibling `agent-rally-watcher` repo. This CLI is the single-process
//! blocking-tail convenience.

use crate::args::WatchCommand;
use crate::output::CliError;
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rally_core::cursors;
use rally_core::store::ChannelStore;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Maximum interval between forced re-reads even when no FS event arrived.
/// Belt-and-suspenders for cases where notify coalesces or drops an event.
const SAFETY_POLL: Duration = Duration::from_secs(5);

pub(crate) fn execute_watch(command: WatchCommand) -> Result<(), CliError> {
    let store = command.common.channel_store("watch")?;
    let changes_path = store.changes_path();
    let deadline = command
        .max_seconds
        .map(|secs| Instant::now() + Duration::from_secs(secs));

    let start_offset = if command.from_start {
        0
    } else if command.since_cursor {
        cursor_to_offset(&store, &command, &changes_path)
    } else {
        file_size(&changes_path)
    };

    let mut offset = start_offset;
    let mut max_seq = current_cursor_value(&store, &command);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The channel directory may not exist yet if no events have been posted to
    // this channel. Delegate to the store so the directory contract stays
    // co-located with the store, not threaded across consumers.
    store.ensure_dir().map_err(|err| {
        CliError::runtime("watch", format!("failed to ensure channel dir: {err}"))
    })?;

    let (tx, rx) = mpsc::channel();
    // Watch the directory rather than the file so we wake up on the first
    // append (which may also be the call that creates the file).
    let watch_dir = store.channel_dir().to_path_buf();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        // Forward all FS events; the receive loop decides whether to re-read.
        let _ = tx.send(res);
    })
    .map_err(|err| CliError::runtime("watch", format!("failed to create watcher: {err}")))?;
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|err| CliError::runtime("watch", format!("failed to watch dir: {err}")))?;

    // Drain whatever is already past `offset` once (handles the case where the
    // file grew between cursor lookup and watcher registration).
    drain(&changes_path, &mut offset, &mut max_seq, &command, &mut out);
    if command.since_cursor && !command.peek {
        advance_cursor(&store, &command, max_seq);
    }

    loop {
        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = (deadline - now).min(SAFETY_POLL);
            let _ = recv_relevant_event(&rx, wait, &changes_path);
        } else {
            let _ = recv_relevant_event(&rx, SAFETY_POLL, &changes_path);
        }
        drain(&changes_path, &mut offset, &mut max_seq, &command, &mut out);
        if command.since_cursor && !command.peek {
            advance_cursor(&store, &command, max_seq);
        }
    }

    drop(watcher);
    Ok(())
}

/// Block up to `timeout` for an FS event affecting `changes_path` (or any
/// event in its parent dir). Returns whether a relevant event arrived; on
/// timeout, returns false and the caller falls back to a safety re-read.
fn recv_relevant_event(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    timeout: Duration,
    changes_path: &Path,
) -> bool {
    match rx.recv_timeout(timeout) {
        Ok(Ok(event)) => event_touches_log(&event, changes_path),
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

fn event_touches_log(event: &notify::Event, changes_path: &Path) -> bool {
    // Append-only growth shows up as ModifyKind::Data on most backends; on
    // some it's Modify::Any. Accept any modify/create touching the file.
    let kind_ok = matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
    );
    if !kind_ok {
        return false;
    }
    event
        .paths
        .iter()
        .any(|p| p == changes_path || p.ends_with("changes.jsonl"))
        || matches!(event.kind, EventKind::Modify(ModifyKind::Any))
}

fn drain(
    changes_path: &Path,
    offset: &mut u64,
    max_seq: &mut u64,
    command: &WatchCommand,
    out: &mut impl Write,
) {
    let (records, new_offset) = read_new(changes_path, *offset);
    *offset = new_offset;
    for record in records {
        let seq = record.get("local_seq").and_then(Value::as_u64).unwrap_or(0);
        if seq > *max_seq {
            *max_seq = seq;
        }
        if matches(&record, command) {
            writeln!(out, "{}", record).ok();
            out.flush().ok();
        }
    }
}

fn matches(record: &Value, command: &WatchCommand) -> bool {
    let event = record.get("event").unwrap_or(record);
    if let Some(kind) = &command.kind {
        if event.get("kind").and_then(Value::as_str) != Some(kind.as_str()) {
            return false;
        }
    }
    if let Some(tool) = &command.tool {
        if event.get("tool").and_then(Value::as_str) != Some(tool.as_str()) {
            return false;
        }
    }
    if let Some(thread) = &command.thread {
        if event.get("thread_id").and_then(Value::as_str) != Some(thread.as_str()) {
            return false;
        }
    }
    true
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn read_new(path: &Path, from: u64) -> (Vec<Value>, u64) {
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), from);
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size <= from {
        return (Vec::new(), from);
    }
    if file.seek(SeekFrom::Start(from)).is_err() {
        return (Vec::new(), from);
    }
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut offset = from;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if !buf.ends_with('\n') {
            break;
        }
        offset += n as u64;
        if let Ok(value) = serde_json::from_str::<Value>(buf.trim_end()) {
            records.push(value);
        }
    }
    (records, offset)
}

fn cursor_to_offset(store: &ChannelStore, command: &WatchCommand, changes_path: &Path) -> u64 {
    let (Some(tool), Some(session)) = (command.tool.as_deref(), command.session_id.as_deref())
    else {
        return file_size(changes_path);
    };
    let cursor = cursors::read_cursor(store.channel_dir(), tool, session);
    if cursor == 0 {
        return 0;
    }
    let Ok(file) = File::open(changes_path) else {
        return 0;
    };
    let reader = BufReader::new(file);
    let mut offset = 0u64;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line_len = line.len() as u64 + 1;
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            let seq = value.get("local_seq").and_then(Value::as_u64).unwrap_or(0);
            if seq > cursor {
                return offset;
            }
        }
        offset += line_len;
    }
    offset
}

fn current_cursor_value(store: &ChannelStore, command: &WatchCommand) -> u64 {
    let (Some(tool), Some(session)) = (command.tool.as_deref(), command.session_id.as_deref())
    else {
        return 0;
    };
    cursors::read_cursor(store.channel_dir(), tool, session)
}

fn advance_cursor(store: &ChannelStore, command: &WatchCommand, max_seq: u64) {
    let (Some(tool), Some(session)) = (command.tool.as_deref(), command.session_id.as_deref())
    else {
        return;
    };
    let _ = cursors::write_cursor(
        store.channel_dir(),
        tool,
        session,
        max_seq,
        &crate::runtime::now_rfc3339(),
    );
}
