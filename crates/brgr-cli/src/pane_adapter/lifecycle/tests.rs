use super::*;
use brgr_protocol::AttemptState;
use std::os::unix::fs::PermissionsExt as _;

struct Fixture {
    temp: tempfile::TempDir,
    paths: Paths,
    task: TaskId,
    attempt: AttemptId,
    receipt: PathBuf,
    report: PathBuf,
    binary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::build(|temp, _, _| {
            let workspace = temp.join("work");
            fs::create_dir_all(&workspace).unwrap();
            workspace
        })
    }

    /// The task runs in a real brgr task worktree of a fresh repository, as a
    /// Git-backed task does.
    fn in_worktree() -> Self {
        Self::build(|temp, paths, task| {
            let repository = temp.join("repo");
            fs::create_dir_all(&repository).unwrap();
            let git = |directory: &Path, args: &[&str]| {
                let status = std::process::Command::new("git")
                    .current_dir(directory)
                    .args(args)
                    .status()
                    .unwrap();
                assert!(status.success(), "git {args:?}");
            };
            git(&repository, &["init", "-q", "-b", "main"]);
            fs::write(repository.join("README"), "seed\n").unwrap();
            git(&repository, &["add", "README"]);
            git(
                &repository,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "seed",
                ],
            );
            let slug = &task.to_string()[..8];
            let workspace = paths.worktrees.join("repo").join(slug);
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("brgr/task-{slug}"),
                    workspace.to_str().unwrap(),
                ],
            );
            workspace.canonicalize().unwrap()
        })
    }

    fn build(workspace: impl FnOnce(&Path, &Paths, TaskId) -> PathBuf) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let task = TaskId::new();
        let workspace = workspace(temp.path(), &paths, task);
        let spec: TaskSpec = serde_json::from_value(serde_json::json!({
            "schema":"brgr/v1","task_id":task,"revision":1,"create_request_id":format!("recover-{task}"),
            "owner_id":"codex:recovery","objective":"Return COMPLETE","workspace":workspace,
            "route":{"harness_id":"local.fixture"},"required_capabilities":[],
            "artifact_contract":{"media_type":"text/plain","max_bytes":4096},
            "acceptance_criteria":["COMPLETE"],"budget":{"deadline_seconds":60,"max_attempts":1}
        })).unwrap();
        let mut store = Store::open(&paths.store).unwrap();
        store.record_task(&spec, "fixture").unwrap();
        if let Some(primary) = crate::workspace::primary_checkout(&workspace) {
            store
                .record_task_checkout(task, 1, &primary.to_string_lossy())
                .unwrap();
        }
        store.bind_owner(&spec.owner_id, "recovery", 1).unwrap();
        let attempt = AttemptId::new();
        store.claim_attempt(task, 1, attempt).unwrap();
        store
            .compare_and_set_attempt_state(attempt, AttemptState::Queued, AttemptState::Starting)
            .unwrap();
        store
            .compare_and_set_attempt_state(attempt, AttemptState::Starting, AttemptState::Running)
            .unwrap();
        let launch: LaunchEnvelope = serde_json::from_value(serde_json::json!({
            "spec":spec,"harness_id":"local.fixture","protocol_generation":"brgr-v1",
            "keep_pane":false,"pane_mode":true
        }))
        .unwrap();
        write_json_atomic(&paths.launch(task, 1), &launch).unwrap();
        let binary = temp.path().join("herdr");
        fs::write(&binary, r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
case "$1 $2" in
 'pane get')
  test ! -e "$d/pane-gone" || { printf '{"error":{"code":"pane_not_found","message":"pane is absent"}}\n' >&2; exit 1; }
  test ! -e "$d/pane-unavailable" || exit 7
  /bin/cat "$d/pane.json";;
 'agent get') printf '{"result":{"agent":{"pane_id":"w1:p2","terminal_id":"owned-terminal","agent_status":"%s"}}}\n' "$(/bin/cat "$d/status")";;
 'pane close') test ! -e "$d/fail-close" || exit 7; /usr/bin/touch "$d/closed";;
 *) exit 8;;
esac
"#).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            temp.path().join("pane.json"),
            r#"{"result":{"pane":{"pane_id":"w1:p2","terminal_id":"owned-terminal"}}}"#,
        )
        .unwrap();
        fs::write(temp.path().join("status"), "working").unwrap();
        let receipt = paths.pane_receipt(task, 1);
        let report = workspace.join("report.md");
        write_json_atomic(
            &receipt,
            &PaneReceipt {
                pane: "w1:p2".into(),
                binary: binary.clone(),
                task: Some(task),
                revision: 1,
                attempt: Some(attempt),
                name: Some("w1:p2".into()),
                terminal: Some("owned-terminal".into()),
                report: Some(report.clone()),
                ..PaneReceipt::default()
            },
        )
        .unwrap();
        Self {
            temp,
            paths,
            task,
            attempt,
            receipt,
            report,
            binary,
        }
    }

    fn unfinished(&self) -> brgr_store::UnfinishedAttempt {
        Store::open(&self.paths.store)
            .unwrap()
            .unfinished_attempts()
            .unwrap()
            .pop()
            .unwrap()
    }
}

