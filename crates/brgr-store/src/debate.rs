//! Explicit peer conversations and native message delivery receipts.
use crate::{MessageKind, Store, StoreError};
use brgr_protocol::{AttemptId, TaskId};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebateGroup {
    pub id: String,
    pub owner: String,
    pub active: bool,
    pub members: Vec<TaskId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerMessage {
    pub id: String,
    pub group: String,
    pub from: TaskId,
    pub to: TaskId,
    pub from_attempt: AttemptId,
    pub to_attempt: AttemptId,
    pub kind: MessageKind,
    pub body: String,
    pub reply_to: Option<String>,
    pub acknowledged: bool,
}

pub struct PeerDraft {
    pub id: String,
    pub group: String,
    pub from: TaskId,
    pub to: TaskId,
    pub kind: MessageKind,
    pub body: String,
    pub reply_to: Option<String>,
}

impl Store {
    /// Create a named debate for sibling attempts with the same owner.
    /// # Errors
    /// Rejects missing tasks, stale attempts, or conflicting group identities.
    pub fn create_debate(&self, id: &str, tasks: &[TaskId]) -> Result<DebateGroup, StoreError> {
        if Uuid::parse_str(id).is_err() || tasks.len() < 2 || tasks.len() > 32 {
            return Err(StoreError::InvalidTaskMessage);
        }
        let first = self.task(tasks[0])?;
        let parent = self.delegation_parent(tasks[0])?;
        let mut members = std::collections::BTreeSet::new();
        let mut attempts = Vec::new();
        for task in tasks {
            if !members.insert(task.to_string())
                || self.task(*task)?.owner_id != first.owner_id
                || self.delegation_parent(*task)? != parent
            {
                return Err(StoreError::InvalidTaskMessage);
            }
            attempts.push((*task, self.active_message_attempt(*task)?));
        }
        let transaction = self.write_transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT owner_id FROM debate_groups WHERE group_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_some() {
            drop(transaction);
            let group = self.debate(id)?;
            if group
                .members
                .iter()
                .map(ToString::to_string)
                .collect::<std::collections::BTreeSet<_>>()
                != members
            {
                return Err(StoreError::InvalidTaskMessage);
            }
            return Ok(group);
        }
        transaction.execute(
            "INSERT INTO debate_groups (group_id,owner_id) VALUES (?1,?2)",
            params![id, first.owner_id.as_str()],
        )?;
        for (task, attempt) in attempts {
            transaction.execute(
                "INSERT INTO debate_members (group_id,task_id,attempt_id) VALUES (?1,?2,?3)",
                params![id, task.to_string(), attempt.to_string()],
            )?;
        }
        transaction.commit()?;
        self.debate(id)
    }

    /// Read the exact debate group and participants.
    /// # Errors
    /// Returns an error for missing groups or invalid stored identifiers.
    pub fn debate(&self, id: &str) -> Result<DebateGroup, StoreError> {
        let (owner, active): (String, bool) = self
            .connection
            .query_row(
                "SELECT owner_id,active FROM debate_groups WHERE group_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::TaskMessageNotFound(id.to_owned()))?;
        let mut statement = self
            .connection
            .prepare("SELECT task_id FROM debate_members WHERE group_id=?1 ORDER BY task_id")?;
        let members = statement
            .query_map([id], |r| r.get::<_, String>(0))?
            .map(|r| r?.parse().map_err(|_| StoreError::InvalidTaskMessage))
            .collect::<Result<_, _>>()?;
        Ok(DebateGroup {
            id: id.to_owned(),
            owner,
            active,
            members,
        })
    }

    /// Stop direct conversation without deleting its history.
    /// # Errors
    /// Returns an error for missing groups or database failure.
    pub fn stop_debate(&self, id: &str) -> Result<(), StoreError> {
        self.debate(id)?;
        self.connection
            .execute("UPDATE debate_groups SET active=0 WHERE group_id=?1", [id])?;
        Ok(())
    }

    /// Read the retained conversation, including acknowledged messages.
    /// # Errors
    /// Returns an error for missing groups, invalid identifiers or database failure.
    pub fn debate_history(&self, id: &str) -> Result<Vec<PeerMessage>, StoreError> {
        self.debate(id)?;
        let mut statement=self.connection.prepare("SELECT message_id,from_task,to_task,from_attempt,to_attempt,kind,body,in_reply_to,acknowledged FROM peer_messages WHERE group_id=?1 ORDER BY rowid")?;
        let rows = statement.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, bool>(8)?,
            ))
        })?;
        rows.map(|r| {
            let (message, from, to, fa, ta, kind, body, reply_to, acknowledged) = r?;
            Ok(PeerMessage {
                id: message,
                group: id.to_owned(),
                from: from.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                to: to.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                from_attempt: fa.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                to_attempt: ta.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                kind: match kind.as_str() {
                    "question" => MessageKind::Question,
                    "reply" => MessageKind::Reply,
                    "note" => MessageKind::Note,
                    _ => return Err(StoreError::InvalidTaskMessage),
                },
                body,
                reply_to,
                acknowledged,
            })
        })
        .collect()
    }

    /// Post an idempotent message between exact, explicitly grouped attempts.
    /// # Errors
    /// Rejects inactive groups, stale participants, invalid replies and replay conflicts.
    pub fn post_peer_message(&self, draft: &PeerDraft) -> Result<PeerMessage, StoreError> {
        self.post_peer_message_from(draft, self.active_message_attempt(draft.from)?)
    }

    /// Send from the caller's exact worker attempt.
    /// # Errors
    /// Rejects stale participants, invalid payloads and conflicting replays.
    pub fn post_peer_message_from(
        &self,
        draft: &PeerDraft,
        from_attempt: AttemptId,
    ) -> Result<PeerMessage, StoreError> {
        if Uuid::parse_str(&draft.id).is_err()
            || draft.body.trim().is_empty()
            || draft.body.len() > 8192
            || draft.from == draft.to
            || (draft.kind == MessageKind::Reply) != draft.reply_to.is_some()
        {
            return Err(StoreError::InvalidTaskMessage);
        }
        let group = self.debate(&draft.group)?;
        if !group.active
            || !group.members.contains(&draft.from)
            || !group.members.contains(&draft.to)
        {
            return Err(StoreError::InvalidTaskMessage);
        }
        let to_attempt = self.active_message_attempt(draft.to)?;
        let transaction = self.write_transaction()?;
        let active: bool = transaction.query_row(
            "SELECT active FROM debate_groups WHERE group_id=?1",
            [&draft.group],
            |row| row.get(0),
        )?;
        if !active {
            return Err(StoreError::InvalidTaskMessage);
        }
        for (task, attempt) in [(draft.from, from_attempt), (draft.to, to_attempt)] {
            let matches: bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM debate_members WHERE group_id=?1 AND task_id=?2 AND attempt_id=?3)",params![draft.group,task.to_string(),attempt.to_string()],|r|r.get(0))?;
            if !matches {
                return Err(StoreError::InvalidTaskMessage);
            }
            let running: bool = transaction.query_row(
                "SELECT state IN ('running','blocked') FROM attempts WHERE attempt_id=?1",
                [attempt.to_string()],
                |row| row.get(0),
            )?;
            if !running {
                return Err(StoreError::InvalidTaskMessage);
            }
        }
        if let Some(reply) = &draft.reply_to {
            let valid: bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM peer_messages WHERE message_id=?1 AND group_id=?2 AND from_task=?3 AND to_task=?4 AND kind='question')",params![reply,draft.group,draft.to.to_string(),draft.from.to_string()],|r|r.get(0))?;
            if !valid {
                return Err(StoreError::InvalidTaskMessage);
            }
        }
        let value = PeerMessage {
            id: draft.id.clone(),
            group: draft.group.clone(),
            from: draft.from,
            to: draft.to,
            from_attempt,
            to_attempt,
            kind: draft.kind,
            body: draft.body.clone(),
            reply_to: draft.reply_to.clone(),
            acknowledged: false,
        };
        let existing: Option<(bool, bool)> = transaction.query_row(
            "SELECT group_id=?2 AND from_task=?3 AND to_task=?4 AND body=?5 AND kind=?6 AND in_reply_to IS ?7 AND from_attempt=?8 AND to_attempt=?9,acknowledged FROM peer_messages WHERE message_id=?1",
            params![draft.id,draft.group,draft.from.to_string(),draft.to.to_string(),draft.body,draft.kind.as_str(),draft.reply_to,from_attempt.to_string(),to_attempt.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        if let Some((matches, acknowledged)) = existing {
            if !matches {
                return Err(StoreError::TaskMessageConflict(draft.id.clone()));
            }
            return Ok(PeerMessage {
                acknowledged,
                ..value
            });
        }
        transaction.execute("INSERT INTO peer_messages (message_id,group_id,from_task,to_task,from_attempt,to_attempt,kind,body,in_reply_to) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![draft.id,draft.group,draft.from.to_string(),draft.to.to_string(),from_attempt.to_string(),to_attempt.to_string(),draft.kind.as_str(),draft.body,draft.reply_to])?;
        transaction.commit()?;
        Ok(value)
    }

    /// Read a task's undelivered peer messages while its group is active.
    /// # Errors
    /// Returns an error for database failure or invalid recorded identifiers.
    pub fn peer_inbox(
        &self,
        task: TaskId,
        attempt: AttemptId,
    ) -> Result<Vec<PeerMessage>, StoreError> {
        let mut statement=self.connection.prepare("SELECT p.message_id,p.group_id,p.from_task,p.to_task,p.from_attempt,p.to_attempt,p.kind,p.body,p.in_reply_to,p.acknowledged FROM peer_messages p JOIN debate_groups g ON g.group_id=p.group_id WHERE p.to_task=?1 AND p.to_attempt=?2 AND p.acknowledged=0 AND g.active=1 ORDER BY p.rowid")?;
        let rows = statement.query_map(params![task.to_string(), attempt.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, bool>(9)?,
            ))
        })?;
        rows.map(|r| {
            let (id, group, from, to, fa, ta, kind, body, reply_to, acknowledged) = r?;
            Ok(PeerMessage {
                id,
                group,
                from: from.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                to: to.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                from_attempt: fa.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                to_attempt: ta.parse().map_err(|_| StoreError::InvalidTaskMessage)?,
                kind: match kind.as_str() {
                    "question" => MessageKind::Question,
                    "reply" => MessageKind::Reply,
                    "note" => MessageKind::Note,
                    _ => return Err(StoreError::InvalidTaskMessage),
                },
                body,
                reply_to,
                acknowledged,
            })
        })
        .collect()
    }

    /// Mark a peer message as read by its exact recipient attempt.
    /// # Errors
    /// Rejects other participants or unknown messages.
    pub fn acknowledge_peer(
        &self,
        id: &str,
        task: TaskId,
        attempt: AttemptId,
    ) -> Result<(), StoreError> {
        if self.connection.execute("UPDATE peer_messages SET acknowledged=1 WHERE message_id=?1 AND to_task=?2 AND to_attempt=?3",params![id,task.to_string(),attempt.to_string()])? != 1 { return Err(StoreError::TaskMessageNotFound(id.to_owned())); }
        Ok(())
    }

    /// Claim a native delivery once; interrupted sending remains observable.
    /// # Errors
    /// Returns an error on database failure.
    pub fn claim_native_delivery(
        &self,
        id: &str,
        attempt: AttemptId,
        pane: &str,
    ) -> Result<bool, StoreError> {
        Ok(self.connection.execute("INSERT OR IGNORE INTO native_message_deliveries (message_id,attempt_id,pane_id,state) VALUES (?1,?2,?3,'sending')",params![id,attempt.to_string(),pane])?==1)
    }

    /// Commit delivery or an uncertain transport outcome without replaying it.
    /// # Errors
    /// Returns an error for missing claims or database failure.
    pub fn finish_native_delivery(&self, id: &str, error: Option<&str>) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE native_message_deliveries SET state=?2,error=?3 WHERE message_id=?1",
            params![
                id,
                if error.is_some() {
                    "uncertain"
                } else {
                    "delivered"
                },
                error
            ],
        )?;
        Ok(())
    }

    /// Release a claim when no native input was attempted.
    /// # Errors
    /// Returns an error on database failure.
    pub fn release_native_delivery(&self, id: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM native_message_deliveries WHERE message_id=?1 AND state='sending'",
            [id],
        )?;
        Ok(())
    }
}
