use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
    command.args(["--json", "--home"]).arg(home);
    for key in [
        "CODEX_THREAD_ID",
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "BRGR_PLUGIN_BRIDGE_DIR",
    ] {
        command.env_remove(key);
    }
    command.env("BRGR_OWNER_ID", "codex:orchestration-test");
    command.env("BRGR_SESSION_ID", "orchestration-test");
    command
}

#[test]
fn default_call_does_not_silently_run_headless_without_a_tui_source() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("work");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/gjc");
    assert!(
        command(&home)
            .args(["harness", "add"])
            .arg(fixture)
            .arg("--workspace")
            .arg(scratch)
            .args(["--prompt", "BRGR_FIXTURE_OK"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = command(&home)
        .args(["run", "BRGR_FIXTURE_OK", "--foreground", "--workspace"])
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "default call unexpectedly ran headless"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("exact Herdr source"));
    let output = command(&home)
        .args([
            "run",
            "BRGR_FIXTURE_OK",
            "--headless",
            "--foreground",
            "--workspace",
        ])
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["outcome"], "candidate");
}

#[test]
fn config_init_preserves_user_instructions_and_calls_apply_their_snapshot() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let first = command(&home).args(["config", "init"]).output().unwrap();
    assert!(first.status.success());
    fs::write(home.join("BRGR.md"), "Use concise Korean reports.").unwrap();
    let second = command(&home).args(["config", "init"]).output().unwrap();
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(home.join("BRGR.md")).unwrap(),
        "Use concise Korean reports."
    );
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fixture = bin.join("gjc");
    fs::write(&fixture,r#"#!/bin/sh
case "$1" in
--version) echo 'gjc fixture';exit 0;;
--help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking';exit 0;;
esac
for argument in "$@";do case "$argument" in @*) prompt_file=${argument#@};;esac;done
exec /usr/bin/python3 - "$prompt_file" <<'PY'
import sys,json
text=open(sys.argv[1]).read()
print(json.dumps({'type':'message_end','message':{'role':'assistant','content':[{'type':'text','text':text}]}}))
print(json.dumps({'type':'agent_end','stopReason':'completed'}))
PY
"#).unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        command(&home)
            .args(["harness", "add"])
            .arg(fixture)
            .arg("--workspace")
            .arg(&scratch)
            .args(["--prompt", "BRGR_FIXTURE_OK"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        command(&home)
            .args(["config", "set", "deadline-seconds", "100"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = command(&home)
        .args([
            "--headless",
            "run",
            "Return the worker prompt",
            "--foreground",
            "--workspace",
        ])
        .arg(&scratch)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let task = result["task_id"].as_str().unwrap();
    let detail = command(&home).args(["result", task]).output().unwrap();
    let detail: Value = serde_json::from_slice(&detail.stdout).unwrap();
    assert!(
        detail["artifacts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Use concise Korean reports.")
    );
    let launch: Value = serde_json::from_slice(
        &fs::read(home.join("launches").join(format!("{task}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(launch["spec"]["budget"]["deadline_seconds"], 100);
    assert!(
        launch["instructions_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        !command(&home)
            .args(["config", "set", "argv", "[\"--permission-mode=yolo\"]"])
            .output()
            .unwrap()
            .status
            .success()
    );
}
