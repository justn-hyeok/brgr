//! Removal of brgr-owned task worktrees and the branches they created.
//!
//! [`crate::workspace`] creates task worktrees and never deletes, so removal
//! lives here: behind `brgr prune`, which defaults to reporting, and
//! [`reclaim_task`], which a decision runs for its own revision.
//!
//! git is used for the two removals, but it is deliberately **not** trusted as
//! the only safety authority. `git worktree remove` refuses a worktree with
//! modified or untracked files, and is forced only when brgr has shown every
//! such change is disposable: an untracked harness cache (`.gjc/`, `.claude/`
//! and the like), or a worktree whose patch is byte-for-byte the diff the store
//! sealed with the result. A task branch goes only
//! when every commit on it survives elsewhere: in `HEAD`, or on another branch
//! or remote-tracking branch — a task started from a feature branch carries that
//! branch's commits, which `git branch -d` would count as unmerged forever. A
//! branch holding a commit found nowhere else is kept. git's clean check runs
//! `git status --porcelain` without `--ignored`, so a `.env`, a downloaded
//! credential, or a build cache is invisible to it and would be deleted
//! silently. A task worktree is exactly where an agent has been working, which
//! makes it the most likely place in a repository to hold such a file. This
//! module therefore checks the ignored set itself and keeps the worktree unless
//! the caller opts in, or every ignored path lies in a directory a build or a
//! package manager recreates (`node_modules`, `target`, `.next` and the like).
//!
//! Every candidate must also be a real directory (not a symlink), a worktree git
//! itself has registered for its repository and has not locked, a name
//! [`crate::workspace`] could have produced, outside the current working
//! directory, and the checkout of a settled task revision: a candidate the owner
//! decided, or a failed, cancelled, or lost result the owner acknowledged, with
//! no attempt or granted retry left and no worker process still alive.
//!
//! The repositories come from the store, never from the worktrees root. Each
//! candidate is judged against the repository its own task was admitted in; a
//! directory's name is only a name, and a checkout of some other repository can
//! be dropped under the root by hand. An earlier version asked whichever
//! directory sorted first and trusted the answer, so a stray `Backup` checkout
//! made `--apply` delete branches in a repository brgr had never touched.
//!
//! A sweep also reclaims the `brgr/task-*` branches left behind by worktrees that
//! no longer exist — removing one by hand was the only reclamation available
//! before this command. Those branches get the same checks as a worktree: a task
//! revision of that repository, a recorded decision, a reported owner, and a
//! merge git will accept. Stale registrations are removed one path at a time;
//! `git worktree prune` is repository-wide and would also drop the registration
//! of a user's own worktree on an unmounted volume. One unreadable directory is
//! reported as a single kept row rather than ending the sweep, and each
//! repository is read from git once.
//!
//! No sealed result, decision, artifact, or task row is ever removed. Those cost
//! roughly 9 KiB per task; a worktree costs the size of the checkout.
//!
//! Cross-owner isolation is explicitly outside v1 scope, so a prune acts on
//! every settled task in this control home. Each row reports its owner so that
//! is visible rather than silent.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;
use brgr_protocol::{TaskId, TaskSpec, TerminalOutcome};
use brgr_store::{OpenReason, Settlement, Store};
use serde_json::json;

use crate::{Paths, print_value};

/// Everything one sweep found: the worktrees it walked, and the branches left
/// behind by worktrees that no longer exist.
pub(crate) struct Prune {
    pub(crate) entries: Vec<Entry>,
    pub(crate) orphans: Vec<Orphan>,
}

