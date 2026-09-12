//! Shell-free process execution and declarative harness manifests.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinError,
    time::timeout,
};

pub const MANIFEST_SCHEMA_V1: &str = "brgr.harness/v1";
pub const PROCESS_ADAPTER_V1: &str = "process/v1";
const CAPTURE_OVERHEAD_BYTES: u64 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
pub struct ProbeSpec {
    pub version_argv: Vec<String>,
    pub help_argv: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
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
    File { path: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
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
            workspace: request.workspace,
            model: request.model,
            effort: request.effort,
        };
        let argv = manifest
            .launch
            .argv
            .iter()
            .map(|argument| substitute(argument, &substitutions))
            .collect::<Result<Vec<_>, _>>()?;

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
        for name in &manifest.launch.env_allow {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }

        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or(RunnerError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(RunnerError::MissingPipe("stderr"))?;
        let limit = manifest.result.max_bytes;
        let stdout_task = tokio::spawn(read_bounded(stdout, limit));
        let stderr_task = tokio::spawn(read_bounded(stderr, limit));

        let (status, timed_out) = if let Ok(status) = timeout(request.deadline, child.wait()).await
        {
            (Some(status?), false)
        } else {
            child.start_kill()?;
            let status = child.wait().await?;
            (Some(status), true)
        };
        let (stdout, stdout_truncated) = join_capture(stdout_task.await)?;
        let (stderr, stderr_truncated) = join_capture(stderr_task.await)?;

        Ok(ExecutionOutput {
            exit_code: status.and_then(|value| value.code()),
            stdout,
            stderr,
            timed_out,
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
            },
        )
        .await
    }
}

impl HarnessManifest {
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
        if self.adapter != PROCESS_ADAPTER_V1 {
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
    workspace: &'a Path,
    model: Option<&'a str>,
    effort: Option<&'a str>,
}

fn substitute(argument: &str, values: &Substitutions<'_>) -> Result<String, RunnerError> {
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
    #[error("unsupported manifest schema: {0}")]
    UnsupportedSchema(String),
    #[error("unsupported adapter: {0}")]
    UnsupportedAdapter(String),
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
    #[error("child process did not expose its {0} pipe")]
    MissingPipe(&'static str),
    #[error("capture task failed")]
    CaptureTask(#[source] JoinError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
        )
        .await
        .unwrap();

        assert!(output.output_truncated);
        assert!(!output.succeeded(&manifest));
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
}
