//! Execute a registered native TUI with its terminal streams inherited.
use crate::write_json_atomic;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

mod opencode;

#[derive(Serialize, Deserialize)]
pub(crate) struct NativeLaunch {
    pub(crate) executable: PathBuf,
    pub(crate) digest: String,
    pub(crate) argv: Vec<String>,
    pub(crate) workspace: PathBuf,
    pub(crate) environment: BTreeMap<String, String>,
    /// The harness's allowlisted variables. The pane's own shell supplies
    /// these, so a login made after the caller started is the one used.
    #[serde(default)]
    pub(crate) pane_environment: Vec<String>,
    pub(crate) state: PathBuf,
    /// `OpenCode` 2 TUI resumes a model-selected session instead of taking a
    /// `--model` flag. Absent in old launch files and for all other harnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) opencode_model: Option<String>,
}

/// What a terminal program reads to decide whether it may use colour. They
/// describe the pane the worker runs in, not the process that started the task:
/// that process may be a tool shell with `TERM=dumb`, or have no terminal at all.
const TERMINAL_ENVIRONMENT: [&str; 3] = ["TERM", "COLORTERM", "NO_COLOR"];

/// The variables the pane's own shell supplies: the harness's allowlist plus the
/// terminal's. Without `TERM` and `COLORTERM` a worker's TUI falls back to a few
/// colours, or none.
pub(crate) fn pane_variables(allowlist: &[String]) -> Vec<String> {
    let mut names = allowlist.to_vec();
    for name in TERMINAL_ENVIRONMENT {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Takes each named variable from the pane's shell, and drops the caller's copy
/// when the pane does not set it. A caller started before the user replaced a
/// key would otherwise hand the worker the stale one.
fn take_from_pane(
    environment: &mut BTreeMap<String, String>,
    names: &[String],
    pane: impl Fn(&str) -> Option<String>,
) {
    for name in names {
        match pane(name) {
            Some(value) => {
                environment.insert(name.clone(), value);
            }
            None => {
                environment.remove(name);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct NativeState {
    pub(crate) pid: u32,
    pub(crate) birth: String,
    pub(crate) phase: String,
    pub(crate) exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) startup_error: Option<String>,
}

pub(crate) fn run(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 65_536 {
        bail!("native launch must be a bounded regular file");
    }
    let mut launch: NativeLaunch = serde_json::from_slice(&fs::read(path)?)?;
    // The host is in the new terminal. Do not give the native worker the
    // controller's inherited parent pane variables from its launch envelope.
    for key in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_BIN_PATH",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            launch.environment.insert(key.to_owned(), value);
        }
    }
    let names = launch.pane_environment.clone();
    take_from_pane(&mut launch.environment, &names, |name| {
        std::env::var(name).ok()
    });
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(launch.state.with_extension("lock"))?;
    lock.try_lock()
        .context("this native TUI already has a host")?;
    let bytes = fs::read(&launch.executable)?;
    let mut digest = String::new();
    for byte in Sha256::digest(bytes) {
        write!(&mut digest, "{byte:02x}")?;
    }
    if launch.digest.trim_start_matches("sha256:") != digest {
        bail!("registered native executable changed before TUI launch");
    }
    if let Some(model) = launch.opencode_model.clone()
        && let Err(error) = opencode::prepare(&mut launch, &model)
    {
        write_json_atomic(
            &launch.state,
            &NativeState {
                pid: std::process::id(),
                birth: String::new(),
                phase: "failed".into(),
                exit_code: Some(1),
                startup_error: Some(format!("{error:#}")),
            },
        )?;
        return Err(error);
    }
    let mut child = Command::new(&launch.executable)
        .args(&launch.argv)
        .current_dir(&launch.workspace)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not start registered native TUI")?;
    let pid = child.id();
    let birth = crate::supervision::ps_field(&pid.to_string(), "lstart")?;
    write_json_atomic(
        &launch.state,
        &NativeState {
            pid,
            birth: birth.clone(),
            phase: "running".into(),
            exit_code: None,
            startup_error: None,
        },
    )?;
    let status = child.wait()?;
    write_json_atomic(
        &launch.state,
        &NativeState {
            pid,
            birth,
            phase: "exited".into(),
            exit_code: status.code(),
            startup_error: None,
        },
    )?;
    Ok(())
}

pub(crate) fn alive(path: &Path) -> bool {
    let Some(state) = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<NativeState>(&b).ok())
    else {
        return false;
    };
    state.phase == "running"
        && crate::supervision::ps_field(&state.pid.to_string(), "lstart")
            .is_ok_and(|birth| birth == state.birth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pane_shell_supplies_harness_variables_over_the_callers() {
        let mut environment = BTreeMap::from([
            ("COMMAND_CODE_API_KEY".to_owned(), "stale".to_owned()),
            ("OLD_TOKEN".to_owned(), "stale".to_owned()),
            ("BRGR_HOME".to_owned(), "/brgr".to_owned()),
        ]);
        let names = vec![
            "COMMAND_CODE_API_KEY".to_owned(),
            "OLD_TOKEN".to_owned(),
            "PATH".to_owned(),
        ];
        take_from_pane(&mut environment, &names, |name| match name {
            "COMMAND_CODE_API_KEY" => Some("fresh".to_owned()),
            "PATH" => Some("/bin".to_owned()),
            _ => None,
        });
        assert_eq!(environment["COMMAND_CODE_API_KEY"], "fresh");
        assert_eq!(environment["PATH"], "/bin");
        assert!(!environment.contains_key("OLD_TOKEN"));
        assert_eq!(environment["BRGR_HOME"], "/brgr");
    }

    #[test]
    fn a_workers_terminal_comes_from_its_pane_not_from_the_process_that_started_it() {
        let allowlist = ["HOME".to_owned(), "PATH".to_owned()];
        let names = pane_variables(&allowlist);
        assert_eq!(names[..2], allowlist);
        for terminal in ["TERM", "COLORTERM", "NO_COLOR"] {
            assert!(names.iter().any(|name| name == terminal), "{terminal}");
        }
        assert_eq!(
            pane_variables(&names),
            names,
            "adding twice changes nothing"
        );

        // The starting process was a tool shell: TERM=dumb and a NO_COLOR it
        // set for its own output. The pane has a real terminal.
        let mut environment = BTreeMap::from([
            ("TERM".to_owned(), "dumb".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
            ("HOME".to_owned(), "/home/u".to_owned()),
        ]);
        take_from_pane(&mut environment, &names, |name| match name {
            "TERM" => Some("xterm-256color".to_owned()),
            "COLORTERM" => Some("truecolor".to_owned()),
            "HOME" => Some("/home/u".to_owned()),
            _ => None,
        });
        assert_eq!(environment["TERM"], "xterm-256color");
        assert_eq!(environment["COLORTERM"], "truecolor");
        assert!(
            !environment.contains_key("NO_COLOR"),
            "the caller's NO_COLOR is not the pane's"
        );
    }

    #[test]
    fn a_launch_written_before_pane_environment_still_reads() {
        let launch: NativeLaunch = serde_json::from_str(
            r#"{"executable":"/bin/echo","digest":"x","argv":[],"workspace":"/","environment":{},"state":"/s"}"#,
        )
        .unwrap();
        assert!(launch.pane_environment.is_empty());
    }
}
