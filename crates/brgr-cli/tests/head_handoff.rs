//! `brgr head handoff`, `take` and the outgoing head's exit, against a fake Herdr.
mod common;

use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

use common::{calls, fake_herdr, herdr_env};

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

fn takeover_count(home: &Path) -> i64 {
    let database = home.join("store").join("brgr.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row("SELECT COUNT(*) FROM head_takeovers", [], |row| row.get(0))
        .unwrap()
}

fn takeover_epoch(home: &Path, takeover_id: &str) -> u64 {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    store
        .takeover(takeover_id.parse().unwrap())
        .unwrap()
        .unwrap()
        .committed_epoch()
}

fn unrelated_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-unrelated"),
    ]
}

fn run_with(
    home: &Path,
    args: &[&str],
    base: &[(&str, &str)],
    extra: &[(&str, String)],
) -> std::process::Output {
    let mut envs: Vec<(&str, &str)> = base.to_vec();
    envs.extend(extra.iter().map(|(name, value)| (*name, value.as_str())));
    run(home, args, &envs)
}

fn closed_panes(dir: &Path) -> Vec<String> {
    calls(dir)
        .lines()
        .filter_map(|line| line.strip_prefix("pane close "))
        .map(str::to_owned)
        .collect()
}

fn offer_key(handoff: &Value) -> String {
    handoff["offer"].as_str().unwrap().to_owned()
}

fn head_pane_env() -> [(&'static str, &'static str); 2] {
    head_env()
}

fn incoming_session() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-incoming"),
    ]
}

fn start_handoff(home: &Path, task_id: &str, fake: &Path, dir: &Path, exit_old: bool) -> Value {
    let mut args = vec![
        "head",
        "handoff",
        task_id,
        "--kind",
        "claude",
        "--goal",
        "Finish the migration",
    ];
    if exit_old {
        args.push("--exit-old");
    }
    json_output(&run_with(
        home,
        &args,
        &head_pane_env(),
        &herdr_env(fake, dir, "p-old"),
    ))
}

#[test]
fn handoff_opens_a_pane_prompts_it_and_leaves_the_binding_with_the_outgoing_head() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let handoff = start_handoff(&home, &task_id, &fake, &dir, false);
    assert_eq!(handoff["incoming_pane"], "p-new");
    let log = calls(&dir);
    assert!(log.contains("pane split"), "{log}");
    assert!(log.contains("BRGR_SESSION_ID="), "{log}");
    assert!(log.contains("--env BRGR_HOME="), "{log}");
    assert!(log.contains("agent start head-"), "{log}");
    let key = offer_key(&handoff);
    assert!(
        log.contains(&format!("head take {task_id} --offer {key}")),
        "{log}"
    );
    // Opening a pane moves nothing: the outgoing head still holds command.
    assert_eq!(takeover_count(&home), 0);
    let still_head = run(&home, &["result", &task_id], &head_env());
    assert!(still_head.status.success());
}

#[test]
fn take_commits_for_the_incoming_session_and_tells_the_outgoing_pane() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, false));
    let taken = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr_env(&fake, &dir, "p-new"),
    ));
    assert_eq!(taken["takeover"]["state"], "committed");
    assert_eq!(
        taken["binding_epoch"].as_u64().unwrap(),
        takeover_epoch(&home, taken["takeover"]["takeover_id"].as_str().unwrap())
    );
    assert!(
        calls(&dir).contains("agent prompt p-old"),
        "{}",
        calls(&dir)
    );
    // A repeat returns the same record and does not raise the epoch again.
    let again = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr_env(&fake, &dir, "p-new"),
    ));
    assert_eq!(
        again["takeover"]["takeover_id"],
        taken["takeover"]["takeover_id"]
    );
    assert_eq!(again["binding_epoch"], taken["binding_epoch"]);
    let stale = run(&home, &["result", &task_id, "--ack"], &head_env());
    assert!(!stale.status.success());
}

#[test]
fn take_refuses_the_outgoing_session_an_unknown_offer_and_a_moved_head() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, false));
    let herdr = herdr_env(&fake, &dir, "p-new");
    let own = run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &head_env(),
        &herdr,
    );
    assert!(!own.status.success());
    let unknown = run_with(
        &home,
        &["head", "take", &task_id, "--offer", "no-such-offer"],
        &incoming_session(),
        &herdr,
    );
    assert!(!unknown.status.success());
    let traversal = run_with(
        &home,
        &["head", "take", &task_id, "--offer", "../x"],
        &incoming_session(),
        &herdr,
    );
    assert!(!traversal.status.success());
    assert_eq!(takeover_count(&home), 0);
    // Another session claimed the head first; the old offer must not override it.
    json_output(&run(
        &home,
        &["head", "claim", &task_id, "--abandoned", "--goal", "g"],
        &unrelated_env(),
    ));
    let late = run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr,
    );
    assert!(!late.status.success());
    assert!(String::from_utf8_lossy(&late.stderr).contains("moved"));
}

