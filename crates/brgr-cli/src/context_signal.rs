//! What a running head reports about its own usage.
//!
//! A harness already tells its status line how full the context window is and,
//! for Claude Code, how much of the plan's rate limits is used. `brgr __context`
//! sits in that status line, writes the numbers to one small file per session
//! under `$BRGR_HOME/context/`, and then hands the same input to the status line
//! the user had before. The rest of brgr reads only that file, never a harness's
//! logs, so a different harness only needs a different writer.
//!
//! The file is a hint for the head, never task state: a missing, stale or
//! malformed one means "unknown", and unknown is never treated as over a limit.

use std::{
    fs,
    io::{Read as _, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Paths, write_json_atomic};

/// A status line's input is a few KiB; anything near this is not one.
const INPUT_LIMIT: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Signal {
    pub(crate) session: String,
    pub(crate) source: String,
    /// Unix seconds when brgr wrote this.
    pub(crate) at: u64,
    pub(crate) context_percent: Option<f64>,
    pub(crate) context_window: Option<u64>,
    pub(crate) five_hour_percent: Option<f64>,
    pub(crate) five_hour_resets_at: Option<u64>,
    pub(crate) seven_day_percent: Option<f64>,
    pub(crate) seven_day_resets_at: Option<u64>,
}

impl Signal {
    /// The higher of the two plan windows, with its name.
    pub(crate) fn quota(&self) -> Option<(&'static str, f64)> {
        [
            ("five-hour", self.five_hour_percent),
            ("seven-day", self.seven_day_percent),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| (name, value)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A session id names a file, so it is held to what real ones look like.
fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn session_path(paths: &Paths, session: &str) -> Result<PathBuf> {
    if !valid_session(session) {
        bail!("session id is not usable as a file name");
    }
    Ok(paths.home.join("context").join(format!("{session}.json")))
}

fn percent(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

/// Reads Claude Code's status line input (also the shape Cursor Agent uses).
/// `None` when it names no session.
pub(crate) fn parse_status_line(input: &Value, at: u64) -> Option<Signal> {
    let session = input.get("session_id")?.as_str()?.to_owned();
    if !valid_session(&session) {
        return None;
    }
    let window = &input["context_window"];
    let limits = &input["rate_limits"];
    Some(Signal {
        session,
        source: "status-line".to_owned(),
        at,
        context_percent: percent(&window["used_percentage"]),
        context_window: window["context_window_size"].as_u64(),
        five_hour_percent: percent(&limits["five_hour"]["used_percentage"]),
        five_hour_resets_at: limits["five_hour"]["resets_at"].as_u64(),
        seven_day_percent: percent(&limits["seven_day"]["used_percentage"]),
        seven_day_resets_at: limits["seven_day"]["resets_at"].as_u64(),
    })
}

pub(crate) fn record(paths: &Paths, signal: &Signal) -> Result<()> {
    let path = session_path(paths, &signal.session)?;
    let dir = path.parent().context("context path has no parent")?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    write_json_atomic(&path, signal)
}

/// The session's last report, or `None` when there is none, it is unreadable,
/// or it is older than `max_age` seconds.
pub(crate) fn read(paths: &Paths, session: &str, max_age: u64) -> Option<Signal> {
    let bytes = fs::read(session_path(paths, session).ok()?).ok()?;
    let signal: Signal = serde_json::from_slice(&bytes).ok()?;
    (signal.session == session && now().saturating_sub(signal.at) <= max_age).then_some(signal)
}

/// The text a plain status line shows when there is none to chain to.
fn summary(signal: &Signal) -> String {
    let mut parts = Vec::new();
    if let Some(value) = signal.context_percent {
        parts.push(format!("ctx {value:.0}%"));
    }
    if let Some(value) = signal.five_hour_percent {
        parts.push(format!("5h {value:.0}%"));
    }
    if let Some(value) = signal.seven_day_percent {
        parts.push(format!("7d {value:.0}%"));
    }
    parts.join(" · ")
}

/// `brgr __context`: runs as a status line command. It records what it was
/// given, then runs `chain` with the same input so the user's own status line
/// still draws. Recording never fails the status line.
pub(crate) fn run(paths: &Paths, chain: &[String]) -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(INPUT_LIMIT).read_to_end(&mut input)?;
    let signal = serde_json::from_slice::<Value>(&input)
        .ok()
        .and_then(|value| parse_status_line(&value, now()));
    if let Some(signal) = &signal {
        let _ = record(paths, signal);
    }
    let Some((program, arguments)) = chain.split_first() else {
        if let Some(signal) = &signal {
            println!("{}", summary(signal));
        }
        return Ok(());
    };
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run the chained status line `{program}`"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A chained command that ignores its input closes the pipe early.
        let _ = stdin.write_all(&input);
    }
    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = include_str!("../../../testdata/fixtures/statusline/claude-2.1.293.json");
    /// Written from what was observed (every figure was `null` before the first
    /// response), not captured.
    const BEFORE_RESPONSE: &str = include_str!(
        "../../../testdata/fixtures/statusline/claude-2.1.293-before-first-response.synthetic.json"
    );

    fn parse(text: &str) -> Option<Signal> {
        parse_status_line(&serde_json::from_str(text).unwrap(), 1_000)
    }

    #[test]
    fn a_claude_status_line_gives_context_and_both_plan_windows() {
        let signal = parse(CLAUDE).unwrap();
        assert_eq!(signal.session, "0a91ffd2-1a5e-4c75-b65a-54cc4b3b1fa0");
        assert_eq!(signal.context_percent, Some(6.0));
        assert_eq!(signal.context_window, Some(1_000_000));
        assert_eq!(signal.five_hour_percent, Some(7.0));
        assert_eq!(signal.seven_day_percent, Some(43.0));
        assert_eq!(signal.seven_day_resets_at, Some(1_793_131_200));
        assert_eq!(signal.quota(), Some(("seven-day", 43.0)));
    }

    #[test]
    fn nothing_is_measured_before_the_first_response_and_that_is_not_zero() {
        let signal = parse(BEFORE_RESPONSE).unwrap();
        assert_eq!(signal.context_percent, None);
        assert_eq!(signal.quota(), None);
    }

    #[test]
    fn input_without_a_usable_session_gives_no_signal() {
        assert!(parse("{}").is_none());
        assert!(parse(r#"{"session_id": "../escape"}"#).is_none());
        assert!(parse(r#"{"session_id": 7}"#).is_none());
    }

    #[test]
    fn a_percentage_outside_zero_to_one_hundred_is_unknown() {
        let signal = parse(
            r#"{"session_id":"s","context_window":{"used_percentage":250},
                "rate_limits":{"five_hour":{"used_percentage":-1}}}"#,
        )
        .unwrap();
        assert_eq!(signal.context_percent, None);
        assert_eq!(signal.five_hour_percent, None);
    }

    #[test]
    fn a_plain_status_line_shows_what_was_measured() {
        let line = summary(&parse(CLAUDE).unwrap());
        assert_eq!(line, "ctx 6% · 5h 7% · 7d 43%");
        assert_eq!(summary(&parse(BEFORE_RESPONSE).unwrap()), "");
    }
}
