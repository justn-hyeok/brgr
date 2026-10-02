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
    io::{self, Read as _, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, TaskId, TaskSpec};
use brgr_runner::{
    ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1,
    PermissionArgv, ProbeSpec, ResultSource, ResultSpec,
};
use brgr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{LaunchEnvelope, Paths, cli::PaneRunArgs, write_json_atomic};
pub(crate) mod lifecycle;
mod native;
mod screens;
pub(crate) use lifecycle::seal_native_report;
pub(crate) use lifecycle::stop_external;
use lifecycle::{PaneReceipt, update_receipt, verify_receipt};
pub(crate) use lifecycle::{
    cleanup_settled, close_leftover_pane, collect_recovered, external_alive, session_status,
};
pub(crate) use native::report_digest as native_digest;
pub(crate) use native::{deliver_owner_notice, send_native_input, serve_session};
use native::{
    deliver_worker_messages, report_digest, send_prompt, spawn_session_server, start_native,
};
use screens::{Screen, ScreenWatch};

/// How often the agent's state is read.
const POLL: Duration = Duration::from_secs(1);
/// How long an agent may sit idle after working without a report before it is
/// reminded once (or, if it wrote the file but never sealed it, sealed for it).
const REPORT_GRACE: Duration = Duration::from_secs(15);
/// How long an agent Herdr never showed working may sit idle without a report
/// before it counts as finished.
const UNSEEN_WORK_GRACE: Duration = Duration::from_mins(1);

/// How long a pane run waits for its calling Codex pane to show the call. The
/// call that started the task ends within seconds, so this is generous.
const CALLER_WAIT: Duration = Duration::from_secs(40);

fn caller_wait() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(millis) = env::var("BRGR_TEST_CALLER_WAIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(millis);
    }
    CALLER_WAIT
}

/// The two report waits, shortened for tests only.
fn report_waits() -> (Duration, Duration) {
    #[cfg(debug_assertions)]
    if let Some(millis) = env::var("BRGR_TEST_REPORT_GRACE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        let wait = Duration::from_millis(millis);
        return (wait, wait);
    }
    (REPORT_GRACE, UNSEEN_WORK_GRACE)
}
/// How long a freshly split pane may take to show its shell prompt. Until it
/// does, Herdr refuses to start an agent there (`agent_pane_busy`).
const SHELL_READY: Duration = Duration::from_secs(20);
const SHELL_POLL: Duration = Duration::from_millis(250);

/// Validates the requested native TUI without changing execution shape.
/// Unsupported options fail before admission; headless requires an explicit flag.
pub(crate) fn require_tui(manifest: &HarnessManifest, spec: &TaskSpec) -> Result<()> {
    let interactive = manifest.launch.interactive.as_ref().context(
        "harness has no activated native TUI recipe; re-register it, or explicitly request --headless"
    )?;
    if interactive.effort_print_only && spec.route.requested_effort.is_some() {
        bail!(
            "this native TUI cannot honour --effort; change the requested option or explicitly request --headless"
        );
    }
    Ok(())
}

