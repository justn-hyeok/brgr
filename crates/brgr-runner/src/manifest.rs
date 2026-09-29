//! The declarative harness manifest (`brgr.harness/v1`) and its validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use brgr_protocol::{PermissionLevel, TaskSpec};
use serde::{Deserialize, Serialize};

use crate::{MANIFEST_SCHEMA_V1, OMP_ROLE_ADAPTER_V1, PROCESS_ADAPTER_V1, RunnerError};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManifest {
    pub schema: String,
    pub id: String,
    pub adapter: String,
    pub executable: PathBuf,
    pub probe: ProbeSpec,
    pub launch: LaunchSpec,
    pub result: ResultSpec,
    #[serde(default)]
    pub capabilities: BTreeMap<String, Capability>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSpec {
    pub version_argv: Vec<String>,
    pub help_argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog: Option<ModelCatalogSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogSpec {
    pub argv: Vec<String>,
    pub format: ModelCatalogFormat,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelCatalogFormat {
    JsonSelectors {
        pointer: String,
        field: String,
    },
    CanonicalProviderTable,
    DashSeparated,
    FirstColumn,
    /// One `provider/model` selector per line and nothing else, as
    /// `opencode models` prints.
    Lines,
    /// No list to read: the CLI refuses a model name it does not know, locally
    /// and before any paid request, so brgr passes the name through. Only for a
    /// CLI where that refusal was observed; its `argv` is empty.
    CliValidated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub model_argv: Vec<String>,
    #[serde(default)]
    pub effort_argv: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    pub mode: ExecutionMode,
    /// Arguments for each permission level the harness can honour. Empty for
    /// a manifest that predates levels or a custom one that declares none.
    #[serde(default, skip_serializing_if = "PermissionArgv::is_empty")]
    pub permission_argv: PermissionArgv,
    /// How to run this harness as its own interactive TUI in a Herdr pane, so
    /// the work is visible. Absent for a harness Herdr does not recognize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive: Option<InteractiveSpec>,
}

/// An interactive launch through `herdr agent start`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractiveSpec {
    /// The agent kind Herdr recognizes, e.g. `claude`.
    pub herdr_kind: String,
    /// Arguments before the permission and model arguments, which are the
    /// same ones the print-mode launch uses.
    #[serde(default)]
    pub argv: Vec<String>,
}

/// Per-level arguments. `Some(vec![])` is a level the harness honours with no
/// flag; `None` is a level it cannot honour, which is refused rather than run
/// under a wider one.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionArgv {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<Vec<String>>,
}

impl PermissionArgv {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.full.is_none() && self.edits.is_none() && self.read_only.is_none()
    }

    #[must_use]
    pub fn for_level(&self, level: PermissionLevel) -> Option<&[String]> {
        match level {
            PermissionLevel::Full => self.full.as_deref(),
            PermissionLevel::Edits => self.edits.as_deref(),
            PermissionLevel::ReadOnly => self.read_only.as_deref(),
        }
    }

    fn all(&self) -> impl Iterator<Item = &String> {
        [&self.full, &self.edits, &self.read_only]
            .into_iter()
            .flatten()
            .flatten()
    }
}

