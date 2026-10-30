//! Read-only event stream for tasks the caller owns.

use std::{
    collections::{BTreeSet, HashSet},
    io::{Write, stdout},
    time::Duration,
};

use anyhow::{Result, bail};
use brgr_protocol::{AttemptState, TaskId};
use brgr_store::{MessageDirection, Store, StoreError};
use serde_json::{Value, json};

use crate::{Paths, cli::Command, plugin_bridge, require_owner, supervision::reconcile_pending};

pub(crate) async fn from_cli(paths: &Paths, command: Command) -> Result<()> {
    let Command::Watch {
        tasks,
        all,
        timeout_seconds,
    } = command
    else {
        bail!("not a watch command");
    };
    watch(paths, tasks, all, timeout_seconds).await
}

pub(crate) async fn watch(
    paths: &Paths,
    tasks: Vec<TaskId>,
    all: bool,
    timeout_seconds: u64,
) -> Result<()> {
    if timeout_seconds == 0 || timeout_seconds > plugin_bridge::MAX_BRIDGE_SECONDS - 120 {
        bail!("watch timeout must be between 1 second and seven days minus the bridge margin");
    }
    let store = Store::open(&paths.store)?;
    let tasks = if all {
        let snapshot = unfinished_owner_tasks(&store)?;
        if snapshot.is_empty() {
            eprintln!("no unfinished tasks for this owner to watch");
            return Ok(());
        }
        snapshot
    } else {
        unique_tasks(tasks)
    };
    for task in &tasks {
        let spec = store.task(*task)?;
        require_owner(&store, &spec.owner_id)?;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let mut last_reconcile = tokio::time::Instant::now() - Duration::from_secs(1);
    let mut watched = tasks
        .into_iter()
        .map(|task| Watched {
            task,
            last_state: None,
            questions: BTreeSet::new(),
            reported_result: false,
        })
        .collect::<Vec<_>>();

    loop {
        if last_reconcile.elapsed() >= Duration::from_secs(1) {
            // Same recovery cadence as `wait`: a dead supervisor becomes a lost
            // result the stream can report. This command never decides or acks.
            reconcile_pending(paths)?;
            last_reconcile = tokio::time::Instant::now();
        }
        let mut all_done = true;
        for task in &mut watched {
            publish_task(&store, task, &mut all_done)?;
        }
        if all_done {
            emit(&json!({"event": "done"}))?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            emit(&json!({"event": "timeout"}))?;
            bail!("watch timed out before every task had a result");
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(Duration::from_millis(200).min(remaining)).await;
    }
}

struct Watched {
    task: TaskId,
    last_state: Option<&'static str>,
    questions: BTreeSet<String>,
    reported_result: bool,
}

fn unique_tasks(tasks: Vec<TaskId>) -> Vec<TaskId> {
    let mut seen = HashSet::new();
    tasks
        .into_iter()
        .filter(|task| seen.insert(*task))
        .collect()
}

fn unfinished_owner_tasks(store: &Store) -> Result<Vec<TaskId>> {
    let owner = crate::owner_from_environment()?;
    Ok(store
        .unfinished_tasks_for_owner(&owner)?
        .into_iter()
        .map(|spec| spec.task_id)
        .collect())
}

fn publish_task(store: &Store, watched: &mut Watched, all_done: &mut bool) -> Result<()> {
    let label = match store.attempt_state(watched.task) {
        Ok(state) => state_label(state),
        Err(StoreError::TaskNotFound(_)) => "queued",
        Err(error) => return Err(error.into()),
    };
    if watched.last_state != Some(label) {
        let first = watched.last_state.is_none();
        watched.last_state = Some(label);
        // Collecting and terminal are the sealed result, reported as `result`.
        // A later poll must not add a second state line for that transition.
        if first || !matches!(label, "collecting" | "terminal" | "cancel_requested") {
            emit(&json!({
                "event": "state",
                "task_id": watched.task,
                "state": label,
            }))?;
        }
    }

    if let Ok(attempt) = store.latest_message_attempt(watched.task) {
        let questions =
            store.unanswered_questions(watched.task, attempt, MessageDirection::WorkerToOwner)?;
        for question in questions {
            if watched.questions.insert(question.message_id.clone()) {
                emit(&json!({
                    "event": "question",
                    "task_id": watched.task,
                    "message_id": question.message_id,
                }))?;
            }
        }
    }

    match store.latest_result(watched.task) {
        Ok(result) => {
            if !watched.reported_result {
                watched.reported_result = true;
                emit(&json!({
                    "event": "result",
                    "task_id": watched.task,
                    "outcome": result.outcome,
                    "revision": result.revision,
                }))?;
            }
        }
        Err(StoreError::TaskNotFound(_)) => *all_done = false,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn state_label(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Queued => "queued",
        AttemptState::Starting => "starting",
        AttemptState::Running => "running",
        AttemptState::Blocked => "blocked",
        AttemptState::Collecting => "collecting",
        AttemptState::CancelRequested => "cancel_requested",
        AttemptState::Terminal => "terminal",
    }
}

fn emit(event: &Value) -> Result<()> {
    let mut stdout = stdout().lock();
    serde_json::to_writer(&mut stdout, event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
