//! The durable workflow step journal, seen only through the public store API.
//!
//! A workflow run owns steps, and each step is one delegated task. The script
//! may re-execute from the top after a crash, so recording a step is
//! idempotent: the same key and input digest returns the same task, and a
//! crash cannot leave a task without its step row or the reverse.

use std::path::Path;

use brgr_protocol::{
    AttemptId, OwnerId, ResultEnvelope, ResultId, SCHEMA_V1, TaskId, TaskSpec, TerminalOutcome,
};
use brgr_store::{
    Store, StoreError, WorkflowRunId, WorkflowRunState, WorkflowStepKey, WorkflowStepOutcome,
};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

fn owner() -> OwnerId {
    OwnerId::new("codex:workflow-owner").unwrap()
}

fn run_id(value: &str) -> WorkflowRunId {
    WorkflowRunId::new(value).unwrap()
}

fn step_key(value: &str) -> WorkflowStepKey {
    WorkflowStepKey::new(value).unwrap()
}

fn spec(owner_id: &OwnerId, task_id: TaskId) -> TaskSpec {
    serde_json::from_value(json!({
        "schema": SCHEMA_V1,
        "task_id": task_id,
        "revision": 1,
        "create_request_id": format!("request-{task_id}"),
        "owner_id": owner_id.as_str(),
        "objective": "Do one workflow step",
        "workspace": "/tmp/workflow-test",
        "route": {"harness_id": "local.fixture"},
        "required_capabilities": [],
        "artifact_contract": {"media_type": "text/plain", "max_bytes": 4096},
        "acceptance_criteria": ["the step is sealed"],
        "budget": {"deadline_seconds": 60, "max_attempts": 1}
    }))
    .unwrap()
}

fn count_where(root: &Path, sql: &str, param: &str) -> i64 {
    let connection = Connection::open(root.join("brgr.sqlite3")).unwrap();
    connection
        .query_row(sql, [param], |row| row.get(0))
        .unwrap()
}

fn count_table(root: &Path, table: &str) -> i64 {
    let connection = Connection::open(root.join("brgr.sqlite3")).unwrap();
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn failed_result(task: &TaskSpec, attempt_id: AttemptId) -> ResultEnvelope {
    ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Failed,
        artifacts: vec![],
        error: Some("the step finished".to_owned()),
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
    }
}

#[test]
fn recording_the_same_step_twice_creates_one_task_and_returns_the_same_task_id() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-same-step");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 2)
        .unwrap();
    let task = spec(&owner_id, TaskId::new());

    let first = store
        .record_workflow_step(
            &run_id,
            &step_key("fetch"),
            "digest-in",
            &task,
            "digest-req",
        )
        .unwrap();
    let WorkflowStepOutcome::Created { task_id, revision } = first else {
        panic!("the first record must create a step, got {first:?}");
    };
    assert_eq!(task_id, task.task_id);
    assert_eq!(revision, task.revision);

    let second = store
        .record_workflow_step(
            &run_id,
            &step_key("fetch"),
            "digest-in",
            &task,
            "digest-req",
        )
        .unwrap();
    assert_eq!(
        second,
        WorkflowStepOutcome::Existing { task_id, revision },
        "a repeated step created a second task"
    );

    assert_eq!(
        count_where(
            root.path(),
            "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
            &task_id.to_string()
        ),
        1
    );
    assert_eq!(
        count_where(
            root.path(),
            "SELECT COUNT(*) FROM idempotency_requests WHERE request_id = ?1",
            &task.create_request_id
        ),
        1
    );
    assert_eq!(store.workflow_steps(&run_id).unwrap().len(), 1);
}

