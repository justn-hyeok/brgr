//! Durable workflow runs: create, status, and owner-driven state changes.

mod driver;
mod init;

pub(crate) use driver::trusts_recorded_pane;

use std::fmt::Write as _;

use anyhow::{Result, bail};
use brgr_store::{Store, WorkflowRun, WorkflowRunId, WorkflowRunState, WorkflowStepKey};
use clap::Subcommand;
use serde_json::json;
use uuid::Uuid;

use crate::admission::bind_owner_session;
use crate::{
    Paths, current_session, owner_from_environment, print_value, require_owner, task_commands,
};

#[derive(Subcommand)]
pub(crate) enum WorkflowCommand {
    /// Record a new workflow run owned by the calling owner.
    Create {
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=64))]
        max_parallel: u32,
        #[arg(long)]
        script_digest: Option<String>,
    },
    /// Show a run and each of its steps, in journal order.
    Status { run: String },
    /// Move a run between the states the journal allows.
    SetState { run: String, state: String },
    /// Start a workflow script under a detached node driver.
    Run {
        script: std::path::PathBuf,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=64))]
        max_parallel: u32,
    },
    /// Start the driver again after it died, from the top of the same script.
    Resume { run: String },
    /// Stop the driver and every unfinished step of a run.
    Cancel { run: String },
    /// Write the JavaScript SDK and an example script into a directory.
    Init {
        #[arg(value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Replace an existing SDK or example file even if its contents differ.
        #[arg(long)]
        force: bool,
    },
    #[command(name = "__drive", hide = true)]
    Drive {
        run: String,
        script: std::path::PathBuf,
    },
}

pub(crate) fn is_hidden_drive(command: &WorkflowCommand) -> bool {
    matches!(command, WorkflowCommand::Drive { .. })
}

pub(crate) fn run(paths: &Paths, command: WorkflowCommand, json_output: bool) -> Result<()> {
    match command {
        WorkflowCommand::Create {
            max_parallel,
            script_digest,
        } => create(paths, max_parallel, script_digest, json_output),
        WorkflowCommand::Status { run } => status(paths, &run, json_output),
        WorkflowCommand::SetState { run, state } => set_state(paths, &run, &state, json_output),
        WorkflowCommand::Run {
            script,
            max_parallel,
        } => driver::run_script(paths, &script, max_parallel, json_output),
        WorkflowCommand::Resume { run } => driver::resume(paths, &run, json_output),
        WorkflowCommand::Cancel { run } => driver::cancel_run(paths, &run, json_output),
        WorkflowCommand::Init { dir, force } => init::write_sdk(dir.as_deref(), force, json_output),
        WorkflowCommand::Drive { run, script } => driver::drive(paths, &run, &script),
    }
}

fn create(
    paths: &Paths,
    max_parallel: u32,
    script_digest: Option<String>,
    json_output: bool,
) -> Result<()> {
    let script_digest = script_digest.unwrap_or_else(|| "none".to_owned());
    let run = create_owned_run(paths, max_parallel, &script_digest)?;
    print_run_receipt(&run, json_output);
    Ok(())
}

fn create_owned_run(paths: &Paths, max_parallel: u32, script_digest: &str) -> Result<WorkflowRun> {
    let owner = owner_from_environment()?;
    let store = Store::open(&paths.store)?;
    let session = current_session()?;
    bind_owner_session(&store, &owner, session.as_deref())?;
    let run_id = new_run_id()?;
    Ok(store.create_workflow_run(&run_id, &owner, script_digest, max_parallel)?)
}

fn status(paths: &Paths, run: &str, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let run_id = WorkflowRunId::new(run)?;
    let run = store.workflow_run(&run_id)?;
    require_owner(&store, &run.owner_id)?;
    let steps = store
        .workflow_steps(&run_id)?
        .into_iter()
        .map(|step| {
            let state = task_commands::attempt_state_name(&store, step.task_id)?;
            Ok(json!({
                "step_key": step.step_key.to_string(),
                "task_id": step.task_id,
                "revision": step.revision,
                "state": state,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let notices = store
        .workflow_notifications(&run_id)?
        .into_iter()
        .map(|notice| {
            json!({
                "seq": notice.seq,
                "state": notice.state.as_str(),
                "delivered": notice.delivered_session.is_some(),
            })
        })
        .collect::<Vec<_>>();
    print_value(
        &json!({
            "run_id": run.run_id.to_string(),
            "owner": run.owner_id.as_str(),
            "state": run.state.as_str(),
            "max_parallel": run.max_parallel,
            "script_digest": run.script_digest,
            "steps": steps,
            "notices": notices,
        }),
        json_output,
    );
    Ok(())
}

fn set_state(paths: &Paths, run: &str, state: &str, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let run_id = WorkflowRunId::new(run)?;
    let current = store.workflow_run(&run_id)?;
    require_owner(&store, &current.owner_id)?;
    let state = parse_run_state(state)?;
    let previous = current.state;
    let run = store.set_workflow_run_state(&run_id, state)?;
    crate::notification::spawn_for_workflow_transition(paths, previous, &run);
    print_run_receipt(&run, json_output);
    Ok(())
}

fn print_run_receipt(run: &WorkflowRun, json_output: bool) {
    print_value(
        &json!({
            "run_id": run.run_id.to_string(),
            "owner": run.owner_id.as_str(),
            "state": run.state.as_str(),
            "max_parallel": run.max_parallel,
        }),
        json_output,
    );
}

fn new_run_id() -> Result<WorkflowRunId> {
    let uuid = Uuid::new_v4();
    let mut hex = String::with_capacity(12);
    for byte in &uuid.as_bytes()[..6] {
        write!(&mut hex, "{byte:02x}")?;
    }
    Ok(WorkflowRunId::new(format!("wf-{hex}"))?)
}

fn parse_run_state(value: &str) -> Result<WorkflowRunState> {
    match value {
        "running" => Ok(WorkflowRunState::Running),
        "blocked" => Ok(WorkflowRunState::Blocked),
        "completed" => Ok(WorkflowRunState::Completed),
        "failed" => Ok(WorkflowRunState::Failed),
        "cancelled" => Ok(WorkflowRunState::Cancelled),
        other => bail!("unknown workflow state {other}"),
    }
}

pub(crate) fn diverged_error(step_key: &WorkflowStepKey) -> anyhow::Error {
    anyhow::anyhow!(
        "workflow step {step_key} diverged: the input does not match the journaled step"
    )
}

/// SYSEXITS.H `EX_TEMPFAIL`: a retryable refusal, not a failed run.
pub(crate) const EX_TEMPFAIL: i32 = 75;

pub(crate) fn exit_parallel_limit(run_id: &WorkflowRunId) -> ! {
    eprintln!("workflow run {run_id} is at its parallel limit; retry later");
    std::process::exit(EX_TEMPFAIL);
}