/// A `brgr/task-*` branch whose worktree is gone.
pub(crate) struct Orphan {
    pub(crate) branch: String,
    pub(crate) owner: Option<String>,
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
    worker_alive: &dyn Fn(TaskId) -> bool,
    apply: bool,
    include_ignored: bool,
) -> Prune {
    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|path| canonical(&path));
    let sweep = Sweep {
        store,
        current_dir: current_dir.as_deref(),
        include_ignored,
        worker_alive,
    };
    let mut entries = Vec::new();
    let mut orphans = Vec::new();

    let mut known = Repositories::load(store);
    let mut unlocated = Vec::new();

    let (directories, unreadable) = repository_directories(worktrees_root);
    entries.extend(unreadable.into_iter().map(unreadable_entry));

    for directory in directories {
        let (candidates, unreadable) = child_directories(&directory);
        entries.extend(unreadable.into_iter().map(unreadable_entry));
        let directory_name = file_name(&directory);
        // Only a directory with nothing left in it can hide a repository: any
        // candidate gets its own row with its own reason.
        let emptied_by_hand = candidates.is_empty();

        let mut removed_any = false;
        for worktree in candidates {
            let slug = file_name(&worktree);
            let assessment = assess(&worktree, &slug, &sweep, &mut known);
            let outcome = match (assessment.blocked, &assessment.primary) {
                (Some(reason), _) => Outcome::Kept(reason),
                (None, Some(primary)) if apply => {
                    match remove(&worktree, &slug, primary, assessment.force) {
                        Ok(None) => {
                            removed_any = true;
                            Outcome::Removed
                        }
                        Ok(Some(reason)) => {
                            removed_any = true;
                            Outcome::RemovedKeepingBranch(reason)
                        }
                        Err(reason) => Outcome::Kept(reason),
                    }
                }
                (None, Some(_)) => Outcome::Removable,
                (None, None) => Outcome::Kept("no repository was resolved for it".to_owned()),
            };
            entries.push(Entry {
                worktree,
                slug,
                owner: assessment.owner,
                ignored: assessment.ignored,
                outcome,
            });
        }

        if apply && removed_any {
            remove_if_emptied(&directory, current_dir.as_deref());
        }
        if emptied_by_hand {
            unlocated.push(directory_name);
        }
    }
    // A directory whose repository nobody could name: every checkout in it is
    // gone and brgr did not record where it came from (tasks admitted before
    // checkouts were recorded). Its branches may remain, and saying so beats a
    // report of zero.
    unlocated.retain(|name| {
        !known
            .inventories
            .iter()
            .any(|inventory| file_name(&inventory.primary) == *name)
    });
    for name in unlocated {
        let directory = worktrees_root.join(&name);
        if directory.is_dir() {
            entries.push(Entry {
                worktree: directory,
                slug: name,
                owner: None,
                ignored: Vec::new(),
                outcome: Outcome::Kept(
                    "no checkout of this repository survives and brgr did not record its path, \
                     so its brgr/task-* branches cannot be located; remove them from the \
                     repository with `git branch -d`"
                        .to_owned(),
                ),
            });
        }
    }

    // Once per repository brgr has tasks in, not once per directory: two
    // directories can resolve to one repository, and a repository can have no
    // directory left at all — which is exactly the case this exists for.
    for inventory in &known.inventories {
        orphans.extend(reconcile_orphans(inventory, store, apply));
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
/// A branch gets the checks a worktree gets: it must name a task revision of
/// this repository with a recorded decision, and its owner is reported. git
/// still holds the last line — `branch -d` refuses unmerged commits — but report
/// mode asks the same merge question first, so `removable` means removable.
fn reconcile_orphans(inventory: &Inventory, store: &Store, apply: bool) -> Vec<Orphan> {
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
    let mut orphans = Vec::new();
    for branch in listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some((prefix, revision)) = branch.strip_prefix("brgr/task-").and_then(parse_slug)
        else {
            continue;
        };
        let mut owner = None;
        let judged = orphan_objection(inventory, store, (prefix, revision), branch, &mut owner);
        let outcome = match judged {
            // Still checked out: the per-worktree pass owns it.
            Err(Live) => continue,
            Ok(Some(reason)) => Outcome::Kept(reason),
            Ok(None) if apply => reclaim(inventory, store, (prefix, revision), branch),
            Ok(None) => Outcome::Removable,
        };
        orphans.push(Orphan {
            branch: branch.to_owned(),
            owner,
            outcome,
        });
    }
    orphans
}

/// The branch's worktree still exists, so it is not an orphan.
struct Live;

/// The task revision of *this* repository a branch belongs to.
///
/// A recorded checkout decides it. A task admitted before checkouts were
/// recorded belongs here when git still registers its worktree path, which it
/// does after a hand deletion until the registration is removed.
fn owning_task(
    inventory: &Inventory,
    store: &Store,
    (prefix, revision): (&str, u32),
) -> Result<Option<TaskSpec>, String> {
    let matches = store
        .task_revisions_with_prefix(prefix, revision)
        .map_err(|error| format!("task lookup failed: {error}"))?;
    let mut ours = Vec::new();
    for task in matches {
        let belongs = match store.task_checkout(task.task_id, revision) {
            Ok(Some(recorded)) => resolve(Path::new(&recorded)) == inventory.primary,
            Ok(None) => inventory.is_registered(&resolve(Path::new(&task.workspace))),
            Err(error) => return Err(format!("checkout lookup failed: {error}")),
        };
        if belongs {
            ours.push(task);
        }
    }
    match ours.len() {
        0 | 1 => Ok(ours.pop()),
        _ => Err("task id prefix is ambiguous; refusing to guess".to_owned()),
    }
}

