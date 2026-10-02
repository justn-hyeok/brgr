//! Task admission: `run` and `revise` up to a recorded task and a started run.

use std::{
    env,
    fmt::Write as _,
    fs::{self},
    path::{Path, PathBuf},
};

use crate::cli::{DelegationArgs, ReviseArgs, RunArgs};
use crate::harness_commands::{recertify_action_fallback, require_healthy_harness};
use crate::supervision::{record_unstarted_terminal, spawn_supervisor, supervise};
use crate::{
    Claimant, LaunchEnvelope, Paths, config, current_session, delegation_parent_from_environment,
    notification, owner_from_environment, print_value, require_owner, workspace, write_json_atomic,
    write_json_new,
};
use anyhow::{Context, Result, bail};
use brgr_core::TaskRevision;
use brgr_protocol::{
    ArtifactContract, AttemptBudget, AttemptId, DecisionVerdict, OwnerId, PermissionLevel, Route,
    SCHEMA_V1, TaskId, TaskInstructions, TaskSpec, TerminalOutcome,
};
use brgr_registry::{ActivationReceipt, Registry};
use brgr_runner::HarnessManifest;
use brgr_store::Store;
use config::Config;
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
    pub(crate) headless: bool,
    pub(crate) calling_options: config::CallingOptions,
    pub(crate) instructions_digest: Option<String>,
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

pub(crate) async fn run_task(paths: &Paths, mut args: RunArgs, json_output: bool) -> Result<()> {
    let config = Config::load(&paths.config)?;
    let harness = args
        .harness
        .take()
        .or(config.defaults.harness.clone())
        .unwrap_or_else(|| "local.gjc".to_owned());
    let mut calling = config.calling(&harness);
    calling.model = args.model.take().or(calling.model);
    calling.effort = args.effort.take().or(calling.effort);
    calling.permission = args.permission.map(Into::into).or(calling.permission);
    calling.deadline_seconds = args.deadline_seconds.or(calling.deadline_seconds);
    config::validate_argv(&calling.argv)?;
    let (user_instructions, instructions_digest) = read_user_instructions(paths)?;
    if let Some(instructions) = user_instructions {
        args.role_instructions.push(instructions);
    }
    let registry = Registry::open_with_control_home(&paths.registry, &paths.home)?;
    let (activated, activation) = load_harness(&registry, &harness).await?;
    config::validate_manifest_argv(&calling.argv, &activated)?;
    let task_id = TaskId::new();
    let forward_criteria = args.forwards_criteria();
    let source_workspace = args.workspace.unwrap_or(env::current_dir()?);
    let parent = delegation_parent(&args.delegation)?;
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
            harness_id: harness,
            requested_model: calling.model.clone(),
            requested_effort: calling.effort.clone(),
        },
        required_capabilities: args.capabilities.required_names()?,
        artifact_contract: ArtifactContract {
            media_type: activated.result.media_type.clone(),
            max_bytes: args.evidence.artifact_limit(activated.result.max_bytes),
        },
        acceptance_criteria: criteria,
        budget: AttemptBudget {
            deadline_seconds: calling.deadline_seconds.unwrap_or(3_600),
            max_attempts: 2,
        },
        instructions: TaskInstructions {
            scope: args.scopes,
            role: args.role_instructions,
            forward_criteria,
        },
        evidence: args.evidence.spec(),
        max_concurrent_children: args.max_children,
        permission: calling.permission,
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
            headless: crate::invocation::current().headless,
            calling_options: calling,
            instructions_digest,
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
    let next_revision = rejected_revision(&store, &previous)?;
    let forward_criteria = args.forwards_criteria();
    let source_workspace = args
        .workspace
        .unwrap_or_else(|| PathBuf::from(&previous.workspace));
    let mut replacement = previous.clone();
    replacement.acceptance_criteria = acceptance_criteria(&args.objective, args.criteria);
    replacement.revision = next_revision;
    replacement.create_request_id = format!("revise-{}-{next_revision}", args.task);
    replacement.objective = args.objective;
    replacement.permission = args.permission.map(Into::into).or(replacement.permission);
    replacement.workspace = source_workspace.to_string_lossy().into_owned();
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
    if previous_launch.instructions_digest.is_some() {
        replacement
            .instructions
            .role
            .retain(|value| !value.starts_with("BRGR USER INSTRUCTIONS\n"));
    }
    let (user_instructions, instructions_digest) = read_user_instructions(paths)?;
    if let Some(instructions) = user_instructions {
        replacement.instructions.role.push(instructions);
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
    let (activated, activation) = load_harness(&registry, &replacement.route.harness_id).await?;
    activated.validate_task_route(&replacement)?;
    registry
        .preflight_model(&activated, replacement.route.requested_model.as_deref())
        .await?;
    let mut calling = previous_launch.calling_options.clone().unwrap_or_default();
    calling.harness = Some(replacement.route.harness_id.clone());
    calling.model.clone_from(&replacement.route.requested_model);
    calling
        .effort
        .clone_from(&replacement.route.requested_effort);
    calling.permission = replacement.permission;
    calling.deadline_seconds = Some(replacement.budget.deadline_seconds);
    config::validate_manifest_argv(&calling.argv, &activated)?;
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
            headless: crate::invocation::current().headless,
            calling_options: calling,
            instructions_digest,
        },
    )
    .await
}

