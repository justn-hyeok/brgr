//! Harness registration, integration, doctor, and cleanup commands.

use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::PathBuf,
};

use crate::cli::{
    AddHarnessArgs, ClaudeCommand, CleanupCommand, CodexCommand, CursorCommand, HarnessCommand,
    IntegrateCommand,
};
use crate::human::{self, DoctorProblem};
use crate::supervision::reconcile_pending;
use crate::{
    Paths, codex_integration, pane_cleanup, print_value, require_owner, statusline_install,
};
use anyhow::{Context, Result, bail};
use brgr_registry::{ActivationReceipt, Health, Registry};
use brgr_runner::HarnessManifest;
use brgr_store::Store;
use serde_json::json;

pub(crate) async fn harness(
    paths: &Paths,
    command: HarnessCommand,
    json_output: bool,
) -> Result<()> {
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
        HarnessCommand::Recertify {
            harness,
            workspace,
            prompt,
        } => {
            let receipt = recertify_harness(&registry, &harness, workspace, prompt).await?;
            print_value(&serde_json::to_value(receipt)?, json_output);
        }
        HarnessCommand::Status { harness } => {
            let configured = crate::config::Config::load(&paths.config)?.defaults.harness;
            let harness = match harness {
                Some(harness) => harness,
                None => default_harness(&registry, configured.as_deref())?,
            };
            let health = registry.health_probed(&harness).await?;
            let action = registry
                .recertify_action_for(&harness)
                .unwrap_or_else(|_| recertify_action_fallback());
            print_value(
                &harness_health_value("harness", &harness, health, &action),
                json_output,
            );
        }
    }
    Ok(())
}

pub(crate) async fn add_harness(
    registry: &Registry,
    args: AddHarnessArgs,
) -> Result<ActivationReceipt> {
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
    let probed = registry.health_probed(&receipt.harness_id).await?;
    require_healthy_harness(
        &receipt.harness_id,
        probed,
        &registry
            .recertify_action_for(&receipt.harness_id)
            .unwrap_or_else(|_| recertify_action_fallback()),
    )?;
    Ok(receipt)
}

/// Default scratch prompt for a re-certification. Any nonempty, successful
/// answer certifies, so the shortest one keeps the paid run cheap.
pub(crate) const RECERTIFY_PROMPT: &str = "say ok";

/// Registers a harness again at the path it was certified with.
pub(crate) async fn recertify_harness(
    registry: &Registry,
    harness: &str,
    workspace: Option<PathBuf>,
    prompt: Option<String>,
) -> Result<ActivationReceipt> {
    // A manifest an agent authored is what the owner approved; it is certified
    // again as it is, not redrafted from help text that may not describe it.
    if let Some(manifest) = registry
        .authored_manifest(harness)
        .with_context(|| format!("harness {harness} is not registered; use `brgr harness add`"))?
    {
        let workspace = match workspace {
            Some(path) => path,
            None => std::env::current_dir().context("cannot read the current directory")?,
        };
        let prompt = prompt.unwrap_or_else(|| RECERTIFY_PROMPT.to_owned());
        let receipt = registry
            .activate_custom_with_scratch(&manifest, &workspace, &prompt, None, None)
            .await?;
        let probed = registry.health_probed(&receipt.harness_id).await?;
        require_healthy_harness(
            &receipt.harness_id,
            probed,
            &registry
                .recertify_action_for(&receipt.harness_id)
                .unwrap_or_else(|_| recertify_action_fallback()),
        )?;
        return Ok(receipt);
    }
    let (executable, presentation_only) = registry
        .certified_executable(harness)
        .with_context(|| format!("harness {harness} is not registered; use `brgr harness add`"))?;
    let args = if presentation_only {
        AddHarnessArgs {
            executable,
            workspace: None,
            prompt: None,
            model: None,
            effort: None,
            presentation_only: true,
        }
    } else {
        AddHarnessArgs {
            executable,
            workspace: Some(match workspace {
                Some(path) => path,
                None => std::env::current_dir().context("cannot read the current directory")?,
            }),
            prompt: Some(prompt.unwrap_or_else(|| RECERTIFY_PROMPT.to_owned())),
            model: None,
            effort: None,
            presentation_only: false,
        }
    };
    add_harness(registry, args).await
}