/// Why a revision's checkout must stay, or `None` once it is settled.
///
/// A revision is settled when the owner decided its candidate, or acknowledged
/// its failed, cancelled, or lost result with nothing left to run. The store
/// guards make that stronger than it looks: `record_decision_in_transaction`
/// accepts only a candidate, `claim_attempt` refuses a new attempt after a
/// candidate or a lost result, and after a failure only with a retry grant,
/// which counts as unsettled. What the store cannot see is a worker still
/// running under a dead supervisor, which a lost result warns about; with a
/// worktree present, `worker_alive` rules that out.
fn unsettled(
    store: &Store,
    task: TaskId,
    revision: u32,
    worker_alive: Option<&dyn Fn(TaskId) -> bool>,
) -> Option<String> {
    let settlement = match store.revision_settlement(task, revision) {
        Ok(settlement) => settlement,
        Err(error) => return Some(format!("settlement lookup failed: {error}")),
    };
    match settlement {
        Settlement::Acknowledged(_) if worker_alive.is_some_and(|alive| alive(task)) => {
            Some("a worker of this task is still running".to_owned())
        }
        Settlement::Decided | Settlement::Acknowledged(_) => None,
        Settlement::Open(OpenReason::NoResult) => {
            Some("no result recorded for this revision yet".to_owned())
        }
        Settlement::Open(OpenReason::Undecided) => {
            Some("no owner decision recorded for this revision".to_owned())
        }
        Settlement::Open(OpenReason::Unacknowledged(outcome)) => Some(format!(
            "its {} result is not acknowledged; run `brgr result {task} --ack`",
            outcome_name(outcome)
        )),
        Settlement::Open(OpenReason::AttemptActive) => {
            Some("an attempt of this revision is still running".to_owned())
        }
        Settlement::Open(OpenReason::RetryGranted) => {
            Some("a retry was granted for this revision and may still start".to_owned())
        }
    }
}

fn outcome_name(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::Candidate => "candidate",
        TerminalOutcome::Failed => "failed",
        TerminalOutcome::Cancelled => "cancelled",
        TerminalOutcome::Lost => "lost",
    }
}

/// Returns why an orphan branch must be kept, `Ok(None)` when it may go, or
/// `Err(Live)` when its worktree still exists.
fn orphan_objection(
    inventory: &Inventory,
    store: &Store,
    (prefix, revision): (&str, u32),
    branch: &str,
    owner: &mut Option<String>,
) -> Result<Option<String>, Live> {
    let task = match owning_task(inventory, store, (prefix, revision)) {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Ok(Some(
                "no task revision of this repository matches this branch".to_owned(),
            ));
        }
        Err(reason) => return Ok(Some(reason)),
    };
    let worktree = resolve(Path::new(&task.workspace));
    if worktree.is_dir() {
        return Err(Live);
    }
    *owner = Some(task.owner_id.as_str().to_owned());
    // Its worktree is gone, so no worker can still be running in it.
    if let Some(reason) = unsettled(store, task.task_id, revision, None) {
        return Ok(Some(reason));
    }
    if inventory.is_locked(&worktree) {
        return Ok(Some(
            "its stale worktree registration is locked; unlock it first with `git worktree unlock`"
                .to_owned(),
        ));
    }
    match preserved_elsewhere(&inventory.primary, branch) {
        Ok(Some(_)) => Ok(None),
        Ok(None) => Ok(Some(UNPRESERVED.to_owned())),
        Err(reason) => Ok(Some(format!("branch could not be checked: {reason}"))),
    }
}

/// Drops the task's own stale registration, then the branch. Never `git
/// worktree prune`: that is repository-wide and would also drop the
/// registration of a user's worktree on an unmounted volume.
fn reclaim(
    inventory: &Inventory,
    store: &Store,
    (prefix, revision): (&str, u32),
    branch: &str,
) -> Outcome {
    let Ok(Some(task)) = owning_task(inventory, store, (prefix, revision)) else {
        return Outcome::Kept("task could not be re-read before removal".to_owned());
    };
    let worktree = resolve(Path::new(&task.workspace));
    if inventory.is_registered(&worktree)
        && let Err(error) = git(
            &inventory.primary,
            &["worktree", "remove", &lossy(&worktree)],
        )
    {
        return Outcome::Kept(format!(
            "stale registration {} could not be removed: {error}",
            worktree.display()
        ));
    }
    match delete_task_branch(&inventory.primary, branch) {
        Ok(()) => Outcome::Removed,
        Err(reason) => Outcome::Kept(reason),
    }
}

/// What the checks found for one candidate. `blocked` is `Some` when the
/// worktree is kept, and the same checks run in report and apply mode so the two
/// runs agree on what is removable.
struct Assessment {
    blocked: Option<String>,
    owner: Option<String>,
    ignored: Vec<String>,
    /// The worktree holds changes that git's own remove refuses, all of which
    /// brgr has shown are disposable: a harness's cache, or exactly the patch
    /// the store sealed. Only then is `git worktree remove --force` used.
    force: bool,
    /// The primary checkout of the repository this worktree's task belongs to.
    primary: Option<PathBuf>,
}

/// What stays the same for every worktree in one sweep.
struct Sweep<'a> {
    store: &'a Store,
    current_dir: Option<&'a Path>,
    include_ignored: bool,
    /// Whether a worker of this task may still be running where the store
    /// cannot see it: a live supervisor, or a harness process whose supervisor
    /// died without reaping it.
    worker_alive: &'a dyn Fn(TaskId) -> bool,
}

