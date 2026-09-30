use std::time::{Duration, Instant};

use super::fixtures::{result, sealed_result, task};

/// Every source file the write-path guards read. Checked against the modules
/// `lib.rs` declares, so a new module cannot quietly escape them.
const SCANNED: [(&str, &str); 16] = [
    ("lib.rs", include_str!("lib.rs")),
    ("artifact.rs", include_str!("artifact.rs")),
    ("attempt.rs", include_str!("attempt.rs")),
    ("board.rs", include_str!("board.rs")),
    ("contention.rs", include_str!("contention.rs")),
    ("delegation.rs", include_str!("delegation.rs")),
    ("error.rs", include_str!("error.rs")),
    ("fixtures.rs", include_str!("fixtures.rs")),
    ("message.rs", include_str!("message.rs")),
    ("notification.rs", include_str!("notification.rs")),
    ("owner.rs", include_str!("owner.rs")),
    ("result.rs", include_str!("result.rs")),
    ("schema.rs", include_str!("schema.rs")),
    ("task.rs", include_str!("task.rs")),
    ("tests.rs", include_str!("tests.rs")),
    ("tree.rs", include_str!("tree.rs")),
];

use super::contention::{BUSY_RETRY_BACKOFF, is_lock_contention};
use brgr_protocol::{
    ArtifactContract, DecisionId, DecisionVerdict, EventId, EventKind, ObservationSource, OwnerId,
    SCHEMA_V1, TaskId, TerminalOutcome,
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
    let result = sealed_result(&store, &task, attempt_id);

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
fn event_sequence_and_owner_binding_are_idempotent() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "digest-event").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let event = Event {
        schema: SCHEMA_V1.to_owned(),
        event_id: EventId::new(),
        attempt_id,
        producer: "fixture".to_owned(),
        producer_seq: 1,
        kind: EventKind::Running,
        payload: serde_json::json!({}),
    };
    assert_eq!(store.record_event(&event).unwrap(), WriteOutcome::Inserted);
    assert_eq!(
        store.record_event(&event).unwrap(),
        WriteOutcome::AlreadyApplied
    );
    let conflicting = Event {
        event_id: EventId::new(),
        ..event
    };
    assert!(matches!(
        store.record_event(&conflicting),
        Err(StoreError::EventConflict)
    ));

    assert_eq!(
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap(),
        WriteOutcome::Inserted
    );
    assert_eq!(
        store.bind_owner(&task.owner_id, "session-a", 1).unwrap(),
        WriteOutcome::AlreadyApplied
    );
    assert!(matches!(
        store.bind_owner(&task.owner_id, "session-b", 1),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(matches!(
        store.bind_owner(&task.owner_id, "session-b", 2),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
    assert!(matches!(
        store.bind_owner(&task.owner_id, "session-a", 100),
        Err(StoreError::OwnerBindingConflict)
    ));
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
    let result = sealed_result(&store, &task, attempt_id);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: task.owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result).unwrap(),
        session_id: Some("session-a".to_owned()),
        binding_epoch: Some(1),
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
fn stale_binding_epoch_cannot_decide_or_replay_after_rebind() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "binding-epoch").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert!(matches!(
        store.record_decision(&Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "fixture".to_owned(),
        }),
        Err(StoreError::OwnerUnbound(_))
    ));
    assert_eq!(store.rebind_owner(&task.owner_id, "session-a").unwrap(), 1);
    let old = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: task.owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result).unwrap(),
        session_id: Some("session-a".to_owned()),
        binding_epoch: Some(1),
        verdict: DecisionVerdict::Accepted,
        reason: "fixture".to_owned(),
    };
    assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
    assert!(matches!(
        store.acknowledge_bound(&task.owner_id, result.result_id, "session-a", 1),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(matches!(
        store.record_decision_and_ack(&old),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    let current = Decision {
        session_id: Some("session-b".to_owned()),
        binding_epoch: Some(2),
        ..old
    };
    store.record_decision_and_ack(&current).unwrap();
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
}

#[test]
fn session_task_list_filters_before_applying_the_limit() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owned = task();
    store.record_task(&owned, "owned").unwrap();
    store.bind_owner(&owned.owner_id, "session-a", 1).unwrap();
    let other_owner = OwnerId::new("codex:other").unwrap();
    store.bind_owner(&other_owner, "session-b", 1).unwrap();
    for number in 0..21 {
        let mut other = task();
        other.owner_id = other_owner.clone();
        other.create_request_id = format!("other-{number}");
        store
            .record_task(&other, &format!("digest-{number}"))
            .unwrap();
    }
    let listed = store.tasks_for_session("session-a", None, 20).unwrap();
    assert_eq!(listed, vec![owned]);
    assert!(
        store
            .tasks_for_session("session-a", Some(&other_owner), 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn transferred_owner_pending_inbox_reappears_in_new_session() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let first = task();
    let mut second = task();
    second.owner_id = OwnerId::new("codex:transferred").unwrap();
    second.create_request_id = "transferred-task".to_owned();
    for (index, spec) in [first.clone(), second.clone()].iter().enumerate() {
        store
            .record_task(spec, &format!("pending-{index}"))
            .unwrap();
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(spec.task_id, spec.revision, attempt_id)
            .unwrap();
        store
            .commit_terminal_result(
                &spec.owner_id,
                &ResultEnvelope {
                    outcome: TerminalOutcome::Failed,
                    error: Some("fixture".to_owned()),
                    ..result(spec, attempt_id)
                },
            )
            .unwrap();
    }
    store.bind_owner(&first.owner_id, "session-a", 1).unwrap();
    store.bind_owner(&second.owner_id, "session-b", 1).unwrap();
    assert_eq!(store.pending_for_session("session-a").unwrap().len(), 1);
    store.rebind_owner(&second.owner_id, "session-a").unwrap();
    let pending = store.pending_for_session("session-a").unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|item| item.owner_id == second.owner_id));
    store
        .acknowledge_bound(
            &first.owner_id,
            pending
                .iter()
                .find(|item| item.owner_id == first.owner_id)
                .unwrap()
                .result
                .result_id,
            "session-a",
            1,
        )
        .unwrap();
    assert_eq!(store.pending_for_session("session-a").unwrap().len(), 1);
}

#[test]
fn completion_delivery_claims_once_and_retargets_after_session_transfer() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt = AttemptId::new();
    store.record_task(&task, "completion-delivery").unwrap();
    store.create_attempt(task.task_id, 1, attempt).unwrap();
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    store
        .register_owner_surface(
            &task.owner_id,
            "session-a",
            1,
            "w1:p1",
            Some("test-session"),
            "/bin/herdr",
        )
        .unwrap();
    let result = sealed_result(&store, &task, attempt);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    let pending = store.pending_notifications_for_task(task.task_id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].result_id, result.result_id);
    let first = store
        .claim_notification(result.result_id, "claim-a", 100, 20)
        .unwrap()
        .unwrap();
    assert_eq!(first.pane_id, "w1:p1");
    assert!(
        store
            .claim_notification(result.result_id, "other", 101, 20)
            .unwrap()
            .is_none()
    );
    store
        .mark_notification_delivered(&first, "claim-a")
        .unwrap();
    assert!(
        store
            .pending_notifications_for_task(task.task_id)
            .unwrap()
            .is_empty()
    );

    assert_eq!(store.rebind_owner(&task.owner_id, "session-b").unwrap(), 2);
    assert!(
        store
            .claim_notification(result.result_id, "claim-b", 200, 20)
            .unwrap()
            .is_none()
    );
    store
        .register_owner_surface(&task.owner_id, "session-b", 2, "w2:p4", None, "/bin/herdr")
        .unwrap();
    let second = store
        .claim_notification(result.result_id, "claim-b", 200, 20)
        .unwrap()
        .unwrap();
    assert_eq!(second.session_id, "session-b");
    assert_eq!(second.pane_id, "w2:p4");
    assert!(matches!(
        store.mark_notification_delivered(&first, "claim-a"),
        Err(StoreError::NotificationClaimStale)
    ));
    store
        .mark_notification_delivered(&second, "claim-b")
        .unwrap();
    store
        .acknowledge_bound(&task.owner_id, result.result_id, "session-b", 2)
        .unwrap();
    assert!(
        store
            .pending_notifications_for_task(task.task_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_result_cannot_be_accepted_through_store_api() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "failed-decision").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = ResultEnvelope {
        outcome: TerminalOutcome::Failed,
        error: Some("no artifact".to_owned()),
        ..result(&task, attempt_id)
    };
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: task.owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result).unwrap(),
        session_id: Some("session-a".to_owned()),
        binding_epoch: Some(1),
        verdict: DecisionVerdict::Accepted,
        reason: "must fail".to_owned(),
    };
    assert!(matches!(
        store.record_decision_and_ack(&decision),
        Err(StoreError::DecisionRequiresCandidate)
    ));
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
}

