//! `brgr __context` as a harness status line command.

use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::Value;
use tempfile::TempDir;

const CLAUDE: &str = include_str!("../../../testdata/fixtures/statusline/claude-2.1.293.json");
const SESSION: &str = "0a91ffd2-1a5e-4c75-b65a-54cc4b3b1fa0";

fn context(home: &Path, chain: &[&str], input: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(home)
        .arg("__context")
        .args(chain)
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn recorded(home: &Path) -> Value {
    let path = home.join("context").join(format!("{SESSION}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn a_status_line_call_is_recorded_and_the_chained_status_line_still_draws() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let seen = temp.path().join("seen.json");
    let output = context(
        &home,
        &[
            "--",
            "sh",
            "-c",
            &format!("cat > {}; echo drawn", seen.display()),
        ],
        CLAUDE,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
    // The chained command got the harness's input untouched.
    assert_eq!(fs::read_to_string(&seen).unwrap(), CLAUDE);
    let signal = recorded(&home);
    assert_eq!(signal["context_percent"], 6.0);
    assert_eq!(signal["seven_day_percent"], 43.0);
    assert_eq!(signal["context_window"], 1_000_000);
}

#[test]
fn without_a_chained_status_line_it_draws_the_measured_usage() {
    let temp = TempDir::new().unwrap();
    let output = context(&temp.path().join("home"), &[], CLAUDE);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ctx 6% · 5h 7% · 7d 43%"
    );
}

#[test]
fn input_that_is_not_a_status_line_records_nothing_and_still_chains() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let output = context(
        &home,
        &["--", "sh", "-c", "cat >/dev/null; echo drawn"],
        "not json",
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
    assert!(!home.join("context").exists());
}

#[test]
fn a_status_line_that_cannot_be_recorded_still_draws() {
    let temp = TempDir::new().unwrap();
    // A file where the context directory must go.
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("context"), "in the way").unwrap();
    let output = context(
        &home,
        &["--", "sh", "-c", "cat >/dev/null; echo drawn"],
        CLAUDE,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
}

const ROLLOUT: &str =
    include_str!("../../../testdata/fixtures/codex-rollout/token-counts-0.162.1.jsonl");

fn codex_hook(home: &Path, event: &str, input: &Value) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(home)
        .args(["__hook", event])
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_codex_hook_records_the_session_from_its_rollout_file() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let rollout = temp.path().join("rollout.jsonl");
    fs::write(&rollout, ROLLOUT).unwrap();
    for event in ["session-start", "user-prompt-submit", "stop"] {
        let output = codex_hook(
            &home,
            event,
            &serde_json::json!({
                "session_id": "codex-session-1",
                "cwd": temp.path(),
                "transcript_path": rollout,
            }),
        );
        assert!(
            output.status.success(),
            "{event}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = home.join("context/codex-session-1.json");
        let signal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(signal["source"], "codex", "{event}");
        assert_eq!(signal["context_window"], 828_400, "{event}");
        assert_eq!(signal["five_hour_percent"], 24.0, "{event}");
        fs::remove_dir_all(home.join("context")).unwrap();
    }
}

#[test]
fn a_codex_hook_with_an_unreadable_rollout_still_answers() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let output = codex_hook(
        &home,
        "stop",
        &serde_json::json!({
            "session_id": "codex-session-2",
            "cwd": temp.path(),
            "transcript_path": temp.path().join("missing.jsonl"),
        }),
    );
    assert!(output.status.success());
    assert!(!home.join("context").exists());
}

/// A Codex worker's session runs in its task worktree, and its rollout is what
/// `route_observation` reads the model from. The hook records that file for
/// the task whose workspace the session runs in.
#[test]
fn a_worker_sessions_hook_records_the_rollout_for_its_task() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let worktree = home.join("worktrees/repo/aaaaaaaa");
    fs::create_dir_all(&worktree).unwrap();
    let task = brgr_protocol::TaskSpec {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        task_id: brgr_protocol::TaskId::new(),
        revision: 1,
        create_request_id: "req".to_owned(),
        owner_id: brgr_protocol::OwnerId::new("codex:test").unwrap(),
        objective: "run".to_owned(),
        workspace: worktree.to_string_lossy().into_owned(),
        route: brgr_protocol::Route {
            harness_id: "local.codex".to_owned(),
            requested_model: None,
            requested_effort: None,
        },
        required_capabilities: vec![],
        artifact_contract: brgr_protocol::ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 1_024,
        },
        acceptance_criteria: vec!["done".to_owned()],
        budget: brgr_protocol::AttemptBudget {
            deadline_seconds: 30,
            max_attempts: 1,
        },
        instructions: brgr_protocol::TaskInstructions::default(),
        evidence: brgr_protocol::EvidenceSpec::default(),
        max_concurrent_children: None,
        permission: None,
    };
    brgr_store::Store::open(home.join("store"))
        .unwrap()
        .record_task(&task, "req")
        .unwrap();
    let codex_home = temp.path().join("codex");
    let rollout = codex_home.join("sessions/2026/10/30/rollout-t-deadbeef.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(&rollout, "{}\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(&home)
        .args(["__hook", "session-start"])
        .env("CODEX_HOME", &codex_home)
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "session_id": "codex-worker-session",
                "cwd": worktree,
                "transcript_path": rollout,
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = home.join(format!("runs/{}-r1.codex-session.jsonl", task.task_id));
    let text = fs::read_to_string(&record).unwrap();
    assert!(text.contains("codex-worker-session"), "{text}");
    assert!(text.contains("rollout-t-deadbeef"), "{text}");
    // An owner's session in its own directory records no worker file.
    let output = codex_hook(
        &home,
        "stop",
        &serde_json::json!({
            "session_id": "codex-owner-session",
            "cwd": temp.path(),
            "transcript_path": rollout,
        }),
    );
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&record).unwrap().lines().count(), 1);
}
