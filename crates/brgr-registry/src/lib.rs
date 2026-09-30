//! Evidence-backed harness registration and health checking.
//!
//! A registered harness is pinned to the evidence it was certified against: the
//! executable's identity and what its probe said. The two are re-checked by
//! different calls, and the difference matters. [`Registry::health`] only
//! re-digests the executable — cheap, spawns nothing — so it cannot see a
//! wrapper script whose interpreter or dependencies changed under an unchanged
//! file. [`Registry::health_probed`] also re-runs the bounded version and help
//! probes, and is the only source of [`Health::EvidenceChanged`]. Task admission
//! uses the probed check, so a harness replaced on disk reports as changed
//! instead of being run as though it were the one that was approved.
//!
//! Two rules shape everything an operator sees. Diagnostic output carries a
//! stable code and never the probe's own bytes, which may contain anything a
//! third-party binary chose to print. And the remedy is always a command the
//! operator runs deliberately — this crate never updates or re-activates a
//! harness on its own.
//!
//! ```
//! use brgr_registry::{Health, recertify_action};
//! use brgr_runner::HarnessManifest;
//!
//! // A probe whose output changed — reported by `health_probed`, never by
//! // `health` — has a fixed code. The differing text is carried in the variant
//! // for logging by the caller, never in the code.
//! let changed = Health::EvidenceChanged {
//!     expected: "sha256:aaa".to_owned(),
//!     observed: "sha256:bbb".to_owned(),
//! };
//! assert_eq!(changed.code(), "probe_evidence_changed");
//! assert_eq!(Health::Healthy.code(), "healthy");
//!
//! // The remedy names a command, and the path is POSIX-quoted so an operator
//! // can paste it even when it contains a space or a quote.
//! # let wire = r#"{
//! #   "schema": "brgr.harness/v1",
//! #   "id": "local.fixture",
//! #   "adapter": "process/v1",
//! #   "executable": "/opt/my harness/bin/run",
//! #   "probe": { "version_argv": ["--version"], "help_argv": ["--help"] },
//! #   "launch": { "argv": ["run"], "mode": "one_shot" },
//! #   "result": {
//! #     "source": { "kind": "stdout" },
//! #     "media_type": "text/plain",
//! #     "max_bytes": 4096,
//! #     "success_exit_codes": [0]
//! #   }
//! # }"#;
//! let manifest: HarnessManifest = serde_json::from_str(wire)?;
//! let action = recertify_action(&manifest);
//! assert!(action.contains("'/opt/my harness/bin/run'"), "{action}");
//! assert!(action.contains("brgr harness add"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod catalog;
mod error;
mod recipe;

pub use error::RegistryError;

use catalog::{executable_on_path, parse_model_catalog};
use recipe::{GENERIC, RECIPES, generate_manifest};

