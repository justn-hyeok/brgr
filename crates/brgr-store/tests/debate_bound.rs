//! Debate writes made by an owner, seen only through the public store API.
//!
//! A takeover moves the owner's head to a new session. The guarded debate
//! writes must accept the new session's proof and refuse the old one, and a
//! refused call must leave nothing behind.

use brgr_protocol::{AttemptId, AttemptState, OwnerId, SCHEMA_V1, TaskId, TaskSpec};
use brgr_store::{MessageKind, OwnerProof, PeerDraft, Store, StoreError};
use serde_json::json;
use tempfile::TempDir;

const FROM: &str = "codex:session-head";
const TO: &str = "codex:session-incoming";

fn owner() -> OwnerId {
    OwnerId::new(FROM).unwrap()
}

fn other_owner() -> OwnerId {
    OwnerId::new("codex:someone-else").unwrap()
}

fn spec(owner_id: &OwnerId) -> TaskSpec {
    let task_id = TaskId::new();
    serde_json::from_value(json!({
        "schema": SCHEMA_V1,
        "task_id": task_id,
        "revision": 1,
        "create_request_id": format!("debate-bound-{task_id}"),
        "owner_id": owner_id.as_str(),
        "objective": "Compare approaches",
        "workspace": "/tmp/debate-bound-test",
        "route": {"harness_id": "local.fixture"},
        "required_capabilities": [],
        "artifact_contract": {"media_type": "text/plain", "max_bytes": 4096},
        "acceptance_criteria": ["answer the peer"],
        "budget": {"deadline_seconds": 60, "max_attempts": 1}
    }))
    .unwrap()
}

/// A task whose attempt is running, so it can take part in a debate.
fn running_task(store: &mut Store, owner_id: &OwnerId) -> (TaskId, AttemptId) {
    let task = spec(owner_id);
    store
        .record_task(&task, &format!("digest-{}", task.task_id))
        .unwrap();
    let attempt = AttemptId::new();
    store.claim_attempt(task.task_id, 1, attempt).unwrap();
    store
        .set_attempt_state(attempt, AttemptState::Starting)
        .unwrap();
    store
        .set_attempt_state(attempt, AttemptState::Running)
        .unwrap();
    (task.task_id, attempt)
}

fn peer_draft(group: &str, from: TaskId, to: TaskId, body: &str) -> PeerDraft {
    PeerDraft {
        id: uuid::Uuid::new_v4().to_string(),
        group: group.to_owned(),
        from,
        to,
        kind: MessageKind::Question,
        body: body.to_owned(),
        reply_to: None,
    }
}

/// Binds the owner to the outgoing session and moves the head to the incoming
/// one, so `FROM` at epoch 1 is stale and `TO` at epoch 2 is current.
fn take_over(store: &Store, owner_id: &OwnerId) {
    store.bind_owner(owner_id, FROM, 1).unwrap();
    let takeover = store
        .request_takeover(owner_id, FROM, TO, "handoff-debate-bound")
        .unwrap();
    store
        .prepare_takeover(
            takeover.takeover_id,
            &brgr_store::HandoffManifest {
                goal: "Finish the debate".to_owned(),
                origin_session: FROM.to_owned(),
                ..brgr_store::HandoffManifest::default()
            },
        )
        .unwrap();
    store.commit_takeover(takeover.takeover_id).unwrap();
}

#[test]
fn incoming_session_creates_posts_and_stops_through_bound_functions() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let (a, attempt_a) = running_task(&mut store, &owner_id);
    let (b, attempt_b) = running_task(&mut store, &owner_id);
    take_over(&store, &owner_id);
    let current = OwnerProof::new(owner_id.clone(), TO, 2).unwrap();
    let group = uuid::Uuid::new_v4().to_string();

    let created = store
        .create_debate_bound(&group, &[a, b], &current)
        .unwrap();
    assert_eq!(created.owner, owner_id.as_str());
    assert!(created.active);
    assert_eq!(created.members.len(), 2);

    let question = peer_draft(&group, a, b, "What is your approach?");
    let posted = store
        .post_peer_message_from_bound(&question, attempt_a, &current)
        .unwrap();
    assert_eq!(posted.id, question.id);
    assert_eq!(store.peer_inbox(b, attempt_b).unwrap().len(), 1);

    store.stop_debate_bound(&group, &current).unwrap();
    assert!(!store.debate(&group).unwrap().active);
}

