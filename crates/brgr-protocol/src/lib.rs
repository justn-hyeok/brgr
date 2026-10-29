//! Versioned protocol types shared by every brgr component.
//!
//! These are the wire types: what a plugin sends brgr and what brgr writes back.
//! Every one of them carries an explicit `schema`, and [`TaskSpec::validate`]
//! refuses anything that is not [`SCHEMA_V1`], so a component built against a
//! different version fails at the boundary instead of part way through a run.
//!
//! A task as it arrives on the wire:
//!
//! ```
//! use brgr_protocol::{SCHEMA_V1, TaskSpec};
//!
//! let wire = r#"{
//!   "schema": "brgr/v1",
//!   "task_id": "3d3c9081-0f4a-4f2e-9c1b-7a2d5e6f8a90",
//!   "revision": 1,
//!   "create_request_id": "req-1",
//!   "owner_id": "codex:alice",
//!   "objective": "Summarize the build log",
//!   "workspace": "/srv/checkout",
//!   "route": { "harness_id": "local.fixture" },
//!   "required_capabilities": ["completion"],
//!   "artifact_contract": { "media_type": "text/plain", "max_bytes": 4096 },
//!   "acceptance_criteria": ["the report is sealed"],
//!   "budget": { "deadline_seconds": 60, "max_attempts": 2 }
//! }"#;
//!
//! let task: TaskSpec = serde_json::from_str(wire)?;
//! task.validate()?;
//! assert_eq!(task.schema, SCHEMA_V1);
//! assert_eq!(task.revision, 1);
//! // Compared as values, not bytes: the input above is pretty-printed and the
//! // re-encoding is compact, so the two texts differ while the tasks do not.
//! let reencoded: TaskSpec = serde_json::from_slice(&serde_json::to_vec(&task)?)?;
//! assert_eq!(reencoded, task);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A schema brgr does not know is refused rather than guessed at:
//!
//! ```
//! # use brgr_protocol::TaskSpec;
//! # let wire = r#"{
//! #   "schema": "brgr/v2",
//! #   "task_id": "3d3c9081-0f4a-4f2e-9c1b-7a2d5e6f8a90",
//! #   "revision": 1,
//! #   "create_request_id": "req-1",
//! #   "owner_id": "codex:alice",
//! #   "objective": "Summarize the build log",
//! #   "workspace": "/srv/checkout",
//! #   "route": { "harness_id": "local.fixture" },
//! #   "required_capabilities": ["completion"],
//! #   "artifact_contract": { "media_type": "text/plain", "max_bytes": 4096 },
//! #   "acceptance_criteria": ["the report is sealed"],
//! #   "budget": { "deadline_seconds": 60, "max_attempts": 2 }
//! # }"#;
//! let task: TaskSpec = serde_json::from_str(wire)?;
//! assert!(task.validate().is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::{
    fmt,
    path::{Component, Path},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SCHEMA_V1: &str = "brgr/v1";

/// Maximum UTF-8 byte length of a task objective at admission.
///
/// JSON Schema `maxLength` on `objective` is 262144 characters, not bytes.
/// [`TaskSpec::validate`] is the authority and counts bytes.
pub const MAX_OBJECTIVE_BYTES: usize = 262_144;
/// Maximum number of acceptance criteria on a newly admitted task.
pub const MAX_ACCEPTANCE_CRITERIA: usize = 32;
/// Maximum UTF-8 byte length of a single acceptance criterion.
pub const MAX_CRITERION_BYTES: usize = 4_096;
/// Maximum combined UTF-8 byte length of all acceptance criteria.
pub const MAX_CRITERIA_TOTAL_BYTES: usize = 65_536;

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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskInstructions {
    pub scope: Vec<String>,
    pub role: Vec<String>,
    pub forward_criteria: bool,
}

impl TaskInstructions {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scope.is_empty() && self.role.is_empty() && !self.forward_criteria
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvidenceSpec {
    pub capture_diff: bool,
    pub capture_logs: bool,
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_tree: Option<String>,
}

impl EvidenceSpec {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.capture_diff
            && !self.capture_logs
            && self.files.is_empty()
            && self.base_commit.is_none()
            && self.base_tree.is_none()
    }
}