#[test]
fn candidate_without_sealed_bytes_cannot_enter_inbox() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "unsealed-decision").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = result(&task, attempt_id);
    assert!(matches!(
        store.commit_terminal_result(&task.owner_id, &result),
        Err(StoreError::CandidateRequiresArtifact)
    ));
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
}

#[test]
fn forged_candidate_artifact_cannot_enter_inbox() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "forged-artifact").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let mut result = sealed_result(&store, &task, attempt_id);
    result.artifacts[0].store_relative_path = "../outside".to_owned();
    assert!(matches!(
        store.commit_terminal_result(&task.owner_id, &result),
        Err(StoreError::InvalidArtifactReference)
    ));
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
}

#[test]
fn stale_attempt_writer_cannot_overwrite_newer_or_terminal_state() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "attempt-cas").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    assert!(matches!(
        store.compare_and_set_attempt_state(
            attempt_id,
            AttemptState::Queued,
            AttemptState::CancelRequested
        ),
        Err(StoreError::AttemptStateConflict {
            actual: AttemptState::Starting,
            ..
        })
    ));
    assert!(matches!(
        store.set_attempt_state(attempt_id, AttemptState::Queued),
        Err(StoreError::AttemptTransitionInvalid { .. })
    ));
    store
        .commit_terminal_result(&task.owner_id, &sealed_result(&store, &task, attempt_id))
        .unwrap();
    assert!(matches!(
        store.set_attempt_state(attempt_id, AttemptState::Running),
        Err(StoreError::AttemptTransitionInvalid { .. })
    ));
    assert_eq!(
        store.attempt_state_by_id(attempt_id).unwrap(),
        AttemptState::Terminal
    );
}

#[test]
fn two_store_connections_cannot_claim_overlapping_attempts() {
    let root = TempDir::new().unwrap();
    let mut first = Store::open(root.path()).unwrap();
    let task = task();
    first.record_task(&task, "claim-attempt").unwrap();
    let second = Store::open(root.path()).unwrap();
    let first_id = AttemptId::new();
    first
        .claim_attempt(task.task_id, task.revision, first_id)
        .unwrap();
    assert!(matches!(
        second.claim_attempt(task.task_id, task.revision, AttemptId::new()),
        Err(StoreError::ActiveAttemptExists { .. })
    ));
    let retryable = ResultEnvelope {
        outcome: TerminalOutcome::Failed,
        artifacts: vec![],
        error: Some("pre-spawn fixture failure".to_owned()),
        ..result(&task, first_id)
    };
    first
        .commit_terminal_result(&task.owner_id, &retryable)
        .unwrap();
    assert!(matches!(
        second.claim_attempt(task.task_id, task.revision, AttemptId::new()),
        Err(StoreError::NonRetryablePriorAttempt { .. })
    ));
    first.grant_pre_spawn_retry(first_id).unwrap();
    second
        .claim_attempt(task.task_id, task.revision, AttemptId::new())
        .unwrap();
}

#[test]
fn launch_intent_survives_reopen_and_cannot_be_replaced() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "launch-intent").unwrap();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .record_launch_intent(attempt_id, "nonce-a", 7)
        .unwrap();
    let identity = RunnerIdentity {
        namespace: "process".to_owned(),
        handle: "4242".to_owned(),
        birth_marker: "kernel-start-123".to_owned(),
    };
    assert_eq!(
        store
            .record_runner_identity(attempt_id, "nonce-a", &identity)
            .unwrap(),
        WriteOutcome::Inserted
    );
    drop(store);

    let reopened = Store::open(root.path()).unwrap();
    let unfinished = reopened.unfinished_attempts().unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].launch.as_ref().unwrap().nonce, "nonce-a");
    assert_eq!(
        unfinished[0].launch.as_ref().unwrap().runner_identity,
        Some(identity.clone())
    );
    assert!(matches!(
        reopened.record_launch_intent(attempt_id, "nonce-b", 8),
        Err(StoreError::LaunchIntentConflict(_))
    ));
    assert!(matches!(
        reopened.record_runner_identity(
            attempt_id,
            "nonce-a",
            &RunnerIdentity {
                birth_marker: "reused-pid".to_owned(),
                ..identity
            }
        ),
        Err(StoreError::RunnerIdentityConflict(_))
    ));
}

#[test]
fn recovery_observation_cannot_overwrite_concurrent_runner_result() {
    let root = TempDir::new().unwrap();
    let mut first = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    first.record_task(&task, "recovery-race").unwrap();
    first
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    first
        .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    first
        .record_launch_intent(attempt_id, "nonce-race", 1)
        .unwrap();
    let observed = first.unfinished_attempts().unwrap().remove(0);
    let mut second = Store::open(root.path()).unwrap();
    let completed = sealed_result(&second, &task, attempt_id);
    second
        .commit_terminal_result(&task.owner_id, &completed)
        .unwrap();
    let lost = ResultEnvelope {
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Lost,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec!["unknown".to_owned()],
        ..completed.clone()
    };
    assert!(matches!(
        first.commit_recovered_lost(&observed, &lost),
        Err(StoreError::RecoveryObservationStale(_))
    ));
    assert_eq!(first.inbox(&task.owner_id, false).unwrap().len(), 1);
    assert_eq!(first.latest_result(task.task_id).unwrap(), completed);
}

#[test]
fn recovery_observation_cannot_ignore_identity_recorded_after_snapshot() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "identity-race").unwrap();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    store
        .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .record_launch_intent(attempt_id, "nonce-identity", 1)
        .unwrap();
    let observed = store.unfinished_attempts().unwrap().remove(0);
    let other = Store::open(root.path()).unwrap();
    other
        .record_runner_identity(
            attempt_id,
            "nonce-identity",
            &RunnerIdentity {
                namespace: "process".to_owned(),
                handle: "55".to_owned(),
                birth_marker: "start-55".to_owned(),
            },
        )
        .unwrap();
    let lost = ResultEnvelope {
        outcome: TerminalOutcome::Lost,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec!["unknown".to_owned()],
        ..result(&task, attempt_id)
    };
    assert!(matches!(
        store.commit_recovered_lost(&observed, &lost),
        Err(StoreError::RecoveryObservationStale(_))
    ));
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
}

