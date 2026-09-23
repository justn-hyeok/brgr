//! Herdr plugin entrypoints; brgr remains the task and result authority.

use std::{
    env,
    ffi::OsString,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use brgr_store::BoardStore;
use serde::Deserialize;
use tempfile::Builder;
use tokio::{
    process::Command,
    time::{sleep, timeout},
};

use crate::{Paths, codex_integration, config::WorkerPlacement, plugin_bridge};

const PLUGIN_ID: &str = "brgr";
const WORKSPACE_PATH_ENV: &str = "BRGR_PLUGIN_WORKSPACE_CWD";
const WORKER_LAUNCH_ENV: &str = "BRGR_PLUGIN_WORKER_LAUNCH";
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
    parse_workspace_id(
        context
            .workspace_id
            .clone()
            .or_else(|| env::var("HERDR_WORKSPACE_ID").ok()),
    )
}

fn parse_workspace_id(id: Option<String>) -> Result<String> {
    let id = id.context("Herdr did not supply a workspace id")?;
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
    require_absolute_dir(path, "Herdr workspace directory")?;
    Ok(path)
}

fn launched_workspace_path(context: &PluginContext) -> Result<PathBuf> {
    launched_workspace_path_from(context, env::var_os(WORKSPACE_PATH_ENV))
}

fn launched_workspace_path_from(
    context: &PluginContext,
    pinned: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(path) = pinned {
        let path = PathBuf::from(path);
        require_absolute_dir(&path, "brgr plugin workspace directory")?;
        return Ok(path);
    }
    Ok(workspace_path(context)?.to_path_buf())
}

