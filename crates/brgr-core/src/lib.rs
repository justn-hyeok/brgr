//! Managed task orchestration across protocol, runner, and store boundaries.
//!
//! This crate owns domain invariants. Persistence and process adapters may
//! report observations, but they cannot bypass task revision or attempt state
//! validation.

use std::{fmt::Write as _, io::Cursor, path::Path, time::Duration};

use brgr_protocol::{
    AttemptId, AttemptState, Event, EventId, EventKind, ProtocolError, ResultEnvelope, ResultId,
    TaskId, TaskSpec, TerminalOutcome,
};
use brgr_runner::{ExecutionMode, HarnessManifest, ProcessRunner, RunRequest, RunnerError};
use brgr_store::{RunnerIdentity, Store, StoreError, UnfinishedAttempt};
use sha2::{Digest, Sha256};

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

/// Runs fresh attempts through the shared state, process, and persistence
/// contract.
pub struct Supervisor {
    store: Store,
    epoch: u64,
}

/// Independent observation of the original runner incarnation. A numeric PID
/// is insufficient: `Alive` must carry the exact recorded birth/session marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionObservation {
    Alive(RunnerIdentity),
    /// A live, task-bound detached supervisor whose identity has not yet been
    /// persisted to the attempt launch intent.
    SupervisorAlive(RunnerIdentity),
    NotObserved,
    Unknown,
}

impl Supervisor {
    /// Opens the durable supervisor store.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError`] when the private store cannot be opened.
    pub fn open(store_root: impl AsRef<Path>) -> Result<Self, SupervisorError> {
        Ok(Self {
            store: Store::open(store_root)?,
            epoch: (uuid::Uuid::new_v4().as_u64_pair().0 & 0x7fff_ffff_ffff_ffff).max(1),
        })
    }