#[test]
fn decision_and_ack_are_atomic_and_semantic_retries_are_idempotent() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "decision-atomic").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: task.owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result).unwrap(),
        session_id: Some("session-a".to_owned()),
        binding_epoch: Some(1),
        verdict: DecisionVerdict::Accepted,
        reason: "verified".to_owned(),
    };
    let wrong = Decision {
        result_digest: "wrong".to_owned(),
        ..decision.clone()
    };
    assert!(matches!(
        store.record_decision_and_ack(&wrong),
        Err(StoreError::ResultDigestMismatch)
    ));
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
    assert_eq!(
        store.record_decision_and_ack(&decision).unwrap(),
        WriteOutcome::Inserted
    );
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    let replay = Decision {
        decision_id: DecisionId::new(),
        ..decision.clone()
    };
    assert_eq!(
        store.record_decision_and_ack(&replay).unwrap(),
        WriteOutcome::AlreadyApplied
    );
    let conflict = Decision {
        reason: "changed".to_owned(),
        ..replay
    };
    assert!(matches!(
        store.record_decision_and_ack(&conflict),
        Err(StoreError::DecisionConflict(_))
    ));
    drop(store);
    let store = Store::open(root.path()).unwrap();
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    assert_eq!(store.inbox(&task.owner_id, true).unwrap().len(), 1);
}

#[test]
fn failed_ack_rolls_back_decision_insert() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    let attempt_id = AttemptId::new();
    store.record_task(&task, "missing-inbox").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_ack BEFORE UPDATE ON inbox_items
         BEGIN SELECT RAISE(ABORT, 'fixture ack failure'); END;",
        )
        .unwrap();
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: task.owner_id.clone(),
        task_id: task.task_id,
        revision: task.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result).unwrap(),
        session_id: Some("session-a".to_owned()),
        binding_epoch: Some(1),
        verdict: DecisionVerdict::Accepted,
        reason: "verified".to_owned(),
    };
    assert!(matches!(
        store.record_decision_and_ack(&decision),
        Err(StoreError::Database(_))
    ));
    let count: u32 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM decisions WHERE result_id = ?1",
            [result.result_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
}

#[test]
fn terminal_result_rolls_back_if_inbox_insert_fails() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "terminal-rollback").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_inbox BEFORE INSERT ON inbox_items
             BEGIN SELECT RAISE(ABORT, 'fixture inbox failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        store.commit_terminal_result(&task.owner_id, &result),
        Err(StoreError::Database(_))
    ));
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    assert_eq!(
        store.attempt_state_by_id(attempt_id).unwrap(),
        AttemptState::Queued
    );
    store
        .connection
        .execute_batch("DROP TRIGGER fail_inbox")
        .unwrap();
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
}

#[test]
fn database_busy_keeps_terminal_result_retriable_without_partial_inbox() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    store.connection.busy_timeout(Duration::ZERO).unwrap();
    let task = task();
    store.record_task(&task, "busy-terminal").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);
    let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    // The un-retried body is asserted directly: every public entry point
    // wraps it in `retry_busy`, which would wait out the whole retry budget
    // against a blocker that never releases. Retry behavior is covered by
    // `retry_busy_gives_up_only_after_its_budget`.
    let prepared = store.prepare_terminal_result(&result).unwrap();
    assert!(matches!(
        store.commit_terminal_result_once(&task.owner_id, &result, &prepared, None, false),
        Err(StoreError::Database(_))
    ));
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    blocker.execute_batch("ROLLBACK").unwrap();
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
}

#[test]
fn sqlite_full_keeps_terminal_result_retriable_without_partial_commit() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    // DELETE mode grows the database file synchronously at commit, so the
    // page cap below fails deterministically. Production WAL shares the
    // same single-transaction rollback path this test exercises.
    store
        .connection
        .execute_batch("PRAGMA journal_mode=DELETE")
        .unwrap();
    let task = task();
    store.record_task(&task, "full-terminal").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let mut result = sealed_result(&store, &task, attempt_id);
    // Force overflow pages so the commit must grow the database file.
    result.error = Some("disk pressure".to_owned() + &"x".repeat(32_768));
    let page_count: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    store
        .connection
        .execute_batch(&format!("PRAGMA max_page_count={page_count}"))
        .unwrap();
    let error = store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            StoreError::Database(rusqlite::Error::SqliteFailure(inner, _))
                if inner.code == rusqlite::ErrorCode::DiskFull
        ),
        "unexpected commit error: {error:?}"
    );
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
    let stored_results: u32 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM results WHERE attempt_id = ?1",
            [result.attempt_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_results, 0);
    assert_eq!(
        store.attempt_state_by_id(attempt_id).unwrap(),
        AttemptState::Queued
    );
    store
        .connection
        .execute_batch("PRAGMA max_page_count=1073741823")
        .unwrap();
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
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

#[test]
fn task_cannot_exceed_its_total_attempt_budget() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "bounded-attempts").unwrap();
    for number in 0..2 {
        let attempt_id = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        for (from, to) in [
            (AttemptState::Queued, AttemptState::Starting),
            (AttemptState::Starting, AttemptState::Running),
            (AttemptState::Running, AttemptState::Collecting),
        ] {
            store
                .compare_and_set_attempt_state(attempt_id, from, to)
                .unwrap();
        }
        let failed = ResultEnvelope {
            outcome: TerminalOutcome::Failed,
            artifacts: vec![],
            error: Some("pre-spawn fixture failure".to_owned()),
            ..result(&task, attempt_id)
        };
        store
            .commit_terminal_result(&task.owner_id, &failed)
            .unwrap();
        if number == 0 {
            store.grant_pre_spawn_retry(attempt_id).unwrap();
        }
    }
    assert!(matches!(
        store.claim_attempt(task.task_id, task.revision, AttemptId::new()),
        Err(StoreError::AttemptBudgetExhausted { .. })
    ));
}

#[test]
fn admitted_task_is_visible_and_lost_attempt_cannot_relaunch() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "admission-test").unwrap();
    assert_eq!(store.unstarted_tasks().unwrap(), vec![task.clone()]);
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    assert!(store.unstarted_tasks().unwrap().is_empty());
    store
        .compare_and_set_attempt_state(attempt_id, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    let lost = ResultEnvelope {
        outcome: TerminalOutcome::Lost,
        error: Some("supervisor disappeared".to_owned()),
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec!["execution identity unknown".to_owned()],
        ..result(&task, attempt_id)
    };
    store.commit_terminal_result(&task.owner_id, &lost).unwrap();
    assert!(matches!(
        store.claim_attempt(task.task_id, task.revision, AttemptId::new()),
        Err(StoreError::UnresolvedPriorAttempt { .. })
    ));
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);
}

