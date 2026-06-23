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

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
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
    /// Daemon-first inject routing (move 2): `true` when this session's pane was
    /// registered with the rally-termd daemon (`agent.register`), so the daemon
    /// owns the PTY and `inject` routes LEDGER-ONLY (the daemon performs the
    /// PTY-write + posts a Receipt). `false`/absent → the framed tmux write is
    /// the operative delivery (`delivery_path: "tmux_framed_fallback"`).
    /// Defaults to `false` so every pre-existing session record stays on the
    /// fallback path with zero behavior change.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) daemon_registered: bool,
    /// The daemon-owned pane handle returned by `agent.register`
    /// (`Registered.pane_id`). `None` unless `daemon_registered` is true.
    /// Surfaced under `rally sessions` so a host can see the daemon binding
    /// (acceptance criterion 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_pane: Option<String>,
    /// [E]: the EXACT rally-owned ptyd socket the spawn path used to spawn +
    /// register this `Backend::Ptyd` pane. Pinned on the session so every later
    /// op (send/stop/read/liveness) reaches the SAME daemon the pane lives in —
    /// not a possibly-different socket re-resolved at call time. `None` for
    /// tmux/cmux sessions and for ptyd sessions recorded before this field
    /// shipped (those fall back to `rally_owned_socket()` resolution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_socket: Option<String>,
}

/// Serde skip helper: omit `daemon_registered` from JSON when false so existing
/// session-record shapes are byte-identical until a session is daemon-bound.
fn is_false(b: &bool) -> bool {
    !*b
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
    /// F2: a LOUD warning when a ptyd spawn succeeded but its `agent.register`
    /// failed, forcing a tmux fallback launch (the spawned daemon pane was
    /// reaped first — no silent orphan). `None` on the happy path. Surfaced so
    /// a host never silently believes it got a daemon-owned pane when it didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) warning: Option<String>,
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
    /// **Daemon-first inject routing (move 2).** Which delivery tier carried
    /// this inject:
    ///   - `"daemon"` — the session is daemon-registered; ONLY the ledger
    ///     Directive was written and the rally-termd daemon owns the PTY-write
    ///     (NO tmux/cmux keystrokes fired). This is the north-star path.
    ///   - `"tmux_framed_fallback"` — no daemon binding; the CLI performed the
    ///     framed `send-keys` write (the degraded-but-correct fallback).
    ///   - `"ledger_only"` — a `LedgerAgent` target (externally-registered
    ///     ptyd pane with no managed session); already daemon-delivered.
    ///
    /// Consumers branch on this to know whether a keystroke write happened.
    pub(crate) delivery_path: &'static str,
    /// ptyd pane-ownership flip: the `state` of the daemon's `agent.send`
    /// Receipt (`sent|seen|acted`) when `delivery_path == "daemon"` and the send
    /// succeeded; `None` for non-daemon paths or a failed/mismatched daemon
    /// send. Lets a caller see how far the daemon delivery got without scraping
    /// the ledger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_receipt_state: Option<String>,
    /// ptyd pane-ownership flip: when a daemon-routed send FAILED (RPC error or
    /// the F4 `daemon_pane_mismatch` cross-check), the honest reason. `None` on
    /// success or non-daemon paths. The directive stays Pending on the ledger;
    /// the CLI does NOT fall back to tmux keystrokes for a ptyd pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_delivery_error: Option<String>,
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
    /// ptyd pane-ownership flip: the agent runs as a pane OWNED by the
    /// rally-dedicated ptyd daemon. Start/inject/stop/liveness all speak the
    /// daemon's unix-socket JSON-RPC (no tmux keystrokes). Resolved by
    /// `Backend::parse` from `"ptyd"`; `"auto"` prefers it iff the rally-owned
    /// socket is LIVE.
    Ptyd,
}

