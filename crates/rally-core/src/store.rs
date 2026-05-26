// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use chrono::{DateTime, SecondsFormat, Utc};
use fs2::FileExt;
use rally_protocol::{event_hash, event_value, store_entry_hash};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CHANGES_JSONL: &str = "changes.jsonl";
pub const RALLY_LOCK: &str = "rally.lock";
pub const ORIGIN_LOCAL: &str = "local";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelStore {
    channel_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoreTail {
    next_seq: u64,
    prev_entry_hash: Option<String>,
}

impl ChannelStore {
    pub fn new(channel_dir: impl Into<PathBuf>) -> Self {
        Self {
            channel_dir: channel_dir.into(),
        }
    }

    pub fn channel_dir(&self) -> &Path {
        &self.channel_dir
    }

    pub fn changes_path(&self) -> PathBuf {
        self.channel_dir.join(CHANGES_JSONL)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.channel_dir.join(RALLY_LOCK)
    }

    pub fn load_records(&self) -> Result<Vec<Value>, CoreError> {
        load_records(self.changes_path())
    }

    pub fn append_event(&self, event: Value) -> Result<Value, CoreError> {
        self.append_event_with_origin(event, ORIGIN_LOCAL)
    }

    pub fn append_event_with_origin(&self, event: Value, origin: &str) -> Result<Value, CoreError> {
        fs::create_dir_all(&self.channel_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())?;
        lock.lock_exclusive()?;

        let result = self.append_event_locked(event, origin);
        FileExt::unlock(&lock)?;
        result
    }

    fn append_event_locked(&self, event: Value, origin: &str) -> Result<Value, CoreError> {
        let tail = inspect_store_tail(self.changes_path())?;
        let entry = store_entry_value(event, tail.next_seq, tail.prev_entry_hash, origin)?;
        let line = serde_json::to_string(&entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.changes_path())?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(entry)
    }
}

pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<Value>, CoreError> {
    read_strict_store(path.as_ref())
        .map(|records| records.into_iter().map(|item| item.value).collect())
}

pub fn store_entry_value(
    event: Value,
    local_seq: u64,
    prev_entry_hash: Option<String>,
    origin: &str,
) -> Result<Value, CoreError> {
    event_value(&event)?;
    Ok(serde_json::json!({
        "local_seq": local_seq,
        "received_at": received_at_now(),
        "origin": origin,
        "event_hash": event_hash(&event)?,
        "prev_entry_hash": prev_entry_hash,
        "event": event
    }))
}

fn received_at_now() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Debug)]
struct StrictStoreRecord {
    value: Value,
    line_hash: String,
}

fn inspect_store_tail(path: impl AsRef<Path>) -> Result<StoreTail, CoreError> {
    let records = read_strict_store(path.as_ref())?;
    Ok(StoreTail {
        next_seq: records.len() as u64 + 1,
        prev_entry_hash: records.last().map(|record| record.line_hash.clone()),
    })
}

fn read_strict_store(path: &Path) -> Result<Vec<StrictStoreRecord>, CoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut records = Vec::new();
    let mut prev_hash = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = index + 1;
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(trimmed).map_err(|source| CoreError::InvalidStoreLine {
                path: path.to_path_buf(),
                line: line_number,
                source,
            })?;
        validate_store_entry(
            path,
            line_number,
            records.len() as u64 + 1,
            &prev_hash,
            &value,
        )?;
        let line_hash = store_entry_hash(trimmed);
        prev_hash = Some(line_hash.clone());
        records.push(StrictStoreRecord { value, line_hash });
    }

    Ok(records)
}

fn validate_store_entry(
    path: &Path,
    line: usize,
    expected_seq: u64,
    expected_prev_hash: &Option<String>,
    value: &Value,
) -> Result<(), CoreError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_entry(path, line, "store entry must be a JSON object"))?;
    let local_seq = object
        .get("local_seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_entry(path, line, "store entry missing numeric local_seq"))?;
    if local_seq != expected_seq {
        return Err(invalid_entry(
            path,
            line,
            format!("expected local_seq {expected_seq}, found {local_seq}"),
        ));
    }

    let expected_event_hash = event_hash(value)?;
    let actual_event_hash = object
        .get("event_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_entry(path, line, "store entry missing string event_hash"))?;
    if actual_event_hash != expected_event_hash {
        return Err(invalid_entry(
            path,
            line,
            format!(
                "event_hash mismatch: expected {expected_event_hash}, found {actual_event_hash}"
            ),
        ));
    }

    let actual_prev_hash = object.get("prev_entry_hash").and_then(Value::as_str);
    if actual_prev_hash != expected_prev_hash.as_deref() {
        return Err(invalid_entry(
            path,
            line,
            format!(
                "prev_entry_hash mismatch: expected {}, found {}",
                expected_prev_hash.as_deref().unwrap_or("null"),
                actual_prev_hash.unwrap_or("null")
            ),
        ));
    }

    event_value(value)?;
    Ok(())
}

fn invalid_entry(path: &Path, line: usize, message: impl Into<String>) -> CoreError {
    CoreError::InvalidStoreEntry {
        path: path.to_path_buf(),
        line,
        message: message.into(),
    }
}