fn assess(worktree: &Path, slug: &str, sweep: &Sweep<'_>, known: &mut Repositories) -> Assessment {
    let mut found = Assessment {
        blocked: None,
        owner: None,
        ignored: Vec::new(),
        force: false,
        primary: None,
    };
    found.blocked = objection(worktree, slug, sweep, known, &mut found);
    found
}

/// Returns the first reason this worktree must be kept, or `None` to remove it.
fn objection(
    worktree: &Path,
    slug: &str,
    sweep: &Sweep<'_>,
    known: &mut Repositories,
    found: &mut Assessment,
) -> Option<String> {
    let Sweep {
        store,
        current_dir,
        include_ignored,
        worker_alive,
    } = *sweep;
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

    let matches = match store.task_revisions_with_prefix(prefix, revision) {
        Ok(matches) => matches,
        Err(error) => return Some(format!("task lookup failed: {error}")),
    };
    // The store records the exact worktree it created for each revision. A
    // directory under the right name that is not that path — a copy, a clone,
    // one someone made — was not put here by brgr. This also settles a prefix
    // shared by two tasks, which the name alone cannot.
    let Some(task) = matches
        .into_iter()
        .find(|task| resolve(Path::new(&task.workspace)) == resolved)
    else {
        return Some("no task revision was admitted in this directory".to_owned());
    };
    found.owner = Some(task.owner_id.as_str().to_owned());

    // Locate the repository before anything can return early: a worktree kept
    // for any reason still tells the sweep which repository its siblings'
    // orphaned branches live in. Ask the repository this task came from, never
    // whatever directory sits beside it.
    let recorded = match store.task_checkout(task.task_id, revision) {
        Ok(recorded) => recorded,
        Err(error) => return Some(format!("checkout lookup failed: {error}")),
    };
    let Some(index) = known.locate(recorded.as_deref(), worktree, &resolved) else {
        return Some("worktree is not a usable git checkout".to_owned());
    };
    let primary = known.inventories[index].primary.clone();
    if let Some(recorded) = recorded
        && resolve(Path::new(&recorded)) != primary
    {
        return Some(format!(
            "brgr recorded this worktree under {recorded}, but git places it in {}",
            primary.display()
        ));
    }

    if let Some(reason) = unsettled(store, task.task_id, revision, Some(worker_alive)) {
        return Some(reason);
    }

    let inventory = &known.inventories[index];
    if !inventory.is_registered(&resolved) {
        return Some("path is not a registered worktree of its repository".to_owned());
    }
    if inventory.is_locked(&resolved) {
        return Some("worktree is locked; unlock it first with `git worktree unlock`".to_owned());
    }

    if let Some(reason) = changes_objection(worktree, store, &task, include_ignored, found) {
        return Some(reason);
    }
    found.primary = Some(inventory.primary.clone());
    None
}

/// The reason a worktree's own contents keep it, or `None`. Sets
/// `found.force` and `found.ignored` for the removal.
fn changes_objection(
    worktree: &Path,
    store: &Store,
    task: &TaskSpec,
    include_ignored: bool,
    found: &mut Assessment,
) -> Option<String> {
    // One status call answers both questions: tracked or untracked changes, and
    // the ignored set that git's own clean check does not look at. `matching`
    // lists each ignored path at the level its pattern matched: the default
    // folds a directory whose files are all ignored into one entry, so a
    // `build/prod.env` caught by `*.env` would read as `build/`.
    let status = match git(worktree, &["status", "--porcelain", "--ignored=matching"]) {
        Ok(status) => status,
        Err(reason) => return Some(format!("worktree status is unreadable: {reason}")),
    };
    let changed: Vec<&str> = status
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("!! "))
        .collect();
    if !changed.is_empty() {
        // Only a harness's own cache, or exactly what the store sealed: either
        // way nothing in the worktree exists only there.
        if changed.iter().all(|line| harness_cache(line)) || patch_is_sealed(store, task) {
            found.force = true;
        } else {
            return Some("worktree holds modified or untracked files".to_owned());
        }
    }
    found.ignored = status
        .lines()
        .filter_map(|line| line.strip_prefix("!! "))
        .map(str::to_owned)
        .collect();
    let kept_ignored: Vec<&String> = found
        .ignored
        .iter()
        .filter(|path| !regenerable(path))
        .collect();
    if !kept_ignored.is_empty() && !include_ignored {
        return Some(format!(
            "worktree holds {} ignored path(s) git's clean check cannot see, such as {}; \
             pass --include-ignored to remove them",
            kept_ignored.len(),
            kept_ignored.first().map_or("", |path| path.as_str())
        ));
    }
    None
}

/// An untracked directory a worker harness keeps only its own session state
/// in. Only an untracked entry counts: a change to a tracked file under one of
/// these names is the repository's own content. Directories such as `.claude/`
/// or `.cursor/` are not here: they also hold project configuration a task may
/// be asked to write (`.claude/commands/`, `.cursor/rules/`), and git reports
/// either as the bare directory.
fn harness_cache(status_line: &str) -> bool {
    const CACHES: [&str; 2] = [".gjc/", ".commandcode/"];
    status_line
        .strip_prefix("?? ")
        .is_some_and(|path| CACHES.iter().any(|cache| path.starts_with(cache)))
}

