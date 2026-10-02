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

/// The Codex pane that runs `session`, proven from what the panes show.
///
/// Herdr's own record of which session a pane runs is not trusted: Codex runs
/// its hooks in a shared daemon whose environment names another session's pane,
/// so the record can bind a new session to the wrong pane. Instead the panes'
/// screens are read: Codex prints each command it runs, and a `brgr` command
/// carries its session, so exactly one Codex pane showing that call is the
/// caller's. Zero or several matches prove nothing, and nothing is guessed.
pub(crate) fn for_session(session: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(pane) = traced(session) {
        return explicit
            .is_none_or(|expected| expected == pane)
            .then_some(pane);
    }
    // A pane this process provably runs inside needs no screen.
    if let Some(pane) = explicit
        && inherited().as_deref() == Some(pane)
        && live_agent_is(pane, "codex")
    {
        return Some(pane.to_owned());
    }
    bridge_receipt(session, explicit)
}

/// Marks a launch whose caller pane is not known yet: Codex prints a command
/// only after it has finished, so the pane cannot be proven while the very
/// call that starts the task is still running.
const PENDING: &str = "session:";

pub(crate) fn pending_marker(session: &str) -> String {
    format!("{PENDING}{session}")
}

pub(crate) fn pending_session(caller: &str) -> Option<&str> {
    caller.strip_prefix(PENDING)
}

