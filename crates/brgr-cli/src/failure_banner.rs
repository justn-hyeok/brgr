//! A failed run must not wait for its owner to ask about it.
//!
//! Every brgr command an owner runs ends with a note on stderr for each failed
//! or lost result it has not acknowledged. An agent that runs brgr from its
//! shell sees stderr in the same tool output as the command's own answer, so the
//! failure reaches it on its very next brgr call whether or not its pane could
//! be prompted. The note repeats until the owner acknowledges the result.

use std::fmt::Write as _;

use brgr_protocol::{InboxItem, TerminalOutcome};
use brgr_store::Store;

use crate::Paths;

const LISTED: usize = 3;
const REASON_LIMIT: usize = 160;

/// Prints the note, if any. Never fails the command it follows.
pub(crate) fn print(paths: &Paths) {
    let Ok(Some(session)) = crate::current_session() else {
        return;
    };
    let Ok(owner) = crate::owner_from_environment() else {
        return;
    };
    let Ok(store) = Store::open(&paths.store) else {
        return;
    };
    // Only a bound owner has results to be told about.
    if store
        .owner_binding(&owner)
        .ok()
        .flatten()
        .is_none_or(|(bound, _)| bound != session)
    {
        return;
    }
    if let Some(note) = store
        .inbox(&owner, false)
        .ok()
        .and_then(|items| note(&items))
    {
        eprintln!("{note}");
    }
}

fn note(items: &[InboxItem]) -> Option<String> {
    let failed: Vec<_> = items
        .iter()
        .filter(|item| {
            !item.acknowledged
                && matches!(
                    item.result.outcome,
                    TerminalOutcome::Failed | TerminalOutcome::Lost
                )
        })
        .collect();
    if failed.is_empty() {
        return None;
    }
    let mut out = format!(
        "brgr: {} run(s) failed or were lost and are not acknowledged:\n",
        failed.len()
    );
    for item in failed.iter().take(LISTED) {
        let result = &item.result;
        let outcome = if result.outcome == TerminalOutcome::Lost {
            "lost"
        } else {
            "failed"
        };
        let reason: String = result
            .error
            .as_deref()
            .unwrap_or("no error text")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(REASON_LIMIT)
            .collect();
        let _ = writeln!(out, "  {} {outcome}: {reason}", result.task_id);
    }
    if failed.len() > LISTED {
        let _ = writeln!(out, "  and {} more", failed.len() - LISTED);
    }
    out.push_str(
        "  Inspect: brgr result TASK. Retry: brgr revise TASK \"corrected request\". \
         Dismiss: brgr result TASK --ack.",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{AttemptId, OwnerId, ResultEnvelope, ResultId, TaskId};

    fn item(outcome: TerminalOutcome, error: &str, acknowledged: bool) -> InboxItem {
        InboxItem {
            owner_id: OwnerId::new("claude:test").unwrap(),
            result: ResultEnvelope {
                schema: "brgr/v1".to_owned(),
                task_id: TaskId::new(),
                revision: 1,
                attempt_id: AttemptId::new(),
                result_id: ResultId::new(),
                outcome,
                artifacts: vec![],
                error: Some(error.to_owned()),
                legacy_embedded_route_observation: None,
                route_observation: None,
                unresolved_effects: vec![],
            },
            acknowledged,
        }
    }

    #[test]
    fn failed_and_lost_results_are_named_with_their_reason() {
        let items = [
            item(TerminalOutcome::Lost, "task pane no longer exists", false),
            item(TerminalOutcome::Failed, "agent deadline elapsed", false),
        ];
        let text = note(&items).unwrap();
        assert!(text.contains("2 run(s) failed or were lost"), "{text}");
        assert!(text.contains("lost: task pane no longer exists"), "{text}");
        assert!(text.contains("failed: agent deadline elapsed"), "{text}");
        assert!(
            text.contains(&items[0].result.task_id.to_string()),
            "{text}"
        );
        assert!(text.contains("brgr result TASK --ack"), "{text}");
    }

    #[test]
    fn candidates_cancellations_and_acknowledged_failures_stay_quiet() {
        let items = [
            item(TerminalOutcome::Candidate, "", false),
            item(TerminalOutcome::Cancelled, "cancelled", false),
            item(TerminalOutcome::Failed, "old", true),
        ];
        assert!(note(&items).is_none());
    }

    #[test]
    fn a_long_list_is_cut_to_three_and_counted() {
        let items: Vec<_> = (0..5)
            .map(|_| item(TerminalOutcome::Failed, "x", false))
            .collect();
        let text = note(&items).unwrap();
        assert_eq!(text.matches(" failed: x").count(), 3, "{text}");
        assert!(text.contains("and 2 more"), "{text}");
    }
}
