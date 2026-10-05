//! Managed task orchestration across protocol, runner, and store boundaries.
//!
//! This crate owns domain invariants. Persistence and process adapters may
//! report observations, but they cannot bypass task revision or attempt state
//! validation.
//!
//! The two invariants are that a task's history is a chain of whole revisions,
//! and that an attempt walks a fixed state machine that can only be left through
//! a result. Both are enforced by construction rather than by convention:
//!
//! ```
//! use brgr_core::{Attempt, CoreError, TaskRevision};
//! use brgr_protocol::{AttemptId, AttemptState};
//!
//! # fn wire(revision: u32) -> brgr_protocol::TaskSpec {
//! #     serde_json::from_str(&format!(r#"{{
//! #       "schema": "brgr/v1",
//! #       "task_id": "3d3c9081-0f4a-4f2e-9c1b-7a2d5e6f8a90",
//! #       "revision": {revision},
//! #       "create_request_id": "req-1",
//! #       "owner_id": "codex:alice",
//! #       "objective": "Summarize the build log",
//! #       "workspace": "/srv/checkout",
//! #       "route": {{ "harness_id": "local.fixture" }},
//! #       "required_capabilities": ["completion"],
//! #       "artifact_contract": {{ "media_type": "text/plain", "max_bytes": 4096 }},
//! #       "acceptance_criteria": ["the report is sealed"],
//! #       "budget": {{ "deadline_seconds": 60, "max_attempts": 2 }}
//! #     }}"#)).unwrap()
//! # }
//! let task = TaskRevision::new(wire(1))?;
//!
//! // A revision is replaced whole, never edited, and only by its own successor.
//! let second = task.revise(wire(2))?;
//! assert_eq!(second.revision(), 2);
//! assert_eq!(second.task_id(), task.task_id());
//! // Revision 3 is skipped here. The receiver is `second` on purpose: asked of
//! // `task` (revision 1), `wire(4)` fails because 4 is not 2, which says nothing
//! // about skipping, and would hold for `wire(0)` or `wire(7)` just the same.
//! assert!(matches!(
//!     second.revise(wire(4)),
//!     Err(CoreError::RevisionNotNext { current: 2, attempted: 4 })
//! ));
//!
//! // An attempt runs against one revision and walks the v1 state machine.
//! let mut attempt = Attempt::new(second, AttemptId::new(), 1)?;
//! assert_eq!(attempt.state(), AttemptState::Queued);
//! attempt.transition(AttemptState::Starting)?;
//! attempt.transition(AttemptState::Running)?;
//!
//! // Terminal is not a state you may step into: it is entered by recording the
//! // result, so every terminal attempt owns one. Matched on the variant, not
//! // `is_err`: from `Running` a step to `Terminal` is also not a legal edge, so
//! // `is_err` alone would still hold if this rule were deleted.
//! assert!(matches!(
//!     attempt.transition(AttemptState::Terminal),
//!     Err(CoreError::TerminalRequiresResult)
//! ));
//! assert!(attempt.terminal_result().is_none());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod evidence;
mod execution;
mod git_diff;
mod state;

pub use evidence::task_patch;
pub use state::{Attempt, CoreError, TaskRevision};

use execution::{AttemptControl, result_for, run_single_attempt, sha256};

use std::path::{Path, PathBuf};

use brgr_protocol::{ResultEnvelope, TaskSpec, TerminalOutcome};
use brgr_runner::{HarnessManifest, RunnerError};
use brgr_store::{RunnerIdentity, Store, StoreError, UnfinishedAttempt};

/// Runs fresh attempts through the shared state, process, and persistence
/// contract.
pub struct Supervisor {
    store: Store,
    epoch: u64,
    delegation_host: Option<DelegationHost>,
}

struct DelegationHost {
    control_home: PathBuf,
    brgr_executable: PathBuf,
    may_delegate: bool,
}

