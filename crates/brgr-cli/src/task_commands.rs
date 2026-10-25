//! Reading and deciding tasks: status, result, wait, cancel, bind, accept/reject.

use std::{
    env,
    fs::{self},
    time::Duration,
};

use crate::supervision::reconcile_pending;
use crate::{
    LaunchEnvelope, Paths, current_session, notification, pane_adapter, pane_cleanup,
    plugin_bridge, print_value, require_owner, tree_status, workspace,
};
use anyhow::{Context, Result, bail};
use brgr_protocol::{
    AttemptState, Decision, DecisionId, DecisionVerdict, OwnerId, SCHEMA_V1, TaskId,
    TerminalOutcome,
};
use brgr_store::{Store, StoreError, SubtreeNode};
use serde_json::json;

pub(crate) fn status(
    paths: &Paths,
    task: Option<TaskId>,
    tree: bool,
    observe: bool,
    json_output: bool,
) -> Result<()> {
    // Recovery records dead supervisors' tasks as lost and runs cleanups, which
    // are writes; an observer only reads what is already recorded.
    if !observe {
        reconcile_pending(paths)?;
    }
    let store = Store::open(&paths.store)?;
    if tree {
        let root = task.context("--tree requires a task ID")?;
        let spec = store.task(root)?;
        check_reader(&store, &spec.owner_id, observe)?;
        return tree_status::show(&store, root, json_output);
    }
    if let Some(task_id) = task {
        let spec = store.task(task_id)?;
        check_reader(&store, &spec.owner_id, observe)?;
        let state = match store.attempt_state(task_id) {
            Ok(state) => state,
            Err(StoreError::TaskNotFound(_)) => AttemptState::Queued,
            Err(error) => return Err(error.into()),
        };
        let launch = fs::read(paths.launch(task_id, spec.revision))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LaunchEnvelope>(&bytes).ok());
        print_value(
            &json!({
                "task": spec,
                "state": format!("{state:?}").to_lowercase(),
                "workspace": spec.workspace,
                "base_commit": spec.evidence.base_commit,
                "workspace_present": workspace::workspace_is_present(&spec.workspace),
                "session": crate::pane_adapter::session_status(paths, task_id, spec.revision),
                "calling_options": launch.as_ref().and_then(|value|value.calling_options.as_ref()),
                "source_pane": launch.as_ref().and_then(|value|value.source_pane.as_ref()),
                "source_session": launch.as_ref().and_then(|value|value.source_session.as_ref()),
            }),
            json_output,
        );
    } else {
        let session = current_session()?;
        let owner = env::var("BRGR_OWNER_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(OwnerId::new)
            .transpose()?;
        let tasks = store
            .tasks_for_session(session.as_deref().unwrap_or(""), owner.as_ref(), 20)?
            .into_iter()
            .map(|spec| {
                let state = store.attempt_state(spec.task_id).ok();
                json!({"task": spec, "state": state.map(|value| format!("{value:?}").to_lowercase())})
            })
            .collect::<Vec<_>>();
        print_value(&json!(tasks), json_output);
    }
    Ok(())
}

/// The owner's session for a read, or, with `--observe`, no check at all.
pub(crate) fn check_reader(store: &Store, owner: &OwnerId, observe: bool) -> Result<()> {
    if observe {
        crate::observe_owner(owner);
        Ok(())
    } else {
        require_owner(store, owner).map(|_| ())
    }
}

