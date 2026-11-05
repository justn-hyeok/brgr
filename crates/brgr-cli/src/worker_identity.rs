//! Which worker a brgr command was called from.
//!
//! A worker normally carries `BRGR_PARENT_TASK_ID` and `BRGR_PARENT_ATTEMPT_ID`.
//! A Codex worker does not: its tool commands run in Codex's shared daemon, which
//! keeps the environment of whoever started it, so the variables are absent or
//! name another worker. Its working directory is still its own, so a task
//! directory inside a brgr worktree wins over the environment. The same daemon
//! loses a custom `BRGR_HOME`, so the worktree's own path also names the home.

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use brgr_protocol::{AttemptId, TaskId};
use brgr_store::Store;

struct Worker {
    task: TaskId,
    attempt: AttemptId,
    pane: Option<String>,
}

static FROM_DIRECTORY: OnceLock<Option<Worker>> = OnceLock::new();

pub(crate) fn initialize(paths: &crate::Paths) {
    let found = env::current_dir()
        .ok()
        .and_then(|directory| directory.canonicalize().ok())
        .and_then(|directory| task_in_directory(&directory, &paths.worktrees))
        .and_then(|task| {
            let store = Store::open(&paths.store).ok()?;
            let attempt = store.active_message_attempt(task).ok()?;
            let revision = store.task(task).ok()?.revision;
            let pane = fs::read(paths.pane_receipt(task, revision))
                .ok()
                .and_then(|bytes| pane_in_receipt(&bytes));
            Some(Worker {
                task,
                attempt,
                pane,
            })
        });
    let _ = FROM_DIRECTORY.set(found);
}

pub(crate) fn from_directory() -> Option<(TaskId, AttemptId)> {
    FROM_DIRECTORY
        .get()
        .and_then(Option::as_ref)
        .map(|worker| (worker.task, worker.attempt))
}

/// The pane this worker runs in, which its daemon's `HERDR_PANE_ID` cannot say.
pub(crate) fn pane() -> Option<String> {
    FROM_DIRECTORY.get()?.as_ref()?.pane.clone()
}

fn pane_in_receipt(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("pane")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn task() -> Option<String> {
    from_directory()
        .map(|(task, _)| task.to_string())
        .or_else(|| env::var("BRGR_PARENT_TASK_ID").ok())
}

pub(crate) fn attempt() -> Option<String> {
    from_directory()
        .map(|(_, attempt)| attempt.to_string())
        .or_else(|| env::var("BRGR_PARENT_ATTEMPT_ID").ok())
}

/// The home to use when neither `--home` nor `BRGR_HOME` chose one, which is how
/// a Codex worker's command arrives. A choice someone made stands, wherever the
/// command runs.
pub(crate) fn resolve_home(chosen: Option<PathBuf>) -> Option<PathBuf> {
    chosen.or_else(|| {
        env::current_dir()
            .ok()
            .and_then(|directory| directory.canonicalize().ok())
            .and_then(|directory| home_in_directory(&directory))
    })
}

/// The home whose worktree holds `directory`: a directory with `.brgr/tasks` at
/// `HOME/worktrees/REPO/TASK`, in a home that has a store. Anywhere else there is
/// none, so a head's own checkout is never moved to another home.
fn home_in_directory(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join(".brgr/tasks").is_dir())
        .and_then(|worktree| {
            let worktrees = worktree.parent()?.parent()?;
            let home = worktrees.parent()?;
            (worktrees.file_name()? == "worktrees" && home.join("store").is_dir())
                .then(|| home.to_owned())
        })
}

/// The task whose `.brgr/tasks/TASK-rN` is in `directory` or a parent of it, up
/// to `worktrees`. Outside `worktrees` there is none, so a head in its own
/// checkout is never taken for a worker. The newest revision wins.
fn task_in_directory(directory: &Path, worktrees: &Path) -> Option<TaskId> {
    let worktrees = worktrees.canonicalize().ok()?;
    if !directory.starts_with(&worktrees) {
        return None;
    }
    directory
        .ancestors()
        .take_while(|ancestor| *ancestor != worktrees)
        .find_map(|ancestor| {
            fs::read_dir(ancestor.join(".brgr/tasks"))
                .ok()?
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let (task, revision) = name.rsplit_once("-r")?;
                    Some((revision.parse::<u32>().ok()?, task.parse::<TaskId>().ok()?))
                })
                .max_by_key(|(revision, _)| *revision)
                .map(|(_, task)| task)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "2b2c279a-b8e1-43a8-846c-cb8344a301d7";
    const NEW: &str = "3b2c279a-b8e1-43a8-846c-cb8344a301d7";

    #[test]
    fn a_worktree_names_its_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let worktree = home.join("worktrees/repo/abc");
        fs::create_dir_all(home.join("store")).unwrap();
        fs::create_dir_all(worktree.join(".brgr/tasks")).unwrap();
        fs::create_dir_all(worktree.join("src")).unwrap();
        assert_eq!(home_in_directory(&worktree.join("src")), Some(home));
    }

    #[test]
    fn a_checkout_with_a_task_directory_names_no_home() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("store")).unwrap();
        fs::create_dir_all(root.join("dev/repo/.brgr/tasks")).unwrap();
        assert_eq!(home_in_directory(&root.join("dev/repo")), None);
    }

    #[test]
    fn the_newest_task_directory_in_a_worktree_names_the_worker() {
        let root = tempfile::tempdir().unwrap();
        let worktrees = root.path().canonicalize().unwrap().join("worktrees");
        let worktree = worktrees.join("repo/abc");
        fs::create_dir_all(worktree.join(format!(".brgr/tasks/{OLD}-r1"))).unwrap();
        fs::create_dir_all(worktree.join(format!(".brgr/tasks/{NEW}-r2"))).unwrap();
        fs::create_dir_all(worktree.join("src/deep")).unwrap();
        assert_eq!(
            task_in_directory(&worktree.join("src/deep"), &worktrees),
            Some(NEW.parse().unwrap())
        );
    }

    #[test]
    fn a_task_directory_outside_the_worktrees_names_no_worker() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let worktrees = root.join("worktrees");
        let checkout = root.join("checkout");
        fs::create_dir_all(&worktrees).unwrap();
        fs::create_dir_all(checkout.join(format!(".brgr/tasks/{OLD}-r1"))).unwrap();
        assert_eq!(task_in_directory(&checkout, &worktrees), None);
    }

    #[test]
    fn a_pane_receipt_names_the_workers_pane() {
        assert_eq!(
            pane_in_receipt(br#"{"pane":"w1:p2","cleanup":"open"}"#).as_deref(),
            Some("w1:p2")
        );
        assert_eq!(pane_in_receipt(b"{}"), None);
        assert_eq!(pane_in_receipt(b"not json"), None);
    }

    #[test]
    fn a_worktree_without_a_task_directory_names_no_worker() {
        let root = tempfile::tempdir().unwrap();
        let worktrees = root.path().canonicalize().unwrap().join("worktrees");
        fs::create_dir_all(worktrees.join("repo/abc")).unwrap();
        assert_eq!(
            task_in_directory(&worktrees.join("repo/abc"), &worktrees),
            None
        );
    }
}