impl Backend {
    /// Parse a `--backend` value. NOTE: `"auto"` here defaults to `Tmux` — the
    /// live-socket preference for `auto` cannot be decided from the string
    /// alone (it needs an I/O probe), so `command_run` resolves `auto → ptyd`
    /// when the rally-owned socket is live via [`Backend::resolve_auto`]. Every
    /// non-auto value maps deterministically.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" | "tmux" => Ok(Self::Tmux),
            "cmux" => Ok(Self::Cmux),
            "ptyd" => Ok(Self::Ptyd),
            "herdr" => Err(RallyError::Usage(
                "backend \"herdr\" is removed (Plan F): use the .rally ledger \
                 (rally inject) and the rally-termd daemon; or fall back to tmux/cmux"
                    .to_string(),
            )),
            other => Err(RallyError::Usage(format!("unsupported backend {other}"))),
        }
    }

    /// True when the user passed `--backend auto` (recorded so `command_run`
    /// can apply the live-socket preference). bpaf maps both `auto` and `tmux`
    /// to `Tmux`, so the raw string is threaded separately when the distinction
    /// matters.
    pub(crate) fn is_auto(raw: &str) -> bool {
        raw == "auto"
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Cmux => "cmux",
            Self::Ptyd => "ptyd",
        }
    }
}

