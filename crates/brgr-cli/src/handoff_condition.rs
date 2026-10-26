//! Telling a head that it is time to hand its command to another harness.
//!
//! Every brgr command a head runs may end with one note on stderr, like the
//! failure banner, when the head's own context window or plan usage has passed
//! the user's threshold. The note carries the exact `brgr head handoff` command;
//! the head runs it, because that command needs a verified pane and brgr never
//! chooses a harness or closes a pane on its own.
//!
//! The usage comes from `context_signal`. A missing, stale or unmeasured figure
//! is never over a limit. The note is given once per crossing and again only
//! when the head has stayed over for [`REPEAT_AFTER`] or has dropped
//! [`REARM_GAP`] points below the limit and come back.

use std::{fmt::Write as _, fs};

use serde::{Deserialize, Serialize};

use crate::{
    Paths,
    config::{Config, HandoffConfig},
    context_signal::{self, Signal},
};
use brgr_store::Store;

/// A figure older than this is treated as unmeasured.
const MAX_AGE: u64 = 30 * 60;
/// A head that stays over its limit is told again this often.
const REPEAT_AFTER: u64 = 30 * 60;
/// Points below a limit before the next crossing counts as new.
const REARM_GAP: f64 = 15.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct State {
    armed: bool,
    last_note_at: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            armed: true,
            last_note_at: 0,
        }
    }
}

struct Over {
    reasons: Vec<String>,
    /// Every configured figure is known and well below its limit.
    rearmed: bool,
}

fn measure(config: &HandoffConfig, signal: &Signal) -> Over {
    let mut reasons = Vec::new();
    let mut rearmed = true;
    let mut check = |label: String, value: Option<f64>, limit: Option<u8>| {
        let Some(limit) = limit.map(f64::from) else {
            return;
        };
        match value {
            Some(value) if value >= limit => {
                rearmed = false;
                reasons.push(format!("{label} {value:.0}% (limit {limit:.0}%)"));
            }
            Some(value) if value >= limit - REARM_GAP => rearmed = false,
            Some(_) => {}
            // Unmeasured is not evidence of being below.
            None => rearmed = false,
        }
    };
    check(
        "context window".to_owned(),
        signal.context_percent,
        config.context_percent,
    );
    let quota = signal.quota();
    check(
        format!("{} plan window", quota.map_or("plan", |(name, _)| name)),
        quota.map(|(_, value)| value),
        config.quota_percent,
    );
    Over { reasons, rearmed }
}

/// What a head's brgr command should do about its usage.
#[derive(Debug, PartialEq)]
pub(crate) struct Verdict {
    /// The note to print, if any.
    pub(crate) note: Option<String>,
    /// The reasons, when the configured handoff is due and may be run.
    pub(crate) handoff: Option<Vec<String>>,
}

/// The note to print, if any, and the state to keep. `owner_kind` is the
/// harness the head runs in, the part of its owner id before the colon.
#[cfg(test)]
pub(crate) fn decide(
    config: &HandoffConfig,
    signal: &Signal,
    state: State,
    now: u64,
    owner_kind: &str,
    target_quota: Option<f64>,
) -> (Option<String>, State) {
    let (verdict, state) = evaluate(config, signal, state, now, owner_kind, target_quota);
    (verdict.note, state)
}

pub(crate) fn evaluate(
    config: &HandoffConfig,
    signal: &Signal,
    state: State,
    now: u64,
    owner_kind: &str,
    target_quota: Option<f64>,
) -> (Verdict, State) {
    let nothing = Verdict {
        note: None,
        handoff: None,
    };
    let Some(kind) = config.kind.as_deref().filter(|_| config.is_armed()) else {
        return (nothing, state);
    };
    // Handing to the harness the head already runs in would be told again by
    // the new head at once.
    if kind == owner_kind || now.saturating_sub(signal.at) > MAX_AGE {
        return (nothing, state);
    }
    let over = measure(config, signal);
    let mut state = state;
    if over.rearmed {
        state.armed = true;
    }
    if over.reasons.is_empty()
        || !(state.armed || now.saturating_sub(state.last_note_at) >= REPEAT_AFTER)
    {
        return (nothing, state);
    }
    state.armed = false;
    state.last_note_at = now;
    // Handing to a harness whose own plan is already at the limit only moves the
    // problem; say so instead of giving the command.
    if let (Some(limit), Some(target)) = (config.quota_percent, target_quota)
        && target >= f64::from(limit)
    {
        let verdict = Verdict {
            note: Some(target_over_note(kind, &over.reasons, target, limit)),
            handoff: None,
        };
        return (verdict, state);
    }
    let verdict = Verdict {
        note: Some(note(config, kind, &over.reasons)),
        handoff: Some(over.reasons),
    };
    (verdict, state)
}

