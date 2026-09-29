//! Shell-free process execution and declarative harness manifests.
//!
//! A harness is described by a manifest, not by a command line. Both words in
//! that sentence are load-bearing and both are checked here.
//!
//! *Declarative*: every manifest struct is `deny_unknown_fields`, so a
//! misspelled key is a parse error rather than a setting that silently does
//! nothing. A harness that looks configured but is not is the failure this
//! prevents.
//!
//! *Shell-free*: `launch.argv` is a list of arguments handed to the process
//! directly. No shell ever sees it, so a value containing `;`, `$(...)`, or a
//! glob is one literal argument and not an injection point.
//!
//! ```
//! use brgr_runner::{HarnessManifest, MANIFEST_SCHEMA_V1};
//!
//! // `/bin/sh` only because `validate` requires the executable to exist; the
//! // manifest never invokes it through a shell.
//! let wire = r#"{
//!   "schema": "brgr.harness/v1",
//!   "id": "local.fixture",
//!   "adapter": "process/v1",
//!   "executable": "/bin/sh",
//!   "probe": { "version_argv": ["--version"], "help_argv": ["--help"] },
//!   "launch": { "argv": ["run", "; rm -rf / $(whoami)"], "mode": "one_shot" },
//!   "result": {
//!     "source": { "kind": "stdout" },
//!     "media_type": "text/plain",
//!     "max_bytes": 4096,
//!     "success_exit_codes": [0]
//!   }
//! }"#;
//!
//! let manifest: HarnessManifest = serde_json::from_str(wire)?;
//! manifest.validate()?;
//! assert_eq!(manifest.schema, MANIFEST_SCHEMA_V1);
//!
//! // Two arguments, and the metacharacters are data inside the second one.
//! assert_eq!(manifest.launch.argv.len(), 2);
//! assert_eq!(manifest.launch.argv[1], "; rm -rf / $(whoami)");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A key that is not part of the schema is refused, rather than ignored:
//!
//! ```
//! # use brgr_runner::HarnessManifest;
//! let wire = r#"{
//!   "schema": "brgr.harness/v1",
//!   "id": "local.fixture",
//!   "adapter": "process/v1",
//!   "executable": "/bin/sh",
//!   "probe": { "version_argv": ["--version"], "help_argv": ["--help"] },
//!   "launch": { "argv": ["run"], "mode": "one_shot", "env_alow": ["PATH"] },
//!   "result": {
//!     "source": { "kind": "stdout" },
//!     "media_type": "text/plain",
//!     "max_bytes": 4096,
//!     "success_exit_codes": [0]
//!   }
//! }"#;
//!
//! // `env_alow` is a typo for `env_allow`. Accepting it would produce a harness
//! // that runs with an empty environment allow-list and looks configured.
//! assert!(serde_json::from_str::<HarnessManifest>(wire).is_err());
//!
//! // The control: the same manifest spelled correctly parses, so the rejection
//! // above is attributable to the typo and not to something else in the text.
//! // Written after an earlier draft of this example passed for the wrong reason.
//! let corrected = wire.replace("env_alow", "env_allow");
//! assert!(serde_json::from_str::<HarnessManifest>(&corrected).is_ok());
//! ```

mod argv;
mod capture;
mod collect;
mod error;
mod manifest;
mod prompt;

pub use error::RunnerError;

use argv::{Substitutions, render_argv};
use capture::{join_capture, start_capture};
use collect::{can_collect_result, collect_result_with_model};
pub use manifest::{
    Capability, CapabilityStatus, ExecutionMode, HarnessManifest, InteractiveSpec, LaunchSpec,
    ModelCatalogFormat, ModelCatalogSpec, PermissionArgv, ProbeSpec, ResultSource, ResultSpec,
};
use prompt::{render_task_prompt, worker_prompt};

use std::{
    ffi::OsStr,
    io::{Read as _, Seek as _, SeekFrom},
    os::unix::process::CommandExt,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Re-exported because manifests take it: a caller choosing a level should not
/// need the protocol crate to name one.
pub use brgr_protocol::PermissionLevel;
use brgr_protocol::{AttemptId, TaskId, TaskInstructions};
use tempfile::TempDir;
use tokio::{
    process::Command,
    time::{sleep, timeout},
};

pub const MANIFEST_SCHEMA_V1: &str = "brgr.harness/v1";
pub const PROCESS_ADAPTER_V1: &str = "process/v1";
pub const OMP_ROLE_ADAPTER_V1: &str = "omp-role/v1";
pub(crate) const CAPTURE_OVERHEAD_BYTES: u64 = 1;
pub(crate) const JSONL_TRANSPORT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const JSONL_METADATA_SLACK_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RunRequest<'a> {
    pub workspace: &'a Path,
    pub prompt: &'a str,
    pub criteria: Option<&'a [String]>,
    pub instructions: Option<&'a TaskInstructions>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub permission: Option<PermissionLevel>,
    pub deadline: Duration,
    pub cancel_path: Option<&'a Path>,
    pub pid_path: Option<&'a Path>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub result: Vec<u8>,
    /// Native model identity from a completed JSONL assistant event, when available.
    pub observed_model: Option<String>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_truncated: bool,
    pub elapsed: Duration,
}

impl ExecutionOutput {
    #[must_use]
    pub fn succeeded(&self, manifest: &HarnessManifest) -> bool {
        !self.timed_out
            && !self.output_truncated
            && self
                .exit_code
                .is_some_and(|code| manifest.result.success_exit_codes.contains(&code))
    }
}

pub struct ProcessRunner;

/// Trusted context supplied by the brgr supervisor to a worker process.
/// It lets that worker delegate another bounded task without teaching it a
/// provider-specific child launcher.
pub struct DelegationContext<'a> {
    pub control_home: &'a Path,
    pub brgr_executable: &'a Path,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    /// Whether this task may start child tasks. Every managed worker may ask
    /// its owner a question; only a task started with delegation may delegate.
    pub may_delegate: bool,
}

impl ProcessRunner {
    /// Executes a validated one-shot process recipe without invoking a shell.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when validation, process execution, bounded
    /// capture, or result collection fails.
    pub async fn run(
        manifest: &HarnessManifest,
        request: RunRequest<'_>,
    ) -> Result<ExecutionOutput, RunnerError> {
        Self::run_with_delegation(manifest, request, None).await
    }

