use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::BackendBins;
use crate::error::{RallyError, Result};
use crate::shell_quote;
use crate::store::Fact;

#[derive(Clone, Debug)]
pub(crate) struct AgentSpec {
    pub(crate) agent: &'static str,
    pub(crate) tool: &'static str,
    command: &'static str,
}

impl AgentSpec {
    pub(crate) fn from_name(agent: &str) -> Result<Self> {
        match agent {
            "claude" | "claude_code" | "claude-code" => Ok(Self {
                agent: "claude",
                tool: "claude_code",
                command: "claude",
            }),
            "codex" => Ok(Self {
                agent: "codex",
                tool: "codex",
                command: "codex",
            }),
            "opencode" | "ocode" | "oc" => Ok(Self {
                agent: "opencode",
                tool: "opencode",
                command: "opencode",
            }),
            "gemini" => Ok(Self {
                agent: "gemini",
                tool: "gemini",
                command: "gemini",
            }),
            other => Err(RallyError::Usage(format!("unsupported agent {other}"))),
        }
    }

    pub(crate) fn command_line(&self, name: &str) -> Vec<String> {
        match self.agent {
            "claude" => cmd![self.command, "--name", name],
            _ => cmd![self.command],
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(crate) struct ManagedSession {
    pub(crate) session_id: String,
    pub(crate) name: String,
    pub(crate) agent: String,
    pub(crate) tool: String,
    pub(crate) backend: String,
    pub(crate) cwd: PathBuf,
    pub(crate) target: String,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct RunData {
    pub(crate) mode: &'static str,
    pub(crate) session: ManagedSession,
    pub(crate) commands: RunCommands,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct RunCommands {
    pub(crate) start: Vec<Value>,
}

/// Envelope for `run`: result under `data.run`.
#[derive(JsonSchema, Serialize)]
pub(crate) struct RunEnvelope {
    pub(crate) run: RunData,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct SessionsData {
    pub(crate) sessions: Vec<ManagedSession>,
}

/// Envelope for `sessions`: result under `data.sessions`.
#[derive(JsonSchema, Serialize)]
pub(crate) struct SessionsEnvelope {
    pub(crate) sessions: SessionsData,
}

/// C-FLEET: shape of `data.adopt` for `rally adopt` responses. Carries the
/// freshly-registered `ManagedSession` so the caller has the assigned
/// `session_id` (which differs from `name` when adoption auto-numbers).
#[derive(JsonSchema, Serialize)]
pub(crate) struct AdoptData {
    pub(crate) session: ManagedSession,
}

/// Envelope for `adopt`: result under `data.adopt`.
#[derive(JsonSchema, Serialize)]
pub(crate) struct AdoptEnvelope {
    pub(crate) adopt: AdoptData,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct InjectData {
    pub(crate) mode: &'static str,
    pub(crate) session: ManagedSession,
    pub(crate) handoff: Option<String>,
    pub(crate) require_ack: bool,
    pub(crate) ack: Option<Value>,
    pub(crate) wake_intent: Option<Fact>,
    pub(crate) commands: Vec<Value>,
    /// The tool that initiated the injection (from --tool; "unknown" when omitted).
    pub(crate) sender_tool: String,
    /// The coordination fact recording message content, or None for --handoff injects
    /// (which already have a handoff fact in the channel).
    pub(crate) content_fact: Option<Fact>,
    /// Whether the live backend delivery succeeded.
    pub(crate) delivered: bool,
}

/// Envelope for `inject`: result under `data.inject`.
#[derive(JsonSchema, Serialize)]
pub(crate) struct InjectEnvelope {
    pub(crate) inject: InjectData,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct SessionActionData {
    pub(crate) mode: &'static str,
    pub(crate) action: &'static str,
    pub(crate) session: ManagedSession,
    pub(crate) output: Option<String>,
    pub(crate) commands: Vec<Value>,
}

/// Envelope for session actions (attach/capture/stop): result under `data[action]`.
///
/// Since the action name is dynamic at runtime but the struct must be
/// serialized with a fixed key, we serialize to `Value` and re-key at call time.
pub(crate) struct SessionActionEnvelope {
    pub(crate) action_name: &'static str,
    pub(crate) data: SessionActionData,
}

impl SessionActionEnvelope {
    pub(crate) fn new(action_name: &'static str, data: SessionActionData) -> Self {
        Self { action_name, data }
    }
}

impl serde::Serialize for SessionActionEnvelope {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(self.action_name, &self.data)?;
        map.end()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    Tmux,
    Herdr,
    Cmux,
}

impl Backend {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" | "tmux" => Ok(Self::Tmux),
            "herdr" => Ok(Self::Herdr),
            "cmux" => Ok(Self::Cmux),
            other => Err(RallyError::Usage(format!("unsupported backend {other}"))),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
            Self::Cmux => "cmux",
        }
    }
}

pub(crate) struct BackendRunner {
    pub(crate) backend: Backend,
    tmux_bin: String,
    herdr_bin: String,
    herdr_socket: Option<String>,
    cmux_bin: String,
}

impl BackendRunner {
    pub(crate) fn new(backend: Backend, bins: BackendBins) -> Self {
        let herdr_bin = if bins.herdr_socket.is_some() && bins.herdr_bin == "herdr" {
            // Easy Terminal exposes a herdr-compatible daemon through ptyd; a
            // private socket without an explicit client should use that proven
            // CLI path instead of assuming a standalone `herdr` binary exists.
            default_private_socket_client()
        } else {
            bins.herdr_bin
        };
        Self {
            backend,
            tmux_bin: bins.tmux_bin,
            herdr_bin,
            herdr_socket: bins.herdr_socket,
            cmux_bin: bins.cmux_bin,
        }
    }

    pub(crate) fn start_commands(
        &self,
        target: &str,
        cwd: &Path,
        command: &[String],
        name: &str,
    ) -> Result<Vec<Vec<String>>> {
        let commands = match self.backend {
            Backend::Tmux => vec![tmux_start_command(&self.tmux_bin, target, cwd, command)?],
            Backend::Herdr => vec![herdr_start_command(
                &self.herdr_bin,
                target,
                cwd,
                command,
                herdr_agents_tab(&self.herdr_bin, self.herdr_socket.as_deref()),
                self.herdr_socket.as_deref(),
            )],
            Backend::Cmux => vec![cmux_start_command(
                &self.cmux_bin,
                target,
                cwd,
                command,
                name,
            )?],
        };
        Ok(commands)
    }

    pub(crate) fn start(
        &self,
        target: &str,
        cwd: &Path,
        command: &[String],
        name: &str,
    ) -> Result<String> {
        let commands = self.start_commands(target, cwd, command, name)?;
        match self.backend {
            Backend::Tmux | Backend::Herdr => run_commands(&commands).map(|()| target.to_string()),
            Backend::Cmux => {
                let output = run_command_output(first_command(&commands)?)?;
                parse_cmux_start_target(&output, target)
            }
        }
    }

    pub(crate) fn live_target(&self, session: &ManagedSession) -> Result<String> {
        match self.backend {
            Backend::Herdr => {
                herdr_live_pane(&self.herdr_bin, self.herdr_socket.as_deref(), session)
            }
            Backend::Tmux | Backend::Cmux => Ok(session.target.clone()),
        }
    }

    pub(crate) fn inject_commands(&self, target: &str, text: &str) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Tmux => tmux_inject_commands(&self.tmux_bin, target, text),
            Backend::Herdr => vec![
                herdr_command(
                    &self.herdr_bin,
                    self.herdr_socket.as_deref(),
                    cmd![&self.herdr_bin, "pane", "send-text", target, "\u{15}"],
                ),
                herdr_command(
                    &self.herdr_bin,
                    self.herdr_socket.as_deref(),
                    cmd![&self.herdr_bin, "pane", "send-text", target, text],
                ),
                herdr_command(
                    &self.herdr_bin,
                    self.herdr_socket.as_deref(),
                    cmd![&self.herdr_bin, "pane", "send-keys", target, "enter"],
                ),
            ],
            Backend::Cmux => vec![
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "ctrl+u"],
                cmd![&self.cmux_bin, "send", "--workspace", target, text],
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "enter"],
            ],
        }
    }

    pub(crate) fn inject(&self, target: &str, text: &str) -> Result<()> {
        run_commands(&self.inject_commands(target, text))
    }

    pub(crate) fn attach_commands(&self, target: &str) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Tmux => vec![cmd![&self.tmux_bin, "attach", "-t", target]],
            Backend::Herdr => vec![herdr_command(
                &self.herdr_bin,
                self.herdr_socket.as_deref(),
                cmd![&self.herdr_bin, "agent", "attach", target],
            )],
            Backend::Cmux => vec![cmd![
                &self.cmux_bin,
                "select-workspace",
                "--workspace",
                target,
            ]],
        }
    }