fn target_over_note(kind: &str, reasons: &[String], target: f64, limit: u8) -> String {
    format!(
        "brgr: this head's {} is over the limit you set, but {kind}, the harness you chose to \
         hand off to, is itself at {target:.0}% of its plan (limit {limit}%), so no handoff \
         command is given. Change the target with `brgr config set-handoff-condition`, or wait.",
        reasons.join(" and "),
    )
}

fn note(config: &HandoffConfig, kind: &str, reasons: &[String]) -> String {
    let mut command = format!("brgr head handoff TASK --kind {kind}");
    if config.exit_old {
        command.push_str(" --exit-old");
    }
    for argument in &config.agent_args {
        let _ = write!(command, " --agent-arg={argument}");
    }
    command.push_str(" --goal \"WHAT YOU ARE FINISHING, IN YOUR OWN WORDS\"");
    format!(
        "brgr: this head's {} is over the limit you set for handing off to {kind}.\n  \
         Finish your current step, then run: {command}\n  \
         TASK is any task you own. This note repeats after {} minutes if still over.",
        reasons.join(" and "),
        REPEAT_AFTER / 60,
    )
}

fn state_path(paths: &Paths, session: &str) -> std::path::PathBuf {
    paths
        .home
        .join("context")
        .join(format!("{session}.note.json"))
}