#[test]
fn board_rows_order_latest_revision_and_skip_malformed_history() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let mut recorded = Vec::new();
    for index in 0..21 {
        let mut item = task();
        item.objective = format!("SECRET_OBJECTIVE_{index}");
        item.create_request_id = format!("board-req-{index}");
        item.workspace = format!("/tmp/project-{index}");
        store
            .record_task(&item, &format!("board-digest-{index}"))
            .unwrap();
        recorded.push(item);
    }
    let mut revised = recorded[0].clone();
    revised.revision = 2;
    revised.create_request_id = "board-req-0-r2".to_owned();
    store.record_task(&revised, "board-digest-0-r2").unwrap();

    let bad_id = TaskId::new();
    store
        .connection
        .execute(
            "INSERT INTO tasks
             (task_id, revision, owner_id, create_request_id, request_digest, spec_json)
             VALUES (?1, 1, 'codex:test-owner', 'malformed-board', 'digest',
                     '{\"objective\":\"SECRET_MALFORMED_BYTES\"}')",
            [bad_id.to_string()],
        )
        .unwrap();

    let count = |store: &Store| {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    let before = count(&store);
    let rows = BoardStore::open_existing(root.path())
        .unwrap()
        .rows(20)
        .unwrap();
    assert!(
        BoardStore::open_existing(root.path())
            .unwrap()
            .rows(0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(count(&store), before);
    assert_eq!(rows.len(), 20);
    assert_eq!(rows[0].task_id, recorded[0].task_id);
    assert_eq!(rows[0].revision, 2);
    assert!(rows.iter().all(|row| row.task_id != recorded[1].task_id));
    assert!(rows.iter().all(|row| row.task_id != bad_id));
    let rendered = format!("{rows:?}");
    assert!(!rendered.contains("SECRET_OBJECTIVE"));
    assert!(!rendered.contains("SECRET_MALFORMED_BYTES"));
    assert!(!rendered.contains("reviewable report"));
}

#[test]
fn board_rows_join_latest_result_decision_and_survive_writer_lock() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let decided_task = task();
    store.record_task(&decided_task, "board-terminal").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(decided_task.task_id, decided_task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &decided_task, attempt_id);
    store
        .commit_terminal_result(&decided_task.owner_id, &result)
        .unwrap();
    store
        .bind_owner(&decided_task.owner_id, "session-a", 1)
        .unwrap();
    store
        .record_decision(&Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: decided_task.owner_id.clone(),
            task_id: decided_task.task_id,
            revision: decided_task.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "SECRET_REASON_NOT_FOR_BOARD".to_owned(),
        })
        .unwrap();

    let mut other = task();
    other.create_request_id = "board-other".to_owned();
    store.record_task(&other, "board-other").unwrap();
    let other_attempt = AttemptId::new();
    store
        .claim_attempt(other.task_id, other.revision, other_attempt)
        .unwrap();
    let other_result = sealed_result(&store, &other, other_attempt);
    store
        .commit_terminal_result(&other.owner_id, &other_result)
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE results SET envelope_json = 'not-json' WHERE result_id = ?1",
            [other_result.result_id.to_string()],
        )
        .unwrap();
    let rows = BoardStore::open_existing(root.path())
        .unwrap()
        .rows(20)
        .unwrap();
    assert_eq!(rows.len(), 2);
    let decided = rows
        .iter()
        .find(|row| row.task_id == decided_task.task_id)
        .unwrap();
    let broken = rows
        .iter()
        .find(|row| row.task_id == other.task_id)
        .unwrap();
    assert_eq!(decided.result_outcome, Some(TerminalOutcome::Candidate));
    assert_eq!(decided.decision_verdict, Some(DecisionVerdict::Accepted));
    assert_eq!(decided.attempt_state, AttemptState::Terminal);
    assert_eq!(broken.result_outcome, None);
    assert!(!format!("{rows:?}").contains("SECRET_REASON_NOT_FOR_BOARD"));
    assert!(!format!("{rows:?}").contains("reviewable report"));

    drop(store);
    let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let locked_rows = BoardStore::open_existing(root.path())
        .unwrap()
        .rows(20)
        .unwrap();
    assert_eq!(locked_rows.len(), 2);
    assert!(locked_rows.iter().any(|row| {
        row.decision_verdict == Some(DecisionVerdict::Accepted)
            && row.result_outcome == Some(TerminalOutcome::Candidate)
    }));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn board_rows_retry_across_concurrent_task_writes() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let seed = task();
    store.record_task(&seed, "board-seed").unwrap();
    let path = root.path().to_path_buf();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let mut writer = Store::open(&writer_path).unwrap();
        for index in 0..24 {
            let mut item = task();
            item.create_request_id = format!("concurrent-{index}");
            writer
                .record_task(&item, &format!("concurrent-digest-{index}"))
                .unwrap();
        }
    });
    let board = BoardStore::open_existing(&path).unwrap();
    for _ in 0..32 {
        board.rows(20).unwrap();
    }
    writer.join().unwrap();
    assert!(board.rows(20).unwrap().len() <= 20);
    assert!(!board.rows(20).unwrap().is_empty());
}

#[test]
fn open_applies_remaining_schema_when_tasks_already_exist() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path()).unwrap();
    let connection = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tasks (
                task_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                owner_id TEXT NOT NULL,
                create_request_id TEXT NOT NULL UNIQUE,
                request_digest TEXT NOT NULL,
                spec_json TEXT NOT NULL,
                PRIMARY KEY (task_id, revision)
            );",
        )
        .unwrap();
    drop(connection);
    let mut store = Store::open(root.path()).unwrap();
    let present = |name: &str| {
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(present("owner_bindings"), 1);
    assert_eq!(present("decisions"), 1);
    assert_eq!(present("one_active_attempt_per_revision"), 1);
    store.record_task(&task(), "partial-schema").unwrap();
}

#[test]
fn board_rows_omit_invalid_attempt_state_and_decision_without_valid_result() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();

    let invalid_state = task();
    store
        .record_task(&invalid_state, "board-invalid-state")
        .unwrap();
    let invalid_attempt = AttemptId::new();
    store
        .claim_attempt(
            invalid_state.task_id,
            invalid_state.revision,
            invalid_attempt,
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE attempts SET state = 'not-a-real-state' WHERE attempt_id = ?1",
            [invalid_attempt.to_string()],
        )
        .unwrap();

    let mut decided = task();
    decided.create_request_id = "board-orphan-decision".to_owned();
    store
        .record_task(&decided, "board-orphan-decision")
        .unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(decided.task_id, decided.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &decided, attempt_id);
    store
        .commit_terminal_result(&decided.owner_id, &result)
        .unwrap();
    store.bind_owner(&decided.owner_id, "session-a", 1).unwrap();
    store
        .record_decision(&Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: decided.owner_id.clone(),
            task_id: decided.task_id,
            revision: decided.revision,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "SECRET_ORPHAN_REASON".to_owned(),
        })
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE results SET envelope_json = 'not-json' WHERE result_id = ?1",
            [result.result_id.to_string()],
        )
        .unwrap();

    let rows = BoardStore::open_existing(root.path())
        .unwrap()
        .rows(20)
        .unwrap();
    assert!(rows.iter().all(|row| row.task_id != invalid_state.task_id));
    let broken = rows
        .iter()
        .find(|row| row.task_id == decided.task_id)
        .unwrap();
    assert_eq!(broken.result_outcome, None);
    assert_eq!(broken.decision_verdict, None);
    assert!(!format!("{rows:?}").contains("SECRET_ORPHAN_REASON"));

    let mut forged = result.clone();
    forged.task_id = TaskId::new();
    store
        .connection
        .execute(
            "UPDATE results SET envelope_json = ?1 WHERE result_id = ?2",
            params![
                serde_json::to_string(&forged).unwrap(),
                result.result_id.to_string()
            ],
        )
        .unwrap();
    let rows = BoardStore::open_existing(root.path())
        .unwrap()
        .rows(20)
        .unwrap();
    let forged = rows
        .iter()
        .find(|row| row.task_id == decided.task_id)
        .unwrap();
    assert_eq!(forged.result_outcome, None);
    assert_eq!(forged.decision_verdict, None);
}