    pub(crate) fn attach(&self, target: &str) -> Result<()> {
        run_commands(&self.attach_commands(target))
    }

    pub(crate) fn capture_commands(&self, target: &str, lines: usize) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Tmux => vec![cmd![
                &self.tmux_bin,
                "capture-pane",
                "-pt",
                target,
                "-S",
                format!("-{lines}"),
            ]],
            Backend::Herdr => vec![herdr_command(
                &self.herdr_bin,
                self.herdr_socket.as_deref(),
                cmd![
                    &self.herdr_bin,
                    "agent",
                    "read",
                    target,
                    "--source",
                    "recent-unwrapped",
                    "--lines",
                    lines,
                ],
            )],
            Backend::Cmux => vec![cmd![
                &self.cmux_bin,
                "read-screen",
                "--workspace",
                target,
                "--scrollback",
                "--lines",
                lines,
            ]],
        }
    }

    pub(crate) fn capture(&self, target: &str, lines: usize) -> Result<String> {
        run_command_output(first_command(&self.capture_commands(target, lines))?)
    }

    pub(crate) fn stop_commands(&self, target: &str) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Tmux => vec![cmd![&self.tmux_bin, "kill-session", "-t", target]],
            Backend::Herdr => vec![herdr_command(
                &self.herdr_bin,
                self.herdr_socket.as_deref(),
                cmd![&self.herdr_bin, "agent", "stop", target],
            )],
            Backend::Cmux => vec![cmd![
                &self.cmux_bin,
                "close-workspace",
                "--workspace",
                target
            ]],
        }
    }

    pub(crate) fn stop(&self, target: &str) -> Result<()> {
        run_commands(&self.stop_commands(target))
    }
}

