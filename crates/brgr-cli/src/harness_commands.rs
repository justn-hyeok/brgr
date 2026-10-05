//! Harness registration, integration, doctor, and cleanup commands.

use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::PathBuf,
};

use crate::cli::{AddHarnessArgs, CleanupCommand, CodexCommand, HarnessCommand, IntegrateCommand};
use crate::supervision::reconcile_pending;
use crate::{Paths, codex_integration, pane_cleanup, print_value, require_owner};
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
        HarnessCommand::Status { harness } => {
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
            let outcome = match command {
                CodexCommand::Install => codex_integration::claude_install(&paths.home)?,
                CodexCommand::Status => codex_integration::claude_status(&paths.home)?,
                CodexCommand::Uninstall => codex_integration::claude_uninstall(&paths.home)?,
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
    let mut harnesses = Vec::new();
    if let Ok(registry) = registry {
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
    }
    let integration = codex_integration::status(&paths.home)?;
    let claude_integration = codex_integration::claude_status(&paths.home)?;
    let healthy_harnesses = !harnesses.is_empty()
        && harnesses
            .iter()
            .all(|harness| harness["health"] == "healthy");
    // Either owner is enough; one that is installed must be current.
    let integrations = [&integration, &claude_integration];
    let integrated = integrations
        .iter()
        .any(|status| status["status"] == "installed")
        && integrations
            .iter()
            .all(|status| status["status"] != "drifted");
    let healthy = store_ok && registry_ok && healthy_harnesses && integrated;
    let value = json!({
        "status": if healthy { "ok" } else { "needs_attention" },
        "store": store_ok,
        "registry": registry_ok,
        "harnesses": harnesses,
        "codex_integration": integration,
        "claude_integration": claude_integration,
        "codex_trace": crate::caller_pane::codex_trace_status(),
    });
    print_value(&value, json_output);
    if healthy {
        Ok(())
    } else {
        bail!("brgr doctor found an unavailable personal-use path")
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
