use std::{
    collections::BTreeMap,
    env,
    fmt::Write as _,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const SKILL_TEXT: &str = r#"---
name: brgr
description: Delegate bounded coding work through brgr. Workers open as Herdr panes beside you, run with full permissions, and report back as sealed results you accept or reject. Use for Claude Code, Codex, Gemini CLI, GitHub Copilot CLI, Pi, OpenCode, Cursor, Devin, Cline, GJC and OMP workers.
---

# brgr

brgr hands bounded work to registered coding agents and seals what they return for you to decide. The user's request says what to do; these rules say how.

## Rules

1. Every worker opens as a Herdr pane split beside your pane, running the harness's own TUI: the first to the right of a wide pane or below a narrow one, later ones stacked under it in equal sizes while you keep your half, and in a new tab once that column is full or after `brgr config set worker-placement tab`. Do not use `--headless`.
2. When the work should be done by Claude, use `--harness local.claude-code`. Do not run Claude through another harness.
3. Workers always run with full permissions (yolo). Nothing lowers it; `--permission` is ignored.
4. Close what you open. Decide every result (`accept`, `reject`, or `result TASK --ack`) so brgr closes the worker's pane and removes its worktree once nothing in it would be lost. Use `--keep-pane` or `--keep-worktree` only when the user asks to keep one.
5. From Codex, start every brgr command with `--as SESSION`, the session in your calling context, and never add `--source-pane`. brgr finds your pane from what the Codex panes show; if it cannot, the run fails instead of opening beside someone else.
6. Leave a worker's screens to brgr. It accepts the workspace trust prompt, continue notices, Claude Code's bypass warning and new-MCP-server prompt, and skips update offers.

## The loop

1. Start: `brgr run "<objective>" --harness <id> --criterion "<observable check>"`. Add `--capture-diff` when you will keep code changes. It returns a task id at once.
2. Wait: do not poll. A `FROM BRGR` notice arrives in your pane when you are idle. If you must block, `brgr wait TASK` returns early with `state: awaiting_input` while the worker has a question for you.
3. Read: `brgr result TASK`, and for code `brgr diff TASK --stat` then `brgr diff TASK`.
4. Decide: `brgr accept TASK --reason "<why>"` or `brgr reject TASK --reason "<why>"`. To try again, `brgr revise TASK "<corrected objective>"`.
5. Integrate: `brgr apply TASK --workspace <repo>` checks the sealed diff; add `--execute` after accepting. Use the brgr-diff-review skill for code you keep.

## Notices

- `brgr_completion`: a result is ready. Check it against the criterion, then decide. It is not acceptance.
- `brgr_failure`: the run failed or was lost, and `reason` says why. Inspect with `brgr result TASK`; retry with `brgr revise TASK "<corrected objective>"`, or dismiss with `brgr result TASK --ack`. Until you do, every brgr command ends with a stderr note listing it.
- `brgr_question`: the worker is waiting on you. Read it with `brgr message list TASK --for owner`, answer with `brgr message send TASK --to worker --kind reply --reply-to MESSAGE_ID --body "<answer>"`, then `brgr message ack TASK MESSAGE_ID --for owner`.
- A repeated id is the same notice, not a new one.

## When a run fails

- The quoted screen says "insufficient credits", a usage limit, or an HTTP 4xx: the harness or its model failed, not brgr. Use another harness, or set a model with `brgr config set model <name> --harness <id>`.
- "a screen no brgr rule answers": the quote shows the screen (`herdr pane read PANE --source visible` shows it live). If a key clears it, answer before the three-minute deadline with `brgr input TASK --key <key>` or `--text <text>`, and tell the user so a rule can be added.
- "could not find the calling Codex pane": the command did not start with `--as SESSION`, or the pane is too narrow to show it.
- "finished without writing its report": the agent stopped early; the quoted screen says why.
- `brgr doctor` reports a harness as unhealthy after its CLI updated: register it again with `brgr harness add <executable> --workspace <scratch> --prompt "<small probe>"`.

## Messages and debate

Owner and worker talk with `brgr message send TASK --to owner|worker --kind note|question|reply --body "<text>"`; a reply adds `--reply-to MESSAGE_ID`. Read with `brgr message list TASK --for owner|worker` or `brgr message wait TASK --for owner|worker`, and acknowledge with `brgr message ack TASK MESSAGE_ID --for owner|worker`. Messages wait while the recipient is busy. A native screen is not a message: answer it with `brgr input TASK --key <key>`. A worker can hand back its answer without editing files with `brgr report TASK --body "<text>"`.

Siblings talk directly only when the user asks for a **debate**: `brgr debate start TASK_A TASK_B`. Participants receive the group id and use `brgr debate send GROUP --to TASK --kind question --body "<text>"`, `brgr debate list`, `brgr debate wait` and `brgr debate ack MESSAGE_ID`; a reply adds `--kind reply --reply-to MESSAGE_ID`. The owner uses `brgr debate status GROUP` and `brgr debate stop GROUP`.

## More

- Parallel attempts: start the same objective on several harnesses, compare with `brgr diff TASK --stat`, accept the best and reject the rest so every pane closes.
- Child tasks: `--enable-delegation` lets a worker start its own workers, and `--max-children <n>` caps them.
- Workspace: a clean Git repository gets a task worktree. `--snapshot-path <file>` carries chosen uncommitted files; `--evidence-file <file>` and `--capture-logs` add artifacts.
- Status: `brgr status`, `brgr status TASK --tree`, `brgr cancel TASK --tree`.
- Config: `brgr config init`, `brgr config show`, `brgr config set model <name> --harness <id>`, `brgr config set worker-placement tab`, `brgr config check`. User instructions live in `$BRGR_HOME/BRGR.md`.
- Housekeeping: `brgr errors` lists recorded failures. A worktree a decision kept (it holds work found nowhere else) stays until `brgr prune --apply`; `brgr prune` first reports what would go and why the rest stays.
- Registration: `brgr harness draft <executable>`, `brgr harness add <executable> --workspace <scratch> --prompt "<probe>"`, `brgr harness status <id>`, `brgr doctor`. Each harness keeps its own login and configuration.
- Owners: Codex sessions and Claude Code sessions (`claude:<CLAUDE_CODE_SESSION_ID>`) both receive notices.
"#;

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    hooks_path: PathBuf,
    skill_path: PathBuf,
    commands: BTreeMap<String, String>,
    skill_text: String,
    #[serde(default)]
    executable_path: Option<PathBuf>,
    #[serde(default)]
    executable_digest: Option<String>,
}

