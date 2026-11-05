mod common;

#[test]
fn config_edit_without_terminal_or_with_json_refuses_promptly() {
    for json in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let mut command = assert_cmd::Command::from_std(common::brgr_command());
        command.arg("--home").arg(home.path());
        if json {
            command.arg("--json");
        }
        command
            .args(["config", "edit"])
            .timeout(std::time::Duration::from_secs(5));
        command
            .assert()
            .failure()
            .stderr(predicates::str::contains("brgr config alias"))
            .stderr(predicates::str::contains("brgr config set"));
        assert!(!home.path().join("config.toml").exists());
    }
}

use common::{
    HANG_GUARD, brgr_binary as brgr, brgr_command, output_when_not_busy, process_exists,
    wait_until, write_fast_script,
};

use std::{
    fmt::Write as _,
    fs,
    io::Write as _,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

/// The task's active attempt once its detached supervisor has claimed it.
fn active_attempt(store: &brgr_store::Store, task: &str) -> brgr_protocol::AttemptId {
    let task_id = task.parse().unwrap();
    let mut found = None;
    assert!(
        wait_until(|| {
            found = store.active_message_attempt(task_id).ok();
            found.is_some()
        }),
        "task {task} never had an active attempt"
    );
    found.unwrap()
}

/// Runs `brgr status`, which drives `reconcile_pending`, until the task is
/// terminal, then reads its result. `None` once the hang guard passes.
fn settle(home: &Path, task: &str, owner: &[(&str, &str)]) -> Option<Value> {
    wait_until(|| json_output(&run(home, &["status", task], owner))["state"] == "terminal")
        .then(|| json_output(&run(home, &["result", task], owner)))
}

/// The owner pane's prompt file once a line satisfies `delivered`.
fn delivered_prompt(prompts: &Path, delivered: impl Fn(&str) -> bool) -> Option<String> {
    let mut text = String::new();
    wait_until(|| {
        text = fs::read_to_string(prompts).unwrap_or_default();
        delivered(&text)
    })
    .then_some(text)
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = brgr_command();
    command.arg("--home").arg(home).arg("--json").args(args);
    // These fixtures certify one-shot process behavior. Native TUI tests opt
    // into the pane fixture below; a production default call is tested separately.
    if matches!(args.first(), Some(&"run" | &"revise"))
        && !args.contains(&"local.omp-herdr")
        && !(envs.iter().any(|(name, _)| *name == "HERDR_PANE_ID")
            && (args.contains(&"local.claude-code")
                || args.contains(&"local.codex")
                || args.contains(&"local.opencode")))
    {
        command.arg("--headless");
    }
    // A Claude Code or Cursor owner is identified by its own session variable.
    if !envs
        .iter()
        .any(|(name, _)| matches!(*name, "CLAUDE_CODE_SESSION_ID" | "CURSOR_CONVERSATION_ID"))
    {
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

/// Fails unless brgr runs it the way the Devin recipe promises: print mode,
/// workspace trust bypassed, and — with no level requested — the full level,
/// which Devin calls `dangerous`.
const DEVIN_FIXTURE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'devin 3000.fixture'; exit 0;;
  --help)
    printf '%s\n' '--prompt-file <FILE>' '-p, --print [<PROMPT>]' '--permission-mode <PERMISSION_MODE>' '--respect-workspace-trust [<RESPECT_WORKSPACE_TRUST>]' '--model <MODEL>'
    exit 0;;
  models)
    test "$2" = list && test "$3" = --format && test "$4" = json || exit 2
    echo '{"families":[{"slug":"swe-1.6"}]}'
    exit 0;;
esac
prompt_file=
print_mode=0
full_mode=0
trust_bypassed=0
model=
while test "$#" -gt 0; do
  case "$1" in
    --prompt-file) shift; prompt_file=$1;;
    -p|--print) print_mode=1;;
    --permission-mode) shift; test "$1" = dangerous || exit 3; full_mode=1;;
    --respect-workspace-trust) shift; test "$1" = false || exit 4; trust_bypassed=1;;
    --model) shift; model=$1;;
    --model=*) model=${1#--model=};;
    *) exit 5;;
  esac
  shift
done
test "$print_mode" = 1 || exit 6
test "$full_mode" = 1 || exit 7
test "$trust_bypassed" = 1 || exit 8
test -f "$prompt_file" || exit 10
/bin/cat "$prompt_file"
if test -n "$model"; then
  printf 'MODEL=%s\n' "$model"
fi
"#;

const DEVIN_OLD_FIXTURE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'devin 3000.old'; exit 0;;
  --help)
    printf '%s\n' '--prompt-file <FILE>' '-p, --print [<PROMPT>]' '--permission-mode <PERMISSION_MODE>' '--respect-workspace-trust [<RESPECT_WORKSPACE_TRUST>]'
    exit 0;;
esac
prompt_file=
print_mode=0
full_mode=0
trust_bypassed=0
while test "$#" -gt 0; do
  case "$1" in
    --prompt-file) shift; prompt_file=$1;;
    -p|--print) print_mode=1;;
    --permission-mode) shift; test "$1" = dangerous || exit 3; full_mode=1;;
    --respect-workspace-trust) shift; test "$1" = false || exit 4; trust_bypassed=1;;
    *) exit 5;;
  esac
  shift
done
test "$print_mode" = 1 || exit 6
test "$full_mode" = 1 || exit 7
test "$trust_bypassed" = 1 || exit 8
test -f "$prompt_file" || exit 10
/bin/cat "$prompt_file"
"#;

/// The fixture fixture harness every contract test registers.
fn brgr_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/brgr-fixture")
        .canonicalize()
        .unwrap()
}

/// A control home with the fixture fixture registered, and an empty workspace.
fn fixture_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    (temp, home, workspace)
}

#[test]
fn aliases_expand_immutably_and_support_korean_names() {
    let (_temp, home, workspace) = fixture_home();
    let set = run(
        &home,
        &[
            "config",
            "alias",
            "set",
            "코덱",
            "--harness",
            "local.fixture",
            "--model",
            "fixture-model",
            "--effort",
            "low",
        ],
        &[],
    );
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let listed = run(&home, &["config", "alias", "list"], &[]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("코덱"));
    let launched = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--use",
            "코덱",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[],
    );
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let receipt = json_output(&launched);
    let task = receipt["task_id"].as_str().unwrap();
    let status = run(&home, &["status", task, "--verbose", "--observe"], &[]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let detail: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(detail["task"]["route"]["harness_id"], "local.fixture");
    assert_eq!(detail["task"]["route"]["requested_model"], "fixture-model");
    let changed = run(
        &home,
        &[
            "config",
            "alias",
            "set",
            "코덱",
            "--harness",
            "local.fixture",
            "--model",
            "changed-model",
        ],
        &[],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let unchanged = run(&home, &["status", task, "--verbose", "--observe"], &[]);
    let detail: Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(detail["task"]["route"]["requested_model"], "fixture-model");
    let removed = run(&home, &["config", "alias", "remove", "코덱"], &[]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&run(&home, &["config", "alias", "list"], &[]).stdout)
            .contains("코덱")
    );
}

#[test]
fn unknown_alias_creates_no_task_and_conflicts_with_route_flags() {
    let (_temp, home, workspace) = fixture_home();
    let unknown = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--use",
            "missing",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!unknown.status.success());
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("unknown alias missing") && stderr.contains("(none)"),
        "{stderr}"
    );
    let conflict = run(
        &home,
        &["run", "BRGR_FIXTURE_OK", "--use", "missing", "--model", "m"],
        &[],
    );
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));
    let tasks = run(&home, &["status", "--brief"], &[]);
    assert_eq!(json_output(&tasks), json!([]));
}

#[test]
fn alias_set_refuses_unregistered_harness_and_preserves_existing_config() {
    let (_temp, home, _workspace) = fixture_home();
    let config = home.join("config.toml");
    fs::write(
        &config,
        "[herdr]\nauto_worker_pane = true\n\n[defaults]\neffort = \"high\"\n",
    )
    .unwrap();
    let refused = run(
        &home,
        &["config", "alias", "set", "x", "--harness", "local.unknown"],
        &[],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not registered"));
    let set = run(
        &home,
        &["config", "alias", "set", "x", "--harness", "local.fixture"],
        &[],
    );
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let written = fs::read_to_string(&config).unwrap();
    assert!(
        written.contains("auto_worker_pane = true") && written.contains("effort = \"high\""),
        "{written}"
    );
    let removed = run(&home, &["config", "alias", "remove", "x"], &[]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let written = fs::read_to_string(&config).unwrap();
    assert!(
        written.contains("auto_worker_pane = true") && written.contains("[defaults]"),
        "{written}"
    );
}

/// An echo harness returns the prompt it was given. The objective must still be
/// its exact first line — some harnesses key on it, OMP on `FROM CODEX` — and
/// the owner-messaging brief must follow, since every worker may ask.
fn assert_worker_prompt(echoed: &str, objective: &str) {
    assert_eq!(echoed.lines().next(), Some(objective), "{echoed}");
    assert!(echoed.contains("BRGR OWNER MESSAGES"), "{echoed}");
    assert!(
        echoed.contains("Child delegation is not enabled"),
        "{echoed}"
    );
}

fn write_devin_fixture(executable: &Path) {
    write_fast_script(executable, DEVIN_FIXTURE);
}

/// A shared Codex daemon can carry another worker's `BRGR_SESSION_ID` and
/// `BRGR_PARENT_*`. `--as` names this session; those parent variables belong
/// to the other one and must not refuse the run. The same environment with
/// `--as` matching `BRGR_SESSION_ID` is still a delegated child and is still
/// refused when the parent attempt is missing.
#[test]
fn as_session_ignores_another_sessions_stale_parent_identity() {
    let (_temp, home, workspace) = fixture_home();
    let parent_task = "11111111-1111-4111-8111-111111111111";
    let parent_attempt = "22222222-2222-4222-8222-222222222222";
    let stale = [
        ("BRGR_SESSION_ID", "other"),
        ("BRGR_PARENT_TASK_ID", parent_task),
        ("BRGR_PARENT_ATTEMPT_ID", parent_attempt),
    ];
    let mine = run(
        &home,
        &[
            "--as",
            "mine",
            "run",
            "BRGR_FIXTURE_OK",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
            "--headless",
        ],
        &stale,
    );
    assert!(
        mine.status.success(),
        "named session should ignore another session's parent: {}",
        String::from_utf8_lossy(&mine.stderr)
    );
    assert!(
        String::from_utf8_lossy(&mine.stderr).contains(
            "brgr: ignoring the parent worker identity of another session (BRGR_SESSION_ID differs from --as)"
        ),
        "{}",
        String::from_utf8_lossy(&mine.stderr)
    );
    assert_eq!(json_output(&mine)["outcome"], "candidate");

    let other = run(
        &home,
        &[
            "--as",
            "other",
            "run",
            "BRGR_FIXTURE_OK",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
            "--headless",
        ],
        &stale,
    );
    assert!(
        !other.status.success(),
        "matching session should still check the parent: {}",
        String::from_utf8_lossy(&other.stderr)
    );
    let stderr = String::from_utf8_lossy(&other.stderr);
    assert!(
        stderr
            .contains("delegation parent attempt is stale, mismatched, or owned by another worker"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("ignoring the parent worker identity of another session"),
        "{stderr}"
    );
}

#[test]
fn plugin_board_shows_candidate_then_explicit_decision_without_prompt_text() {
    let (_temp, home, workspace) = fixture_home();
    let envs = [
        ("BRGR_OWNER_ID", "codex:board-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PLUGIN_ID", "brgr"),
    ];
    let created = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    ));
    let task_id = created["task_id"].as_str().unwrap();
    let board = run(&home, &["plugin", "board", "--once"], &envs);
    assert!(board.status.success());
    let board_text = String::from_utf8(board.stdout).unwrap();
    assert!(board_text.contains(&task_id[..8]));
    assert!(board_text.contains(" work "));
    assert!(board_text.contains("candidate"));
    assert!(!board_text.contains("BRGR_FIXTURE_OK"));

    json_output(&run(
        &home,
        &["accept", task_id, "--reason", "fixture verified"],
        &envs,
    ));
    let decided = run(&home, &["plugin", "board", "--once"], &envs);
    assert!(decided.status.success());
    assert!(
        String::from_utf8(decided.stdout)
            .unwrap()
            .contains("accepted")
    );
    let decided_text =
        String::from_utf8(run(&home, &["plugin", "board", "--once"], &envs).stdout).unwrap();
    assert!(!decided_text.contains("BRGR_FIXTURE_OK"));
    assert!(!decided_text.contains("fixture verified"));
}

#[test]
fn plugin_board_refresh_failure_is_visible_without_mutating_tasks() {
    let (_temp, home, workspace) = fixture_home();
    let envs = [
        ("BRGR_OWNER_ID", "codex:board-fail"),
        ("HERDR_ENV", "1"),
        ("HERDR_PLUGIN_ID", "brgr"),
    ];
    let created = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    ));
    let task_id = created["task_id"].as_str().unwrap();
    let store = home.join("store").join("brgr.sqlite3");
    let original = fs::read(&store).unwrap();
    fs::write(&store, b"not a sqlite database").unwrap();
    let failed = run(&home, &["plugin", "board", "--once"], &envs);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("board refresh failed"));
    assert!(stderr.contains("No tasks were mutated or reconciled"));
    assert!(!stderr.contains("BRGR_FIXTURE_OK"));
    fs::write(&store, original).unwrap();
    let restored = run(&home, &["plugin", "board", "--once"], &envs);
    assert!(restored.status.success());
    let restored_text = String::from_utf8(restored.stdout).unwrap();
    assert!(restored_text.contains(&task_id[..8]));
    assert!(restored_text.contains("candidate"));
}

#[test]
fn plugin_codex_bridge_runs_a_fixture_outside_the_codex_process() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex-home");
    let workspace = temp.path().join("work");
    let outside = temp.path().join("outside");
    let fake_bin = temp.path().join("bin");
    let output_path = temp.path().join("candidate.json");
    let codex_args_path = temp.path().join("codex-args.txt");
    let denied_harness_path = temp.path().join("denied-harness.txt");
    let denied_workspace_path = temp.path().join("denied-workspace.txt");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));

    let fake_codex = fake_bin.join("codex");
    write_fast_script(
        &fake_codex,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRGR_TEST_CODEX_ARGS\"\nexport CODEX_THREAD_ID=bridge-test\n\"$BRGR_BIN\" --json harness add /bin/echo --presentation-only > /dev/null 2> \"$BRGR_TEST_DENIED_HARNESS\"\nprintf '%s\\n' \"$?\" >> \"$BRGR_TEST_DENIED_HARNESS\"\n\"$BRGR_BIN\" --json --headless run ESCAPE --harness local.fixture --workspace \"$BRGR_TEST_OUTSIDE\" --foreground > /dev/null 2> \"$BRGR_TEST_DENIED_WORKSPACE\"\nprintf '%s\\n' \"$?\" >> \"$BRGR_TEST_DENIED_WORKSPACE\"\nexec \"$BRGR_BIN\" --json --headless run BRGR_FIXTURE_OK --harness local.fixture --criterion 'artifact text equals BRGR_FIXTURE_OK' --workspace \"$BRGR_TEST_WORKSPACE\" --foreground > \"$BRGR_TEST_OUTPUT\"\n",
    );
    let path = std::env::join_paths(
        std::iter::once(fake_bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let context = json!({"workspace_id": "w1", "workspace_cwd": workspace});
    let launched = brgr_command()
        .args(["plugin", "codex"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PLUGIN_ID", "brgr")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("BRGR_HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .env("BRGR_BIN", brgr())
        .env("BRGR_TEST_WORKSPACE", &workspace)
        .env("BRGR_TEST_OUTSIDE", &outside)
        .env("BRGR_TEST_OUTPUT", &output_path)
        .env("BRGR_TEST_CODEX_ARGS", &codex_args_path)
        .env("BRGR_TEST_DENIED_HARNESS", &denied_harness_path)
        .env("BRGR_TEST_DENIED_WORKSPACE", &denied_workspace_path)
        .env("PATH", path)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_SESSION_ID")
        .output()
        .unwrap();
    assert!(
        launched.status.success(),
        "plugin Codex bridge failed: {}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let codex_args = fs::read_to_string(codex_args_path).unwrap();
    assert!(codex_args.contains("shell_environment_policy.set.PATH="));
    assert!(codex_args.contains("shell_environment_policy.set.BRGR_PLUGIN_BRIDGE_DIR="));
    let args = codex_args.lines().collect::<Vec<_>>();
    let add_dir = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--add-dir").then_some(pair[1]))
        .expect("Codex launch must expose only the private bridge directory");
    assert_ne!(Path::new(add_dir), home);
    assert!(
        !Path::new(add_dir).exists(),
        "bridge tempdir must close with Codex"
    );
    let denied_harness = fs::read_to_string(&denied_harness_path).unwrap();
    assert!(denied_harness.contains("unavailable through the Herdr host bridge"));
    assert_ne!(denied_harness.lines().last(), Some("0"));
    let denied_workspace = fs::read_to_string(&denied_workspace_path).unwrap();
    assert!(denied_workspace.contains("outside the selected Herdr workspace"));
    assert_ne!(denied_workspace.lines().last(), Some("0"));
    assert_eq!(
        brgr_registry::Registry::open(home.join("registry"))
            .unwrap()
            .registered_harness_ids()
            .unwrap(),
        vec!["local.fixture"]
    );
    let candidate: Value = serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(candidate["outcome"], "candidate");
    let task = candidate["task_id"].as_str().unwrap();
    let result = json_output(&run(
        &home,
        &["result", task],
        &[
            ("CODEX_THREAD_ID", "bridge-test"),
            ("BRGR_SESSION_ID", "bridge-test"),
        ],
    ));
    assert_eq!(result["artifacts"][0]["text"], "BRGR_FIXTURE_OK");
    assert!(!fs::read_dir(&home).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("plugin-bridge-")
    }));
}

#[test]
fn plugin_codex_uses_pinned_workspace_not_live_plugin_cwd() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex-home");
    let project = temp.path().join("project");
    let plugin_checkout = temp.path().join("plugin-checkout");
    let fake_bin = temp.path().join("bin");
    let codex_args_path = temp.path().join("codex-args.txt");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&plugin_checkout).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_codex = fake_bin.join("codex");
    write_fast_script(
        &fake_codex,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRGR_TEST_CODEX_ARGS\"\n",
    );
    let path = std::env::join_paths(
        std::iter::once(fake_bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let context = json!({
        "workspace_id": "w1",
        "workspace_cwd": plugin_checkout,
        "focused_pane_cwd": plugin_checkout
    });
    let launched = brgr_command()
        .args(["plugin", "codex"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PLUGIN_ID", "brgr")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("BRGR_PLUGIN_WORKSPACE_CWD", &project)
        .env("BRGR_HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .env("BRGR_TEST_CODEX_ARGS", &codex_args_path)
        .env("PATH", path)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_SESSION_ID")
        .output()
        .unwrap();
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let args = fs::read_to_string(codex_args_path).unwrap();
    let mut found_cwd = false;
    let mut lines = args.lines();
    while let Some(line) = lines.next() {
        if line == "-C" {
            assert_eq!(lines.next(), Some(project.to_str().unwrap()));
            found_cwd = true;
            break;
        }
    }
    assert!(found_cwd, "{args}");
    assert!(args.contains("shell_environment_policy.set.PATH="));
    assert!(args.contains("shell_environment_policy.set.BRGR_HOME="));
}

#[test]
fn plugin_entrypoints_fail_closed_without_herdr_host() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let board = run(&home, &["plugin", "board", "--once"], &[]);
    assert!(!board.status.success());
    assert!(String::from_utf8_lossy(&board.stderr).contains("brgr Herdr plugin host"));
    let open = run(&home, &["plugin", "open", "--no-focus"], &[]);
    assert!(!open.status.success());
    assert!(String::from_utf8_lossy(&open.stderr).contains("brgr Herdr plugin host"));
}

/// A task placed in a Herdr worker pane is claimed only once Herdr has opened
/// the pane and started `brgr plugin worker` in it. Another brgr command in the
/// meantime must not record it lost after the detached supervisor's five-second
/// window; it gets a minute.
#[test]
fn explicit_headless_does_not_open_a_second_worker_pane() {
    let fixture = pane_fixture();
    json_output(&run(
        &fixture.home,
        &["config", "set-auto-worker-pane", "true"],
        &[],
    ));
    let ran = fixture.run(&["--headless"]);
    assert_eq!(ran["outcome"], "candidate");
    assert!(!fixture.state("calls.log").contains("pane split"));
}
#[test]
fn native_worker_placement_uses_config_and_reaches_owner_decision() {
    let fixture = pane_fixture();
    json_output(&run(
        &fixture.home,
        &["config", "set-worker-placement", "tab"],
        &[],
    ));
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate");
    let calls = fixture.state("calls.log");
    assert!(calls.contains("tab create --workspace w9"), "{calls}");
    assert!(!calls.contains("pane split"));
    assert!(calls.contains("--no-focus"));
    assert!(fixture.state("closed").is_empty());
    json_output(&run(
        &fixture.home,
        &["accept", ran["task_id"].as_str().unwrap()],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    // The decision closes the pane at once when the agent reads idle;
    // otherwise the session loop closes it on its next poll.
    assert!(
        wait_until(|| fixture.state("closed").trim() == "closed"),
        "the pane was never closed after the decision: {}",
        fixture.state("calls.log")
    );
}
/// A worker that asks its owner, or tries to delegate, as its prompt says.
///
/// Runs as a real harness process, so what it can do is exactly what brgr gave
/// it — the identity in its environment and nothing the test simulates.
const ASKING_FIXTURE_FIXTURE: &str = r#"#!/bin/sh
set -u
case "${1:-}" in
  --version) echo 'fixture v-asking-fixture'; exit 0;;
  --help)
    printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'
    exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
answer=NO_WORKER_IDENTITY
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}"; then
  if /usr/bin/grep -q '^ASK' "$prompt_file"; then
    "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body 'Which token?' >/dev/null
    reply=$("$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds 20)
    reply_id=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"message_id":"\([^"]*\)".*/\1/p')
    "$BRGR_BIN" --json message ack "$BRGR_PARENT_TASK_ID" "$reply_id" --for worker >/dev/null
    answer=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"body":"\([^"]*\)".*/\1/p')
  elif /usr/bin/grep -q '^SPAWN' "$prompt_file"; then
    if "$BRGR_BIN" --json --headless run LEAF --harness local.fixture --workspace "$PWD" --foreground >/dev/null 2>"$PWD/../spawn.err"; then
      answer=SPAWN_ALLOWED
    elif /usr/bin/grep -q 'not started with delegation' "$PWD/../spawn.err"; then
      answer=SPAWN_REFUSED
    else
      answer=SPAWN_FAILED_OTHERWISE
    fi
  fi
fi
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' "$answer"
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn asking_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&executable, ASKING_FIXTURE_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    (temp, home, workspace)
}

/// A worker started by a plain `brgr run` can ask its owner and use the reply.
///
/// Before 2.4.0 the identity a worker needs to message was attached only when
/// the task was started with delegation, so the default route — the one the
/// orchestration skill uses — could not ask at all. The author's store held
/// zero messages across 57 tasks.
#[test]
fn a_worker_started_without_delegation_can_ask_its_owner() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:ask-test")];
    let launched = json_output(&run(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    let task = launched["task_id"].as_str().unwrap();

    let question = json_output(&run(
        &home,
        &[
            "message",
            "wait",
            task,
            "--for",
            "owner",
            "--timeout-seconds",
            "60",
        ],
        &owner,
    ));
    assert_eq!(question["body"], "Which token?", "{question}");
    let question_id = question["message_id"].as_str().unwrap();
    json_output(&run(
        &home,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            question_id,
            "--body",
            "token-42",
        ],
        &owner,
    ));
    json_output(&run(
        &home,
        &["message", "ack", task, question_id, "--for", "owner"],
        &owner,
    ));

    json_output(&run(
        &home,
        &["wait", task, "--timeout-seconds", "60"],
        &owner,
    ));
    let result = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(result["artifacts"][0]["text"], "token-42", "{result}");
}

/// Messaging is for every worker; delegation stays a permission.
///
/// Giving every worker its identity also lets it name itself as a parent, so
/// the permission is checked at child admission against the parent's launch.
#[test]
fn a_worker_started_without_delegation_cannot_start_a_child() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:spawn-test")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "SPAWN",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    let task = result["task_id"].as_str().unwrap();
    let sealed = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(sealed["artifacts"][0]["text"], "SPAWN_REFUSED", "{sealed}");
    let listed = json_output(&run(&home, &["status"], &owner));
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
}

#[test]
#[allow(clippy::too_many_lines)] // End-to-end pane-mode admission and worker identity contract.
fn a_pane_worker_without_delegation_is_refused_before_child_admission() {
    let fixture = pane_fixture();
    seed_git_repo(&fixture.workspace);
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("hang"), "").unwrap();
    add_fixture(
        &fixture.home,
        &brgr_fixture(),
        &fixture.workspace.parent().unwrap().join("fixture-scratch"),
    );
    let mut parent_process = brgr_command();
    parent_process
        .arg("--home")
        .arg(&fixture.home)
        .arg("--json")
        .args([
            "run",
            "Do not delegate",
            "--harness",
            "local.claude-code",
            "--workspace",
        ])
        .arg(&fixture.workspace)
        .arg("--foreground")
        .env_remove("CODEX_THREAD_ID")
        .env("BRGR_SESSION_ID", "fixture-session")
        .env("BRGR_OWNER_ID", "codex:pane-test")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", &fixture.herdr)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let parent_process = parent_process.spawn().unwrap();
    assert!(
        wait_until(|| fixture.state("calls.log").contains("agent prompt")),
        "the pane worker was never prompted"
    );
    let parent_task = fs::read_dir(fixture.home.join("launches"))
        .unwrap()
        .find_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".json").map(str::to_owned)
        })
        .unwrap();
    let store = brgr_store::Store::open(fixture.home.join("store")).unwrap();
    let parent_id = parent_task.parse().unwrap();
    let parent_spec = store.task(parent_id).unwrap();
    let parent_attempt = active_attempt(&store, &parent_task);
    let launch: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .home
                .join("launches")
                .join(format!("{parent_task}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let before = store.tasks(100).unwrap().len();
    let wrong_task = "2b2c279a-b8e1-43a8-846c-cb8344a301d7";
    let wrong_attempt = "3b2c279a-b8e1-43a8-846c-cb8344a301d7";
    let worker_owner = format!("worker:{parent_attempt}");
    let mut child_command = brgr_command();
    let child = child_command
        .arg("--home")
        .arg(&fixture.home)
        .arg("--json")
        .args([
            "--headless",
            "run",
            "LEAF",
            "--harness",
            "local.fixture",
            "--workspace",
        ])
        .arg(&parent_spec.workspace)
        .args(["--foreground"])
        .current_dir(&parent_spec.workspace)
        .env_remove("CODEX_THREAD_ID")
        .env("BRGR_SESSION_ID", "fixture-session")
        .env("BRGR_OWNER_ID", &worker_owner)
        .env("BRGR_PARENT_TASK_ID", wrong_task)
        .env("BRGR_PARENT_ATTEMPT_ID", wrong_attempt)
        .output()
        .unwrap();
    assert!(
        !child.status.success(),
        "child admission unexpectedly succeeded"
    );
    let error = String::from_utf8_lossy(&child.stderr);
    assert!(error.contains("--enable-delegation"), "{error}");
    assert_eq!(
        store.tasks(100).unwrap().len(),
        before,
        "a refused child was admitted"
    );
    assert_eq!(launch["delegation_enabled"], false, "{launch}");
    json_output(&run(
        &fixture.home,
        &["cancel", &parent_task],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    let output = parent_process.wait_with_output().unwrap();
    assert!(!output.status.success() || String::from_utf8_lossy(&output.stdout).contains("lost"));
}

/// A level the harness cannot honour is refused before anything runs, and a
/// configured cap binds a task that asked for nothing instead of being ignored.
///
/// Workers always run with full permissions. A lower level asked for by an old
/// script, a config value or a stored task is accepted and ignored, and the
/// commands that used to set a cap are gone.
#[test]
fn a_requested_permission_never_lowers_a_worker() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:permission-test")];
    let ws = workspace.to_str().unwrap();

    let output = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            ws,
            "--foreground",
            "--permission",
            "read-only",
        ],
        &owner,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("ignored"));
    let ran = json_output(&output);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    assert_eq!(
        store_task(&home, ran["task_id"].as_str().unwrap()).permission,
        None
    );

    // The config key is accepted for old scripts and never applied.
    let set = run(&home, &["config", "set", "permission", "read-only"], &owner);
    assert!(set.status.success());
    let ran = json_output(&run(
        &home,
        &["run", "BRGR_FIXTURE_OK", "--workspace", ws, "--foreground"],
        &owner,
    ));
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    assert_eq!(
        store_task(&home, ran["task_id"].as_str().unwrap()).permission,
        None
    );

    // There is no cap to set any more.
    for command in ["set-max-permission", "clear-max-permission"] {
        let gone = run(&home, &["config", command, "edits"], &owner);
        assert!(!gone.status.success(), "{command} still exists");
    }
}

fn store_task(home: &Path, task: &str) -> brgr_protocol::TaskSpec {
    brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task.parse().unwrap())
        .unwrap()
}

/// A Claude Code stand-in for registration: it documents the recipe's flags and
/// answers a print-mode run.
const CLAUDE_FIXTURE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo '2.1.0 (Claude Code)'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--output-format <format>' '--model <model>' '--permission-mode <mode>' '--effort <level>'; exit 0;;
esac
echo CLAUDE_PRINT_MODE_OK
"#;

/// An `opencode` stand-in: its interactive command takes no effort flag, which
/// only `opencode run` accepts.
const OPENCODE_FIXTURE: &str = r#"#!/bin/sh
case "$1 $2" in
  '--version '*) echo '1.18.0'; exit 0;;
  'run --help') printf '%s\n' '--model' '--variant' '--agent' '--auto'; exit 0;;
  'models '*) echo 'opencode/big-pickle'; exit 0;;
esac
echo OPENCODE_PRINT_MODE_OK
"#;

/// An `OpenCode` 2 stand-in: `--variant` is gone, and a run outside a private
/// server (`--standalone`) would keep going in the shared service after brgr
/// stops the client, so this one refuses it.
const OPENCODE_2_FIXTURE: &str = r#"#!/bin/sh
case "$1 $2" in
  '--version '*) echo 'opencode v2.0.20'; exit 0;;
  'run --help') printf '%s\n' '  --standalone            Run with a private server instead of the background service' '  --model, -m string      Model to use in the format provider/model#variant' '  --agent string          Agent to use' '  --auto                  Auto-approve permissions that are not explicitly denied'; exit 0;;
esac
model=none standalone=no previous=
for arg in "$@"; do
  test "$previous" = --model && model=$arg
  test "$arg" = --standalone && standalone=yes
  previous=$arg
done
test "$standalone" = yes || { echo 'ran in the shared background service' >&2; exit 7; }
echo "OPENCODE_2_MODEL=$model"
"#;

/// A Codex stand-in: it documents the `exec` recipe's flags and answers a
/// print-mode run.
const CODEX_FIXTURE: &str = r#"#!/bin/sh
case "$1 $2" in
  '--version '*) echo 'codex-cli 0.0.0'; exit 0;;
  'exec --help') printf '%s\n' '--dangerously-bypass-approvals-and-sandbox' '--skip-git-repo-check' '--ephemeral' '-m, --model <MODEL>' '-c, --config <key=value>'; exit 0;;
esac
echo CODEX_PRINT_MODE_OK
"#;

/// A Herdr stand-in. It opens a pane, starts an agent, and — standing in for
/// the agent too — writes the report named in the prompt, then goes from
/// working to idle. Every call and the pane's environment are recorded beside
/// it, since the pane runner does not pass test variables through.
const PANE_HERDR_FIXTURE: &str = r#"#!/bin/sh
d="$(/usr/bin/dirname "$0")/herdr-state"
/bin/mkdir -p "$d"
seal_report() {
  /usr/bin/python3 - "$d/native-config.json" <<'PY'
import pathlib,json,subprocess,sys,os
c=json.loads(pathlib.Path(sys.argv[1]).read_text());e=dict(os.environ);e.update(c['environment']);revision=pathlib.Path(c['state']).name.split('-r')[1].split('.')[0]
p=subprocess.run([e['BRGR_BIN'],'--home',e['BRGR_HOME'],'__seal-report',e['BRGR_PARENT_TASK_ID'],revision],env=e,capture_output=True);assert p.returncode==0,p.stderr
PY
}
if [ "$1" = --session ]; then shift 2; fi
printf '%s\n' "$*" >> "$d/calls.log"
printf '%s ppid=%s %s\n' "$(/bin/date +%s)" "$PPID" "$*" >> "$d/calls.trace"
case "$1 $2" in
  'pane split'|'tab create')
    shift 2
    while [ $# -gt 0 ]; do
      case "$1" in
        --env) printf '%s\n' "$2" >> "$d/pane-env"; shift 2;;
        --cwd) printf '%s\n' "$2" > "$d/pane-cwd"; shift 2;;
        *) shift;;
      esac
    done
    echo '{"result":{"pane":{"pane_id":"w9:p2"}}}';;
  'pane run')
    /usr/bin/python3 - "$4" "$d" <<'PY'
import json,sys,shlex,pathlib,subprocess,os
config=json.loads(pathlib.Path(shlex.split(sys.argv[1])[-1]).read_text())
root=pathlib.Path(sys.argv[2]);pid=int(subprocess.check_output(['/bin/ps','-p',str(os.getppid()),'-o','ppid='],text=True).strip())
(root/'native-config.json').write_text(json.dumps(config))
birth=subprocess.check_output(['/bin/ps','-p',str(pid),'-o','lstart='],text=True).strip()

if (root/'start-fails').exists():
 print('registered native executable could not start',file=sys.stderr);sys.exit(9)
pathlib.Path(config['state']).write_text(json.dumps({'pid':pid,'birth':birth,'phase':'running','exit_code':None}))
kind=pathlib.Path(config['executable']).name
(root/'start-args').write_text('\n'.join(['agent','start','--kind',kind,*config['argv']]))
(root/'start-attempts').write_text('native host started once\n')
(root/'state').write_text('blocked' if (root/'start-blocked').exists() else 'idle')
PY
    test $? = 0 || exit 9
    echo '{"result":{"type":"ok"}}';;
  'pane layout')
    w=$(/bin/cat "$d/caller-width" 2>/dev/null || echo 200)
    printf '{"result":{"layout":{"panes":[{"pane_id":"w9:p1","rect":{"width":%s,"height":50}}]}}}\n' "$w";;
  'pane get') echo '{"result":{"pane":{"pane_id":"w9:p2","terminal_id":"terminal-native-fixture"}}}';;
  'pane send-text') printf '%s' "$4" > "$d/paste"; echo '{}';;
  'pane send-keys')
    /bin/rm -f "$d/start-blocked"
    if [ -s "$d/paste" ] && [ -e "$d/slow-seal" ]; then
      report=$(/usr/bin/grep -E '^/.*\.md$' "$d/paste" | /usr/bin/tail -1)
      ( /bin/sleep 4; test -z "$report" || printf '%s' PANE_REPORT_OK > "$report"; test -z "$report" || seal_report ) >/dev/null 2>&1 &
    elif [ -s "$d/paste" ] && [ ! -e "$d/no-report" ]; then
      report=$(/usr/bin/grep -E '^/.*\.md$' "$d/paste" | /usr/bin/tail -1)
      test -z "$report" || printf '%s' PANE_REPORT_OK > "$report"
      test -z "$report" || [ -e "$d/no-seal" ] || seal_report
    fi
    echo idle > "$d/state"
    echo '{"result":{"type":"ok"}}';;
  'agent start')
    printf '%s\n' "$@" >> "$d/start-attempts"
    if [ -s "$d/busy" ]; then
      n=$(/bin/cat "$d/busy"); if [ "$n" -gt 0 ]; then
        echo $((n - 1)) > "$d/busy"
        echo '{"error":{"code":"agent_pane_busy","message":"agent target pane w9:p2 is not an available shell"}}' >&2
        exit 1
      fi
    fi
    if [ -e "$d/start-blocked" ]; then
      /bin/rm "$d/start-blocked"
      echo idle > "$d/state"
      echo '{"error":{"code":"agent_not_ready","message":"agent is blocked during startup"}}' >&2
      exit 1
    fi
    if [ -e "$d/start-timeout" ]; then
      /bin/rm "$d/start-timeout"
      echo idle > "$d/state"
      echo '{"error":{"code":"timeout","message":"timed out waiting for agent startup"}}' >&2
      exit 1
    fi
    if [ -e "$d/start-fails" ]; then
      echo '{"error":{"code":"agent_kind_unknown","message":"no such agent kind"}}' >&2
      exit 1
    fi
    printf '%s\n' "$@" > "$d/start-args"
    echo idle > "$d/state"
    echo '{"result":{"agent":{"agent":"claude","agent_status":"idle","pane_id":"w9:p2"}}}';;
  'agent prompt')
    if [ -e "$d/prompt-refused" ]; then
      echo '{"error":{"code":"agent_not_ready","message":"agent w9:p2 is not an active named agent"}}' >&2
      exit 1
    fi
    if printf '%s' "$4" | /usr/bin/grep -q 'BRGR RESULT CHANNEL'; then
      /usr/bin/python3 - "$d/native-config.json" <<'PY'
