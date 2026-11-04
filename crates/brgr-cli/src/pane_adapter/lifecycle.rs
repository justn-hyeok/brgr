use super::*;

#[cfg(test)]
mod tests;

/// Records the pane a run opened until the run closes it. Cancellation kills
/// the runner before it can, so the supervisor closes whatever is still
/// recorded once the attempt is over.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct PaneReceipt {
    pub(crate) pane: String,
    pub(super) binary: PathBuf,
    pub(super) session: Option<String>,
    #[serde(default)]
    pub(crate) task: Option<TaskId>,
    #[serde(default)]
    pub(crate) revision: u32,
    #[serde(default)]
    pub(crate) attempt: Option<AttemptId>,
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The display name of the pane, which begins with the task's words. It is
    /// never part of a result, but a Herdr error can quote it.
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(super) terminal: Option<String>,
    #[serde(default)]
    pub(crate) report: Option<PathBuf>,
    #[serde(default)]
    pub(super) phase: String,
    #[serde(default)]
    pub(super) cleanup: String,
    #[serde(default)]
    pub(super) cleanup_error: Option<String>,
    #[serde(default)]
    pub(crate) report_digest: Option<String>,
    #[serde(default)]
    pub(crate) archived_report: Option<PathBuf>,
    /// Why a handled report was left in place instead of archived. Set once so a
    /// permanent mismatch is reported once, not on every later command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) archive_refused: Option<String>,
    #[serde(default)]
    pub(super) finished_at: Option<u64>,
    /// The pane the worker was opened beside, so its siblings can be laid out
    /// together.
    #[serde(default)]
    pub(crate) caller: Option<String>,
}

pub(super) fn update_receipt(path: &Path, update: impl FnOnce(&mut PaneReceipt)) -> Result<()> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut receipt: PaneReceipt = serde_json::from_slice(&fs::read(path)?)?;
    update(&mut receipt);
    write_json_atomic(path, &receipt)
}

pub(crate) fn seal_native_report(paths: &Paths, task: TaskId, revision: u32) -> Result<()> {
    let path = paths.pane_receipt(task, revision);
    let receipt: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    let attempt: AttemptId = env::var("BRGR_PARENT_ATTEMPT_ID")?.parse()?;
    if env::var("BRGR_PARENT_TASK_ID")? != task.to_string()
        || receipt.task != Some(task)
        || receipt.attempt != Some(attempt)
        || receipt.revision != revision
        || receipt.cleanup == "closed"
    {
        bail!("report completion differs from the current worker session");
    }
    let spec = Store::open(&paths.store)?.task(task)?;
    if spec.revision != revision {
        bail!("report belongs to an older revision");
    }
    let report = receipt.report.context("task report path is absent")?;
    let bytes = read_report(&report, spec.artifact_contract.max_bytes)?;
    let digest = report_digest(&bytes);
    if receipt
        .report_digest
        .as_ref()
        .is_some_and(|expected| *expected != digest)
    {
        bail!("completed report was changed");
    }
    update_receipt(&path, |value| {
        value.report_digest = Some(digest);
        "finished".clone_into(&mut value.phase);
        value.finished_at.get_or_insert_with(now_seconds);
    })?;
    spawn_session_server(paths, task)
}

