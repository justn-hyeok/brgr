//! The command-line surface: every subcommand and its arguments.

use std::{collections::BTreeSet, path::PathBuf};

use crate::{config, message};
use anyhow::{Result, bail};
use brgr_protocol::{AttemptId, EvidenceSpec, TaskId, TaskSpec};
use clap::{Args, Parser, Subcommand};
use config::WorkerPlacement;

#[derive(Parser)]
#[command(name = "brgr", version, about = "Durable local agent task bridge")]
pub(crate) struct Cli {
    /// Control home holding the private registry, store, and worktrees.
    #[arg(long, global = true, env = "BRGR_HOME")]
    pub(crate) home: Option<PathBuf>,
    /// Emit one machine-readable JSON receipt instead of human output.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Start a bounded fresh run and seal its result for owner review.
    Run(RunArgs),
    /// Start a new revision of a task without rewriting its prior results.
    Revise(ReviseArgs),
    /// Show managed tasks, or one task's attempt and result state.
    Status {
        task: Option<TaskId>,
        /// Include the delegation tree beneath the task.
        #[arg(long)]
        tree: bool,
    },
    /// Show a task's sealed result, optionally acknowledging a failed one.
    Result {
        task: TaskId,
        #[arg(long)]
        ack: bool,
    },
    /// Export a sealed result artifact to a file.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    /// Block until a task reaches a terminal result.
    Wait {
        task: TaskId,
        #[arg(long, default_value_t = 3_600)]
        timeout_seconds: u64,
    },
    /// Send or list messages between an owner and a managed task.
    Message {
        #[command(subcommand)]
        command: message::MessageCommand,
    },
    /// Stop an owned in-flight run and settle it as a terminal result.
    Cancel {
        task: TaskId,
        /// Also cancel the delegated children beneath the task.
        #[arg(long)]
        tree: bool,
    },
    /// Claim a task for the current owner session before reading or deciding.
    Bind {
        task: TaskId,
        #[arg(long)]
        session: Option<String>,
    },
    /// Record an explicit acceptance of a task's sealed candidate result.
    Accept {
        task: TaskId,
        #[arg(long, default_value = "acceptance criteria verified")]
        reason: String,
    },
    /// Record an explicit rejection with a reason, keeping the result intact.
    Reject {
        task: TaskId,
        #[arg(long)]
        reason: String,
    },
    /// Integrate a candidate's sealed Git diff into a matching workspace.
    Apply {
        task: TaskId,
        /// Repository root to integrate into; must match the task's base.
        #[arg(long)]
        workspace: PathBuf,
        /// Write the diff instead of only checking that it would apply.
        #[arg(long)]
        execute: bool,
    },
    /// Register, contract-test, activate, and inspect local harnesses.
    Harness {
        #[command(subcommand)]
        command: HarnessCommand,
    },
    /// Install, inspect, or remove brgr-owned Codex integration entries.
    Integrate {
        #[command(subcommand)]
        command: IntegrateCommand,
    },
    /// Probe every registered harness, the Codex integration, and the store.
    Doctor,
    /// Show or change local brgr configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Herdr plugin entrypoints; these require the brgr plugin host.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Report, and with --apply remove, settled task worktrees and branches.
    Prune {
        /// Remove instead of only reporting. Every check still applies.
        #[arg(long)]
        apply: bool,
        /// Also remove a worktree holding ignored files such as `.env` or a
        /// build cache. Those are invisible to git's own clean check.
        #[arg(long)]
        include_ignored: bool,
    },
    /// Inspect or retry the close of a pane brgr recorded as its own.
    Cleanup {
        #[command(subcommand)]
        command: CleanupCommand,
    },
    #[command(name = "__supervise", hide = true)]
    Supervise { launch: PathBuf },
    #[command(name = "__hook", hide = true)]
    Hook { event: HookEvent },
    #[command(name = "__notify", hide = true)]
    Notify { task: TaskId },
    #[command(name = "__omp-run", hide = true)]
    OmpRun {
        #[arg(long)]
        prompt_file: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        task: TaskId,
        #[arg(long)]
        revision: u32,
        #[arg(long)]
        launcher: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        keep_pane: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum PluginCommand {
    /// Open the read-only task board, or a worktree-bound Codex pane.
    Open {
        #[arg(long)]
        no_focus: bool,
        #[arg(long)]
        codex: bool,
    },
    /// Render the read-only task board, refreshing until interrupted.
    Board {
        #[arg(long)]
        once: bool,
    },
    /// Launch a Codex pane wired to the plugin's binary and host bridge.
    Codex,
    #[command(hide = true)]
    Worker,
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    Show,
    SetCodexExecutable {
        executable: PathBuf,
    },
    ClearCodexExecutable,
    SetWorkerPlacement {
        placement: WorkerPlacement,
    },
    SetAutoWorkerPane {
        #[arg(action = clap::ArgAction::Set)]
        enabled: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ArtifactCommand {
    Export {
        task: TaskId,
        index: usize,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Args)]
pub(crate) struct RunArgs {
    pub(crate) objective: String,
    #[command(flatten)]
    pub(crate) delegation: DelegationArgs,
    #[arg(long, default_value = "local.gjc")]
    pub(crate) harness: String,
    #[arg(long)]
    pub(crate) model: Option<String>,
    #[arg(long)]
    pub(crate) effort: Option<String>,
    #[arg(long = "criterion")]
    pub(crate) criteria: Vec<String>,
    #[arg(long = "scope")]
    pub(crate) scopes: Vec<String>,
    #[arg(long = "role-instruction")]
    pub(crate) role_instructions: Vec<String>,
    #[command(flatten)]
    pub(crate) capabilities: CapabilityArgs,
    #[command(flatten)]
    pub(crate) evidence: EvidenceArgs,
    #[arg(long, default_value_t = 3_600)]
    pub(crate) deadline_seconds: u64,
    #[arg(long)]
    pub(crate) max_children: Option<u8>,
    #[arg(long)]
    pub(crate) workspace: Option<PathBuf>,
    #[arg(long)]
    pub(crate) allow_clean_head_snapshot: bool,
    #[arg(long = "snapshot-path", conflicts_with = "allow_clean_head_snapshot")]
    pub(crate) snapshot_paths: Vec<PathBuf>,
    #[arg(long, hide = true)]
    pub(crate) foreground: bool,
    #[arg(long)]
    pub(crate) keep_pane: bool,
}

impl RunArgs {
    pub(crate) fn forwards_criteria(&self) -> bool {
        !self.criteria.is_empty() || !self.scopes.is_empty() || !self.role_instructions.is_empty()
    }
}

#[derive(Args)]
pub(crate) struct DelegationArgs {
    #[arg(long, requires = "parent_attempt")]
    pub(crate) parent_task: Option<TaskId>,
    #[arg(long, requires = "parent_task")]
    pub(crate) parent_attempt: Option<AttemptId>,
    #[arg(long)]
    pub(crate) enable_delegation: bool,
}

#[derive(Args)]
pub(crate) struct ReviseArgs {
    pub(crate) task: TaskId,
    pub(crate) objective: String,
    #[arg(long = "criterion")]
    pub(crate) criteria: Vec<String>,
    #[arg(long = "scope")]
    pub(crate) scopes: Vec<String>,
    #[arg(long = "role-instruction")]
    pub(crate) role_instructions: Vec<String>,
    #[command(flatten)]
    pub(crate) capabilities: CapabilityArgs,
    #[command(flatten)]
    pub(crate) evidence: EvidenceArgs,
    #[arg(long)]
    pub(crate) max_children: Option<u8>,
    #[arg(long)]
    pub(crate) workspace: Option<PathBuf>,
    #[arg(long)]
    pub(crate) allow_clean_head_snapshot: bool,
    #[arg(long = "snapshot-path", conflicts_with = "allow_clean_head_snapshot")]
    pub(crate) snapshot_paths: Vec<PathBuf>,
    #[arg(long, hide = true)]
    pub(crate) foreground: bool,
    #[arg(long)]
    pub(crate) keep_pane: bool,
}

impl ReviseArgs {
    pub(crate) fn forwards_criteria(&self) -> bool {
        !self.criteria.is_empty() || !self.scopes.is_empty() || !self.role_instructions.is_empty()
    }
}

#[derive(Args, Default)]
pub(crate) struct CapabilityArgs {
    #[arg(long)]
    pub(crate) requires_write: bool,
    #[arg(long)]
    pub(crate) requires_browser: bool,
    #[arg(long = "requires-mcp")]
    pub(crate) requires_mcp: Vec<String>,
    #[arg(long = "require-capability")]
    pub(crate) required: Vec<String>,
}

impl CapabilityArgs {
    pub(crate) fn required_names(&self) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        names.insert("completion".to_owned());
        if self.requires_write {
            names.insert("workspace_write".to_owned());
        }
        if self.requires_browser {
            names.insert("browser".to_owned());
        }
        for server in &self.requires_mcp {
            names.insert(format!("mcp:{server}"));
        }
        names.extend(self.required.iter().cloned());
        if names.iter().any(|name| {
            name.is_empty()
                || name.len() > 128
                || !name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-')
                })
        }) {
            bail!("required capability names must be bounded ASCII identifiers");
        }
        Ok(names.into_iter().collect())
    }
}

#[derive(Args, Default)]
pub(crate) struct EvidenceArgs {
    #[arg(long)]
    pub(crate) capture_diff: bool,
    #[arg(long)]
    pub(crate) capture_logs: bool,
    #[arg(long = "evidence-file")]
    pub(crate) files: Vec<PathBuf>,
}

impl EvidenceArgs {
    pub(crate) fn spec(&self) -> EvidenceSpec {
        EvidenceSpec {
            capture_diff: self.capture_diff,
            capture_logs: self.capture_logs,
            base_commit: None,
            base_tree: None,
            files: self
                .files
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        }
    }

