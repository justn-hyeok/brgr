//! `brgr head` request, prepare, claim, show, and status.

use std::{fs, path::Path, process::Command};

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

fn other_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-other"),
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

fn result_id(home: &Path, task_id: &str) -> String {
    let shown = json_output(&run(home, &["result", task_id], &head_env()));
    shown["result"]["result_id"].as_str().unwrap().to_owned()
}

fn takeover_count(home: &Path) -> i64 {
    let database = home.join("store").join("brgr.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row("SELECT COUNT(*) FROM head_takeovers", [], |row| row.get(0))
        .unwrap()
}

fn manifest_count(home: &Path) -> i64 {
    let database = home.join("store").join("brgr.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row("SELECT COUNT(*) FROM handoff_manifests", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn takeover_state(home: &Path, takeover_id: &str) -> String {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    store
        .takeover(takeover_id.parse().unwrap())
        .unwrap()
        .unwrap()
        .state
        .as_str()
        .to_owned()
}

fn takeover_epoch(home: &Path, takeover_id: &str) -> u64 {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    store
        .takeover(takeover_id.parse().unwrap())
        .unwrap()
        .unwrap()
        .committed_epoch()
}

fn incoming_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-incoming"),
    ]
}

fn unrelated_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-unrelated"),
    ]
}

fn request_and_prepare(home: &Path, task_id: &str, key: &str) -> String {
    let requested = json_output(&run(
        home,
        &[
            "head",
            "request",
            task_id,
            "--to",
            "session-incoming",
            "--key",
            key,
        ],
        &head_env(),
    ));
    let takeover_id = requested["takeover_id"].as_str().unwrap().to_owned();
    json_output(&run(
        home,
        &[
            "head",
            "prepare",
            &takeover_id,
            "--goal",
            "Finish the handoff",
        ],
        &head_env(),
    ));
    takeover_id
}

#[test]
fn head_takeover_resumes_retires_and_refuses_the_stale_outgoing_session() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "takeover-e2e");
    let shown = json_output(&run(
        &home,
        &["head", "show", &takeover_id],
        &incoming_env(),
    ));
    assert_eq!(shown["takeover"]["state"], "prepared");
    let committed = json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    assert_eq!(committed["takeover"]["state"], "committed");
    assert!(committed["previous_session_stale"].as_bool().unwrap());
    assert_eq!(takeover_state(&home, &takeover_id), "committed");
    json_output(&run(
        &home,
        &["accept", &task_id, "--reason", "incoming head acted"],
        &incoming_env(),
    ));
    assert_eq!(takeover_state(&home, &takeover_id), "resumed");
    let retired = json_output(&run(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_env(),
    ));
    assert_eq!(retired["state"], "retired");
    let stale = run(&home, &["result", &task_id, "--ack"], &head_env());
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("bound to another session"));
}

#[test]
fn head_commit_abort_and_retire_refuse_wrong_callers_without_writes() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "wrong-callers");
    let outgoing_commit = run(&home, &["head", "commit", &takeover_id], &head_env());
    assert!(!outgoing_commit.status.success());
    assert!(
        String::from_utf8_lossy(&outgoing_commit.stderr)
            .contains("only the incoming session may commit")
    );
    assert_eq!(takeover_state(&home, &takeover_id), "prepared");
    let outgoing_retire = run(&home, &["head", "retire", &takeover_id], &head_env());
    assert!(!outgoing_retire.status.success());
    assert_eq!(takeover_state(&home, &takeover_id), "prepared");
    let unrelated_abort = run(
        &home,
        &["head", "abort", &takeover_id, "--reason", "not my handoff"],
        &unrelated_env(),
    );
    assert!(!unrelated_abort.status.success());
    assert_eq!(takeover_state(&home, &takeover_id), "prepared");
}

#[test]
fn head_mutations_refuse_a_mismatched_owner_id() {
    let wrong_owner_incoming = [
        ("BRGR_OWNER_ID", "codex:some-other-owner"),
        ("BRGR_SESSION_ID", "session-incoming"),
    ];
    for action in ["commit", "abort", "retire"] {
        let (_temp, home, workspace) = gjc_home();
        let task_id = start_task(&home, &workspace);
        let takeover_id = request_and_prepare(&home, &task_id, &format!("owner-mismatch-{action}"));
        if action == "retire" {
            json_output(&run(
                &home,
                &["head", "commit", &takeover_id],
                &incoming_env(),
            ));
            json_output(&run(
                &home,
                &["accept", &task_id, "--reason", "resume for retirement"],
                &incoming_env(),
            ));
        }
        let args = match action {
            "commit" => vec!["head", "commit", &takeover_id],
            "abort" => vec!["head", "abort", &takeover_id, "--reason", "wrong owner"],
            _ => vec!["head", "retire", &takeover_id],
        };
        let output = run(&home, &args, &wrong_owner_incoming);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("BRGR_OWNER_ID differs"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            takeover_state(&home, &takeover_id),
            if action == "retire" {
                "resumed"
            } else {
                "prepared"
            }
        );
    }
}

