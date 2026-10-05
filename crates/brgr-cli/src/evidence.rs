use std::{
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{
    ArtifactRef, DecisionVerdict, ResultEnvelope, TaskId, TaskSpec, TerminalOutcome,
};
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
    if result.outcome != TerminalOutcome::Candidate {
        bail!("only a candidate with a requested sealed Git diff can be integrated");
    }
    let (reference, patch) = sealed_patch(&store, &spec, &result)?;
    let reference = &reference;
    let target = workspace_path.canonicalize()?;
    let repository_root = git_value(&target, &["rev-parse", "--show-toplevel"])?.canonicalize()?;
    if target != repository_root {
        bail!("integration target must be the repository root, not a subdirectory");
    }
    // The worktree is removed once the result is decided and nothing in it
    // would be lost, so the sealed patch, the recorded base commit, and the
    // checkout recorded at admission are what integration relies on.
    let worktree = Path::new(&spec.workspace).canonicalize().ok();
    let task_workspace = match &worktree {
        Some(worktree) => worktree.clone(),
        None => store
            .task_checkout(task, spec.revision)?
            .map(PathBuf::from)
            .and_then(|checkout| checkout.canonicalize().ok())
            .context("the task's worktree was removed and its repository was not recorded")?,
    };
    let expected_head = match (&spec.evidence.base_commit, &worktree) {
        (Some(commit), _) => PathBuf::from(commit),
        (None, Some(worktree)) => git_value(worktree, &["rev-parse", "HEAD"])?,
        (None, None) => {
            bail!("the task's base commit was not recorded and its worktree was removed")
        }
    };
    let target_head = git_value(&target, &["rev-parse", "HEAD"])?;
    if git_value(
        &target,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )? != git_value(
        &task_workspace,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )? || !(target_head == expected_head || is_ancestor(&target, &expected_head, &target_head)?)
    {
        bail!(
            "integration target must be the same repository at the task base commit or a commit that descends from it"
        );
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
            "target": target, "patch_digest": reference.digest, "status": status,
            "base_commit": expected_head, "target_head": target_head}),
        json_output,
    );
    Ok(())
}

/// Prints a result's sealed Git diff, or with `stat` the files it changes.
pub fn show_diff(paths: &Paths, task: TaskId, stat: bool, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    let (reference, patch) = sealed_patch(&store, &spec, &result)?;
    if !stat && !json_output {
        io::stdout().write_all(&patch)?;
        return Ok(());
    }
    let files = numstat(&patch)?;
    if !json_output {
        let (mut added, mut deleted) = (0_u64, 0_u64);
        for file in &files {
            match (file.added, file.deleted) {
                (Some(plus), Some(minus)) => {
                    added += plus;
                    deleted += minus;
                    println!("+{plus:<6} -{minus:<6} {}", file.path);
                }
                _ => println!("{:<15} {}", "binary", file.path),
            }
        }
        let noun = if files.len() == 1 { "file" } else { "files" };
        println!("{} {noun}, +{added} -{deleted}", files.len());
        return Ok(());
    }
    let files: Vec<_> = files
        .iter()
        .map(|file| json!({"path": file.path, "added": file.added, "deleted": file.deleted}))
        .collect();
    let mut value = json!({"task_id": task, "result_id": result.result_id,
        "outcome": result.outcome, "patch_digest": reference.digest,
        "bytes": reference.bytes, "files": files});
    if !stat {
        value["patch"] = String::from_utf8(patch)
            .context("the sealed diff is not UTF-8; read it without --json")?
            .into();
    }
    print_value(&value, json_output);
    Ok(())
}

/// The sealed diff of a result whose task asked for one.
fn sealed_patch(
    store: &Store,
    spec: &TaskSpec,
    result: &ResultEnvelope,
) -> Result<(ArtifactRef, Vec<u8>)> {
    if !spec.evidence.capture_diff {
        bail!("the task did not request a sealed Git diff; run it with --capture-diff");
    }
    let reference = result
        .artifacts
        .get(1)
        .filter(|artifact| artifact.media_type == "text/x-diff")
        .context("result has no sealed Git diff")?;
    let patch = store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
    Ok((reference.clone(), patch))
}

struct FileStat {
    path: String,
    /// `None` for a binary file.
    added: Option<u64>,
    deleted: Option<u64>,
}

/// Per-file line counts, read by `git apply --numstat` outside any repository
/// so no repository configuration takes part.
fn numstat(patch: &[u8]) -> Result<Vec<FileStat>> {
    if patch.is_empty() {
        return Ok(Vec::new());
    }
    let scratch = tempfile::tempdir()?;
    let mut child = Command::new("git")
        .args(["apply", "--numstat", "-z", "-"])
        .current_dir(scratch.path())
        .env("GIT_CEILING_DIRECTORIES", scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .context("Git numstat input is unavailable")?;
    let input = patch.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if !output.status.success() {
        bail!("the sealed diff could not be summarized");
    }
    parse_numstat(&output.stdout)
}

/// Parses `--numstat -z` records: `ADDED\tDELETED\tPATH\0`, or for a rename
/// `ADDED\tDELETED\t\0OLD\0NEW\0`.
fn parse_numstat(bytes: &[u8]) -> Result<Vec<FileStat>> {
    let mut fields = bytes.split(|byte| *byte == 0).map(String::from_utf8_lossy);
    let mut files = Vec::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            bail!("Git numstat output is malformed");
        };
        let path = if path.is_empty() {
            let from = fields.next().context("Git numstat rename is incomplete")?;
            let to = fields.next().context("Git numstat rename is incomplete")?;
            format!("{from} => {to}")
        } else {
            path.to_owned()
        };
        files.push(FileStat {
            path,
            added: added.parse().ok(),
            deleted: deleted.parse().ok(),
        });
    }
    Ok(files)
}

fn is_ancestor(workspace: &Path, ancestor: &Path, descendant: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["merge-base", "--is-ancestor"])
        .arg(ancestor)
        .arg(descendant)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("integration workspace is not a readable Git checkout"),
    }
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
    if patch.is_empty() {
        return Ok(());
    }
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
