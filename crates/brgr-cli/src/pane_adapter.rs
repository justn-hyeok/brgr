//! Pane mode: a harness runs as its own interactive TUI in a Herdr pane beside
//! the caller, so the work is visible while it happens, and brgr seals the
//! report the agent writes.
//!
//! Like the `omp-role` adapter, this is a process harness: the supervisor runs
//! `brgr __pane-run`, which drives Herdr and prints the final report to stdout.
//! Deadlines, cancellation, and sealing are therefore the ordinary ones.
//!
//! The agent's own screen is not the result. A TUI draws on the terminal's
//! alternate screen, which scrollback does not keep, so the agent is asked to
//! write its final answer to a file, and that file is what gets sealed.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, PermissionLevel, TaskId, TaskSpec};
use brgr_runner::{
    ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1,
    PermissionArgv, ProbeSpec, ResultSource, ResultSpec,
};
use brgr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{LaunchEnvelope, Paths, cli::PaneRunArgs, write_json_atomic};

/// How often the agent's state is read.
const POLL: Duration = Duration::from_secs(1);
/// How long an agent may sit idle after working without having written its
/// report before it is reminded once.
const REPORT_GRACE: Duration = Duration::from_secs(15);
/// How long a freshly split pane may take to show its shell prompt. Until it
/// does, Herdr refuses to start an agent there (`agent_pane_busy`).
const SHELL_READY: Duration = Duration::from_secs(20);
/// How long an agent Herdr never showed working may sit idle without a report
/// before it is reminded, as one that was seen working is after `REPORT_GRACE`.
const UNSEEN_WORK_GRACE: Duration = Duration::from_mins(1);
const SHELL_POLL: Duration = Duration::from_millis(250);

/// Whether a task should run in pane mode: inside Herdr, with a harness that
/// declares an interactive launch, and not read-only — an agent that may not
/// write cannot write its report. A requested effort the interactive command
/// cannot take keeps the task headless, where the effort is honoured.
pub(crate) fn applies(manifest: &HarnessManifest, spec: &TaskSpec) -> bool {
    env::var("HERDR_ENV").as_deref() == Ok("1")
        && env::var_os("HERDR_PANE_ID").is_some_and(|pane| !pane.is_empty())
        && manifest
            .launch
            .interactive
            .as_ref()
            .is_some_and(|interactive| {
                !(interactive.effort_print_only && spec.route.requested_effort.is_some())
            })
        && spec.permission != Some(PermissionLevel::ReadOnly)
}

/// The process manifest the supervisor runs in place of the harness itself.
pub(crate) fn pane_process_manifest(
    paths: &Paths,
    launch: &LaunchEnvelope,
    activated: &HarnessManifest,
) -> Result<HarnessManifest> {
    let interactive = activated
        .launch
        .interactive
        .as_ref()
        .context("harness has no interactive launch")?;
    // The agent gets the same permission and model flags as a print-mode run.
    let mut agent_args = interactive.argv.clone();
    agent_args.extend_from_slice(activated.permission_arguments(launch.spec.permission)?);
    if launch.spec.route.requested_model.is_some() {
        agent_args.extend(activated.launch.model_argv.iter().cloned());
    }
    if launch.spec.route.requested_effort.is_some() {
        agent_args.extend(activated.launch.effort_argv.iter().cloned());
    }
    let mut argv = vec![
        "--home".to_owned(),
        paths.home.to_string_lossy().into_owned(),
        "__pane-run".to_owned(),
        "--prompt-file".to_owned(),
        "${input.prompt_file}".to_owned(),
        "--workspace".to_owned(),
        "${task.workspace}".to_owned(),
        "--task".to_owned(),
        launch.spec.task_id.to_string(),
        "--revision".to_owned(),
        launch.spec.revision.to_string(),
        "--kind".to_owned(),
        interactive.herdr_kind.clone(),
    ];
    for arg in agent_args {
        argv.push(format!("--agent-arg={arg}"));
    }
    if launch.keep_pane {
        argv.push("--keep-pane".to_owned());
    }
    // The level is already in the agent's arguments, so every level is
    // honoured here with no further flag.
    let every_level = || Some(Vec::new());
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        // The same harness, run in a pane: keeping its id keeps the task's
        // route check honest instead of adding another exemption to it.
        id: activated.id.clone(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable: env::current_exe()?,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: None,
        },
        launch: LaunchSpec {
            argv,
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: [
                "HOME",
                "PATH",
                "LANG",
                "TMPDIR",
                "USER",
                "HERDR_ENV",
                "HERDR_PANE_ID",
                "HERDR_WORKSPACE_ID",
                "HERDR_SESSION",
                "HERDR_SOCKET_PATH",
                "HERDR_BIN_PATH",
            ]
            .map(str::to_owned)
            .to_vec(),
            // The pane, and the agent in it, outlive this process.
            mode: ExecutionMode::DelegatedExternal,
            permission_argv: PermissionArgv {
                full: every_level(),
                edits: every_level(),
                read_only: every_level(),
            },
            interactive: None,
        },
        result: ResultSpec {
            source: ResultSource::Stdout,
            media_type: activated.result.media_type.clone(),
            max_bytes: launch.spec.artifact_contract.max_bytes,
            success_exit_codes: vec![0],
        },
        capabilities: activated.capabilities.clone(),
    })
}

