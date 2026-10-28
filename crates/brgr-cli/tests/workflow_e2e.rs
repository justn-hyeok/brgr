//! Black-box proof that a workflow script creates a later task from an observed result.

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

const INHERITED: &[&str] = &[
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

const HERDR: &[&str] = &[
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_SESSION",
    "HERDR_PLUGIN_ID",
];

const CRITERION_SCRIPT: &str = r#"
import { existsSync, writeFileSync } from "node:fs";
import { agent, parallel } from "./brgr-workflow.mjs";

const [reviewA, reviewB] = await parallel([
  () => agent({ task: "BRGR_FIXTURE_OK review-a", mode: "headless", key: "review-a" }),
  () => agent({ task: "BRGR_FIXTURE_OK review-b", mode: "headless", key: "review-b" }),
]);
writeFileSync(process.env.BRGR_HOME + "/ab-sealed", "1");
const pause = process.env.BRGR_HOME + "/pause-before-c";
while (existsSync(pause)) {
  await new Promise((resolve) => setTimeout(resolve, 50));
}
if (reviewA.report.includes("BRGR_FIXTURE_OK")) {
  await agent({ task: "BRGR_FIXTURE_OK follow-up", mode: "headless", key: "follow-up" });
}
"#;

const TYPEERROR_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const cases = [
  { task: undefined },
  { task: "BRGR_FIXTURE_OK", extra: NaN },
  { task: "BRGR_FIXTURE_OK", stamp: new Date() },
];
for (const options of cases) {
  try {
    await agent(options);
    throw new Error("expected TypeError");
  } catch (error) {
    if (!(error instanceof TypeError)) {
      throw error;
    }
  }
}
writeFileSync("typeerror-ok", "1");
"#;

const FAILED_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";

const failed = await agent({ task: "FAIL", mode: "headless", key: "fail-step" });
if (failed.outcome !== "failed") {
  throw new Error("expected failed, got " + failed.outcome);
}
console.log("HANDLED_FAILED");
await agent({ task: "BRGR_FIXTURE_OK recovered", mode: "headless", key: "recovered" });
"#;

const QUESTION_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";
await agent({ task: "ASK which token", mode: "headless", key: "ask" });
"#;

const DIVERGED_SCRIPT: &str = r#"
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { agent } from "./brgr-workflow.mjs";

const objective = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "objective.txt"),
  "utf8",
).trim();
await agent({ task: objective, mode: "headless" });
await new Promise((resolve) => setTimeout(resolve, 60_000));
"#;

const REVISE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({ task: "BRGR_FIXTURE_OK first", mode: "headless", key: "revised" });
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const HOLD_REV2_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-hold-rev2\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q REV2 "$prompt_file"; then
  n=0
  while [ ! -e "$BRGR_HOME/release-rev2" ] && [ "$n" -lt 600 ]; do
    /bin/sleep 0.1
    n=$((n + 1))
  done
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"REV2_SEALED"}]}}'
else
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"REV1_SEALED"}]}}'
fi
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

const FAILING_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-fail-fixture\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q BRGR_FIXTURE_OK "$prompt_file"; then
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
  printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
  exit 0
fi
exit 7
"#;

const ASKING_GJC: &str = r#"#!/bin/sh
set -u
case "${1:-}" in
  --version) echo 'gjc v-asking-fixture'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}" && /usr/bin/grep -q ASK "$prompt_file"; then
  "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body 'Which token?' >/dev/null
  reply=$("$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds 60)
  reply_id=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"message_id":"\([^"]*\)".*/\1/p')
  "$BRGR_BIN" --json message ack "$BRGR_PARENT_TASK_ID" "$reply_id" --for worker >/dev/null
  answer=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"body":"\([^"]*\)".*/\1/p')
else
  answer=BRGR_FIXTURE_OK
fi
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' "$answer"
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

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
    for name in INHERITED {
        command.env_remove(name);
    }
    command
}

fn run(
    home: &Path,
    workspace: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> std::process::Output {
    let mut command = brgr_command();
    command
        .current_dir(workspace)
        .arg("--home")
        .arg(home)
        .arg("--json")
        .args(args);
    if matches!(args.first(), Some(&"run" | &"revise")) {
        command.arg("--headless");
    }
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove("CLAUDE_CODE_SESSION_ID");
    command.env_remove("CURSOR_CONVERSATION_ID");
    for name in HERDR {
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

fn add_harness(home: &Path, executable: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        scratch,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    ));
    assert!(receipt["scratch_result_digest"].as_str().is_some());
}

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    scripts: PathBuf,
}