#[test]
fn recursive_delegation_edges_bind_each_child_to_its_live_parent_attempt() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let root_task = task();
    store.record_task(&root_task, "root-digest").unwrap();
    let root_attempt = AttemptId::new();
    store
        .claim_attempt(root_task.task_id, 1, root_attempt)
        .unwrap();
    store
        .compare_and_set_attempt_state(root_attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(root_attempt, AttemptState::Starting, AttemptState::Running)
        .unwrap();

    let mut child = task();
    child.create_request_id = "request-child".to_owned();
    child.owner_id = OwnerId::new(format!("worker:{root_attempt}")).unwrap();
    store
        .record_child_task(&child, "child-digest", root_task.task_id, root_attempt)
        .unwrap();
    assert_eq!(
        store.delegation_parent(child.task_id).unwrap(),
        Some((root_task.task_id, root_attempt, 1))
    );
    let child_attempt = AttemptId::new();
    store
        .claim_attempt(child.task_id, 1, child_attempt)
        .unwrap();
    store
        .compare_and_set_attempt_state(child_attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(child_attempt, AttemptState::Starting, AttemptState::Running)
        .unwrap();

    let mut grandchild = task();
    grandchild.create_request_id = "request-grandchild".to_owned();
    grandchild.owner_id = OwnerId::new(format!("worker:{child_attempt}")).unwrap();
    store
        .record_child_task(
            &grandchild,
            "grandchild-digest",
            child.task_id,
            child_attempt,
        )
        .unwrap();
    assert_eq!(
        store.delegation_parent(grandchild.task_id).unwrap(),
        Some((child.task_id, child_attempt, 2))
    );
    let mut wrong_owner = task();
    wrong_owner.create_request_id = "request-wrong-owner".to_owned();
    assert!(matches!(
        store.record_child_task(&wrong_owner, "wrong-owner", child.task_id, child_attempt),
        Err(StoreError::InvalidDelegationParent)
    ));
    assert!(matches!(
        store.record_child_task(
            &wrong_owner,
            "missing-attempt",
            child.task_id,
            AttemptId::new()
        ),
        Err(StoreError::InvalidDelegationParent)
    ));
    assert!(matches!(
        store.task(wrong_owner.task_id),
        Err(StoreError::TaskNotFound(_))
    ));
}

#[test]
fn child_revision_keeps_its_parent_edge_and_rejects_unparented_write() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let parent = task();
    store.record_task(&parent, "parent").unwrap();
    let parent_attempt = AttemptId::new();
    store
        .claim_attempt(parent.task_id, 1, parent_attempt)
        .unwrap();
    store
        .compare_and_set_attempt_state(parent_attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(
            parent_attempt,
            AttemptState::Starting,
            AttemptState::Running,
        )
        .unwrap();
    let mut child = task();
    child.create_request_id = "child-first".to_owned();
    child.owner_id = OwnerId::new(format!("worker:{parent_attempt}")).unwrap();
    store
        .record_child_task(&child, "child-first", parent.task_id, parent_attempt)
        .unwrap();
    let mut revised = child.clone();
    revised.revision = 2;
    revised.create_request_id = "child-revised".to_owned();
    assert!(matches!(
        store.record_task(&revised, "child-revised"),
        Err(StoreError::InvalidDelegationParent)
    ));
    store
        .record_child_task(&revised, "child-revised", parent.task_id, parent_attempt)
        .unwrap();
    assert_eq!(store.task(child.task_id).unwrap().revision, 2);
    assert_eq!(
        store.delegation_parent(child.task_id).unwrap(),
        Some((parent.task_id, parent_attempt, 1))
    );
}

#[test]
fn subtree_cancellation_blocks_new_children_and_bounds_concurrency() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let mut parent = task();
    parent.max_concurrent_children = Some(1);
    store.record_task(&parent, "tree-parent").unwrap();
    let attempt = AttemptId::new();
    store.claim_attempt(parent.task_id, 1, attempt).unwrap();
    store
        .compare_and_set_attempt_state(attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(attempt, AttemptState::Starting, AttemptState::Running)
        .unwrap();
    let mut child = task();
    child.owner_id = OwnerId::new(format!("worker:{attempt}")).unwrap();
    child.create_request_id = "first-child".to_owned();
    store
        .record_child_task(&child, "first-child", parent.task_id, attempt)
        .unwrap();
    let mut second = task();
    second.owner_id = child.owner_id.clone();
    second.create_request_id = "second-child".to_owned();
    assert!(matches!(
        store.validate_delegation_parent(parent.task_id, attempt, &second.owner_id),
        Err(StoreError::ConcurrentChildLimit)
    ));
    let nodes = store
        .record_cancellation_intents(parent.task_id, true)
        .unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].task_id, child.task_id);
    assert!(store.cancellation_requested(parent.task_id).unwrap());
    assert!(store.cancellation_requested(child.task_id).unwrap());
    assert!(matches!(
        store.record_child_task(&second, "second-child", parent.task_id, attempt),
        Err(StoreError::DelegationParentCancelled)
    ));
    assert!(
        store
            .latest_attempt_clock(parent.task_id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn retrying_child_still_occupies_its_parent_concurrency_slot() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let mut parent = task();
    parent.max_concurrent_children = Some(1);
    store.record_task(&parent, "retry-parent").unwrap();
    let parent_attempt = AttemptId::new();
    store
        .claim_attempt(parent.task_id, 1, parent_attempt)
        .unwrap();
    store
        .compare_and_set_attempt_state(parent_attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(
            parent_attempt,
            AttemptState::Starting,
            AttemptState::Running,
        )
        .unwrap();
    let mut child = task();
    child.owner_id = OwnerId::new(format!("worker:{parent_attempt}")).unwrap();
    child.create_request_id = "retry-child".to_owned();
    store
        .record_child_task(&child, "retry-child", parent.task_id, parent_attempt)
        .unwrap();
    let first_attempt = AttemptId::new();
    store
        .claim_attempt(child.task_id, 1, first_attempt)
        .unwrap();
    let failed = ResultEnvelope {
        outcome: TerminalOutcome::Failed,
        artifacts: vec![],
        error: Some("transient spawn failure".to_owned()),
        ..result(&child, first_attempt)
    };
    store
        .commit_terminal_result(&child.owner_id, &failed)
        .unwrap();
    store.grant_pre_spawn_retry(first_attempt).unwrap();
    assert_eq!(store.active_child_count(parent_attempt).unwrap(), 1);
    assert!(matches!(
        store.validate_delegation_parent(parent.task_id, parent_attempt, &child.owner_id),
        Err(StoreError::ConcurrentChildLimit)
    ));
    let second_attempt = AttemptId::new();
    store
        .claim_attempt(child.task_id, 1, second_attempt)
        .unwrap();
    assert_eq!(store.active_child_count(parent_attempt).unwrap(), 1);
    let final_failed = ResultEnvelope {
        attempt_id: second_attempt,
        result_id: ResultId::new(),
        ..failed
    };
    store
        .commit_terminal_result_final(&child.owner_id, &final_failed)
        .unwrap();
    assert!(store.run_completed(final_failed.result_id).unwrap());
    assert_eq!(store.active_child_count(parent_attempt).unwrap(), 0);
}

#[test]
fn ninth_delegation_edge_is_rejected_without_recording_a_task() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let mut parent = task();
    store.record_task(&parent, "root").unwrap();
    let mut parent_attempt = AttemptId::new();
    for depth in 1..=9 {
        store
            .claim_attempt(parent.task_id, 1, parent_attempt)
            .unwrap();
        store
            .compare_and_set_attempt_state(
                parent_attempt,
                AttemptState::Queued,
                AttemptState::Starting,
            )
            .unwrap();
        store
            .compare_and_set_attempt_state(
                parent_attempt,
                AttemptState::Starting,
                AttemptState::Running,
            )
            .unwrap();
        let mut child = task();
        child.create_request_id = format!("depth-{depth}");
        child.owner_id = OwnerId::new(format!("worker:{parent_attempt}")).unwrap();
        let outcome = store.record_child_task(
            &child,
            &format!("digest-{depth}"),
            parent.task_id,
            parent_attempt,
        );
        if depth == 9 {
            assert!(matches!(outcome, Err(StoreError::DelegationDepthExceeded)));
            assert!(matches!(
                store.task(child.task_id),
                Err(StoreError::TaskNotFound(_))
            ));
        } else {
            outcome.unwrap();
            parent = child;
            parent_attempt = AttemptId::new();
        }
    }
}

#[test]
fn native_route_receipt_commits_atomically_without_changing_result_digest() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "native-route").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let mut result = sealed_result(&store, &task, attempt_id);
    let legacy_digest = Store::result_digest(&result).unwrap();
    let observation = RouteObservation {
        model: Some("workbuddy/deepseek-v4.1-flash".to_owned()),
        model_source: ObservationSource::HarnessJsonl,
        effort: None,
        effort_source: ObservationSource::Unavailable,
    };
    result.route_observation = Some(observation.clone());
    assert_eq!(Store::result_digest(&result).unwrap(), legacy_digest);
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert_eq!(
        store.route_observation(result.result_id).unwrap(),
        Some(observation)
    );
    let reopened = store.latest_result(task.task_id).unwrap();
    assert!(reopened.route_observation.is_none());
    assert_eq!(Store::result_digest(&reopened).unwrap(), legacy_digest);
    assert_eq!(store.inbox(&task.owner_id, false).unwrap().len(), 1);

    result.route_observation.as_mut().unwrap().model = Some("other/fallback".to_owned());
    assert!(matches!(
        store.commit_terminal_result(&task.owner_id, &result),
        Err(StoreError::RouteObservationConflict(_))
    ));
    store
        .connection
        .execute(
            "UPDATE route_observations SET observation_json = '{}' WHERE result_id = ?1",
            [result.result_id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.route_observation(result.result_id),
        Err(StoreError::RouteObservationIntegrityMismatch)
    ));
}

