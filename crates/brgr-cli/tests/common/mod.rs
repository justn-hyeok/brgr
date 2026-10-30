//! Shared fixtures for brgr-cli integration tests.
#![allow(dead_code)]

use std::{fs, path::Path, path::PathBuf, process::Command};

use serde_json::Value;

/// A Herdr that records every call and answers the few pane-mode and handoff
/// tests make. `status-<pane>` files choose a pane's agent state (default
/// `idle`). `list.json` and per-pane screen files, when present, feed
/// `agent list` and `pane read`.
pub fn fake_herdr(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("herdr");
    fs::write(
        &script,
        r#"#!/bin/bash
d="${FAKE_HERDR_DIR:-$(/usr/bin/dirname "$0")}"
echo "$@" >> "$d/calls.log"
if [ "$1" = "--session" ]; then shift 2; fi
case "$1 $2" in
  "pane split")
    echo '{"result":{"pane":{"pane_id":"p-new"}}}' ;;
  "pane get")
    echo "{\"result\":{\"pane\":{\"terminal_id\":\"term-$3\"}}}" ;;
  "agent list")
    if [ -f "$d/list.json" ]; then /bin/cat "$d/list.json"
    else echo '{"result":{"agents":[]}}'; fi ;;
  "agent get")
    if [ -f "$d/missing-$3" ]; then
      echo '{"error":{"code":"pane_not_found"}}' >&2
      exit 1
    fi
    s=idle
    [ -f "$d/status-$3" ] && s=$(/bin/cat "$d/status-$3")
    kind=claude
    [ -f "$d/kind-$3" ] && kind=$(/bin/cat "$d/kind-$3")
    echo "{\"result\":{\"agent\":{\"agent\":\"$kind\",\"pane_id\":\"$3\",\"agent_status\":\"$s\"}}}" ;;
  "pane layout") exit 1 ;;
  "pane read")
    f=$(printf %s "$3" | /usr/bin/sed 's/:/_/')
    if [ -f "$d/$f" ]; then /bin/cat "$d/$f"; fi ;;
  *) ;;
esac
exit 0
"#,
    )
    .unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

pub fn herdr_env<'a>(fake: &'a Path, dir: &'a Path, pane: &'a str) -> Vec<(&'static str, String)> {
    vec![
        ("HERDR_ENV", "1".to_owned()),
        ("HERDR_PANE_ID", pane.to_owned()),
        ("HERDR_BIN_PATH", fake.to_str().unwrap().to_owned()),
        ("FAKE_HERDR_DIR", dir.to_str().unwrap().to_owned()),
    ]
}

pub fn calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

/// A worker's own identity. `cargo test` inside a worker inherits it, and a
/// fixture command must not: a stale parent attempt or a session that
/// disagrees with `CODEX_THREAD_ID` fails the command before it runs.
pub const INHERITED_WORKER_ENV: &[&str] = &[
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

/// The brgr binary under test, with the worker identity removed. Debug builds
/// trust a faked `HERDR_PANE_ID` only with `BRGR_TEST_TRUST_HERDR_PANE`.
pub fn brgr_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
    command.env("BRGR_TEST_TRUST_HERDR_PANE", "1");
    for name in INHERITED_WORKER_ENV {
        command.env_remove(name);
    }
    command
}

/// Runs `brgr --home HOME ARGS` with a clean Herdr/worker environment and a
/// default `BRGR_SESSION_ID`. `--json` is part of `args` so a test can compare
/// the compact, pretty, and human forms of one command. `run`/`revise` get
/// `--headless` unless the test opts into pane fixtures.
pub fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = brgr_command();
    command.arg("--home").arg(home).args(args);
    let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
    if matches!(subcommand.copied(), Some("run" | "revise")) {
        command.arg("--headless");
    }
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
    if !envs
        .iter()
        .any(|(name, _)| matches!(*name, "CLAUDE_CODE_SESSION_ID" | "BRGR_SESSION_ID"))
    {
        command.env("BRGR_SESSION_ID", "fixture-session");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

/// Parses a successful command's stdout as JSON, failing with stderr shown.
pub fn json_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The gjc fixture harness every contract test registers.
pub fn gjc_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

/// Certifies the gjc fixture against `scratch` so `local.gjc` can be run.
pub fn add_fixture(home: &Path, fixture: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        &[
            "--json",
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
