use brgr_protocol::{AttemptId, TaskId};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Store, StoreError};

const MAX_BODY_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    OwnerToWorker,
    WorkerToOwner,
}

impl MessageDirection {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerToWorker => "owner_to_worker",
            Self::WorkerToOwner => "worker_to_owner",
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::OwnerToWorker => Self::WorkerToOwner,
            Self::WorkerToOwner => Self::OwnerToWorker,
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "owner_to_worker" => Ok(Self::OwnerToWorker),
            "worker_to_owner" => Ok(Self::WorkerToOwner),
            _ => Err(StoreError::InvalidTaskMessage),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Question,
    Reply,
    Note,
}

impl MessageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Reply => "reply",
            Self::Note => "note",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "question" => Ok(Self::Question),
            "reply" => Ok(Self::Reply),
            "note" => Ok(Self::Note),
            _ => Err(StoreError::InvalidTaskMessage),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskMessage {
    pub message_id: String,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub direction: MessageDirection,
    pub kind: MessageKind,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageDraft {
    pub message_id: String,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub direction: MessageDirection,
    pub kind: MessageKind,
    pub body: String,
    pub in_reply_to: Option<String>,
}

struct RawMessage {
    id: String,
    task: String,
    attempt: String,
    direction: String,
    kind: String,
    body: String,
    in_reply_to: Option<String>,
    acknowledged: bool,
}

impl MessageDraft {
    #[must_use]
    pub fn new(
        task_id: TaskId,
        attempt_id: AttemptId,
        direction: MessageDirection,
        kind: MessageKind,
        body: String,
        in_reply_to: Option<String>,
    ) -> Self {
        Self {
            message_id: Uuid::new_v4().to_string(),
            task_id,
            attempt_id,
            direction,
            kind,
            body,
            in_reply_to,
        }
    }
}

impl Store {
    /// Returns the latest attempt for a task, including a settled attempt.
    ///
    /// # Errors
    ///
    /// Returns an error if no attempt exists or stored identity is invalid.
    pub fn latest_message_attempt(&self, task_id: TaskId) -> Result<AttemptId, StoreError> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT attempt_id FROM attempts WHERE task_id = ?1 ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let raw = raw.ok_or(StoreError::TaskMessageNotFound(task_id.to_string()))?;
        raw.parse().map_err(|_| StoreError::InvalidAttemptId(raw))
    }

    /// Returns the exact active attempt that can exchange new messages.
    ///
    /// # Errors
    ///
    /// Returns an error if no active attempt exists or stored identity is invalid.
    pub fn active_message_attempt(&self, task_id: TaskId) -> Result<AttemptId, StoreError> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT attempt_id FROM attempts WHERE task_id = ?1
             AND state IN ('running', 'blocked') ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let raw = raw.ok_or(StoreError::TaskMessageNotFound(task_id.to_string()))?;
        raw.parse().map_err(|_| StoreError::InvalidAttemptId(raw))
    }

    /// Inserts one bounded message, deduplicating an exact message ID replay.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid content, stale attempts, mismatched replies,
    /// conflicting replay, or database failure.
    pub fn post_message(&self, draft: &MessageDraft) -> Result<TaskMessage, StoreError> {
        validate_draft(draft)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some(existing) = load_message(&transaction, &draft.message_id)? {
            if message_matches_draft(&existing, draft) {
                return Ok(existing);
            }
            return Err(StoreError::TaskMessageConflict(draft.message_id.clone()));
        }
        validate_active_attempt(&transaction, draft)?;
        if let Some(reply_id) = &draft.in_reply_to {
            let original = load_message(&transaction, reply_id)?
                .ok_or_else(|| StoreError::TaskMessageNotFound(reply_id.clone()))?;
            if original.task_id != draft.task_id
                || original.attempt_id != draft.attempt_id
                || original.direction != draft.direction.opposite()
                || original.kind != MessageKind::Question
            {
                return Err(StoreError::InvalidTaskMessage);
            }
        }
        transaction.execute(
            "INSERT INTO task_messages
             (message_id, task_id, attempt_id, direction, kind, body, in_reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                draft.message_id,
                draft.task_id.to_string(),
                draft.attempt_id.to_string(),
                draft.direction.as_str(),
                draft.kind.as_str(),
                draft.body,
                draft.in_reply_to,
            ],
        )?;
        transaction.commit()?;
        Ok(TaskMessage {
            message_id: draft.message_id.clone(),
            task_id: draft.task_id,
            attempt_id: draft.attempt_id,
            direction: draft.direction,
            kind: draft.kind,
            body: draft.body.clone(),
            in_reply_to: draft.in_reply_to.clone(),
            acknowledged: false,
        })
    }

    /// Lists messages for one task attempt and recipient side in insertion order.
    ///
    /// # Errors
    ///
    /// Returns an error if the database or a stored message is invalid.
    pub fn task_messages(
        &self,
        task_id: TaskId,
        attempt_id: AttemptId,
        direction: MessageDirection,
        include_acknowledged: bool,
    ) -> Result<Vec<TaskMessage>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT message_id FROM task_messages WHERE task_id = ?1 AND attempt_id = ?2
             AND direction = ?3 AND (?4 = 1 OR acknowledged = 0) ORDER BY rowid",
        )?;
        let ids = statement.query_map(
            params![
                task_id.to_string(),
                attempt_id.to_string(),
                direction.as_str(),
                include_acknowledged
            ],
            |row| row.get::<_, String>(0),
        )?;
        ids.map(|id| load_message(&self.connection, &id?)?.ok_or(StoreError::InvalidTaskMessage))
            .collect()
    }

    /// Acknowledges only a message addressed to the selected side.
    ///
    /// # Errors
    ///
    /// Returns an error for a mismatched recipient or database failure.
    pub fn acknowledge_message(
        &self,
        task_id: TaskId,
        attempt_id: AttemptId,
        direction: MessageDirection,
        message_id: &str,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE task_messages SET acknowledged = 1 WHERE message_id = ?1
             AND task_id = ?2 AND attempt_id = ?3 AND direction = ?4",
            params![
                message_id,
                task_id.to_string(),
                attempt_id.to_string(),
                direction.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::TaskMessageNotFound(message_id.to_owned()));
        }
        Ok(())
    }

    /// Counts questions without a reply on the same attempt. A reply is durable
    /// even if its recipient has not acknowledged it yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn unsettled_questions(
        &self,
        task_id: TaskId,
        attempt_id: AttemptId,
    ) -> Result<u64, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM task_messages q
             WHERE q.task_id = ?1 AND q.attempt_id = ?2 AND q.kind = 'question'
               AND NOT EXISTS (
                 SELECT 1 FROM task_messages r
                 WHERE r.in_reply_to = q.message_id AND r.kind = 'reply'
               )",
            params![task_id.to_string(), attempt_id.to_string()],
            |row| row.get(0),
        )?;
        u64::try_from(count).map_err(|_| StoreError::NumericOverflow)
    }
}

