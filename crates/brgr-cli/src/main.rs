mod admission;
#[cfg(test)]
mod adversary;
mod bridge_host;
mod caller_pane;
mod cli;
mod codex_integration;
mod config;
mod debate;
mod error_memo;
mod evidence;
mod failure_banner;
mod harness_commands;
mod herdr_plugin;
mod hook;
mod invocation;
mod message;
mod native_result;
mod notification;
mod omp_adapter;
mod pane_adapter;
mod pane_cleanup;
mod plugin_bridge;
mod supervision;
mod task_commands;
mod tree_status;
mod tui_host;
mod workspace;
mod worktree_prune;

use std::{
    env,
    fs::{self},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use admission::{revise_task, run_task};
use anyhow::{Context, Result, bail};
use brgr_protocol::{AttemptId, DecisionVerdict, OwnerId, TaskId, TaskSpec};
use brgr_runner::HarnessManifest;
use brgr_store::{RunnerIdentity, Store};
use clap::Parser;
use cli::{ArtifactCommand, Cli, Command, ConfigCommand, PluginCommand};
use config::Config;
use harness_commands::{cleanup, doctor, harness, integrate};
use omp_adapter::{OmpOptions, run_omp_adapter};
use serde::{Deserialize, Serialize};
use supervision::supervise;
use task_commands::{bind, cancel, decide, result, status, wait_for_result};
use tempfile::NamedTempFile;

#[derive(Clone, Debug)]
struct Paths {
    home: PathBuf,
    config: PathBuf,
    store: PathBuf,
    registry: PathBuf,
    launches: PathBuf,
    runs: PathBuf,
    worktrees: PathBuf,
}

impl Paths {
    fn new(home: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(path) = home {
            path
        } else {
            let user_home = env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(user_home)
                .join("Library")
                .join("Application Support")
                .join("brgr")
        };
        fs::create_dir_all(&home)?;
        let home = home.canonicalize()?;
        let paths = Self {
            config: home.join("config.toml"),
            store: home.join("store"),
            registry: home.join("registry"),
            launches: home.join("launches"),
            runs: home.join("runs"),
            worktrees: home.join("worktrees"),
            home,
        };
        for path in [&paths.home, &paths.launches, &paths.runs, &paths.worktrees] {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(paths)
    }

    fn cancel(&self, task: TaskId) -> PathBuf {
        self.runs.join(format!("{task}.cancel"))
    }

    fn pid(&self, task: TaskId) -> PathBuf {
        self.runs.join(format!("{task}.pid"))
    }

    /// The Herdr pane a pane-mode run opened, while that run owns it.
    fn pane_receipt(&self, task: TaskId, revision: u32) -> PathBuf {
        self.runs.join(format!("{task}-r{revision}.pane.json"))
    }

    fn supervisor(&self, task: TaskId) -> PathBuf {
        self.runs.join(format!("{task}.supervisor.json"))
    }

    fn launch(&self, task: TaskId, revision: u32) -> PathBuf {
        if revision == 1 {
            self.launches.join(format!("{task}.json"))
        } else {
            self.launches.join(format!("{task}-r{revision}.json"))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LaunchEnvelope {
    spec: TaskSpec,
    harness_id: String,
    protocol_generation: String,
    keep_pane: bool,
    #[serde(default)]
    delegation_enabled: bool,
    #[serde(default)]
    manifest: Option<HarnessManifest>,
    #[serde(default)]
    executable_digest: Option<String>,
    /// Run the harness as its own TUI in a Herdr pane beside the caller.
    #[serde(default)]
    pane_mode: bool,
    #[serde(default)]
    claimant: Claimant,
    #[serde(default)]
    source_pane: Option<String>,
    #[serde(default)]
    source_session: Option<String>,
    #[serde(default)]
    calling_options: Option<config::CallingOptions>,
    #[serde(default)]
    instructions_digest: Option<String>,
}

/// What claims an admitted task by starting its supervisor.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Claimant {
    /// A detached `brgr __supervise` process, spawned at admission.
    #[default]
    Supervisor,
    /// `brgr plugin worker` in a Herdr pane, which Herdr has to open first.
    WorkerPane,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProcessReceipt {
    task_id: TaskId,
    launch_path: PathBuf,
    identity: RunnerIdentity,
}

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: Option<String>,
}

fn calling_cli() -> Cli {
    let cli = Cli::parse();
    invocation::initialize(
        cli.owner_session.clone(),
        cli.source_pane.clone(),
        cli.headless,
    );
    cli
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("output path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = calling_cli();
    if !matches!(
        &cli.command,
        Command::Plugin { .. }
            | Command::Supervise { .. }
            | Command::Hook { .. }
            | Command::Notify { .. }
            | Command::TuiHost { .. }
            | Command::Session { .. }
            | Command::SealReport { .. }
            | Command::NativeResult { .. }
    ) && let Some(dir) = env::var_os(plugin_bridge::BRIDGE_DIR_ENV)
    {
        let budget_seconds = bridge_host::bridge_budget_seconds(&cli.command);
        return plugin_bridge::client(Path::new(&dir), budget_seconds.saturating_add(120)).await;
    }
    bridge_host::validate_bridge_host_preflight(&cli)?;
    let paths = Paths::new(cli.home.clone())?;
    let internal = matches!(
        &cli.command,
        Command::Plugin { .. }
            | Command::Supervise { .. }
            | Command::Hook { .. }
            | Command::Notify { .. }
            | Command::PaneRun(_)
            | Command::TuiHost { .. }
            | Command::Session { .. }
            | Command::SealReport { .. }
            | Command::NativeResult { .. }
            | Command::OmpRun { .. }
    );
    let outcome = Box::pin(execute(&paths, cli.command, cli.json)).await;
    if !internal {
        failure_banner::print(&paths);
    }
    outcome
}

async fn plugin_command(paths: &Paths, command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Open { no_focus, codex } => herdr_plugin::open(no_focus, codex).await,
        PluginCommand::Board { once } => herdr_plugin::board(paths, once).await,
        PluginCommand::Codex => herdr_plugin::codex(paths).await,
        PluginCommand::Worker => herdr_plugin::worker(paths).await,
    }
}

async fn execute(paths: &Paths, command: Command, json_output: bool) -> Result<()> {
    match command {
        Command::Run(args) => run_task(paths, args, json_output).await,
        Command::Revise(args) => revise_task(paths, args, json_output).await,
        Command::Status { task, tree } => status(paths, task, tree, json_output),
        Command::Result { task, ack } => result(paths, task, ack, json_output),
        Command::Artifact { command } => evidence::artifact_command(paths, command, json_output),
        Command::Wait {
            task,
            timeout_seconds,
        } => wait_for_result(paths, task, timeout_seconds, json_output).await,
        Command::Message { command } => message::run(paths, command, json_output).await,
        Command::Debate { command } => debate::run(paths, command, json_output).await,
        Command::Input { task, text, key } => pane_adapter::send_native_input(
            paths,
            task,
            text.as_deref(),
            key.as_deref(),
            json_output,
        ),
        Command::Cancel { task, tree } => cancel(paths, task, tree, json_output),
        Command::Bind { task, session } => bind(paths, task, session, json_output).await,
        Command::Accept { task, reason } => {
            decide(paths, task, DecisionVerdict::Accepted, reason, json_output)
        }
        Command::Reject { task, reason } => {
            decide(paths, task, DecisionVerdict::Rejected, reason, json_output)
        }
        Command::Diff { task, stat } => evidence::show_diff(paths, task, stat, json_output),
        Command::Apply {
            task,
            workspace,
            execute,
        } => evidence::apply_result(paths, task, &workspace, execute, json_output),
        Command::Harness { command } => harness(paths, command, json_output).await,
        Command::Integrate { command } => integrate(paths, command, json_output),
        Command::Doctor => doctor(paths, json_output).await,
        Command::Errors { command } => error_memo::run(paths, command.as_ref()),
        Command::Config { command } => config_command(paths, &command, json_output).await,
        Command::Prune {
            apply,
            include_ignored,
        } => worktree_prune::command(paths, apply, include_ignored, json_output),
        Command::Plugin { command } => plugin_command(paths, command).await,
        Command::Cleanup { command } => cleanup(paths, command, json_output),
        Command::Supervise { launch } => supervise(paths, &launch, json_output).await,
        Command::Hook { event } => {
            if hook::hook(paths, event).await.is_err() {
                eprintln!("brgr hook could not read the inbox; run `brgr doctor`");
                println!("{{}}");
            }
            Ok(())
        }
        Command::Notify { task } => notification::deliver_pending(paths, task).await,
        Command::PaneRun(args) => pane_adapter::run_pane_adapter(paths, &args),
        Command::TuiHost { config } => tui_host::run(&config),
        Command::Session { task } => pane_adapter::serve_session(paths, task),
        Command::SealReport { task, revision } => {
            pane_adapter::seal_native_report(paths, task, revision)
        }
        Command::Report { task, body } => native_result::submit(paths, task, &body),
        Command::NativeResult {
            task,
            revision,
            kind,
        } => {
            if native_result::hook(paths, task, revision, &kind).is_err() {
                eprintln!("brgr native result remains pending");
            }
            println!("{{}}");
            Ok(())
        }
        Command::OmpRun {
            prompt_file,
            workspace,
            task,
            revision,
            launcher,
            model,
            effort,
            keep_pane,
        } => run_omp_adapter(
            paths,
            &prompt_file,
            &workspace,
            task,
            &launcher,
            OmpOptions {
                revision,
                model: model.as_deref(),
                effort: effort.as_deref(),
                keep_pane,
            },
        ),
    }
}

async fn config_command(paths: &Paths, command: &ConfigCommand, json_output: bool) -> Result<()> {
    let mut config = Config::load(&paths.config)?;
    match command {
        ConfigCommand::Init => return config::initialize(paths, json_output),
        ConfigCommand::Set {
            key,
            value,
            harness,
        } => {
            config.set(key, value, harness.as_deref())?;
            config.save(&paths.config)?;
        }
        ConfigCommand::Check => return config::check(paths, &config, json_output).await,
        ConfigCommand::Show => {}
        ConfigCommand::SetCodexExecutable { executable } => {
            config::validate_codex_executable(executable)?;
            config.herdr.codex_executable = Some(executable.clone());
            config.save(&paths.config)?;
        }
        ConfigCommand::ClearCodexExecutable => {
            config.herdr.codex_executable = None;
            config.save(&paths.config)?;
        }
        ConfigCommand::SetWorkerPlacement { placement } => {
            config.herdr.worker_placement = *placement;
            config.save(&paths.config)?;
        }
        ConfigCommand::SetAutoWorkerPane { enabled } => {
            config.herdr.auto_worker_pane = *enabled;
            config.save(&paths.config)?;
        }
        ConfigCommand::SetPaneMode { enabled } => {
            config.herdr.prefer_print_mode = !*enabled;
            config.save(&paths.config)?;
        }
        ConfigCommand::SetMaxPermission { level } => {
            config.worker.max_permission = Some((*level).into());
            config.save(&paths.config)?;
        }
        ConfigCommand::ClearMaxPermission => {
            config.worker.max_permission = None;
            config.save(&paths.config)?;
        }
        ConfigCommand::SetIssueReporting { repo, min_count } => {
            if repo.split('/').count() != 2 || repo.contains(char::is_whitespace) {
                bail!("repository must be OWNER/NAME");
            }
            if *min_count == 0 {
                bail!("--min-count must be at least 1");
            }
            config.issues.auto_file = true;
            config.issues.repo = Some(repo.clone());
            config.issues.min_count = *min_count;
            config.save(&paths.config)?;
        }
        ConfigCommand::ClearIssueReporting => {
            config.issues.auto_file = false;
            config.save(&paths.config)?;
        }
    }
    if json_output {
        print_value(&serde_json::to_value(&config)?, true);
    } else {
        print!("{}", toml::to_string_pretty(&config)?);
    }
    Ok(())
}

fn owner_from_environment() -> Result<OwnerId> {
    let session = current_session()?;
    let owner = env::var("BRGR_OWNER_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| session.map(|id| format!("{}:{id}", owner_kind())))
        .unwrap_or_else(|| "codex:manual".to_owned());
    Ok(OwnerId::new(owner)?)
}

fn delegation_parent_from_environment() -> Result<Option<(TaskId, AttemptId)>> {
    match (
        env::var("BRGR_PARENT_TASK_ID").ok(),
        env::var("BRGR_PARENT_ATTEMPT_ID").ok(),
    ) {
        (None, None) => Ok(None),
        (Some(task), Some(attempt)) => Ok(Some((
            task.parse().context("BRGR_PARENT_TASK_ID is invalid")?,
            attempt
                .parse()
                .context("BRGR_PARENT_ATTEMPT_ID is invalid")?,
        ))),
        _ => bail!("brgr worker parent identity is incomplete"),
    }
}

/// Which kind of harness owns this shell. A Codex or explicit brgr session keeps
/// the `codex` owner prefix; a Claude Code session, which has neither, is a
/// `claude` owner.
fn owner_kind() -> &'static str {
    let set = |name: &str| env::var(name).is_ok_and(|value| !value.trim().is_empty());
    if !set("CODEX_THREAD_ID") && !set("BRGR_SESSION_ID") && set("CLAUDE_CODE_SESSION_ID") {
        "claude"
    } else {
        "codex"
    }
}

fn current_session() -> Result<Option<String>> {
    if let Some(session) = &invocation::current().session {
        if session.trim().is_empty() {
            bail!("owner session must not be empty");
        }
        return Ok(Some(session.clone()));
    }
    let codex = env::var("CODEX_THREAD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let explicit = env::var("BRGR_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if codex
        .as_ref()
        .zip(explicit.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        bail!("CODEX_THREAD_ID and BRGR_SESSION_ID disagree");
    }
    let claude = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    Ok(codex.or(explicit).or(claude))
}

fn require_owner(store: &Store, expected: &OwnerId) -> Result<(String, u64)> {
    if let Ok(explicit_owner) = env::var("BRGR_OWNER_ID")
        && explicit_owner != expected.as_str()
    {
        bail!("task belongs to {expected}; BRGR_OWNER_ID differs");
    }
    let session = current_session()?.context("a Codex session is required; use `brgr bind TASK --session SESSION` to claim an unbound task")?;
    let (bound, epoch) = store.owner_binding(expected)?.with_context(|| {
        format!("owner {expected} is unbound; run `brgr bind TASK --session {session}`")
    })?;
    if bound != session {
        bail!(
            "owner {expected} is bound to another session; use `brgr bind TASK --session {session}` to transfer it"
        );
    }
    Ok((session, epoch))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("launch path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("launch path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(())
}

fn print_value(value: &serde_json::Value, json_output: bool) {
    if json_output {
        println!("{value}");
    } else if let Ok(pretty) = serde_json::to_string_pretty(value) {
        println!("{pretty}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_host::{require_bridge_home, validate_bridge_host_command};
    use crate::omp_adapter::{
        fresh_omp_report_path, omp_completion_ready, omp_spawn_matches_initial,
        read_bounded_regular_report,
    };
    use crate::supervision::pinned_manifest_for_launch;
    use serde_json::json;
    use tempfile::TempDir;

    /// `--model` and `--permission` are options of `run` and `revise`. Adding
    /// `--permission` once slipped between `model`'s `#[arg(long)]` and its
    /// field, which turned `--model` into a positional and broke every run that
    /// named a model; only a contract test several layers away noticed.
    #[test]
    fn run_and_revise_take_model_and_permission_as_options() {
        let run = Cli::try_parse_from([
            "brgr",
            "run",
            "objective",
            "--model",
            "m",
            "--effort",
            "high",
            "--permission",
            "edits",
        ])
        .unwrap();
        let Command::Run(args) = run.command else {
            panic!("not a run");
        };
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.permission, Some(crate::cli::PermissionArg::Edits));

        let task = TaskId::new().to_string();
        let revise = Cli::try_parse_from([
            "brgr",
            "revise",
            task.as_str(),
            "objective",
            "--permission",
            "read-only",
        ])
        .unwrap();
        let Command::Revise(args) = revise.command else {
            panic!("not a revise");
        };
        assert_eq!(args.permission, Some(crate::cli::PermissionArg::ReadOnly));
    }

    /// A requested level is kept unless it exceeds a bound, which is an error;
    /// no request takes the tightest bound, and `full` is no bound at all.
    #[test]
    fn a_permission_is_bounded_by_the_cap_and_the_parent() {
        use crate::admission::bound_permission;
        use brgr_protocol::PermissionLevel::{Edits, Full, ReadOnly};

        assert_eq!(bound_permission(None, None, None).unwrap(), None);
        assert_eq!(
            bound_permission(Some(Edits), None, None).unwrap(),
            Some(Edits)
        );
        // A cap of full locks nothing: the task runs as it always has.
        assert_eq!(bound_permission(None, Some(Full), None).unwrap(), None);
        // A cap applies to a task that asked for nothing.
        assert_eq!(
            bound_permission(None, Some(Edits), None).unwrap(),
            Some(Edits)
        );
        // The tighter of cap and parent wins.
        assert_eq!(
            bound_permission(None, Some(Edits), Some(ReadOnly)).unwrap(),
            Some(ReadOnly)
        );
        // Asking under a bound is fine; over it is refused, never clamped.
        assert_eq!(
            bound_permission(Some(ReadOnly), Some(Edits), None).unwrap(),
            Some(ReadOnly)
        );
        assert!(bound_permission(Some(Full), Some(Edits), None).is_err());
        assert!(bound_permission(Some(Edits), None, Some(ReadOnly)).is_err());
    }
    #[test]
    fn bridge_host_allows_managed_operations_and_rejects_privileged_changes() {
        let root = TempDir::new().unwrap();
        let inside = root.path().join("project");
        let outside = TempDir::new().unwrap();
        fs::create_dir(&inside).unwrap();

        let run = Cli::try_parse_from([
            "brgr",
            "run",
            "check",
            "--workspace",
            inside.to_str().unwrap(),
        ])
        .unwrap();
        validate_bridge_host_command(&run.command, root.path(), root.path()).unwrap();

        let escaped = Cli::try_parse_from([
            "brgr",
            "run",
            "check",
            "--workspace",
            outside.path().to_str().unwrap(),
        ])
        .unwrap();
        assert!(validate_bridge_host_command(&escaped.command, root.path(), root.path()).is_err());

        let harness_status =
            Cli::try_parse_from(["brgr", "harness", "status", "local.gjc"]).unwrap();
        validate_bridge_host_command(&harness_status.command, root.path(), root.path()).unwrap();

        let harness_add = Cli::try_parse_from([
            "brgr",
            "harness",
            "add",
            "/tmp/untrusted-agent",
            "--workspace",
            inside.to_str().unwrap(),
            "--prompt",
            "probe",
        ])
        .unwrap();
        assert!(
            validate_bridge_host_command(&harness_add.command, root.path(), root.path()).is_err()
        );

        let integrate = Cli::try_parse_from(["brgr", "integrate", "codex", "uninstall"]).unwrap();
        assert!(
            validate_bridge_host_command(&integrate.command, root.path(), root.path()).is_err()
        );

        // Pruning removes worktrees and branches in a repository brgr does not
        // own, so it stays outside the plugin Codex pane even in report mode.
        for argv in [vec!["brgr", "prune"], vec!["brgr", "prune", "--apply"]] {
            let prune = Cli::try_parse_from(argv).unwrap();
            assert!(
                validate_bridge_host_command(&prune.command, root.path(), root.path()).is_err()
            );
        }
    }

    #[test]
    fn bridge_host_rejects_control_home_override() {
        let expected = Path::new("/private/tmp/brgr-home");
        assert!(require_bridge_home(Some(expected), expected).is_ok());
        assert!(require_bridge_home(None, expected).is_err());
        assert!(require_bridge_home(Some(Path::new("/private/tmp/other-home")), expected).is_err());
    }

    #[test]
    fn legacy_launch_without_pinned_manifest_cannot_replay() {
        let error = pinned_manifest_for_launch(None, None, "local.gjc").unwrap_err();
        assert!(error.to_string().contains("unsafe replay is disabled"));
        let error = pinned_manifest_for_launch(None, Some("digest"), "local.gjc").unwrap_err();
        assert!(error.to_string().contains("unsafe replay is disabled"));
    }

    #[test]
    fn omp_report_import_is_bounded_and_rejects_symlinks() {
        let root = TempDir::new().unwrap();
        let report = root.path().join("report.md");
        fs::write(&report, b"sealed fixture").unwrap();
        assert_eq!(
            read_bounded_regular_report(&report, 100).unwrap(),
            b"sealed fixture"
        );
        assert!(read_bounded_regular_report(&report, 3).is_err());
        let link = root.path().join("linked.md");
        std::os::unix::fs::symlink(&report, &link).unwrap();
        assert!(read_bounded_regular_report(&link, 100).is_err());
        fs::write(&report, b"").unwrap();
        assert!(read_bounded_regular_report(&report, 100).is_err());
    }

    #[test]
    fn omp_completion_needs_a_new_lifecycle_turn_with_unchanged_identity() {
        let initial = json!({"result": {"agent": {
            "name": "brgr-fixture", "agent": "omp", "pane_id": "w1:p2",
            "terminal_id": "term-fixture",
            "agent_session": {"kind": "path", "value": "/tmp/fixture-session"},
            "state_change_seq": 7, "agent_status": "idle"
        }}});
        let mut observed = initial.clone();
        assert!(!omp_completion_ready(&initial, &observed, "brgr-fixture").unwrap());
        observed["result"]["agent"]["state_change_seq"] = json!(8);
        observed["result"]["agent"]["agent_status"] = json!("working");
        assert!(!omp_completion_ready(&initial, &observed, "brgr-fixture").unwrap());
        observed["result"]["agent"]["agent_status"] = json!("done");
        assert!(omp_completion_ready(&initial, &observed, "brgr-fixture").unwrap());
        observed["result"]["agent"]["agent_session"]["value"] = json!("/tmp/other-session");
        assert!(omp_completion_ready(&initial, &observed, "brgr-fixture").is_err());
        observed["result"]["agent"]["agent_session"]["value"] = json!("/tmp/fixture-session");
        observed["result"]["agent"]["pane_id"] = json!("w1:p3");
        assert!(omp_completion_ready(&initial, &observed, "brgr-fixture").is_err());
        observed["result"]["agent"]["pane_id"] = json!("w1:p2");
        observed["result"]["agent"]["agent_status"] = json!("blocked");
        assert!(omp_completion_ready(&initial, &observed, "brgr-fixture").is_err());
    }

    #[test]
    fn omp_report_path_is_revision_scoped_and_never_reuses_existing_bytes() {
        let root = TempDir::new().unwrap();
        let paths = Paths::new(Some(root.path().join("brgr"))).unwrap();
        let task = TaskId::new();
        let first = fresh_omp_report_path(&paths, task, 1).unwrap();
        let second = fresh_omp_report_path(&paths, task, 2).unwrap();
        assert_ne!(first, second);
        fs::write(&first, b"stale revision one").unwrap();
        assert_eq!(fresh_omp_report_path(&paths, task, 2).unwrap(), second);
        assert!(fresh_omp_report_path(&paths, task, 1).is_err());
        fs::write(&second, b"stale revision two").unwrap();
        assert!(fresh_omp_report_path(&paths, task, 2).is_err());
    }

    #[test]
    fn omp_initial_agent_must_match_the_recorded_spawn_identity() {
        let spawned = pane_cleanup::SpawnIdentity {
            pane_id: "w1:p2".to_owned(),
            terminal_id: "term-owned".to_owned(),
            session_value: "/tmp/session-owned".to_owned(),
        };
        let mut initial = json!({"result": {"agent": {
            "pane_id": "w1:p2", "terminal_id": "term-owned",
            "agent_session": {"kind": "path", "value": "/tmp/session-owned"}
        }}});
        omp_spawn_matches_initial(&spawned, &initial).unwrap();
        initial["result"]["agent"]["agent_session"]["value"] = json!("/tmp/session-replaced");
        assert!(omp_spawn_matches_initial(&spawned, &initial).is_err());
        initial["result"]["agent"]["agent_session"]["value"] = json!("/tmp/session-owned");
        initial["result"]["agent"]["terminal_id"] = json!("term-replaced");
        assert!(omp_spawn_matches_initial(&spawned, &initial).is_err());
    }

    #[test]
    fn omp_second_read_rejects_replaced_session_or_reused_pane() {
        let initial = json!({"result": {"agent": {
            "name": "brgr-fixture",
            "agent": "omp",
            "pane_id": "w1:p2",
            "terminal_id": "term-original",
            "agent_session": {"kind": "path", "value": "/tmp/session-original"},
            "state_change_seq": 11,
            "agent_status": "idle"
        }}});
        let mut second_read = initial.clone();
        second_read["result"]["agent"]["state_change_seq"] = json!(12);
        second_read["result"]["agent"]["agent_status"] = json!("done");

        // A replacement can retain the pane and terminal while rotating the
        // immutable session identity; fail closed before accepting its report.
        second_read["result"]["agent"]["agent_session"]["value"] = json!("/tmp/session-replaced");
        assert!(omp_completion_ready(&initial, &second_read, "brgr-fixture").is_err());

        // A reused pane id is unsafe even when the session value is restored:
        // the replacement terminal is a distinct live identity.
        second_read["result"]["agent"]["agent_session"]["value"] = json!("/tmp/session-original");
        second_read["result"]["agent"]["terminal_id"] = json!("term-reused");
        assert!(omp_completion_ready(&initial, &second_read, "brgr-fixture").is_err());
    }
}
