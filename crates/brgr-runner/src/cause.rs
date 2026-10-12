//! The harness's own words for why a run failed, bounded and flattened.
//!
//! The text is untrusted data. It is cut to a fixed length with control
//! characters removed, and callers mark it as a quote so nothing downstream
//! treats it as an instruction or copies it into a shared record.

use serde_json::Value;

use crate::ExecutionOutput;

const LIMIT: usize = 300;
/// JSONL lines scanned for an error event; a failing run reports it early.
const MAX_LINES: usize = 4_000;
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 6;
const SIGNS: [&str; 9] = [
    "error",
    "quota",
    "insufficient",
    "denied",
    "unauthorized",
    "forbidden",
    "not logged in",
    "rate limit",
    "not available",
];

/// Where a quoted failure cause came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CauseSource {
    Stderr,
    /// An `errorMessage` or `type: "error"` event on stdout, for harnesses
    /// that report failures in their JSONL stream and leave stderr empty.
    ErrorEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureCause {
    pub source: CauseSource,
    pub text: String,
}

impl ExecutionOutput {
    /// The stderr line most likely to say why a run failed, else the first
    /// error event on stdout.
    #[must_use]
    pub fn failure_cause(&self) -> Option<FailureCause> {
        stderr_line(&self.stderr)
            .map(|text| FailureCause {
                source: CauseSource::Stderr,
                text,
            })
            .or_else(|| {
                error_event(&self.stdout).map(|text| FailureCause {
                    source: CauseSource::ErrorEvent,
                    text,
                })
            })
    }
}

/// The first stderr line that reads like an error and is not a stack frame,
/// else the last non-empty one.
fn stderr_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = lines
        .iter()
        .find(|line| {
            let lower = line.to_lowercase();
            !line.starts_with("at ") && SIGNS.iter().any(|sign| lower.contains(sign))
        })
        .or_else(|| lines.last())?;
    flatten(line)
}

fn error_event(stdout: &[u8]) -> Option<String> {
    stdout
        .split(|byte| *byte == b'\n')
        .take(MAX_LINES)
        .filter(|line| line.len() <= MAX_LINE_BYTES && line.starts_with(b"{"))
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|event| event_message(&event, 0))
        .and_then(|message| flatten(&message))
}

fn event_message(value: &Value, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(message)) = map.get("errorMessage")
                && !message.trim().is_empty()
            {
                return Some(message.clone());
            }
            if map.get("type").and_then(Value::as_str) == Some("error") {
                for key in ["message", "error"] {
                    if let Some(Value::String(message)) = map.get(key)
                        && !message.trim().is_empty()
                    {
                        return Some(message.clone());
                    }
                }
            }
            map.values()
                .find_map(|child| event_message(child, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| event_message(child, depth + 1)),
        _ => None,
    }
}

/// One line, control characters and runs of whitespace collapsed, cut at
/// `LIMIT` characters.
fn flatten(text: &str) -> Option<String> {
    let flat: String = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let flat: String = flat.chars().take(LIMIT).collect();
    (!flat.is_empty()).then_some(flat)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn output(stdout: &str, stderr: &str) -> ExecutionOutput {
        ExecutionOutput {
            exit_code: Some(1),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            result: Vec::new(),
            observed_model: None,
            timed_out: false,
            cancelled: false,
            output_truncated: false,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn a_json_stream_failure_with_empty_stderr_quotes_the_error_event() {
        // GJC 0.18.9 on a spent provider quota: exit 1, nothing on stderr.
        let stdout = concat!(
            "{\"type\":\"session\",\"id\":\"x\"}\n",
            "{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\",\"content\":[],",
            "\"stopReason\":\"error\",\"errorMessage\":\"Connect error resource_exhausted: Error\"}}\n",
            "{\"type\":\"agent_end\",\"messages\":[]}\n",
        );
        assert_eq!(
            output(stdout, "").failure_cause(),
            Some(FailureCause {
                source: CauseSource::ErrorEvent,
                text: "Connect error resource_exhausted: Error".to_owned(),
            })
        );
    }

    #[test]
    fn stderr_wins_over_a_stdout_error_event() {
        let stdout = "{\"errorMessage\":\"from stdout\"}\n";
        let cause = output(stdout, "Error: from stderr\n")
            .failure_cause()
            .unwrap();
        assert_eq!(cause.source, CauseSource::Stderr);
        assert_eq!(cause.text, "Error: from stderr");
    }

    #[test]
    fn a_cause_is_one_bounded_line_without_control_characters() {
        let nasty = format!(
            "{{\"errorMessage\":\"a\\u001b[31m\\nb\\t{}\"}}\n",
            "x".repeat(900)
        );
        let cause = output(&nasty, "").failure_cause().unwrap();
        assert!(cause.text.chars().count() <= LIMIT);
        assert!(cause.text.starts_with("a [31m b x"), "{}", cause.text);
        assert!(!cause.text.chars().any(char::is_control));
    }

    #[test]
    fn plain_output_without_an_error_has_no_cause() {
        assert_eq!(
            output("{\"type\":\"agent_end\"}\nnot json\n", "").failure_cause(),
            None
        );
    }
}
