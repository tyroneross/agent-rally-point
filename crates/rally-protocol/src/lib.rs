// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # rally-protocol
//!
//! The **entire coupling surface** between rally (writer) and the daemon
//! (consumer) in Plan F's hybrid ledger-driven coordination architecture.
//!
//! Plan F inverts the herdr-era direction: rally no longer calls into the
//! daemon. Instead, rally appends typed [`Directive`] records to an
//! append-only per-agent inbox file in the `.rally` ledger; the daemon
//! ([`rally-termd`](https://github.com/tyroneross/ptyd)) subscribes via
//! kernel file-events, executes terminal-touching directives by logical
//! agent-id, and posts [`Receipt`]s back to the same ledger.
//!
//! This crate owns the types + the [`Inbox`] trait + the canonical
//! file-backed implementation [`ledger::FileInbox`]. Everything else is
//! private to each binary.
//!
//! ## Why the legacy surface stays small
//! The active inbox types remain deliberately ~2 structs + 3 enums + 1 trait.
//! The additive [`delivery`] module freezes the future cross-adapter contract
//! without changing the current FileInbox wire or breaking sibling Rust
//! consumers. The shared crate IS the contract; round-trip and frozen-reader
//! tests enforce both surfaces.
//!
//! ## Wire format
//! Directives + Receipts are JSON, one record per line, appended via
//! `O_APPEND` to a per-agent inbox file in `<ledger-root>/inbox/<agent>.jsonl`.
//! The format is line-delimited JSON (NDJSON) so partial-line crashes never
//! corrupt the file — a half-written line is detected by readers and skipped
//! (consistent with the existing `.rally/changes.jsonl` substrate).
//!
//! Receipts go to `<ledger-root>/receipts/<agent>.jsonl` (separate file so a
//! reader scanning only Directives doesn't have to filter).
//!
//! ## Wire field naming
//! `Directive::itype` serialises as `"type"` to match the InterruptBench
//! research vocabulary (Addition | Revision | Retraction). The Rust field
//! is `itype` because `type` is a reserved keyword.
//!
//! ## Forward-compat
//! `Directive` and `Receipt` are DELIBERATELY not annotated with
//! `#[serde(deny_unknown_fields)]`. Future fields added by a newer writer
//! must not break an older reader; the contract round-trip test asserts
//! the wire vocabulary that EXISTS today, not that future additions are
//! rejected. Removing/renaming a field is a breaking change and the test
//! catches it; adding one is forward-compat by design.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

pub mod adapter;
pub mod delivery;
pub mod ledger;
pub mod store_wire;

// ---------------------------------------------------------------------------
// Directive — rally writes; daemon (or self-acking agent) reads.
// ---------------------------------------------------------------------------

/// Why a delivered message was sent.
///
/// This is independent of [`DirectiveKind`]: `DirectiveKind::Deliver` says the
/// transport should submit text, while `MessageIntent` says whether the text is
/// allowed to control the recipient. Unknown future values fail closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageIntent {
    /// Information supplied for the recipient's awareness.
    Inform,
    /// A non-binding request the recipient may accept or refuse.
    Request,
    /// A proposed approach or decision for consideration.
    Propose,
    /// A controlling instruction, subject to Rally authority checks.
    #[default]
    Directive,
    /// A future intent this build does not understand. Controlling by default.
    #[serde(other)]
    Unknown,
}

impl MessageIntent {
    /// Whether this intent may change another session's goal or actions.
    pub const fn is_controlling(self) -> bool {
        matches!(self, Self::Directive | Self::Unknown)
    }

    /// Stable wire/display spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inform => "inform",
            Self::Request => "request",
            Self::Propose => "propose",
            Self::Directive => "directive",
            Self::Unknown => "unknown",
        }
    }
}

/// Kind of actor claimed by the sender at delivery time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// Human operator.
    Human,
    /// Interactive or managed model agent.
    Agent,
    /// Non-interactive service process.
    Service,
    /// Deterministic Rally/system process.
    System,
    /// Actor kind was absent or not understood.
    #[default]
    #[serde(other)]
    Unknown,
}

