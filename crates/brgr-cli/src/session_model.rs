//! The model a pane worker's own session record names.
//!
//! A pane worker's TUI writes its own session log: Claude Code a transcript
//! under `~/.claude/projects/<workspace>/<session>.jsonl`, Codex a rollout
//! under `~/.codex/sessions/...`. brgr launches a Claude Code worker with
//! `--session-id <attempt>`, so that file's name is an identifier brgr set; a
//! Codex worker's rollout path is recorded by the installed Codex hook, bound
//! to the task by the session's own working directory. When brgr cannot name
//! exactly one file, the observation stays unavailable: never a newest-file
//! guess.
//!
//! A session file is worker-writable, so every read is bounded and only the
//! model name is taken; anything missing, oversize or odd is "unknown",
//! never an error, and never more than the displayed observation.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, TaskId, TaskSpec};
use brgr_store::Store;
use serde_json::Value;

use crate::{LaunchEnvelope, Paths, codex_integration};

/// The longest session log read for a model name; a larger file is "unknown".
const MAX_SESSION_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// The longest recorded-session index a task revision accumulates.
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// Reads the model a pane worker's session named for `attempt` reports, when
/// brgr can name that worker's own session file exactly.
pub(crate) fn pane_session_model(
    paths: &Paths,
    spec: &TaskSpec,
    attempt: AttemptId,
) -> Option<String> {
    let launch: LaunchEnvelope =
        serde_json::from_slice(&fs::read(paths.launch(spec.task_id, spec.revision)).ok()?).ok()?;
    if !launch.pane_mode {
        return None;
    }
    let kind = launch
        .manifest
        .as_ref()?
        .launch
        .interactive
        .as_ref()?
        .herdr_kind
        .as_str();
    match kind {
        "claude" => {
            let home = codex_integration::claude_home().ok()?;
            read_claude_model(
                &claude_transcript(&home, &spec.workspace, attempt),
                &attempt.to_string(),
            )
        }
        "codex" => {
            let rollout = recorded_codex_rollout(&paths.runs, spec.task_id, spec.revision)?;
            read_codex_model(&rollout, Path::new(&spec.workspace))
        }
        _ => None,
    }
}

/// Gives a supervisor the pane session reader so sealed results can name the
/// model a Claude Code or Codex worker's own session record reported.
pub(crate) fn install(paths: &Paths, supervisor: &mut brgr_core::Supervisor) {
    let paths = paths.clone();
    supervisor.observe_session_models(Box::new(move |spec, attempt| {
        pane_session_model(&paths, spec, attempt)
    }));
}

/// Claude Code's project directory for a workspace path: every character that
/// is not an ASCII letter or digit becomes `-`.
fn claude_project_dir(workspace: &str) -> String {
    workspace
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// The transcript brgr's `--session-id <attempt>` names.
fn claude_transcript(claude_home: &Path, workspace: &str, attempt: AttemptId) -> PathBuf {
    claude_home
        .join("projects")
        .join(claude_project_dir(workspace))
        .join(format!("{attempt}.jsonl"))
}

/// The last model the transcript's own assistant messages name for `session`.
/// Lines for another session, or with no `sessionId`, are not the worker's.
fn read_claude_model(path: &Path, session: &str) -> Option<String> {
    read_bounded(path, MAX_SESSION_LOG_BYTES)?
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|value| {
            value.get("type").and_then(Value::as_str) == Some("assistant")
                && value.get("sessionId").and_then(Value::as_str) == Some(session)
        })
        .filter_map(|value| {
            value
                .pointer("/message/model")
                .and_then(Value::as_str)
                .and_then(clean_model)
        })
        .next_back()
}

/// The last model the rollout's `turn_context` records name while the session
/// ran in `workspace`. Turns for another directory are not this task's.
fn read_codex_model(path: &Path, workspace: &Path) -> Option<String> {
    read_bounded(path, MAX_SESSION_LOG_BYTES)?
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|value| {
            value.get("type").and_then(Value::as_str) == Some("turn_context")
                && value
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .is_some_and(|cwd| same_directory(cwd, workspace))
        })
        .filter_map(|value| {
            value
                .pointer("/payload/model")
                .and_then(Value::as_str)
                .and_then(clean_model)
        })
        .next_back()
}

/// The recorded-session index of a Codex task revision.
fn codex_record(runs: &Path, task: TaskId, revision: u32) -> PathBuf {
    runs.join(format!("{task}-r{revision}.codex-session.jsonl"))
}