    pub(crate) fn extend_task(&self, task: &mut TaskSpec) {
        task.evidence.capture_diff |= self.capture_diff;
        task.evidence.capture_logs |= self.capture_logs;
        task.evidence.files.extend(self.spec().files);
        task.evidence.files.sort();
        task.evidence.files.dedup();
        if !task.evidence.is_empty() {
            task.artifact_contract.max_bytes =
                task.artifact_contract.max_bytes.max(8 * 1024 * 1024);
        }
    }

    pub(crate) fn artifact_limit(&self, base: u64) -> u64 {
        if self.spec().is_empty() {
            base
        } else {
            base.max(8 * 1024 * 1024)
        }
    }
}

#[derive(Clone, Copy, Subcommand)]
pub(crate) enum CleanupCommand {
    /// Report whether an owned pane was closed or deliberately retained.
    Status { task: TaskId },
    /// Retry a pending close of a pane brgr recorded as its own.
    Run { task: TaskId },
}

#[derive(Subcommand)]
pub(crate) enum HarnessCommand {
    /// Probe, contract-test, scratch-run, and activate one approved executable.
    Add(AddHarnessArgs),
    /// Print a candidate manifest for an executable without registering it.
    Draft { executable: PathBuf },
    /// Run the manifest contract test without starting a paid scratch run.
    Test {
        executable: Option<PathBuf>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Activate a contract-tested manifest with an authorized scratch run.
    Activate {
        executable: Option<PathBuf>,
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
    /// Report whether one registered harness is still healthy.
    Status {
        #[arg(default_value = "local.gjc")]
        harness: String,
    },
}

#[derive(Args)]
pub(crate) struct AddHarnessArgs {
    pub(crate) executable: PathBuf,
    #[arg(long)]
    pub(crate) workspace: Option<PathBuf>,
    #[arg(long)]
    pub(crate) prompt: Option<String>,
    #[arg(long)]
    pub(crate) model: Option<String>,
    #[arg(long)]
    pub(crate) effort: Option<String>,
    #[arg(long)]
    pub(crate) presentation_only: bool,
}

#[derive(Subcommand)]
pub(crate) enum IntegrateCommand {
    /// Manage the brgr-owned Codex hooks and skill.
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
}

#[derive(Subcommand)]
pub(crate) enum CodexCommand {
    Install,
    Status,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    Stop,
}