fn default_private_socket_client() -> String {
    if binary_on_path("ptyd") {
        return "ptyd".to_string();
    }
    ptyd_candidate_paths()
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "ptyd".to_string())
}

/// C-HERDR: discover an Easy Terminal herdr/ptyd socket path for `rally run
/// --backend herdr` callers that did NOT pass `--herdr-socket`. Without this,
/// `rally run --backend herdr` silently targets `~/.config/herdr/herdr.sock`
/// (the standalone daemon) instead of the Easy Terminal app's private socket,
/// so the launched agent ends up invisible to the Easy Terminal UI.
///
/// Precedence:
///   1. `PTYD_SOCKET_PATH` env (preferred — caller-explicit, no guesswork).
///   2. `HERDR_SOCKET_PATH` env.
///   3. macOS: `~/Library/Application Support/EasyTerminal/herdr.sock`.
///   4. Linux: `$XDG_RUNTIME_DIR/easy-terminal/herdr.sock` (future-proofing —
///      macOS ships now; Linux falls through to None until ET ports).
///
/// Returns `None` when no candidate exists on disk; callers should preserve
/// today's behavior (use the bare `herdr` binary against its default socket)
/// rather than fabricate a path that does not exist.
pub(crate) fn find_easy_terminal_socket() -> Option<PathBuf> {
    for env_var in ["PTYD_SOCKET_PATH", "HERDR_SOCKET_PATH"] {
        if let Ok(value) = env::var(env_var) {
            if !value.is_empty() {
                let path = PathBuf::from(&value);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    easy_terminal_socket_candidates()
        .into_iter()
        .find(|candidate| candidate.exists())
}

/// Disk locations to probe for the Easy Terminal herdr socket, in order.
/// Extracted as a separate function so unit tests can lock the candidate list
/// without invoking the env-first path.
fn easy_terminal_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        // macOS app sandbox / Application Support layout.
        candidates.push(home.join("Library/Application Support/EasyTerminal/herdr.sock"));
    }
    // Linux XDG runtime dir layout (future port; macOS will never match).
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            candidates.push(PathBuf::from(xdg).join("easy-terminal/herdr.sock"));
        }
    }
    candidates
}