use std::{
    collections::BTreeSet,
    fmt::Write as FmtWrite,
    fs,
    io::Write as IoWrite,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use brgr_runner::{
    CapabilityStatus, ExecutionMode, ExecutionOutput, HarnessManifest, ModelCatalogFormat,
    OMP_ROLE_ADAPTER_V1, PROCESS_ADAPTER_V1, ProcessRunner, RunnerError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// How long a version, help, or model-catalog probe may take. It only bounds a
/// hung CLI; a healthy one answers in about a second. Five seconds was short
/// enough that an ordinary stall on a busy machine (a scan of freshly written
/// executables, a parallel build) reported a working harness as timed out and
/// refused the run.
const PROBE_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct Registry {
    root: PathBuf,
    control_home: PathBuf,
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
    #[serde(default)]
    pub registration_mode: String,
    #[serde(default)]
    pub contract_suite: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Health {
    Healthy,
    ExecutableChanged { expected: String, observed: String },
    Unspawnable,
    TimedOut,
    ExitedNonzero { exit_code: Option<i32> },
    EvidenceChanged { expected: String, observed: String },
}

impl Health {
    /// Stable diagnostic code for operator output. Never includes probe text.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::ExecutableChanged { .. } => "executable_changed",
            Self::Unspawnable => "unspawnable",
            Self::TimedOut => "timed_out",
            Self::ExitedNonzero { .. } => "exited_nonzero",
            Self::EvidenceChanged { .. } => "probe_evidence_changed",
        }
    }
}

/// Concrete re-certification command. Never updates, copies, or activates.
/// The executable path is POSIX-quoted so spaces are copy-safe; probe bytes are never included.
#[must_use]
pub fn recertify_action(manifest: &HarnessManifest) -> String {
    let executable = quote_executable(&manifest.executable);
    if manifest.adapter == OMP_ROLE_ADAPTER_V1 {
        format!("re-certify with `brgr harness add {executable} --presentation-only`")
    } else {
        format!(
            "re-certify with `brgr harness add {executable} --workspace <dir> --prompt <prompt>`"
        )
    }
}

fn quote_executable(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut quoted = String::from("'");
    for ch in raw.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
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
        Ok(Self {
            control_home: root.clone(),
            root,
        })
    }

    /// Opens a registry whose scratch runs must remain outside the entire
    /// supervisor control home, not only the registry subdirectory.
    ///
    /// # Errors
    ///
    /// Rejects a control home that does not contain the registry root.
    pub fn open_with_control_home(
        root: impl Into<PathBuf>,
        control_home: &Path,
    ) -> Result<Self, RegistryError> {
        let mut registry = Self::open(root)?;
        let canonical_home = control_home.canonicalize()?;
        if !registry.root.canonicalize()?.starts_with(&canonical_home) {
            return Err(RegistryError::InvalidControlHome);
        }
        registry.control_home = canonical_home;
        Ok(registry)
    }

    /// Lists every package name present in either registry directory so an
    /// incomplete activation is visible to diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when a registry entry cannot be read or has
    /// an invalid harness identifier.
    pub fn registered_harness_ids(&self) -> Result<Vec<String>, RegistryError> {
        let mut ids = BTreeSet::new();
        for directory in ["manifests", "activations"] {
            for entry in fs::read_dir(self.root.join(directory))? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                validate_harness_id(&stem)?;
                ids.insert(stem.into_owned());
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Probes and activates only the explicitly presentation-only Herdr adapter.
    /// Process harnesses require a successful authorized scratch run.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when probing fails, required documented flags
    /// are absent, or activation data cannot be persisted.
    pub async fn add(&self, executable: &Path) -> Result<ActivationReceipt, RegistryError> {
        let (manifest, probe) = draft_manifest(executable).await?;
        if manifest.id != "local.omp-herdr" || manifest.adapter != OMP_ROLE_ADAPTER_V1 {
            return Err(RegistryError::ScratchRunRequired(manifest.id));
        }
        self.persist_activation(&manifest, &probe, None, RecipeAuthority::Generated)
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
        let requested_name = name_from_id(&manifest.id, &manifest.adapter)?;
        let (observed, _) = draft_manifest_as(&manifest.executable, requested_name).await?;
        if &observed != manifest || manifest.adapter != PROCESS_ADAPTER_V1 {
            return Err(RegistryError::ManifestNotObserved);
        }
        manifest.validate()?;
        Ok(())
    }

    /// Checks an agent-authored process recipe against bounded native help and
    /// a closed set of argv/environment substitutions without invoking a model.
    ///
    /// # Errors
    ///
    /// Rejects reserved names, unobserved flags, unsafe environment expansion,
    /// or malformed capability claims before scratch activation.
    pub async fn contract_test_custom(
        &self,
        manifest: &HarnessManifest,
    ) -> Result<(), RegistryError> {
        probe_custom_contract(manifest).await.map(|_| ())
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
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<ActivationReceipt, RegistryError> {
        self.activate_checked(
            manifest,
            workspace,
            prompt,
            model,
            effort,
            RecipeAuthority::Generated,
        )
        .await
    }

    /// Activates an agent-authored declarative recipe only after its own
    /// documented-flag contract and an authorized bounded live scratch pass.
    ///
    /// # Errors
    ///
    /// Rejects alias collisions, recipe drift, failed scratch output, or a
    /// privileged environment name before persisting an activation.
    pub async fn activate_custom_with_scratch(
        &self,
        manifest: &HarnessManifest,
        workspace: &Path,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<ActivationReceipt, RegistryError> {
        if self.manifest_path(&manifest.id).exists() {
            let existing: HarnessManifest =
                serde_json::from_slice(&fs::read(self.manifest_path(&manifest.id))?)?;
            if existing.executable != manifest.executable {
                return Err(RegistryError::HarnessAliasCollision(manifest.id.clone()));
            }
        }
        self.activate_checked(
            manifest,
            workspace,
            prompt,
            model,
            effort,
            RecipeAuthority::Custom,
        )
        .await
    }

    async fn activate_checked(
        &self,
        manifest: &HarnessManifest,
        workspace: &Path,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
        authority: RecipeAuthority,
    ) -> Result<ActivationReceipt, RegistryError> {
        let workspace = workspace.canonicalize()?;
        let control = self.control_home.canonicalize()?;
        if workspace.starts_with(&control) || control.starts_with(&workspace) {
            return Err(RegistryError::ScratchOverlapsControlHome);
        }
        let before = match authority {
            RecipeAuthority::Generated => {
                self.contract_test(manifest).await?;
                let requested_name = name_from_id(&manifest.id, &manifest.adapter)?;
                draft_manifest_as(&manifest.executable, requested_name)
                    .await?
                    .1
            }
            RecipeAuthority::Custom => probe_custom_contract(manifest).await?,
        };
        if prompt.trim().is_empty() {
            return Err(RegistryError::EmptyScratchPrompt);
        }
        if model.is_some() && manifest.launch.model_argv.is_empty() {
            return Err(RegistryError::UnsupportedScratchModel);
        }
        if effort.is_some() && manifest.launch.effort_argv.is_empty() {
            return Err(RegistryError::UnsupportedScratchEffort);
        }
        self.preflight_model(manifest, model).await?;
        let output = ProcessRunner::run(
            manifest,
            brgr_runner::RunRequest {
                workspace: &workspace,
                prompt,
                criteria: None,
                instructions: None,
                model,
                effort,
                permission: None,
                deadline: Duration::from_mins(1),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await?;
        if !output.succeeded(manifest) || output.result.is_empty() {
            // Say which condition failed. The bare "did not succeed" left a
            // working CLI looking broken with nothing to go on. The harness's own
            // output is never echoed, as everywhere else in this crate.
            return Err(RegistryError::ScratchRunFailed(format!(
                "exit code {}, timed out {}, output truncated {}, {} result bytes, {} stderr bytes",
                output
                    .exit_code
                    .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                output.timed_out,
                output.output_truncated,
                output.result.len(),
                output.stderr.len()
            )));
        }
        let after = match authority {
            RecipeAuthority::Generated => {
                let requested_name = name_from_id(&manifest.id, &manifest.adapter)?;
                let (observed, probe) =
                    draft_manifest_as(&manifest.executable, requested_name).await?;
                if observed != *manifest {
                    return Err(RegistryError::ManifestNotObserved);
                }
                probe
            }
            RecipeAuthority::Custom => probe_custom_contract(manifest).await?,
        };
        if after != before {
            return Err(RegistryError::ManifestNotObserved);
        }
        self.persist_activation(
            manifest,
            &after,
            Some(digest_bytes(&output.result)),
            authority,
        )
    }

    /// Loads an activated manifest after checking executable identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the package is missing, malformed, or has
    /// drifted since activation.
    pub fn load_healthy(&self, harness_id: &str) -> Result<HarnessManifest, RegistryError> {
        self.load_healthy_with_receipt(harness_id)
            .map(|(manifest, _)| manifest)
    }

    /// Loads one validated activation snapshot for task admission.
    ///
    /// # Errors
    ///
    /// Rejects a malformed package or changed executable before returning the
    /// manifest and the digest that the task will pin.
    pub fn load_healthy_with_receipt(
        &self,
        harness_id: &str,
    ) -> Result<(HarnessManifest, ActivationReceipt), RegistryError> {
        validate_harness_id(harness_id)?;
        let manifest_bytes = fs::read(self.manifest_path(harness_id))?;
        let manifest: HarnessManifest = serde_json::from_slice(&manifest_bytes)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        validate_package(harness_id, &manifest, &receipt, &manifest_bytes)?;
        match health_for(&receipt)? {
            Health::Healthy => {
                manifest.validate()?;
                Ok((manifest, receipt))
            }
            Health::ExecutableChanged { expected, observed } => {
                Err(RegistryError::ExecutableDrift { expected, observed })
            }
            Health::Unspawnable => Err(RegistryError::ExecutableUnspawnable),
            Health::TimedOut | Health::ExitedNonzero { .. } | Health::EvidenceChanged { .. } => {
                Err(RegistryError::ProbeFailed)
            }
        }
    }

    /// Rechecks a task-pinned executable without consulting a newer activation.
    ///
    /// # Errors
    ///
    /// Rejects any change to the executable bytes before process spawn.
    pub fn verify_pinned_executable(
        manifest: &HarnessManifest,
        expected_digest: &str,
    ) -> Result<(), RegistryError> {
        let actual = digest_file(&manifest.executable)?;
        if actual != expected_digest {
            return Err(RegistryError::ExecutableDrift {
                expected: expected_digest.to_owned(),
                observed: actual,
            });
        }
        Ok(())
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
    /// Returns an error when package identity cannot be read. Probe spawn,
    /// timeout, nonzero exit, and evidence drift are classified as [`Health`].
    /// Probes inherit the caller `PATH` via [`ProcessRunner::probe`].
    pub async fn health_probed(&self, harness_id: &str) -> Result<Health, RegistryError> {
        validate_harness_id(harness_id)?;
        let manifest_bytes = fs::read(self.manifest_path(harness_id))?;
        let manifest: HarnessManifest = serde_json::from_slice(&manifest_bytes)?;
        let receipt: ActivationReceipt =
            serde_json::from_slice(&fs::read(self.activation_path(harness_id))?)?;
        validate_package_identity(harness_id, &manifest, &receipt, &manifest_bytes)?;
        if !receipt.executable_realpath.is_file() {
            return Ok(Health::Unspawnable);
        }
        validate_package(harness_id, &manifest, &receipt, &manifest_bytes)?;
        match health_for(&receipt)? {
            Health::Healthy => {}
            other => return Ok(other),
        }
        manifest.validate()?;
        // Help is digested the way registration digested it, so a CLI that
        // prints help to stderr is not reported as changed on every check.
        for (argv, expected, is_help) in [
            (&manifest.probe.version_argv, &receipt.version_digest, false),
            (&manifest.probe.help_argv, &receipt.help_digest, true),
        ] {
            let observed =
                match ProcessRunner::probe(&manifest.executable, argv, PROBE_DEADLINE).await {
                    Ok(output) => output,
                    Err(RunnerError::SpawnIo(_) | RunnerError::InvalidExecutable(_)) => {
                        return Ok(Health::Unspawnable);
                    }
                    Err(error) => return Err(error.into()),
                };
            let evidence = if is_help {
                help_output(&observed)
            } else {
                &observed.stdout
            };
            match classify_probe(&observed, evidence, expected)? {
                Health::Healthy => {}
                other => return Ok(other),
            }
        }
        Ok(Health::Healthy)
    }

    /// Operator action for a stored recipe. Does not re-run or mutate it.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the manifest cannot be read.
    pub fn recertify_action_for(&self, harness_id: &str) -> Result<String, RegistryError> {
        validate_harness_id(harness_id)?;
        let manifest: HarnessManifest =
            serde_json::from_slice(&fs::read(self.manifest_path(harness_id))?)?;
        Ok(recertify_action(&manifest))
    }

    /// Resolves an exact requested model through a bounded native catalog
    /// before task admission or a paid scratch run.
    ///
    /// # Errors
    ///
    /// Fails closed when no catalog exists, probing fails, or the exact
    /// selector is absent. The Herdr presentation adapter probes the local OMP
    /// catalog before brgr admission; its launcher rechecks at dispatch.
    pub async fn preflight_model(
        &self,
        manifest: &HarnessManifest,
        requested: Option<&str>,
    ) -> Result<(), RegistryError> {
        let Some(requested) = requested else {
            return Ok(());
        };
        if requested.is_empty() || requested == "auto" {
            return Err(RegistryError::ModelNotExact(requested.to_owned()));
        }
        let catalog = manifest
            .probe
            .model_catalog
            .as_ref()
            .ok_or_else(|| RegistryError::ModelCatalogMissing(manifest.id.clone()))?;
        if catalog.format == ModelCatalogFormat::CliValidated {
            // The CLI itself refuses an unknown name before any paid request.
            return Ok(());
        }
        if catalog.argv.is_empty() || catalog.argv.len() > 64 {
            return Err(RegistryError::InvalidModelCatalog);
        }
        let argv = catalog
            .argv
            .iter()
            .map(|arg| {
                let model_id = requested.rsplit('/').next().unwrap_or(requested);
                let rendered = arg
                    .replace("${model.query}", requested)
                    .replace("${model.id}", model_id);
                if rendered.contains("${") || rendered.contains('\0') {
                    Err(RegistryError::InvalidModelCatalog)
                } else {
                    Ok(rendered)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let catalog_executable = if manifest.adapter == OMP_ROLE_ADAPTER_V1 {
            executable_on_path("omp")?
        } else {
            manifest.executable.clone()
        };
        let output = ProcessRunner::probe(&catalog_executable, &argv, PROBE_DEADLINE).await?;
        if output.exit_code != Some(0) || output.timed_out || output.output_truncated {
            return Err(RegistryError::ModelCatalogProbeFailed);
        }
        let selectors = parse_model_catalog(&catalog.format, &output.stdout)?;
        if !selectors.contains(requested) {
            return Err(RegistryError::ModelNotInCatalog(requested.to_owned()));
        }
        Ok(())
    }

    fn persist_activation(
        &self,
        manifest: &HarnessManifest,
        probe: &ProbeEvidence,
        scratch_result_digest: Option<String>,
        authority: RecipeAuthority,
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
            registration_mode: authority.as_str().to_owned(),
            contract_suite: if manifest.adapter == OMP_ROLE_ADAPTER_V1 {
                "presentation-probe/v1"
            } else {
                "process-contract/v1"
            }
            .to_owned(),
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProbeEvidence {
    executable: String,
    version: String,
    help: String,
}

#[derive(Clone, Copy)]
enum RecipeAuthority {
    Generated,
    Custom,
}

impl RecipeAuthority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Custom => "agent_authored",
        }
    }
}

async fn probe_custom_contract(manifest: &HarnessManifest) -> Result<ProbeEvidence, RegistryError> {
    manifest.validate()?;
    validate_harness_id(&manifest.id)?;
    if manifest.adapter != PROCESS_ADAPTER_V1
        || matches!(
            manifest.id.as_str(),
            "local.gjc"
                | "local.omp"
                | "local.omp-herdr"
                | "local.cursor-cli"
                | "local.command-code"
                | "local.devin"
        )
    {
        return Err(RegistryError::ReservedCustomHarness);
    }
    if manifest.executable.canonicalize()? != manifest.executable {
        return Err(RegistryError::ExecutableMustBeCanonical);
    }
    if manifest.probe.version_argv != ["--version"] || manifest.probe.help_argv != ["--help"] {
        return Err(RegistryError::UnsafeCustomProbe);
    }
    if manifest.launch.mode != ExecutionMode::OneShot {
        return Err(RegistryError::UnsupportedCustomMode);
    }
    if manifest
        .launch
        .env_allow
        .iter()
        .any(|name| !matches!(name.as_str(), "HOME" | "PATH" | "LANG" | "TMPDIR"))
    {
        return Err(RegistryError::CustomEnvironmentDenied);
    }
    let completion = manifest.capabilities.get("completion");
    if !completion.is_some_and(|cap| cap.status == CapabilityStatus::Supported) {
        return Err(RegistryError::CustomCapabilityMismatch(
            "completion".to_owned(),
        ));
    }
    for (name, args) in [
        ("model_select", &manifest.launch.model_argv),
        ("effort_select", &manifest.launch.effort_argv),
    ] {
        let claimed = manifest
            .capabilities
            .get(name)
            .is_some_and(|cap| cap.status == CapabilityStatus::Supported);
        if claimed == args.is_empty() {
            return Err(RegistryError::CustomCapabilityMismatch(name.to_owned()));
        }
    }
    let version = ProcessRunner::probe(
        &manifest.executable,
        &manifest.probe.version_argv,
        PROBE_DEADLINE,
    )
    .await?;
    let help = ProcessRunner::probe(
        &manifest.executable,
        &manifest.probe.help_argv,
        PROBE_DEADLINE,
    )
    .await?;
    if version.exit_code != Some(0)
        || help.exit_code != Some(0)
        || version.timed_out
        || help.timed_out
        || version.output_truncated
        || help.output_truncated
    {
        return Err(RegistryError::ProbeFailed);
    }
    let help_text = String::from_utf8_lossy(help_output(&help));
    validate_custom_argv(manifest, &help_text)?;
    Ok(ProbeEvidence {
        executable: digest_file(&manifest.executable)?,
        version: digest_bytes(&version.stdout),
        help: digest_bytes(help_output(&help)),
    })
}

fn validate_custom_argv(manifest: &HarnessManifest, help: &str) -> Result<(), RegistryError> {
    let catalog_argv = manifest
        .probe
        .model_catalog
        .as_ref()
        .map_or(&[][..], |catalog| catalog.argv.as_slice());
    if let Some(forbidden) = catalog_argv.iter().find(|argument| {
        [
            "delete", "remove", "update", "set", "install", "login", "logout", "clear", "reset",
            "purge", "rm",
        ]
        .contains(&argument.as_str())
    }) {
        return Err(RegistryError::UnsafeCustomPermission(forbidden.clone()));
    }
    if let Some(first) = catalog_argv.first()
        && !first.starts_with('-')
        && !help.split_whitespace().any(|word| word == first)
    {
        return Err(RegistryError::UndocumentedCustomFlag(first.clone()));
    }
    for argument in manifest
        .launch
        .argv
        .iter()
        .chain(&manifest.launch.model_argv)
        .chain(&manifest.launch.effort_argv)
        .chain(catalog_argv)
    {
        if [
            "--yolo",
            "--dangerously-skip-permissions",
            "--auto-approve",
            "--auto-accept",
            "--force",
            "--approve-mcps",
            "--tools-all",
            "--permission-mode",
            "--approval-mode",
            "--sandbox",
            "--config",
        ]
        .iter()
        .any(|flag| argument == flag || argument.starts_with(&format!("{flag}=")))
        {
            return Err(RegistryError::UnsafeCustomPermission(argument.clone()));
        }
        if argument.contains("${input.prompt}") && argument != "${input.prompt}" {
            return Err(RegistryError::UnsafeCustomPlaceholder(argument.clone()));
        }
        let mut rest = argument.as_str();
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let end = after
                .find('}')
                .ok_or_else(|| RegistryError::UnsafeCustomPlaceholder(argument.clone()))?;
            if !matches!(
                &after[..end],
                "input.prompt"
                    | "input.prompt_file"
                    | "task.workspace"
                    | "route.model"
                    | "route.effort"
                    | "model.query"
                    | "model.id"
            ) {
                return Err(RegistryError::UnsafeCustomPlaceholder(argument.clone()));
            }
            rest = &after[end + 1..];
        }
        if argument.starts_with('-') {
            let flag = argument.split('=').next().unwrap_or(argument);
            let documented = help
                .split(|character: char| {
                    character.is_whitespace()
                        || matches!(
                            character,
                            ',' | '=' | '<' | '>' | '[' | ']' | '(' | ')' | ':'
                        )
                })
                .any(|token| token == flag);
            if !documented {
                return Err(RegistryError::UndocumentedCustomFlag(flag.to_owned()));
            }
        }
    }
    Ok(())
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

/// The help a probe printed. Some CLIs print it to stderr — `opencode run
/// --help` writes nothing to stdout — so stderr counts when stdout is empty.
/// A CLI that prints to stdout keeps exactly the evidence it had before.
fn help_output(help: &ExecutionOutput) -> &[u8] {
    if help.stdout.is_empty() {
        &help.stderr
    } else {
        &help.stdout
    }
}

async fn draft_manifest_as(
    executable: &Path,
    requested_name: &str,
) -> Result<(HarnessManifest, ProbeEvidence), RegistryError> {
    let realpath = executable.canonicalize()?;
    if !realpath.is_file() {
        return Err(RegistryError::UnsupportedHarness);
    }
    // The recipe says where its version and help live; a CLI whose flags
    // belong to a subcommand documents them only there.
    let recipe = RECIPES
        .iter()
        .find(|recipe| recipe.names.contains(&requested_name))
        .unwrap_or(&GENERIC);
    let version_argv = vec![recipe.version_argv.to_owned()];
    let help_argv: Vec<String> = recipe
        .help_argv
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
    let version = ProcessRunner::probe(&realpath, &version_argv, PROBE_DEADLINE).await?;
    let help = ProcessRunner::probe(&realpath, &help_argv, PROBE_DEADLINE).await?;
    if version.exit_code != Some(0)
        || help.exit_code != Some(0)
        || version.timed_out
        || help.timed_out
        || version.output_truncated
        || help.output_truncated
    {
        return Err(RegistryError::ProbeFailed);
    }
    let help_text = String::from_utf8_lossy(help_output(&help));
    let manifest = generate_manifest(requested_name, realpath, &help_text)?;
    manifest.validate()?;
    let executable_digest = digest_file(&manifest.executable)?;
    Ok((
        manifest,
        ProbeEvidence {
            executable: executable_digest,
            version: digest_bytes(&version.stdout),
            help: digest_bytes(help_output(&help)),
        },
    ))
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

fn name_from_id<'a>(id: &'a str, adapter: &str) -> Result<&'a str, RegistryError> {
    validate_harness_id(id)?;
    if matches!(id, "local.omp" | "local.omp-herdr") && adapter == OMP_ROLE_ADAPTER_V1 {
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
    validate_package_identity(harness_id, manifest, receipt, manifest_bytes)?;
    if manifest.executable.canonicalize()? != receipt.executable_realpath {
        return Err(RegistryError::ActivationMismatch);
    }
    Ok(())
}

fn validate_package_identity(
    harness_id: &str,
    manifest: &HarnessManifest,
    receipt: &ActivationReceipt,
    manifest_bytes: &[u8],
) -> Result<(), RegistryError> {
    if receipt.schema != "brgr.activation/v1"
        || receipt.harness_id != harness_id
        || manifest.id != harness_id
        || manifest.executable != receipt.executable_realpath
    {
        return Err(RegistryError::ActivationMismatch);
    }
    if manifest.adapter == PROCESS_ADAPTER_V1
        && receipt
            .scratch_result_digest
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(RegistryError::ScratchCertificationMissing(
            harness_id.to_owned(),
        ));
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
    match fs::read(&receipt.executable_realpath) {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(Health::Unspawnable)
        }
        Err(error) => Err(error.into()),
        Ok(bytes) => {
            let observed = digest_bytes(&bytes);
            if observed == receipt.executable_digest {
                Ok(Health::Healthy)
            } else {
                Ok(Health::ExecutableChanged {
                    expected: receipt.executable_digest.clone(),
                    observed,
                })
            }
        }
    }
}

fn classify_probe(
    observed: &ExecutionOutput,
    evidence: &[u8],
    expected_digest: &str,
) -> Result<Health, RegistryError> {
    if observed.timed_out {
        return Ok(Health::TimedOut);
    }
    if observed.exit_code != Some(0) {
        return Ok(Health::ExitedNonzero {
            exit_code: observed.exit_code,
        });
    }
    if observed.output_truncated {
        return Err(RegistryError::ProbeFailed);
    }
    let observed_digest = digest_bytes(evidence);
    if observed_digest == expected_digest {
        Ok(Health::Healthy)
    } else {
        Ok(Health::EvidenceChanged {
            expected: expected_digest.to_owned(),
            observed: observed_digest,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{supported, unsupported};
    use brgr_runner::PermissionLevel;
    use brgr_runner::{
        LaunchSpec, MANIFEST_SCHEMA_V1, ModelCatalogSpec, ProbeSpec, ResultSource, ResultSpec,
    };
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    fn scratch_workspace(root: &tempfile::TempDir) -> PathBuf {
        let workspace = root.path().join("scratch");
        fs::create_dir_all(&workspace).unwrap();
        workspace
    }

    /// Help printed to stderr is the evidence when stdout is empty, in both
    /// registration and later health checks; a CLI printing to stdout keeps
    /// exactly the evidence it had.
    #[test]
    fn help_on_stderr_is_evidence_only_when_stdout_is_empty() {
        let output = |stdout: &[u8], stderr: &[u8]| ExecutionOutput {
            exit_code: Some(0),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            result: vec![],
            observed_model: None,
            timed_out: false,
            cancelled: false,
            output_truncated: false,
            elapsed: Duration::ZERO,
        };
        assert_eq!(help_output(&output(b"", b"--auto")), b"--auto");
        assert_eq!(
            help_output(&output(b"--help text", b"noise")),
            b"--help text"
        );
        let stderr_only = output(b"", b"--auto");
        let expected = digest_bytes(b"--auto");
        assert!(matches!(
            classify_probe(&stderr_only, help_output(&stderr_only), &expected).unwrap(),
            Health::Healthy
        ));
    }

    #[test]
    fn a_lines_catalog_takes_bare_selectors_and_ignores_everything_else() {
        let output = b"opencode/big-pickle\nopencode-go/glm-5.3\n\nWARN something happened\nnot-a-selector\n";
        let parsed = parse_model_catalog(&ModelCatalogFormat::Lines, output).unwrap();
        assert_eq!(
            parsed.into_iter().collect::<Vec<_>>(),
            ["opencode-go/glm-5.3", "opencode/big-pickle"]
        );
        assert!(parse_model_catalog(&ModelCatalogFormat::Lines, b"no selectors here\n").is_err());
    }

    #[test]
    fn model_catalog_formats_keep_only_exact_selectors() {
        let json = br#"{"models":[{"selector":"workbuddy/deepseek-v4.1-flash"}]}"#;
        let parsed = parse_model_catalog(
            &ModelCatalogFormat::JsonSelectors {
                pointer: "/models".to_owned(),
                field: "selector".to_owned(),
            },
            json,
        )
        .unwrap();
        assert!(parsed.contains("workbuddy/deepseek-v4.1-flash"));
        assert!(!parsed.contains("workbuddy/deepseek-v4.1"));
        assert!(
            parse_model_catalog(
                &ModelCatalogFormat::JsonSelectors {
                    pointer: "/models".to_owned(),
                    field: "selector".to_owned(),
                },
                br#"{"models":[]}"#,
            )
            .unwrap()
            .is_empty()
        );
        let gjc = b"Canonical models\ncanonical selected variants\ngpt-5.6-luna openai-codex/gpt-5.6-luna 8\nProvider models\nprovider model context\nopencode-zen gpt-5.6-luna 1M\n";
        let parsed = parse_model_catalog(&ModelCatalogFormat::CanonicalProviderTable, gjc).unwrap();
        for selector in [
            "gpt-5.6-luna",
            "openai-codex/gpt-5.6-luna",
            "opencode-zen/gpt-5.6-luna",
        ] {
            assert!(parsed.contains(selector));
        }
        let cursor = b"Available models\nauto - Auto (default)\ngpt-5.6-luna-low-fast - Luna\n";
        let parsed = parse_model_catalog(&ModelCatalogFormat::DashSeparated, cursor).unwrap();
        assert!(parsed.contains("gpt-5.6-luna-low-fast"));
        assert!(!parsed.contains("auto"));
        let command = b"Available models\nOpen-Source Group\ndeepseek/deepseek-v4-flash   fast\ngpt-5.6-luna   low cost\n";
        let parsed = parse_model_catalog(&ModelCatalogFormat::FirstColumn, command).unwrap();
        assert!(parsed.contains("deepseek/deepseek-v4-flash"));
        assert!(parsed.contains("gpt-5.6-luna"));
        assert!(!parsed.contains("Available"));
        assert!(!parsed.contains("Open-Source"));
        assert!(
            parse_model_catalog(
                &ModelCatalogFormat::JsonSelectors {
                    pointer: "/models".to_owned(),
                    field: "selector".to_owned()
                },
                b"not json",
            )
            .is_err()
        );
    }

    /// A CLI that refuses an unknown model itself gets the name passed through,
    /// but an empty or `auto` name is still refused before anything runs.
    #[tokio::test]
    async fn a_cli_validated_catalog_passes_exact_names_through() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let help = "-p, --print --output-format <format> --model <model> --permission-mode <mode>";
        let manifest = generate_manifest("claude", PathBuf::from("/bin/echo"), help).unwrap();
        manifest.validate().unwrap();
        registry
            .preflight_model(&manifest, Some("haiku"))
            .await
            .unwrap();
        registry
            .preflight_model(&manifest, Some("claude-opus-5-5"))
            .await
            .unwrap();
        assert!(matches!(
            registry.preflight_model(&manifest, Some("auto")).await,
            Err(RegistryError::ModelNotExact(_))
        ));
        let cline = generate_manifest(
            "cline",
            PathBuf::from("/bin/echo"),
            "-p, --plan --auto-approve <boolean> -m, --model <model-id> --thinking <level>",
        )
        .unwrap();
        // Cline would save the name as its own default, so brgr never passes one.
        assert!(cline.launch.model_argv.is_empty());
        assert!(matches!(
            registry
                .preflight_model(&cline, Some("anthropic/claude-haiku-4-5"))
                .await,
            Err(RegistryError::ModelCatalogMissing(_))
        ));
    }

    #[tokio::test]
    async fn missing_catalog_blocks_requested_model_without_a_probe() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let manifest = registry.draft(&executable).await.unwrap();
        assert!(manifest.probe.model_catalog.is_none());
        registry.preflight_model(&manifest, None).await.unwrap();
        assert!(matches!(
            registry
                .preflight_model(&manifest, Some("provider/model"))
                .await,
            Err(RegistryError::ModelCatalogMissing(_))
        ));
        assert!(matches!(
            registry.preflight_model(&manifest, Some("auto")).await,
            Err(RegistryError::ModelNotExact(_))
        ));
    }

    #[tokio::test]
    async fn authored_catalog_certifies_a_future_model_selecting_harness() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("future-agent");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$1\" in\n --version) echo 'future 1';;\n --help) echo '--prompt-file <path> --model <name> --list-models';;\n --list-models) echo 'future-model - supported';;\n --prompt-file) /bin/cat \"$2\";;\n *) exit 2;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "local.future-agent".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: executable.canonicalize().unwrap(),
            probe: ProbeSpec {
                version_argv: vec!["--version".to_owned()],
                help_argv: vec!["--help".to_owned()],
                model_catalog: Some(ModelCatalogSpec {
                    argv: vec!["--list-models".to_owned()],
                    format: ModelCatalogFormat::DashSeparated,
                }),
            },
            launch: LaunchSpec {
                argv: vec![
                    "--prompt-file".to_owned(),
                    "${input.prompt_file}".to_owned(),
                ],
                model_argv: vec!["--model".to_owned(), "${route.model}".to_owned()],
                effort_argv: vec![],
                env_allow: vec!["HOME".to_owned(), "PATH".to_owned()],
                mode: ExecutionMode::OneShot,
                permission_argv: brgr_runner::PermissionArgv::default(),
                interactive: None,
            },
            result: ResultSpec {
                source: ResultSource::Stdout,
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
                success_exit_codes: vec![0],
            },
            capabilities: BTreeMap::from([
                ("completion".to_owned(), supported("process_exit")),
                ("model_select".to_owned(), supported("--model")),
            ]),
        };
        let registry = Registry::open(root.path().join("registry")).unwrap();
        registry.contract_test_custom(&manifest).await.unwrap();
        let receipt = registry
            .activate_custom_with_scratch(
                &manifest,
                &scratch_workspace(&root),
                "future fixture",
                Some("future-model"),
                None,
            )
            .await
            .unwrap();
        assert!(receipt.scratch_result_digest.is_some());
        assert!(matches!(
            registry
                .preflight_model(&manifest, Some("missing-model"))
                .await,
            Err(RegistryError::ModelNotInCatalog(_))
        ));
        let mut unsafe_catalog = manifest.clone();
        unsafe_catalog
            .probe
            .model_catalog
            .as_mut()
            .unwrap()
            .argv
            .push("delete".to_owned());
        assert!(matches!(
            registry.contract_test_custom(&unsafe_catalog).await,
            Err(RegistryError::UnsafeCustomPermission(_))
        ));
    }

    #[tokio::test]
    async fn gjc_catalog_filters_by_model_id_but_matches_full_provider_selector() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("gjc");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$1\" in\n --version) echo 'gjc 1';;\n --help) echo '--mode=<value> --no-session --no-mcp -p, --print --model=<value>';;\n --list-models=gpt-5.6-luna) printf '%s\\n' 'Canonical models' 'canonical selected' 'gpt-5.6-luna openai-codex/gpt-5.6-luna' 'Provider models' 'provider model' 'opencode-zen gpt-5.6-luna';;\n *) exit 2;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let manifest = registry.draft(&executable).await.unwrap();
        registry
            .preflight_model(&manifest, Some("opencode-zen/gpt-5.6-luna"))
            .await
            .unwrap();
        registry
            .preflight_model(&manifest, Some("gpt-5.6-luna"))
            .await
            .unwrap();
    }

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

    #[tokio::test]
    async fn scratch_rejects_any_control_home_child_and_symlink_alias() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("control");
        let store = home.join("store");
        fs::create_dir_all(&store).unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open_with_control_home(home.join("registry"), &home).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        let alias = root.path().join("alias");
        symlink(&store, &alias).unwrap();
        for workspace in [&store, &alias] {
            assert!(matches!(
                registry
                    .activate_with_scratch(&draft, workspace, "probe", None, None)
                    .await,
                Err(RegistryError::ScratchOverlapsControlHome)
            ));
        }
        assert!(!registry.activation_path(&draft.id).exists());
    }

    #[test]
    fn unknown_harness_is_not_guessed() {
        let error = generate_manifest("mystery", PathBuf::from("/bin/echo"), "--help").unwrap_err();
        assert!(matches!(error, RegistryError::RequiredFlagsMissing));
        let error = generate_manifest("omp", PathBuf::from("/bin/echo"), "--prompt-file <path>")
            .unwrap_err();
        assert!(matches!(error, RegistryError::RequiredFlagsMissing));
    }

    #[test]
    fn omp_process_recipe_is_separate_from_herdr_presentation() {
        let help = "-p, --print --mode=<value> --no-session --no-prewalk --no-extensions --no-title --model=<value> --thinking=<value>";
        let process = generate_manifest("omp", PathBuf::from("/bin/echo"), help).unwrap();
        assert_eq!(process.id, "local.omp");
        assert_eq!(process.adapter, PROCESS_ADAPTER_V1);
        assert_eq!(process.result.source, ResultSource::JsonlAssistantFinal);
        assert!(!process.launch.env_allow.contains(&"HERDR_ENV".to_owned()));
        assert_eq!(
            process.capabilities["cancel"].status,
            CapabilityStatus::Supported
        );
    }

    #[test]
    fn named_cursor_and_command_code_recipes_default_to_full_and_can_be_locked() {
        let cursor_help = "--print --mode <mode> --output-format <format> --model <model>";
        let cursor =
            generate_manifest("cursor-agent", PathBuf::from("/bin/echo"), cursor_help).unwrap();
        assert_eq!(cursor.id, "local.cursor-cli");
        // No permission flag in the base argv: the level decides it.
        assert!(!cursor.launch.argv.contains(&"ask".to_owned()));
        assert_eq!(cursor.permission_arguments(None).unwrap(), ["--force"]);
        assert!(
            cursor
                .permission_arguments(Some(PermissionLevel::Edits))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cursor
                .permission_arguments(Some(PermissionLevel::ReadOnly))
                .unwrap(),
            ["--mode", "plan"]
        );
        assert!(cursor.launch.argv.contains(&"${input.prompt}".to_owned()));
        assert_eq!(
            cursor.capabilities["model_select"].status,
            CapabilityStatus::Supported
        );
        assert_eq!(
            cursor.capabilities["effort_select"].status,
            CapabilityStatus::Unsupported
        );
        assert!(generate_manifest("cursor-agent", PathBuf::from("/bin/echo"), "--print").is_err());

        let command_code_help = "--print [query] --permission-mode <mode> --no-session --no-skills --skip-onboarding --no-auto-update --max-turns <number> --model <model>";
        let command_code = generate_manifest(
            "command-code",
            PathBuf::from("/bin/echo"),
            command_code_help,
        )
        .unwrap();
        assert_eq!(command_code.id, "local.command-code");
        // The old recipe pinned plan mode and two turns, so it could not finish
        // real work; the level decides the mode now, and turns are uncapped.
        assert!(!command_code.launch.argv.contains(&"plan".to_owned()));
        assert!(!command_code.launch.argv.contains(&"--max-turns".to_owned()));
        for (level, mode) in [
            (None, "yolo"),
            (Some(PermissionLevel::Edits), "accept-edits"),
            (Some(PermissionLevel::ReadOnly), "plan"),
        ] {
            assert_eq!(
                command_code.permission_arguments(level).unwrap(),
                ["--permission-mode", mode]
            );
        }
        assert!(
            command_code
                .launch
                .argv
                .contains(&"${input.prompt}".to_owned())
        );
        assert!(
            command_code
                .launch
                .env_allow
                .contains(&"COMMAND_CODE_API_KEY".to_owned())
        );
        assert_eq!(
            command_code.capabilities["model_select"].status,
            CapabilityStatus::Supported
        );
        assert_eq!(
            command_code.capabilities["effort_select"].status,
            CapabilityStatus::Unsupported
        );
        let command_code_with_effort = generate_manifest(
            "command-code",
            PathBuf::from("/bin/echo"),
            &format!("{command_code_help} --effort <level>"),
        )
        .unwrap();
        assert_eq!(
            command_code_with_effort.capabilities["effort_select"].status,
            CapabilityStatus::Supported
        );
        assert_eq!(
            command_code_with_effort.launch.effort_argv,
            vec!["--effort", "${route.effort}"]
        );
        assert!(
            generate_manifest(
                "command-code",
                PathBuf::from("/bin/echo"),
                "--print [query]"
            )
            .is_err()
        );
    }

    /// The Claude Code, Cline, and `opencode` recipes draft from their own flags,
    /// default to full permission, and refuse a level they have no way to honour.
    #[test]
    fn claude_cline_and_opencode_recipes_map_every_level_they_support() {
        let claude_help = "-p, --print --output-format <format> --model <model> --permission-mode <mode> --effort <level>";
        let claude = generate_manifest("claude", PathBuf::from("/bin/echo"), claude_help).unwrap();
        assert_eq!(claude.id, "local.claude-code");
        assert_eq!(claude.launch.argv.last().unwrap(), "${input.prompt}");
        assert_eq!(claude.launch.effort_argv, ["--effort", "${route.effort}"]);
        assert!(claude.launch.env_allow.contains(&"USER".to_owned()));
        // No model list, but the CLI validates names itself, so they pass through.
        assert_eq!(claude.launch.model_argv, ["--model", "${route.model}"]);
        assert_eq!(
            claude.probe.model_catalog.as_ref().unwrap().format,
            ModelCatalogFormat::CliValidated
        );
        for (level, mode) in [
            (None, "bypassPermissions"),
            (Some(PermissionLevel::Edits), "acceptEdits"),
            (Some(PermissionLevel::ReadOnly), "plan"),
        ] {
            assert_eq!(
                claude.permission_arguments(level).unwrap(),
                ["--permission-mode", mode]
            );
        }
        let older = generate_manifest(
            "claude",
            PathBuf::from("/bin/echo"),
            "-p, --print --output-format <format> --model <model> --permission-mode <mode>",
        )
        .unwrap();
        assert!(
            older.launch.effort_argv.is_empty(),
            "effort offered without --effort in help"
        );

        let cline_help =
            "-p, --plan --auto-approve <boolean> -m, --model <model-id> --thinking <level>";
        let cline = generate_manifest("cline", PathBuf::from("/bin/echo"), cline_help).unwrap();
        assert_eq!(cline.id, "local.cline");
        assert_eq!(
            cline.permission_arguments(None).unwrap(),
            ["--auto-approve", "true"]
        );
        assert_eq!(
            cline
                .permission_arguments(Some(PermissionLevel::ReadOnly))
                .unwrap(),
            ["--plan"]
        );
        // Cline has no edits-only mode, so edits is refused, never widened.
        assert!(
            cline
                .permission_arguments(Some(PermissionLevel::Edits))
                .is_err()
        );

        let opencode_help = "--model --variant --agent --auto";
        let opencode =
            generate_manifest("opencode", PathBuf::from("/bin/echo"), opencode_help).unwrap();
        assert_eq!(opencode.id, "local.opencode");
        // `run`'s flags are documented only by `opencode run --help`.
        assert_eq!(opencode.probe.help_argv, ["run", "--help"]);
        assert_eq!(
            opencode.probe.model_catalog.as_ref().unwrap().argv,
            ["models"]
        );
        assert_eq!(opencode.launch.argv, ["run", "${input.prompt}"]);
        assert_eq!(opencode.permission_arguments(None).unwrap(), ["--auto"]);
        assert_eq!(
            opencode
                .permission_arguments(Some(PermissionLevel::ReadOnly))
                .unwrap(),
            ["--agent", "plan"]
        );
        assert!(
            opencode
                .permission_arguments(Some(PermissionLevel::Edits))
                .is_err()
        );

        for (name, help) in [
            ("claude", "--print"),
            ("cline", "--plan"),
            ("opencode", "--model"),
        ] {
            assert!(
                generate_manifest(name, PathBuf::from("/bin/echo"), help).is_err(),
                "{name} drafted without its required flags"
            );
        }
    }

    #[test]
    fn devin_recipe_uses_print_mode_with_permission_levels() {
        let help = "--prompt-file <FILE> -p, --print [<PROMPT>] --permission-mode <PERMISSION_MODE> --respect-workspace-trust [<RESPECT_WORKSPACE_TRUST>] --model <MODEL>";
        let devin = generate_manifest("devin", PathBuf::from("/bin/echo"), help).unwrap();
        assert_eq!(devin.id, "local.devin");
        assert_eq!(
            devin.launch.argv,
            [
                "--respect-workspace-trust",
                "false",
                "--prompt-file",
                "${input.prompt_file}",
                "-p",
            ]
        );
        for (level, mode) in [
            (None, "dangerous"),
            (Some(PermissionLevel::Edits), "accept-edits"),
            (Some(PermissionLevel::ReadOnly), "auto"),
        ] {
            assert_eq!(
                devin.permission_arguments(level).unwrap(),
                ["--permission-mode", mode]
            );
        }
        assert!(devin.launch.model_argv.is_empty());
        assert!(devin.launch.effort_argv.is_empty());
        assert!(devin.probe.model_catalog.is_none());
        assert_eq!(
            devin.capabilities["model_select"].status,
            CapabilityStatus::Unsupported
        );
        assert_eq!(
            devin.capabilities["effort_select"].status,
            CapabilityStatus::Unsupported
        );
        assert!(
            generate_manifest("devin", PathBuf::from("/bin/echo"), "--prompt-file <FILE>").is_err()
        );
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
        assert!(matches!(
            registry
                .activate_with_scratch(&draft, root.path(), "fixture request", None, None)
                .await,
            Err(RegistryError::ScratchOverlapsControlHome)
        ));
        assert!(!registry.activation_path(&draft.id).exists());
        registry.contract_test(&draft).await.unwrap();
        let receipt = registry
            .activate_with_scratch(
                &draft,
                &scratch_workspace(&root),
                "fixture request",
                None,
                None,
            )
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
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
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
    async fn agent_authored_positional_recipe_activates_without_core_changes() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("novel-agent");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$1\" in\n --version) echo 'novel 1';;\n --help) echo '  -p, --print prompt';;\n -p) printf '%s' \"$2\";;\n *) exit 2;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = HarnessManifest {
            schema: MANIFEST_SCHEMA_V1.to_owned(),
            id: "local.novel-agent".to_owned(),
            adapter: PROCESS_ADAPTER_V1.to_owned(),
            executable: executable.canonicalize().unwrap(),
            probe: ProbeSpec {
                version_argv: vec!["--version".to_owned()],
                help_argv: vec!["--help".to_owned()],
                model_catalog: None,
            },
            launch: LaunchSpec {
                argv: vec!["-p".to_owned(), "${input.prompt}".to_owned()],
                model_argv: vec![],
                effort_argv: vec![],
                env_allow: vec!["HOME".to_owned(), "PATH".to_owned()],
                mode: ExecutionMode::OneShot,
                permission_argv: brgr_runner::PermissionArgv::default(),
                interactive: None,
            },
            result: ResultSpec {
                source: ResultSource::Stdout,
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
                success_exit_codes: vec![0],
            },
            capabilities: BTreeMap::from([
                ("completion".to_owned(), supported("process_exit")),
                ("model_select".to_owned(), unsupported("not_observed")),
            ]),
        };
        let registry = Registry::open(root.path().join("registry")).unwrap();
        registry.contract_test_custom(&manifest).await.unwrap();
        let receipt = registry
            .activate_custom_with_scratch(&manifest, &scratch_workspace(&root), "first", None, None)
            .await
            .unwrap();
        assert!(receipt.scratch_result_digest.is_some());
        assert_eq!(receipt.registration_mode, "agent_authored");
        assert_eq!(receipt.contract_suite, "process-contract/v1");
        assert_eq!(registry.health(&manifest.id).unwrap(), Health::Healthy);
        let loaded = registry.load_healthy(&manifest.id).unwrap();
        let output = ProcessRunner::run(
            &loaded,
            brgr_runner::RunRequest {
                workspace: root.path(),
                prompt: "second",
                criteria: None,
                instructions: None,
                model: None,
                effort: None,
                permission: None,
                deadline: Duration::from_secs(2),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(output.result, b"second");
        assert_custom_manifest_rejections(&registry, &root, &manifest, &executable).await;
    }

    async fn assert_custom_manifest_rejections(
        registry: &Registry,
        root: &tempfile::TempDir,
        manifest: &HarnessManifest,
        executable: &Path,
    ) {
        let other_executable = root.path().join("other-agent");
        fs::copy(executable, &other_executable).unwrap();
        let mut collision = manifest.clone();
        collision.executable = other_executable.canonicalize().unwrap();
        assert!(matches!(
            registry
                .activate_custom_with_scratch(
                    &collision,
                    &scratch_workspace(root),
                    "third",
                    None,
                    None
                )
                .await,
            Err(RegistryError::HarnessAliasCollision(_))
        ));
        let mut altered = manifest.clone();
        altered.launch.argv[0] = "--not-documented".to_owned();
        assert!(matches!(
            registry.contract_test_custom(&altered).await,
            Err(RegistryError::UndocumentedCustomFlag(_))
        ));
        altered = manifest.clone();
        altered
            .launch
            .env_allow
            .push("COMMAND_CODE_API_KEY".to_owned());
        assert!(matches!(
            registry.contract_test_custom(&altered).await,
            Err(RegistryError::CustomEnvironmentDenied)
        ));
        altered = manifest.clone();
        altered.launch.mode = ExecutionMode::DelegatedExternal;
        assert!(matches!(
            registry.contract_test_custom(&altered).await,
            Err(RegistryError::UnsupportedCustomMode)
        ));
        altered = manifest.clone();
        altered.launch.argv.insert(0, "--yolo".to_owned());
        assert!(matches!(
            registry.contract_test_custom(&altered).await,
            Err(RegistryError::UnsafeCustomPermission(_))
        ));
    }

    #[tokio::test]
    async fn altered_manifest_and_executable_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        let activation = registry
            .activate_with_scratch(&draft, &scratch_workspace(&root), "fixture", None, None)
            .await
            .unwrap();
        let pinned = registry.load_healthy(&draft.id).unwrap();
        Registry::verify_pinned_executable(&pinned, &activation.executable_digest).unwrap();
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
        assert!(matches!(
            Registry::verify_pinned_executable(&pinned, &activation.executable_digest),
            Err(RegistryError::ExecutableDrift { .. })
        ));
    }

    #[tokio::test]
    async fn old_process_activation_without_scratch_cannot_run() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mystery-agent");
        fixture_executable(&executable);
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        let mut receipt = registry
            .activate_with_scratch(&draft, &scratch_workspace(&root), "fixture", None, None)
            .await
            .unwrap();
        receipt.scratch_result_digest = None;
        fs::write(
            registry.activation_path(&draft.id),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            registry.load_healthy(&draft.id),
            Err(RegistryError::ScratchCertificationMissing(_))
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
                .activate_with_scratch(&draft, &scratch_workspace(&root), "fixture", None, None)
                .await,
            Err(RegistryError::ManifestNotObserved)
        ));
        assert!(!registry.activation_path(&draft.id).exists());
    }

    #[tokio::test]
    async fn omp_role_symlink_keeps_legacy_adapter_name() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("launch_tui.py");
        fs::write(&target, "#!/bin/sh\necho '--expected-report --reuse-worktree-objective --reuse-worktree-owner --model --effort'\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.path().join("omp-role");
        symlink(&target, &alias).unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let receipt = registry.add(&alias).await.unwrap();
        assert_eq!(receipt.harness_id, "local.omp-herdr");
        assert_eq!(receipt.contract_suite, "presentation-probe/v1");
        assert!(receipt.scratch_result_digest.is_none());
        assert_eq!(
            registry.load_healthy("local.omp-herdr").unwrap().adapter,
            OMP_ROLE_ADAPTER_V1
        );
        assert!(
            registry
                .load_healthy("local.omp-herdr")
                .unwrap()
                .probe
                .model_catalog
                .is_some()
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
            .activate_with_scratch(&draft, &scratch_workspace(&root), "test", None, None)
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
            Health::EvidenceChanged { .. }
        ));
    }

    #[tokio::test]
    async fn health_probed_classifies_local_drift() {
        let root = tempfile::tempdir().unwrap();
        let sidecar = root.path().join("sidecar");
        let executable = root.path().join("fixture-agent");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ -f '{0}' ]; then\n  case \"$(/bin/cat '{0}')\" in\n    stall) /bin/sleep 6;;\n    sleep) /bin/sleep 30;;\n    fail) exit 3;;\n  esac\nfi\ncase \"$1\" in\n  --version) echo 1.0;;\n  --help) echo '  --prompt-file <path>';;\n  --prompt-file) /bin/cat \"$2\";;\nesac\n",
                sidecar.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = Registry::open(root.path().join("registry")).unwrap();
        let draft = registry.draft(&executable).await.unwrap();
        registry
            .activate_with_scratch(&draft, &scratch_workspace(&root), "test", None, None)
            .await
            .unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Healthy
        );

        let original = fs::read(&executable).unwrap();
        let alias = root.path().join("gjc-link");
        symlink(&executable, &alias).unwrap();
        let mut rebuilt = original.clone();
        rebuilt.extend_from_slice(b"# rebuilt\n");
        fs::write(&executable, &rebuilt).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::ExecutableChanged { .. }
        ));

        fs::write(&executable, &original).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Healthy
        );

        // A slow probe is not a hung one: a multi-second stall, which a busy
        // machine produces, must not fail a working harness.
        fs::write(&sidecar, "stall").unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Healthy
        );
        fs::write(&sidecar, "sleep").unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::TimedOut
        );
        fs::write(&sidecar, "fail").unwrap();
        assert!(matches!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::ExitedNonzero { exit_code: Some(3) }
        ));
        fs::remove_file(&sidecar).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Unspawnable
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(&executable).unwrap();
        assert_eq!(
            registry.health_probed(&draft.id).await.unwrap(),
            Health::Unspawnable
        );
        let manifest_path = registry.manifest_path(&draft.id);
        let original_manifest = fs::read(&manifest_path).unwrap();
        let mut changed_manifest = original_manifest.clone();
        changed_manifest.push(b' ');
        fs::write(&manifest_path, changed_manifest).unwrap();
        assert!(matches!(
            registry.health_probed(&draft.id).await,
            Err(RegistryError::ManifestDrift { .. })
        ));
        fs::write(&manifest_path, original_manifest).unwrap();
        let action = registry.recertify_action_for(&draft.id).unwrap();
        assert!(action.contains("re-certify"));
        assert!(action.contains("--prompt"));
        assert!(!action.contains("--help"));
    }

    #[test]
    fn recertify_action_quotes_executable_paths_with_spaces() {
        let help =
            "--expected-report --reuse-worktree-objective --reuse-worktree-owner --model --effort";
        let presentation =
            generate_manifest("omp-role", PathBuf::from("/tmp/my harness/omp-role"), help).unwrap();
        let action = recertify_action(&presentation);
        assert!(action.contains("'/tmp/my harness/omp-role'"));
        assert!(action.contains("--presentation-only"));
        assert!(!action.contains("unused-help"));
        assert!(!action.contains("--expected-report"));

        let process = generate_manifest(
            "mystery-agent",
            PathBuf::from("/opt/my tools/agent"),
            "  --prompt-file <path>\n",
        )
        .unwrap();
        let action = recertify_action(&process);
        assert!(action.contains("'/opt/my tools/agent'"));
        assert!(action.contains("--prompt"));
        assert!(!action.contains("--prompt-file <path>"));
    }
}