import json,pathlib,sys,subprocess,shlex,os
c=json.loads(pathlib.Path(sys.argv[1]).read_text());settings=json.loads(pathlib.Path(c['argv'][c['argv'].index('--settings')+1]).read_text())
a=c['environment']['BRGR_PARENT_ATTEMPT_ID'];payload={'hook_event_name':'Stop','session_id':a,'cwd':c['workspace'],'last_assistant_message':f'BRGR_REPORT_BEGIN_{a}\nPANE_REPORT_OK\nBRGR_REPORT_END_{a}'}
env=dict(os.environ);env.update(c['environment']);command=settings['hooks']['Stop'][-1]['hooks'][0]['command']
p=subprocess.run(shlex.split(command),input=json.dumps(payload),text=True,env=env,capture_output=True);assert p.returncode==0,p.stderr
PY
      echo unknown > "$d/state"
      echo '{}'; exit 0
    fi
    if [ -e "$d/no-report" ]; then
      if [ -e "$d/change" ]; then
        # An agent that edits the worktree and then stops without its report.
        report=$(printf '%s\n' "$4" | /usr/bin/grep -E '^/.*\.md$' | /usr/bin/tail -1)
        root=$(/usr/bin/dirname "$(/usr/bin/dirname "$(/usr/bin/dirname "$(/usr/bin/dirname "$report")")")")
        test -z "$report" || printf '%s\n' unreported > "$root/changed.txt"
      fi
      echo idle > "$d/state"; echo '{"result":{"type":"ok"}}'; exit 0
    fi
    report=$(printf '%s\n' "$4" | /usr/bin/grep -E '^/.*\.md$' | /usr/bin/tail -1)
    if [ -n "$report" ]; then printf '%s' "$report" > "$d/report-path"; else report=$(/bin/cat "$d/report-path"); fi
    if [ -e "$d/hang" ]; then
      echo working > "$d/state.hang"
      echo '{"result":{"type":"ok"}}'
      exit 0
    fi
    if [ -e "$d/partial-report" ]; then
      printf '%s' PARTIAL_REPORT > "$report"
      echo unknown > "$d/state"; echo '{}'; exit 0
    fi
    printf '%s' 'PANE_REPORT_OK' > "$report"
    seal_report
    if [ -e "$d/unseen" ]; then echo idle > "$d/state"; else echo working > "$d/state"; fi
    echo '{"result":{"type":"ok"}}';;
  'agent get')
    if [ -e "$d/unclassified" ]; then
      echo '{"error":{"code":"agent_not_found","message":"native detection is not registered yet"}}' >&2; exit 1
    fi
    if [ -e "$d/state.hang" ]; then
      printf '{"result":{"agent":{"agent":"claude","agent_status":"%s","pane_id":"w9:p2"}}}\n' "$(/bin/cat "$d/state.hang")"
      exit 0
    fi
    s=$(/bin/cat "$d/state")
    printf '{"result":{"agent":{"agent":"claude","agent_status":"%s","pane_id":"w9:p2"}}}\n' "$s"
    if [ "$s" = working ]; then echo idle > "$d/state"; fi;;
  'pane read')
    if [ -e "$d/dialog" ] && [ -s "$d/paste" ]; then
      printf '%s\n' 'Pick a region' '› 1. us-east' '  2. eu-west'
      exit 0
    fi
    if [ -e "$d/blockquote" ] && [ -s "$d/paste" ]; then
      printf '%s\n' 'Working on it' '> 1. update the config, then skip later steps' '  2. next'
      exit 0
    fi
    n=$(/bin/cat "$d/menu" 2>/dev/null || echo 0)
    if [ "$n" -gt 0 ]; then
      echo $((n - 1)) > "$d/menu"
      printf '%s\n' '  Update available' '› 1. Update now' '  2. Skip'
    else
      if [ -e "$d/start-blocked" ]; then
        printf '%s\n' 'Claude Code fixture' 'Accessing workspace:' '' "$(/bin/cat "$d/pane-cwd")" '' '❯ No, exit' 'Yes, I trust this folder'
      else
        printf '%s\n' 'Claude Code fixture' '› Ask anything'
      fi
    fi;;
  'pane rename')
    if [ -e "$d/rename-fails" ]; then
      echo '{"error":{"code":"invalid_label","message":"the pane label is invalid"}}' >&2
      exit 1
    fi
    echo '{"result":{"type":"ok"}}';;
  'pane close') echo closed > "$d/closed"; echo '{"result":{"type":"ok"}}';;
  *) exit 2;;
esac
"#;

struct PaneFixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    herdr: PathBuf,
    state: PathBuf,
}

fn pane_fixture() -> PaneFixture {
    pane_fixture_named("brgr", "work")
}

fn pane_fixture_named(home: &str, workspace: &str) -> PaneFixture {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join(home);
    let workspace = temp.path().join(workspace);
    let bin = temp.path().join("bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("claude", CLAUDE_FIXTURE),
        ("codex", CODEX_FIXTURE),
        ("opencode", OPENCODE_FIXTURE),
        ("herdr", PANE_HERDR_FIXTURE),
    ] {
        write_fast_script(&bin.join(name), body);
    }
    add_fixture(&home, &bin.join("claude"), &temp.path().join("scratch"));
    add_fixture(&home, &bin.join("codex"), &temp.path().join("scratch"));
    add_fixture(&home, &bin.join("opencode"), &temp.path().join("scratch"));
    PaneFixture {
        home,
        workspace,
        herdr: bin.join("herdr"),
        state: bin.join("herdr-state"),
        _temp: temp,
    }
}

impl PaneFixture {
    fn run(&self, extra: &[&str]) -> Value {
        self.run_on("local.claude-code", extra)
    }

    fn run_with_env(&self, extra: &[&str], env: &[(&str, &str)]) -> Value {
        self.run_on_with_env("local.claude-code", extra, env)
    }

    fn run_on(&self, harness: &str, extra: &[&str]) -> Value {
        self.run_on_with_env(harness, extra, &[])
    }

    fn run_on_with_env(&self, harness: &str, extra: &[&str], more: &[(&str, &str)]) -> Value {
        let herdr = self.herdr.to_str().unwrap();
        let mut args = vec![
            "run",
            "Explain the build",
            "--harness",
            harness,
            "--workspace",
            self.workspace.to_str().unwrap(),
            "--foreground",
        ];
        args.extend_from_slice(extra);
        json_output(&run(
            &self.home,
            &args,
            &[
                &[
                    ("BRGR_OWNER_ID", "codex:pane-test"),
                    ("HERDR_ENV", "1"),
                    ("HERDR_PANE_ID", "w9:p1"),
                    ("HERDR_BIN_PATH", herdr),
                ][..],
                more,
            ]
            .concat(),
        ))
    }

    fn state(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }

    /// Runs `brgr status` until `done`, for up to the hang guard. A decision's
    /// cleanup that finds the agent busy leaves the pane to the next brgr
    /// command (the fake Herdr reports `working` to one status read and `idle`
    /// after it, and two brgr processes can run the same cleanup at once), so a
    /// test looks for the outcome by running a command instead of expecting it
    /// in the instant the deciding command returns.
    fn wait_for_cleanup(&self, owner: &[(&str, &str)], done: impl Fn() -> bool) -> bool {
        let started = Instant::now();
        loop {
            let _ = run(&self.home, &["status"], owner);
            if done() {
                return true;
            }
            if started.elapsed() >= HANG_GUARD {
                return false;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Bounded leftovers so a rare `.brgr` survival after accept names the files, the
/// supervisor process, and what decides whether a decision's cleanup may close
/// the pane: the fake agent's state, the pane receipt, the native host's state
/// and the last Herdr calls.
fn leftover_brgr_debug_with_herdr(
    workspace: &Path,
    home: &Path,
    task: &str,
    herdr_state: Option<&Path>,
) -> String {
    const LIMIT: usize = 6144;
    let mut out = String::new();
    out.push_str(".brgr listing:\n");
    list_tree(&mut out, &workspace.join(".brgr"), 0);
    out.push_str("supervisor:\n");
    describe_supervisor_processes(&mut out, home, task);
    if let Some(state) = herdr_state {
        let read = |name: &str| fs::read_to_string(state.join(name)).unwrap_or_default();
        let _ = writeln!(out, "fake agent state: {}", clipped(&read("state"), 40));
        let _ = writeln!(out, "closed marker: {:?}", clipped(&read("closed"), 40));
        let calls = read("calls.trace");
        let tail: Vec<&str> = calls.lines().rev().take(16).collect();
        let _ = writeln!(
            out,
            "last herdr calls (newest first; time, calling process id, call):"
        );
        for line in tail {
            let _ = writeln!(out, "  {}", clipped(line, 110));
        }
    }
    let runs = home.join("runs");
    for (label, name) in [
        ("pane receipt", format!("{task}-r1.pane.json")),
        ("native state", format!("{task}-r1.native-state.json")),
    ] {
        let text = fs::read_to_string(runs.join(name)).unwrap_or_default();
        let _ = writeln!(out, "{label}: {}", clipped(&text, 520));
    }
    if out.len() > LIMIT {
        out.truncate(LIMIT);
        out.push_str("\ntruncated");
    }
    out
}

fn clipped(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((idx, _)) => format!("{}…", &text[..idx]),
    }
}

fn list_tree(out: &mut String, path: &Path, depth: usize) {
    const MAX_DEPTH: usize = 6;
    const MAX_ENTRIES: usize = 32;
    if depth > MAX_DEPTH {
        out.push_str("  …\n");
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        let _ = writeln!(out, "  missing {}", path.display());
        return;
    };
    let kind = if meta.file_type().is_symlink() {
        "symlink"
    } else if meta.is_dir() {
        "dir"
    } else {
        "file"
    };
    let _ = writeln!(
        out,
        "{:indent$}{kind} {} {}",
        "",
        path.file_name().unwrap_or_default().to_string_lossy(),
        meta.len(),
        indent = depth.saturating_mul(2)
    );
    if !meta.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        out.push_str("  unreadable\n");
        return;
    };
    for (index, entry) in entries.filter_map(Result::ok).take(MAX_ENTRIES).enumerate() {
        if index + 1 == MAX_ENTRIES {
            out.push_str("  …\n");
            break;
        }
        list_tree(out, &entry.path(), depth + 1);
    }
}

fn describe_supervisor_processes(out: &mut String, home: &Path, task: &str) {
    let pid_path = home.join("runs").join(format!("{task}.pid"));
    let receipt_path = home.join("runs").join(format!("{task}.supervisor.json"));
    match fs::read_to_string(&pid_path) {
        Ok(text) => {
            let _ = writeln!(out, "pid-file: {}", clipped(&text, 128));
        }
        Err(_) => out.push_str("pid-file: absent\n"),
    }
    match fs::read_to_string(&receipt_path) {
        Ok(text) => {
            let _ = writeln!(out, "supervisor.json: {}", clipped(&text, 512));
        }
        Err(_) => out.push_str("supervisor.json: absent\n"),
    }
    let mut pids = Vec::new();
    if let Ok(text) = fs::read_to_string(&pid_path)
        && let Ok(pid) = text.trim().parse::<u32>()
        && pid > 1
    {
        pids.push(pid.to_string());
    }
    if let Ok(bytes) = fs::read(&receipt_path)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(handle) = value.pointer("/identity/handle").and_then(Value::as_str)
        && let Ok(pid) = handle.parse::<u32>()
        && pid > 1
        && !pids.iter().any(|seen| seen == handle)
    {
        pids.push(handle.to_owned());
    }
    if pids.is_empty() {
        out.push_str("ps: no supervisor pid recorded\n");
        return;
    }
    let output = Command::new("/bin/ps")
        .args([
            "-ww",
            "-p",
            &pids.join(","),
            "-o",
            "pid=,ppid=,stat=,lstart=,command=",
        ])
        .output();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = writeln!(
                out,
                "ps status={}: {}\n{}",
                output.status.code().unwrap_or(-1),
                clipped(&stdout, 1024),
                clipped(&stderr, 256)
            );
        }
        Err(error) => {
            let _ = writeln!(out, "ps: {error}");
        }
    }
}

/// The rendered name follows the split, before anything is sent to the agent.
#[test]
fn pane_mode_names_the_worker_pane_after_it_opens() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    let split = calls
        .find("pane split --pane w9:p1 --direction right")
        .unwrap_or_else(|| panic!("{calls}"));
    let rename = calls
        .find("pane rename w9:p2 claude-code/anthropic: Explain the build")
        .unwrap_or_else(|| panic!("the pane was not named: {calls}"));
    assert!(split < rename, "{calls}");
}

/// Herdr gives a pane the agent's own name when `agent start` finishes, which
/// replaces a label set before it, and may then overwrite that with `cmd` when
/// the agent registers. The name that stays is applied again after start.
#[test]
fn the_pane_name_is_set_again_after_the_agent_has_started() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    // The agent is started by `agent start`, or for a native TUI by `pane run`.
    let start = calls
        .find("agent start")
        .or_else(|| calls.find("pane run w9:p2"))
        .unwrap_or_else(|| panic!("the agent was never started: {calls}"));
    let after_start = &calls[start..];
    let rename = "pane rename w9:p2 claude-code/anthropic: Explain the build";
    let last_rename = calls
        .rfind(rename)
        .unwrap_or_else(|| panic!("the pane was not named: {calls}"));
    assert!(
        last_rename > start,
        "the last rename came before agent start and Herdr overwrites it: {calls}"
    );
    assert!(
        after_start.matches(rename).count() >= 2,
        "the name was not applied again after Herdr could overwrite it: {calls}"
    );
}

/// An empty template is the off switch: no rename, and the creation arguments
/// are the ones brgr used before naming existed.
#[test]
fn an_empty_pane_name_template_leaves_pane_creation_unchanged() {
    let fixture = pane_fixture();
    json_output(&run(
        &fixture.home,
        &["config", "set", "pane-name-template", ""],
        &[],
    ));
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    assert!(!calls.contains("pane rename"), "{calls}");
    assert!(
        calls.contains("pane split --pane w9:p1 --direction right"),
        "{calls}"
    );
    assert!(calls.contains("--no-focus"), "{calls}");
}

/// A new tab carries the name as its label instead of the fixed `brgr-work`.
#[test]
fn a_worker_tab_uses_the_rendered_pane_name_as_its_label() {
    let fixture = pane_fixture();
    json_output(&run(
        &fixture.home,
        &["config", "set-worker-placement", "tab"],
        &[],
    ));
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    assert!(
        calls
            .contains("tab create --workspace w9 --label claude-code/anthropic: Explain the build"),
        "{calls}"
    );
    assert!(!calls.contains("--label brgr-work"), "{calls}");
    assert!(
        calls.contains("pane rename w9:p2 claude-code/anthropic: Explain the build"),
        "{calls}"
    );
}

/// Naming is display only: a Herdr that refuses the label does not cost the run.
#[test]
fn a_failed_pane_rename_does_not_fail_the_run() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("rename-fails"), "").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let task = ran["task_id"].as_str().unwrap();
    let result = json_output(&run(
        &fixture.home,
        &["result", task],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    assert_eq!(result["artifacts"][0]["text"], "PANE_REPORT_OK", "{result}");
}

#[test]
fn the_pane_name_template_round_trips_through_config_set_and_show() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    json_output(&run(
        &home,
        &[
            "config",
            "set",
            "pane-name-template",
            "{harness}: {objective}",
        ],
        &[],
    ));
    let shown = json_output(&run(&home, &["config", "show"], &[]));
    assert_eq!(
        shown["herdr"]["pane_name_template"], "{harness}: {objective}",
        "{shown}"
    );
    json_output(&run(&home, &["config", "clear-pane-name-template"], &[]));
    let shown = json_output(&run(&home, &["config", "show"], &[]));
    assert_eq!(
        shown["herdr"]["pane_name_template"], "{harness}/{provider}/{model}: {objective}",
        "{shown}"
    );
    let bad = run(
        &home,
        &["config", "set", "pane-name-template", "{nope}"],
        &[],
    );
    assert!(!bad.status.success());
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("{objective}"), "{stderr}");
}

/// Inside Herdr, a harness with an interactive launch runs as its own TUI in a
/// pane beside the caller, and the report the agent writes is the result.
#[test]
fn pane_mode_runs_the_agent_in_a_pane_and_seals_its_report() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let task = ran["task_id"].as_str().unwrap();
    let result = json_output(&run(
        &fixture.home,
        &["result", task],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    assert_eq!(result["artifacts"][0]["text"], "PANE_REPORT_OK", "{result}");

    // Beside the caller, in the task workspace, and not stealing focus.
    let calls = fixture.state("calls.log");
    assert!(
        calls.contains("pane split --pane w9:p1 --direction right"),
        "{calls}"
    );
    assert!(calls.contains("--no-focus"), "{calls}");
    // The same full-permission flags a print-mode run would get.
    let start = fixture.state("start-args");
    assert!(start.contains("--kind\nclaude"), "{start}");
    assert!(
        start.contains("--permission-mode\nbypassPermissions"),
        "{start}"
    );
    // The agent can message its owner like any other worker.
    let env = fixture.state("pane-env");
    assert!(
        env.contains(&format!("BRGR_PARENT_TASK_ID={task}")),
        "{env}"
    );
    // Keep the completed TUI until its owner reviews the sealed result.
    assert!(fixture.workspace.join(".brgr").exists());
    assert!(fixture.state("closed").is_empty());
    json_output(&run(
        &fixture.home,
        &["accept", task],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    // The pane closes and the report is archived once the agent shows idle; a
    // loaded run showed two brgr processes running this cleanup at once, one
    // reading the fake's single `working` and backing off.
    let settled = fixture.wait_for_cleanup(&[("BRGR_OWNER_ID", "codex:pane-test")], || {
        !fixture.workspace.join(".brgr").exists() && fixture.state("closed").trim() == "closed"
    });
    assert!(
        settled,
        "{}",
        leftover_brgr_debug_with_herdr(
            &fixture.workspace,
            &fixture.home,
            task,
            Some(&fixture.state)
        )
    );
}

/// The failure report of the pane-cleanup test names what decides the cleanup, so
/// a rare failure under load can be explained instead of only repeated.
#[test]
fn the_pane_cleanup_failure_report_names_the_state_that_decides_it() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    let task = ran["task_id"].as_str().unwrap();
    let report = leftover_brgr_debug_with_herdr(
        &fixture.workspace,
        &fixture.home,
        task,
        Some(&fixture.state),
    );
    for needle in [
        ".brgr listing:",
        "supervisor:",
        "fake agent state:",
        "closed marker:",
        "last herdr calls (newest first; time, calling process id, call):",
        "ppid=",
        "pane receipt:",
        "native state:",
        "\"pane\"",
    ] {
        assert!(report.contains(needle), "missing {needle:?} in:\n{report}");
    }
}

/// A native run starts its host once.
#[test]
fn native_tui_bootstrap_starts_once() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate");
    assert_eq!(fixture.state("start-attempts").lines().count(), 1);
}

/// Herdr can recognize a native host's agent on screen yet refuse to prompt it,
/// because it only prompts agents it started and named (seen live with Fixture).
/// The prompt is then typed into the pane instead of the run being lost.
#[test]
fn a_prompt_herdr_refuses_for_a_native_host_is_typed_in_instead() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("prompt-refused"), "").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    assert!(calls.contains("pane send-text w9:p2"), "{calls}");
}

/// Herdr never classifies Fixture or Command Code, so their status is `unknown` for
/// the whole run. A report they wrote but never sealed is sealed once it has
/// stopped changing, instead of the run waiting for its deadline.
#[test]
fn an_unclassified_agent_that_wrote_but_did_not_seal_its_report_is_sealed() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    for flag in ["unclassified", "no-seal"] {
        fs::write(fixture.state.join(flag), "").unwrap();
    }
    let started = Instant::now();
    let ran = fixture.run_with_env(&[], &[("BRGR_TEST_REPORT_GRACE_MS", "400")]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    assert!(started.elapsed() < Duration::from_mins(1));
}

/// Herdr does not classify every harness (Fixture, Command Code), so a native run
/// can report `unknown` for its whole life. Output that looks like a menu then
/// must be left alone while the agent works.
#[test]
fn menu_like_output_from_an_unclassified_agent_is_not_pressed_mid_run() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    for flag in ["unclassified", "blockquote", "slow-seal"] {
        fs::write(fixture.state.join(flag), "").unwrap();
    }
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    assert_eq!(
        calls.matches("pane send-keys").count(),
        1,
        "only the prompt's own Enter may be sent: {calls}"
    );
}

/// An agent Herdr never classifies still gets the unknown-screen deadline: a
/// dialog that stands still is reported and fails the run, instead of waiting
/// out the hour-long task deadline. Nothing is pressed on it.
#[test]
fn an_unclassified_agent_stuck_on_an_unknown_dialog_fails_on_the_screen_deadline() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    for flag in ["unclassified", "dialog", "no-report"] {
        fs::write(fixture.state.join(flag), "").unwrap();
    }
    let started = Instant::now();
    let ran = fixture.run_with_env(
        &[],
        &[
            ("BRGR_TEST_SCREEN_DEADLINE_MS", "500"),
            ("BRGR_TEST_REPORT_GRACE_MS", "600000"),
        ],
    );
    assert_ne!(ran["outcome"], "candidate", "{ran}");
    let error = ran["error"].as_str().unwrap_or_default();
    assert!(error.contains("no brgr rule answers"), "{error}");
    assert!(error.contains("Pick a region"), "{error}");
    assert!(started.elapsed() < Duration::from_mins(1));
    let calls = fixture.state("calls.log");
    assert_eq!(
        calls.matches("pane send-keys").count(),
        1,
        "only the prompt's own Enter may be sent: {calls}"
    );
}

/// The default control home on macOS is `~/Library/Application Support/brgr`,
/// so every worktree path has a space. A pane run must survive one in both the
/// control home and the workspace (dogfood bug D4 was a space in that path).
#[test]
fn a_pane_run_works_with_spaces_in_the_home_and_workspace_paths() {
    let fixture = pane_fixture_named("Application Support/brgr", "work space");
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    assert!(
        fixture.state("pane-cwd").contains("work space"),
        "{}",
        fixture.state("pane-cwd")
    );
}

/// Codex runs a worker's tool commands in a shared daemon that keeps the
/// environment of whoever started it, so the pane launch carries the run's home
/// as session config instead (issue #362).
#[test]
fn a_codex_pane_worker_gets_the_control_home_as_session_config() {
    let fixture = pane_fixture();
    let ran = fixture.run_on("local.codex", &[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let args = fixture.state("start-args");
    let home = fixture.home.canonicalize().unwrap();
    let expected = format!(
        "-c\nshell_environment_policy.set.BRGR_HOME={}",
        serde_json::to_string(home.to_str().unwrap()).unwrap()
    );
    assert!(args.contains(&expected), "{args}");
    assert_eq!(
        args.matches("shell_environment_policy.set.").count(),
        1,
        "{args}"
    );
}

/// The same override survives a home path with a space and a double quote:
/// the value is JSON-quoted, so the pane's command line still parses.
#[test]
fn a_codex_pane_worker_gets_a_home_with_spaces_and_quotes() {
    let fixture = pane_fixture_named("Application Support/br\"gr", "work space");
    let ran = fixture.run_on("local.codex", &[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let args = fixture.state("start-args");
    let home = fixture.home.canonicalize().unwrap();
    let expected = format!(
        "shell_environment_policy.set.BRGR_HOME={}",
        serde_json::to_string(home.to_str().unwrap()).unwrap()
    );
    assert!(args.contains(&expected), "{args}");
}

/// A `revise` rebuilds the launch, so the second revision's Codex pane launch
/// carries the same home override.
#[test]
fn a_revised_codex_pane_worker_still_gets_the_home_override() {
    let fixture = pane_fixture();
    let ran = fixture.run_on("local.codex", &[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let task = ran["task_id"].as_str().unwrap();
    let herdr = [
        ("BRGR_OWNER_ID", "codex:pane-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
    ];
    json_output(&run(
        &fixture.home,
        &["reject", task, "--reason", "revise"],
        &herdr,
    ));
    // Not through `run`, which adds `--headless` to every revise.
    let revised = json_output(
        &brgr_command()
            .arg("--home")
            .arg(&fixture.home)
            .args(["--json", "revise", task, "Explain again", "--foreground"])
            .env("BRGR_SESSION_ID", "fixture-session")
            .envs(herdr)
            .output()
            .unwrap(),
    );
    assert_eq!(revised["outcome"], "candidate", "{revised}");
    assert_eq!(revised["revision"], 2);
    let args = fixture.state("start-args");
    assert!(
        args.contains("shell_environment_policy.set.BRGR_HOME="),
        "{args}"
    );
}

/// Only Codex needs its home as session config: another harness's pane launch
/// is unchanged.
#[test]
fn a_non_codex_pane_worker_has_no_home_config_override() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let args = fixture.state("start-args");
    assert!(!args.contains("shell_environment_policy"), "{args}");
}

#[test]
fn owned_native_editor_accepts_input_before_herdr_registers_the_agent() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("unclassified"), "").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    assert!(calls.contains("pane send-text w9:p2"), "{calls}");
    assert!(!calls.contains("agent prompt"));
    assert!(calls.contains("pane send-keys w9:p2 enter"));
    // A real native TUI remains alive after its collector exits. Give the
    // transport stand-in an independent lifetime for its cleanup check.
    let mut native = Command::new("/bin/sleep").arg("15").spawn().unwrap();
    let task = ran["task_id"].as_str().unwrap();
    let state_path = fixture
        .home
        .join("runs")
        .join(format!("{task}-r1.native-state.json"));
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["pid"] = native.id().into();
    state["birth"] = String::from_utf8(
        Command::new("/bin/ps")
            .args(["-p", &native.id().to_string(), "-o", "lstart="])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .into();
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    json_output(&run(
        &fixture.home,
        &["accept", ran["task_id"].as_str().unwrap()],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    assert!(
        fixture.wait_for_cleanup(&[("BRGR_OWNER_ID", "codex:pane-test")], || {
            fixture.state("closed").trim() == "closed"
        }),
        "the pane was never closed after the decision"
    );
    let _ = native.kill();
    let _ = native.wait();
}

#[test]
fn native_partial_file_is_not_a_completed_report() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("partial-report"), "").unwrap();
    let env = [
        ("BRGR_OWNER_ID", "codex:pane-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
    ];
    let launched = json_output(&run(
        &fixture.home,
        &[
            "run",
            "Partial report probe",
            "--harness",
            "local.claude-code",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &env,
    ));
    let task = launched["task_id"].as_str().unwrap();
    assert!(
        wait_until(|| fixture.state("calls.log").contains("agent prompt")),
        "the agent was never prompted: {}",
        fixture.state("calls.log")
    );
    // A pause only gives a wrong run the time to publish the partial file; a
    // correct run never does, however long this waits.
    thread::sleep(Duration::from_millis(1200));
    let result = run(&fixture.home, &["result", task], &env);
    assert!(
        !result.status.success(),
        "partial report was published: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    json_output(&run(&fixture.home, &["cancel", task], &env));
}

#[test]
fn native_conversation_waits_while_busy_then_delivers_once() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("hang"), "").unwrap();
    let env = [
        ("BRGR_OWNER_ID", "codex:pane-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
    ];
    let launched = json_output(&run(
        &fixture.home,
        &[
            "run",
            "Explain the build",
            "--harness",
            "local.claude-code",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &env,
    ));
    let task = launched["task_id"].as_str().unwrap();
    assert!(
        wait_until(|| fixture.state("calls.log").contains("agent prompt")),
        "the agent was never prompted: {}",
        fixture.state("calls.log")
    );
    json_output(&run(
        &fixture.home,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "note",
            "--body",
            "NATIVE_FOLLOWUP",
        ],
        &env,
    ));
    // The agent is held busy, so the follow-up must not be prompted however
    // long this waits; the pause only gives a wrong run the chance to send it.
    thread::sleep(Duration::from_millis(1200));
    let calls = fixture.state("calls.log");
    assert_eq!(calls.matches("agent prompt w9:p2").count(), 1);
    assert!(
        !calls.contains("pane send-text"),
        "busy TUI received raw input"
    );
    fs::remove_file(fixture.state.join("hang")).unwrap();
    fs::write(fixture.state.join("state.hang"), "idle").unwrap();
    let result = json_output(&run(
        &fixture.home,
        &["wait", task, "--timeout-seconds", "60"],
        &env,
    ));
    assert_eq!(result["outcome"], "candidate");
    let calls = fixture.state("calls.log");
    assert_eq!(calls.matches("agent prompt w9:p2").count(), 2, "{calls}");
    assert!(calls.contains("NATIVE_FOLLOWUP"));
    json_output(&run(&fixture.home, &["accept", task], &env));
}

/// An agent that starts but stops on a screen Herdr cannot classify — Cline's
/// product notice — times out as "not ready" rather than "blocked". The run
/// asks its owner to look at the pane and waits, instead of failing.
#[test]
fn native_tui_uses_the_registered_executable_and_full_arguments() {
    let fixture = pane_fixture();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate");
    let task = ran["task_id"].as_str().unwrap();
    let config: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .home
                .join("runs")
                .join(format!("{task}-r1.native-launch.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(config["executable"].as_str().unwrap().ends_with("/claude"));
    assert_eq!(config["argv"][0], "--permission-mode");
    assert_eq!(config["argv"][1], "bypassPermissions");
    assert!(
        !config["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "-p")
    );
}

/// An approval at startup — a folder trust or MCP prompt — is the owner's to
/// answer in the pane. Once the agent is ready the run withdraws its question,
/// so a finished result is not held back by one nobody needed to reply to.
#[test]
fn native_trust_for_the_owned_workspace_is_resolved_without_a_user_question() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("start-blocked"), "").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate");
    let messages = json_output(&run(
        &fixture.home,
        &[
            "message",
            "list",
            ran["task_id"].as_str().unwrap(),
            "--for",
            "owner",
        ],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    assert!(messages.as_array().unwrap().is_empty(), "{messages}");
    assert!(
        fixture
            .state("calls.log")
            .contains("pane send-keys w9:p2 down enter")
    );
}

/// The `opencode` TUI has no effort flag. A task that asks for an effort runs
/// headless, where the effort is honoured, rather than in a pane without it.
#[test]
fn unsupported_tui_effort_requires_an_explicit_headless_request() {
    let fixture = pane_fixture();
    let output = run(
        &fixture.home,
        &[
            "run",
            "Explain",
            "--harness",
            "local.opencode",
            "--effort",
            "high",
            "--mode",
            "auto",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w9:p1"),
            ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
            ("BRGR_OWNER_ID", "codex:pane-test"),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot honour --effort"));
    assert!(!fixture.state("calls.log").contains("pane split"));
    let headless = fixture.run_on("local.opencode", &["--effort", "high", "--headless"]);
    assert_eq!(headless["outcome"], "candidate");
    assert!(!fixture.state("calls.log").contains("pane split"));
    let paned = fixture.run_on("local.opencode", &[]);
    assert_eq!(paned["outcome"], "candidate");
    assert!(fixture.state("calls.log").contains("pane split"));
}

#[test]
fn auto_mode_with_a_herdr_pane_selects_pane_and_reports_why() {
    let fixture = pane_fixture();
    let receipt = fixture.run(&["--mode", "auto"]);
    assert_eq!(receipt["outcome"], "candidate");
    assert_eq!(receipt["mode"], "pane");
    assert_eq!(receipt["mode_reason"], "herdr pane available");
    assert!(fixture.state("calls.log").contains("pane split"));
}

#[test]
fn opencode_auto_falls_back_to_headless_and_explicit_pane_is_refused_before_admission() {
    let fixture = pane_fixture();
    let herdr = fixture.herdr.to_str().unwrap();
    let output = brgr_command()
        .arg("--home")
        .arg(&fixture.home)
        .args([
            "--json",
            "run",
            "Explain the build",
            "--harness",
            "local.opencode",
            "--mode",
            "auto",
            "--workspace",
        ])
        .arg(&fixture.workspace)
        .args(["--foreground"])
        .env_remove("BRGR_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("BRGR_TEST_TRUST_HERDR_PANE")
        .env("OPENCODE_SESSION_ID", "ses_opencode_fixture")
        .arg("--owner-session")
        .arg("ses_opencode_fixture")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", herdr)
        .output()
        .unwrap();
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["outcome"], "candidate", "{receipt}");
    assert_eq!(receipt["mode"], "headless", "{receipt}");
    assert_eq!(receipt["mode_reason"], "no herdr pane in this environment");
    assert!(receipt["source_pane"].is_null());
    assert!(!fixture.state("calls.log").contains("pane split"));
    let task = receipt["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(fixture.home.join("store")).unwrap();
    assert_eq!(
        store.task(task.parse().unwrap()).unwrap().owner_id.as_str(),
        "opencode:ses_opencode_fixture"
    );

    let explicit = brgr_command()
        .arg("--home")
        .arg(&fixture.home)
        .args([
            "--json",
            "run",
            "Refuse the build",
            "--harness",
            "local.opencode",
            "--mode",
            "pane",
            "--workspace",
        ])
        .arg(&fixture.workspace)
        .arg("--foreground")
        .env_remove("BRGR_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("BRGR_TEST_TRUST_HERDR_PANE")
        .env("OPENCODE_SESSION_ID", "ses_opencode_fixture")
        .arg("--owner-session")
        .arg("ses_opencode_fixture")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", herdr)
        .output()
        .unwrap();
    assert!(!explicit.status.success());
    let error = String::from_utf8_lossy(&explicit.stderr);
    assert!(error.contains("--mode headless"), "{error}");
    assert!(!error.contains("Codex"), "{error}");
    assert!(!error.contains("--as"), "{error}");
    let store = brgr_store::Store::open(fixture.home.join("store")).unwrap();
    assert_eq!(store.tasks(100).unwrap().len(), 1);
}

#[test]
fn codex_owner_session_without_thread_id_keeps_the_pending_pane_path() {
    let fixture = pane_fixture();
    let output = brgr_command()
        .arg("--home")
        .arg(&fixture.home)
        .args([
            "--json",
            "--as",
            "codex-session-fixture",
            "run",
            "Explain the build",
            "--harness",
            "local.claude-code",
            "--mode",
            "auto",
            "--workspace",
        ])
        .arg(&fixture.workspace)
        .env_remove("BRGR_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("BRGR_TEST_TRUST_HERDR_PANE")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap())
        .output()
        .unwrap();
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["state"], "starting", "{receipt}");
    assert_eq!(receipt["mode"], "pane", "{receipt}");
    let task = receipt["task_id"].as_str().unwrap();
    let launch: Value = serde_json::from_slice(
        &fs::read(fixture.home.join("launches").join(format!("{task}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(launch["source_pane"], "session:codex-session-fixture");
}

#[test]
fn revise_accepts_an_explicit_mode() {
    let fixture = pane_fixture();
    let initial = fixture.run_on("local.opencode", &["--headless"]);
    let task = initial["task_id"].as_str().unwrap();
    let herdr = [
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
        ("BRGR_OWNER_ID", "codex:pane-test"),
    ];
    json_output(&run(
        &fixture.home,
        &["reject", task, "--reason", "revise"],
        &herdr,
    ));
    let revised = brgr_command()
        .arg("--home")
        .arg(&fixture.home)
        .args([
            "--json",
            "revise",
            task,
            "Explain again",
            "--mode",
            "headless",
            "--foreground",
        ])
        .env("BRGR_SESSION_ID", "fixture-session")
        .envs(herdr)
        .output()
        .unwrap();
    let revised = json_output(&revised);
    assert_eq!(revised["revision"], 2);
    assert_eq!(revised["mode"], "headless");
    assert_eq!(revised["mode_reason"], "requested");
}

#[test]
fn extra_argv_cannot_bypass_a_harness_effort_selector() {
    let fixture = pane_fixture();
    json_output(&run(
        &fixture.home,
        &["config", "set", "harness", "local.opencode"],
        &[],
    ));
    json_output(&run(
        &fixture.home,
        &[
            "config",
            "set",
            "argv",
            "[\"--variant\",\"high\"]",
            "--harness",
            "local.opencode",
        ],
        &[],
    ));
    let check = run(&fixture.home, &["config", "check"], &[]);
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stderr).contains("--variant"));
    let output = run(
        &fixture.home,
        &[
            "run",
            "Explain",
            "--harness",
            "local.opencode",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w9:p1"),
            ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--variant"));
    assert!(!fixture.state("calls.log").contains("pane split"));
}

/// `OpenCode` 2 runs in a private server in both modes. It checks its own model
/// names in headless mode; its TUI host prepares a model-selected session.
#[test]
fn opencode_2_runs_standalone_and_selects_tui_models_through_a_session() {
    let fixture = pane_fixture();
    let opencode = fixture.herdr.with_file_name("opencode");
    write_fast_script(&opencode, OPENCODE_2_FIXTURE);
    add_fixture(
        &fixture.home,
        &opencode,
        &fixture.home.with_file_name("scratch-2"),
    );
    let herdr = [
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
        ("BRGR_OWNER_ID", "codex:pane-test"),
    ];
    let model_reached = |task: &str| {
        let result = json_output(&run(&fixture.home, &["result", task], &herdr));
        assert_eq!(
            result["artifacts"][0]["text"].as_str().map(str::trim),
            Some("OPENCODE_2_MODEL=opencode/big-pickle#high"),
            "{result}"
        );
    };
    let headless = fixture.run_on(
        "local.opencode",
        &["--headless", "--model", "opencode/big-pickle#high"],
    );
    assert_eq!(headless["outcome"], "candidate", "{headless}");
    let task = headless["task_id"].as_str().unwrap();
    model_reached(task);

    // A revision of it stays headless rather than asking the TUI for a model.
    json_output(&run(
        &fixture.home,
        &["reject", task, "--reason", "revise"],
        &herdr,
    ));
    // Not through `run`, which adds `--headless` to every revise.
    let revised = json_output(
        &brgr_command()
            .arg("--home")
            .arg(&fixture.home)
            .args(["--json", "revise", task, "Explain again", "--foreground"])
            .env("BRGR_SESSION_ID", "fixture-session")
            .envs(herdr)
            .output()
            .unwrap(),
    );
    assert_eq!(revised["outcome"], "candidate", "{revised}");
    assert_eq!(revised["revision"], 2);
    model_reached(task);
    let modelled = run(
        &fixture.home,
        &[
            "run",
            "Explain",
            "--harness",
            "local.opencode",
            "--model",
            "opencode/big-pickle#high",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w9:p1"),
            ("HERDR_BIN_PATH", fixture.herdr.to_str().unwrap()),
            ("BRGR_OWNER_ID", "codex:pane-test"),
        ],
    );
    let modelled = json_output(&modelled);
    assert_eq!(modelled["outcome"], "candidate", "{modelled}");
    let native: Value = serde_json::from_str(&fixture.state("native-config.json")).unwrap();
    assert_eq!(native["opencode_model"], "opencode/big-pickle#high");
    assert_eq!(native["argv"], json!(["--standalone", "--auto"]));
    let paned = fixture.run_on("local.opencode", &[]);
    assert_eq!(paned["outcome"], "candidate", "{paned}");
    let start = fixture.state("start-args");
    assert!(start.contains("--kind\nopencode\n--standalone"), "{start}");
    json_output(&run(
        &fixture.home,
        &[
            "config",
            "set",
            "model",
            "opencode/big-pickle#high",
            "--harness",
            "local.opencode",
        ],
        &herdr,
    ));
    json_output(&run(&fixture.home, &["config", "check"], &herdr));
    let configured = fixture.run_on("local.opencode", &[]);
    assert_eq!(configured["outcome"], "candidate", "{configured}");
    let native: Value = serde_json::from_str(&fixture.state("native-config.json")).unwrap();
    assert_eq!(native["opencode_model"], "opencode/big-pickle#high");
}

/// Herdr may never show an agent working: Cursor went from unknown straight to
/// idle once it had finished between two polls. The report is what completes
/// the run, so it must not wait for a working state it will never see.
#[test]
fn pane_mode_completes_on_the_report_without_ever_seeing_work() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("unseen"), "").unwrap();
    // A run that waited for the working state it never sees would wait out the
    // unseen-work grace, set here to ten minutes, so only such a run can reach
    // the one-minute bound; a loaded machine cannot.
    let started = Instant::now();
    let ran = fixture.run_with_env(&[], &[("BRGR_TEST_REPORT_GRACE_MS", "600000")]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    assert!(
        started.elapsed() < Duration::from_mins(1),
        "the run waited for a working state it never sees: {:?}",
        started.elapsed()
    );
}

/// Cancelling a pane-mode task kills its runner before the runner can close
/// its pane, so the supervisor closes the pane the runner recorded.
#[test]
fn cancelling_a_pane_mode_task_closes_its_pane() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("hang"), "").unwrap();
    let mut command = brgr_command();
    command
        .arg("--home")
        .arg(&fixture.home)
        .arg("--json")
        .args(["run", "Explain the build", "--harness", "local.claude-code"])
        .arg("--workspace")
        .arg(&fixture.workspace)
        .arg("--foreground")
        .env_remove("CODEX_THREAD_ID")
        .env("BRGR_SESSION_ID", "fixture-session")
        .env("BRGR_OWNER_ID", "codex:pane-test")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", &fixture.herdr)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = command.spawn().unwrap();
    assert!(
        wait_until(|| fixture.state("calls.log").contains("agent prompt")),
        "the agent was never prompted: {}",
        fixture.state("calls.log")
    );
    let task = fs::read_dir(fixture.home.join("launches"))
        .unwrap()
        .find_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json").map(str::to_owned)
        })
        .unwrap();
    json_output(&run(
        &fixture.home,
        &["cancel", &task],
        &[("BRGR_OWNER_ID", "codex:pane-test")],
    ));
    let output = child.wait_with_output().unwrap();
    let ran: Value = serde_json::from_slice(&output.stdout).unwrap();
    // Like every Herdr-backed run, a stopped one is recorded lost rather than
    // cancelled: its agent was outside the runner's process group.
    assert_eq!(ran["outcome"], "lost", "{ran}");
    assert_eq!(
        fixture.state("closed").trim(),
        "closed",
        "{}",
        fixture.state("calls.log")
    );
    let receipts: Vec<_> = fs::read_dir(fixture.home.join("runs"))
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".pane.json"))
        .collect();
    assert_eq!(receipts.len(), 1);
    let receipt: Value =
        serde_json::from_slice(&fs::read(fixture.home.join("runs").join(&receipts[0])).unwrap())
            .unwrap();
    assert_eq!(receipt["cleanup"], "closed");
}

/// A Herdr pane id this process does not provably run in — what a Codex tool
/// command inherits from its shared daemon — is never used to split beside a
/// pane. A caller that is not a Codex session has nowhere to be placed, and
/// the error says how to give it a source rather than suggesting no window.
#[test]
fn an_unverified_caller_is_never_split_beside_and_the_error_says_what_to_give() {
    let fixture = pane_fixture();
    let output = brgr_command()
        .arg("--home")
        .arg(&fixture.home)
        .arg("--json")
        .args([
            "run",
            "Explain",
            "--harness",
            "local.claude-code",
            "--foreground",
            "--workspace",
        ])
        .arg(&fixture.workspace)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_TEST_TRUST_HERDR_PANE")
        .env("BRGR_SESSION_ID", "fixture-session")
        .env("BRGR_OWNER_ID", "codex:pane-test")
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w9:p1")
        .env("HERDR_BIN_PATH", &fixture.herdr)
        .env("BRGR_TEST_CALLER_WAIT_MS", "300")
        .output()
        .unwrap();
    // A Codex session's pane is looked up after the call, so the run is
    // admitted; what must never happen is a split beside the claimed pane.
    assert!(
        !fixture
            .state("calls.log")
            .contains("pane split --pane w9:p1")
    );
    let _ = output;
}

/// Herdr may call an agent ready while it shows a menu — Codex opened on an
/// update offer. The prompt's Enter would pick the highlighted "Update now", so
/// the run first moves to the offer's skip option, and only then prompts.
#[test]
fn pane_mode_skips_an_update_offer_before_it_prompts() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    // Waiting for the shell to settle reads the screen three times first.
    fs::write(fixture.state.join("menu"), "6\n").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let calls = fixture.state("calls.log");
    let prompt = calls.find("agent prompt").unwrap();
    let skip = calls
        .find("pane send-keys w9:p2 down enter")
        .unwrap_or_else(|| panic!("the update offer was not skipped: {calls}"));
    assert!(skip < prompt, "prompted before skipping the offer: {calls}");
    assert!(
        !calls[..prompt].contains("pane send-keys w9:p2 enter"),
        "Enter alone would have chosen Update now: {calls}"
    );
}

/// A run that fails still closes the pane it opened, and says why it failed.
#[test]
fn pane_mode_failure_closes_the_pane_and_names_the_cause() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("start-fails"), "").unwrap();
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "lost", "{ran}");
    let error = ran["error"].as_str().unwrap();
    assert!(error.contains("native host bootstrap failed"), "{error}");
    assert!(
        error.contains("registered native executable could not start"),
        "{error}"
    );
    assert_eq!(fixture.state("closed").trim(), "closed");
}

/// A wide caller gets its worker beside it; a narrow one gets it below, so
/// repeated workers do not squeeze the caller down to nothing.
/// Any brgr command evens out a caller's workers that a manual close or a
/// terminal resize left uneven, and leaves panes brgr did not open alone.
#[test]
fn any_command_evens_out_an_uneven_worker_column() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let runs = home.join("runs");
    fs::create_dir_all(&runs).unwrap();
    let calls = temp.path().join("calls.log");
    let herdr = temp.path().join("herdr");
    write_fast_script(
        &herdr,
        &format!(
            r#"#!/bin/sh
echo "$*" >> '{calls}'
case "$1 $2" in
  'pane layout') echo '{{"result":{{"layout":{{"panes":[
    {{"pane_id":"w9:p1","rect":{{"x":0,"y":0,"width":104,"height":66}}}},
    {{"pane_id":"w9:p2","rect":{{"x":104,"y":0,"width":104,"height":50}}}},
    {{"pane_id":"w9:p3","rect":{{"x":104,"y":50,"width":104,"height":16}}}},
    {{"pane_id":"w9:p9","rect":{{"x":0,"y":66,"width":208,"height":7}}}}],
   "splits":[
    {{"direction":"right","ratio":0.5,"rect":{{"x":0,"y":0,"width":208,"height":66}}}},
    {{"direction":"down","ratio":0.76,"rect":{{"x":104,"y":0,"width":104,"height":66}}}}]}}}}}}';;
  *) echo '{{}}';;
