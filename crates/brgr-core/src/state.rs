//! The task-revision and attempt state machine, and its errors.

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
