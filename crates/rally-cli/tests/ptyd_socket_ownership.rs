// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Socket-ownership invariant for `rally run --backend ptyd` autostart.
//!
//! Rally may start a ptyd daemon only on its own canonical socket
//! (`$HOME/.local/share/rally/ptyd.sock`) with no `RALLY_PTYD_SOCKET` override.
//! An overridden socket belongs to somebody else — Easy Terminal exports its
//! production socket there — and a second `ptyd server` bound to it wipes every
//! live workspace. Each case points `RALLY_PTYD_BIN` at a fake daemon that only
//! touches a marker file, so "was a daemon spawned" is observed directly.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const RALLY_BIN: &str = env!("CARGO_BIN_EXE_rally");
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    fake_ptyd: PathBuf,
    marker: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Short root: the canonical socket path must fit in sun_path (104 bytes).
        let root = PathBuf::from(format!("/tmp/rso{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("h");
        let cwd = root.join("cwd");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let marker = root.join("spawned");
        let fake_ptyd = root.join("fake-ptyd");
        fs::write(
            &fake_ptyd,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&fake_ptyd, fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox {
            root,
            home,
            cwd,
            fake_ptyd,
            marker,
        }
    }

    fn canonical_socket(&self) -> PathBuf {
        self.home.join(".local/share/rally/ptyd.sock")
    }

    fn run(&self, override_socket: Option<&Path>) -> Output {
        let mut cmd = Command::new(RALLY_BIN);
        cmd.args([
            "run",
            "claude",
            "--json",
            "--name",
            "owner-check",
            "--shared",
            "--backend",
            "ptyd",
        ])
        .current_dir(&self.cwd)
        .env("HOME", &self.home)
        .env("RALLY_PTYD_BIN", &self.fake_ptyd)
        .env("RALLY_HOOK_TIMEOUT_MS", "20000")
        .env_remove("PTYD_SOCKET_PATH")
        .env_remove("PWD");
        match override_socket {
            Some(sock) => cmd.env("RALLY_PTYD_SOCKET", sock),
            None => cmd.env_remove("RALLY_PTYD_SOCKET"),
        };
        cmd.output().expect("spawn rally")
    }

    fn spawned(&self) -> bool {
        self.marker.exists()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_refused(out: &Output, sb: &Sandbox, case: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "{case}: rally must fail, stderr: {stderr}"
    );
    assert!(
        stderr.contains("will not start one there"),
        "{case}: the error must say rally refuses to start a daemon; stderr: {stderr}"
    );
    assert!(!sb.spawned(), "{case}: no ptyd may be spawned");
}

#[test]
fn override_with_absent_socket_never_spawns() {
    let sb = Sandbox::new();
    let foreign = sb.root.join("foreign.sock");
    let out = sb.run(Some(&foreign));
    assert_refused(&out, &sb, "override + absent");
    assert!(
        !foreign.exists(),
        "the foreign socket path must not be created"
    );
}

#[test]
fn override_with_stale_socket_file_never_spawns() {
    let sb = Sandbox::new();
    let foreign = sb.root.join("stale.sock");
    fs::write(&foreign, b"").unwrap();
    let out = sb.run(Some(&foreign));
    assert_refused(&out, &sb, "override + stale file");
}

#[test]
fn override_equal_to_canonical_path_is_still_foreign() {
    // An override is foreign by provenance, even when it spells rally's own path.
    let sb = Sandbox::new();
    let out = sb.run(Some(&sb.canonical_socket()));
    assert_refused(&out, &sb, "override == canonical");
}

#[test]
fn canonical_socket_symlink_never_spawns() {
    let sb = Sandbox::new();
    let dir = sb.home.join(".local/share/rally");
    fs::create_dir_all(&dir).unwrap();
    symlink(sb.root.join("elsewhere.sock"), dir.join("ptyd.sock")).unwrap();
    let out = sb.run(None);
    assert_refused(&out, &sb, "canonical socket is a symlink");
}

#[test]
fn aliased_socket_directory_never_spawns() {
    let sb = Sandbox::new();
    let other = sb.root.join("other-dir");
    fs::create_dir_all(&other).unwrap();
    fs::create_dir_all(sb.home.join(".local/share")).unwrap();
    symlink(&other, sb.home.join(".local/share/rally")).unwrap();
    let out = sb.run(None);
    assert_refused(&out, &sb, "socket directory is an alias");
}

#[test]
fn aliased_parent_with_missing_rally_directory_never_spawns() {
    let sb = Sandbox::new();
    let other = sb.root.join("other-dir");
    fs::create_dir_all(&other).unwrap();
    fs::create_dir_all(sb.home.join(".local")).unwrap();
    symlink(&other, sb.home.join(".local/share")).unwrap();
    let out = sb.run(None);
    assert_refused(
        &out,
        &sb,
        "socket parent is an alias and rally leaf is absent",
    );
    assert!(!other.join("rally").exists());
}

#[test]
fn override_socket_appearing_mid_run_never_spawns() {
    // Restart window: the owning app is coming back while rally runs. Refusal
    // is decided from provenance, not from a liveness race, so timing cannot
    // flip it.
    let sb = Sandbox::new();
    let foreign = sb.root.join("restarting.sock");
    let appear = foreign.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = fs::write(&appear, b"");
    });
    let out = sb.run(Some(&foreign));
    writer.join().unwrap();
    assert_refused(&out, &sb, "override appearing mid-run");
}

#[test]
fn canonical_socket_without_override_still_autostarts() {
    // Control: the fake daemon never binds, so the run fails after its wait,
    // but it must have been launched.
    let sb = Sandbox::new();
    let out = sb.run(None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        sb.spawned(),
        "rally must autostart on its own canonical socket; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("will not start one there"),
        "no ownership refusal expected; stderr: {stderr}"
    );
}