    /// Runs a process with an optional brgr worker identity. The runner sets
    /// these environment values itself after the manifest allowlist is applied.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::run`].
    pub async fn run_with_delegation(
        manifest: &HarnessManifest,
        request: RunRequest<'_>,
        delegation: Option<DelegationContext<'_>>,
    ) -> Result<ExecutionOutput, RunnerError> {
        validate_run_input(manifest, &request)?;

        let prompt = &worker_prompt(delegation.as_ref(), render_task_prompt(&request));
        let scratch = tempfile::tempdir()?;
        let prompt_path = scratch.path().join("prompt.txt");
        std::fs::write(&prompt_path, prompt.as_bytes())?;
        let substitutions = Substitutions {
            prompt_file: &prompt_path,
            prompt,
            workspace: request.workspace,
            model: request.model,
            effort: request.effort,
        };
        let argv = render_argv(manifest, &request, &substitutions)?;

        let started = Instant::now();
        let mut command = Command::new(&manifest.executable);
        command
            .args(argv)
            .current_dir(request.workspace)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        for name in &manifest.launch.env_allow {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(context) = delegation {
            apply_delegation_env(&mut command, &context);
        }

        let mut child = command.spawn().map_err(RunnerError::SpawnIo)?;
        #[cfg(debug_assertions)]
        crash_after_spawn_before_pid(&request);
        if let Some(path) = request.pid_path {
            std::fs::write(path, format!("{}\n", child.id().unwrap_or_default()))?;
        }
        let (mut stdout_task, mut stderr_task, overflow) = start_capture(&mut child, manifest)?;
        let process_group_id = child.id().ok_or(RunnerError::MissingProcessId)?;

        let (status, timed_out, cancelled) = wait_for_exit(
            &mut child,
            started,
            request.deadline,
            request.cancel_path,
            &overflow,
        )
        .await?;
        if let Some(path) = request.pid_path {
            let _ = std::fs::remove_file(path);
        }
        let remaining = request.deadline.saturating_sub(started.elapsed());
        let captures = timeout(remaining, async {
            let (stdout, stderr) = tokio::join!(&mut stdout_task, &mut stderr_task);
            Ok::<_, RunnerError>((join_capture(stdout)?, join_capture(stderr)?))
        })
        .await;
        let ((stdout, stdout_truncated), (stderr, stderr_truncated)) = if let Ok(result) = captures
        {
            result?
        } else {
            let _ = std::process::Command::new("/bin/kill")
                .arg("-KILL")
                .arg(format!("-{process_group_id}"))
                .output();
            stdout_task.abort();
            stderr_task.abort();
            return Err(RunnerError::CaptureDeadlineElapsed);
        };
        let output_truncated =
            overflow.load(Ordering::Relaxed) || stdout_truncated || stderr_truncated;
        let (result, observed_model) = if can_collect_result(
            status.as_ref(),
            manifest,
            cancelled,
            timed_out,
            output_truncated,
        ) {
            collect_result_with_model(manifest, &request, &substitutions, &stdout)?
        } else {
            (Vec::new(), None)
        };

        Ok(ExecutionOutput {
            exit_code: status.and_then(|value| value.code()),
            stdout,
            stderr,
            result,
            observed_model,
            timed_out,
            cancelled,
            output_truncated,
            elapsed: started.elapsed(),
        })
    }

    /// Executes a bounded probe such as `--version` or `--help`.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the executable or probe contract is
    /// invalid, times out, or cannot be captured.
    pub async fn probe(
        executable: &Path,
        argv: &[String],
        deadline: Duration,
    ) -> Result<ExecutionOutput, RunnerError> {
        Self::probe_with_path(executable, argv, deadline, None).await
    }

    /// Like [`Self::probe`], with an optional `PATH` that replaces the process
    /// environment. Production health probing uses [`Self::probe`] so it
    /// inherits the caller `PATH`; this override is for deterministic tests.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the executable or probe contract is
    /// invalid, times out, or cannot be captured.
    pub async fn probe_with_path(
        executable: &Path,
        argv: &[String],
        deadline: Duration,
        path: Option<&OsStr>,
    ) -> Result<ExecutionOutput, RunnerError> {
        if !executable.is_absolute() || !executable.is_file() {
            return Err(RunnerError::InvalidExecutable(executable.to_path_buf()));
        }
        if argv.len() > 64 || argv.iter().any(|argument| argument.contains('\0')) {
            return Err(RunnerError::InvalidArgument);
        }
        let scratch = TempDir::new()?;
        let stdout_path = scratch.path().join("stdout");
        let stderr_path = scratch.path().join("stderr");
        let mut stdout_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&stdout_path)?;
        let mut stderr_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&stderr_path)?;
        let mut command = Command::new(executable);
        command
            .args(argv)
            .current_dir(scratch.path())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file.try_clone()?))
            .stderr(Stdio::from(stderr_file.try_clone()?))
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        for name in ["HOME", "LANG"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(path) = path {
            command.env("PATH", path);
        } else if let Some(value) = std::env::var_os("PATH") {
            command.env("PATH", value);
        }
        let started = Instant::now();
        let mut child = command.spawn().map_err(RunnerError::SpawnIo)?;
        let (status, timed_out, quota_exceeded) = wait_for_probe_exit(
            &mut child,
            started,
            deadline,
            &stdout_file,
            &stderr_file,
            65_536,
        )
        .await?;
        let (stdout, stdout_truncated) = read_probe_file(&mut stdout_file, 65_536)?;
        let (stderr, stderr_truncated) = read_probe_file(&mut stderr_file, 65_536)?;
        let exit_code = status.and_then(|value| value.code());
        let output_truncated = quota_exceeded || stdout_truncated || stderr_truncated;
        let result = if exit_code == Some(0) && !timed_out && !output_truncated {
            stdout.clone()
        } else {
            Vec::new()
        };
        Ok(ExecutionOutput {
            exit_code,
            stdout,
            stderr,
            result,
            observed_model: None,
            timed_out,
            cancelled: false,
            output_truncated,
            elapsed: started.elapsed(),
        })
    }
}

