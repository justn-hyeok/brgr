use std::{
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(brgr());
    command.arg("--home").arg(home).arg("--json").args(args);
    command.env_remove("CODEX_THREAD_ID");
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
    command.env("BRGR_SESSION_ID", "fixture-session");
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
smart_mode=0
trust_bypassed=0
while test "$#" -gt 0; do
  case "$1" in
    --prompt-file) shift; prompt_file=$1;;
    -p|--print) print_mode=1;;
    --permission-mode) shift; test "$1" = smart || exit 3; smart_mode=1;;
    --respect-workspace-trust) shift; test "$1" = false || exit 4; trust_bypassed=1;;
    *) exit 5;;
  esac
  shift
done
test "$print_mode" = 1 || exit 6
test "$smart_mode" = 1 || exit 7
test "$trust_bypassed" = 1 || exit 8
test -f "$prompt_file" || exit 10
/bin/cat "$prompt_file"
"#;

fn write_devin_fixture(executable: &Path) {
    fs::write(executable, DEVIN_FIXTURE).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn plugin_board_shows_candidate_then_explicit_decision_without_prompt_text() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));

    let fake_codex = fake_bin.join("codex");
    fs::write(
        &fake_codex,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRGR_TEST_CODEX_ARGS\"\nexport CODEX_THREAD_ID=bridge-test\n\"$BRGR_BIN\" --json harness add /bin/echo --presentation-only > /dev/null 2> \"$BRGR_TEST_DENIED_HARNESS\"\nprintf '%s\\n' \"$?\" >> \"$BRGR_TEST_DENIED_HARNESS\"\n\"$BRGR_BIN\" --json run ESCAPE --harness local.gjc --workspace \"$BRGR_TEST_OUTSIDE\" --foreground > /dev/null 2> \"$BRGR_TEST_DENIED_WORKSPACE\"\nprintf '%s\\n' \"$?\" >> \"$BRGR_TEST_DENIED_WORKSPACE\"\nexec \"$BRGR_BIN\" --json run BRGR_FIXTURE_OK --harness local.gjc --criterion 'artifact text equals BRGR_FIXTURE_OK' --workspace \"$BRGR_TEST_WORKSPACE\" --foreground > \"$BRGR_TEST_OUTPUT\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let context = json!({"workspace_id": "w1", "workspace_cwd": workspace});
    let launched = Command::new(brgr())
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
        vec!["local.gjc"]
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
    fs::write(
        &fake_codex,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRGR_TEST_CODEX_ARGS\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
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
    let launched = Command::new(brgr())
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

fn ordinary_herdr_pane_opens_a_brgr_worker(
    home: &Path,
    workspace: &Path,
    fake_herdr: &Path,
    herdr_args: &Path,
) {
    json_output(&run(
        home,
        &["config", "set-worker-placement", "adjacent"],
        &[],
    ));
    let ordinary_herdr_env = [
        ("BRGR_OWNER_ID", "codex:plugin-worker"),
        ("HERDR_ENV", "1"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_PANE_ID", "w1:p9"),
        ("HERDR_BIN_PATH", fake_herdr.to_str().unwrap()),
        ("BRGR_TEST_HERDR_ARGS", herdr_args.to_str().unwrap()),
    ];
    let disabled = json_output(&run(
        home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &ordinary_herdr_env,
    ));
    assert!(disabled.get("worker_pane").is_none());
    let disabled_task = disabled["task_id"].as_str().unwrap();
    let owner = [("BRGR_OWNER_ID", "codex:plugin-worker")];
    assert_eq!(
        json_output(&run(
            home,
            &["wait", disabled_task, "--timeout-seconds", "10"],
            &owner,
        ))["outcome"],
        "candidate"
    );
    assert_eq!(
        json_output(&run(home, &["accept", disabled_task], &owner))["verdict"],
        "accepted"
    );
    let enabled = json_output(&run(home, &["config", "set-auto-worker-pane", "true"], &[]));
    assert_eq!(enabled["herdr"]["auto_worker_pane"], true);
    let ordinary = json_output(&run(
        home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &ordinary_herdr_env,
    ));
    assert_eq!(ordinary["worker_placement"], "adjacent");
    assert_eq!(ordinary["worker_pane"], "w1:p2");
    let ordinary_args = fs::read_to_string(herdr_args).unwrap();
    assert!(ordinary_args.contains("--target-pane\nw1:p9\n"));
    assert!(!ordinary_args.contains("--workspace\n"));
}

#[test]
fn plugin_worker_placement_respects_config_and_reaches_owner_decision() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let fake_herdr = temp.path().join("herdr");
    let herdr_args = temp.path().join("herdr-args");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        &fake_herdr,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRGR_TEST_HERDR_ARGS\"\nprintf '%s\\n' '{\"result\":{\"type\":\"plugin_pane_opened\",\"plugin_pane\":{\"plugin_id\":\"brgr\",\"entrypoint\":\"worker\",\"pane\":{\"pane_id\":\"w1:p2\"}}}}'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let host_home = home.to_str().unwrap();
    let host_env = [
        ("BRGR_OWNER_ID", "codex:plugin-worker"),
        ("HERDR_ENV", "1"),
        ("HERDR_PLUGIN_ID", "brgr"),
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_SESSION", "fixture-session"),
        ("HERDR_BIN_PATH", fake_herdr.to_str().unwrap()),
        ("BRGR_PLUGIN_HOST_HOME", host_home),
        ("BRGR_PLUGIN_HOST_WORKSPACE", workspace.to_str().unwrap()),
        ("BRGR_TEST_HERDR_ARGS", herdr_args.to_str().unwrap()),
    ];
    let first = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &host_env,
    ));
    assert_eq!(first["worker_placement"], "adjacent");
    assert_eq!(first["worker_pane"], "w1:p2");
    let first_args = fs::read_to_string(&herdr_args).unwrap();
    assert!(first_args.starts_with("--session\nfixture-session\n"));
    assert!(first_args.contains("--target-pane\nw1:p1\n"));
    assert!(first_args.contains("--placement\nsplit\n"));
    assert!(!first_args.contains("--workspace\n"));
    assert!(first_args.contains("--no-focus\n"));

    let config = json_output(&run(&home, &["config", "set-worker-placement", "tab"], &[]));
    assert_eq!(config["herdr"]["worker_placement"], "tab");
    let second = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &host_env,
    ));
    assert_eq!(second["worker_placement"], "tab");
    let second_args = fs::read_to_string(&herdr_args).unwrap();
    assert!(second_args.contains("--placement\ntab\n"));
    assert!(second_args.contains("--workspace\nw1\n"));
    assert!(!second_args.contains("--target-pane\n"));

    ordinary_herdr_pane_opens_a_brgr_worker(&home, &workspace, &fake_herdr, &herdr_args);

    let task = first["task_id"].as_str().unwrap();
    let launch = home.join("launches").join(format!("{task}.json"));
    let worker = run(
        &home,
        &["plugin", "worker"],
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PLUGIN_ID", "brgr"),
            ("BRGR_PLUGIN_WORKER_LAUNCH", launch.to_str().unwrap()),
        ],
    );
    assert!(
        worker.status.success(),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    let result = json_output(&run(&home, &["result", task], &host_env));
    assert_eq!(result["result"]["outcome"], "candidate");
    assert_eq!(result["artifacts"][0]["text"], "BRGR_FIXTURE_OK");
    let decision = json_output(&run(
        &home,
        &["accept", task, "--reason", "fixture output verified"],
        &host_env,
    ));
    assert_eq!(decision["verdict"], "accepted");
}

