use crate::{Paths, message::MessageKindArg, print_value, require_owner};
use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, OwnerId, TaskId};
use brgr_store::{PeerDraft, Store, StoreError};
use clap::Subcommand;
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Subcommand)]
pub(crate) enum DebateCommand {
    /// Start an explicit peer conversation for sibling tasks.
    Start {
        /// Two or more task ids to join in the conversation.
        #[arg(required=true,num_args=2..)]
        tasks: Vec<TaskId>,
        /// Stable idempotency key for a repeated start request.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Show members, unread messages, and conversation history.
    Status {
        /// Debate group UUID.
        group: String,
    },
    /// Stop the conversation while retaining its history.
    Stop {
        /// Debate group UUID.
        group: String,
    },
    /// Send a peer message to another member.
    Send {
        /// Debate group UUID.
        group: String,
        /// Sender task; defaults to the current worker task.
        #[arg(long)]
        from: Option<TaskId>,
        /// Recipient member task.
        #[arg(long)]
        to: TaskId,
        /// Message body.
        #[arg(long)]
        body: String,
        /// Message kind.
        #[arg(long, value_enum, default_value = "note")]
        kind: MessageKindArg,
        /// Question message id this reply answers.
        #[arg(long)]
        reply_to: Option<String>,
        /// Stable idempotency key for a repeated send request.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// List unread peer messages for a member.
    List {
        /// Member task; defaults to the current worker task.
        task: Option<TaskId>,
    },
    /// Wait for a peer message; exits early if an owner message needs attention.
    Wait {
        /// Member task; defaults to the current worker task.
        task: Option<TaskId>,
        /// Maximum wait duration in seconds.
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    /// Acknowledge a peer message as read.
    Ack {
        /// Peer message id.
        message: String,
        /// Member task; defaults to the current worker task.
        task: Option<TaskId>,
    },
}

fn worker_task(explicit: Option<TaskId>) -> Result<TaskId> {
    let inherited = crate::worker_identity::task()
        .map(|id| id.parse::<TaskId>())
        .transpose()?;
    if inherited.is_some() && explicit.is_some() && inherited != explicit {
        bail!("debate sender differs from this worker task");
    }
    explicit
        .or(inherited)
        .context("provide a task or call from a managed worker")
}

fn authorize(store: &Store, task: TaskId) -> Result<(AttemptId, Option<brgr_store::OwnerProof>)> {
    if crate::worker_identity::task().as_deref() == Some(task.to_string().as_str()) {
        let attempt = crate::worker_identity::attempt()
            .context("worker attempt is missing")?
            .parse()?;
        if store.active_message_attempt(task)? != attempt {
            bail!("worker attempt differs from debate participant");
        }
        return Ok((attempt, None));
    }
    let owner_id = store.task(task)?.owner_id;
    let (session_id, binding_epoch) = require_owner(store, &owner_id)?;
    let proof = brgr_store::OwnerProof::new(owner_id, session_id, binding_epoch)?;
    Ok((store.active_message_attempt(task)?, Some(proof)))
}

fn status_value(
    paths: &Paths,
    store: &Store,
    group: &brgr_store::DebateGroup,
    messages: &[brgr_store::PeerMessage],
) -> Result<serde_json::Value> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .cast_signed();
    let members = group.members.iter().map(|task| {
        let attempt = store.debate_member_attempt(&group.id, *task)?;
        let state = store.attempt_state_by_id(attempt)?;
        let (unread_count, oldest) = store.peer_unread_summary(&group.id, *task)?;
        let marker = find_wait_marker(&paths.runs.join("debate-waits"), &task.to_string());
        Ok(serde_json::json!({
            "task": task,
            "unread_count": unread_count,
            "oldest_unread_seconds": oldest.map(|at| (now - at).max(0)),
            "worker_state": if state == brgr_protocol::AttemptState::Terminal {"finished"} else if matches!(state, brgr_protocol::AttemptState::Running | brgr_protocol::AttemptState::Blocked) {"running"} else {"gone"},
            "blocking_wait_seconds": marker.map(|started| now.saturating_sub(started).max(0)),
        }))
    }).collect::<Result<Vec<_>>>()?;
    Ok(
        serde_json::json!({"group":group,"members":members,"conversation_over":members.iter().all(|member| member["worker_state"] != "running"),"messages":messages}),
    )
}

fn find_wait_marker(dir: &std::path::Path, task: &str) -> Option<i64> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid_text) = name
            .strip_prefix(&format!("{task}-"))
            .and_then(|tail| tail.strip_suffix(".json"))
        else {
            continue;
        };
        let Ok(pid) = pid_text.parse::<i32>() else {
            continue;
        };
        if !std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
        {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        let marker = std::fs::read_to_string(entry.path()).ok()?;
        let mut lines = marker.lines();
        let started = lines.next()?.parse().ok()?;
        let recorded_signature = lines.next()?;
        if crate::message::process_signature(pid).as_deref() != Some(recorded_signature) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        return Some(started);
    }
    None
}

