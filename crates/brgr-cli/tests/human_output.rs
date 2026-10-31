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
    completed_task_with_objective(home, workspace, "BRGR_FIXTURE_OK")
}

fn completed_task_with_objective(home: &Path, workspace: &Path, objective: &str) -> String {
    let receipt = json_output(&run(
        home,
        &[
            "--json",
            "run",
            objective,
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

/// Top-level keys `brgr status TASK` printed before `--brief`, in the
/// order `serde_json::Value` emits without `preserve_order` (alphabetical).
/// A new field here is a pipe-contract change; `--brief` must not add one.
const STATUS_TASK_JSON_KEYS: [&str; 10] = [
    "base_commit",
    "calling_options",
    "created_at",
    "session",
    "source_pane",
    "source_session",
    "state",
    "task",
    "workspace",
    "workspace_present",
];

const BRIEF_JSON_KEYS: [&str; 5] = [
    "task_id",
    "state",
    "harness",
    "elapsed_seconds",
    "objective_head",
];

fn object_keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn compact_json_bytes(value: &Value) -> String {
    format!("{value}\n")
}

/// (1) Twenty tasks: `--brief` is one line each and at most 5% of the
/// current piped pretty JSON.
#[test]
fn brief_status_of_twenty_tasks_is_one_line_each_and_at_most_five_percent_of_piped() {
    let (_temp, home, workspace) = fixture_home();
    // Real tasks carry a long objective in the pretty JSON; brief only prints
    // the bounded head, which is why the 5% cap holds.
    let objective = format!("BRGR_FIXTURE_OK {}", "x".repeat(2048));
    let ids: Vec<String> = (0..20)
        .map(|_| completed_task_with_objective(&home, &workspace, &objective))
        .collect();

    let piped = run(&home, &["status"], &[]);
    assert!(
        piped.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let brief = run(&home, &["status", "--brief"], &[]);
    assert!(
        brief.status.success(),
        "status --brief failed: {}",
        String::from_utf8_lossy(&brief.stderr)
    );

    let text = String::from_utf8_lossy(&brief.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 20, "{text}");
    assert!(
        !brief.stdout.contains(&0x1b),
        "brief output must not contain ANSI: {text}"
    );
    for id in &ids {
        let hits = lines
            .iter()
            .filter(|line| line.contains(id.as_str()))
            .count();
        assert_eq!(
            hits, 1,
            "{id} should appear on exactly one brief line:\n{text}"
        );
    }
    assert!(
        brief.stdout.len().saturating_mul(20) <= piped.stdout.len(),
        "brief {} bytes / piped {} bytes exceeds 5%",
        brief.stdout.len(),
        piped.stdout.len()
    );
}

/// (2) Without `--brief`, piped and `--json` bytes stay the pre-change
/// fixture snapshot: pretty vs compact JSON and the same key order.
#[test]
fn status_without_brief_keeps_piped_and_json_bytes_of_the_fixture() {
    let (_temp, home, workspace) = fixture_home();
    let task = completed_task(&home, &workspace);

    let list_compact = run(&home, &["--json", "status"], &[]);
    let list: Value = serde_json::from_slice(&list_compact.stdout).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&list_compact.stdout),
        compact_json_bytes(&list)
    );
    let list_piped = run(&home, &["status"], &[]);
    assert!(list_piped.status.success());
    assert_eq!(String::from_utf8_lossy(&list_piped.stdout), pretty(&list));
    let item = &list.as_array().expect("status list is an array")[0];
    assert_eq!(object_keys(item), ["state", "task"], "{item}");

    let task_compact = run(&home, &["--json", "status", &task], &[]);
    let detail: Value = serde_json::from_slice(&task_compact.stdout).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&task_compact.stdout),
        compact_json_bytes(&detail)
    );
    let task_piped = run(&home, &["status", &task], &[]);
    assert!(task_piped.status.success());
    assert_eq!(String::from_utf8_lossy(&task_piped.stdout), pretty(&detail));
    assert_eq!(object_keys(&detail), STATUS_TASK_JSON_KEYS, "{detail}");
}

/// (3) `--brief --json` is a compact array whose object keys are this
/// set, in this order.
#[test]
fn brief_json_uses_the_fixed_key_set_and_order() {
    let (_temp, home, workspace) = fixture_home();
    completed_task(&home, &workspace);
    let output = run(&home, &["--json", "status", "--brief"], &[]);
    assert!(
        output.status.success(),
        "status --brief --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.ends_with('\n') && text.lines().count() == 1,
        "brief JSON must be one compact line: {text}"
    );
    assert!(
        !text.contains("  "),
        "brief JSON must not be pretty-printed: {text}"
    );
    let parsed: Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("not JSON: {text}"));
    let rows = parsed.as_array().expect("brief JSON is an array");
    assert_eq!(rows.len(), 1, "{parsed}");
    let keys: std::collections::BTreeSet<&str> = rows[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BRIEF_JSON_KEYS.iter().copied().collect(),
        "{}",
        rows[0]
    );
    assert_eq!(
        first_object_key_order(&text),
        BRIEF_JSON_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>(),
        "key order in compact bytes: {text}"
    );
    assert!(rows[0]["task_id"].as_str().unwrap().len() == 36);
    assert!(rows[0]["elapsed_seconds"].as_u64().is_some() || rows[0]["elapsed_seconds"].is_null());
    assert!(rows[0]["objective_head"].as_str().is_some());
}

