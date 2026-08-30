// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Versioned, host-neutral control-adapter descriptors.
//!
//! A descriptor reports what an adapter actually proves. It deliberately does
//! not turn a successful terminal write into receiver acknowledgement: direct
//! delivery remains an acceleration path over the durable inbox.

use crate::delivery::{AdapterCapabilities, AdapterKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Wire schema for [`AgentAdapterDescriptorV1`].
pub const AGENT_ADAPTER_SCHEMA_V1: &str = "rally.agent-adapter.v1";

/// Truthful delivery tier advertised by one control adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterDeliveryGrade {
    /// The adapter owns the runtime lifecycle and can provide an adapter receipt.
    Managed,
    /// The adapter can write to a live transport but cannot prove receiver acceptance.
    UnverifiedTransport,
    /// The adapter exposes only the durable mailbox; no direct runtime control exists.
    Mailbox,
}

impl AdapterDeliveryGrade {
    /// Stable wire spelling for diagnostics and compact session projections.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::UnverifiedTransport => "unverified_transport",
            Self::Mailbox => "mailbox",
        }
    }
}

/// One runtime operation an adapter can declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterOperation {
    /// Start a runtime for a managed session.
    Start,
    /// Attempt a direct transport delivery after durable directive append.
    Deliver,
    /// Read bounded runtime output.
    Capture,
    /// Stop the exact managed runtime.
    Stop,
    /// Observe liveness for an exact managed runtime.
    Probe,
}

impl AdapterOperation {
    /// Stable wire spelling for session projections and fixtures.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Deliver => "deliver",
            Self::Capture => "capture",
            Self::Stop => "stop",
            Self::Probe => "probe",
        }
    }
}

/// A versioned capability declaration for one local coding-agent control adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAdapterDescriptorV1 {
    /// Required schema identifier for this descriptor.
    pub adapter_schema: String,
    /// Stable, provider-neutral adapter identifier.
    pub adapter_id: AdapterKind,
    /// Delivery truth tier; never infer receiver acknowledgement from this value.
    pub delivery_grade: AdapterDeliveryGrade,
    /// Operations the adapter exposes. This declaration is capability metadata,
    /// not proof that an operation ran successfully for a specific session.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub operations: BTreeSet<AdapterOperation>,
    /// Provider-neutral endpoint capabilities.
    #[serde(default)]
    pub capabilities: AdapterCapabilities,
    /// Provider-owned metadata retained without interpretation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl AgentAdapterDescriptorV1 {
    /// Construct a descriptor using the current schema version.
    pub fn new(
        adapter_id: impl Into<AdapterKind>,
        delivery_grade: AdapterDeliveryGrade,
        operations: impl IntoIterator<Item = AdapterOperation>,
    ) -> Self {
        Self {
            adapter_schema: AGENT_ADAPTER_SCHEMA_V1.to_string(),
            adapter_id: adapter_id.into(),
            delivery_grade,
            operations: operations.into_iter().collect(),
            capabilities: AdapterCapabilities::default(),
            extensions: BTreeMap::new(),
        }
    }

    /// Whether this adapter declared one exact operation.
    pub fn supports_operation(&self, operation: AdapterOperation) -> bool {
        self.operations.contains(&operation)
    }

    /// Return the compact, stable names of all declared operations.
    pub fn operation_names(&self) -> Vec<String> {
        self.operations
            .iter()
            .map(|operation| operation.as_str().to_string())
            .collect()
    }

    /// Validate invariants that every adapter descriptor must satisfy.
    pub fn validate(&self) -> Result<(), String> {
        if self.adapter_schema != AGENT_ADAPTER_SCHEMA_V1 {
            return Err(format!(
                "unsupported adapter schema {}; expected {AGENT_ADAPTER_SCHEMA_V1}",
                self.adapter_schema
            ));
        }
        if self.adapter_id.as_str().trim().is_empty() {
            return Err("adapter_id must not be empty".to_string());
        }
        if let Some(capability) = self.capabilities.conflicting_reserved_capability() {
            return Err(format!(
                "adapter capability {capability} conflicts with its authoritative typed field"
            ));
        }
        if self.delivery_grade == AdapterDeliveryGrade::Mailbox && !self.operations.is_empty() {
            return Err(
                "mailbox adapters must not advertise direct runtime operations".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::CAPABILITY_POSITIVE_ACK;

    #[test]
    fn managed_descriptor_round_trips_and_lists_operations_stably() {
        let descriptor = AgentAdapterDescriptorV1::new(
            "rally.ptyd.v1",
            AdapterDeliveryGrade::Managed,
            [
                AdapterOperation::Probe,
                AdapterOperation::Start,
                AdapterOperation::Deliver,
            ],
        );
        descriptor.validate().unwrap();
        assert_eq!(
            descriptor.operation_names(),
            vec!["start", "deliver", "probe"],
            "BTreeSet must make capability projection deterministic"
        );
        let encoded = serde_json::to_string(&descriptor).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentAdapterDescriptorV1>(&encoded).unwrap(),
            descriptor
        );
    }

    #[test]
    fn validation_rejects_mailbox_runtime_claims_and_reserved_capability_conflicts() {
        let mailbox = AgentAdapterDescriptorV1::new(
            "rally.mailbox.v1",
            AdapterDeliveryGrade::Mailbox,
            [AdapterOperation::Deliver],
        );
        assert!(mailbox.validate().unwrap_err().contains("mailbox"));

        let mut transport = AgentAdapterDescriptorV1::new(
            "rally.tmux.v1",
            AdapterDeliveryGrade::UnverifiedTransport,
            [AdapterOperation::Deliver],
        );
        transport
            .capabilities
            .features
            .insert(CAPABILITY_POSITIVE_ACK.to_string());
        assert!(
            transport
                .validate()
                .unwrap_err()
                .contains(CAPABILITY_POSITIVE_ACK)
        );
    }
}
