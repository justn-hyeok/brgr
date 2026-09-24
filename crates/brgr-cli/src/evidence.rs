use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{DecisionVerdict, TaskId, TerminalOutcome};
use brgr_store::Store;
use serde_json::json;
use tempfile::NamedTempFile;

use crate::{
    ArtifactCommand, Paths, print_value, require_owner, workspace, write_json_atomic,
    write_json_new,
};

pub fn artifact_command(paths: &Paths, command: ArtifactCommand, json_output: bool) -> Result<()> {
    match command {
        ArtifactCommand::Export {
            task,
            index,
            output,
        } => export_artifact(paths, task, index, &output, json_output),
    }
}

fn export_artifact(
    paths: &Paths,
    task: TaskId,
    index: usize,
    output: &Path,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    let reference = result
        .artifacts
        .get(index)
        .context("artifact index is out of range")?;
    let bytes = store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
    if output.exists() {
        bail!(
            "artifact export will not replace an existing file: {}",
            output.display()
        );
    }
    let parent = output
        .parent()
        .context("artifact output has no parent directory")?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.error)?;
    print_value(
        &json!({"task_id": task, "index": index, "output": output, "digest": reference.digest,
            "bytes": reference.bytes, "media_type": reference.media_type}),
        json_output,
    );
    Ok(())
}

pub fn apply_result(
    paths: &Paths,
    task: TaskId,
    workspace_path: &Path,
    execute: bool,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    if result.outcome != TerminalOutcome::Candidate || !spec.evidence.capture_diff {
        bail!("only a candidate with a requested sealed Git diff can be integrated");
    }
    let reference = result
        .artifacts
        .get(1)
        .filter(|artifact| artifact.media_type == "text/x-diff")
        .context("result has no sealed Git diff")?;
    let patch = store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
    let target = workspace_path.canonicalize()?;
    let repository_root = git_value(&target, &["rev-parse", "--show-toplevel"])?.canonicalize()?;
    if target != repository_root {
        bail!("integration target must be the repository root, not a subdirectory");
    }
    let task_workspace = Path::new(&spec.workspace).canonicalize()?;
    let expected_head = spec.evidence.base_commit.as_ref().map_or_else(
        || git_value(&task_workspace, &["rev-parse", "HEAD"]),
        |commit| Ok(PathBuf::from(commit)),
    )?;
    if git_value(
        &target,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )? != git_value(
        &task_workspace,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )? || git_value(&target, &["rev-parse", "HEAD"])? != expected_head
    {
        bail!("integration target must be the same repository at the task base commit");
    }
    let _admission = workspace::acquire_admission_lock(&paths.worktrees, &target)?;
    git_apply(&target, &patch, true)?;
    let status = if execute {
        let decision = store
            .decision_for_result(result.result_id)?
            .context("result has no owner decision")?;
        if decision.verdict != DecisionVerdict::Accepted {
            bail!("owner must accept the sealed result before integration");
        }
        let receipt = paths
            .runs
            .join(format!("{}-{}.apply.json", task, result.result_id));
        let intent = json!({"task_id": task, "result_id": result.result_id,
            "target": target, "patch_digest": reference.digest, "state": "started"});
        write_json_new(&receipt, &intent)
            .context("an integration receipt already exists or could not be written")?;
        git_apply(&target, &patch, false)?;
        write_json_atomic(
            &receipt,
            &json!({"task_id": task, "result_id": result.result_id, "target": target,
                "patch_digest": reference.digest, "state": "applied"}),
        )?;
        "applied"
    } else {
        "ready"
    };
    print_value(
        &json!({"task_id": task, "result_id": result.result_id,
            "target": target, "patch_digest": reference.digest, "status": status}),
        json_output,
    );
    Ok(())
}

fn git_value(workspace: &Path, argv: &[&str]) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(argv)
        .output()?;
    if !output.status.success() {
        bail!("integration workspace is not a readable Git checkout");
    }
    let value = String::from_utf8(output.stdout)?;
    let value = value.trim();
    if argv.last() == Some(&"--git-common-dir") {
        Ok(PathBuf::from(value).canonicalize()?)
    } else {
        Ok(PathBuf::from(value))
    }
}

fn git_apply(workspace: &Path, patch: &[u8], check_only: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace).args(["apply", "--binary"]);
    if check_only {
        command.arg("--check");
    }
    let mut child = command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let write = child
        .stdin
        .take()
        .context("Git apply input is unavailable")?
        .write_all(patch);
    let output = child.wait_with_output()?;
    write?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Git apply conflict or failure: {}",
            diagnostic.chars().take(2_048).collect::<String>()
        );
    }
    Ok(())
}
