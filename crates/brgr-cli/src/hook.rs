//! The Codex lifecycle hook: surfaces a session's pending results.

use std::{
    io::{self, Read as _},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use brgr_protocol::OwnerId;
use brgr_store::Store;
use serde_json::json;

use crate::{HookInput, Paths, cli::HookEvent, notification, supervision::reconcile_pending};

pub(crate) async fn hook(paths: &Paths, event: HookEvent) -> Result<()> {
    crate::invocation::mark_hook();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: HookInput = serde_json::from_str(&input).unwrap_or(HookInput {
        session_id: None,
        cwd: None,
    });
    // A Codex worker runs in a brgr task worktree, and Codex runs every
    // installed hook for it too. This hook is for owners: binding the worker's
    // session, or telling it to call brgr as an owner, would mix the two up.
    if input
        .cwd
        .as_deref()
        .is_some_and(|cwd| in_task_worktree(paths, cwd))
    {
        println!("{{}}");
        return Ok(());
    }
    let Some(session_id) = input.session_id else {
        println!("{{}}");
        return Ok(());
    };
    let owner = OwnerId::new(format!("codex:{session_id}"))?;
    let source = crate::caller_pane::for_session(&session_id, None);
    crate::invocation::capture_hook(session_id.clone(), source.clone());
    reconcile_pending(paths)?;
    let store = Store::open(&paths.store)?;
    if event == HookEvent::SessionStart {
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        store.bind_owner(&owner, &session_id, epoch.max(1))?;
    }
    let surface_ready = if event == HookEvent::Stop {
        false
    } else {
        match tokio::time::timeout(
            Duration::from_millis(500),
            notification::register_current_surface(paths, &store, &owner, &session_id),
        )
        .await
        {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                eprintln!("brgr completion notification remains queued: {error}");
                false
            }
            Err(_) => {
                eprintln!("brgr completion notification remains queued: Herdr lookup timed out");
                false
            }
        }
    };
    if surface_ready {
        for task in store.pending_notification_tasks_for_session(&session_id)? {
            if let Err(error) = notification::spawn_for_task(paths, task) {
                eprintln!("brgr completion notification remains queued: {error}");
            }
        }
    }
    let pending = store.pending_for_session(&session_id)?;
    if pending.is_empty() {
        if event == HookEvent::Stop {
            println!("{{}}");
        } else {
            // The pane is never named here: brgr works it out from what the panes
            // show, because the hook's own idea of the pane can be another
            // session's.
            println!(
                "{}",
                json!({"hookSpecificOutput":{"hookEventName":format!("{event:?}"),"additionalContext":format!("brgr calling context for this session: brgr --as {session_id}. Put exactly these options right after `brgr` in every brgr command, and do not add --source-pane.")}})
            );
        }
        return Ok(());
    }
    let handles = pending
        .iter()
        .take(10)
        .map(|item| format!("{}:{:?}", item.result.task_id, item.result.outcome))
        .collect::<Vec<_>>()
        .join(", ");
    let summary = format!("{} pending result(s): {handles}", pending.len());
    match event {
        // Codex accepts `hookSpecificOutput` only for the events that define
        // one, and Stop does not: an object carrying it was rejected whole as
        // "invalid stop hook JSON output", so the block never took effect.
        HookEvent::Stop => println!(
            "{}",
            json!({
                "decision": "block",
                "reason": format!(
                    "brgr has unprocessed terminal results. Pending brgr inbox: {summary}. \
                     Use brgr result TASK, then accept/reject or ack."
                ),
            })
        ),
        HookEvent::SessionStart | HookEvent::UserPromptSubmit => println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": format!("{event:?}"),
                    "additionalContext": format!("Pending brgr inbox: {summary}. Verify each result, then run brgr accept/reject; acknowledge non-candidate outcomes with brgr result TASK --ack.")
                }
            })
        ),
    }
    Ok(())
}

/// Whether a working directory lies in one of this control home's task
/// worktrees.
fn in_task_worktree(paths: &Paths, cwd: &std::path::Path) -> bool {
    let Ok(root) = paths.worktrees.canonicalize() else {
        return false;
    };
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .starts_with(root)
}