#[test]
fn the_same_step_key_with_a_different_input_digest_is_refused_as_diverged_and_creates_nothing() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-diverged");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 2)
        .unwrap();
    let task = spec(&owner_id, TaskId::new());
    store
        .record_workflow_step(
            &run_id,
            &step_key("fetch"),
            "digest-in-a",
            &task,
            "digest-req",
        )
        .unwrap();

    let other = spec(&owner_id, TaskId::new());
    let refused = store.record_workflow_step(
        &run_id,
        &step_key("fetch"),
        "digest-in-b",
        &other,
        "digest-other",
    );
    assert!(
        matches!(refused, Err(StoreError::WorkflowStepDiverged { .. })),
        "a changed input digest was accepted: {refused:?}"
    );

    assert_eq!(count_table(root.path(), "tasks"), 1);
    assert_eq!(count_table(root.path(), "idempotency_requests"), 1);
    assert_eq!(count_table(root.path(), "workflow_steps"), 1);
    assert_eq!(
        store.workflow_steps(&run_id).unwrap()[0].task_id,
        task.task_id
    );
}

#[test]
fn a_step_whose_task_insert_fails_leaves_no_step_row() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-atomic");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 2)
        .unwrap();

    let recorded = spec(&owner_id, TaskId::new());
    store.record_task(&recorded, "digest-existing").unwrap();

    let mut colliding = spec(&owner_id, TaskId::new());
    colliding.create_request_id = recorded.create_request_id.clone();
    let refused = store.record_workflow_step(
        &run_id,
        &step_key("fetch"),
        "digest-in",
        &colliding,
        "digest-existing",
    );
    assert!(
        matches!(refused, Err(StoreError::IdempotencyConflict(_))),
        "a colliding create_request_id was admitted as a step: {refused:?}"
    );

    assert_eq!(count_table(root.path(), "workflow_steps"), 0);
    assert_eq!(count_table(root.path(), "tasks"), 1);
    assert_eq!(
        count_where(
            root.path(),
            "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
            &colliding.task_id.to_string()
        ),
        0
    );
    assert!(store.workflow_steps(&run_id).unwrap().is_empty());
}

#[test]
fn the_parallel_limit_refuses_the_next_unfinished_step_and_admits_it_after_a_terminal_result() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-parallel");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 1)
        .unwrap();

    let first_task = spec(&owner_id, TaskId::new());
    let first = store
        .record_workflow_step(
            &run_id,
            &step_key("one"),
            "digest-in-1",
            &first_task,
            "digest-req-1",
        )
        .unwrap();
    let WorkflowStepOutcome::Created { task_id, revision } = first else {
        panic!("the first step must be created, got {first:?}");
    };

    let second_task = spec(&owner_id, TaskId::new());
    let refused = store.record_workflow_step(
        &run_id,
        &step_key("two"),
        "digest-in-2",
        &second_task,
        "digest-req-2",
    );
    assert!(
        matches!(refused, Err(StoreError::WorkflowParallelLimit)),
        "the second unfinished step was admitted: {refused:?}"
    );
    assert_eq!(store.workflow_steps(&run_id).unwrap().len(), 1);
    assert_eq!(count_table(root.path(), "tasks"), 1);

    let attempt_id = AttemptId::new();
    store.claim_attempt(task_id, revision, attempt_id).unwrap();
    store
        .commit_terminal_result(&owner_id, &failed_result(&first_task, attempt_id))
        .unwrap();

    let admitted = store
        .record_workflow_step(
            &run_id,
            &step_key("two"),
            "digest-in-2",
            &second_task,
            "digest-req-2",
        )
        .unwrap();
    assert!(
        matches!(
            admitted,
            WorkflowStepOutcome::Created { task_id, .. } if task_id == second_task.task_id
        ),
        "a finished slot did not admit the waiting step: {admitted:?}"
    );
    assert_eq!(store.workflow_steps(&run_id).unwrap().len(), 2);
}

