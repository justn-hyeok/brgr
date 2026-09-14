//! Owner-scoped best-effort cleanup of brgr-created Herdr panes.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use brgr_protocol::{AttemptId, OwnerId, ResultEnvelope, ResultId, TaskId, TerminalOutcome};
use brgr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::write_json_atomic;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PaneReceipt {
    task_id: TaskId,
    attempt_id: AttemptId,
    owner_id: OwnerId,
    agent: String,
    pane_id: String,
    parent_pane_id: String,
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
    Eligible,
    Closing,
    Closed,
}

pub fn record_spawn(
    store: &Store,
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
    let active = store
        .unfinished_attempts()?
        .into_iter()
        .filter(|attempt| attempt.task.task_id == task_id)
        .collect::<Vec<_>>();
    let [attempt] = active.as_slice() else {
        bail!("brgr task must have exactly one active attempt before recording its pane");
    };
    let receipt = PaneReceipt {
        task_id,
        attempt_id: attempt.attempt_id,
        owner_id: attempt.task.owner_id.clone(),
        agent: agent.to_owned(),
        pane_id: required_str(&live, "pane_id")?.to_owned(),
        parent_pane_id: required_str(&launcher, "parent_pane")?.to_owned(),
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
    if receipt.pane_id == receipt.parent_pane_id {
        bail!("brgr child pane cannot be the parent pane");
    }
    if env::var("HERDR_PANE_ID").as_deref() != Ok(receipt.parent_pane_id.as_str()) {
        bail!("OMP launcher parent pane differs from the current Herdr caller");
    }
    write_json_atomic(&receipt_path(runs, task_id), &receipt)
}

pub fn mark_pending(runs: &Path, task_id: TaskId, result: &ResultEnvelope) -> Result<()> {
    let path = receipt_path(runs, task_id);
    if !path.exists() {
        return Ok(());
    }
    let mut receipt: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    if receipt.task_id != task_id
        || result.task_id != task_id
        || receipt.attempt_id != result.attempt_id
    {
        bail!("cleanup receipt belongs to another task");
    }
    if receipt.keep_pane || receipt.state == CleanupState::Closed {
        return Ok(());
    }
    receipt.state = CleanupState::CleanupPending;
    receipt.result_id = Some(result.result_id);
    receipt.result_digest = Some(Store::result_digest(result)?);
    write_json_atomic(&path, &receipt)
}

pub fn status(store: &Store, runs: &Path, task_id: TaskId) -> Result<String> {
    let path = receipt_path(runs, task_id);
    if !path.exists() {
        return Ok("no_owned_pane".to_owned());
    }
    let receipt: PaneReceipt = serde_json::from_slice(&fs::read(path)?)?;
    if receipt.task_id != task_id || receipt.keep_pane || receipt.state == CleanupState::Retained {
        return Ok("retained".to_owned());
    }
    if receipt.state == CleanupState::Closed {
        return Ok("closed".to_owned());
    }
    if !decision_is_complete(store, &receipt)? {
        return Ok("cleanup_pending_decision_or_ack".to_owned());
    }
    let live = live_agent(&receipt.agent)?;
    if !live_matches(&receipt, &live) {
        return Ok("retained_identity_changed".to_owned());
    }
    let pane = live_pane(&receipt.pane_id)?;
    if pane.get("pane_id").and_then(Value::as_str) != Some(receipt.pane_id.as_str())
        || pane.get("terminal_id").and_then(Value::as_str) != Some(receipt.terminal_id.as_str())
        || pane.get("agent").and_then(Value::as_str) != Some("omp")
    {
        return Ok("retained_pane_changed".to_owned());
    }
    let tab_id = required_str(&pane, "tab_id")?;
    if live_tab(tab_id)?.get("label").and_then(Value::as_str) == Some("lobby") {
        return Ok("retained_protected_lobby".to_owned());
    }
    let state = live
        .get("agent_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if !matches!(state, "idle" | "done") {
        return Ok(format!("retained_agent_{state}"));
    }
    Ok("eligible".to_owned())
}

/// Closes only a brgr-owned, accepted-and-acknowledged idle child pane.
/// Herdr 0.9.0 has no conditional close, so the final live check and close
/// remain separate operations. Identity changes detected by either read fail
/// closed; an undetectable change in that interval cannot be ruled out.
pub fn close_if_eligible(store: &Store, runs: &Path, task_id: TaskId) -> Result<String> {
    let path = receipt_path(runs, task_id);
    if !path.exists() {
        return Ok("no_owned_pane".to_owned());
    }
    let mut receipt: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    if receipt.keep_pane || receipt.state == CleanupState::Closed {
        return Ok(if receipt.keep_pane {
            "retained"
        } else {
            "closed"
        }
        .to_owned());
    }
    if receipt.state == CleanupState::Closing && pane_absent(&receipt.pane_id)? {
        receipt.state = CleanupState::Closed;
        write_json_atomic(&path, &receipt)?;
        return Ok("closed".to_owned());
    }
    if env::var("HERDR_ENV").as_deref() != Ok("1")
        || env::var("HERDR_PANE_ID").as_deref() != Ok(receipt.parent_pane_id.as_str())
    {
        return Ok("cleanup_pending_parent_unavailable".to_owned());
    }
    let eligibility = status(store, runs, task_id)?;
    if eligibility != "eligible" {
        return Ok(eligibility);
    }
    receipt.state = CleanupState::Eligible;
    write_json_atomic(&path, &receipt)?;
    receipt.state = CleanupState::Closing;
    write_json_atomic(&path, &receipt)?;
    if status(store, runs, task_id)? != "eligible" {
        receipt.state = CleanupState::CleanupPending;
        write_json_atomic(&path, &receipt)?;
        return Ok("retained_live_state_changed".to_owned());
    }
    let closed = Command::new("herdr")
        .args(["pane", "close", &receipt.pane_id])
        .output()?;
    for _ in 0..10 {
        if pane_absent(&receipt.pane_id)? {
            receipt.state = CleanupState::Closed;
            write_json_atomic(&path, &receipt)?;
            return Ok("closed".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
    receipt.state = CleanupState::CleanupPending;
    write_json_atomic(&path, &receipt)?;
    if !closed.status.success() {
        bail!(
            "Herdr did not close the brgr-owned pane {}",
            receipt.pane_id
        );
    }
    Ok("cleanup_pending_close_unconfirmed".to_owned())
}

fn pane_absent(pane_id: &str) -> Result<bool> {
    let output = Command::new("herdr")
        .args(["pane", "get", pane_id])
        .output()?;
    if output.status.success() {
        return Ok(false);
    }
    let response: Value = serde_json::from_slice(&output.stderr)?;
    if response.pointer("/error/code").and_then(Value::as_str) == Some("pane_not_found") {
        return Ok(true);
    }
    bail!("Herdr pane lookup failed without a not-found proof")
}

fn decision_is_complete(store: &Store, receipt: &PaneReceipt) -> Result<bool> {
    let task = store.task(receipt.task_id)?;
    if task.owner_id != receipt.owner_id {
        return Ok(false);
    }
    let result = store.latest_result(receipt.task_id)?;
    if result.attempt_id != receipt.attempt_id
        || receipt.result_id != Some(result.result_id)
        || receipt.result_digest.as_deref() != Some(Store::result_digest(&result)?.as_str())
    {
        return Ok(false);
    }
    let acknowledged = store
        .inbox(&receipt.owner_id, true)?
        .iter()
        .any(|item| item.result.result_id == result.result_id && item.acknowledged);
    if !acknowledged {
        return Ok(false);
    }
    if result.outcome == TerminalOutcome::Candidate {
        let decision = store.decision_for_result(result.result_id)?;
        return Ok(decision.is_some_and(|value| {
            value.owner_id == receipt.owner_id
                && value.result_digest == receipt.result_digest.as_deref().unwrap_or_default()
        }));
    }
    Ok(true)
}

fn live_matches(receipt: &PaneReceipt, live: &Value) -> bool {
    live.get("name").and_then(Value::as_str) == Some(receipt.agent.as_str())
        && live.get("agent").and_then(Value::as_str) == Some("omp")
        && live.get("pane_id").and_then(Value::as_str) == Some(receipt.pane_id.as_str())
        && live.get("terminal_id").and_then(Value::as_str) == Some(receipt.terminal_id.as_str())
        && live.pointer("/agent_session/value").and_then(Value::as_str)
            == Some(receipt.session_value.as_str())
}

fn receipt_path(runs: &Path, task_id: TaskId) -> PathBuf {
    runs.join(format!("{task_id}.pane.json"))
}

fn live_agent(agent: &str) -> Result<Value> {
    herdr_get("agent", agent)?
        .pointer("/result/agent")
        .cloned()
        .context("Herdr agent response is missing")
}

fn live_pane(pane_id: &str) -> Result<Value> {
    herdr_get("pane", pane_id)?
        .pointer("/result/pane")
        .cloned()
        .context("Herdr pane response is missing")
}

fn live_tab(tab_id: &str) -> Result<Value> {
    herdr_get("tab", tab_id)?
        .pointer("/result/tab")
        .cloned()
        .context("Herdr tab response is missing")
}

fn herdr_get(kind: &str, id: &str) -> Result<Value> {
    let output = Command::new("herdr").args([kind, "get", id]).output()?;
    if !output.status.success() {
        bail!("Herdr cannot identify {kind} {id}");
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn required_str<'a>(object: &'a Value, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("Herdr agent response is missing {field}"))
}

#[cfg(test)]
mod tests {
    use brgr_protocol::{
        ArtifactContract, AttemptBudget, Decision, DecisionId, DecisionVerdict, ResultEnvelope,
        ResultId, Route, SCHEMA_V1, TaskSpec, TerminalOutcome,
    };

    use super::*;

    #[test]
    fn accepted_result_queues_only_matching_owned_pane() {
        let temp = tempfile::tempdir().unwrap();
        let task_id = TaskId::new();
        let receipt = PaneReceipt {
            task_id,
            attempt_id: brgr_protocol::AttemptId::new(),
            owner_id: OwnerId::new("codex:test").unwrap(),
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            parent_pane_id: "w1:p1".to_owned(),
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
            attempt_id: receipt.attempt_id,
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
            attempt_id: brgr_protocol::AttemptId::new(),
            owner_id: OwnerId::new("codex:test").unwrap(),
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            parent_pane_id: "w1:p1".to_owned(),
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
            attempt_id: receipt.attempt_id,
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![],
            error: None,
            unresolved_effects: vec![],
        };
        mark_pending(temp.path(), task_id, &result).unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        assert_eq!(status(&store, temp.path(), task_id).unwrap(), "retained");
        assert_eq!(
            close_if_eligible(&store, temp.path(), task_id).unwrap(),
            "retained"
        );
    }

    #[test]
    fn missing_ownership_receipt_never_targets_a_pane() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        assert_eq!(
            close_if_eligible(&store, temp.path(), TaskId::new()).unwrap(),
            "no_owned_pane"
        );
    }

    #[test]
    fn changed_agent_session_or_terminal_fails_the_close_check() {
        let task_id = TaskId::new();
        let receipt = PaneReceipt {
            task_id,
            attempt_id: AttemptId::new(),
            owner_id: OwnerId::new("codex:test").unwrap(),
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            parent_pane_id: "w1:p1".to_owned(),
            terminal_id: "term-owned".to_owned(),
            session_value: "session-owned".to_owned(),
            launcher_receipt: PathBuf::from("/tmp/launcher.json"),
            keep_pane: false,
            state: CleanupState::CleanupPending,
            result_id: None,
            result_digest: None,
        };
        let matching = serde_json::json!({
            "name": "brgr-owned",
            "agent": "omp",
            "pane_id": "w1:p2",
            "terminal_id": "term-owned",
            "agent_session": {"value": "session-owned"}
        });
        assert!(live_matches(&receipt, &matching));
        let mut changed_session = matching.clone();
        changed_session["agent_session"]["value"] = serde_json::json!("session-new");
        assert!(!live_matches(&receipt, &changed_session));
        let mut changed_terminal = matching;
        changed_terminal["terminal_id"] = serde_json::json!("term-new");
        assert!(!live_matches(&receipt, &changed_terminal));
    }

    #[test]
    fn candidate_needs_both_owner_decision_and_inbox_ack() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("store")).unwrap();
        let task_id = TaskId::new();
        let attempt_id = AttemptId::new();
        let owner_id = OwnerId::new("codex:test").unwrap();
        let task = TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision: 1,
            create_request_id: "cleanup-test".to_owned(),
            owner_id: owner_id.clone(),
            objective: "test".to_owned(),
            workspace: temp.path().to_string_lossy().into_owned(),
            route: Route {
                harness_id: "local.omp".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec!["completion".to_owned()],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 100,
            },
            acceptance_criteria: vec!["result is reviewable".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 10,
                max_attempts: 1,
            },
        };
        store.record_task(&task, "cleanup-request-digest").unwrap();
        store.claim_attempt(task_id, 1, attempt_id).unwrap();
        let result = ResultEnvelope {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision: 1,
            attempt_id,
            result_id: ResultId::new(),
            outcome: TerminalOutcome::Candidate,
            artifacts: vec![],
            error: None,
            unresolved_effects: vec![],
        };
        store.commit_terminal_result(&owner_id, &result).unwrap();
        let receipt = PaneReceipt {
            task_id,
            attempt_id,
            owner_id: owner_id.clone(),
            agent: "brgr-owned".to_owned(),
            pane_id: "w1:p2".to_owned(),
            parent_pane_id: "w1:p1".to_owned(),
            terminal_id: "term-owned".to_owned(),
            session_value: "session-owned".to_owned(),
            launcher_receipt: temp.path().join("launcher.json"),
            keep_pane: false,
            state: CleanupState::CleanupPending,
            result_id: Some(result.result_id),
            result_digest: Some(Store::result_digest(&result).unwrap()),
        };
        assert!(!decision_is_complete(&store, &receipt).unwrap());
        let decision = Decision {
            schema: SCHEMA_V1.to_owned(),
            decision_id: DecisionId::new(),
            owner_id,
            task_id,
            revision: 1,
            result_id: result.result_id,
            result_digest: Store::result_digest(&result).unwrap(),
            verdict: DecisionVerdict::Accepted,
            reason: "checked".to_owned(),
        };
        store.record_decision(&decision).unwrap();
        assert!(!decision_is_complete(&store, &receipt).unwrap());
        store
            .acknowledge(&decision.owner_id, result.result_id)
            .unwrap();
        assert!(decision_is_complete(&store, &receipt).unwrap());
    }
}