pub(super) fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn stop_external(
    paths: &Paths,
    attempt: &brgr_store::UnfinishedAttempt,
) -> Result<bool> {
    let store = Store::open(&paths.store)?;
    let cancelled = store.cancellation_requested(attempt.task.task_id)?
        || paths.cancel(attempt.task.task_id).exists();
    let started = store
        .latest_attempt_clock(attempt.task.task_id)?
        .and_then(|value| u64::try_from(value).ok());
    let Some(started) = started else {
        return Ok(false);
    };
    let deadline = started.saturating_add(attempt.task.budget.deadline_seconds);
    let path = paths.pane_receipt(attempt.task.task_id, attempt.task.revision);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(false);
    };
    let receipt: PaneReceipt = serde_json::from_slice(&bytes)?;
    let expired =
        now_seconds() >= deadline && receipt.finished_at.is_none_or(|time| time > deadline);
    if !cancelled && !expired {
        return Ok(false);
    }
    if receipt.task != Some(attempt.task.task_id) || receipt.attempt != Some(attempt.attempt_id) {
        return Ok(false);
    }
    // Before anything closes the pane: once the agent is gone its tools have no
    // recorded parent to be found under.
    crate::tui_host::stop_descendants(&paths.runs.join(format!(
        "{}-r{}.native-state.json",
        attempt.task.task_id, attempt.task.revision
    )));
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(
        paths.launch(attempt.task.task_id, attempt.task.revision),
    )?)?;
    if launch.keep_pane {
        let native = paths.runs.join(format!(
            "{}-r{}.native-state.json",
            attempt.task.task_id, attempt.task.revision
        ));
        if crate::tui_host::alive(&native) {
            let state: crate::tui_host::NativeState = serde_json::from_slice(&fs::read(&native)?)?;
            let _ = Command::new("/bin/kill")
                .args(["-TERM", &state.pid.to_string()])
                .status()?;
        }
    } else {
        close_leftover_pane(paths, attempt.task.task_id, attempt.task.revision, false);
    }
    let mut result = brgr_protocol::ResultEnvelope {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        task_id: attempt.task.task_id,
        revision: attempt.task.revision,
        attempt_id: attempt.attempt_id,
        result_id: brgr_protocol::ResultId::new(),
        outcome: if cancelled {
            brgr_protocol::TerminalOutcome::Cancelled
        } else {
            brgr_protocol::TerminalOutcome::Failed
        },
        artifacts: Vec::new(),
        error: Some(
            if cancelled {
                "external session cancelled"
            } else {
                "external session exceeded its original deadline"
            }
            .to_owned(),
        ),
        legacy_embedded_route_observation: None,
        route_observation: Some(brgr_protocol::RouteObservation::unavailable()),
        unresolved_effects: Vec::new(),
        slept_seconds: None,
    };
    if let Some(model) =
        crate::session_model::pane_session_model(paths, &attempt.task, attempt.attempt_id)
    {
        result.route_observation = Some(brgr_protocol::RouteObservation {
            model: Some(model),
            model_source: brgr_protocol::ObservationSource::HarnessSessionLog,
            effort: None,
            effort_source: brgr_protocol::ObservationSource::Unavailable,
        });
    }
    Store::open(&paths.store)?.commit_terminal_result_final(&attempt.task.owner_id, &result)?;
    Ok(true)
}

pub(super) fn verify_receipt(
    paths: &Paths,
    task: TaskId,
    revision: u32,
    receipt: &PaneReceipt,
    herdr: &Herdr,
) -> Result<()> {
    if !receipt_pane_present(paths, task, revision, receipt, herdr)? {
        bail!("task pane no longer exists");
    }
    Ok(())
}

fn receipt_pane_present(
    paths: &Paths,
    task: TaskId,
    revision: u32,
    receipt: &PaneReceipt,
    herdr: &Herdr,
) -> Result<bool> {
    if receipt.task != Some(task) || receipt.revision != revision || receipt.cleanup == "closed" {
        bail!("task TUI receipt identity changed");
    }
    let attempt = receipt.attempt.context("task TUI attempt is absent")?;
    let spec = Store::open(&paths.store)?.task_for_attempt(attempt)?;
    if spec.task_id != task || spec.revision != revision {
        bail!("task TUI attempt identity changed");
    }
    let terminal = receipt
        .terminal
        .as_deref()
        .context("task terminal identity is absent")?;
    let pane = match herdr.call(&["pane", "get", &receipt.pane]) {
        Err(HerdrFailure::Code(code, _)) if code == "pane_not_found" => return Ok(false),
        Err(error) => bail!("task pane unavailable: {error}"),
        Ok(pane) => pane,
    };
    if pane.pointer("/result/pane/pane_id").and_then(Value::as_str) != Some(receipt.pane.as_str())
        || pane
            .pointer("/result/pane/terminal_id")
            .and_then(Value::as_str)
            != Some(terminal)
    {
        bail!("task terminal identity changed");
    }
    Ok(true)
}

pub(crate) fn session_status(paths: &Paths, task: TaskId, revision: u32) -> Option<Value> {
    let receipt: PaneReceipt =
        serde_json::from_slice(&fs::read(paths.pane_receipt(task, revision)).ok()?).ok()?;
    Some(
        serde_json::json!({"pane":receipt.pane,"terminal":receipt.terminal,"attempt":receipt.attempt,"phase":receipt.phase,"cleanup":receipt.cleanup,"cleanup_error":receipt.cleanup_error}),
    )
}

