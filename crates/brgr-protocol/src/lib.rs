//! Versioned protocol types shared by every brgr component.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SCHEMA_V1: &str = "brgr/v1";

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(TaskId);
uuid_id!(AttemptId);
uuid_id!(EventId);
uuid_id!(ResultId);
uuid_id!(DecisionId);

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerId(String);

impl OwnerId {
    /// Creates a non-empty owner identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidOwnerId`] when the value is empty or
    /// exceeds 256 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 256 {
            return Err(ProtocolError::InvalidOwnerId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Queued,
    Starting,
    Running,
    Blocked,
    Collecting,
    CancelRequested,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Candidate,
    Failed,
    Cancelled,
    Lost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionVerdict {
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub harness_id: String,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactContract {
    pub media_type: String,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttemptBudget {
    pub deadline_seconds: u64,
    pub max_attempts: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub schema: String,
    pub task_id: TaskId,
    pub revision: u32,
    pub create_request_id: String,
    pub owner_id: OwnerId,
    pub objective: String,
    pub workspace: String,
    pub route: Route,
    pub required_capabilities: Vec<String>,
    pub artifact_contract: ArtifactContract,
    pub acceptance_criteria: Vec<String>,
    pub budget: AttemptBudget,
}

impl TaskSpec {
    /// Validates the bounded v1 task contract.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] when a required field or execution bound is
    /// invalid.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != SCHEMA_V1 {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        if self.objective.trim().is_empty() {
            return Err(ProtocolError::EmptyObjective);
        }
        if self.workspace.trim().is_empty() {
            return Err(ProtocolError::EmptyWorkspace);
        }
        if self.route.harness_id.trim().is_empty() {
            return Err(ProtocolError::EmptyHarness);
        }
        if self
            .route
            .requested_model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
            || self
                .route
                .requested_effort
                .as_ref()
                .is_some_and(|effort| effort.trim().is_empty())
        {
            return Err(ProtocolError::EmptyRouteSelector);
        }
        if self.acceptance_criteria.is_empty()
            || self
                .acceptance_criteria
                .iter()
                .any(|criterion| criterion.trim().is_empty())
        {
            return Err(ProtocolError::InvalidAcceptanceCriteria);
        }
        if self.artifact_contract.max_bytes == 0 || self.budget.deadline_seconds == 0 {
            return Err(ProtocolError::InvalidBudget);
        }
        if self.budget.max_attempts == 0 || self.budget.max_attempts > 2 {
            return Err(ProtocolError::InvalidAttemptCount);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub digest: String,
    pub bytes: u64,
    pub media_type: String,
    pub store_relative_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    HarnessJsonl,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteObservation {
    pub model: Option<String>,
    pub model_source: ObservationSource,
    pub effort: Option<String>,
    pub effort_source: ObservationSource,
}

impl RouteObservation {
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            model: None,
            model_source: ObservationSource::Unavailable,
            effort: None,
            effort_source: ObservationSource::Unavailable,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResultEnvelope {
    pub schema: String,
    pub task_id: TaskId,
    pub revision: u32,
    pub attempt_id: AttemptId,
    pub result_id: ResultId,
    pub outcome: TerminalOutcome,
    pub artifacts: Vec<ArtifactRef>,
    pub error: Option<String>,
    /// Transient native evidence; the store commits it separately from the
    /// versioned result envelope so older binaries retain its decision digest.
    #[serde(skip)]
    pub route_observation: Option<RouteObservation>,
    pub unresolved_effects: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub schema: String,
    pub decision_id: DecisionId,
    pub owner_id: OwnerId,
    pub task_id: TaskId,
    pub revision: u32,
    pub result_id: ResultId,
    pub result_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_epoch: Option<u64>,
    pub verdict: DecisionVerdict,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub owner_id: OwnerId,
    pub result: ResultEnvelope,
    pub acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Starting,
    Running,
    Blocked,
    Collecting,
    CancelRequested,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema: String,
    pub event_id: EventId,
    pub attempt_id: AttemptId,
    pub producer: String,
    pub producer_seq: u64,
    pub kind: EventKind,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("owner id must contain 1 to 256 bytes")]
    InvalidOwnerId,
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    #[error("objective must not be empty")]
    EmptyObjective,
    #[error("workspace must not be empty")]
    EmptyWorkspace,
    #[error("harness id must not be empty")]
    EmptyHarness,
    #[error("requested model and effort must not be blank")]
    EmptyRouteSelector,
    #[error("at least one nonempty acceptance criterion is required")]
    InvalidAcceptanceCriteria,
    #[error("artifact and deadline limits must be positive")]
    InvalidBudget,
    #[error("v1 permits one initial attempt and at most one retry")]
    InvalidAttemptCount,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_spec_round_trips_without_losing_typed_ids() {
        let task = TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            create_request_id: "request-1".to_owned(),
            owner_id: OwnerId::new("codex:thread-1").unwrap(),
            objective: "Review the change".to_owned(),
            workspace: "/tmp/worktree".to_owned(),
            route: Route {
                harness_id: "local.synthetic".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec!["completion".to_owned()],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1_048_576,
            },
            acceptance_criteria: vec!["report exists".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 3_600,
                max_attempts: 2,
            },
        };

        task.validate().unwrap();
        let encoded = serde_json::to_string(&task).unwrap();
        let decoded: TaskSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, task);
    }

    #[test]
    fn task_spec_rejects_unbounded_attempts() {
        let mut task = TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            create_request_id: "request-1".to_owned(),
            owner_id: OwnerId::new("codex:thread-1").unwrap(),
            objective: "Review".to_owned(),
            workspace: "/tmp/worktree".to_owned(),
            route: Route {
                harness_id: "local.synthetic".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec![],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1,
            },
            acceptance_criteria: vec!["reviewed result".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 1,
                max_attempts: 3,
            },
        };

        assert_eq!(task.validate(), Err(ProtocolError::InvalidAttemptCount));
        task.budget.max_attempts = 2;
        assert!(task.validate().is_ok());
        task.acceptance_criteria.clear();
        assert_eq!(
            task.validate(),
            Err(ProtocolError::InvalidAcceptanceCriteria)
        );
    }

    #[test]
    fn old_result_without_route_observation_keeps_its_serialized_digest_shape() {
        let old = ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            attempt_id: AttemptId::new(),
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Failed,
            artifacts: vec![],
            error: Some("old result".to_owned()),
            route_observation: None,
            unresolved_effects: vec![],
        };
        let bytes = serde_json::to_vec(&old).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("route_observation"));
        let decoded: ResultEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        let mut observed = decoded;
        observed.route_observation = Some(RouteObservation {
            model: Some("workbuddy/deepseek-v4.1-flash".to_owned()),
            model_source: ObservationSource::HarnessJsonl,
            effort: None,
            effort_source: ObservationSource::Unavailable,
        });
        assert_eq!(serde_json::to_vec(&observed).unwrap(), bytes);
    }
}