fn fixture_from_gjc(executable: &Path) -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    let scripts = root.join("scripts");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    seed_git_repo(&workspace);
    add_harness(&home, executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
    }
}

fn default_gjc() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

fn fixture_home() -> Fixture {
    fixture_from_gjc(&default_gjc())
}

fn fixture_with_script(source: &str) -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    let scripts = root.join("scripts");
    let executable = root.join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    seed_git_repo(&workspace);
    fs::write(&executable, source).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_harness(&home, &executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
    }
}

fn owner_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ]
}

fn wait_until(timeout: Duration, ready: impl FnMut() -> bool) -> bool {
    wait_until_every(timeout, Duration::from_millis(100), ready)
}

fn wait_until_every(timeout: Duration, every: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if ready() {
            return true;
        }
        thread::sleep(every);
    }
    false
}

fn wait_run_state(fixture: &Fixture, run_id: &str, want: &str, timeout: Duration) -> Value {
    let started = Instant::now();
    let envs = owner_env();
    loop {
        let output = run(
            &fixture.home,
            &fixture.workspace,
            &["workflow", "status", run_id],
            &envs,
        );
        if output.status.success() {
            let value = serde_json::from_slice::<Value>(&output.stdout).unwrap();
            if value["state"].as_str() == Some(want) {
                return value;
            }
        }
        assert!(
            started.elapsed() <= timeout,
            "run {run_id} did not reach {want}: status={} stderr={} log={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            workflow_log(fixture, run_id)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn init_sdk(fixture: &Fixture) {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "init", fixture.scripts.to_str().unwrap()],
        &[],
    ));
    assert!(fixture.scripts.join("brgr-workflow.mjs").is_file());
}

fn write_script(fixture: &Fixture, name: &str, source: &str) -> PathBuf {
    let path = fixture.scripts.join(name);
    fs::write(&path, source).unwrap();
    path
}

fn start_workflow(fixture: &Fixture, script: &Path) -> String {
    start_workflow_with(fixture, script, &[])
}

fn start_workflow_with(fixture: &Fixture, script: &Path, extra: &[&str]) -> String {
    let script = script.to_str().unwrap();
    let mut args = vec!["workflow", "run", script];
    args.extend_from_slice(extra);
    json_output(&run(&fixture.home, &fixture.workspace, &args, &owner_env()))["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn wait_for_first_step(fixture: &Fixture, run_id: &str) -> String {
    let mut task_id = String::new();
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["workflow", "status", run_id],
                &owner_env(),
            );
            let Some(value) = status
                .status
                .success()
                .then(|| serde_json::from_slice::<Value>(&status.stdout).ok())
                .flatten()
            else {
                return false;
            };
            let Some(task) = value["steps"]
                .as_array()
                .and_then(|steps| steps.first())
                .and_then(|step| step["task_id"].as_str())
            else {
                return false;
            };
            task.clone_into(&mut task_id);
            true
        }),
        "first step was never journaled: {}",
        dump_run(fixture, run_id)
    );
    task_id
}

