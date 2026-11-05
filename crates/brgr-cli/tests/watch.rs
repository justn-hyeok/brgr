//! `brgr watch` streams read-only task events, one compact JSON line each.

mod common;

use common::{brgr_command, process_exists, wait_until, write_fast_script};

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

fn isolated_brgr(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Command {
    let mut command = brgr_command();
    command.arg("--home").arg(home).arg("--json").args(args);
    if matches!(args.first(), Some(&"run" | &"revise")) {
        command.arg("--headless");
    }
    if !envs
        .iter()
        .any(|(name, _)| *name == "CLAUDE_CODE_SESSION_ID")
    {
        command.env("BRGR_SESSION_ID", "fixture-session");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    isolated_brgr(home, args, envs).output().unwrap()
}

fn json_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn add_fixture(home: &Path, fixture: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        &[
            "harness",
            "add",
            fixture.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    ));
    assert!(receipt["scratch_result_digest"].as_str().is_some());
}

/// Prints the fixture success protocol. `PAUSE` holds until the test writes
/// `release-pause` in the control home (see [`release_pause`]), so a watch can
/// observe the task while it is still running however slow the machine; `HOLD`
/// stays up through a short watch timeout.
const WATCH_FIXTURE: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'fixture v-fixture\n'; exit 0;;
  --help)
    printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'
    exit 0;;
esac
prompt_file=''
for argument in "$@"; do
  case "$argument" in
    @*) prompt_file=${argument#@} ;;
  esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q 'HOLD' "$prompt_file"; then
  /bin/sleep 30
elif [ -n "$prompt_file" ] && /usr/bin/grep -q 'PAUSE' "$prompt_file"; then
  n=0
  while [ -d "$BRGR_HOME" ] && [ ! -e "$BRGR_HOME/release-pause" ] && [ "$n" -lt 6000 ]; do
    /bin/sleep 0.1
    n=$((n + 1))
  done
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn watch_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("fixture");
    write_fast_script(&executable, WATCH_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    (temp, home, workspace)
}

#[test]
fn pause_worker_and_supervisor_exit_when_the_test_home_is_removed() {
    let (temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "PAUSE", false);
    let run_dir = home.join("runs");
    let worker_pid_path = run_dir.join(format!("{task}.pid"));
    assert!(
        wait_until(|| fs::metadata(&worker_pid_path).is_ok_and(|metadata| metadata.len() > 0)),
        "PAUSE worker did not start"
    );
    let worker_pid = fs::read_to_string(worker_pid_path)
        .unwrap()
        .trim()
        .to_owned();
    let supervisor: Value =
        serde_json::from_slice(&fs::read(run_dir.join(format!("{task}.supervisor.json"))).unwrap())
            .unwrap();
    let supervisor_pid = supervisor["identity"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::remove_dir_all(temp.path()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_exists(&worker_pid) && !process_exists(&supervisor_pid) {
            assert!(
                !home.exists(),
                "supervisor exit path recreated the removed home {}",
                home.display()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{worker_pid}")])
        .status();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &supervisor_pid])
        .status();
    panic!("PAUSE worker or supervisor stayed alive after its test home was removed");
}

fn start_task(home: &Path, workspace: &Path, objective: &str, foreground: bool) -> String {
    let mut args = vec!["run", objective, "--workspace", workspace.to_str().unwrap()];
    if foreground {
        args.push("--foreground");
    }
    json_output(&run(home, &args, &[("BRGR_OWNER_ID", "codex:watch-test")]))["task_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn wait_until_running(home: &Path, task: &str) {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let task_id = task.parse().unwrap();
    assert!(
        wait_until(|| matches!(
            store.attempt_state(task_id),
            Ok(brgr_protocol::AttemptState::Running)
        )),
        "task {task} did not start running"
    );
}

/// Lets every `PAUSE` worker in `home` finish.
fn release_pause(home: &Path) {
    fs::write(home.join("release-pause"), b"1\n").unwrap();
}

/// A watch whose stdout the test reads line by line, so it can act once the
/// watch has printed an event instead of guessing when it has.
fn spawn_watch(
    home: &Path,
    args: &[&str],
) -> (std::process::Child, BufReader<std::process::ChildStdout>) {
    let mut child = isolated_brgr(home, args, &[("BRGR_OWNER_ID", "codex:watch-test")]);
    child.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = child.spawn().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdout)
}

/// Reads event lines until one satisfies `wanted`, returning every line read.
/// A watch that never prints such a line ends on its own `--timeout-seconds`,
/// which closes stdout and fails the read.
fn read_until(
    stdout: &mut BufReader<std::process::ChildStdout>,
    wanted: impl Fn(&Value) -> bool,
) -> String {
    let mut read = String::new();
    loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the watch ended before the awaited event: {read}"
        );
        read.push_str(&line);
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|_| panic!("not one JSON object: {line}"));
        if wanted(&event) {
            return read;
        }
    }
}

