// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! # log_watch — event-driven wakeups for `rally watch`
//!
//! Charter constraint (non-negotiable): Rally RECORDS and NOTIFIES, it never
//! executes. This module only OBSERVES the filesystem — it writes nothing,
//! spawns nothing, and posts no facts. `command_watch` in `lib.rs` is the
//! only caller; it decides what to do with a reported change.
//!
//! ## Why this exists
//! The old `rally watch` loop was a pure interval poller: `thread::sleep`
//! then re-read `.rally/log/index.json`. Default `--interval` is 5s and
//! backs off x1.5 up to `--max-interval` 300s, so a handoff posted to an
//! idle managed peer could sit undelivered for up to 5 minutes.
//! `wait_for_change` blocks on a kernel change notification instead, so the
//! interval becomes a safety-net ceiling rather than the delivery latency.
//!
//! ## Backend selection
//! - macOS/iOS/FreeBSD/OpenBSD/NetBSD: kqueue (`EVFILT_VNODE` on the log
//!   directory fd, plus the index file fd when it exists — belt and braces,
//!   see the module doc on [`kqueue_backend`]).
//! - Linux: inotify on the log directory.
//! - Everything else: `thread::sleep` the full timeout, reporting
//!   [`WaitOutcome::Unsupported`] so the caller can label the tick "poll".
//!
//! ## FD lifetime
//! `command_watch` loops forever. Every raw fd opened here is wrapped in
//! [`OwnedFd`], whose `Drop` impl closes it — every early return and every
//! `?` path closes the fd. A leaked fd per iteration would exhaust the
//! process's fd table within days on a long-running watcher.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Outcome of one [`wait_for_change`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// The kernel reported a change before the deadline.
    Changed,
    /// The deadline elapsed with no reported change.
    TimedOut,
    /// No usable watch backend on this platform/fd; the caller slept the
    /// full timeout as a fallback. Callers should treat this the same as a
    /// (slow) `TimedOut` but label the tick "poll" rather than "event".
    Unsupported,
}

/// A LONG-LIVED watch registration over `.rally/log/`.
///
/// Registration must outlive a single wait. This is the whole design
/// constraint, and getting it wrong is silent: an earlier version of this
/// module built a fresh kqueue per wait, and lost every change that landed
/// in the gap between "wait returned" and "next wait registered".
///
/// That gap is not theoretical — it is the common case here. `rally say`
/// writes the index tmp-then-rename, which is TWO directory events. The
/// watcher woke on the tmp-create, read an index that still held the OLD
/// `max_seq`, saw no advance, and re-registered — by which time the rename
/// had already happened, unobserved. The fact was then invisible until the
/// safety-net timeout, which is precisely the multi-minute stall this module
/// exists to remove. Measured before the fix: the watcher woke on the right
/// event and still reported `seq: 5` while `index.json` said 6.
///
/// A persistent registration closes it: the kernel QUEUES events that arrive
/// while we are not blocked, so the next wait returns immediately instead of
/// sleeping through a change that already happened.
pub(crate) struct LogWatcher {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    inner: Option<kqueue_backend::Watcher>,
    #[cfg(target_os = "linux")]
    inner: Option<inotify_backend::Watcher>,
    /// Paths kept so the watcher can RE-ARM. See `may_rearm`.
    #[allow(dead_code)]
    log_dir: PathBuf,
    #[allow(dead_code)]
    index_file: PathBuf,
    /// True when the backend has never successfully armed and it is still
    /// worth retrying; false once it armed, or once it armed and then failed
    /// hard.
    ///
    /// The distinction matters and is not cosmetic:
    ///
    ///  * NEVER-ARMED is usually transient. `.rally/log/` does not exist until
    ///    the first fact is written, so a watcher started on a fresh clone —
    ///    exactly what `rally watch --print-launchd` installs — has nothing to
    ///    open yet. Without a retry it would stay inert forever and deliver
    ///    NOTHING, silently, while looking healthy. Verified before this fix:
    ///    a watcher started right after `rally init` produced zero output
    ///    while a handoff was posted.
    ///  * ARMED-THEN-FAILED is usually permanent (the volume went away, the fd
    ///    went bad). Retrying that every iteration would fail every iteration
    ///    instantly, which is the hot-spin the sleep floor in `wait` exists to
    ///    prevent. So a hard failure degrades for good.
    may_rearm: bool,
}

