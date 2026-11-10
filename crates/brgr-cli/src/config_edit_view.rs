//! Terminal rendering and I/O only; choices and mutations belong to `config_edit`.

use std::io::{self, IsTerminal as _};

use anyhow::{Result, bail};
use dialoguer::{
    Select,
    console::{Key, Term},
};

use crate::{
    Paths,
    aliases::Alias,
    config::Config,
    config_edit::{self, Action, Choice, Harness, Screen},
};

struct CursorGuard(Term);

impl Drop for CursorGuard {
    fn drop(&mut self) {
        let _ = self.0.show_cursor();
    }
}

fn choose(
    term: &Term,
    config: &Config,
    harnesses: &[Harness],
    screen: Screen<'_>,
) -> Result<Choice> {
    let items = config_edit::menu(config, harnesses, screen);
    let labels: Vec<_> = items.iter().map(|item| &item.label).collect();
    let selected = Select::new()
        .with_prompt("brgr config — Esc to go back")
        .items(&labels)
        .default(0)
        // A model catalog can be longer than the pane.
        .max_length(15)
        .interact_on_opt(term)?;
    Ok(selected.map_or(Choice::Back, |index| items[index].choice.clone()))
}

/// console's per-key raw-mode guard restores terminal settings after every read,
/// including Ctrl+C. Unlike line input, this also supports Esc while typing.
fn text(term: &Term, prompt: &str, initial: &str) -> Result<Option<String>> {
    let mut value = initial.to_owned();
    loop {
        term.clear_line()?;
        term.write_str(&format!("{prompt} (Esc cancels): {value}"))?;
        match term.read_key()? {
            Key::Escape => {
                term.clear_line()?;
                return Ok(None);
            }
            Key::Enter => {
                term.write_line("")?;
                return Ok(Some(value));
            }
            Key::Backspace => {
                value.pop();
            }
            Key::Char(c) if !c.is_control() => value.push(c),
            _ => {}
        }
    }
}

