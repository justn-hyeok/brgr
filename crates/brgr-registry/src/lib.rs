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
    OMP_ROLE_ADAPTER_V1, PROCESS_ADAPTER_V1, ProbeSpec, ProcessRunner, ResultSource, ResultSpec,
    RunnerError,
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
    #[serde(default)]
    pub scratch_result_digest: Option<String>,
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
        let (manifest, probe) = draft_manifest(executable).await?;
        if !matches!(manifest.id.as_str(), "local.gjc" | "local.omp")
            || (manifest.id == "local.omp" && manifest.adapter != OMP_ROLE_ADAPTER_V1)
        {
            return Err(RegistryError::ScratchRunRequired(manifest.id));
        }
        self.persist_activation(&manifest, &probe, None)
    }

    /// Builds a non-active process recipe from observed `--help` and `--version`.
    /// Only the exact documented `--prompt-file <path>` form is inferred for an
    /// otherwise unknown CLI; other features remain unsupported.
    ///
    /// # Errors
    ///
    /// Returns an error if probing fails or a bounded fresh run cannot be
    /// described without guessing flags.
    pub async fn draft(&self, executable: &Path) -> Result<HarnessManifest, RegistryError> {
        draft_manifest(executable)
            .await
            .map(|(manifest, _)| manifest)
    }

    /// Performs the deterministic process/v1 contract check without invoking
    /// the target model. Activation still requires a caller-authorized scratch
    /// run through [`Self::activate_with_scratch`].
    ///
    /// # Errors
    ///
    /// Rejects changed, unsupported, or unobserved recipes.
    pub async fn contract_test(&self, manifest: &HarnessManifest) -> Result<(), RegistryError> {
        let requested_name = name_from_id(&manifest.id)?;
        let (observed, _) = draft_manifest_as(&manifest.executable, requested_name).await?;
        if &observed != manifest || manifest.adapter != PROCESS_ADAPTER_V1 {
            return Err(RegistryError::ManifestNotObserved);
        }
        manifest.validate()?;
        Ok(())
    }

    /// Runs an explicitly authorized scratch task, then activates the exact
    /// observed recipe only when it produces a nonempty successful result.
    /// This method may invoke a paid model and must not be called by discovery.
    ///
    /// # Errors
    ///
    /// Rejects recipe drift, failed/empty results, and persistence failures.
    pub async fn activate_with_scratch(
        &self,
        manifest: &HarnessManifest,
        workspace: &Path,
        prompt: &str,
    ) -> Result<ActivationReceipt, RegistryError> {
        self.contract_test(manifest).await?;
        if prompt.trim().is_empty() {
            return Err(RegistryError::EmptyScratchPrompt);
        }
        let before_digest = digest_file(&manifest.executable)?;
        let output = ProcessRunner::run(
            manifest,
            brgr_runner::RunRequest {
                workspace,
                prompt,
                model: None,
                effort: None,
                deadline: Duration::from_mins(1),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await?;
        if !output.succeeded(manifest) || output.result.is_empty() {
            return Err(RegistryError::ScratchRunFailed);
        }
        let requested_name = name_from_id(&manifest.id)?;
        let (after_manifest, probe) =
            draft_manifest_as(&manifest.executable, requested_name).await?;
        if after_manifest != *manifest || digest_file(&manifest.executable)? != before_digest {
            return Err(RegistryError::ManifestNotObserved);
        }
        self.persist_activation(manifest, &probe, Some(digest_bytes(&output.result)))
    }

    /// Loads an activated manifest after checking executable identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the package is missing, malformed, or has
    /// drifted since activation.
    pub fn load_healthy(&self, harness_id: &str) -> Result<HarnessManifest, RegistryError> {
        validate_harness_id(harness_id)?;
        let manifest_bytes = fs::read(self.manifest_path(harness_id))?;
        let manifest: HarnessManifest = serde_json::from_slice(&manifest_bytes)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        validate_package(harness_id, &manifest, &receipt, &manifest_bytes)?;
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
        validate_harness_id(harness_id)?;
        let manifest_bytes = fs::read(self.manifest_path(harness_id))?;
        let manifest: HarnessManifest = serde_json::from_slice(&manifest_bytes)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        validate_package(harness_id, &manifest, &receipt, &manifest_bytes)?;
        health_for(&receipt)
    }

    /// Repeats bounded version/help probes to detect dependency or wrapper
    /// drift that an unchanged executable digest cannot reveal. No model task
    /// is started.
    ///
    /// # Errors
    ///
    /// Returns an error when package identity or probing fails.
    pub async fn health_probed(&self, harness_id: &str) -> Result<Health, RegistryError> {
        let manifest = self.load_healthy(harness_id)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        for (argv, expected) in [
            (&manifest.probe.version_argv, &receipt.version_digest),
            (&manifest.probe.help_argv, &receipt.help_digest),
        ] {
            let observed = ProcessRunner::probe(&manifest.executable, argv, PROBE_DEADLINE).await?;
            if observed.exit_code != Some(0) || observed.timed_out || observed.output_truncated {
                return Err(RegistryError::ProbeFailed);
            }
            let observed_digest = digest_bytes(&observed.stdout);
            if &observed_digest != expected {
                return Ok(Health::Drifted {
                    expected: expected.clone(),
                    observed: observed_digest,
                });
            }
        }
        Ok(Health::Healthy)
    }

    fn persist_activation(
        &self,
        manifest: &HarnessManifest,
        probe: &ProbeEvidence,
        scratch_result_digest: Option<String>,
    ) -> Result<ActivationReceipt, RegistryError> {
        validate_harness_id(&manifest.id)?;
        manifest.validate()?;
        if digest_file(&manifest.executable)? != probe.executable {
            return Err(RegistryError::ActivationMismatch);
        }
        let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
        let receipt = ActivationReceipt {
            schema: "brgr.activation/v1".to_owned(),
            harness_id: manifest.id.clone(),
            executable_realpath: manifest.executable.clone(),
            executable_digest: probe.executable.clone(),
            version_digest: probe.version.clone(),
            help_digest: probe.help.clone(),
            manifest_digest: digest_bytes(&manifest_bytes),
            tested_os: std::env::consts::OS.to_owned(),
            tested_arch: std::env::consts::ARCH.to_owned(),
            scratch_result_digest,
        };
        write_json_atomic(&self.manifest_path(&manifest.id), &manifest_bytes)?;
        write_json_atomic(
            &self.activation_path(&manifest.id),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        Ok(receipt)
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

struct ProbeEvidence {
    executable: String,
    version: String,
    help: String,
}

async fn draft_manifest(
    executable: &Path,
) -> Result<(HarnessManifest, ProbeEvidence), RegistryError> {
    if !executable.is_absolute() {
        return Err(RegistryError::ExecutableMustBeAbsolute);
    }
    let requested_name = executable
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(RegistryError::UnsupportedHarness)?
        .to_owned();
    draft_manifest_as(executable, &requested_name).await
}

async fn draft_manifest_as(
    executable: &Path,
    requested_name: &str,
) -> Result<(HarnessManifest, ProbeEvidence), RegistryError> {
    let realpath = executable.canonicalize()?;
    if !realpath.is_file() {
        return Err(RegistryError::UnsupportedHarness);
    }
    let version_argv = if requested_name == "omp-role" {
        vec!["--help".to_owned()]
    } else {
        vec!["--version".to_owned()]
    };
    let version = ProcessRunner::probe(&realpath, &version_argv, PROBE_DEADLINE).await?;
    let help = ProcessRunner::probe(&realpath, &["--help".to_owned()], PROBE_DEADLINE).await?;
    if version.exit_code != Some(0)
        || help.exit_code != Some(0)
        || version.timed_out
        || help.timed_out
        || version.output_truncated
        || help.output_truncated
    {
        return Err(RegistryError::ProbeFailed);
    }
    let help_text = String::from_utf8_lossy(&help.stdout);
    let manifest = generate_manifest(requested_name, realpath, &help_text)?;
    manifest.validate()?;
    let executable_digest = digest_file(&manifest.executable)?;
    Ok((
        manifest,
        ProbeEvidence {
            executable: executable_digest,
            version: digest_bytes(&version.stdout),
            help: digest_bytes(&help.stdout),
        },
    ))
}

fn generate_manifest(
    requested_name: &str,
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    match requested_name {
        "gjc" => generate_gjc_manifest(executable, help),
        "omp-role" => generate_omp_manifest(executable, help),
        _ => generate_generic_manifest(requested_name, executable, help),
    }
}

fn generate_generic_manifest(
    requested_name: &str,
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    // The only v1 generic profile is an explicitly documented fresh run that
    // accepts a prompt file and writes its final result to stdout. Other
    // shapes stay draft-blocked rather than receiving guessed arguments.
    if !help.lines().any(|line| {
        let words: Vec<_> = line.split_whitespace().collect();
        words.windows(2).any(|pair| {
            pair[0] == "--prompt-file" && matches!(pair[1], "<path>" | "<file>" | "PATH" | "FILE")
        })
    }) {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    let name = requested_name.to_ascii_lowercase();
    let id = format!("local.{name}");
    validate_harness_id(&id)?;
    if matches!(id.as_str(), "local.gjc" | "local.omp") {
        return Err(RegistryError::UnsupportedHarness);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id,
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
        },
        launch: LaunchSpec {
            argv: vec![
                "--prompt-file".to_owned(),
                "${input.prompt_file}".to_owned(),
            ],
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: vec!["HOME".to_owned(), "PATH".to_owned(), "LANG".to_owned()],
            mode: ExecutionMode::OneShot,
        },
        result: ResultSpec {
            source: ResultSource::Stdout,
            media_type: "text/plain".to_owned(),
            max_bytes: 1_048_576,
            success_exit_codes: vec![0],
        },
        capabilities: BTreeMap::from([
            (
                "completion".to_owned(),
                supported("process_exit_with_nonempty_stdout"),
            ),
            ("cancel".to_owned(), supported("local_process_only")),
            ("model_select".to_owned(), unsupported("not_observed")),
            ("effort_select".to_owned(), unsupported("not_observed")),
        ]),
    })
}

fn generate_gjc_manifest(
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if !["--mode=<value>", "--no-session", "--no-mcp", "-p, --print"]
        .iter()
        .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
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
            model_argv: vec!["--model".to_owned(), "${route.model}".to_owned()],
            effort_argv: vec!["--thinking".to_owned(), "${route.effort}".to_owned()],
            env_allow: vec![
                "HOME".to_owned(),
                "PATH".to_owned(),
                "LANG".to_owned(),
                "TMPDIR".to_owned(),
            ],
            mode: ExecutionMode::OneShot,
        },
        result: ResultSpec {
            source: ResultSource::JsonlAssistantFinal,
            media_type: "text/plain".to_owned(),
            max_bytes: 1_048_576,
            success_exit_codes: vec![0],
        },
        capabilities: BTreeMap::from([
            (
                "completion".to_owned(),
                supported("process_exit_with_json_capture"),
            ),
            ("cancel".to_owned(), supported("local_process_only")),
            ("model_select".to_owned(), supported("--model")),
            ("effort_select".to_owned(), supported("--thinking")),
        ]),
    })
}

