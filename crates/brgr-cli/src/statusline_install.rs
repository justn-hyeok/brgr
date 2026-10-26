//! Opt-in: put `brgr __context` in front of a harness's status line.
//!
//! Claude Code and Cursor Agent give their status line command the session's
//! context (Claude Code also its plan usage), and each has room for only one such
//! command. `integrate claude install --context` and `integrate cursor install`
//! make brgr's recorder that command and chain to the one the user had, so the
//! status line looks the same. Nothing else in the config file is touched: it is
//! edited as text, only the `statusLine` member changes, and the previous member
//! is kept verbatim so `uninstall` restores it exactly. A symlinked config (a
//! managed one) is edited at its target.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Harness {
    Claude,
    Cursor,
}

impl Harness {
    fn receipt(self) -> &'static str {
        match self {
            Self::Claude => "claude-statusline.json",
            Self::Cursor => "cursor-statusline.json",
        }
    }

    /// The `brgr __context` flag that tells the recorder which harness this is.
    fn source_flag(self) -> &'static str {
        match self {
            Self::Claude => "",
            Self::Cursor => " --source cursor",
        }
    }

    fn config_file(self) -> Result<PathBuf> {
        Ok(match self {
            Self::Claude => crate::codex_integration::claude_home()?.join("settings.json"),
            Self::Cursor => {
                let dir = match std::env::var_os("CURSOR_CONFIG_DIR") {
                    Some(path) => PathBuf::from(path),
                    None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                        .join(".cursor"),
                };
                dir.join("cli-config.json")
            }
        })
    }
}
/// Marks a status line command as brgr's recorder.
const MARKER: &str = " __context";

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    /// The file that was edited: the target of a symlinked `settings.json`.
    settings: PathBuf,
    /// The `statusLine.command` brgr wrote.
    command: String,
    /// The previous `statusLine` member's value, verbatim; `None` if there was none.
    original: Option<String>,
    backup: PathBuf,
}

struct Member {
    /// Start of the member's key.
    start: usize,
    value_start: usize,
    value_end: usize,
}

fn skip_space(text: &str, mut at: usize) -> usize {
    while text.as_bytes().get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

fn skip_string(text: &str, start: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut at = start + 1;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b'"' => return Ok(at + 1),
            _ => at += 1,
        }
    }
    bail!("unterminated string in settings.json")
}

