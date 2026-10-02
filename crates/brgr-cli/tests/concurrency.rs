//! Concurrent admission must not surface a raw store error to the caller.
//!
//! The workspace is a real Git repository, which is what makes this a gate:
//! a non-Git workspace short-circuits `acquire_admission_lock` and
//! `prepare_workspace`, so a sweep over plain directories never takes the
//! admission lock, never creates a worktree, and cannot observe the store
//! contention this is about. `cargo bench -p brgr-cli --bench
//! concurrent_admission` measures the same shape and reports how much of the
//! ideal speedup survives.
//!
//! Child stderr goes to files rather than pipes: eight children awaited in turn
//! would deadlock on a full pipe buffer if any of them produced a large
//! backtrace.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::Value;
use tempfile::TempDir;

/// Runs that all finish inside the same instant, which is what a caller gets
/// after dispatching a batch of tasks to one harness.
const WIDTH: usize = 8;

#[test]
fn concurrent_admissions_all_reach_a_sealed_result() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let scratch = temp.path().join("scratch");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&scratch).unwrap();
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    register_bench_fixture(&home, &scratch);

    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let children: Vec<_> = (0..WIDTH)
        .map(|index| {
            let log = logs.join(format!("{index}.stderr"));
            let child = brgr(&home)
                .args(["--json", "run", "SLOW concurrency gate", "--workspace"])
                .arg(&repository)
                .arg("--foreground")
                .stdout(Stdio::piped())
                .stderr(Stdio::from(File::create(&log).unwrap()))
                .spawn()
                .unwrap();
            (child, log)
        })
        .collect();

    let mut failures = Vec::new();
    let mut task_ids = BTreeSet::new();
    let mut result_ids = BTreeSet::new();
    for (child, log) in children {
        let output = child.wait_with_output().unwrap();
        if output.status.success() {
            let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(receipt["outcome"], "candidate");
            task_ids.insert(receipt["task_id"].as_str().unwrap().to_owned());
            result_ids.insert(receipt["result_id"].as_str().unwrap().to_owned());
        } else {
            failures.push(
                fs::read_to_string(&log)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            );
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {WIDTH} concurrent admissions failed:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
    assert_eq!(task_ids.len(), WIDTH, "task identities collided");
    assert_eq!(result_ids.len(), WIDTH, "result identities collided");

    // Proof that this sweep actually went through admission: one worktree per
    // task, and the cross-process admission lock directory exists.
    let worktrees = home.join("worktrees");
    assert!(
        worktrees.join(".locks").is_dir(),
        "the admission lock was never taken, so this did not exercise admission"
    );
    assert_eq!(
        fs::read_dir(worktrees.join("repo")).unwrap().count(),
        WIDTH,
        "a worktree per task was not created"
    );
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n",
        "the source workspace was modified"
    );
}

fn register_bench_fixture(home: &Path, scratch: &Path) {
    let output = brgr(home)
        .arg("harness")
        .arg("add")
        .arg(bench_fixture())
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

fn brgr(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
    command.arg("--headless");
    command
        .arg("--home")
        .arg(home)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_HOME")
        .env("BRGR_OWNER_ID", "codex:concurrency-owner")
        .env("BRGR_SESSION_ID", "concurrency-session");
    command
}

fn bench_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/bench/gjc")
        .canonicalize()
        .unwrap()
}

fn seed_git_repo(repository: &Path) {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    fs::write(repository.join("README"), b"seed\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-m", "seed"]);
}
