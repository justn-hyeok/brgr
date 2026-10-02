use brgr_protocol::{OwnerId, ResultId, TaskId};
use rusqlite::{OptionalExtension as _, params};

use super::{Store, StoreError, owner::assert_owner_binding};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingNotification {
    pub result_id: ResultId,
    pub task_id: TaskId,
    pub owner_id: OwnerId,
    pub attempts: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationTarget {
    pub result_id: ResultId,
    pub task_id: TaskId,
    pub owner_id: OwnerId,
    pub session_id: String,
    pub binding_epoch: u64,
    pub pane_id: String,
    pub herdr_session: Option<String>,
    pub herdr_bin: String,
}

/// A worker's question its owner's current Codex session has not been told of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionTarget {
    pub message_id: String,
    pub task_id: TaskId,
    pub owner_id: String,
    pub session_id: String,
    pub pane_id: String,
    pub herdr_session: Option<String>,
    pub herdr_bin: String,
    pub kind: String,
    pub body: String,
}

struct ClaimRow {
    task: String,
    owner: String,
    session: String,
    pane: String,
    epoch: i64,
    herdr_bin: String,
    herdr_session: Option<String>,
    prior_token: String,
    lease: i64,
}

impl Store {
    /// Returns whether this result ended the supervisor run.
    ///
    /// # Errors
    ///
    /// Returns a database error if the completion record cannot be read.
    pub fn run_completed(&self, result_id: ResultId) -> Result<bool, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM task_run_completions WHERE result_id = ?1",
                [result_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    /// Records the exact Codex pane currently bound to an owner.
    ///
    /// # Errors
    ///
    /// Rejects a stale session/epoch or invalid surface identity.
    pub fn register_owner_surface(
        &self,
        owner_id: &OwnerId,
        session_id: &str,
        binding_epoch: u64,
        pane_id: &str,
        herdr_session: Option<&str>,
        herdr_bin: &str,
    ) -> Result<(), StoreError> {
        if pane_id.trim().is_empty() || herdr_bin.trim().is_empty() {
            return Err(StoreError::InvalidOwnerSurface);
        }
        let transaction = self.write_transaction()?;
        assert_owner_binding(
            &transaction,
            owner_id,
            Some(session_id),
            Some(binding_epoch),
        )?;
        transaction.execute(
            "INSERT INTO owner_surfaces
             (owner_id, session_id, binding_epoch, pane_id, herdr_session, herdr_bin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(owner_id) DO UPDATE SET
               session_id = excluded.session_id,
               binding_epoch = excluded.binding_epoch,
               pane_id = excluded.pane_id,
               herdr_session = excluded.herdr_session,
               herdr_bin = excluded.herdr_bin",
            params![
                owner_id.as_str(),
                session_id,
                i64::try_from(binding_epoch).map_err(|_| StoreError::NumericOverflow)?,
                pane_id,
                herdr_session,
                herdr_bin,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Lists undelivered, unacknowledged terminal results for one task.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored IDs or a database failure.
    pub fn pending_notifications_for_task(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<PendingNotification>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT n.result_id, n.task_id, n.owner_id, n.attempts, n.last_error
             FROM completion_notifications n
             JOIN inbox_items i ON i.result_id = n.result_id AND i.owner_id = n.owner_id
             WHERE n.task_id = ?1 AND n.resolved = 0 AND n.delivered_session IS NULL
               AND i.acknowledged = 0 ORDER BY n.rowid",
        )?;
        let rows = statement.query_map([task_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (result, task, owner, attempts, last_error) = row?;
            Ok(PendingNotification {
                result_id: result
                    .parse()
                    .map_err(|_| StoreError::InvalidNotification)?,
                task_id: task.parse().map_err(|_| StoreError::InvalidNotification)?,
                owner_id: OwnerId::new(owner)?,
                attempts: u64::try_from(attempts).map_err(|_| StoreError::NumericOverflow)?,
                last_error,
            })
        })
        .collect()
    }

    /// Finds tasks that have undelivered results for a currently bound session.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored IDs or a database failure.
    pub fn pending_notification_tasks_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<TaskId>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT n.task_id FROM completion_notifications n
             JOIN inbox_items i ON i.result_id = n.result_id AND i.owner_id = n.owner_id
             JOIN owner_bindings b ON b.owner_id = n.owner_id
             WHERE b.session_id = ?1 AND n.resolved = 0
               AND n.delivered_session IS NULL AND i.acknowledged = 0
             ORDER BY n.rowid",
        )?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|row| row?.parse().map_err(|_| StoreError::InvalidNotification))
            .collect()
    }

    /// Claims one pending result for bounded notification delivery.
    ///
    /// # Errors
    ///
    /// Returns an error if stored identity or the database is invalid.
    pub fn claim_notification(
        &self,
        result_id: ResultId,
        token: &str,
        now_seconds: i64,
        lease_seconds: i64,
    ) -> Result<Option<NotificationTarget>, StoreError> {
        if token.is_empty() || lease_seconds <= 0 {
            return Err(StoreError::InvalidNotification);
        }
        let transaction = self.write_transaction()?;
        let record: Option<ClaimRow> = transaction
            .query_row(
                "SELECT n.task_id, n.owner_id, b.session_id, s.pane_id,
                        b.binding_epoch, s.herdr_bin, s.herdr_session,
                        COALESCE(n.claim_token, ''), n.claim_until
                 FROM completion_notifications n
                 JOIN inbox_items i ON i.result_id = n.result_id AND i.owner_id = n.owner_id
                 JOIN owner_bindings b ON b.owner_id = n.owner_id
                 JOIN owner_surfaces s ON s.owner_id = b.owner_id
                   AND s.session_id = b.session_id AND s.binding_epoch = b.binding_epoch
                 WHERE n.result_id = ?1 AND n.resolved = 0
                   AND n.delivered_session IS NULL AND i.acknowledged = 0",
                [result_id.to_string()],
                |row| {
                    Ok(ClaimRow {
                        task: row.get(0)?,
                        owner: row.get(1)?,
                        session: row.get(2)?,
                        pane: row.get(3)?,
                        epoch: row.get(4)?,
                        herdr_bin: row.get(5)?,
                        herdr_session: row.get(6)?,
                        prior_token: row.get(7)?,
                        lease: row.get(8)?,
                    })
                },
            )
            .optional()?;
        let Some(record) = record else {
            return Ok(None);
        };
        if record.lease > now_seconds && record.prior_token != token {
            return Ok(None);
        }
        transaction.execute(
            "UPDATE completion_notifications SET claim_token = ?1,
               claim_until = ?2, attempts = attempts + 1 WHERE result_id = ?3",
            params![
                token,
                now_seconds.saturating_add(lease_seconds),
                result_id.to_string()
            ],
        )?;
        transaction.commit()?;
        Ok(Some(NotificationTarget {
            result_id,
            task_id: record
                .task
                .parse()
                .map_err(|_| StoreError::InvalidNotification)?,
            owner_id: OwnerId::new(record.owner)?,
            session_id: record.session,
            binding_epoch: u64::try_from(record.epoch).map_err(|_| StoreError::NumericOverflow)?,
            pane_id: record.pane,
            herdr_session: record.herdr_session,
            herdr_bin: record.herdr_bin,
        }))
    }

    /// Completes a claimed delivery only while the owner binding still matches.
    ///
    /// # Errors
    ///
    /// Rejects a stale claim or owner transfer.
    pub fn mark_notification_delivered(
        &self,
        target: &NotificationTarget,
        token: &str,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE completion_notifications SET delivered_session = ?1,
                delivered_epoch = ?2, delivered_pane = ?3,
                claim_token = NULL, claim_until = 0, last_error = NULL
             WHERE result_id = ?4 AND claim_token = ?5 AND resolved = 0
               AND delivered_session IS NULL
               AND EXISTS (SELECT 1 FROM owner_bindings b
                   JOIN owner_surfaces s ON s.owner_id = b.owner_id
                   WHERE b.owner_id = completion_notifications.owner_id
                     AND b.session_id = ?1 AND b.binding_epoch = ?2
                     AND s.session_id = b.session_id AND s.binding_epoch = b.binding_epoch
                     AND s.pane_id = ?3)",
            params![
                target.session_id,
                i64::try_from(target.binding_epoch).map_err(|_| StoreError::NumericOverflow)?,
                target.pane_id,
                target.result_id.to_string(),
                token,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::NotificationClaimStale);
        }
        Ok(())
    }

