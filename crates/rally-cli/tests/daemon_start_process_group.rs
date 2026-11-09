// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Explicit daemon start must survive cleanup of the short-lived launcher group.

#![cfg(unix)]

#[path = "../../test-support/temp_root.rs"]
mod temp_root;

use temp_root::TempRoot;

use serde_json::Value;
use std::cell::Cell;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/rally_cmd.rs"]
mod rally_cmd;

struct Room {
    root: PathBuf,
    home: PathBuf,
    may_stop: Cell<bool>,
    _roots: [TempRoot; 1],
}

impl Room {
    fn new() -> Self {
        let root_guard = TempRoot::new("rally-start-group");
        let root = root_guard.path().to_path_buf();
        let home = root.join("home");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            root,
            home,
            may_stop: Cell::new(false),
            _roots: [root_guard],
        }
    }

    fn command(&self) -> Command {
        let mut cmd = rally_cmd::rally_command();
        cmd.current_dir(&self.root)
            .env("HOME", &self.home)
            .env("RALLY_DAEMON_AUTOSTART", "0")
            .env("RALLY_NO_AUTO_REAP", "1");
        for key in [
            "GITHUB_ACTIONS",
            "GITHUB_RUN_ID",
            "RALLY_TOOL_ID",
            "RALLY_SESSION_ID",
            "RALLY_MANAGED_SESSION_MODE",
            "RALLY_ENGAGEMENT",
            "RALLY_REPO_ROOT",
            "EASY_TERMINAL_PANE_ID",
            "PTYD_SOCKET_PATH",
            "RALLY_PTYD_SOCKET",
        ] {
            cmd.env_remove(key);
        }
        cmd
    }

    fn status(&self) -> Value {
        let out = self
            .command()
            .args(["daemon", "status", "--json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "daemon status: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        // Only the compatible CLI scoped to this disposable room may stop its daemon.
        if self.may_stop.get() {
            let _ = self.command().args(["daemon", "stop", "--json"]).output();
        }
    }
}

#[test]
fn explicit_start_survives_launcher_process_group_cleanup() {
    let room = Room::new();
    let launcher = room
        .command()
        .args(["daemon", "start", "--idle-exit-secs", "30", "--json"])
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let launcher_group = i32::try_from(launcher.id()).unwrap();
    assert!(launcher_group > 1);
    // SAFETY: getpgrp only queries this test's process group. Never signal it.
    assert_ne!(launcher_group, unsafe { libc::getpgrp() });
    let started = launcher.wait_with_output().unwrap();
    assert!(
        started.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let body: Value = serde_json::from_slice(&started.stdout).unwrap();
    let state = &body["data"]["daemon"];
    assert_eq!(state["live"], true);
    assert_eq!(
        PathBuf::from(state["repo_root"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        room.root.canonicalize().unwrap()
    );
    room.may_stop.set(true);
    let daemon_pid = i32::try_from(state["pid"].as_u64().unwrap()).unwrap();
    assert!(daemon_pid > 1 && daemon_pid != launcher_group);
    // SAFETY: query only the exact daemon PID returned for this fixture room.
    let daemon_group = unsafe { libc::getpgid(daemon_pid) };
    assert!(
        daemon_group > 1,
        "fixture daemon exited before group cleanup"
    );

    if daemon_group == launcher_group {
        // Baseline: daemon inherited the isolated launcher group. It is safe to
        // signal because that exact fixture daemon still owns this known group;
        // no parent/test/terminal group is eligible for this call.
        // SAFETY: validated positive group of our known isolated fixture child.
        assert_eq!(unsafe { libc::kill(-launcher_group, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        while room.status()["data"]["daemon"]["live"] == true && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    } else {
        // Detached daemon has its own group. Do not signal an unrelated group
        // if a PID was reused after reaping the launcher; require the old group
        // to be gone instead of assuming that numeric group is still ours.
        assert_eq!(daemon_group, daemon_pid);
        // SAFETY: signal 0 queries existence without delivering a signal.
        assert_eq!(unsafe { libc::kill(-launcher_group, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    let status = room.status();
    assert_eq!(
        status["data"]["daemon"]["live"], true,
        "explicitly detached daemon died when its launcher process group was cleaned up"
    );
    assert_eq!(
        status["data"]["daemon"]["pid"].as_i64(),
        Some(i64::from(daemon_pid))
    );
}
