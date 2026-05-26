// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use chrono::{DateTime, SecondsFormat, Utc};
use fs2::FileExt;
use rally_protocol::{event_hash, event_value, store_entry_hash};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CHANGES_JSONL: &str = "changes.jsonl";
pub const RALLY_LOCK: &str = "rally.lock";
pub const RALLY_TAIL: &str = "rally.tail.json";
pub const ORIGIN_LOCAL: &str = "local";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelStore {
    channel_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoreTail {
    next_seq: u64,
    prev_entry_hash: Option<String>,
    log_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TailCache {
    next_seq: u64,
    prev_entry_hash: Option<String>,
    log_bytes: u64,
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

    pub fn tail_path(&self) -> PathBuf {
        self.channel_dir.join(RALLY_TAIL)
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
        let tail = inspect_store_tail(self.changes_path(), self.tail_path())?;
        let entry = store_entry_value(event, tail.next_seq, tail.prev_entry_hash, origin)?;
        let line = serde_json::to_string(&entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.changes_path())?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        write_tail_cache(
            self.tail_path(),
            TailCache {
                next_seq: tail.next_seq + 1,
                prev_entry_hash: Some(store_entry_hash(&line)),
                log_bytes: tail.log_bytes + line.len() as u64 + 1,
            },
        )?;
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

fn inspect_store_tail(
    changes_path: impl AsRef<Path>,
    tail_path: impl AsRef<Path>,
) -> Result<StoreTail, CoreError> {
    let changes_path = changes_path.as_ref();
    let tail_path = tail_path.as_ref();
    if let Some(tail) = read_tail_cache(changes_path, tail_path)? {
        return Ok(tail);
    }
    let records = read_strict_store(changes_path)?;
    let log_bytes = fs::metadata(changes_path)
        .map(|metadata| metadata.len())
        .or_else(|err| {
            (err.kind() == std::io::ErrorKind::NotFound)
                .then_some(0)
                .ok_or(err)
        })?;
    Ok(StoreTail {
        next_seq: records.len() as u64 + 1,
        prev_entry_hash: records.last().map(|record| record.line_hash.clone()),
        log_bytes,
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
    let mut reader = BufReader::new(file);
    let mut line_bytes = Vec::new();
    let mut line_number = 0;
    loop {
        line_bytes.clear();
        let read = reader.read_until(b'\n', &mut line_bytes)?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if line_bytes.last() != Some(&b'\n') {
            return Err(invalid_entry(
                path,
                line_number,
                "store entry line is missing trailing newline",
            ));
        }
        line_bytes.pop();
        if line_bytes.is_empty() {
            return Err(invalid_entry(path, line_number, "blank store entry line"));
        }
        let line = std::str::from_utf8(&line_bytes).map_err(|err| {
            invalid_entry(
                path,
                line_number,
                format!("store entry is not UTF-8: {err}"),
            )
        })?;
        if line.trim() != line {
            return Err(invalid_entry(
                path,
                line_number,
                "store entry line has leading or trailing whitespace",
            ));
        }
        let value: Value =
            serde_json::from_str(line).map_err(|source| CoreError::InvalidStoreLine {
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
        let line_hash = store_entry_hash(line);
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

fn read_tail_cache(changes_path: &Path, tail_path: &Path) -> Result<Option<StoreTail>, CoreError> {
    let mut file = match File::open(tail_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let cache: TailCache = serde_json::from_str(&text)?;
    let log_bytes = fs::metadata(changes_path)
        .map(|metadata| metadata.len())
        .or_else(|err| {
            (err.kind() == std::io::ErrorKind::NotFound)
                .then_some(0)
                .ok_or(err)
        })?;
    if cache.log_bytes != log_bytes {
        return Ok(None);
    }
    Ok(Some(StoreTail {
        next_seq: cache.next_seq,
        prev_entry_hash: cache.prev_entry_hash,
        log_bytes: cache.log_bytes,
    }))
}

fn write_tail_cache(path: impl AsRef<Path>, cache: TailCache) -> Result<(), CoreError> {
    let path = path.as_ref();
    let tmp_path = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)?;
    serde_json::to_writer(&mut file, &cache)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    fs::rename(tmp_path, path)?;
    Ok(())
}
