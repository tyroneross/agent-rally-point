// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! When the store router gives up, it must name the contender.
//!
//! # The failure these tests exist for
//!
//! `RoomStore::route` waits `watchdog_remaining() - 250ms` for either a live
//! daemon route or exclusive direct ownership, then refuses. Because the wait
//! is derived from the caller's own budget, raising the budget makes that
//! refusal slower and never rarer — measured 1 failing run in 20 at a 3,000ms
//! budget and the same 1 in 20 at 30,000ms
//! (`AGEN-RALLY-CLI-DURABILITY-m1dn22wz37g8c73m747d3`).
//!
//! Three separate diagnoses of that flake were reached by reading code, and the
//! reason nobody could reach one by observation is that `flock` is anonymous: a
//! waiter learns `EWOULDBLOCK` and nothing else. The refusal named a duration
//! and told the operator to run `rally daemon status` — which runs after the
//! contender has exited, and therefore reports a healthy room. A 30-second
//! stall left no evidence of who caused it.
//!
//! So the EX holder stamps its identity into the lock file it holds, and the
//! waiter reads it back. These tests hold the two locks from known processes
//! and assert the refusal names them.

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "support/rally_cmd.rs"]
mod rally_cmd;

use rally_cmd::rally_command;

/// Budget for the command under test.
///
/// Small on purpose. These tests do not measure how long contention takes to
/// clear; they assert what the refusal SAYS, and the refusal arrives one
/// `direct_owner_wait_bound()` after the command starts. Two seconds keeps each
/// case under ~2s while staying clear of the CLI's 100ms clamp floor.
const CONTENDED_BUDGET_MS: &str = "2000";

// `flock(2)`, hand-declared exactly as `store.rs` declares it — the crate keeps
// a zero-extra-dependency contract, and the test binary must be able to hold a
// lock the CLI child will then lose to.
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

static ROOM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Room {
    cwd: PathBuf,
    home: PathBuf,
}

impl Room {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = ROOM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cwd = std::env::temp_dir().join(format!("rca-{pid}-{nanos}-{unique}-cwd"));
        let home = std::env::temp_dir().join(format!("rca-{pid}-{nanos}-{unique}-home"));
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".rally")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn rally_dir(&self) -> PathBuf {
        self.cwd.join(".rally")
    }

    /// Run a mutating command under a deliberately small watchdog budget.
    ///
    /// `say artifact` is the shape the production stall was observed on: a
    /// mutating command whose durable append never commits because the router
    /// never resolves a store.
    fn contended_say(&self) -> Output {
        rally_command()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            // Overrides the harness's 30s budget set by `rally_command()`.
            // Explicit, because a 30s wait would make each case a 30s test
            // while proving nothing extra.
            .env("RALLY_HOOK_TIMEOUT_MS", CONTENDED_BUDGET_MS)
            .args([
                "say",
                "artifact",
                "--tool",
                "codex:01",
                "--subject",
                "contended",
                "--json",
            ])
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        rally_command()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("RALLY_HOOKS", "off")
            .args(args)
            .output()
            .unwrap()
    }

    /// The refusal text, proven to be a typed refusal and not a watchdog
    /// envelope (a watchdog envelope would mean the budget, not the router,
    /// ended the command — a different failure that must not read as this one).
    fn refusal_text(out: &Output) -> String {
        let bytes = if out.stdout.is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let body: Value = serde_json::from_slice(bytes)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(bytes)));
        assert_ne!(
            body.get("command").and_then(Value::as_str),
            Some("watchdog"),
            "the command hit its wall-clock watchdog instead of the router's own \
             bounded refusal, so this assertion would be about the budget rather \
             than about contention attribution\nenvelope: {body}"
        );
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("expected a string refusal\nenvelope: {body}"))
            .to_string()
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

struct Daemon(Child);

