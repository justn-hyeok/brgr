//! Evidence-backed harness registration and health checking.

use std::{
    collections::BTreeMap,
    fmt::Write as FmtWrite,
    fs,
    io::Write as IoWrite,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use brgr_runner::{
    Capability, CapabilityStatus, ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1,
    PROCESS_ADAPTER_V1, ProbeSpec, ProcessRunner, ResultSource, ResultSpec, RunnerError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Registry {
    root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivationReceipt {
    pub schema: String,
    pub harness_id: String,
    pub executable_realpath: PathBuf,
    pub executable_digest: String,
    pub version_digest: String,
    pub help_digest: String,
    pub manifest_digest: String,
    pub tested_os: String,
    pub tested_arch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Health {
    Healthy,
    Drifted { expected: String, observed: String },
}

impl Registry {
    /// Opens or creates a private harness registry.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when registry directories cannot be created or
    /// secured.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let root = root.into();
        fs::create_dir_all(root.join("manifests"))?;
        fs::create_dir_all(root.join("activations"))?;
        for path in [&root, &root.join("manifests"), &root.join("activations")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }

    /// Probes and activates a supported installed harness.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when probing fails, required documented flags
    /// are absent, or activation data cannot be persisted.
    pub async fn add(&self, executable: &Path) -> Result<ActivationReceipt, RegistryError> {
        if !executable.is_absolute() {
            return Err(RegistryError::ExecutableMustBeAbsolute);
        }
        let requested_name = executable
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(RegistryError::UnsupportedHarness)?
            .to_owned();
        let realpath = executable.canonicalize()?;
        let version =
            ProcessRunner::probe(&realpath, &["--version".to_owned()], PROBE_DEADLINE).await?;
        let help = ProcessRunner::probe(&realpath, &["--help".to_owned()], PROBE_DEADLINE).await?;
        if version.exit_code != Some(0) || help.exit_code != Some(0) {
            return Err(RegistryError::ProbeFailed);
        }
        let help_text = String::from_utf8_lossy(&help.stdout);
        let manifest = generate_manifest(&requested_name, realpath.clone(), &help_text)?;
        manifest.validate()?;
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let receipt = ActivationReceipt {
            schema: "brgr.activation/v1".to_owned(),
            harness_id: manifest.id.clone(),
            executable_realpath: realpath.clone(),
            executable_digest: digest_file(&realpath)?,
            version_digest: digest_bytes(&version.stdout),
            help_digest: digest_bytes(&help.stdout),
            manifest_digest: digest_bytes(&manifest_bytes),
            tested_os: std::env::consts::OS.to_owned(),
            tested_arch: std::env::consts::ARCH.to_owned(),
        };

        write_json_atomic(&self.manifest_path(&manifest.id), &manifest_bytes)?;
        write_json_atomic(
            &self.activation_path(&manifest.id),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        Ok(receipt)
    }

    /// Loads an activated manifest after checking executable identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the package is missing, malformed, or has
    /// drifted since activation.
    pub fn load_healthy(&self, harness_id: &str) -> Result<HarnessManifest, RegistryError> {
        let manifest: HarnessManifest =
            serde_json::from_slice(&fs::read(self.manifest_path(harness_id))?)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        match health_for(&receipt)? {
            Health::Healthy => {
                manifest.validate()?;
                Ok(manifest)
            }
            Health::Drifted { expected, observed } => {
                Err(RegistryError::ExecutableDrift { expected, observed })
            }
        }
    }

    /// Checks the activated executable digest without performing a paid model
    /// request.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when activation data or the executable cannot
    /// be read.
    pub fn health(&self, harness_id: &str) -> Result<Health, RegistryError> {
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        health_for(&receipt)
    }

    fn manifest_path(&self, harness_id: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{harness_id}.json"))
    }

    fn activation_path(&self, harness_id: &str) -> PathBuf {
        self.root
            .join("activations")
            .join(format!("{harness_id}.json"))
    }
}

fn generate_manifest(
    requested_name: &str,
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    match requested_name {
        "gjc"
            if ["--mode=<value>", "--no-session", "--no-mcp", "-p, --print"]
                .iter()
                .all(|flag| help.contains(flag)) =>
        {
            let capabilities = BTreeMap::from([
                (
                    "completion".to_owned(),
                    supported("process_exit_with_json_capture"),
                ),
                ("cancel".to_owned(), supported("local_process_only")),
                ("model_select".to_owned(), supported("--model")),
                ("effort_select".to_owned(), supported("--thinking")),
            ]);
            Ok(HarnessManifest {
                schema: MANIFEST_SCHEMA_V1.to_owned(),
                id: "local.gjc".to_owned(),
                adapter: PROCESS_ADAPTER_V1.to_owned(),
                executable,
                probe: ProbeSpec {
                    version_argv: vec!["--version".to_owned()],
                    help_argv: vec!["--help".to_owned()],
                },
                launch: LaunchSpec {
                    argv: vec![
                        "-p".to_owned(),
                        "--mode=json".to_owned(),
                        "--no-session".to_owned(),
                        "--no-mcp".to_owned(),
                        "@${input.prompt_file}".to_owned(),
                    ],
                    env_allow: vec![
                        "HOME".to_owned(),
                        "PATH".to_owned(),
                        "LANG".to_owned(),
                        "TMPDIR".to_owned(),
                    ],
                    mode: ExecutionMode::OneShot,
                },
                result: ResultSpec {
                    source: ResultSource::Stdout,
                    media_type: "application/x-ndjson".to_owned(),
                    max_bytes: 1_048_576,
                    success_exit_codes: vec![0],
                },
                capabilities,
            })
        }
        "gjc" => Err(RegistryError::RequiredFlagsMissing),
        _ => Err(RegistryError::UnsupportedHarness),
    }
}

fn supported(semantics: &str) -> Capability {
    Capability {
        status: CapabilityStatus::Supported,
        semantics: semantics.to_owned(),
        evidence_ref: Some("activation-help-digest".to_owned()),
        tested_identity: None,
    }
}

fn health_for(receipt: &ActivationReceipt) -> Result<Health, RegistryError> {
    let observed = digest_file(&receipt.executable_realpath)?;
    if observed == receipt.executable_digest {
        Ok(Health::Healthy)
    } else {
        Ok(Health::Drifted {
            expected: receipt.executable_digest.clone(),
            observed,
        })
    }
}

fn digest_file(path: &Path) -> Result<String, RegistryError> {
    Ok(digest_bytes(&fs::read(path)?))
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn write_json_atomic(path: &Path, bytes: &[u8]) -> Result<(), RegistryError> {
    let parent = path.parent().ok_or(RegistryError::MissingParent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(bytes)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("executable path must be absolute")]
    ExecutableMustBeAbsolute,
    #[error("the installed harness is not supported by a verified recipe")]
    UnsupportedHarness,
    #[error("the installed GJC help does not contain the required managed-run flags")]
    RequiredFlagsMissing,
    #[error("version or help probe failed")]
    ProbeFailed,
    #[error("activation executable drifted: expected {expected}, observed {observed}")]
    ExecutableDrift { expected: String, observed: String },
    #[error("registry path has no parent")]
    MissingParent,
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_directories_are_private() {
        let root = tempfile::tempdir().unwrap();
        let registry_path = root.path().join("registry");
        Registry::open(&registry_path).unwrap();
        assert_eq!(
            fs::metadata(registry_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn unknown_harness_is_not_guessed() {
        let error = generate_manifest("mystery", PathBuf::from("/bin/echo"), "--help").unwrap_err();
        assert!(matches!(error, RegistryError::UnsupportedHarness));
    }
}
