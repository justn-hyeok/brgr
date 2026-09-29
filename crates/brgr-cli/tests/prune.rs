//! `brgr prune` removes settled task worktrees and nothing else.
//!
//! A task worktree is a rebuildable checkout that costs a full copy of the
//! source working tree and leaves a branch behind in a repository brgr does not
//! own. The sealed result and its decision are the durable record. These tests
//! pin that split, and pin every refusal: what git declines, what git would
//! happily destroy but must not, and what is not a brgr worktree at all.

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn prune_reclaims_decided_worktrees_and_keeps_every_sealed_record() {
    let fixture = Fixture::new();

    let clean = fixture.run_task("clean");
    let committed = fixture.run_task("committed");
    let undecided = fixture.run_task("undecided");
    fixture.accept(&clean);
    fixture.accept(&committed);

    // An agent that committed inside its worktree is the ordinary case, and git
    // refuses to delete that branch. The checkout is still reclaimed.
    fixture.commit_in_worktree(&committed);

    let report = fixture.prune(&[]);
    assert_eq!(report["applied"], false);
    assert_eq!(report["removed"], 0);
    assert_eq!(report["removable"], 2);
    assert_eq!(report["kept"], 1);
    assert!(
        fixture.worktree(&clean).is_dir(),
        "report mode removed a worktree"
    );

    let applied = fixture.prune(&["--apply"]);
    // Report and apply must agree: `removable` in one run is removed in the next.
    assert_eq!(applied["removed"], 2);
    assert_eq!(applied["removed_keeping_branch"], 1);
    assert_eq!(applied["kept"], 1);

    assert!(!fixture.worktree(&clean).exists());
    assert!(!fixture.worktree(&committed).exists());
    assert!(fixture.worktree(&undecided).is_dir());
    assert_eq!(
        status_of(&applied, &Fixture::slug(&committed)),
        "removed_branch_kept",
        "a reclaimed checkout whose branch git kept must not report as kept"
    );

    let branches = fixture.branches();
    assert!(
        !branches.contains(&format!("brgr/task-{}", Fixture::slug(&clean))),
        "merged branch survived: {branches:?}"
    );
    assert!(
        branches.contains(&format!("brgr/task-{}", Fixture::slug(&committed))),
        "committed work lost its branch: {branches:?}"
    );
    assert!(
        branches.contains(&format!("brgr/task-{}", Fixture::slug(&undecided))),
        "undecided branch was deleted: {branches:?}"
    );
    assert!(
        branches.iter().any(|branch| branch == "user/keep-me"),
        "a branch brgr does not own was removed: {branches:?}"
    );

    for task in [&clean, &committed, &undecided] {
        let detail = fixture.json(&["result", task]);
        assert_eq!(detail["result"]["outcome"], "candidate", "lost {task}");
        assert_eq!(detail["artifacts"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn prune_refuses_ignored_files_symlinks_foreign_directories_and_its_own_cwd() {
    let fixture = Fixture::new();
    let ignored = fixture.run_task("ignored");
    // Decided only once the earlier prune runs are done, so it survives to be
    // the candidate the cwd check needs.
    let cwd = fixture.run_task("cwd");
    fixture.accept(&ignored);

    // `git status --porcelain` omits ignored paths, so `git worktree remove`
    // deletes a `.env` or a build cache without refusing. brgr must not.
    fs::write(fixture.worktree(&ignored).join(".env"), b"SECRET=1\n").unwrap();

    // A symlink would make git resolve through it and destroy a checkout outside
    // the worktrees root.
    let escape = fixture.worktrees_root().join("repo").join("aaaabbbb");
    std::os::unix::fs::symlink(&fixture.repository, &escape).unwrap();

    // Neither a foreign directory nor brgr's own admission-lock directory is a
    // task worktree.
    fs::create_dir_all(fixture.worktrees_root().join("repo").join("cafe")).unwrap();
    let locks = fixture.worktrees_root().join(".locks");
    fs::create_dir_all(&locks).unwrap();

    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["removed"], 0, "{applied}");

    assert!(
        fixture.worktree(&ignored).join(".env").is_file(),
        "an ignored file git cannot see was destroyed"
    );
    assert!(
        reason_of(&applied, ".env-holder", &Fixture::slug(&ignored)).contains("ignored path"),
        "{applied}"
    );
    assert!(
        reason_of(&applied, "symlink", "aaaabbbb").contains("symlink"),
        "{applied}"
    );
    assert!(
        reason_of(&applied, "foreign directory", "cafe").contains("not a brgr task slug"),
        "{applied}"
    );
    assert!(
        fixture.repository.join(".git").is_dir(),
        "the symlink target repository was destroyed"
    );
    assert!(
        locks.is_dir(),
        "brgr's admission lock directory was removed"
    );

    // Opting in removes the ignored worktree, and only that one.
    let opted_in = fixture.prune(&["--apply", "--include-ignored"]);
    assert_eq!(opted_in["removed"], 1, "{opted_in}");
    assert!(!fixture.worktree(&ignored).exists());

    // A prune run from inside a decided worktree must not delete it.
    fixture.accept(&cwd);
    let from_inside = fixture.prune_in(fixture.worktree(&cwd), &["--apply"]);
    assert_eq!(from_inside["removed"], 0, "{from_inside}");
    assert!(fixture.worktree(&cwd).is_dir());
    assert!(
        reason_of(&from_inside, "cwd", &Fixture::slug(&cwd)).contains("current working directory"),
        "{from_inside}"
    );
}

#[test]
fn a_locked_worktree_is_kept_in_both_modes_rather_than_promised_then_refused() {
    let fixture = Fixture::new();
    let locked = fixture.run_task("locked");
    fixture.accept(&locked);
    fixture.git(&[
        "worktree",
        "lock",
        fixture.worktree(&locked).to_str().unwrap(),
    ]);

    // `git worktree remove` refuses a locked worktree, so report mode has to see
    // the lock. It previously said `removable` and apply then said `kept`, which
    // is exactly the divergence `Removable` promises does not happen.
    let report = fixture.prune(&[]);
    assert_eq!(report["removable"], 0, "{report}");
    assert!(
        reason_of(&report, "locked", &Fixture::slug(&locked)).contains("locked"),
        "{report}"
    );

    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["removed"], 0, "{applied}");
    assert!(fixture.worktree(&locked).is_dir());
}

#[test]
fn a_branch_left_by_a_hand_removed_worktree_is_reclaimed() {
    let fixture = Fixture::new();
    let gone = fixture.run_task("gone");
    let kept = fixture.run_task("kept");
    fixture.accept(&gone);
    // The only reclamation available before `brgr prune` existed.
    fs::remove_dir_all(fixture.worktree(&gone)).unwrap();

    let branch = format!("brgr/task-{}", Fixture::slug(&gone));
    let report = fixture.prune(&[]);
    assert_eq!(report["orphan_branches_removable"], 1, "{report}");
    assert!(
        fixture.branches().contains(&branch),
        "report mode deleted a branch"
    );

    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 1, "{applied}");
    assert!(
        !fixture.branches().contains(&branch),
        "orphan branch survived: {:?}",
        fixture.branches()
    );
    // The still-present worktree keeps its branch and its registration.
    assert!(
        fixture
            .branches()
            .contains(&format!("brgr/task-{}", Fixture::slug(&kept)))
    );
    assert!(fixture.worktree(&kept).is_dir());
    // And git no longer lists the removed worktree.
    let listing = fixture.git_stdout(&["worktree", "list"]);
    assert!(
        !listing.contains(&Fixture::slug(&gone)),
        "stale registration survived: {listing}"
    );
}

#[test]
fn an_unreadable_directory_is_one_kept_row_rather_than_an_aborted_sweep() {
    let fixture = Fixture::new();
    let settled = fixture.run_task("settled");
    fixture.accept(&settled);
    let blocked = fixture.worktrees_root().join("unreadable");
    fs::create_dir_all(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

    let applied = fixture.prune(&["--apply"]);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

    // The reclaimable worktree elsewhere is still reclaimed, and the unreadable
    // directory is reported instead of aborting everything.
    assert_eq!(applied["removed"], 1, "{applied}");
    assert!(
        reason_of(&applied, "unreadable", "unreadable").contains("could not be read"),
        "{applied}"
    );
}

/// The per-repository listing is read once, not once per candidate.
#[test]
fn a_report_scales_by_repository_rather_than_by_candidate() {
    let fixture = Fixture::new();
    for index in 0..4 {
        let task = fixture.run_task(&format!("task {index}"));
        fixture.accept(&task);
    }
    let calls = fixture.prune_counting_git(&[]);
    let listings = calls
        .iter()
        .filter(|call| call.contains("worktree list"))
        .count();
    assert_eq!(
        listings, 1,
        "the repository inventory was read {listings} times for four candidates: {calls:?}"
    );
    // Apply mode reads it a second time, after `git worktree prune` may have
    // dropped a stale registration. That one is earned; a per-candidate listing is
    // not.
    let applied = fixture.prune_counting_git(&["--apply"]);
    assert!(
        applied
            .iter()
            .filter(|call| call.contains("worktree list"))
            .count()
            <= 2,
        "{applied:?}"
    );
    // One status call per candidate answers both the clean check and the ignored
    // set, so four candidates cost four.
    let statuses = calls.iter().filter(|call| call.contains("status")).count();
    assert_eq!(statuses, 4, "{calls:?}");
}

fn status_of(receipt: &Value, slug: &str) -> String {
    row(receipt, slug)["status"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[test]
fn a_stray_checkout_of_another_repository_is_never_asked_which_repository_it_is() {
    let fixture = Fixture::new();
    let settled = fixture.run_task("settled");
    fixture.accept(&settled);

    // Another repository, with a branch of the shape prune reclaims, and a
    // checkout of it dropped under our root with a name that sorts first. The
    // sweep used to ask whichever directory sorted first for the repository,
    // then deleted this branch in a repository brgr had never worked in. `+`
    // sorts before every hex digit: an earlier draft used `Backup`, which a slug
    // starting with a digit precedes, so that draft caught the defect only when
    // the random task id happened to start with a letter.
    let other = fixture.temp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    git(&other, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&other, &["branch", "brgr/task-cafed00d"]);
    let stray = fixture.worktrees_root().join("repo").join("+backup");
    git(
        &other,
        &["worktree", "add", "-q", "--detach", stray.to_str().unwrap()],
    );

    let report = fixture.prune(&[]);
    assert_eq!(report["removable"], 1, "{report}");
    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["removed"], 1, "{applied}");
    assert!(!fixture.worktree(&settled).exists());

    assert!(
        branches_of(&other).contains(&"brgr/task-cafed00d".to_owned()),
        "prune deleted a branch in an unrelated repository"
    );
    assert!(stray.is_dir(), "prune removed a checkout it did not create");
}

#[test]
fn a_worktree_brgr_did_not_create_is_kept_even_under_a_task_name() {
    let fixture = Fixture::new();
    let settled = fixture.run_task("settled");
    fixture.accept(&settled);
    // The user's own worktree of the same repository, registered with git and
    // carrying a settled task's name, but not the path brgr created for it. Name,
    // registration, and decision all check out; only the path the store recorded
    // tells the two apart.
    let theirs = fixture
        .worktrees_root()
        .join("elsewhere")
        .join(Fixture::slug(&settled));
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        theirs.to_str().unwrap(),
    ]);

    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["removed"], 1, "{applied}");
    assert!(!fixture.worktree(&settled).exists());
    assert!(
        theirs.is_dir(),
        "prune removed a worktree it did not create"
    );
    // Kept for its own reason, not reported as a repository nobody can locate.
    assert!(
        !applied.to_string().contains("cannot be located"),
        "{applied}"
    );
}

