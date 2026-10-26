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
    io::{Read as _, Seek as _, SeekFrom, Write as _},
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
pub(crate) fn parse_status_line(input: &Value, at: u64, source: &str) -> Option<Signal> {
    let session = input.get("session_id")?.as_str()?.to_owned();
    if !valid_session(&session) {
        return None;
    }
    let window = &input["context_window"];
    let limits = &input["rate_limits"];
    Some(Signal {
        session,
        source: source.to_owned(),
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

/// The plan usage, in percent, that the newest fresh report from `provider`
/// carries. A plan is the account's, so any session of that harness will do;
/// `None` when no session of it has reported lately, which says nothing.
pub(crate) fn latest_quota(paths: &Paths, provider: &str, max_age: u64) -> Option<f64> {
    let now = now();
    fs::read_dir(paths.home.join("context"))
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            // `<session>.json` is a report; `<session>.note.json` is the note state.
            if path.extension().is_none_or(|ext| ext != "json") || name.contains(".note.") {
                return None;
            }
            serde_json::from_slice::<Signal>(&fs::read(&path).ok()?).ok()
        })
        .filter(|signal| signal.source == provider && now.saturating_sub(signal.at) <= max_age)
        .filter_map(|signal| Some((signal.at, signal.quota()?.1)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, quota)| quota)
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

/// How much of a Codex rollout's end is read to find its last token report.
const ROLLOUT_TAIL: u64 = 256 * 1024;

/// Reads the last token report in the end of a Codex rollout file.
///
/// Codex writes an `event_msg` / `token_count` line after every model response.
/// The context in use is the last request's total over the model's window; the
/// cumulative total is not the context. The first line of a tail may be cut
/// and is skipped like any line that is not JSON.
pub(crate) fn parse_codex_rollout(session: &str, tail: &[u8], at: u64) -> Option<Signal> {
    if !valid_session(session) {
        return None;
    }
    let text = String::from_utf8_lossy(tail);
    let report = text.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        (value["type"] == "event_msg"
            && value["payload"]["type"] == "token_count"
            && value["payload"]["info"].is_object())
        .then(|| value["payload"].clone())
    })?;
    let window = report["info"]["model_context_window"]
        .as_u64()
        .filter(|window| *window > 0);
    let used = report["info"]["last_token_usage"]["total_tokens"].as_u64();
    // Basis points keep the arithmetic in integers; a context cannot be over full.
    let context_percent = used.zip(window).and_then(|(used, window)| {
        let basis_points = (u128::from(used) * 10_000 / u128::from(window)).min(10_000);
        Some(f64::from(u32::try_from(basis_points).ok()?) / 100.0)
    });
    let mut signal = Signal {
        session: session.to_owned(),
        source: "codex".to_owned(),
        at,
        context_percent,
        context_window: window,
        five_hour_percent: None,
        five_hour_resets_at: None,
        seven_day_percent: None,
        seven_day_resets_at: None,
    };
    for name in ["primary", "secondary"] {
        let limit = &report["rate_limits"][name];
        let percent = percent(&limit["used_percent"]);
        let resets_at = limit["resets_at"].as_u64();
        // The long window is the weekly one; anything up to a day is the short one.
        if limit["window_minutes"]
            .as_u64()
            .is_some_and(|minutes| minutes > 1_440)
        {
            signal.seven_day_percent = percent;
            signal.seven_day_resets_at = resets_at;
        } else {
            signal.five_hour_percent = percent;
            signal.five_hour_resets_at = resets_at;
        }
    }
    Some(signal)
}

/// Records a Codex session's figures from its rollout file. Never fails the hook.
pub(crate) fn record_codex(paths: &Paths, session: &str, rollout: &std::path::Path) {
    let read = || -> Result<Vec<u8>> {
        let mut file = fs::File::open(rollout)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(ROLLOUT_TAIL)))?;
        let mut tail = Vec::new();
        file.take(ROLLOUT_TAIL).read_to_end(&mut tail)?;
        Ok(tail)
    };
    if let Some(signal) = read()
        .ok()
        .and_then(|tail| parse_codex_rollout(session, &tail, now()))
    {
        let _ = record(paths, &signal);
    }
}

/// Cursor splits its status line command into words itself and promises `~`
/// only for the command path, so a chained `~/script.sh` is expanded here.
fn expand_home(word: &str) -> String {
    match (word.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest).display().to_string(),
        _ => word.to_owned(),
    }
}

/// `brgr __context`: runs as a status line command. It records what it was
/// given, then runs `chain` with the same input so the user's own status line
/// still draws. Recording never fails the status line.
pub(crate) fn run(paths: &Paths, source: &str, chain: &[String]) -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(INPUT_LIMIT).read_to_end(&mut input)?;
    let signal = serde_json::from_slice::<Value>(&input)
        .ok()
        .and_then(|value| parse_status_line(&value, now(), source));
    if let Some(signal) = &signal {
        let _ = record(paths, signal);
    }
    let chain: Vec<String> = chain.iter().map(|word| expand_home(word)).collect();
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
        parse_status_line(&serde_json::from_str(text).unwrap(), 1_000, "claude")
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
    fn only_a_leading_tilde_slash_is_a_home_directory() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/bin/s.sh"), format!("{home}/bin/s.sh"));
        assert_eq!(expand_home("~other/s.sh"), "~other/s.sh");
        assert_eq!(expand_home("a/~/b"), "a/~/b");
        assert_eq!(expand_home("--flag"), "--flag");
    }

    #[test]
    fn a_plain_status_line_shows_what_was_measured() {
        let line = summary(&parse(CLAUDE).unwrap());
        assert_eq!(line, "ctx 6% · 5h 7% · 7d 43%");
        assert_eq!(summary(&parse(BEFORE_RESPONSE).unwrap()), "");
    }
}

#[cfg(test)]
mod codex_tests {
    use super::*;

    /// Two real `token_count` reports from Codex 0.162.1, cut down to the fields
    /// brgr reads.
    const ROLLOUT: &str =
        include_str!("../../../testdata/fixtures/codex-rollout/token-counts-0.162.1.jsonl");

    #[test]
    fn the_last_token_report_gives_the_context_in_use_and_both_plan_windows() {
        let mut tail = String::from("{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_mes");
        tail.push('\n');
        tail.push_str(ROLLOUT);
        tail.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\"}}\n");
        let signal = parse_codex_rollout("01a15cac-c100", tail.as_bytes(), 5).unwrap();
        assert_eq!(signal.source, "codex");
        assert_eq!(signal.context_window, Some(828_400));
        // The second report, not the first: 94224 / 828400.
        let percent = signal.context_percent.unwrap();
        assert!((percent - 11.37).abs() < 0.01, "{percent}");
        assert_eq!(signal.five_hour_percent, Some(24.0));
        assert_eq!(signal.seven_day_percent, Some(4.0));
        assert_eq!(signal.seven_day_resets_at, Some(1_793_084_253));
    }

    #[test]
    fn a_rollout_without_a_token_report_gives_nothing() {
        let tail = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\"}}\n";
        assert!(parse_codex_rollout("s", tail.as_bytes(), 5).is_none());
        // The first report of a session carries no usage yet.
        let empty =
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null}}\n";
        assert!(parse_codex_rollout("s", empty.as_bytes(), 5).is_none());
        assert!(parse_codex_rollout("../x", ROLLOUT.as_bytes(), 5).is_none());
    }
}
