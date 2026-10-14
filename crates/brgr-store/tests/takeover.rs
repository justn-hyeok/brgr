//! The head handoff, seen only through the public store API.
//!
//! Every test here drives a store the way an owner would: bind a session,
//! produce a result, ask for the head to move, then act as one session or the
//! other. Nothing pokes at the tables except the migration test, which has to
//! build a file the way an older build wrote it.

use std::{io::Cursor, path::Path};

use brgr_protocol::{
    AttemptId, AttemptState, Decision, DecisionId, DecisionVerdict, OwnerId, ResultEnvelope,
    ResultId, SCHEMA_V1, TaskId, TaskSpec, TerminalOutcome,
};
use brgr_store::{
    HandoffManifest, ManifestTask, MessageDirection, MessageDraft, MessageKind, OwnerProof,
    RetireOutcome, Store, StoreError, TakeoverId, TakeoverState, WriteOutcome,
};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

const FROM: &str = "codex:session-head";
const TO: &str = "codex:session-incoming";

fn owner() -> OwnerId {
    OwnerId::new(FROM).unwrap()
}

fn spec(owner_id: &OwnerId, revision: u32, task_id: TaskId) -> TaskSpec {
    serde_json::from_value(json!({
        "schema": SCHEMA_V1,
        "task_id": task_id,
        "revision": revision,
        "create_request_id": format!("request-{task_id}-{revision}"),
        "owner_id": owner_id.as_str(),
        "objective": "Answer the owner",
        "workspace": "/tmp/takeover-test",
        "route": {"harness_id": "local.fixture"},
        "required_capabilities": [],
        "artifact_contract": {"media_type": "text/plain", "max_bytes": 4096},
        "acceptance_criteria": ["the answer is sealed"],
        "budget": {"deadline_seconds": 60, "max_attempts": 1}
    }))
    .unwrap()
}

/// Binds the owner to the outgoing session, as `brgr bind` does.
fn head(store: &Store, owner_id: &OwnerId) -> u64 {
    store.bind_owner(owner_id, FROM, 1).unwrap();
    1
}

/// A task with a sealed candidate result waiting in the owner's inbox.
fn candidate(store: &mut Store, owner_id: &OwnerId) -> (TaskSpec, ResultEnvelope) {
    let task = spec(owner_id, 1, TaskId::new());
    store.record_task(&task, "digest-task").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let envelope = ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Candidate,
        artifacts: vec![
            store
                .seal_artifact_reader(
                    Cursor::new(b"sealed-artifact-bytes"),
                    &task.artifact_contract.media_type,
                    task.artifact_contract.max_bytes,
                )
                .unwrap(),
        ],
        error: None,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
    };
    store.commit_terminal_result(owner_id, &envelope).unwrap();
    (task, envelope)
}

fn decision(
    owner_id: &OwnerId,
    task: &TaskSpec,
    envelope: &ResultEnvelope,
    session: &str,
    epoch: u64,
) -> Decision {
    Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: envelope.result_id,
        result_digest: Store::result_digest(envelope).unwrap(),
        session_id: Some(session.to_owned()),
        binding_epoch: Some(epoch),
        verdict: DecisionVerdict::Accepted,
        reason: "it answers the question".to_owned(),
    }
}

fn manifest(origin: &str, task: &TaskSpec, envelope: &ResultEnvelope) -> HandoffManifest {
    HandoffManifest {
        goal: "Finish the answer".to_owned(),
        decision_notes: "Revision one is waiting on an accept or reject.".to_owned(),
        origin_session: origin.to_owned(),
        tasks: vec![ManifestTask {
            task_id: task.task_id,
            revision: task.revision,
            attempt_id: envelope.attempt_id,
        }],
        pending_result_ids: vec![envelope.result_id],
        pending_message_ids: vec![],
    }
}

/// Plain manifest, for the paths that do not need a result to point at.
fn bare_manifest(origin: &str) -> HandoffManifest {
    HandoffManifest {
        goal: "Finish the answer".to_owned(),
        origin_session: origin.to_owned(),
        ..HandoffManifest::default()
    }
}

/// Requests and prepares a handoff, leaving it one step from committed.
fn prepared(
    store: &Store,
    owner_id: &OwnerId,
    key: &str,
    manifest: &HandoffManifest,
) -> TakeoverId {
    let takeover = store.request_takeover(owner_id, FROM, TO, key).unwrap();
    store
        .prepare_takeover(takeover.takeover_id, manifest)
        .unwrap();
    takeover.takeover_id
}

fn kinds(store: &Store, takeover_id: TakeoverId) -> Vec<TakeoverState> {
    store
        .takeover_events(takeover_id)
        .unwrap()
        .iter()
        .map(|event| event.kind)
        .collect()
}

fn state(store: &Store, takeover_id: TakeoverId) -> TakeoverState {
    store.takeover(takeover_id).unwrap().unwrap().state
}

