//! Shell-free process execution and declarative harness manifests.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

use brgr_protocol::TaskSpec;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinError,
    time::{sleep, timeout},
};

pub const MANIFEST_SCHEMA_V1: &str = "brgr.harness/v1";
pub const PROCESS_ADAPTER_V1: &str = "process/v1";
pub const OMP_ROLE_ADAPTER_V1: &str = "omp-role/v1";
const CAPTURE_OVERHEAD_BYTES: u64 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManifest {
    pub schema: String,
    pub id: String,
    pub adapter: String,
    pub executable: PathBuf,
    pub probe: ProbeSpec,
    pub launch: LaunchSpec,
    pub result: ResultSpec,
    #[serde(default)]
    pub capabilities: BTreeMap<String, Capability>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSpec {
    pub version_argv: Vec<String>,
    pub help_argv: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub model_argv: Vec<String>,
    #[serde(default)]
    pub effort_argv: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    pub mode: ExecutionMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    OneShot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSpec {
    pub source: ResultSource,
    pub media_type: String,
    pub max_bytes: u64,
    pub success_exit_codes: Vec<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultSource {
    Stdout,
    JsonlAssistantFinal,
    File { path: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub status: CapabilityStatus,
    pub semantics: String,
    pub evidence_ref: Option<String>,
    pub tested_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct RunRequest<'a> {
    pub workspace: &'a Path,
    pub prompt: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
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
        manifest.validate()?;
        if manifest.adapter != PROCESS_ADAPTER_V1 {
            return Err(RunnerError::UnsupportedAdapter(manifest.adapter.clone()));
        }
        if !request.workspace.is_dir() {
            return Err(RunnerError::InvalidWorkspace(
                request.workspace.to_path_buf(),
            ));
        }

        let scratch = tempfile::tempdir()?;
        let prompt_path = scratch.path().join("prompt.txt");
        std::fs::write(&prompt_path, request.prompt.as_bytes())?;
        let substitutions = Substitutions {
            prompt_file: &prompt_path,
            prompt: request.prompt,
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

        let mut child = command.spawn().map_err(RunnerError::SpawnIo)?;
        if let Some(path) = request.pid_path {
            std::fs::write(path, format!("{}\n", child.id().unwrap_or_default()))?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or(RunnerError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(RunnerError::MissingPipe("stderr"))?;
        let limit = manifest.result.max_bytes;
        let mut stdout_task = tokio::spawn(read_bounded(stdout, limit));
        let mut stderr_task = tokio::spawn(read_bounded(stderr, limit));
        let process_group_id = child.id().ok_or(RunnerError::MissingProcessId)?;

        let (status, timed_out, cancelled) =
            wait_for_exit(&mut child, started, request.deadline, request.cancel_path).await?;
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
        let successful_exit = status
            .as_ref()
            .and_then(std::process::ExitStatus::code)
            .is_some_and(|code| manifest.result.success_exit_codes.contains(&code));
        let result = if !cancelled && !timed_out && !stdout_truncated && successful_exit {
            collect_result(manifest, request.workspace, &substitutions, &stdout)?
        } else {
            Vec::new()
        };

        Ok(ExecutionOutput {
            exit_code: status.and_then(|value| value.code()),
            stdout,
            stderr,
            result,
            timed_out,
            cancelled,
            output_truncated: stdout_truncated || stderr_truncated,
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
        let manifest = HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "internal.probe".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: executable.to_path_buf(),
            probe: ProbeSpec {
                version_argv: argv.to_vec(),
                help_argv: argv.to_vec(),
            },
            launch: LaunchSpec {
                argv: argv.to_vec(),
                model_argv: vec![],
                effort_argv: vec![],
                env_allow: vec!["HOME".to_owned(), "PATH".to_owned(), "LANG".to_owned()],
                mode: ExecutionMode::OneShot,
            },
            result: ResultSpec {
                source: ResultSource::Stdout,
                media_type: "text/plain".to_owned(),
                max_bytes: 65_536,
                success_exit_codes: vec![0],
            },
            capabilities: BTreeMap::new(),
        };
        let scratch = TempDir::new()?;
        Self::run(
            &manifest,
            RunRequest {
                workspace: scratch.path(),
                prompt: "",
                model: None,
                effort: None,
                deadline,
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
    }
}

impl HarnessManifest {
    /// Rejects an unsupported route before any task workspace or process is created.
    ///
    /// # Errors
    ///
    /// Returns an explicit capability error for an unclaimed model, effort, or
    /// required operation. OMP's internal process wrapper preserves the
    /// activated OMP capabilities but has its own executable identity.
    pub fn validate_task_route(&self, task: &TaskSpec) -> Result<(), RunnerError> {
        self.validate()?;
        if self.id != task.route.harness_id
            && !(self.id == "internal.omp-runner"
                && matches!(
                    task.route.harness_id.as_str(),
                    "local.omp" | "local.omp-herdr"
                ))
        {
            return Err(RunnerError::HarnessRouteMismatch {
                requested: task.route.harness_id.clone(),
                actual: self.id.clone(),
            });
        }
        for name in &task.required_capabilities {
            self.require_capability(name)?;
        }
        if task.route.requested_model.is_some() {
            self.require_capability("model_select")?;
            if self.adapter != OMP_ROLE_ADAPTER_V1
                && self.launch.model_argv.is_empty()
                && !self
                    .launch
                    .argv
                    .iter()
                    .any(|arg| arg.contains("${route.model}"))
            {
                return Err(RunnerError::UnsupportedCapability(
                    "model_select".to_owned(),
                ));
            }
        }
        if task.route.requested_effort.is_some() {
            self.require_capability("effort_select")?;
            if self.adapter != OMP_ROLE_ADAPTER_V1
                && self.launch.effort_argv.is_empty()
                && !self
                    .launch
                    .argv
                    .iter()
                    .any(|arg| arg.contains("${route.effort}"))
            {
                return Err(RunnerError::UnsupportedCapability(
                    "effort_select".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn require_capability(&self, name: &str) -> Result<(), RunnerError> {
        if self
            .capabilities
            .get(name)
            .is_some_and(|capability| capability.status == CapabilityStatus::Supported)
        {
            Ok(())
        } else {
            Err(RunnerError::UnsupportedCapability(name.to_owned()))
        }
    }

    /// Validates the non-programmable v1 manifest contract.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] for unsupported schemas, relative executables,
    /// unsafe arguments, invalid limits, or duplicate environment names.
    pub fn validate(&self) -> Result<(), RunnerError> {
        if self.schema != MANIFEST_SCHEMA_V1 {
            return Err(RunnerError::UnsupportedSchema(self.schema.clone()));
        }
        if !matches!(
            self.adapter.as_str(),
            PROCESS_ADAPTER_V1 | OMP_ROLE_ADAPTER_V1
        ) {
            return Err(RunnerError::UnsupportedAdapter(self.adapter.clone()));
        }
        if self.id.trim().is_empty() || !self.id.contains('.') {
            return Err(RunnerError::InvalidHarnessId(self.id.clone()));
        }
        if !self.executable.is_absolute() || !self.executable.is_file() {
            return Err(RunnerError::InvalidExecutable(self.executable.clone()));
        }
        if self.launch.argv.iter().any(|value| value.contains('\0')) {
            return Err(RunnerError::InvalidArgument);
        }
        if self.result.max_bytes == 0 || self.result.max_bytes > 20 * 1024 * 1024 {
            return Err(RunnerError::InvalidOutputLimit);
        }
        if self.result.success_exit_codes.is_empty() {
            return Err(RunnerError::MissingSuccessExitCode);
        }
        let mut names = BTreeSet::new();
        for name in &self.launch.env_allow {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                || !names.insert(name)
            {
                return Err(RunnerError::InvalidEnvironmentName(name.clone()));
            }
        }
        Ok(())
    }
}

struct Substitutions<'a> {
    prompt_file: &'a Path,
    prompt: &'a str,
    workspace: &'a Path,
    model: Option<&'a str>,
    effort: Option<&'a str>,
}

fn render_argv(
    manifest: &HarnessManifest,
    request: &RunRequest<'_>,
    values: &Substitutions<'_>,
) -> Result<Vec<String>, RunnerError> {
    let mut arguments = manifest.launch.argv.clone();
    if request.model.is_some() {
        arguments.extend(manifest.launch.model_argv.clone());
    }
    if request.effort.is_some() {
        arguments.extend(manifest.launch.effort_argv.clone());
    }
    arguments
        .iter()
        .map(|argument| substitute(argument, values))
        .collect()
}

async fn wait_for_exit(
    child: &mut tokio::process::Child,
    started: Instant,
    deadline: Duration,
    cancel_path: Option<&Path>,
) -> Result<(Option<ExitStatus>, bool, bool), RunnerError> {
    loop {
        let cancelled = cancel_path.is_some_and(Path::exists);
        let timed_out = started.elapsed() >= deadline;
        if cancelled || timed_out {
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
        .status()?;
    if !term.success() {
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

fn substitute(argument: &str, values: &Substitutions<'_>) -> Result<String, RunnerError> {
    // A prompt is one opaque argv value, never a template or shell fragment.
    if argument == "${input.prompt}" {
        return Ok(values.prompt.to_owned());
    }
    let mut output = argument
        .replace(
            "${input.prompt_file}",
            &values.prompt_file.to_string_lossy(),
        )
        .replace("${task.workspace}", &values.workspace.to_string_lossy());
    for (placeholder, value) in [
        ("${route.model}", values.model),
        ("${route.effort}", values.effort),
    ] {
        if output.contains(placeholder) {
            output = output.replace(
                placeholder,
                value.ok_or_else(|| RunnerError::MissingSubstitution(placeholder.to_owned()))?,
            );
        }
    }
    if output.contains("${") {
        return Err(RunnerError::UnknownSubstitution(output));
    }
    Ok(output)
}

fn collect_result(
    manifest: &HarnessManifest,
    workspace: &Path,
    values: &Substitutions<'_>,
    stdout: &[u8],
) -> Result<Vec<u8>, RunnerError> {
    match &manifest.result.source {
        ResultSource::Stdout => Ok(stdout.to_vec()),
        ResultSource::JsonlAssistantFinal => extract_jsonl_assistant_final(stdout),
        ResultSource::File { path } => {
            let rendered = substitute(path, values)?;
            let relative = Path::new(&rendered);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(RunnerError::ResultPathOutsideWorkspace(rendered));
            }
            let result_path = workspace.join(relative);
            let metadata = std::fs::symlink_metadata(&result_path)?;
            if !metadata.file_type().is_file() {
                return Err(RunnerError::ResultNotRegularFile(result_path));
            }
            if metadata.len() > manifest.result.max_bytes {
                return Err(RunnerError::ResultTooLarge {
                    max_bytes: manifest.result.max_bytes,
                    observed_bytes: metadata.len(),
                });
            }
            let mut file = std::fs::File::open(&result_path)?;
            if !file.metadata()?.is_file() {
                return Err(RunnerError::ResultNotRegularFile(result_path));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(manifest.result.max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > manifest.result.max_bytes {
                return Err(RunnerError::ResultTooLarge {
                    max_bytes: manifest.result.max_bytes,
                    observed_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                });
            }
            Ok(bytes)
        }
    }
}

fn extract_jsonl_assistant_final(stdout: &[u8]) -> Result<Vec<u8>, RunnerError> {
    let mut final_text = None;
    let mut completed = false;
    let mut assistant_turn_ended = false;
    let mut unqualified_agent_end = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_end")
                if event
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("assistant") =>
            {
                let parts = event
                    .pointer("/message/content")
                    .and_then(serde_json::Value::as_array)
                    .ok_or(RunnerError::MissingAssistantText)?;
                let text = parts
                    .iter()
                    .filter(|part| {
                        part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    final_text = Some(text.into_bytes());
                }
            }
            Some("agent_end") => {
                match event.get("stopReason").and_then(serde_json::Value::as_str) {
                    Some("completed") => completed = true,
                    None => unqualified_agent_end = true,
                    _ => {}
                }
            }
            Some("turn_end")
                if event
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("assistant") =>
            {
                assistant_turn_ended = true;
            }
            _ => {}
        }
    }
    if !(completed || (unqualified_agent_end && assistant_turn_ended)) {
        return Err(RunnerError::MissingTerminalEvent);
    }
    final_text.ok_or(RunnerError::MissingAssistantText)
}

async fn read_bounded<R>(reader: R, limit: u64) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(limit + CAPTURE_OVERHEAD_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    let truncated = bytes.len() as u64 > limit;
    if truncated {
        bytes.truncate(usize::try_from(limit).expect("validated output limit fits usize"));
    }
    Ok((bytes, truncated))
}

fn join_capture(
    result: Result<Result<(Vec<u8>, bool), std::io::Error>, JoinError>,
) -> Result<(Vec<u8>, bool), RunnerError> {
    result
        .map_err(RunnerError::CaptureTask)?
        .map_err(RunnerError::Io)
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("child process could not start: {0}")]
    SpawnIo(std::io::Error),
    #[error("unsupported manifest schema: {0}")]
    UnsupportedSchema(String),
    #[error("unsupported adapter: {0}")]
    UnsupportedAdapter(String),
    #[error("task requested harness {requested}, but manifest is {actual}")]
    HarnessRouteMismatch { requested: String, actual: String },
    #[error("harness does not support required capability: {0}")]
    UnsupportedCapability(String),
    #[error("invalid harness id: {0}")]
    InvalidHarnessId(String),
    #[error("executable must be an existing absolute file: {}", .0.display())]
    InvalidExecutable(PathBuf),
    #[error("workspace must be an existing directory: {}", .0.display())]
    InvalidWorkspace(PathBuf),
    #[error("arguments must not contain NUL bytes")]
    InvalidArgument,
    #[error("result max_bytes must be between 1 and 20 MiB")]
    InvalidOutputLimit,
    #[error("at least one success exit code is required")]
    MissingSuccessExitCode,
    #[error("invalid or duplicate environment name: {0}")]
    InvalidEnvironmentName(String),
    #[error("required substitution is missing: {0}")]
    MissingSubstitution(String),
    #[error("unknown substitution in argument: {0}")]
    UnknownSubstitution(String),
    #[error("result path must stay relative to the task workspace: {0}")]
    ResultPathOutsideWorkspace(String),
    #[error("result is not a regular file: {}", .0.display())]
    ResultNotRegularFile(PathBuf),
    #[error("result exceeds {max_bytes} bytes (observed {observed_bytes})")]
    ResultTooLarge { max_bytes: u64, observed_bytes: u64 },
    #[error("JSONL output has no completed agent_end event")]
    MissingTerminalEvent,
    #[error("JSONL output has no final assistant text")]
    MissingAssistantText,
    #[error("JSONL output is malformed: {0}")]
    MalformedJsonl(#[from] serde_json::Error),
    #[error("child process did not expose its {0} pipe")]
    MissingPipe(&'static str),
    #[error("child process did not expose a process id")]
    MissingProcessId,
    #[error("capture task failed")]
    CaptureTask(#[source] JoinError),
    #[error("process output capture exceeded the total attempt deadline")]
    CaptureDeadlineElapsed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{ArtifactContract, AttemptBudget, OwnerId, Route, SCHEMA_V1, TaskId};
    use std::os::unix::fs::PermissionsExt;

    fn echo_manifest(max_bytes: u64) -> HarnessManifest {
        HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "test.echo".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: PathBuf::from("/bin/echo"),
            probe: ProbeSpec {
                version_argv: vec!["--version".to_owned()],
                help_argv: vec!["--help".to_owned()],
            },
            launch: LaunchSpec {
                argv: vec!["@${input.prompt_file}".to_owned()],
                model_argv: vec![],
                effort_argv: vec![],
                env_allow: vec![],
                mode: ExecutionMode::OneShot,
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
    fn literal_prompt_is_one_argv_value_even_with_template_like_text() {
        let workspace = tempfile::tempdir().unwrap();
        let prompt = "Do not expand ${route.model} or $(touch /tmp/never)";
        let request = RunRequest {
            workspace: workspace.path(),
            prompt,
            model: None,
            effort: None,
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
    fn unsupported_model_is_rejected_before_process_execution() {
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
        let task = TaskSpec {
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
        };
        assert!(matches!(
            manifest.validate_task_route(&task),
            Err(RunnerError::UnsupportedCapability(name)) if name == "model_select"
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
                model: None,
                effort: None,
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
                model: None,
                effort: None,
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
                model: None,
                effort: None,
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
                model: None,
                effort: None,
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
}
