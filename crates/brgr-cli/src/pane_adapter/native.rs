use super::*;

pub(crate) fn report_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut digest = "sha256:".to_owned();
    for byte in Sha256::digest(bytes) {
        let _ = write!(&mut digest, "{byte:02x}");
    }
    digest
}

pub(super) fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether the screen is Claude's trust prompt for exactly this workspace.
pub(super) fn trust_workspace_matches(screen: &str, workspace: &Path) -> bool {
    // A narrow pane wraps even the option text, so look for it without spaces.
    if !squash(screen).contains("Yes,Itrustthisfolder") {
        return false;
    }
    shown_by_rows(screen, workspace) || shown_in_a_narrow_pane(screen, workspace)
}

/// The path wraps across rows, and a wrap at a space drops that space. Walk the
/// workspace path through the rows, accepting that one space at each row break,
/// so a longer or different path never matches.
fn shown_by_rows(screen: &str, workspace: &Path) -> bool {
    let Some((_, content)) = screen.split_once("Accessing workspace:") else {
        return false;
    };
    let workspace = workspace.to_string_lossy();
    let mut rest = workspace.trim_end_matches('/');
    let mut rows = 0;
    for row in content
        .lines()
        .map(str::trim)
        .skip_while(|row| row.is_empty())
    {
        if row.is_empty() || rest.is_empty() {
            break;
        }
        if rows > 0 {
            rest = rest.strip_prefix(' ').unwrap_or(rest);
        }
        rest = match rest.strip_prefix(row) {
            Some(remaining) => remaining,
            None => match rest.strip_prefix(row.trim_end_matches('/')) {
                Some(remaining) if remaining.is_empty() => remaining,
                _ => return false,
            },
        };
        rows += 1;
    }
    rows > 0 && rest.is_empty()
}

/// In a pane only a few columns wide the header wraps too, so rows no longer
/// mark where the path ends. Compare with all spaces removed, and require the
/// prompt's next sentence right after the path so a longer path cannot match.
fn shown_in_a_narrow_pane(screen: &str, workspace: &Path) -> bool {
    let flat = squash(screen);
    let Some((_, after)) = flat.split_once("Accessingworkspace:") else {
        return false;
    };
    let workspace = squash(&workspace.to_string_lossy());
    after
        .strip_prefix(workspace.trim_end_matches('/'))
        .is_some_and(|rest| rest.trim_start_matches('/').starts_with("Quicksafetycheck"))
}

pub(super) fn spawn_session_server(paths: &Paths, task: TaskId) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.runs.join(format!("{task}.session.log")))?;
    Command::new(env::current_exe()?)
        .arg("--home")
        .arg(&paths.home)
        .arg("__session")
        .arg(task.to_string())
        // The pane runner sits in the task worktree; the session outlives the
        // worker and reclaims that worktree, so it must not sit in it too.
        .current_dir(&paths.home)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(())
}