/// Waits for the call that started a task to finish and show up in its pane,
/// then returns that pane. `None` after `limit` means no Codex pane ever
/// showed it, and the caller must fail rather than guess.
pub(crate) fn wait_for_session(session: &str, limit: std::time::Duration) -> Option<String> {
    let started = std::time::Instant::now();
    loop {
        if let Some(pane) = traced(session) {
            return Some(pane);
        }
        if started.elapsed() >= limit {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
}

/// Codex shows `• Ran brgr --owner-session <id> ...` for a command it ran. A
/// narrow pane cuts the line, so the first characters of the id are enough.
const TRACE_PREFIX: usize = 13;

fn traced(session: &str) -> Option<String> {
    let prefix: String = session.chars().take(TRACE_PREFIX).collect();
    if prefix.chars().count() < TRACE_PREFIX {
        return None;
    }
    let agents = herdr_json(&["agent", "list"])?;
    let mut found = Vec::new();
    for item in agents.pointer("/result/agents")?.as_array()? {
        if item.get("agent").and_then(Value::as_str) != Some("codex") {
            continue;
        }
        let pane = item.get("pane_id").and_then(Value::as_str)?;
        let screen = herdr_text(&[
            "pane",
            "read",
            pane,
            "--source",
            "recent-unwrapped",
            "--lines",
            "1500",
        ])?;
        if shows_brgr_call(&screen, &prefix) {
            found.push(pane.to_owned());
        }
    }
    match found.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// A line where Codex says it ran (or is running) `brgr` with this session.
/// The id alone is not enough: any pane can print it.
fn shows_brgr_call(screen: &str, prefix: &str) -> bool {
    screen.lines().any(|line| {
        let line = line.trim_start().trim_start_matches('•').trim_start();
        (line.starts_with("Ran brgr ") || line.starts_with("Running brgr "))
            && line.contains(prefix)
    })
}

fn live_agent_is(pane: &str, kind: &str) -> bool {
    herdr_json(&["agent", "get", pane])
        .and_then(|live| {
            let agent = live.pointer("/result/agent")?.clone();
            Some(
                agent.get("agent").and_then(Value::as_str) == Some(kind)
                    && agent.get("pane_id").and_then(Value::as_str) == Some(pane),
            )
        })
        .unwrap_or(false)
}

fn bridge_receipt(session: &str, explicit: Option<&str>) -> Option<String> {
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

fn herdr_output(args: &[&str]) -> Option<Vec<u8>> {
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
    Some(bytes)
}

fn herdr_json(args: &[&str]) -> Option<Value> {
    serde_json::from_slice(&herdr_output(args)?).ok()
}

fn herdr_text(args: &[&str]) -> Option<String> {
    Some(String::from_utf8_lossy(&herdr_output(args)?).into_owned())
}

pub(crate) fn binary() -> Option<std::ffi::OsString> {
    #[cfg(test)]
    if let Some(path) = tests::HERDR.with(|slot| slot.borrow().clone()) {
        return Some(path.into_os_string());
    }
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

    const SESSION: &str = "01a0fb33-447c-7ba0-9f37-ba04f33ce809";

    #[test]
    fn a_ran_line_with_the_session_is_the_trace() {
        let ran = format!("• Ran brgr --as {SESSION} run \"Run the sh…");
        assert!(shows_brgr_call(&ran, "01a0fb33-447c"));
        // A pane too narrow for the whole id still shows its first characters.
        assert!(shows_brgr_call(
            "• Ran brgr --as 01a0fb33-447c…",
            "01a0fb33-447c"
        ));
        assert!(shows_brgr_call(
            &format!("• Running brgr --as {SESSION} status"),
            "01a0fb33-447c"
        ));
    }

    #[test]
    fn the_bare_id_or_another_command_is_not_a_trace() {
        // Any pane can print the id: a chat message, a pasted log, a Claude pane.
        assert!(!shows_brgr_call(
            &format!("the session is {SESSION}"),
            "01a0fb33-447c"
        ));
        assert!(!shows_brgr_call(
            &format!("• Ran echo {SESSION}"),
            "01a0fb33-447c"
        ));
        assert!(!shows_brgr_call(
            "• Ran brgr --as 11111111-2222 run",
            "01a0fb33-447c"
        ));
        // The user's own prompt quoting a command is not a command Codex ran.
        assert!(!shows_brgr_call(
            &format!("› please run brgr --as {SESSION} status"),
            "01a0fb33-447c"
        ));
    }

    /// A fake Herdr that lists Codex, Claude and shell panes and serves their
    /// screens from files, so `traced` runs end to end.
    fn fake_herdr(screens: &[(&str, &str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let mut agents = Vec::new();
        for (pane, kind, screen) in screens {
            fs::write(temp.path().join(pane.replace(':', "_")), screen).unwrap();
            agents.push(format!(r#"{{"pane_id":"{pane}","agent":"{kind}"}}"#));
        }
        fs::write(
            temp.path().join("list.json"),
            format!(r#"{{"result":{{"agents":[{}]}}}}"#, agents.join(",")),
        )
        .unwrap();
        let binary = temp.path().join("herdr");
        fs::write(
            &binary,
            "#!/bin/sh\nd=$(/usr/bin/dirname \"$0\")\ncase \"$1 $2\" in\n 'agent list') /bin/cat \"$d/list.json\";;\n 'pane read') f=$(printf %s \"$3\" | /usr/bin/sed 's/:/_/'); /bin/cat \"$d/$f\";;\n *) exit 2;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        (temp, binary)
    }

    fn trace_with(screens: &[(&str, &str, &str)]) -> Option<String> {
        let (_temp, binary) = fake_herdr(screens);
        with_herdr(&binary, || traced(SESSION))
    }

    thread_local! {
        pub(super) static HERDR: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    fn with_herdr<T>(binary: &Path, run: impl FnOnce() -> T) -> T {
        HERDR.with(|slot| *slot.borrow_mut() = Some(binary.to_path_buf()));
        let out = run();
        HERDR.with(|slot| *slot.borrow_mut() = None);
        out
    }

    #[test]
    fn exactly_one_codex_pane_showing_the_call_is_the_callers() {
        let ran = format!("• Ran brgr --as {SESSION} run \"x\"");
        let found = trace_with(&[
            ("w1:p1", "codex", "• Ran echo hello"),
            ("w1:p2", "codex", &ran),
            ("w1:p3", "codex", "idle"),
        ]);
        assert_eq!(found.as_deref(), Some("w1:p2"));
    }

    #[test]
    fn no_pane_or_two_panes_prove_nothing() {
        let ran = format!("• Ran brgr --as {SESSION} run \"x\"");
        assert_eq!(trace_with(&[("w1:p1", "codex", "nothing")]), None);
        assert_eq!(
            trace_with(&[("w1:p1", "codex", &ran), ("w1:p2", "codex", &ran)]),
            None
        );
    }

    /// D14: Herdr's record said this session ran in the wrong pane, because the
    /// Codex hook reported the shared daemon's pane. The record is ignored.
    #[test]
    fn a_poisoned_herdr_session_record_does_not_move_the_session() {
        let ran = format!("• Ran brgr --as {SESSION} run \"x\"");
        let (temp, binary) = fake_herdr(&[
            ("w6G:p1", "codex", "other work"),
            ("w5G:p30", "codex", &ran),
        ]);
        fs::write(
            temp.path().join("list.json"),
            format!(
                r#"{{"result":{{"agents":[{{"pane_id":"w6G:p1","agent":"codex","agent_session":{{"value":"{SESSION}"}}}},{{"pane_id":"w5G:p30","agent":"codex"}}]}}}}"#
            ),
        )
        .unwrap();
        let found = with_herdr(&binary, || for_session(SESSION, None));
        assert_eq!(found.as_deref(), Some("w5G:p30"));
        // A caller that names the poisoned pane is refused, not believed.
        let named = with_herdr(&binary, || for_session(SESSION, Some("w6G:p1")));
        assert_eq!(named, None);
    }

    #[test]
    fn a_pane_that_is_not_codex_never_counts() {
        // This session's own conversation printed the id in a Claude pane.
        let ran = format!("• Ran brgr --as {SESSION} run \"x\"");
        let found = trace_with(&[("w1:p1", "claude", &ran), ("w1:p2", "codex", &ran)]);
        assert_eq!(found.as_deref(), Some("w1:p2"));
    }

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