/// The one rollout the installed hook recorded for this task's Codex worker.
/// Zero or several distinct recordings mean brgr cannot say which file is the
/// worker's.
fn recorded_codex_rollout(runs: &Path, task: TaskId, revision: u32) -> Option<PathBuf> {
    let bytes = read_bounded(&codex_record(runs, task, revision), MAX_RECORD_BYTES)?;
    let mut rollouts: Vec<PathBuf> = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let rollout = PathBuf::from(
            serde_json::from_slice::<Value>(line)
                .ok()?
                .get("rollout")
                .and_then(Value::as_str)?
                .to_owned(),
        );
        if !rollouts.contains(&rollout) {
            rollouts.push(rollout);
        }
    }
    match rollouts.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Records a Codex session's rollout for the task whose workspace the session
/// runs in. The installed Codex hook fires for worker sessions too; a session
/// outside a task worktree records nothing. Never fails the hook.
pub(crate) fn record_codex_session(paths: &Paths, cwd: &Path, session_id: &str, rollout: &Path) {
    if let Ok(sessions) = codex_integration::codex_home().map(|home| home.join("sessions"))
        && let Err(error) = record_codex_session_in(paths, &sessions, cwd, session_id, rollout)
    {
        eprintln!("brgr Codex session record failed: {error}");
    }
}

/// Binds a session's rollout to exactly one task when the session's own
/// `cwd` is that task's workspace and the rollout lies inside the harness's
/// own sessions directory.
fn record_codex_session_in(
    paths: &Paths,
    sessions: &Path,
    cwd: &Path,
    session_id: &str,
    rollout: &Path,
) -> Result<()> {
    let name = rollout
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !name.starts_with("rollout-")
        || !rollout
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        bail!("not a Codex rollout file name");
    }
    let rollout = rollout
        .canonicalize()
        .context("recorded rollout path does not resolve")?;
    if !rollout.starts_with(sessions.canonicalize().context("Codex sessions missing")?) {
        bail!("recorded rollout is outside the Codex sessions directory");
    }
    let store = Store::open(&paths.store)?;
    let mut matches = store
        .tasks(100)?
        .into_iter()
        .filter(|task| same_directory(&task.workspace, cwd));
    let Some(task) = matches.next() else {
        return Ok(());
    };
    if matches.next().is_some() {
        return Ok(());
    }
    let record = codex_record(&paths.runs, task.task_id, task.revision);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let line = serde_json::json!({
        "session_id": session_id,
        "rollout": rollout.to_string_lossy(),
        "at": at,
    });
    let mut file = OpenOptions::new().create(true).append(true).open(&record)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Reads at most `limit` bytes of a file; `None` when it is absent,
/// unreadable, or over the limit.
fn read_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > limit {
        return None;
    }
    Some(bytes)
}

/// A model name worth showing: nonempty, no control characters, bounded
/// length like every other recorded model.
fn clean_model(raw: &str) -> Option<String> {
    let model = raw.trim();
    if model.is_empty() || model.chars().any(char::is_control) {
        return None;
    }
    Some(model.chars().take(brgr_runner::MAX_MODEL_CHARS).collect())
}