fn validate_run_input(
    manifest: &HarnessManifest,
    request: &RunRequest<'_>,
) -> Result<(), RunnerError> {
    manifest.validate()?;
    if manifest.adapter != PROCESS_ADAPTER_V1 {
        return Err(RunnerError::UnsupportedAdapter(manifest.adapter.clone()));
    }
    if !request.workspace.is_dir() {
        return Err(RunnerError::InvalidWorkspace(
            request.workspace.to_path_buf(),
        ));
    }
    Ok(())
}

fn apply_delegation_env(command: &mut Command, context: &DelegationContext<'_>) {
    let worker_identity = format!("worker:{}", context.attempt_id);
    command
        .env("BRGR_HOME", context.control_home)
        .env("BRGR_BIN", context.brgr_executable)
        .env("BRGR_PARENT_TASK_ID", context.task_id.to_string())
        .env("BRGR_PARENT_ATTEMPT_ID", context.attempt_id.to_string())
        .env("BRGR_OWNER_ID", &worker_identity)
        .env("BRGR_SESSION_ID", worker_identity);
    if std::env::var("HERDR_ENV").as_deref() == Ok("1")
        && std::env::var("HERDR_PLUGIN_ID").as_deref() == Ok("brgr")
        && std::env::var_os("HERDR_PANE_ID").is_some()
    {
        for name in [
            "HERDR_ENV",
            "HERDR_PLUGIN_ID",
            "HERDR_PANE_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_BIN_PATH",
            "HERDR_SESSION",
            "HERDR_SOCKET_PATH",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("BRGR_WORKER_HERDR_CONTEXT", "1");
    }
}

fn read_probe_file(file: &mut std::fs::File, limit: u64) -> Result<(Vec<u8>, bool), RunnerError> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit;
    if truncated {
        bytes.truncate(usize::try_from(limit).expect("probe limit fits usize"));
    }
    Ok((bytes, truncated))
}

async fn wait_for_probe_exit(
    child: &mut tokio::process::Child,
    started: Instant,
    deadline: Duration,
    stdout: &std::fs::File,
    stderr: &std::fs::File,
    limit: u64,
) -> Result<(Option<ExitStatus>, bool, bool), RunnerError> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((Some(status), false, false));
        }
        if stdout.metadata()?.len() > limit || stderr.metadata()?.len() > limit {
            return Ok((Some(stop_probe_group(child).await?), false, true));
        }
        if started.elapsed() >= deadline {
            return Ok((Some(stop_probe_group(child).await?), true, false));
        }
        sleep(Duration::from_millis(1)).await;
    }
}

