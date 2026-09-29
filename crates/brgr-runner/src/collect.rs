//! Collecting a run's final result from stdout, a file, or JSONL events.

use std::{io::Read as _, os::unix::fs::MetadataExt as _, path::Path, process::ExitStatus};

use crate::{
    HarnessManifest, ResultSource, RunRequest, RunnerError,
    argv::{Substitutions, substitute},
};

pub(crate) fn can_collect_result(
    status: Option<&ExitStatus>,
    manifest: &HarnessManifest,
    cancelled: bool,
    timed_out: bool,
    truncated: bool,
) -> bool {
    !cancelled
        && !timed_out
        && !truncated
        && status
            .and_then(ExitStatus::code)
            .is_some_and(|code| manifest.result.success_exit_codes.contains(&code))
}

pub(crate) fn collect_result(
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
            if !result_path
                .canonicalize()?
                .starts_with(workspace.canonicalize()?)
            {
                return Err(RunnerError::ResultPathOutsideWorkspace(rendered));
            }
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
            let opened = file.metadata()?;
            if !opened.is_file()
                || (metadata.dev(), metadata.ino(), metadata.len())
                    != (opened.dev(), opened.ino(), opened.len())
            {
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

pub(crate) fn collect_result_with_model(
    manifest: &HarnessManifest,
    request: &RunRequest<'_>,
    values: &Substitutions<'_>,
    stdout: &[u8],
) -> Result<(Vec<u8>, Option<String>), RunnerError> {
    let result = collect_result(manifest, request.workspace, values, stdout)?;
    if u64::try_from(result.len()).unwrap_or(u64::MAX) > manifest.result.max_bytes {
        return Err(RunnerError::ResultTooLarge {
            max_bytes: manifest.result.max_bytes,
            observed_bytes: u64::try_from(result.len()).unwrap_or(u64::MAX),
        });
    }
    let observed_model =
        if !result.is_empty() && manifest.result.source == ResultSource::JsonlAssistantFinal {
            observe_jsonl_model(stdout, request.model)?
        } else {
            None
        };
    Ok((result, observed_model))
}

pub(crate) fn extract_jsonl_assistant_final(stdout: &[u8]) -> Result<Vec<u8>, RunnerError> {
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

pub(crate) fn observe_jsonl_model(
    stdout: &[u8],
    requested: Option<&str>,
) -> Result<Option<String>, RunnerError> {
    let mut observed: Option<String> = None;
    let mut missing_identity = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("message_end")
            || event
                .pointer("/message/role")
                .and_then(serde_json::Value::as_str)
                != Some("assistant")
        {
            continue;
        }
        let identity = event
            .pointer("/message/provider")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .zip(
                event
                    .pointer("/message/model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty()),
            )
            .map(|(provider, model)| format!("{provider}/{model}"));
        match (&observed, identity) {
            (Some(previous), Some(current)) if previous != &current => {
                return Err(RunnerError::MixedObservedModels);
            }
            (None, Some(current)) => observed = Some(current),
            (_, None) => missing_identity = true,
            _ => {}
        }
    }
    if missing_identity {
        return if requested.is_some() || observed.is_some() {
            Err(RunnerError::ObservedModelUnavailable)
        } else {
            Ok(None)
        };
    }
    if let Some(requested) = requested {
        let actual = observed
            .as_deref()
            .ok_or(RunnerError::ObservedModelUnavailable)?;
        if actual != requested {
            return Err(RunnerError::ObservedModelMismatch {
                requested: requested.to_owned(),
                observed: actual.to_owned(),
            });
        }
    }
    Ok(observed)
}
