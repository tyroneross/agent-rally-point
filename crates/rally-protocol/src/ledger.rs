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
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::{Directive, Inbox, Receipt};

// ---------------------------------------------------------------------------
// Security constants (SEC-002/003/007/008 — control-plane review 2026-06-02)
// ---------------------------------------------------------------------------

/// Private directory mode for `inbox/` + `receipts/` (SEC-007). The ledger is
/// a same-UID-local trust surface: directives are executed by the daemon as
/// keystrokes, so the inbox MUST NOT be world-/group-readable or -writable.
/// Mirrors `ptyd/src/persist.rs::PRIVATE_DIR_MODE`.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Private file mode for the per-agent `.jsonl` logs (SEC-007).
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Hard ceiling on a single `Directive.text` payload (SEC-008). 64 KiB. A
/// directive becomes keystrokes in another agent's pane; an unbounded payload
/// is an OOM / disk-flood / PTY-flood vector. Enforced on the WRITE side here
/// (the daemon enforces the same ceiling defensively before executing).
pub const MAX_DIRECTIVE_TEXT_BYTES: usize = 64 * 1024;

/// Hard ceiling on the byte length of a single NDJSON line the reader will
/// buffer (SEC-008). A line carries one framed Directive/Receipt; we bound it
/// to the text ceiling plus generous JSON-envelope headroom so a single
/// adversarially-long line cannot OOM the daemon's line-incremental reader.
const MAX_LINE_BYTES: usize = MAX_DIRECTIVE_TEXT_BYTES + 8 * 1024;

/// Maximum length of a sanitized agent-id filename stem (SEC-003).
const MAX_AGENT_ID_LEN: usize = 128;

/// Validate an agent-id for use as a ledger filename stem (SEC-003).
///
/// REJECTS (returns `InvalidInput`): empty, longer than [`MAX_AGENT_ID_LEN`],
/// a leading `.` (hidden / `.`/`..` relative refs), any `..` substring, any
/// path separator (`/` or `\\`), and any character outside the allowlist
/// `[A-Za-z0-9:_-]`. The canonical rally id vocabulary (`claude_code:lead-01`,
/// `rally-cli`, `rally-termd:heartbeat`) is entirely within the allowlist, so
/// legitimate ids never trip this; a traversal payload like `../../etc/passwd`
/// is rejected at the write boundary instead of being silently mangled.
pub fn validate_agent_id(agent: &str) -> io::Result<()> {
    if agent.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent id is empty",
        ));
    }
    if agent.len() > MAX_AGENT_ID_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("agent id exceeds {MAX_AGENT_ID_LEN} bytes"),
        ));
    }
    if agent.starts_with('.') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent id must not start with '.'",
        ));
    }
    if agent.contains("..") || agent.contains('/') || agent.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent id must not contain path separators or '..'",
        ));
    }
    if let Some(bad) = agent
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-')))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("agent id contains disallowed character {bad:?} (allowlist [A-Za-z0-9:_-])"),
        ));
    }
    Ok(())
}

/// Set `path` to `mode` (unix only; no-op elsewhere).
fn set_private_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// `create_dir_all` then clamp to [`PRIVATE_DIR_MODE`] (SEC-007). Repairs an
/// existing world-readable dir created before the fix.
fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        set_private_mode(path, PRIVATE_DIR_MODE)?;
    }
    Ok(())
}

/// Append-open a ledger file with [`PRIVATE_FILE_MODE`] on create (SEC-007),
/// then repair perms in case it pre-existed with a looser mode. Preserves the
/// `O_APPEND` atomicity contract (mode only affects the create case).
fn private_append_open(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(PRIVATE_FILE_MODE);
    }
    let f = opts.open(path)?;
    #[cfg(unix)]
    {
        set_private_mode(path, PRIVATE_FILE_MODE)?;
    }
    Ok(f)
}

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
        // SEC-007: inbox/receipts are a same-UID-local keystroke-execution
        // surface — create them 0700 (and repair if they pre-existed looser).
        ensure_private_dir(&root.join("inbox"))?;
        ensure_private_dir(&root.join("receipts"))?;
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
        // SEC-003: reject a traversal/garbage target at the write boundary
        // instead of silently mangling it into a filename.
        validate_agent_id(&directive.to)?;
        // SEC-008: bound the payload. A directive becomes keystrokes in the
        // target's pane; an unbounded `text` is an OOM/disk/PTY-flood vector.
        if let Some(text) = directive.text.as_deref()
            && text.len() > MAX_DIRECTIVE_TEXT_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "directive text {} bytes exceeds MAX_DIRECTIVE_TEXT_BYTES ({MAX_DIRECTIVE_TEXT_BYTES})",
                    text.len()
                ),
            ));
        }
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
        debug_assert_eq!(
            path.parent(),
            Some(self.root.join("inbox").as_path()),
            "SEC-003: directive path escaped inbox/"
        );
        let mut line = serde_json::to_string(&to_write)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut f = private_append_open(&path)?;
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
        // SEC-003: same write-boundary validation as directives.
        validate_agent_id(&receipt.to)?;
        let path = self.receipts_path(&receipt.to);
        debug_assert_eq!(
            path.parent(),
            Some(self.root.join("receipts").as_path()),
            "SEC-003: receipt path escaped receipts/"
        );
        let mut line = serde_json::to_string(receipt)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut f = private_append_open(&path)?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        Ok(())
    }

    fn read_receipts_since(&self, agent: &str, after_ref_seq: u64) -> io::Result<Vec<Receipt>> {
        let path = self.receipts_path(agent);
        read_jsonl_since(&path, |r: &Receipt| r.ref_seq, after_ref_seq)
    }
}

