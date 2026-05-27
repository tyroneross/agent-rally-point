// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use crate::event::EventBuilder;
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
pub const RALLY_CHECKPOINT: &str = "rally.checkpoint.json";
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointStatus {
    pub exists: bool,
    pub valid: bool,
    pub records: usize,
    pub log_bytes: u64,
    pub last_entry_hash: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CheckpointCache {
    schema: String,
    records: Vec<Value>,
    record_count: usize,
    log_bytes: u64,
    last_entry_hash: Option<String>,
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

    /// Ensure the channel directory exists on disk. Cheap, idempotent. Useful
    /// for callers that observe the directory (e.g. `rally watch`) before the
    /// first append has lazily created it.
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.channel_dir)
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

    pub fn checkpoint_path(&self) -> PathBuf {
        self.channel_dir.join(RALLY_CHECKPOINT)
    }

    pub fn load_records(&self) -> Result<Vec<Value>, CoreError> {
        load_records(self.changes_path())
    }

    pub fn load_records_cached(&self) -> Result<Vec<Value>, CoreError> {
        if let Some(records) = read_valid_checkpoint(
            self.changes_path(),
            self.tail_path(),
            self.checkpoint_path(),
        )? {
            return Ok(records);
        }
        let records = read_strict_store(&self.changes_path())?;
        let log_bytes = log_bytes(&self.changes_path())?;
        let last_entry_hash = records.last().map(|record| record.line_hash.clone());
        let records = records
            .into_iter()
            .map(|record| record.value)
            .collect::<Vec<_>>();
        write_tail_cache(
            self.tail_path(),
            TailCache {
                next_seq: records.len() as u64 + 1,
                prev_entry_hash: last_entry_hash.clone(),
                log_bytes,
            },
        )?;
        write_checkpoint_cache(
            self.checkpoint_path(),
            CheckpointCache {
                schema: "agent-rally.checkpoint.v1".to_string(),
                record_count: records.len(),
                records: records.clone(),
                log_bytes,
                last_entry_hash,
            },
        )?;
        Ok(records)
    }

    pub fn rebuild_checkpoint(&self) -> Result<CheckpointStatus, CoreError> {
        fs::create_dir_all(&self.channel_dir)?;
        let records = read_strict_store(&self.changes_path())?;
        let log_bytes = log_bytes(&self.changes_path())?;
        let last_entry_hash = records.last().map(|record| record.line_hash.clone());
        let records = records
            .into_iter()
            .map(|record| record.value)
            .collect::<Vec<_>>();
        write_tail_cache(
            self.tail_path(),
            TailCache {
                next_seq: records.len() as u64 + 1,
                prev_entry_hash: last_entry_hash.clone(),
                log_bytes,
            },
        )?;
        write_checkpoint_cache(
            self.checkpoint_path(),
            CheckpointCache {
                schema: "agent-rally.checkpoint.v1".to_string(),
                record_count: records.len(),
                records,
                log_bytes,
                last_entry_hash: last_entry_hash.clone(),
            },
        )?;
        Ok(CheckpointStatus {
            exists: true,
            valid: true,
            records: checkpoint_record_count(&self.checkpoint_path())?,
            log_bytes,
            last_entry_hash,
            reason: None,
        })
    }

    pub fn checkpoint_status(&self) -> Result<CheckpointStatus, CoreError> {
        checkpoint_status(
            self.changes_path(),
            self.tail_path(),
            self.checkpoint_path(),
        )
    }

    pub fn append_event(&self, event: Value) -> Result<Value, CoreError> {
        self.append_event_with_origin(event, ORIGIN_LOCAL)
    }

    pub fn append_typed(&self, event: EventBuilder) -> Result<Value, CoreError> {
        self.append_typed_with_origin(event, ORIGIN_LOCAL)
    }

    pub fn append_typed_with_origin(
        &self,
        event: EventBuilder,
        origin: &str,
    ) -> Result<Value, CoreError> {
        self.append_event_with_origin(event.build()?, origin)
    }

    pub fn append_event_with_origin(&self, event: Value, origin: &str) -> Result<Value, CoreError> {
        self.append_event_with_origin_and_trust(event, origin, None)
    }

    pub fn append_event_with_origin_and_trust(
        &self,
        event: Value,
        origin: &str,
        trust_status: Option<&str>,
    ) -> Result<Value, CoreError> {
        fs::create_dir_all(&self.channel_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())?;
        lock.lock_exclusive()?;

        let result = self.append_event_locked(event, origin, trust_status);
        FileExt::unlock(&lock)?;
        result
    }

    fn append_event_locked(
        &self,
        event: Value,
        origin: &str,
        trust_status: Option<&str>,
    ) -> Result<Value, CoreError> {
        let tail = inspect_store_tail(self.changes_path(), self.tail_path())?;
        let entry = store_entry_value_with_trust(
            event,
            tail.next_seq,
            tail.prev_entry_hash,
            origin,
            trust_status,
        )?;
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
    store_entry_value_with_trust(event, local_seq, prev_entry_hash, origin, None)
}

pub fn store_entry_value_with_trust(
    event: Value,
    local_seq: u64,
    prev_entry_hash: Option<String>,
    origin: &str,
    trust_status: Option<&str>,
) -> Result<Value, CoreError> {
    event_value(&event)?;
    let mut entry = serde_json::json!({
        "local_seq": local_seq,
        "received_at": received_at_now(),
        "origin": origin,
        "event_hash": event_hash(&event)?,
        "prev_entry_hash": prev_entry_hash,
        "event": event
    });
    if let Some(trust_status) = trust_status {
        entry["trust_status"] = Value::String(trust_status.to_string());
    }
    Ok(entry)
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

fn read_valid_checkpoint(
    changes_path: impl AsRef<Path>,
    tail_path: impl AsRef<Path>,
    checkpoint_path: impl AsRef<Path>,
) -> Result<Option<Vec<Value>>, CoreError> {
    let changes_path = changes_path.as_ref();
    let tail_path = tail_path.as_ref();
    let checkpoint_path = checkpoint_path.as_ref();
    let Some(tail) = read_tail_cache(changes_path, tail_path)? else {
        return Ok(None);
    };
    let Some(checkpoint) = read_checkpoint_cache(checkpoint_path)? else {
        return Ok(None);
    };
    if checkpoint.schema != "agent-rally.checkpoint.v1"
        || checkpoint.log_bytes != tail.log_bytes
        || checkpoint.last_entry_hash != tail.prev_entry_hash
        || checkpoint.record_count != checkpoint.records.len()
    {
        return Ok(None);
    }
    Ok(Some(checkpoint.records))
}

fn checkpoint_status(
    changes_path: impl AsRef<Path>,
    tail_path: impl AsRef<Path>,
    checkpoint_path: impl AsRef<Path>,
) -> Result<CheckpointStatus, CoreError> {
    let changes_path = changes_path.as_ref();
    let tail_path = tail_path.as_ref();
    let checkpoint_path = checkpoint_path.as_ref();
    let Some(checkpoint) = read_checkpoint_cache(checkpoint_path)? else {
        return Ok(CheckpointStatus {
            exists: false,
            valid: false,
            records: 0,
            log_bytes: log_bytes(changes_path)?,
            last_entry_hash: None,
            reason: Some("checkpoint is missing".to_string()),
        });
    };
    let Some(tail) = read_tail_cache(changes_path, tail_path)? else {
        return Ok(CheckpointStatus {
            exists: true,
            valid: false,
            records: checkpoint.record_count,
            log_bytes: checkpoint.log_bytes,
            last_entry_hash: checkpoint.last_entry_hash,
            reason: Some("tail cache is missing or stale".to_string()),
        });
    };
    let valid = checkpoint.schema == "agent-rally.checkpoint.v1"
        && checkpoint.log_bytes == tail.log_bytes
        && checkpoint.last_entry_hash == tail.prev_entry_hash
        && checkpoint.record_count == checkpoint.records.len();
    Ok(CheckpointStatus {
        exists: true,
        valid,
        records: checkpoint.record_count,
        log_bytes: checkpoint.log_bytes,
        last_entry_hash: checkpoint.last_entry_hash,
        reason: (!valid).then(|| "checkpoint does not match the current log tail".to_string()),
    })
}

fn read_checkpoint_cache(path: &Path) -> Result<Option<CheckpointCache>, CoreError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn write_checkpoint_cache(path: impl AsRef<Path>, cache: CheckpointCache) -> Result<(), CoreError> {
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

fn checkpoint_record_count(path: &Path) -> Result<usize, CoreError> {
    Ok(read_checkpoint_cache(path)?.map_or(0, |checkpoint| checkpoint.record_count))
}

fn log_bytes(path: &Path) -> Result<u64, CoreError> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .or_else(|err| {
            (err.kind() == std::io::ErrorKind::NotFound)
                .then_some(0)
                .ok_or(err)
        })
        .map_err(CoreError::from)
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