#[test]
fn duplicate_request_with_the_same_key_keeps_one_takeover() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    head(&store, &owner_id);

    let first = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();
    let replay = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();
    assert_eq!(replay.takeover_id, first.takeover_id);
    assert_eq!(replay.from_epoch, first.from_epoch);
    assert_eq!(replay.from_session, first.from_session);
    assert_eq!(replay.state, TakeoverState::Requested);
    assert_eq!(
        store.owner_takeovers(&owner_id).unwrap().len(),
        1,
        "a repeated key started a second handoff"
    );

    // A key replayed for a different destination is refused rather than
    // retargeting the handoff that is already recorded.
    assert!(matches!(
        store.request_takeover(&owner_id, FROM, "codex:session-third", "handoff-1"),
        Err(StoreError::TakeoverIdempotencyConflict(key)) if key == "handoff-1"
    ));
    assert_eq!(store.owner_takeovers(&owner_id).unwrap().len(), 1);

    // Only one handoff may be open, and only until it is closed.
    assert!(matches!(
        store.request_takeover(&owner_id, FROM, "codex:session-third", "handoff-2"),
        Err(StoreError::TakeoverInProgress(open)) if open == first.takeover_id
    ));
    assert_eq!(
        state(&store, first.takeover_id),
        TakeoverState::Requested,
        "a refused request changed the open handoff"
    );
    store
        .abort_takeover(first.takeover_id, "the owner stayed put")
        .unwrap();
    assert_eq!(
        store
            .request_takeover(&owner_id, FROM, "codex:session-third", "handoff-2")
            .unwrap()
            .state,
        TakeoverState::Requested,
        "an aborted handoff kept the owner busy"
    );
}

#[test]
fn commit_twice_keeps_one_epoch_bump() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );

    let committed = store.commit_takeover(takeover_id).unwrap();
    assert_eq!(committed.state, TakeoverState::Committed);
    assert_eq!(committed.committed_epoch(), epoch + 1);
    assert_eq!(
        store.owner_binding(&owner_id).unwrap(),
        Some((TO.to_owned(), epoch + 1))
    );

    // The lost-response replay: it must report the same answer, not a new one.
    let replay = store.commit_takeover(takeover_id).unwrap();
    assert_eq!(replay.state, TakeoverState::Committed);
    assert_eq!(replay.updated_at, committed.updated_at);
    assert_eq!(
        store.owner_binding(&owner_id).unwrap(),
        Some((TO.to_owned(), epoch + 1)),
        "a second commit raised the epoch again"
    );
    assert_eq!(
        kinds(&store, takeover_id),
        vec![
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed
        ],
        "the trail recorded the commit twice"
    );
}

#[test]
fn commit_fails_and_leaves_the_state_untouched_when_the_binding_moved_after_request() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );

    // The head moved by another route while this handoff sat prepared.
    assert_eq!(
        store
            .rebind_owner(&owner_id, "codex:session-third")
            .unwrap(),
        2
    );

    let refused = store.commit_takeover(takeover_id);
    assert!(
        matches!(refused, Err(StoreError::TakeoverBindingConflict(id)) if id == takeover_id),
        "a stale handoff committed against a binding that moved: {refused:?}"
    );
    let recorded = store.takeover(takeover_id).unwrap().unwrap();
    assert_eq!(recorded.state, TakeoverState::Prepared);
    assert_eq!(recorded.failure_reason, None);
    assert_eq!(
        store.owner_binding(&owner_id).unwrap(),
        Some(("codex:session-third".to_owned(), 2)),
        "a refused commit moved the binding anyway"
    );
    assert_eq!(
        kinds(&store, takeover_id),
        vec![TakeoverState::Requested, TakeoverState::Prepared]
    );

    // Aborting is still available, because nothing moved.
    assert_eq!(
        store
            .abort_takeover(takeover_id, "the head moved elsewhere")
            .unwrap()
            .state,
        TakeoverState::Aborted
    );
    assert_eq!(
        store.owner_binding(&owner_id).unwrap(),
        Some(("codex:session-third".to_owned(), 2))
    );
}

#[test]
fn stale_from_session_decision_is_rejected_after_commit() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );
    store.commit_takeover(takeover_id).unwrap();

    let refused = store.record_decision(&decision(&owner_id, &task, &envelope, FROM, epoch));
    assert!(
        matches!(refused, Err(StoreError::OwnerBindingConflict)),
        "the outgoing head decided after handing over: {refused:?}"
    );
    assert!(
        store
            .decision_for_result(envelope.result_id)
            .unwrap()
            .is_none(),
        "a refused decision was recorded"
    );
    assert_eq!(
        store.inbox(&owner_id, false).unwrap().len(),
        1,
        "a refused decision resolved the inbox item"
    );
    assert_eq!(state(&store, takeover_id), TakeoverState::Committed);

    // The incoming head at the committed epoch decides.
    assert_eq!(
        store
            .record_decision(&decision(&owner_id, &task, &envelope, TO, epoch + 1))
            .unwrap(),
        WriteOutcome::Inserted
    );
    assert_eq!(state(&store, takeover_id), TakeoverState::Resumed);
}

