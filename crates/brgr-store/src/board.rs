//! Read-only Herdr board projection. Never returns objective or artifact bytes.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::StoreError;
use brgr_protocol::{AttemptState, DecisionVerdict, ResultId, SCHEMA_V1, TaskId, TerminalOutcome};
use rusqlite::{Connection, OpenFlags, Transaction};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// Read-only Herdr board row. Never includes the task objective or artifact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoardTaskRow {
    pub task_id: TaskId,
    pub revision: u32,
    pub workspace: String,
    pub harness_id: String,
    pub attempt_state: AttemptState,
    pub result_outcome: Option<TerminalOutcome>,
    pub decision_verdict: Option<DecisionVerdict>,
}

/// Existing-database reader for the Herdr board. Never applies schema and never
/// opens artifact storage.
pub struct BoardStore {
    connection: Option<Connection>,
}

struct ProjectedBoardRow {
    stored_task_id: String,
    stored_revision: String,
    spec_schema: Option<String>,
    spec_task_id: Option<String>,
    spec_revision: Option<String>,
    workspace: Option<String>,
    harness_id: Option<String>,
    state: Option<String>,
    stored_result_id: Option<String>,
    result_schema: Option<String>,
    result_task_id: Option<String>,
    result_revision: Option<String>,
    result_id: Option<String>,
    outcome: Option<String>,
    decision_schema: Option<String>,
    decision_task_id: Option<String>,
    decision_revision: Option<String>,
    decision_result_id: Option<String>,
    verdict: Option<String>,
}

impl BoardStore {
    /// Opens a query-only connection to an already initialized store database.
    ///
    /// A missing database is treated as an empty board. An existing file is
    /// opened read-only with a one-second busy timeout and never receives
    /// schema or artifact initialization.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing database cannot be opened.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let database_path = PathBuf::from(root.as_ref()).join("brgr.sqlite3");
        if !database_path.is_file() {
            return Ok(Self { connection: None });
        }
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    /// Lists newest task revisions for the read-only Herdr board.
    ///
    /// One deferred snapshot joins the latest revision, its latest attempt,
    /// that revision's latest terminal result, and the decision for that
    /// result. Malformed specs and invalid attempt states are skipped. A
    /// decision is shown only when that same result envelope is valid. Rows
    /// never include objective text or artifact bytes. SQL projects only the
    /// JSON fields needed by the board into Rust values.
    ///
    /// # Errors
    ///
    /// Returns an error for a database failure.
    pub fn rows(&self, limit: usize) -> Result<Vec<BoardTaskRow>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let Some(connection) = self.connection.as_ref() else {
            return Ok(Vec::new());
        };
        load_board_rows(connection, bounded)
    }
}

fn load_board_rows(connection: &Connection, limit: i64) -> Result<Vec<BoardTaskRow>, StoreError> {
    let transaction = connection.unchecked_transaction()?;
    let rows = load_board_rows_in_transaction(&transaction, limit)?;
    transaction.commit()?;
    Ok(rows)
}

