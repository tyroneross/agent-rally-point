// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Core of the per-repo `rallyd` single-writer store daemon (BACKLOG S-P3,
//! ADR-03, ADR-05).
//!
//! This module lives INSIDE `rally-cli` on purpose: the daemon dispatcher needs
//! `pub(crate)` access to the `RoomStore`/`DirectRoomStore` facade (the warm
//! pool via `fact_store_handle()`, the per-request `set_engagement_scope`, the
//! owner-lock helpers) without any store internals leaking to the public API.
//! The `crates/rallyd` bin is a thin shell that only calls [`serve`].
//!
//! ## Chunk B — filled body (signature FROZEN in Chunk A, R5)
//!
//! [`ServeConfig`] and the [`serve`] signature are FINAL as of Chunk A: Chunk B
//! fills the body only. The normative startup order (R3) is:
//!
//! 1. Acquire the owner lock EXCLUSIVE, blocking (`acquire_owner_exclusive_blocking`)
//!    — kernel-enforced handover (ADR-01/L1). Held for the whole serving
//!    lifetime; released by the kernel on any death.
//! 2. Resolve the socket path (L7), unlink any stale socket (safe — EX proves no
//!    live daemon), bind the `UnixListener`, chmod 0600, write `.sock.addr` and
//!    `.pid` (both 0600). This is cheap/immediate so bounded-block corridor
//!    clients have a socket to connect to during a cold start.
//! 3. `DirectRoomStore::open_direct_at` — opens the direct store (reconcile may
//!    take seconds on a real room). The socket is already bound, so connects
//!    queue in the listen backlog and dispatch only once the store is ready
//!    (accept-after-ready). A store-open failure drains pending connections with
//!    a structured error, then exits non-zero (never hangs).
//! 4. Start the ONE dispatcher thread that owns the store (total order by
//!    construction, ADR-05) and the nonblocking accept loop.
//!
//! ## Serving
//!
//! Nonblocking accept loop polling a shutdown flag every ~100ms; one
//! per-connection reader thread reads exactly ONE line -> `StoreRequest`
//! (rejecting `> MAX_LINE_BYTES` with a structured transport error + close),
//! funnels `(req, reply)` over an mpsc channel to the single dispatcher. The
//! dispatcher applies each request's engagement via `set_engagement_scope`
//! BEFORE the op (L9/R4), then converts `Value -> Fact` for appends, runs the
//! store method, and converts the result back into the frozen `StoreOk`/
//! `StoreError` wire shapes. `Ping` is answered directly with `Pong` (the first
//! successful ping implies the dispatcher is live — Chunk C's `start` blocks on
//! it).
//!
//! ## Lifecycle
//!
//! SIGTERM/SIGINT set the shutdown flag; the accept loop drains, the dispatcher
//! drops the store (releasing the warm pool), the runtime files are unlinked,
//! and the EX guard is released at scope end. Optional `--idle-exit-secs N`
//! (default off) exits after N idle seconds — test hygiene against orphaned
//! daemons.
//!
//! ## R8 — segment-staleness refresh
//!
//! R8 (keeping the served view current as segments change) needs no separate
//! dispatcher-side gate: while serving, the daemon is the SOLE segment writer
//! (single writer, EX-held), and every store op already runs the per-op
//! reconcile fingerprint fast path (`store.rs`'s `reconcile_segments_and_db` —
//! the cheap len+mtime check at `store.rs:3453-3468`) before touching facts.db,
//! so a stale segment set is detected and reconciled inline on the next op. The
//! per-op reconcile therefore SUBSUMES a standalone R8 staleness gate here.
//!
//! ## Charter purity (G5)
//!
//! No `Command::new`, no external-process/agent spawn, no scheduling, no LLM or
//! network anywhere in this module. Spawning the daemon's own reader/dispatcher
//! threads is host-side store infrastructure, not WORK.

use std::path::PathBuf;

/// Configuration for a `rallyd` serve loop, parsed from the `crates/rallyd`
/// bin's args (or `rally daemon serve`). FROZEN in Chunk A (R5).
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Repo root whose `.rally/` the daemon owns and serves.
    pub repo_root: PathBuf,
    /// Optional idle-exit window: exit after this many idle seconds. `None`
    /// (default) = serve until signalled. Used mainly for test hygiene against
    /// orphaned daemons.
    pub idle_exit_secs: Option<u64>,
    /// Run in the foreground (log -> stderr). `false` = the detached posture the
    /// `rally daemon start` parent spawns (log -> `.rally/rallyd.log`, redirected
    /// by the parent per ADR-03). The daemon itself always logs to stderr; this
    /// field is carried frozen for the detach contract.
    pub foreground: bool,
}

/// Error returned by [`serve`]. A `pub` type so the `crates/rallyd` bin can
/// surface it without exposing `rally-cli`'s crate-private `RallyError`.
#[derive(Debug)]
pub struct ServeError {
    message: String,
}

impl ServeError {
    /// Construct a serve error with a human-readable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServeError {}

/// Run the `rallyd` serve loop for `config.repo_root`.
///
/// On unix this is the real single-writer daemon (see the module docs for the
/// normative startup order). On non-unix targets rallyd is unsupported (the
/// owner-lock handover is flock-based); [`serve`] returns a [`ServeError`] there
/// so the CLI degrades to today's direct, no-daemon behavior.
pub fn serve(config: ServeConfig) -> Result<(), ServeError> {
    #[cfg(unix)]
    {
        imp::serve_unix(config)
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        Err(ServeError::new(
            "rallyd is a unix-only daemon (the SH/EX ownership handover is flock-based)",
        ))
    }
}

#[cfg(unix)]
mod imp {
    use super::{ServeConfig, ServeError};

    use std::fs::{OpenOptions, Permissions};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError, Sender};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde::Serialize;
    use serde_json::Value;