/// Waits for the watch to exit and returns its full stdout: the lines already
/// read followed by the rest.
fn finish_watch(
    mut child: std::process::Child,
    mut stdout: BufReader<std::process::ChildStdout>,
    read: String,
) -> (std::process::ExitStatus, String, String) {
    let status = child.wait().unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    let mut rendered = read;
    rendered.push_str(&rest);
    (
        status,
        rendered,
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn event_lines(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8(stdout.to_vec()).unwrap();
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|_| panic!("not one JSON object: {line}"))
        })
        .collect()
}

fn event_names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect()
}

/// Decisions, messages, and acknowledged inbox rows. Watching must leave them
/// unchanged.
fn store_counts(home: &Path) -> (i64, i64, i64) {
    let connection = rusqlite::Connection::open(home.join("store").join("brgr.sqlite3")).unwrap();
    let count = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
    (
        count("SELECT COUNT(*) FROM decisions"),
        count("SELECT COUNT(*) FROM task_messages"),
        count("SELECT COUNT(*) FROM inbox_items WHERE acknowledged = 1"),
    )
}

#[test]
fn watch_prints_state_then_result_then_done_once_each() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "PAUSE", false);
    wait_until_running(&home, &task);
    // The worker is held until the watch has printed its state, so the state
    // event is observed while the task runs on any machine.
    let (child, mut stdout) =
        spawn_watch(&home, &["watch", &task, &task, "--timeout-seconds", "60"]);
    let read = read_until(&mut stdout, |event| event["event"] == "state");
    release_pause(&home);
    let (status, rendered, stderr) = finish_watch(child, stdout, read);
    assert!(status.success(), "{stderr}");
    let events = event_lines(rendered.as_bytes());
    assert_eq!(event_names(&events), ["state", "result", "done"]);
    assert_eq!(events[0]["task_id"], task);
    assert_ne!(events[0]["state"], "terminal");
    assert_eq!(events[1]["task_id"], task);
    assert_eq!(events[1]["outcome"], "candidate");
    assert!(events[1]["revision"].as_u64().unwrap() >= 1);
    assert_eq!(events[2], serde_json::json!({"event": "done"}));
}