pub(crate) fn serve_session(paths: &Paths, task: TaskId) -> Result<()> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.runs.join(format!("{task}.session.lock")))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let started = Instant::now();
    let mut ticks = 0_u64;
    loop {
        if !paths.store.join("brgr.sqlite3").is_file() {
            return Ok(());
        }
        if started.elapsed() > Duration::from_hours(24) {
            return Ok(());
        }
        let store = Store::open(&paths.store)?;
        let spec = store.task(task)?;
        let path = paths.pane_receipt(task, spec.revision);
        let Ok(bytes) = fs::read(&path) else {
            return Ok(());
        };
        let receipt: PaneReceipt = serde_json::from_slice(&bytes)?;
        if receipt.cleanup == "closed" {
            return Ok(());
        }
        // Recovery scans every retained receipt, so it is not run every second.
        ticks += 1;
        if ticks % 5 == 1
            && let Err(error) = crate::supervision::reconcile_pending(paths)
        {
            eprintln!("brgr session recovery pending: {error}");
        }
        if !matches!(
            store.revision_settlement(task, spec.revision)?,
            brgr_store::Settlement::Open(_)
        ) {
            cleanup_settled(paths, task, spec.revision)?;
            let latest: PaneReceipt = serde_json::from_slice(&fs::read(&path)?)?;
            let launch: LaunchEnvelope =
                serde_json::from_slice(&fs::read(paths.launch(task, spec.revision))?)?;
            if launch.keep_pane || latest.cleanup == "closed" {
                return Ok(());
            }
            thread::sleep(POLL);
            continue;
        }
        let launch: LaunchEnvelope =
            serde_json::from_slice(&fs::read(paths.launch(task, spec.revision))?)?;
        let manifest = launch
            .manifest
            .as_ref()
            .context("session manifest is absent")?;
        let interactive = manifest
            .launch
            .interactive
            .as_ref()
            .context("native session recipe is absent")?;
        let run = PaneRunArgs {
            caller: launch.source_pane,
            native_executable: interactive.native_host.then(|| manifest.executable.clone()),
            opencode_model: super::session_model_supported(interactive)
                .then(|| spec.route.requested_model.clone())
                .flatten(),
            prompt_file: PathBuf::new(),
            workspace: PathBuf::from(spec.workspace),
            task,
            revision: spec.revision,
            kind: interactive.herdr_kind.clone(),
            agent_args: Vec::new(),
            keep_pane: launch.keep_pane,
        };
        let herdr = Herdr {
            binary: receipt.binary.into_os_string(),
            session: receipt.session,
        };
        if (run.native_executable.is_some()
            || receipt.name.as_deref().is_some_and(|name| {
                herdr
                    .status(name)
                    .is_ok_and(|status| matches!(status.as_str(), "idle" | "done"))
            }))
            && let Err(error) = deliver_worker_messages(
                &herdr,
                receipt.name.as_deref().unwrap_or(&receipt.pane),
                &receipt.pane,
                &run,
                paths,
            )
        {
            eprintln!("brgr native delivery pending: {error}");
        }
        thread::sleep(POLL);
    }
}

pub(super) fn deliver_worker_messages(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    paths: &Paths,
) -> Result<()> {
    let lock = input_lock(paths, run.task, run.revision)?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let store = Store::open(&paths.store)?;
    if !prompt_ready(herdr, pane, run, paths)? {
        return Ok(());
    }
    let attempt = store.latest_message_attempt(run.task)?;
    if !matches!(
        store.attempt_state_by_id(attempt)?,
        brgr_protocol::AttemptState::Running | brgr_protocol::AttemptState::Blocked
    ) {
        return Ok(());
    }
    let mut pending = Vec::new();
    for message in store.task_messages(
        run.task,
        attempt,
        brgr_store::MessageDirection::OwnerToWorker,
        false,
    )? {
        pending.push((
            message.message_id.clone(),
            format!("FROM BRGR MESSAGE\n{}", serde_json::to_string(&message)?),
        ));
    }
    for message in store.peer_inbox(run.task, attempt)? {
        pending.push((
            message.id.clone(),
            format!("FROM BRGR DEBATE\n{}", serde_json::to_string(&message)?),
        ));
    }
    for (id, text) in pending {
        if !store.claim_native_delivery(&id, attempt, pane)? {
            continue;
        }
        match send_prompt(herdr, name, pane, run, &text, paths) {
            Ok(()) => store.finish_native_delivery(&id, None)?,
            Err(error) => {
                if error.is::<Unsent>() {
                    store.release_native_delivery(&id)?;
                } else {
                    store
                        .finish_native_delivery(&id, Some("native transport outcome uncertain"))?;
                }
                eprintln!("brgr native message {id} remains observable: {error}");
            }
        }
    }
    Ok(())
}

pub(crate) fn send_native_input(
    paths: &Paths,
    task: TaskId,
    text: Option<&str>,
    key: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let spec = store.task(task)?;
    let lock = input_lock(paths, task, spec.revision)?;
    lock.lock()?;
    crate::require_owner(&store, &spec.owner_id)?;
    let receipt: PaneReceipt =
        serde_json::from_slice(&fs::read(paths.pane_receipt(task, spec.revision))?)?;
    if receipt.cleanup == "closed" {
        bail!("task TUI is already closed");
    }
    let herdr = Herdr {
        binary: receipt.binary.clone().into_os_string(),
        session: receipt.session.clone(),
    };
    verify_receipt(paths, task, spec.revision, &receipt, &herdr)?;
    let native = paths
        .runs
        .join(format!("{}-r{}.native-state.json", task, spec.revision));
    if native.exists() && !crate::tui_host::alive(&native) {
        bail!("task native TUI is no longer alive");
    }
    if let Some(text) = text {
        if text.is_empty() || text.len() > 8192 {
            bail!("native input must contain 1..8192 bytes");
        }
        paste(&herdr, &receipt.pane, text)?;
    } else if let Some(key) = key {
        herdr
            .call(&["pane", "send-keys", &receipt.pane, key])
            .map_err(|e| anyhow::anyhow!("native key input failed: {e}"))?;
    }
    crate::print_value(
        &serde_json::json!({"task_id":task,"pane":receipt.pane,"input_sent":true}),
        json_output,
    );
    Ok(())
}

