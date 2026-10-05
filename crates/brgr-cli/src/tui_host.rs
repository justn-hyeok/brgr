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
    pub(crate) state: PathBuf,
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