fn skip_value(text: &str, start: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    match bytes.get(start) {
        Some(b'"') => skip_string(text, start),
        Some(b'{' | b'[') => {
            let mut depth = 0_usize;
            let mut at = start;
            while let Some(&byte) = bytes.get(at) {
                match byte {
                    b'"' => {
                        at = skip_string(text, at)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(at + 1);
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            bail!("unterminated value in settings.json")
        }
        Some(_) => {
            let mut at = start;
            while bytes.get(at).is_some_and(|byte| {
                !byte.is_ascii_whitespace() && !matches!(byte, b',' | b'}' | b']')
            }) {
                at += 1;
            }
            Ok(at)
        }
        None => bail!("settings.json ends inside an object"),
    }
}

/// Index just after the top-level object's `{`.
fn object_open(text: &str) -> Result<usize> {
    let at = skip_space(text, 0);
    if text.as_bytes().get(at) != Some(&b'{') {
        bail!("settings.json is not a JSON object");
    }
    Ok(at + 1)
}

/// Finds a member of the top-level object by key, without parsing the rest.
fn find_member(text: &str, key: &str) -> Result<Option<Member>> {
    let mut at = object_open(text)?;
    loop {
        at = skip_space(text, at);
        match text.as_bytes().get(at) {
            Some(b'}') | None => return Ok(None),
            Some(b'"') => {}
            Some(_) => bail!("unexpected text in settings.json"),
        }
        let key_end = skip_string(text, at)?;
        let name: String = serde_json::from_str(&text[at..key_end])?;
        let colon = skip_space(text, key_end);
        if text.as_bytes().get(colon) != Some(&b':') {
            bail!("settings.json has a key without a value");
        }
        let value_start = skip_space(text, colon + 1);
        let value_end = skip_value(text, value_start)?;
        if name == key {
            return Ok(Some(Member {
                start: at,
                value_start,
                value_end,
            }));
        }
        at = skip_space(text, value_end);
        if text.as_bytes().get(at) == Some(&b',') {
            at += 1;
        }
    }
}

/// The whitespace a new first member should start with, matching the file.
fn first_member_indent(text: &str, open: usize) -> Option<String> {
    let rest = &text[open..];
    let newline = rest.find('\n')?;
    if !rest[..newline].trim().is_empty() {
        return None;
    }
    let after = &rest[newline + 1..];
    Some(
        after
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect(),
    )
}

fn render(value: &Value, indent: Option<&str>) -> Result<String> {
    let Some(indent) = indent else {
        return Ok(serde_json::to_string(value)?);
    };
    let pretty = serde_json::to_string_pretty(value)?;
    Ok(pretty.replace('\n', &format!("\n{indent}")))
}

/// Replaces the `statusLine` member's value, or adds it as the first member.
fn with_status_line(text: &str, value: &Value) -> Result<String> {
    if let Some(member) = find_member(text, "statusLine")? {
        let indent = line_indent(text, member.start);
        let rendered = render(value, indent.as_deref())?;
        return Ok(format!(
            "{}{rendered}{}",
            &text[..member.value_start],
            &text[member.value_end..]
        ));
    }
    let open = object_open(text)?;
    let indent = first_member_indent(text, open);
    let empty = text.as_bytes().get(skip_space(text, open)) == Some(&b'}');
    let rendered = render(value, indent.as_deref())?;
    Ok(match (indent, empty) {
        (Some(indent), true) => {
            format!(
                "{}\n{indent}\"statusLine\": {rendered}\n{}",
                &text[..open],
                &text[open..].trim_start()
            )
        }
        (Some(indent), false) => {
            format!(
                "{}\n{indent}\"statusLine\": {rendered},{}",
                &text[..open],
                &text[open..]
            )
        }
        (None, true) => format!(
            "{}\"statusLine\": {rendered}{}",
            &text[..open],
            &text[open..]
        ),
        (None, false) => format!(
            "{}\"statusLine\": {rendered},{}",
            &text[..open],
            &text[open..]
        ),
    })
}

/// The whitespace in front of `at` on its own line, when only whitespace is.
fn line_indent(text: &str, at: usize) -> Option<String> {
    let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let before = &text[line_start..at];
    (line_start > 0 && before.chars().all(|c| c == ' ' || c == '\t')).then(|| before.to_owned())
}

/// Removes the `statusLine` member that `with_status_line` added.
fn without_status_line(text: &str) -> Result<String> {
    let Some(member) = find_member(text, "statusLine")? else {
        return Ok(text.to_owned());
    };
    let mut start = member.start;
    if let Some(indent) = line_indent(text, start) {
        // The member's own line, including its newline.
        start -= indent.len() + 1;
    }
    let mut end = member.value_end;
    let after = skip_space(text, end);
    if text.as_bytes().get(after) == Some(&b',') {
        end = after + 1;
    } else {
        // Last member: take the comma before it instead.
        let before = text[..start].trim_end();
        if before.ends_with(',') {
            start = before.len() - 1;
        }
    }
    Ok(format!("{}{}", &text[..start], &text[end..]))
}

/// A command safe to place after `brgr __context --`: the shell Claude Code
/// runs it with must see the same words, not new pipes or redirections.
fn chainable(command: &str) -> Result<()> {
    if command
        .chars()
        .any(|c| matches!(c, '|' | '&' | ';' | '<' | '>' | '\n' | '\r' | '`'))
        || command.contains("$(")
    {
        bail!(
            "the existing statusLine command uses shell operators, which brgr will not wrap; \
             simplify it to a single command, or leave it and use `brgr config` thresholds without `--context`"
        );
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn status_line_command(value: &Value) -> Option<&str> {
    let object = value.get("statusLine")?.as_object()?;
    (object.get("type").and_then(Value::as_str) == Some("command"))
        .then(|| object.get("command").and_then(Value::as_str))
        .flatten()
}

fn read_receipt(brgr_home: &Path, harness: Harness) -> Result<Option<Receipt>> {
    let path = brgr_home.join(harness.receipt());
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

/// What the settings' current `statusLine` means for an install: its raw value
/// to restore later, and the command to chain to.
fn previous_status_line(
    text: &str,
    parsed: &Value,
    receipt: Option<&Receipt>,
) -> Result<(Option<String>, Option<String>)> {
    if parsed.get("statusLine").is_none() {
        return Ok((None, None));
    }
    let command = status_line_command(parsed)
        .context("the existing statusLine is not a plain command; brgr will not wrap it")?;
    if !command.contains(MARKER) {
        chainable(command)?;
        let member = find_member(text, "statusLine")?.context("statusLine member vanished")?;
        return Ok((
            Some(text[member.value_start..member.value_end].to_owned()),
            Some(command.to_owned()),
        ));
    }
    // Already brgr's: keep what it replaced, from the receipt.
    let receipt = receipt
        .filter(|receipt| receipt.command == command)
        .context("statusLine already runs brgr but no receipt matches; remove it by hand")?;
    let original = receipt.original.clone();
    let chain = original
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()?
        .and_then(|value| {
            value
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    Ok((original, chain))
}

pub(crate) fn install(brgr_home: &Path, harness: Harness, executable: &Path) -> Result<Value> {
    let link = harness.config_file()?;
    // A managed config is often a symlink; edit what it points to, not the link.
    let target = if link.exists() {
        link.canonicalize()?
    } else {
        link.clone()
    };
    let text = if target.exists() {
        fs::read_to_string(&target)?
    } else {
        "{}\n".to_owned()
    };
    let parsed: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON; brgr will not edit it",
            target.display()
        )
    })?;
    if !parsed.is_object() {
        bail!("{} is not a JSON object", target.display());
    }
    let receipt = read_receipt(brgr_home, harness)?;
    let (original, chain) = previous_status_line(&text, &parsed, receipt.as_ref())?;
    let mut command = format!(
        "{} --home {} __context{}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&brgr_home.to_string_lossy()),
        harness.source_flag(),
    );
    if let Some(chain) = &chain {
        command.push_str(" -- ");
        command.push_str(chain);
    }
    if let Some(receipt) = &receipt
        && receipt.command == command
        && receipt.settings == target
        && status_line_command(&parsed) == Some(command.as_str())
    {
        return Ok(json!({"status": "already_installed", "settings": target}));
    }
    // Keep the previous object's other fields (padding and the like).
    let mut member = original
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"type": "command"}));
    member["type"] = json!("command");
    member["command"] = json!(command);
    let edited = with_status_line(&text, &member)?;
    let check: Value =
        serde_json::from_str(&edited).context("the edit would break settings.json")?;
    if status_line_command(&check) != Some(command.as_str()) {
        bail!("the edit did not produce the intended statusLine");
    }
    let backup = brgr_home.join("backups").join(format!(
        "claude-settings-{}.json",
        crate::context_signal::now()
    ));
    write_atomic(&backup, text.as_bytes(), 0o600)?;
    let mode = fs::metadata(&target).map_or(0o600, |meta| meta.permissions().mode() & 0o777);
    write_atomic(&target, edited.as_bytes(), mode)?;
    let receipt = Receipt {
        settings: target.clone(),
        command,
        original,
        backup: backup.clone(),
    };
    write_atomic(
        &brgr_home.join(harness.receipt()),
        &serde_json::to_vec_pretty(&receipt)?,
        0o600,
    )?;
    Ok(json!({
        "status": "installed",
        "settings": target,
        "symlink": link.is_symlink(),
        "backup": backup,
        "chained": chain.is_some(),
    }))
}

pub(crate) fn status(brgr_home: &Path, harness: Harness) -> Result<Value> {
    let Some(receipt) = read_receipt(brgr_home, harness)? else {
        return Ok(json!({"status": "not_installed"}));
    };
    let current = fs::read_to_string(&receipt.settings)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let intact = current
        .as_ref()
        .and_then(status_line_command)
        .is_some_and(|command| command == receipt.command);
    Ok(json!({
        "status": if intact { "installed" } else { "drifted" },
        "settings": receipt.settings,
    }))
}

pub(crate) fn uninstall(brgr_home: &Path, harness: Harness) -> Result<Value> {
    let Some(receipt) = read_receipt(brgr_home, harness)? else {
        return Ok(json!({"status": "not_installed"}));
    };
    let text = fs::read_to_string(&receipt.settings).ok();
    let current = text
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let intact = current
        .as_ref()
        .and_then(status_line_command)
        .is_some_and(|command| command == receipt.command);
    let restored = if let (true, Some(text)) = (intact, text) {
        let edited = match &receipt.original {
            Some(original) => {
                let member = find_member(&text, "statusLine")?.context("statusLine vanished")?;
                format!(
                    "{}{original}{}",
                    &text[..member.value_start],
                    &text[member.value_end..]
                )
            }
            None => without_status_line(&text)?,
        };
        serde_json::from_str::<Value>(&edited).context("the restore would break settings.json")?;
        let mode =
            fs::metadata(&receipt.settings).map_or(0o600, |meta| meta.permissions().mode() & 0o777);
        write_atomic(&receipt.settings, edited.as_bytes(), mode)?;
        true
    } else {
        // The user changed it since; leave their setting alone.
        false
    };
    fs::remove_file(brgr_home.join(harness.receipt()))?;
    Ok(json!({"status": "uninstalled", "status_line_restored": restored}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(command: &str) -> Value {
        json!({"type": "command", "command": command})
    }

    /// Install then remove must give the file back byte for byte.
    fn round_trip(text: &str) {
        let added = with_status_line(text, &member("brgr __context")).unwrap();
        let value: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(
            status_line_command(&value),
            Some("brgr __context"),
            "{added}"
        );
        assert_eq!(without_status_line(&added).unwrap(), text, "{added}");
    }

    #[test]
    fn adding_and_removing_the_member_restores_the_file_exactly() {
        round_trip("{\n  \"model\": \"opus\",\n  \"hooks\": {\"a\": [1, 2]}\n}\n");
        round_trip("{\n\t\"model\": \"opus\"\n}\n");
        round_trip("{\"model\":\"opus\",\"env\":{\"A\":\"}{\\\"\"}}");
        round_trip("  {\n    \"only\": true\n  }\n");
    }

    #[test]
    fn an_empty_object_gets_a_valid_member_and_loses_it_again() {
        let added = with_status_line("{}\n", &member("brgr __context")).unwrap();
        assert!(serde_json::from_str::<Value>(&added).is_ok(), "{added}");
        let removed = without_status_line(&added).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&removed).unwrap(), json!({}));
    }

    #[test]
    fn replacing_the_member_touches_nothing_else() {
        let text = "{\n  \"a\": 1,\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"bash x.sh\"\n  },\n  \"z\": [\"}\", {\"k\": \"v\"}]\n}\n";
        let edited = with_status_line(text, &member("brgr __context -- bash x.sh")).unwrap();
        let before = find_member(text, "statusLine").unwrap().unwrap();
        let after = find_member(&edited, "statusLine").unwrap().unwrap();
        assert_eq!(&text[..before.value_start], &edited[..after.value_start]);
        assert_eq!(&text[before.value_end..], &edited[after.value_end..]);
        assert!(edited.contains("brgr __context -- bash x.sh"));
    }

    #[test]
    fn a_key_that_only_appears_inside_a_value_is_not_the_member() {
        let text = "{\"note\": \"\\\"statusLine\\\": 1\", \"nested\": {\"statusLine\": 2}}";
        assert!(find_member(text, "statusLine").unwrap().is_none());
    }

    #[test]
    fn a_command_with_shell_operators_is_not_wrapped() {
        for command in [
            "a | b",
            "a && b",
            "a; b",
            "a > out",
            "a < in",
            "echo `x`",
            "echo $(x)",
            "a\nb",
        ] {
            assert!(chainable(command).is_err(), "{command}");
        }
        assert!(chainable("bash /Users/me/.claude/statusline-command.sh").is_ok());
        assert!(chainable("node \"/path with space/s.js\" --flag").is_ok());
    }
}