pub(crate) fn external_alive(paths: &Paths, attempt: &brgr_store::UnfinishedAttempt) -> bool {
    let Ok(bytes) = fs::read(paths.pane_receipt(attempt.task.task_id, attempt.task.revision))
    else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<PaneReceipt>(&bytes) else {
        return false;
    };
    if receipt.task != Some(attempt.task.task_id)
        || receipt.attempt != Some(attempt.attempt_id)
        || receipt.cleanup == "closed"
    {
        return false;
    }
    if let (Some(path), Some(digest)) = (&receipt.report, &receipt.report_digest)
        && read_report(path, attempt.task.artifact_contract.max_bytes)
            .is_ok_and(|bytes| report_digest(&bytes) == *digest)
    {
        return true;
    }
    let herdr = Herdr {
        binary: receipt.binary.clone().into_os_string(),
        session: receipt.session.clone(),
    };
    let state = paths.runs.join(format!(
        "{}-r{}.native-state.json",
        attempt.task.task_id, attempt.task.revision
    ));
    if crate::tui_host::alive(&state) {
        return verify_receipt(
            paths,
            attempt.task.task_id,
            attempt.task.revision,
            &receipt,
            &herdr,
        )
        .is_ok();
    }
    let Some(name) = receipt.name.as_deref() else {
        return false;
    };
    let Ok(agent) = herdr.call(&["agent", "get", name]) else {
        return false;
    };
    agent
        .pointer("/result/agent/pane_id")
        .and_then(Value::as_str)
        == Some(receipt.pane.as_str())
        && receipt.terminal.as_ref().is_none_or(|expected| {
            agent
                .pointer("/result/agent/terminal_id")
                .and_then(Value::as_str)
                == Some(expected.as_str())
        })
}

pub(crate) fn collect_recovered(
    paths: &Paths,
    attempt: &brgr_store::UnfinishedAttempt,
    supervisor: &mut brgr_core::Supervisor,
) -> Result<bool> {
    let path = paths.pane_receipt(attempt.task.task_id, attempt.task.revision);
    let Ok(data) = fs::read(&path) else {
        return Ok(false);
    };
    let receipt: PaneReceipt = serde_json::from_slice(&data)?;
    if receipt.task != Some(attempt.task.task_id)
        || receipt.attempt != Some(attempt.attempt_id)
        || receipt.revision != attempt.task.revision
    {
        return Ok(false);
    }
    if receipt.report_digest.is_none() {
        return Ok(false);
    }
    let report = receipt
        .report
        .as_ref()
        .context("native session has no report path")?;
    if !report.is_file() {
        return Ok(false);
    }
    let bytes = read_report(report, attempt.task.artifact_contract.max_bytes)?;
    let digest = report_digest(&bytes);
    if receipt
        .report_digest
        .as_ref()
        .is_some_and(|expected| *expected != digest)
    {
        bail!("completed session report digest changed");
    }
    update_receipt(&path, |value| {
        value.report_digest = Some(digest);
        value.phase.clone_from(&"finished".to_owned());
        value.finished_at.get_or_insert_with(now_seconds);
    })?;
    supervisor.recover_external_report(attempt.task.task_id, attempt.attempt_id, &bytes)?;
    Ok(true)
}

