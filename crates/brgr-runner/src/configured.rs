//! The model a configured-default-only harness will use.
//!
//! Such a harness takes no model per run, so a run records no model of its
//! own. When its settings file is a documented, stable place to read, the
//! model it names is recorded instead, labelled as configured rather than
//! observed in the run. The file is read, never written.

use std::{fs::File, io::Read, path::Path};

use serde_json::Value;

const MAX_BYTES: u64 = 1 << 20;
/// Longest model id kept; real ids are far shorter.
pub const MAX_MODEL_CHARS: usize = 200;

/// The model named by the harness's own settings, when it is one brgr knows
/// how to read. `None` is "unknown", never a guess.
#[must_use]
pub fn configured_model(harness_id: &str) -> Option<String> {
    match harness_id {
        "local.cline" => {
            let home = std::env::var_os("HOME")?;
            cline_model(&Path::new(&home).join(".cline/data/settings/providers.json"))
        }
        _ => None,
    }
}

/// `providers.json`: `lastUsedProvider` names the provider whose
/// `settings.model` the next run uses.
fn cline_model(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_BYTES {
        return None;
    }
    let settings: Value = serde_json::from_slice(&bytes).ok()?;
    let provider = settings.get("lastUsedProvider")?.as_str()?;
    let model = settings
        .pointer(&format!(
            "/providers/{}/settings/model",
            provider.replace('~', "~0").replace('/', "~1")
        ))?
        .as_str()?
        .trim();
    if model.is_empty() || model.chars().any(char::is_control) {
        return None;
    }
    Some(model.chars().take(MAX_MODEL_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("providers.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn cline_names_the_model_of_its_last_used_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            r#"{"lastUsedProvider":"cline","providers":{
                "cline":{"settings":{"model":"cline-free/muse-spark-1.3-contributor","auth":"secret"}},
                "other":{"settings":{"model":"not-this-one"}}}}"#,
        );
        assert_eq!(
            cline_model(&path).as_deref(),
            Some("cline-free/muse-spark-1.3-contributor")
        );
    }

    #[test]
    fn an_unreadable_or_odd_settings_file_is_unknown_not_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(cline_model(&dir.path().join("missing.json")), None);
        for body in [
            "not json",
            r#"{"providers":{"cline":{"settings":{"model":"m"}}}}"#,
            r#"{"lastUsedProvider":"cline","providers":{"cline":{"settings":{"model":""}}}}"#,
            r#"{"lastUsedProvider":"cline","providers":{"cline":{"settings":{"model":"a\nb"}}}}"#,
            r#"{"lastUsedProvider":"cline","providers":{"cline":{"settings":{"model":7}}}}"#,
        ] {
            assert_eq!(cline_model(&write(&dir, body)), None, "{body}");
        }
    }

    #[test]
    fn only_known_harnesses_are_read() {
        assert_eq!(configured_model("local.gjc"), None);
    }
}