impl HarnessManifest {
    /// The arguments that make this harness run at `requested`.
    ///
    /// No request keeps the harness's own default: the full level where the
    /// recipe declares levels, and plain `argv` for a manifest without them —
    /// which is how every task before levels existed ran. An explicit request
    /// the harness cannot honour is an error, never a wider level.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError::UnsupportedPermission`] for a level the manifest
    /// does not declare.
    pub fn permission_arguments(
        &self,
        requested: Option<PermissionLevel>,
    ) -> Result<&[String], RunnerError> {
        let table = &self.launch.permission_argv;
        match requested {
            None if table.is_empty() => Ok(&[]),
            level => {
                let level = level.unwrap_or(PermissionLevel::Full);
                table
                    .for_level(level)
                    .ok_or(RunnerError::UnsupportedPermission(level))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    OneShot,
    /// The wrapper may leave a separately managed worker alive after it exits.
    DelegatedExternal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSpec {
    pub source: ResultSource,
    pub media_type: String,
    pub max_bytes: u64,
    pub success_exit_codes: Vec<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    try_from = "ResultSourceFields"
)]
pub enum ResultSource {
    Stdout,
    JsonlAssistantFinal,
    File { path: String },
}

/// Every key a result source may carry, checked before the variant is chosen.
///
/// `deny_unknown_fields` on an internally tagged enum does not reach its unit
/// variants: `{"kind":"stdout","path":"report.md"}` parsed as `Stdout` with the
/// path silently dropped, so a manifest whose `kind` was wrong but whose `path`
/// was set sealed raw stdout and skipped every file-result guard. Deserializing
/// through this struct rejects an unknown key and a `path` on a source that has
/// none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultSourceFields {
    kind: String,
    path: Option<String>,
}

impl TryFrom<ResultSourceFields> for ResultSource {
    type Error = String;

    fn try_from(fields: ResultSourceFields) -> Result<Self, Self::Error> {
        match (fields.kind.as_str(), fields.path) {
            ("stdout", None) => Ok(Self::Stdout),
            ("jsonl_assistant_final", None) => Ok(Self::JsonlAssistantFinal),
            ("file", Some(path)) => Ok(Self::File { path }),
            ("file", None) => Err("a `file` result source requires `path`".to_owned()),
            ("stdout" | "jsonl_assistant_final", Some(_)) => Err(format!(
                "a `{}` result source takes no `path`; did you mean `file`?",
                fields.kind
            )),
            (other, _) => Err(format!("unknown result source kind `{other}`")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub status: CapabilityStatus,
    pub semantics: String,
    pub evidence_ref: Option<String>,
    pub tested_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Unknown,
}

impl HarnessManifest {
    /// Rejects an unsupported route before any task workspace or process is created.
    ///
    /// # Errors
    ///
    /// Returns an explicit capability error for an unclaimed model, effort, or
    /// required operation. OMP's internal process wrapper preserves the
    /// activated OMP capabilities but has its own executable identity.
    pub fn validate_task_route(&self, task: &TaskSpec) -> Result<(), RunnerError> {
        // Refused at admission rather than after a paid launch.
        self.permission_arguments(task.permission)?;
        self.validate()?;
        if self.id != task.route.harness_id
            && !(self.id == "internal.omp-runner"
                && matches!(
                    task.route.harness_id.as_str(),
                    "local.omp" | "local.omp-herdr"
                ))
        {
            return Err(RunnerError::HarnessRouteMismatch {
                requested: task.route.harness_id.clone(),
                actual: self.id.clone(),
            });
        }
        for name in &task.required_capabilities {
            self.require_capability(name)?;
        }
        if task.route.requested_model.is_some() {
            self.require_capability("model_select")?;
            if self.adapter != OMP_ROLE_ADAPTER_V1
                && self.launch.model_argv.is_empty()
                && !self
                    .launch
                    .argv
                    .iter()
                    .any(|arg| arg.contains("${route.model}"))
            {
                return Err(RunnerError::UnsupportedCapability(
                    "model_select".to_owned(),
                ));
            }
        }
        if task.route.requested_effort.is_some() {
            self.require_capability("effort_select")?;
            if self.adapter != OMP_ROLE_ADAPTER_V1
                && self.launch.effort_argv.is_empty()
                && !self
                    .launch
                    .argv
                    .iter()
                    .any(|arg| arg.contains("${route.effort}"))
            {
                return Err(RunnerError::UnsupportedCapability(
                    "effort_select".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn require_capability(&self, name: &str) -> Result<(), RunnerError> {
        if self
            .capabilities
            .get(name)
            .is_some_and(|capability| capability.status == CapabilityStatus::Supported)
        {
            Ok(())
        } else {
            Err(RunnerError::UnsupportedCapability(name.to_owned()))
        }
    }

    /// Validates the non-programmable v1 manifest contract.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] for unsupported schemas, relative executables,
    /// unsafe arguments, invalid limits, or duplicate environment names.
    pub fn validate(&self) -> Result<(), RunnerError> {
        if self.schema != MANIFEST_SCHEMA_V1 {
            return Err(RunnerError::UnsupportedSchema(self.schema.clone()));
        }
        if !matches!(
            self.adapter.as_str(),
            PROCESS_ADAPTER_V1 | OMP_ROLE_ADAPTER_V1
        ) {
            return Err(RunnerError::UnsupportedAdapter(self.adapter.clone()));
        }
        if self.id.trim().is_empty() || !self.id.contains('.') {
            return Err(RunnerError::InvalidHarnessId(self.id.clone()));
        }
        if !self.executable.is_absolute() || !self.executable.is_file() {
            return Err(RunnerError::InvalidExecutable(self.executable.clone()));
        }
        if self
            .launch
            .argv
            .iter()
            .chain(self.launch.permission_argv.all())
            .chain(
                self.launch
                    .interactive
                    .iter()
                    .flat_map(|spec| spec.argv.iter()),
            )
            .any(|value| value.contains('\0'))
            || self.launch.interactive.as_ref().is_some_and(|spec| {
                spec.herdr_kind.is_empty()
                    || !spec.herdr_kind.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err(RunnerError::InvalidArgument);
        }
        if let Some(catalog) = &self.probe.model_catalog
            && (catalog.argv.is_empty()
                != matches!(catalog.format, ModelCatalogFormat::CliValidated)
                || catalog.argv.len() > 64
                || catalog.argv.iter().any(|arg| {
                    arg.contains('\0')
                        || arg
                            .replace("${model.query}", "")
                            .replace("${model.id}", "")
                            .contains("${")
                })
                || matches!(
                    &catalog.format,
                    ModelCatalogFormat::JsonSelectors { pointer, field }
                        if !pointer.starts_with('/') || field.is_empty()
                ))
        {
            return Err(RunnerError::InvalidModelCatalog);
        }
        if self.result.max_bytes == 0 || self.result.max_bytes > 20 * 1024 * 1024 {
            return Err(RunnerError::InvalidOutputLimit);
        }
        if self.result.success_exit_codes.is_empty() {
            return Err(RunnerError::MissingSuccessExitCode);
        }
        let mut names = BTreeSet::new();
        for name in &self.launch.env_allow {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                || !names.insert(name)
            {
                return Err(RunnerError::InvalidEnvironmentName(name.clone()));
            }
        }
        Ok(())
    }
}