#[test]
fn branches_are_reclaimed_when_every_worktree_was_removed_by_hand() {
    let fixture = Fixture::new();
    let first = fixture.run_task("first");
    let second = fixture.run_task("second");
    fixture.accept(&first);
    fixture.accept(&second);
    // No checkout survives to ask which repository these came from. That is
    // the case orphan reclamation exists for, and it reported zero.
    fs::remove_dir_all(fixture.worktree(&first)).unwrap();
    fs::remove_dir_all(fixture.worktree(&second)).unwrap();

    let report = fixture.prune(&[]);
    assert_eq!(report["orphan_branches_removable"], 2, "{report}");
    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 2, "{applied}");
    assert!(
        !fixture
            .branches()
            .iter()
            .any(|branch| branch.starts_with("brgr/task-")),
        "{:?}",
        fixture.branches()
    );
    assert_eq!(
        fixture
            .git_stdout(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1,
        "stale registrations survived"
    );
}

#[test]
fn reclaiming_an_orphan_leaves_the_users_own_missing_worktree_registered() {
    let fixture = Fixture::new();
    // A surviving worktree, so this test isolates its own defect: without one,
    // the version before this fix never found the orphan at all.
    let _anchor = fixture.run_task("anchor");
    // A user's detached worktree on a volume that is not mounted right now.
    // `git worktree prune` would drop its registration, and with a detached HEAD
    // nothing else references its commits.
    let volume = fixture.temp.path().join("unmounted").join("scratch");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        volume.to_str().unwrap(),
    ]);
    fs::remove_dir_all(fixture.temp.path().join("unmounted")).unwrap();

    let gone = fixture.run_task("gone");
    fixture.accept(&gone);
    fs::remove_dir_all(fixture.worktree(&gone)).unwrap();

    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 1, "{applied}");
    let listing = fixture.git_stdout(&["worktree", "list", "--porcelain"]);
    assert!(
        listing.contains("unmounted/scratch"),
        "prune dropped a registration it did not create:\n{listing}"
    );
    assert!(
        !listing.contains(&Fixture::slug(&gone)),
        "the task's own stale registration survived:\n{listing}"
    );
}

