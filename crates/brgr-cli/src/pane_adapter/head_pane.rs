//! Panes for a head handoff: opening the incoming head, telling a live head,
//! and closing the outgoing one.
//!
//! A head is not a task. Nothing here writes task state; the takeover record in
//! the store stays the only account of who holds command.

use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::TaskId;
use serde_json::Value;

use super::{Herdr, Notices, POLL, native, start_agent};
use crate::{Paths, cli::PaneRunArgs};

/// How long the incoming agent may take to be ready for its first prompt.
const READY: Duration = Duration::from_mins(2);

pub(crate) struct IncomingHead {
    pub(crate) pane: String,
    pub(crate) terminal: Option<String>,
}

pub(crate) struct IncomingSpec<'a> {
    pub(crate) caller: &'a str,
    pub(crate) task: TaskId,
    pub(crate) revision: u32,
    pub(crate) workspace: &'a Path,
    pub(crate) kind: &'a str,
    pub(crate) agent_args: &'a [String],
    pub(crate) env: &'a [(&'a str, String)],
    pub(crate) label: &'a str,
    pub(crate) prompt: &'a str,
}

/// Opens a pane beside the outgoing head, starts the agent in it, answers the
/// screens a rule covers, and submits the first prompt once it is ready.
pub(crate) fn open_incoming(paths: &Paths, spec: &IncomingSpec<'_>) -> Result<IncomingHead> {
    let herdr = Herdr::locate();
    let pane = herdr.open_pane(
        paths,
        spec.caller,
        spec.workspace,
        Some(spec.label),
        spec.env,
    )?;
    let run = PaneRunArgs {
        caller: Some(spec.caller.to_owned()),
        native_executable: None,
        opencode_model: None,
        prompt_file: PathBuf::new(),
        workspace: spec.workspace.to_path_buf(),
        task: spec.task,
        revision: spec.revision,
        kind: spec.kind.to_owned(),
        agent_args: spec.agent_args.to_vec(),
        keep_pane: true,
    };
    let name = format!("head-{}", &spec.task.to_string()[..8]);
    let mut notices = Notices {
        paths,
        task: spec.task,
        outstanding: Vec::new(),
    };
    let ready = start_agent(&herdr, &name, &pane, &run, &mut notices).and_then(|()| {
        let waited = Instant::now();
        while !native::native_ready(&herdr, &pane, spec.kind, false) {
            if waited.elapsed() > READY {
                bail!(
                    "the incoming {} agent never became ready for a prompt",
                    spec.kind
                );
            }
            thread::sleep(POLL);
        }
        herdr
            .call(&["agent", "prompt", &name, spec.prompt])
            .map_err(|failure| anyhow::anyhow!("could not prompt the incoming head: {failure}"))?;
        Ok(())
    });
    if let Err(error) = ready {
        // A pane that never took its prompt holds nothing; do not leave it.
        let _ = herdr.call(&["pane", "close", &pane]);
        return Err(error);
    }
    let terminal = herdr.call(&["pane", "get", &pane]).ok().and_then(|value| {
        value
            .pointer("/result/pane/terminal_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    Ok(IncomingHead { pane, terminal })
}

/// The terminal Herdr reports for a pane, recorded so a later close can tell
/// the pane from another that reused its id.
pub(crate) fn terminal_of(pane: &str) -> Option<String> {
    Herdr::locate()
        .call(&["pane", "get", pane])
        .ok()
        .and_then(|value| {
            value
                .pointer("/result/pane/terminal_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

/// Tells a live head something, only while its agent is idle so the text does
/// not land in the middle of its turn. Returns whether it was delivered.
pub(crate) fn tell_head(pane: &str, text: &str) -> bool {
    let herdr = Herdr::locate();
    let idle = herdr
        .call(&["agent", "get", pane])
        .ok()
        .and_then(|value| {
            value
                .pointer("/result/agent/agent_status")
                .and_then(Value::as_str)
                .map(|status| matches!(status, "idle" | "done"))
        })
        .unwrap_or(false);
    idle && herdr.call(&["agent", "prompt", pane, text]).is_ok()
}

/// Closes the outgoing head's pane after checking it is still the pane that was
/// recorded and is not working. A refusal says why and closes nothing.
pub(crate) fn close_outgoing(pane: &str, terminal: Option<&str>, keep: &str) -> Result<()> {
    if pane == keep {
        bail!("the outgoing pane is the incoming head's own pane");
    }
    let herdr = Herdr::locate();
    let current = herdr
        .call(&["pane", "get", pane])
        .map_err(|failure| anyhow::anyhow!("the outgoing pane is gone: {failure}"))?;
    let seen = current
        .pointer("/result/pane/terminal_id")
        .and_then(Value::as_str);
    if terminal.is_some() && seen != terminal {
        bail!("pane {pane} is no longer the outgoing head's terminal");
    }
    let status = herdr
        .status(pane)
        .context("the outgoing agent state is unknown")?;
    if !matches!(status.as_str(), "idle" | "done") {
        bail!("the outgoing head is {status}, not idle; it was not closed");
    }
    herdr
        .call(&["pane", "close", pane])
        .map_err(|failure| anyhow::anyhow!("Herdr could not close the outgoing pane: {failure}"))?;
    Ok(())
}
