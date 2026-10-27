//! Black-box admission of workflow steps through `brgr workflow` and `brgr run`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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
    if matches!(args.first(), Some(&"run" | &"revise")) {
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
}

fn fixture_home() -> Fixture {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("repo");
    fs::create_dir_all(&workspace).unwrap();
    seed_git_repo(&workspace);
    add_fixture(&home, &temp.path().join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
    }
}

fn owner_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ]
}

fn create_run(home: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut args = vec!["workflow", "create"];
    args.extend_from_slice(extra);
    json_output(&run(home, &args, envs))
}

fn launch_step(
    home: &Path,
    workspace: &Path,
    run_id: &str,
    step: &str,
    objective: &str,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> std::process::Output {
    let workspace = workspace.to_str().unwrap();
    let mut args = vec![
        "run",
        objective,
        "--workspace",
        workspace,
        "--workflow",
        run_id,
        "--step",
        step,
    ];
    args.extend_from_slice(extra);
    run(home, &args, envs)
}

fn task_worktrees(home: &Path) -> Vec<PathBuf> {
    let root = home.join("worktrees");
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(&root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".locks") {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if let Ok(children) = fs::read_dir(&path) {
            found.extend(
                children
                    .filter_map(Result::ok)
                    .map(|child| child.path())
                    .filter(|child| child.is_dir()),
            );
        }
    }
    found
}

fn launch_files(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join("launches")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

#[test]
fn the_same_step_launched_twice_creates_one_task_and_one_worktree() {
    let fixture = fixture_home();
    let envs = owner_env();
    let created = create_run(&fixture.home, &[], &envs);
    let run_id = created["run_id"].as_str().unwrap();

    let first = json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    ));
    let first_task = first["task_id"].as_str().unwrap().to_owned();

    let second = json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    ));
    assert_eq!(
        second["task_id"].as_str().unwrap(),
        first_task,
        "a repeated step launched a second task: {second}"
    );

    assert_eq!(
        launch_files(&fixture.home).len(),
        1,
        "a repeated step wrote a second launch file"
    );
    assert_eq!(
        task_worktrees(&fixture.home).len(),
        1,
        "a repeated step created a second task worktree"
    );
}

#[test]
fn the_same_step_key_with_a_different_objective_is_refused_as_diverged() {
    let fixture = fixture_home();
    let envs = owner_env();
    let created = create_run(&fixture.home, &[], &envs);
    let run_id = created["run_id"].as_str().unwrap();

    json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    ));

    let refused = launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK changed",
        &["--foreground"],
        &envs,
    );
    assert!(
        !refused.status.success(),
        "a changed objective was admitted: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("diverged"), "{stderr}");
    assert!(stderr.contains("fetch"), "{stderr}");

    assert_eq!(launch_files(&fixture.home).len(), 1);
    assert_eq!(task_worktrees(&fixture.home).len(), 1);
}

#[test]
fn a_second_unfinished_step_exits_75_until_the_first_has_a_result() {
    let fixture = fixture_home();
    let envs = owner_env();
    let created = create_run(&fixture.home, &["--max-parallel", "1"], &envs);
    let run_id = created["run_id"].as_str().unwrap();

    let first = json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "one",
        "SLOW first step",
        &[],
        &envs,
    ));
    let first_task = first["task_id"].as_str().unwrap().to_owned();
    assert_eq!(task_worktrees(&fixture.home).len(), 1);

    let refused = launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "two",
        "BRGR_FIXTURE_OK second step",
        &[],
        &envs,
    );
    assert_eq!(
        refused.status.code(),
        Some(75),
        "parallel limit used {}: {}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!(
            "workflow run {run_id} is at its parallel limit; retry later"
        )),
        "{stderr}"
    );
    assert_eq!(
        task_worktrees(&fixture.home).len(),
        1,
        "the refused step left a worktree behind"
    );
    assert_eq!(launch_files(&fixture.home).len(), 1);

    json_output(&run(
        &fixture.home,
        &["wait", &first_task, "--timeout-seconds", "90"],
        &envs,
    ));

    let admitted = json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "two",
        "BRGR_FIXTURE_OK second step",
        &["--foreground"],
        &envs,
    ));
    assert_ne!(admitted["task_id"].as_str().unwrap(), first_task);
    assert_eq!(task_worktrees(&fixture.home).len(), 2);
}

#[test]
fn a_run_created_by_one_owner_cannot_be_used_by_another() {
    let fixture = fixture_home();
    let owner = owner_env();
    let created = create_run(&fixture.home, &[], &owner);
    let run_id = created["run_id"].as_str().unwrap();
    let stranger = [
        ("BRGR_OWNER_ID", "codex:someone-else"),
        ("BRGR_SESSION_ID", "stranger-session"),
    ];

    let refused = launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &stranger,
    );
    assert!(
        !refused.status.success(),
        "another owner used the run: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(launch_files(&fixture.home).len(), 0);
    assert_eq!(task_worktrees(&fixture.home).len(), 0);

    let status = run(&fixture.home, &["workflow", "status", run_id], &stranger);
    assert!(!status.status.success());
    let set = run(
        &fixture.home,
        &["workflow", "set-state", run_id, "blocked"],
        &stranger,
    );
    assert!(!set.status.success());
}

