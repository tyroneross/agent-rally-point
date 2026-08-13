// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! rallyd handover / fail-open / crash-release invariants (BACKLOG S-P3,
//! Chunk D, ADR-01). These tests drive the REAL `rally` binary as
//! subprocesses (`CARGO_BIN_EXE_rally`) plus direct kernel-level `flock` on
//! `.rally/rallyd.owner.lock`, so every assertion is against the shipped
//! artifact's observable behavior — never against `pub(crate)` internals the
//! integration-test crate cannot see.
//!
//! Coverage (Spec Object T-ids):
//!
//! * T-03 — handover: a routed client opens NO facts.db fd; `daemon stop`
//!   releases the lock so the direct path resumes; a fresh `daemon serve`
//!   EX-blocks until a live direct SH holder exits.
//! * T-04 — fail-open: no daemon (and a STALE `.sock.addr`) behave as the
//!   direct, no-daemon path.
//! * T-05 — engagement scoping: a routed append with `RALLY_ENGAGEMENT=X`
//!   lands in segment `X` (the daemon applies `set_engagement_scope` per
//!   request).
//! * T-06 — crash-release: SIGKILL the daemon; the kernel releases EX; the
//!   next client fails open; facts.db rebuilds from the ledger.
//! * T-08 — wedged corridor: SH refused AND no ping within the corridor bound
//!   fails LOUD naming the remedy, and NEVER writes directly.
//! * T-09 — mid-command disconnect: losing the routed transport yields a
//!   concrete transport error (exit 1), never a false daemon-death claim or a
//!   direct facts.db open.
//!
//! These run on macOS locally (the owner-lock handover is flock-based; the
//! `lsof` fd assertions assume a unix host). They are NOT part of the docker
//! #50 hammer — that runs the two daemon-serving concurrency tests only.

#![cfg(unix)]
// The daemon/holder children are always reaped — via `DaemonHandle`'s `Drop`
// (SIGTERM + `wait`), via `try_wait` polling loops, or via explicit `wait` — but
// clippy's `zombie_processes` heuristic cannot follow Drop-based / try_wait-based
// reaping through a wrapper struct, so it false-positives here. The reaping is
// real (no orphaned daemons past a test); the lint is scoped off for this fixture.
#![allow(clippy::zombie_processes)]

use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rally_protocol::store_wire::WIRE_VERSION;
use serde_json::Value;

