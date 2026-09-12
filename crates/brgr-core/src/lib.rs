//! Managed task orchestration across protocol, runner, and store boundaries.
//!
//! This crate owns domain invariants. Persistence and process adapters may
//! report observations, but they cannot bypass task revision or attempt state
//! validation.

use brgr_protocol::{
    AttemptId, AttemptState, ProtocolError, ResultEnvelope, ResultId, TaskId, TaskSpec,
    TerminalOutcome,
};

/// An immutable, validated snapshot of a task specification.
///
/// Revisions are replaced by constructing a new `TaskRevision`; an attempt
/// always retains the exact snapshot from which it was created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRevision {
    spec: TaskSpec,
}

impl TaskRevision {
    /// Validates and freezes a task specification.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidRevision`] for revision zero, or forwards a
    /// protocol validation error for an invalid task contract.
    pub fn new(spec: TaskSpec) -> Result<Self, CoreError> {
        spec.validate()?;
        if spec.revision == 0 {
            return Err(CoreError::InvalidRevision(0));
        }
        Ok(Self { spec })
    }

    /// Creates the next immutable revision for the same task.
    ///
    /// # Errors
    ///
    /// Returns an error when the replacement changes task identity, skips a
    /// revision, or fails task validation.
    pub fn revise(&self, replacement: TaskSpec) -> Result<Self, CoreError> {
        if replacement.task_id != self.spec.task_id {
            return Err(CoreError::RevisionTaskMismatch {
                expected: self.spec.task_id,
                actual: replacement.task_id,
            });
        }

        let expected = self
            .spec
            .revision
            .checked_add(1)
            .ok_or(CoreError::RevisionOverflow)?;
        if replacement.revision != expected {
            return Err(CoreError::RevisionNotNext {
                current: self.spec.revision,
                attempted: replacement.revision,
            });
        }

        Self::new(replacement)
    }

    #[must_use]
    pub fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    #[must_use]
    pub fn task_id(&self) -> TaskId {
        self.spec.task_id
    }

    #[must_use]
    pub fn revision(&self) -> u32 {
        self.spec.revision
    }
}

/// A single bounded execution attempt tied to one immutable task revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attempt {
    id: AttemptId,
    number: u8,
    task: TaskRevision,
    state: AttemptState,
    terminal_result: Option<ResultEnvelope>,
}

impl Attempt {
    /// Creates a queued attempt within the task's declared attempt budget.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidAttemptNumber`] when `number` is zero or
    /// exceeds the task's maximum number of attempts.
    pub fn new(task: TaskRevision, id: AttemptId, number: u8) -> Result<Self, CoreError> {
        let max = task.spec.budget.max_attempts;
        if number == 0 || number > max {
            return Err(CoreError::InvalidAttemptNumber { number, max });
        }

        Ok(Self {
            id,
            number,
            task,
            state: AttemptState::Queued,
            terminal_result: None,
        })
    }

    #[must_use]
    pub fn id(&self) -> AttemptId {
        self.id
    }

    #[must_use]
    pub fn number(&self) -> u8 {
        self.number
    }

    #[must_use]
    pub fn task(&self) -> &TaskRevision {
        &self.task
    }

    #[must_use]
    pub fn state(&self) -> AttemptState {
        self.state
    }

    #[must_use]
    pub fn terminal_result(&self) -> Option<&ResultEnvelope> {
        self.terminal_result.as_ref()
    }