pub fn install(brgr_home: &Path) -> Result<Value> {
    let codex_home = codex_home()?;
    let hooks_path = codex_home.join("hooks.json");
    let skill_path = codex_home.join("skills/brgr/SKILL.md");
    let receipt_path = brgr_home.join("codex-integration.json");
    let previous_receipt = if receipt_path.exists() {
        Some(serde_json::from_slice::<Receipt>(&fs::read(
            &receipt_path,
        )?)?)
    } else {
        None
    };
    let original_skill = load_owned_skill(
        &skill_path,
        previous_receipt
            .as_ref()
            .map(|receipt| (receipt.skill_path.as_path(), receipt.skill_text.as_str())),
    )?;
    let original_hooks = if hooks_path.exists() {
        Some(fs::read(&hooks_path)?)
    } else {
        None
    };
    let executable = env::current_exe()?;
    let commands = hook_commands(&executable, brgr_home);
    let mut document = if let Some(bytes) = &original_hooks {
        serde_json::from_slice::<Value>(bytes)?
    } else {
        json!({"hooks": {}})
    };
    let hooks = document
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .context("Codex hooks.json must contain an object named hooks")?;

    remove_obsolete_hooks(hooks, &hooks_path, previous_receipt.as_ref(), &commands);

    let mut added = 0_u8;
    for (event, command) in &commands {
        let entries = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("Codex hook event {event} must be an array"))?;
        let present = entries.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|inner| {
                    inner.iter().any(|hook| {
                        hook.get("command").and_then(Value::as_str) == Some(command.as_str())
                    })
                })
        });
        if !present {
            entries.push(json!({
                "hooks": [{"type": "command", "command": command, "timeout": 1}]
            }));
            added = added.saturating_add(1);
        }
    }

    fs::create_dir_all(&codex_home)?;
    let hooks_changed = original_hooks.as_ref().is_none_or(|bytes| {
        serde_json::from_slice::<Value>(bytes).is_ok_and(|original| original != document)
    });
    let backup = if hooks_path.exists() && hooks_changed {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = hooks_path.with_file_name(format!("hooks.json.brgr-backup-{stamp}"));
        fs::copy(&hooks_path, &path)?;
        Some(path)
    } else {
        None
    };
    let skill_dir = skill_path.parent().context("skill path has no parent")?;
    fs::create_dir_all(skill_dir)?;
    fs::set_permissions(skill_dir, fs::Permissions::from_mode(0o700))?;
    let receipt = installation_receipt(&hooks_path, &skill_path, commands, &executable)?;
    let changed = (|| -> Result<()> {
        write_bytes_atomic(&skill_path, SKILL_TEXT.as_bytes())?;
        write_json_atomic(&hooks_path, &document)?;
        write_json_atomic(&receipt_path, &serde_json::to_value(&receipt)?)?;
        Ok(())
    })();
    if let Err(error) = changed {
        if let Some(bytes) = original_hooks {
            write_bytes_atomic(&hooks_path, &bytes)?;
        } else if hooks_path.exists() {
            fs::remove_file(&hooks_path)?;
        }
        if let Some(bytes) = original_skill {
            write_bytes_atomic(&skill_path, &bytes)?;
        } else if skill_path.exists() {
            fs::remove_file(&skill_path)?;
        }
        return Err(error);
    }
    Ok(json!({
        "status": "installed",
        "hooks_added": added,
        "backup": backup,
        "receipt": receipt_path,
    }))
}

