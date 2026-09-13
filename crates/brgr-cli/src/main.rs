mod codex_integration;

use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use brgr_core::Supervisor;
use brgr_protocol::{
    ArtifactContract, AttemptBudget, Decision, DecisionId, DecisionVerdict, OwnerId, Route,
    SCHEMA_V1, TaskId, TaskSpec, TerminalOutcome,
};
use brgr_registry::{Health, Registry};
use brgr_runner::{
    ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1, ProbeSpec,
    ResultSource, ResultSpec,
};
use brgr_store::Store;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
        launcher: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
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
    #[arg(long, default_value_t = 3_600)]
    deadline_seconds: u64,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, hide = true)]
    foreground: bool,
}

#[derive(Subcommand)]
enum HarnessCommand {
    Add {
        executable: PathBuf,
    },
    Status {
        #[arg(default_value = "local.gjc")]
        harness: String,
    },
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
}

#[derive(Debug, Serialize, Deserialize)]
struct LaunchEnvelope {
    spec: TaskSpec,
    harness_id: String,
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
        Command::Status { task } => status(&paths, task, cli.json),
        Command::Result { task, ack } => result(&paths, task, ack, cli.json),
        Command::Cancel { task } => cancel(&paths, task, cli.json),
        Command::Accept { task, reason } => {
            decide(&paths, task, DecisionVerdict::Accepted, reason, cli.json)
        }
        Command::Reject { task, reason } => {
            decide(&paths, task, DecisionVerdict::Rejected, reason, cli.json)
        }
        Command::Harness { command } => harness(&paths, command, cli.json).await,
        Command::Integrate { command } => integrate(&paths, command, cli.json),
        Command::Doctor => doctor(&paths, cli.json),
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
            launcher,
            model,
            effort,
        } => run_omp_adapter(
            &paths,
            &prompt_file,
            &workspace,
            task,
            &launcher,
            model.as_deref(),
            effort.as_deref(),
        ),
    }
}

async fn run_task(paths: &Paths, args: RunArgs, json_output: bool) -> Result<()> {
    let registry = Registry::open(&paths.registry)?;
    let activated = registry
        .load_healthy(&args.harness)
        .with_context(|| format!("harness {} is not active and healthy", args.harness))?;
    let task_id = TaskId::new();
    let source_workspace = args.workspace.unwrap_or(env::current_dir()?);
    let workspace = prepare_workspace(paths, &source_workspace, task_id, &args.harness)?;
    let owner_id = owner_from_environment()?;
    let spec = TaskSpec {
        schema: SCHEMA_V1.to_owned(),
        task_id,
        revision: 1,
        create_request_id: format!("run-{task_id}"),
        owner_id,
        objective: args.objective,
        workspace: workspace.to_string_lossy().into_owned(),
        route: Route {
            harness_id: args.harness.clone(),
            requested_model: args.model,
            requested_effort: args.effort,
        },
        required_capabilities: vec!["completion".to_owned()],
        artifact_contract: ArtifactContract {
            media_type: activated.result.media_type,
            max_bytes: activated.result.max_bytes,
        },
        acceptance_criteria: vec!["sealed non-empty result".to_owned()],
        budget: AttemptBudget {
            deadline_seconds: args.deadline_seconds,
            max_attempts: 2,
        },
    };
    spec.validate()?;
    let launch = LaunchEnvelope {
        spec,
        harness_id: args.harness,
    };
    let launch_path = paths.launches.join(format!("{task_id}.json"));
    write_json_atomic(&launch_path, &launch)?;

    if args.foreground {
        return supervise(paths, &launch_path, json_output).await;
    }
    spawn_supervisor(paths, &launch_path)?;
    let receipt = json!({
        "task_id": task_id,
        "state": "starting",
        "workspace": workspace,
        "harness": launch.harness_id,
    });
    print_value(&receipt, json_output);
    Ok(())
}

