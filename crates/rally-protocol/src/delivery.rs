// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Versioned, provider-neutral delivery contracts.
//!
//! R0 adds these contracts without changing what [`crate::ledger::FileInbox`]
//! writes. [`DeliveryEnvelopeV1`] and [`DeliveryReceiptV1`] flatten the legacy
//! [`crate::Directive`] and [`crate::Receipt`] shapes on the wire, so an older
//! JSON reader sees the same required fields and ignores the additive metadata.
//! The legacy Rust structs remain unchanged, which also avoids breaking sibling
//! consumers that construct them with struct literals.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Directive, Receipt};

/// Wire schema for the first provider-neutral delivery envelope.
pub const DELIVERY_SCHEMA_V1: &str = "rally.delivery.v1";
/// Wire schema for delivery evidence carried beside a legacy receipt.
pub const DELIVERY_EVIDENCE_SCHEMA_V1: &str = "rally.delivery-evidence.v1";
/// Wire schema for endpoint registry snapshots consumed by route planners.
pub const ENDPOINT_SCHEMA_V1: &str = "rally.endpoint.v1";
/// Wire schema for the minimal immutable identity of a delivery attempt.
pub const DELIVERY_ATTEMPT_SCHEMA_V1: &str = "rally.delivery-attempt.v1";
/// Reserved capability name backed by [`AdapterCapabilities::supports_positive_ack`].
pub const CAPABILITY_POSITIVE_ACK: &str = "positive_ack";
/// Reserved capability name backed by [`AdapterCapabilities::supports_idempotent_delivery`].
pub const CAPABILITY_IDEMPOTENT_DELIVERY: &str = "idempotent_delivery";

fn default_delivery_schema() -> String {
    DELIVERY_SCHEMA_V1.to_string()
}

fn default_evidence_schema() -> String {
    DELIVERY_EVIDENCE_SCHEMA_V1.to_string()
}

fn default_endpoint_schema() -> String {
    ENDPOINT_SCHEMA_V1.to_string()
}

fn default_attempt_schema() -> String {
    DELIVERY_ATTEMPT_SCHEMA_V1.to_string()
}

fn present(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

/// Build the stable logical-delivery deduplication key.
///
/// The length-prefixed form prevents delimiter collisions even when IDs contain
/// `:` or `|`. Retries to the same repository and target retain this key.
pub fn stable_delivery_dedupe_key(
    event_id: &str,
    repository_id: &str,
    target_agent_id: &str,
) -> String {
    format!(
        "d1|{}:{}|{}:{}|{}:{}",
        event_id.len(),
        event_id,
        repository_id.len(),
        repository_id,
        target_agent_id.len(),
        target_agent_id
    )
}

/// Build a unique attempt ID without changing the logical-delivery dedupe key.
pub fn stable_attempt_id(delivery_dedupe_key: &str, attempt_number: u32) -> String {
    format!(
        "a1|{}:{}|{}",
        delivery_dedupe_key.len(),
        delivery_dedupe_key,
        attempt_number
    )
}

/// Coordination authority role asserted in an event context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Read-only observer.
    Observer,
    /// Ordinary coordinating agent.
    Agent,
    /// Agent currently holding the Rally lead seat.
    LeadAgent,
    /// Repository maintainer.
    Maintainer,
    /// Repository or organization owner.
    Owner,
    /// Deterministic system process.
    System,
}

/// Authorization context asserted on a privileged event.
///
/// R0 records this context; it does not cryptographically authenticate it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthContext {
    /// Asserted role.
    pub role: Role,
    /// Policy revision used to make the authorization decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    /// Provider-neutral operation capabilities asserted for the author.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// Causal, identity, and authorization context attached to a durable event.
///
/// Every field remains optional so legacy facts and directives can be decoded.
/// Semantic validators decide which fields are required for a specific action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Stable writer retry key. For delivery, use [`stable_delivery_dedupe_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Event that directly caused this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    /// Larger flow or user request containing this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Exact prior event answered by this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_event_id: Option<String>,
    /// Work item identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    /// Orchestrator run identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Unique attempt identifier; never use it as the logical dedupe key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    /// Claim identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// Handoff identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<String>,
    /// Live session lease that authored the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_session_id: Option<String>,
    /// Human or service principal behind the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// Logical actor within the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Asserted authorization context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_context: Option<AuthContext>,
}