pub(crate) fn cleanup_settled(paths: &Paths, task: TaskId, revision: u32) -> Result<()> {
    let launch_path = paths.launch(task, revision);
    let launch: LaunchEnvelope = serde_json::from_slice(
        &fs::read(&launch_path).with_context(|| format!("reading {}", launch_path.display()))?,
    )?;
    if !launch.pane_mode || launch.keep_pane {
        return Ok(());
    }
    let store = Store::open(&paths.store)?;
    if matches!(
        store.revision_settlement(task, revision)?,
        brgr_store::Settlement::Open(_)
    ) {
        return Ok(());
    }
    let path = paths.pane_receipt(task, revision);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(());
    };
    let receipt: PaneReceipt = serde_json::from_slice(&bytes)?;
    if receipt.cleanup == "closed" {
        archive_report(paths, task, revision)?;
        crate::worktree_prune::reclaim_and_report(paths, task, revision);
        return Ok(());
    }
    let herdr = Herdr {
        binary: receipt.binary.clone().into_os_string(),
        session: receipt.session.clone(),
    };
    if matches!(
        receipt_pane_present(paths, task, revision, &receipt, &herdr),
        Ok(false)
    ) {
        mark_closed(&path)?;
        archive_report(paths, task, revision)?;
        crate::worktree_prune::reclaim_and_report(paths, task, revision);
        return Ok(());
    }
    if let Some(interactive) = launch
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.launch.interactive.as_ref())
        && interactive.native_host
    {
        let native = paths
            .runs
            .join(format!("{task}-r{revision}.native-state.json"));
        if !super::native::native_ready(
            &herdr,
            &receipt.pane,
            &interactive.herdr_kind,
            crate::tui_host::alive(&native),
        ) {
            return Ok(());
        }
    } else if let Some(name) = receipt.name.as_deref() {
        match herdr.status(name) {
            Ok(status) if !matches!(status.as_str(), "idle" | "done") => return Ok(()),
            _ => {}
        }
    }
    close_leftover_pane(paths, task, revision, false);
    let latest: PaneReceipt = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    if latest.cleanup == "closed" {
        archive_report(paths, task, revision)?;
        crate::worktree_prune::reclaim_and_report(paths, task, revision);
    }
    Ok(())
}

/// A point in a cleanup step where another process running the same cleanup
/// (the pane's session loop and the deciding command both do) can act between
/// this process's check and what follows it. Tests act there; a release build
/// does nothing.
#[cfg(not(test))]
pub(crate) fn other_process_may_act(_point: &str) {}

/// What the other process does at a named point, in a test.
#[cfg(test)]
type OtherProcess = Box<dyn Fn(&str)>;

#[cfg(test)]
thread_local! {
    pub(super) static OTHER_PROCESS: std::cell::RefCell<Option<OtherProcess>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn other_process_may_act(point: &str) {
    OTHER_PROCESS.with(|slot| {
        if let Some(act) = slot.borrow().as_deref() {
            act(point);
        }
    });
}

/// A handled report the receipt still records in the workspace, to be moved
/// into the run directory once the pane has closed.
struct PendingArchive {
    receipt: PathBuf,
    report: PathBuf,
    expected: String,
    max_bytes: u64,
    archive: PathBuf,
}

fn pending_archive(paths: &Paths, task: TaskId, revision: u32) -> Result<Option<PendingArchive>> {
    let path = paths.pane_receipt(task, revision);
    let receipt: PaneReceipt = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    if receipt.archived_report.is_some() || receipt.archive_refused.is_some() {
        return Ok(None);
    }
    let (Some(report), Some(expected), Some(attempt)) =
        (receipt.report, receipt.report_digest, receipt.attempt)
    else {
        return Ok(None);
    };
    let spec = Store::open(&paths.store)?.task_for_attempt(attempt)?;
    let owned =
        PathBuf::from(&spec.workspace).join(format!(".brgr/tasks/{task}-r{revision}/report.md"));
    if report != owned {
        return Ok(None);
    }
    Ok(Some(PendingArchive {
        receipt: path,
        report,
        expected,
        max_bytes: spec.artifact_contract.max_bytes,
        archive: paths.runs.join(format!("{task}-r{revision}.report.md")),
    }))
}

/// Whether the revision's handled report is still in its workspace waiting to
/// be archived. A worktree reclaim leaves such a worktree to the cleanup that
/// archives the report, which reclaims right after.
pub(crate) fn report_awaits_archival(paths: &Paths, task: TaskId, revision: u32) -> bool {
    pending_archive(paths, task, revision)
        .ok()
        .flatten()
        .is_some_and(|pending| pending.report.is_file())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// Moves the handled report out of the workspace. The pane's session loop and
/// the deciding command both run this for the same task, without a lock
/// between them, so every file this step touches may already have been handled
/// by the other process: that is the step's success, proven by the archive's
/// bytes, never by skipping the digest check. A file missing for any other
/// reason is reported, naming it.
fn archive_report(paths: &Paths, task: TaskId, revision: u32) -> Result<()> {
    crate::native_result::restore_cursor(paths, task, revision)?;
    let Some(pending) = pending_archive(paths, task, revision)? else {
        return Ok(());
    };
    let PendingArchive {
        receipt,
        report,
        expected,
        max_bytes,
        archive,
    } = pending;
    other_process_may_act("before the sealed report is read");
    let bytes = match read_report(&report, max_bytes) {
        Ok(bytes) => bytes,
        Err(error) if is_not_found(&error) => {
            // The other process writes the archive before it removes the
            // report and updates the receipt last, so a report gone after
            // this receipt was read is its work exactly when the archive
            // holds the sealed bytes.
            if read_report(&archive, max_bytes).is_ok_and(|bytes| report_digest(&bytes) == expected)
            {
                return update_receipt(&receipt, |value| value.archived_report = Some(archive));
            }
            // Reported once, like a changed report: returning an error made
            // every later command retry and warn again.
            let reason = format!(
                "sealed report {} is missing before archival",
                report.display()
            );
            update_receipt(&receipt, |value| {
                value.archive_refused = Some(reason.clone());
            })?;
            eprintln!(
                "brgr could not archive the handled report of task {task} (revision {revision}): {reason}"
            );
            return Ok(());
        }
        Err(error) => {
            // Non-NotFound read failures (for example a directory, FIFO,
            // symlink, or oversized report) cannot be archived safely. Keep
            // the owner's artifact in place and record the refusal so this
            // permanent condition does not make every command retry it.
            let reason = format!(
                "could not read sealed report {}: {error:#}",
                report.display()
            );
            update_receipt(&receipt, |value| {
                value.archive_refused = Some(reason.clone());
            })?;
            eprintln!(
                "brgr could not archive the handled report of task {task} (revision {revision}): {reason}"
            );
            return Ok(());
        }
    };
    if report_digest(&bytes) != expected {
        // A permanent mismatch: leave the file for the owner and say so once.
        // Returning an error made every later command retry and warn again.
        let reason = "handled report changed before archival";
        update_receipt(&receipt, |value| {
            value.archive_refused = Some(reason.to_owned());
        })?;
        eprintln!(
            "brgr kept the handled report of task {task} (revision {revision}) in place: {reason}: {}",
            report.display()
        );
        return Ok(());
    }
    crate::write_bytes_atomic(&archive, &bytes)
        .with_context(|| format!("writing the report archive {}", archive.display()))?;
    other_process_may_act("before the sealed report is removed");
    match fs::remove_file(&report) {
        Ok(()) => {}
        // Removed by the other process, after it wrote the same archive.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::from(error)
                .context(format!("removing the archived report {}", report.display())));
        }
    }
    let mut directory = report.parent();
    for _ in 0..3 {
        let Some(parent) = directory else {
            break;
        };
        if fs::remove_dir(parent).is_err() {
            break;
        }
        directory = parent.parent();
    }
    update_receipt(&receipt, |value| value.archived_report = Some(archive))
}

