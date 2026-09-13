//! Owner-scoped cleanup receipts. Automatic close stays disabled until Herdr
//! supports an atomic identity-checked close operation.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use brgr_protocol::{ResultEnvelope, ResultId, TaskId};
use brgr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::write_json_atomic;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaneReceipt {
    task_id: TaskId,
    agent: String,
    pane_id: String,
    terminal_id: String,
    session_value: String,
    launcher_receipt: PathBuf,
    keep_pane: bool,
    state: CleanupState,
    result_id: Option<ResultId>,
    result_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CleanupState {
    Retained,
    CleanupPending,
}

pub fn record_spawn(
    runs: &Path,
    task_id: TaskId,
    agent: &str,
    launcher_receipt: &Path,
    keep_pane: bool,
) -> Result<()> {
    let launcher: Value = serde_json::from_slice(&fs::read(launcher_receipt)?)?;
    if launcher.get("agent").and_then(Value::as_str) != Some(agent)
        || launcher.get("codex_prompt_marked").and_then(Value::as_bool) != Some(false)
    {
        bail!("OMP launcher receipt does not identify a brgr-owned child");
    }
    let live = live_agent(agent)?;
    if live.get("name").and_then(Value::as_str) != Some(agent) {
        bail!("Herdr agent identity differs from brgr launcher receipt");
    }
    let receipt = PaneReceipt {
        task_id,
        agent: agent.to_owned(),
        pane_id: required_str(&live, "pane_id")?.to_owned(),
        terminal_id: required_str(&live, "terminal_id")?.to_owned(),
        session_value: live
            .pointer("/agent_session/value")
            .and_then(Value::as_str)
            .context("Herdr has not reported an immutable OMP session")?
            .to_owned(),
        launcher_receipt: launcher_receipt.to_path_buf(),
        keep_pane,
        state: CleanupState::Retained,
        result_id: None,
        result_digest: None,
    };
    write_json_atomic(&receipt_path(runs, task_id), &receipt)
}

pub fn mark_pending(runs: &Path, task_id: TaskId, result: &ResultEnvelope) -> Result<()> {
    let path = receipt_path(runs, task_id);
    if !path.exists() {
        return Ok(());
    }
    let mut receipt: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    if receipt.task_id != task_id || result.task_id != task_id {
        bail!("cleanup receipt belongs to another task");
    }
    if receipt.keep_pane {
        return Ok(());
    }
    receipt.state = CleanupState::CleanupPending;
    receipt.result_id = Some(result.result_id);
    receipt.result_digest = Some(Store::result_digest(result)?);
    write_json_atomic(&path, &receipt)
}

pub fn status(runs: &Path, task_id: TaskId) -> Result<String> {
    let path = receipt_path(runs, task_id);
    if !path.exists() {
        return Ok("no_owned_pane".to_owned());
    }
    let receipt: PaneReceipt = serde_json::from_slice(&fs::read(path)?)?;
    if receipt.task_id != task_id || receipt.keep_pane || receipt.state == CleanupState::Retained {
        return Ok("retained".to_owned());
    }
    let live = live_agent(&receipt.agent)?;
    let matching = live.get("pane_id").and_then(Value::as_str) == Some(&receipt.pane_id)
        && live.get("terminal_id").and_then(Value::as_str) == Some(&receipt.terminal_id)
        && live.pointer("/agent_session/value").and_then(Value::as_str)
            == Some(&receipt.session_value);
    if !matching {
        return Ok("retained_identity_changed".to_owned());
    }
    let state = live
        .get("agent_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(state, "idle" | "done") {
        return Ok(format!("retained_agent_{state}"));
    }
    Ok("eligible_conditional_close_unavailable".to_owned())
}

fn receipt_path(runs: &Path, task_id: TaskId) -> PathBuf {
    runs.join(format!("{task_id}.pane.json"))
}

fn live_agent(agent: &str) -> Result<Value> {
    let output = Command::new("herdr")
        .args(["agent", "get", agent])
        .output()?;
    if !output.status.success() {
        bail!("Herdr cannot identify the owned agent {agent}");
    }
    let response: Value = serde_json::from_slice(&output.stdout)?;
    response
        .pointer("/result/agent")
        .cloned()
        .context("Herdr agent response is missing")
}

fn required_str<'a>(object: &'a Value, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("Herdr agent response is missing {field}"))
}

#[cfg(test)]
mod tests {
    use brgr_protocol::{ResultEnvelope, ResultId, SCHEMA_V1, TerminalOutcome};

    use super::*;

    #[test]
    fn accepted_result_queues_only_matching_owned_pane() {
        let temp = tempfile::tempdir().unwrap();
        let task_id = TaskId::new();
        let receipt = PaneReceipt {
            task_id,
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            terminal_id: "term-owned".to_owned(),
            session_value: "session-owned".to_owned(),
            launcher_receipt: temp.path().join("launcher.json"),
            keep_pane: false,
            state: CleanupState::Retained,
            result_id: None,
            result_digest: None,
        };
        write_json_atomic(&receipt_path(temp.path(), task_id), &receipt).unwrap();
        let result = ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision: 1,
            attempt_id: brgr_protocol::AttemptId::new(),
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![],
            error: None,
            unresolved_effects: vec![],
        };
        mark_pending(temp.path(), task_id, &result).unwrap();
        let saved: PaneReceipt =
            serde_json::from_slice(&fs::read(receipt_path(temp.path(), task_id)).unwrap()).unwrap();
        assert_eq!(saved.state, CleanupState::CleanupPending);
        assert_eq!(saved.result_id, Some(result.result_id));
        assert_eq!(
            saved.result_digest,
            Some(Store::result_digest(&result).unwrap())
        );
    }

    #[test]
    fn keep_pane_never_enters_cleanup_queue() {
        let temp = tempfile::tempdir().unwrap();
        let task_id = TaskId::new();
        let receipt = PaneReceipt {
            task_id,
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            terminal_id: "term-owned".to_owned(),
            session_value: "session-owned".to_owned(),
            launcher_receipt: temp.path().join("launcher.json"),
            keep_pane: true,
            state: CleanupState::Retained,
            result_id: None,
            result_digest: None,
        };
        write_json_atomic(&receipt_path(temp.path(), task_id), &receipt).unwrap();
        let result = ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision: 1,
            attempt_id: brgr_protocol::AttemptId::new(),
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![],
            error: None,
            unresolved_effects: vec![],
        };
        mark_pending(temp.path(), task_id, &result).unwrap();
        assert_eq!(status(temp.path(), task_id).unwrap(), "retained");
    }
}