#[test]
fn an_orphan_with_unmerged_commits_is_kept_in_report_mode_too() {
    let fixture = Fixture::new();
    // A surviving worktree, so this test isolates its own defect: without one,
    // the version before this fix never found the orphan at all.
    let _anchor = fixture.run_task("anchor");
    let committed = fixture.run_task("committed");
    fixture.accept(&committed);
    fixture.commit_in_worktree(&committed);
    fs::remove_dir_all(fixture.worktree(&committed)).unwrap();

    // `git branch -d` refuses this branch. Report mode said `removable` and apply
    // then said `kept` — the divergence `Removable` promises cannot happen.
    let report = fixture.prune(&[]);
    assert_eq!(report["orphan_branches_removable"], 0, "{report}");
    assert!(orphan_reason(&report).contains("not merged"), "{report}");
    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 0, "{applied}");
    assert!(
        fixture
            .branches()
            .contains(&format!("brgr/task-{}", Fixture::slug(&committed)))
    );
}

#[test]
fn an_orphan_of_an_undecided_revision_is_kept_and_names_its_owner() {
    let fixture = Fixture::new();
    // A surviving worktree, so this test isolates its own defect: without one,
    // the version before this fix never found the orphan at all.
    let _anchor = fixture.run_task("anchor");
    let undecided = fixture.run_task("undecided");
    fs::remove_dir_all(fixture.worktree(&undecided)).unwrap();

    // The worktree pass requires a decision; the orphan pass required nothing.
    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 0, "{applied}");
    assert!(orphan_reason(&applied).contains("decision"), "{applied}");
    assert_eq!(
        applied["orphan_branches"][0]["owner_id"], "codex:prune-owner",
        "{applied}"
    );
    assert!(
        fixture
            .branches()
            .contains(&format!("brgr/task-{}", Fixture::slug(&undecided)))
    );
}

