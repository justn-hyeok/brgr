//! `brgr run`, `brgr status`, and `brgr doctor` print a human form on a
//! terminal. Stdout is always a pipe in tests, so these pin the other half of
//! the contract: with a pipe or `--json` the output is exactly what it was
//! before — the compact or pretty JSON, byte for byte — plus doctor's new
//! top-level `problems` array. The human form itself is covered by unit tests
//! on the pure render functions, reached through the injectable
//! `use_human_output` gate.

mod common;

use std::{
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use common::{add_fixture, gjc_fixture, json_output, run};
use serde_json::Value;
use tempfile::TempDir;

/// The pretty JSON a non-`--json` call prints when stdout is not a terminal.
fn pretty(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

/// Asserts both halves of the pipe contract for one command: the non-json
/// call's stdout parses to the same value as `--json`'s, and its bytes are
/// exactly `serde_json::to_string_pretty` of that value plus a newline.
fn assert_piped_output_is_the_pretty_json(home: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let mut compact_args = vec!["--json"];
    compact_args.extend_from_slice(args);
    let compact: Value = serde_json::from_slice(&run(home, &compact_args, envs).stdout).unwrap();

    let piped = run(home, args, envs);
    assert!(
        piped.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let parsed: Value = serde_json::from_slice(&piped.stdout)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&piped.stdout)));
    assert_eq!(parsed, compact);
    assert_eq!(String::from_utf8_lossy(&piped.stdout), pretty(&parsed));
}

fn fixture_home() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    add_fixture(&home, &gjc_fixture(), &temp.path().join("scratch"));
    (temp, home, workspace)
}

/// A task that ran to a sealed result, for `status` to list.
fn completed_task(home: &Path, workspace: &Path) -> String {
    let receipt = json_output(&run(
        home,
        &[
            "--json",
            "run",
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[],
    ));
    receipt["task_id"].as_str().unwrap().to_owned()
}

#[test]
fn status_list_on_a_pipe_is_the_pretty_json_byte_for_byte() {
    let (_temp, home, workspace) = fixture_home();
    // Empty first: `status` on a fresh home still has to answer as JSON.
    assert_piped_output_is_the_pretty_json(&home, &["status"], &[]);
    completed_task(&home, &workspace);
    assert_piped_output_is_the_pretty_json(&home, &["status"], &[]);
}

#[test]
fn status_task_on_a_pipe_is_the_pretty_json_byte_for_byte() {
    let (_temp, home, workspace) = fixture_home();
    let task = completed_task(&home, &workspace);
    assert_piped_output_is_the_pretty_json(&home, &["status", &task], &[]);
}

#[test]
fn status_tree_on_a_pipe_is_the_pretty_json_byte_for_byte() {
    let (_temp, home, workspace) = fixture_home();
    let task = completed_task(&home, &workspace);
    assert_piped_output_is_the_pretty_json(&home, &["status", &task, "--tree"], &[]);
}

#[test]
fn status_verbose_on_a_pipe_is_the_pretty_json_byte_for_byte() {
    let (_temp, home, workspace) = fixture_home();
    let task = completed_task(&home, &workspace);
    assert_piped_output_is_the_pretty_json(&home, &["status"], &[]);
    assert_piped_output_is_the_pretty_json(&home, &["status", "--verbose"], &[]);
    assert_piped_output_is_the_pretty_json(&home, &["status", &task, "--verbose"], &[]);
}

#[test]
fn foreground_run_on_a_pipe_is_the_result_receipt_byte_for_byte() {
    let (temp, home, workspace) = fixture_home();
    let workspace_arg = workspace.to_str().unwrap();
    let compact = json_output(&run(
        &home,
        &[
            "--json",
            "run",
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            workspace_arg,
        ],
        &[],
    ));
    let workspace_arg = temp_workspace(&temp);
    let piped = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            &workspace_arg,
        ],
        &[],
    );
    assert!(
        piped.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let parsed: Value = serde_json::from_slice(&piped.stdout)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&piped.stdout)));
    // The receipt differs only in its ids: same shape, same outcome.
    assert_eq!(parsed["outcome"], compact["outcome"]);
    assert_eq!(parsed["schema"], compact["schema"]);
    assert!(parsed["task_id"].as_str().unwrap().len() == 36);
    assert_eq!(String::from_utf8_lossy(&piped.stdout), pretty(&parsed));
}

#[test]
fn detached_run_on_a_pipe_is_the_launch_receipt_byte_for_byte() {
    let (temp, home, _workspace) = fixture_home();
    let piped = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            &temp_workspace(&temp),
        ],
        &[],
    );
    assert!(
        piped.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let parsed: Value = serde_json::from_slice(&piped.stdout)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&piped.stdout)));
    assert_eq!(parsed["state"], "starting");
    assert_eq!(String::from_utf8_lossy(&piped.stdout), pretty(&parsed));
    // `--json` on a second launch receipt has the same shape.
    let compact = json_output(&run(
        &home,
        &[
            "--json",
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            &temp_workspace(&temp),
        ],
        &[],
    ));
    assert_eq!(compact["state"], "starting");
    assert_eq!(compact["schema"], parsed["schema"]);
}

