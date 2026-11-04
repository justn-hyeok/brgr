//! Cross-process settling checks for one shared control home.
mod common;

use common::{HANG_GUARD, brgr_command, isolate_test_command, wait_until};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Instant,
};
use tempfile::TempDir;

const ROUNDS: usize = 10;
const WIDTH: usize = 4;

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    scratch: PathBuf,
    logs: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let scratch = temp.path().join("scratch");
        let logs = temp.path().join("logs");
        fs::create_dir_all(&scratch).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let fixture_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fixtures/brgr-fixture")
            .canonicalize()
            .unwrap();
        let fixture = temp.path().join("fixture");
        let fixture_text = fs::read_to_string(&fixture_source).unwrap();
        let marker = "# A worker that leaves a file of its own, or only its harness cache.";
        let wait = r#"if [ -n "$prompt_file" ] && /usr/bin/grep -q 'CONCURRENCY_RELEASE' "$prompt_file"; then
  : > "$BRGR_HOME/concurrency-worker-ready"
  while [ ! -f "$BRGR_HOME/concurrency-release" ]; do /bin/sleep 0.01; done
fi
"#;
        fs::write(
            &fixture,
            fixture_text.replace(marker, &format!("{wait}{marker}")),
        )
        .unwrap();
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o755)).unwrap();
        let added = brgr(&home)
            .args(["harness", "add"])
            .arg(fixture)
            .arg("--workspace")
            .arg(&scratch)
            .args(["--prompt", "BRGR_FIXTURE_OK"])
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "fixture registration: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        Self {
            _temp: temp,
            home,
            scratch,
            logs,
        }
    }

    fn start_slow(&self, round: usize) -> String {
        let output = brgr(&self.home)
            .args([
                "--json",
                "run",
                "SLOW concurrency settle",
                "--harness",
                "local.fixture",
                "--workspace",
            ])
            .arg(&self.scratch)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        let task = receipt["task_id"]
            .as_str()
            .expect("run receipt task_id")
            .to_owned();
        assert!(
            wait_until(|| self.home.join("runs").join(format!("{task}.pid")).exists()),
            "supervisor pid not written in round {round}"
        );
        task
    }

    fn start_marker_worker(&self, prompt: &str, _round: usize) -> String {
        let output = brgr(&self.home)
            .args([
                "--json",
                "run",
                prompt,
                "--harness",
                "local.fixture",
                "--workspace",
            ])
            .arg(&self.scratch)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        receipt["task_id"].as_str().expect("task_id").to_owned()
    }
}

fn brgr(home: &Path) -> Command {
    let mut command = brgr_command();
    command
        .args(["--headless", "--home"])
        .arg(home)
        .env("BRGR_OWNER_ID", "codex:concurrency-settle")
        .env("BRGR_SESSION_ID", "concurrency-settle-session");
    command
}

/// Start N brgr clients behind a filesystem release barrier. Stderr is logged
/// to files so a failing child cannot block on a full pipe.
fn compete(f: &Fixture, round: usize, args: &[&str]) -> Vec<std::process::Output> {
    compete_at(f, round, args, f.logs.join(format!("release-{round}")))
}

fn compete_at(
    f: &Fixture,
    round: usize,
    args: &[&str],
    release: PathBuf,
) -> Vec<std::process::Output> {
    let variants: Vec<Vec<String>> = (0..WIDTH)
        .map(|_| args.iter().map(|arg| (*arg).to_owned()).collect())
        .collect();
    compete_variants_at(f, round, &variants, release)
}