pub(crate) struct BackendRunner {
    pub(crate) backend: Backend,
    tmux_bin: String,
    cmux_bin: String,
    /// The RALLY-OWNED ptyd socket (F3), resolved once at construction. `None`
    /// only when HOME is unset. Used exclusively by the `Backend::Ptyd` arms;
    /// the tmux/cmux arms never touch it.
    ptyd_socket: Option<String>,
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
            // F3: the rally-owned socket only — NEVER detect_host_runtime's
            // wider candidate list (which includes Easy Terminal's daemon).
            ptyd_socket: crate::daemon_client::rally_owned_socket(),
        }
    }

    /// [E]: pin the ptyd socket this runner uses to the EXACT socket recorded on
    /// a `Backend::Ptyd` session (`ManagedSession::daemon_socket`), so send/
    /// stop/read/liveness reach the SAME daemon the pane was spawned in. A no-op
    /// (`None`/empty) leaves the constructor's `rally_owned_socket()` resolution.
    pub(crate) fn pin_ptyd_socket(&mut self, socket: Option<&str>) {
        if let Some(s) = socket {
            if !s.is_empty() {
                self.ptyd_socket = Some(s.to_string());
            }
        }
    }

    /// The socket this runner will use for ptyd ops, if resolvable. Used by the
    /// spawn path to RECORD the pinned socket on the session ([E]).
    pub(crate) fn ptyd_socket(&self) -> Option<&str> {
        self.ptyd_socket.as_deref()
    }

    /// The resolved rally-owned ptyd socket, or a clear error when unresolved.
    fn require_ptyd_socket(&self) -> Result<&str> {
        self.ptyd_socket.as_deref().ok_or_else(|| {
            RallyError::Command(
                "rally ptyd socket unresolved (HOME unset); cannot reach the rally daemon"
                    .to_string(),
            )
        })
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
            // ptyd spawn is a daemon RPC, not a subprocess command. The plan is
            // surfaced for observability as a single pseudo-command; the actual
            // spawn runs through `command_run`'s ptyd path
            // (`daemon_client::start_agent`), which also does register + F2.
            Backend::Ptyd => vec![ptyd_start_plan(target, cwd, command, name)],
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
            // `command_run` drives the ptyd spawn directly (it needs the pane
            // id for register + F2 rollback), so this generic path is not the
            // ptyd entry point. Reject it loudly rather than silently no-op.
            Backend::Ptyd => Err(RallyError::Command(
                "internal: ptyd sessions must start via command_run's ptyd path, \
                 not BackendRunner::start"
                    .to_string(),
            )),
        }
    }

    /// Spawn a ptyd-owned agent pane via daemon RPC (design-3 start arm). Used
    /// by `command_run`. Returns the daemon pane id (→ `session.target` AND
    /// `session.daemon_pane`).
    pub(crate) fn ptyd_start(
        &self,
        name: &str,
        cwd: &Path,
        command: &[String],
        workspace_id: &str,
    ) -> Result<String> {
        let socket = self.require_ptyd_socket()?;
        match crate::daemon_client::start_agent(socket, name, cwd, command, workspace_id) {
            crate::daemon_client::StartOutcome::Started { pane_id } => Ok(pane_id),
            crate::daemon_client::StartOutcome::Failed { reason } => Err(RallyError::Command(
                format!("ptyd agent.start failed: {reason}"),
            )),
        }
    }

    /// Ensure the rally-dedicated ptyd workspace exists, returning its id so a
    /// spawned pane never lands in the user's focused tab (design-1).
    pub(crate) fn ptyd_ensure_workspace(&self, label: &str) -> Result<String> {
        let socket = self.require_ptyd_socket()?;
        crate::daemon_client::ensure_rally_workspace(socket, label).map_err(RallyError::Command)
    }

    /// Stop (reap) a ptyd-owned pane by daemon name (`agent.stop`).
    pub(crate) fn ptyd_stop(&self, name: &str) -> Result<()> {
        let socket = self.require_ptyd_socket()?;
        crate::daemon_client::stop_agent(socket, name).map_err(RallyError::Command)
    }

    /// [G]: Reap a ptyd-owned pane by its PANE ID (`pane.close`) — used by F2
    /// register-fail rollback, which holds the exact pane id it just spawned. By
    /// id (not name) so a label collision can't reap the wrong pane.
    pub(crate) fn ptyd_close_pane(&self, pane_id: &str) -> Result<()> {
        let socket = self.require_ptyd_socket()?;
        crate::daemon_client::close_pane_by_id(socket, pane_id).map_err(RallyError::Command)
    }

    pub(crate) fn live_target(&self, session: &ManagedSession) -> Result<String> {
        match self.backend {
            Backend::Tmux | Backend::Cmux | Backend::Ptyd => Ok(session.target.clone()),
        }
    }

    pub(crate) fn inject_commands(&self, target: &str, text: &str) -> Vec<Vec<String>> {
        // Sanitize ONCE here, before backend dispatch, so EVERY backend (tmux,
        // cmux, and any future one) receives control-stripped text and no future
        // caller can route around the paste-breakout hardening. Downstream
        // framers/senders must treat their input as already-sanitized.
        let text = sanitize_inject_text(text);
        match self.backend {
            Backend::Tmux => tmux_inject_commands(&self.tmux_bin, target, &text),
            // cmux kept as the separate-submit sequence: its `send` subcommand
            // accepts literal text only (and `send-key <name>` named keys) —
            // there is no raw-byte / hex write equivalent to tmux's
            // `send-keys -H`, so the atomic bracketed-paste frame (ESC[200~ …
            // ESC[201~ + CR) cannot be expressed. `send-key enter` submits as a
            // discrete key, which works for cmux's own TUI; the framed-write
            // fix is tmux-specific (where Codex's bracketed-paste TUI lives).
            // cmux now also receives sanitized text (intended hardening): a
            // control byte in a cmux `send` could otherwise inject keystrokes.
            Backend::Cmux => vec![
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "ctrl+u"],
                cmd![&self.cmux_bin, "send", "--workspace", target, &text],
                cmd![&self.cmux_bin, "send-key", "--workspace", target, "enter"],
            ],
            // Observability-only plan line: the real ptyd inject is the
            // `agent.send` RPC driven by command_inject_managed (with the F4
            // pane cross-check + Receipt fact). No keystrokes are ever sent.
            Backend::Ptyd => vec![cmd![
                "ptyd-rpc",
                "agent.send",
                "--to",
                target,
                "--text",
                &text,
                // submit:true + confirm:"sent" — the real RPC appends the
                // submitting CR and resolves on bytes-written ([A]/[B]).
                "--submit",
                "--confirm",
                "sent"
            ]],
        }
    }

    pub(crate) fn inject(&self, target: &str, text: &str) -> Result<()> {
        run_commands(&self.inject_commands(target, text))
    }

    /// Deliver `text` to a ptyd-owned pane bound to `identity` via `agent.send`
    /// (design-3 inject arm). Applies [`sanitize_inject_text`] (F1) before the
    /// RPC and cross-checks the Receipt's `pane_id` against `expect_pane` (F4):
    /// a mismatch is a HARD failure with `daemon_pane_mismatch` — NO fallback
    /// delivery. Returns the receipt `state` on success.
    pub(crate) fn ptyd_inject(
        &self,
        identity: &str,
        text: &str,
        expect_pane: &str,
    ) -> Result<String> {
        let socket = self.require_ptyd_socket()?;
        // F1: strip control bytes BEFORE the daemon write, same chokepoint
        // semantics as the tmux path.
        let sanitized = sanitize_inject_text(text);
        match crate::daemon_client::send_agent(socket, identity, &sanitized) {
            crate::daemon_client::SendOutcome::Sent(receipt) => {
                // F4: the daemon must have written the pane WE bound. A receipt
                // for a different pane means the identity→pane mapping drifted;
                // refuse to claim delivery and do NOT fall back.
                if receipt.pane_id != expect_pane {
                    return Err(RallyError::Command(format!(
                        "daemon_pane_mismatch: agent.send receipt pane {:?} != session daemon_pane {:?} \
                         (refusing fallback delivery)",
                        receipt.pane_id, expect_pane
                    )));
                }
                Ok(receipt.state)
            }
            crate::daemon_client::SendOutcome::Failed { reason } => Err(RallyError::Command(
                format!("ptyd agent.send failed: {reason}"),
            )),
        }
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
            // A ptyd pane is attached via EasyTerminal / `ptyd attach`, not a
            // tmux client. Surface the real command rather than fake one.
            Backend::Ptyd => vec![cmd!["ptyd", "attach", target]],
        }
    }

    pub(crate) fn attach(&self, target: &str) -> Result<()> {
        if self.backend == Backend::Ptyd {
            // Don't pretend to attach: a ptyd pane lives inside the daemon /
            // EasyTerminal, not a tmux client the CLI can hand the TTY to.
            return Err(RallyError::Usage(format!(
                "attach is unsupported for ptyd sessions; open the pane in EasyTerminal \
                 or run `ptyd attach {target}`"
            )));
        }
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
            // ptyd capture is the `agent.read` RPC against the pane id.
            Backend::Ptyd => vec![cmd![
                "ptyd-rpc",
                "agent.read",
                "--name",
                target,
                "--lines",
                lines
            ]],
        }
    }

    pub(crate) fn capture(&self, target: &str, lines: usize) -> Result<String> {
        if self.backend == Backend::Ptyd {
            // design-3 capture arm: the agent.read scrollback verb.
            let socket = self.require_ptyd_socket()?;
            return crate::daemon_client::read_agent(socket, target, lines)
                .map_err(RallyError::Command);
        }
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
            // ptyd stop is the `agent.stop` RPC (reaps the PTY child daemon-side).
            Backend::Ptyd => vec![cmd!["ptyd-rpc", "agent.stop", "--name", target]],
        }
    }

    pub(crate) fn stop(&self, target: &str) -> Result<()> {
        if self.backend == Backend::Ptyd {
            // design-3 stop arm: agent.stop RPC.
            let socket = self.require_ptyd_socket()?;
            return crate::daemon_client::stop_agent(socket, target).map_err(RallyError::Command);
        }
        run_commands(&self.stop_commands(target))
    }

    pub(crate) fn liveness(&self, targets: &[String]) -> Vec<SessionLiveness> {
        if targets.is_empty() {
            return Vec::new();
        }
        match self.backend {
            Backend::Tmux => probe_tmux_liveness(&self.tmux_bin, targets),
            Backend::Cmux => probe_cmux_liveness(&self.cmux_bin, targets),
            Backend::Ptyd => self.probe_ptyd_liveness(targets),
        }
    }

    /// design-3 liveness arm: probe `pane.list` and map each session target
    /// (a daemon pane id) to Live (listed) / Stale (daemon answered, pane gone)
    /// / Unknown (daemon unreachable — never a false Stale).
    fn probe_ptyd_liveness(&self, targets: &[String]) -> Vec<SessionLiveness> {
        let Some(socket) = self.ptyd_socket.as_deref() else {
            return targets.iter().map(|_| SessionLiveness::Unknown).collect();
        };
        match crate::daemon_client::live_pane_ids(socket) {
            Some(live) => {
                let live: BTreeSet<String> = live.into_iter().collect();
                targets
                    .iter()
                    .map(|t| {
                        if live.contains(t) {
                            SessionLiveness::Live
                        } else {
                            SessionLiveness::Stale
                        }
                    })
                    .collect()
            }
            None => targets.iter().map(|_| SessionLiveness::Unknown).collect(),
        }
    }
}

