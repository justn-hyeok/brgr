//! Changes a worker left in the shared Git stash (issue #150).
//!
//! `git stash` belongs to the repository, not to a worktree: a worker that
//! stashes its changes to compare with a baseline and never pops them ends with
//! a clean worktree, so its sealed diff silently misses that work. The stash
//! cannot be isolated, but a sealed result must say it was left behind.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use crate::git_diff;

/// The note a seal adds when the state of the stash could not be read.
pub(crate) const COULD_NOT_CHECK: &str = "could not check the git stash for left-behind changes";

/// Largest stash listing this check reads.
const MAX_STASH_BYTES: u64 = 64 * 1024;

/// One note when this workspace's branch left entries in the shared stash.
/// Stash messages and contents are never quoted.
pub(crate) fn stash_notes(workspace: &Path, remaining: Duration) -> Vec<String> {
    stash_notes_with_executable(Path::new("git"), workspace, remaining)
}

pub(crate) fn stash_notes_with_executable(
    executable: &Path,
    workspace: &Path,
    remaining: Duration,
) -> Vec<String> {
    let started = Instant::now();
    let Ok(branch) = git_diff::bounded_git_output_with_executable(
        executable,
        workspace,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        4_096,
        remaining,
        "Git branch lookup",
    ) else {
        return could_not_check(workspace);
    };
    let Ok(branch) = String::from_utf8(branch) else {
        return could_not_check(workspace);
    };
    let branch = branch.trim();
    if branch.is_empty() {
        return could_not_check(workspace);
    }
    let Ok(listing) = git_diff::bounded_git_output_with_executable(
        executable,
        workspace,
        &["stash", "list"],
        MAX_STASH_BYTES,
        remaining.saturating_sub(started.elapsed()),
        "git stash listing",
    ) else {
        return could_not_check(workspace);
    };
    let Ok(listing) = String::from_utf8(listing) else {
        return could_not_check(workspace);
    };
    let count = stash_entries_on(&listing, branch);
    if count == 0 {
        return Vec::new();
    }
    let entries = if count == 1 { "entry" } else { "entries" };
    vec![format!(
        "The worker left {count} {entries} in the git stash on {branch}; changes held in a stash are not part of this diff"
    )]
}

/// Counts `stash@{N}: On <branch>: ...` and `stash@{N}: WIP on <branch>: ...`
/// lines for `branch`, ignoring every other branch's entries and the messages.
fn stash_entries_on(listing: &str, branch: &str) -> usize {
    listing
        .lines()
        .filter(|line| stash_branch(line) == Some(branch))
        .count()
}

fn stash_branch(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("stash@{")?;
    let (_, subject) = rest.split_once("}: ")?;
    let subject = subject
        .strip_prefix("On ")
        .or_else(|| subject.strip_prefix("WIP on "))?;
    let (branch, _) = subject.split_once(':')?;
    Some(branch)
}

/// The seal still succeeds when git cannot be read; a workspace that is not a
/// repository has nothing to check, so it gets no note.
fn could_not_check(workspace: &Path) -> Vec<String> {
    if workspace.ancestors().any(|dir| dir.join(".git").exists()) {
        vec![COULD_NOT_CHECK.to_owned()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, os::unix::fs::PermissionsExt as _, path::Path, process::Command, time::Duration,
    };

    use super::*;

    const BRANCH: &str = "brgr/task-ab12cd34";

    fn git(workspace: &Path, argv: &[&str]) {
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
        assert!(
            output.status.success(),
            "git {argv:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository(branch: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q", "-b", branch]);
        fs::write(root.path().join("tracked.txt"), "base\n").unwrap();
        git(root.path(), &["add", "tracked.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);
        root
    }

    fn stash(root: &Path, message: &str) {
        fs::write(root.join("tracked.txt"), format!("{message}\n")).unwrap();
        git(root, &["stash", "push", "-m", message]);
    }

    fn notes(root: &Path) -> Vec<String> {
        stash_notes(root, Duration::from_secs(20))
    }

    #[test]
    fn a_stash_on_the_task_branch_is_one_note_without_its_message() {
        let repo = repository(BRANCH);
        stash(repo.path(), "takeover-baseline-count");

        let notes = notes(repo.path());

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("1 entry"), "{}", notes[0]);
        assert!(notes[0].contains(BRANCH), "{}", notes[0]);
        assert!(
            !notes[0].contains("takeover-baseline-count"),
            "{}",
            notes[0]
        );
    }

    #[test]
    fn two_stashes_on_the_task_branch_are_counted_as_two() {
        let repo = repository(BRANCH);
        stash(repo.path(), "first");
        stash(repo.path(), "second");

        let notes = notes(repo.path());

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("2 entries"), "{}", notes[0]);
    }

    #[test]
    fn a_stash_on_another_branch_is_not_counted() {
        let repo = repository(BRANCH);
        git(repo.path(), &["checkout", "-qb", "brgr/task-ffffffff"]);
        stash(repo.path(), "another-tasks-change");
        git(repo.path(), &["checkout", "-q", BRANCH]);

        assert!(notes(repo.path()).is_empty());
    }

    #[test]
    fn a_wip_entry_for_the_task_branch_is_counted() {
        let repo = repository(BRANCH);
        fs::write(repo.path().join("tracked.txt"), "wip\n").unwrap();
        git(repo.path(), &["stash", "push", "-q"]);

        let notes = notes(repo.path());

        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("1 entry"), "{}", notes[0]);
        assert!(notes[0].contains(BRANCH), "{}", notes[0]);
    }

    #[test]
    fn no_stash_produces_no_note() {
        let repo = repository(BRANCH);
        assert!(notes(repo.path()).is_empty());
    }

    #[test]
    fn a_non_git_workspace_produces_no_note() {
        let root = tempfile::tempdir().unwrap();
        assert!(notes(root.path()).is_empty());
    }

    #[test]
    fn a_failing_stash_list_notes_that_the_check_could_not_run() {
        let repo = repository(BRANCH);
        let fake = repo.path().join("fake-git");
        // The branch lookup succeeds; only the stash listing fails.
        fs::write(
            &fake,
            "#!/bin/sh\ncase \"$*\" in\n  *\"stash list\"*) exit 1 ;;\nesac\nprintf 'brgr/task-ab12cd34\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

        let notes = stash_notes_with_executable(&fake, repo.path(), Duration::from_secs(20));

        assert_eq!(notes, vec![COULD_NOT_CHECK.to_owned()]);
    }

    #[test]
    fn a_missing_git_outside_a_repository_adds_no_note() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("no-such-git");
        assert!(
            stash_notes_with_executable(&missing, root.path(), Duration::from_secs(20)).is_empty()
        );
    }
}
