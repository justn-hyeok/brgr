use std::{
    fs::{self, File},
    io::{Read as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum WorkerPlacement {
    #[default]
    Adjacent,
    Tab,
}

impl WorkerPlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Adjacent => "adjacent",
            Self::Tab => "tab",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub herdr: HerdrConfig,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrConfig {
    pub worker_placement: WorkerPlacement,
    pub auto_worker_pane: bool,
    pub codex_executable: Option<PathBuf>,
}

pub fn validate_codex_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Codex executable must be an absolute path");
    }
    let metadata = fs::metadata(path).context("configured Codex executable is unavailable")?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("configured Codex executable is not an executable file");
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
            bail!("brgr config must be a regular file of at most {MAX_CONFIG_BYTES} bytes");
        }
        let mut source = String::new();
        File::open(path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut source)?;
        if source.len() as u64 > MAX_CONFIG_BYTES {
            bail!("brgr config exceeds {MAX_CONFIG_BYTES} bytes");
        }
        toml::from_str(&source).context("brgr config is invalid")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("brgr config has no parent directory")?;
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .as_file_mut()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_defaults_to_adjacent_and_invalid_file_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        assert_eq!(
            Config::load(&path).unwrap().herdr.worker_placement,
            WorkerPlacement::Adjacent
        );
        assert!(!Config::load(&path).unwrap().herdr.auto_worker_pane);
        assert!(
            Config::load(&path)
                .unwrap()
                .herdr
                .codex_executable
                .is_none()
        );
        fs::write(&path, "[herdr]\nworker_placement = 'elsewhere'\n").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "[herdr]\nworker_placement = 'tab'\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().herdr.worker_placement,
            WorkerPlacement::Tab
        );
        fs::write(
            &path,
            "[herdr]\nworker_placement = 'tab'\nauto_worker_pane = true\n",
        )
        .unwrap();
        assert!(Config::load(&path).unwrap().herdr.auto_worker_pane);
        let linked = root.path().join("linked.toml");
        std::os::unix::fs::symlink(&path, &linked).unwrap();
        assert!(Config::load(&linked).is_err());
    }
}