impl ActorKind {
    /// Stable wire/display spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Service => "service",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }
}

/// The sender's observed Rally room seat when the message was written.
///
/// This is audit metadata, not a durable grant. `lead_epoch` in
/// [`MessageContext`] binds the observation to one room projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomSeat {
    /// Held the room lead lease at the observed lead epoch.
    Lead,
    /// Appeared as a room squad but did not hold the lead lease.
    Participant,
    /// Did not appear in the room projection at send time.
    Unjoined,
    /// Room state was unavailable or legacy metadata omitted the field.
    #[default]
    #[serde(other)]
    Unknown,
}

impl RoomSeat {
    /// Stable wire/display spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Participant => "participant",
            Self::Unjoined => "unjoined",
            Self::Unknown => "unknown",
        }
    }
}

/// Scoped work responsibility asserted by the sender.
///
/// Responsibility never grants room authority. In particular, there is no
/// `Peer` variant: peer is an observer-relative relationship, not a durable
/// responsibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkResponsibility {
    /// Investigates facts, causes, or options without owning implementation.
    Investigator,
    /// Defines task decomposition and dependencies.
    Planner,
    /// Produces the scoped implementation.
    Implementer,
    /// Runs acceptance checks against defined criteria.
    Verifier,
    /// Reviews quality, correctness, or risk independently of implementation.
    Reviewer,
    /// Reconciles accepted outputs into the shared candidate.
    Integrator,
    /// Operates or monitors the runtime without owning product decisions.
    Operator,
    /// No responsibility was asserted.
    #[default]
    Unspecified,
    /// A future responsibility this build does not understand.
    #[serde(other)]
    Unknown,
}

impl WorkResponsibility {
    /// Stable wire/display spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Investigator => "investigator",
            Self::Planner => "planner",
            Self::Implementer => "implementer",
            Self::Verifier => "verifier",
            Self::Reviewer => "reviewer",
            Self::Integrator => "integrator",
            Self::Operator => "operator",
            Self::Unspecified => "unspecified",
            Self::Unknown => "unknown",
        }
    }
}

/// Evidence basis for treating a message as controlling at send time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityBasis {
    /// Sender held the observed room lead lease.
    LeadLease,
    /// A task assignment granted scoped control authority.
    TaskAssignment,
    /// Target opened a handoff to this sender.
    TargetConsent,
    /// Human/service principal supplied an out-of-band grant.
    PrincipalGrant,
    /// Sender and target are the same logical tool id. This does not prove an
    /// exact session identity because `--tool` remains self-asserted.
    SelfTarget,
    /// No lead seat existed, so Rally allowed the documented bootstrap path.
    LeaderlessBootstrap,
    /// The message is non-controlling, so no control authority is required.
    NotRequired,
    /// Legacy or otherwise unverified authority context.
    #[default]
    #[serde(other)]
    Unverified,
}

impl AuthorityBasis {
    /// Stable wire/display spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LeadLease => "lead_lease",
            Self::TaskAssignment => "task_assignment",
            Self::TargetConsent => "target_consent",
            Self::PrincipalGrant => "principal_grant",
            Self::SelfTarget => "self_target",
            Self::LeaderlessBootstrap => "leaderless_bootstrap",
            Self::NotRequired => "not_required",
            Self::Unverified => "unverified",
        }
    }
}