pub(super) fn send_prompt(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    text: &str,
    paths: &Paths,
) -> Result<()> {
    if !prompt_ready(herdr, pane, run, paths).map_err(|error| Unsent(error.to_string()))? {
        return Err(Unsent("native TUI is waiting for input readiness".to_owned()).into());
    }
    if run.native_executable.is_some() {
        let state = paths
            .runs
            .join(format!("{}-r{}.native-state.json", run.task, run.revision));
        if !crate::tui_host::alive(&state) {
            return Err(Unsent("native TUI is no longer alive".to_owned()).into());
        }
        if let Ok(agent) = herdr.call(&["agent", "get", pane])
            && agent
                .pointer("/result/agent/agent")
                .and_then(Value::as_str)
                .is_some()
            && matches!(
                agent
                    .pointer("/result/agent/agent_status")
                    .and_then(Value::as_str),
                Some("idle" | "done")
            )
        {
            match herdr.call(&["agent", "prompt", pane, text]) {
                Ok(_) => return Ok(()),
                // Herdr recognizes the agent on screen but only prompts agents it
                // started and named, which a native host is not. It refuses before
                // sending anything, so typing the prompt in cannot duplicate it.
                Err(HerdrFailure::Code(code, _))
                    if code == "agent_not_ready" || code == "agent_not_found" => {}
                Err(failure) => {
                    return Err(anyhow::anyhow!(
                        "native TUI prompt delivery failed: {failure}"
                    ));
                }
            }
        }
        paste(herdr, pane, text)?;
    } else {
        herdr
            .call(&["agent", "prompt", name, text])
            .map_err(|e| anyhow::anyhow!("native TUI prompt delivery failed: {e}"))?;
    }
    Ok(())
}

#[derive(Debug)]
struct Unsent(String);
impl std::fmt::Display for Unsent {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}
impl std::error::Error for Unsent {}

fn input_lock(paths: &Paths, task: TaskId, revision: u32) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.runs.join(format!("{task}-r{revision}.input.lock")))?)
}

fn paste(herdr: &Herdr, pane: &str, text: &str) -> Result<()> {
    let framed = format!("\u{1b}[200~{text}\u{1b}[201~");
    herdr
        .call(&["pane", "send-text", pane, &framed])
        .map_err(|e| anyhow::anyhow!("native TUI text delivery failed: {e}"))?;
    thread::sleep(Duration::from_millis(150));
    herdr
        .call(&["pane", "send-keys", pane, "enter"])
        .map_err(|e| anyhow::anyhow!("native TUI submission failed: {e}"))?;
    Ok(())
}

fn prompt_ready(herdr: &Herdr, pane: &str, run: &PaneRunArgs, paths: &Paths) -> Result<bool> {
    let receipt: PaneReceipt =
        serde_json::from_slice(&fs::read(paths.pane_receipt(run.task, run.revision))?)?;
    verify_receipt(paths, run.task, run.revision, &receipt, herdr)?;
    let alive = run.native_executable.is_some()
        && crate::tui_host::alive(
            &paths
                .runs
                .join(format!("{}-r{}.native-state.json", run.task, run.revision)),
        );
    Ok(native_ready(herdr, pane, &run.kind, alive))
}

/// Whether the screen shows a Codex that is idle although Herdr says
/// `working`. Herdr keeps a freshly started Codex at `working` until its first
/// turn ends, so waiting for `idle` would never send the first prompt; the
/// screen says what is true: an empty composer and no `esc to interrupt`.
fn screen_overrides_working(status: &str, kind: &str, screen: &str) -> bool {
    status == "working"
        && kind == "codex"
        && editor_ready(screen, kind)
        && !screen.contains("esc to interrupt")
}

