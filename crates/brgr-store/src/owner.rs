//! Which Codex session owns a task, and the epoch that makes a move explicit.

use rusqlite::{OptionalExtension as _, Transaction, params};

use super::{Store, StoreError, WriteOutcome};
use brgr_protocol::OwnerId;

impl Store {
    /// Initially binds an owner to an explicit session epoch without inferring
    /// focus. A late hook cannot replace a different session; use
    /// `rebind_owner` for an explicit transfer.
    ///
    /// # Errors
    ///
    /// Returns an error when another session is already bound or persistence
    /// fails.
    pub fn bind_owner(
        &self,
        owner_id: &OwnerId,
        session_id: &str,
        binding_epoch: u64,
    ) -> Result<WriteOutcome, StoreError> {
        if session_id.trim().is_empty() || binding_epoch == 0 {
            return Err(StoreError::InvalidOwnerBinding);
        }
        let binding_epoch =
            i64::try_from(binding_epoch).map_err(|_| StoreError::NumericOverflow)?;
        let transaction = self.write_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((stored_session, _stored_epoch)) = existing {
            if stored_session == session_id {
                return Ok(WriteOutcome::AlreadyApplied);
            }
            return Err(StoreError::OwnerBindingConflict);
        }
        transaction.execute(
            "INSERT INTO owner_bindings (owner_id, session_id, binding_epoch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(owner_id) DO UPDATE SET
               session_id = excluded.session_id,
               binding_epoch = excluded.binding_epoch",
            params![owner_id.as_str(), session_id, binding_epoch],
        )?;
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }
    /// Reads the current cooperative session binding for an owner.
    ///
    /// # Errors
    ///
    /// Returns an error if persistence or epoch conversion fails.
    pub fn owner_binding(&self, owner_id: &OwnerId) -> Result<Option<(String, u64)>, StoreError> {
        self.connection
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(session, epoch)| {
                Ok((
                    session,
                    u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)?,
                ))
            })
            .transpose()
    }
    /// Explicitly transfers an owner to a new session with a larger epoch.
    /// Existing inbox items and decisions remain under the same owner ID.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty session, epoch overflow, or DB failure.
    pub fn rebind_owner(&self, owner_id: &OwnerId, session_id: &str) -> Result<u64, StoreError> {
        if session_id.trim().is_empty() {
            return Err(StoreError::InvalidOwnerBinding);
        }
        let transaction = self.write_transaction()?;
        let prior: Option<(String, i64)> = transaction
            .query_row(
                "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
                [owner_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_session, epoch)) = &prior
            && stored_session == session_id
        {
            return u64::try_from(*epoch).map_err(|_| StoreError::NumericOverflow);
        }
        let epoch = prior.map_or(Ok(1_i64), |(_, epoch)| {
            epoch.checked_add(1).ok_or(StoreError::NumericOverflow)
        })?;
        transaction.execute(
            "INSERT INTO owner_bindings (owner_id, session_id, binding_epoch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(owner_id) DO UPDATE SET
               session_id = excluded.session_id,
               binding_epoch = excluded.binding_epoch",
            params![owner_id.as_str(), session_id, epoch],
        )?;
        transaction.execute(
            "UPDATE completion_notifications SET delivered_session = NULL,
               delivered_epoch = NULL, delivered_pane = NULL,
               claim_token = NULL, claim_until = 0
             WHERE owner_id = ?1 AND resolved = 0
               AND EXISTS (SELECT 1 FROM inbox_items i
                   WHERE i.result_id = completion_notifications.result_id
                     AND i.owner_id = ?1 AND i.acknowledged = 0)",
            [owner_id.as_str()],
        )?;
        transaction.commit()?;
        u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)
    }
}

pub(crate) fn assert_owner_binding(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    session_id: Option<&str>,
    binding_epoch: Option<u64>,
) -> Result<(), StoreError> {
    let Some((bound_session, bound_epoch)) = transaction
        .query_row(
            "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
            [owner_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Err(StoreError::OwnerUnbound(owner_id.clone()));
    };
    if session_id != Some(bound_session.as_str())
        || binding_epoch != u64::try_from(bound_epoch).ok()
    {
        return Err(StoreError::OwnerBindingConflict);
    }
    Ok(())
}
