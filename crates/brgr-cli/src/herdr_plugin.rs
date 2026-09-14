//! Herdr plugin entrypoints; brgr remains the task and result authority.

use std::{
    env,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::AttemptState;
use brgr_store::{Store, StoreError};
use serde::Deserialize;
use tempfile::Builder;
use tokio::{
    process::Command,
    time::{sleep, timeout},
};

use crate::{Paths, codex_integration, plugin_bridge};

const PLUGIN_ID: &str = "brgr";
const WORKSPACE_PATH_ENV: &str = "BRGR_PLUGIN_WORKSPACE_CWD";
const HERDR_CALL_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Default, Deserialize)]
struct PluginContext {
    workspace_id: Option<String>,
    workspace_cwd: Option<PathBuf>,
    focused_pane_cwd: Option<PathBuf>,
    worktree: Option<PluginWorktree>,
}

#[derive(Deserialize)]
struct PluginWorktree {
    checkout_path: PathBuf,
}

fn context() -> Result<PluginContext> {
    let Some(raw) = env::var_os("HERDR_PLUGIN_CONTEXT_JSON") else {
        return Ok(PluginContext::default());
    };
    let raw = raw.to_string_lossy();
    if raw.len() > 65_536 {
        bail!("Herdr plugin context exceeds 64 KiB");
    }
    serde_json::from_str(&raw).context("Herdr plugin context is invalid JSON")
}

fn require_host() -> Result<()> {
    if env::var("HERDR_ENV").as_deref() != Ok("1")
        || env::var("HERDR_PLUGIN_ID").as_deref() != Ok(PLUGIN_ID)
    {
        bail!("brgr plugin entrypoints require the brgr Herdr plugin host");
    }
    Ok(())
}

fn workspace_id(context: &PluginContext) -> Result<String> {
    let id = context
        .workspace_id
        .clone()
        .or_else(|| env::var("HERDR_WORKSPACE_ID").ok())
        .context("Herdr did not supply a workspace id")?;
    if id.is_empty() || id.len() > 128 || !id.is_ascii() {
        bail!("Herdr supplied an invalid workspace id");
    }
    Ok(id)
}

fn workspace_path(context: &PluginContext) -> Result<&Path> {
    let path = context
        .worktree
        .as_ref()
        .map(|worktree| worktree.checkout_path.as_path())
        .or(context.workspace_cwd.as_deref())
        .or(context.focused_pane_cwd.as_deref())
        .context("Herdr did not supply a workspace directory")?;
    if !path.is_absolute() || !path.is_dir() {
        bail!(
            "Herdr workspace directory is unavailable: {}",
            path.display()
        );
    }
    Ok(path)
}

fn launched_workspace_path(context: &PluginContext) -> Result<PathBuf> {
    if let Some(path) = env::var_os(WORKSPACE_PATH_ENV) {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_dir() {
            bail!(
                "brgr plugin workspace directory is unavailable: {}",
                path.display()
            );
        }
        return Ok(path);
    }
    Ok(workspace_path(context)?.to_path_buf())
}

pub async fn open(no_focus: bool, codex: bool) -> Result<()> {
    require_host()?;
    let context = context()?;
    let workspace = workspace_id(&context)?;
    let path = workspace_path(&context)?;
    let path_text = path
        .to_str()
        .context("Herdr workspace directory is not UTF-8")?;
    let herdr = PathBuf::from(env::var_os("HERDR_BIN_PATH").context("HERDR_BIN_PATH is absent")?);
    if !herdr.is_absolute() || !herdr.is_file() {
        bail!("HERDR_BIN_PATH is not an absolute Herdr executable");
    }
    let mut command = Command::new(herdr);
    command.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN_ID,
        "--entrypoint",
        if codex { "codex" } else { "board" },
        "--placement",
        "tab",
        "--workspace",
        &workspace,
    ]);
    command.arg(if no_focus { "--no-focus" } else { "--focus" });
    command.arg("--cwd").arg(path);
    command
        .arg("--env")
        .arg(format!("{WORKSPACE_PATH_ENV}={path_text}"));
    let output = timeout(HERDR_CALL_DEADLINE, command.kill_on_drop(true).output())
        .await
        .context("Herdr pane open timed out")??;
    if !output.status.success() {
        bail!(
            "Herdr could not open the brgr pane: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if output.stdout.len() > 65_536 {
        bail!("Herdr pane response exceeds 64 KiB");
    }
    io::stdout().write_all(&output.stdout)?;
    Ok(())
}

