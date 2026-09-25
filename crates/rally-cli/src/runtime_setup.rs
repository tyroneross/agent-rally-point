// SPDX-License-Identifier: Apache-2.0
//! Explicit, permissioned runtime setup. Discovery never installs or starts services.
use bpaf::{Parser, construct, long};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{RallyError, Result};
use crate::output::Output;

#[derive(Clone, Debug)]
pub(crate) struct SetupArgs {
    pub json: bool,
    pub component: String,
    pub apply: bool,
    pub approve_plan: Option<String>,
}

pub(crate) fn parser() -> impl Parser<SetupArgs> {
    let json = long("json").switch();
    let component = long("component")
        .argument::<String>("ITEM")
        .fallback("tmux".to_string())
        .guard(
            |s| matches!(s.as_str(), "tmux" | "coordinator" | "ptyd"),
            "choose tmux, coordinator or ptyd",
        );
    let apply = long("apply")
        .help("Ask permission, then install/enable and test the selected item.")
        .switch();
    let approve_plan = long("approve-plan").help("Apply this exact plan ID after explicit user approval; never an implicit agent approval.").argument::<String>("ID").optional();
    construct!(SetupArgs {
        json,
        component,
        apply,
        approve_plan
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub plan_id: String,
    pub component: String,
    pub feature: String,
    pub question: String,
    pub state: String,
    pub reason: String,
    pub command: Vec<String>,
    pub binary: Option<String>,
    pub repo: Option<PathBuf>,
    pub starts_at_login: bool,
    pub source_policy: String,
    pub destination: String,
    pub initial_start: String,
    pub permission_required: bool,
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn executable(name: &str) -> Option<PathBuf> {
    let valid = |p: &Path| {
        p.is_file()
            && p.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        return valid(Path::new(name)).then(|| PathBuf::from(name));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|p| p.join(name))
        .find(|p| valid(p))
}

pub(crate) fn state_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("RALLY_SETUP_STATE_DIR") {
        return Ok(p.into());
    }
    std::env::var_os("HOME")
        .map(|p| PathBuf::from(p).join(".local/share/rally/setup"))
        .ok_or_else(|| {
            RallyError::Usage("HOME is unavailable; cannot store an approved setup receipt".into())
        })
}

pub(crate) fn private_write(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().expect("receipt parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(RallyError::io("create setup receipt directory"))?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(RallyError::io("create private receipt"))?;
    let result = (|| {
        f.write_all(serde_json::to_string_pretty(value).unwrap().as_bytes())
            .map_err(RallyError::io("write receipt"))?;
        f.sync_all().map_err(RallyError::io("sync receipt"))?;
        fs::rename(&tmp, path).map_err(RallyError::io("publish receipt"))?;
        File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(RallyError::io("sync receipt directory"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn approval_path(repo: &Path) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!(
        "coordinator-{}.json",
        digest(repo.to_string_lossy().as_bytes())
    )))
}

pub(crate) fn coordinator_approved(repo: &Path) -> bool {
    let Ok(expected) = plan_unapproved("coordinator", false) else {
        return false;
    };
    if expected.repo.as_deref() != Some(repo) {
        return false;
    }
    approval_path(repo)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| {
            v["approved"] == true
                && v["repo"].as_str() == repo.to_str()
                && v["plan_id"] == expected.plan_id
        })
}

/// Bounded process observation uses files, not pipes that a descendant can keep open.
pub(crate) fn capture(command: &mut Command, timeout: Duration) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("rally-check-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(RallyError::io("create connection check directory"))?;
    let result = (|| {
        let out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("out"))
            .map_err(RallyError::io("create check output"))?;
        let err = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("err"))
            .map_err(RallyError::io("create check error"))?;
        // Isolate descendants so a timed-out installer cannot keep installing behind a retry.
        command.process_group(0);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(RallyError::io("start connection check"))?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {}
                Err(e) => {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    return Err(RallyError::Command(format!(
                        "Process observation failed; outcome unknown: {e}"
                    )));
                }
            }
            let oversized = ["out", "err"]
                .iter()
                .any(|name| fs::metadata(dir.join(name)).is_ok_and(|m| m.len() > 1024 * 1024));
            if Instant::now() >= deadline || oversized {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                // Reaping is bounded too: an OS loader stall must not hang setup.
                let cleanup = Instant::now() + Duration::from_secs(1);
                while Instant::now() < cleanup {
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(RallyError::Command(if oversized { "Setup process exceeded its diagnostic output limit; outcome unknown" } else { "Setup process timed out; outcome unknown; inspect installation state before retrying" }.into()));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = String::new();
        File::open(dir.join("out"))
            .and_then(|f| f.take(16384).read_to_string(&mut stdout))
            .map_err(RallyError::io("read check output"))?;
        if !status.success() {
            let mut stderr = String::new();
            let _ =
                File::open(dir.join("err")).and_then(|f| f.take(4096).read_to_string(&mut stderr));
            return Err(RallyError::Command(format!(
                "connection check failed ({status}): {}",
                stderr.trim()
            )));
        }
        Ok(stdout)
    })();
    let _ = fs::remove_dir_all(dir);
    result
}

fn package_command() -> Vec<String> {
    if cfg!(target_os = "macos") {
        return executable("brew")
            .map(|p| vec![p.display().to_string(), "install".into(), "tmux".into()])
            .unwrap_or_default();
    }
    if cfg!(target_os = "linux") {
        for name in ["apt-get", "dnf"] {
            if let Some(p) = executable(name) {
                let mut args = vec![
                    p.display().to_string(),
                    "install".into(),
                    "-y".into(),
                    "tmux".into(),
                ];
                if unsafe { libc::geteuid() } != 0 {
                    let Some(sudo) = executable("sudo") else {
                        return vec![];
                    };
                    args.splice(0..0, [sudo.display().to_string(), "-n".into(), "--".into()]);
                }
                return args;
            }
        }
    }
    vec![]
}

pub(crate) fn plan(component: &str) -> Result<Plan> {
    let mut p = plan_unapproved(component, true)?;
    if component == "coordinator" && coordinator_approved(p.repo.as_ref().unwrap()) {
        p.permission_required = false;
    }
    Ok(p)
}

fn plan_unapproved(component: &str, inspect_runtime: bool) -> Result<Plan> {
    let mut p = Plan {
        plan_id: String::new(),
        component: component.into(),
        feature: String::new(),
        question: String::new(),
        state: "missing".into(),
        reason: String::new(),
        command: vec![],
        binary: None,
        repo: None,
        starts_at_login: false,
        source_policy:
            "Existing supported platform package manager; stable tmux and required dependencies"
                .into(),
        destination: "Package manager default prefix".into(),
        initial_start: "Temporary isolated connection check only".into(),
        permission_required: true,
    };
    match component {
        "tmux" => {
            p.feature = "background terminal agents".into();
            p.question = "Install tmux to enable background terminal agents?".into();
            if let Some(bin) = executable("tmux") {
                p.binary = Some(bin.display().to_string());
                match capture(Command::new(&bin).arg("-V"), Duration::from_secs(3)) {
                    Ok(version) if version.trim().starts_with("tmux ") => {
                        p.state = "installed".into();
                        p.reason = version.trim().into();
                        p.permission_required = false;
                    }
                    Ok(_) => {
                        p.state = "blocked".into();
                        p.reason = "Executable did not identify itself as tmux".into();
                    }
                    Err(e) => {
                        p.state = "blocked".into();
                        p.reason = e.to_string();
                    }
                }
            } else {
                p.command = package_command();
                p.reason = if p.command.is_empty() { "No supported package manager found; install tmux with your platform installer, then rerun setup" } else { "tmux is not installed" }.into();
            }
        }
        "coordinator" => {
            let root = crate::repo_root()?;
            p.feature = "faster multi-agent coordination".into();
            p.question =
                "Enable Rally's background coordinator to improve multi-agent coordination?".into();
            p.binary = Some(
                std::env::current_exe()
                    .map_err(RallyError::io("resolve Rally"))?
                    .display()
                    .to_string(),
            );
            p.repo = Some(root);
            p.source_policy = "Included Rally executable".into();
            p.destination = "Repository .rally directory".into();
            p.initial_start =
                "Per-repository coordinator, subject to its idle timeout; no login registration"
                    .into();
            p.state = "awaiting_permission".into();
            p.reason = "Included in Rally; enablement starts a per-repo background service, without login registration".into();
            p.command = vec![p.binary.clone().unwrap(), "daemon".into(), "start".into()];
            if inspect_runtime
                && let Ok(status) = crate::command_daemon_status(true)
                && status.body["data"]["daemon"]["live"] == true
            {
                p.state = "ready".into();
                // A live manually started daemon is not consent to future
                // automatic activation. Only the matching receipt grants it.
            }
        }
        "ptyd" => {
            p.feature = "managed terminal connections".into();
            p.question = "Install ptyd to enable managed terminal connections?".into();
            p.source_policy = "No automatic installer configured".into();
            p.destination = "Existing installation".into();
            p.initial_start = "None; explicit backend launch required".into();
            p.binary = match std::env::var("RALLY_PTYD_BIN") {
                Ok(p) => executable(&p),
                Err(_) => executable("ptyd"),
            }
            .map(|p| p.display().to_string());
            p.reason = "No verified automatic ptyd installer is configured; use tmux or install ptyd from its supported distribution".into();
            if p.binary.is_some() {
                p.state = "installed".into();
                p.permission_required = false;
                p.reason =
                    "ptyd is installed; use its explicit backend launch to bind and test an agent"
                        .into();
            }
            if crate::daemon_client::rally_owned_socket()
                .is_some_and(|s| crate::daemon_client::socket_is_live(&s))
            {
                p.state = "ready".into();
                p.permission_required = false;
                p.reason = "Rally-owned ptyd endpoint answers its protocol probe; agent receipt still requires a route test".into();
            }
        }
        _ => return Err(RallyError::Usage("unknown setup component".into())),
    }
    // Bind approval to the action, not fluctuating readiness text.
    p.plan_id = digest(&serde_json::to_vec(&json!({"component":p.component,"feature":p.feature,"binary":p.binary,"command":p.command,"repo":p.repo,"login":false,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"source_policy":p.source_policy,"destination":p.destination,"initial_start":p.initial_start})).unwrap());
    Ok(p)
}

struct SetupLock(File);
impl SetupLock {
    fn acquire() -> Result<Self> {
        let dir = state_dir()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(RallyError::io("create setup directory"))?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join("setup.lock"))
            .map_err(RallyError::io("open setup lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(RallyError::NotStarted(
                "Another setup is in progress; inspect its receipt and retry after it finishes"
                    .into(),
            ));
        }
        Ok(Self(f))
    }
}
impl Drop for SetupLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The prompt has a deadline even when an agent invokes setup from a PTY.
fn read_approval(fd: libc::c_int, timeout: Duration) -> Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    while Instant::now() < deadline && bytes.len() < 4096 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining.as_millis().clamp(1, 1000) as i32,
            )
        };
        if polled < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(RallyError::io("wait for approval")(e));
        }
        if polled == 0 {
            continue;
        }
        let mut byte = 0u8;
        let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        if read <= 0 {
            return Ok(None);
        }
        if byte == b'\n' || byte == b'\r' {
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
        bytes.push(byte);
    }
    Ok(None)
}