/// Runs the agent in a pane and prints its report.
pub(crate) fn run_pane_adapter(paths: &Paths, run: &PaneRunArgs) -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("pane mode requires a Herdr caller");
    }
    let caller = env::var("HERDR_PANE_ID").context("Herdr caller pane id is absent")?;
    let spec = Store::open(&paths.store)?.task(run.task)?;
    if spec.revision != run.revision {
        bail!("pane runner revision differs from the admitted task revision");
    }
    let herdr = Herdr::locate();
    let short = &run.task.to_string()[..8];
    let name = if run.revision == 1 {
        format!("brgr-{short}")
    } else {
        format!("brgr-{short}-r{}", run.revision)
    };
    let report_dir = run.workspace.join(".brgr");
    let report = report_dir.join("report.md");
    fs::create_dir_all(&report_dir)?;
    let _ = fs::remove_file(&report);

    let pane = herdr.open_pane(&caller, &run.workspace)?;
    let receipt = paths.pane_receipt(run.task, run.revision);
    write_json_atomic(
        &receipt,
        &PaneReceipt {
            pane: pane.clone(),
            binary: PathBuf::from(&herdr.binary),
            session: herdr.session.clone(),
        },
    )?;
    eprintln!("brgr pane mode · {} agent {name} in pane {pane}", run.kind);
    let mut notices = Notices {
        paths,
        task: run.task,
        outstanding: Vec::new(),
    };
    let outcome = drive(
        &herdr,
        &name,
        &pane,
        run,
        &report,
        &spec.objective,
        &mut notices,
    );
    let finished = outcome.and_then(|()| {
        let bytes = read_report(&report, spec.artifact_contract.max_bytes)?;
        io::stdout().write_all(&bytes)?;
        Ok(())
    });
    // The sealed copy is the record. Left behind, the file would also make
    // `brgr prune` keep this worktree for holding an ignored path.
    let _ = fs::remove_file(&report);
    let _ = fs::remove_dir(&report_dir);
    // A failed run closes its pane too: the task is over, and an agent left
    // behind there would keep running unobserved.
    if !run.keep_pane {
        let _ = herdr.call(&["pane", "close", &pane]);
    }
    let _ = fs::remove_file(receipt);
    finished
}

/// Starts the agent in the pane, waiting out a slow shell and, through the
/// owner, any screen that stops it before it is ready.
fn start_agent(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    notices: &mut Notices<'_>,
) -> Result<()> {
    let mut start: Vec<String> = [
        "agent",
        "start",
        name,
        "--kind",
        &run.kind,
        "--pane",
        pane,
        "--timeout",
        "60000",
        "--",
    ]
    .map(str::to_owned)
    .to_vec();
    start.extend(run.agent_args.iter().cloned());
    let opened = Instant::now();
    loop {
        match herdr.call_owned(&start) {
            Ok(_) => break,
            // The new pane's shell has not drawn its prompt yet.
            Err(HerdrFailure::Code(code, _))
                if code == "agent_pane_busy" && opened.elapsed() < SHELL_READY =>
            {
                thread::sleep(SHELL_POLL);
            }
            // Startup stopped at an approval — a folder trust prompt, a new MCP
            // server. The owner answers it in the pane; the run waits.
            Err(HerdrFailure::Code(code, _)) if code == "agent_not_ready" => {
                notices.blocked(&run.kind, pane, "while starting");
                wait_ready(herdr, name)?;
                notices.withdraw();
                break;
            }
            // The agent is up but on a screen Herdr cannot classify, so it is
            // neither ready nor blocked: Cline opens with a product notice that
            // waits for a key. Only a person should dismiss it; the run waits.
            Err(HerdrFailure::Code(code, _)) if code == "timeout" && herdr.status(name).is_ok() => {
                notices.tell(
                    &run.kind,
                    pane,
                    &format!(
                        "The {} agent in Herdr pane {pane} started but is on a screen Herdr does \
                         not recognize, such as a product notice or a first-run question. Look at \
                         that pane and deal with it; the task continues once the agent is ready.",
                        run.kind
                    ),
                );
                wait_ready(herdr, name)?;
                notices.withdraw();
                break;
            }
            Err(failure) => bail!("Herdr could not start the {} agent: {failure}", run.kind),
        }
    }
    Ok(())
}