pub(crate) fn result(
    paths: &Paths,
    task: TaskId,
    ack: bool,
    observe: bool,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let (session_id, binding_epoch) = if observe {
        crate::observe_owner(&spec.owner_id);
        (String::new(), 0)
    } else {
        require_owner(&store, &spec.owner_id)?
    };
    let result = store.latest_result(task)?;
    let route_observation = store
        .route_observation(result.result_id)?
        .or_else(|| result.legacy_embedded_route_observation.clone());
    let artifacts = result
        .artifacts
        .iter()
        .map(|reference| {
            let bytes = store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
            let text = (reference.media_type.starts_with("text/")
                || reference.media_type == "application/json")
                .then(|| String::from_utf8_lossy(&bytes).into_owned());
            Ok::<_, anyhow::Error>(json!({
                "reference": reference,
                "text": text,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    if ack {
        store.acknowledge_bound(&spec.owner_id, result.result_id, &session_id, binding_epoch)?;
        if let Err(error) = pane_cleanup::mark_pending(&paths.runs, task, &result) {
            eprintln!("brgr pane cleanup queue could not be updated: {error}");
        } else if let Err(error) = pane_cleanup::close_if_eligible(&store, &paths.runs, task) {
            eprintln!("brgr pane cleanup remains pending: {error}");
        }
        // The acknowledgement is already durable: a cleanup problem is reported,
        // not returned, or the caller would retry a decision that was recorded.
        if let Err(error) = crate::pane_adapter::cleanup_settled(paths, task, spec.revision) {
            eprintln!("brgr cleanup pending: {error}");
        }
        crate::worktree_prune::reclaim_and_report(paths, task, spec.revision);
    }
    print_value(
        &json!({"result": result, "artifacts": artifacts, "route_observation": route_observation}),
        json_output,
    );
    Ok(())
}

pub(crate) async fn wait_for_result(
    paths: &Paths,
    task: TaskId,
    timeout_seconds: u64,
    json_output: bool,
) -> Result<()> {
    if timeout_seconds == 0 || timeout_seconds > plugin_bridge::MAX_BRIDGE_SECONDS - 120 {
        bail!("wait timeout must be between 1 second and seven days minus the bridge margin");
    }
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let mut last_reconcile = tokio::time::Instant::now() - Duration::from_secs(1);
    loop {
        if last_reconcile.elapsed() >= Duration::from_secs(1) {
            reconcile_pending(paths)?;
            last_reconcile = tokio::time::Instant::now();
        }
        match store.latest_result(task) {
            Ok(result) => {
                print_value(
                    &json!({
                        "task_id": task,
                        "result_id": result.result_id,
                        "outcome": result.outcome,
                        "revision": result.revision,
                    }),
                    json_output,
                );
                return Ok(());
            }
            Err(StoreError::TaskNotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        if let Ok(attempt) = store.latest_message_attempt(task) {
            let questions = store.unanswered_questions(
                task,
                attempt,
                brgr_store::MessageDirection::WorkerToOwner,
            )?;
            if !questions.is_empty() {
                print_value(
                    &json!({"task_id":task,"state":"awaiting_input","waiting_for":"question","messages":questions}),
                    json_output,
                );
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("task {task} did not produce a terminal result before the wait timeout");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub(crate) fn cancel(
    paths: &Paths,
    task: TaskId,
    tree: bool,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let proof = if force {
        eprintln!(
            "brgr: force-cancelling a task owned by {}; session check skipped",
            spec.owner_id
        );
        None
    } else {
        let (session_id, binding_epoch) = require_owner(&store, &spec.owner_id)?;
        Some(brgr_store::OwnerProof::new(
            spec.owner_id.clone(),
            &session_id,
            binding_epoch,
        )?)
    };
    let nodes = if tree {
        store.subtree(task)?
    } else {
        vec![SubtreeNode {
            task_id: task,
            parent_task_id: None,
            depth: 0,
        }]
    };
    let mut active = Vec::new();
    for node in &nodes {
        let child = store.task(node.task_id)?;
        if !task_needs_cancel(&store, node.task_id)? {
            continue;
        }
        let launch: LaunchEnvelope =
            serde_json::from_slice(&fs::read(paths.launch(node.task_id, child.revision))?)?;
        if launch
            .manifest
            .as_ref()
            .map_or(child.route.harness_id == "local.omp-herdr", |manifest| {
                manifest.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1
            })
        {
            bail!("Herdr-backed OMP cancellation is not certified; use the process adapter");
        }
        active.push(node.task_id);
    }
    if active.is_empty() && !tree {
        if force {
            let had_pane = paths.pane_receipt(task, spec.revision).is_file();
            pane_adapter::close_leftover_pane(paths, task, spec.revision, false);
            let pane_closed = had_pane
                && fs::read(paths.pane_receipt(task, spec.revision))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|value| {
                        value
                            .get("cleanup")
                            .and_then(|cleanup| cleanup.as_str())
                            .map(|cleanup| cleanup == "closed")
                    })
                    .unwrap_or(false);
            print_value(
                &json!({
                    "task_id": task,
                    "state": "force_stopped",
                    "tree": tree,
                    "pane_closed": pane_closed,
                }),
                json_output,
            );
            return Ok(());
        }
        bail!("task {task} is already terminal");
    }
    // The intent records live inside the same transaction that checks the
    // session is still the head: work handed away is stopped by the new head,
    // not by a session that lost it while this command ran. `--force` skips
    // that check: the control home is the access check.
    let recorded = match proof.as_ref() {
        Some(proof) => store.record_cancellation_intents_bound(task, tree, proof)?,
        None => store.record_cancellation_intents(task, tree)?,
    };
    let mut requested = Vec::new();
    for node in &recorded {
        if task_needs_cancel(&store, node.task_id)? {
            fs::write(paths.cancel(node.task_id), b"cancel\n")?;
            requested.push(node.task_id);
        }
    }
    if force && tree {
        for node in &nodes {
            if !requested.iter().any(|id| *id == node.task_id) {
                let child = store.task(node.task_id)?;
                pane_adapter::close_leftover_pane(paths, node.task_id, child.revision, false);
            }
        }
    }
    print_value(
        &json!({"task_id": task, "state": "cancel_requested", "tree": tree,
        "requested": requested}),
        json_output,
    );
    Ok(())
}

pub(crate) fn task_needs_cancel(store: &Store, task: TaskId) -> Result<bool> {
    match store.attempt_state(task) {
        Ok(AttemptState::Terminal) => Ok(false),
        Ok(_) | Err(StoreError::TaskNotFound(_)) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn bind(
    paths: &Paths,
    task: TaskId,
    session: Option<String>,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    if let Ok(explicit_owner) = env::var("BRGR_OWNER_ID")
        && explicit_owner != spec.owner_id.as_str()
    {
        bail!("task belongs to {}; BRGR_OWNER_ID differs", spec.owner_id);
    }
    let observed = current_session()?;
    let session = session
        .or_else(|| observed.clone())
        .context("provide --session SESSION or run inside a Codex session")?;
    if observed.as_ref().is_some_and(|value| value != &session) {
        bail!("requested session does not match the current Codex session");
    }
    if let Some(takeover) = store
        .owner_takeovers(&spec.owner_id)?
        .into_iter()
        .find(|takeover| !takeover.state.is_terminal())
    {
        bail!(
            "takeover {} is {}; finish or abort it with `brgr head ...` before binding",
            takeover.takeover_id,
            takeover.state.as_str()
        );
    }
    let epoch = store.rebind_owner(&spec.owner_id, &session)?;
    let surface_ready =
        match notification::register_current_surface(paths, &store, &spec.owner_id, &session).await
        {
            Ok(ready) => ready,
            Err(error) => {
                eprintln!("brgr completion notification remains queued: {error}");
                false
            }
        };
    if surface_ready {
        for pending in store.pending_notification_tasks_for_session(&session)? {
            if let Err(error) = notification::spawn_for_task(paths, pending) {
                eprintln!("brgr completion notification remains queued: {error}");
            }
        }
    }
    print_value(
        &json!({"owner_id": spec.owner_id, "session_id": session, "binding_epoch": epoch}),
        json_output,
    );
    Ok(())
}

pub(crate) fn decide(
    paths: &Paths,
    task: TaskId,
    decision: (DecisionVerdict, String, bool),
    json_output: bool,
) -> Result<()> {
    let persisted = record_verdict(paths, task, decision)?;
    print_value(&serde_json::to_value(persisted)?, json_output);
    Ok(())
}

/// Records the owner's accept or reject of a candidate, acknowledges it, and
/// starts the cleanup its decision allows. Prints nothing.
pub(crate) fn record_verdict(
    paths: &Paths,
    task: TaskId,
    (verdict, reason, include_ignored): (DecisionVerdict, String, bool),
) -> Result<Decision> {
    if reason.trim().is_empty() {
        bail!("decision reason must not be empty");
    }
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let (session_id, binding_epoch) = require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    if result.outcome != TerminalOutcome::Candidate {
        bail!("only candidate results can be accepted or rejected");
    }
    if result.artifacts.is_empty() {
        bail!("candidate result has no sealed artifact");
    }
    for reference in &result.artifacts {
        store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
    }
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: spec.owner_id.clone(),
        task_id: task,
        revision: result.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result)?,
        session_id: Some(session_id),
        binding_epoch: Some(binding_epoch),
        verdict,
        reason,
    };
    // Written before the decision, not after: recording it lets the pane's own
    // cleanup reclaim the worktree at once, and a reclaim that ran first would
    // assess it without the flag and mark it assessed for good. It names this
    // decision, so one that never gets recorded cannot leak into a later one.
    if include_ignored {
        crate::worktree_prune::allow_ignored(
            paths,
            task,
            spec.revision,
            &decision.decision_id.to_string(),
        );
    }
    if let Err(error) = store.record_decision_and_ack(&decision) {
        if include_ignored {
            crate::worktree_prune::forget_ignored(paths, task, spec.revision);
        }
        return Err(error.into());
    }
    if let Err(error) = crate::pane_adapter::cleanup_settled(paths, task, spec.revision) {
        eprintln!("brgr cleanup pending: {error}");
    }
    let persisted = store
        .decision_for_result(result.result_id)?
        .context("decision was not readable after commit")?;
    // A replay of an earlier, identical decision keeps that decision's id.
    if include_ignored && persisted.decision_id != decision.decision_id {
        crate::worktree_prune::allow_ignored(
            paths,
            task,
            spec.revision,
            &persisted.decision_id.to_string(),
        );
    }
    if let Err(error) = pane_cleanup::mark_pending(&paths.runs, task, &result) {
        eprintln!("brgr pane cleanup queue could not be updated: {error}");
    } else if let Err(error) = pane_cleanup::close_if_eligible(&store, &paths.runs, task) {
        eprintln!("brgr pane cleanup remains pending: {error}");
    }
    crate::worktree_prune::reclaim_and_report(paths, task, spec.revision);
    Ok(persisted)
}