fn board_snapshot(paths: &Paths) -> Result<String> {
    let store = Store::open(&paths.store)?;
    let mut output = String::from("brgr · managed tasks\n\n");
    output.push_str("Codex owns final accept/reject. Use the Herdr 'Open Codex for brgr' action to run or review work.\n\n");
    output.push_str("TASK      PROJECT               HARNESS            REV  STATE       RESULT      DECISION\n");
    let tasks = store.tasks(20)?;
    if tasks.is_empty() {
        output.push_str("No managed tasks yet.\n");
    }
    for task in tasks {
        let state = match store.attempt_state(task.task_id) {
            Ok(state) => state,
            Err(StoreError::TaskNotFound(_)) => AttemptState::Queued,
            Err(error) => return Err(error.into()),
        };
        let result = match store.latest_result(task.task_id) {
            Ok(result) => Some(result),
            Err(StoreError::TaskNotFound(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let decision = result
            .as_ref()
            .map(|result| store.decision_for_result(result.result_id))
            .transpose()?
            .flatten();
        let project = Path::new(&task.workspace)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("-");
        let task_id = task.task_id.to_string();
        let outcome = result.as_ref().map_or_else(
            || "-".to_owned(),
            |value| format!("{:?}", value.outcome).to_lowercase(),
        );
        let verdict = decision.map_or_else(
            || "-".to_owned(),
            |value| format!("{:?}", value.verdict).to_lowercase(),
        );
        writeln!(
            output,
            "{:<9} {:<21} {:<18} {:<4} {:<11} {:<11} {}",
            &task_id[..8],
            truncate(project, 20),
            truncate(&task.route.harness_id, 17),
            task.revision,
            format!("{state:?}").to_lowercase(),
            outcome,
            verdict,
        )?;
    }
    output.push_str(
        "\nRead-only · latest 20 tasks · refreshes every 2 seconds · close the pane to exit\n",
    );
    Ok(output)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub async fn board(paths: &Paths, once: bool) -> Result<()> {
    require_host()?;
    loop {
        let snapshot = board_snapshot(paths)?;
        if once {
            print!("{snapshot}");
            return Ok(());
        }
        print!("\x1b[2J\x1b[H{snapshot}");
        io::stdout().flush()?;
        sleep(Duration::from_secs(2)).await;
    }
}

pub async fn codex(paths: &Paths) -> Result<()> {
    let result = launch_codex(paths).await;
    if let Err(error) = &result
        && io::stdin().is_terminal()
    {
        eprintln!("brgr could not start Codex: {error:#}\nPress Enter to close this pane.");
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }
    result
}

async fn launch_codex(paths: &Paths) -> Result<()> {
    require_host()?;
    let context = context()?;
    let workspace = launched_workspace_path(&context)?;
    codex_integration::install(&paths.home)
        .context("brgr could not install its Codex integration")?;
    let executable = env::current_exe()?;
    let binary_dir = executable
        .parent()
        .context("brgr executable has no parent directory")?
        .to_path_buf();
    let mut binary_paths = vec![binary_dir];
    if let Some(previous) = env::var_os("PATH") {
        binary_paths.extend(env::split_paths(&previous));
    }
    let path = env::join_paths(binary_paths).context("could not add brgr to Codex PATH")?;
    let path_text = path.to_str().context("Codex PATH is not UTF-8")?;
    let bridge_dir = Builder::new()
        .prefix("plugin-bridge-")
        .tempdir_in(&paths.home)
        .context("brgr could not create its private Codex bridge")?;
    fs::set_permissions(bridge_dir.path(), fs::Permissions::from_mode(0o700))?;
    let mut child = Command::new("codex")
        .arg("-C")
        .arg(&workspace)
        .arg("--add-dir")
        .arg(&paths.home)
        .arg("-c")
        .arg(codex_env_config("PATH", path_text)?)
        .arg("-c")
        .arg(codex_env_config(
            plugin_bridge::BRIDGE_DIR_ENV,
            bridge_dir
                .path()
                .to_str()
                .context("bridge path is not UTF-8")?,
        )?)
        .arg("-c")
        .arg(codex_env_config(
            "BRGR_HOME",
            paths.home.to_str().context("brgr home is not UTF-8")?,
        )?)
        .env("PATH", path)
        .env(plugin_bridge::BRIDGE_DIR_ENV, bridge_dir.path())
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_SESSION_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("brgr could not start Codex")?;
    let mut bridge = tokio::spawn(plugin_bridge::serve(
        bridge_dir.path().to_path_buf(),
        executable,
        paths.home.clone(),
    ));
    let status = tokio::select! {
        status = child.wait() => status.context("brgr could not wait for Codex")?,
        stopped = &mut bridge => {
            let _ = child.kill().await;
            bail!("brgr Herdr bridge stopped while Codex was running: {stopped:?}");
        }
    };
    bridge.abort();
    let _ = bridge.await;
    if !status.success() {
        bail!("Codex exited with {status}");
    }
    Ok(())
}

fn codex_env_config(name: &str, value: &str) -> Result<String> {
    Ok(format!(
        "shell_environment_policy.set.{name}={}",
        serde_json::to_string(value)?
    ))
}

#[cfg(test)]
mod tests {
    use super::{PluginContext, PluginWorktree, workspace_path};
    use std::path::PathBuf;

    #[test]
    fn worktree_checkout_wins_over_pane_and_workspace_cwd() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let context = PluginContext {
            workspace_id: Some("w1".to_owned()),
            workspace_cwd: Some(PathBuf::from("/unused")),
            focused_pane_cwd: Some(PathBuf::from("/also-unused")),
            worktree: Some(PluginWorktree {
                checkout_path: checkout.clone(),
            }),
        };
        assert_eq!(workspace_path(&context).unwrap(), checkout.as_path());
    }

    #[test]
    fn absent_or_relative_workspace_fails_closed() {
        let empty = PluginContext::default();
        assert!(workspace_path(&empty).is_err());
        let relative = PluginContext {
            workspace_cwd: Some(PathBuf::from("relative")),
            ..PluginContext::default()
        };
        assert!(workspace_path(&relative).is_err());
    }

    #[test]
    fn workspace_cwd_wins_over_focused_plugin_pane_cwd() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let plugin = root.path().join("plugin");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&plugin).unwrap();
        let context = PluginContext {
            workspace_cwd: Some(project.clone()),
            focused_pane_cwd: Some(plugin),
            ..PluginContext::default()
        };
        assert_eq!(workspace_path(&context).unwrap(), project.as_path());
    }
}
