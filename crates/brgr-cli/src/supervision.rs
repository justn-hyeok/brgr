//! The detached supervisor and recovery of runs it never started.

use std::{
    env,
    fs::{self, OpenOptions},
    io::{self},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use crate::omp_adapter::omp_process_manifest;
use crate::{LaunchEnvelope, Paths, ProcessReceipt, print_value, workspace, write_json_atomic};
use anyhow::{Context, Result, bail};
use brgr_core::{ExecutionObservation, Supervisor};
use brgr_protocol::{
    AttemptId, AttemptState, ResultEnvelope, ResultId, SCHEMA_V1, TaskSpec, TerminalOutcome,
};
use brgr_registry::Registry;
use brgr_runner::HarnessManifest;
use brgr_store::{RunnerIdentity, Store, StoreError, UnfinishedAttempt};

pub(crate) async fn supervise(paths: &Paths, launch_path: &Path, json_output: bool) -> Result<()> {
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(launch_path)?)?;
    if launch.protocol_generation != "brgr-v1" {
        bail!("unsupported task protocol generation");
    }
    let manifest = pinned_manifest_for_launch(
        launch.manifest.as_ref(),
        launch.executable_digest.as_deref(),
        &launch.harness_id,
    )?;
    manifest.validate_task_route(&launch.spec)?;
    #[cfg(debug_assertions)]
    if env::var_os("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM").is_some() {
        std::process::exit(79);
    }
    let receipt = ProcessReceipt {
        task_id: launch.spec.task_id,
        launch_path: launch_path.to_path_buf(),
        identity: process_identity(std::process::id())?,
    };
    let manifest = if manifest.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1 {
        omp_process_manifest(paths, &launch, &manifest)?
    } else {
        manifest
    };
    let cancel_path = paths.cancel(launch.spec.task_id);
    let pid_path = paths.pid(launch.spec.task_id);
    let mut supervisor = Supervisor::open(&paths.store)?;
    // Every worker gets its identity so it can ask its owner; only a task
    // started with delegation may also start children (checked at admission).
    supervisor.enable_worker_context(
        paths.home.clone(),
        env::current_exe()?,
        launch.delegation_enabled,
    );
    // Reconcile before publishing our own task receipt. Otherwise a previous
    // crashed attempt without a launch identity could mistake this new process
    // for its original live supervisor and remain unfinished forever.
    supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?;
    write_json_atomic(&paths.supervisor(launch.spec.task_id), &receipt)?;
    let result = supervisor
        .run_fresh_controlled(
            launch.spec,
            &manifest,
            Some(&cancel_path),
            Some(&pid_path),
            Some(&receipt.identity),
        )
        .await?;
    let _ = fs::remove_file(cancel_path);
    let _ = fs::remove_file(paths.supervisor(result.task_id));
    print_value(&serde_json::to_value(result)?, json_output);
    Ok(())
}

pub(crate) fn pinned_manifest_for_launch(
    manifest: Option<&HarnessManifest>,
    digest: Option<&str>,
    harness_id: &str,
) -> Result<HarnessManifest> {
    let (Some(manifest), Some(digest)) = (manifest, digest) else {
        bail!("launch lacks a complete pinned manifest; unsafe replay is disabled");
    };
    if manifest.id != harness_id {
        bail!("task-pinned harness id differs from its manifest");
    }
    Registry::verify_pinned_executable(manifest, digest)?;
    Ok(manifest.clone())
}

