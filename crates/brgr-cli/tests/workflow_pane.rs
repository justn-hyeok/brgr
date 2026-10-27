//! Black-box proof that a workflow started from a verified Herdr pane can
//! launch TUI (pane-mode) steps, and that a shell with no pane still runs
//! headless steps.

mod common;

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

use common::{fake_herdr, herdr_env};

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

const NO_PANE_NOTE: &str = "no verified Herdr pane: workflow steps must use mode headless";
const OWNER_SESSION: &str = "workflow-session";
const SOURCE_PANE: &str = "w1:p1";
const RESUME_PANE: &str = "w1:p2";

const CLAUDE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo '2.1.0 (Claude Code)'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--output-format <format>' '--model <model>' '--permission-mode <mode>' '--effort <level>'; exit 0;;
esac
echo BRGR_FIXTURE_OK
"#;

const TUI_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";
await agent({ task: "BRGR_FIXTURE_OK", key: "tui-step" });
"#;

const HEADLESS_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";
await agent({ task: "BRGR_FIXTURE_OK", mode: "headless", key: "headless-step" });
"#;

const PAUSE_SCRIPT: &str = r"
await new Promise((resolve) => setTimeout(resolve, 60_000));
";

const ERROR_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";
try {
  await agent({ task: "x", harness: "no-such-harness", mode: "headless", key: "err" });
  throw new Error("expected brgr to fail");
} catch (error) {
  console.log(String(error));
}
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

fn run_with(
    home: &Path,
    workspace: &Path,
    args: &[&str],
    base: &[(&str, &str)],
    extra: &[(&str, String)],
) -> std::process::Output {
    let mut envs: Vec<(&str, &str)> = base.to_vec();
    envs.extend(extra.iter().map(|(name, value)| (*name, value.as_str())));
    run(home, workspace, args, &envs)
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

fn gjc_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

fn add_harness(home: &Path, workspace: &Path, executable: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        workspace,
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
    root: PathBuf,
}

fn fixture_dirs() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    let scripts = root.join("scripts");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    seed_git_repo(&workspace);
    (temp, root, home, workspace, scripts)
}

fn fixture_with_executable(executable: &Path) -> Fixture {
    let (temp, root, home, workspace, scripts) = fixture_dirs();
    add_harness(&home, &workspace, executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
        root,
    }
}

fn fixture_gjc() -> Fixture {
    fixture_with_executable(&gjc_fixture())
}

fn fixture_claude() -> Fixture {
    let (temp, root, home, workspace, scripts) = fixture_dirs();
    let executable = root.join("claude");
    fs::write(&executable, CLAUDE).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_harness(&home, &workspace, &executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
        root,
    }
}

fn owner_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "claude:workflow-owner"),
        ("BRGR_SESSION_ID", OWNER_SESSION),
    ]
}

fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
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

fn pane_file(fixture: &Fixture, run_id: &str) -> PathBuf {
    fixture
        .home
        .join("runs")
        .join(format!("{run_id}.workflow.pane"))
}

fn pid_file(fixture: &Fixture, run_id: &str) -> PathBuf {
    fixture
        .home
        .join("runs")
        .join(format!("{run_id}.workflow.pid"))
}

fn init_sdk(fixture: &Fixture) {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "init", fixture.scripts.to_str().unwrap()],
        &[],
    ));
}

fn write_script(fixture: &Fixture, name: &str, source: &str) -> PathBuf {
    let path = fixture.scripts.join(name);
    fs::write(&path, source).unwrap();
    path
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

fn wait_first_step(fixture: &Fixture, run_id: &str, timeout: Duration) -> Value {
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
            if value["steps"]
                .as_array()
                .is_some_and(|steps| !steps.is_empty())
            {
                return value;
            }
        }
        assert!(
            started.elapsed() <= timeout,
            "run {run_id} never journaled a step: status={} stderr={} log={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            workflow_log(fixture, run_id)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn launch_envelope(home: &Path, task_id: &str) -> Value {
    let path = home.join("launches").join(format!("{task_id}.json"));
    serde_json::from_slice(&fs::read(&path).unwrap_or_else(|_| {
        panic!("launch file missing: {}", path.display());
    }))
    .unwrap()
}

fn presentation(launch: &Value) -> &'static str {
    if launch["pane_mode"].as_bool() == Some(true) {
        "tui"
    } else {
        "headless"
    }
}