fn compete_variants_at(
    f: &Fixture,
    round: usize,
    variants: &[Vec<String>],
    release: PathBuf,
) -> Vec<std::process::Output> {
    let mut children: Vec<(Child, PathBuf)> = (0..WIDTH)
        .map(|index| {
            let log = f.logs.join(format!("{round}-{index}.stderr"));
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg("while [ ! -f \"$1\" ]; do /bin/sleep 0.01; done; shift; exec \"$@\"")
                .arg("barrier")
                .arg(&release)
                .arg(common::brgr_binary())
                .arg("--json")
                .arg("--headless")
                .arg("--home")
                .arg(&f.home)
                .args(&variants[index])
                .stdout(Stdio::piped())
                .stderr(Stdio::from(File::create(&log).unwrap()));
            isolate_test_command(&mut command);
            command
                .env("BRGR_OWNER_ID", "codex:concurrency-settle")
                .env("BRGR_SESSION_ID", "concurrency-settle-session");
            (command.spawn().unwrap(), log)
        })
        .collect();
    assert!(
        wait_until(|| children
            .iter_mut()
            .all(|(child, _)| child.try_wait().unwrap().is_none())),
        "barrier clients exited before release"
    );
    fs::write(release, b"go").unwrap();
    let deadline = Instant::now() + HANG_GUARD;
    children
        .drain(..)
        .map(|(child, log)| {
            assert!(Instant::now() < deadline, "child exceeded hang guard");
            let output = child.wait_with_output().unwrap();
            let stderr = fs::read_to_string(&log).unwrap_or_default();
            assert!(!raw_store_error(&stderr), "raw store error: {stderr}");
            assert!(
                !raw_store_error(&String::from_utf8_lossy(&output.stdout)),
                "raw store error on stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            output
        })
        .collect()
}

/// A cancel client either asks for the cancel, or finds the run already over:
/// another client's cancel, or the worker, can settle it first, and `brgr
/// cancel` refuses a terminal task. Anything else is a failure.
fn assert_cancel_clients(f: &Fixture, round: usize, outputs: &[std::process::Output]) {
    for (index, output) in outputs.iter().enumerate() {
        let stderr =
            fs::read_to_string(f.logs.join(format!("{round}-{index}.stderr"))).unwrap_or_default();
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(
                matches!(
                    value["state"].as_str(),
                    Some("cancel_requested" | "force_stopped")
                ),
                "unexpected cancel response in round {round}: {value}"
            );
        } else {
            assert!(
                stderr.contains("is already terminal"),
                "cancel client {index} failed in round {round}: {stderr}"
            );
        }
    }
}