#[test]
fn a_run_that_is_not_running_refuses_new_steps() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let missing = run_id("run-missing");
    let task = spec(&owner_id, TaskId::new());
    assert!(
        matches!(
            store.record_workflow_step(
                &missing,
                &step_key("fetch"),
                "digest-in",
                &task,
                "digest-req"
            ),
            Err(StoreError::WorkflowRunNotFound(_))
        ),
        "a step was recorded against a run that does not exist"
    );

    let run_id = run_id("run-blocked");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 2)
        .unwrap();
    store
        .set_workflow_run_state(&run_id, WorkflowRunState::Blocked)
        .unwrap();
    assert!(
        matches!(
            store.record_workflow_step(
                &run_id,
                &step_key("fetch"),
                "digest-in",
                &task,
                "digest-req"
            ),
            Err(StoreError::WorkflowRunNotRunning(_))
        ),
        "a blocked run accepted a new step"
    );

    store
        .set_workflow_run_state(&run_id, WorkflowRunState::Running)
        .unwrap();
    store
        .set_workflow_run_state(&run_id, WorkflowRunState::Completed)
        .unwrap();
    assert!(
        matches!(
            store.record_workflow_step(
                &run_id,
                &step_key("fetch"),
                "digest-in",
                &task,
                "digest-req"
            ),
            Err(StoreError::WorkflowRunNotRunning(_))
        ),
        "a completed run accepted a new step"
    );
    assert_eq!(count_table(root.path(), "workflow_steps"), 0);
    assert_eq!(count_table(root.path(), "tasks"), 0);
}

#[test]
fn run_state_transitions_allow_the_documented_graph_and_refuse_leaving_a_terminal_state() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();

    let blocked = run_id("run-to-blocked");
    store
        .create_workflow_run(&blocked, &owner_id, "digest-script", 2)
        .unwrap();
    assert_eq!(
        store
            .set_workflow_run_state(&blocked, WorkflowRunState::Blocked)
            .unwrap()
            .state,
        WorkflowRunState::Blocked
    );
    assert_eq!(
        store
            .set_workflow_run_state(&blocked, WorkflowRunState::Running)
            .unwrap()
            .state,
        WorkflowRunState::Running
    );

    for (name, to) in [
        ("run-to-completed", WorkflowRunState::Completed),
        ("run-to-failed", WorkflowRunState::Failed),
        ("run-to-cancelled", WorkflowRunState::Cancelled),
    ] {
        let run_id = run_id(name);
        store
            .create_workflow_run(&run_id, &owner_id, "digest-script", 1)
            .unwrap();
        assert_eq!(
            store.set_workflow_run_state(&run_id, to).unwrap().state,
            to,
            "{to:?} was refused from running"
        );
        let refused = store.set_workflow_run_state(&run_id, WorkflowRunState::Running);
        assert!(
            matches!(refused, Err(StoreError::WorkflowRunTerminal(_))),
            "left {to:?} for running: {refused:?}"
        );
        let refused = store.set_workflow_run_state(&run_id, WorkflowRunState::Failed);
        assert!(
            matches!(refused, Err(StoreError::WorkflowRunTerminal(_))),
            "left {to:?} for failed: {refused:?}"
        );
    }

    let from_blocked = run_id("run-blocked-to-cancelled");
    store
        .create_workflow_run(&from_blocked, &owner_id, "digest-script", 1)
        .unwrap();
    store
        .set_workflow_run_state(&from_blocked, WorkflowRunState::Blocked)
        .unwrap();
    assert_eq!(
        store
            .set_workflow_run_state(&from_blocked, WorkflowRunState::Cancelled)
            .unwrap()
            .state,
        WorkflowRunState::Cancelled
    );
    assert!(matches!(
        store.set_workflow_run_state(&from_blocked, WorkflowRunState::Blocked),
        Err(StoreError::WorkflowRunTerminal(_))
    ));
}