    /// Reconciles only attempts from a previous supervisor incarnation.
    /// The observer must inspect the native runner using its recorded nonce
    /// and birth marker. Unknown and unobserved runs become durable `lost`
    /// results with unresolved effects; no automatic retry is made. A live,
    /// exactly matching runner remains active for later reconciliation.
    ///
    /// # Errors
    ///
    /// Returns an error on a concurrent state/identity change or failed store
    /// transaction. The caller should reopen and retry reconciliation, not
    /// start another attempt.
    pub fn reconcile_after_restart(
        &mut self,
        mut observe: impl FnMut(&UnfinishedAttempt) -> ExecutionObservation,
    ) -> Result<Vec<ResultEnvelope>, SupervisorError> {
        let unfinished = self.store.unfinished_attempts()?;
        let mut recovered = Vec::new();
        for attempt in unfinished {
            let observation = observe(&attempt);
            let exactly_alive = matches!(
                (&observation, &attempt.launch),
                (ExecutionObservation::Alive(actual), Some(launch))
                    if launch.runner_identity.as_ref() == Some(actual)
                        && actual.validate().is_ok()
            ) || matches!(
                (&observation, &attempt.launch),
                (ExecutionObservation::SupervisorAlive(actual), launch)
                    if actual.validate().is_ok()
                        && launch.as_ref().is_none_or(|intent| {
                            intent.runner_identity.as_ref().is_none_or(|expected| expected == actual)
                        })
            );
            if exactly_alive {
                continue;
            }
            let reason = match observation {
                ExecutionObservation::Alive(_) | ExecutionObservation::SupervisorAlive(_) => {
                    "runner identity did not match the durable launch receipt"
                }
                ExecutionObservation::NotObserved => {
                    "original runner was not observed after supervisor restart"
                }
                ExecutionObservation::Unknown => {
                    "original runner state could not be established after supervisor restart"
                }
            };
            let mut result = result_for(
                &attempt.task,
                attempt.attempt_id,
                TerminalOutcome::Lost,
                vec![],
                Some(reason.to_owned()),
            );
            result.unresolved_effects.push(
                "External side effects of the original run are unresolved; do not retry automatically"
                    .to_owned(),
            );
            match self.store.commit_recovered_lost(&attempt, &result) {
                Ok(_) => recovered.push(result),
                // A live writer changed the attempt after our read. Its newer
                // state/result wins; the next read observes that state.
                Err(StoreError::RecoveryObservationStale(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(recovered)
    }

    /// Executes one fresh, bounded attempt and publishes its terminal result.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError`] when validation, execution, sealing, or the
    /// terminal transaction fails.
    pub async fn run_fresh(
        &mut self,
        spec: TaskSpec,
        manifest: &HarnessManifest,
    ) -> Result<ResultEnvelope, SupervisorError> {
        self.run_fresh_controlled(spec, manifest, None, None, None)
            .await
    }

    /// Executes a fresh attempt with optional cancellation and PID receipt
    /// files used by the detached CLI supervisor.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError`] under the same conditions as
    /// [`Supervisor::run_fresh`].
    pub async fn run_fresh_controlled(
        &mut self,
        spec: TaskSpec,
        manifest: &HarnessManifest,
        cancel_path: Option<&Path>,
        pid_path: Option<&Path>,
        runner_identity: Option<&RunnerIdentity>,
    ) -> Result<ResultEnvelope, SupervisorError> {
        let revision = TaskRevision::new(spec.clone())?;
        manifest.validate_task_route(&spec)?;
        let request_bytes = serde_json::to_vec(&spec)?;
        self.store.record_task(&spec, &sha256(&request_bytes))?;
        let control = AttemptControl {
            cancel_path,
            pid_path,
            runner_identity,
        };

        for number in 1..=spec.budget.max_attempts {
            let (result, retryable) = run_single_attempt(
                &mut self.store,
                self.epoch,
                &revision,
                manifest,
                number,
                control,
            )
            .await?;
            if !retryable || number == spec.budget.max_attempts {
                return Ok(result);
            }
        }
        Err(SupervisorError::NoAttempt)
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    #[must_use]
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }
}

#[derive(Clone, Copy)]
struct AttemptControl<'a> {
    cancel_path: Option<&'a Path>,
    pid_path: Option<&'a Path>,
    runner_identity: Option<&'a RunnerIdentity>,
}

async fn run_single_attempt(
    store: &mut Store,
    epoch: u64,
    revision: &TaskRevision,
    manifest: &HarnessManifest,
    number: u8,
    control: AttemptControl<'_>,
) -> Result<(ResultEnvelope, bool), SupervisorError> {
    let spec = revision.spec();

    let attempt_id = AttemptId::new();
    let mut attempt = Attempt::new(revision.clone(), attempt_id, number)?;
    let mut producer_seq = 0_u64;
    store.claim_attempt(spec.task_id, spec.revision, attempt_id)?;
    #[cfg(debug_assertions)]
    crash_at("after_claim");
    transition(
        store,
        &mut attempt,
        AttemptState::Starting,
        &mut producer_seq,
    )?;
    if control.cancel_path.is_some_and(Path::exists) {
        transition(
            store,
            &mut attempt,
            AttemptState::CancelRequested,
            &mut producer_seq,
        )?;
        let result = result_for(
            spec,
            attempt_id,
            TerminalOutcome::Cancelled,
            vec![],
            Some("cancelled before process spawn".to_owned()),
        );
        attempt.record_terminal(result.clone())?;
        store.commit_terminal_result(&spec.owner_id, &result)?;
        return Ok((result, false));
    }
    let launch_nonce = uuid::Uuid::new_v4().to_string();
    store.record_launch_intent(attempt_id, &launch_nonce, epoch)?;
    if let Some(identity) = control.runner_identity {
        store.record_runner_identity(attempt_id, &launch_nonce, identity)?;
    }
    #[cfg(debug_assertions)]
    crash_at("after_launch_intent");
    transition(
        store,
        &mut attempt,
        AttemptState::Running,
        &mut producer_seq,
    )?;

    let execution = ProcessRunner::run(
        manifest,
        RunRequest {
            workspace: Path::new(&spec.workspace),
            prompt: &spec.objective,
            model: spec.route.requested_model.as_deref(),
            effort: spec.route.requested_effort.as_deref(),
            deadline: Duration::from_secs(spec.budget.deadline_seconds),
            cancel_path: control.cancel_path,
            pid_path: control.pid_path,
        },
    )
    .await;

    let retryable =
        manifest.launch.mode == ExecutionMode::OneShot && is_retryable_spawn_failure(&execution);

    let result = finish_execution(
        store,
        spec,
        manifest,
        &mut attempt,
        &mut producer_seq,
        execution,
    )?;
    #[cfg(debug_assertions)]
    crash_at("after_seal_before_commit");

    attempt.record_terminal(result.clone())?;
    store.commit_terminal_result(&spec.owner_id, &result)?;
    #[cfg(debug_assertions)]
    crash_at("after_terminal_commit");
    if retryable {
        store.grant_pre_spawn_retry(attempt_id)?;
    }
    Ok((result, retryable))
}

#[cfg(debug_assertions)]
fn crash_at(stage: &str) {
    if std::env::var("BRGR_TEST_CRASH_STAGE").as_deref() == Ok(stage) {
        std::process::exit(79);
    }
}

fn is_retryable_spawn_failure(
    execution: &Result<brgr_runner::ExecutionOutput, RunnerError>,
) -> bool {
    matches!(
        execution,
        Err(RunnerError::SpawnIo(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            )
    )
}

fn finish_execution(
    store: &Store,
    spec: &TaskSpec,
    manifest: &HarnessManifest,
    attempt: &mut Attempt,
    producer_seq: &mut u64,
    execution: Result<brgr_runner::ExecutionOutput, RunnerError>,
) -> Result<ResultEnvelope, SupervisorError> {
    let attempt_id = attempt.id();
    if manifest.launch.mode == ExecutionMode::DelegatedExternal
        && !matches!(&execution, Ok(output) if !output.cancelled && output.succeeded(manifest) && !output.result.is_empty())
    {
        let mut result = result_for(
            spec,
            attempt_id,
            TerminalOutcome::Lost,
            vec![],
            Some("Herdr-backed OMP wrapper did not provide a valid final result".to_owned()),
        );
        result.unresolved_effects.push(
            "The separately launched OMP worker may still be running or may have caused external effects"
                .to_owned(),
        );
        return Ok(result);
    }
    match execution {
        Ok(output) if output.cancelled => {
            transition(store, attempt, AttemptState::CancelRequested, producer_seq)?;
            Ok(result_for(
                spec,
                attempt_id,
                TerminalOutcome::Cancelled,
                vec![],
                Some("cancellation requested by owner".to_owned()),
            ))
        }
        Ok(output) if output.succeeded(manifest) && !output.result.is_empty() => {
            transition(store, attempt, AttemptState::Collecting, producer_seq)?;
            let artifact = store.seal_artifact_reader(
                Cursor::new(output.result),
                &spec.artifact_contract.media_type,
                spec.artifact_contract.max_bytes,
            )?;
            Ok(result_for(
                spec,
                attempt_id,
                TerminalOutcome::Candidate,
                vec![artifact],
                None,
            ))
        }
        Ok(output) => {
            let reason = if output.timed_out {
                "attempt deadline elapsed".to_owned()
            } else if output.output_truncated {
                "process output exceeded the configured limit".to_owned()
            } else if output.result.is_empty() {
                "process produced no result artifact".to_owned()
            } else {
                format!("process exited with status {:?}", output.exit_code)
            };
            Ok(result_for(
                spec,
                attempt_id,
                TerminalOutcome::Failed,
                vec![],
                Some(reason),
            ))
        }
        Err(error) => Ok(result_for(
            spec,
            attempt_id,
            TerminalOutcome::Failed,
            vec![],
            Some(error.to_string()),
        )),
    }
}

fn transition(
    store: &Store,
    attempt: &mut Attempt,
    next: AttemptState,
    producer_seq: &mut u64,
) -> Result<(), SupervisorError> {
    let previous = attempt.state();
    attempt.transition(next)?;
    store.compare_and_set_attempt_state(attempt.id(), previous, next)?;
    *producer_seq = producer_seq.saturating_add(1);
    let kind = match next {
        AttemptState::Starting => EventKind::Starting,
        AttemptState::Running => EventKind::Running,
        AttemptState::Blocked => EventKind::Blocked,
        AttemptState::Collecting => EventKind::Collecting,
        AttemptState::CancelRequested => EventKind::CancelRequested,
        AttemptState::Queued | AttemptState::Terminal => {
            return Err(CoreError::InvalidTransition {
                from: attempt.state(),
                to: next,
            }
            .into());
        }
    };
    store.record_event(&Event {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        event_id: EventId::new(),
        attempt_id: attempt.id(),
        producer: "brgr.supervisor".to_owned(),
        producer_seq: *producer_seq,
        kind,
        payload: serde_json::json!({}),
    })?;
    Ok(())
}

fn result_for(
    spec: &TaskSpec,
    attempt_id: AttemptId,
    outcome: TerminalOutcome,
    artifacts: Vec<brgr_protocol::ArtifactRef>,
    error: Option<String>,
) -> ResultEnvelope {
    ResultEnvelope {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        task_id: spec.task_id,
        revision: spec.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome,
        artifacts,
        error,
        unresolved_effects: vec![],
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("task budget contained no attempt")]
    NoAttempt,
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{ArtifactContract, AttemptBudget, OwnerId, Route, SCHEMA_V1};
    use brgr_runner::{
        ExecutionOutput, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1, ProbeSpec,
        ResultSource, ResultSpec,
    };
    use std::{collections::BTreeMap, path::PathBuf};

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

    #[test]
    fn only_pre_spawn_transient_errors_are_retryable() {
        let transient = Err(RunnerError::SpawnIo(std::io::Error::from(
            std::io::ErrorKind::WouldBlock,
        )));
        assert!(is_retryable_spawn_failure(&transient));
        let denied = Err(RunnerError::SpawnIo(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert!(!is_retryable_spawn_failure(&denied));
        let after_spawn = Err(RunnerError::Io(std::io::Error::from(
            std::io::ErrorKind::WouldBlock,
        )));
        assert!(!is_retryable_spawn_failure(&after_spawn));
    }

    #[test]
    fn delegated_external_failure_is_lost_with_unresolved_effects() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let spec = task_spec(TaskId::new(), 1);
        store.record_task(&spec, "delegated-failure").unwrap();
        let attempt_id = AttemptId::new();
        let mut attempt =
            Attempt::new(TaskRevision::new(spec.clone()).unwrap(), attempt_id, 1).unwrap();
        store
            .claim_attempt(spec.task_id, spec.revision, attempt_id)
            .unwrap();
        let mut sequence = 0;
        transition(&store, &mut attempt, AttemptState::Starting, &mut sequence).unwrap();
        transition(&store, &mut attempt, AttemptState::Running, &mut sequence).unwrap();
        let mut manifest = HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "internal.fixture-delegated".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: PathBuf::from("/bin/echo"),
            probe: ProbeSpec {
                version_argv: vec!["--version".to_owned()],
                help_argv: vec!["--help".to_owned()],
            },
            launch: LaunchSpec {
                argv: vec![],
                model_argv: vec![],
                effort_argv: vec![],
                env_allow: vec![],
                mode: ExecutionMode::DelegatedExternal,
            },
            result: ResultSpec {
                source: ResultSource::Stdout,
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
                success_exit_codes: vec![0],
            },
            capabilities: BTreeMap::new(),
        };
        let failed = ExecutionOutput {
            exit_code: Some(1),
            stdout: vec![],
            stderr: vec![],
            result: vec![],
            timed_out: false,
            cancelled: false,
            output_truncated: false,
            elapsed: Duration::from_millis(1),
        };
        let result = finish_execution(
            &store,
            &spec,
            &manifest,
            &mut attempt,
            &mut sequence,
            Ok(failed.clone()),
        )
        .unwrap();
        assert_eq!(result.outcome, TerminalOutcome::Lost);
        assert!(!result.unresolved_effects.is_empty());
        for (cancelled, timed_out) in [(true, false), (false, true)] {
            let interrupted = ExecutionOutput {
                exit_code: Some(0),
                result: b"partial report".to_vec(),
                cancelled,
                timed_out,
                ..failed.clone()
            };
            let result = finish_execution(
                &store,
                &spec,
                &manifest,
                &mut attempt,
                &mut sequence,
                Ok(interrupted),
            )
            .unwrap();
            assert_eq!(result.outcome, TerminalOutcome::Lost);
        }
        manifest.launch.mode = ExecutionMode::OneShot;
        let ordinary = finish_execution(
            &store,
            &spec,
            &manifest,
            &mut attempt,
            &mut sequence,
            Ok(failed),
        )
        .unwrap();
        assert_eq!(ordinary.outcome, TerminalOutcome::Failed);
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
    fn restart_reconciles_unknown_run_to_one_durable_lost_inbox_item() {
        let root = tempfile::TempDir::new().unwrap();
        let task = task_spec(TaskId::new(), 1);
        let attempt_id = AttemptId::new();
        {
            let mut store = Store::open(root.path()).unwrap();
            store.record_task(&task, "restart-task").unwrap();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
            store
                .record_launch_intent(attempt_id, "launch-1", 1)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Starting,
                    AttemptState::Running,
                )
                .unwrap();
        }
        let mut restarted = Supervisor::open(root.path()).unwrap();
        let recovered = restarted
            .reconcile_after_restart(|_| ExecutionObservation::Unknown)
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].outcome, TerminalOutcome::Lost);
        assert!(!recovered[0].unresolved_effects.is_empty());
        assert_eq!(
            restarted
                .store()
                .inbox(&task.owner_id, false)
                .unwrap()
                .len(),
            1
        );
        assert!(restarted.store().unfinished_attempts().unwrap().is_empty());
        drop(restarted);
        let mut reopened = Supervisor::open(root.path()).unwrap();
        let replay = reopened
            .reconcile_after_restart(|_| ExecutionObservation::Unknown)
            .unwrap();
        assert!(replay.is_empty());
        assert_eq!(
            reopened.store().inbox(&task.owner_id, false).unwrap().len(),
            1
        );
    }

    #[test]
    fn matching_birth_marker_preserves_live_run_but_pid_only_does_not() {
        let root = tempfile::TempDir::new().unwrap();
        let task = task_spec(TaskId::new(), 1);
        let attempt_id = AttemptId::new();
        let identity = RunnerIdentity {
            namespace: "process".to_owned(),
            handle: "4242".to_owned(),
            birth_marker: "start-1".to_owned(),
        };
        {
            let mut store = Store::open(root.path()).unwrap();
            store.record_task(&task, "live-task").unwrap();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
            store
                .record_launch_intent(attempt_id, "launch-2", 1)
                .unwrap();
            store
                .record_runner_identity(attempt_id, "launch-2", &identity)
                .unwrap();
        }
        let mut restarted = Supervisor::open(root.path()).unwrap();
        assert!(
            restarted
                .reconcile_after_restart(|_| ExecutionObservation::Alive(identity.clone()))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            restarted.store().attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Starting
        );
        let reused_pid = RunnerIdentity {
            birth_marker: "start-2".to_owned(),
            ..identity
        };
        let recovered = restarted
            .reconcile_after_restart(|_| ExecutionObservation::Alive(reused_pid.clone()))
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].outcome, TerminalOutcome::Lost);
        assert!(matches!(
            restarted
                .store()
                .record_runner_identity(attempt_id, "launch-2", &reused_pid),
            Err(StoreError::RunnerIdentityConflict(_))
        ));
    }