fn create_workflow(fixture: &Fixture, extra: &[&str]) -> String {
    let mut args = vec!["workflow", "create"];
    args.extend_from_slice(extra);
    json_output(&run(&fixture.home, &fixture.workspace, &args, &owner_env()))["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn write_wait_gate(fixture: &Fixture) -> PathBuf {
    let path = fixture.scripts.join("wait-gate-brgr");
    fs::write(
        &path,
        format!(
            r#"#!/bin/sh
set -eu
real="{real}"
home="{home}"
count="$home/wait-count"
first_wait="$home/first-wait.json"
first_result="$home/first-result.json"
if [ "${{1:-}}" = "--json" ] && [ "${{2:-}}" = "wait" ]; then
  n=0
  if [ -f "$count" ]; then
    n=$(cat "$count")
  fi
  n=$((n + 1))
  printf '%s\n' "$n" > "$count"
  if [ "$n" -eq 1 ]; then
    "$real" "$@" > "$first_wait"
    task="$3"
    "$real" --json result "$task" > "$first_result"
    printf '1\n' > "$home/wait-returned"
    while [ ! -e "$home/allow-wait-return" ]; do
      /bin/sleep 0.05
    done
    cat "$first_wait"
    exit 0
  fi
fi
if [ "${{1:-}}" = "--json" ] && [ "${{2:-}}" = "result" ]; then
  n=0
  if [ -f "$count" ]; then
    n=$(cat "$count")
  fi
  if [ "$n" -eq 1 ] && [ -f "$first_result" ]; then
    cat "$first_result"
    exit 0
  fi
fi
exec "$real" "$@"
"#,
            real = brgr(),
            home = fixture.home.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn spawn_agent_script(
    fixture: &Fixture,
    script: &Path,
    run_id: &str,
    brgr_bin: &Path,
) -> std::process::Child {
    let mut command = Command::new("node");
    command
        .arg(script)
        .current_dir(&fixture.workspace)
        .env("BRGR_BIN", brgr_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", run_id)
        .env("BRGR_OWNER_ID", "codex:workflow-owner")
        .env("BRGR_SESSION_ID", "workflow-session")
        .env_remove("BRGR_PARENT_TASK_ID")
        .env_remove("BRGR_PARENT_ATTEMPT_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env("BRGR_TEST_TRUST_HERDR_PANE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in HERDR {
        command.env_remove(name);
    }
    command.spawn().unwrap()
}

fn reject_and_revise(fixture: &Fixture, task: &str) {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["reject", task, "--reason", "revise the step"],
        &owner_env(),
    ));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "revise",
            task,
            "BRGR_FIXTURE_OK REV2",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &owner_env(),
    ));
}

fn launch_workflow_step(
    fixture: &Fixture,
    run_id: &str,
    step: &str,
    objective: &str,
) -> std::process::Output {
    run(
        &fixture.home,
        &fixture.workspace,
        &[
            "run",
            objective,
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--workflow",
            run_id,
            "--step",
            step,
        ],
        &owner_env(),
    )
}

fn agent_result(fixture: &Fixture) -> Value {
    let path = fixture.home.join("agent-result.json");
    wait_for_file(&path, Duration::from_secs(90));
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn release_revision_two(fixture: &Fixture) {
    fs::write(fixture.home.join("release-rev2"), b"1").unwrap();
}

fn step_keys(status: &Value) -> Vec<String> {
    status["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["step_key"].as_str().unwrap().to_owned())
        .collect()
}

fn notice_states(status: &Value) -> Vec<String> {
    status["notices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|notice| notice["state"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_reviews_then_follow_up(status: &Value) {
    let keys = step_keys(status);
    assert_eq!(keys.len(), 3, "{status}");
    assert!(
        keys[..2].contains(&"review-a".to_owned()) && keys[..2].contains(&"review-b".to_owned()),
        "reviews were not journaled before follow-up: {status}"
    );
    assert_eq!(
        keys[2], "follow-up",
        "follow-up was not created after both reviews sealed: {status}"
    );
}

fn workflow_log(fixture: &Fixture, run_id: &str) -> String {
    fs::read_to_string(
        fixture
            .home
            .join("runs")
            .join(format!("{run_id}.workflow.log")),
    )
    .unwrap_or_default()
}

fn kill_driver(fixture: &Fixture, run_id: &str) {
    let pid_path = fixture
        .home
        .join("runs")
        .join(format!("{run_id}.workflow.pid"));
    assert!(
        wait_until(Duration::from_secs(5), || pid_path.is_file()),
        "driver pid file missing"
    );
    let pid: u32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let killed = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .status()
        .unwrap();
    assert!(killed.success(), "failed to kill driver group {pid}");
    assert!(
        wait_until(Duration::from_secs(5), || {
            Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| !status.success())
        }),
        "driver {pid} still alive"
    );
}

fn wait_for_file(path: &Path, timeout: Duration) {
    assert!(
        wait_until(timeout, || path.is_file()),
        "{} was never written",
        path.display()
    );
}

fn dump_run(fixture: &Fixture, run_id: &str) -> String {
    let status = run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", run_id],
        &owner_env(),
    );
    format!(
        "status_ok={} stdout={} stderr={} log={}",
        status.status.success(),
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr),
        workflow_log(fixture, run_id)
    )
}

#[test]
fn a_follow_up_task_is_created_only_from_an_observed_report() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "criterion.mjs", CRITERION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_reviews_then_follow_up(&status);
    let notices = notice_states(&status);
    assert_eq!(notices, vec!["completed"], "{status}");
}

#[test]
fn resume_after_the_driver_dies_still_creates_exactly_three_tasks() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(fixture.home.join("pause-before-c"), b"1").unwrap();
    let script = write_script(&fixture, "criterion.mjs", CRITERION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    wait_for_file(&fixture.home.join("ab-sealed"), Duration::from_secs(90));
    let before = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    let before_keys = step_keys(&before);
    assert_eq!(before_keys.len(), 2, "{before}");
    assert!(
        before_keys.contains(&"review-a".to_owned())
            && before_keys.contains(&"review-b".to_owned()),
        "{before}"
    );
    kill_driver(&fixture, &run_id);
    fs::remove_file(fixture.home.join("pause-before-c")).unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_reviews_then_follow_up(&status);
}

#[test]
fn a_question_blocks_the_run_until_the_owner_replies() {
    require_node();
    let fixture = fixture_with_script(ASKING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "ask.mjs", QUESTION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let blocked = wait_run_state(&fixture, &run_id, "blocked", Duration::from_secs(90));
    let blocked_notices = notice_states(&blocked)
        .into_iter()
        .filter(|state| state == "blocked")
        .count();
    assert_eq!(blocked_notices, 1, "{blocked}");
    let task = blocked["steps"][0]["task_id"].as_str().unwrap().to_owned();
    let question = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "message",
            "wait",
            &task,
            "--for",
            "owner",
            "--timeout-seconds",
            "30",
        ],
        &owner_env(),
    ));
    let question_id = question["message_id"].as_str().unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "message",
            "send",
            &task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            question_id,
            "--body",
            "token-42",
        ],
        &owner_env(),
    ));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["message", "ack", &task, question_id, "--for", "owner"],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        notice_states(&status)
            .iter()
            .filter(|state| *state == "blocked")
            .count(),
        1,
        "{status}"
    );
}

