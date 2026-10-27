//! The note a head gets when its usage passes the configured limit.
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

const NOW_OFFSET: u64 = 0;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + NOW_OFFSET
}

fn write_signal(home: &Path, session: &str, quota: f64, at: u64) {
    let dir = home.join("context");
    fs::create_dir_all(&dir).unwrap();
    let signal = serde_json::json!({
        "session": session,
        "source": "codex",
        "at": at,
        "context_percent": 99.0,
        "context_window": 1_000_000,
        "five_hour_percent": quota,
        "five_hour_resets_at": null,
        "seven_day_percent": null,
        "seven_day_resets_at": null,
    });
    fs::write(
        dir.join(format!("{session}.json")),
        serde_json::to_vec(&signal).unwrap(),
    )
    .unwrap();
}

fn arm(home: &Path) {
    json_output(&run(
        home,
        &[
            "config",
            "set-handoff-condition",
            "--kind",
            "claude",
            "--exit-old",
            "--quota-percent",
            "70",
            "--agent-arg=--dangerously-skip-permissions",
        ],
        &[],
    ));
}

fn stderr_of(home: &Path, task_id: &str, envs: &[(&str, &str)]) -> String {
    let output = run(home, &["result", task_id], envs);
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_head_over_its_limit_is_told_once_with_the_exact_handoff_command() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm(&home);
    write_signal(&home, "session-head", 72.0, now());
    let first = stderr_of(&home, &task_id, &head_env());
    assert!(
        first.contains("five-hour plan window 72% (limit 70%)"),
        "{first}"
    );
    assert!(
        first.contains(
            "brgr head handoff TASK --kind claude --exit-old --agent-arg=--dangerously-skip-permissions"
        ),
        "{first}"
    );
    let second = stderr_of(&home, &task_id, &head_env());
    assert!(!second.contains("over the limit"), "{second}");
}

#[test]
fn nothing_is_said_without_a_configured_condition_a_fresh_signal_or_the_bound_session() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    write_signal(&home, "session-head", 99.0, now());
    // No condition configured.
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
    arm(&home);
    // A signal older than the freshness limit.
    write_signal(&home, "session-head", 99.0, now() - 3_600);
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
    // A different session than the one the owner is bound to.
    write_signal(&home, "session-other", 99.0, now());
    let other = [
        ("BRGR_OWNER_ID", "codex:head-takeover"),
        ("BRGR_SESSION_ID", "session-other"),
    ];
    assert!(!stderr_of(&home, &task_id, &other).contains("over the limit"));
}

#[test]
fn an_open_handoff_silences_the_note() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm(&home);
    write_signal(&home, "session-head", 80.0, now());
    // The request is itself a brgr command the head runs, so it is where a note
    // would first appear if an open handoff did not silence it.
    let request = run(
        &home,
        &[
            "head",
            "request",
            &task_id,
            "--to",
            "session-incoming",
            "--key",
            "open-handoff",
        ],
        &head_env(),
    );
    assert!(request.status.success());
    let said = String::from_utf8_lossy(&request.stderr);
    assert!(!said.contains("over the limit"), "{said}");
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
}

#[test]
fn a_handoff_condition_needs_a_target_and_a_percentage() {
    let (_temp, home, _workspace) = gjc_home();
    for args in [
        vec!["config", "set-handoff-condition", "--kind", "claude"],
        vec![
            "config",
            "set-handoff-condition",
            "--kind",
            "claude",
            "--quota-percent",
            "100",
        ],
        vec![
            "config",
            "set-handoff-condition",
            "--kind",
            "two words",
            "--quota-percent",
            "70",
        ],
    ] {
        assert!(!run(&home, &args, &[]).status.success(), "{args:?}");
    }
    arm(&home);
    json_output(&run(&home, &["config", "clear-handoff-condition"], &[]));
    let config = json_output(&run(&home, &["config", "show"], &[]));
    assert!(config.get("handoff").is_none(), "{config}");
}

fn write_report(home: &Path, session: &str, source: &str, quota: f64, at: u64) {
    let dir = home.join("context");
    fs::create_dir_all(&dir).unwrap();
    let signal = serde_json::json!({
        "session": session,
        "source": source,
        "at": at,
        "context_percent": null,
        "context_window": null,
        "five_hour_percent": quota,
        "five_hour_resets_at": null,
        "seven_day_percent": null,
        "seven_day_resets_at": null,
    });
    fs::write(
        dir.join(format!("{session}.json")),
        serde_json::to_vec(&signal).unwrap(),
    )
    .unwrap();
}

fn arm_with_quota(home: &Path) {
    json_output(&run(
        home,
        &[
            "config",
            "set-handoff-condition",
            "--kind",
            "claude",
            "--quota-percent",
            "85",
        ],
        &[],
    ));
}

