//! Placeholder substitution and argv rendering for a manifest launch.

use std::path::Path;

use crate::{HarnessManifest, RunRequest, RunnerError};

pub(crate) struct Substitutions<'a> {
    pub(crate) prompt_file: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) workspace: &'a Path,
    pub(crate) model: Option<&'a str>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn render_argv(
    manifest: &HarnessManifest,
    request: &RunRequest<'_>,
    values: &Substitutions<'_>,
) -> Result<Vec<String>, RunnerError> {
    let mut arguments = manifest.launch.argv.clone();
    arguments.extend_from_slice(manifest.permission_arguments(request.permission)?);
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

pub(crate) fn substitute(
    argument: &str,
    values: &Substitutions<'_>,
) -> Result<String, RunnerError> {
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