fn validate_draft(draft: &MessageDraft) -> Result<(), StoreError> {
    if Uuid::parse_str(&draft.message_id).is_err()
        || draft.body.trim().is_empty()
        || draft.body.len() > MAX_BODY_BYTES
        || (draft.kind == MessageKind::Reply) != draft.in_reply_to.is_some()
        || draft
            .in_reply_to
            .as_ref()
            .is_some_and(|id| Uuid::parse_str(id).is_err())
    {
        return Err(StoreError::InvalidTaskMessage);
    }
    Ok(())
}

fn validate_active_attempt(
    transaction: &Transaction<'_>,
    draft: &MessageDraft,
) -> Result<(), StoreError> {
    let current: Option<(String, String)> = transaction
        .query_row(
            "SELECT task_id, state FROM attempts WHERE attempt_id = ?1",
            [draft.attempt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if current.is_some_and(|(task, state)| {
        task == draft.task_id.to_string() && matches!(state.as_str(), "running" | "blocked")
    }) {
        Ok(())
    } else {
        Err(StoreError::InvalidTaskMessage)
    }
}

fn load_message(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> Result<Option<TaskMessage>, StoreError> {
    let raw: Option<RawMessage> = connection
        .query_row(
            "SELECT message_id, task_id, attempt_id, direction, kind, body, in_reply_to, acknowledged
             FROM task_messages WHERE message_id = ?1",
            [message_id],
            |row| {
                Ok(RawMessage {
                    id: row.get(0)?,
                    task: row.get(1)?,
                    attempt: row.get(2)?,
                    direction: row.get(3)?,
                    kind: row.get(4)?,
                    body: row.get(5)?,
                    in_reply_to: row.get(6)?,
                    acknowledged: row.get(7)?,
                })
            },
        )
        .optional()?;
    raw.map(|record| {
        Ok(TaskMessage {
            message_id: record.id,
            task_id: record
                .task
                .parse()
                .map_err(|_| StoreError::InvalidTaskMessage)?,
            attempt_id: record
                .attempt
                .parse()
                .map_err(|_| StoreError::InvalidTaskMessage)?,
            direction: MessageDirection::parse(&record.direction)?,
            kind: MessageKind::parse(&record.kind)?,
            body: record.body,
            in_reply_to: record.in_reply_to,
            acknowledged: record.acknowledged,
        })
    })
    .transpose()
}

fn message_matches_draft(message: &TaskMessage, draft: &MessageDraft) -> bool {
    message.task_id == draft.task_id
        && message.attempt_id == draft.attempt_id
        && message.direction == draft.direction
        && message.kind == draft.kind
        && message.body == draft.body
        && message.in_reply_to == draft.in_reply_to
}
