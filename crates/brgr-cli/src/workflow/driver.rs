//! Detached node driver for a workflow script, plus resume and cancel.

use std::{
    env,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{Read, Write as IoWrite},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::TaskId;
use brgr_store::{Store, WorkflowRun, WorkflowRunId, WorkflowRunState};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::create_owned_run;
use crate::task_commands::{CancelOutcome, cancel_task, task_needs_cancel};
use crate::{Paths, current_session, owner_from_environment, print_value, require_owner};

const LOG_LIMIT: u64 = 4 * 1024 * 1024;
const ALLOWED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "BRGR_HOME",
    "BRGR_OWNER_ID",
    "BRGR_SESSION_ID",
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CURSOR_CONVERSATION_ID",
];

struct PidGuard {
    path: PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct BoundedLog {
    file: File,
    written: u64,
}

impl BoundedLog {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let written = file.metadata()?.len();
        Ok(Self { file, written })
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.written >= LOG_LIMIT || bytes.is_empty() {
            return;
        }
        let remain = usize::try_from(LOG_LIMIT - self.written).unwrap_or(0);
        let take = bytes.len().min(remain);
        if IoWrite::write_all(&mut self.file, &bytes[..take]).is_ok() {
            self.written += u64::try_from(take).unwrap_or(0);
        }
    }
}

pub(super) fn run_script(
    paths: &Paths,
    script: &Path,
    max_parallel: u32,
    json_output: bool,
) -> Result<()> {
    let script = require_script(script)?;
    let digest = file_digest(&script)?;
    let run = create_owned_run(paths, max_parallel, &digest)?;
    write_private(
        &script_record_path(paths, &run.run_id),
        script.to_string_lossy().as_bytes(),
    )?;
    spawn_driver(paths, &run.run_id, &script)?;
    print_launch_receipt(paths, &run, json_output);
    Ok(())
}

pub(super) fn resume(paths: &Paths, run: &str, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let run_id = WorkflowRunId::new(run)?;
    let current = store.workflow_run(&run_id)?;
    require_owner(&store, &current.owner_id)?;
    if current.state.is_terminal() {
        bail!(
            "workflow run {run_id} is {}; cannot resume",
            current.state.as_str()
        );
    }
    if live_driver_pid(paths, &run_id).is_some() {
        print_value(
            &json!({
                "run_id": current.run_id.to_string(),
                "owner": current.owner_id.as_str(),
                "state": current.state.as_str(),
                "driver": "already running",
            }),
            json_output,
        );
        return Ok(());
    }
    if current.state == WorkflowRunState::Blocked {
        store.set_workflow_run_state(&run_id, WorkflowRunState::Running)?;
    }
    let script = recorded_script(paths, &run_id)?;
    spawn_driver(paths, &run_id, &script)?;
    let run = store.workflow_run(&run_id)?;
    print_launch_receipt(paths, &run, json_output);
    Ok(())
}

pub(super) fn cancel_run(paths: &Paths, run: &str, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let run_id = WorkflowRunId::new(run)?;
    let current = store.workflow_run(&run_id)?;
    require_owner(&store, &current.owner_id)?;
    let previous = current.state;
    let run = mark_cancelled(&store, &run_id, current)?;
    crate::notification::spawn_for_workflow_transition(paths, previous, &run);
    let driver_stopped = stop_live_driver(paths, &run_id);
    let cancelled_tasks = cancel_unfinished_steps(paths, &store, &run_id)?;
    print_value(
        &json!({
            "run_id": run.run_id.to_string(),
            "owner": run.owner_id.as_str(),
            "state": run.state.as_str(),
            "driver_stopped": driver_stopped,
            "cancelled_tasks": cancelled_tasks,
        }),
        json_output,
    );
    Ok(())
}

pub(super) fn drive(paths: &Paths, run: &str, script: &Path) -> Result<()> {
    let run_id = WorkflowRunId::new(run)?;
    let pid_path = pid_path(paths, &run_id);
    write_private(&pid_path, format!("{}\n", std::process::id()).as_bytes())?;
    let _guard = PidGuard { path: pid_path };
    let succeeded = run_node(paths, &run_id, script).is_ok_and(|status| status.success());
    settle_driver_exit(paths, &run_id, succeeded)
}

fn mark_cancelled(
    store: &Store,
    run_id: &WorkflowRunId,
    current: WorkflowRun,
) -> Result<WorkflowRun> {
    match current.state {
        WorkflowRunState::Cancelled => Ok(current),
        state if state.is_terminal() => {
            bail!("workflow run {run_id} is {}", state.as_str())
        }
        _ => Ok(store.set_workflow_run_state(run_id, WorkflowRunState::Cancelled)?),
    }
}

fn stop_live_driver(paths: &Paths, run_id: &WorkflowRunId) -> bool {
    let Some(pid) = live_driver_pid(paths, run_id) else {
        return false;
    };
    stop_driver_group(pid);
    true
}

fn cancel_unfinished_steps(
    paths: &Paths,
    store: &Store,
    run_id: &WorkflowRunId,
) -> Result<Vec<TaskId>> {
    let mut cancelled = Vec::new();
    for step in store.workflow_steps(run_id)? {
        if !task_needs_cancel(store, step.task_id)? {
            continue;
        }
        match cancel_task(paths, step.task_id, false, false)? {
            CancelOutcome::Requested { requested } => cancelled.extend(requested),
            CancelOutcome::AlreadyTerminal | CancelOutcome::ForceStopped { .. } => {}
        }
    }
    Ok(cancelled)
}