fn approved(p: &Plan, token: Option<&str>, answer: Option<&str>) -> bool {
    !p.permission_required
        || token == Some(p.plan_id.as_str())
        || answer.is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn tmux_connection(bin: &str) -> Result<Value> {
    let dir = std::env::temp_dir().join(format!(
        "rc-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    fs::create_dir(&dir).map_err(RallyError::io("create private tmux test"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(RallyError::io("protect tmux test"))?;
    let socket = dir.join("s");
    let call = |args: &[&str]| {
        capture(
            Command::new(bin).arg("-S").arg(&socket).args(args),
            Duration::from_secs(4),
        )
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let reply = format!("RALLY_REPLY_{nonce}");
    let result = (|| {
        call(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "check",
            "/bin/sh",
            "-c",
            "read line; printf 'RALLY_REPLY_%s\\n' \"$line\"; sleep 15",
        ])?;
        let guards = call(&[
            "display-message",
            "-p",
            "-t",
            "check:0.0",
            &crate::backends::tmux_format(&["#{pane_input_off}", "#{synchronize-panes}"]),
        ])?;
        let guard_fields = crate::backends::split_tmux_fields(&guards, 2, None);
        if guard_fields.as_deref() != Some(&["0".to_string(), "0".to_string()][..]) {
            return Err(RallyError::Command("tmux does not expose the required safe-input guard fields; update tmux before using this adapter".into()));
        }
        call(&["send-keys", "-t", "check:0.0", "-l", &nonce])?;
        call(&["send-keys", "-t", "check:0.0", "Enter"])?;
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if call(&["capture-pane", "-p", "-t", "check:0.0"])?.contains(&reply) {
                return Ok(
                    json!({"transport_roundtrip":true,"receiver":"controlled shell","agent_ack":false,"scope":"runtime only"}),
                );
            }
            if Instant::now() >= deadline {
                return Err(RallyError::Command(
                    "tmux started but its controlled receiver did not respond".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    let _ = call(&["kill-server"]);
    let _ = fs::remove_dir_all(dir);
    result
}

fn render(args: &SetupArgs, plan: &Plan, state: &str, detail: Value, code: u8) -> Result<Output> {
    let reason = if state == "ready" {
        "Connection check passed for this runtime; agent routes require their own receipt."
    } else if state == "blocked" {
        detail["error"]
            .as_str()
            .or_else(|| detail["reason"].as_str())
            .unwrap_or(&plan.reason)
    } else {
        &plan.reason
    };
    let text = format!(
        "{}: {state}\n{}\n{}",
        plan.component,
        reason,
        if state == "awaiting_permission" {
            plan.question.as_str()
        } else {
            "Use rally routes to check agent delivery separately."
        }
    );
    let body = crate::envelope_value(
        "setup",
        "agent-rally.command.setup.v1",
        json!({"setup":{"state":state,"plan":plan,"result":detail,"agent_routes_verified":false}}),
    )?;
    Ok(Output::new(args.json, text, body).with_exit_code(code))
}

pub(crate) fn command(args: SetupArgs) -> Result<Output> {
    let p = plan(&args.component)?;
    if args.approve_plan.is_some() && !args.apply {
        return Err(RallyError::Usage("--approve-plan requires --apply".into()));
    }
    if !args.apply {
        return render(
            &args,
            &p,
            &p.state,
            json!({"next_action":format!("rally setup --component {} --apply",p.component)}),
            0,
        );
    }
    if let Some(token) = &args.approve_plan
        && token != &p.plan_id
    {
        return Err(RallyError::Usage(
            "Setup plan changed; obtain approval for the current plan ID".into(),
        ));
    }
    if p.state == "blocked" || (p.state == "missing" && p.command.is_empty()) {
        return render(&args, &p, "blocked", json!({"reason":p.reason}), 4);
    }
    let mut answer = String::new();
    if p.permission_required
        && args.approve_plan.is_none()
        && !args.json
        && std::io::stdin().is_terminal()
    {
        eprint!("{} [y/N] ", p.question);
        std::io::stderr()
            .flush()
            .map_err(RallyError::io("flush approval prompt"))?;
        answer = read_approval(0, Duration::from_secs(30))?.unwrap_or_default();
        eprintln!();
    }
    if !approved(&p, args.approve_plan.as_deref(), Some(&answer)) {
        return render(
            &args,
            &p,
            "awaiting_permission",
            json!({"installed":false,"approved":false}),
            4,
        );
    }
    let _lock = SetupLock::acquire()?;
    let current = plan(&p.component)?;
    if current.plan_id != p.plan_id {
        return Err(RallyError::NotStarted(
            "Setup changed while waiting; review the new plan".into(),
        ));
    }
    let receipt = state_dir()?.join(format!("{}.json", p.plan_id));
    private_write(
        &receipt,
        &json!({"plan":p,"state":"starting","approved":true,"started_at":chrono::Utc::now().to_rfc3339()}),
    )?;
    let result = (|| {
        if current.component == "coordinator" {
            private_write(
                &approval_path(current.repo.as_ref().unwrap())?,
                &json!({"approved":true,"repo":current.repo,"plan_id":p.plan_id,"starts_at_login":false}),
            )?;
            let result = crate::command_daemon_start(
                true,
                crate::cli::DaemonStartArgs {
                    idle_exit_secs: None,
                },
            )?;
            return Ok(result.body["data"]["daemon"].clone());
        }
        if current.component == "ptyd" {
            return Ok(
                json!({"installed":current.binary.is_some(),"agent_ack":false,"next_action":"Launch the explicit ptyd backend and run rally routes --probe for that agent"}),
            );
        }
        if current.state == "missing" {
            let mut installer = Command::new(&current.command[0]);
            installer
                .args(&current.command[1..])
                .env("HOMEBREW_NO_AUTO_UPDATE", "1");
            capture(&mut installer, Duration::from_secs(900))?;
        }
        let checked = plan("tmux")?;
        if checked.state != "installed" {
            return Err(RallyError::Command(
                "Installer finished but tmux is not runnable; check PATH and rerun setup".into(),
            ));
        }
        tmux_connection(checked.binary.as_deref().unwrap())
    })();
    let (state, detail, code) = match result {
        Ok(value) => (
            if current.component == "ptyd" && current.state != "ready" {
                "installed"
            } else {
                "ready"
            },
            value,
            0,
        ),
        Err(e) => (
            "blocked",
            json!({"error":e.to_string(),"retry_requires_revalidation":true}),
            4,
        ),
    };
    private_write(
        &receipt,
        &json!({"plan":p,"state":state,"result":detail,"finished_at":chrono::Utc::now().to_rfc3339()}),
    )?;
    render(&args, &p, state, detail, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending() -> Plan {
        Plan {
            plan_id: "approved-plan".into(),
            component: "tmux".into(),
            feature: "agents".into(),
            question: "Install?".into(),
            state: "missing".into(),
            reason: String::new(),
            command: vec![],
            binary: None,
            repo: None,
            starts_at_login: false,
            source_policy: String::new(),
            destination: String::new(),
            initial_start: String::new(),
            permission_required: true,
        }
    }
    #[test]
    fn consent_never_defaults_to_yes() {
        let p = pending();
        for answer in [None, Some(""), Some("no"), Some("timeout")] {
            assert!(!approved(&p, None, answer));
        }
        assert!(!approved(&p, Some("old-plan"), None));
        assert!(approved(&p, Some("approved-plan"), None));
        assert!(approved(&p, None, Some("yes")));
    }
    #[test]
    fn already_installed_needs_no_install_approval() {
        let mut p = pending();
        p.permission_required = false;
        assert!(approved(&p, None, None));
    }
    #[test]
    fn non_executable_is_not_installed() {
        let p = std::env::temp_dir().join(format!("rally-noexec-{}", uuid::Uuid::new_v4()));
        fs::write(&p, "x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(executable(p.to_str().unwrap()).is_none());
        fs::remove_file(p).unwrap();
    }
    #[test]
    fn hung_subprocess_is_bounded() {
        let start = Instant::now();
        let err = capture(
            Command::new("/bin/sh").args(["-c", "sleep 20 & wait"]),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(err.to_string().contains("outcome unknown"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn noisy_subprocess_fails_with_explicit_budget() {
        let err = capture(
            Command::new("/bin/sh").args(["-c", "while :; do printf 'diagnostic output'; done"]),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.to_string().contains("diagnostic output limit"));
    }
    #[test]
    fn approval_input_times_out_without_authorizing() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let answer = read_approval(fds[0], Duration::from_millis(20)).unwrap();
        assert!(answer.is_none());
        assert!(!approved(&pending(), None, answer.as_deref()));
        assert_eq!(
            unsafe { libc::write(fds[1], b"yes\n".as_ptr().cast(), 4) },
            4
        );
        assert_eq!(
            read_approval(fds[0], Duration::from_secs(1))
                .unwrap()
                .as_deref(),
            Some("yes")
        );
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
