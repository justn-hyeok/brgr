use brgr_protocol::{AttemptId, AttemptState, TaskId};
use brgr_store::{MessageKind, PeerDraft, Store};
use serde_json::json;

fn task(id: TaskId) -> brgr_protocol::TaskSpec {
    serde_json::from_value(json!({"schema":"brgr/v1","task_id":id,"revision":1,"create_request_id":format!("debate-{id}"),"owner_id":"codex:debate-test","objective":"Compare approaches","workspace":"/tmp/debate-test","route":{"harness_id":"local.fixture"},"required_capabilities":[],"artifact_contract":{"media_type":"text/plain","max_bytes":4096},"acceptance_criteria":["answer the peer"],"budget":{"deadline_seconds":60,"max_attempts":1}})).unwrap()
}

#[test]
fn peers_only_talk_in_an_explicit_active_group_and_keep_history() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    let a = TaskId::new();
    let b = TaskId::new();
    let c = TaskId::new();
    let mut attempts = Vec::new();
    for id in [a, b, c] {
        store
            .record_task(&task(id), &format!("digest-{id}"))
            .unwrap();
        let attempt = AttemptId::new();
        store.claim_attempt(id, 1, attempt).unwrap();
        store
            .compare_and_set_attempt_state(attempt, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        store
            .compare_and_set_attempt_state(attempt, AttemptState::Starting, AttemptState::Running)
            .unwrap();
        attempts.push(attempt);
    }
    let group = uuid::Uuid::new_v4().to_string();
    let mut draft = PeerDraft {
        id: uuid::Uuid::new_v4().to_string(),
        group: group.clone(),
        from: a,
        to: b,
        kind: MessageKind::Question,
        body: "What is your approach?".into(),
        reply_to: None,
    };
    assert!(store.post_peer_message(&draft).is_err());
    store.create_debate(&group, &[a, b]).unwrap();
    let question = store.post_peer_message(&draft).unwrap();
    assert_eq!(store.peer_inbox(b, attempts[1]).unwrap().len(), 1);
    assert_eq!(store.post_peer_message(&draft).unwrap().id, question.id);
    draft.kind = MessageKind::Note;
    assert!(
        store.post_peer_message(&draft).is_err(),
        "changed kind was accepted as a replay"
    );
    draft.kind = MessageKind::Question;
    assert!(
        store
            .post_peer_message_from(&draft, AttemptId::new())
            .is_err()
    );
    assert!(
        store
            .acknowledge_peer(&question.id, c, attempts[2])
            .is_err()
    );
    store
        .acknowledge_peer(&question.id, b, attempts[1])
        .unwrap();
    assert!(store.peer_inbox(b, attempts[1]).unwrap().is_empty());
    draft.to = c;
    draft.id = uuid::Uuid::new_v4().to_string();
    assert!(store.post_peer_message(&draft).is_err());
    let reply = PeerDraft {
        id: uuid::Uuid::new_v4().to_string(),
        group: group.clone(),
        from: b,
        to: a,
        kind: MessageKind::Reply,
        body: "Use the native TUI.".into(),
        reply_to: Some(question.id),
    };
    store.post_peer_message(&reply).unwrap();
    assert_eq!(
        store.peer_inbox(a, attempts[0]).unwrap()[0].body,
        "Use the native TUI."
    );
    store.stop_debate(&group).unwrap();
    assert!(!store.debate(&group).unwrap().active);
    assert!(store.post_peer_message(&reply).is_err());
}

#[test]
fn interrupted_native_delivery_is_not_automatically_sent_twice() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let attempt = AttemptId::new();
    let id = uuid::Uuid::new_v4().to_string();
    assert!(store.claim_native_delivery(&id, attempt, "w1:p2").unwrap());
    assert!(!store.claim_native_delivery(&id, attempt, "w1:p2").unwrap());
    store
        .finish_native_delivery(&id, Some("transport outcome uncertain"))
        .unwrap();
    drop(store);
    let store = Store::open(temp.path()).unwrap();
    assert!(!store.claim_native_delivery(&id, attempt, "w1:p2").unwrap());
}

#[test]
fn confirmed_unattempted_delivery_can_be_retried() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let attempt = AttemptId::new();
    let id = uuid::Uuid::new_v4().to_string();
    assert!(store.claim_native_delivery(&id, attempt, "w1:p2").unwrap());
    store.release_native_delivery(&id).unwrap();
    assert!(store.claim_native_delivery(&id, attempt, "w1:p2").unwrap());
}

#[test]
fn reading_a_question_does_not_answer_it_and_replying_does() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    let spec = task(TaskId::new());
    let attempt = AttemptId::new();
    store.record_task(&spec, "question").unwrap();
    store.claim_attempt(spec.task_id, 1, attempt).unwrap();
    store
        .compare_and_set_attempt_state(attempt, AttemptState::Queued, AttemptState::Starting)
        .unwrap();
    store
        .compare_and_set_attempt_state(attempt, AttemptState::Starting, AttemptState::Running)
        .unwrap();
    let question = brgr_store::MessageDraft::new(
        spec.task_id,
        attempt,
        brgr_store::MessageDirection::WorkerToOwner,
        MessageKind::Question,
        "Which token?".into(),
        None,
    );
    store.post_message(&question).unwrap();
    store
        .acknowledge_message(
            spec.task_id,
            attempt,
            brgr_store::MessageDirection::WorkerToOwner,
            &question.message_id,
        )
        .unwrap();
    assert_eq!(
        store
            .unanswered_questions(
                spec.task_id,
                attempt,
                brgr_store::MessageDirection::WorkerToOwner
            )
            .unwrap()
            .len(),
        1
    );
    store
        .post_message(&brgr_store::MessageDraft::new(
            spec.task_id,
            attempt,
            brgr_store::MessageDirection::OwnerToWorker,
            MessageKind::Reply,
            "token".into(),
            Some(question.message_id),
        ))
        .unwrap();
    assert!(
        store
            .unanswered_questions(
                spec.task_id,
                attempt,
                brgr_store::MessageDirection::WorkerToOwner
            )
            .unwrap()
            .is_empty()
    );
}