#[test]
fn unreleased_embedded_route_result_retains_its_original_decision_digest() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "intermediate-result").unwrap();
    let attempt_id = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let mut result = sealed_result(&store, &task, attempt_id);
    result.legacy_embedded_route_observation = Some(RouteObservation {
        model: Some("workbuddy/deepseek-v4.1-flash".to_owned()),
        model_source: ObservationSource::HarnessJsonl,
        effort: None,
        effort_source: ObservationSource::Unavailable,
    });
    let original_digest = Store::result_digest(&result).unwrap();
    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    assert!(store.route_observation(result.result_id).unwrap().is_none());
    let reopened = store.latest_result(task.task_id).unwrap();
    assert_eq!(Store::result_digest(&reopened).unwrap(), original_digest);
    assert_eq!(
        reopened.legacy_embedded_route_observation,
        result.legacy_embedded_route_observation
    );
    store.bind_owner(&task.owner_id, "session-a", 1).unwrap();
    store
        .record_decision_and_ack(&Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id: task.owner_id.clone(),
            task_id: task.task_id,
            revision: task.revision,
            result_id: result.result_id,
            result_digest: original_digest,
            session_id: Some("session-a".to_owned()),
            binding_epoch: Some(1),
            verdict: DecisionVerdict::Accepted,
            reason: "legacy observation verified".to_owned(),
        })
        .unwrap();
    assert!(store.inbox(&task.owner_id, false).unwrap().is_empty());
}

#[test]
fn per_task_result_reads_use_the_task_revision_index() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let plan: String = store
        .connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT envelope_json FROM results
             WHERE task_id = ?1 AND revision = ?2 ORDER BY rowid DESC LIMIT 1",
            params!["task", 1],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("results_task_revision"),
        "per-task result read fell back to a table scan: {plan}"
    );
}

/// A terminal commit must not hold the store's write lock across artifact
/// file I/O.
///
/// `verify_candidate_artifacts` reads and re-hashes every sealed artifact, up
/// to the contract's 20 MiB ceiling. With that inside the transaction, every
/// other writer queued behind one commit's file reads. This measures the lock
/// window directly: a second connection with no busy timeout must be able to
/// take the write lock while the hashing happens.
#[test]
fn a_terminal_commit_hashes_artifacts_before_it_takes_the_write_lock() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = TaskSpec {
        // Large enough that hashing is measurable work, and within the 20 MiB
        // ceiling a manifest may declare.
        artifact_contract: ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 4 * 1024 * 1024,
        },
        ..task()
    };
    let attempt_id = AttemptId::new();
    store.record_task(&task, "hash-outside-lock").unwrap();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();

    let payload = vec![b'a'; 1_000_000];
    let mut result = result(&task, attempt_id);
    result.artifacts.push(
        store
            .seal_artifact_reader(
                std::io::Cursor::new(payload),
                &task.artifact_contract.media_type,
                task.artifact_contract.max_bytes,
            )
            .unwrap(),
    );

    // Held for the whole verification window, released before the insert.
    let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    blocker.busy_timeout(Duration::ZERO).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let verified = store.task_spec_for_attempt(attempt_id);
    assert!(
        verified.is_ok(),
        "reading the spec for verification needed the write lock"
    );
    assert!(
        verify_candidate_artifacts(&store.artifacts, &result, &verified.unwrap()).is_ok(),
        "verifying artifacts needed the write lock"
    );
    blocker.execute_batch("ROLLBACK").unwrap();

    assert_eq!(
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap(),
        WriteOutcome::Inserted
    );
}

/// Task admission runs inside the repository admission lock, so it must give
/// that lock back rather than wait for the store. With a retry loop here a
/// contended holder occupied the lock for 17.5s and still failed, while a
/// second admission gave up at 10.2s blaming the wrong thing. This fails if a
/// retry is put back on the admission path.
/// A contended terminal commit hashes its artifacts once, not once per try.
///
/// Verification was moved out of the write lock but left inside the retry,
/// so each contended attempt re-read and re-hashed up to 20 MiB and discarded
/// it. Both counts are asserted: without a retry this would pass vacuously.
#[test]
fn a_contended_terminal_commit_verifies_its_artifacts_once() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "contended-commit").unwrap();
    let attempt_id = AttemptId::new();
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .unwrap();
    let result = sealed_result(&store, &task, attempt_id);

    store.connection.busy_timeout(Duration::ZERO).unwrap();
    let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        blocker.execute_batch("ROLLBACK").unwrap();
    });
    COMMIT_TRIES.with(|tries| tries.set(0));
    VERIFICATIONS.with(|count| count.set(0));

    store
        .commit_terminal_result(&task.owner_id, &result)
        .unwrap();
    release.join().unwrap();

    let tries = COMMIT_TRIES.with(std::cell::Cell::get);
    assert!(
        tries > 1,
        "the commit never contended ({tries} try); the test proves nothing"
    );
    assert_eq!(
        VERIFICATIONS.with(std::cell::Cell::get),
        1,
        "over {tries} tries"
    );
}

