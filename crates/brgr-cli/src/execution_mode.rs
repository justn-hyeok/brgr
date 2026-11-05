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
}