impl LogWatcher {
    /// Register the watch once. A backend that cannot be established yields a
    /// watcher whose `wait` degrades to sleeping — never an error, because a
    /// watcher that refuses to start is strictly worse than a slow one — and
    /// which retries arming on each wait until it succeeds.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    pub(crate) fn new(log_dir: &Path, index_file: &Path) -> Self {
        let inner = kqueue_backend::Watcher::new(log_dir, index_file);
        Self {
            may_rearm: inner.is_none(),
            inner,
            log_dir: log_dir.to_path_buf(),
            index_file: index_file.to_path_buf(),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new(log_dir: &Path, index_file: &Path) -> Self {
        let inner = inotify_backend::Watcher::new(log_dir);
        Self {
            may_rearm: inner.is_none(),
            inner,
            log_dir: log_dir.to_path_buf(),
            index_file: index_file.to_path_buf(),
        }
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    )))]
    pub(crate) fn new(log_dir: &Path, index_file: &Path) -> Self {
        Self {
            log_dir: log_dir.to_path_buf(),
            index_file: index_file.to_path_buf(),
            may_rearm: false,
        }
    }

    /// Whether an event backend is currently armed. `command_watch` reports
    /// this per tick so a blind watcher cannot claim event delivery.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    ))]
    pub(crate) fn is_armed(&self) -> bool {
        self.inner.is_some()
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    )))]
    pub(crate) fn is_armed(&self) -> bool {
        false
    }

    /// Block until the watched directory changes or `timeout` elapses.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    ))]
    pub(crate) fn wait(&mut self, timeout: Duration) -> WaitOutcome {
        let started = std::time::Instant::now();

        // Re-arm attempt: `.rally/log/` is created lazily on the first fact
        // write, so a watcher started before that had nothing to open. One
        // cheap `open` per interval until it succeeds.
        if self.inner.is_none() && self.may_rearm {
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd"
            ))]
            {
                self.inner = kqueue_backend::Watcher::new(&self.log_dir, &self.index_file);
            }
            #[cfg(target_os = "linux")]
            {
                self.inner = inotify_backend::Watcher::new(&self.log_dir);
            }
            if self.inner.is_some() {
                self.may_rearm = false;
            }
        }

        let outcome = match self.inner {
            Some(ref mut w) => {
                let outcome = w.wait(timeout);
                if matches!(outcome, WaitOutcome::Unsupported) {
                    // The backend armed and THEN failed — the watched volume
                    // went away, the fd went bad. Retrying that every
                    // iteration would fail every iteration, instantly.
                    // Degrade to the sleeping path permanently (no re-arm).
                    self.inner = None;
                    self.may_rearm = false;
                }
                outcome
            }
            None => {
                std::thread::sleep(timeout);
                WaitOutcome::Unsupported
            }
        };

        // INVARIANT: this function returns early ONLY for `Changed`. A
        // `TimedOut` or `Unsupported` return must have consumed the full
        // timeout, so the caller's loop can never spin faster than the
        // interval it asked for.
        //
        // Without this, a backend that fails instantly turns the watch loop
        // into a hot spin: `command_watch` re-reads and re-parses index.json
        // and (under --json) writes a heartbeat line per iteration, at CPU
        // speed, into an unrotated launchd log — tens of MB per second. The
        // 3s watchdog used to make that unreachable; now that the watcher
        // legitimately runs for weeks, the floor has to be explicit.
        if !matches!(outcome, WaitOutcome::Changed) {
            let elapsed = started.elapsed();
            if elapsed < timeout {
                std::thread::sleep(timeout - elapsed);
            }
        }
        outcome
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    )))]
    pub(crate) fn wait(&mut self, timeout: Duration) -> WaitOutcome {
        std::thread::sleep(timeout);
        WaitOutcome::Unsupported
    }
}

