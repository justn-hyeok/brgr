//! The sealed Git diff behind `--capture-diff`.
//!
//! A worker's change is whatever its worktree holds against the task base, and
//! that includes files it created without staging them. `git diff BASE` alone
//! compares only paths the index already tracks, so a new file would silently
//! fall out of the patch. The collector therefore marks untracked, non-ignored
//! files intent-to-add in a throwaway copy of the worker's index and diffs with
//! that copy; the worker's own index is never written.

use std::{
    fs,
    io::{Read as _, Write as _},
    os::unix::process::CommandExt as _,
    path::{Component, Path},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Largest untracked-path listing the collector reads.
const MAX_LISTING_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn bounded_git_diff(
    workspace: &Path,
    base_tree: &str,
    excluded: &[String],
    max_bytes: u64,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    bounded_git_diff_with_executable(
        Path::new("git"),
        workspace,
        base_tree,
        excluded,
        max_bytes,
        deadline,
    )
}

pub(crate) fn bounded_git_diff_with_executable(
    executable: &Path,
    workspace: &Path,
    base_tree: &str,
    excluded: &[String],
    max_bytes: u64,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    let started = Instant::now();
    let index = tempfile::tempdir().map_err(|error| error.to_string())?;
    let index = index.path().join("index");
    let git = |remaining: Duration| Git {
        executable,
        workspace,
        index: Some(&index),
        remaining,
    };
    let real_index = Git {
        index: None,
        ..git(deadline)
    }
    .run(
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        None,
        4_096,
        "Git index lookup",
    )?;
    let real_index = String::from_utf8(real_index).map_err(|error| error.to_string())?;
    let real_index = Path::new(real_index.trim_end_matches('\n'));
    // A repository without a commit or a staged file has no index yet.
    if real_index.is_file() {
        fs::copy(real_index, &index).map_err(|error| error.to_string())?;
    }
    let listing = git(deadline.saturating_sub(started.elapsed())).run(
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        MAX_LISTING_BYTES,
        "untracked file listing",
    )?;
    let untracked = untracked_to_add(&listing, excluded);
    if !untracked.is_empty() {
        git(deadline.saturating_sub(started.elapsed())).run(
            &[
                "--literal-pathspecs",
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(untracked),
            4_096,
            "untracked file staging",
        )?;
    }
    let mut arguments = vec![
        "-c".to_owned(),
        "diff.noprefix=false".to_owned(),
        "-c".to_owned(),
        "diff.relative=false".to_owned(),
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--no-relative".to_owned(),
        "--binary".to_owned(),
        "--src-prefix=a/".to_owned(),
        "--dst-prefix=b/".to_owned(),
        base_tree.to_owned(),
    ];
    if !excluded.is_empty() {
        arguments.extend(["--".to_owned(), ":(top)**".to_owned()]);
        arguments.extend(
            excluded
                .iter()
                .map(|path| format!(":(exclude,literal){path}")),
        );
    }
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    git(deadline.saturating_sub(started.elapsed())).run(
        &arguments,
        None,
        max_bytes,
        "requested Git diff",
    )
}

/// The NUL-separated untracked paths, less the requested evidence files: a
/// report the worker wrote for the owner is evidence, not a change to apply.
fn untracked_to_add(listing: &[u8], excluded: &[String]) -> Vec<u8> {
    let excluded: Vec<Vec<Component<'_>>> = excluded
        .iter()
        .map(|path| normal_components(Path::new(path)))
        .collect();
    let mut paths = Vec::new();
    for entry in listing.split(|byte| *byte == 0) {
        let Ok(text) = std::str::from_utf8(entry) else {
            paths.extend_from_slice(entry);
            paths.push(0);
            continue;
        };
        let components = normal_components(Path::new(text));
        if entry.is_empty() || excluded.iter().any(|prefix| components.starts_with(prefix)) {
            continue;
        }
        paths.extend_from_slice(entry);
        paths.push(0);
    }
    paths
}

fn normal_components(path: &Path) -> Vec<Component<'_>> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

struct Git<'a> {
    executable: &'a Path,
    workspace: &'a Path,
    /// The throwaway index, or `None` to address the worker's own.
    index: Option<&'a Path>,
    remaining: Duration,
}

impl Git<'_> {
    /// Runs one Git command against the throwaway index, bounding its output
    /// and its time, and returns its stdout.
    fn run(
        &self,
        argv: &[&str],
        input: Option<Vec<u8>>,
        max_bytes: u64,
        label: &str,
    ) -> Result<Vec<u8>, String> {
        if self.remaining.is_zero() {
            return Err(format!("{label} exceeded the remaining attempt deadline"));
        }
        let mut command = Command::new(self.executable);
        match self.index {
            Some(index) => command.env("GIT_INDEX_FILE", index),
            None => command.env_remove("GIT_INDEX_FILE"),
        };
        let mut child = command
            .arg("-C")
            .arg(self.workspace)
            .args(["-c", "core.fsmonitor=false"])
            .args(argv)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| error.to_string())?;
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take();
            thread::spawn(move || {
                if let Some(stdin) = stdin.as_mut() {
                    let _ = stdin.write_all(&bytes);
                }
            })
        });
        let result = collect(&mut child, max_bytes, self.remaining, label);
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        result
    }
}