fn raw_store_error(text: &str) -> bool {
    [
        "database is locked",
        "UNIQUE constraint",
        "panicked at",
        "thread 'main' panicked",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn raw_cleanup_error(text: &str) -> bool {
    [
        "brgr cleanup pending:",
        "pane cleanup queue could not be updated:",
        "pane cleanup remains pending:",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[test]
fn concurrent_cancel_clients_settle_one_running_attempt() {
    for round in 0..ROUNDS {
        let f = Fixture::new();
        let task = f.start_slow(round);
        let results = compete(&f, round, &["cancel", &task]);
        assert_cancel_clients(&f, round, &results);
        assert!(
            wait_until(|| brgr(&f.home)
                .args(["--json", "result", &task])
                .output()
                .is_ok_and(|o| o.status.success())),
            "cancel did not settle in round {round}"
        );
        let output = brgr(&f.home)
            .args(["--json", "result", &task])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["result"]["outcome"], "cancelled",
            "round {round}: {value}"
        );
        let store = brgr_store::Store::open(f.home.join("store")).unwrap();
        let id = task.parse().unwrap();
        assert_eq!(
            store
                .inbox(
                    &brgr_protocol::OwnerId::new("codex:concurrency-settle").unwrap(),
                    true
                )
                .unwrap()
                .len(),
            1,
            "round {round}"
        );
        assert_eq!(
            store.latest_result(id).unwrap().outcome,
            brgr_protocol::TerminalOutcome::Cancelled
        );
    }
}

#[test]
fn cancel_races_worker_completion_on_shared_release() {
    let mut cancelled = 0;
    let mut completed = 0;
    for round in 0..ROUNDS {
        let f = Fixture::new();
        let release = f.home.join("concurrency-release");
        let task = f.start_marker_worker("CONCURRENCY_RELEASE", round);
        assert!(
            wait_until(|| f.home.join("concurrency-worker-ready").exists()),
            "worker did not reach release in round {round}"
        );
        let outputs = compete_at(&f, round, &["cancel", &task], release.clone());
        assert_cancel_clients(&f, round, &outputs);
        fs::write(&release, b"go").unwrap();
        assert!(
            wait_until(|| brgr(&f.home)
                .args(["--json", "result", &task])
                .output()
                .is_ok_and(|o| o.status.success())),
            "race did not settle in round {round}"
        );
        let store = brgr_store::Store::open(f.home.join("store")).unwrap();
        let id = task.parse().unwrap();
        match store.latest_result(id).unwrap().outcome {
            brgr_protocol::TerminalOutcome::Cancelled => cancelled += 1,
            brgr_protocol::TerminalOutcome::Candidate => completed += 1,
            outcome => panic!("unexpected race outcome {outcome:?} in round {round}"),
        }
        assert_eq!(
            store
                .inbox(
                    &brgr_protocol::OwnerId::new("codex:concurrency-settle").unwrap(),
                    true
                )
                .unwrap()
                .len(),
            1,
            "round {round}"
        );
    }
    eprintln!("cancel race split: cancelled={cancelled}, candidate={completed}");
}

#[test]
fn competing_accept_accept_reject_records_one_decision() {
    let mut accepted = 0;
    let mut rejected = 0;
    for round in 0..ROUNDS {
        let f = Fixture::new();
        let task = f.start_marker_worker("BRGR_FIXTURE_OK", round);
        assert!(
            wait_until(|| brgr(&f.home)
                .args(["--json", "result", &task])
                .output()
                .is_ok_and(|o| o.status.success())),
            "candidate not ready in round {round}"
        );
        let variants = vec![
            vec![
                "accept".into(),
                task.clone(),
                "--reason".into(),
                "concurrent decision".into(),
            ],
            vec![
                "accept".into(),
                task.clone(),
                "--reason".into(),
                "concurrent decision".into(),
            ],
            vec![
                "reject".into(),
                task.clone(),
                "--reason".into(),
                "different decision".into(),
            ],
            vec![
                "accept".into(),
                task.clone(),
                "--reason".into(),
                "concurrent decision".into(),
            ],
        ];
        let outputs = compete_variants_at(
            &f,
            round,
            &variants,
            f.logs.join(format!("release-{round}")),
        );
        for (index, output) in outputs.iter().enumerate() {
            let stderr = fs::read_to_string(f.logs.join(format!("{round}-{index}.stderr")))
                .unwrap_or_default();
            if !output.status.success() {
                assert!(
                    stderr.contains("decision") || stderr.contains("candidate"),
                    "unclear losing decision refusal in round {round}: {stderr}"
                );
            }
            assert!(
                !raw_store_error(&stderr),
                "cleanup/store emitted raw error: {stderr}"
            );
            assert!(
                !raw_cleanup_error(&stderr),
                "pane/worktree cleanup emitted a raw error: {stderr}"
            );
            assert!(
                !raw_store_error(&String::from_utf8_lossy(&output.stdout)),
                "raw error: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        let store = brgr_store::Store::open(f.home.join("store")).unwrap();
        let id = task.parse().unwrap();
        let result = store.latest_result(id).unwrap();
        match store
            .decision_for_result(result.result_id)
            .unwrap()
            .unwrap()
            .verdict
        {
            brgr_protocol::DecisionVerdict::Accepted => accepted += 1,
            brgr_protocol::DecisionVerdict::Rejected => rejected += 1,
        }
        assert_eq!(
            store
                .inbox(
                    &brgr_protocol::OwnerId::new("codex:concurrency-settle").unwrap(),
                    true
                )
                .unwrap()
                .len(),
            1
        );
    }
    eprintln!("decision race split: accepted={accepted}, rejected={rejected}");
}

#[test]
fn concurrent_status_watch_reconcile_crash_windows_once() {
    for (round, (stage, expected)) in [
        ("after_claim", "lost"),
        ("after_launch_intent", "lost"),
        ("after_spawn_before_pid", "lost"),
        ("after_seal_before_commit", "lost"),
        ("after_terminal_commit", "candidate"),
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::new();
        let output = brgr(&f.home)
            .args([
                "--json",
                "run",
                "BRGR_FIXTURE_OK",
                "--harness",
                "local.fixture",
                "--workspace",
            ])
            .arg(&f.scratch)
            .env("BRGR_TEST_CRASH_STAGE", stage)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "crash-stage launch {stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
        let task = receipt["task_id"].as_str().unwrap().to_owned();
        let variants = vec![
            vec!["status".into(), task.clone()],
            vec!["status".into(), task.clone()],
            vec![
                "watch".into(),
                task.clone(),
                "--timeout-seconds".into(),
                "2".into(),
            ],
            vec![
                "watch".into(),
                task.clone(),
                "--timeout-seconds".into(),
                "2".into(),
            ],
        ];
        let outputs = compete_variants_at(
            &f,
            round + ROUNDS,
            &variants,
            f.logs.join(format!("release-{}", round + ROUNDS)),
        );
        for output in outputs {
            assert!(
                output.status.success(),
                "status/watch refused stage {stage}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_reconciled_once(&f, &task, expected, stage);
    }
}

fn assert_reconciled_once(f: &Fixture, task: &str, expected: &str, stage: &str) {
    assert!(
        wait_until(|| brgr(&f.home)
            .args(["--json", "result", task])
            .output()
            .is_ok_and(|o| o.status.success())),
        "stage {stage} did not reconcile"
    );
    let store = brgr_store::Store::open(f.home.join("store")).unwrap();
    let id = task.parse().unwrap();
    let result = store.latest_result(id).unwrap();
    assert_eq!(
        format!("{:?}", result.outcome).to_lowercase(),
        expected,
        "stage {stage}"
    );
    let owner = brgr_protocol::OwnerId::new("codex:concurrency-settle").unwrap();
    assert_eq!(
        store.inbox(&owner, true).unwrap().len(),
        1,
        "duplicate inbox stage {stage}"
    );
    assert_eq!(
        store.attempt_state(id).unwrap(),
        brgr_protocol::AttemptState::Terminal,
        "stage {stage}"
    );
    assert!(
        store
            .claim_attempt(id, 1, brgr_protocol::AttemptId::new())
            .is_err(),
        "attempt restarted at stage {stage}"
    );
    assert!(
        store.run_completed(result.result_id).unwrap(),
        "notice not recorded stage {stage}"
    );
    let connection = rusqlite::Connection::open(f.home.join("store/brgr.sqlite3")).unwrap();
    let notices: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM completion_notifications WHERE result_id = ?1",
            [result.result_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1, "duplicate notice at stage {stage}");
}

#[test]
fn concurrent_status_reconciles_sigkilled_supervisor_once() {
    for round in 0..ROUNDS {
        let f = Fixture::new();
        let task = f.start_slow(round);
        let receipt_path = f.home.join("runs").join(format!("{task}.supervisor.json"));
        assert!(
            wait_until(|| receipt_path.exists()),
            "supervisor receipt not written"
        );
        let receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        let pid = receipt["identity"]["handle"].as_str().unwrap();
        let killed = Command::new("/bin/kill")
            .args(["-KILL", pid])
            .status()
            .unwrap();
        assert!(killed.success(), "failed to SIGKILL supervisor");
        assert!(
            wait_until(|| !common::process_exists(pid.trim())),
            "supervisor remained after SIGKILL"
        );
        let outputs = compete(&f, round, &["status", &task]);
        for output in outputs {
            assert!(
                output.status.success(),
                "status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut result = None;
        assert!(
            wait_until(|| {
                result = Some(
                    brgr(&f.home)
                        .args(["--json", "result", &task])
                        .output()
                        .unwrap(),
                );
                result
                    .as_ref()
                    .is_some_and(|output| output.status.success())
            }),
            "reconcile did not seal a result in round {round}"
        );
        let result = result.unwrap();
        assert!(
            result.status.success(),
            "result unavailable: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["result"]["outcome"], "lost", "round {round}: {value}");
        let store = brgr_store::Store::open(f.home.join("store")).unwrap();
        let id = task.parse().unwrap();
        assert_eq!(
            store
                .inbox(
                    &brgr_protocol::OwnerId::new("codex:concurrency-settle").unwrap(),
                    true
                )
                .unwrap()
                .len(),
            1,
            "round {round}"
        );
        assert_eq!(
            store.latest_result(id).unwrap().outcome,
            brgr_protocol::TerminalOutcome::Lost
        );
        assert_eq!(
            store.attempt_state(id).unwrap(),
            brgr_protocol::AttemptState::Terminal,
            "round {round}"
        );
    }
}
