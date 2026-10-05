use super::*;
use crate::execution::{
    delegated_lost_reason, finish_execution, is_retryable_spawn_failure, last_stderr_line,
    transition,
};
use brgr_protocol::{ArtifactContract, AttemptBudget, OwnerId, Route, SCHEMA_V1};
use brgr_protocol::{AttemptId, AttemptState, ResultId, TaskId};
use brgr_runner::ExecutionMode;
use brgr_runner::{
    ExecutionOutput, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1, ProbeSpec, ResultSource,
    ResultSpec,
};
use std::{collections::BTreeMap, path::PathBuf};
use std::{io::Cursor, time::Duration};

#[test]
fn a_lost_delegated_run_names_the_adapter_error() {
    let output = brgr_runner::ExecutionOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"brgr pane mode \xc2\xb7 claude agent in pane w1:p2\nError: Herdr could not start the claude agent\n\n".to_vec(),
        result: Vec::new(),
        observed_model: None,
        timed_out: false,
        cancelled: false,
        output_truncated: false,
        elapsed: Duration::from_secs(1),
    };
    assert_eq!(
        delegated_lost_reason(Some(&output)),
        "the Herdr-backed worker did not provide a valid final result: Herdr could not start the claude agent"
    );
    assert_eq!(
        delegated_lost_reason(None),
        "the Herdr-backed worker did not provide a valid final result"
    );
    // At the deadline the last stderr line is progress noise, not the cause.
    let timed_out = brgr_runner::ExecutionOutput {
        stderr: b"brgr pane mode \xc2\xb7 prompted\n".to_vec(),
        timed_out: true,
        ..output
    };
    assert_eq!(
        delegated_lost_reason(Some(&timed_out)),
        "the Herdr-backed worker did not provide a valid final result: the attempt deadline elapsed"
    );
    let noisy = format!("{}\u{1b}[31m", "x".repeat(400));
    let line = last_stderr_line(noisy.as_bytes()).unwrap();
    assert_eq!(line.chars().count(), 300);
    assert!(!line.contains('\u{1b}'));
}

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
        instructions: brgr_protocol::TaskInstructions::default(),
        evidence: brgr_protocol::EvidenceSpec::default(),
        max_concurrent_children: None,
        permission: None,
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

fn delegated_fixture_manifest() -> HarnessManifest {
    HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "internal.fixture-delegated".to_owned(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures/gjc")
            .canonicalize()
            .unwrap(),
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: None,
        },
        launch: LaunchSpec {
            argv: vec![
                "-p".to_owned(),
                "--mode=json".to_owned(),
                "@${input.prompt_file}".to_owned(),
            ],
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: vec![],
            mode: ExecutionMode::DelegatedExternal,
            permission_argv: brgr_runner::PermissionArgv::default(),
            interactive: None,
        },
        result: ResultSpec {
            source: ResultSource::JsonlAssistantFinal,
            media_type: "text/plain".to_owned(),
            max_bytes: 1_024,
            success_exit_codes: vec![0],
        },
        capabilities: BTreeMap::new(),
    }
}

async fn run_delegated_interrupted(timed_out: bool) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cancel = root.path().join("cancel");
    let mut spec = task_spec(TaskId::new(), 1);
    spec.route.harness_id = "internal.fixture-delegated".to_owned();
    spec.required_capabilities = vec![];
    spec.workspace = workspace.path().to_string_lossy().into_owned();
    spec.objective = "SLOW".to_owned();
    spec.budget.deadline_seconds = if timed_out { 1 } else { 30 };
    let owner = spec.owner_id.clone();
    let task_id = spec.task_id;
    let supervisor = Supervisor::open(root.path()).unwrap();
    let manifest = delegated_fixture_manifest();
    let (supervisor, result) = if timed_out {
        let mut supervisor = supervisor;
        let result = supervisor
            .run_fresh_controlled(spec, &manifest, None, None, None)
            .await
            .unwrap();
        (supervisor, result)
    } else {
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            let mut supervisor = supervisor;
            let result = supervisor
                .run_fresh_controlled(spec, &manifest, Some(&cancel_for_task), None, None)
                .await;
            (supervisor, result)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&cancel, b"cancel").unwrap();
        let (supervisor, result) = task.await.unwrap();
        (supervisor, result.unwrap())
    };

    assert_eq!(result.outcome, TerminalOutcome::Lost);
    assert!(result.artifacts.is_empty());
    assert!(!result.unresolved_effects.is_empty());
    let inbox = supervisor.store().inbox(&owner, false).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].result.outcome, TerminalOutcome::Lost);
    assert!(inbox[0].result.artifacts.is_empty());
    let stored = supervisor.store().latest_result(task_id).unwrap();
    assert_eq!(stored.outcome, result.outcome);
    assert_eq!(stored.attempt_id, result.attempt_id);
    assert!(!stored.unresolved_effects.is_empty());
    assert!(matches!(
        supervisor
            .store()
            .claim_attempt(task_id, 1, AttemptId::new()),
        Err(StoreError::UnresolvedPriorAttempt { .. })
    ));
}

