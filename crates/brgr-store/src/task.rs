//! Task rows: admission, revisions, checkouts, and task queries.

use brgr_protocol::{AttemptId, OwnerId, TaskId, TaskSpec};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use super::{
    OwnerProof, Store, StoreError, WriteOutcome,
    delegation::{recorded_delegation_parent, validate_new_task_parent, validated_parent_depth},
    prefix_upper_bound, record_idempotency,
    takeover::{assert_owner_proof, note_owner_action},
    validate_digest,
};

impl Store {
    /// Records a task revision, enforcing its create-request digest.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid task, serialization failure, conflicting
    /// idempotency digest, or database failure.
    pub fn record_task(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(task, request_digest, None, None)
    }

    /// Records a task revision only while the caller's session and epoch are
    /// still the owner's binding.
    ///
    /// This is the owner-side admission path, and it is also how a committed
    /// handoff learns that the incoming session took the work rather than only
    /// the binding: the new revision is recorded in the same transaction that
    /// moves the takeover to `resumed`.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale session or an owner the proof does not
    /// match, and otherwise the same errors as [`Self::record_task`].
    pub fn record_task_bound(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        proof: &OwnerProof,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(task, request_digest, None, Some(proof))
    }

    /// Records a child task only while its exact parent attempt is active.
    /// The parent attempt, not a mutable pane or task name, owns the edge.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale parent, wrong child owner, excessive depth,
    /// conflicting replay, or the same errors as [`Self::record_task`].
    pub fn record_child_task(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        parent_task_id: TaskId,
        parent_attempt_id: AttemptId,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(
            task,
            request_digest,
            Some((parent_task_id, parent_attempt_id)),
            None,
        )
    }

    /// [`Self::record_child_task`] for a session that proved it is the head.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale session or an owner the proof does not
    /// match, and otherwise the same errors as [`Self::record_child_task`].
    pub fn record_child_task_bound(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        parent_task_id: TaskId,
        parent_attempt_id: AttemptId,
        proof: &OwnerProof,
    ) -> Result<WriteOutcome, StoreError> {
        self.record_task_with_parent(
            task,
            request_digest,
            Some((parent_task_id, parent_attempt_id)),
            Some(proof),
        )
    }

