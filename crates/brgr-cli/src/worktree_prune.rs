//! Removal of brgr-owned task worktrees and the branches they created.
//!
//! [`crate::workspace`] creates task worktrees and never deletes, so removal
//! lives here behind an explicit command that defaults to reporting.
//!
//! git is used for the two removals, but it is deliberately **not** trusted as
//! the only safety authority. `git worktree remove` refuses a worktree with
//! modified or untracked files, and `git branch -d` refuses a branch with
//! unmerged commits, and neither is ever forced — but git's clean check runs
//! `git status --porcelain` without `--ignored`, so a `.env`, a downloaded
//! credential, or a build cache is invisible to it and would be deleted
//! silently. A task worktree is exactly where an agent has been working, which
//! makes it the most likely place in a repository to hold such a file. This
//! module therefore checks the ignored set itself and keeps the worktree unless
//! the caller opts in.
//!
//! Every candidate must also be a real directory (not a symlink), a worktree
//! git itself has registered for its repository, a name [`crate::workspace`]
//! could have produced, outside the current working directory, and the checkout
//! of a task revision that carries a recorded owner decision.
//!
//! No sealed result, decision, artifact, or task row is ever removed. Those cost
//! roughly 9 KiB per task; a worktree costs the size of the checkout.
//!
//! Cross-owner isolation is explicitly outside v1 scope, so a prune acts on
//! every decided task in this control home. Each row reports its owner so that
//! is visible rather than silent.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use brgr_protocol::TaskSpec;
use brgr_store::Store;

/// One brgr-owned worktree directory and what pruning did or would do with it.
pub(crate) struct Entry {
    pub(crate) worktree: PathBuf,
    pub(crate) slug: String,
    pub(crate) owner: Option<String>,
    /// Ignored-but-present paths git's own clean check cannot see.
    pub(crate) ignored: Vec<String>,
    pub(crate) outcome: Outcome,
}

pub(crate) enum Outcome {
    /// Eligible, and this was a report-only run. Every check that `--apply`
    /// performs has already passed, so the two runs agree.
    Removable,
    Removed,
    /// The checkout is gone but git kept the branch, which is the normal outcome
    /// for a task whose agent committed work. Counted as removed, because the
    /// reclaimed space is gone and the directory will never be seen again.
    RemovedKeepingBranch(String),
    /// Left in place; the string says why, so a report is actionable.
    Kept(String),
}

impl Outcome {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Removable => "removable",
            Self::Removed => "removed",
            Self::RemovedKeepingBranch(_) => "removed_branch_kept",
            Self::Kept(_) => "kept",
        }
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Self::Kept(reason) | Self::RemovedKeepingBranch(reason) => Some(reason),
            Self::Removable | Self::Removed => None,
        }
    }
}

/// Reports, and with `apply` removes, every prunable task worktree.
///
/// `include_ignored` opts in to removing a worktree that holds ignored files.
pub(crate) fn prune(
    worktrees_root: &Path,
    store: &Store,
    apply: bool,
    include_ignored: bool,
) -> Result<Vec<Entry>> {
    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|path| canonical(&path));
    let mut entries = Vec::new();
    let mut emptied: Vec<PathBuf> = Vec::new();

    for worktree in worktree_directories(worktrees_root)? {
        let slug = file_name(&worktree);
        let assessment = assess(
            &worktree,
            &slug,
            store,
            current_dir.as_deref(),
            include_ignored,
        );
        let outcome = match assessment.blocked {
            Some(reason) => Outcome::Kept(reason),
            None if apply => match remove(&worktree, &slug) {
                Ok(None) => {
                    if let Some(parent) = worktree.parent() {
                        emptied.push(parent.to_path_buf());
                    }
                    Outcome::Removed
                }
                Ok(Some(reason)) => {
                    if let Some(parent) = worktree.parent() {
                        emptied.push(parent.to_path_buf());
                    }
                    Outcome::RemovedKeepingBranch(reason)
                }
                Err(reason) => Outcome::Kept(reason),
            },
            None => Outcome::Removable,
        };
        entries.push(Entry {
            worktree,
            slug,
            owner: assessment.owner,
            ignored: assessment.ignored,
            outcome,
        });
    }

    // Only directories this prune actually emptied are considered, and never a
    // dot-directory: `<worktrees>/.locks` holds the cross-process admission lock
    // and is empty at rest, so treating every empty child as a stale repository
    // directory would delete it and break a concurrent `brgr run`.
    for parent in emptied {
        if file_name(&parent).starts_with('.') {
            continue;
        }
        if fs::read_dir(&parent).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = fs::remove_dir(&parent);
        }
    }

    entries.sort_by(|left, right| left.worktree.cmp(&right.worktree));
    Ok(entries)
}