fn load_board_rows_in_transaction(
    transaction: &Transaction<'_>,
    limit: i64,
) -> Result<Vec<BoardTaskRow>, StoreError> {
    let mut statement = transaction.prepare(
        "WITH latest_attempts AS (
             SELECT a.task_id, a.revision, a.state
             FROM attempts a
             WHERE a.rowid = (
                 SELECT MAX(candidate.rowid) FROM attempts candidate
                 WHERE candidate.task_id = a.task_id AND candidate.revision = a.revision
             )
         ),
         latest_results AS (
             SELECT r.result_id, r.task_id, r.revision, r.envelope_json
             FROM results r
             WHERE r.rowid = (
                 SELECT MAX(candidate.rowid) FROM results candidate
                 WHERE candidate.task_id = r.task_id AND candidate.revision = r.revision
             )
         )
         SELECT CAST(t.task_id AS TEXT), CAST(t.revision AS TEXT),
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.schema') = 'text' THEN CAST(json_extract(t.spec_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.task_id') = 'text' THEN CAST(json_extract(t.spec_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.revision') = 'integer' THEN CAST(json_extract(t.spec_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.workspace') = 'text' THEN CAST(json_extract(t.spec_json, '$.workspace') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.route.harness_id') = 'text' THEN CAST(json_extract(t.spec_json, '$.route.harness_id') AS TEXT) END END,
                CAST(a.state AS TEXT),
                CAST(r.result_id AS TEXT),
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.schema') = 'text' THEN CAST(json_extract(r.envelope_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.task_id') = 'text' THEN CAST(json_extract(r.envelope_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.revision') = 'integer' THEN CAST(json_extract(r.envelope_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.result_id') = 'text' THEN CAST(json_extract(r.envelope_json, '$.result_id') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.outcome') = 'text' THEN CAST(json_extract(r.envelope_json, '$.outcome') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.schema') = 'text' THEN CAST(json_extract(d.decision_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.task_id') = 'text' THEN CAST(json_extract(d.decision_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.revision') = 'integer' THEN CAST(json_extract(d.decision_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.result_id') = 'text' THEN CAST(json_extract(d.decision_json, '$.result_id') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.verdict') = 'text' THEN CAST(json_extract(d.decision_json, '$.verdict') AS TEXT) END END
         FROM tasks t
         JOIN (
             SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id
         ) latest
           ON latest.task_id = t.task_id AND latest.revision = t.revision
         LEFT JOIN latest_attempts a
           ON a.task_id = t.task_id AND a.revision = t.revision
         LEFT JOIN latest_results r
           ON r.task_id = t.task_id AND r.revision = t.revision
         LEFT JOIN decisions d ON d.result_id = r.result_id
         ORDER BY t.rowid DESC
         LIMIT 100",
    )?;
    let mapped = statement.query_map([], |row| {
        Ok(ProjectedBoardRow {
            stored_task_id: row.get(0)?,
            stored_revision: row.get(1)?,
            spec_schema: row.get(2)?,
            spec_task_id: row.get(3)?,
            spec_revision: row.get(4)?,
            workspace: row.get(5)?,
            harness_id: row.get(6)?,
            state: row.get(7)?,
            stored_result_id: row.get(8)?,
            result_schema: row.get(9)?,
            result_task_id: row.get(10)?,
            result_revision: row.get(11)?,
            result_id: row.get(12)?,
            outcome: row.get(13)?,
            decision_schema: row.get(14)?,
            decision_task_id: row.get(15)?,
            decision_revision: row.get(16)?,
            decision_result_id: row.get(17)?,
            verdict: row.get(18)?,
        })
    })?;
    let mut rows = Vec::new();
    for entry in mapped {
        let Some(row) = parse_board_row(entry?) else {
            continue;
        };
        rows.push(row);
        if i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
    }
    Ok(rows)
}

fn parse_board_row(row: ProjectedBoardRow) -> Option<BoardTaskRow> {
    let task_id = row.stored_task_id.parse::<TaskId>().ok()?;
    let revision = row.stored_revision.parse::<u32>().ok()?;
    let workspace = row.workspace?;
    let harness_id = row.harness_id?;
    if row.spec_schema.as_deref() != Some(SCHEMA_V1)
        || row.spec_task_id.as_deref() != Some(row.stored_task_id.as_str())
        || row.spec_revision.as_deref() != Some(row.stored_revision.as_str())
        || harness_id.trim().is_empty()
        || workspace.trim().is_empty()
    {
        return None;
    }
    let attempt_state = match row.state.as_deref() {
        None => AttemptState::Queued,
        Some(value) => parse_attempt_state(value)?,
    };
    let result = row
        .stored_result_id
        .as_deref()
        .and_then(|id| id.parse::<ResultId>().ok())
        .and_then(|stored_result_id| {
            let envelope_result_id = row.result_id.as_deref()?.parse::<ResultId>().ok()?;
            let outcome = parse_terminal_outcome(row.outcome.as_deref()?)?;
            (row.result_schema.as_deref() == Some(SCHEMA_V1)
                && row.result_task_id.as_deref() == Some(row.stored_task_id.as_str())
                && row.result_revision.as_deref() == Some(row.stored_revision.as_str())
                && envelope_result_id == stored_result_id)
                .then_some((stored_result_id, outcome))
        });
    let decision_verdict = result.as_ref().and_then(|(result_id, _)| {
        let decision_result_id = row
            .decision_result_id
            .as_deref()?
            .parse::<ResultId>()
            .ok()?;
        let verdict = parse_decision_verdict(row.verdict.as_deref()?)?;
        (row.decision_schema.as_deref() == Some(SCHEMA_V1)
            && row.decision_task_id.as_deref() == Some(row.stored_task_id.as_str())
            && row.decision_revision.as_deref() == Some(row.stored_revision.as_str())
            && decision_result_id == *result_id)
            .then_some(verdict)
    });
    let result_outcome = result.map(|(_, outcome)| outcome);
    Some(BoardTaskRow {
        task_id,
        revision,
        workspace,
        harness_id,
        attempt_state,
        result_outcome,
        decision_verdict,
    })
}

fn parse_attempt_state(value: &str) -> Option<AttemptState> {
    match value {
        "queued" => Some(AttemptState::Queued),
        "starting" => Some(AttemptState::Starting),
        "running" => Some(AttemptState::Running),
        "blocked" => Some(AttemptState::Blocked),
        "collecting" => Some(AttemptState::Collecting),
        "cancel_requested" => Some(AttemptState::CancelRequested),
        "terminal" => Some(AttemptState::Terminal),
        _ => None,
    }
}

fn parse_terminal_outcome(value: &str) -> Option<TerminalOutcome> {
    match value {
        "candidate" => Some(TerminalOutcome::Candidate),
        "failed" => Some(TerminalOutcome::Failed),
        "cancelled" => Some(TerminalOutcome::Cancelled),
        "lost" => Some(TerminalOutcome::Lost),
        _ => None,
    }
}

fn parse_decision_verdict(value: &str) -> Option<DecisionVerdict> {
    match value {
        "accepted" => Some(DecisionVerdict::Accepted),
        "rejected" => Some(DecisionVerdict::Rejected),
        _ => None,
    }
}
