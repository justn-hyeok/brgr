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
//! Every candidate must also be a real directory (not a symlink), a worktree git
//! itself has registered for its repository and has not locked, a name
//! [`crate::workspace`] could have produced, outside the current working
//! directory, and the checkout of a task revision that carries a recorded owner
//! decision.
//!
//! A sweep also reclaims the `brgr/task-*` branches left behind by worktrees that
//! no longer exist — removing one by hand was the only reclamation available
//! before this command, and it leaves a branch and a stale registration behind
//! forever. One unreadable directory is reported as a single kept row rather than
//! ending the sweep, and the repository listing is read once per repository
//! rather than once per candidate.
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

use anyhow::Result;
use brgr_protocol::TaskSpec;
use brgr_store::Store;

/// Everything one sweep found: the worktrees it walked, and the branches left
/// behind by worktrees that no longer exist.
pub(crate) struct Prune {
    pub(crate) entries: Vec<Entry>,
    pub(crate) orphans: Vec<Orphan>,
}

/// A `brgr/task-*` branch whose worktree is gone.
pub(crate) struct Orphan {
    pub(crate) branch: String,
    pub(crate) outcome: Outcome,
}

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
) -> Prune {
    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|path| canonical(&path));
    let mut entries = Vec::new();
    let mut orphans = Vec::new();

    let (repositories, unreadable) = repository_directories(worktrees_root);
    entries.extend(unreadable.into_iter().map(unreadable_entry));

    for repository in repositories {
        let (candidates, unreadable) = child_directories(&repository);
        entries.extend(unreadable.into_iter().map(unreadable_entry));
        // One listing for the whole repository rather than one per candidate.
        let inventory = Inventory::load(&candidates);

        let mut removed_any = false;
        for worktree in candidates {
            let slug = file_name(&worktree);
            let assessment = assess(
                &worktree,
                &slug,
                store,
                inventory.as_ref(),
                current_dir.as_deref(),
                include_ignored,
            );
            let outcome = match assessment.blocked {
                Some(reason) => Outcome::Kept(reason),
                None if apply => match remove(&worktree, &slug, inventory.as_ref()) {
                    Ok(None) => {
                        removed_any = true;
                        Outcome::Removed
                    }
                    Ok(Some(reason)) => {
                        removed_any = true;
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

        // A worktree removed by hand — the only reclamation available before
        // `brgr prune` existed — leaves its branch and its registration behind
        // forever, because this sweep only ever sees directories that still exist.
        if let Some(inventory) = inventory.as_ref() {
            orphans.extend(reconcile_orphans(inventory, worktrees_root, apply));
        }

        if apply && removed_any {
            remove_if_emptied(&repository, current_dir.as_deref());
        }
    }

    entries.sort_by(|left, right| left.worktree.cmp(&right.worktree));
    orphans.sort_by(|left, right| left.branch.cmp(&right.branch));
    Prune { entries, orphans }
}

/// Drops a repository directory this run emptied.
///
/// Never a dot-directory: `<worktrees>/.locks` holds the cross-process admission
/// lock and is empty at rest. Never the directory the process is sitting in or an
/// ancestor of it either, or removing the last child unlinks the caller's own
/// working directory.
fn remove_if_emptied(repository: &Path, current_dir: Option<&Path>) {
    if file_name(repository).starts_with('.') {
        return;
    }
    if current_dir.is_some_and(|cwd| is_self_or_ancestor(repository, cwd)) {
        return;
    }
    if fs::read_dir(repository).is_ok_and(|mut entries| entries.next().is_none()) {
        let _ = fs::remove_dir(repository);
    }
}

/// Reclaims `brgr/task-*` branches whose worktree no longer exists.
///
/// git holds the safety line as everywhere else in this module: `worktree prune`
/// only drops registrations whose directory is gone, and `branch -d` refuses a
/// branch whose commits are not merged.
fn reconcile_orphans(inventory: &Inventory, worktrees_root: &Path, apply: bool) -> Vec<Orphan> {
    let Some(root) = canonical(worktrees_root) else {
        return Vec::new();
    };
    // Stale registrations first, so the branch listing below is not kept alive by
    // a worktree directory that is already gone.
    if apply {
        let _ = git(&inventory.primary, &["worktree", "prune"]);
    }
    let Ok(listing) = git(
        &inventory.primary,
        &[
            "branch",
            "--list",
            "brgr/task-*",
            "--format=%(refname:short)",
        ],
    ) else {
        return Vec::new();
    };
    // Only re-read when the prune above could have changed the listing; report
    // mode reuses the inventory it already has.
    let live = if apply {
        match git(&inventory.primary, &["worktree", "list", "--porcelain"]) {
            Ok(listing) => Inventory::parse(inventory.primary.clone(), &listing),
            Err(_) => return Vec::new(),
        }
    } else {
        Inventory {
            primary: inventory.primary.clone(),
            registered: inventory.registered.clone(),
            locked: inventory.locked.clone(),
        }
    };

    let mut orphans = Vec::new();
    for branch in listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some(slug) = branch.strip_prefix("brgr/task-") else {
            continue;
        };
        if parse_slug(slug).is_none() {
            continue;
        }
        // Still checked out somewhere under our root? Then it is not an orphan and
        // the per-worktree pass above owns it. The directory has to actually
        // exist: in report mode `git worktree prune` has not run, so a worktree
        // removed by hand is still listed while its checkout is gone, which is
        // precisely the case this reclaims.
        if live
            .registered
            .iter()
            .any(|path| path.starts_with(&root) && file_name(path) == slug && path.is_dir())
        {
            continue;
        }
        let outcome = if apply {
            match git(&inventory.primary, &["branch", "-d", branch]) {
                Ok(_) => Outcome::Removed,
                Err(reason) => Outcome::Kept(format!("git refused to delete it: {reason}")),
            }
        } else {
            Outcome::Removable
        };
        orphans.push(Orphan {
            branch: branch.to_owned(),
            outcome,
        });
    }
    orphans
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
    inventory: Option<&Inventory>,
    current_dir: Option<&Path>,
    include_ignored: bool,
) -> Assessment {
    let mut found = Assessment {
        blocked: None,
        owner: None,
        ignored: Vec::new(),
    };
    found.blocked = objection(
        worktree,
        slug,
        store,
        inventory,
        current_dir,
        include_ignored,
        &mut found,
    );
    found
}

/// Returns the first reason this worktree must be kept, or `None` to remove it.
fn objection(
    worktree: &Path,
    slug: &str,
    store: &Store,
    inventory: Option<&Inventory>,
    current_dir: Option<&Path>,
    include_ignored: bool,
    found: &mut Assessment,
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
        && is_self_or_ancestor(&resolved, cwd)
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
    found.owner = Some(task.owner_id.as_str().to_owned());

    // A recorded decision is what makes this revision settled, and that is
    // stronger than it looks. `record_decision_in_transaction` rejects anything
    // whose outcome is not `Candidate`, and `claim_attempt` refuses a new attempt
    // on a revision whose prior result is a candidate. So a decided revision can
    // have no attempt still running in this worktree — the two guards live in
    // brgr-store, which is why it is spelled out here.
    match store.decision_for_revision(task.task_id, revision) {
        Ok(Some(_)) => {}
        Ok(None) => return Some("no owner decision recorded for this revision".to_owned()),
        Err(error) => return Some(format!("decision lookup failed: {error}")),
    }

    // Ask git the same questions `--apply` would, so `removable` means the apply
    // run will remove it rather than discover a veto later.
    let Some(inventory) = inventory else {
        return Some("repository worktree inventory is unreadable".to_owned());
    };
    if !inventory.is_registered(&resolved) {
        return Some("path is not a registered worktree of its repository".to_owned());
    }
    if inventory.is_locked(&resolved) {
        return Some("worktree is locked; unlock it first with `git worktree unlock`".to_owned());
    }

    // One status call answers both questions: tracked or untracked changes, and
    // the ignored set that git's own clean check does not look at.
    let status = match git(worktree, &["status", "--porcelain", "--ignored"]) {
        Ok(status) => status,
        Err(reason) => return Some(format!("worktree status is unreadable: {reason}")),
    };
    if status
        .lines()
        .any(|line| !line.is_empty() && !line.starts_with("!! "))
    {
        return Some("worktree holds modified or untracked files".to_owned());
    }
    found.ignored = status
        .lines()
        .filter_map(|line| line.strip_prefix("!! "))
        .map(str::to_owned)
        .collect();
    if !found.ignored.is_empty() && !include_ignored {
        return Some(format!(
            "worktree holds {} ignored path(s) git's clean check cannot see, such as {}; \
             pass --include-ignored to remove them",
            found.ignored.len(),
            found.ignored.first().map_or("", String::as_str)
        ));
    }
    None
}

/// Lists the repository directories under the worktrees root, and the ones that
/// could not be read.
///
/// An unreadable entry is returned rather than propagated: every other failure in
/// this module becomes one reported row, and enumeration was the last place where
/// a single bad directory made the whole sweep report nothing — including the
/// worktrees it could have reclaimed elsewhere.
fn repository_directories(worktrees_root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    if !worktrees_root.is_dir() {
        return (Vec::new(), Vec::new());
    }
    let (children, mut unreadable) = child_directories(worktrees_root);
    let mut repositories = Vec::new();
    for child in children {
        if fs::read_dir(&child).is_ok() {
            repositories.push(child);
        } else {
            unreadable.push(child);
        }
    }
    (repositories, unreadable)
}

/// Sorted children of `directory`, skipping dot-entries. The second half is the
/// directory itself when it cannot be read at all.
fn child_directories(directory: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(directory) else {
        return (Vec::new(), vec![directory.to_path_buf()]);
    };
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) if !file_name(&entry.path()).starts_with('.') => found.push(entry.path()),
            Ok(_) => {}
            Err(_) => unreadable.push(directory.to_path_buf()),
        }
    }
    found.sort();
    unreadable.dedup();
    (found, unreadable)
}

