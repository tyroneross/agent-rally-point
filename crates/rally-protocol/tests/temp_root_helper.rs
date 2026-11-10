// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Tests for the shared test scratch-folder helper, `crates/test-support/temp_root.rs`.
//!
//! They live in this crate because it is the smallest one, so the helper's own
//! checks compile and run in seconds.
//!
//! The process-level cases re-run this test binary as a child. A child is the
//! only honest way to grade "what happens when the owner is killed": the
//! kernel releases the ownership lock only when a real process ends.

#[path = "../../test-support/temp_root.rs"]
mod temp_root;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use temp_root::TempRoot;

/// Set in a child to select its role. Absent in a normal test run.
const CHILD_ROLE: &str = "RALLY_TEMP_ROOT_TEST_CHILD";
const AREA_PREFIX: &str = "AREA=";

#[test]
fn a_root_exists_while_held_and_is_gone_after_drop() {
    let root = TempRoot::new("held");
    let path = root.path().to_path_buf();
    assert!(path.is_dir(), "a new root must exist: {}", path.display());
    std::fs::write(root.join("file.txt"), b"x").expect("write inside the root");
    drop(root);
    assert!(!path.exists(), "drop must remove the root and its contents");
}

#[test]
fn a_root_is_removed_when_the_test_panics() {
    let seen = std::sync::Mutex::new(None::<PathBuf>);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let root = TempRoot::new("panics");
        *seen.lock().unwrap() = Some(root.path().to_path_buf());
        panic!("simulated test failure");
    }));
    assert!(result.is_err(), "the closure must have panicked");
    let path = seen.lock().unwrap().clone().expect("root path recorded");
    assert!(
        !path.exists(),
        "a failed test must not leave its root behind: {}",
        path.display()
    );
}

#[test]
fn two_roots_with_one_label_do_not_collide() {
    let a = TempRoot::new("same");
    let b = TempRoot::new("same");
    assert_ne!(a.path(), b.path());
    assert!(a.path().is_dir() && b.path().is_dir());
}

#[test]
fn roots_stay_under_the_system_temp_directory() {
    let root = TempRoot::new("contained");
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp"); // temp-root-read: containment check
    let real = std::fs::canonicalize(root.path()).expect("canonical root");
    assert!(
        real.starts_with(&temp),
        "{} must be inside {}",
        real.display(),
        temp.display()
    );
}

/// The short variant exists for socket addresses, so its whole point is the
/// length of the path it returns.
#[test]
#[cfg(unix)]
fn a_short_root_is_short_and_is_removed_on_drop() {
    let root = TempRoot::short("sock");
    let path = root.path().to_path_buf();
    assert!(path.is_dir());
    let canonical = std::fs::canonicalize(&path).expect("canonical short root");
    // A macOS socket address holds 103 bytes. Leave room for the longest
    // suffix this repo hangs off a HOME: `/h/.local/share/rally/ptyd.sock`.
    let longest_suffix = "/h/.local/share/rally/ptyd.sock".len();
    assert!(
        canonical.as_os_str().len() + longest_suffix <= 103,
        "{} leaves no room for a socket path",
        canonical.display()
    );
    drop(root);
    assert!(!path.exists(), "drop must remove a short root too");
}

/// A label can never fail a test or point outside the process area.
#[test]
fn a_hostile_or_overlong_label_is_made_safe() {
    let area_of = |root: &TempRoot| root.path().parent().expect("process area").to_path_buf();
    let plain = TempRoot::new("plain");
    let area = area_of(&plain);

    for label in [
        "a/b",
        "../../escape",
        "..",
        "",
        ".hidden",
        "sp ace\u{e9}",
        "/abs",
    ] {
        let root = TempRoot::new(label);
        assert!(root.path().is_dir(), "{label:?} must still yield a folder");
        assert_eq!(
            area_of(&root),
            area,
            "{label:?} must stay directly inside the process area: {}",
            root.path().display()
        );
        let name = root
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            !name.starts_with('.'),
            "{label:?} gave a hidden name {name:?}"
        );
    }

    let long = "x".repeat(200);
    let root = TempRoot::new(&long);
    let name = root
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        name.len() <= 48 + 1 + 20,
        "a 200-character label must be cut down, got {} characters",
        name.len()
    );
}