/// Prints the note, if any. Never fails the command it follows.
pub(crate) fn print(paths: &Paths) {
    let Ok(config) = Config::load(&paths.config) else {
        return;
    };
    if !config.handoff.is_armed() {
        return;
    }
    let Ok(Some(session)) = crate::current_session() else {
        return;
    };
    let Ok(owner) = crate::owner_from_environment() else {
        return;
    };
    let Ok(store) = Store::open(&paths.store) else {
        return;
    };
    // Only the bound head is told, and not while a handoff is already open.
    if store
        .owner_binding(&owner)
        .ok()
        .flatten()
        .is_none_or(|(bound, _)| bound != session)
        || store.owner_takeovers(&owner).ok().is_none_or(|takeovers| {
            takeovers
                .iter()
                .any(|takeover| !takeover.state.is_terminal())
        })
    {
        return;
    }
    let Some(signal) = context_signal::read(paths, &session, MAX_AGE) else {
        return;
    };
    let path = state_path(paths, &session);
    let state = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let owner_kind = owner.as_str().split(':').next().unwrap_or_default();
    let target_quota = config
        .handoff
        .kind
        .as_deref()
        .and_then(|kind| context_signal::latest_quota(paths, kind, MAX_AGE));
    let (verdict, state) = evaluate(
        &config.handoff,
        &signal,
        state,
        context_signal::now(),
        owner_kind,
        target_quota,
    );
    // Written before anything slow runs, so a handoff that takes a while is not
    // started twice by a second command.
    let _ = crate::write_json_atomic(&path, &state);
    match (&verdict.handoff, config.handoff.auto) {
        (Some(reasons), true) => {
            match crate::head::auto_handoff(paths, &store, &owner, &config.handoff, reasons) {
                Ok(receipt) => eprintln!(
                    "brgr: this head's {} is over the limit you set, so a handoff to {} was \
                     started automatically (incoming pane {}). Finish your current step; the \
                     incoming head takes command, and {}",
                    reasons.join(" and "),
                    config.handoff.kind.as_deref().unwrap_or_default(),
                    receipt["incoming_pane"].as_str().unwrap_or("unknown"),
                    if config.handoff.exit_old {
                        "this pane closes after it has retired the handoff."
                    } else {
                        "this pane stays open."
                    },
                ),
                Err(error) => eprintln!(
                    "{}\nbrgr: the automatic handoff did not start: {error:#}",
                    verdict.note.as_deref().unwrap_or_default()
                ),
            }
        }
        _ => {
            if let Some(text) = &verdict.note {
                eprintln!("{text}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HandoffConfig {
        HandoffConfig {
            kind: Some("codex".to_owned()),
            exit_old: true,
            context_percent: Some(70),
            quota_percent: Some(85),
            agent_args: vec!["--yolo".to_owned()],
            auto: false,
        }
    }

    fn signal(context: Option<f64>, quota: Option<f64>, at: u64) -> Signal {
        Signal {
            session: "s".to_owned(),
            source: "claude".to_owned(),
            at,
            context_percent: context,
            context_window: Some(1_000_000),
            five_hour_percent: quota,
            five_hour_resets_at: None,
            seven_day_percent: None,
            seven_day_resets_at: None,
        }
    }

    fn step(
        state: State,
        context: Option<f64>,
        quota: Option<f64>,
        now: u64,
    ) -> (Option<String>, State) {
        decide(
            &config(),
            &signal(context, quota, now),
            state,
            now,
            "claude",
            None,
        )
    }

    #[test]
    fn crossing_a_limit_gives_the_exact_command_once() {
        let (text, state) = step(State::default(), Some(72.0), Some(10.0), 1_000);
        let text = text.unwrap();
        assert!(text.contains("context window 72% (limit 70%)"), "{text}");
        assert!(
            text.contains("brgr head handoff TASK --kind codex --exit-old --agent-arg=--yolo"),
            "{text}"
        );
        // The next command, a minute later, is not told again.
        assert_eq!(step(state, Some(73.0), Some(10.0), 1_060).0, None);
    }

    #[test]
    fn staying_over_is_told_again_after_the_repeat_interval() {
        let (_, state) = step(State::default(), Some(72.0), None, 1_000);
        assert_eq!(
            step(state, Some(75.0), None, 1_000 + REPEAT_AFTER - 1).0,
            None
        );
        assert!(
            step(state, Some(75.0), None, 1_000 + REPEAT_AFTER)
                .0
                .is_some()
        );
    }

    #[test]
    fn dropping_well_below_the_limit_rearms_the_next_crossing() {
        let (_, state) = step(State::default(), Some(72.0), None, 1_000);
        // Near the limit is not "below": 60% is within the gap of 70%.
        let (none, state) = step(state, Some(60.0), None, 1_100);
        assert_eq!(none, None);
        assert_eq!(step(state, Some(72.0), None, 1_200).0, None);
        // 40% is below limit minus the gap, and the quota is measured and low.
        let (none, state) = step(state, Some(40.0), Some(10.0), 1_300);
        assert_eq!(none, None);
        assert!(step(state, Some(71.0), Some(10.0), 1_400).0.is_some());
    }

    #[test]
    fn an_unmeasured_figure_is_never_over_and_never_rearms() {
        assert_eq!(step(State::default(), None, None, 1_000).0, None);
        let (_, state) = step(State::default(), Some(72.0), None, 1_000);
        // Context vanished (for example just after a compaction): not rearmed.
        let (none, state) = step(state, None, None, 1_100);
        assert_eq!(none, None);
        assert_eq!(step(state, Some(72.0), None, 1_200).0, None);
    }

    #[test]
    fn a_stale_signal_says_nothing() {
        let stale = signal(Some(99.0), Some(99.0), 1_000);
        let (text, _) = decide(
            &config(),
            &stale,
            State::default(),
            1_000 + MAX_AGE + 1,
            "claude",
            None,
        );
        assert_eq!(text, None);
    }

    #[test]
    fn a_head_already_running_in_the_target_harness_is_not_told() {
        let (text, _) = decide(
            &config(),
            &signal(Some(99.0), None, 1_000),
            State::default(),
            1_000,
            "codex",
            None,
        );
        assert_eq!(text, None);
    }

    #[test]
    fn nothing_is_said_until_a_target_and_a_threshold_are_set() {
        let mut off = config();
        off.kind = None;
        let signal = signal(Some(99.0), Some(99.0), 1_000);
        assert_eq!(
            decide(&off, &signal, State::default(), 1_000, "claude", None).0,
            None
        );
        let mut no_limit = config();
        no_limit.context_percent = None;
        no_limit.quota_percent = None;
        assert_eq!(
            decide(&no_limit, &signal, State::default(), 1_000, "claude", None).0,
            None
        );
    }

    #[test]
    fn both_limits_are_named_when_both_are_over() {
        let (text, _) = step(State::default(), Some(80.0), Some(90.0), 1_000);
        let text = text.unwrap();
        assert!(text.contains("context window 80%"), "{text}");
        assert!(text.contains("five-hour plan window 90%"), "{text}");
    }

    #[test]
    fn a_target_already_at_its_plan_limit_is_named_instead_of_handed_to() {
        let (text, state) = decide(
            &config(),
            &signal(Some(80.0), Some(10.0), 1_000),
            State::default(),
            1_000,
            "claude",
            Some(91.0),
        );
        let text = text.unwrap();
        assert!(text.contains("codex, the harness you chose"), "{text}");
        assert!(text.contains("91% of its plan"), "{text}");
        assert!(!text.contains("brgr head handoff TASK"), "{text}");
        // Said once per crossing, like the command would have been.
        let (again, _) = decide(
            &config(),
            &signal(Some(81.0), Some(10.0), 1_060),
            state,
            1_060,
            "claude",
            Some(91.0),
        );
        assert_eq!(again, None);
    }

    #[test]
    fn a_target_below_its_limit_or_unmeasured_gets_the_command() {
        for target in [None, Some(40.0), Some(84.9)] {
            let (text, _) = decide(
                &config(),
                &signal(Some(80.0), Some(10.0), 1_000),
                State::default(),
                1_000,
                "claude",
                target,
            );
            assert!(
                text.unwrap()
                    .contains("brgr head handoff TASK --kind codex"),
                "{target:?}"
            );
        }
    }

    #[test]
    fn the_target_is_not_checked_without_a_plan_limit() {
        let mut context_only = config();
        context_only.quota_percent = None;
        let (text, _) = decide(
            &context_only,
            &signal(Some(80.0), Some(10.0), 1_000),
            State::default(),
            1_000,
            "claude",
            Some(99.0),
        );
        assert!(text.unwrap().contains("brgr head handoff TASK"));
    }
}