/// Outcome of one capped line read (SEC-008).
struct LineRead {
    /// Bytes stored in `buf` (<= cap), EXCLUDING the trailing `\n`.
    stored: usize,
    /// The physical line ended with `\n` (vs ending at EOF with no newline).
    had_newline: bool,
    /// The physical line was longer than the cap; the tail was discarded.
    truncated: bool,
    /// EOF reached with NOTHING read this call.
    eof: bool,
}

/// Read one NDJSON line into `buf`, storing at most `cap` bytes (SEC-008).
///
/// The whole-file `fs::read` it replaces loaded the ENTIRE inbox into memory —
/// an unbounded directive payload (or a maliciously huge single line written
/// directly to the file) was a daemon-OOM vector. This consumes the physical
/// line incrementally and stops STORING at `cap` while still draining the rest
/// of the physical line so the next call starts cleanly. The newline byte is
/// consumed but never stored.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<LineRead> {
    buf.clear();
    let mut had_newline = false;
    let mut truncated = false;
    let mut any = false;
    loop {
        let chunk = loop {
            match reader.fill_buf() {
                Ok(b) => break b,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        if chunk.is_empty() {
            return Ok(LineRead {
                stored: buf.len(),
                had_newline,
                truncated,
                eof: !any,
            });
        }
        any = true;
        let nl = chunk.iter().position(|&b| b == b'\n');
        let store_end = nl.unwrap_or(chunk.len());
        let room = cap.saturating_sub(buf.len());
        if store_end > 0 {
            let take = store_end.min(room);
            buf.extend_from_slice(&chunk[..take]);
            if take < store_end {
                truncated = true;
            }
        }
        let consumed = nl.map(|i| i + 1).unwrap_or(chunk.len());
        reader.consume(consumed);
        if nl.is_some() {
            had_newline = true;
            return Ok(LineRead {
                stored: buf.len(),
                had_newline,
                truncated,
                eof: false,
            });
        }
    }
}

/// Generic "read NDJSON where `field(record) > after`" — line-incremental and
/// per-line bounded (SEC-008), with partial-tail-line tolerance.
fn read_jsonl_since<T, F>(path: &Path, field: F, after: u64) -> io::Result<Vec<T>>
where
    T: DeserializeOwned,
    F: Fn(&T) -> u64,
{
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let lr = read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES)?;
        if lr.eof {
            break;
        }
        if lr.truncated {
            // Overlong line (flood/corruption guard). If it ended with a
            // newline it's a mid-file garbage line we skip; if not, it's an
            // overlong unframed tail — stop reading.
            if lr.had_newline {
                continue;
            }
            break;
        }
        let content = &buf[..lr.stored];
        if content.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let parsed = std::str::from_utf8(content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            .and_then(|s| {
                serde_json::from_str::<T>(s)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            });
        match parsed {
            Ok(record) => {
                if field(&record) > after {
                    out.push(record);
                }
            }
            Err(e) => {
                if !lr.had_newline {
                    // Half-written final line under concurrent append — skip.
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt jsonl line in {}: {}", path.display(), e),
                ));
            }
        }
    }
    Ok(out)
}

/// Sanitise an agent id for use as a filename stem (SEC-003).
///
/// Allowlist `[A-Za-z0-9:_-]`; every other byte (including `/`, `\\`, `.`,
/// whitespace) maps to `_`, and the result is length-capped. [`validate_agent_id`]
/// REJECTS a bad id at the write boundary; this function keeps READ-side path
/// construction inherently traversal-proof even for a legacy/garbage id already
/// present on disk — a stem produced here can contain no separator and no `.`,
/// so `inbox.join(stem + ".jsonl")` can never escape `inbox/`.
fn sanitize(agent: &str) -> String {
    let mut s: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_AGENT_ID_LEN)
        .collect();
    if s.is_empty() {
        s.push('_');
    }
    s
}