/// Required identity slot for a coordination event.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredId {
    /// Author session lease.
    FromSessionId,
    /// Referenced prior event.
    RefEventId,
    /// Direct causal event.
    CausationId,
    /// Claim identity.
    ClaimId,
    /// Handoff identity.
    HandoffId,
    /// Work identity.
    WorkId,
    /// Run identity.
    RunId,
    /// Attempt identity.
    AttemptId,
}

/// Compatibility posture for event-envelope validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatMode {
    /// Accept legacy events without a session lease.
    Lenient,
    /// Require a session lease on every non-system event.
    Strict,
}

/// Provider-neutral durable coordination event vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolEventKind {
    /// A session became addressable.
    SessionRegistered,
    /// A session closed normally.
    SessionClosed,
    /// A session lease was revoked.
    SessionRevoked,
    /// Work was created.
    WorkCreated,
    /// Work published a checkpoint.
    WorkCheckpoint,
    /// Work became blocked.
    WorkBlocked,
    /// Work resolved successfully.
    WorkResolved,
    /// Work failed.
    WorkFailed,
    /// Work was cancelled.
    WorkCancelled,
    /// Work was abandoned.
    WorkAbandoned,
    /// Work was superseded.
    WorkSuperseded,
    /// A claim was acquired.
    ClaimAcquired,
    /// A claim was released.
    ClaimReleased,
    /// A claim expired.
    ClaimExpired,
    /// A claim moved between owners.
    ClaimTransferred,
    /// A handoff was requested.
    HandoffRequested,
    /// A target acknowledged a handoff.
    HandoffAcked,
    /// A target accepted a handoff.
    HandoffAccepted,
    /// A target rejected a handoff.
    HandoffRejected,
    /// An artifact was published.
    ArtifactPublished,
    /// A validator reported a result.
    ValidationResult,
    /// A decision was recorded.
    DecisionRecorded,
    /// A conflict was detected.
    ConflictDetected,
    /// A conflict was resolved.
    ConflictResolved,
    /// A consequential operation was declared.
    OperationIntent,
    /// A consequential operation returned evidence.
    OperationResult,
}

/// Missing identity reported by [`ProtocolEventKind::validate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeError {
    /// Event kind being validated.
    pub kind: ProtocolEventKind,
    /// Required identity that was absent.
    pub missing: RequiredId,
}

impl ProtocolEventKind {
    fn is_brainstem(self) -> bool {
        matches!(
            self,
            Self::SessionRegistered | Self::SessionClosed | Self::SessionRevoked
        )
    }