async fn supervise(paths: &Paths, launch_path: &Path, json_output: bool) -> Result<()> {
    let launch: LaunchEnvelope = serde_json::from_slice(&fs::read(launch_path)?)?;
    let manifest = Registry::open(&paths.registry)?.load_healthy(&launch.harness_id)?;
    let manifest = if manifest.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1 {
        omp_process_manifest(paths, &launch, &manifest)?
    } else {
        manifest
    };
    let cancel_path = paths.cancel(launch.spec.task_id);
    let pid_path = paths.pid(launch.spec.task_id);
    let mut supervisor = Supervisor::open(&paths.store)?;
    let result = supervisor
        .run_fresh_controlled(launch.spec, &manifest, Some(&cancel_path), Some(&pid_path))
        .await?;
    let _ = fs::remove_file(cancel_path);
    print_value(&serde_json::to_value(result)?, json_output);
    Ok(())
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

fn status(paths: &Paths, task: Option<TaskId>, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    if let Some(task_id) = task {
        let spec = store.task(task_id)?;
        let state = store.attempt_state(task_id)?;
        print_value(
            &json!({"task": spec, "state": format!("{state:?}").to_lowercase()}),
            json_output,
        );
    } else {
        let tasks = store
            .tasks(20)?
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
    let result = store.latest_result(task)?;
    if ack {
        require_owner(&spec.owner_id)?;
        store.acknowledge(&spec.owner_id, result.result_id)?;
    }
    print_value(&serde_json::to_value(result)?, json_output);
    Ok(())
}

fn cancel(paths: &Paths, task: TaskId, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&spec.owner_id)?;
    if spec.route.harness_id == "local.omp" {
        bail!("OMP cancellation is not certified in v1");
    }
    if store.attempt_state(task)? == brgr_protocol::AttemptState::Terminal {
        bail!("task {task} is already terminal");
    }
    fs::write(paths.cancel(task), b"cancel\n")?;
    print_value(
        &json!({"task_id": task, "state": "cancel_requested"}),
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
    require_owner(&spec.owner_id)?;
    let result = store.latest_result(task)?;
    if result.outcome != TerminalOutcome::Candidate {
        bail!("only candidate results can be accepted or rejected");
    }
    let decision = Decision {
        schema: SCHEMA_V1.to_owned(),
        decision_id: DecisionId::new(),
        owner_id: spec.owner_id.clone(),
        task_id: task,
        revision: result.revision,
        result_id: result.result_id,
        result_digest: Store::result_digest(&result)?,
        verdict,
        reason,
    };
    store.record_decision(&decision)?;
    store.acknowledge(&spec.owner_id, result.result_id)?;
    print_value(&serde_json::to_value(decision)?, json_output);
    Ok(())
}

async fn harness(paths: &Paths, command: HarnessCommand, json_output: bool) -> Result<()> {
    let registry = Registry::open(&paths.registry)?;
    match command {
        HarnessCommand::Add { executable } => {
            let receipt = registry.add(&executable).await?;
            print_value(&serde_json::to_value(receipt)?, json_output);
        }
        HarnessCommand::Status { harness } => {
            let health = registry.health(&harness)?;
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
    let store_ok = Store::open(&paths.store).is_ok();
    let registry_ok = Registry::open(&paths.registry).is_ok();
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

fn hook(paths: &Paths, event: HookEvent) -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: HookInput = serde_json::from_str(&input).unwrap_or(HookInput { session_id: None });
    let Some(session_id) = input.session_id else {
        println!("{{}}");
        return Ok(());
    };
    let owner = OwnerId::new(format!("codex:{session_id}"))?;
    let store = Store::open(&paths.store)?;
    if event == HookEvent::SessionStart {
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        store.bind_owner(&owner, &session_id, epoch.max(1))?;
    }
    let pending = store.inbox(&owner, false)?;
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
        "--launcher".to_owned(),
        activated.executable.to_string_lossy().into_owned(),
    ];
    if launch.spec.route.requested_model.is_some() {
        argv.extend(["--model".to_owned(), "${route.model}".to_owned()]);
    }
    if launch.spec.route.requested_effort.is_some() {
        argv.extend(["--effort".to_owned(), "${route.effort}".to_owned()]);
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
            mode: ExecutionMode::OneShot,
        },
        result: ResultSpec {
            source: ResultSource::Stdout,
            media_type: "text/markdown".to_owned(),
            max_bytes: launch.spec.artifact_contract.max_bytes,
            success_exit_codes: vec![0],
        },
        capabilities: BTreeMap::new(),
    })
}

fn run_omp_adapter(
    paths: &Paths,
    prompt_file: &Path,
    workspace: &Path,
    task: TaskId,
    launcher: &Path,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") || env::var_os("HERDR_PANE_ID").is_none() {
        bail!("OMP adapter requires a verified Herdr parent session");
    }
    let short = &task.to_string()[..8];
    let agent = format!("brgr-{short}");
    let task_slug = format!("brgr-{short}");
    let report = paths.runs.join(format!("{task}.omp-report.md"));
    let prompt = fs::read_to_string(prompt_file)?;

    let mut launch = ProcessCommand::new("python");
    launch
        .arg(launcher)
        .args(["default", &agent, "--cwd"])
        .arg(workspace)
        .args(["--task", &task_slug])
        .args(["--reuse-worktree-objective", &task_slug])
        .args(["--reuse-worktree-owner", &agent])
        .arg("--expected-report")
        .arg(&report);
    if let Some(value) = model {
        launch.args(["--model", value]);
    }
    if let Some(value) = effort {
        launch.args(["--effort", value]);
    }
    let launch_output = launch.output()?;
    if !launch_output.status.success() {
        let failure: serde_json::Value =
            serde_json::from_slice(&launch_output.stdout).unwrap_or_else(|_| json!({}));
        if failure.get("detail").and_then(serde_json::Value::as_str)
            == Some("immutable agent session identity is missing")
        {
            recover_omp_contract(&agent, &task_slug, &report, &failure)?;
        } else {
            bail!(
                "OMP launcher preflight failed: {}{}",
                String::from_utf8_lossy(&launch_output.stdout),
                String::from_utf8_lossy(&launch_output.stderr)
            );
        }
    }

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

    loop {
        let observed = ProcessCommand::new("herdr")
            .args(["agent", "get", &agent])
            .output()?;
        if !observed.status.success() {
            bail!("Herdr lost the managed OMP agent {agent}");
        }
        let document: serde_json::Value = serde_json::from_slice(&observed.stdout)?;
        let status = document
            .pointer("/result/agent/agent_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        if status == "blocked" {
            bail!("OMP agent {agent} is blocked on external input");
        }
        if matches!(status, "idle" | "done") && report.is_file() {
            let metadata = fs::symlink_metadata(&report)?;
            if !metadata.file_type().is_file() || metadata.len() == 0 {
                bail!("OMP report is missing or invalid");
            }
            io::stdout().write_all(&fs::read(report)?)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
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
    harness_id: &str,
) -> Result<PathBuf> {
    let source = source.canonicalize()?;
    let root_output = ProcessCommand::new("git")
        .args([
            "-C",
            &source.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()?;
    if !root_output.status.success() {
        return Ok(source);
    }
    let root = PathBuf::from(String::from_utf8(root_output.stdout)?.trim());
    let revision = command_output("git", &["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])?;
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
    let target_parent = paths.worktrees.join(repo_name);
    fs::create_dir_all(&target_parent)?;
    let target = target_parent.join(&short);
    let branch = format!("brgr/task-{short}");
    let status = if harness_id == "local.omp"
        && env::var("HERDR_ENV").as_deref() == Ok("1")
        && env::var_os("HERDR_PANE_ID").is_some()
    {
        ProcessCommand::new("herdr")
            .args(["worktree", "create", "--cwd"])
            .arg(&primary)
            .args(["--branch", &branch, "--base", revision.trim(), "--path"])
            .arg(&target)
            .args([
                "--label",
                &format!("brgr-{short}"),
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
            .arg(revision.trim())
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
    let owner = env::var("BRGR_OWNER_ID")
        .ok()
        .or_else(|| {
            env::var("CODEX_THREAD_ID")
                .ok()
                .map(|id| format!("codex:{id}"))
        })
        .unwrap_or_else(|| "codex:manual".to_owned());
    Ok(OwnerId::new(owner)?)
}

fn require_owner(expected: &OwnerId) -> Result<()> {
    let caller = owner_from_environment()?;
    if &caller != expected {
        bail!("task belongs to {expected}; current caller is {caller}");
    }
    Ok(())
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

fn print_value(value: &serde_json::Value, json_output: bool) {
    if json_output {
        println!("{value}");
    } else if let Ok(pretty) = serde_json::to_string_pretty(value) {
        println!("{pretty}");
    }
}