pub(super) fn native_ready(herdr: &Herdr, pane: &str, kind: &str, alive: bool) -> bool {
    if menu_on_screen(herdr, pane) {
        return false;
    }
    match herdr.status(pane) {
        Ok(status) if matches!(status.as_str(), "idle" | "done") => true,
        Ok(status)
            if herdr
                .screen(pane)
                .is_some_and(|screen| screen_overrides_working(&status, kind, &screen)) =>
        {
            true
        }
        Ok(status) if status != "unknown" => false,
        _ => {
            alive
                && herdr
                    .screen(pane)
                    .is_some_and(|screen| editor_ready(&screen, kind))
        }
    }
}

fn editor_ready(screen: &str, kind: &str) -> bool {
    if screen.contains("trust this folder") {
        return false;
    }
    let brand = match kind {
        "claude" => "Claude Code",
        "gjc" => "GJC",
        "command-code" => "Command",
        _ => "",
    };
    if !brand.is_empty() && !screen.contains(brand) {
        return false;
    }
    screen.lines().any(|line| {
        let line = line.trim().trim_start_matches(['│', '┃']).trim();
        ["❯", "›", ">"].iter().any(|cursor| {
            line.trim().strip_prefix(cursor).is_some_and(|rest| {
                let rest = rest.trim();
                !rest.starts_with("exec ")
                    && !rest.starts_with("No,")
                    && !rest.starts_with("Yes,")
                    && !numbered_option(rest)
            })
        })
    })
}

pub(crate) fn deliver_owner_notice(
    paths: &Paths,
    task: TaskId,
    revision: u32,
    pane: &str,
    body: &str,
) -> Result<()> {
    let lock = input_lock(paths, task, revision)?;
    lock.try_lock()
        .context("parent input is already being delivered")?;
    let receipt: PaneReceipt =
        serde_json::from_slice(&fs::read(paths.pane_receipt(task, revision))?)?;
    if receipt.pane != pane || receipt.cleanup == "closed" {
        bail!("parent session identity changed");
    }
    let herdr = Herdr {
        binary: receipt.binary.clone().into_os_string(),
        session: receipt.session.clone(),
    };
    verify_receipt(paths, task, revision, &receipt, &herdr)?;
    let native = paths
        .runs
        .join(format!("{task}-r{revision}.native-state.json"));
    if native.exists() {
        if !crate::tui_host::alive(&native) {
            bail!("parent native TUI is not alive");
        }
        let launch: LaunchEnvelope =
            serde_json::from_slice(&fs::read(paths.launch(task, revision))?)?;
        let kind = launch
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.launch.interactive.as_ref())
            .context("parent native recipe absent")?
            .herdr_kind
            .as_str();
        if !native_ready(&herdr, pane, kind, true) {
            bail!("parent native TUI is not ready");
        }
        if let Ok(agent) = herdr.call(&["agent", "get", pane])
            && agent
                .pointer("/result/agent/agent")
                .and_then(Value::as_str)
                .is_some()
            && herdr
                .status(pane)
                .is_ok_and(|status| matches!(status.as_str(), "idle" | "done"))
        {
            herdr
                .call(&["agent", "prompt", pane, body])
                .map_err(|e| anyhow::anyhow!("parent prompt failed: {e}"))?;
        } else {
            paste(&herdr, pane, body)?;
        }
    } else {
        let name = receipt
            .name
            .as_deref()
            .context("parent session name missing")?;
        if !herdr
            .status(name)
            .is_ok_and(|status| matches!(status.as_str(), "idle" | "done"))
        {
            bail!("parent TUI is not idle");
        }
        herdr
            .call(&["agent", "prompt", name, body])
            .map_err(|e| anyhow::anyhow!("parent message failed: {e}"))?;
    }
    Ok(())
}

/// Environment that keeps a harness from updating itself mid-run. A finished
/// update replaces the binary and asks for a restart, and the native host exits
/// with its pane, so the run cannot be relaunched in place. Preventing the
/// update is the faithful form of "skip updates". User config files stay as
/// they are.
fn self_update_env(kind: &str) -> &'static [(&'static str, &'static str)] {
    match kind {
        "opencode" => &[("OPENCODE_DISABLE_AUTOUPDATE", "1")],
        "claude" => &[("DISABLE_AUTOUPDATER", "1")],
        _ => &[],
    }
}