#[test]
fn head_commit_is_idempotent_abort_after_commit_is_refused_and_retirement_requires_resumed() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "transitions");
    let first = json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    let epoch = takeover_epoch(&home, &takeover_id);
    let second = json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    assert_eq!(
        first["takeover"]["takeover_id"],
        second["takeover"]["takeover_id"]
    );
    assert_eq!(takeover_epoch(&home, &takeover_id), epoch);
    let abort = run(
        &home,
        &["head", "abort", &takeover_id, "--reason", "too late"],
        &incoming_env(),
    );
    assert!(!abort.status.success());
    assert_eq!(takeover_state(&home, &takeover_id), "committed");
    let retire = run(&home, &["head", "retire", &takeover_id], &incoming_env());
    assert!(!retire.status.success());
    assert_eq!(takeover_state(&home, &takeover_id), "committed");
    let message = String::from_utf8_lossy(&retire.stderr);
    assert!(
        message.contains("cannot go from committed to retired") && !message.contains("TakeoverId("),
        "the refusal should name the states and print the id plainly: {message}"
    );
    json_output(&run(
        &home,
        &["accept", &task_id, "--reason", "resumed"],
        &incoming_env(),
    ));
    let failed = json_output(&run(
        &home,
        &["head", "retire", &takeover_id, "--failed", "cleanup failed"],
        &incoming_env(),
    ));
    assert_eq!(failed["state"], "retire_failed");
}

#[test]
fn head_request_replay_after_commit_returns_the_existing_record() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "post-commit-replay");
    json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    let replay = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "post-commit-replay",
        ],
        &head_env(),
    ));
    assert_eq!(replay["takeover_id"], takeover_id);
    assert_eq!(replay["state"], "committed");
    assert_eq!(takeover_count(&home), 1);
}

#[test]
fn bind_during_open_takeover_is_refused_without_changing_state() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "binding-conflict");
    let third_env = [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-third"),
    ];
    let owner_id = brgr_protocol::OwnerId::new("codex:head-takeover").unwrap();
    let before = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .owner_binding(&owner_id)
        .unwrap();
    let refused = run(&home, &["bind", &task_id], &third_env);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(error.contains(&takeover_id), "{error}");
    assert!(error.contains("prepared"), "{error}");
    assert!(error.contains("finish or abort"), "{error}");
    let after = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .owner_binding(&owner_id)
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(after.unwrap().0, "session-head");
    assert_eq!(takeover_state(&home, &takeover_id), "prepared");
    let committed = run(&home, &["head", "commit", &takeover_id], &incoming_env());
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(takeover_state(&home, &takeover_id), "committed");
}

#[test]
fn bind_is_allowed_again_after_takeover_is_terminal() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "binding-after-abort");
    json_output(&run(
        &home,
        &["head", "abort", &takeover_id, "--reason", "cancelled"],
        &head_env(),
    ));
    let third_env = [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-third"),
    ];
    json_output(&run(&home, &["bind", &task_id], &third_env));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert_eq!(
        store
            .owner_binding(&brgr_protocol::OwnerId::new("codex:head-takeover").unwrap())
            .unwrap()
            .unwrap()
            .0,
        "session-third"
    );
}

#[test]
fn head_commit_alone_never_marks_takeover_resumed() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "commit-is-not-resume");
    json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    assert_eq!(takeover_state(&home, &takeover_id), "committed");
}

#[test]
fn head_request_by_the_current_head_returns_the_same_record_for_the_same_key() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let first = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-1",
        ],
        &head_env(),
    ));
    let replay = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-1",
        ],
        &head_env(),
    ));
    assert_eq!(replay["takeover_id"], first["takeover_id"]);
    assert_eq!(replay["from_session"], "session-head");
    assert_eq!(replay["to_session"], "session-incoming");
    assert_eq!(replay["state"], "requested");
    assert_eq!(replay["idempotency_key"], "handoff-1");
    assert_eq!(takeover_count(&home), 1);
}

#[test]
fn head_request_with_the_same_key_and_another_destination_is_a_conflict() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let first = run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-1",
        ],
        &head_env(),
    );
    assert!(first.status.success());
    let retarget = run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-elsewhere",
            "--key",
            "handoff-1",
        ],
        &head_env(),
    );
    assert!(
        !retarget.status.success(),
        "a replayed key for a different destination was answered with the old takeover"
    );
    assert_eq!(takeover_count(&home), 1);
}