    fn intrinsic_required(self) -> &'static [RequiredId] {
        use ProtocolEventKind as Kind;
        use RequiredId as Id;
        match self {
            Kind::ClaimAcquired | Kind::ClaimReleased | Kind::ClaimExpired => &[Id::ClaimId],
            Kind::ClaimTransferred => &[Id::ClaimId],
            Kind::HandoffRequested => &[Id::HandoffId],
            Kind::HandoffAcked | Kind::HandoffAccepted | Kind::HandoffRejected => {
                &[Id::HandoffId, Id::RefEventId, Id::CausationId]
            }
            Kind::WorkResolved | Kind::WorkSuperseded => {
                &[Id::WorkId, Id::RefEventId, Id::CausationId]
            }
            Kind::WorkFailed => &[Id::WorkId, Id::AttemptId],
            Kind::WorkCheckpoint
            | Kind::WorkBlocked
            | Kind::WorkCancelled
            | Kind::WorkAbandoned => &[Id::WorkId],
            Kind::ConflictResolved => &[Id::RefEventId, Id::CausationId],
            _ => &[],
        }
    }

    /// Validate required causal identities under the selected compatibility mode.
    pub fn validate(
        self,
        envelope: &EventEnvelope,
        mode: CompatMode,
    ) -> Result<(), Vec<EnvelopeError>> {
        // Preserve the active CLI validator's v1 semantics exactly: identity
        // presence means `Some`, even when the contained string is empty.
        // DeliveryEnvelopeV1 applies its stricter non-empty checks separately.
        let has = |required: RequiredId| match required {
            RequiredId::FromSessionId => envelope.from_session_id.is_some(),
            RequiredId::RefEventId => envelope.ref_event_id.is_some(),
            RequiredId::CausationId => envelope.causation_id.is_some(),
            RequiredId::ClaimId => envelope.claim_id.is_some(),
            RequiredId::HandoffId => envelope.handoff_id.is_some(),
            RequiredId::WorkId => envelope.work_id.is_some(),
            RequiredId::RunId => envelope.run_id.is_some(),
            RequiredId::AttemptId => envelope.attempt_id.is_some(),
        };
        let mut errors = self
            .intrinsic_required()
            .iter()
            .copied()
            .filter(|required| !has(*required))
            .map(|missing| EnvelopeError {
                kind: self,
                missing,
            })
            .collect::<Vec<_>>();
        if mode == CompatMode::Strict && !self.is_brainstem() && !has(RequiredId::FromSessionId) {
            errors.push(EnvelopeError {
                kind: self,
                missing: RequiredId::FromSessionId,
            });
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// In-memory idempotency guard for tests and single-process callers.
///
/// Durable retry suppression still belongs to the future delivery-state store.
#[derive(Default)]
pub struct Deduper {
    seen: BTreeSet<String>,
}

impl Deduper {
    /// Create an empty guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return `true` for the first observation and `false` for a duplicate.
    pub fn observe(&mut self, envelope: &EventEnvelope, event_id: &str) -> bool {
        let key = envelope
            .idempotency_key
            .clone()
            .unwrap_or_else(|| event_id.to_string());
        self.seen.insert(key)
    }
}

/// Semantic error in a current-version delivery envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", content = "detail", rename_all = "snake_case")]
pub enum DeliveryContractError {
    /// The envelope names an unsupported schema.
    UnsupportedSchema(String),
    /// A field required by the current contract is absent.
    MissingRequiredField(String),
    /// A required preservation field is not part of the known vocabulary.
    UnsupportedRequiredField(String),
    /// The supplied logical dedupe key does not match event/repository/target.
    DedupeKeyMismatch {
        /// Deterministic value required by the contract.
        expected: String,
        /// Value supplied by the writer.
        found: String,
    },
}

/// A versioned delivery directive that remains flat on the JSON wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryEnvelopeV1 {
    /// Unchanged legacy directive fields, serialized at the top level.
    #[serde(flatten)]
    pub directive: Directive,
    /// Delivery schema. Legacy JSON defaults to v1 on read.
    #[serde(default = "default_delivery_schema")]
    pub delivery_schema: String,
    /// Canonical durable event identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Stable repository/room identity used before endpoint preference scoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Digest of the canonical message payload and semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
    /// Provider-neutral fields an adapter must preserve to be eligible.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_fields: BTreeSet<String>,
    /// Semantic capabilities an adapter must support.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_capabilities: BTreeSet<String>,
    /// Provider-owned data that must survive decode/re-encode unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
    /// Causal, identity, and authorization fields, serialized at the top level.
    #[serde(flatten)]
    pub context: EventEnvelope,
}

impl DeliveryEnvelopeV1 {
    /// Construct a current-version envelope with a stable logical dedupe key.
    pub fn new(
        directive: Directive,
        event_id: impl Into<String>,
        repository_id: impl Into<String>,
        content_digest: impl Into<String>,
    ) -> Self {
        let event_id = event_id.into();
        let repository_id = repository_id.into();
        let context = EventEnvelope {
            idempotency_key: Some(stable_delivery_dedupe_key(
                &event_id,
                &repository_id,
                &directive.to,
            )),
            ..EventEnvelope::default()
        };
        Self {
            directive,
            delivery_schema: default_delivery_schema(),
            event_id: Some(event_id),
            repository_id: Some(repository_id),
            content_digest: Some(content_digest.into()),
            required_fields: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            extensions: BTreeMap::new(),
            context,
        }
    }

    /// Wrap a legacy directive for replay. [`Self::validate_current`] will
    /// report the current-version fields that still need migration.
    pub fn from_legacy(directive: Directive) -> Self {
        Self {
            directive,
            delivery_schema: default_delivery_schema(),
            event_id: None,
            repository_id: None,
            content_digest: None,
            required_fields: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            extensions: BTreeMap::new(),
            context: EventEnvelope::default(),
        }
    }

    /// Return the stable logical-delivery dedupe key when present.
    pub fn delivery_dedupe_key(&self) -> Option<&str> {
        self.context.idempotency_key.as_deref()
    }

    fn required_field_present(&self, field: &str) -> Option<bool> {
        Some(match field {
            "from" => !self.directive.from.trim().is_empty(),
            "to" => !self.directive.to.trim().is_empty(),
            "event_id" => present(&self.event_id),
            "repository_id" => present(&self.repository_id),
            "idempotency_key" => present(&self.context.idempotency_key),
            "content_digest" => present(&self.content_digest),
            "correlation_id" => present(&self.context.correlation_id),
            "causation_id" => present(&self.context.causation_id),
            "ref_event_id" => present(&self.context.ref_event_id),
            "work_id" => present(&self.context.work_id),
            "run_id" => present(&self.context.run_id),
            "attempt_id" => present(&self.context.attempt_id),
            "claim_id" => present(&self.context.claim_id),
            "handoff_id" => present(&self.context.handoff_id),
            "from_session_id" => present(&self.context.from_session_id),
            "principal_id" => present(&self.context.principal_id),
            "actor_id" => present(&self.context.actor_id),
            "auth_context" => self.context.auth_context.is_some(),
            "text" => self.directive.text.is_some(),
            "extensions" => !self.extensions.is_empty(),
            other if other.starts_with("extensions.") => {
                self.extensions.contains_key(&other["extensions.".len()..])
            }
            _ => return None,
        })
    }

    /// Validate the fields required before a planner may select an endpoint.
    pub fn validate_current(&self) -> Result<(), Vec<DeliveryContractError>> {
        let mut errors = Vec::new();
        if self.delivery_schema != DELIVERY_SCHEMA_V1 {
            errors.push(DeliveryContractError::UnsupportedSchema(
                self.delivery_schema.clone(),
            ));
        }
        for (name, value) in [
            ("event_id", &self.event_id),
            ("repository_id", &self.repository_id),
            ("content_digest", &self.content_digest),
            ("idempotency_key", &self.context.idempotency_key),
        ] {
            if !present(value) {
                errors.push(DeliveryContractError::MissingRequiredField(
                    name.to_string(),
                ));
            }
        }
        for (name, value) in [
            ("from", self.directive.from.as_str()),
            ("to", self.directive.to.as_str()),
        ] {
            if value.trim().is_empty() {
                errors.push(DeliveryContractError::MissingRequiredField(
                    name.to_string(),
                ));
            }
        }
        if let (Some(event_id), Some(repository_id), Some(found)) = (
            self.event_id.as_deref(),
            self.repository_id.as_deref(),
            self.context.idempotency_key.as_deref(),
        ) {
            let expected = stable_delivery_dedupe_key(event_id, repository_id, &self.directive.to);
            if found != expected {
                errors.push(DeliveryContractError::DedupeKeyMismatch {
                    expected,
                    found: found.to_string(),
                });
            }
        }
        for field in &self.required_fields {
            match self.required_field_present(field) {
                Some(true) => {}
                Some(false) => {
                    errors.push(DeliveryContractError::MissingRequiredField(field.clone()))
                }
                None => errors.push(DeliveryContractError::UnsupportedRequiredField(
                    field.clone(),
                )),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Semantic delivery state. These values do not extend legacy DeliveryStatus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Canonical message was recorded.
    Recorded,
    /// A route was selected but no transport call is proven.
    RouteSelected,
    /// A transport accepted or wrote the message.
    TransportSent,
    /// The target asserted or proved receipt.
    TargetAcknowledged,
    /// The target reports active work.
    Working,
    /// The target reports completion.
    Completed,
    /// Delivery definitely failed.
    Failed,
    /// The transport may have sent, but the outcome cannot be established.
    OutcomeUnknown,
}

/// Provenance class for delivery evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// Sender-authored observation.
    Sender,
    /// Adapter or transport-authored observation.
    Transport,
    /// Target-authored assertion without cryptographic/session proof.
    AssertedTarget,
    /// Target-authored evidence bound to a separately verified session identity.
    VerifiedTarget,
}

/// Expected identities and correlations for semantic evidence validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceExpectation {
    /// Canonical event being acknowledged.
    pub event_id: String,
    /// Exact logical target agent.
    pub target_agent_id: String,
    /// Exact target session when the route was pinned to one.
    pub target_session_id: Option<String>,
    /// Required flow correlation when present on the envelope.
    pub correlation_id: Option<String>,
    /// Required handoff correlation when present on the envelope.
    pub handoff_id: Option<String>,
}

/// Semantic evidence validation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceValidationError {
    /// Evidence names an unsupported semantic schema.
    UnsupportedSchema(String),
    /// Sender or transport attempted to claim a target-only state.
    UnauthorizedPromotion {
        /// State being claimed.
        state: DeliveryState,
        /// Evidence source making the claim.
        source: EvidenceSource,
    },
    /// Evidence references the wrong event.
    EventMismatch,
    /// Evidence names the wrong logical target.
    TargetMismatch,
    /// Evidence names the wrong or no target session.
    SessionMismatch,
    /// Evidence references the wrong or no correlation ID.
    CorrelationMismatch,
    /// Evidence references the wrong or no handoff ID.
    HandoffMismatch,
}

/// Versioned semantic evidence returned through Rally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryEvidenceV1 {
    /// Evidence schema.
    #[serde(default = "default_evidence_schema")]
    pub delivery_evidence_schema: String,
    /// Canonical event this evidence describes.
    pub event_id: String,
    /// Unique attempt when the evidence is attempt-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    /// Semantic state being reported.
    pub state: DeliveryState,
    /// Authority class of the author.
    pub source: EvidenceSource,
    /// Logical agent asserted as the author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_agent_id: Option<String>,
    /// Session lease asserted as the author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_session_id: Option<String>,
    /// Larger flow correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Handoff correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<String>,
    /// Opaque adapter evidence retained losslessly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl DeliveryEvidenceV1 {
    fn target_only_state(&self) -> bool {
        matches!(
            self.state,
            DeliveryState::TargetAcknowledged | DeliveryState::Working | DeliveryState::Completed
        )
    }

    /// Validate semantic authority plus exact target/correlation bindings.
    pub fn validate_against(
        &self,
        expected: &EvidenceExpectation,
    ) -> Result<(), EvidenceValidationError> {
        if self.delivery_evidence_schema != DELIVERY_EVIDENCE_SCHEMA_V1 {
            return Err(EvidenceValidationError::UnsupportedSchema(
                self.delivery_evidence_schema.clone(),
            ));
        }
        if self.target_only_state()
            && !matches!(
                self.source,
                EvidenceSource::AssertedTarget | EvidenceSource::VerifiedTarget
            )
        {
            return Err(EvidenceValidationError::UnauthorizedPromotion {
                state: self.state,
                source: self.source,
            });
        }
        if self.event_id != expected.event_id {
            return Err(EvidenceValidationError::EventMismatch);
        }
        if matches!(
            self.source,
            EvidenceSource::AssertedTarget | EvidenceSource::VerifiedTarget
        ) && self.author_agent_id.as_deref() != Some(expected.target_agent_id.as_str())
        {
            return Err(EvidenceValidationError::TargetMismatch);
        }
        if self.source == EvidenceSource::VerifiedTarget
            && (expected.target_session_id.is_none()
                || self.author_session_id != expected.target_session_id)
        {
            return Err(EvidenceValidationError::SessionMismatch);
        }
        if self.source == EvidenceSource::AssertedTarget
            && self.target_only_state()
            && expected.target_session_id.is_some()
            && self.author_session_id != expected.target_session_id
        {
            return Err(EvidenceValidationError::SessionMismatch);
        }
        if expected.correlation_id.is_some() && self.correlation_id != expected.correlation_id {
            return Err(EvidenceValidationError::CorrelationMismatch);
        }
        if expected.handoff_id.is_some() && self.handoff_id != expected.handoff_id {
            return Err(EvidenceValidationError::HandoffMismatch);
        }
        Ok(())
    }
}