/// The process manifest the supervisor runs in place of the harness itself.
pub(crate) fn pane_process_manifest(
    paths: &Paths,
    launch: &LaunchEnvelope,
    activated: &HarnessManifest,
) -> Result<HarnessManifest> {
    let argv = pane_argv(paths, launch, activated)?;
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
            env_allow: activated
                .launch
                .env_allow
                .iter()
                .cloned()
                .chain(
                    [
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
                    .map(str::to_owned),
                )
                // Lets a test shorten the report waits in the detached runner.
                .chain(cfg!(debug_assertions).then(|| "BRGR_TEST_REPORT_GRACE_MS".to_owned()))
                .chain(cfg!(debug_assertions).then(|| "BRGR_TEST_CALLER_WAIT_MS".to_owned()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
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

fn pane_argv(
    paths: &Paths,
    launch: &LaunchEnvelope,
    activated: &HarnessManifest,
) -> Result<Vec<String>> {
    let interactive = activated
        .launch
        .interactive
        .as_ref()
        .context("harness has no interactive launch")?;
    // The agent gets the same permission and model flags as a print-mode run.
    let mut agent_args = interactive.argv.clone();
    if let Some(options) = &launch.calling_options {
        agent_args.extend(options.argv.clone());
    }
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
    if let Some(caller) = &launch.source_pane {
        argv.extend(["--caller".to_owned(), caller.clone()]);
    }
    if interactive.native_host {
        argv.extend([
            "--native-executable".to_owned(),
            activated.executable.to_string_lossy().into_owned(),
        ]);
    }
    for arg in agent_args {
        argv.push(format!("--agent-arg={arg}"));
    }
    if launch.keep_pane {
        argv.push("--keep-pane".to_owned());
    }
    Ok(argv)
}

/// Runs the agent in a pane and prints its report.
pub(crate) fn run_pane_adapter(paths: &Paths, run: &PaneRunArgs) -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") && run.caller.is_none() {
        bail!("pane mode requires a Herdr caller");
    }
    let caller = match run.caller.as_deref() {
        Some(marker) if crate::caller_pane::pending_session(marker).is_some() => {
            let session = crate::caller_pane::pending_session(marker).unwrap_or_default();
            crate::caller_pane::wait_for_session(session, caller_wait()).context(
                "could not find the calling Codex pane: no Codex pane shows this session's brgr call",
            )?
        }
        Some(pane) => pane.to_owned(),
        None => crate::caller_pane::verified().context("Herdr caller pane id is absent")?,
    };
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
    let report_dir = run
        .workspace
        .join(format!(".brgr/tasks/{}-r{}", run.task, run.revision));
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
            task: Some(run.task),
            revision: run.revision,
            attempt: env::var("BRGR_PARENT_ATTEMPT_ID")
                .ok()
                .and_then(|id| id.parse().ok()),
            name: Some(if run.native_executable.is_some() {
                pane.clone()
            } else {
                name.clone()
            }),
            phase: "starting".to_owned(),
            report: Some(report.clone()),
            terminal: herdr.call(&["pane", "get", &pane]).ok().and_then(|v| {
                v.pointer("/result/pane/terminal_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
            ..PaneReceipt::default()
        },
    )?;
    eprintln!("brgr pane mode · {} agent {name} in pane {pane}", run.kind);
    let mut notices = Notices {
        paths,
        task: run.task,
        outstanding: Vec::new(),
    };
    let outcome = drive(&herdr, &name, &pane, run, &report, &mut notices);
    let finished = outcome.and_then(|()| {
        let bytes = read_report(&report, spec.artifact_contract.max_bytes)?;
        let digest = report_digest(&bytes);
        update_receipt(&receipt, |value| {
            "finished".clone_into(&mut value.phase);
            value.report_digest = Some(digest);
            value.finished_at = Some(lifecycle::now_seconds());
        })?;
        io::stdout().write_all(&bytes)?;
        Ok(())
    });
    if finished.is_ok() {
        update_receipt(&receipt, |value| "finished".clone_into(&mut value.phase))?;
    } else if !run.keep_pane && !report.is_file() {
        close_leftover_pane(paths, run.task, run.revision, false);
    }
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
    if run.kind == "claude"
        && let Ok(attempt) = env::var("BRGR_PARENT_ATTEMPT_ID")
    {
        start.extend(["--session-id".to_owned(), attempt]);
    }
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
            // The agent is up but not ready: a trust prompt, a new MCP server,
            // or a screen Herdr cannot classify. `settle` answers it.
            Err(HerdrFailure::Code(code, _))
                if code == "agent_not_ready"
                    || (code == "timeout" && herdr.status(name).is_ok()) =>
            {
                break;
            }
            Err(failure) => bail!("Herdr could not start the {} agent: {failure}", run.kind),
        }
    }
    // Herdr can call an agent ready while it shows a menu: Codex opened on an
    // update offer and was reported idle. `settle` answers what a rule covers.
    settle(herdr, name, pane, run, notices)
}

/// Waits until the agent is ready, pressing what the screen rules say and
/// failing, with the screen's text, when a screen no rule covers outlasts the
/// deadline.
fn settle(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    notices: &mut Notices<'_>,
) -> Result<()> {
    let mut watch = ScreenWatch::new(&run.kind);
    let mut announced = false;
    loop {
        let status = herdr.status(name)?;
        match screens::resolve(herdr, pane, run, notices.paths, Some(&status))? {
            Screen::Resolve { .. } => watch.reset(),
            Screen::Unknown(text) => {
                if !announced {
                    notices.tell(
                        &run.kind,
                        pane,
                        &format!(
                            "The {} agent in Herdr pane {pane} is on a screen no brgr rule answers: {text}. Use brgr input if it needs a key.",
                            run.kind
                        ),
                    );
                    announced = true;
                }
                watch.observe(&Screen::Unknown(text))?;
            }
            Screen::Clear => match status.as_str() {
                "idle" | "done" => {
                    notices.withdraw();
                    return Ok(());
                }
                "working" => watch.reset(),
                _ => watch.observe_blocked(herdr.screen(pane).as_deref())?,
            },
        }
        thread::sleep(POLL);
    }
}

fn drive(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    report: &Path,
    notices: &mut Notices<'_>,
) -> Result<()> {
    if run.native_executable.is_some() {
        start_native(herdr, pane, run, notices)?;
    } else {
        start_agent(herdr, name, pane, run, notices)?;
    }

    let prompt = fs::read_to_string(&run.prompt_file)?;
    let instruction = result_instruction(notices.paths, run, &prompt, report)?;
    send_prompt(herdr, name, pane, run, &instruction, notices.paths)?;
    spawn_session_server(notices.paths, run.task)?;
    set_phase(notices.paths, run, "working");
    eprintln!("brgr pane mode · prompted the agent");

    let mut blocked = false;
    let mut watch = ScreenWatch::new(&run.kind);
    let mut idle = IdleReport::new();
    loop {
        let receipt: PaneReceipt = serde_json::from_slice(&fs::read(
            notices.paths.pane_receipt(run.task, run.revision),
        )?)?;
        if let Some(expected) = receipt.report_digest {
            let bytes = read_report(
                report,
                Store::open(&notices.paths.store)?
                    .task(run.task)?
                    .artifact_contract
                    .max_bytes,
            )?;
            if report_digest(&bytes) != expected {
                bail!("completed native report changed");
            }
            return Ok(());
        }
        let status = if run.native_executable.is_some() {
            herdr.status(pane).unwrap_or_else(|_| "unknown".to_owned())
        } else {
            herdr.status(name)?
        };
        if run.native_executable.is_some() || matches!(status.as_str(), "idle" | "done") {
            deliver_worker_messages(herdr, name, pane, run, notices.paths)?;
        }
        // Whatever stopped the agent was dealt with in the pane.
        if blocked && status != "blocked" {
            notices.withdraw();
            blocked = false;
            set_phase(notices.paths, run, "working");
        }
        match status.as_str() {
            // Only `blocked` is acted on while the agent works. Herdr never
            // classifies some harnesses (GJC, Command Code), which report
            // `unknown` for their whole life, and their own output can look
            // like a menu.
            "blocked" => match screens::resolve(herdr, pane, run, notices.paths, Some(&status))? {
                Screen::Resolve { .. } => {
                    notices.withdraw();
                    blocked = false;
                    set_phase(notices.paths, run, "working");
                    watch.reset();
                    thread::sleep(POLL);
                    continue;
                }
                Screen::Unknown(text) => {
                    if !blocked {
                        notices.blocked(&run.kind, pane, "while working");
                        blocked = true;
                        set_phase(notices.paths, run, "awaiting_input");
                    }
                    watch.observe(&Screen::Unknown(text))?;
                }
                Screen::Clear => {
                    if !blocked {
                        notices.blocked(&run.kind, pane, "while working");
                        blocked = true;
                        set_phase(notices.paths, run, "awaiting_input");
                    }
                    watch.observe_blocked(herdr.screen(pane).as_deref())?;
                }
            },
            // The report is the completion signal. The runner removed any old
            // one before starting, and Herdr may never have shown this agent
            // working: Cursor went straight from unknown to idle once it had
            // finished between two polls.
            "idle" | "done" if run.native_executable.is_none() && report.is_file() => return Ok(()),
            "working" => {
                idle.worked();
                watch.reset();
            }
            "idle" | "done" => {
                idle.settle(herdr, name, pane, run, report, notices)?;
                watch.reset();
            }
            _ => watch.reset(),
        }
        thread::sleep(POLL);
    }
}

/// An agent that stopped without its report being sealed. It seals a file the
/// agent did write; otherwise it reminds the agent once, then fails with the
/// cause instead of waiting for the deadline.
struct IdleReport {
    report_grace: Duration,
    unseen_work_grace: Duration,
    prompted: Instant,
    worked: bool,
    reminded: bool,
    idle_since: Option<Instant>,
}

impl IdleReport {
    fn new() -> Self {
        let (report_grace, unseen_work_grace) = report_waits();
        Self {
            report_grace,
            unseen_work_grace,
            prompted: Instant::now(),
            worked: false,
            reminded: false,
            idle_since: None,
        }
    }

    fn worked(&mut self) {
        self.worked = true;
        self.idle_since = None;
    }

    fn settle(
        &mut self,
        herdr: &Herdr,
        name: &str,
        pane: &str,
        run: &PaneRunArgs,
        report: &Path,
        notices: &Notices<'_>,
    ) -> Result<()> {
        if !self.worked && self.prompted.elapsed() < self.unseen_work_grace {
            return Ok(());
        }
        let since = *self.idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() < self.report_grace {
            return Ok(());
        }
        if report.is_file() {
            return seal_native_report(notices.paths, run.task, run.revision);
        }
        if self.reminded {
            bail!(
                "the agent finished without writing its report to {}",
                report.display()
            );
        }
        let nudge = format!(
            "Write your complete final answer as Markdown to {} now. After completing that file, record completion with \"$BRGR_BIN\" __seal-report {} {}.",
            report.display(),
            run.task,
            run.revision
        );
        send_prompt(herdr, name, pane, run, &nudge, notices.paths)?;
        self.reminded = true;
        self.worked = false;
        self.idle_since = None;
        Ok(())
    }
}

/// Shows what the agent is doing in `brgr status`. Display only: a missed
/// update never changes a run.
fn set_phase(paths: &Paths, run: &PaneRunArgs, phase: &str) {
    let _ = update_receipt(&paths.pane_receipt(run.task, run.revision), |value| {
        phase.clone_into(&mut value.phase);
    });
}

fn result_instruction(
    paths: &Paths,
    run: &PaneRunArgs,
    prompt: &str,
    report: &Path,
) -> Result<String> {
    let spec = Store::open(&paths.store)?.task(run.task)?;
    if spec.permission == Some(brgr_protocol::PermissionLevel::ReadOnly) {
        let attempt: AttemptId = env::var("BRGR_PARENT_ATTEMPT_ID")?.parse()?;
        let (begin, end) = crate::native_result::marker(attempt);
        Ok(format!(
            "{prompt}\n\nBRGR RESULT CHANNEL\nDo not modify source files to return your answer. When this task is finished, publish the complete answer with \"$BRGR_BIN\" report {} --body <answer>. This only publishes orchestration output. Claude/Cursor can also return the answer directly in this TUI: put {begin} on a line before the final answer and {end} on a line after it. Use these markers only for the completed task, not an interim reply. A native response hook captures that answer without a worker file write.\n",
            run.task
        ))
    } else {
        Ok(format!(
            "{prompt}\n\nWhen you have finished, write your complete final answer as Markdown to this file, and write it only once you are done:\n{}\n\nAfter completing that file, record completion with \"$BRGR_BIN\" __seal-report {} {}. This records its digest for recovery; the owner handles the result.\n",
            report.display(),
            run.task,
            run.revision
        ))
    }
}

/// Whether the pane shows a numbered menu with its first option highlighted
/// (`› 1.`, `❯ 1.`), where Enter would choose that option. A banner without a
/// menu, such as "update available", does not count: it takes no Enter and may
/// never go away.
fn menu_on_screen(herdr: &Herdr, pane: &str) -> bool {
    herdr.screen(pane).is_some_and(|screen| {
        screen.lines().any(|line| {
            let line = line.trim_start();
            ["›", "❯", ">", "▸"].iter().any(|cursor| {
                line.strip_prefix(cursor)
                    .map(str::trim_start)
                    .is_some_and(numbered_option)
            })
        })
    })
}

fn numbered_option(rest: &str) -> bool {
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.starts_with('.') || rest.starts_with(')')
}

/// Native input notices this run sent its owner about the pane. Each is withdrawn
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
                    .and_then(|store| {
                        store.acknowledge_message(
                            self.task,
                            attempt,
                            brgr_store::MessageDirection::WorkerToOwner,
                            &message,
                        )
                    })
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
             a folder trust or MCP prompt). This is native input, not a mailbox question. \
             Read the pane and use brgr input TASK --key KEY or --text TEXT. The task continues once unblocked."
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
            "--json", "message", "send", &task, "--to", "owner", "--kind", "note", "--body",
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
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(report)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("the agent's report is not a regular file within the artifact limit");
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > max_bytes {
        bail!("the agent's report exceeds the artifact limit");
    }
    Ok(bytes)
}

