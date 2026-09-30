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

use std::{collections::HashMap, env, path::Path, process::Command};

/// The caller's Herdr pane, when this process provably runs inside it.
pub(crate) fn verified() -> Option<String> {
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