pub(crate) fn spawn_supervisor(paths: &Paths, launch_path: &Path) -> Result<()> {
    let executable = env::current_exe()?;
    let log_path = paths.runs.join("supervisor.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    stdout.set_permissions(fs::Permissions::from_mode(0o600))?;
    let stderr = stdout.try_clone()?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--home")
        .arg(&paths.home)
        .arg("--json")
        .arg("__supervise")
        .arg(launch_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0);
    command
        .spawn()
        .context("failed to start detached supervisor")?;
    Ok(())
}

pub(crate) fn reconcile_pending(paths: &Paths) -> Result<()> {
    let mut supervisor = Supervisor::open(&paths.store)?;
    supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?;
    let mut store = Store::open(&paths.store)?;
    for task in store.unstarted_tasks()? {
        if unstarted_admission_is_stale(paths, &task)? {
            let cancelled = paths.cancel(task.task_id).exists()
                || store.cancellation_requested(task.task_id)?;
            record_unstarted_terminal(
                &mut store,
                &task,
                if cancelled {
                    TerminalOutcome::Cancelled
                } else {
                    TerminalOutcome::Lost
                },
                if cancelled {
                    "cancelled before the supervisor claimed the task".to_owned()
                } else if !workspace::workspace_is_present(&task.workspace) {
                    "task worktree is missing; refusing to recreate or retry automatically"
                        .to_owned()
                } else {
                    "supervisor did not claim the admitted task".to_owned()
                },
            )?;
        }
    }
    Ok(())
}

pub(crate) fn unstarted_admission_is_stale(paths: &Paths, task: &TaskSpec) -> Result<bool> {
    if !workspace::workspace_is_present(&task.workspace) {
        return Ok(true);
    }
    let launch_path = paths.launch(task.task_id, task.revision);
    let metadata = match fs::symlink_metadata(&launch_path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !metadata
        .modified()?
        .elapsed()
        .is_ok_and(|age| age >= Duration::from_secs(5))
    {
        return Ok(false);
    }
    let receipt = fs::read(paths.supervisor(task.task_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ProcessReceipt>(&bytes).ok());
    let Some(receipt) =
        receipt.filter(|value| value.task_id == task.task_id && value.launch_path == launch_path)
    else {
        return Ok(true);
    };
    let live = receipt
        .identity
        .handle
        .parse::<u32>()
        .ok()
        .and_then(|pid| process_identity(pid).ok())
        .is_some_and(|actual| actual == receipt.identity);
    Ok(!live)
}

pub(crate) fn record_unstarted_terminal(
    store: &mut Store,
    task: &TaskSpec,
    outcome: TerminalOutcome,
    reason: String,
) -> Result<bool> {
    let attempt_id = AttemptId::new();
    match store.claim_attempt(task.task_id, task.revision, attempt_id) {
        Ok(()) => {}
        Err(
            StoreError::ActiveAttemptExists { .. }
            | StoreError::UnresolvedPriorAttempt { .. }
            | StoreError::NonRetryablePriorAttempt { .. }
            | StoreError::AttemptBudgetExhausted { .. },
        ) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let next = if outcome == TerminalOutcome::Cancelled {
        AttemptState::CancelRequested
    } else {
        AttemptState::Starting
    };
    store.compare_and_set_attempt_state(attempt_id, AttemptState::Queued, next)?;
    let result = ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome,
        artifacts: vec![],
        error: Some(reason),
        legacy_embedded_route_observation: None,
        route_observation: Some(brgr_protocol::RouteObservation::unavailable()),
        unresolved_effects: if outcome == TerminalOutcome::Lost {
            vec!["execution identity was not established".to_owned()]
        } else {
            vec![]
        },
    };
    store.commit_terminal_result_final(&task.owner_id, &result)?;
    Ok(true)
}

pub(crate) fn observe_attempt(paths: &Paths, attempt: &UnfinishedAttempt) -> ExecutionObservation {
    let receipt: ProcessReceipt = match fs::read(paths.supervisor(attempt.task.task_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(receipt) => receipt,
        None => return ExecutionObservation::Unknown,
    };
    if receipt.task_id != attempt.task.task_id
        || receipt.launch_path != paths.launch(attempt.task.task_id, attempt.task.revision)
    {
        return ExecutionObservation::Unknown;
    }
    let Ok(pid) = receipt.identity.handle.parse::<u32>() else {
        return ExecutionObservation::Unknown;
    };
    match process_identity(pid) {
        Ok(actual) if actual == receipt.identity => ExecutionObservation::SupervisorAlive(actual),
        Ok(_) => ExecutionObservation::NotObserved,
        Err(_) => ExecutionObservation::Unknown,
    }
}

pub(crate) fn process_identity(pid: u32) -> Result<RunnerIdentity> {
    let pid_text = pid.to_string();
    let start = ps_field(&pid_text, "lstart")?;
    let executable = ps_field(&pid_text, "comm")?;
    if Path::new(&executable)
        .file_name()
        .is_none_or(|name| name != "brgr")
    {
        bail!("process {pid} is not brgr");
    }
    Ok(RunnerIdentity {
        namespace: "brgr.supervisor".to_owned(),
        handle: pid_text,
        birth_marker: start,
    })
}

pub(crate) fn ps_field(pid: &str, field: &str) -> Result<String> {
    let output = ProcessCommand::new("/bin/ps")
        .args(["-ww", "-p", pid, "-o"])
        .arg(format!("{field}="))
        .output()?;
    if !output.status.success() {
        bail!("process {pid} is not observable");
    }
    let text = String::from_utf8(output.stdout)?.trim().to_owned();
    if text.is_empty() {
        bail!("process {pid} has no {field} identity");
    }
    Ok(text)
}