/// A versioned delivery receipt that remains flat on the legacy JSON wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryReceiptV1 {
    /// Unchanged legacy receipt fields, serialized at the top level.
    #[serde(flatten)]
    pub receipt: Receipt,
    /// Delivery schema. Legacy JSON defaults to v1 on read.
    #[serde(default = "default_delivery_schema")]
    pub delivery_schema: String,
    /// Typed semantic evidence; absence means the legacy receipt has only
    /// transport/self-asserted status semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_evidence: Option<DeliveryEvidenceV1>,
    /// Provider-owned fields retained losslessly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl DeliveryReceiptV1 {
    /// Wrap a legacy receipt without promoting it to semantic ACK/completion.
    pub fn from_legacy(receipt: Receipt) -> Self {
        Self {
            receipt,
            delivery_schema: default_delivery_schema(),
            delivery_evidence: None,
            extensions: BTreeMap::new(),
        }
    }
}

/// Extensible adapter identifier. Unknown strings remain round-trippable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdapterKind(pub String);

impl AdapterKind {
    /// Borrow the stable adapter identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AdapterKind {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for AdapterKind {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Provider-neutral capabilities used to filter endpoint candidates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterCapabilities {
    /// Envelope fields the adapter preserves end to end.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub preserves_fields: BTreeSet<String>,
    /// Extensible semantic capabilities such as `a2a` or `streaming`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<String>,
    /// Whether the route can return target-authored positive ACK evidence.
    #[serde(default)]
    pub supports_positive_ack: bool,
    /// Whether the target deduplicates the stable delivery key across retries.
    #[serde(default)]
    pub supports_idempotent_delivery: bool,
    /// Provider-owned capability data retained losslessly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl AdapterCapabilities {
    /// Resolve one capability through its single authoritative representation.
    ///
    /// Reserved capabilities use their typed fields. All other capability
    /// names use the extensible feature set.
    pub fn supports(&self, capability: &str) -> bool {
        match capability {
            CAPABILITY_POSITIVE_ACK => self.supports_positive_ack,
            CAPABILITY_IDEMPOTENT_DELIVERY => self.supports_idempotent_delivery,
            _ => self.features.contains(capability),
        }
    }