fn mark_closed(path: &Path) -> Result<()> {
    update_receipt(path, |value| {
        "closed".clone_into(&mut value.cleanup);
        value.cleanup_error = None;
    })
}

/// Closes an owned leftover pane and retains its receipt. An explicit
/// pane-not-found response also settles cleanup; transport failures retry.
pub(crate) fn close_leftover_pane(paths: &Paths, task: TaskId, revision: u32, keep_pane: bool) {
    let path = paths.pane_receipt(task, revision);
    let Some(receipt) = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PaneReceipt>(&bytes).ok())
    else {
        return;
    };
    if keep_pane || receipt.cleanup == "closed" {
        return;
    }
    let herdr = Herdr {
        binary: receipt.binary.clone().into_os_string(),
        session: receipt.session.clone(),
    };
    match receipt_pane_present(paths, task, revision, &receipt, &herdr) {
        Ok(false) => {
            let _ = mark_closed(&path);
            return;
        }
        Err(error) => {
            let _ = update_receipt(&path, |value| {
                "pending".clone_into(&mut value.cleanup);
                value.cleanup_error = Some(error.to_string());
            });
            return;
        }
        Ok(true) => {}
    }
    let _ = update_receipt(&path, |value| "closing".clone_into(&mut value.cleanup));
    if herdr.call(&["pane", "close", &receipt.pane]).is_ok() {
        let _ = mark_closed(&path);
        if let Some(caller) = receipt.caller.as_deref() {
            herdr.balance(&paths.runs, caller);
        }
        eprintln!(
            "brgr pane mode · closed pane {} left by a stopped run",
            receipt.pane
        );
    } else {
        let _ = update_receipt(&path, |value| {
            "pending".clone_into(&mut value.cleanup);
            value.cleanup_error = Some("Herdr close failed; retry retained".to_owned());
        });
    }
}
