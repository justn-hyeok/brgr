use std::{
    env,
    fs::{self, OpenOptions, TryLockError},
    os::unix::fs::OpenOptionsExt as _,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{OwnerId, TaskId, TaskSpec, TerminalOutcome};
use brgr_store::{
    NotificationTarget, PendingWorkflowNotification, QuestionTarget, Store, StoreError,
    TakeoverState, WorkflowNotificationTarget, WorkflowRunId, WorkflowRunState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{Paths, plugin_bridge};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// The longest wait between delivery attempts while the owner stays busy.
const MAX_BACKOFF: Duration = Duration::from_secs(3);
const MAX_LIFETIME: Duration = Duration::from_hours(24);
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

/// Records the pane of a head that has just taken command, and starts delivery
/// of the notices a takeover requeued for it.
///
/// Notices go to the pane recorded for the owner's bound session, and a takeover
/// binds a session nothing had recorded, so a worker that finished after the
/// handoff still reached the old head's pane. The detached dispatcher cannot
/// prove which pane the new head runs in; this command can, and a head started by
/// `head handoff` names its own pane, which brgr opened. Best effort: a notice
/// that cannot be delivered stays queued.
pub async fn register_incoming_head(paths: &Paths, incoming: &crate::head::IncomingHead) {
    let Ok(store) = Store::open(&paths.store) else {
        return;
    };
    let (owner_id, session_id) = (&incoming.owner, incoming.session.as_str());
    let mut ready = false;
    if let Some(offered) = &incoming.offered {
        match register_offered_pane(paths, &store, owner_id, session_id, offered).await {
            Ok(done) => ready = done,
            Err(error) => eprintln!("brgr completion notification remains queued: {error}"),
        }
    }
    if !ready {
        match register_current_surface(paths, &store, owner_id, session_id).await {
            Ok(done) => ready = done,
            Err(error) => eprintln!("brgr completion notification remains queued: {error}"),
        }
    }
    if ready {
        spawn_pending_for_session(paths, &store, session_id);
    }
}

/// Whether `session` took command of `owner` by a takeover. Such a head may run
/// in a different harness than the one the owner id names.
fn is_taken_head(store: &Store, owner_id: &OwnerId, session: &str) -> bool {
    store.owner_takeovers(owner_id).is_ok_and(|takeovers| {
        takeovers.iter().any(|takeover| {
            takeover.to_session == session
                && matches!(
                    takeover.state,
                    TakeoverState::Committed
                        | TakeoverState::Resumed
                        | TakeoverState::Retired
                        | TakeoverState::RetireFailed
                )
        })
    })
}

/// The owner-inbox kind a Herdr agent reports, if it is one.
fn inbox_kind(live: &str) -> Option<&'static str> {
    match live {
        "claude" => Some("claude"),
        "cursor" => Some("cursor"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// Records the pane `head handoff` opened, proven by what Herdr says about it
/// now: it is still the recorded terminal and runs an owner-inbox agent. A Codex
/// head's session is reported to Herdr, as it is for any Codex owner.
async fn register_offered_pane(
    paths: &Paths,
    store: &Store,
    owner_id: &OwnerId,
    session_id: &str,
    offered: &crate::head::OfferedPane,
) -> Result<bool> {
    let binary = PathBuf::from(&offered.herdr_bin);
    if !binary.is_absolute() || !binary.is_file() {
        return Ok(false);
    }
    if offered.terminal.is_some()
        && crate::pane_adapter::head_pane::terminal_of(&offered.pane) != offered.terminal
    {
        return Ok(false);
    }
    let Some((bound, epoch)) = store.owner_binding(owner_id)? else {
        return Ok(false);
    };
    if bound != session_id {
        return Ok(false);
    }
    let binary = binary
        .to_str()
        .context("Herdr executable path is not UTF-8")?;
    let herdr_session = offered.herdr_session.as_deref();
    let agent = recorded_agent(binary, herdr_session, &offered.pane).await?;
    let Some(kind) = agent
        .get("agent")
        .and_then(Value::as_str)
        .and_then(inbox_kind)
    else {
        return Ok(false);
    };
    if agent.get("pane_id").and_then(Value::as_str) != Some(offered.pane.as_str()) {
        bail!("recorded Herdr pane is not the incoming head's pane");
    }
    if kind == "codex" {
        ensure_codex_session_reported(binary, herdr_session, &offered.pane, session_id, &agent)
            .await?;
    }
    if kind == "claude"
        && let Some(process) = claude_ancestor(&offered.pane)
    {
        record_claude_owner(paths, session_id, &process)?;
    }
    store.register_owner_surface(
        owner_id,
        session_id,
        epoch,
        &offered.pane,
        herdr_session,
        binary,
    )?;
    Ok(true)
}

pub async fn register_current_surface(
    paths: &Paths,
    store: &Store,
    owner_id: &OwnerId,
    session_id: &str,
) -> Result<bool> {
    if owner_id.as_str().starts_with("worker:") {
        return register_worker_surface(paths, store, owner_id, session_id);
    }
    let Some(kind) = owner_agent(owner_id.as_str()) else {
        return Ok(false);
    };
    // A hook Codex runs from its shared app-server daemon carries the Herdr
    // environment of whichever pane started that daemon. Registering that pane
    // would push this session's notices into another session.
    let (Some(pane), Some(binary)) = (
        crate::caller_pane::for_session(session_id, crate::invocation::current().pane.as_deref())
            .or_else(|| {
                (crate::current_session().ok().flatten().as_deref() == Some(session_id))
                    .then(crate::caller_pane::verified)
                    .flatten()
            }),
        crate::caller_pane::binary(),
    ) else {
        return Ok(false);
    };
    let binary = PathBuf::from(binary);
    if pane.is_empty() || !binary.is_absolute() || !binary.is_file() {
        return Ok(false);
    }
    let (bound_session, epoch) = store
        .owner_binding(owner_id)?
        .context("Codex owner has no session binding")?;
    if bound_session != session_id {
        return Ok(false);
    }
    let binary = binary
        .to_str()
        .context("Herdr executable path is not UTF-8")?;
    let herdr_session = env::var("HERDR_SESSION").ok();
    let agent = recorded_agent(binary, herdr_session.as_deref(), &pane).await?;
    let recorded_kind = agent.get("agent").and_then(Value::as_str);
    // A head that took command by a takeover runs whatever harness it runs.
    let kind = if is_taken_head(store, owner_id, session_id) {
        recorded_kind.and_then(inbox_kind).unwrap_or(kind)
    } else {
        kind
    };
    if recorded_kind.is_some_and(|found| !crate::caller_pane::owner_inbox_kind(found)) {
        eprintln!(
            "brgr: Herdr agent {} is not an owner inbox; FROM BRGR will not arrive. Use `brgr watch`.",
            recorded_kind.unwrap_or("unknown")
        );
        return Ok(false);
    }
    if recorded_kind != Some(kind)
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane.as_ref())
    {
        bail!("recorded Herdr pane is not the current owner agent");
    }
    // Herdr keeps no session id for a Claude Code or Cursor agent. The Claude
    // Code process this command runs under is recorded instead; Cursor runs in
    // the pane. Codex is bound to its exact session below.
    if kind != "codex" {
        if kind == "claude"
            && let Some(owner) = claude_ancestor(&pane)
        {
            record_claude_owner(paths, session_id, &owner)?;
        }
        store.register_owner_surface(
            owner_id,
            session_id,
            epoch,
            &pane,
            herdr_session.as_deref(),
            binary,
        )?;
        return Ok(true);
    }
    ensure_codex_session_reported(binary, herdr_session.as_deref(), &pane, session_id, &agent)
        .await?;
    store.register_owner_surface(
        owner_id,
        session_id,
        epoch,
        &pane,
        herdr_session.as_deref(),
        binary,
    )?;
    Ok(true)
}

async fn ensure_codex_session_reported(
    binary: &str,
    herdr_session: Option<&str>,
    pane: &str,
    session_id: &str,
    agent: &Value,
) -> Result<()> {
    match agent
        .pointer("/agent_session/value")
        .and_then(Value::as_str)
    {
        Some(observed) if observed == session_id => Ok(()),
        Some(_) => bail!("recorded Herdr Codex session differs from the owner binding"),
        None => {
            let mut report = surface_command(binary, herdr_session);
            report.args([
                "pane",
                "report-agent-session",
                pane,
                "--source",
                "herdr:codex",
                "--agent",
                "codex",
                "--agent-session-id",
                session_id,
            ]);
            let output = timeout(HERDR_TIMEOUT, report.kill_on_drop(true).output())
                .await
                .context("Herdr Codex session report timed out")??;
            if !output.status.success() {
                bail!("Herdr did not accept the Codex session report");
            }
            let reported = recorded_agent(binary, herdr_session, pane).await?;
            if reported.get("agent").and_then(Value::as_str) != Some("codex")
                || reported.get("pane_id").and_then(Value::as_str) != Some(pane)
                || reported
                    .pointer("/agent_session/value")
                    .and_then(Value::as_str)
                    != Some(session_id)
            {
                bail!("Herdr did not bind the exact Codex session to the pane");
            }
            Ok(())
        }
    }
}

fn register_worker_surface(
    paths: &Paths,
    store: &Store,
    owner_id: &OwnerId,
    session_id: &str,
) -> Result<bool> {
    let attempt = owner_id
        .as_str()
        .trim_start_matches("worker:")
        .parse::<brgr_protocol::AttemptId>()?;
    let parent = store
        .unfinished_attempts()?
        .into_iter()
        .find(|item| item.attempt_id == attempt)
        .context("parent worker attempt is not active")?;
    let source =
        crate::pane_adapter::session_status(paths, parent.task.task_id, parent.task.revision)
            .context("parent has no native TUI session")?;
    let pane = source
        .get("pane")
        .and_then(Value::as_str)
        .context("parent TUI pane is missing")?;
    let binary = crate::caller_pane::binary().context("Herdr executable missing")?;
    let (bound, epoch) = store
        .owner_binding(owner_id)?
        .context("worker owner binding missing")?;
    if bound != session_id {
        bail!("worker session differs from owner binding");
    }
    store.register_owner_surface(
        owner_id,
        session_id,
        epoch,
        pane,
        env::var("HERDR_SESSION").ok().as_deref(),
        binary.to_str().context("Herdr path is not UTF-8")?,
    )?;
    Ok(true)
}

pub fn spawn_registration_and_delivery(paths: &Paths, task: &TaskSpec, session: Option<&str>) {
    if crate::config::Config::load(&paths.config)
        .is_ok_and(|config| config.calling(&task.route.harness_id).notifications == Some(false))
    {
        return;
    }
    if session.is_none()
        || (crate::invocation::current().pane.is_none()
            && (env::var("HERDR_ENV").as_deref() != Ok("1")
                || env::var_os("HERDR_PANE_ID").is_none()))
        || !(task.owner_id.as_str().starts_with("codex:")
            || task.owner_id.as_str().starts_with("claude:")
            || task.owner_id.as_str().starts_with("cursor:")
            || task.owner_id.as_str().starts_with("worker:"))
        || crate::caller_pane::binary().is_none()
    {
        return;
    }
    if let Err(error) = spawn_for_task(paths, task.task_id) {
        eprintln!("brgr completion notification remains queued: {error}");
    }
}

async fn recorded_agent(binary: &str, herdr_session: Option<&str>, pane: &str) -> Result<Value> {
    let mut get = surface_command(binary, herdr_session);
    get.args(["agent", "get", pane]);
    let output = timeout(HERDR_TIMEOUT, get.kill_on_drop(true).output())
        .await
        .context("Herdr Codex pane lookup timed out")??;
    if !output.status.success() || output.stdout.len() > 65_536 {
        bail!("recorded Codex pane is unavailable");
    }
    let state: Value = serde_json::from_slice(&output.stdout)?;
    state
        .pointer("/result/agent")
        .cloned()
        .context("Herdr did not return a Codex agent")
}

fn surface_command(binary: &str, herdr_session: Option<&str>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    if let Some(session) = herdr_session {
        command.arg("--session").arg(session);
    }
    command
}

pub fn spawn_for_task(paths: &Paths, task: TaskId) -> Result<()> {
    if !notifications_enabled(paths, task)? {
        return Ok(());
    }
    let task_id = task.to_string();
    spawn_dispatcher(
        paths,
        &format!("{task}.notify.log"),
        &["__notify", &task_id],
        |command| configure_task_dispatcher(paths, command, task),
    )
}

/// Starts the detached dispatcher that pushes one workflow run's notices.
///
/// # Errors
///
/// Returns an error if the dispatcher process cannot be started.
pub fn spawn_for_workflow(paths: &Paths, run_id: &WorkflowRunId) -> Result<()> {
    spawn_dispatcher(
        paths,
        &format!("{run_id}.workflow.notify.log"),
        &["__notify-workflow", run_id.as_str()],
        |command| {
            if let Ok(store) = Store::open(&paths.store)
                && let Ok(run) = store.workflow_run(run_id)
                && let Ok(Some((session, _))) = store.owner_binding(&run.owner_id)
            {
                command.args(["--owner-session", &session]);
            }
        },
    )
}

/// Spawns delivery when a run actually moved onto an owner-facing state.
pub fn spawn_for_workflow_transition(
    paths: &Paths,
    previous: WorkflowRunState,
    run: &brgr_store::WorkflowRun,
) {
    if previous == run.state || !run.state.notifies_owner() {
        return;
    }
    if let Err(error) = spawn_for_workflow(paths, &run.run_id) {
        eprintln!("brgr workflow notification remains queued: {error}");
    }
}

pub fn spawn_pending_for_session(paths: &Paths, store: &Store, session_id: &str) {
    for task in store
        .pending_notification_tasks_for_session(session_id)
        .unwrap_or_default()
    {
        if let Err(error) = spawn_for_task(paths, task) {
            eprintln!("brgr completion notification remains queued: {error}");
        }
    }
    spawn_pending_workflow_for_session(paths, store, session_id);
}

pub(crate) fn spawn_pending_workflow_for_session(paths: &Paths, store: &Store, session_id: &str) {
    for run_id in store
        .pending_workflow_notification_runs_for_session(session_id)
        .unwrap_or_default()
    {
        if let Err(error) = spawn_for_workflow(paths, &run_id) {
            eprintln!("brgr workflow notification remains queued: {error}");
        }
    }
}

fn configure_task_dispatcher(paths: &Paths, command: &mut Command, task: TaskId) {
    let Ok(store) = Store::open(&paths.store) else {
        return;
    };
    let Ok(spec) = store.task(task) else {
        return;
    };
    let Ok(Some((session, _))) = store.owner_binding(&spec.owner_id) else {
        return;
    };
    command.args(["--owner-session", &session]);
    if let Some(pane) = fs::read(paths.launch(task, spec.revision))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<crate::LaunchEnvelope>(&bytes).ok())
        .and_then(|launch| launch.source_pane)
        .filter(|pane| crate::caller_pane::pending_session(pane).is_none())
    {
        command.args(["--source-pane", &pane]);
    }
}

fn spawn_dispatcher(
    paths: &Paths,
    log_name: &str,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Result<()> {
    let mut command = Command::new(env::current_exe()?);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.runs.join(log_name))?;
    configure(&mut command);
    command
        .arg("--home")
        .arg(&paths.home)
        .args(args)
        .env_remove(plugin_bridge::BRIDGE_DIR_ENV)
        .env_remove(plugin_bridge::BRIDGE_HOST_HOME_ENV)
        .env_remove(plugin_bridge::BRIDGE_HOST_WORKSPACE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    command.process_group(0);
    command
        .spawn()
        .context("brgr notification dispatcher could not start")?;
    Ok(())
}

pub async fn deliver_pending(paths: &Paths, task: TaskId) -> Result<()> {
    if !notifications_enabled(paths, task)? {
        return Ok(());
    }
    let Some(_lock) = try_notify_lock(&paths.runs.join(format!("{task}.notify.lock")))? else {
        return Ok(());
    };
    let started = Instant::now();
    let store = open_store(paths, started).await?;
    // A finished task with nothing left to send exits before anything else:
    // proving the owner's pane reads every Codex pane, which takes seconds.
    if settled(&store, task)? {
        return Ok(());
    }
    let mut registered = register_for_task(paths, &store, task).await;
    let mut next_try = Instant::now() + Duration::from_secs(2);
    let mut gap = Duration::from_secs(2);
    let database = paths.store.join("brgr.sqlite3");
    let mut backoff = POLL_INTERVAL;
    while started.elapsed() < MAX_LIFETIME {
        // An open connection keeps reading a deleted database, so a dispatcher
        // whose control home was removed polled a dead store for a day. Test
        // suites delete their homes; each run left one such process behind.
        if !database.exists() {
            return Ok(());
        }
        // The owner's pane may only be provable once the call that started this
        // task has finished and shown up on its screen, so keep trying.
        if !registered && Instant::now() >= next_try {
            registered = register_for_task(paths, &store, task).await;
            gap = (gap * 2).min(Duration::from_mins(1));
            next_try = Instant::now() + gap;
        }
        deliver_questions(paths, &store, task).await;
        let pending = match store.pending_notifications_for_task(task) {
            Ok(pending) => pending,
            Err(error) if error.is_retryable_database_contention() => {
                sleep(POLL_INTERVAL).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if pending.is_empty() {
            match settled(&store, task) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
            sleep(POLL_INTERVAL).await;
            continue;
        }
        let mut failed = false;
        for notice in pending {
            let token = Uuid::new_v4().to_string();
            let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
            let claimed = match store.claim_notification(notice.result_id, &token, now, 20) {
                Ok(claimed) => claimed,
                Err(error) if error.is_retryable_database_contention() => continue,
                Err(error) => return Err(error.into()),
            };
            let Some(target) = claimed else {
                continue;
            };
            match try_deliver(paths, &target).await {
                Ok(()) => {
                    commit_delivery(|| store.mark_notification_delivered(&target, &token)).await?;
                }
                Err(error) => {
                    failed = true;
                    if let Err(release_error) = store.release_notification_claim(
                        notice.result_id,
                        &token,
                        &error.to_string(),
                    ) && !release_error.is_retryable_database_contention()
                    {
                        return Err(release_error.into());
                    }
                }
            }
        }
        // A busy owner is retried at a growing interval, not twice a second.
        backoff = if failed {
            (backoff * 2).min(MAX_BACKOFF)
        } else {
            POLL_INTERVAL
        };
        sleep(backoff).await;
    }
    Ok(())
}

/// The prompt was accepted. Keep the lease while retrying its receipt, so
/// transient database contention does not trigger another prompt.
async fn commit_delivery(mut mark: impl FnMut() -> Result<(), StoreError>) -> Result<()> {
    let started = Instant::now();
    loop {
        match mark() {
            Ok(()) | Err(StoreError::NotificationClaimStale) => return Ok(()),
            Err(error)
                if error.is_retryable_database_contention()
                    && started.elapsed() < Duration::from_secs(15) =>
            {
                sleep(POLL_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn try_notify_lock(path: &Path) -> Result<Option<std::fs::File>> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Delivers queued workflow notices for one run until none remain.
///
/// # Errors
///
/// Returns an error if the store cannot be opened or a durable write fails.
pub async fn deliver_pending_workflow(paths: &Paths, run: &str) -> Result<()> {
    let run_id = WorkflowRunId::new(run)?;
    let Some(_lock) = try_notify_lock(&paths.runs.join(format!("{run_id}.workflow.notify.lock")))?
    else {
        return Ok(());
    };
    let started = Instant::now();
    let store = open_store(paths, started).await?;
    let mut registered = register_for_workflow(paths, &store, &run_id).await;
    let mut next_try = Instant::now() + Duration::from_secs(2);
    let mut gap = Duration::from_secs(2);
    let database = paths.store.join("brgr.sqlite3");
    let mut backoff = POLL_INTERVAL;
    while started.elapsed() < MAX_LIFETIME {
        if !database.exists() {
            return Ok(());
        }
        if !registered && Instant::now() >= next_try {
            registered = register_for_workflow(paths, &store, &run_id).await;
            gap = (gap * 2).min(Duration::from_mins(1));
            next_try = Instant::now() + gap;
        }
        match drain_workflow_notices(paths, &store, &run_id).await? {
            Drain::Empty => return Ok(()),
            Drain::Retry => {
                backoff = (backoff * 2).min(MAX_BACKOFF);
                sleep(backoff).await;
            }
            Drain::Progress => {
                backoff = POLL_INTERVAL;
                sleep(backoff).await;
            }
        }
    }
    Ok(())
}

enum Drain {
    Empty,
    Retry,
    Progress,
}

async fn drain_workflow_notices(
    paths: &Paths,
    store: &Store,
    run_id: &WorkflowRunId,
) -> Result<Drain> {
    let pending = match store.pending_workflow_notifications(run_id) {
        Ok(pending) => pending,
        Err(error) if error.is_retryable_database_contention() => {
            sleep(POLL_INTERVAL).await;
            return Ok(Drain::Retry);
        }
        Err(error) => return Err(error.into()),
    };
    if pending.is_empty() {
        return Ok(Drain::Empty);
    }
    let mut failed = false;
    for notice in pending {
        if !deliver_one_workflow(paths, store, &notice).await? {
            failed = true;
        }
    }
    Ok(if failed {
        Drain::Retry
    } else {
        Drain::Progress
    })
}

async fn deliver_one_workflow(
    paths: &Paths,
    store: &Store,
    notice: &PendingWorkflowNotification,
) -> Result<bool> {
    let token = Uuid::new_v4().to_string();
    let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let claimed =
        match store.claim_workflow_notification(&notice.run_id, notice.seq, &token, now, 20) {
            Ok(claimed) => claimed,
            Err(error) if error.is_retryable_database_contention() => return Ok(false),
            Err(error) => return Err(error.into()),
        };
    let Some(target) = claimed else {
        return Ok(true);
    };
    match try_deliver_workflow(paths, &target).await {
        Ok(()) => {
            commit_delivery(|| store.mark_workflow_notification_delivered(&target, &token)).await?;
            Ok(true)
        }
        Err(error) => {
            if let Err(release_error) = store.release_workflow_notification_claim(
                &notice.run_id,
                notice.seq,
                &token,
                &error.to_string(),
            ) && !release_error.is_retryable_database_contention()
            {
                return Err(release_error.into());
            }
            Ok(false)
        }
    }
}

async fn try_deliver_workflow(paths: &Paths, target: &WorkflowNotificationTarget) -> Result<()> {
    let counts = Store::open(&paths.store)?.workflow_step_counts(&target.run_id)?;
    let body = format!(
        "FROM BRGR\n{}",
        json!({
            "type": "brgr_workflow",
            "run_id": target.run_id.to_string(),
            "state": target.state.as_str(),
            "steps": {
                "total": counts.total,
                "finished": counts.finished,
                "failed": counts.failed,
            },
            "instruction": format!(
                "Run `brgr workflow status {}`, read and decide each sealed step result, and acknowledge a failed one. Do not treat this notification as acceptance.",
                target.run_id
            )
        })
    );
    prompt_owner(paths, &owner_pane_for_workflow(paths, target), &body).await
}

fn owner_pane_for_workflow<'a>(
    paths: &'a Paths,
    target: &'a WorkflowNotificationTarget,
) -> OwnerPane<'a> {
    OwnerPane {
        herdr_bin: &target.herdr_bin,
        herdr_session: target.herdr_session.as_deref(),
        pane_id: &target.pane_id,
        session_id: &target.session_id,
        owner_kind: owner_agent(target.owner_id.as_str()).unwrap_or("codex"),
        any_kind: Store::open(&paths.store)
            .is_ok_and(|store| is_taken_head(&store, &target.owner_id, &target.session_id)),
    }
}

async fn register_for_workflow(paths: &Paths, store: &Store, run_id: &WorkflowRunId) -> bool {
    let Ok(run) = store.workflow_run(run_id) else {
        return false;
    };
    let Ok(Some((session, _))) = store.owner_binding(&run.owner_id) else {
        return false;
    };
    if store
        .owner_surface_pane(&run.owner_id, &session)
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    register_current_surface(paths, store, &run.owner_id, &session)
        .await
        .unwrap_or(false)
}

fn notifications_enabled(paths: &Paths, task: TaskId) -> Result<bool> {
    let store = Store::open(&paths.store)?;
    if store.workflow_step_for_task(task)?.is_some() {
        return Ok(false);
    }
    let spec = store.task(task)?;
    Ok(crate::config::Config::load(&paths.config)?
        .calling(&spec.route.harness_id)
        .notifications
        != Some(false))
}

async fn open_store(paths: &Paths, started: Instant) -> Result<Store> {
    loop {
        match Store::open(&paths.store) {
            Ok(store) => return Ok(store),
            Err(error)
                if error.is_retryable_database_contention() && started.elapsed() < MAX_LIFETIME =>
            {
                sleep(POLL_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Whether the task's run is over and nothing is left to send.
fn settled(store: &Store, task: TaskId) -> Result<bool> {
    let pending = match store.pending_notifications_for_task(task) {
        Ok(pending) => pending,
        Err(error) if error.is_retryable_database_contention() => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !pending.is_empty() {
        return Ok(false);
    }
    match store.latest_result(task) {
        Ok(result)
            if result.outcome != TerminalOutcome::Failed
                || store.run_completed(result.result_id).unwrap_or(false) =>
        {
            // Result and notification commit together. Re-read after seeing
            // the result so a concurrent terminal commit cannot be missed.
            match store.pending_notifications_for_task(task) {
                Ok(pending) => Ok(pending.is_empty()),
                Err(error) if error.is_retryable_database_contention() => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
        // A failed spawn may receive a bounded automatic retry. Its first
        // result is not proof that this task's run is finished.
        Ok(_) | Err(StoreError::TaskNotFound(_)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Registers the task owner's pane when it can be proven now.
async fn register_for_task(paths: &Paths, store: &Store, task: TaskId) -> bool {
    let Ok(spec) = store.task(task) else {
        return false;
    };
    let Ok(Some((session, _))) = store.owner_binding(&spec.owner_id) else {
        return false;
    };
    // Already proven and recorded (a Claude Code owner's shell does it at
    // admission): rescanning every Codex pane would only repeat it.
    if store
        .owner_surface_pane(&spec.owner_id, &session)
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    register_current_surface(paths, store, &spec.owner_id, &session)
        .await
        .unwrap_or(false)
}

/// The owner's Codex pane, as the store recorded it for the bound session.
struct OwnerPane<'a> {
    herdr_bin: &'a str,
    herdr_session: Option<&'a str>,
    pane_id: &'a str,
    session_id: &'a str,
    owner_kind: &'a str,
    /// A head that took command by a takeover: its pane runs whatever harness it
    /// runs, not necessarily the one the owner id names.
    any_kind: bool,
}

/// The Claude Code process that registered an owner session's pane.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ClaudeOwnerProcess {
    pane: String,
    pid: u32,
    birth: String,
}

fn claude_owner_path(paths: &Paths, session_id: &str) -> Option<PathBuf> {
    (!session_id.is_empty()
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'))
    .then(|| paths.runs.join(format!("claude-owner-{session_id}.json")))
}

fn record_claude_owner(paths: &Paths, session_id: &str, owner: &ClaudeOwnerProcess) -> Result<()> {
    if let Some(path) = claude_owner_path(paths, session_id) {
        crate::write_json_atomic(&path, &serde_json::to_value(owner)?)?;
    }
    Ok(())
}

/// The nearest ancestor of this process that is Claude Code: the session
/// whose shell ran this brgr command.
fn claude_ancestor(pane: &str) -> Option<ClaudeOwnerProcess> {
    let mut pid = std::process::id();
    for _ in 0..32 {
        let parent: u32 = crate::supervision::ps_field(&pid.to_string(), "ppid")
            .ok()?
            .parse()
            .ok()?;
        if parent <= 1 {
            return None;
        }
        let command = crate::supervision::ps_field(&parent.to_string(), "comm").ok()?;
        if Path::new(&command)
            .file_name()
            .is_some_and(|name| name == "claude")
        {
            return Some(ClaudeOwnerProcess {
                pane: pane.to_owned(),
                pid: parent,
                birth: crate::supervision::ps_field(&parent.to_string(), "lstart").ok()?,
            });
        }
        pid = parent;
    }
    None
}

/// Fails when the Claude Code process that registered this owner session no
/// longer runs, or registered another pane. A session registered before this
/// record existed keeps the pane-and-kind check alone.
fn claude_owner_present(paths: &Paths, session_id: &str, pane: &str) -> Result<()> {
    let Some(bytes) = claude_owner_path(paths, session_id).and_then(|path| fs::read(path).ok())
    else {
        return Ok(());
    };
    let owner: ClaudeOwnerProcess = serde_json::from_slice(&bytes)?;
    let alive = crate::supervision::ps_field(&owner.pid.to_string(), "lstart")
        .is_ok_and(|birth| birth == owner.birth);
    if owner.pane != pane || !alive {
        bail!("the Claude Code session that owns this task no longer runs in pane {pane}");
    }
    Ok(())
}

/// The Herdr agent kind an owner runs as.
fn owner_agent(owner_id: &str) -> Option<&'static str> {
    if owner_id.starts_with("claude:") {
        Some("claude")
    } else if owner_id.starts_with("cursor:") {
        Some("cursor")
    } else if owner_id.starts_with("codex:") {
        Some("codex")
    } else {
        None
    }
}

async fn try_deliver(paths: &Paths, target: &NotificationTarget) -> Result<()> {
    let task = target.task_id;
    // A run that did not produce a candidate is a failure the owner must act
    // on, not a result to verify, so it is announced as one with its reason.
    let failure = Store::open(&paths.store)
        .ok()
        .and_then(|store| store.latest_result(task).ok())
        .filter(|result| {
            result.result_id == target.result_id
                && matches!(
                    result.outcome,
                    TerminalOutcome::Failed | TerminalOutcome::Lost
                )
        });
    let body = if let Some(result) = failure {
        format!(
            "FROM BRGR\n{}",
            json!({
                "type": "brgr_failure",
                "completion_id": target.result_id,
                "task_id": task,
                "outcome": if result.outcome == TerminalOutcome::Lost { "lost" } else { "failed" },
                "reason": result.error.as_deref().unwrap_or("no error text"),
                "instruction": format!("A brgr run failed. Inspect it with `brgr result {task}`. Retry with `brgr revise {task} \"corrected request\"`, or dismiss it with `brgr result {task} --ack`. Do not treat this notification as acceptance.")
            })
        )
    } else {
        format!(
            "FROM BRGR\n{}",
            json!({
                "type": "brgr_completion",
                "completion_id": target.result_id,
                "task_id": task,
                "instruction": "Read the sealed brgr result, verify its criteria, and decide or acknowledge it. Do not treat this notification as acceptance."
            })
        )
    };
    prompt_owner(
        paths,
        &OwnerPane {
            herdr_bin: &target.herdr_bin,
            herdr_session: target.herdr_session.as_deref(),
            pane_id: &target.pane_id,
            session_id: &target.session_id,
            owner_kind: owner_agent(target.owner_id.as_str()).unwrap_or("codex"),
            any_kind: Store::open(&paths.store)
                .is_ok_and(|store| is_taken_head(&store, &target.owner_id, &target.session_id)),
        },
        &body,
    )
    .await
}

/// Tells the owner's idle Codex pane that a worker is waiting on its answer.
///
/// Only completions used to reach the owner, so a worker's question sat until
/// the worker's own wait timed out. Best effort each poll: a busy or missing
/// pane leaves the question pending for the next one, and `brgr status --tree`
/// still shows it.
async fn deliver_questions(paths: &Paths, store: &Store, task: TaskId) {
    let Ok(pending) = store.pending_question_notices(task) else {
        return;
    };
    for target in pending {
        if deliver_question(paths, &target).await.is_ok() {
            let _ = store.record_question_notice(&target.message_id, &target.session_id);
        }
    }
}

async fn deliver_question(paths: &Paths, target: &QuestionTarget) -> Result<()> {
    let task = target.task_id;
    let message = &target.message_id;
    let body = format!(
        "FROM BRGR\n{}",
        json!({
            "type": if target.kind=="question" {"brgr_question"} else {"brgr_message"},
            "message_id": message,
            "task_id": task,
            "body": target.body,
            "instruction": format!(
                "Read this worker message or `brgr message list {task} --for owner`. For a task question reply with `brgr message send {task} --to worker --kind reply --reply-to {message} --body <answer>`. For a native input notice use `brgr input {task} --key <key>` or `--text <text>`. Acknowledge the read message with `brgr message ack {task} {message} --for owner`. Repeated message_id identifies the same message."
            )
        })
    );
    prompt_owner(
        paths,
        &OwnerPane {
            herdr_bin: &target.herdr_bin,
            herdr_session: target.herdr_session.as_deref(),
            pane_id: &target.pane_id,
            session_id: &target.session_id,
            owner_kind: owner_agent(&target.owner_id).unwrap_or("codex"),
            any_kind: OwnerId::new(target.owner_id.clone()).is_ok_and(|owner| {
                Store::open(&paths.store)
                    .is_ok_and(|store| is_taken_head(&store, &owner, &target.session_id))
            }),
        },
        &body,
    )
    .await
}

async fn prompt_owner(paths: &Paths, pane: &OwnerPane<'_>, body: &str) -> Result<()> {
    if let Some(id) = pane.session_id.strip_prefix("worker:") {
        let attempt = id.parse::<brgr_protocol::AttemptId>()?;
        let parent = Store::open(&paths.store)?
            .unfinished_attempts()?
            .into_iter()
            .find(|item| item.attempt_id == attempt)
            .context("parent worker session ended")?;
        return crate::pane_adapter::deliver_owner_notice(
            paths,
            parent.task.task_id,
            parent.task.revision,
            pane.pane_id,
            body,
        );
    }
    let binary = Path::new(pane.herdr_bin);
    if !binary.is_absolute() || !binary.is_file() {
        bail!("recorded Herdr executable is unavailable");
    }
    let mut get = herdr_command(pane);
    get.args(["agent", "get", pane.pane_id]);
    let output = timeout(HERDR_TIMEOUT, get.kill_on_drop(true).output())
        .await
        .context("Herdr agent identity lookup timed out")??;
    if !output.status.success() || output.stdout.len() > 65_536 {
        bail!("recorded parent pane is unavailable");
    }
    let state: Value = serde_json::from_slice(&output.stdout)?;
    let agent = state
        .pointer("/result/agent")
        .context("Herdr did not return an agent")?;
    let live = agent.get("agent").and_then(Value::as_str);
    let kind = if pane.any_kind {
        live.and_then(inbox_kind)
    } else {
        Some(pane.owner_kind)
    };
    if kind.is_none()
        || live != kind
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane.pane_id)
        || (kind == Some("codex")
            && agent
                .pointer("/agent_session/value")
                .and_then(Value::as_str)
                != Some(pane.session_id))
    {
        bail!("recorded parent agent identity changed");
    }
    if kind == Some("claude") {
        claude_owner_present(paths, pane.session_id, pane.pane_id)?;
    }
    if !matches!(
        agent.get("agent_status").and_then(Value::as_str),
        Some("idle" | "done")
    ) {
        bail!("parent agent is not idle");
    }
    let mut prompt = herdr_command(pane);
    prompt.args(["agent", "prompt", pane.pane_id, body]);
    let output = timeout(HERDR_TIMEOUT, prompt.kill_on_drop(true).output())
        .await
        .context("Herdr parent prompt timed out")??;
    if !output.status.success() {
        bail!("Herdr did not accept the parent prompt");
    }
    Ok(())
}

fn herdr_command(pane: &OwnerPane<'_>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(pane.herdr_bin);
    if let Some(session) = pane.herdr_session {
        command.arg("--session").arg(session);
    }
    command
}

#[cfg(test)]
mod claude_owner_tests {
    use super::*;

    #[test]
    fn a_claude_owner_is_present_only_while_its_process_runs_in_its_pane() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("brgr"))).unwrap();
        fs::create_dir_all(&paths.runs).unwrap();
        let session = "6b0e2a7c-1f7e-4c55-9d8f-0a1b2c3d4e5f";
        // Nothing recorded: the pane-and-kind check alone applies.
        claude_owner_present(&paths, session, "w1:p1").unwrap();
        let me = std::process::id();
        let birth = crate::supervision::ps_field(&me.to_string(), "lstart").unwrap();
        let owner = ClaudeOwnerProcess {
            pane: "w1:p1".into(),
            pid: me,
            birth,
        };
        record_claude_owner(&paths, session, &owner).unwrap();
        claude_owner_present(&paths, session, "w1:p1").unwrap();
        assert!(claude_owner_present(&paths, session, "w1:p2").is_err());
        record_claude_owner(
            &paths,
            session,
            &ClaudeOwnerProcess {
                birth: "Thu Jan  1 00:00:00 1970".into(),
                ..owner
            },
        )
        .unwrap();
        assert!(claude_owner_present(&paths, session, "w1:p1").is_err());
        assert!(claude_owner_path(&paths, "../escape").is_none());
    }

    #[test]
    fn owner_agent_kind_matches_the_owner_prefix() {
        assert_eq!(owner_agent("claude:abc"), Some("claude"));
        assert_eq!(owner_agent("cursor:abc"), Some("cursor"));
        assert_eq!(owner_agent("codex:abc"), Some("codex"));
        assert_eq!(owner_agent("worker:1"), None);
    }
}
