//! The worker prompt: task text, owner messaging, and delegation.

use std::fmt::Write as _;

use brgr_protocol::TaskInstructions;

use crate::{DelegationContext, RunRequest};

pub(crate) fn render_task_prompt(request: &RunRequest<'_>) -> String {
    let criteria = request.criteria.unwrap_or_default();
    let instructions = request.instructions;
    if criteria.is_empty() && instructions.is_none_or(TaskInstructions::is_empty) {
        return request.prompt.to_owned();
    }
    let mut rendered = format!(
        "OBJECTIVE\n{}\n\nTASK WORKSPACE\n{}",
        request.prompt,
        request.workspace.display()
    );
    if !criteria.is_empty() {
        rendered.push_str("\n\nACCEPTANCE CRITERIA");
        for (index, criterion) in criteria.iter().enumerate() {
            let _ = write!(rendered, "\n{}. {criterion}", index + 1);
        }
    }
    if let Some(instructions) = instructions {
        if !instructions.scope.is_empty() {
            rendered.push_str("\n\nSCOPE");
            for item in &instructions.scope {
                let _ = write!(rendered, "\n- {item}");
            }
        }
        if !instructions.role.is_empty() {
            rendered.push_str("\n\nROLE INSTRUCTIONS");
            for item in &instructions.role {
                let _ = write!(rendered, "\n- {item}");
            }
        }
    }
    rendered
}

/// The prompt a worker receives: the task alone when brgr is not managing the
/// run, otherwise the task with the brief for what this worker may do.
pub(crate) fn worker_prompt(
    context: Option<&DelegationContext<'_>>,
    task_prompt: String,
) -> String {
    match context {
        None => task_prompt,
        Some(context) if context.may_delegate => delegation_prompt(context, &task_prompt),
        Some(_) => messaging_prompt(&task_prompt),
    }
}

/// How any managed worker asks its owner, shared by both worker briefs.
pub(crate) const OWNER_MESSAGING: &str = r#"For a question to your owner, use "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body <question>. Read the reply with "$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds <limit>, then ack that message. Check "$BRGR_BIN" --json message list "$BRGR_PARENT_TASK_ID" --for worker at natural checkpoints for owner follow-ups. If your owner asks a question, ack it after reading and send a reply --to owner --kind reply --reply-to <message-id>."#;

/// The brief for a worker that may ask its owner but not delegate.
///
/// Appended rather than prepended: the task's own first line is a contract
/// with some harnesses (an OMP objective must begin `FROM CODEX`), and the
/// worker that was never offered this could not ask at all — the store of the
/// author's machine held zero messages across 57 tasks.
pub(crate) fn messaging_prompt(objective: &str) -> String {
    format!(
        "{objective}\n\nBRGR OWNER MESSAGES\nIf you need a decision or information only your owner has, ask rather than guess. {OWNER_MESSAGING} This task may not start child tasks."
    )
}

pub(crate) fn delegation_prompt(context: &DelegationContext<'_>, objective: &str) -> String {
    format!(
        r#"BRGR WORKER CONTEXT
You may delegate bounded subtasks with "$BRGR_BIN" --json run <objective> --harness <id> --criterion <check> only when your TASK explicitly asks for a child. A leaf task must not delegate. Each child belongs to this exact task attempt. Wait for an asynchronous child with "$BRGR_BIN" --json wait <child-task-id> --timeout-seconds <limit>. Inspect its sealed bytes with "$BRGR_BIN" --json result <child-task-id>, then accept or reject a candidate with a reason, or acknowledge a failed/lost result. Settle every child before reporting your own result. Brgr handles Herdr pane placement.

{OWNER_MESSAGING}
For a child question, use "$BRGR_BIN" --json message wait <child-task-id> --for owner --timeout-seconds <limit>. Reply with message send <child-task-id> --to worker --kind reply --reply-to <message-id> --body <answer>, then ack the question. A message ack is not a result decision.
Do not launch a second copy after an uncertain response; inspect task status first.
Parent task: {}
Parent attempt: {}

TASK
{}"#,
        context.task_id, context.attempt_id, objective
    )
}
