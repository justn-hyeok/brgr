use std::{fs, path::Path, process::Command};

use serde_json::{Value, json};
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(brgr());
    command.arg("--home").arg(home).arg("--json").args(args);
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
fn real_cli_run_binds_candidate_to_its_owner() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap();
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
    let result = json_output(&run(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
}