const RECURSIVE_GJC_FIXTURE: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) echo 'gjc v-recursive-fixture'; exit 0;;
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
    "$BRGR_BIN" --json run LEAF --harness local.gjc --workspace "$PWD" --foreground >/dev/null
  elif /usr/bin/grep -q ROOT "$prompt_file"; then
    child_json=$("$BRGR_BIN" --json run CHILD --harness local.gjc --workspace "$PWD")
    child_task=$(printf '%s\n' "$child_json" | /usr/bin/sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
    test -n "$child_task"
    "$BRGR_BIN" --json wait "$child_task" --timeout-seconds 10 >/dev/null
    "$BRGR_BIN" --json result "$child_task" >/dev/null
    "$BRGR_BIN" --json accept "$child_task" --reason 'child artifact checked' >/dev/null
  elif /usr/bin/grep -q CHILD "$prompt_file"; then
    child_json=$("$BRGR_BIN" --json run LEAF --harness local.gjc --workspace "$PWD")
    child_task=$(printf '%s\n' "$child_json" | /usr/bin/sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
    test -n "$child_task"
    "$BRGR_BIN" --json wait "$child_task" --timeout-seconds 10 >/dev/null
    "$BRGR_BIN" --json result "$child_task" >/dev/null
    "$BRGR_BIN" --json accept "$child_task" --reason 'leaf artifact checked' >/dev/null
  fi
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_RECURSIVE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

#[test]
fn a_worker_can_delegate_twice_and_decide_each_child_before_reporting() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&executable, RECURSIVE_GJC_FIXTURE).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));

    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.gjc",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(root["outcome"], "candidate");
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
    let root_result = json_output(&run(
        &home,
        &["result", root_task],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(root_result["artifacts"][0]["text"], "BRGR_RECURSIVE_OK");

    let unsettled = json_output(&run(
        &home,
        &[
            "run",
            "UNSETTLED",
            "--harness",
            "local.gjc",
            "--workspace",
            workspace.to_str().unwrap(),
            "--enable-delegation",
            "--foreground",
        ],
        &[("BRGR_OWNER_ID", "codex:recursive-root")],
    ));
    assert_eq!(unsettled["outcome"], "failed");
    assert!(
        unsettled["error"]
            .as_str()
            .unwrap()
            .contains("child task(s) remain")
    );
}

#[test]
fn recursive_worker_uses_sibling_worktrees_from_a_git_parent() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    fs::write(&executable, RECURSIVE_GJC_FIXTURE).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let root = json_output(&run(
        &home,
        &[
            "run",
            "ROOT",
            "--harness",
            "local.gjc",
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    let root_attempt = (0..200)
        .find_map(|_| {
            let found = store.active_message_attempt(root_id).ok();
            if found.is_none() {
                thread::sleep(Duration::from_millis(25));
            }
            found
        })
        .expect("root attempt did not become active");
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
        &["wait", root_task, "--timeout-seconds", "10"],
        &root_owner,
    ));
    assert_eq!(terminal["outcome"], "cancelled");
}

#[test]
fn tree_status_shows_question_and_tree_cancel_stops_both_processes() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let root_attempt = (0..100)
        .find_map(|_| {
            let found = store
                .active_message_attempt(root_task.parse().unwrap())
                .ok();
            if found.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
            found
        })
        .expect("root attempt did not become active");
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
    for _ in 0..100 {
        if store
            .active_message_attempt(child_task.parse().unwrap())
            .is_ok()
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
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
        &["wait", child_task, "--timeout-seconds", "10"],
        &worker,
    ));
    assert_eq!(child_terminal["outcome"], "cancelled", "{child_terminal}");
    assert_eq!(
        json_output(&run(
            &home,
            &["wait", root_task, "--timeout-seconds", "10"],
            &root_owner
        ))["outcome"],
        "cancelled"
    );
}