/// One-shot convenience wrapper: construct a watcher, wait once, drop it.
///
/// Correct ONLY for a single isolated wait. Any loop must hold a
/// [`LogWatcher`] across iterations instead — see its doc comment for the
/// lost-wakeup race that a per-call registration reintroduces.
#[cfg(test)]
pub(crate) fn wait_for_change(log_dir: &Path, index_file: &Path, timeout: Duration) -> WaitOutcome {
    LogWatcher::new(log_dir, index_file).wait(timeout)
}

/// "kqueue" | "inotify" | "poll" — reported in the watcher's JSON output.
/// This names the COMPILE-TIME backend; `command_watch` additionally
/// downgrades a per-tick label to "poll" whenever `--poll` was passed or a
/// given `wait_for_change` call returned [`WaitOutcome::Unsupported`].
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub(crate) fn backend_name() -> &'static str {
    kqueue_backend::NAME
}

#[cfg(target_os = "linux")]
pub(crate) fn backend_name() -> &'static str {
    inotify_backend::NAME
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux"
)))]
pub(crate) fn backend_name() -> &'static str {
    "poll"
}

/// RAII guard for a raw Unix fd opened by this module. `Drop` closes it, so
/// every early return (including a `?` or a bare `return`) closes the fd —
/// no hand-rolled `libc::close` at each exit point, which is exactly the
/// pattern that leaks under a future refactor that adds a new early return.
#[cfg(unix)]
struct OwnedFd(std::os::unix::io::RawFd);

