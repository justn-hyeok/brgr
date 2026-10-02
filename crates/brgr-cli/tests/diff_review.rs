//! The owner reviews a worker's sealed diff and integrates it.
//!
//! `brgr diff` shows exactly the bytes `brgr apply` would write, so review and
//! integration cannot disagree. `brgr apply` takes the target at the task's
//! base commit or at any commit that descends from it, because an owner keeps
//! working while a worker runs; a patch that no longer applies is still refused
//! by `git apply --check` before anything is written.

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

/// A gjc-shaped worker that edits `README` and adds a line to `notes.txt`.
const EDITING_WORKER: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'gjc v-diff-review-fixture'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
for item in "$@"; do case "$item" in @*) prompt=${item#@};; esac; done
if /usr/bin/grep -q EDIT_TASK "$prompt"; then
  printf 'worker edit\n' > README
  printf 'one\ntwo\nworker line\n' > notes.txt
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"DIFF_REVIEW_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

#[test]
fn diff_prints_the_sealed_patch_and_its_file_summary() {
    let fixture = Fixture::new();
    let task = fixture.run_editing_task(true);

    let patch = fixture.text(&["diff", &task]);
    assert!(patch.starts_with("diff --git a/"), "patch: {patch}");
    assert!(patch.contains("+worker edit"));
    assert!(patch.contains("+worker line"));

    let stat = fixture.text(&["diff", &task, "--stat"]);
    assert!(stat.contains("README"), "stat: {stat}");
    assert!(stat.contains("notes.txt"));
    assert!(stat.contains("2 files, +2 -1"), "stat: {stat}");

    let summary = fixture.json(&["diff", &task, "--stat"]);
    let files = summary["files"].as_array().unwrap();
    let readme = files.iter().find(|file| file["path"] == "README").unwrap();
    assert_eq!(readme["added"], 1);
    assert_eq!(readme["deleted"], 1);
    assert!(summary.get("patch").is_none());

    let full = fixture.json(&["diff", &task]);
    assert_eq!(full["patch"], patch);
}

#[test]
fn diff_names_the_missing_flag_when_no_patch_was_requested() {
    let fixture = Fixture::new();
    let task = fixture.run_editing_task(false);
    let output = fixture.command().args(["diff", &task]).output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--capture-diff"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn empty_sealed_diff_can_be_reviewed_and_applied_as_a_noop() {
    let fixture = Fixture::new();
    let result = fixture.json(&[
        "run",
        "READ_TASK",
        "--workspace",
        fixture.repo(),
        "--foreground",
        "--capture-diff",
    ]);
    let task = result["task_id"].as_str().unwrap();
    assert_eq!(fixture.text(&["diff", task]), "");
    let summary = fixture.json(&["diff", task, "--stat"]);
    assert!(summary["files"].as_array().unwrap().is_empty());
    fixture.accept(task);
    let head = fixture.head();
    let applied = fixture.json(&["apply", task, "--workspace", fixture.repo(), "--execute"]);
    assert_eq!(applied["status"], "applied");
    assert_eq!(fixture.head(), head);
    assert_eq!(
        fs::read_to_string(fixture.repository.join("README")).unwrap(),
        "seed\n"
    );
}

#[test]
fn apply_follows_the_owner_onto_a_later_commit() {
    let fixture = Fixture::new();
    let task = fixture.run_editing_task(true);
    fixture.accept(&task);
    let base = fixture.head();
    // The owner keeps committing while the worker runs.
    fs::write(fixture.repository.join("owner.txt"), "owner work\n").unwrap();
    git(&fixture.repository, &["add", "owner.txt"]);
    git(&fixture.repository, &["commit", "-qam", "owner work"]);
    let moved = fixture.head();

    let applied = fixture.json(&["apply", &task, "--workspace", fixture.repo(), "--execute"]);

    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["base_commit"], base);
    assert_eq!(applied["target_head"], moved);
    assert_eq!(
        fs::read_to_string(fixture.repository.join("notes.txt")).unwrap(),
        "one\ntwo\nworker line\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repository.join("owner.txt")).unwrap(),
        "owner work\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repository.join("README")).unwrap(),
        "worker edit\n"
    );
}

#[test]
fn apply_refuses_a_target_that_left_the_base_history() {
    let fixture = Fixture::new();
    let task = fixture.run_editing_task(true);
    fixture.accept(&task);
    // A new root commit shares no history with the task base, even though the
    // patch would apply to its files.
    git(
        &fixture.repository,
        &["checkout", "-q", "--orphan", "elsewhere"],
    );
    git(&fixture.repository, &["commit", "-qm", "unrelated root"]);

    let output = fixture
        .command()
        .args(["apply", &task, "--workspace", fixture.repo(), "--execute"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("descends from it"));
    assert_eq!(
        fs::read_to_string(fixture.repository.join("README")).unwrap(),
        "seed\n"
    );
}

#[test]
fn apply_refuses_a_later_commit_the_patch_conflicts_with() {
    let fixture = Fixture::new();
    let task = fixture.run_editing_task(true);
    fixture.accept(&task);
    fs::write(fixture.repository.join("README"), "owner rewrite\n").unwrap();
    git(&fixture.repository, &["commit", "-qam", "owner rewrite"]);

    let output = fixture
        .command()
        .args(["apply", &task, "--workspace", fixture.repo(), "--execute"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflict"));
    assert_eq!(
        fs::read_to_string(fixture.repository.join("README")).unwrap(),
        "owner rewrite\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repository.join("notes.txt")).unwrap(),
        "one\ntwo\n"
    );
}

struct Fixture {
    /// Held so the tree outlives the fixture.
    _temp: TempDir,
    home: PathBuf,
    repository: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let repository = temp.path().join("repo");
        let scratch = temp.path().join("scratch");
        let worker = temp.path().join("gjc");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(&worker, EDITING_WORKER).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(repository.join("README"), "seed\n").unwrap();
        fs::write(repository.join("notes.txt"), "one\ntwo\n").unwrap();
        git(&repository, &["add", "README", "notes.txt"]);
        git(&repository, &["commit", "-qm", "seed"]);
        let fixture = Self {
            _temp: temp,
            home,
            repository,
        };
        fixture.json(&[
            "harness",
            "add",
            worker.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ]);
        fixture
    }

    fn run_editing_task(&self, capture_diff: bool) -> String {
        let mut args = vec![
            "run",
            "EDIT_TASK",
            "--workspace",
            self.repo(),
            "--foreground",
        ];
        if capture_diff {
            args.push("--capture-diff");
        }
        let receipt = self.json(&args);
        assert_eq!(receipt["outcome"], "candidate");
        receipt["task_id"].as_str().unwrap().to_owned()
    }

    fn accept(&self, task: &str) {
        self.json(&["accept", task]);
    }

    fn head(&self) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn repo(&self) -> &str {
        self.repository.to_str().unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
        command
            .arg("--headless")
            .arg("--home")
            .arg(&self.home)
            .env_remove("CODEX_THREAD_ID")
            .env_remove("BRGR_HOME")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env("BRGR_OWNER_ID", "codex:diff-review")
            .env("BRGR_SESSION_ID", "diff-review-session");
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().arg("--json").args(args).output().unwrap();
        serde_json::from_slice(&succeeded(&output).stdout).unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().unwrap();
        String::from_utf8(succeeded(&output).stdout.clone()).unwrap()
    }
}

fn succeeded(output: &Output) -> &Output {
    assert!(
        output.status.success(),
        "brgr failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
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
