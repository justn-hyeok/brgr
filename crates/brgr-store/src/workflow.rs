//! Durable workflow runs and the steps they launch.
//!
//! A workflow run owns steps. Each step is one delegated task. The script that
//! launches those tasks may re-execute from the top after a crash, so recording
//! a step is idempotent: the same key and input digest returns the existing
//! task, and the step row is written in the same transaction as the task so a
//! crash cannot leave one without the other.

use std::fmt;

use brgr_protocol::{OwnerId, TaskId, TaskSpec};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::{Store, StoreError, task::record_task_in_transaction, validate_digest};

const RUN_ID_MAX: usize = 64;
const STEP_KEY_MAX: usize = 128;

/// Stable identity of one workflow run.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkflowRunId(String);

impl WorkflowRunId {
    /// A run id is 1..=64 characters of `A-Za-z0-9_-`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidWorkflowRunId`] when the value is empty,
    /// too long, or contains a character outside the allowed set.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if !valid_token(&value, RUN_ID_MAX) {
            return Err(StoreError::InvalidWorkflowRunId(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The script's own name for one step inside a run.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkflowStepKey(String);

impl WorkflowStepKey {
    /// A step key is 1..=128 characters of `A-Za-z0-9_-`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidWorkflowStepKey`] when the value is empty,
    /// too long, or contains a character outside the allowed set.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if !valid_token(&value, STEP_KEY_MAX) {
            return Err(StoreError::InvalidWorkflowStepKey(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowStepKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Where one workflow run has got to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowRunState {
    Running,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowRunState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StoreError::InvalidWorkflowRunState(other.to_owned())),
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// `running <-> blocked`, and `running|blocked -> completed|failed|cancelled`.
    #[must_use]
    pub fn allows(self, to: Self) -> bool {
        matches!(
            (self, to),
            (
                Self::Running,
                Self::Blocked | Self::Completed | Self::Failed | Self::Cancelled
            ) | (
                Self::Blocked,
                Self::Running | Self::Completed | Self::Failed | Self::Cancelled
            )
        )
    }
}

/// One recorded workflow run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRun {
    pub run_id: WorkflowRunId,
    pub owner_id: OwnerId,
    pub script_digest: String,
    pub max_parallel: u32,
    pub state: WorkflowRunState,
    pub created_at: i64,
}

/// One recorded step of a workflow run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowStep {
    pub run_id: WorkflowRunId,
    pub step_key: WorkflowStepKey,
    pub input_digest: String,
    pub task_id: TaskId,
    pub revision: u32,
}

/// Whether recording a step inserted a task or found the one already journaled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowStepOutcome {
    Created { task_id: TaskId, revision: u32 },
    Existing { task_id: TaskId, revision: u32 },
}

impl Store {
    /// Records a workflow run. Repeating the same id with the same fields
    /// returns the existing run; a different owner, script digest, or
    /// `max_parallel` is a conflict.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WorkflowRunConflict`] when the id is already
    /// stored with different fields, [`StoreError::InvalidWorkflowMaxParallel`]
    /// when the cap is outside 1..=64, or a digest or database failure.
    pub fn create_workflow_run(
        &self,
        run_id: &WorkflowRunId,
        owner: &OwnerId,
        script_digest: &str,
        max_parallel: u32,
    ) -> Result<WorkflowRun, StoreError> {
        validate_digest(script_digest)?;
        if !(1..=64).contains(&max_parallel) {
            return Err(StoreError::InvalidWorkflowMaxParallel);
        }
        let transaction = self.write_transaction()?;
        if let Some(existing) = load_run_optional(&transaction, run_id)? {
            if existing.owner_id == *owner
                && existing.script_digest == script_digest
                && existing.max_parallel == max_parallel
            {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreError::WorkflowRunConflict(run_id.clone()));
        }
        transaction.execute(
            "INSERT INTO workflow_runs
             (run_id, owner_id, script_digest, max_parallel, state, created_at)
             VALUES (?1, ?2, ?3, ?4, 'running', CAST(strftime('%s','now') AS INTEGER))",
            params![
                run_id.as_str(),
                owner.as_str(),
                script_digest,
                i64::from(max_parallel),
            ],
        )?;
        let run = load_run(&transaction, run_id)?;
        transaction.commit()?;
        Ok(run)
    }

    /// Journals a step and its task in one transaction.
    ///
    /// An existing step with the same input digest returns the stored task and
    /// writes nothing. A different digest is refused. A run at its parallel
    /// cap is a retryable refusal, not a failure of the run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WorkflowRunNotFound`] or
    /// [`StoreError::WorkflowRunNotRunning`] when the run cannot accept a
    /// step, [`StoreError::WorkflowStepDiverged`] when the key is reused with
    /// a different digest, [`StoreError::WorkflowParallelLimit`] when too many
    /// steps still lack a terminal result, or the same errors as recording a
    /// task.
    pub fn record_workflow_step(
        &mut self,
        run_id: &WorkflowRunId,
        step_key: &WorkflowStepKey,
        input_digest: &str,
        task: &TaskSpec,
        request_digest: &str,
    ) -> Result<WorkflowStepOutcome, StoreError> {
        validate_digest(input_digest)?;
        let transaction = self.write_transaction()?;
        let outcome = record_step_in_transaction(
            &transaction,
            run_id,
            step_key,
            input_digest,
            task,
            request_digest,
        )?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Every step of a run, in the order they were inserted.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WorkflowRunNotFound`] when the run is missing, or
    /// a database failure.
    pub fn workflow_steps(&self, run_id: &WorkflowRunId) -> Result<Vec<WorkflowStep>, StoreError> {
        if load_run_optional(&self.connection, run_id)?.is_none() {
            return Err(StoreError::WorkflowRunNotFound(run_id.clone()));
        }
        let mut statement = self.connection.prepare(
            "SELECT run_id, step_key, input_digest, task_id, revision
             FROM workflow_steps WHERE run_id = ?1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([run_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (stored_run, stored_key, input_digest, task_id, revision) = row?;
            Ok(WorkflowStep {
                run_id: WorkflowRunId::new(stored_run)?,
                step_key: WorkflowStepKey::new(stored_key)?,
                input_digest,
                task_id: parse_task_id(&task_id)?,
                revision: u32::try_from(revision).map_err(|_| StoreError::NumericOverflow)?,
            })
        })
        .collect()
    }

    /// Moves a run between the states the journal allows.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WorkflowRunNotFound`] when the run is missing, or
    /// [`StoreError::WorkflowRunTerminal`] when the run has already completed,
    /// failed, or been cancelled.
    pub fn set_workflow_run_state(
        &self,
        run_id: &WorkflowRunId,
        state: WorkflowRunState,
    ) -> Result<WorkflowRun, StoreError> {
        let transaction = self.write_transaction()?;
        let current = load_run(&transaction, run_id)?;
        if current.state.is_terminal() {
            return Err(StoreError::WorkflowRunTerminal(run_id.clone()));
        }
        if current.state != state {
            if !current.state.allows(state) {
                return Err(StoreError::WorkflowRunTerminal(run_id.clone()));
            }
            transaction.execute(
                "UPDATE workflow_runs SET state = ?1 WHERE run_id = ?2",
                params![state.as_str(), run_id.as_str()],
            )?;
        }
        let run = load_run(&transaction, run_id)?;
        transaction.commit()?;
        Ok(run)
    }
}

fn record_step_in_transaction(
    transaction: &Transaction<'_>,
    run_id: &WorkflowRunId,
    step_key: &WorkflowStepKey,
    input_digest: &str,
    task: &TaskSpec,
    request_digest: &str,
) -> Result<WorkflowStepOutcome, StoreError> {
    let run = load_run(transaction, run_id)?;
    if run.state != WorkflowRunState::Running {
        return Err(StoreError::WorkflowRunNotRunning(run_id.clone()));
    }
    if task.owner_id != run.owner_id {
        return Err(StoreError::WorkflowStepOwnerMismatch(run_id.clone()));
    }
    if let Some(existing) = load_step(transaction, run_id, step_key)? {
        if existing.input_digest == input_digest {
            return Ok(WorkflowStepOutcome::Existing {
                task_id: existing.task_id,
                revision: existing.revision,
            });
        }
        return Err(StoreError::WorkflowStepDiverged {
            run_id: run_id.clone(),
            step_key: step_key.clone(),
        });
    }
    let unfinished = unfinished_step_count(transaction, run_id)?;
    if unfinished >= i64::from(run.max_parallel) {
        return Err(StoreError::WorkflowParallelLimit);
    }
    task.validate()?;
    validate_digest(request_digest)?;
    record_task_in_transaction(transaction, task, request_digest, None, None)?;
    transaction.execute(
        "INSERT INTO workflow_steps (run_id, step_key, input_digest, task_id, revision)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            run_id.as_str(),
            step_key.as_str(),
            input_digest,
            task.task_id.to_string(),
            task.revision,
        ],
    )?;
    Ok(WorkflowStepOutcome::Created {
        task_id: task.task_id,
        revision: task.revision,
    })
}

fn unfinished_step_count(
    transaction: &Transaction<'_>,
    run_id: &WorkflowRunId,
) -> Result<i64, StoreError> {
    transaction
        .query_row(
            "SELECT COUNT(*) FROM workflow_steps s
             WHERE s.run_id = ?1
               AND NOT EXISTS (
                 SELECT 1 FROM results r
                 WHERE r.task_id = s.task_id AND r.revision = s.revision
               )",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn load_run(connection: &Connection, run_id: &WorkflowRunId) -> Result<WorkflowRun, StoreError> {
    load_run_optional(connection, run_id)?
        .ok_or_else(|| StoreError::WorkflowRunNotFound(run_id.clone()))
}

fn load_run_optional(
    connection: &Connection,
    run_id: &WorkflowRunId,
) -> Result<Option<WorkflowRun>, StoreError> {
    connection
        .query_row(
            "SELECT run_id, owner_id, script_digest, max_parallel, state, created_at
             FROM workflow_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?
        .map(
            |(id, owner_id, script_digest, max_parallel, state, created_at)| {
                Ok(WorkflowRun {
                    run_id: WorkflowRunId::new(id)?,
                    owner_id: OwnerId::new(owner_id)?,
                    script_digest,
                    max_parallel: u32::try_from(max_parallel)
                        .map_err(|_| StoreError::NumericOverflow)?,
                    state: WorkflowRunState::parse(&state)?,
                    created_at,
                })
            },
        )
        .transpose()
}

fn load_step(
    connection: &Connection,
    run_id: &WorkflowRunId,
    step_key: &WorkflowStepKey,
) -> Result<Option<WorkflowStep>, StoreError> {
    connection
        .query_row(
            "SELECT run_id, step_key, input_digest, task_id, revision
             FROM workflow_steps WHERE run_id = ?1 AND step_key = ?2",
            params![run_id.as_str(), step_key.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .map(|(id, key, input_digest, task_id, revision)| {
            Ok(WorkflowStep {
                run_id: WorkflowRunId::new(id)?,
                step_key: WorkflowStepKey::new(key)?,
                input_digest,
                task_id: parse_task_id(&task_id)?,
                revision: u32::try_from(revision).map_err(|_| StoreError::NumericOverflow)?,
            })
        })
        .transpose()
}

fn parse_task_id(value: &str) -> Result<TaskId, StoreError> {
    value
        .parse()
        .map_err(|_| StoreError::InvalidWorkflowRunId(value.to_owned()))
}

fn valid_token(value: &str, max_len: usize) -> bool {
    (1..=max_len).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_and_step_key_reject_empty_overlong_or_foreign_characters() {
        assert!(WorkflowRunId::new("run-1").is_ok());
        assert!(WorkflowRunId::new("A").is_ok());
        assert!(WorkflowRunId::new("x".repeat(64)).is_ok());
        assert!(matches!(
            WorkflowRunId::new(""),
            Err(StoreError::InvalidWorkflowRunId(_))
        ));
        assert!(matches!(
            WorkflowRunId::new("x".repeat(65)),
            Err(StoreError::InvalidWorkflowRunId(_))
        ));
        assert!(matches!(
            WorkflowRunId::new("run/1"),
            Err(StoreError::InvalidWorkflowRunId(_))
        ));
        assert!(WorkflowStepKey::new("x".repeat(128)).is_ok());
        assert!(matches!(
            WorkflowStepKey::new("x".repeat(129)),
            Err(StoreError::InvalidWorkflowStepKey(_))
        ));
        assert!(matches!(
            WorkflowStepKey::new("step key"),
            Err(StoreError::InvalidWorkflowStepKey(_))
        ));
    }

    #[test]
    fn the_transition_table_is_the_only_source_of_the_run_graph() {
        for from in [
            WorkflowRunState::Running,
            WorkflowRunState::Blocked,
            WorkflowRunState::Completed,
            WorkflowRunState::Failed,
            WorkflowRunState::Cancelled,
        ] {
            for to in [
                WorkflowRunState::Running,
                WorkflowRunState::Blocked,
                WorkflowRunState::Completed,
                WorkflowRunState::Failed,
                WorkflowRunState::Cancelled,
            ] {
                assert_eq!(
                    from.allows(to),
                    matches!(
                        (from, to),
                        (
                            WorkflowRunState::Running,
                            WorkflowRunState::Blocked
                                | WorkflowRunState::Completed
                                | WorkflowRunState::Failed
                                | WorkflowRunState::Cancelled
                        ) | (
                            WorkflowRunState::Blocked,
                            WorkflowRunState::Running
                                | WorkflowRunState::Completed
                                | WorkflowRunState::Failed
                                | WorkflowRunState::Cancelled
                        )
                    ),
                    "{from:?} -> {to:?}"
                );
            }
        }
        assert!(WorkflowRunState::Completed.is_terminal());
        assert!(WorkflowRunState::Failed.is_terminal());
        assert!(WorkflowRunState::Cancelled.is_terminal());
        assert!(!WorkflowRunState::Running.is_terminal());
        assert!(!WorkflowRunState::Blocked.is_terminal());
        assert!(matches!(
            WorkflowRunState::parse("working"),
            Err(StoreError::InvalidWorkflowRunState(_))
        ));
        for state in [
            WorkflowRunState::Running,
            WorkflowRunState::Blocked,
            WorkflowRunState::Completed,
            WorkflowRunState::Failed,
            WorkflowRunState::Cancelled,
        ] {
            assert_eq!(WorkflowRunState::parse(state.as_str()).unwrap(), state);
        }
    }
}