/// How much a worker may do without asking. Ordered: each level allows a
/// subset of the next, so a cap is a comparison.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    /// Read and propose; no file changes.
    ReadOnly,
    /// Edit files in the task workspace; commands still need approval.
    Edits,
    /// Every tool auto-approved.
    Full,
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
    #[serde(default, skip_serializing_if = "TaskInstructions::is_empty")]
    pub instructions: TaskInstructions,
    #[serde(default, skip_serializing_if = "EvidenceSpec::is_empty")]
    pub evidence: EvidenceSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_children: Option<u8>,
    /// `None` runs the harness as its recipe does by default, which is also how
    /// every task admitted before permission levels existed runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionLevel>,
}

impl TaskSpec {
    /// Validates the bounded v1 task contract for a **new** admission.
    ///
    /// Includes [`MAX_OBJECTIVE_BYTES`] and the acceptance-criteria size
    /// limits. Loading a stored task deserializes without calling this;
    /// wrapping a stored spec uses [`Self::validate_stored`].
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] when a required field or execution bound is
    /// invalid.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.validate_stored()?;
        if self.objective.len() > MAX_OBJECTIVE_BYTES {
            return Err(ProtocolError::ObjectiveTooLong);
        }
        if acceptance_criteria_exceed_size(&self.acceptance_criteria) {
            return Err(ProtocolError::AcceptanceCriteriaTooLarge);
        }
        Ok(())
    }

    /// Shape checks that stored v1 tasks still satisfy, including those
    /// admitted before objective and criteria size limits existed.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] when a required field or execution bound is
    /// invalid.
    pub fn validate_stored(&self) -> Result<(), ProtocolError> {
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
        if self.instructions.scope.len() > 16
            || self.instructions.role.len() > 16
            || self
                .instructions
                .scope
                .iter()
                .chain(&self.instructions.role)
                .any(|entry| entry.trim().is_empty() || entry.len() > 2_048)
        {
            return Err(ProtocolError::InvalidTaskInstructions);
        }
        if self.evidence.base_commit.as_ref().is_some_and(|commit| {
            !self.evidence.capture_diff
                || !matches!(commit.len(), 40 | 64)
                || !commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) || self.evidence.base_tree.as_ref().is_some_and(|tree| {
            !self.evidence.capture_diff
                || !matches!(tree.len(), 40 | 64)
                || !tree
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) || self.evidence.files.len() > 8
            || self.evidence.files.iter().any(|path| {
                path.is_empty()
                    || path.len() > 1_024
                    || Path::new(path).components().any(
                        |component| !matches!(component, Component::Normal(name) if name != ".git"),
                    )
            })
        {
            return Err(ProtocolError::InvalidEvidenceSpec);
        }
        if self
            .max_concurrent_children
            .is_some_and(|limit| limit == 0 || limit > 8)
        {
            return Err(ProtocolError::InvalidChildLimit);
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

fn acceptance_criteria_exceed_size(criteria: &[String]) -> bool {
    let total = criteria
        .iter()
        .fold(0usize, |sum, criterion| sum.saturating_add(criterion.len()));
    criteria.len() > MAX_ACCEPTANCE_CRITERIA
        || criteria
            .iter()
            .any(|criterion| criterion.len() > MAX_CRITERION_BYTES)
        || total > MAX_CRITERIA_TOTAL_BYTES
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub digest: String,
    pub bytes: u64,
    pub media_type: String,
    pub store_relative_path: String,
}

/// Where a recorded model or effort came from.
///
/// A value this build does not know reads as `Unavailable`:
/// a store written by a newer brgr stays readable, and says only that the source
/// cannot be vouched for. (A release before this one rejected a source it did
/// not know, so a store with `harness_config` rows cannot be read by v2.13.4
/// or earlier.)
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    HarnessJsonl,
    /// Read from the harness's own settings, not from the run: the model a
    /// configured-default-only harness was set to use.
    HarnessConfig,
    Unavailable,
}

impl<'de> Deserialize<'de> for ObservationSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "harness_jsonl" => Self::HarnessJsonl,
            "harness_config" => Self::HarnessConfig,
            _ => Self::Unavailable,
        })
    }
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
    /// Preserves bytes written by an unreleased intermediate v1 build.
    /// New results never populate this field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "route_observation"
    )]
    pub legacy_embedded_route_observation: Option<RouteObservation>,
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
    #[error("objective must be at most {} bytes", MAX_OBJECTIVE_BYTES)]
    ObjectiveTooLong,
    #[error(
        "acceptance criteria must be at most {} entries, each at most {} bytes, totaling at most {} bytes",
        MAX_ACCEPTANCE_CRITERIA,
        MAX_CRITERION_BYTES,
        MAX_CRITERIA_TOTAL_BYTES
    )]
    AcceptanceCriteriaTooLarge,
    #[error("workspace must not be empty")]
    EmptyWorkspace,
    #[error("harness id must not be empty")]
    EmptyHarness,
    #[error("requested model and effort must not be blank")]
    EmptyRouteSelector,
    #[error("at least one nonempty acceptance criterion is required")]
    InvalidAcceptanceCriteria,
    #[error("scope and role instructions must be bounded and nonempty")]
    InvalidTaskInstructions,
    #[error("requested evidence paths must be bounded and nonempty")]
    InvalidEvidenceSpec,
    #[error("concurrent child limit must be between 1 and 8")]
    InvalidChildLimit,
    #[error("artifact and deadline limits must be positive")]
    InvalidBudget,
    #[error("v1 permits one initial attempt and at most one retry")]
    InvalidAttemptCount,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> TaskSpec {
        TaskSpec {
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
                max_attempts: 2,
            },
            instructions: TaskInstructions::default(),
            evidence: EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
        }
    }

    #[test]
    fn validate_stored_accepts_an_over_limit_objective_that_admission_refuses() {
        let mut task = sample_task();
        task.objective = "a".repeat(MAX_OBJECTIVE_BYTES + 1);
        task.validate_stored().unwrap();
        assert_eq!(task.validate(), Err(ProtocolError::ObjectiveTooLong));
        task.objective = "ok".to_owned();
        task.acceptance_criteria = vec!["y".repeat(MAX_CRITERION_BYTES + 1)];
        task.validate_stored().unwrap();
        assert_eq!(
            task.validate(),
            Err(ProtocolError::AcceptanceCriteriaTooLarge)
        );
    }

    #[test]
    fn a_256_kib_objective_is_accepted() {
        let mut task = sample_task();
        task.objective = "a".repeat(MAX_OBJECTIVE_BYTES);
        assert_eq!(task.objective.len(), MAX_OBJECTIVE_BYTES);
        task.validate().unwrap();
    }

    #[test]
    fn a_256_kib_plus_one_objective_is_refused_without_quoting_it() {
        let mut task = sample_task();
        task.objective = "a".repeat(MAX_OBJECTIVE_BYTES + 1);
        let error = task
            .validate()
            .expect_err("over-long objective was accepted");
        assert_eq!(error, ProtocolError::ObjectiveTooLong);
        let shown = error.to_string();
        assert!(
            shown.contains(&MAX_OBJECTIVE_BYTES.to_string()),
            "error must name the limit: {shown}"
        );
        assert!(
            !shown.contains(&task.objective),
            "error quoted the objective"
        );
        assert!(
            !shown.contains("aaa"),
            "error leaked objective text: {shown}"
        );
    }

    #[test]
    fn objective_limit_counts_utf8_bytes_not_characters() {
        let mut task = sample_task();
        // 262143 ASCII bytes + one 2-byte character: 262144 characters, 262145 bytes.
        // JSON Schema maxLength would accept this; the byte check is the authority.
        let mut objective = "a".repeat(MAX_OBJECTIVE_BYTES - 1);
        objective.push('é');
        assert_eq!(objective.len(), MAX_OBJECTIVE_BYTES + 1);
        assert_eq!(objective.chars().count(), MAX_OBJECTIVE_BYTES);
        task.objective = objective.clone();
        let error = task
            .validate()
            .expect_err("multibyte over-long objective was accepted");
        assert_eq!(error, ProtocolError::ObjectiveTooLong);
        assert!(!error.to_string().contains(&objective));

        task.objective = "가".repeat(MAX_OBJECTIVE_BYTES / 3);
        assert_eq!(task.objective.len(), (MAX_OBJECTIVE_BYTES / 3) * 3);
        task.validate().unwrap();
    }

    #[test]
    fn acceptance_criteria_are_bounded_by_count_and_bytes() {
        let mut task = sample_task();
        task.acceptance_criteria = vec!["x".repeat(MAX_CRITERION_BYTES)];
        task.validate().unwrap();
        task.acceptance_criteria = vec!["x".repeat(MAX_CRITERION_BYTES + 1)];
        assert_eq!(
            task.validate(),
            Err(ProtocolError::AcceptanceCriteriaTooLarge)
        );

        task.acceptance_criteria = vec!["c".to_owned(); MAX_ACCEPTANCE_CRITERIA];
        task.validate().unwrap();
        task.acceptance_criteria = vec!["c".to_owned(); MAX_ACCEPTANCE_CRITERIA + 1];
        assert_eq!(
            task.validate(),
            Err(ProtocolError::AcceptanceCriteriaTooLarge)
        );

        let piece = "y".repeat(MAX_CRITERION_BYTES);
        let n = MAX_CRITERIA_TOTAL_BYTES / MAX_CRITERION_BYTES;
        task.acceptance_criteria = vec![piece.clone(); n];
        assert_eq!(
            task.acceptance_criteria
                .iter()
                .map(String::len)
                .sum::<usize>(),
            n * MAX_CRITERION_BYTES
        );
        task.validate().unwrap();
        task.acceptance_criteria.push("z".to_owned());
        assert_eq!(
            task.validate(),
            Err(ProtocolError::AcceptanceCriteriaTooLarge)
        );
        let shown = ProtocolError::AcceptanceCriteriaTooLarge.to_string();
        assert!(
            shown.contains(&MAX_ACCEPTANCE_CRITERIA.to_string()),
            "{shown}"
        );
        assert!(shown.contains(&MAX_CRITERION_BYTES.to_string()), "{shown}");
        assert!(
            shown.contains(&MAX_CRITERIA_TOTAL_BYTES.to_string()),
            "{shown}"
        );
        assert!(!shown.contains(&piece));
    }

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
            instructions: TaskInstructions::default(),
            evidence: EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
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
            instructions: TaskInstructions::default(),
            evidence: EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
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
    fn an_observation_source_this_build_does_not_know_reads_as_unavailable() {
        for (wire, expected) in [
            ("\"harness_jsonl\"", ObservationSource::HarnessJsonl),
            ("\"harness_config\"", ObservationSource::HarnessConfig),
            ("\"unavailable\"", ObservationSource::Unavailable),
            ("\"a_source_added_later\"", ObservationSource::Unavailable),
        ] {
            let read: ObservationSource = serde_json::from_str(wire).unwrap();
            assert_eq!(read, expected, "{wire}");
        }
        // What is written is unchanged.
        assert_eq!(
            serde_json::to_string(&ObservationSource::HarnessConfig).unwrap(),
            "\"harness_config\""
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
            legacy_embedded_route_observation: None,
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
        observed.legacy_embedded_route_observation = observed.route_observation.clone();
        let intermediate_bytes = serde_json::to_vec(&observed).unwrap();
        let intermediate: ResultEnvelope = serde_json::from_slice(&intermediate_bytes).unwrap();
        assert_eq!(
            serde_json::to_vec(&intermediate).unwrap(),
            intermediate_bytes
        );
    }
}
