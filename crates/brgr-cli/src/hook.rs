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
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: HookInput = serde_json::from_str(&input).unwrap_or(HookInput { session_id: None });
    let Some(session_id) = input.session_id else {
        println!("{{}}");
        return Ok(());
    };
    let owner = OwnerId::new(format!("codex:{session_id}"))?;
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
            notification::register_current_surface(&store, &owner, &session_id),
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
        println!("{{}}");
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