fn drive(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    report: &Path,
    objective: &str,
    notices: &mut Notices<'_>,
) -> Result<()> {
    start_agent(herdr, name, pane, run, notices)?;

    let prompt = fs::read_to_string(&run.prompt_file)?;
    let instruction = format!(
        "{prompt}\n\nWhen you have finished, write your complete final answer as Markdown to this \
         file, and write it only once you are done:\n{}\n",
        report.display()
    );
    herdr
        .call(&["agent", "prompt", name, &instruction])
        .map_err(|failure| anyhow::anyhow!("Herdr did not accept the prompt: {failure}"))?;
    eprintln!("brgr pane mode · prompted: {}", first_line(objective));

    let prompted = Instant::now();
    let mut worked = false;
    let mut blocked = false;
    let mut reminded = false;
    let mut idle_since: Option<Instant> = None;
    loop {
        let status = herdr.status(name)?;
        // Whatever stopped the agent was dealt with in the pane.
        if blocked && status != "blocked" {
            notices.withdraw();
            blocked = false;
        }
        match status.as_str() {
            "working" => {
                worked = true;
                idle_since = None;
            }
            "blocked" => {
                if !blocked {
                    notices.blocked(&run.kind, pane, "while working");
                    blocked = true;
                }
                idle_since = None;
            }
            // The report is the completion signal. The runner removed any old
            // one before starting, and Herdr may never have shown this agent
            // working: Cursor went straight from unknown to idle once it had
            // finished between two polls.
            "idle" | "done" if report.is_file() => return Ok(()),
            "idle" | "done" if worked || prompted.elapsed() >= UNSEEN_WORK_GRACE => {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= REPORT_GRACE {
                    if reminded {
                        bail!(
                            "the agent finished without writing its report to {}",
                            report.display()
                        );
                    }
                    let nudge = format!(
                        "Write your complete final answer as Markdown to {} now.",
                        report.display()
                    );
                    herdr
                        .call(&["agent", "prompt", name, &nudge])
                        .map_err(|failure| {
                            anyhow::anyhow!("Herdr did not accept the reminder: {failure}")
                        })?;
                    reminded = true;
                    worked = false;
                    idle_since = None;
                }
            }
            _ => {}
        }
        thread::sleep(POLL);
    }
}

fn wait_ready(herdr: &Herdr, name: &str) -> Result<()> {
    loop {
        if matches!(herdr.status(name)?.as_str(), "idle" | "done") {
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

/// Tells the owner the agent is waiting on an answer only a person can give,
/// through the question channel, which reaches an idle Codex owner by itself.
/// The questions this run sent its owner about its pane. Each one is withdrawn
/// once the agent is ready again: someone dealt with it in the pane, and an
/// unanswered question would otherwise hold the finished result back.
struct Notices<'a> {
    paths: &'a Paths,
    task: TaskId,
    outstanding: Vec<String>,
}

impl Notices<'_> {
    fn blocked(&mut self, kind: &str, pane: &str, when: &str) {
        self.outstanding.extend(announce_blocked(kind, pane, when));
    }

    fn tell(&mut self, kind: &str, pane: &str, body: &str) {
        self.outstanding.extend(tell_owner(kind, pane, body));
    }

    fn withdraw(&mut self) {
        let attempt = env::var("BRGR_PARENT_ATTEMPT_ID")
            .ok()
            .and_then(|id| id.parse::<AttemptId>().ok());
        for message in self.outstanding.drain(..) {
            let withdrawn = attempt.ok_or(()).and_then(|attempt| {
                Store::open(&self.paths.store)
                    .and_then(|store| store.withdraw_question(self.task, attempt, &message))
                    .map_err(drop)
            });
            if withdrawn.is_err() {
                eprintln!("brgr pane mode · could not withdraw question {message}");
            }
        }
    }
}

fn announce_blocked(kind: &str, pane: &str, when: &str) -> Option<String> {
    tell_owner(
        kind,
        pane,
        &format!(
            "The {kind} agent in Herdr pane {pane} is waiting for an approval {when} (for example \
             a folder trust or MCP prompt). Answer it in that pane; the task continues once it is \
             unblocked."
        ),
    )
}

/// Sends the owner a question and returns its id, or prints it when this run
/// has no owner channel.
fn tell_owner(kind: &str, pane: &str, body: &str) -> Option<String> {
    let (Some(bin), Some(task)) = (
        env::var_os("BRGR_BIN"),
        env::var("BRGR_PARENT_TASK_ID").ok(),
    ) else {
        eprintln!("brgr pane mode · the {kind} agent in pane {pane} needs attention: {body}");
        return None;
    };
    let sent = Command::new(bin)
        .args([
            "--json", "message", "send", &task, "--to", "owner", "--kind", "question", "--body",
        ])
        .arg(body)
        .output();
    let id = sent
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .and_then(|message| message["message_id"].as_str().map(str::to_owned));
    if id.is_none() {
        eprintln!("brgr pane mode · the {kind} agent in pane {pane} needs attention: {body}");
    }
    id
}

fn read_report(report: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(report)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("the agent's report is not a regular file within the artifact limit");
    }
    Ok(fs::read(report)?)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// A Herdr CLI call that failed, with Herdr's error code when it gave one.
enum HerdrFailure {
    Code(String, String),
    Other(String),
}

impl std::fmt::Display for HerdrFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(code, message) => write!(formatter, "{code}: {message}"),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

/// Records the pane a run opened until the run closes it. Cancellation kills
/// the runner before it can, so the supervisor closes whatever is still
/// recorded once the attempt is over.
#[derive(Serialize, Deserialize)]
struct PaneReceipt {
    pane: String,
    binary: PathBuf,
    session: Option<String>,
}

/// Closes the pane a pane-mode run left open when it was stopped — cancelled,
/// out of time, or killed — and forgets it. A run asked to keep its pane keeps
/// it. Best effort: the task is over either way.
pub(crate) fn close_leftover_pane(paths: &Paths, task: TaskId, revision: u32, keep_pane: bool) {
    let path = paths.pane_receipt(task, revision);
    let Some(receipt) = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PaneReceipt>(&bytes).ok())
    else {
        return;
    };
    let _ = fs::remove_file(&path);
    if keep_pane {
        return;
    }
    let herdr = Herdr {
        binary: receipt.binary.into_os_string(),
        session: receipt.session,
    };
    if herdr.call(&["pane", "close", &receipt.pane]).is_ok() {
        eprintln!(
            "brgr pane mode · closed pane {} left by a stopped run",
            receipt.pane
        );
    }
}

