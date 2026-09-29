//! Typed runner failures.

use std::path::PathBuf;

use brgr_protocol::PermissionLevel;
use thiserror::Error;
use tokio::task::JoinError;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("harness does not support permission level {0:?}; it will not run under a wider one")]
    UnsupportedPermission(PermissionLevel),
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
    #[error("model catalog manifest is malformed")]
    InvalidModelCatalog,
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
    #[error("JSONL assistant model identity is unavailable")]
    ObservedModelUnavailable,
    #[error("JSONL assistant model changed during one run")]
    MixedObservedModels,
    #[error("JSONL assistant used {observed}, not requested model {requested}")]
    ObservedModelMismatch { requested: String, observed: String },
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
