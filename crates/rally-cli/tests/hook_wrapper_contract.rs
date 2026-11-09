// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Hook-wrapper contract for fail-closed mutation timeouts.
//!
//! e1c1383 made timed-out uncommitted mutations exit 4 with
//! `watchdog-timeout-uncommitted-mutation` instead of the old bare `ok:true`.
//! The coordination hook's charter is fail-open advisory (exit 0 always, never
//! block the host tool) — so the NEW envelope must be routed sanely by
//! `hooks/rally-coordination-hook.sh`: the hook still exits 0 and surfaces no
//! raw panic/backtrace into the host, even when every internal mutating rally
//! call fails closed.

#[path = "../../test-support/temp_root.rs"]
mod temp_root;
use temp_root::TempRoot;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn temp_path(name: &str) -> TempRoot {
    TempRoot::new(name)
}

fn hook_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks/rally-coordination-hook.sh")
}

struct TempRoom {
    cwd: PathBuf,
    home: PathBuf,
    _roots: Vec<TempRoot>,
}

impl TempRoom {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("hook-contract-{name}-cwd"));
        let home = temp_path(&format!("hook-contract-{name}-home"));
        fs::create_dir_all(cwd.join(".git")).expect("create temp .git");
        fs::create_dir_all(&home).expect("create temp HOME");
        Self {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            _roots: vec![cwd, home],
        }
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
        // Invoke the script DIRECTLY, exactly like production does (the
        // installer writes `$HOOK_PATH start claude_code` into settings.json,
        // so the kernel honors the `#!/usr/bin/env bash` shebang). Wrapping in
        // `sh` overrides the shebang — and on Ubuntu `sh` is dash, which
        // rejects the script's `set -o pipefail` with exit 2, failing this
        // test for a reason production can never hit (issue #48, layer 3).
        let out = Command::new(hook_script())
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