fn binary_on_path(name: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn ptyd_candidate_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        candidates.push(
            cwd.join("build/Build/Products/Release/Easy Terminal.app/Contents/Resources/bin/ptyd"),
        );
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        candidates.push(
            home.join(
                "dev/git-folder/easy-terminal/build/Build/Products/Release/Easy Terminal.app/Contents/Resources/bin/ptyd",
            ),
        );
        candidates.push(home.join("dev/git-folder/ptyd/target/debug/ptyd"));
        candidates.push(home.join("dev/git-folder/ptyd/target/release/ptyd"));
    }
    candidates.push(PathBuf::from(
        "/Applications/Easy Terminal.app/Contents/Resources/bin/ptyd",
    ));
    candidates
}

fn tmux_start_command(
    bin: &str,
    session: &str,
    cwd: &Path,
    command: &[String],
) -> Result<Vec<String>> {
    let shell_command = format!(
        "cd {} && exec {}",
        shell_quote(&cwd.display().to_string()),
        shell_words(command)?
    );
    Ok(cmd![
        bin,
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        "140",
        "-y",
        "50",
        shell_command,
    ])
}

fn tmux_inject_commands(bin: &str, session: &str, text: &str) -> Vec<Vec<String>> {
    let buffer = format!("rally-inject-{session}");
    vec![
        cmd![bin, "send-keys", "-t", session, "C-u"],
        cmd![bin, "set-buffer", "-b", &buffer, text],
        cmd![bin, "paste-buffer", "-b", buffer, "-t", session],
        cmd![bin, "send-keys", "-t", session, "Enter"],
    ]
}

fn herdr_start_command(
    bin: &str,
    target: &str,
    cwd: &Path,
    command: &[String],
    tab: Option<String>,
    socket: Option<&str>,
) -> Vec<String> {
    let mut args = cmd![bin, "agent", "start", target, "--cwd", cwd.display(),];
    if let Some(tab) = tab {
        args.extend(cmd!["--tab", tab]);
    }
    args.extend(cmd!["--no-focus", "--"]);
    args.extend(command.iter().cloned());
    herdr_command(bin, socket, args)
}

fn herdr_command(bin: &str, socket: Option<&str>, command: Vec<String>) -> Vec<String> {
    match socket {
        Some(socket) => {
            let mut wrapped = cmd![
                "env",
                format!("PTYD_SOCKET_PATH={socket}"),
                format!("HERDR_SOCKET_PATH={socket}"),
                bin,
            ];
            wrapped.extend(command.into_iter().skip(1));
            wrapped
        }
        None => command,
    }
}

fn herdr_agents_tab(bin: &str, socket: Option<&str>) -> Option<String> {
    let command = herdr_command(bin, socket, cmd![bin, "tab", "list"]);
    let output = run_command_output(&command).ok()?;
    parse_herdr_agents_tab(&output)
}

fn parse_herdr_agents_tab(output: &str) -> Option<String> {
    let value: Value = serde_json::from_str(output).ok()?;
    let tabs = value.pointer("/result/tabs").and_then(Value::as_array)?;
    let workspace_id = tabs
        .iter()
        .find(|tab| tab.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|tab| tab.get("workspace_id").and_then(Value::as_str))?;
    tabs.iter()
        .find(|tab| {
            tab.get("workspace_id").and_then(Value::as_str) == Some(workspace_id)
                && tab.get("label").and_then(Value::as_str) == Some("agents")
                && tab.get("focused").and_then(Value::as_bool) != Some(true)
        })
        .and_then(|tab| tab.get("tab_id").and_then(Value::as_str))
        .map(str::to_string)
}