/// Keys of the first JSON object, in the order they appear in `json`.
/// Brief objects are flat, so the first `{`…`}` is the row.
fn first_object_key_order(json: &str) -> Vec<String> {
    let start = json.find('{').expect("object");
    let end = start + json[start..].find('}').expect("object end");
    let object = &json[start..=end];
    let mut keys = Vec::new();
    let mut at = 0;
    while let Some(quote) = object[at..].find('"') {
        let from = at + quote + 1;
        let Some(close) = object[from..].find('"') else {
            break;
        };
        let key = &object[from..from + close];
        let after = from + close + 1;
        if object[after..].trim_start().starts_with(':') {
            keys.push(key.to_owned());
        }
        at = after;
    }
    keys
}

/// (4) The objective head is `safe_text`: control, bidi and zero-width
/// characters become spaces, and the row stays one line.
#[test]
fn brief_objective_head_strips_control_bidi_and_zero_width_and_stays_one_line() {
    let (_temp, home, workspace) = fixture_home();
    let objective = "BRGR_FIXTURE_OK \n\u{1b}[31m\u{202e}\u{200b}hidden";
    let receipt = json_output(&run(
        &home,
        &[
            "--json",
            "run",
            objective,
            "--foreground",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[],
    ));
    let task = receipt["task_id"].as_str().unwrap();
    let brief = run(&home, &["status", "--brief"], &[]);
    assert!(
        brief.status.success(),
        "status --brief failed: {}",
        String::from_utf8_lossy(&brief.stderr)
    );
    let text = String::from_utf8_lossy(&brief.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!text.contains('\u{202e}'), "{text:?}");
    assert!(!text.contains('\u{200b}'), "{text:?}");
    assert!(!lines[0].contains('\n'), "{text:?}");
    assert!(lines[0].contains(task), "{text}");
    assert!(lines[0].contains("hidden"), "{text}");

    let json_out = run(&home, &["--json", "status", "--brief"], &[]);
    let parsed: Value = serde_json::from_slice(&json_out.stdout).unwrap();
    let head = parsed[0]["objective_head"].as_str().unwrap();
    assert!(!head.contains('\u{1b}'));
    assert!(!head.contains('\u{202e}'));
    assert!(!head.contains('\u{200b}'));
    assert!(!head.contains('\n'));
    assert!(head.contains("hidden"), "{head}");
}

/// (5) `status TASK --brief` is a few lines: state, last event, whether a
/// result exists, whether a decision exists.
#[test]
fn status_task_brief_shows_state_last_event_result_and_decision() {
    let (_temp, home, workspace) = fixture_home();
    let task = completed_task(&home, &workspace);
    let brief = run(&home, &["status", &task, "--brief"], &[]);
    assert!(
        brief.status.success(),
        "status TASK --brief failed: {}",
        String::from_utf8_lossy(&brief.stderr)
    );
    let text = String::from_utf8_lossy(&brief.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.len() <= 6,
        "a few lines, got {}:\n{text}",
        lines.len()
    );
    assert!(text.contains("state:"), "{text}");
    assert!(text.contains("last_event:"), "{text}");
    assert!(text.contains("result: yes"), "{text}");
    assert!(text.contains("decision: no"), "{text}");
    assert!(!brief.stdout.contains(&0x1b), "{text}");

    json_output(&run(
        &home,
        &["--json", "accept", &task, "--reason", "fixture verified"],
        &[],
    ));
    let decided = run(&home, &["status", &task, "--brief"], &[]);
    let decided_text = String::from_utf8_lossy(&decided.stdout);
    assert!(decided_text.contains("decision: yes"), "{decided_text}");
}

#[test]
fn status_help_teaches_brief() {
    let temp = TempDir::new().unwrap();
    let help = run(temp.path(), &["status", "--help"], &[]);
    assert!(
        help.status.success(),
        "status --help failed: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    let text = String::from_utf8_lossy(&help.stdout);
    let first = text.split("Usage:").next().unwrap_or(&text);
    assert!(
        first.contains("brgr status --brief"),
        "first paragraph of status --help must teach brgr status --brief:\n{text}"
    );
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