fn require_absolute_dir(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() || !path.is_dir() {
        bail!("{label} is unavailable: {}", path.display());
    }
    Ok(())
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
    if let Ok(session) = env::var("HERDR_SESSION")
        && !session.trim().is_empty()
    {
        command.arg("--session").arg(session);
    }
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
    let store = BoardStore::open_existing(&paths.store)?;
    let mut output = String::from("brgr · managed tasks\n\n");
    output.push_str("Codex owns final accept/reject. Use the Herdr 'Open Codex for brgr' action to run or review work.\n\n");
    output.push_str("TASK      PROJECT               HARNESS            REV  STATE       RESULT      DECISION\n");
    let tasks = store.rows(20)?;
    if tasks.is_empty() {
        output.push_str("No managed tasks yet.\n");
    }
    for task in tasks {
        let project = Path::new(&task.workspace)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("-");
        let task_id = task.task_id.to_string();
        let outcome = task.result_outcome.map_or_else(
            || "-".to_owned(),
            |value| format!("{value:?}").to_lowercase(),
        );
        let verdict = task.decision_verdict.map_or_else(
            || "-".to_owned(),
            |value| format!("{value:?}").to_lowercase(),
        );
        writeln!(
            output,
            "{:<9} {:<21} {:<18} {:<4} {:<11} {:<11} {}",
            &task_id[..8],
            truncate(project, 20),
            truncate(&task.harness_id, 17),
            task.revision,
            format!("{:?}", task.attempt_state).to_lowercase(),
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
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .take(max_chars)
        .collect()
}

pub async fn board(paths: &Paths, once: bool) -> Result<()> {
    require_host()?;
    loop {
        match board_snapshot(paths) {
            Ok(snapshot) => {
                if once {
                    print!("{snapshot}");
                    return Ok(());
                }
                print!("\x1b[2J\x1b[H{snapshot}");
                io::stdout().flush()?;
            }
            Err(error) => {
                let rendered = format!(
                    "brgr · managed tasks\n\nboard refresh failed: {error}\nNo tasks were mutated or reconciled.\n"
                );
                if once {
                    eprint!("{rendered}");
                    return Err(error);
                }
                print!("\x1b[2J\x1b[H{rendered}");
                io::stdout().flush()?;
            }
        }
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

pub async fn worker(paths: &Paths) -> Result<()> {
    require_host()?;
    let launch =
        PathBuf::from(env::var_os(WORKER_LAUNCH_ENV).context("brgr worker launch path is absent")?);
    let canonical = launch
        .canonicalize()
        .context("brgr worker launch path is unavailable")?;
    if !canonical.starts_with(paths.launches.canonicalize()?) || !canonical.is_file() {
        bail!("brgr worker launch path is outside the control home");
    }
    println!("brgr worker · {}", canonical.display());
    crate::supervise(paths, &canonical, true).await
}

pub async fn open_worker(
    paths: &Paths,
    launch: &Path,
    workspace: &Path,
    placement: WorkerPlacement,
) -> Result<serde_json::Value> {
    require_host()?;
    let parent_pane = env::var("HERDR_PANE_ID").context("Herdr parent pane id is absent")?;
    let workspace_id = env::var("HERDR_WORKSPACE_ID").context("Herdr workspace id is absent")?;
    if parent_pane.trim().is_empty() || workspace_id.trim().is_empty() {
        bail!("Herdr parent pane or workspace id is empty");
    }
    let herdr = PathBuf::from(env::var_os("HERDR_BIN_PATH").context("HERDR_BIN_PATH is absent")?);
    if !herdr.is_absolute() || !herdr.is_file() {
        bail!("HERDR_BIN_PATH is not an absolute Herdr executable");
    }
    let mut command = Command::new(herdr);
    if let Ok(session) = env::var("HERDR_SESSION")
        && !session.trim().is_empty()
    {
        command.arg("--session").arg(session);
    }
    command.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN_ID,
        "--entrypoint",
        "worker",
        "--placement",
        if placement == WorkerPlacement::Adjacent {
            "split"
        } else {
            "tab"
        },
    ]);
    if placement == WorkerPlacement::Adjacent {
        command.args(["--target-pane", &parent_pane, "--direction", "right"]);
    } else {
        command.args(["--workspace", &workspace_id]);
    }
    command
        .arg("--cwd")
        .arg(workspace)
        .arg("--env")
        .arg(format!("BRGR_HOME={}", paths.home.display()))
        .arg("--env")
        .arg(format!("{WORKER_LAUNCH_ENV}={}", launch.display()))
        .arg("--no-focus");
    let output = timeout(HERDR_CALL_DEADLINE, command.kill_on_drop(true).output())
        .await
        .context("Herdr worker pane open timed out")??;
    if !output.status.success() {
        bail!(
            "Herdr worker pane could not open: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if output.stdout.len() > 65_536 {
        bail!("Herdr worker pane response exceeds 64 KiB");
    }
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Herdr worker pane response is not JSON")?;
    if receipt
        .pointer("/result/type")
        .and_then(serde_json::Value::as_str)
        != Some("plugin_pane_opened")
        || receipt
            .pointer("/result/plugin_pane/plugin_id")
            .and_then(serde_json::Value::as_str)
            != Some(PLUGIN_ID)
        || receipt
            .pointer("/result/plugin_pane/entrypoint")
            .and_then(serde_json::Value::as_str)
            != Some("worker")
        || receipt
            .pointer("/result/plugin_pane/pane/pane_id")
            .and_then(serde_json::Value::as_str)
            .is_none()
    {
        bail!("Herdr worker pane response has no matching worker identity");
    }
    Ok(receipt)
}

async fn launch_codex(paths: &Paths) -> Result<()> {
    require_host()?;
    let context = context()?;
    let workspace = launched_workspace_path(&context)?;
    codex_integration::install(&paths.home)
        .context("brgr could not install its Codex integration")?;
    let executable = env::current_exe()?;
    let path = plugin_path_value(&executable, env::var_os("PATH"))?;
    let path_text = path.to_str().context("Codex PATH is not UTF-8")?;
    let bridge_dir = Builder::new()
        .prefix("brgr-plugin-bridge-")
        .tempdir()
        .context("brgr could not create its private Codex bridge")?;
    fs::set_permissions(bridge_dir.path(), fs::Permissions::from_mode(0o700))?;
    let mut child = Command::new("codex")
        .arg("-C")
        .arg(&workspace)
        .arg("--add-dir")
        .arg(bridge_dir.path())
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
        workspace.clone(),
    ));
    let status = tokio::select! {
        status = child.wait() => status.context("brgr could not wait for Codex")?,
        stopped = &mut bridge => {
            let _ = child.kill().await;
            let _ = child.wait().await;
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
fn plugin_path_value(executable: &Path, previous: Option<OsString>) -> Result<OsString> {
    let binary_dir = executable
        .parent()
        .context("brgr executable has no parent directory")?;
    let mut binary_paths = vec![binary_dir.to_path_buf()];
    if let Some(previous) = previous {
        binary_paths.extend(env::split_paths(&previous));
    }
    env::join_paths(binary_paths).context("could not add brgr to Codex PATH")
}

fn codex_env_config(name: &str, value: &str) -> Result<String> {
    Ok(format!(
        "shell_environment_policy.set.{name}={}",
        serde_json::to_string(value)?
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        OsString, Path, PathBuf, PluginContext, PluginWorktree, codex_env_config,
        launched_workspace_path_from, parse_workspace_id, plugin_path_value, workspace_path,
    };

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

    #[test]
    fn pinned_workspace_beats_live_focused_plugin_checkout() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let plugin = root.path().join("plugin");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&plugin).unwrap();
        let context = PluginContext {
            workspace_cwd: Some(plugin.clone()),
            focused_pane_cwd: Some(plugin),
            ..PluginContext::default()
        };
        let pinned = launched_workspace_path_from(&context, Some(project.clone().into())).unwrap();
        assert_eq!(pinned, project);
        assert!(launched_workspace_path_from(&context, Some(OsString::from("relative"))).is_err());
    }

    #[test]
    fn workspace_id_rejects_empty_non_ascii_and_oversize() {
        assert!(parse_workspace_id(None).is_err());
        assert!(parse_workspace_id(Some(String::new())).is_err());
        assert!(parse_workspace_id(Some("워크".to_owned())).is_err());
        assert!(parse_workspace_id(Some("a".repeat(129))).is_err());
        assert_eq!(parse_workspace_id(Some("w1".to_owned())).unwrap(), "w1");
    }

    #[test]
    fn plugin_path_puts_brgr_directory_first() {
        let executable = Path::new("/opt/brgr/bin/brgr");
        let path = plugin_path_value(executable, Some(OsString::from("/usr/bin:/bin"))).unwrap();
        let path = path.to_str().unwrap();
        assert!(path.starts_with("/opt/brgr/bin:"));
        assert!(path.contains("/usr/bin"));
    }

    #[test]
    fn sandbox_env_injection_json_quotes_special_characters() {
        let value = r#"/tmp/a b/"quote""#;
        assert_eq!(
            codex_env_config("PATH", value).unwrap(),
            r#"shell_environment_policy.set.PATH="/tmp/a b/\"quote\"""#
        );
    }

    #[test]
    fn board_cells_replace_terminal_control_characters() {
        assert_eq!(super::truncate("repo\n\u{1b}[31m", 20), "repo��[31m");
    }
}
