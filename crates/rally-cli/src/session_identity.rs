// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Session identity registry — layered runtime identity distinct from tool id.
//!
//! ## Why
//! `tool_type` ("claude_code", "codex") cannot prove *which* terminal, pane,
//! process, or cloud worker acted. The protocol north star
//! ([`docs/PROTOCOL-NORTH-STAR.md`](../../../docs/PROTOCOL-NORTH-STAR.md))
//! requires layered identity so thousands of agents can be targeted and
//! verified:
//!
//! ```text
//! endpoint_id  — stable-ish addressable place (pane, tty, process, cloud job)
//! session_id   — a fresh live lease occupying an endpoint
//! ```
//!
//! When an endpoint restarts, is re-used, or changes actor, a **new
//! `session_id`** is issued while the **`endpoint_id` lineage is preserved**.
//! This is what answers "which Claude received it?": a Rally ACK from a specific
//! `from_session_id` is proof; transport to a pane is not.
//!
//! ## Charter alignment
//! This module is **pure derivation** — it reads no global state, writes no
//! ledger, spawns nothing. The only impurity is the thin
//! [`EndpointInputs::from_env`] boundary constructor, which reads env + process
//! metadata once at a call site; the derivation functions it feeds are pure and
//! unit-tested without env. Rally facilitates; the host executes.
//!
//! ## Compatibility
//! Older events carry only `tool`. [`ProtocolSessionIdentity::from_legacy_tool`]
//! synthesizes a stable identity for them so replay never breaks. The
//! synthesized endpoint/session ids are namespaced `legacy:` and report
//! [`ProtocolSessionIdentity::is_legacy`] so callers can distinguish a real
//! live lease from a back-filled one.
//!
//! ## Staged delivery
//! `#![allow(dead_code)]`: this is the Phase-1 module of a staged build. Its
//! public surface is consumed by `whoami`/`say` in the later `integration-wiring`
//! task; until then the items are unused by the crate proper (they are exercised
//! by this module's own tests). The allow is removed when integration lands.
#![allow(dead_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Identity-string charset: lowercase alphanumerics plus the structural
/// separators this module emits (`:` segment, `#` lease, `.` `-` `_` literal).
/// Everything else collapses to `-` so ids stay log/filesystem/url safe.
fn sanitize_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_');
        if keep {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            // Collapse runs of separators (incl. `:` `#` `/` whitespace) to one `-`.
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Raw runtime signals observed at a call site. Every field is optional: the
/// derivation picks the highest-fidelity signal present and records whether the
/// remainder leaves the endpoint ambiguous. Construct via [`Self::from_env`] at
/// a boundary, or literally in tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointInputs {
    /// Hostname (`uname -n`). Weak on its own — two terminals share a host.
    pub host: Option<String>,
    /// OS process id of the host agent.
    pub pid: Option<u32>,
    /// Process start token (epoch secs or boot-relative); disambiguates pid reuse.
    pub process_start: Option<String>,
    /// Controlling tty path, e.g. `/dev/ttys003`.
    pub tty: Option<String>,
    /// `TERM_SESSION_ID` (Terminal.app / iTerm) — stable per visible terminal.
    pub term_session_id: Option<String>,
    /// tmux server socket path (`tmux display -p '#{socket_path}'`).
    pub tmux_socket: Option<String>,
    /// tmux session name.
    pub tmux_session: Option<String>,
    /// tmux window index.
    pub tmux_window: Option<String>,
    /// tmux pane id (`%N`) — the stable addressable place inside tmux.
    pub tmux_pane: Option<String>,
    /// Rally-managed session id (a `rally run` backend session).
    pub managed_session_id: Option<String>,
    /// Cloud provider (e.g. `github-actions`, `fly`, `modal`).
    pub cloud_provider: Option<String>,
    /// Cloud job/run id.
    pub cloud_job_id: Option<String>,
}

/// Result of resolving raw signals into a stable endpoint id.
///
/// `ambiguous` is true when the signals are too weak to distinguish this
/// runtime from a sibling on the same host (e.g. host-only, no pid/tty/pane).
/// Agents must not silently guess in that case — surface it, exactly as
/// `whoami` does for multiple ptyd sockets.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub(crate) struct EndpointResolution {
    pub endpoint_id: String,
    /// Which signal class won the precedence contest (for human trust + debugging).
    pub source: EndpointSource,
    pub ambiguous: bool,
}

