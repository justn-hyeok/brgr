//! The schema text, the stamp that identifies it, and connection setup.
//!
//! Kept together because they are one decision: what this store is expected to
//! contain, how an open recognises a store that already contains it, and what a
//! connection must declare for itself every time.

use rusqlite::Connection;

use super::{StoreError, sha256};

pub(crate) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    owner_id TEXT NOT NULL,
    create_request_id TEXT NOT NULL UNIQUE,
    request_digest TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    PRIMARY KEY (task_id, revision)
);
CREATE TABLE IF NOT EXISTS attempts (
    attempt_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    state TEXT NOT NULL,
    FOREIGN KEY (task_id, revision) REFERENCES tasks(task_id, revision)
);
CREATE TABLE IF NOT EXISTS attempt_clocks (
    attempt_id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
);
CREATE TABLE IF NOT EXISTS launch_intents (
    attempt_id TEXT PRIMARY KEY,
    launch_nonce TEXT NOT NULL UNIQUE,
    supervisor_epoch INTEGER NOT NULL CHECK (supervisor_epoch > 0),
    runner_identity_json TEXT,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS one_active_attempt_per_revision
ON attempts (task_id, revision) WHERE state <> 'terminal';
CREATE TABLE IF NOT EXISTS results (
    result_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    result_digest TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id),
    FOREIGN KEY (task_id, revision) REFERENCES tasks(task_id, revision)
);
CREATE TABLE IF NOT EXISTS route_observations (
    result_id TEXT PRIMARY KEY,
    observation_digest TEXT NOT NULL,
    observation_json TEXT NOT NULL,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS pre_spawn_retry_grants (
    attempt_id TEXT PRIMARY KEY,
    FOREIGN KEY (attempt_id) REFERENCES results(attempt_id)
);
CREATE TABLE IF NOT EXISTS inbox_items (
    owner_id TEXT NOT NULL,
    result_id TEXT NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
    PRIMARY KEY (owner_id, result_id),
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS decisions (
    decision_id TEXT PRIMARY KEY,
    result_id TEXT NOT NULL UNIQUE,
    decision_json TEXT NOT NULL,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE TABLE IF NOT EXISTS idempotency_requests (
    request_id TEXT PRIMARY KEY,
    request_digest TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    event_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    producer TEXT NOT NULL,
    producer_seq INTEGER NOT NULL,
    event_json TEXT NOT NULL,
    UNIQUE (attempt_id, producer, producer_seq),
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id)
);
CREATE TABLE IF NOT EXISTS owner_bindings (
    owner_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    binding_epoch INTEGER NOT NULL CHECK (binding_epoch > 0)
);
CREATE TABLE IF NOT EXISTS delegation_edges (
    child_task_id TEXT PRIMARY KEY,
    parent_task_id TEXT NOT NULL,
    parent_attempt_id TEXT NOT NULL,
    depth INTEGER NOT NULL CHECK (depth BETWEEN 1 AND 8),
    FOREIGN KEY (parent_attempt_id) REFERENCES attempts(attempt_id)
);
CREATE INDEX IF NOT EXISTS delegation_edges_parent ON delegation_edges (parent_task_id);
CREATE TABLE IF NOT EXISTS cancellation_intents (
    task_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    requested_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS task_checkouts (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    primary_checkout TEXT NOT NULL,
    PRIMARY KEY (task_id, revision)
);
CREATE TABLE IF NOT EXISTS task_run_completions (
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    result_id TEXT NOT NULL UNIQUE,
    completed_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, revision)
);
CREATE TABLE IF NOT EXISTS task_messages (
    message_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('owner_to_worker', 'worker_to_owner')),
    kind TEXT NOT NULL CHECK (kind IN ('question', 'reply', 'note')),
    body TEXT NOT NULL,
    in_reply_to TEXT,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
    FOREIGN KEY (attempt_id) REFERENCES attempts(attempt_id),
    FOREIGN KEY (in_reply_to) REFERENCES task_messages(message_id)
);
CREATE INDEX IF NOT EXISTS task_messages_inbox
ON task_messages (task_id, attempt_id, direction, acknowledged);
CREATE UNIQUE INDEX IF NOT EXISTS task_message_one_reply
ON task_messages (in_reply_to) WHERE kind = 'reply';
CREATE TABLE IF NOT EXISTS withdrawn_questions (
    message_id TEXT PRIMARY KEY,
    FOREIGN KEY (message_id) REFERENCES task_messages(message_id)
);
CREATE TABLE IF NOT EXISTS owner_surfaces (
    owner_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    binding_epoch INTEGER NOT NULL,
    pane_id TEXT NOT NULL,
    herdr_session TEXT,
    herdr_bin TEXT NOT NULL,
    FOREIGN KEY (owner_id) REFERENCES owner_bindings(owner_id)
);
CREATE TABLE IF NOT EXISTS question_notices (
    message_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (message_id, session_id),
    FOREIGN KEY (message_id) REFERENCES task_messages(message_id)
);
CREATE TABLE IF NOT EXISTS completion_notifications (
    result_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0 CHECK (resolved IN (0, 1)),
    delivered_session TEXT,
    delivered_epoch INTEGER,
    delivered_pane TEXT,
    claim_token TEXT,
    claim_until INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    FOREIGN KEY (result_id) REFERENCES results(result_id)
);
CREATE INDEX IF NOT EXISTS completion_notifications_owner
ON completion_notifications (owner_id, resolved, delivered_session);
CREATE INDEX IF NOT EXISTS results_task_revision ON results (task_id, revision);
CREATE INDEX IF NOT EXISTS attempts_task_revision ON attempts (task_id, revision);
CREATE TABLE IF NOT EXISTS native_message_deliveries (
    message_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    state TEXT NOT NULL,
    error TEXT
);
CREATE TABLE IF NOT EXISTS debate_groups (
    group_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS debate_members (
    group_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    PRIMARY KEY (group_id, task_id),
    FOREIGN KEY (group_id) REFERENCES debate_groups(group_id)
);
CREATE TABLE IF NOT EXISTS peer_messages (
    message_id TEXT PRIMARY KEY,
    group_id TEXT NOT NULL,
    from_task TEXT NOT NULL,
    to_task TEXT NOT NULL,
    from_attempt TEXT NOT NULL,
    to_attempt TEXT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    in_reply_to TEXT,
    acknowledged INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (group_id) REFERENCES debate_groups(group_id)
);
";

/// Identifies the exact [`SCHEMA`] a store was last initialized with, so an
/// ordinary command no longer opens a write transaction just to re-apply an
/// unchanged schema.
///
/// Derived from the schema text rather than hand-maintained. A hand-bumped
/// constant can be forgotten, and the previous unconditional
/// `CREATE ... IF NOT EXISTS` batch self-healed on every open; deriving the
/// stamp keeps that property, because any edit to `SCHEMA` changes it and every
/// store with a different stamp re-applies the batch. `user_version` is a
/// signed 32-bit field, and `0` is reserved for a store written before the
/// stamp existed.
pub(crate) fn schema_version() -> i64 {
    schema_stamp(SCHEMA)
}

/// Folds a schema's text into the `user_version` field. Never silently falls
/// back: a stamp that does not track the text would make every store skip a new
/// object forever.
pub(crate) fn schema_stamp(schema: &str) -> i64 {
    let digest = sha256(schema.as_bytes());
    let hex = digest
        .strip_prefix("sha256:")
        .expect("sha256 renders a prefixed digest");
    let head = i64::from_str_radix(&hex[..8], 16).expect("a digest's leading bytes are hex");
    (head & 0x7fff_ffff).max(1)
}

/// Applies the per-connection pragmas and, only when the stored schema is
/// behind, the idempotent DDL batch.
///
/// `foreign_keys` and `synchronous` are per-connection and are always set.
/// `journal_mode` is persistent, and re-declaring it takes a lock that
/// `busy_timeout` does not cover, so it is only written when it differs.
pub(crate) fn initialize_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    let stamp = schema_version();
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == stamp {
        return Ok(());
    }

    // A different stamp means a build with different schema text opened this store,
    // which is not the same as something being absent. Applying the batch on that
    // signal alone made two builds rewrite `user_version` past each other, so a
    // read-only command such as `brgr status` needed the write lock and failed
    // after 16.8s against a held one — the contention the stamp exists to avoid.
    // Check first; write only when an object really is missing.
    let missing = missing_schema_objects(connection)?;
    if !missing.is_empty() {
        connection.execute_batch(SCHEMA)?;
        connection.pragma_update(None, "user_version", stamp)?;
    } else if version == 0 {
        // Written before the stamp existed: record it once so ordinary opens stop
        // re-checking. A build that disagrees only on the digest leaves it alone.
        connection.pragma_update(None, "user_version", stamp)?;
    }
    Ok(())
}

/// Names every table and index [`SCHEMA`] declares that the store does not have.
///
/// Read-only, and one query rather than one per object.
pub(crate) fn missing_schema_objects(connection: &Connection) -> Result<Vec<String>, StoreError> {
    let mut statement =
        connection.prepare("SELECT name FROM sqlite_master WHERE name IS NOT NULL")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let present = rows.collect::<Result<Vec<_>, _>>()?;
    Ok(declared_schema_objects(SCHEMA)
        .into_iter()
        .filter(|name| !present.iter().any(|existing| existing == name))
        .map(str::to_owned)
        .collect())
}

/// Reads the object names out of the schema text instead of keeping a second list
/// beside it, for the same reason the stamp is derived rather than hand-written: a
/// parallel list is a thing to forget.
pub(crate) fn declared_schema_objects(schema: &str) -> Vec<&str> {
    schema
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("CREATE TABLE IF NOT EXISTS ")
                .or_else(|| line.strip_prefix("CREATE UNIQUE INDEX IF NOT EXISTS "))
                .or_else(|| line.strip_prefix("CREATE INDEX IF NOT EXISTS "))?;
            rest.split(|character: char| character.is_whitespace() || character == '(')
                .next()
                .filter(|name| !name.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::Store;
    use crate::fixtures::{index_names, sealed_result, task};
    use brgr_protocol::AttemptId;

    #[test]
    fn open_sets_per_connection_pragmas_and_records_the_schema_version() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).unwrap();

        // `foreign_keys` moved out of the versioned DDL batch, so it has to be
        // re-declared on every connection rather than only on a first open.
        let foreign_keys: i64 = store
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let journal_mode: String = store
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(journal_mode.eq_ignore_ascii_case("wal"));
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema_version());

        drop(store);
        let reopened = Store::open(root.path()).unwrap();
        let foreign_keys: i64 = reopened
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            foreign_keys, 1,
            "reopen skipped the DDL and lost enforcement"
        );
    }
    /// A hand-maintained version constant could be left behind by a schema
    /// edit, and a store stamped with the stale value would then skip the new
    /// objects forever while a freshly created store got them — a bug that only
    /// reproduces on someone else's machine.
    #[test]
    fn a_stale_schema_stamp_reapplies_the_batch_even_when_it_is_nonzero() {
        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).unwrap();
        store
            .connection
            .execute_batch(
                "DROP INDEX results_task_revision;
                 DROP INDEX one_active_attempt_per_revision;
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(root.path()).unwrap();
        let indexes = index_names(&reopened.connection);
        assert!(indexes.contains(&"results_task_revision".to_owned()));
        assert!(
            indexes.contains(&"one_active_attempt_per_revision".to_owned()),
            "the sole enforcement of one active attempt per revision was lost"
        );
        let version: i64 = reopened
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema_version());
    }
    /// The stamp has to change whenever the schema text does, or the self-healing
    /// above cannot notice an edit.
    #[test]
    fn the_schema_stamp_is_derived_from_the_schema_text() {
        assert_eq!(schema_version(), schema_version());
        assert_ne!(schema_version(), 0, "0 is reserved for an unstamped store");
        assert!(
            schema_version() > 0,
            "user_version is a signed 32-bit field"
        );
        assert_ne!(
            schema_version(),
            schema_stamp(&format!(
                "{SCHEMA}\nCREATE INDEX IF NOT EXISTS later ON tasks (owner_id);"
            )),
            "the stamp does not change with the schema text, so a new object \
             would be skipped by every existing store"
        );
    }
    /// Two builds whose schema text differs must not rewrite `user_version` past
    /// each other on every open.
    ///
    /// The stamp is derived from the schema text, so a build with one extra object
    /// carries a different one. Applying the batch on that signal alone turned a
    /// read-only command into one that needs the write lock: measured on
    /// 2026-09-29, `brgr status` against a held lock failed after 16.8s. The
    /// mismatch is now checked read-only first.
    #[test]
    fn a_stamp_from_another_build_does_not_make_an_open_take_the_write_lock() {
        let root = TempDir::new().unwrap();
        let first = Store::open(root.path()).unwrap();
        // What the other build's stamp looks like: different, nonzero, and with
        // every declared object still in place.
        first
            .connection
            .pragma_update(None, "user_version", 4_242)
            .unwrap();
        drop(first);

        let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        blocker.busy_timeout(Duration::ZERO).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = Instant::now();
        let reopened = Store::open(root.path());
        let elapsed = started.elapsed();
        blocker.execute_batch("ROLLBACK").unwrap();

        assert!(
            reopened.is_ok(),
            "a foreign stamp made the open take the write lock: {:?}",
            reopened.err()
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the open waited {elapsed:?} on the write lock"
        );
        // The disagreement is left alone rather than fought over.
        let version: i64 = reopened
            .unwrap()
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4_242);
    }
    /// The read-only check has to name what is actually absent, or the self-healing
    /// above degrades into never applying the batch.
    #[test]
    fn the_declared_objects_are_read_from_the_schema_text() {
        let declared = declared_schema_objects(SCHEMA);
        for expected in [
            "tasks",
            "attempts",
            "results",
            "decisions",
            "inbox_items",
            "one_active_attempt_per_revision",
            "results_task_revision",
            "attempts_task_revision",
        ] {
            assert!(
                declared.contains(&expected),
                "{expected} is declared by SCHEMA but not recognised: {declared:?}"
            );
        }

        let root = TempDir::new().unwrap();
        let store = Store::open(root.path()).unwrap();
        assert!(
            missing_schema_objects(&store.connection)
                .unwrap()
                .is_empty(),
            "a freshly initialized store is missing a declared object"
        );
        store
            .connection
            .execute_batch("DROP INDEX results_task_revision;")
            .unwrap();
        assert_eq!(
            missing_schema_objects(&store.connection).unwrap(),
            vec!["results_task_revision".to_owned()]
        );
    }
    /// Opening an already-initialized store must not block on a live writer.
    ///
    /// This pins the property, not the optimization: the earlier per-open
    /// pragma and DDL batch also satisfied it, and its contention effect was
    /// measured at roughly one failed admission in fifty rather than anything a
    /// deterministic test can observe. `benches/concurrent_admission.rs` is
    /// where that cost is reported.
    #[test]
    fn opening_an_initialized_store_needs_no_write_lock() {
        let root = TempDir::new().unwrap();
        let first = Store::open(root.path()).unwrap();
        let blocker = Connection::open(root.path().join("brgr.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = Instant::now();
        let second = Store::open(root.path());
        assert!(
            second.is_ok(),
            "opening a store took a write lock: {:?}",
            second.err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "opening a store waited on the write lock for {:?}",
            started.elapsed()
        );

        blocker.execute_batch("ROLLBACK").unwrap();
        drop(second);
        drop(first);
    }
    #[test]
    fn a_store_written_before_the_version_gate_gains_indexes_without_losing_rows() {
        let root = TempDir::new().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let task = task();
        let attempt_id = AttemptId::new();
        store.record_task(&task, "digest-migrate").unwrap();
        store
            .create_attempt(task.task_id, task.revision, attempt_id)
            .unwrap();
        let result = sealed_result(&store, &task, attempt_id);
        store
            .commit_terminal_result(&task.owner_id, &result)
            .unwrap();

        // Recreate a store from before the indexes and the version marker.
        store
            .connection
            .execute_batch(
                "DROP INDEX results_task_revision;
                 DROP INDEX attempts_task_revision;
                 PRAGMA user_version = 0;",
            )
            .unwrap();
        assert!(!index_names(&store.connection).contains(&"results_task_revision".to_owned()));
        drop(store);

        let reopened = Store::open(root.path()).unwrap();
        let indexes = index_names(&reopened.connection);
        assert!(indexes.contains(&"results_task_revision".to_owned()));
        assert!(indexes.contains(&"attempts_task_revision".to_owned()));
        let version: i64 = reopened
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema_version());
        assert_eq!(reopened.inbox(&task.owner_id, false).unwrap().len(), 1);
        assert_eq!(
            reopened.latest_result(task.task_id).unwrap().result_id,
            result.result_id
        );
    }
}
