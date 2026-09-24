//! Git-backed task worktree creation. Fail closed; never delete or reset.

use std::{
    collections::BTreeSet,
    env,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read as _, Write},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use brgr_protocol::TaskId;
use brgr_runner::OMP_ROLE_ADAPTER_V1;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, tempdir_in};

const GIT_LOCK_ATTEMPTS: u32 = 8;
const ADMISSION_LOCK_WAIT: Duration = Duration::from_secs(10);
const MAX_SELECTED_FILES: usize = 32;
const MAX_SELECTED_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SELECTED_TOTAL_BYTES: u64 = 20 * 1024 * 1024;

pub(crate) struct SelectedSnapshot {
    source: PathBuf,
    base_revision: String,
    files: Vec<SelectedFile>,
}

struct SelectedFile {
    relative: PathBuf,
    git_relative: PathBuf,
    action: SelectedAction,
}

enum SelectedAction {
    Copy { contents: Vec<u8>, mode: u32 },
    Delete,
}

#[derive(Serialize)]
pub(crate) struct SnapshotReceipt {
    task_id: TaskId,
    revision: u32,
    source: String,
    base_revision: String,
    entries: Vec<SnapshotEntry>,
}

#[derive(Serialize)]
struct SnapshotEntry {
    path: String,
    action: &'static str,
    bytes: u64,
    digest: Option<String>,
}

impl SelectedSnapshot {
    pub(crate) fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub(crate) fn receipt(&self, task_id: TaskId, revision: u32) -> SnapshotReceipt {
        let entries = self
            .files
            .iter()
            .map(|file| {
                let (action, bytes, digest) = match &file.action {
                    SelectedAction::Copy { contents, .. } => (
                        "copy",
                        u64::try_from(contents.len()).unwrap_or(u64::MAX),
                        Some(sha256_bytes(contents)),
                    ),
                    SelectedAction::Delete => ("delete", 0, None),
                };
                SnapshotEntry {
                    path: file.relative.to_string_lossy().into_owned(),
                    action,
                    bytes,
                    digest,
                }
            })
            .collect();
        SnapshotReceipt {
            task_id,
            revision,
            source: self.source.to_string_lossy().into_owned(),
            base_revision: self.base_revision.clone(),
            entries,
        }
    }
}

pub(crate) fn read_selected_snapshot(
    source: &Path,
    selected: &[PathBuf],
) -> Result<SelectedSnapshot> {
    if selected.is_empty() || selected.len() > MAX_SELECTED_FILES {
        bail!("select between 1 and {MAX_SELECTED_FILES} changed files");
    }
    let source = source.canonicalize()?;
    let root_output = Command::new("git")
        .args(["-C", &lossy(&source), "rev-parse", "--show-toplevel"])
        .output()?;
    if !root_output.status.success() {
        bail!("selected dirty snapshots require a Git workspace");
    }
    let root = PathBuf::from(String::from_utf8(root_output.stdout)?.trim()).canonicalize()?;
    let base_revision = git_stdout(&root, &["rev-parse", "HEAD"])?;
    let mut seen = BTreeSet::new();
    let mut total_bytes = 0_u64;
    let mut files = Vec::with_capacity(selected.len());
    for relative in selected {
        validate_selected_path(relative)?;
        if !seen.insert(relative.clone()) {
            bail!(
                "selected snapshot contains a duplicate path: {}",
                relative.display()
            );
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["status", "--porcelain=v1", "--untracked-files=all", "--"])
            .arg(relative)
            .output()?;
        if !status.status.success() || status.stdout.is_empty() {
            bail!(
                "selected path is not an uncommitted change: {}",
                relative.display()
            );
        }
        let action = read_selected_file(&source, &root, relative, &mut total_bytes)?;
        let root_relative = source.strip_prefix(&root)?.join(relative);
        files.push(SelectedFile {
            relative: relative.clone(),
            git_relative: root_relative,
            action,
        });
    }
    Ok(SelectedSnapshot {
        source,
        base_revision: base_revision.trim().to_owned(),
        files,
    })
}

