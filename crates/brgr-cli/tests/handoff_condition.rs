//! The note a head gets when its usage passes the configured limit.
use std::{fs, path::Path, process::Command};

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
    if matches!(args.first(), Some(&"run")) {
        command.arg("--headless");
    }
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove("CLAUDE_CODE_SESSION_ID");
    command.env_remove("CURSOR_CONVERSATION_ID");
    for name in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_BIN_PATH",
        "HERDR_SOCKET_PATH",
        "HERDR_SESSION",
        "HERDR_PLUGIN_ID",
    ] {
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

fn gjc_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

fn gjc_home() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let receipt = json_output(&run(
        &home,
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
    (temp, home, workspace)
}

fn head_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-head"),
    ]
}

fn start_task(home: &Path, workspace: &Path) -> String {
    let created = json_output(&run(
        home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &head_env(),
    ));
    created["task_id"].as_str().unwrap().to_owned()
}

const NOW_OFFSET: u64 = 0;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + NOW_OFFSET
}

fn write_signal(home: &Path, session: &str, context: f64, at: u64) {
    let dir = home.join("context");
    fs::create_dir_all(&dir).unwrap();
    let signal = serde_json::json!({
        "session": session,
        "source": "status-line",
        "at": at,
        "context_percent": context,
        "context_window": 1_000_000,
        "five_hour_percent": null,
        "five_hour_resets_at": null,
        "seven_day_percent": null,
        "seven_day_resets_at": null,
    });
    fs::write(
        dir.join(format!("{session}.json")),
        serde_json::to_vec(&signal).unwrap(),
    )
    .unwrap();
}

fn arm(home: &Path) {
    json_output(&run(
        home,
        &[
            "config",
            "set-handoff-condition",
            "--kind",
            "claude",
            "--exit-old",
            "--context-percent",
            "70",
            "--agent-arg=--dangerously-skip-permissions",
        ],
        &[],
    ));
}

fn stderr_of(home: &Path, task_id: &str, envs: &[(&str, &str)]) -> String {
    let output = run(home, &["result", task_id], envs);
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_head_over_its_limit_is_told_once_with_the_exact_handoff_command() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm(&home);
    write_signal(&home, "session-head", 72.0, now());
    let first = stderr_of(&home, &task_id, &head_env());
    assert!(first.contains("context window 72% (limit 70%)"), "{first}");
    assert!(
        first.contains(
            "brgr head handoff TASK --kind claude --exit-old --agent-arg=--dangerously-skip-permissions"
        ),
        "{first}"
    );
    let second = stderr_of(&home, &task_id, &head_env());
    assert!(!second.contains("over the limit"), "{second}");
}

#[test]
fn nothing_is_said_without_a_configured_condition_a_fresh_signal_or_the_bound_session() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    write_signal(&home, "session-head", 99.0, now());
    // No condition configured.
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
    arm(&home);
    // A signal older than the freshness limit.
    write_signal(&home, "session-head", 99.0, now() - 3_600);
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
    // A different session than the one the owner is bound to.
    write_signal(&home, "session-other", 99.0, now());
    let other = [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-other"),
    ];
    assert!(!stderr_of(&home, &task_id, &other).contains("over the limit"));
}

#[test]
fn an_open_handoff_silences_the_note() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm(&home);
    write_signal(&home, "session-head", 80.0, now());
    // The request is itself a brgr command the head runs, so it is where a note
    // would first appear if an open handoff did not silence it.
    let request = run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "open-handoff",
        ],
        &head_env(),
    );
    assert!(request.status.success());
    let said = String::from_utf8_lossy(&request.stderr);
    assert!(!said.contains("over the limit"), "{said}");
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
}

#[test]
fn a_handoff_condition_needs_a_target_and_a_percentage() {
    let (_temp, home, _workspace) = gjc_home();
    for args in [
        vec!["config", "set-handoff-condition", "--kind", "claude"],
        vec![
            "config",
            "set-handoff-condition",
            "--kind",
            "claude",
            "--context-percent",
            "100",
        ],
        vec![
            "config",
            "set-handoff-condition",
            "--kind",
            "two words",
            "--context-percent",
            "70",
        ],
    ] {
        assert!(!run(&home, &args, &[]).status.success(), "{args:?}");
    }
    arm(&home);
    json_output(&run(&home, &["config", "clear-handoff-condition"], &[]));
    let config = json_output(&run(&home, &["config", "show"], &[]));
    assert!(config.get("handoff").is_none(), "{config}");
}
