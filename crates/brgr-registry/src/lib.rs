//! Evidence-backed harness registration and health checking.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as FmtWrite,
    fs,
    io::Write as IoWrite,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use brgr_runner::{
    Capability, CapabilityStatus, ExecutionMode, HarnessManifest, LaunchSpec, MANIFEST_SCHEMA_V1,
    ModelCatalogFormat, ModelCatalogSpec, OMP_ROLE_ADAPTER_V1, PROCESS_ADAPTER_V1, ProbeSpec,
    ProcessRunner, ResultSource, ResultSpec, RunnerError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const PROBE_DEADLINE: Duration = Duration::from_secs(5);

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
                model,
                effort,
                deadline: Duration::from_mins(1),
                cancel_path: None,
                pid_path: None,
            },
        )
        .await?;
        if !output.succeeded(manifest) || output.result.is_empty() {
            return Err(RegistryError::ScratchRunFailed);
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
            Health::Drifted { expected, observed } => {
                Err(RegistryError::ExecutableDrift { expected, observed })
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

    /// Resolves an exact requested model through a bounded native catalog
    /// before task admission or a paid scratch run.
    ///
    /// # Errors
    ///
    /// Fails closed when no catalog exists, probing fails, or the exact
    /// selector is absent. The Herdr presentation adapter uses its own OMP
    /// launcher preflight instead of a process manifest catalog.
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
        if manifest.adapter == OMP_ROLE_ADAPTER_V1 {
            return Ok(());
        }
        let catalog = manifest
            .probe
            .model_catalog
            .as_ref()
            .ok_or_else(|| RegistryError::ModelCatalogMissing(manifest.id.clone()))?;
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
        let output = ProcessRunner::probe(&manifest.executable, &argv, PROBE_DEADLINE).await?;
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
            contract_suite: "process-contract/v1".to_owned(),
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
    let help_text = String::from_utf8_lossy(&help.stdout);
    validate_custom_argv(manifest, &help_text)?;
    Ok(ProbeEvidence {
        executable: digest_file(&manifest.executable)?,
        version: digest_bytes(&version.stdout),
        help: digest_bytes(&help.stdout),
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
        "omp" => generate_omp_process_manifest(executable, help),
        "omp-role" => generate_omp_manifest(executable, help),
        "cursor" | "cursor-agent" | "cursor-cli" => generate_cursor_manifest(executable, help),
        "command-code" | "commandcode" | "cmdc" => generate_command_code_manifest(executable, help),
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
    if matches!(id.as_str(), "local.gjc" | "local.omp" | "local.omp-herdr") {
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
            model_catalog: None,
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
            model_catalog: Some(ModelCatalogSpec {
                argv: vec!["--list-models=${model.id}".to_owned()],
                format: ModelCatalogFormat::CanonicalProviderTable,
            }),
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

fn generate_omp_process_manifest(
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if ![
        "-p, --print",
        "--mode=<value>",
        "--no-session",
        "--no-prewalk",
        "--no-extensions",
        "--no-title",
        "--model=<value>",
        "--thinking=<value>",
    ]
    .iter()
    .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "local.omp".to_owned(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: Some(ModelCatalogSpec {
                argv: vec![
                    "models".to_owned(),
                    "find".to_owned(),
                    "${model.query}".to_owned(),
                    "--json".to_owned(),
                ],
                format: ModelCatalogFormat::JsonSelectors {
                    pointer: "/models".to_owned(),
                    field: "selector".to_owned(),
                },
            }),
        },
        launch: LaunchSpec {
            argv: vec![
                "-p".to_owned(),
                "--mode=json".to_owned(),
                "--no-session".to_owned(),
                "--no-prewalk".to_owned(),
                "--no-extensions".to_owned(),
                "--no-title".to_owned(),
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

fn generate_cursor_manifest(
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if ![
        "--print",
        "--mode <mode>",
        "--output-format <format>",
        "--model <model>",
    ]
    .iter()
    .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "local.cursor-cli".to_owned(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: Some(ModelCatalogSpec {
                argv: vec!["models".to_owned()],
                format: ModelCatalogFormat::DashSeparated,
            }),
        },
        launch: LaunchSpec {
            argv: vec![
                "--print".to_owned(),
                "--mode".to_owned(),
                "ask".to_owned(),
                "--output-format".to_owned(),
                "text".to_owned(),
                "--trust".to_owned(),
                "--workspace".to_owned(),
                "${task.workspace}".to_owned(),
                "${input.prompt}".to_owned(),
            ],
            model_argv: vec!["--model".to_owned(), "${route.model}".to_owned()],
            effort_argv: vec![],
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
            ("model_select".to_owned(), supported("--model")),
            ("effort_select".to_owned(), unsupported("not_observed")),
        ]),
    })
}

fn generate_command_code_manifest(
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if ![
        "--print [query]",
        "--permission-mode <mode>",
        "--no-session",
        "--no-skills",
        "--skip-onboarding",
        "--no-auto-update",
        "--max-turns <number>",
        "--model <model>",
    ]
    .iter()
    .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "local.command-code".to_owned(),
        adapter: PROCESS_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--version".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: Some(ModelCatalogSpec {
                argv: vec!["--list-models".to_owned()],
                format: ModelCatalogFormat::FirstColumn,
            }),
        },
        launch: LaunchSpec {
            argv: vec![
                "--no-session".to_owned(),
                "--no-skills".to_owned(),
                "--skip-onboarding".to_owned(),
                "--no-auto-update".to_owned(),
                "--max-turns".to_owned(),
                "2".to_owned(),
                "--permission-mode".to_owned(),
                "plan".to_owned(),
                "--print".to_owned(),
                "${input.prompt}".to_owned(),
            ],
            model_argv: vec!["--model".to_owned(), "${route.model}".to_owned()],
            effort_argv: vec![],
            env_allow: vec![
                "HOME".to_owned(),
                "PATH".to_owned(),
                "LANG".to_owned(),
                "TMPDIR".to_owned(),
                "COMMAND_CODE_API_KEY".to_owned(),
            ],
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
            ("model_select".to_owned(), supported("--model")),
            ("effort_select".to_owned(), unsupported("not_observed")),
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
        "--model",
        "--effort",
    ]
    .iter()
    .all(|flag| help.contains(flag))
    {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    Ok(HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id: "local.omp-herdr".to_owned(),
        adapter: OMP_ROLE_ADAPTER_V1.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec!["--help".to_owned()],
            help_argv: vec!["--help".to_owned()],
            model_catalog: None,
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
            ("model_select".to_owned(), supported("omp-role --model")),
            ("effort_select".to_owned(), supported("omp-role --effort")),
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

fn parse_model_catalog(
    format: &ModelCatalogFormat,
    bytes: &[u8],
) -> Result<BTreeSet<String>, RegistryError> {
    let mut selectors = BTreeSet::new();
    match format {
        ModelCatalogFormat::JsonSelectors { pointer, field } => {
            let document: serde_json::Value = serde_json::from_slice(bytes)?;
            let items = document
                .pointer(pointer)
                .and_then(serde_json::Value::as_array)
                .ok_or(RegistryError::InvalidModelCatalog)?;
            for item in items {
                let selector = item
                    .as_str()
                    .or_else(|| item.get(field).and_then(serde_json::Value::as_str))
                    .filter(|value| !value.is_empty())
                    .ok_or(RegistryError::InvalidModelCatalog)?;
                selectors.insert(selector.to_owned());
            }
        }
        ModelCatalogFormat::CanonicalProviderTable => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| RegistryError::InvalidModelCatalog)?;
            let mut section = 0_u8;
            for line in text.lines().map(str::trim) {
                match line {
                    "Canonical models" => section = 1,
                    "Provider models" => section = 2,
                    _ => {
                        let columns = line.split_whitespace().collect::<Vec<_>>();
                        if columns.len() >= 2 && section == 1 && columns[0] != "canonical" {
                            selectors.insert(columns[0].to_owned());
                            selectors.insert(columns[1].to_owned());
                        } else if columns.len() >= 2 && section == 2 && columns[0] != "provider" {
                            selectors.insert(format!("{}/{}", columns[0], columns[1]));
                        }
                    }
                }
            }
        }
        ModelCatalogFormat::DashSeparated => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| RegistryError::InvalidModelCatalog)?;
            for line in text.lines() {
                if let Some((selector, _)) = line.split_once(" - ") {
                    let selector = selector.trim();
                    if selector != "auto" && !selector.is_empty() {
                        selectors.insert(selector.to_owned());
                    }
                }
            }
        }
        ModelCatalogFormat::FirstColumn => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| RegistryError::InvalidModelCatalog)?;
            for line in text.lines() {
                if let Some(selector) = line.split_whitespace().next()
                    && selector
                        .bytes()
                        .any(|byte| byte == b'/' || byte == b'-' || byte.is_ascii_digit())
                {
                    selectors.insert(selector.to_owned());
                }
            }
        }
    }
    if selectors.is_empty() && !matches!(format, ModelCatalogFormat::JsonSelectors { .. }) {
        return Err(RegistryError::InvalidModelCatalog);
    }
    Ok(selectors)
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
    if receipt.schema != "brgr.activation/v1"
        || receipt.harness_id != harness_id
        || manifest.id != harness_id
        || manifest.executable != receipt.executable_realpath
        || manifest.executable.canonicalize()? != receipt.executable_realpath
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
    #[error("custom manifest executable must be its canonical real path")]
    ExecutableMustBeCanonical,
    #[error("custom manifests cannot shadow a built-in harness or adapter")]
    ReservedCustomHarness,
    #[error("custom manifest probes must be exactly --version and --help")]
    UnsafeCustomProbe,
    #[error("custom manifests support only the one-shot execution mode")]
    UnsupportedCustomMode,
    #[error("custom manifest requested a privileged environment variable")]
    CustomEnvironmentDenied,
    #[error("custom manifest capability and argv disagree: {0}")]
    CustomCapabilityMismatch(String),
    #[error("custom manifest uses an unsupported placeholder: {0}")]
    UnsafeCustomPlaceholder(String),
    #[error("custom manifest flag was not found in bounded help: {0}")]
    UndocumentedCustomFlag(String),
    #[error("custom manifest cannot silently expand execution permissions: {0}")]
    UnsafeCustomPermission(String),
    #[error("custom manifest id is already activated for another executable: {0}")]
    HarnessAliasCollision(String),
    #[error("the installed harness is not supported by a verified recipe")]
    UnsupportedHarness,
    #[error("help does not document a supported bounded fresh-run flag contract")]
    RequiredFlagsMissing,
    #[error("harness id contains unsafe path characters: {0}")]
    InvalidHarnessId(String),
    #[error("harness {0} needs an explicitly authorized scratch run before activation")]
    ScratchRunRequired(String),
    #[error(
        "process harness {0} has no scratch certification; re-add it with --workspace and --prompt"
    )]
    ScratchCertificationMissing(String),
    #[error("scratch prompt must be nonempty")]
    EmptyScratchPrompt,
    #[error("scratch workspace overlaps the brgr control directory")]
    ScratchOverlapsControlHome,
    #[error("requested model is not an exact selector: {0}")]
    ModelNotExact(String),
    #[error("harness {0} has no model catalog; re-certify it before selecting a model")]
    ModelCatalogMissing(String),
    #[error("model catalog recipe or output is malformed")]
    InvalidModelCatalog,
    #[error("bounded model catalog probe failed")]
    ModelCatalogProbeFailed,
    #[error("requested model is absent from the current catalog: {0}")]
    ModelNotInCatalog(String),
    #[error("registry root is outside its declared control home")]
    InvalidControlHome,
    #[error("this harness cannot select a model for its scratch run")]
    UnsupportedScratchModel,
    #[error("this harness cannot select effort for its scratch run")]
    UnsupportedScratchEffort,
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

    fn scratch_workspace(root: &tempfile::TempDir) -> PathBuf {
        let workspace = root.path().join("scratch");
        fs::create_dir_all(&workspace).unwrap();
        workspace
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
        let command = b"Available models\nOpen Source\ndeepseek/deepseek-v4-flash   fast\ngpt-5.6-luna   low cost\n";
        let parsed = parse_model_catalog(&ModelCatalogFormat::FirstColumn, command).unwrap();
        assert!(parsed.contains("deepseek/deepseek-v4-flash"));
        assert!(parsed.contains("gpt-5.6-luna"));
        assert!(!parsed.contains("Available"));
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
    fn named_cursor_and_command_code_recipes_use_observed_read_only_flags() {
        let cursor_help = "--print --mode <mode> --output-format <format> --model <model>";
        let cursor =
            generate_manifest("cursor-agent", PathBuf::from("/bin/echo"), cursor_help).unwrap();
        assert_eq!(cursor.id, "local.cursor-cli");
        assert!(cursor.launch.argv.contains(&"ask".to_owned()));
        assert!(cursor.launch.argv.contains(&"${input.prompt}".to_owned()));
        assert_eq!(
            cursor.capabilities["model_select"].status,
            CapabilityStatus::Supported
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
        assert!(command_code.launch.argv.contains(&"plan".to_owned()));
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
        assert!(
            generate_manifest(
                "command-code",
                PathBuf::from("/bin/echo"),
                "--print [query]"
            )
            .is_err()
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
                model: None,
                effort: None,
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
        assert_eq!(
            registry.load_healthy("local.omp-herdr").unwrap().adapter,
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
            Health::Drifted { .. }
        ));
    }
}