/// The signal class that determined an [`EndpointResolution`], highest-fidelity first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndpointSource {
    Cloud,
    Managed,
    Tmux,
    Terminal,
    Process,
    /// Host-only or empty — cannot distinguish sibling runtimes.
    HostOnly,
    /// Back-filled for a legacy event that carried only `tool`.
    Legacy,
}

/// Derive a stable, structured `endpoint_id` from runtime signals.
///
/// Precedence (a higher-fidelity signal always wins so the same physical place
/// maps to the same id regardless of which weaker signals are also present):
/// cloud job → managed pane → tmux pane → terminal session/tty → local process.
/// Falls back to `host:` (ambiguous) and finally `unknown` (ambiguous).
///
/// Pure: no env, no clock, no filesystem.
pub(crate) fn derive_endpoint(inputs: &EndpointInputs) -> EndpointResolution {
    let seg = |s: &Option<String>| s.as_deref().map(sanitize_segment).filter(|s| !s.is_empty());

    if let (Some(provider), Some(job)) = (seg(&inputs.cloud_provider), seg(&inputs.cloud_job_id)) {
        return EndpointResolution {
            endpoint_id: format!("cloud:{provider}:{job}"),
            source: EndpointSource::Cloud,
            ambiguous: false,
        };
    }
    if let Some(managed) = seg(&inputs.managed_session_id) {
        return EndpointResolution {
            endpoint_id: format!("managed:{managed}"),
            source: EndpointSource::Managed,
            ambiguous: false,
        };
    }
    if let Some(pane) = seg(&inputs.tmux_pane) {
        // socket+session+window+pane is the fully-qualified place; pane alone is
        // already unique within a server but we keep the lineage when present.
        let server = seg(&inputs.tmux_socket);
        let session = seg(&inputs.tmux_session);
        let mut id = String::from("tmux:");
        for part in [server, session, seg(&inputs.tmux_window)]
            .into_iter()
            .flatten()
        {
            id.push_str(&part);
            id.push(':');
        }
        id.push_str(&pane);
        return EndpointResolution {
            endpoint_id: id,
            source: EndpointSource::Tmux,
            ambiguous: false,
        };
    }
    if let Some(term) = seg(&inputs.term_session_id).or_else(|| seg(&inputs.tty)) {
        let host = seg(&inputs.host);
        let endpoint_id = match host {
            Some(h) => format!("term:{h}:{term}"),
            None => format!("term:{term}"),
        };
        return EndpointResolution {
            endpoint_id,
            source: EndpointSource::Terminal,
            ambiguous: false,
        };
    }
    if let Some(pid) = inputs.pid {
        let host = seg(&inputs.host).unwrap_or_else(|| "localhost".to_string());
        let start = seg(&inputs.process_start);
        let endpoint_id = match start {
            Some(s) => format!("proc:{host}:{pid}:{s}"),
            // pid without a start token is reuse-ambiguous across the host's
            // lifetime, but still distinguishes concurrently-live processes.
            None => format!("proc:{host}:{pid}"),
        };
        // Concurrently-live pids are distinct; pid reuse across the host's
        // lifetime is the only residual aliasing and is out of scope here.
        return EndpointResolution {
            endpoint_id,
            source: EndpointSource::Process,
            ambiguous: false,
        };
    }
    if let Some(host) = seg(&inputs.host) {
        return EndpointResolution {
            endpoint_id: format!("host:{host}"),
            source: EndpointSource::HostOnly,
            ambiguous: true,
        };
    }
    EndpointResolution {
        endpoint_id: "unknown".to_string(),
        source: EndpointSource::HostOnly,
        ambiguous: true,
    }
}

/// A layered, live session identity. `session_id` is the lease handle that
/// every durable write should carry as `from_session_id`.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub(crate) struct ProtocolSessionIdentity {
    /// Fresh live lease on the endpoint. Use as `from_session_id` on writes.
    pub session_id: String,
    /// Stable-ish addressable place. Survives across session leases.
    pub endpoint_id: String,
    /// Host family: `claude_code`, `codex`, `cursor`, `ci`, …
    pub tool_type: String,
    /// Logical persona/subagent inside the session (optional precision).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Human/service/agent principal behind the session (privileged actions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// Human-legible name for operator trust ("Claude audit in tmux %2").
    pub legible_name: String,
    /// True when synthesized for a legacy `tool`-only event, not a live lease.
    pub legacy: bool,
    /// Whether the endpoint could not be distinguished from siblings.
    pub ambiguous: bool,
}