#[test]
fn workflow_create_then_status_shows_the_run_and_then_its_step() {
    let fixture = fixture_home();
    let envs = owner_env();
    let created = create_run(&fixture.home, &[], &envs);
    assert_eq!(created["state"], "running");
    assert_eq!(created["max_parallel"], 4);
    assert_eq!(created["owner"], "codex:workflow-owner");
    let run_id = created["run_id"].as_str().unwrap();
    assert!(
        run_id.starts_with("wf-")
            && run_id.len() == 15
            && run_id[3..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "run_id {run_id} is not wf- plus 12 lowercase hex characters"
    );

    let empty = json_output(&run(&fixture.home, &["workflow", "status", run_id], &envs));
    assert_eq!(empty["run_id"], run_id);
    assert_eq!(empty["state"], "running");
    assert_eq!(empty["steps"].as_array().map(Vec::len), Some(0));

    let launched = json_output(&launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    ));
    let task_id = launched["task_id"].as_str().unwrap();

    let listed = json_output(&run(&fixture.home, &["workflow", "status", run_id], &envs));
    let steps = listed["steps"].as_array().expect("status omitted steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["step_key"], "fetch");
    assert_eq!(steps[0]["task_id"], task_id);
    assert_eq!(steps[0]["revision"], 1);
    assert!(steps[0]["state"].as_str().is_some());
}

#[test]
fn the_host_bridge_refuses_the_workflow_group() {
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let home = fixture.home.to_str().unwrap();
    let bridge = [
        ("BRGR_PLUGIN_HOST_HOME", home),
        ("BRGR_PLUGIN_HOST_WORKSPACE", workspace),
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ];

    for args in [
        vec!["workflow", "create"],
        vec!["workflow", "status", "wf-aaaaaaaaaaaa"],
        vec!["workflow", "set-state", "wf-aaaaaaaaaaaa", "blocked"],
        vec![
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace,
            "--workflow",
            "wf-aaaaaaaaaaaa",
            "--step",
            "fetch",
        ],
    ] {
        let denied = run(&fixture.home, &args, &bridge);
        assert!(
            !denied.status.success(),
            "{args:?} was allowed through the host bridge"
        );
        let stderr = String::from_utf8_lossy(&denied.stderr);
        assert!(
            stderr.contains("unavailable through the Herdr host bridge"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn the_host_bridge_allows_workflow_init_inside_the_workspace() {
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let home = fixture.home.to_str().unwrap();
    let bridge = [
        ("BRGR_PLUGIN_HOST_HOME", home),
        ("BRGR_PLUGIN_HOST_WORKSPACE", workspace),
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ];
    let allowed = run(&fixture.home, &["workflow", "init", workspace], &bridge);
    assert!(
        allowed.status.success(),
        "workflow init inside the workspace was refused: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(fixture.workspace.join("brgr-workflow.mjs").is_file());
    assert!(fixture.workspace.join("example.mjs").is_file());
}

#[test]
fn the_host_bridge_refuses_workflow_init_outside_the_workspace() {
    let fixture = fixture_home();
    let workspace = fixture.workspace.to_str().unwrap();
    let home = fixture.home.to_str().unwrap();
    let outside = fixture.workspace.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let bridge = [
        ("BRGR_PLUGIN_HOST_HOME", home),
        ("BRGR_PLUGIN_HOST_WORKSPACE", workspace),
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ];
    let denied = run(
        &fixture.home,
        &["workflow", "init", outside.to_str().unwrap()],
        &bridge,
    );
    assert!(
        !denied.status.success(),
        "workflow init escaped the workspace"
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("unavailable through the Herdr host bridge")
            || stderr.contains("outside the selected Herdr workspace"),
        "{stderr}"
    );
}

#[test]
fn workflow_init_writes_the_sdk_and_refuses_a_different_existing_file() {
    let fixture = fixture_home();
    let dir = fixture.workspace.to_str().unwrap();
    json_output(&run(&fixture.home, &["workflow", "init", dir], &[]));
    let sdk = fixture.workspace.join("brgr-workflow.mjs");
    let example = fixture.workspace.join("example.mjs");
    assert!(sdk.is_file(), "init did not write brgr-workflow.mjs");
    assert!(example.is_file(), "init did not write example.mjs");
    let sdk_text = fs::read_to_string(&sdk).unwrap();
    assert!(sdk_text.contains("export async function agent"));
    assert!(sdk_text.contains("export async function parallel"));
    assert!(!sdk_text.contains("export async function accept"));
    assert!(!sdk_text.contains("export async function reject"));
    fs::write(&sdk, "not the sdk\n").unwrap();
    let refused = run(&fixture.home, &["workflow", "init", dir], &[]);
    assert!(
        !refused.status.success(),
        "init overwrote a different file without --force"
    );
    json_output(&run(
        &fixture.home,
        &["workflow", "init", dir, "--force"],
        &[],
    ));
    let restored = fs::read_to_string(&sdk).unwrap();
    assert!(
        restored.contains("export async function agent"),
        "--force did not restore the SDK"
    );
}

#[test]
fn a_missing_or_non_running_run_refuses_a_step() {
    let fixture = fixture_home();
    let envs = owner_env();

    let missing = launch_step(
        &fixture.home,
        &fixture.workspace,
        "wf-doesnotexist",
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    );
    assert!(
        !missing.status.success(),
        "a missing run admitted a step: {}",
        String::from_utf8_lossy(&missing.stdout)
    );
    assert_eq!(launch_files(&fixture.home).len(), 0);
    assert_eq!(task_worktrees(&fixture.home).len(), 0);

    let created = create_run(&fixture.home, &[], &envs);
    let run_id = created["run_id"].as_str().unwrap();
    json_output(&run(
        &fixture.home,
        &["workflow", "set-state", run_id, "completed"],
        &envs,
    ));

    let refused = launch_step(
        &fixture.home,
        &fixture.workspace,
        run_id,
        "fetch",
        "BRGR_FIXTURE_OK",
        &["--foreground"],
        &envs,
    );
    assert!(
        !refused.status.success(),
        "a completed run admitted a step: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(launch_files(&fixture.home).len(), 0);
    assert_eq!(task_worktrees(&fixture.home).len(), 0);
}
