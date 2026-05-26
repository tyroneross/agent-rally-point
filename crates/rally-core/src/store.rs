// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::CoreError;
use rally_protocol::read_jsonl;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const CHANGES_JSONL: &str = "changes.jsonl";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelStore {
    channel_dir: PathBuf,
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

    pub fn load_records(&self) -> Result<Vec<Value>, CoreError> {
        load_records(self.changes_path())
    }
}

pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<Value>, CoreError> {
    Ok(read_jsonl(path)?)
}