    /// Return a reserved feature that contradicts its authoritative typed field.
    pub fn conflicting_reserved_capability(&self) -> Option<&'static str> {
        if self.features.contains(CAPABILITY_POSITIVE_ACK) && !self.supports_positive_ack {
            return Some(CAPABILITY_POSITIVE_ACK);
        }
        if self.features.contains(CAPABILITY_IDEMPOTENT_DELIVERY)
            && !self.supports_idempotent_delivery
        {
            return Some(CAPABILITY_IDEMPOTENT_DELIVERY);
        }
        None
    }
}

/// Observed endpoint health supplied to the pure planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointHealth {
    /// Endpoint is observed ready.
    Ready,
    /// Endpoint is reachable but impaired.
    Degraded,
    /// Endpoint is observed unavailable.
    Unavailable,
    /// No trustworthy observation exists.
    Unknown,
}

/// Versioned endpoint registry snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointDescriptorV1 {
    /// Endpoint schema.
    #[serde(default = "default_endpoint_schema")]
    pub endpoint_schema: String,
    /// Repository/room identity.
    pub repository_id: String,
    /// Exact logical Rally agent identity.
    pub agent_id: String,
    /// Live session lease when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Stable addressable endpoint identity.
    pub endpoint_id: String,
    /// Adapter used to reach the endpoint.
    pub adapter: AdapterKind,
    /// Provider-neutral route capabilities.
    #[serde(default)]
    pub capabilities: AdapterCapabilities,
    /// Last observed health.
    pub health: EndpointHealth,
    /// Whether runtime signals could not distinguish this endpoint from peers.
    #[serde(default)]
    pub ambiguous: bool,
    /// Observation timestamp supplied by the registry.
    pub observed_at: f64,
    /// Soft expiry; the planner rejects endpoints at or past this timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    /// Delivery schemas this endpoint can decode without semantic loss.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub accepted_schemas: BTreeSet<String>,
    /// Principal associated with the endpoint, for tracking only in R0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// Provider-owned endpoint data retained losslessly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

