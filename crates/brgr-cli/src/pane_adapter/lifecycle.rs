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
    let result = brgr_protocol::ResultEnvelope {
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
    };
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
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(paths.launch(task, revision))?)?;
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
    let latest: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    if latest.cleanup == "closed" {
        archive_report(paths, task, revision)?;
    }
    Ok(())
}

fn archive_report(paths: &Paths, task: TaskId, revision: u32) -> Result<()> {
    crate::native_result::restore_cursor(paths, task, revision)?;
    let path = paths.pane_receipt(task, revision);
    let receipt: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
    if receipt.archived_report.is_some() {
        return Ok(());
    }
    let (Some(report), Some(expected), Some(attempt)) =
        (&receipt.report, &receipt.report_digest, receipt.attempt)
    else {
        return Ok(());
    };
    let spec = Store::open(&paths.store)?.task_for_attempt(attempt)?;
    let owned =
        PathBuf::from(&spec.workspace).join(format!(".brgr/tasks/{task}-r{revision}/report.md"));
    if *report != owned || !report.is_file() {
        return Ok(());
    }
    let bytes = read_report(report, spec.artifact_contract.max_bytes)?;
    if report_digest(&bytes) != *expected {
        bail!("handled report changed before archival");
    }
    let archive = paths.runs.join(format!("{task}-r{revision}.report.md"));
    crate::write_bytes_atomic(&archive, &bytes)?;
    fs::remove_file(report)?;
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
    update_receipt(&path, |value| value.archived_report = Some(archive))
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
