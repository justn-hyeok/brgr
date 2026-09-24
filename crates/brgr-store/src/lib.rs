//! Durable `SQLite` metadata and content-addressed artifact storage.

mod artifact;
mod board;
mod message;

use std::{
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use artifact::ArtifactStore;
pub use board::{BoardStore, BoardTaskRow};
use brgr_protocol::{
    ArtifactRef, AttemptId, AttemptState, Decision, Event, EventId, EventKind, InboxItem, OwnerId,
    ResultEnvelope, ResultId, RouteObservation, SCHEMA_V1, TaskId, TaskSpec,
};
pub use message::{MessageDirection, MessageDraft, MessageKind, TaskMessage};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = r"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    owner_id TEXT NOT NULL,
    create_request_id TEXT NOT NULL UNIQUE,
    request_digest TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    PRIMARY KEY (task_id, revision)
);
CREATE TABLE IF NOT EXISTS attempts (
    attempt_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    state TEXT NOT NULL,
    FOREIGN KEY (task_id, revision) REFERENCES tasks(task_id, revision)
);
CREATE TABLE IF NOT EXISTS launch_intents (
    attempt_id TEXT PRIMARY KEY,
    launch_nonce TEXT NOT NULL UNIQUE,
    supervisor_epoch INTEGER NOT NULL CHECK (supervisor_epoch > 0),
    runner_identity_json TEXT,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS one_active_attempt_per_revision
ON attempts (task_id, revision) WHERE state <> 'terminal';
CREATE TABLE IF NOT EXISTS results (
    result_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    result_digest TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id),
    FOREIGN KEY (task_id, revision) REFERENCES tasks(task_id, revision)
);
CREATE TABLE IF NOT EXISTS route_observations (
    result_id TEXT PRIMARY KEY,
    observation_digest TEXT NOT NULL,
    observation_json TEXT NOT NULL,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS pre_spawn_retry_grants (
    attempt_id TEXT PRIMARY KEY,
    FOREIGN KEY (attempt_id) REFERENCES results(attempt_id)
);
CREATE TABLE IF NOT EXISTS inbox_items (
    owner_id TEXT NOT NULL,
    result_id TEXT NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
    PRIMARY KEY (owner_id, result_id),
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS decisions (
    decision_id TEXT PRIMARY KEY,
    result_id TEXT NOT NULL UNIQUE,
    decision_json TEXT NOT NULL,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS idempotency_requests (
    request_id TEXT PRIMARY KEY,
    request_digest TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    event_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    producer TEXT NOT NULL,
    producer_seq INTEGER NOT NULL,
    event_json TEXT NOT NULL,
    UNIQUE (attempt_id, producer, producer_seq),
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
);
CREATE TABLE IF NOT EXISTS owner_bindings (
    owner_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    binding_epoch INTEGER NOT NULL CHECK (binding_epoch > 0)
);
CREATE TABLE IF NOT EXISTS delegation_edges (
    child_task_id TEXT PRIMARY KEY,
    parent_task_id TEXT NOT NULL,
    parent_attempt_id TEXT NOT NULL,
    depth INTEGER NOT NULL CHECK (depth BETWEEN 1 AND 8),
    FOREIGN KEY (parent_attempt_id) REFERENCES attempts(attempt_id)
);
CREATE TABLE IF NOT EXISTS task_messages (
    message_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('owner_to_worker', 'worker_to_owner')),
    kind TEXT NOT NULL CHECK (kind IN ('question', 'reply', 'note')),
    body TEXT NOT NULL,
    in_reply_to TEXT,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id),
    FOREIGN KEY (in_reply_to) REFERENCES task_messages(message_id)
);
CREATE INDEX IF NOT EXISTS task_messages_inbox
ON task_messages (task_id, attempt_id, direction, acknowledged);
CREATE UNIQUE INDEX IF NOT EXISTS task_message_one_reply
ON task_messages (in_reply_to) WHERE kind = 'reply';
";

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
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(SCHEMA)?;
        private_file(&database_path)?;
        let artifacts = ArtifactStore::open(root)?;
        Ok(Self {
            connection,
            artifacts,
        })
    }

    /// Records a task revision, enforcing its create-request digest.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid task, serialization failure, conflicting
    /// idempotency digest, or database failure.
    pub fn record_task(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(task, request_digest, None)
    }

    /// Records a child task only while its exact parent attempt is active.
    /// The parent attempt, not a mutable pane or task name, owns the edge.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale parent, wrong child owner, excessive depth,
    /// conflicting replay, or the same errors as [`Self::record_task`].
    pub fn record_child_task(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        parent_task_id: TaskId,
        parent_attempt_id: AttemptId,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(
            task,
            request_digest,
            Some((parent_task_id, parent_attempt_id)),
        )
    }

    /// Checks a proposed parent before creating any child worktree. The
    /// transactional check in `record_child_task` remains authoritative.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale parent, wrong owner, excessive depth, or
    /// a database failure.
    pub fn validate_delegation_parent(
        &self,
        parent_task_id: TaskId,
        parent_attempt_id: AttemptId,
        child_owner: &brgr_protocol::OwnerId,
    ) -> Result<(), StoreError> {
        let parent: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT task_id, state FROM attempts WHERE attempt_id = ?1",
                [parent_attempt_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if !parent.is_some_and(|(id, state)| {
            id == parent_task_id.to_string() && matches!(state.as_str(), "running" | "blocked")
        }) || child_owner.as_str() != format!("worker:{parent_attempt_id}")
        {
            return Err(StoreError::InvalidDelegationParent);
        }
        let depth: u32 = self
            .connection
            .query_row(
                "SELECT depth FROM delegation_edges WHERE child_task_id = ?1",
                [parent_task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if depth >= 8 {
            return Err(StoreError::DelegationDepthExceeded);
        }
        Ok(())
    }

    fn record_task_with_parent(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        parent: Option<(TaskId, AttemptId)>,
    ) -> Result<WriteOutcome, StoreError> {
        task.validate()?;
        validate_digest(request_digest)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let idempotency =
            record_idempotency(&transaction, &task.create_request_id, request_digest)?;
        let recorded_parent = recorded_delegation_parent(&transaction, task.task_id)?;
        let requested_parent =
            parent.map(|(task_id, attempt_id)| (task_id.to_string(), attempt_id.to_string()));
        if idempotency == WriteOutcome::AlreadyApplied {
            let matches = transaction
                .query_row(
                    "SELECT task_id, revision FROM tasks WHERE create_request_id = ?1",
                    [&task.create_request_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )
                .optional()?
                .is_some_and(|(task_id, revision)| {
                    task_id == task.task_id.to_string() && revision == task.revision
                });
            if !matches {
                return Err(StoreError::IdempotencyConflict(
                    task.create_request_id.clone(),
                ));
            }
            if requested_parent.is_some() && recorded_parent != requested_parent {
                return Err(StoreError::InvalidDelegationParent);
            }
            transaction.commit()?;
            return Ok(WriteOutcome::AlreadyApplied);
        }

        validate_new_task_parent(
            &transaction,
            task.task_id,
            recorded_parent.as_ref(),
            requested_parent.as_ref(),
        )?;

        let depth = if let Some((parent_task_id, parent_attempt_id)) = parent {
            let parent_attempt: Option<(String, String)> = transaction
                .query_row(
                    "SELECT task_id, state FROM attempts WHERE attempt_id = ?1",
                    [parent_attempt_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if !parent_attempt.is_some_and(|(id, state)| {
                id == parent_task_id.to_string() && matches!(state.as_str(), "running" | "blocked")
            }) || task.owner_id.as_str() != format!("worker:{parent_attempt_id}")
            {
                return Err(StoreError::InvalidDelegationParent);
            }
            let parent_depth: u32 = transaction
                .query_row(
                    "SELECT depth FROM delegation_edges WHERE child_task_id = ?1",
                    [parent_task_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if parent_depth >= 8 {
                return Err(StoreError::DelegationDepthExceeded);
            }
            Some(parent_depth + 1)
        } else {
            None
        };

        let spec_json = serde_json::to_string(task)?;
        transaction.execute(
            "INSERT INTO tasks
             (task_id, revision, owner_id, create_request_id, request_digest, spec_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task.task_id.to_string(),
                task.revision,
                task.owner_id.as_str(),
                task.create_request_id,
                request_digest,
                spec_json,
            ],
        )?;
        if let (Some((parent_task_id, parent_attempt_id)), Some(depth), None) =
            (parent, depth, recorded_parent)
        {
            transaction.execute(
                "INSERT INTO delegation_edges (child_task_id, parent_task_id, parent_attempt_id, depth) VALUES (?1, ?2, ?3, ?4)",
                params![task.task_id.to_string(), parent_task_id.to_string(), parent_attempt_id.to_string(), depth],
            )?;
        }
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Returns the stable parent attempt for a child task, when one exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read or an ID is malformed.
    pub fn delegation_parent(
        &self,
        child_task_id: TaskId,
    ) -> Result<Option<(TaskId, AttemptId, u32)>, StoreError> {
        self.connection
            .query_row(
                "SELECT parent_task_id, parent_attempt_id, depth FROM delegation_edges WHERE child_task_id = ?1",
                [child_task_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?)),
            )
            .optional()?
            .map(|(task, attempt, depth)| {
                Ok((
                    task.parse().map_err(|_| StoreError::InvalidDelegationParent)?,
                    attempt.parse().map_err(|_| StoreError::InvalidDelegationParent)?,
                    depth,
                ))
            })
            .transpose()
    }

    /// Counts children whose latest revision has not been delivered and
    /// acknowledged by this parent worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata query fails.
    pub fn unsettled_children(&self, parent_attempt_id: AttemptId) -> Result<u64, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM delegation_edges e
             JOIN tasks t ON t.task_id = e.child_task_id
               AND t.revision = (SELECT MAX(t2.revision) FROM tasks t2 WHERE t2.task_id = e.child_task_id)
             LEFT JOIN results r ON r.result_id = (
               SELECT latest.result_id FROM results latest
               WHERE latest.task_id = t.task_id AND latest.revision = t.revision
               ORDER BY latest.rowid DESC LIMIT 1
             )
             LEFT JOIN inbox_items i ON i.result_id = r.result_id AND i.owner_id = t.owner_id
             LEFT JOIN decisions d ON d.result_id = r.result_id
             WHERE e.parent_attempt_id = ?1
               AND (r.result_id IS NULL OR i.acknowledged IS NULL OR i.acknowledged = 0
                 OR (json_extract(r.envelope_json, '$.outcome') = 'candidate' AND d.result_id IS NULL))",
            [parent_attempt_id.to_string()],
            |row| row.get(0),
        )?;
        u64::try_from(count).map_err(|_| StoreError::NumericOverflow)
    }

    /// Claims the sole active attempt slot for a task revision.
    ///
    /// Only a failed prior attempt can release the slot for a bounded retry.
    /// Lost, cancelled, and candidate results cannot start another attempt on
    /// the same revision. A concurrent supervisor cannot overlap a live run.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or an active attempt exists.
    pub fn claim_attempt(
        &self,
        task_id: TaskId,
        revision: u32,
        attempt_id: AttemptId,
    ) -> Result<(), StoreError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let spec_json = transaction
            .query_row(
                "SELECT spec_json FROM tasks WHERE task_id = ?1 AND revision = ?2",
                params![task_id.to_string(), revision],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        let task: TaskSpec = serde_json::from_str(&spec_json)?;
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM attempts WHERE task_id = ?1 AND revision = ?2",
            params![task_id.to_string(), revision],
            |row| row.get(0),
        )?;
        if count >= i64::from(task.budget.max_attempts) {
            return Err(StoreError::AttemptBudgetExhausted { task_id, revision });
        }
        let prior_result: Option<(String, String)> = transaction
            .query_row(
                "SELECT a.attempt_id, json_extract(r.envelope_json, '$.outcome')
                 FROM results r JOIN attempts a ON a.attempt_id = r.attempt_id
                 WHERE a.task_id = ?1 AND a.revision = ?2
                 ORDER BY r.rowid DESC LIMIT 1",
                params![task_id.to_string(), revision],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((prior_id, outcome)) = &prior_result {
            match outcome.as_str() {
                "lost" => return Err(StoreError::UnresolvedPriorAttempt { task_id, revision }),
                "candidate" | "cancelled" => {
                    return Err(StoreError::NonRetryablePriorAttempt { task_id, revision });
                }
                "failed" => {
                    let grant = transaction
                        .query_row(
                            "SELECT 1 FROM pre_spawn_retry_grants WHERE attempt_id = ?1",
                            [prior_id],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !grant {
                        return Err(StoreError::NonRetryablePriorAttempt { task_id, revision });
                    }
                }
                _ => return Err(StoreError::NonRetryablePriorAttempt { task_id, revision }),
            }
        }
        let inserted = transaction.execute(
            "INSERT INTO attempts (attempt_id, task_id, revision, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt_id.to_string(),
                task_id.to_string(),
                revision,
                state_name(AttemptState::Queued),
            ],
        );
        match inserted {
            Ok(_) => {
                transaction.commit()?;
                Ok(())
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let active = transaction.query_row(
                    "SELECT 1 FROM attempts WHERE task_id = ?1 AND revision = ?2 AND state <> 'terminal' LIMIT 1",
                    params![task_id.to_string(), revision],
                    |_| Ok(()),
                ).optional()?.is_some();
                if active {
                    Err(StoreError::ActiveAttemptExists { task_id, revision })
                } else {
                    Err(StoreError::Database(rusqlite::Error::SqliteFailure(
                        error, None,
                    )))
                }
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }

    /// Allows one bounded retry only after the runner reported a transient
    /// process-spawn failure. A crash before this receipt stays non-retryable.
    ///
    /// # Errors
    ///
    /// Returns an error unless the attempt has a committed failed result.
    pub fn grant_pre_spawn_retry(&self, attempt_id: AttemptId) -> Result<(), StoreError> {
        let outcome: Option<String> = self
            .connection
            .query_row(
                "SELECT json_extract(envelope_json, '$.outcome') FROM results WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if outcome.as_deref() != Some("failed") {
            return Err(StoreError::RetryGrantRequiresFailedAttempt(attempt_id));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO pre_spawn_retry_grants (attempt_id) VALUES (?1)",
            [attempt_id.to_string()],
        )?;
        Ok(())
    }

    /// Compatibility alias for `claim_attempt`.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or the attempt already exists.
    pub fn create_attempt(
        &self,
        task_id: TaskId,
        revision: u32,
        attempt_id: AttemptId,
    ) -> Result<(), StoreError> {
        self.claim_attempt(task_id, revision, attempt_id)
    }

    /// Persists the launch intent before spawning a process. The nonce is a
    /// fresh opaque value for this attempt and must not be reused on retry.
    ///
    /// # Errors
    ///
    /// Rejects a missing/non-starting attempt, invalid receipt, or a second
    /// launch claim, including one from another supervisor connection.
    pub fn record_launch_intent(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        supervisor_epoch: u64,
    ) -> Result<(), StoreError> {
        if nonce.trim().is_empty() || supervisor_epoch == 0 {
            return Err(StoreError::InvalidLaunchIntent);
        }
        let epoch = i64::try_from(supervisor_epoch).map_err(|_| StoreError::NumericOverflow)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                "SELECT state FROM attempts WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(attempt_id))?;
        let state = parse_state(&state)?;
        if state != AttemptState::Starting {
            return Err(StoreError::AttemptStateConflict {
                expected: AttemptState::Starting,
                actual: state,
            });
        }
        let inserted = transaction.execute(
            "INSERT INTO launch_intents (attempt_id, launch_nonce, supervisor_epoch)
             VALUES (?1, ?2, ?3)",
            params![attempt_id.to_string(), nonce, epoch],
        );
        match inserted {
            Ok(1) => transaction.commit().map_err(StoreError::from),
            Ok(_) => Err(StoreError::InvalidLaunchIntent),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::LaunchIntentConflict(attempt_id))
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }

    /// Attaches a verified process/session incarnation to the original intent.
    /// A changed incarnation cannot replace it.
    ///
    /// # Errors
    ///
    /// Rejects a stale nonce, invalid identity, or conflicting observation.
    pub fn record_runner_identity(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        identity: &RunnerIdentity,
    ) -> Result<WriteOutcome, StoreError> {
        identity.validate()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let stored = read_launch_intent(&transaction, attempt_id)?
            .ok_or(StoreError::LaunchIntentNotFound(attempt_id))?;
        if stored.nonce != nonce {
            return Err(StoreError::LaunchIntentConflict(attempt_id));
        }
        if let Some(existing) = stored.runner_identity {
            return if existing == *identity {
                Ok(WriteOutcome::AlreadyApplied)
            } else {
                Err(StoreError::RunnerIdentityConflict(attempt_id))
            };
        }
        let state = self.attempt_state_by_id(attempt_id)?;
        if state == AttemptState::Terminal {
            return Err(StoreError::AttemptStateConflict {
                expected: AttemptState::Running,
                actual: state,
            });
        }
        transaction.execute(
            "UPDATE launch_intents SET runner_identity_json = ?1
             WHERE attempt_id = ?2 AND launch_nonce = ?3 AND runner_identity_json IS NULL",
            params![
                serde_json::to_string(identity)?,
                attempt_id.to_string(),
                nonce
            ],
        )?;
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Lists every attempt that has no terminal result, including legacy
    /// attempts with no launch receipt. Recovery never silently retries them.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data or database failure.
    pub fn unfinished_attempts(&self) -> Result<Vec<UnfinishedAttempt>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT a.attempt_id, a.state, t.spec_json, l.launch_nonce,
                    l.supervisor_epoch, l.runner_identity_json
             FROM attempts a JOIN tasks t
               ON t.task_id = a.task_id AND t.revision = a.revision
             LEFT JOIN launch_intents l ON l.attempt_id = a.attempt_id
             WHERE a.state <> 'terminal' ORDER BY a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, state, task, nonce, epoch, identity) = row?;
            let launch = match (nonce, epoch) {
                (Some(nonce), Some(epoch)) => Some(LaunchIntent {
                    nonce,
                    supervisor_epoch: u64::try_from(epoch)
                        .map_err(|_| StoreError::NumericOverflow)?,
                    runner_identity: identity
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                }),
                (None, None) => None,
                _ => return Err(StoreError::InvalidLaunchIntent),
            };
            Ok(UnfinishedAttempt {
                task: serde_json::from_str(&task)?,
                attempt_id: id.parse().map_err(|_| StoreError::InvalidAttemptId(id))?,
                state: parse_state(&state)?,
                launch,
            })
        })
        .collect()
    }

    /// Updates the durable state for an attempt using the current state.
    ///
    /// Prefer `compare_and_set_attempt_state` when the caller has an observed
    /// state: only that method rejects a competing writer's intervening update.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt does not exist or storage fails.
    pub fn set_attempt_state(
        &self,
        attempt_id: AttemptId,
        state: AttemptState,
    ) -> Result<(), StoreError> {
        let current = self.attempt_state_by_id(attempt_id)?;
        self.compare_and_set_attempt_state(attempt_id, current, state)
    }

    /// Applies a legal attempt transition only if the observed state is current.
    ///
    /// # Errors
    ///
    /// Returns a conflict for a stale writer or an invalid transition. Terminal
    /// state is reserved for `commit_terminal_result`.
    pub fn compare_and_set_attempt_state(
        &self,
        attempt_id: AttemptId,
        expected: AttemptState,
        next: AttemptState,
    ) -> Result<(), StoreError> {
        if !allowed_attempt_transition(expected, next) {
            return Err(StoreError::AttemptTransitionInvalid {
                from: expected,
                to: next,
            });
        }
        let changed = self.connection.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2 AND state = ?3",
            params![
                state_name(next),
                attempt_id.to_string(),
                state_name(expected)
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let actual = self.attempt_state_by_id(attempt_id)?;
        Err(StoreError::AttemptStateConflict { expected, actual })
    }

    /// Returns an attempt's durable state by its immutable ID.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing attempt or invalid stored state.
    pub fn attempt_state_by_id(&self, attempt_id: AttemptId) -> Result<AttemptState, StoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT state FROM attempts WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(attempt_id))?;
        parse_state(&state)
    }

    /// Atomically stores one terminal result and its owner inbox item.
    ///
    /// Exact replay is idempotent. A second, different result for an attempt is
    /// rejected without changing either result or inbox state.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid relationship, conflicting terminal
    /// result, serialization failure, or database failure.
    pub fn commit_terminal_result(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
    ) -> Result<WriteOutcome, StoreError> {
        self.commit_terminal_result_guarded(owner_id, result, None)
    }

    /// Reads separately committed native route evidence for one result.
    /// Older results without a receipt return `None`.
    ///
    /// # Errors
    ///
    /// Rejects altered receipt bytes or malformed persisted JSON.
    pub fn route_observation(
        &self,
        result_id: ResultId,
    ) -> Result<Option<RouteObservation>, StoreError> {
        let stored: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT observation_digest, observation_json FROM route_observations WHERE result_id = ?1",
                [result_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        stored
            .map(|(digest, json)| {
                if sha256(json.as_bytes()) != digest {
                    return Err(StoreError::RouteObservationIntegrityMismatch);
                }
                Ok(serde_json::from_str(&json)?)
            })
            .transpose()
    }

    /// Publishes a lost result only if the observed unfinished state and
    /// launch receipt are still current. A concurrent runner result wins or
    /// loses atomically; it can never be overwritten by recovery.
    ///
    /// # Errors
    ///
    /// Rejects a stale observation, non-lost outcome, or normal terminal
    /// result conflict.
    pub fn commit_recovered_lost(
        &mut self,
        observed: &UnfinishedAttempt,
        result: &ResultEnvelope,
    ) -> Result<WriteOutcome, StoreError> {
        if result.outcome != brgr_protocol::TerminalOutcome::Lost {
            return Err(StoreError::RecoveryRequiresLost);
        }
        self.commit_terminal_result_guarded(&observed.task.owner_id, result, Some(observed))
    }

    fn commit_terminal_result_guarded(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
        observed: Option<&UnfinishedAttempt>,
    ) -> Result<WriteOutcome, StoreError> {
        validate_terminal_result(result)?;
        let envelope_json = serde_json::to_string(result)?;
        let digest = sha256(envelope_json.as_bytes());
        let observation = serialize_route_observation(result.route_observation.as_ref())?;
        let transaction = self.connection.transaction()?;

        if let Some(observed) = observed {
            validate_recovery_observation(&transaction, observed)?;
        }

        if let Some((stored_id, stored_digest)) = transaction
            .query_row(
                "SELECT result_id, result_digest FROM results WHERE attempt_id = ?1",
                [result.attempt_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_id == result.result_id.to_string() && stored_digest == digest {
                verify_replayed_route_observation(
                    &transaction,
                    result.result_id,
                    observation.as_ref(),
                )?;
                transaction.commit()?;
                return Ok(WriteOutcome::AlreadyApplied);
            }
            return Err(StoreError::TerminalResultConflict(result.attempt_id));
        }

        let expected = terminal_attempt(&transaction, result.attempt_id)?;
        if (expected.0, expected.1) != (result.task_id.to_string(), result.revision) {
            return Err(StoreError::AttemptResultMismatch);
        }
        if expected.2 != owner_id.as_str() {
            return Err(StoreError::ResultOwnerMismatch);
        }
        let current = parse_state(&expected.3)?;
        if current == AttemptState::Terminal {
            return Err(StoreError::AttemptTransitionInvalid {
                from: current,
                to: AttemptState::Terminal,
            });
        }
        verify_candidate_artifacts(&self.artifacts, result, &expected.4)?;

        transaction.execute(
            "INSERT INTO results
             (result_id, attempt_id, task_id, revision, result_digest, envelope_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                result.result_id.to_string(),
                result.attempt_id.to_string(),
                result.task_id.to_string(),
                result.revision,
                digest,
                envelope_json,
            ],
        )?;
        insert_route_observation(&transaction, result.result_id, observation.as_ref())?;
        transaction.execute(
            "INSERT INTO inbox_items (owner_id, result_id) VALUES (?1, ?2)",
            params![owner_id.as_str(), result.result_id.to_string()],
        )?;
        transaction.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2 AND state = ?3",
            params![
                state_name(AttemptState::Terminal),
                result.attempt_id.to_string(),
                state_name(current),
            ],
        )?;
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
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Lists an owner's inbox, optionally including acknowledged entries.
    ///
    /// # Errors
    ///
    /// Returns an error when stored data is invalid or storage fails.
    pub fn inbox(
        &self,
        owner_id: &OwnerId,
        include_acknowledged: bool,
    ) -> Result<Vec<InboxItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT r.envelope_json, i.acknowledged
             FROM inbox_items i
             JOIN results r ON r.result_id = i.result_id
             WHERE i.owner_id = ?1 AND (?2 = 1 OR i.acknowledged = 0)
             ORDER BY r.rowid",
        )?;
        let rows = statement
            .query_map(params![owner_id.as_str(), include_acknowledged], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?;
        rows.map(|row| {
            let (json, acknowledged) = row?;
            Ok(InboxItem {
                owner_id: owner_id.clone(),
                result: serde_json::from_str(&json)?,
                acknowledged,
            })
        })
        .collect()
    }

    /// Reads pending inbox items for every owner explicitly bound to one
    /// session, including owners transferred from earlier sessions.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stored data or a DB failure.
    pub fn pending_for_session(&self, session_id: &str) -> Result<Vec<InboxItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT i.owner_id, r.envelope_json FROM inbox_items i
             JOIN results r ON r.result_id = i.result_id
             JOIN owner_bindings b ON b.owner_id = i.owner_id
             WHERE b.session_id = ?1 AND i.acknowledged = 0
             ORDER BY r.rowid LIMIT 100",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (owner_id, envelope_json) = row?;
            Ok(InboxItem {
                owner_id: OwnerId::new(owner_id)?,
                result: serde_json::from_str(&envelope_json)?,
                acknowledged: false,
            })
        })
        .collect()
    }

    /// Acknowledges one result in the named owner's inbox.
    ///
    /// # Errors
    ///
    /// Returns an error if that owner has no matching inbox item.
    pub fn acknowledge(&self, owner_id: &OwnerId, result_id: ResultId) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE inbox_items SET acknowledged = 1
             WHERE owner_id = ?1 AND result_id = ?2",
            params![owner_id.as_str(), result_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InboxItemNotFound);
        }
        Ok(())
    }

    /// Acknowledges an inbox item only while the caller's session epoch is
    /// still current. The binding check and acknowledgment are one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for an unbound/stale session, missing item, or DB failure.
    pub fn acknowledge_bound(
        &self,
        owner_id: &OwnerId,
        result_id: ResultId,
        session_id: &str,
        binding_epoch: u64,
    ) -> Result<(), StoreError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        assert_owner_binding(
            &transaction,
            owner_id,
            Some(session_id),
            Some(binding_epoch),
        )?;
        let changed = transaction.execute(
            "UPDATE inbox_items SET acknowledged = 1 WHERE owner_id = ?1 AND result_id = ?2",
            params![owner_id.as_str(), result_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InboxItemNotFound);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Records the sole accept/reject decision for a result.
    ///
    /// Semantic replay is idempotent even when a caller generated a new
    /// `DecisionId` after a lost response. A different verdict or reason is
    /// rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for missing results, digest/owner mismatches,
    /// conflicting decisions, invalid serialization, or database failure.
    pub fn record_decision(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let outcome = record_decision_in_transaction(&transaction, &self.artifacts, decision)?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Records an accept/reject decision and acknowledges its owner's inbox
    /// item in one transaction. A semantic retry remains idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing inbox item, mismatched owner or digest,
    /// conflicting decision, or failed transaction.
    pub fn record_decision_and_ack(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let outcome = record_decision_in_transaction(&transaction, &self.artifacts, decision)?;
        let changed = transaction.execute(
            "UPDATE inbox_items SET acknowledged = 1 WHERE owner_id = ?1 AND result_id = ?2",
            params![decision.owner_id.as_str(), decision.result_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::InboxItemNotFound);
        }
        transaction.commit()?;
        Ok(outcome)
    }

    /// Returns the stored decision for one terminal result, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored decision is invalid or storage fails.
    pub fn decision_for_result(&self, result_id: ResultId) -> Result<Option<Decision>, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT decision_json FROM decisions WHERE result_id = ?1",
                [result_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
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

    /// Loads the latest revision for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or stored data is invalid.
    pub fn task(&self, task_id: TaskId) -> Result<TaskSpec, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT spec_json FROM tasks WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Lists the most recently created task revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when stored data is invalid or storage fails.
    pub fn tasks(&self, limit: usize) -> Result<Vec<TaskSpec>, StoreError> {
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             JOIN (SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id) latest
             ON latest.task_id = t.task_id AND latest.revision = t.revision
             ORDER BY t.rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([bounded], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Lists latest task revisions whose owner is currently bound to a session.
    /// The optional owner narrows an explicit `BRGR_OWNER_ID` without allowing
    /// unrelated owners to consume the result limit.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data, limits, or DB failures.
    pub fn tasks_for_session(
        &self,
        session_id: &str,
        owner_id: Option<&OwnerId>,
        limit: usize,
    ) -> Result<Vec<TaskSpec>, StoreError> {
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             JOIN owner_bindings b ON b.owner_id = t.owner_id
             JOIN (SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id) latest
               ON latest.task_id = t.task_id AND latest.revision = t.revision
             WHERE b.session_id = ?1 AND (?2 IS NULL OR t.owner_id = ?2)
             ORDER BY t.rowid DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![session_id, owner_id.map(OwnerId::as_str), bounded],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Returns admitted task revisions for which no supervisor ever claimed
    /// an attempt. The caller can reconcile an abandoned launch without guessing
    /// that a model run succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored task data or a database failure.
    pub fn unstarted_tasks(&self) -> Result<Vec<TaskSpec>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             WHERE NOT EXISTS (
                 SELECT 1 FROM attempts a
                 WHERE a.task_id = t.task_id AND a.revision = t.revision
             ) ORDER BY t.rowid",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Returns the most recent attempt state for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when the task has no attempt or stored state is invalid.
    pub fn attempt_state(&self, task_id: TaskId) -> Result<AttemptState, StoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT state FROM attempts WHERE task_id = ?1
                 AND revision = (SELECT MAX(revision) FROM tasks WHERE task_id = ?1)
                 ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        parse_state(&state)
    }

    /// Returns the latest terminal result for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when no terminal result exists or stored data is invalid.
    pub fn latest_result(&self, task_id: TaskId) -> Result<ResultEnvelope, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT envelope_json FROM results WHERE task_id = ?1
                 AND revision = (SELECT MAX(revision) FROM tasks WHERE task_id = ?1)
                 ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Records a producer event once by both event ID and producer sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed events, missing attempts, or conflicting
    /// reuse of an event identity or producer sequence.
    pub fn record_event(&self, event: &Event) -> Result<WriteOutcome, StoreError> {
        validate_schema(&event.schema)?;
        if event.producer.trim().is_empty() || event.producer_seq == 0 {
            return Err(StoreError::InvalidEvent);
        }
        let producer_seq =
            i64::try_from(event.producer_seq).map_err(|_| StoreError::NumericOverflow)?;
        let json = serde_json::to_string(event)?;
        if let Some(stored) = self
            .connection
            .query_row(
                "SELECT event_json FROM events WHERE event_id = ?1",
                [event.event_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return if stored == json {
                Ok(WriteOutcome::AlreadyApplied)
            } else {
                Err(StoreError::EventConflict)
            };
        }
        let inserted = self.connection.execute(
            "INSERT INTO events (event_id, attempt_id, producer, producer_seq, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.event_id.to_string(),
                event.attempt_id.to_string(),
                event.producer,
                producer_seq,
                json,
            ],
        );
        match inserted {
            Ok(_) => Ok(WriteOutcome::Inserted),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::EventConflict)
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }

    /// Initially binds an owner to an explicit session epoch without inferring
    /// focus. A late hook cannot replace a different session; use
    /// `rebind_owner` for an explicit transfer.
    ///
    /// # Errors
    ///
    /// Returns an error when another session is already bound or persistence
    /// fails.
    pub fn bind_owner(
        &self,
        owner_id: &OwnerId,
        session_id: &str,
        binding_epoch: u64,
    ) -> Result<WriteOutcome, StoreError> {
        if session_id.trim().is_empty() || binding_epoch == 0 {
            return Err(StoreError::InvalidOwnerBinding);
        }
        let binding_epoch =
            i64::try_from(binding_epoch).map_err(|_| StoreError::NumericOverflow)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((stored_session, _stored_epoch)) = existing {
            if stored_session == session_id {
                return Ok(WriteOutcome::AlreadyApplied);
            }
            return Err(StoreError::OwnerBindingConflict);
        }
        transaction.execute(
            "INSERT INTO owner_bindings (owner_id, session_id, binding_epoch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(owner_id) DO UPDATE SET
               session_id = excluded.session_id,
               binding_epoch = excluded.binding_epoch",
            params![owner_id.as_str(), session_id, binding_epoch],
        )?;
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Reads the current cooperative session binding for an owner.
    ///
    /// # Errors
    ///
    /// Returns an error if persistence or epoch conversion fails.
    pub fn owner_binding(&self, owner_id: &OwnerId) -> Result<Option<(String, u64)>, StoreError> {
        self.connection
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(session, epoch)| {
                Ok((
                    session,
                    u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)?,
                ))
            })
            .transpose()
    }

    /// Explicitly transfers an owner to a new session with a larger epoch.
    /// Existing inbox items and decisions remain under the same owner ID.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty session, epoch overflow, or DB failure.
    pub fn rebind_owner(&self, owner_id: &OwnerId, session_id: &str) -> Result<u64, StoreError> {
        if session_id.trim().is_empty() {
            return Err(StoreError::InvalidOwnerBinding);
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let prior: Option<(String, i64)> = transaction
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_session, epoch)) = &prior
            && stored_session == session_id
        {
            return u64::try_from(*epoch).map_err(|_| StoreError::NumericOverflow);
        }
        let epoch = prior.map_or(Ok(1_i64), |(_, epoch)| {
            epoch.checked_add(1).ok_or(StoreError::NumericOverflow)
        })?;
        transaction.execute(
            "INSERT INTO owner_bindings (owner_id, session_id, binding_epoch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(owner_id) DO UPDATE SET
               session_id = excluded.session_id,
               binding_epoch = excluded.binding_epoch",
            params![owner_id.as_str(), session_id, epoch],
        )?;
        transaction.commit()?;
        u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)
    }
}

fn assert_owner_binding(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    session_id: Option<&str>,
    binding_epoch: Option<u64>,
) -> Result<(), StoreError> {
    let Some((bound_session, bound_epoch)) = transaction
        .query_row(
            "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
            [owner_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Err(StoreError::OwnerUnbound(owner_id.clone()));
    };
    if session_id != Some(bound_session.as_str())
        || binding_epoch != u64::try_from(bound_epoch).ok()
    {
        return Err(StoreError::OwnerBindingConflict);
    }
    Ok(())
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

fn recorded_delegation_parent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
) -> Result<Option<(String, String)>, StoreError> {
    transaction
        .query_row(
            "SELECT parent_task_id, parent_attempt_id FROM delegation_edges WHERE child_task_id = ?1",
            [task_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)
}

fn validate_new_task_parent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    recorded: Option<&(String, String)>,
    requested: Option<&(String, String)>,
) -> Result<(), StoreError> {
    if recorded.is_some() && recorded != requested {
        return Err(StoreError::InvalidDelegationParent);
    }
    if recorded.is_none() && requested.is_some() {
        let prior_task: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM tasks WHERE task_id = ?1 LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if prior_task.is_some() {
            return Err(StoreError::InvalidDelegationParent);
        }
    }
    Ok(())
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

fn verify_candidate_artifacts(
    artifacts: &ArtifactStore,
    result: &ResultEnvelope,
    task_json: &str,
) -> Result<(), StoreError> {
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

fn private_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use brgr_protocol::{
        ArtifactContract, AttemptBudget, DecisionId, DecisionVerdict, EventId, EventKind,
        ObservationSource, Route, SCHEMA_V1, TerminalOutcome,
    };
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn duplicate_terminal_event_keeps_one_inbox_item() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "digest-1").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);

        assert_eq!(
            store
                .commit_terminal_result(&task.owner_id, &result)
                .unwrap(),
            WriteOutcome::Inserted
        );
        assert_eq!(
            store
                .commit_terminal_result(&task.owner_id, &result)
                .unwrap(),
            WriteOutcome::AlreadyApplied
        );
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);

        let result_id = result.result_id;
        let conflicting = ResultEnvelope {
            result_id: ResultId::new(),
            ..result
        };
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &conflicting),
            Err(StoreError::TerminalResultConflict(_))
        ));
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);

        drop(store);
        let reopened = Store::open(root.path()).unwrap();
        assert_eq!(reopened.inbox(&task.owner_id, false).unwrap().len(), 1);
        reopened.acknowledge(&task.owner_id, result_id).unwrap();
        drop(reopened);
        let reopened = Store::open(root.path()).unwrap();
        assert!(reopened.inbox(&task.owner_id, false).unwrap().is_empty());
        assert_eq!(reopened.inbox(&task.owner_id, true).unwrap().len(), 1);
    }

    #[test]
    fn event_sequence_and_owner_binding_are_idempotent() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "digest-event").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let event = Event {
            schema: SCHEMA_V1.to_owned(),
            event_id: EventId::new(),
            attempt_id,
            producer: "fixture".to_owned(),
            producer_seq: 1,
            kind: EventKind::Running,
            payload: serde_json::json!({}),
        };
        assert_eq!(store.record_event(&event).unwrap(), WriteOutcome::Inserted);
        assert_eq!(
            store.record_event(&event).unwrap(),
            WriteOutcome::AlreadyApplied
        );
        let conflicting = Event {
            event_id: EventId::new(),
            ..event
        };
        assert!(matches!(
            store.record_event(&conflicting),
            Err(StoreError::EventConflict)
        ));

        assert_eq!(
            store.bind_owner(&task.owner_id, "session-a", 1).unwrap(),
            WriteOutcome::Inserted
        );
        assert_eq!(
            store.bind_owner(&task.owner_id, "session-a", 1).unwrap(),
            WriteOutcome::AlreadyApplied
        );
        assert!(matches!(
            store.bind_owner(&task.owner_id, "session-b", 1),
            Err(StoreError::OwnerBindingConflict)
        ));
        assert!(matches!(
            store.bind_owner(&task.owner_id, "session-b", 2),
            Err(StoreError::OwnerBindingConflict)
        ));
        assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
        assert!(matches!(
            store.bind_owner(&task.owner_id, "session-a", 100),
            Err(StoreError::OwnerBindingConflict)
        ));
    }

    #[test]
    fn one_decision_is_bound_to_owner_and_result_digest() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "digest-1").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "meets contract".to_owned(),
        };

        assert_eq!(
            store.record_decision(&decision).unwrap(),
            WriteOutcome::Inserted
        );
        assert_eq!(
            store.record_decision(&decision).unwrap(),
            WriteOutcome::AlreadyApplied
        );
        let conflicting = Decision {
            decision_id: DecisionId::new(),
            verdict: DecisionVerdict::Rejected,
            ..decision
        };
        assert!(matches!(
            store.record_decision(&conflicting),
            Err(StoreError::DecisionConflict(_))
        ));
    }

    #[test]
    fn stale_binding_epoch_cannot_decide_or_replay_after_rebind() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "binding-epoch").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert!(matches!(
            store.record_decision(&Decision {
                schema: SCHEMA_V1.to_owned(),
                decision_id: DecisionId::new(),
                owner_id: task.owner_id.clone(),
                task_id: task.task_id,
                revision: task.revision,
                result_id: result.result_id,
                result_digest: Store::result_digest(&result).unwrap(),
                session_id: Some("session-a".to_owned()),
                binding_epoch: Some(1),
                verdict: DecisionVerdict::Accepted,
                reason: "fixture".to_owned(),
            }),
            Err(StoreError::OwnerUnbound(_))
        ));
        assert_eq!(store.rebind_owner(&task.owner_id, "session-a").unwrap(), 1);
        let old = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "fixture".to_owned(),
        };
        assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
        assert!(matches!(
            store.acknowledge_bound(&task.owner_id, result.result_id, "session-a", 1),
            Err(StoreError::OwnerBindingConflict)
        ));
        assert!(matches!(
            store.record_decision_and_ack(&old),
            Err(StoreError::OwnerBindingConflict)
        ));
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
        let current = Decision {
            session_id: Some("session-b".to_owned()),
            binding_epoch: Some(2),
            ..old
        };
        store.record_decision_and_ack(&current).unwrap();
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
    }

    #[test]
    fn session_task_list_filters_before_applying_the_limit() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let owned = task();
        store.record_task(&owned, "owned").unwrap();
        store.bind_owner(&owned.owner_id, "session-a", 1).unwrap();
        let other_owner = OwnerId::new("codex:other").unwrap();
        store.bind_owner(&other_owner, "session-b", 1).unwrap();
        for number in 0..21 {
            let mut other = task();
            other.owner_id = other_owner.clone();
            other.create_request_id = format!("other-{number}");
            store
                .record_task(&other, &format!("digest-{number}"))
                .unwrap();
        }
        let listed = store.tasks_for_session("session-a", None, 20).unwrap();
        assert_eq!(listed, vec![owned]);
        assert!(
            store
                .tasks_for_session("session-a", Some(&other_owner), 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn transferred_owner_pending_inbox_reappears_in_new_session() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let first = task();
        let mut second = task();
        second.owner_id = OwnerId::new("codex:transferred").unwrap();
        second.create_request_id = "transferred-task".to_owned();
        for (index, spec) in [first.clone(), second.clone()].iter().enumerate() {
            store
                .record_task(spec, &format!("pending-{index}"))
                .unwrap();
            let attempt_id = AttemptId::new();
            store
                .claim_attempt(spec.task_id, spec.revision, attempt_id)
                .unwrap();
            store
                .commit_terminal_result(
                    &spec.owner_id,
                    &ResultEnvelope {
                        outcome: TerminalOutcome::Failed,
                        error: Some("fixture".to_owned()),
                        ..result(spec, attempt_id)
                    },
                )
                .unwrap();
        }
        store.bind_owner(&first.owner_id, "session-a", 1).unwrap();
        store.bind_owner(&second.owner_id, "session-b", 1).unwrap();
        assert_eq!(store.pending_for_session("session-a").unwrap().len(), 1);
        store.rebind_owner(&second.owner_id, "session-a").unwrap();
        let pending = store.pending_for_session("session-a").unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|item| item.owner_id == second.owner_id));
        store
            .acknowledge_bound(
                &first.owner_id,
                pending
                    .iter()
                    .find(|item| item.owner_id == first.owner_id)
                    .unwrap()
                    .result
                    .result_id,
                "session-a",
                1,
            )
            .unwrap();
        assert_eq!(store.pending_for_session("session-a").unwrap().len(), 1);
    }

    #[test]
    fn failed_result_cannot_be_accepted_through_store_api() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "failed-decision").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = ResultEnvelope {
            outcome: TerminalOutcome::Failed,
            error: Some("no artifact".to_owned()),
            ..result(&task, attempt_id)
        };
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "must fail".to_owned(),
        };
        assert!(matches!(
            store.record_decision_and_ack(&decision),
            Err(StoreError::DecisionRequiresCandidate)
        ));
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn candidate_without_sealed_bytes_cannot_enter_inbox() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "unsealed-decision").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = result(&task, attempt_id);
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &result),
            Err(StoreError::CandidateRequiresArtifact)
        ));
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    }

    #[test]
    fn forged_candidate_artifact_cannot_enter_inbox() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "forged-artifact").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let mut result = sealed_result(&store, &task, attempt_id);
        result.artifacts[0].store_relative_path = "../outside".to_owned();
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &result),
            Err(StoreError::InvalidArtifactReference)
        ));
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    }

    #[test]
    fn stale_attempt_writer_cannot_overwrite_newer_or_terminal_state() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "attempt-cas").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        store
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        assert!(matches!(
            store.compare_and_set_attempt_state(
                attempt_id,
                AttemptState::Queued,
                AttemptState::CancelRequested
            ),
            Err(StoreError::AttemptStateConflict {
                actual: AttemptState::Starting,
                ..
            })
        ));
        assert!(matches!(
            store.set_attempt_state(attempt_id, AttemptState::Queued),
            Err(StoreError::AttemptTransitionInvalid { .. })
        ));
        store
            .commit_terminal_result(&task.owner_id, &sealed_result(&store, &task, attempt_id))
            .unwrap();
        assert!(matches!(
            store.set_attempt_state(attempt_id, AttemptState::Running),
            Err(StoreError::AttemptTransitionInvalid { .. })
        ));
        assert_eq!(
            store.attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Terminal
        );
    }

    #[test]
    fn two_store_connections_cannot_claim_overlapping_attempts() {
        let root = TempDir::new().unwrap();
        let mut first = Store::open(root.path()).unwrap();
        let task = task();
        first.record_task(&task, "claim-attempt").unwrap();
        let second = Store::open(root.path()).unwrap();
        let first_id = AttemptId::new();
        first
            .claim_attempt(task.task_id, task.revision, first_id)
            .unwrap();
        assert!(matches!(
            second.claim_attempt(task.task_id, task.revision, AttemptId::new()),
            Err(StoreError::ActiveAttemptExists { .. })
        ));
        let retryable = ResultEnvelope {
            outcome: TerminalOutcome::Failed,
            artifacts: vec![],
            error: Some("pre-spawn fixture failure".to_owned()),
            ..result(&task, first_id)
        };
        first
            .commit_terminal_result(&task.owner_id, &retryable)
            .unwrap();
        assert!(matches!(
            second.claim_attempt(task.task_id, task.revision, AttemptId::new()),
            Err(StoreError::NonRetryablePriorAttempt { .. })
        ));
        first.grant_pre_spawn_retry(first_id).unwrap();
        second
            .claim_attempt(task.task_id, task.revision, AttemptId::new())
            .unwrap();
    }

    #[test]
    fn launch_intent_survives_reopen_and_cannot_be_replaced() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "launch-intent").unwrap();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        store
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        store
            .record_launch_intent(attempt_id, "nonce-a", 7)
            .unwrap();
        let identity = RunnerIdentity {
            namespace: "process".to_owned(),
            handle: "4242".to_owned(),
            birth_marker: "kernel-start-123".to_owned(),
        };
        assert_eq!(
            store
                .record_runner_identity(attempt_id, "nonce-a", &identity)
                .unwrap(),
            WriteOutcome::Inserted
        );
        drop(store);

        let reopened = Store::open(root.path()).unwrap();
        let unfinished = reopened.unfinished_attempts().unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].launch.as_ref().unwrap().nonce, "nonce-a");
        assert_eq!(
            unfinished[0].launch.as_ref().unwrap().runner_identity,
            Some(identity.clone())
        );
        assert!(matches!(
            reopened.record_launch_intent(attempt_id, "nonce-b", 8),
            Err(StoreError::LaunchIntentConflict(_))
        ));
        assert!(matches!(
            reopened.record_runner_identity(
                attempt_id,
                "nonce-a",
                &RunnerIdentity {
                    birth_marker: "reused-pid".to_owned(),
                    ..identity
                }
            ),
            Err(StoreError::RunnerIdentityConflict(_))
        ));
    }

    #[test]
    fn recovery_observation_cannot_overwrite_concurrent_runner_result() {
        let root = TempDir::new().unwrap();
        let mut first = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        first.record_task(&task, "recovery-race").unwrap();
        first
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        first
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        first
            .record_launch_intent(attempt_id, "nonce-race", 1)
            .unwrap();
        let observed = first.unfinished_attempts().unwrap().remove(0);
        let mut second = Store::open(root.path()).unwrap();
        let completed = sealed_result(&second, &task, attempt_id);
        second
            .commit_terminal_result(&task.owner_id, &completed)
            .unwrap();
        let lost = ResultEnvelope {
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Lost,
            legacy_embedded_route_observation: None,
            route_observation: None,
            unresolved_effects: vec!["unknown".to_owned()],
            ..completed.clone()
        };
        assert!(matches!(
            first.commit_recovered_lost(&observed, &lost),
            Err(StoreError::RecoveryObservationStale(_))
        ));
        assert_eq!(first.inbox(&task.owner_id, false).unwrap().len(), 1);
        assert_eq!(first.latest_result(task.task_id).unwrap(), completed);
    }

    #[test]
    fn recovery_observation_cannot_ignore_identity_recorded_after_snapshot() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "identity-race").unwrap();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        store
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        store
            .record_launch_intent(attempt_id, "nonce-identity", 1)
            .unwrap();
        let observed = store.unfinished_attempts().unwrap().remove(0);
        let other = Store::open(root.path()).unwrap();
        other
            .record_runner_identity(
                attempt_id,
                "nonce-identity",
                &RunnerIdentity {
                    namespace: "process".to_owned(),
                    handle: "55".to_owned(),
                    birth_marker: "start-55".to_owned(),
                },
            )
            .unwrap();
        let lost = ResultEnvelope {
            outcome: TerminalOutcome::Lost,
            legacy_embedded_route_observation: None,
            route_observation: None,
            unresolved_effects: vec!["unknown".to_owned()],
            ..result(&task, attempt_id)
        };
        assert!(matches!(
            store.commit_recovered_lost(&observed, &lost),
            Err(StoreError::RecoveryObservationStale(_))
        ));
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    }

    #[test]
    fn decision_and_ack_are_atomic_and_semantic_retries_are_idempotent() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "decision-atomic").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "verified".to_owned(),
        };
        let wrong = Decision {
            result_digest: "wrong".to_owned(),
            ..decision.clone()
        };
        assert!(matches!(
            store.record_decision_and_ack(&wrong),
            Err(StoreError::ResultDigestMismatch)
        ));
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
        assert_eq!(
            store.record_decision_and_ack(&decision).unwrap(),
            WriteOutcome::Inserted
        );
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        let replay = Decision {
            decision_id: DecisionId::new(),
            ..decision.clone()
        };
        assert_eq!(
            store.record_decision_and_ack(&replay).unwrap(),
            WriteOutcome::AlreadyApplied
        );
        let conflict = Decision {
            reason: "changed".to_owned(),
            ..replay
        };
        assert!(matches!(
            store.record_decision_and_ack(&conflict),
            Err(StoreError::DecisionConflict(_))
        ));
        drop(store);
        let store = Store::open(root.path()).unwrap();
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        assert_eq!(store.inbox(&task.owner_id, true).unwrap().len(), 1);
    }

    #[test]
    fn failed_ack_rolls_back_decision_insert() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "missing-inbox").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_ack BEFORE UPDATE ON inbox_items
             BEGIN SELECT RAISE(ABORT, 'fixture ack failure'); END;",
            )
            .unwrap();
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "verified".to_owned(),
        };
        assert!(matches!(
            store.record_decision_and_ack(&decision),
            Err(StoreError::Database(_))
        ));
        let count: u32 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE result_id = ?1",
                [result.result_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn terminal_result_rolls_back_if_inbox_insert_fails() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "terminal-rollback").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_inbox BEFORE INSERT ON inbox_items
                 BEGIN SELECT RAISE(ABORT, 'fixture inbox failure'); END;",
            )
            .unwrap();
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &result),
            Err(StoreError::Database(_))
        ));
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        assert_eq!(
            store.attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Queued
        );
        store
            .connection
            .execute_batch("DROP TRIGGER fail_inbox")
            .unwrap();
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn database_busy_keeps_terminal_result_retriable_without_partial_inbox() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        store.connection.busy_timeout(Duration::ZERO).unwrap();
        let task = task();
        store.record_task(&task, "busy-terminal").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &result),
            Err(StoreError::Database(_))
        ));
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        blocker.execute_batch("ROLLBACK").unwrap();
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn sqlite_full_keeps_terminal_result_retriable_without_partial_commit() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        // DELETE mode grows the database file synchronously at commit, so the
        // page cap below fails deterministically. Production WAL shares the
        // same single-transaction rollback path this test exercises.
        store
            .connection
            .execute_batch("PRAGMA journal_mode=DELETE")
            .unwrap();
        let task = task();
        store.record_task(&task, "full-terminal").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let mut result = sealed_result(&store, &task, attempt_id);
        // Force overflow pages so the commit must grow the database file.
        result.error = Some("disk pressure".to_owned() + &"x".repeat(32_768));
        let page_count: i64 = store
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        store
            .connection
            .execute_batch(&format!("PRAGMA max_page_count={page_count}"))
            .unwrap();
        let error = store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap_err();
        assert!(
            matches!(
                &error,
                StoreError::Database(rusqlite::Error::SqliteFailure(inner, _))
                    if inner.code == rusqlite::ErrorCode::DiskFull
            ),
            "unexpected commit error: {error:?}"
        );
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
        let stored_results: u32 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM results WHERE attempt_id = ?1",
                [result.attempt_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_results, 0);
        assert_eq!(
            store.attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Queued
        );
        store
            .connection
            .execute_batch("PRAGMA max_page_count=1073741823")
            .unwrap();
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn request_id_rejects_a_changed_digest() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();

        assert_eq!(
            store.record_task(&task, "digest-1").unwrap(),
            WriteOutcome::Inserted
        );
        assert_eq!(
            store.record_task(&task, "digest-1").unwrap(),
            WriteOutcome::AlreadyApplied
        );
        assert!(matches!(
            store.record_task(&task, "changed"),
            Err(StoreError::IdempotencyConflict(_))
        ));
    }

    #[test]
    fn task_cannot_exceed_its_total_attempt_budget() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "bounded-attempts").unwrap();
        for number in 0..2 {
            let attempt_id = AttemptId::new();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            for (from, to) in [
                (AttemptState::Queued, AttemptState::Starting),
                (AttemptState::Starting, AttemptState::Running),
                (AttemptState::Running, AttemptState::Collecting),
            ] {
                store
                    .compare_and_set_attempt_state(attempt_id, from, to)
                    .unwrap();
            }
            let failed = ResultEnvelope {
                outcome: TerminalOutcome::Failed,
                artifacts: vec![],
                error: Some("pre-spawn fixture failure".to_owned()),
                ..result(&task, attempt_id)
            };
            store
                .commit_terminal_result(&task.owner_id, &failed)
                .unwrap();
            if number == 0 {
                store.grant_pre_spawn_retry(attempt_id).unwrap();
            }
        }
        assert!(matches!(
            store.claim_attempt(task.task_id, task.revision, AttemptId::new()),
            Err(StoreError::AttemptBudgetExhausted { .. })
        ));
    }

    #[test]
    fn admitted_task_is_visible_and_lost_attempt_cannot_relaunch() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "admission-test").unwrap();
        assert_eq!(store.unstarted_tasks().unwrap(), vec![task.clone()]);
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        assert!(store.unstarted_tasks().unwrap().is_empty());
        store
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        let lost = ResultEnvelope {
            outcome: TerminalOutcome::Lost,
            error: Some("supervisor disappeared".to_owned()),
            legacy_embedded_route_observation: None,
            route_observation: None,
            unresolved_effects: vec!["execution identity unknown".to_owned()],
            ..result(&task, attempt_id)
        };
        store.commit_terminal_result(&task.owner_id, &lost).unwrap();
        assert!(matches!(
            store.claim_attempt(task.task_id, task.revision, AttemptId::new()),
            Err(StoreError::UnresolvedPriorAttempt { .. })
        ));
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    #[test]
    fn board_rows_order_latest_revision_and_skip_malformed_history() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let mut recorded = Vec::new();
        for index in 0..21 {
            let mut item = task();
            item.objective = format!("SECRET_OBJECTIVE_{index}");
            item.create_request_id = format!("board-req-{index}");
            item.workspace = format!("/tmp/project-{index}");
            store
                .record_task(&item, &format!("board-digest-{index}"))
                .unwrap();
            recorded.push(item);
        }
        let mut revised = recorded[0].clone();
        revised.revision = 2;
        revised.create_request_id = "board-req-0-r2".to_owned();
        store.record_task(&revised, "board-digest-0-r2").unwrap();

        let bad_id = TaskId::new();
        store
            .connection
            .execute(
                "INSERT INTO tasks
                 (task_id, revision, owner_id, create_request_id, request_digest, spec_json)
                 VALUES (?1, 1, 'codex:test-owner', 'malformed-board', 'digest',
                         '{\"objective\":\"SECRET_MALFORMED_BYTES\"}')",
                [bad_id.to_string()],
            )
            .unwrap();

        let count = |store: &Store| {
            store
                .connection
                .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        let before = count(&store);
        let rows = BoardStore::open_existing(root.path())
            .unwrap()
            .rows(20)
            .unwrap();
        assert!(
            BoardStore::open_existing(root.path())
                .unwrap()
                .rows(0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(count(&store), before);
        assert_eq!(rows.len(), 20);
        assert_eq!(rows[0].task_id, recorded[0].task_id);
        assert_eq!(rows[0].revision, 2);
        assert!(rows.iter().all(|row| row.task_id != recorded[1].task_id));
        assert!(rows.iter().all(|row| row.task_id != bad_id));
        let rendered = format!("{rows:?}");
        assert!(!rendered.contains("SECRET_OBJECTIVE"));
        assert!(!rendered.contains("SECRET_MALFORMED_BYTES"));
        assert!(!rendered.contains("reviewable report"));
    }

    #[test]
    fn board_rows_join_latest_result_decision_and_survive_writer_lock() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let decided_task = task();
        store.record_task(&decided_task, "board-terminal").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(decided_task.task_id, decided_task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &decided_task, attempt_id);
        store
            .commit_terminal_result(&decided_task.owner_id, &result)
            .unwrap();
        store
            .bind_owner(&decided_task.owner_id, "session-a", 1)
            .unwrap();
        store
            .record_decision(&Decision {
                schema: SCHEMA_V1.to_owned(),
                decision_id: DecisionId::new(),
                owner_id: decided_task.owner_id.clone(),
                task_id: decided_task.task_id,
                revision: decided_task.revision,
                result_id: result.result_id,
                result_digest: Store::result_digest(&result).unwrap(),
                session_id: Some("session-a".to_owned()),
                binding_epoch: Some(1),
                verdict: DecisionVerdict::Accepted,
                reason: "SECRET_REASON_NOT_FOR_BOARD".to_owned(),
            })
            .unwrap();

        let mut other = task();
        other.create_request_id = "board-other".to_owned();
        store.record_task(&other, "board-other").unwrap();
        let other_attempt = AttemptId::new();
        store
            .claim_attempt(other.task_id, other.revision, other_attempt)
            .unwrap();
        let other_result = sealed_result(&store, &other, other_attempt);
        store
            .commit_terminal_result(&other.owner_id, &other_result)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE results SET envelope_json = 'not-json' WHERE result_id = ?1",
                [other_result.result_id.to_string()],
            )
            .unwrap();
        let rows = BoardStore::open_existing(root.path())
            .unwrap()
            .rows(20)
            .unwrap();
        assert_eq!(rows.len(), 2);
        let decided = rows
            .iter()
            .find(|row| row.task_id == decided_task.task_id)
            .unwrap();
        let broken = rows
            .iter()
            .find(|row| row.task_id == other.task_id)
            .unwrap();
        assert_eq!(decided.result_outcome, Some(TerminalOutcome::Candidate));
        assert_eq!(decided.decision_verdict, Some(DecisionVerdict::Accepted));
        assert_eq!(decided.attempt_state, AttemptState::Terminal);
        assert_eq!(broken.result_outcome, None);
        assert!(!format!("{rows:?}").contains("SECRET_REASON_NOT_FOR_BOARD"));
        assert!(!format!("{rows:?}").contains("reviewable report"));

        drop(store);
        let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let locked_rows = BoardStore::open_existing(root.path())
            .unwrap()
            .rows(20)
            .unwrap();
        assert_eq!(locked_rows.len(), 2);
        assert!(locked_rows.iter().any(|row| {
            row.decision_verdict == Some(DecisionVerdict::Accepted)
                && row.result_outcome == Some(TerminalOutcome::Candidate)
        }));
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn board_rows_retry_across_concurrent_task_writes() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let seed = task();
        store.record_task(&seed, "board-seed").unwrap();
        let path = root.path().to_path_buf();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut writer = Store::open(&writer_path).unwrap();
            for index in 0..24 {
                let mut item = task();
                item.create_request_id = format!("concurrent-{index}");
                writer
                    .record_task(&item, &format!("concurrent-digest-{index}"))
                    .unwrap();
            }
        });
        let board = BoardStore::open_existing(&path).unwrap();
        for _ in 0..32 {
            board.rows(20).unwrap();
        }
        writer.join().unwrap();
        assert!(board.rows(20).unwrap().len() <= 20);
        assert!(!board.rows(20).unwrap().is_empty());
    }

    #[test]
    fn open_applies_remaining_schema_when_tasks_already_exist() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path()).unwrap();
        let connection = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE tasks (
                    task_id TEXT NOT NULL,
                    revision INTEGER NOT NULL,
                    owner_id TEXT NOT NULL,
                    create_request_id TEXT NOT NULL UNIQUE,
                    request_digest TEXT NOT NULL,
                    spec_json TEXT NOT NULL,
                    PRIMARY KEY (task_id, revision)
                );",
            )
            .unwrap();
        drop(connection);
        let mut store = Store::open(root.path()).unwrap();
        let present = |name: &str| {
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    [name],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(present("owner_bindings"), 1);
        assert_eq!(present("decisions"), 1);
        assert_eq!(present("one_active_attempt_per_revision"), 1);
        store.record_task(&task(), "partial-schema").unwrap();
    }

    #[test]
    fn board_rows_omit_invalid_attempt_state_and_decision_without_valid_result() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();

        let invalid_state = task();
        store
            .record_task(&invalid_state, "board-invalid-state")
            .unwrap();
        let invalid_attempt = AttemptId::new();
        store
            .claim_attempt(
                invalid_state.task_id,
                invalid_state.revision,
                invalid_attempt,
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE attempts SET state = 'not-a-real-state' WHERE attempt_id = ?1",
                [invalid_attempt.to_string()],
            )
            .unwrap();

        let mut decided = task();
        decided.create_request_id = "board-orphan-decision".to_owned();
        store
            .record_task(&decided, "board-orphan-decision")
            .unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(decided.task_id, decided.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &decided, attempt_id);
        store
            .commit_terminal_result(&decided.owner_id, &result)
            .unwrap();
        store.bind_owner(&decided.owner_id, "session-a", 1).unwrap();
        store
            .record_decision(&Decision {
                schema: SCHEMA_V1.to_owned(),
                decision_id: DecisionId::new(),
                owner_id: decided.owner_id.clone(),
                task_id: decided.task_id,
                revision: decided.revision,
                result_id: result.result_id,
                result_digest: Store::result_digest(&result).unwrap(),
                session_id: Some("session-a".to_owned()),
                binding_epoch: Some(1),
                verdict: DecisionVerdict::Accepted,
                reason: "SECRET_ORPHAN_REASON".to_owned(),
            })
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE results SET envelope_json = 'not-json' WHERE result_id = ?1",
                [result.result_id.to_string()],
            )
            .unwrap();

        let rows = BoardStore::open_existing(root.path())
            .unwrap()
            .rows(20)
            .unwrap();
        assert!(rows.iter().all(|row| row.task_id != invalid_state.task_id));
        let broken = rows
            .iter()
            .find(|row| row.task_id == decided.task_id)
            .unwrap();
        assert_eq!(broken.result_outcome, None);
        assert_eq!(broken.decision_verdict, None);
        assert!(!format!("{rows:?}").contains("SECRET_ORPHAN_REASON"));

        let mut forged = result.clone();
        forged.task_id = TaskId::new();
        store
            .connection
            .execute(
                "UPDATE results SET envelope_json = ?1 WHERE result_id = ?2",
                params![
                    serde_json::to_string(&forged).unwrap(),
                    result.result_id.to_string()
                ],
            )
            .unwrap();
        let rows = BoardStore::open_existing(root.path())
            .unwrap()
            .rows(20)
            .unwrap();
        let forged = rows
            .iter()
            .find(|row| row.task_id == decided.task_id)
            .unwrap();
        assert_eq!(forged.result_outcome, None);
        assert_eq!(forged.decision_verdict, None);
    }

    #[test]
    fn recursive_delegation_edges_bind_each_child_to_its_live_parent_attempt() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let root_task = task();
        store.record_task(&root_task, "root-digest").unwrap();
        let root_attempt = AttemptId::new();
        store
            .claim_attempt(root_task.task_id, 1, root_attempt)
            .unwrap();
        store
            .compare_and_set_attempt_state(
                root_attempt,
                AttemptState::Queued,
                AttemptState::Starting,
            )
            .unwrap();
        store
            .compare_and_set_attempt_state(
                root_attempt,
                AttemptState::Starting,
                AttemptState::Running,
            )
            .unwrap();

        let mut child = task();
        child.create_request_id = "request-child".to_owned();
        child.owner_id = OwnerId::new(format!("worker:{root_attempt}")).unwrap();
        store
            .record_child_task(&child, "child-digest", root_task.task_id, root_attempt)
            .unwrap();
        assert_eq!(
            store.delegation_parent(child.task_id).unwrap(),
            Some((root_task.task_id, root_attempt, 1))
        );
        let child_attempt = AttemptId::new();
        store
            .claim_attempt(child.task_id, 1, child_attempt)
            .unwrap();
        store
            .compare_and_set_attempt_state(
                child_attempt,
                AttemptState::Queued,
                AttemptState::Starting,
            )
            .unwrap();
        store
            .compare_and_set_attempt_state(
                child_attempt,
                AttemptState::Starting,
                AttemptState::Running,
            )
            .unwrap();

        let mut grandchild = task();
        grandchild.create_request_id = "request-grandchild".to_owned();
        grandchild.owner_id = OwnerId::new(format!("worker:{child_attempt}")).unwrap();
        store
            .record_child_task(
                &grandchild,
                "grandchild-digest",
                child.task_id,
                child_attempt,
            )
            .unwrap();
        assert_eq!(
            store.delegation_parent(grandchild.task_id).unwrap(),
            Some((child.task_id, child_attempt, 2))
        );
        let mut wrong_owner = task();
        wrong_owner.create_request_id = "request-wrong-owner".to_owned();
        assert!(matches!(
            store.record_child_task(&wrong_owner, "wrong-owner", child.task_id, child_attempt),
            Err(StoreError::InvalidDelegationParent)
        ));
        assert!(matches!(
            store.record_child_task(
                &wrong_owner,
                "missing-attempt",
                child.task_id,
                AttemptId::new()
            ),
            Err(StoreError::InvalidDelegationParent)
        ));
        assert!(matches!(
            store.task(wrong_owner.task_id),
            Err(StoreError::TaskNotFound(_))
        ));
    }

    #[test]
    fn child_revision_keeps_its_parent_edge_and_rejects_unparented_write() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let parent = task();
        store.record_task(&parent, "parent").unwrap();
        let parent_attempt = AttemptId::new();
        store
            .claim_attempt(parent.task_id, 1, parent_attempt)
            .unwrap();
        store
            .compare_and_set_attempt_state(
                parent_attempt,
                AttemptState::Queued,
                AttemptState::Starting,
            )
            .unwrap();
        store
            .compare_and_set_attempt_state(
                parent_attempt,
                AttemptState::Starting,
                AttemptState::Running,
            )
            .unwrap();
        let mut child = task();
        child.create_request_id = "child-first".to_owned();
        child.owner_id = OwnerId::new(format!("worker:{parent_attempt}")).unwrap();
        store
            .record_child_task(&child, "child-first", parent.task_id, parent_attempt)
            .unwrap();
        let mut revised = child.clone();
        revised.revision = 2;
        revised.create_request_id = "child-revised".to_owned();
        assert!(matches!(
            store.record_task(&revised, "child-revised"),
            Err(StoreError::InvalidDelegationParent)
        ));
        store
            .record_child_task(&revised, "child-revised", parent.task_id, parent_attempt)
            .unwrap();
        assert_eq!(store.task(child.task_id).unwrap().revision, 2);
        assert_eq!(
            store.delegation_parent(child.task_id).unwrap(),
            Some((parent.task_id, parent_attempt, 1))
        );
    }

    #[test]
    fn ninth_delegation_edge_is_rejected_without_recording_a_task() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let mut parent = task();
        store.record_task(&parent, "root").unwrap();
        let mut parent_attempt = AttemptId::new();
        for depth in 1..=9 {
            store
                .claim_attempt(parent.task_id, 1, parent_attempt)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    parent_attempt,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    parent_attempt,
                    AttemptState::Starting,
                    AttemptState::Running,
                )
                .unwrap();
            let mut child = task();
            child.create_request_id = format!("depth-{depth}");
            child.owner_id = OwnerId::new(format!("worker:{parent_attempt}")).unwrap();
            let outcome = store.record_child_task(
                &child,
                &format!("digest-{depth}"),
                parent.task_id,
                parent_attempt,
            );
            if depth == 9 {
                assert!(matches!(outcome, Err(StoreError::DelegationDepthExceeded)));
                assert!(matches!(
                    store.task(child.task_id),
                    Err(StoreError::TaskNotFound(_))
                ));
            } else {
                outcome.unwrap();
                parent = child;
                parent_attempt = AttemptId::new();
            }
        }
    }

    fn task() -> TaskSpec {
        TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            create_request_id: "request-1".to_owned(),
            owner_id: OwnerId::new("codex:test-owner").unwrap(),
            objective: "Store one bounded result".to_owned(),
            workspace: "/tmp/brgr-test".to_owned(),
            route: Route {
                harness_id: "local.fixture".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec!["completion".to_owned()],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
            },
            acceptance_criteria: vec!["result is sealed".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 30,
                max_attempts: 2,
            },
        }
    }

    fn result(task: &TaskSpec, attempt_id: AttemptId) -> ResultEnvelope {
        ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id: task.task_id,
            revision: task.revision,
            attempt_id,
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![],
            error: None,
            legacy_embedded_route_observation: None,
            route_observation: None,
            unresolved_effects: vec![],
        }
    }

    fn sealed_result(store: &Store, task: &TaskSpec, attempt_id: AttemptId) -> ResultEnvelope {
        let mut envelope = result(task, attempt_id);
        envelope.artifacts.push(
            store
                .seal_artifact_reader(
                    std::io::Cursor::new(b"reviewable report"),
                    &task.artifact_contract.media_type,
                    task.artifact_contract.max_bytes,
                )
                .unwrap(),
        );
        envelope
    }

    #[test]
    fn native_route_receipt_commits_atomically_without_changing_result_digest() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "native-route").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let mut result = sealed_result(&store, &task, attempt_id);
        let legacy_digest = Store::result_digest(&result).unwrap();
        let observation = RouteObservation {
            model: Some("workbuddy/deepseek-v4.1-flash".to_owned()),
            model_source: ObservationSource::HarnessJsonl,
            effort: None,
            effort_source: ObservationSource::Unavailable,
        };
        result.route_observation = Some(observation.clone());
        assert_eq!(Store::result_digest(&result).unwrap(), legacy_digest);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert_eq!(
            store.route_observation(result.result_id).unwrap(),
            Some(observation)
        );
        let reopened = store.latest_result(task.task_id).unwrap();
        assert!(reopened.route_observation.is_none());
        assert_eq!(Store::result_digest(&reopened).unwrap(), legacy_digest);
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);

        result.route_observation.as_mut().unwrap().model = Some("other/fallback".to_owned());
        assert!(matches!(
            store.commit_terminal_result(&task.owner_id, &result),
            Err(StoreError::RouteObservationConflict(_))
        ));
        store
            .connection
            .execute(
                "UPDATE route_observations SET observation_json = '{}' WHERE result_id = ?1",
                [result.result_id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.route_observation(result.result_id),
            Err(StoreError::RouteObservationIntegrityMismatch)
        ));
    }

    #[test]
    fn unreleased_embedded_route_result_retains_its_original_decision_digest() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        store.record_task(&task, "intermediate-result").unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let mut result = sealed_result(&store, &task, attempt_id);
        result.legacy_embedded_route_observation = Some(RouteObservation {
            model: Some("workbuddy/deepseek-v4.1-flash".to_owned()),
            model_source: ObservationSource::HarnessJsonl,
            effort: None,
            effort_source: ObservationSource::Unavailable,
        });
        let original_digest = Store::result_digest(&result).unwrap();
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        assert!(store.route_observation(result.result_id).unwrap().is_none());
        let reopened = store.latest_result(task.task_id).unwrap();
        assert_eq!(Store::result_digest(&reopened).unwrap(), original_digest);
        assert_eq!(
            reopened.legacy_embedded_route_observation,
            result.legacy_embedded_route_observation
        );
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
        store
            .record_decision_and_ack(&Decision {
                schema: SCHEMA_V1.to_owned(),
                decision_id: DecisionId::new(),
                owner_id: task.owner_id.clone(),
                task_id: task.task_id,
                revision: task.revision,
                result_id: result.result_id,
                result_digest: original_digest,
                session_id: Some("session-a".to_owned()),
                binding_epoch: Some(1),
                verdict: DecisionVerdict::Accepted,
                reason: "legacy observation verified".to_owned(),
            })
            .unwrap();
        assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    }
}
