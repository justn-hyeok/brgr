use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread, time::Duration};

use serde_json::{Value, json};
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(brgr());
    command.arg("--home").arg(home).arg("--json").args(args);
    command.env_remove("CODEX_THREAD_ID");
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
        json_output(&run(
            &home,
            &["harness", "add", fixture.to_str().unwrap()],
            &[],
        ));
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
    json_output(&run(
        &home,
        &["harness", "add", fixture.to_str().unwrap()],
        &[],
    ));
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
fn detached_success_reopens_for_an_offline_owner_then_accepts_once() {
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
