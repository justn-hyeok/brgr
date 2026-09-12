//! Durable `SQLite` metadata and content-addressed artifact storage.

mod artifact;

use std::{
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use artifact::ArtifactStore;
use brgr_protocol::{
    ArtifactRef, AttemptId, AttemptState, Decision, InboxItem, OwnerId, ResultEnvelope, ResultId,
    SCHEMA_V1, TaskId, TaskSpec,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
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
";

/// The result of an idempotent store mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    Inserted,
    AlreadyApplied,
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
        task.validate()?;
        validate_digest(request_digest)?;
        let transaction = self.connection.transaction()?;
        let idempotency =
            record_idempotency(&transaction, &task.create_request_id, request_digest)?;
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
            transaction.commit()?;
            return Ok(WriteOutcome::AlreadyApplied);
        }

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
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Records a new attempt for an existing task revision.
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
        self.connection.execute(
            "INSERT INTO attempts (attempt_id, task_id, revision, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt_id.to_string(),
                task_id.to_string(),
                revision,
                state_name(AttemptState::Queued),
            ],
        )?;
        Ok(())
    }

    /// Updates the durable state for an attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt does not exist or storage fails.
    pub fn set_attempt_state(
        &self,
        attempt_id: AttemptId,
        state: AttemptState,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2",
            params![state_name(state), attempt_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::AttemptNotFound(attempt_id));
        }
        Ok(())
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
        validate_schema(&result.schema)?;
        let envelope_json = serde_json::to_string(result)?;
        let digest = sha256(envelope_json.as_bytes());
        let transaction = self.connection.transaction()?;

        if let Some((stored_id, stored_digest)) = transaction
            .query_row(
                "SELECT result_id, result_digest FROM results WHERE attempt_id = ?1",
                [result.attempt_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_id == result.result_id.to_string() && stored_digest == digest {
                transaction.commit()?;
                return Ok(WriteOutcome::AlreadyApplied);
            }
            return Err(StoreError::TerminalResultConflict(result.attempt_id));
        }

        let expected = transaction
            .query_row(
                "SELECT a.task_id, a.revision, t.owner_id
                 FROM attempts a
                 JOIN tasks t ON t.task_id = a.task_id AND t.revision = a.revision
                 WHERE a.attempt_id = ?1",
                [result.attempt_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(result.attempt_id))?;
        if (expected.0, expected.1) != (result.task_id.to_string(), result.revision) {
            return Err(StoreError::AttemptResultMismatch);
        }
        if expected.2 != owner_id.as_str() {
            return Err(StoreError::ResultOwnerMismatch);
        }

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
        transaction.execute(
            "INSERT INTO inbox_items (owner_id, result_id) VALUES (?1, ?2)",
            params![owner_id.as_str(), result.result_id.to_string()],
        )?;
        transaction.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2",
            params![
                state_name(AttemptState::Terminal),
                result.attempt_id.to_string()
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

    /// Records the sole accept/reject decision for a result.
    ///
    /// Exact replay is idempotent. A different decision is rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for missing results, digest/owner mismatches,
    /// conflicting decisions, invalid serialization, or database failure.
    pub fn record_decision(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        validate_schema(&decision.schema)?;
        let decision_json = serde_json::to_string(decision)?;
        if let Some(stored_json) = self
            .connection
            .query_row(
                "SELECT decision_json FROM decisions WHERE result_id = ?1",
                [decision.result_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return if stored_json == decision_json {
                Ok(WriteOutcome::AlreadyApplied)
            } else {
                Err(StoreError::DecisionConflict(decision.result_id))
            };
        }

        let (stored_digest, owner_id, task_id, revision) = self
            .connection
            .query_row(
                "SELECT r.result_digest, i.owner_id, r.task_id, r.revision
                 FROM results r JOIN inbox_items i ON i.result_id = r.result_id
                 WHERE r.result_id = ?1",
                [decision.result_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::ResultNotFound(decision.result_id))?;
        if stored_digest != decision.result_digest {
            return Err(StoreError::ResultDigestMismatch);
        }
        if owner_id != decision.owner_id.as_str() {
            return Err(StoreError::DecisionOwnerMismatch);
        }
        if task_id != decision.task_id.to_string() || revision != decision.revision {
            return Err(StoreError::DecisionResultMismatch);
        }

        self.connection.execute(
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
    #[error("attempt {0} does not exist")]
    AttemptNotFound(AttemptId),
    #[error("terminal result does not belong to its attempt")]
    AttemptResultMismatch,
    #[error("terminal result must be delivered to the task owner")]
    ResultOwnerMismatch,
    #[error("attempt {0} already has a different terminal result")]
    TerminalResultConflict(AttemptId),
    #[error("result {0} does not exist")]
    ResultNotFound(ResultId),
    #[error("result digest does not match the sealed result")]
    ResultDigestMismatch,
    #[error("only the result inbox owner may decide")]
    DecisionOwnerMismatch,
    #[error("decision does not belong to its result task revision")]
    DecisionResultMismatch,
    #[error("result {0} already has a different decision")]
    DecisionConflict(ResultId),
    #[error("the owner inbox item does not exist")]
    InboxItemNotFound,
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
}

#[cfg(test)]
mod tests {
    use brgr_protocol::{
        ArtifactContract, AttemptBudget, DecisionId, DecisionVerdict, Route, SCHEMA_V1,
        TerminalOutcome,
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
        let result = result(&task, attempt_id);

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
    fn one_decision_is_bound_to_owner_and_result_digest() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "digest-1").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = result(&task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
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
            unresolved_effects: vec![],
        }
    }
}
