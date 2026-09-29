//! Owner-facing results: the inbox, acknowledgement, and decisions.

use brgr_protocol::{Decision, InboxItem, OwnerId, ResultEnvelope, ResultId, TaskId};
use rusqlite::{OptionalExtension as _, params};

use super::{
    Store, StoreError, WriteOutcome, contention::retry_busy, owner::assert_owner_binding,
    record_decision_in_transaction,
};

impl Store {
    /// Lists an owner's inbox, optionally including acknowledged entries.
    ///
    /// # Errors
    ///
    /// Returns an error when stored data is invalid or storage fails.
    pub fn inbox(
        &self,
        owner_id: &OwnerId,
        include_acknowledged: bool,
    ) -> Result<Vec<InboxItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT r.envelope_json, i.acknowledged
             FROM inbox_items i
             JOIN results r ON r.result_id = i.result_id
             WHERE i.owner_id = ?1 AND (?2 = 1 OR i.acknowledged = 0)
             ORDER BY r.rowid",
        )?;
        let rows = statement
            .query_map(params![owner_id.as_str(), include_acknowledged], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?;
        rows.map(|row| {
            let (json, acknowledged) = row?;
            Ok(InboxItem {
                owner_id: owner_id.clone(),
                result: serde_json::from_str(&json)?,
                acknowledged,
            })
        })
        .collect()
    }

    /// Reads pending inbox items for every owner explicitly bound to one
    /// session, including owners transferred from earlier sessions.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stored data or a DB failure.
    pub fn pending_for_session(&self, session_id: &str) -> Result<Vec<InboxItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT i.owner_id, r.envelope_json FROM inbox_items i
             JOIN results r ON r.result_id = i.result_id
             JOIN owner_bindings b ON b.owner_id = i.owner_id
             WHERE b.session_id = ?1 AND i.acknowledged = 0
             ORDER BY r.rowid LIMIT 100",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (owner_id, envelope_json) = row?;
            Ok(InboxItem {
                owner_id: OwnerId::new(owner_id)?,
                result: serde_json::from_str(&envelope_json)?,
                acknowledged: false,
            })
        })
        .collect()
    }

    /// Acknowledges one result in the named owner's inbox.
    ///
    /// # Errors
    ///
    /// Returns an error if that owner has no matching inbox item.
    pub fn acknowledge(&self, owner_id: &OwnerId, result_id: ResultId) -> Result<(), StoreError> {
        let transaction = self.write_transaction()?;
        let changed = transaction.execute(
            "UPDATE inbox_items SET acknowledged = 1
             WHERE owner_id = ?1 AND result_id = ?2",
            params![owner_id.as_str(), result_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InboxItemNotFound);
        }
        transaction.execute(
            "UPDATE completion_notifications SET resolved = 1,
               claim_token = NULL, claim_until = 0 WHERE result_id = ?1",
            [result_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Acknowledges an inbox item only while the caller's session epoch is
    /// still current. The binding check and acknowledgment are one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for an unbound/stale session, missing item, or DB failure.
    pub fn acknowledge_bound(
        &self,
        owner_id: &OwnerId,
        result_id: ResultId,
        session_id: &str,
        binding_epoch: u64,
    ) -> Result<(), StoreError> {
        let transaction = self.write_transaction()?;
        assert_owner_binding(
            &transaction,
            owner_id,
            Some(session_id),
            Some(binding_epoch),
        )?;
        let changed = transaction.execute(
            "UPDATE inbox_items SET acknowledged = 1 WHERE owner_id = ?1 AND result_id = ?2",
            params![owner_id.as_str(), result_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::InboxItemNotFound);
        }
        transaction.execute(
            "UPDATE completion_notifications SET resolved = 1,
               claim_token = NULL, claim_until = 0 WHERE result_id = ?1",
            [result_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records the sole accept/reject decision for a result.
    ///
    /// Semantic replay is idempotent even when a caller generated a new
    /// `DecisionId` after a lost response. A different verdict or reason is
    /// rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for missing results, digest/owner mismatches,
    /// conflicting decisions, invalid serialization, or database failure.
    pub fn record_decision(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        retry_busy(|| self.record_decision_once(decision))
    }

    fn record_decision_once(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        let transaction = self.write_transaction()?;
        let outcome = record_decision_in_transaction(&transaction, &self.artifacts, decision)?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Records an accept/reject decision and acknowledges its owner's inbox
    /// item in one transaction. A semantic retry remains idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing inbox item, mismatched owner or digest,
    /// conflicting decision, or failed transaction.
    pub fn record_decision_and_ack(&self, decision: &Decision) -> Result<WriteOutcome, StoreError> {
        retry_busy(|| self.record_decision_and_ack_once(decision))
    }

    fn record_decision_and_ack_once(
        &self,
        decision: &Decision,
    ) -> Result<WriteOutcome, StoreError> {
        let transaction = self.write_transaction()?;
        let outcome = record_decision_in_transaction(&transaction, &self.artifacts, decision)?;
        let changed = transaction.execute(
            "UPDATE inbox_items SET acknowledged = 1 WHERE owner_id = ?1 AND result_id = ?2",
            params![decision.owner_id.as_str(), decision.result_id.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::InboxItemNotFound);
        }
        transaction.execute(
            "UPDATE completion_notifications SET resolved = 1,
               claim_token = NULL, claim_until = 0 WHERE result_id = ?1",
            [decision.result_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Returns the stored decision for one terminal result, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored decision is invalid or storage fails.
    pub fn decision_for_result(&self, result_id: ResultId) -> Result<Option<Decision>, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT decision_json FROM decisions WHERE result_id = ?1",
                [result_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    /// Returns the decision recorded against one task revision's result.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data or a database failure.
    pub fn decision_for_revision(
        &self,
        task_id: TaskId,
        revision: u32,
    ) -> Result<Option<Decision>, StoreError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT d.decision_json FROM decisions d
                 JOIN results r ON r.result_id = d.result_id
                 WHERE r.task_id = ?1 AND r.revision = ?2
                 ORDER BY r.rowid DESC LIMIT 1",
                params![task_id.to_string(), revision],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    /// Returns the latest terminal result for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when no terminal result exists or stored data is invalid.
    pub fn latest_result(&self, task_id: TaskId) -> Result<ResultEnvelope, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT envelope_json FROM results WHERE task_id = ?1
                 AND revision = (SELECT MAX(revision) FROM tasks WHERE task_id = ?1)
                 ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        Ok(serde_json::from_str(&json)?)
    }
}
