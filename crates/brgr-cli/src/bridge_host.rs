//! Host-side checks for commands that arrive through the Herdr plugin bridge.

use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

use crate::{
    cli::{ArtifactCommand, Cli, Command, HarnessCommand},
    message, plugin_bridge,
};

pub(crate) fn bridge_budget_seconds(command: &Command) -> u64 {
    match command {
        Command::Run(args) if args.foreground => args.deadline_seconds,
        Command::Revise(args) if args.foreground => {
            plugin_bridge::MAX_BRIDGE_SECONDS.saturating_sub(120)
        }
        Command::Wait {
            timeout_seconds, ..
        }
        | Command::Message {
            command:
                message::MessageCommand::Wait {
                    timeout_seconds, ..
                },
        } => *timeout_seconds,
        _ => 3_600,
    }
}

pub(crate) fn validate_bridge_host_preflight(cli: &Cli) -> Result<()> {
    let host_home = env::var_os(plugin_bridge::BRIDGE_HOST_HOME_ENV);
    let host_workspace = env::var_os(plugin_bridge::BRIDGE_HOST_WORKSPACE_ENV);
    match (host_home, host_workspace) {
        (None, None) => Ok(()),
        (Some(home), Some(workspace)) => {
            let home = PathBuf::from(home);
            require_bridge_home(cli.home.as_deref(), &home)?;
            validate_bridge_host_command(
                &cli.command,
                &PathBuf::from(workspace),
                &env::current_dir()?,
            )
        }
        _ => bail!("brgr Herdr bridge host context is incomplete"),
    }
}

pub(crate) fn require_bridge_home(requested: Option<&Path>, expected: &Path) -> Result<()> {
    if requested != Some(expected) {
        bail!("brgr Herdr bridge cannot override its control home");
    }
    Ok(())
}

pub(crate) fn validate_bridge_host_command(
    command: &Command,
    workspace_root: &Path,
    current_dir: &Path,
) -> Result<()> {
    let workspace_root = workspace_root
        .canonicalize()
        .context("brgr Herdr bridge workspace is unavailable")?;
    match command {
        Command::Run(args) => {
            require_bridge_workspace(
                args.workspace.as_deref().unwrap_or(current_dir),
                &workspace_root,
            )?;
            Ok(())
        }
        Command::Revise(args) => {
            if let Some(workspace) = args.workspace.as_deref() {
                require_bridge_workspace(workspace, &workspace_root)?;
            }
            Ok(())
        }
        Command::Status { .. }
        | Command::Result { .. }
        | Command::Diff { .. }
        | Command::Wait { .. }
        | Command::Message { .. }
        | Command::Cancel { .. }
        | Command::Bind { .. }
        | Command::Accept { .. }
        | Command::Reject { .. }
        | Command::Doctor
        | Command::Config { .. }
        | Command::Cleanup { .. }
        | Command::Harness {
            command: HarnessCommand::Status { .. },
        }
        | Command::Supervise { .. } => Ok(()),
        Command::Artifact {
            command: ArtifactCommand::Export { output, .. },
        } => require_bridge_workspace(output.parent().unwrap_or(current_dir), &workspace_root),
        Command::Apply { workspace, .. } => require_bridge_workspace(workspace, &workspace_root),
        Command::Harness { .. } | Command::Integrate { .. } | Command::Prune { .. } => bail!(
            "this brgr command is unavailable through the Herdr host bridge; run it explicitly outside the plugin Codex pane"
        ),
        Command::Plugin { .. }
        | Command::Hook { .. }
        | Command::Notify { .. }
        | Command::PaneRun(_)
        | Command::OmpRun { .. } => {
            bail!("internal brgr commands are unavailable through the Herdr host bridge")
        }
    }
}

pub(crate) fn require_bridge_workspace(candidate: &Path, workspace_root: &Path) -> Result<()> {
    let candidate = candidate
        .canonicalize()
        .context("brgr Herdr bridge task workspace is unavailable")?;
    if !candidate.starts_with(workspace_root) {
        bail!("brgr Herdr bridge task workspace is outside the selected Herdr workspace");
    }
    Ok(())
}
