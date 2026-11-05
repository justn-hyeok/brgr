//! Deterministic coverage of the `omp-role` adapter's execution path
//! (`src/omp_adapter.rs`): a stub `omp-role` launcher, `herdr`, `python3`,
//! and `omp-prompt` on a test PATH drive a launch, a sealed completion, the
//! `~/.omp` callback-contract recovery, and supervisor-gone recovery.
//! Everything runs under a temporary `HOME`; nothing may land in the real one.

mod common;

use common::{json_output, process_exists, run, wait_until, write_fast_script};

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use tempfile::TempDir;

const PARENT_PANE: &str = "p-owner";

struct OmpWorld {
    home: PathBuf,
    user_home: PathBuf,
    bin: PathBuf,
    repository: PathBuf,
}

/// Installs the stub toolchain under `temp` and registers `local.omp-herdr`.
/// The launcher is `python3 <launcher>` in the adapter, so `python3` is a
/// shim that runs its first argument as a shell script.
fn omp_world(temp: &TempDir) -> OmpWorld {
    let home = temp.path().join("brgr");
    let user_home = temp.path().join("user-home");
    let bin = temp.path().join("bin");
    let repository = temp.path().join("repo");
    for directory in [&user_home, &bin, &repository] {
        fs::create_dir_all(directory).unwrap();
    }
    write_fast_script(&bin.join("python3"), "#!/bin/sh\nexec /bin/sh \"$@\"\n");
    write_fast_script(
        &bin.join("omp"),
        "#!/bin/sh\ncase \"$1\" in models) echo '{\"models\":[]}';; *) exit 2;; esac\n",
    );
    // Every `agent get` increments the agent's sequence so the adapter can
    // observe a lifecycle change. `hang` keeps the status at `working`
    // forever; otherwise it is `done`.
    write_fast_script(
        &bin.join("herdr"),
        r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
echo "$@" >> "$d/herdr-calls.log"
if [ "$1" = "--session" ]; then shift 2; fi
case "$1 $2" in
  "worktree create")
    cwd="" branch="" base="" target=""
    prev=""
    for a in "$@"; do
      case "$prev" in
        --cwd) cwd=$a;; --branch) branch=$a;; --base) base=$a;; --path) target=$a;;
      esac
      prev=$a
    done
    git -C "$cwd" worktree add -b "$branch" "$target" "$base" || exit 1
    printf '{}\n'
    ;;
  "agent get")
    a="$3"
    n=0
    [ -f "$d/seq-$a" ] && n=$(/bin/cat "$d/seq-$a")
    n=$((n + 1))
    printf '%s' "$n" > "$d/seq-$a"
    s=done
    [ -f "$d/hang" ] && s=working
    printf '{"result":{"agent":{"name":"%s","agent":"omp","pane_id":"p-child","terminal_id":"t-child","agent_session":{"kind":"id","value":"sess-%s"},"state_change_seq":%s,"agent_status":"%s"}}}\n' "$a" "$a" "$n" "$s"
    ;;
  *) printf '{}\n' ;;
esac
exit 0
"#,
    );
    // `omp-prompt --expected-report <path> <agent> <instruction>`: writes the
    // canned report when `report-body` exists, then confirms the target.
    write_fast_script(
        &bin.join("omp-prompt"),
        r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
echo "$@" >> "$d/prompt-calls.log"
report="$2"
agent="$3"
if [ -f "$d/report-body" ]; then /bin/cat "$d/report-body" > "$report"; fi
printf '{"status":"prompted","target":"%s"}\n' "$agent"
"#,
    );
    // `omp-role`: `--help` serves the registration probe; a launch writes a
    // receipt naming this process's Herdr pane as the parent, then prints the
    // launcher-receipt JSON the adapter reads. `launcher-mode` selects the
    // missing-identity failure that drives `recover_omp_contract`.
    write_fast_script(
        &bin.join("omp-role"),
        r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
echo "$@" >> "$d/launcher-calls.log"
if [ "$1" = "--help" ]; then
  printf '%s\n' '--expected-report' '--reuse-worktree-objective' '--reuse-worktree-owner' '--model' '--effort'
  exit 0
fi
agent="$2"
receipt="$d/receipt-$agent.json"
printf '{"agent":"%s","parent_pane":"%s","codex_prompt_marked":false,"callback_run_id":"run-%s"}\n' \
  "$agent" "$HERDR_PANE_ID" "$agent" > "$receipt"
if [ -f "$d/launcher-mode" ]; then
  /bin/mkdir -p "$HOME/.omp/agent/callbacks/contracts"
  printf '{"detail":"immutable agent session identity is missing","receipt":"%s"}\n' "$receipt"
  exit 1
fi
printf '{"launcher_receipt":"%s"}\n' "$receipt"
"#,
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let envs = [
        ("PATH", path.as_str()),
        ("HOME", user_home.to_str().unwrap()),
    ];
    json_output(&run(
        &home,
        &[
            "harness",
            "add",
            bin.join("omp-role").to_str().unwrap(),
            "--presentation-only",
        ],
        &envs,
    ));
    seed_git_repo(&repository);
    OmpWorld {
        home,
        user_home,
        bin,
        repository,
    }
}

