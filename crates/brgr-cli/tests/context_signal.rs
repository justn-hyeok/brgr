//! `brgr __context` as a harness status line command.

use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::Value;
use tempfile::TempDir;

const CLAUDE: &str = include_str!("../../../testdata/fixtures/statusline/claude-2.1.293.json");
const SESSION: &str = "0a91ffd2-1a5e-4c75-b65a-54cc4b3b1fa0";

fn context(home: &Path, chain: &[&str], input: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(home)
        .arg("__context")
        .args(chain)
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn recorded(home: &Path) -> Value {
    let path = home.join("context").join(format!("{SESSION}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn a_status_line_call_is_recorded_and_the_chained_status_line_still_draws() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let seen = temp.path().join("seen.json");
    let output = context(
        &home,
        &[
            "--",
            "sh",
            "-c",
            &format!("cat > {}; echo drawn", seen.display()),
        ],
        CLAUDE,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
    // The chained command got the harness's input untouched.
    assert_eq!(fs::read_to_string(&seen).unwrap(), CLAUDE);
    let signal = recorded(&home);
    assert_eq!(signal["context_percent"], 6.0);
    assert_eq!(signal["seven_day_percent"], 43.0);
    assert_eq!(signal["context_window"], 1_000_000);
}

#[test]
fn without_a_chained_status_line_it_draws_the_measured_usage() {
    let temp = TempDir::new().unwrap();
    let output = context(&temp.path().join("home"), &[], CLAUDE);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ctx 6% · 5h 7% · 7d 43%"
    );
}

#[test]
fn input_that_is_not_a_status_line_records_nothing_and_still_chains() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let output = context(
        &home,
        &["--", "sh", "-c", "cat >/dev/null; echo drawn"],
        "not json",
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
    assert!(!home.join("context").exists());
}

#[test]
fn a_status_line_that_cannot_be_recorded_still_draws() {
    let temp = TempDir::new().unwrap();
    // A file where the context directory must go.
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("context"), "in the way").unwrap();
    let output = context(
        &home,
        &["--", "sh", "-c", "cat >/dev/null; echo drawn"],
        CLAUDE,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "drawn");
}
