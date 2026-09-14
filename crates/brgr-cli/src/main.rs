mod codex_integration;
mod pane_cleanup;

use std::{
    env,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use brgr_core::{ExecutionObservation, Supervisor, TaskRevision};
use brgr_protocol::{
    ArtifactContract, AttemptBudget, AttemptId, AttemptState, Decision, DecisionId,
    DecisionVerdict, OwnerId, ResultEnvelope, ResultId, Route, SCHEMA_V1, TaskId, TaskSpec,
    TerminalOutcome,
};
use brgr_registry::{ActivationReceipt, Health, Registry};
use brgr_runner::{
    ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1, ProbeSpec,
    ResultSource, ResultSpec,
};
use brgr_store::{RunnerIdentity, Store, StoreError, UnfinishedAttempt};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(name = "brgr", version, about = "Durable local agent task bridge")]
struct Cli {
    #[arg(long, global = true, env = "BRGR_HOME")]
    home: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Run(RunArgs),
    Revise(ReviseArgs),
    Status {
        task: Option<TaskId>,
    },
    Result {
        task: TaskId,
        #[arg(long)]
        ack: bool,
    },
    Cancel {
        task: TaskId,
    },
    Bind {
        task: TaskId,
        #[arg(long)]
        session: Option<String>,
    },
    Accept {
        task: TaskId,
        #[arg(long, default_value = "acceptance criteria verified")]
        reason: String,
    },
    Reject {
        task: TaskId,
        #[arg(long)]
        reason: String,
    },
    Harness {
        #[command(subcommand)]
        command: HarnessCommand,
    },
    Integrate {
        #[command(subcommand)]
        command: IntegrateCommand,
    },
    Doctor,
    Cleanup {
        #[command(subcommand)]
        command: CleanupCommand,
    },
    #[command(name = "__supervise", hide = true)]
    Supervise {
        launch: PathBuf,
    },
    #[command(name = "__hook", hide = true)]
    Hook {
        event: HookEvent,
    },
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

#[derive(Args)]
struct RunArgs {
    objective: String,
    #[arg(long, default_value = "local.gjc")]
    harness: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    #[arg(long = "criterion")]
    criteria: Vec<String>,
    #[arg(long, default_value_t = 3_600)]
    deadline_seconds: u64,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    allow_clean_head_snapshot: bool,
    #[arg(long, hide = true)]
    foreground: bool,
    #[arg(long)]
    keep_pane: bool,
}

#[derive(Args)]
struct ReviseArgs {
    task: TaskId,
    objective: String,
    #[arg(long = "criterion")]
    criteria: Vec<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    allow_clean_head_snapshot: bool,
    #[arg(long, hide = true)]
    foreground: bool,
    #[arg(long)]
    keep_pane: bool,
}

struct StartOptions<'a> {
    source_workspace: &'a Path,
    snapshot: WorkspaceSnapshot,
    pane: PaneDisposition,
    execution: ExecutionDisposition,
    json_output: bool,
}

enum WorkspaceSnapshot {
    RequireClean,
    AllowCleanHead,
}

enum PaneDisposition {
    CleanupAfterDecision,
    Keep,
}

enum ExecutionDisposition {
    Detached,
    Foreground,
}

#[derive(Clone, Copy, Subcommand)]
enum CleanupCommand {
    Status { task: TaskId },
    Run { task: TaskId },
}