pub(crate) async fn harness_manifest_input(
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

pub(crate) fn integrate(paths: &Paths, command: IntegrateCommand, json_output: bool) -> Result<()> {
    match command {
        IntegrateCommand::Codex { command } => {
            let outcome = match command {
                CodexCommand::Install => codex_integration::install(&paths.home)?,
                CodexCommand::Status => codex_integration::status(&paths.home)?,
                CodexCommand::Uninstall => codex_integration::uninstall(&paths.home)?,
            };
            print_value(&outcome, json_output);
        }
        IntegrateCommand::Claude { command } => {
            let mut outcome = match command {
                ClaudeCommand::Install { .. } => codex_integration::claude_install(&paths.home)?,
                ClaudeCommand::Status => codex_integration::claude_status(&paths.home)?,
                ClaudeCommand::Uninstall => codex_integration::claude_uninstall(&paths.home)?,
            };
            let status_line = match command {
                ClaudeCommand::Install { context: true } => Some(statusline_install::install(
                    &paths.home,
                    statusline_install::Harness::Claude,
                    &std::env::current_exe()?,
                )?),
                ClaudeCommand::Install { context: false } => None,
                ClaudeCommand::Status => Some(statusline_install::status(
                    &paths.home,
                    statusline_install::Harness::Claude,
                )?),
                ClaudeCommand::Uninstall => Some(statusline_install::uninstall(
                    &paths.home,
                    statusline_install::Harness::Claude,
                )?),
            };
            if let (Some(status_line), Some(object)) = (status_line, outcome.as_object_mut()) {
                object.insert("status_line".to_owned(), status_line);
            }
            print_value(&outcome, json_output);
        }
        IntegrateCommand::Cursor { command } => {
            let harness = statusline_install::Harness::Cursor;
            let outcome = match command {
                CursorCommand::Install => {
                    statusline_install::install(&paths.home, harness, &std::env::current_exe()?)?
                }
                CursorCommand::Status => statusline_install::status(&paths.home, harness)?,
                CursorCommand::Uninstall => statusline_install::uninstall(&paths.home, harness)?,
            };
            print_value(&outcome, json_output);
        }
    }
    Ok(())
}

pub(crate) async fn doctor(paths: &Paths, json_output: bool) -> Result<()> {
    reconcile_pending(paths)?;
    let store_ok = Store::open(&paths.store).is_ok();
    let registry = Registry::open_with_control_home(&paths.registry, &paths.home);
    let registry_ok = registry.is_ok();
    let harnesses = match registry {
        Ok(registry) => doctor_harnesses(&registry).await,
        Err(_) => Vec::new(),
    };
    let integration = codex_integration::status(&paths.home)?;
    let claude_integration = codex_integration::claude_status(&paths.home)?;
    let trace = crate::caller_pane::codex_trace_status();
    let healthy_harnesses = !harnesses.is_empty()
        && harnesses
            .iter()
            .all(|harness| harness["health"] == "healthy");
    // Either owner is enough; one that is installed must be current.
    let integrated = doctor_integrations_current(&integration, &claude_integration);
    let healthy = store_ok && registry_ok && healthy_harnesses && integrated;
    let problems = doctor_problems(
        store_ok,
        registry_ok,
        &harnesses,
        &integration,
        &claude_integration,
        &trace,
        healthy,
    );
    let value = json!({
        "status": if healthy { "ok" } else { "needs_attention" },
        "store": store_ok,
        "registry": registry_ok,
        "harnesses": harnesses,
        "codex_integration": integration,
        "claude_integration": claude_integration,
        "codex_trace": trace,
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    human::print_doctor(&value, &problems, json_output);
    if healthy {
        Ok(())
    } else {
        bail!("{}", unavailable_path_error(&problems))
    }
}

async fn doctor_harnesses(registry: &Registry) -> Vec<serde_json::Value> {
    let mut harnesses = Vec::new();
    match registry.registered_harness_ids() {
        Ok(ids) => {
            for id in ids {
                let health = match registry.health_probed(&id).await {
                    Ok(status) => {
                        let action = registry
                            .recertify_action_for(&id)
                            .unwrap_or_else(|_| recertify_action_fallback());
                        doctor_harness_value(&id, status, &action)
                    }
                    Err(error) => json!({
                        "id": id,
                        "health": "unhealthy",
                        "action": recertify_action_fallback(),
                        "reason": error.to_string(),
                    }),
                };
                harnesses.push(health);
            }
        }
        Err(error) => harnesses.push(json!({
            "health": "unhealthy",
            "reason": error.to_string(),
        })),
    }
    harnesses
}

fn doctor_integrations_current(
    integration: &serde_json::Value,
    claude: &serde_json::Value,
) -> bool {
    let integrations = [integration, claude];
    integrations
        .iter()
        .any(|status| status["status"] == "installed")
        && integrations
            .iter()
            .all(|status| status["status"] != "drifted")
}

fn doctor_problems(
    store_ok: bool,
    registry_ok: bool,
    harnesses: &[serde_json::Value],
    integration: &serde_json::Value,
    claude_integration: &serde_json::Value,
    trace: &serde_json::Value,
    healthy: bool,
) -> Vec<DoctorProblem> {
    let mut problems = Vec::new();
    if !store_ok {
        problems.push(named_problem("store", "unavailable"));
    }
    if !registry_ok {
        problems.push(named_problem("registry", "unavailable"));
    }
    if harnesses.is_empty() {
        problems.push(named_problem("harnesses", "none registered"));
    }
    for harness in harnesses {
        if let Some(problem) = harness_problem(harness) {
            problems.push(problem);
        }
    }
    push_integration_problems(&mut problems, integration, claude_integration);
    if !healthy && let Some(problem) = trace_problem(trace) {
        problems.push(problem);
    }
    problems
}

fn push_integration_problems(
    problems: &mut Vec<DoctorProblem>,
    integration: &serde_json::Value,
    claude_integration: &serde_json::Value,
) {
    if integration["status"] == "drifted" {
        problems.push(integration_problem("codex", "drifted"));
    }
    if claude_integration["status"] == "drifted" {
        problems.push(integration_problem("claude", "drifted"));
    }
    if integration["status"] == "installed" || claude_integration["status"] == "installed" {
        return;
    }
    if integration["status"] == "not_installed" {
        problems.push(integration_problem("codex", "not_installed"));
    }
    if claude_integration["status"] == "not_installed" {
        problems.push(integration_problem("claude", "not_installed"));
    }
}

/// A drifted or missing integration is fixed by installing it again.
fn integration_problem(owner: &str, reason: &str) -> DoctorProblem {
    DoctorProblem {
        name: format!("{owner}_integration"),
        reason: reason.to_owned(),
        command: Some(format!("brgr integrate {owner} install")),
    }
}

fn harness_problem(harness: &serde_json::Value) -> Option<DoctorProblem> {
    if harness["health"] == "healthy" {
        return None;
    }
    let name = harness["id"].as_str().unwrap_or("harnesses").to_owned();
    let action = harness["action"].as_str();
    let command = action.and_then(backtick_command).map(str::to_owned);
    let recertify =
        action.is_some_and(|text| text.contains("re-certify") || text.contains("recertify"));
    let reason = if recertify {
        "recertify".to_owned()
    } else if let Some(reason) = harness["reason"].as_str() {
        reason.to_owned()
    } else if let Some(health) = harness["health"].as_str() {
        health.to_owned()
    } else {
        "unhealthy".to_owned()
    };
    Some(DoctorProblem {
        name,
        reason,
        command,
    })
}

fn trace_problem(trace: &serde_json::Value) -> Option<DoctorProblem> {
    if trace["verified"] == true {
        return None;
    }
    let reason = match (trace["codex"].as_str(), trace["verified_with"].as_str()) {
        (Some(version), Some(baseline)) => {
            format!("not verified: {version}, verified baseline {baseline}")
        }
        (None, Some(baseline)) => format!("not verified: verified baseline {baseline}"),
        (Some(version), None) => format!("not verified: {version}"),
        (None, None) => "not verified".to_owned(),
    };
    Some(DoctorProblem {
        name: "codex_trace".to_owned(),
        reason,
        command: None,
    })
}

fn named_problem(name: &str, reason: &str) -> DoctorProblem {
    DoctorProblem {
        name: name.to_owned(),
        reason: reason.to_owned(),
        command: None,
    }
}

fn backtick_command(text: &str) -> Option<&str> {
    let start = text.find('`')?;
    let inner = text.get(start + 1..)?;
    let end = inner.find('`')?;
    let command = inner.get(..end)?.trim();
    (!command.is_empty()).then_some(command)
}

fn problem_json(problem: &DoctorProblem) -> serde_json::Value {
    match &problem.command {
        Some(command) => json!({
            "name": problem.name,
            "reason": problem.reason,
            "command": command,
        }),
        None => json!({
            "name": problem.name,
            "reason": problem.reason,
        }),
    }
}

fn unavailable_path_error(problems: &[DoctorProblem]) -> String {
    if problems.is_empty() {
        return "brgr doctor found an unavailable personal-use path".to_owned();
    }
    let mut names = String::new();
    for (index, problem) in problems.iter().enumerate() {
        if index > 0 {
            names.push_str(", ");
        }
        names.push_str(&problem.name);
    }
    format!("brgr doctor found an unavailable personal-use path: {names}")
}

/// The harness a call uses when it names none: the configured default, else
/// the only registered one. With several registered and no default, brgr stops
/// and lists them rather than picking one the owner did not choose.
pub(crate) fn default_harness(registry: &Registry, configured: Option<&str>) -> Result<String> {
    if let Some(id) = configured {
        return Ok(id.to_owned());
    }
    let mut ids = registry.registered_harness_ids().unwrap_or_default();
    match ids.len() {
        1 => Ok(ids.remove(0)),
        0 => bail!(
            "no harness is registered; run `brgr harness add <executable> --workspace <dir> --prompt <prompt>`"
        ),
        _ => bail!(
            "no harness named and no default configured; pass `--harness ID` or set `harness` under `[defaults]` in the brgr config. Registered: {}",
            ids.join(", ")
        ),
    }
}

pub(crate) fn recertify_action_fallback() -> String {
    "re-certify with `brgr harness add <executable>`".to_owned()
}

pub(crate) fn require_healthy_harness(id: &str, health: Health, action: &str) -> Result<()> {
    match health {
        Health::Healthy => Ok(()),
        other => bail!("harness {id} is {}; {action}", other.code()),
    }
}

pub(crate) fn doctor_harness_value(id: &str, health: Health, action: &str) -> serde_json::Value {
    harness_health_value("id", id, health, action)
}

pub(crate) fn harness_health_value(
    id_key: &str,
    id: &str,
    health: Health,
    action: &str,
) -> serde_json::Value {
    match health {
        Health::Healthy => json!({ id_key: id, "health": "healthy" }),
        Health::ExitedNonzero { exit_code } => json!({
            id_key: id,
            "health": "exited_nonzero",
            "exit_code": exit_code,
            "action": action,
        }),
        other => json!({
            id_key: id,
            "health": other.code(),
            "action": action,
        }),
    }
}

pub(crate) fn cleanup(paths: &Paths, command: CleanupCommand, json_output: bool) -> Result<()> {
    let task = match command {
        CleanupCommand::Status { task } | CleanupCommand::Run { task } => task,
    };
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    require_owner(&store, &spec.owner_id)?;
    if crate::pane_adapter::session_status(paths, task, spec.revision).is_some() {
        if matches!(command, CleanupCommand::Run { .. }) {
            crate::pane_adapter::cleanup_settled(paths, task, spec.revision)?;
        }
        print_value(
            &crate::pane_adapter::session_status(paths, task, spec.revision)
                .unwrap_or(serde_json::Value::Null),
            json_output,
        );
        return Ok(());
    }
    let status = match command {
        CleanupCommand::Status { .. } => pane_cleanup::status(&store, &paths.runs, task)?,
        CleanupCommand::Run { .. } => pane_cleanup::close_if_eligible(&store, &paths.runs, task)?,
    };
    print_value(&json!({"task_id": task, "cleanup": status}), json_output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::push_integration_problems;

    fn problems_for(codex: &str, claude: &str) -> Vec<(String, Option<String>)> {
        let mut problems = Vec::new();
        push_integration_problems(
            &mut problems,
            &json!({"status": codex}),
            &json!({"status": claude}),
        );
        problems
            .into_iter()
            .map(|problem| (problem.name, problem.command))
            .collect()
    }

    #[test]
    fn a_drifted_integration_names_the_command_that_reinstalls_it() {
        assert_eq!(
            problems_for("drifted", "drifted"),
            vec![
                (
                    "codex_integration".to_owned(),
                    Some("brgr integrate codex install".to_owned())
                ),
                (
                    "claude_integration".to_owned(),
                    Some("brgr integrate claude install".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn a_missing_integration_names_the_command_that_installs_it() {
        assert_eq!(
            problems_for("not_installed", "not_installed"),
            vec![
                (
                    "codex_integration".to_owned(),
                    Some("brgr integrate codex install".to_owned())
                ),
                (
                    "claude_integration".to_owned(),
                    Some("brgr integrate claude install".to_owned())
                ),
            ]
        );
    }
}