async fn pick_model(
    term: &Term,
    config: &Config,
    harnesses: &mut [Harness],
    registry: &brgr_registry::Registry,
    harness: &str,
    clear: bool,
) -> Result<Option<Option<String>>> {
    let models = registry.registered_models(harness).await;
    let models = match models {
        Ok(models) => Some(models),
        Err(error) if config_edit::catalog_unavailable(&error) => {
            term.write_line(&format!(
                "Could not read model catalog: {error}. Type an exact model name."
            ))?;
            None
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(entry) = harnesses.iter_mut().find(|h| h.id == harness) {
        entry.models = models;
    }
    match choose(term, config, harnesses, Screen::Models { harness, clear })? {
        Choice::Model(model) => Ok(Some(Some(model))),
        Choice::ClearModel => Ok(Some(None)),
        Choice::TypeModel => Ok(text(term, "Model", "")?.map(Some)),
        _ => Ok(None),
    }
}

async fn alias_form(
    term: &Term,
    config: &Config,
    harnesses: &mut [Harness],
    registry: &brgr_registry::Registry,
    previous: Option<String>,
) -> Result<Option<Action>> {
    let Choice::Harness(harness) = choose(term, config, harnesses, Screen::HarnessPicker)? else {
        return Ok(None);
    };
    let Some(model) = pick_model(term, config, harnesses, registry, &harness, false).await? else {
        return Ok(None);
    };
    let old = previous.as_ref().and_then(|name| config.aliases.get(name));
    let Some(effort) = text(
        term,
        "Effort (empty uses default)",
        old.and_then(|a| a.effort.as_deref()).unwrap_or(""),
    )?
    else {
        return Ok(None);
    };
    let Some(name) = text(term, "Alias name", previous.as_deref().unwrap_or(""))? else {
        return Ok(None);
    };
    Ok(Some(Action::SetAlias {
        previous,
        name,
        alias: Alias {
            harness,
            model,
            effort: if effort.is_empty() {
                None
            } else {
                Some(effort)
            },
        },
    }))
}

fn save(
    paths: &Paths,
    term: &Term,
    config: &mut Config,
    harnesses: &[Harness],
    action: Action,
) -> Result<()> {
    match save_config(paths, harnesses, action)? {
        Ok((next, message)) => {
            *config = next;
            term.write_line(&message)?;
        }
        Err(error) => term.write_line(&format!("Not saved: {error}"))?,
    }
    Ok(())
}

fn save_config(
    paths: &Paths,
    harnesses: &[Harness],
    action: Action,
) -> Result<std::result::Result<(Config, String), config_edit::EditError>> {
    let mut message = None;
    let mut edit_error = None;
    let next = Config::update(
        &paths.config,
        &paths.home,
        |fresh| match config_edit::apply(fresh, harnesses, action) {
            Ok((updated, saved_message)) => {
                *fresh = updated;
                message = Some(saved_message);
                Ok(true)
            }
            Err(error) => {
                edit_error = Some(error);
                Ok(false)
            }
        },
    )?;
    if let Some(error) = edit_error {
        Ok(Err(error))
    } else {
        Ok(Ok((
            next,
            message.expect("successful config edit has a save message"),
        )))
    }
}

pub(crate) async fn run(paths: &Paths, json: bool) -> Result<()> {
    if json
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
    {
        bail!(
            "brgr config edit requires a terminal and does not support --json; use brgr config alias or brgr config set instead"
        );
    }
    let term = Term::stderr();
    let _cursor = CursorGuard(term.clone());
    // console restores termios before raising SIGINT and returning Interrupted.
    // Keep the signal from terminating the process before CursorGuard can run.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! {
        result = edit(paths, &term) => result,
        _ = interrupt.recv() => Ok(()),
    }
}

async fn edit(paths: &Paths, term: &Term) -> Result<()> {
    let registry = brgr_registry::Registry::open_with_control_home(&paths.registry, &paths.home)?;
    let mut config = Config::load(&paths.config)?;
    let mut harnesses: Vec<_> = registry
        .registered_harness_ids()?
        .into_iter()
        .filter(|id| config_edit::visible(id))
        .map(|id| {
            let health = registry
                .health(&id)
                .map_or_else(|e| format!("unavailable: {e}"), |h| h.code().into());
            Harness {
                id,
                health,
                models: None,
            }
        })
        .collect();
    loop {
        match choose(term, &config, &harnesses, Screen::Main)? {
            Choice::Quit | Choice::Back => return Ok(()),
            Choice::Aliases => loop {
                let action = match choose(term, &config, &harnesses, Screen::Aliases)? {
                    Choice::AddAlias => {
                        alias_form(term, &config, &mut harnesses, &registry, None).await?
                    }
                    Choice::EditAlias(name) => {
                        match choose(term, &config, &harnesses, Screen::Alias(&name))? {
                            Choice::EditAlias(name) => {
                                alias_form(term, &config, &mut harnesses, &registry, Some(name))
                                    .await?
                            }
                            Choice::RemoveAlias(name) => Some(Action::RemoveAlias(name)),
                            _ => None,
                        }
                    }
                    _ => break,
                };
                if let Some(action) = action {
                    save(paths, term, &mut config, &harnesses, action)?;
                }
            },
            Choice::Defaults => loop {
                let Choice::Harness(harness) =
                    choose(term, &config, &harnesses, Screen::HarnessPicker)?
                else {
                    break;
                };
                if let Some(model) =
                    pick_model(term, &config, &mut harnesses, &registry, &harness, true).await?
                {
                    save(
                        paths,
                        term,
                        &mut config,
                        &harnesses,
                        Action::DefaultModel { harness, model },
                    )?;
                }
            },
            Choice::Harnesses => {
                choose(term, &config, &harnesses, Screen::Harnesses)?;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_an_edit_keeps_a_setting_changed_elsewhere() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_path_buf())).unwrap();
        Config::update(&paths.config, &paths.home, |starting| {
            starting.issues.auto_file = true;
            starting.issues.repo = Some("owner/repo".into());
            Ok(true)
        })
        .unwrap();

        Config::update(&paths.config, &paths.home, |changed_elsewhere| {
            changed_elsewhere.issues.auto_file = false;
            changed_elsewhere.issues.repo = Some("elsewhere/repo".into());
            Ok(true)
        })
        .unwrap();

        let harnesses = [Harness {
            id: "local.codex".into(),
            health: "healthy".into(),
            models: Some(vec!["gpt-6".into()]),
        }];
        save_config(
            &paths,
            &harnesses,
            Action::DefaultModel {
                harness: "local.codex".into(),
                model: Some("gpt-6".into()),
            },
        )
        .unwrap()
        .unwrap();

        let saved = Config::load(&paths.config).unwrap();
        assert!(!saved.issues.auto_file);
        assert_eq!(saved.issues.repo.as_deref(), Some("elsewhere/repo"));
        assert_eq!(
            saved.harnesses["local.codex"].model.as_deref(),
            Some("gpt-6")
        );
    }
}
