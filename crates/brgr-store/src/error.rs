//! Typed store failures.

use std::path::PathBuf;

use brgr_protocol::{AttemptId, AttemptState, OwnerId, ResultId, TaskId};

/// Failures surfaced by durable metadata and artifact operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol validation failed: {0}")]
    Protocol(#[from] brgr_protocol::ProtocolError),
    #[error("stored protocol data is invalid: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("idempotency request {0} was replayed with a different digest")]
    IdempotencyConflict(String),
    #[error("request digest must not be empty")]
    InvalidRequestDigest,
    #[error("delegation parent attempt is stale, mismatched, or owned by another worker")]
    InvalidDelegationParent,
    #[error("delegation depth exceeds eight worker edges")]
    DelegationDepthExceeded,
    #[error("parent task has a cancellation request")]
    DelegationParentCancelled,
    #[error("parent task reached its concurrent child limit")]
    ConcurrentChildLimit,
    #[error("task subtree exceeds the supported size limit")]
    SubtreeTooLarge,
    #[error("Codex owner surface is invalid")]
    InvalidOwnerSurface,
    #[error("completion notification data is invalid")]
    InvalidNotification,
    #[error("completion notification claim or owner binding is stale")]
    NotificationClaimStale,
    #[error("task message is invalid or exceeds its size limit")]
    InvalidTaskMessage,
    #[error("task message {0} does not exist for this recipient")]
    TaskMessageNotFound(String),
    #[error("task message {0} conflicts with an earlier request")]
    TaskMessageConflict(String),
    #[error("attempt {0} does not exist")]
    AttemptNotFound(AttemptId),
    #[error("stored attempt ID is invalid: {0}")]
    InvalidAttemptId(String),
    #[error("launch nonce and supervisor epoch must be non-empty and positive")]
    InvalidLaunchIntent,
    #[error("attempt {0} has no durable launch intent")]
    LaunchIntentNotFound(AttemptId),
    #[error("attempt {0} already has a different launch intent")]
    LaunchIntentConflict(AttemptId),
    #[error("runner identity requires namespace, handle, and birth marker")]
    InvalidRunnerIdentity,
    #[error("attempt {0} already has a different runner incarnation")]
    RunnerIdentityConflict(AttemptId),
    #[error("recovery observation for attempt {0} is stale")]
    RecoveryObservationStale(AttemptId),
    #[error("recovery can publish only a lost result")]
    RecoveryRequiresLost,
    #[error("task {task_id} revision {revision} already has an active attempt")]
    ActiveAttemptExists { task_id: TaskId, revision: u32 },
    #[error("attempt state changed: expected {expected:?}, found {actual:?}")]
    AttemptStateConflict {
        expected: AttemptState,
        actual: AttemptState,
    },
    #[error("invalid attempt transition from {from:?} to {to:?}")]
    AttemptTransitionInvalid {
        from: AttemptState,
        to: AttemptState,
    },
    #[error("task {0} does not exist")]
    TaskNotFound(TaskId),
    #[error("task {task_id} revision {revision} exhausted its attempt budget")]
    AttemptBudgetExhausted { task_id: TaskId, revision: u32 },
    #[error("task list limit is invalid")]
    InvalidTaskLimit,
    #[error("task id prefix must be nonempty hexadecimal")]
    InvalidTaskPrefix,
    #[error("numeric value exceeds SQLite integer range")]
    NumericOverflow,
    #[error("stored attempt state is invalid: {0}")]
    InvalidAttemptState(String),
    #[error("terminal result does not belong to its attempt")]
    AttemptResultMismatch,
    #[error("terminal result must be delivered to the task owner")]
    ResultOwnerMismatch,
    #[error("attempt {0} already has a different terminal result")]
    TerminalResultConflict(AttemptId),
    #[error("result {0} has conflicting native route observation")]
    RouteObservationConflict(ResultId),
    #[error("stored native route observation has a digest mismatch")]
    RouteObservationIntegrityMismatch,
    #[error("task {task_id} revision {revision} has an unresolved lost attempt")]
    UnresolvedPriorAttempt { task_id: TaskId, revision: u32 },
    #[error("task spec changed while its terminal result was being committed")]
    TaskSpecChangedDuringCommit,
    #[error("task {task_id} revision {revision} already has a non-retryable terminal result")]
    NonRetryablePriorAttempt { task_id: TaskId, revision: u32 },
    #[error("attempt {0} cannot receive a pre-spawn retry grant without a failed result")]
    RetryGrantRequiresFailedAttempt(AttemptId),
    #[error("candidate terminal result requires a sealed artifact")]
    CandidateRequiresArtifact,
    #[error("result {0} does not exist")]
    ResultNotFound(ResultId),
    #[error("result digest does not match the sealed result")]
    ResultDigestMismatch,
    #[error("only the result inbox owner may decide")]
    DecisionOwnerMismatch,
    #[error("decision does not belong to its result task revision")]
    DecisionResultMismatch,
    #[error("only a candidate result may be accepted or rejected")]
    DecisionRequiresCandidate,
    #[error("candidate decision requires at least one sealed artifact")]
    DecisionRequiresSealedArtifact,
    #[error("result {0} already has a different decision")]
    DecisionConflict(ResultId),
    #[error("the owner inbox item does not exist")]
    InboxItemNotFound,
    #[error("event producer and sequence must be non-empty and positive")]
    InvalidEvent,
    #[error("event identity or producer sequence conflicts with stored data")]
    EventConflict,
    #[error("owner binding session and epoch are invalid")]
    InvalidOwnerBinding,
    #[error("owner binding belongs to a different session or stale epoch")]
    OwnerBindingConflict,
    #[error("owner {0} has no bound Codex session")]
    OwnerUnbound(OwnerId),
    #[error("artifact limit must be positive")]
    InvalidArtifactLimit,
    #[error("artifact media type must not be empty")]
    InvalidMediaType,
    #[error("artifact input is not a regular file: {path:?}")]
    ArtifactNotRegularFile { path: PathBuf },
    #[error("artifact exceeds {max_bytes} bytes (observed at least {observed_bytes})")]
    ArtifactTooLarge { max_bytes: u64, observed_bytes: u64 },
    #[error("artifact size overflowed u64")]
    ArtifactSizeOverflow,
    #[error("content-addressed artifact collision for {0}")]
    ArtifactDigestCollision(String),
    #[error("artifact path escaped the store root")]
    ArtifactPathOutsideStore,
    #[error("artifact reference does not name its content-addressed path")]
    InvalidArtifactReference,
    #[error("artifact digest or size does not match its sealed reference")]
    ArtifactIntegrityMismatch,
}

impl StoreError {
    #[must_use]
    pub fn is_retryable_database_contention(&self) -> bool {
        matches!(
            self,
            Self::Database(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}