fn start_debate(
    store: &Store,
    tasks: &[TaskId],
    request_id: &str,
) -> Result<brgr_store::DebateGroup> {
    let mut owner_proof = None;
    let mut owner_id = None;
    for task in tasks {
        let task_owner = store.task(*task)?.owner_id;
        if owner_id.as_ref().is_some_and(|owner| owner != &task_owner) {
            bail!("debate tasks must belong to one owner");
        }
        let (session_id, binding_epoch) = require_owner(store, &task_owner)?;
        if owner_proof.is_none() {
            owner_proof = Some(brgr_store::OwnerProof::new(
                task_owner.clone(),
                session_id,
                binding_epoch,
            )?);
        }
        owner_id = Some(task_owner);
    }
    let proof = owner_proof.context("debate requires at least one owner")?;
    let group = store.create_debate_bound(request_id, tasks, &proof)?;
    if !group.active {
        return Ok(group);
    }
    let body = format!(
        "FROM BRGR DEBATE GROUP\n{}\nYou joined this explicitly requested peer conversation. Use brgr debate send {} --to TASK --kind question --body TEXT; replies use --kind reply --reply-to MESSAGE_ID. Receive with debate list/wait and acknowledge with debate ack MESSAGE_ID. If brgr message wait returns state debate_message_pending, run brgr debate list, then wait again.",
        serde_json::to_string(&group)?,
        group.id
    );
    for task in tasks {
        let attempt = store.active_message_attempt(*task)?;
        let mut invitation = brgr_store::MessageDraft::new(
            *task,
            attempt,
            brgr_store::MessageDirection::OwnerToWorker,
            brgr_store::MessageKind::Note,
            body.clone(),
            None,
        );
        let digest = Sha256::digest(format!("brgr-debate-invitation/{}/{task}", group.id));
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        invitation.message_id = uuid::Uuid::from_bytes(bytes).to_string();
        store.post_message_bound(&invitation, &proof)?;
    }
    Ok(group)
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run(paths: &Paths, command: DebateCommand, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let stopping = matches!(&command, DebateCommand::Stop { .. });
    let waiting = matches!(&command, DebateCommand::Wait { .. });
    match command {
        DebateCommand::Start { tasks, request_id } => {
            let group = start_debate(
                &store,
                &tasks,
                &request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            )?;
            print_value(&serde_json::to_value(group)?, json_output);
        }
        DebateCommand::Status { group } | DebateCommand::Stop { group } => {
            let record = store.debate(&group)?;
            let owner_id = OwnerId::new(record.owner.clone())?;
            let (session_id, binding_epoch) = require_owner(&store, &owner_id)?;
            if stopping {
                let proof = brgr_store::OwnerProof::new(owner_id, session_id, binding_epoch)?;
                store.stop_debate_bound(&group, &proof)?;
            }
            print_value(
                &status_value(
                    paths,
                    &store,
                    &store.debate(&group)?,
                    &store.debate_history(&group)?,
                )?,
                json_output,
            );
        }
        DebateCommand::Send {
            group,
            from,
            to,
            body,
            kind,
            reply_to,
            request_id,
        } => {
            let from = worker_task(from)?;
            let (attempt, proof) = authorize(&store, from)?;
            match store.active_message_attempt(to) {
                Ok(_) => {}
                Err(StoreError::TaskMessageNotFound(_)) => {
                    bail!(
                        "cannot send debate message to {to}: that member has finished or is gone"
                    );
                }
                Err(error) => return Err(error.into()),
            }
            let draft = PeerDraft {
                id: request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                group,
                from,
                to,
                kind: kind.into(),
                body,
                reply_to,
            };
            let message = if let Some(proof) = proof {
                store.post_peer_message_from_bound(&draft, attempt, &proof)?
            } else {
                store.post_peer_message_from(&draft, attempt)?
            };
            print_value(&serde_json::to_value(message)?, json_output);
        }
        DebateCommand::List { task } | DebateCommand::Wait { task, .. } => {
            let task = worker_task(task)?;
            let (attempt, proof) = authorize(&store, task)?;
            let _wait_marker = if waiting && proof.is_none() {
                Some(crate::message::WaitMarker::create(paths, task)?)
            } else {
                None
            };
            let seconds = if let DebateCommand::Wait {
                timeout_seconds, ..
            } = command
            {
                timeout_seconds
            } else {
                0
            };
            if seconds > 86400 {
                bail!("debate wait exceeds 86400 seconds");
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
            // A busy pane cannot show the notice that a debate started, so a wait
            // that began before it says so itself.
            let joined_at_start = store.active_debate_for_task(task, attempt)?.is_some();
            loop {
                let messages = store.peer_inbox(task, attempt)?;
                if !messages.is_empty() || seconds == 0 {
                    print_value(&serde_json::to_value(messages)?, json_output);
                    break;
                }
                let group = store
                    .active_debate_for_task(task, attempt)?
                    .map(|id| store.debate(&id))
                    .transpose()?;
                if !joined_at_start && let Some(group) = &group {
                    print_value(
                        &serde_json::json!({
                            "state": "debate_joined",
                            "group": group.id,
                            "members": group.members,
                            "instruction": "You joined this debate. Use brgr debate send with this group, then wait again.",
                        }),
                        json_output,
                    );
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    let unread = group.as_ref().map_or_else(Vec::new, |group| {
                        group
                            .members
                            .iter()
                            .filter(|member| {
                                store
                                    .peer_unread_summary(&group.id, **member)
                                    .is_ok_and(|(count, _)| count > 0)
                            })
                            .map(ToString::to_string)
                            .collect()
                    });
                    print_value(
                        &serde_json::json!({
                            "state": "timeout",
                            "group": group.as_ref().map(|group| &group.id),
                            "members": group.as_ref().map(|group| &group.members),
                            "unread_members": unread,
                            "message": "No peer message arrived before the timeout. A partner may be busy; this does not mean the debate failed.",
                        }),
                        json_output,
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        DebateCommand::Ack { message, task } => {
            let task = worker_task(task)?;
            let (attempt, proof) = authorize(&store, task)?;
            if let Some(proof) = proof {
                store.acknowledge_peer_bound(&message, task, attempt, &proof)?;
            } else {
                store.acknowledge_peer(&message, task, attempt)?;
            }
            print_value(
                &serde_json::json!({"message_id":message,"acknowledged":true}),
                json_output,
            );
        }
    }
    Ok(())
}