esac
"#,
            calls = calls.display()
        ),
    );
    for (index, pane) in ["w9:p2", "w9:p3"].into_iter().enumerate() {
        fs::write(
            runs.join(format!(
                "00000000-0000-4000-8000-00000000000{index}-r1.pane.json"
            )),
            serde_json::to_vec(&json!({
                "pane": pane,
                "binary": herdr,
                "session": null,
                "cleanup": "pending",
                "caller": "w9:p1",
            }))
            .unwrap(),
        )
        .unwrap();
    }

    json_output(&run(&home, &["status"], &[]));
    let log = fs::read_to_string(&calls).unwrap();
    assert!(
        log.contains("pane resize --pane w9:p3 --direction up --amount 0.2600"),
        "{log}"
    );
    assert!(!log.contains("w9:p9 --direction"), "{log}");
}

#[test]
fn a_narrow_caller_gets_its_worker_below_and_a_wide_one_beside() {
    for (width, direction) in [("200", "--direction right"), ("60", "--direction down")] {
        let fixture = pane_fixture();
        fs::create_dir_all(&fixture.state).unwrap();
        fs::write(fixture.state.join("caller-width"), width).unwrap();
        let ran = fixture.run(&[]);
        assert_eq!(ran["outcome"], "candidate", "{ran}");
        let calls = fixture.state("calls.log");
        assert!(calls.contains(direction), "width {width}: {calls}");
    }
}

/// An agent that stops without ever writing its report is reminded once and
/// then fails with that cause, instead of the run waiting for its deadline.
#[test]
fn an_agent_that_never_writes_its_report_fails_with_the_cause() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("no-report"), "").unwrap();
    let started = Instant::now();
    let ran = fixture.run_with_env(&[], &[("BRGR_TEST_REPORT_GRACE_MS", "300")]);
    assert_eq!(ran["outcome"], "failed", "{ran}");
    let error = ran["error"].as_str().unwrap();
    assert!(
        error.contains("finished without writing its report"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_mins(1));
    let calls = fixture.state("calls.log");
    assert_eq!(
        calls.matches("agent prompt").count(),
        2,
        "the task prompt plus exactly one reminder: {calls}"
    );
}

/// `--include-ignored` reaches the reclaim that a pane's own cleanup runs when a
/// decision lands. In pane mode that cleanup can reclaim the worktree before the
/// deciding command does, and it used to assess it without the flag and mark it
/// assessed for good.
#[test]
fn include_ignored_reaches_the_reclaim_a_pane_cleanup_runs() {
    let fixture = pane_fixture();
    seed_git_repo(&fixture.workspace);
    fs::write(fixture.workspace.join(".gitignore"), ".env\n").unwrap();
    for argv in [&["add", ".gitignore"][..], &["commit", "-m", "ignore"][..]] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&fixture.workspace)
                .args(argv)
                .status()
                .unwrap()
                .success()
        );
    }
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate", "{ran}");
    let task = ran["task_id"].as_str().unwrap();
    let herdr = fixture.herdr.to_str().unwrap();
    let env = [
        ("BRGR_OWNER_ID", "codex:pane-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", herdr),
    ];
    let status = json_output(&run(&fixture.home, &["status", task], &env));
    let worktree = PathBuf::from(status["workspace"].as_str().unwrap());
    assert!(worktree.is_dir(), "{status}");
    fs::write(worktree.join(".env"), "CACHE=1\n").unwrap();

    json_output(&run(
        &fixture.home,
        &["accept", task, "--reason", "verified", "--include-ignored"],
        &env,
    ));
    assert!(
        fixture.wait_for_cleanup(&env, || !worktree.exists()),
        "--include-ignored did not reach the pane cleanup's reclaim"
    );
}

/// A Codex worker's commands run in a shared daemon that lost `BRGR_HOME`. Its
/// worktree's path still names the home. A home someone chose, by `--home` or
/// `BRGR_HOME`, still wins.
#[test]
fn a_command_from_a_worker_directory_uses_the_workers_home() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let home = root.join("custom");
    let elsewhere = root.join("elsewhere");
    let worktree = home.join("worktrees/repo/abc");
    let task = worktree.join(".brgr/tasks/2b2c279a-b8e1-43a8-846c-cb8344a301d7-r1");
    fs::create_dir_all(&task).unwrap();
    fs::create_dir_all(home.join("store")).unwrap();
    let brgr = || {
        let mut command = common::brgr_command();
        command
            .current_dir(&worktree)
            .env_remove("BRGR_HOME")
            .env("HOME", root.join("user"));
        command
    };
    let default_home = brgr().args(["config", "init"]).output().unwrap();
    assert!(default_home.status.success(), "{default_home:?}");
    assert!(home.join("config.toml").is_file());

    let typed = brgr()
        .args(["--home", elsewhere.to_str().unwrap(), "config", "init"])
        .output()
        .unwrap();
    assert!(typed.status.success(), "{typed:?}");
    assert!(elsewhere.join("config.toml").is_file());

    let exported = root.join("exported");
    let from_environment = brgr()
        .env("BRGR_HOME", &exported)
        .args(["config", "init"])
        .output()
        .unwrap();
    assert!(from_environment.status.success(), "{from_environment:?}");
    assert!(exported.join("config.toml").is_file());
}

/// A worker that changed files and then never wrote its report has failed, but
/// its work stays reviewable: the worktree diff is sealed with the result.
#[test]
fn a_run_without_a_report_keeps_its_worktree_diff_reviewable() {
    let fixture = pane_fixture();
    seed_git_repo(&fixture.workspace);
    fs::create_dir_all(&fixture.state).unwrap();
    for flag in ["no-report", "change"] {
        fs::write(fixture.state.join(flag), "").unwrap();
    }
    let ran = fixture.run_with_env(&["--capture-diff"], &[("BRGR_TEST_REPORT_GRACE_MS", "300")]);
    assert_eq!(ran["outcome"], "failed", "{ran}");
    let task = ran["task_id"].as_str().unwrap();
    let owner = [("BRGR_OWNER_ID", "codex:pane-test")];
    let shown = run(&fixture.home, &["diff", task], &owner);
    let diff = String::from_utf8(shown.stdout).unwrap();
    assert!(
        diff.contains("changed.txt"),
        "stdout: {diff}\nstderr: {}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(diff.contains("+unreported"), "{diff}");
    // The change cannot be accepted: only a candidate can.
    let accepted = run(&fixture.home, &["accept", task, "--reason", "x"], &owner);
    assert!(!accepted.status.success());
}

/// A lost result has no candidate to accept or reject, so it can be revised
/// without a decision; revising it is the owner's answer to it. An accepted
/// result still cannot be.
#[test]
fn a_lost_result_can_be_revised_without_a_decision() {
    let fixture = pane_fixture();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::write(fixture.state.join("start-fails"), "").unwrap();
    let lost = fixture.run(&[]);
    assert_eq!(lost["outcome"], "lost", "{lost}");
    let task = lost["task_id"].as_str().unwrap();
    fs::remove_file(fixture.state.join("start-fails")).unwrap();

    let herdr = fixture.herdr.to_str().unwrap();
    let env = [
        ("BRGR_OWNER_ID", "codex:pane-test"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w9:p1"),
        ("HERDR_BIN_PATH", herdr),
    ];
    let revised = run(
        &fixture.home,
        &["revise", task, "Explain again", "--foreground"],
        &env,
    );
    let stderr = String::from_utf8_lossy(&revised.stderr).into_owned();
    assert!(stderr.contains("the previous run was lost"), "{stderr}");
    let revised = json_output(&revised);
    assert_eq!(revised["outcome"], "candidate", "{revised}");
    assert_eq!(revised["revision"], 2);

    // The revision answered the lost result: nothing is left to acknowledge.
    let status = run(&fixture.home, &["status", task], &env);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(!stderr.contains("not acknowledged"), "{stderr}");

    // An accepted candidate is still final.
    let revised_id = revised["task_id"].as_str().unwrap();
    json_output(&run(&fixture.home, &["accept", revised_id], &env));
    let again = run(
        &fixture.home,
        &["revise", revised_id, "Once more", "--foreground"],
        &env,
    );
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("only a rejected result can be revised")
    );
}

/// A read-only request no longer turns the TUI off or into plan mode: the
/// worker runs in its own pane with full permissions.
#[test]
fn a_read_only_request_still_runs_the_tui_with_full_permissions() {
    let fixture = pane_fixture();
    let ran = fixture.run(&["--permission", "read-only"]);
    assert_eq!(ran["outcome"], "candidate");
    assert!(fixture.state("calls.log").contains("pane split"));
    let args = fixture.state("start-args");
    assert!(args.contains("bypassPermissions"), "{args}");
    assert!(!args.contains("plan"), "{args}");
    json_output(&run(
        &fixture.home,
        &["config", "set-pane-mode", "false"],
        &[],
    ));
    let ran = fixture.run(&[]);
    assert_eq!(ran["outcome"], "candidate");
    assert_eq!(
        fixture
            .state("start-args")
            .lines()
            .filter(|line| *line == "bypassPermissions")
            .count(),
        1
    );
}

const RECURSIVE_FIXTURE_FIXTURE: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) echo 'fixture v-recursive-fixture'; exit 0;;
  --help)
    printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'
    exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
test -f "$prompt_file"
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}"; then
  if /usr/bin/grep -q UNSETTLED "$prompt_file"; then
    "$BRGR_BIN" --json --headless run LEAF --harness local.fixture --workspace "$PWD" --foreground >/dev/null
  elif /usr/bin/grep -q CANCEL_CHILD "$prompt_file"; then
    child_json=$("$BRGR_BIN" --json --headless run SLOW --harness local.fixture --workspace "$PWD")
    child_task=$(printf '%s\n' "$child_json" | /usr/bin/sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
    test -n "$child_task"
    "$BRGR_BIN" --json cancel "$child_task" >/dev/null
    "$BRGR_BIN" --json wait "$child_task" --timeout-seconds 60 >/dev/null
  elif /usr/bin/grep -q ROOT "$prompt_file"; then
    child_json=$("$BRGR_BIN" --json --headless run CHILD --harness local.fixture --workspace "$PWD")
    child_task=$(printf '%s\n' "$child_json" | /usr/bin/sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
    test -n "$child_task"
    "$BRGR_BIN" --json wait "$child_task" --timeout-seconds 120 >/dev/null
    "$BRGR_BIN" --json result "$child_task" >/dev/null
    "$BRGR_BIN" --json accept "$child_task" --reason 'child artifact checked' >/dev/null
  elif /usr/bin/grep -q CHILD "$prompt_file"; then
    child_json=$("$BRGR_BIN" --json --headless run LEAF --harness local.fixture --workspace "$PWD")
    child_task=$(printf '%s\n' "$child_json" | /usr/bin/sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
    test -n "$child_task"
    "$BRGR_BIN" --json wait "$child_task" --timeout-seconds 120 >/dev/null
    "$BRGR_BIN" --json result "$child_task" >/dev/null
    "$BRGR_BIN" --json accept "$child_task" --reason 'leaf artifact checked' >/dev/null
  fi
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_RECURSIVE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn recursive_fixture_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&executable, RECURSIVE_FIXTURE_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    (temp, home, workspace)
}

#[test]
fn an_owner_can_read_all_descendants_after_parent_workers_exit() {
    let (_temp, home, workspace) = recursive_fixture_home();

    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(root["outcome"], "candidate", "{root}");
    let root_task = root["task_id"].as_str().unwrap();
    let waited = json_output(&run(
        &home,
        &["wait", root_task, "--timeout-seconds", "2"],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(waited["outcome"], "candidate");
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let root_id = root_task.parse().unwrap();
    let tasks = store.tasks(10).unwrap();
    assert_eq!(tasks.len(), 3);
    let child = tasks.iter().find(|task| task.objective == "CHILD").unwrap();
    let leaf = tasks.iter().find(|task| task.objective == "LEAF").unwrap();
    let (child_parent, child_attempt, child_depth) =
        store.delegation_parent(child.task_id).unwrap().unwrap();
    assert_eq!(child_parent, root_id);
    assert_eq!(child_depth, 1);
    let (leaf_parent, _, leaf_depth) = store.delegation_parent(leaf.task_id).unwrap().unwrap();
    assert_eq!(leaf_parent, child.task_id);
    assert_eq!(leaf_depth, 2);
    assert_eq!(child.owner_id.as_str(), format!("worker:{child_attempt}"));
    assert_eq!(store.unsettled_children(child_attempt).unwrap(), 0);
    let owner = [("BRGR_OWNER_ID", "codex:recursive-root")];
    let brief = json_output(&run(&home, &["status", "--brief"], &owner));
    assert_eq!(brief.as_array().unwrap().len(), 3, "{brief}");
    let brief_child = brief
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["task_id"] == child.task_id.to_string())
        .unwrap();
    assert_eq!(brief_child["parent_task_id"], root_task);
    assert_eq!(brief_child["harness"], "local.fixture");
    assert!(brief_child["model"].is_null());
    let status = json_output(&run(&home, &["status"], &owner));
    assert_eq!(status.as_array().unwrap().len(), 3, "{status}");
    let status_leaf = status
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["task"]["task_id"] == leaf.task_id.to_string())
        .unwrap();
    assert_eq!(status_leaf["parent_task_id"], child.task_id.to_string());
    assert_eq!(status_leaf["task"]["route"]["harness_id"], "local.fixture");
    assert_eq!(status_leaf["task"]["route"]["requested_model"], Value::Null);
    let child_task_id = child.task_id.to_string();
    let child_status = json_output(&run(&home, &["status", &child_task_id], &owner));
    assert_eq!(child_status["parent_task_id"], root_task);
    let child_result = json_output(&run(&home, &["result", &child_task_id], &owner));
    assert_eq!(child_result["artifacts"][0]["text"], "BRGR_RECURSIVE_OK");
    let cannot_decide = run(&home, &["accept", &child_task_id], &owner);
    assert!(
        !cannot_decide.status.success(),
        "ancestor owner acquired decision rights"
    );
    let root_result = json_output(&run(
        &home,
        &["result", root_task],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(root_result["artifacts"][0]["text"], "BRGR_RECURSIVE_OK");
}

#[test]
fn cancelled_children_settle_but_undecided_results_name_their_commands() {
    let (_temp, home, workspace) = recursive_fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:recursive-root")];
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let unsettled = json_output(&run(
        &home,
        &[
            "run",
            "UNSETTLED",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(unsettled["outcome"], "failed");
    let unsettled_root_id = unsettled["task_id"].as_str().unwrap().parse().unwrap();
    let unsettled_child = store
        .subtree(unsettled_root_id)
        .unwrap()
        .into_iter()
        .find(|node| node.depth == 1)
        .map(|node| store.task(node.task_id).unwrap())
        .unwrap();
    let error = unsettled["error"].as_str().unwrap();
    assert!(
        error.contains(&unsettled_child.task_id.to_string()),
        "{error}"
    );
    assert!(
        error.contains(&format!("brgr accept {}", unsettled_child.task_id)),
        "{error}"
    );

    let cancelled_parent = json_output(&run(
        &home,
        &[
            "run",
            "CANCEL_CHILD",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(
        cancelled_parent["outcome"], "candidate",
        "{cancelled_parent}"
    );
    let parent_id = cancelled_parent["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let cancelled_child = store
        .subtree(parent_id)
        .unwrap()
        .into_iter()
        .find(|node| node.depth == 1)
        .map(|node| store.task(node.task_id).unwrap())
        .unwrap();
    assert_eq!(
        store
            .latest_result(cancelled_child.task_id)
            .unwrap()
            .outcome,
        brgr_protocol::TerminalOutcome::Cancelled
    );
    let parent_attempt = store.latest_message_attempt(parent_id).unwrap();
    assert_eq!(store.unsettled_children(parent_attempt).unwrap(), 0);
}

#[test]
fn recursive_worker_uses_sibling_worktrees_from_a_git_parent() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    write_fast_script(&executable, RECURSIVE_FIXTURE_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.fixture",
            "--workspace",
            repository.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:git-recursive")],
    ));
    assert_eq!(root["outcome"], "candidate");
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let tasks = store.tasks(10).unwrap();
    assert_eq!(tasks.len(), 3);
    let parent = home.canonicalize().unwrap().join("worktrees/repo");
    assert!(
        tasks
            .iter()
            .all(|task| Path::new(&task.workspace).starts_with(&parent)),
        "workspaces: {:?}",
        tasks.iter().map(|task| &task.workspace).collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n"
    );
}

#[test]
fn rejected_delegated_child_revises_inside_parent_git_worktree() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let root_owner = [("BRGR_OWNER_ID", "codex:git-revise")];
    let root = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            repository.to_str().unwrap(),
            "--enable-delegation",
            "--deadline-seconds",
            "20",
        ],
        &root_owner,
    ));
    let root_task = root["task_id"].as_str().unwrap();
    let root_id = root_task.parse().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let root_attempt = active_attempt(&store, root_task);
    let parent_workspace = store.task(root_id).unwrap().workspace;
    assert!(Path::new(&parent_workspace).starts_with(home.canonicalize().unwrap()));
    let worker_owner = format!("worker:{root_attempt}");
    let root_attempt_text = root_attempt.to_string();
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", root_task),
        ("BRGR_PARENT_ATTEMPT_ID", root_attempt_text.as_str()),
    ];
    let child = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            &parent_workspace,
            "--foreground",
        ],
        &worker,
    ));
    assert_eq!(child["outcome"], "candidate");
    let child_task = child["task_id"].as_str().unwrap();
    assert_eq!(
        json_output(&run(
            &home,
            &["reject", child_task, "--reason", "revise fixture"],
            &worker
        ))["verdict"],
        "rejected"
    );
    let revised = json_output(&run(
        &home,
        &[
            "revise",
            child_task,
            "BRGR_FIXTURE_OK",
            "--workspace",
            &parent_workspace,
            "--foreground",
        ],
        &worker,
    ));
    assert_eq!(revised["outcome"], "candidate");
    assert_eq!(revised["revision"], 2);
    let child_id = child_task.parse().unwrap();
    assert_eq!(
        store.delegation_parent(child_id).unwrap(),
        Some((root_id, root_attempt, 1))
    );
    assert_eq!(store.unsettled_children(root_attempt).unwrap(), 1);
    assert_eq!(
        json_output(&run(&home, &["accept", child_task], &worker))["verdict"],
        "accepted"
    );
    assert_eq!(store.unsettled_children(root_attempt).unwrap(), 0);
    json_output(&run(&home, &["cancel", root_task], &root_owner));
    let terminal = json_output(&run(
        &home,
        &["wait", root_task, "--timeout-seconds", "60"],
        &root_owner,
    ));
    assert_eq!(terminal["outcome"], "cancelled");
}

#[test]
fn tree_status_shows_question_and_tree_cancel_stops_both_processes() {
    let (_temp, home, workspace) = fixture_home();
    let root_owner = [("BRGR_OWNER_ID", "codex:tree-cancel")];
    let root = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
        ],
        &root_owner,
    ));
    let root_task = root["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let root_attempt = active_attempt(&store, root_task);
    let worker_owner = format!("worker:{root_attempt}");
    let root_attempt_text = root_attempt.to_string();
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", root_task),
        ("BRGR_PARENT_ATTEMPT_ID", root_attempt_text.as_str()),
    ];
    let child = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &worker,
    ));
    let child_task = child["task_id"].as_str().unwrap();
    active_attempt(&store, child_task);
    json_output(&run(
        &home,
        &[
            "message",
            "send",
            child_task,
            "--to",
            "worker",
            "--kind",
            "question",
            "--body",
            "Need direction",
        ],
        &worker,
    ));
    let tree = json_output(&run(&home, &["status", root_task, "--tree"], &root_owner));
    assert_eq!(tree["nodes"].as_array().unwrap().len(), 2);
    let child_row = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["task_id"] == child_task)
        .unwrap();
    assert_eq!(child_row["waiting_for"], "question");
    assert!(child_row["remaining_seconds"].as_i64().is_some());
    let cancelled = json_output(&run(&home, &["cancel", root_task, "--tree"], &root_owner));
    assert_eq!(cancelled["requested"].as_array().unwrap().len(), 2);
    let child_terminal = json_output(&run(
        &home,
        &["wait", child_task, "--timeout-seconds", "60"],
        &worker,
    ));
    assert_eq!(child_terminal["outcome"], "cancelled", "{child_terminal}");
    assert_eq!(
        json_output(&run(
            &home,
            &["wait", root_task, "--timeout-seconds", "60"],
            &root_owner
        ))["outcome"],
        "cancelled"
    );
}

#[test]
fn cancelling_a_parent_stops_a_child_process_group_and_receipts_both_tasks() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    let child_json_path = temp.path().join("child.json");
    let ping_pid_path = temp.path().join("ping.pid");
    fs::create_dir_all(&workspace).unwrap();
    let script = format!(
        r#"#!/bin/sh
set -eu
case "${{1:-}}" in
  --version) echo 'fixture v-cancel-group'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do case "$argument" in @*) prompt_file=${{argument#@}};; esac; done
if /usr/bin/grep -q '^ROOT' "$prompt_file"; then
  child_json=$("$BRGR_BIN" --json --headless run CHILD --harness local.fixture --workspace "$PWD")
  printf '%s\n' "$child_json" > "{}"
  while :; do /bin/sleep 1; done
elif /usr/bin/grep -q '^CHILD' "$prompt_file"; then
  /bin/sleep 120 &
  ping_pid=$!
  printf '%s\n' "$ping_pid" > "{}"
  wait "$ping_pid"
fi
printf '%s\n' '{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"cancel fixture completed"}}]}}}}'
printf '%s\n' '{{"type":"agent_end","stopReason":"completed"}}'
"#,
        child_json_path.display(),
        ping_pid_path.display()
    );
    write_fast_script(&executable, &script);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:cancel-group")];
    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
        ],
        &owner,
    ));
    let root_task = root["task_id"].as_str().unwrap();
    assert!(
        wait_until(|| file_is_nonempty(&child_json_path) && file_is_nonempty(&ping_pid_path)),
        "the nested task did not start the process: {}",
        fs::read_to_string(&child_json_path).unwrap_or_default()
    );
    let child_json: Value = serde_json::from_slice(&fs::read(&child_json_path).unwrap()).unwrap();
    let child_task = child_json["task_id"].as_str().unwrap();
    let ping_pid = fs::read_to_string(&ping_pid_path)
        .unwrap()
        .trim()
        .to_owned();
    assert!(process_exists(&ping_pid), "the child process did not start");
    let cancelled = json_output(&run(&home, &["cancel", root_task], &owner));
    assert_eq!(cancelled["state"], "cancel_requested", "{cancelled}");
    let stopped: Vec<String> = serde_json::from_value(cancelled["stopped_tasks"].clone()).unwrap();
    assert!(stopped.iter().any(|task| task == root_task), "{cancelled}");
    assert!(stopped.iter().any(|task| task == child_task), "{cancelled}");
    assert!(
        wait_until(|| !process_exists(&ping_pid)),
        "the child process group survived parent cancellation (pid {ping_pid})"
    );
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(wait_until(|| store
        .attempt_state(child_task.parse().unwrap())
        .is_ok_and(
            |state| state == brgr_protocol::AttemptState::Terminal
        )));
}

fn file_is_nonempty(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

#[test]
fn committed_cancellation_intent_stops_running_worker_without_cancel_file() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:cancel-intent")];
    let task = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ))["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let task_id = task.parse().unwrap();
    active_attempt(&store, &task);
    store.record_cancellation_intents(task_id, false).unwrap();
    let result = json_output(&run(
        &home,
        &["wait", &task, "--timeout-seconds", "60"],
        &owner,
    ));
    assert_eq!(result["outcome"], "cancelled");
}

#[test]
fn terminal_tree_cancellation_does_not_cancel_a_later_revision() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:cancel-revision")];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(first["outcome"], "candidate");
    let task = first["task_id"].as_str().unwrap();
    json_output(&run(&home, &["cancel", task, "--tree"], &owner));
    json_output(&run(&home, &["reject", task, "--reason", "revise"], &owner));
    let revised = json_output(&run(
        &home,
        &["revise", task, "BRGR_FIXTURE_OK", "--foreground"],
        &owner,
    ));
    assert_eq!(revised["outcome"], "candidate");
    assert_eq!(revised["revision"], 2);
}

#[test]
fn relative_control_home_is_canonical_before_worker_delegation() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("relative-home");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&executable, RECURSIVE_FIXTURE_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let output = brgr_command()
        .current_dir(temp.path())
        .args([
            "--home",
            "relative-home",
            "--json",
            "--headless",
            "run",
            "ROOT",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ])
        .env("BRGR_SESSION_ID", "fixture-session")
        .env("BRGR_OWNER_ID", "codex:relative-home")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    let result = json_output(&output);
    assert_eq!(result["outcome"], "candidate");
    assert_eq!(
        brgr_store::Store::open(home.join("store"))
            .unwrap()
            .tasks(10)
            .unwrap()
            .len(),
        3
    );
}

struct MessageFixture {
    _temp: TempDir,
    home: PathBuf,
    task: String,
    attempt: String,
}

fn start_message_fixture() -> MessageFixture {
    let (temp, home, workspace) = fixture_home();
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
        ],
        &[("BRGR_OWNER_ID", "codex:message-test")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = active_attempt(&store, task);
    MessageFixture {
        _temp: temp,
        home,
        task: task.to_owned(),
        attempt: attempt.to_string(),
    }
}

fn worker_question_roundtrip(
    home: &Path,
    task: &str,
    owner: &[(&str, &str)],
    worker: &[(&str, &str)],
) {
    let question = json_output(&run(
        home,
        &[
            "message",
            "send",
            task,
            "--to",
            "owner",
            "--kind",
            "question",
            "--body",
            "Which token?",
        ],
        worker,
    ));
    let question_id = question["message_id"].as_str().unwrap();
    let received = json_output(&run(
        home,
        &[
            "message",
            "wait",
            task,
            "--for",
            "owner",
            "--timeout-seconds",
            "2",
        ],
        owner,
    ));
    assert_eq!(received["message_id"], question_id);
    assert_eq!(received["body"], "Which token?");
    json_output(&run(
        home,
        &["message", "ack", task, question_id, "--for", "owner"],
        owner,
    ));
    let answer = json_output(&run(
        home,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            question_id,
            "--body",
            "TOKEN_OK",
        ],
        owner,
    ));
    let answer_id = answer["message_id"].as_str().unwrap();
    let worker_received = json_output(&run(
        home,
        &[
            "message",
            "wait",
            task,
            "--for",
            "worker",
            "--timeout-seconds",
            "2",
        ],
        worker,
    ));
    assert_eq!(worker_received["message_id"], answer_id);
    assert_eq!(worker_received["in_reply_to"], question_id);
    json_output(&run(
        home,
        &["message", "ack", task, answer_id, "--for", "worker"],
        worker,
    ));
}