#[cfg(unix)]
impl OwnedFd {
    /// Open `path` with `flags` (no `O_CREAT` — we only ever watch things
    /// that may or may not already exist; a missing path is `None`, not an
    /// error, since the index file legitimately doesn't exist before the
    /// first fact is posted).
    /// `O_CLOEXEC` is forced on. `command_watch` spawns `sh -c` for
    /// `--on-activity`, and without it every delivery child inherits the
    /// watched directory and index fds — an adapter that daemonizes a helper
    /// would leave that helper pinning `.rally/log/` and any unlinked inode
    /// for its own lifetime. The inotify backend already sets `IN_CLOEXEC`;
    /// this makes the kqueue side consistent rather than accidentally laxer.
    fn open(path: &Path, flags: libc::c_int) -> Option<OwnedFd> {
        use std::os::unix::ffi::OsStrExt;
        let cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let fd = unsafe { libc::open(cstr.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 { None } else { Some(OwnedFd(fd)) }
    }

    fn raw(&self) -> std::os::unix::io::RawFd {
        self.0
    }
}

#[cfg(unix)]
impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

/// macOS / BSD backend: kqueue's `EVFILT_VNODE`.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
mod kqueue_backend {
    use super::{OwnedFd, WaitOutcome};
    use std::path::Path;
    use std::time::{Duration, Instant};

    pub(super) const NAME: &str = "kqueue";

    /// macOS/iOS have `O_EVTONLY` — an fd that doesn't count as an open
    /// reference for `NOTE_DELETE`/unmount purposes, which is exactly what
    /// a watch-only fd wants. The other BSDs don't define it; `O_RDONLY` is
    /// the closest equivalent for a descriptor we only ever pass to
    /// `kevent`, never `read`.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const WATCH_OPEN_FLAGS: libc::c_int = libc::O_EVTONLY;
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    const WATCH_OPEN_FLAGS: libc::c_int = libc::O_RDONLY;

    /// `index.json` is written tmp-then-rename (verified: leftover
    /// `index.json.tmp-*` files exist in a live `.rally/log/`), so the
    /// DIRECTORY vnode is the primary signal — a rename changes a
    /// directory's entry list. The file fd, when the file already exists,
    /// additionally catches any future in-place write. A missed wakeup here
    /// means a stall back to the old 5-minute worst case, so this covers
    /// both write shapes rather than assuming the file is always
    /// replaced atomically.
    /// A kqueue plus the fds it watches, all living as long as the watcher.
    /// The `_dir_fd`/`_file_fd` fields are never read after registration —
    /// they are held so the fds stay OPEN, because closing a watched fd
    /// silently deregisters its kevent.
    pub(super) struct Watcher {
        kq: OwnedFd,
        _dir_fd: OwnedFd,
        _file_fd: Option<OwnedFd>,
    }

    impl Watcher {
        pub(super) fn new(log_dir: &Path, index_file: &Path) -> Option<Watcher> {
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return None;
            }
            let kq = OwnedFd(kq);
            // `kqueue()` takes no flags, so CLOEXEC has to be set after the
            // fact. Same reason as `OwnedFd::open`: `--on-activity` children
            // must not inherit the watcher's descriptors.
            unsafe {
                libc::fcntl(kq.raw(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let dir_fd = OwnedFd::open(log_dir, WATCH_OPEN_FLAGS)?;
            // Best-effort: the index file may not exist yet (fresh room, no
            // fact posted). That's fine — the directory watch still catches
            // its eventual tmp+rename creation.
            let file_fd = OwnedFd::open(index_file, WATCH_OPEN_FLAGS);

            let fflags = libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_LINK
                | libc::NOTE_RENAME
                | libc::NOTE_DELETE
                | libc::NOTE_ATTRIB;

            let mut changelist: Vec<libc::kevent> = vec![make_kevent(dir_fd.raw(), fflags)];
            if let Some(ref f) = file_fd {
                changelist.push(make_kevent(f.raw(), fflags));
            }

            // Register ONCE, with a zero timeout so this only applies the
            // changelist and returns. From here the kernel queues matching
            // events whether or not we happen to be blocked in `wait`.
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let n = unsafe {
                libc::kevent(
                    kq.raw(),
                    changelist.as_ptr(),
                    changelist.len() as libc::c_int,
                    std::ptr::null_mut(),
                    0,
                    &zero,
                )
            };
            if n < 0 {
                return None;
            }
            Some(Watcher {
                kq,
                _dir_fd: dir_fd,
                _file_fd: file_fd,
            })
        }

        pub(super) fn wait(&mut self, timeout: Duration) -> WaitOutcome {
            let start = Instant::now();
            loop {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    return WaitOutcome::TimedOut;
                }
                let remaining = timeout - elapsed;
                let ts = libc::timespec {
                    tv_sec: remaining.as_secs() as _,
                    tv_nsec: remaining.subsec_nanos() as _,
                };
                let mut events: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
                // Empty changelist: the watch was registered in `new` and is
                // still armed. Passing the changelist again here would be
                // harmless, but re-registering per wait is exactly the shape
                // that produced the lost-wakeup bug, so it is not done.
                let n = unsafe {
                    libc::kevent(
                        self.kq.raw(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as libc::c_int,
                        &ts,
                    )
                };
                match n {
                    0 => return WaitOutcome::TimedOut,
                    n if n > 0 => return WaitOutcome::Changed,
                    _ => {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() == Some(libc::EINTR) {
                            // Retry with the REMAINING time, recomputed from
                            // `start` at the top of the loop — do not restart
                            // the full timeout on every signal.
                            continue;
                        }
                        return WaitOutcome::Unsupported;
                    }
                }
            }
        }
    }

    fn make_kevent(fd: std::os::unix::io::RawFd, fflags: u32) -> libc::kevent {
        libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        }
    }
}

/// Linux backend: inotify on the log directory.
#[cfg(target_os = "linux")]
mod inotify_backend {
    use super::{OwnedFd, WaitOutcome};
    use std::path::Path;
    use std::time::{Duration, Instant};

    pub(super) const NAME: &str = "inotify";

    /// The inotify fd, held open for the watcher's lifetime. Same reason as
    /// the kqueue backend: a watch registered per-wait loses every event that
    /// lands between waits, and `rally say`'s tmp-then-rename reliably
    /// produces one.
    pub(super) struct Watcher {
        fd: OwnedFd,
    }

    impl Watcher {
        pub(super) fn new(log_dir: &Path) -> Option<Watcher> {
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            let fd = OwnedFd(fd);
            let dir_cstr = path_to_cstring(log_dir)?;
            let mask = (libc::IN_CREATE
                | libc::IN_MODIFY
                | libc::IN_MOVED_TO
                | libc::IN_CLOSE_WRITE
                | libc::IN_DELETE
                | libc::IN_MOVED_FROM) as u32;
            let watch_id = unsafe { libc::inotify_add_watch(fd.raw(), dir_cstr.as_ptr(), mask) };
            if watch_id < 0 {
                return None;
            }
            Some(Watcher { fd })
        }

        pub(super) fn wait(&mut self, timeout: Duration) -> WaitOutcome {
            let start = Instant::now();
            loop {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    return WaitOutcome::TimedOut;
                }
                let remaining_ms = (timeout - elapsed)
                    .as_millis()
                    .min(i64::from(i32::MAX) as u128) as i32;
                let mut pfd = libc::pollfd {
                    fd: self.fd.raw(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let n = unsafe {
                    libc::poll(
                        &mut pfd as *mut libc::pollfd,
                        1 as libc::nfds_t,
                        remaining_ms,
                    )
                };
                match n {
                    0 => return WaitOutcome::TimedOut,
                    n if n > 0 => {
                        // A revents of POLLERR/POLLHUP/POLLNVAL is NOT data.
                        // Treating it as data is a permanent instant-return:
                        // poll keeps reporting ready, the read keeps failing,
                        // and the caller's loop spins. Report it as a dead
                        // backend so `LogWatcher::wait` retires this watcher
                        // and falls back to sleeping.
                        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                            return WaitOutcome::Unsupported;
                        }
                        // Drain the buffer so an already-consumed event does
                        // not immediately re-wake the NEXT wait. We do not
                        // parse individual records — only that something fired.
                        let mut buf = [0u8; 4096];
                        unsafe {
                            libc::read(
                                self.fd.raw(),
                                buf.as_mut_ptr() as *mut libc::c_void,
                                buf.len(),
                            );
                        }
                        return WaitOutcome::Changed;
                    }
                    _ => {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() == Some(libc::EINTR) {
                            continue;
                        }
                        return WaitOutcome::Unsupported;
                    }
                }
            }
        }
    }

    fn path_to_cstring(path: &Path) -> Option<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
    }
}

/// Lexically normalize `path` — resolve `.`/`..` components without
/// touching the filesystem (so it works for paths that don't exist yet,
/// e.g. an `--ack-file` that hasn't been created).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `--ack-file` loop-hazard guard: an ack file written inside the
/// watched `.rally/log/` dir would bump the very `max_seq` the watcher
/// polls, so the watcher would report its OWN delivery ack as new activity
/// — a self-triggering feedback loop that never settles. `command_watch`
/// calls this at startup and refuses to run when it returns `true`.
///
/// `ack_file` may be relative (resolved against `repo`) or absolute.
/// `log_dir` is expected already-absolute (as `command_watch` builds it),
/// but `Path::join` with an absolute RHS replaces the LHS entirely, so this
/// is correct either way.
pub(crate) fn ack_file_conflicts_with_log_dir(
    repo: &Path,
    log_dir: &Path,
    ack_file: &Path,
) -> bool {
    let log_dir_norm = lexical_normalize(&repo.join(log_dir));
    let ack_norm = lexical_normalize(&repo.join(ack_file));
    ack_norm == log_dir_norm || ack_norm.starts_with(&log_dir_norm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rally-log-watch-{label}-{}-{}",
            std::process::id(),
            crate::short_id()
        ))
    }