fn rejected_revision(store: &Store, previous: &TaskSpec) -> Result<u32> {
    require_owner(store, &previous.owner_id)?;
    let result = store.latest_result(previous.task_id)?;
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
    Ok(next_revision)
}

pub(crate) async fn load_harness(
    registry: &Registry,
    harness: &str,
) -> Result<(HarnessManifest, ActivationReceipt)> {
    let probed = registry.health_probed(harness).await?;
    require_healthy_harness(
        harness,
        probed,
        &registry
            .recertify_action_for(harness)
            .unwrap_or_else(|_| recertify_action_fallback()),
    )?;
    let (mut activated, activation) = registry.load_healthy_with_receipt(harness)?;
    if !crate::invocation::current().headless && activation.registration_mode == "generated" {
        let current = registry.draft(&activated.executable).await?;
        if current.id == activated.id {
            activated.launch.interactive = current.launch.interactive;
        }
    }
    Ok((activated, activation))
}

fn bind_owner_session(store: &Store, owner: &OwnerId, session: Option<&str>) -> Result<()> {
    match (store.owner_binding(owner)?, session) {
        (Some((bound, _)), Some(current)) if bound == current => {}
        (None, Some(current)) => {
            store.rebind_owner(owner, current)?;
        }
        (Some(_), _) => bail!(
            "owner is bound to another session; run `brgr bind TASK --session SESSION` before starting another revision"
        ),
        (None, None) => {}
    }
    Ok(())
}

/// Pane mode puts the agent's own TUI in the split pane, so brgr's worker
/// pane would only be a second, empty one.
/// Refusals that need the resolved source and the activated harness.
fn check_admissible(activated: &HarnessManifest, spec: &TaskSpec, source: &Path) -> Result<()> {
    if spec.evidence.capture_diff && !workspace::is_git_workspace(source)? {
        bail!("--capture-diff requires a Git workspace before task admission");
    }
    if activated.adapter == brgr_runner::OMP_ROLE_ADAPTER_V1
        && crate::caller_pane::verified().is_none()
    {
        bail!(
            "{} reports back to the Herdr pane it was started from, and this process's pane \
             cannot be verified (a Codex tool command runs in a shared daemon that carries \
             another pane's Herdr environment); use local.omp instead",
            activated.id
        );
    }
    Ok(())
}

pub(crate) async fn start_task(
    paths: &Paths,
    mut spec: TaskSpec,
    activated: &HarnessManifest,
    activation: &ActivationReceipt,
    options: StartOptions<'_>,
) -> Result<()> {
    spec.validate()?;
    spec.permission = effective_permission(paths, &spec, options.parent)?;
    activated.validate_task_route(&spec)?;
    let source = options.source_workspace.canonicalize()?;
    let home = paths.home.canonicalize()?;
    validate_source_home(paths, &source, &home, options.parent, &spec.owner_id)?;
    check_admissible(activated, &spec, &source)?;
    let source_pane = if options.headless {
        None
    } else {
        if activated.adapter != brgr_runner::OMP_ROLE_ADAPTER_V1 {
            crate::pane_adapter::require_tui(activated, &spec)?;
        }
        let source_pane = crate::caller_pane::verified().context(
            "default TUI execution needs an exact Herdr source; provide --owner-session and --source-pane, or explicitly request --headless"
        )?;
        Some(source_pane)
    };
    let pane_mode = source_pane.is_some() && activated.adapter != brgr_runner::OMP_ROLE_ADAPTER_V1;
    let admission = workspace::acquire_admission_lock(&paths.worktrees, options.source_workspace)?;
    let store = Store::open(&paths.store)?;
    if let Some((parent_task, parent_attempt)) = options.parent {
        store.validate_delegation_parent(parent_task, parent_attempt, &spec.owner_id)?;
        require_parent_may_delegate(paths, &store, parent_task, parent_attempt)?;
    }
    let session = current_session()?;
    bind_owner_session(&store, &spec.owner_id, session.as_deref())?;
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
        delegation_enabled: pane_mode || options.enable_delegation || options.parent.is_some(),
        manifest: Some(activated.clone()),
        executable_digest: Some(activation.executable_digest.clone()),
        pane_mode,
        claimant: Claimant::Supervisor,
        source_pane,
        source_session: session.clone(),
        calling_options: Some(options.calling_options),
        instructions_digest: options.instructions_digest,
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
    // Claude Code has no hook that binds its pane, and the detached dispatcher
    // cannot prove it runs inside that pane. This shell can, so record the
    // owner's pane now.
    if launch.spec.owner_id.as_str().starts_with("claude:")
        && let Some(session) = session.as_deref()
        && let Err(error) =
            notification::register_current_surface(paths, &store, &launch.spec.owner_id, session)
                .await
    {
        eprintln!("brgr completion notification remains queued: {error}");
    }
    notification::spawn_registration_and_delivery(paths, &launch.spec, session.as_deref());

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
    print_launch_receipt(&launch, &workspace, options.json_output);
    Ok(())
}