fn assert_message_replay_and_direction(
    home: &Path,
    task: &str,
    owner: &[(&str, &str)],
    question_id: &str,
) {
    assert_eq!(
        json_output(&run(
            home,
            &[
                "message",
                "send",
                task,
                "--to",
                "worker",
                "--kind",
                "question",
                "--body",
                "Confirm receipt?",
                "--request-id",
                question_id,
            ],
            owner,
        ))["message_id"],
        question_id
    );
    assert!(
        !run(
            home,
            &[
                "message",
                "send",
                task,
                "--to",
                "worker",
                "--kind",
                "question",
                "--body",
                "Changed replay",
                "--request-id",
                question_id,
            ],
            owner,
        )
        .status
        .success()
    );
    assert!(
        !run(
            home,
            &[
                "message",
                "send",
                task,
                "--to",
                "worker",
                "--kind",
                "reply",
                "--body",
                "Invalid self reply",
                "--reply-to",
                question_id,
            ],
            owner,
        )
        .status
        .success()
    );
}

fn unanswered_message_count(home: &Path, task: &str, attempt: &str) -> u64 {
    brgr_store::Store::open(home.join("store"))
        .unwrap()
        .unsettled_questions(task.parse().unwrap(), attempt.parse().unwrap())
        .unwrap()
}

fn terminal_worker_can_read_and_ack_existing_message(
    home: &Path,
    task: &str,
    owner: &[(&str, &str)],
    worker: &[(&str, &str)],
) {
    let pending = json_output(&run(
        home,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "note",
            "--body",
            "Read after exit",
        ],
        owner,
    ));
    let message_id = pending["message_id"].as_str().unwrap();
    json_output(&run(home, &["cancel", task], owner));
    let settled = json_output(&run(
        home,
        &["wait", task, "--timeout-seconds", "60"],
        owner,
    ));
    assert_eq!(settled["outcome"], "cancelled");
    let unread = json_output(&run(
        home,
        &["message", "list", task, "--for", "worker"],
        worker,
    ));
    assert_eq!(unread[0]["message_id"], message_id);
    json_output(&run(
        home,
        &["message", "ack", task, message_id, "--for", "worker"],
        worker,
    ));
    let remaining = json_output(&run(
        home,
        &["message", "list", task, "--for", "worker"],
        worker,
    ));
    assert_eq!(remaining.as_array().unwrap().len(), 0);
    let other_attempt = brgr_protocol::AttemptId::new().to_string();
    let mut wrong_worker = worker.to_vec();
    wrong_worker.push(("BRGR_PARENT_ATTEMPT_ID", other_attempt.as_str()));
    assert!(
        !run(
            home,
            &["message", "list", task, "--for", "worker"],
            &wrong_worker,
        )
        .status
        .success()
    );
    assert!(
        !run(
            home,
            &[
                "message",
                "send",
                task,
                "--to",
                "owner",
                "--kind",
                "note",
                "--body",
                "Stale attempt"
            ],
            worker,
        )
        .status
        .success()
    );
}

#[test]
fn owner_message_wait_survives_the_gap_before_attempt_creation() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &scratch);
    let launch = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &[
            ("BRGR_OWNER_ID", "codex:message-gap"),
            ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1"),
        ],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let started = Instant::now();
    let wait = run(
        &home,
        &[
            "message",
            "wait",
            task,
            "--for",
            "owner",
            "--timeout-seconds",
            "1",
        ],
        &[("BRGR_OWNER_ID", "codex:message-gap")],
    );
    assert!(!wait.status.success());
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(String::from_utf8_lossy(&wait.stderr).contains("before the wait timeout"));
}

#[test]
fn worker_receives_criteria_scope_and_role_before_reporting() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-instructions-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q CHECK_INSTRUCTIONS \"$prompt\"; then\n /usr/bin/grep -q 'snapshot matches' \"$prompt\" || exit 7\n /usr/bin/grep -q 'Only docs' \"$prompt\" || exit 8\n /usr/bin/grep -q 'Act as reviewer' \"$prompt\" || exit 9\nfi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"INSTRUCTIONS_OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let result = json_output(&run(
        &home,
        &[
            "run",
            "CHECK_INSTRUCTIONS",
            "--workspace",
            workspace.to_str().unwrap(),
            "--criterion",
            "snapshot matches",
            "--scope",
            "Only docs",
            "--role-instruction",
            "Act as reviewer",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:instructions")],
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(
        &home,
        &["result", task],
        &[("BRGR_OWNER_ID", "codex:instructions")],
    ));
    assert_eq!(detail["artifacts"][0]["text"], "INSTRUCTIONS_OK");
    let stored = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task.parse().unwrap())
        .unwrap();
    assert_eq!(stored.instructions.scope, ["Only docs"]);
    assert_eq!(stored.instructions.role, ["Act as reviewer"]);
}

#[test]
fn idle_codex_parent_receives_completion_without_another_user_turn() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "working").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\",\"agent_session\":{\"value\":\"session-a\"}}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &scratch);
    let envs = [
        ("CODEX_THREAD_ID", "session-a"),
        ("BRGR_OWNER_ID", "codex:session-a"),
        ("BRGR_SESSION_ID", "session-a"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("HERDR_SESSION", "fixture-herdr"),
        ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
        ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &envs,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(
        wait_until(|| store.latest_result(task.parse().unwrap()).is_ok()),
        "the run never sealed a result"
    );
    // The parent is busy, so no prompt may land however long this waits; the
    // pause only gives a wrong dispatcher the chance to send one.
    thread::sleep(Duration::from_millis(700));
    assert!(!prompts.exists(), "busy Codex parent was prompted");
    assert_duplicate_notification_dispatcher_exits(&home, task, &envs);
    fs::write(&agent_state, "idle").unwrap();
    // The file exists a moment before its text does, so wait for the text.
    let prompt = delivered_prompt(&prompts, |text| text.contains("FROM BRGR"))
        .expect("completion was not delivered");
    assert!(prompt.contains("FROM BRGR"));
    assert!(prompt.contains(task));
    assert!(
        wait_until(|| store
            .pending_notifications_for_task(task.parse().unwrap())
            .unwrap()
            .is_empty()),
        "the delivered completion stayed pending"
    );
    // Delivered once: a pause only gives a wrong dispatcher the chance to
    // prompt again.
    thread::sleep(Duration::from_millis(700));
    assert_eq!(fs::read_to_string(prompts).unwrap(), prompt);
}

#[test]
fn idle_codex_parent_is_told_a_run_failed_with_its_reason() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "working").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\",\"agent_session\":{\"value\":\"session-a\"}}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    let executable = temp.path().join("fixture");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-failed-notice-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q BRGR_FIXTURE_OK \"$prompt\"; then\n printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"BRGR_FIXTURE_OK\"}]}}'\n printf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\nelse\n exit 7\nfi\n",
    );
    add_fixture(&home, &executable, &scratch);
    let envs = [
        ("CODEX_THREAD_ID", "session-a"),
        ("BRGR_OWNER_ID", "codex:session-a"),
        ("BRGR_SESSION_ID", "session-a"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("HERDR_SESSION", "fixture-herdr"),
        ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
        ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &["run", "FAIL", "--workspace", workspace.to_str().unwrap()],
        &envs,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(
        wait_until(|| store.latest_result(task.parse().unwrap()).is_ok()),
        "the run never sealed a result"
    );
    // The parent is busy, so no prompt may land however long this waits; the
    // pause only gives a wrong dispatcher the chance to send one.
    thread::sleep(Duration::from_millis(700));
    assert!(!prompts.exists(), "busy Codex parent was prompted");
    assert_duplicate_notification_dispatcher_exits(&home, task, &envs);
    fs::write(&agent_state, "idle").unwrap();
    // The file exists a moment before its text does, so wait for the text.
    let prompt = delivered_prompt(&prompts, |text| text.contains("FROM BRGR"))
        .expect("completion was not delivered");
    assert!(prompt.contains("FROM BRGR"), "{prompt}");
    assert!(prompt.contains("\"type\":\"brgr_failure\""), "{prompt}");
    assert!(prompt.contains("\"outcome\":\"failed\""), "{prompt}");
    assert!(prompt.contains(task), "{prompt}");
    assert!(prompt.contains("brgr result"), "{prompt}");
}

/// A Cursor owner gets the completion pushed into its pane like Codex and
/// Claude Code do. Cursor runs in the Herdr pane and has no hook, so its own
/// shell has to record the pane and a dispatcher has to be started for it.
#[test]
fn idle_cursor_owner_is_told_when_its_task_completes() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "idle").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"cursor\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\"}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let owner = [
        ("CURSOR_CONVERSATION_ID", "cursor-session"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("HERDR_SESSION", "fixture-herdr"),
        ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
        ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let delivered = delivered_prompt(&prompts, |text| text.contains(task))
        .expect("the completion was not delivered to the Cursor owner");
    assert!(delivered.contains("FROM BRGR"), "{delivered}");
    assert!(delivered.contains("brgr_completion"), "{delivered}");
}

/// After a handoff the owner's completion notices follow the new head.
///
/// Notices go to the pane recorded for the owner's bound session. A takeover
/// binds a new session, and nothing recorded its pane, so a worker that finished
/// after the handoff, or a notice requeued by it, still went to the old head's
/// pane. The incoming head's own commit now records its pane.
#[test]
fn a_new_head_receives_the_owners_completion_notices_after_a_handoff() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "idle").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"cursor\",\"pane_id\":\"%s\",\"agent_status\":\"%s\"}}}\\n' \"$3\" \"$state\";;\n 'agent prompt') printf '%s %s\\n' \"$3\" \"$(printf '%s' \"$4\" | /usr/bin/tr '\\n' ' ')\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let head = |session: &'static str, pane: &'static str| {
        vec![
            ("CURSOR_CONVERSATION_ID", session),
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", pane),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
            ("HERDR_SESSION", "fixture-herdr"),
            ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
            ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
        ]
    };
    let outgoing = head("cursor-a", "w1:p1");
    let incoming = head("cursor-b", "w1:p2");
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &outgoing,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let delivered_to = |pane: &str| {
        delivered_prompt(&prompts, |text| {
            text.lines()
                .any(|line| line.starts_with(pane) && line.contains(task))
        })
    };
    delivered_to("w1:p1").expect("the completion did not reach the first head");
    let requested = json_output(&run(
        &home,
        &["head", "request", task, "--to", "cursor-b"],
        &outgoing,
    ));
    let takeover = requested["takeover_id"].as_str().unwrap();
    json_output(&run(
        &home,
        &["head", "prepare", takeover, "--goal", "Carry on"],
        &outgoing,
    ));
    json_output(&run(&home, &["head", "commit", takeover], &incoming));
    delivered_to("w1:p2").expect("the requeued completion did not reach the new head's pane");
}

/// The owner id names the harness the owner started in; after a handoff the head
/// may run in another one. Delivery checked that the pane runs the harness the
/// owner id names, so a Claude head taking a Cursor owner's work was never told.
#[test]
fn a_head_in_another_harness_receives_the_owners_completion_notices_after_a_handoff() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "idle").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); kind=cursor; [ \"$3\" = w1:p2 ] && kind=claude; printf '{\"result\":{\"agent\":{\"agent\":\"%s\",\"pane_id\":\"%s\",\"agent_status\":\"%s\"}}}\\n' \"$kind\" \"$3\" \"$state\";;\n 'agent prompt') printf '%s %s\\n' \"$3\" \"$(printf '%s' \"$4\" | /usr/bin/tr '\\n' ' ')\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let head = |session: &'static str, pane: &'static str| {
        vec![
            ("CURSOR_CONVERSATION_ID", session),
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", pane),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
            ("HERDR_SESSION", "fixture-herdr"),
            ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
            ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
        ]
    };
    let outgoing = head("cursor-a", "w1:p1");
    let mut incoming = head("cursor-b", "w1:p2");
    // The new head runs Claude Code, not Cursor.
    incoming[0] = ("CLAUDE_CODE_SESSION_ID", "claude-b");
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &outgoing,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let delivered_to = |pane: &str| {
        delivered_prompt(&prompts, |text| {
            text.lines()
                .any(|line| line.starts_with(pane) && line.contains(task))
        })
    };
    delivered_to("w1:p1").expect("the completion did not reach the first head");
    let requested = json_output(&run(
        &home,
        &["head", "request", task, "--to", "claude-b"],
        &outgoing,
    ));
    let takeover = requested["takeover_id"].as_str().unwrap();
    json_output(&run(
        &home,
        &["head", "prepare", takeover, "--goal", "Carry on"],
        &outgoing,
    ));
    json_output(&run(&home, &["head", "commit", takeover], &incoming));
    delivered_to("w1:p2").expect("the requeued completion did not reach the new head's pane");
}

/// A worker's question reaches an idle Codex owner without another user turn.
///
/// Only completions used to be pushed, so a question waited until the worker's
/// own `message wait` timed out unless the owner happened to poll. The notice is
/// held while Codex is busy, sent once per session, and stops once answered.
#[test]
fn idle_codex_owner_is_told_when_a_worker_asks() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "working").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\",\"agent_session\":{\"value\":\"session-q\"}}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let owner = [
        ("CODEX_THREAD_ID", "session-q"),
        ("BRGR_OWNER_ID", "codex:session-q"),
        ("BRGR_SESSION_ID", "session-q"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("HERDR_SESSION", "fixture-herdr"),
        ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
        ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
        ],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = active_attempt(&store, task).to_string();
    let worker_owner = format!("worker:{attempt}");
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", task),
        ("BRGR_PARENT_ATTEMPT_ID", attempt.as_str()),
    ];
    let question = json_output(&run(
        &home,
        &[
            "message",
            "send",
            task,
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

    thread::sleep(Duration::from_millis(1_200));
    assert!(!prompts.exists(), "a busy Codex owner was prompted");
    fs::write(&agent_state, "idle").unwrap();
    let delivered = delivered_prompt(&prompts, |text| text.contains(&question_id))
        .expect("the question was not delivered");
    assert!(delivered.contains("FROM BRGR"), "{delivered}");
    assert!(delivered.contains("brgr_question"), "{delivered}");
    assert!(delivered.contains(task), "{delivered}");

    // Several more polls while the question is still open: told once, not again.
    thread::sleep(Duration::from_millis(1_500));
    let text = fs::read_to_string(&prompts).unwrap();
    // The id also appears inside the notice's own instructions, so count
    // notices by their `message_id` field.
    let field = format!("\"message_id\":\"{question_id}\"");
    assert_eq!(text.matches(field.as_str()).count(), 1, "{text}");

    json_output(&run(&home, &["cancel", task], &owner));
}

/// A worker's question reaches an idle Claude Code owner too. Herdr keeps no
/// session id for Claude Code, so its pane and agent kind are the identity, and
/// the owner id comes from `CLAUDE_CODE_SESSION_ID`.
///
/// Only completions used to be pushed, so a question waited until the worker's
/// own `message wait` timed out unless the owner happened to poll. The notice is
/// held while Codex is busy, sent once per session, and stops once answered.
#[test]
fn idle_claude_owner_is_told_when_a_worker_asks() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let agent_state = temp.path().join("agent-state");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&agent_state, "working").unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"claude\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\"}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let owner = [
        ("CLAUDE_CODE_SESSION_ID", "session-q"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("HERDR_SESSION", "fixture-herdr"),
        ("BRGR_TEST_AGENT_STATE", agent_state.to_str().unwrap()),
        ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
        ],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = active_attempt(&store, task).to_string();
    let worker_owner = format!("worker:{attempt}");
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", task),
        ("BRGR_PARENT_ATTEMPT_ID", attempt.as_str()),
    ];
    let question = json_output(&run(
        &home,
        &[
            "message",
            "send",
            task,
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

    thread::sleep(Duration::from_millis(1_200));
    assert!(!prompts.exists(), "a busy Claude owner was prompted");
    fs::write(&agent_state, "idle").unwrap();
    let delivered = delivered_prompt(&prompts, |text| text.contains(&question_id))
        .expect("the question was not delivered");
    assert!(delivered.contains("FROM BRGR"), "{delivered}");
    assert!(delivered.contains("brgr_question"), "{delivered}");
    assert!(delivered.contains(task), "{delivered}");

    // Several more polls while the question is still open: told once, not again.
    thread::sleep(Duration::from_millis(1_500));
    let text = fs::read_to_string(&prompts).unwrap();
    // The id also appears inside the notice's own instructions, so count
    // notices by their `message_id` field.
    let field = format!("\"message_id\":\"{question_id}\"");
    assert_eq!(text.matches(field.as_str()).count(), 1, "{text}");

    json_output(&run(&home, &["cancel", task], &owner));
}

fn set_dispatcher_notifications(home: &Path, enabled: bool) {
    let enabled = if enabled { "true" } else { "false" };
    assert!(
        run(
            home,
            &[
                "config",
                "set",
                "notifications",
                enabled,
                "--harness",
                "local.fixture"
            ],
            &[],
        )
        .status
        .success()
    );
}

fn dispatcher_with_pending_notice() -> (
    TempDir,
    PathBuf,
    String,
    PathBuf,
    PathBuf,
    PathBuf,
    std::process::Child,
) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    let herdr = temp.path().join("herdr");
    let herdr_calls = temp.path().join("herdr-calls");
    let herdr_gate = temp.path().join("herdr-gate");
    let herdr_release = temp.path().join("herdr-release");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') printf called >> \"$BRGR_TEST_HERDR_CALLS\"; if [ ! -e \"$BRGR_TEST_HERDR_GATE\" ]; then : > \"$BRGR_TEST_HERDR_GATE\"; while [ ! -e \"$BRGR_TEST_HERDR_RELEASE\" ]; do /bin/sleep 0.01; done; fi; printf '%s\\n' '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"working\",\"agent_session\":{\"value\":\"dispatcher-session\"}}}}';;\n *) exit 2;;\nesac\n",
    );
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-dispatcher-notice'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q BRGR_FIXTURE_OK \"$prompt\"; then\n printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"BRGR_FIXTURE_OK\"}]}}'\n printf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\nelse\n exit 7\nfi\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:dispatcher-exit")];
    set_dispatcher_notifications(&home, false);
    let launched = json_output(&run(
        &home,
        &[
            "run",
            "FAIL",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(launched["outcome"], "failed", "{launched}");
    let task = launched["task_id"].as_str().unwrap().to_owned();
    let attempt = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .latest_result(task.parse().unwrap())
        .unwrap()
        .attempt_id;
    assert_eq!(
        brgr_store::Store::open(home.join("store"))
            .unwrap()
            .pending_notifications_for_task(task.parse().unwrap())
            .unwrap()
            .len(),
        1,
        "the fixture must leave an undelivered notice"
    );
    let owner_id = brgr_protocol::OwnerId::new("codex:dispatcher-exit").unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let epoch = store.rebind_owner(&owner_id, "dispatcher-session").unwrap();
    store
        .register_owner_surface(
            &owner_id,
            "dispatcher-session",
            epoch,
            "w1:p1",
            None,
            herdr.to_str().unwrap(),
        )
        .unwrap();
    drop(store);
    let message_id = uuid::Uuid::new_v4().to_string();
    rusqlite::Connection::open(home.join("store/brgr.sqlite3"))
        .unwrap()
        .execute(
            "INSERT INTO task_messages (message_id, task_id, attempt_id, direction, kind, body)
             VALUES (?1, ?2, ?3, 'worker_to_owner', 'question', 'dispatcher race gate')",
            rusqlite::params![message_id, task, attempt.to_string()],
        )
        .unwrap();
    set_dispatcher_notifications(&home, true);
    let dispatcher = brgr_command()
        .arg("--home")
        .arg(&home)
        .args(["__notify", &task])
        .env_remove("HERDR_ENV")
        .env("BRGR_TEST_HERDR_CALLS", &herdr_calls)
        .env("BRGR_TEST_HERDR_GATE", &herdr_gate)
        .env("BRGR_TEST_HERDR_RELEASE", &herdr_release)
        .spawn()
        .unwrap();
    (
        temp,
        home,
        task,
        herdr_calls,
        herdr_gate,
        herdr_release,
        dispatcher,
    )
}

fn wait_for_dispatcher_exit(
    dispatcher: &mut std::process::Child,
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Some(status) = dispatcher.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

fn wait_for_herdr_gate(path: &Path, dispatcher: &mut std::process::Child) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if path.exists() {
            return;
        }
        assert!(
            dispatcher.try_wait().unwrap().is_none(),
            "dispatcher exited before reaching the gated Herdr call"
        );
        thread::sleep(Duration::from_millis(50));
    }
    panic!("dispatcher did not reach the gated Herdr call within five seconds");
}

/// A dispatcher does not recreate its control home after deletion during a pass.
#[test]
fn a_dispatcher_exits_without_recreating_its_deleted_control_home() {
    let (_temp, home, _task, _herdr_calls, herdr_gate, herdr_release, mut dispatcher) =
        dispatcher_with_pending_notice();
    wait_for_herdr_gate(&herdr_gate, &mut dispatcher);
    fs::remove_dir_all(&home).unwrap();
    fs::write(herdr_release, "continue").unwrap();
    let status = wait_for_dispatcher_exit(&mut dispatcher);
    if status.is_none() {
        dispatcher.kill().unwrap();
        dispatcher.wait().unwrap();
    }
    let status = status.expect("the dispatcher kept polling a removed control home");
    assert!(
        status.success(),
        "dispatcher failed after home deletion: {status}"
    );
    assert!(!home.exists(), "the dispatcher recreated its deleted home");
}

#[test]
fn a_dispatcher_exits_when_its_database_is_recreated() {
    let (_temp, home, _task, _herdr_calls, herdr_gate, herdr_release, mut dispatcher) =
        dispatcher_with_pending_notice();
    wait_for_herdr_gate(&herdr_gate, &mut dispatcher);
    let database = home.join("store/brgr.sqlite3");
    let old_identity = fs::metadata(&database).unwrap();
    fs::remove_file(&database).unwrap();
    drop(brgr_store::Store::open(home.join("store")).unwrap());
    let new_identity = fs::metadata(&database).unwrap();
    assert!(
        old_identity.dev() != new_identity.dev() || old_identity.ino() != new_identity.ino(),
        "replacement database reused its device and inode"
    );
    fs::write(herdr_release, "continue").unwrap();
    let status = wait_for_dispatcher_exit(&mut dispatcher);
    if status.is_none() {
        dispatcher.kill().unwrap();
        dispatcher.wait().unwrap();
    }
    let status = status.expect("the dispatcher stayed alive after database replacement");
    assert!(
        status.success(),
        "dispatcher failed after database replacement: {status}"
    );
}

#[test]
fn missing_herdr_codex_session_is_reported_from_exact_bound_pane() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let reported = temp.path().join("reported-session");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &herdr,
        r#"#!/bin/sh
case "$1 $2" in
 'agent get')
  if [ -s "$BRGR_TEST_SESSION_FILE" ]; then
   printf '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working","agent_session":{"value":"%s"}}}}\n' "$(/bin/cat "$BRGR_TEST_SESSION_FILE")"
  else
   printf '%s\n' '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working"}}}'
  fi;;
 'pane report-agent-session')
  test "$3" = w1:p1 || exit 7
  while [ "$#" -gt 0 ]; do
   if [ "$1" = --source ]; then test "$2" = herdr:codex || exit 8; fi
   if [ "$1" = --agent-session-id ]; then printf '%s' "$2" > "$BRGR_TEST_SESSION_FILE"; fi
   shift
  done
  printf '%s\n' '{}';;
 *) exit 9;;
esac
"#,
    );
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let envs = [
        ("CODEX_THREAD_ID", "session-a"),
        ("BRGR_OWNER_ID", "codex:session-a"),
        ("BRGR_SESSION_ID", "session-a"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("BRGR_TEST_SESSION_FILE", reported.to_str().unwrap()),
    ];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    ));
    assert_eq!(result["outcome"], "candidate");
    assert!(
        wait_until(|| reported.exists()),
        "the Codex session was never reported"
    );
    assert_eq!(fs::read_to_string(&reported).unwrap(), "session-a");
    let task = result["task_id"].as_str().unwrap();
    json_output(&run(&home, &["accept", task], &envs));
}

/// The owner-surface registration must not trust an unverified pane either:
/// binding this session to another Codex's pane would push its notices there.
#[test]
fn an_unverified_pane_is_never_bound_to_a_codex_session() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let reported = temp.path().join("reported-session");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &herdr,
        r#"#!/bin/sh
case "$1 $2" in
 'agent get')
  if [ -s "$BRGR_TEST_SESSION_FILE" ]; then
   printf '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working","agent_session":{"value":"%s"}}}}\n' "$(/bin/cat "$BRGR_TEST_SESSION_FILE")"
  else
   printf '%s\n' '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working"}}}'
  fi;;
 'pane report-agent-session')
  test "$3" = w1:p1 || exit 7
  while [ "$#" -gt 0 ]; do
   if [ "$1" = --source ]; then test "$2" = herdr:codex || exit 8; fi
   if [ "$1" = --agent-session-id ]; then printf '%s' "$2" > "$BRGR_TEST_SESSION_FILE"; fi
   shift
  done
  printf '%s\n' '{}';;
 *) exit 9;;
esac
"#,
    );
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let envs = [
        ("CODEX_THREAD_ID", "session-a"),
        ("BRGR_OWNER_ID", "codex:session-a"),
        ("BRGR_SESSION_ID", "session-a"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("BRGR_TEST_SESSION_FILE", reported.to_str().unwrap()),
        // A pane id this process does not run in, as a hook run by Codex's
        // shared daemon inherits.
        ("BRGR_TEST_TRUST_HERDR_PANE", "0"),
    ];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    ));
    assert_eq!(result["outcome"], "candidate");
    thread::sleep(Duration::from_secs(2));
    assert!(
        !reported.exists(),
        "a Codex session was bound to a pane this process does not run in"
    );
    let task = result["task_id"].as_str().unwrap();
    json_output(&run(&home, &["accept", task], &envs));
}

#[test]
fn unavailable_herdr_does_not_hide_the_codex_inbox_hook() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    fs::create_dir_all(&workspace).unwrap();
    // A Herdr that never answers: a hook that waited for it would wait ten
    // minutes, so only such a hook can reach the one-minute bounds below.
    write_fast_script(&herdr, "#!/bin/sh\nexec /bin/sleep 600\n");
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let owner = [
        ("BRGR_OWNER_ID", "codex:hook-fallback"),
        ("BRGR_SESSION_ID", "hook-fallback"),
    ];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    let task = result["task_id"].as_str().unwrap();
    let (output, elapsed) = invoke_hook_with_herdr(&home, &herdr, "user-prompt-submit");
    assert!(
        elapsed < HANG_GUARD,
        "slow Herdr delayed the inbox hook: {elapsed:?}"
    );
    assert!(output.status.success());
    let surfaced: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        surfaced["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(task)
    );
    let (stop, stop_elapsed) = invoke_hook_with_herdr(&home, &herdr, "stop");
    assert!(
        stop_elapsed < HANG_GUARD,
        "slow Herdr delayed the stop hook: {stop_elapsed:?}"
    );
    let stop: Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(stop["decision"], "block");
    assert!(stop["reason"].as_str().unwrap().contains(task), "{stop}");
    // Codex defines no hook-specific output for Stop and rejects the whole
    // object when one is present.
    assert!(stop.get("hookSpecificOutput").is_none(), "{stop}");
    json_output(&run(&home, &["accept", task], &owner));
}

#[test]
fn slow_herdr_registration_cannot_leave_an_unclaimed_task() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let reported = temp.path().join("reported-session");
    let release = temp.path().join("herdr-release");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &herdr,
        r#"#!/bin/sh
case "$1 $2" in
 'agent get')
  if [ -s "$BRGR_TEST_SESSION_FILE" ]; then
   printf '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working","agent_session":{"value":"%s"}}}}\n' "$(/bin/cat "$BRGR_TEST_SESSION_FILE")"
  else
   n=0; while [ ! -e "$BRGR_TEST_RELEASE" ] && [ "$n" -lt 600 ]; do /bin/sleep 0.1; n=$((n+1)); done
   printf '%s\n' '{"result":{"agent":{"agent":"codex","pane_id":"w1:p1","agent_status":"working"}}}'
  fi;;
 'pane report-agent-session')
  n=0; while [ ! -e "$BRGR_TEST_RELEASE" ] && [ "$n" -lt 600 ]; do /bin/sleep 0.1; n=$((n+1)); done
  while [ "$#" -gt 0 ]; do
   if [ "$1" = --agent-session-id ]; then printf '%s' "$2" > "$BRGR_TEST_SESSION_FILE"; fi
   shift
  done
  printf '%s\n' '{}';;
 *) exit 9;;
esac
"#,
    );
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let envs = [
        ("CODEX_THREAD_ID", "session-a"),
        ("BRGR_OWNER_ID", "codex:session-a"),
        ("BRGR_SESSION_ID", "session-a"),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
        ("BRGR_TEST_SESSION_FILE", reported.to_str().unwrap()),
        ("BRGR_TEST_RELEASE", release.to_str().unwrap()),
    ];
    let launch = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &envs,
    ));
    // Herdr answers only once the release file exists, and it does not yet:
    // a `run` that has returned did not wait for it. No clock is involved.
    assert!(!release.exists(), "task admission waited for Herdr");
    let task = launch["task_id"].as_str().unwrap();
    fs::write(&release, "go").unwrap();
    // Let the slow registration finish: it records the session once released.
    assert!(
        wait_until(|| reported.is_file()),
        "the released registration never recorded the session"
    );
    assert!(
        wait_until(|| { json_output(&run(&home, &["status", task], &envs))["state"] == "running" }),
        "the released task never reached a claimed running attempt"
    );
    let unfinished = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .unfinished_attempts()
        .unwrap();
    assert!(
        unfinished
            .iter()
            .any(|attempt| attempt.task.task_id.to_string() == task),
        "the slow registration left no claimed attempt for task {task}: {unfinished:?}"
    );
    json_output(&run(&home, &["cancel", task], &envs));
    let terminal = json_output(&run(
        &home,
        &["wait", task, "--timeout-seconds", "60"],
        &envs,
    ));
    assert_eq!(terminal["outcome"], "cancelled");
    json_output(&run(&home, &["result", task, "--ack"], &envs));
    thread::sleep(Duration::from_millis(800));
}

/// Codex runs every installed hook for a worker as well. brgr's hook is for
/// owners: in a task worktree it neither binds the session nor hands it the
/// owner's calling context.
#[test]
fn the_codex_hook_stays_out_of_a_worker_in_a_task_worktree() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let hook = |cwd: &Path| {
        let mut child = brgr_command()
            .arg("--home")
            .arg(&home)
            .args(["__hook", "session-start"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let input = json!({"session_id": "019b0000-0000-7000-8000-000000000001", "cwd": cwd});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let owner = temp.path().join("project");
    fs::create_dir_all(&owner).unwrap();
    assert!(hook(&owner).contains("brgr calling context"));
    let worker = home.join("worktrees/project/0123abcd");
    fs::create_dir_all(&worker).unwrap();
    assert_eq!(hook(&worker).trim(), "{}");
}

fn invoke_hook_with_herdr(
    home: &Path,
    herdr: &Path,
    event: &str,
) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let mut child = brgr_command()
        .arg("--home")
        .arg(home)
        .args(["__hook", event])
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_BIN_PATH", herdr)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"session_id\":\"hook-fallback\"}")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (output, started.elapsed())
}

fn assert_duplicate_notification_dispatcher_exits(home: &Path, task: &str, envs: &[(&str, &str)]) {
    let mut duplicate = brgr_command()
        .arg("--home")
        .arg(home)
        .arg("__notify")
        .arg(task)
        .envs(envs.iter().copied())
        .spawn()
        .unwrap();
    // A duplicate that stays is one that polls for up to a day; its exit is
    // the event, and the hang guard cannot be reached by one that does exit.
    if !wait_until(|| duplicate.try_wait().unwrap().is_some()) {
        duplicate.kill().unwrap();
        duplicate.wait().unwrap();
        panic!("duplicate notification dispatcher stayed active");
    }
}

#[test]
fn a_failed_run_is_reported_on_every_owner_command_until_acknowledged() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-failed-notice-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q BRGR_FIXTURE_OK \"$prompt\"; then\n printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"BRGR_FIXTURE_OK\"}]}}'\n printf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\nelse\n exit 7\nfi\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [
        ("BRGR_OWNER_ID", "codex:failed-banner"),
        ("BRGR_SESSION_ID", "banner-session"),
    ];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "FAIL",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "failed");
    let task = result["task_id"].as_str().unwrap();
    let stderr_of =
        |args: &[&str]| String::from_utf8_lossy(&run(&home, args, &owner).stderr).into_owned();
    // Any later command of the owner carries the note, not only `result`.
    for args in [&["status"][..], &["doctor"][..]] {
        let text = stderr_of(args);
        assert!(text.contains("failed or were lost"), "{args:?}: {text}");
        assert!(text.contains(task), "{args:?}: {text}");
    }
    // The same owner ID under an outgoing session is not told about it.
    let other = [
        ("BRGR_OWNER_ID", "codex:failed-banner"),
        ("BRGR_SESSION_ID", "other-session"),
    ];
    assert!(!String::from_utf8_lossy(&run(&home, &["status"], &other).stderr).contains(task));
    // A different owner with a real binding is not told about it either.
    let someone_else = brgr_protocol::OwnerId::new("codex:someone-else").unwrap();
    brgr_store::Store::open(home.join("store"))
        .unwrap()
        .rebind_owner(&someone_else, "other-session-2")
        .unwrap();
    let other_owner = [
        ("BRGR_OWNER_ID", "codex:someone-else"),
        ("BRGR_SESSION_ID", "other-session-2"),
    ];
    assert!(!String::from_utf8_lossy(&run(&home, &["status"], &other_owner).stderr).contains(task));
    json_output(&run(&home, &["result", task, "--ack"], &owner));
    let text = stderr_of(&["status"]);
    assert!(!text.contains("failed or were lost"), "{text}");
}

#[test]
fn final_failed_result_does_not_leave_a_notification_dispatcher_running() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-failed-notice-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q BRGR_FIXTURE_OK \"$prompt\"; then\n printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"BRGR_FIXTURE_OK\"}]}}'\n printf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\nelse\n exit 7\nfi\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:failed-notice")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "FAIL",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "failed");
    let task = result["task_id"].as_str().unwrap();
    json_output(&run(&home, &["result", task, "--ack"], &owner));
    let mut dispatcher = brgr_command()
        .arg("--home")
        .arg(&home)
        .arg("__notify")
        .arg(task)
        .spawn()
        .unwrap();
    if wait_until(|| dispatcher.try_wait().unwrap().is_some()) {
        return;
    }
    dispatcher.kill().unwrap();
    dispatcher.wait().unwrap();
    panic!("final failed task kept its notification dispatcher alive");
}

#[test]
fn failed_execution_seals_requested_stderr() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(
        &executable,
        r#"#!/bin/sh
case "$1" in
 --version) echo 'fixture v-failed-log-fixture'; exit 0;;
 --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
for item in "$@"; do case "$item" in @*) prompt=${item#@};; esac; done
if /usr/bin/grep -q BRGR_FIXTURE_OK "$prompt"; then
 printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
 printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
else
 printf 'failure diagnostic 23\n' >&2
 exit 23
fi
"#,
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:failed-log")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "FAIL",
            "--workspace",
            workspace.to_str().unwrap(),
            "--capture-logs",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "failed");
    let detail = json_output(&run(
        &home,
        &["result", result["task_id"].as_str().unwrap()],
        &owner,
    ));
    assert_eq!(
        detail["artifacts"][0]["reference"]["media_type"],
        "text/x-brgr-stderr"
    );
    assert!(
        detail["artifacts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("failure diagnostic 23")
    );
}

