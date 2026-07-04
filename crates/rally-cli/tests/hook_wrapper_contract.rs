// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Hook-wrapper contract for fail-closed mutation timeouts (PLAN-D §6).
//!
//! e1c1383 made timed-out uncommitted mutations exit 4 with
//! `watchdog-timeout-uncommitted-mutation` instead of the old bare `ok:true`.
//! The coordination hook's charter is fail-open advisory (exit 0 always, never
//! block the host tool) — so the NEW envelope must be routed sanely by
//! `hooks/rally-coordination-hook.sh`: the hook still exits 0 and surfaces no
//! raw panic/backtrace into the host, even when every internal mutating rally
//! call fails closed.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn hook_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh")
}

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("hook-contract-{name}-cwd"));
        let home = temp_path(&format!("hook-contract-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self { cwd, home }
    }

    /// Seed a real room (no block armed) so the hook's self-gate finds .rally/.
    fn seed(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_rally"))
            .args([
                "enter",
                "--tool",
                "hook-contract-seeder",
                "--json",
                "--timeout-ms",
                "15000",
            ])
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .output()
            .expect("spawn rally enter");
        assert!(
            out.status.success(),
            "seed enter failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(self.cwd.join(".rally").exists(), "seed must create .rally/");
    }
}

impl Drop for TempRoom {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.cwd).ok();
        fs::remove_dir_all(&self.home).ok();
    }
}

/// Every internal rally call the hook makes is forced past the default 3000ms
/// watchdog (`RALLY_TEST_BLOCK_MS=3600` sleeps before dispatch, debug builds
/// only), so mutating calls (presence/status) fail CLOSED with exit 4. The
/// hook must swallow that per its never-block charter: exit 0, no panic
/// leaked to the host.
#[test]
fn hook_stays_fail_open_when_internal_mutations_time_out_uncommitted() {
    let room = TempRoom::new("failclosed");
    room.seed();

    for phase in ["start", "idle"] {
        let out = Command::new("sh")
            .arg(hook_script())
            .args([phase, "hook-contract-tool"])
            .current_dir(&room.cwd)
            .env("HOME", &room.home)
            .env("RALLY_BIN", env!("CARGO_BIN_EXE_rally"))
            .env("RALLY_TEST_BLOCK_MS", "3600")
            .env("RALLY_HOOK_TIMEOUT_MS", "5000")
            .env("RALLY_HOOK_PROMPT", "off")
            .env_remove("RALLY_HOOKS")
            .output()
            .expect("spawn coordination hook");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "hook must exit 0 (fail-open charter) on phase `{phase}` even when \
             internal mutations time out uncommitted; got {:?}\nstdout={stdout}\nstderr={stderr}",
            out.status.code()
        );
        for leaked in ["panicked at", "RUST_BACKTRACE", "thread 'main'"] {
            assert!(
                !stdout.contains(leaked) && !stderr.contains(leaked),
                "hook leaked a raw failure marker `{leaked}` into host output on \
                 phase `{phase}`\nstdout={stdout}\nstderr={stderr}"
            );
        }
    }
}