/// What the checks found for one candidate. `blocked` is `Some` when the
/// worktree is kept, and the same checks run in report and apply mode so the two
/// runs agree on what is removable.
struct Assessment {
    blocked: Option<String>,
    owner: Option<String>,
    ignored: Vec<String>,
}

fn assess(
    worktree: &Path,
    slug: &str,
    store: &Store,
    current_dir: Option<&Path>,
    include_ignored: bool,
) -> Assessment {
    let mut owner = None;
    let mut ignored = Vec::new();
    let blocked = objection(
        worktree,
        slug,
        store,
        current_dir,
        include_ignored,
        &mut owner,
        &mut ignored,
    );
    Assessment {
        blocked,
        owner,
        ignored,
    }
}

/// Returns the first reason this worktree must be kept, or `None` to remove it.
fn objection(
    worktree: &Path,
    slug: &str,
    store: &Store,
    current_dir: Option<&Path>,
    include_ignored: bool,
    owner: &mut Option<String>,
    ignored: &mut Vec<String>,
) -> Option<String> {
    // `?` must never be used for these: in a function whose `None` means "no
    // objection", a short-circuit would mark an unrecognized directory removable.
    let Some((prefix, revision)) = parse_slug(slug) else {
        return Some("directory name is not a brgr task slug; refusing to touch it".to_owned());
    };

    // `Path::is_dir` follows symlinks, and `git worktree remove` resolves its
    // argument, so a symlink placed here would make git destroy a checkout
    // entirely outside the worktrees root.
    match fs::symlink_metadata(worktree) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Some("path is a symlink; refusing to follow it".to_owned());
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Some("path is not a directory".to_owned());
        }
        Ok(_) => {}
        Err(error) => return Some(format!("path is unreadable: {error}")),
    }

    let Some(resolved) = canonical(worktree) else {
        return Some("path could not be resolved".to_owned());
    };
    if let Some(cwd) = current_dir
        && (cwd == resolved || cwd.starts_with(&resolved))
    {
        return Some(
            "this is the current working directory; refusing to remove it from under the \
             running process"
                .to_owned(),
        );
    }

    let mut matches = match store.task_revisions_with_prefix(prefix, revision) {
        Ok(matches) => matches,
        Err(error) => return Some(format!("task lookup failed: {error}")),
    };
    let task: TaskSpec = match matches.len() {
        1 => matches.remove(0),
        0 => return Some("no task revision matches this worktree".to_owned()),
        _ => return Some("task id prefix is ambiguous; refusing to guess".to_owned()),
    };
    *owner = Some(task.owner_id.as_str().to_owned());

    match store.decision_for_revision(task.task_id, revision) {
        Ok(Some(_)) => {}
        Ok(None) => return Some("no owner decision recorded for this revision".to_owned()),
        Err(error) => return Some(format!("decision lookup failed: {error}")),
    }

    // Ask git the same questions `--apply` would, so `removable` means the apply
    // run will remove it rather than discover a veto later.
    let primary = match primary_checkout(worktree) {
        Ok(primary) => primary,
        Err(reason) => return Some(reason),
    };
    if let Err(reason) = registered_worktree(&primary, &resolved) {
        return Some(reason);
    }
    match git(worktree, &["status", "--porcelain"]) {
        Ok(status) if !status.trim().is_empty() => {
            return Some("worktree holds modified or untracked files".to_owned());
        }
        Ok(_) => {}
        Err(reason) => return Some(format!("worktree status is unreadable: {reason}")),
    }

    *ignored = ignored_paths(worktree);
    if !ignored.is_empty() && !include_ignored {
        return Some(format!(
            "worktree holds {} ignored path(s) git's clean check cannot see, such as {}; \
             pass --include-ignored to remove them",
            ignored.len(),
            ignored.first().map_or("", String::as_str)
        ));
    }
    None
}

/// Enumerates `<worktrees>/<repository>/<slug>` directories, skipping the
/// dot-directories brgr keeps beside them.
fn worktree_directories(worktrees_root: &Path) -> Result<Vec<PathBuf>> {
    if !worktrees_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for repository in read_dir_sorted(worktrees_root)? {
        if file_name(&repository).starts_with('.') || !repository.is_dir() {
            continue;
        }
        for worktree in read_dir_sorted(&repository)? {
            if !file_name(&worktree).starts_with('.') {
                found.push(worktree);
            }
        }
    }
    Ok(found)
}