#[test]
fn transferred_codex_session_receives_undecided_completion() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let herdr = temp.path().join("herdr");
    let prompts = temp.path().join("prompts");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let task = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[
            ("CODEX_THREAD_ID", "session-a"),
            ("BRGR_OWNER_ID", "codex:session-a"),
            ("BRGR_SESSION_ID", "session-a"),
        ],
    ))["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    write_fast_script(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') printf '%s\\n' '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p2\",\"agent_status\":\"idle\",\"agent_session\":{\"value\":\"session-b\"}}}}';;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    );
    let rebound = json_output(&run(
        &home,
        &["bind", &task, "--session", "session-b"],
        &[
            ("CODEX_THREAD_ID", "session-b"),
            ("BRGR_OWNER_ID", "codex:session-a"),
            ("BRGR_SESSION_ID", "session-b"),
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p2"),
            ("HERDR_BIN_PATH", herdr.to_str().unwrap()),
            ("BRGR_TEST_PROMPTS", prompts.to_str().unwrap()),
        ],
    ));
    assert_eq!(rebound["session_id"], "session-b");
    // The detached `__notify` dispatcher has to start, read the store, call
    // `agent get`, then `agent prompt`; a loaded machine stretches that chain
    // past five seconds, so the wait is generous. The event is still the
    // prompt arriving.
    // The file exists a moment before its text does, so wait for the text.
    let prompt = delivered_prompt(&prompts, |text| text.contains(&task)).unwrap_or_else(|| {
        panic!(
            "transferred Codex session was not prompted: {}",
            run_state_debug(&home, &task)
        )
    });
    assert!(prompt.contains("FROM BRGR"));
}

#[test]
fn owner_and_worker_exchange_questions_and_replies_during_one_attempt() {
    let fixture = start_message_fixture();
    let home = &fixture.home;
    let task = fixture.task.as_str();
    let attempt_text = fixture.attempt.as_str();
    let owner = [("BRGR_OWNER_ID", "codex:message-test")];
    let worker_owner = format!("worker:{attempt_text}");
    let worker = [
        ("BRGR_OWNER_ID", worker_owner.as_str()),
        ("BRGR_SESSION_ID", worker_owner.as_str()),
        ("BRGR_PARENT_TASK_ID", task),
        ("BRGR_PARENT_ATTEMPT_ID", attempt_text),
    ];
    worker_question_roundtrip(home, task, &owner, &worker);

    let owner_question = json_output(&run(
        home,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "question",
            "--body",
            "Confirm receipt?",
            "--request-id",
            "11111111-1111-4111-8111-111111111111",
        ],
        &owner,
    ));
    let owner_question_id = owner_question["message_id"].as_str().unwrap();
    assert_message_replay_and_direction(home, task, &owner, owner_question_id);
    assert_eq!(unanswered_message_count(home, task, attempt_text), 1);
    json_output(&run(
        home,
        &["message", "ack", task, owner_question_id, "--for", "worker"],
        &worker,
    ));
    let worker_reply = json_output(&run(
        home,
        &[
            "message",
            "send",
            task,
            "--to",
            "owner",
            "--kind",
            "reply",
            "--reply-to",
            owner_question_id,
            "--body",
            "Confirmed",
        ],
        &worker,
    ));
    assert_eq!(worker_reply["in_reply_to"], owner_question_id);
    let owner_messages = json_output(&run(
        home,
        &["message", "list", task, "--for", "owner"],
        &owner,
    ));
    assert_eq!(owner_messages.as_array().unwrap().len(), 1);
    assert_eq!(owner_messages[0]["body"], "Confirmed");
    let reply_id = worker_reply["message_id"].as_str().unwrap();
    json_output(&run(
        home,
        &["message", "ack", task, reply_id, "--for", "owner"],
        &owner,
    ));
    assert_eq!(unanswered_message_count(home, task, attempt_text), 0);
    terminal_worker_can_read_and_ack_existing_message(home, task, &owner, &worker);
}

#[test]
fn omp_worker_delegates_to_fixture_then_fixture_without_pair_specific_routing() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let fixture = temp.path().join("fixture");
    let omp = temp.path().join("omp");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&fixture, RECURSIVE_FIXTURE_FIXTURE);
    let omp_fixture = RECURSIVE_FIXTURE_FIXTURE
        .replace("fixture v-recursive-fixture", "omp/fixture")
        .replace(
            "'--no-mcp' '--model' '--thinking'",
            "'--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'",
        );
    write_fast_script(&omp, &omp_fixture);
    for executable in [&fixture, &omp] {
        add_fixture(
            &home,
            executable,
            &temp.path().join(format!(
                "scratch-{}",
                executable.file_name().unwrap().to_string_lossy()
            )),
        );
    }
    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.omp",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:mixed-root")],
    ));
    assert_eq!(root["outcome"], "candidate");
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let tasks = store.tasks(10).unwrap();
    assert_eq!(tasks.len(), 3);
    let root_task = tasks.iter().find(|task| task.objective == "ROOT").unwrap();
    assert_eq!(root_task.route.harness_id, "local.omp");
    assert_eq!(
        tasks
            .iter()
            .filter(|task| task.route.harness_id == "local.fixture")
            .count(),
        2
    );
}

#[test]
fn plugin_codex_missing_binary_fails_visibly_without_launching() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let empty_bin = temp.path().join("empty-bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&empty_bin).unwrap();
    let context = json!({"workspace_id": "w1", "workspace_cwd": workspace});
    let launched = brgr_command()
        .args(["plugin", "codex"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PLUGIN_ID", "brgr")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("BRGR_HOME", &home)
        .env("CODEX_HOME", temp.path().join("codex-home"))
        .env("PATH", &empty_bin)
        .output()
        .unwrap();
    assert!(!launched.status.success());
    assert!(String::from_utf8_lossy(&launched.stderr).contains("could not start Codex"));
}

#[test]
fn plugin_codex_uses_configured_executable_with_minimal_herdr_path() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let fake_bin = temp.path().join("bin");
    let executable = fake_bin.join("codex");
    let observed_path = temp.path().join("codex-path.txt");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' \"$PATH\" > \"$BRGR_TEST_CODEX_PATH\"\n/bin/sleep 0.2\n",
    );
    let configured = json_output(&run(
        &home,
        &[
            "config",
            "set-codex-executable",
            executable.to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(
        configured["herdr"]["codex_executable"],
        executable.to_str().unwrap()
    );
    let context = json!({"workspace_id":"w1", "workspace_cwd":workspace});
    let launched = brgr_command()
        .args(["plugin", "codex"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PLUGIN_ID", "brgr")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("BRGR_HOME", &home)
        .env("CODEX_HOME", temp.path().join("codex-home"))
        .env("BRGR_TEST_CODEX_PATH", &observed_path)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let passed_path = fs::read_to_string(observed_path).unwrap();
    assert!(
        passed_path
            .split(':')
            .any(|entry| entry == fake_bin.to_str().unwrap())
    );
    let cleared = json_output(&run(&home, &["config", "clear-codex-executable"], &[]));
    assert!(cleared["herdr"]["codex_executable"].is_null());
    assert!(
        !run(
            &home,
            &["config", "set-codex-executable", "relative/codex"],
            &[]
        )
        .status
        .success()
    );
}

#[test]
fn plugin_open_surfaces_herdr_launch_failure() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    let herdr = fake_bin.join("herdr");
    write_fast_script(&herdr, "#!/bin/sh\necho 'pane refused' >&2\nexit 7\n");
    let context = json!({"workspace_id": "w1", "workspace_cwd": workspace});
    let launched = brgr_command()
        .args(["plugin", "open", "--no-focus", "--codex"])
        .env("HERDR_ENV", "1")
        .env("HERDR_PLUGIN_ID", "brgr")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("HERDR_BIN_PATH", &herdr)
        .env("BRGR_HOME", &home)
        .output()
        .unwrap();
    assert!(!launched.status.success());
    let stderr = String::from_utf8_lossy(&launched.stderr);
    assert!(stderr.contains("could not open the brgr pane"));
    assert!(stderr.contains("pane refused"));
}

#[test]
fn stale_codex_skill_or_hooks_report_drift_until_reinstall() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let envs = [("CODEX_HOME", codex_home.to_str().unwrap())];
    json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    assert_eq!(
        json_output(&run(&home, &["integrate", "codex", "status"], &envs))["status"],
        "installed"
    );

    let receipt_path = home.join("codex-integration.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    let skill_path = PathBuf::from(receipt["skill_path"].as_str().unwrap());
    fs::write(&skill_path, "owned prior brgr skill\n").unwrap();
    receipt["skill_text"] = json!("owned prior brgr skill\n");
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let stale_skill = json_output(&run(&home, &["integrate", "codex", "status"], &envs));
    assert_eq!(stale_skill["status"], "drifted");
    assert_eq!(stale_skill["current_skill"], false);

    json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    let old_command = receipt["commands"]["Stop"].as_str().unwrap().to_owned();
    let drifted_command = format!("{old_command}-old");
    receipt["commands"]["Stop"] = json!(drifted_command);
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let mut hooks: Value =
        serde_json::from_slice(&fs::read(codex_home.join("hooks.json")).unwrap()).unwrap();
    for entry in hooks["hooks"]["Stop"].as_array_mut().unwrap() {
        if entry["hooks"][0]["command"] == old_command {
            entry["hooks"][0]["command"] = json!(drifted_command);
        }
    }
    fs::write(
        codex_home.join("hooks.json"),
        serde_json::to_vec(&hooks).unwrap(),
    )
    .unwrap();
    let stale_hooks = json_output(&run(&home, &["integrate", "codex", "status"], &envs));
    assert_eq!(stale_hooks["status"], "drifted");
    assert_eq!(stale_hooks["current_hooks"], false);
    json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    assert_eq!(
        json_output(&run(&home, &["integrate", "codex", "status"], &envs))["status"],
        "installed"
    );
}

#[test]
fn claude_skill_installs_reports_drift_and_refuses_a_modified_copy() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let claude_home = temp.path().join("claude");
    let envs = [("CLAUDE_CONFIG_DIR", claude_home.to_str().unwrap())];
    let skill_path = claude_home.join("skills/brgr/SKILL.md");

    let installed = json_output(&run(&home, &["integrate", "claude", "install"], &envs));
    assert_eq!(installed["status"], "installed");
    assert!(
        fs::read_to_string(&skill_path)
            .unwrap()
            .contains("name: brgr")
    );
    assert_eq!(
        json_output(&run(&home, &["integrate", "claude", "status"], &envs))["status"],
        "installed"
    );

    fs::write(&skill_path, "hand-edited\n").unwrap();
    assert_eq!(
        json_output(&run(&home, &["integrate", "claude", "status"], &envs))["status"],
        "drifted"
    );
    let refused = run(&home, &["integrate", "claude", "install"], &envs);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("refusing to overwrite"));
    assert_eq!(fs::read_to_string(&skill_path).unwrap(), "hand-edited\n");

    let removed = json_output(&run(&home, &["integrate", "claude", "uninstall"], &envs));
    assert_eq!(removed["skill_removed"], false);
    assert!(skill_path.exists());
    assert_eq!(
        json_output(&run(&home, &["integrate", "claude", "status"], &envs))["status"],
        "not_installed"
    );
}

#[test]
fn codex_integration_accepts_an_equivalent_caller_and_detects_hook_binary_drift() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let hook_binary = temp.path().join("installed-brgr");
    fs::copy(brgr(), &hook_binary).unwrap();
    fs::set_permissions(&hook_binary, fs::Permissions::from_mode(0o700)).unwrap();

    let installed = output_when_not_busy(
        Command::new(&hook_binary)
            .arg("--home")
            .arg(&home)
            .arg("--json")
            .args(["integrate", "codex", "install"])
            .env("CODEX_HOME", &codex_home),
    );
    assert!(installed.status.success());
    let envs = [("CODEX_HOME", codex_home.to_str().unwrap())];
    let equivalent = json_output(&run(&home, &["integrate", "codex", "status"], &envs));
    assert_eq!(equivalent["status"], "installed");
    assert_eq!(equivalent["current_hooks"], true);

    fs::OpenOptions::new()
        .append(true)
        .open(&hook_binary)
        .unwrap()
        .write_all(b"drift")
        .unwrap();
    let drifted = json_output(&run(&home, &["integrate", "codex", "status"], &envs));
    assert_eq!(drifted["status"], "drifted");
    assert_eq!(drifted["current_hooks"], false);
}

#[test]
fn missing_plugin_bridge_fails_before_task_admission() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let output = brgr_command()
        .arg("--home")
        .arg(&home)
        .args(["run", "BRGR_FIXTURE_OK"])
        .env("BRGR_PLUGIN_BRIDGE_DIR", temp.path().join("missing"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bridge directory is unavailable"));
    assert!(!home.join("store/brgr.sqlite3").exists());
}

#[test]
fn doctor_reports_changed_harness_instead_of_ok() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let executable = temp.path().join("fixture");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/brgr-fixture");
    fs::copy(fixture, &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let envs = [("CODEX_HOME", codex_home.to_str().unwrap())];
    json_output(&run(&home, &["integrate", "codex", "install"], &envs));

    let healthy = json_output(&run(&home, &["doctor"], &envs));
    assert_eq!(healthy["status"], "ok");
    assert_eq!(healthy["harnesses"][0]["health"], "healthy");

    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&executable)
            .unwrap(),
        "# drift"
    )
    .unwrap();
    let changed = run(&home, &["doctor"], &envs);
    assert!(!changed.status.success());
    let report: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(report["status"], "needs_attention");
    assert_eq!(report["harnesses"][0]["id"], "local.fixture");
    assert_eq!(report["harnesses"][0]["health"], "executable_changed");
    let action = report["harnesses"][0]["action"].as_str().unwrap();
    assert!(action.contains("re-certify"));
    assert!(action.contains("brgr harness recertify local.fixture"));

    let scratch = temp.path().join("scratch");
    let recertified = run(
        &home,
        &[
            "harness",
            "recertify",
            "local.fixture",
            "--workspace",
            scratch.to_str().unwrap(),
        ],
        &envs,
    );
    assert!(recertified.status.success(), "{recertified:?}");
    let healed = json_output(&run(&home, &["doctor"], &envs));
    assert_eq!(healed["harnesses"][0]["health"], "healthy");
    let dumped = String::from_utf8_lossy(&changed.stdout);
    assert!(!dumped.contains("--mode=<value>"));
    assert!(!dumped.contains("BRGR_FIXTURE_OK"));
}

/// A copy of the Fixture fixture registered, then changed as an update would.
fn drifted_fixture_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("fixture");
    fs::copy(brgr_fixture(), &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&executable)
            .unwrap(),
        "# update"
    )
    .unwrap();
    (temp, home, workspace)
}

#[test]
fn a_run_stops_on_an_updated_harness_unless_auto_recertify_is_on() {
    let (_temp, home, workspace) = drifted_fixture_home();
    let envs = [("BRGR_OWNER_ID", "codex:recertify-test")];
    let args = [
        "run",
        "BRGR_FIXTURE_OK",
        "--workspace",
        workspace.to_str().unwrap(),
        "--foreground",
    ];

    let stopped = run(&home, &args, &envs);
    assert!(!stopped.status.success());
    let stderr = String::from_utf8_lossy(&stopped.stderr);
    assert!(stderr.contains("executable_changed"), "{stderr}");
    assert!(
        stderr.contains("brgr harness recertify local.fixture"),
        "{stderr}"
    );
    assert!(!stderr.contains("warning"), "{stderr}");

    json_output(&run(
        &home,
        &["config", "set-auto-recertify", "true"],
        &envs,
    ));
    let created = run(&home, &args, &envs);
    let stderr = String::from_utf8_lossy(&created.stderr);
    assert!(
        stderr.contains("warning: local.fixture changed (executable_changed"),
        "{stderr}"
    );
    assert!(stderr.contains("set-auto-recertify false"), "{stderr}");
    assert_eq!(json_output(&created)["outcome"], "candidate");
    assert_eq!(
        json_output(&run(&home, &["harness", "status", "local.fixture"], &envs))["health"],
        "healthy"
    );
}

#[test]
fn a_failed_automatic_recertification_still_stops_the_run() {
    let (temp, home, workspace) = drifted_fixture_home();
    let envs = [("BRGR_OWNER_ID", "codex:recertify-test")];
    json_output(&run(
        &home,
        &["config", "set-auto-recertify", "true"],
        &envs,
    ));
    // The updated CLI no longer runs at all.
    fs::write(temp.path().join("fixture"), "#!/bin/sh\nexit 1\n").unwrap();
    let failed = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    );
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("automatic re-certification of local.fixture failed"),
        "{stderr}"
    );
}

#[test]
fn a_run_without_a_harness_uses_the_only_registered_one_and_refuses_to_pick() {
    let (temp, home, workspace) = fixture_home();
    let envs = [("BRGR_OWNER_ID", "codex:default-harness")];
    let args = [
        "run",
        "BRGR_FIXTURE_OK",
        "--workspace",
        workspace.to_str().unwrap(),
        "--foreground",
    ];
    assert_eq!(
        json_output(&run(&home, &args, &envs))["outcome"],
        "candidate"
    );

    add_slow_prompt_file_harness(&home, &temp.path().join("slow"));
    let refused = run(&home, &args, &envs);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("no harness named and no default configured"),
        "{stderr}"
    );
    assert!(
        stderr.contains("local.fixture") && stderr.contains("local.slow-agent"),
        "{stderr}"
    );
}

/// A repository whose `docs.md` a worker appended `TASK` to, accepted, and
/// then a commit on the target changed `docs.md` too (`main_edit`).
fn accepted_append_then_main_edit(main_edit: &str) -> (TempDir, PathBuf, PathBuf, String) {
    append_then_main_edit(main_edit, true)
}

/// The same, optionally leaving the candidate undecided.
fn append_then_main_edit(main_edit: &str, accept: bool) -> (TempDir, PathBuf, PathBuf, String) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    fs::write(repository.join("docs.md"), "a\nb\nc\n").unwrap();
    for argv in [&["add", "docs.md"][..], &["commit", "-m", "docs"][..]] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(argv)
                .status()
                .unwrap()
                .success()
        );
    }
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-3way-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'a\\nb\\nc\\nTASK\\n' > docs.md\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "append TASK",
            "--workspace",
            repository.to_str().unwrap(),
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap().to_owned();
    if accept {
        json_output(&run(&home, &["accept", &task], &owner));
    }
    fs::write(repository.join("docs.md"), main_edit).unwrap();
    for argv in [&["add", "docs.md"][..], &["commit", "-m", "main edit"][..]] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(argv)
                .status()
                .unwrap()
                .success()
        );
    }
    (temp, home, repository, task)
}

#[test]
fn apply_three_way_marks_a_conflict_instead_of_refusing_everything() {
    let (_temp, home, repository, task) = accepted_append_then_main_edit("a\nb\nc\nMAIN\n");
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();

    let plain = run(
        &home,
        &["apply", &task, "--workspace", repo, "--execute"],
        &owner,
    );
    assert!(!plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stderr).contains("--3way"));

    let dry = run(
        &home,
        &["apply", &task, "--workspace", repo, "--3way"],
        &owner,
    );
    assert!(!dry.status.success());
    let report: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(report["status"], "ready_with_conflicts");
    assert_eq!(report["conflicts"][0], "docs.md");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "a\nb\nc\nMAIN\n",
        "a dry run must not touch the target"
    );

    let applied = run(
        &home,
        &["apply", &task, "--workspace", repo, "--3way", "--execute"],
        &owner,
    );
    assert!(!applied.status.success());
    let report: Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(report["status"], "applied_with_conflicts");
    let docs = fs::read_to_string(repository.join("docs.md")).unwrap();
    assert!(
        docs.contains("<<<<<<<") && docs.contains("MAIN") && docs.contains("TASK"),
        "{docs}"
    );
    let unmerged = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&unmerged.stdout).trim(), "docs.md");
}

/// A candidate that rewrote `a` and `z`, in a repository whose `a` has since
/// changed on the target (a conflict) and whose `z` is edited but not staged
/// (git refuses to touch it). Returns the home, repository and task.
fn candidate_with_conflict_and_dirty_file() -> (TempDir, PathBuf, PathBuf, String) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    for name in ["a", "z"] {
        fs::write(repository.join(name), "base\n").unwrap();
    }
    for argv in [&["add", "a", "z"][..], &["commit", "-m", "files"][..]] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(argv)
                .status()
                .unwrap()
                .success()
        );
    }
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-mixed-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'task\\n' > a\nprintf 'task\\n' > z\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "rewrite a and z",
            "--workspace",
            repository.to_str().unwrap(),
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap().to_owned();
    for name in ["a", "z"] {
        fs::write(repository.join(name), "main\n").unwrap();
    }
    for argv in [&["add", "a", "z"][..], &["commit", "-m", "main"][..]] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(argv)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repository.join("z"), "dirty\n").unwrap();
    (temp, home, repository, task)
}

/// `git apply --3way` reports a conflict in one file and a hard refusal in
/// another in the same run, and applies nothing. That is a failure, not a
/// merge to resolve: no decision, no receipt claiming conflicts, no advice to
/// resolve markers that were never written.
#[test]
fn land_three_way_is_a_failure_not_a_conflict_when_git_refuses_another_file() {
    let (_temp, home, repository, task) = candidate_with_conflict_and_dirty_file();
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let landed = run(
        &home,
        &[
            "land",
            &task,
            "--workspace",
            repo,
            "--3way",
            "--reason",
            "reviewed",
        ],
        &owner,
    );
    assert!(
        !landed.status.success(),
        "land succeeded over a dirty file.\nstdout: {}\nstderr: {}\nsealed diff: {:?}",
        String::from_utf8_lossy(&landed.stdout),
        String::from_utf8_lossy(&landed.stderr),
        String::from_utf8_lossy(&run(&home, &["diff", &task], &owner).stdout),
    );
    let stderr = String::from_utf8_lossy(&landed.stderr);
    assert!(!stderr.contains("resolve them"), "{stderr}");
    assert!(stderr.contains("does not match index"), "{stderr}");
    assert_eq!(fs::read_to_string(repository.join("a")).unwrap(), "main\n");
    assert_eq!(fs::read_to_string(repository.join("z")).unwrap(), "dirty\n");
    // Nothing was decided: the owner can still reject it.
    json_output(&run(
        &home,
        &["reject", &task, "--reason", "rework"],
        &owner,
    ));
}

/// A `git` that passes everything through except the real `git apply` (not
/// `--check`), which it fails once: the diff checks out, then the write fails.
fn git_that_fails_the_real_apply_once(dir: &Path) -> String {
    let bin = dir.join("flaky-bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = dir.join("failed-once");
    write_fast_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\napply=0; check=0\nfor a in \"$@\"; do\n  [ \"$a\" = apply ] && apply=1\n  [ \"$a\" = --check ] && check=1\ndone\nif [ $apply = 1 ] && [ $check = 0 ] && [ ! -e '{marker}' ]; then\n  : > '{marker}'\n  echo 'error: injected transient write failure' >&2\n  exit 1\nfi\nexec /usr/bin/git \"$@\"\n",
            marker = marker.display()
        ),
    );
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

/// `land` accepts, then applies. If that apply fails for a reason that leaves
/// the repository untouched, the error names `brgr apply` to retry, and the
/// retry has to work: a failed apply must not leave a receipt that blocks it.
#[test]
fn a_failed_apply_after_land_can_be_retried_as_the_error_says() {
    let (temp, home, repository, task) = append_then_main_edit("A\nb\nc\n", false);
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let flaky_path = git_that_fails_the_real_apply_once(temp.path());
    let landed = run(
        &home,
        &[
            "land",
            &task,
            "--workspace",
            repo,
            "--3way",
            "--reason",
            "reviewed",
        ],
        &[
            ("BRGR_OWNER_ID", "codex:three-way"),
            ("PATH", flaky_path.as_str()),
        ],
    );
    assert!(!landed.status.success());
    let stderr = String::from_utf8_lossy(&landed.stderr);
    assert!(stderr.contains("brgr apply"), "{stderr}");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "A\nb\nc\n",
        "the failed apply must not have touched the repository"
    );

    let retried = json_output(&run(
        &home,
        &["apply", &task, "--workspace", repo, "--3way", "--execute"],
        &owner,
    ));
    assert_eq!(retried["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "A\nb\nc\nTASK\n"
    );
}

#[test]
fn land_accepts_and_applies_in_one_step_and_leaves_nothing_to_acknowledge() {
    let (_temp, home, repository, task) = append_then_main_edit("A\nb\nc\n", false);
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let landed = json_output(&run(
        &home,
        &[
            "land",
            &task,
            "--workspace",
            repo,
            "--3way",
            "--reason",
            "reviewed",
        ],
        &owner,
    ));
    assert_eq!(landed["decision"]["verdict"], "accepted");
    assert_eq!(landed["apply"]["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "A\nb\nc\nTASK\n"
    );
    // The decision acknowledged the result: no failure banner, no ack needed.
    let status = run(&home, &["status", &task], &owner);
    assert!(!String::from_utf8_lossy(&status.stderr).contains("not acknowledged"));
}

#[test]
fn land_records_no_decision_when_the_diff_cannot_be_applied() {
    let (_temp, home, repository, task) = append_then_main_edit("a\nb\nc\nMAIN\n", false);
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let refused = run(
        &home,
        &["land", &task, "--workspace", repo, "--reason", "reviewed"],
        &owner,
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--3way"), "{stderr}");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "a\nb\nc\nMAIN\n"
    );
    // Still undecided: it can be landed later, or rejected.
    json_output(&run(
        &home,
        &["reject", &task, "--reason", "rework"],
        &owner,
    ));
}

#[test]
fn land_with_three_way_accepts_and_leaves_conflict_markers_to_resolve() {
    let (_temp, home, repository, task) = append_then_main_edit("a\nb\nc\nMAIN\n", false);
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let landed = run(
        &home,
        &[
            "land",
            &task,
            "--workspace",
            repo,
            "--3way",
            "--reason",
            "reviewed",
        ],
        &owner,
    );
    assert!(!landed.status.success());
    let report: Value = serde_json::from_slice(&landed.stdout).unwrap();
    assert_eq!(report["decision"]["verdict"], "accepted");
    assert_eq!(report["apply"]["status"], "applied_with_conflicts");
    assert!(
        fs::read_to_string(repository.join("docs.md"))
            .unwrap()
            .contains("<<<<<<<")
    );
}

/// A revision starts from the repository's current HEAD, not from the base of
/// the revision it replaces, so work merged in between does not make it conflict.
#[test]
fn a_revision_starts_from_the_current_head_not_the_old_base() {
    let (_temp, home, repository, task) = append_then_main_edit("A\nb\nc\n", false);
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let old = json_output(&run(&home, &["status", &task], &owner));
    let old_base = old["base_commit"].as_str().unwrap().to_owned();
    json_output(&run(
        &home,
        &["reject", &task, "--reason", "redo on current HEAD"],
        &owner,
    ));
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    assert_ne!(
        head, old_base,
        "the repository moved after the first revision"
    );

    let revised = json_output(&run(
        &home,
        &[
            "revise",
            &task,
            "append TASK again",
            "--workspace",
            repo,
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(revised["revision"], 2);
    let status = json_output(&run(&home, &["status", &task], &owner));
    assert_eq!(status["base_commit"], head.as_str(), "{status}");
}

/// Another session can read a task with `--observe`, and only read it: the
/// owner's session is still needed to decide, acknowledge, or apply.
#[test]
fn observe_reads_another_sessions_task_and_changes_nothing() {
    let (_temp, home, repository, task) = append_then_main_edit("A\nb\nc\n", false);
    let repo = repository.to_str().unwrap();
    // The helper's runs are owned by the runner's default session.
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let other = [
        ("BRGR_OWNER_ID", "codex:three-way"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
    json_output(&run(&home, &["status", &task], &owner));

    for args in [
        vec!["diff", &task],
        vec!["result", &task],
        vec!["status", &task],
    ] {
        let denied = run(&home, &args, &other);
        assert!(!denied.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&denied.stderr).contains("bound to another session"),
            "{args:?}"
        );
        let mut observed_args = args.clone();
        observed_args.push("--observe");
        let seen = run(&home, &observed_args, &other);
        assert!(
            seen.status.success(),
            "{observed_args:?}: {}",
            String::from_utf8_lossy(&seen.stderr)
        );
        assert!(
            String::from_utf8_lossy(&seen.stderr).contains("observing a task owned by"),
            "{observed_args:?}"
        );
    }
    let diff = String::from_utf8(run(&home, &["diff", &task, "--observe"], &other).stdout).unwrap();
    assert!(diff.contains("+TASK"), "{diff}");

    // Reading never decides, and observing cannot acknowledge.
    let combined = run(&home, &["result", &task, "--observe", "--ack"], &other);
    assert!(!combined.status.success());
    for args in [
        vec!["accept", &task, "--reason", "x"],
        vec!["land", &task, "--workspace", repo, "--reason", "x"],
        vec!["apply", &task, "--workspace", repo],
    ] {
        let refused = run(&home, &args, &other);
        assert!(!refused.status.success(), "{args:?}");
    }
    // The owner still decides.
    json_output(&run(&home, &["accept", &task, "--reason", "owner"], &owner));
}

#[test]
fn apply_three_way_merges_cleanly_when_the_edits_do_not_overlap() {
    let (_temp, home, repository, task) = accepted_append_then_main_edit("A\nb\nc\n");
    let owner = [("BRGR_OWNER_ID", "codex:three-way")];
    let repo = repository.to_str().unwrap();
    let applied = json_output(&run(
        &home,
        &["apply", &task, "--workspace", repo, "--3way", "--execute"],
        &owner,
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("docs.md")).unwrap(),
        "A\nb\nc\nTASK\n"
    );
}

#[test]
fn named_process_harness_cannot_activate_without_authorized_scratch() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let fixture = brgr_fixture();
    let denied = run(&home, &["harness", "add", fixture.to_str().unwrap()], &[]);
    assert!(!denied.status.success());
    assert!(
        !home
            .join("registry/activations/local.fixture.json")
            .exists()
    );

    let unsafe_scratch = home.join("store");
    fs::create_dir_all(&unsafe_scratch).unwrap();
    let overlapping = run(
        &home,
        &[
            "harness",
            "add",
            fixture.to_str().unwrap(),
            "--workspace",
            unsafe_scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    );
    assert!(!overlapping.status.success());
    assert!(String::from_utf8_lossy(&overlapping.stderr).contains("control directory"));
    assert!(
        !home
            .join("registry/activations/local.fixture.json")
            .exists()
    );

    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let health = json_output(&run(&home, &["harness", "status", "local.fixture"], &[]));
    assert_eq!(health["health"], "healthy");

    let activation_path = home.join("registry/activations/local.fixture.json");
    let mut old_receipt: Value =
        serde_json::from_slice(&fs::read(&activation_path).unwrap()).unwrap();
    old_receipt["scratch_result_digest"] = Value::Null;
    fs::write(&activation_path, serde_json::to_vec(&old_receipt).unwrap()).unwrap();
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let blocked = run(
        &home,
        &[
            "run",
            "must not start",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:old-activation")],
    );
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("scratch certification"));
    assert_eq!(
        fs::read_dir(home.join("launches")).map_or(0, Iterator::count),
        0
    );
}

#[test]
fn herdr_unknown_model_stops_before_git_worktree_and_task_admission() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let binaries = temp.path().join("bin");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&binaries).unwrap();
    fs::create_dir_all(&repository).unwrap();
    let omp = binaries.join("omp");
    write_fast_script(
        &omp,
        "#!/bin/sh\ncase \"$1\" in models) echo '{\"models\":[]}';; *) exit 2;; esac\n",
    );
    let launcher = binaries.join("omp-role");
    write_fast_script(
        &launcher,
        "#!/bin/sh\necho '--expected-report --reuse-worktree-objective --reuse-worktree-owner --model --effort'\n",
    );
    let path = format!("{}:/usr/bin:/bin", binaries.display());
    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            launcher.to_str().unwrap(),
            "--presentation-only",
        ],
        &[("PATH", &path)],
    ));
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repository)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(repository.join("README"), b"clean fixture\n").unwrap();
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "add", "README"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "seed",
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let denied = run(
        &home,
        &[
            "run",
            "MUST_NOT_START",
            "--harness",
            "local.omp-herdr",
            "--model",
            "fake/missing",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[("PATH", &path), ("BRGR_OWNER_ID", "codex:herdr-preflight")],
    );
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("absent from the current catalog"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(fs::read_dir(home.join("launches")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(home.join("worktrees")).unwrap().count(), 0);
}

#[test]
fn installed_hooks_preserve_foreign_entries_and_uninstall_exact_ownership() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    fs::create_dir_all(&codex_home).unwrap();
    let hooks_path = codex_home.join("hooks.json");
    let original =
        json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"foreign-hook"}]}]}});
    fs::write(&hooks_path, serde_json::to_vec(&original).unwrap()).unwrap();
    let codex_home_value = codex_home.to_str().unwrap();
    let envs = [("CODEX_HOME", codex_home_value)];

    let first = json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    assert_eq!(first["hooks_added"], 3);
    let second = json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    assert_eq!(second["hooks_added"], 0);
    assert_eq!(
        json_output(&run(&home, &["integrate", "codex", "status"], &envs))["status"],
        "installed"
    );

    json_output(&run(&home, &["integrate", "codex", "uninstall"], &envs));
    let after: Value = serde_json::from_slice(&fs::read(hooks_path).unwrap()).unwrap();
    assert_eq!(after["hooks"]["Stop"], original["hooks"]["Stop"]);
    assert!(!codex_home.join("skills/brgr/SKILL.md").exists());
}

#[test]
fn conflicting_skill_does_not_change_existing_hooks() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let skill_dir = codex_home.join("skills/brgr");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), b"user-owned skill\n").unwrap();
    let hooks_path = codex_home.join("hooks.json");
    fs::write(&hooks_path, b"{\"hooks\":{}}\n").unwrap();
    let before = fs::read(&hooks_path).unwrap();
    let output = run(
        &home,
        &["integrate", "codex", "install"],
        &[("CODEX_HOME", codex_home.to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert_eq!(fs::read(&hooks_path).unwrap(), before);
    assert_eq!(
        fs::read(skill_dir.join("SKILL.md")).unwrap(),
        b"user-owned skill\n"
    );
}

#[test]
fn owned_old_skill_and_hook_upgrade_without_touching_foreign_hook() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let envs = [("CODEX_HOME", codex_home.to_str().unwrap())];
    let hooks_path = codex_home.join("hooks.json");
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(
        &hooks_path,
        br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"foreign-hook"}]}]}}"#,
    )
    .unwrap();
    json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    let receipt_path = home.join("codex-integration.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    let skill_path = codex_home.join("skills/brgr/SKILL.md");
    fs::write(&skill_path, "owned prior brgr skill\n").unwrap();
    receipt["skill_text"] = json!("owned prior brgr skill\n");
    let current_command = receipt["commands"]["Stop"].as_str().unwrap().to_owned();
    let old_command = "owned-old-brgr-hook";
    receipt["commands"]["Stop"] = json!(old_command);
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let mut hooks: Value = serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    for entry in hooks["hooks"]["Stop"].as_array_mut().unwrap() {
        if entry["hooks"][0]["command"] == current_command {
            entry["hooks"][0]["command"] = json!(old_command);
        }
    }
    fs::write(&hooks_path, serde_json::to_vec(&hooks).unwrap()).unwrap();

    json_output(&run(&home, &["integrate", "codex", "install"], &envs));
    let updated: Value = serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    let commands = updated["hooks"]["Stop"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["hooks"][0]["command"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(commands.contains(&"foreign-hook"));
    assert!(commands.contains(&current_command.as_str()));
    assert!(!commands.contains(&old_command));
    assert!(
        fs::read_to_string(skill_path)
            .unwrap()
            .contains("brgr revise")
    );
}

#[test]
fn real_cli_run_binds_candidate_to_its_owner() {
    let (_temp, home, workspace) = fixture_home();
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--criterion",
            "artifact text equals BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--deadline-seconds",
            "5",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:owner-a")],
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let current = json_output(&run(
        &home,
        &["status", task],
        &[("BRGR_OWNER_ID", "codex:owner-a")],
    ));
    assert_eq!(
        current["task"]["acceptance_criteria"][0],
        "artifact text equals BRGR_FIXTURE_OK"
    );
    let readable = json_output(&run(
        &home,
        &["result", task],
        &[("BRGR_OWNER_ID", "codex:owner-a")],
    ));
    assert_eq!(readable["artifacts"][0]["text"], "BRGR_FIXTURE_OK");
    let unreadable = run(
        &home,
        &["result", task],
        &[
            ("BRGR_OWNER_ID", "codex:owner-b"),
            ("BRGR_SESSION_ID", "other-session"),
        ],
    );
    assert!(!unreadable.status.success());
    let denied = run(
        &home,
        &["accept", task],
        &[
            ("BRGR_OWNER_ID", "codex:owner-b"),
            ("BRGR_SESSION_ID", "other-session"),
        ],
    );
    assert!(!denied.status.success());
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "sealed fixture result checked"],
        &[("BRGR_OWNER_ID", "codex:owner-a")],
    ));
    assert_eq!(accepted["verdict"], "accepted");
    let replay = json_output(&run(
        &home,
        &["accept", task, "--reason", "sealed fixture result checked"],
        &[("BRGR_OWNER_ID", "codex:owner-a")],
    ));
    assert_eq!(replay["decision_id"], accepted["decision_id"]);
}