#[test]
fn a_failed_worker_is_returned_and_the_script_branches_on_it() {
    require_node();
    let fixture = fixture_with_script(FAILING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "fail.mjs", FAILED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        step_keys(&status),
        vec!["fail-step", "recovered"],
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(
        log.contains("HANDLED_FAILED"),
        "script did not branch: {log}"
    );
}

#[test]
fn invalid_agent_options_throw_before_launching_a_task() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "types.mjs", TYPEERROR_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "invalid options launched a task: {status}"
    );
    assert!(
        fixture.workspace.join("typeerror-ok").is_file(),
        "script did not record TypeError coverage"
    );
}

#[test]
fn a_changed_objective_under_an_automatic_key_diverges_and_launches_nothing_new() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(
        fixture.scripts.join("objective.txt"),
        "BRGR_FIXTURE_OK first\n",
    )
    .unwrap();
    let script = write_script(&fixture, "diverge.mjs", DIVERGED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["workflow", "status", &run_id],
                &owner_env(),
            );
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .is_some_and(|value| {
                        value["steps"]
                            .as_array()
                            .is_some_and(|steps| !steps.is_empty())
                    })
        }),
        "first step was never journaled: {}",
        dump_run(&fixture, &run_id)
    );
    kill_driver(&fixture, &run_id);
    fs::write(
        fixture.scripts.join("objective.txt"),
        "BRGR_FIXTURE_OK changed\n",
    )
    .unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "failed", Duration::from_secs(30));
    assert_eq!(
        status["steps"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(log.contains("diverged"), "resume did not diverge: {log}");
}

#[test]
fn a_script_receives_the_newest_revision_when_the_owner_revises_a_sealed_step() {
    require_node();
    let fixture = fixture_with_script(HOLD_REV2_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "revise.mjs", REVISE_SCRIPT);
    let run_id = create_workflow(&fixture, &["--max-parallel", "1"]);
    let gate = write_wait_gate(&fixture);
    let mut child = spawn_agent_script(&fixture, &script, &run_id, &gate);
    wait_for_file(&fixture.home.join("wait-returned"), Duration::from_secs(90));
    let task = wait_for_first_step(&fixture, &run_id);
    reject_and_revise(&fixture, &task);

    let listed = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(listed["state"], "running", "{listed}");
    assert_eq!(listed["steps"][0]["revision"], 1, "{listed}");

    let probe = launch_workflow_step(&fixture, &run_id, "probe", "BRGR_FIXTURE_OK probe");
    assert_eq!(
        probe.status.code(),
        Some(75),
        "a revised step without a newest result freed its parallel slot: status={} stderr={}",
        probe.status,
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        !fixture.home.join("agent-result.json").is_file(),
        "the script received a sealed result before revision 2 finished"
    );

    fs::write(fixture.home.join("allow-wait-return"), b"1").unwrap();
    release_revision_two(&fixture);
    let sealed = agent_result(&fixture);
    let status = child.wait().unwrap();
    assert!(status.success(), "agent script failed: {status}");
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert!(
        sealed["report"]
            .as_str()
            .is_some_and(|text| text.contains("REV2_SEALED")),
        "{sealed}"
    );
}

#[test]
fn resume_after_a_revise_returns_the_newest_revision_result() {
    require_node();
    let fixture = fixture_with_script(HOLD_REV2_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "revise-resume.mjs", REVISE_SCRIPT);
    let run_id = create_workflow(&fixture, &[]);
    let gate = write_wait_gate(&fixture);
    let mut first = spawn_agent_script(&fixture, &script, &run_id, &gate);
    wait_for_file(&fixture.home.join("wait-returned"), Duration::from_secs(90));
    let task = wait_for_first_step(&fixture, &run_id);
    reject_and_revise(&fixture, &task);
    assert!(
        !fixture.home.join("agent-result.json").is_file(),
        "the script received a result before resume"
    );
    let _ = first.kill();
    let _ = first.wait();
    let mut resumed = spawn_agent_script(&fixture, &script, &run_id, Path::new(brgr()));
    release_revision_two(&fixture);
    let sealed = agent_result(&fixture);
    let status = resumed.wait().unwrap();
    assert!(status.success(), "resumed script failed: {status}");
    let listed = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(step_keys(&listed), vec!["revised"], "{listed}");
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert!(
        sealed["report"]
            .as_str()
            .is_some_and(|text| text.contains("REV2_SEALED")),
        "{sealed}"
    );
}

#[test]
fn wait_for_sealed_keeps_waiting_when_status_shows_a_newer_revision() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let argv_log = fixture.scripts.join("fake-brgr.log");
    let wait_count = fixture.scripts.join("wait-count");
    let fake_bin = fixture.scripts.join("fake-brgr");
    fs::write(
        &fake_bin,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
count_file="{count}"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid1="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
rid2="cccccccc-cccc-4ccc-8ccc-cccccccccccc"
case " $* " in
  *" wait "*)
    n=0
    if [ -f "$count_file" ]; then
      n=$(cat "$count_file")
    fi
    n=$((n + 1))
    printf '%s\n' "$n" > "$count_file"
    if [ "$n" -eq 1 ]; then
      printf '%s\n' "{{\"task_id\":\"$task\",\"result_id\":\"$rid1\",\"outcome\":\"candidate\",\"revision\":1}}"
    else
      printf '%s\n' "{{\"task_id\":\"$task\",\"result_id\":\"$rid2\",\"outcome\":\"candidate\",\"revision\":2}}"
    fi
    ;;
  *" workflow status "*)
    printf '%s\n' '{{\"state\":\"running\"}}'
    ;;
  *" status "*)
    printf '%s\n' "{{\"task\":{{\"revision\":2}},\"state\":\"running\"}}"
    ;;
  *" result "*)
    n=0
    if [ -f "$count_file" ]; then
      n=$(cat "$count_file")
    fi
    if [ "$n" -eq 1 ]; then
      printf '%s\n' "{{\"result\":{{\"outcome\":\"candidate\",\"revision\":1,\"result_id\":\"$rid1\"}},\"artifacts\":[{{\"text\":\"REV1\"}}]}}"
    else
      printf '%s\n' "{{\"result\":{{\"outcome\":\"candidate\",\"revision\":2,\"result_id\":\"$rid2\"}},\"artifacts\":[{{\"text\":\"REV2\"}}]}}"
    fi
    ;;
  *" run "*)
    printf '%s\n' "{{\"task_id\":\"$task\"}}"
    ;;
  *)
    printf '%s\n' '{{\"ok\":true}}'
    ;;
esac
"#,
            log = argv_log.display(),
            count = wait_count.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();
    let script = write_script(&fixture, "stale-wait.mjs", REVISE_SCRIPT);
    let output = Command::new("node")
        .arg(&script)
        .current_dir(&fixture.scripts)
        .env("BRGR_BIN", &fake_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", "wf-aaaaaaaaaaaa")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sealed: Value =
        serde_json::from_str(&fs::read_to_string(fixture.home.join("agent-result.json")).unwrap())
            .unwrap();
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["report"], "REV2", "{sealed}");
}