#[test]
fn unfinished_report_waits_then_completed_digest_recovers_without_a_live_pane() {
    let fixture = Fixture::new();
    fs::write(&fixture.report, "PARTIAL").unwrap();
    let attempt = fixture.unfinished();
    let mut supervisor = brgr_core::Supervisor::open(&fixture.paths.store).unwrap();
    assert!(!collect_recovered(&fixture.paths, &attempt, &mut supervisor).unwrap());
    fs::write(&fixture.report, "COMPLETE").unwrap();
    update_receipt(&fixture.receipt, |receipt| {
        receipt.report_digest = Some(report_digest(b"COMPLETE"));
    })
    .unwrap();
    fs::remove_file(&fixture.binary).unwrap();
    assert!(collect_recovered(&fixture.paths, &attempt, &mut supervisor).unwrap());
    let result = supervisor.store().latest_result(fixture.task).unwrap();
    assert_eq!(result.attempt_id, fixture.attempt);
    assert_eq!(result.outcome, brgr_protocol::TerminalOutcome::Candidate);
    let replay = supervisor
        .recover_external_report(fixture.task, fixture.attempt, b"COMPLETE")
        .unwrap();
    assert_eq!(replay.result_id, result.result_id);
    assert_eq!(
        supervisor
            .store()
            .pending_for_session("recovery")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn close_checks_terminal_identity_and_retries_without_discarding_the_report() {
    let fixture = Fixture::new();
    fs::write(&fixture.report, "COMPLETE").unwrap();
    fs::write(
        fixture.temp.path().join("pane.json"),
        r#"{"result":{"pane":{"pane_id":"w1:p2","terminal_id":"replacement"}}}"#,
    )
    .unwrap();
    close_leftover_pane(&fixture.paths, fixture.task, 1, false);
    assert!(!fixture.temp.path().join("closed").exists());
    fs::write(
        fixture.temp.path().join("pane.json"),
        r#"{"result":{"pane":{"pane_id":"w1:p2","terminal_id":"owned-terminal"}}}"#,
    )
    .unwrap();
    let failure = fixture.temp.path().join("fail-close");
    fs::write(&failure, "").unwrap();
    close_leftover_pane(&fixture.paths, fixture.task, 1, false);
    let pending: PaneReceipt =
        serde_json::from_slice(&fs::read(&fixture.receipt).unwrap()).unwrap();
    assert_eq!(pending.cleanup, "pending");
    fs::remove_file(failure).unwrap();
    close_leftover_pane(&fixture.paths, fixture.task, 1, false);
    let closed: PaneReceipt = serde_json::from_slice(&fs::read(&fixture.receipt).unwrap()).unwrap();
    assert_eq!(closed.cleanup, "closed");
    assert_eq!(fs::read_to_string(&fixture.report).unwrap(), "COMPLETE");
}

#[test]
fn confirmed_absent_pane_settles_cleanup_and_archives_the_handled_report() {
    let mut fixture = Fixture::new();
    fixture.report = fixture
        .temp
        .path()
        .join(format!("work/.brgr/tasks/{}-r1/report.md", fixture.task));
    fs::create_dir_all(fixture.report.parent().unwrap()).unwrap();
    fs::write(&fixture.report, "COMPLETE").unwrap();
    update_receipt(&fixture.receipt, |receipt| {
        receipt.report = Some(fixture.report.clone());
        receipt.report_digest = Some(report_digest(b"COMPLETE"));
    })
    .unwrap();
    let mut supervisor = brgr_core::Supervisor::open(&fixture.paths.store).unwrap();
    assert!(collect_recovered(&fixture.paths, &fixture.unfinished(), &mut supervisor).unwrap());
    let result = supervisor.store().latest_result(fixture.task).unwrap();
    let owner = supervisor.store().task(fixture.task).unwrap().owner_id;
    let decision: brgr_protocol::Decision = serde_json::from_value(serde_json::json!({
        "schema":"brgr/v1", "decision_id":brgr_protocol::DecisionId::new(),
        "owner_id":owner, "task_id":fixture.task, "revision":1,
        "result_id":result.result_id, "result_digest":Store::result_digest(&result).unwrap(),
        "session_id":"recovery", "binding_epoch":1, "verdict":"accepted", "reason":"COMPLETE verified"
    }))
    .unwrap();
    supervisor
        .store()
        .record_decision_and_ack(&decision)
        .unwrap();

    let unavailable = fixture.temp.path().join("pane-unavailable");
    fs::write(&unavailable, "").unwrap();
    close_leftover_pane(&fixture.paths, fixture.task, 1, false);
    cleanup_settled(&fixture.paths, fixture.task, 1).unwrap();
    let pending: PaneReceipt =
        serde_json::from_slice(&fs::read(&fixture.receipt).unwrap()).unwrap();
    assert_eq!(pending.cleanup, "pending");
    assert!(fixture.report.exists());
    fs::remove_file(unavailable).unwrap();

    fs::write(fixture.temp.path().join("pane-gone"), "").unwrap();
    cleanup_settled(&fixture.paths, fixture.task, 1).unwrap();
    let closed: PaneReceipt = serde_json::from_slice(&fs::read(&fixture.receipt).unwrap()).unwrap();
    assert_eq!(closed.cleanup, "closed");
    assert!(closed.cleanup_error.is_none());
    assert!(!fixture.temp.path().join("closed").exists());
    assert!(!fixture.report.exists());
    assert_eq!(
        fs::read_to_string(closed.archived_report.unwrap()).unwrap(),
        "COMPLETE"
    );
    cleanup_settled(&fixture.paths, fixture.task, 1).unwrap();
}

/// A decided task whose pane disappeared before brgr closed it takes the
/// pane-already-gone path, which must reclaim the worktree like the others.
#[test]
fn a_pane_that_vanished_after_the_decision_still_reclaims_the_worktree() {
    let mut fixture = Fixture::in_worktree();
    let workspace = Store::open(&fixture.paths.store)
        .unwrap()
        .task(fixture.task)
        .unwrap()
        .workspace;
    fixture.report =
        PathBuf::from(&workspace).join(format!(".brgr/tasks/{}-r1/report.md", fixture.task));
    fs::create_dir_all(fixture.report.parent().unwrap()).unwrap();
    fs::write(&fixture.report, "COMPLETE").unwrap();
    update_receipt(&fixture.receipt, |receipt| {
        receipt.report = Some(fixture.report.clone());
        receipt.report_digest = Some(report_digest(b"COMPLETE"));
    })
    .unwrap();
    let mut supervisor = brgr_core::Supervisor::open(&fixture.paths.store).unwrap();
    assert!(collect_recovered(&fixture.paths, &fixture.unfinished(), &mut supervisor).unwrap());
    let result = supervisor.store().latest_result(fixture.task).unwrap();
    let owner = supervisor.store().task(fixture.task).unwrap().owner_id;
    let decision: brgr_protocol::Decision = serde_json::from_value(serde_json::json!({
        "schema":"brgr/v1", "decision_id":brgr_protocol::DecisionId::new(),
        "owner_id":owner, "task_id":fixture.task, "revision":1,
        "result_id":result.result_id, "result_digest":Store::result_digest(&result).unwrap(),
        "session_id":"recovery", "binding_epoch":1, "verdict":"accepted", "reason":"COMPLETE verified"
    }))
    .unwrap();
    supervisor
        .store()
        .record_decision_and_ack(&decision)
        .unwrap();
    fs::write(fixture.temp.path().join("pane-gone"), "").unwrap();
    cleanup_settled(&fixture.paths, fixture.task, 1).unwrap();
    assert!(
        !Path::new(&workspace).exists(),
        "the worktree survived the pane-already-gone path"
    );
}

#[test]
fn recovered_session_keeps_its_original_deadline() {
    let fixture = Fixture::new();
    rusqlite::Connection::open(fixture.paths.store.join("brgr.sqlite3"))
        .unwrap()
        .execute("UPDATE attempt_clocks SET started_at=0", [])
        .unwrap();
    assert!(stop_external(&fixture.paths, &fixture.unfinished()).unwrap());
    let result = Store::open(&fixture.paths.store)
        .unwrap()
        .latest_result(fixture.task)
        .unwrap();
    assert_eq!(result.outcome, brgr_protocol::TerminalOutcome::Failed);
    assert!(result.error.unwrap().contains("original deadline"));
    assert!(fixture.temp.path().join("closed").is_file());
}

#[test]
fn non_regular_report_does_not_block_a_recovery_probe() {
    let fixture = Fixture::new();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fixture.report)
            .status()
            .unwrap()
            .success()
    );
    let started = Instant::now();
    assert!(read_report(&fixture.report, 4096).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}
