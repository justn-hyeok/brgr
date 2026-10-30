//! Human-facing terminal output for `brgr run` and `brgr status`.
//!
//! Printed only when stdout is a terminal and `--json` was not given; a pipe
//! or a file gets the same pretty JSON as before, byte for byte. The fields
//! shown can carry text a worker or a prompt supplied, so every one is
//! neutralized for a terminal line before it prints.

use std::{fmt::Write as _, io::IsTerminal};

/// Whether the human form may print: only on a terminal, never under
/// `--json`. The terminal check is injected so tests pass it directly rather
/// than allocating a pty.
pub(crate) fn use_human_output(json_output: bool, stdout_is_terminal: bool) -> bool {
    !json_output && stdout_is_terminal
}

/// The real terminal check call sites use; kept beside [`use_human_output`]
/// so the only difference between a terminal and a test is this one line.
pub(crate) fn stdout_is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

/// Unix seconds now, for elapsed columns. A clock before the epoch reads as
/// zero, which the saturating subtraction below turns into `0s`.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Characters that steer a terminal or change what a line appears to say:
/// C0/C1 controls (ESC among them), bidi controls and isolates, and the
/// zero-width format characters. Each one becomes a plain space.
fn unsafe_display(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}' // Arabic letter mark: bidi control outside the LRM block
                | '\u{200b}'..='\u{200f}' // zero-width space, ZWNJ, ZWJ, LRM, RLM
                | '\u{202a}'..='\u{202e}' // bidi embeddings, overrides, PDF
                | '\u{2060}'..='\u{2064}' // word joiner and invisible operators
                | '\u{2066}'..='\u{2069}' // bidi isolates
                | '\u{feff}' // BOM / zero-width no-break space
        )
}

/// Untrusted text on one terminal line: every control or format character
/// (ESC included) becomes a space, so a worker string can never move the
/// cursor, recolor output, or rewrite what was already printed.
pub(crate) fn safe_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if unsafe_display(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// [`safe_text`] cut to `max_chars` on a char boundary; a cut earns an
/// ellipsis.
pub(crate) fn cell(text: &str, max_chars: usize) -> String {
    let text = safe_text(text);
    let mut characters = text.chars();
    let cut: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// `12s` / `4m` / `2h` since the first attempt started; `-` without a clock.
/// A clock change can put the start in the future: it clamps to `0s`, the
/// same convention the remaining-time column in `status --tree` uses.
pub(crate) fn elapsed_text(created_at: Option<u64>, now: u64) -> String {
    let Some(started) = created_at else {
        return "-".to_owned();
    };
    let elapsed = now.saturating_sub(started);
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3_600 {
        format!("{}m", elapsed / 60)
    } else {
        format!("{}h", elapsed / 3_600)
    }
}

/// The first eight characters of a task id, as `brgr status` shows it.
fn short_id(task_id: &str) -> String {
    task_id.chars().take(8).collect()
}

/// One `brgr status` table row, already read from the store.
pub(crate) struct StatusRow {
    pub task_id: String,
    pub state: String,
    pub harness: String,
    /// First attempt start in Unix seconds; `None` until an attempt exists.
    pub created_at: Option<u64>,
    pub objective: String,
}

/// The `brgr status TASK` block, already read from the store.
pub(crate) struct TaskDetail {
    pub task_id: String,
    pub state: String,
    pub harness: String,
    pub model: Option<String>,
    pub workspace: String,
    pub created_at: Option<u64>,
    /// Sealed result outcome; absent until a result exists.
    pub outcome: Option<String>,
    /// Questions awaiting an owner answer; absent when there are none.
    pub questions: Option<u64>,
}

/// What `brgr run` reports, from the same receipt the JSON form prints.
pub(crate) struct RunReceipt {
    pub state: String,
    /// The sealed outcome once a foreground run has finished.
    pub outcome: Option<String>,
    pub harness: String,
    pub workspace: String,
    pub task_id: String,
}

/// A display cell is capped so one hostile or huge field cannot stretch a
/// line forever.
const CELL_MAX: usize = 60;

/// Widths in characters of the widest cell in each column, headers included.
fn column_widths(widths: &[usize], cells: &[&str]) -> Vec<usize> {
    let mut widths = widths.to_vec();
    for (index, cell) in cells.iter().enumerate() {
        widths[index] = widths[index].max(cell.chars().count());
    }
    widths
}

fn pad(cell: &str, width: usize) -> String {
    let mut padded = cell.to_owned();
    for _ in cell.chars().count()..width {
        padded.push(' ');
    }
    padded
}

/// The `brgr status` list: a header and one line per task, `no tasks` when
/// the list is empty. `now` is injected so elapsed columns are testable.
pub(crate) fn status_table(rows: &[StatusRow], now: u64) -> String {
    if rows.is_empty() {
        return "no tasks\n".to_owned();
    }
    let headers = ["ID", "STATE", "HARNESS", "ELAPSED", "OBJECTIVE"];
    let mut widths = column_widths(&[0; 5], &headers);
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|row| {
            [
                short_id(&row.task_id),
                cell(&row.state, CELL_MAX),
                cell(&row.harness, CELL_MAX),
                elapsed_text(row.created_at, now),
                cell(&row.objective, CELL_MAX),
            ]
        })
        .collect();
    for row in &cells {
        widths = column_widths(&widths, &row.each_ref().map(String::as_str));
    }
    let mut output = String::new();
    for (index, header) in headers.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        output.push_str(&pad(header, widths[index]));
    }
    output.push('\n');
    for row in &cells {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                output.push_str("  ");
            }
            // The last column is never padded; the table ends with it.
            if index + 1 == row.len() {
                output.push_str(cell);
            } else {
                output.push_str(&pad(cell, widths[index]));
            }
        }
        output.push('\n');
    }
    output
}

