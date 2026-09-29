//! Durable `SQLite` metadata and content-addressed artifact storage.

mod artifact;
mod attempt;
mod board;
mod contention;
mod delegation;
mod error;
#[cfg(test)]
mod fixtures;
mod message;
mod notification;
mod owner;
mod result;
mod schema;
mod task;
mod tree;

use std::{fmt::Write as _, fs, io::Read, path::Path};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use artifact::ArtifactStore;
pub use board::{BoardStore, BoardTaskRow};
use brgr_protocol::{
    ArtifactRef, AttemptId, AttemptState, Decision, Event, EventId, EventKind, ResultEnvelope,
    ResultId, RouteObservation, SCHEMA_V1, TaskSpec,
};
use contention::BUSY_TIMEOUT;
pub use error::StoreError;
pub use message::{MessageDirection, MessageDraft, MessageKind, TaskMessage};
pub use notification::{NotificationTarget, PendingNotification, QuestionTarget};
use owner::assert_owner_binding;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use schema::initialize_connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub use tree::SubtreeNode;

/// The result of an idempotent store mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    Inserted,
    AlreadyApplied,
}

/// Stable identity of a particular runner incarnation, not merely its PID.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunnerIdentity {
    pub namespace: String,
    pub handle: String,
    pub birth_marker: String,
}

impl RunnerIdentity {
    /// Rejects incomplete identities. The caller must verify the birth marker
    /// against the native process/session before reporting it as alive.
    ///
    /// # Errors
    ///
    /// Returns an error if any identity component is blank.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.namespace.trim().is_empty()
            || self.handle.trim().is_empty()
            || self.birth_marker.trim().is_empty()
        {
            return Err(StoreError::InvalidRunnerIdentity);
        }
        Ok(())
    }
}

/// Durable pre-spawn receipt for one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchIntent {
    pub nonce: String,
    pub supervisor_epoch: u64,
    pub runner_identity: Option<RunnerIdentity>,
}

/// An unfinished attempt discovered after reopening the supervisor store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnfinishedAttempt {
    pub task: TaskSpec,
    pub attempt_id: AttemptId,
    pub state: AttemptState,
    pub launch: Option<LaunchIntent>,
}

/// A durable metadata and artifact store rooted at one private directory.
pub struct Store {
    connection: Connection,
    artifacts: ArtifactStore,
}

impl Store {
    /// Opens or creates a store and applies its idempotent schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory, database, permissions, or schema
    /// cannot be initialized.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref();
        private_directory(root)?;
        let database_path = root.join("brgr.sqlite3");
        let connection = Connection::open(&database_path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        // Deliberately not retried. `Store::open` runs inside the cross-process
        // admission lock, and a caller that waits there keeps every other
        // admission out; see [`retry_busy`] for why that trade is wrong. An
        // initialized store writes nothing here, so there is nothing to wait for.
        initialize_connection(&connection)?;
        private_file(&database_path)?;
        let artifacts = ArtifactStore::open(root)?;
        Ok(Self {
            connection,
            artifacts,
        })
    }

    /// Reads the task spec an attempt belongs to, without taking a write lock.
    ///
    /// Used to size and verify sealed artifacts before the terminal commit opens
    /// its transaction. `tasks.spec_json` is insert-only, so this cannot go stale
    /// in a way the commit would miss.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt is unknown or storage fails.
    fn task_spec_for_attempt(&self, attempt_id: AttemptId) -> Result<String, StoreError> {
        self.connection
            .query_row(
                "SELECT t.spec_json FROM attempts a
                 JOIN tasks t ON t.task_id = a.task_id AND t.revision = a.revision
                 WHERE a.attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(attempt_id))
    }

