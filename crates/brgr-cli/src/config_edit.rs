//! Terminal-independent settings screen and transitions. Catalogs are supplied by the caller.

use crate::{
    aliases::{self, Alias},
    config::Config,
};

pub(crate) const TIERS: &[(&str, u8)] = &[
    ("local.claude-code", 1),
    ("local.codex", 1),
    ("local.cursor-cli", 1),
    ("local.devin", 1),
    ("local.command-code", 2),
    ("local.omp", 2),
    ("local.omp-herdr", 2),
    ("local.opencode", 2),
    ("local.cline", 2),
    ("local.copilot", 3),
    ("local.gemini", 3),
    ("local.pi", 3),
];

#[derive(Clone, Debug)]
pub(crate) struct Harness {
    pub id: String,
    pub health: String,
    /// None means the catalog could not be read (including CLI-validated names).
    pub models: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Aliases,
    Defaults,
    Harnesses,
    Quit,
    AddAlias,
    EditAlias(String),
    RemoveAlias(String),
    Harness(String),
    Model(String),
    TypeModel,
    ClearModel,
    Back,
}

pub(crate) struct Item {
    pub label: String,
    pub choice: Choice,
}

fn item(label: impl Into<String>, choice: Choice) -> Item {
    Item {
        label: label.into(),
        choice,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Screen<'a> {
    Main,
    Aliases,
    Alias(&'a str),
    HarnessPicker,
    Harnesses,
    Models { harness: &'a str, clear: bool },
}

pub(crate) fn visible(id: &str) -> bool {
    !matches!(id, "local.fixture" | "local.gjc")
}

/// Match the alias command's fallback policy; malformed registry data is not
/// permission to accept arbitrary models.
pub(crate) fn catalog_unavailable(error: &brgr_registry::RegistryError) -> bool {
    matches!(
        error,
        brgr_registry::RegistryError::ModelCatalogMissing(_)
            | brgr_registry::RegistryError::ModelCatalogExecutableMissing(_)
            | brgr_registry::RegistryError::InvalidModelCatalog
            | brgr_registry::RegistryError::ModelCatalogProbeFailed { .. }
    )
}

pub(crate) fn menu(config: &Config, harnesses: &[Harness], screen: Screen<'_>) -> Vec<Item> {
    let mut items = match screen {
        Screen::Main => {
            return vec![
                item("Aliases", Choice::Aliases),
                item("Default model per harness", Choice::Defaults),
                item("Harnesses", Choice::Harnesses),
                item("Quit", Choice::Quit),
            ];
        }
        Screen::Aliases => {
            let mut items = vec![item("Add alias", Choice::AddAlias)];
            items.extend(aliases::list(config).iter().map(|(name, alias)| {
                item(
                    format!(
                        "{name}: {} / {} / {}",
                        alias.harness,
                        alias.model.as_deref().unwrap_or("default"),
                        alias.effort.as_deref().unwrap_or("default effort")
                    ),
                    Choice::EditAlias(name.clone()),
                )
            }));
            items
        }
        Screen::Alias(name) => vec![
            item("Edit", Choice::EditAlias(name.into())),
            item("Remove", Choice::RemoveAlias(name.into())),
        ],
        Screen::HarnessPicker | Screen::Harnesses => harnesses
            .iter()
            .filter(|h| visible(&h.id))
            .map(|h| {
                let tier = TIERS
                    .iter()
                    .find(|(id, _)| *id == h.id)
                    .map_or("custom".into(), |(_, tier)| format!("tier {tier}"));
                item(
                    format!(
                        "{} — {} — {tier} — model: {}",
                        h.id,
                        h.health,
                        config
                            .harnesses
                            .get(&h.id)
                            .and_then(|o| o.model.as_deref())
                            .unwrap_or("default")
                    ),
                    if matches!(screen, Screen::Harnesses) {
                        Choice::Back
                    } else {
                        Choice::Harness(h.id.clone())
                    },
                )
            })
            .collect(),
        Screen::Models { harness, clear } => {
            let mut items = harnesses
                .iter()
                .find(|h| h.id == harness)
                .and_then(|h| h.models.as_ref())
                .map_or_else(
                    || vec![item("Type model", Choice::TypeModel)],
                    |models| {
                        models
                            .iter()
                            .map(|m| item(m, Choice::Model(m.clone())))
                            .collect()
                    },
                );
            if clear {
                items.push(item("Clear default model", Choice::ClearModel));
            }
            items
        }
    };
    items.push(item("Back", Choice::Back));
    items
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum EditError {
    #[error("harness {0} is not registered")]
    UnregisteredHarness(String),
    #[error("model {0} is not in the harness catalog")]
    UnknownModel(String),
    #[error("{0}")]
    InvalidAlias(String),
    #[error("alias {0} already exists")]
    AliasExists(String),
    #[error("no alias named {0}")]
    MissingAlias(String),
    #[error("{0}")]
    InvalidModel(String),
}

pub(crate) enum Action {
    SetAlias {
        previous: Option<String>,
        name: String,
        alias: Alias,
    },
    RemoveAlias(String),
    DefaultModel {
        harness: String,
        model: Option<String>,
    },
}

fn validate_route(harnesses: &[Harness], id: &str, model: Option<&str>) -> Result<(), EditError> {
    let h = harnesses
        .iter()
        .find(|h| h.id == id && visible(id))
        .ok_or_else(|| EditError::UnregisteredHarness(id.into()))?;
    if let Some(model) = model {
        if model.is_empty() || model == "auto" || model.chars().any(char::is_control) {
            return Err(EditError::InvalidModel(
                "model must be an exact, non-empty name without control characters".into(),
            ));
        }
        if h.models
            .as_ref()
            .is_some_and(|models| !models.iter().any(|m| m == model))
        {
            return Err(EditError::UnknownModel(model.into()));
        }
    }
    Ok(())
}

pub(crate) fn apply(
    config: &Config,
    harnesses: &[Harness],
    action: Action,
) -> Result<(Config, String), EditError> {
    let mut next = config.clone();
    let message = match action {
        Action::SetAlias {
            previous,
            name,
            alias,
        } => {
            validate_route(harnesses, &alias.harness, alias.model.as_deref())?;
            if let Some(old) = &previous
                && aliases::get(config, old).is_none()
            {
                return Err(EditError::MissingAlias(old.clone()));
            }
            if previous.as_deref() != Some(&name) && aliases::get(config, &name).is_some() {
                return Err(EditError::AliasExists(name));
            }
            aliases::set(&mut next, &name, alias)
                .map_err(|e| EditError::InvalidAlias(e.to_string()))?;
            if let Some(old) = &previous
                && old != &name
            {
                aliases::remove(&mut next, old)
                    .map_err(|e| EditError::InvalidAlias(e.to_string()))?;
            }
            format!("Saved alias {name}.")
        }
        Action::RemoveAlias(name) => {
            aliases::remove(&mut next, &name)
                .map_err(|e| EditError::InvalidAlias(e.to_string()))?;
            format!("Removed alias {name}.")
        }
        Action::DefaultModel { harness, model } => {
            validate_route(harnesses, &harness, model.as_deref())?;
            if let Some(model) = &model {
                next.set("model", model, Some(&harness))
                    .map_err(|e| EditError::InvalidModel(e.to_string()))?;
            } else if let Some(options) = next.harnesses.get_mut(&harness) {
                options.model = None;
            }
            format!(
                "{} default model for {harness}{}.",
                if model.is_some() { "Saved" } else { "Cleared" },
                model.map_or(String::new(), |m| format!(": {m}"))
            )
        }
    };
    Ok((next, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harnesses() -> Vec<Harness> {
        vec![Harness {
            id: "local.codex".into(),
            health: "healthy".into(),
            models: Some(vec!["gpt-6".into()]),
        }]
    }

    fn alias_action(previous: Option<&str>, name: &str, harness: &str, model: &str) -> Action {
        Action::SetAlias {
            previous: previous.map(str::to_owned),
            name: name.into(),
            alias: Alias {
                harness: harness.into(),
                model: Some(model.into()),
                effort: Some("high".into()),
            },
        }
    }

    #[test]
    fn add_edit_remove_alias_preserves_other_sections() {
        let mut original = Config::default();
        original.set("model", "global-model", None).unwrap();
        original
            .set("effort", "medium", Some("local.codex"))
            .unwrap();
        original.recertify.auto = true;
        original.issues.repo = Some("owner/repo".into());
        let (added, _) = apply(
            &original,
            &harnesses(),
            alias_action(None, "코덱", "local.codex", "gpt-6"),
        )
        .unwrap();
        assert_eq!(added.aliases["코덱"].model.as_deref(), Some("gpt-6"));
        assert_eq!(added.aliases["코덱"].effort.as_deref(), Some("high"));
        let (edited, _) = apply(
            &added,
            &harnesses(),
            alias_action(Some("코덱"), "새이름", "local.codex", "gpt-6"),
        )
        .unwrap();
        assert!(!edited.aliases.contains_key("코덱"));
        assert!(edited.aliases.contains_key("새이름"));
        let (removed, _) =
            apply(&edited, &harnesses(), Action::RemoveAlias("새이름".into())).unwrap();
        assert_eq!(removed, original);
        assert!(original.aliases.is_empty());
    }

    #[test]
    fn rejects_invalid_alias_routes_without_changing_config() {
        let config = Config::default();
        assert_eq!(
            apply(
                &config,
                &harnesses(),
                alias_action(None, "x", "local.unknown", "gpt-6")
            )
            .unwrap_err(),
            EditError::UnregisteredHarness("local.unknown".into())
        );
        assert_eq!(
            apply(
                &config,
                &harnesses(),
                alias_action(None, "x", "local.codex", "unknown")
            )
            .unwrap_err(),
            EditError::UnknownModel("unknown".into())
        );
        for name in ["", "-bad", "has space", "control\n"] {
            assert!(matches!(
                apply(
                    &config,
                    &harnesses(),
                    alias_action(None, name, "local.codex", "gpt-6")
                ),
                Err(EditError::InvalidAlias(_))
            ));
        }
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unreadable_catalog_accepts_typed_model_and_rename_cannot_overwrite() {
        let mut harnesses = harnesses();
        harnesses[0].models = None;
        let (config, _) = apply(
            &Config::default(),
            &harnesses,
            alias_action(None, "a", "local.codex", "future-model"),
        )
        .unwrap();
        let (config, _) = apply(
            &config,
            &harnesses,
            alias_action(None, "b", "local.codex", "other-model"),
        )
        .unwrap();
        assert_eq!(
            apply(
                &config,
                &harnesses,
                alias_action(Some("a"), "b", "local.codex", "future-model")
            )
            .unwrap_err(),
            EditError::AliasExists("b".into())
        );
        assert_eq!(config.aliases["b"].model.as_deref(), Some("other-model"));
    }

    #[test]
    fn default_model_matches_config_set_and_clear_preserves_other_options() {
        let mut expected = Config::default();
        expected.set("effort", "high", Some("local.codex")).unwrap();
        expected.set("model", "global", None).unwrap();
        let original = expected.clone();
        expected.set("model", "gpt-6", Some("local.codex")).unwrap();
        let (set, _) = apply(
            &original,
            &harnesses(),
            Action::DefaultModel {
                harness: "local.codex".into(),
                model: Some("gpt-6".into()),
            },
        )
        .unwrap();
        assert_eq!(set, expected);
        let (cleared, _) = apply(
            &set,
            &harnesses(),
            Action::DefaultModel {
                harness: "local.codex".into(),
                model: None,
            },
        )
        .unwrap();
        assert_eq!(cleared, original);
    }

    #[test]
    fn menu_exposes_routes_models_and_read_only_health() {
        let mut harnesses = harnesses();
        harnesses.push(Harness {
            id: "local.fixture".into(),
            health: "healthy".into(),
            models: None,
        });
        harnesses.push(Harness {
            id: "local.gjc".into(),
            health: "healthy".into(),
            models: None,
        });
        let config = Config::default();
        assert_eq!(
            menu(&config, &harnesses, Screen::Main)
                .into_iter()
                .map(|i| i.choice)
                .collect::<Vec<_>>(),
            vec![
                Choice::Aliases,
                Choice::Defaults,
                Choice::Harnesses,
                Choice::Quit
            ]
        );
        let list = menu(&config, &harnesses, Screen::Harnesses);
        assert_eq!(list.len(), 2);
        assert!(list[0].label.contains("healthy — tier 1"));
        assert_eq!(list[0].choice, Choice::Back);
        assert_eq!(
            menu(
                &config,
                &harnesses,
                Screen::Models {
                    harness: "local.codex",
                    clear: true
                }
            )
            .into_iter()
            .map(|i| i.choice)
            .collect::<Vec<_>>(),
            vec![
                Choice::Model("gpt-6".into()),
                Choice::ClearModel,
                Choice::Back
            ]
        );
    }

    #[test]
    fn tier_table_covers_every_builtin_recipe_except_fixture() {
        // Read the shipped recipe source, so a newly added recipe requires a tier decision.
        let source = include_str!("../../brgr-registry/src/recipe.rs")
            .split_once("pub(crate) const RECIPES:")
            .unwrap()
            .1;
        let ids: std::collections::BTreeSet<_> = source
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("id: \"")
                    .and_then(|s| s.strip_suffix("\","))
            })
            .filter(|id| *id != "local.fixture")
            .collect();
        let tiers: std::collections::BTreeSet<_> = TIERS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, tiers);
        assert_eq!(tiers.len(), TIERS.len());
        assert!(TIERS.iter().all(|(_, tier)| (1..=3).contains(tier)));
    }
}