/// A Herdr CLI call that failed, with Herdr's error code when it gave one.
#[derive(Debug)]
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

pub(crate) struct Herdr {
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

    /// The pane's visible text, or `None` when Herdr cannot read it.
    fn screen(&self, pane: &str) -> Option<String> {
        let mut command = Command::new(&self.binary);
        if let Some(session) = &self.session {
            command.arg("--session").arg(session);
        }
        let output = command
            .args([
                "pane", "read", pane, "--source", "visible", "--lines", "300",
            ])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
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
            if output.stdout.iter().all(u8::is_ascii_whitespace) {
                return Ok(Value::Null);
            }
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
        let placement = crate::config::Config::load(
            &crate::Paths::new(env::var_os("BRGR_HOME").map(PathBuf::from))?.config,
        )?
        .herdr
        .worker_placement;
        let mut args: Vec<String> = if placement == crate::config::WorkerPlacement::Tab {
            vec![
                "tab".into(),
                "create".into(),
                "--workspace".into(),
                caller
                    .split(':')
                    .next()
                    .context("caller workspace missing")?
                    .into(),
                "--label".into(),
                "brgr-work".into(),
                "--cwd".into(),
                workspace.to_string_lossy().into_owned(),
                "--no-focus".into(),
            ]
        } else {
            vec![
                "pane".into(),
                "split".into(),
                "--pane".into(),
                caller.into(),
                "--direction".into(),
                "right".into(),
                "--cwd".into(),
                workspace.to_string_lossy().into_owned(),
                "--no-focus".into(),
            ]
        };
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
            .or_else(|| opened.pointer("/result/root_pane/pane_id"))
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn an_empty_successful_herdr_acknowledgement_is_a_valid_transport_reply() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("herdr");
        fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let herdr = Herdr {
            binary: binary.into_os_string(),
            session: None,
        };
        assert_eq!(
            herdr
                .call(&["pane", "run", "w1:p2", "owned bootstrap"])
                .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn failed_pane_close_keeps_ownership_for_a_later_retry() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let binary = temp.path().join("herdr");
        fs::write(&binary, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let task = TaskId::new();
        let path = paths.pane_receipt(task, 1);
        write_json_atomic(
            &path,
            &PaneReceipt {
                pane: "w1:p2".into(),
                binary,
                session: None,
                ..PaneReceipt::default()
            },
        )
        .unwrap();
        close_leftover_pane(&paths, task, 1, false);
        assert!(
            path.exists(),
            "a failed close discarded the only ownership receipt"
        );
    }
}