    /// Applies a legal non-terminal state transition.
    ///
    /// Terminal state can only be entered with [`Attempt::record_terminal`],
    /// ensuring that every terminal attempt owns a result envelope.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the requested transition is not part of the
    /// v1 state machine.
    pub fn transition(&mut self, next: AttemptState) -> Result<(), CoreError> {
        if next == AttemptState::Terminal {
            return Err(CoreError::TerminalRequiresResult);
        }
        if let Some(result) = &self.terminal_result {
            return Err(CoreError::AttemptAlreadyTerminal {
                result_id: result.result_id,
            });
        }
        if self.state == AttemptState::Terminal {
            return Err(CoreError::TerminalInvariantViolated);
        }

        let legal = match self.state {
            AttemptState::Queued => {
                matches!(next, AttemptState::Starting | AttemptState::CancelRequested)
            }
            AttemptState::Starting => {
                matches!(next, AttemptState::Running | AttemptState::CancelRequested)
            }
            AttemptState::Running => matches!(
                next,
                AttemptState::Blocked | AttemptState::Collecting | AttemptState::CancelRequested
            ),
            AttemptState::Blocked => matches!(
                next,
                AttemptState::Running | AttemptState::Collecting | AttemptState::CancelRequested
            ),
            AttemptState::Collecting => next == AttemptState::CancelRequested,
            AttemptState::CancelRequested | AttemptState::Terminal => false,
        };

        if !legal {
            return Err(CoreError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.state = next;
        Ok(())
    }

    /// Records the first valid terminal result for this attempt.
    ///
    /// Duplicate or late terminal events never replace the first result.
    /// Result identity is checked against the attempt's frozen task revision.
    ///
    /// # Errors
    ///
    /// Returns a typed identity, outcome, or first-writer conflict error.
    pub fn record_terminal(&mut self, result: ResultEnvelope) -> Result<(), CoreError> {
        if let Some(existing) = &self.terminal_result {
            return Err(CoreError::TerminalResultAlreadyRecorded {
                existing: existing.result_id,
                attempted: result.result_id,
            });
        }

        if result.task_id != self.task.task_id() {
            return Err(CoreError::ResultTaskMismatch {
                expected: self.task.task_id(),
                actual: result.task_id,
            });
        }
        if result.revision != self.task.revision() {
            return Err(CoreError::ResultRevisionMismatch {
                expected: self.task.revision(),
                actual: result.revision,
            });
        }
        if result.attempt_id != self.id {
            return Err(CoreError::ResultAttemptMismatch {
                expected: self.id,
                actual: result.attempt_id,
            });
        }

        if !outcome_allowed(self.state, result.outcome) {
            return Err(CoreError::OutcomeNotAllowed {
                state: self.state,
                outcome: result.outcome,
            });
        }

        self.state = AttemptState::Terminal;
        self.terminal_result = Some(result);
        Ok(())
    }
}

fn outcome_allowed(state: AttemptState, outcome: TerminalOutcome) -> bool {
    match outcome {
        TerminalOutcome::Candidate => state == AttemptState::Collecting,
        TerminalOutcome::Cancelled => state == AttemptState::CancelRequested,
        TerminalOutcome::Failed | TerminalOutcome::Lost => matches!(
            state,
            AttemptState::Starting
                | AttemptState::Running
                | AttemptState::Blocked
                | AttemptState::Collecting
                | AttemptState::CancelRequested
        ),
    }
}

/// Domain errors produced while managing task revisions and attempts.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    InvalidTask(#[from] ProtocolError),
    #[error("task revision must be greater than zero, got {0}")]
    InvalidRevision(u32),
    #[error("task revision overflow")]
    RevisionOverflow,
    #[error("replacement revision belongs to task {actual}, expected {expected}")]
    RevisionTaskMismatch { expected: TaskId, actual: TaskId },
    #[error("replacement must be revision {}, got {attempted}", current + 1)]
    RevisionNotNext { current: u32, attempted: u32 },
    #[error("attempt number must be in 1..={max}, got {number}")]
    InvalidAttemptNumber { number: u8, max: u8 },
    #[error("invalid attempt transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: AttemptState,
        to: AttemptState,
    },
    #[error("terminal transition requires a result envelope")]
    TerminalRequiresResult,
    #[error("attempt is already terminal with result {result_id}")]
    AttemptAlreadyTerminal { result_id: ResultId },
    #[error("terminal attempt is missing its result envelope")]
    TerminalInvariantViolated,
    #[error("terminal result {existing} is already recorded; rejected {attempted}")]
    TerminalResultAlreadyRecorded {
        existing: ResultId,
        attempted: ResultId,
    },
    #[error("result belongs to task {actual}, expected {expected}")]
    ResultTaskMismatch { expected: TaskId, actual: TaskId },
    #[error("result belongs to revision {actual}, expected {expected}")]
    ResultRevisionMismatch { expected: u32, actual: u32 },
    #[error("result belongs to attempt {actual}, expected {expected}")]
    ResultAttemptMismatch {
        expected: AttemptId,
        actual: AttemptId,
    },
    #[error("outcome {outcome:?} is not allowed while attempt is {state:?}")]
    OutcomeNotAllowed {
        state: AttemptState,
        outcome: TerminalOutcome,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{ArtifactContract, AttemptBudget, OwnerId, Route, SCHEMA_V1};

    fn task_spec(task_id: TaskId, revision: u32) -> TaskSpec {
        TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision,
            create_request_id: format!("create-{revision}"),
            owner_id: OwnerId::new("codex:test").unwrap(),
            objective: "Exercise the state machine".to_owned(),
            workspace: "/tmp/brgr-test".to_owned(),
            route: Route {
                harness_id: "local.synthetic".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec!["completion".to_owned()],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
            },
            acceptance_criteria: vec!["result exists".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 30,
                max_attempts: 2,
            },
        }
    }

