//! Parent and child task edges.
//!
//! The parent attempt, not a pane or a task name, owns the edge, so everything
//! that reads or validates one is here rather than spread through admission.

use rusqlite::{OptionalExtension as _, Transaction, params};

use super::{Store, StoreError, tree};
use brgr_protocol::{AttemptId, TaskId, TaskSpec};

impl Store {
    /// Checks a proposed parent before creating any child worktree. The
    /// transactional check in `record_child_task` remains authoritative.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale parent, wrong owner, excessive depth, or
    /// a database failure.
    pub fn validate_delegation_parent(
        &self,
        parent_task_id: TaskId,
        parent_attempt_id: AttemptId,
        child_owner: &brgr_protocol::OwnerId,
    ) -> Result<(), StoreError> {
        let parent: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT task_id, state FROM attempts WHERE attempt_id = ?1",
                [parent_attempt_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if !parent.is_some_and(|(id, state)| {
            id == parent_task_id.to_string() && matches!(state.as_str(), "running" | "blocked")
        }) || child_owner.as_str() != format!("worker:{parent_attempt_id}")
        {
            return Err(StoreError::InvalidDelegationParent);
        }
        if self.cancellation_requested(parent_task_id)? {
            return Err(StoreError::DelegationParentCancelled);
        }
        let depth: u32 = self
            .connection
            .query_row(
                "SELECT depth FROM delegation_edges WHERE child_task_id = ?1",
                [parent_task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if depth >= 8 {
            return Err(StoreError::DelegationDepthExceeded);
        }
        let parent_spec = self.task_for_attempt(parent_attempt_id)?;
        if self.active_child_count(parent_attempt_id)?
            >= u64::from(parent_spec.max_concurrent_children.unwrap_or(2))
        {
            return Err(StoreError::ConcurrentChildLimit);
        }
        Ok(())
    }
    /// Returns the stable parent attempt for a child task, when one exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be read or an ID is malformed.
    pub fn delegation_parent(
        &self,
        child_task_id: TaskId,
    ) -> Result<Option<(TaskId, AttemptId, u32)>, StoreError> {
        self.connection
            .query_row(
                "SELECT parent_task_id, parent_attempt_id, depth FROM delegation_edges WHERE child_task_id = ?1",
                [child_task_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?)),
            )
            .optional()?
            .map(|(task, attempt, depth)| {
                Ok((
                    task.parse().map_err(|_| StoreError::InvalidDelegationParent)?,
                    attempt.parse().map_err(|_| StoreError::InvalidDelegationParent)?,
                    depth,
                ))
            })
            .transpose()
    }
    /// Counts children whose latest revision has not been delivered and
    /// acknowledged by this parent worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata query fails.
    pub fn unsettled_children(&self, parent_attempt_id: AttemptId) -> Result<u64, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM delegation_edges e
             JOIN tasks t ON t.task_id = e.child_task_id
               AND t.revision = (SELECT MAX(t2.revision) FROM tasks t2 WHERE t2.task_id = e.child_task_id)
             LEFT JOIN results r ON r.result_id = (
               SELECT latest.result_id FROM results latest
               WHERE latest.task_id = t.task_id AND latest.revision = t.revision
               ORDER BY latest.rowid DESC LIMIT 1
             )
             LEFT JOIN inbox_items i ON i.result_id = r.result_id AND i.owner_id = t.owner_id
             LEFT JOIN decisions d ON d.result_id = r.result_id
             WHERE e.parent_attempt_id = ?1
               AND (r.result_id IS NULL OR i.acknowledged IS NULL OR i.acknowledged = 0
                 OR (json_extract(r.envelope_json, '$.outcome') = 'candidate' AND d.result_id IS NULL))",
            [parent_attempt_id.to_string()],
            |row| row.get(0),
        )?;
        u64::try_from(count).map_err(|_| StoreError::NumericOverflow)
    }
}

pub(crate) fn recorded_delegation_parent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
) -> Result<Option<(String, String)>, StoreError> {
    transaction
        .query_row(
            "SELECT parent_task_id, parent_attempt_id FROM delegation_edges WHERE child_task_id = ?1",
            [task_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)
}
pub(crate) fn validate_new_task_parent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    recorded: Option<&(String, String)>,
    requested: Option<&(String, String)>,
) -> Result<(), StoreError> {
    if recorded.is_some() && recorded != requested {
        return Err(StoreError::InvalidDelegationParent);
    }
    if recorded.is_none() && requested.is_some() {
        let prior_task: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM tasks WHERE task_id = ?1 LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if prior_task.is_some() {
            return Err(StoreError::InvalidDelegationParent);
        }
    }
    Ok(())
}
pub(crate) fn validated_parent_depth(
    transaction: &Transaction<'_>,
    task: &TaskSpec,
    parent: Option<(TaskId, AttemptId)>,
) -> Result<Option<u32>, StoreError> {
    let Some((parent_task_id, parent_attempt_id)) = parent else {
        return Ok(None);
    };
    let parent_attempt: Option<(String, String)> = transaction
        .query_row(
            "SELECT task_id, state FROM attempts WHERE attempt_id = ?1",
            [parent_attempt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if !parent_attempt.is_some_and(|(id, state)| {
        id == parent_task_id.to_string() && matches!(state.as_str(), "running" | "blocked")
    }) || task.owner_id.as_str() != format!("worker:{parent_attempt_id}")
    {
        return Err(StoreError::InvalidDelegationParent);
    }
    let cancelling: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM cancellation_intents WHERE task_id = ?1
             AND revision = (SELECT revision FROM attempts WHERE attempt_id = ?2)",
            params![parent_task_id.to_string(), parent_attempt_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if cancelling.is_some() {
        return Err(StoreError::DelegationParentCancelled);
    }
    let parent_spec_json: String = transaction.query_row(
        "SELECT t.spec_json FROM attempts a JOIN tasks t
         ON t.task_id = a.task_id AND t.revision = a.revision
         WHERE a.attempt_id = ?1",
        [parent_attempt_id.to_string()],
        |row| row.get(0),
    )?;
    let parent_spec: TaskSpec = serde_json::from_str(&parent_spec_json)?;
    let active_children = tree::active_child_count(transaction, parent_attempt_id)?;
    if active_children >= i64::from(parent_spec.max_concurrent_children.unwrap_or(2)) {
        return Err(StoreError::ConcurrentChildLimit);
    }
    let parent_depth: u32 = transaction
        .query_row(
            "SELECT depth FROM delegation_edges WHERE child_task_id = ?1",
            [parent_task_id.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if parent_depth >= 8 {
        return Err(StoreError::DelegationDepthExceeded);
    }
    Ok(Some(parent_depth + 1))
}