fn cmux_start_command(
    bin: &str,
    target: &str,
    cwd: &Path,
    command: &[String],
    name: &str,
) -> Result<Vec<String>> {
    let layout = json!({
        "pane": {
            "surfaces": [
                {
                    "type": "terminal",
                    "command": shell_words(command)?
                }
            ]
        }
    })
    .to_string();
    Ok(cmd![
        bin,
        "new-workspace",
        "--name",
        name,
        "--description",
        target,
        "--cwd",
        cwd.display(),
        "--layout",
        layout,
        "--focus",
        "false",
    ])
}

pub(crate) fn parse_cmux_start_target(output: &str, fallback: &str) -> Result<String> {
    output
        .split_whitespace()
        .find_map(|word| {
            let value = word.trim_matches(|ch: char| {
                ch == '"' || ch == '\'' || ch == ',' || ch == ';' || ch == '.'
            });
            if value.starts_with("workspace:") {
                Some(value.to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            RallyError::Command(format!(
                "cmux did not report a workspace ref for {fallback}; stdout: {}",
                output.trim()
            ))
        })
}

fn herdr_live_pane(bin: &str, socket: Option<&str>, session: &ManagedSession) -> Result<String> {
    let command = herdr_command(bin, socket, cmd![bin, "agent", "list"]);
    let output = run_command_output(&command)?;
    if output.trim().is_empty() {
        return Ok(session.target.clone());
    }
    let value: Value =
        serde_json::from_str(&output).map_err(RallyError::json("parse herdr agent list"))?;
    resolve_agent_pane_from_list(&value, session)
}

fn resolve_agent_pane_from_list(value: &Value, session: &ManagedSession) -> Result<String> {
    let agents = value
        .pointer("/result/agents")
        .or_else(|| value.pointer("/result/panes"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RallyError::Message("herdr agent list did not return agents or panes".to_string())
        })?;
    for agent in agents {
        let matches = ["name", "pane_id", "terminal_id", "label"]
            .iter()
            .filter_map(|key| agent.get(*key).and_then(Value::as_str))
            .any(|value| {
                value == session.target
                    || value == session.session_id
                    || value == session.name
                    || value == session.tool
            });
        if matches {
            if let Some(pane_id) = agent.get("pane_id").and_then(Value::as_str) {
                return Ok(pane_id.to_string());
            }
        }
    }
    Err(RallyError::NotFound(format!(
        "herdr managed session {} is not currently live",
        session.session_id
    )))
}

pub(crate) fn command_plan_json(commands: &[Vec<String>]) -> Vec<Value> {
    commands.iter().map(|command| json!(command)).collect()
}

fn first_command(commands: &[Vec<String>]) -> Result<&[String]> {
    commands
        .first()
        .map(Vec::as_slice)
        .ok_or_else(|| RallyError::Command("empty command plan".to_string()))
}

fn run_commands(commands: &[Vec<String>]) -> Result<()> {
    for command in commands {
        run_command_owned(command)?;
    }
    Ok(())
}

fn run_command_owned(args: &[String]) -> Result<()> {
    let (bin, rest) = args
        .split_first()
        .ok_or_else(|| RallyError::Command("empty command".to_string()))?;
    let status = Command::new(bin)
        .args(rest)
        .status()
        .map_err(|err| RallyError::Command(format!("run {bin}: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(RallyError::Command(format!("{bin} exited with {status}")))
    }
}

fn run_command_output(args: &[String]) -> Result<String> {
    let (bin, rest) = args
        .split_first()
        .ok_or_else(|| RallyError::Command("empty command".to_string()))?;
    let output = Command::new(bin)
        .args(rest)
        .output()
        .map_err(|err| RallyError::Command(format!("run {bin}: {err}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(RallyError::Command(format!(
            "{bin} exited with {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn shell_words(words: &[String]) -> Result<String> {
    shlex::try_join(words.iter().map(String::as_str))
        .map_err(|err| RallyError::Usage(format!("agent command cannot be shell-quoted: {err}")))
}
#[cfg(test)]
mod tests {
    use super::{InjectData, RunData, SessionActionData, SessionsData};
    use super::{
        herdr_command, parse_cmux_start_target, parse_herdr_agents_tab,
        resolve_agent_pane_from_list, shell_words,
    };
    use crate::check::CheckData;
    use crate::store::Fact;
    use crate::{EnterData, Envelope, NextData, RoomData, SayData};
    use schemars::schema_for;
    use serde_json::json;

    #[test]
    fn cmux_start_target_uses_workspace_ref_from_status_output() {
        assert_eq!(
            parse_cmux_start_target("OK workspace:11\n", "claude-cmux-smoke").unwrap(),
            "workspace:11"
        );
    }

    #[test]
    fn cmux_start_target_rejects_output_without_workspace_ref() {
        let err = parse_cmux_start_target("created workspace\n", "claude-cmux-smoke").unwrap_err();
        assert!(err.to_string().contains("did not report a workspace ref"));
    }

    #[test]
    fn shell_words_rejects_nul_bytes() {
        let command = vec!["claude".to_string(), "bad\0arg".to_string()];
        let err = shell_words(&command).unwrap_err();
        assert!(err.to_string().contains("cannot be shell-quoted"));
    }

    #[test]
    fn herdr_agents_tab_uses_agents_tab_from_focused_workspace() {
        let output = json!({
            "result": {
                "tabs": [
                    {
                        "focused": false,
                        "label": "agents",
                        "tab_id": "other:2",
                        "workspace_id": "other"
                    },
                    {
                        "focused": true,
                        "label": "main",
                        "tab_id": "current:1",
                        "workspace_id": "current"
                    },
                    {
                        "focused": false,
                        "label": "agents",
                        "tab_id": "current:2",
                        "workspace_id": "current"
                    }
                ]
            }
        })
        .to_string();

        assert_eq!(
            parse_herdr_agents_tab(&output).as_deref(),
            Some("current:2")
        );
    }

    #[test]
    fn herdr_agents_tab_does_not_return_focused_agents_tab() {
        let output = json!({
            "result": {
                "tabs": [
                    {
                        "focused": true,
                        "label": "agents",
                        "tab_id": "current:1",
                        "workspace_id": "current"
                    }
                ]
            }
        })
        .to_string();

        assert_eq!(parse_herdr_agents_tab(&output), None);
    }

    #[test]
    fn herdr_command_wraps_private_socket_for_ptyd_and_herdr_clients() {
        let command = herdr_command(
            "ptyd",
            Some("/tmp/easy-terminal/herdr.sock"),
            vec![
                "ptyd".to_string(),
                "agent".to_string(),
                "start".to_string(),
                "codex-01".to_string(),
            ],
        );

        assert_eq!(
            command,
            vec![
                "env",
                "PTYD_SOCKET_PATH=/tmp/easy-terminal/herdr.sock",
                "HERDR_SOCKET_PATH=/tmp/easy-terminal/herdr.sock",
                "ptyd",
                "agent",
                "start",
                "codex-01",
            ]
        );
    }

    #[test]
    fn ptyd_agent_list_shape_resolves_live_herdr_target() {
        let session = super::ManagedSession {
            session_id: "codex-01".to_string(),
            name: "codex-01".to_string(),
            agent: "codex".to_string(),
            tool: "codex:01".to_string(),
            backend: "herdr".to_string(),
            cwd: std::path::PathBuf::from("/tmp/repo"),
            target: "codex-01".to_string(),
        };
        let output = json!({
            "result": {
                "panes": [
                    {
                        "pane_id": "pane-abc",
                        "terminal_id": "term-abc",
                        "label": "codex-01"
                    }
                ]
            }
        });

        let pane_id = resolve_agent_pane_from_list(&output, &session).unwrap();
        assert_eq!(pane_id, "pane-abc");
    }

    #[test]
    fn command_contracts_have_typed_json_schemas() {
        let schemas = [
            serde_json::to_value(schema_for!(Fact)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<EnterData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<SayData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<RoomData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<NextData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<CheckData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<RunData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<SessionsData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<InjectData>)).unwrap(),
            serde_json::to_value(schema_for!(Envelope<SessionActionData>)).unwrap(),
        ];
        assert!(schemas.iter().all(|schema| schema.is_object()));
    }

    /// Shared mutex for all `find_et_socket_*` tests. Each test mutates
    /// process-global env (`PTYD_SOCKET_PATH`, `HOME`, etc.) and reads it
    /// back; without serialization, concurrent test threads race and one
    /// reads the other's mutated env. Matches the discovery.rs::b17 pattern.
    /// Poison-tolerant so a panicking holder cannot wedge the rest of the
    /// suite.
    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// C-HERDR: PTYD_SOCKET_PATH always wins when set and the file exists.
    #[test]
    fn find_et_socket_prefers_ptyd_socket_path_env() {
        let _guard = env_test_lock();
        let tmp = std::env::temp_dir().join(format!("rally-et-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock = tmp.join("herdr.sock");
        std::fs::write(&sock, b"").unwrap();
        let prior_ptyd = std::env::var("PTYD_SOCKET_PATH").ok();
        let prior_herdr = std::env::var("HERDR_SOCKET_PATH").ok();
        let prior_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prior_home = std::env::var("HOME").ok();
        // SAFETY: these unsafe blocks are intentional — tests must control env
        // for the duration of the assertion. Restored in the finally-equivalent
        // tail of the test.
        unsafe {
            std::env::set_var("PTYD_SOCKET_PATH", &sock);
            std::env::remove_var("HERDR_SOCKET_PATH");
            // Set HOME to nonexistent so candidate list misses, isolating
            // the env-first arm.
            std::env::set_var("HOME", "/nonexistent-rally-et-test");
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        let found = super::find_easy_terminal_socket();
        assert_eq!(found.as_deref(), Some(sock.as_path()));

        unsafe {
            match prior_ptyd {
                Some(v) => std::env::set_var("PTYD_SOCKET_PATH", v),
                None => std::env::remove_var("PTYD_SOCKET_PATH"),
            }
            match prior_herdr {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// C-HERDR: empty env + no candidate socket on disk returns None — caller
    /// preserves the pre-discovery default (bare `herdr`).
    #[test]
    fn find_et_socket_returns_none_when_no_candidate_exists() {
        let _guard = env_test_lock();
        let prior_ptyd = std::env::var("PTYD_SOCKET_PATH").ok();
        let prior_herdr = std::env::var("HERDR_SOCKET_PATH").ok();
        let prior_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prior_home = std::env::var("HOME").ok();
        unsafe {
            std::env::remove_var("PTYD_SOCKET_PATH");
            std::env::remove_var("HERDR_SOCKET_PATH");
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("HOME", "/nonexistent-rally-et-noop");
        }

        let found = super::find_easy_terminal_socket();
        assert!(
            found.is_none(),
            "expected None when no env + no on-disk socket; got: {:?}",
            found
        );

        unsafe {
            match prior_ptyd {
                Some(v) => std::env::set_var("PTYD_SOCKET_PATH", v),
                None => std::env::remove_var("PTYD_SOCKET_PATH"),
            }
            match prior_herdr {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// C-HERDR: PTYD_SOCKET_PATH pointing at a NONEXISTENT path falls through
    /// to disk candidates — env-set-but-stale must not poison the search.
    #[test]
    fn find_et_socket_falls_through_stale_env() {
        let _guard = env_test_lock();
        let tmp = std::env::temp_dir().join(format!("rally-et-fallthrough-{}", std::process::id()));
        let appsupp = tmp.join("Library/Application Support/EasyTerminal");
        std::fs::create_dir_all(&appsupp).unwrap();
        let sock = appsupp.join("herdr.sock");
        std::fs::write(&sock, b"").unwrap();

        let prior_ptyd = std::env::var("PTYD_SOCKET_PATH").ok();
        let prior_herdr = std::env::var("HERDR_SOCKET_PATH").ok();
        let prior_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prior_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("PTYD_SOCKET_PATH", "/this/path/never/exists.sock");
            std::env::remove_var("HERDR_SOCKET_PATH");
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("HOME", &tmp);
        }

        let found = super::find_easy_terminal_socket();
        assert_eq!(
            found.as_deref(),
            Some(sock.as_path()),
            "stale env path must fall through to disk candidate"
        );

        unsafe {
            match prior_ptyd {
                Some(v) => std::env::set_var("PTYD_SOCKET_PATH", v),
                None => std::env::remove_var("PTYD_SOCKET_PATH"),
            }
            match prior_herdr {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
            match prior_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prior_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
