// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! File-backed [`Inbox`] implementation.
//!
//! Files live under `<root>/inbox/<agent>.jsonl` (directives) and
//! `<root>/receipts/<agent>.jsonl` (receipts). The split is deliberate:
//! a reader scanning only Directives never has to filter Receipts out of
//! the same stream, and the daemon can pull-by-seq cheaply without
//! deserialising every Receipt the agent has ever posted.
//!
//! ## Atomicity
//! Each line is serialised to a `String` in memory, the trailing `\n` is
//! appended, then `write_all` is called on a single `OpenOptions::append`
//! handle. POSIX `O_APPEND` guarantees the entire `write_all` is atomic
//! against concurrent writers up to PIPE_BUF (usually 4096 bytes). The
//! canonical Directive/Receipt JSON is well under 4096 bytes; documents
//! exceeding that are silently allowed but lose multi-writer atomicity
//! (they STILL parse correctly under single-writer access).
//!
//! ## Read tolerance
//! [`FileInbox::read_since`] and [`FileInbox::read_receipts_since`]
//! split on `\n`. The last segment is parsed; if it fails to parse AND
//! does not end with `\n`, it is silently treated as a partial-write and
//! skipped. A line that DOES end with `\n` but fails to parse is a
//! genuine corruption and surfaced as `io::ErrorKind::InvalidData`.
//!
//! ## Monotonic seq
//! `append_directive(seq=0)` (or any value <= the current max) is
//! re-assigned to `max_seq + 1`. Callers that want strict producer-side
//! seq (e.g. the daemon replaying a snapshot) can pass `seq > 0` and we
//! validate it is strictly greater than the current max.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::{Directive, Inbox, Receipt};

/// File-backed Inbox rooted at a directory.
///
/// Typical roots:
/// - `.rally/` (the repo-local rally ledger root)
/// - a scratch tempdir during tests (via `ChannelSandbox`/`TermdSandbox`)
#[derive(Debug, Clone)]
pub struct FileInbox {
    /// The ledger root directory. `inbox/` and `receipts/` are
    /// auto-created relative to this.
    root: PathBuf,
}

impl FileInbox {
    /// Open a [`FileInbox`] at `root`. Creates `root/inbox/` and
    /// `root/receipts/` if missing. Does NOT create the root itself
    /// (callers are responsible for ledger-root creation; this prevents
    /// a typo'd path silently producing a fresh empty ledger).
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("ledger root does not exist: {}", root.display()),
            ));
        }
        fs::create_dir_all(root.join("inbox"))?;
        fs::create_dir_all(root.join("receipts"))?;
        Ok(Self { root })
    }

    /// Root directory of this ledger.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the agent's directives file. Public so consumers (the
    /// daemon) can attach a kernel file-event watcher to the exact path
    /// the writer appends to. Plan F functional core: `rally-termd`
    /// watches `directives_path(agent)` and wakes on inotify/kqueue
    /// EVFILT_VNODE so the wake is event-driven (sub-poll-floor latency)
    /// instead of timer-driven.
    pub fn directives_path(&self, agent: &str) -> PathBuf {
        self.root
            .join("inbox")
            .join(format!("{}.jsonl", sanitize(agent)))
    }

    /// Path to the agent's receipts file. Public for the same reason
    /// as [`directives_path`].
    pub fn receipts_path(&self, agent: &str) -> PathBuf {
        self.root
            .join("receipts")
            .join(format!("{}.jsonl", sanitize(agent)))
    }

    /// The highest `seq` currently in the agent's inbox (0 if missing).
    ///
    /// Currently scans the full inbox file on every `append_directive` call.
    /// This is the slow path; on a real ledger the rate-limit is the
    /// file-event wake at the consumer (not writer throughput). Known
    /// follow-up: cache last seq in memory or maintain a `<agent>.seq`
    /// sidecar when measured write throughput becomes a bottleneck.
    fn current_max_seq(&self, agent: &str) -> io::Result<u64> {
        let path = self.directives_path(agent);
        if !path.exists() {
            return Ok(0);
        }
        let f = File::open(&path)?;
        let reader = BufReader::new(f);
        let mut max = 0u64;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // Tolerant parse: a directive whose seq we can't read is treated
            // as max=0 for this line (it WILL be re-parsed below; if it
            // really is garbage, read_since will surface it).
            if let Ok(d) = serde_json::from_str::<Directive>(&line)
                && d.seq > max
            {
                max = d.seq;
            }
        }
        Ok(max)
    }
}

impl Inbox for FileInbox {
    fn append_directive(&self, directive: &Directive) -> io::Result<u64> {
        // Assign or validate seq.
        let current_max = self.current_max_seq(&directive.to)?;
        let assigned_seq = if directive.seq == 0 || directive.seq <= current_max {
            current_max + 1
        } else {
            directive.seq
        };
        let to_write = if assigned_seq == directive.seq {
            directive.clone()
        } else {
            let mut copy = directive.clone();
            copy.seq = assigned_seq;
            copy
        };

        let path = self.directives_path(&directive.to);
        let mut line = serde_json::to_string(&to_write)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())?;
        // Explicit flush. Reads via `read_since` open a fresh handle so
        // this is belt-and-braces; the POSIX append is already durable to
        // the page cache.
        f.flush()?;
        Ok(assigned_seq)
    }

    fn read_since(&self, agent: &str, after_seq: u64) -> io::Result<Vec<Directive>> {
        let path = self.directives_path(agent);
        read_jsonl_since(&path, |d: &Directive| d.seq, after_seq)
    }

    fn append_receipt(&self, receipt: &Receipt) -> io::Result<()> {
        let path = self.receipts_path(&receipt.to);
        let mut line = serde_json::to_string(receipt)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        Ok(())
    }

    fn read_receipts_since(&self, agent: &str, after_ref_seq: u64) -> io::Result<Vec<Receipt>> {
        let path = self.receipts_path(agent);
        read_jsonl_since(&path, |r: &Receipt| r.ref_seq, after_ref_seq)
    }
}

/// Generic "read NDJSON where `field(record) > after`" with partial-line
/// tolerance.
fn read_jsonl_since<T, F>(path: &Path, field: F, after: u64) -> io::Result<Vec<T>>
where
    T: DeserializeOwned,
    F: Fn(&T) -> u64,
{
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');

    // Convert to UTF-8 (the .jsonl substrate is UTF-8 by contract).
    let s =
        std::str::from_utf8(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Split. If the file does NOT end with \n, the LAST segment is a
    // potential partial-write; tolerate a parse failure on JUST that
    // segment.
    let lines: Vec<&str> = s.split('\n').collect();
    let last_index = lines.len().saturating_sub(1);

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let is_tail_partial = i == last_index && !ends_with_newline;
        match serde_json::from_str::<T>(line) {
            Ok(record) => {
                if field(&record) > after {
                    out.push(record);
                }
            }
            Err(e) => {
                if is_tail_partial {
                    // Quietly skip — half-written final line is expected
                    // under concurrent append.
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "corrupt jsonl line at {} (offset ~line {}): {}",
                        path.display(),
                        i + 1,
                        e
                    ),
                ));
            }
        }
    }
    Ok(out)
}

/// Sanitise an agent id for use as a filename. The id itself MAY contain
/// `:` (e.g. `claude_code:lead-01`) but POSIX filenames are happy with
/// `:`; we replace `/` with `_` to prevent path-traversal.
fn sanitize(agent: &str) -> String {
    agent.replace('/', "_")
}