/// Whether two path strings name the same directory, following links when
/// they resolve.
fn same_directory(left: &str, right: &Path) -> bool {
    if left == right.as_os_str() {
        return true;
    }
    match (Path::new(left).canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{
        ArtifactContract, AttemptBudget, EvidenceSpec, OwnerId, Route, SCHEMA_V1, TaskInstructions,
    };
    use tempfile::TempDir;

    const CLAUDE_SESSION: &str = "11111111-2222-3333-4444-555555555555";
    const CLAUDE_TRANSCRIPT: &str =
        include_str!("../../../testdata/fixtures/claude-session/transcript.jsonl");
    const CODEX_ROLLOUT: &str =
        include_str!("../../../testdata/fixtures/codex-rollout/turn-context.jsonl");

    fn write(path: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        path.to_path_buf()
    }

    fn claude_line(session: &str, model: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"{session}\",\"cwd\":\"/repo/work\",\"message\":{{\"model\":\"{model}\",\"role\":\"assistant\"}}}}\n"
        )
    }

    fn rollout_line(cwd: &str, model: &str) -> String {
        format!(
            "{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"t\",\"cwd\":\"{cwd}\",\"model\":\"{model}\"}}}}\n"
        )
    }

    fn spec(workspace: &str, task_id: TaskId) -> TaskSpec {
        TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id,
            revision: 1,
            create_request_id: "req".to_owned(),
            owner_id: OwnerId::new("codex:test").unwrap(),
            objective: "Run".to_owned(),
            workspace: workspace.to_owned(),
            route: Route {
                harness_id: "local.codex".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec![],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
            },
            acceptance_criteria: vec!["done".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 30,
                max_attempts: 1,
            },
            instructions: TaskInstructions::default(),
            evidence: EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
        }
    }

    fn paths(dir: &TempDir) -> Paths {
        let home = dir.path().join("home");
        let paths = Paths {
            home: home.clone(),
            config: home.join("config.toml"),
            store: home.join("store"),
            registry: home.join("registry"),
            launches: home.join("launches"),
            runs: home.join("runs"),
            worktrees: home.join("worktrees"),
        };
        for path in [&paths.launches, &paths.runs, &paths.worktrees] {
            std::fs::create_dir_all(path).unwrap();
        }
        paths
    }

    #[test]
    fn claude_transcript_names_the_model_of_the_last_assistant_message() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir.path().join("session.jsonl"), CLAUDE_TRANSCRIPT);
        assert_eq!(
            read_claude_model(&path, CLAUDE_SESSION).as_deref(),
            Some("claude-fixture-2")
        );
    }

    #[test]
    fn claude_model_is_unavailable_for_a_missing_odd_or_foreign_file() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            read_claude_model(&dir.path().join("missing.jsonl"), CLAUDE_SESSION),
            None
        );
        for body in [
            "not json".to_owned(),
            "{\"type\":\"assistant\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"message\":{\"role\":\"assistant\"}}\n".to_owned(),
            "{\"type\":\"user\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\"}\n".to_owned(),
            claude_line("22222222-2222-3333-4444-555555555555", "claude-other"),
            "{\"type\":\"assistant\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"message\":{\"model\":\"a\\u0007b\",\"role\":\"assistant\"}}\n".to_owned(),
            "{\"type\":\"assistant\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\",\"message\":{\"model\":\"\",\"role\":\"assistant\"}}\n".to_owned(),
        ] {
            let path = write(&dir.path().join("case.jsonl"), &body);
            assert_eq!(read_claude_model(&path, CLAUDE_SESSION), None, "{body}");
        }
    }

    #[test]
    fn codex_rollout_names_the_model_of_the_last_turn() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("repo").join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let rollout = CODEX_ROLLOUT.replace("/repo/work", &workspace.to_string_lossy());
        let path = write(&dir.path().join("rollout.jsonl"), &rollout);
        assert_eq!(
            read_codex_model(&path, &workspace).as_deref(),
            Some("gpt-fixture-8")
        );
    }

    #[test]
    fn codex_model_is_unavailable_for_a_missing_odd_or_foreign_file() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("repo").join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        assert_eq!(
            read_codex_model(&dir.path().join("missing.jsonl"), &workspace),
            None
        );
        for body in [
            "not json".to_owned(),
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\"}}\n".to_owned(),
            rollout_line("/other/repo", "gpt-other"),
            format!(
                "{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":{:?},\"model\":\"a\\u0007b\"}}}}\n",
                workspace.to_string_lossy()
            ),
            format!(
                "{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":{:?},\"model\":\"\"}}}}\n",
                workspace.to_string_lossy()
            ),
        ] {
            let path = write(&dir.path().join("case.jsonl"), &body);
            assert_eq!(read_codex_model(&path, &workspace), None, "{body}");
        }
    }

    #[test]
    fn an_oversize_session_log_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let mut body = String::new();
        while body.len() as u64 <= MAX_SESSION_LOG_BYTES {
            body.push_str(&claude_line(CLAUDE_SESSION, "claude-fixture-2"));
        }
        let path = write(&dir.path().join("large.jsonl"), &body);
        assert_eq!(read_claude_model(&path, CLAUDE_SESSION), None);
    }

    #[test]
    fn claude_project_dirs_follow_claude_code_encoding() {
        assert_eq!(
            claude_project_dir("/Users/justn/Library/Application Support/brgr"),
            "-Users-justn-Library-Application-Support-brgr"
        );
        assert_eq!(claude_project_dir("/repo/work"), "-repo-work");
        assert_eq!(claude_project_dir("/repo/a.b_c"), "-repo-a-b-c");
    }

    #[test]
    fn a_codex_record_must_name_exactly_one_rollout() {
        let dir = TempDir::new().unwrap();
        let task = TaskId::new();
        assert_eq!(recorded_codex_rollout(dir.path(), task, 1), None);
        let record = codex_record(dir.path(), task, 1);
        write(
            &record,
            "{\"session_id\":\"s1\",\"rollout\":\"/x/rollout-a.jsonl\"}\n\
             {\"session_id\":\"s1\",\"rollout\":\"/x/rollout-a.jsonl\"}\n",
        );
        assert_eq!(
            recorded_codex_rollout(dir.path(), task, 1),
            Some(PathBuf::from("/x/rollout-a.jsonl"))
        );
        write(
            &record,
            "{\"session_id\":\"s1\",\"rollout\":\"/x/rollout-a.jsonl\"}\n\
             {\"session_id\":\"s2\",\"rollout\":\"/x/rollout-b.jsonl\"}\n",
        );
        assert_eq!(recorded_codex_rollout(dir.path(), task, 1), None);
        write(&record, "not json\n");
        assert_eq!(recorded_codex_rollout(dir.path(), task, 1), None);
    }

    #[test]
    fn only_an_exact_workspace_records_a_codex_session() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let worktree = paths.worktrees.join("repo").join("aaaaaaaa");
        std::fs::create_dir_all(&worktree).unwrap();
        let sessions = dir.path().join("codexhome").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let rollout = write(
            &sessions.join("rollout-t-111.jsonl"),
            &rollout_line(&worktree.to_string_lossy(), "gpt-fixture-8"),
        );
        let task = TaskId::new();
        let task_spec = spec(&worktree.to_string_lossy(), task);
        Store::open(&paths.store)
            .unwrap()
            .record_task(&task_spec, "req")
            .unwrap();

        record_codex_session_in(&paths, &sessions, &worktree, "s1", &rollout).unwrap();
        assert_eq!(
            recorded_codex_rollout(&paths.runs, task, 1),
            Some(rollout.canonicalize().unwrap())
        );
        // The same session's later hook events do not add another rollout.
        record_codex_session_in(&paths, &sessions, &worktree, "s1", &rollout).unwrap();
        assert_eq!(
            recorded_codex_rollout(&paths.runs, task, 1),
            Some(rollout.canonicalize().unwrap())
        );

        // A path outside the harness's own sessions directory is not one.
        let foreign = write(&dir.path().join("elsewhere.jsonl"), "");
        let other_task = TaskId::new();
        let other_tree = paths.worktrees.join("repo").join("bbbbbbbb");
        std::fs::create_dir_all(&other_tree).unwrap();
        let mut other_spec = spec(&other_tree.to_string_lossy(), other_task);
        other_spec.create_request_id = "req2".to_owned();
        Store::open(&paths.store)
            .unwrap()
            .record_task(&other_spec, "req2")
            .unwrap();
        assert!(record_codex_session_in(&paths, &sessions, &other_tree, "s2", &foreign).is_err());
        assert_eq!(recorded_codex_rollout(&paths.runs, other_task, 1), None);

        // A directory that is no task's workspace records nothing.
        let stray = dir.path().join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        record_codex_session_in(&paths, &sessions, &stray, "s3", &rollout).unwrap();
        let record_text = fs::read_to_string(codex_record(&paths.runs, task, 1)).unwrap();
        assert!(!record_text.contains("s3"), "{record_text}");
    }

    #[test]
    fn other_harness_kinds_have_no_session_model() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        for kind in ["gjc", "gemini", "cursor"] {
            let task = TaskId::new();
            let task_spec = spec("/repo/work", task);
            let launch = serde_json::json!({
                "spec": task_spec,
                "harness_id": format!("local.{kind}"),
                "protocol_generation": "brgr-v1",
                "keep_pane": false,
                "pane_mode": true,
                "claimant": "supervisor",
                "manifest": {
                    "schema": "brgr.harness/v1",
                    "id": format!("local.{kind}"),
                    "adapter": "process/v1",
                    "executable": "/bin/echo",
                    "probe": {"version_argv": ["--version"], "help_argv": ["--help"]},
                    "launch": {
                        "argv": [],
                        "model_argv": [],
                        "effort_argv": [],
                        "env_allow": [],
                        "mode": "one_shot",
                        "permission_argv": {},
                        "interactive": {"herdr_kind": kind, "argv": []}
                    },
                    "result": {"source": {"kind": "stdout"}, "media_type": "text/plain", "max_bytes": 1024, "success_exit_codes": [0]},
                    "capabilities": {}
                },
            });
            std::fs::write(
                paths.launch(task, 1),
                serde_json::to_vec_pretty(&launch).unwrap(),
            )
            .unwrap();
            assert_eq!(
                pane_session_model(&paths, &task_spec, AttemptId::new()),
                None,
                "{kind}"
            );
        }
    }
}
