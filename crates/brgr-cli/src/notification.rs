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
use brgr_store::{NotificationTarget, QuestionTarget, Store, StoreError};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{Paths, plugin_bridge};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// The longest wait between delivery attempts while the owner stays busy.
const MAX_BACKOFF: Duration = Duration::from_secs(3);
const MAX_LIFETIME: Duration = Duration::from_hours(24);
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn register_current_surface(
    paths: &Paths,
    store: &Store,
    owner_id: &OwnerId,
    session_id: &str,
) -> Result<bool> {
    if owner_id.as_str().starts_with("worker:") {
        return register_worker_surface(paths, store, owner_id, session_id);
    }
    let claude = owner_id.as_str().starts_with("claude:");
    if !claude && !owner_id.as_str().starts_with("codex:") {
        return Ok(false);
    }
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
    if agent.get("agent").and_then(Value::as_str) != Some(owner_agent(claude))
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane.as_ref())
    {
        bail!("recorded Herdr pane is not the current owner agent");
    }
    // Herdr keeps no session id for a Claude Code agent, so the pane and agent
    // kind are the identity; Codex is also bound to its exact session.
    if claude {
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
    match agent
        .pointer("/agent_session/value")
        .and_then(Value::as_str)
    {
        Some(observed) if observed == session_id => {}
        Some(_) => bail!("recorded Herdr Codex session differs from the owner binding"),
        None => {
            let mut report = surface_command(binary, herdr_session.as_deref());
            report.args([
                "pane",
                "report-agent-session",
                &pane,
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
            let reported = recorded_agent(binary, herdr_session.as_deref(), &pane).await?;
            if reported.get("agent").and_then(Value::as_str) != Some("codex")
                || reported.get("pane_id").and_then(Value::as_str) != Some(pane.as_ref())
                || reported
                    .pointer("/agent_session/value")
                    .and_then(Value::as_str)
                    != Some(session_id)
            {
                bail!("Herdr did not bind the exact Codex session to the pane");
            }
        }
    }
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
    let mut command = Command::new(env::current_exe()?);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.runs.join(format!("{task}.notify.log")))?;
    if let Ok(store) = Store::open(&paths.store)
        && let Ok(spec) = store.task(task)
        && let Ok(Some((session, _))) = store.owner_binding(&spec.owner_id)
    {
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
    command
        .arg("--home")
        .arg(&paths.home)
        .arg("__notify")
        .arg(task.to_string())
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
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.runs.join(format!("{task}.notify.lock")))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    // Keep this descriptor alive throughout delivery. The OS releases the
    // lock after a crash, so a later hook may resume the durable queue.
    let _lock = lock;
    let started = Instant::now();
    let (store, mut registered) = registered_store(paths, task, started).await?;
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
            match store.latest_result(task) {
                Ok(result)
                    if result.outcome != TerminalOutcome::Failed
                        || store.run_completed(result.result_id).unwrap_or(false) =>
                {
                    // Result and notification commit together. Re-read after seeing
                    // the result so a concurrent terminal commit cannot be missed.
                    match store.pending_notifications_for_task(task) {
                        Ok(pending) if pending.is_empty() => return Ok(()),
                        Ok(_) => {}
                        Err(error) if error.is_retryable_database_contention() => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                // A failed spawn may receive a bounded automatic retry. Its
                // first result is not proof that this task's run is finished.
                Ok(_) | Err(StoreError::TaskNotFound(_)) => {}
                Err(error) => return Err(error.into()),
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
                Ok(()) => mark_delivered(&store, &target, &token).await?,
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
async fn mark_delivered(store: &Store, target: &NotificationTarget, token: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        match store.mark_notification_delivered(target, token) {
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

fn notifications_enabled(paths: &Paths, task: TaskId) -> Result<bool> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    Ok(crate::config::Config::load(&paths.config)?
        .calling(&spec.route.harness_id)
        .notifications
        != Some(false))
}

async fn registered_store(paths: &Paths, task: TaskId, started: Instant) -> Result<(Store, bool)> {
    let store = loop {
        match Store::open(&paths.store) {
            Ok(store) => break store,
            Err(error)
                if error.is_retryable_database_contention() && started.elapsed() < MAX_LIFETIME =>
            {
                sleep(POLL_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let registered = register_for_task(paths, &store, task).await;
    Ok((store, registered))
}

/// Registers the task owner's pane when it can be proven now.
async fn register_for_task(paths: &Paths, store: &Store, task: TaskId) -> bool {
    let Ok(spec) = store.task(task) else {
        return false;
    };
    let Ok(Some((session, _))) = store.owner_binding(&spec.owner_id) else {
        return false;
    };
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
    owner_is_claude: bool,
}

/// The Herdr agent kind an owner runs as.
fn owner_agent(claude: bool) -> &'static str {
    if claude { "claude" } else { "codex" }
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
            owner_is_claude: target.owner_id.as_str().starts_with("claude:"),
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
            owner_is_claude: target.owner_id.starts_with("claude:"),
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
    let claude = pane.owner_is_claude;
    if agent.get("agent").and_then(Value::as_str) != Some(owner_agent(claude))
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane.pane_id)
        || (!claude
            && agent
                .pointer("/agent_session/value")
                .and_then(Value::as_str)
                != Some(pane.session_id))
    {
        bail!("recorded parent agent identity changed");
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
