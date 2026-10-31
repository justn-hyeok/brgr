//! Black-box owner notices for workflow run state changes.

mod common;

use common::write_fast_script;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

const INHERITED_WORKER_ENV: &[&str] = &[
    "BRGR_HOME",
    "BRGR_BIN",
    "BRGR_OWNER_ID",
    "BRGR_SESSION_ID",
    "BRGR_PARENT_TASK_ID",
    "BRGR_PARENT_ATTEMPT_ID",
    "BRGR_WORKER_HERDR_CONTEXT",
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CURSOR_CONVERSATION_ID",
];

const HERDR_ENV: &[&str] = &[
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_SESSION",
    "HERDR_PLUGIN_ID",
];

fn brgr_command() -> Command {
    let mut command = Command::new(brgr());
    command.env("BRGR_TEST_TRUST_HERDR_PANE", "1");
    for name in INHERITED_WORKER_ENV {
        command.env_remove(name);
    }
    command
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = brgr_command();
    command.arg("--home").arg(home).arg("--json").args(args);
    if matches!(args.first(), Some(&"run" | &"revise")) {
        command.arg("--headless");
    }
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove("CLAUDE_CODE_SESSION_ID");
    command.env_remove("CURSOR_CONVERSATION_ID");
    for name in HERDR_ENV {
        command.env_remove(name);
    }
    if !envs.iter().any(|(name, _)| {
        matches!(
            *name,
            "BRGR_SESSION_ID" | "CLAUDE_CODE_SESSION_ID" | "CURSOR_CONVERSATION_ID"
        )
    }) {
        command.env("BRGR_SESSION_ID", "fixture-session");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn json_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn gjc_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

fn add_fixture(home: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        &[
            "harness",
            "add",
            gjc_fixture().to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    ));
    assert!(receipt["scratch_result_digest"].as_str().is_some());
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

struct NoticeFixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    agent_state: PathBuf,
    prompts: PathBuf,
}

fn notice_fixture(initial_state: &str) -> NoticeFixture {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("repo");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    seed_git_repo(&workspace);
    fs::write(&agent_state, initial_state).unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\",\"agent_session\":{\"value\":\"session-a\"}}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &temp.path().join("scratch"));
    NoticeFixture {
        _temp: temp,
        home,
        workspace,
        agent_state,
        prompts,
    }
}

fn owner_envs(fixture: &NoticeFixture) -> Vec<(&'static str, String)> {
    vec![
        ("CODEX_THREAD_ID", "session-a".to_owned()),
        ("BRGR_OWNER_ID", "codex:session-a".to_owned()),
        ("BRGR_SESSION_ID", "session-a".to_owned()),
        ("HERDR_ENV", "1".to_owned()),
        ("HERDR_PANE_ID", "w1:p1".to_owned()),
        ("HERDR_WORKSPACE_ID", "w1".to_owned()),
        (
            "HERDR_BIN_PATH",
            fixture
                .home
                .parent()
                .unwrap()
                .join("herdr")
                .to_string_lossy()
                .into_owned(),
        ),
        ("HERDR_SESSION", "fixture-herdr".to_owned()),
        (
            "BRGR_TEST_AGENT_STATE",
            fixture.agent_state.to_string_lossy().into_owned(),
        ),
        (
            "BRGR_TEST_PROMPTS",
            fixture.prompts.to_string_lossy().into_owned(),
        ),
    ]
}

fn env_refs(envs: &[(impl AsRef<str>, impl AsRef<str>)]) -> Vec<(&str, &str)> {
    envs.iter()
        .map(|(name, value)| (name.as_ref(), value.as_ref()))
        .collect()
}

fn wait_for_prompt(path: &Path, needle: &str, timeout: Duration) -> String {
    let started = Instant::now();
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            started.elapsed() <= timeout,
            "prompt did not mention {needle} within {timeout:?}: {text}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn workflow_notices(home: &Path, run_id: &str) -> Vec<(i64, String, Option<String>)> {
    let connection = Connection::open(home.join("store").join("brgr.sqlite3")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT seq, state, delivered_session FROM workflow_notifications
             WHERE run_id = ?1 ORDER BY seq",
        )
        .unwrap();
    statement
        .query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

fn completion_notice_count(home: &Path, task_id: &str) -> i64 {
    let connection = Connection::open(home.join("store").join("brgr.sqlite3")).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM completion_notifications WHERE task_id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn create_run(home: &Path, envs: &[(&str, &str)]) -> String {
    json_output(&run(home, &["workflow", "create"], envs))["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_completed_run_notice_is_delivered_to_the_owner_pane_exactly_once() {
    let fixture = notice_fixture("idle");
    let owned = owner_envs(&fixture);
    let envs = env_refs(&owned);
    let run_id = create_run(&fixture.home, &envs);
    json_output(&run(
        &fixture.home,
        &["workflow", "set-state", &run_id, "completed"],
        &envs,
    ));
    let prompt = wait_for_prompt(&fixture.prompts, &run_id, Duration::from_secs(8));
    assert!(prompt.contains("FROM BRGR"), "{prompt}");
    assert!(prompt.contains("\"type\":\"brgr_workflow\""), "{prompt}");
    assert!(prompt.contains("\"state\":\"completed\""), "{prompt}");
    assert!(
        prompt.contains("Do not treat this notification as acceptance."),
        "{prompt}"
    );
    let field = format!("\"run_id\":\"{run_id}\"");
    assert_eq!(prompt.matches(field.as_str()).count(), 1, "{prompt}");
    let notices = workflow_notices(&fixture.home, &run_id);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].1, "completed");
    assert_eq!(notices[0].2.as_deref(), Some("session-a"));

    let mut duplicate = brgr_command();
    duplicate
        .arg("--home")
        .arg(&fixture.home)
        .args(["__notify-workflow", &run_id])
        .envs(envs.iter().copied());
    let mut child = duplicate.spawn().unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    thread::sleep(Duration::from_millis(400));
    let later = fs::read_to_string(&fixture.prompts).unwrap();
    assert_eq!(later.matches(field.as_str()).count(), 1, "{later}");
    assert_eq!(
        workflow_notices(&fixture.home, &run_id)
            .iter()
            .filter(|row| row.2.is_some())
            .count(),
        1
    );
}

#[test]
fn a_busy_owner_pane_keeps_the_notice_pending_until_idle() {
    let fixture = notice_fixture("working");
    let owned = owner_envs(&fixture);
    let envs = env_refs(&owned);
    let run_id = create_run(&fixture.home, &envs);
    json_output(&run(
        &fixture.home,
        &["workflow", "set-state", &run_id, "blocked"],
        &envs,
    ));
    thread::sleep(Duration::from_millis(700));
    assert!(
        !fixture.prompts.exists(),
        "busy Codex owner was prompted: {}",
        fs::read_to_string(&fixture.prompts).unwrap_or_default()
    );
    let status = json_output(&run(&fixture.home, &["workflow", "status", &run_id], &envs));
    let notices = status["notices"]
        .as_array()
        .expect("status omitted notices");
    assert_eq!(notices.len(), 1, "{status}");
    assert_eq!(notices[0]["seq"], 1);
    assert_eq!(notices[0]["state"], "blocked");
    assert_eq!(notices[0]["delivered"], false);
    fs::write(&fixture.agent_state, "idle").unwrap();
    let prompt = wait_for_prompt(&fixture.prompts, &run_id, Duration::from_secs(8));
    assert!(prompt.contains("\"state\":\"blocked\""), "{prompt}");
    let delivered = json_output(&run(&fixture.home, &["workflow", "status", &run_id], &envs));
    assert_eq!(delivered["notices"][0]["delivered"], true, "{delivered}");
}

#[test]
fn workflow_steps_still_produce_no_per_task_completion_notice() {
    let fixture = notice_fixture("idle");
    let owned = owner_envs(&fixture);
    let envs = env_refs(&owned);
    let ordinary = json_output(&run(
        &fixture.home,
        &[
            "run",
            "BRGR_FIXTURE_OK ordinary",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    ));
    let ordinary_task = ordinary["task_id"].as_str().unwrap();
    assert!(
        completion_notice_count(&fixture.home, ordinary_task) > 0,
        "ordinary task wrote no completion_notifications row"
    );

    let run_id = create_run(&fixture.home, &envs);
    let step = json_output(&run(
        &fixture.home,
        &[
            "run",
            "BRGR_FIXTURE_OK workflow",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--workflow",
            &run_id,
            "--step",
            "notify",
            "--foreground",
        ],
        &envs,
    ));
    let step_task = step["task_id"].as_str().unwrap();
    assert_eq!(
        completion_notice_count(&fixture.home, step_task),
        0,
        "workflow step wrote a completion_notifications row"
    );
}