fn read_dir_sorted(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(directory)
        .with_context(|| format!("read {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

/// Splits `abcd1234` or `abcd1234-r3` into its task-id prefix and revision.
///
/// Only the exact shape [`crate::workspace`] produces is accepted: anything
/// else is a directory somebody put here, not a task slug.
fn parse_slug(slug: &str) -> Option<(&str, u32)> {
    const PREFIX_LENGTH: usize = 8;
    let (prefix, revision) = match slug.split_once("-r") {
        Some((prefix, revision)) => (prefix, revision.parse::<u32>().ok()?),
        None => (slug, 1),
    };
    if revision == 0
        || prefix.len() != PREFIX_LENGTH
        || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some((prefix, revision))
}

/// Removes one worktree and then its branch.
///
/// `Ok(None)` removed both. `Ok(Some(reason))` removed the checkout while git
/// kept the branch, which preserves committed work.
fn remove(worktree: &Path, slug: &str) -> Result<Option<String>, String> {
    let primary = primary_checkout(worktree)?;
    git(&primary, &["worktree", "remove", &lossy(worktree)])
        .map_err(|error| format!("git declined to remove the worktree: {error}"))?;
    let branch = format!("brgr/task-{slug}");
    if let Err(error) = git(&primary, &["branch", "-d", &branch]) {
        return Ok(Some(format!(
            "checkout removed; branch {branch} kept because git refused to delete it: {error}"
        )));
    }
    Ok(None)
}

/// Resolves the primary checkout that owns a linked worktree.
fn primary_checkout(worktree: &Path) -> Result<PathBuf, String> {
    let listing = git(worktree, &["worktree", "list", "--porcelain"])
        .map_err(|error| format!("worktree is not a usable git checkout: {error}"))?;
    listing
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| "git worktree inventory has no primary checkout".to_owned())
}

/// Confirms git itself registered this path as a worktree of `primary`.
fn registered_worktree(primary: &Path, resolved: &Path) -> Result<(), String> {
    let listing = git(primary, &["worktree", "list", "--porcelain"])
        .map_err(|error| format!("repository worktree inventory is unreadable: {error}"))?;
    let registered = listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| canonical(Path::new(path)))
        .any(|path| path == resolved);
    if registered {
        Ok(())
    } else {
        Err("path is not a registered worktree of its repository".to_owned())
    }
}

/// Lists ignored-but-present paths, which `git status --porcelain` omits.
fn ignored_paths(worktree: &Path) -> Vec<String> {
    let Ok(listing) = git(worktree, &["status", "--porcelain", "--ignored"]) else {
        // Unreadable status is already refused by the caller's clean check.
        return Vec::new();
    };
    listing
        .lines()
        .filter_map(|line| line.strip_prefix("!! "))
        .map(str::to_owned)
        .collect()
}

fn git(directory: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().replace('\n', "; "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_parse_their_revision_and_reject_every_other_directory_name() {
        assert_eq!(parse_slug("3d3c9081"), Some(("3d3c9081", 1)));
        assert_eq!(parse_slug("3d3c9081-r4"), Some(("3d3c9081", 4)));
        assert_eq!(parse_slug("3d3c9081-r0"), None);
        assert_eq!(parse_slug("3d3c9081-rx"), None);
        assert_eq!(parse_slug("not-a-slug"), None);
        assert_eq!(parse_slug(""), None);
        assert_eq!(parse_slug(".DS_Store"), None);
        // `workspace::task_slug` emits exactly eight hex characters. A shorter or
        // longer hex name is somebody else's directory, and accepting it would
        // prefix-match a large share of all task ids.
        assert_eq!(parse_slug("f"), None);
        assert_eq!(parse_slug("cafe"), None);
        assert_eq!(parse_slug("dec0de"), None);
        assert_eq!(parse_slug("deadbeefcafe"), None);
        assert_eq!(parse_slug("deadbeefcafe-r2"), None);
    }

    #[test]
    fn outcomes_report_their_reason_and_removed_states_do_not() {
        assert_eq!(Outcome::Removable.code(), "removable");
        assert_eq!(Outcome::Removed.code(), "removed");
        assert_eq!(Outcome::Kept("why".to_owned()).code(), "kept");
        assert_eq!(Outcome::Kept("why".to_owned()).reason(), Some("why"));
        let kept_branch = Outcome::RemovedKeepingBranch("branch kept".to_owned());
        assert_eq!(kept_branch.code(), "removed_branch_kept");
        assert_eq!(kept_branch.reason(), Some("branch kept"));
        assert_eq!(Outcome::Removed.reason(), None);
        assert_eq!(Outcome::Removable.reason(), None);
    }
}