fn herdr_for(fixture: &Fixture, pane: &str) -> (PathBuf, PathBuf, Vec<(&'static str, String)>) {
    let dir = fixture
        .root
        .join(format!("fake-herdr-{}", pane.replace(':', "_")));
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    let env = herdr_env(&fake, &dir, pane);
    (dir, fake, env)
}

fn dead_herdr_env(fixture: &Fixture) -> Vec<(&'static str, String)> {
    let dir = fixture.root.join("dead-herdr");
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("herdr");
    fs::write(&binary, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    vec![("HERDR_BIN_PATH", binary.to_str().unwrap().to_owned())]
}

fn start_workflow(
    fixture: &Fixture,
    script: &Path,
    extra: &[(&str, String)],
) -> (String, std::process::Output) {
    let output = run_with(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "run", script.to_str().unwrap()],
        &owner_env(),
        extra,
    );
    let run_id = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| value["run_id"].as_str().map(str::to_owned))
        .unwrap_or_default();
    (run_id, output)
}

fn cancel_run(fixture: &Fixture, run_id: &str) {
    let _ = run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "cancel", run_id],
        &owner_env(),
    );
}

fn pid_is_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn command_line_contains(pid: u32, needle: &str) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-www", "-o", "command="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains(needle))
}

fn live_driver_pid(fixture: &Fixture, run_id: &str) -> Option<u32> {
    let pid = fs::read_to_string(pid_file(fixture, run_id))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (pid_is_alive(pid) && command_line_contains(pid, "workflow __drive")).then_some(pid)
}

fn kill_driver(fixture: &Fixture, run_id: &str) {
    assert!(
        wait_until(Duration::from_secs(5), || {
            live_driver_pid(fixture, run_id).is_some()
        }),
        "driver never became live"
    );
    let pid = live_driver_pid(fixture, run_id).unwrap();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert!(
        wait_until(Duration::from_secs(5), || {
            live_driver_pid(fixture, run_id).is_none()
        }),
        "driver {pid} still alive"
    );
}

fn assert_private_pane(path: &Path, pane: &str) {
    let text = fs::read_to_string(path).unwrap_or_else(|_| panic!("{} missing", path.display()));
    assert_eq!(text.trim(), pane, "{}", path.display());
    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{} mode {mode:o}", path.display());
}

fn resume(fixture: &Fixture, run_id: &str, extra: &[(&str, String)]) -> std::process::Output {
    run_with(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", run_id],
        &owner_env(),
        extra,
    )
}

#[test]
fn a_workflow_step_without_headless_opens_beside_the_verified_source_pane() {
    require_node();
    let fixture = fixture_claude();
    init_sdk(&fixture);
    let script = write_script(&fixture, "tui.mjs", TUI_SCRIPT);
    let (_dir, _fake, herdr) = herdr_for(&fixture, SOURCE_PANE);
    let (run_id, started) = start_workflow(&fixture, &script, &herdr);
    assert!(
        started.status.success(),
        "workflow run failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let listed = wait_first_step(&fixture, &run_id, Duration::from_secs(30));
    assert_private_pane(&pane_file(&fixture, &run_id), SOURCE_PANE);
    let task_id = listed["steps"][0]["task_id"].as_str().unwrap();
    let status = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", task_id],
        &owner_env(),
    ));
    let launch = launch_envelope(&fixture.home, task_id);
    assert_eq!(presentation(&launch), "tui", "{launch}");
    assert_eq!(
        launch["source_pane"].as_str(),
        Some(SOURCE_PANE),
        "{launch}"
    );
    assert_eq!(
        status["source_pane"].as_str(),
        Some(SOURCE_PANE),
        "{status}"
    );
    cancel_run(&fixture, &run_id);
}

#[test]
fn workflow_run_without_a_pane_warns_and_a_headless_step_still_completes() {
    require_node();
    let fixture = fixture_gjc();
    init_sdk(&fixture);
    let script = write_script(&fixture, "headless.mjs", HEADLESS_SCRIPT);
    let (run_id, started) = start_workflow(&fixture, &script, &[]);
    assert!(
        started.status.success(),
        "workflow run failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(
        stderr.lines().any(|line| line == NO_PANE_NOTE),
        "missing pane note: {stderr}"
    );
    assert!(!pane_file(&fixture, &run_id).exists());
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(status["steps"][0]["step_key"], "headless-step", "{status}");
}