    use crate::error::RallyError;
    use crate::store::{self, DirectRoomStore};
    use rally_protocol::store_wire::{
        MAX_LINE_BYTES, StoreError, StoreErrorKind, StoreOk, StoreOp, StoreRequest, StoreResponse,
        WIRE_VERSION,
    };

    /// Bound socket filename inside `.rally/` (L7).
    const SOCK_FILENAME: &str = "rallyd.sock";
    /// Discovery pointer holding the ACTUAL bound socket path (L7). The sole
    /// discovery mechanism clients read.
    const ADDR_FILENAME: &str = "rallyd.sock.addr";
    /// Daemon pid file (operator surface for `rally daemon stop`/`status`).
    const PID_FILENAME: &str = "rallyd.pid";

    /// macOS `sun_path` is 104 bytes incl. the NUL terminator (L7). A bound path
    /// longer than this must use the `$TMPDIR` hash fallback.
    const SUN_PATH_MAX: usize = 103;

    /// Accept-loop poll interval: how often the nonblocking accept loop wakes to
    /// re-check the shutdown flag + idle window. This bounds shutdown latency
    /// ONLY — accept throughput is bounded by the drain-all-pending inner loop
    /// (each wake accepts every queued connection down to `WouldBlock`), not by
    /// this interval.
    const ACCEPT_POLL: Duration = Duration::from_millis(100);

    /// Explicit listen backlog. `UnixListener::bind` calls `listen(2)` with a
    /// smallish platform default (128 on Linux, capped further by an older
    /// `SOMAXCONN`); under a burst of concurrent client connects the kernel
    /// queue can fill between accept-loop wakes and REFUSE connects
    /// (ECONNREFUSED), which the client's fresh-connection-per-op path
    /// misclassifies as R6's retryable "daemon stopped mid-request". A large
    /// backlog lets the queue hold a full burst until the next drain wake. 1024
    /// == a common `SOMAXCONN`; the kernel silently clamps to its own max.
    const LISTEN_BACKLOG: i32 = 1024;

    /// Per-connection read/write timeout. Bounds a stalled client so a reader
    /// thread cannot wedge indefinitely (each connection carries one request).
    const CONN_TIMEOUT: Duration = Duration::from_secs(10);

    /// SEC-003: hard cap on concurrent per-connection reader threads. Each
    /// accepted connection spawns one short-lived reader thread that funnels its
    /// single request to the ONE dispatcher and waits (bounded by
    /// [`CONN_TIMEOUT`]) for the reply. Without a cap, a wedged dispatcher (a
    /// store op that never returns) would let reader threads accumulate until
    /// `thread::spawn` itself fails/panics and kills the accept loop. The cap is
    /// deliberately GENEROUS — far above F4's ~24-connection burst — so it never
    /// rejects a legitimate burst; it only sheds load in a genuine
    /// thread-exhaustion / wedge scenario, answering over-cap connections with a
    /// retryable transport error. This bounds ACCEPT-side resources only; the
    /// dispatcher stays single-threaded (total order preserved).
    const MAX_READER_THREADS: usize = 512;

    /// Log the "waiting for direct writers to drain" line only if the EX acquire
    /// did not complete near-immediately (flock has no fairness guarantee).
    const OWNER_WAIT_LOG_THRESHOLD: Duration = Duration::from_millis(50);

    /// How long to drain pending connections with a structured error when the
    /// store fails to open (R3 "never hang"), before exiting non-zero.
    const STORE_OPEN_FAIL_DRAIN: Duration = Duration::from_secs(2);

