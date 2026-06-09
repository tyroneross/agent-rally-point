use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output as ProcessOutput};

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

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionLiveness {
    Live,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct SessionView {
    #[serde(flatten)]
    pub(crate) session: ManagedSession,
    pub(crate) liveness: SessionLiveness,
    pub(crate) liveness_source: &'static str,
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
    pub(crate) sessions: Vec<SessionView>,
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
    /// The matched managed session for `target_kind == "managed_session"`;
    /// `None` for `target_kind == "ledger_agent"` (rally-termd-registered
    /// ptyd-pane identities have no `ManagedSession` record).
    ///
    /// Serialized as `null` for ledger-only injects rather than omitted, so
    /// downstream JSON consumers can branch on a stable field shape (the
    /// `target_kind` field below is the authoritative discriminator).
    pub(crate) session: Option<ManagedSession>,
    /// Discriminator: `"managed_session"` (tmux/cmux/herdr — dual-delivery
    /// path, intentional in P2) or `"ledger_agent"` (rally-termd-registered
    /// agent — ledger-only delivery; rally-termd performs the PTY-write and
    /// posts a Receipt). Consumers should branch on this, not on `session`.
    pub(crate) target_kind: &'static str,
    pub(crate) handoff: Option<String>,
    pub(crate) require_ack: bool,
    pub(crate) ack: Option<Value>,
    /// Whether the target has posted Rally evidence for this injection.
    /// Transport success alone does not set this true.
    pub(crate) verified_received: bool,
    /// Machine-readable ACK lifecycle for callers that need fallback routing.
    /// Values: `not_required`, `planned`, `acked`, `blocked`, `timeout`.
    pub(crate) ack_state: &'static str,
    /// Present when an ACK was required but did not arrive. This is the
    /// deterministic fallback tree callers should execute instead of assuming
    /// the injected text was read.
    pub(crate) fallback_plan: Option<Value>,
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
        // PROVENANCE: the BackendBins struct previously carried `herdr_bin` and
        // `herdr_socket`, which this constructor ignored once Backend::Herdr was
        // removed in Plan F. Those fields and their CLI flags have now been
        // deleted at the source; nothing to discard here anymore.
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
            // cmux kept as the separate-submit sequence: its `send` subcommand
            // accepts literal text only (and `send-key <name>` named keys) —
            // there is no raw-byte / hex write equivalent to tmux's
            // `send-keys -H`, so the atomic bracketed-paste frame (ESC[200~ …
            // ESC[201~ + CR) cannot be expressed. `send-key enter` submits as a
            // discrete key, which works for cmux's own TUI; the framed-write
            // fix is tmux-specific (where Codex's bracketed-paste TUI lives).
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

    pub(crate) fn liveness(&self, targets: &[String]) -> Vec<SessionLiveness> {
        if targets.is_empty() {
            return Vec::new();
        }
        match self.backend {
            Backend::Tmux => probe_tmux_liveness(&self.tmux_bin, targets),
            Backend::Cmux => probe_cmux_liveness(&self.cmux_bin, targets),
        }
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

/// Bracketed-paste start marker: `ESC [ 200 ~`.
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker: `ESC [ 201 ~`.
const PASTE_END: &[u8] = b"\x1b[201~";
/// Carriage return — the submit byte.
const CR: u8 = 0x0D;

/// Strip control bytes from inject text BEFORE it is framed, so the body can
/// never carry its own bracketed-paste end marker (`ESC[201~`) or a raw submit
/// CR. Mirrors ptyd's `sanitize_delivery_text` (ptyd `src/termd.rs`,
/// Apache-2.0) — keep printable chars plus `\t`; drop every C0 control, DEL,
/// and ESC (0x1B). This closes a paste-breakout: without it, a `--text`
/// payload containing `ESC[201~` would close the frame early and everything
/// after it (including a CR) would reach the shell as live keystrokes — the
/// exact L7/SEC keystroke-injection class. Newline is also dropped here (unlike
/// ptyd's daemon path, which keeps `\n` as paste content) because this fallback
/// appends its OWN submit CR after the frame; a body newline could otherwise
/// submit a partial line inside a non-paste-aware target.
fn sanitize_inject_text(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\t' || (!c.is_control()))
        .collect()
}

/// Build the framed byte string for a submit-delivery, mirroring ptyd's
/// `frame_line(text, submit=true, paste_frame=true)` (ptyd `src/comms.rs`
/// §4.1/§4.2, Apache-2.0, same author — reimplemented here so this repo stays
/// self-contained with no path dependency on ptyd).
///
/// The body is first run through [`sanitize_inject_text`] so it cannot carry
/// its own paste-end marker or control bytes (paste-breakout hardening).
///
/// Layout: `ESC[200~ <sanitized-body> ESC[201~` followed by a single CR placed
/// **after** the closing bracketed-paste marker — never inside the frame, where
/// bracketed-paste semantics would paste the CR as literal text instead of
/// submitting (§4.2). A paste-aware TUI (codex) treats the wrapped body as a
/// paste; the trailing CR then submits the prompt. The separate-Enter sequence
/// this replaces empirically failed against Codex's TUI: the message landed in
/// the input box but never submitted.
fn frame_line_bytes(text: &str) -> Vec<u8> {
    let body = sanitize_inject_text(text);
    let mut out = Vec::with_capacity(body.len() + PASTE_START.len() + PASTE_END.len() + 1);
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(PASTE_END);
    out.push(CR);
    out
}

/// Encode raw bytes as the lowercase 2-hex-digit tokens `tmux send-keys -H`
/// expects (one token per byte). `send-keys -H 1b 5b 32 30 30 7e …` writes the
/// exact bytes to the pane with no key-name interpretation, so the whole frame
/// — markers, body, and submit CR — arrives in ONE atomic tmux write rather
/// than the prior four separate commands.
fn hex_tokens(bytes: &[u8]) -> Vec<String> {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn tmux_inject_commands(bin: &str, session: &str, text: &str) -> Vec<Vec<String>> {
    // C-u clears any stale input still sitting at the prompt; kept as its own
    // prior command (it is a control-key chord, not part of the framed paste).
    let clear = cmd![bin, "send-keys", "-t", session, "C-u"];
    // The framed paste + submit CR delivered as a SINGLE hex send-keys write.
    let mut framed = cmd![bin, "send-keys", "-t", session, "-H"];
    framed.extend(hex_tokens(&frame_line_bytes(text)));
    vec![clear, framed]
}

fn probe_tmux_liveness(bin: &str, targets: &[String]) -> Vec<SessionLiveness> {
    let output = Command::new(bin)
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\n#{window_id}\n#{pane_id}",
        ])
        .output();
    classify_probe_output(output, targets)
}

fn probe_cmux_liveness(bin: &str, targets: &[String]) -> Vec<SessionLiveness> {
    let output = Command::new(bin).arg("list-workspaces").output();
    classify_probe_output(output, targets)
}

fn classify_probe_output(
    output: std::io::Result<ProcessOutput>,
    targets: &[String],
) -> Vec<SessionLiveness> {
    let Ok(output) = output else {
        return targets.iter().map(|_| SessionLiveness::Unknown).collect();
    };
    if output.status.success() {
        let live_targets = target_tokens(&String::from_utf8_lossy(&output.stdout));
        if live_targets.is_empty() {
            return targets.iter().map(|_| SessionLiveness::Unknown).collect();
        }
        return targets
            .iter()
            .map(|target| {
                if live_targets.contains(target) {
                    SessionLiveness::Live
                } else {
                    SessionLiveness::Stale
                }
            })
            .collect();
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    let status = if stderr.contains("no server running")
        || stderr.contains("no such file or directory")
        || stderr.contains("can't find")
        || stderr.contains("not found")
    {
        SessionLiveness::Stale
    } else {
        SessionLiveness::Unknown
    };
    targets.iter().map(|_| status).collect()
}

fn target_tokens(output: &str) -> BTreeSet<String> {
    output
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| {
                ch == '"' || ch == '\'' || ch == ',' || ch == ';' || ch == '.'
            })
        })
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
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
    use super::{
        CR, PASTE_END, PASTE_START, frame_line_bytes, hex_tokens, parse_cmux_start_target,
        sanitize_inject_text, shell_words, tmux_inject_commands,
    };
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

    // ---- frame_line port (ptyd src/comms.rs §4.1/§4.2) -------------------

    #[test]
    fn frame_line_wraps_body_and_appends_cr_after_close_marker() {
        let got = frame_line_bytes("hello");
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[200~hello\x1b[201~");
        want.push(0x0d);
        assert_eq!(got, want);
    }

    #[test]
    fn frame_line_cr_is_outside_the_frame() {
        let got = frame_line_bytes("x");
        // Last byte is the submit CR; the byte before it is the final byte of
        // the closing marker (`~`) — the CR is never inside the paste body.
        assert_eq!(*got.last().unwrap(), CR);
        assert_eq!(got[got.len() - 2], b'~');
        // The body sits strictly between the two markers.
        assert!(got.starts_with(PASTE_START));
        let after_start = &got[PASTE_START.len()..];
        assert!(after_start.starts_with(b"x"));
        assert!(after_start[1..].starts_with(PASTE_END));
    }

    #[test]
    fn frame_line_passes_printable_multibyte_through_verbatim() {
        // UTF-8 multibyte printable body bytes pass through verbatim.
        let got = frame_line_bytes("café✓");
        let mut want = Vec::new();
        want.extend_from_slice(PASTE_START);
        want.extend_from_slice("café✓".as_bytes());
        want.extend_from_slice(PASTE_END);
        want.push(CR);
        assert_eq!(got, want);
    }

    #[test]
    fn frame_line_strips_embedded_paste_end_marker_breakout() {
        // A malicious body carrying its own ESC[201~ (+ a shell line + CR) must
        // NOT close the frame early. The ESC and CR are control bytes and are
        // stripped; only printable residue survives, safely inside the frame.
        let attack = "ok\x1b[201~rm -rf /\r";
        let got = frame_line_bytes(attack);
        // There must be exactly ONE close marker in the output: the framer's own.
        let occurrences = got
            .windows(PASTE_END.len())
            .filter(|w| *w == PASTE_END)
            .count();
        assert_eq!(occurrences, 1, "no attacker-supplied close marker survives");
        // Exactly ONE CR — the framer's submit byte, as the final byte.
        assert_eq!(got.iter().filter(|&&b| b == CR).count(), 1);
        assert_eq!(*got.last().unwrap(), CR);
        // The single close marker is immediately before the submit CR.
        assert_eq!(
            &got[got.len() - 1 - PASTE_END.len()..got.len() - 1],
            PASTE_END
        );
        // No ESC byte survives inside the body (all stripped except the markers').
        // The only ESC bytes are the two framer markers (start + the surviving end).
        assert_eq!(got.iter().filter(|&&b| b == 0x1b).count(), 2);
    }

    #[test]
    fn sanitize_inject_text_keeps_printable_and_tab_drops_controls() {
        assert_eq!(sanitize_inject_text("hello world"), "hello world");
        assert_eq!(sanitize_inject_text("a\tb"), "a\tb");
        // ESC, CR, LF, NUL, DEL all dropped.
        assert_eq!(sanitize_inject_text("a\x1bb\rc\nd\0e\x7ff"), "abcdef");
        assert_eq!(sanitize_inject_text("café✓"), "café✓");
    }

    #[test]
    fn hex_tokens_encodes_each_byte_as_lowercase_two_digits() {
        assert_eq!(
            hex_tokens(b"\x1b[200~"),
            vec!["1b", "5b", "32", "30", "30", "7e"]
        );
        assert_eq!(hex_tokens(&[0x00, 0x0d, 0xff]), vec!["00", "0d", "ff"]);
        assert_eq!(hex_tokens(&[]), Vec::<String>::new());
    }

    #[test]
    fn tmux_inject_clears_then_sends_one_framed_hex_write() {
        let cmds = tmux_inject_commands("tmux", "rally-codex", "do the thing");
        // Exactly two commands: the C-u clear, then the single framed -H write.
        assert_eq!(cmds.len(), 2, "must be one clear + one atomic framed write");
        assert_eq!(
            cmds[0],
            vec!["tmux", "send-keys", "-t", "rally-codex", "C-u"]
        );
        // The second command is a single send-keys -H with hex tokens for the
        // whole frame — NOT a separate paste-buffer + Enter pair.
        let framed = &cmds[1];
        assert_eq!(
            &framed[..5],
            &["tmux", "send-keys", "-t", "rally-codex", "-H"]
        );
        let hex: Vec<u8> = framed[5..]
            .iter()
            .map(|t| u8::from_str_radix(t, 16).unwrap())
            .collect();
        assert_eq!(hex, frame_line_bytes("do the thing"));
        // The decoded frame ends in CR (submit) right after the close marker.
        assert_eq!(*hex.last().unwrap(), CR);
        assert_eq!(hex[hex.len() - 2], b'~');
        // No legacy paste-buffer / set-buffer / separate Enter survives.
        for cmd in &cmds {
            assert!(!cmd.iter().any(|a| a == "paste-buffer" || a == "set-buffer"));
            assert!(!cmd.iter().any(|a| a == "Enter"));
        }
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
