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
    fn a_launch_written_before_pane_environment_still_reads() {
        let launch: NativeLaunch = serde_json::from_str(
            r#"{"executable":"/bin/echo","digest":"x","argv":[],"workspace":"/","environment":{},"state":"/s"}"#,
        )
        .unwrap();
        assert!(launch.pane_environment.is_empty());
    }
}