#[test]
fn old_session_proof_is_refused_for_all_three_writes_and_nothing_is_written() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let (a, attempt_a) = running_task(&mut store, &owner_id);
    let (b, _attempt_b) = running_task(&mut store, &owner_id);
    let group = uuid::Uuid::new_v4().to_string();
    store.bind_owner(&owner_id, FROM, 1).unwrap();
    store.create_debate(&group, &[a, b]).unwrap();
    let old = OwnerProof::new(owner_id.clone(), FROM, 1).unwrap();
    take_over(&store, &owner_id);

    let fresh_group = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        store.create_debate_bound(&fresh_group, &[a, b], &old),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(matches!(
        store.debate(&fresh_group),
        Err(StoreError::TaskMessageNotFound(_))
    ));

    assert!(matches!(
        store.stop_debate_bound(&group, &old),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(store.debate(&group).unwrap().active);

    let question = peer_draft(&group, a, b, "Still the old head?");
    assert!(matches!(
        store.post_peer_message_from_bound(&question, attempt_a, &old),
        Err(StoreError::OwnerBindingConflict)
    ));
    assert!(store.debate_history(&group).unwrap().is_empty());
}

#[test]
fn proof_for_a_different_owner_is_refused_for_all_three_writes() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let (a, attempt_a) = running_task(&mut store, &owner_id);
    let (b, _attempt_b) = running_task(&mut store, &owner_id);
    let group = uuid::Uuid::new_v4().to_string();
    store.bind_owner(&owner_id, FROM, 1).unwrap();
    store.create_debate(&group, &[a, b]).unwrap();

    // The other owner is bound to a live session, so the proof itself is
    // valid; only the ownership of the group and tasks is wrong.
    let other = other_owner();
    store.bind_owner(&other, TO, 1).unwrap();
    let wrong = OwnerProof::new(other, TO, 1).unwrap();

    let fresh_group = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        store.create_debate_bound(&fresh_group, &[a, b], &wrong),
        Err(StoreError::OwnerProofMismatch)
    ));
    assert!(matches!(
        store.debate(&fresh_group),
        Err(StoreError::TaskMessageNotFound(_))
    ));

    assert!(matches!(
        store.stop_debate_bound(&group, &wrong),
        Err(StoreError::OwnerProofMismatch)
    ));
    assert!(store.debate(&group).unwrap().active);

    let question = peer_draft(&group, a, b, "Who am I speaking for?");
    assert!(matches!(
        store.post_peer_message_from_bound(&question, attempt_a, &wrong),
        Err(StoreError::OwnerProofMismatch)
    ));
    assert!(store.debate_history(&group).unwrap().is_empty());
}

#[test]
fn unguarded_debate_writes_keep_working_after_a_takeover() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let (a, attempt_a) = running_task(&mut store, &owner_id);
    let (b, attempt_b) = running_task(&mut store, &owner_id);
    take_over(&store, &owner_id);

    // A worker-side caller is identified by its attempt, not by the owner's
    // session, so the takeover must not stop it.
    let group = uuid::Uuid::new_v4().to_string();
    store.create_debate(&group, &[a, b]).unwrap();
    let question = peer_draft(&group, a, b, "Still talking after the handoff?");
    store.post_peer_message_from(&question, attempt_a).unwrap();
    assert_eq!(store.peer_inbox(b, attempt_b).unwrap().len(), 1);
    store.stop_debate(&group).unwrap();
    assert!(!store.debate(&group).unwrap().active);
}

#[test]
fn a_debate_with_one_foreign_task_is_refused_as_a_mismatch() {
    let root = TempDir::new().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let owner_id = owner();
    let (a, _attempt_a) = running_task(&mut store, &owner_id);
    let (foreign, _attempt_foreign) = running_task(&mut store, &other_owner());
    store.bind_owner(&owner_id, FROM, 1).unwrap();
    let proof = OwnerProof::new(owner_id.clone(), FROM, 1).unwrap();

    // The first task is the proof owner's; the second is somebody else's. The
    // refusal names the ownership, not the shape of the list.
    let group = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        store.create_debate_bound(&group, &[a, foreign], &proof),
        Err(StoreError::OwnerProofMismatch)
    ));
    assert!(matches!(
        store.debate(&group),
        Err(StoreError::TaskMessageNotFound(_))
    ));
}