#[test]
fn a_target_whose_plan_is_already_over_is_named_and_not_handed_to() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm_with_quota(&home);
    write_signal(&home, "session-head", 90.0, now());
    // Some Claude session reported 91% of its plan a minute ago.
    write_report(&home, "claude-elsewhere", "claude", 91.0, now() - 60);
    let said = stderr_of(&home, &task_id, &head_env());
    assert!(said.contains("claude, the harness you chose"), "{said}");
    assert!(said.contains("91% of its plan"), "{said}");
    assert!(!said.contains("handoff TASK"), "{said}");
}

#[test]
fn a_stale_or_other_harness_report_does_not_block_the_handoff() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm_with_quota(&home);
    write_signal(&home, "session-head", 90.0, now());
    // Too old to say anything about the plan now, and the wrong harness.
    write_report(&home, "claude-old", "claude", 99.0, now() - 7_200);
    write_report(&home, "codex-other", "codex", 99.0, now() - 60);
    let said = stderr_of(&home, &task_id, &head_env());
    assert!(said.contains("handoff TASK --kind claude"), "{said}");
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

fn calls_for(dir: &Path, prefix: &str) -> usize {
    calls(dir)
        .lines()
        .filter(|line| line.starts_with(prefix))
        .count()
}

fn arm_auto(home: &Path, auto: bool) {
    let mut args = vec![
        "config",
        "set-handoff-condition",
        "--kind",
        "claude",
        "--exit-old",
        "--quota-percent",
        "70",
    ];
    if auto {
        args.push("--auto");
    }
    json_output(&run(home, &args, &[]));
}

#[test]
fn an_auto_handoff_runs_from_the_head_s_own_command_once() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    arm_auto(&home, true);
    write_signal(&home, "session-head", 80.0, now());
    let herdr = herdr_env(&fake, &dir, "p-old");
    let first = run_with(&home, &["result", &task_id], &head_env(), &herdr);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(
        said.contains("a handoff to claude was started automatically (incoming pane p-new)"),
        "{said}"
    );
    assert!(
        said.contains("this pane closes after it has retired"),
        "{said}"
    );
    // The same `head handoff`: a pane beside the head, the offer, the prompt.
    assert_eq!(calls_for(&dir, "pane split"), 1, "{}", calls(&dir));
    assert!(calls(&dir).contains("agent start head-"), "{}", calls(&dir));
    assert!(
        calls(&dir).contains("Continue this owner's open work"),
        "{}",
        calls(&dir)
    );
    let offers = fs::read_dir(home.join("handoffs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("offer-"))
        .count();
    assert_eq!(offers, 1);
    // Nothing moved: the incoming head still has to take command, and a second
    // command does not start a second handoff.
    let second = run_with(&home, &["result", &task_id], &head_env(), &herdr);
    assert!(second.status.success());
    assert_eq!(calls_for(&dir, "pane split"), 1, "{}", calls(&dir));
}

#[test]
fn without_auto_no_pane_is_opened() {
    let (temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    let dir = temp.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let fake = fake_herdr(&dir);
    arm_auto(&home, false);
    write_signal(&home, "session-head", 80.0, now());
    let herdr = herdr_env(&fake, &dir, "p-old");
    let output = run_with(&home, &["result", &task_id], &head_env(), &herdr);
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("handoff TASK --kind claude"), "{said}");
    assert_eq!(calls_for(&dir, "pane split"), 0, "{}", calls(&dir));
}

#[test]
fn an_auto_handoff_that_cannot_start_says_why_and_still_gives_the_command() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm_auto(&home, true);
    write_signal(&home, "session-head", 80.0, now());
    // No Herdr pane: `head handoff` refuses without a verified one.
    let output = run(&home, &["result", &task_id], &head_env());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("the automatic handoff did not start"),
        "{said}"
    );
    assert!(said.contains("verified Herdr pane"), "{said}");
    assert!(said.contains("handoff TASK --kind claude"), "{said}");
}

#[test]
fn a_full_context_window_alone_is_never_told() {
    let (_temp, home, workspace) = gjc_home();
    let task_id = start_task(&home, &workspace);
    arm(&home);
    // The signal says 99% of the context window and 10% of the plan.
    write_signal(&home, "session-head", 10.0, now());
    assert!(!stderr_of(&home, &task_id, &head_env()).contains("over the limit"));
}

#[test]
fn the_context_threshold_is_gone_but_an_old_config_still_loads() {
    let (_temp, home, _workspace) = gjc_home();
    assert!(
        !run(
            &home,
            &[
                "config",
                "set-handoff-condition",
                "--kind",
                "claude",
                "--quota-percent",
                "80",
                "--context-percent",
                "70",
            ],
            &[],
        )
        .status
        .success()
    );
    // A config written by 2.15.5 or 2.15.6 named both thresholds.
    fs::write(
        home.join("config.toml"),
        "[handoff]\nkind = \"claude\"\ncontext_percent = 70\nquota_percent = 80\n",
    )
    .unwrap();
    let config = json_output(&run(&home, &["config", "show"], &[]));
    assert_eq!(config["handoff"]["quota_percent"], 80, "{config}");
    assert!(
        config["handoff"].get("context_percent").is_none(),
        "{config}"
    );
}