fn prepare_native_config(run: &PaneRunArgs, paths: &Paths) -> Result<(PathBuf, PathBuf)> {
    let launch: LaunchEnvelope =
        serde_json::from_slice(&fs::read(paths.launch(run.task, run.revision))?)?;
    let manifest = launch
        .manifest
        .as_ref()
        .context("native host has no pinned manifest")?;
    let opencode_model = manifest
        .launch
        .interactive
        .as_ref()
        .filter(|interactive| super::session_model_supported(interactive))
        .and(launch.spec.route.requested_model.clone());
    if run.opencode_model != opencode_model {
        bail!("native OpenCode model differs from the pinned task route");
    }
    let state = paths
        .runs
        .join(format!("{}-r{}.native-state.json", run.task, run.revision));
    let config = paths
        .runs
        .join(format!("{}-r{}.native-launch.json", run.task, run.revision));
    let mut environment = std::collections::BTreeMap::new();
    for name in manifest.launch.env_allow.iter().map(String::as_str).chain([
        "BRGR_HOME",
        "BRGR_BIN",
        "BRGR_PARENT_TASK_ID",
        "BRGR_PARENT_ATTEMPT_ID",
        "BRGR_OWNER_ID",
        "BRGR_SESSION_ID",
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_BIN_PATH",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
    ]) {
        if let Ok(value) = env::var(name) {
            environment.insert(name.to_owned(), value);
        }
    }
    for (name, value) in self_update_env(&run.kind) {
        environment.insert((*name).to_owned(), (*value).to_owned());
    }
    let mut argv = run.agent_args.clone();
    if run.kind == "claude"
        && !argv.iter().any(|arg| arg == "--session-id")
        && let Ok(attempt) = env::var("BRGR_PARENT_ATTEMPT_ID")
    {
        argv.extend(["--session-id".to_owned(), attempt]);
    }
    if launch.spec.permission == Some(brgr_protocol::PermissionLevel::ReadOnly) {
        crate::native_result::install(
            paths,
            run.task,
            run.revision,
            &run.kind,
            &run.workspace,
            &mut argv,
        )?;
    }
    write_json_atomic(
        &config,
        &crate::tui_host::NativeLaunch {
            executable: run
                .native_executable
                .clone()
                .context("native executable is absent")?,
            digest: launch
                .executable_digest
                .context("native executable digest is absent")?,
            argv,
            workspace: run.workspace.clone(),
            environment,
            pane_environment: crate::tui_host::pane_variables(&manifest.launch.env_allow),
            opencode_model,
            state: state.clone(),
        },
    )?;
    Ok((state, config))
}

/// How long a harness must look ready before brgr believes it. A harness can
/// draw its input box for an instant before a dialog covers it: Codex 0.166
/// shows its banner and prompt about half a second before the folder prompt,
/// and a worker started on that frame fails its first prompt when the dialog
/// arrives.
const READY_SETTLE: Duration = Duration::from_secs(1);

/// Tracks how long the harness has looked ready without a break.
#[derive(Default)]
struct ReadySettle {
    since: Option<Instant>,
}

impl ReadySettle {
    /// Whether it has looked ready for [`READY_SETTLE`] up to `now`. A poll that
    /// is not ready starts the count again.
    fn held(&mut self, ready: bool, now: Instant) -> bool {
        if !ready {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.duration_since(since) >= READY_SETTLE
    }

    /// Forgets a readiness in progress, for a poll that pressed a key or met a
    /// screen no rule answers.
    fn reset(&mut self) {
        self.since = None;
    }
}

/// Runs `attempt` until it goes through. While it fails as [`Unsent`] (the
/// agent has not shown it is ready yet) it looks at the screen with `between`,
/// waits `poll`, and tries again until `limit` has passed; the last
/// [`Unsent`] is then the error. Any other error ends it at once.
fn retry_unsent(
    limit: Duration,
    poll: Duration,
    mut attempt: impl FnMut() -> Result<()>,
    mut between: impl FnMut() -> Result<()>,
) -> Result<()> {
    let started = Instant::now();
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) if error.is::<Unsent>() && started.elapsed() < limit => {
                between()?;
                thread::sleep(poll);
            }
            Err(error) => return Err(error),
        }
    }
}