#[test]
fn resumed_is_reached_only_by_a_to_session_owner_action_at_the_committed_epoch() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );

    // An owner action before the commit cannot resume anything: the incoming
    // session is not in command yet, so acting as it is refused outright.
    let incoming = OwnerProof::new(owner_id.clone(), TO, epoch + 1).unwrap();
    assert!(matches!(
        store.record_decision(&decision(&owner_id, &task, &envelope, TO, epoch + 1)),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert_eq!(state(&store, takeover_id), TakeoverState::Prepared);

    // The outgoing head still acting is fine, and still not a resume.
    assert_eq!(
        store
            .record_task_bound(
                &spec(&owner_id, 2, task.task_id),
                "digest-outgoing",
                &OwnerProof::new(owner_id.clone(), FROM, epoch).unwrap()
            )
            .unwrap(),
        WriteOutcome::Inserted
    );
    assert_eq!(state(&store, takeover_id), TakeoverState::Prepared);

    store.commit_takeover(takeover_id).unwrap();
    assert_eq!(state(&store, takeover_id), TakeoverState::Committed);

    // The outgoing session cannot act at all once the binding has moved.
    assert!(matches!(
        store.record_decision(&decision(&owner_id, &task, &envelope, FROM, epoch)),
        Err(StoreError::OwnerBindingConflict)
    ));
    // Nor can the incoming session act at the outgoing epoch.
    assert!(matches!(
        store.record_decision(&decision(&owner_id, &task, &envelope, TO, epoch)),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert_eq!(
        state(&store, takeover_id),
        TakeoverState::Committed,
        "a refused action resumed the handoff"
    );
    assert!(
        matches!(
            store.record_task_bound(
                &spec(&owner_id, 3, task.task_id),
                "digest-wrong-epoch",
                &OwnerProof::new(owner_id.clone(), TO, epoch).unwrap()
            ),
            Err(StoreError::OwnerBindingConflict)
        ),
        "the incoming session revised at the outgoing epoch"
    );

    // The action the store derives `resumed` from: a decision at the committed
    // epoch by the incoming session.
    store
        .record_decision(&decision(&owner_id, &task, &envelope, TO, epoch + 1))
        .unwrap();
    assert_eq!(state(&store, takeover_id), TakeoverState::Resumed);
    assert_eq!(
        kinds(&store, takeover_id),
        vec![
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed,
            TakeoverState::Resumed
        ]
    );

    // It is derived once: a later owner action does not append a second one.
    store
        .record_task_bound(&spec(&owner_id, 3, task.task_id), "digest-after", &incoming)
        .unwrap();
    assert_eq!(
        kinds(&store, takeover_id)
            .iter()
            .filter(|kind| **kind == TakeoverState::Resumed)
            .count(),
        1
    );
}

#[test]
fn retire_before_resumed_is_refused_and_a_failed_retirement_does_not_revert_resumed() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );

    assert!(
        matches!(
            store.retire_takeover(takeover_id, RetireOutcome::Completed),
            Err(StoreError::TakeoverStateConflict {
                from: TakeoverState::Prepared,
                to: TakeoverState::Retired,
                ..
            })
        ),
        "a prepared handoff retired itself"
    );
    store.commit_takeover(takeover_id).unwrap();
    assert!(
        matches!(
            store.retire_takeover(takeover_id, RetireOutcome::Completed),
            Err(StoreError::TakeoverStateConflict {
                from: TakeoverState::Committed,
                ..
            })
        ),
        "a committed but unresumed handoff retired itself"
    );

    store
        .record_decision(&decision(&owner_id, &task, &envelope, TO, epoch + 1))
        .unwrap();

    // A failure to close is its own report and does not walk the handoff back.
    let failed = store
        .fail_retirement(takeover_id, "the old pane was already gone")
        .unwrap();
    assert_eq!(failed.state, TakeoverState::RetireFailed);
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("the old pane was already gone")
    );
    assert!(
        failed.state.is_terminal(),
        "a failed close left the handoff open"
    );
    assert_eq!(
        kinds(&store, takeover_id),
        vec![
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed,
            TakeoverState::Resumed,
            TakeoverState::RetireFailed
        ],
        "the resumed step must still be on the record"
    );
    assert!(
        store
            .retire_takeover(takeover_id, RetireOutcome::Completed)
            .is_err(),
        "a failed close was later turned into a clean one"
    );
}

#[test]
fn a_resumed_handoff_can_be_retired_and_a_retirement_records_its_outcome() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );
    store.commit_takeover(takeover_id).unwrap();
    store
        .record_decision(&decision(&owner_id, &task, &envelope, TO, epoch + 1))
        .unwrap();

    let retired = store
        .retire_takeover(takeover_id, RetireOutcome::Completed)
        .unwrap();
    assert_eq!(retired.state, TakeoverState::Retired);
    assert_eq!(retired.failure_reason, None);
    assert_eq!(
        store
            .takeover_events(takeover_id)
            .unwrap()
            .last()
            .and_then(|event| event.detail.clone()),
        Some("completed".to_owned()),
        "the retirement outcome is not on the record"
    );
    assert!(matches!(
        store.retire_takeover(takeover_id, RetireOutcome::Completed),
        Err(StoreError::TakeoverStateConflict {
            from: TakeoverState::Retired,
            ..
        })
    ));

    // The owner is free again once the handoff is closed.
    assert_eq!(
        store
            .request_takeover(&owner_id, TO, FROM, "handoff-2")
            .unwrap()
            .state,
        TakeoverState::Requested
    );
}

