//! Sealing the evidence a task requested: logs, files, and the Git diff.

use std::{io::Cursor, path::Path, time::Duration};

use brgr_protocol::{ArtifactRef, TaskSpec};
use brgr_runner::ExecutionOutput;
use brgr_store::Store;

use crate::git_diff;

/// The patch `--capture-diff` seals for a task revision, computed from its
/// workspace now. Sealing and worktree removal both use it, so a worktree whose
/// patch still equals the sealed one holds nothing the store lacks.
///
/// # Errors
///
/// Returns an error when git fails, the patch exceeds `limit` bytes, or the
/// diff outlasts `remaining`.
pub fn task_patch(spec: &TaskSpec, limit: u64, remaining: Duration) -> Result<Vec<u8>, String> {
    let mut excluded = spec.evidence.files.clone();
    excluded.push(format!(".brgr/tasks/{}-r{}", spec.task_id, spec.revision));
    git_diff::bounded_git_diff(
        Path::new(&spec.workspace),
        spec.evidence
            .base_tree
            .as_deref()
            .or(spec.evidence.base_commit.as_deref())
            .unwrap_or("HEAD"),
        &excluded,
        limit,
        remaining,
    )
}

pub(crate) fn seal_requested_evidence(
    store: &Store,
    spec: &TaskSpec,
    output: &ExecutionOutput,
) -> Result<Vec<ArtifactRef>, String> {
    if spec.evidence.is_empty() {
        return Ok(Vec::new());
    }
    let limit = spec.artifact_contract.max_bytes.min(8 * 1024 * 1024);
    let remaining =
        Duration::from_secs(spec.budget.deadline_seconds).saturating_sub(output.elapsed);
    if remaining.is_zero() {
        return Err("evidence collection exceeded the attempt deadline".to_owned());
    }
    let mut artifacts = Vec::new();
    let mut total = 0_u64;
    if spec.evidence.capture_diff {
        let patch = task_patch(spec, limit, remaining)?;
        let reference = store
            .seal_artifact_reader(Cursor::new(patch), "text/x-diff", limit)
            .map_err(|error| error.to_string())?;
        push_evidence(&mut artifacts, &mut total, reference)?;
    }
    for reference in seal_requested_logs(store, spec, output)? {
        push_evidence(&mut artifacts, &mut total, reference)?;
    }
    let workspace = Path::new(&spec.workspace)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    for relative in &spec.evidence.files {
        let path = workspace.join(relative);
        let canonical = path.canonicalize().map_err(|error| error.to_string())?;
        if !canonical.starts_with(&workspace) {
            return Err(format!(
                "requested evidence leaves the task workspace: {relative}"
            ));
        }
        let media_type = evidence_media_type(Path::new(relative));
        let reference = store
            .seal_artifact_path(&path, media_type, limit)
            .map_err(|error| error.to_string())?;
        push_evidence(&mut artifacts, &mut total, reference)?;
    }
    Ok(artifacts)
}

pub(crate) fn seal_requested_logs(
    store: &Store,
    spec: &TaskSpec,
    output: &ExecutionOutput,
) -> Result<Vec<ArtifactRef>, String> {
    if !spec.evidence.capture_logs {
        return Ok(Vec::new());
    }
    let limit = spec.artifact_contract.max_bytes.min(8 * 1024 * 1024);
    let mut artifacts = Vec::new();
    let mut total = 0_u64;
    for (bytes, media_type) in [
        (&output.stdout, "text/x-brgr-stdout"),
        (&output.stderr, "text/x-brgr-stderr"),
    ] {
        if !bytes.is_empty() {
            let reference = store
                .seal_artifact_reader(Cursor::new(bytes), media_type, limit)
                .map_err(|error| error.to_string())?;
            push_evidence(&mut artifacts, &mut total, reference)?;
        }
    }
    Ok(artifacts)
}

pub(crate) fn push_evidence(
    artifacts: &mut Vec<ArtifactRef>,
    total: &mut u64,
    reference: ArtifactRef,
) -> Result<(), String> {
    *total = total
        .checked_add(reference.bytes)
        .ok_or("evidence size overflow")?;
    if *total > 32 * 1024 * 1024 {
        return Err("requested evidence exceeds 32 MiB in total".to_owned());
    }
    artifacts.push(reference);
    Ok(())
}

pub(crate) fn evidence_media_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("diff" | "patch") => "text/x-diff",
        Some("log" | "txt") => "text/plain",
        _ => "application/octet-stream",
    }
}