    fn result_for(attempt: &Attempt, outcome: TerminalOutcome) -> ResultEnvelope {
        ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id: attempt.task().task_id(),
            revision: attempt.task().revision(),
            attempt_id: attempt.id(),
            result_id: ResultId::new(),
            outcome,
            artifacts: vec![],
            error: None,
            unresolved_effects: vec![],
        }
    }

    #[test]
    fn candidate_follows_the_complete_happy_path() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();

        attempt.transition(AttemptState::Starting).unwrap();
        attempt.transition(AttemptState::Running).unwrap();
        attempt.transition(AttemptState::Blocked).unwrap();
        attempt.transition(AttemptState::Running).unwrap();
        attempt.transition(AttemptState::Collecting).unwrap();
        let result = result_for(&attempt, TerminalOutcome::Candidate);
        let result_id = result.result_id;
        attempt.record_terminal(result).unwrap();

        assert_eq!(attempt.state(), AttemptState::Terminal);
        assert_eq!(attempt.terminal_result().unwrap().result_id, result_id);
    }

    #[test]
    fn illegal_transition_does_not_change_state() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();

        let error = attempt.transition(AttemptState::Running).unwrap_err();

        assert_eq!(
            error,
            CoreError::InvalidTransition {
                from: AttemptState::Queued,
                to: AttemptState::Running,
            }
        );
        assert_eq!(attempt.state(), AttemptState::Queued);
    }

    #[test]
    fn terminal_state_requires_a_result() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();

        assert_eq!(
            attempt.transition(AttemptState::Terminal),
            Err(CoreError::TerminalRequiresResult)
        );
        assert_eq!(attempt.state(), AttemptState::Queued);
    }

    #[test]
    fn duplicate_terminal_result_keeps_the_first_writer() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();
        attempt.transition(AttemptState::Starting).unwrap();
        attempt.transition(AttemptState::Running).unwrap();
        attempt.transition(AttemptState::Collecting).unwrap();
        let first = result_for(&attempt, TerminalOutcome::Candidate);
        let first_id = first.result_id;
        attempt.record_terminal(first).unwrap();

        let second = result_for(&attempt, TerminalOutcome::Candidate);
        let second_id = second.result_id;
        let error = attempt.record_terminal(second).unwrap_err();

        assert_eq!(
            error,
            CoreError::TerminalResultAlreadyRecorded {
                existing: first_id,
                attempted: second_id,
            }
        );
        assert_eq!(attempt.terminal_result().unwrap().result_id, first_id);
    }

    #[test]
    fn result_identity_mismatch_is_rejected_without_ending_attempt() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();
        attempt.transition(AttemptState::Starting).unwrap();
        attempt.transition(AttemptState::Running).unwrap();
        attempt.transition(AttemptState::Collecting).unwrap();
        let mut result = result_for(&attempt, TerminalOutcome::Candidate);
        result.attempt_id = AttemptId::new();

        let error = attempt.record_terminal(result).unwrap_err();

        assert!(matches!(error, CoreError::ResultAttemptMismatch { .. }));
        assert_eq!(attempt.state(), AttemptState::Collecting);
        assert!(attempt.terminal_result().is_none());
    }

    #[test]
    fn outcome_must_match_the_observed_state() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let mut attempt = Attempt::new(task, AttemptId::new(), 1).unwrap();
        attempt.transition(AttemptState::Starting).unwrap();
        attempt.transition(AttemptState::Running).unwrap();
        let candidate = result_for(&attempt, TerminalOutcome::Candidate);

        assert_eq!(
            attempt.record_terminal(candidate),
            Err(CoreError::OutcomeNotAllowed {
                state: AttemptState::Running,
                outcome: TerminalOutcome::Candidate,
            })
        );

        attempt.transition(AttemptState::CancelRequested).unwrap();
        let cancelled = result_for(&attempt, TerminalOutcome::Cancelled);
        attempt.record_terminal(cancelled).unwrap();
        assert_eq!(attempt.state(), AttemptState::Terminal);
    }

    #[test]
    fn attempt_number_is_bounded_by_frozen_revision() {
        let task = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();

        assert_eq!(
            Attempt::new(task.clone(), AttemptId::new(), 0),
            Err(CoreError::InvalidAttemptNumber { number: 0, max: 2 })
        );
        assert!(Attempt::new(task.clone(), AttemptId::new(), 2).is_ok());
        assert_eq!(
            Attempt::new(task, AttemptId::new(), 3),
            Err(CoreError::InvalidAttemptNumber { number: 3, max: 2 })
        );
    }

    #[test]
    fn revision_replacement_is_sequential_and_preserves_original() {
        let task_id = TaskId::new();
        let original = TaskRevision::new(task_spec(task_id, 1)).unwrap();
        let revised = original.revise(task_spec(task_id, 2)).unwrap();

        assert_eq!(original.revision(), 1);
        assert_eq!(revised.revision(), 2);

        let skipped = original.revise(task_spec(task_id, 3)).unwrap_err();
        assert_eq!(
            skipped,
            CoreError::RevisionNotNext {
                current: 1,
                attempted: 3,
            }
        );
    }

    #[test]
    fn revision_cannot_change_task_identity() {
        let original = TaskRevision::new(task_spec(TaskId::new(), 1)).unwrap();
        let replacement_id = TaskId::new();

        assert!(matches!(
            original.revise(task_spec(replacement_id, 2)),
            Err(CoreError::RevisionTaskMismatch {
                actual,
                ..
            }) if actual == replacement_id
        ));
    }
}
