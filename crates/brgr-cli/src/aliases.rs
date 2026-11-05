//! Named harness and model routes shared by CLI and future user interfaces.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Alias {
    pub(crate) harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "alias name must be non-empty, must not start with '-', or contain whitespace/control characters"
        );
    }
    Ok(())
}

pub(crate) fn set(config: &mut Config, name: &str, alias: Alias) -> Result<()> {
    validate_name(name)?;
    if alias.harness.is_empty() {
        bail!("alias harness must not be empty");
    }
    for (label, value) in [("model", &alias.model), ("effort", &alias.effort)] {
        if value
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            bail!("alias {label} must be non-empty and contain no control characters");
        }
    }
    config.aliases.insert(name.to_owned(), alias);
    Ok(())
}

pub(crate) fn remove(config: &mut Config, name: &str) -> Result<Alias> {
    validate_name(name)?;
    config
        .aliases
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("no alias named {name}"))
}

pub(crate) fn get<'a>(config: &'a Config, name: &str) -> Option<&'a Alias> {
    config.aliases.get(name)
}

pub(crate) fn list(config: &Config) -> &BTreeMap<String, Alias> {
    &config.aliases
}

pub(crate) fn known(aliases: &BTreeMap<String, Alias>) -> String {
    if aliases.is_empty() {
        "(none)".to_owned()
    } else {
        aliases.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}