#[test]
fn abort_after_commit_is_refused() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );

    // A prepared handoff is still the owner's to stop.
    let second = store
        .request_takeover(&owner_id, FROM, "codex:session-third", "handoff-2")
        .err();
    assert!(matches!(second, Some(StoreError::TakeoverInProgress(_))));

    store.commit_takeover(takeover_id).unwrap();
    assert!(
        matches!(
            store.abort_takeover(takeover_id, "changed my mind"),
            Err(StoreError::TakeoverStateConflict {
                from: TakeoverState::Committed,
                to: TakeoverState::Aborted,
                ..
            })
        ),
        "a committed handoff was aborted"
    );
    assert!(
        matches!(
            store.abort_takeover(takeover_id, "   "),
            Err(StoreError::InvalidTakeoverRequest(_))
        ),
        "an empty reason was accepted"
    );
    assert_eq!(state(&store, takeover_id), TakeoverState::Committed);
    assert_eq!(
        recorded_binding(&store, &owner_id),
        Some((TO.to_owned(), epoch + 1))
    );
    // The owner stays busy until this handoff is closed, so no second one can
    // start from the outgoing session either.
    assert!(matches!(
        store.request_takeover(&owner_id, TO, FROM, "handoff-2"),
        Err(StoreError::TakeoverInProgress(open)) if open == takeover_id
    ));
}

fn recorded_binding(store: &Store, owner_id: &OwnerId) -> Option<(String, u64)> {
    store.owner_binding(owner_id).unwrap()
}

/// A task whose attempt is still running, so the worker can still send messages.
fn running_attempt(store: &mut Store, owner_id: &OwnerId) -> (TaskId, AttemptId) {
    let task = spec(owner_id, 1, TaskId::new());
    store.record_task(&task, "digest-running").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .set_attempt_state(attempt_id, AttemptState::Starting)
        .unwrap();
    store
        .set_attempt_state(attempt_id, AttemptState::Running)
        .unwrap();
    (task.task_id, attempt_id)
}

fn message(
    task_id: TaskId,
    attempt_id: AttemptId,
    direction: MessageDirection,
    kind: MessageKind,
    body: &str,
) -> MessageDraft {
    MessageDraft::new(task_id, attempt_id, direction, kind, body.to_owned(), None)
}

#[test]
fn a_restart_mid_handoff_reports_the_exact_state_reached() {
    let root = TempDir::new().unwrap();
    let owner_id = owner();
    let expected;
    let takeover_id;
    {
        let mut store = Store::open(root.path()).unwrap();
        head(&store, &owner_id);
        let (task, envelope) = candidate(&mut store, &owner_id);
        expected = manifest(FROM, &task, &envelope);
        takeover_id = prepared(&store, &owner_id, "handoff-1", &expected);
        assert_eq!(state(&store, takeover_id), TakeoverState::Prepared);
    }

    let store = Store::open(root.path()).unwrap();
    let reopened = store.takeover(takeover_id).unwrap().unwrap();
    assert_eq!(
        reopened.state,
        TakeoverState::Prepared,
        "the reopen reported a state the handoff never reached"
    );
    assert_eq!(reopened.from_session, FROM);
    assert_eq!(reopened.to_session, TO);
    assert_eq!(
        recorded_binding(&store, &owner_id),
        Some((FROM.to_owned(), 1)),
        "a prepared handoff moved the binding across a restart"
    );
    assert_eq!(
        kinds(&store, takeover_id),
        vec![TakeoverState::Requested, TakeoverState::Prepared]
    );
    assert_eq!(
        store
            .handoff_manifest(takeover_id)
            .unwrap()
            .expect("a prepared handoff keeps its manifest")
            .manifest,
        expected
    );
    drop(store);

    // The step that is still owed is taken after the restart.
    let store = Store::open(root.path()).unwrap();
    store.commit_takeover(takeover_id).unwrap();
    assert_eq!(state(&store, takeover_id), TakeoverState::Committed);
    drop(store);

    let store = Store::open(root.path()).unwrap();
    assert_eq!(state(&store, takeover_id), TakeoverState::Committed);
    assert_eq!(
        recorded_binding(&store, &owner_id),
        Some((TO.to_owned(), 2))
    );
    assert_eq!(
        kinds(&store, takeover_id),
        vec![
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed
        ]
    );
}