#[test]
fn head_request_by_a_non_head_is_refused_and_writes_nothing() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let refused = run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-other",
        ],
        &other_env(),
    );
    assert!(!refused.status.success(), "a non-head request was accepted");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("bound to another session"),
        "refusal did not name the head: {stderr}"
    );
    assert_eq!(
        takeover_count(&home),
        0,
        "a refused request wrote a takeover"
    );
}

#[test]
fn head_prepare_builds_the_manifest_from_the_store() {
    let (_temp, home, workspace) = gjc_home();
    let undecided = start_task(&home, &workspace);
    let decided = start_task(&home, &workspace);
    let undecided_result = result_id(&home, &undecided);
    let decided_result = result_id(&home, &decided);
    json_output(&run(
        &home,
        &["accept", &decided, "--reason", "fixture verified"],
        &head_env(),
    ));
    let requested = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &undecided,
            "--to",
            "session-incoming",
            "--key",
            "handoff-prepare",
        ],
        &head_env(),
    ));
    let takeover_id = requested["takeover_id"].as_str().unwrap();
    let prepared = json_output(&run(
        &home,
        &[
            "head",
            "prepare",
            takeover_id,
            "--goal",
            "Finish the answer",
            "--notes",
            "One result is still open.",
        ],
        &head_env(),
    ));
    assert_eq!(prepared["state"], "prepared");
    let pending = prepared["manifest"]["pending_result_ids"]
        .as_array()
        .unwrap();
    assert!(
        pending.iter().any(|id| id == &json!(undecided_result)),
        "the undecided result was left out: {prepared}"
    );
    assert!(
        pending.iter().all(|id| id != &json!(decided_result)),
        "a decided result was copied into the manifest: {prepared}"
    );
    let tasks = prepared["manifest"]["tasks"].as_array().unwrap();
    assert!(
        tasks
            .iter()
            .any(|task| task["task_id"].as_str() == Some(undecided.as_str())),
        "the open task was left out: {prepared}"
    );
    assert!(
        tasks
            .iter()
            .all(|task| task["task_id"].as_str() != Some(decided.as_str())),
        "a decided task was named: {prepared}"
    );
    assert_eq!(prepared["manifest"]["origin_session"], "session-head");
    assert_eq!(prepared["manifest"]["goal"], "Finish the answer");
}

#[test]
fn head_prepare_by_anyone_but_the_outgoing_session_is_refused() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let requested = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-prepare-refused",
        ],
        &head_env(),
    ));
    let takeover_id = requested["takeover_id"].as_str().unwrap();
    let refused = run(
        &home,
        &[
            "head",
            "prepare",
            takeover_id,
            "--goal",
            "Finish the answer",
        ],
        &other_env(),
    );
    assert!(!refused.status.success(), "another session prepared");
    assert_eq!(
        manifest_count(&home),
        0,
        "a refused prepare stored a manifest"
    );
    let shown = json_output(&run(&home, &["head", "show", takeover_id], &head_env()));
    assert_eq!(shown["takeover"]["state"], "requested");
}

#[test]
fn head_show_prints_the_record_events_and_verified_manifest_and_rejects_a_tampered_one() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let requested = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-show",
        ],
        &head_env(),
    ));
    let takeover_id = requested["takeover_id"].as_str().unwrap();
    json_output(&run(
        &home,
        &[
            "head",
            "prepare",
            takeover_id,
            "--goal",
            "Finish the answer",
        ],
        &head_env(),
    ));
    let shown = run(&home, &["head", "show", takeover_id], &head_env());
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let stderr = String::from_utf8_lossy(&shown.stderr);
    assert!(stderr.contains("read only"), "{stderr}");
    let value = json_output(&shown);
    assert_eq!(value["takeover"]["takeover_id"], takeover_id);
    assert_eq!(value["takeover"]["state"], "prepared");
    let events = value["events"].as_array().unwrap();
    assert!(events.iter().any(|event| event["kind"] == "requested"));
    assert!(events.iter().any(|event| event["kind"] == "prepared"));
    assert_eq!(value["manifest"]["manifest"]["goal"], "Finish the answer");
    assert!(value["manifest"]["digest"].as_str().unwrap().len() > 16);

    let database = home.join("store").join("brgr.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let changed = connection
        .execute(
            "UPDATE handoff_manifests SET manifest_json = replace(manifest_json, 'Finish', 'Tamper')",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    let tampered = run(&home, &["head", "show", takeover_id], &head_env());
    assert!(!tampered.status.success(), "a tampered manifest was shown");
    let error = String::from_utf8_lossy(&tampered.stderr);
    assert!(
        error.contains("does not match its recorded digest"),
        "{error}"
    );
}

#[test]
fn head_status_lists_the_owners_takeovers_newest_first() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let first = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-old",
        ],
        &head_env(),
    ));
    let first_id = first["takeover_id"].as_str().unwrap().to_owned();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let takeover_id = first_id.parse().unwrap();
    store
        .abort_takeover(takeover_id, "make room for a later one")
        .unwrap();
    drop(store);
    let second = json_output(&run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "handoff-new",
        ],
        &head_env(),
    ));
    let second_id = second["takeover_id"].as_str().unwrap();
    let status = json_output(&run(&home, &["head", "status", &task_id], &head_env()));
    let listed = status["takeovers"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["takeover_id"], second_id);
    assert_eq!(listed[1]["takeover_id"], first_id);
    assert_eq!(listed[0]["state"], "requested");
    assert_eq!(listed[1]["state"], "aborted");
    assert_eq!(listed[0]["from_session"], "session-head");
    assert_eq!(listed[0]["to_session"], "session-incoming");
    assert!(listed[0]["from_epoch"].as_u64().is_some());
    assert_eq!(listed[0]["committed_epoch"], Value::Null);
    assert!(listed[0].get("epoch").is_none());
    assert!(listed[0]["updated_at"].as_i64().is_some());
}