fn collect(
    child: &mut Child,
    max_bytes: u64,
    deadline: Duration,
    label: &str,
) -> Result<Vec<u8>, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} stream is unavailable"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(max_bytes + 1).read_to_end(&mut bytes);
        let _ = sender.send(read.map(|_| bytes));
    });
    let started = Instant::now();
    let fail = |child: &mut Child, message: String| {
        stop(child);
        Err(message)
    };
    let bytes = match receiver.recv_timeout(deadline) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            let outcome = fail(child, error.to_string());
            let _ = reader.join();
            return outcome;
        }
        Err(_) => {
            let outcome = fail(
                child,
                format!("{label} exceeded the remaining attempt deadline"),
            );
            let _ = reader.join();
            return outcome;
        }
    };
    let _ = reader.join();
    if bytes.len() as u64 > max_bytes {
        return fail(child, format!("{label} exceeds {max_bytes} bytes"));
    }
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            if !status.success() {
                return Err(format!("{label} could not be read"));
            }
            return Ok(bytes);
        }
        if started.elapsed() >= deadline {
            return fail(
                child,
                format!("{label} exceeded the remaining attempt deadline"),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn stop(child: &mut Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn git(workspace: &Path, argv: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args([
                "-c",
                "user.name=brgr",
                "-c",
                "user.email=brgr@example.invalid",
            ])
            .args(argv)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {argv:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("tracked.txt"), "base\n").unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.log\n").unwrap();
        git(root.path(), &["add", "tracked.txt", ".gitignore"]);
        git(root.path(), &["commit", "-qm", "base"]);
        root
    }

    fn diff(root: &Path, excluded: &[String]) -> String {
        let patch =
            bounded_git_diff(root, "HEAD", excluded, 1 << 20, Duration::from_secs(20)).unwrap();
        String::from_utf8(patch).unwrap()
    }

    #[test]
    fn new_files_are_in_the_patch_and_the_worker_index_is_untouched() {
        let root = repository();
        let root = root.path();
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/new file.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("glob[1].txt"), "literal\n").unwrap();
        fs::write(root.join("ignored.log"), "noise\n").unwrap();
        let before = git(root, &["status", "--porcelain"]);

        let patch = diff(root, &[]);

        assert!(patch.contains("+changed"));
        assert!(patch.contains("new file mode"));
        assert!(patch.contains("+fn main() {}"));
        assert!(patch.contains("b/glob[1].txt"));
        assert!(!patch.contains("ignored.log"));
        assert_eq!(git(root, &["status", "--porcelain"]), before);
    }

    #[test]
    fn a_subdirectory_workspace_keeps_the_rest_of_the_tree() {
        let root = repository();
        let root = root.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/kept.txt"), "kept\n").unwrap();
        git(root, &["add", "sub/kept.txt"]);
        git(root, &["commit", "-qm", "sub"]);
        fs::write(root.join("sub/new.txt"), "new\n").unwrap();

        let patch = diff(&root.join("sub"), &[]);

        assert!(patch.contains("b/sub/new.txt"), "patch: {patch}");
        assert!(!patch.contains("deleted file"), "patch: {patch}");
    }

    #[test]
    fn new_binary_files_apply_to_a_clean_checkout() {
        let root = repository();
        let root = root.path();
        fs::write(root.join("image.bin"), [0_u8, 1, 2, 255]).unwrap();
        let patch = diff(root, &[]);
        let clone = tempfile::tempdir().unwrap();
        let target = clone.path().join("clone");
        git(
            clone.path(),
            &["clone", "-q", root.to_str().unwrap(), "clone"],
        );
        let mut apply = Command::new("git")
            .arg("-C")
            .arg(&target)
            .args(["apply", "--binary", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        apply
            .stdin
            .take()
            .unwrap()
            .write_all(patch.as_bytes())
            .unwrap();
        assert!(apply.wait().unwrap().success());
        assert_eq!(fs::read(target.join("image.bin")).unwrap(), [0, 1, 2, 255]);
    }

    #[test]
    fn requested_evidence_files_stay_out_of_the_patch() {
        let root = repository();
        let root = root.path();
        fs::write(root.join("report.md"), "evidence\n").unwrap();
        fs::write(root.join("kept.md"), "change\n").unwrap();
        let patch = diff(root, &["./report.md".to_owned()]);
        assert!(!patch.contains("report.md"));
        assert!(patch.contains("b/kept.md"));
    }

    #[test]
    fn staged_managed_report_stays_out_without_hiding_other_source_changes() {
        let root = repository();
        let root = root.path();
        fs::create_dir_all(root.join(".brgr/tasks/owned-r1")).unwrap();
        fs::write(root.join(".brgr/tasks/owned-r1/report.md"), "report").unwrap();
        fs::write(root.join(".brgr/source.txt"), "source").unwrap();
        git(root, &["add", ".brgr"]);
        let before = git(root, &["status", "--porcelain"]);
        let patch = diff(root, &[".brgr/tasks/owned-r1".to_owned()]);
        assert!(!patch.contains("report.md"));
        assert!(patch.contains("b/.brgr/source.txt"));
        assert_eq!(git(root, &["status", "--porcelain"]), before);
    }

    #[test]
    fn a_repository_without_commits_still_lists_new_files() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("first.txt"), "first\n").unwrap();
        let empty_tree = git(root.path(), &["hash-object", "-t", "tree", "/dev/null"]);
        let patch = bounded_git_diff(
            root.path(),
            empty_tree.trim(),
            &[],
            1 << 20,
            Duration::from_secs(20),
        )
        .unwrap();
        assert!(String::from_utf8(patch).unwrap().contains("+first"));
    }

    #[test]
    fn bounded_diff_stops_a_stalled_collector() {
        let root = tempfile::tempdir().unwrap();
        let slow = root.path().join("slow-git");
        fs::write(&slow, "#!/bin/sh\n/bin/sleep 5\nprintf 'late patch'\n").unwrap();
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let error = bounded_git_diff_with_executable(
            &slow,
            root.path(),
            "HEAD",
            &[],
            1_024,
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(error.contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