#[test]
fn a_pending_completion_notification_is_redelivered_to_the_new_session_after_commit() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    store
        .register_owner_surface(&owner_id, FROM, epoch, "w1:p1", None, "/bin/herdr")
        .unwrap();
    let (task, envelope) = candidate(&mut store, &owner_id);

    // Delivered to the outgoing head, so it is owed to nobody right now.
    let claimed = store
        .claim_notification(envelope.result_id, "token-a", 0, 60)
        .unwrap()
        .expect("the outgoing head can claim its own notification");
    assert_eq!(claimed.session_id, FROM);
    store
        .mark_notification_delivered(&claimed, "token-a")
        .unwrap();
    assert!(
        store
            .pending_notifications_for_task(task.task_id)
            .unwrap()
            .is_empty()
    );

    let takeover_id = prepared(
        &store,
        &owner_id,
        "handoff-1",
        &manifest(FROM, &task, &envelope),
    );
    store.commit_takeover(takeover_id).unwrap();

    // The result was still undecided when the head moved, so it belongs to
    // whoever is the head now: the delivered mark was withdrawn with the old
    // binding rather than duplicated by the handoff.
    assert!(
        store
            .claim_notification(envelope.result_id, "token-b", 0, 60)
            .unwrap()
            .is_none(),
        "the incoming head claimed a notification before it had a surface"
    );
    store
        .register_owner_surface(&owner_id, TO, epoch + 1, "w1:p2", None, "/bin/herdr")
        .unwrap();
    assert_eq!(
        store.pending_notification_tasks_for_session(TO).unwrap(),
        vec![task.task_id]
    );
    let redelivered = store
        .claim_notification(envelope.result_id, "token-b", 0, 60)
        .unwrap()
        .expect("the result is owed to the incoming head after the handoff");
    assert_eq!(redelivered.session_id, TO);
    assert_eq!(redelivered.binding_epoch, epoch + 1);
    assert_eq!(redelivered.pane_id, "w1:p2");
    assert!(
        store
            .pending_notification_tasks_for_session(FROM)
            .unwrap()
            .is_empty(),
        "the outgoing head is still being told about it"
    );
}

#[test]
fn the_handoff_manifest_holds_references_only_and_never_overwrites_a_decision() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);

    // The owner decides before the handoff is even requested.
    let decided = decision(&owner_id, &task, &envelope, FROM, epoch);
    store.record_decision(&decided).unwrap();

    let takeover = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();
    // A result the owner has already decided is not pending, so it cannot be
    // handed forward as work still owed; the decision itself lives in
    // `decisions` and the manifest only refers to the revision that produced it.
    assert!(
        matches!(
            store.prepare_takeover(takeover.takeover_id, &manifest(FROM, &task, &envelope)),
            Err(StoreError::InvalidHandoffManifest(_))
        ),
        "a decided result was carried forward as still pending"
    );
    let carried = HandoffManifest {
        pending_result_ids: vec![],
        ..manifest(FROM, &task, &envelope)
    };
    store
        .prepare_takeover(takeover.takeover_id, &carried)
        .unwrap();
    store.commit_takeover(takeover.takeover_id).unwrap();

    let stored = store
        .handoff_manifest(takeover.takeover_id)
        .unwrap()
        .expect("a prepared handoff keeps its manifest");
    assert!(
        stored.manifest.pending_result_ids.is_empty(),
        "a decided result was stored as pending: {:?}",
        stored.manifest.pending_result_ids
    );
    assert_eq!(
        stored.manifest.tasks,
        vec![ManifestTask {
            task_id: task.task_id,
            revision: task.revision,
            attempt_id: envelope.attempt_id,
        }]
    );
    assert_eq!(
        stored.digest,
        Store::handoff_manifest_digest(&stored.manifest).unwrap()
    );
    let text = format!("{:?}", stored.manifest);
    assert!(
        !text.contains("sealed-artifact-bytes") && !text.contains(&decided.reason),
        "the manifest copied result or decision content: {text}"
    );

    // The recorded decision is exactly what it was, and a second one is refused.
    let recorded = store
        .decision_for_result(envelope.result_id)
        .unwrap()
        .expect("the decision survived the handoff");
    assert_eq!(recorded.decision_id, decided.decision_id);
    assert_eq!(recorded.reason, decided.reason);
    assert!(matches!(
        store.record_decision(&Decision {
            decision_id: DecisionId::new(),
            verdict: DecisionVerdict::Rejected,
            ..decision(&owner_id, &task, &envelope, TO, epoch + 1)
        }),
        Err(StoreError::DecisionConflict(_))
    ));

    // Preparing again with the same content is accepted; different content is
    // not, so the incoming session cannot be handed two manifests.
    assert_eq!(
        store
            .prepare_takeover(takeover.takeover_id, &carried)
            .unwrap()
            .state,
        TakeoverState::Committed
    );
    assert!(matches!(
        store.prepare_takeover(
            takeover.takeover_id,
            &HandoffManifest {
                goal: "Something else".to_owned(),
                ..carried.clone()
            }
        ),
        Err(StoreError::HandoffManifestConflict(_))
    ));
}