#[test]
fn head_status_reports_the_epoch_after_commit() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let takeover_id = request_and_prepare(&home, &task_id, "status-committed-epoch");
    let from_epoch = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .takeover(takeover_id.parse().unwrap())
        .unwrap()
        .unwrap()
        .from_epoch;
    json_output(&run(
        &home,
        &["head", "commit", &takeover_id],
        &incoming_env(),
    ));
    let status = json_output(&run(&home, &["head", "status", &task_id], &incoming_env()));
    let takeover = &status["takeovers"][0];
    assert_eq!(takeover["from_epoch"], from_epoch);
    assert_eq!(takeover["committed_epoch"], from_epoch + 1);
    assert!(takeover.get("epoch").is_none());
}

/// Incoming session takes an abandoned head without the outgoing session
/// typing `request`. A different `BRGR_OWNER_ID` still works: the control home
/// is the access check.
#[test]
fn an_abandoned_head_is_claimed_by_a_foreign_session() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let refused = run(
        &home,
        &["head", "claim", &task_id, "--goal", "keep going"],
        &incoming_env(),
    );
    assert!(
        !refused.status.success(),
        "claim without --abandoned was accepted: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let same = run(
        &home,
        &[
            "head",
            "claim",
            &task_id,
            "--abandoned",
            "--goal",
            "keep going",
        ],
        &head_env(),
    );
    assert!(
        !same.status.success(),
        "the current head claimed itself as abandoned: {}",
        String::from_utf8_lossy(&same.stderr)
    );
    let foreign = [
        ("BRGR_OWNER_ID", "codex:someone-else"),
        ("BRGR_SESSION_ID", "session-incoming"),
    ];
    let claimed = json_output(&run(
        &home,
        &[
            "head",
            "claim",
            &task_id,
            "--abandoned",
            "--goal",
            "keep going",
            "--key",
            "claim-1",
        ],
        &foreign,
    ));
    assert_eq!(claimed["takeover"]["state"], "committed", "{claimed}");
    assert_eq!(claimed["previous_session_stale"], true);
    let replay = json_output(&run(
        &home,
        &[
            "head",
            "claim",
            &task_id,
            "--abandoned",
            "--goal",
            "keep going",
            "--key",
            "claim-1",
        ],
        &foreign,
    ));
    assert_eq!(
        replay["takeover"]["takeover_id"], claimed["takeover"]["takeover_id"],
        "{replay}"
    );
    let stale = run(
        &home,
        &["reject", &task_id, "--reason", "stale"],
        &head_env(),
    );
    assert!(
        !stale.status.success(),
        "the abandoned session still decided: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    json_output(&run(
        &home,
        &["reject", &task_id, "--reason", "claimed"],
        &foreign,
    ));
}

/// A Cursor pane's `CURSOR_CONVERSATION_ID` is the session. `--as` is not
/// required.
#[test]
fn a_cursor_conversation_claims_an_abandoned_head_without_as() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let cursor = [("CURSOR_CONVERSATION_ID", "cursor-conversation-1")];
    let claimed = json_output(&run(
        &home,
        &[
            "head",
            "claim",
            &task_id,
            "--abandoned",
            "--goal",
            "keep going",
        ],
        &cursor,
    ));
    assert_eq!(claimed["takeover"]["state"], "committed", "{claimed}");
    json_output(&run(
        &home,
        &["reject", &task_id, "--reason", "claimed from cursor"],
        &cursor,
    ));
}