#[test]
fn committed_cancellation_intent_stops_running_worker_without_cancel_file() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    for _ in 0..100 {
        if store.active_message_attempt(task_id).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(store.active_message_attempt(task_id).is_ok());
    store.record_cancellation_intents(task_id, false).unwrap();
    let result = json_output(&run(
        &home,
        &["wait", &task, "--timeout-seconds", "10"],
        &owner,
    ));
    assert_eq!(result["outcome"], "cancelled");
}

#[test]
fn terminal_tree_cancellation_does_not_cancel_a_later_revision() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&executable, RECURSIVE_GJC_FIXTURE).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    let output = Command::new(brgr())
        .current_dir(temp.path())
        .args([
            "--home",
            "relative-home",
            "--json",
            "run",
            "ROOT",
            "--harness",
            "local.gjc",
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let task_id = task.parse().unwrap();
    let store = brgr_store::Store::open(home.join("store")).unwrap();
    let attempt = (0..100)
        .find_map(|_| {
            let found = store.active_message_attempt(task_id).ok();
            if found.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
            found
        })
        .expect("worker attempt did not become active");
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
        &["wait", task, "--timeout-seconds", "10"],
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-instructions-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q CHECK_INSTRUCTIONS \"$prompt\"; then\n /usr/bin/grep -q 'snapshot matches' \"$prompt\" || exit 7\n /usr/bin/grep -q 'Only docs' \"$prompt\" || exit 8\n /usr/bin/grep -q 'Act as reviewer' \"$prompt\" || exit 9\nfi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"INSTRUCTIONS_OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    fs::write(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') state=$(/bin/cat \"$BRGR_TEST_AGENT_STATE\"); printf '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p1\",\"agent_status\":\"%s\",\"agent_session\":{\"value\":\"session-a\"}}}}\\n' \"$state\";;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    for _ in 0..100 {
        if store.latest_result(task.parse().unwrap()).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(store.latest_result(task.parse().unwrap()).is_ok());
    thread::sleep(Duration::from_millis(700));
    assert!(!prompts.exists(), "busy Codex parent was prompted");
    assert_duplicate_notification_dispatcher_exits(&home, task, &envs);
    fs::write(&agent_state, "idle").unwrap();
    for _ in 0..100 {
        if prompts.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let prompt = fs::read_to_string(&prompts).expect("completion was not delivered");
    assert!(prompt.contains("FROM BRGR"));
    assert!(prompt.contains(task));
    for _ in 0..100 {
        if store
            .pending_notifications_for_task(task.parse().unwrap())
            .unwrap()
            .is_empty()
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        store
            .pending_notifications_for_task(task.parse().unwrap())
            .unwrap()
            .is_empty()
    );
    thread::sleep(Duration::from_millis(700));
    assert_eq!(fs::read_to_string(prompts).unwrap(), prompt);
}

fn assert_duplicate_notification_dispatcher_exits(home: &Path, task: &str, envs: &[(&str, &str)]) {
    let mut duplicate = Command::new(brgr())
        .arg("--home")
        .arg(home)
        .arg("__notify")
        .arg(task)
        .envs(envs.iter().copied())
        .spawn()
        .unwrap();
    let duplicate_started = Instant::now();
    while duplicate_started.elapsed() < Duration::from_secs(2) {
        if duplicate.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if duplicate.try_wait().unwrap().is_none() {
        duplicate.kill().unwrap();
        duplicate.wait().unwrap();
        panic!("duplicate notification dispatcher stayed active");
    }
}

#[test]
fn final_failed_result_does_not_leave_a_notification_dispatcher_running() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&executable, "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-failed-notice-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q BRGR_FIXTURE_OK \"$prompt\"; then\n printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"BRGR_FIXTURE_OK\"}]}}'\n printf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\nelse\n exit 7\nfi\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut dispatcher = Command::new(brgr())
        .arg("--home")
        .arg(&home)
        .arg("__notify")
        .arg(task)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if dispatcher.try_wait().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
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
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&executable, r#"#!/bin/sh
case "$1" in
 --version) echo 'gjc v-failed-log-fixture'; exit 0;;
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
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    fs::write(
        &herdr,
        "#!/bin/sh\nif [ \"$1\" = --session ]; then shift 2; fi\ncase \"$1 $2\" in\n 'agent get') printf '%s\\n' '{\"result\":{\"agent\":{\"agent\":\"codex\",\"pane_id\":\"w1:p2\",\"agent_status\":\"idle\",\"agent_session\":{\"value\":\"session-b\"}}}}';;\n 'agent prompt') printf '%s\\n' \"$4\" >> \"$BRGR_TEST_PROMPTS\"; printf '{}\\n';;\n *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
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
    for _ in 0..100 {
        if prompts.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let prompt = fs::read_to_string(prompts).expect("transferred Codex session was not prompted");
    assert!(prompt.contains(&task));
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
fn omp_worker_delegates_to_gjc_then_gjc_without_pair_specific_routing() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let gjc = temp.path().join("gjc");
    let omp = temp.path().join("omp");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(&gjc, RECURSIVE_GJC_FIXTURE).unwrap();
    let omp_fixture = RECURSIVE_GJC_FIXTURE
        .replace("gjc v-recursive-fixture", "omp/fixture")
        .replace(
            "'--no-mcp' '--model' '--thinking'",
            "'--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'",
        );
    fs::write(&omp, omp_fixture).unwrap();
    for executable in [&gjc, &omp] {
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
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
            .filter(|task| task.route.harness_id == "local.gjc")
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
    let launched = Command::new(brgr())
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
fn plugin_open_surfaces_herdr_launch_failure() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let fake_bin = temp.path().join("bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    let herdr = fake_bin.join("herdr");
    fs::write(&herdr, "#!/bin/sh\necho 'pane refused' >&2\nexit 7\n").unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let context = json!({"workspace_id": "w1", "workspace_cwd": workspace});
    let launched = Command::new(brgr())
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
fn codex_integration_accepts_an_equivalent_caller_and_detects_hook_binary_drift() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let codex_home = temp.path().join("codex");
    let hook_binary = temp.path().join("installed-brgr");
    fs::copy(brgr(), &hook_binary).unwrap();
    fs::set_permissions(&hook_binary, fs::Permissions::from_mode(0o700)).unwrap();

    let installed = Command::new(&hook_binary)
        .arg("--home")
        .arg(&home)
        .arg("--json")
        .args(["integrate", "codex", "install"])
        .env("CODEX_HOME", &codex_home)
        .output()
        .unwrap();
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
    let output = Command::new(brgr())
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
    let executable = temp.path().join("gjc");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/gjc");
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
    assert_eq!(report["harnesses"][0]["id"], "local.gjc");
    assert_eq!(report["harnesses"][0]["health"], "executable_changed");
    let action = report["harnesses"][0]["action"].as_str().unwrap();
    assert!(action.contains("re-certify"));
    assert!(action.contains("harness add"));
    assert!(action.contains("--prompt"));
    let dumped = String::from_utf8_lossy(&changed.stdout);
    assert!(!dumped.contains("--mode=<value>"));
    assert!(!dumped.contains("BRGR_FIXTURE_OK"));
}

#[test]
fn named_process_harness_cannot_activate_without_authorized_scratch() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    let denied = run(&home, &["harness", "add", fixture.to_str().unwrap()], &[]);
    assert!(!denied.status.success());
    assert!(!home.join("registry/activations/local.gjc.json").exists());

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
    assert!(!home.join("registry/activations/local.gjc.json").exists());

    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let health = json_output(&run(&home, &["harness", "status", "local.gjc"], &[]));
    assert_eq!(health["health"], "healthy");

    let activation_path = home.join("registry/activations/local.gjc.json");
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
    fs::write(
        &omp,
        "#!/bin/sh\ncase \"$1\" in models) echo '{\"models\":[]}';; *) exit 2;; esac\n",
    )
    .unwrap();
    let launcher = binaries.join("omp-role");
    fs::write(
        &launcher,
        "#!/bin/sh\necho '--expected-report --reuse-worktree-objective --reuse-worktree-owner --model --effort'\n",
    )
    .unwrap();
    for executable in [&omp, &launcher] {
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
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
    assert!(String::from_utf8_lossy(&denied.stderr).contains("absent from the current catalog"));
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
        &[("BRGR_OWNER_ID", "codex:owner-b")],
    );
    assert!(!unreadable.status.success());
    let denied = run(
        &home,
        &["accept", task],
        &[("BRGR_OWNER_ID", "codex:owner-b")],
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

#[test]
fn control_home_inside_worker_workspace_is_rejected_before_task_admission() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("work");
    let home = workspace.join("brgr-control");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'prompt-only 1';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file) /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    assert_eq!(sealed["artifacts"][0]["text"], "GENERIC_PROCESS_OK");
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "generic result checked"],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
}

#[test]
fn missing_write_browser_and_mcp_capabilities_fail_before_task_admission() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    assert!(String::from_utf8_lossy(&explicit_model.stderr).contains("model_select"));
    assert_eq!(fs::read_dir(home.join("launches")).unwrap().count(), 0);

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
    assert_eq!(detail["artifacts"][0]["text"], "DEVIN_PROCESS_OK");
    assert_eq!(detail["route_observation"]["model_source"], "unavailable");
    let accepted = json_output(&run(
        &home,
        &["accept", task, "--reason", "Devin fixture result verified"],
        &owner,
    ));
    assert_eq!(accepted["verdict"], "accepted");
}

#[test]
fn authored_manifest_runs_unknown_positional_cli_to_owner_acceptance() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let executable = temp.path().join("positional-agent");
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'positional 1';;\n --help) echo '  -p, --print prompt';;\n -p) printf '%s' \"$2\";;\n *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    assert_eq!(sealed["artifacts"][0]["text"], "CUSTOM_MANIFEST_OK");
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let mut terminal = None;
    for _ in 0..120 {
        let status = json_output(&run(&home, &["status", task], &owner));
        if status["state"] == "terminal" {
            terminal = Some(json_output(&run(&home, &["result", task], &owner)));
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let terminal = terminal.expect("abandoned admitted task did not settle");
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
fn wait_reconciles_a_crashed_detached_supervisor_to_lost() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
        &["wait", task, "--timeout-seconds", "8"],
        &owner,
    ));
    assert_eq!(waited["outcome"], "lost");
    let result = json_output(&run(&home, &["result", task], &owner));
    assert_eq!(waited["result_id"], result["result"]["result_id"]);
}

