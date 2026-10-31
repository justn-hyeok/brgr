//! Black-box runner for `brgr workflow run|resume|cancel` and the hidden driver.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
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

fn require_node() {
    let output = Command::new("node")
        .arg("-v")
        .output()
        .expect("node must be executable on PATH");
    assert!(
        output.status.success(),
        "node -v failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

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

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    root: PathBuf,
}

fn fixture_home() -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    fs::create_dir_all(&workspace).unwrap();
    seed_git_repo(&workspace);
    add_fixture(&home, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        root,
    }
}

fn owner_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ]
}

fn write_script(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, source).unwrap();
    path
}

fn workflow_log(home: &Path, run_id: &str) -> PathBuf {
    home.join("runs").join(format!("{run_id}.workflow.log"))
}

fn workflow_pid(home: &Path, run_id: &str) -> PathBuf {
    home.join("runs").join(format!("{run_id}.workflow.pid"))
}

fn wait_run_state(home: &Path, run_id: &str, want: &str, timeout: Duration) -> Value {
    let started = Instant::now();
    let envs = owner_env();
    loop {
        let output = run(home, &["workflow", "status", run_id], &envs);
        if output.status.success() {
            let value = serde_json::from_slice::<Value>(&output.stdout).unwrap();
            if value["state"].as_str() == Some(want) {
                return value;
            }
        }
        assert!(
            started.elapsed() <= timeout,
            "run {run_id} did not reach {want}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_until<F>(timeout: Duration, mut ready: F) -> bool
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    while started.elapsed() < timeout {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn notification_count(home: &Path, task_id: &str) -> i64 {
    let connection = rusqlite::Connection::open(home.join("store").join("brgr.sqlite3")).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM completion_notifications WHERE task_id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn process_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

#[test]
fn workflow_run_returns_before_a_sleeping_script_finishes() {
    require_node();
    let fixture = fixture_home();
    let script = write_script(
        &fixture.root,
        "sleep.mjs",
        "console.log('workflow-driver-stdout');\nawait new Promise((resolve) => setTimeout(resolve, 2000));\n",
    );
    let started = Instant::now();
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
    ));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "workflow run blocked for {elapsed:?}"
    );
    let run_id = receipt["run_id"].as_str().unwrap();
    assert_eq!(receipt["state"], "running");
    assert_eq!(receipt["owner"], "codex:workflow-owner");
    wait_run_state(&fixture.home, run_id, "completed", Duration::from_secs(15));
    let log = fs::read_to_string(workflow_log(&fixture.home, run_id)).unwrap();
    assert!(
        log.contains("workflow-driver-stdout"),
        "log missing script stdout: {log}"
    );
}

#[test]
fn a_script_that_exits_nonzero_leaves_the_run_failed() {
    require_node();
    let fixture = fixture_home();
    let script = write_script(&fixture.root, "fail.mjs", "process.exit(1);\n");
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
    ));
    let run_id = receipt["run_id"].as_str().unwrap();
    wait_run_state(&fixture.home, run_id, "failed", Duration::from_secs(15));
}

#[test]
fn the_driver_passes_allowlisted_environment_and_strips_the_parent_sentinel() {
    require_node();
    let fixture = fixture_home();
    let script = write_script(
        &fixture.root,
        "env.mjs",
        "console.log(JSON.stringify(process.env));\n",
    );
    let mut envs = owner_env().to_vec();
    envs.push(("WORKFLOW_PARENT_SECRET", "should-not-leak"));
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &envs,
    ));
    let run_id = receipt["run_id"].as_str().unwrap();
    wait_run_state(&fixture.home, run_id, "completed", Duration::from_secs(15));
    let log = fs::read_to_string(workflow_log(&fixture.home, run_id)).unwrap();
    let env: Value =
        serde_json::from_str(log.lines().find(|line| line.starts_with('{')).unwrap()).unwrap();
    assert_eq!(env["BRGR_WORKFLOW_RUN"], run_id);
    assert_eq!(env["BRGR_BIN"], brgr());
    assert_eq!(env["BRGR_OWNER_ID"], "codex:workflow-owner");
    assert_eq!(env["BRGR_SESSION_ID"], "workflow-session");
    assert!(env.get("WORKFLOW_PARENT_SECRET").is_none(), "{env}");
}