#[derive(Subcommand)]
enum HarnessCommand {
    Add(AddHarnessArgs),
    Draft {
        executable: PathBuf,
    },
    Test {
        executable: Option<PathBuf>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
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
    Status {
        #[arg(default_value = "local.gjc")]
        harness: String,
    },
}

#[derive(Args)]
struct AddHarnessArgs {
    executable: PathBuf,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    #[arg(long)]
    presentation_only: bool,
}

#[derive(Subcommand)]
enum IntegrateCommand {
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
}

#[derive(Subcommand)]
enum CodexCommand {
    Install,
    Status,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    Stop,
}

#[derive(Clone, Debug)]
struct Paths {
    home: PathBuf,
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
        let paths = Self {
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
    manifest: Option<HarnessManifest>,
    #[serde(default)]
    executable_digest: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.home)?;
    match cli.command {
        Command::Run(args) => run_task(&paths, args, cli.json).await,
        Command::Revise(args) => revise_task(&paths, args, cli.json).await,
        Command::Status { task } => status(&paths, task, cli.json),
        Command::Result { task, ack } => result(&paths, task, ack, cli.json),
        Command::Cancel { task } => cancel(&paths, task, cli.json),
        Command::Bind { task, session } => bind(&paths, task, session, cli.json),
        Command::Accept { task, reason } => {
            decide(&paths, task, DecisionVerdict::Accepted, reason, cli.json)
        }
        Command::Reject { task, reason } => {
            decide(&paths, task, DecisionVerdict::Rejected, reason, cli.json)
        }
        Command::Harness { command } => harness(&paths, command, cli.json).await,
        Command::Integrate { command } => integrate(&paths, command, cli.json),
        Command::Doctor => doctor(&paths, cli.json),
        Command::Cleanup { command } => cleanup(&paths, command, cli.json),
        Command::Supervise { launch } => supervise(&paths, &launch, cli.json).await,
        Command::Hook { event } => {
            if hook(&paths, event).is_err() {
                eprintln!("brgr hook could not read the inbox; run `brgr doctor`");
                println!("{{}}");
            }
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
            &paths,
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

async fn run_task(paths: &Paths, args: RunArgs, json_output: bool) -> Result<()> {
    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    match registry.health_probed(&args.harness).await? {
        Health::Healthy => {}
        Health::Drifted { .. } => bail!("harness {} probe identity changed", args.harness),
    }
    let (activated, activation) = registry
        .load_healthy_with_receipt(&args.harness)
        .with_context(|| format!("harness {} is not active and healthy", args.harness))?;
    let task_id = TaskId::new();
    let source_workspace = args.workspace.unwrap_or(env::current_dir()?);
    let owner_id = owner_from_environment()?;
    let criteria = if args.criteria.is_empty() {
        vec![format!("Objective achieved: {}", args.objective)]
    } else {
        args.criteria
    };
    let spec = TaskSpec {
        schema: SCHEMA_V1.to_owned(),
        task_id,
        revision: 1,
        create_request_id: format!("run-{task_id}"),
        owner_id,
        objective: args.objective,
        workspace: source_workspace.to_string_lossy().into_owned(),
        route: Route {
            harness_id: args.harness.clone(),
            requested_model: args.model,
            requested_effort: args.effort,
        },
        required_capabilities: vec!["completion".to_owned()],
        artifact_contract: ArtifactContract {
            media_type: activated.result.media_type.clone(),
            max_bytes: activated.result.max_bytes,
        },
        acceptance_criteria: criteria,
        budget: AttemptBudget {
            deadline_seconds: args.deadline_seconds,
            max_attempts: 2,
        },
    };
    spec.validate()?;
    start_task(
        paths,
        spec,
        &activated,
        &activation,
        StartOptions {
            source_workspace: &source_workspace,
            snapshot: if args.allow_clean_head_snapshot {
                WorkspaceSnapshot::AllowCleanHead
            } else {
                WorkspaceSnapshot::RequireClean
            },
            pane: if args.keep_pane {
                PaneDisposition::Keep
            } else {
                PaneDisposition::CleanupAfterDecision
            },
            execution: if args.foreground {
                ExecutionDisposition::Foreground
            } else {
                ExecutionDisposition::Detached
            },
            json_output,
        },
    )
    .await
}

async fn revise_task(paths: &Paths, args: ReviseArgs, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let previous = store.task(args.task)?;
    require_owner(&store, &previous.owner_id)?;
    let result = store.latest_result(args.task)?;
    if result.revision != previous.revision {
        bail!("the latest revision has not produced a terminal result");
    }
    let decision = store
        .decision_for_result(result.result_id)?
        .context("the previous result has no Codex decision")?;
    if decision.verdict != DecisionVerdict::Rejected {
        bail!("only a rejected result can be revised");
    }
    let next_revision = previous
        .revision
        .checked_add(1)
        .context("revision overflow")?;
    let source_workspace = args
        .workspace
        .unwrap_or_else(|| PathBuf::from(&previous.workspace));
    let criteria = if args.criteria.is_empty() {
        vec![format!("Objective achieved: {}", args.objective)]
    } else {
        args.criteria
    };
    let mut replacement = previous.clone();
    replacement.revision = next_revision;
    replacement.create_request_id = format!("revise-{}-{next_revision}", args.task);
    replacement.objective = args.objective;
    replacement.workspace = source_workspace.to_string_lossy().into_owned();
    replacement.acceptance_criteria = criteria;
    let replacement = TaskRevision::new(previous)?
        .revise(replacement)?
        .spec()
        .clone();

    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    match registry
        .health_probed(&replacement.route.harness_id)
        .await?
    {
        Health::Healthy => {}
        Health::Drifted { .. } => bail!("harness probe identity changed"),
    }
    let (activated, activation) =
        registry.load_healthy_with_receipt(&replacement.route.harness_id)?;
    start_task(
        paths,
        replacement,
        &activated,
        &activation,
        StartOptions {
            source_workspace: &source_workspace,
            snapshot: if args.allow_clean_head_snapshot {
                WorkspaceSnapshot::AllowCleanHead
            } else {
                WorkspaceSnapshot::RequireClean
            },
            pane: if args.keep_pane {
                PaneDisposition::Keep
            } else {
                PaneDisposition::CleanupAfterDecision
            },
            execution: if args.foreground {
                ExecutionDisposition::Foreground
            } else {
                ExecutionDisposition::Detached
            },
            json_output,
        },
    )
    .await
}

async fn start_task(
    paths: &Paths,
    mut spec: TaskSpec,
    activated: &HarnessManifest,
    activation: &ActivationReceipt,
    options: StartOptions<'_>,
) -> Result<()> {
    spec.validate()?;
    activated.validate_task_route(&spec)?;
    let source = options.source_workspace.canonicalize()?;
    let home = paths.home.canonicalize()?;
    if source.starts_with(&home) || home.starts_with(&source) {
        bail!("brgr control home and the source workspace must not overlap");
    }
    let store = Store::open(&paths.store)?;
    let session = current_session()?;
    match (store.owner_binding(&spec.owner_id)?, session.as_deref()) {
        (Some((bound, _)), Some(current)) if bound == current => {}
        (None, Some(current)) => {
            store.rebind_owner(&spec.owner_id, current)?;
        }
        (Some(_), _) => bail!(
            "owner is bound to another session; run `brgr bind TASK --session SESSION` before starting another revision"
        ),
        (None, None) => {}
    }
    let task_id = spec.task_id;
    let harness_id = spec.route.harness_id.clone();
    let workspace = prepare_workspace(
        paths,
        options.source_workspace,
        task_id,
        spec.revision,
        &activated.adapter,
        matches!(options.snapshot, WorkspaceSnapshot::AllowCleanHead),
    )?;
    spec.workspace = workspace.to_string_lossy().into_owned();
    let launch = LaunchEnvelope {
        spec,
        harness_id,
        protocol_generation: "brgr-v1".to_owned(),
        keep_pane: matches!(options.pane, PaneDisposition::Keep),
        manifest: Some(activated.clone()),
        executable_digest: Some(activation.executable_digest.clone()),
    };
    let launch_path = paths.launch(task_id, launch.spec.revision);
    write_json_new(&launch_path, &launch)?;
    let mut store = Store::open(&paths.store)?;
    let request_digest = Sha256::digest(serde_json::to_vec(&launch.spec)?);
    let mut request_digest_text = String::with_capacity(64);
    for byte in request_digest {
        write!(&mut request_digest_text, "{byte:02x}")?;
    }
    store.record_task(&launch.spec, &request_digest_text)?;

    if matches!(options.execution, ExecutionDisposition::Foreground) {
        return supervise(paths, &launch_path, options.json_output).await;
    }
    if let Err(error) = spawn_supervisor(paths, &launch_path) {
        record_unstarted_terminal(
            &mut store,
            &launch.spec,
            TerminalOutcome::Failed,
            format!("detached supervisor could not start: {error}"),
        )?;
        return Err(error);
    }
    let receipt = json!({
        "task_id": task_id,
        "state": "starting",
        "workspace": workspace,
        "harness": launch.harness_id,
        "revision": launch.spec.revision,
        "requested_model": launch.spec.route.requested_model,
        "requested_effort": launch.spec.route.requested_effort,
    });
    print_value(&receipt, options.json_output);
    Ok(())
}

async fn supervise(paths: &Paths, launch_path: &Path, json_output: bool) -> Result<()> {
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(launch_path)?)?;
    if launch.protocol_generation != "brgr-v1" {
        bail!("unsupported task protocol generation");
    }
    let manifest = pinned_manifest_for_launch(
        launch.manifest.as_ref(),
        launch.executable_digest.as_deref(),
        &launch.harness_id,
    )?;
    manifest.validate_task_route(&launch.spec)?;
    #[cfg(debug_assertions)]
    if env::var_os("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM").is_some() {
        std::process::exit(79);
    }
    let receipt = ProcessReceipt {
        task_id: launch.spec.task_id,
        launch_path: launch_path.to_path_buf(),
        identity: process_identity(std::process::id())?,
    };
    let manifest = if manifest.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1 {
        omp_process_manifest(paths, &launch, &manifest)?
    } else {
        manifest
    };
    let cancel_path = paths.cancel(launch.spec.task_id);
    let pid_path = paths.pid(launch.spec.task_id);
    let mut supervisor = Supervisor::open(&paths.store)?;
    // Reconcile before publishing our own task receipt. Otherwise a previous
    // crashed attempt without a launch identity could mistake this new process
    // for its original live supervisor and remain unfinished forever.
    supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?;
    write_json_atomic(&paths.supervisor(launch.spec.task_id), &receipt)?;
    let result = supervisor
        .run_fresh_controlled(
            launch.spec,
            &manifest,
            Some(&cancel_path),
            Some(&pid_path),
            Some(&receipt.identity),
        )
        .await?;
    let _ = fs::remove_file(cancel_path);
    let _ = fs::remove_file(paths.supervisor(result.task_id));
    print_value(&serde_json::to_value(result)?, json_output);
    Ok(())
}

fn pinned_manifest_for_launch(
    manifest: Option<&HarnessManifest>,
    digest: Option<&str>,
    harness_id: &str,
) -> Result<HarnessManifest> {
    let (Some(manifest), Some(digest)) = (manifest, digest) else {
        bail!("launch lacks a complete pinned manifest; unsafe replay is disabled");
    };
    if manifest.id != harness_id {
        bail!("task-pinned harness id differs from its manifest");
    }
    Registry::verify_pinned_executable(manifest, digest)?;
    Ok(manifest.clone())
}

fn spawn_supervisor(paths: &Paths, launch_path: &Path) -> Result<()> {
    let executable = env::current_exe()?;
    let log_path = paths.runs.join("supervisor.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    stdout.set_permissions(fs::Permissions::from_mode(0o600))?;
    let stderr = stdout.try_clone()?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--home")
        .arg(&paths.home)
        .arg("--json")
        .arg("__supervise")
        .arg(launch_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0);
    command
        .spawn()
        .context("failed to start detached supervisor")?;
    Ok(())
}

fn reconcile_pending(paths: &Paths) -> Result<()> {
    let mut supervisor = Supervisor::open(&paths.store)?;
    supervisor.reconcile_after_restart(|attempt| observe_attempt(paths, attempt))?;
    let mut store = Store::open(&paths.store)?;
    for task in store.unstarted_tasks()? {
        if unstarted_admission_is_stale(paths, &task)? {
            let cancelled = paths.cancel(task.task_id).exists();
            record_unstarted_terminal(
                &mut store,
                &task,
                if cancelled {
                    TerminalOutcome::Cancelled
                } else {
                    TerminalOutcome::Lost
                },
                if cancelled {
                    "cancelled before the supervisor claimed the task".to_owned()
                } else {
                    "supervisor did not claim the admitted task".to_owned()
                },
            )?;
        }
    }
    Ok(())
}

fn unstarted_admission_is_stale(paths: &Paths, task: &TaskSpec) -> Result<bool> {
    let launch_path = paths.launch(task.task_id, task.revision);
    let metadata = match fs::symlink_metadata(&launch_path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !metadata
        .modified()?
        .elapsed()
        .is_ok_and(|age| age >= Duration::from_secs(5))
    {
        return Ok(false);
    }
    let receipt = fs::read(paths.supervisor(task.task_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ProcessReceipt>(&bytes).ok());
    let Some(receipt) =
        receipt.filter(|value| value.task_id == task.task_id && value.launch_path == launch_path)
    else {
        return Ok(true);
    };
    let live = receipt
        .identity
        .handle
        .parse::<u32>()
        .ok()
        .and_then(|pid| process_identity(pid).ok())
        .is_some_and(|actual| actual == receipt.identity);
    Ok(!live)
}

fn record_unstarted_terminal(
    store: &mut Store,
    task: &TaskSpec,
    outcome: TerminalOutcome,
    reason: String,
) -> Result<bool> {
    let attempt_id = AttemptId::new();
    match store.claim_attempt(task.task_id, task.revision, attempt_id) {
        Ok(()) => {}
        Err(
            StoreError::ActiveAttemptExists { .. }
            | StoreError::UnresolvedPriorAttempt { .. }
            | StoreError::NonRetryablePriorAttempt { .. }
            | StoreError::AttemptBudgetExhausted { .. },
        ) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let next = if outcome == TerminalOutcome::Cancelled {
        AttemptState::CancelRequested
    } else {
        AttemptState::Starting
    };
    store.compare_and_set_attempt_state(attempt_id, AttemptState::Queued, next)?;
    let result = ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome,
        artifacts: vec![],
        error: Some(reason),
        route_observation: Some(brgr_protocol::RouteObservation::unavailable()),
        unresolved_effects: if outcome == TerminalOutcome::Lost {
            vec!["execution identity was not established".to_owned()]
        } else {
            vec![]
        },
    };
    store.commit_terminal_result(&task.owner_id, &result)?;
    Ok(true)
}

fn observe_attempt(paths: &Paths, attempt: &UnfinishedAttempt) -> ExecutionObservation {
    let receipt: ProcessReceipt = match fs::read(paths.supervisor(attempt.task.task_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(receipt) => receipt,
        None => return ExecutionObservation::Unknown,
    };
    if receipt.task_id != attempt.task.task_id
        || receipt.launch_path != paths.launch(attempt.task.task_id, attempt.task.revision)
    {
        return ExecutionObservation::Unknown;
    }
    let Ok(pid) = receipt.identity.handle.parse::<u32>() else {
        return ExecutionObservation::Unknown;
    };
    match process_identity(pid) {
        Ok(actual) if actual == receipt.identity => ExecutionObservation::SupervisorAlive(actual),
        Ok(_) => ExecutionObservation::NotObserved,
        Err(_) => ExecutionObservation::Unknown,
    }
}

fn process_identity(pid: u32) -> Result<RunnerIdentity> {
    let pid_text = pid.to_string();
    let start = ps_field(&pid_text, "lstart")?;
    let executable = ps_field(&pid_text, "comm")?;
    if Path::new(&executable)
        .file_name()
        .is_none_or(|name| name != "brgr")
    {
        bail!("process {pid} is not brgr");
    }
    Ok(RunnerIdentity {
        namespace: "brgr.supervisor".to_owned(),
        handle: pid_text,
        birth_marker: start,
    })
}

fn ps_field(pid: &str, field: &str) -> Result<String> {
    let output = ProcessCommand::new("/bin/ps")
        .args(["-ww", "-p", pid, "-o"])
        .arg(format!("{field}="))
        .output()?;
    if !output.status.success() {
        bail!("process {pid} is not observable");
    }
    let text = String::from_utf8(output.stdout)?.trim().to_owned();
    if text.is_empty() {
        bail!("process {pid} has no {field} identity");
    }
    Ok(text)
}

fn status(paths: &Paths, task: Option<TaskId>, json_output: bool) -> Result<()> {
    reconcile_pending(paths)?;
    let store = Store::open(&paths.store)?;
    if let Some(task_id) = task {
        let spec = store.task(task_id)?;
        require_owner(&store, &spec.owner_id)?;
        let state = match store.attempt_state(task_id) {
            Ok(state) => state,
            Err(StoreError::TaskNotFound(_)) => AttemptState::Queued,
            Err(error) => return Err(error.into()),
        };
        print_value(
            &json!({"task": spec, "state": format!("{state:?}").to_lowercase()}),
            json_output,
        );
    } else {
        let session = current_session()?;
        let owner = env::var("BRGR_OWNER_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(OwnerId::new)
            .transpose()?;
        let tasks = store
            .tasks_for_session(session.as_deref().unwrap_or(""), owner.as_ref(), 20)?
            .into_iter()
            .map(|spec| {
                let state = store.attempt_state(spec.task_id).ok();
                json!({"task": spec, "state": state.map(|value| format!("{value:?}").to_lowercase())})
            })
            .collect::<Vec<_>>();
        print_value(&json!(tasks), json_output);
    }
    Ok(())
}

fn result(paths: &Paths, task: TaskId, ack: bool, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let (session_id, binding_epoch) = require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    let artifacts = result
        .artifacts
        .iter()
        .map(|reference| {
            let bytes = store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
            Ok::<_, anyhow::Error>(json!({
                "reference": reference,
                "text": String::from_utf8_lossy(&bytes),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    if ack {
        store.acknowledge_bound(&spec.owner_id, result.result_id, &session_id, binding_epoch)?;
        if let Err(error) = pane_cleanup::mark_pending(&paths.runs, task, &result) {
            eprintln!("brgr pane cleanup queue could not be updated: {error}");
        } else if let Err(error) = pane_cleanup::close_if_eligible(&store, &paths.runs, task) {
            eprintln!("brgr pane cleanup remains pending: {error}");
        }
    }
    print_value(
        &json!({"result": result, "artifacts": artifacts}),
        json_output,
    );
    Ok(())
}

fn cancel(paths: &Paths, task: TaskId, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let state = match store.attempt_state(task) {
        Ok(state) => state,
        Err(StoreError::TaskNotFound(_)) => AttemptState::Queued,
        Err(error) => return Err(error.into()),
    };
    if state == AttemptState::Terminal {
        bail!("task {task} is already terminal");
    }
    let launch: LaunchEnvelope =
        serde_json::from_slice(&fs::read(paths.launch(task, spec.revision))?)?;
    let legacy_omp = launch.manifest.as_ref().map_or(
        matches!(
            spec.route.harness_id.as_str(),
            "local.omp" | "local.omp-herdr"
        ),
        |manifest| manifest.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1,
    );
    if legacy_omp {
        bail!("Herdr-backed OMP cancellation is not certified; use the process adapter");
    }
    fs::write(paths.cancel(task), b"cancel\n")?;
    print_value(
        &json!({"task_id": task, "state": "cancel_requested"}),
        json_output,
    );
    Ok(())
}

fn bind(paths: &Paths, task: TaskId, session: Option<String>, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    if let Ok(explicit_owner) = env::var("BRGR_OWNER_ID")
        && explicit_owner != spec.owner_id.as_str()
    {
        bail!("task belongs to {}; BRGR_OWNER_ID differs", spec.owner_id);
    }
    let observed = current_session()?;
    let session = session
        .or_else(|| observed.clone())
        .context("provide --session SESSION or run inside a Codex session")?;
    if observed.as_ref().is_some_and(|value| value != &session) {
        bail!("requested session does not match the current Codex session");
    }
    let epoch = store.rebind_owner(&spec.owner_id, &session)?;
    print_value(
        &json!({"owner_id": spec.owner_id, "session_id": session, "binding_epoch": epoch}),
        json_output,
    );
    Ok(())
}

fn decide(
    paths: &Paths,
    task: TaskId,
    verdict: DecisionVerdict,
    reason: String,
    json_output: bool,
) -> Result<()> {
    if reason.trim().is_empty() {
        bail!("decision reason must not be empty");
    }
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let (session_id, binding_epoch) = require_owner(&store, &spec.owner_id)?;
    let result = store.latest_result(task)?;
    if result.outcome != TerminalOutcome::Candidate {
        bail!("only candidate results can be accepted or rejected");
    }
    if result.artifacts.is_empty() {
        bail!("candidate result has no sealed artifact");
    }
    for reference in &result.artifacts {
        store.read_artifact(reference, spec.artifact_contract.max_bytes)?;
    }
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: spec.owner_id.clone(),
        task_id: task,
        revision: result.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result)?,
        session_id: Some(session_id),
        binding_epoch: Some(binding_epoch),
        verdict,
        reason,
    };
    store.record_decision_and_ack(&decision)?;
    let persisted = store
        .decision_for_result(result.result_id)?
        .context("decision was not readable after commit")?;
    if let Err(error) = pane_cleanup::mark_pending(&paths.runs, task, &result) {
        eprintln!("brgr pane cleanup queue could not be updated: {error}");
    } else if let Err(error) = pane_cleanup::close_if_eligible(&store, &paths.runs, task) {
        eprintln!("brgr pane cleanup remains pending: {error}");
    }
    print_value(&serde_json::to_value(persisted)?, json_output);
    Ok(())
}

async fn harness(paths: &Paths, command: HarnessCommand, json_output: bool) -> Result<()> {
    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    match command {
        HarnessCommand::Add(args) => {
            let receipt = add_harness(&registry, args).await?;
            print_value(&serde_json::to_value(receipt)?, json_output);
        }
        HarnessCommand::Draft { executable } => {
            let manifest = registry.draft(&executable).await?;
            print_value(&serde_json::to_value(manifest)?, json_output);
        }
        HarnessCommand::Test {
            executable,
            manifest,
        } => {
            let (manifest, custom) =
                harness_manifest_input(&registry, executable, manifest).await?;
            if custom {
                registry.contract_test_custom(&manifest).await?;
            } else {
                registry.contract_test(&manifest).await?;
            }
            print_value(
                &json!({"harness": manifest.id, "contract": "passed", "activation": "requires_scratch_run"}),
                json_output,
            );
        }
        HarnessCommand::Activate {
            executable,
            manifest,
            workspace,
            prompt,
            model,
            effort,
        } => {
            let (manifest, custom) =
                harness_manifest_input(&registry, executable, manifest).await?;
            let receipt = if custom {
                registry
                    .activate_custom_with_scratch(
                        &manifest,
                        &workspace,
                        &prompt,
                        model.as_deref(),
                        effort.as_deref(),
                    )
                    .await?
            } else {
                registry
                    .activate_with_scratch(
                        &manifest,
                        &workspace,
                        &prompt,
                        model.as_deref(),
                        effort.as_deref(),
                    )
                    .await?
            };
            print_value(&serde_json::to_value(receipt)?, json_output);
        }
        HarnessCommand::Status { harness } => {
            let health = registry.health_probed(&harness).await?;
            let value = match health {
                Health::Healthy => json!({"harness": harness, "health": "healthy"}),
                Health::Drifted { expected, observed } => json!({
                    "harness": harness,
                    "health": "drifted",
                    "expected": expected,
                    "observed": observed,
                }),
            };
            print_value(&value, json_output);
        }
    }
    Ok(())
}

async fn add_harness(registry: &Registry, args: AddHarnessArgs) -> Result<ActivationReceipt> {
    let receipt = if args.presentation_only {
        if args.workspace.is_some()
            || args.prompt.is_some()
            || args.model.is_some()
            || args.effort.is_some()
        {
            bail!("presentation-only registration cannot claim a scratch run or model route");
        }
        registry.add(&args.executable).await?
    } else {
        let workspace = args
            .workspace
            .context("harness add needs --workspace for an authorized scratch run")?;
        let prompt = args
            .prompt
            .context("harness add needs --prompt for an authorized scratch run")?;
        let manifest = registry.draft(&args.executable).await?;
        registry.contract_test(&manifest).await?;
        registry
            .activate_with_scratch(
                &manifest,
                &workspace,
                &prompt,
                args.model.as_deref(),
                args.effort.as_deref(),
            )
            .await?
    };
    match registry.health_probed(&receipt.harness_id).await? {
        Health::Healthy => Ok(receipt),
        Health::Drifted { .. } => bail!("harness changed during activation health check"),
    }
}

async fn harness_manifest_input(
    registry: &Registry,
    executable: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
) -> Result<(HarnessManifest, bool)> {
    match (executable, manifest_path) {
        (Some(executable), None) => Ok((registry.draft(&executable).await?, false)),
        (None, Some(path)) => {
            if !path.is_absolute() || !fs::symlink_metadata(&path)?.file_type().is_file() {
                bail!("custom manifest must be an absolute regular file");
            }
            let mut bytes = Vec::new();
            OpenOptions::new()
                .read(true)
                .open(path)?
                .take(1_048_577)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 1_048_576 {
                bail!("custom manifest exceeds 1 MiB");
            }
            Ok((serde_json::from_slice(&bytes)?, true))
        }
        _ => bail!("specify exactly one executable or --manifest path"),
    }
}

fn integrate(paths: &Paths, command: IntegrateCommand, json_output: bool) -> Result<()> {
    match command {
        IntegrateCommand::Codex { command } => {
            let outcome = match command {
                CodexCommand::Install => codex_integration::install(&paths.home)?,
                CodexCommand::Status => codex_integration::status(&paths.home)?,
                CodexCommand::Uninstall => codex_integration::uninstall(&paths.home)?,
            };
            print_value(&outcome, json_output);
        }
    }
    Ok(())
}

fn doctor(paths: &Paths, json_output: bool) -> Result<()> {
    reconcile_pending(paths)?;
    let store_ok = Store::open(&paths.store).is_ok();
    let registry_ok = Registry::open_with_control_home(&paths.registry, &paths.home).is_ok();
    let integration = codex_integration::status(&paths.home)?;
    let value = json!({
        "status": if store_ok && registry_ok { "ok" } else { "error" },
        "store": store_ok,
        "registry": registry_ok,
        "codex_integration": integration,
    });
    print_value(&value, json_output);
    Ok(())
}

fn cleanup(paths: &Paths, command: CleanupCommand, json_output: bool) -> Result<()> {
    let task = match command {
        CleanupCommand::Status { task } | CleanupCommand::Run { task } => task,
    };
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    let status = match command {
        CleanupCommand::Status { .. } => pane_cleanup::status(&store, &paths.runs, task)?,
        CleanupCommand::Run { .. } => pane_cleanup::close_if_eligible(&store, &paths.runs, task)?,
    };
    print_value(&json!({"task_id": task, "cleanup": status}), json_output);
    Ok(())
}

fn hook(paths: &Paths, event: HookEvent) -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: HookInput = serde_json::from_str(&input).unwrap_or(HookInput { session_id: None });
    let Some(session_id) = input.session_id else {
        println!("{{}}");
        return Ok(());
    };
    let owner = OwnerId::new(format!("codex:{session_id}"))?;
    reconcile_pending(paths)?;
    let store = Store::open(&paths.store)?;
    if event == HookEvent::SessionStart {
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        store.bind_owner(&owner, &session_id, epoch.max(1))?;
    }
    let pending = store.pending_for_session(&session_id)?;
    if pending.is_empty() {
        println!("{{}}");
        return Ok(());
    }
    let handles = pending
        .iter()
        .take(10)
        .map(|item| format!("{}:{:?}", item.result.task_id, item.result.outcome))
        .collect::<Vec<_>>()
        .join(", ");
    let summary = format!("{} pending result(s): {handles}", pending.len());
    match event {
        HookEvent::Stop => println!(
            "{}",
            json!({
                "decision": "block",
                "reason": "brgr has unprocessed terminal results; inspect and accept, reject, or acknowledge them before stopping",
                "hookSpecificOutput": {
                    "hookEventName": "Stop",
                    "decision": "block",
                    "reason": format!("Pending brgr inbox: {summary}. Use brgr result TASK, then accept/reject or ack."),
                }
            })
        ),
        HookEvent::SessionStart | HookEvent::UserPromptSubmit => println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": format!("{event:?}"),
                    "additionalContext": format!("Pending brgr inbox: {summary}. Verify each result, then run brgr accept/reject; acknowledge non-candidate outcomes with brgr result TASK --ack.")
                }
            })
        ),
    }
    Ok(())
}

fn omp_process_manifest(
    paths: &Paths,
    launch: &LaunchEnvelope,
    activated: &HarnessManifest,
) -> Result<HarnessManifest> {
    let executable = env::current_exe()?;
    let mut argv = vec![
        "--home".to_owned(),
        paths.home.to_string_lossy().into_owned(),
        "__omp-run".to_owned(),
        "--prompt-file".to_owned(),
        "${input.prompt_file}".to_owned(),
        "--workspace".to_owned(),
        "${task.workspace}".to_owned(),
        "--task".to_owned(),
        launch.spec.task_id.to_string(),
        "--revision".to_owned(),
        launch.spec.revision.to_string(),
        "--launcher".to_owned(),
        activated.executable.to_string_lossy().into_owned(),
    ];
    if launch.spec.route.requested_model.is_some() {
        argv.extend(["--model".to_owned(), "${route.model}".to_owned()]);
    }
    if launch.spec.route.requested_effort.is_some() {
        argv.extend(["--effort".to_owned(), "${route.effort}".to_owned()]);
    }
    if launch.keep_pane {
        argv.push("--keep-pane".to_owned());
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "internal.omp-runner".to_owned(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
        },
        launch: LaunchSpec {
            argv,
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: vec![
                "HOME".to_owned(),
                "PATH".to_owned(),
                "LANG".to_owned(),
                "HERDR_ENV".to_owned(),
                "HERDR_PANE_ID".to_owned(),
            ],
            mode: ExecutionMode::DelegatedExternal,
        },
        result: ResultSpec {
            source: ResultSource::Stdout,
            media_type: "text/markdown".to_owned(),
            max_bytes: launch.spec.artifact_contract.max_bytes,
            success_exit_codes: vec![0],
        },
        capabilities: activated.capabilities.clone(),
    })
}

#[derive(Clone, Copy)]
struct OmpOptions<'a> {
    revision: u32,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    keep_pane: bool,
}

fn run_omp_adapter(
    paths: &Paths,
    prompt_file: &Path,
    workspace: &Path,
    task: TaskId,
    launcher: &Path,
    options: OmpOptions<'_>,
) -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") || env::var_os("HERDR_PANE_ID").is_none() {
        bail!("OMP adapter requires a verified Herdr parent session");
    }
    let spec = Store::open(&paths.store)?.task(task)?;
    if spec.revision != options.revision {
        bail!("OMP wrapper revision differs from the admitted task revision");
    }
    let report_limit = spec.artifact_contract.max_bytes;
    let short = &task.to_string()[..8];
    let task_slug = if options.revision == 1 {
        format!("brgr-{short}")
    } else {
        format!("brgr-{short}-r{}", options.revision)
    };
    let agent = task_slug.clone();
    let report = fresh_omp_report_path(paths, task, options.revision)?;
    let prompt = fs::read_to_string(prompt_file)?;

    let launcher_receipt = launch_omp(
        launcher,
        workspace,
        &agent,
        &task_slug,
        &report,
        options.model,
        options.effort,
    )?;
    let spawn_identity = pane_cleanup::record_spawn(
        &Store::open(&paths.store)?,
        &paths.runs,
        task,
        &agent,
        &launcher_receipt,
        options.keep_pane,
    )?;
    let initial = get_omp_agent(&agent)?;
    omp_spawn_matches_initial(&spawn_identity, &initial)?;

    let instruction = format!(
        "{prompt}\n\nWrite the final result as Markdown to {} before finishing.",
        report.display()
    );
    let prompted = ProcessCommand::new("omp-prompt")
        .arg("--expected-report")
        .arg(&report)
        .arg(&agent)
        .arg(instruction)
        .output()?;
    if !prompted.status.success() {
        bail!(
            "OMP prompt failed: {}{}",
            String::from_utf8_lossy(&prompted.stdout),
            String::from_utf8_lossy(&prompted.stderr)
        );
    }
    let prompt_receipt: serde_json::Value = serde_json::from_slice(&prompted.stdout)?;
    if prompt_receipt
        .get("status")
        .and_then(serde_json::Value::as_str)
        != Some("prompted")
        || prompt_receipt
            .get("target")
            .and_then(serde_json::Value::as_str)
            != Some(agent.as_str())
    {
        bail!("OMP prompt did not confirm the exact agent target");
    }

    loop {
        let observed = get_omp_agent(&agent)?;
        if omp_completion_ready(&initial, &observed, &agent)? && report.is_file() {
            io::stdout().write_all(&read_bounded_regular_report(&report, report_limit)?)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn fresh_omp_report_path(paths: &Paths, task: TaskId, revision: u32) -> Result<PathBuf> {
    let report = paths.runs.join(format!("{task}-r{revision}.omp-report.md"));
    match fs::symlink_metadata(&report) {
        Ok(_) => bail!("fresh OMP report path already exists for this task revision"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(report),
        Err(error) => Err(error.into()),
    }
}

fn get_omp_agent(agent: &str) -> Result<serde_json::Value> {
    let observed = ProcessCommand::new("herdr")
        .args(["agent", "get", agent])
        .output()?;
    if !observed.status.success() {
        bail!("Herdr lost the managed OMP agent {agent}");
    }
    Ok(serde_json::from_slice(&observed.stdout)?)
}

fn omp_spawn_matches_initial(
    spawned: &pane_cleanup::SpawnIdentity,
    initial: &serde_json::Value,
) -> Result<()> {
    let agent = initial
        .pointer("/result/agent")
        .context("OMP launch has no agent receipt")?;
    if agent.get("pane_id").and_then(serde_json::Value::as_str) != Some(spawned.pane_id.as_str())
        || agent.get("terminal_id").and_then(serde_json::Value::as_str)
            != Some(spawned.terminal_id.as_str())
        || agent
            .pointer("/agent_session/value")
            .and_then(serde_json::Value::as_str)
            != Some(spawned.session_value.as_str())
    {
        bail!("OMP agent identity changed after its brgr ownership receipt");
    }
    Ok(())
}

fn omp_completion_ready(
    initial: &serde_json::Value,
    observed: &serde_json::Value,
    agent: &str,
) -> Result<bool> {
    let first = initial
        .pointer("/result/agent")
        .context("OMP launch has no agent receipt")?;
    let current = observed
        .pointer("/result/agent")
        .context("OMP observation has no agent receipt")?;
    if first.get("name").and_then(serde_json::Value::as_str) != Some(agent)
        || current.get("name").and_then(serde_json::Value::as_str) != Some(agent)
        || first.get("agent").and_then(serde_json::Value::as_str) != Some("omp")
        || current.get("agent").and_then(serde_json::Value::as_str) != Some("omp")
    {
        bail!("OMP agent name or kind changed");
    }
    for field in ["pane_id", "terminal_id"] {
        if first
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_none()
            || first.get(field) != current.get(field)
        {
            bail!("OMP pane or terminal identity changed");
        }
    }
    for field in ["kind", "value"] {
        if first
            .pointer(&format!("/agent_session/{field}"))
            .and_then(serde_json::Value::as_str)
            .is_none()
            || first.pointer(&format!("/agent_session/{field}"))
                != current.pointer(&format!("/agent_session/{field}"))
        {
            bail!("OMP session identity changed");
        }
    }
    let initial_seq = first
        .get("state_change_seq")
        .and_then(serde_json::Value::as_u64)
        .context("OMP launch lacks a lifecycle sequence")?;
    let current_seq = current
        .get("state_change_seq")
        .and_then(serde_json::Value::as_u64)
        .context("OMP observation lacks a lifecycle sequence")?;
    if current_seq < initial_seq {
        bail!("OMP lifecycle sequence regressed");
    }
    let status = current
        .get("agent_status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    if status == "blocked" {
        bail!("OMP agent {agent} is blocked on external input");
    }
    Ok(current_seq > initial_seq && matches!(status, "idle" | "done"))
}

fn read_bounded_regular_report(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let checked = fs::symlink_metadata(path)?;
    if !checked.file_type().is_file() || checked.len() == 0 {
        bail!("OMP report is missing or not a regular nonempty file");
    }
    if checked.len() > max_bytes {
        bail!("OMP report exceeds the task artifact limit");
    }
    let mut file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || (checked.dev(), checked.ino(), checked.len())
            != (opened.dev(), opened.ino(), opened.len())
    {
        bail!("OMP report changed before its descriptor was read");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || u64::try_from(bytes.len())? > max_bytes {
        bail!("OMP report is empty or exceeds the task artifact limit");
    }
    Ok(bytes)
}

fn launch_omp(
    launcher: &Path,
    workspace: &Path,
    agent: &str,
    task_slug: &str,
    report: &Path,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<PathBuf> {
    let mut launch = ProcessCommand::new("python");
    launch
        .arg(launcher)
        .args(["default", agent, "--cwd"])
        .arg(workspace)
        .args(["--task", task_slug])
        .args(["--reuse-worktree-objective", task_slug])
        .args(["--reuse-worktree-owner", agent])
        .arg("--expected-report")
        .arg(report);
    if let Some(value) = model {
        launch.args(["--model", value]);
    }
    if let Some(value) = effort {
        launch.args(["--effort", value]);
    }
    let launch_output = launch.output()?;
    let receipt_path = if launch_output.status.success() {
        let response: serde_json::Value = serde_json::from_slice(&launch_output.stdout)?;
        response
            .get("launcher_receipt")
            .and_then(serde_json::Value::as_str)
            .context("OMP launch is missing a launcher receipt")?
            .to_owned()
    } else {
        let failure: serde_json::Value =
            serde_json::from_slice(&launch_output.stdout).unwrap_or_else(|_| json!({}));
        if failure.get("detail").and_then(serde_json::Value::as_str)
            == Some("immutable agent session identity is missing")
        {
            recover_omp_contract(agent, task_slug, report, &failure)?;
            failure
                .get("receipt")
                .and_then(serde_json::Value::as_str)
                .context("OMP recovery is missing its launcher receipt")?
                .to_owned()
        } else {
            bail!(
                "OMP launcher preflight failed: {}{}",
                String::from_utf8_lossy(&launch_output.stdout),
                String::from_utf8_lossy(&launch_output.stderr)
            );
        }
    };
    let launcher_receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
    if launcher_receipt
        .get("codex_prompt_marked")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        bail!("new OMP run could also activate the legacy parent callback");
    }
    Ok(PathBuf::from(receipt_path))
}

fn recover_omp_contract(
    agent: &str,
    task_slug: &str,
    report: &Path,
    failure: &serde_json::Value,
) -> Result<()> {
    let receipt_path = failure
        .get("receipt")
        .and_then(serde_json::Value::as_str)
        .context("OMP recovery is missing its launcher receipt")?;
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(receipt_path)?)?;
    let live = (0..20)
        .find_map(|_| {
            let output = ProcessCommand::new("herdr")
                .args(["agent", "get", agent])
                .output()
                .ok()?;
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            let current = document.pointer("/result/agent")?.clone();
            if current.get("agent_session").is_some() {
                Some(current)
            } else {
                thread::sleep(Duration::from_millis(100));
                None
            }
        })
        .context("OMP session identity did not become observable")?;
    let child_pane = live
        .get("pane_id")
        .and_then(serde_json::Value::as_str)
        .context("OMP recovery is missing child pane identity")?;
    let session = live
        .get("agent_session")
        .context("OMP recovery is missing session identity")?;
    let session_kind = session
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("path");
    let session_value = session
        .get("value")
        .and_then(serde_json::Value::as_str)
        .context("OMP recovery session identity is malformed")?;
    let run_id = receipt
        .get("callback_run_id")
        .and_then(serde_json::Value::as_str)
        .context("OMP recovery is missing run identity")?;
    let parent_pane = receipt
        .get("parent_pane")
        .and_then(serde_json::Value::as_str)
        .context("OMP recovery is missing parent pane")?;
    let mut contract = json!({
        "version": 2,
        "run_id": run_id,
        "task": task_slug,
        "child_agent": agent,
        "child_pane": child_pane,
        "parent_pane": parent_pane,
        "parent_kind": "codex",
        "depth": 0,
        "nested": false,
        "root_owner": parent_pane,
        "root_run_id": run_id,
        "expected_report": report,
        "report_contracted": true,
        "qa_limits": null,
        "qa_resume": null,
    });
    let session_key = if session_kind == "id" {
        "agent_session_id"
    } else {
        "agent_session_path"
    };
    contract[session_key] = json!(session_value);
    let user_home = env::var_os("HOME").context("HOME is not set")?;
    let contract_path = PathBuf::from(user_home)
        .join(".omp/agent/callbacks/contracts")
        .join(format!("{agent}.json"));
    write_json_atomic(&contract_path, &contract)
}

fn prepare_workspace(
    paths: &Paths,
    source: &Path,
    task_id: TaskId,
    revision: u32,
    adapter: &str,
    allow_clean_head_snapshot: bool,
) -> Result<PathBuf> {
    let source = source.canonicalize()?;
    let root_output = ProcessCommand::new("git")
        .args([
            "-C",
            &source.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output();
    let root_output = match root_output {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(source),
        Err(error) => return Err(error.into()),
    };
    if !root_output.status.success() {
        return Ok(source);
    }
    let root = PathBuf::from(String::from_utf8(root_output.stdout)?.trim());
    let dirty = command_output(
        "git",
        &["-C", &root.to_string_lossy(), "status", "--porcelain"],
    )?;
    if !dirty.trim().is_empty() && !allow_clean_head_snapshot {
        bail!(
            "source worktree contains uncommitted changes; commit or capture them first, or pass --allow-clean-head-snapshot to explicitly exclude them"
        );
    }
    let base_revision =
        command_output("git", &["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])?;
    let worktree_list = command_output(
        "git",
        &[
            "-C",
            &root.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ],
    )?;
    let primary = worktree_list
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .context("git worktree inventory did not contain a primary checkout")?;
    let repo_name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("workspace");
    let short = task_id.to_string()[..8].to_owned();
    let task_slug = if revision == 1 {
        short
    } else {
        format!("{short}-r{revision}")
    };
    let target_parent = paths.worktrees.join(repo_name);
    fs::create_dir_all(&target_parent)?;
    let target = target_parent.join(&task_slug);
    let branch = format!("brgr/task-{task_slug}");
    let status = if adapter == brgr_runner::OMP_ROLE_ADAPTER_V1
        && env::var("HERDR_ENV").as_deref() == Ok("1")
        && env::var_os("HERDR_PANE_ID").is_some()
    {
        ProcessCommand::new("herdr")
            .args(["worktree", "create", "--cwd"])
            .arg(&primary)
            .args([
                "--branch",
                &branch,
                "--base",
                base_revision.trim(),
                "--path",
            ])
            .arg(&target)
            .args([
                "--label",
                &format!("brgr-{task_slug}"),
                "--no-focus",
                "--trust-repository",
            ])
            .status()?
    } else {
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&primary)
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&target)
            .arg(base_revision.trim())
            .status()?
    };
    if !status.success() {
        bail!("failed to create task worktree {}", target.display());
    }
    let relative = source.strip_prefix(&root).unwrap_or(Path::new(""));
    Ok(target.join(relative))
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new(program).args(args).output()?;
    if !output.status.success() {
        bail!("{program} command failed");
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn owner_from_environment() -> Result<OwnerId> {
    let session = current_session()?;
    let owner = env::var("BRGR_OWNER_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| session.map(|id| format!("codex:{id}")))
        .unwrap_or_else(|| "codex:manual".to_owned());
    Ok(OwnerId::new(owner)?)
}

fn current_session() -> Result<Option<String>> {
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
    Ok(codex.or(explicit))
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
    use tempfile::TempDir;

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
}
