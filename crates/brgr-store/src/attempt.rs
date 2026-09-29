//! Attempt lifecycle: claims, launch intents, runner identity, state transitions, terminal results, and events.

use brgr_protocol::{
    AttemptId, AttemptState, Event, OwnerId, ResultEnvelope, ResultId, RouteObservation, TaskId,
    TaskSpec,
};
use rusqlite::{OptionalExtension as _, params};

#[cfg(test)]
use super::COMMIT_TRIES;

use super::{
    LaunchIntent, PreparedTerminal, RunnerIdentity, Store, StoreError, UnfinishedAttempt,
    WriteOutcome, allowed_attempt_transition, contention::retry_busy, insert_route_observation,
    insert_run_completion, insert_terminal_event, parse_state, read_launch_intent,
    serialize_route_observation, sha256, state_name, terminal_attempt,
    validate_recovery_observation, validate_schema, validate_terminal_result,
    verify_candidate_artifacts, verify_replayed_route_observation,
};

impl Store {
    /// Claims the sole active attempt slot for a task revision.
    ///
    /// Only a failed prior attempt can release the slot for a bounded retry.
    /// Lost, cancelled, and candidate results cannot start another attempt on
    /// the same revision. A concurrent supervisor cannot overlap a live run.
    ///
    /// The candidate case is load-bearing well outside this function. `brgr
    /// prune` removes the worktree of a settled revision, and its argument for
    /// why no attempt can still be running there is exactly this refusal —
    /// a revision with a recorded decision has a candidate result, and a
    /// candidate result admits no further attempt. That is asserted here rather
    /// than only stated, so a change that relaxes it fails at its source.
    ///
    /// The match on the prior outcome defaults to refusing, so an outcome this
    /// build does not recognize cannot open the slot either:
    ///
    /// ```
    /// use brgr_protocol::{AttemptId, ResultEnvelope, ResultId, SCHEMA_V1, TerminalOutcome};
    /// use brgr_store::{Store, StoreError};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let root = tempfile::tempdir()?;
    /// # let task: brgr_protocol::TaskSpec = serde_json::from_str(r#"{
    /// #   "schema": "brgr/v1",
    /// #   "task_id": "3d3c9081-0f4a-4f2e-9c1b-7a2d5e6f8a90",
    /// #   "revision": 1,
    /// #   "create_request_id": "req-1",
    /// #   "owner_id": "codex:alice",
    /// #   "objective": "Summarize the build log",
    /// #   "workspace": "/srv/checkout",
    /// #   "route": { "harness_id": "local.fixture" },
    /// #   "required_capabilities": ["completion"],
    /// #   "artifact_contract": { "media_type": "text/plain", "max_bytes": 4096 },
    /// #   "acceptance_criteria": ["the report is sealed"],
    /// #   "budget": { "deadline_seconds": 60, "max_attempts": 2 }
    /// # }"#)?;
    /// let mut store = Store::open(root.path())?;
    /// store.record_task(&task, "digest-1")?;
    ///
    /// let first = AttemptId::new();
    /// store.create_attempt(task.task_id, task.revision, first)?;
    /// let artifact = store.seal_artifact_reader(
    ///     std::io::Cursor::new(b"the report"),
    ///     &task.artifact_contract.media_type,
    ///     task.artifact_contract.max_bytes,
    /// )?;
    /// store.commit_terminal_result(
    ///     &task.owner_id,
    ///     &ResultEnvelope {
    ///         schema: SCHEMA_V1.to_owned(),
    ///         task_id: task.task_id,
    ///         revision: task.revision,
    ///         attempt_id: first,
    ///         result_id: ResultId::new(),
    ///         outcome: TerminalOutcome::Candidate,
    ///         artifacts: vec![artifact],
    ///         error: None,
    ///         legacy_embedded_route_observation: None,
    ///         route_observation: None,
    ///         unresolved_effects: vec![],
    ///     },
    /// )?;
    ///
    /// // The budget permits a retry and only one attempt was spent, so the
    /// // refusal below is the candidate rule and not exhaustion. Matched on the
    /// // variant for exactly that reason: `is_err` would not tell them apart.
    /// let refusal = store.claim_attempt(task.task_id, task.revision, AttemptId::new());
    /// assert!(
    ///     matches!(refusal, Err(StoreError::NonRetryablePriorAttempt { .. })),
    ///     "a candidate result must admit no further attempt, got {refusal:?}"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or an active attempt exists.
    pub fn claim_attempt(
        &self,
        task_id: TaskId,
        revision: u32,
        attempt_id: AttemptId,
    ) -> Result<(), StoreError> {
        retry_busy(|| self.claim_attempt_once(task_id, revision, attempt_id))
    }

