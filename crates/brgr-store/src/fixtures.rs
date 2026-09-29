//! Shared test fixtures.
//!
//! The store's tests live beside the code they cover, which means several modules
//! build the same task and result. These are here so they are written once.

use brgr_protocol::{
    ArtifactContract, AttemptBudget, AttemptId, OwnerId, ResultEnvelope, ResultId, Route,
    SCHEMA_V1, TaskId, TaskSpec, TerminalOutcome,
};
use rusqlite::Connection;

use crate::Store;

pub(crate) fn task() -> TaskSpec {
    TaskSpec {
        schema: SCHEMA_V1.to_owned(),
        task_id: TaskId::new(),
        revision: 1,
        create_request_id: "request-1".to_owned(),
        owner_id: OwnerId::new("codex:test-owner").unwrap(),
        objective: "Store one bounded result".to_owned(),
        workspace: "/tmp/brgr-test".to_owned(),
        route: Route {
            harness_id: "local.fixture".to_owned(),
            requested_model: None,
            requested_effort: None,
        },
        required_capabilities: vec!["completion".to_owned()],
        artifact_contract: ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 1_024,
        },
        acceptance_criteria: vec!["result is sealed".to_owned()],
        budget: AttemptBudget {
            deadline_seconds: 30,
            max_attempts: 2,
        },
        instructions: brgr_protocol::TaskInstructions::default(),
        evidence: brgr_protocol::EvidenceSpec::default(),
        max_concurrent_children: None,
    }
}
pub(crate) fn result(task: &TaskSpec, attempt_id: AttemptId) -> ResultEnvelope {
    ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Candidate,
        artifacts: vec![],
        error: None,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
    }
}
pub(crate) fn sealed_result(
    store: &Store,
    task: &TaskSpec,
    attempt_id: AttemptId,
) -> ResultEnvelope {
    let mut envelope = result(task, attempt_id);
    envelope.artifacts.push(
        store
            .seal_artifact_reader(
                std::io::Cursor::new(b"reviewable report"),
                &task.artifact_contract.media_type,
                task.artifact_contract.max_bytes,
            )
            .unwrap(),
    );
    envelope
}
pub(crate) fn index_names(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name IS NOT NULL")
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}
