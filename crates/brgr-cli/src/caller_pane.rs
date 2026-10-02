//! Whether `HERDR_PANE_ID` names the pane this process actually runs in.
//!
//! A process inherits `HERDR_PANE_ID` from whatever started it, and that is not
//! always its pane. Codex runs tool commands and hooks in a shared app-server
//! daemon, detached from every terminal, which keeps the Herdr environment of
//! the pane that first started it — a pane that may be closed, or may belong to
//! another Codex session. Trusting it made pane mode split beside a pane that
//! no longer existed (`pane_not_found`), and could have bound one Codex
//! session's completion notices to another session's pane.
//!
//! A process running in a Herdr pane descends from that pane's shell, which the
//! Herdr server started. So the claim is trusted only when walking up the
//! process tree reaches a `herdr` process, and no ancestor on the way carries a
//! different `HERDR_PANE_ID`. A daemon's chain ends at launchd and never
//! reaches Herdr.

use serde_json::Value;
use std::{
    collections::HashMap,
    env, fs,
    io::{Read as _, Seek as _},
    path::Path,
    process::Command,
};

/// The caller's Herdr pane, when this process provably runs inside it.
pub(crate) fn verified() -> Option<String> {
    if let Some(pane) = crate::invocation::current().pane.as_deref() {
        return crate::current_session()
            .ok()
            .flatten()
            .and_then(|session| for_session(&session, Some(pane)));
    }
    let native = inherited();
    if native.is_some() {
        return native;
    }
    crate::current_session()
        .ok()
        .flatten()
        .and_then(|session| for_session(&session, crate::invocation::current().pane.as_deref()))
}

fn inherited() -> Option<String> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        return None;
    }
    let pane = env::var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.trim().is_empty())?;
    #[cfg(debug_assertions)]
    if env::var("BRGR_TEST_TRUST_HERDR_PANE").as_deref() == Ok("1") {
        return Some(pane);
    }
    runs_inside(&pane).then_some(pane)
}

/// Resolve only a native session match, never focus, cwd uniqueness, or recency.
pub(crate) fn for_session(session: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(pane) = explicit {
        let live = herdr_json(&["agent", "get", pane])?;
        let agent = live.pointer("/result/agent")?;
        if agent.get("agent").and_then(Value::as_str) == Some("codex")
            && agent.get("pane_id").and_then(Value::as_str) == Some(pane)
            && agent
                .pointer("/agent_session/value")
                .and_then(Value::as_str)
                .is_some_and(|id| id == session)
        {
            return Some(pane.to_owned());
        }
        if agent
            .pointer("/agent_session/value")
            .and_then(Value::as_str)
            .is_some()
        {
            return None;
        }
        if inherited().as_deref() == Some(pane) {
            return Some(pane.to_owned());
        }
    }
    let agent = herdr_json(&["agent", "list"])?;
    let records = agent.pointer("/result/agents").and_then(Value::as_array);
    if let Some(records) = records {
        let matches: Vec<_> = records
            .iter()
            .filter(|item| {
                item.pointer("/agent_session/value").and_then(Value::as_str) == Some(session)
                    && explicit.is_none_or(|pane| {
                        item.get("pane_id").and_then(Value::as_str) == Some(pane)
                    })
            })
            .collect();
        if let [record] = matches.as_slice() {
            return record.get("pane_id")?.as_str().map(str::to_owned);
        }
    }
    // Optional native frontend receipts also cover Codex versions where Herdr
    // has not reported the session yet. Match the exact session and live PID.
    let cwd = env::current_dir().ok()?;
    let mut roots = vec![cwd.clone()];
    if let Some(primary) = crate::workspace::primary_checkout(&cwd) {
        roots.push(primary);
    }
    for root in roots {
        let Ok(entries) = fs::read_dir(root.join(".agent-progress/bridges")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|v| v != "json") {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > 65_536 {
                continue;
            }
            let Some(record) = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            if record.get("native_session").and_then(Value::as_str) != Some(session)
                || record.get("session").and_then(Value::as_str) != Some(session)
            {
                continue;
            }
            let Some(pane) = record.get("pane").and_then(Value::as_str) else {
                continue;
            };
            if explicit.is_some_and(|expected| expected != pane) {
                continue;
            }
            let Some(pid) = record.get("pid").and_then(Value::as_u64) else {
                continue;
            };
            let Some(process) = herdr_json(&["pane", "process-info", "--pane", pane]) else {
                continue;
            };
            if contains_pid(&process, pid) {
                return Some(pane.to_owned());
            }
        }
    }
    None
}

fn contains_pid(value: &Value, pid: u64) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            (matches!(key.as_str(), "pid" | "foreground_process_group")
                && value.as_u64() == Some(pid))
                || contains_pid(value, pid)
        }),
        Value::Array(values) => values.iter().any(|v| contains_pid(v, pid)),
        _ => false,
    }
}

fn herdr_json(args: &[&str]) -> Option<Value> {
    let binary = binary()?;
    let mut command = Command::new(binary);
    if let Ok(session) = env::var("HERDR_SESSION") {
        command.args(["--session", &session]);
    }
    let mut out = tempfile::tempfile().ok()?;
    let err = tempfile::tempfile().ok()?;
    let mut child = command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone().ok()?)
        .stderr(err)
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        let budget = if crate::invocation::is_hook() {
            200
        } else {
            2000
        };
        if started.elapsed() > std::time::Duration::from_millis(budget) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    if !status.success() || out.metadata().ok()?.len() > 1024 * 1024 {
        return None;
    }
    out.rewind().ok()?;
    let mut bytes = Vec::new();
    out.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn binary() -> Option<std::ffi::OsString> {
    if let Some(path) = env::var_os("HERDR_BIN_PATH") {
        return Some(path);
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join("herdr"))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
        .map(std::path::PathBuf::into_os_string)
}

/// Walks this process's ancestors to the Herdr server.
fn runs_inside(pane: &str) -> bool {
    let Some(table) = process_table() else {
        return false;
    };
    let mut pid = std::process::id();
    for _ in 0..64 {
        let Some((parent, command)) = table.get(&pid) else {
            return false;
        };
        if Path::new(command)
            .file_name()
            .is_some_and(|name| name == "herdr")
        {
            return true;
        }
        if pane_in_environment(pid).is_some_and(|claimed| claimed != pane) {
            return false;
        }
        if *parent <= 1 {
            return false;
        }
        pid = *parent;
    }
    false
}

/// Every process's parent and executable, from one `ps` call.
fn process_table() -> Option<HashMap<u32, (u32, String)>> {
    let output = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let parent = fields.next()?.parse().ok()?;
                let command = fields.collect::<Vec<_>>().join(" ");
                Some((pid, (parent, command)))
            })
            .collect(),
    )
}

/// The `HERDR_PANE_ID` in a process's environment, when `ps` can read it.
/// `ps -E` prints the arguments before the environment, so the last match is
/// the environment's.
fn pane_in_environment(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (_, rest) = text.rsplit_once(" HERDR_PANE_ID=")?;
    rest.split_whitespace().next().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test process's own chain: either it never reaches Herdr (CI), or it
    /// does through a pane whose id is not the one claimed here.
    #[test]
    fn an_unrelated_pane_claim_is_not_trusted() {
        assert!(!runs_inside("w999:p999-not-a-real-pane"));
    }

    #[test]
    fn the_process_table_includes_this_process() {
        let table = process_table().unwrap();
        assert!(table.contains_key(&std::process::id()));
    }
}