fn installation_receipt(
    hooks_path: &Path,
    skill_path: &Path,
    commands: BTreeMap<String, String>,
    executable: &Path,
) -> Result<Receipt> {
    Ok(Receipt {
        hooks_path: hooks_path.to_path_buf(),
        skill_path: skill_path.to_path_buf(),
        commands,
        skill_text: SKILL_TEXT.to_owned(),
        executable_path: Some(executable.to_path_buf()),
        executable_digest: Some(file_digest(executable)?),
    })
}

fn load_owned_skill(path: &Path, previous: Option<(&Path, &str)>) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let current = String::from_utf8(bytes.clone())?;
    let owned_previous = previous
        .is_some_and(|(skill_path, skill_text)| skill_path == path && skill_text == current);
    if current != SKILL_TEXT && !owned_previous {
        bail!("refusing to overwrite a modified {}", path.display());
    }
    Ok(Some(bytes))
}

/// The Claude Code side installs only the skill: Claude Code owners receive
/// notices in their pane, so they need no hooks.
#[derive(Debug, Serialize, Deserialize)]
struct SkillReceipt {
    skill_path: PathBuf,
    skill_text: String,
}

const CLAUDE_RECEIPT: &str = "claude-integration.json";

pub fn claude_install(brgr_home: &Path) -> Result<Value> {
    let skill_path = claude_home()?.join("skills/brgr/SKILL.md");
    let receipt_path = brgr_home.join(CLAUDE_RECEIPT);
    let previous = read_skill_receipt(&receipt_path)?;
    load_owned_skill(
        &skill_path,
        previous
            .as_ref()
            .map(|receipt| (receipt.skill_path.as_path(), receipt.skill_text.as_str())),
    )?;
    let skill_dir = skill_path.parent().context("skill path has no parent")?;
    fs::create_dir_all(skill_dir)?;
    fs::set_permissions(skill_dir, fs::Permissions::from_mode(0o700))?;
    write_bytes_atomic(&skill_path, SKILL_TEXT.as_bytes())?;
    let receipt = SkillReceipt {
        skill_path: skill_path.clone(),
        skill_text: SKILL_TEXT.to_owned(),
    };
    write_json_atomic(&receipt_path, &serde_json::to_value(&receipt)?)?;
    Ok(json!({"status": "installed", "skill": skill_path, "receipt": receipt_path}))
}

pub fn claude_status(brgr_home: &Path) -> Result<Value> {
    let Some(receipt) = read_skill_receipt(&brgr_home.join(CLAUDE_RECEIPT))? else {
        return Ok(json!({"status": "not_installed"}));
    };
    let skill_matches = receipt.skill_path.exists()
        && fs::read_to_string(&receipt.skill_path)? == receipt.skill_text;
    let current_skill = receipt.skill_text == SKILL_TEXT;
    Ok(json!({
        "status": if skill_matches && current_skill { "installed" } else { "drifted" },
        "skill_matches": skill_matches,
        "current_skill": current_skill,
    }))
}