impl Daemon {
    /// Start a daemon and wait until it actually answers, so the test never
    /// races the startup window it is not measuring.
    ///
    /// Readiness is `data.daemon.live == true`, NOT the exit status of
    /// `daemon status`. `daemon status` reports a room with no daemon in it as
    /// `{"ok": true, "data": {"daemon": {"live": false, "note": "not
    /// running"}}}` and exits 0 — correctly, because failing to find a daemon
    /// is not a command failure. A readiness gate written against the exit
    /// status therefore passes on its FIRST poll, before the daemon has bound
    /// anything, and hands the test straight into the startup window it meant
    /// to skip.
    fn start(room: &Room) -> Self {
        let child = rally_command()
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_HOOKS", "off")
            .args(["daemon", "serve", "--idle-exit-secs", "120"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start rally daemon");
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let out = room.run(&["daemon", "status", "--json"]);
            if rally_cmd::daemon_is_live(&out.stdout) {
                return daemon;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("rally daemon did not start");
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// A daemon holding the room whose socket does not answer is the shape the
/// production stall matches: the daemon-exclusion SH lock is correctly refused,
/// so direct opening is forbidden, and the probe cannot route either.
///
/// Seeded deterministically by removing the socket the daemon published, which
/// leaves the EX hold and the discovery pointer exactly as a mid-startup or
/// wedged daemon leaves them.
#[test]
fn a_daemon_holding_the_room_without_an_answering_socket_is_named_by_pid() {
    let room = Room::new();
    let daemon = Daemon::start(&room);
    let socket = PathBuf::from(
        fs::read_to_string(room.rally_dir().join("rallyd.sock.addr"))
            .expect("daemon published a discovery pointer")
            .trim(),
    );
    fs::remove_file(&socket).expect("remove the published socket");

    let message = Room::refusal_text(&room.contended_say());

    assert!(
        message.contains("direct-store-busy-unknown"),
        "expected the router's bounded refusal; got: {message}"
    );
    assert!(
        message.contains("blocked on rallyd.owner.lock"),
        "the refusal must name WHICH lock blocked, not just that something did; got: {message}"
    );
    assert!(
        message.contains(&format!("pid={}", daemon.pid())),
        "the refusal must name the contending daemon's pid {}; got: {message}",
        daemon.pid()
    );
    assert!(
        message.contains("role=daemon-owner") && message.contains("verb=daemon-serve"),
        "the refusal must say what the contender IS, so a reader need not guess \
         whether a daemon or a peer command held the room; got: {message}"
    );
    assert!(
        message.contains("(alive)"),
        "a live contender must not read the same as a stale stamp; got: {message}"
    );
}

/// The other branch: a peer DIRECT command owns the room. Held here from the
/// test process, which never stamps, so the newest stamp on the file belongs to
/// an earlier command that has since exited.
///
/// That is the case a naive reader of the lock body would get wrong — it would
/// report a long-gone pid as the contender. The liveness probe is what keeps
/// the report honest, and this asserts it fires.
#[test]
fn a_stale_stamp_is_reported_as_stale_rather_than_as_the_contender() {
    let room = Room::new();
    let first = room.run(&["enter", "--tool", "codex:01", "--json"]);
    assert!(
        first.status.success(),
        "setup command failed: {}",
        String::from_utf8_lossy(&first.stdout)
    );
    let stamp = fs::read_to_string(room.rally_dir().join("direct.owner.lock"))
        .expect("the first command stamped the direct-owner lock");
    let stale_pid = stamp
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .expect("stamp carries a pid")
        .to_string();

    // Hold direct EX from this process for the duration of the child command.
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(room.rally_dir().join("direct.owner.lock"))
        .unwrap();
    assert_eq!(
        unsafe { flock(held.as_raw_fd(), LOCK_EX | LOCK_NB) },
        0,
        "the exited command must have released direct EX"
    );

    let message = Room::refusal_text(&room.contended_say());
    drop(held);

    assert!(
        message.contains("blocked on direct.owner.lock"),
        "expected the direct-ownership branch; got: {message}"
    );
    assert!(
        message.contains(&format!("pid={stale_pid}")) && message.contains("STALE"),
        "the stamp from exited pid {stale_pid} must be reported as stale, never as \
         the live contender; got: {message}"
    );
}

/// The router must lose to nothing — least of all to its own probe.
///
/// # What this is really asserting
///
/// `direct_owner_wait_bound()` is `watchdog_remaining() - 250ms`. That 250ms
/// reserve exists for one reason: so the router refuses with its own typed
/// `direct-store-busy-unknown`, which names the contender, rather than being cut
/// off by the wall-clock watchdog, which names nothing.
///
/// The reserve did not hold. The router checked its deadline only AFTER each
/// probe, and one probe against a socket that accepts and never answers costs
/// the full 3s `PROBE_TIMEOUT` — measured at 3008ms. So the deadline landed
/// mid-probe and the watchdog fired first. Measured against this same seeded
/// state before the clamp: a 2,000ms budget produced
/// `watchdog-timeout-uncommitted-mutation` every time, and an 8,000ms budget did
/// too, because 8,000 falls between the probe boundaries at 6.0s and 9.0s.
///
/// That is why every report of this failure arrived as a watchdog envelope with
/// no attribution, and why three diagnoses of it had to be guessed from code.
/// A 2,000ms budget is used here because it makes the pre-clamp failure
/// deterministic rather than dependent on where the deadline falls on the 3s
/// probe grid: one unclamped probe (3008ms) always outlives it.
#[test]
fn the_router_refuses_within_its_own_bound_instead_of_losing_to_the_watchdog() {
    let room = Room::new();

    // Bind a socket and never answer on it. A daemon mid-startup presents
    // exactly this: it has bound and published its socket but its accept loop
    // is not yet serving, so a connect succeeds and a ping is never answered.
    let socket_path = room.cwd.join("silent.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    fs::write(
        room.rally_dir().join("rallyd.sock.addr"),
        socket_path.to_string_lossy().as_bytes(),
    )
    .unwrap();

    // Hold the daemon-owner lock EX, so the daemon-exclusion SH try is refused
    // and direct opening stays correctly forbidden for the whole wait.
    let owner = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(room.rally_dir().join("rallyd.owner.lock"))
        .unwrap();
    assert_eq!(unsafe { flock(owner.as_raw_fd(), LOCK_EX | LOCK_NB) }, 0);

    let out = room.contended_say();
    drop(owner);
    drop(listener);

    // The discriminator is WHICH mechanism ended the command, and it is read
    // from the envelope rather than from wall clock: the budget is measured
    // inside the process, while a test's own timing also carries process spawn,
    // which is neither the router's nor the watchdog's.
    //
    // `refusal_text` fails loudly on a watchdog envelope, which IS the
    // regression — pre-clamp, this command died on the watchdog every time.
    let message = Room::refusal_text(&out);
    assert!(
        message.contains("direct-store-busy-unknown"),
        "got: {message}"
    );
    assert!(
        message.contains("blocked on rallyd.owner.lock"),
        "the refusal must still name the contended lock; got: {message}"
    );
    // The bound the router reports must be the one derived from THIS budget,
    // proving the refusal came from the wait under test and not from some other
    // bounded path. Compared as a range, not an equality: the bound is
    // `watchdog_remaining() - 250ms` read a few milliseconds into the command,
    // so it lands just under `budget - 250`, never exactly on it.
    let ceiling = CONTENDED_BUDGET_MS.parse::<u64>().unwrap() - 250;
    let reported: u64 = message
        .split("route within ")
        .nth(1)
        .and_then(|rest| rest.split("ms").next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("refusal did not report a bound; got: {message}"));
    assert!(
        reported <= ceiling && reported > ceiling - 200,
        "reported bound {reported}ms is not the one derived from a \
         {CONTENDED_BUDGET_MS}ms budget (expected just under {ceiling}ms); got: {message}"
    );
}

/// No daemon has ever run here, so there is nothing to route to. The report
/// must say that rather than leave a reader wondering whether a daemon was
/// involved — "no pointer" and "pointer to a dead socket" are different rooms
/// to debug.
#[test]
fn a_room_with_no_daemon_pointer_says_so() {
    let room = Room::new();
    let held = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(room.rally_dir().join("direct.owner.lock"))
        .unwrap();
    assert_eq!(unsafe { flock(held.as_raw_fd(), LOCK_EX | LOCK_NB) }, 0);

    let message = Room::refusal_text(&room.contended_say());
    drop(held);

    assert!(
        message.contains("rallyd.sock.addr absent"),
        "expected the no-daemon-pointer state to be named; got: {message}"
    );
    assert!(
        message.contains("direct.owner.lock carries no holder stamp"),
        "an unstamped lock must be reported as unstamped, not silently omitted; \
         got: {message}"
    );
}