/// Typed sender and authority metadata attached to one delivered message.
///
/// Identity and responsibility fields are claims/observations made at send
/// time. They make the boundary recognizable without pretending Rally has
/// authenticated the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageContext {
    /// Why the message was sent and whether it is controlling.
    #[serde(default)]
    pub intent: MessageIntent,
    /// Claimed actor class.
    #[serde(default)]
    pub actor_kind: ActorKind,
    /// Calling process's current protocol session, when resolvable. This is
    /// observed locally but is not authenticated as belonging to `from`.
    #[serde(
        default,
        alias = "sender_session_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub caller_session_id: Option<String>,
    /// Room seat observed when the message was written.
    #[serde(default)]
    pub room_seat: RoomSeat,
    /// Lead decision sequence associated with `room_seat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_epoch: Option<i64>,
    /// Scoped work responsibility asserted by the sender.
    #[serde(default)]
    pub responsibility: WorkResponsibility,
    /// Evidence basis for control, or `not_required` for non-controlling intent.
    #[serde(default)]
    pub authority_basis: AuthorityBasis,
}

/// A push directive appended to an agent's inbox in the `.rally` ledger.
///
/// `(to, seq)` is the canonical dedup key — the daemon MUST dedup on this
/// pair (H2) and MUST persist its high-water-mark per agent across
/// restarts so a crash-after-deliver-before-receipt does not re-deliver.
///
/// The directive is the entire payload rally writes; the daemon owns its
/// internal `id → pane` mapping (rally never sees pane-ids — that whole
/// bug class is structurally eliminated).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Directive {
    /// Monotonic per-inbox sequence number. `(to, seq)` is the dedup key.
    pub seq: u64,
    /// LOGICAL agent id — never a pane-id, never a tmux target. The daemon
    /// resolves to a pane internally.
    pub to: String,
    /// Author tool id (e.g. `claude_code:lead-01`, `rally-cli`, `build-loop`).
    pub from: String,
    /// Message semantics and claimed/observed sender context. Missing on legacy
    /// rows, which safely default to a controlling directive with unverified
    /// authority and unknown sender context.
    #[serde(default)]
    pub message: MessageContext,
    /// What to do (deliver text / read scrollback / stop the agent).
    pub kind: DirectiveKind,
    /// Interrupt semantic (InterruptBench taxonomy). Serialised as `"type"`
    /// to match the research vocabulary.
    #[serde(rename = "type")]
    pub itype: InterruptType,
    /// Payload text for `Deliver`; ignored for `Read` / `Stop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `true` => synchronous override (daemon does immediate PTY-write).
    /// Restricted to `Stop` + `Retraction` semantics — `Addition` / `Revision`
    /// with `urgent=true` is a contract violation the daemon SHOULD reject.
    #[serde(default)]
    pub urgent: bool,
    /// Unix epoch seconds (f64 to match `.rally/changes.jsonl` substrate).
    pub ts: f64,
}

/// Operation the directive asks the daemon to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveKind {
    /// Deliver `text` to the agent's pane (sync if `urgent`, else async).
    Deliver,
    /// Read raw scrollback from the agent's pane; daemon posts a Receipt
    /// whose `evidence` contains the bytes.
    Read,
    /// Stop the agent gracefully (or with `urgent=true`, immediately via
    /// PTY-write of the host's stop sequence).
    Stop,
}

/// Interrupt semantic. From the InterruptBench paper: agents handle these
/// three classes differently and the daemon may apply different policies
/// (e.g. only `Retraction` is allowed with `urgent=true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptType {
    /// New instruction added to the current task.
    Addition,
    /// Modify the goal / constraints of the current task.
    Revision,
    /// Cancel the current task entirely.
    Retraction,
}

// ---------------------------------------------------------------------------
// Receipt — daemon (or self-acking agent) writes; consumers read.
// ---------------------------------------------------------------------------

/// A receipt the daemon (or a self-acking agent) appends back to the
/// ledger to truthfully report what happened to a Directive.
///
/// No silent-false: a `Receipt` with `status: Failed` is the only way to
/// report failure; absence of a Receipt means "in flight" (or daemon down),
/// reported to consumers as [`DeliveryStatus::Pending`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// The [`Directive::seq`] this receipt answers. `0` is reserved for
    /// daemon heartbeats (not tied to any Directive).
    pub ref_seq: u64,
    /// Agent the original Directive addressed. Mirrored here so a reader
    /// scanning receipts.jsonl never has to cross-reference inbox.jsonl.
    pub to: String,
    /// What happened.
    pub status: DeliveryStatus,
    /// Who posted the receipt (`rally-termd`, an agent's self-ack id, etc.).
    pub by: String,
    /// Truthful evidence — e.g. for `Read`, the bytes; for `Acted`, the
    /// agent's confirmation. Bounded; readers may truncate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Diagnostic for `Failed`. Mutually exclusive with `evidence` is NOT
    /// required — a partial success can carry both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix epoch seconds.
    pub ts: f64,
}