fn unreadable_entry(path: PathBuf) -> Entry {
    let slug = file_name(&path);
    Entry {
        worktree: path,
        slug,
        owner: None,
        ignored: Vec::new(),
        outcome: Outcome::Kept("directory could not be read".to_owned()),
    }
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
    // `task_slug` renders a UUID, which is lowercase. Accepting `A-F` would let a
    // directory brgr never created resolve to a real task on a case-sensitive
    // filesystem, and the branch name rebuilt from the raw slug would not exist.
    if revision == 0
        || prefix.len() != PREFIX_LENGTH
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some((prefix, revision))
}

/// Removes one worktree and then its branch.
///
/// `Ok(None)` removed both. `Ok(Some(reason))` removed the checkout while git
/// kept the branch, which preserves committed work.
fn remove(
    worktree: &Path,
    slug: &str,
    inventory: Option<&Inventory>,
) -> Result<Option<String>, String> {
    let primary = match inventory {
        Some(inventory) => inventory.primary.clone(),
        None => primary_checkout(worktree)?,
    };
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

/// One repository's worktree registrations, read once for all of its candidates.
///
/// Every candidate under `<worktrees>/<repository>` belongs to the same primary
/// checkout, so asking git per candidate re-ran an identical listing: a control
/// home with a hundred settled worktrees paid hundreds of process spawns for one
/// report.
struct Inventory {
    primary: PathBuf,
    registered: Vec<PathBuf>,
    locked: Vec<PathBuf>,
}

impl Inventory {
    /// Loads the inventory from the first candidate that is a usable checkout.
    fn load(candidates: &[PathBuf]) -> Option<Self> {
        candidates.iter().find_map(|candidate| {
            let listing = git(candidate, &["worktree", "list", "--porcelain"]).ok()?;
            let primary = listing
                .lines()
                .find_map(|line| line.strip_prefix("worktree "))
                .map(PathBuf::from)?;
            Some(Self::parse(primary, &listing))
        })
    }

    fn parse(primary: PathBuf, listing: &str) -> Self {
        let mut registered = Vec::new();
        let mut locked = Vec::new();
        let mut current: Option<PathBuf> = None;
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                let path = canonical(Path::new(path)).unwrap_or_else(|| PathBuf::from(path));
                registered.push(path.clone());
                current = Some(path);
            } else if (line == "locked" || line.starts_with("locked "))
                && let Some(path) = current.clone()
            {
                locked.push(path);
            }
        }
        Self {
            primary,
            registered,
            locked,
        }
    }

    fn is_registered(&self, resolved: &Path) -> bool {
        self.registered.iter().any(|path| path == resolved)
    }

    /// `git worktree remove` refuses a locked worktree, so report mode has to see
    /// the lock too or it promises a removal that apply mode then declines.
    fn is_locked(&self, resolved: &Path) -> bool {
        self.locked.iter().any(|path| path == resolved)
    }
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