/// Observability-only pseudo-command describing a ptyd `agent.start` for the
/// run envelope's `commands` plan. The actual spawn is a daemon RPC.
fn ptyd_start_plan(_target: &str, cwd: &Path, command: &[String], name: &str) -> Vec<String> {
    let mut plan = cmd![
        "ptyd-rpc",
        "agent.start",
        "--name",
        name,
        "--cwd",
        cwd.display(),
        "--no-focus",
        "--"
    ];
    plan.extend(command.iter().cloned());
    plan
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
        // Layer 3 parent-lifecycle binding: stamp the launching parent's PID into
        // the new session's environment so the orphan reaper can later ask "is the
        // parent still alive?". `-e KEY=VALUE` sets a session-scoped env var on the
        // created session in the SAME atomic new-session call (no follow-up
        // command, no race). Read back via `tmux show-environment -t <session>`.
        // FAIL-SAFE: if the var is ever absent/unparseable, the reaper falls back
        // to the liveness-window criterion alone and NEVER reaps on the parent
        // criterion (see liveness::reapable, parent_alive=None).
        "-e",
        format!("RALLY_PARENT_PID={}", std::process::id()),
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
/// PURE FRAMER — input MUST be pre-sanitized. The paste-breakout hardening
/// ([`sanitize_inject_text`]) is applied at the single chokepoint
/// [`BackendRunner::inject_commands`], before backend dispatch, so this framer
/// assumes its body carries no control bytes / paste-end marker. Do NOT feed it
/// raw, attacker-controllable text directly — route through `inject_commands`.
///
/// Layout: `ESC[200~ <body> ESC[201~` followed by a single CR placed **after**
/// the closing bracketed-paste marker — never inside the frame, where
/// bracketed-paste semantics would paste the CR as literal text instead of
/// submitting (§4.2). A paste-aware TUI (codex) treats the wrapped body as a
/// paste; the trailing CR then submits the prompt. The separate-Enter sequence
/// this replaces empirically failed against Codex's TUI: the message landed in
/// the input box but never submitted.
fn frame_line_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len() + 1);
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(text.as_bytes());
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