// ── flock / kill: hand-declared externs, mirroring store.rs's own no-`libc`
//    pattern. Used to (a) simulate a wedged EX holder for T-08 and (b) send a
//    graceful SIGTERM / hard SIGKILL to the daemon child.
const LOCK_EX: i32 = 2;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn flock(fd: i32, op: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

fn send_signal(pid: u32, sig: i32) {
    // SAFETY: `kill(2)` with a plain pid + signal number; the pid is our own
    // spawned child (or 0/negative are never passed here).
    unsafe {
        kill(pid as i32, sig);
    }
}

// ── Temp room: a `.git`-anchored repo root + isolated HOME, cleaned on Drop.

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("rallyd-handover-{name}-cwd"));
        let home = temp_path(&format!("rallyd-handover-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(cwd.join(".rally")).expect("create temp .rally");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    fn rally_dir(&self) -> PathBuf {
        self.cwd.join(".rally")
    }

    /// A `rally` command bound to this room (cwd + isolated HOME).
    fn rally(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rally"));
        cmd.current_dir(&self.cwd).env("HOME", &self.home);
        cmd
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

// ── Daemon fixture: spawn `rally daemon serve` as a DETACHED child (do NOT
//    wait), poll for `.sock.addr` + a live `daemon status` ping, then SIGTERM +
//    wait on teardown. D1 makes `daemon serve` bypass the hook watchdog, so it
//    serves until signalled and won't self-terminate mid-test.

struct DaemonHandle {
    child: Child,
    cwd: PathBuf,
    home: PathBuf,
    stopped: bool,
}

impl DaemonHandle {
    /// Start the daemon on `room` and block until it answers a ping (dispatcher
    /// live — R3). `idle_exit_secs` is a self-heal backstop against an orphaned
    /// daemon if a test panics before teardown.
    fn start(room: &TempRoom) -> Self {
        Self::start_at(&room.cwd, &room.home)
    }

    fn start_at(cwd: &Path, home: &Path) -> Self {
        let log = fs::File::create(cwd.join(".rally").join("rallyd-serve.log"))
            .expect("create daemon log");
        let child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(cwd)
            .env("HOME", home)
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn rally daemon serve");

        let handle = DaemonHandle {
            child,
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            stopped: false,
        };

        // Block on a live status ping. `.sock.addr` is written before the store
        // open, but a ping only round-trips once the dispatcher is up (R3).
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            if daemon_live(cwd, home) {
                return handle;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let log =
            fs::read_to_string(cwd.join(".rally").join("rallyd-serve.log")).unwrap_or_default();
        panic!("daemon never became ready; serve log:\n{log}");
    }

    /// Spawn the serve child WITHOUT blocking on readiness — used by T-03c,
    /// which must observe the not-yet-ready window while a direct SH holder
    /// still lives (the daemon's EX acquire blocks behind that SH).
    fn start_at_unready(room: &TempRoom) -> Self {
        let log =
            fs::File::create(room.rally_dir().join("rallyd-serve.log")).expect("create daemon log");
        let child = Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .args(["daemon", "serve", "--idle-exit-secs", "180"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn rally daemon serve (unready)");
        DaemonHandle {
            child,
            cwd: room.cwd.clone(),
            home: room.home.clone(),
            stopped: false,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Graceful SIGTERM + wait. Idempotent.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        send_signal(self.child.id(), SIGTERM);
        // Bounded wait for graceful exit, then hard-kill as a backstop.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    send_signal(self.child.id(), SIGKILL);
                    let _ = self.child.wait();
                    break;
                }
            }
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        // Backstop only — tests call `stop()` explicitly where teardown timing
        // matters. Never leak a serving daemon past the test.
        send_signal(self.child.id(), SIGTERM);
        let _ = self.child.wait();
        let _ = fs::remove_file(self.cwd.join(".rally").join("rallyd.sock.addr"));
        let _ = fs::remove_file(self.cwd.join(".rally").join("rallyd.pid"));
        let _ = &self.home; // retained for symmetry / future diagnostics.
    }
}

// ── Small subprocess helpers.

fn daemon_live(cwd: &Path, home: &Path) -> bool {
    let out = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(cwd)
        .env("HOME", home)
        .args(["daemon", "status", "--json"])
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .map(|v| v["data"]["daemon"]["live"] == Value::Bool(true))
        .unwrap_or(false)
}

/// Append one handoff fact through whichever path is live (routed if a daemon
/// serves, direct otherwise). Returns the process `Output`.
fn say_handoff(room: &TempRoom, subject: &str) -> std::process::Output {
    room.rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            subject,
            "--json",
        ])
        .output()
        .expect("spawn rally say handoff")
}

/// Count open-handoff facts bearing `subject` in the room projection. Reads via
/// whichever path is live at call time.
fn room_handoff_count(room: &TempRoom, subject: &str) -> usize {
    let out = room
        .rally()
        .args(["room", "--json", "--timeout-ms", "15000"])
        .output()
        .expect("spawn rally room");
    assert!(
        out.status.success(),
        "rally room failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let room_json: Value = serde_json::from_slice(&out.stdout).expect("room stdout must be JSON");
    match room_json["data"]["room"]["open_handoffs"].as_array() {
        Some(handoffs) => handoffs
            .iter()
            .filter(|f| f["subject"] == Value::String(subject.to_string()))
            .count(),
        None => 0,
    }
}

/// True if `lsof` reports the process holding an fd on any `facts.db`. Returns
/// `None` when `lsof` is unavailable or the process is already gone.
fn process_has_factsdb_fd(pid: u32) -> Option<bool> {
    let out = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    Some(listing.contains("facts.db"))
}

// ── T-04 — fail-open: no daemon, and a stale `.sock.addr`, both behave as the
//    direct, no-daemon path (byte-identity is proven by the full suite under
//    T-02; here we assert the fail-open behavior + stale-addr tolerance).

#[test]
fn t04_fail_open_no_daemon_and_stale_addr() {
    let room = TempRoom::new("t04-failopen");

    // No daemon: a direct append succeeds and is visible on replay.
    let out = say_handoff(&room, "t04-plain");
    assert!(
        out.status.success(),
        "no-daemon say must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(room_handoff_count(&room, "t04-plain"), 1);

    // A STALE `.sock.addr` pointing at a socket that was never bound must be
    // treated as "no live daemon" (the probe fails, SH is acquirable) — NOT an
    // error, and NOT a routing attempt.
    let stale_socket = room.rally_dir().join("nonexistent-rallyd.sock");
    fs::write(
        room.rally_dir().join("rallyd.sock.addr"),
        stale_socket.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let out = say_handoff(&room, "t04-stale-addr");
    assert!(
        out.status.success(),
        "stale-addr say must fail open to direct; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(room_handoff_count(&room, "t04-stale-addr"), 1);
    // The earlier fact is still exactly-once present (no corruption).
    assert_eq!(room_handoff_count(&room, "t04-plain"), 1);
}

// ── T-03a — a ROUTED client opens NO facts.db fd. We keep the routed client
//    alive past its durable commit (post-commit block seam) so `lsof` can
//    sample its fds; a routed process never opens facts.db (G3).

#[test]
fn t03_routed_client_holds_no_factsdb_fd() {
    let room = TempRoom::new("t03-lsof");
    let mut daemon = DaemonHandle::start(&room);

    // Long-lived routed append: `--timeout-ms 40000` keeps the hook watchdog
    // from pre-empting, and the post-commit block keeps the CLIENT process
    // alive ~2.5s AFTER the append durably lands over the wire, giving `lsof`
    // a window. The block seam is client-side, so it fires identically whether
    // the store op routes or opens direct.
    let mut child = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            "t03-routed",
            "--json",
            "--timeout-ms",
            "40000",
        ])
        .env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", "2500")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn routed say child");

    // Sample lsof across the post-commit window: the routed client must NEVER
    // hold a facts.db fd. Require at least one live sample (proves we actually
    // observed the process, not a race where it exited first).
    let mut observed_alive = false;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        match process_has_factsdb_fd(child.id()) {
            Some(true) => panic!("routed client opened a facts.db fd (G3 violation)"),
            Some(false) => observed_alive = true,
            None => {}
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = child.wait().expect("routed say child joins");
    assert!(status.success(), "routed say must succeed");
    // `lsof` should be present on macOS; if it wasn't, we degrade to the
    // behavioral proof below rather than failing the invariant test.
    if !observed_alive {
        eprintln!("note: lsof unavailable or process too fast; relying on routing-behavior proof");
    }

    // Behavioral proof that the append went THROUGH the daemon: stop the
    // daemon, then the fact is present on the direct replay (the daemon wrote
    // the segment; the client never could have, EX excludes its SH).
    daemon.stop();
    assert_eq!(
        room_handoff_count(&room, "t03-routed"),
        1,
        "routed append must be durably present after daemon stop"
    );
}

// ── T-03b — `daemon stop` releases the lock; the direct path resumes.

#[test]
fn t03_stop_releases_lock_direct_path_resumes() {
    let room = TempRoom::new("t03-stop");
    let mut daemon = DaemonHandle::start(&room);

    // Routed append while the daemon serves.
    let out = say_handoff(&room, "t03-routed-first");
    assert!(out.status.success(), "routed say must succeed");

    // Stop the daemon: the EX lock releases (verified by `daemon stop`'s own
    // non-blocking EX probe before it returns).
    daemon.stop();
    drop(daemon);
    assert!(
        !daemon_live(&room.cwd, &room.home),
        "daemon must be gone after stop"
    );

    // A direct append (SH now acquirable) succeeds, and BOTH facts are present.
    let out = say_handoff(&room, "t03-direct-after-stop");
    assert!(
        out.status.success(),
        "direct say after stop must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(room_handoff_count(&room, "t03-routed-first"), 1);
    assert_eq!(room_handoff_count(&room, "t03-direct-after-stop"), 1);
}

// ── T-03c — a fresh `daemon serve` EX-blocks until a live direct SH holder
//    exits. The daemon writes `.sock.addr` only AFTER acquiring EX (startup
//    order), so the first successful ping can only happen after the holder's
//    SH is released.

#[test]
fn t03_daemon_ex_blocks_until_sh_holder_exits() {
    let room = TempRoom::new("t03-exblock");

    // A long-lived DIRECT SH holder: no daemon, so `say` takes the SH lock, and
    // the post-commit block keeps the process (and its SH) alive ~3s. A high
    // `--timeout-ms` stops the watchdog from killing it early.
    let mut holder = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            "t03-sh-holder",
            "--json",
            "--timeout-ms",
            "40000",
        ])
        .env("RALLY_TEST_BLOCK_AFTER_COMMIT_MS", "3000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn SH holder");

    // Let the holder get past its append (acquire + hold SH).
    std::thread::sleep(Duration::from_millis(800));

    // Now start a daemon. Its EX acquire blocks until the holder's SH releases,
    // so `.sock.addr` + first ping cannot appear until after the holder exits.
    let mut daemon = DaemonHandle::start_at_unready(&room);

    let mut holder_exited_at: Option<Instant> = None;
    let mut first_ping_at: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && (holder_exited_at.is_none() || first_ping_at.is_none()) {
        if holder_exited_at.is_none() && holder.try_wait().ok().flatten().is_some() {
            holder_exited_at = Some(Instant::now());
        }
        if first_ping_at.is_none() && daemon_live(&room.cwd, &room.home) {
            first_ping_at = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let holder_exited_at = holder_exited_at.expect("SH holder must exit within the window");
    let first_ping_at = first_ping_at.expect("daemon must eventually become ready");

    // The kernel EX/SH exclusion theorem, observed: the daemon did not serve a
    // ping until the direct SH holder had already released its lock. Allow a
    // small slack for the 50ms poll granularity.
    assert!(
        first_ping_at + Duration::from_millis(200) >= holder_exited_at,
        "daemon answered a ping BEFORE the SH holder released its lock — EX/SH exclusion violated"
    );

    daemon.stop();
}

// ── T-05 — engagement scoping: a routed append carrying `RALLY_ENGAGEMENT=X`
//    lands in segment `X` (the daemon applies `set_engagement_scope` per
//    request, never consulting its own process env — L9/R4).

#[test]
fn t05_engagement_scoping_through_daemon() {
    let room = TempRoom::new("t05-engagement");
    let mut daemon = DaemonHandle::start(&room);

    let engagement = "engscopealpha";
    let out = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            "t05-scoped",
            "--json",
        ])
        .env("RALLY_ENGAGEMENT", engagement)
        .output()
        .expect("spawn routed scoped say");
    assert!(
        out.status.success(),
        "routed scoped say must succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    daemon.stop();

    // The daemon must have written the append into the engagement-X segment.
    let segment = room
        .rally_dir()
        .join("log")
        .join(format!("{engagement}.jsonl"));
    assert!(
        segment.exists(),
        "engagement segment {} must exist (daemon applied set_engagement_scope)",
        segment.display()
    );
    let contents = fs::read_to_string(&segment).unwrap();
    assert!(
        contents.contains("t05-scoped"),
        "engagement segment must contain the routed append's subject; got:\n{contents}"
    );
}

// ── T-06 — crash-release: SIGKILL the daemon; the kernel releases EX; the next
//    client fails open; facts.db rebuilds from the ledger (disposability, F5).

#[test]
fn t06_sigkill_release_failopen_and_db_rebuild() {
    let room = TempRoom::new("t06-crash");
    let mut daemon = DaemonHandle::start(&room);

    let out = say_handoff(&room, "t06-before-crash");
    assert!(out.status.success(), "routed say must succeed");

    // Hard crash — no graceful cleanup, so the socket/.addr files linger, but
    // the kernel releases the EX lock.
    let pid = daemon.pid();
    send_signal(pid, SIGKILL);
    let _ = daemon.child.wait();
    daemon.stopped = true; // suppress the Drop SIGTERM (process already gone).

    // The next client: the stale `.addr` points at a dead socket (probe fails),
    // SH is now acquirable (EX released), so it fails open to the direct path.
    let out = say_handoff(&room, "t06-after-crash");
    assert!(
        out.status.success(),
        "post-crash say must fail open to direct; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(room_handoff_count(&room, "t06-before-crash"), 1);
    assert_eq!(room_handoff_count(&room, "t06-after-crash"), 1);

    // Disposability: delete the derived facts.db; the room still replays both
    // facts from the canonical JSONL ledger (D-01/F5).
    let db = room.rally_dir().join("facts.db");
    if db.exists() {
        fs::remove_file(&db).unwrap();
    }
    // Also drop the sqlite side-files so the rebuild is clean.
    for suffix in ["-wal", "-shm"] {
        let side = room.rally_dir().join(format!("facts.db{suffix}"));
        let _ = fs::remove_file(side);
    }
    assert_eq!(
        room_handoff_count(&room, "t06-before-crash"),
        1,
        "fact must survive a facts.db wipe (rebuilt from the ledger)"
    );
    assert_eq!(room_handoff_count(&room, "t06-after-crash"), 1);
}

// ── T-08 — wedged corridor: hold EX from THIS process (a stand-in for a
//    wedged daemon that acquired EX but never answered a ping), so a client's
//    SH try is refused and its probe never succeeds. After the 30s corridor
//    bound the client fails LOUD naming the remedy, and NEVER writes directly.
//
//    NOTE ON DURATION: `store_client::CORRIDOR_BOUND` (30s) is defined in a
//    Chunk-C file this Chunk-D slice does not own, so it cannot be shortened
//    from here. The client's `--timeout-ms 40000` keeps the hook watchdog from
//    pre-empting the corridor (max clamp is 60s), so this test spends ~30s
//    proving the fail-loud-never-direct invariant end to end.

#[test]
fn t08_wedged_daemon_fails_loud_never_direct() {
    let room = TempRoom::new("t08-wedged");

    // Acquire EX on the owner lock directly — indistinguishable, to the kernel,
    // from a daemon holding it. No socket is ever bound, so the client's probe
    // finds no live daemon while its SH try is refused: the exact wedged shape.
    let lock_path = room.rally_dir().join("rallyd.owner.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&lock_path)
        .expect("open owner lock");
    // SAFETY: flock(2) on our own fd; held until `lock_file` drops.
    let rc = unsafe { flock(lock_file.as_raw_fd(), LOCK_EX) };
    assert_eq!(rc, 0, "test must hold EX on the owner lock");

    let started = Instant::now();
    let out = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            "t08-wedged",
            "--json",
            "--timeout-ms",
            "40000",
        ])
        .output()
        .expect("spawn wedged-corridor say");
    let elapsed = started.elapsed();

    // Fail LOUD: non-zero exit, and the remedy text names the operator surface.
    assert!(
        !out.status.success(),
        "wedged corridor must fail loud, not succeed; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stderr}{}", String::from_utf8_lossy(&out.stdout));
    assert!(
        combined.contains("rally daemon status") || combined.contains("rally daemon stop"),
        "fail-loud error must name `rally daemon status`/`stop`; got: {combined}"
    );
    // It waited on the corridor rather than failing instantly (sanity: the
    // corridor was actually exercised, not short-circuited).
    assert!(
        elapsed >= Duration::from_secs(20),
        "expected the ~30s corridor to be exercised; elapsed={elapsed:?}"
    );

    // Release EX and prove NO direct write happened during the wedged corridor.
    drop(lock_file);
    assert_eq!(
        room_handoff_count(&room, "t08-wedged"),
        0,
        "the wedged corridor must NEVER open facts.db / write directly"
    );
}

// ── T-09 — mid-command transport loss (R6): a routed op whose socket answered
//    the liveness probe but then disconnected before dispatch must fail fast
//    with the concrete transport cause (exit 1), never assert that the daemon
//    died, and never open facts.db directly.
//
//    We reproduce that exact interleaving deterministically with a fake socket
//    the TEST controls: it answers ONE `Ping` with a valid `Pong` (so the
//    router's `probe_identity` returns live and constructs a RoutedRoomStore),
//    then stops accepting — so the FIRST dispatch connect is refused, which is
//    precisely the mid-command dead-socket case (R6). No real daemon and no
//    30s corridor: the router already committed to the routed path on the
//    probe, so a refused dispatch is a routed transport error, not a re-probe.

/// Spawn a fake rallyd socket that answers exactly one `Ping` with a valid
/// `Pong` for `canonical_repo_root`, then drops its listener so the next
/// connect is refused. Writes the `.sock.addr` pointer the client discovers.
/// Returns the join handle (the accept thread) so the test can await it.
fn spawn_one_ping_then_die(
    rally_dir: &Path,
    canonical_repo_root: &str,
) -> std::thread::JoinHandle<()> {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    // Short absolute socket path (well under the 103-byte sun_path limit).
    let socket_path = std::env::temp_dir().join(format!(
        "rally-t09-{}.sock",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind fake rallyd socket");
    fs::write(
        rally_dir.join("rallyd.sock.addr"),
        socket_path.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let pong = format!(
        r#"{{"ok":{{"kind":"pong","repo_root":{},"pid":424242,"wire_version":{}}}}}"#,
        serde_json::to_string(canonical_repo_root).unwrap(),
        WIRE_VERSION
    );
    std::thread::spawn(move || {
        // Answer the single probe ping, then let `listener` drop → subsequent
        // connects (the dispatch) get ECONNREFUSED.
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // the Ping request
            let mut w = &stream;
            let _ = w.write_all(pong.as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
        drop(listener);
        let _ = fs::remove_file(&socket_path);
    })
}

#[test]
fn t09_mid_command_transport_failure_never_direct() {
    let room = TempRoom::new("t09-middeath");
    let canonical = fs::canonicalize(&room.cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let server = spawn_one_ping_then_die(&room.rally_dir(), &canonical);

    // The routed op: probe succeeds (fake Pong), the router commits to routing,
    // then the FIRST dispatch connect is refused → transport error (exit 1).
    let out = room
        .rally()
        .args([
            "say",
            "handoff",
            "--tool",
            "codex",
            "--subject",
            "t09-after-death",
            "--json",
            "--timeout-ms",
            "40000",
        ])
        .output()
        .expect("spawn mid-death routed say");

    server.join().ok();

    // Exit 1 with the concrete transport classification (R6).
    assert_eq!(
        out.status.code(),
        Some(1),
        "mid-command dead socket must exit 1; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        combined.contains("daemon transport failure"),
        "must carry the routed transport failure; got: {combined}"
    );
    assert!(
        !combined.contains("daemon stopped"),
        "a disconnect must not be misreported as proven daemon death: {combined}"
    );
    assert!(
        !combined.contains("; retry"),
        "a routed transport loss must not invite a blind retry: {combined}"
    );

    // And it NEVER fell back to a direct facts.db write mid-command (G2/R6): no
    // fact landed. The `.addr` now points at a removed socket; a fresh read
    // path fails open to direct and finds nothing under this subject.
    let _ = fs::remove_file(room.rally_dir().join("rallyd.sock.addr"));
    assert_eq!(
        room_handoff_count(&room, "t09-after-death"),
        0,
        "R6: a routed op that lost its daemon mid-command must NOT write directly"
    );
}