fn spawn_driver(paths: &Paths, run_id: &WorkflowRunId, script: &Path) -> Result<()> {
    let executable = env::current_exe()?;
    let mut command = ProcessCommand::new(&executable);
    command
        .arg("--home")
        .arg(&paths.home)
        .arg("workflow")
        .arg("__drive")
        .arg(run_id.as_str())
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    apply_driver_env(&mut command, paths, run_id)?;
    command
        .spawn()
        .context("failed to start detached workflow driver")?;
    Ok(())
}

fn run_node(paths: &Paths, run_id: &WorkflowRunId, script: &Path) -> Result<ExitStatus> {
    let log = Arc::new(Mutex::new(BoundedLog::open(&log_path(paths, run_id))?));
    let mut command = ProcessCommand::new("node");
    command
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_driver_env(&mut command, paths, run_id)?;
    let mut child = command.spawn().context("failed to start node")?;
    let stdout = spawn_copy(child.stdout.take(), Arc::clone(&log));
    let stderr = spawn_copy(child.stderr.take(), Arc::clone(&log));
    let status = child.wait()?;
    let _ = stdout.join();
    let _ = stderr.join();
    Ok(status)
}

fn spawn_copy<R: Read + Send + 'static>(
    stream: Option<R>,
    log: Arc<Mutex<BoundedLog>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Some(mut stream) = stream else {
            return;
        };
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut log) = log.lock() {
                        log.write(&buf[..n]);
                    }
                }
            }
        }
    })
}

fn settle_driver_exit(paths: &Paths, run_id: &WorkflowRunId, succeeded: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let run = store.workflow_run(run_id)?;
    if run.state != WorkflowRunState::Running {
        return Ok(());
    }
    let previous = run.state;
    let next = if succeeded {
        WorkflowRunState::Completed
    } else {
        WorkflowRunState::Failed
    };
    let run = store.set_workflow_run_state(run_id, next)?;
    crate::notification::spawn_for_workflow_transition(paths, previous, &run);
    Ok(())
}

fn apply_driver_env(
    command: &mut ProcessCommand,
    paths: &Paths,
    run_id: &WorkflowRunId,
) -> Result<()> {
    let owner = owner_from_environment().ok();
    let session = current_session().ok().flatten();
    let copied: Vec<(OsString, OsString)> = env::vars_os()
        .filter(|(name, _)| env_allowed(name))
        .collect();
    command.env_clear();
    for (name, value) in copied {
        command.env(name, value);
    }
    command.env("BRGR_HOME", &paths.home);
    command.env("BRGR_WORKFLOW_RUN", run_id.as_str());
    command.env("BRGR_BIN", env::current_exe()?);
    if let Some(owner) = owner {
        command.env("BRGR_OWNER_ID", owner.as_str());
    }
    if let Some(session) = session {
        command.env("BRGR_SESSION_ID", session);
    }
    Ok(())
}

fn env_allowed(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    ALLOWED_ENV.contains(&name) || name.starts_with("HERDR_")
}

fn live_driver_pid(paths: &Paths, run_id: &WorkflowRunId) -> Option<u32> {
    let text = fs::read_to_string(pid_path(paths, run_id)).ok()?;
    let pid = text.trim().parse::<u32>().ok()?;
    if pid_is_alive(pid) && command_line_contains(pid, "workflow __drive") {
        Some(pid)
    } else {
        None
    }
}

fn pid_is_alive(pid: u32) -> bool {
    ProcessCommand::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn command_line_contains(pid: u32, needle: &str) -> bool {
    let output = ProcessCommand::new("ps")
        .args(["-p", &pid.to_string(), "-www", "-o", "command="])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains(needle))
}

fn stop_driver_group(pid: u32) {
    signal_group(pid, "-TERM");
    thread::sleep(Duration::from_millis(50));
    signal_group(pid, "-KILL");
}

fn signal_group(pid: u32, signal: &str) {
    let _ = ProcessCommand::new("/bin/kill")
        .args([signal, &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn require_script(path: &Path) -> Result<PathBuf> {
    match path.extension().and_then(OsStr::to_str) {
        Some("js" | "mjs") => {}
        _ => bail!("workflow script must end in .js or .mjs"),
    }
    if !path.is_file() {
        bail!("workflow script not found: {}", path.display());
    }
    Ok(path.canonicalize()?)
}

fn recorded_script(paths: &Paths, run_id: &WorkflowRunId) -> Result<PathBuf> {
    let path = script_record_path(paths, run_id);
    let recorded = fs::read_to_string(&path).with_context(|| {
        format!(
            "workflow script path for {run_id} is missing ({})",
            path.display()
        )
    })?;
    let script = PathBuf::from(recorded.trim());
    if !script.is_file() {
        bail!("workflow script no longer exists: {}", script.display());
    }
    Ok(script)
}

fn file_digest(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut text = String::from("sha256:");
    for byte in digest {
        write!(&mut text, "{byte:02x}")?;
    }
    Ok(text)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn print_launch_receipt(paths: &Paths, run: &WorkflowRun, json_output: bool) {
    print_value(
        &json!({
            "run_id": run.run_id.to_string(),
            "owner": run.owner_id.as_str(),
            "state": run.state.as_str(),
            "log": log_path(paths, &run.run_id),
        }),
        json_output,
    );
}

fn pid_path(paths: &Paths, run_id: &WorkflowRunId) -> PathBuf {
    paths.runs.join(format!("{run_id}.workflow.pid"))
}

fn log_path(paths: &Paths, run_id: &WorkflowRunId) -> PathBuf {
    paths.runs.join(format!("{run_id}.workflow.log"))
}

fn script_record_path(paths: &Paths, run_id: &WorkflowRunId) -> PathBuf {
    paths.runs.join(format!("{run_id}.workflow.script"))
}