impl ProtocolSessionIdentity {
    /// Mint a live identity from a resolved endpoint plus a **lease token**.
    ///
    /// The lease token (process start, a uuid, or a monotonic nonce) is what
    /// makes a restart on the *same* endpoint a *new* session while preserving
    /// `endpoint_id` lineage. Callers must pass a token that changes per live
    /// runtime; this function never invents one (keeps it pure/testable).
    pub(crate) fn mint(
        endpoint: &EndpointResolution,
        tool_type: &str,
        lease_token: &str,
        actor_id: Option<&str>,
        principal_id: Option<&str>,
    ) -> Self {
        let tool_type = sanitize_segment(tool_type);
        let lease = sanitize_segment(lease_token);
        let session_id = format!("sess:{}#{}", endpoint.endpoint_id, lease);
        let legible_name = legible_name(&tool_type, actor_id, endpoint);
        Self {
            session_id,
            endpoint_id: endpoint.endpoint_id.clone(),
            tool_type,
            actor_id: actor_id.map(str::to_string).filter(|s| !s.is_empty()),
            principal_id: principal_id.map(str::to_string).filter(|s| !s.is_empty()),
            legible_name,
            legacy: false,
            ambiguous: endpoint.ambiguous,
        }
    }

    /// Synthesize a stable identity for a legacy event carrying only `tool`
    /// (e.g. `"codex:01"` or `"claude_code"`). Namespaced `legacy:` so it never
    /// collides with a live lease, and flagged [`is_legacy`](Self::is_legacy).
    pub(crate) fn from_legacy_tool(tool: &str) -> Self {
        let raw = tool.trim();
        let (tool_type, actor) = match raw.split_once(':') {
            Some((t, a)) if !a.is_empty() => (t, Some(a)),
            _ => (raw, None),
        };
        let tool_type = sanitize_segment(tool_type);
        let actor_seg = actor.map(sanitize_segment).filter(|s| !s.is_empty());
        let endpoint_id = match &actor_seg {
            Some(a) => format!("legacy:{tool_type}:{a}"),
            None => format!("legacy:{tool_type}"),
        };
        let legible_name = format!("{raw} (legacy, pre-session-identity)");
        Self {
            session_id: format!("sess:{endpoint_id}"),
            endpoint_id,
            tool_type,
            actor_id: actor_seg,
            principal_id: None,
            legible_name,
            legacy: true,
            ambiguous: false,
        }
    }

    /// True when this identity was back-filled from a legacy `tool`-only event.
    pub(crate) fn is_legacy(&self) -> bool {
        self.legacy
    }

    /// `from_session_id` value to stamp on a durable write originating here.
    /// Named for the protocol field it yields (not a constructor); the &self
    /// getter convention is intentional.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn from_session_id(&self) -> &str {
        &self.session_id
    }

    /// Two identities are the **same live runtime** iff their session leases match.
    /// Same endpoint + different lease = same place, different session (a restart).
    pub(crate) fn same_runtime(&self, other: &Self) -> bool {
        self.session_id == other.session_id
    }

    /// Two identities share an **endpoint lineage** (same physical place across
    /// restarts) even when they are distinct sessions.
    pub(crate) fn same_endpoint(&self, other: &Self) -> bool {
        self.endpoint_id == other.endpoint_id
    }

    /// Reconstruct a session identity from a STORED `from_session_id` string —
    /// the authority key a durable write carried. This is the read-side inverse
    /// of [`from_session_id`](Self::from_session_id): the live `mint` path is
    /// gone by the time we replay a fact, so we recover the addressable place
    /// (`endpoint_id`) from the stored key and pair it with the still-present
    /// display `tool` and any recorded `principal_id`.
    ///
    /// `session_key` is a `sess:<endpoint_id>#<lease>` (live) or
    /// `sess:<endpoint_id>` (legacy mint) value. The endpoint is the segment
    /// between the `sess:` prefix and the first `#`; the lease (after `#`) is
    /// not needed to address the place. A `session_key` missing the `sess:`
    /// prefix is tolerated (taken verbatim as the endpoint) so a hand-written /
    /// imported key never panics — the field is authority data, not a contract.
    ///
    /// Pure: no env, no clock. The `legacy` flag is false here (this came from a
    /// real recorded session key); a fact with NO `from_session_id` routes
    /// through [`from_legacy_tool`](Self::from_legacy_tool) instead.
    pub(crate) fn from_session_key(
        session_key: &str,
        tool: Option<&str>,
        principal_id: Option<&str>,
    ) -> Self {
        let body = session_key.strip_prefix("sess:").unwrap_or(session_key);
        let endpoint_id = body.split('#').next().unwrap_or(body).to_string();
        let raw_tool = tool.map(str::trim).filter(|s| !s.is_empty());
        let (tool_type, actor) = match raw_tool {
            Some(t) => match t.split_once(':') {
                Some((tt, a)) if !a.is_empty() => (tt.to_string(), Some(a.to_string())),
                _ => (t.to_string(), None),
            },
            None => ("unknown".to_string(), None),
        };
        let tool_type = sanitize_segment(&tool_type);
        let actor_id = actor
            .as_deref()
            .map(sanitize_segment)
            .filter(|s| !s.is_empty());
        let endpoint = EndpointResolution {
            endpoint_id: endpoint_id.clone(),
            source: EndpointSource::Process,
            ambiguous: false,
        };
        let legible_name = legible_name(&tool_type, actor_id.as_deref(), &endpoint);
        Self {
            session_id: session_key.to_string(),
            endpoint_id,
            tool_type,
            actor_id,
            principal_id: principal_id
                .map(str::to_string)
                .filter(|s| !s.is_empty()),
            legible_name,
            legacy: false,
            ambiguous: false,
        }
    }
}