    #[test]
    fn task_bound_supervisor_receipt_preserves_attempt_before_identity_is_written() {
        let root = tempfile::TempDir::new().unwrap();
        let task = task_spec(TaskId::new(), 1);
        let attempt_id = AttemptId::new();
        let identity = RunnerIdentity {
            namespace: "brgr.supervisor".to_owned(),
            handle: "4242".to_owned(),
            birth_marker: "start-1".to_owned(),
        };
        {
            let mut store = Store::open(root.path()).unwrap();
            store.record_task(&task, "startup-race").unwrap();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
        }
        let mut restarted = Supervisor::open(root.path()).unwrap();
        assert!(
            restarted
                .reconcile_after_restart(|_| ExecutionObservation::SupervisorAlive(
                    identity.clone()
                ))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            restarted.store().attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Starting
        );
        assert!(
            restarted
                .store()
                .inbox(&task.owner_id, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stale_recovery_snapshot_yields_to_concurrent_runner_progress() {
        let root = tempfile::TempDir::new().unwrap();
        let task = task_spec(TaskId::new(), 1);
        let attempt_id = AttemptId::new();
        {
            let mut store = Store::open(root.path()).unwrap();
            store.record_task(&task, "recovery-race").unwrap();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
        }
        let mut restarted = Supervisor::open(root.path()).unwrap();
        let recovered = restarted
            .reconcile_after_restart(|_| {
                let writer = Store::open(root.path()).unwrap();
                writer
                    .record_launch_intent(attempt_id, "new-launch", 1)
                    .unwrap();
                ExecutionObservation::Unknown
            })
            .unwrap();
        assert!(recovered.is_empty());
        assert_eq!(
            restarted.store().attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Starting
        );
        assert!(
            restarted
                .store()
                .inbox(&task.owner_id, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn crash_before_launch_intent_is_not_left_starting_forever() {
        let root = tempfile::TempDir::new().unwrap();
        let task = task_spec(TaskId::new(), 1);
        let attempt_id = AttemptId::new();
        {
            let mut store = Store::open(root.path()).unwrap();
            store.record_task(&task, "before-launch").unwrap();
            store
                .claim_attempt(task.task_id, task.revision, attempt_id)
                .unwrap();
            store
                .compare_and_set_attempt_state(
                    attempt_id,
                    AttemptState::Queued,
                    AttemptState::Starting,
                )
                .unwrap();
        }
        let mut restarted = Supervisor::open(root.path()).unwrap();
        let recovered = restarted
            .reconcile_after_restart(|attempt| {
                assert!(attempt.launch.is_none());
                ExecutionObservation::NotObserved
            })
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].outcome, TerminalOutcome::Lost);
        assert_eq!(
            restarted.store().attempt_state_by_id(attempt_id).unwrap(),
            AttemptState::Terminal
        );
        assert_eq!(
            restarted
                .store()
                .inbox(&task.owner_id, false)
                .unwrap()
                .len(),
            1
        );
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
