use crate::{Paths, message::MessageKindArg, print_value, require_owner};
use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, OwnerId, TaskId};
use brgr_store::{PeerDraft, Store};
use clap::Subcommand;
use sha2::{Digest as _, Sha256};
use std::{env, time::Duration};

#[derive(Subcommand)]
pub(crate) enum DebateCommand {
    Start {
        #[arg(required=true,num_args=2..)]
        tasks: Vec<TaskId>,
        #[arg(long)]
        request_id: Option<String>,
    },
    Status {
        group: String,
    },
    Stop {
        group: String,
    },
    Send {
        group: String,
        #[arg(long)]
        from: Option<TaskId>,
        #[arg(long)]
        to: TaskId,
        #[arg(long)]
        body: String,
        #[arg(long, value_enum, default_value = "note")]
        kind: MessageKindArg,
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    List {
        task: Option<TaskId>,
    },
    Wait {
        task: Option<TaskId>,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    Ack {
        message: String,
        task: Option<TaskId>,
    },
}

fn worker_task(explicit: Option<TaskId>) -> Result<TaskId> {
    let inherited = env::var("BRGR_PARENT_TASK_ID")
        .ok()
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
    if env::var("BRGR_PARENT_TASK_ID").ok().as_deref() == Some(task.to_string().as_str()) {
        let attempt = env::var("BRGR_PARENT_ATTEMPT_ID")
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
        "FROM BRGR DEBATE GROUP\n{}\nYou joined this explicitly requested peer conversation. Use brgr debate send {} --to TASK --kind question --body TEXT; replies use --kind reply --reply-to MESSAGE_ID. Receive with debate list/wait and acknowledge with debate ack MESSAGE_ID.",
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

pub(crate) async fn run(paths: &Paths, command: DebateCommand, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    let stopping = matches!(&command, DebateCommand::Stop { .. });
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
                &serde_json::json!({"group":store.debate(&group)?,"messages":store.debate_history(&group)?}),
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
            let (attempt, _) = authorize(&store, task)?;
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
            loop {
                let messages = store.peer_inbox(task, attempt)?;
                if !messages.is_empty() || seconds == 0 {
                    print_value(&serde_json::to_value(messages)?, json_output);
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("no peer message before debate wait timeout");
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