/// Build the operator-facing legible name. The stable machine id handles
/// routing; this handles human trust.
fn legible_name(tool_type: &str, actor_id: Option<&str>, endpoint: &EndpointResolution) -> String {
    let display_tool = match tool_type {
        "claude_code" => "Claude",
        "codex" => "Codex",
        other => other,
    };
    let place = match endpoint.source {
        EndpointSource::Tmux | EndpointSource::Terminal => format!("in {}", endpoint.endpoint_id),
        EndpointSource::Managed => format!("in managed {}", endpoint.endpoint_id),
        EndpointSource::Cloud => format!("on {}", endpoint.endpoint_id),
        EndpointSource::Process => format!("as {}", endpoint.endpoint_id),
        EndpointSource::HostOnly => format!("on {} (ambiguous)", endpoint.endpoint_id),
        EndpointSource::Legacy => "(legacy)".to_string(),
    };
    match actor_id.map(str::trim).filter(|a| !a.is_empty()) {
        Some(actor) => format!("{display_tool} {actor} {place}"),
        None => format!("{display_tool} {place}"),
    }
}

impl EndpointInputs {
    /// Boundary constructor: read env + process metadata once. This is the only
    /// impure surface; the derivation it feeds is pure. Kept out of unit tests.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn from_env() -> Self {
        use std::env;
        let nonempty = |v: Result<String, env::VarError>| v.ok().filter(|s| !s.is_empty());
        Self {
            host: nonempty(env::var("HOSTNAME")).or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            }),
            pid: Some(std::process::id()),
            process_start: None,
            tty: nonempty(env::var("TTY")),
            term_session_id: nonempty(env::var("TERM_SESSION_ID")),
            tmux_socket: None,
            tmux_session: None,
            tmux_window: None,
            tmux_pane: nonempty(env::var("TMUX_PANE")),
            managed_session_id: nonempty(env::var("RALLY_SESSION_ID")),
            cloud_provider: if env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                Some("github-actions".to_string())
            } else {
                None
            },
            cloud_job_id: nonempty(env::var("GITHUB_RUN_ID")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_inputs(host: &str, pid: u32, start: &str) -> EndpointInputs {
        EndpointInputs {
            host: Some(host.to_string()),
            pid: Some(pid),
            process_start: Some(start.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn two_claude_sessions_are_distinguishable() {
        // Two interactive Claudes on the same host, different terminals.
        let a = EndpointInputs {
            term_session_id: Some("AAAA-1111".into()),
            host: Some("ws.local".into()),
            ..Default::default()
        };
        let b = EndpointInputs {
            term_session_id: Some("BBBB-2222".into()),
            host: Some("ws.local".into()),
            ..Default::default()
        };
        let ida =
            ProtocolSessionIdentity::mint(&derive_endpoint(&a), "claude_code", "L1", None, None);
        let idb =
            ProtocolSessionIdentity::mint(&derive_endpoint(&b), "claude_code", "L1", None, None);
        assert_ne!(ida.session_id, idb.session_id, "two Claudes must differ");
        assert_ne!(ida.endpoint_id, idb.endpoint_id);
        assert_eq!(ida.tool_type, "claude_code");
        assert!(!ida.same_runtime(&idb));
        assert_ne!(ida.legible_name, idb.legible_name);
    }

    #[test]
    fn claude_and_codex_on_same_pane_differ_by_tool() {
        let ep = derive_endpoint(&EndpointInputs {
            tmux_pane: Some("%7".into()),
            tmux_session: Some("work".into()),
            ..Default::default()
        });
        let claude = ProtocolSessionIdentity::mint(&ep, "claude_code", "L1", None, None);
        let codex = ProtocolSessionIdentity::mint(&ep, "codex", "L2", None, None);
        assert_ne!(claude.session_id, codex.session_id);
        assert!(
            claude.same_endpoint(&codex),
            "same pane = same endpoint lineage"
        );
        assert!(!claude.same_runtime(&codex));
    }

    #[test]
    fn pane_restart_keeps_endpoint_changes_session() {
        let inputs = EndpointInputs {
            tmux_socket: Some("/tmp/tmux-501/default".into()),
            tmux_session: Some("main".into()),
            tmux_window: Some("0".into()),
            tmux_pane: Some("%2".into()),
            ..Default::default()
        };
        let ep = derive_endpoint(&inputs);
        let before =
            ProtocolSessionIdentity::mint(&ep, "claude_code", "lease-1700000000", None, None);
        // Same pane, the agent process restarts → new lease token.
        let after =
            ProtocolSessionIdentity::mint(&ep, "claude_code", "lease-1700009999", None, None);
        assert_eq!(
            before.endpoint_id, after.endpoint_id,
            "endpoint lineage stable"
        );
        assert_ne!(
            before.session_id, after.session_id,
            "restart = new session lease"
        );
        assert!(before.same_endpoint(&after));
        assert!(!before.same_runtime(&after));
    }

    #[test]
    fn endpoint_precedence_orders_high_fidelity_first() {
        // Every signal present: cloud wins.
        let all = EndpointInputs {
            host: Some("h".into()),
            pid: Some(9),
            process_start: Some("s".into()),
            tty: Some("/dev/ttys003".into()),
            term_session_id: Some("T".into()),
            tmux_pane: Some("%1".into()),
            managed_session_id: Some("m".into()),
            cloud_provider: Some("fly".into()),
            cloud_job_id: Some("job9".into()),
            ..Default::default()
        };
        assert_eq!(derive_endpoint(&all).source, EndpointSource::Cloud);
        // Drop cloud → managed wins, etc.
        let no_cloud = EndpointInputs {
            cloud_provider: None,
            cloud_job_id: None,
            ..all.clone()
        };
        assert_eq!(derive_endpoint(&no_cloud).source, EndpointSource::Managed);
        let no_managed = EndpointInputs {
            managed_session_id: None,
            ..no_cloud.clone()
        };
        assert_eq!(derive_endpoint(&no_managed).source, EndpointSource::Tmux);
        let no_tmux = EndpointInputs {
            tmux_pane: None,
            ..no_managed.clone()
        };
        assert_eq!(derive_endpoint(&no_tmux).source, EndpointSource::Terminal);
        let no_term = EndpointInputs {
            tty: None,
            term_session_id: None,
            ..no_tmux.clone()
        };
        assert_eq!(derive_endpoint(&no_term).source, EndpointSource::Process);
    }

    #[test]
    fn host_only_is_ambiguous() {
        let r = derive_endpoint(&EndpointInputs {
            host: Some("shared.local".into()),
            ..Default::default()
        });
        assert_eq!(r.source, EndpointSource::HostOnly);
        assert!(
            r.ambiguous,
            "host alone cannot distinguish sibling terminals"
        );
        let id = ProtocolSessionIdentity::mint(&r, "claude_code", "L1", None, None);
        assert!(id.ambiguous);
        assert!(id.legible_name.contains("ambiguous"));
    }

    #[test]
    fn empty_inputs_resolve_to_ambiguous_unknown() {
        let r = derive_endpoint(&EndpointInputs::default());
        assert_eq!(r.endpoint_id, "unknown");
        assert!(r.ambiguous);
    }

    #[test]
    fn from_session_id_is_the_lease() {
        let id = ProtocolSessionIdentity::mint(
            &derive_endpoint(&proc_inputs("h", 42, "1700")),
            "codex",
            "lease",
            Some("worker"),
            Some("svc-ci"),
        );
        assert_eq!(id.from_session_id(), id.session_id);
        assert!(id.session_id.starts_with("sess:proc:h:42:1700#"));
        assert_eq!(id.actor_id.as_deref(), Some("worker"));
        assert_eq!(id.principal_id.as_deref(), Some("svc-ci"));
    }

    #[test]
    fn legacy_tool_back_fill_is_stable_and_flagged() {
        let a = ProtocolSessionIdentity::from_legacy_tool("codex:01");
        let b = ProtocolSessionIdentity::from_legacy_tool("codex:01");
        assert_eq!(a, b, "legacy synthesis is deterministic");
        assert!(a.is_legacy());
        assert_eq!(a.tool_type, "codex");
        assert_eq!(a.actor_id.as_deref(), Some("01"));
        assert!(a.endpoint_id.starts_with("legacy:"));
        // A bare tool with no actor.
        let bare = ProtocolSessionIdentity::from_legacy_tool("claude_code");
        assert!(bare.is_legacy());
        assert_eq!(bare.tool_type, "claude_code");
        assert_eq!(bare.actor_id, None);
        // Legacy never collides with a live lease for the same logical tool.
        let live = ProtocolSessionIdentity::mint(
            &derive_endpoint(&proc_inputs("h", 1, "t")),
            "codex",
            "L1",
            Some("01"),
            None,
        );
        assert_ne!(a.session_id, live.session_id);
        assert_ne!(a.endpoint_id, live.endpoint_id);
    }

    #[test]
    fn from_session_key_recovers_endpoint_and_carries_display_and_principal() {
        // A live mint round-trips: mint → from_session_id() → from_session_key
        // recovers the SAME endpoint lineage (same_endpoint), pairing in the
        // display tool + principal that the stored fact still carries.
        let minted = ProtocolSessionIdentity::mint(
            &derive_endpoint(&proc_inputs("ws.local", 42, "1700")),
            "claude_code",
            "live",
            Some("auditor"),
            None,
        );
        let key = minted.from_session_id().to_string();
        let recovered =
            ProtocolSessionIdentity::from_session_key(&key, Some("claude_code:auditor"), Some("tyrone"));
        assert_eq!(recovered.session_id, key, "session key is preserved verbatim");
        assert!(
            recovered.same_endpoint(&minted),
            "endpoint lineage recovered from the stored session key"
        );
        assert_eq!(recovered.tool_type, "claude_code");
        assert_eq!(recovered.actor_id.as_deref(), Some("auditor"));
        assert_eq!(recovered.principal_id.as_deref(), Some("tyrone"));
        assert!(!recovered.legacy, "a real session key is not legacy");
    }

    #[test]
    fn from_session_key_tolerates_missing_prefix_and_no_lease() {
        // No `sess:` prefix and no `#lease` — the whole thing is the endpoint.
        let r = ProtocolSessionIdentity::from_session_key("term:host:abc", None, None);
        assert_eq!(r.endpoint_id, "term:host:abc");
        assert_eq!(r.tool_type, "unknown");
        assert_eq!(r.principal_id, None);
        // A `sess:` key without a lease still strips to the endpoint.
        let r2 = ProtocolSessionIdentity::from_session_key("sess:legacy:codex", None, None);
        assert_eq!(r2.endpoint_id, "legacy:codex");
    }

    #[test]
    fn sanitize_collapses_unsafe_runs() {
        assert_eq!(sanitize_segment("Foo Bar//Baz"), "foo-bar-baz");
        assert_eq!(sanitize_segment("  a:b#c  "), "a-b-c");
        assert_eq!(
            sanitize_segment("keep.dash-under_score"),
            "keep.dash-under_score"
        );
        assert_eq!(sanitize_segment("///"), "");
    }

    #[test]
    fn ids_are_charset_safe() {
        let id = ProtocolSessionIdentity::mint(
            &derive_endpoint(&EndpointInputs {
                term_session_id: Some("w3C session/ID!".into()),
                ..Default::default()
            }),
            "claude code",
            "lease token!",
            None,
            None,
        );
        // session_id is composed only of our structural separators + safe chars.
        assert!(
            id.session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '#' | '.' | '-' | '_'))
        );
    }
}