/// A foreground run waits for the whole task, so once the worker is up the slow
/// notice must name the run, not "preparing the worktree". The fixture sleeps
/// 30s on SLOW; a 12s deadline settles it past the 8s notice.
#[test]
fn a_slow_foreground_run_says_it_is_running_not_preparing() {
    let (_temp, home, workspace) = fixture_home();
    let ran = run(
        &home,
        &[
            "run",
            "SLOW",
            "--workspace",
            workspace.to_str().unwrap(),
            "--deadline-seconds",
            "12",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:slow-notice")],
    );
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(
        stderr.contains("brgr: still running the task in the foreground..."),
        "{stderr}"
    );
    assert!(!stderr.contains("preparing the worktree"), "{stderr}");

    let fast = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:slow-notice")],
    );
    assert_eq!(json_output(&fast)["outcome"], "candidate");
    let fast_stderr = String::from_utf8_lossy(&fast.stderr);
    assert!(!fast_stderr.contains("brgr: still"), "{fast_stderr}");
}

#[test]
fn control_home_inside_worker_workspace_is_rejected_before_task_admission() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("work");
    let home = workspace.join("brgr-control");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let output = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:home-overlap")],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not overlap"));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(store.unstarted_tasks().unwrap().is_empty());
    assert!(
        store
            .inbox(
                &brgr_protocol::OwnerId::new("codex:home-overlap").unwrap(),
                false
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unbound_task_needs_explicit_session_and_stale_session_cannot_decide() {
    let (_temp, home, workspace) = fixture_home();
    let unbound = [("BRGR_OWNER_ID", "codex:unbound"), ("BRGR_SESSION_ID", "")];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &unbound,
    ));
    let task = first["task_id"].as_str().unwrap();
    assert_eq!(first["outcome"], "candidate");
    assert!(!run(&home, &["result", task], &unbound).status.success());
    assert!(!run(&home, &["accept", task], &unbound).status.success());
    let bound = json_output(&run(
        &home,
        &["bind", task, "--session", "session-a"],
        &unbound,
    ));
    let initial_epoch = bound["binding_epoch"].as_u64().unwrap();
    let session_a = [
        ("BRGR_OWNER_ID", "codex:unbound"),
        ("BRGR_SESSION_ID", "session-a"),
    ];
    assert_eq!(
        json_output(&run(&home, &["result", task], &session_a))["artifacts"][0]["text"],
        "BRGR_FIXTURE_OK"
    );
    let session_b = [("BRGR_SESSION_ID", "session-b")];
    let rebound = json_output(&run(&home, &["bind", task], &session_b));
    assert!(rebound["binding_epoch"].as_u64().unwrap() > initial_epoch);
    assert!(!run(&home, &["result", task], &session_a).status.success());
    assert!(!run(&home, &["accept", task], &session_a).status.success());
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "new session verified fixture"],
        &session_b,
    ));
    assert_eq!(accepted["verdict"], "accepted");
    assert_eq!(accepted["session_id"], "session-b");
    assert_eq!(accepted["binding_epoch"], rebound["binding_epoch"]);
}

#[test]
fn non_git_workspace_runs_without_a_git_executable() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("non-git");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[
            ("BRGR_OWNER_ID", "codex:no-git"),
            ("PATH", "/bin:/usr/sbin:/sbin"),
        ],
    ));
    assert_eq!(result["outcome"], "candidate");
}

#[test]
fn unsupported_model_fails_before_task_admission() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("prompt-only");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'prompt-only 1';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file) /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n",
    );
    json_output(&run(
        &home,
        &[
            "harness",
            "activate",
            executable.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));

    let attempted = run(
        &home,
        &[
            "run",
            "should not launch",
            "--harness",
            "local.prompt-only",
            "--model",
            "gpt-5.6-luna",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!attempted.status.success());
    assert!(String::from_utf8_lossy(&attempted.stderr).contains("model_select"));
    assert!(
        fs::read_dir(home.join("launches"))
            .unwrap()
            .next()
            .is_none()
    );
    let owner = [("BRGR_OWNER_ID", "codex:generic-owner")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "GENERIC_PROCESS_OK",
            "--harness",
            "local.prompt-only",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let sealed = json_output(&run(&home, &["result", task], &owner));
    assert_worker_prompt(
        sealed["artifacts"][0]["text"].as_str().unwrap(),
        "GENERIC_PROCESS_OK",
    );
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "generic result checked"],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
}

#[test]
fn missing_write_browser_and_mcp_capabilities_fail_before_task_admission() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:capability-preflight")];
    for (flag, value, missing) in [
        ("--requires-write", None, "workspace_write"),
        ("--requires-browser", None, "browser"),
        ("--requires-mcp", Some("figma"), "mcp:figma"),
    ] {
        let mut args = vec![
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            flag,
        ];
        if let Some(value) = value {
            args.push(value);
        }
        let rejected = run(&home, &args, &owner);
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(missing));
    }
    assert!(
        fs::read_dir(home.join("launches"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        fs::read_dir(home.join("worktrees"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn devin_process_recipe_reaches_owner_acceptance_without_shell_interpolation() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let executable = temp.path().join("devin");
    write_devin_fixture(&executable);

    let activation = json_output(&run(
        &home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "DEVIN_SCRATCH_OK",
        ],
        &[],
    ));
    assert_eq!(activation["harness_id"], "local.devin");
    assert!(activation["scratch_result_digest"].as_str().is_some());
    let health = json_output(&run(&home, &["harness", "status", "local.devin"], &[]));
    assert_eq!(health["health"], "healthy");

    let owner = [("BRGR_OWNER_ID", "codex:devin-owner")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "DEVIN_PROCESS_OK",
            "--harness",
            "local.devin",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    assert_worker_prompt(
        detail["artifacts"][0]["text"].as_str().unwrap(),
        "DEVIN_PROCESS_OK",
    );
    assert_eq!(detail["route_observation"]["model_source"], "unavailable");
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "Devin fixture result verified"],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
}

#[test]
fn devin_print_run_passes_the_requested_model() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let executable = temp.path().join("devin");
    write_devin_fixture(&executable);

    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "DEVIN_SCRATCH_OK",
        ],
        &[],
    ));

    let owner = [("BRGR_OWNER_ID", "codex:devin-model")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "DEVIN_PROCESS_OK",
            "--harness",
            "local.devin",
            "--model",
            "swe-2-high",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    let text = detail["artifacts"][0]["text"].as_str().unwrap();
    assert_worker_prompt(text, "DEVIN_PROCESS_OK");
    assert!(
        text.contains("MODEL=swe-2-high"),
        "model missing from fixture output: {text}"
    );
}

#[test]
fn old_devin_without_model_flag_keeps_the_configured_default_error() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let executable = temp.path().join("devin");
    write_fast_script(&executable, DEVIN_OLD_FIXTURE);

    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "DEVIN_SCRATCH_OK",
        ],
        &[],
    ));

    let owner = [("BRGR_OWNER_ID", "codex:devin-old")];
    let explicit_model = run(
        &home,
        &[
            "run",
            "must not start",
            "--harness",
            "local.devin",
            "--model",
            "swe-1.6",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &owner,
    );
    assert!(!explicit_model.status.success());
    let stderr = String::from_utf8_lossy(&explicit_model.stderr);
    assert!(
        stderr.contains(
            "harness local.devin does not support required capability: model_select; it runs only the model set in its own configuration, so change it in that CLI's own settings, then run without --model"
        ),
        "{stderr}"
    );
    assert_eq!(fs::read_dir(home.join("launches")).unwrap().count(), 0);
}

/// A harness an agent authored a manifest for is certified again from that
/// manifest. Redrafting it from help text, as for a known CLI, fails for a CLI
/// whose flags no recipe describes, so an update would strand it.
#[test]
fn an_agent_authored_harness_is_recertified_from_its_own_manifest() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let executable = temp.path().join("positional-agent");
    // Unique inode: this test appends to the file and asserts `executable_changed`.
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'positional 1';;\n --help) echo '  -p, --print prompt';;\n -p) printf '%s' \"$2\";;\n *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest_path = temp.path().join("manifest.json");
    let manifest = json!({
        "schema": "brgr.harness/v1",
        "id": "local.positional-agent",
        "adapter": "process/v1",
        "executable": executable.canonicalize().unwrap(),
        "probe": {"version_argv": ["--version"], "help_argv": ["--help"]},
        "launch": {
            "argv": ["-p", "${input.prompt}"],
            "model_argv": [], "effort_argv": [],
            "env_allow": ["HOME", "PATH"], "mode": "one_shot"
        },
        "result": {
            "source": {"kind": "stdout"}, "media_type": "text/plain",
            "max_bytes": 1024, "success_exit_codes": [0]
        },
        "capabilities": {
            "completion": {
                "status": "supported", "semantics": "process_exit",
                "evidence_ref": "help", "tested_identity": null
            }
        }
    });
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    json_output(&run(
        &home,
        &[
            "harness",
            "activate",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));
    assert_eq!(
        json_output(&run(
            &home,
            &["harness", "status", "local.positional-agent"],
            &[]
        ))["health"],
        "healthy"
    );

    // The CLI updates.
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&executable)
            .unwrap(),
        "# updated"
    )
    .unwrap();
    assert_eq!(
        json_output(&run(
            &home,
            &["harness", "status", "local.positional-agent"],
            &[]
        ))["health"],
        "executable_changed"
    );

    json_output(&run(
        &home,
        &[
            "harness",
            "recertify",
            "local.positional-agent",
            "--workspace",
            scratch.to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(
        json_output(&run(
            &home,
            &["harness", "status", "local.positional-agent"],
            &[]
        ))["health"],
        "healthy"
    );
}

#[test]
fn authored_manifest_runs_unknown_positional_cli_to_owner_acceptance() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("positional-agent");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'positional 1';;\n --help) echo '  -p, --print prompt';;\n -p) printf '%s' \"$2\";;\n *) exit 2;;\nesac\n",
    );
    let manifest_path = temp.path().join("positional-manifest.json");
    let manifest = json!({
        "schema": "brgr.harness/v1",
        "id": "local.positional-agent",
        "adapter": "process/v1",
        "executable": executable.canonicalize().unwrap(),
        "probe": {"version_argv": ["--version"], "help_argv": ["--help"]},
        "launch": {
            "argv": ["-p", "${input.prompt}"],
            "model_argv": [], "effort_argv": [],
            "env_allow": ["HOME", "PATH"], "mode": "one_shot"
        },
        "result": {
            "source": {"kind": "stdout"}, "media_type": "text/plain",
            "max_bytes": 1024, "success_exit_codes": [0]
        },
        "capabilities": {
            "completion": {
                "status": "supported", "semantics": "process_exit",
                "evidence_ref": "help", "tested_identity": null
            }
        }
    });
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let path = manifest_path.to_str().unwrap();
    let tested = json_output(&run(&home, &["harness", "test", "--manifest", path], &[]));
    assert_eq!(tested["contract"], "passed");
    json_output(&run(
        &home,
        &[
            "harness",
            "activate",
            "--manifest",
            path,
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));
    let health = json_output(&run(
        &home,
        &["harness", "status", "local.positional-agent"],
        &[],
    ));
    assert_eq!(health["health"], "healthy");
    let owner = [("BRGR_OWNER_ID", "codex:custom-manifest")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "CUSTOM_MANIFEST_OK",
            "--harness",
            "local.positional-agent",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let sealed = json_output(&run(&home, &["result", task], &owner));
    assert_worker_prompt(
        sealed["artifacts"][0]["text"].as_str().unwrap(),
        "CUSTOM_MANIFEST_OK",
    );
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "custom result checked"],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
}

#[cfg(debug_assertions)]
#[test]
fn detached_supervisor_exit_before_claim_becomes_one_durable_lost_inbox_item() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:admission-crash")];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "no model effect",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[owner[0], ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let first_status = json_output(&run(&home, &["status", task], &owner));
    assert_eq!(first_status["state"], "queued");
    let terminal = settle(&home, task, &owner).expect("abandoned admitted task did not settle");
    assert_eq!(terminal["result"]["outcome"], "lost");
    assert_eq!(terminal["artifacts"].as_array().unwrap().len(), 0);
    let result_id = terminal["result"]["result_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert_eq!(
        store
            .inbox(
                &brgr_protocol::OwnerId::new("codex:admission-crash").unwrap(),
                false
            )
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        store.claim_attempt(task.parse().unwrap(), 1, brgr_protocol::AttemptId::new()),
        Err(brgr_store::StoreError::UnresolvedPriorAttempt { .. })
    ));
    json_output(&run(&home, &["status", task], &owner));
    let replay = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(replay["result"]["result_id"], result_id);
}

#[cfg(debug_assertions)]
#[test]
fn a_cancel_of_a_lost_revision_does_not_cancel_the_next_one() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:cancel-lost-revision")];
    let launch = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    active_attempt(&store, task);
    let worker_pid_path = home.join("runs").join(format!("{task}.pid"));
    assert!(
        wait_until(|| file_is_nonempty(&worker_pid_path)),
        "worker pid file was not populated"
    );
    let worker_pid = fs::read_to_string(worker_pid_path)
        .unwrap()
        .trim()
        .to_owned();
    let receipt: Value = serde_json::from_slice(
        &fs::read(home.join("runs").join(format!("{task}.supervisor.json"))).unwrap(),
    )
    .unwrap();
    let supervisor_pid = receipt
        .pointer("/identity/handle")
        .unwrap()
        .as_str()
        .unwrap();
    Command::new("/bin/kill")
        .args(["-9", supervisor_pid])
        .status()
        .unwrap();
    Command::new("/bin/kill")
        .args(["-9", &worker_pid])
        .status()
        .unwrap();
    // After the kill: a live supervisor would stop the worker and remove its pid
    // file, and the revision would end cancelled, not lost.
    json_output(&run(&home, &["cancel", task], &owner));
    let terminal = settle(&home, task, &owner).expect("cancelled dead supervisor did not settle");
    assert_eq!(terminal["result"]["outcome"], "lost");

    let revised = json_output(&run(
        &home,
        &["revise", task, "BRGR_FIXTURE_OK", "--foreground"],
        &owner,
    ));
    assert_eq!(revised["revision"], 2);
    assert_eq!(revised["outcome"], "candidate");
}

#[cfg(debug_assertions)]
#[test]
fn wait_reconciles_a_crashed_detached_supervisor_to_lost() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:wait-crash")];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "crash before claim",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[owner[0], ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let waited = json_output(&run(
        &home,
        &["wait", task, "--timeout-seconds", "60"],
        &owner,
    ));
    assert_eq!(waited["outcome"], "lost");
    let result = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(waited["result_id"], result["result"]["result_id"]);
}

#[test]
fn wait_and_watch_report_a_killed_supervisor_within_the_reconcile_interval() {
    for mode in ["wait", "watch"] {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("brgr");
        let repository = temp.path().join("repo");
        fs::create_dir_all(&repository).unwrap();
        seed_git_repo(&repository);
        let slow = temp.path().join("slow");
        add_slow_prompt_file_harness(&home, &slow);
        let owner = [("BRGR_OWNER_ID", "codex:wait-watch-killed")];
        let started = json_output(&run(
            &home,
            &[
                "run",
                "HOLD_RUN",
                "--harness",
                "local.slow-agent",
                "--workspace",
                repository.to_str().unwrap(),
            ],
            &owner,
        ));
        let task = started["task_id"].as_str().unwrap();
        let worker_started = slow.join("started");
        assert!(
            wait_until(|| worker_started.is_file()),
            "worker never started"
        );
        let supervisor_record = home.join("runs").join(format!("{task}.supervisor.json"));
        let supervisor: Value =
            serde_json::from_slice(&fs::read(&supervisor_record).unwrap()).unwrap();
        let supervisor_pid = supervisor["identity"]["handle"].as_str().unwrap();
        assert!(
            Command::new("/bin/kill")
                .args(["-9", supervisor_pid])
                .status()
                .unwrap()
                .success()
        );
        assert!(wait_until(|| !process_exists(supervisor_pid)));
        let worker_pid = fs::read_to_string(home.join("runs").join(format!("{task}.pid"))).unwrap();
        assert!(
            Command::new("/bin/kill")
                .args(["-9", worker_pid.trim()])
                .status()
                .unwrap()
                .success()
        );

        let started = Instant::now();
        let output = if mode == "wait" {
            run(&home, &["wait", task, "--timeout-seconds", "20"], &owner)
        } else {
            run(&home, &["watch", task, "--timeout-seconds", "20"], &owner)
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{mode} exceeded the five-second reconciliation interval"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if mode == "wait" {
            assert_eq!(json_output(&output)["outcome"], "lost");
        } else {
            assert!(
                stdout
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|event| event["event"] == "result" && event["outcome"] == "lost"),
                "{stdout}"
            );
        }
    }
}

#[test]
fn wait_returns_promptly_when_a_sealed_result_arrives() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    let slow = temp.path().join("slow");
    add_slow_prompt_file_harness(&home, &slow);
    let owner = [("BRGR_OWNER_ID", "codex:wait-prompt-result")];
    let started = json_output(&run(
        &home,
        &[
            "run",
            "HOLD_RUN",
            "--harness",
            "local.slow-agent",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = started["task_id"].as_str().unwrap();
    assert!(
        wait_until(|| slow.join("started").is_file()),
        "worker never started"
    );

    let mut command = brgr_command();
    command
        .arg("--home")
        .arg(&home)
        .args(["--json", "wait", task, "--timeout-seconds", "20"])
        .env("BRGR_SESSION_ID", "fixture-session");
    for (key, value) in owner {
        command.env(key, value);
    }
    let mut waiter = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let released = Instant::now();
    fs::write(slow.join("release"), "release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while waiter.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "wait did not return after the result was sealed"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let output = waiter.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        released.elapsed() < Duration::from_secs(5),
        "wait delayed a sealed result until a reconciliation tick"
    );
    assert_eq!(json_output(&output)["outcome"], "candidate");
}

#[cfg(debug_assertions)]
#[test]
fn detached_crash_windows_reconcile_without_duplicate_or_overlapping_attempts() {
    let fixture = brgr_fixture();
    for (stage, expected) in [
        ("after_claim", "lost"),
        ("after_launch_intent", "lost"),
        ("after_spawn_before_pid", "lost"),
        ("after_seal_before_commit", "lost"),
        ("after_terminal_commit", "candidate"),
    ] {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("brgr");
        let workspace = temp.path().join("work");
        fs::create_dir_all(&workspace).unwrap();
        add_fixture(&home, &fixture, &temp.path().join("scratch"));
        let owner = [("BRGR_OWNER_ID", "codex:crash-window")];
        let launch = json_output(&run(
            &home,
            &[
                "run",
                "BRGR_FIXTURE_OK",
                "--workspace",
                workspace.to_str().unwrap(),
            ],
            &[owner[0], ("BRGR_TEST_CRASH_STAGE", stage)],
        ));
        let task = launch["task_id"].as_str().unwrap();
        let terminal = settle(&home, task, &owner)
            .unwrap_or_else(|| panic!("crash stage {stage} did not settle"));
        assert_eq!(terminal["result"]["outcome"], expected, "stage {stage}");
        if expected == "lost" {
            assert!(terminal["artifacts"].as_array().unwrap().is_empty());
            assert!(
                !terminal["result"]["unresolved_effects"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        } else {
            assert_eq!(terminal["artifacts"][0]["text"], "BRGR_FIXTURE_OK");
        }
        let result_id = terminal["result"]["result_id"].as_str().unwrap();
        let store = brgr_store::Store::open(home.join("store")).unwrap();
        assert!(store.run_completed(result_id.parse().unwrap()).unwrap());
        let owner_id = brgr_protocol::OwnerId::new("codex:crash-window").unwrap();
        assert_eq!(
            store.inbox(&owner_id, false).unwrap().len(),
            1,
            "stage {stage}"
        );
        assert!(
            store
                .claim_attempt(task.parse().unwrap(), 1, brgr_protocol::AttemptId::new())
                .is_err()
        );
        json_output(&run(&home, &["status", task], &owner));
        let replay = json_output(&run(&home, &["result", task], &owner));
        assert_eq!(replay["result"]["result_id"], result_id, "stage {stage}");
    }
}

#[cfg(debug_assertions)]
#[test]
fn restarted_supervisor_cannot_adopt_its_predecessors_unfinished_attempt() {
    let (_temp, home, workspace) = fixture_home();
    let owner = ("BRGR_OWNER_ID", "codex:restarted-supervisor");
    let crashed = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[owner, ("BRGR_TEST_CRASH_STAGE", "after_claim")],
    );
    assert!(!crashed.status.success());
    let launch_path = fs::read_dir(home.join("launches"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let launch: Value = serde_json::from_slice(&fs::read(&launch_path).unwrap()).unwrap();
    let task = launch["spec"]["task_id"].as_str().unwrap();

    // The replacement process is allowed to reconcile the old attempt, but
    // must not claim that its own PID proves the dead predecessor is alive.
    let restarted = run(
        &home,
        &["__supervise", launch_path.to_str().unwrap()],
        &[owner],
    );
    assert!(!restarted.status.success());
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let result = store.latest_result(task.parse().unwrap()).unwrap();
    assert_eq!(result.outcome, brgr_protocol::TerminalOutcome::Lost);
    let owner_id = brgr_protocol::OwnerId::new(owner.1).unwrap();
    assert_eq!(store.inbox(&owner_id, false).unwrap().len(), 1);
    assert!(store.unfinished_attempts().unwrap().is_empty());
}

#[cfg(debug_assertions)]
#[test]
fn status_during_live_pre_identity_window_does_not_publish_lost() {
    let (_temp, home, workspace) = fixture_home();
    let owner = ("BRGR_OWNER_ID", "codex:pre-identity");
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        // The longest pause the product allows: the window opens whenever the
        // detached supervisor gets to its claim, and stays open for the pause.
        &[owner, ("BRGR_TEST_PAUSE_AFTER_CLAIM_MS", "2000")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(
        wait_until(|| store
            .unfinished_attempts()
            .unwrap()
            .iter()
            .any(|attempt| attempt.task.task_id.to_string() == task && attempt.launch.is_none())),
        "the live pre-identity window was not observed"
    );
    // This status drives recovery inside the window. A slow machine may reach
    // it after the window closes and the run has finished, so the state it
    // prints proves nothing; a live supervisor recovered as lost shows below.
    json_output(&run(&home, &["status", task], &[owner]));
    let result = settle(&home, task, &[owner]).expect("the paused run never settled");
    assert_eq!(
        result["result"]["outcome"], "candidate",
        "a live supervisor was incorrectly recovered as lost"
    );
}

#[test]
fn queued_cancel_settles_without_starting_the_harness() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:queued-cancel")];
    let launch = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &[owner[0], ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let requested = json_output(&run(&home, &["cancel", task], &owner));
    assert_eq!(requested["state"], "cancel_requested");
    let result = settle(&home, task, &owner).expect("queued cancellation did not settle");
    assert_eq!(result["result"]["outcome"], "cancelled");
    assert!(result["artifacts"].as_array().unwrap().is_empty());
    assert!(matches!(
        brgr_store::Store::open(home.join("store"))
            .unwrap()
            .claim_attempt(task.parse().unwrap(), 1, brgr_protocol::AttemptId::new()),
        Err(brgr_store::StoreError::NonRetryablePriorAttempt { .. })
    ));
}

#[test]
fn rejected_result_can_be_revised_without_rewriting_its_decision() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:revision-owner")];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "first draft",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    let task = first["task_id"].as_str().unwrap();
    let original_result = first["result_id"].as_str().unwrap().to_owned();
    assert_eq!(first["revision"], 1);
    assert_eq!(first["outcome"], "candidate");
    json_output(&run(
        &home,
        &["reject", task, "--reason", "needs correction"],
        &owner,
    ));

    let second = json_output(&run(
        &home,
        &[
            "revise",
            task,
            "corrected draft",
            "--criterion",
            "the corrected draft is reviewable",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(second["task_id"], task);
    assert_eq!(second["revision"], 2);
    assert_eq!(second["outcome"], "candidate");
    assert_ne!(second["result_id"], original_result);
    assert!(home.join("launches").join(format!("{task}.json")).is_file());
    assert!(
        home.join("launches")
            .join(format!("{task}-r2.json"))
            .is_file()
    );
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let first_id = original_result.parse().unwrap();
    let original_decision = store.decision_for_result(first_id).unwrap().unwrap();
    assert_eq!(
        original_decision.verdict,
        brgr_protocol::DecisionVerdict::Rejected
    );
    let latest = store.latest_result(task.parse().unwrap()).unwrap();
    assert_eq!(
        latest.result_id.to_string(),
        second["result_id"].as_str().unwrap()
    );
    json_output(&run(
        &home,
        &["accept", task, "--reason", "corrected result checked"],
        &owner,
    ));
    assert_eq!(
        store
            .decision_for_result(first_id)
            .unwrap()
            .unwrap()
            .verdict,
        brgr_protocol::DecisionVerdict::Rejected
    );
    let after_accept = run(
        &home,
        &["revise", task, "a third draft", "--foreground"],
        &owner,
    );
    assert!(!after_accept.status.success());
    assert!(
        !home
            .join("launches")
            .join(format!("{task}-r3.json"))
            .exists()
    );
}

#[test]
fn omp_process_can_cancel_without_a_herdr_pane() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("omp");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'omp fixture 1'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'; exit 0;;\nesac\nfor argument in \"$@\"; do case \"$argument\" in @*) prompt_file=${argument#@};; esac; done\nif /usr/bin/grep -q SLOW \"$prompt_file\"; then /bin/sleep 30; fi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OMP_FIXTURE_OK\"}]}}' '{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}' '{\"type\":\"agent_end\"}'\n",
    );
    json_output(&run(
        &home,
        &[
            "harness",
            "activate",
            executable.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));
    let owner = [("BRGR_OWNER_ID", "codex:omp-cancel-test")];
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "SLOW",
            "--harness",
            "local.omp",
            "--workspace",
            workspace.to_str().unwrap(),
            "--deadline-seconds",
            "10",
        ],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap();
    assert!(
        wait_until(|| run(&home, &["status", task], &owner).status.success()),
        "the OMP run never became visible"
    );
    let requested = json_output(&run(&home, &["cancel", task], &owner));
    assert_eq!(requested["state"], "cancel_requested");
    let final_result = settle(&home, task, &owner).expect("the cancelled OMP run never settled");
    assert_eq!(final_result["result"]["outcome"], "cancelled");
    let acknowledged = json_output(&run(&home, &["result", task, "--ack"], &owner));
    assert_eq!(acknowledged["result"]["outcome"], "cancelled");
}

#[test]
fn omp_fallback_model_cannot_be_sealed_as_requested_model() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let executable = temp.path().join("omp");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'omp fixture 1'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'; exit 0;;\n models) echo '{\"models\":[{\"selector\":\"other/fallback\"},{\"selector\":\"workbuddy/deepseek-v4.1-flash\"}]}'; exit 0;;\nesac\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"provider\":\"other\",\"model\":\"fallback\",\"content\":[{\"type\":\"text\",\"text\":\"SHOULD_NOT_ACCEPT\"}]}}' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "fixture",
        ],
        &[],
    ));
    let owner = [("BRGR_OWNER_ID", "codex:wrong-model")];
    let unknown = run(
        &home,
        &[
            "run",
            "unknown model must not start",
            "--harness",
            "local.omp",
            "--workspace",
            workspace.to_str().unwrap(),
            "--model",
            "unknown/model",
        ],
        &owner,
    );
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("absent from the current catalog"));
    assert_eq!(fs::read_dir(home.join("launches")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(home.join("worktrees")).unwrap().count(), 0);
    let result = json_output(&run(
        &home,
        &[
            "run",
            "must use WorkBuddy",
            "--harness",
            "local.omp",
            "--workspace",
            workspace.to_str().unwrap(),
            "--model",
            "workbuddy/deepseek-v4.1-flash",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "failed");
    assert!(result["artifacts"].as_array().unwrap().is_empty());
    assert!(result["error"].as_str().unwrap().contains("other/fallback"));
    let task = result["task_id"].as_str().unwrap();
    assert!(!run(&home, &["accept", task], &owner).status.success());

    let matched = json_output(&run(
        &home,
        &[
            "run",
            "record native route",
            "--harness",
            "local.omp",
            "--workspace",
            workspace.to_str().unwrap(),
            "--model",
            "other/fallback",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(matched["outcome"], "candidate");
    let detail = json_output(&run(
        &home,
        &["result", matched["task_id"].as_str().unwrap()],
        &owner,
    ));
    assert_eq!(detail["route_observation"]["model"], "other/fallback");
    assert_eq!(detail["route_observation"]["model_source"], "harness_jsonl");
    assert_eq!(detail["route_observation"]["effort_source"], "unavailable");
}

#[test]
fn detached_success_reopens_for_an_offline_owner_then_accepts_once() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:offline-owner")];

    let launch = json_output(&run(
        &home,
        &[
            "run",
            "DETACHED_OWNER_OFFLINE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = launch["task_id"].as_str().unwrap().to_owned();
    assert_eq!(launch["state"], "starting");

    let terminal = settle(&home, &task, &owner)
        .expect("detached fixture run did not settle for its offline owner");
    assert_eq!(terminal["result"]["outcome"], "candidate");
    assert_eq!(terminal["artifacts"][0]["text"], "BRGR_FIXTURE_OK");
    let result_id = terminal["result"]["result_id"].as_str().unwrap().to_owned();

    let owner_id = brgr_protocol::OwnerId::new("codex:offline-owner").unwrap();
    let pending = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .inbox(&owner_id, false)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending[0].acknowledged);
    assert_eq!(pending[0].result.result_id.to_string(), result_id);

    let replay = json_output(&run(&home, &["result", &task], &owner));
    assert_eq!(replay["result"]["result_id"], result_id);
    let still_pending = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .inbox(&owner_id, false)
        .unwrap();
    assert_eq!(still_pending.len(), 1);
    assert_eq!(still_pending[0].result.result_id.to_string(), result_id);

    let accepted = json_output(&run(
        &home,
        &[
            "accept",
            &task,
            "--reason",
            "detached fixture output verified",
        ],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
    assert_eq!(accepted["result_id"], result_id);

    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let decision = store
        .decision_for_result(result_id.parse().unwrap())
        .unwrap()
        .expect("accepted result has no recorded decision");
    assert_eq!(decision.verdict, brgr_protocol::DecisionVerdict::Accepted);
    assert_eq!(decision.result_id.to_string(), result_id);
    assert_eq!(
        serde_json::to_value(&decision).unwrap()["decision_id"],
        accepted["decision_id"]
    );
    assert!(store.inbox(&owner_id, false).unwrap().is_empty());
    let acknowledged = store.inbox(&owner_id, true).unwrap();
    assert_eq!(acknowledged.len(), 1);
    assert!(acknowledged[0].acknowledged);
    assert_eq!(acknowledged[0].result.result_id.to_string(), result_id);
}

#[test]
fn dirty_source_is_rejected_before_creating_a_task_worktree() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    let initialized = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    fs::write(repository.join("user-note.txt"), b"uncommitted work\n").unwrap();

    let rejected = run(
        &home,
        &["run", "check", "--workspace", repository.to_str().unwrap()],
        &[("BRGR_OWNER_ID", "codex:dirty-test")],
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("uncommitted changes"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!home.join("worktrees/repo").exists());
    assert_eq!(
        fs::read_to_string(repository.join("user-note.txt")).unwrap(),
        "uncommitted work\n"
    );
    let branches = Command::new("git")
        .args([
            "-C",
            repository.to_str().unwrap(),
            "branch",
            "--list",
            "brgr/*",
        ])
        .output()
        .unwrap();
    assert!(branches.status.success());
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
}

#[test]
fn selected_dirty_files_are_copied_without_touching_other_source_changes() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    fs::write(repository.join("README"), "selected edit\n").unwrap();
    fs::write(repository.join("selected.txt"), "new selected file\n").unwrap();
    fs::write(repository.join("private-note.txt"), "keep at source\n").unwrap();
    let owner = [("BRGR_OWNER_ID", "codex:selected-dirty")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
            "--snapshot-path",
            "README",
            "--snapshot-path",
            "selected.txt",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let task_workspace = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task.parse().unwrap())
        .unwrap()
        .workspace;
    assert_eq!(
        fs::read_to_string(Path::new(&task_workspace).join("README")).unwrap(),
        "selected edit\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&task_workspace).join("selected.txt")).unwrap(),
        "new selected file\n"
    );
    assert!(!Path::new(&task_workspace).join("private-note.txt").exists());
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "selected edit\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("private-note.txt")).unwrap(),
        "keep at source\n"
    );
    let receipt: Value = serde_json::from_slice(
        &fs::read(home.join("runs").join(format!("{task}-r1.snapshot.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["entries"].as_array().unwrap().len(), 2);
    assert_eq!(receipt["entries"][1]["path"], "selected.txt");
    let rejected = run(
        &home,
        &[
            "run",
            "bad path",
            "--workspace",
            repository.to_str().unwrap(),
            "--snapshot-path",
            "../secret",
        ],
        &owner,
    );
    assert!(!rejected.status.success());
}

#[test]
fn selected_tracked_deletion_is_reproduced_in_task_worktree() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    fs::write(repository.join("remove.txt"), "tracked\n").unwrap();
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "add", "remove.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "commit",
                "-m",
                "add deletion fixture"
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::remove_file(repository.join("remove.txt")).unwrap();
    let fixture = brgr_fixture();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
            "--snapshot-path",
            "remove.txt",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:selected-delete")],
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let workspace = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task.parse().unwrap())
        .unwrap()
        .workspace;
    assert!(!Path::new(&workspace).join("remove.txt").exists());
    let receipt: Value = serde_json::from_slice(
        &fs::read(home.join("runs").join(format!("{task}-r1.snapshot.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["entries"][0]["action"], "delete");
}

fn integration_conflict_preserves_owner_edit(
    home: &Path,
    repository: &Path,
    task: &str,
    owner: &[(&str, &str)],
) {
    let subdirectory = repository.join("subdirectory");
    fs::create_dir(&subdirectory).unwrap();
    let partial = run(
        home,
        &["apply", task, "--workspace", subdirectory.to_str().unwrap()],
        owner,
    );
    assert!(!partial.status.success());
    assert!(String::from_utf8_lossy(&partial.stderr).contains("repository root"));
    fs::write(repository.join("README"), "conflicting owner edit\n").unwrap();
    assert!(
        !run(
            home,
            &["apply", task, "--workspace", repository.to_str().unwrap()],
            owner,
        )
        .status
        .success()
    );
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "conflicting owner edit\n"
    );
    fs::write(repository.join("README"), "seed\n").unwrap();
}

fn assert_png_artifact_exports(home: &Path, output: &Path, task: &str, owner: &[(&str, &str)]) {
    json_output(&run(
        home,
        &[
            "artifact",
            "export",
            task,
            "5",
            "--output",
            output.to_str().unwrap(),
        ],
        owner,
    ));
    assert_eq!(fs::read(output).unwrap(), b"PNG_BYTES");
}

fn assert_tree_waits_for_owner_approval(home: &Path, task: &str, owner: &[(&str, &str)]) {
    let status = json_output(&run(home, &["status", task, "--tree"], owner));
    assert_eq!(status["nodes"][0]["waiting_for"], "approval");
}

fn configure_misleading_external_diff(repository: &Path, script: &Path) {
    write_fast_script(script, "#!/bin/sh\nprintf 'NOT_A_PATCH\\n'\n");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["config", "diff.external"])
            .arg(script)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["config", "diff.noprefix", "true"])
            .status()
            .unwrap()
            .success()
    );
}

fn assert_sealed_patch_ignores_external_driver(detail: &Value) {
    let patch = detail["artifacts"][1]["text"].as_str().unwrap();
    assert!(patch.contains("diff --git"));
    assert!(!patch.contains("NOT_A_PATCH"));
}

#[test]
fn sealed_evidence_is_reviewed_before_separate_conflict_checked_integration() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    configure_misleading_external_diff(&repository, &temp.path().join("external-diff"));
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-evidence-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q EVIDENCE_TASK \"$prompt\"; then\n printf 'changed\\n' > README\n printf 'added\\n' > added.txt\n printf 'report\\n' > report.md\n printf 'PNG_BYTES' > screen.png\n printf 'worker log\\n' >&2\nfi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"EVIDENCE_OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:evidence")];
    let outcome = json_output(&run(
        &home,
        &[
            "run",
            "EVIDENCE_TASK",
            "--workspace",
            repository.to_str().unwrap(),
            "--capture-diff",
            "--capture-logs",
            "--evidence-file",
            "report.md",
            "--evidence-file",
            "screen.png",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(outcome["outcome"], "candidate");
    let task = outcome["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(detail["artifacts"].as_array().unwrap().len(), 6);
    assert_sealed_patch_ignores_external_driver(&detail);
    assert_tree_waits_for_owner_approval(&home, task, &owner);
    assert_eq!(
        detail["artifacts"][1]["reference"]["media_type"],
        "text/x-diff"
    );
    assert_eq!(
        detail["artifacts"][5]["reference"]["media_type"],
        "image/png"
    );
    assert!(detail["artifacts"][5]["text"].is_null());
    assert_png_artifact_exports(&home, &temp.path().join("export.png"), task, &owner);
    integration_conflict_preserves_owner_edit(&home, &repository, task, &owner);
    let check = json_output(&run(
        &home,
        &["apply", task, "--workspace", repository.to_str().unwrap()],
        &owner,
    ));
    assert_eq!(check["status"], "ready");
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n"
    );
    assert!(
        !run(
            &home,
            &[
                "apply",
                task,
                "--workspace",
                repository.to_str().unwrap(),
                "--execute"
            ],
            &owner,
        )
        .status
        .success()
    );
    json_output(&run(&home, &["accept", task], &owner));
    let applied = json_output(&run(
        &home,
        &[
            "apply",
            task,
            "--workspace",
            repository.to_str().unwrap(),
            "--execute",
        ],
        &owner,
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("added.txt")).unwrap(),
        "added\n",
        "a file the worker created without staging must reach the owner"
    );
    assert!(!repository.join("report.md").exists());
}

#[test]
fn selected_input_is_excluded_from_worker_patch_and_source_edit_survives_apply() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    fs::write(repository.join("worker.txt"), "base\n").unwrap();
    fs::write(repository.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["add", "worker.txt", ".gitattributes"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["commit", "-m", "worker fixture"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(repository.join("README"), "selected source edit\n").unwrap();
    fs::write(repository.join("selected.txt"), b"new source file\r\n").unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-snapshot-patch-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'worker edit\\n' > worker.txt\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:snapshot-patch")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "worker patch",
            "--workspace",
            repository.to_str().unwrap(),
            "--snapshot-path",
            "README",
            "--snapshot-path",
            "selected.txt",
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    let patch = detail["artifacts"][1]["text"].as_str().unwrap();
    assert!(patch.contains("worker.txt"));
    assert!(!patch.contains("README"));
    assert!(!patch.contains("selected.txt"));
    json_output(&run(&home, &["accept", task], &owner));
    let applied = json_output(&run(
        &home,
        &[
            "apply",
            task,
            "--workspace",
            repository.to_str().unwrap(),
            "--execute",
        ],
        &owner,
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "selected source edit\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("selected.txt")).unwrap(),
        "new source file\r\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("worker.txt")).unwrap(),
        "worker edit\r\n"
    );
}

#[test]
fn committed_worker_change_uses_the_task_start_commit_for_diff_and_apply() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-commit-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'committed worker edit\\n' > README\n/usr/bin/git add README\n/usr/bin/git -c user.name=Worker -c user.email=worker@example.test commit -m worker-edit >/dev/null\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:committed-patch")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "commit the change",
            "--workspace",
            repository.to_str().unwrap(),
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    assert!(
        detail["artifacts"][1]["text"]
            .as_str()
            .unwrap()
            .contains("committed worker edit")
    );
    json_output(&run(&home, &["accept", task], &owner));
    let applied = json_output(&run(
        &home,
        &[
            "apply",
            task,
            "--workspace",
            repository.to_str().unwrap(),
            "--execute",
        ],
        &owner,
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "committed worker edit\n"
    );
}

#[test]
fn subdirectory_snapshot_uses_repository_relative_paths() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let subdirectory = repository.join("sub");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&subdirectory).unwrap();
    seed_git_repo(&repository);
    fs::write(subdirectory.join("a"), "base\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["add", "sub/a"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["commit", "-m", "sub fixture"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["config", "diff.relative", "true"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(subdirectory.join("a"), "selected sub edit\n").unwrap();
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'fixture v-subdir-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'worker root edit\\n' > ../README\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    );
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:subdir-snapshot")];
    let result = json_output(&run(
        &home,
        &[
            "run",
            "subdir snapshot",
            "--workspace",
            subdirectory.to_str().unwrap(),
            "--snapshot-path",
            "a",
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(result["outcome"], "candidate");
    let task = result["task_id"].as_str().unwrap();
    let detail = json_output(&run(&home, &["result", task], &owner));
    let patch = detail["artifacts"][1]["text"].as_str().unwrap();
    assert!(patch.contains("README"), "sealed patch: {patch}");
    assert!(!patch.contains("sub/a"), "sealed patch: {patch}");
    json_output(&run(&home, &["accept", task], &owner));
    let applied = json_output(&run(
        &home,
        &[
            "apply",
            task,
            "--workspace",
            repository.to_str().unwrap(),
            "--execute",
        ],
        &owner,
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read_to_string(subdirectory.join("a")).unwrap(),
        "selected sub edit\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "worker root edit\n"
    );
}

fn seed_git_repo(repository: &Path) {
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repository)
            .output()
            .unwrap()
            .status
            .success()
    );
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
    ] {
        assert!(
            Command::new("git")
                .args(["-C", repository.to_str().unwrap(), "config", key, value])
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repository.join("README"), b"seed\n").unwrap();
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "add", "README"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "commit", "-m", "seed"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "branch", "user/keep-me"])
            .status()
            .unwrap()
            .success()
    );
}

fn git_branches(repository: &Path) -> String {
    let output = Command::new("git")
        .args(["-C", repository.to_str().unwrap(), "branch", "--list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn concurrent_git_worktree_admissions_keep_distinct_identities() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let repo = repository.to_str().unwrap().to_owned();
    let home_path = home.clone();
    thread::scope(|scope| {
        let left = scope.spawn(|| {
            run(
                &home_path,
                &[
                    "run",
                    "BRGR_FIXTURE_OK",
                    "--workspace",
                    &repo,
                    "--foreground",
                ],
                &[("BRGR_OWNER_ID", "codex:worktree-left")],
            )
        });
        let right = scope.spawn(|| {
            run(
                &home_path,
                &[
                    "run",
                    "BRGR_FIXTURE_OK",
                    "--workspace",
                    &repo,
                    "--foreground",
                ],
                &[("BRGR_OWNER_ID", "codex:worktree-right")],
            )
        });
        let left = json_output(&left.join().unwrap());
        let right = json_output(&right.join().unwrap());
        assert_eq!(left["outcome"], "candidate");
        assert_eq!(right["outcome"], "candidate");
        assert_ne!(left["task_id"], right["task_id"]);
        assert_ne!(left["result_id"], right["result_id"]);
    });
    let listed = git_branches(&repository);
    assert!(listed.contains("user/keep-me"));
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n"
    );
    let created = fs::read_dir(home.join("worktrees/repo")).unwrap().count();
    assert_eq!(created, 2);
}

#[test]
fn worktree_created_before_admission_is_preserved_and_not_reused() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let crashed = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[
            ("BRGR_OWNER_ID", "codex:worktree-gap"),
            ("BRGR_TEST_EXIT_AFTER_WORKTREE", "1"),
        ],
    );
    assert_eq!(crashed.status.code(), Some(78));
    let leftover = fs::read_dir(home.join("worktrees/repo")).unwrap();
    let leftovers: Vec<_> = leftover.map(|entry| entry.unwrap().path()).collect();
    assert_eq!(leftovers.len(), 1);
    assert!(leftovers[0].join("README").is_file());
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(store.unstarted_tasks().unwrap().is_empty());
    assert_eq!(
        home.join("launches").read_dir().map_or(0, Iterator::count),
        0
    );
    let listed = git_branches(&repository);
    assert!(listed.contains("user/keep-me"));
    assert!(listed.contains("brgr/task-"));

    let recovered = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:worktree-gap")],
    ));
    assert_eq!(recovered["outcome"], "candidate");
    assert_eq!(
        fs::read_dir(home.join("worktrees/repo")).unwrap().count(),
        2,
        "orphan worktree must be kept and a new admission must use a new path"
    );
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n"
    );
}

fn rejected_git_task(temp: &TempDir) -> (PathBuf, PathBuf, String) {
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:revise-orphan")];
    let task = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ))["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    json_output(&run(
        &home,
        &[
            "reject",
            &task,
            "--reason",
            "revise after interrupted admission",
        ],
        &owner,
    ));
    (home, repository, task)
}

#[test]
fn revise_reuses_a_clean_unadmitted_revision_after_process_death() {
    let temp = TempDir::new().unwrap();
    let (home, repository, task) = rejected_git_task(&temp);
    let owner = [("BRGR_OWNER_ID", "codex:revise-orphan")];
    let orphan = home
        .join("worktrees/repo")
        .join(format!("{}-r2", &task[..8]));
    let crashed = run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[owner[0], ("BRGR_TEST_EXIT_AFTER_WORKTREE", "1")],
    );
    assert_eq!(crashed.status.code(), Some(78));
    assert!(orphan.is_dir());

    let revised = json_output(&run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    ));
    assert_eq!(revised["outcome"], "candidate", "{revised}");
    assert_eq!(revised["revision"], 2);
    let task_id = task.parse().unwrap();
    let admitted = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task_id)
        .unwrap();
    assert_eq!(
        Path::new(&admitted.workspace),
        orphan.canonicalize().unwrap()
    );
}

#[test]
fn revise_cleans_created_files_when_admission_record_is_locked() {
    let temp = TempDir::new().unwrap();
    let (home, repository, task) = rejected_git_task(&temp);
    let database = home.join("store/brgr.sqlite3");
    let blocker = rusqlite::Connection::open(&database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let output = run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:revise-orphan")],
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("locked"));
    assert!(
        !home.join(format!("launches/{task}-r2.json")).exists(),
        "failed admission left its launch file behind"
    );
    let orphan = home
        .join("worktrees/repo")
        .join(format!("{}-r2", &task[..8]));
    assert!(
        !orphan.exists(),
        "failed admission left its worktree behind"
    );

    let revised = json_output(&run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:revise-orphan")],
    ));
    assert_eq!(revised["outcome"], "candidate", "{revised}");
    assert_eq!(revised["revision"], 2);
}

#[test]
fn revise_refuses_an_unadmitted_revision_that_contains_work() {
    let temp = TempDir::new().unwrap();
    let (home, repository, task) = rejected_git_task(&temp);
    let owner = [("BRGR_OWNER_ID", "codex:revise-orphan")];
    let orphan = home
        .join("worktrees/repo")
        .join(format!("{}-r2", &task[..8]));
    let crashed = run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[owner[0], ("BRGR_TEST_EXIT_AFTER_WORKTREE", "1")],
    );
    assert_eq!(crashed.status.code(), Some(78));
    let marker = orphan.join("owner-work.txt");
    fs::write(&marker, "keep this work\n").unwrap();

    let refused = run(
        &home,
        &[
            "revise",
            &task,
            "BRGR_FIXTURE_OK",
            "--foreground",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains(&orphan.display().to_string()));
    assert_eq!(fs::read_to_string(marker).unwrap(), "keep this work\n");
}

#[test]
fn missing_task_worktree_fails_closed_without_recreate_or_new_identity() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let started = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &[
            ("BRGR_OWNER_ID", "codex:worktree-missing"),
            ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1"),
        ],
    ));
    let task = started["task_id"].as_str().unwrap();
    let workspace = Path::new(started["workspace"].as_str().unwrap());
    assert!(workspace.is_dir());
    fs::remove_dir_all(workspace).unwrap();
    assert!(
        Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "worktree", "prune"])
            .status()
            .unwrap()
            .success()
    );
    let status = json_output(&run(
        &home,
        &["status", task],
        &[("BRGR_OWNER_ID", "codex:worktree-missing")],
    ));
    assert_eq!(status["task"]["task_id"], task);
    assert_eq!(status["workspace_present"], false);
    assert_eq!(status["state"], "terminal");
    let result = json_output(&run(
        &home,
        &["result", task],
        &[("BRGR_OWNER_ID", "codex:worktree-missing")],
    ));
    assert_eq!(result["result"]["outcome"], "lost");
    assert_eq!(result["result"]["task_id"], task);
    assert!(
        result["result"]["error"]
            .as_str()
            .unwrap()
            .contains("task worktree is missing")
    );
    assert!(!workspace.exists());
    assert!(git_branches(&repository).contains("user/keep-me"));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert_eq!(store.unstarted_tasks().unwrap().len(), 0);
}

