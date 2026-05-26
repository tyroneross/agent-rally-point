// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::{ProtocolError, event_value};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    Handoff,
    Ack,
    Feedback,
    Claim,
    ClaimRelease,
    Blocker,
    BlockerResolved,
    Other(String),
}

impl EventKind {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("handoff") => Self::Handoff,
            Some("ack") => Self::Ack,
            Some("feedback") => Self::Feedback,
            Some("claim") => Self::Claim,
            Some("claim-release") => Self::ClaimRelease,
            Some("blocker") => Self::Blocker,
            Some("blocker-resolved") => Self::BlockerResolved,
            Some(value) => Self::Other(value.to_string()),
            None => Self::Other(String::new()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub event: Value,
    pub kind: EventKind,
    pub id: Option<String>,
    pub tool: Option<String>,
    pub thread_id: Option<String>,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
}

impl EventRecord {
    pub fn parse(record: &Value) -> Result<Self, ProtocolError> {
        let event = event_value(record)?;
        Ok(Self {
            id: string_field(&event, "id"),
            kind: EventKind::parse(event.get("kind").and_then(Value::as_str)),
            tool: string_field(&event, "tool"),
            thread_id: string_field(&event, "thread_id"),
            causation_id: string_field(&event, "causation_id"),
            correlation_id: string_field(&event, "correlation_id"),
            event,
        })
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}