    /// Releases a claim for a later retry, retaining a bounded diagnostic.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn release_notification_claim(
        &self,
        result_id: ResultId,
        token: &str,
        error: &str,
    ) -> Result<(), StoreError> {
        let diagnostic: String = error.chars().take(512).collect();
        self.connection.execute(
            "UPDATE completion_notifications SET claim_token = NULL,
               claim_until = 0, last_error = ?1
             WHERE result_id = ?2 AND claim_token = ?3 AND resolved = 0",
            params![diagnostic, result_id.to_string(), token],
        )?;
        Ok(())
    }

    /// Worker questions on `task_id` still waiting on the owner, which the
    /// owner's currently bound session has not been notified of.
    ///
    /// Only the completion ever reached the owner by itself, so a worker that
    /// asked sat until its own wait timed out unless someone happened to run
    /// `brgr status --tree`. A question stops being pending once it is replied
    /// to or acknowledged; a notice is per session, so a transferred owner is
    /// told again.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails or stored identifiers are invalid.
    pub fn pending_question_notices(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<QuestionTarget>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT q.message_id, b.session_id, s.pane_id, s.herdr_session, s.herdr_bin, q.kind, q.body, t.owner_id
             FROM task_messages q
             JOIN attempts a ON a.attempt_id = q.attempt_id
             JOIN tasks t ON t.task_id = a.task_id AND t.revision = a.revision
             JOIN owner_bindings b ON b.owner_id = t.owner_id
             JOIN owner_surfaces s ON s.owner_id = b.owner_id
               AND s.session_id = b.session_id AND s.binding_epoch = b.binding_epoch
             WHERE q.task_id = ?1 AND q.kind IN ('question','note')
               AND q.direction = 'worker_to_owner' AND q.acknowledged = 0
               AND NOT EXISTS (
                 SELECT 1 FROM task_messages r
                 WHERE r.in_reply_to = q.message_id AND r.kind = 'reply'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM question_notices n
                 WHERE n.message_id = q.message_id AND n.session_id = b.session_id
               )
               AND NOT EXISTS (
                 SELECT 1 FROM withdrawn_questions w WHERE w.message_id = q.message_id
               )
             ORDER BY q.rowid",
        )?;
        let rows = statement.query_map([task_id.to_string()], |row| {
            Ok(QuestionTarget {
                message_id: row.get(0)?,
                task_id,
                session_id: row.get(1)?,
                pane_id: row.get(2)?,
                herdr_session: row.get(3)?,
                herdr_bin: row.get(4)?,
                kind: row.get(5)?,
                body: row.get(6)?,
                owner_id: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records that `session_id` was told about a question, so it is not told
    /// again. Losing this write re-sends one notice, which the notice itself
    /// says to treat as a duplicate by `message_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails.
    pub fn record_question_notice(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO question_notices (message_id, session_id) VALUES (?1, ?2)",
            params![message_id, session_id],
        )?;
        Ok(())
    }
}