#[test]
fn task_admission_fails_fast_instead_of_retrying_under_the_admission_lock() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    // Zero makes the wait observable: any time spent here is a retry loop,
    // not SQLite's own busy handler.
    store.connection.busy_timeout(Duration::ZERO).unwrap();
    let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = Instant::now();
    let error = store
        .record_task(&task(), "contended-admission")
        .unwrap_err();
    let elapsed = started.elapsed();
    blocker.execute_batch("ROLLBACK").unwrap();

    assert!(is_lock_contention(&error), "unexpected error: {error}");
    assert!(
        elapsed < BUSY_RETRY_BACKOFF * 8,
        "admission waited {elapsed:?} while holding the admission lock"
    );
}

/// A worker question is owed a notice until it is answered, once per
/// session: recorded notices suppress repeats, a transferred owner is told
/// again, and a reply ends it.
#[test]
fn a_question_notice_is_owed_until_answered_once_per_session() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "question-notice").unwrap();
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
    let bind = |session: &str| {
        let epoch = store.rebind_owner(&task.owner_id, session).unwrap();
        store
            .register_owner_surface(&task.owner_id, session, epoch, "w1:p1", None, "/bin/herdr")
            .unwrap();
    };
    bind("session-a");
    let question = MessageDraft {
        message_id: uuid::Uuid::new_v4().to_string(),
        task_id: task.task_id,
        attempt_id,
        direction: MessageDirection::WorkerToOwner,
        kind: MessageKind::Question,
        body: "Which token?".to_owned(),
        in_reply_to: None,
    };
    store.post_message(&question).unwrap();
    let owed = |store: &Store| store.pending_question_notices(task.task_id).unwrap();

    assert_eq!(owed(&store).len(), 1);
    assert_eq!(owed(&store)[0].session_id, "session-a");
    store
        .record_question_notice(&question.message_id, "session-a")
        .unwrap();
    assert!(owed(&store).is_empty(), "a recorded notice was owed again");

    bind("session-b");
    assert_eq!(owed(&store).len(), 1, "a transferred owner was not told");

    store
        .post_message(&MessageDraft {
            message_id: uuid::Uuid::new_v4().to_string(),
            direction: MessageDirection::OwnerToWorker,
            kind: MessageKind::Reply,
            body: "token-42".to_owned(),
            in_reply_to: Some(question.message_id.clone()),
            ..question.clone()
        })
        .unwrap();
    assert!(
        owed(&store).is_empty(),
        "an answered question was still owed"
    );
}

/// A module added without being listed in `SCANNED` would silently stop being
/// covered, which is the failure mode the write-path guards exist to prevent.
fn assert_every_module_is_scanned() {
    let declared: Vec<&str> = include_str!("lib.rs")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("mod "))
        .filter_map(|rest| rest.strip_suffix(';'))
        .collect();
    for module in &declared {
        assert!(
            SCANNED
                .iter()
                .any(|(name, _)| *name == format!("{module}.rs")),
            "module {module} is not scanned by the write-path guards; add it to SCANNED"
        );
    }
}

/// Every write path must have a recorded decision about waiting for a lock.
///
/// The rule this encodes: retry a write a running attempt depends on, do not
/// retry a write the caller can simply reissue, and never retry inside the
/// repository admission lock — #28 measured that turning one failure into two.
///
/// Retry coverage was claimed once and was wrong, so it is bound here rather
/// than described. Adding a write path without classifying it fails this test.
#[test]
fn every_write_path_has_a_recorded_retry_decision() {
    /// `true` where a contended write is retried.
    const CLASSIFIED: &[(&str, bool)] = &[
        // A running attempt depends on these: losing one leaves a paid run
        // unfinished, which recovery can only settle as `Lost`.
        ("claim_attempt_once", true),
        ("commit_terminal_result_once", true),
        ("record_decision_once", true),
        ("record_decision_and_ack_once", true),
        ("record_launch_intent_once", true),
        ("record_runner_identity_once", true),
        ("compare_and_set_attempt_state_once", true),
        ("grant_pre_spawn_retry_once", true),
        ("record_event_once", true),
        // Runs inside the repository admission lock. Waiting here blocks every
        // other admission on that repository; it must fail fast instead.
        ("record_task_with_parent", false),
        ("record_task_checkout", false),
        // Reissuable by the caller. Waiting would hold a runtime worker for a
        // command the user can simply run again.
        ("acknowledge", false),
        ("acknowledge_bound", false),
        ("bind_owner", false),
        ("rebind_owner", false),
        ("record_cancellation_intents", false),
        ("post_message", false),
        ("acknowledge_message", false),
        // Pane mode withdraws its question; losing that fails a finished run.
        ("withdraw_question_once", true),
        // Notification delivery carries its own claim and lease protocol, which
        // already re-drives a lost step. Left alone deliberately.
        ("register_owner_surface", false),
        ("claim_notification", false),
        ("mark_notification_delivered", false),
        ("release_notification_claim", false),
        // The dispatcher re-reads pending questions every poll, so a lost
        // notice record costs one repeated notice, not a lost question.
        ("record_question_notice", false),
    ];

    assert_every_module_is_scanned();

    let sources = SCANNED;
    // Assembled at runtime so this test's own text is not a match.
    let explicit = format!("self.{}()?", "write_transaction");
    // Matched anywhere in the line. An earlier version compared the whole
    // untrimmed line to `self.connection`, which no indented line can equal,
    // so the implicit half matched nothing; a first fix matched only a line
    // starting with it and still missed `let changed = self.connection...`
    // and rustfmt's `self` / `.connection` split — five of seven paths.
    let implicit = format!("self.{}", "connection");

    let mut unclassified = Vec::new();
    for (name, source) in sources {
        let body = source.split("\nmod tests").next().unwrap_or(source);
        let lines: Vec<&str> = body.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let writes = line.contains(explicit.as_str())
                || ((line.contains(implicit.as_str())
                    // `self` and `.connection` split across lines by rustfmt.
                    || (line.trim().starts_with(".connection")
                        && index > 0
                        && lines[index - 1].trim().ends_with("self")))
                    && lines[index..index.saturating_add(7).min(lines.len())]
                        .iter()
                        .any(|ahead| {
                            ahead.contains("INSERT ")
                                || ahead.contains("UPDATE ")
                                || ahead.contains("DELETE ")
                        }));
            if !writes {
                continue;
            }
            let enclosing = lines[..=index]
                .iter()
                .rev()
                .find_map(|candidate| {
                    let trimmed = candidate.strip_prefix("    ")?;
                    // Any visibility: a method a sibling module's test calls
                    // is `pub(super)`, and must not hide inside its neighbour.
                    let rest = ["pub fn ", "pub(super) fn ", "pub(crate) fn ", "fn "]
                        .iter()
                        .find_map(|prefix| trimmed.strip_prefix(prefix))?;
                    rest.split('(').next()
                })
                .unwrap_or("<unknown>");
            if !CLASSIFIED.iter().any(|(known, _)| *known == enclosing) {
                unclassified.push(format!("{name}:{} in {enclosing}", index + 1));
            }
        }
    }
    assert!(
        unclassified.is_empty(),
        "write paths with no recorded retry decision: {unclassified:?}\n\
         add each to CLASSIFIED with the reason it does or does not wait"
    );

    // And the declared decisions must match what the code does, in whichever
    // module the function now lives. This read only `lib.rs` and skipped any
    // name it could not find there, so the module split silently dropped nine
    // of twenty-one declarations from the check. A declared name that exists
    // nowhere is now a failure: it is either stale or misspelled.
    let all: String = SCANNED
        .iter()
        .map(|(_, source)| source.split("\nmod tests").next().unwrap_or(source))
        .collect::<Vec<_>>()
        .join("\n");
    // Whitespace removed, so the check is about the call and not about how
    // rustfmt happened to lay it out: a long argument list moves the call
    // into a block, which a literal match read as "not retried".
    let compact: String = all.chars().filter(|c| !c.is_whitespace()).collect();
    for (name, retried) in CLASSIFIED {
        assert!(
            all.contains(&format!("fn {name}(")),
            "{name} is classified but defined in no scanned module"
        );
        let wrapped = compact.contains(&format!("retry_busy(||self.{name}("))
            || compact.contains(&format!("retry_busy(||{{self.{name}("));
        assert_eq!(
            wrapped, *retried,
            "{name} is declared retried={retried} but the code says {wrapped}"
        );
    }
}