/// Receipt status. `Pending` is a CONSUMER-side synthetic state — it never
/// goes on the wire; a Directive without a Receipt yet IS the Pending
/// state.
///
/// Wire variants are `Delivered | Seen | Acted | Failed`. `Pending` is
/// included in the enum so consumer-facing JSON envelopes (e.g.
/// `rally inject` output) can use a single type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Directive appended; no Receipt yet. CONSUMER-synthetic; not posted
    /// to the ledger.
    Pending,
    /// Daemon wrote bytes to the agent's pane (`Deliver`) or completed the
    /// op (`Read` / `Stop`). The agent may not have *seen* it yet.
    Delivered,
    /// The agent (cooperating) acknowledges it read the directive but has
    /// not necessarily applied it. Optional.
    Seen,
    /// The agent (cooperating) acknowledges it applied the directive.
    /// Strongest positive signal.
    Acted,
    /// Daemon could not deliver; `error` carries the diagnostic.
    Failed,
}

// ---------------------------------------------------------------------------
// Inbox — the only two operations either side needs on the ledger.
// ---------------------------------------------------------------------------

/// The only two operations rally + daemon perform on the ledger.
///
/// Implementations:
/// - [`ledger::FileInbox`] (this crate) — the canonical `.rally`-backed
///   implementation. Append-only NDJSON files; std-only.
/// - Test doubles in `tests/` for harness work.
///
/// Trait implementations MUST be:
/// - **append-atomic**: a partial write must not corrupt prior records.
///   `FileInbox` achieves this by writing each line via a single
///   `write_all` after the line is fully framed in memory + an explicit
///   trailing `\n`.
/// - **read-tolerant**: a half-written final line MUST be skipped, not
///   parsed (mirrors the existing `.rally/changes.jsonl` reader semantics).
/// - **monotonic seq**: `append_directive` MUST assign or accept a `seq`
///   strictly greater than any prior `seq` in that agent's inbox.
pub trait Inbox {
    /// Append a Directive to `directive.to`'s inbox.
    ///
    /// The implementation MAY mutate `seq` to ensure monotonicity (the
    /// canonical `FileInbox` does so); callers may pass `seq: 0` to mean
    /// "assign the next free seq".
    fn append_directive(&self, directive: &Directive) -> std::io::Result<u64>;

    /// Read all Directives for `agent` with `seq > after_seq`, in order.
    ///
    /// Returns an empty vec if the inbox is missing or empty.
    fn read_since(&self, agent: &str, after_seq: u64) -> std::io::Result<Vec<Directive>>;

    /// Append a Receipt to `receipt.to`'s receipt log.
    ///
    /// Receipts have NO seq of their own — they reference [`Receipt::ref_seq`].
    fn append_receipt(&self, receipt: &Receipt) -> std::io::Result<()>;

    /// Read all Receipts for `agent` with `ref_seq > after_ref_seq`, in order.
    /// Convenience for consumer-side polling (e.g. `rally status`).
    fn read_receipts_since(&self, agent: &str, after_ref_seq: u64)
    -> std::io::Result<Vec<Receipt>>;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Current unix epoch seconds as f64 (matches `.rally/changes.jsonl`).
///
/// Panics on a `SystemTime::now() < UNIX_EPOCH` clock skew — a node with
/// a pre-1970 clock has bigger problems than this contract.
pub fn now_ts() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock pre-1970; rally-protocol cannot timestamp");
    dur.as_secs_f64()
}