/// Reports whether `candidate` is `inside` itself or one of its ancestors, which
/// is what makes removing it pull the ground out from under `inside`.
fn is_self_or_ancestor(candidate: &Path, inside: &Path) -> bool {
    inside == candidate || inside.starts_with(candidate)
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
    fn slugs_reject_uppercase_hex_that_task_slug_could_not_emit() {
        assert_eq!(parse_slug("abcd1234"), Some(("abcd1234", 1)));
        // A UUID renders lowercase, so these name a directory brgr never created.
        assert_eq!(parse_slug("ABCD1234"), None);
        assert_eq!(parse_slug("abcD1234"), None);
        assert_eq!(parse_slug("ABCD1234-r2"), None);
    }

    #[test]
    fn the_cwd_guard_covers_the_directory_holding_the_worktrees() {
        let repository = Path::new("/home/brgr/worktrees/repo");
        let worktree = repository.join("abcd1234");
        // Sitting in the worktree blocks the worktree.
        assert!(is_self_or_ancestor(&worktree, &worktree));
        assert!(is_self_or_ancestor(&worktree, &worktree.join("src")));
        // Sitting in the repository directory blocks that directory too, which the
        // per-worktree check alone cannot see.
        assert!(is_self_or_ancestor(repository, repository));
        assert!(is_self_or_ancestor(repository, &worktree));
        // An unrelated directory blocks nothing.
        assert!(!is_self_or_ancestor(&worktree, repository));
        assert!(!is_self_or_ancestor(
            &worktree,
            Path::new("/home/brgr/worktrees/other/abcd1234")
        ));
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