#[test]
fn create_workflow_run_is_idempotent_and_refuses_a_conflicting_repeat() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-create");

    let first = store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 3)
        .unwrap();
    assert_eq!(first.state, WorkflowRunState::Running);
    assert_eq!(first.max_parallel, 3);
    assert_eq!(first.owner_id, owner_id);
    assert_eq!(first.script_digest, "digest-script");

    let replay = store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 3)
        .unwrap();
    assert_eq!(replay.run_id, first.run_id);
    assert_eq!(replay.created_at, first.created_at);
    assert_eq!(replay.state, first.state);
    assert_eq!(count_table(root.path(), "workflow_runs"), 1);

    store
        .set_workflow_run_state(&run_id, WorkflowRunState::Blocked)
        .unwrap();
    let after_block = store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 3)
        .unwrap();
    assert_eq!(
        after_block.state,
        WorkflowRunState::Blocked,
        "an identical create reset a blocked run"
    );

    assert!(
        matches!(
            store.create_workflow_run(&run_id, &owner_id, "digest-other", 3),
            Err(StoreError::WorkflowRunConflict(_))
        ),
        "a different script digest overwrote the run"
    );
    assert!(
        matches!(
            store.create_workflow_run(&run_id, &owner_id, "digest-script", 4),
            Err(StoreError::WorkflowRunConflict(_))
        ),
        "a different max_parallel overwrote the run"
    );
    let other_owner = OwnerId::new("codex:someone-else").unwrap();
    assert!(
        matches!(
            store.create_workflow_run(&run_id, &other_owner, "digest-script", 3),
            Err(StoreError::WorkflowRunConflict(_))
        ),
        "a different owner overwrote the run"
    );
    assert_eq!(count_table(root.path(), "workflow_runs"), 1);
    let kept = store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 3)
        .unwrap();
    assert_eq!(kept.state, WorkflowRunState::Blocked);
    assert_eq!(kept.max_parallel, 3);
    assert_eq!(kept.script_digest, "digest-script");
}

#[test]
fn a_step_whose_task_belongs_to_another_owner_is_refused_and_creates_nothing() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let run_id = run_id("run-owner");
    store
        .create_workflow_run(&run_id, &owner(), "digest-script", 2)
        .unwrap();
    let stranger = OwnerId::new("codex:someone-else").unwrap();
    let task = spec(&stranger, TaskId::new());
    assert!(
        matches!(
            store.record_workflow_step(
                &run_id,
                &step_key("fetch"),
                "digest-in",
                &task,
                "digest-req"
            ),
            Err(StoreError::WorkflowStepOwnerMismatch(_))
        ),
        "a task owned by someone else was journaled under the run"
    );
    assert_eq!(count_table(root.path(), "tasks"), 0);
    assert_eq!(count_table(root.path(), "workflow_steps"), 0);
}

#[test]
fn workflow_step_for_task_returns_the_journaled_step_or_none() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-lookup");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 2)
        .unwrap();
    let task = spec(&owner_id, TaskId::new());
    store
        .record_workflow_step(
            &run_id,
            &step_key("fetch"),
            "digest-in",
            &task,
            "digest-req",
        )
        .unwrap();
    let found = store.workflow_step_for_task(task.task_id).unwrap().unwrap();
    assert_eq!(found.run_id, run_id);
    assert_eq!(found.step_key, step_key("fetch"));
    assert_eq!(found.task_id, task.task_id);
    assert!(
        store
            .workflow_step_for_task(TaskId::new())
            .unwrap()
            .is_none()
    );
}

fn notice_rows(root: &Path, run_id: &str) -> Vec<(i64, String, Option<String>)> {
    let connection = Connection::open(root.join("brgr.sqlite3")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT seq, state, delivered_session FROM workflow_notifications
             WHERE run_id = ?1 ORDER BY seq",
        )
        .unwrap();
    statement
        .query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

#[test]
fn blocked_then_running_then_blocked_creates_two_notices_and_a_repeated_cancel_creates_one() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();

    let bounce = run_id("run-bounce");
    store
        .create_workflow_run(&bounce, &owner_id, "digest-script", 1)
        .unwrap();
    store
        .set_workflow_run_state(&bounce, WorkflowRunState::Blocked)
        .unwrap();
    store
        .set_workflow_run_state(&bounce, WorkflowRunState::Running)
        .unwrap();
    store
        .set_workflow_run_state(&bounce, WorkflowRunState::Blocked)
        .unwrap();
    let bounced = notice_rows(root.path(), bounce.as_str());
    assert_eq!(
        bounced
            .iter()
            .map(|(seq, state, _)| (*seq, state.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "blocked"), (2, "blocked")],
        "blocked -> running -> blocked must record two notices, not one"
    );

    let cancel = run_id("run-cancel-twice");
    store
        .create_workflow_run(&cancel, &owner_id, "digest-script", 1)
        .unwrap();
    store
        .set_workflow_run_state(&cancel, WorkflowRunState::Cancelled)
        .unwrap();
    let again = store.set_workflow_run_state(&cancel, WorkflowRunState::Cancelled);
    assert!(
        matches!(again, Err(StoreError::WorkflowRunTerminal(_))),
        "a second cancel must not rewrite the run: {again:?}"
    );
    let cancelled = notice_rows(root.path(), cancel.as_str());
    assert_eq!(
        cancelled
            .iter()
            .map(|(seq, state, _)| (*seq, state.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "cancelled")],
        "cancelling twice must keep a single notice"
    );
}