#[test]
fn resume_after_a_dead_driver_replays_a_completed_step_instead_of_creating_another() {
    require_node();
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let source = format!(
        r#"
import {{ spawnSync }} from "node:child_process";
const brgr = process.env.BRGR_BIN;
const run = process.env.BRGR_WORKFLOW_RUN;
const launched = spawnSync(brgr, [
  "--home", process.env.BRGR_HOME,
  "--json",
  "--headless",
  "run",
  "BRGR_FIXTURE_OK",
  "--workspace", {workspace},
  "--workflow", run,
  "--step", "fetch",
], {{ encoding: "utf8" }});
if (launched.status !== 0) {{
  console.error(launched.stderr);
  process.exit(launched.status ?? 1);
}}
await new Promise((resolve) => setTimeout(resolve, 8000));
"#,
        workspace = json!(workspace)
    );
    let script = write_script(&fixture.root, "step-then-sleep.mjs", &source);
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
    ));
    let run_id = receipt["run_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &["workflow", "status", &run_id],
                &owner_env(),
            );
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .and_then(|value| value["steps"].as_array().cloned())
                    .is_some_and(|steps| !steps.is_empty())
        }),
        "step was never journaled"
    );
    let before = json_output(&run(
        &fixture.home,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    let task_id = before["steps"][0]["task_id"].as_str().unwrap().to_owned();
    json_output(&run(
        &fixture.home,
        &["wait", &task_id, "--timeout-seconds", "90"],
        &owner_env(),
    ));
    assert!(
        wait_until(Duration::from_secs(5), || workflow_pid(
            &fixture.home,
            &run_id
        )
        .is_file()),
        "driver pid file missing"
    );
    let pid = read_pid(&workflow_pid(&fixture.home, &run_id));
    let killed = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .status()
        .unwrap();
    assert!(killed.success(), "failed to kill driver group {pid}");
    assert!(wait_until(Duration::from_secs(5), || !process_alive(pid)));
    let resumed = json_output(&run(
        &fixture.home,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    assert_ne!(
        resumed.get("driver").and_then(Value::as_str),
        Some("already running")
    );
    wait_run_state(&fixture.home, &run_id, "completed", Duration::from_secs(20));
    let after = json_output(&run(
        &fixture.home,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    let steps = after["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "resume created extra steps: {after}");
    assert_eq!(steps[0]["task_id"], task_id);
    assert_eq!(steps[0]["step_key"], "fetch");
}

#[test]
fn resume_with_a_live_driver_starts_nothing() {
    require_node();
    let fixture = fixture_home();
    let script = write_script(
        &fixture.root,
        "idle.mjs",
        "await new Promise((resolve) => setTimeout(resolve, 4000));\n",
    );
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
    ));
    let run_id = receipt["run_id"].as_str().unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || workflow_pid(
            &fixture.home,
            run_id
        )
        .is_file()),
        "driver pid file missing"
    );
    let pid_before = fs::read_to_string(workflow_pid(&fixture.home, run_id)).unwrap();
    let resumed = json_output(&run(
        &fixture.home,
        &["workflow", "resume", run_id],
        &owner_env(),
    ));
    let text = format!("{resumed}");
    assert!(
        text.contains("already running") || resumed["driver"] == "already_running",
        "resume did not report an already-running driver: {resumed}"
    );
    let pid_after = fs::read_to_string(workflow_pid(&fixture.home, run_id)).unwrap();
    assert_eq!(pid_before, pid_after);
    wait_run_state(&fixture.home, run_id, "completed", Duration::from_secs(15));
}

#[test]
fn cancel_stops_the_driver_and_unfinished_steps_and_is_idempotent() {
    require_node();
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let source = format!(
        r#"
import {{ spawnSync }} from "node:child_process";
const brgr = process.env.BRGR_BIN;
const run = process.env.BRGR_WORKFLOW_RUN;
const launched = spawnSync(brgr, [
  "--home", process.env.BRGR_HOME,
  "--json",
  "--headless",
  "run",
  "SLOW unfinished step",
  "--workspace", {workspace},
  "--workflow", run,
  "--step", "slow",
], {{ encoding: "utf8" }});
if (launched.status !== 0) {{
  console.error(launched.stderr);
  process.exit(launched.status ?? 1);
}}
await new Promise((resolve) => setTimeout(resolve, 60000));
"#,
        workspace = json!(workspace)
    );
    let script = write_script(&fixture.root, "slow-step.mjs", &source);
    let receipt = json_output(&run(
        &fixture.home,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
    ));
    let run_id = receipt["run_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &["workflow", "status", &run_id],
                &owner_env(),
            );
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .and_then(|value| value["steps"].as_array().cloned())
                    .is_some_and(|steps| !steps.is_empty())
        }),
        "unfinished step was never journaled"
    );
    let listed = json_output(&run(
        &fixture.home,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    let task_id = listed["steps"][0]["task_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(Duration::from_secs(5), || workflow_pid(
            &fixture.home,
            &run_id
        )
        .is_file()),
        "driver pid file missing"
    );
    let pid = read_pid(&workflow_pid(&fixture.home, &run_id));
    let cancelled = json_output(&run(
        &fixture.home,
        &["workflow", "cancel", &run_id],
        &owner_env(),
    ));
    assert_eq!(cancelled["state"], "cancelled");
    wait_run_state(&fixture.home, &run_id, "cancelled", Duration::from_secs(10));
    assert!(
        wait_until(Duration::from_secs(5), || !process_alive(pid)),
        "driver {pid} still alive"
    );
    assert!(
        wait_until(Duration::from_secs(15), || {
            let status = run(
                &fixture.home,
                &["workflow", "status", &run_id],
                &owner_env(),
            );
            serde_json::from_slice::<Value>(&status.stdout)
                .ok()
                .and_then(|value| value["steps"][0]["state"].as_str().map(str::to_owned))
                .is_some_and(|state| state != "running" && state != "starting" && state != "queued")
        }),
        "step {task_id} was not cancelled"
    );
    let again = run(
        &fixture.home,
        &["workflow", "cancel", &run_id],
        &owner_env(),
    );
    assert!(
        again.status.success(),
        "second cancel failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
}

