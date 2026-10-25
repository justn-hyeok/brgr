use anyhow::{Result, bail};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum ModeRequest {
    #[default]
    Pane,
    Headless,
    Auto,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedMode {
    Pane,
    Headless,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Resolution {
    pub(crate) mode: ResolvedMode,
    pub(crate) reason: &'static str,
}

pub(crate) fn resolve(
    requested: ModeRequest,
    pane_available: bool,
    headless_available: bool,
) -> Result<Resolution> {
    match requested {
        ModeRequest::Pane => Ok(Resolution {
            mode: ResolvedMode::Pane,
            reason: "requested",
        }),
        ModeRequest::Headless => Ok(Resolution {
            mode: ResolvedMode::Headless,
            reason: "requested",
        }),
        ModeRequest::Auto if pane_available => Ok(Resolution {
            mode: ResolvedMode::Pane,
            reason: "herdr pane available",
        }),
        ModeRequest::Auto if headless_available => Ok(Resolution {
            mode: ResolvedMode::Headless,
            reason: "no herdr pane in this environment",
        }),
        ModeRequest::Auto => bail!(
            "cannot resolve --mode auto: no Herdr pane is available and the harness has no headless recipe"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_modes_and_auto_resolve_from_only_the_two_route_facts() {
        for (pane_available, headless_available) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            assert_eq!(
                resolve(ModeRequest::Pane, pane_available, headless_available).unwrap(),
                Resolution {
                    mode: ResolvedMode::Pane,
                    reason: "requested"
                }
            );
            assert_eq!(
                resolve(ModeRequest::Headless, pane_available, headless_available).unwrap(),
                Resolution {
                    mode: ResolvedMode::Headless,
                    reason: "requested"
                }
            );
            if pane_available {
                assert_eq!(
                    resolve(ModeRequest::Auto, pane_available, headless_available).unwrap(),
                    Resolution {
                        mode: ResolvedMode::Pane,
                        reason: "herdr pane available"
                    }
                );
            } else if headless_available {
                assert_eq!(
                    resolve(ModeRequest::Auto, pane_available, headless_available).unwrap(),
                    Resolution {
                        mode: ResolvedMode::Headless,
                        reason: "no herdr pane in this environment"
                    }
                );
            } else {
                let error = resolve(ModeRequest::Auto, pane_available, headless_available)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("no Herdr pane"));
                assert!(error.contains("no headless recipe"));
            }
        }
    }

    #[test]
    fn auto_selection_depends_on_pane_availability_even_when_other_route_constraints_exist() {
        // All route constraints are deliberately absent from the resolver's
        // facts: unsupported TUI options and launch/runtime outcomes cannot
        // trigger a headless retry after a pane was selected.
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn failed_pane_launch_does_not_select_headless() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn unsupported_tui_option_does_not_select_headless() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn read_only_scope_does_not_select_headless() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn prefer_print_mode_does_not_select_headless() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn harness_or_model_does_not_select_headless_when_pane_is_available() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }

    #[test]
    fn post_launch_failure_does_not_select_headless() {
        assert_eq!(
            resolve(ModeRequest::Auto, true, true).unwrap().mode,
            ResolvedMode::Pane
        );
    }
}