#[cfg(debug_assertions)]
#[test]
fn detached_crash_windows_reconcile_without_duplicate_or_overlapping_attempts() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
        let mut terminal = None;
        for _ in 0..120 {
            let status = json_output(&run(&home, &["status", task], &owner));
            if status["state"] == "terminal" {
                terminal = Some(json_output(&run(&home, &["result", task], &owner)));
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let terminal = terminal.unwrap_or_else(|| panic!("crash stage {stage} did not settle"));
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let owner = ("BRGR_OWNER_ID", "codex:pre-identity");
    let launch = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &[owner, ("BRGR_TEST_PAUSE_AFTER_CLAIM_MS", "1500")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let mut saw_window = false;
    for _ in 0..100 {
        let store = brgr_store::Store::open(home.join("store")).unwrap();
        if store
            .unfinished_attempts()
            .unwrap()
            .iter()
            .any(|attempt| attempt.task.task_id.to_string() == task && attempt.launch.is_none())
        {
            let status = json_output(&run(&home, &["status", task], &[owner]));
            assert_ne!(status["state"], "terminal");
            saw_window = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(saw_window, "the live pre-identity window was not observed");
    let mut result = None;
    for _ in 0..120 {
        let status = json_output(&run(&home, &["status", task], &[owner]));
        if status["state"] == "terminal" {
            result = Some(json_output(&run(&home, &["result", task], &[owner])));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        result.unwrap()["result"]["outcome"],
        "candidate",
        "a live supervisor was incorrectly recovered as lost"
    );
}

#[test]
fn queued_cancel_settles_without_starting_the_harness() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
    let owner = [("BRGR_OWNER_ID", "codex:queued-cancel")];
    let launch = json_output(&run(
        &home,
        &["run", "SLOW", "--workspace", workspace.to_str().unwrap()],
        &[owner[0], ("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1")],
    ));
    let task = launch["task_id"].as_str().unwrap();
    let requested = json_output(&run(&home, &["cancel", task], &owner));
    assert_eq!(requested["state"], "cancel_requested");
    let mut outcome = None;
    for _ in 0..120 {
        let status = json_output(&run(&home, &["status", task], &owner));
        if status["state"] == "terminal" {
            outcome = Some(json_output(&run(&home, &["result", task], &owner)));
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let result = outcome.expect("queued cancellation did not settle");
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'omp fixture 1'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'; exit 0;;\nesac\nfor argument in \"$@\"; do case \"$argument\" in @*) prompt_file=${argument#@};; esac; done\nif /usr/bin/grep -q SLOW \"$prompt_file\"; then /bin/sleep 30; fi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OMP_FIXTURE_OK\"}]}}' '{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}' '{\"type\":\"agent_end\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    for _ in 0..100 {
        if run(&home, &["status", task], &owner).status.success() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let requested = json_output(&run(&home, &["cancel", task], &owner));
    assert_eq!(requested["state"], "cancel_requested");
    let mut final_result = None;
    for _ in 0..100 {
        let output = run(&home, &["result", task], &owner);
        if output.status.success() {
            final_result = Some(json_output(&output));
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(final_result.unwrap()["result"]["outcome"], "cancelled");
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
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'omp fixture 1'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-prewalk' '--no-extensions' '--no-title' '--model=<value>' '--thinking=<value>'; exit 0;;\n models) echo '{\"models\":[{\"selector\":\"other/fallback\"},{\"selector\":\"workbuddy/deepseek-v4.1-flash\"}]}'; exit 0;;\nesac\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"provider\":\"other\",\"model\":\"fallback\",\"content\":[{\"type\":\"text\",\"text\":\"SHOULD_NOT_ACCEPT\"}]}}' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    add_fixture(&home, &fixture, &temp.path().join("scratch"));
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

    let mut terminal = None;
    for _ in 0..120 {
        let status = json_output(&run(&home, &["status", &task], &owner));
        if status["state"] == "terminal" {
            terminal = Some(json_output(&run(&home, &["result", &task], &owner)));
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let terminal = terminal.expect("detached fixture run did not settle for its offline owner");
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
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
    fs::write(script, "#!/bin/sh\nprintf 'NOT_A_PATCH\\n'\n").unwrap();
    fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
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
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    configure_misleading_external_diff(&repository, &temp.path().join("external-diff"));
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-evidence-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nfor item in \"$@\"; do case \"$item\" in @*) prompt=${item#@};; esac; done\nif /usr/bin/grep -q EVIDENCE_TASK \"$prompt\"; then\n printf 'changed\\n' > README\n printf 'report\\n' > report.md\n printf 'PNG_BYTES' > screen.png\n printf 'worker log\\n' >&2\nfi\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"EVIDENCE_OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
}

#[test]
fn selected_input_is_excluded_from_worker_patch_and_source_edit_survives_apply() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    let executable = temp.path().join("gjc");
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
    fs::write(&executable, "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-snapshot-patch-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'worker edit\\n' > worker.txt\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    let executable = temp.path().join("gjc");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    fs::write(&executable, "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-commit-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'committed worker edit\\n' > README\n/usr/bin/git add README\n/usr/bin/git -c user.name=Worker -c user.email=worker@example.test commit -m worker-edit >/dev/null\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    let executable = temp.path().join("gjc");
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
    fs::write(&executable, "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc v-subdir-fixture'; exit 0;;\n --help) printf '%s\\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;\nesac\nprintf 'worker root edit\\n' > ../README\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}}'\nprintf '%s\\n' '{\"type\":\"agent_end\",\"stopReason\":\"completed\"}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    assert!(!patch.contains("sub/a"));
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

fn gjc_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

#[test]
fn concurrent_git_worktree_admissions_keep_distinct_identities() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &gjc_fixture(), &temp.path().join("scratch"));
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
    add_fixture(&home, &gjc_fixture(), &temp.path().join("scratch"));
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

#[test]
fn missing_task_worktree_fails_closed_without_recreate_or_new_identity() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &gjc_fixture(), &temp.path().join("scratch"));
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
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'slow 1';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file)\n    if /usr/bin/grep -q SLOW_RUN \"$2\"; then /bin/sleep 4; fi\n    /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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

#[test]
fn foreground_admission_lock_is_released_before_slow_harness_execution() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let repository = temp.path().join("repo");
    fs::create_dir_all(&repository).unwrap();
    seed_git_repo(&repository);
    add_fixture(&home, &gjc_fixture(), &temp.path().join("gjc-scratch"));
    add_slow_prompt_file_harness(&home, &temp.path().join("slow"));
    let repo = repository.to_str().unwrap().to_owned();
    let home_path = home.clone();
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            run(
                &home_path,
                &[
                    "run",
                    "SLOW_RUN",
                    "--harness",
                    "local.slow-agent",
                    "--workspace",
                    &repo,
                    "--foreground",
                ],
                &[("BRGR_OWNER_ID", "codex:worktree-slow")],
            )
        });
        let admitted = Instant::now();
        while admitted.elapsed() < Duration::from_secs(2) {
            if home_path
                .join("launches")
                .read_dir()
                .map_or(0, Iterator::count)
                > 0
            {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let second_started = Instant::now();
        let second = json_output(&run(
            &home_path,
            &[
                "run",
                "BRGR_FIXTURE_OK",
                "--workspace",
                &repo,
                "--foreground",
            ],
            &[("BRGR_OWNER_ID", "codex:worktree-fast")],
        ));
        let second_elapsed = second_started.elapsed();
        assert_eq!(second["outcome"], "candidate");
        assert!(
            second_elapsed < Duration::from_secs(3),
            "second admission waited on the first foreground run: {second_elapsed:?}"
        );
        assert!(
            !first.is_finished(),
            "second task finished only after the slow first harness completed"
        );
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