/// The `brgr status TASK` block: key lines plus where to get the full JSON.
pub(crate) fn task_detail(detail: &TaskDetail, now: u64) -> String {
    let mut output = String::new();
    let mut line = |key: &str, value: &str| {
        let _ = writeln!(output, "{key}: {value}");
    };
    line("task", &detail.task_id);
    line("state", &safe_text(&detail.state));
    line("harness", &safe_text(&detail.harness));
    line(
        "model",
        &detail
            .model
            .as_deref()
            .map_or_else(|| "-".to_owned(), safe_text),
    );
    line("workspace", &safe_text(&detail.workspace));
    line("elapsed", &elapsed_text(detail.created_at, now));
    if let Some(outcome) = &detail.outcome {
        line("result", &safe_text(outcome));
    }
    if let Some(questions) = detail.questions {
        line("questions", &questions.to_string());
    }
    let _ = writeln!(output, "full detail: brgr status {} --json", detail.task_id);
    output
}

/// Prints a `brgr run` receipt in the right form for the current stdout:
/// the human block on a terminal, the JSON envelope everywhere else.
pub(crate) fn print_run_receipt(
    receipt: &serde_json::Value,
    human: &RunReceipt,
    json_output: bool,
) {
    if use_human_output(json_output, stdout_is_terminal()) {
        print!("{}", run_receipt(human));
    } else {
        crate::print_value(receipt, json_output);
    }
}