    /// Begins a write transaction that takes its reserved lock at `BEGIN`.
    ///
    /// A `DEFERRED` transaction that reads before it writes fails with
    /// `SQLITE_BUSY_SNAPSHOT` once another writer commits inside that window,
    /// and `busy_timeout` does not cover that code. `IMMEDIATE` makes the
    /// contention visible at `BEGIN`, where the busy handler applies.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction cannot be started.
    fn write_transaction(&self) -> Result<Transaction<'_>, StoreError> {
        Ok(Transaction::new_unchecked(
            &self.connection,
            TransactionBehavior::Immediate,
        )?)
    }

    /// Reads a sealed artifact and verifies its reference, size, and digest.
    ///
    /// # Errors
    ///
    /// Returns an error for a forged path, symlink, non-file, missing or
    /// modified artifact, or data larger than `max_bytes`.
    pub fn read_artifact(
        &self,
        reference: &ArtifactRef,
        max_bytes: u64,
    ) -> Result<Vec<u8>, StoreError> {
        self.artifacts.read_verified(reference, max_bytes)
    }

    /// Returns the canonical SHA-256 digest used to bind a decision to a result.
    ///
    /// # Errors
    ///
    /// Returns an error if the result cannot be serialized.
    pub fn result_digest(result: &ResultEnvelope) -> Result<String, StoreError> {
        Ok(sha256(serde_json::to_vec(result)?.as_slice()))
    }

    /// Seals a regular file into the content-addressed artifact store.
    ///
    /// # Errors
    ///
    /// Returns an error for non-regular files, invalid bounds, oversized data,
    /// or storage failures.
    pub fn seal_artifact_path(
        &self,
        source: impl AsRef<Path>,
        media_type: &str,
        max_bytes: u64,
    ) -> Result<ArtifactRef, StoreError> {
        self.artifacts
            .seal_path(source.as_ref(), media_type, max_bytes)
    }

    /// Seals bytes from a reader into the content-addressed artifact store.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, oversized data, or storage failures.
    pub fn seal_artifact_reader(
        &self,
        reader: impl Read,
        media_type: &str,
        max_bytes: u64,
    ) -> Result<ArtifactRef, StoreError> {
        self.artifacts.seal_reader(reader, media_type, max_bytes)
    }
}

fn record_decision_in_transaction(
    transaction: &Transaction<'_>,
    artifacts: &ArtifactStore,
    decision: &Decision,
) -> Result<WriteOutcome, StoreError> {
    validate_schema(&decision.schema)?;
    assert_owner_binding(
        transaction,
        &decision.owner_id,
        decision.session_id.as_deref(),
        decision.binding_epoch,
    )?;
    let decision_json = serde_json::to_string(decision)?;
    if let Some(stored_json) = transaction
        .query_row(
            "SELECT decision_json FROM decisions WHERE result_id = ?1",
            [decision.result_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        let stored: Decision = serde_json::from_str(&stored_json)?;
        return if decisions_equal_except_id(&stored, decision) {
            Ok(WriteOutcome::AlreadyApplied)
        } else {
            Err(StoreError::DecisionConflict(decision.result_id))
        };
    }

    let (stored_digest, owner_id, task_id, revision, envelope_json, spec_json) = transaction
        .query_row(
            "SELECT r.result_digest, i.owner_id, r.task_id, r.revision, r.envelope_json, t.spec_json
                 FROM results r JOIN inbox_items i ON i.result_id = r.result_id
                 JOIN tasks t ON t.task_id = r.task_id AND t.revision = r.revision
                 WHERE r.result_id = ?1",
            [decision.result_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::ResultNotFound(decision.result_id))?;
    let result: ResultEnvelope = serde_json::from_str(&envelope_json)?;
    if result.outcome != brgr_protocol::TerminalOutcome::Candidate {
        return Err(StoreError::DecisionRequiresCandidate);
    }
    if result.artifacts.is_empty() {
        return Err(StoreError::DecisionRequiresSealedArtifact);
    }
    if stored_digest != decision.result_digest {
        return Err(StoreError::ResultDigestMismatch);
    }
    if owner_id != decision.owner_id.as_str() {
        return Err(StoreError::DecisionOwnerMismatch);
    }
    if task_id != decision.task_id.to_string() || revision != decision.revision {
        return Err(StoreError::DecisionResultMismatch);
    }
    let spec: TaskSpec = serde_json::from_str(&spec_json)?;
    for reference in &result.artifacts {
        artifacts.read_verified(reference, spec.artifact_contract.max_bytes)?;
    }

    transaction.execute(
        "INSERT INTO decisions (decision_id, result_id, decision_json)
             VALUES (?1, ?2, ?3)",
        params![
            decision.decision_id.to_string(),
            decision.result_id.to_string(),
            decision_json,
        ],
    )?;
    Ok(WriteOutcome::Inserted)
}

fn decisions_equal_except_id(left: &Decision, right: &Decision) -> bool {
    left.schema == right.schema
        && left.owner_id == right.owner_id
        && left.task_id == right.task_id
        && left.revision == right.revision
        && left.result_id == right.result_id
        && left.result_digest == right.result_digest
        && left.session_id == right.session_id
        && left.binding_epoch == right.binding_epoch
        && left.verdict == right.verdict
        && left.reason == right.reason
}

fn record_idempotency(
    transaction: &Transaction<'_>,
    request_id: &str,
    request_digest: &str,
) -> Result<WriteOutcome, StoreError> {
    if let Some(stored_digest) = transaction
        .query_row(
            "SELECT request_digest FROM idempotency_requests WHERE request_id = ?1",
            [request_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return if stored_digest == request_digest {
            Ok(WriteOutcome::AlreadyApplied)
        } else {
            Err(StoreError::IdempotencyConflict(request_id.to_owned()))
        };
    }
    transaction.execute(
        "INSERT INTO idempotency_requests (request_id, request_digest) VALUES (?1, ?2)",
        params![request_id, request_digest],
    )?;
    Ok(WriteOutcome::Inserted)
}

fn read_launch_intent(
    transaction: &Transaction<'_>,
    attempt_id: AttemptId,
) -> Result<Option<LaunchIntent>, StoreError> {
    let row = transaction
        .query_row(
            "SELECT launch_nonce, supervisor_epoch, runner_identity_json
             FROM launch_intents WHERE attempt_id = ?1",
            [attempt_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(nonce, epoch, identity)| {
        Ok(LaunchIntent {
            nonce,
            supervisor_epoch: u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)?,
            runner_identity: identity
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
        })
    })
    .transpose()
}

fn insert_terminal_event(
    transaction: &Transaction<'_>,
    result: &ResultEnvelope,
) -> Result<(), StoreError> {
    let terminal_event = Event {
        schema: SCHEMA_V1.to_owned(),
        event_id: EventId::new(),
        attempt_id: result.attempt_id,
        producer: "brgr.terminal".to_owned(),
        producer_seq: 1,
        kind: EventKind::Terminal,
        payload: serde_json::json!({"result_id": result.result_id}),
    };
    transaction.execute(
        "INSERT INTO events (event_id, attempt_id, producer, producer_seq, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            terminal_event.event_id.to_string(),
            terminal_event.attempt_id.to_string(),
            terminal_event.producer,
            1_i64,
            serde_json::to_string(&terminal_event)?,
        ],
    )?;
    Ok(())
}

fn insert_run_completion(
    transaction: &Transaction<'_>,
    result: &ResultEnvelope,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO task_run_completions
         (task_id, revision, result_id, completed_at)
         VALUES (?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER))",
        params![
            result.task_id.to_string(),
            result.revision,
            result.result_id.to_string(),
        ],
    )?;
    let recorded: String = transaction.query_row(
        "SELECT result_id FROM task_run_completions WHERE task_id = ?1 AND revision = ?2",
        params![result.task_id.to_string(), result.revision],
        |row| row.get(0),
    )?;
    if recorded != result.result_id.to_string() {
        return Err(StoreError::InvalidNotification);
    }
    Ok(())
}

