use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

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
    let current = json_output(&run(&home, &["status", task], &[]));
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