    /// Process-global shutdown flag set by the SIGTERM/SIGINT handler and (in
    /// tests) by [`request_shutdown_for_test`]. Reset at the top of every
    /// [`serve_unix`] so a prior run's signal cannot pre-empt a fresh serve.
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    // Hand-declared signal binding, mirroring `store::unix_lock`'s hand-declared
    // flock `extern "C"` (no `libc`/`nix` dep — G5/Q-criteria: crates/rallyd
    // depends on rally-cli ONLY). `signal(2)` is exported by libc on macOS and
    // Linux and linked by default.
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        // `listen(2)` — re-called after `UnixListener::bind` to RAISE the accept
        // backlog above the platform default. Both Linux and BSD honor a second
        // `listen()` on an already-listening socket to update the backlog.
        // Exported by libc on macOS and Linux, linked by default (same posture
        // as the `flock`/`signal` hand-declared externs — no `libc`/`nix` dep).
        fn listen(fd: i32, backlog: i32) -> i32;
        // `getuid(2)` — the caller's real uid, used to name + verify the private
        // per-user `$TMPDIR` socket subdir (SEC-002). Same hand-declared posture
        // as `signal`/`listen`/`flock` — no `libc`/`nix` dep. Cannot fail
        // (POSIX: getuid is always successful).
        fn getuid() -> u32;
    }

    /// Async-signal-safe handler: only touch a lock-free atomic (SIGTERM/SIGINT).
    extern "C" fn on_signal(_sig: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    fn install_signal_handlers() {
        // SAFETY: `on_signal` is async-signal-safe (a single atomic store).
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
    }

    /// One job on the dispatcher channel: a parsed request + the oneshot reply
    /// channel back to the connection's reader thread.
    type Job = (StoreRequest, Sender<StoreResponse>);

    fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn log(msg: &str) {
        eprintln!("rallyd[{}] {}", std::process::id(), msg);
    }

    /// Deterministic short digest for the `$TMPDIR` socket-name fallback (L7).
    ///
    /// FNV-1a/64. NOTE (deliberate deviation from the plan's "sha256"): the
    /// socket NAME is a daemon-internal detail — clients discover the bound path
    /// EXCLUSIVELY via `.sock.addr` (L7) and never recompute this hash, so no
    /// cryptographic property is required, only per-repo uniqueness and
    /// determinism within this binary. FNV-1a keeps the daemon std-only with
    /// zero new deps (a self-contained sha256 would add ~80 lines of crypto to
    /// get exactly right for no observable benefit). Surfaced to the orchestrator.
    fn short_hash(input: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in input.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    /// The canonical repo-root string used for both the socket-name hash and the
    /// `Pong` identity (so a client's `repo_root` verification compares against a
    /// canonical path). Falls back to the given path if canonicalization fails.
    fn canonical_repo_root(root: &Path) -> String {
        std::fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    /// Resolve the bind path per L7: `.rally/rallyd.sock`, or a
    /// `$TMPDIR/rallyd-<uid>/rallyd-<hash>.sock` fallback when the absolute path
    /// would exceed the platform `sun_path` limit.
    ///
    /// SEC-002: the fallback binds inside a PER-USER subdir (`rallyd-<uid>/`)
    /// rather than directly in world-writable `$TMPDIR`. `$TMPDIR` (often
    /// `/tmp`, mode 1777) lets any local user pre-create a predictable socket
    /// NAME and squat it (a DoS on the daemon's bind). Nesting under a
    /// uid-owned, 0700 subdir (created + ownership-verified by
    /// [`secure_tmp_socket_dir`] before bind) removes the cross-user squat
    /// surface. The `.sock.addr` pointer remains the SOLE discovery path (L7),
    /// so this new nesting is invisible to clients.
    fn resolve_socket_path(repo_root: &Path, rally_dir: &Path) -> PathBuf {
        let primary = rally_dir.join(SOCK_FILENAME);
        if primary.as_os_str().len() <= SUN_PATH_MAX {
            return primary;
        }
        // SAFETY: `getuid` is a POSIX call that cannot fail and touches no
        // shared state.
        let uid = unsafe { getuid() };
        let tmp = std::env::var_os("TMPDIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let digest = short_hash(&canonical_repo_root(repo_root));
        tmp.join(format!("rallyd-{uid}"))
            .join(format!("rallyd-{}.sock", &digest[..12]))
    }

    /// SEC-002: create + verify the per-user `$TMPDIR/rallyd-<uid>/` socket
    /// subdir BEFORE binding inside it. Creates it mode 0700 (idempotent — an
    /// existing dir is accepted, then re-checked), then `lstat`-verifies (does
    /// NOT follow symlinks) that the path is a real DIRECTORY (not a symlink an
    /// attacker planted to redirect the bind) owned by the current uid. A
    /// mismatch is a hard error — the daemon refuses to bind into a dir it
    /// cannot prove is private to this user. Finally re-asserts 0700 so a
    /// pre-existing dir we own but with looser bits is tightened.
    fn secure_tmp_socket_dir(dir: &Path) -> Result<(), ServeError> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(ServeError::new(format!(
                    "create private socket dir {}: {e}",
                    dir.display()
                )));
            }
        }
        // lstat (symlink_metadata): a symlink here has file_type().is_symlink()
        // and is NOT is_dir(), so a planted symlink is rejected outright.
        let meta = std::fs::symlink_metadata(dir).map_err(|e| {
            ServeError::new(format!("stat private socket dir {}: {e}", dir.display()))
        })?;
        if !meta.file_type().is_dir() {
            return Err(ServeError::new(format!(
                "private socket dir {} is not a directory (symlink/squat?); refusing to bind",
                dir.display()
            )));
        }
        let uid = unsafe { getuid() };
        if meta.uid() != uid {
            return Err(ServeError::new(format!(
                "private socket dir {} is owned by uid {}, not {} (squat?); refusing to bind",
                dir.display(),
                meta.uid(),
                uid
            )));
        }
        // We own it and it's a real dir: tighten perms to 0700 in case it
        // pre-existed with looser bits.
        std::fs::set_permissions(dir, Permissions::from_mode(0o700)).map_err(|e| {
            ServeError::new(format!("chmod private socket dir {}: {e}", dir.display()))
        })?;
        Ok(())
    }

    fn bind_socket(socket_path: &Path) -> Result<UnixListener, ServeError> {
        // Stale unlink is safe: we hold EX, which proves no live daemon owns
        // this socket (L7).
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ServeError::new(format!("create {}: {e}", parent.display())))?;
        }
        let listener = UnixListener::bind(socket_path)
            .map_err(|e| ServeError::new(format!("bind {}: {e}", socket_path.display())))?;
        // Raise the accept backlog above the platform default so a burst of
        // concurrent connects queues in the kernel instead of being refused
        // between accept-loop wakes (see LISTEN_BACKLOG). `bind` already
        // socket()+bind()+listen()ed at the default; this second listen(2)
        // updates the backlog. Best-effort: a failure here leaves the default
        // backlog in place (still functional, just the pre-fix throughput).
        // SAFETY: `listener` owns the fd for the duration of this call.
        let rc = unsafe { listen(listener.as_raw_fd(), LISTEN_BACKLOG) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            log(&format!(
                "listen(backlog={LISTEN_BACKLOG}) failed: {e}; keeping default backlog"
            ));
        }
        std::fs::set_permissions(socket_path, Permissions::from_mode(0o600))
            .map_err(|e| ServeError::new(format!("chmod {}: {e}", socket_path.display())))?;
        Ok(listener)
    }

    fn write_private_file(path: &Path, contents: &str) -> Result<(), ServeError> {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| ServeError::new(format!("open {}: {e}", path.display())))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| ServeError::new(format!("write {}: {e}", path.display())))
    }

    fn cleanup(socket: &Path, addr: &Path, pid: &Path) {
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_file(addr);
        let _ = std::fs::remove_file(pid);
    }

    /// Read exactly one `\n`-terminated line, capped at [`MAX_LINE_BYTES`]. An
    /// over-long line is the transport-error class (R7): reject + close.
    fn read_request_line(stream: &UnixStream) -> Result<String, StoreError> {
        let clone = stream
            .try_clone()
            .map_err(|e| StoreError::transport(format!("clone stream: {e}")))?;
        let mut reader = BufReader::new(clone);
        let mut buf = Vec::new();
        // Read at most MAX_LINE_BYTES + 1 so an exactly-at-limit line still
        // parses while anything larger is detected and rejected.
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);
        limited
            .read_until(b'\n', &mut buf)
            .map_err(|e| StoreError::transport(format!("read request: {e}")))?;
        if buf.len() > MAX_LINE_BYTES {
            return Err(StoreError::transport(format!(
                "request exceeds {MAX_LINE_BYTES} bytes; run `rally daemon status`"
            )));
        }
        String::from_utf8(buf).map_err(|e| StoreError::transport(format!("non-utf8 request: {e}")))
    }

    fn write_response(mut stream: &UnixStream, resp: &StoreResponse) -> std::io::Result<()> {
        let mut line =
            serde_json::to_string(resp).map_err(|e| std::io::Error::other(e.to_string()))?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()
    }

    /// SEC-003: answer an over-cap / un-spawnable connection immediately with a
    /// retryable transport error, then close. The client's fresh-connection-per-
    /// op path maps this to R6's "retry", so a momentary reader-thread saturation
    /// sheds load gracefully instead of the accept loop dying on a `spawn` panic.
    fn respond_busy(stream: &UnixStream) {
        let _ = stream.set_read_timeout(Some(CONN_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONN_TIMEOUT));
        let resp = StoreResponse::Err(StoreError::transport("daemon busy; retry"));
        let _ = write_response(stream, &resp);
    }

    /// Per-connection reader thread: read one request line, route it through the
    /// dispatcher, write one reply line, close. Any framing/parse failure yields
    /// a structured error reply (never a panic — falsifier B).
    fn handle_conn(stream: UnixStream, job_tx: Sender<Job>) {
        let _ = stream.set_read_timeout(Some(CONN_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONN_TIMEOUT));

        let response = match read_request_line(&stream) {
            Err(e) => StoreResponse::Err(e),
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    StoreResponse::Err(StoreError::transport("empty request line"))
                } else {
                    match serde_json::from_str::<StoreRequest>(trimmed) {
                        Err(e) => StoreResponse::Err(StoreError::new(
                            StoreErrorKind::Command,
                            format!("malformed request: {e}"),
                        )),
                        Ok(req) => {
                            let (reply_tx, reply_rx) = mpsc::channel();
                            if job_tx.send((req, reply_tx)).is_err() {
                                StoreResponse::Err(StoreError::transport(
                                    "daemon dispatcher stopped; retry",
                                ))
                            } else {
                                // SEC-003: bound the wait on the dispatcher reply
                                // with CONN_TIMEOUT. A wedged store op would
                                // otherwise park this reader thread FOREVER;
                                // timing out lets the reader shed (close the
                                // connection, free the thread) with a retryable
                                // error instead of accumulating parked threads.
                                match reply_rx.recv_timeout(CONN_TIMEOUT) {
                                    Ok(resp) => resp,
                                    Err(RecvTimeoutError::Timeout) => StoreResponse::Err(
                                        StoreError::transport("daemon store op timed out; retry"),
                                    ),
                                    Err(RecvTimeoutError::Disconnected) => {
                                        StoreResponse::Err(StoreError::transport(
                                            "daemon dispatcher dropped the reply; retry",
                                        ))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        let _ = write_response(&stream, &response);
    }

    /// Map an internal [`RallyError`] onto the frozen wire [`StoreError`] with
    /// exit-code parity (G8). `Io`/`Json` collapse to `Internal` (source dropped
    /// over the wire).
    fn rally_to_wire(err: RallyError) -> StoreError {
        let message = err.to_string();
        let kind = match err {
            RallyError::Usage(_) => StoreErrorKind::Usage,
            RallyError::NotFound(_) => StoreErrorKind::NotFound,
            RallyError::Command(_) => StoreErrorKind::Command,
            RallyError::Message(_) => StoreErrorKind::Message,
            RallyError::Io { .. } | RallyError::Json { .. } => StoreErrorKind::Internal,
        };
        StoreError::new(kind, message)
    }

    fn fact_from_value(v: Value) -> Result<store::Fact, StoreError> {
        serde_json::from_value(v)
            .map_err(|e| StoreError::new(StoreErrorKind::Command, format!("bad fact payload: {e}")))
    }

    fn to_wire_value<T: Serialize>(v: &T) -> Result<Value, StoreError> {
        serde_json::to_value(v)
            .map_err(|e| StoreError::new(StoreErrorKind::Internal, format!("serialize reply: {e}")))
    }

    fn to_wire_values<'a, T: Serialize + 'a>(
        items: impl IntoIterator<Item = &'a T>,
    ) -> Result<Vec<Value>, StoreError> {
        items.into_iter().map(to_wire_value).collect()
    }

    /// Snapshots do NOT go through [`to_wire_value`]. Four `RoomSnapshot`
    /// projections are `#[serde(skip)]` so they stay out of the public room
    /// JSON, and serializing the snapshot plainly would drop them on the wire
    /// too — which made the daemon path rank, checkpoint, and coalesce
    /// differently from the direct path (design audit D1/D6). See
    /// `store::SnapshotInternals`.
    fn snapshot_to_wire(snapshot: &store::RoomSnapshot) -> Result<Value, StoreError> {
        store::snapshot_to_wire_value(snapshot).map_err(|e| {
            StoreError::new(
                StoreErrorKind::Internal,
                format!("serialize snapshot reply: {e}"),
            )
        })
    }

    /// Dispatch one request against the single-owner store. Applies the request's
    /// engagement BEFORE the op (L9/R4); answers `Ping` directly.
    fn dispatch_one(
        store: &mut DirectRoomStore,
        repo_root: &str,
        req: StoreRequest,
    ) -> StoreResponse {
        if req.wire_version != WIRE_VERSION {
            return StoreResponse::Err(StoreError::transport(format!(
                "wire_version mismatch: daemon speaks {WIRE_VERSION}, client sent {}; \
                 run `rally daemon status`",
                req.wire_version
            )));
        }
        if matches!(req.op, StoreOp::Ping) {
            return StoreResponse::Ok(StoreOk::Pong {
                repo_root: repo_root.to_string(),
                pid: std::process::id(),
                wire_version: WIRE_VERSION,
            });
        }
        // Per-request engagement rebind (L9/R4): safe because the dispatcher is
        // single-threaded. The daemon NEVER consults its own process env here.
        store.set_engagement_scope(req.engagement.clone());
        match run_op(store, req.op) {
            Ok(ok) => StoreResponse::Ok(ok),
            Err(e) => StoreResponse::Err(e),
        }
    }

    fn run_op(store: &mut DirectRoomStore, op: StoreOp) -> Result<StoreOk, StoreError> {
        Ok(match op {
            StoreOp::Ping => {
                // Handled in dispatch_one before engagement rebinding.
                return Err(StoreError::new(
                    StoreErrorKind::Command,
                    "ping is answered before dispatch",
                ));
            }
            StoreOp::AppendFact { fact } => {
                let f = fact_from_value(fact)?;
                let out = store.append_fact(&f).map_err(rally_to_wire)?;
                StoreOk::AppendFact {
                    fact: to_wire_value(&out)?,
                }
            }
            StoreOp::AppendFactVerified { fact } => {
                let f = fact_from_value(fact)?;
                let out = store.append_fact_verified(&f).map_err(rally_to_wire)?;
                StoreOk::AppendFactVerified {
                    fact: to_wire_value(&out)?,
                }
            }
            StoreOp::AppendStateTransitionVerified { fact } => {
                let f = fact_from_value(fact)?;
                let out = store
                    .append_state_transition_verified(&f)
                    .map_err(rally_to_wire)?;
                StoreOk::AppendStateTransitionVerified {
                    fact: to_wire_value(&out)?,
                }
            }
            StoreOp::AppendSessionFactIfContext {
                fact,
                expected_context_version,
            } => {
                let f = fact_from_value(fact)?;
                let out = store
                    .append_session_fact_if_context(&f, expected_context_version)
                    .map_err(rally_to_wire)?;
                let fact = match out {
                    Some(x) => Some(to_wire_value(&x)?),
                    None => None,
                };
                StoreOk::AppendSessionFactIfContext { fact }
            }
            StoreOp::Facts => {
                let facts = store.facts().map_err(rally_to_wire)?;
                StoreOk::Facts {
                    facts: to_wire_values(&facts)?,
                }
            }
            StoreOp::RebuildClaimIndex => {
                store.rebuild_claim_index().map_err(rally_to_wire)?;
                StoreOk::RebuildClaimIndex
            }
            StoreOp::RenewClaimLease {
                claim_id,
                lease_expires_at,
            } => {
                let record = store
                    .renew_claim_lease(&claim_id, lease_expires_at)
                    .map_err(rally_to_wire)?;
                let record = match record {
                    Some(r) => Some(to_wire_value(&r)?),
                    None => None,
                };
                StoreOk::RenewClaimLease { record }
            }
            StoreOp::ExpireClaimLeasesAt { now_rfc3339 } => {
                let now = chrono::DateTime::parse_from_rfc3339(&now_rfc3339)
                    .map_err(|e| {
                        StoreError::new(
                            StoreErrorKind::Command,
                            format!("bad rfc3339 timestamp: {e}"),
                        )
                    })?
                    .with_timezone(&chrono::Utc);
                let facts = store.expire_claim_leases_at(now).map_err(rally_to_wire)?;
                StoreOk::ExpireClaimLeasesAt {
                    facts: to_wire_values(&facts)?,
                }
            }
            StoreOp::SessionFactsWithContextVersion => {
                let (facts, context_version) = store
                    .session_facts_with_context_version()
                    .map_err(rally_to_wire)?;
                StoreOk::SessionFactsWithContextVersion {
                    facts: to_wire_values(&facts)?,
                    context_version,
                }
            }
            StoreOp::SnapshotWithArchived { include_archived } => {
                let snap = store
                    .snapshot_with_archived(include_archived)
                    .map_err(rally_to_wire)?;
                StoreOk::Snapshot {
                    snapshot: snapshot_to_wire(&snap)?,
                }
            }
            StoreOp::SnapshotWithReadersArchived { include_archived } => {
                let snap = store
                    .snapshot_with_readers_archived(include_archived)
                    .map_err(rally_to_wire)?;
                StoreOk::SnapshotWithReaders {
                    snapshot: snapshot_to_wire(&snap)?,
                }
            }
            StoreOp::LastCheckpointSeq { tool } => {
                let seq = store.last_checkpoint_seq(&tool).map_err(rally_to_wire)?;
                StoreOk::LastCheckpointSeq { seq }
            }
            StoreOp::MaybeAppendReadCheckpoint { tool, read_seq } => {
                let out = store
                    .maybe_append_read_checkpoint(&tool, read_seq)
                    .map_err(rally_to_wire)?;
                let fact = match out {
                    Some(x) => Some(to_wire_value(&x)?),
                    None => None,
                };
                StoreOk::MaybeAppendReadCheckpoint { fact }
            }
            StoreOp::ProjectReadReceipts { max_seq } => {
                let receipts = store
                    .project_read_receipts(max_seq)
                    .map_err(rally_to_wire)?;
                StoreOk::ProjectReadReceipts {
                    receipts: to_wire_values(&receipts)?,
                }
            }
        })
    }

    /// Drain any pending connections with a structured error for a bounded
    /// window when the store fails to open (R3 "never hang"), then return.
    fn respond_all_with_error(listener: &UnixListener, err: &StoreError, window: Duration) {
        let _ = listener.set_nonblocking(true);
        let resp = StoreResponse::Err(err.clone());
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(CONN_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(CONN_TIMEOUT));
                    // Best-effort: discard the request line, then reply the error.
                    let _ = read_request_line(&stream);
                    let _ = write_response(&stream, &resp);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn serve_unix(config: ServeConfig) -> Result<(), ServeError> {
        // `daemon serve` is a lifetime process, not a bounded command. Both the
        // CLI dispatcher and standalone rallyd must reach this function with no
        // watchdog installed; otherwise a short SQLite budget would be derived
        // from a deadline guaranteed to expire while serving. Enforce that
        // contract at the real serve entry point, not only in parser tests.
        if crate::watchdog_remaining().is_some() {
            return Err(ServeError::new(
                "daemon serve entered with a command watchdog armed; refusing to start",
            ));
        }
        // Reset the process-global flag so a prior signal (or test run) does not
        // pre-empt this serve, then install the signal handlers.
        SHUTDOWN.store(false, Ordering::SeqCst);
        install_signal_handlers();
        let _ = config.foreground; // frozen field; parent handles log redirection.

        let repo_root = config.repo_root.clone();
        let rally_dir = repo_root.join(".rally");
        std::fs::create_dir_all(&rally_dir)
            .map_err(|e| ServeError::new(format!("create {}: {e}", rally_dir.display())))?;

        // (1) Owner lock EXCLUSIVE, blocking (ADR-01/L1). Held for the whole
        // serving lifetime; the kernel releases it on any death.
        let t0 = Instant::now();
        log(&format!(
            "acquiring exclusive owner lock at {}",
            rally_dir.display()
        ));
        let _owner = store::acquire_owner_exclusive_blocking(&rally_dir)
            .map_err(|e| ServeError::new(format!("acquire owner EX lock: {e}")))?;
        if t0.elapsed() > OWNER_WAIT_LOG_THRESHOLD {
            log(&format!(
                "waited {:?} for direct writers to drain",
                t0.elapsed()
            ));
        }

        // (2) Socket + .addr + pid — cheap and immediate, BEFORE the store open
        // (R3), so bounded-block corridor clients have a socket during a cold
        // start.
        let socket_path = resolve_socket_path(&repo_root, &rally_dir);
        // SEC-002: when the socket falls back OUTSIDE `.rally/` (the `$TMPDIR`
        // over-long-path case), its parent is the per-user `rallyd-<uid>/`
        // subdir — harden + ownership-verify it before binding. The primary
        // `.rally/` path (parent == rally_dir) is repo-local and unaffected.
        if socket_path.parent() != Some(rally_dir.as_path())
            && let Some(parent) = socket_path.parent()
        {
            secure_tmp_socket_dir(parent)?;
        }
        let addr_path = rally_dir.join(ADDR_FILENAME);
        let pid_path = rally_dir.join(PID_FILENAME);
        let listener = bind_socket(&socket_path)?;
        write_private_file(&addr_path, &socket_path.to_string_lossy())?;
        write_private_file(&pid_path, &std::process::id().to_string())?;
        log(&format!(
            "listening on {} (pid {})",
            socket_path.display(),
            std::process::id()
        ));

        // (3) Open the direct store (reconcile may take seconds; connects queue
        // in the listen backlog). On failure, drain pending connections with a
        // structured error, then exit non-zero.
        let mut store = match DirectRoomStore::open_direct_at(repo_root.clone()) {
            Ok(s) => s,
            Err(e) => {
                let err = StoreError::new(
                    StoreErrorKind::Internal,
                    format!("daemon store open failed: {e}"),
                );
                log(&format!("store open failed: {e}; draining pending clients"));
                respond_all_with_error(&listener, &err, STORE_OPEN_FAIL_DRAIN);
                cleanup(&socket_path, &addr_path, &pid_path);
                return Err(ServeError::new(format!("store open failed: {e}")));
            }
        };

        // ==== G10/R1 WARM-POOL INSTALL (A-amendment landed) ====
        // Install the ONE warm facts.db pool so the hot interior ops
        // (append/query/snapshot) reuse it via fact_store_handle() instead of
        // churning a pool per request — the in-process re-creation of #50
        // (factstr-sqlite 0.5.2's un-closed-on-Drop background checkpoint racing
        // the next open) that R1 exists to prevent. Single writer, total order,
        // and now a single warm pool: G10 satisfied.
        store
            .install_warm_fact_store()
            .map_err(|e| ServeError::new(format!("install warm pool: {e}")))?;

        let canonical_root = Arc::new(canonical_repo_root(&repo_root));

        // (4) ONE dispatcher thread owns the store => total order by construction.
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let last_activity = Arc::new(AtomicI64::new(now_millis()));
        let disp_root = canonical_root.clone();
        let disp_activity = last_activity.clone();
        let dispatcher = thread::spawn(move || {
            let mut store = store;
            while let Ok((req, reply)) = job_rx.recv() {
                let resp = dispatch_one(&mut store, disp_root.as_str(), req);
                disp_activity.store(now_millis(), Ordering::Relaxed);
                let _ = reply.send(resp);
            }
            // Channel closed (all senders dropped): drop the store here, which
            // releases the (warm) pool before the runtime files are unlinked.
            drop(store);
        });

        // Nonblocking accept loop: poll the shutdown flag + idle window ~every
        // ACCEPT_POLL; on each wake DRAIN ALL pending connections (accept until
        // WouldBlock), spawning one reader thread per connection, THEN sleep.
        // Draining all-per-wake — not one-per-wake — is what makes the accept
        // side keep up with a burst: the kernel backlog holds connections that
        // arrive between wakes and this inner loop empties it fully each time,
        // so clients no longer see ECONNREFUSED (which the fresh-connect client
        // path misreads as R6's "daemon stopped mid-request; retry"). The
        // dispatcher stays single-threaded (total order preserved); we widen
        // ONLY accept concurrency (reader threads feeding the one mpsc).
        let _ = listener.set_nonblocking(true);
        // SEC-003: live count of concurrent per-connection reader threads, so a
        // wedged dispatcher can't let them accumulate until `spawn` panics and
        // takes the accept loop with it. Scoped to THIS serve (an Arc, not a
        // process-global) so concurrent in-process daemons (tests) don't share a
        // counter.
        let reader_threads = Arc::new(AtomicUsize::new(0));
        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                log("shutdown signal received; draining");
                break;
            }
            if let Some(secs) = config.idle_exit_secs {
                let idle_ms = now_millis() - last_activity.load(Ordering::Relaxed);
                if idle_ms >= (secs as i64) * 1000 {
                    log("idle-exit window elapsed; stopping");
                    break;
                }
            }
            // Drain every queued connection before going back to the poll sleep.
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Reserve a reader-thread slot. Over the GENEROUS cap
                        // (well above any legitimate burst) ⇒ shed this
                        // connection with a retryable transport error and move on
                        // rather than spawning unboundedly.
                        let reserved = reader_threads.fetch_add(1, Ordering::SeqCst);
                        if reserved >= MAX_READER_THREADS {
                            reader_threads.fetch_sub(1, Ordering::SeqCst);
                            respond_busy(&stream);
                            continue;
                        }
                        let jt = job_tx.clone();
                        let rt = reader_threads.clone();
                        // Keep a fallback handle so a `spawn` FAILURE can still
                        // answer with a structured error instead of panicking.
                        let busy_fallback = stream.try_clone().ok();
                        match thread::Builder::new().spawn(move || {
                            handle_conn(stream, jt);
                            rt.fetch_sub(1, Ordering::SeqCst);
                        }) {
                            Ok(_handle) => {}
                            Err(e) => {
                                // Release the reserved slot; the thread never ran.
                                reader_threads.fetch_sub(1, Ordering::SeqCst);
                                log(&format!(
                                    "reader thread spawn failed: {e}; shedding connection"
                                ));
                                if let Some(s) = busy_fallback {
                                    respond_busy(&s);
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Backlog drained; return to the shutdown/idle poll.
                        break;
                    }
                    Err(e) => {
                        log(&format!("accept error: {e}"));
                        break;
                    }
                }
            }
            thread::sleep(ACCEPT_POLL);
        }

        // Lifecycle: drop the accept-loop sender so the dispatcher drains its
        // queue and exits (dropping the store); join it; then unlink the runtime
        // files. The EX guard (`_owner`) releases at scope end — AFTER unlink —
        // so a fresh daemon sees no stale socket before it acquires EX.
        drop(job_tx);
        let _ = dispatcher.join();
        cleanup(&socket_path, &addr_path, &pid_path);
        log("stopped");
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn request_shutdown_for_test() {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};

        fn unique_repo_root(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "rallyd-core-{tag}-{}-{}",
                std::process::id(),
                now_millis()
            ));
            std::fs::create_dir_all(dir.join(".rally")).unwrap();
            dir
        }

        /// One line-delimited round trip against the daemon (mirrors the client
        /// `round_trip` framing the daemon serves).
        fn round_trip(socket: &Path, req: &StoreRequest) -> StoreResponse {
            let stream = UnixStream::connect(socket).expect("connect daemon socket");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut w = &stream;
            let mut line = serde_json::to_string(req).unwrap();
            line.push('\n');
            w.write_all(line.as_bytes()).unwrap();
            w.flush().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut resp = String::new();
            reader.read_line(&mut resp).unwrap();
            serde_json::from_str(resp.trim()).expect("parse response line")
        }

        /// Wait for `.sock.addr` and read the bound socket path.
        fn wait_for_addr(rally_dir: &Path) -> PathBuf {
            let addr = rally_dir.join(ADDR_FILENAME);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(s) = std::fs::read_to_string(&addr) {
                    let s = s.trim();
                    if !s.is_empty() {
                        return PathBuf::from(s);
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("daemon never wrote {}", addr.display());
        }

        fn ping_ok(socket: &Path) -> bool {
            let req = StoreRequest::new(None, StoreOp::Ping);
            matches!(
                round_trip(socket, &req),
                StoreResponse::Ok(StoreOk::Pong { .. })
            )
        }

        #[test]
        fn daemon_rejects_v1_requests_after_the_snapshot_wire_change() {
            assert_eq!(WIRE_VERSION, 2, "this control grades the v1 to v2 cutover");
            let repo_root = unique_repo_root("reject-v1");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let request = StoreRequest {
                wire_version: 1,
                engagement: None,
                op: StoreOp::Ping,
            };
            match dispatch_one(&mut store, "/expected/repo", request) {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::Transport);
                    assert!(error.message.contains("daemon speaks 2"));
                    assert!(error.message.contains("client sent 1"));
                }
                other => panic!("v1 request was not rejected: {other:?}"),
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn daemon_serve_rejects_an_armed_command_watchdog() {
            let repo_root = unique_repo_root("armed-watchdog");
            let _deadline =
                crate::install_watchdog_deadline(Instant::now() + Duration::from_secs(3));
            let err = super::serve_unix(ServeConfig {
                repo_root: repo_root.clone(),
                idle_exit_secs: Some(1),
                foreground: true,
            })
            .unwrap_err();
            assert!(err.to_string().contains("watchdog armed"), "{err}");
            std::fs::remove_dir_all(repo_root).ok();
        }

        // Smoke test (Chunk B integration checkpoint): start the daemon on a temp
        // room, ping it, do two CONSECUTIVE appends + a snapshot round trip over a
        // raw socket, assert single-store total order, then shut down and confirm
        // the socket/.addr/pid are removed. Also the STRICT warm-pool proof
        // (G10, f1): the A-amendment warm-pool installer landed (serve_unix calls
        // `install_warm_fact_store` after `open_direct_at`), so this test now
        // asserts the `fact_store_handle` cold-open counter stays ZERO across the
        // two appends + snapshot — proving the hot path reuses the ONE warm pool
        // with no per-op churn, not merely that the appends succeed.
        #[test]
        fn smoke_serve_ping_append_snapshot_and_shutdown() {
            let repo_root = unique_repo_root("smoke");
            let rally_dir = repo_root.join(".rally");

            let cfg = ServeConfig {
                repo_root: repo_root.clone(),
                // Idle backstop so the daemon self-exits even if the test panics
                // before requesting shutdown.
                idle_exit_secs: Some(8),
                foreground: true,
            };
            let handle = thread::spawn(move || {
                let _ = super::serve_unix(cfg);
            });

            let socket = wait_for_addr(&rally_dir);
            // Block until the dispatcher answers a ping (dispatcher-live gate, R3).
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !ping_ok(&socket) {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(ping_ok(&socket), "daemon never answered a ping");

            // Ping identity carries the canonical repo_root + wire version.
            match round_trip(&socket, &StoreRequest::new(None, StoreOp::Ping)) {
                StoreResponse::Ok(StoreOk::Pong {
                    wire_version,
                    repo_root: pong_root,
                    ..
                }) => {
                    assert_eq!(wire_version, WIRE_VERSION);
                    assert_eq!(pong_root, canonical_repo_root(&repo_root));
                }
                other => panic!("unexpected ping reply: {other:?}"),
            }

            // Reads succeed on the fresh room.
            match round_trip(&socket, &StoreRequest::new(None, StoreOp::Facts)) {
                StoreResponse::Ok(StoreOk::Facts { .. }) => {}
                other => panic!("unexpected facts reply: {other:?}"),
            }

            // G10 warm-pool proof (f1): start counting `fact_store_handle`
            // cold-branch opens under THIS daemon's repo_root. The daemon store
            // holds a warm pool, so its append/query/snapshot ops must NEVER
            // reach the cold branch; a nonzero count would mean per-op pool
            // churn (the #50 regression the warm pool prevents). Scoped to this
            // repo_root so parallel unrelated direct-mode tests can't contaminate
            // it. (The daemon's own `open_direct_at`/`install_warm_fact_store`
            // open the pool DIRECTLY, not via `fact_store_handle`, so they don't
            // count either — only hot-path cold opens would.)
            crate::store::watch_cold_opens_under(&repo_root);

            // Two consecutive engagement-scoped appends through the ONE store.
            let append = |subject: &str| {
                let fact = serde_json::json!({
                    "kind": "lesson",
                    "subject": subject,
                    "tool": "claude_code:01",
                    "scope": ["rallyd-smoke"],
                });
                round_trip(
                    &socket,
                    &StoreRequest::new(
                        Some("rallyd-smoke".to_string()),
                        StoreOp::AppendFact { fact },
                    ),
                )
            };
            match append("first append") {
                StoreResponse::Ok(StoreOk::AppendFact { .. }) => {}
                other => panic!("first append not Ok: {other:?}"),
            }
            match append("second append") {
                StoreResponse::Ok(StoreOk::AppendFact { .. }) => {}
                other => panic!("second append not Ok: {other:?}"),
            }

            // The two appends are visible through a snapshot read (same store).
            match round_trip(
                &socket,
                &StoreRequest::new(
                    None,
                    StoreOp::SnapshotWithArchived {
                        include_archived: false,
                    },
                ),
            ) {
                StoreResponse::Ok(StoreOk::Snapshot { .. }) => {}
                other => panic!("unexpected snapshot reply: {other:?}"),
            }

            // G10 proof (f1): the two appends + snapshot were served entirely
            // through the ONE warm pool — the hot path never cold-opened a fresh
            // facts.db pool. (Recovery/reconcile paths that open directly are not
            // routed through `fact_store_handle` and so are not counted.)
            assert_eq!(
                crate::store::cold_open_count(),
                0,
                "daemon hot path cold-opened the facts.db pool — G10 warm-pool churn regression"
            );

            // A malformed request line yields a structured error, not a crash.
            {
                let stream = UnixStream::connect(&socket).unwrap();
                let mut w = &stream;
                w.write_all(b"{not json}\n").unwrap();
                w.flush().unwrap();
                let mut reader = BufReader::new(&stream);
                let mut resp = String::new();
                reader.read_line(&mut resp).unwrap();
                let parsed: StoreResponse = serde_json::from_str(resp.trim()).unwrap();
                assert!(
                    matches!(parsed, StoreResponse::Err(_)),
                    "expected structured error"
                );
            }

            // Graceful shutdown: request stop, join, assert runtime files gone.
            super::request_shutdown_for_test();
            handle.join().expect("serve thread joined");

            assert!(
                !socket.exists(),
                "socket not unlinked: {}",
                socket.display()
            );
            assert!(
                !rally_dir.join(ADDR_FILENAME).exists(),
                ".addr not unlinked"
            );
            assert!(!rally_dir.join(PID_FILENAME).exists(), "pid not unlinked");

            let _ = std::fs::remove_dir_all(&repo_root);
        }

        #[test]
        fn socket_path_falls_back_when_over_sun_path_limit() {
            // A short .rally path uses the primary socket.
            let short = PathBuf::from("/tmp/r");
            let p = resolve_socket_path(&short, &short.join(".rally"));
            assert!(p.ends_with(SOCK_FILENAME));

            // A pathologically deep repo forces the $TMPDIR hash fallback.
            let deep = PathBuf::from(format!("/tmp/{}", "d".repeat(SUN_PATH_MAX)));
            let fb = resolve_socket_path(&deep, &deep.join(".rally"));
            let name = fb.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("rallyd-") && name.ends_with(".sock"));
            assert!(
                fb.as_os_str().len() <= SUN_PATH_MAX,
                "fallback still over-long"
            );
        }

        #[test]
        fn short_hash_is_deterministic() {
            assert_eq!(short_hash("/a/b/c"), short_hash("/a/b/c"));
            assert_ne!(short_hash("/a/b/c"), short_hash("/a/b/d"));
            assert_eq!(short_hash("x").len(), 16);
        }
    }
}
