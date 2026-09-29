//! The `omp-role` adapter: launching OMP in a Herdr pane and recovering its report.

use std::{
    env,
    fs::{self},
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    thread,
    time::Duration,
};

use crate::{LaunchEnvelope, Paths, pane_cleanup, write_json_atomic};
use anyhow::{Context, Result, bail};
use brgr_protocol::TaskId;
use brgr_runner::{
    ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1, PROCESS_ADAPTER_V1, ProbeSpec,
    ResultSource, ResultSpec,
};
use brgr_store::Store;
use serde_json::json;

pub(crate) fn omp_process_manifest(
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
            model_catalog: None,
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
pub(crate) struct OmpOptions<'a> {
    pub(crate) revision: u32,
    pub(crate) model: Option<&'a str>,
    pub(crate) effort: Option<&'a str>,
    pub(crate) keep_pane: bool,
}

pub(crate) fn run_omp_adapter(
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

pub(crate) fn fresh_omp_report_path(paths: &Paths, task: TaskId, revision: u32) -> Result<PathBuf> {
    let report = paths.runs.join(format!("{task}-r{revision}.omp-report.md"));
    match fs::symlink_metadata(&report) {
        Ok(_) => bail!("fresh OMP report path already exists for this task revision"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(report),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn get_omp_agent(agent: &str) -> Result<serde_json::Value> {
    let observed = ProcessCommand::new("herdr")
        .args(["agent", "get", agent])
        .output()?;
    if !observed.status.success() {
        bail!("Herdr lost the managed OMP agent {agent}");
    }
    Ok(serde_json::from_slice(&observed.stdout)?)
}

pub(crate) fn omp_spawn_matches_initial(
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

pub(crate) fn omp_completion_ready(
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

pub(crate) fn read_bounded_regular_report(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
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

pub(crate) fn launch_omp(
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

pub(crate) fn recover_omp_contract(
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
