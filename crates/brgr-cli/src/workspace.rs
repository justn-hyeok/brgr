//! Git-backed task worktree creation. Fail closed; never delete or reset.

use std::{
    env,
    fmt::Write as _,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use brgr_protocol::TaskId;
use brgr_runner::OMP_ROLE_ADAPTER_V1;
use sha2::{Digest, Sha256};

const GIT_LOCK_ATTEMPTS: u32 = 8;
const ADMISSION_LOCK_WAIT: Duration = Duration::from_secs(10);

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
    let repo_name = root
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