#[test]
fn a_failed_state_update_after_the_notice_insert_rolls_back_both() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-atomic-notice");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 1)
        .unwrap();

    let connection = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_state_update BEFORE UPDATE ON workflow_runs
             WHEN EXISTS (SELECT 1 FROM workflow_notifications WHERE run_id = NEW.run_id)
             BEGIN
               SELECT RAISE(ABORT, 'forced state update failure');
             END;",
        )
        .unwrap();

    let refused = store.set_workflow_run_state(&run_id, WorkflowRunState::Blocked);
    assert!(
        refused.is_err(),
        "the forced state-update failure was not surfaced: {refused:?}"
    );
    assert_eq!(
        store.workflow_run(&run_id).unwrap().state,
        WorkflowRunState::Running,
        "the state change committed without its notice"
    );
    assert!(
        notice_rows(root.path(), run_id.as_str()).is_empty(),
        "the notice row survived a failed state update"
    );
}

#[test]
fn a_workflow_step_terminal_result_still_writes_no_completion_notification() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-step-quiet");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 1)
        .unwrap();
    let task = spec(&owner_id, TaskId::new());
    store
        .record_workflow_step(
            &run_id,
            &step_key("quiet"),
            "digest-in",
            &task,
            "digest-req",
        )
        .unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .commit_terminal_result(&owner_id, &failed_result(&task, attempt_id))
        .unwrap();
    assert_eq!(
        count_where(
            root.path(),
            "SELECT COUNT(*) FROM completion_notifications WHERE task_id = ?1",
            &task.task_id.to_string()
        ),
        0,
        "a workflow step wrote a per-task completion notice"
    );
}

#[test]
fn a_workflow_notice_claim_token_prevents_a_second_delivery() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let run_id = run_id("run-claim");
    store
        .create_workflow_run(&run_id, &owner_id, "digest-script", 1)
        .unwrap();
    store.bind_owner(&owner_id, "session-a", 1).unwrap();
    store
        .register_owner_surface(
            &owner_id,
            "session-a",
            1,
            "w1:p1",
            Some("test-session"),
            "/bin/herdr",
        )
        .unwrap();
    store
        .set_workflow_run_state(&run_id, WorkflowRunState::Blocked)
        .unwrap();

    let pending = store.pending_workflow_notifications(&run_id).unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let seq = pending[0].seq;
    let first = store
        .claim_workflow_notification(&run_id, seq, "claim-a", 100, 20)
        .unwrap()
        .expect("the first claimer must receive the notice");
    assert_eq!(first.pane_id, "w1:p1");
    assert!(
        store
            .claim_workflow_notification(&run_id, seq, "other", 101, 20)
            .unwrap()
            .is_none(),
        "a live claim was handed to a second token"
    );
    store
        .mark_workflow_notification_delivered(&first, "claim-a")
        .unwrap();
    assert!(
        store
            .pending_workflow_notifications(&run_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .claim_workflow_notification(&run_id, seq, "claim-b", 200, 20)
            .unwrap()
            .is_none(),
        "a delivered notice was claimed again"
    );
    assert!(matches!(
        store.mark_workflow_notification_delivered(&first, "claim-a"),
        Err(StoreError::NotificationClaimStale)
    ));
    store.resolve_workflow_notification(&run_id, seq).unwrap();
}