#[test]
fn a_task_admitted_before_checkouts_were_recorded_is_located_through_git() {
    let fixture = Fixture::new();
    let gone = fixture.run_task("gone");
    let kept = fixture.run_task("kept");
    fixture.accept(&gone);
    fixture.forget_checkouts();
    fs::remove_dir_all(fixture.worktree(&gone)).unwrap();

    // With no record, the surviving worktree names the repository, and git's
    // stale registration ties the orphan to it.
    let applied = fixture.prune(&["--apply"]);
    assert_eq!(applied["orphan_branches_removed"], 1, "{applied}");
    assert!(fixture.worktree(&kept).is_dir());
}

#[test]
fn a_repository_nobody_can_locate_is_reported_rather_than_counted_as_clean() {
    let fixture = Fixture::new();
    let gone = fixture.run_task("gone");
    fixture.accept(&gone);
    fixture.forget_checkouts();
    fs::remove_dir_all(fixture.worktree(&gone)).unwrap();

    let report = fixture.prune(&[]);
    assert_eq!(report["orphan_branches_removable"], 0, "{report}");
    let reason = reason_of(&report, "unlocated repository", "repo");
    assert!(reason.contains("cannot be located"), "{report}");
}

fn orphan_reason(receipt: &Value) -> String {
    receipt["orphan_branches"][0]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("no orphan reason in {receipt}"))
        .to_owned()
}

fn branches_of(repository: &Path) -> Vec<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect()
}

fn reason_of(receipt: &Value, label: &str, slug: &str) -> String {
    let row = row(receipt, slug);
    row["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("{label} ({slug}) has no reason in {receipt}"))
        .to_owned()
}

fn row<'a>(receipt: &'a Value, slug: &str) -> &'a Value {
    receipt["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["task_slug"] == slug)
        .unwrap_or_else(|| panic!("no row for {slug} in {receipt}"))
}