#[test]
fn a_manifest_may_only_name_rows_the_outgoing_head_actually_held() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    let takeover = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();

    for bad in [
        HandoffManifest {
            goal: "   ".to_owned(),
            ..manifest(FROM, &task, &envelope)
        },
        HandoffManifest {
            pending_result_ids: vec![ResultId::new()],
            ..manifest(FROM, &task, &envelope)
        },
        HandoffManifest {
            pending_message_ids: vec!["6f1a4a2e-0000-4000-8000-000000000000".to_owned()],
            ..manifest(FROM, &task, &envelope)
        },
        HandoffManifest {
            tasks: vec![
                ManifestTask {
                    task_id: task.task_id,
                    revision: task.revision,
                    attempt_id: envelope.attempt_id,
                },
                ManifestTask {
                    task_id: task.task_id,
                    revision: task.revision,
                    attempt_id: envelope.attempt_id,
                },
            ],
            ..manifest(FROM, &task, &envelope)
        },
    ] {
        assert!(
            matches!(
                store.prepare_takeover(takeover.takeover_id, &bad),
                Err(StoreError::InvalidHandoffManifest(_))
            ),
            "a manifest that names nothing real was accepted: {bad:?}"
        );
    }
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Requested,
        "a refused manifest moved the takeover"
    );
}

#[test]
fn a_manifest_may_only_name_an_attempt_of_the_task_revision_it_names() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    head(&store, &owner_id);
    let (task, envelope) = candidate(&mut store, &owner_id);
    // A second task of the same owner, so its attempt is real and still owned:
    // only the link the manifest asserts is wrong.
    let (_, other_envelope) = candidate(&mut store, &owner_id);
    let takeover = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();

    for attempt_id in [AttemptId::new(), other_envelope.attempt_id] {
        let bad = HandoffManifest {
            tasks: vec![ManifestTask {
                task_id: task.task_id,
                revision: task.revision,
                attempt_id,
            }],
            ..bare_manifest(FROM)
        };
        assert!(
            matches!(
                store.prepare_takeover(takeover.takeover_id, &bad),
                Err(StoreError::InvalidHandoffManifest(_))
            ),
            "a manifest named attempt {attempt_id} against a task revision it does not \
             belong to"
        );
    }
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Requested,
        "a refused manifest moved the takeover"
    );

    // The attempt this revision really ran, so the refusal above is about the
    // attempt and nothing else.
    assert_eq!(
        store
            .prepare_takeover(takeover.takeover_id, &manifest(FROM, &task, &envelope))
            .unwrap()
            .state,
        TakeoverState::Prepared
    );
}

#[test]
fn a_manifest_may_only_name_rows_that_are_still_pending_for_this_owner() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let (decided_task, decided) = candidate(&mut store, &owner_id);
    let (_, acked) = candidate(&mut store, &owner_id);
    let (waiting_task, waiting) = candidate(&mut store, &owner_id);
    store
        .record_decision(&decision(&owner_id, &decided_task, &decided, FROM, epoch))
        .unwrap();
    store.acknowledge(&owner_id, acked.result_id).unwrap();

    // A still-running attempt, so the worker can ask while the head is busy.
    let (asking, running_attempt) = running_attempt(&mut store, &owner_id);
    let open = store
        .post_message(&message(
            asking,
            running_attempt,
            MessageDirection::WorkerToOwner,
            MessageKind::Question,
            "Which branch?",
        ))
        .unwrap();
    let read = store
        .post_message(&message(
            asking,
            running_attempt,
            MessageDirection::WorkerToOwner,
            MessageKind::Question,
            "Still which branch?",
        ))
        .unwrap();
    let owner_note = store
        .post_message(&message(
            asking,
            running_attempt,
            MessageDirection::OwnerToWorker,
            MessageKind::Note,
            "Going with main",
        ))
        .unwrap();
    store
        .acknowledge_message(
            asking,
            running_attempt,
            MessageDirection::WorkerToOwner,
            &read.message_id,
        )
        .unwrap();

    let takeover = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();
    for bad in [
        HandoffManifest {
            pending_result_ids: vec![decided.result_id],
            ..bare_manifest(FROM)
        },
        HandoffManifest {
            pending_result_ids: vec![acked.result_id],
            ..bare_manifest(FROM)
        },
        HandoffManifest {
            pending_message_ids: vec![read.message_id.clone()],
            ..bare_manifest(FROM)
        },
        HandoffManifest {
            pending_message_ids: vec![owner_note.message_id.clone()],
            ..bare_manifest(FROM)
        },
    ] {
        assert!(
            matches!(
                store.prepare_takeover(takeover.takeover_id, &bad),
                Err(StoreError::InvalidHandoffManifest(_))
            ),
            "a manifest named a settled row as pending: {bad:?}"
        );
    }
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Requested,
        "a refused manifest moved the takeover"
    );

    // What really is still owed to the head goes through.
    assert_eq!(
        store
            .prepare_takeover(
                takeover.takeover_id,
                &HandoffManifest {
                    tasks: vec![ManifestTask {
                        task_id: waiting_task.task_id,
                        revision: waiting_task.revision,
                        attempt_id: waiting.attempt_id,
                    }],
                    pending_result_ids: vec![waiting.result_id],
                    pending_message_ids: vec![open.message_id.clone()],
                    ..bare_manifest(FROM)
                }
            )
            .unwrap()
            .state,
        TakeoverState::Prepared
    );
}