async fn stop_probe_group(child: &mut tokio::process::Child) -> Result<ExitStatus, RunnerError> {
    let pid = child.id().ok_or(RunnerError::MissingProcessId)?;
    let kill = std::process::Command::new("/bin/kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .output()?;
    if !kill.status.success() && child.try_wait()?.is_none() {
        child.start_kill()?;
    }
    Ok(child.wait().await?)
}

#[cfg(debug_assertions)]
fn crash_after_spawn_before_pid(request: &RunRequest<'_>) {
    if request.pid_path.is_some()
        && std::env::var("BRGR_TEST_CRASH_STAGE").as_deref() == Ok("after_spawn_before_pid")
    {
        std::process::exit(79);
    }
}

async fn wait_for_exit(
    child: &mut tokio::process::Child,
    started: Instant,
    deadline: Duration,
    cancel_path: Option<&Path>,
    overflow: &AtomicBool,
) -> Result<(Option<ExitStatus>, bool, bool), RunnerError> {
    loop {
        let cancelled = cancel_path.is_some_and(Path::exists);
        let timed_out = started.elapsed() >= deadline;
        if cancelled || timed_out || overflow.load(Ordering::Relaxed) {
            let status = stop_process_group(child).await?;
            return Ok((Some(status), timed_out, cancelled));
        }
        if let Some(status) = child.try_wait()? {
            return Ok((Some(status), false, false));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_process_group(child: &mut tokio::process::Child) -> Result<ExitStatus, RunnerError> {
    let pid = child.id().ok_or(RunnerError::MissingProcessId)?;
    let term = std::process::Command::new("/bin/kill")
        .arg("-TERM")
        .arg(format!("-{pid}"))
        .output()?;
    if !term.status.success() {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        child.start_kill()?;
        return Ok(child.wait().await?);
    }
    let grace_started = Instant::now();
    while grace_started.elapsed() < Duration::from_millis(500) {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        sleep(Duration::from_millis(25)).await;
    }
    let _ = std::process::Command::new("/bin/kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .status();
    Ok(child.wait().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::read_jsonl_semantic;
    use crate::collect::{collect_result, extract_jsonl_assistant_final, observe_jsonl_model};
    use brgr_protocol::TaskSpec;
    use brgr_protocol::{ArtifactContract, AttemptBudget, OwnerId, Route, SCHEMA_V1, TaskId};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    fn echo_manifest(max_bytes: u64) -> HarnessManifest {
        HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "test.echo".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: PathBuf::from("/bin/echo"),
            probe: ProbeSpec {
                version_argv: vec!["--version".to_owned()],
                help_argv: vec!["--help".to_owned()],
                model_catalog: None,
            },
            launch: LaunchSpec {
                argv: vec!["@${input.prompt_file}".to_owned()],
                model_argv: vec![],
                effort_argv: vec![],
                env_allow: vec![],
                mode: ExecutionMode::OneShot,
                permission_argv: PermissionArgv::default(),
                interactive: None,
            },
            result: ResultSpec {
                source: ResultSource::Stdout,
                media_type: "text/plain".to_owned(),
                max_bytes,
                success_exit_codes: vec![0],
            },
            capabilities: BTreeMap::new(),
        }
    }

    #[test]
    fn manifest_rejects_unrecognized_fields() {
        let mut value = serde_json::to_value(echo_manifest(4_096)).unwrap();
        value["unrecognized"] = serde_json::json!(true);
        assert!(serde_json::from_value::<HarnessManifest>(value).is_err());
    }

    #[test]
    fn file_result_is_bounded_and_cannot_follow_a_symlink() {
        let workspace = tempfile::tempdir().unwrap();
        let report = workspace.path().join("report.md");
        std::fs::write(&report, b"sealed fixture").unwrap();
        let mut manifest = echo_manifest(100);
        manifest.result.source = ResultSource::File {
            path: "report.md".to_owned(),
        };
        let prompt_file = workspace.path().join("prompt.txt");
        let values = Substitutions {
            prompt_file: &prompt_file,
            prompt: "fixture",
            workspace: workspace.path(),
            model: None,
            effort: None,
        };
        assert_eq!(
            collect_result(&manifest, workspace.path(), &values, b"").unwrap(),
            b"sealed fixture"
        );
        manifest.result.max_bytes = 3;
        assert!(matches!(
            collect_result(&manifest, workspace.path(), &values, b""),
            Err(RunnerError::ResultTooLarge { .. })
        ));
        let link = workspace.path().join("linked.md");
        std::os::unix::fs::symlink(&report, &link).unwrap();
        manifest.result.max_bytes = 100;
        manifest.result.source = ResultSource::File {
            path: "linked.md".to_owned(),
        };
        assert!(matches!(
            collect_result(&manifest, workspace.path(), &values, b""),
            Err(RunnerError::ResultNotRegularFile(_))
        ));
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape")).unwrap();
        manifest.result.source = ResultSource::File {
            path: "escape/secret.md".to_owned(),
        };
        assert!(matches!(
            collect_result(&manifest, workspace.path(), &values, b""),
            Err(RunnerError::ResultPathOutsideWorkspace(_))
        ));
    }

    #[test]
    fn literal_prompt_is_one_argv_value_even_with_template_like_text() {
        let workspace = tempfile::tempdir().unwrap();
        let prompt = "Do not expand ${route.model} or $(touch /tmp/never)";
        let request = RunRequest {
            workspace: workspace.path(),
            prompt,
            criteria: None,
            instructions: None,
            model: None,
            effort: None,
            permission: None,
            deadline: Duration::from_secs(2),
            cancel_path: None,
            pid_path: None,
        };
        let mut manifest = echo_manifest(4_096);
        manifest.launch.argv = vec!["--print".to_owned(), "${input.prompt}".to_owned()];
        let prompt_path = workspace.path().join("unused-prompt.txt");
        let substitutions = Substitutions {
            prompt_file: &prompt_path,
            prompt,
            workspace: workspace.path(),
            model: None,
            effort: None,
        };

        assert_eq!(
            render_argv(&manifest, &request, &substitutions).unwrap(),
            ["--print", prompt]
        );
    }

    #[test]
    fn unsupported_route_is_rejected_before_process_execution() {
        let mut manifest = echo_manifest(4_096);
        manifest.id = "local.synthetic".to_owned();
        manifest.capabilities.insert(
            "completion".to_owned(),
            Capability {
                status: CapabilityStatus::Supported,
                semantics: "process_exit".to_owned(),
                evidence_ref: None,
                tested_identity: None,
            },
        );
        let mut task = TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            create_request_id: "route-test".to_owned(),
            owner_id: OwnerId::new("codex:test").unwrap(),
            objective: "check route".to_owned(),
            workspace: "/tmp/check-route".to_owned(),
            route: Route {
                harness_id: manifest.id.clone(),
                requested_model: Some("gpt-5.6-luna".to_owned()),
                requested_effort: None,
            },
            required_capabilities: vec!["completion".to_owned()],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 4_096,
            },
            acceptance_criteria: vec!["exact answer".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 2,
                max_attempts: 1,
            },
            instructions: TaskInstructions::default(),
            evidence: brgr_protocol::EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
        };
        assert!(matches!(
            manifest.validate_task_route(&task),
            Err(RunnerError::UnsupportedCapability(name)) if name == "model_select"
        ));
        task.route.requested_model = None;
        task.route.requested_effort = Some("low".to_owned());
        assert!(matches!(
            manifest.validate_task_route(&task),
            Err(RunnerError::UnsupportedCapability(name)) if name == "effort_select"
        ));
    }

    #[tokio::test]
    async fn inherited_stdout_cannot_extend_the_total_deadline() {
        let workspace = tempfile::tempdir().unwrap();
        let executable = workspace.path().join("fork-stdout-holder");
        std::fs::write(
            &executable,
            "#!/bin/sh\n(sleep 3) &\nprintf 'parent done\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manifest = echo_manifest(4_096);
        manifest.executable = executable;
        manifest.launch.argv.clear();

        let started = Instant::now();
        let result = ProcessRunner::run(
            &manifest,
            RunRequest {
                workspace: workspace.path(),
                prompt: "bounded",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_millis(250),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await;

        match result {
            Err(RunnerError::CaptureDeadlineElapsed) => {}
            Ok(output) => assert!(output.timed_out && !output.succeeded(&manifest)),
            other => panic!("unexpected unbounded outcome: {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn executes_without_a_shell_and_captures_output() {
        let workspace = tempfile::tempdir().unwrap();
        let manifest = echo_manifest(4_096);
        let output = ProcessRunner::run(
            &manifest,
            RunRequest {
                workspace: workspace.path(),
                prompt: "hello",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_secs(2),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
        .unwrap();

        assert!(output.succeeded(&manifest));
        assert!(String::from_utf8(output.stdout).unwrap().starts_with('@'));
    }

    #[tokio::test]
    async fn marks_oversized_output_as_truncated() {
        let workspace = tempfile::tempdir().unwrap();
        let manifest = echo_manifest(2);
        let output = ProcessRunner::run(
            &manifest,
            RunRequest {
                workspace: workspace.path(),
                prompt: "hello",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_secs(2),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
        .unwrap();

        assert!(output.output_truncated);
        assert!(!output.succeeded(&manifest));
    }

    #[tokio::test]
    async fn jsonl_updates_can_exceed_result_limit_without_losing_final_evidence() {
        use tokio::io::AsyncWriteExt as _;

        let update = format!(
            "{}\n",
            json!({"type": "message_update", "delta": "x".repeat(8192)})
        );
        let mut raw = update.repeat(200).into_bytes();
        raw.extend_from_slice(
            format!(
                "{}\n{}\n",
                json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "provider": "openai-codex",
                        "model": "gpt-5.6-luna",
                        "content": [{"type": "text", "text": "READY"}],
                    },
                }),
                json!({"type": "agent_end", "stopReason": "completed"})
            )
            .as_bytes(),
        );
        assert!(raw.len() > 1_048_576);
        let (mut writer, reader) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move { writer.write_all(&raw).await.unwrap() });
        let overflow = Arc::new(AtomicBool::new(false));
        let (semantic, truncated) = read_jsonl_semantic(
            reader,
            1_048_576,
            JSONL_TRANSPORT_LIMIT_BYTES,
            Arc::clone(&overflow),
        )
        .await
        .unwrap();
        writer_task.await.unwrap();

        assert!(!truncated);
        assert!(!overflow.load(Ordering::Relaxed));
        assert!(semantic.len() < 1024);
        assert_eq!(extract_jsonl_assistant_final(&semantic).unwrap(), b"READY");
        assert_eq!(
            observe_jsonl_model(&semantic, Some("openai-codex/gpt-5.6-luna"))
                .unwrap()
                .as_deref(),
            Some("openai-codex/gpt-5.6-luna")
        );
    }

    /// The published contract in the README bounds raw JSONL transport at
    /// 64 MiB. Behavior is exercised against a small injected bound below, so
    /// this locks the value the production capture path actually passes.
    /// A key a result source's kind does not take is an error, never ignored.
    ///
    /// `{"kind":"stdout","path":"report.md"}` used to parse as `Stdout`, so a
    /// manifest meaning `file` sealed raw stdout and skipped the file guards.
    /// Plain `deny_unknown_fields` does not fix that on a unit variant of an
    /// internally tagged enum — checked, it still parsed — hence the field
    /// struct this now goes through.
    #[test]
    fn a_result_source_refuses_keys_its_kind_does_not_take() {
        let parse = |text: &str| serde_json::from_str::<ResultSource>(text);
        for (text, expected) in [
            (r#"{"kind":"stdout"}"#, ResultSource::Stdout),
            (
                r#"{"kind":"jsonl_assistant_final"}"#,
                ResultSource::JsonlAssistantFinal,
            ),
            (
                r#"{"kind":"file","path":"out.md"}"#,
                ResultSource::File {
                    path: "out.md".to_owned(),
                },
            ),
        ] {
            let parsed = parse(text).unwrap();
            assert_eq!(parsed, expected);
            // What brgr writes, brgr can read back.
            let written = serde_json::to_string(&parsed).unwrap();
            assert_eq!(parse(&written).unwrap(), expected, "{written}");
        }
        for text in [
            r#"{"kind":"stdout","path":"report.md"}"#,
            r#"{"kind":"jsonl_assistant_final","extra":1}"#,
            r#"{"kind":"file"}"#,
            r#"{"kind":"file","path":"out.md","mode":"x"}"#,
            r#"{"kind":"stdoutt"}"#,
            r#"{"path":"out.md"}"#,
        ] {
            assert!(parse(text).is_err(), "{text} was accepted");
        }
    }

    #[test]
    fn jsonl_transport_limit_matches_the_published_bound() {
        assert_eq!(JSONL_TRANSPORT_LIMIT_BYTES, 64 * 1024 * 1024);
    }

    /// A deterministic generator of adversarial bytes.
    ///
    /// `AGENTS.md` calls harness output untrusted, and the capture path is the
    /// only place that reads it. The example-based tests around it all describe
    /// well-formed streams; this walks shapes nobody wrote by hand. Seeded, so a
    /// failure is reproducible from the printed case rather than from luck.
    struct Adversary(u64);

    impl Adversary {
        fn next(&mut self) -> u64 {
            // xorshift64*, enough for shaping input and small enough to read.
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn pick<'a, T: ?Sized>(&mut self, options: &'a [&'a T]) -> &'a T {
            options[usize::try_from(self.next()).unwrap_or(0) % options.len()]
        }

        /// Builds a stream out of fragments that have each broken something:
        /// truncated JSON, the right shape with the wrong types, control bytes,
        /// invalid UTF-8, no terminator, and a line with no newline at all.
        fn stream(&mut self) -> Vec<u8> {
            const FRAGMENTS: [&[u8]; 16] = [
                // Two identities, so model observation is reached and a stream
                // carrying both exercises the mixed-model refusal. Without them
                // the model assertion below was a branch no case entered.
                b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"provider\":\"anthropic\",\"model\":\"opus\"}}",
                b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"provider\":\"openai\",\"model\":\"gpt\"}}",
                b"{\"type\":\"message_update\",\"delta\":\"x\"}",
                b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\"}}",
                b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[]}}",
                b"{\"type\":\"agent_end\"}",
                b"{\"type\":\"agent_end\",\"stopReason\":\"completed\"}",
                b"{\"type\":42}",
                b"{\"type\":\"message_end\",\"message\":null}",
                b"{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}",
                b"{",
                b"}",
                b"not json at all",
                b"",
                b"\x00\x01\x02\x7f",
                b"\xff\xfe invalid utf-8",
            ];
            let mut stream = Vec::new();
            for _ in 0..(self.next() % 12) {
                stream.extend_from_slice(self.pick(&FRAGMENTS));
                if !self.next().is_multiple_of(8) {
                    stream.push(b'\n');
                }
            }
            stream
        }

        /// A stream whose *retained* content passes the retention bound.
        ///
        /// Small fragments never reach it: retention is bounded by the result
        /// limit plus a megabyte of slack, so a few kilobytes of input leaves the
        /// bound untested. Verified by deleting the limit check — with only
        /// [`Self::stream`] the property still passed, which made it decoration.
        /// Only retained event kinds count, so this emits those.
        fn flood(past: usize) -> Vec<u8> {
            let filler = "y".repeat(8_000);
            let mut stream = Vec::with_capacity(past + 16_000);
            while stream.len() < past {
                stream.extend_from_slice(
                    format!(
                        "{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\
                         \"content\":[{{\"type\":\"text\",\"text\":\"{filler}\"}}]}}}}\n"
                    )
                    .as_bytes(),
                );
            }
            stream
        }
    }

    /// The capture path must hold its bounds and its refusals for any input.
    ///
    /// Three properties, none of which any example test states: retention never
    /// exceeds the limit it was given, a stream is never reported both complete
    /// and truncated, and a final answer is only ever produced for a stream that
    /// actually carries a terminal assistant event.
    #[tokio::test]
    async fn arbitrary_harness_output_never_breaks_the_capture_bounds() {
        use tokio::io::AsyncWriteExt as _;

        /// The head of a stream, for failure messages.
        ///
        /// A failing flood case would otherwise print a megabyte of filler and
        /// bury the assertion that fired.
        fn excerpt(raw: &[u8]) -> String {
            let head = String::from_utf8_lossy(&raw[..raw.len().min(160)]).into_owned();
            format!("{} bytes starting {head:?}", raw.len())
        }

        const RESULT_LIMIT: u64 = 4 * 1024;
        // Deliberately above the retention bound. With a transport limit below
        // it the reader always cuts on transport first, so retention never grows
        // far enough for its own bound to mean anything — verified by deleting
        // the limit check in `retain_jsonl_event` and watching this test still
        // pass with a 64 KiB transport. Transport truncation has its own tests.
        const TRANSPORT: u64 = 8 * 1024 * 1024;
        let mut adversary = Adversary(0x5eed_1234_abcd_0001);
        let mut observed_models = 0_u32;

        for case in 0..512 {
            // Every sixteenth case carries enough retainable content to press on
            // the retention bound; the rest explore shape rather than size.
            let raw = if case % 16 == 0 {
                Adversary::flood(
                    usize::try_from(RESULT_LIMIT + JSONL_METADATA_SLACK_BYTES).unwrap() + 32_000,
                )
            } else {
                adversary.stream()
            };
            let (mut writer, reader) = tokio::io::duplex(256);
            let payload = raw.clone();
            let writer_task = tokio::spawn(async move {
                let _ = writer.write_all(&payload).await;
            });
            let overflow = Arc::new(AtomicBool::new(false));
            let (semantic, truncated) =
                read_jsonl_semantic(reader, RESULT_LIMIT, TRANSPORT, Arc::clone(&overflow))
                    .await
                    .expect("a duplex read cannot fail");
            writer_task.await.unwrap();

            assert!(
                u64::try_from(semantic.len()).unwrap() <= RESULT_LIMIT + JSONL_METADATA_SLACK_BYTES,
                "case {case} retained {} bytes past the limit: {}",
                semantic.len(),
                excerpt(&raw)
            );
            assert_eq!(
                truncated,
                overflow.load(Ordering::Relaxed),
                "case {case} disagreed with its own overflow flag: {}",
                excerpt(&raw)
            );
            // A final answer may only come from a stream that carries the event
            // that ends one. Anything else must be an error, never a value.
            if let Ok(final_answer) = extract_jsonl_assistant_final(&semantic) {
                assert!(
                    semantic
                        .windows(b"message_end".len())
                        .any(|w| w == b"message_end")
                        || semantic
                            .windows(b"turn_end".len())
                            .any(|w| w == b"turn_end"),
                    "case {case} produced {final_answer:?} with no terminal event: {}",
                    excerpt(&raw)
                );
            }
            // Model observation reads the same bytes and must not panic or invent
            // an identity the stream does not contain.
            // The observer reports `provider/model`, joined, while the stream
            // carries them as two JSON members, so the joined form is never
            // contiguous in it. Each half has to be present instead.
            if let Ok(Some(model)) = observe_jsonl_model(&semantic, None) {
                observed_models += 1;
                let (provider, name) = model.split_once('/').expect("provider/model");
                for part in [provider, name] {
                    let quoted = format!("\"{part}\"");
                    assert!(
                        semantic
                            .windows(quoted.len())
                            .any(|w| w == quoted.as_bytes()),
                        "case {case} reported model {model}, but {part} is absent: {}",
                        excerpt(&raw)
                    );
                }
            }
        }
        // Counted for the same reason as the retention flood: a branch no case
        // enters asserts nothing.
        assert!(
            observed_models > 5,
            "only {observed_models} of 512 cases reached model observation"
        );
    }

    #[tokio::test]
    async fn jsonl_transport_limit_truncates_without_retaining_the_raw_stream() {
        use tokio::io::AsyncWriteExt as _;

        // A small injected bound keeps the test deterministic and cheap; the
        // production value is locked separately.
        const TRANSPORT: u64 = 256 * 1024;

        // Update events are discarded by retention, so anything retained here
        // would be raw transport rather than evidence worth sealing.
        let update = format!(
            "{}\n",
            json!({"type": "message_update", "delta": "x".repeat(8192)})
        );
        let line_bytes = u64::try_from(update.len()).unwrap();
        let over_limit = TRANSPORT.saturating_add(line_bytes);
        let (mut writer, reader) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            let mut written = 0_u64;
            while written < over_limit {
                if writer.write_all(update.as_bytes()).await.is_err() {
                    // Capture stopped at the limit and dropped its end.
                    break;
                }
                written = written.saturating_add(line_bytes);
            }
            written
        });
        let overflow = Arc::new(AtomicBool::new(false));
        let (semantic, truncated) =
            read_jsonl_semantic(reader, 1_048_576, TRANSPORT, Arc::clone(&overflow))
                .await
                .unwrap();
        let written = writer_task.await.unwrap();

        assert!(truncated);
        assert!(overflow.load(Ordering::Relaxed));
        assert!(
            semantic.is_empty(),
            "retained {} bytes of a discarded raw stream",
            semantic.len()
        );
        assert!(
            written > TRANSPORT / 2,
            "writer stopped too early to exercise the transport limit"
        );
    }

    #[tokio::test]
    async fn jsonl_stream_just_under_the_transport_limit_keeps_its_final_evidence() {
        use tokio::io::AsyncWriteExt as _;

        const TRANSPORT: u64 = 256 * 1024;

        let update = format!(
            "{}\n",
            json!({"type": "message_update", "delta": "x".repeat(8192)})
        );
        let tail = format!(
            "{}\n{}\n",
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "provider": "openai-codex",
                    "model": "gpt-5.6-luna",
                    "content": [{"type": "text", "text": "READY"}],
                },
            }),
            json!({"type": "agent_end", "stopReason": "completed"})
        );
        let line_bytes = u64::try_from(update.len()).unwrap();
        let tail_bytes = u64::try_from(tail.len()).unwrap();
        let (mut writer, reader) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            let mut written = 0_u64;
            while written
                .saturating_add(line_bytes)
                .saturating_add(tail_bytes)
                <= TRANSPORT
            {
                writer.write_all(update.as_bytes()).await.unwrap();
                written = written.saturating_add(line_bytes);
            }
            writer.write_all(tail.as_bytes()).await.unwrap();
            written
        });
        let overflow = Arc::new(AtomicBool::new(false));
        let (semantic, truncated) =
            read_jsonl_semantic(reader, 1_048_576, TRANSPORT, Arc::clone(&overflow))
                .await
                .unwrap();
        let written = writer_task.await.unwrap();

        assert!(!truncated);
        assert!(!overflow.load(Ordering::Relaxed));
        assert_eq!(extract_jsonl_assistant_final(&semantic).unwrap(), b"READY");
        assert!(
            written > TRANSPORT / 2,
            "stream stopped too early to approach the transport limit"
        );
        assert!(
            u64::try_from(semantic.len()).unwrap() < JSONL_METADATA_SLACK_BYTES,
            "retention grew with the stream instead of the evidence"
        );
    }

    #[tokio::test]
    async fn malformed_jsonl_line_is_not_hidden_by_semantic_capture() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(256);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(b"not-json\n{\"type\":\"agent_end\",\"stopReason\":\"completed\"}\n")
                .await
                .unwrap();
        });
        let overflow = Arc::new(AtomicBool::new(false));
        let (semantic, truncated) =
            read_jsonl_semantic(reader, 1024, JSONL_TRANSPORT_LIMIT_BYTES, overflow)
                .await
                .unwrap();
        writer_task.await.unwrap();

        assert!(!truncated);
        assert!(matches!(
            extract_jsonl_assistant_final(&semantic),
            Err(RunnerError::MalformedJsonl(_))
        ));
    }

    #[test]
    fn jsonl_final_artifact_still_obeys_result_limit() {
        let workspace = tempfile::tempdir().unwrap();
        let mut manifest = echo_manifest(4);
        manifest.result.source = ResultSource::JsonlAssistantFinal;
        let events = b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"TOO-LONG\"}]}}\n{\"type\":\"agent_end\",\"stopReason\":\"completed\"}\n";
        let request = RunRequest {
            workspace: workspace.path(),
            prompt: "ignored",
            criteria: None,
            instructions: None,
            model: None,
            effort: None,
            permission: None,
            deadline: Duration::from_secs(1),
            cancel_path: None,
            pid_path: None,
        };
        let values = Substitutions {
            prompt_file: workspace.path(),
            prompt: "ignored",
            workspace: workspace.path(),
            model: None,
            effort: None,
        };
        assert!(matches!(
            collect_result_with_model(&manifest, &request, &values, events),
            Err(RunnerError::ResultTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn oversized_jsonl_event_stops_its_process_group_before_deadline() {
        let workspace = tempfile::tempdir().unwrap();
        let executable = workspace.path().join("oversized-jsonl");
        std::fs::write(
            &executable,
            "#!/bin/sh\n/usr/bin/awk 'BEGIN { for (i = 0; i < 1200000; i++) printf \"x\" }'\n/bin/sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manifest = echo_manifest(16);
        manifest.executable = executable;
        manifest.result.source = ResultSource::JsonlAssistantFinal;
        let started = Instant::now();
        let output = ProcessRunner::run(
            &manifest,
            RunRequest {
                workspace: workspace.path(),
                prompt: "ignored",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_secs(10),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
        .unwrap();

        assert!(output.output_truncated);
        assert!(!output.succeeded(&manifest));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn cancellation_stops_a_process_group_with_a_grandchild() {
        let workspace = tempfile::tempdir().unwrap();
        let cancel = workspace.path().join("cancel");
        std::fs::write(&cancel, b"cancel").unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures/gjc")
            .canonicalize()
            .unwrap();
        let mut manifest = echo_manifest(4_096);
        manifest.executable = fixture;
        manifest.result.source = ResultSource::JsonlAssistantFinal;
        manifest.launch.argv = vec![
            "-p".to_owned(),
            "--mode=json".to_owned(),
            "@${input.prompt_file}".to_owned(),
        ];

        let started = Instant::now();
        let output = ProcessRunner::run(
            &manifest,
            RunRequest {
                workspace: workspace.path(),
                prompt: "SLOW",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_secs(10),
                cancel_path: Some(&cancel),
                pid_path: None,
            },
        )
        .await
        .unwrap();

        assert!(output.cancelled);
        assert!(!output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn rejects_relative_executables() {
        let mut manifest = echo_manifest(4_096);
        manifest.executable = PathBuf::from("echo");
        assert!(matches!(
            manifest.validate(),
            Err(RunnerError::InvalidExecutable(_))
        ));
    }

    /// A CLI-validated catalog names no command, and every other format must.
    #[test]
    fn a_cli_validated_catalog_is_the_only_one_without_a_command() {
        let mut manifest = echo_manifest(4_096);
        manifest.probe.model_catalog = Some(ModelCatalogSpec {
            argv: vec![],
            format: ModelCatalogFormat::CliValidated,
        });
        manifest.validate().unwrap();
        manifest.probe.model_catalog = Some(ModelCatalogSpec {
            argv: vec!["models".to_owned()],
            format: ModelCatalogFormat::CliValidated,
        });
        assert!(matches!(
            manifest.validate(),
            Err(RunnerError::InvalidModelCatalog)
        ));
        manifest.probe.model_catalog = Some(ModelCatalogSpec {
            argv: vec![],
            format: ModelCatalogFormat::Lines,
        });
        assert!(matches!(
            manifest.validate(),
            Err(RunnerError::InvalidModelCatalog)
        ));
    }

    #[test]
    fn optional_catalog_preserves_old_manifest_shape_and_rejects_unknown_templates() {
        let mut manifest = echo_manifest(4_096);
        let old = serde_json::to_vec(&manifest).unwrap();
        assert!(!String::from_utf8_lossy(&old).contains("model_catalog"));
        let decoded: HarnessManifest = serde_json::from_slice(&old).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), old);
        manifest.probe.model_catalog = Some(ModelCatalogSpec {
            argv: vec!["--list-models=${unknown}".to_owned()],
            format: ModelCatalogFormat::FirstColumn,
        });
        assert!(matches!(
            manifest.validate(),
            Err(RunnerError::InvalidModelCatalog)
        ));
    }

    /// Long enough that machine load cannot fire it. Used wherever the deadline
    /// is incidental to what a probe test asserts; a test whose subject *is* the
    /// deadline keeps its own tight one.
    const INCIDENTAL_PROBE_DEADLINE: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn file_backed_probe_captures_full_help_and_flags_oversize() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("probe");
        std::fs::write(
            &executable,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 100 ]; do printf 'model-catalog-line-12345678901234567890123456789012345678901234567890\\n'; i=$((i+1)); done\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = ProcessRunner::probe(&executable, &[], INCIDENTAL_PROBE_DEADLINE)
            .await
            .unwrap();
        assert!(!output.timed_out);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.len() > 512);
        assert!(!output.output_truncated);
        std::fs::write(
            &executable,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 1200 ]; do printf 'model-catalog-line-12345678901234567890123456789012345678901234567890\\n'; i=$((i+1)); done\n",
        )
        .unwrap();
        let oversized = ProcessRunner::probe(&executable, &[], INCIDENTAL_PROBE_DEADLINE)
            .await
            .unwrap();
        assert!(!oversized.timed_out);
        assert!(oversized.output_truncated);
        assert_eq!(oversized.stdout.len(), 65_536);
    }

    #[tokio::test]
    async fn probe_stops_a_flood_before_its_deadline_and_reads_the_original_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("probe");
        let captured = root.path().join("captured");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n/bin/ln stdout '{}'\nexec /usr/bin/yes MODEL_CATALOG_FLOOD\n",
                captured.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let flooded = ProcessRunner::probe(&executable, &[], Duration::from_secs(2))
            .await
            .unwrap();
        assert!(flooded.output_truncated);
        assert!(
            !flooded.timed_out,
            "the deadline fired before the output watchdog stopped the flood"
        );
        // The watchdog is a sampled soft limit, not an OS-enforced disk quota.
        // A fixed byte ceiling is scheduler-dependent on fast CI machines;
        // instead prove the process group is gone and the file stops growing.
        let stopped_len = std::fs::metadata(&captured).unwrap().len();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(std::fs::metadata(&captured).unwrap().len(), stopped_len);

        std::fs::write(
            &executable,
            "#!/bin/sh\n/bin/mv stdout moved\n/bin/ln -s /etc/passwd stdout\nprintf 'SAFE_PROBE'\n",
        )
        .unwrap();
        let replaced = ProcessRunner::probe(&executable, &[], INCIDENTAL_PROBE_DEADLINE)
            .await
            .unwrap();
        assert!(!replaced.timed_out);
        assert_eq!(replaced.stdout, b"SAFE_PROBE");
        assert!(!replaced.output_truncated);
    }

    #[test]
    fn jsonl_result_keeps_only_final_assistant_text() {
        let events = concat!(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"secret prompt\"}]}}\n",
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
            "{\"type\":\"agent_end\",\"stopReason\":\"completed\"}\n",
        );
        assert_eq!(
            extract_jsonl_assistant_final(events.as_bytes()).unwrap(),
            b"answer"
        );
    }

    #[test]
    fn jsonl_model_identity_rejects_missing_mismatch_and_fallback() {
        let event = |provider: Option<&str>, model: Option<&str>| {
            serde_json::json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "provider": provider,
                    "model": model,
                    "content": [{"type": "text", "text": "answer"}],
                }
            })
            .to_string()
        };
        let exact = event(Some("workbuddy"), Some("deepseek-v4.1-flash"));
        assert_eq!(
            observe_jsonl_model(exact.as_bytes(), Some("workbuddy/deepseek-v4.1-flash"))
                .unwrap()
                .as_deref(),
            Some("workbuddy/deepseek-v4.1-flash")
        );
        assert!(matches!(
            observe_jsonl_model(exact.as_bytes(), Some("other/model")),
            Err(RunnerError::ObservedModelMismatch { .. })
        ));
        let missing = event(None, None);
        assert!(matches!(
            observe_jsonl_model(missing.as_bytes(), Some("workbuddy/deepseek-v4.1-flash")),
            Err(RunnerError::ObservedModelUnavailable)
        ));
        let mixed = format!("{exact}\n{}", event(Some("other"), Some("model")));
        assert!(matches!(
            observe_jsonl_model(mixed.as_bytes(), None),
            Err(RunnerError::MixedObservedModels)
        ));
    }

    #[test]
    fn jsonl_without_terminal_event_fails_closed() {
        let events = b"{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n";
        assert!(matches!(
            extract_jsonl_assistant_final(events),
            Err(RunnerError::MissingTerminalEvent)
        ));
    }

    #[test]
    fn omp_jsonl_requires_final_turn_and_agent_end_when_stop_reason_is_absent() {
        let events = concat!(
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
            "{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}\n",
            "{\"type\":\"agent_end\"}\n",
        );
        assert_eq!(
            extract_jsonl_assistant_final(events.as_bytes()).unwrap(),
            b"answer"
        );
        let missing_turn = events.replace(
            "{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}\n",
            "",
        );
        assert!(matches!(
            extract_jsonl_assistant_final(missing_turn.as_bytes()),
            Err(RunnerError::MissingTerminalEvent)
        ));
    }

    #[tokio::test]
    async fn probe_with_path_can_hide_an_env_interpreter() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let interp = bin.join("brgr-health-interp");
        std::fs::write(&interp, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&interp, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.path().join("tool");
        std::fs::write(&executable, "#!/usr/bin/env brgr-health-interp\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        let wide = ProcessRunner::probe_with_path(
            &executable,
            &[],
            INCIDENTAL_PROBE_DEADLINE,
            Some(bin.as_os_str()),
        )
        .await
        .unwrap();
        assert!(!wide.timed_out, "the probe deadline fired under load");
        assert_eq!(wide.exit_code, Some(0));
        assert_eq!(wide.stdout, b"ok\n");

        let narrow = ProcessRunner::probe_with_path(
            &executable,
            &[],
            INCIDENTAL_PROBE_DEADLINE,
            Some(OsStr::new("/usr/bin:/bin")),
        )
        .await
        .unwrap();
        assert_ne!(narrow.exit_code, Some(0));
        let dumped = String::from_utf8_lossy(&narrow.stdout);
        assert!(!dumped.contains("python"));
        assert_ne!(dumped.trim(), "ok");
    }
}