#[test]
fn the_pane_file_is_private_replaced_on_resume_and_dropped_when_stale() {
    require_node();
    let fixture = fixture_gjc();
    let script = write_script(&fixture, "pause.mjs", PAUSE_SCRIPT);
    let (_dir, _fake, first) = herdr_for(&fixture, SOURCE_PANE);
    let (run_id, started) = start_workflow(&fixture, &script, &first);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let path = pane_file(&fixture, &run_id);
    assert_private_pane(&path, SOURCE_PANE);
    kill_driver(&fixture, &run_id);

    let (_dir, _fake, second) = herdr_for(&fixture, RESUME_PANE);
    let resumed = resume(&fixture, &run_id, &second);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_private_pane(&path, RESUME_PANE);
    kill_driver(&fixture, &run_id);

    let dropped = resume(&fixture, &run_id, &dead_herdr_env(&fixture));
    assert!(
        dropped.status.success(),
        "{}",
        String::from_utf8_lossy(&dropped.stderr)
    );
    let stderr = String::from_utf8_lossy(&dropped.stderr);
    assert!(
        stderr.lines().any(|line| line == NO_PANE_NOTE),
        "missing drop note: {stderr}"
    );
    assert!(!path.exists(), "stale pane file was kept");
    cancel_run(&fixture, &run_id);
}

#[test]
fn the_sdk_does_not_pass_source_pane_for_a_headless_step() {
    require_node();
    let fixture = fixture_gjc();
    init_sdk(&fixture);
    let argv_log = fixture.root.join("sdk-argv.log");
    let fake_bin = fixture.root.join("fake-brgr");
    fs::write(
        &fake_bin,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case " $* " in
  *" wait "*) echo '{{"state":"completed","task_id":"00000000-0000-4000-8000-000000000000","result_id":"00000000-0000-4000-8000-000000000001","revision":1,"outcome":"candidate"}}';;
  *" result "*) echo '{{"result":{{"outcome":"candidate","revision":1,"result_id":"00000000-0000-4000-8000-000000000001"}},"artifacts":[{{"text":"ok"}}]}}';;
  *" workflow status "*) echo '{{"state":"running"}}';;
  *" run "*) echo '{{"task_id":"00000000-0000-4000-8000-000000000000"}}';;
  *) echo '{{"ok":true}}';;
esac
"#,
            log = argv_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();
    let script = write_script(&fixture, "sdk-headless.mjs", HEADLESS_SCRIPT);
    let output = Command::new("node")
        .arg(&script)
        .current_dir(&fixture.scripts)
        .env("BRGR_BIN", &fake_bin)
        .env("BRGR_WORKFLOW_RUN", "wf-aaaaaaaaaaaa")
        .env("BRGR_WORKFLOW_SOURCE_PANE", SOURCE_PANE)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sdk script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv = fs::read_to_string(&argv_log).unwrap();
    let run_line = argv
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == "run"))
        .unwrap_or_else(|| panic!("no run invocation: {argv}"));
    assert!(
        !run_line.contains("--source-pane"),
        "headless step passed --source-pane: {run_line}"
    );
}

#[test]
fn a_brgr_error_is_not_prefixed_with_a_second_error() {
    require_node();
    let fixture = fixture_gjc();
    init_sdk(&fixture);
    let script = write_script(&fixture, "error.mjs", ERROR_SCRIPT);
    let (run_id, started) = start_workflow(&fixture, &script, &[]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    let log = workflow_log(&fixture, &run_id);
    assert!(
        log.contains("Error:"),
        "thrown message missing Error prefix: {log}"
    );
    assert!(
        !log.contains("Error: Error:"),
        "doubled Error prefix: {log}"
    );
}

#[test]
fn a_named_pane_outside_a_workflow_driver_is_not_trusted_just_for_being_alive() {
    // `--source-pane` names a live Claude pane that this process neither runs
    // inside nor owns a workflow record for: it must be refused, not used.
    let fixture = fixture_claude();
    // This process is "in" p-me; it names p-other.
    let (_dir, _fake, herdr) = herdr_for(&fixture, "p-me");
    let output = run_with(
        &fixture.home,
        &fixture.workspace,
        &["--source-pane", "p-other", "run", "BRGR_FIXTURE_OK"],
        &owner_env(),
        &herdr,
    );
    assert!(
        !output.status.success(),
        "a live agent pane was accepted as the source: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("exact Herdr source"), "{said}");
}