fn seed_git_repo(repository: &Path) {
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(repository)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    fs::write(repository.join("README"), b"seed\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-m", "seed"]);
}

/// The environment every `brgr` call in these tests runs under: the isolated
/// stub PATH, the temporary HOME, and a verified caller pane.
fn omp_env<'a>(world: &'a OmpWorld, path: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![
        ("PATH", path),
        ("HOME", world.user_home.to_str().unwrap()),
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", PARENT_PANE),
        ("BRGR_OWNER_ID", "codex:omp-owner"),
    ]
}

fn start_omp_run(world: &OmpWorld) -> String {
    let path = format!("{}:/usr/bin:/bin", world.bin.display());
    let envs = omp_env(world, &path);
    let launch = json_output(&run(
        &world.home,
        &[
            "run",
            "write the report",
            "--harness",
            "local.omp-herdr",
            "--workspace",
            world.repository.to_str().unwrap(),
        ],
        &envs
            .iter()
            .map(|(name, value)| (*name, *value))
            .collect::<Vec<_>>(),
    ));
    launch["task_id"].as_str().unwrap().to_owned()
}

fn owner_env(world: &OmpWorld) -> Vec<(String, String)> {
    let path = format!("{}:/usr/bin:/bin", world.bin.display());
    omp_env(world, &path)
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// `brgr status` drives `reconcile_pending`; poll it until the task settles,
/// then read the sealed result.
fn settle(world: &OmpWorld, task: &str) -> Value {
    let envs = owner_env(world);
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert!(
        wait_until(
            || json_output(&run(&world.home, &["status", task], &envs))["state"] == "terminal"
        ),
        "the OMP run never reached a terminal state"
    );
    json_output(&run(&world.home, &["result", task], &envs))
}

/// The real HOME must not see this test's callback contract.
fn assert_real_home_untouched(agent: &str) {
    let real_home = env::var_os("HOME").expect("test process has no HOME");
    assert!(
        !Path::new(&real_home)
            .join(".omp/agent/callbacks/contracts")
            .join(format!("{agent}.json"))
            .exists(),
        "the OMP callback contract escaped into the real HOME"
    );
}

#[test]
fn an_omp_run_reaches_the_stub_launcher_with_the_task_argv() {
    let temp = TempDir::new().unwrap();
    let world = omp_world(&temp);
    fs::write(world.bin.join("report-body"), b"# stub report\n").unwrap();
    let task = start_omp_run(&world);
    let slug = format!("brgr-{}", &task[..8]);

    let result = settle(&world, &task);
    assert_eq!(result["result"]["outcome"], "candidate", "{result}");

    let calls = fs::read_to_string(world.bin.join("launcher-calls.log")).unwrap();
    let launch_line = calls
        .lines()
        .find(|line| line.starts_with("default "))
        .expect("the stub launcher was never invoked");
    assert!(
        launch_line.starts_with(&format!("default {slug} --cwd ")),
        "{launch_line}"
    );
    for expected in [
        format!("--task {slug}"),
        format!("--reuse-worktree-objective {slug}"),
        format!("--reuse-worktree-owner {slug}"),
        "-r1.omp-report.md".to_owned(),
    ] {
        assert!(launch_line.contains(&expected), "{launch_line}");
    }
    assert!(
        launch_line.contains(&format!("{task}-r1.omp-report.md")),
        "{launch_line}"
    );
    assert_real_home_untouched(&slug);
}

#[test]
fn a_completed_omp_run_seals_the_report_the_agent_wrote() {
    let temp = TempDir::new().unwrap();
    let world = omp_world(&temp);
    let body = "# sealed stub result\n\nwith a second line\n";
    fs::write(world.bin.join("report-body"), body.as_bytes()).unwrap();
    let task = start_omp_run(&world);

    let result = settle(&world, &task);
    assert_eq!(result["result"]["outcome"], "candidate", "{result}");
    assert_eq!(result["artifacts"][0]["text"], body, "{result}");

    // Only the brgr control home and the stub directory may have been
    // written; the temporary HOME stays empty.
    assert_eq!(
        fs::read_dir(&world.user_home).unwrap().count(),
        0,
        "a run that never re-contracted still wrote under HOME"
    );
    assert_real_home_untouched(&format!("brgr-{}", &task[..8]));
}

#[test]
fn a_missing_session_identity_writes_the_callback_contract_under_home() {
    let temp = TempDir::new().unwrap();
    let world = omp_world(&temp);
    fs::write(world.bin.join("launcher-mode"), b"recover\n").unwrap();
    fs::write(world.bin.join("report-body"), b"# recovered report\n").unwrap();
    let task = start_omp_run(&world);
    let slug = format!("brgr-{}", &task[..8]);

    let result = settle(&world, &task);
    assert_eq!(result["result"]["outcome"], "candidate", "{result}");

    let contract_path = world
        .user_home
        .join(".omp/agent/callbacks/contracts")
        .join(format!("{slug}.json"));
    let contract: Value = serde_json::from_slice(&fs::read(&contract_path).unwrap()).unwrap();
    assert_eq!(contract["version"], 2, "{contract}");
    assert_eq!(contract["run_id"], format!("run-{slug}"), "{contract}");
    assert_eq!(contract["task"], slug, "{contract}");
    assert_eq!(contract["child_agent"], slug, "{contract}");
    assert_eq!(contract["child_pane"], "p-child", "{contract}");
    assert_eq!(contract["parent_pane"], PARENT_PANE, "{contract}");
    assert_eq!(
        contract["agent_session_id"],
        format!("sess-{slug}"),
        "{contract}"
    );
    assert_eq!(contract["report_contracted"], true, "{contract}");
    assert!(
        contract["expected_report"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{task}-r1.omp-report.md")),
        "{contract}"
    );
    // Nothing outside the temporary HOME.
    assert_eq!(
        fs::read_dir(&world.user_home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        [std::ffi::OsStr::new(".omp")]
    );
    assert_real_home_untouched(&slug);
}

#[test]
fn a_dead_supervisors_omp_run_is_recovered_as_terminal() {
    let temp = TempDir::new().unwrap();
    let world = omp_world(&temp);
    // The agent stays `working` and no report body exists, so the run can
    // only end through recovery, never through completion.
    fs::write(world.bin.join("hang"), b"1\n").unwrap();
    let task = start_omp_run(&world);

    let supervisor_record = world
        .home
        .join("runs")
        .join(format!("{task}.supervisor.json"));
    assert!(
        wait_until(|| supervisor_record.is_file()),
        "the supervisor never wrote its record"
    );
    let pid_file = world.home.join("runs").join(format!("{task}.pid"));
    assert!(
        wait_until(|| pid_file.is_file()),
        "the OMP worker never started"
    );
    let supervisor: Value = serde_json::from_slice(&fs::read(&supervisor_record).unwrap()).unwrap();
    let supervisor_pid = supervisor["identity"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        Command::new("/bin/kill")
            .args(["-9", &supervisor_pid])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        wait_until(|| !process_exists(&supervisor_pid)),
        "the killed supervisor {supervisor_pid} never went away"
    );
    let worker_pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();
    assert!(
        Command::new("/bin/kill")
            .args(["-9", &worker_pid])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        wait_until(|| !process_exists(&worker_pid)),
        "the OMP worker {worker_pid} outlived the test"
    );

    let envs = owner_env(&world);
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert!(
        wait_until(
            || json_output(&run(&world.home, &["status", &task], &envs))["state"] == "terminal"
        ),
        "the orphaned OMP run was never recovered"
    );
    let result = json_output(&run(&world.home, &["result", &task], &envs));
    assert_eq!(result["result"]["outcome"], "lost", "{result}");
    assert_real_home_untouched(&format!("brgr-{}", &task[..8]));
}