fn validate_selected_path(relative: &Path) -> Result<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(name) if name != ".git"))
    {
        bail!("snapshot paths must be relative files below the source workspace");
    }
    Ok(())
}

fn read_selected_file(
    source: &Path,
    root: &Path,
    relative: &Path,
    total_bytes: &mut u64,
) -> Result<SelectedAction> {
    let path = source.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(before) if before.file_type().is_file() => {
            if !path.canonicalize()?.starts_with(source) || before.len() > MAX_SELECTED_FILE_BYTES {
                bail!(
                    "selected file leaves its source or exceeds 8 MiB: {}",
                    relative.display()
                );
            }
            *total_bytes = total_bytes
                .checked_add(before.len())
                .context("selected snapshot size overflow")?;
            if *total_bytes > MAX_SELECTED_TOTAL_BYTES {
                bail!("selected snapshot exceeds 20 MiB");
            }
            let file = File::open(&path)?;
            let opened = file.metadata()?;
            if before.dev() != opened.dev() || before.ino() != opened.ino() {
                bail!(
                    "selected file changed while being opened: {}",
                    relative.display()
                );
            }
            let mut contents = Vec::new();
            file.take(MAX_SELECTED_FILE_BYTES + 1)
                .read_to_end(&mut contents)?;
            let after = fs::symlink_metadata(&path)?;
            if contents.len() as u64 != before.len()
                || before.dev() != after.dev()
                || before.ino() != after.ino()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
            {
                bail!(
                    "selected file changed while being captured: {}",
                    relative.display()
                );
            }
            Ok(SelectedAction::Copy {
                contents,
                mode: before.permissions().mode() & 0o777,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let root_relative = source.strip_prefix(root)?.join(relative);
            let old = format!("HEAD:{}", root_relative.to_string_lossy());
            let tracked = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["cat-file", "-e", &old])
                .status()?;
            if !tracked.success() {
                bail!(
                    "selected missing file is not a tracked deletion: {}",
                    relative.display()
                );
            }
            Ok(SelectedAction::Delete)
        }
        Ok(_) => bail!(
            "selected path is not a regular file: {}",
            relative.display()
        ),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn apply_selected_snapshot(target: &Path, snapshot: &SelectedSnapshot) -> Result<()> {
    let target = target.canonicalize()?;
    for file in &snapshot.files {
        let mut parent = target.clone();
        if let Some(components) = file.relative.parent() {
            for component in components.components() {
                parent.push(component.as_os_str());
                match fs::symlink_metadata(&parent) {
                    Ok(metadata) if metadata.file_type().is_dir() => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&parent)?;
                    }
                    _ => bail!("snapshot target contains an unsafe parent directory"),
                }
            }
        }
        if !parent.canonicalize()?.starts_with(&target) {
            bail!("snapshot target leaves its task worktree");
        }
        let destination = target.join(&file.relative);
        match &file.action {
            SelectedAction::Copy { contents, mode } => {
                if let Ok(metadata) = fs::symlink_metadata(&destination)
                    && !metadata.file_type().is_file()
                {
                    bail!(
                        "snapshot target is not a regular file: {}",
                        file.relative.display()
                    );
                }
                let mut temporary = NamedTempFile::new_in(&parent)?;
                temporary.write_all(contents)?;
                temporary.as_file_mut().sync_all()?;
                temporary
                    .as_file_mut()
                    .set_permissions(fs::Permissions::from_mode(*mode))?;
                temporary.persist(&destination)?;
            }
            SelectedAction::Delete => {
                let metadata = fs::symlink_metadata(&destination)?;
                if !metadata.file_type().is_file() {
                    bail!(
                        "snapshot deletion target is not regular: {}",
                        file.relative.display()
                    );
                }
                fs::remove_file(&destination)?;
            }
        }
    }
    Ok(())
}