fn add_slow_prompt_file_harness(home: &Path, root: &Path) {
    fs::create_dir_all(root.join("scratch")).unwrap();
    let executable = root.join("slow-agent");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'slow 1';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file)\n    if /usr/bin/grep -q SLOW_RUN \"$2\"; then /bin/sleep 4; fi\n    if /usr/bin/grep -q HOLD_RUN \"$2\"; then : > \"${0%/*}/started\"; n=0; while [ ! -e \"${0%/*}/release\" ] && [ $n -lt 600 ]; do /bin/sleep 0.1; n=$((n+1)); done; fi\n    /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n",
    );
    json_output(&run(
        home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            root.join("scratch").to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));
}

/// `--observe` reads. It must not run the recovery that records a dead
/// supervisor's task as lost, which `status` does for its owner.
#[test]
fn observing_a_status_does_not_recover_or_record_anything() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_slow_prompt_file_harness(&home, &temp.path().join("slow"));
    let owner = [("BRGR_OWNER_ID", "codex:observe-recovery")];
    let observer = [
        ("BRGR_OWNER_ID", "codex:observe-recovery"),
        ("BRGR_SESSION_ID", "observer-session"),
    ];
    let started = json_output(&run(
        &home,
        &[
            "run",
            "SLOW_RUN",
            "--harness",
            "local.slow-agent",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = started["task_id"].as_str().unwrap();
    let record = home.join("runs").join(format!("{task}.supervisor.json"));
    assert!(
        wait_until(|| record.is_file()),
        "the supervisor never wrote its record"
    );
    let supervisor: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    let pid = supervisor["identity"]["handle"].as_str().unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-9", pid])
            .status()
            .unwrap()
            .success()
    );
    // Recovery looks the pid up; it is gone once the system has reaped it.
    assert!(
        wait_until(|| !process_exists(pid)),
        "the killed supervisor {pid} never went away"
    );

    let seen = json_output(&run(&home, &["status", task, "--observe"], &observer));
    assert_ne!(seen["state"], "terminal", "{seen}");
    let still_open = run(&home, &["result", task, "--observe"], &observer);
    assert!(
        !still_open.status.success(),
        "observing recorded a result for the dead supervisor's task"
    );

    // The owner's own status is what recovers it.
    let recovered = json_output(&run(&home, &["status", task], &owner));
    assert_eq!(recovered["state"], "terminal", "{recovered}");
}

/// A lost run's worker can outlive its supervisor. Revising it then would start
/// a second run beside the first and, by overwriting the shared pid record,
/// hide the first worker from `prune`, which would delete its worktree.
#[test]
fn a_lost_run_whose_worker_still_runs_cannot_be_revised_until_it_stops() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_slow_prompt_file_harness(&home, &temp.path().join("slow"));
    let owner = [("BRGR_OWNER_ID", "codex:revise-live")];
    let started = json_output(&run(
        &home,
        &[
            "run",
            "HOLD_RUN",
            "--harness",
            "local.slow-agent",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = started["task_id"].as_str().unwrap();
    let record = home.join("runs").join(format!("{task}.supervisor.json"));
    assert!(
        wait_until(|| record.is_file()),
        "the supervisor never wrote its record"
    );
    // The worker must be running before its supervisor is killed: on a loaded
    // machine the supervisor's record can exist before the worker has started,
    // and a run with no live worker may rightly be revised.
    let worker_started = temp.path().join("slow").join("started");
    assert!(
        wait_until(|| worker_started.is_file()),
        "the worker never started"
    );
    let supervisor: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    let pid = supervisor["identity"]["handle"].as_str().unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-9", pid])
            .status()
            .unwrap()
            .success()
    );
    // Recovery looks the pid up; it is gone once the system has reaped it.
    assert!(
        wait_until(|| !process_exists(pid)),
        "the killed supervisor {pid} never went away"
    );
    // Recovery records the run as lost while its worker keeps sleeping.
    let lost = json_output(&run(&home, &["status", task], &owner));
    assert_eq!(lost["state"], "terminal", "{lost}");

    let refused = run(
        &home,
        &[
            "revise",
            task,
            "again",
            "--workspace",
            repository.to_str().unwrap(),
        ],
        &owner,
    );
    assert!(
        !refused.status.success(),
        "revised a run whose worker is alive"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("may still be running"), "{stderr}");
    assert!(stderr.contains("wait for it to exit"), "{stderr}");

    // Once the worker has stopped the same revision goes ahead. The worker held
    // on a file, not a timer, so it was alive for the refusal however slow the
    // machine; releasing it is what stops it.
    fs::write(temp.path().join("slow").join("release"), "").unwrap();
    let began = Instant::now();
    loop {
        let retried = run(
            &home,
            &[
                "revise",
                task,
                "again",
                "--workspace",
                repository.to_str().unwrap(),
            ],
            &owner,
        );
        if retried.status.success() {
            break;
        }
        assert!(
            began.elapsed() < HANG_GUARD,
            "{}",
            String::from_utf8_lossy(&retried.stderr)
        );
        thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn status_does_not_wait_for_a_newer_revision_and_reclaims_the_old_worktree_afterward() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    let harness_root = temp.path().join("revision-gate");
    fs::create_dir_all(harness_root.join("scratch")).unwrap();
    let executable = harness_root.join("slow-agent");
    write_fast_script(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'revision gate 1';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file)\n    if [ ! -e \"${0%/*}/probe\" ]; then : > \"${0%/*}/probe\"; elif [ ! -e \"${0%/*}/first-run\" ]; then : > \"${0%/*}/first-run\"; else : > \"${0%/*}/started\"; while [ ! -e \"${0%/*}/release\" ]; do /bin/sleep 0.1; done; fi\n    /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n",
    );
    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            harness_root.join("scratch").to_str().unwrap(),
            "--prompt",
            "scratch",
        ],
        &[],
    ));
    let owner = [("BRGR_OWNER_ID", "codex:reconcile-revision")];
    let repo = repository.to_str().unwrap();

    let first = json_output(&run(
        &home,
        &[
            "run",
            "first revision",
            "--harness",
            "local.slow-agent",
            "--workspace",
            repo,
            "--keep-worktree",
            "--foreground",
        ],
        &owner,
    ));
    let task = first["task_id"].as_str().unwrap();
    let task_id = task.parse::<brgr_protocol::TaskId>().unwrap();
    let first_spec = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .task(task_id)
        .unwrap();
    let first_worktree = PathBuf::from(first_spec.workspace);
    let _ = fs::remove_file(harness_root.join("release"));

    json_output(&run(&home, &["reject", task, "--reason", "revise"], &owner));
    let second = json_output(&run(
        &home,
        &["revise", task, "second revision", "--workspace", repo],
        &owner,
    ));
    assert_eq!(second["revision"], 2);
    assert!(
        wait_until(|| harness_root.join("started").is_file()),
        "revision 2 worker did not start"
    );

    let first_launch = home.join("launches").join(format!("{task}.json"));
    let mut launch: Value = serde_json::from_slice(&fs::read(&first_launch).unwrap()).unwrap();
    launch["keep_worktree"] = json!(false);
    launch["pane_mode"] = json!(true);
    fs::write(&first_launch, serde_json::to_vec(&launch).unwrap()).unwrap();
    fs::write(
        home.join("runs").join(format!("{task}-r1.pane.json")),
        serde_json::to_vec(&json!({
            "pane": "fixture-pane",
            "binary": "/bin/false",
            "task": task,
            "revision": 1,
            "cleanup": "closed"
        }))
        .unwrap(),
    )
    .unwrap();
    let second_launch = home.join("launches").join(format!("{task}-r2.json"));
    let mut launch: Value = serde_json::from_slice(&fs::read(&second_launch).unwrap()).unwrap();
    launch["keep_worktree"] = json!(false);
    fs::write(&second_launch, serde_json::to_vec(&launch).unwrap()).unwrap();

    let started = Instant::now();
    let status = run(&home, &["status", task], &owner);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_elapsed = started.elapsed();
    let first_worktree_retained = first_worktree.is_dir();
    fs::write(harness_root.join("release"), "go").unwrap();
    assert!(
        wait_until(|| {
            let result = run(&home, &["result", task], &owner);
            result.status.success()
                && serde_json::from_slice::<Value>(&result.stdout)
                    .is_ok_and(|value| value["result"]["revision"] == 2)
        }),
        "revision 2 did not finish"
    );
    json_output(&run(&home, &["accept", task], &owner));
    let reclaimed = run(&home, &["status", task], &owner);
    assert!(
        reclaimed.status.success(),
        "{}",
        String::from_utf8_lossy(&reclaimed.stderr)
    );
    assert!(
        !first_worktree.exists(),
        "revision 1 worktree was not reclaimed after revision 2 was decided"
    );
    assert!(
        first_worktree_retained,
        "revision 1 worktree was removed while revision 2 was running"
    );
    assert!(
        status_elapsed < Duration::from_secs(4),
        "status took {status_elapsed:?} while revision 2 was running"
    );
}

#[test]
fn foreground_admission_lock_is_released_before_slow_harness_execution() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("fixture-scratch"));
    add_slow_prompt_file_harness(&home, &temp.path().join("slow"));
    let repo = repository.to_str().unwrap().to_owned();
    let home_path = home.clone();
    // The first run's harness holds on a release file, not a timer, so it is
    // still executing whenever the second finishes however slow the machine.
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            run(
                &home_path,
                &[
                    "run",
                    "HOLD_RUN",
                    "--harness",
                    "local.slow-agent",
                    "--workspace",
                    &repo,
                    "--foreground",
                ],
                &[("BRGR_OWNER_ID", "codex:worktree-slow")],
            )
        });
        // Wait for the first run's launch envelope: the event that proves its
        // admission has begun, not a fixed window. Under load the spawn alone
        // can take seconds.
        let admitted = Instant::now();
        while home_path
            .join("launches")
            .read_dir()
            .map_or(0, Iterator::count)
            == 0
        {
            assert!(
                admitted.elapsed() < HANG_GUARD,
                "the first foreground run never reached admission"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let second = json_output(&run(
            &home_path,
            &[
                "run",
                "BRGR_FIXTURE_OK",
                "--harness",
                "local.fixture",
                "--workspace",
                &repo,
                "--foreground",
            ],
            &[("BRGR_OWNER_ID", "codex:worktree-fast")],
        ));
        assert_eq!(second["outcome"], "candidate");
        // The semantic check: the second run finished while the first's held
        // harness was still executing. A wall-clock bound on the second run's
        // whole lifecycle only measures machine speed, not the lock.
        assert!(
            !first.is_finished(),
            "second task finished only after the slow first harness completed"
        );
        fs::write(temp.path().join("slow").join("release"), "").unwrap();
        let first = json_output(&first.join().unwrap());
        assert_eq!(first["outcome"], "candidate");
        assert_ne!(first["task_id"], second["task_id"]);
        assert_ne!(first["result_id"], second["result_id"]);
    });
    assert!(git_branches(&repository).contains("user/keep-me"));
    assert_eq!(
        fs::read_to_string(repository.join("README")).unwrap(),
        "seed\n"
    );
}

/// A script reads `status --json` from stdout alone: it is one object, and the
/// worktree and base commit are at the top level.
#[test]
fn status_json_is_one_object_with_workspace_and_base_commit_at_the_top() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:status-json")];
    let created = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            repository.to_str().unwrap(),
            "--capture-diff",
            "--foreground",
        ],
        &owner,
    ));
    let task = created["task_id"].as_str().unwrap();
    let output = run(&home, &["status", task], &owner);
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).expect("stdout is one JSON object");
    assert_eq!(status["workspace"], status["task"]["workspace"]);
    assert_eq!(
        status["base_commit"],
        status["task"]["evidence"]["base_commit"]
    );
    assert!(
        status["base_commit"]
            .as_str()
            .is_some_and(|commit| commit.len() >= 7)
    );
}

// ---- Slice 2B: owner writes after a head handoff ----

/// A worker that asks once, then — only after its owner answered — announces it
/// finished, from the worker side, while the owner head is already elsewhere.
const ANNOUNCING_FIXTURE_FIXTURE: &str = r#"#!/bin/sh
set -u
case "${1:-}" in
  --version) echo 'fixture v-announce-fixture'; exit 0;;
  --help)
    printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'
    exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
result=NO_WORKER_IDENTITY
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}" && /usr/bin/grep -q '^ANNOUNCE' "$prompt_file"; then
  "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body 'Any word?' >/dev/null
  reply=$("$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds 30)
  reply_id=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"message_id":"\([^"]*\)".*/\1/p')
  "$BRGR_BIN" --json message ack "$BRGR_PARENT_TASK_ID" "$reply_id" --for worker >/dev/null || result=WORKER_ACK_FAILED
  if "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind note --body 'post-takeover note' >/dev/null; then
    result=POST_TAKEOVER_NOTE_SENT
  else
    result=NOTE_SEND_FAILED
  fi
fi
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' "$result"
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn announcing_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&executable, ANNOUNCING_FIXTURE_FIXTURE);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    (temp, home, workspace)
}

/// Requests, prepares and commits a handoff straight through the store: the
/// takeover choreography does not go through the CLI, and no CLI command does
/// any of it.
fn committed_takeover(
    home: &Path,
    owner_str: &str,
    from_session: &str,
    to_session: &str,
    idempotency_key: &str,
) {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let owner_id = brgr_protocol::OwnerId::new(owner_str.to_owned()).unwrap();
    let takeover = store
        .request_takeover(&owner_id, from_session, to_session, idempotency_key)
        .unwrap();
    let takeover = store
        .prepare_takeover(
            takeover.takeover_id,
            &brgr_store::HandoffManifest {
                goal: "Keep the work moving".to_owned(),
                origin_session: from_session.to_owned(),
                tasks: Vec::new(),
                pending_result_ids: Vec::new(),
                pending_message_ids: Vec::new(),
                decision_notes: String::new(),
            },
        )
        .unwrap();
    assert_eq!(takeover.state, brgr_store::TakeoverState::Prepared);
    let takeover = store.commit_takeover(takeover.takeover_id).unwrap();
    assert_eq!(takeover.state, brgr_store::TakeoverState::Committed);
}

fn takeover_state(
    home: &Path,
    owner_str: &str,
    idempotency_key: &str,
) -> brgr_store::TakeoverState {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let owner_id = brgr_protocol::OwnerId::new(owner_str.to_owned()).unwrap();
    let takeovers = store.owner_takeovers(&owner_id).unwrap();
    let found = takeovers
        .iter()
        .find(|takeover| takeover.idempotency_key == idempotency_key)
        .unwrap_or_else(|| panic!("no handoff {idempotency_key} for {owner_str}: {takeovers:?}"));
    found.state
}

fn retire_resumed_takeovers(home: &Path, owner_str: &str) {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let owner_id = brgr_protocol::OwnerId::new(owner_str.to_owned()).unwrap();
    for takeover in store.owner_takeovers(&owner_id).unwrap() {
        if takeover.state == brgr_store::TakeoverState::Resumed {
            store
                .retire_takeover(takeover.takeover_id, brgr_store::RetireOutcome::Completed)
                .unwrap();
        }
    }
}

fn debate_home(owner: &str) -> (TempDir, PathBuf, Vec<String>) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let mut store = brgr_store::Store::open(home.join("store")).unwrap();
    let owner_id = brgr_protocol::OwnerId::new(owner.to_owned()).unwrap();
    store.bind_owner(&owner_id, "fixture-session", 1).unwrap();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let task_id = brgr_protocol::TaskId::new();
        let task: brgr_protocol::TaskSpec = serde_json::from_value(json!({
            "schema": "brgr/v1",
            "task_id": task_id,
            "revision": 1,
            "create_request_id": format!("debate-{task_id}"),
            "owner_id": owner,
            "objective": "Debate contract fixture",
            "workspace": temp.path().to_string_lossy(),
            "route": {"harness_id": "local.fixture"},
            "required_capabilities": [],
            "artifact_contract": {"media_type": "text/plain", "max_bytes": 4096},
            "acceptance_criteria": ["remain active for debate"],
            "budget": {"deadline_seconds": 60, "max_attempts": 1}
        }))
        .unwrap();
        store
            .record_task(&task, &format!("digest-{task_id}"))
            .unwrap();
        let attempt = brgr_protocol::AttemptId::new();
        store.claim_attempt(task_id, 1, attempt).unwrap();
        store
            .compare_and_set_attempt_state(
                attempt,
                brgr_protocol::AttemptState::Queued,
                brgr_protocol::AttemptState::Starting,
            )
            .unwrap();
        store
            .compare_and_set_attempt_state(
                attempt,
                brgr_protocol::AttemptState::Starting,
                brgr_protocol::AttemptState::Running,
            )
            .unwrap();
        tasks.push(task_id.to_string());
    }
    (temp, home, tasks)
}

fn debate_takeover(home: &Path, owner: &str, key: &str) {
    committed_takeover(home, owner, "fixture-session", "session-b", key);
}

/// Starts `args` as worker `task`'s blocking command.
fn debate_waiter(home: &Path, owner: &str, task: &str, args: &[&str]) -> std::process::Child {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = store
        .latest_message_attempt(task.parse().unwrap())
        .unwrap()
        .to_string();
    brgr_command()
        .arg("--home")
        .arg(home)
        .arg("--json")
        .args(args)
        .env("BRGR_PARENT_TASK_ID", task)
        .env("BRGR_PARENT_ATTEMPT_ID", attempt)
        .env("BRGR_OWNER_ID", owner)
        .env("BRGR_SESSION_ID", "fixture-session")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn send_peer_question(home: &Path, group: &str, from: &str, to: &str) {
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = store.latest_message_attempt(from.parse().unwrap()).unwrap();
    store
        .post_peer_message_from(
            &brgr_store::PeerDraft {
                id: "323e4567-e89b-12d3-a456-426614174000".to_owned(),
                group: group.to_owned(),
                from: from.parse().unwrap(),
                to: to.parse().unwrap(),
                kind: brgr_store::MessageKind::Question,
                body: "Need your view".to_owned(),
                reply_to: None,
            },
            attempt,
        )
        .unwrap();
}

fn assert_returns_for_debate(mut child: std::process::Child) {
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "the wait did not return for a debate message"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("debate_message_pending"),
        "{output:?}"
    );
}

/// The session a Codex worker's daemon inherited says nothing about the worker,
/// so it only conflicts with the thread outside a worker's directory.
#[test]
fn a_stale_daemon_session_does_not_stop_a_worker() {
    let owner = "codex:debate-session";
    let (_temp, home, tasks) = debate_home(owner);
    let worktree = home.join("worktrees/repo/abc");
    fs::create_dir_all(worktree.join(format!(".brgr/tasks/{}-r1", tasks[0]))).unwrap();
    let stale_session_in = |directory: &Path| {
        brgr_command()
            .current_dir(directory)
            .arg("--home")
            .arg(&home)
            .args(["status", &tasks[0]])
            .env("CODEX_THREAD_ID", "thread")
            .env("BRGR_SESSION_ID", "another")
            .output()
            .unwrap()
    };
    let stderr =
        |output: std::process::Output| String::from_utf8_lossy(&output.stderr).into_owned();
    let error = "CODEX_THREAD_ID and BRGR_SESSION_ID disagree";
    assert!(!stderr(stale_session_in(&worktree)).contains(error));
    assert!(stderr(stale_session_in(&home)).contains(error));
}

