use std::{
    env,
    fs::{OpenOptions, TryLockError},
    os::unix::fs::OpenOptionsExt as _,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{OwnerId, TaskId, TaskSpec, TerminalOutcome};
use brgr_store::{NotificationTarget, Store, StoreError};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{Paths, plugin_bridge};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_LIFETIME: Duration = Duration::from_hours(24);
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn register_current_surface(
    store: &Store,
    owner_id: &OwnerId,
    session_id: &str,
) -> Result<bool> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") || !owner_id.as_str().starts_with("codex:") {
        return Ok(false);
    }
    let (Some(pane), Some(binary)) = (env::var_os("HERDR_PANE_ID"), env::var_os("HERDR_BIN_PATH"))
    else {
        return Ok(false);
    };
    let pane = pane.to_string_lossy();
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
    if agent.get("agent").and_then(Value::as_str) != Some("codex")
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane.as_ref())
    {
        bail!("recorded Herdr pane is not the current Codex agent");
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

pub fn spawn_registration_and_delivery(paths: &Paths, task: &TaskSpec, session: Option<&str>) {
    if session.is_none()
        || env::var("HERDR_ENV").as_deref() != Ok("1")
        || !task.owner_id.as_str().starts_with("codex:")
        || env::var_os("HERDR_PANE_ID").is_none()
        || env::var_os("HERDR_BIN_PATH").is_none()
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
    let mut command = Command::new(env::current_exe()?);
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
        .stderr(Stdio::null());
    command.process_group(0);
    command
        .spawn()
        .context("brgr notification dispatcher could not start")?;
    Ok(())
}

pub async fn deliver_pending(paths: &Paths, task: TaskId) -> Result<()> {
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
    let store = registered_store(paths, task, started).await?;
    while started.elapsed() < MAX_LIFETIME {
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
            match try_deliver(&target).await {
                Ok(()) => {
                    // The prompt was accepted. Keep the lease while retrying its receipt,
                    // so transient database contention does not trigger another prompt.
                    let receipt_started = Instant::now();
                    loop {
                        match store.mark_notification_delivered(&target, &token) {
                            Ok(()) | Err(StoreError::NotificationClaimStale) => break,
                            Err(error)
                                if error.is_retryable_database_contention()
                                    && receipt_started.elapsed() < Duration::from_secs(15) =>
                            {
                                sleep(POLL_INTERVAL).await;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                Err(error) => {
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
        sleep(POLL_INTERVAL).await;
    }
    Ok(())
}

async fn registered_store(paths: &Paths, task: TaskId, started: Instant) -> Result<Store> {
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
    let spec = store.task(task)?;
    if let Some((session, _)) = store.owner_binding(&spec.owner_id)? {
        register_current_surface(&store, &spec.owner_id, &session).await?;
    }
    Ok(store)
}

async fn try_deliver(target: &NotificationTarget) -> Result<()> {
    let binary = Path::new(&target.herdr_bin);
    if !binary.is_absolute() || !binary.is_file() {
        bail!("recorded Herdr executable is unavailable");
    }
    let mut get = herdr_command(target);
    get.args(["agent", "get", &target.pane_id]);
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
    if agent.get("agent").and_then(Value::as_str) != Some("codex")
        || agent.get("pane_id").and_then(Value::as_str) != Some(target.pane_id.as_str())
        || agent
            .pointer("/agent_session/value")
            .and_then(Value::as_str)
            != Some(target.session_id.as_str())
    {
        bail!("recorded parent agent identity changed");
    }
    if !matches!(
        agent.get("agent_status").and_then(Value::as_str),
        Some("idle" | "done")
    ) {
        bail!("parent agent is not idle");
    }
    let body = format!(
        "FROM BRGR\n{}",
        json!({
            "type": "brgr_completion",
            "completion_id": target.result_id,
            "task_id": target.task_id,
            "instruction": "Read the sealed brgr result, verify its criteria, and decide or acknowledge it. Do not treat this notification as acceptance."
        })
    );
    let mut prompt = herdr_command(target);
    prompt.args(["agent", "prompt", &target.pane_id, &body]);
    let output = timeout(HERDR_TIMEOUT, prompt.kill_on_drop(true).output())
        .await
        .context("Herdr parent prompt timed out")??;
    if !output.status.success() {
        bail!("Herdr did not accept the parent completion prompt");
    }
    Ok(())
}

fn herdr_command(target: &NotificationTarget) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&target.herdr_bin);
    if let Some(session) = &target.herdr_session {
        command.arg("--session").arg(session);
    }
    command
}