/// A fresh directory for a run's `--workspace`; each run takes its own.
fn temp_workspace(temp: &TempDir) -> String {
    let workspace = temp.path().join(format!("work-{}", uuid_suffix()));
    fs::create_dir_all(&workspace).unwrap();
    workspace.to_str().unwrap().to_owned()
}

fn uuid_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}")
}

/// A home with a certified GJC fixture and a current Codex integration, so
/// `brgr doctor` is healthy until a test drifts the executable.
fn doctor_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let executable = temp.path().join("gjc");
    fs::copy(gjc_fixture(), &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let envs = [("CODEX_HOME", codex_home.to_str().unwrap())];
    json_output(&run(
        &home,
        &["--json", "integrate", "codex", "install"],
        &envs,
    ));
    (temp, home, executable, codex_home)
}

fn doctor_envs(codex_home: &Path) -> [(&str, &str); 1] {
    [("CODEX_HOME", codex_home.to_str().unwrap())]
}

fn drift_harness_executable(executable: &Path) {
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(executable)
            .unwrap(),
        "# drift"
    )
    .unwrap();
}

/// Doctor JSON keys, including the added `problems` array. `serde_json` maps
/// serialize in sorted order, so this is alphabetical; existing fields stay
/// in that same relative order around the new key.
const DOCTOR_JSON_KEYS: [&str; 8] = [
    "claude_integration",
    "codex_integration",
    "codex_trace",
    "harnesses",
    "problems",
    "registry",
    "status",
    "store",
];

/// The keys doctor printed before `problems` was added.
const DOCTOR_EXISTING_KEYS: [&str; 7] = [
    "claude_integration",
    "codex_integration",
    "codex_trace",
    "harnesses",
    "registry",
    "status",
    "store",
];

fn doctor_object_keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn existing_doctor_fields(value: &Value) -> Value {
    let mut old = value.clone();
    old.as_object_mut()
        .unwrap()
        .retain(|key, _| DOCTOR_EXISTING_KEYS.contains(&key.as_str()));
    old
}

#[test]
fn doctor_exit_error_names_the_unhealthy_harness() {
    let (_temp, home, executable, codex_home) = doctor_fixture();
    let envs = doctor_envs(&codex_home);
    drift_harness_executable(&executable);
    let changed = run(&home, &["doctor"], &envs);
    assert!(!changed.status.success(), "{changed:?}");
    let stderr = String::from_utf8_lossy(&changed.stderr);
    assert!(
        stderr.contains("brgr doctor found an unavailable personal-use path: local.gjc"),
        "{stderr}"
    );
}

#[test]
fn doctor_json_keeps_existing_fields_and_adds_problems() {
    let (_temp, home, executable, codex_home) = doctor_fixture();
    let envs = doctor_envs(&codex_home);
    drift_harness_executable(&executable);
    let changed = run(&home, &["--json", "doctor"], &envs);
    assert!(!changed.status.success(), "{changed:?}");
    let report: Value = serde_json::from_slice(&changed.stdout).unwrap();
    let keys = doctor_object_keys(&report);
    assert_eq!(
        keys, DOCTOR_JSON_KEYS,
        "existing fields keep their names; problems is added: {keys:?}"
    );
    assert_eq!(report["status"], "needs_attention");
    assert_eq!(report["store"], true);
    assert_eq!(report["registry"], true);
    assert_eq!(report["harnesses"][0]["id"], "local.gjc");
    assert_eq!(report["harnesses"][0]["health"], "executable_changed");
    let problems = report["problems"]
        .as_array()
        .expect("problems array is present");
    assert!(
        problems.iter().any(|problem| problem["name"] == "local.gjc"
            && problem["reason"] == "recertify"
            && problem["command"] == "brgr harness recertify local.gjc"),
        "{problems:?}"
    );
    let old = existing_doctor_fields(&report);
    assert_eq!(doctor_object_keys(&old), DOCTOR_EXISTING_KEYS);
    assert_eq!(old["harnesses"][0]["id"], report["harnesses"][0]["id"]);
    assert_eq!(
        old["harnesses"][0]["health"],
        report["harnesses"][0]["health"]
    );
    assert!(old.get("problems").is_none());
}

#[test]
fn doctor_on_a_pipe_is_the_pretty_json_byte_for_byte() {
    let (_temp, home, _executable, codex_home) = doctor_fixture();
    let envs = doctor_envs(&codex_home);
    assert_piped_output_is_the_pretty_json(&home, &["doctor"], &envs);
}

#[test]
fn doctor_healthy_json_has_an_empty_problems_array() {
    let (_temp, home, _executable, codex_home) = doctor_fixture();
    let envs = doctor_envs(&codex_home);
    let report = json_output(&run(&home, &["--json", "doctor"], &envs));
    assert_eq!(report["status"], "ok");
    assert_eq!(doctor_object_keys(&report), DOCTOR_JSON_KEYS, "{report}");
    assert_eq!(report["problems"], serde_json::json!([]));
}