fn generate_omp_manifest(
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if ![
        "--expected-report",
        "--reuse-worktree-objective",
        "--reuse-worktree-owner",
    ]
    .iter()
    .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "local.omp".to_owned(),
        adapter: OMP_ROLE_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--help".to_owned()],
            help_argv: vec!["--help".to_owned()],
        },
        launch: LaunchSpec {
            argv: vec![],
            model_argv: vec![],
            effort_argv: vec![],
            env_allow: vec![
                "HOME".to_owned(),
                "PATH".to_owned(),
                "LANG".to_owned(),
                "HERDR_ENV".to_owned(),
                "HERDR_PANE_ID".to_owned(),
            ],
            mode: ExecutionMode::OneShot,
        },
        result: ResultSpec {
            source: ResultSource::Stdout,
            media_type: "text/markdown".to_owned(),
            max_bytes: 1_048_576,
            success_exit_codes: vec![0],
        },
        capabilities: BTreeMap::from([
            (
                "completion".to_owned(),
                supported("contracted_report_and_terminal_herdr_state"),
            ),
            (
                "presentation".to_owned(),
                supported("herdr_optional_adapter"),
            ),
            (
                "cancel".to_owned(),
                Capability {
                    status: CapabilityStatus::Unknown,
                    semantics: "not_certified_in_v1".to_owned(),
                    evidence_ref: None,
                    tested_identity: None,
                },
            ),
        ]),
    })
}

