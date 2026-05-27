// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::{ProtocolError, event_value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    Handoff,
    Ack,
    Feedback,
    Claim,
    ClaimRelease,
    Blocker,
    BlockerResolved,
    Profile,
    Task,
    Artifact,
    Decision,
    Lesson,
    Subscription,
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
            Some("profile") => Self::Profile,
            Some("task") => Self::Task,
            Some("artifact") => Self::Artifact,
            Some("decision") => Self::Decision,
            Some("lesson") => Self::Lesson,
            Some("subscription") => Self::Subscription,
            Some(value) => Self::Other(value.to_string()),
            None => Self::Other(String::new()),
        }
    }

    pub fn as_kind_str(&self) -> &str {
        match self {
            Self::Handoff => "handoff",
            Self::Ack => "ack",
            Self::Feedback => "feedback",
            Self::Claim => "claim",
            Self::ClaimRelease => "claim-release",
            Self::Blocker => "blocker",
            Self::BlockerResolved => "blocker-resolved",
            Self::Profile => "profile",
            Self::Task => "task",
            Self::Artifact => "artifact",
            Self::Decision => "decision",
            Self::Lesson => "lesson",
            Self::Subscription => "subscription",
            Self::Other(value) => value.as_str(),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Other(value) if value.is_empty() => "event",
            value => value.as_kind_str(),
        }
    }

    pub fn event_type(&self) -> String {
        match self {
            Self::Handoff => "agent-rally.handoff.created.v1".to_string(),
            Self::Ack => "agent-rally.handoff.acknowledged.v1".to_string(),
            Self::Feedback => "agent-rally.feedback.posted.v1".to_string(),
            Self::Claim => "agent-rally.claim.created.v1".to_string(),
            Self::ClaimRelease => "agent-rally.claim.released.v1".to_string(),
            Self::Blocker => "agent-rally.blocker.raised.v1".to_string(),
            Self::BlockerResolved => "agent-rally.blocker.resolved.v1".to_string(),
            Self::Profile => "agent-rally.profile.updated.v1".to_string(),
            Self::Task => "agent-rally.task.updated.v1".to_string(),
            Self::Artifact => "agent-rally.artifact.recorded.v1".to_string(),
            Self::Decision => "agent-rally.decision.recorded.v1".to_string(),
            Self::Lesson => "agent-rally.lesson.recorded.v1".to_string(),
            Self::Subscription => "agent-rally.subscription.updated.v1".to_string(),
            Self::Other(value) => format!("agent-rally.{value}.v1"),
        }
    }

    pub fn schema_name(&self) -> String {
        match self {
            Self::Handoff => "handoff.created.v1".to_string(),
            Self::Ack => "handoff.acknowledged.v1".to_string(),
            Self::Feedback => "feedback.posted.v1".to_string(),
            Self::Claim => "claim.created.v1".to_string(),
            Self::ClaimRelease => "claim.released.v1".to_string(),
            Self::Blocker => "blocker.raised.v1".to_string(),
            Self::BlockerResolved => "blocker.resolved.v1".to_string(),
            Self::Profile => "profile.updated.v1".to_string(),
            Self::Task => "task.updated.v1".to_string(),
            Self::Artifact => "artifact.recorded.v1".to_string(),
            Self::Decision => "decision.recorded.v1".to_string(),
            Self::Lesson => "lesson.recorded.v1".to_string(),
            Self::Subscription => "subscription.updated.v1".to_string(),
            Self::Other(value) => format!("{value}.v1"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum EventPayload {
    Handoff(HandoffPayload),
    Ack(AckPayload),
    Feedback(AckPayload),
    Claim(ClaimPayload),
    ClaimRelease(ClaimReleasePayload),
    Blocker(BlockerPayload),
    BlockerResolved(BlockerResolvedPayload),
    Profile(ProfilePayload),
    Task(TaskPayload),
    Artifact(ArtifactPayload),
    Decision(DecisionPayload),
    Lesson(LessonPayload),
    Subscription(SubscriptionPayload),
    Other { kind: String, payload: Value },
}

impl EventPayload {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Handoff(_) => EventKind::Handoff,
            Self::Ack(_) => EventKind::Ack,
            Self::Feedback(_) => EventKind::Feedback,
            Self::Claim(_) => EventKind::Claim,
            Self::ClaimRelease(_) => EventKind::ClaimRelease,
            Self::Blocker(_) => EventKind::Blocker,
            Self::BlockerResolved(_) => EventKind::BlockerResolved,
            Self::Profile(_) => EventKind::Profile,
            Self::Task(_) => EventKind::Task,
            Self::Artifact(_) => EventKind::Artifact,
            Self::Decision(_) => EventKind::Decision,
            Self::Lesson(_) => EventKind::Lesson,
            Self::Subscription(_) => EventKind::Subscription,
            Self::Other { kind, .. } => EventKind::Other(kind.clone()),
        }
    }

    pub fn parse(kind: &EventKind, payload: &Value) -> Option<Self> {
        match kind {
            EventKind::Handoff => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Handoff),
            EventKind::Ack => serde_json::from_value(payload.clone()).ok().map(Self::Ack),
            EventKind::Feedback => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Feedback),
            EventKind::Claim => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Claim),
            EventKind::ClaimRelease => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::ClaimRelease),
            EventKind::Blocker => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Blocker),
            EventKind::BlockerResolved => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::BlockerResolved),
            EventKind::Profile => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Profile),
            EventKind::Task => serde_json::from_value(payload.clone()).ok().map(Self::Task),
            EventKind::Artifact => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Artifact),
            EventKind::Decision => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Decision),
            EventKind::Lesson => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Lesson),
            EventKind::Subscription => serde_json::from_value(payload.clone())
                .ok()
                .map(Self::Subscription),
            EventKind::Other(value) => Some(Self::Other {
                kind: value.clone(),
                payload: payload.clone(),
            }),
        }
    }

    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            Self::Handoff(payload) => serde_json::to_value(payload),
            Self::Ack(payload) | Self::Feedback(payload) => serde_json::to_value(payload),
            Self::Claim(payload) => serde_json::to_value(payload),
            Self::ClaimRelease(payload) => serde_json::to_value(payload),
            Self::Blocker(payload) => serde_json::to_value(payload),
            Self::BlockerResolved(payload) => serde_json::to_value(payload),
            Self::Profile(payload) => serde_json::to_value(payload),
            Self::Task(payload) => serde_json::to_value(payload),
            Self::Artifact(payload) => serde_json::to_value(payload),
            Self::Decision(payload) => serde_json::to_value(payload),
            Self::Lesson(payload) => serde_json::to_value(payload),
            Self::Subscription(payload) => serde_json::to_value(payload),
            Self::Other { payload, .. } => Ok(payload.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffPayload {
    pub subject: String,
    #[serde(default)]
    pub to_tool: Option<String>,
    #[serde(default)]
    pub from_tool: Option<String>,
    #[serde(default = "default_requires_ack")]
    pub requires_ack: bool,
    #[serde(default)]
    pub ref_files: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AckPayload {
    pub ref_handoff_id: String,
    pub verdict: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClaimPayload {
    pub owner_tool: String,
    pub resource: String,
    pub subject: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClaimReleasePayload {
    pub ref_claim_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockerPayload {
    pub subject: String,
    pub reason: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockerResolvedPayload {
    pub ref_blocker_id: String,
    pub resolution: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfilePayload {
    pub tool: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub watch: Vec<String>,
    #[serde(default)]
    pub current_task: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskPayload {
    pub subject: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub owner_tool: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub verification: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactPayload {
    pub subject: String,
    pub artifact_kind: String,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub ref_task_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DecisionPayload {
    pub subject: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LessonPayload {
    pub subject: String,
    #[serde(default)]
    pub lesson_kind: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub source_event_ids: Vec<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionPayload {
    pub tool: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub event_kinds: Vec<String>,
    #[serde(default)]
    pub threads: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<String>,
}

fn default_requires_ack() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub event: Value,
    pub kind: EventKind,
    pub payload: Option<EventPayload>,
    pub id: Option<String>,
    pub tool: Option<String>,
    pub thread_id: Option<String>,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
}

impl EventRecord {
    pub fn parse(record: &Value) -> Result<Self, ProtocolError> {
        let event = event_value(record)?;
        let kind = EventKind::parse(event.get("kind").and_then(Value::as_str));
        let payload = event
            .get("payload")
            .and_then(|value| EventPayload::parse(&kind, value));
        Ok(Self {
            id: string_field(&event, "id"),
            kind,
            payload,
            tool: string_field(&event, "tool"),
            thread_id: string_field(&event, "thread_id"),
            causation_id: string_field(&event, "causation_id"),
            correlation_id: string_field(&event, "correlation_id"),
            event,
        })
    }

    pub fn subject_label(&self) -> String {
        match self.payload.as_ref() {
            Some(EventPayload::Handoff(payload)) => payload.subject.clone(),
            Some(EventPayload::Claim(payload)) => payload.subject.clone(),
            Some(EventPayload::Blocker(payload)) => payload.subject.clone(),
            Some(EventPayload::Profile(payload)) => format!("profile {}", payload.tool),
            Some(EventPayload::Task(payload)) => payload.subject.clone(),
            Some(EventPayload::Artifact(payload)) => payload.subject.clone(),
            Some(EventPayload::Decision(payload)) => payload.subject.clone(),
            Some(EventPayload::Lesson(payload)) => payload.subject.clone(),
            Some(EventPayload::Subscription(payload)) => {
                format!("subscription {}", payload.tool)
            }
            Some(EventPayload::Ack(payload)) | Some(EventPayload::Feedback(payload)) => payload
                .summary
                .clone()
                .or_else(|| payload.reason.clone())
                .unwrap_or_else(|| payload.ref_handoff_id.clone()),
            Some(EventPayload::ClaimRelease(payload)) => payload
                .reason
                .clone()
                .unwrap_or_else(|| format!("release {}", payload.ref_claim_id)),
            Some(EventPayload::BlockerResolved(payload)) => payload.resolution.clone(),
            Some(EventPayload::Other { payload, .. }) => payload
                .get("subject")
                .and_then(Value::as_str)
                .or_else(|| payload.get("summary").and_then(Value::as_str))
                .or_else(|| payload.get("notes").and_then(Value::as_str))
                .unwrap_or("(no subject)")
                .to_string(),
            None => self
                .event
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("(no subject)")
                .to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventBuilder {
    id: String,
    payload: EventPayload,
    tool: String,
    model: String,
    run_id: String,
    app_slug: String,
    thread_id: String,
    source: Option<String>,
    subject: Option<String>,
    time: Option<String>,
    causation_id: Option<String>,
    correlation_id: Option<String>,
}

impl EventBuilder {
    pub fn new(
        id: impl Into<String>,
        payload: EventPayload,
        tool: impl Into<String>,
        run_id: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> Self {
        let tool = tool.into();
        Self {
            id: id.into(),
            payload,
            source: Some(format!("urn:agent-rally-point:tool:{tool}")),
            subject: Some("agent-rally-point".to_string()),
            tool,
            model: "unknown".to_string(),
            run_id: run_id.into(),
            app_slug: "agent-rally-point".to_string(),
            thread_id: thread_id.into(),
            time: None,
            causation_id: None,
            correlation_id: None,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn app_slug(mut self, app_slug: impl Into<String>) -> Self {
        self.app_slug = app_slug.into();
        self
    }

    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn time(mut self, time: impl Into<String>) -> Self {
        self.time = Some(time.into());
        self
    }

    pub fn causation_id(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }

    pub fn correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    pub fn build(self) -> Result<Value, serde_json::Error> {
        let kind = self.payload.kind();
        let mut event = Map::new();
        event.insert("specversion".to_string(), json!("1.0"));
        event.insert("id".to_string(), json!(self.id));
        event.insert("type".to_string(), json!(kind.event_type()));
        event.insert("kind".to_string(), json!(kind.as_kind_str()));
        event.insert("tool".to_string(), json!(self.tool));
        event.insert("model".to_string(), json!(self.model));
        event.insert("run_id".to_string(), json!(self.run_id));
        event.insert("app_slug".to_string(), json!(self.app_slug));
        event.insert("thread_id".to_string(), json!(self.thread_id));
        event.insert("datacontenttype".to_string(), json!("application/json"));
        event.insert(
            "dataschema".to_string(),
            json!(format!(
                "urn:agent-rally-point:schema:{}",
                kind.schema_name()
            )),
        );
        if let Some(source) = self.source {
            event.insert("source".to_string(), json!(source));
        }
        if let Some(subject) = self.subject {
            event.insert("subject".to_string(), json!(subject));
        }
        if let Some(time) = self.time {
            event.insert("time".to_string(), json!(time));
        }
        event.insert(
            "causation_id".to_string(),
            self.causation_id.map_or(Value::Null, Value::String),
        );
        if let Some(correlation_id) = self.correlation_id {
            event.insert("correlation_id".to_string(), json!(correlation_id));
        }
        event.insert("payload".to_string(), self.payload.to_value()?);
        Ok(Value::Object(event))
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}