/// Every write transaction in this crate must begin through
/// [`Store::write_transaction`], or the gate below pins only the paths that
/// happen to use it.
///
/// This exists because a coverage claim went out wrong: the 2.4.0 notes said
/// the deterministic gate covered every write path while it covered two of
/// sixteen. A bench binds a performance claim to a measurement; nothing bound
/// that claim to anything, so it is bound here. Searching this crate's own
/// source is blunt, and it is the only thing that would have caught it.
#[test]
fn every_write_transaction_begins_through_one_helper() {
    // Assembled at runtime so this test's own source does not match a search.
    let inline_immediate = format!("Transaction::{}", "new_unchecked");
    let behaviour = format!("transaction_{}", "with_behavior");
    let deferred = format!(".connection.{}()", "transaction");

    let sources = SCANNED;

    let mut constructions = Vec::new();
    for (name, source) in sources {
        let inline = source.matches(inline_immediate.as_str()).count();
        if inline > 0 {
            constructions.push(format!("{name}: {inline}"));
        }
        assert_eq!(
            source.matches(behaviour.as_str()).count(),
            0,
            "{name} begins a transaction by behaviour instead of write_transaction"
        );
        assert_eq!(
            source.matches(deferred.as_str()).count(),
            0,
            "{name} begins a deferred transaction on the store connection"
        );
    }
    assert_eq!(
        constructions,
        vec!["lib.rs: 1".to_owned()],
        "a write transaction is built outside Store::write_transaction, so \
         a_write_transaction_takes_its_lock_at_begin no longer covers it"
    );
}

/// A write transaction that reads before it takes its lock leaves a window
/// in which another process can commit, which WAL reports as
/// `SQLITE_BUSY_SNAPSHOT` — a code `busy_timeout` does not cover. This is
/// the deterministic gate for that: it fails if `write_transaction` is ever
/// changed back to a deferred begin.
///
/// It covers every write path in the crate, because
/// `every_write_transaction_begins_through_one_helper` holds them all to this
/// one entry point.
#[test]
fn a_write_transaction_takes_its_lock_at_begin() {
    let root = TempDir::new().unwrap();
    let store = Store::open(root.path()).unwrap();
    let other = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();

    let transaction = store.write_transaction().unwrap();
    let blocked = other.execute_batch("BEGIN IMMEDIATE; CREATE TABLE probe(x); COMMIT;");
    assert!(
        blocked.is_err(),
        "another writer committed between this transaction's begin and its first write"
    );
    assert!(is_lock_contention(&StoreError::Database(
        blocked.unwrap_err()
    )));
    transaction.commit().unwrap();
}

/// A failed revision is settled only once nothing can run in it again and its
/// owner has seen the failure; each earlier state names what is still open.
#[test]
fn a_failed_revision_settles_only_after_acknowledgement_with_no_retry_left() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "settlement").unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Open(OpenReason::NoResult)
    );

    let attempt = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt)
        .unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Open(OpenReason::AttemptActive)
    );

    let failed = ResultEnvelope {
        outcome: TerminalOutcome::Failed,
        artifacts: vec![],
        error: Some("fixture failure".to_owned()),
        ..result(&task, attempt)
    };
    store
        .commit_terminal_result(&task.owner_id, &failed)
        .unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Open(OpenReason::Unacknowledged(TerminalOutcome::Failed))
    );

    store.grant_pre_spawn_retry(attempt).unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Open(OpenReason::RetryGranted)
    );
    store.acknowledge(&task.owner_id, failed.result_id).unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Open(OpenReason::RetryGranted),
        "an acknowledgement does not withdraw a granted retry"
    );
}

#[test]
fn an_acknowledged_failure_without_a_retry_is_settled() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "settled-failure").unwrap();
    let attempt = AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt)
        .unwrap();
    let lost = ResultEnvelope {
        outcome: TerminalOutcome::Lost,
        artifacts: vec![],
        error: Some("fixture loss".to_owned()),
        ..result(&task, attempt)
    };
    store.commit_terminal_result(&task.owner_id, &lost).unwrap();
    store.acknowledge(&task.owner_id, lost.result_id).unwrap();
    assert_eq!(
        store
            .revision_settlement(task.task_id, task.revision)
            .unwrap(),
        Settlement::Acknowledged(TerminalOutcome::Lost)
    );
}

/// A worker may withdraw its own question once what it asked about resolved:
/// the question stops holding the result back and stops being pushed. Nobody
/// else's message can be withdrawn this way.
#[test]
fn a_withdrawn_question_no_longer_holds_the_result_or_gets_pushed() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let task = task();
    store.record_task(&task, "withdraw").unwrap();
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
    let epoch = store.rebind_owner(&task.owner_id, "session-a").unwrap();
    store
        .register_owner_surface(
            &task.owner_id,
            "session-a",
            epoch,
            "w1:p1",
            None,
            "/bin/herdr",
        )
        .unwrap();
    let draft = |direction, kind| MessageDraft {
        message_id: uuid::Uuid::new_v4().to_string(),
        task_id: task.task_id,
        attempt_id,
        direction,
        kind,
        body: "Look at pane w1:p2".to_owned(),
        in_reply_to: None,
    };
    let question = draft(MessageDirection::WorkerToOwner, MessageKind::Question);
    store.post_message(&question).unwrap();
    assert_eq!(
        store.unsettled_questions(task.task_id, attempt_id).unwrap(),
        1
    );
    assert_eq!(
        store.pending_question_notices(task.task_id).unwrap().len(),
        1
    );

    // Only this attempt's own question.
    assert!(
        store
            .withdraw_question(task.task_id, AttemptId::new(), &question.message_id)
            .is_err()
    );
    let note = draft(MessageDirection::WorkerToOwner, MessageKind::Note);
    store.post_message(&note).unwrap();
    assert!(
        store
            .withdraw_question(task.task_id, attempt_id, &note.message_id)
            .is_err(),
        "only a question can be withdrawn"
    );

    store
        .withdraw_question(task.task_id, attempt_id, &question.message_id)
        .unwrap();
    // Idempotent.
    store
        .withdraw_question(task.task_id, attempt_id, &question.message_id)
        .unwrap();
    assert_eq!(
        store.unsettled_questions(task.task_id, attempt_id).unwrap(),
        0
    );
    assert!(
        store
            .pending_question_notices(task.task_id)
            .unwrap()
            .is_empty()
    );
}