/// A Codex worker's commands run in a daemon with someone else's environment, so
/// only its directory says who it is.
#[test]
fn a_worker_with_a_stale_environment_is_known_by_its_task_directory() {
    let owner = "codex:debate-directory";
    let (_temp, home, tasks) = debate_home(owner);
    let group = json_output(&run(
        &home,
        &["debate", "start", &tasks[0], &tasks[1]],
        &[("BRGR_OWNER_ID", owner)],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let worktree = home.join("worktrees/repo/abc");
    fs::create_dir_all(worktree.join(format!(".brgr/tasks/{}-r1", tasks[0]))).unwrap();
    let output = brgr_command()
        .current_dir(&worktree)
        .arg("--home")
        .arg(&home)
        .arg("--json")
        .args([
            "debate",
            "send",
            &group,
            "--to",
            &tasks[1],
            "--kind",
            "question",
            "--body",
            "Which is larger?",
        ])
        // The other member's identity, as a daemon that member started would hold.
        .env("BRGR_PARENT_TASK_ID", &tasks[1])
        .env(
            "BRGR_PARENT_ATTEMPT_ID",
            "00000000-0000-4000-8000-000000000000",
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let status = json_output(&run(
        &home,
        &["debate", "status", &group],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let member = status["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["task"] == tasks[1].as_str())
        .unwrap();
    assert_eq!(member["unread_count"], 1, "{status}");
    let age = member["oldest_unread_seconds"].as_i64().unwrap();
    assert!((0..60).contains(&age), "a new message's age: {age}");
}

#[test]
fn a_debate_wait_started_before_the_debate_returns_with_the_group() {
    let owner = "codex:debate-joined";
    let (_temp, home, tasks) = debate_home(owner);
    let mut child = debate_waiter(
        &home,
        owner,
        &tasks[0],
        &["debate", "wait", "--timeout-seconds", "60"],
    );
    // Its marker says the wait has begun; a debate older than the wait is not news.
    wait_until(|| {
        fs::read_dir(home.join("runs/debate-waits"))
            .is_ok_and(|mut entries| entries.next().is_some())
    });
    thread::sleep(Duration::from_millis(300));
    let group = json_output(&run(
        &home,
        &["debate", "start", &tasks[0], &tasks[1]],
        &[("BRGR_OWNER_ID", owner)],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "the wait did not return"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["state"], "debate_joined");
    assert_eq!(receipt["group"], group.as_str());
}

#[test]
fn a_wait_started_before_the_debate_still_returns_for_its_message() {
    let owner = "codex:debate-late-start";
    let (_temp, home, tasks) = debate_home(owner);
    let child = debate_waiter(
        &home,
        owner,
        &tasks[0],
        &[
            "message",
            "wait",
            &tasks[1],
            "--for",
            "owner",
            "--timeout-seconds",
            "60",
        ],
    );
    thread::sleep(Duration::from_millis(500));
    let group = json_output(&run(
        &home,
        &["debate", "start", &tasks[0], &tasks[1]],
        &[("BRGR_OWNER_ID", owner)],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send_peer_question(&home, &group, &tasks[1], &tasks[0]);
    assert_returns_for_debate(child);
}

#[test]
fn a_debate_members_brgr_wait_returns_for_a_debate_message() {
    let owner = "codex:debate-brgr-wait";
    let (_temp, home, tasks) = debate_home(owner);
    let group = json_output(&run(
        &home,
        &["debate", "start", &tasks[0], &tasks[1]],
        &[("BRGR_OWNER_ID", owner)],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The fixture's attempts have no live supervisor, so `wait` would settle them
    // on its first reconcile: the message is already there when it starts.
    send_peer_question(&home, &group, &tasks[1], &tasks[0]);
    assert_returns_for_debate(debate_waiter(
        &home,
        owner,
        &tasks[0],
        &["wait", &tasks[1], "--timeout-seconds", "60"],
    ));
}

#[test]
fn a_debate_members_watch_returns_for_a_debate_message() {
    let owner = "codex:debate-watch";
    let (_temp, home, tasks) = debate_home(owner);
    let group = json_output(&run(
        &home,
        &["debate", "start", &tasks[0], &tasks[1]],
        &[("BRGR_OWNER_ID", owner)],
    ))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send_peer_question(&home, &group, &tasks[1], &tasks[0]);
    assert_returns_for_debate(debate_waiter(
        &home,
        owner,
        &tasks[0],
        &["watch", &tasks[1]],
    ));
}

#[test]
fn debate_member_message_wait_returns_early_for_peer_message() {
    let owner = "codex:debate-early-wait";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "123e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap().to_owned();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let mut waiter = brgr_command();
    waiter
        .arg("--home")
        .arg(&home)
        .arg("--json")
        .args([
            "message",
            "wait",
            &tasks[1],
            "--for",
            "owner",
            "--timeout-seconds",
            "30",
        ])
        .env("BRGR_PARENT_TASK_ID", &tasks[0])
        .env(
            "BRGR_PARENT_ATTEMPT_ID",
            store
                .latest_message_attempt(tasks[0].parse().unwrap())
                .unwrap()
                .to_string(),
        )
        .env("BRGR_OWNER_ID", owner)
        .env("BRGR_SESSION_ID", "fixture-session")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = waiter.spawn().unwrap();
    thread::sleep(Duration::from_millis(300));
    let from_attempt = store
        .latest_message_attempt(tasks[1].parse().unwrap())
        .unwrap();
    store
        .post_peer_message_from(
            &brgr_store::PeerDraft {
                id: "223e4567-e89b-12d3-a456-426614174000".to_owned(),
                group,
                from: tasks[1].parse().unwrap(),
                to: tasks[0].parse().unwrap(),
                kind: brgr_store::MessageKind::Question,
                body: "Need your view".to_owned(),
                reply_to: None,
            },
            from_attempt,
        )
        .unwrap();
    let began = Instant::now();
    let output = loop {
        if child.try_wait().unwrap().is_some() {
            break child.wait_with_output().unwrap();
        }
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "message wait did not return early"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("debate_message_pending"));
}

#[test]
fn non_debate_message_wait_keeps_waiting_until_timeout() {
    let owner = "codex:non-debate-wait";
    let (_temp, home, tasks) = debate_home(owner);
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = store
        .latest_message_attempt(tasks[0].parse().unwrap())
        .unwrap()
        .to_string();
    let began = Instant::now();
    let output = run(
        &home,
        &[
            "message",
            "wait",
            &tasks[0],
            "--for",
            "owner",
            "--timeout-seconds",
            "2",
        ],
        &[
            ("BRGR_PARENT_TASK_ID", &tasks[0]),
            ("BRGR_PARENT_ATTEMPT_ID", &attempt),
            ("BRGR_OWNER_ID", owner),
        ],
    );
    assert!(!output.status.success());
    assert!(began.elapsed() >= Duration::from_millis(1700));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("debate_message_pending"));
}

#[test]
fn debate_status_shows_unread_age_and_finished_member() {
    let owner = "codex:debate-status-members";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "323e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let from: brgr_protocol::TaskId = tasks[0].parse().unwrap();
    let to: brgr_protocol::TaskId = tasks[1].parse().unwrap();
    let target_attempt = store.latest_message_attempt(to).unwrap();
    store
        .post_peer_message_from(
            &brgr_store::PeerDraft {
                id: "423e4567-e89b-12d3-a456-426614174000".into(),
                group: group.into(),
                from,
                to,
                kind: brgr_store::MessageKind::Question,
                body: "Unread".into(),
                reply_to: None,
            },
            store.latest_message_attempt(from).unwrap(),
        )
        .unwrap();
    rusqlite::Connection::open(home.join("store/brgr.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE attempts SET state='terminal' WHERE attempt_id=?1",
            [target_attempt.to_string()],
        )
        .unwrap();
    let status = json_output(&run(
        &home,
        &["debate", "status", group],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let member = status["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["task"] == tasks[1])
        .unwrap();
    assert_eq!(member["unread_count"], 1);
    assert!(member["oldest_unread_seconds"].as_i64().unwrap() >= 0);
    assert_eq!(member["worker_state"], "finished");
}

#[test]
fn debate_send_refuses_a_finished_member() {
    let owner = "codex:debate-finished-send";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "523e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let target: brgr_protocol::TaskId = tasks[1].parse().unwrap();
    let attempt = store.latest_message_attempt(target).unwrap();
    rusqlite::Connection::open(home.join("store/brgr.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE attempts SET state='terminal' WHERE attempt_id=?1",
            [attempt.to_string()],
        )
        .unwrap();
    let output = run(
        &home,
        &[
            "debate", "send", group, "--from", &tasks[0], "--to", &tasks[1], "--body", "Hello",
        ],
        &[("BRGR_OWNER_ID", owner)],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("finished or is gone"));
}

#[test]
fn debate_subcommands_have_help_descriptions() {
    let (_temp, home, _tasks) = debate_home("codex:debate-help");
    for command in ["start", "status", "stop", "send", "list", "wait", "ack"] {
        let output = run(&home, &["debate", command, "--help"], &[]);
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.lines()
                .next()
                .is_some_and(|line| !line.trim().is_empty()),
            "{command} missing one-line help: {text}"
        );
    }
    let root = run(&home, &["debate", "--help"], &[]);
    assert!(
        String::from_utf8_lossy(&root.stdout)
            .lines()
            .next()
            .is_some_and(|line| !line.trim().is_empty())
    );
}

fn owner_env<'a>(owner: &'a str, session: &'a str) -> [(&'a str, &'a str); 2] {
    [("BRGR_OWNER_ID", owner), ("BRGR_SESSION_ID", session)]
}

#[test]
fn debate_start_refuses_stale_owner_without_group_or_invitations() {
    let owner = "codex:debate-start-takeover";
    let (_temp, home, tasks) = debate_home(owner);
    debate_takeover(&home, owner, "debate-start-handoff");
    let stale = owner_env(owner, "fixture-session");
    let started = run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "123e4567-e89b-12d3-a456-426614174000",
        ],
        &stale,
    );
    assert!(!started.status.success(), "stale start succeeded");
    assert!(String::from_utf8_lossy(&started.stderr).contains("bound to another session"));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(
        store
            .debate("123e4567-e89b-12d3-a456-426614174000")
            .is_err()
    );
    for task in &tasks {
        let task_id = task.parse().unwrap();
        let attempt = store.latest_message_attempt(task_id).unwrap();
        assert!(
            store
                .task_messages(
                    task_id,
                    attempt,
                    brgr_store::MessageDirection::OwnerToWorker,
                    true
                )
                .unwrap()
                .is_empty()
        );
    }
    let head = owner_env(owner, "session-b");
    assert!(
        run(
            &home,
            &[
                "debate",
                "start",
                &tasks[0],
                &tasks[1],
                "--request-id",
                "123e4567-e89b-12d3-a456-426614174000"
            ],
            &head,
        )
        .status
        .success()
    );
}

#[test]
fn debate_stop_refuses_stale_owner_and_keeps_group_active() {
    let owner = "codex:debate-stop-takeover";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "223e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap().to_owned();
    debate_takeover(&home, owner, "debate-stop-handoff");
    let stale = run(
        &home,
        &["debate", "stop", &group],
        &owner_env(owner, "fixture-session"),
    );
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("bound to another session"));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(store.debate(&group).unwrap().active);
    assert!(
        run(
            &home,
            &["debate", "stop", &group],
            &owner_env(owner, "session-b")
        )
        .status
        .success()
    );
    assert!(!store.debate(&group).unwrap().active);
}

#[test]
fn debate_owner_send_refuses_stale_owner_without_peer_message() {
    let owner = "codex:debate-send-takeover";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "323e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap().to_owned();
    debate_takeover(&home, owner, "debate-send-handoff");
    let args = [
        "debate",
        "send",
        &group,
        "--from",
        &tasks[0],
        "--to",
        &tasks[1],
        "--kind",
        "note",
        "--body",
        "owner note",
        "--request-id",
        "523e4567-e89b-12d3-a456-426614174000",
    ];
    let stale = run(&home, &args, &owner_env(owner, "fixture-session"));
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("bound to another session"));
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(store.debate_history(&group).unwrap().is_empty());
    assert!(
        run(&home, &args, &owner_env(owner, "session-b"))
            .status
            .success()
    );
    assert_eq!(store.debate_history(&group).unwrap().len(), 1);
}

#[test]
fn debate_worker_send_remains_available_after_owner_takeover() {
    let owner = "codex:debate-worker-takeover";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "423e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap().to_owned();
    debate_takeover(&home, owner, "debate-worker-handoff");
    let worker_task = tasks[0].clone();
    let worker_attempt = brgr_store::Store::open(home.join("store"))
        .unwrap()
        .latest_message_attempt(worker_task.parse().unwrap())
        .unwrap()
        .to_string();
    let output = run(
        &home,
        &[
            "debate",
            "send",
            &group,
            "--to",
            &tasks[1],
            "--kind",
            "note",
            "--body",
            "worker note",
            "--request-id",
            "623e4567-e89b-12d3-a456-426614174000",
        ],
        &[
            ("BRGR_PARENT_TASK_ID", &worker_task),
            ("BRGR_PARENT_ATTEMPT_ID", &worker_attempt),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert_eq!(store.debate_history(&group).unwrap().len(), 1);
}

#[test]
fn debate_ack_is_bound_for_owner_but_remains_available_to_worker() {
    let owner = "codex:debate-ack-takeover";
    let (_temp, home, tasks) = debate_home(owner);
    let started = json_output(&run(
        &home,
        &[
            "debate",
            "start",
            &tasks[0],
            &tasks[1],
            "--request-id",
            "723e4567-e89b-12d3-a456-426614174000",
        ],
        &[("BRGR_OWNER_ID", owner)],
    ));
    let group = started["id"].as_str().unwrap().to_owned();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let a: brgr_protocol::TaskId = tasks[0].parse().unwrap();
    let b: brgr_protocol::TaskId = tasks[1].parse().unwrap();
    let attempt_a = store.latest_message_attempt(a).unwrap();
    let attempt_b = store.latest_message_attempt(b).unwrap();
    let question = brgr_store::PeerDraft {
        id: "823e4567-e89b-12d3-a456-426614174000".to_owned(),
        group: group.clone(),
        from: a,
        to: b,
        kind: brgr_store::MessageKind::Question,
        body: "Please acknowledge".to_owned(),
        reply_to: None,
    };
    store.post_peer_message_from(&question, attempt_a).unwrap();
    debate_takeover(&home, owner, "debate-ack-handoff");

    // Existing `authorize` already rejects an old owner before the ack write,
    // so this CLI leg is expected to be green even before store-level binding.
    let stale = run(
        &home,
        &["debate", "ack", &question.id, &tasks[1]],
        &owner_env(owner, "fixture-session"),
    );
    assert!(!stale.status.success());
    assert!(!String::from_utf8_lossy(&stale.stderr).is_empty());
    assert!(!store.debate_history(&group).unwrap()[0].acknowledged);

    let fresh = run(
        &home,
        &["debate", "ack", &question.id, &tasks[1]],
        &[
            ("BRGR_OWNER_ID", owner),
            ("BRGR_SESSION_ID", "session-b"),
            ("CODEX_THREAD_ID", "session-b"),
        ],
    );
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    assert!(store.debate_history(&group).unwrap()[0].acknowledged);

    let second = brgr_store::PeerDraft {
        id: "923e4567-e89b-12d3-a456-426614174000".to_owned(),
        body: "Worker ack remains available".to_owned(),
        ..question
    };
    store.post_peer_message_from(&second, attempt_a).unwrap();
    let worker = run(
        &home,
        &["debate", "ack", &second.id],
        &[
            ("BRGR_PARENT_TASK_ID", &tasks[1]),
            ("BRGR_PARENT_ATTEMPT_ID", &attempt_b.to_string()),
        ],
    );
    assert!(
        worker.status.success(),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    assert!(store.debate_history(&group).unwrap()[1].acknowledged);
}

fn manual(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = brgr_command();
    command.arg("--home").arg(home).arg("--json").args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

/// A task whose worker asked a question, whose owner just handed the head to
/// `session-b`, with the question still waiting for an owner answer.
fn handing_over_home() -> (TempDir, PathBuf, String, String) {
    let (temp, home, workspace) = announcing_home();
    let owner = [("BRGR_OWNER_ID", "codex:handoff-message")];
    let launched = json_output(&run(
        &home,
        &[
            "run",
            "ANNOUNCE",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &owner,
    ));
    let task = launched["task_id"].as_str().unwrap();
    let question = json_output(&run(
        &home,
        &[
            "message",
            "wait",
            task,
            "--for",
            "owner",
            "--timeout-seconds",
            "60",
        ],
        &owner,
    ));
    let question_id = question["message_id"].as_str().unwrap().to_owned();
    committed_takeover(
        &home,
        "codex:handoff-message",
        "fixture-session",
        "session-b",
        "handoff-message-1",
    );
    let task = task.to_owned();
    (temp, home, task, question_id)
}

/// A stale session's send and ack change nothing.
#[test]
fn a_stale_session_writes_no_owner_message_after_a_handoff() {
    let (_temp, home, task, question_id) = handing_over_home();
    let stale = [
        ("BRGR_OWNER_ID", "codex:handoff-message"),
        ("BRGR_SESSION_ID", "fixture-session"),
    ];
    let sent = run(
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
            "stale answer",
        ],
        &stale,
    );
    assert!(
        !sent.status.success(),
        "a session that handed its head away sent an owner message: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let acked = run(
        &home,
        &["message", "ack", &task, &question_id, "--for", "owner"],
        &stale,
    );
    assert!(
        !acked.status.success(),
        "a session that handed its head away acknowledged a worker message: {}",
        String::from_utf8_lossy(&acked.stderr)
    );
    // Nothing was written: the question is still there, unacknowledged, and no
    // stale reply reached the worker.
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let task_id = task.parse().unwrap();
    let attempt = store.latest_message_attempt(task_id).unwrap();
    let inbox = store
        .task_messages(
            task_id,
            attempt,
            brgr_store::MessageDirection::WorkerToOwner,
            true,
        )
        .unwrap();
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    assert!(!inbox[0].acknowledged, "{inbox:?}");
    let replies = store
        .task_messages(
            task_id,
            attempt,
            brgr_store::MessageDirection::OwnerToWorker,
            true,
        )
        .unwrap();
    assert!(
        replies.iter().all(|message| message.body != "stale answer"),
        "{replies:?}"
    );
}

/// The worker side stays unguarded: after the handoff, the incoming session
/// answers the question and the worker still reports on its own.
#[test]
fn the_worker_still_reports_to_the_head_after_a_handoff() {
    let (_temp, home, task, question_id) = handing_over_home();
    let head = [
        ("BRGR_OWNER_ID", "codex:handoff-message"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
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
            "all word",
        ],
        &head,
    ));
    json_output(&run(
        &home,
        &["message", "ack", &task, &question_id, "--for", "owner"],
        &head,
    ));

    json_output(&run(
        &home,
        &["wait", &task, "--timeout-seconds", "60"],
        &head,
    ));
    let result = json_output(&run(&home, &["result", &task], &head));
    assert_eq!(
        result["artifacts"][0]["text"], "POST_TAKEOVER_NOTE_SENT",
        "{result}"
    );
    // The worker's post-takeover note reached the head (worker reporting stays
    // unguarded) and can be answered from there.
    let note = json_output(&run(
        &home,
        &[
            "message",
            "wait",
            &task,
            "--for",
            "owner",
            "--timeout-seconds",
            "60",
        ],
        &head,
    ));
    assert_eq!(note["body"], "post-takeover note", "{note}");
}

/// The same refusal for a cancellation: the outgoing session may not stop work
/// it handed away; the incoming session cancels and the cancellation lands.
#[test]
fn an_old_session_stops_nothing_after_a_handoff() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:handoff-cancel")];
    let launched = json_output(&run(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    let task = launched["task_id"].as_str().unwrap();
    // The worker is waiting on its owner, so the task is still running.

    committed_takeover(
        &home,
        "codex:handoff-cancel",
        "fixture-session",
        "session-b",
        "handoff-cancel-1",
    );
    let stale = [
        ("BRGR_OWNER_ID", "codex:handoff-cancel"),
        ("BRGR_SESSION_ID", "fixture-session"),
    ];
    let cancelled = run(&home, &["cancel", task], &stale);
    assert!(
        !cancelled.status.success(),
        "a session that handed its head away cancelled the work: {}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    // The head cancels it instead, and the run stops on its own.
    let head = [
        ("BRGR_OWNER_ID", "codex:handoff-cancel"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
    // Nothing changed hands: the task still runs (it never produced a result
    // the head could read).
    assert!(
        !run_with_error(&home, task, &head).ok,
        "the cancellation the stale session refused still happened"
    );
    json_output(&run(&home, &["cancel", task], &head));
    // `result` only reads; `status` drives reconcile_pending, which settles a
    // run whose detached supervisor died unclaimed or never spawned. The
    // window is generous because the surviving path — supervisor spawn,
    // claim, process-group kill, seal — is real work that a loaded runner can
    // stretch past twenty seconds.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        assert!(
            Instant::now() <= deadline,
            "the task did not stop after the head cancelled it: {}",
            run_state_debug(&home, task)
        );
        let seen = json_output(&run(&home, &["status", task], &head));
        if seen["state"].as_str() == Some("terminal") {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let result = run_with_error(&home, task, &head);
    assert!(result.ok, "{result:?}");
    assert_eq!(result.outcome, "cancelled", "{result:?}");
}

/// A foreign owner cannot cancel; `--force` stops the worker without taking
/// the head or deciding the sealed result.
#[test]
fn a_foreign_owner_force_cancels_a_running_worker() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:force-owner")];
    let launched = json_output(&run(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    let task = launched["task_id"].as_str().unwrap();
    let foreign = [
        ("BRGR_OWNER_ID", "codex:someone-else"),
        ("BRGR_SESSION_ID", "other-session"),
    ];
    let refused = run(&home, &["cancel", task], &foreign);
    assert!(
        !refused.status.success(),
        "a different owner cancelled without --force: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("BRGR_OWNER_ID differs"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let forced = json_output(&run(&home, &["cancel", task, "--force"], &foreign));
    assert_eq!(forced["state"], "cancel_requested", "{forced}");
    // `result` only reads; `status` drives reconcile_pending, which settles a
    // run whose detached supervisor died unclaimed or never spawned. The
    // window is generous because the surviving path — supervisor spawn,
    // claim, process-group kill, seal — is real work that a loaded runner can
    // stretch past twenty seconds.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        assert!(
            Instant::now() <= deadline,
            "the task did not stop after --force: {}",
            run_state_debug(&home, task)
        );
        let seen = json_output(&run(&home, &["status", task], &owner));
        if seen["state"].as_str() == Some("terminal") {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let result = run_with_error(&home, task, &owner);
    assert!(result.ok, "{result:?}");
    assert_eq!(result.outcome, "cancelled", "{result:?}");
}

/// A task's supervisor and dispatcher trail when a settle or delivery deadline
/// fires: its launch, receipts, notify log and lock, and `supervisor.log`.
fn run_state_debug(home: &Path, task: &str) -> String {
    let mut out = String::new();
    for path in [
        home.join("launches").join(format!("{task}.json")),
        home.join("runs").join(format!("{task}.supervisor.json")),
        home.join("runs").join(format!("{task}.pid")),
        home.join("runs").join(format!("{task}.notify.log")),
        home.join("runs").join(format!("{task}.notify.lock")),
        home.join("runs").join("supervisor.log"),
    ] {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = writeln!(out, "{}: {}", path.display(), clipped(&text, 512));
    }
    out
}

/// After a handoff the outgoing session still cannot cancel, but `--force`
/// stops the worker for whoever can reach the control home.
#[test]
fn a_stale_session_force_cancels_after_a_handoff() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:handoff-force-cancel")];
    let launched = json_output(&run(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    let task = launched["task_id"].as_str().unwrap();
    committed_takeover(
        &home,
        "codex:handoff-force-cancel",
        "fixture-session",
        "session-b",
        "handoff-force-cancel-1",
    );
    let stale = [
        ("BRGR_OWNER_ID", "codex:handoff-force-cancel"),
        ("BRGR_SESSION_ID", "fixture-session"),
    ];
    let refused = run(&home, &["cancel", task], &stale);
    assert!(
        !refused.status.success(),
        "a session that handed its head away cancelled without --force: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let forced = json_output(&run(&home, &["cancel", task, "--force"], &stale));
    assert_eq!(forced["state"], "cancel_requested", "{forced}");
    let head = [
        ("BRGR_OWNER_ID", "codex:handoff-force-cancel"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
    // `result` only reads; `status` drives reconcile_pending, which settles a
    // run whose detached supervisor died unclaimed or never spawned. The
    // window is generous because the surviving path — supervisor spawn,
    // claim, process-group kill, seal — is real work that a loaded runner can
    // stretch past twenty seconds.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        assert!(
            Instant::now() <= deadline,
            "the task did not stop after the stale session force-cancelled it: {}",
            run_state_debug(&home, task)
        );
        let seen = json_output(&run(&home, &["status", task], &head));
        if seen["state"].as_str() == Some("terminal") {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let result = run_with_error(&home, task, &head);
    assert!(result.ok, "{result:?}");
    assert_eq!(result.outcome, "cancelled", "{result:?}");
}

/// `--force` on an already sealed candidate closes nothing about the result:
/// reject and revise still work, and a cancellation intent is not recorded.
#[test]
fn force_cancel_of_a_candidate_does_not_block_revise() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:force-terminal")];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(first["outcome"], "candidate");
    let task = first["task_id"].as_str().unwrap();
    let foreign = [("BRGR_OWNER_ID", "codex:someone-else")];
    let forced = json_output(&run(&home, &["cancel", task, "--force"], &foreign));
    assert_eq!(forced["state"], "force_stopped", "{forced}");
    let result = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(result["result"]["outcome"], "candidate", "{result}");
    json_output(&run(
        &home,
        &["reject", task, "--reason", "revise after force-stop"],
        &owner,
    ));
    let revised = json_output(&run(
        &home,
        &["revise", task, "BRGR_FIXTURE_OK", "--foreground"],
        &owner,
    ));
    assert_eq!(revised["outcome"], "candidate");
    assert_eq!(revised["revision"], 2);
}

/// A running task exists; only its result is missing. Calling it missing sent
/// callers looking for a task that `brgr status` could still see.
#[test]
fn a_task_without_a_result_yet_is_not_reported_as_missing() {
    let (_temp, home, workspace) = asking_home();
    let owner = [("BRGR_OWNER_ID", "codex:no-result-yet")];
    let launched = json_output(&run(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &owner,
    ));
    // The worker is waiting on its owner, so no result can be sealed yet.
    let task = launched["task_id"].as_str().unwrap();
    for args in [["result", task], ["accept", task]] {
        let output = run(&home, &args, &owner);
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} succeeded");
        assert!(text.contains("no sealed result yet"), "{args:?}: {text}");
        assert!(!text.contains("does not exist"), "{args:?}: {text}");
    }

    let unknown = "00000000-0000-4000-8000-000000000000";
    let output = run(&home, &["result", unknown], &owner);
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(text.contains("does not exist"), "{text}");

    json_output(&run(&home, &["cancel", task], &owner));
}

#[derive(Debug)]
struct Poll {
    ok: bool,
    outcome: String,
}

fn run_with_error(home: &Path, task: &str, envs: &[(&str, &str)]) -> Poll {
    let output = run(home, &["result", task], envs);
    let text = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        return Poll {
            ok: true,
            outcome: value["result"]["outcome"].as_str().unwrap().to_owned(),
        };
    }
    assert!(text.contains("no sealed result yet"), "{text}");
    Poll {
        ok: false,
        outcome: String::new(),
    }
}

/// Starting a task as the owner is also an owner-side write: after a handoff
/// commits, the incoming session's first admitted revision is what proves it
/// took the work over, so admission goes through the guarded store write.
#[test]
fn the_incoming_session_admits_the_next_task_through_the_guard() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:handoff-admission")];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    assert_eq!(first["outcome"], "candidate", "{first}");
    committed_takeover(
        &home,
        "codex:handoff-admission",
        "fixture-session",
        "session-b",
        "handoff-admission-1",
    );

    let stale = [
        ("BRGR_OWNER_ID", "codex:handoff-admission"),
        ("BRGR_SESSION_ID", "fixture-session"),
    ];
    let refused = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &stale,
    );
    assert!(
        !refused.status.success(),
        "a session that handed its head away started another revision: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // The head starts the next task, and the handoff is resumed by that write.
    let head = [
        ("BRGR_OWNER_ID", "codex:handoff-admission"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
    let second = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &head,
    ));
    assert_eq!(second["outcome"], "candidate", "{second}");
    assert_eq!(
        takeover_state(&home, "codex:handoff-admission", "handoff-admission-1"),
        brgr_store::TakeoverState::Resumed,
        "an admitted task did not resume the handoff"
    );
}

/// Revising replaces the head's result with a new revision, so the new revision
/// must be recorded inside the transaction that still proves the session is the
/// head. The old session revises nothing, and the head's revision resumes.
#[test]
fn the_incoming_session_records_the_next_revision_through_the_guard() {
    let (_temp, home, workspace) = fixture_home();
    let owner = [("BRGR_OWNER_ID", "codex:handoff-revise")];
    let created = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &owner,
    ));
    let task = created["task_id"].as_str().unwrap();

    committed_takeover(
        &home,
        "codex:handoff-revise",
        "fixture-session",
        "session-b",
        "handoff-revise-1",
    );
    // The head rejects the candidate (that resumes handoff 1), closes it, and
    // requests a second handoff so the revise stands on its own.
    let head = [
        ("BRGR_OWNER_ID", "codex:handoff-revise"),
        ("BRGR_SESSION_ID", "session-b"),
    ];
    json_output(&run(&home, &["reject", task, "--reason", "handoff"], &head));
    let stale = [
        ("BRGR_OWNER_ID", "codex:handoff-revise"),
        ("BRGR_SESSION_ID", "fixture-session"),
    ];
    let refused = manual(&home, &["revise", task, "ex again", "--foreground"], &stale);
    let refusal = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && refusal.contains("is bound to another session"),
        "the outgoing session must be refused by the binding guard: {refusal}"
    );
    let spec = store_task(&home, task);
    assert_eq!(spec.revision, 1, "the refused revision was recorded");
    retire_resumed_takeovers(&home, "codex:handoff-revise");
    committed_takeover(
        &home,
        "codex:handoff-revise",
        "session-b",
        "session-c",
        "handoff-revise-2",
    );

    let newer = [
        ("BRGR_OWNER_ID", "codex:handoff-revise"),
        ("BRGR_SESSION_ID", "session-c"),
    ];
    let revised = manual(
        &home,
        &["revise", task, "ex a third time", "--foreground"],
        &newer,
    );
    assert!(
        revised.status.success(),
        "{}",
        String::from_utf8_lossy(&revised.stderr)
    );
    let revised: Value = serde_json::from_slice(&revised.stdout).unwrap();
    assert_eq!(revised["revision"], 2, "{revised}");
    assert_eq!(
        takeover_state(&home, "codex:handoff-revise", "handoff-revise-2"),
        brgr_store::TakeoverState::Resumed,
        "a recorded revision did not resume the handoff"
    );
}

#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "The operating system caps a single argument at 128 KiB; this test exercises positional input (#229)"
)]
fn a_256_kib_objective_is_admitted_by_brgr_run() {
    let (_temp, home, workspace) = fixture_home();
    let objective = "a".repeat(brgr_protocol::MAX_OBJECTIVE_BYTES);
    let launched = json_output(&run(
        &home,
        &[
            "run",
            &objective,
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:objective-limit")],
    ));
    let task = launched["task_id"].as_str().unwrap();
    let stored = store_task(&home, task);
    assert_eq!(stored.objective.len(), brgr_protocol::MAX_OBJECTIVE_BYTES);
    let _ = run(
        &home,
        &["cancel", task],
        &[("BRGR_OWNER_ID", "codex:objective-limit")],
    );
}

#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "The operating system caps a single argument at 128 KiB; this test exercises positional input (#229)"
)]
fn a_256_kib_plus_one_objective_is_refused_by_brgr_run_without_creating_a_task() {
    let (_temp, home, workspace) = fixture_home();
    let objective = "a".repeat(brgr_protocol::MAX_OBJECTIVE_BYTES + 1);
    let output = run(
        &home,
        &[
            "run",
            &objective,
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:objective-limit")],
    );
    assert!(
        !output.status.success(),
        "over-long objective was admitted: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&brgr_protocol::MAX_OBJECTIVE_BYTES.to_string()),
        "error must name the limit: {stderr}"
    );
    assert!(!stderr.contains(&objective), "error quoted the objective");
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    assert!(
        store.tasks(100).unwrap().is_empty(),
        "an over-long run created a task"
    );
}

#[test]
fn objective_file_admits_exactly_256_kib() {
    let (temp, home, workspace) = fixture_home();
    let objective_file = temp.path().join("objective.txt");
    fs::write(
        &objective_file,
        "a".repeat(brgr_protocol::MAX_OBJECTIVE_BYTES),
    )
    .unwrap();
    let launched = json_output(&run(
        &home,
        &[
            "run",
            "--objective-file",
            objective_file.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:objective-file")],
    ));
    let task = launched["task_id"].as_str().unwrap();
    assert_eq!(
        store_task(&home, task).objective.len(),
        brgr_protocol::MAX_OBJECTIVE_BYTES
    );
    let _ = run(
        &home,
        &["cancel", task],
        &[("BRGR_OWNER_ID", "codex:objective-file")],
    );
}

#[test]
fn objective_file_over_limit_is_refused_without_task() {
    let (temp, home, workspace) = fixture_home();
    let objective_file = temp.path().join("objective.txt");
    fs::write(
        &objective_file,
        "a".repeat(brgr_protocol::MAX_OBJECTIVE_BYTES + 1),
    )
    .unwrap();
    let output = run(
        &home,
        &[
            "run",
            "--objective-file",
            objective_file.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:objective-file")],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("262144"));
    assert!(
        brgr_store::Store::open(home.join("store"))
            .unwrap()
            .tasks(100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn objective_file_rejects_non_regular_and_missing_inputs_without_task() {
    use std::os::unix::fs::symlink;
    let (temp, home, workspace) = fixture_home();
    let dir = temp.path().join("dir");
    fs::create_dir(&dir).unwrap();
    let target = temp.path().join("target");
    fs::write(&target, "objective").unwrap();
    let link = temp.path().join("link");
    symlink(&target, &link).unwrap();
    let fifo = temp.path().join("fifo");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let missing = temp.path().join("missing");
    for path in [&dir, &fifo, &link, &missing] {
        let output = run(
            &home,
            &[
                "run",
                "--objective-file",
                path.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
            ],
            &[("BRGR_OWNER_ID", "codex:objective-file")],
        );
        assert!(!output.status.success(), "accepted {}", path.display());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("objective"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            brgr_store::Store::open(home.join("store"))
                .unwrap()
                .tasks(100)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn objective_file_requires_exactly_one_objective_form() {
    let (temp, home, _workspace) = fixture_home();
    let file = temp.path().join("objective.txt");
    fs::write(&file, "objective").unwrap();
    for args in [
        vec!["run", "both", "--objective-file", file.to_str().unwrap()],
        vec!["run"],
    ] {
        let output = run(&home, &args, &[("BRGR_OWNER_ID", "codex:objective-file")]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("objective"));
    }
}

#[test]
fn default_criterion_for_a_100_kib_objective_stays_small() {
    let (_temp, home, workspace) = fixture_home();
    let objective = "b".repeat(100 * 1024);
    let launched = json_output(&run(
        &home,
        &[
            "run",
            &objective,
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:criterion-bound")],
    ));
    let task = launched["task_id"].as_str().unwrap();
    let stored = store_task(&home, task);
    assert_eq!(stored.acceptance_criteria.len(), 1);
    assert!(
        stored.acceptance_criteria[0].len() < 64,
        "default criterion copied the objective: {:?}",
        stored.acceptance_criteria[0].len()
    );
    assert!(!stored.acceptance_criteria[0].contains(&objective));
    let _ = run(
        &home,
        &["cancel", task],
        &[("BRGR_OWNER_ID", "codex:criterion-bound")],
    );
}

#[test]
fn an_older_stored_over_limit_task_still_reads_through_status() {
    let (_temp, home, workspace) = fixture_home();
    let created = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:legacy-objective")],
    ));
    let task = created["task_id"].as_str().unwrap();
    let over = "z".repeat(brgr_protocol::MAX_OBJECTIVE_BYTES + 1);
    {
        let spec = store_task(&home, task);
        let mut value = serde_json::to_value(&spec).unwrap();
        value["objective"] = json!(over);
        let conn = rusqlite::Connection::open(home.join("store").join("brgr.sqlite3")).unwrap();
        conn.execute(
            "UPDATE tasks SET spec_json = ?1 WHERE task_id = ?2 AND revision = ?3",
            rusqlite::params![value.to_string(), task, spec.revision],
        )
        .unwrap();
    }
    let listed = json_output(&run(
        &home,
        &["status", task, "--observe"],
        &[("BRGR_OWNER_ID", "codex:legacy-objective")],
    ));
    assert_eq!(
        listed["task"]["objective"].as_str().unwrap().len(),
        over.len()
    );
    assert_eq!(listed["task"]["objective"], over);
}
#[test]
fn as_session_ignores_another_sessions_stale_owner_identity() {
    // A shared Codex daemon keeps the owner and session of the worker that
    // started it. A call that names its own session is that session's call.
    let (_temp, home, workspace) = fixture_home();
    let stale = [
        ("BRGR_SESSION_ID", "other"),
        ("BRGR_OWNER_ID", "worker:other"),
    ];
    let launched = run(
        &home,
        &[
            "--as",
            "mine",
            "run",
            "BRGR_FIXTURE_OK",
            "--harness",
            "local.fixture",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
            "--headless",
        ],
        &stale,
    );
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let task = json_output(&launched)["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // `mine` sees the task as its own; a task recorded under `worker:other`
    // would be invisible to it.
    let listing = run(&home, &["--as", "mine", "status", "--brief"], &[]);
    let text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        text.contains(&task),
        "the task was not recorded for `mine`: {text}"
    );
    let owned = run(&home, &["--as", "mine", "status", &task, "--verbose"], &[]);
    assert!(
        String::from_utf8_lossy(&owned.stdout).contains("codex:mine"),
        "{}",
        String::from_utf8_lossy(&owned.stdout)
    );
}

/// Registers `local.gjc` the way an installation from before its removal has it.
fn register_removed_harness(home: &Path) {
    for directory in ["manifests", "activations"] {
        let directory = home.join("registry").join(directory);
        fs::copy(
            directory.join("local.fixture.json"),
            directory.join("local.gjc.json"),
        )
        .unwrap();
    }
}

#[test]
fn a_run_on_a_registered_gjc_is_refused_without_creating_a_task() {
    let (_temp, home, workspace) = fixture_home();
    register_removed_harness(&home);
    let output = run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--harness",
            "local.gjc",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[("BRGR_OWNER_ID", "codex:removed-harness")],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("support for GJC was removed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = run(
        &home,
        &["status"],
        &[("BRGR_OWNER_ID", "codex:removed-harness")],
    );
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains("local.gjc"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[test]
fn doctor_does_not_report_a_registered_gjc() {
    let (_temp, home, _workspace) = fixture_home();
    register_removed_harness(&home);
    let output = run(&home, &["doctor"], &[]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("local.gjc"), "{text}");
    assert!(text.contains("local.fixture"), "{text}");
}

#[test]
fn a_config_section_for_gjc_keeps_loading() {
    let (_temp, home, _workspace) = fixture_home();
    let config = home.join("config.toml");
    let mut text = fs::read_to_string(&config).unwrap_or_default();
    text.push_str("\n[harnesses.\"local.gjc\"]\nmodel = \"openai-codex/gpt-6-luna\"\nargv = []\n");
    fs::write(&config, text).unwrap();
    json_output(&run(&home, &["config", "show"], &[]));
    json_output(&run(&home, &["config", "check"], &[]));
}

/// The internal commands a dead home's launchers can still fire (issue #350)
/// exit quietly and create nothing; `Paths::new` is never reached.
#[test]
fn internal_command_for_a_missing_home_exits_without_creating_it() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("missing-home");
    let cases: [&[&str]; 4] = [
        &["__notify", "11111111-1111-4111-8111-111111111111"],
        &["__notify-workflow", "a-run-id"],
        &["__session", "11111111-1111-4111-8111-111111111111"],
        &["__supervise", "/nonexistent/launch.json"],
    ];
    for args in cases {
        let output = brgr_command()
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{args:?} wrote to stdout");
        assert!(output.stderr.is_empty(), "{args:?} wrote to stderr");
        assert!(!home.exists(), "{args:?} created {}", home.display());
    }
}

/// A dangling symlink counts as missing: `Path::exists` follows it, and brgr
/// must not create the directory it points at either.
#[test]
fn internal_command_for_a_dangling_home_symlink_exits_without_creating_it() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("gone");
    let home = temp.path().join("home-link");
    std::os::unix::fs::symlink(&target, &home).unwrap();
    let output = brgr_command()
        .arg("--home")
        .arg(&home)
        .args(["__notify", "11111111-1111-4111-8111-111111111111"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.exists());
    assert!(!target.exists());
}

/// Nothing else changes: a user-facing command still creates its home on
/// first use.
#[test]
fn a_user_command_for_a_missing_home_still_creates_it() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("missing-home");
    run(&home, &["status", "--brief"], &[]);
    assert!(home.is_dir());
}
