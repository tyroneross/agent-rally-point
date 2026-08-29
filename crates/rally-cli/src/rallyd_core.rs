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
//! explicitly closes the warm pool, and the runtime files are unlinked. The EX
//! guard is released after successful close; a close timeout deliberately keeps
//! it until process exit so detached store work cannot overlap a replacement
//! owner. Optional `--idle-exit-secs N` (default off) exits after N idle seconds
//! — test hygiene against orphaned daemons.
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
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
    /// surfaces as R6's routed transport failure. A large
    /// backlog lets the queue hold a full burst until the next drain wake. 1024
    /// == a common `SOMAXCONN`; the kernel silently clamps to its own max.
    const LISTEN_BACKLOG: i32 = 1024;

    /// Per-connection read/write timeout. Bounds a stalled client so a reader
    /// thread cannot wedge indefinitely (each connection carries one request).
    const CONN_TIMEOUT: Duration = Duration::from_secs(10);
    /// Preserve reply time after a mutation start deadline expires.
    const MUTATION_REPLY_RESERVE: Duration = Duration::from_millis(250);

    /// Daemon lifecycle waits are explicit and finite. Store-open work runs
    /// inside this deadline; explicit warm close gets its own shorter budget.
    const DAEMON_OPEN_BOUND: Duration = Duration::from_secs(10);
    const DAEMON_WARM_CLOSE_BOUND: Duration = Duration::from_secs(5);
    const DAEMON_SHUTDOWN_BOUND: Duration = Duration::from_secs(16);

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

    /// Receipt timing is captured before a request enters the single dispatcher
    /// queue. Queue delay is then subtracted with the daemon's monotonic clock,
    /// so a wall-clock rollback after receipt cannot extend mutation work.
    #[derive(Clone, Copy)]
    struct RequestReceipt {
        monotonic: Instant,
        unix_ms: u64,
    }

    impl RequestReceipt {
        fn now() -> Self {
            Self {
                monotonic: Instant::now(),
                unix_ms: unix_now_ms(),
            }
        }
    }

    /// One parsed request plus receipt timing and its oneshot reply channel.
    struct Job {
        request: StoreRequest,
        reply: Sender<StoreResponse>,
        receipt: RequestReceipt,
    }

    fn unix_now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

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
        if line.len() > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "serialized daemon reply is {} bytes, exceeding protocol maximum {MAX_LINE_BYTES}",
                    line.len()
                ),
            ));
        }
        stream.write_all(line.as_bytes())?;
        stream.flush()
    }

    /// A nonblocking listener may yield an accepted stream with nonblocking
    /// status on some Unix platforms. Per-connection handling uses blocking
    /// framed I/O with explicit timeouts; normalize the accepted fd before any
    /// read or write so a reply larger than the socket buffer cannot terminate
    /// at the first `WouldBlock` boundary.
    fn prepare_accepted_stream(stream: &UnixStream) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(CONN_TIMEOUT))?;
        stream.set_write_timeout(Some(CONN_TIMEOUT))
    }

    /// SEC-003: answer an over-cap / un-spawnable connection immediately with a
    /// retryable transport error, then close. The client's fresh-connection-per-
    /// op path maps this to R6's "retry", so a momentary reader-thread saturation
    /// sheds load gracefully instead of the accept loop dying on a `spawn` panic.
    fn respond_busy(stream: &UnixStream) {
        if let Err(e) = prepare_accepted_stream(stream) {
            log(&format!("configure busy response stream failed: {e}"));
            return;
        }
        let resp = StoreResponse::Err(StoreError::transport("daemon busy; retry"));
        if let Err(e) = write_response(stream, &resp) {
            log(&format!("write busy response failed: {e}"));
        }
    }

    /// Per-connection reader thread: read one request line, route it through the
    /// dispatcher, write one reply line, close. Any framing/parse failure yields
    /// a structured error reply (never a panic — falsifier B).
    fn handle_conn(
        stream: UnixStream,
        job_tx: Sender<Job>,
        rally_dir: &Path,
    ) -> std::io::Result<()> {
        prepare_accepted_stream(&stream)?;

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
                            let job = Job {
                                request: req,
                                reply: reply_tx,
                                receipt: RequestReceipt::now(),
                            };
                            if job_tx.send(job).is_err() {
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
        if store::trigger_o26_fault(rally_dir, store::O26FaultPoint::DaemonReplyDrop).is_err() {
            return Ok(());
        }
        write_response(&stream, &response)
    }

    /// Map an internal [`RallyError`] onto the frozen wire [`StoreError`] with
    /// exit-code parity (G8). `Io`/`Json` collapse to `Internal` (source dropped
    /// over the wire).
    fn rally_to_wire(err: RallyError) -> StoreError {
        match err {
            RallyError::OutcomeUnknown {
                event_id,
                phase,
                detail,
            } => StoreError::outcome_unknown(event_id, phase, detail),
            other => {
                let message = other.to_string();
                let kind = match other {
                    RallyError::Usage(_) => StoreErrorKind::Usage,
                    RallyError::NotFound(_) => StoreErrorKind::NotFound,
                    RallyError::Command(_) => StoreErrorKind::Command,
                    RallyError::Message(_) => StoreErrorKind::Message,
                    RallyError::NotStarted(_) => StoreErrorKind::NotStarted,
                    RallyError::IncompatibleWire { .. } => StoreErrorKind::IncompatibleWire,
                    RallyError::Io { .. } | RallyError::Json { .. } => StoreErrorKind::Internal,
                    RallyError::OutcomeUnknown { .. } => unreachable!("handled above"),
                };
                StoreError::new(kind, message)
            }
        }
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
    #[cfg(test)]
    fn dispatch_one(
        store: &mut DirectRoomStore,
        repo_root: &str,
        req: StoreRequest,
    ) -> StoreResponse {
        dispatch_one_received(store, repo_root, req, RequestReceipt::now())
    }

    fn dispatch_one_received(
        store: &mut DirectRoomStore,
        repo_root: &str,
        req: StoreRequest,
        receipt: RequestReceipt,
    ) -> StoreResponse {
        dispatch_one_received_with_hook(store, repo_root, req, receipt, || {})
    }

    fn dispatch_one_received_with_hook<F>(
        store: &mut DirectRoomStore,
        repo_root: &str,
        req: StoreRequest,
        receipt: RequestReceipt,
        before_deadline_install: F,
    ) -> StoreResponse
    where
        F: FnOnce(),
    {
        if req.wire_version != WIRE_VERSION {
            return StoreResponse::Err(StoreError::new(
                StoreErrorKind::IncompatibleWire,
                format!(
                    "wire_version mismatch: daemon speaks {WIRE_VERSION}, client sent {}; \
                 stop the incompatible daemon with `rally daemon stop` before retrying",
                    req.wire_version
                ),
            ));
        }
        if matches!(req.op, StoreOp::Ping) {
            return StoreResponse::Ok(StoreOk::Pong {
                repo_root: repo_root.to_string(),
                pid: std::process::id(),
                wire_version: WIRE_VERSION,
            });
        }
        if matches!(req.op, StoreOp::SnapshotScoped { .. }) && req.engagement.is_none() {
            return StoreResponse::Err(StoreError::new(
                StoreErrorKind::Usage,
                "snapshot_scoped requires StoreRequest.engagement",
            ));
        }
        // Per-request engagement rebind (L9/R4): safe because the dispatcher is
        // single-threaded. The daemon NEVER consults its own process env here.
        let request_engagement = req.engagement;
        let request_deadline = req.deadline_unix_ms;
        let request_budget_ms = req.mutation_budget_ms;
        let op = req.op;
        store.set_engagement_scope(request_engagement.clone());
        let result = if op.is_mutating() {
            let dispatch_monotonic = Instant::now();
            let dispatch_unix_ms = unix_now_ms();
            let deadline = match bounded_mutation_deadline(
                request_deadline,
                request_budget_ms,
                receipt,
                dispatch_unix_ms,
                dispatch_monotonic,
            ) {
                None => {
                    return StoreResponse::Err(StoreError::new(
                        StoreErrorKind::NotStarted,
                        "mutation-not-started: client deadline elapsed before daemon dispatch; no durable mutation started and retry is safe",
                    ));
                }
                Some(deadline) => deadline,
            };
            before_deadline_install();
            store::with_mutation_deadline_at(deadline, || {
                run_op(store, op, request_engagement.as_deref())
            })
        } else {
            run_op(store, op, request_engagement.as_deref())
        };
        match result {
            Ok(ok) => StoreResponse::Ok(ok),
            Err(e) => StoreResponse::Err(e),
        }
    }

    /// Translate untrusted dual wire timing into one anchored monotonic deadline.
    /// The absolute deadline consumes normal connect/read delay. The relative
    /// budget caps rollback before receipt. Anchoring the result to the receipt
    /// `Instant` consumes dispatcher queueing and preemption without ever
    /// subtracting elapsed time and then adding the remainder to a newer clock.
    /// A rollback that occurs entirely before receipt cannot be measured across
    /// processes; taking the relative minimum bounds that residual to the
    /// client's original budget instead of the 64-bit absolute delta.
    fn bounded_mutation_deadline(
        deadline_unix_ms: Option<u64>,
        mutation_budget_ms: Option<u64>,
        receipt: RequestReceipt,
        dispatch_unix_ms: u64,
        dispatch_monotonic: Instant,
    ) -> Option<Instant> {
        let cap = CONN_TIMEOUT.saturating_sub(MUTATION_REPLY_RESERVE);
        let requested = mutation_budget_ms
            .map(Duration::from_millis)
            .unwrap_or(cap)
            .min(cap);
        if requested.is_zero() {
            return None;
        }
        let absolute_remaining = |now_unix_ms| match deadline_unix_ms {
            Some(deadline) if deadline <= now_unix_ms => None,
            Some(deadline) => {
                Some(Duration::from_millis(deadline.saturating_sub(now_unix_ms)).min(cap))
            }
            None => Some(cap),
        };
        let at_receipt = requested.min(absolute_remaining(receipt.unix_ms)?);
        let receipt_deadline = receipt.monotonic.checked_add(at_receipt)?;
        let dispatch_deadline =
            dispatch_monotonic.checked_add(absolute_remaining(dispatch_unix_ms)?)?;
        let deadline = receipt_deadline.min(dispatch_deadline);
        if deadline <= dispatch_monotonic {
            None
        } else {
            Some(deadline)
        }
    }

    struct StoreOpenFailure {
        error: RallyError,
        retain_owner_until_exit: bool,
    }

    /// Open and warm the daemon-owned store on an owned worker. The caller waits
    /// only until the absolute total deadline; a timed-out worker is detached,
    /// and `serve_unix` retains owner EX until process exit before returning the
    /// loud failure.
    fn open_direct_store_bounded(
        repo_root: PathBuf,
        budget: Duration,
    ) -> Result<DirectRoomStore, StoreOpenFailure> {
        open_direct_store_bounded_with(repo_root, budget, || {})
    }

    fn open_direct_store_bounded_with<F>(
        repo_root: PathBuf,
        budget: Duration,
        after_open: F,
    ) -> Result<DirectRoomStore, StoreOpenFailure>
    where
        F: FnOnce() + Send + 'static,
    {
        let budget = budget.min(DAEMON_OPEN_BOUND);
        let deadline = Instant::now() + budget;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("rally-daemon-store-open".to_string())
            .spawn(move || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let reply_reserve = MUTATION_REPLY_RESERVE.min(remaining / 4);
                let lock_deadline = deadline.checked_sub(reply_reserve).unwrap_or(deadline);
                let result = store::with_mutation_deadline_at(lock_deadline, || {
                    let mut store = DirectRoomStore::open_direct_at(repo_root)?;
                    after_open();
                    store.install_warm_fact_store()?;
                    Ok(store)
                });
                let _ = result_tx.send(result);
            })
            .map_err(|error| StoreOpenFailure {
                error: RallyError::Command(format!(
                    "daemon-open-not-started: could not spawn store-open worker: {error}"
                )),
                retain_owner_until_exit: false,
            })?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = match result_rx.recv_timeout(remaining) {
            Ok(result) => result.map_err(|error| StoreOpenFailure {
                error,
                retain_owner_until_exit: false,
            }),
            Err(RecvTimeoutError::Timeout) => Err(StoreOpenFailure {
                error: RallyError::Command(format!(
                    "daemon-open-timeout: store open did not finish within {}ms; startup worker remains isolated under owner lock until process exit",
                    budget.as_millis()
                )),
                retain_owner_until_exit: true,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(StoreOpenFailure {
                error: RallyError::Command(
                    "daemon-open-failed: store-open worker exited without a result; owner lock retained until process exit"
                        .to_string(),
                ),
                retain_owner_until_exit: true,
            }),
        };
        drop(worker);
        result
    }

    fn retain_owner_after_open_failure(
        owner: &mut Option<store::OwnerGuard>,
        failure: &StoreOpenFailure,
    ) {
        if failure.retain_owner_until_exit
            && let Some(owner) = owner.take()
        {
            std::mem::forget(owner);
        }
    }

    /// Wait for explicit dispatcher/store close without ever joining past the
    /// deadline. On failure, deliberately retain daemon owner EX until process
    /// exit: the detached dispatcher owns every value it can still access, and
    /// no second daemon or direct owner may enter while it finishes or remains
    /// wedged.
    fn await_dispatcher_close(
        close_rx: Receiver<Result<(), String>>,
        dispatcher: thread::JoinHandle<()>,
        owner: store::OwnerGuard,
        bound: Duration,
    ) -> Result<store::OwnerGuard, ServeError> {
        let close_result = match close_rx.recv_timeout(bound) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "daemon-close-timeout: dispatcher did not close within {}ms",
                bound.as_millis()
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("daemon-close-failed: dispatcher exited without close result".to_string())
            }
        };
        // The completion channel is the bounded contract. Dropping a live
        // JoinHandle detaches; the move closure owns its store and paths.
        drop(dispatcher);
        match close_result {
            Ok(()) => Ok(owner),
            Err(error) => {
                // This is a deliberate failure-path guard leak. The OS releases
                // the fd when the daemon process exits; until then, fail closed.
                std::mem::forget(owner);
                Err(ServeError::new(error))
            }
        }
    }

    fn run_op(
        store: &mut DirectRoomStore,
        op: StoreOp,
        request_engagement: Option<&str>,
    ) -> Result<StoreOk, StoreError> {
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
                    outcome: to_wire_value(&out)?,
                }
            }
            StoreOp::AppendFactVerified { fact } => {
                let f = fact_from_value(fact)?;
                let out = store.append_fact_verified(&f).map_err(rally_to_wire)?;
                StoreOk::AppendFactVerified {
                    outcome: to_wire_value(&out)?,
                }
            }
            StoreOp::AppendStateTransitionVerified { fact } => {
                let f = fact_from_value(fact)?;
                let out = store
                    .append_state_transition_verified(&f)
                    .map_err(rally_to_wire)?;
                StoreOk::AppendStateTransitionVerified {
                    outcome: to_wire_value(&out)?,
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
                StoreOk::AppendSessionFactIfContext {
                    result: to_wire_value(&out)?,
                }
            }
            StoreOp::Facts => {
                let facts = store.facts().map_err(rally_to_wire)?;
                StoreOk::Facts {
                    facts: to_wire_values(&facts)?,
                }
            }
            StoreOp::RepoWideClaimLifecycleFacts => {
                let facts = store
                    .repo_wide_claim_lifecycle_facts()
                    .map_err(rally_to_wire)?;
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
                event_id,
                thread_id,
                created_at,
                caller_tool,
                caller_session_id,
                expected_owner_session_id,
            } => {
                let record = store
                    .renew_claim_lease(
                        &claim_id,
                        lease_expires_at,
                        caller_tool.as_deref(),
                        caller_session_id.as_deref(),
                        expected_owner_session_id.as_deref(),
                        &event_id,
                        &thread_id,
                        &created_at,
                    )
                    .map_err(rally_to_wire)?;
                StoreOk::RenewClaimLease {
                    outcome: to_wire_value(&record)?,
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
                let capture = store
                    .snapshot_cache_capture(include_archived)
                    .map_err(rally_to_wire)?;
                let snapshot = capture.snapshot.without_obligation_transport();
                StoreOk::Snapshot {
                    snapshot: snapshot_to_wire(&snapshot)?,
                    fingerprint: capture
                        .fingerprint
                        .as_ref()
                        .map(to_wire_value)
                        .transpose()?,
                }
            }
            StoreOp::SnapshotForObligationTarget {
                tool,
                row_limit,
                include_archived,
            } => {
                let snapshot = store
                    .snapshot_with_archived(include_archived)
                    .map_err(rally_to_wire)?
                    .for_obligation_target(&tool, row_limit);
                StoreOk::Snapshot {
                    snapshot: snapshot_to_wire(&snapshot)?,
                    fingerprint: None,
                }
            }
            StoreOp::SnapshotScoped {
                run_id,
                path,
                include_archived,
                include_presence_only,
            } => {
                let engagement = request_engagement.ok_or_else(|| {
                    StoreError::new(
                        StoreErrorKind::Usage,
                        "snapshot_scoped requires StoreRequest.engagement",
                    )
                })?;
                let snap = store
                    .snapshot_scoped(
                        engagement,
                        run_id.as_deref(),
                        path.as_deref(),
                        include_archived,
                        include_presence_only,
                    )
                    .map_err(rally_to_wire)?
                    .without_obligation_transport();
                StoreOk::Snapshot {
                    snapshot: snapshot_to_wire(&snap)?,
                    fingerprint: None,
                }
            }
            StoreOp::SnapshotWithReadersArchived { include_archived } => {
                let snap = store
                    .snapshot_with_readers_archived(include_archived)
                    .map_err(rally_to_wire)?
                    .without_obligation_transport();
                StoreOk::SnapshotWithReaders {
                    snapshot: snapshot_to_wire(&snap)?,
                }
            }
            StoreOp::LastCheckpointSeq { tool } => {
                let seq = store.last_checkpoint_seq(&tool).map_err(rally_to_wire)?;
                StoreOk::LastCheckpointSeq { seq }
            }
            StoreOp::MaybeAppendReadCheckpoint { fact, read_seq } => {
                let fact = fact_from_value(fact)?;
                let out = store
                    .maybe_append_read_checkpoint(&fact, read_seq)
                    .map_err(rally_to_wire)?;
                StoreOk::MaybeAppendReadCheckpoint {
                    result: to_wire_value(&out)?,
                }
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
                    if let Err(e) = prepare_accepted_stream(&stream) {
                        log(&format!("configure store-open error stream failed: {e}"));
                        continue;
                    }
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1));
                    let _ = stream.set_write_timeout(Some(remaining));
                    // Reply immediately. Waiting to consume a stalled request
                    // would let one client outlive the bounded drain window.
                    if let Err(e) = write_response(&stream, &resp) {
                        log(&format!("write store-open error response failed: {e}"));
                    }
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
        // RC-072: a daemon cold-starting into a repo nobody ran `rally init`
        // in is the other way `.rally/` first appears. Same best-effort
        // contract as the direct-open path in `store.rs`.
        crate::init::ensure_ignore_present(&rally_dir);

        // (1) Owner lock EXCLUSIVE, blocking (ADR-01/L1). Held for the whole
        // serving lifetime; the kernel releases it on any death.
        let t0 = Instant::now();
        log(&format!(
            "acquiring exclusive owner lock at {}",
            rally_dir.display()
        ));
        let mut owner = Some(
            store::acquire_owner_exclusive_bounded(&rally_dir, DAEMON_OPEN_BOUND)
                .map_err(|e| ServeError::new(format!("acquire owner EX lock: {e}")))?,
        );
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
        let store = match open_direct_store_bounded(repo_root.clone(), DAEMON_OPEN_BOUND) {
            Ok(s) => s,
            Err(failure) => {
                retain_owner_after_open_failure(&mut owner, &failure);
                let error = failure.error;
                let err = StoreError::new(
                    StoreErrorKind::Internal,
                    format!("daemon store open failed: {error}"),
                );
                log(&format!(
                    "store open failed: {error}; draining pending clients"
                ));
                respond_all_with_error(&listener, &err, STORE_OPEN_FAIL_DRAIN);
                cleanup(&socket_path, &addr_path, &pid_path);
                return Err(ServeError::new(format!("store open failed: {error}")));
            }
        };
        let owner = owner.take().ok_or_else(|| {
            ServeError::new("daemon owner lock missing after successful store open")
        })?;

        let canonical_root = Arc::new(canonical_repo_root(&repo_root));

        // (4) ONE dispatcher thread owns the store => total order by construction.
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let last_activity = Arc::new(AtomicI64::new(now_millis()));
        let disp_root = canonical_root.clone();
        let disp_activity = last_activity.clone();
        let (close_tx, close_rx) = mpsc::sync_channel(1);
        let dispatcher = thread::spawn(move || {
            let mut store = store;
            while let Ok(job) = job_rx.recv() {
                let resp =
                    dispatch_one_received(&mut store, disp_root.as_str(), job.request, job.receipt);
                disp_activity.store(now_millis(), Ordering::Relaxed);
                let _ = job.reply.send(resp);
            }
            // Channel closed (all senders dropped): explicitly close the warm
            // pool under mutation.lock. The caller bounds its wait on
            // `close_rx`; DirectRoomStore::Drop is only a prompt fallback.
            let close_result = store
                .close_warm_fact_store_bounded(DAEMON_WARM_CLOSE_BOUND)
                .map_err(|error| error.to_string());
            let _ = close_tx.send(close_result);
            drop(store);
        });

        // Nonblocking accept loop: poll the shutdown flag + idle window ~every
        // ACCEPT_POLL; on each wake DRAIN ALL pending connections (accept until
        // WouldBlock), spawning one reader thread per connection, THEN sleep.
        // Draining all-per-wake — not one-per-wake — is what makes the accept
        // side keep up with a burst: the kernel backlog holds connections that
        // arrive between wakes and this inner loop empties it fully each time,
        // so clients no longer see ECONNREFUSED (which the fresh-connect client
        // path surfaces as R6's routed transport failure). The
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
                        let connection_rally_dir = rally_dir.clone();
                        // Keep a fallback handle so a `spawn` FAILURE can still
                        // answer with a structured error instead of panicking.
                        let busy_fallback = stream.try_clone().ok();
                        match thread::Builder::new().spawn(move || {
                            if let Err(e) = handle_conn(stream, jt, &connection_rally_dir) {
                                log(&format!("serve connection failed: {e}"));
                            }
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

        // Lifecycle: drop the accept-loop sender so existing bounded reader
        // threads finish and the dispatcher explicitly closes its warm store.
        // Never join without a deadline: if close misses the bound, detach the
        // thread, report failure, and let its mutation-lock guard protect any
        // delayed WAL cleanup. Owner EX is released only after a successful
        // close; a failed/expired close deliberately retains it until process
        // exit so a detached dispatcher cannot overlap a replacement owner.
        drop(job_tx);
        let owner = await_dispatcher_close(close_rx, dispatcher, owner, DAEMON_SHUTDOWN_BOUND);
        cleanup(&socket_path, &addr_path, &pid_path);
        let _owner = owner?;
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

        fn facts_response_with_line_len(line_len: usize) -> StoreResponse {
            let empty = StoreResponse::Ok(StoreOk::Facts {
                facts: vec![Value::String(String::new())],
            });
            let base_len = serde_json::to_string(&empty).unwrap().len();
            assert!(line_len > base_len + 1);
            let response = StoreResponse::Ok(StoreOk::Facts {
                facts: vec![Value::String("x".repeat(line_len - base_len - 1))],
            });
            assert_eq!(
                serde_json::to_string(&response).unwrap().len() + 1,
                line_len,
                "fixture must include exactly one trailing newline in its wire size"
            );
            response
        }

        fn assert_real_socket_response_complete(line_len: usize) {
            let socket = std::env::temp_dir().join(format!(
                "rallyd-large-reply-{}-{line_len}.sock",
                now_millis()
            ));
            std::fs::remove_file(&socket).ok();
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let response = facts_response_with_line_len(line_len);
            let expected = response.clone();
            let server = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            prepare_accepted_stream(&stream).unwrap();
                            write_response(&stream, &response).unwrap();
                            return;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "client never connected");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("accept large-reply client: {e}"),
                    }
                }
            });

            let stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.len(), line_len, "reply was partial or truncated");
            assert!(line.ends_with('\n'), "reply frame is missing its newline");
            let actual: StoreResponse = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            server.join().unwrap();
            std::fs::remove_file(&socket).ok();
        }

        #[test]
        fn real_socket_replies_above_buffer_and_near_protocol_max_are_complete() {
            // The dogfood failure cut every reply at exactly 8 KiB. Exercise a
            // comfortably larger frame and the largest legal framed reply.
            assert_real_socket_response_complete(64 * 1024);
            assert_real_socket_response_complete(MAX_LINE_BYTES);
        }

        #[test]
        fn response_write_reports_a_disconnected_peer() {
            let (server, peer) = UnixStream::pair().unwrap();
            drop(peer);
            let response = facts_response_with_line_len(64 * 1024);
            let err = write_response(&server, &response).unwrap_err();
            assert!(
                matches!(
                    err.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ),
                "disconnect must be observable, got: {err}"
            );
        }

        #[test]
        fn daemon_rejects_v5_requests_after_targeted_read_cutover() {
            assert_eq!(WIRE_VERSION, 6, "this control grades the v5 to v6 cutover");
            let repo_root = unique_repo_root("reject-v5");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let request = StoreRequest {
                wire_version: 5,
                engagement: None,
                deadline_unix_ms: None,
                mutation_budget_ms: None,
                op: StoreOp::Ping,
            };
            match dispatch_one(&mut store, "/expected/repo", request) {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::IncompatibleWire);
                    assert!(error.message.contains("daemon speaks 6"));
                    assert!(error.message.contains("client sent 5"));
                }
                other => panic!("v5 request was not rejected: {other:?}"),
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_routed_snapshot_carries_server_captured_fingerprint() {
            let repo_root = unique_repo_root("o26-routed-snapshot-capture");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let fact = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o26-routed-snapshot-fact".to_string(),
                seq: 0,
                thread_id: "o26-routed-snapshot-thread".to_string(),
                kind: crate::store::FactKind::Decision,
                tool: Some("codex:test".to_string()),
                role: None,
                subject: "server-anchored snapshot capture".to_string(),
                scope: Vec::new(),
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact(&fact).unwrap();

            let response = dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                StoreRequest::new(
                    None,
                    StoreOp::SnapshotWithArchived {
                        include_archived: false,
                    },
                ),
            );
            match response {
                StoreResponse::Ok(StoreOk::Snapshot {
                    snapshot,
                    fingerprint: Some(fingerprint),
                }) => {
                    let projection_unix_sec = fingerprint["projection_unix_sec"]
                        .as_i64()
                        .expect("routed fingerprint must carry its projection epoch");
                    let direct = store
                        .test_snapshot_cache_capture_at(false, projection_unix_sec)
                        .unwrap();
                    assert_eq!(
                        snapshot,
                        crate::store::snapshot_to_wire_value(&direct.snapshot).unwrap()
                    );
                    assert_eq!(
                        fingerprint,
                        serde_json::to_value(direct.fingerprint.as_ref().unwrap()).unwrap()
                    );
                }
                other => panic!("unexpected routed snapshot capture: {other:?}"),
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn routed_repo_wide_claim_lifecycle_query_crosses_engagements() {
            let repo_root = unique_repo_root("repo-wide-claim-lifecycle");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            for (engagement, event_id, path) in [
                ("engagement-alpha", "claim-alpha", "src/alpha.rs"),
                ("engagement-beta", "claim-beta", "src/beta.rs"),
            ] {
                store.set_engagement_scope(Some(engagement.to_string()));
                store
                    .append_fact(&crate::store::Fact {
                        schema: crate::FACT_SCHEMA.to_string(),
                        event_id: event_id.to_string(),
                        thread_id: format!("thread-{event_id}"),
                        kind: crate::store::FactKind::Claim,
                        tool: Some("codex:session-owner".to_string()),
                        subject: format!("own {path}"),
                        scope: vec![format!("file:{path}")],
                        created_at: crate::now_string(),
                        ..crate::store::Fact::default()
                    })
                    .unwrap();
            }

            let response = dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                StoreRequest::new(
                    Some("engagement-gamma".to_string()),
                    StoreOp::RepoWideClaimLifecycleFacts,
                ),
            );
            match response {
                StoreResponse::Ok(StoreOk::Facts { facts }) => {
                    let ids = facts
                        .iter()
                        .filter_map(|fact| fact["event_id"].as_str())
                        .collect::<std::collections::BTreeSet<_>>();
                    assert_eq!(
                        ids,
                        std::collections::BTreeSet::from(["claim-alpha", "claim-beta"])
                    );
                }
                other => panic!("unexpected repository-wide lifecycle reply: {other:?}"),
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn raw_routed_writes_cannot_extend_a_closed_session_history() {
            let repo_root = unique_repo_root("closed-session-raw-wire");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let token = "close-token";
            let presence = crate::store::Fact {
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "session-presence".to_string(),
                thread_id: "session-thread".to_string(),
                kind: crate::store::FactKind::Presence,
                tool: Some("codex:closed".to_string()),
                from_session_id: Some("sess:closed".to_string()),
                subject: "session active".to_string(),
                evidence: vec![
                    "protocol:session_state=active".to_string(),
                    crate::session_identity::session_close_token_hash_marker(token),
                ],
                created_at: crate::now_string(),
                ..crate::store::Fact::default()
            };
            store.append_fact(&presence).unwrap();
            let close = crate::store::Fact {
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "session-close".to_string(),
                thread_id: "session-thread".to_string(),
                kind: crate::store::FactKind::SessionClosed,
                tool: presence.tool.clone(),
                from_session_id: presence.from_session_id.clone(),
                subject: "session closed".to_string(),
                evidence: vec![
                    "protocol:session_state=closed".to_string(),
                    format!(
                        "{}{}",
                        crate::session_identity::SESSION_CLOSE_TOKEN_REVEAL_PREFIX,
                        token
                    ),
                ],
                created_at: crate::now_string(),
                ..crate::store::Fact::default()
            };
            store.append_fact_verified(&close).unwrap();

            for mut forged in [
                crate::store::Fact {
                    event_id: "duplicate-session-close".to_string(),
                    ..close.clone()
                },
                crate::store::Fact {
                    event_id: "post-close-artifact".to_string(),
                    kind: crate::store::FactKind::Artifact,
                    subject: "must not append".to_string(),
                    evidence: Vec::new(),
                    ..close.clone()
                },
            ] {
                forged.seq = 0;
                let response = dispatch_one(
                    &mut store,
                    canonical_repo_root(&repo_root).as_str(),
                    StoreRequest::new(
                        None,
                        StoreOp::AppendFactVerified {
                            fact: serde_json::to_value(forged).unwrap(),
                        },
                    ),
                );
                assert!(
                    matches!(response, StoreResponse::Err(ref error) if error.kind == StoreErrorKind::Usage && error.message.contains("is closed")),
                    "closed-session raw write was not refused: {response:?}"
                );
            }

            assert_eq!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .filter(|fact| fact.kind == crate::store::FactKind::SessionClosed)
                    .count(),
                1
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn routed_snapshots_survive_more_than_one_thousand_distinct_obligations() {
            let repo_root = unique_repo_root("obligation-flood-wire");
            let engagement = "engagement-flood";
            let log_dir = repo_root.join(".rally").join(crate::store::LOG_DIRNAME);
            std::fs::create_dir_all(&log_dir).unwrap();
            let mut segment =
                std::fs::File::create(log_dir.join(format!("{engagement}.jsonl"))).unwrap();
            for seq in 1..=1025i64 {
                let fact = crate::store::Fact {
                    schema: crate::FACT_SCHEMA.to_string(),
                    event_id: format!("obligation-{seq}"),
                    seq,
                    thread_id: format!("thread-{seq}"),
                    kind: crate::store::FactKind::Handoff,
                    tool: Some("claude_code:sender".to_string()),
                    subject: format!("obligation {seq}"),
                    target: Some("codex:receiver".to_string()),
                    created_at: crate::now_string(),
                    ..crate::store::Fact::default()
                };
                writeln!(
                    segment,
                    "{}",
                    serde_json::json!({
                        "seq": seq,
                        "occurred_at": fact.created_at,
                        "event_type": "handoff",
                        "payload": fact,
                        "engagement": engagement
                    })
                )
                .unwrap();
            }
            segment.sync_all().unwrap();

            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let normal = dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                StoreRequest::new(
                    Some(engagement.to_string()),
                    StoreOp::SnapshotWithArchived {
                        include_archived: false,
                    },
                ),
            );
            assert!(
                matches!(normal, StoreResponse::Ok(StoreOk::Snapshot { .. })),
                "ordinary routed snapshot failed under obligation flood: {normal:?}"
            );

            let targeted = dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                StoreRequest::new(
                    Some(engagement.to_string()),
                    StoreOp::SnapshotForObligationTarget {
                        tool: "codex:receiver".to_string(),
                        row_limit: 32,
                        include_archived: false,
                    },
                ),
            );
            match targeted {
                StoreResponse::Ok(StoreOk::Snapshot { snapshot, .. }) => {
                    let restored = crate::store::snapshot_from_wire_value(snapshot).unwrap();
                    assert_eq!(restored.open_obligations_total, 1025);
                    assert_eq!(restored.open_obligations.len(), 32);
                    assert_eq!(restored.open_obligations_omitted, 993);
                }
                other => {
                    panic!("targeted routed snapshot failed under obligation flood: {other:?}")
                }
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_fresh_daemon_repairs_canonical_ahead_db_before_warm_install() {
            let repo_root = unique_repo_root("o26-fresh-daemon-repair");
            let rally_dir = repo_root.join(".rally");
            let seed = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let fact = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o26-fresh-daemon-canonical".to_string(),
                seq: 0,
                thread_id: "o26-fresh-daemon-thread".to_string(),
                kind: crate::store::FactKind::Decision,
                tool: Some("codex:test".to_string()),
                role: None,
                subject: "canonical commit before daemon restart".to_string(),
                scope: Vec::new(),
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            crate::store::fail_o26_once(&rally_dir, crate::store::O26FaultPoint::FactsDbProjection);
            let degraded = seed.append_fact(&fact).unwrap();
            assert!(degraded.committed);
            assert!(!degraded.projection_complete);
            drop(seed);

            let mut daemon =
                match open_direct_store_bounded(repo_root.clone(), Duration::from_secs(5)) {
                    Ok(store) => store,
                    Err(failure) => panic!("fresh daemon open failed: {}", failure.error),
                };
            let response = dispatch_one(
                &mut daemon,
                canonical_repo_root(&repo_root).as_str(),
                StoreRequest::new(
                    None,
                    StoreOp::SnapshotWithArchived {
                        include_archived: false,
                    },
                ),
            );
            let snapshot = match response {
                StoreResponse::Ok(StoreOk::Snapshot {
                    snapshot,
                    fingerprint: Some(_),
                }) => crate::store::snapshot_from_wire_value(snapshot).unwrap(),
                other => panic!("unexpected fresh-daemon snapshot reply: {other:?}"),
            };
            assert!(
                snapshot
                    .current_decisions
                    .iter()
                    .any(|candidate| candidate.event_id == fact.event_id)
            );
            assert_eq!(
                daemon
                    .facts()
                    .unwrap()
                    .iter()
                    .filter(|candidate| candidate.event_id == fact.event_id)
                    .count(),
                1,
                "pre-warm reconcile must project the canonical fact exactly once"
            );
            daemon
                .close_warm_fact_store_bounded(Duration::from_secs(2))
                .unwrap();
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_routed_lost_reply_then_same_id_retry_appends_once() {
            let repo_root = unique_repo_root("o26-lost-reply");
            let rally_dir = repo_root.join(".rally");
            let socket = std::env::temp_dir().join(format!(
                "r26-lost-{}-{}.sock",
                std::process::id(),
                now_millis()
            ));
            let listener = UnixListener::bind(&socket).unwrap();
            let store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let canonical_root = canonical_repo_root(&repo_root);
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let dispatcher = thread::spawn(move || {
                let mut store = store;
                while let Ok(job) = job_rx.recv() {
                    let response = dispatch_one_received(
                        &mut store,
                        &canonical_root,
                        job.request,
                        job.receipt,
                    );
                    job.reply.send(response).unwrap();
                }
                store
            });
            let server_tx = job_tx.clone();
            let server_rally_dir = rally_dir.clone();
            let server = thread::spawn(move || {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    handle_conn(stream, server_tx.clone(), &server_rally_dir).unwrap();
                }
            });
            let routed = crate::store_client::RoutedRoomStore::for_test(
                socket.clone(),
                repo_root.clone(),
                "default",
            );
            let candidate = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o26-routed-lost-reply-event".to_string(),
                seq: 0,
                thread_id: "o26-routed-lost-reply-thread".to_string(),
                kind: crate::store::FactKind::Decision,
                tool: Some("test:01".to_string()),
                role: None,
                subject: "routed lost reply".to_string(),
                scope: vec!["file:src/routed.rs".to_string()],
                created_at: "2026-08-10T00:00:00Z".to_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            crate::store::fail_o26_once(&rally_dir, crate::store::O26FaultPoint::DaemonReplyDrop);

            let error = routed
                .append_fact(&candidate)
                .expect_err("dropped daemon reply must be outcome-unknown");
            assert!(matches!(
                error,
                RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                    if event_id == &candidate.event_id && phase == "daemon-transport"
            ));
            let retry = routed.append_fact(&candidate).unwrap();
            assert!(retry.committed);
            assert_eq!(retry.fact.seq, 1);

            server.join().unwrap();
            drop(job_tx);
            let store = dispatcher.join().unwrap();
            assert_eq!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .filter(|fact| fact.event_id == candidate.event_id)
                    .count(),
                1
            );
            drop(store);
            std::fs::remove_file(&socket).ok();
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_routed_renewal_and_checkpoint_retry_the_exact_lost_request_once() {
            let repo_root = unique_repo_root("o26-renew-checkpoint-lost-reply");
            let rally_dir = repo_root.join(".rally");
            let socket = std::env::temp_dir().join(format!(
                "r26-rc-{}-{}.sock",
                std::process::id(),
                now_millis()
            ));
            let listener = UnixListener::bind(&socket).unwrap();
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let claim = crate::store::Fact {
                from_session_id: Some("sess:o26".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o26-routed-renew-claim".to_string(),
                seq: 0,
                thread_id: "o26-routed-renew-claim-thread".to_string(),
                kind: crate::store::FactKind::Claim,
                tool: Some("test:renew".to_string()),
                role: None,
                subject: "claim for routed renewal retry".to_string(),
                scope: vec!["file:src/renew.rs".to_string()],
                created_at: "2026-08-10T00:00:00Z".to_string(),
                summary: None,
                evidence: vec!["lease_expires_at:2026-08-10T00:00:01Z".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact(&claim).unwrap();
            let canonical_root = canonical_repo_root(&repo_root);
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let dispatcher = thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let response = dispatch_one_received(
                        &mut store,
                        &canonical_root,
                        job.request,
                        job.receipt,
                    );
                    job.reply.send(response).unwrap();
                }
                store
            });
            let server_tx = job_tx.clone();
            let server_rally_dir = rally_dir.clone();
            let server = thread::spawn(move || {
                for _ in 0..4 {
                    let (stream, _) = listener.accept().unwrap();
                    handle_conn(stream, server_tx.clone(), &server_rally_dir).unwrap();
                }
            });
            let routed = crate::store_client::RoutedRoomStore::for_test(
                socket.clone(),
                repo_root.clone(),
                "default",
            );

            let renewal_event_id = "o26-routed-renew-request".to_string();
            let renewal_op = StoreOp::RenewClaimLease {
                claim_id: claim.event_id.clone(),
                lease_expires_at: "2099-01-01T00:00:00Z".to_string(),
                event_id: renewal_event_id.clone(),
                thread_id: "o26-routed-renew-request-thread".to_string(),
                created_at: "2026-08-10T00:00:02Z".to_string(),
                caller_tool: Some("test:renew".to_string()),
                caller_session_id: Some("sess:o26".to_string()),
                expected_owner_session_id: Some("sess:o26".to_string()),
            };
            crate::store::fail_o26_once(&rally_dir, crate::store::O26FaultPoint::DaemonReplyDrop);
            let renewal_error = routed
                .dispatch_for_test(renewal_op.clone())
                .expect_err("lost renewal reply must remain queryable");
            assert!(matches!(
                renewal_error,
                RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                    if event_id == &renewal_event_id && phase == "daemon-transport"
            ));
            let renewal_retry = routed.dispatch_for_test(renewal_op).unwrap();
            match renewal_retry {
                StoreOk::RenewClaimLease { outcome } => {
                    let outcome: crate::store::RenewClaimLeaseOutcome =
                        serde_json::from_value(outcome).unwrap();
                    assert_eq!(
                        outcome.append_outcome.unwrap().fact.event_id,
                        renewal_event_id
                    );
                }
                other => panic!("unexpected renewal retry reply: {other:?}"),
            }

            let checkpoint_event_id = "o26-routed-checkpoint-request".to_string();
            let checkpoint = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: checkpoint_event_id.clone(),
                seq: 0,
                thread_id: "read-test-renew".to_string(),
                kind: crate::store::FactKind::Read,
                tool: Some("test:reader".to_string()),
                role: None,
                subject: "read-checkpoint: test:reader at seq 2".to_string(),
                scope: Vec::new(),
                created_at: "2026-08-10T00:00:03Z".to_string(),
                summary: Some("read_seq:2".to_string()),
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            let checkpoint_op = StoreOp::MaybeAppendReadCheckpoint {
                fact: serde_json::to_value(&checkpoint).unwrap(),
                read_seq: 2,
            };
            crate::store::fail_o26_once(&rally_dir, crate::store::O26FaultPoint::DaemonReplyDrop);
            let checkpoint_error = routed
                .dispatch_for_test(checkpoint_op.clone())
                .expect_err("lost checkpoint reply must remain queryable");
            assert!(matches!(
                checkpoint_error,
                RallyError::OutcomeUnknown { ref event_id, ref phase, .. }
                    if event_id == &checkpoint_event_id && phase == "daemon-transport"
            ));
            let checkpoint_retry = routed.dispatch_for_test(checkpoint_op).unwrap();
            match checkpoint_retry {
                StoreOk::MaybeAppendReadCheckpoint { result } => {
                    let outcome: crate::store::ConditionalAppendOutcome =
                        serde_json::from_value(result).unwrap();
                    assert!(matches!(
                        outcome,
                        crate::store::ConditionalAppendOutcome::Applied(ref append)
                            if append.fact.event_id == checkpoint_event_id
                    ));
                }
                other => panic!("unexpected checkpoint retry reply: {other:?}"),
            }

            server.join().unwrap();
            drop(job_tx);
            let store = dispatcher.join().unwrap();
            let facts = store.facts().unwrap();
            for event_id in [&renewal_event_id, &checkpoint_event_id] {
                assert_eq!(
                    facts
                        .iter()
                        .filter(|fact| &fact.event_id == event_id)
                        .count(),
                    1,
                    "lost-reply retry must retain exactly one canonical {event_id}"
                );
            }
            drop(store);
            std::fs::remove_file(&socket).ok();
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_routed_release_and_resolve_projection_warnings_do_not_retry_append() {
            for (label, target_kind, close_kind) in [
                (
                    "release",
                    crate::store::FactKind::Claim,
                    crate::store::FactKind::Release,
                ),
                (
                    "resolve",
                    crate::store::FactKind::Blocker,
                    crate::store::FactKind::Resolve,
                ),
            ] {
                let repo_root = unique_repo_root(&format!("o26-routed-{label}"));
                let rally_dir = repo_root.join(".rally");
                let socket = std::env::temp_dir().join(format!(
                    "r26-{label}-{}-{}.sock",
                    std::process::id(),
                    now_millis()
                ));
                let listener = UnixListener::bind(&socket).unwrap();
                let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
                let target = crate::store::Fact {
                    from_session_id: None,
                    schema: crate::FACT_SCHEMA.to_string(),
                    event_id: format!("o26-routed-{label}-target"),
                    seq: 0,
                    thread_id: format!("o26-routed-{label}-target-thread"),
                    kind: target_kind,
                    tool: Some("test:01".to_string()),
                    role: None,
                    subject: format!("routed {label} target"),
                    scope: vec![format!("file:src/{label}.rs")],
                    created_at: "2026-08-10T00:00:00Z".to_string(),
                    summary: None,
                    evidence: Vec::new(),
                    target: None,
                    ref_id: None,
                    status: None,
                    severity: None,
                    uri: None,
                    session: None,
                };
                store.append_fact(&target).unwrap();
                let canonical_root = canonical_repo_root(&repo_root);
                let (job_tx, job_rx) = mpsc::channel::<Job>();
                let dispatcher = thread::spawn(move || {
                    while let Ok(job) = job_rx.recv() {
                        let response = dispatch_one_received(
                            &mut store,
                            &canonical_root,
                            job.request,
                            job.receipt,
                        );
                        job.reply.send(response).unwrap();
                    }
                    store
                });
                let server_tx = job_tx.clone();
                let server_rally_dir = rally_dir.clone();
                let server = thread::spawn(move || {
                    for _ in 0..2 {
                        let (stream, _) = listener.accept().unwrap();
                        handle_conn(stream, server_tx.clone(), &server_rally_dir).unwrap();
                    }
                });
                let routed = crate::store_client::RoutedRoomStore::for_test(
                    socket.clone(),
                    repo_root.clone(),
                    "default",
                );
                let close = crate::store::Fact {
                    from_session_id: None,
                    schema: crate::FACT_SCHEMA.to_string(),
                    event_id: format!("o26-routed-{label}-close"),
                    seq: 0,
                    thread_id: format!("o26-routed-{label}-close-thread"),
                    kind: close_kind,
                    tool: Some("test:01".to_string()),
                    role: None,
                    subject: format!("routed {label} close"),
                    scope: target.scope.clone(),
                    created_at: "2026-08-10T00:00:01Z".to_string(),
                    summary: None,
                    evidence: Vec::new(),
                    target: None,
                    ref_id: Some(target.event_id.clone()),
                    status: None,
                    severity: None,
                    uri: None,
                    session: None,
                };
                crate::store::fail_o26_once(
                    &rally_dir,
                    crate::store::O26FaultPoint::FactsDbProjection,
                );

                let degraded = routed.append_state_transition_verified(&close).unwrap();
                assert!(degraded.committed);
                assert!(!degraded.projection_complete);
                assert!(!degraded.warnings.is_empty());
                let retry = routed.append_state_transition_verified(&close).unwrap();
                assert_eq!(retry.fact.seq, degraded.fact.seq);

                server.join().unwrap();
                drop(job_tx);
                let store = dispatcher.join().unwrap();
                assert_eq!(
                    store
                        .facts()
                        .unwrap()
                        .iter()
                        .filter(|fact| fact.event_id == close.event_id)
                        .count(),
                    1
                );
                drop(store);
                std::fs::remove_file(&socket).ok();
                std::fs::remove_dir_all(repo_root).ok();
            }
        }

        #[test]
        fn daemon_store_open_honors_the_mutation_lock_deadline() {
            let repo_root = unique_repo_root("bounded-store-open");
            let rally_dir = repo_root.join(".rally");
            let seed = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            drop(seed);
            let held = store::acquire_room_mutation_lock(&rally_dir).unwrap();

            let started = Instant::now();
            let result = open_direct_store_bounded(repo_root.clone(), Duration::from_millis(75));
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_millis(300),
                "daemon store open escaped its 75ms bound: {elapsed:?}"
            );
            let failure = match result {
                Ok(_) => panic!("contended daemon store open unexpectedly succeeded"),
                Err(failure) => failure,
            };
            assert!(matches!(failure.error, RallyError::NotStarted(_)));
            assert!(!failure.retain_owner_until_exit);

            drop(held);
            let reopened = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            drop(reopened);
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn daemon_store_total_open_timeout_retains_owner_for_late_worker() {
            let repo_root = unique_repo_root("total-open-timeout-owner-retained");
            let rally_dir = repo_root.join(".rally");
            let mut owner = Some(
                store::acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(75))
                    .unwrap(),
            );

            let started = Instant::now();
            let result =
                open_direct_store_bounded_with(repo_root, Duration::from_millis(50), || {
                    thread::sleep(Duration::from_millis(250))
                });
            let elapsed = started.elapsed();
            let failure = match result {
                Ok(_) => panic!("delayed daemon store open unexpectedly completed"),
                Err(failure) => failure,
            };
            assert!(
                elapsed < Duration::from_millis(150),
                "total daemon store open escaped its 50ms bound: {elapsed:?}"
            );
            assert!(failure.retain_owner_until_exit);
            assert!(
                failure
                    .error
                    .to_string()
                    .starts_with("daemon-open-timeout:")
            );
            retain_owner_after_open_failure(&mut owner, &failure);
            assert!(owner.is_none());
            assert!(
                store::acquire_owner_shared_nb(&rally_dir)
                    .unwrap()
                    .is_none(),
                "direct ownership entered while timed-out open worker remained live"
            );
            assert!(
                matches!(
                    store::acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(25)),
                    Err(RallyError::NotStarted(_))
                ),
                "replacement daemon entered while timed-out open worker remained live"
            );
            // The worker and owner guard are intentionally process-lifetime on
            // timeout. Do not unlink the lock rendezvous in this test.
        }

        #[test]
        fn dispatcher_close_timeout_retains_owner_until_process_exit() {
            let repo_root = unique_repo_root("close-timeout-owner-retained");
            let rally_dir = repo_root.join(".rally");
            let owner =
                store::acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(75))
                    .unwrap();
            let (_close_tx, close_rx) = mpsc::sync_channel::<Result<(), String>>(1);
            let dispatcher = thread::spawn(|| thread::sleep(Duration::from_millis(100)));

            let started = Instant::now();
            let result =
                await_dispatcher_close(close_rx, dispatcher, owner, Duration::from_millis(25));
            let elapsed = started.elapsed();
            let error = match result {
                Ok(_) => panic!("dispatcher close unexpectedly completed"),
                Err(error) => error,
            };
            assert!(
                elapsed < Duration::from_millis(150),
                "dispatcher close wait escaped its 25ms bound: {elapsed:?}"
            );
            assert!(error.message().starts_with("daemon-close-timeout:"));
            assert!(
                store::acquire_owner_shared_nb(&rally_dir)
                    .unwrap()
                    .is_none(),
                "a direct owner entered after close timeout"
            );
            assert!(
                matches!(
                    store::acquire_owner_exclusive_bounded(&rally_dir, Duration::from_millis(25)),
                    Err(RallyError::NotStarted(_))
                ),
                "a replacement daemon entered after close timeout"
            );
            // `await_dispatcher_close` intentionally leaked owner EX. Do not
            // unlink the lock path: it remains the process-lifetime rendezvous.
        }

        #[test]
        fn raw_far_future_mutation_deadline_is_capped_without_panic() {
            let cap = CONN_TIMEOUT.saturating_sub(MUTATION_REPLY_RESERVE);
            let remaining = |deadline_unix_ms: Option<u64>,
                             mutation_budget_ms: Option<u64>,
                             receipt_unix_ms: u64,
                             dispatch_unix_ms: u64,
                             queue_elapsed: Duration| {
                let receipt_monotonic = Instant::now();
                let dispatch_monotonic = receipt_monotonic + queue_elapsed;
                bounded_mutation_deadline(
                    deadline_unix_ms,
                    mutation_budget_ms,
                    RequestReceipt {
                        monotonic: receipt_monotonic,
                        unix_ms: receipt_unix_ms,
                    },
                    dispatch_unix_ms,
                    dispatch_monotonic,
                )
                .map(|deadline| deadline.saturating_duration_since(dispatch_monotonic))
            };
            assert_eq!(
                remaining(Some(u64::MAX), Some(u64::MAX), 1, 1, Duration::ZERO),
                Some(cap)
            );
            assert_eq!(
                remaining(
                    Some(2_000),
                    Some(1_000),
                    1_000,
                    1_400,
                    Duration::from_millis(400)
                ),
                Some(Duration::from_millis(600))
            );
            assert_eq!(
                remaining(
                    Some(2_000),
                    Some(1_000),
                    1_000,
                    900,
                    Duration::from_millis(400)
                ),
                Some(Duration::from_millis(600)),
                "wall-clock rollback after receipt extended the monotonic budget"
            );
            assert_eq!(
                remaining(Some(2_000), Some(1_000), 900, 900, Duration::ZERO),
                Some(Duration::from_millis(1_000)),
                "pre-receipt rollback escaped the client-relative cap"
            );
            assert_eq!(
                remaining(
                    Some(2_000),
                    Some(1_000),
                    1_000,
                    2_100,
                    Duration::from_millis(100)
                ),
                None,
                "forward clock step did not expire the request"
            );
            assert_eq!(
                remaining(Some(1_000), Some(1_000), 1_000, 1_000, Duration::ZERO),
                None
            );
            assert_eq!(
                remaining(Some(2_000), Some(0), 1_000, 1_000, Duration::ZERO),
                None
            );

            let repo_root = unique_repo_root("far-future-mutation-deadline");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let mut request = StoreRequest::new(None, StoreOp::RebuildClaimIndex);
            request.deadline_unix_ms = Some(u64::MAX);
            request.mutation_budget_ms = Some(u64::MAX);
            let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch_one(&mut store, repo_root.to_string_lossy().as_ref(), request)
            }));
            assert!(
                matches!(response, Ok(StoreResponse::Ok(StoreOk::RebuildClaimIndex))),
                "far-future raw deadline panicked or failed: {response:?}"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn expired_routed_mutation_is_typed_not_started_and_never_appends() {
            let repo_root = unique_repo_root("expired-routed-mutation");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let fact = crate::store::Fact {
                from_session_id: Some("sess:o25".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o25-routed-no-late-commit".to_string(),
                seq: 0,
                thread_id: "thread-o25-routed".to_string(),
                kind: crate::store::FactKind::Artifact,
                tool: Some("codex:o25".to_string()),
                role: None,
                subject: "expired routed mutation".to_string(),
                scope: Vec::new(),
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            let mut request = StoreRequest::new(
                Some("engagement-o25".to_string()),
                StoreOp::AppendFact {
                    fact: serde_json::to_value(&fact).unwrap(),
                },
            );
            request.deadline_unix_ms = Some(0);

            let response = dispatch_one(&mut store, repo_root.to_string_lossy().as_ref(), request);
            match response {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::NotStarted);
                    assert_eq!(error.code, 4);
                    assert!(error.message.contains("no durable mutation started"));
                }
                other => panic!("expired mutation was not rejected: {other:?}"),
            }
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|row| row.event_id != fact.event_id),
                "expired routed mutation appended after NotStarted"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn routed_dispatch_pause_does_not_rebase_receipt_deadline() {
            let repo_root = unique_repo_root("dispatch-pause-deadline-anchor");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let fact = crate::store::Fact {
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o25-routed-dispatch-pause-no-late-commit".to_string(),
                thread_id: "thread-o25-routed-dispatch-pause".to_string(),
                kind: crate::store::FactKind::Artifact,
                subject: "dispatch pause must consume request deadline".to_string(),
                created_at: crate::now_string(),
                ..crate::store::Fact::default()
            };
            let mut request = StoreRequest::new(
                None,
                StoreOp::AppendFact {
                    fact: serde_json::to_value(&fact).unwrap(),
                },
            );
            let deadline = SystemTime::now() + Duration::from_millis(30);
            request.deadline_unix_ms =
                Some(deadline.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
            request.mutation_budget_ms = Some(30);

            let response = dispatch_one_received_with_hook(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                request,
                RequestReceipt::now(),
                || thread::sleep(Duration::from_millis(70)),
            );

            match response {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::NotStarted);
                    assert_eq!(error.code, 4);
                    assert!(error.message.contains("before acquiring"));
                }
                other => panic!("dispatch pause rebased the deadline: {other:?}"),
            }
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|row| row.event_id != fact.event_id),
                "routed mutation committed after its receipt-anchored deadline"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[cfg(unix)]
        #[test]
        fn routed_mutation_rechecks_deadline_after_flock_success() {
            let repo_root = unique_repo_root("routed-post-flock-deadline");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let fact = crate::store::Fact {
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o25-routed-post-flock-no-late-commit".to_string(),
                thread_id: "thread-o25-routed-post-flock".to_string(),
                kind: crate::store::FactKind::Artifact,
                subject: "routed post-flock pause must consume deadline".to_string(),
                created_at: crate::now_string(),
                ..crate::store::Fact::default()
            };
            let mut request = StoreRequest::new(
                None,
                StoreOp::AppendFact {
                    fact: serde_json::to_value(&fact).unwrap(),
                },
            );
            let deadline = SystemTime::now() + Duration::from_millis(30);
            request.deadline_unix_ms =
                Some(deadline.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
            request.mutation_budget_ms = Some(30);
            store::force_next_room_lock_post_flock_pause(Duration::from_millis(70));

            let response = dispatch_one(&mut store, repo_root.to_string_lossy().as_ref(), request);

            match response {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::NotStarted);
                    assert_eq!(error.code, 4);
                    assert!(error.message.contains("after provisional lock acquisition"));
                    assert!(
                        error
                            .message
                            .contains("lock released before any durable mutation")
                    );
                    assert!(!error.message.contains("before acquiring"));
                }
                other => panic!("post-flock pause started routed mutation: {other:?}"),
            }
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|row| row.event_id != fact.event_id),
                "routed mutation committed after its post-flock deadline"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn routed_lock_contention_returns_not_started_and_never_commits_late() {
            let repo_root = unique_repo_root("routed-lock-contention");
            let rally_dir = repo_root.join(".rally");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let held = store::acquire_room_mutation_lock(&rally_dir).unwrap();
            let fact = crate::store::Fact {
                from_session_id: Some("sess:o25".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "o25-routed-contended-no-late-commit".to_string(),
                seq: 0,
                thread_id: "thread-o25-routed-contended".to_string(),
                kind: crate::store::FactKind::Artifact,
                tool: Some("codex:o25".to_string()),
                role: None,
                subject: "contended routed mutation".to_string(),
                scope: Vec::new(),
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            let mut request = StoreRequest::new(
                Some("engagement-o25".to_string()),
                StoreOp::AppendFact {
                    fact: serde_json::to_value(&fact).unwrap(),
                },
            );
            let deadline = SystemTime::now() + Duration::from_millis(150);
            request.deadline_unix_ms =
                Some(deadline.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
            let root_text = repo_root.to_string_lossy().into_owned();
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let started = Instant::now();
                let response = dispatch_one(&mut store, &root_text, request);
                tx.send((started.elapsed(), response)).unwrap();
            });

            let (elapsed, response) = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("routed mutation did not return before its outer timeout");
            drop(held);
            worker.join().unwrap();
            assert!(elapsed < Duration::from_secs(1), "elapsed={elapsed:?}");
            match response {
                StoreResponse::Err(error) => {
                    assert_eq!(error.kind, StoreErrorKind::NotStarted);
                    assert_eq!(error.code, 4);
                    assert!(error.message.starts_with("mutation-not-started:"));
                }
                other => panic!("contended routed mutation was not rejected: {other:?}"),
            }

            let reopened = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            assert!(
                reopened
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|row| row.event_id != fact.event_id),
                "routed NotStarted mutation committed after lock release"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn scoped_snapshot_dispatch_requires_engagement_and_matches_direct_projection() {
            let repo_root = unique_repo_root("scoped-snapshot-parity");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            store.set_engagement_scope(Some("engagement-alpha".to_string()));
            let artifact = crate::store::Fact {
                from_session_id: Some("sess:alpha".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "artifact-scoped-parity".to_string(),
                seq: 0,
                thread_id: "thread-scoped-parity".to_string(),
                kind: crate::store::FactKind::Artifact,
                tool: Some("codex:alpha".to_string()),
                role: None,
                subject: "scoped parity".to_string(),
                scope: vec!["run:audit-run".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact(&artifact).unwrap();

            let missing = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(
                    None,
                    StoreOp::SnapshotScoped {
                        run_id: Some("audit-run".to_string()),
                        path: None,
                        include_archived: false,
                        include_presence_only: false,
                    },
                ),
            );
            assert!(
                matches!(missing, StoreResponse::Err(ref err) if err.kind == StoreErrorKind::Usage),
                "missing engagement must fail as usage: {missing:?}"
            );

            let rewritten_label = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(
                    Some("engagement-/alpha".to_string()),
                    StoreOp::SnapshotScoped {
                        run_id: Some("audit-run".to_string()),
                        path: None,
                        include_archived: false,
                        include_presence_only: false,
                    },
                ),
            );
            assert!(
                matches!(rewritten_label, StoreResponse::Err(ref err) if err.kind == StoreErrorKind::Usage),
                "daemon must validate the raw label instead of silently selecting engagement-alpha: {rewritten_label:?}"
            );

            for include_archived in [false, true] {
                let direct = store
                    .snapshot_scoped(
                        "engagement-alpha",
                        Some("audit-run"),
                        None,
                        include_archived,
                        false,
                    )
                    .unwrap();
                let routed = dispatch_one(
                    &mut store,
                    repo_root.to_string_lossy().as_ref(),
                    StoreRequest::new(
                        Some("engagement-alpha".to_string()),
                        StoreOp::SnapshotScoped {
                            run_id: Some("audit-run".to_string()),
                            path: None,
                            include_archived,
                            include_presence_only: false,
                        },
                    ),
                );
                match routed {
                    StoreResponse::Ok(StoreOk::Snapshot {
                        snapshot,
                        fingerprint,
                    }) => {
                        assert!(fingerprint.is_none(), "scoped snapshots are not cacheable");
                        assert_eq!(
                            snapshot,
                            crate::store::snapshot_to_wire_value(&direct).unwrap(),
                            "direct/routed scoped projection drifted for include_archived={include_archived}"
                        );
                    }
                    other => panic!("unexpected scoped snapshot reply: {other:?}"),
                }
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn scoped_path_collision_stops_direct_and_routed_writers_with_nonoverlap_control() {
            let repo_root = unique_repo_root("scoped-path-collision-parity");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            store.set_engagement_scope(Some("engagement-alpha".to_string()));
            let artifact = crate::store::Fact {
                from_session_id: Some("sess:alpha".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "artifact-alpha-path".to_string(),
                seq: 0,
                thread_id: "thread-alpha-path".to_string(),
                kind: crate::store::FactKind::Artifact,
                tool: Some("codex:alpha".to_string()),
                role: None,
                subject: "alpha path work".to_string(),
                scope: vec!["file:src/lib.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact(&artifact).unwrap();
            store.set_engagement_scope(Some("engagement-beta".to_string()));
            let claim = crate::store::Fact {
                from_session_id: Some("sess:beta".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "claim-beta-path".to_string(),
                seq: 0,
                thread_id: "thread-beta-path".to_string(),
                kind: crate::store::FactKind::Claim,
                tool: Some("codex:beta".to_string()),
                role: None,
                subject: "beta owns path".to_string(),
                scope: vec!["file:src/lib.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: Vec::new(),
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact(&claim).unwrap();

            let direct = store
                .snapshot_scoped("engagement-alpha", None, Some("src/lib.rs"), false, false)
                .unwrap();
            let routed = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(
                    Some("engagement-alpha".to_string()),
                    StoreOp::SnapshotScoped {
                        run_id: None,
                        path: Some("src/lib.rs".to_string()),
                        include_archived: false,
                        include_presence_only: false,
                    },
                ),
            );
            let routed = match routed {
                StoreResponse::Ok(StoreOk::Snapshot {
                    snapshot,
                    fingerprint,
                }) => {
                    assert!(fingerprint.is_none(), "scoped snapshots are not cacheable");
                    crate::store::snapshot_from_wire_value(snapshot).unwrap()
                }
                other => panic!("unexpected scoped snapshot reply: {other:?}"),
            };

            assert_eq!(
                crate::store::snapshot_to_wire_value(&direct).unwrap(),
                crate::store::snapshot_to_wire_value(&routed).unwrap(),
                "direct and routed collision context must match"
            );
            for (mode, snapshot) in [("direct", &direct), ("routed", &routed)] {
                let mut collision = Vec::new();
                crate::check::check_before_write_for_test(
                    snapshot,
                    "codex:alpha",
                    Some("src/lib.rs"),
                    &mut collision,
                );
                assert!(
                    collision.contains(&("claimed-path", "stop")),
                    "{mode} path collision must stop the writer: {collision:?}"
                );

                let mut nonoverlap = Vec::new();
                crate::check::check_before_write_for_test(
                    snapshot,
                    "codex:alpha",
                    Some("src/other.rs"),
                    &mut nonoverlap,
                );
                assert!(
                    !nonoverlap.contains(&("claimed-path", "stop")),
                    "{mode} non-overlap control must not invent a collision: {nonoverlap:?}"
                );
            }
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn routed_renewal_appends_the_same_durable_fact_as_direct_mode() {
            let repo_root = unique_repo_root("renewal-parity");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let claim = crate::store::Fact {
                from_session_id: Some("sess:test".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "claim-routed-renew".to_string(),
                seq: 0,
                thread_id: crate::new_id("room"),
                kind: crate::store::FactKind::Claim,
                tool: Some("tool-a".to_string()),
                role: None,
                subject: "routed renewal claim".to_string(),
                scope: vec!["file:src/lib.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact_verified(&claim).unwrap();
            let before = store.facts().unwrap().len();

            let response = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(
                    None,
                    StoreOp::RenewClaimLease {
                        claim_id: claim.event_id.clone(),
                        lease_expires_at: "2099-01-01T00:00:00Z".to_string(),
                        event_id: "routed-renewal-request".to_string(),
                        thread_id: "routed-renewal-thread".to_string(),
                        created_at: "2026-08-10T00:00:00Z".to_string(),
                        caller_tool: Some("tool-a".to_string()),
                        caller_session_id: Some("sess:test".to_string()),
                        expected_owner_session_id: Some("sess:test".to_string()),
                    },
                ),
            );

            assert!(
                matches!(
                    response,
                    StoreResponse::Ok(StoreOk::RenewClaimLease { outcome: _ })
                ),
                "routed renewal failed: {response:?}"
            );
            let facts = store.facts().unwrap();
            assert_eq!(facts.len(), before + 1);
            assert_eq!(
                facts.last().unwrap().kind,
                crate::store::FactKind::ClaimRenewed
            );
            assert_eq!(
                facts.last().unwrap().ref_id.as_deref(),
                Some(claim.event_id.as_str())
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn raw_routed_sibling_cannot_renew_another_sessions_claim() {
            let repo_root = unique_repo_root("renewal-sibling-auth");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let claim = crate::store::Fact {
                from_session_id: Some("session-owner".to_string()),
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "claim-routed-sibling".to_string(),
                seq: 0,
                thread_id: crate::new_id("room"),
                kind: crate::store::FactKind::Claim,
                tool: Some("shared:01".to_string()),
                role: None,
                subject: "routed sibling authority claim".to_string(),
                scope: vec!["file:src/lib.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact_verified(&claim).unwrap();

            // Decode a raw wire payload instead of constructing StoreOp so this
            // remains a compatibility control when authority fields are added.
            let op: StoreOp = serde_json::from_value(serde_json::json!({
                "kind": "renew_claim_lease",
                "claim_id": claim.event_id.clone(),
                "lease_expires_at": "2099-01-01T00:00:00Z",
                "event_id": "renew-routed-sibling",
                "thread_id": "renew-routed-sibling-thread",
                "created_at": "2026-08-10T00:00:00Z",
                "caller_tool": "shared:01",
                "caller_session_id": "session-sibling",
                "expected_owner_session_id": "session-owner"
            }))
            .unwrap();
            let response = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(None, op),
            );

            assert!(
                matches!(response, StoreResponse::Err(_)),
                "raw routed sibling renewal must be refused: {response:?}"
            );
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|fact| fact.kind != crate::store::FactKind::ClaimRenewed),
                "a refused raw renewal must not synthesize owner authority from claim_id"
            );

            let missing_caller: StoreOp = serde_json::from_value(serde_json::json!({
                "kind": "renew_claim_lease",
                "claim_id": claim.event_id.clone(),
                "lease_expires_at": "2099-01-01T00:00:00Z",
                "event_id": "renew-routed-missing-caller",
                "thread_id": "renew-routed-missing-caller-thread",
                "created_at": "2026-08-10T00:00:00Z"
            }))
            .unwrap();
            let missing_response = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(None, missing_caller),
            );
            assert!(
                matches!(missing_response, StoreResponse::Err(_)),
                "a legacy wire request with no caller authority must fail closed: {missing_response:?}"
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn raw_routed_anonymous_identity_cannot_renew_or_append_owner_transitions() {
            let repo_root = unique_repo_root("renewal-anonymous-auth");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let claim = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "claim-routed-anonymous".to_string(),
                seq: 0,
                thread_id: crate::new_id("room"),
                kind: crate::store::FactKind::Claim,
                tool: None,
                role: None,
                subject: "anonymous routed authority claim".to_string(),
                scope: vec!["file:src/anonymous.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: vec!["lease_expires_at:2099-01-01T00:00:00Z".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact_verified(&claim).unwrap();
            let before = store.facts().unwrap().len();

            let renew_op: StoreOp = serde_json::from_value(serde_json::json!({
                "kind": "renew_claim_lease",
                "claim_id": claim.event_id.clone(),
                "lease_expires_at": "2099-01-01T00:30:00Z",
                "event_id": "renew-routed-anonymous",
                "thread_id": "renew-routed-anonymous-thread",
                "created_at": "2026-08-10T00:00:00Z"
            }))
            .unwrap();
            let renew_response = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(None, renew_op),
            );
            assert!(
                matches!(renew_response, StoreResponse::Err(_)),
                "anonymous routed renewal must fail closed: {renew_response:?}"
            );
            assert_eq!(store.facts().unwrap().len(), before);

            for (kind, event_id) in [
                (crate::store::FactKind::ClaimRenewed, "renew-raw-anonymous"),
                (crate::store::FactKind::Release, "release-raw-anonymous"),
            ] {
                let transition = crate::store::Fact {
                    from_session_id: None,
                    schema: crate::FACT_SCHEMA.to_string(),
                    event_id: event_id.to_string(),
                    seq: 0,
                    thread_id: crate::new_id("room"),
                    kind: kind.clone(),
                    tool: None,
                    role: None,
                    subject: event_id.to_string(),
                    scope: claim.scope.clone(),
                    created_at: crate::now_string(),
                    summary: None,
                    evidence: if kind == crate::store::FactKind::ClaimRenewed {
                        vec!["lease_expires_at:2099-01-01T00:30:00Z".to_string()]
                    } else {
                        Vec::new()
                    },
                    target: None,
                    ref_id: Some(claim.event_id.clone()),
                    status: None,
                    severity: None,
                    uri: None,
                    session: None,
                };
                let append_op: StoreOp = serde_json::from_value(serde_json::json!({
                    "kind": "append_fact_verified",
                    "fact": serde_json::to_value(&transition).unwrap()
                }))
                .unwrap();
                let response = dispatch_one(
                    &mut store,
                    repo_root.to_string_lossy().as_ref(),
                    StoreRequest::new(None, append_op),
                );
                assert!(
                    matches!(response, StoreResponse::Err(_)),
                    "anonymous raw {kind:?} must fail closed: {response:?}"
                );
                assert_eq!(
                    store.facts().unwrap().len(),
                    before,
                    "refused raw {kind:?} must not append"
                );
            }
            assert!(
                store
                    .snapshot()
                    .unwrap()
                    .active_claims
                    .iter()
                    .any(|active| active.event_id == claim.event_id)
            );
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn raw_routed_modern_caller_can_renew_a_legacy_sessionless_claim() {
            let repo_root = unique_repo_root("renewal-legacy-modern");
            let mut store = DirectRoomStore::open_direct_at(repo_root.clone()).unwrap();
            let claim = crate::store::Fact {
                from_session_id: None,
                schema: crate::FACT_SCHEMA.to_string(),
                event_id: "claim-routed-legacy".to_string(),
                seq: 0,
                thread_id: crate::new_id("room"),
                kind: crate::store::FactKind::Claim,
                tool: Some("tool-a".to_string()),
                role: None,
                subject: "legacy routed authority claim".to_string(),
                scope: vec!["file:src/legacy.rs".to_string()],
                created_at: crate::now_string(),
                summary: None,
                evidence: vec!["lease_expires_at:2000-01-01T00:00:00Z".to_string()],
                target: None,
                ref_id: None,
                status: None,
                severity: None,
                uri: None,
                session: None,
            };
            store.append_fact_verified(&claim).unwrap();

            let op: StoreOp = serde_json::from_value(serde_json::json!({
                "kind": "renew_claim_lease",
                "claim_id": claim.event_id.clone(),
                "lease_expires_at": "2099-01-01T00:30:00Z",
                "event_id": "legacy-renewal-request",
                "thread_id": "legacy-renewal-thread",
                "created_at": "2026-08-10T00:00:00Z",
                "caller_tool": "tool-a",
                "caller_session_id": "session-modern",
                "expected_owner_session_id": null
            }))
            .unwrap();
            let response = dispatch_one(
                &mut store,
                repo_root.to_string_lossy().as_ref(),
                StoreRequest::new(None, op),
            );
            assert!(
                matches!(
                    response,
                    StoreResponse::Ok(StoreOk::RenewClaimLease { outcome: _ })
                ),
                "identified caller must retain legacy renewal compatibility: {response:?}"
            );
            assert_eq!(
                crate::claim_authority::active_claim_record(
                    &store.facts().unwrap(),
                    &claim.event_id
                )
                .and_then(|record| record.lease_expires_at)
                .as_deref(),
                Some("2099-01-01T00:30:00Z")
            );
            let renewal = store
                .facts()
                .unwrap()
                .into_iter()
                .find(|fact| fact.kind == crate::store::FactKind::ClaimRenewed)
                .expect("routed renewal must append");
            assert_eq!(renewal.tool.as_deref(), Some("tool-a"));
            assert_eq!(renewal.from_session_id.as_deref(), Some("session-modern"));
            std::fs::remove_dir_all(repo_root).ok();
        }

        #[test]
        fn o26_routed_mutations_are_fenced_until_db_only_marker_removal() {
            let repo_root = unique_repo_root("o26-routed-migration-fence");
            let rally_dir = repo_root.join(".rally");
            let mut store = DirectRoomStore::open_direct_at_with_engagement(
                repo_root.clone(),
                Some("alpha".to_string()),
            )
            .unwrap();
            store.install_warm_fact_store().unwrap();

            let request = |event_id: &str| {
                StoreRequest::new(
                    Some("alpha".to_string()),
                    StoreOp::AppendFact {
                        fact: serde_json::json!({
                            "schema": crate::FACT_SCHEMA,
                            "event_id": event_id,
                            "seq": 0,
                            "thread_id": format!("thread-{event_id}"),
                            "kind": "decision",
                            "subject": "migration fence control",
                            "tool": "codex:test",
                            "scope": [],
                            "created_at": "2026-08-10T00:00:00Z",
                            "evidence": [],
                        }),
                    },
                )
            };
            let marker = rally_dir.join(crate::store::DB_ONLY_MIGRATION_MARKER_FILENAME);
            let marker_stage =
                rally_dir.join(crate::store::DB_ONLY_MIGRATION_MARKER_STAGE_FILENAME);
            let receipt = rally_dir.join("db-only-migration.v1.receipt.json");
            let db_before = std::fs::read(rally_dir.join("facts.db")).unwrap();

            std::fs::write(&marker_stage, b"prepared marker staging evidence").unwrap();
            let stage_before = std::fs::read(&marker_stage).unwrap();
            let response = dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                request("routed-fenced-marker-stage"),
            );
            match response {
                StoreResponse::Err(error) => {
                    assert!(error.message.contains("migrate-db-only"), "{error:?}");
                }
                other => panic!("routed append bypassed migration marker stage: {other:?}"),
            }
            assert_eq!(std::fs::read(&marker_stage).unwrap(), stage_before);
            assert_eq!(
                std::fs::read(rally_dir.join("facts.db")).unwrap(),
                db_before
            );
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .all(|fact| fact.event_id != "routed-fenced-marker-stage")
            );
            std::fs::remove_file(&marker_stage).unwrap();

            std::fs::write(&marker, b"prepared marker evidence").unwrap();

            for (event_id, cleanup_pending) in [
                ("routed-fenced-pre-receipt", false),
                ("routed-fenced-cleanup-pending", true),
            ] {
                if cleanup_pending {
                    std::fs::write(&receipt, b"immutable receipt evidence").unwrap();
                }
                let response = dispatch_one(
                    &mut store,
                    canonical_repo_root(&repo_root).as_str(),
                    request(event_id),
                );
                match response {
                    StoreResponse::Err(error) => {
                        assert!(error.message.contains("migrate-db-only"), "{error:?}");
                    }
                    other => panic!("routed append bypassed migration marker: {other:?}"),
                }
                assert_eq!(
                    std::fs::read(rally_dir.join("facts.db")).unwrap(),
                    db_before
                );
                assert!(
                    store
                        .facts()
                        .unwrap()
                        .iter()
                        .all(|fact| fact.event_id != event_id)
                );
            }

            std::fs::remove_file(&marker).unwrap();
            match dispatch_one(
                &mut store,
                canonical_repo_root(&repo_root).as_str(),
                request("routed-after-marker-removal"),
            ) {
                StoreResponse::Ok(StoreOk::AppendFact { .. }) => {}
                other => panic!("marker removal must reopen ordinary routed writes: {other:?}"),
            }
            assert!(
                store
                    .facts()
                    .unwrap()
                    .iter()
                    .any(|fact| fact.event_id == "routed-after-marker-removal")
            );
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
            let mut append_index = 0usize;
            let mut append = |subject: &str| {
                append_index += 1;
                let fact = serde_json::json!({
                    "schema": crate::FACT_SCHEMA,
                    "event_id": format!("rallyd-smoke-{append_index}"),
                    "seq": 0,
                    "thread_id": "rallyd-smoke-thread",
                    "kind": "lesson",
                    "subject": subject,
                    "tool": "claude_code:01",
                    "scope": ["rallyd-smoke"],
                    "created_at": "2026-08-10T00:00:00Z",
                    "evidence": [],
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

            // Seed enough real room state to force both facts and snapshot
            // replies past the 8 KiB socket-buffer boundary seen in dogfood.
            for i in 0..12 {
                let subject = format!("large-reply-{i}-{}", "x".repeat(1024));
                match append(&subject) {
                    StoreResponse::Ok(StoreOk::AppendFact { .. }) => {}
                    other => panic!("large-reply append not Ok: {other:?}"),
                }
            }
            let facts_reply = round_trip(&socket, &StoreRequest::new(None, StoreOp::Facts));
            assert!(
                serde_json::to_string(&facts_reply).unwrap().len() > 8 * 1024,
                "fixture did not cross the historical truncation boundary"
            );
            assert!(
                matches!(facts_reply, StoreResponse::Ok(StoreOk::Facts { .. })),
                "large facts reply was incomplete: {facts_reply:?}"
            );

            // All appends are visible through a large snapshot read (same store).
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

            // G10 proof (f1): the appends + large facts/snapshot reads were served entirely
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