/// Child entry point. In a normal run it returns at once.
///
/// * `hold`: create one root, report the process area, then wait for stdin to
///   close. Killed children never reach the end, which is the point.
/// * `scoped`: create one process-scoped root, report the area, exit.
/// * `sweep`: create and drop one root. Its first use sweeps abandoned areas.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var(CHILD_ROLE) else {
        return;
    };
    match role.as_str() {
        "hold" => {
            let root = TempRoot::new("hold");
            report_area(root.path());
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
        }
        "hold-short" => {
            let root = TempRoot::short("hold");
            report_area(root.path());
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
        }
        "sweep-short" => {
            let root = TempRoot::short("sweep");
            report_area(root.path());
        }
        "keep" => {
            let root = TempRoot::new("evidence");
            std::fs::write(root.join("log.txt"), b"evidence").expect("write evidence");
            report_area(root.path());
            let _ = root.keep();
        }
        "heal" => {
            let first = TempRoot::new("first");
            let area = first.path().parent().expect("process area").to_path_buf();
            report_area(first.path());
            // Something outside the test removes the whole area.
            std::fs::remove_dir_all(&area).expect("remove area from outside");
            let again = TempRoot::new("again");
            assert!(again.path().is_dir(), "the helper must put its area back");
            println!("HEALED");
        }
        "scoped" => {
            let path = TempRoot::process_scoped("scoped");
            report_area(path);
        }
        "sweep" => {
            let root = TempRoot::new("sweep");
            report_area(root.path());
        }
        other => panic!("unknown child role {other:?}"),
    }
}

fn report_area(root: &Path) {
    let area = root.parent().expect("a root has a process area");
    println!("{AREA_PREFIX}{}", area.display());
}

struct Held {
    child: Child,
    stdout: BufReader<ChildStdout>,
    area: PathBuf,
}

fn spawn_child(role: &str) -> Held {
    let mut child = Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(CHILD_ROLE, role)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child test process");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let area = read_area(&mut stdout);
    Held {
        child,
        stdout,
        area,
    }
}

fn read_area(stdout: &mut BufReader<ChildStdout>) -> PathBuf {
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdout.read_line(&mut line).expect("read child output");
        assert!(read > 0, "child ended before reporting its area");
        if let Some(found) = line.find(AREA_PREFIX) {
            return PathBuf::from(line[found + AREA_PREFIX.len()..].trim());
        }
    }
}

fn lock_of(area: &Path) -> PathBuf {
    let mut name = area.file_name().expect("area name").to_os_string();
    name.push(".lock");
    area.with_file_name(name)
}

/// The contract a killed run depends on: the next test process removes what a
/// killed process left, and never touches a process that is still alive.
#[test]
fn a_killed_process_is_swept_and_a_live_one_is_left_alone() {
    const EACH: usize = 4;
    let mut doomed: Vec<Held> = (0..EACH).map(|_| spawn_child("hold")).collect();
    let living: Vec<Held> = (0..EACH).map(|_| spawn_child("hold")).collect();
    for held in doomed.iter().chain(living.iter()) {
        assert!(held.area.is_dir(), "{} must exist", held.area.display());
    }

    for held in &mut doomed {
        held.child.kill().expect("kill child");
        held.child.wait().expect("reap killed child");
    }
    for held in &doomed {
        assert!(
            held.area.is_dir(),
            "a killed process cannot clean up after itself; {} should still be there \
             or this test proves nothing",
            held.area.display()
        );
    }

    // Several sweepers at once: they must not fail each other, and between
    // them they must remove every abandoned area exactly once.
    let sweepers: Vec<Held> = (0..EACH).map(|_| spawn_child("sweep")).collect();
    for mut sweeper in sweepers {
        drop(sweeper.child.stdin.take());
        let mut rest = String::new();
        let _ = sweeper.stdout.read_to_string(&mut rest);
        assert!(sweeper.child.wait().expect("reap sweeper").success());
        assert!(
            !sweeper.area.exists(),
            "a process that dropped every root must remove its own area: {}",
            sweeper.area.display()
        );
    }

    for held in &doomed {
        assert!(
            !held.area.exists(),
            "the next test process must remove a killed process's area: {}",
            held.area.display()
        );
        assert!(
            !lock_of(&held.area).exists(),
            "and its lock file: {}",
            lock_of(&held.area).display()
        );
    }
    for held in &living {
        assert!(
            held.area.is_dir(),
            "a sweep must never remove a live process's area: {}",
            held.area.display()
        );
    }

    for mut held in living {
        drop(held.child.stdin.take());
        assert!(held.child.wait().expect("reap child").success());
        assert!(
            !held.area.exists(),
            "a process that exits normally removes its own area: {}",
            held.area.display()
        );
    }
}

