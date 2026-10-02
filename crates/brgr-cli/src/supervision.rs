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
use crate::{
    Claimant, LaunchEnvelope, Paths, ProcessReceipt, print_value, workspace, write_json_atomic,
};
use anyhow::{Context, Result, bail};
use brgr_core::{ExecutionObservation, Supervisor};
use brgr_protocol::{
    AttemptId, AttemptState, ResultEnvelope, ResultId, SCHEMA_V1, TaskId, TaskSpec, TerminalOutcome,
};
use brgr_registry::Registry;
use brgr_runner::HarnessManifest;
use brgr_store::{RunnerIdentity, Store, StoreError, UnfinishedAttempt};
use serde::Deserialize;

pub(crate) async fn supervise(paths: &Paths, launch_path: &Path, json_output: bool) -> Result<()> {
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(launch_path)?)?;
    if launch.protocol_generation != "brgr-v1" {
        bail!("unsupported task protocol generation");
    }
    let mut manifest = pinned_manifest_for_launch(
        launch.manifest.as_ref(),
        launch.executable_digest.as_deref(),
        &launch.harness_id,
    )?;
    if !launch.pane_mode
        && let Some(options) = &launch.calling_options
    {
        manifest.launch.argv.extend(options.argv.clone());
    }
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
    } else if launch.pane_mode {
        crate::pane_adapter::pane_process_manifest(paths, &launch, &manifest)?
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
    record_recovered(
        paths,
        &supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?,
    );
    write_json_atomic(&paths.supervisor(launch.spec.task_id), &receipt)?;
    let (task_id, revision) = (launch.spec.task_id, launch.spec.revision);
    let harness = launch.spec.route.harness_id.clone();
    let result = supervisor
        .run_fresh_controlled(
            launch.spec,
            &manifest,
            Some(&cancel_path),
            Some(&pid_path),
            Some(&receipt.identity),
        )
        .await;
    if launch.pane_mode
        && !result
            .as_ref()
            .is_ok_and(|value| value.outcome == TerminalOutcome::Candidate)
    {
        crate::pane_adapter::close_leftover_pane(paths, task_id, revision, launch.keep_pane);
    }
    let result = result?;
    crate::error_memo::record_result(paths, &harness, &result);
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

/// Failures settled by recovery after a crashed supervisor go to the memo too.
fn record_recovered(paths: &Paths, results: &[ResultEnvelope]) {
    let Ok(store) = Store::open(&paths.store) else {
        return;
    };
    for result in results {
        let harness = store
            .task(result.task_id)
            .map_or_else(|_| "unknown".to_owned(), |spec| spec.route.harness_id);
        crate::error_memo::record_result(paths, &harness, result);
    }
}

pub(crate) fn reconcile_pending(paths: &Paths) -> Result<()> {
    let mut supervisor = Supervisor::open(&paths.store)?;
    for attempt in supervisor.store().unfinished_attempts()? {
        if !matches!(
            observe_attempt(paths, &attempt),
            ExecutionObservation::SupervisorAlive(_)
        ) {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(
                    paths
                        .runs
                        .join(format!("{}.recover.lock", attempt.task.task_id)),
                )?;
            if lock.try_lock().is_ok() && !crate::pane_adapter::stop_external(paths, &attempt)? {
                crate::pane_adapter::collect_recovered(paths, &attempt, &mut supervisor)?;
            }
        }
    }
    record_recovered(
        paths,
        &supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?,
    );
    let mut store = Store::open(&paths.store)?;
    for task in store.unstarted_tasks()? {
        if unstarted_admission_is_stale(paths, &task)? {
            let cancelled = paths.cancel(task.task_id).exists()
                || store.cancellation_requested(task.task_id)?;
            record_unstarted_terminal(
                paths,
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
    for entry in fs::read_dir(&paths.runs)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".pane.json") {
            continue;
        }
        if let Some(id) = name.strip_suffix(".pane.json")
            && let Ok(task) = id.parse::<TaskId>()
        {
            if let Err(error) = crate::pane_cleanup::close_if_eligible(&store, &paths.runs, task) {
                eprintln!("brgr legacy pane cleanup pending: {error}");
            }
            continue;
        }
        if let Some((id, suffix)) = name.split_once("-r")
            && let (Ok(task), Ok(revision)) = (
                id.parse::<TaskId>(),
                suffix.trim_end_matches(".pane.json").parse::<u32>(),
            )
            && let Err(error) = crate::pane_adapter::cleanup_settled(paths, task, revision)
        {
            eprintln!("brgr cleanup pending: {error}");
        }
    }
    Ok(())
}

/// How long a detached supervisor has to claim a task it was spawned for.
const SUPERVISOR_CLAIM_GRACE: Duration = Duration::from_secs(5);
/// How long a Herdr worker pane has to claim its task. Herdr must open the pane
/// and start `brgr plugin worker` in it first, which a busy machine can stretch
/// well past the detached supervisor's window; a task reaped before then is
/// recorded lost while its worker is still on the way.
const WORKER_PANE_CLAIM_GRACE: Duration = Duration::from_mins(1);

/// The claim window for a launch envelope. An unreadable envelope gets the
/// short window, as before.
fn claim_grace(launch: &[u8]) -> Duration {
    #[derive(Deserialize)]
    struct Placement {
        #[serde(default)]
        claimant: Claimant,
    }
    match serde_json::from_slice::<Placement>(launch) {
        Ok(Placement {
            claimant: Claimant::WorkerPane,
        }) => WORKER_PANE_CLAIM_GRACE,
        _ => SUPERVISOR_CLAIM_GRACE,
    }
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
    let grace = fs::read(&launch_path).map_or(SUPERVISOR_CLAIM_GRACE, |bytes| claim_grace(&bytes));
    if !metadata.modified()?.elapsed().is_ok_and(|age| age >= grace) {
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
    paths: &Paths,
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
    crate::error_memo::record_result(paths, &task.route.harness_id, &result);
    Ok(true)
}

pub(crate) fn observe_attempt(paths: &Paths, attempt: &UnfinishedAttempt) -> ExecutionObservation {
    let receipt: ProcessReceipt = match fs::read(paths.supervisor(attempt.task.task_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(receipt) => receipt,
        None => return external_observation(paths, attempt),
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
        Err(_) => external_observation(paths, attempt),
    }
}

fn external_observation(paths: &Paths, attempt: &UnfinishedAttempt) -> ExecutionObservation {
    if crate::pane_adapter::external_alive(paths, attempt) {
        ExecutionObservation::ExternalAlive(RunnerIdentity {
            namespace: "brgr.native-pane".to_owned(),
            handle: attempt.task.task_id.to_string(),
            birth_marker: attempt.attempt_id.to_string(),
        })
    } else {
        ExecutionObservation::Unknown
    }
}

/// Whether anything of this task may still be running: its supervisor, or the
/// harness process group the supervisor started. The runner removes the pid
/// file once the harness exits, so a file left behind means the supervisor
/// died first. Anything unreadable counts as running.
pub(crate) fn worker_may_be_running(paths: &Paths, task: TaskId) -> bool {
    let receipt = fs::read(paths.supervisor(task))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ProcessReceipt>(&bytes).ok());
    if let Some(receipt) = receipt
        && let Ok(pid) = receipt.identity.handle.parse::<u32>()
        && process_identity(pid).is_ok_and(|actual| actual == receipt.identity)
    {
        return true;
    }
    match fs::read_to_string(paths.pid(task)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(pid) if pid > 1 => ProcessCommand::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            _ => true,
        },
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

#[cfg(test)]
mod claim_grace_tests {
    use super::*;

    #[test]
    fn a_worker_pane_launch_gets_the_long_claim_window() {
        assert_eq!(
            claim_grace(br#"{"claimant":"worker_pane","spec":{}}"#),
            WORKER_PANE_CLAIM_GRACE
        );
        assert_eq!(
            claim_grace(br#"{"claimant":"supervisor"}"#),
            SUPERVISOR_CLAIM_GRACE
        );
        // Envelopes written before the field existed, and unreadable ones,
        // keep the short window they always had.
        assert_eq!(
            claim_grace(br#"{"pane_mode":false}"#),
            SUPERVISOR_CLAIM_GRACE
        );
        assert_eq!(claim_grace(b"not json"), SUPERVISOR_CLAIM_GRACE);
    }
}