fn print_launch_receipt(launch: &LaunchEnvelope, workspace: &Path, json_output: bool) {
    let receipt = json!({
        "task_id": launch.spec.task_id,
        "state": "starting",
        "workspace": workspace,
        "harness": launch.harness_id,
        "revision": launch.spec.revision,
        "requested_model": launch.spec.route.requested_model,
        "requested_effort": launch.spec.route.requested_effort,
        "presentation": if launch.pane_mode { "tui" } else { "headless" },
        "permission": launch.spec.permission.unwrap_or(PermissionLevel::Full),
        "source_pane": launch.source_pane,
        "instructions_digest": launch.instructions_digest,
    });
    print_value(&receipt, json_output);
}

fn read_user_instructions(paths: &Paths) -> Result<(Option<String>, Option<String>)> {
    let path = paths.home.join("BRGR.md");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() > 16_384 {
        bail!("BRGR.md must be a regular file of at most 16 KiB");
    }
    let text = fs::read_to_string(path)?;
    if text.len() > 16_384 {
        bail!("BRGR.md exceeds 16 KiB");
    }
    let mut digest = "sha256:".to_owned();
    for byte in Sha256::digest(text.as_bytes()) {
        write!(&mut digest, "{byte:02x}")?;
    }
    Ok((
        Some(format!("BRGR USER INSTRUCTIONS\n{text}")),
        Some(digest),
    ))
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

pub(crate) fn require_parent_may_delegate(
    paths: &Paths,
    store: &Store,
    parent_task: TaskId,
    parent_attempt: AttemptId,
) -> Result<()> {
    let parent = store.task_for_attempt(parent_attempt)?;
    let launch: LaunchEnvelope = serde_json::from_slice(
        &fs::read(paths.launch(parent_task, parent.revision))
            .context("the parent task's launch record is unreadable")?,
    )?;
    if !launch.delegation_enabled {
        bail!(
            "the parent task was not started with delegation; its worker may message its \
             owner but may not start child tasks"
        );
    }
    Ok(())
}

/// The level a task runs at.
///
/// Whatever was asked for, bounded by the configured cap and by the parent
/// task's own level: a child never gets more than the worker that started it.
/// Asking for more is an error rather than a quiet downgrade, so the caller
/// finds out. Asking for nothing yields the tightest bound, if any; a bound of
/// `full` is no bound, which keeps a task that asked for nothing running
/// exactly as it did before levels existed.
fn effective_permission(
    paths: &Paths,
    spec: &TaskSpec,
    parent: Option<(TaskId, AttemptId)>,
) -> Result<Option<PermissionLevel>> {
    let cap = Config::load(&paths.config)?.worker.max_permission;
    let parent_level = match parent {
        Some((_, attempt)) => {
            Store::open(&paths.store)?
                .task_for_attempt(attempt)?
                .permission
        }
        None => None,
    };
    bound_permission(spec.permission, cap, parent_level)
}

/// The rule behind [`effective_permission`], without the lookups.
pub(crate) fn bound_permission(
    requested: Option<PermissionLevel>,
    cap: Option<PermissionLevel>,
    parent: Option<PermissionLevel>,
) -> Result<Option<PermissionLevel>> {
    let bounds = [
        ("the configured maximum", cap),
        ("the parent task's level", parent),
    ];
    if let Some(requested) = requested {
        for (source, bound) in bounds {
            if let Some(bound) = bound
                && requested > bound
            {
                bail!("permission {requested:?} exceeds {source}, {bound:?}");
            }
        }
        return Ok(Some(requested));
    }
    Ok(bounds
        .into_iter()
        .filter_map(|(_, bound)| bound)
        .min()
        .filter(|level| *level != PermissionLevel::Full))
}

/// The attempt a new task is delegated from: named on the command line or
/// inherited from the worker environment, and the two must agree.
fn delegation_parent(args: &DelegationArgs) -> Result<Option<(TaskId, AttemptId)>> {
    let explicit = args.parent_task.zip(args.parent_attempt);
    let inherited = delegation_parent_from_environment()?;
    if explicit.is_some() && inherited.is_some() && explicit != inherited {
        bail!("explicit delegation parent differs from the current worker attempt");
    }
    Ok(explicit.or(inherited))
}