fn validate_recovery_observation(
    transaction: &Transaction<'_>,
    observed: &UnfinishedAttempt,
) -> Result<(), StoreError> {
    let current = transaction
        .query_row(
            "SELECT state FROM attempts WHERE attempt_id = ?1",
            [observed.attempt_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(StoreError::AttemptNotFound(observed.attempt_id))?;
    if parse_state(&current)? != observed.state
        || read_launch_intent(transaction, observed.attempt_id)? != observed.launch
    {
        return Err(StoreError::RecoveryObservationStale(observed.attempt_id));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), StoreError> {
    if digest.trim().is_empty() {
        return Err(StoreError::InvalidRequestDigest);
    }
    Ok(())
}

fn validate_schema(schema: &str) -> Result<(), StoreError> {
    if schema != SCHEMA_V1 {
        return Err(StoreError::Protocol(
            brgr_protocol::ProtocolError::UnsupportedSchema(schema.to_owned()),
        ));
    }
    Ok(())
}

fn serialize_route_observation(
    observation: Option<&RouteObservation>,
) -> Result<Option<(String, String)>, StoreError> {
    observation
        .map(|value| {
            let json = serde_json::to_string(value)?;
            Ok((sha256(json.as_bytes()), json))
        })
        .transpose()
}

fn verify_replayed_route_observation(
    transaction: &Transaction<'_>,
    result_id: ResultId,
    observation: Option<&(String, String)>,
) -> Result<(), StoreError> {
    if let Some((expected, _)) = observation {
        let stored: Option<String> = transaction
            .query_row(
                "SELECT observation_digest FROM route_observations WHERE result_id = ?1",
                [result_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if stored.as_ref() != Some(expected) {
            return Err(StoreError::RouteObservationConflict(result_id));
        }
    }
    Ok(())
}

fn insert_route_observation(
    transaction: &Transaction<'_>,
    result_id: ResultId,
    observation: Option<&(String, String)>,
) -> Result<(), StoreError> {
    if let Some((digest, json)) = observation {
        transaction.execute(
            "INSERT INTO route_observations (result_id, observation_digest, observation_json)
             VALUES (?1, ?2, ?3)",
            params![result_id.to_string(), digest, json],
        )?;
    }
    Ok(())
}

fn validate_terminal_result(result: &ResultEnvelope) -> Result<(), StoreError> {
    validate_schema(&result.schema)?;
    if result.outcome == brgr_protocol::TerminalOutcome::Candidate && result.artifacts.is_empty() {
        return Err(StoreError::CandidateRequiresArtifact);
    }
    Ok(())
}

fn terminal_attempt(
    transaction: &Transaction<'_>,
    attempt_id: AttemptId,
) -> Result<(String, u32, String, String, String), StoreError> {
    transaction
        .query_row(
            "SELECT a.task_id, a.revision, t.owner_id, a.state, t.spec_json
             FROM attempts a
             JOIN tasks t ON t.task_id = a.task_id AND t.revision = a.revision
             WHERE a.attempt_id = ?1",
            [attempt_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::AttemptNotFound(attempt_id))
}

/// What a terminal commit computes once before contending for the lock.
struct PreparedTerminal {
    envelope_json: String,
    digest: String,
    observation: Option<(String, String)>,
    verified_spec: String,
}

#[cfg(test)]
thread_local! {
    static COMMIT_TRIES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static VERIFICATIONS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn verify_candidate_artifacts(
    artifacts: &ArtifactStore,
    result: &ResultEnvelope,
    task_json: &str,
) -> Result<(), StoreError> {
    #[cfg(test)]
    VERIFICATIONS.with(|count| count.set(count.get() + 1));
    if result.outcome == brgr_protocol::TerminalOutcome::Candidate {
        let task: TaskSpec = serde_json::from_str(task_json)?;
        for reference in &result.artifacts {
            artifacts.read_verified(reference, task.artifact_contract.max_bytes)?;
        }
    }
    Ok(())
}

fn state_name(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Queued => "queued",
        AttemptState::Starting => "starting",
        AttemptState::Running => "running",
        AttemptState::Blocked => "blocked",
        AttemptState::Collecting => "collecting",
        AttemptState::CancelRequested => "cancel_requested",
        AttemptState::Terminal => "terminal",
    }
}

fn allowed_attempt_transition(from: AttemptState, to: AttemptState) -> bool {
    match from {
        AttemptState::Queued => {
            matches!(to, AttemptState::Starting | AttemptState::CancelRequested)
        }
        AttemptState::Starting => {
            matches!(to, AttemptState::Running | AttemptState::CancelRequested)
        }
        AttemptState::Running | AttemptState::Blocked => {
            matches!(
                to,
                AttemptState::Running
                    | AttemptState::Blocked
                    | AttemptState::Collecting
                    | AttemptState::CancelRequested
            ) && to != from
        }
        AttemptState::Collecting => to == AttemptState::CancelRequested,
        AttemptState::CancelRequested | AttemptState::Terminal => false,
    }
}

fn parse_state(value: &str) -> Result<AttemptState, StoreError> {
    match value {
        "queued" => Ok(AttemptState::Queued),
        "starting" => Ok(AttemptState::Starting),
        "running" => Ok(AttemptState::Running),
        "blocked" => Ok(AttemptState::Blocked),
        "collecting" => Ok(AttemptState::Collecting),
        "cancel_requested" => Ok(AttemptState::CancelRequested),
        "terminal" => Ok(AttemptState::Terminal),
        other => Err(StoreError::InvalidAttemptState(other.to_owned())),
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format_sha256(digest.as_ref())
}

fn format_sha256(digest: &[u8]) -> String {
    let mut encoded = String::with_capacity("sha256:".len() + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn private_directory(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Returns the exclusive upper bound of a lowercase-hexadecimal prefix range.
///
/// `g` sorts above every hex digit under the default `BINARY` collation, so
/// `prefix || "g"` is greater than every id starting with `prefix` and less than
/// every id that starts with a higher prefix. This needs no digit carry.
fn prefix_upper_bound(prefix: &str) -> String {
    format!("{prefix}g")
}

fn private_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests;
