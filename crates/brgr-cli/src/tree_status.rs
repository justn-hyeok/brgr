use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use brgr_protocol::{AttemptState, TaskId, TerminalOutcome};
use brgr_store::{MessageDirection, Store, StoreError};
use serde_json::json;

use crate::print_value;

pub fn show(store: &Store, root: TaskId, json_output: bool) -> Result<()> {
    let mut nodes = store.subtree(root)?;
    nodes.reverse();
    let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let mut rows = Vec::with_capacity(nodes.len());
    for node in nodes {
        let spec = store.task(node.task_id)?;
        let state = match store.attempt_state(node.task_id) {
            Ok(state) => state,
            Err(StoreError::TaskNotFound(_)) => AttemptState::Queued,
            Err(error) => return Err(error.into()),
        };
        let attempt = match store.latest_message_attempt(node.task_id) {
            Ok(attempt) => Some(attempt),
            Err(StoreError::TaskMessageNotFound(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let questions = attempt.map_or(Ok(0), |id| store.unsettled_questions(node.task_id, id))?;
        let children = attempt.map_or(Ok(0), |id| store.unsettled_children(id))?;
        let owner_messages = attempt.map_or(Ok(0), |id| {
            store
                .task_messages(node.task_id, id, MessageDirection::WorkerToOwner, false)
                .map(|messages| messages.len())
        })?;
        let worker_messages = attempt.map_or(Ok(0), |id| {
            store
                .task_messages(node.task_id, id, MessageDirection::OwnerToWorker, false)
                .map(|messages| messages.len())
        })?;
        let result = match store.latest_result(node.task_id) {
            Ok(result) => Some(result),
            Err(StoreError::TaskNotFound(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let decision = result
            .as_ref()
            .map(|result| store.decision_for_result(result.result_id))
            .transpose()?
            .flatten();
        let waiting = if questions > 0 {
            Some("question")
        } else if children > 0 {
            Some("child_result")
        } else if result
            .as_ref()
            .is_some_and(|result| result.outcome == TerminalOutcome::Candidate)
            && decision.is_none()
        {
            Some("approval")
        } else {
            None
        };
        let remaining = if state == AttemptState::Terminal {
            None
        } else {
            store.latest_attempt_clock(node.task_id)?.map(|started| {
                i64::try_from(spec.budget.deadline_seconds)
                    .unwrap_or(i64::MAX)
                    .saturating_sub(now.saturating_sub(started))
                    .max(0)
            })
        };
        rows.push(json!({
            "task_id": node.task_id,
            "parent_task_id": node.parent_task_id,
            "depth": node.depth,
            "harness": spec.route.harness_id,
            "state": format!("{state:?}").to_lowercase(),
            "outcome": result.as_ref().map(|value| value.outcome),
            "decision": decision.as_ref().map(|value| value.verdict),
            "waiting_for": waiting,
            "remaining_seconds": remaining,
            "unanswered_questions": questions,
            "unsettled_children": children,
            "unread_owner_messages": owner_messages,
            "unread_worker_messages": worker_messages,
            "completion_notifications_queued": store.pending_notifications_for_task(node.task_id)?.len(),
            "cancellation_requested": store.cancellation_requested(node.task_id)?,
        }));
    }
    print_value(&json!({"root_task_id": root, "nodes": rows}), json_output);
    Ok(())
}