/// The `brgr run` receipt: a few key lines, ending with `task: <full id>`.
pub(crate) fn run_receipt(receipt: &RunReceipt) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "state: {}", safe_text(&receipt.state));
    if let Some(outcome) = &receipt.outcome {
        let _ = writeln!(output, "outcome: {}", safe_text(outcome));
    }
    let _ = writeln!(output, "harness: {}", safe_text(&receipt.harness));
    let _ = writeln!(output, "workspace: {}", safe_text(&receipt.workspace));
    // The last line is the machine-detectable id: `task: <full task id>`.
    let _ = writeln!(output, "task: {}", receipt.task_id);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        task_id: &str,
        state: &str,
        harness: &str,
        created_at: Option<u64>,
        objective: &str,
    ) -> StatusRow {
        StatusRow {
            task_id: task_id.to_owned(),
            state: state.to_owned(),
            harness: harness.to_owned(),
            created_at,
            objective: objective.to_owned(),
        }
    }

    #[test]
    fn human_output_needs_a_terminal_and_no_json_flag() {
        assert!(use_human_output(false, true));
        assert!(!use_human_output(true, true));
        assert!(!use_human_output(false, false));
        assert!(!use_human_output(true, false));
    }

    #[test]
    fn an_empty_status_list_says_no_tasks() {
        assert_eq!(status_table(&[], 1_000), "no tasks\n");
    }

    #[test]
    fn the_status_table_is_a_header_plus_one_line_per_task() {
        let rows = vec![
            row(
                "2b2c279a-b8e1-43a8-846c-cb8344a301d7",
                "terminal",
                "local.gjc",
                Some(940),
                "Review this change",
            ),
            row(
                "aaaaaaaa-1111-4111-8111-111111111111",
                "running",
                "local.opencode",
                Some(1_000),
                "Second task",
            ),
        ];
        let text = status_table(&rows, 1_000);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("ID"), "{text}");
        assert!(lines[0].contains("STATE"), "{text}");
        assert!(lines[0].contains("HARNESS"), "{text}");
        assert!(lines[0].contains("ELAPSED"), "{text}");
        assert!(lines[0].contains("OBJECTIVE"), "{text}");
        assert_eq!(&lines[1][..8], "2b2c279a");
        assert!(lines[1].contains("terminal"));
        assert!(lines[1].contains("local.gjc"));
        assert!(lines[1].contains("1m"));
        assert!(lines[1].contains("Review this change"));
        assert_eq!(&lines[2][..8], "aaaaaaaa");
        assert!(lines[2].contains("0s"), "{text}");
        assert!(!text.contains("2b2c279a-b8e1-43a8-846c-cb8344a301d7"));
    }

    #[test]
    fn elapsed_is_dash_without_a_clock_then_seconds_minutes_hours() {
        assert_eq!(elapsed_text(None, 1_000), "-");
        assert_eq!(elapsed_text(Some(988), 1_000), "12s");
        assert_eq!(elapsed_text(Some(0), 12), "12s");
        assert_eq!(elapsed_text(Some(760), 1_000), "4m");
        assert_eq!(elapsed_text(Some(0), 7_200), "2h");
        assert_eq!(elapsed_text(Some(0), 86_400), "24h");
        // A clock change can put the start in the future; it clamps, never
        // wraps or reports a negative age.
        assert_eq!(elapsed_text(Some(2_000), 1_000), "0s");
    }

    #[test]
    fn the_objective_cell_cuts_at_sixty_chars_with_an_ellipsis() {
        let exact = "a".repeat(60);
        assert_eq!(cell(&exact, 60), exact);
        let over = "b".repeat(61);
        let shown = cell(&over, 60);
        assert_eq!(shown.chars().count(), 61, "{shown}");
        assert!(shown.ends_with('…'));
        assert_eq!(&shown[..60], &over[..60]);
    }

    #[test]
    fn the_objective_cell_does_not_split_a_multibyte_character() {
        let mut objective = "가".repeat(61);
        let shown = cell(&objective, 60);
        assert_eq!(shown.chars().count(), 61);
        assert_eq!(&shown[..180], &objective[..180]);
        objective.insert(0, 'é');
        let shown = cell(&objective, 60);
        assert_eq!(shown.chars().count(), 61);
        assert!(shown.starts_with('é'));
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn the_arabic_letter_mark_cannot_reorder_a_line() {
        // U+061C ALM is a bidi control in the same class as LRM/RLM: an
        // invisible character that changes how the terminal orders the line.
        let text = safe_text("owner\u{061c}name");
        assert!(!text.contains('\u{061c}'), "{text:?}");
        assert_eq!(text, "owner name");
    }

    #[test]
    fn untrusted_text_loses_every_terminal_control_character() {
        assert_eq!(safe_text("plain"), "plain");
        assert_eq!(safe_text("a\nb\tc"), "a b c");
        assert_eq!(safe_text("ok\u{1b}[31mred"), "ok [31mred");
        assert_eq!(safe_text("\u{1b}[2Jhidden"), " [2Jhidden");
        // Bidi overrides and zero-width format characters are as hostile as
        // ESC on a terminal line.
        assert_eq!(safe_text("a\u{202e}b"), "a b");
        assert_eq!(safe_text("a\u{200b}b"), "a b");
        assert_eq!(safe_text("a\u{2066}b"), "a b");
        assert_eq!(safe_text("a\u{feff}b"), "a b");
        // A C0/C1 sweep leaves only spaces behind.
        let hostile: String = (0_u32..0xa0)
            .filter_map(char::from_u32)
            .filter(|c| c.is_control())
            .collect();
        assert!(safe_text(&hostile).chars().all(|c| c == ' '));
    }

    #[test]
    fn a_cell_is_sanitized_before_it_is_cut() {
        // The escape never reaches the terminal even when the cut lands
        // mid-sequence, and control characters count toward the 60.
        let hostile = "\u{1b}[2J".repeat(20);
        let shown = cell(&hostile, 60);
        assert!(!shown.contains('\u{1b}'));
        assert!(shown.contains("[2J"), "{shown}");
        assert_eq!(shown.chars().count(), 61, "{shown}");
    }

    #[test]
    fn task_detail_lists_the_summary_then_points_at_json() {
        let detail = TaskDetail {
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
            state: "terminal".to_owned(),
            harness: "local.gjc".to_owned(),
            model: Some("swe-1.6".to_owned()),
            workspace: "/tmp/work tree".to_owned(),
            created_at: Some(940),
            outcome: Some("candidate".to_owned()),
            questions: Some(2),
        };
        let text = task_detail(&detail, 1_000);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "task: 2b2c279a-b8e1-43a8-846c-cb8344a301d7");
        assert_eq!(lines[1], "state: terminal");
        assert_eq!(lines[2], "harness: local.gjc");
        assert_eq!(lines[3], "model: swe-1.6");
        assert_eq!(lines[4], "workspace: /tmp/work tree");
        assert_eq!(lines[5], "elapsed: 1m");
        assert_eq!(lines[6], "result: candidate");
        assert_eq!(lines[7], "questions: 2");
        assert_eq!(
            lines[8],
            "full detail: brgr status 2b2c279a-b8e1-43a8-846c-cb8344a301d7 --json"
        );
        assert_eq!(lines.len(), 9, "{text}");
    }

    #[test]
    fn task_detail_omits_missing_model_result_and_questions() {
        let detail = TaskDetail {
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
            state: "running".to_owned(),
            harness: "local.gjc".to_owned(),
            model: None,
            workspace: "/tmp/work".to_owned(),
            created_at: None,
            outcome: None,
            questions: None,
        };
        let text = task_detail(&detail, 1_000);
        assert!(text.contains("model: -"));
        assert!(text.contains("elapsed: -"));
        assert!(!text.contains("result:"));
        assert!(!text.contains("questions:"));
        assert!(
            text.ends_with(
                "full detail: brgr status 2b2c279a-b8e1-43a8-846c-cb8344a301d7 --json\n"
            )
        );
    }

    #[test]
    fn task_detail_sanitizes_untrusted_fields() {
        let detail = TaskDetail {
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
            state: "running".to_owned(),
            harness: "local.gjc\u{1b}[31m".to_owned(),
            model: None,
            workspace: "/tmp/work\n/etc".to_owned(),
            created_at: None,
            outcome: None,
            questions: None,
        };
        let text = task_detail(&detail, 1_000);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("harness: local.gjc [31m"));
        assert!(text.contains("workspace: /tmp/work /etc"));
    }

    #[test]
    fn run_receipt_ends_with_the_full_task_id() {
        let receipt = RunReceipt {
            state: "starting".to_owned(),
            outcome: None,
            harness: "local.gjc".to_owned(),
            workspace: "/tmp/work".to_owned(),
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
        };
        let text = run_receipt(&receipt);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "state: starting");
        assert_eq!(lines[1], "harness: local.gjc");
        assert_eq!(lines[2], "workspace: /tmp/work");
        assert_eq!(
            lines.last(),
            Some(&"task: 2b2c279a-b8e1-43a8-846c-cb8344a301d7")
        );
    }

    #[test]
    fn run_receipt_shows_a_finished_runs_outcome() {
        let receipt = RunReceipt {
            state: "terminal".to_owned(),
            outcome: Some("candidate".to_owned()),
            harness: "local.gjc".to_owned(),
            workspace: "/tmp/work".to_owned(),
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
        };
        let text = run_receipt(&receipt);
        assert!(text.contains("outcome: candidate"), "{text}");
        assert!(text.ends_with("task: 2b2c279a-b8e1-43a8-846c-cb8344a301d7\n"));
    }

    #[test]
    fn run_receipt_sanitizes_workspace_and_harness() {
        let receipt = RunReceipt {
            state: "starting".to_owned(),
            outcome: None,
            harness: "bad\u{1b}[31m".to_owned(),
            workspace: "/tmp/a\u{202e}b".to_owned(),
            task_id: "2b2c279a-b8e1-43a8-846c-cb8344a301d7".to_owned(),
        };
        let text = run_receipt(&receipt);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(text.contains("harness: bad [31m"), "{text}");
        assert!(text.contains("workspace: /tmp/a b"), "{text}");
        assert!(text.ends_with("task: 2b2c279a-b8e1-43a8-846c-cb8344a301d7\n"));
    }
}