/// The task's first prompt. A native TUI can look ready and then go back to
/// starting up, or a dialog can open after it looked ready (Codex shows its
/// status as unknown, then working, while it loads), so a prompt that is not
/// accepted yet is tried again, answering any screen that appeared, for as long
/// as startup may take instead of failing the run on the first refusal.
pub(super) fn send_first_prompt(
    herdr: &Herdr,
    name: &str,
    pane: &str,
    run: &PaneRunArgs,
    text: &str,
    paths: &Paths,
) -> Result<()> {
    let mut watch = ScreenWatch::new(&run.kind);
    retry_unsent(
        SHELL_READY,
        SHELL_POLL,
        || send_prompt(herdr, name, pane, run, text, paths),
        || {
            let status = herdr.status(pane).ok();
            match screens::resolve(herdr, pane, run, paths, status.as_deref())? {
                Screen::Resolve { rule, .. } => watch.pressed(rule),
                Screen::Unknown(text) => watch.observe(&Screen::Unknown(text)),
                Screen::Clear => {
                    watch.reset();
                    Ok(())
                }
            }
        },
    )
}

pub(super) fn start_native(
    herdr: &Herdr,
    pane: &str,
    run: &PaneRunArgs,
    notices: &mut Notices<'_>,
) -> Result<()> {
    let paths = notices.paths;
    let (state, config) = prepare_native_config(run, paths)?;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    // Terminal bootstrap contains only this trusted executable and a generated
    // config path. The harness itself is executed as an argv array by the host.
    let bootstrap = format!(
        "exec {} __tui-host {}",
        quote(&env::current_exe()?.to_string_lossy()),
        quote(&config.to_string_lossy())
    );
    herdr
        .call(&["pane", "run", pane, &bootstrap])
        .map_err(|e| anyhow::anyhow!("native host bootstrap failed: {e}"))?;
    let opened = Instant::now();
    while !crate::tui_host::alive(&state) {
        if let Some(error) = fs::read(&state)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::tui_host::NativeState>(&bytes).ok())
            .filter(|state| state.phase == "failed")
            .and_then(|state| state.startup_error)
        {
            bail!("native TUI preparation failed: {error}");
        }
        if opened.elapsed() > SHELL_READY {
            bail!("native TUI host did not start; inspect task session state");
        }
        thread::sleep(SHELL_POLL);
    }
    let mut announced = false;
    let mut watch = ScreenWatch::new(&run.kind);
    let mut settle = ReadySettle::default();
    loop {
        if !crate::tui_host::alive(&state) {
            bail!("native TUI exited before accepting the task");
        }
        let status = herdr.status(pane).ok();
        match screens::resolve(herdr, pane, run, paths, status.as_deref())? {
            Screen::Resolve { rule, .. } => {
                watch.pressed(rule)?;
                settle.reset();
                thread::sleep(POLL);
                continue;
            }
            Screen::Unknown(text) => {
                settle.reset();
                if !announced {
                    notices.tell(&run.kind, pane, &format!("The {} agent is on a screen no brgr rule answers: {text}. Use brgr input if it needs a key.", run.kind));
                    announced = true;
                }
                watch.observe(&Screen::Unknown(text))?;
            }
            Screen::Clear => watch.reset(),
        }
        let detected_ready = herdr.call(&["agent", "get", pane]).is_ok_and(|value| {
            value.pointer("/result/agent/agent").and_then(Value::as_str) == Some(run.kind.as_str())
                && matches!(status.as_deref(), Some("idle" | "done"))
        });
        let prompt = herdr
            .screen(pane)
            .is_some_and(|screen| editor_ready(&screen, &run.kind));
        let working_but_idle = status.as_deref().is_some_and(|status| {
            herdr
                .screen(pane)
                .is_some_and(|screen| screen_overrides_working(status, &run.kind, &screen))
        });
        let ready = (matches!(status.as_deref(), Some("idle" | "done"))
            || status.is_none()
            || matches!(status.as_deref(), Some("unknown"))
            || working_but_idle
            || matches!(run.kind.as_str(), "gjc" | "command-code"))
            && (prompt || detected_ready)
            && !menu_on_screen(herdr, pane);
        if settle.held(ready, Instant::now()) {
            break;
        }
        if status.as_deref() == Some("blocked") && !announced {
            notices.blocked(&run.kind, pane, "while starting");
            announced = true;
        }
        if !announced && opened.elapsed() > SHELL_READY {
            notices.tell(&run.kind,pane,"Native TUI has started and is waiting for input. Inspect its screen with brgr status and use brgr input for native input.");
            announced = true;
        }
        thread::sleep(SHELL_POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn just_before_settled() -> Duration {
        READY_SETTLE.saturating_sub(Duration::from_millis(1))
    }

    #[test]
    fn readiness_counts_only_after_it_has_held_for_the_settle_window() {
        let start = Instant::now();
        let mut settle = ReadySettle::default();
        assert!(!settle.held(true, start));
        assert!(!settle.held(true, start + Duration::from_millis(500)));
        assert!(!settle.held(true, start + just_before_settled()));
        assert!(settle.held(true, start + READY_SETTLE));
    }

    #[test]
    fn a_dialog_that_covers_the_input_restarts_the_settle_window() {
        // Codex 0.166 draws its banner and input box for about half a second
        // before the folder prompt covers them.
        let start = Instant::now();
        let mut settle = ReadySettle::default();
        assert!(!settle.held(true, start));
        assert!(!settle.held(false, start + Duration::from_millis(400)));
        assert!(!settle.held(true, start + Duration::from_millis(800)));
        assert!(!settle.held(
            true,
            start + Duration::from_millis(800) + just_before_settled()
        ));
        assert!(settle.held(true, start + Duration::from_millis(800) + READY_SETTLE));
    }

    #[test]
    fn an_explicit_reset_forgets_a_readiness_in_progress() {
        let start = Instant::now();
        let mut settle = ReadySettle::default();
        assert!(!settle.held(true, start));
        settle.reset();
        assert!(!settle.held(true, start + READY_SETTLE));
    }

    #[test]
    fn an_unsent_prompt_is_retried_until_it_goes_through() {
        let mut attempts = 0;
        let mut between = 0;
        let outcome = retry_unsent(
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                attempts += 1;
                if attempts < 4 {
                    Err(Unsent("not ready yet".to_owned()).into())
                } else {
                    Ok(())
                }
            },
            || {
                between += 1;
                Ok(())
            },
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(attempts, 4);
        assert_eq!(between, 3, "the screen is looked at between attempts");
    }

    #[test]
    fn a_prompt_that_stays_unsent_fails_after_the_limit_with_the_reason() {
        let outcome = retry_unsent(
            Duration::from_millis(30),
            Duration::from_millis(5),
            || Err(Unsent("native TUI is waiting for input readiness".to_owned()).into()),
            || Ok(()),
        );
        let error = outcome.unwrap_err();
        assert!(error.is::<Unsent>(), "{error}");
        assert!(error.to_string().contains("waiting for input readiness"));
    }

    #[test]
    fn any_other_error_is_not_retried() {
        let mut attempts = 0;
        let outcome = retry_unsent(
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                attempts += 1;
                Err(anyhow::anyhow!("native transport outcome uncertain"))
            },
            || Ok(()),
        );
        assert!(outcome.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn an_error_while_looking_at_the_screen_stops_the_retry() {
        let outcome = retry_unsent(
            Duration::from_secs(5),
            Duration::from_millis(1),
            || Err(Unsent("not ready yet".to_owned()).into()),
            || {
                Err(anyhow::anyhow!(
                    "a screen no rule answers has stood too long"
                ))
            },
        );
        assert!(outcome.unwrap_err().to_string().contains("stood too long"));
    }

    const CODEX_IDLE_COMPOSER: &str = "  permissions: YOLO mode\n\n\u{2800}\u{2819}\u{281b}\u{283f}\u{283f}\n\u{203a}\u{2801}Ask Codex to do anything\n  GPT-6-Astra low \u{b7} ~/work/ws\n  \u{2190} for agents \u{b7} ? for shortcuts\n";
    const CODEX_BUSY: &str = "  permissions: YOLO mode\n\n\u{2022} Working (3s \u{2022} esc to interrupt)\n\u{203a} Ask Codex to do anything\n  GPT-6-Astra low \u{b7} ~/work/ws\n";

    #[test]
    fn codex_idle_at_its_composer_is_ready_even_while_herdr_says_working() {
        // Herdr keeps a freshly started Codex at `working` until its first turn
        // ends, with the composer empty and nothing running.
        assert!(screen_overrides_working(
            "working",
            "codex",
            CODEX_IDLE_COMPOSER
        ));
    }

    #[test]
    fn codex_that_is_running_a_turn_is_not_ready() {
        assert!(!screen_overrides_working("working", "codex", CODEX_BUSY));
    }

    #[test]
    fn only_a_working_codex_is_read_from_the_screen() {
        for (status, kind) in [
            ("working", "claude"),
            ("working", "cursor"),
            ("blocked", "codex"),
            ("idle", "codex"),
            ("unknown", "codex"),
        ] {
            assert!(
                !screen_overrides_working(status, kind, CODEX_IDLE_COMPOSER),
                "{status} {kind}"
            );
        }
    }

    #[test]
    fn a_codex_folder_prompt_is_not_a_ready_composer() {
        let trust = "  Folder access\n  /work/ws\n  Trust this folder? Codex can read\n\u{203a} 1. Trust and continue\n  2. Back to Agent Command Center\n";
        assert!(!screen_overrides_working("working", "codex", trust));
    }

    #[test]
    fn opencode_and_claude_launch_with_self_update_disabled() {
        assert_eq!(
            self_update_env("opencode"),
            [("OPENCODE_DISABLE_AUTOUPDATE", "1")]
        );
        assert_eq!(self_update_env("claude"), [("DISABLE_AUTOUPDATER", "1")]);
        assert!(self_update_env("cursor").is_empty());
    }

    #[test]
    fn trust_path_preserves_spaces_and_rejects_prefixes() {
        let screen = |path: &str| {
            format!("Accessing workspace:\n\n{path}\n\n❯ No, exit\nYes, I trust this folder\n")
        };
        assert!(trust_workspace_matches(
            &screen("/repo/task one/"),
            Path::new("/repo/task one")
        ));
        assert!(!trust_workspace_matches(
            &screen("/repo/taskone"),
            Path::new("/repo/task one")
        ));
        assert!(!trust_workspace_matches(
            &screen("/repo/task-other"),
            Path::new("/repo/task")
        ));
        assert!(trust_workspace_matches(
            &screen("/repo/\ntask"),
            Path::new("/repo/task")
        ));
    }

    /// A worker pane only a few columns wide (task `e2174e4c`, dogfood D7): the
    /// header, the path and even the option text wrap word by word.
    #[test]
    fn trust_prompt_in_a_very_narrow_pane_still_matches() {
        let screen = "Accessing\nworkspace:\n/Users/justn/Library/Application\nSupport/brgr/worktrees/fresh\nrepo/e2174e4c\nQuick\nsafety\ncheck:\nIs this a project\n❯ No, exit\n  Yes, I\n  trust\n  this\n  folder\n";
        let workspace = Path::new(
            "/Users/justn/Library/Application Support/brgr/worktrees/fresh repo/e2174e4c",
        );
        assert!(trust_workspace_matches(screen, workspace));
        assert!(!trust_workspace_matches(
            &screen.replace("e2174e4c", "e2174e4c-extra"),
            workspace
        ));
        assert!(!trust_workspace_matches(
            screen,
            Path::new("/Users/justn/Library/Application Support/brgr/worktrees/fresh repo/other")
        ));
    }

    /// The prompt as seen in a narrow pane (task `915c0be5`, dogfood D4): the
    /// path wrapped at "Application Support" and the space went with the wrap,
    /// with no blank row before the next paragraph.
    #[test]
    fn trust_path_wrapped_at_a_space_still_matches() {
        let screen = "Accessing workspace:\n /Users/justn/Library/Application\n Support/brgr/worktrees/dogfood-repo/915c0be5\n Quick safety check: Is this a project you created\n\n ❯ No, exit\n   Yes, I trust this folder\n";
        let workspace = Path::new(
            "/Users/justn/Library/Application Support/brgr/worktrees/dogfood-repo/915c0be5/",
        );
        assert!(trust_workspace_matches(screen, workspace));
        assert!(!trust_workspace_matches(
            screen,
            Path::new("/Users/justn/Library/Application Support/brgr/worktrees/dogfood-repo/other")
        ));
        assert!(!trust_workspace_matches(
            &screen.replace("915c0be5", "915c0be5-extra"),
            workspace
        ));
    }
}