/// An ignored directory a build or a package manager recreates: an entry git
/// reported as a directory whose own name is a well-known output or dependency
/// directory. A file is never regenerable, wherever it sits.
fn regenerable(path: &str) -> bool {
    const REGENERABLE: [&str; 12] = [
        "node_modules",
        "target",
        ".next",
        ".turbo",
        "dist",
        "build",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".venv",
        ".gradle",
        "coverage",
    ];
    path.strip_suffix('/')
        .map(|directory| directory.rsplit('/').next().unwrap_or(directory))
        .is_some_and(|name| REGENERABLE.contains(&name))
}

/// Whether the worktree's changes are exactly the patch sealed with the
/// revision's result, so removing the checkout loses nothing the store lacks.
fn patch_is_sealed(store: &Store, task: &TaskSpec) -> bool {
    if !task.evidence.capture_diff {
        return false;
    }
    let Ok(Some(result)) = store.revision_result(task.task_id, task.revision) else {
        return false;
    };
    let Some(sealed) = result
        .artifacts
        .iter()
        .find(|artifact| artifact.media_type == "text/x-diff")
    else {
        return false;
    };
    let limit = task.artifact_contract.max_bytes.min(8 * 1024 * 1024);
    let Ok(sealed) = store.read_artifact(sealed, limit) else {
        return false;
    };
    brgr_core::task_patch(task, limit, std::time::Duration::from_secs(30))
        .is_ok_and(|now| now == sealed)
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
        // The suffix has to be the one `task_slug` would have written. Rust's
        // integer parser accepts a leading `+` and leading zeros, so `-r+5`,
        // `-r007`, and `-r01` all resolved to real revisions; `-r1` did too,
        // although `task_slug` drops the suffix entirely at revision one. Each
        // named a directory brgr never created, and each would then have its
        // checkout removed and a branch rebuilt from the raw slug that does not
        // exist. Re-rendering is the check: a suffix that does not round-trip is
        // not ours. Found by `a_parsed_slug_is_always_one_brgr_could_have_written`.
        Some((prefix, revision)) => {
            let parsed = revision.parse::<u32>().ok()?;
            if parsed < 2 || revision != parsed.to_string() {
                return None;
            }
            (prefix, parsed)
        }
        None => (slug, 1),
    };
    // `task_slug` renders a UUID, which is lowercase. Accepting `A-F` would let a
    // directory brgr never created resolve to a real task on a case-sensitive
    // filesystem, and the branch name rebuilt from the raw slug would not exist.
    if prefix.len() != PREFIX_LENGTH
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
    primary: &Path,
    force: bool,
) -> Result<Option<String>, String> {
    let path = lossy(worktree);
    let mut arguments = vec!["worktree", "remove"];
    if force {
        arguments.push("--force");
    }
    arguments.push(&path);
    git(primary, &arguments)
        .map_err(|error| format!("git declined to remove the worktree: {error}"))?;
    let branch = format!("brgr/task-{slug}");
    if let Err(reason) = delete_task_branch(primary, &branch) {
        return Ok(Some(format!(
            "checkout removed; branch {branch} kept: {reason}"
        )));
    }
    Ok(None)
}

/// Every repository brgr has worktrees in, each read from git once.
///
/// Seeded from the checkouts the store recorded at admission, so a directory
/// dropped under the worktrees root by hand is never asked which repository it
/// belongs to. A task admitted before checkouts were recorded is located from
/// its own worktree, once the store has confirmed brgr created that path.
struct Repositories {
    inventories: Vec<Inventory>,
}

impl Repositories {
    fn load(store: &Store) -> Self {
        let mut known = Self {
            inventories: Vec::new(),
        };
        for primary in store.task_checkouts().unwrap_or_default() {
            if let Some(inventory) = Inventory::load(Path::new(&primary)) {
                known.adopt(inventory);
            }
        }
        known
    }

    fn adopt(&mut self, inventory: Inventory) -> usize {
        if let Some(index) = self
            .inventories
            .iter()
            .position(|known| known.primary == inventory.primary)
        {
            return index;
        }
        self.inventories.push(inventory);
        self.inventories.len() - 1
    }

    /// The repository holding a store-verified worktree: its recorded checkout,
    /// else one already read that registers it, else the worktree's own answer.
    fn locate(
        &mut self,
        recorded: Option<&str>,
        worktree: &Path,
        resolved: &Path,
    ) -> Option<usize> {
        if let Some(recorded) = recorded {
            let primary = resolve(Path::new(recorded));
            if let Some(index) = self
                .inventories
                .iter()
                .position(|known| known.primary == primary)
            {
                return Some(index);
            }
        }
        if let Some(index) = self
            .inventories
            .iter()
            .position(|known| known.is_registered(resolved))
        {
            return Some(index);
        }
        Inventory::load(worktree).map(|inventory| self.adopt(inventory))
    }
}

