//! `brgr run` and `brgr status` print a human form on a terminal. Stdout is
//! always a pipe in tests, so these pin the other half of the contract: with
//! a pipe or `--json` the output is exactly what it was before — the compact
//! or pretty JSON, byte for byte. The human form itself is covered by unit
//! tests on the pure render functions, reached through the injectable
//! `use_human_output` gate.

mod common;

use std::{fs, path::Path};

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
