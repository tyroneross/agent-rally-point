// SPDX-License-Identifier: Apache-2.0
//! Restart the room coordinator (rallyd) when it is down in an approved room.
//!
//! Before this, nothing restarted a coordinator after it idled out or stopped:
//! ordinary commands only probed for it and then fell back to direct store
//! ownership, where every agent contends for one lock. Now the first ordinary
//! command that finds no coordinator in a room whose owner approved it
//! (`rally setup --component coordinator`) starts one detached coordinator and
//! carries on directly; later commands route through it.
//!
//! Guards, all deterministic:
//! - owner approval for this exact repo, and `RALLY_DAEMON_AUTOSTART != 0`;
//! - only the `rally` CLI starts it (other binaries link this crate);
//! - single flight with a crash-loop throttle: one attempt per room per
//!   [`ATTEMPT_INTERVAL`], claimed by atomically creating a marker file;
//! - the coordinator gets its own process group, so a Ctrl-C or a signal sent
//!   to the caller's terminal group does not stop it.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

static SUPPRESSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Commands that start, stop or activate the coordinator themselves
/// (`rally daemon *`, `rally session ensure`, `rally setup`) call this first, so
/// an ordinary-path autostart never races their own lifecycle work.
pub(crate) fn suppress_for_this_process() {
    SUPPRESSED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Minimum spacing between start attempts in one room.
pub(crate) const ATTEMPT_INTERVAL: Duration = Duration::from_secs(60);

const MARKER: &str = "rallyd.autostart";

/// Pure gate: may this process try to start the coordinator now?
pub(crate) fn should_attempt(
    autostart_env: Option<&str>,
    exe_is_rally: bool,
    approved: bool,
) -> bool {
    autostart_env != Some("0") && exe_is_rally && approved
}

/// Claim this room's start attempt. Returns true for exactly one caller per
/// [`ATTEMPT_INTERVAL`]: the marker is created atomically, and an older marker
/// is replaced once.
pub(crate) fn claim_attempt(rally_dir: &Path, now: SystemTime) -> bool {
    let marker = rally_dir.join(MARKER);
    for _ in 0..2 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let fresh = fs::metadata(&marker)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|at| now.duration_since(at).ok())
                    .is_none_or(|age| age < ATTEMPT_INTERVAL);
                if fresh || fs::remove_file(&marker).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

/// Start one detached coordinator if the room is approved and no attempt ran
/// recently. Never waits for readiness and never fails the caller: the caller
/// continues on the direct path either way.
pub(crate) fn maybe_start(root: &Path, rally_dir: &Path) {
    if cfg!(test) || SUPPRESSED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Ok(exe) = env::current_exe() else { return };
    let exe_is_rally = exe.file_name().is_some_and(|name| name == "rally");
    if !should_attempt(
        env::var("RALLY_DAEMON_AUTOSTART").ok().as_deref(),
        exe_is_rally,
        crate::runtime_setup::coordinator_approved(root),
    ) || !claim_attempt(rally_dir, SystemTime::now())
    {
        return;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rally_dir.join("rallyd.log"));
    let (out, err) = match log.and_then(|f| Ok((f.try_clone()?, f))) {
        Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "serve", "--detached"])
        .current_dir(root)
        // The coordinator's own store opens must never start another one.
        .env("RALLY_DAEMON_AUTOSTART", "0")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Detached on purpose: the coordinator outlives this command.
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rally-autostart-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn gate_requires_approval_rally_exe_and_no_opt_out() {
        assert!(should_attempt(None, true, true));
        assert!(should_attempt(Some("1"), true, true));
        assert!(!should_attempt(Some("0"), true, true), "explicit opt-out");
        assert!(
            !should_attempt(None, false, true),
            "other binaries linking the crate"
        );
        assert!(
            !should_attempt(None, true, false),
            "room not approved by its owner"
        );
    }

    #[test]
    fn one_attempt_per_interval_then_one_more_after_it() {
        let dir = tmp("claim");
        let now = SystemTime::now();
        assert!(claim_attempt(&dir, now), "first caller claims");
        assert!(
            !claim_attempt(&dir, now),
            "second caller in the window is refused"
        );
        assert!(
            !claim_attempt(&dir, now + ATTEMPT_INTERVAL / 2),
            "still inside the throttle window"
        );
        assert!(
            claim_attempt(&dir, now + ATTEMPT_INTERVAL + Duration::from_secs(1)),
            "a crash-looping coordinator is retried once per interval"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