fn supported(semantics: &str) -> Capability {
    Capability {
        status: CapabilityStatus::Supported,
        semantics: semantics.to_owned(),
        evidence_ref: Some("activation-help-digest".to_owned()),
        tested_identity: None,
    }
}

fn unsupported(semantics: &str) -> Capability {
    Capability {
        status: CapabilityStatus::Unsupported,
        semantics: semantics.to_owned(),
        evidence_ref: None,
        tested_identity: None,
    }
}

fn validate_harness_id(id: &str) -> Result<(), RegistryError> {
    if !id.starts_with("local.")
        || id.len() <= "local.".len()
        || id.contains("..")
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(RegistryError::InvalidHarnessId(id.to_owned()));
    }
    Ok(())
}

fn name_from_id(id: &str) -> Result<&str, RegistryError> {
    validate_harness_id(id)?;
    if id == "local.omp" {
        Ok("omp-role")
    } else {
        Ok(&id["local.".len()..])
    }
}

fn validate_package(
    harness_id: &str,
    manifest: &HarnessManifest,
    receipt: &ActivationReceipt,
    manifest_bytes: &[u8],
) -> Result<(), RegistryError> {
    if receipt.schema != "brgr.activation/v1"
        || receipt.harness_id != harness_id
        || manifest.id != harness_id
        || manifest.executable != receipt.executable_realpath
        || manifest.executable.canonicalize()? != receipt.executable_realpath
    {
        return Err(RegistryError::ActivationMismatch);
    }
    let canonical_bytes = manifest_bytes.strip_suffix(b"\n").unwrap_or(manifest_bytes);
    let actual = digest_bytes(canonical_bytes);
    if actual != receipt.manifest_digest {
        return Err(RegistryError::ManifestDrift {
            expected: receipt.manifest_digest.clone(),
            observed: actual,
        });
    }
    if receipt.tested_os != std::env::consts::OS || receipt.tested_arch != std::env::consts::ARCH {
        return Err(RegistryError::ActivationMismatch);
    }
    Ok(())
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
    #[error("help does not document a supported bounded fresh-run flag contract")]
    RequiredFlagsMissing,
    #[error("harness id contains unsafe path characters: {0}")]
    InvalidHarnessId(String),
    #[error("harness {0} needs an explicitly authorized scratch run before activation")]
    ScratchRunRequired(String),
    #[error("scratch prompt must be nonempty")]
    EmptyScratchPrompt,
    #[error("scratch run did not produce a successful nonempty result")]
    ScratchRunFailed,
    #[error("manifest differs from the currently observed process/v1 recipe")]
    ManifestNotObserved,
    #[error("manifest drifted: expected {expected}, observed {observed}")]
    ManifestDrift { expected: String, observed: String },
    #[error("manifest, activation, or executable identity disagree")]
    ActivationMismatch,
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
    use std::os::unix::fs::symlink;

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
        assert!(matches!(error, RegistryError::RequiredFlagsMissing));
        let error = generate_manifest("omp", PathBuf::from("/bin/echo"), "--prompt-file <path>")
            .unwrap_err();
        assert!(matches!(error, RegistryError::UnsupportedHarness));
    }

    #[test]
    fn registry_rejects_path_escape_identifiers() {
        for id in ["local.../private", "../local.foo", "local.Foo", "local."] {
            assert!(matches!(
                validate_harness_id(id),
                Err(RegistryError::InvalidHarnessId(_))
            ));
        }
    }

    fn fixture_executable(path: &Path) {
        fs::write(path, "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'fixture 1.0';;\n  --help) echo '  --prompt-file <path>  fresh run';;\n  --prompt-file) /bin/cat \"$2\";;\n  *) exit 2;;\nesac\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[tokio::test]
    async fn unknown_cli_needs_scratch_then_runs_without_name_switch() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        assert_eq!(draft.id, "local.mystery-agent");
        assert_eq!(draft.adapter, PROCESS_ADAPTER_V1);
        assert_eq!(draft.launch.argv, ["--prompt-file", "${input.prompt_file}"]);
        assert_eq!(
            draft.capabilities["model_select"].status,
            CapabilityStatus::Unsupported
        );
        assert!(matches!(
            registry.add(&executable).await,
            Err(RegistryError::ScratchRunRequired(_))
        ));
        registry.contract_test(&draft).await.unwrap();
        let receipt = registry
            .activate_with_scratch(&draft, root.path(), "fixture request")
            .await
            .unwrap();
        assert!(receipt.scratch_result_digest.is_some());
        assert_eq!(registry.health(&draft.id).unwrap(), Health::Healthy);
        let loaded = registry.load_healthy(&draft.id).unwrap();
        let result = ProcessRunner::run(
            &loaded,
            brgr_runner::RunRequest {
                workspace: root.path(),
                prompt: "second result",
                model: None,
                effort: None,
                deadline: Duration::from_secs(2),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.result, b"second result");
    }

    #[tokio::test]
    async fn altered_manifest_and_executable_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        registry
            .activate_with_scratch(&draft, root.path(), "fixture")
            .await
            .unwrap();
        let path = registry.manifest_path(&draft.id);
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed.push(b' ');
        fs::write(&path, changed).unwrap();
        assert!(matches!(
            registry.load_healthy(&draft.id),
            Err(RegistryError::ManifestDrift { .. })
        ));
        fs::write(&path, original).unwrap();
        fs::write(&executable, "#!/bin/sh\necho changed\n").unwrap();
        assert!(matches!(
            registry.load_healthy(&draft.id),
            Err(RegistryError::ExecutableDrift { .. })
        ));
    }

    #[tokio::test]
    async fn changed_recipe_cannot_be_activated() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let mut draft = registry.draft(&executable).await.unwrap();
        draft.launch.argv.push("--unobserved".to_owned());
        assert!(matches!(
            registry.contract_test(&draft).await,
            Err(RegistryError::ManifestNotObserved)
        ));
        assert!(matches!(
            registry
                .activate_with_scratch(&draft, root.path(), "fixture")
                .await,
            Err(RegistryError::ManifestNotObserved)
        ));
        assert!(!registry.activation_path(&draft.id).exists());
    }

    #[tokio::test]
    async fn omp_role_symlink_keeps_legacy_adapter_name() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("launch_tui.py");
        fs::write(&target, "#!/bin/sh\necho '--expected-report --reuse-worktree-objective --reuse-worktree-owner'\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.path().join("omp-role");
        symlink(&target, &alias).unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let receipt = registry.add(&alias).await.unwrap();
        assert_eq!(receipt.harness_id, "local.omp");
        assert_eq!(
            registry.load_healthy("local.omp").unwrap().adapter,
            OMP_ROLE_ADAPTER_V1
        );
    }

    #[tokio::test]
    async fn deep_health_detects_help_drift_without_executable_change() {
        let root = tempfile::tempdir().unwrap();
        let help_path = root.path().join("help.txt");
        fs::write(&help_path, "--prompt-file <path>\n").unwrap();
        let executable = root.path().join("fixture-agent");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  --version) echo 1.0;;\n  --help) /bin/cat '{}';;\n  --prompt-file) /bin/cat \"$2\";;\nesac\n",
                help_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        registry
            .activate_with_scratch(&draft, root.path(), "test")
            .await
            .unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Healthy
        );
        fs::write(&help_path, "--prompt-file <path>  changed\n").unwrap();
        assert_eq!(registry.health(&draft.id).unwrap(), Health::Healthy);
        assert!(matches!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Drifted { .. }
        ));
    }
}
