use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    /// Filesystem path of the dedicated linked git worktree provisioned for
    /// this agent, when worktree-per-agent isolation is in effect. `None`
    /// for sessions launched with `--shared`/`--no-worktree`, for sessions
    /// recorded before Phase 1b shipped, or under dry-run when no worktree
    /// is actually created.  Used at session stop to clean up the worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worktree_path: Option<PathBuf>,
    /// Name of the per-agent git branch created off the run base when the
    /// worktree was provisioned.  Set together with `worktree_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
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
    /// **Compatibility field.** Whether the synchronous backend delivery
    /// succeeded. Becomes `true` ONLY when `delivery_state in
    /// {Delivered, Seen, Acted}`; `false` covers BOTH `Pending` (in-flight)
    /// AND `Failed` outcomes. Prefer `delivery_state` for new code; this
    /// field is preserved for downstream tools that scrape the existing JSON
    /// envelope.
    pub(crate) delivered: bool,
    /// **Plan F.** The truthful delivery state, mirroring
    /// `rally_protocol::DeliveryStatus`. `Pending` means the Directive has
    /// been durably appended to the ledger but no Receipt has arrived yet
    /// (the daemon is the canonical receipt-poster; absent it, a cooperating
    /// agent self-acks). Wire shape: snake_case (`pending|delivered|seen|
    /// acted|failed`).
    pub(crate) delivery_state: &'static str,
    /// **Plan F.** The assigned per-inbox sequence of the Directive this
    /// inject wrote. `None` in dry-run or when the inject was a no-op.
    /// Consumers may pass this through to `rally status` to look up the
    /// matching Receipt.
    pub(crate) directive_seq: Option<u64>,
    /// **Plan F.** Logical agent id the Directive was written to (mirrors
    /// `session.tool` for the common case; surfaced explicitly so consumer
    /// tools don't have to thread through the session blob).
    pub(crate) directive_to: Option<String>,
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

// Plan F functional core (Chunk 3): the `Backend::Herdr` variant and its
// run/start/attach/capture/stop paths are REMOVED. herdr was the legacy
// "rally calls the daemon" path that the F architecture inverted. Plan F
// rally writes Directives to the .rally ledger; the daemon SUBSCRIBES.
// The 34-caller audit (tools/check_inject_callsites.sh) stays green
// because the inject critical path was already routed through the
// ledger writer in Plan F P2. Only the backend enum + its callers
// in this file are removed here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    Tmux,
    Cmux,
}

impl Backend {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" | "tmux" => Ok(Self::Tmux),
            "cmux" => Ok(Self::Cmux),
            "herdr" => Err(RallyError::Usage(
                "backend \"herdr\" is removed (Plan F): use the .rally ledger \
                 (rally inject) and the rally-termd daemon; or fall back to tmux/cmux"
                    .to_string(),
            )),
            other => Err(RallyError::Usage(format!("unsupported backend {other}"))),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Cmux => "cmux",
        }
    }
}

pub(crate) struct BackendRunner {
    pub(crate) backend: Backend,
    tmux_bin: String,
    cmux_bin: String,
}

impl BackendRunner {
    pub(crate) fn new(backend: Backend, bins: BackendBins) -> Self {
        // herdr_bin / herdr_socket fields removed with Backend::Herdr;
        // callers that pass them get ignored fields (BackendBins still
        // carries them for now to keep CLI parsing stable — they're
        // de-facto dead but a separate cleanup pass).
        let _ = bins.herdr_bin;
        let _ = bins.herdr_socket;
        Self {
            backend,
            tmux_bin: bins.tmux_bin,
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
            Backend::Tmux => run_commands(&commands).map(|()| target.to_string()),
            Backend::Cmux => {
                let output = run_command_output(first_command(&commands)?)?;
                parse_cmux_start_target(&output, target)
            }
        }
    }

    pub(crate) fn live_target(&self, session: &ManagedSession) -> Result<String> {
        match self.backend {
            Backend::Tmux | Backend::Cmux => Ok(session.target.clone()),
        }
    }

    pub(crate) fn inject_commands(&self, target: &str, text: &str) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Tmux => tmux_inject_commands(&self.tmux_bin, target, text),
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

// Plan F functional core (Chunk 3): default_private_socket_client +
// binary_on_path + ptyd_candidate_paths used to resolve the herdr-or-ptyd
// CLI client when Backend::Herdr was active. With the herdr backend
// removed, all three are dead. The Plan F daemon (rally-termd) is
// addressed via the .rally ledger, not via a CLI client path.

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

// Plan F functional core (Chunk 3): herdr_* helpers are removed with
// the Backend::Herdr enum arm. The Plan F daemon addresses agents by
// logical id through the .rally ledger; there is no rally-side CLI
// shim into a daemon binary anymore.

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

// Plan F functional core (Chunk 3): herdr_live_pane and
// resolve_agent_pane_from_list are removed with the Backend::Herdr
// enum arm — the Plan F daemon addresses agents by logical id via the
// .rally ledger, not by walking a daemon-side pane list.

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
    // Plan F functional core (Chunk 3): herdr_command, parse_herdr_agents_tab,
    // and resolve_agent_pane_from_list removed with the Backend::Herdr arm.
    use super::{parse_cmux_start_target, shell_words};
    use crate::check::CheckData;
    use crate::store::Fact;
    use crate::{EnterData, Envelope, NextData, RoomData, SayData};
    use schemars::schema_for;

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

    // Plan F functional core (Chunk 3): herdr_agents_tab_*, herdr_command_*,
    // and ptyd_agent_list_shape_resolves_live_herdr_target unit tests
    // removed alongside the Backend::Herdr arm and its parser helpers.

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
}