#[test]
fn a_stale_session_cannot_send_or_acknowledge_as_the_owner() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let task = spec(&owner_id, 1, TaskId::new());
    store.record_task(&task, "digest-task").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .set_attempt_state(attempt_id, AttemptState::Starting)
        .unwrap();
    store
        .set_attempt_state(attempt_id, AttemptState::Running)
        .unwrap();

    let question = MessageDraft::new(
        task.task_id,
        attempt_id,
        MessageDirection::WorkerToOwner,
        MessageKind::Question,
        "Which branch?".to_owned(),
        None,
    );
    store.post_message(&question).unwrap();

    // The head moves while the worker is still running and still asking.
    let takeover_id = prepared(&store, &owner_id, "handoff-1", &bare_manifest(FROM));
    store.commit_takeover(takeover_id).unwrap();

    let stale = OwnerProof::new(owner_id.clone(), FROM, epoch).unwrap();
    let current = OwnerProof::new(owner_id.clone(), TO, epoch + 1).unwrap();
    let reply = || {
        MessageDraft::new(
            task.task_id,
            attempt_id,
            MessageDirection::OwnerToWorker,
            MessageKind::Reply,
            "main".to_owned(),
            Some(question.message_id.clone()),
        )
    };
    let owed = || {
        store
            .unanswered_questions(task.task_id, attempt_id, MessageDirection::WorkerToOwner)
            .unwrap()
            .len()
    };
    assert_eq!(owed(), 1);

    assert!(matches!(
        store.post_message_bound(&reply(), &stale),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(matches!(
        store.acknowledge_message_bound(
            task.task_id,
            attempt_id,
            MessageDirection::WorkerToOwner,
            &question.message_id,
            &stale
        ),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert_eq!(
        owed(),
        1,
        "the stale head answered or acknowledged on its way out"
    );

    store.post_message_bound(&reply(), &current).unwrap();
    assert_eq!(owed(), 0);

    // A proof that is current but names somebody else's task is refused too.
    let other = spec(
        &OwnerId::new("codex:someone-else").unwrap(),
        1,
        TaskId::new(),
    );
    store.record_task(&other, "digest-other").unwrap();
    assert!(matches!(
        store.record_cancellation_intents_bound(other.task_id, false, &current),
        Err(StoreError::OwnerProofMismatch)
    ));
    store
        .acknowledge_message_bound(
            task.task_id,
            attempt_id,
            MessageDirection::WorkerToOwner,
            &question.message_id,
            &current,
        )
        .unwrap();
}

#[test]
fn a_stale_session_cannot_cancel_or_record_a_revision() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let epoch = head(&store, &owner_id);
    let task = spec(&owner_id, 1, TaskId::new());
    store.record_task(&task, "digest-task").unwrap();

    let takeover = store
        .request_takeover(&owner_id, FROM, TO, "handoff-1")
        .unwrap();
    store
        .prepare_takeover(takeover.takeover_id, &bare_manifest(FROM))
        .unwrap();
    store.commit_takeover(takeover.takeover_id).unwrap();
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Committed
    );

    // The session that used to be the head cannot cancel or revise anything.
    let stale = OwnerProof::new(owner_id.clone(), FROM, epoch).unwrap();
    let current = OwnerProof::new(owner_id.clone(), TO, epoch + 1).unwrap();
    assert!(matches!(
        store.record_cancellation_intents_bound(task.task_id, false, &stale),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(matches!(
        store.record_task_bound(&spec(&owner_id, 2, task.task_id), "digest-revision", &stale),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(
        !store.cancellation_requested(task.task_id).unwrap(),
        "the stale head cancelled a task it no longer owns"
    );
    assert_eq!(
        store.task(task.task_id).unwrap().revision,
        1,
        "a refused revision was recorded anyway"
    );
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Committed
    );

    // The incoming session can do both, and the revision it records is what
    // resumes the handoff.
    assert_eq!(
        store
            .record_cancellation_intents_bound(task.task_id, false, &current)
            .unwrap()
            .len(),
        1
    );
    store
        .record_task_bound(
            &spec(&owner_id, 2, task.task_id),
            "digest-revision",
            &current,
        )
        .unwrap();
    assert_eq!(store.task(task.task_id).unwrap().revision, 2);
    assert_eq!(
        state(&store, takeover.takeover_id),
        TakeoverState::Resumed,
        "the incoming session's new revision did not resume the handoff"
    );
}

/// The schema text as it stood before the handoff tables existed, verbatim.
/// Everything an older build's `$BRGR_HOME` contains is here, and nothing this
/// change added.
const PRE_TAKEOVER_SCHEMA: &str = r"
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
CREATE TABLE IF NOT EXISTS attempt_clocks (
    attempt_id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
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
CREATE INDEX IF NOT EXISTS delegation_edges_parent ON delegation_edges (parent_task_id);
CREATE TABLE IF NOT EXISTS cancellation_intents (
    task_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    requested_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS task_checkouts (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    primary_checkout TEXT NOT NULL,
    PRIMARY KEY (task_id, revision)
);
CREATE TABLE IF NOT EXISTS task_run_completions (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    result_id TEXT NOT NULL UNIQUE,
    completed_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, revision)
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
CREATE TABLE IF NOT EXISTS withdrawn_questions (
    message_id TEXT PRIMARY KEY,
    FOREIGN KEY (message_id) REFERENCES task_messages(message_id)
);
CREATE TABLE IF NOT EXISTS owner_surfaces (
    owner_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    binding_epoch INTEGER NOT NULL,
    pane_id TEXT NOT NULL,
    herdr_session TEXT,
    herdr_bin TEXT NOT NULL,
    FOREIGN KEY (owner_id) REFERENCES owner_bindings(owner_id)
);
CREATE TABLE IF NOT EXISTS question_notices (
    message_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (message_id, session_id),
    FOREIGN KEY (message_id) REFERENCES task_messages(message_id)
);
CREATE TABLE IF NOT EXISTS completion_notifications (
    result_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0 CHECK (resolved IN (0, 1)),
    delivered_session TEXT,
    delivered_epoch INTEGER,
    delivered_pane TEXT,
    claim_token TEXT,
    claim_until INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE INDEX IF NOT EXISTS completion_notifications_owner
ON completion_notifications (owner_id, resolved, delivered_session);
CREATE INDEX IF NOT EXISTS results_task_revision ON results (task_id, revision);
CREATE INDEX IF NOT EXISTS attempts_task_revision ON attempts (task_id, revision);
CREATE TABLE IF NOT EXISTS native_message_deliveries (
    message_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    state TEXT NOT NULL,
    error TEXT
);
CREATE TABLE IF NOT EXISTS debate_groups (
    group_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS debate_members (
    group_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    PRIMARY KEY (group_id, task_id),
    FOREIGN KEY (group_id) REFERENCES debate_groups(group_id)
);
CREATE TABLE IF NOT EXISTS peer_messages (
    message_id TEXT PRIMARY KEY,
    group_id TEXT NOT NULL,
    from_task TEXT NOT NULL,
    to_task TEXT NOT NULL,
    from_attempt TEXT NOT NULL,
    to_attempt TEXT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    in_reply_to TEXT,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (group_id) REFERENCES debate_groups(group_id)
);
";

#[test]
fn a_store_written_before_the_handoff_tables_opens_unchanged() {
    // `0` is what a store written before the stamp existed carries; a nonzero
    // value is another build's digest. Both are read paths an open must
    // survive, and both must gain the new tables without touching old rows.
    for stamp in [0_i64, 4_242] {
        let root = TempDir::new().unwrap();
        let owner_id = owner();
        let task = spec(&owner_id, 1, TaskId::new());
        write_pre_takeover_store(root.path(), &owner_id, &task, stamp);

        let store = Store::open(root.path()).unwrap();
        assert_eq!(
            store.owner_binding(&owner_id).unwrap(),
            Some((FROM.to_owned(), 1)),
            "the old binding did not survive the open (stamp {stamp})"
        );
        assert_eq!(
            store.task(task.task_id).unwrap().objective,
            task.objective,
            "the old task did not survive the open (stamp {stamp})"
        );
        assert_eq!(
            store
                .request_takeover(&owner_id, FROM, TO, "handoff-1")
                .unwrap()
                .state,
            TakeoverState::Requested,
            "the handoff tables were not applied to a store at stamp {stamp}"
        );
        drop(store);

        let connection = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        for expected in ["head_takeovers", "takeover_events", "handoff_manifests"] {
            assert!(
                tables.contains(&expected.to_owned()),
                "{expected} is missing from a store written at stamp {stamp}"
            );
        }
        // The table the handoff hangs off is exactly the one that was there. An
        // added column would not be applied to a file that already has the
        // table, which is why this checks the columns and not the tables.
        let columns = connection
            .prepare("SELECT name FROM pragma_table_info('owner_bindings')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(columns, vec!["owner_id", "session_id", "binding_epoch"]);
    }
}

fn write_pre_takeover_store(root: &Path, owner_id: &OwnerId, task: &TaskSpec, stamp: i64) {
    let connection = Connection::open(root.join("brgr.sqlite3")).unwrap();
    connection.execute_batch(PRE_TAKEOVER_SCHEMA).unwrap();
    connection
        .execute(
            "INSERT INTO owner_bindings (owner_id, session_id, binding_epoch)
             VALUES (?1, ?2, 1)",
            rusqlite::params![owner_id.as_str(), FROM],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks (task_id, revision, owner_id, create_request_id, request_digest,
                spec_json)
             VALUES (?1, ?2, ?3, ?4, 'digest-old', ?5)",
            rusqlite::params![
                task.task_id.to_string(),
                task.revision,
                owner_id.as_str(),
                task.create_request_id,
                serde_json::to_string(task).unwrap()
            ],
        )
        .unwrap();
    connection
        .pragma_update(None, "user_version", stamp)
        .unwrap();
}
