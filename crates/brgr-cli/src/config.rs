use std::{
    collections::BTreeMap,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub herdr: HerdrConfig,
    pub worker: WorkerConfig,
    // The tables below did not exist in earlier releases, which refuse unknown
    // keys and share this file. They are written only once they hold something.
    #[serde(skip_serializing_if = "IssuesConfig::is_default")]
    pub issues: IssuesConfig,
    #[serde(skip_serializing_if = "CallingOptions::is_unset")]
    pub defaults: CallingOptions,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub harnesses: BTreeMap<String, CallingOptions>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallingOptions {
    pub(crate) harness: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) permission: Option<brgr_protocol::PermissionLevel>,
    pub(crate) deadline_seconds: Option<u64>,
    pub(crate) argv: Vec<String>,
    pub(crate) notifications: Option<bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    /// The most a worker may be given. A task asking for more is refused, and a
    /// task asking for nothing runs at this level instead of the harness's
    /// full default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_permission: Option<brgr_protocol::PermissionLevel>,
}

/// Whether brgr files GitHub issues for the failures it records. Off by default:
/// issue text leaves the machine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssuesConfig {
    pub auto_file: bool,
    /// `OWNER/NAME` of the repository that receives the issues.
    pub repo: Option<String>,
    /// How many times a failure must be seen before its issue is filed.
    pub min_count: u64,
}