/// Records the selected input state as a Git tree without changing either
/// checkout's index. Worker diffs use this tree instead of HEAD.
pub(crate) fn selected_snapshot_tree(
    target: &Path,
    snapshot: &SelectedSnapshot,
    control_dir: &Path,
) -> Result<String> {
    let root = PathBuf::from(git_stdout(target, &["rev-parse", "--show-toplevel"])?.trim())
        .canonicalize()?;
    let scratch = tempdir_in(control_dir)?;
    let index = scratch.path().join("snapshot.index");
    let git = |args: &[&str]| -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .output()?;
        if !output.status.success() {
            bail!("selected snapshot Git tree could not be prepared");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    git(&["read-tree", "HEAD"])?;
    for file in &snapshot.files {
        let path = file
            .git_relative
            .to_str()
            .context("snapshot path is not UTF-8")?;
        match &file.action {
            SelectedAction::Copy { contents, mode } => {
                let mut child = Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["hash-object", "-w", "--path", path, "--stdin"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                child
                    .stdin
                    .take()
                    .context("Git hash input is unavailable")?
                    .write_all(contents)?;
                let output = child.wait_with_output()?;
                if !output.status.success() {
                    bail!("selected snapshot object could not be written");
                }
                let oid = String::from_utf8(output.stdout)?;
                let mode = if mode & 0o111 == 0 {
                    "100644"
                } else {
                    "100755"
                };
                git(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{},{path}", oid.trim()),
                ])?;
            }
            SelectedAction::Delete => {
                git(&["update-index", "--force-remove", "--", path])?;
            }
        }
    }
    let tree = git(&["write-tree"])?;
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("selected snapshot tree identifier is invalid");
    }
    // The worker's own index must know about selected new files. Otherwise
    // `git diff <tree>` treats them as deleted despite their worktree bytes.
    // Never replace the source checkout's index.
    let index_path = |workspace: &Path| -> Result<PathBuf> {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
            .output()?;
        if !output.status.success() {
            bail!("selected snapshot worktree index is unavailable");
        }
        Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
    };
    let worker_index = index_path(target)?;
    if worker_index == index_path(&snapshot.source)? {
        bail!("selected snapshot would modify the source Git index");
    }
    let mut replacement = NamedTempFile::new_in(
        worker_index
            .parent()
            .context("worker Git index has no parent")?,
    )?;
    replacement.write_all(&fs::read(&index)?)?;
    replacement.as_file_mut().sync_all()?;
    replacement.persist(&worker_index)?;
    Ok(tree)
}

pub(crate) fn git_head(workspace: &Path) -> Result<String> {
    Ok(git_stdout(workspace, &["rev-parse", "HEAD"])?
        .trim()
        .to_owned())
}

