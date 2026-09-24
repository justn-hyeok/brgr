use std::{env, time::Duration};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{AttemptId, TaskId};
use brgr_store::{MessageDirection, MessageDraft, MessageKind, Store, StoreError};
use clap::{Subcommand, ValueEnum};
use serde_json::json;

use crate::{Paths, print_value, require_owner};

const MAX_WAIT_SECONDS: u64 = 86_400;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MessageSide {
    Owner,
    Worker,
}

impl MessageSide {
    fn direction(self) -> MessageDirection {
        match self {
            Self::Owner => MessageDirection::WorkerToOwner,
            Self::Worker => MessageDirection::OwnerToWorker,
        }
    }

    fn sender(self) -> Self {
        match self {
            Self::Owner => Self::Worker,
            Self::Worker => Self::Owner,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MessageKindArg {
    Question,
    Reply,
    Note,
}

impl From<MessageKindArg> for MessageKind {
    fn from(value: MessageKindArg) -> Self {
        match value {
            MessageKindArg::Question => Self::Question,
            MessageKindArg::Reply => Self::Reply,
            MessageKindArg::Note => Self::Note,
        }
    }
}

#[derive(Subcommand)]
pub enum MessageCommand {
    Send {
        task: TaskId,
        #[arg(long)]
        to: MessageSide,
        #[arg(long)]
        kind: MessageKindArg,
        #[arg(long)]
        body: String,
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    List {
        task: TaskId,
        #[arg(long = "for")]
        recipient: MessageSide,
        #[arg(long)]
        all: bool,
    },
    Wait {
        task: TaskId,
        #[arg(long = "for")]
        recipient: MessageSide,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    Ack {
        task: TaskId,
        message_id: String,
        #[arg(long = "for")]
        recipient: MessageSide,
    },
}

pub async fn run(paths: &Paths, command: MessageCommand, json_output: bool) -> Result<()> {
    match command {
        MessageCommand::Send {
            task,
            to,
            kind,
            body,
            reply_to,
            request_id,
        } => {
            let store = Store::open(&paths.store)?;
            let attempt = authorized_attempt(&store, task, to.sender(), true)?;
            let mut draft =
                MessageDraft::new(task, attempt, to.direction(), kind.into(), body, reply_to);
            if let Some(id) = request_id {
                draft.message_id = id;
            }
            print_value(
                &serde_json::to_value(store.post_message(&draft)?)?,
                json_output,
            );
        }
        MessageCommand::List {
            task,
            recipient,
            all,
        } => {
            let store = Store::open(&paths.store)?;
            let attempt = authorized_attempt(&store, task, recipient, false)?;
            let messages = store.task_messages(task, attempt, recipient.direction(), all)?;
            print_value(&serde_json::to_value(messages)?, json_output);
        }
        MessageCommand::Wait {
            task,
            recipient,
            timeout_seconds,
        } => {
            wait_for_message(paths, task, recipient, timeout_seconds, json_output).await?;
        }
        MessageCommand::Ack {
            task,
            message_id,
            recipient,
        } => {
            let store = Store::open(&paths.store)?;
            let attempt = authorized_attempt(&store, task, recipient, false)?;
            store.acknowledge_message(task, attempt, recipient.direction(), &message_id)?;
            print_value(
                &json!({"message_id": message_id, "acknowledged": true}),
                json_output,
            );
        }
    }
    Ok(())
}

async fn wait_for_message(
    paths: &Paths,
    task: TaskId,
    recipient: MessageSide,
    timeout_seconds: u64,
    json_output: bool,
) -> Result<()> {
    if timeout_seconds == 0 || timeout_seconds > MAX_WAIT_SECONDS {
        bail!("message wait timeout must be between 1 and 86400 seconds");
    }
    let store = Store::open(&paths.store)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let attempt = match authorized_attempt(&store, task, recipient, false) {
            Ok(attempt) => Some(attempt),
            Err(error)
                if recipient == MessageSide::Owner
                    && matches!(
                        error.downcast_ref::<StoreError>(),
                        Some(StoreError::TaskMessageNotFound(_))
                    ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        if let Some(attempt) = attempt
            && let Some(message) = store
                .task_messages(task, attempt, recipient.direction(), false)?
                .into_iter()
                .next()
        {
            print_value(&serde_json::to_value(message)?, json_output);
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("no message for task {task} before the wait timeout");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn authorized_attempt(
    store: &Store,
    task: TaskId,
    side: MessageSide,
    require_active: bool,
) -> Result<AttemptId> {
    match side {
        MessageSide::Owner => {
            let spec = store.task(task)?;
            require_owner(store, &spec.owner_id)?;
            if require_active {
                Ok(store.active_message_attempt(task)?)
            } else {
                Ok(store.latest_message_attempt(task)?)
            }
        }
        MessageSide::Worker => {
            let own_task: TaskId = env::var("BRGR_PARENT_TASK_ID")
                .context("worker task identity is absent")?
                .parse()
                .context("worker task identity is invalid")?;
            let own_attempt: AttemptId = env::var("BRGR_PARENT_ATTEMPT_ID")
                .context("worker attempt identity is absent")?
                .parse()
                .context("worker attempt identity is invalid")?;
            let permitted = if require_active {
                store.active_message_attempt(task)? == own_attempt
            } else {
                store.message_attempt_belongs_to_task(task, own_attempt)?
            };
            if own_task != task || !permitted {
                bail!("message target differs from the current worker attempt");
            }
            Ok(own_attempt)
        }
    }
}