#[tokio::test]
async fn delegated_cancel_during_flight_is_lost_without_stop_claim_or_retry() {
    run_delegated_interrupted(false).await;
}

#[tokio::test]
async fn delegated_deadline_exceeded_is_lost_without_stop_claim_or_retry() {
    run_delegated_interrupted(true).await;
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
            model_catalog: None,
        },
        launch: LaunchSpec {
            argv: vec![],
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: vec![],
            mode: ExecutionMode::DelegatedExternal,
            permission_argv: brgr_runner::PermissionArgv::default(),
            interactive: None,
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
        observed_model: None,
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
        legacy_embedded_route_observation: None,
        route_observation: None,
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
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
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
/// Gate 3-2: OMP duplicate turn notifications live outside brgr's store —
/// there is no production ingestion path to quarantine-test. What brgr
/// owns is idempotent commit: replaying the same sealed result after a
/// restart (the shape duplicate notifications take when they reach the
/// commit path) must return `AlreadyApplied` with zero new inbox, result,
/// or decision rows. Conflicting replays must be rejected, not merged.
#[test]
fn restart_replay_of_same_result_is_idempotent_without_new_rows() {
    let root = tempfile::TempDir::new().unwrap();
    let task = task_spec(TaskId::new(), 1);
    let attempt_id = AttemptId::new();
    let report = b"deterministic brgr report".to_vec();
    let result;
    {
        let mut store = Store::open(root.path()).unwrap();
        store.record_task(&task, "gate-3-2").unwrap();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let artifact = store
            .seal_artifact_reader(Cursor::new(report.clone()), "text/plain", 1_024)
            .unwrap();
        result = ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id: task.task_id,
            revision: task.revision,
            attempt_id,
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![artifact],
            error: None,
            legacy_embedded_route_observation: None,
            route_observation: None,
            unresolved_effects: vec![],
        };
        assert_eq!(
            store
                .commit_terminal_result(&task.owner_id, &result)
                .unwrap(),
            brgr_store::WriteOutcome::Inserted
        );
        assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    }

    // Restart, then replay the identical sealed result 11 times — the
    // duplicate-notification shape. Every replay is AlreadyApplied and
    // adds no rows anywhere.
    let mut restarted = Store::open(root.path()).unwrap();
    for _ in 1..=11 {
        assert_eq!(
            restarted
                .commit_terminal_result(&task.owner_id, &result)
                .unwrap(),
            brgr_store::WriteOutcome::AlreadyApplied
        );
    }
    assert_eq!(restarted.inbox(&task.owner_id, false).unwrap().len(), 1);
    assert_eq!(restarted.inbox(&task.owner_id, true).unwrap().len(), 1);
    assert_eq!(restarted.latest_result(task.task_id).unwrap(), result);
    assert!(
        restarted
            .decision_for_result(result.result_id)
            .unwrap()
            .is_none()
    );

    // A conflicting replay (same attempt, new result id) is rejected —
    // never merged into a second inbox item.
    let conflicting = ResultEnvelope {
        result_id: ResultId::new(),
        ..result.clone()
    };
    assert!(
        restarted
            .commit_terminal_result(&task.owner_id, &conflicting)
            .is_err()
    );
    assert_eq!(restarted.inbox(&task.owner_id, false).unwrap().len(), 1);
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
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
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
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
    }
    let mut restarted = Supervisor::open(root.path()).unwrap();
    assert!(
        restarted
            .reconcile_after_restart(|_| ExecutionObservation::SupervisorAlive(identity.clone()))
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
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
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
            .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
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