/// A detached, all-stale agent tmux session that the orphan reaper would kill.
#[derive(Clone, Debug, Eq, PartialEq, JsonSchema, Serialize)]
pub(crate) struct OrphanTmux {
    /// tmux session name (matches the `rally-*` convention).
    pub(crate) session_name: String,
    /// Age in seconds since the session's last activity.
    pub(crate) idle_secs: i64,
    /// Layer 3: why this session was staged for reaping — `"stale"` (liveness
    /// window alone; no parent info) or `"stale+parent-dead"` (both criteria).
    /// Never `"parent-dead"` alone: the control NEVER reaps on the parent
    /// criterion without the session also being stale by liveness.
    pub(crate) reason: String,
}

/// Look up the `RALLY_PARENT_PID` env var Layer 3 stamped onto a session at
/// launch, then resolve `parent_alive`:
/// * `Some(true)`  — the PID exists / is alive.
/// * `Some(false)` — the PID is parseable but no such process exists.
/// * `None`        — no var, unparseable var, or the env lookup failed
///   (pre-binding session / non-rally launch) → parent criterion UNAVAILABLE.
///
/// `now`-free + side-effecting (reads tmux + signals the PID), so the orphan
/// classifier takes this as an injected closure for deterministic tests.
pub(crate) fn session_parent_alive(tmux_bin: &str, session_name: &str) -> Option<bool> {
    let out = Command::new(tmux_bin)
        .args(["show-environment", "-t", session_name, "RALLY_PARENT_PID"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    // tmux prints `RALLY_PARENT_PID=<value>`; an unset var prints `-RALLY_PARENT_PID`.
    let value = raw
        .lines()
        .find_map(|l| l.trim().strip_prefix("RALLY_PARENT_PID="))?;
    let pid: i32 = value.trim().parse().ok()?;
    if pid <= 0 {
        return None;
    }
    Some(pid_is_alive(pid))
}

/// POSIX liveness probe via the `kill(1)` builtin with signal 0 — no new crate
/// dependency (this repo keeps a ZERO-extra-dependency contract; see Cargo.toml).
/// `kill -0 <pid>` exits 0 iff the process exists and is signalable; non-zero
/// when it is gone (ESRCH) OR when it exists but we lack permission (EPERM).
///
/// The two non-zero cases are disambiguated by stderr: a "no such process"
/// message means DEAD; a permission error means the process EXISTS (alive,
/// fail-safe). When the `kill` invocation itself fails to spawn, we report
/// `true` (alive) — never reap on an ambiguous/failed probe.
fn pid_is_alive(pid: i32) -> bool {
    let out = Command::new("kill").args(["-0", &pid.to_string()]).output();
    match out {
        // exit 0: process exists + is signalable → alive.
        Ok(o) if o.status.success() => true,
        // Non-zero: DEAD only when stderr says ESRCH ("no such process");
        // any other failure (EPERM "operation not permitted", etc.) means the
        // process EXISTS → alive (fail-safe: never treat live-but-unsignalable
        // as dead).
        Ok(o) => !String::from_utf8_lossy(&o.stderr)
            .to_lowercase()
            .contains("no such process"),
        // Could not even run `kill` → cannot prove death → alive (fail-safe).
        Err(_) => true,
    }
}

/// PURE parse + classify of `tmux list-sessions -F
/// '#{session_name}|#{session_attached}|#{session_activity}'` output.
///
/// A session is an orphan candidate iff ALL hold:
/// * the name starts with `rally-` (the agent-session naming convention),
/// * it is DETACHED (`session_attached == 0` — an attached session is a human
///   actively looking at it; never kill it),
/// * [`liveness::reapable`] returns true for its liveness verdict + parent state.
///
/// Liveness is computed from the single observable orphan signal — tmux
/// `session_activity` age, mapped onto the `code_progress` slot of the 4-signal
/// model (forward terminal activity is the orphan-level proxy for code progress).
/// The other three signals are absent for an UNMANAGED orphan, so `is_live`
/// yields `Live` (fresh) or — once that one signal is stale — `Unknown`. To
/// preserve the established "stale orphan past its window IS reaped" behavior,
/// the orphan path treats a single stale activity signal as `Stale` (provably
/// idle past the window) rather than `Unknown`; this is sound because tmux
/// `session_activity` is always observed for a real session (never absent).
///
/// `parent_alive_fn` is injected (closure over `session_name`) so tests can
/// supply parent state without a live process table; production passes
/// [`session_parent_alive`]. The final keep/reap decision is
/// [`liveness::reapable`] — the single shared authority.
///
/// `now_epoch` is injected for deterministic tests. A line we cannot parse is
/// SKIPPED (fail-safe — never kill on a malformed line).
pub(crate) fn classify_orphan_tmux(
    list_output: &str,
    now_epoch: i64,
    idle_window_secs: i64,
    mut parent_alive_fn: impl FnMut(&str) -> Option<bool>,
) -> Vec<OrphanTmux> {
    use crate::liveness::{Liveness, LivenessSignals, is_live, reapable};
    let mut out = Vec::new();
    for line in list_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let name = parts[0].trim();
        if !name.starts_with("rally-") {
            continue;
        }
        // attached flag: tmux prints "1" when attached, "0" when detached.
        let Ok(attached) = parts[1].trim().parse::<i64>() else {
            continue;
        };
        if attached != 0 {
            continue; // a human is attached — never kill
        }
        let Ok(activity) = parts[2].trim().parse::<i64>() else {
            continue;
        };
        let idle = now_epoch - activity;

        // Map the single observable signal (terminal activity age) onto the
        // 4-signal model. is_live → Live when fresh; for a stale single signal
        // the orphan path promotes Unknown → Stale because a real session ALWAYS
        // has an observed activity timestamp (never genuinely absent), so the
        // idle reading is trustworthy enough to be "provably stale".
        let signals = LivenessSignals {
            code_progress_age: Some(idle),
            ..Default::default()
        };
        let verdict = match is_live(&signals, idle_window_secs) {
            Liveness::Live => Liveness::Live,
            // Single observed signal stale → treat as provably Stale for the
            // reaper (see doc comment); never Unknown for a real session.
            _ => Liveness::Stale,
        };

        let parent_alive = parent_alive_fn(name);
        if !reapable(verdict, parent_alive) {
            continue;
        }

        let reason = match parent_alive {
            Some(false) => "stale+parent-dead",
            // Some(true) cannot reach here (reapable would have returned false);
            // None and any other path is the liveness-window criterion alone.
            _ => "stale",
        }
        .to_string();

        out.push(OrphanTmux {
            session_name: name.to_string(),
            idle_secs: idle,
            reason,
        });
    }
    out
}

/// Detect detached, all-stale `rally-*` orphan tmux sessions. Shells out to tmux
/// once; returns `[]` when tmux is absent / no server / the call fails (fail-open
/// — never invent orphans). `idle_window_secs` is the adaptive default-cadence
/// window. The actual kill is performed by the caller via `BackendRunner::stop`.
pub(crate) fn detect_orphan_tmux(
    tmux_bin: &str,
    now_epoch: i64,
    idle_window_secs: i64,
) -> Vec<OrphanTmux> {
    let output = Command::new(tmux_bin)
        .args([
            "list-sessions",
            "-F",
            "#{session_name}|#{session_attached}|#{session_activity}",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => classify_orphan_tmux(
            &String::from_utf8_lossy(&o.stdout),
            now_epoch,
            idle_window_secs,
            // Layer 3: resolve each candidate's launching-parent liveness from
            // the RALLY_PARENT_PID env var stamped at launch. Absent/unparseable
            // → None → reaper falls back to the liveness-window criterion alone.
            |name| session_parent_alive(tmux_bin, name),
        ),
        // no server / tmux missing / error → no orphans to report (fail-open).
        _ => Vec::new(),
    }
}

/// Kill a tmux session by name (orphan reap / self-kill). Best-effort: returns
/// whether the kill command reported success. Never panics.
pub(crate) fn kill_tmux_session(tmux_bin: &str, session_name: &str) -> bool {
    Command::new(tmux_bin)
        .args(["kill-session", "-t", session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The tmux session name that THIS process is running inside, if any
/// (`$TMUX_PANE` → `tmux display-message`), restricted to `rally-*`. Used by
/// `rally stop` to self-kill its own agent tmux session at session end. Returns
/// `None` when not inside tmux or not a `rally-*` session.
pub(crate) fn own_rally_tmux_session(tmux_bin: &str) -> Option<String> {
    if std::env::var_os("TMUX").is_none() {
        return None;
    }
    let out = Command::new(tmux_bin)
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.starts_with("rally-") {
        Some(name)
    } else {
        None
    }
}

/// Read an integer session-scoped tmux env var (`tmux show-environment -t
/// <session> <key>`). Returns `None` when not inside a value, unset, or
/// unparseable. Used by the Layer 1 self-exit re-check to persist the
/// consecutive-empty streak in the session's OWN env (dies with the session).
pub(crate) fn get_session_env_i64(tmux_bin: &str, session: &str, key: &str) -> Option<i64> {
    let out = Command::new(tmux_bin)
        .args(["show-environment", "-t", session, key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let prefix = format!("{key}=");
    raw.lines()
        .find_map(|l| l.trim().strip_prefix(&prefix).map(str::to_string))
        .and_then(|v| v.trim().parse::<i64>().ok())
}

/// Set a session-scoped tmux env var. Best-effort; returns success.
pub(crate) fn set_session_env_i64(
    tmux_bin: &str,
    session: &str,
    key: &str,
    value: i64,
) -> bool {
    Command::new(tmux_bin)
        .args([
            "set-environment",
            "-t",
            session,
            key,
            &value.to_string(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        CR, PASTE_END, PASTE_START, classify_orphan_tmux, frame_line_bytes, hex_tokens,
        parse_cmux_start_target, pid_is_alive, sanitize_inject_text, shell_words,
        tmux_inject_commands,
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
        // Sanitization now happens at the inject_commands chokepoint (not inside
        // frame_line_bytes), so this exercises the real pipeline order
        // sanitize -> frame, which is exactly what inject_commands does.
        let attack = "ok\x1b[201~rm -rf /\r";
        let got = frame_line_bytes(&sanitize_inject_text(attack));
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

    // ---- orphan-tmux classifier (R4) ----
    // Default-cadence adaptive window: 300*6+60 = 1860s (31m). Use now=1_000_000.
    const NOW: i64 = 1_000_000;
    const WIN: i64 = 1860;

    fn line(name: &str, attached: i64, activity: i64) -> String {
        format!("{name}|{attached}|{activity}")
    }

    /// Default parent closure for the pre-Layer-3 behavior tests: no parent info
    /// recorded → `None` → reaper falls back to the liveness-window criterion
    /// alone (proves the fail-safe degradation preserves prior behavior).
    fn no_parent(_: &str) -> Option<bool> {
        None
    }

    #[test]
    fn detached_stale_rally_session_is_orphan() {
        // detached, last activity 40 min ago (2400s > 1860).
        let out = line("rally-claude-foo", 0, NOW - 2400);
        let orphans = classify_orphan_tmux(&out, NOW, WIN, no_parent);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].session_name, "rally-claude-foo");
        assert_eq!(orphans[0].idle_secs, 2400);
        // No parent info → window criterion alone → reason "stale".
        assert_eq!(orphans[0].reason, "stale");
    }

    #[test]
    fn attached_session_is_never_orphan() {
        // attached=1: a human is looking at it — never kill, even if stale.
        let out = line("rally-codex-bar", 1, NOW - 99999);
        assert!(classify_orphan_tmux(&out, NOW, WIN, no_parent).is_empty());
    }

    #[test]
    fn fresh_detached_rally_session_is_not_orphan() {
        // detached but active 5 min ago (300s < 1860).
        let out = line("rally-claude-baz", 0, NOW - 300);
        assert!(classify_orphan_tmux(&out, NOW, WIN, no_parent).is_empty());
    }

    #[test]
    fn non_rally_session_is_ignored() {
        // a user's own "work" tmux session, detached and ancient, is NOT touched.
        let out = line("work", 0, NOW - 99999);
        assert!(classify_orphan_tmux(&out, NOW, WIN, no_parent).is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let out = "garbage\nrally-x|notanumber|123\nrally-y|0|alsobad\n\n";
        assert!(classify_orphan_tmux(out, NOW, WIN, no_parent).is_empty());
    }

    #[test]
    fn mixed_list_picks_only_stale_detached_rally() {
        let out = [
            line("rally-claude-1", 0, NOW - 5000), // orphan
            line("rally-codex-2", 1, NOW - 5000),  // attached — keep
            line("rally-claude-3", 0, NOW - 60),   // fresh — keep
            line("work", 0, NOW - 99999),          // non-rally — keep
        ]
        .join("\n");
        let orphans = classify_orphan_tmux(&out, NOW, WIN, no_parent);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].session_name, "rally-claude-1");
    }

    // ---- Layer 3: parent-lifecycle binding (acceptance scenario 3) ----

    #[test]
    fn stale_session_with_dead_parent_is_reaped_with_reason() {
        // Stale by window AND parent provably dead → reaped, reason names both.
        let out = line("rally-claude-orphan", 0, NOW - 5000);
        let orphans = classify_orphan_tmux(&out, NOW, WIN, |_| Some(false));
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].reason, "stale+parent-dead");
    }

    #[test]
    fn stale_session_with_live_parent_is_kept() {
        // Stale by window BUT parent still alive → conservative keep (a live
        // parent may re-drive the child; we never reap under a live parent).
        let out = line("rally-claude-childofparent", 0, NOW - 5000);
        assert!(
            classify_orphan_tmux(&out, NOW, WIN, |_| Some(true)).is_empty(),
            "stale session under a LIVE parent must be kept"
        );
    }

    #[test]
    fn code_progressing_session_with_dead_parent_is_kept() {
        // FAIL-SAFE: parent is dead, but the session is making forward progress
        // (fresh activity within the window → Live). Independently-live sessions
        // are NEVER reaped on the parent criterion alone.
        let out = line("rally-claude-busy", 0, NOW - 60);
        assert!(
            classify_orphan_tmux(&out, NOW, WIN, |_| Some(false)).is_empty(),
            "a code-progressing (live) session must survive even with a dead parent"
        );
    }

    #[test]
    fn missing_parent_info_falls_back_to_window_not_reaped_when_fresh() {
        // No parent info AND fresh → kept (window criterion: not stale).
        let fresh = line("rally-claude-freshnoparent", 0, NOW - 60);
        assert!(classify_orphan_tmux(&fresh, NOW, WIN, no_parent).is_empty());
        // No parent info AND stale → reaped on the window criterion ALONE
        // (never reaped on the parent criterion, which is unavailable).
        let stale = line("rally-claude-stalenoparent", 0, NOW - 5000);
        let orphans = classify_orphan_tmux(&stale, NOW, WIN, no_parent);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].reason, "stale");
    }

    #[test]
    fn pid_is_alive_reports_self_alive_and_unused_pid_dead() {
        // Our own PID is alive.
        let me = std::process::id() as i32;
        assert!(pid_is_alive(me), "current process must read alive");
        // PID 999999999 is (almost certainly) not a live process → dead.
        assert!(
            !pid_is_alive(999_999_999),
            "an unused high PID must read dead (ESRCH)"
        );
    }
}
