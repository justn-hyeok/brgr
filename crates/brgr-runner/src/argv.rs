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
    // Most CLIs take it as a bare positional or an option value, so one that
    // starts with `-` (a markdown list) would parse as flags and one that
    // starts with `@` is a file include to Pi; a leading space keeps it text.
    if argument == "${input.prompt}" {
        return Ok(if values.prompt.starts_with(['-', '@']) {
            format!(" {}", values.prompt)
        } else {
            values.prompt.to_owned()
        });
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

#[cfg(test)]
mod prompt_tests {
    use super::*;

    fn rendered(prompt: &str) -> String {
        substitute(
            "${input.prompt}",
            &Substitutions {
                prompt_file: Path::new("/tmp/prompt"),
                prompt,
                workspace: Path::new("/tmp"),
                model: None,
                effort: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_prompt_that_looks_like_an_option_or_include_stays_text() {
        assert_eq!(
            rendered("- add tests\n- fix lint"),
            " - add tests\n- fix lint"
        );
        assert_eq!(rendered("--help me"), " --help me");
        assert_eq!(rendered("@src/main.rs fix it"), " @src/main.rs fix it");
        assert_eq!(rendered("Fix the bug"), "Fix the bug");
    }
}