/// Independent observation of the original runner incarnation. A numeric PID
/// is insufficient: `Alive` must carry the exact recorded birth/session marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionObservation {
    Alive(RunnerIdentity),
    /// A live, task-bound detached supervisor whose identity has not yet been
    /// persisted to the attempt launch intent.
    SupervisorAlive(RunnerIdentity),
    /// A task/attempt-bound external TUI session observed by its adapter.
    ExternalAlive(RunnerIdentity),
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
            delegation_host: None,
        })
    }

    /// Gives each worker process its exact attempt identity and brgr
    /// entrypoint, so it can message its owner; `may_delegate` additionally
    /// allows it to start child tasks.
    pub fn enable_worker_context(
        &mut self,
        control_home: PathBuf,
        brgr_executable: PathBuf,
        may_delegate: bool,
    ) {
        self.delegation_host = Some(DelegationHost {
            control_home,
            brgr_executable,
            may_delegate,
        });
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
            ) || matches!(&observation, ExecutionObservation::ExternalAlive(identity) if identity.validate().is_ok());
            if exactly_alive {
                continue;
            }
            let reason = match observation {
                ExecutionObservation::Alive(_)
                | ExecutionObservation::SupervisorAlive(_)
                | ExecutionObservation::ExternalAlive(_) => {
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
            delegation_host: self.delegation_host.as_ref(),
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

    /// Seal a completed external session on its original attempt after the
    /// collector exited. The adapter verifies the session and report identity.
    ///
    /// # Errors
    /// Returns an error for a stale attempt, invalid evidence, or store failure.
    pub fn recover_external_report(
        &mut self,
        task: brgr_protocol::TaskId,
        attempt: brgr_protocol::AttemptId,
        report: &[u8],
    ) -> Result<ResultEnvelope, SupervisorError> {
        if let Ok(existing) = self.store.latest_result(task)
            && existing.attempt_id == attempt
        {
            return Ok(existing);
        }
        let spec = self
            .store
            .unfinished_attempts()?
            .into_iter()
            .find(|value| value.task.task_id == task && value.attempt_id == attempt)
            .ok_or(brgr_store::StoreError::AttemptNotFound(attempt))?
            .task;
        let state = self.store.attempt_state_by_id(attempt)?;
        if !matches!(
            state,
            brgr_protocol::AttemptState::Running
                | brgr_protocol::AttemptState::Collecting
                | brgr_protocol::AttemptState::Blocked
        ) {
            return Err(brgr_store::StoreError::AttemptNotFound(attempt).into());
        }
        if state != brgr_protocol::AttemptState::Collecting {
            self.store.compare_and_set_attempt_state(
                attempt,
                state,
                brgr_protocol::AttemptState::Collecting,
            )?;
        }
        let settlement = execution::unsettled_worker_reason(&self.store, task, attempt)?;
        let (artifacts, error) = if report.is_empty() {
            (
                Vec::new(),
                Some("external session returned an empty report".to_owned()),
            )
        } else if let Some(reason) = settlement {
            (Vec::new(), Some(reason))
        } else {
            let reference = self.store.seal_artifact_reader(
                std::io::Cursor::new(report),
                &spec.artifact_contract.media_type,
                spec.artifact_contract.max_bytes,
            )?;
            let output = brgr_runner::ExecutionOutput {
                exit_code: None,
                stdout: report.to_vec(),
                stderr: vec![],
                result: report.to_vec(),
                observed_model: None,
                timed_out: false,
                cancelled: false,
                output_truncated: false,
                elapsed: std::time::Duration::ZERO,
            };
            match evidence::seal_requested_evidence(&self.store, &spec, &output) {
                Ok(mut evidence) => {
                    evidence.insert(0, reference);
                    (evidence, None)
                }
                Err(error) => (vec![reference], Some(error)),
            }
        };
        let outcome = if error.is_none() {
            brgr_protocol::TerminalOutcome::Candidate
        } else {
            brgr_protocol::TerminalOutcome::Failed
        };
        let result = execution::result_for(&spec, attempt, outcome, artifacts, error);
        self.store
            .commit_terminal_result_final(&spec.owner_id, &result)?;
        Ok(result)
    }
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
mod tests;
