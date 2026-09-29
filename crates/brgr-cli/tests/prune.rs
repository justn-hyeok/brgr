//! `brgr prune` removes settled task worktrees and nothing else.
//!
//! A task worktree is a rebuildable checkout that costs a full copy of the
//! source working tree and leaves a branch behind in a repository brgr does not
//! own. The sealed result and its decision are the durable record. These tests
//! pin that split, and pin every refusal: what git declines, what git would
//! happily destroy but must not, and what is not a brgr worktree at all.

use std::{
    fs,
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

fn status_of(receipt: &Value, slug: &str) -> String {
    row(receipt, slug)["status"]
        .as_str()
        .unwrap_or("")
        .to_owned()
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
    _temp: TempDir,
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
            _temp: temp,
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