#[test]
fn watch_prints_an_existing_result_and_exits() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    // An explicit timeout turns "kept running after the result existed" into a
    // `timeout` event and a failed exit, so no wall-clock bound is needed.
    let output = run(
        &home,
        &["watch", &task, "--timeout-seconds", "60"],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(
        output.status.success(),
        "watch kept running after the task already had a result, or failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = event_lines(&output.stdout);
    assert!(events.iter().any(|event| {
        event["event"] == "result" && event["task_id"] == task && event["outcome"] == "candidate"
    }));
    assert_eq!(events.last().unwrap()["event"], "done");
}

#[test]
fn watch_emits_done_only_after_every_task_has_a_result() {
    let (_temp, home, workspace) = watch_home();
    let finished = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    let running = start_task(&home, &workspace, "PAUSE", false);
    wait_until_running(&home, &running);
    // The running task is held until the watch has printed the finished one's
    // result, so `done` can only follow a result that arrived later.
    let (child, mut stdout) = spawn_watch(
        &home,
        &[
            "watch",
            &finished,
            &running,
            &finished,
            "--timeout-seconds",
            "60",
        ],
    );
    let read = read_until(&mut stdout, |event| {
        event["event"] == "result" && event["task_id"] == finished
    });
    release_pause(&home);
    let (status, rendered, stderr) = finish_watch(child, stdout, read);
    assert!(status.success(), "{stderr}");
    let events = event_lines(rendered.as_bytes());
    let names = event_names(&events);
    let done_at = names.iter().position(|name| *name == "done").unwrap();
    assert_eq!(done_at, names.len() - 1);
    assert_eq!(names.iter().filter(|name| **name == "done").count(), 1);
    let results: Vec<&str> = events
        .iter()
        .filter(|event| event["event"] == "result")
        .map(|event| event["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|id| **id == finished).count(), 1);
    assert_eq!(results.iter().filter(|id| **id == running).count(), 1);
    let result_positions: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == "result")
        .map(|(index, _)| index)
        .collect();
    assert!(result_positions.iter().all(|index| *index < done_at));
    assert_eq!(result_positions.len(), 2);
}

#[test]
fn watch_prints_one_question_event_and_does_not_repeat_it_after_the_answer() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "PAUSE", false);
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let task_id = task.parse().unwrap();
    let mut attempt = None;
    assert!(
        wait_until(|| {
            attempt = store.active_message_attempt(task_id).ok();
            attempt.is_some()
        }),
        "worker attempt did not become active"
    );
    let attempt = attempt.unwrap();
    let attempt_text = attempt.to_string();
    let worker_owner = format!("worker:{attempt_text}");
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", task.as_str()),
        ("BRGR_PARENT_ATTEMPT_ID", attempt_text.as_str()),
    ];
    let (child, mut stdout) =
        spawn_watch(&home, &["watch", task.as_str(), "--timeout-seconds", "60"]);
    let question = json_output(&run(
        &home,
        &[
            "message",
            "send",
            &task,
            "--to",
            "owner",
            "--kind",
            "question",
            "--body",
            "Which token?",
        ],
        &worker,
    ));
    let question_id = question["message_id"].as_str().unwrap().to_owned();
    // Answer only once the watch has printed the question: what is checked is
    // that the answer does not make it print the question again.
    let read = read_until(&mut stdout, |event| event["event"] == "question");
    json_output(&run(
        &home,
        &[
            "message",
            "send",
            &task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            &question_id,
            "--body",
            "TOKEN_OK",
        ],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    ));
    release_pause(&home);
    let (status, rendered, stderr) = finish_watch(child, stdout, read);
    assert!(status.success(), "{stderr}");
    let events = event_lines(rendered.as_bytes());
    let questions: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "question")
        .collect();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["task_id"], task);
    assert_eq!(questions[0]["message_id"], question_id);
    assert!(!rendered.contains("Which token?"));
    assert!(!rendered.contains("TOKEN_OK"));
    assert_eq!(events.last().unwrap()["event"], "done");
}

#[test]
fn watch_timeout_prints_timeout_and_exits_nonzero() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "HOLD", false);
    let started = Instant::now();
    let output = run(
        &home,
        &["watch", &task, "--timeout-seconds", "1"],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(
        started.elapsed() < Duration::from_mins(1),
        "watch did not honor --timeout-seconds"
    );
    assert!(!output.status.success());
    let events = event_lines(&output.stdout);
    assert!(events.iter().any(|event| event["event"] == "timeout"));
    assert!(!String::from_utf8(output.stdout).unwrap().contains("HOLD"));
}