/// One repository's worktree registrations.
struct Inventory {
    primary: PathBuf,
    registered: Vec<PathBuf>,
    locked: Vec<PathBuf>,
}

impl Inventory {
    /// Reads the repository that `checkout` belongs to.
    fn load(checkout: &Path) -> Option<Self> {
        let listing = git(checkout, &["worktree", "list", "--porcelain"]).ok()?;
        let primary = listing
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
            .map(|path| resolve(Path::new(path)))?;
        Some(Self::parse(primary, &listing))
    }

    fn parse(primary: PathBuf, listing: &str) -> Self {
        let mut registered = Vec::new();
        let mut locked = Vec::new();
        let mut current: Option<PathBuf> = None;
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                let path = resolve(Path::new(path));
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

const UNPRESERVED: &str = "branch has commits not merged into the repository's HEAD and \
     found on no other branch or remote; deleting it would lose them";

/// Where every commit on a task branch also lives, or `None` when some commit
/// is on this branch alone. Returns the branch tip it checked, and the holder:
/// `HEAD`, or the first other branch or remote-tracking branch containing it.
/// Other task branches do not count, since a sweep may delete them too.
fn preserved_elsewhere(primary: &Path, branch: &str) -> Result<Option<(String, String)>, String> {
    let tip = git(
        primary,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )?
    .trim()
    .to_owned();
    if git(primary, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok() {
        return Ok(Some((tip, "HEAD".to_owned())));
    }
    let holders = git(
        primary,
        &[
            "for-each-ref",
            "--contains",
            &tip,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let holder = holders
        .lines()
        .map(str::trim)
        .find(|name| {
            !name.is_empty()
                && !name.starts_with("refs/heads/brgr/task-")
                && !name.ends_with("/HEAD")
        })
        .map(str::to_owned);
    Ok(holder.map(|holder| (tip, holder)))
}

/// Deletes a task branch whose commits all survive elsewhere.
///
/// `git branch -D` is used for its refusal to delete a branch some worktree has
/// checked out; the force only skips its merge test, which
/// [`preserved_elsewhere`] replaces. Should the branch have moved between the
/// check and the deletion, it is recreated at the commit git reports it had.
fn delete_task_branch(primary: &Path, branch: &str) -> Result<(), String> {
    let Some((tip, _)) = preserved_elsewhere(primary, branch)? else {
        return Err(UNPRESERVED.to_owned());
    };
    let deleted = git(primary, &["branch", "-D", branch])
        .map_err(|error| format!("git refused to delete it: {error}"))?;
    // "Deleted branch NAME (was ABBREV)."
    let was = deleted
        .rsplit("(was ")
        .next()
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_default()
        .trim();
    if was.is_empty() || !tip.starts_with(was) {
        let restored = if was.is_empty() { tip.as_str() } else { was };
        git(primary, &["branch", branch, restored]).map_err(|error| {
            format!("branch moved during removal and could not be restored: {error}")
        })?;
        return Err("branch moved while it was being removed; restored and kept".to_owned());
    }
    Ok(())
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

/// Canonical form of a path that may no longer exist. A stale registration's
/// directory is gone, but its parent usually is not, and comparing the raw text
/// against a canonical root misses it wherever a symlink sits in between —
/// `/tmp` on macOS, for one.
fn resolve(path: &Path) -> PathBuf {
    if let Some(found) = canonical(path) {
        return found;
    }
    match (path.parent().and_then(canonical), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
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

/// Removes one decided revision's worktree when nothing in it would be lost,
/// with the same checks as `brgr prune --apply` and never `--include-ignored`.
/// Returns what happened, for a note on stderr; `None` when there was nothing
/// to do. Never fails the decision that triggered it.
pub(crate) fn reclaim_task(paths: &Paths, task: TaskId, revision: u32) -> Option<String> {
    let store = Store::open(&paths.store).ok()?;
    let launch: crate::LaunchEnvelope =
        serde_json::from_slice(&fs::read(paths.launch(task, revision)).ok()?).ok()?;
    if launch.keep_worktree || launch.keep_pane {
        return None;
    }
    let worktree = resolve(Path::new(&launch.spec.workspace));
    // Only a checkout brgr made under its own root; a non-Git task ran in
    // place, in the user's own directory.
    let root = canonical(&paths.worktrees)?;
    let parent = worktree.parent()?;
    if parent.parent() != Some(root.as_path()) || !worktree.is_dir() {
        return None;
    }
    // A pane-mode worker still runs in the worktree until its pane closes;
    // the pane cleanup calls this again once it has.
    if paths.pane_receipt(task, revision).exists() && !pane_closed(paths, task, revision) {
        return None;
    }
    if crate::supervision::worker_may_be_running(paths, task) {
        return None;
    }
    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|path| canonical(&path));
    let sweep = Sweep {
        store: &store,
        current_dir: current_dir.as_deref(),
        include_ignored: false,
        worker_alive: &|task| crate::supervision::worker_may_be_running(paths, task),
    };
    let slug = file_name(&worktree);
    let mut known = Repositories::load(&store);
    let assessment = assess(&worktree, &slug, &sweep, &mut known);
    let shown = worktree.display();
    match (assessment.blocked, assessment.primary) {
        (Some(reason), _) => Some(format!("kept worktree {shown}: {reason}")),
        (None, Some(primary)) => match remove(&worktree, &slug, &primary, assessment.force) {
            Ok(kept_branch) => {
                remove_if_emptied(parent, current_dir.as_deref());
                Some(match kept_branch {
                    None => format!("removed worktree {shown}"),
                    Some(reason) => format!("removed worktree {shown}; {reason}"),
                })
            }
            Err(reason) => Some(format!("kept worktree {shown}: {reason}")),
        },
        (None, None) => None,
    }
}

/// Runs [`reclaim_task`] and notes the outcome on stderr.
pub(crate) fn reclaim_and_report(paths: &Paths, task: TaskId, revision: u32) {
    if let Some(note) = reclaim_task(paths, task, revision) {
        eprintln!("brgr · {note}");
    }
}

/// Whether the revision's pane receipt says its pane has been closed.
fn pane_closed(paths: &Paths, task: TaskId, revision: u32) -> bool {
    fs::read(paths.pane_receipt(task, revision))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|receipt| receipt["cleanup"] == "closed")
}

/// Runs `brgr prune` and prints its report.
/// Reports, and with `apply` removes, task worktrees whose revision is decided.
///
/// Nothing in the store is removed: a worktree is a rebuildable checkout, while
/// a sealed result and its decision are the durable record brgr exists to keep.
pub(crate) fn command(
    paths: &Paths,
    apply: bool,
    include_ignored: bool,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let swept = prune(
        &paths.worktrees,
        &store,
        &|task| crate::supervision::worker_may_be_running(paths, task),
        apply,
        include_ignored,
    );
    let worktrees: Vec<serde_json::Value> = swept
        .entries
        .iter()
        .map(|entry| {
            let mut row = json!({
                "worktree": entry.worktree,
                "task_slug": entry.slug,
                "status": entry.outcome.code(),
            });
            if let Some(reason) = entry.outcome.reason() {
                row["reason"] = json!(reason);
            }
            if let Some(owner) = &entry.owner {
                // Pruning is not owner-scoped, so the owner is reported rather
                // than silently acted on.
                row["owner_id"] = json!(owner);
            }
            if !entry.ignored.is_empty() {
                row["ignored_paths"] = json!(entry.ignored);
            }
            row
        })
        .collect();
    let orphans: Vec<serde_json::Value> = swept
        .orphans
        .iter()
        .map(|orphan| {
            let mut row = json!({"branch": orphan.branch, "status": orphan.outcome.code()});
            if let Some(reason) = orphan.outcome.reason() {
                row["reason"] = json!(reason);
            }
            if let Some(owner) = &orphan.owner {
                row["owner_id"] = json!(owner);
            }
            row
        })
        .collect();
    // One pass each rather than one per reported figure.
    let mut tally = BTreeMap::<&str, usize>::new();
    for entry in &swept.entries {
        *tally.entry(entry.outcome.code()).or_default() += 1;
    }
    let mut orphan_tally = BTreeMap::<&str, usize>::new();
    for orphan in &swept.orphans {
        *orphan_tally.entry(orphan.outcome.code()).or_default() += 1;
    }
    let count = |code: &str| tally.get(code).copied().unwrap_or_default();
    let orphan_count = |code: &str| orphan_tally.get(code).copied().unwrap_or_default();
    // A checkout whose branch git kept is still reclaimed: its directory is gone
    // and will never be enumerated again, so counting it as kept would report
    // that nothing happened.
    let removed = count("removed") + count("removed_branch_kept");
    print_value(
        &json!({
            "applied": apply,
            "removed": removed,
            "removed_keeping_branch": count("removed_branch_kept"),
            "removable": count("removable"),
            "kept": count("kept"),
            "worktrees": worktrees,
            "orphan_branches_removed": orphan_count("removed"),
            "orphan_branches_removable": orphan_count("removable"),
            "orphan_branches_kept": orphan_count("kept"),
            "orphan_branches": orphans,
            "note": "sealed results, decisions, artifacts, and task rows are never removed",
        }),
        json_output,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_untracked_harness_directory_is_a_cache() {
        assert!(harness_cache("?? .gjc/"));
        assert!(harness_cache("?? .commandcode/"));
        assert!(!harness_cache("?? .claude/"));
        assert!(!harness_cache("?? .cursor/"));
        assert!(!harness_cache(" M .gjc/config.json"));
        assert!(!harness_cache("?? src/main.rs"));
        assert!(!harness_cache("?? .gjcx/"));
    }

    #[test]
    fn only_dependency_and_build_directories_are_regenerable() {
        assert!(regenerable("node_modules/"));
        assert!(regenerable("packages/web/.next/"));
        assert!(regenerable("target/"));
        assert!(!regenerable(".env"));
        assert!(!regenerable("secrets/credentials.json"));
        assert!(!regenerable("node_modules_backup.tar"));
        // A secret inside one of those directories is still a file.
        assert!(!regenerable("build/prod.env"));
        assert!(!regenerable("dist/.env"));
        assert!(!regenerable("node_modules/x/.env"));
    }
    use crate::adversary::Adversary;

    /// Every slug the parser accepts must be one `task_slug` would have written.
    ///
    /// The parser's job is not "does this look plausible" but "did brgr create
    /// this", because what follows an accepted slug is `git worktree remove`.
    /// Re-rendering the parse is the whole property: if the round trip does not
    /// land back on the input, the directory was named by someone else.
    ///
    /// This found four families the example tests missed — `-r+5`, `-r007`,
    /// `-r01`, and a plain `-r1` — all of which resolved to a live revision.
    #[test]
    fn a_parsed_slug_is_always_one_brgr_could_have_written() {
        // Structured, not free-form. Sixteen random characters over a mixed
        // alphabet essentially never lands on "eight hex digits, `-r`, a
        // revision", so a free-form generator explores only the rejection path
        // and the property never fires — confirmed by reverting the parser and
        // watching 4,096 free-form cases all pass. Building a slug from the
        // parts a real one has puts the cases where the decision is.
        // Weighted, not uniform. A uniform draw over lengths and an alphabet
        // half of which is uppercase left 11 of 4,096 cases parsing at all, so
        // the property almost never ran — the counter below is what caught it.
        // The point is to crowd the boundary, not to sample garbage evenly.
        const PREFIX_ALPHABET: &str = "0123456789abcdefabcdefabcdefABCDEF";
        // Each of these is a plausible revision suffix, and the ones that are
        // not canonical decimal are exactly the interesting half.
        const SUFFIXES: &[&str] = &[
            "1",
            "2",
            "10",
            "01",
            "007",
            "+5",
            "-3",
            "0",
            "",
            "x",
            " 2",
            "2 ",
            "4294967295",
            "4294967296",
            "99999999999999999999",
            "1_0",
            "2.0",
            "٣",
        ];
        let mut adversary = Adversary::new(0x5eed_0f0f_c0de_0002);

        // See the bridge property's counter: a generator that stops reaching the
        // accepting path leaves the assertion below unreached and the test green.
        let mut accepted = 0_u32;
        for case in 0..4_096 {
            // Eight characters most of the time; the other lengths are there to
            // keep the length check honest.
            let length = if adversary.below(4) == 0 {
                adversary.below(11)
            } else {
                8
            };
            let mut slug: String = (0..length)
                .map(|_| {
                    let letters: Vec<char> = PREFIX_ALPHABET.chars().collect();
                    letters[adversary.below(letters.len())]
                })
                .collect();
            match adversary.below(4) {
                // A bare slug, which is what revision one looks like.
                0 => {}
                // A suffix drawn from the plausible set.
                1 | 2 => {
                    let suffix = SUFFIXES[adversary.below(SUFFIXES.len())];
                    slug.push_str("-r");
                    slug.push_str(suffix);
                }
                // Free-form, so the rejection path keeps getting exercised too.
                _ => slug = adversary.text("0123456789abcdefABCDEFr-+ ._", 16),
            }
            let Some((prefix, revision)) = parse_slug(&slug) else {
                continue;
            };
            accepted += 1;
            // `task_slug`, spelled out rather than called: the two live in
            // different modules, and a test that reuses the renderer would pass
            // even if both agreed on the wrong thing.
            let rendered = if revision == 1 {
                prefix.to_owned()
            } else {
                format!("{prefix}-r{revision}")
            };
            assert_eq!(
                rendered, slug,
                "case {case}: {slug:?} parsed as ({prefix:?}, {revision}), which renders as \
                 {rendered:?} — brgr never created a directory by that name"
            );
        }
        assert!(
            accepted > 100,
            "only {accepted} of 4096 generated slugs parsed; the generator is no \
             longer reaching the path this asserts about"
        );
    }

    #[test]
    fn a_slug_suffix_has_to_be_the_one_task_slug_would_write() {
        // The four the property found, pinned so a future edit that reopens any
        // one of them fails by name rather than by seed.
        assert_eq!(parse_slug("3d3c9081-r+5"), None);
        assert_eq!(parse_slug("3d3c9081-r007"), None);
        assert_eq!(parse_slug("3d3c9081-r01"), None);
        assert_eq!(parse_slug("3d3c9081-r1"), None);
        // Still accepted: what `task_slug` actually writes.
        assert_eq!(parse_slug("3d3c9081"), Some(("3d3c9081", 1)));
        assert_eq!(parse_slug("3d3c9081-r2"), Some(("3d3c9081", 2)));
        assert_eq!(
            parse_slug("3d3c9081-r4294967295"),
            Some(("3d3c9081", u32::MAX))
        );
    }

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