pub fn claude_uninstall(brgr_home: &Path) -> Result<Value> {
    let receipt_path = brgr_home.join(CLAUDE_RECEIPT);
    let Some(receipt) = read_skill_receipt(&receipt_path)? else {
        return Ok(json!({"status": "not_installed"}));
    };
    let skill_removed = if receipt.skill_path.exists()
        && fs::read_to_string(&receipt.skill_path)? == receipt.skill_text
    {
        fs::remove_file(&receipt.skill_path)?;
        true
    } else {
        false
    };
    fs::remove_file(receipt_path)?;
    Ok(json!({"status": "uninstalled", "skill_removed": skill_removed}))
}

fn read_skill_receipt(path: &Path) -> Result<Option<SkillReceipt>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn claude_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".claude"))
}

fn remove_obsolete_hooks(
    hooks: &mut serde_json::Map<String, Value>,
    hooks_path: &Path,
    previous: Option<&Receipt>,
    commands: &BTreeMap<String, String>,
) {
    let Some(previous) = previous.filter(|receipt| receipt.hooks_path == hooks_path) else {
        return;
    };
    for (event, old_command) in &previous.commands {
        if commands.get(event) == Some(old_command) {
            continue;
        }
        if let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            entries.retain(|entry| {
                !entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|inner| {
                        inner.len() == 1
                            && inner[0].get("command").and_then(Value::as_str)
                                == Some(old_command.as_str())
                    })
            });
        }
    }
}

pub fn status(brgr_home: &Path) -> Result<Value> {
    let receipt_path = brgr_home.join("codex-integration.json");
    if !receipt_path.exists() {
        return Ok(json!({"status": "not_installed"}));
    }
    let receipt: Receipt = serde_json::from_slice(&fs::read(&receipt_path)?)?;
    let hooks_present = installed_command_count(&receipt)?;
    let skill_matches = receipt.skill_path.exists()
        && fs::read_to_string(&receipt.skill_path)? == receipt.skill_text;
    let current_skill = receipt.skill_text == SKILL_TEXT;
    let current_hooks = match (
        receipt.executable_path.as_deref(),
        receipt.executable_digest.as_deref(),
    ) {
        (Some(executable), Some(expected_digest)) => {
            receipt.commands == hook_commands(executable, brgr_home)
                && file_digest(executable).is_ok_and(|actual| actual == expected_digest)
        }
        _ => receipt.commands == hook_commands(&env::current_exe()?, brgr_home),
    };
    let installed =
        hooks_present == receipt.commands.len() && skill_matches && current_skill && current_hooks;
    Ok(json!({
        "status": if installed { "installed" } else { "drifted" },
        "hooks_present": hooks_present,
        "hooks_expected": receipt.commands.len(),
        "skill_matches": skill_matches,
        "current_skill": current_skill,
        "current_hooks": current_hooks,
    }))
}

fn file_digest(path: &Path) -> Result<String> {
    let digest = Sha256::digest(fs::read(path)?);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}

pub fn uninstall(brgr_home: &Path) -> Result<Value> {
    let receipt_path = brgr_home.join("codex-integration.json");
    if !receipt_path.exists() {
        return Ok(json!({"status": "not_installed"}));
    }
    let receipt: Receipt = serde_json::from_slice(&fs::read(&receipt_path)?)?;
    if receipt.hooks_path.exists() {
        let mut document: Value = serde_json::from_slice(&fs::read(&receipt.hooks_path)?)?;
        let hooks = document
            .get_mut("hooks")
            .and_then(Value::as_object_mut)
            .context("Codex hooks.json must contain an object named hooks")?;
        for (event, command) in &receipt.commands {
            if let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) {
                entries.retain(|entry| {
                    !entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|inner| {
                            inner.len() == 1
                                && inner[0].get("command").and_then(Value::as_str)
                                    == Some(command.as_str())
                        })
                });
            }
        }
        write_json_atomic(&receipt.hooks_path, &document)?;
    }

    let skill_removed = if receipt.skill_path.exists()
        && fs::read_to_string(&receipt.skill_path)? == receipt.skill_text
    {
        fs::remove_file(&receipt.skill_path)?;
        true
    } else {
        false
    };
    fs::remove_file(receipt_path)?;
    Ok(json!({"status": "uninstalled", "skill_removed": skill_removed}))
}

