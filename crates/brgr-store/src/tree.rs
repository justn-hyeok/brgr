use brgr_protocol::{AttemptId, TaskId, TaskSpec};
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};

use super::{Store, StoreError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubtreeNode {
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub depth: u32,
}

impl Store {
    /// Lists the exact recorded descendants, deepest first.
    ///
    /// # Errors
    ///
    /// Rejects a missing root, malformed IDs, or more than 256 tasks.
    pub fn subtree(&self, root: TaskId) -> Result<Vec<SubtreeNode>, StoreError> {
        self.task(root)?;
        query_subtree(&self.connection, root)
    }

    /// Atomically records cancellation intent for one task or its entire
    /// current subtree before any cancel files are written.
    ///
    /// # Errors
    ///
    /// Returns an error for missing tasks, oversize trees, or database failure.
    pub fn record_cancellation_intents(
        &self,
        root: TaskId,
        include_descendants: bool,
    ) -> Result<Vec<SubtreeNode>, StoreError> {
        self.task(root)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let nodes = if include_descendants {
            query_subtree(&transaction, root)?
        } else {
            vec![SubtreeNode {
                task_id: root,
                parent_task_id: None,
                depth: 0,
            }]
        };
        for node in &nodes {
            let revision: u32 = transaction.query_row(
                "SELECT MAX(revision) FROM tasks WHERE task_id = ?1",
                [node.task_id.to_string()],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO cancellation_intents (task_id, revision, requested_at)
                 VALUES (?1, ?2, CAST(strftime('%s','now') AS INTEGER))
                 ON CONFLICT(task_id) DO UPDATE SET revision = excluded.revision,
                     requested_at = excluded.requested_at",
                params![node.task_id.to_string(), revision],
            )?;
        }
        transaction.commit()?;
        Ok(nodes)
    }

    /// Returns whether cancellation has already been requested for a task.
    ///
    /// # Errors
    ///
    /// Returns an error if the database lookup fails.
    pub fn cancellation_requested(&self, task_id: TaskId) -> Result<bool, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM cancellation_intents WHERE task_id = ?1
                 AND revision = (SELECT MAX(revision) FROM tasks WHERE task_id = ?1)",
                [task_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    /// Reads the start clock for the latest attempt on a task, if present.
    ///
    /// # Errors
    ///
    /// Returns an error if the database lookup fails.
    pub fn latest_attempt_clock(&self, task_id: TaskId) -> Result<Option<i64>, StoreError> {
        self.connection
            .query_row(
                "SELECT c.started_at FROM attempts a
                 JOIN attempt_clocks c ON c.attempt_id = a.attempt_id
                 WHERE a.task_id = ?1 ORDER BY a.rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Counts child tasks whose latest revision has no terminal result.
    ///
    /// # Errors
    ///
    /// Returns an error if the database lookup fails.
    pub fn active_child_count(&self, attempt_id: AttemptId) -> Result<u64, StoreError> {
        let count = active_child_count(&self.connection, attempt_id)?;
        u64::try_from(count).map_err(|_| StoreError::NumericOverflow)
    }

    /// Reads the task spec frozen for an exact attempt.
    ///
    /// # Errors
    ///
    /// Returns an error if the attempt or task data is invalid.
    pub fn task_for_attempt(&self, attempt_id: AttemptId) -> Result<TaskSpec, StoreError> {
        let json: String = self.connection.query_row(
            "SELECT t.spec_json FROM attempts a JOIN tasks t
             ON t.task_id = a.task_id AND t.revision = a.revision
             WHERE a.attempt_id = ?1",
            [attempt_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str(&json)?)
    }
}

pub(super) fn active_child_count(
    connection: &Connection,
    parent_attempt_id: AttemptId,
) -> Result<i64, StoreError> {
    Ok(connection.query_row(
        "SELECT COUNT(*) FROM delegation_edges e
         JOIN tasks t ON t.task_id = e.child_task_id
           AND t.revision = (SELECT MAX(t2.revision) FROM tasks t2 WHERE t2.task_id = e.child_task_id)
         WHERE e.parent_attempt_id = ?1
           AND (NOT EXISTS (SELECT 1 FROM results r
                            WHERE r.task_id = t.task_id AND r.revision = t.revision)
                OR (EXISTS (SELECT 1 FROM results r
                            WHERE r.task_id = t.task_id AND r.revision = t.revision
                              AND json_extract(r.envelope_json, '$.outcome') = 'failed')
                    AND NOT EXISTS (SELECT 1 FROM task_run_completions c
                                    WHERE c.task_id = t.task_id AND c.revision = t.revision)))",
        [parent_attempt_id.to_string()],
        |row| row.get(0),
    )?)
}

fn query_subtree(connection: &Connection, root: TaskId) -> Result<Vec<SubtreeNode>, StoreError> {
    let mut statement = connection.prepare(
        "WITH RECURSIVE nodes(task_id, parent_task_id, depth) AS (
             SELECT ?1, NULL, 0
             UNION ALL
             SELECT e.child_task_id, e.parent_task_id, nodes.depth + 1
             FROM delegation_edges e JOIN nodes ON e.parent_task_id = nodes.task_id
             WHERE nodes.depth < 8
         ) SELECT task_id, parent_task_id, depth FROM nodes ORDER BY depth DESC LIMIT 257",
    )?;
    let rows = statement.query_map(params![root.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, u32>(2)?,
        ))
    })?;
    let nodes: Vec<SubtreeNode> = rows
        .map(|row| {
            let (task, parent, depth) = row?;
            Ok(SubtreeNode {
                task_id: task
                    .parse()
                    .map_err(|_| StoreError::InvalidDelegationParent)?,
                parent_task_id: parent
                    .map(|value| {
                        value
                            .parse()
                            .map_err(|_| StoreError::InvalidDelegationParent)
                    })
                    .transpose()?,
                depth,
            })
        })
        .collect::<Result<_, StoreError>>()?;
    if nodes.len() > 256 {
        return Err(StoreError::SubtreeTooLarge);
    }
    Ok(nodes)
}