/// Outcome of one delivery attempt. Leases and deadlines remain R2 concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryAttemptOutcome {
    /// Attempt identity exists; no send is proven.
    Planned,
    /// Adapter proved the failure occurred before any send.
    FailedBeforeSend,
    /// Transport accepted or wrote the message, but no target ACK exists.
    TransportSent,
    /// Response loss prevents determining whether the message was sent.
    OutcomeUnknown,
    /// Target acknowledged the message.
    TargetAcknowledged,
    /// Target completed the requested work.
    Completed,
    /// Failure occurred after a send may have happened.
    FailedAfterSend,
}

/// Versioned immutable attempt identity plus its latest coarse outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryAttemptV1 {
    /// Attempt schema.
    #[serde(default = "default_attempt_schema")]
    pub delivery_attempt_schema: String,
    /// Canonical event being delivered.
    pub event_id: String,
    /// Stable key shared by retries of this logical delivery.
    pub delivery_dedupe_key: String,
    /// Unique attempt identifier.
    pub attempt_id: String,
    /// Monotonic attempt number within this logical delivery.
    pub attempt_number: u32,
    /// Endpoint selected for this attempt.
    pub endpoint_id: String,
    /// Latest coarse outcome.
    pub outcome: DeliveryAttemptOutcome,
    /// Whether the stable key reached this endpoint through its advertised
    /// deduplication contract. R1 permits a retry only through the same
    /// endpoint; cross-endpoint retries require a future verified dedupe domain.
    #[serde(default)]
    pub idempotent_delivery: bool,
    /// Provider-owned attempt data retained losslessly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl DeliveryAttemptV1 {
    /// Construct a unique attempt while retaining the logical delivery key.
    pub fn new(
        event_id: impl Into<String>,
        delivery_dedupe_key: impl Into<String>,
        attempt_number: u32,
        endpoint_id: impl Into<String>,
        outcome: DeliveryAttemptOutcome,
        idempotent_delivery: bool,
    ) -> Self {
        let delivery_dedupe_key = delivery_dedupe_key.into();
        Self {
            delivery_attempt_schema: default_attempt_schema(),
            event_id: event_id.into(),
            attempt_id: stable_attempt_id(&delivery_dedupe_key, attempt_number),
            delivery_dedupe_key,
            attempt_number,
            endpoint_id: endpoint_id.into(),
            outcome,
            idempotent_delivery,
            extensions: BTreeMap::new(),
        }
    }

    /// Whether the same endpoint may be retried without known duplicate risk.
    pub fn permits_same_endpoint_retry(&self) -> bool {
        match self.outcome {
            DeliveryAttemptOutcome::FailedBeforeSend => true,
            DeliveryAttemptOutcome::TransportSent
            | DeliveryAttemptOutcome::OutcomeUnknown
            | DeliveryAttemptOutcome::FailedAfterSend => self.idempotent_delivery,
            DeliveryAttemptOutcome::Planned
            | DeliveryAttemptOutcome::TargetAcknowledged
            | DeliveryAttemptOutcome::Completed => false,
        }
    }
}
