use std::time::Duration;

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
            let (attempt, proof) = authorized_attempt(&store, task, to.sender(), true)?;
            let mut draft =
                MessageDraft::new(task, attempt, to.direction(), kind.into(), body, reply_to);
            if let Some(id) = request_id {
                draft.message_id = id;
            }
            // The write sits inside the same transaction that checks the
            // session is still the owner's head: a takeover that lands while
            // this command runs cannot accept a message the new head no longer
            // owns. The worker side has no proof and writes unguarded.
            let posted = match proof {
                Some(proof) => store.post_message_bound(&draft, &proof)?,
                None => store.post_message(&draft)?,
            };
            print_value(&serde_json::to_value(posted)?, json_output);
        }
        MessageCommand::List {
            task,
            recipient,
            all,
        } => {
            let store = Store::open(&paths.store)?;
            let (attempt, _proof) = authorized_attempt(&store, task, recipient, false)?;
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
            let (attempt, proof) = authorized_attempt(&store, task, recipient, false)?;
            // Same transaction, for the same reason as `send`: a stale session
            // must mark nothing read once its head has moved.
            match &proof {
                Some(owner_proof) => store.acknowledge_message_bound(
                    task,
                    attempt,
                    recipient.direction(),
                    &message_id,
                    owner_proof,
                )?,
                None => {
                    store.acknowledge_message(task, attempt, recipient.direction(), &message_id)?;
                }
            }
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
    let debate = DebateWait::start(paths);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let attempt = match authorized_attempt(&store, task, recipient, false) {
            Ok((attempt, _proof)) => Some(attempt),
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
        if debate.message_pending(&store)? {
            print_value(&debate_pending_receipt(), json_output);
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("no message for task {task} before the wait timeout");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What a blocking command prints when it returns early for a debate message.
pub(crate) fn debate_pending_receipt() -> serde_json::Value {
    json!({
        "state": "debate_message_pending",
        "instruction": "Run brgr debate list, answer or brgr debate ack each message, then wait again.",
    })
}

/// Lets a worker's blocking brgr command notice a debate message, which it could
/// otherwise not read until the command ends. It asks on every poll, because a
/// debate can start while the command is already waiting.
pub(crate) struct DebateWait {
    worker: Option<(TaskId, AttemptId)>,
    _marker: Option<WaitMarker>,
}

impl DebateWait {
    pub(crate) fn start(paths: &Paths) -> Self {
        let task = crate::worker_identity::task().and_then(|value| value.parse::<TaskId>().ok());
        let attempt =
            crate::worker_identity::attempt().and_then(|value| value.parse::<AttemptId>().ok());
        let worker = task.zip(attempt);
        // The marker only informs `debate status`, so a failure must not stop the wait.
        let marker = worker.and_then(|(task, _)| WaitMarker::create(paths, task).ok());
        Self {
            worker,
            _marker: marker,
        }
    }

    pub(crate) fn message_pending(&self, store: &Store) -> Result<bool> {
        let Some((task, attempt)) = self.worker else {
            return Ok(false);
        };
        Ok(!store.peer_inbox(task, attempt)?.is_empty())
    }
}

pub(crate) struct WaitMarker(std::path::PathBuf);
impl WaitMarker {
    pub(crate) fn create(paths: &Paths, task: TaskId) -> Result<Self> {
        use std::io::Write as _;
        let directory = paths.runs.join("debate-waits");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{task}-{}.json", std::process::id()));
        let mut file = std::fs::File::create(&path)?;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let birth = process_signature(std::process::id().cast_signed())
            .context("could not verify wait process identity")?;
        write!(file, "{seconds}\n{birth}")?;
        Ok(Self(path))
    }
}

pub(crate) fn process_signature(pid: i32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}
impl Drop for WaitMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn authorized_attempt(
    store: &Store,
    task: TaskId,
    side: MessageSide,
    require_active: bool,
) -> Result<(AttemptId, Option<brgr_store::OwnerProof>)> {
    match side {
        MessageSide::Owner => {
            let spec = store.task(task)?;
            let (session_id, binding_epoch) = require_owner(store, &spec.owner_id)?;
            let proof =
                brgr_store::OwnerProof::new(spec.owner_id.clone(), &session_id, binding_epoch)?;
            let attempt = if require_active {
                store.active_message_attempt(task)?
            } else {
                store.latest_message_attempt(task)?
            };
            Ok((attempt, Some(proof)))
        }
        MessageSide::Worker => {
            let own_task: TaskId = crate::worker_identity::task()
                .context("worker task identity is absent")?
                .parse()
                .context("worker task identity is invalid")?;
            let own_attempt: AttemptId = crate::worker_identity::attempt()
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
            Ok((own_attempt, None))
        }
    }
}
