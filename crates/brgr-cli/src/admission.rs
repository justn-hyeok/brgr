//! Task admission: `run` and `revise` up to a recorded task and a started run.

use std::{
    env,
    fmt::Write as _,
    fs::{self},
    path::{Path, PathBuf},
};

use crate::cli::{ReviseArgs, RunArgs};
use crate::harness_commands::{recertify_action_fallback, require_healthy_harness};
use crate::supervision::{record_unstarted_terminal, spawn_supervisor, supervise};
use crate::{
    LaunchEnvelope, Paths, config, current_session, delegation_parent_from_environment,
    herdr_plugin, notification, owner_from_environment, plugin_bridge, print_value, require_owner,
    workspace, write_json_atomic, write_json_new,
};
use anyhow::{Context, Result, bail};
use brgr_core::TaskRevision;
use brgr_protocol::{
    ArtifactContract, AttemptBudget, AttemptId, DecisionVerdict, OwnerId, Route, SCHEMA_V1, TaskId,
    TaskInstructions, TaskSpec, TerminalOutcome,
};
use brgr_registry::{ActivationReceipt, Registry};
use brgr_runner::HarnessManifest;
use brgr_store::Store;
use config::{Config, WorkerPlacement};
use serde_json::json;
use sha2::{Digest, Sha256};

pub(crate) fn acceptance_criteria(objective: &str, criteria: Vec<String>) -> Vec<String> {
    if criteria.is_empty() {
        vec![format!("Objective achieved: {objective}")]
    } else {
        criteria
    }
}

pub(crate) struct StartOptions<'a> {
    pub(crate) source_workspace: &'a Path,
    pub(crate) snapshot_paths: &'a [PathBuf],
    pub(crate) parent: Option<(TaskId, AttemptId)>,
    pub(crate) enable_delegation: bool,
    pub(crate) snapshot: WorkspaceSnapshot,
    pub(crate) pane: PaneDisposition,
    pub(crate) execution: ExecutionDisposition,
    pub(crate) json_output: bool,
}

pub(crate) enum WorkspaceSnapshot {
    RequireClean,
    AllowCleanHead,
}

pub(crate) enum PaneDisposition {
    CleanupAfterDecision,
    Keep,
}

pub(crate) enum ExecutionDisposition {
    Detached,
    Foreground,
}