    /// Test 1: `wait_for_change` returns `Changed` within ~2s (generous —
    /// a CI box is slow; the point is proving it does not wait the full
    /// interval, not micro-benchmarking) when a file is created in a
    /// watched directory.
    #[test]
    fn wait_for_change_detects_file_creation_quickly() {
        let dir = unique_temp_dir("create");
        std::fs::create_dir_all(&dir).unwrap();
        let index_file = dir.join("index.json");

        let watch_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::write(watch_dir.join("new-file.txt"), b"x").unwrap();
        });

        let started = Instant::now();
        let outcome = wait_for_change(&dir, &index_file, Duration::from_secs(5));
        let elapsed = started.elapsed();
        handle.join().unwrap();

        assert_eq!(
            outcome,
            WaitOutcome::Changed,
            "expected Changed when a file is created in the watched dir (or Unsupported \
             on a platform with no backend — if this fired, the CI box's backend needs \
             checking): got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "wait_for_change must not wait out the full 5s timeout when the kernel reports \
             a change quickly; took {elapsed:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// THE test for the actual fix. Everything else in this module exercises
    /// the one-shot `wait_for_change` wrapper, which is the very shape that
    /// was broken — so none of it would fail if `LogWatcher::new` were moved
    /// back inside the caller's loop.
    ///
    /// This asserts the property that matters: a change that lands while the
    /// watcher is NOT blocked is still delivered on the next wait, because a
    /// persistent registration lets the kernel queue it. Per-call registration
    /// drops that event and the wait sits until its timeout.
    ///
    /// The 3s timeout vs the ~50ms assertion is the discriminator: a lost
    /// wakeup returns TimedOut at 3s, a queued one returns Changed almost
    /// immediately.
    #[test]
    fn a_change_while_not_blocked_is_queued_and_delivered_next_wait() {
        let dir = unique_temp_dir("queued-while-idle");
        std::fs::create_dir_all(&dir).unwrap();
        let index_file = dir.join("index.json");
        std::fs::write(&index_file, b"{}").unwrap();

        let mut watcher = LogWatcher::new(&dir, &index_file);
        assert!(
            watcher.is_armed(),
            "backend must arm over an existing dir, or this test proves nothing"
        );

        // Drain the spurious arm-time wake (opening the dir fd can itself
        // register as NOTE_ATTRIB), so what we measure below is our write.
        let _ = watcher.wait(Duration::from_millis(250));

        // The critical window: write while NOT inside wait(), exactly as
        // `rally say` does while command_watch is processing the previous
        // tick. Use the real write shape — tmp then rename over index.json.
        let tmp = dir.join("index.json.tmp-queued");
        std::fs::write(&tmp, b"{\"segments\":[]}").unwrap();
        std::fs::rename(&tmp, &index_file).unwrap();

        let started = Instant::now();
        let outcome = watcher.wait(Duration::from_secs(3));
        let elapsed = started.elapsed();

        assert_eq!(
            outcome,
            WaitOutcome::Changed,
            "a change made while the watcher was not blocked must still be \
             reported — this is the lost-wakeup regression"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "the queued event must return almost immediately, not after the \
             timeout; took {elapsed:?} (a per-call registration would sit the \
             full 3s here)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The anti-hot-spin invariant: a non-`Changed` return must consume the
    /// full timeout. Asserted on a MISSING directory, which is the cheapest
    /// way to drive the backend into its failure path — that is exactly the
    /// case that returned instantly before, turning the caller's loop into a
    /// CPU-speed spin that writes heartbeat JSON into an unrotated log.
    #[test]
    fn a_failed_backend_still_consumes_the_full_timeout() {
        let missing = unique_temp_dir("absent-never-created");
        let index_file = missing.join("index.json");
        let timeout = Duration::from_millis(400);

        let mut watcher = LogWatcher::new(&missing, &index_file);
        for round in 1..=3 {
            let started = Instant::now();
            let outcome = watcher.wait(timeout);
            let elapsed = started.elapsed();
            assert_ne!(
                outcome,
                WaitOutcome::Changed,
                "round {round}: nothing changed (the dir does not even exist)"
            );
            // Generous lower bound: sleeps may undershoot slightly, but an
            // instant return is ~0ms and is what we are guarding against.
            assert!(
                elapsed >= Duration::from_millis(300),
                "round {round}: wait returned in {elapsed:?} without consuming its \
                 {timeout:?} timeout — this is the hot-spin regression"
            );
        }
    }

    /// A watcher whose backend never established must not hot-spin either —
    /// it degrades to sleeping, and keeps doing so.
    #[test]
    fn watcher_over_missing_dir_reports_a_non_event_backend() {
        let missing = unique_temp_dir("absent-2");
        let mut watcher = LogWatcher::new(&missing, &missing.join("index.json"));
        assert_eq!(
            watcher.wait(Duration::from_millis(120)),
            WaitOutcome::Unsupported,
            "an unopenable watch dir must report Unsupported so command_watch \
             labels the tick 'poll' rather than claiming event delivery"
        );
    }

    /// Test 2: `wait_for_change` returns `TimedOut` (not `Changed`) when
    /// nothing happens within a short timeout.
    #[test]
    fn wait_for_change_times_out_when_nothing_happens() {
        let dir = unique_temp_dir("idle");
        std::fs::create_dir_all(&dir).unwrap();
        let index_file = dir.join("index.json");

        let started = Instant::now();
        let outcome = wait_for_change(&dir, &index_file, Duration::from_millis(300));
        let elapsed = started.elapsed();

        assert_ne!(
            outcome,
            WaitOutcome::Changed,
            "no change occurred; must not report Changed"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "an idle wait must return close to its own timeout, not hang; took {elapsed:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Test 3: the REAL write shape — `index.json.tmp-*` written then
    /// renamed over `index.json` — must be detected as `Changed`. This is
    /// how the store actually persists `index.json` (verified against
    /// `watch_write_once_cursor`'s tmp+rename pattern), not a plain
    /// in-place write.
    #[test]
    fn wait_for_change_detects_atomic_rename_over_index_file() {
        let dir = unique_temp_dir("rename");
        std::fs::create_dir_all(&dir).unwrap();
        let index_file = dir.join("index.json");
        std::fs::write(&index_file, b"{}").unwrap();

        let watch_dir = dir.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let tmp = watch_dir.join("index.json.tmp-atomic-test");
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"{\"segments\":[]}").unwrap();
            drop(f);
            std::fs::rename(&tmp, watch_dir.join("index.json")).unwrap();
        });

        let started = Instant::now();
        let outcome = wait_for_change(&dir, &index_file, Duration::from_secs(5));
        let elapsed = started.elapsed();
        handle.join().unwrap();

        assert_eq!(
            outcome,
            WaitOutcome::Changed,
            "atomic tmp+rename over index.json must be detected as Changed: got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must not wait out the full timeout on a rename; took {elapsed:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Test 4: the `--ack-file` loop-hazard guard refuses a path inside the
    /// watched log dir (exact match and nested paths), and allows a sibling
    /// path outside it.
    #[test]
    fn ack_file_conflicts_with_log_dir_catches_the_feedback_loop() {
        let repo = PathBuf::from("/tmp/rally-ack-guard-test-repo");
        let log_dir = repo.join(".rally").join("log");

        assert!(
            ack_file_conflicts_with_log_dir(
                &repo,
                &log_dir,
                &PathBuf::from(".rally/log/watch-acks.jsonl")
            ),
            "a relative ack-file nested under the log dir must be refused"
        );
        assert!(
            ack_file_conflicts_with_log_dir(
                &repo,
                &log_dir,
                &repo.join(".rally/log/nested/watch-acks.jsonl")
            ),
            "an absolute ack-file nested under the log dir must be refused"
        );
        assert!(
            ack_file_conflicts_with_log_dir(&repo, &log_dir, &log_dir),
            "an ack-file that IS the log dir itself must be refused"
        );
        assert!(
            !ack_file_conflicts_with_log_dir(
                &repo,
                &log_dir,
                &PathBuf::from(".rally/watch-acks.jsonl")
            ),
            "the documented sibling path .rally/watch-acks.jsonl must be allowed"
        );
        assert!(
            !ack_file_conflicts_with_log_dir(
                &repo,
                &log_dir,
                &PathBuf::from(".rally/log-archive/acks.jsonl")
            ),
            "a directory that merely shares the `log` PREFIX (log-archive) must not be \
             treated as nested under log/ — this pins the guard to path components, not \
             a string prefix check"
        );
    }

    #[test]
    fn backend_name_is_one_of_the_documented_values() {
        let name = backend_name();
        assert!(
            matches!(name, "kqueue" | "inotify" | "poll"),
            "backend_name() must be one of kqueue|inotify|poll; got {name}"
        );
    }
}
