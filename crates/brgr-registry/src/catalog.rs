//! Locating a harness's model-list command and parsing what it prints.

use std::{collections::BTreeSet, os::unix::fs::PermissionsExt as _, path::PathBuf};

use brgr_runner::ModelCatalogFormat;

use crate::RegistryError;

pub(crate) fn executable_on_path(name: &str) -> Result<PathBuf, RegistryError> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| RegistryError::ModelCatalogExecutableMissing(name.to_owned()))?;
    for directory in std::env::split_paths(&path).filter(|part| part.is_absolute()) {
        let candidate = directory.join(name);
        if candidate.is_file() && candidate.metadata()?.permissions().mode() & 0o111 != 0 {
            return Ok(candidate.canonicalize()?);
        }
    }
    Err(RegistryError::ModelCatalogExecutableMissing(
        name.to_owned(),
    ))
}

pub(crate) fn parse_model_catalog(
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
        // Never read: nothing is listed, and preflight passes the name through.
        ModelCatalogFormat::CliValidated => {}
        ModelCatalogFormat::Lines => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| RegistryError::InvalidModelCatalog)?;
            for line in text.lines().map(str::trim) {
                // A bare `provider/model`; prose or a table row is not one.
                if line.contains('/') && !line.contains(char::is_whitespace) {
                    selectors.insert(line.to_owned());
                }
            }
        }
        ModelCatalogFormat::FirstColumn => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| RegistryError::InvalidModelCatalog)?;
            for line in text.lines() {
                let line = line.trim_start();
                if let Some(selector) = line.split_whitespace().next()
                    && line[selector.len()..].starts_with("  ")
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