struct Fixture {
    /// Held so the tree outlives the fixture, and used for scratch files.
    temp: TempDir,
    home: PathBuf,
    repository: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let scratch = temp.path().join("scratch");
        let repository = temp.path().join("repo");
        fs::create_dir_all(&scratch).unwrap();
        fs::create_dir_all(&repository).unwrap();
        let fixture = Self {
            temp,
            home,
            repository,
        };
        fixture.seed_repository();
        fixture.register_harness(&scratch);
        fixture
    }

    fn worktrees_root(&self) -> PathBuf {
        self.home.join("worktrees")
    }

    fn worktree(&self, task: &str) -> PathBuf {
        self.worktrees_root().join("repo").join(Fixture::slug(task))
    }

    fn slug(task: &str) -> String {
        task[..8].to_owned()
    }

    fn run_task(&self, objective: &str) -> String {
        let receipt = self.json(&[
            "run",
            objective,
            "--workspace",
            self.repo_arg(),
            "--foreground",
        ]);
        assert_eq!(receipt["outcome"], "candidate");
        receipt["task_id"].as_str().unwrap().to_owned()
    }

    fn accept(&self, task: &str) {
        let decision = self.json(&["accept", task, "--reason", "fixture result verified"]);
        assert_eq!(decision["verdict"], "accepted");
    }

    fn commit_in_worktree(&self, task: &str) {
        let worktree = self.worktree(task);
        fs::write(worktree.join("AGENT_WORK"), b"produced by the run\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-m", "agent work"]);
    }

    fn prune(&self, args: &[&str]) -> Value {
        self.prune_in(self.home.clone(), args)
    }

    fn prune_in(&self, directory: PathBuf, args: &[&str]) -> Value {
        let mut command = self.command();
        command.current_dir(directory).arg("prune").args(args);
        json(&command.output().unwrap())
    }

    fn json(&self, args: &[&str]) -> Value {
        json(&self.command().args(args).output().unwrap())
    }

    fn repo_arg(&self) -> &str {
        self.repository.to_str().unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
        command
            .arg("--home")
            .arg(&self.home)
            .arg("--json")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("BRGR_HOME")
            .env("BRGR_OWNER_ID", "codex:prune-owner")
            .env("BRGR_SESSION_ID", "prune-session");
        command
    }

    fn git(&self, args: &[&str]) {
        git(&self.repository, args);
    }

    fn git_stdout(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repository)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs a prune with a `git` shim first on `PATH` and returns what it invoked.
    fn prune_counting_git(&self, args: &[&str]) -> Vec<String> {
        let shim = self.temp.path().join("shim");
        fs::create_dir_all(&shim).unwrap();
        let log = self.temp.path().join("git-calls.log");
        // Truncated per run, so a later sweep does not inherit an earlier tally.
        let _ = fs::remove_file(&log);
        fs::write(
            shim.join("git"),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec /usr/bin/git \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            shim.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = self
            .command()
            .env("PATH", path)
            .arg("prune")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "prune failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// Puts the store back in the state every task admitted before checkouts
    /// were recorded is in.
    fn forget_checkouts(&self) {
        let store =
            rusqlite::Connection::open(self.home.join("store").join("brgr.sqlite3")).unwrap();
        store.execute("DELETE FROM task_checkouts", []).unwrap();
    }

    fn branches(&self) -> Vec<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repository)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn register_harness(&self, scratch: &Path) {
        let output = self
            .command()
            .arg("harness")
            .arg("add")
            .arg(harness_fixture())
            .arg("--workspace")
            .arg(scratch)
            .args(["--prompt", "BRGR_FIXTURE_OK"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture registration failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn seed_repository(&self) {
        let repo = &self.repository;
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.name", "Fixture"]);
        git(repo, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(repo.join("README"), b"seed\n").unwrap();
        // Committed so every task worktree inherits it and `.env` is ignored.
        fs::write(repo.join(".gitignore"), b".env\n").unwrap();
        git(repo, &["add", "README", ".gitignore"]);
        git(repo, &["commit", "-m", "seed"]);
        // A branch brgr does not own, to prove pruning leaves it alone.
        git(repo, &["branch", "user/keep-me"]);
    }
}

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn harness_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}