pub(crate) async fn run_task(paths: &Paths, args: RunArgs, json_output: bool) -> Result<()> {
    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    let probed = registry.health_probed(&args.harness).await?;
    require_healthy_harness(
        &args.harness,
        probed,
        &registry
            .recertify_action_for(&args.harness)
            .unwrap_or_else(|_| recertify_action_fallback()),
    )?;
    let (activated, activation) = registry
        .load_healthy_with_receipt(&args.harness)
        .with_context(|| format!("harness {} is not active and healthy", args.harness))?;
    let task_id = TaskId::new();
    let forward_criteria = args.forwards_criteria();
    let source_workspace = args.workspace.unwrap_or(env::current_dir()?);
    let explicit_parent = args
        .delegation
        .parent_task
        .zip(args.delegation.parent_attempt);
    let inherited_parent = delegation_parent_from_environment()?;
    if explicit_parent.is_some()
        && inherited_parent.is_some()
        && explicit_parent != inherited_parent
    {
        bail!("explicit delegation parent differs from the current worker attempt");
    }
    let parent = explicit_parent.or(inherited_parent);
    let owner_id = if let Some((_, attempt)) = parent {
        OwnerId::new(format!("worker:{attempt}"))?
    } else {
        owner_from_environment()?
    };
    let criteria = acceptance_criteria(&args.objective, args.criteria);
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
        required_capabilities: args.capabilities.required_names()?,
        artifact_contract: ArtifactContract {
            media_type: activated.result.media_type.clone(),
            max_bytes: args.evidence.artifact_limit(activated.result.max_bytes),
        },
        acceptance_criteria: criteria,
        budget: AttemptBudget {
            deadline_seconds: args.deadline_seconds,
            max_attempts: 2,
        },
        instructions: TaskInstructions {
            scope: args.scopes,
            role: args.role_instructions,
            forward_criteria,
        },
        evidence: args.evidence.spec(),
        max_concurrent_children: args.max_children,
    };
    spec.validate()?;
    activated.validate_task_route(&spec)?;
    registry
        .preflight_model(&activated, spec.route.requested_model.as_deref())
        .await?;
    start_task(
        paths,
        spec,
        &activated,
        &activation,
        StartOptions {
            source_workspace: &source_workspace,
            snapshot_paths: &args.snapshot_paths,
            parent,
            enable_delegation: args.delegation.enable_delegation,
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

pub(crate) async fn revise_task(paths: &Paths, args: ReviseArgs, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let previous = store.task(args.task)?;
    let parent = store
        .delegation_parent(args.task)?
        .map(|(task, attempt, _depth)| (task, attempt));
    let previous_launch: LaunchEnvelope =
        serde_json::from_slice(&fs::read(paths.launch(args.task, previous.revision))?)?;
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
    let forward_criteria = args.forwards_criteria();
    let source_workspace = args
        .workspace
        .unwrap_or_else(|| PathBuf::from(&previous.workspace));
    let criteria = acceptance_criteria(&args.objective, args.criteria);
    let mut replacement = previous.clone();
    replacement.revision = next_revision;
    replacement.create_request_id = format!("revise-{}-{next_revision}", args.task);
    replacement.objective = args.objective;
    replacement.workspace = source_workspace.to_string_lossy().into_owned();
    replacement.acceptance_criteria = criteria;
    let requested = args.capabilities.required_names()?;
    replacement.required_capabilities.extend(requested);
    replacement.required_capabilities.sort();
    replacement.required_capabilities.dedup();
    if !args.scopes.is_empty() {
        replacement.instructions.scope = args.scopes;
    }
    if !args.role_instructions.is_empty() {
        replacement.instructions.role = args.role_instructions;
    }
    replacement.instructions.forward_criteria |= forward_criteria;
    args.evidence.extend_task(&mut replacement);
    if args.max_children.is_some() {
        replacement.max_concurrent_children = args.max_children;
    }
    let replacement = TaskRevision::new(previous)?
        .revise(replacement)?
        .spec()
        .clone();

    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    let probed = registry
        .health_probed(&replacement.route.harness_id)
        .await?;
    require_healthy_harness(
        &replacement.route.harness_id,
        probed,
        &registry
            .recertify_action_for(&replacement.route.harness_id)
            .unwrap_or_else(|_| recertify_action_fallback()),
    )?;
    let (activated, activation) =
        registry.load_healthy_with_receipt(&replacement.route.harness_id)?;
    activated.validate_task_route(&replacement)?;
    registry
        .preflight_model(&activated, replacement.route.requested_model.as_deref())
        .await?;
    start_task(
        paths,
        replacement,
        &activated,
        &activation,
        StartOptions {
            source_workspace: &source_workspace,
            snapshot_paths: &args.snapshot_paths,
            parent,
            enable_delegation: previous_launch.delegation_enabled,
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

pub(crate) async fn start_task(
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
    validate_source_home(paths, &source, &home, options.parent, &spec.owner_id)?;
    if spec.evidence.capture_diff && !workspace::is_git_workspace(&source)? {
        bail!("--capture-diff requires a Git workspace before task admission");
    }
    let plugin_placement = plugin_worker_placement(paths, &options.execution)?;
    let admission = workspace::acquire_admission_lock(&paths.worktrees, options.source_workspace)?;
    let store = Store::open(&paths.store)?;
    if let Some((parent_task, parent_attempt)) = options.parent {
        store.validate_delegation_parent(parent_task, parent_attempt, &spec.owner_id)?;
    }
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
    let workspace = prepare_task_workspace(paths, &mut spec, &activated.adapter, &options)?;
    spec.workspace = workspace.to_string_lossy().into_owned();
    spec.validate()?;
    let launch = LaunchEnvelope {
        spec,
        harness_id,
        protocol_generation: "brgr-v1".to_owned(),
        keep_pane: matches!(options.pane, PaneDisposition::Keep),
        delegation_enabled: options.enable_delegation
            || plugin_placement.is_some()
            || options.parent.is_some(),
        manifest: Some(activated.clone()),
        executable_digest: Some(activation.executable_digest.clone()),
    };
    let launch_path = paths.launch(task_id, launch.spec.revision);
    write_json_new(&launch_path, &launch)?;
    let mut store = Store::open(&paths.store)?;
    let request_digest_text = task_request_digest(&launch.spec)?;
    // The spec records the worktree, not the repository it came from; `brgr
    // prune` needs the repository once that worktree is gone.
    if let Some(primary) = workspace::primary_checkout(&workspace) {
        store.record_task_checkout(task_id, launch.spec.revision, &primary.to_string_lossy())?;
    }
    if let Some((parent_task, parent_attempt)) = options.parent {
        store.record_child_task(
            &launch.spec,
            &request_digest_text,
            parent_task,
            parent_attempt,
        )?;
    } else {
        store.record_task(&launch.spec, &request_digest_text)?;
    }
    drop(admission);
    notification::spawn_registration_and_delivery(paths, &launch.spec, session.as_deref());

    if matches!(options.execution, ExecutionDisposition::Foreground) {
        return supervise(paths, &launch_path, options.json_output).await;
    }
    if let Some(placement) = plugin_placement {
        return start_herdr_worker(
            paths,
            &launch_path,
            &workspace,
            &launch,
            placement,
            &mut store,
            options.json_output,
        )
        .await;
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

pub(crate) fn prepare_task_workspace(
    paths: &Paths,
    spec: &mut TaskSpec,
    adapter: &str,
    options: &StartOptions<'_>,
) -> Result<PathBuf> {
    spec.evidence.base_commit = None;
    spec.evidence.base_tree = None;
    let selected = if options.snapshot_paths.is_empty() {
        None
    } else {
        Some(workspace::read_selected_snapshot(
            options.source_workspace,
            options.snapshot_paths,
        )?)
    };
    let workspace = workspace::prepare_workspace(
        &paths.worktrees,
        options.source_workspace,
        spec.task_id,
        spec.revision,
        adapter,
        matches!(options.snapshot, WorkspaceSnapshot::AllowCleanHead) || selected.is_some(),
    )?;
    if spec.evidence.capture_diff {
        spec.evidence.base_commit = Some(workspace::git_head(&workspace)?);
    }
    if let Some(snapshot) = selected {
        if workspace::git_head(&workspace)? != snapshot.base_revision() {
            bail!("source HEAD changed while the selected snapshot was prepared");
        }
        workspace::apply_selected_snapshot(&workspace, &snapshot)?;
        if spec.evidence.capture_diff {
            spec.evidence.base_tree = Some(workspace::selected_snapshot_tree(
                &workspace,
                &snapshot,
                &paths.runs,
            )?);
        }
        write_json_atomic(
            &paths
                .runs
                .join(format!("{}-r{}.snapshot.json", spec.task_id, spec.revision)),
            &serde_json::to_value(snapshot.receipt(spec.task_id, spec.revision))?,
        )?;
    }
    Ok(workspace)
}

pub(crate) fn validate_source_home(
    paths: &Paths,
    source: &Path,
    home: &Path,
    parent: Option<(TaskId, AttemptId)>,
    owner: &OwnerId,
) -> Result<()> {
    let nested_source = if let Some((parent_task_id, parent_attempt_id)) = parent {
        let store = Store::open(&paths.store)?;
        store.validate_delegation_parent(parent_task_id, parent_attempt_id, owner)?;
        let parent = store.task(parent_task_id)?;
        let parent_workspace = Path::new(&parent.workspace).canonicalize()?;
        parent_workspace.starts_with(paths.worktrees.canonicalize()?)
            && source.starts_with(parent_workspace)
    } else {
        false
    };
    if (source.starts_with(home) && !nested_source) || home.starts_with(source) {
        bail!("brgr control home and the source workspace must not overlap");
    }
    Ok(())
}

pub(crate) fn task_request_digest(spec: &TaskSpec) -> Result<String> {
    let request_digest = Sha256::digest(serde_json::to_vec(spec)?);
    let mut text = String::with_capacity(64);
    for byte in request_digest {
        write!(&mut text, "{byte:02x}")?;
    }
    Ok(text)
}

pub(crate) fn plugin_worker_placement(
    paths: &Paths,
    execution: &ExecutionDisposition,
) -> Result<Option<WorkerPlacement>> {
    if env::var("HERDR_ENV").as_deref() == Ok("1")
        && matches!(execution, ExecutionDisposition::Detached)
    {
        let config = Config::load(&paths.config)?;
        let plugin_caller = env::var("HERDR_PLUGIN_ID").as_deref() == Ok("brgr")
            && (env::var_os(plugin_bridge::BRIDGE_HOST_HOME_ENV).is_some()
                || env::var("BRGR_WORKER_HERDR_CONTEXT").as_deref() == Ok("1"));
        Ok((plugin_caller || config.herdr.auto_worker_pane)
            .then_some(config.herdr.worker_placement))
    } else {
        Ok(None)
    }
}

pub(crate) async fn start_herdr_worker(
    paths: &Paths,
    launch_path: &Path,
    workspace: &Path,
    launch: &LaunchEnvelope,
    placement: WorkerPlacement,
    store: &mut Store,
    json_output: bool,
) -> Result<()> {
    let task_id = launch.spec.task_id;
    match herdr_plugin::open_worker(paths, launch_path, workspace, placement).await {
        Ok(pane) => {
            write_json_atomic(
                &paths.runs.join(format!("{task_id}.worker-pane.json")),
                &pane,
            )
            .with_context(|| {
                format!(
                    "Herdr opened worker pane {} for task {task_id}, but brgr could not persist its receipt; inspect that exact pane and do not repeat the task",
                    pane.pointer("/result/plugin_pane/pane/pane_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                )
            })?;
            print_value(
                &json!({
                    "task_id": task_id,
                    "state": "starting",
                    "workspace": workspace,
                    "harness": launch.harness_id,
                    "revision": launch.spec.revision,
                    "worker_placement": placement.as_str(),
                    "worker_pane": pane.pointer("/result/plugin_pane/pane/pane_id"),
                }),
                json_output,
            );
            Ok(())
        }
        Err(error) => {
            record_unstarted_terminal(
                store,
                &launch.spec,
                TerminalOutcome::Lost,
                format!("Herdr worker pane launch could not be confirmed: {error}"),
            )?;
            Err(error)
        }
    }
}