    /// Deliberately not retried, for the reason given on [`BUSY_RETRY_BUDGET`]:
    /// task admission runs inside the repository admission lock, so waiting here
    /// blocks every other admission on the same repository and turns one failure
    /// into two. It relies on `busy_timeout` alone and fails fast enough to
    /// release that lock.
    fn record_task_with_parent(
        &mut self,
        task: &TaskSpec,
        request_digest: &str,
        parent: Option<(TaskId, AttemptId)>,
        proof: Option<&OwnerProof>,
    ) -> Result<WriteOutcome, StoreError> {
        task.validate()?;
        validate_digest(request_digest)?;
        let transaction = self.write_transaction()?;
        let outcome =
            record_task_in_transaction(&transaction, task, request_digest, parent, proof)?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Loads the latest revision for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or stored data is invalid.
    pub fn task(&self, task_id: TaskId) -> Result<TaskSpec, StoreError> {
        let json = self
            .connection
            .query_row(
                "SELECT spec_json FROM tasks WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        Ok(serde_json::from_str(&json)?)
    }

    /// The highest recorded revision number for a task.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TaskNotFound`] when the task is missing.
    pub fn latest_revision(&self, task_id: TaskId) -> Result<u32, StoreError> {
        latest_revision_on(&self.connection, task_id)
    }

    /// Lists the most recently created task revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when stored data is invalid or storage fails.
    pub fn tasks(&self, limit: usize) -> Result<Vec<TaskSpec>, StoreError> {
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             JOIN (SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id) latest
             ON latest.task_id = t.task_id AND latest.revision = t.revision
             ORDER BY t.rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([bounded], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Records the primary checkout of the repository a task worktree was
    /// created from.
    ///
    /// The task spec records the worktree brgr created, not the repository it
    /// came from, and once that worktree is deleted by hand nothing else can say
    /// which repository still holds its branch. `brgr prune` needs exactly that.
    /// Written inside the admission lock, so it does not wait on contention.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails.
    pub fn record_task_checkout(
        &self,
        task_id: TaskId,
        revision: u32,
        primary_checkout: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO task_checkouts (task_id, revision, primary_checkout)
             VALUES (?1, ?2, ?3)",
            params![task_id.to_string(), revision, primary_checkout],
        )?;
        Ok(())
    }

    /// The recorded primary checkout for one task revision, if brgr recorded it.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails.
    pub fn task_checkout(
        &self,
        task_id: TaskId,
        revision: u32,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT primary_checkout FROM task_checkouts WHERE task_id = ?1 AND revision = ?2",
                params![task_id.to_string(), revision],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Every distinct primary checkout brgr has recorded a task worktree for.
    ///
    /// # Errors
    ///
    /// Returns an error when storage fails.
    pub fn task_checkouts(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT primary_checkout FROM task_checkouts ORDER BY 1")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Resolves the task revisions whose id starts with `prefix`.
    ///
    /// A brgr-owned worktree directory is named after a short task-id prefix, so
    /// recovering the task it belongs to needs a prefix lookup. More than one
    /// match is returned rather than guessed.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-hexadecimal prefix, invalid stored data, or a
    /// database failure.
    pub fn task_revisions_with_prefix(
        &self,
        prefix: &str,
        revision: u32,
    ) -> Result<Vec<TaskSpec>, StoreError> {
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidTaskPrefix);
        }
        // Stored ids are lowercase, so the range bounds have to be too.
        let prefix = prefix.to_ascii_lowercase();
        let prefix = prefix.as_str();
        let mut statement = self.connection.prepare(
            "SELECT spec_json FROM tasks
             WHERE task_id >= ?1 AND task_id < ?2 AND revision = ?3
             ORDER BY task_id",
        )?;
        // A half-open range on the primary key beats LIKE: it uses the index and
        // cannot be widened by a wildcard inside the prefix.
        let upper = prefix_upper_bound(prefix);
        let rows = statement.query_map(params![prefix, upper, revision], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Lists latest task revisions whose owner is currently bound to a session.
    /// The optional owner narrows an explicit `BRGR_OWNER_ID` without allowing
    /// unrelated owners to consume the result limit.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data, limits, or DB failures.
    pub fn tasks_for_session(
        &self,
        session_id: &str,
        owner_id: Option<&OwnerId>,
        limit: usize,
    ) -> Result<Vec<TaskSpec>, StoreError> {
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             JOIN owner_bindings b ON b.owner_id = t.owner_id
             JOIN (SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id) latest
               ON latest.task_id = t.task_id AND latest.revision = t.revision
             WHERE b.session_id = ?1 AND (?2 IS NULL OR t.owner_id = ?2)
             ORDER BY t.rowid DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![session_id, owner_id.map(OwnerId::as_str), bounded],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Latest revisions owned by `owner_id` that do not yet have a terminal result.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data or a database failure.
    pub fn unfinished_tasks_for_owner(
        &self,
        owner_id: &OwnerId,
    ) -> Result<Vec<TaskSpec>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             JOIN (SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id) latest
               ON latest.task_id = t.task_id AND latest.revision = t.revision
             WHERE t.owner_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM results r
                   WHERE r.task_id = t.task_id AND r.revision = t.revision
               )
             ORDER BY t.rowid",
        )?;
        let rows = statement.query_map([owner_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Returns admitted task revisions for which no supervisor ever claimed
    /// an attempt. The caller can reconcile an abandoned launch without guessing
    /// that a model run succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored task data or a database failure.
    pub fn unstarted_tasks(&self) -> Result<Vec<TaskSpec>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT t.spec_json FROM tasks t
             WHERE NOT EXISTS (
                 SELECT 1 FROM attempts a
                 WHERE a.task_id = t.task_id AND a.revision = t.revision
             ) ORDER BY t.rowid",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

pub(crate) fn latest_revision_on(
    connection: &Connection,
    task_id: TaskId,
) -> Result<u32, StoreError> {
    connection
        .query_row(
            "SELECT MAX(revision) FROM tasks WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get::<_, Option<u32>>(0),
        )?
        .ok_or(StoreError::TaskNotFound(task_id))
}

/// The body of task admission, shared with workflow step recording so a step
/// row and its task are written in one transaction rather than copied.
pub(crate) fn record_task_in_transaction(
    transaction: &Transaction<'_>,
    task: &TaskSpec,
    request_digest: &str,
    parent: Option<(TaskId, AttemptId)>,
    proof: Option<&OwnerProof>,
) -> Result<WriteOutcome, StoreError> {
    if let Some(proof) = proof {
        assert_owner_proof(transaction, proof)?;
        // A revision may be the task's first, so the owner compared here is
        // the one the spec claims, not one already on a row.
        if task.owner_id != proof.owner_id {
            return Err(StoreError::OwnerProofMismatch);
        }
        // The third owner-side action a committed handoff waits to observe.
        note_owner_action(
            transaction,
            &proof.owner_id,
            &proof.session_id,
            proof.binding_epoch,
        )?;
    }
    let idempotency = record_idempotency(transaction, &task.create_request_id, request_digest)?;
    let recorded_parent = recorded_delegation_parent(transaction, task.task_id)?;
    let requested_parent =
        parent.map(|(task_id, attempt_id)| (task_id.to_string(), attempt_id.to_string()));
    if idempotency == WriteOutcome::AlreadyApplied {
        return replayed_task(
            transaction,
            task,
            recorded_parent.as_ref(),
            requested_parent.as_ref(),
        );
    }

    validate_new_task_parent(
        transaction,
        task.task_id,
        recorded_parent.as_ref(),
        requested_parent.as_ref(),
    )?;

    let depth = validated_parent_depth(transaction, task, parent)?;

    let spec_json = serde_json::to_string(task)?;
    transaction.execute(
        "INSERT INTO tasks
         (task_id, revision, owner_id, create_request_id, request_digest, spec_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            task.task_id.to_string(),
            task.revision,
            task.owner_id.as_str(),
            task.create_request_id,
            request_digest,
            spec_json,
        ],
    )?;
    if let (Some((parent_task_id, parent_attempt_id)), Some(depth), None) =
        (parent, depth, recorded_parent)
    {
        transaction.execute(
            "INSERT INTO delegation_edges (child_task_id, parent_task_id, parent_attempt_id, depth) VALUES (?1, ?2, ?3, ?4)",
            params![task.task_id.to_string(), parent_task_id.to_string(), parent_attempt_id.to_string(), depth],
        )?;
    }
    Ok(WriteOutcome::Inserted)
}

fn replayed_task(
    transaction: &Transaction<'_>,
    task: &TaskSpec,
    recorded_parent: Option<&(String, String)>,
    requested_parent: Option<&(String, String)>,
) -> Result<WriteOutcome, StoreError> {
    let matches = transaction
        .query_row(
            "SELECT task_id, revision FROM tasks WHERE create_request_id = ?1",
            [&task.create_request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
        )
        .optional()?
        .is_some_and(|(task_id, revision)| {
            task_id == task.task_id.to_string() && revision == task.revision
        });
    if !matches {
        return Err(StoreError::IdempotencyConflict(
            task.create_request_id.clone(),
        ));
    }
    if requested_parent.is_some() && recorded_parent != requested_parent {
        return Err(StoreError::InvalidDelegationParent);
    }
    Ok(WriteOutcome::AlreadyApplied)
}