impl IssuesConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for IssuesConfig {
    fn default() -> Self {
        Self {
            auto_file: false,
            repo: None,
            min_count: 2,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrConfig {
    pub worker_placement: WorkerPlacement,
    pub auto_worker_pane: bool,
    pub codex_executable: Option<PathBuf>,
    /// Keep harnesses in print mode even inside Herdr, instead of running them
    /// as their own TUI in a pane.
    pub prefer_print_mode: bool,
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

impl CallingOptions {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

impl Config {
    pub(crate) fn calling(&self, harness: &str) -> CallingOptions {
        let global = &self.defaults;
        let local = self.harnesses.get(harness).cloned().unwrap_or_default();
        CallingOptions {
            harness: Some(harness.to_owned()),
            model: local.model.or_else(|| global.model.clone()),
            effort: local.effort.or_else(|| global.effort.clone()),
            permission: local.permission.or(global.permission),
            deadline_seconds: local.deadline_seconds.or(global.deadline_seconds),
            argv: global.argv.iter().chain(&local.argv).cloned().collect(),
            notifications: local.notifications.or(global.notifications),
        }
    }

    pub(crate) fn set(&mut self, key: &str, value: &str, harness: Option<&str>) -> Result<()> {
        let options = if let Some(id) = harness {
            validate_id(id)?;
            self.harnesses.entry(id.to_owned()).or_default()
        } else {
            &mut self.defaults
        };
        match key {
            "harness" if harness.is_none() => {
                validate_id(value)?;
                options.harness = Some(value.to_owned());
            }
            "model" => options.model = Some(nonempty(value)?.to_owned()),
            "effort" => options.effort = Some(nonempty(value)?.to_owned()),
            "notifications" => {
                options.notifications = Some(
                    value
                        .parse()
                        .context("notifications must be true or false")?,
                );
            }
            // Workers always run with full permissions; a stored level is
            // accepted for old scripts and never applied.
            "permission" => {
                eprintln!("brgr: permission is ignored; workers always run with full permissions");
            }
            "deadline-seconds" => {
                let seconds: u64 = value.parse()?;
                if seconds == 0 || seconds > 604_800 {
                    bail!("deadline must be 1..604800 seconds");
                }
                options.deadline_seconds = Some(seconds);
            }
            "argv" => {
                options.argv =
                    serde_json::from_str(value).context("argv must be a JSON string array")?;
                validate_argv(&options.argv)?;
            }
            "worker-placement" if harness.is_none() => {
                self.herdr.worker_placement = match value {
                    "adjacent" => WorkerPlacement::Adjacent,
                    "tab" => WorkerPlacement::Tab,
                    _ => bail!("worker-placement must be adjacent or tab"),
                }
            }
            _ => bail!("unknown calling option {key}"),
        }
        Ok(())
    }

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

fn nonempty(value: &str) -> Result<&str> {
    if value.trim().is_empty() {
        bail!("setting must not be empty");
    }
    Ok(value)
}

fn validate_id(id: &str) -> Result<()> {
    if !id.contains('.')
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("invalid harness id {id}");
    }
    Ok(())
}

pub(crate) fn validate_argv(argv: &[String]) -> Result<()> {
    if argv.len() > 128
        || argv
            .iter()
            .any(|arg| arg.contains('\0') || arg.len() > 4096)
    {
        bail!("calling argv exceeds the supported size");
    }
    for arg in argv {
        let flag = arg.split('=').next().unwrap_or(arg);
        if [
            "--model",
            "-m",
            "--effort",
            "--thinking",
            "--permission-mode",
            "--approval-mode",
            "--yolo",
            "--force",
            "-p",
            "--print",
            "--session-id",
            "--resume",
            "--continue",
        ]
        .contains(&flag)
        {
            bail!("{flag} conflicts with brgr calling options; configure its named setting");
        }
    }
    Ok(())
}

pub(crate) fn validate_manifest_argv(
    argv: &[String],
    manifest: &brgr_runner::HarnessManifest,
) -> Result<()> {
    validate_argv(argv)?;
    let selectors = manifest
        .launch
        .model_argv
        .iter()
        .chain(&manifest.launch.effort_argv)
        .chain(manifest.launch.permission_argv.full.iter().flatten())
        .chain(manifest.launch.permission_argv.edits.iter().flatten())
        .chain(manifest.launch.permission_argv.read_only.iter().flatten());
    for selector in selectors.filter(|argument| argument.starts_with('-')) {
        let flag = selector.split('=').next().unwrap_or(selector);
        if argv
            .iter()
            .any(|argument| argument.split('=').next() == Some(flag))
        {
            bail!("{flag} is a native selector owned by brgr's named calling settings");
        }
    }
    Ok(())
}

pub(crate) fn initialize(paths: &crate::Paths, json_output: bool) -> Result<()> {
    let instructions = paths.home.join("BRGR.md");
    let config_created = !paths.config.exists();
    if config_created {
        Config::default().save(&paths.config)?;
    }
    let mut instructions_created = false;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&instructions)
    {
        Ok(mut file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all("# brgr 사용자 지침\n\n<!-- 이 파일에 오케스트레이션에 전달할 지침을 작성하세요. -->\n".as_bytes())?;
            file.sync_all()?;
            instructions_created = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    crate::print_value(
        &serde_json::json!({"config":paths.config,"instructions":instructions,"config_created":config_created,"instructions_created":instructions_created}),
        json_output,
    );
    Ok(())
}

pub(crate) async fn check(paths: &crate::Paths, config: &Config, json_output: bool) -> Result<()> {
    let registry = brgr_registry::Registry::open_with_control_home(&paths.registry, &paths.home)?;
    validate_argv(&config.defaults.argv)?;
    let mut checked = Vec::new();
    let default = config
        .defaults
        .harness
        .as_deref()
        .unwrap_or("local.gjc")
        .to_owned();
    for id in config
        .harnesses
        .keys()
        .chain(std::iter::once(&default))
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (manifest, _) = crate::admission::load_harness(&registry, id).await?;
        let options = config.calling(id);
        validate_manifest_argv(&options.argv, &manifest)?;
        manifest.permission_arguments(options.permission)?;
        if options.model.is_some() && manifest.launch.model_argv.is_empty() {
            bail!("{id} has no model option");
        }
        if options.effort.is_some() && manifest.launch.effort_argv.is_empty() {
            bail!("{id} has no effort option");
        }
        let interactive = manifest
            .launch
            .interactive
            .as_ref()
            .context("configured harness has no native TUI recipe")?;
        if interactive.effort_print_only && options.effort.is_some() {
            bail!("{id} cannot apply effort in its TUI");
        }
        if interactive.model_print_only && options.model.is_some() {
            bail!("{id} cannot apply a model in its TUI");
        }
        if options
            .deadline_seconds
            .is_some_and(|seconds| seconds == 0 || seconds > 604_800)
        {
            bail!("invalid deadline setting for {id}");
        }
        registry
            .preflight_model(&manifest, options.model.as_deref())
            .await?;
        checked.push(id);
    }
    crate::print_value(
        &serde_json::json!({"status":"ok","harnesses":checked,"default_presentation":"tui","default_permission":"full","instructions":paths.home.join("BRGR.md")}),
        json_output,
    );
    Ok(())
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

#[cfg(test)]
mod serialize_tests {
    use super::*;

    /// Earlier releases refuse unknown tables and share this file, so saving an
    /// unrelated setting must not add the tables they do not know.
    #[test]
    fn a_default_config_writes_no_table_an_older_release_would_refuse() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        for table in ["[issues]", "[defaults]", "[harnesses"] {
            assert!(!text.contains(table), "{table} written:\n{text}");
        }
        let mut config = Config::default();
        config.issues.auto_file = true;
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[issues]"), "{text}");
    }
}