struct Herdr {
    binary: OsString,
    session: Option<String>,
}

impl Herdr {
    fn locate() -> Self {
        let binary = env::var_os("HERDR_BIN_PATH")
            .filter(|path| PathBuf::from(path).is_absolute())
            .unwrap_or_else(|| OsString::from("herdr"));
        let session = env::var("HERDR_SESSION")
            .ok()
            .filter(|session| !session.trim().is_empty());
        Self { binary, session }
    }

    fn call(&self, args: &[&str]) -> std::result::Result<Value, HerdrFailure> {
        self.call_owned(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    fn call_owned(&self, args: &[String]) -> std::result::Result<Value, HerdrFailure> {
        let mut command = Command::new(&self.binary);
        if let Some(session) = &self.session {
            command.arg("--session").arg(session);
        }
        let output = command
            .args(args)
            .output()
            .map_err(|error| HerdrFailure::Other(error.to_string()))?;
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).map_err(|error| {
                HerdrFailure::Other(format!("unreadable Herdr response: {error}"))
            });
        }
        let error: Option<Value> = serde_json::from_slice(&output.stderr).ok();
        match error.as_ref().and_then(|value| value.get("error")) {
            Some(error) => Err(HerdrFailure::Code(
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )),
            None => Err(HerdrFailure::Other(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            )),
        }
    }

    /// A new shell pane to the right of the caller, in the task workspace,
    /// without taking focus. The worker identity goes with it, so the agent
    /// can message its owner like any other worker.
    fn open_pane(&self, caller: &str, workspace: &Path) -> Result<String> {
        let mut args: Vec<String> = vec![
            "pane".into(),
            "split".into(),
            "--pane".into(),
            caller.into(),
            "--direction".into(),
            "right".into(),
            "--cwd".into(),
            workspace.to_string_lossy().into_owned(),
            "--no-focus".into(),
        ];
        for name in [
            "BRGR_HOME",
            "BRGR_BIN",
            "BRGR_PARENT_TASK_ID",
            "BRGR_PARENT_ATTEMPT_ID",
            "BRGR_OWNER_ID",
            "BRGR_SESSION_ID",
        ] {
            if let Ok(value) = env::var(name) {
                args.push("--env".into());
                args.push(format!("{name}={value}"));
            }
        }
        let opened = self
            .call_owned(&args)
            .map_err(|failure| anyhow::anyhow!("Herdr could not open a pane: {failure}"))?;
        opened
            .pointer("/result/pane/pane_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Herdr did not report the new pane")
    }

    fn status(&self, name: &str) -> Result<String> {
        let agent = self
            .call(&["agent", "get", name])
            .map_err(|failure| anyhow::anyhow!("the agent is gone: {failure}"))?;
        Ok(agent
            .pointer("/result/agent/agent_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned())
    }
}
