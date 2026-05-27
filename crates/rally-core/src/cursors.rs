// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Per-(tool, session) read cursors over the append-only log.
//!
//! Each cursor stores the largest `local_seq` value already seen by one agent
//! session. Multiple sessions of the same tool (e.g. several terminal tabs,
//! several CI runs) each get their own cursor, so "what's new since I last
//! checked" is per-session, not per-tool.
//!
//! Cursors are caches. They live under `<channel>/rally/cursors/` and are
//! always rebuildable: deleting the file just makes the next read return
//! every event again.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CURSORS_SUBDIR: &str = "rally/cursors";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub tool: String,
    pub session_id: String,
    pub last_seq: u64,
    pub updated_at: String,
}

pub fn cursor_dir(channel_dir: &Path) -> PathBuf {
    channel_dir.join(CURSORS_SUBDIR)
}

pub fn cursor_path(channel_dir: &Path, tool: &str, session_id: &str) -> PathBuf {
    cursor_dir(channel_dir).join(format!("{}.json", cursor_filename(tool, session_id)))
}

/// Read the last-seen `local_seq` for `(tool, session_id)`. Returns `0` if no
/// cursor file exists yet (so the caller sees every event on first read).
pub fn read_cursor(channel_dir: &Path, tool: &str, session_id: &str) -> u64 {
    let path = cursor_path(channel_dir, tool, session_id);
    let Ok(bytes) = fs::read(&path) else {
        return 0;
    };
    let Ok(cursor) = serde_json::from_slice::<Cursor>(&bytes) else {
        return 0;
    };
    cursor.last_seq
}

/// Atomically write the cursor file using tmp+rename so a partially written
/// cursor is never observable on disk.
pub fn write_cursor(
    channel_dir: &Path,
    tool: &str,
    session_id: &str,
    last_seq: u64,
    now_rfc3339: &str,
) -> io::Result<()> {
    let dir = cursor_dir(channel_dir);
    fs::create_dir_all(&dir)?;
    let path = cursor_path(channel_dir, tool, session_id);
    let cursor = Cursor {
        tool: tool.to_string(),
        session_id: session_id.to_string(),
        last_seq,
        updated_at: now_rfc3339.to_string(),
    };
    let body = serde_json::to_vec(&cursor)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &body)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Sanitize `tool` + `session_id` into a filesystem-safe filename. Any
/// character outside `[A-Za-z0-9._-]` is replaced with `-`.
fn cursor_filename(tool: &str, session_id: &str) -> String {
    fn safe(value: &str) -> String {
        let out: String = value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if out.is_empty() {
            "unknown".to_string()
        } else {
            out
        }
    }
    format!("{}--{}", safe(tool), safe(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_channel(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "rally-cursors-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_cursor_returns_zero() {
        // intent: a fresh session must see every event in the log, not silently start halfway through.
        let dir = temp_channel("missing");
        assert_eq!(read_cursor(&dir, "claude", "s1"), 0);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_then_read_round_trip() {
        // intent: cursor advance is the only mechanism that makes "what's new since I last checked" work.
        let dir = temp_channel("roundtrip");
        write_cursor(&dir, "claude", "s1", 42, "2026-05-27T00:00:00.000Z").unwrap();
        assert_eq!(read_cursor(&dir, "claude", "s1"), 42);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cursors_are_scoped_per_session() {
        // intent: two tabs of the same tool must not stomp on each other's read position.
        let dir = temp_channel("per-session");
        write_cursor(&dir, "claude", "tab-1", 10, "t1").unwrap();
        write_cursor(&dir, "claude", "tab-2", 25, "t2").unwrap();
        assert_eq!(read_cursor(&dir, "claude", "tab-1"), 10);
        assert_eq!(read_cursor(&dir, "claude", "tab-2"), 25);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cursor_filename_sanitizes_unsafe_chars() {
        // intent: tool/session ids containing slashes or spaces can't escape the cursors directory.
        let name = cursor_filename("claude/code", "session id with spaces");
        assert!(!name.contains('/'));
        assert!(!name.contains(' '));
    }

    #[test]
    fn corrupt_cursor_file_returns_zero() {
        // intent: a corrupt cache must not prevent reads — cursors are disposable.
        let dir = temp_channel("corrupt");
        let path = cursor_path(&dir, "claude", "s1");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json").unwrap();
        assert_eq!(read_cursor(&dir, "claude", "s1"), 0);
        fs::remove_dir_all(dir).ok();
    }
}
