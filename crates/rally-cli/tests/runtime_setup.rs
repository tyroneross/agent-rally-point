// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! CLI contracts for permissioned runtime discovery and setup.
//!
//! Every executable named here is a fixture in a fresh `PATH`.  These tests
//! never invoke a host package manager, tmux, ptyd, or coordinator daemon.

mod common;
mod support;

use common::test_git_fixture::fixture_git;
use serde_json::Value;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::rally_cmd::rally_command;

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let unique = format!(
            "rally-runtime-setup-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("wall clock")
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let root = base.join("repo");
        let home = base.join("home");
        let state = base.join("setup-state");
        let bin = base.join("bin");
        fs::create_dir_all(&root).expect("create repo");
        fs::create_dir_all(&home).expect("create HOME");
        fs::create_dir_all(&bin).expect("create fake PATH");
        fixture_git(&root, &["init", "-q", "-b", "main"]);
        Self {
            root,
            home,
            state,
            bin,
        }
    }

    fn rally(&self) -> Command {
        let mut command = rally_command();
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("RALLY_SETUP_STATE_DIR", &self.state)
            .env("PATH", &self.bin)
            // The product supports explicit ptyd overrides.  Tests must not
            // inherit an operator's live daemon or binary selection.
            .env_remove("RALLY_PTYD_BIN")
            .env_remove("RALLY_PTYD_SOCKET")
            .env_remove("RALLY_DAEMON_AUTOSTART")
            .env_remove("RALLY_SESSION_ID")
            .env_remove("RALLY_SESSION_CLOSE_TOKEN")
            .env_remove("TMUX");
        command
    }

    fn command(&self, args: &[&str]) -> Output {
        let mut command = self.rally();
        command.args(args);
        self.output_bounded(command)
    }

    /// The product watchdog begins inside Rally.  A loader stall can happen
    /// before `main`, so the fixture owns an outer bound and captures to files
    /// rather than pipes a descendant could keep open after the Rally child
    /// exits.  The child itself is always reaped on timeout.
    fn output_bounded(&self, mut command: Command) -> Output {
        let capture = self
            .root
            .join(format!(".rally-test-output-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&capture).expect("create fixture output directory");
        fs::set_permissions(&capture, fs::Permissions::from_mode(0o700))
            .expect("protect fixture output directory");
        let open = |name: &str| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(capture.join(name))
                .expect("open fixture output")
        };
        let mut child = command
            .stdout(Stdio::from(open("stdout")))
            .stderr(Stdio::from(open("stderr")))
            .spawn()
            .expect("spawn rally");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            match child.try_wait().expect("poll rally") {
                Some(status) => break status,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let (stdout, stderr) = read_capture(&capture);
                    let _ = fs::remove_dir_all(&capture);
                    panic!(
                        "rally fixture exceeded its 60s outer bound before the product watchdog returned\\nstdout: {}\\nstderr: {}",
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr)
                    );
                }
            }
        };
        let (stdout, stderr) = read_capture(&capture);
        fs::remove_dir_all(&capture).expect("remove fixture output directory");
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn write_executable(&self, name: &str, script: &str) -> PathBuf {
        let path = self.bin.join(name);
        fs::write(&path, script).expect("write fake executable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("mark fake executable executable");
        path
    }

    fn cleanup(self) {
        if let Some(base) = self.root.parent() {
            fs::remove_dir_all(base).ok();
        }
    }
}

fn read_capture(capture: &Path) -> (Vec<u8>, Vec<u8>) {
    let read = |name: &str| {
        let mut bytes = Vec::new();
        File::open(capture.join(name))
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .expect("read fixture output");
        bytes
    };
    (read("stdout"), read("stderr"))
}

fn fake_tmux_script() -> &'static str {
    r#"#!/bin/sh
if [ "$1" = "-V" ]; then
  printf 'tmux 3.6a\n'
  exit 0
fi
command=''
for argument in "$@"; do
  case "$argument" in
    new-session|send-keys|capture-pane|kill-server|display-message) command="$argument"; break ;;
  esac
done
case "$command" in
  display-message) printf '0\t0\n' ;;
  send-keys)
    while [ "$#" -gt 0 ]; do
      if [ "$1" = "-l" ]; then
        shift
        printf '%s' "$1" > "$0.nonce"
        break
      fi
      shift
    done
    ;;
  capture-pane)
    nonce=''
    if [ -r "$0.nonce" ]; then IFS= read -r nonce < "$0.nonce"; fi
    printf 'RALLY_REPLY_%s\n' "$nonce"
    ;;
esac
exit 0
"#
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