pub(crate) fn is_git_workspace(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()?;
    Ok(output.status.success() && output.stdout == b"true\n")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub(crate) struct AdmissionLock {
    path: PathBuf,
}

impl Drop for AdmissionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn acquire_admission_lock(
    worktrees_root: &Path,
    source: &Path,
) -> Result<Option<AdmissionLock>> {
    let Ok(source) = source.canonicalize() else {
        return Ok(None);
    };
    let output = match Command::new("git")
        .args([
            "-C",
            &lossy(&source),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Ok(None),
    };
    let common_dir = PathBuf::from(String::from_utf8(output.stdout)?.trim()).canonicalize()?;
    let parent = worktrees_root.join(".locks");
    fs::create_dir_all(&parent)?;
    let path = admission_lock_file(&parent, &common_dir);
    let started = Instant::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
                file.sync_all()?;
                return Ok(Some(AdmissionLock { path }));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if started.elapsed() >= ADMISSION_LOCK_WAIT {
                    bail!(
                        "another admission is in progress for {}; refusing to start a duplicate task worktree",
                        common_dir.display()
                    );
                }
                if admission_lock_holder_is_dead(&path) {
                    let _ = fs::remove_file(&path);
                } else {
                    thread::sleep(Duration::from_millis(25));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn admission_lock_file(lock_dir: &Path, git_common_dir: &Path) -> PathBuf {
    let digest = Sha256::digest(lossy(git_common_dir).as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(hex, "{byte:02x}");
    }
    lock_dir.join(format!("{hex}.lock"))
}

fn admission_lock_holder_is_dead(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output();
    match output {
        Ok(output) => {
            !output.status.success() || String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        Err(_) => false,
    }
}

pub(crate) fn prepare_workspace(
    worktrees_root: &Path,
    source: &Path,
    task_id: TaskId,
    revision: u32,
    adapter: &str,
    allow_clean_head_snapshot: bool,
) -> Result<PathBuf> {
    let source = source.canonicalize()?;
    let root_output = Command::new("git")
        .args(["-C", &lossy(&source), "rev-parse", "--show-toplevel"])
        .output();
    let root_output = match root_output {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(source),
        Err(error) => return Err(error.into()),
    };
    if !root_output.status.success() {
        return Ok(source);
    }
    let root = PathBuf::from(String::from_utf8(root_output.stdout)?.trim());
    let dirty = git_stdout(&root, &["status", "--porcelain"])?;
    if !dirty.trim().is_empty() && !allow_clean_head_snapshot {
        bail!(
            "source worktree contains uncommitted changes; commit or capture them first, or pass --allow-clean-head-snapshot to explicitly exclude them"
        );
    }
    let base_revision = git_stdout(&root, &["rev-parse", "HEAD"])?;
    let worktree_list = git_stdout(&root, &["worktree", "list", "--porcelain"])?;
    let primary = worktree_list
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .context("git worktree inventory did not contain a primary checkout")?;
    let repo_name = primary
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("workspace");
    let task_slug = task_slug(task_id, revision);
    let target_parent = worktrees_root.join(repo_name);
    fs::create_dir_all(&target_parent)?;
    let target = target_parent.join(&task_slug);
    let planned = match target_parent.canonicalize() {
        Ok(parent) => parent.join(&task_slug),
        Err(_) => target.clone(),
    };
    let branch = format!("brgr/task-{task_slug}");
    refuse_existing_path_or_branch(&root, &worktree_list, &planned, &target, &branch)?;

    let status = if adapter == OMP_ROLE_ADAPTER_V1
        && env::var("HERDR_ENV").as_deref() == Ok("1")
        && env::var_os("HERDR_PANE_ID").is_some()
    {
        add_herdr_worktree(&primary, &branch, base_revision.trim(), &target, &task_slug)?
    } else {
        add_git_worktree(&primary, &branch, &target, base_revision.trim())?
    };
    if !status.success() {
        bail!("failed to create task worktree {}", target.display());
    }
    if !target.is_dir() {
        bail!(
            "task worktree {} was not created as a directory; refusing to continue",
            target.display()
        );
    }
    #[cfg(debug_assertions)]
    if env::var_os("BRGR_TEST_EXIT_AFTER_WORKTREE").is_some() {
        std::process::exit(78);
    }
    let relative = source.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(target.join(relative))
}

pub(crate) fn workspace_is_present(workspace: &str) -> bool {
    Path::new(workspace).is_dir()
}

fn task_slug(task_id: TaskId, revision: u32) -> String {
    let short = task_id.to_string()[..8].to_owned();
    if revision == 1 {
        short
    } else {
        format!("{short}-r{revision}")
    }
}

fn refuse_existing_path_or_branch(
    root: &Path,
    worktree_list: &str,
    planned: &Path,
    target: &Path,
    branch: &str,
) -> Result<()> {
    if target.exists() || planned.exists() {
        bail!(
            "refusing to replace existing path {} for a task worktree",
            target.display()
        );
    }
    let branch_ref = format!("refs/heads/{branch}");
    for line in worktree_list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let registered = Path::new(path);
            if registered == planned || registered == target {
                bail!(
                    "refusing to replace existing path {} for a task worktree",
                    target.display()
                );
            }
        }
        if line.trim() == format!("branch {branch_ref}") {
            bail!("refusing to replace existing branch {branch}");
        }
    }
    if git_ref_exists(root, &branch_ref)? {
        bail!("refusing to replace existing branch {branch}");
    }
    Ok(())
}

fn add_git_worktree(
    primary: &Path,
    branch: &str,
    target: &Path,
    revision: &str,
) -> Result<std::process::ExitStatus> {
    let mut last = None;
    for attempt in 0..GIT_LOCK_ATTEMPTS {
        let output = Command::new("git")
            .arg("-C")
            .arg(primary)
            .args(["worktree", "add", "-b"])
            .arg(branch)
            .arg(target)
            .arg(revision)
            .output()?;
        if output.status.success() {
            return Ok(output.status);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_git_lock_error(&stderr) && attempt + 1 < GIT_LOCK_ATTEMPTS {
            thread::sleep(Duration::from_millis(25 * u64::from(attempt + 1)));
            last = Some(output);
            continue;
        }
        if is_git_collision_error(&stderr) {
            bail!(
                "refusing to replace existing task worktree or branch at {}: {stderr}",
                target.display()
            );
        }
        bail!(
            "failed to create task worktree {}: {stderr}",
            target.display()
        );
    }
    Ok(last
        .map(|output| output.status)
        .expect("lock retry loop keeps at least one git status"))
}

fn add_herdr_worktree(
    primary: &Path,
    branch: &str,
    revision: &str,
    target: &Path,
    task_slug: &str,
) -> Result<std::process::ExitStatus> {
    let mut last = None;
    for attempt in 0..GIT_LOCK_ATTEMPTS {
        let output = Command::new("herdr")
            .args(["worktree", "create", "--cwd"])
            .arg(primary)
            .args(["--branch", branch, "--base", revision, "--path"])
            .arg(target)
            .args([
                "--label",
                &format!("brgr-{task_slug}"),
                "--no-focus",
                "--trust-repository",
            ])
            .output()?;
        if output.status.success() {
            return Ok(output.status);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_git_lock_error(&stderr) && attempt + 1 < GIT_LOCK_ATTEMPTS {
            thread::sleep(Duration::from_millis(25 * u64::from(attempt + 1)));
            last = Some(output);
            continue;
        }
        if is_git_collision_error(&stderr) {
            bail!(
                "refusing to replace existing task worktree or branch at {}: {stderr}",
                target.display()
            );
        }
        bail!(
            "failed to create task worktree {}: {stderr}",
            target.display()
        );
    }
    Ok(last
        .map(|output| output.status)
        .expect("lock retry loop keeps at least one herdr status"))
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git command failed: {stderr}");
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_ref_exists(root: &Path, git_ref: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show-ref", "--verify", "--quiet", git_ref])
        .status()?;
    Ok(output.success())
}

fn is_git_lock_error(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("lock")
        && (text.contains("unable to create")
            || text.contains("unable to write")
            || text.contains("another git process")
            || text.contains("index.lock")
            || text.contains("config.lock")
            || text.contains("worktrees"))
}

fn is_git_collision_error(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("already exists")
        || text.contains("already used")
        || text.contains("is already checked out")
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tempfile::TempDir;

    fn task(id: &str) -> TaskId {
        TaskId::from_str(id).unwrap()
    }

    fn seed_repo(root: &Path) {
        assert!(
            Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["-C", &lossy(root), "config", "user.name", "Fixture"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    &lossy(root),
                    "config",
                    "user.email",
                    "fixture@example.invalid",
                ])
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("README"), b"seed\n").unwrap();
        assert!(
            Command::new("git")
                .args(["-C", &lossy(root), "add", "README"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["-C", &lossy(root), "commit", "-m", "seed"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["-C", &lossy(root), "branch", "user/keep-me"])
                .status()
                .unwrap()
                .success()
        );
    }

    fn branches(root: &Path) -> String {
        git_stdout(root, &["branch", "--list"]).unwrap()
    }

    #[test]
    fn dirty_source_is_rejected_without_creating_a_branch_or_worktree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        fs::write(repo.join("user-note.txt"), b"keep me\n").unwrap();
        let worktrees = temp.path().join("worktrees");
        let error = prepare_workspace(
            &worktrees,
            &repo,
            task("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            1,
            "process/v1",
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("uncommitted changes"));
        assert!(!worktrees.join("repo").exists());
        assert!(!branches(&repo).contains("brgr/task-"));
        assert!(branches(&repo).contains("user/keep-me"));
        assert_eq!(
            fs::read_to_string(repo.join("user-note.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn existing_path_collision_preserves_user_files() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        let worktrees = temp.path().join("worktrees");
        let occupant = worktrees.join("repo").join("aaaaaaaa");
        fs::create_dir_all(&occupant).unwrap();
        fs::write(occupant.join("user-file.txt"), b"do not delete\n").unwrap();
        let error = prepare_workspace(
            &worktrees,
            &repo,
            task("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            1,
            "process/v1",
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to replace existing path")
        );
        assert_eq!(
            fs::read_to_string(occupant.join("user-file.txt")).unwrap(),
            "do not delete\n"
        );
        assert!(branches(&repo).contains("user/keep-me"));
        assert!(!branches(&repo).contains("brgr/task-aaaaaaaa"));
    }

    #[test]
    fn existing_branch_collision_does_not_delete_the_branch() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        assert!(
            Command::new("git")
                .args(["-C", &lossy(&repo), "branch", "brgr/task-aaaaaaaa"])
                .status()
                .unwrap()
                .success()
        );
        let worktrees = temp.path().join("worktrees");
        let error = prepare_workspace(
            &worktrees,
            &repo,
            task("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            1,
            "process/v1",
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to replace existing branch")
        );
        assert!(branches(&repo).contains("brgr/task-aaaaaaaa"));
        assert!(branches(&repo).contains("user/keep-me"));
        assert!(!worktrees.join("repo").join("aaaaaaaa").exists());
    }

    #[test]
    fn concurrent_admissions_create_distinct_worktrees() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        let worktrees = temp.path().join("worktrees");
        let left_id = task("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let right_id = task("22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        std::thread::scope(|scope| {
            let left = scope
                .spawn(|| prepare_workspace(&worktrees, &repo, left_id, 1, "process/v1", false));
            let right = scope
                .spawn(|| prepare_workspace(&worktrees, &repo, right_id, 1, "process/v1", false));
            left.join().unwrap().unwrap();
            right.join().unwrap().unwrap();
        });
        assert!(worktrees.join("repo/11111111").is_dir());
        assert!(worktrees.join("repo/22222222").is_dir());
        let listed = branches(&repo);
        assert!(listed.contains("brgr/task-11111111"));
        assert!(listed.contains("brgr/task-22222222"));
        assert!(listed.contains("user/keep-me"));
        assert_eq!(fs::read_to_string(repo.join("README")).unwrap(), "seed\n");
    }

    #[test]
    fn same_basename_repositories_use_distinct_admission_locks() {
        let temp = TempDir::new().unwrap();
        let left = temp.path().join("a/repo");
        let right = temp.path().join("b/repo");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        seed_repo(&left);
        seed_repo(&right);
        let worktrees = temp.path().join("worktrees");
        let first = acquire_admission_lock(&worktrees, &left)
            .unwrap()
            .expect("left git repo needs an admission lock");
        let second = acquire_admission_lock(&worktrees, &right)
            .unwrap()
            .expect("same basename must not share the left lock");
        assert_ne!(first.path, second.path);
        let lock_count = fs::read_dir(worktrees.join(".locks")).unwrap().count();
        assert_eq!(lock_count, 2);
        assert!(left.join("README").is_file());
        assert!(right.join("README").is_file());
        assert!(branches(&left).contains("user/keep-me"));
        assert!(branches(&right).contains("user/keep-me"));
    }

    #[test]
    fn linked_worktrees_of_one_repository_share_an_admission_lock() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        fs::create_dir_all(&repo).unwrap();
        seed_repo(&repo);
        assert!(
            Command::new("git")
                .args(["-C", &lossy(&repo), "worktree", "add", "-b", "linked"])
                .arg(&linked)
                .status()
                .unwrap()
                .success()
        );
        let common = |path: &Path| {
            PathBuf::from(
                git_stdout(
                    path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )
                .unwrap()
                .trim(),
            )
            .canonicalize()
            .unwrap()
        };
        let lock_dir = temp.path().join("locks");
        assert_eq!(
            admission_lock_file(&lock_dir, &common(&repo)),
            admission_lock_file(&lock_dir, &common(&linked))
        );
    }
}