/// The short area gets the same guarantee as the standard one: a killed
/// owner's folder is removed by the next process that uses a short root.
#[test]
#[cfg(unix)]
fn a_killed_process_short_area_is_swept_too() {
    let mut doomed = spawn_child("hold-short");
    let mut living = spawn_child("hold-short");
    doomed.child.kill().expect("kill child");
    doomed.child.wait().expect("reap killed child");
    assert!(doomed.area.is_dir(), "killed short area should still exist");

    let mut sweeper = spawn_child("sweep-short");
    drop(sweeper.child.stdin.take());
    assert!(sweeper.child.wait().expect("reap sweeper").success());

    assert!(
        !doomed.area.exists(),
        "the next short-root user must remove a killed process's short area: {}",
        doomed.area.display()
    );
    assert!(
        living.area.is_dir(),
        "and must leave a live one alone: {}",
        living.area.display()
    );
    drop(living.child.stdin.take());
    assert!(living.child.wait().expect("reap child").success());
    assert!(!living.area.exists(), "normal exit removes the short area");
}

/// A fixture that keeps failure evidence must still find it after later test
/// runs. `keep` leaves the folder in place and out of reach of every sweep.
#[test]
fn a_kept_root_survives_later_sweeps() {
    let mut keeper = spawn_child("keep");
    drop(keeper.child.stdin.take());
    assert!(keeper.child.wait().expect("reap keeper").success());
    let evidence = std::fs::read_dir(&keeper.area)
        .expect("kept area must still exist")
        .flatten()
        .map(|entry| entry.path().join("log.txt"))
        .find(|path| path.is_file())
        .expect("kept evidence file");

    for _ in 0..2 {
        let mut sweeper = spawn_child("sweep");
        drop(sweeper.child.stdin.take());
        assert!(sweeper.child.wait().expect("reap sweeper").success());
    }
    assert!(
        evidence.is_file(),
        "a kept folder must survive later test processes: {}",
        evidence.display()
    );
    assert!(
        lock_of(&keeper.area).exists(),
        "the lock file stays, reserving the kept area's name against process-id reuse"
    );

    // Someone inspects the evidence and removes the folder. The next sweep
    // then clears the reservation, so nothing is left behind for good.
    std::fs::remove_dir_all(&keeper.area).expect("remove kept area");
    let mut sweeper = spawn_child("sweep");
    drop(sweeper.child.stdin.take());
    assert!(sweeper.child.wait().expect("reap sweeper").success());
    assert!(
        !lock_of(&keeper.area).exists(),
        "once the kept folder is gone, the next sweep releases its name"
    );
}

/// A cleanup script or a person can remove the helper's folders while tests
/// run. One removal must not fail every later test in that process.
#[test]
fn an_area_removed_from_outside_is_put_back() {
    let mut child = spawn_child("heal");
    drop(child.child.stdin.take());
    let mut rest = String::new();
    child
        .stdout
        .read_to_string(&mut rest)
        .expect("read child output");
    assert!(
        child.child.wait().expect("reap child").success(),
        "allocating after the area was removed must work: {rest}"
    );
    assert!(
        rest.contains("HEALED"),
        "child did not reach the second allocation: {rest}"
    );
}

/// A once-per-process fixture outlives every test, so it cannot be removed at
/// exit. The next test process must remove it.
#[test]
fn a_process_scoped_root_is_swept_by_the_next_process() {
    let mut scoped = spawn_child("scoped");
    drop(scoped.child.stdin.take());
    assert!(scoped.child.wait().expect("reap scoped child").success());
    assert!(
        scoped.area.is_dir(),
        "a process-scoped root is left at exit; {} should still be there",
        scoped.area.display()
    );

    let mut sweeper = spawn_child("sweep");
    drop(sweeper.child.stdin.take());
    assert!(sweeper.child.wait().expect("reap sweeper").success());
    assert!(
        !scoped.area.exists(),
        "the next test process must remove it: {}",
        scoped.area.display()
    );
}