/// Installers selected by `setup` differ by platform.  Each fixture command
/// writes a controlled tmux binary and an installation counter using only
/// shell builtins (the fixture PATH intentionally excludes host tools).
fn write_fake_tmux_installers(fixture: &Fixture, marker: &Path) {
    let tmux = fixture.bin.join("tmux");
    let marker = shell_path(marker);
    let tmux = shell_path(&tmux);
    let installer = format!(
        "#!/bin/sh\ncount=0\nif [ -r {marker} ]; then IFS= read -r count < {marker}; fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > {marker}\n/bin/cat > {tmux} <<'RALLY_TMUX'\n{}RALLY_TMUX\n/bin/chmod 700 {tmux}\nexit 0\n",
        fake_tmux_script(),
        marker = marker,
        tmux = tmux,
    );
    for name in ["brew", "apt-get", "dnf"] {
        fixture.write_executable(name, &installer);
    }
    fixture.write_executable("sudo", "#!/bin/sh\nshift\nshift\nexec \"$@\"\n");
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected JSON stdout: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Non-interactive apply must stop at the consent boundary.  A discovered
/// package manager is only a proposed command until the user approves it.
#[test]
fn readonly_json_apply_never_runs_the_discovered_installer() {
    let fixture = Fixture::new("no-consent");
    let marker = fixture.root.join("installer-ran");
    fixture.write_executable(
        "brew",
        &format!("#!/bin/sh\nprintf ran > {}\n", marker.display()),
    );
    // Linux chooses apt-get (and possibly sudo) instead of brew.  Supplying
    // every supported discovery name makes this assertion host-independent.
    fixture.write_executable(
        "apt-get",
        &format!("#!/bin/sh\nprintf ran > {}\n", marker.display()),
    );
    fixture.write_executable(
        "dnf",
        &format!("#!/bin/sh\nprintf ran > {}\n", marker.display()),
    );
    fixture.write_executable("sudo", "#!/bin/sh\nshift\nshift\nexec \"$@\"\n");

    let output = fixture.command(&["setup", "--component", "tmux", "--apply", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(
        body.pointer("/data/setup/state").and_then(Value::as_str),
        Some("awaiting_permission")
    );
    assert_eq!(
        body.pointer("/data/setup/result/approved")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        !marker.exists(),
        "an unattended JSON invocation must not execute an installer"
    );
    fixture.cleanup();
}

/// A token obtained from the same immutable plan is accepted.  ptyd is the
/// safe integration fixture: setup records discovery only and does not start
/// the externally installed daemon.
#[test]
fn exact_setup_plan_token_is_accepted_without_starting_a_service() {
    let fixture = Fixture::new("exact-plan");
    fixture.write_executable("ptyd", "#!/bin/sh\nexit 99\n");

    let planned = fixture.command(&["setup", "--component", "ptyd", "--json"]);
    assert!(
        planned.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan = json(&planned);
    let id = plan
        .pointer("/data/setup/plan/plan_id")
        .and_then(Value::as_str)
        .expect("plan id")
        .to_string();

    let applied = fixture.command(&[
        "setup",
        "--component",
        "ptyd",
        "--apply",
        "--approve-plan",
        &id,
        "--json",
    ]);
    assert!(
        applied.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let body = json(&applied);
    assert_eq!(
        body.pointer("/data/setup/state").and_then(Value::as_str),
        Some("installed")
    );
    assert_eq!(
        body.pointer("/data/setup/result/installed")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        !fixture.home.join(".local/share/rally/ptyd.sock").exists(),
        "ptyd discovery must not start a daemon"
    );
    fixture.cleanup();
}

/// This is the permission-required path: the reviewed tmux plan invokes a
/// fixture installer, which installs a fixture tmux that completes Rally's
/// isolated `read -> reply` transport check.  Reapplying after readiness must
/// reuse that runtime rather than call the package manager again.
#[test]
fn approved_tmux_plan_installs_once_and_passes_the_controlled_roundtrip() {
    let fixture = Fixture::new("approved-tmux");
    let marker = fixture.root.join("installer-count");
    write_fake_tmux_installers(&fixture, &marker);

    let planned = fixture.command(&["setup", "--component", "tmux", "--json"]);
    assert!(
        planned.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan = json(&planned);
    assert_eq!(
        plan.pointer("/data/setup/plan/permission_required")
            .and_then(Value::as_bool),
        Some(true),
        "this fixture must exercise explicit approval"
    );
    let id = plan
        .pointer("/data/setup/plan/plan_id")
        .and_then(Value::as_str)
        .expect("approved plan id")
        .to_string();

    let applied = fixture.command(&[
        "setup",
        "--component",
        "tmux",
        "--apply",
        "--approve-plan",
        &id,
        "--json",
    ]);
    assert!(
        applied.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    let body = json(&applied);
    assert_eq!(
        body.pointer("/data/setup/state").and_then(Value::as_str),
        Some("ready")
    );
    assert_eq!(
        body.pointer("/data/setup/result/transport_roundtrip")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("first installer count"),
        "1"
    );

    let reused = fixture.command(&["setup", "--component", "tmux", "--apply", "--json"]);
    assert!(
        reused.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&reused.stdout),
        String::from_utf8_lossy(&reused.stderr)
    );
    assert_eq!(
        json(&reused)
            .pointer("/data/setup/state")
            .and_then(Value::as_str),
        Some("ready")
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("reused installer count"),
        "1",
        "a ready tmux runtime must not trigger a second install"
    );
    fixture.cleanup();
}

#[test]
fn stale_setup_plan_token_is_refused_before_any_apply_side_effect() {
    let fixture = Fixture::new("stale-plan");
    let planned = fixture.command(&["setup", "--component", "tmux", "--json"]);
    assert!(
        planned.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let old_id = json(&planned)
        .pointer("/data/setup/plan/plan_id")
        .and_then(Value::as_str)
        .expect("old plan id")
        .to_string();
    // Discovery has changed between review and apply: this is the state the
    // lock/replan check must reject rather than treating the old consent as
    // authorization for a different action.
    fixture.write_executable(
        "tmux",
        "#!/bin/sh\nif [ \"$1\" = \"-V\" ]; then echo 'tmux 3.6a'; exit 0; fi\nexit 99\n",
    );

    let output = fixture.command(&[
        "setup",
        "--component",
        "tmux",
        "--apply",
        "--approve-plan",
        &old_id,
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Setup plan changed"));
    assert!(
        !fixture.state.exists(),
        "a rejected approval token must not write a setup receipt"
    );
    fixture.cleanup();
}

#[test]
fn installed_tmux_is_reused_without_proposing_or_running_an_installer() {
    let fixture = Fixture::new("installed-tmux");
    let marker = fixture.root.join("installer-ran");
    fixture.write_executable(
        "tmux",
        "#!/bin/sh\nif [ \"$1\" = \"-V\" ]; then echo 'tmux 3.6a'; exit 0; fi\nexit 99\n",
    );
    fixture.write_executable(
        "brew",
        &format!("#!/bin/sh\nprintf ran > {}\n", marker.display()),
    );

    let output = fixture.command(&["setup", "--component", "tmux", "--json"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(
        body.pointer("/data/setup/state").and_then(Value::as_str),
        Some("installed")
    );
    assert_eq!(
        body.pointer("/data/setup/plan/permission_required")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        body.pointer("/data/setup/plan/command")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    assert!(
        !marker.exists(),
        "installed tmux must be reused without invoking an installer"
    );
    fixture.cleanup();
}

#[test]
fn non_executable_explicit_ptyd_override_is_not_reported_installed() {
    let fixture = Fixture::new("bad-ptyd-override");
    let bad = fixture.bin.join("not-executable-ptyd");
    fs::write(&bad, "not executable").expect("write bad ptyd override");
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o600))
        .expect("make override non-executable");

    let mut command = fixture.rally();
    command
        .env("RALLY_PTYD_BIN", &bad)
        .args(["setup", "--component", "ptyd", "--json"]);
    let output = fixture.output_bounded(command);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(
        body.pointer("/data/setup/state").and_then(Value::as_str),
        Some("missing")
    );
    assert!(
        body.pointer("/data/setup/plan/binary")
            .is_some_and(Value::is_null)
    );
    fixture.cleanup();
}

fn assert_no_coordinator_started(fixture: &Fixture) {
    for name in ["rallyd.pid", "rallyd.sock", "rallyd.sock.addr"] {
        assert!(
            !fixture.root.join(".rally").join(name).exists(),
            "coordinator must not create {name} without setup consent"
        );
    }
    assert!(
        !fixture.state.exists(),
        "coordinator consent receipt must not be created without approval"
    );
}

/// Two fresh session leases meet the automatic-activation threshold.  Even
/// with autostart enabled, that threshold may report only `permission_required`
/// until the user has approved the coordinator setup plan.
#[test]
fn coordinator_autostart_and_explicit_setup_require_consent() {
    let fixture = Fixture::new("coordinator-consent");
    let ensure = |session_id: &str, tool: &str| {
        let mut command = fixture.rally();
        command.env("RALLY_DAEMON_AUTOSTART", "1").args([
            "session",
            "ensure",
            "--json",
            "--session-id",
            session_id,
            "--tool",
            tool,
            "--adapter",
            "test",
        ]);
        let output = fixture.output_bounded(command);
        assert!(
            output.status.success(),
            "session ensure failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        json(&output)
    };

    let first = ensure("fresh-lease-one", "test:one");
    assert_eq!(
        first
            .pointer("/data/session/daemon/activation")
            .and_then(Value::as_str),
        Some("not_needed")
    );
    let second = ensure("fresh-lease-two", "test:two");
    assert_eq!(
        second
            .pointer("/data/session/daemon/activation")
            .and_then(Value::as_str),
        Some("permission_required")
    );
    assert_no_coordinator_started(&fixture);

    let setup = fixture.command(&["setup", "--component", "coordinator", "--apply", "--json"]);
    assert_eq!(
        setup.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );
    assert_eq!(
        json(&setup)
            .pointer("/data/setup/state")
            .and_then(Value::as_str),
        Some("awaiting_permission")
    );
    assert_no_coordinator_started(&fixture);
    fixture.cleanup();
}

#[test]
fn routes_probe_parser_rejects_an_out_of_range_timeout_before_delivery() {
    let fixture = Fixture::new("probe-parser");
    let output = fixture.command(&[
        "routes",
        "--probe",
        "codex:01",
        "--tool",
        "codex:00",
        "--timeout-seconds",
        "0",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timeout must be 1..120 seconds"));
    assert!(
        !fixture.root.join(".rally/log").exists(),
        "parser refusal must not append a probe handoff"
    );
    fixture.cleanup();
}