    fn claim_attempt_once(
        &self,
        task_id: TaskId,
        revision: u32,
        attempt_id: AttemptId,
    ) -> Result<(), StoreError> {
        let transaction = self.write_transaction()?;
        let spec_json = transaction
            .query_row(
                "SELECT spec_json FROM tasks WHERE task_id = ?1 AND revision = ?2",
                params![task_id.to_string(), revision],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        let task: TaskSpec = serde_json::from_str(&spec_json)?;
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM attempts WHERE task_id = ?1 AND revision = ?2",
            params![task_id.to_string(), revision],
            |row| row.get(0),
        )?;
        if count >= i64::from(task.budget.max_attempts) {
            return Err(StoreError::AttemptBudgetExhausted { task_id, revision });
        }
        let prior_result: Option<(String, String)> = transaction
            .query_row(
                "SELECT a.attempt_id, json_extract(r.envelope_json, '$.outcome')
                 FROM results r JOIN attempts a ON a.attempt_id = r.attempt_id
                 WHERE a.task_id = ?1 AND a.revision = ?2
                 ORDER BY r.rowid DESC LIMIT 1",
                params![task_id.to_string(), revision],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((prior_id, outcome)) = &prior_result {
            match outcome.as_str() {
                "lost" => return Err(StoreError::UnresolvedPriorAttempt { task_id, revision }),
                "candidate" | "cancelled" => {
                    return Err(StoreError::NonRetryablePriorAttempt { task_id, revision });
                }
                "failed" => {
                    let grant = transaction
                        .query_row(
                            "SELECT 1 FROM pre_spawn_retry_grants WHERE attempt_id = ?1",
                            [prior_id],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !grant {
                        return Err(StoreError::NonRetryablePriorAttempt { task_id, revision });
                    }
                }
                _ => return Err(StoreError::NonRetryablePriorAttempt { task_id, revision }),
            }
        }
        let inserted = transaction.execute(
            "INSERT INTO attempts (attempt_id, task_id, revision, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt_id.to_string(),
                task_id.to_string(),
                revision,
                state_name(AttemptState::Queued),
            ],
        );
        match inserted {
            Ok(_) => {
                transaction.execute(
                    "INSERT INTO attempt_clocks (attempt_id, started_at)
                     VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
                    [attempt_id.to_string()],
                )?;
                transaction.commit()?;
                Ok(())
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let active = transaction.query_row(
                    "SELECT 1 FROM attempts WHERE task_id = ?1 AND revision = ?2 AND state <> 'terminal' LIMIT 1",
                    params![task_id.to_string(), revision],
                    |_| Ok(()),
                ).optional()?.is_some();
                if active {
                    Err(StoreError::ActiveAttemptExists { task_id, revision })
                } else {
                    Err(StoreError::Database(rusqlite::Error::SqliteFailure(
                        error, None,
                    )))
                }
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }

    /// Allows one bounded retry only after the runner reported a transient
    /// process-spawn failure. A crash before this receipt stays non-retryable.
    ///
    /// # Errors
    ///
    /// Returns an error unless the attempt has a committed failed result.
    pub fn grant_pre_spawn_retry(&self, attempt_id: AttemptId) -> Result<(), StoreError> {
        // Retried: without the grant a transient spawn failure becomes terminal.
        retry_busy(|| self.grant_pre_spawn_retry_once(attempt_id))
    }

    fn grant_pre_spawn_retry_once(&self, attempt_id: AttemptId) -> Result<(), StoreError> {
        let outcome: Option<String> = self
            .connection
            .query_row(
                "SELECT json_extract(envelope_json, '$.outcome') FROM results WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if outcome.as_deref() != Some("failed") {
            return Err(StoreError::RetryGrantRequiresFailedAttempt(attempt_id));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO pre_spawn_retry_grants (attempt_id) VALUES (?1)",
            [attempt_id.to_string()],
        )?;
        Ok(())
    }

    /// Compatibility alias for `claim_attempt`.
    ///
    /// # Errors
    ///
    /// Returns an error when the task is missing or the attempt already exists.
    pub fn create_attempt(
        &self,
        task_id: TaskId,
        revision: u32,
        attempt_id: AttemptId,
    ) -> Result<(), StoreError> {
        self.claim_attempt(task_id, revision, attempt_id)
    }

    /// Persists the launch intent before spawning a process. The nonce is a
    /// fresh opaque value for this attempt and must not be reused on retry.
    ///
    /// # Errors
    ///
    /// Rejects a missing/non-starting attempt, invalid receipt, or a second
    /// launch claim, including one from another supervisor connection.
    pub fn record_launch_intent(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        supervisor_epoch: u64,
    ) -> Result<(), StoreError> {
        // Retried: a running attempt depends on this receipt existing.
        retry_busy(|| self.record_launch_intent_once(attempt_id, nonce, supervisor_epoch))
    }

    fn record_launch_intent_once(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        supervisor_epoch: u64,
    ) -> Result<(), StoreError> {
        if nonce.trim().is_empty() || supervisor_epoch == 0 {
            return Err(StoreError::InvalidLaunchIntent);
        }
        let epoch = i64::try_from(supervisor_epoch).map_err(|_| StoreError::NumericOverflow)?;
        let transaction = self.write_transaction()?;
        let state = transaction
            .query_row(
                "SELECT state FROM attempts WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(attempt_id))?;
        let state = parse_state(&state)?;
        if state != AttemptState::Starting {
            return Err(StoreError::AttemptStateConflict {
                expected: AttemptState::Starting,
                actual: state,
            });
        }
        let inserted = transaction.execute(
            "INSERT INTO launch_intents (attempt_id, launch_nonce, supervisor_epoch)
             VALUES (?1, ?2, ?3)",
            params![attempt_id.to_string(), nonce, epoch],
        );
        match inserted {
            Ok(1) => transaction.commit().map_err(StoreError::from),
            Ok(_) => Err(StoreError::InvalidLaunchIntent),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::LaunchIntentConflict(attempt_id))
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }

    /// Attaches a verified process/session incarnation to the original intent.
    /// A changed incarnation cannot replace it.
    ///
    /// # Errors
    ///
    /// Rejects a stale nonce, invalid identity, or conflicting observation.
    pub fn record_runner_identity(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        identity: &RunnerIdentity,
    ) -> Result<WriteOutcome, StoreError> {
        // Retried: the process is already spawned when this is written.
        retry_busy(|| self.record_runner_identity_once(attempt_id, nonce, identity))
    }

    fn record_runner_identity_once(
        &self,
        attempt_id: AttemptId,
        nonce: &str,
        identity: &RunnerIdentity,
    ) -> Result<WriteOutcome, StoreError> {
        identity.validate()?;
        let transaction = self.write_transaction()?;
        let stored = read_launch_intent(&transaction, attempt_id)?
            .ok_or(StoreError::LaunchIntentNotFound(attempt_id))?;
        if stored.nonce != nonce {
            return Err(StoreError::LaunchIntentConflict(attempt_id));
        }
        if let Some(existing) = stored.runner_identity {
            return if existing == *identity {
                Ok(WriteOutcome::AlreadyApplied)
            } else {
                Err(StoreError::RunnerIdentityConflict(attempt_id))
            };
        }
        let state = self.attempt_state_by_id(attempt_id)?;
        if state == AttemptState::Terminal {
            return Err(StoreError::AttemptStateConflict {
                expected: AttemptState::Running,
                actual: state,
            });
        }
        transaction.execute(
            "UPDATE launch_intents SET runner_identity_json = ?1
             WHERE attempt_id = ?2 AND launch_nonce = ?3 AND runner_identity_json IS NULL",
            params![
                serde_json::to_string(identity)?,
                attempt_id.to_string(),
                nonce
            ],
        )?;
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Lists every attempt that has no terminal result, including legacy
    /// attempts with no launch receipt. Recovery never silently retries them.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid stored data or database failure.
    pub fn unfinished_attempts(&self) -> Result<Vec<UnfinishedAttempt>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT a.attempt_id, a.state, t.spec_json, l.launch_nonce,
                    l.supervisor_epoch, l.runner_identity_json
             FROM attempts a JOIN tasks t
               ON t.task_id = a.task_id AND t.revision = a.revision
             LEFT JOIN launch_intents l ON l.attempt_id = a.attempt_id
             WHERE a.state <> 'terminal' ORDER BY a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, state, task, nonce, epoch, identity) = row?;
            let launch = match (nonce, epoch) {
                (Some(nonce), Some(epoch)) => Some(LaunchIntent {
                    nonce,
                    supervisor_epoch: u64::try_from(epoch)
                        .map_err(|_| StoreError::NumericOverflow)?,
                    runner_identity: identity
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                }),
                (None, None) => None,
                _ => return Err(StoreError::InvalidLaunchIntent),
            };
            Ok(UnfinishedAttempt {
                task: serde_json::from_str(&task)?,
                attempt_id: id.parse().map_err(|_| StoreError::InvalidAttemptId(id))?,
                state: parse_state(&state)?,
                launch,
            })
        })
        .collect()
    }

    /// Updates the durable state for an attempt using the current state.
    ///
    /// Prefer `compare_and_set_attempt_state` when the caller has an observed
    /// state: only that method rejects a competing writer's intervening update.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt does not exist or storage fails.
    pub fn set_attempt_state(
        &self,
        attempt_id: AttemptId,
        state: AttemptState,
    ) -> Result<(), StoreError> {
        let current = self.attempt_state_by_id(attempt_id)?;
        self.compare_and_set_attempt_state(attempt_id, current, state)
    }

    /// Applies a legal attempt transition only if the observed state is current.
    ///
    /// # Errors
    ///
    /// Returns a conflict for a stale writer or an invalid transition. Terminal
    /// state is reserved for `commit_terminal_result`.
    pub fn compare_and_set_attempt_state(
        &self,
        attempt_id: AttemptId,
        expected: AttemptState,
        next: AttemptState,
    ) -> Result<(), StoreError> {
        // Retried: a state transition lost mid-run leaves the attempt unfinished.
        retry_busy(|| self.compare_and_set_attempt_state_once(attempt_id, expected, next))
    }

    fn compare_and_set_attempt_state_once(
        &self,
        attempt_id: AttemptId,
        expected: AttemptState,
        next: AttemptState,
    ) -> Result<(), StoreError> {
        if !allowed_attempt_transition(expected, next) {
            return Err(StoreError::AttemptTransitionInvalid {
                from: expected,
                to: next,
            });
        }
        let changed = self.connection.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2 AND state = ?3",
            params![
                state_name(next),
                attempt_id.to_string(),
                state_name(expected)
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let actual = self.attempt_state_by_id(attempt_id)?;
        Err(StoreError::AttemptStateConflict { expected, actual })
    }

    /// Returns an attempt's durable state by its immutable ID.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing attempt or invalid stored state.
    pub fn attempt_state_by_id(&self, attempt_id: AttemptId) -> Result<AttemptState, StoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT state FROM attempts WHERE attempt_id = ?1",
                [attempt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::AttemptNotFound(attempt_id))?;
        parse_state(&state)
    }

    /// Atomically stores one terminal result and its owner inbox item.
    ///
    /// Exact replay is idempotent. A second, different result for an attempt is
    /// rejected without changing either result or inbox state.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid relationship, conflicting terminal
    /// result, serialization failure, or database failure.
    pub fn commit_terminal_result(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
    ) -> Result<WriteOutcome, StoreError> {
        self.commit_terminal_result_guarded(owner_id, result, None, false)
    }

    /// Commits the terminal result and the supervisor's final retry decision
    /// in one transaction.
    ///
    /// # Errors
    ///
    /// Rejects a conflicting result or completion marker, or a database failure.
    pub fn commit_terminal_result_final(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
    ) -> Result<WriteOutcome, StoreError> {
        self.commit_terminal_result_guarded(owner_id, result, None, true)
    }

    /// Reads separately committed native route evidence for one result.
    /// Older results without a receipt return `None`.
    ///
    /// # Errors
    ///
    /// Rejects altered receipt bytes or malformed persisted JSON.
    pub fn route_observation(
        &self,
        result_id: ResultId,
    ) -> Result<Option<RouteObservation>, StoreError> {
        let stored: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT observation_digest, observation_json FROM route_observations WHERE result_id = ?1",
                [result_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        stored
            .map(|(digest, json)| {
                if sha256(json.as_bytes()) != digest {
                    return Err(StoreError::RouteObservationIntegrityMismatch);
                }
                Ok(serde_json::from_str(&json)?)
            })
            .transpose()
    }

    /// Publishes a lost result only if the observed unfinished state and
    /// launch receipt are still current. A concurrent runner result wins or
    /// loses atomically; it can never be overwritten by recovery.
    ///
    /// # Errors
    ///
    /// Rejects a stale observation, non-lost outcome, or normal terminal
    /// result conflict.
    pub fn commit_recovered_lost(
        &mut self,
        observed: &UnfinishedAttempt,
        result: &ResultEnvelope,
    ) -> Result<WriteOutcome, StoreError> {
        if result.outcome != brgr_protocol::TerminalOutcome::Lost {
            return Err(StoreError::RecoveryRequiresLost);
        }
        self.commit_terminal_result_guarded(&observed.task.owner_id, result, Some(observed), true)
    }

    /// Lock contention here is the most expensive failure in brgr: the harness
    /// has already run, and a raw busy error leaves the attempt unfinished until
    /// recovery settles it as `Lost` with unresolved effects, which no later
    /// attempt on that revision can supersede.
    fn commit_terminal_result_guarded(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
        observed: Option<&UnfinishedAttempt>,
        complete_run: bool,
    ) -> Result<WriteOutcome, StoreError> {
        // Prepared once, outside the retry. `verify_candidate_artifacts` reads and
        // re-hashes every sealed artifact, up to the contract's `max_bytes` of
        // 20 MiB. Inside the lock it serialized every other writer behind that
        // I/O; moved out of the lock but left inside the retry, every contended
        // attempt re-read and re-hashed it all and threw the work away. None of it
        // depends on the lock: `tasks.spec_json` is only ever inserted, and the
        // transaction compares its own copy against this one.
        let prepared = self.prepare_terminal_result(result)?;
        retry_busy(|| {
            self.commit_terminal_result_once(owner_id, result, &prepared, observed, complete_run)
        })
    }

    pub(super) fn prepare_terminal_result(
        &self,
        result: &ResultEnvelope,
    ) -> Result<PreparedTerminal, StoreError> {
        validate_terminal_result(result)?;
        let envelope_json = serde_json::to_string(result)?;
        let prepared = PreparedTerminal {
            digest: sha256(envelope_json.as_bytes()),
            envelope_json,
            observation: serialize_route_observation(result.route_observation.as_ref())?,
            verified_spec: self.task_spec_for_attempt(result.attempt_id)?,
        };
        verify_candidate_artifacts(&self.artifacts, result, &prepared.verified_spec)?;
        Ok(prepared)
    }

    pub(super) fn commit_terminal_result_once(
        &mut self,
        owner_id: &OwnerId,
        result: &ResultEnvelope,
        prepared: &PreparedTerminal,
        observed: Option<&UnfinishedAttempt>,
        complete_run: bool,
    ) -> Result<WriteOutcome, StoreError> {
        #[cfg(test)]
        COMMIT_TRIES.with(|tries| tries.set(tries.get() + 1));
        let PreparedTerminal {
            envelope_json,
            digest,
            observation,
            verified_spec,
        } = prepared;

        // Terminal commit reads the attempt before it writes, so it goes through
        // the same immediate begin as every other write path.
        let transaction = self.write_transaction()?;

        if let Some(observed) = observed {
            validate_recovery_observation(&transaction, observed)?;
        }

        if let Some((stored_id, stored_digest)) = transaction
            .query_row(
                "SELECT result_id, result_digest FROM results WHERE attempt_id = ?1",
                [result.attempt_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_id == result.result_id.to_string() && stored_digest == *digest {
                verify_replayed_route_observation(
                    &transaction,
                    result.result_id,
                    observation.as_ref(),
                )?;
                if complete_run {
                    insert_run_completion(&transaction, result)?;
                }
                transaction.commit()?;
                return Ok(WriteOutcome::AlreadyApplied);
            }
            return Err(StoreError::TerminalResultConflict(result.attempt_id));
        }

        let expected = terminal_attempt(&transaction, result.attempt_id)?;
        if (expected.0, expected.1) != (result.task_id.to_string(), result.revision) {
            return Err(StoreError::AttemptResultMismatch);
        }
        if expected.2 != owner_id.as_str() {
            return Err(StoreError::ResultOwnerMismatch);
        }
        let current = parse_state(&expected.3)?;
        if current == AttemptState::Terminal {
            return Err(StoreError::AttemptTransitionInvalid {
                from: current,
                to: AttemptState::Terminal,
            });
        }
        // The spec the artifacts were checked against must be the one this
        // transaction sees. It cannot change — nothing updates `tasks.spec_json` —
        // so a mismatch means an assumption broke rather than a race.
        if expected.4 != *verified_spec {
            return Err(StoreError::TaskSpecChangedDuringCommit);
        }

        transaction.execute(
            "INSERT INTO results
             (result_id, attempt_id, task_id, revision, result_digest, envelope_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                result.result_id.to_string(),
                result.attempt_id.to_string(),
                result.task_id.to_string(),
                result.revision,
                digest,
                envelope_json,
            ],
        )?;
        insert_route_observation(&transaction, result.result_id, observation.as_ref())?;
        transaction.execute(
            "INSERT INTO inbox_items (owner_id, result_id) VALUES (?1, ?2)",
            params![owner_id.as_str(), result.result_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO completion_notifications (result_id, task_id, owner_id)
             VALUES (?1, ?2, ?3)",
            params![
                result.result_id.to_string(),
                result.task_id.to_string(),
                owner_id.as_str(),
            ],
        )?;
        if complete_run {
            insert_run_completion(&transaction, result)?;
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1 WHERE attempt_id = ?2 AND state = ?3",
            params![
                state_name(AttemptState::Terminal),
                result.attempt_id.to_string(),
                state_name(current),
            ],
        )?;
        insert_terminal_event(&transaction, result)?;
        transaction.commit()?;
        Ok(WriteOutcome::Inserted)
    }

    /// Returns the most recent attempt state for a task.
    ///
    /// # Errors
    ///
    /// Returns an error when the task has no attempt or stored state is invalid.
    pub fn attempt_state(&self, task_id: TaskId) -> Result<AttemptState, StoreError> {
        let state = self
            .connection
            .query_row(
                "SELECT state FROM attempts WHERE task_id = ?1
                 AND revision = (SELECT MAX(revision) FROM tasks WHERE task_id = ?1)
                 ORDER BY rowid DESC LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound(task_id))?;
        parse_state(&state)
    }

    /// Records a producer event once by both event ID and producer sequence.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed events, missing attempts, or conflicting
    /// reuse of an event identity or producer sequence.
    pub fn record_event(&self, event: &Event) -> Result<WriteOutcome, StoreError> {
        // Retried: supervision events are written while the attempt is live.
        retry_busy(|| self.record_event_once(event))
    }

    fn record_event_once(&self, event: &Event) -> Result<WriteOutcome, StoreError> {
        validate_schema(&event.schema)?;
        if event.producer.trim().is_empty() || event.producer_seq == 0 {
            return Err(StoreError::InvalidEvent);
        }
        let producer_seq =
            i64::try_from(event.producer_seq).map_err(|_| StoreError::NumericOverflow)?;
        let json = serde_json::to_string(event)?;
        if let Some(stored) = self
            .connection
            .query_row(
                "SELECT event_json FROM events WHERE event_id = ?1",
                [event.event_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return if stored == json {
                Ok(WriteOutcome::AlreadyApplied)
            } else {
                Err(StoreError::EventConflict)
            };
        }
        let inserted = self.connection.execute(
            "INSERT INTO events (event_id, attempt_id, producer, producer_seq, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.event_id.to_string(),
                event.attempt_id.to_string(),
                event.producer,
                producer_seq,
                json,
            ],
        );
        match inserted {
            Ok(_) => Ok(WriteOutcome::Inserted),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::EventConflict)
            }
            Err(error) => Err(StoreError::Database(error)),
        }
    }
}
