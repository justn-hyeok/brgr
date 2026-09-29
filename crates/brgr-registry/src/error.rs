//! Typed registration and health-check failures.

use brgr_runner::RunnerError;
use thiserror::Error;

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
    #[error("model catalog executable is unavailable: {0}")]
    ModelCatalogExecutableMissing(String),
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
    #[error("scratch run did not produce a successful nonempty result: {0}")]
    ScratchRunFailed(String),
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
    #[error("activation executable cannot be spawned; re-certify it with brgr harness add")]
    ExecutableUnspawnable,
    #[error("registry path has no parent")]
    MissingParent,
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
