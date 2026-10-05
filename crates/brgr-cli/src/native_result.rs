//! A source-independent result channel for native TUI workers.
use crate::{Paths, pane_adapter::lifecycle::PaneReceipt, write_json_atomic};
use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, TaskId};
use brgr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{env, fs, io::Read as _, path::PathBuf};

#[derive(Serialize, Deserialize)]
struct CursorHook {
    path: PathBuf,
    original: Option<Vec<u8>>,
    generated: Vec<u8>,
}

fn worker(paths: &Paths, task: TaskId) -> Result<(brgr_protocol::TaskSpec, PaneReceipt)> {
    let attempt: AttemptId = env::var("BRGR_PARENT_ATTEMPT_ID")?.parse()?;
    if env::var("BRGR_PARENT_TASK_ID")? != task.to_string() {
        bail!("report belongs to another worker");
    }
    let spec = Store::open(&paths.store)?.task_for_attempt(attempt)?;
    let receipt: PaneReceipt =
        serde_json::from_slice(&fs::read(paths.pane_receipt(task, spec.revision))?)?;
    if spec.task_id != task || receipt.task != Some(task) || receipt.attempt != Some(attempt) {
        bail!("report attempt identity differs");
    }
    Ok((spec, receipt))
}

pub(crate) fn submit(paths: &Paths, task: TaskId, body: &str) -> Result<()> {
    let (spec, receipt) = worker(paths, task)?;
    if body.trim().is_empty() || u64::try_from(body.len())? > spec.artifact_contract.max_bytes {
        bail!("native report exceeds its contract");
    }
    if let Some(expected) = receipt.report_digest.as_deref() {
        if expected != crate::pane_adapter::native_digest(body.as_bytes()) {
            bail!("completed native report is immutable");
        }
        return Ok(());
    }
    restore_cursor(paths, task, spec.revision)?;
    let path = receipt.report.context("native report path missing")?;
    crate::write_bytes_atomic(&path, body.as_bytes())?;
    crate::pane_adapter::seal_native_report(paths, task, spec.revision)
}

pub(crate) fn marker(attempt: AttemptId) -> (String, String) {
    (
        format!("BRGR_REPORT_BEGIN_{attempt}"),
        format!("BRGR_REPORT_END_{attempt}"),
    )
}

pub(crate) fn hook(paths: &Paths, task: TaskId, revision: u32, kind: &str) -> Result<()> {
    let (spec, receipt) = worker(paths, task)?;
    if spec.revision != revision {
        bail!("native result revision changed");
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(
            spec.artifact_contract
                .max_bytes
                .saturating_mul(2)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > spec.artifact_contract.max_bytes.saturating_mul(2) {
        bail!("native hook input too large");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let text = if kind == "claude" {
        if value["hook_event_name"] != "Stop"
            || value["session_id"].as_str() != receipt.attempt.map(|id| id.to_string()).as_deref()
            || value["cwd"]
                .as_str()
                .map(PathBuf::from)
                .and_then(|path| path.canonicalize().ok())
                != Some(PathBuf::from(&spec.workspace).canonicalize()?)
        {
            bail!("native hook session differs");
        }
        value["last_assistant_message"].as_str()
    } else if kind == "cursor" {
        if value["hook_event_name"] != "afterAgentResponse"
            || !value["workspace_roots"].as_array().is_some_and(|roots| {
                roots.iter().any(|root| {
                    root.as_str()
                        .map(PathBuf::from)
                        .and_then(|path| path.canonicalize().ok())
                        == PathBuf::from(&spec.workspace).canonicalize().ok()
                })
            })
        {
            bail!("native hook workspace differs");
        }
        value["text"].as_str()
    } else {
        bail!("unsupported native response event");
    };
    let Some(text) = text else {
        return Ok(());
    };
    let (begin, end) = marker(receipt.attempt.context("native attempt missing")?);
    if let Some((_, body)) = text.rsplit_once(&begin)
        && let Some((body, _)) = body.split_once(&end)
    {
        submit(paths, task, body.trim_matches(['\r', '\n']))?;
    }
    Ok(())
}

pub(crate) fn install(
    paths: &Paths,
    task: TaskId,
    revision: u32,
    kind: &str,
    workspace: &std::path::Path,
    argv: &mut Vec<String>,
) -> Result<()> {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let command = format!(
        "{} --home {} __native-result {} {} {}",
        quote(&env::current_exe()?.to_string_lossy()),
        quote(&paths.home.to_string_lossy()),
        task,
        revision,
        kind
    );
    if kind == "claude" {
        let path = paths
            .runs
            .join(format!("{task}-r{revision}.claude-settings.json"));
        let mut settings = json!({});
        if let Some(index) = argv.iter().position(|arg| arg == "--settings") {
            let source = argv.get(index + 1).context("--settings value missing")?;
            settings = if source.starts_with('{') {
                serde_json::from_str(source)?
            } else {
                serde_json::from_slice(&fs::read(source)?)?
            };
            argv.drain(index..=index + 1);
        }
        if !settings.is_object() {
            bail!("native settings must be an object");
        }
        let hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let stop = hooks
            .as_object_mut()
            .context("native hooks must be an object")?
            .entry("Stop")
            .or_insert_with(|| json!([]));
        stop.as_array_mut()
            .context("native Stop hooks must be an array")?
            .push(json!({"hooks":[{"type":"command","command":command,"timeout":20}]}));
        write_json_atomic(&path, &settings)?;
        argv.extend(["--settings".to_owned(), path.to_string_lossy().into_owned()]);
    } else if kind == "cursor" {
        if fs::symlink_metadata(workspace.join(".cursor"))
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("task Cursor hook directory must be local to the task worktree");
        }
        let path = workspace.join(".cursor/hooks.json");
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("task Cursor hook file must be local to the task worktree");
        }
        let original = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut config: Value = original
            .as_ref()
            .map(|bytes| serde_json::from_slice(bytes))
            .transpose()?
            .unwrap_or_else(|| json!({"version":1,"hooks":{}}));
        let hooks = config
            .as_object_mut()
            .context("Cursor hooks must be an object")?
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let response = hooks
            .as_object_mut()
            .context("Cursor hook events must be an object")?
            .entry("afterAgentResponse")
            .or_insert_with(|| json!([]));
        response
            .as_array_mut()
            .context("Cursor response hooks must be an array")?
            .push(json!({"command":command}));
        let generated = serde_json::to_vec_pretty(&config)?;
        fs::create_dir_all(path.parent().unwrap())?;
        write_json_atomic(
            &cursor_receipt(paths, task, revision),
            &CursorHook {
                path: path.clone(),
                original,
                generated: generated.clone(),
            },
        )?;
        crate::write_bytes_atomic(&path, &generated)?;
    }
    Ok(())
}

fn cursor_receipt(paths: &Paths, task: TaskId, revision: u32) -> PathBuf {
    paths
        .runs
        .join(format!("{task}-r{revision}.cursor-hook.json"))
}

pub(crate) fn restore_cursor(paths: &Paths, task: TaskId, revision: u32) -> Result<()> {
    let path = cursor_receipt(paths, task, revision);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(());
    };
    let hook: CursorHook = serde_json::from_slice(&bytes)?;
    let current = fs::read(&hook.path).ok();
    if current.as_deref() == Some(hook.generated.as_slice()) {
        if let Some(original) = hook.original {
            crate::write_bytes_atomic(&hook.path, &original)?;
        } else {
            fs::remove_file(&hook.path)?;
            let _ = fs::remove_dir(hook.path.parent().unwrap());
        }
    }
    Ok(())
}