#[test]
fn a_workflow_step_creates_no_completion_notification_row() {
    let fixture = fixture_home();
    let envs = owner_env();
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
        notification_count(&fixture.home, ordinary_task) > 0,
        "ordinary task wrote no completion_notifications row"
    );

    let created = json_output(&run(&fixture.home, &["workflow", "create"], &envs));
    let run_id = created["run_id"].as_str().unwrap();
    let step = json_output(&run(
        &fixture.home,
        &[
            "run",
            "BRGR_FIXTURE_OK workflow",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--workflow",
            run_id,
            "--step",
            "notify",
            "--foreground",
        ],
        &envs,
    ));
    let step_task = step["task_id"].as_str().unwrap();
    assert_eq!(
        notification_count(&fixture.home, step_task),
        0,
        "workflow step wrote a completion_notifications row"
    );
}

#[test]
fn the_host_bridge_refuses_workflow_drive() {
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let home = fixture.home.to_str().unwrap();
    let script = write_script(&fixture.root, "noop.mjs", "console.log('ok');\n");
    let bridge = [
        ("BRGR_PLUGIN_HOST_HOME", home),
        ("BRGR_PLUGIN_HOST_WORKSPACE", workspace),
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ];
    let denied = run(
        &fixture.home,
        &[
            "workflow",
            "__drive",
            "wf-aaaaaaaaaaaa",
            script.to_str().unwrap(),
        ],
        &bridge,
    );
    assert!(
        !denied.status.success(),
        "workflow __drive was allowed through the host bridge"
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("unavailable through the Herdr host bridge")
            || stderr.contains("internal brgr commands are unavailable"),
        "{stderr}"
    );
}

#[test]
fn hidden_drive_is_absent_from_workflow_help() {
    let output = brgr_command()
        .args(["workflow", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        !help.contains("__drive"),
        "hidden driver leaked into --help: {help}"
    );
}