fn installed_command_count(receipt: &Receipt) -> Result<usize> {
    if !receipt.hooks_path.exists() {
        return Ok(0);
    }
    let document: Value = serde_json::from_slice(&fs::read(&receipt.hooks_path)?)?;
    let hooks = document
        .get("hooks")
        .and_then(Value::as_object)
        .context("Codex hooks.json must contain an object named hooks")?;
    Ok(receipt
        .commands
        .iter()
        .filter(|(event, command)| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|inner| {
                                inner.iter().any(|hook| {
                                    hook.get("command").and_then(Value::as_str)
                                        == Some(command.as_str())
                                })
                            })
                    })
                })
        })
        .count())
}

fn hook_commands(executable: &Path, brgr_home: &Path) -> BTreeMap<String, String> {
    ["SessionStart", "UserPromptSubmit", "Stop"]
        .into_iter()
        .map(|event| {
            let value = match event {
                "SessionStart" => "session-start",
                "UserPromptSubmit" => "user-prompt-submit",
                "Stop" => "stop",
                _ => unreachable!(),
            };
            (
                event.to_owned(),
                format!(
                    "{} --home {} __hook {value}",
                    shell_quote(executable),
                    shell_quote(brgr_home)
                ),
            )
        })
        .collect()
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".codex"))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &[bytes.as_slice(), b"\n"].concat())
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits one skill command into argv, filling its placeholders with
    /// sample values so the command line can be parsed.
    fn sample_argv(command: &str) -> Vec<String> {
        let mut argv = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        for character in command.chars() {
            match character {
                '"' => quoted = !quoted,
                ' ' if !quoted => argv.push(std::mem::take(&mut word)),
                _ => word.push(character),
            }
        }
        argv.push(word);
        argv.retain(|word| !word.is_empty());
        argv.into_iter()
            .map(|word| {
                if !word.is_empty()
                    && word
                        .chars()
                        .all(|character| character.is_ascii_uppercase() || character == '_')
                {
                    "00000000-0000-4000-8000-000000000000".to_owned()
                } else if word.starts_with('<') && word.ends_with('>') {
                    "sample".to_owned()
                } else if word.contains('|') {
                    word.split('|').next().unwrap_or_default().to_owned()
                } else {
                    word
                }
            })
            .collect()
    }

    #[test]
    fn every_brgr_command_in_the_skill_parses() {
        use clap::Parser as _;
        let commands: Vec<&str> = SKILL_TEXT
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("brgr "))
            .collect();
        assert!(
            commands.len() > 30,
            "found only {} commands",
            commands.len()
        );
        for command in commands {
            if let Err(error) = crate::cli::Cli::try_parse_from(sample_argv(command)) {
                panic!("skill command `{command}` does not parse: {error}");
            }
        }
    }

    /// The README's commands, and the reference split out of it, are what a
    /// new reader copies first.
    #[test]
    fn every_brgr_command_in_the_readme_parses() {
        use clap::Parser as _;
        let readme = concat!(
            include_str!("../../../README.md"),
            "\n",
            include_str!("../../../docs/guides/reference.md")
        );
        let mut commands: Vec<String> = Vec::new();
        let mut prose = String::new();
        let mut fenced = false;
        for line in readme.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                let command = line.split(" # ").next().unwrap_or_default().trim();
                if command.starts_with("brgr ") {
                    commands.push(command.to_owned());
                }
            } else {
                prose.push_str(line);
                prose.push('\n');
            }
        }
        commands.extend(
            prose
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|span| span.starts_with("brgr ") && !span.contains('\n'))
                .map(str::to_owned),
        );
        assert!(
            commands.len() > 20,
            "found only {} commands",
            commands.len()
        );
        let failures: Vec<String> = commands
            .iter()
            .filter_map(|command| {
                crate::cli::Cli::try_parse_from(sample_argv(command))
                    .err()
                    .map(|error| format!("`{command}`: {}", error.kind()))
            })
            .collect();
        assert!(
            failures.is_empty(),
            "README commands that do not parse:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn skill_states_the_operating_rules() {
        for rule in ["--headless", "local.claude-code", "--keep-pane", "yolo"] {
            assert!(SKILL_TEXT.contains(rule), "skill no longer mentions {rule}");
        }
        assert!(!SKILL_TEXT.contains("without a pane"));
    }

    #[test]
    fn shell_quote_handles_spaces_and_apostrophes() {
        assert_eq!(
            shell_quote(Path::new("/tmp/a b/c'd")),
            "'/tmp/a b/c'\"'\"'d'"
        );
    }
}
