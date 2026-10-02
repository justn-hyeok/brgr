//! Running one attempt and turning its outcome into a sealed result.

use std::{fmt::Write as _, io::Cursor, path::Path, time::Duration};

use brgr_protocol::{
    AttemptId, AttemptState, Event, EventId, EventKind, ObservationSource, ResultEnvelope,
    ResultId, RouteObservation, TaskId, TaskSpec, TerminalOutcome,
};
use brgr_runner::{
    DelegationContext, ExecutionMode, ExecutionOutput, HarnessManifest, ProcessRunner, RunRequest,
    RunnerError,
};
use brgr_store::{RunnerIdentity, Store, StoreError};
use sha2::{Digest, Sha256};

use crate::{
    Attempt, CoreError, DelegationHost, SupervisorError, TaskRevision,
    evidence::{seal_requested_evidence, seal_requested_logs},
};

#[derive(Clone, Copy)]
pub(crate) struct AttemptControl<'a> {
    pub(crate) cancel_path: Option<&'a Path>,
    pub(crate) pid_path: Option<&'a Path>,
    pub(crate) runner_identity: Option<&'a RunnerIdentity>,
    pub(crate) delegation_host: Option<&'a DelegationHost>,
}

pub(crate) async fn run_single_attempt(
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
    if let Some(milliseconds) = std::env::var("BRGR_TEST_PAUSE_AFTER_CLAIM_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value <= 2_000)
    {
        std::thread::sleep(Duration::from_millis(milliseconds));
    }
    #[cfg(debug_assertions)]
    crash_at("after_claim");
    transition(
        store,
        &mut attempt,
        AttemptState::Starting,
        &mut producer_seq,
    )?;
    if control.cancel_path.is_some_and(Path::exists)
        || store.cancellation_requested(spec.task_id)?
    {
        return cancel_before_spawn(store, spec, attempt_id, &mut attempt, &mut producer_seq);
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

    let execution = await_with_cancellation(
        ProcessRunner::run_with_delegation(
            manifest,
            RunRequest {
                workspace: Path::new(&spec.workspace),
                prompt: &spec.objective,
                criteria: spec
                    .instructions
                    .forward_criteria
                    .then_some(spec.acceptance_criteria.as_slice()),
                instructions: Some(&spec.instructions),
                model: spec.route.requested_model.as_deref(),
                effort: spec.route.requested_effort.as_deref(),
                permission: spec.permission,
                deadline: Duration::from_secs(spec.budget.deadline_seconds),
                cancel_path: control.cancel_path,
                pid_path: control.pid_path,
            },
            control.delegation_host.map(|host| DelegationContext {
                control_home: &host.control_home,
                brgr_executable: &host.brgr_executable,
                task_id: spec.task_id,
                attempt_id,
                may_delegate: host.may_delegate,
            }),
        ),
        store,
        spec.task_id,
        control.cancel_path,
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
    if retryable && number < spec.budget.max_attempts {
        store.commit_terminal_result(&spec.owner_id, &result)?;
    } else {
        store.commit_terminal_result_final(&spec.owner_id, &result)?;
    }
    #[cfg(debug_assertions)]
    crash_at("after_terminal_commit");
    if retryable {
        store.grant_pre_spawn_retry(attempt_id)?;
    }
    Ok((result, retryable))
}

pub(crate) fn cancel_before_spawn(
    store: &mut Store,
    spec: &TaskSpec,
    attempt_id: AttemptId,
    attempt: &mut Attempt,
    producer_seq: &mut u64,
) -> Result<(ResultEnvelope, bool), SupervisorError> {
    transition(store, attempt, AttemptState::CancelRequested, producer_seq)?;
    let result = result_for(
        spec,
        attempt_id,
        TerminalOutcome::Cancelled,
        vec![],
        Some("cancelled before process spawn".to_owned()),
    );
    attempt.record_terminal(result.clone())?;
    store.commit_terminal_result_final(&spec.owner_id, &result)?;
    Ok((result, false))
}

pub(crate) async fn await_with_cancellation<F: Future>(
    future: F,
    store: &mut Store,
    task: TaskId,
    cancel_path: Option<&Path>,
) -> F::Output {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => break result,
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if store.cancellation_requested(task).unwrap_or(false)
                    && let Some(path) = cancel_path
                {
                    let _ = std::fs::write(path, b"cancel\n");
                }
            }
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn crash_at(stage: &str) {
    if std::env::var("BRGR_TEST_CRASH_STAGE").as_deref() == Ok(stage) {
        std::process::exit(79);
    }
}

pub(crate) fn is_retryable_spawn_failure(
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

/// The last non-empty line a Herdr-backed adapter wrote to stderr, bounded. The
/// adapter is brgr's own `__pane-run` or `__omp-run`, whose last line is its
/// error, which says why the run failed far better than the outcome alone.
pub(crate) fn delegated_lost_reason(output: Option<&brgr_runner::ExecutionOutput>) -> String {
    let reason = "the Herdr-backed worker did not provide a valid final result";
    // A run stopped at its deadline leaves whatever the adapter last printed,
    // which says nothing about why it stopped.
    if output.is_some_and(|output| output.timed_out) {
        return format!("{reason}: the attempt deadline elapsed");
    }
    match output.and_then(|output| last_stderr_line(&output.stderr)) {
        Some(line) => format!("{reason}: {line}"),
        None => reason.to_owned(),
    }
}

pub(crate) fn last_stderr_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    Some(
        line.chars()
            .filter(|character| !character.is_control())
            .take(300)
            .collect(),
    )
}

pub(crate) fn finish_execution(
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
        let reason = delegated_lost_reason(execution.as_ref().ok());
        let mut result = terminal_with_requested_logs(
            store,
            spec,
            attempt_id,
            TerminalOutcome::Lost,
            reason,
            execution.as_ref().ok(),
        );
        result.unresolved_effects.push(
            "The separately launched worker may still be running or may have caused external effects"
                .to_owned(),
        );
        return Ok(result);
    }
    match execution {
        Ok(output) if output.cancelled => {
            transition(store, attempt, AttemptState::CancelRequested, producer_seq)?;
            Ok(terminal_with_requested_logs(
                store,
                spec,
                attempt_id,
                TerminalOutcome::Cancelled,
                "cancellation requested by owner".to_owned(),
                Some(&output),
            ))
        }
        Ok(output) if output.succeeded(manifest) && !output.result.is_empty() => {
            transition(store, attempt, AttemptState::Collecting, producer_seq)?;
            if let Some(reason) = unsettled_worker_reason(store, spec.task_id, attempt_id)? {
                return Ok(terminal_with_requested_logs(
                    store,
                    spec,
                    attempt_id,
                    TerminalOutcome::Failed,
                    reason,
                    Some(&output),
                ));
            }
            let observed_model = output.observed_model.clone();
            let extra = match seal_requested_evidence(store, spec, &output) {
                Ok(extra) => extra,
                Err(reason) => {
                    return Ok(terminal_with_requested_logs(
                        store,
                        spec,
                        attempt_id,
                        TerminalOutcome::Failed,
                        reason,
                        Some(&output),
                    ));
                }
            };
            let artifact = store.seal_artifact_reader(
                Cursor::new(output.result),
                &spec.artifact_contract.media_type,
                spec.artifact_contract.max_bytes,
            )?;
            let mut artifacts = vec![artifact];
            artifacts.extend(extra);
            let mut result = result_for(
                spec,
                attempt_id,
                TerminalOutcome::Candidate,
                artifacts,
                None,
            );
            if let Some(model) = observed_model {
                result.route_observation = Some(RouteObservation {
                    model: Some(model),
                    model_source: ObservationSource::HarnessJsonl,
                    effort: None,
                    effort_source: ObservationSource::Unavailable,
                });
            }
            Ok(result)
        }
        Ok(output) => Ok(terminal_with_requested_logs(
            store,
            spec,
            attempt_id,
            TerminalOutcome::Failed,
            execution_failure_reason(&output),
            Some(&output),
        )),
        Err(error) => Ok(result_for(
            spec,
            attempt_id,
            TerminalOutcome::Failed,
            vec![],
            Some(error.to_string()),
        )),
    }
}

pub(crate) fn execution_failure_reason(output: &ExecutionOutput) -> String {
    if output.timed_out {
        "attempt deadline elapsed".to_owned()
    } else if output.output_truncated {
        "process output exceeded the configured limit".to_owned()
    } else if output.result.is_empty() {
        "process produced no result artifact".to_owned()
    } else {
        format!("process exited with status {:?}", output.exit_code)
    }
}

pub(crate) fn terminal_with_requested_logs(
    store: &Store,
    spec: &TaskSpec,
    attempt_id: AttemptId,
    outcome: TerminalOutcome,
    reason: String,
    output: Option<&ExecutionOutput>,
) -> ResultEnvelope {
    let (artifacts, error) = match output.map(|output| seal_requested_logs(store, spec, output)) {
        Some(Ok(artifacts)) => (artifacts, reason),
        Some(Err(log_error)) => (
            vec![],
            format!("{reason}; requested logs unavailable: {log_error}"),
        ),
        None => (vec![], reason),
    };
    result_for(spec, attempt_id, outcome, artifacts, Some(error))
}

pub(crate) fn transition(
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

pub(crate) fn unsettled_worker_reason(
    store: &Store,
    task_id: TaskId,
    attempt_id: AttemptId,
) -> Result<Option<String>, StoreError> {
    let children = store.unsettled_children(attempt_id)?;
    if children > 0 {
        return Ok(Some(format!(
            "{children} child task(s) remain undecided or unacknowledged"
        )));
    }
    let questions = store.unsettled_questions(task_id, attempt_id)?;
    if questions > 0 {
        return Ok(Some(format!("{questions} question(s) remain unanswered")));
    }
    Ok(None)
}

pub(crate) fn result_for(
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
        legacy_embedded_route_observation: None,
        route_observation: Some(RouteObservation::unavailable()),
        unresolved_effects: vec![],
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}