#[test]
fn watch_refuses_another_owners_task_without_printing_events() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    let output = run(
        &home,
        &["watch", &task],
        &[
            ("BRGR_OWNER_ID", "codex:someone-else"),
            ("BRGR_SESSION_ID", "other-session"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "stdout was {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("BRGR_OWNER_ID differs"));
}

#[test]
fn watch_does_not_write_decisions_messages_or_inbox_acknowledgements() {
    let (_temp, home, workspace) = watch_home();
    let finished = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    let before_finished = store_counts(&home);
    let watched = run(
        &home,
        &["watch", &finished],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(
        watched.status.success(),
        "{}",
        String::from_utf8_lossy(&watched.stderr)
    );
    assert_eq!(store_counts(&home), before_finished);

    let running = start_task(&home, &workspace, "HOLD", false);
    wait_until_running(&home, &running);
    let before_poll = store_counts(&home);
    let timed_out = run(
        &home,
        &["watch", &running, "--timeout-seconds", "1"],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(!timed_out.status.success());
    assert_eq!(store_counts(&home), before_poll);
}

#[test]
fn watch_all_includes_only_this_owners_unfinished_tasks() {
    let (_temp, home, workspace) = watch_home();
    let finished = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    let ours = start_task(&home, &workspace, "PAUSE", false);
    wait_until_running(&home, &ours);
    let theirs = json_output(&run(
        &home,
        &["run", "HOLD", "--workspace", workspace.to_str().unwrap()],
        &[
            ("BRGR_OWNER_ID", "codex:someone-else"),
            ("BRGR_SESSION_ID", "other-session"),
        ],
    ))["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_until_running(&home, &theirs);
    let (child, mut stdout) = spawn_watch(&home, &["watch", "--all", "--timeout-seconds", "60"]);
    let read = read_until(&mut stdout, |event| event["task_id"] == ours);
    release_pause(&home);
    let (status, rendered, stderr) = finish_watch(child, stdout, read);
    assert!(status.success(), "{stderr}");
    let events = event_lines(rendered.as_bytes());
    let results: Vec<&str> = events
        .iter()
        .filter(|event| event["event"] == "result")
        .map(|event| event["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(results, vec![ours.as_str()]);
    let mentioned: Vec<&str> = events
        .iter()
        .filter_map(|event| event["task_id"].as_str())
        .collect();
    assert!(!mentioned.contains(&finished.as_str()));
    assert!(!mentioned.contains(&theirs.as_str()));
    assert_eq!(events.last().unwrap()["event"], "done");
}

#[test]
fn watch_all_does_not_add_a_task_created_after_it_started() {
    let (_temp, home, workspace) = watch_home();
    let first = start_task(&home, &workspace, "PAUSE", false);
    wait_until_running(&home, &first);
    let (child, mut stdout) = spawn_watch(&home, &["watch", "--all", "--timeout-seconds", "60"]);
    let read = read_until(&mut stdout, |event| event["task_id"] == first);
    let later = start_task(&home, &workspace, "HOLD", false);
    wait_until_running(&home, &later);
    // The first task is held until the later one runs, so the watch is still
    // open to pick it up, and must not.
    release_pause(&home);
    let (status, rendered, stderr) = finish_watch(child, stdout, read);
    assert!(status.success(), "{stderr}");
    let events = event_lines(rendered.as_bytes());
    let mentioned: Vec<&str> = events
        .iter()
        .filter_map(|event| event["task_id"].as_str())
        .collect();
    assert!(mentioned.contains(&first.as_str()));
    assert!(
        !mentioned.contains(&later.as_str()),
        "watch --all picked up a task created after it started"
    );
    assert_eq!(events.last().unwrap()["event"], "done");
}

#[test]
fn watch_all_with_no_unfinished_tasks_exits_zero_with_a_message() {
    let (_temp, home, workspace) = watch_home();
    let finished = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    // With an explicit timeout, waiting would end as a `timeout` and a failed
    // exit; the message below is what an immediate exit says.
    let output = run(
        &home,
        &["watch", "--all", "--timeout-seconds", "60"],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(
        output.status.success(),
        "watch --all waited when this owner had no unfinished tasks, or failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no unfinished tasks for this owner to watch"),
        "stderr was {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(&finished),
        "empty --all still streamed a finished task: {stdout}"
    );
}

#[test]
fn watch_all_with_positional_ids_is_a_usage_error() {
    let (_temp, home, workspace) = watch_home();
    let task = start_task(&home, &workspace, "BRGR_FIXTURE_OK", true);
    let output = run(
        &home,
        &["watch", "--all", &task],
        &[("BRGR_OWNER_ID", "codex:watch-test")],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "stderr was {stderr}"
    );
}

#[test]
fn watch_help_contains_multi_id_and_all_examples() {
    let output = brgr_command().args(["watch", "--help"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("brgr watch A B C"),
        "help missing multi-id example: {help}"
    );
    assert!(
        help.contains("brgr watch --all"),
        "help missing --all example: {help}"
    );
}