#[test]
fn exit_old_closes_the_outgoing_pane_only_after_resume_and_a_clean_retire() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, true));
    let herdr = herdr_env(&fake, &dir, "p-new");
    let taken = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr,
    ));
    let takeover_id = taken["takeover"]["takeover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Committed is not resumed: retire is refused and the old pane stays open.
    let early = run_with(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_session(),
        &herdr,
    );
    assert!(!early.status.success());
    assert!(closed_panes(&dir).is_empty(), "{}", calls(&dir));
    json_output(&run_with(
        &home,
        &["accept", &task_id, "--reason", "incoming head acted"],
        &incoming_session(),
        &herdr,
    ));
    let retired = json_output(&run_with(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_session(),
        &herdr,
    ));
    assert_eq!(retired["state"], "retired");
    assert_eq!(retired["outgoing_head"]["closed"], true);
    assert_eq!(closed_panes(&dir), ["p-old"], "{}", calls(&dir));
}

#[test]
fn a_busy_outgoing_head_is_not_closed_and_the_failure_is_reported_apart_from_the_handoff() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, true));
    fs::write(dir.join("status-p-old"), "working").unwrap();
    let herdr = herdr_env(&fake, &dir, "p-new");
    let taken = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr,
    ));
    let takeover_id = taken["takeover"]["takeover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    json_output(&run_with(
        &home,
        &["accept", &task_id, "--reason", "incoming head acted"],
        &incoming_session(),
        &herdr,
    ));
    let retired = json_output(&run_with(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_session(),
        &herdr,
    ));
    assert_eq!(retired["state"], "retired");
    assert_eq!(retired["outgoing_head"]["closed"], false);
    assert!(closed_panes(&dir).is_empty(), "{}", calls(&dir));
    // Once the old head is idle, the retry closes it.
    fs::write(dir.join("status-p-old"), "idle").unwrap();
    let exited = json_output(&run_with(
        &home,
        &["head", "exit", &takeover_id],
        &incoming_session(),
        &herdr,
    ));
    assert_eq!(exited["outgoing_head"]["closed"], true);
    assert_eq!(closed_panes(&dir), ["p-old"], "{}", calls(&dir));
}

#[test]
fn a_handoff_without_exit_old_never_closes_a_pane() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, false));
    let herdr = herdr_env(&fake, &dir, "p-new");
    let taken = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr,
    ));
    let takeover_id = taken["takeover"]["takeover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    json_output(&run_with(
        &home,
        &["accept", &task_id, "--reason", "incoming head acted"],
        &incoming_session(),
        &herdr,
    ));
    let retired = json_output(&run_with(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_session(),
        &herdr,
    ));
    assert!(retired.get("outgoing_head").is_none());
    assert!(closed_panes(&dir).is_empty(), "{}", calls(&dir));
}

#[test]
fn the_incoming_prompt_states_the_owner_and_home_on_every_command() {
    // A shared Codex daemon runs tool calls with its own environment, which can
    // hold another worker's `BRGR_OWNER_ID` and not this control home. The pane
    // env cannot be relied on, so each command carries both.
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    start_handoff(&home, &task_id, &fake, &dir, false);
    let log = calls(&dir);
    let prefix = "env BRGR_OWNER_ID='codex:head-takeover' BRGR_HOME='";
    for command in ["head take", "status", "result", "head retire", "head exit"] {
        let at = log
            .find(&format!("{command} "))
            .or_else(|| log.find(&format!("{command}`")))
            .unwrap_or_else(|| panic!("no `{command}` in {log}"));
        assert!(
            log[..at]
                .trim_end()
                .rsplit('`')
                .next()
                .is_some_and(|run| run.contains(prefix)),
            "`{command}` lacks the owner and home prefix: {log}"
        );
    }
}

fn surface_pane(home: &Path, owner: &str) -> Option<(String, String)> {
    let database = home.join("store").join("brgr.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .query_row(
            "SELECT session_id, pane_id FROM owner_surfaces WHERE owner_id = ?1",
            [owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}

#[test]
fn take_records_the_pane_the_handoff_opened_as_the_new_head_s_notification_pane() {
    // A Codex head runs commands in its daemon's environment, which names
    // another pane, and its screen may not show its own call yet. The pane that
    // received the offer is the one brgr opened for it.
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, false));
    assert_eq!(surface_pane(&home, "codex:head-takeover"), None);
    let wrong = herdr_env(&fake, &dir, "p-someone-else");
    json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &wrong,
    ));
    assert_eq!(
        surface_pane(&home, "codex:head-takeover"),
        Some(("session-incoming".to_owned(), "p-new".to_owned()))
    );
}

#[test]
fn a_new_head_with_nothing_left_to_do_can_still_retire_the_handoff() {
    // The old head decided the only result before the new one took command, as
    // happens when the worker finishes while the incoming agent is starting.
    // There is then no owner-side write for the incoming head to make.
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let key = offer_key(&start_handoff(&home, &task_id, &fake, &dir, true));
    json_output(&run(
        &home,
        &["accept", &task_id, "--reason", "decided before the handoff"],
        &head_env(),
    ));
    let herdr = herdr_env(&fake, &dir, "p-new");
    let taken = json_output(&run_with(
        &home,
        &["head", "take", &task_id, "--offer", &key],
        &incoming_session(),
        &herdr,
    ));
    let takeover_id = taken["takeover"]["takeover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let retired = json_output(&run_with(
        &home,
        &["head", "retire", &takeover_id],
        &incoming_session(),
        &herdr,
    ));
    assert_eq!(retired["state"], "retired");
    assert_eq!(retired["outgoing_head"]["closed"], true);
    assert_eq!(closed_panes(&dir), ["p-old"], "{}", calls(&dir));
}
